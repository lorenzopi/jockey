//! Startup update check: at most every 20 hours, ask GitHub for the newest
//! version tag, and say how to upgrade for the way jockey was installed.
//! `JOCKEY_NO_UPDATE_CHECK=1` turns it off.

use std::time::Duration;

const TAGS_URL: &str = "https://api.github.com/repos/lorenzopi/jockey/tags";
const EVERY: Duration = Duration::from_secs(20 * 3600);

pub struct Update {
    pub latest: String,
    pub command: String,
}

/// Some when a newer release exists than the running build.
pub async fn check() -> Option<Update> {
    if std::env::var_os("JOCKEY_NO_UPDATE_CHECK").is_some() {
        return None;
    }
    let command = update_command()?;
    let latest = latest().await?;
    (parse(&latest)? > parse(env!("CARGO_PKG_VERSION"))?).then_some(Update { latest, command })
}

/// Cached in `~/.jockey/latest-version`; refetched once the cache is older than `EVERY`.
async fn latest() -> Option<String> {
    let cache = crate::config::home().join("latest-version");
    let fresh = std::fs::metadata(&cache)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < EVERY);
    if fresh {
        return std::fs::read_to_string(&cache)
            .ok()
            .map(|s| s.trim().to_string());
    }
    // curl ships with macOS, so no HTTP client dependency.
    let out = tokio::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "5", TAGS_URL])
        .output()
        .await
        .ok()?;
    let tags: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    let latest = newest(tags.iter().filter_map(|t| t["name"].as_str()))?;
    let _ = std::fs::create_dir_all(crate::config::home());
    let _ = std::fs::write(&cache, &latest);
    Some(latest)
}

/// Highest `vX.Y.Z` tag, without the `v`; pre-release tags are skipped.
fn newest<'a>(tags: impl Iterator<Item = &'a str>) -> Option<String> {
    tags.filter_map(|t| t.strip_prefix('v'))
        .filter_map(|v| parse(v).map(|p| (p, v)))
        .max()
        .map(|(_, v)| v.to_string())
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.split('.').map(|n| n.parse().ok());
    let p = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(p)
}

/// None for a development build, which has nothing to upgrade.
fn update_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let exe = exe.to_string_lossy();
    if exe.contains("/target/") {
        None
    } else if exe.contains("/Cellar/") {
        Some("brew upgrade jockey".into())
    } else {
        Some("cargo install --locked --git https://github.com/lorenzopi/jockey".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_highest_release_tag() {
        let tags = ["v0.9.0", "v0.10.0", "v1.0.0-rc1", "nightly", "v0.2.0"];
        assert_eq!(newest(tags.into_iter()).as_deref(), Some("0.10.0"));
        assert!(parse("0.10.0") > parse("0.9.9"));
        assert_eq!(parse("1.2"), None);
    }
}
