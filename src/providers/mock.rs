//! Deterministic fake agent (`--agent mock`). Used by tests and for trying the
//! TUI without spending provider quota. Never touches the filesystem.
//!
//! Prompt keywords change the script: `approve` asks for a command approval,
//! `fail` ends the turn with an error. Steering is acknowledged immediately
//! and applied at the next step, like a real tool boundary.

use super::*;
use crate::events::{EventKind, PlanStep};
use crate::security::{ApprovalRequest, Decision};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

pub struct Mock {
    pub delay: Duration,
}

impl Default for Mock {
    fn default() -> Self {
        let ms = std::env::var("JOCKEY_MOCK_DELAY_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(40);
        Self {
            delay: Duration::from_millis(ms),
        }
    }
}

#[async_trait]
impl ProviderAdapter for Mock {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn get_capabilities(&self) -> Capabilities {
        Capabilities {
            stream_events: true,
            live_steer: true,
            steer_ack: true,
            interrupt: true,
            approvals_to_client: true,
            read_only_mode: true,
            list_models: true,
            effort: true,
            usage: true,
            native_commands: vec!["review"],
            ..Default::default()
        }
    }

    async fn probe(&self) -> Probe {
        Probe {
            provider: "mock",
            path: None,
            version: Some("built-in".into()),
            adapter: "mock",
            notes: vec![],
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![
            ModelInfo {
                id: "mock-fast".into(),
                is_default: true,
                efforts: vec!["low".into(), "high".into()],
                display: Some("Mock Fast".into()),
                resolved: None,
                note: None,
                bills_credits: false,
            },
            ModelInfo {
                id: "mock-weak".into(),
                is_default: false,
                efforts: vec![],
                display: Some("Mock Weak".into()),
                resolved: None,
                note: Some("drafts always fail judgement".into()),
                bills_credits: false,
            },
            ModelInfo {
                id: "mock-slow".into(),
                is_default: false,
                efforts: vec![],
                display: Some("Mock Slow".into()),
                resolved: None,
                note: Some("20× slower, for watching autosteer".into()),
                bills_credits: false,
            },
            ModelInfo {
                id: "mock-deep".into(),
                is_default: false,
                efforts: vec!["low".into(), "medium".into(), "high".into()],
                display: Some("Mock Deep".into()),
                resolved: None,
                note: None,
                bills_credits: false,
            },
        ])
    }

    async fn start_session(&self, spec: SessionSpec, ev: Emitter) -> Result<Arc<dyn AgentSession>> {
        let id = format!("mock-{}", uuid::Uuid::now_v7());
        let model = spec.model.clone().unwrap_or_else(|| "mock-fast".into());
        ev.emit(EventKind::AgentStarted {
            role: if spec.role == Role::Reviewer {
                "reviewer"
            } else {
                "primary"
            }
            .into(),
            session_id: Some(id.clone()),
            model: Some(model.clone()),
            effort: spec.effort.clone(),
        })
        .await;
        Ok(Arc::new(MockSession {
            id,
            role: spec.role,
            weak: model == "mock-weak",
            delay_scale: if model == "mock-slow" { 20 } else { 1 },
            ev,
            delay: self.delay,
            approver: spec.approver,
            opts: Mutex::new((Some(model), spec.effort)),
            running: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            wake: Notify::new(),
            steers: Mutex::default(),
            tokens: AtomicU64::new(0),
            items: AtomicU64::new(0),
        }))
    }
}

pub struct MockSession {
    id: String,
    role: Role,
    /// `mock-weak`: final answers are marked as weak drafts for judges to reject.
    weak: bool,
    delay_scale: u32,
    ev: Emitter,
    delay: Duration,
    approver: Arc<dyn Approver>,
    opts: Mutex<(Option<String>, Option<String>)>,
    running: AtomicBool,
    interrupted: AtomicBool,
    wake: Notify,
    steers: Mutex<VecDeque<(String, String)>>,
    tokens: AtomicU64,
    items: AtomicU64,
}

/// Returned by script steps when the turn was interrupted.
struct Stop;

impl MockSession {
    fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.items.fetch_add(1, Ordering::Relaxed))
    }

    async fn pause(&self, units: u32) -> Result<(), Stop> {
        if self.interrupted.load(Ordering::SeqCst) {
            return Err(Stop);
        }
        tokio::select! {
            _ = tokio::time::sleep(self.delay * units * self.delay_scale) => {}
            _ = self.wake.notified() => {}
        }
        if self.interrupted.load(Ordering::SeqCst) {
            Err(Stop)
        } else {
            Ok(())
        }
    }

    /// Reviewer replies arrive in one piece; only the primary streams word by word.
    async fn say_once(&self, text: &str) -> Result<(), Stop> {
        let item_id = self.next_id("msg-");
        self.pause(5).await?;
        self.ev
            .emit(EventKind::Message {
                item_id,
                text: text.into(),
                phase: Some("final_answer".into()),
            })
            .await;
        Ok(())
    }

    async fn say(&self, text: &str, phase: &str) -> Result<(), Stop> {
        let item_id = self.next_id("msg-");
        let mut first = true;
        for word in text.split_inclusive(' ') {
            if !first {
                self.pause(1).await?;
            }
            first = false;
            self.ev
                .emit(EventKind::MessageDelta {
                    item_id: item_id.clone(),
                    text: word.into(),
                })
                .await;
        }
        self.ev
            .emit(EventKind::Message {
                item_id,
                text: text.into(),
                phase: Some(phase.into()),
            })
            .await;
        Ok(())
    }

    async fn tool(&self, title: &str, ok: bool, output: &str) -> Result<(), Stop> {
        let id = self.next_id("exec-");
        self.ev
            .emit(EventKind::ToolStarted {
                id: id.clone(),
                kind: "command".into(),
                title: title.into(),
            })
            .await;
        self.pause(4).await?;
        self.ev
            .emit(EventKind::ToolCompleted {
                id,
                kind: "command".into(),
                title: title.into(),
                ok,
                exit_code: Some(if ok { 0 } else { 1 }),
                output: Some(output.into()),
            })
            .await;
        Ok(())
    }

    async fn plan(&self, done: usize) {
        let steps = ["Inspect the code", "Implement the change", "Run the tests"];
        let steps = steps
            .iter()
            .enumerate()
            .map(|(i, s)| PlanStep {
                step: (*s).into(),
                status: match i.cmp(&done) {
                    std::cmp::Ordering::Less => "completed",
                    std::cmp::Ordering::Equal => "inProgress",
                    std::cmp::Ordering::Greater => "pending",
                }
                .into(),
            })
            .collect();
        self.ev
            .emit(EventKind::Plan {
                steps,
                explanation: None,
            })
            .await;
    }

    /// The "tool boundary": queued steering is applied here.
    async fn apply_steers(&self, applied: &mut Vec<String>) -> Result<(), Stop> {
        let pending: Vec<_> = self.steers.lock().unwrap().drain(..).collect();
        for (id, text) in pending {
            self.ev
                .emit(EventKind::SteeringApplied { steer_id: id })
                .await;
            self.say(
                &format!("Noted: {text}. Adjusting the approach."),
                "commentary",
            )
            .await?;
            applied.push(text);
        }
        Ok(())
    }

    /// Reviewer behavior: a JSON verdict that fails weak drafts.
    async fn judge(&self, prompt: &str) -> Result<TurnOutcome, Stop> {
        if let Some(progress) = prompt.split("<progress>\n").nth(1) {
            return self
                .watch(progress.split("</progress>").next().unwrap_or(""))
                .await;
        }
        if let Some(draft) = prompt.split("<draft>\n").nth(1) {
            return self
                .critique(draft.split("</draft>").next().unwrap_or(""))
                .await;
        }
        let response = prompt
            .split("<agent_response>\n")
            .nth(1)
            .and_then(|r| r.split("</agent_response>").next())
            .unwrap_or("");
        let text = if response.contains("MALFORMED") {
            "Looks fine to me overall.".to_string()
        } else if response.contains("WEAK DRAFT") {
            r#"{"pass": false, "score": 40, "issues": ["the result is only a weak draft"], "evidence": ["agent_response starts with WEAK DRAFT"], "recommended_action": "escalate"}"#.to_string()
        } else {
            r#"{"pass": true, "score": 92, "issues": [], "evidence": ["agent_response reports passing tests"], "recommended_action": "accept"}"#.to_string()
        };
        self.say_once(&text).await?;
        Ok(TurnOutcome {
            status: TurnStatus::Completed,
            final_text: text,
            error: None,
        })
    }

    /// Critic behavior: every critic reports the same nit (to exercise dedupe),
    /// plus a high-severity finding for weak drafts.
    async fn critique(&self, draft: &str) -> Result<TurnOutcome, Stop> {
        let mut findings = vec![
            r#"{"severity": "low", "confidence": 0.6, "description": "Add a doc comment to price", "evidence": "src/lib.rs:1 has no doc comment", "proposed_fix": "Document price()", "file": "src/lib.rs", "line": 1}"#,
        ];
        if draft.contains("WEAK DRAFT") {
            findings.push(r#"{"severity": "high", "confidence": 0.9, "description": "The draft skips the edge cases", "evidence": "the draft is marked WEAK DRAFT", "proposed_fix": "Handle empty and negative inputs", "file": "src/lib.rs", "line": 2}"#);
        }
        let text = format!("{{\"findings\": [{}]}}", findings.join(", "));
        self.say_once(&text).await?;
        Ok(TurnOutcome {
            status: TurnStatus::Completed,
            final_text: text,
            error: None,
        })
    }

    /// Autosteer critic behavior: once src/lib.rs has been edited, report a
    /// steer-worthy bug and a medium suggestion.
    async fn watch(&self, progress: &str) -> Result<TurnOutcome, Stop> {
        let text = if progress.contains("src/lib.rs") {
            r#"{"findings": [{"severity": "high", "confidence": 0.9, "description": "price() ignores the discount factor", "evidence": "src/lib.rs:2 returns x * 1.0", "proposed_fix": "multiply by exp(-r*t)", "file": "src/lib.rs", "line": 2}, {"severity": "medium", "confidence": 0.6, "description": "No test covers negative rates", "evidence": "cargo test ran 3 tests, none with r < 0", "proposed_fix": "add a negative-rate test", "file": "tests/price.rs", "line": null}]}"#
        } else {
            r#"{"findings": []}"#
        };
        self.say_once(text).await?;
        Ok(TurnOutcome {
            status: TurnStatus::Completed,
            final_text: text.into(),
            error: None,
        })
    }

    async fn script(&self, prompt: &str) -> Result<TurnOutcome, Stop> {
        if self.role == Role::Reviewer {
            return self.judge(prompt).await;
        }
        let mut applied = vec![];
        self.plan(0).await;
        self.say("I'll look at the code first.", "commentary")
            .await?;
        self.tool("ls", true, "Cargo.toml\nsrc\n").await?;
        self.apply_steers(&mut applied).await?;
        self.plan(1).await;

        if prompt.contains("approve") {
            let req = ApprovalRequest {
                id: self.next_id("approval-"),
                kind: "command".into(),
                detail: "rm -rf build".into(),
            };
            self.ev
                .emit(EventKind::ApprovalRequested {
                    id: req.id.clone(),
                    kind: req.kind.clone(),
                    detail: req.detail.clone(),
                })
                .await;
            let (d, by) = self.approver.decide(&req).await;
            let word = if d == Decision::Accept {
                "accept"
            } else {
                "decline"
            };
            self.ev
                .emit(EventKind::ApprovalResolved {
                    id: req.id,
                    decision: word.into(),
                    by: by.into(),
                })
                .await;
            if d == Decision::Accept {
                self.tool("rm -rf build", true, "").await?;
            } else {
                self.say(
                    "The cleanup was declined; continuing without it.",
                    "commentary",
                )
                .await?;
            }
        }

        self.pause(3).await?;
        self.ev
            .emit(EventKind::FileChanged {
                path: "src/lib.rs".into(),
                change: "update".into(),
                diff: "@@ -1,3 +1,4 @@\n pub fn price(x: f64) -> f64 {\n-    x\n+    // mock change\n+    x * 1.0\n }\n".into(),
            })
            .await;
        self.apply_steers(&mut applied).await?;
        self.plan(2).await;
        let ok = !prompt.contains("fail");
        self.tool(
            "cargo test",
            ok,
            if ok {
                "test result: ok. 3 passed"
            } else {
                "test result: FAILED. 1 failed"
            },
        )
        .await?;
        self.apply_steers(&mut applied).await?;
        let t = self.tokens.fetch_add(1200, Ordering::Relaxed) + 1200;
        self.ev
            .emit(EventKind::UsageUpdated {
                input_tokens: t,
                cached_input_tokens: t / 2,
                output_tokens: t / 10,
                context_window: Some(200_000),
            })
            .await;
        if !ok {
            let error = "tests failed (mock)".to_string();
            self.ev
                .emit(EventKind::AgentFailed {
                    error: error.clone(),
                })
                .await;
            return Ok(TurnOutcome {
                status: TurnStatus::Failed,
                final_text: String::new(),
                error: Some(error),
            });
        }
        self.plan(3).await;
        let summary: String = prompt.chars().take(60).collect();
        let mut text = format!("Done with \"{summary}\". Tests pass.");
        if self.weak {
            text.insert_str(0, "WEAK DRAFT. ");
        }
        if !applied.is_empty() {
            text.push_str(&format!(" Applied {} correction(s).", applied.len()));
        }
        self.say(&text, "final_answer").await?;
        self.ev
            .emit(EventKind::AgentCompleted {
                status: "completed".into(),
                final_text: text.clone(),
            })
            .await;
        Ok(TurnOutcome {
            status: TurnStatus::Completed,
            final_text: text,
            error: None,
        })
    }
}

#[async_trait]
impl AgentSession for MockSession {
    fn provider(&self) -> &'static str {
        "mock"
    }

    fn session_id(&self) -> Option<String> {
        Some(self.id.clone())
    }

    async fn execute(&self, prompt: &str) -> Result<TurnOutcome> {
        if self.running.swap(true, Ordering::SeqCst) {
            bail!("a mock turn is already running");
        }
        // A dropped turn (cancelled workflow) must not wedge the session.
        struct Clear<'a>(&'a AtomicBool);
        impl Drop for Clear<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _running = Clear(&self.running);
        self.interrupted.store(false, Ordering::SeqCst);
        let out = match self.script(prompt).await {
            Ok(o) => o,
            Err(Stop) => {
                self.ev
                    .emit(EventKind::AgentInterrupted {
                        reason: "interrupted".into(),
                    })
                    .await;
                TurnOutcome {
                    status: TurnStatus::Interrupted,
                    final_text: String::new(),
                    error: None,
                }
            }
        };
        let missed: Vec<_> = {
            let mut q = self.steers.lock().unwrap();
            self.running.store(false, Ordering::SeqCst);
            q.drain(..).collect()
        };
        for (steer_id, _) in missed {
            self.ev
                .emit(EventKind::SteeringRejected {
                    steer_id,
                    reason: STEER_MISSED_TURN.into(),
                })
                .await;
        }
        Ok(out)
    }

    async fn steer(&self, text: &str) -> Result<SteerReceipt> {
        let steer_id = uuid::Uuid::now_v7().to_string();
        self.ev
            .emit(EventKind::SteeringSent {
                steer_id: steer_id.clone(),
                text: text.into(),
            })
            .await;
        // Check and enqueue under one lock so a turn ending now can't lose it.
        let accepted = {
            let mut q = self.steers.lock().unwrap();
            let running = self.running.load(Ordering::SeqCst);
            if running {
                q.push_back((steer_id.clone(), text.into()));
            }
            running
        };
        if !accepted {
            self.ev
                .emit(EventKind::SteeringRejected {
                    steer_id,
                    reason: "no active turn".into(),
                })
                .await;
            bail!("no active turn");
        }
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
        self.interrupted.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        Ok(())
    }

    async fn set_turn_options(&self, model: Option<String>, effort: Option<String>) -> Result<()> {
        *self.opts.lock().unwrap() = (model, effort);
        Ok(())
    }

    async fn native_command(&self, name: &str) -> Result<Option<TurnOutcome>> {
        if name != "review" {
            bail!("mock has no native command '{name}' (try: review)");
        }
        let text = "Review: no issues found in the mock workspace.".to_string();
        let _ = self.say(&text, "final_answer").await;
        Ok(Some(TurnOutcome {
            status: TurnStatus::Completed,
            final_text: text,
            error: None,
        }))
    }

    async fn cancel(&self) {
        let _ = self.interrupt().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::RulesApprover;
    use tokio::sync::mpsc;

    fn spec() -> SessionSpec {
        SessionSpec {
            cwd: "/tmp".into(),
            model: None,
            effort: None,
            profile: crate::security::Profile::WorkspaceWrite,
            role: Role::Primary,
            approver: Arc::new(RulesApprover {
                allow_commands: vec![],
            }),
            allow_credits: false,
            resume_id: None,
            stderr_log: None,
        }
    }

    #[tokio::test]
    async fn steer_is_acked_then_applied_at_next_boundary() {
        let (tx, mut rx) = mpsc::channel(10_000);
        let ev = Emitter::new("r", "mock", "primary", tx);
        let m = Mock {
            delay: Duration::from_millis(2),
        };
        let s = m.start_session(spec(), ev).await.unwrap();
        let s2 = s.clone();
        let run = tokio::spawn(async move { s2.execute("do it").await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let r = s.steer("use continuous compounding").await.unwrap();
        let out = run.await.unwrap().unwrap();
        assert_eq!(out.status, TurnStatus::Completed);
        assert!(out.final_text.contains("Applied 1 correction"));
        let mut kinds = vec![];
        while let Ok(e) = rx.try_recv() {
            kinds.push(e.kind);
        }
        let pos = |f: &dyn Fn(&EventKind) -> bool| kinds.iter().position(f).unwrap();
        let delivered = pos(
            &|e| matches!(e, EventKind::SteeringDelivered { steer_id } if *steer_id == r.steer_id),
        );
        let applied = pos(
            &|e| matches!(e, EventKind::SteeringApplied { steer_id } if *steer_id == r.steer_id),
        );
        assert!(delivered < applied);
    }

    #[tokio::test]
    async fn interrupt_stops_turn() {
        let (tx, _rx) = mpsc::channel(10_000);
        let ev = Emitter::new("r", "mock", "primary", tx);
        let s = Mock {
            delay: Duration::from_millis(50),
        }
        .start_session(spec(), ev)
        .await
        .unwrap();
        let s2 = s.clone();
        let run = tokio::spawn(async move { s2.execute("x").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.interrupt().await.unwrap();
        let out = tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(out.status, TurnStatus::Interrupted);
    }
}
