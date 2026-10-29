//! Interactive terminal UI (`jockey` with no subcommand).

mod app;
mod editor;
mod horse_art;

mod theme;
mod track;
pub(crate) mod ui;

use crate::config::Config;
use crate::providers::{self, AgentSpec};
use crate::security::Profile;
use anyhow::Result;
use app::{App, Msg};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as CtEvent, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::{execute, terminal};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;

pub struct TuiOptions {
    pub cfg: Config,
    pub cwd: PathBuf,
    /// None: no flag and no saved default, so ask at startup.
    pub agent: Option<AgentSpec>,
    pub profile: Profile,
    pub allow_credits: bool,
    pub mode: String,
    pub judge: Option<AgentSpec>,
    pub fallback: Option<AgentSpec>,
}

pub async fn run(o: TuiOptions) -> Result<i32> {
    if let Some(a) = &o.agent {
        providers::adapter(&a.provider, &o.cfg)?; // fail before taking over the terminal
    }
    let needs_agent = o.agent.is_none();
    let agent = o.agent.unwrap_or_else(|| AgentSpec {
        provider: String::new(),
        model: None,
        effort: None,
    });
    let (tx, mut rx) = mpsc::channel::<Msg>(4096);
    let mut app = App::new(o.cfg, o.cwd, agent, o.profile, o.allow_credits, tx.clone());
    app.mode = o.mode;
    if o.judge.is_some() {
        app.judge = o.judge;
    }
    if o.fallback.is_some() {
        app.fallback = o.fallback;
    }
    if needs_agent {
        app.needs_agent = true; // home screen first; Enter opens the lab picker
    } else {
        app.load_models();
    }

    let mut term = ratatui::try_init()?;
    let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, EnableBracketedPaste);
    if enhanced {
        // Lets Shift+Enter be told apart from Enter.
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    let paused = Arc::new(AtomicBool::new(false));
    spawn_input(tx.clone(), paused.clone());
    spawn_ticker(tx.clone());
    spawn_signals(tx.clone());

    // No mouse capture: text selects and copies like in any terminal; PgUp/PgDn scroll.
    let result: Result<()> = async {
        loop {
            term.draw(|f| ui::draw(f, &app))?;
            let Some(msg) = rx.recv().await else { break };
            // Some terminals reflow the screen on resize; repaint everything.
            if matches!(msg, Msg::Resize) {
                term.clear()?;
            }
            app.handle(msg).await;
            // Drain whatever else is ready before redrawing.
            while let Ok(m) = rx.try_recv() {
                app.handle(m).await;
            }
            if app.external_edit {
                app.external_edit = false;
                paused.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(80)).await; // let the input thread park
                ratatui::restore();
                let edited = edit_externally(&app.editor.text);
                term = ratatui::try_init()?;
                term.clear()?;
                paused.store(false, Ordering::SeqCst);
                match edited {
                    Ok(t) => app.editor.set(t.trim_end_matches('\n')),
                    Err(e) => app.notice(app::Level::Error, format!("editor: {e:#}")),
                }
            }
            if app.quit {
                if app.busy() {
                    if !app.interrupting {
                        app.handle(Msg::Key(event::KeyEvent::from(event::KeyCode::Esc)))
                            .await;
                    }
                    // Wait (briefly) for the interrupted turn to come back.
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                    while app.busy() {
                        match tokio::time::timeout_at(deadline, rx.recv()).await {
                            Ok(Some(m)) => app.handle(m).await,
                            _ => break,
                        }
                    }
                }
                break;
            }
        }
        Ok(())
    }
    .await;

    if enhanced {
        let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    let run_id = app.run.as_ref().map(|r| r.id.clone());
    app.finish_run().await;
    result?;
    if let Some(id) = run_id {
        eprintln!("run saved · jockey show {}", &id[..13]);
    }
    Ok(0)
}

/// Blocking reader on its own thread; `paused` hands the tty to an external editor.
fn spawn_input(tx: mpsc::Sender<Msg>, paused: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        loop {
            if tx.is_closed() {
                break;
            }
            if paused.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(30));
                continue;
            }
            if !event::poll(Duration::from_millis(50)).unwrap_or(false)
                || paused.load(Ordering::SeqCst)
            {
                continue;
            }
            let msg = match event::read() {
                Ok(CtEvent::Key(k)) if k.kind != KeyEventKind::Release => Msg::Key(k),
                Ok(CtEvent::Paste(s)) => Msg::Paste(s),
                Ok(CtEvent::Resize(..)) => Msg::Resize,
                _ => continue,
            };
            if tx.blocking_send(msg).is_err() {
                break;
            }
        }
    });
}

/// SIGHUP (terminal closed) and SIGTERM quit cleanly so the run is saved, not left "running".
fn spawn_signals(tx: mpsc::Sender<Msg>) {
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut hup), Ok(mut term)) = (
            signal(SignalKind::hangup()),
            signal(SignalKind::terminate()),
        ) else {
            return;
        };
        tokio::select! {
            _ = hup.recv() => {}
            _ = term.recv() => {}
        }
        let _ = tx.send(Msg::Quit).await;
    });
}

/// Spinner and elapsed-time refresh.
fn spawn_ticker(tx: mpsc::Sender<Msg>) {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_millis(80));
        loop {
            t.tick().await;
            // try_send: never pile up ticks behind real work.
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(Msg::Redraw) {
                break;
            }
        }
    });
}

fn edit_externally(initial: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("jockey-prompt-{}.md", std::process::id()));
    std::fs::write(&path, initial)?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&path)
        .status()?;
    let text = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    anyhow::ensure!(status.success(), "{editor} exited with {status}");
    Ok(text)
}

#[cfg(test)]
mod tests;
