//! Provider adapters. Workflow code only sees `ProviderAdapter` and `AgentSession`;
//! everything CLI-specific lives in the submodules.

pub mod claude;
pub mod codex;
pub mod grok;
pub mod jsonrpc;
pub mod mock;

use crate::config::Config;
use crate::events::Emitter;
use crate::security::{Approver, Profile};
use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;

pub const PROVIDERS: &[&str] = &["codex", "claude", "gemini", "grok"];

/// `SteeringRejected` reason prefix for an acknowledged instruction the turn ended before applying.
pub const STEER_MISSED_TURN: &str = "turn ended before it was applied";

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{provider} does not support {capability}")]
    Unsupported {
        provider: &'static str,
        capability: &'static str,
    },
    #[error("{0}")]
    QuotaExhausted(String),
}

pub fn unsupported<T>(provider: &'static str, capability: &'static str) -> Result<T> {
    Err(ProviderError::Unsupported {
        provider,
        capability,
    }
    .into())
}

/// What an adapter can do. Workflows branch on this, never on provider names.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Capabilities {
    pub stream_events: bool,
    /// Accepts new instructions during an active turn.
    pub live_steer: bool,
    /// Live steering is acknowledged by the provider (not fire-and-forget).
    pub steer_ack: bool,
    pub interrupt: bool,
    pub resume: bool,
    /// Tool approvals are routed to jockey instead of the CLI's own UI.
    pub approvals_to_client: bool,
    pub read_only_mode: bool,
    pub list_models: bool,
    pub effort: bool,
    pub usage: bool,
    pub rate_limits: bool,
    /// Provider-native actions surfaced as `/<provider>:<name>`.
    pub native_commands: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Probe {
    pub provider: &'static str,
    pub path: Option<PathBuf>,
    pub version: Option<String>,
    pub adapter: &'static str,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub is_default: bool,
    pub efforts: Vec<String>,
    pub display: Option<String>,
    /// Versioned id when `id` is an alias (Claude `haiku` → `claude-haiku-5-5`).
    pub resolved: Option<String>,
    /// Shown next to the model in pickers, e.g. "may bill usage credits".
    pub note: Option<String>,
    /// Using it can bill beyond the subscription, so it needs `--allow-credits`.
    pub bills_credits: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Primary,
    #[allow(dead_code)] // judges/critics: cascade, critique, autosteer milestones
    Reviewer,
}

pub struct SessionSpec {
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub profile: Profile,
    pub role: Role,
    pub approver: Arc<dyn Approver>,
    /// Allow the provider to bill beyond the subscription (e.g. Codex credits).
    pub allow_credits: bool,
    /// Provider session to resume instead of starting fresh.
    pub resume_id: Option<String>,
    pub stderr_log: Option<PathBuf>,
}

impl SessionSpec {
    /// Reviewers are read-only no matter what was requested.
    pub fn effective_profile(&self) -> Profile {
        match self.role {
            Role::Reviewer => Profile::ReadOnly,
            Role::Primary => self.profile,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnOutcome {
    pub status: TurnStatus,
    pub final_text: String,
    pub error: Option<String>,
}

#[allow(dead_code)] // consumed by the autosteer controller milestone
#[derive(Debug, Clone, Serialize)]
pub struct SteerReceipt {
    pub steer_id: String,
    pub acknowledged: bool,
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn get_capabilities(&self) -> Capabilities;
    async fn probe(&self) -> Probe;
    /// Starts the CLI. Normalized events for the whole session go to `events`;
    /// that emitter is the adapter's event stream (`stream_events`).
    async fn start_session(
        &self,
        spec: SessionSpec,
        events: Emitter,
    ) -> Result<Arc<dyn AgentSession>>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        unsupported(self.id(), "list_models")
    }
    async fn get_usage(&self) -> Result<serde_json::Value> {
        unsupported(self.id(), "get_usage")
    }
}

/// A live provider session. All methods take `&self` so a workflow can steer
/// or interrupt from one task while another awaits `execute`.
#[async_trait]
pub trait AgentSession: Send + Sync {
    fn provider(&self) -> &'static str;
    fn session_id(&self) -> Option<String>;
    /// Runs one turn to completion.
    async fn execute(&self, prompt: &str) -> Result<TurnOutcome>;
    async fn steer(&self, _text: &str) -> Result<SteerReceipt> {
        unsupported(self.provider(), "steer")
    }
    async fn interrupt(&self) -> Result<()> {
        unsupported(self.provider(), "interrupt")
    }
    /// Changes model/effort for later turns without restarting the session.
    async fn set_turn_options(
        &self,
        _model: Option<String>,
        _effort: Option<String>,
    ) -> Result<()> {
        unsupported(self.provider(), "changing model mid-session")
    }
    /// Provider-native action (`/codex:review`). `None` when it doesn't produce a turn.
    async fn native_command(&self, _name: &str) -> Result<Option<TurnOutcome>> {
        unsupported(self.provider(), "native commands")
    }
    /// Kills the CLI and its process group. Does not roll back file changes.
    async fn cancel(&self);
}

pub fn adapter(id: &str, cfg: &Config) -> Result<Arc<dyn ProviderAdapter>> {
    let bin = cfg
        .provider(id)
        .path
        .clone()
        .unwrap_or_else(|| id.to_string());
    match id {
        "codex" => Ok(Arc::new(codex::Codex { bin })),
        "claude" => Ok(Arc::new(claude::Claude { bin })),
        "grok" => Ok(Arc::new(grok::Grok { bin })),
        "mock" => Ok(Arc::new(mock::Mock::default())),
        "gemini" => bail!(
            "Gemini CLI is unavailable: Google ended Gemini CLI sign-in for personal accounts on 2026-06-18 (see docs/providers.md)"
        ),
        p => bail!("unknown provider '{p}' (known: {})", PROVIDERS.join(", ")),
    }
}

/// Probe for providers whose adapter isn't built yet: just the binary.
pub async fn probe_binary(id: &'static str, cfg: &Config) -> Probe {
    let bin = cfg
        .provider(id)
        .path
        .clone()
        .unwrap_or_else(|| id.to_string());
    Probe {
        provider: id,
        path: crate::process::which(&bin),
        version: crate::process::version_of(&bin).await,
        adapter: "pending",
        notes: vec![],
    }
}

/// `provider[:model][@effort]`, e.g. `codex:gpt-6.1-sol@high` or `claude@low`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentSpec {
    pub provider: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl std::fmt::Display for AgentSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.provider)?;
        if let Some(m) = &self.model {
            write!(f, ":{m}")?;
        }
        if let Some(e) = &self.effort {
            write!(f, "@{e}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for AgentSpec {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        let (head, effort) = match s.rsplit_once('@') {
            Some((h, e)) if !e.is_empty() => (h, Some(e.to_string())),
            _ => (s, None),
        };
        let (provider, model) = match head.split_once(':') {
            Some((p, m)) if !m.is_empty() => (p, Some(m.to_string())),
            _ => (head, None),
        };
        if provider.is_empty() {
            bail!("empty provider in '{s}'");
        }
        Ok(Self {
            provider: provider.to_string(),
            model,
            effort,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_spec_syntax() {
        let a: AgentSpec = "codex:gpt-6.1-sol@high".parse().unwrap();
        assert_eq!(
            (a.provider.as_str(), a.model.as_deref(), a.effort.as_deref()),
            ("codex", Some("gpt-6.1-sol"), Some("high"))
        );
        let b: AgentSpec = "claude@low".parse().unwrap();
        assert_eq!((b.model, b.effort.as_deref()), (None, Some("low")));
        let c: AgentSpec = "ollama:qwen3:30b".parse().unwrap();
        assert_eq!(c.model.as_deref(), Some("qwen3:30b"));
        assert!("".parse::<AgentSpec>().is_err());
        for s in [
            "codex:gpt-6.1-sol@high",
            "claude@low",
            "grok",
            "ollama:qwen3:30b",
        ] {
            assert_eq!(s.parse::<AgentSpec>().unwrap().to_string(), s);
        }
    }
}
