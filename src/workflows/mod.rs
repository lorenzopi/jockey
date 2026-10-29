//! Workflows only talk to `ProviderAdapter`/`AgentSession`; no provider names here.

pub mod autosteer;
pub mod cascade;
pub mod critique;
mod shared;
pub mod single;

use crate::config::Config;
use crate::events::{Emitter, Event, EventKind};
use crate::providers::{self, AgentSession, AgentSpec, Role, SessionSpec, TurnOutcome, TurnStatus};
use crate::security::{Approver, Profile, RulesApprover};
use crate::sessions::AgentAssignment;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

/// A labelled session and how it was assigned.
type Named = (Arc<dyn AgentSession>, AgentAssignment);

pub struct RunCtx {
    pub cfg: Config,
    pub cwd: PathBuf,
    pub profile: Profile,
    /// `--allow-credits` for this run; per-provider config can also allow it.
    pub allow_credits: bool,
    pub timeout: Duration,
    pub run_dir: PathBuf,
    pub ev: Emitter,
    pub approver: Arc<dyn Approver>,
    /// The recorder's fan-out, for workflows that react to agent events.
    pub events: broadcast::Sender<Event>,
    /// Every session this run started, so shutdown can kill them all.
    live: Mutex<Vec<Arc<dyn AgentSession>>>,
    /// Sessions by label, so the UI can steer a running workflow's primary.
    named: Mutex<std::collections::HashMap<String, Named>>,
    /// Asks a running autosteer workflow for a review now (`/review`).
    pub review_now: tokio::sync::Notify,
}

pub struct WorkflowResult {
    pub outcome: TurnOutcome,
    pub agents: Vec<AgentAssignment>,
}

impl RunCtx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        cwd: PathBuf,
        profile: Profile,
        allow_credits: bool,
        timeout: Duration,
        run_dir: PathBuf,
        ev: Emitter,
        approver: Arc<dyn Approver>,
        events: broadcast::Sender<Event>,
    ) -> Self {
        Self {
            cfg,
            cwd,
            profile,
            allow_credits,
            timeout,
            run_dir,
            ev,
            approver,
            events,
            live: Mutex::default(),
            named: Mutex::default(),
            review_now: tokio::sync::Notify::new(),
        }
    }

    /// Starts an agent with config defaults filled in. `label` names it in the
    /// event log (e.g. "primary", "critic-1").
    pub async fn start_agent(
        &self,
        spec: &AgentSpec,
        role: Role,
        label: &str,
        resume_id: Option<String>,
    ) -> Result<(Arc<dyn AgentSession>, AgentAssignment)> {
        self.start_agent_in(spec, role, label, resume_id, None)
            .await
    }

    /// Like `start_agent`, but in another directory (reviewers get a snapshot).
    pub async fn start_agent_in(
        &self,
        spec: &AgentSpec,
        role: Role,
        label: &str,
        resume_id: Option<String>,
        cwd: Option<&Path>,
    ) -> Result<(Arc<dyn AgentSession>, AgentAssignment)> {
        let adapter = providers::adapter(&spec.provider, &self.cfg)?;
        // The primary is one conversation for the whole run: workflows that ask for
        // it again (every TUI prompt in a multi-agent mode) get the same session.
        let existing = match role {
            Role::Primary if label == "primary" => self.named.lock().unwrap().get(label).cloned(),
            _ => None,
        };
        if let Some((s, mut a)) = existing
            && s.provider() == spec.provider
        {
            if (spec.model.is_some() && spec.model != a.model)
                || (spec.effort.is_some() && spec.effort != a.effort)
            {
                match s
                    .set_turn_options(spec.model.clone(), spec.effort.clone())
                    .await
                {
                    Ok(()) => {
                        a.model = spec.model.clone().or(a.model);
                        a.effort = spec.effort.clone().or(a.effort);
                    }
                    Err(e) => {
                        self.ev
                            .emit(crate::events::EventKind::Warning {
                                message: format!(
                                    "{e:#}; continuing the conversation on {}",
                                    a.model.as_deref().unwrap_or("its model")
                                ),
                            })
                            .await
                    }
                }
            }
            return Ok((s, a));
        }
        // Reviewers never get to ask for more: anything not pre-allowed is denied.
        let approver: Arc<dyn Approver> = match role {
            Role::Reviewer => Arc::new(RulesApprover {
                allow_commands: vec![],
            }),
            Role::Primary => self.approver.clone(),
        };
        let pcfg = self.cfg.provider(&spec.provider);
        let model = spec.model.clone().or(pcfg.model);
        let effort = spec.effort.clone().or(pcfg.effort);
        let em = self.ev.for_agent(&spec.provider, label);
        let session = adapter
            .start_session(
                SessionSpec {
                    cwd: cwd.map_or_else(|| self.cwd.clone(), Path::to_path_buf),
                    model: model.clone(),
                    effort: effort.clone(),
                    profile: self.profile,
                    role,
                    approver,
                    allow_credits: self.allow_credits || pcfg.allow_credits,
                    resume_id,
                    stderr_log: Some(self.run_dir.join("raw").join(format!("{label}.stderr.log"))),
                },
                em,
            )
            .await?;
        self.live.lock().unwrap().push(session.clone());
        let assignment = AgentAssignment {
            role: label.into(),
            provider: spec.provider.clone(),
            model,
            effort,
            session_id: session.session_id(),
        };
        self.named
            .lock()
            .unwrap()
            .insert(label.into(), (session.clone(), assignment.clone()));
        Ok((session, assignment))
    }

    pub fn session(&self, label: &str) -> Option<Arc<dyn AgentSession>> {
        self.named
            .lock()
            .unwrap()
            .get(label)
            .map(|(s, _)| s.clone())
    }

    /// Kills every session except the primary conversation (judges, critics, fallbacks).
    pub async fn shutdown_helpers(&self) {
        let keep = self.session("primary");
        self.named
            .lock()
            .unwrap()
            .retain(|label, _| label == "primary");
        let doomed: Vec<_> = {
            let mut live = self.live.lock().unwrap();
            let (k, d): (Vec<_>, Vec<_>) = live
                .drain(..)
                .partition(|s| keep.as_ref().is_some_and(|p| Arc::ptr_eq(s, p)));
            *live = k;
            d
        };
        for s in doomed {
            s.cancel().await;
        }
    }

    /// Kills every provider process this run started. Idempotent.
    pub async fn shutdown(&self) {
        self.named.lock().unwrap().clear();
        let live: Vec<_> = self.live.lock().unwrap().drain(..).collect();
        for s in live {
            s.cancel().await;
        }
    }
}

/// One turn with the run's timeout. A timed-out agent is interrupted, then killed.
pub async fn run_turn(
    s: &Arc<dyn AgentSession>,
    em: &Emitter,
    prompt: &str,
    timeout: Duration,
) -> Result<TurnOutcome> {
    match tokio::time::timeout(timeout, s.execute(prompt)).await {
        Ok(r) => r,
        Err(_) => {
            em.emit(EventKind::AgentInterrupted {
                reason: format!("timed out after {}s", timeout.as_secs()),
            })
            .await;
            let _ = tokio::time::timeout(Duration::from_secs(5), s.interrupt()).await;
            s.cancel().await;
            Ok(TurnOutcome {
                status: TurnStatus::Failed,
                final_text: String::new(),
                error: Some("timed out".into()),
            })
        }
    }
}
