//! Lifecycle of one saved run: directory, event bus, recorder, and closing
//! metadata. Shared by `jockey run` and the TUI (where a run spans many turns).

use crate::config::Config;
use crate::events::{self, Emitter, Event, EventKind};
use crate::security::{Approver, Profile};
use crate::sessions::{self, AgentAssignment, RunMeta};
use crate::workflows::RunCtx;
use anyhow::{Result, bail};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

pub struct RunOptions {
    pub cfg: Config,
    pub cwd: PathBuf,
    pub mode: String,
    pub profile: Profile,
    pub allow_credits: bool,
    pub timeout: Duration,
    pub approver: Arc<dyn Approver>,
    pub first_prompt: String,
    pub resumed_from: Option<String>,
}

pub struct Run {
    pub id: String,
    pub dir: PathBuf,
    pub meta: RunMeta,
    pub root: Emitter,
    pub ctx: Arc<RunCtx>,
    recorder: JoinHandle<Result<()>>,
    out: broadcast::Sender<Event>,
}

/// `full` only ever comes from the command line flag, never from config.
pub fn resolve_profile(flag: Option<Profile>, dangerously: bool, cfg: &Config) -> Result<Profile> {
    Ok(match (flag, dangerously) {
        (Some(Profile::Full), false) => bail!("--profile full requires --dangerously-allow-all"),
        (_, true) => Profile::Full,
        (Some(p), false) => p,
        (None, false) if cfg.defaults.profile == Profile::Full => {
            bail!("profile 'full' cannot come from config; pass --dangerously-allow-all")
        }
        (None, false) => cfg.defaults.profile,
    })
}

impl Run {
    pub fn start(o: RunOptions) -> Result<Self> {
        let id = sessions::new_run_id();
        let dir = sessions::create_run_dir(&id)?;
        let meta = RunMeta {
            run_id: id.clone(),
            created_ms: events::now_ms(),
            mode: o.mode,
            prompt: o.first_prompt,
            cwd: o.cwd.clone(),
            resumed_from: o.resumed_from,
            config: serde_json::to_value(&o.cfg)?,
            status: "running".into(),
            ..Default::default()
        };
        sessions::write_meta(&dir, &meta)?;
        let (root, rx, out) = events::bus(&id);
        let recorder = sessions::spawn_recorder(dir.clone(), rx, out.clone())?;
        let ctx = Arc::new(RunCtx::new(
            o.cfg,
            o.cwd,
            o.profile,
            o.allow_credits,
            o.timeout,
            dir.clone(),
            root.clone(),
            o.approver,
            out.clone(),
        ));
        Ok(Self {
            id,
            dir,
            meta,
            root,
            ctx,
            recorder,
            out,
        })
    }

    /// Subscribe before emitting anything you want to see.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.out.subscribe()
    }

    pub fn set_agents(&mut self, agents: Vec<AgentAssignment>) {
        self.meta.agents = agents;
        let _ = sessions::write_meta(&self.dir, &self.meta);
    }

    /// Kills remaining agents, drains the event log, and writes final metadata.
    pub async fn finish(
        self,
        status: &str,
        final_output: Option<String>,
        error: Option<String>,
    ) -> Result<RunMeta> {
        self.ctx.shutdown().await;
        let Run {
            dir,
            mut meta,
            root,
            ctx,
            recorder,
            out,
            ..
        } = self;
        // The recorder ends once every emitter is gone.
        drop((root, ctx, out));
        let log_error = match tokio::time::timeout(Duration::from_secs(10), recorder).await {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(e))) => Some(format!("event log incomplete: {e:#}")),
            Ok(Err(e)) => Some(format!("event recorder crashed: {e}")),
            Err(_) => {
                eprintln!("warning: event log did not close cleanly");
                None
            }
        };
        meta.status = status.into();
        meta.final_output = final_output;
        meta.error = match (error, log_error) {
            (Some(e), Some(l)) => Some(format!("{e}; {l}")),
            (e, l) => e.or(l),
        };
        meta.usage = summarize_usage(&sessions::read_events(&dir)?);
        meta.ended_ms = Some(events::now_ms());
        sessions::write_meta(&dir, &meta)?;
        Ok(meta)
    }
}

/// Last reported usage and rate limit per agent.
pub fn summarize_usage(evs: &[Event]) -> Option<serde_json::Value> {
    let mut m = serde_json::Map::new();
    for e in evs {
        let key = match &e.kind {
            EventKind::UsageUpdated { .. } => "tokens",
            EventKind::RateLimit { .. } => "rate_limit",
            _ => continue,
        };
        let entry = m
            .entry(format!("{}/{}", e.provider, e.agent))
            .or_insert_with(|| json!({}));
        entry[key] = serde_json::to_value(&e.kind).ok()?["data"].clone();
    }
    (!m.is_empty()).then_some(serde_json::Value::Object(m))
}
