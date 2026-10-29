//! Startup update check: ask GitHub for the newest version tag (in the
//! background, so it never delays startup), and say how to upgrade for the
//! way jockey was installed.
//! `JOCKEY_NO_UPDATE_CHECK=1` turns it off.

const TAGS_URL: &str = "https://api.github.com/repos/lorenzopi/jockey/tags";

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

/// Asked fresh every launch: a cached answer hid new releases for hours.
async fn latest() -> Option<String> {
    // curl ships with macOS, so no HTTP client dependency.
    let out = tokio::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "5", TAGS_URL])
        .output()
        .await
        .ok()?;
    let tags: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;
    newest(tags.iter().filter_map(|t| t["name"].as_str()))
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

/// `jockey update`: runs the upgrade command matching how jockey was installed.
pub fn run() -> anyhow::Result<i32> {
    let Some(cmd) = update_command() else {
        anyhow::bail!("this is a development build; pull and rebuild it instead");
    };
    eprintln!("running: {cmd}");
    let status = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .status()?;
    Ok(status.code().unwrap_or(1))
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
