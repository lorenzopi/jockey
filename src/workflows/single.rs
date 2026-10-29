//! Mode 1: one agent, one turn.

use super::{RunCtx, WorkflowResult, run_turn};
use crate::providers::{AgentSpec, Role};
use anyhow::Result;

pub async fn run(
    ctx: &RunCtx,
    agent: &AgentSpec,
    prompt: &str,
    resume_id: Option<String>,
) -> Result<WorkflowResult> {
    let (session, mut assignment) = ctx
        .start_agent(agent, Role::Primary, "primary", resume_id)
        .await?;
    let em = ctx.ev.for_agent(&agent.provider, "primary");
    let outcome = run_turn(&session, &em, prompt, ctx.timeout).await?;
    assignment.session_id = session.session_id();
    Ok(WorkflowResult {
        outcome,
        agents: vec![assignment],
    })
}
