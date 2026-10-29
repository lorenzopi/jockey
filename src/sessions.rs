//! Run persistence under `~/.jockey/runs/<run-id>/`:
//! `meta.json` (prompt, agents, config, outcome), `events.jsonl` (normalized
//! events) and `raw/<agent>.jsonl` (untranslated provider traffic).
//! Directories are 0700 and files 0600; every line is redacted before writing.

use crate::events::{Event, EventKind};
use crate::security::redact;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentAssignment {
    pub role: String,
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Provider-side session/thread id, usable for a real provider resume.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunMeta {
    pub run_id: String,
    pub created_ms: u64,
    pub ended_ms: Option<u64>,
    pub mode: String,
    pub prompt: String,
    pub cwd: PathBuf,
    pub agents: Vec<AgentAssignment>,
    /// Set when this run continued a provider session from an earlier run.
    pub resumed_from: Option<String>,
    pub config: Value,
    pub status: String,
    pub final_output: Option<String>,
    pub error: Option<String>,
    pub usage: Option<Value>,
}

pub fn runs_dir() -> PathBuf {
    crate::config::home().join("runs")
}

pub fn new_run_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn private_dir(p: &Path) -> Result<()> {
    fs::create_dir_all(p).with_context(|| format!("cannot create {}", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_file(p: &Path, append: bool) -> Result<File> {
    let mut o = OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(p)
        .with_context(|| format!("cannot write {}", p.display()))
}

pub fn create_run_dir(run_id: &str) -> Result<PathBuf> {
    let root = runs_dir();
    private_dir(&crate::config::home())?;
    private_dir(&root)?;
    let dir = root.join(run_id);
    private_dir(&dir)?;
    private_dir(&dir.join("raw"))?;
    Ok(dir)
}

pub fn write_meta(dir: &Path, meta: &RunMeta) -> Result<()> {
    let tmp = dir.join("meta.json.tmp");
    let mut f = private_file(&tmp, false)?;
    f.write_all(redact(&serde_json::to_string_pretty(meta)?).as_bytes())?;
    f.sync_all()?;
    fs::rename(tmp, dir.join("meta.json"))?;
    Ok(())
}

pub fn read_meta(dir: &Path) -> Result<RunMeta> {
    let s = fs::read_to_string(dir.join("meta.json"))
        .with_context(|| format!("no run at {}", dir.display()))?;
    Ok(serde_json::from_str(&s)?)
}

pub fn read_events(dir: &Path) -> Result<Vec<Event>> {
    let s = fs::read_to_string(dir.join("events.jsonl")).unwrap_or_default();
    Ok(s.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

/// Most recent first.
pub fn list_runs(limit: usize) -> Vec<RunMeta> {
    let Ok(rd) = fs::read_dir(runs_dir()) else {
        return vec![];
    };
    let mut ids: Vec<_> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a)); // uuid v7 sorts by time
    ids.iter()
        .filter_map(|id| read_meta(&runs_dir().join(id)).ok())
        .take(limit)
        .collect()
}

/// Accepts a full id, a unique prefix, or `last`.
pub fn resolve(id: &str) -> Result<PathBuf> {
    if id == "last" {
        return list_runs(1)
            .first()
            .map(|m| runs_dir().join(&m.run_id))
            .context("no saved runs");
    }
    let exact = runs_dir().join(id);
    if exact.join("meta.json").is_file() {
        return Ok(exact);
    }
    let hits: Vec<_> = list_runs(usize::MAX)
        .into_iter()
        .filter(|m| m.run_id.starts_with(id))
        .collect();
    match hits.as_slice() {
        [m] => Ok(runs_dir().join(&m.run_id)),
        [] => bail!("no run matching '{id}'"),
        _ => bail!("'{id}' matches {} runs; use more characters", hits.len()),
    }
}

/// The single consumer of the event bus: stamps `seq`, persists, then fans out.
/// A failed write does not stop it: subscribers still get every event plus one
/// warning, and the first error is returned when the bus closes.
pub fn spawn_recorder(
    dir: PathBuf,
    mut rx: mpsc::Receiver<Event>,
    out: broadcast::Sender<Event>,
) -> Result<JoinHandle<Result<()>>> {
    let mut events = BufWriter::new(private_file(&dir.join("events.jsonl"), true)?);
    Ok(tokio::spawn(async move {
        let mut raw: HashMap<String, BufWriter<File>> = HashMap::new();
        let mut seq = 0u64;
        let mut failed: Option<anyhow::Error> = None;
        while let Some(mut ev) = rx.recv().await {
            seq += 1;
            ev.seq = seq;
            let res = persist(&dir, &mut events, &mut raw, &ev, rx.is_empty());
            if let Err(e) = res
                && failed.is_none()
            {
                let _ = out.send(Event {
                    run_id: ev.run_id.clone(),
                    seq,
                    ts_ms: ev.ts_ms,
                    provider: "jockey".into(),
                    agent: "harness".into(),
                    kind: EventKind::Warning {
                        message: format!("saving the run failed; its log is incomplete: {e:#}"),
                    },
                });
                failed = Some(e);
            }
            let _ = out.send(ev); // no subscribers is fine
        }
        let flushed = flush_all(&mut events, &mut raw);
        match failed {
            Some(e) => Err(e),
            None => flushed,
        }
    }))
}

fn persist(
    dir: &Path,
    events: &mut BufWriter<File>,
    raw: &mut HashMap<String, BufWriter<File>>,
    ev: &Event,
    flush: bool,
) -> Result<()> {
    if let EventKind::Raw { value } = &ev.kind {
        let w = match raw.get_mut(&ev.agent) {
            Some(w) => w,
            None => {
                let f = private_file(&dir.join("raw").join(format!("{}.jsonl", ev.agent)), true)?;
                raw.entry(ev.agent.clone()).or_insert(BufWriter::new(f))
            }
        };
        let line = serde_json::json!({"seq": ev.seq, "ts_ms": ev.ts_ms, "msg": value});
        writeln!(w, "{}", redact(&line.to_string()))?;
    } else {
        writeln!(events, "{}", redact(&serde_json::to_string(ev)?))?;
    }
    if flush {
        flush_all(events, raw)?;
    }
    Ok(())
}

fn flush_all(
    events: &mut BufWriter<File>,
    raw: &mut HashMap<String, BufWriter<File>>,
) -> Result<()> {
    events.flush()?;
    for w in raw.values_mut() {
        w.flush()?;
    }
    Ok(())
}

/// Copies a child's stderr into a private (0600), redacted log. Keeps draining
/// after a write error so the child never blocks or gets SIGPIPE on stderr.
pub fn record_stderr(stderr: tokio::process::ChildStderr, path: &Path) -> Result<()> {
    use tokio::io::AsyncBufReadExt;
    let mut f = private_file(path, false)?;
    tokio::spawn(async move {
        let mut r = tokio::io::BufReader::new(stderr);
        let mut line = vec![];
        loop {
            line.clear();
            match r.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let _ = f.write_all(redact(&String::from_utf8_lossy(&line)).as_bytes());
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::bus;

    #[tokio::test]
    async fn recorder_orders_redacts_and_splits_raw() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("raw")).unwrap();
        let (root, rx, out) = bus("r1");
        let mut sub = out.subscribe();
        let h = spawn_recorder(dir.path().into(), rx, out).unwrap();
        let em = root.for_agent("codex", "primary");
        em.emit(EventKind::Warning {
            message: "key sk-proj-aaaaaaaaaaaaaaaaaaaa".into(),
        })
        .await;
        em.emit(EventKind::Raw {
            value: serde_json::json!({"method": "x"}),
        })
        .await;
        drop((root, em));
        h.await.unwrap().unwrap();

        let evs = read_events(dir.path()).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].seq, 1);
        let text = fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(text.contains("[REDACTED]") && !text.contains("aaaaaaaaaaaa"));
        let raw = fs::read_to_string(dir.path().join("raw/primary.jsonl")).unwrap();
        assert!(raw.contains("\"seq\":2"));
        assert_eq!(sub.recv().await.unwrap().seq, 1);
        assert_eq!(sub.recv().await.unwrap().seq, 2);
    }

    #[tokio::test]
    async fn recorder_keeps_broadcasting_after_a_write_error() {
        let dir = tempfile::tempdir().unwrap(); // no raw/ dir: raw writes fail
        let (root, rx, out) = bus("r1");
        let mut sub = out.subscribe();
        let h = spawn_recorder(dir.path().into(), rx, out).unwrap();
        let em = root.for_agent("codex", "primary");
        em.emit(EventKind::Raw {
            value: serde_json::json!({}),
        })
        .await;
        em.emit(EventKind::Warning {
            message: "after".into(),
        })
        .await;
        drop((root, em));
        assert!(h.await.unwrap().is_err(), "first error reported at close");
        let kinds: Vec<_> = std::iter::from_fn(|| sub.try_recv().ok())
            .map(|e| e.kind)
            .collect();
        assert!(
            matches!(&kinds[0], EventKind::Warning { message } if message.contains("incomplete"))
        );
        assert!(matches!(&kinds[1], EventKind::Raw { .. }));
        assert_eq!(
            kinds[2],
            EventKind::Warning {
                message: "after".into()
            }
        );
        assert_eq!(
            read_events(dir.path()).unwrap().len(),
            1,
            "later events still saved"
        );
    }
}
