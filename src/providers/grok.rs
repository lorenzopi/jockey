//! Grok Build adapter over the Agent Client Protocol (`grok agent stdio`).
//!
//! Verified against grok 1.0.70, see docs/research/grok.md:
//! - `_x.ai/interject` queues text into the running turn (`{"status":"queued"}`);
//!   it is delivered at the next tool boundary. Grok emits no live "delivered"
//!   event, so jockey infers "applied" from the next completed tool call and
//!   reports the steer as missed when the turn ends without one.
//! - The user's `~/.grok/config.toml` can force always-approve; every session
//!   is created with `yoloMode: false` so approvals reach jockey.
//! - Paid fallback exists (pay-as-you-go cap, prepaid balance); checked via
//!   `_x.ai/billing` before the first turn.

use super::jsonrpc::{Incoming, RpcClient};
use super::*;
use crate::events::{EventKind, PlanStep};
use crate::process::ManagedChild;
use crate::security::{ApprovalRequest, Decision};
use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;

pub struct Grok {
    pub bin: String,
}

fn caps() -> Capabilities {
    Capabilities {
        stream_events: true,
        live_steer: true,
        steer_ack: true,
        interrupt: true,
        resume: true,
        approvals_to_client: true,
        read_only_mode: true,
        list_models: true,
        effort: true,
        usage: true,
        rate_limits: true,
        native_commands: vec!["compact"],
    }
}

#[async_trait]
impl ProviderAdapter for Grok {
    fn id(&self) -> &'static str {
        "grok"
    }

    fn get_capabilities(&self) -> Capabilities {
        caps()
    }

    async fn probe(&self) -> Probe {
        let version = crate::process::version_of(&self.bin).await;
        let mut notes = vec![];
        if let Some(v) = &version
            && !v.contains("1.0.70")
        {
            notes.push(format!("verified against 1.0.70, found '{v}'"));
        }
        Probe {
            provider: "grok",
            path: crate::process::which(&self.bin),
            version,
            adapter: "acp",
            notes,
        }
    }

    async fn start_session(&self, spec: SessionSpec, ev: Emitter) -> Result<Arc<dyn AgentSession>> {
        let (child, rpc, rx) = self.spawn(&spec.cwd, spec.stderr_log.as_deref())?;
        let s = GrokSession::open(rpc, rx, Some(child), spec, ev).await?;
        Ok(s)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (mut child, rpc, _rx) = self.spawn(&std::env::temp_dir(), None)?;
        let res = async { Ok(parse_models(&initialize(&rpc).await?).0) }.await;
        child.terminate(Duration::from_secs(2)).await;
        res
    }
}

impl Grok {
    fn spawn(
        &self,
        cwd: &std::path::Path,
        stderr_log: Option<&std::path::Path>,
    ) -> Result<(ManagedChild, Arc<RpcClient>, mpsc::Receiver<Incoming>)> {
        let stderr = match stderr_log {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        };
        let mut cmd = Command::new(&self.bin);
        cmd.args(["agent", "--no-leader", "stdio"])
            .current_dir(cwd)
            // Subscription sign-in only: an API key here would bill per token.
            .env_remove("XAI_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr);
        let mut child = ManagedChild::spawn(&mut cmd)
            .with_context(|| format!("is '{}' installed? try `jockey doctor`", self.bin))?;
        if let (Some(p), Some(err)) = (stderr_log, child.child.stderr.take()) {
            crate::sessions::record_stderr(err, p)?;
        }
        let stdin = child.child.stdin.take().context("no stdin")?;
        let stdout = child.child.stdout.take().context("no stdout")?;
        let (rpc, rx) = RpcClient::start(stdout, stdin);
        Ok((child, rpc, rx))
    }
}

async fn initialize(rpc: &RpcClient) -> Result<Value> {
    rpc.request(
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
            "clientInfo": {"name": "jockey", "version": env!("CARGO_PKG_VERSION")},
        }),
    )
    .await
}

/// Models and their context windows from the `initialize` result.
fn parse_models(init: &Value) -> (Vec<ModelInfo>, HashMap<String, u64>) {
    let state = &init["_meta"]["modelState"];
    let current = str_of(state, "currentModelId");
    let mut windows = HashMap::new();
    let models = state["availableModels"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let id = str_of(m, "modelId");
            if let Some(w) = m["_meta"]["totalContextTokens"].as_u64() {
                windows.insert(id.clone(), w);
            }
            ModelInfo {
                is_default: id == current,
                efforts: m["_meta"]["reasoningEfforts"]
                    .as_array()
                    .map(|a| a.iter().map(|e| str_of(e, "id")).collect())
                    .unwrap_or_default(),
                display: m["name"].as_str().map(String::from),
                resolved: None,
                note: None,
                bills_credits: false,
                id,
            }
        })
        .collect();
    (models, windows)
}

#[derive(Default)]
struct State {
    session_id: String,
    active: bool,
    interrupting: bool,
    loading: bool,
    segment: Option<String>,
    segment_text: String,
    segments: u64,
    /// Steers acknowledged by Grok but not yet seen to cross a tool boundary.
    pending_steers: Vec<String>,
    context_window: Option<u64>,
    /// Tool id -> (kind, title) from `tool_call`.
    tools: HashMap<String, (String, String)>,
    /// Fired by the pump on `_x.ai/session/prompt_complete`, so the turn only
    /// ends after every streamed chunk before it has been processed.
    drained: Option<tokio::sync::oneshot::Sender<()>>,
}

struct ClearOnDrop<'a>(&'a Mutex<State>);

impl Drop for ClearOnDrop<'_> {
    fn drop(&mut self) {
        if let Ok(mut st) = self.0.lock() {
            st.active = false;
        }
    }
}

pub struct GrokSession {
    rpc: Arc<RpcClient>,
    state: Arc<Mutex<State>>,
    child: tokio::sync::Mutex<Option<ManagedChild>>,
    ev: Emitter,
    cwd: PathBuf,
    profile: Profile,
}

impl GrokSession {
    pub async fn open(
        rpc: Arc<RpcClient>,
        rx: mpsc::Receiver<Incoming>,
        child: Option<ManagedChild>,
        spec: SessionSpec,
        ev: Emitter,
    ) -> Result<Arc<Self>> {
        let s = Arc::new(Self {
            rpc,
            state: Arc::default(),
            child: tokio::sync::Mutex::new(child),
            ev,
            cwd: spec.cwd.clone(),
            profile: spec.effective_profile(),
        });
        tokio::spawn(pump(s.clone(), rx, spec.approver.clone()));
        if let Err(e) = s.setup(&spec).await {
            s.cancel().await;
            return Err(e);
        }
        Ok(s)
    }

    async fn setup(&self, spec: &SessionSpec) -> Result<()> {
        let init = initialize(&self.rpc).await?;
        let (models, windows) = parse_models(&init);
        let model = match &spec.model {
            Some(id) => Some(models.iter().find(|m| &m.id == id).ok_or_else(|| {
                let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
                anyhow!("grok has no model '{id}' (available: {})", ids.join(", "))
            })?),
            None => models.iter().find(|m| m.is_default),
        };
        if let (Some(e), Some(m)) = (&spec.effort, model)
            && !m.efforts.contains(e)
        {
            bail!(
                "grok model '{}' does not support effort '{e}' (supported: {})",
                m.id,
                m.efforts.join(", ")
            );
        }

        let meta = json!({
            // Never inherit always-approve from ~/.grok/config.toml.
            "yoloMode": self.profile == Profile::Full,
            "autoMode": self.profile == Profile::Auto,
        });
        let session_id = match &spec.resume_id {
            Some(id) => {
                // session/load replays history as updates; don't show it as new activity.
                self.state.lock().unwrap().loading = true;
                let r = self
                    .rpc
                    .request(
                        "session/load",
                        json!({"sessionId": id, "cwd": spec.cwd, "mcpServers": [], "_meta": meta}),
                    )
                    .await;
                self.state.lock().unwrap().loading = false;
                r?;
                id.clone()
            }
            None => {
                let r = self
                    .rpc
                    .request(
                        "session/new",
                        json!({"cwd": spec.cwd, "mcpServers": [], "_meta": meta}),
                    )
                    .await?;
                str_of(&r, "sessionId")
            }
        };
        if session_id.is_empty() {
            bail!("grok returned no session id");
        }
        {
            let mut st = self.state.lock().unwrap();
            st.session_id = session_id.clone();
            st.context_window = model.and_then(|m| windows.get(&m.id).copied());
        }
        self.preflight_billing(&session_id, spec.allow_credits)
            .await?;
        if let Some(m) = &spec.model {
            self.rpc
                .request(
                    "session/set_model",
                    json!({"sessionId": session_id, "modelId": m}),
                )
                .await?;
        }
        if let Some(e) = &spec.effort {
            self.rpc
                .request(
                    "session/set_config_option",
                    json!({"sessionId": session_id, "configId": "reasoning_effort", "value": e}),
                )
                .await?;
        }
        self.ev
            .emit(EventKind::AgentStarted {
                role: if spec.role == Role::Reviewer {
                    "reviewer"
                } else {
                    "primary"
                }
                .into(),
                session_id: Some(session_id),
                model: model.map(|m| m.id.clone()),
                effort: spec.effort.clone(),
            })
            .await;
        Ok(())
    }

    /// Refuses to run when pay-as-you-go or prepaid credit could be charged.
    async fn preflight_billing(&self, session_id: &str, allow_credits: bool) -> Result<()> {
        let b = match self
            .rpc
            .request("_x.ai/billing", json!({"sessionId": session_id}))
            .await
        {
            Ok(b) => b,
            Err(e) => {
                self.ev
                    .emit(EventKind::Warning {
                        message: format!("could not read grok billing: {e}"),
                    })
                    .await;
                return Ok(());
            }
        };
        let c = &b["config"];
        if let Some(pct) = c["creditUsagePercent"].as_f64() {
            self.ev
                .emit(EventKind::RateLimit {
                    used_percent: pct,
                    window_mins: Some(7 * 24 * 60),
                    resets_at: None,
                    credits_balance: None,
                    limit_reached: pct >= 100.0,
                })
                .await;
        }
        let positive = |v: &Value| v["val"].as_f64().unwrap_or(0.0) > 0.0;
        let paid = positive(&c["onDemandCap"])
            || positive(&c["prepaidBalance"])
            || !c["autoTopup"].is_null()
            || !c["monthlyLimit"].is_null();
        if paid && !allow_credits {
            return Err(ProviderError::QuotaExhausted(
                "grok has pay-as-you-go or prepaid credit enabled, so usage past the plan would be billed. \
                 Pass --allow-credits to permit that."
                    .into(),
            )
            .into());
        }
        Ok(())
    }

    fn sid(&self) -> String {
        self.state.lock().unwrap().session_id.clone()
    }

    /// Emits the finished text segment (Grok streams text without message ids).
    async fn flush_segment(&self) -> Option<String> {
        let seg = {
            let mut st = self.state.lock().unwrap();
            st.segment
                .take()
                .map(|id| (id, std::mem::take(&mut st.segment_text)))
        };
        let (item_id, text) = seg?;
        self.ev
            .emit(EventKind::Message {
                item_id,
                text: text.clone(),
                phase: None,
            })
            .await;
        Some(text)
    }

    async fn turn(&self, text: &str) -> Result<TurnOutcome> {
        let sid = {
            let mut st = self.state.lock().unwrap();
            if st.active {
                bail!("a grok turn is already running in this session");
            }
            st.active = true;
            st.interrupting = false;
            st.session_id.clone()
        };
        // If the caller drops this turn (e.g. a cancelled workflow), the session stays usable.
        let _active = ClearOnDrop(&self.state);
        let (drained_tx, drained_rx) = tokio::sync::oneshot::channel();
        self.state.lock().unwrap().drained = Some(drained_tx);
        let res = self
            .rpc
            .request_timeout(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
                None,
            )
            .await;
        // The reply bypasses the event pump; let the pump catch up first.
        let _ = tokio::time::timeout(Duration::from_secs(2), drained_rx).await;
        let last = self.flush_segment().await.unwrap_or_default();
        let (interrupted, missed, window) = {
            let mut st = self.state.lock().unwrap();
            st.active = false;
            (
                st.interrupting,
                std::mem::take(&mut st.pending_steers),
                st.context_window,
            )
        };
        for steer_id in missed {
            let reason = if interrupted {
                "interrupted".to_string()
            } else {
                STEER_MISSED_TURN.to_string()
            };
            self.ev
                .emit(EventKind::SteeringRejected { steer_id, reason })
                .await;
        }
        let r = match res {
            Ok(r) => r,
            Err(e) => {
                let error = format!("{e:#}");
                self.ev
                    .emit(EventKind::AgentFailed {
                        error: error.clone(),
                    })
                    .await;
                return Ok(TurnOutcome {
                    status: TurnStatus::Failed,
                    final_text: last,
                    error: Some(error),
                });
            }
        };
        let u = &r["_meta"]["usage"];
        if u.is_object() {
            self.ev
                .emit(EventKind::UsageUpdated {
                    input_tokens: u["inputTokens"].as_u64().unwrap_or(0),
                    cached_input_tokens: u["cachedReadTokens"].as_u64().unwrap_or(0),
                    output_tokens: u["outputTokens"].as_u64().unwrap_or(0),
                    context_window: window,
                })
                .await;
        }
        let stop = str_of(&r, "stopReason");
        let outcome = match stop.as_str() {
            "end_turn" => TurnOutcome {
                status: TurnStatus::Completed,
                final_text: last,
                error: None,
            },
            "cancelled" if interrupted => TurnOutcome {
                status: TurnStatus::Interrupted,
                final_text: last,
                error: None,
            },
            "cancelled" => TurnOutcome {
                status: TurnStatus::Failed,
                final_text: last,
                error: Some(
                    "grok ended the turn (a denied permission ends the whole turn in Grok)".into(),
                ),
            },
            other => TurnOutcome {
                status: TurnStatus::Failed,
                final_text: last,
                error: Some(format!("grok stopped: {other}")),
            },
        };
        let kind = match outcome.status {
            TurnStatus::Completed => EventKind::AgentCompleted {
                status: "completed".into(),
                final_text: outcome.final_text.clone(),
            },
            TurnStatus::Interrupted => EventKind::AgentInterrupted {
                reason: "interrupted".into(),
            },
            TurnStatus::Failed => EventKind::AgentFailed {
                error: outcome.error.clone().unwrap_or_default(),
            },
        };
        self.ev.emit(kind).await;
        Ok(outcome)
    }
}

#[async_trait]
impl AgentSession for GrokSession {
    fn provider(&self) -> &'static str {
        "grok"
    }

    fn session_id(&self) -> Option<String> {
        let id = self.sid();
        (!id.is_empty()).then_some(id)
    }

    async fn execute(&self, prompt: &str) -> Result<TurnOutcome> {
        self.turn(prompt).await
    }

    async fn steer(&self, text: &str) -> Result<SteerReceipt> {
        let steer_id = uuid::Uuid::now_v7().to_string();
        self.ev
            .emit(EventKind::SteeringSent {
                steer_id: steer_id.clone(),
                text: text.into(),
            })
            .await;
        let (active, sid) = {
            let st = self.state.lock().unwrap();
            (st.active && !st.interrupting, st.session_id.clone())
        };
        if !active {
            self.ev
                .emit(EventKind::SteeringRejected {
                    steer_id,
                    reason: "no active turn".into(),
                })
                .await;
            bail!("no active turn");
        }
        let r = self
            .rpc
            .request("_x.ai/interject", json!({"sessionId": sid, "text": text}))
            .await;
        let queued = match &r {
            Ok(v) => v["result"]["status"] == "queued" || v["status"] == "queued",
            Err(_) => false,
        };
        if !queued {
            let reason = match r {
                Err(e) => e.to_string(),
                Ok(v) => format!("unexpected reply {v}"),
            };
            self.ev
                .emit(EventKind::SteeringRejected {
                    steer_id,
                    reason: reason.clone(),
                })
                .await;
            bail!(reason);
        }
        self.state
            .lock()
            .unwrap()
            .pending_steers
            .push(steer_id.clone());
        self.ev
            .emit(EventKind::SteeringDelivered {
                steer_id: steer_id.clone(),
            })
            .await;
        Ok(SteerReceipt {
            steer_id,
            acknowledged: true,
        })
    }

    async fn interrupt(&self) -> Result<()> {
        let sid = {
            let mut st = self.state.lock().unwrap();
            if !st.active {
                return Ok(());
            }
            st.interrupting = true;
            st.session_id.clone()
        };
        self.rpc
            .notify("session/cancel", Some(json!({"sessionId": sid})))
            .await
    }

    async fn set_turn_options(&self, model: Option<String>, effort: Option<String>) -> Result<()> {
        let sid = self.sid();
        if let Some(m) = model {
            self.rpc
                .request("session/set_model", json!({"sessionId": sid, "modelId": m}))
                .await?;
        }
        if let Some(e) = effort {
            self.rpc
                .request(
                    "session/set_config_option",
                    json!({"sessionId": sid, "configId": "reasoning_effort", "value": e}),
                )
                .await?;
        }
        Ok(())
    }

    async fn native_command(&self, name: &str) -> Result<Option<TurnOutcome>> {
        match name {
            "compact" => self.turn("/compact").await.map(Some),
            other => bail!("grok has no native command '{other}' (try: compact)"),
        }
    }

    async fn cancel(&self) {
        if let Some(mut c) = self.child.lock().await.take() {
            c.terminate(Duration::from_secs(2)).await;
        }
    }
}

async fn pump(s: Arc<GrokSession>, mut rx: mpsc::Receiver<Incoming>, approver: Arc<dyn Approver>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            Incoming::Notification { method, params } => {
                s.ev.emit(EventKind::Raw {
                    value: json!({"method": method, "params": params}),
                })
                .await;
                if method == "session/update" && !s.state.lock().unwrap().loading {
                    on_update(&s, &params["update"]).await;
                } else if method == "_x.ai/session/prompt_complete" {
                    let tx = s.state.lock().unwrap().drained.take();
                    if let Some(tx) = tx {
                        let _ = tx.send(());
                    }
                }
            }
            Incoming::Request { id, method, params } => {
                s.ev.emit(EventKind::Raw {
                    value: json!({"id": id, "method": method, "params": params}),
                })
                .await;
                tokio::spawn(answer_request(
                    s.clone(),
                    approver.clone(),
                    id,
                    method,
                    params,
                ));
            }
        }
    }
}

async fn on_update(s: &Arc<GrokSession>, u: &Value) {
    match u["sessionUpdate"].as_str() {
        Some("agent_message_chunk") => {
            let text = str_of(&u["content"], "text");
            let item_id = {
                let mut st = s.state.lock().unwrap();
                if st.segment.is_none() {
                    st.segments += 1;
                    st.segment = Some(format!("grok-msg-{}", st.segments));
                }
                st.segment_text.push_str(&text);
                st.segment.clone().unwrap()
            };
            s.ev.emit(EventKind::MessageDelta { item_id, text }).await;
        }
        Some("agent_thought_chunk") => {
            let n = s.state.lock().unwrap().segments;
            s.ev.emit(EventKind::ReasoningDelta {
                item_id: format!("grok-thought-{n}"),
                text: str_of(&u["content"], "text"),
            })
            .await;
        }
        Some("tool_call") => {
            s.flush_segment().await;
            let tool = &u["_meta"]["x.ai/tool"];
            let kind = tool_kind(
                tool["kind"]
                    .as_str()
                    .or(u["kind"].as_str())
                    .unwrap_or("tool"),
            );
            let id = str_of(u, "toolCallId");
            let title = tool_title(u);
            s.state
                .lock()
                .unwrap()
                .tools
                .insert(id.clone(), (kind.into(), title.clone()));
            s.ev.emit(EventKind::ToolStarted {
                id,
                kind: kind.into(),
                title,
            })
            .await;
        }
        Some("tool_call_update") => {
            let status = u["status"].as_str();
            if !matches!(status, Some("completed") | Some("failed")) {
                return; // proposals and progress; the final update repeats the content
            }
            let ok = status == Some("completed");
            let id = str_of(u, "toolCallId");
            let mut output = vec![];
            for c in u["content"].as_array().into_iter().flatten() {
                match c["type"].as_str() {
                    Some("diff") if ok => {
                        let old = str_of(c, "oldText");
                        let new = str_of(c, "newText");
                        s.ev.emit(EventKind::FileChanged {
                            path: str_of(c, "path"),
                            change: if old.is_empty() { "write" } else { "edit" }.into(),
                            diff: diff(&old, &new),
                        })
                        .await;
                    }
                    Some("content") => output.push(str_of(&c["content"], "text")),
                    _ => {}
                }
            }
            // Later updates often omit the input, so reuse what `tool_call` said.
            let known = s.state.lock().unwrap().tools.remove(&id);
            let (kind, title) = known.unwrap_or_else(|| {
                (
                    tool_kind(u["kind"].as_str().unwrap_or("tool")).into(),
                    tool_title(u),
                )
            });
            s.ev.emit(EventKind::ToolCompleted {
                id,
                kind,
                title,
                ok,
                exit_code: None,
                output: Some(output.join("\n")),
            })
            .await;
            // A tool boundary: queued interjections are injected here.
            let applied: Vec<_> = std::mem::take(&mut s.state.lock().unwrap().pending_steers);
            for steer_id in applied {
                s.ev.emit(EventKind::SteeringApplied { steer_id }).await;
            }
        }
        Some("plan") => {
            let steps = u["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| PlanStep {
                    step: str_of(e, "content"),
                    status: match e["status"].as_str() {
                        Some("in_progress") => "inProgress".into(),
                        Some(other) => other.into(),
                        None => "pending".into(),
                    },
                })
                .collect();
            s.ev.emit(EventKind::Plan {
                steps,
                explanation: None,
            })
            .await;
        }
        _ => {}
    }
}

async fn answer_request(
    s: Arc<GrokSession>,
    approver: Arc<dyn Approver>,
    id: Value,
    method: String,
    p: Value,
) {
    if method != "session/request_permission" {
        let _ = s
            .rpc
            .respond_error(id, &format!("{method} is not supported by jockey"))
            .await;
        return;
    }
    let tc = &p["toolCall"];
    let tool = &tc["_meta"]["x.ai/tool"];
    let raw = &tc["rawInput"];
    let is_edit = matches!(
        tc["kind"].as_str(),
        Some("edit") | Some("write") | Some("delete")
    ) || tool["kind"] == "write";
    let path = raw["file_path"]
        .as_str()
        .or(raw["target_file"].as_str())
        .unwrap_or("");
    let (kind, detail) = if is_edit {
        ("file_change", str_of(tc, "title"))
    } else if let Some(cmd) = raw["command"].as_str() {
        ("command", cmd.to_string())
    } else {
        ("tool", str_of(tc, "title"))
    };
    let req = ApprovalRequest {
        id: str_of(tc, "toolCallId"),
        kind: kind.into(),
        detail,
    };
    // workspace-write: edits inside the working directory don't need asking.
    let in_workspace = is_edit
        && !path.is_empty()
        && std::path::Path::new(path).starts_with(&s.cwd)
        && !path.contains("..")
        && resolves_inside(std::path::Path::new(path), &s.cwd);
    let (decision, by) = match s.profile {
        Profile::ReadOnly => (Decision::Decline, "profile"),
        Profile::WorkspaceWrite | Profile::Auto if in_workspace => (Decision::Accept, "profile"),
        _ => {
            s.ev.emit(EventKind::ApprovalRequested {
                id: req.id.clone(),
                kind: req.kind.clone(),
                detail: req.detail.clone(),
            })
            .await;
            approver.decide(&req).await
        }
    };
    let want = if decision == Decision::Accept {
        "allow_once"
    } else {
        "reject_once"
    };
    let option = p["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["kind"] == want)
        .map(|o| str_of(o, "optionId"));
    let outcome = match option {
        Some(o) => json!({"outcome": {"outcome": "selected", "optionId": o}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    };
    let _ = s.rpc.respond(id, outcome).await;
    if by != "profile" {
        s.ev.emit(EventKind::ApprovalResolved {
            id: req.id,
            decision: if decision == Decision::Accept {
                "accept"
            } else {
                "decline"
            }
            .into(),
            by: by.into(),
        })
        .await;
    }
}

/// `path` stays inside `root` once symlinks are resolved, judged by its deepest
/// existing ancestor. A dangling link counts as outside: writing through it
/// would create its target wherever it points.
fn resolves_inside(path: &std::path::Path, root: &std::path::Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    for a in path.ancestors() {
        match a.canonicalize() {
            Ok(real) => return real.starts_with(&root),
            Err(_) if a.symlink_metadata().is_ok() => return false,
            Err(_) => {}
        }
    }
    false
}

fn tool_kind(k: &str) -> &'static str {
    match k {
        "execute" | "shell" | "bash" => "command",
        "edit" | "write" | "delete" | "move" => "file_change",
        "read" => "read",
        "search" => "search",
        "fetch" => "web",
        _ => "tool",
    }
}

fn tool_title(u: &Value) -> String {
    let raw = &u["rawInput"];
    if let Some(c) = raw["command"].as_str() {
        return c.to_string();
    }
    let title = str_of(u, "title");
    let path = raw["file_path"].as_str().or(raw["target_file"].as_str());
    match path {
        Some(p) if !title.contains(p) => format!("{title} {p}"),
        _ => title,
    }
}

fn diff(old: &str, new: &str) -> String {
    let mut d = String::from("@@\n");
    for l in old.lines() {
        d.push_str(&format!("-{l}\n"));
    }
    for l in new.lines().take(400) {
        d.push_str(&format!("+{l}\n"));
    }
    d
}

fn str_of(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests;
