//! Codex adapter over `codex app-server` (JSON-RPC 2.0 on stdio).
//!
//! Verified against codex-cli 0.165.0, see docs/research/codex.md:
//! - `turn/steer` injects input into the active turn and is acknowledged;
//!   the input then appears as a `userMessage` item carrying our client id.
//! - Approvals arrive as server requests and are answered by jockey.
//! - When the subscription window is exhausted Codex silently bills credits,
//!   so every turn is preceded by a quota check and watched for credit drain.

use super::jsonrpc::{Incoming, RpcClient};
use super::*;
use crate::events::{EventKind, PlanStep};
use crate::process::ManagedChild;
use crate::security::{ApprovalRequest, Decision};
use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

const OUTPUT_CAP: usize = 8 * 1024;

pub struct Codex {
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
        native_commands: vec!["review", "compact", "fork"],
    }
}

/// (sandbox, approvalPolicy, approvalsReviewer) for a jockey profile.
fn native_policy(p: Profile) -> (&'static str, &'static str, &'static str) {
    match p {
        Profile::ReadOnly => ("read-only", "never", "user"),
        Profile::WorkspaceWrite => ("workspace-write", "on-request", "user"),
        Profile::Auto => ("workspace-write", "on-request", "auto_review"),
        Profile::Full => ("danger-full-access", "never", "user"),
    }
}

#[async_trait]
impl ProviderAdapter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn get_capabilities(&self) -> Capabilities {
        caps()
    }

    async fn probe(&self) -> Probe {
        let mut notes = vec![];
        let version = crate::process::version_of(&self.bin).await;
        if let Some(v) = &version
            && !v.contains("0.165")
        {
            notes.push(format!("verified against 0.165.0, found '{v}'"));
        }
        Probe {
            provider: "codex",
            path: crate::process::which(&self.bin),
            version,
            adapter: "app-server",
            notes,
        }
    }

    async fn start_session(
        &self,
        spec: SessionSpec,
        events: Emitter,
    ) -> Result<Arc<dyn AgentSession>> {
        let (child, rpc, rx) = self.spawn(&spec.cwd, spec.stderr_log.as_deref())?;
        let s = CodexSession::open(rpc, rx, Some(child), spec, events).await?;
        Ok(s)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (mut child, rpc, _rx) = self.spawn(&std::env::temp_dir(), None)?;
        let res = async {
            handshake(&rpc).await?;
            parse_models(&rpc.request("model/list", json!({})).await?)
        }
        .await;
        child.terminate(Duration::from_secs(2)).await;
        res
    }

    async fn get_usage(&self) -> Result<Value> {
        let (mut child, rpc, _rx) = self.spawn(&std::env::temp_dir(), None)?;
        let res = async {
            handshake(&rpc).await?;
            rpc.request("account/rateLimits/read", Value::Null).await
        }
        .await;
        child.terminate(Duration::from_secs(2)).await;
        res
    }
}

impl Codex {
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
        cmd.arg("app-server")
            .current_dir(cwd)
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

async fn handshake(rpc: &RpcClient) -> Result<()> {
    rpc.request(
        "initialize",
        json!({"clientInfo": {"name": "jockey", "title": "jockey", "version": env!("CARGO_PKG_VERSION")}}),
    )
    .await?;
    rpc.notify("initialized", None).await
}

fn parse_models(v: &Value) -> Result<Vec<ModelInfo>> {
    let data = v["data"].as_array().context("model/list: no data")?;
    Ok(data
        .iter()
        .filter(|m| !m["hidden"].as_bool().unwrap_or(false))
        .map(|m| ModelInfo {
            id: str_of(m, "id"),
            display: m["displayName"].as_str().map(String::from),
            resolved: None,
            note: None,
            bills_credits: false,
            is_default: m["isDefault"].as_bool().unwrap_or(false),
            efforts: m["supportedReasoningEfforts"]
                .as_array()
                .map(|a| a.iter().map(|e| str_of(e, "reasoningEffort")).collect())
                .unwrap_or_default(),
        })
        .collect())
}

#[derive(Default)]
struct State {
    thread_id: String,
    turn_id: Option<String>,
    done: Option<oneshot::Sender<TurnOutcome>>,
    last_text: String,
    steers: HashSet<String>,
    credits_start: Option<f64>,
    /// Thread-total (output, reasoning) tokens, and their values when the turn began.
    tokens_total: (u64, u64),
    tokens_base: (u64, u64),
    quota_tripped: Option<String>,
}

pub struct CodexSession {
    rpc: Arc<RpcClient>,
    state: Arc<Mutex<State>>,
    child: tokio::sync::Mutex<Option<ManagedChild>>,
    ev: Emitter,
    /// (model, effort) applied to every subsequent turn.
    opts: Mutex<(Option<String>, Option<String>)>,
    allow_credits: bool,
}

impl CodexSession {
    /// Opens a session over an already-started transport. `child` is None in tests.
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
            opts: Mutex::new((spec.model.clone(), spec.effort.clone())),
            allow_credits: spec.allow_credits,
        });
        tokio::spawn(pump(s.clone(), rx, spec.approver.clone()));
        if let Err(e) = s.setup(&spec).await {
            s.cancel().await;
            return Err(e);
        }
        Ok(s)
    }

    async fn setup(&self, spec: &SessionSpec) -> Result<()> {
        handshake(&self.rpc).await?;
        self.preflight_quota().await?;

        if spec.model.is_some() || spec.effort.is_some() {
            let models = parse_models(&self.rpc.request("model/list", json!({})).await?)?;
            let m = match &spec.model {
                Some(id) => models.iter().find(|m| &m.id == id).ok_or_else(|| {
                    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
                    anyhow!("codex has no model '{id}' (available: {})", ids.join(", "))
                })?,
                None => models
                    .iter()
                    .find(|m| m.is_default)
                    .context("codex reported no default model")?,
            };
            if let Some(e) = &spec.effort
                && !m.efforts.contains(e)
            {
                bail!(
                    "codex model '{}' does not support effort '{e}' (supported: {})",
                    m.id,
                    m.efforts.join(", ")
                );
            }
        }

        let (sandbox, approval, reviewer) = native_policy(spec.effective_profile());
        let mut params = json!({
            "cwd": spec.cwd,
            "sandbox": sandbox,
            "approvalPolicy": approval,
            "approvalsReviewer": reviewer,
        });
        if let Some(m) = &spec.model {
            params["model"] = json!(m);
        }
        let res = match &spec.resume_id {
            Some(id) => {
                params["threadId"] = json!(id);
                self.rpc.request("thread/resume", params).await?
            }
            None => self.rpc.request("thread/start", params).await?,
        };
        let thread_id = str_of(&res["thread"], "id");
        if thread_id.is_empty() {
            bail!("codex returned no thread id");
        }
        self.state.lock().unwrap().thread_id = thread_id.clone();
        self.ev
            .emit(EventKind::AgentStarted {
                role: serde_json::to_value(spec.role)?
                    .as_str()
                    .unwrap_or("")
                    .into(),
                session_id: Some(thread_id),
                model: res["model"].as_str().map(String::from),
                effort: spec
                    .effort
                    .clone()
                    .or_else(|| res["reasoningEffort"].as_str().map(String::from)),
            })
            .await;
        Ok(())
    }

    /// Refuses to start a turn that would be billed beyond the subscription.
    async fn preflight_quota(&self) -> Result<()> {
        let v = match self
            .rpc
            .request("account/rateLimits/read", Value::Null)
            .await
        {
            Ok(v) => v,
            // Without a starting balance neither this check nor the credit
            // watchdog can work, so fail closed unless credits are allowed.
            Err(e) if !self.allow_credits => {
                return Err(ProviderError::QuotaExhausted(format!(
                    "could not read codex rate limits ({e}), so jockey cannot tell whether this turn \
                     would draw paid credits. Retry, or pass --allow-credits to proceed anyway."
                ))
                .into());
            }
            Err(e) => {
                self.ev
                    .emit(EventKind::Warning {
                        message: format!("could not read codex rate limits: {e}"),
                    })
                    .await;
                return Ok(());
            }
        };
        let rl = &v["rateLimits"];
        if let Some(k) = rate_limit_event(rl) {
            self.ev.emit(k).await;
        }
        let balance = credits(rl);
        self.state.lock().unwrap().credits_start = balance;
        if self.allow_credits {
            return Ok(());
        }
        let exhausted = v["ordinaryUsageAllowed"] == json!(false)
            || !rl["rateLimitReachedType"].is_null()
            || rl["primary"]["usedPercent"].as_f64().unwrap_or(0.0) >= 100.0
            || rl["secondary"]["usedPercent"].as_f64().unwrap_or(0.0) >= 100.0;
        if exhausted {
            let reset = rl["primary"]["resetsAt"].as_i64().unwrap_or(0) * 1000;
            let mins = (reset - crate::events::now_ms() as i64).max(0) / 60_000;
            return Err(ProviderError::QuotaExhausted(format!(
                "codex subscription limit reached (window resets in ~{mins} min); further use would draw paid credits. \
                 Pass --allow-credits to permit that."
            ))
            .into());
        }
        Ok(())
    }

    /// Starts a turn-producing request and waits for its `turn/completed`.
    async fn run_turn(&self, method: &str, mut params: Value) -> Result<TurnOutcome> {
        // A turn whose caller went away (cancelled workflow) is being interrupted;
        // give it a moment to finish instead of refusing the next prompt.
        for _ in 0..50 {
            let orphaned = self
                .state
                .lock()
                .unwrap()
                .done
                .as_ref()
                .is_some_and(|t| t.is_closed());
            if !orphaned {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.preflight_quota().await?;
        let (tx, rx) = oneshot::channel();
        let thread_id = {
            let mut st = self.state.lock().unwrap();
            if st.turn_id.is_some() || st.done.is_some() {
                bail!("a codex turn is already running in this session");
            }
            st.done = Some(tx);
            st.last_text.clear();
            st.quota_tripped = None;
            st.tokens_base = st.tokens_total;
            st.thread_id.clone()
        };
        params["threadId"] = json!(thread_id);
        match self.rpc.request(method, params).await {
            Ok(res) => self.adopt_turn_id(res["turn"]["id"].as_str()),
            Err(e) => {
                self.state.lock().unwrap().done = None;
                return Err(e);
            }
        }
        Ok(rx.await.unwrap_or(TurnOutcome {
            status: TurnStatus::Failed,
            final_text: String::new(),
            error: Some("codex session ended before the turn completed".into()),
        }))
    }

    /// Records the running turn's id. If the credit watchdog tripped before the
    /// id was known, the interrupt it could not send goes out now.
    fn adopt_turn_id(&self, id: Option<&str>) {
        let late = {
            let mut st = self.state.lock().unwrap();
            if st.done.is_none() || st.turn_id.is_some() {
                return;
            }
            st.turn_id = id.map(String::from);
            match (&st.quota_tripped, &st.turn_id) {
                (Some(_), Some(t)) => Some((st.thread_id.clone(), t.clone())),
                _ => None,
            }
        };
        if let Some((thread_id, turn_id)) = late {
            let rpc = self.rpc.clone();
            tokio::spawn(async move {
                let p = json!({"threadId": thread_id, "turnId": turn_id});
                let _ = rpc.request("turn/interrupt", p).await;
            });
        }
    }

    fn thread_and_turn(&self) -> (String, Option<String>) {
        let st = self.state.lock().unwrap();
        (st.thread_id.clone(), st.turn_id.clone())
    }

    async fn finish_turn(&self, outcome: TurnOutcome) {
        let tx = {
            let mut st = self.state.lock().unwrap();
            st.turn_id = None;
            st.done.take()
        };
        if let Some(tx) = tx {
            let _ = tx.send(outcome);
        }
    }
}

#[async_trait]
impl AgentSession for CodexSession {
    fn provider(&self) -> &'static str {
        "codex"
    }

    fn session_id(&self) -> Option<String> {
        let id = self.state.lock().unwrap().thread_id.clone();
        (!id.is_empty()).then_some(id)
    }

    async fn execute(&self, prompt: &str) -> Result<TurnOutcome> {
        let mut params = json!({"input": [{"type": "text", "text": prompt}]});
        let (model, effort) = self.opts.lock().unwrap().clone();
        if let Some(m) = model {
            params["model"] = json!(m);
        }
        if let Some(e) = effort {
            params["effort"] = json!(e);
        }
        self.run_turn("turn/start", params).await
    }

    async fn set_turn_options(&self, model: Option<String>, effort: Option<String>) -> Result<()> {
        *self.opts.lock().unwrap() = (model, effort);
        Ok(())
    }

    async fn native_command(&self, name: &str) -> Result<Option<TurnOutcome>> {
        match name {
            "review" => {
                let p = json!({"target": {"type": "uncommittedChanges"}});
                self.run_turn("review/start", p).await.map(Some)
            }
            "compact" => {
                let thread_id = self.thread_and_turn().0;
                self.rpc
                    .request("thread/compact/start", json!({"threadId": thread_id}))
                    .await?;
                Ok(None)
            }
            other => bail!("codex has no native command '{other}' (try: review, compact)"),
        }
    }

    async fn steer(&self, text: &str) -> Result<SteerReceipt> {
        let steer_id = uuid::Uuid::now_v7().to_string();
        self.ev
            .emit(EventKind::SteeringSent {
                steer_id: steer_id.clone(),
                text: text.into(),
            })
            .await;
        let (thread_id, turn) = self.thread_and_turn();
        let Some(turn_id) = turn else {
            let reason = "no active turn".to_string();
            self.ev
                .emit(EventKind::SteeringRejected {
                    steer_id,
                    reason: reason.clone(),
                })
                .await;
            bail!(reason);
        };
        self.state.lock().unwrap().steers.insert(steer_id.clone());
        let res = self
            .rpc
            .request(
                "turn/steer",
                json!({
                    "threadId": thread_id,
                    "expectedTurnId": turn_id,
                    "input": [{"type": "text", "text": text}],
                    "clientUserMessageId": steer_id,
                }),
            )
            .await;
        match res {
            Ok(_) => {
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
            Err(e) => {
                self.state.lock().unwrap().steers.remove(&steer_id);
                self.ev
                    .emit(EventKind::SteeringRejected {
                        steer_id,
                        reason: e.to_string(),
                    })
                    .await;
                Err(e)
            }
        }
    }

    async fn interrupt(&self) -> Result<()> {
        let (thread_id, turn) = self.thread_and_turn();
        let Some(turn_id) = turn else { return Ok(()) };
        self.rpc
            .request(
                "turn/interrupt",
                json!({"threadId": thread_id, "turnId": turn_id}),
            )
            .await
            .map(|_| ())
    }

    async fn cancel(&self) {
        if let Some(mut c) = self.child.lock().await.take() {
            c.terminate(Duration::from_secs(2)).await;
        }
        self.finish_turn(TurnOutcome {
            status: TurnStatus::Interrupted,
            final_text: String::new(),
            error: Some("cancelled".into()),
        })
        .await;
    }
}

/// Routes everything the app-server sends: normalized events, raw passthrough,
/// turn bookkeeping, the credit guard, and approval requests.
async fn pump(s: Arc<CodexSession>, mut rx: mpsc::Receiver<Incoming>, approver: Arc<dyn Approver>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            Incoming::Notification { method, params } => {
                s.ev.emit(EventKind::Raw {
                    value: json!({"method": method, "params": params}),
                })
                .await;
                for k in translate(&method, &params) {
                    s.ev.emit(k).await;
                }
                bookkeeping(&s, &method, &params).await;
            }
            Incoming::Request { id, method, params } => {
                s.ev.emit(EventKind::Raw {
                    value: json!({"id": id, "method": method, "params": params}),
                })
                .await;
                // Approvals can wait on a human; never block the event stream on them.
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
    s.finish_turn(TurnOutcome {
        status: TurnStatus::Failed,
        final_text: String::new(),
        error: Some("codex app-server exited".into()),
    })
    .await;
}

async fn bookkeeping(s: &Arc<CodexSession>, method: &str, p: &Value) {
    match method {
        "turn/started" => s.adopt_turn_id(p["turn"]["id"].as_str()),
        "item/completed" => {
            let item = &p["item"];
            match item["type"].as_str() {
                Some("agentMessage") => {
                    s.state.lock().unwrap().last_text = str_of(item, "text");
                }
                Some("userMessage") => {
                    let cid = str_of(item, "clientId");
                    let hit = !cid.is_empty() && s.state.lock().unwrap().steers.remove(&cid);
                    if hit {
                        s.ev.emit(EventKind::SteeringApplied { steer_id: cid })
                            .await;
                    }
                }
                _ => {}
            }
        }
        "thread/tokenUsage/updated" => {
            let t = &p["tokenUsage"]["total"];
            let now = (
                t["outputTokens"].as_u64().unwrap_or(0),
                t["reasoningOutputTokens"].as_u64().unwrap_or(0),
            );
            let base = {
                let mut st = s.state.lock().unwrap();
                st.tokens_total = now;
                st.tokens_base
            };
            s.ev.emit(EventKind::TokensLive {
                output_tokens: now.0.saturating_sub(base.0),
                thinking_tokens: Some(now.1.saturating_sub(base.1)),
            })
            .await;
        }
        "account/rateLimits/updated" if !s.allow_credits => {
            let now = credits(&p["rateLimits"]);
            let tripped = {
                let mut st = s.state.lock().unwrap();
                match (st.credits_start, now) {
                    (Some(b0), Some(b1)) if b1 < b0 && st.quota_tripped.is_none() => {
                        let msg = format!(
                            "codex started drawing paid credits ({b0:.2} -> {b1:.2}); interrupting. \
                             Re-run with --allow-credits to permit."
                        );
                        st.quota_tripped = Some(msg.clone());
                        Some((msg, st.thread_id.clone(), st.turn_id.clone()))
                    }
                    _ => None,
                }
            };
            if let Some((msg, thread_id, turn_id)) = tripped {
                s.ev.emit(EventKind::Warning { message: msg }).await;
                // Ids captured now: the turn may complete before the request goes out.
                if let Some(turn_id) = turn_id {
                    let rpc = s.rpc.clone();
                    tokio::spawn(async move {
                        let p = json!({"threadId": thread_id, "turnId": turn_id});
                        let _ = rpc.request("turn/interrupt", p).await;
                    });
                }
            }
        }
        "turn/completed" => {
            let turn = &p["turn"];
            let (current, tripped, last) = {
                let st = s.state.lock().unwrap();
                (
                    st.turn_id.clone(),
                    st.quota_tripped.clone(),
                    st.last_text.clone(),
                )
            };
            if current.is_some() && current.as_deref() != turn["id"].as_str() {
                return;
            }
            let missed: Vec<_> = s.state.lock().unwrap().steers.drain().collect();
            for steer_id in missed {
                s.ev.emit(EventKind::SteeringRejected {
                    steer_id,
                    reason: STEER_MISSED_TURN.into(),
                })
                .await;
            }
            let final_text = turn["items"]
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .rev()
                        .find(|i| i["type"] == "agentMessage" && i["phase"] == "final_answer")
                })
                .map(|i| str_of(i, "text"))
                .unwrap_or(last);
            let err = turn["error"]["message"].as_str().map(String::from);
            let (status, error) = match (tripped, turn["status"].as_str()) {
                (Some(t), _) => (TurnStatus::Failed, Some(t)),
                (None, Some("completed")) => (TurnStatus::Completed, None),
                (None, Some("interrupted")) => (TurnStatus::Interrupted, err),
                (None, _) => (TurnStatus::Failed, err.or(Some("turn failed".into()))),
            };
            let kind = match status {
                TurnStatus::Completed => EventKind::AgentCompleted {
                    status: "completed".into(),
                    final_text: final_text.clone(),
                },
                TurnStatus::Interrupted => EventKind::AgentInterrupted {
                    reason: error.clone().unwrap_or_else(|| "interrupted".into()),
                },
                TurnStatus::Failed => EventKind::AgentFailed {
                    error: error.clone().unwrap_or_default(),
                },
            };
            s.ev.emit(kind).await;
            s.finish_turn(TurnOutcome {
                status,
                final_text,
                error,
            })
            .await;
        }
        _ => {}
    }
}

async fn answer_request(
    s: Arc<CodexSession>,
    approver: Arc<dyn Approver>,
    id: Value,
    method: String,
    p: Value,
) {
    let (kind, detail) = match method.as_str() {
        "item/commandExecution/requestApproval" => {
            let cmd = match &p["command"] {
                Value::Array(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                v => v.as_str().unwrap_or_default().to_string(),
            };
            ("command", cmd)
        }
        "item/fileChange/requestApproval" => (
            "file_change",
            p["grantRoot"]
                .as_str()
                .or(p["reason"].as_str())
                .unwrap_or("file change outside the workspace")
                .to_string(),
        ),
        "mcpServer/elicitation/request" => {
            let _ = s.rpc.respond(id, json!({"action": "decline"})).await;
            return;
        }
        _ => {
            // Dynamic tools, auth token refresh, attestation, extra permissions:
            // jockey never provides these.
            let _ = s
                .rpc
                .respond_error(id, &format!("{method} is not supported by jockey"))
                .await;
            s.ev.emit(EventKind::Warning {
                message: format!("declined unsupported codex request {method}"),
            })
            .await;
            return;
        }
    };
    let req = ApprovalRequest {
        id: str_of(&p, "itemId"),
        kind: kind.into(),
        detail,
    };
    s.ev.emit(EventKind::ApprovalRequested {
        id: req.id.clone(),
        kind: req.kind.clone(),
        detail: req.detail.clone(),
    })
    .await;
    let (decision, by) = approver.decide(&req).await;
    let word = match decision {
        Decision::Accept => "accept",
        Decision::Decline => "decline",
    };
    let _ = s.rpc.respond(id, json!({"decision": word})).await;
    s.ev.emit(EventKind::ApprovalResolved {
        id: req.id,
        decision: word.into(),
        by: by.into(),
    })
    .await;
}

/// Pure mapping from app-server notifications to normalized events.
pub fn translate(method: &str, p: &Value) -> Vec<EventKind> {
    let item = &p["item"];
    let item_type = item["type"].as_str().unwrap_or("");
    match method {
        "item/agentMessage/delta" => vec![EventKind::MessageDelta {
            item_id: str_of(p, "itemId"),
            text: str_of(p, "delta"),
        }],
        "item/reasoning/summaryTextDelta" => vec![EventKind::ReasoningDelta {
            item_id: str_of(p, "itemId"),
            text: str_of(p, "delta"),
        }],
        "item/commandExecution/outputDelta" => vec![EventKind::ToolOutputDelta {
            id: str_of(p, "itemId"),
            text: str_of(p, "delta"),
        }],
        "item/started" => match (item_type, tool_title(item)) {
            ("reasoning", _) => vec![EventKind::Activity {
                state: "thinking".into(),
                detail: None,
            }],
            ("agentMessage", _) => vec![EventKind::Activity {
                state: "writing".into(),
                detail: None,
            }],
            ("contextCompaction", _) => vec![EventKind::Activity {
                state: "compacting".into(),
                detail: None,
            }],
            (_, Some((kind, title))) => vec![EventKind::ToolStarted {
                id: str_of(item, "id"),
                kind,
                title,
            }],
            _ => vec![],
        },
        "item/completed" => match item_type {
            "agentMessage" => vec![EventKind::Message {
                item_id: str_of(item, "id"),
                text: str_of(item, "text"),
                phase: item["phase"].as_str().map(String::from),
            }],
            _ => {
                let mut out = vec![];
                if item_type == "fileChange" {
                    for c in item["changes"].as_array().into_iter().flatten() {
                        out.push(EventKind::FileChanged {
                            path: str_of(c, "path"),
                            change: match &c["kind"] {
                                Value::String(s) => s.clone(),
                                v => str_of(v, "type"),
                            },
                            diff: str_of(c, "diff"),
                        });
                    }
                }
                if let Some((kind, title)) = tool_title(item) {
                    let status = item["status"].as_str().unwrap_or("completed");
                    let exit_code = item["exitCode"].as_i64();
                    out.push(EventKind::ToolCompleted {
                        id: str_of(item, "id"),
                        kind,
                        title,
                        ok: status == "completed" && exit_code.unwrap_or(0) == 0,
                        exit_code,
                        output: item["aggregatedOutput"].as_str().map(cap_output),
                    });
                }
                out
            }
        },
        "turn/plan/updated" => vec![EventKind::Plan {
            steps: p["plan"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|s| PlanStep {
                    step: str_of(s, "step"),
                    status: str_of(s, "status"),
                })
                .collect(),
            explanation: p["explanation"].as_str().map(String::from),
        }],
        "turn/diff/updated" => vec![EventKind::Diff {
            unified: str_of(p, "diff"),
        }],
        "thread/tokenUsage/updated" => {
            let t = &p["tokenUsage"]["total"];
            vec![EventKind::UsageUpdated {
                input_tokens: t["inputTokens"].as_u64().unwrap_or(0),
                cached_input_tokens: t["cachedInputTokens"].as_u64().unwrap_or(0),
                output_tokens: t["outputTokens"].as_u64().unwrap_or(0),
                context_window: p["tokenUsage"]["modelContextWindow"].as_u64(),
            }]
        }
        "account/rateLimits/updated" => rate_limit_event(&p["rateLimits"]).into_iter().collect(),
        "error" => vec![EventKind::Warning {
            message: format!(
                "{}{}",
                p["error"]["message"].as_str().unwrap_or("codex error"),
                if p["willRetry"] == json!(true) {
                    " (retrying)"
                } else {
                    ""
                }
            ),
        }],
        "model/rerouted" | "warning" | "configWarning" | "deprecationNotice" => {
            vec![EventKind::Warning {
                message: format!(
                    "codex {method}: {}",
                    p["message"].as_str().unwrap_or(&p.to_string())
                ),
            }]
        }
        _ => vec![],
    }
}

fn tool_title(item: &Value) -> Option<(String, String)> {
    let (kind, title) = match item["type"].as_str()? {
        "commandExecution" => ("command", str_of(item, "command")),
        "fileChange" => (
            "file_change",
            item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| str_of(c, "path"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
        "mcpToolCall" => (
            "mcp",
            format!("{}/{}", str_of(item, "server"), str_of(item, "tool")),
        ),
        "dynamicToolCall" => ("tool", str_of(item, "tool")),
        "webSearch" => ("web_search", str_of(item, "query")),
        "collabAgentToolCall" | "subAgentActivity" => ("subagent", str_of(item, "id")),
        _ => return None,
    };
    Some((kind.into(), title))
}

fn rate_limit_event(rl: &Value) -> Option<EventKind> {
    let primary = rl.get("primary")?;
    Some(EventKind::RateLimit {
        used_percent: primary["usedPercent"].as_f64().unwrap_or(0.0),
        window_mins: primary["windowDurationMins"].as_u64(),
        resets_at: primary["resetsAt"].as_i64(),
        credits_balance: rl["credits"]["balance"].as_str().map(String::from),
        limit_reached: !rl["rateLimitReachedType"].is_null(),
    })
}

fn credits(rl: &Value) -> Option<f64> {
    rl["credits"]["balance"].as_str()?.parse().ok()
}

fn str_of(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn cap_output(s: &str) -> String {
    if s.len() <= OUTPUT_CAP {
        return s.to_string();
    }
    let mut end = OUTPUT_CAP;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[{} bytes truncated]", &s[..end], s.len() - end)
}

#[cfg(test)]
mod tests;
