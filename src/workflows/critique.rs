//! Mode 3: primary draft → independent critics in parallel (read-only workspace) →
//! merged findings → primary revises → … (≤ revisions) → final checks.

use super::shared::{NO_CHANGES, collect_changes, relativize, run_checks};
use super::{RunCtx, WorkflowResult, run_turn};
use crate::config::Critique as CritiqueCfg;
use crate::critics::{self, Finding};
use crate::events::EventKind;
use crate::providers::{AgentSpec, Role, TurnOutcome, TurnStatus};
use crate::snapshot::ReviewDir;
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};

/// Asks the user whose findings go back: (critic, focus, findings) per critic
/// that found something; the reply names the critics to keep.
pub type Gate = mpsc::Sender<GateAsk>;
pub type GateAsk = (Vec<(String, String, usize)>, oneshot::Sender<Vec<String>>);

pub struct CritiqueOpts {
    /// (agent, focus role) per critic.
    pub critics: Vec<(AgentSpec, String)>,
    pub revisions: u32,
    pub checks: Vec<String>,
    pub check_timeout: Duration,
    pub max_parallel_per_provider: usize,
    /// Set by the TUI when `[critique] ask` is on.
    pub gate: Option<Gate>,
    /// "auto", "coding" or "general"; ignored when roles were set explicitly.
    pub focus: String,
    pub explicit_roles: bool,
}

impl CritiqueOpts {
    /// This round's critics: general focuses for answers that aren't code.
    fn critics_for(&self, changes: &str) -> Vec<(AgentSpec, String)> {
        let general = match self.focus.as_str() {
            "general" => true,
            "coding" => false,
            _ => changes == NO_CHANGES,
        };
        if self.explicit_roles || !general {
            return self.critics.clone();
        }
        let roles = critics::GENERAL_ROLES;
        self.critics
            .iter()
            .enumerate()
            .map(|(i, (s, _))| (s.clone(), roles[i % roles.len()].to_string()))
            .collect()
    }

    /// Critics come from flags, then config; with neither, `count` critics on the primary's model.
    pub fn resolve(cfg: &CritiqueCfg, primary: &AgentSpec, flags: &[AgentSpec]) -> Result<Self> {
        let specs: Vec<AgentSpec> = if !flags.is_empty() {
            flags.to_vec()
        } else if !cfg.critics.is_empty() {
            cfg.critics
                .iter()
                .map(|c| c.parse())
                .collect::<Result<_>>()?
        } else {
            vec![primary.clone(); cfg.count.clamp(1, 8)]
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
            revisions: cfg.revisions.clamp(1, 5),
            checks: cfg.checks.clone(),
            check_timeout: Duration::from_secs(cfg.check_timeout_secs),
            max_parallel_per_provider: cfg.max_parallel_per_provider.max(1),
            gate: None,
            focus: cfg.focus.clone(),
            explicit_roles: !cfg.roles.is_empty(),
        })
    }
}

pub async fn run(
    ctx: &RunCtx,
    primary: &AgentSpec,
    o: &CritiqueOpts,
    task: &str,
) -> Result<WorkflowResult> {
    let mut sub = ctx.events.subscribe();
    let (session, assignment) = ctx
        .start_agent(primary, Role::Primary, "primary", None)
        .await?;
    let em = ctx.ev.for_agent(&primary.provider, "primary");
    let mut outcome = run_turn(&session, &em, task, ctx.timeout).await?;

    for round in 1..=o.revisions {
        if outcome.status != TurnStatus::Completed {
            break;
        }
        let changes = collect_changes(&mut sub, "primary").await;
        let critics = o.critics_for(&changes);
        let mut findings =
            review(ctx, o, &critics, round, task, &outcome.final_text, &changes).await?;
        if let Some(gate) = &o.gate {
            findings = ask(gate, &critics, findings).await;
        }
        if findings.is_empty() {
            break;
        }
        outcome = run_turn(
            &session,
            &em,
            &critics::revision_prompt(&findings),
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

/// Waits for the user to pick whose findings go back; none picked ends the rounds.
async fn ask(gate: &Gate, critics: &[(AgentSpec, String)], findings: Vec<Finding>) -> Vec<Finding> {
    if findings.is_empty() {
        return findings;
    }
    let found = critics
        .iter()
        .enumerate()
        .map(|(i, (_, role))| {
            let label = format!("critic-{}", i + 1);
            let n = findings
                .iter()
                .filter(|f| f.critics.contains(&label))
                .count();
            (label, role.clone(), n)
        })
        .filter(|c| c.2 > 0)
        .collect();
    let (tx, rx) = oneshot::channel();
    if gate.send((found, tx)).await.is_err() {
        return findings;
    }
    let keep = rx.await.unwrap_or_default();
    findings
        .into_iter()
        .filter(|f| f.critics.iter().any(|c| keep.contains(c)))
        .collect()
}

/// Runs every critic concurrently on the finished draft and merges their findings.
async fn review(
    ctx: &RunCtx,
    o: &CritiqueOpts,
    critics: &[(AgentSpec, String)],
    round: u32,
    task: &str,
    draft: &str,
    changes: &str,
) -> Result<Vec<Finding>> {
    let reason = if round == 1 {
        "draft completed"
    } else {
        "revision completed"
    };
    let dir = ReviewDir::new(&ctx.cwd, changes != NO_CHANGES, false).await?;
    let (draft, changes) = (relativize(draft, &ctx.cwd), relativize(changes, &ctx.cwd));
    review_round(
        ctx,
        dir.path(),
        critics,
        o.max_parallel_per_provider,
        round,
        reason,
        |role| critics::critic_prompt(role, task, &draft, &changes),
    )
    .await
}

/// One review round: every critic reads the same `dir`, concurrently (bounded
/// per provider); findings are merged and ranked. Shared with Autosteer.
pub(super) async fn review_round(
    ctx: &RunCtx,
    dir: &Path,
    critic_specs: &[(AgentSpec, String)],
    max_parallel_per_provider: usize,
    round: u32,
    reason: &str,
    prompt_for: impl Fn(&str) -> String,
) -> Result<Vec<Finding>> {
    let labels: Vec<String> = (1..=critic_specs.len())
        .map(|i| format!("critic-{i}"))
        .collect();
    ctx.ev
        .emit(EventKind::ReviewTriggered {
            round,
            reason: reason.into(),
            critics: labels.clone(),
            models: critic_specs.iter().map(|(s, _)| s.to_string()).collect(),
        })
        .await;
    let mut limits: HashMap<&str, Arc<Semaphore>> = HashMap::new();
    for (spec, _) in critic_specs {
        limits
            .entry(spec.provider.as_str())
            .or_insert_with(|| Arc::new(Semaphore::new(max_parallel_per_provider)));
    }
    let runs = critic_specs
        .iter()
        .zip(&labels)
        .map(|((spec, role), label)| {
            let limit = limits[spec.provider.as_str()].clone();
            let prompt = prompt_for(role);
            let path = dir.to_path_buf();
            async move {
                let _permit = limit.acquire_owned().await;
                let (findings, error) = match critic(ctx, spec, label, &path, &prompt).await {
                    Ok(f) => (f, None),
                    Err(e) => (vec![], Some(format!("{e:#}"))),
                };
                ctx.ev
                    .emit(EventKind::CritiqueReceived {
                        critic: label.clone(),
                        role: role.clone(),
                        findings: findings.clone(),
                        error,
                    })
                    .await;
                findings
            }
        });
    let all: Vec<Finding> = futures_join_all(runs).await.into_iter().flatten().collect();
    let (merged, duplicates) = critics::merge(all);
    ctx.ev
        .emit(EventKind::FindingsMerged {
            round,
            findings: merged.clone(),
            duplicates,
        })
        .await;
    Ok(merged)
}

async fn critic(
    ctx: &RunCtx,
    spec: &AgentSpec,
    label: &str,
    cwd: &Path,
    prompt: &str,
) -> Result<Vec<Finding>> {
    let (session, _) = ctx
        .start_agent_in(spec, Role::Reviewer, label, None, Some(cwd))
        .await?;
    let em = ctx.ev.for_agent(&spec.provider, label);
    let result = async {
        let out = run_turn(&session, &em, prompt, ctx.timeout).await?;
        // One nudge, never a loop: for a critic that forgot the JSON, or one whose
        // turn ended early (Grok ends the whole turn when a tool is denied).
        let nudge = match out.status {
            TurnStatus::Completed => match critics::parse_findings(&out.final_text, label) {
                Some(f) => return Ok(f),
                None => "Reply again with ONLY the JSON object described above, nothing else.",
            },
            TurnStatus::Failed if out.error.as_deref().is_some_and(|e| e.contains("denied")) => {
                DENIED_NUDGE
            }
            _ => anyhow::bail!("critic did not finish: {}", out.error.unwrap_or_default()),
        };
        let retry = run_turn(&session, &em, nudge, ctx.timeout).await?;
        if retry.status != TurnStatus::Completed {
            let e = retry.error.or(out.error).unwrap_or_default();
            anyhow::bail!("critic did not finish: {e}");
        }
        critics::parse_findings(&retry.final_text, label)
            .ok_or_else(|| anyhow::anyhow!("critic returned no parseable findings"))
    }
    .await;
    session.cancel().await;
    result
}

const DENIED_NUDGE: &str = "Your last turn stopped early, probably because a tool you tried was \
denied. You are a read-only reviewer: shell commands, web access and edits are not available. \
Use only file read and search tools, or none. Reply now with ONLY the JSON object described \
above, nothing else.";

/// Polls futures concurrently without pulling in the `futures` crate.
async fn futures_join_all<F: std::future::Future>(
    fs: impl IntoIterator<Item = F>,
) -> Vec<F::Output> {
    let mut set = Vec::new();
    for f in fs {
        set.push(Box::pin(f));
    }
    let mut out: Vec<Option<F::Output>> = (0..set.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (i, f) in set.iter_mut().enumerate() {
            if out[i].is_none() {
                match f.as_mut().poll(cx) {
                    std::task::Poll::Ready(v) => out[i] = Some(v),
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    out.into_iter().map(|o| o.unwrap()).collect()
}

#[cfg(test)]
mod tests;
