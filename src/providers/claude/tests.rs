//! Claude adapter tests against a fake CLI that replays a trace recorded from
//! Claude Code 2.1.312 (tests/fixtures/claude_steer.jsonl): a prompt, a
//! steering message injected mid-turn, and its lifecycle events.

use super::*;
use crate::events::Event;
use crate::security::RulesApprover;

const FIXTURE: &str = include_str!("../../../tests/fixtures/claude_steer.jsonl");
const PROMPT_UUID: &str = "11111111-1111-4111-8111-111111111111";
const STEER_UUID: &str = "22222222-2222-4222-8222-222222222222";

#[derive(Default, Clone)]
struct Fake {
    paid_extra_usage: bool,
    ask_approval: Option<String>,
    hang_until_interrupt: bool,
}

type Seen = Arc<Mutex<Vec<Value>>>;

async fn run_fake(io: tokio::io::DuplexStream, f: Fake, seen: Seen) {
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
    let mut prompt_uuid = String::new();
    loop {
        let m = next!();
        if m["type"] == "control_request" {
            let req = &m["request"];
            let resp = match req["subtype"].as_str().unwrap() {
                "initialize" => json!({"models": [
                    {"value": "default", "displayName": "Default", "supportedEffortLevels": ["low", "high"]},
                    {"value": "haiku", "resolvedModel": "claude-haiku-5-5", "supportedEffortLevels": ["low", "high"]},
                    {"value": "fable", "resolvedModel": "claude-fable-5", "supportedEffortLevels": ["high"]}
                ]}),
                "get_usage" => json!({"rate_limits": {
                    "five_hour": {"utilization": 10},
                    "extra_usage": {"is_enabled": f.paid_extra_usage},
                    "spend": {"enabled": false}
                }}),
                "interrupt" => {
                    send!(
                        json!({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"], "response": {"still_queued": []}}})
                    );
                    send!(
                        json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools", "session_id": "s"})
                    );
                    send!(
                        json!({"type": "command_lifecycle", "command_uuid": prompt_uuid, "state": "cancelled"})
                    );
                    continue;
                }
                _ => json!({}),
            };
            send!(
                json!({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"], "response": resp}})
            );
            continue;
        }
        if m["type"] == "control_response" || m["type"] != "user" {
            continue;
        }
        // A new prompt: replay the recorded turn with our uuids.
        prompt_uuid = m["uuid"].as_str().unwrap().to_string();
        if let Some(cmd) = &f.ask_approval {
            send!(
                json!({"type": "control_request", "request_id": "perm-1", "request": {
                "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": cmd},
                "description": "cleanup", "tool_use_id": "toolu_x"}})
            );
            while next!()["type"] != "control_response" {}
        }
        if f.hang_until_interrupt || f.ask_approval.is_some() {
            send!(
                json!({"type": "command_lifecycle", "command_uuid": prompt_uuid, "state": "started"})
            );
            continue;
        }
        let mut steer_uuid = String::new();
        for l in FIXTURE.lines() {
            let rec: Value = serde_json::from_str(l).unwrap();
            if rec["dir"] == "IN" {
                if rec["ev"]["uuid"] == STEER_UUID {
                    let s = next!();
                    assert_eq!(s["type"], "user");
                    steer_uuid = s["uuid"].as_str().unwrap().to_string();
                }
                continue;
            }
            let line = rec["ev"]
                .to_string()
                .replace(PROMPT_UUID, &prompt_uuid)
                .replace(STEER_UUID, &steer_uuid);
            send!(line);
        }
    }
}

fn spec(allow_credits: bool) -> SessionSpec {
    SessionSpec {
        cwd: "/tmp".into(),
        model: None,
        effort: None,
        profile: Profile::WorkspaceWrite,
        role: Role::Primary,
        approver: Arc::new(RulesApprover {
            allow_commands: vec!["ls".into()],
        }),
        allow_credits,
        resume_id: None,
        stderr_log: None,
    }
}

async fn open(
    f: Fake,
    spec: SessionSpec,
) -> (Result<Arc<ClaudeSession>>, mpsc::Receiver<Event>, Seen) {
    let (client, server) = tokio::io::duplex(1 << 20);
    let seen: Seen = Arc::default();
    tokio::spawn(run_fake(server, f, seen.clone()));
    let (r, w) = tokio::io::split(client);
    let (tx, erx) = mpsc::channel(100_000);
    let ev = Emitter::new("run", "claude", "primary", tx);
    let s = ClaudeSession::open(r, w, None, "sess-1".into(), spec, ev).await;
    (s, erx, seen)
}

fn drain(rx: &mut mpsc::Receiver<Event>) -> Vec<EventKind> {
    let mut v = vec![];
    while let Ok(e) = rx.try_recv() {
        v.push(e.kind);
    }
    v
}

#[tokio::test]
async fn steer_mid_turn_is_acked_applied_and_turn_completes() {
    let (s, mut rx, _) = open(Fake::default(), spec(false)).await;
    let s = s.unwrap();
    let runner = tokio::spawn({
        let s = s.clone();
        async move { s.execute("run the commands").await }
    });
    while s.state.lock().unwrap().done.is_none() {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let receipt = s.steer("skip the sleeps").await.unwrap();
    assert!(receipt.acknowledged);
    let out = runner.await.unwrap().unwrap();
    assert_eq!(out.status, TurnStatus::Completed, "{:?}", out.error);
    assert!(out.final_text.contains("BANANA"), "{}", out.final_text);

    let evs = drain(&mut rx);
    let id = &receipt.steer_id;
    let pos = |f: &dyn Fn(&EventKind) -> bool| evs.iter().position(f);
    let delivered =
        pos(&|e| matches!(e, EventKind::SteeringDelivered { steer_id } if steer_id == id)).unwrap();
    let applied =
        pos(&|e| matches!(e, EventKind::SteeringApplied { steer_id } if steer_id == id)).unwrap();
    assert!(delivered < applied);
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventKind::ToolStarted { kind, .. } if kind == "command"))
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventKind::ToolCompleted { .. }))
    );
    assert!(evs.iter().any(|e| matches!(e, EventKind::RateLimit { .. })));
    assert!(matches!(evs.last(), Some(EventKind::AgentCompleted { .. })));
}

#[tokio::test]
async fn refuses_when_paid_extra_usage_is_enabled() {
    let f = Fake {
        paid_extra_usage: true,
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
async fn approvals_are_answered_by_jockey() {
    let f = Fake {
        ask_approval: Some("rm -rf build".into()),
        ..Default::default()
    };
    let (s, _, seen) = open(f, spec(false)).await;
    let s = s.unwrap();
    let runner = tokio::spawn({
        let s = s.clone();
        async move { s.execute("clean").await }
    });
    loop {
        let resp = seen
            .lock()
            .unwrap()
            .iter()
            .find(|m| m["type"] == "control_response")
            .cloned();
        if let Some(r) = resp {
            assert_eq!(r["response"]["request_id"], "perm-1");
            assert_eq!(r["response"]["response"]["behavior"], "deny");
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    s.interrupt().await.unwrap();
    runner.await.unwrap().unwrap();
}

#[tokio::test]
async fn interrupt_ends_turn_as_interrupted() {
    let f = Fake {
        hang_until_interrupt: true,
        ..Default::default()
    };
    let (s, mut rx, _) = open(f, spec(false)).await;
    let s = s.unwrap();
    let runner = tokio::spawn({
        let s = s.clone();
        async move { s.execute("long").await }
    });
    while s.state.lock().unwrap().done.is_none() {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    s.interrupt().await.unwrap();
    let out = runner.await.unwrap().unwrap();
    assert_eq!(out.status, TurnStatus::Interrupted);
    assert!(
        drain(&mut rx)
            .iter()
            .any(|e| matches!(e, EventKind::AgentInterrupted { .. }))
    );
}

#[tokio::test]
async fn credit_billing_models_need_opt_in() {
    let mut sp = spec(false);
    sp.model = Some("fable".into());
    let msg = Claude {
        bin: "claude".into(),
    }
    .start_session(
        sp,
        Emitter::new("r", "claude", "primary", mpsc::channel(1).0),
    )
    .await
    .err()
    .unwrap()
    .to_string();
    assert!(msg.contains("usage credits"), "{msg}");
}

#[test]
fn reviewer_profile_maps_to_read_only_tools() {
    let (mode, extra) = native_policy(Profile::ReadOnly);
    assert_eq!(mode, "dontAsk");
    assert_eq!(extra, &["--tools", "Read,Grep,Glob"]);
}

#[test]
fn stream_events_report_activity_and_turn_tokens() {
    let mut st = State::default();
    let ev = |e: Value| json!({"type": "stream_event", "event": e, "parent_tool_use_id": null});
    let k = translate(
        &mut st,
        &ev(
            json!({"type": "content_block_start", "content_block": {"type": "tool_use", "name": "Bash"}}),
        ),
    );
    assert_eq!(
        k,
        vec![EventKind::Activity {
            state: "tool".into(),
            detail: Some("Bash".into())
        }]
    );
    translate(
        &mut st,
        &ev(json!({"type": "message_delta", "usage": {"output_tokens": 40}})),
    );
    translate(&mut st, &ev(json!({"type": "message_stop"})));
    let k = translate(
        &mut st,
        &ev(json!({"type": "message_delta", "usage": {"output_tokens": 15}})),
    );
    assert_eq!(
        k,
        vec![EventKind::TokensLive {
            output_tokens: 55,
            thinking_tokens: None
        }]
    );
    let k = translate(
        &mut st,
        &json!({"type": "system", "subtype": "thinking_tokens", "estimated_tokens": 209}),
    );
    assert_eq!(
        k,
        vec![EventKind::TokensLive {
            output_tokens: 55,
            thinking_tokens: Some(209)
        }]
    );
    let k = translate(
        &mut st,
        &json!({"type": "system", "subtype": "status", "status": "compacting"}),
    );
    assert_eq!(
        k,
        vec![EventKind::Activity {
            state: "compacting".into(),
            detail: None
        }]
    );
}
