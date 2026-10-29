//! Autosteer, driven by the mock provider (no quota used). `mock-slow` keeps
//! the primary's turn running long enough for reviews to land mid-turn.

use super::*;
use crate::config::Config;
use crate::events::Event;
use crate::run::{Run, RunOptions};
use crate::security::{Profile, RulesApprover};

fn opts(policy: Policy) -> AutosteerOpts {
    AutosteerOpts {
        critics: vec![
            ("mock".parse().unwrap(), "correctness".into()),
            ("mock".parse().unwrap(), "security".into()),
        ],
        policy,
        debounce: Duration::ZERO,
        min_interval: Duration::ZERO,
        event_interval: 10_000,
        limits: Limits {
            cooldown: Duration::from_secs(60),
            max_interventions: 5,
        },
        final_review: true,
        checks: vec![],
        check_timeout: Duration::from_secs(10),
        max_parallel_per_provider: 3,
    }
}

async fn autosteer(primary: &str, o: AutosteerOpts) -> (WorkflowResult, Vec<Event>) {
    crate::testutil::env();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join("src")).unwrap();
    std::fs::write(
        ws.path().join("src/lib.rs"),
        "pub fn price(x: f64) -> f64 { x }",
    )
    .unwrap();
    let run = Run::start(RunOptions {
        cfg: Config::default(),
        cwd: ws.path().to_path_buf(),
        mode: "autosteer".into(),
        profile: Profile::WorkspaceWrite,
        allow_credits: false,
        timeout: Duration::from_secs(30),
        approver: Arc::new(RulesApprover {
            allow_commands: vec![],
        }),
        first_prompt: "price the bond".into(),
        resumed_from: None,
    })
    .unwrap();
    let ctx = run.ctx.clone();
    let res = super::run(&ctx, &primary.parse().unwrap(), &o, "price the bond")
        .await
        .unwrap();
    drop(ctx);
    let dir = run.dir.clone();
    run.finish("done", None, None).await.unwrap();
    (res, crate::sessions::read_events(&dir).unwrap())
}

fn pos(evs: &[Event], f: impl Fn(&Event) -> bool) -> Option<usize> {
    evs.iter().position(f)
}

fn decisions(evs: &[Event]) -> Vec<(String, String, String)> {
    evs.iter()
        .filter_map(|e| match &e.kind {
            EventKind::SteeringDecision {
                finding,
                action,
                reason,
                ..
            } => Some((finding.description.clone(), action.clone(), reason.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn critics_review_mid_turn_and_a_strong_finding_is_steered_live() {
    let (res, evs) = autosteer("mock:mock-slow", opts(Policy::Balanced)).await;
    assert_eq!(
        res.outcome.status,
        TurnStatus::Completed,
        "{:?}",
        res.outcome.error
    );

    let primary_done = pos(&evs, |e| {
        e.agent == "primary" && matches!(e.kind, EventKind::AgentCompleted { .. })
    })
    .unwrap();
    // 1-2. An edit triggered a review while the primary was still working.
    let review = pos(&evs, |e| matches!(&e.kind, EventKind::ReviewTriggered { reason, .. } if reason.contains("changed"))).unwrap();
    assert!(
        review < primary_done,
        "review should start before the primary finishes"
    );
    // 3. Both critics started before either reported.
    let first_report = pos(&evs, |e| {
        matches!(e.kind, EventKind::CritiqueReceived { .. })
    })
    .unwrap();
    let starts = evs[review..first_report]
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::AgentStarted { role, .. } if role == "reviewer"))
        .count();
    assert_eq!(starts, 2);
    // 5. Two critics × same findings → merged.
    assert!(evs.iter().any(
        |e| matches!(e.kind, EventKind::FindingsMerged { duplicates, .. } if duplicates >= 2)
    ));

    let ds = decisions(&evs);
    assert!(
        ds.iter()
            .any(|(d, a, _)| d.contains("discount") && a == "steer"),
        "{ds:?}"
    );
    assert!(
        ds.iter()
            .any(|(d, a, _)| d.contains("negative rates") && a == "suggest"),
        "medium → suggestion"
    );

    // 7-9. The steer went to the primary's session and was acknowledged and applied.
    let sent = pos(&evs, |e| {
        e.agent == "primary" && matches!(e.kind, EventKind::SteeringSent { .. })
    })
    .unwrap();
    let delivered = pos(&evs, |e| {
        matches!(e.kind, EventKind::SteeringDelivered { .. })
    })
    .unwrap();
    let applied = pos(&evs, |e| {
        matches!(e.kind, EventKind::SteeringApplied { .. })
    })
    .unwrap();
    assert!(sent < delivered && delivered < applied && applied < primary_done);
    assert!(
        res.outcome.final_text.contains("Applied 1 correction"),
        "{}",
        res.outcome.final_text
    );

    // 11. A later review re-raising the same issue is not steered again.
    assert!(
        ds.iter()
            .any(|(d, a, r)| d.contains("discount") && a == "reject" && r.contains("loop guard")),
        "{ds:?}"
    );
    // 15. Final validation ran after the primary finished.
    let fin = pos(&evs, |e| matches!(&e.kind, EventKind::ReviewTriggered { reason, .. } if reason == "final validation")).unwrap();
    assert!(fin > primary_done);
    // Only one intervention reached the agent.
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e.kind, EventKind::SteeringSent { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn without_a_live_turn_steer_worthy_findings_become_one_follow_up() {
    // Slow critics land after a fast primary has finished: the documented fallback.
    let mut o = opts(Policy::Balanced);
    o.critics = vec![("mock:mock-slow".parse().unwrap(), "correctness".into())];
    let (res, evs) = autosteer("mock", o).await;
    assert_eq!(res.outcome.status, TurnStatus::Completed);
    let ds = decisions(&evs);
    assert!(
        ds.iter()
            .any(|(d, a, _)| d.contains("discount") && a == "queue"),
        "{ds:?}"
    );
    let turns = evs
        .iter()
        .filter(|e| e.agent == "primary" && matches!(e.kind, EventKind::AgentCompleted { .. }))
        .count();
    assert_eq!(turns, 2, "original turn + one follow-up revision");
    assert!(
        !evs.iter()
            .any(|e| matches!(e.kind, EventKind::SteeringSent { .. }))
    );
}

#[tokio::test]
async fn suggest_policy_never_sends_but_offers_every_finding() {
    let (_, evs) = autosteer("mock:mock-slow", opts(Policy::Suggest)).await;
    assert!(
        !evs.iter()
            .any(|e| matches!(e.kind, EventKind::SteeringSent { .. }))
    );
    let suggestions: Vec<_> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::SteeringDecision {
                action,
                suggestion_id: Some(id),
                steer_text: Some(t),
                ..
            } if action == "suggest" => Some((id.clone(), t.clone())),
            _ => None,
        })
        .collect();
    assert!(suggestions.len() >= 2);
    assert!(
        suggestions
            .iter()
            .all(|(_, t)| t.contains("does not change your permissions"))
    );
}

#[tokio::test]
async fn observe_policy_only_records() {
    let (res, evs) = autosteer("mock:mock-slow", opts(Policy::Observe)).await;
    assert_eq!(res.outcome.status, TurnStatus::Completed);
    let ds = decisions(&evs);
    assert!(
        !ds.is_empty() && ds.iter().all(|(_, a, _)| a == "observe" || a == "reject"),
        "{ds:?}"
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e.kind, EventKind::SteeringSent { .. }))
    );
    assert!(!evs.iter().any(|e| matches!(&e.kind, EventKind::ReviewTriggered { reason, .. } if reason == "final validation")));
}

#[test]
fn watch_triggers_on_meaningful_events_and_detects_staleness() {
    let ev = |seq, kind| Event {
        run_id: "r".into(),
        seq,
        ts_ms: 0,
        provider: "mock".into(),
        agent: "primary".into(),
        kind,
    };
    let mut w = Watch::default();
    assert!(
        w.observe(
            &ev(
                1,
                EventKind::MessageDelta {
                    item_id: "m".into(),
                    text: "x".into()
                }
            ),
            100
        )
        .is_none()
    );
    let t = w.observe(
        &ev(
            2,
            EventKind::FileChanged {
                path: "/ws/src/lib.rs".into(),
                change: "edit".into(),
                diff: "+x".into(),
            },
        ),
        100,
    );
    assert_eq!(t.as_deref(), Some("/ws/src/lib.rs changed"));
    let failed = EventKind::ToolCompleted {
        id: "t".into(),
        kind: "command".into(),
        title: "make".into(),
        ok: false,
        exit_code: Some(2),
        output: None,
    };
    assert_eq!(
        w.observe(&ev(3, failed), 100).as_deref(),
        Some("`make` failed")
    );
    // Staleness: a finding about src/lib.rs reviewed at seq 1 is stale (changed at 2); at seq 2 it isn't.
    assert!(w.changed_after("src/lib.rs", 1));
    assert!(!w.changed_after("src/lib.rs", 2));
    assert!(!w.changed_after("src/other.rs", 0));
    // Other agents' events (critics) never trigger reviews.
    let mut critic = ev(
        4,
        EventKind::FileChanged {
            path: "x".into(),
            change: "edit".into(),
            diff: String::new(),
        },
    );
    critic.agent = "critic-1".into();
    assert!(w.observe(&critic, 100).is_none());
}
