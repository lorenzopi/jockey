//! Grok adapter tests against a fake `grok agent stdio` that replays ACP
//! sessions recorded from grok 1.0.70 (tests/fixtures/grok_*.jsonl).
//! Recorded requests are matched to ours by method; requests jockey makes that
//! the recording lacks (billing, config) get synthetic answers.

use super::*;
use crate::events::Event;
use crate::security::RulesApprover;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const STEER: &str = include_str!("../../../tests/fixtures/grok_steer.jsonl");
const PERMISSION: &str = include_str!("../../../tests/fixtures/grok_permission.jsonl");

#[derive(Clone, Default)]
struct Fake {
    paid: bool,
}

type Seen = Arc<Mutex<Vec<Value>>>;

async fn run_fake(io: tokio::io::DuplexStream, script: &'static str, f: Fake, seen: Seen) {
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
    // Reads client messages until one satisfies `want`, answering stray requests.
    macro_rules! wait_for {
        ($want:expr) => {{
            loop {
                let Ok(Some(l)) = lines.next_line().await else { return };
                let m: Value = serde_json::from_str(&l).unwrap();
                seen.lock().unwrap().push(m.clone());
                if $want(&m) {
                    break m;
                }
                if m.get("method").is_some() && m.get("id").is_some() {
                    let result = match m["method"].as_str().unwrap() {
                        "_x.ai/billing" => json!({"config": {
                            "creditUsagePercent": 5.0,
                            "onDemandCap": {"val": if f.paid { 50 } else { 0 }},
                            "prepaidBalance": {"val": 0}}}),
                        _ => json!({}),
                    };
                    send!(json!({"jsonrpc": "2.0", "id": m["id"], "result": result}));
                }
            }
        }};
    }
    let mut ids: HashMap<i64, Value> = HashMap::new();
    for l in script.lines() {
        let rec: Value = serde_json::from_str(l).unwrap();
        let m = &rec["msg"];
        if rec["dir"] == "->" {
            if let (Some(method), Some(rid)) = (m["method"].as_str(), m["id"].as_i64()) {
                let got = wait_for!(|c: &Value| c["method"] == method);
                ids.insert(rid, got["id"].clone());
            } else if m.get("method").is_none() {
                // Our reply to a recorded server request (a permission answer).
                wait_for!(|c: &Value| c.get("method").is_none() && c["id"] == m["id"]);
            }
            continue;
        }
        let mut out = m.clone();
        if out.get("method").is_none()
            && let Some(rid) = m["id"].as_i64()
        {
            match ids.get(&rid) {
                Some(cid) => out["id"] = cid.clone(),
                None => continue,
            }
        }
        send!(out);
    }
    // Keep answering until the client hangs up.
    wait_for!(|_: &Value| false);
}

fn spec() -> SessionSpec {
    // The recorded traces use this cwd; it must exist for the workspace check.
    std::fs::create_dir_all("/tmp/work-grok").unwrap();
    SessionSpec {
        cwd: "/tmp/work-grok".into(),
        model: None,
        effort: None,
        profile: Profile::WorkspaceWrite,
        role: Role::Primary,
        approver: Arc::new(RulesApprover {
            allow_commands: vec![],
        }),
        allow_credits: false,
        resume_id: None,
        stderr_log: None,
        live_profile: false,
    }
}

async fn open(
    script: &'static str,
    f: Fake,
    spec: SessionSpec,
) -> (Result<Arc<GrokSession>>, mpsc::Receiver<Event>, Seen) {
    let (client, server) = tokio::io::duplex(1 << 20);
    let seen: Seen = Arc::default();
    tokio::spawn(run_fake(server, script, f, seen.clone()));
    let (r, w) = tokio::io::split(client);
    let (rpc, rx) = RpcClient::start(r, w);
    let (tx, erx) = mpsc::channel(100_000);
    let ev = Emitter::new("run", "grok", "primary", tx);
    (GrokSession::open(rpc, rx, None, spec, ev).await, erx, seen)
}

fn drain(rx: &mut mpsc::Receiver<Event>) -> Vec<EventKind> {
    let mut v = vec![];
    while let Ok(e) = rx.try_recv() {
        v.push(e.kind);
    }
    v
}

#[tokio::test]
async fn interject_is_queued_applied_at_tool_boundary_and_turn_completes() {
    let (s, mut rx, seen) = open(STEER, Fake::default(), spec()).await;
    let s = s.unwrap();
    let runner = tokio::spawn({
        let s = s.clone();
        async move { s.execute("read calc.py three times").await }
    });
    while !s.state.lock().unwrap().active {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let r = s.steer("end with BANANA").await.unwrap();
    assert!(r.acknowledged);
    let out = runner.await.unwrap().unwrap();
    assert_eq!(out.status, TurnStatus::Completed, "{:?}", out.error);
    assert!(out.final_text.contains("BANANA"), "{}", out.final_text);

    // yoloMode must be off so the user's always-approve config can't leak in.
    let new = seen
        .lock()
        .unwrap()
        .iter()
        .find(|m| m["method"] == "session/new")
        .cloned()
        .unwrap();
    assert_eq!(new["params"]["_meta"]["yoloMode"], json!(false));

    let evs = drain(&mut rx);
    let id = &r.steer_id;
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventKind::SteeringDelivered { steer_id } if steer_id == id))
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventKind::SteeringApplied { steer_id } if steer_id == id))
    );
    assert!(
        evs.iter()
            .filter(|e| matches!(e, EventKind::ToolCompleted { ok: true, .. }))
            .count()
            >= 3
    );
    assert!(
        evs.iter()
            .any(|e| matches!(e, EventKind::UsageUpdated { .. }))
    );
    // Completion updates omit the input; titles must still come through.
    assert!(
        evs.iter().all(
            |e| !matches!(e, EventKind::ToolCompleted { title, .. } if title.trim().is_empty())
        )
    );
    assert!(matches!(evs.last(), Some(EventKind::AgentCompleted { .. })));
}

#[tokio::test]
async fn edits_inside_workspace_are_allowed_by_profile() {
    // The recording rejected this write; replaying it, jockey answers by profile
    // (workspace-write allows edits under cwd) and the fake ends the turn as recorded.
    let (s, _rx, seen) = open(PERMISSION, Fake::default(), spec()).await;
    let s = s.unwrap();
    let out = s.execute("create acp_test2.txt").await.unwrap();
    assert_eq!(out.status, TurnStatus::Failed); // recorded stopReason: cancelled
    let answer = seen
        .lock()
        .unwrap()
        .iter()
        .find(|m| m.get("method").is_none() && m["result"]["outcome"].is_object())
        .cloned()
        .unwrap();
    assert_eq!(answer["result"]["outcome"]["optionId"], "allow-once");
}

#[tokio::test]
async fn read_only_profile_rejects_permission_requests() {
    let mut sp = spec();
    sp.role = Role::Reviewer;
    let (s, _rx, seen) = open(PERMISSION, Fake::default(), sp).await;
    s.unwrap().execute("create acp_test2.txt").await.unwrap();
    let answer = seen
        .lock()
        .unwrap()
        .iter()
        .find(|m| m.get("method").is_none() && m["result"]["outcome"].is_object())
        .cloned()
        .unwrap();
    assert_eq!(answer["result"]["outcome"]["optionId"], "reject-once");
}

#[tokio::test]
async fn refuses_when_paid_credit_is_enabled() {
    let (s, _, _) = open(STEER, Fake { paid: true }, spec()).await;
    let err = s.err().expect("must refuse");
    assert!(matches!(
        err.downcast_ref::<ProviderError>(),
        Some(ProviderError::QuotaExhausted(_))
    ));
}

#[test]
fn models_come_from_initialize() {
    let init: Value = STEER
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|r| r["msg"]["result"]["_meta"]["modelState"].is_object())
        .unwrap()["msg"]["result"]
        .clone();
    let (models, windows) = parse_models(&init);
    assert!(models.iter().any(|m| m.is_default));
    assert!(models.iter().all(|m| !m.efforts.is_empty()));
    assert!(!windows.is_empty());
}

#[cfg(unix)]
#[test]
fn workspace_check_resolves_symlinks() {
    let ws = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = ws.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("out")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("nope"), root.join("dangling")).unwrap();
    assert!(resolves_inside(&root.join("src/new/file.rs"), root));
    assert!(!resolves_inside(&root.join("out/authorized_keys"), root));
    assert!(!resolves_inside(&root.join("dangling"), root));
}
