//! Cascade branches, driven by the mock provider (no quota used).

use super::*;
use crate::config::Config;
use crate::run::{Run, RunOptions};
use crate::security::{Profile, RulesApprover};

fn opts(judge: &str, fallback: &str, checks: &[&str], max_attempts: u32) -> CascadeOpts {
    CascadeOpts {
        judge: judge.parse().unwrap(),
        fallback: fallback.parse().unwrap(),
        threshold: 85,
        max_attempts,
        checks: checks.iter().map(|c| c.to_string()).collect(),
        check_timeout: Duration::from_secs(10),
    }
}

/// Runs a cascade and returns its result plus every persisted event kind.
async fn cascade(primary: &str, o: CascadeOpts, task: &str) -> (WorkflowResult, Vec<EventKind>) {
    crate::testutil::env();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("lib.rs"), "fn f() {}").unwrap();
    let run = Run::start(RunOptions {
        cfg: Config::default(),
        cwd: ws.path().to_path_buf(),
        mode: "cascade".into(),
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

fn verdicts(evs: &[EventKind]) -> Vec<(u32, u8, bool)> {
    evs.iter()
        .filter_map(|e| match e {
            EventKind::JudgeResult {
                attempt,
                score,
                accepted,
                ..
            } => Some((*attempt, *score, *accepted)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn passing_primary_is_accepted_without_escalation() {
    let (res, evs) = cascade(
        "mock",
        opts("mock", "mock:mock-deep", &["true"], 2),
        "add a feature",
    )
    .await;
    assert_eq!(res.outcome.status, TurnStatus::Completed);
    assert_eq!(verdicts(&evs), vec![(1, 92, true)]);
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventKind::CheckCompleted { ok: true, .. }))
    );
    assert!(!evs.iter().any(|e| matches!(e, EventKind::Escalated { .. })));
    assert_eq!(res.agents.len(), 1);
}

#[tokio::test]
async fn failing_judgement_escalates_to_fallback_with_feedback() {
    let (res, evs) = cascade(
        "mock:mock-weak",
        opts("mock", "mock:mock-deep", &[], 2),
        "fix the bug",
    )
    .await;
    assert_eq!(res.outcome.status, TurnStatus::Completed);
    assert!(!res.outcome.final_text.contains("WEAK DRAFT"));
    assert_eq!(verdicts(&evs), vec![(1, 40, false), (2, 92, true)]);
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventKind::Escalated { from, .. } if from == "primary"))
    );
    let roles: Vec<_> = res.agents.iter().map(|a| a.role.as_str()).collect();
    assert_eq!(roles, vec!["primary", "fallback"]);
}

#[test]
fn fallback_prompt_carries_task_prior_work_feedback_and_failing_checks() {
    let eval = Evaluation {
        score: 40,
        issues: vec!["wrong discount factor".into()],
        evidence: vec!["pricing.rs:42".into()],
        ..Default::default()
    };
    let checks = vec![CheckResult {
        command: "cargo test".into(),
        ok: false,
        exit_code: Some(101),
        output_tail: "1 failed".into(),
    }];
    let p = fallback_prompt("price bonds", "draft answer", &eval, &checks);
    for want in [
        "price bonds",
        "draft answer",
        "40/100",
        "wrong discount factor",
        "pricing.rs:42",
        "cargo test",
        "1 failed",
    ] {
        assert!(p.contains(want), "missing {want}");
    }
}

#[tokio::test]
async fn failing_check_blocks_acceptance_even_when_judge_passes() {
    let (res, evs) = cascade("mock", opts("mock", "mock", &["exit 3"], 2), "task").await;
    assert_eq!(res.outcome.status, TurnStatus::Failed);
    assert!(res.outcome.error.unwrap().contains("not accepted after 2"));
    assert_eq!(verdicts(&evs), vec![(1, 92, false), (2, 92, false)]);
    assert!(evs.iter().any(|e| matches!(
        e,
        EventKind::CheckCompleted {
            ok: false,
            exit_code: Some(3),
            ..
        }
    )));
}

#[tokio::test]
async fn attempts_are_capped() {
    let (res, evs) = cascade(
        "mock:mock-weak",
        opts("mock", "mock:mock-weak", &[], 3),
        "task",
    )
    .await;
    assert_eq!(res.outcome.status, TurnStatus::Failed);
    assert_eq!(verdicts(&evs).len(), 3);
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, EventKind::Escalated { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn unparseable_verdict_gets_one_retry() {
    let (res, evs) = cascade("mock", opts("mock", "mock", &[], 1), "MALFORMED request").await;
    assert_eq!(res.outcome.status, TurnStatus::Completed);
    assert_eq!(verdicts(&evs), vec![(1, 92, true)]);
}

#[test]
fn verdict_parsing_is_lenient_but_strict_on_required_fields() {
    let e = parse_evaluation(
        "Sure!\n```json\n{\"pass\": false, \"score\": \"71.6\", \"issues\": [\"off by one\"]}\n```",
    )
    .unwrap();
    assert_eq!(
        (e.pass, e.score, e.recommended_action.as_str()),
        (false, 72, "revise")
    );
    assert_eq!(e.issues, vec!["off by one"]);
    assert_eq!(
        parse_evaluation(r#"{"pass": true, "score": 250}"#)
            .unwrap()
            .score,
        100
    );
    assert!(
        parse_evaluation(r#"{"score": 90}"#).is_none(),
        "pass is required"
    );
    assert!(parse_evaluation("no json here").is_none());
}

#[test]
fn judge_and_fallback_must_be_chosen() {
    let cfg = crate::config::Cascade::default();
    let err = CascadeOpts::resolve(&cfg, None, None)
        .err()
        .unwrap()
        .to_string();
    assert!(err.contains("--judge"));
    let o = CascadeOpts::resolve(
        &cfg,
        Some("claude".parse().unwrap()),
        Some("codex".parse().unwrap()),
    )
    .unwrap();
    assert_eq!((o.threshold, o.max_attempts), (85, 2));
}
