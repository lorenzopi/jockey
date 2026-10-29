//! Codex adapter tests against an in-memory fake app-server that replays a
//! trace recorded from codex-cli 0.165.0 (tests/fixtures/codex_turn.jsonl).

use super::*;
use crate::events::Event;
use crate::security::RulesApprover;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const FIXTURE: &str = include_str!("../../../tests/fixtures/codex_turn.jsonl");

fn fixture() -> Vec<Value> {
    FIXTURE
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn ids() -> (String, String) {
    let f = fixture();
    let started = f.iter().find(|m| m["method"] == "turn/started").unwrap();
    (
        started["params"]["threadId"].as_str().unwrap().into(),
        started["params"]["turn"]["id"].as_str().unwrap().into(),
    )
}

#[derive(Default, Clone)]
struct Fake {
    quota_exhausted: bool,
    pause_for_steer: bool,
    /// Ack the steer but never echo it into the turn (it missed the boundary).
    drop_steer_echo: bool,
    approval_cmd: Option<String>,
}

type Seen = Arc<Mutex<Vec<Value>>>;

async fn run_fake(io: tokio::io::DuplexStream, f: Fake, seen: Seen) {
    let (thread, turn) = ids();
    let (r, mut w) = tokio::io::split(io);
    let mut lines = BufReader::new(r).lines();
    macro_rules! send {
        ($v:expr) => {{
            let s = $v.to_string() + "\n";
            if w.write_all(s.as_bytes()).await.is_err() {
                return;
            }
        }};
    }
    macro_rules! next {
        () => {{
            let Ok(Some(l)) = lines.next_line().await else {
                return;
            };
            let m: Value = serde_json::from_str(&l).unwrap();
            seen.lock().unwrap().push(m.clone());
            m
        }};
    }
    loop {
        let m = next!();
        let id = m["id"].clone();
        let result = match m["method"].as_str().unwrap_or("") {
            "initialize" => json!({}),
            "account/rateLimits/read" => json!({
                "ordinaryUsageAllowed": !f.quota_exhausted,
                "rateLimits": {
                    "primary": {"usedPercent": if f.quota_exhausted {100} else {10}, "windowDurationMins": 300, "resetsAt": 1793233167},
                    "secondary": {"usedPercent": 16},
                    "credits": {"hasCredits": true, "balance": "64176.1987522500"},
                    "rateLimitReachedType": if f.quota_exhausted { json!("rate_limit_reached") } else { Value::Null },
                }
            }),
            "model/list" => json!({"data": [
                {"id": "gpt-6.1-sol", "isDefault": true, "hidden": false,
                 "supportedReasoningEfforts": [{"reasoningEffort": "low"}, {"reasoningEffort": "high"}]}
            ]}),
            "thread/start" | "thread/resume" => {
                json!({"thread": {"id": thread}, "model": "gpt-6.1-sol"})
            }
            "turn/interrupt" => json!({}),
            "turn/start" => {
                send!(json!({"jsonrpc": "2.0", "id": id, "result": {"turn": {"id": turn}}}));
                if let Some(cmd) = &f.approval_cmd {
                    send!(
                        json!({"jsonrpc": "2.0", "id": 99, "method": "item/commandExecution/requestApproval",
                                 "params": {"itemId": "exec-1", "threadId": thread, "turnId": turn, "command": cmd}})
                    );
                    while next!()["id"] != json!(99) {}
                }
                let mut paused = !f.pause_for_steer;
                for n in fixture() {
                    send!(n);
                    let first_cmd_done = n["method"] == "item/completed"
                        && n["params"]["item"]["type"] == "commandExecution";
                    if first_cmd_done && !paused {
                        paused = true;
                        let steer = next!();
                        assert_eq!(steer["method"], "turn/steer");
                        send!(
                            json!({"jsonrpc": "2.0", "id": steer["id"], "result": {"turnId": turn}})
                        );
                        if f.drop_steer_echo {
                            continue;
                        }
                        send!(
                            json!({"jsonrpc": "2.0", "method": "item/completed", "params": {
                            "threadId": thread, "turnId": turn,
                            "item": {"type": "userMessage", "id": "u2",
                                     "clientId": steer["params"]["clientUserMessageId"],
                                     "content": [{"type": "text", "text": "steer"}]}}})
                        );
                    }
                }
                continue;
            }
            _ if id.is_null() => continue, // notification
            other => {
                send!(
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": other}})
                );
                continue;
            }
        };
        send!(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }
}

fn spec(allow_credits: bool) -> SessionSpec {
    SessionSpec {
        cwd: "/tmp".into(),
        model: None,
        effort: Some("high".into()),
        profile: Profile::WorkspaceWrite,
        role: Role::Primary,
        approver: Arc::new(RulesApprover {
            allow_commands: vec!["cargo test".into()],
        }),
        allow_credits,
        resume_id: None,
        stderr_log: None,
    }
}

async fn open(
    f: Fake,
    spec: SessionSpec,
) -> (Result<Arc<CodexSession>>, mpsc::Receiver<Event>, Seen) {
    let (client, server) = tokio::io::duplex(1 << 20);
    let seen: Seen = Arc::default();
    tokio::spawn(run_fake(server, f, seen.clone()));
    let (r, w) = tokio::io::split(client);
    let (rpc, rx) = RpcClient::start(r, w);
    let (tx, erx) = mpsc::channel(100_000);
    let ev = Emitter::new("run", "codex", "primary", tx);
    let s = CodexSession::open(rpc, rx, None, spec, ev).await;
    (s, erx, seen)
}

fn drain(rx: &mut mpsc::Receiver<Event>) -> Vec<EventKind> {
    let mut v = vec![];
    while let Ok(e) = rx.try_recv() {
        v.push(e.kind);
    }
    v
}

fn count(evs: &[EventKind], f: impl Fn(&EventKind) -> bool) -> usize {
    evs.iter().filter(|e| f(e)).count()
}

#[tokio::test]
async fn completes_turn_and_normalizes_events() {
    let (s, mut rx, _) = open(Fake::default(), spec(true)).await;
    let s = s.unwrap();
    let out = s.execute("go").await.unwrap();
    assert_eq!(out.status, TurnStatus::Completed);
    assert!(out.final_text.contains("FINISHED"), "{}", out.final_text);
    let evs = drain(&mut rx);
    let (thread, _) = ids();
    assert!(evs.iter().any(
        |e| matches!(e, EventKind::AgentStarted { session_id: Some(id), .. } if *id == thread)
    ));
    assert_eq!(
        count(&evs, |e| matches!(e, EventKind::ToolStarted { .. })),
        4
    );
    assert_eq!(
        count(&evs, |e| matches!(
            e,
            EventKind::ToolCompleted { ok: true, .. }
        )),
        4
    );
    assert!(count(&evs, |e| matches!(e, EventKind::MessageDelta { .. })) > 10);
    assert!(count(&evs, |e| matches!(e, EventKind::UsageUpdated { .. })) > 0);
    assert!(count(&evs, |e| matches!(e, EventKind::Raw { .. })) >= fixture().len());
    assert!(matches!(evs.last(), Some(EventKind::AgentCompleted { .. })));
    // Per-turn live counts come from thread/tokenUsage/updated.
    assert!(
        evs.iter().any(
            |e| matches!(e, EventKind::TokensLive { output_tokens, .. } if *output_tokens > 0)
        )
    );
}

#[tokio::test]
async fn refuses_to_start_when_subscription_exhausted() {
    let f = Fake {
        quota_exhausted: true,
        ..Default::default()
    };
    let (s, _, _) = open(f, spec(false)).await;
    let err = s.err().expect("must refuse");
    assert!(matches!(
        err.downcast_ref::<ProviderError>(),
        Some(ProviderError::QuotaExhausted(_))
    ));
}

#[tokio::test]
async fn credit_drain_mid_turn_interrupts_and_fails() {
    // Fixture balance drops from 64176.19 to 64172.48 during the turn.
    let (s, _, seen) = open(Fake::default(), spec(false)).await;
    let out = s.unwrap().execute("go").await.unwrap();
    assert_eq!(out.status, TurnStatus::Failed);
    assert!(out.error.unwrap().contains("paid credits"));
    for _ in 0..50 {
        if seen
            .lock()
            .unwrap()
            .iter()
            .any(|m| m["method"] == "turn/interrupt")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no turn/interrupt sent");
}

#[tokio::test]
async fn steer_reaches_active_turn_and_is_acknowledged() {
    let f = Fake {
        pause_for_steer: true,
        ..Default::default()
    };
    let (s, mut rx, seen) = open(f, spec(true)).await;
    let s = s.unwrap();
    let runner = tokio::spawn({
        let s = s.clone();
        async move { s.execute("go").await }
    });
    while s.thread_and_turn().1.is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let receipt = s.steer("write FINISHED instead").await.unwrap();
    assert!(receipt.acknowledged);
    assert_eq!(runner.await.unwrap().unwrap().status, TurnStatus::Completed);

    let (thread, turn) = ids();
    let req = seen
        .lock()
        .unwrap()
        .iter()
        .find(|m| m["method"] == "turn/steer")
        .cloned()
        .unwrap();
    assert_eq!(req["params"]["threadId"], json!(thread));
    assert_eq!(req["params"]["expectedTurnId"], json!(turn));
    assert_eq!(
        req["params"]["clientUserMessageId"],
        json!(receipt.steer_id)
    );

    let evs = drain(&mut rx);
    let id = &receipt.steer_id;
    for want in ["sent", "delivered", "applied"] {
        assert!(
            evs.iter().any(|e| match (want, e) {
                ("sent", EventKind::SteeringSent { steer_id, .. }) => steer_id == id,
                ("delivered", EventKind::SteeringDelivered { steer_id }) => steer_id == id,
                ("applied", EventKind::SteeringApplied { steer_id }) => steer_id == id,
                _ => false,
            }),
            "missing steering {want}"
        );
    }
}

#[tokio::test]
async fn steer_without_active_turn_is_rejected() {
    let (s, mut rx, _) = open(Fake::default(), spec(true)).await;
    assert!(s.unwrap().steer("x").await.is_err());
    assert!(
        drain(&mut rx)
            .iter()
            .any(|e| matches!(e, EventKind::SteeringRejected { .. }))
    );
}

async fn approval_decision(cmd: &str) -> Value {
    let f = Fake {
        approval_cmd: Some(cmd.into()),
        ..Default::default()
    };
    let (s, _, seen) = open(f, spec(true)).await;
    s.unwrap().execute("go").await.unwrap();
    let resp = seen
        .lock()
        .unwrap()
        .iter()
        .find(|m| m["id"] == json!(99) && m.get("method").is_none())
        .cloned()
        .unwrap();
    resp["result"]["decision"].clone()
}

#[tokio::test]
async fn approvals_follow_rules_and_deny_by_default() {
    assert_eq!(
        approval_decision("/bin/zsh -lc 'cargo test'").await,
        "accept"
    );
    assert_eq!(approval_decision("rm -rf build").await, "decline");
}

#[tokio::test]
async fn reviewer_role_is_forced_read_only() {
    let mut sp = spec(true);
    sp.role = Role::Reviewer;
    sp.profile = Profile::Full;
    let (s, _, seen) = open(Fake::default(), sp).await;
    s.unwrap();
    let start = seen
        .lock()
        .unwrap()
        .iter()
        .find(|m| m["method"] == "thread/start")
        .cloned()
        .unwrap();
    assert_eq!(start["params"]["sandbox"], "read-only");
    assert_eq!(start["params"]["approvalPolicy"], "never");
}

#[tokio::test]
async fn unsupported_effort_fails_at_startup() {
    let mut sp = spec(true);
    sp.effort = Some("ultra".into());
    let (s, _, _) = open(Fake::default(), sp).await;
    let msg = s.err().unwrap().to_string();
    assert!(msg.contains("does not support effort 'ultra'"), "{msg}");
}

#[tokio::test]
async fn acked_steer_that_misses_the_turn_is_reported() {
    let f = Fake {
        pause_for_steer: true,
        drop_steer_echo: true,
        ..Default::default()
    };
    let (s, mut rx, _) = open(f, spec(true)).await;
    let s = s.unwrap();
    let runner = tokio::spawn({
        let s = s.clone();
        async move { s.execute("go").await }
    });
    while s.thread_and_turn().1.is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let r = s.steer("late").await.unwrap();
    runner.await.unwrap().unwrap();
    let evs = drain(&mut rx);
    assert!(evs.iter().any(|e| matches!(e,
        EventKind::SteeringRejected { steer_id, reason }
            if *steer_id == r.steer_id && reason.starts_with(STEER_MISSED_TURN))));
    assert!(
        !evs.iter()
            .any(|e| matches!(e, EventKind::SteeringApplied { .. }))
    );
}
