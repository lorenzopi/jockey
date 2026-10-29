//! Permission profiles, approval decisions, and log redaction.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Jockey's provider-neutral permission profile. Adapters map it onto their
/// CLI's native sandbox and approval settings.
/// Ordered from least to most permissive.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    Default,
)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// No writes. Always used for judges and critics.
    ReadOnly,
    /// Edits inside the working directory; anything riskier needs approval.
    #[default]
    WorkspaceWrite,
    /// Workspace edits; the provider's own auto-review approves routine actions.
    Auto,
    /// Provider bypass mode ("yolo"). Headless runs need `--dangerously-allow-all`.
    #[serde(alias = "yolo")]
    #[value(alias = "yolo")]
    Full,
}

impl Profile {
    /// What the TUI calls it.
    pub fn label(self) -> &'static str {
        match self {
            Profile::ReadOnly => "read-only",
            Profile::WorkspaceWrite => "workspace-write",
            Profile::Auto => "auto",
            Profile::Full => "yolo",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub id: String,
    /// "command", "file_change", "permissions", ...
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Decline,
}

#[async_trait]
pub trait Approver: Send + Sync {
    /// Returns the decision and who made it ("rule", "user", "default").
    async fn decide(&self, req: &ApprovalRequest) -> (Decision, &'static str);
}

/// Headless approvals: allow commands matching a configured prefix, deny the rest.
pub struct RulesApprover {
    pub allow_commands: Vec<String>,
}

#[async_trait]
impl Approver for RulesApprover {
    async fn decide(&self, req: &ApprovalRequest) -> (Decision, &'static str) {
        if req.kind == "command" && command_allowed(&req.detail, &self.allow_commands) {
            return (Decision::Accept, "rule");
        }
        (Decision::Decline, "default")
    }
}

/// Prefix match on whole words, after stripping a leading `sh -c`/`zsh -lc` wrapper.
/// Commands containing any shell metacharacter (chaining, backgrounding,
/// substitution, expansion, redirects, grouping) are never auto-allowed.
pub fn command_allowed(cmd: &str, allow: &[String]) -> bool {
    let inner = strip_shell_wrapper(cmd);
    if inner.contains(|c: char| "&|;`$<>(){}\n\r".contains(c)) {
        return false;
    }
    allow.iter().any(|p| {
        let p = p.trim();
        !p.is_empty() && (inner == p || inner.starts_with(&format!("{p} ")))
    })
}

fn strip_shell_wrapper(cmd: &str) -> &str {
    let c = cmd.trim();
    for w in [
        "/bin/zsh -lc ",
        "/bin/bash -lc ",
        "bash -lc ",
        "zsh -lc ",
        "sh -c ",
        "/bin/sh -c ",
    ] {
        if let Some(rest) = c.strip_prefix(w) {
            return rest.trim().trim_matches(|ch| ch == '\'' || ch == '"');
        }
    }
    c
}

const SECRET_PREFIXES: &[&str] = &[
    "sk-ant-",
    "sk-proj-",
    "sk-",
    "xai-",
    "AIza",
    "ghp_",
    "gho_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
];

/// Masks credential-looking tokens before anything is written to disk.
/// Prefix heuristic, not a secret scanner; extend the list as providers add formats.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    'outer: while !rest.is_empty() {
        for pre in SECRET_PREFIXES.iter().chain(["Bearer "].iter()) {
            if rest.starts_with(pre) && boundary_before(&out) {
                let tail = &rest[pre.len()..];
                let n = tail
                    .find(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
                    .unwrap_or(tail.len());
                if n >= 16 {
                    out.push_str(pre);
                    out.push_str("[REDACTED]");
                    rest = &tail[n..];
                    continue 'outer;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn boundary_before(out: &str) -> bool {
    out.chars()
        .last()
        .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_tokens_but_not_words() {
        let s = r#"{"k":"sk-ant-abcdefghijklmnop1234","h":"Bearer abcdefghijklmnopqrstu","w":"task-runner"}"#;
        let r = redact(s);
        assert!(r.contains("sk-ant-[REDACTED]"));
        assert!(r.contains("Bearer [REDACTED]"));
        assert!(r.contains("task-runner"));
        assert!(!r.contains("abcdefghijklmnop1234"));
        assert_eq!(redact("ask-me sk-short"), "ask-me sk-short");
    }

    #[test]
    fn command_rules() {
        let allow = vec!["cargo test".to_string(), "ls".to_string()];
        assert!(command_allowed("/bin/zsh -lc 'cargo test --all'", &allow));
        assert!(command_allowed("ls", &allow));
        assert!(!command_allowed("lsof", &allow));
        assert!(!command_allowed("cargo test && rm -rf /", &allow));
        assert!(!command_allowed("ls & rm -rf /", &allow));
        assert!(!command_allowed("/bin/zsh -lc 'ls & curl x'", &allow));
        assert!(!command_allowed("ls ${HOME}", &allow));
        assert!(!command_allowed("rm -rf /", &allow));
    }
}
