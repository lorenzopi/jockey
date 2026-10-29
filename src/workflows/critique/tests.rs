//! Critique branches, driven by the mock provider (no quota used).

use super::*;
use crate::config::{Config, Critique as CritiqueCfg};
use crate::run::{Run, RunOptions};
use crate::security::{Profile, RulesApprover};

async fn critique(primary: &str, o: CritiqueOpts, task: &str) -> (WorkflowResult, Vec<EventKind>) {
    crate::testutil::env();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("lib.rs"), "fn f() {}").unwrap();
    let run = Run::start(RunOptions {
        cfg: Config::default(),
        cwd: ws.path().to_path_buf(),
        mode: "critique".into(),
        profile: Profile::WorkspaceWrite,
        allow_credits: false,
        timeout: Duration::from_secs(30),
        approver: Arc::new(RulesApprover {
            allow_commands: vec![],
        }),
        first_prompt: task.into(),
        resumed_from: None,
    })
    .unwrap();
    let ctx = run.ctx.clone();
    let res = super::run(&ctx, &primary.parse().unwrap(), &o, task)
        .await
        .unwrap();
    drop(ctx);
    let dir = run.dir.clone();
    run.finish("done", None, None).await.unwrap();
    let kinds = crate::sessions::read_events(&dir)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    (res, kinds)
}

fn opts(primary: &str, cfg: CritiqueCfg) -> CritiqueOpts {
    CritiqueOpts::resolve(&cfg, &primary.parse().unwrap(), &[]).unwrap()
}

#[test]
fn critics_default_to_the_primary_model_with_distinct_roles() {
    let o = opts("claude:haiku@low", CritiqueCfg::default());
    let specs: Vec<_> = o
        .critics
        .iter()
        .map(|(s, r)| (s.model.as_deref().unwrap(), r.as_str()))
        .collect();
    assert_eq!(
        specs,
        vec![
            ("haiku", "correctness"),
            ("haiku", "security"),
            ("haiku", "performance")
        ]
    );
    assert!(
        o.critics
            .iter()
            .all(|(s, _)| s.provider == "claude" && s.effort.as_deref() == Some("low"))
    );
    let flags: Vec<AgentSpec> = vec!["grok".parse().unwrap(), "codex".parse().unwrap()];
    let o =
        CritiqueOpts::resolve(&CritiqueCfg::default(), &"claude".parse().unwrap(), &flags).unwrap();
    assert_eq!(o.critics.len(), 2, "explicit critics win");
}

#[tokio::test]
async fn critics_run_on_one_snapshot_findings_merge_and_primary_revises() {
    let (res, evs) = critique(
        "mock:mock-weak",
        opts("mock:mock-weak", CritiqueCfg::default()),
        "price bonds",
    )
    .await;
    assert_eq!(res.outcome.status, TurnStatus::Completed);

    let received: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            EventKind::CritiqueReceived {
                critic,
                findings,
                error: None,
                ..
            } => Some((critic.clone(), findings.len())),
            _ => None,
        })
        .collect();
    assert_eq!(received.len(), 3);
    assert!(received.iter().all(|(_, n)| *n == 2));

    let (findings, dups) = evs
        .iter()
        .find_map(|e| match e {
            EventKind::FindingsMerged {
                findings,
                duplicates,
                ..
            } => Some((findings.clone(), *duplicates)),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (findings.len(), dups),
        (2, 4),
        "3 critics × 2 identical findings → 2"
    );
    assert_eq!(
        findings[0].severity,
        crate::critics::Severity::High,
        "ranked most severe first"
    );
    assert_eq!(findings[0].critics.len(), 3);

    // The primary got a revision turn after the review (two AgentCompleted from "primary").
    let review_at = evs
        .iter()
        .position(|e| matches!(e, EventKind::ReviewTriggered { .. }))
        .unwrap();
    assert!(
        evs[review_at..]
            .iter()
            .any(|e| matches!(e, EventKind::AgentCompleted { .. }))
    );
    assert_eq!(res.agents.len(), 1);
}

#[tokio::test]
async fn gate_lets_the_user_pick_whose_findings_go_back() {
    for (keep, revises) in [(vec![], false), (vec!["critic-2".to_string()], true)] {
        let (gtx, mut grx) = mpsc::channel(1);
        let mut o = opts("mock:mock-weak", CritiqueCfg::default());
        o.gate = Some(gtx);
        let answer = keep.clone();
        let asked = tokio::spawn(async move {
            let (critics, reply) = grx.recv().await.unwrap();
            reply.send(answer).unwrap();
            critics
        });
        let (_, evs) = critique("mock:mock-weak", o, "price bonds").await;
        let critics = asked.await.unwrap();
        assert_eq!(critics.len(), 3, "every critic with findings is offered");
        assert_eq!(critics[1], ("critic-2".into(), "security".into(), 2));
        let merged = evs
            .iter()
            .position(|e| matches!(e, EventKind::FindingsMerged { .. }))
            .unwrap();
        let revised = evs[merged..]
            .iter()
            .any(|e| matches!(e, EventKind::AgentCompleted { .. }));
        assert_eq!(revised, revises, "keep {keep:?}");
    }
}

#[tokio::test]
async fn critic_whose_turn_ends_on_a_denied_tool_gets_one_retry() {
    let (_, evs) = critique("mock", opts("mock", CritiqueCfg::default()), "deny-tool").await;
    let errors: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            EventKind::CritiqueReceived { error, .. } => Some(error.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(errors, vec![None, None, None], "every critic recovered");
}

#[tokio::test]
async fn critics_run_concurrently() {
    // 3 critics on one provider with parallelism 3 should overlap: all three start
    // before any reports back.
    let (_, evs) = critique("mock", opts("mock", CritiqueCfg::default()), "task").await;
    let starts: Vec<_> = evs
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e, EventKind::AgentStarted { role, .. } if role == "reviewer"))
        .map(|(i, _)| i)
        .collect();
    let first_report = evs
        .iter()
        .position(|e| matches!(e, EventKind::CritiqueReceived { .. }))
        .unwrap();
    assert_eq!(starts.len(), 3);
    assert!(
        starts.iter().all(|i| *i < first_report),
        "a critic waited for another to finish"
    );
}

#[tokio::test]
async fn failing_final_check_fails_the_run() {
    let cfg = CritiqueCfg {
        checks: vec!["exit 2".into()],
        ..Default::default()
    };
    let (res, evs) = critique("mock", opts("mock", cfg), "task").await;
    assert_eq!(res.outcome.status, TurnStatus::Failed);
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventKind::CheckCompleted { ok: false, .. }))
    );
}

#[tokio::test]
async fn revisions_are_capped() {
    let cfg = CritiqueCfg {
        revisions: 2,
        count: 1,
        ..Default::default()
    };
    let (_, evs) = critique("mock", opts("mock", cfg), "task").await;
    let rounds = evs
        .iter()
        .filter(|e| matches!(e, EventKind::ReviewTriggered { .. }))
        .count();
    assert_eq!(
        rounds, 2,
        "the nit keeps coming back, so both rounds run and then it stops"
    );
}
