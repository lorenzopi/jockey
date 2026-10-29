//! Claude Code adapter over `claude -p --input-format stream-json --output-format stream-json`.
//!
//! Verified against Claude Code 2.1.312, see docs/research/claude-code.md:
//! - A user message written mid-turn lands at the next tool boundary. The CLI
//!   reports it via `command_lifecycle` (`queued` = ack, `started` = in context).
//! - Control requests (`initialize`, `interrupt`, `set_model`, `get_usage`) and
//!   `can_use_tool` approvals share stdin/stdout with the event stream.
//! - Paid "extra usage" can be switched on by the user; the session refuses to
//!   start while it is enabled and aborts if a turn starts using overage.

use super::*;
use crate::events::EventKind;
use crate::process::ManagedChild;
use crate::security::{ApprovalRequest, Decision};
use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(60);
const STEER_ACK_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Claude {
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

/// (permission mode, extra args) for a jockey profile.
fn native_policy(p: Profile) -> (&'static str, &'static [&'static str]) {
    match p {
        Profile::ReadOnly => ("dontAsk", &["--tools", "Read,Grep,Glob"]),
        Profile::WorkspaceWrite => ("acceptEdits", &[]),
        Profile::Auto => ("auto", &[]),
        Profile::Full => ("bypassPermissions", &[]),
    }
}

/// Fable can bill usage credits instead of plan limits (Claude Code model-config docs).
fn may_bill_credits(model: &str) -> bool {
    model.to_ascii_lowercase().contains("fable")
}

#[async_trait]
impl ProviderAdapter for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn get_capabilities(&self) -> Capabilities {
        caps()
    }

    async fn probe(&self) -> Probe {
        let version = crate::process::version_of(&self.bin).await;
        let mut notes = vec![];
        if let Some(v) = &version
            && !v.contains("2.1.312")
        {
            notes.push(format!("verified against 2.1.312, found '{v}'"));
        }
        Probe {
            provider: "claude",
            path: crate::process::which(&self.bin),
            version,
            adapter: "stream-json",
            notes,
        }
    }

    async fn start_session(&self, spec: SessionSpec, ev: Emitter) -> Result<Arc<dyn AgentSession>> {
        if let Some(m) = &spec.model
            && may_bill_credits(m)
            && !spec.allow_credits
        {
            bail!("claude model '{m}' can bill usage credits; pass --allow-credits to use it");
        }
        let session_id = spec
            .resume_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        let mut args: Vec<String> = vec![];
        let (mode, extra) = native_policy(spec.effective_profile());
        args.extend(["--permission-mode".into(), mode.into()]);
        args.extend(extra.iter().map(|s| s.to_string()));
        if spec.live_profile && spec.effective_profile() != Profile::ReadOnly {
            // Lets Shift+Tab switch to yolo later; it doesn't turn bypass on.
            args.push("--allow-dangerously-skip-permissions".into());
        }
        match &spec.resume_id {
            Some(id) => args.extend(["--resume".into(), id.clone()]),
            None => args.extend(["--session-id".into(), session_id.clone()]),
        }
        if let Some(m) = &spec.model {
            args.extend(["--model".into(), m.clone()]);
        }
        if let Some(e) = &spec.effort {
            args.extend(["--effort".into(), e.clone()]);
        }
        let (child, rx_lines, writer) = self.spawn(&spec.cwd, &args, spec.stderr_log.as_deref())?;
        let s = ClaudeSession::open(rx_lines, writer, Some(child), session_id, spec, ev).await?;
        Ok(s)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let (mut child, rx, w) = self.spawn(&std::env::temp_dir(), &[], None)?;
        let wire = Wire::start(rx, w);
        let res =
            async { parse_models(&wire.0.control(json!({"subtype": "initialize"})).await?) }.await;
        child.terminate(Duration::from_secs(2)).await;
        res
    }

    async fn get_usage(&self) -> Result<Value> {
        let (mut child, rx, w) = self.spawn(&std::env::temp_dir(), &[], None)?;
        let wire = Wire::start(rx, w);
        let res = async {
            wire.0.control(json!({"subtype": "initialize"})).await?;
            wire.0.control(json!({"subtype": "get_usage"})).await
        }
        .await;
        child.terminate(Duration::from_secs(2)).await;
        res
    }
}

impl Claude {
    #[allow(clippy::type_complexity)]
    fn spawn(
        &self,
        cwd: &std::path::Path,
        extra: &[String],
        stderr_log: Option<&std::path::Path>,
    ) -> Result<(
        ManagedChild,
        tokio::process::ChildStdout,
        tokio::process::ChildStdin,
    )> {
        let stderr = match stderr_log {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        };
        let mut cmd = Command::new(&self.bin);
        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            "--permission-prompt-tool",
            "stdio",
        ])
        .args(extra)
        .current_dir(cwd)
        // Subscription sign-in only: never let an API key in the environment switch billing.
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ANTHROPIC_AUTH_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr);
        let mut child = ManagedChild::spawn(&mut cmd)
            .with_context(|| format!("is '{}' installed? try `jockey doctor`", self.bin))?;
        if let (Some(p), Some(err)) = (stderr_log, child.child.stderr.take()) {
            crate::sessions::record_stderr(err, p)?;
        }
        let stdout = child.child.stdout.take().context("no stdout")?;
        let stdin = child.child.stdin.take().context("no stdin")?;
        Ok((child, stdout, stdin))
    }
}

fn parse_models(init: &Value) -> Result<Vec<ModelInfo>> {
    let models = init["models"]
        .as_array()
        .context("claude initialize: no models")?;
    Ok(models
        .iter()
        .map(|m| {
            let id = str_of(m, "value");
            let resolved = str_of(m, "resolvedModel");
            let bills = may_bill_credits(&id) || may_bill_credits(&resolved);
            ModelInfo {
                is_default: id == "default",
                efforts: m["supportedEffortLevels"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                display: m["displayName"].as_str().map(String::from),
                resolved: m["resolvedModel"].as_str().map(String::from),
                note: bills.then(|| "may bill usage credits".to_string()),
                bills_credits: bills,
                id,
            }
        })
        .collect())
}

/// Line-delimited JSON transport with request/response for control messages.
struct Wire {
    out: mpsc::Sender<String>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>,
    next: AtomicU64,
}

impl Wire {
    fn start<R, W>(reader: R, writer: W) -> (Arc<Self>, mpsc::Receiver<Value>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, mut out_rx) = mpsc::channel::<String>(256);
        let (in_tx, in_rx) = mpsc::channel(1024);
        let wire = Arc::new(Self {
            out: out_tx,
            pending: Mutex::default(),
            next: AtomicU64::new(1),
        });
        tokio::spawn(async move {
            let mut w = writer;
            while let Some(line) = out_rx.recv().await {
                if w.write_all(line.as_bytes()).await.is_err()
                    || w.write_all(b"\n").await.is_err()
                    || w.flush().await.is_err()
                {
                    break;
                }
            }
        });
        let wr = Arc::downgrade(&wire);
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if v["type"] == "control_response" {
                    let r = &v["response"];
                    let id = str_of(r, "request_id");
                    let tx = wr
                        .upgrade()
                        .and_then(|w| w.pending.lock().unwrap().remove(&id));
                    if let Some(tx) = tx {
                        let res = if r["subtype"] == "error" {
                            Err(anyhow!(
                                "{}",
                                r["error"].as_str().unwrap_or("control request failed")
                            ))
                        } else {
                            Ok(r["response"].clone())
                        };
                        let _ = tx.send(res);
                    }
                }
                if in_tx.send(v).await.is_err() {
                    break;
                }
            }
            if let Some(w) = wr.upgrade() {
                for (_, tx) in w.pending.lock().unwrap().drain() {
                    let _ = tx.send(Err(anyhow!("claude process closed the connection")));
                }
            }
        });
        (wire, in_rx)
    }

    async fn send(&self, v: Value) -> Result<()> {
        self.out
            .send(v.to_string())
            .await
            .map_err(|_| anyhow!("claude process is gone"))
    }

    async fn control(&self, request: Value) -> Result<Value> {
        let id = format!("jockey-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let what = str_of(&request, "subtype");
        self.send(json!({"type": "control_request", "request_id": id, "request": request}))
            .await?;
        match tokio::time::timeout(CONTROL_TIMEOUT, rx).await {
            Ok(Ok(r)) => r.map_err(|e| anyhow!("claude {what}: {e}")),
            Ok(Err(_)) => Err(anyhow!("claude {what}: connection closed")),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(anyhow!("claude {what}: timed out"))
            }
        }
    }

    async fn user_message(&self, uuid: &str, text: &str) -> Result<()> {
        self.send(json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "parent_tool_use_id": null,
            "session_id": "",
            "uuid": uuid,
        }))
        .await
    }
}

#[derive(Default)]
struct State {
    done: Option<oneshot::Sender<TurnOutcome>>,
    /// Commands (prompt + steers) the CLI hasn't finished yet.
    inflight: HashSet<String>,
    /// Steer id -> ack waiter (taken on `queued`).
    steer_acks: HashMap<String, Option<oneshot::Sender<()>>>,
    applied: HashSet<String>,
    last_result: Option<Value>,
    lifecycle: bool,
    interrupting: bool,
    quota_tripped: Option<String>,
    msg_id: String,
    last_text: String,
    tools: HashMap<String, (String, String, Value)>,
    model: Option<String>,
    effort: Option<String>,
    /// Last model the CLI itself reported (versioned id).
    reported_model: Option<String>,
    /// Output tokens of finished messages this turn, and of the streaming one.
    turn_out: u64,
    msg_out: u64,
}

pub struct ClaudeSession {
    wire: Arc<Wire>,
    session_id: String,
    state: Arc<Mutex<State>>,
    child: tokio::sync::Mutex<Option<ManagedChild>>,
    ev: Emitter,
    allow_credits: bool,
    /// Started read-only: the tool allowlist can't be widened mid-session.
    read_only: bool,
}

impl ClaudeSession {
    async fn open<R, W>(
        reader: R,
        writer: W,
        child: Option<ManagedChild>,
        session_id: String,
        spec: SessionSpec,
        ev: Emitter,
    ) -> Result<Arc<Self>>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (wire, rx) = Wire::start(reader, writer);
        let s = Arc::new(Self {
            wire,
            session_id,
            state: Arc::new(Mutex::new(State {
                model: spec.model.clone(),
                effort: spec.effort.clone(),
                ..Default::default()
            })),
            child: tokio::sync::Mutex::new(child),
            ev,
            allow_credits: spec.allow_credits,
            read_only: spec.effective_profile() == Profile::ReadOnly,
        });
        tokio::spawn(pump(s.clone(), rx, spec.approver.clone()));
        if let Err(e) = s.setup(&spec).await {
            s.cancel().await;
            return Err(e);
        }
        Ok(s)
    }

    async fn setup(&self, spec: &SessionSpec) -> Result<()> {
        let init = self.wire.control(json!({"subtype": "initialize"})).await?;
        let models = parse_models(&init).unwrap_or_default();
        let current = match &spec.model {
            Some(id) => models
                .iter()
                .find(|m| &m.id == id)
                .ok_or_else(|| {
                    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
                    anyhow!("claude has no model '{id}' (available: {})", ids.join(", "))
                })
                .map(Some)?,
            None => models.iter().find(|m| m.is_default),
        };
        if let (Some(e), Some(m)) = (&spec.effort, current)
            && !m.efforts.is_empty()
            && !m.efforts.contains(e)
        {
            bail!(
                "claude model '{}' does not support effort '{e}' (supported: {})",
                m.id,
                m.efforts.join(", ")
            );
        }
        self.preflight_quota().await?;
        self.ev
            .emit(EventKind::AgentStarted {
                role: if spec.role == Role::Reviewer {
                    "reviewer"
                } else {
                    "primary"
                }
                .into(),
                session_id: Some(self.session_id.clone()),
                model: current
                    .and_then(|m| m.resolved.clone())
                    .or_else(|| spec.model.clone()),
                effort: spec.effort.clone(),
            })
            .await;
        Ok(())
    }

    /// Refuses to start while paid extra usage is switched on.
    async fn preflight_quota(&self) -> Result<()> {
        let u = match self.wire.control(json!({"subtype": "get_usage"})).await {
            Ok(u) => u,
            Err(e) => {
                self.ev
                    .emit(EventKind::Warning {
                        message: format!("could not read claude usage: {e}"),
                    })
                    .await;
                return Ok(());
            }
        };
        let rl = &u["rate_limits"];
        if let Some(pct) = rl["five_hour"]["utilization"].as_f64() {
            self.ev
                .emit(EventKind::RateLimit {
                    used_percent: pct,
                    window_mins: Some(300),
                    resets_at: None,
                    credits_balance: None,
                    limit_reached: pct >= 100.0,
                })
                .await;
        }
        let paid =
            rl["extra_usage"]["is_enabled"] == json!(true) || rl["spend"]["enabled"] == json!(true);
        if paid && !self.allow_credits {
            return Err(ProviderError::QuotaExhausted(
                "claude has paid extra usage enabled, so turns past the plan limit would be billed. \
                 Turn it off in Claude settings or pass --allow-credits."
                    .into(),
            )
            .into());
        }
        Ok(())
    }

    async fn turn(&self, text: &str) -> Result<TurnOutcome> {
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
        let uuid = uuid::Uuid::now_v7().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.state.lock().unwrap();
            if st.done.is_some() {
                bail!("a claude turn is already running in this session");
            }
            st.done = Some(tx);
            st.inflight.insert(uuid.clone());
            st.last_result = None;
            st.last_text.clear();
            st.interrupting = false;
            st.quota_tripped = None;
            st.turn_out = 0;
            st.msg_out = 0;
        }
        if let Err(e) = self.wire.user_message(&uuid, text).await {
            self.state.lock().unwrap().done = None;
            return Err(e);
        }
        Ok(rx.await.unwrap_or(TurnOutcome {
            status: TurnStatus::Failed,
            final_text: String::new(),
            error: Some("claude session ended before the turn completed".into()),
        }))
    }

    /// Completes the turn once a `result` arrived and every queued command finished.
    async fn maybe_finish(&self) {
        let (outcome, missed) = {
            let mut st = self.state.lock().unwrap();
            let Some(r) = st.last_result.clone() else {
                return;
            };
            if st.done.is_none() || (st.lifecycle && !st.interrupting && !st.inflight.is_empty()) {
                return;
            }
            let interrupted = st.interrupting;
            let text = r["result"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| st.last_text.clone());
            let outcome = if let Some(t) = st.quota_tripped.take() {
                TurnOutcome {
                    status: TurnStatus::Failed,
                    final_text: text,
                    error: Some(t),
                }
            } else if interrupted {
                TurnOutcome {
                    status: TurnStatus::Interrupted,
                    final_text: text,
                    error: None,
                }
            } else if r["subtype"] == "success" && r["is_error"] != json!(true) {
                TurnOutcome {
                    status: TurnStatus::Completed,
                    final_text: text,
                    error: None,
                }
            } else {
                let err = r["errors"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .filter(|e| !e.is_empty())
                    .unwrap_or_else(|| format!("claude turn ended: {}", str_of(&r, "subtype")));
                TurnOutcome {
                    status: TurnStatus::Failed,
                    final_text: text,
                    error: Some(err),
                }
            };
            let missed: Vec<String> = st
                .steer_acks
                .keys()
                .filter(|id| !st.applied.contains(*id))
                .cloned()
                .collect();
            st.steer_acks.clear();
            st.applied.clear();
            st.inflight.clear();
            st.last_result = None;
            st.interrupting = false;
            let tx = st.done.take();
            (
                tx.map(|t| (t, outcome)),
                if interrupted { vec![] } else { missed },
            )
        };
        for steer_id in missed {
            self.ev
                .emit(EventKind::SteeringRejected {
                    steer_id,
                    reason: STEER_MISSED_TURN.into(),
                })
                .await;
        }
        if let Some((tx, outcome)) = outcome {
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
            let _ = tx.send(outcome);
        }
    }
}

#[async_trait]
impl AgentSession for ClaudeSession {
    fn provider(&self) -> &'static str {
        "claude"
    }

    fn session_id(&self) -> Option<String> {
        Some(self.session_id.clone())
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
        let (tx, rx) = oneshot::channel();
        let active = {
            let mut st = self.state.lock().unwrap();
            let active = st.done.is_some() && !st.interrupting;
            if active {
                st.inflight.insert(steer_id.clone());
                st.steer_acks.insert(steer_id.clone(), Some(tx));
            }
            active
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
        if let Err(e) = self.wire.user_message(&steer_id, text).await {
            self.ev
                .emit(EventKind::SteeringRejected {
                    steer_id,
                    reason: e.to_string(),
                })
                .await;
            return Err(e);
        }
        let acknowledged = matches!(
            tokio::time::timeout(STEER_ACK_TIMEOUT, rx).await,
            Ok(Ok(()))
        );
        Ok(SteerReceipt {
            steer_id,
            acknowledged,
        })
    }

    async fn interrupt(&self) -> Result<()> {
        {
            let mut st = self.state.lock().unwrap();
            if st.done.is_none() {
                return Ok(());
            }
            st.interrupting = true;
        }
        self.wire
            .control(json!({"subtype": "interrupt", "cancel_queued": true}))
            .await
            .map(|_| ())
    }

    async fn set_profile(&self, profile: Profile) -> Result<()> {
        // Read-only is a tool allowlist fixed at launch; permission modes alone
        // (dontAsk) still run tools the user's Claude settings allow.
        if self.read_only != (profile == Profile::ReadOnly) {
            bail!("claude's read-only tool list is set at launch");
        }
        let mode = native_policy(profile).0;
        self.wire
            .control(json!({"subtype": "set_permission_mode", "mode": mode}))
            .await
            .map(|_| ())
    }

    async fn set_turn_options(&self, model: Option<String>, effort: Option<String>) -> Result<()> {
        let (cur_model, cur_effort) = {
            let st = self.state.lock().unwrap();
            (st.model.clone(), st.effort.clone())
        };
        if effort != cur_effort {
            bail!("claude applies effort per session");
        }
        if model != cur_model {
            let m = model.clone().unwrap_or_else(|| "default".into());
            if may_bill_credits(&m) && !self.allow_credits {
                bail!("claude model '{m}' can bill usage credits; restart with --allow-credits");
            }
            let r = self
                .wire
                .control(json!({"subtype": "set_model", "model": m}))
                .await?;
            if let Some(resolved) = r["model"].as_str() {
                self.state.lock().unwrap().reported_model = Some(resolved.into());
                self.ev
                    .emit(EventKind::ModelChanged {
                        model: resolved.into(),
                    })
                    .await;
            }
            self.state.lock().unwrap().model = model;
        }
        Ok(())
    }

    async fn native_command(&self, name: &str) -> Result<Option<TurnOutcome>> {
        match name {
            "compact" => self.turn("/compact").await.map(Some),
            other => bail!("claude has no native command '{other}' (try: compact)"),
        }
    }

    async fn cancel(&self) {
        if let Some(mut c) = self.child.lock().await.take() {
            c.terminate(Duration::from_secs(2)).await;
        }
        let tx = self.state.lock().unwrap().done.take();
        if let Some(tx) = tx {
            let _ = tx.send(TurnOutcome {
                status: TurnStatus::Interrupted,
                final_text: String::new(),
                error: Some("cancelled".into()),
            });
        }
    }
}

async fn pump(s: Arc<ClaudeSession>, mut rx: mpsc::Receiver<Value>, approver: Arc<dyn Approver>) {
    while let Some(v) = rx.recv().await {
        if v["type"] == "control_response" {
            continue; // routed to the waiting request by the transport
        }
        s.ev.emit(EventKind::Raw { value: v.clone() }).await;
        if v["type"] == "control_request" {
            tokio::spawn(answer_control(s.clone(), approver.clone(), v));
            continue;
        }
        let top_level = v["parent_tool_use_id"].is_null();
        let kinds = {
            let mut st = s.state.lock().unwrap();
            if top_level {
                translate(&mut st, &v)
            } else {
                vec![]
            }
        };
        for k in kinds {
            s.ev.emit(k).await;
        }
        bookkeeping(&s, &v).await;
    }
    let tx = s.state.lock().unwrap().done.take();
    if let Some(tx) = tx {
        let _ = tx.send(TurnOutcome {
            status: TurnStatus::Failed,
            final_text: String::new(),
            error: Some("claude process exited".into()),
        });
    }
}

async fn bookkeeping(s: &Arc<ClaudeSession>, v: &Value) {
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            let has = v["capabilities"]
                .as_array()
                .is_some_and(|c| c.iter().any(|x| x == "msg_lifecycle_v1"));
            let model = v["model"].as_str().map(String::from);
            let changed = {
                let mut st = s.state.lock().unwrap();
                st.lifecycle = has;
                let changed = model.is_some() && model != st.reported_model;
                if changed {
                    st.reported_model = model.clone();
                }
                changed
            };
            if let (true, Some(model)) = (changed, model) {
                s.ev.emit(EventKind::ModelChanged { model }).await;
            }
        }
        Some("command_lifecycle") => {
            let id = str_of(v, "command_uuid");
            let state = str_of(v, "state");
            let mut emit = vec![];
            {
                let mut st = s.state.lock().unwrap();
                st.lifecycle = true;
                let is_steer = st.steer_acks.contains_key(&id);
                match state.as_str() {
                    "queued" if is_steer => {
                        if let Some(Some(tx)) = st.steer_acks.get_mut(&id).map(Option::take) {
                            let _ = tx.send(());
                        }
                        emit.push(EventKind::SteeringDelivered {
                            steer_id: id.clone(),
                        });
                    }
                    "started" if is_steer && st.applied.insert(id.clone()) => {
                        emit.push(EventKind::SteeringApplied {
                            steer_id: id.clone(),
                        });
                    }
                    "completed" | "cancelled" => {
                        st.inflight.remove(&id);
                    }
                    _ => {}
                }
            }
            for k in emit {
                s.ev.emit(k).await;
            }
            s.maybe_finish().await;
        }
        // Fallback when lifecycle events are unavailable: the replayed echo means "consumed".
        Some("user") if v["isReplay"] == json!(true) => {
            let id = str_of(v, "uuid");
            let hit = {
                let mut st = s.state.lock().unwrap();
                st.steer_acks.contains_key(&id) && st.applied.insert(id.clone())
            };
            if hit {
                s.ev.emit(EventKind::SteeringApplied { steer_id: id }).await;
            }
        }
        Some("rate_limit_event") if !s.allow_credits => {
            let info = &v["rate_limit_info"];
            let overage = info["isUsingOverage"] == json!(true)
                || info["overageStatus"]
                    .as_str()
                    .is_some_and(|o| o != "rejected");
            let tripped = overage
                && {
                    let mut st = s.state.lock().unwrap();
                    let first = st.quota_tripped.is_none() && st.done.is_some();
                    if first {
                        st.quota_tripped = Some(
                        "claude reported paid overage for this turn; interrupted. Pass --allow-credits to permit."
                            .into(),
                    );
                    }
                    first
                };
            if tripped {
                s.ev.emit(EventKind::Warning {
                    message: "claude overage detected: interrupting to avoid paid usage".into(),
                })
                .await;
                let s2 = s.clone();
                tokio::spawn(async move {
                    let _ = s2.interrupt().await;
                });
            }
        }
        Some("result") => {
            s.state.lock().unwrap().last_result = Some(v.clone());
            s.maybe_finish().await;
        }
        _ => {}
    }
}

async fn answer_control(s: Arc<ClaudeSession>, approver: Arc<dyn Approver>, v: Value) {
    let req = &v["request"];
    let request_id = str_of(&v, "request_id");
    if req["subtype"] != "can_use_tool" {
        let _ = s
            .wire
            .send(json!({"type": "control_response", "response": {
                "subtype": "error", "request_id": request_id,
                "error": format!("{} is not supported by jockey", str_of(req, "subtype"))}}))
            .await;
        return;
    }
    let tool = str_of(req, "tool_name");
    let input = req["input"].clone();
    let (kind, detail) = match tool.as_str() {
        "Bash" => ("command", str_of(&input, "command")),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => (
            "file_change",
            format!("{tool} {}", str_of(&input, "file_path")),
        ),
        "WebFetch" => ("network", str_of(&input, "url")),
        _ => ("tool", format!("{tool}: {}", str_of(req, "description"))),
    };
    let ar = ApprovalRequest {
        id: str_of(req, "tool_use_id"),
        kind: kind.into(),
        detail,
    };
    s.ev.emit(EventKind::ApprovalRequested {
        id: ar.id.clone(),
        kind: ar.kind.clone(),
        detail: ar.detail.clone(),
    })
    .await;
    let (decision, by) = approver.decide(&ar).await;
    let response = match decision {
        Decision::Accept => json!({"behavior": "allow", "updatedInput": input}),
        Decision::Decline => {
            json!({"behavior": "deny", "message": "Denied by the user in jockey."})
        }
    };
    let _ = s
        .wire
        .send(json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": request_id, "response": response}}))
        .await;
    s.ev.emit(EventKind::ApprovalResolved {
        id: ar.id,
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

/// Maps one top-level stream-json line to normalized events.
fn translate(st: &mut State, v: &Value) -> Vec<EventKind> {
    match v["type"].as_str() {
        Some("stream_event") => {
            let e = &v["event"];
            match e["type"].as_str() {
                Some("message_start") => {
                    st.msg_id = str_of(&e["message"], "id");
                    vec![]
                }
                Some("content_block_start") => {
                    let b = &e["content_block"];
                    let (state, detail) = match b["type"].as_str() {
                        Some("thinking") | Some("redacted_thinking") => ("thinking", None),
                        Some("text") => ("writing", None),
                        Some("tool_use") => ("tool", b["name"].as_str().map(String::from)),
                        _ => return vec![],
                    };
                    vec![EventKind::Activity {
                        state: state.into(),
                        detail,
                    }]
                }
                Some("message_delta") => match e["usage"]["output_tokens"].as_u64() {
                    Some(n) => {
                        st.msg_out = n;
                        vec![EventKind::TokensLive {
                            output_tokens: st.turn_out + n,
                            thinking_tokens: None,
                        }]
                    }
                    None => vec![],
                },
                Some("message_stop") => {
                    st.turn_out += std::mem::take(&mut st.msg_out);
                    vec![]
                }
                Some("content_block_delta") => match e["delta"]["type"].as_str() {
                    Some("text_delta") => vec![EventKind::MessageDelta {
                        item_id: st.msg_id.clone(),
                        text: str_of(&e["delta"], "text"),
                    }],
                    Some("thinking_delta") => vec![EventKind::ReasoningDelta {
                        item_id: format!("{}:thinking", st.msg_id),
                        text: str_of(&e["delta"], "thinking"),
                    }],
                    _ => vec![],
                },
                _ => vec![],
            }
        }
        Some("assistant") => {
            let msg = &v["message"];
            let id = str_of(msg, "id");
            let mut out = vec![];
            for b in msg["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("text") => {
                        let text = str_of(b, "text");
                        st.last_text = text.clone();
                        out.push(EventKind::Message {
                            item_id: id.clone(),
                            text,
                            phase: None,
                        });
                    }
                    Some("tool_use") => {
                        let name = str_of(b, "name");
                        let (kind, title) = tool_title(&name, &b["input"]);
                        let tid = str_of(b, "id");
                        st.tools.insert(
                            tid.clone(),
                            (kind.clone(), title.clone(), b["input"].clone()),
                        );
                        out.push(EventKind::ToolStarted {
                            id: tid,
                            kind,
                            title,
                        });
                    }
                    _ => {}
                }
            }
            out
        }
        Some("user") if v["isReplay"] != json!(true) => {
            let mut out = vec![];
            for b in v["message"]["content"].as_array().into_iter().flatten() {
                if b["type"] != "tool_result" {
                    continue;
                }
                let tid = str_of(b, "tool_use_id");
                let ok = b["is_error"] != json!(true);
                let output = match &b["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                let (kind, title, input) = st
                    .tools
                    .remove(&tid)
                    .unwrap_or_else(|| ("tool".into(), "tool".into(), Value::Null));
                if ok && kind == "file_change" {
                    out.extend(file_changes(&title, &input));
                }
                out.push(EventKind::ToolCompleted {
                    id: tid,
                    kind,
                    title,
                    ok,
                    exit_code: None,
                    output: Some(output),
                });
            }
            out
        }
        Some("result") => {
            let u = &v["usage"];
            let input = u["input_tokens"].as_u64().unwrap_or(0)
                + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let window = v["modelUsage"]
                .as_object()
                .and_then(|m| m.values().find_map(|x| x["contextWindow"].as_u64()));
            vec![EventKind::UsageUpdated {
                input_tokens: input,
                cached_input_tokens: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                context_window: window,
            }]
        }
        Some("rate_limit_event") => {
            let i = &v["rate_limit_info"];
            let w = &i["unifiedWindows"]["five_hour"];
            vec![EventKind::RateLimit {
                used_percent: w["utilization"].as_f64().unwrap_or(0.0) * 100.0,
                window_mins: Some(300),
                resets_at: w["resetsAt"].as_i64().or(i["resetsAt"].as_i64()),
                credits_balance: None,
                limit_reached: i["status"] == "rejected",
            }]
        }
        Some("system") => match v["subtype"].as_str() {
            Some("compact_boundary") => {
                let m = &v["compact_metadata"];
                vec![EventKind::Warning {
                    message: format!(
                        "context compacted ({} → {} tokens)",
                        m["pre_tokens"], m["post_tokens"]
                    ),
                }]
            }
            Some("permission_denied") => {
                // The CLI appends a long advisory paragraph; the first sentence says it all.
                let msg = str_of(v, "message");
                let first = msg.split(". ").next().unwrap_or(&msg).trim_end_matches('.');
                vec![EventKind::Warning {
                    message: format!("{first}."),
                }]
            }
            Some("thinking_tokens") => vec![EventKind::TokensLive {
                output_tokens: st.turn_out + st.msg_out,
                thinking_tokens: v["estimated_tokens"].as_u64(),
            }],
            Some("status") if v["status"] == "compacting" => vec![EventKind::Activity {
                state: "compacting".into(),
                detail: None,
            }],
            Some("api_retry") => vec![
                EventKind::Activity {
                    state: "retrying".into(),
                    detail: None,
                },
                EventKind::Warning {
                    message: format!(
                        "claude retrying: {}",
                        v["error"].as_str().unwrap_or("api error")
                    ),
                },
            ],
            _ => vec![],
        },
        _ => vec![],
    }
}

fn tool_title(name: &str, input: &Value) -> (String, String) {
    let (kind, title) = match name {
        "Bash" => ("command", str_of(input, "command")),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            ("file_change", str_of(input, "file_path"))
        }
        "Read" => ("read", format!("Read {}", str_of(input, "file_path"))),
        "Grep" => ("search", format!("Grep {}", str_of(input, "pattern"))),
        "Glob" => ("search", format!("Glob {}", str_of(input, "pattern"))),
        "WebFetch" => ("web", str_of(input, "url")),
        "WebSearch" => ("web_search", str_of(input, "query")),
        "Task" | "Agent" => ("subagent", str_of(input, "description")),
        n if n.starts_with("mcp__") => ("mcp", n.trim_start_matches("mcp__").replace("__", "/")),
        n => ("tool", n.to_string()),
    };
    (kind.into(), title)
}

/// Synthesized diffs for Edit/Write/MultiEdit tool inputs.
fn file_changes(path: &str, input: &Value) -> Vec<EventKind> {
    let edit = |old: &str, new: &str| {
        let mut d = String::from("@@\n");
        for l in old.lines() {
            d.push_str(&format!("-{l}\n"));
        }
        for l in new.lines() {
            d.push_str(&format!("+{l}\n"));
        }
        d
    };
    let diff = if let Some(content) = input["content"].as_str() {
        content
            .lines()
            .take(400)
            .map(|l| format!("+{l}\n"))
            .collect()
    } else if let Some(edits) = input["edits"].as_array() {
        edits
            .iter()
            .map(|e| edit(&str_of(e, "old_string"), &str_of(e, "new_string")))
            .collect()
    } else {
        edit(&str_of(input, "old_string"), &str_of(input, "new_string"))
    };
    let change = if input["content"].is_string() {
        "write"
    } else {
        "edit"
    };
    vec![EventKind::FileChanged {
        path: path.into(),
        change: change.into(),
        diff,
    }]
}

fn str_of(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests;
