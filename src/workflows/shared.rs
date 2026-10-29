//! Pieces shared by the multi-agent workflows: deterministic checks and
//! collecting what an agent changed.

use super::RunCtx;
use crate::events::{Event, EventKind};
use crate::process::ManagedChild;
use std::process::Stdio;
use std::time::Duration;
use tokio::sync::broadcast;

const DIFF_CAP: usize = 60_000;
pub const NO_CHANGES: &str = "(the agent reported no file changes)";
const OUTPUT_TAIL: usize = 4_000;

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub command: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub output_tail: String,
}

/// Diffs the agent reported for this attempt (latest turn diff wins, else per-file diffs).
pub async fn collect_changes(sub: &mut broadcast::Receiver<Event>, label: &str) -> String {
    let mut turn_diff = None;
    let mut files = String::new();
    // Events are still draining from the recorder when the turn returns; stop at its end marker.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let ev = match tokio::time::timeout_at(deadline, sub.recv()).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        };
        if ev.agent != label {
            continue;
        }
        match ev.kind {
            EventKind::Diff { unified } => turn_diff = Some(unified),
            EventKind::FileChanged { path, change, diff } => {
                files.push_str(&format!("--- {change} {path}\n{diff}\n"));
            }
            EventKind::AgentCompleted { .. }
            | EventKind::AgentFailed { .. }
            | EventKind::AgentInterrupted { .. } => {
                break;
            }
            _ => {}
        }
    }
    let mut d = turn_diff.unwrap_or(files);
    if d.trim().is_empty() {
        d = NO_CHANGES.into();
    }
    truncate(&d, DIFF_CAP)
}

pub async fn run_checks(ctx: &RunCtx, checks: &[String], timeout: Duration) -> Vec<CheckResult> {
    let mut out = vec![];
    for cmd in checks {
        let r = run_check(ctx, cmd, timeout).await;
        ctx.ev
            .emit(EventKind::CheckCompleted {
                command: r.command.clone(),
                ok: r.ok,
                exit_code: r.exit_code,
                output_tail: r.output_tail.clone(),
            })
            .await;
        out.push(r);
    }
    out
}

async fn run_check(ctx: &RunCtx, cmd: &str, timeout: Duration) -> CheckResult {
    let fail = |msg: String| CheckResult {
        command: cmd.into(),
        ok: false,
        exit_code: None,
        output_tail: msg,
    };
    let mut c = tokio::process::Command::new("sh");
    c.arg("-c")
        .arg(cmd)
        .current_dir(&ctx.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match ManagedChild::spawn(&mut c) {
        Ok(ch) => ch,
        Err(e) => return fail(format!("could not start: {e:#}")),
    };
    let (mut so, mut se) = (
        child.child.stdout.take().unwrap(),
        child.child.stderr.take().unwrap(),
    );
    let read = async {
        let (mut a, b) = tokio::join!(read_tail(&mut so), read_tail(&mut se));
        a.extend(b);
        a
    };
    match tokio::time::timeout(timeout, async {
        let out = read.await;
        (child.child.wait().await, out)
    })
    .await
    {
        Ok((Ok(status), out)) => CheckResult {
            command: cmd.into(),
            ok: status.success(),
            exit_code: status.code(),
            output_tail: tail(&String::from_utf8_lossy(&out), OUTPUT_TAIL),
        },
        Ok((Err(e), _)) => fail(format!("wait failed: {e}")),
        Err(_) => {
            child.terminate(Duration::from_secs(2)).await;
            fail(format!("timed out after {}s", timeout.as_secs()))
        }
    }
}

/// Drains a pipe, keeping only about the last `OUTPUT_TAIL` bytes.
async fn read_tail(r: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let (mut keep, mut buf) = (vec![], [0u8; 8192]);
    while let Ok(n @ 1..) = r.read(&mut buf).await {
        keep.extend_from_slice(&buf[..n]);
        if keep.len() > 2 * OUTPUT_TAIL {
            keep.drain(..keep.len() - OUTPUT_TAIL);
        }
    }
    keep
}

/// Reviewers may work in a snapshot, so absolute paths into the real workspace
/// (in drafts, diffs, check output) are made relative to resolve in either.
pub fn relativize(text: &str, workspace: &std::path::Path) -> String {
    let w = workspace.display().to_string();
    // macOS reports /tmp paths both with and without the /private prefix.
    let alt = w
        .strip_prefix("/private")
        .map(String::from)
        .unwrap_or_else(|| format!("/private{w}"));
    let mut out = text.to_string();
    for prefix in [w, alt] {
        out = out.replace(&format!("{prefix}/"), "").replace(&prefix, ".");
    }
    out
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[{} bytes truncated]", &s[..end], s.len() - end)
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_tail_is_bounded() {
        let data = vec![b'x'; 1_000_000];
        let tail = read_tail(&mut data.as_slice()).await;
        assert!(tail.len() <= 2 * OUTPUT_TAIL && tail.len() >= OUTPUT_TAIL);
    }

    #[test]
    fn relativize_handles_private_tmp_aliases() {
        let ws = std::path::Path::new("/private/tmp/ws");
        assert_eq!(
            relativize(
                "Created /private/tmp/ws/a.py and /tmp/ws/b.py in /tmp/ws",
                ws
            ),
            "Created a.py and b.py in ."
        );
        assert_eq!(relativize("/Users/x/other/c.py", ws), "/Users/x/other/c.py");
    }
}
