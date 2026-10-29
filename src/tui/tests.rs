//! Drives the TUI state machine with the mock provider; no terminal needed.

use super::app::*;
use super::ui;
use crate::config::Config;
use crate::providers::AgentSpec;
use crate::security::Profile;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::testutil::env;

fn app() -> (App, mpsc::Receiver<Msg>) {
    let home = env();
    let (tx, rx) = mpsc::channel(4096);
    let app = App::new(
        Config::default(),
        home.path().to_path_buf(),
        "mock".parse().unwrap(),
        Profile::WorkspaceWrite,
        false,
        tx,
    );
    (app, rx)
}

fn key(code: KeyCode, m: KeyModifiers) -> Msg {
    Msg::Key(KeyEvent::new(code, m))
}

async fn type_line(app: &mut App, s: &str, m: KeyModifiers) {
    for c in s.chars() {
        app.handle(key(KeyCode::Char(c), KeyModifiers::NONE)).await;
    }
    app.handle(key(KeyCode::Enter, m)).await;
}

async fn pump_until(app: &mut App, rx: &mut mpsc::Receiver<Msg>, cond: impl Fn(&App) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !cond(app) {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(m)) => app.handle(m).await,
            _ => panic!("condition not reached; transcript: {:#?}", app.transcript),
        }
    }
}

fn screen(app: &App) -> String {
    screen_sized(app, 200, 50)
}

fn screen_sized(app: &App, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| ui::draw(f, app)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut s = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            s.push_str(buf[(x, y)].symbol());
        }
        s.push('\n');
    }
    s
}

#[tokio::test]
async fn prompt_streams_steers_and_saves_run() {
    let (mut app, mut rx) = app();
    type_line(&mut app, "implement pricing", KeyModifiers::NONE).await;
    assert!(app.busy());
    pump_until(&mut app, &mut rx, |a| {
        a.session.is_some() && a.transcript.iter().any(|e| matches!(e, Entry::Tool { .. }))
    })
    .await;

    // Enter while busy steers the running agent.
    type_line(&mut app, "use continuous compounding", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| !a.busy()).await;

    let steer = app
        .transcript
        .iter()
        .find_map(|e| match e {
            Entry::Steer { state, .. } => Some(state.clone()),
            _ => None,
        })
        .expect("steer entry");
    assert_eq!(steer, SteerState::Applied);
    assert!(
        app.transcript
            .iter()
            .any(|e| matches!(e, Entry::User(t) if t == "implement pricing"))
    );
    assert!(
        app.last_answer
            .as_deref()
            .unwrap()
            .contains("Applied 1 correction")
    );

    let s = screen(&app);
    assert!(s.contains("› implement pricing"), "{s}");
    assert!(s.contains("· mock-fast ·"), "{s}");
    assert!(s.contains("applied"), "{s}");

    let dir = app.run.as_ref().unwrap().dir.clone();
    app.finish_run().await;
    let meta = crate::sessions::read_meta(&dir).unwrap();
    assert_eq!(meta.status, "completed");
    assert_eq!(meta.agents[0].provider, "mock");
    let evs = crate::sessions::read_events(&dir).unwrap();
    assert!(
        evs.iter()
            .any(|e| matches!(e.kind, crate::events::EventKind::UserPrompt { .. }))
    );
}

#[tokio::test]
async fn approval_popup_answers_the_agent() {
    let (mut app, mut rx) = app();
    type_line(&mut app, "please approve the cleanup", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| {
        matches!(a.popup, Some(Popup::Approval(..)))
    })
    .await;
    assert!(screen(&app).contains("rm -rf build"));
    app.handle(key(KeyCode::Char('n'), KeyModifiers::NONE))
        .await;
    pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    assert!(app.transcript.iter().any(|e| matches!(
        e,
        Entry::Approval { decision: Some(d), .. } if d == "decline (user)"
    )));
    app.finish_run().await;
}

#[tokio::test]
async fn alt_enter_queues_a_follow_up_turn() {
    let (mut app, mut rx) = app();
    type_line(&mut app, "first", KeyModifiers::NONE).await;
    type_line(&mut app, "second", KeyModifiers::ALT).await;
    assert_eq!(app.queue.len(), 1);
    pump_until(&mut app, &mut rx, |a| {
        a.queue.is_empty()
            && !a.busy()
            && a.transcript
                .iter()
                .filter(|e| matches!(e, Entry::User(_)))
                .count()
                == 2
    })
    .await;
    app.finish_run().await;
}

#[tokio::test]
async fn escape_interrupts() {
    let (mut app, mut rx) = app();
    type_line(&mut app, "long task", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| a.session.is_some()).await;
    app.handle(key(KeyCode::Esc, KeyModifiers::NONE)).await;
    pump_until(&mut app, &mut rx, |a| {
        !a.busy()
            && a.transcript
                .iter()
                .any(|e| matches!(e, Entry::Notice(_, t) if t.starts_with("interrupted")))
    })
    .await;
    assert!(app.last_answer.is_none());
    app.finish_run().await;
}

#[tokio::test]
async fn slash_menu_filters_and_completes() {
    let (mut app, _rx) = app();
    for c in "/mo".chars() {
        app.handle(key(KeyCode::Char(c), KeyModifiers::NONE)).await;
    }
    let names: Vec<_> = app.menu().into_iter().map(|(c, _)| c).collect();
    assert_eq!(names, vec!["/model", "/mode", "/mock:review"]);
    // Exact matches come first so Enter runs what was typed.
    app.editor.set("/mode");
    assert_eq!(app.menu()[0].0, "/mode");
    app.editor.set("/mo");
    app.handle(key(KeyCode::Tab, KeyModifiers::NONE)).await;
    assert_eq!(app.editor.text, "/model");
    assert!(app.menu().iter().any(|(c, _)| c == "/model"));
    app.handle(key(KeyCode::Char('/'), KeyModifiers::NONE))
        .await; // "/model/" no longer matches
    assert!(app.menu().is_empty());
}

#[tokio::test]
async fn shift_tab_cycles_effort_from_model_list() {
    let (mut app, mut rx) = app();
    app.load_models();
    pump_until(&mut app, &mut rx, |a| !a.models.is_empty()).await;
    app.handle(key(KeyCode::BackTab, KeyModifiers::SHIFT)).await;
    assert_eq!(app.agent.effort.as_deref(), Some("low"));
    app.handle(key(KeyCode::BackTab, KeyModifiers::SHIFT)).await;
    assert_eq!(app.agent.effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn missed_steer_becomes_the_next_prompt() {
    use crate::events::{Event, EventKind};
    let (mut app, mut rx) = app();
    let ev = |kind| {
        Msg::Bus(Event {
            run_id: "r".into(),
            seq: 0,
            ts_ms: 0,
            provider: "mock".into(),
            agent: "primary".into(),
            kind,
        })
    };
    app.handle(ev(EventKind::SteeringSent {
        steer_id: "s1".into(),
        text: "also add docs".into(),
    }))
    .await;
    app.handle(ev(EventKind::SteeringRejected {
        steer_id: "s1".into(),
        reason: crate::providers::STEER_MISSED_TURN.into(),
    }))
    .await;
    assert!(
        app.busy(),
        "idle app should send the missed correction as a new turn"
    );
    pump_until(&mut app, &mut rx, |a| {
        !a.busy()
            && a.transcript
                .iter()
                .any(|e| matches!(e, Entry::User(t) if t == "also add docs"))
    })
    .await;
    app.finish_run().await;
}

fn picker(app: &App) -> &Picker {
    match &app.popup {
        Some(Popup::Picker(p)) => p,
        _ => panic!("no picker open"),
    }
}

#[tokio::test]
async fn ctrl_l_walks_lab_model_thinking() {
    let (mut app, mut rx) = app();
    app.handle(key(KeyCode::Char('l'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(picker(&app).kind, PickKind::Lab);
    let labs: Vec<_> = picker(&app).items.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(labs, vec!["codex", "claude", "grok", "mock"]);
    assert_eq!(picker(&app).selected, 3, "starts on the current lab");

    app.handle(key(KeyCode::Enter, KeyModifiers::NONE)).await;
    assert_eq!(picker(&app).kind, PickKind::Model);
    pump_until(
        &mut app,
        &mut rx,
        |a| matches!(&a.popup, Some(Popup::Picker(p)) if !p.items.is_empty()),
    )
    .await;
    assert!(picker(&app).title.starts_with("Mock › model"));

    // mock-fast, mock-weak, mock-slow, mock-deep
    for _ in 0..3 {
        app.handle(key(KeyCode::Down, KeyModifiers::NONE)).await;
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE)).await;
    assert_eq!(picker(&app).kind, PickKind::Effort);
    assert_eq!(picker(&app).title, "Mock › mock-deep › thinking");

    // Backspace on an empty filter goes back a level, then forward again.
    app.handle(key(KeyCode::Backspace, KeyModifiers::NONE))
        .await;
    assert_eq!(picker(&app).kind, PickKind::Model);
    assert_eq!(picker(&app).selected, 0);
    for _ in 0..3 {
        app.handle(key(KeyCode::Down, KeyModifiers::NONE)).await;
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE)).await;

    for c in "med".chars() {
        app.handle(key(KeyCode::Char(c), KeyModifiers::NONE)).await;
    }
    app.handle(key(KeyCode::Enter, KeyModifiers::NONE)).await;
    assert!(app.popup.is_none());
    assert_eq!(app.agent.model.as_deref(), Some("mock-deep"));
    assert_eq!(app.agent.effort.as_deref(), Some("medium"));
    assert!(screen(&app).contains("· mock-deep@medium ·"));
}

#[tokio::test]
async fn footer_shows_versioned_model() {
    use crate::providers::ModelInfo;
    let (mut app, _rx) = app();
    let m = |id: &str, resolved: Option<&str>, default: bool| ModelInfo {
        id: id.into(),
        is_default: default,
        efforts: vec![],
        display: None,
        resolved: resolved.map(String::from),
        note: None,
        bills_credits: false,
    };
    app.agent = "claude".parse().unwrap();
    app.models = vec![
        m("default", Some("claude-opus-5-5"), true),
        m("haiku", Some("claude-haiku-5-5"), false),
    ];
    assert_eq!(app.agent_label(), "claude-opus-5-5");
    app.agent = "claude:haiku@low".parse().unwrap();
    assert_eq!(app.agent_label(), "claude-haiku-5-5@low");
    app.agent = "codex@high".parse().unwrap();
    app.models = vec![m("gpt-6.1-sol", None, true)];
    assert_eq!(app.agent_label(), "codex:gpt-6.1-sol@high");
    // What the running session reports wins over the list.
    app.model_label = Some("gpt-6.1-sol-2026-10".into());
    assert!(screen(&app).contains("codex:gpt-6.1-sol-2026-10@high"));
}

async fn press(app: &mut App, code: KeyCode) {
    app.handle(key(code, KeyModifiers::NONE)).await;
}

async fn type_text(app: &mut App, t: &str) {
    for c in t.chars() {
        press(app, KeyCode::Char(c)).await;
    }
}

/// From a role's lab picker: filter to the mock lab, pick `nth` model, then the first thinking level.
async fn pick_mock(app: &mut App, rx: &mut mpsc::Receiver<Msg>, nth: usize) {
    assert_eq!(picker(app).kind, PickKind::Lab);
    type_text(app, "built-in").await;
    press(app, KeyCode::Enter).await;
    pump_until(app, rx, |a| matches!(&a.popup, Some(Popup::Picker(p)) if p.kind == PickKind::Model && !p.items.is_empty())).await;
    for _ in 0..nth {
        press(app, KeyCode::Down).await;
    }
    press(app, KeyCode::Enter).await;
    assert_eq!(picker(app).kind, PickKind::Effort);
    press(app, KeyCode::Enter).await;
}

#[tokio::test]
async fn switching_to_cascade_opens_role_setup_and_runs() {
    let (mut app, mut rx) = app();
    app.agent = "mock:mock-weak".parse().unwrap();
    app.command("/mode cascade").await;
    assert_eq!(app.mode, "cascade");
    let roles: Vec<_> = picker(&app).items.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(roles, vec!["primary", "judge", "fallback", "done"]);
    assert_eq!(
        picker(&app).selected,
        1,
        "lands on the first role still to choose"
    );

    press(&mut app, KeyCode::Enter).await; // judge
    assert_eq!(picker(&app).title, "judge › lab");
    pick_mock(&mut app, &mut rx, 0).await; // mock-fast@low
    assert_eq!(picker(&app).kind, PickKind::Role, "back to the setup menu");
    assert_eq!(picker(&app).selected, 3, "fallback is optional: on Done");
    assert!(picker(&app).items[2].1.contains("off"));

    press(&mut app, KeyCode::Up).await;
    press(&mut app, KeyCode::Enter).await; // enable fallback
    pick_mock(&mut app, &mut rx, 3).await; // mock-deep@low
    assert_eq!(picker(&app).items[3].0, "done");
    assert_eq!(picker(&app).selected, 3);
    press(&mut app, KeyCode::Enter).await;
    assert!(app.popup.is_none());
    assert_eq!(
        app.judge.as_ref().unwrap().model.as_deref(),
        Some("mock-fast")
    );
    assert_eq!(
        app.fallback.as_ref().unwrap().model.as_deref(),
        Some("mock-deep")
    );
    assert!(screen(&app).contains("cascade (judge mock-fast@low, fallback mock-deep@low)"));

    type_line(&mut app, "fix the bug", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| {
        !a.busy()
            && a.transcript
                .iter()
                .filter(|e| matches!(e, Entry::Verdict { .. }))
                .count()
                == 2
    })
    .await;
    let verdicts: Vec<_> = app
        .transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Verdict {
                score, accepted, ..
            } => Some((*score, *accepted)),
            _ => None,
        })
        .collect();
    assert_eq!(verdicts, vec![(40, false), (92, true)]);
    assert!(screen(&app).contains("escalating primary"));
    app.finish_run().await;
}

#[tokio::test]
async fn critique_defaults_to_primary_model_and_critics_are_selectable() {
    let (mut app, mut rx) = app();
    app.agent = "mock:mock-weak".parse().unwrap();
    press(&mut app, KeyCode::Char('/')).await;
    app.editor.take();
    app.command("/mode").await;
    assert_eq!(picker(&app).kind, PickKind::Mode);
    type_text(&mut app, "crit").await;
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(app.mode, "critique");
    let labels: Vec<_> = picker(&app).items.iter().map(|(_, l)| l.clone()).collect();
    assert!(labels[1].contains("3× same as primary"), "{labels:?}");

    // Change critics to another model, then back to "same as primary".
    press(&mut app, KeyCode::Up).await; // Done → Count
    press(&mut app, KeyCode::Up).await; // → Critics
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(picker(&app).items[0].0, "same");
    press(&mut app, KeyCode::Down).await; // past "same", then filter
    pick_mock(&mut app, &mut rx, 3).await;
    assert_eq!(
        app.critic.as_ref().unwrap().model.as_deref(),
        Some("mock-deep")
    );
    press(&mut app, KeyCode::Up).await;
    press(&mut app, KeyCode::Up).await;
    press(&mut app, KeyCode::Enter).await;
    for _ in 0..5 {
        press(&mut app, KeyCode::Up).await; // to "same", the first row
    }
    press(&mut app, KeyCode::Enter).await;
    assert!(app.critic.is_none());
    press(&mut app, KeyCode::Enter).await; // Done
    assert!(app.popup.is_none());

    type_line(&mut app, "price bonds", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| {
        !a.busy()
            && a.transcript
                .iter()
                .any(|e| matches!(e, Entry::Findings { .. }))
    })
    .await;
    let critics = app
        .transcript
        .iter()
        .filter(|e| matches!(e, Entry::Critic { error: None, .. }))
        .count();
    assert_eq!(critics, 3);
    let s = screen(&app);
    assert!(
        s.contains("2 finding(s) sent back for revision (4 duplicates merged)"),
        "{s}"
    );
    assert!(s.contains("critique (3× same as primary)"), "{s}");
    app.finish_run().await;
}

#[tokio::test]
async fn bare_slash_commands_open_their_menus() {
    let (mut app, _rx) = app();
    for (cmd, kind, title) in [
        ("/mode", PickKind::Mode, "mode"),
        ("/judge", PickKind::Lab, "judge › lab"),
        ("/fallback", PickKind::Lab, "fallback › lab"),
        ("/critics", PickKind::Lab, "critics › lab"),
        ("/model", PickKind::Lab, "lab"),
        ("/profile", PickKind::Profile, "profile (new sessions)"),
    ] {
        // Through the slash menu, as a user would: type, then Enter on the match.
        type_text(&mut app, cmd).await;
        press(&mut app, KeyCode::Enter).await;
        let p = picker(&app);
        assert_eq!((p.kind, p.title.as_str()), (kind, title), "{cmd}");
        press(&mut app, KeyCode::Esc).await;
        assert!(app.popup.is_none());
    }
}

fn bus(kind: crate::events::EventKind) -> Msg {
    Msg::Bus(crate::events::Event {
        run_id: "r".into(),
        seq: 0,
        ts_ms: 0,
        provider: "mock".into(),
        agent: "primary".into(),
        kind,
    })
}

#[tokio::test]
async fn status_line_shows_cli_activity_elapsed_and_tokens() {
    use crate::events::EventKind;
    let (mut app, _rx) = app();
    app.busy_since = Some(std::time::Instant::now() - Duration::from_secs(606));
    app.handle(bus(EventKind::Activity {
        state: "tool".into(),
        detail: Some("cargo test".into()),
    }))
    .await;
    app.handle(bus(EventKind::TokensLive {
        output_tokens: 56_000,
        thinking_tokens: Some(1_200),
    }))
    .await;
    // Text streamed after the last provider count is estimated on top of it.
    app.handle(bus(EventKind::MessageDelta {
        item_id: "m".into(),
        text: "x".repeat(1_600),
    }))
    .await;
    let s = screen(&app);
    assert!(
        s.contains("mock · Writing… (10m 6s · ↓ 56.4k tokens · thought 1.2k)"),
        "{s}"
    );
    app.handle(bus(EventKind::Activity {
        state: "compacting".into(),
        detail: None,
    }))
    .await;
    assert!(screen(&app).contains("Compacting context…"));
    app.busy_since = None;
}

#[tokio::test]
async fn supervisor_pane_scrolls_on_its_own() {
    let (mut app, _rx) = app();
    app.mode = "critique".into();
    for i in 0..60 {
        app.transcript
            .push(Entry::Feed(Level::Info, format!("review {i}")));
    }
    let s = screen(&app);
    assert!(
        s.contains("review 59") && !s.contains("review 0 "),
        "follows the bottom"
    );
    // Alt+PgUp / Alt+PgDn scroll the supervisor pane, not the primary pane.
    app.handle(key(KeyCode::PageUp, KeyModifiers::ALT)).await;
    assert_eq!((app.scroll_sup, app.scroll), (10, 0));
    assert!(screen(&app).contains("supervisor · ↑ 10 lines"));
    app.handle(key(KeyCode::PageDown, KeyModifiers::ALT)).await;
    app.handle(key(KeyCode::PageDown, KeyModifiers::ALT)).await;
    assert_eq!(app.scroll_sup, 0);
}

#[tokio::test]
async fn autosteer_setup_has_policy_and_manual_suggestions_can_be_sent() {
    let (mut app, mut rx) = app();
    app.agent = "mock:mock-slow".parse().unwrap();
    app.cfg.autosteer.debounce_ms = 0;
    app.cfg.autosteer.min_review_interval_secs = 0;
    app.command("/critics mock").await; // fast critics
    app.command("/mode autosteer").await;
    let roles: Vec<_> = picker(&app).items.iter().map(|(v, _)| v.as_str()).collect();
    assert_eq!(roles, vec!["primary", "critics", "count", "policy", "done"]);
    press(&mut app, KeyCode::Up).await; // Policy
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(picker(&app).kind, PickKind::Policy);
    type_text(&mut app, "manual").await; // filters to "suggest"
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(app.policy, crate::steering::Policy::Suggest);
    assert_eq!(picker(&app).kind, PickKind::Role, "back to setup");
    press(&mut app, KeyCode::Esc).await;

    type_line(&mut app, "price the bond", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| a.open_suggestion_count() > 0).await;
    let s = screen(&app);
    assert!(
        s.contains("supervisor ·") && s.contains("primary ·"),
        "split pane: {s}"
    );
    assert!(s.contains("steer suggestion"), "{s}");

    // The user picks a critic's suggestion; it goes to the running agent.
    app.handle(key(KeyCode::Char('s'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(picker(&app).kind, PickKind::Suggestion);
    let was_busy = app.busy();
    press(&mut app, KeyCode::Enter).await;
    pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    if was_busy {
        assert!(
            app.transcript
                .iter()
                .any(|e| matches!(e, Entry::Steer { .. })),
            "sent as a live steer"
        );
    } else {
        assert!(
            app.editor.text.contains("independent reviewer"),
            "staged as the next prompt"
        );
    }
    assert!(
        app.transcript
            .iter()
            .any(|e| matches!(e, Entry::Decision { action, .. } if action == "suggest"))
    );
    app.finish_run().await;
}

#[tokio::test]
async fn user_can_steer_an_autosteer_run_by_typing() {
    let (mut app, mut rx) = app();
    app.agent = "mock:mock-slow".parse().unwrap();
    app.command("/critics mock").await;
    app.command("/policy observe").await;
    app.command("/mode autosteer").await;
    press(&mut app, KeyCode::Esc).await;
    type_line(&mut app, "price the bond", KeyModifiers::NONE).await;
    // Wait until the workflow has started the primary, then type a correction.
    pump_until(&mut app, &mut rx, |a| {
        a.run
            .as_ref()
            .and_then(|r| r.ctx.session("primary"))
            .is_some()
    })
    .await;
    type_line(&mut app, "use continuous compounding", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    let steer = app.transcript.iter().find_map(|e| match e {
        Entry::Steer { text, state, .. } if text == "use continuous compounding" => {
            Some(state.clone())
        }
        _ => None,
    });
    assert!(
        matches!(
            steer,
            Some(SteerState::Applied) | Some(SteerState::Delivered)
        ),
        "{steer:?}"
    );
    app.finish_run().await;
}

#[tokio::test]
async fn split_pane_flips_and_zooms() {
    let (mut app, _rx) = app();
    app.mode = "autosteer".into();
    app.transcript.push(Entry::Feed(
        Level::Info,
        "review 1 (src/lib.rs changed): 2 critic(s)".into(),
    ));
    app.transcript.push(Entry::User("price the bond".into()));
    let s = screen(&app);
    assert!(s.contains("review 1") && s.contains("› price the bond"));
    app.handle(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
        .await;
    let s = screen(&app);
    assert!(
        s.contains("› price the bond") && !s.contains("review 1"),
        "zoomed on primary"
    );
    app.handle(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
        .await;
    let s = screen(&app);
    assert!(
        !s.contains("› price the bond") && s.contains("review 1"),
        "zoomed on supervisor"
    );
    app.handle(key(KeyCode::Char('z'), KeyModifiers::CONTROL))
        .await;
    app.handle(key(KeyCode::Char('\\'), KeyModifiers::CONTROL))
        .await;
    assert!(app.split_flip);
}

/// Session ids of every primary AgentStarted event in the run so far.
fn primary_sessions(app: &App) -> Vec<String> {
    let dir = app.run.as_ref().unwrap().dir.clone();
    crate::sessions::read_events(&dir)
        .unwrap()
        .into_iter()
        .filter(|e| e.agent == "primary")
        .filter_map(|e| match e.kind {
            crate::events::EventKind::AgentStarted { session_id, .. } => session_id,
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn one_conversation_across_prompts_modes_and_esc() {
    let (mut app, mut rx) = app();
    app.command("/critics mock").await;
    type_line(&mut app, "first question", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    let first = app.session.as_ref().unwrap().session_id();

    // Switching mode keeps the run and its primary conversation.
    app.command("/mode autosteer").await;
    press(&mut app, KeyCode::Esc).await;
    for prompt in ["second question", "third question"] {
        type_line(&mut app, prompt, KeyModifiers::NONE).await;
        pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    }
    assert_eq!(app.session.as_ref().unwrap().session_id(), first);

    // Esc stops the workflow but not the conversation.
    type_line(&mut app, "fourth question", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| {
        a.run
            .as_ref()
            .and_then(|r| r.ctx.session("primary"))
            .is_some()
    })
    .await;
    press(&mut app, KeyCode::Esc).await;
    pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    type_line(&mut app, "fifth question", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    assert!(
        !app.transcript
            .iter()
            .any(|e| matches!(e, Entry::Notice(Level::Error, t) if t.contains("already running"))),
        "{:#?}",
        app.transcript
    );

    let started = primary_sessions(&app);
    assert_eq!(started.len(), 1, "the primary started once: {started:?}");
    // Reviews are numbered across the conversation.
    let feeds: Vec<_> = app
        .transcript
        .iter()
        .filter_map(|e| match e {
            Entry::Feed(_, t) if t.starts_with("review ") => {
                Some(t.split(' ').nth(1).unwrap().to_string())
            }
            _ => None,
        })
        .collect();
    let mut sorted = feeds.clone();
    sorted.dedup();
    assert_eq!(feeds, sorted, "no repeated review numbers: {feeds:?}");
    app.finish_run().await;
}

#[tokio::test]
async fn autosteer_defaults_to_one_critic_and_count_is_selectable() {
    let (mut app, _rx) = app();
    app.command("/mode autosteer").await;
    assert!(picker(&app).items[1].1.contains("1× same as primary"));
    press(&mut app, KeyCode::Up).await; // Done → Policy
    press(&mut app, KeyCode::Up).await; // → Count
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(picker(&app).kind, PickKind::Count);
    press(&mut app, KeyCode::Down).await;
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(app.critic_count, Some(2));
    assert!(picker(&app).items[1].1.contains("2× same as primary"));
    assert_eq!(app.critic_flags(1).len(), 2);
}

#[tokio::test]
async fn without_a_saved_default_the_tui_asks_for_a_model_first() {
    let (mut app, mut rx) = app();
    // What `tui::run` does when there's no --agent and no defaults.agent.
    app.agent = AgentSpec {
        provider: String::new(),
        model: None,
        effort: None,
    };
    app.needs_agent = true;
    assert!(
        screen(&app).contains("J O C K E Y"),
        "home screen, no picker yet"
    );
    press(&mut app, KeyCode::Enter).await;
    assert_eq!(picker(&app).kind, PickKind::Lab);
    assert!(screen(&app).contains("no model selected (Ctrl+L)"));

    // Typing before choosing keeps the prompt and asks again.
    press(&mut app, KeyCode::Esc).await;
    type_line(&mut app, "hello there", KeyModifiers::NONE).await;
    assert!(
        !app.busy() && app.run.is_none(),
        "nothing runs without a model"
    );
    assert_eq!(app.editor.text, "hello there");
    assert_eq!(picker(&app).kind, PickKind::Lab);

    pick_mock(&mut app, &mut rx, 0).await; // mock-fast@low
    assert!(!app.needs_agent);
    assert_eq!(app.agent.provider, "mock");
    assert_eq!(
        app.saved_models()[0].2.as_deref(),
        Some("mock:mock-fast@low")
    );
    assert!(
        app.transcript
            .iter()
            .any(|e| matches!(e, Entry::Notice(_, t) if t.contains("saved as your default model")))
    );
    press(&mut app, KeyCode::Enter).await; // send the kept prompt
    pump_until(&mut app, &mut rx, |a| !a.busy() && a.last_answer.is_some()).await;
    app.finish_run().await;
}

#[tokio::test]
async fn supervisor_marks_each_prompt_and_names_the_critic_model() {
    let (mut app, mut rx) = app();
    app.command("/critics mock:mock-deep@high").await;
    app.command("/mode autosteer").await;
    press(&mut app, KeyCode::Esc).await;
    type_line(&mut app, "build it", KeyModifiers::NONE).await;
    // Bus events (the review line) can land just after the turn-done message.
    pump_until(&mut app, &mut rx, |a| {
        !a.busy()
            && a.transcript
                .iter()
                .any(|e| matches!(e, Entry::Feed(_, t) if t.starts_with("review 1 (")))
    })
    .await;
    assert!(
        app.transcript
            .iter()
            .any(|e| matches!(e, Entry::Divider(t) if t == "build it"))
    );
    assert!(
        app.transcript.iter().any(|e| matches!(e,
            Entry::Feed(_, t) if t.starts_with("review 1 (") && t.contains("1× mock-deep@high"))),
        "{:#?}",
        app.transcript
    );
    assert!(screen(&app).contains("── build it ──"));
}

#[tokio::test]
async fn a_model_that_bills_credits_is_refused_when_picked() {
    use crate::providers::ModelInfo;
    let (mut app, mut rx) = app();
    let paid = ModelInfo {
        id: "mock-paid".into(),
        is_default: false,
        efforts: vec!["low".into()],
        display: None,
        resolved: None,
        note: None,
        bills_credits: true,
    };
    app.handle(Msg::Models("mock".into(), vec![paid])).await;
    app.command("/critics").await;
    pick_mock(&mut app, &mut rx, 0).await;
    assert!(app.critic.is_none());
    assert!(app.transcript.iter().any(|e| matches!(e,
        Entry::Notice(Level::Error, t) if t.contains("can bill usage credits"))));
}

#[tokio::test]
async fn racetrack_gallops_while_busy_and_shows_the_finish() {
    use crate::events::EventKind;
    let (mut app, mut rx) = app();
    let s = screen(&app);
    assert!(
        s.contains("J O C K E Y") && s.contains("@@@@"),
        "emblem and title on the splash: {s}"
    );
    assert!(
        screen(&app).contains("one saddle, many coding agents"),
        "splash on an empty screen"
    );
    type_line(&mut app, "price the bond", KeyModifiers::NONE).await;
    let s = screen(&app);
    assert!(
        s.contains("🏇") && s.contains("🏁"),
        "rider on the track: {s}"
    );

    // Critics get their own lane while they review.
    app.handle(bus(EventKind::ReviewTriggered {
        round: 1,
        reason: "x".into(),
        critics: vec!["critic-1".into()],
        models: vec![],
    }))
    .await;
    assert!(screen(&app).contains("1 critic reviewing"));
    app.handle(bus(EventKind::FindingsMerged {
        round: 1,
        findings: vec![],
        duplicates: 0,
    }))
    .await;
    assert!(!screen(&app).contains("critic reviewing"));

    pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    let s = screen(&app);
    assert!(
        s.contains("finished ·"),
        "finish line after a completed turn: {s}"
    );
    // Tool cards carry durations; file edits carry +/− counts.
    assert!(s.contains("+2 −1"), "{s}");
    assert!(app.transcript.iter().any(|e| matches!(
        e,
        Entry::Tool {
            duration: Some(_),
            ..
        }
    )));
    app.finish_run().await;
}

#[tokio::test]
async fn interrupted_turn_is_pulled_up_and_long_commands_stay_one_row() {
    use crate::events::EventKind;
    let (mut app, mut rx) = app();
    type_line(&mut app, "long task", KeyModifiers::NONE).await;
    pump_until(&mut app, &mut rx, |a| a.session.is_some()).await;
    let cmd = "cd /tmp && python3 -c '\nimport json\nprint(1)\n'";
    app.handle(bus(EventKind::ToolStarted {
        id: "t1".into(),
        kind: "command".into(),
        title: cmd.into(),
    }))
    .await;
    let s = screen(&app);
    assert!(
        s.contains("cd /tmp && python3 -c '…"),
        "multi-line command clipped to one row: {s}"
    );
    assert!(!s.contains("import json"));
    press(&mut app, KeyCode::Esc).await;
    pump_until(&mut app, &mut rx, |a| !a.busy()).await;
    assert!(screen(&app).contains("pulled up ·"));
    app.finish_run().await;
}

#[tokio::test]
async fn missed_automated_correction_is_not_requeued_by_the_tui() {
    use crate::events::EventKind;
    let (mut app, _rx) = app();
    app.handle(bus(EventKind::SteeringSent {
        steer_id: "a1".into(),
        text: "Correction from an independent reviewer watching your progress (automated). …"
            .into(),
    }))
    .await;
    app.handle(bus(EventKind::SteeringRejected {
        steer_id: "a1".into(),
        reason: crate::providers::STEER_MISSED_TURN.into(),
    }))
    .await;
    assert!(
        !app.busy() && app.queue.is_empty(),
        "autosteer delivers its own follow-up"
    );
}

#[tokio::test]
async fn splash_keeps_the_mark_in_a_small_window() {
    let (mut app, _rx) = app();
    app.handle(key(KeyCode::Char('l'), KeyModifiers::CONTROL))
        .await;
    // 55x25 with the lab picker open: the small mark and the name.
    let s = screen_sized(&app, 55, 25);
    assert!(s.contains("J O C K E Y") && s.contains("@#"), "{s}");
    // Too short for any mark: the text alone.
    let s = screen_sized(&app, 55, 16);
    assert!(s.contains("J O C K E Y") && !s.contains("@#"), "{s}");
}

#[tokio::test]
async fn home_screen_shows_an_available_update() {
    let (mut app, _rx) = app();
    assert!(!screen(&app).contains("is available"));
    app.handle(Msg::Update(crate::update::Update {
        latest: "9.0.0".into(),
        command: "brew upgrade jockey".into(),
    }))
    .await;
    let s = screen(&app);
    assert!(s.contains("jockey 9.0.0 is available") && s.contains("brew upgrade jockey"));
}
