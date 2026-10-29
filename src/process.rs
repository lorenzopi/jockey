//! Child processes that never outlive the run.
//!
//! Each provider CLI is started in its own process group so that cancelling
//! kills the CLI and every tool process it spawned, not just the top PID.

use anyhow::{Context, Result};
use std::time::Duration;
use tokio::process::{Child, Command};

pub struct ManagedChild {
    pub child: Child,
    pgid: Option<i32>,
}

impl ManagedChild {
    pub fn spawn(cmd: &mut Command) -> Result<Self> {
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn().context("failed to start provider CLI")?;
        let pgid = child.id().map(|p| p as i32);
        Ok(Self { child, pgid })
    }

    /// SIGTERM the whole group, then SIGKILL after `grace`.
    pub async fn terminate(&mut self, grace: Duration) {
        self.signal_group(libc::SIGTERM);
        if tokio::time::timeout(grace, self.child.wait())
            .await
            .is_err()
        {
            self.signal_group(libc::SIGKILL);
            let _ = self.child.wait().await;
        }
        // The leader is gone but tool processes may linger in the group.
        self.signal_group(libc::SIGKILL);
        self.pgid = None;
    }

    fn signal_group(&self, sig: i32) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            // SAFETY: plain syscall; a stale pgid only yields ESRCH.
            unsafe {
                libc::killpg(pgid, sig);
            }
        }
        // Windows needs a Job Object here; kill_on_drop covers the leader only.
        #[cfg(not(unix))]
        let _ = sig;
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.signal_group(libc::SIGKILL);
    }
}

/// `<bin> --version`, first line, or None if the binary is missing.
pub async fn version_of(bin: &str) -> Option<String> {
    let out = Command::new(bin)
        .arg("--version")
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().next().map(|l| l.trim().to_string())
}

/// Absolute path of `bin` on PATH, if any.
pub fn which(bin: &str) -> Option<std::path::PathBuf> {
    if bin.contains('/') {
        let p = std::path::PathBuf::from(bin);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminate_kills_grandchildren() {
        // sh spawns a background sleep that would orphan without group kill.
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()));
        let mut child = ManagedChild::spawn(&mut cmd).unwrap();
        for _ in 0..50 {
            if pidfile.exists() && !std::fs::read_to_string(&pidfile).unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        child.terminate(Duration::from_millis(500)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "grandchild {pid} survived cancellation");
    }
}
