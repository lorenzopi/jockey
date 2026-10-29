//! Mode 4: Autosteer. The primary works; critics review its observable
//! progress at checkpoints; the steering controller decides what to do with
//! each finding and corrections reach the running turn live.
//!
//! ```text
//! primary turn ──events──► checkpoint detector ──debounce/interval──► review round (snapshot, critics ∥)
//!      ▲                                                                       │
//!      └──── steer (live) / interrupt + replan ◄── controller (policy, dedupe, stale, cooldown, cap)
//! after the turn: in-flight review → final review → queued follow-up turn → checks
//! ```

use super::critique::review_round;
use super::shared::{relativize, run_checks, truncate};
use super::{RunCtx, WorkflowResult, run_turn};
use crate::config::Autosteer as Cfg;
use crate::critics::{self, Finding};
use crate::events::{Event, EventKind};
use crate::providers::{self, AgentSession, AgentSpec, Role, TurnOutcome, TurnStatus};
use crate::steering::{self, Action, Controller, Limits, Policy};
use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::Instant;

pub struct AutosteerOpts {
    pub critics: Vec<(AgentSpec, String)>,
    pub policy: Policy,
    pub debounce: Duration,
    pub min_interval: Duration,
    pub event_interval: usize,
    pub limits: Limits,
    pub final_review: bool,
    pub checks: Vec<String>,
    pub check_timeout: Duration,
    pub max_parallel_per_provider: usize,
}

impl AutosteerOpts {
    /// Critics come from flags, then config; with neither, `count` critics on the primary's model.
    pub fn resolve(
        cfg: &Cfg,
        primary: &AgentSpec,
        flags: &[AgentSpec],
        policy: Option<Policy>,
    ) -> Result<Self> {
        let specs: Vec<AgentSpec> = if !flags.is_empty() {
            flags.to_vec()
        } else if !cfg.critics.is_empty() {
            cfg.critics
                .iter()
                .map(|c| c.parse())
                .collect::<Result<_>>()?
        } else {
            vec![primary.clone(); cfg.count.clamp(1, 6)]
        };
        let roles: Vec<String> = if cfg.roles.is_empty() {
            critics::DEFAULT_ROLES
                .iter()
                .map(|r| r.to_string())
                .collect()
        } else {
            cfg.roles.clone()
        };
        Ok(Self {
            critics: specs
                .into_iter()
                .enumerate()
                .map(|(i, s)| (s, roles[i % roles.len()].clone()))
                .collect(),
            policy: policy.unwrap_or(cfg.policy),
            debounce: Duration::from_millis(cfg.debounce_ms),
            min_interval: Duration::from_secs(cfg.min_review_interval_secs),
            event_interval: cfg.event_interval.max(1),
            limits: Limits {
                cooldown: Duration::from_secs(cfg.cooldown_secs),
                max_interventions: cfg.max_interventions,
            },
            final_review: cfg.final_review,
            checks: cfg.checks.clone(),
            check_timeout: Duration::from_secs(cfg.check_timeout_secs),
            max_parallel_per_provider: cfg.max_parallel_per_provider.max(1),
        })
    }
}

/// What critics get to see: the primary's observable activity (never hidden reasoning).
#[derive(Default)]
struct Watch {
    seq: u64,
    lines: VecDeque<String>,
    since_trigger: usize,
    /// Last event seq that changed each file, for staleness checks.
    file_seq: HashMap<String, u64>,
}

const PROGRESS_LINES: usize = 80;

impl Watch {
    /// Records a primary event; returns a checkpoint reason when it warrants a review.
    fn observe(&mut self, ev: &Event, every: usize) -> Option<String> {
        self.seq = self.seq.max(ev.seq);
        if ev.agent != "primary" {
            return None;
        }
        self.since_trigger += 1;
        let trigger = match &ev.kind {
            EventKind::FileChanged { path, change, diff } => {
                self.file_seq.insert(path.clone(), ev.seq);
                self.push(format!("{change} {path}\n{}", truncate(diff, 1_500)));
                Some(format!("{path} changed"))
            }
            EventKind::ToolCompleted {
                title, ok, output, ..
            } => {
                let out = output
                    .as_deref()
                    .map(|o| truncate(o, 400))
                    .unwrap_or_default();
                self.push(format!(
                    "ran `{title}` → {}\n{out}",
                    if *ok { "ok" } else { "FAILED" }
                ));
                if !ok {
                    Some(format!("`{title}` failed"))
                } else if title.contains("test") {
                    Some("tests ran".into())
                } else {
                    None
                }
            }
            EventKind::Message { text, .. } => {
                self.push(format!("agent said: {}", truncate(text, 600)));
                None
            }
            EventKind::Plan { steps, .. } => {
                let p: Vec<_> = steps
                    .iter()
                    .map(|s| format!("{} [{}]", s.step, s.status))
                    .collect();
                self.push(format!("plan: {}", p.join("; ")));
                None
            }
            EventKind::SteeringApplied { .. } => {
                self.push("(a reviewer correction was applied)".into());
                None
            }
            _ => None,
        };
        let trigger =
            trigger.or_else(|| (self.since_trigger >= every).then(|| "activity".to_string()));
        if trigger.is_some() {
            self.since_trigger = 0;
        }
        trigger
    }

    fn push(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > PROGRESS_LINES {
            self.lines.pop_front();
        }
    }

    fn progress(&self) -> String {
        if self.lines.is_empty() {
            return "(no observable activity yet)".into();
        }
        self.lines.iter().cloned().collect::<Vec<_>>().join("\n\n")
    }

    /// Did `file` change after event `seq`? Paths may be absolute or relative.
    fn changed_after(&self, file: &str, seq: u64) -> bool {
        let f = file.trim_start_matches("./");
        self.file_seq
            .iter()
            .any(|(p, s)| *s > seq && (p.ends_with(f) || f.ends_with(p.as_str())))
    }
}

type Review<'a> = Pin<Box<dyn Future<Output = Result<(u64, Vec<Finding>)>> + Send + 'a>>;

pub async fn run(
    ctx: &RunCtx,
    primary: &AgentSpec,
    o: &AutosteerOpts,
    task: &str,
) -> Result<WorkflowResult> {
    let mut sub = ctx.events.subscribe();
    let (session, assignment) = ctx
        .start_agent(primary, Role::Primary, "primary", None)
        .await?;
    let live_capable = providers::adapter(&primary.provider, &ctx.cfg)?
        .get_capabilities()
        .live_steer;
    let em = ctx.ev.for_agent(&primary.provider, "primary");
    let mut ctl = Controller::new(o.policy, o.limits.clone());
    let mut watch = Watch::default();
    let mut sent: Vec<String> = vec![];
    let mut queued: Vec<Finding> = vec![];
    // Steers acknowledged but not yet applied, by steer id.
    let mut inflight: HashMap<String, Vec<Finding>> = HashMap::new();
    let mut replan: Option<String> = None;
    let mut replans = 0;
    let mut round = 0;

    let start_turn = |prompt: String| {
        let (s, em, t) = (session.clone(), em.clone(), ctx.timeout);
        Box::pin(async move { run_turn(&s, &em, &prompt, t).await })
    };
    let mut turn = start_turn(task.to_string());
    let mut review: Option<Review> = None;
    let mut pending: Option<String> = None;
    let mut due = Instant::now();
    let mut last_review: Option<Instant> = None;

    let outcome: TurnOutcome = loop {
        tokio::select! {
            out = &mut turn => {
                let out = out?;
                if out.status == TurnStatus::Interrupted
                    && replans < 2
                    && let Some(p) = replan.take()
                {
                    replans += 1;
                    turn = start_turn(p);
                    continue;
                }
                break out;
            }
            ev = sub.recv() => match ev {
                Ok(ev) => {
                    missed_steer(&ev, &mut inflight, &mut queued);
                    if let Some(reason) = watch.observe(&ev, o.event_interval) {
                        if pending.is_none() {
                            let earliest = last_review.map_or(Instant::now(), |t| t + o.min_interval);
                            due = (Instant::now() + o.debounce).max(earliest);
                        }
                        pending = Some(reason);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {}
            },
            _ = ctx.review_now.notified() => {
                pending = Some("requested by the user".into());
                due = Instant::now();
            }
            _ = tokio::time::sleep_until(due), if pending.is_some() && review.is_none() => {
                round += 1;
                last_review = Some(Instant::now());
                review = Some(start_review(ctx, o, round, task, &watch, &sent, pending.take().unwrap()));
            }
            r = async { review.as_mut().unwrap().await }, if review.is_some() => {
                review = None;
                let (seq, findings) = r?;
                let live = live_capable;
                if let Some(p) = act(ctx, &session, &mut ctl, &watch, findings, seq, live, &mut sent, &mut queued, &mut inflight).await {
                    replan = Some(p);
                }
            }
        }
    };
    drop(turn);
    // A steer acknowledged but not applied before the turn ended is reported just
    // after it; catch those so they become follow-ups instead of vanishing.
    while let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(300), sub.recv()).await {
        missed_steer(&ev, &mut inflight, &mut queued);
    }

    // After the turn: finish an in-flight review, then a final one.
    let mut outcome = outcome;
    if let Some(r) = review.take() {
        let (seq, findings) = r.await?;
        act(
            ctx,
            &session,
            &mut ctl,
            &watch,
            findings,
            seq,
            false,
            &mut sent,
            &mut queued,
            &mut inflight,
        )
        .await;
    }
    if outcome.status == TurnStatus::Completed && o.final_review && o.policy != Policy::Observe {
        round += 1;
        let (seq, findings) = start_review(
            ctx,
            o,
            round,
            task,
            &watch,
            &sent,
            "final validation".into(),
        )
        .await?;
        act(
            ctx,
            &session,
            &mut ctl,
            &watch,
            findings,
            seq,
            false,
            &mut sent,
            &mut queued,
            &mut inflight,
        )
        .await;
    }
    // Steer-worthy findings that had no live channel become one follow-up turn.
    if outcome.status == TurnStatus::Completed && !queued.is_empty() {
        ctl.record_intervention(Instant::now().into_std());
        outcome = run_turn(
            &session,
            &em,
            &critics::revision_prompt(&queued),
            ctx.timeout,
        )
        .await?;
    }
    if outcome.status == TurnStatus::Completed {
        let checks = run_checks(ctx, &o.checks, o.check_timeout).await;
        if let Some(c) = checks.iter().find(|c| !c.ok) {
            outcome = TurnOutcome {
                status: TurnStatus::Failed,
                error: Some(format!("final check failed: {}", c.command)),
                ..outcome
            };
        }
    }
    Ok(WorkflowResult {
        outcome,
        agents: vec![assignment],
    })
}

fn missed_steer(
    ev: &Event,
    inflight: &mut HashMap<String, Vec<Finding>>,
    queued: &mut Vec<Finding>,
) {
    match &ev.kind {
        EventKind::SteeringApplied { steer_id } => {
            inflight.remove(steer_id);
        }
        EventKind::SteeringRejected { steer_id, reason }
            if reason.starts_with(providers::STEER_MISSED_TURN) =>
        {
            if let Some(fs) = inflight.remove(steer_id) {
                queued.extend(fs);
            }
        }
        _ => {}
    }
}

fn start_review<'a>(
    ctx: &'a RunCtx,
    o: &'a AutosteerOpts,
    round: u32,
    task: &'a str,
    watch: &Watch,
    sent: &[String],
    reason: String,
) -> Review<'a> {
    // Freeze what critics see now; the primary keeps working meanwhile.
    let seq = watch.seq;
    let progress = relativize(&watch.progress(), &ctx.cwd);
    let sent = sent.to_vec();
    Box::pin(async move {
        let findings = review_round(
            ctx,
            &o.critics,
            o.max_parallel_per_provider,
            round,
            &reason,
            |role| critics::progress_prompt(role, task, &progress, &sent),
        )
        .await?;
        Ok((seq, findings))
    })
}

/// Applies the controller to one round of findings and delivers the result.
/// Returns a replan prompt when the decision was to interrupt.
#[allow(clippy::too_many_arguments)]
async fn act(
    ctx: &RunCtx,
    session: &Arc<dyn AgentSession>,
    ctl: &mut Controller,
    watch: &Watch,
    findings: Vec<Finding>,
    seq: u64,
    live: bool,
    sent: &mut Vec<String>,
    queued: &mut Vec<Finding>,
    inflight: &mut HashMap<String, Vec<Finding>>,
) -> Option<String> {
    let now = Instant::now().into_std();
    let mut steer = vec![];
    let mut interrupt = vec![];
    for f in findings {
        let stale = f
            .file
            .as_deref()
            .is_some_and(|p| watch.changed_after(p, seq));
        let d = ctl.decide(&f, stale, live, now);
        let suggestion =
            matches!(d.action, Action::Suggest).then(|| uuid::Uuid::now_v7().to_string());
        let text = matches!(
            d.action,
            Action::Suggest | Action::Steer | Action::Interrupt
        )
        .then(|| steering::steer_text(&[&f]));
        ctx.ev
            .emit(EventKind::SteeringDecision {
                finding: f.clone(),
                action: d.action.as_str().into(),
                reason: d.reason,
                suggestion_id: suggestion,
                steer_text: text,
            })
            .await;
        match d.action {
            Action::Steer => steer.push(f),
            Action::Interrupt => interrupt.push(f),
            Action::Queue => queued.push(f),
            _ => {}
        }
    }
    if !interrupt.is_empty() {
        ctl.record_intervention(now);
        interrupt.extend(steer);
        sent.extend(interrupt.iter().map(|f| f.description.clone()));
        let all: Vec<&Finding> = interrupt.iter().collect();
        let _ = session.interrupt().await;
        return Some(format!(
            "{}\n\nYou were interrupted because of the problem above. Replan with it in mind and continue the original task.",
            steering::steer_text(&all)
        ));
    }
    if !steer.is_empty() {
        let batch: Vec<&Finding> = steer.iter().collect();
        match session.steer(&steering::steer_text(&batch)).await {
            Ok(receipt) => {
                ctl.record_intervention(now);
                sent.extend(steer.iter().map(|f| f.description.clone()));
                inflight.insert(receipt.steer_id, steer);
            }
            Err(e) => {
                // Not delivered (e.g. the turn just ended): deliver as a follow-up instead.
                ctx.ev
                    .emit(EventKind::Warning {
                        message: format!("steer not delivered ({e:#}); queued as a follow-up"),
                    })
                    .await;
                queued.extend(steer);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;
