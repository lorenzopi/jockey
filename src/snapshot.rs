//! Where judges and critics run.
//!
//! Reviewers always run in their provider's read-only mode. They get the real
//! workspace when the work is finished, an empty folder when nothing changed,
//! and a snapshot only while the primary may still be editing (autosteer), so
//! they read one consistent state. Snapshot files are cloned copy-on-write
//! where the filesystem supports it (APFS, Btrfs, XFS, ReFS) and plain-copied
//! elsewhere, then chmod'ed read-only.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

/// Heavy or irrelevant trees reviewers don't need.
/// Fixed list, not .gitignore-aware; use the `ignore` crate if snapshots get slow.
const SKIP: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".jockey",
];
const MAX_FILES: usize = 200_000;

pub enum ReviewDir {
    /// The workspace itself; the work is done, so it holds still.
    Live(PathBuf),
    /// No file changes to verify: no workspace access needed.
    Empty(tempfile::TempDir),
    /// The primary is still working: freeze what reviewers read.
    Frozen(Snapshot),
}

impl ReviewDir {
    pub async fn new(cwd: &Path, changed: bool, primary_running: bool) -> Result<Self> {
        Ok(match (changed, primary_running) {
            (false, _) => Self::Empty(
                tempfile::Builder::new()
                    .prefix("jockey-review-")
                    .tempdir()?,
            ),
            (true, false) => Self::Live(cwd.to_path_buf()),
            (true, true) => Self::Frozen(Snapshot::take(cwd).await?),
        })
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Live(p) => p,
            Self::Empty(d) => d.path(),
            Self::Frozen(s) => &s.path,
        }
    }
}

pub struct Snapshot {
    root: tempfile::TempDir,
    pub path: PathBuf,
}

impl Snapshot {
    /// Copies on a blocking thread; a large workspace takes a while.
    pub async fn take(src: &Path) -> Result<Self> {
        let src = src.to_path_buf();
        tokio::task::spawn_blocking(move || Self::take_blocking(&src)).await?
    }

    fn take_blocking(src: &Path) -> Result<Self> {
        // Home holds protected folders (~/Library) and far too much to copy.
        if std::env::home_dir().is_some_and(|h| h == src) || src.parent().is_none() {
            bail!(
                "reviewers need a project folder, not {}; run jockey inside a project (or use /mode single)",
                src.display()
            );
        }
        let mut count = 0;
        count_tree(src, &mut count).with_context(|| format!("snapshotting {}", src.display()))?;
        let root = tempfile::Builder::new()
            .prefix("jockey-snapshot-")
            .tempdir()?;
        let path = root
            .path()
            .join(src.file_name().unwrap_or_else(|| "workspace".as_ref()));
        // Built before copying so a failed copy is still made writable and removed.
        let snap = Self { root, path };
        copy_tree(src, &snap.path).with_context(|| format!("snapshotting {}", src.display()))?;
        Ok(snap)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // Read-only dirs can't be emptied; restore write permission first.
        let _ = set_tree_writable(self.root.path());
    }
}

fn kept_entries(src: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut out = vec![];
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        if !SKIP.iter().any(|s| entry.file_name() == *s) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Refuses oversized workspaces before anything is copied.
fn count_tree(src: &Path, count: &mut usize) -> Result<()> {
    for entry in kept_entries(src)? {
        *count += 1;
        if *count > MAX_FILES {
            bail!("workspace has more than {MAX_FILES} files; too large to snapshot");
        }
        if entry.file_type()?.is_dir() {
            count_tree(&entry.path(), count)?;
        }
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in kept_entries(src)? {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&from)?;
            // An absolute link still points at the real file, writable through
            // the copy. Dropped, not rewritten into the snapshot.
            #[cfg(unix)]
            if target.is_relative() {
                std::os::unix::fs::symlink(target, &to)?;
            }
        } else if ft.is_file() {
            reflink_copy::reflink_or_copy(&from, &to)?;
            set_mode(&to, 0o444)?;
        }
    }
    set_mode(dst, 0o555)
}

fn set_mode(p: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let mut perm = fs::metadata(p)?.permissions();
        perm.set_readonly(mode & 0o200 == 0);
        fs::set_permissions(p, perm)?;
    }
    Ok(())
}

fn set_tree_writable(p: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(p)?;
    if meta.is_dir() {
        set_mode(p, 0o755)?;
        for e in fs::read_dir(p)? {
            set_tree_writable(&e?.path())?;
        }
    } else if meta.is_file() {
        set_mode(p, 0o644)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn review_dir_copies_only_while_the_primary_runs() {
        let ws = tempfile::tempdir().unwrap();
        fs::write(ws.path().join("a.rs"), "x").unwrap();
        let empty = ReviewDir::new(ws.path(), false, true).await.unwrap();
        assert_eq!(fs::read_dir(empty.path()).unwrap().count(), 0);
        let live = ReviewDir::new(ws.path(), true, false).await.unwrap();
        assert_eq!(live.path(), ws.path());
        let frozen = ReviewDir::new(ws.path(), true, true).await.unwrap();
        assert_ne!(frozen.path(), ws.path());
        assert!(frozen.path().join("a.rs").exists());
    }

    #[tokio::test]
    async fn snapshot_is_read_only_and_isolated() {
        let ws = tempfile::tempdir().unwrap();
        fs::create_dir_all(ws.path().join("src")).unwrap();
        fs::write(ws.path().join("src/lib.rs"), "fn a() {}").unwrap();
        fs::create_dir_all(ws.path().join(".git")).unwrap();
        fs::write(ws.path().join(".git/HEAD"), "ref").unwrap();

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(ws.path().join("src/lib.rs"), ws.path().join("abs"))
                .unwrap();
            std::os::unix::fs::symlink("src/lib.rs", ws.path().join("rel")).unwrap();
        }

        let snap = Snapshot::take(ws.path()).await.unwrap();
        let copy = snap.path.join("src/lib.rs");
        assert_eq!(fs::read_to_string(&copy).unwrap(), "fn a() {}");
        assert!(!snap.path.join(".git").exists());
        #[cfg(unix)]
        {
            assert!(
                snap.path.join("abs").symlink_metadata().is_err(),
                "absolute link dropped"
            );
            assert_eq!(
                fs::read_to_string(snap.path.join("rel")).unwrap(),
                "fn a() {}"
            );
        }

        // A reviewer writing into the snapshot is refused, and the original is untouched.
        assert!(fs::write(&copy, "hacked").is_err());
        assert!(fs::write(snap.path.join("src/new.rs"), "x").is_err());
        assert_eq!(
            fs::read_to_string(ws.path().join("src/lib.rs")).unwrap(),
            "fn a() {}"
        );

        let root = snap.root.path().to_path_buf();
        drop(snap);
        assert!(!root.exists(), "snapshot cleaned up");
    }
}
