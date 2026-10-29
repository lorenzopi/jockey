//! Command-line interface. Every command works without the TUI.

use crate::config::{self, Config};
use crate::events::EventKind;
use crate::providers::{self, AgentSpec, PROVIDERS, TurnStatus};
use crate::render::TextRenderer;
use crate::run::{self, Run, RunOptions};
use crate::security::{Profile, RulesApprover};
use crate::sessions;
use crate::workflows;
use crate::workflows::autosteer::AutosteerOpts;
use crate::workflows::cascade::CascadeOpts;
use crate::workflows::critique::CritiqueOpts;
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

#[derive(Parser)]
#[command(
    name = "jockey",
    version,
    about = "Orchestrate coding-agent CLIs: single, cascade, critique, autosteer"
)]
pub struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Interactive TUI agent: provider[:model][@effort]
    #[arg(long)]
    agent: Option<AgentSpec>,
    /// Interactive TUI working directory
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Interactive TUI permission profile
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    /// Allow providers to bill beyond the subscription (e.g. Codex credits)
    #[arg(long)]
    allow_credits: bool,
    /// Required for --profile full
    #[arg(long)]
    dangerously_allow_all: bool,
    /// Interactive TUI mode: single or cascade
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    /// Interactive TUI cascade judge
    #[arg(long)]
    judge: Option<AgentSpec>,
    /// Interactive TUI cascade fallback
    #[arg(long)]
    fallback: Option<AgentSpec>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a task
    Run(Box<RunArgs>),
    /// List provider CLIs, versions and adapter capabilities
    Providers {
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
        /// Also list each provider's models and supported effort levels
        #[arg(long)]
        models: bool,
    },
    /// Check installation, configuration, auth and quota
    Doctor,
    /// List saved runs
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Show a saved run (`last`, an id, or an id prefix)
    Show {
        id: String,
        #[arg(long, value_enum, default_value_t = Format::Text)]
        format: Format,
    },
    /// Continue a saved run's provider session with a new prompt
    Resume {
        id: String,
        prompt: Vec<String>,
        #[command(flatten)]
        opts: CommonOpts,
    },
    /// Print the effective configuration and where it came from
    Config {
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    Single,
    Cascade,
    Critique,
    Autosteer,
}

#[derive(Args)]
struct CommonOpts {
    /// Working directory the agent operates in
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Per-turn timeout in seconds
    #[arg(long)]
    timeout: Option<u64>,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// Permission profile for the primary agent
    #[arg(long, value_enum)]
    profile: Option<Profile>,
    /// Allow providers to bill beyond the subscription (e.g. Codex credits)
    #[arg(long)]
    allow_credits: bool,
    /// Required for --profile full: runs the provider in its bypass mode
    #[arg(long)]
    dangerously_allow_all: bool,
}

#[derive(Args)]
struct RunArgs {
    /// Prompt text; `-` or no prompt with piped stdin reads stdin
    prompt: Vec<String>,
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    /// provider[:model][@effort], e.g. codex:gpt-6.1-sol@high
    #[arg(long)]
    agent: Option<AgentSpec>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<String>,
    #[arg(long)]
    judge: Option<AgentSpec>,
    #[arg(long)]
    fallback: Option<AgentSpec>,
    #[arg(long, value_delimiter = ',')]
    critics: Vec<AgentSpec>,
    /// Autosteer policy: observe, suggest (manual), balanced, aggressive
    #[arg(long, value_enum)]
    policy: Option<crate::steering::Policy>,
    #[arg(long)]
    preset: Option<String>,
    /// Read the prompt from a file
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    #[command(flatten)]
    opts: CommonOpts,
}

pub async fn main() -> Result<i32> {
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
                bail!("the TUI needs a terminal; use `jockey run` for scripts");
            }
            let cwd = resolve_cwd(cli.cwd)?;
            let (cfg, warnings) = Config::load(&cwd)?;
            for w in &warnings {
                eprintln!("warning: {w}");
            }
            // No flag and no saved default: the TUI asks which model to use.
            let agent = match cli.agent {
                Some(a) => Some(a),
                None => configured_agent(&cfg)?,
            };
            let profile = run::resolve_profile(cli.profile, cli.dangerously_allow_all, &cfg)?;
            let mode = match cli.mode {
                None | Some(Mode::Single) => "single",
                Some(Mode::Cascade) => {
                    CascadeOpts::resolve(&cfg.cascade, cli.judge.clone(), cli.fallback.clone())?;
                    "cascade"
                }
                Some(Mode::Critique) => "critique",
                Some(Mode::Autosteer) => "autosteer",
            };
            crate::tui::run(crate::tui::TuiOptions {
                cfg,
                cwd,
                agent,
                profile,
                allow_credits: cli.allow_credits,
                mode: mode.into(),
                judge: cli.judge,
                fallback: cli.fallback,
            })
            .await
        }

        Some(Cmd::Run(a)) => cmd_run(*a).await,
        Some(Cmd::Providers { format, models }) => cmd_providers(format, models).await,
        Some(Cmd::Doctor) => cmd_doctor().await,
        Some(Cmd::History { limit, format }) => cmd_history(limit, format),
        Some(Cmd::Show { id, format }) => cmd_show(&id, format),
        Some(Cmd::Resume { id, prompt, opts }) => cmd_resume(&id, prompt, opts).await,
        Some(Cmd::Config { cwd }) => cmd_config(cwd),
    }
}

fn resolve_cwd(cwd: Option<PathBuf>) -> Result<PathBuf> {
    let p = match cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    p.canonicalize()
        .with_context(|| format!("working directory {} not found", p.display()))
}

fn read_prompt(args: &[String], file: Option<&PathBuf>) -> Result<String> {
    let p = if let Some(f) = file {
        std::fs::read_to_string(f).with_context(|| format!("cannot read {}", f.display()))?
    } else if (args.is_empty() && !std::io::stdin().is_terminal()) || args == ["-"] {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        args.join(" ")
    };
    if p.trim().is_empty() {
        bail!("empty prompt");
    }
    Ok(p)
}

async fn cmd_run(a: RunArgs) -> Result<i32> {
    let cwd = resolve_cwd(a.opts.cwd.clone())?;
    let (cfg, warnings) = Config::load(&cwd)?;
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    if a.preset.is_some() {
        bail!("presets arrive with the cascade/critique/autosteer milestones");
    }
    let mode = match a.mode {
        Some(m) => m,
        None => Mode::from_str(&cfg.defaults.mode, true)
            .map_err(|e| anyhow::anyhow!("defaults.mode: {e}"))?,
    };
    let workflow = match mode {
        Mode::Single => {
            if a.judge.is_some()
                || a.fallback.is_some()
                || !a.critics.is_empty()
                || a.policy.is_some()
            {
                bail!("--judge/--fallback/--critics/--policy need a multi-agent mode, not single");
            }
            Workflow::Single { resume_id: None }
        }
        Mode::Cascade => Workflow::Cascade(CascadeOpts::resolve(
            &cfg.cascade,
            a.judge.clone(),
            a.fallback.clone(),
        )?),
        Mode::Critique => {
            if a.judge.is_some() || a.fallback.is_some() {
                bail!("--judge/--fallback belong to --mode cascade");
            }
            let primary = agent_for_opts(&a, &cfg)?;
            Workflow::Critique(CritiqueOpts::resolve(&cfg.critique, &primary, &a.critics)?)
        }
        Mode::Autosteer => {
            if a.judge.is_some() || a.fallback.is_some() {
                bail!("--judge/--fallback belong to --mode cascade");
            }
            let primary = agent_for_opts(&a, &cfg)?;
            Workflow::Autosteer(AutosteerOpts::resolve(
                &cfg.autosteer,
                &primary,
                &a.critics,
                a.policy,
            )?)
        }
    };
    let agent = agent_for_opts(&a, &cfg)?;
    let prompt = read_prompt(&a.prompt, a.prompt_file.as_ref())?;
    execute_run(cfg, cwd, workflow, agent, prompt, None, &a.opts).await
}

/// The primary agent from flags and config (needed before `cmd_run` builds it, for critic defaults).
fn agent_for_opts(a: &RunArgs, cfg: &Config) -> Result<AgentSpec> {
    let mut agent = match &a.agent {
        Some(s) => s.clone(),
        None => configured_agent(cfg)?.context(
            "no agent chosen: pass --agent provider[:model][@effort] (e.g. --agent claude:sonnet), \
             or save a default as `agent` under [defaults] in ~/.jockey/config.toml",
        )?,
    };
    if a.model.is_some() {
        agent.model = a.model.clone();
    }
    if a.effort.is_some() {
        agent.effort = a.effort.clone();
    }
    Ok(agent)
}

/// `defaults.agent` from config, if the user saved one.
fn configured_agent(cfg: &Config) -> Result<Option<AgentSpec>> {
    cfg.defaults.agent.as_deref().map(str::parse).transpose()
}

/// What `execute_run` runs.
enum Workflow {
    Single { resume_id: Option<String> },
    Cascade(CascadeOpts),
    Critique(CritiqueOpts),
    Autosteer(AutosteerOpts),
}

impl Workflow {
    fn mode(&self) -> &'static str {
        match self {
            Workflow::Single { .. } => "single",
            Workflow::Cascade(_) => "cascade",
            Workflow::Critique(_) => "critique",
            Workflow::Autosteer(_) => "autosteer",
        }
    }
}

async fn cmd_resume(id: &str, prompt: Vec<String>, opts: CommonOpts) -> Result<i32> {
    let prev = sessions::read_meta(&sessions::resolve(id)?)?;
    let primary = prev
        .agents
        .iter()
        .find(|a| a.role == "primary")
        .context("saved run has no primary agent")?;
    let sid = primary
        .session_id
        .clone()
        .context("saved run has no provider session id to resume")?;
    let cwd = resolve_cwd(opts.cwd.clone().or(Some(prev.cwd.clone())))?;
    let (cfg, _) = Config::load(&cwd)?;
    if !providers::adapter(&primary.provider, &cfg)?
        .get_capabilities()
        .resume
    {
        bail!("{} cannot resume provider sessions", primary.provider);
    }
    let prompt = if prompt.is_empty() {
        "Continue.".to_string()
    } else {
        read_prompt(&prompt, None)?
    };
    let agent = AgentSpec {
        provider: primary.provider.clone(),
        model: primary.model.clone(),
        effort: primary.effort.clone(),
    };
    // Resuming continues the primary's provider session as a single-agent turn.
    let wf = Workflow::Single {
        resume_id: Some(sid),
    };
    execute_run(cfg, cwd, wf, agent, prompt, Some(prev.run_id), &opts).await
}

#[allow(clippy::too_many_arguments)]
async fn execute_run(
    cfg: Config,
    cwd: PathBuf,
    workflow: Workflow,
    agent: AgentSpec,
    prompt: String,
    resumed_from: Option<String>,
    opts: &CommonOpts,
) -> Result<i32> {
    let mode = workflow.mode();
    let profile = run::resolve_profile(opts.profile, opts.dangerously_allow_all, &cfg)?;
    let timeout = Duration::from_secs(opts.timeout.unwrap_or(cfg.defaults.timeout_secs));
    let approver = Arc::new(RulesApprover {
        allow_commands: cfg.approvals.allow_commands.clone(),
    });
    let mut run = Run::start(RunOptions {
        cfg,
        cwd,
        mode: mode.into(),
        profile,
        allow_credits: opts.allow_credits,
        timeout,
        approver,
        first_prompt: prompt.clone(),
        resumed_from,
    })?;

    let mut sub = run.subscribe();
    let json_out = opts.format == Format::Json;
    let printer = tokio::spawn(async move {
        let mut r = TextRenderer::default();
        loop {
            match sub.recv().await {
                Ok(ev) if json_out => {
                    if !matches!(ev.kind, EventKind::Raw { .. }) {
                        println!("{}", serde_json::to_string(&ev).unwrap_or_default());
                    }
                }
                Ok(ev) => r.render(&ev),
                Err(RecvError::Lagged(n)) => {
                    eprintln!("  [{n} events not displayed; see the saved run]")
                }
                Err(RecvError::Closed) => break,
            }
        }
    });

    run.root
        .emit(EventKind::SessionStarted {
            mode: mode.into(),
            prompt: prompt.clone(),
        })
        .await;
    run.root
        .emit(EventKind::UserPrompt {
            text: prompt.clone(),
        })
        .await;

    let ctx = run.ctx.clone();
    let result = tokio::select! {
        r = async {
            match workflow {
                Workflow::Single { resume_id } => workflows::single::run(&ctx, &agent, &prompt, resume_id).await,
                Workflow::Cascade(o) => workflows::cascade::run(&ctx, &agent, &o, &prompt).await,
                Workflow::Critique(o) => workflows::critique::run(&ctx, &agent, &o, &prompt).await,
                Workflow::Autosteer(o) => workflows::autosteer::run(&ctx, &agent, &o, &prompt).await,
            }
        } => r,
        _ = tokio::signal::ctrl_c() => {
            run.root.emit(EventKind::AgentInterrupted { reason: "cancelled by user (Ctrl-C)".into() }).await;
            Err(anyhow::anyhow!("cancelled by user"))
        }
    };
    drop(ctx);

    let (status, output, error, code) = match &result {
        Ok(w) => {
            run.set_agents(w.agents.clone());
            let (s, c) = match w.outcome.status {
                TurnStatus::Completed => ("completed", 0),
                TurnStatus::Interrupted => ("interrupted", 130),
                TurnStatus::Failed => ("failed", 1),
            };
            (
                s,
                Some(w.outcome.final_text.clone()),
                w.outcome.error.clone(),
                c,
            )
        }
        Err(e) => {
            run.root
                .emit(EventKind::AgentFailed {
                    error: format!("{e:#}"),
                })
                .await;
            let cancelled = e.to_string().contains("cancelled");
            let (s, c) = if cancelled {
                ("interrupted", 130)
            } else {
                ("failed", 1)
            };
            (s, None, Some(format!("{e:#}")), c)
        }
    };
    let run_id = run.id.clone();
    let meta = run.finish(status, output, error).await?;
    let _ = printer.await;

    if json_out {
        println!(
            "{}",
            json!({"type": "result", "run_id": run_id, "status": meta.status,
                   "final_output": meta.final_output, "error": meta.error, "usage": meta.usage})
        );
    } else {
        if let Some(e) = &meta.error {
            eprintln!("error: {e}");
        }
        eprintln!(
            "run {run_id} {} · jockey show {}",
            meta.status,
            &run_id[..13]
        );
    }
    Ok(code)
}

async fn cmd_providers(format: Format, with_models: bool) -> Result<i32> {
    let (cfg, _) = Config::load(&std::env::current_dir()?)?;
    let mut rows = vec![];
    for id in PROVIDERS {
        let (probe, caps, models) = match providers::adapter(id, &cfg) {
            Ok(a) => {
                let models = if with_models && a.get_capabilities().list_models {
                    a.list_models().await.ok()
                } else {
                    None
                };
                (a.probe().await, Some(a.get_capabilities()), models)
            }
            Err(_) => (providers::probe_binary(id, &cfg).await, None, None),
        };
        rows.push((probe, caps, models));
    }
    if format == Format::Json {
        let v: Vec<_> = rows
            .iter()
            .map(|(p, c, m)| json!({"probe": p, "capabilities": c, "models": m}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(0);
    }
    println!(
        "{:<8} {:<36} {:<11} LIVE STEER",
        "PROVIDER", "VERSION", "ADAPTER"
    );
    for (p, c, models) in &rows {
        let steer = match c {
            Some(c) if c.live_steer && c.steer_ack => "yes (acknowledged)",
            Some(c) if c.live_steer => "yes (unacknowledged)",
            Some(_) => "no (checkpoint)",
            None => "-",
        };
        let version = p.version.as_deref().unwrap_or("not installed");
        println!(
            "{:<8} {:<36} {:<11} {steer}",
            p.provider, version, p.adapter
        );
        for n in &p.notes {
            println!("         note: {n}");
        }
        for m in models.iter().flatten() {
            let d = if m.is_default { " (default)" } else { "" };
            println!("         {}{d}  effort: {}", m.id, m.efforts.join(", "));
        }
    }
    Ok(0)
}

async fn cmd_doctor() -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let mut problems = 0;
    println!(
        "config   {} ({})",
        config::global_path().display(),
        exists(&config::global_path())
    );
    println!(
        "project  {} ({})",
        config::project_path(&cwd).display(),
        exists(&config::project_path(&cwd))
    );
    let (cfg, warnings) = match Config::load(&cwd) {
        Ok(c) => c,
        Err(e) => {
            println!("✗ config: {e:#}");
            return Ok(1);
        }
    };
    for w in warnings {
        println!("! {w}");
    }
    match sessions::create_run_dir(".doctor") {
        Ok(d) => {
            let _ = std::fs::remove_dir_all(d);
            println!("✓ runs     {}", sessions::runs_dir().display());
        }
        Err(e) => {
            problems += 1;
            println!("✗ runs     {e:#}");
        }
    }
    for id in PROVIDERS {
        match providers::adapter(id, &cfg) {
            Ok(a) => {
                let p = a.probe().await;
                let Some(v) = p.version else {
                    problems += 1;
                    println!("✗ {id:<7} not installed");
                    continue;
                };
                println!("✓ {id:<7} {v}");
                match a.get_usage().await {
                    Ok(u) => println!("          {}", usage_line(id, &u)),
                    Err(_) if *id == "grok" => {
                        println!(
                            "          quota is checked when a session starts (_x.ai/billing needs a session)"
                        )
                    }
                    Err(e) => println!("          usage unavailable: {e:#}"),
                }
            }
            Err(e) => {
                let p = providers::probe_binary(id, &cfg).await;
                println!(
                    "· {id:<7} {}",
                    p.version.as_deref().unwrap_or("not installed")
                );
                println!("          {e:#}");
            }
        }
    }
    Ok(if problems > 0 { 1 } else { 0 })
}

/// One-line subscription summary from a provider's `get_usage` payload.
fn usage_line(provider: &str, u: &serde_json::Value) -> String {
    match provider {
        "codex" => {
            let rl = &u["rateLimits"];
            format!(
                "plan {} · 5h window {}% used · weekly {}% used · subscription usage {}",
                rl["planType"].as_str().unwrap_or("?"),
                rl["primary"]["usedPercent"],
                rl["secondary"]["usedPercent"],
                if u["ordinaryUsageAllowed"] == json!(false) {
                    "EXHAUSTED (runs will be refused without --allow-credits)"
                } else {
                    "available"
                }
            )
        }
        "claude" => {
            let rl = &u["rate_limits"];
            let paid = rl["extra_usage"]["is_enabled"] == json!(true)
                || rl["spend"]["enabled"] == json!(true);
            format!(
                "plan {} · 5h window {}% used · weekly {}% used · paid extra usage {}",
                u["subscription_type"].as_str().unwrap_or("?"),
                rl["five_hour"]["utilization"],
                rl["seven_day"]["utilization"],
                if paid {
                    "ON (runs will be refused without --allow-credits)"
                } else {
                    "off"
                }
            )
        }
        _ => u.to_string(),
    }
}

fn exists(p: &std::path::Path) -> &'static str {
    if p.exists() { "found" } else { "not present" }
}

fn cmd_history(limit: usize, format: Format) -> Result<i32> {
    let runs = sessions::list_runs(limit);
    if format == Format::Json {
        println!("{}", serde_json::to_string_pretty(&runs)?);
        return Ok(0);
    }
    if runs.is_empty() {
        println!("no saved runs in {}", sessions::runs_dir().display());
    }
    for m in runs {
        let agents: Vec<_> = m.agents.iter().map(|a| a.provider.as_str()).collect();
        let prompt: String = m
            .prompt
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        println!(
            "{}  {:<9} {:<11} {:<10} {}",
            &m.run_id[..13],
            m.mode,
            m.status,
            agents.join(","),
            prompt
        );
    }
    Ok(0)
}

fn cmd_show(id: &str, format: Format) -> Result<i32> {
    let dir = sessions::resolve(id)?;
    let meta = sessions::read_meta(&dir)?;
    let evs = sessions::read_events(&dir)?;
    if format == Format::Json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"meta": meta, "events": evs}))?
        );
        return Ok(0);
    }
    println!("run      {}", meta.run_id);
    println!("mode     {} · status {}", meta.mode, meta.status);
    println!("cwd      {}", meta.cwd.display());
    if let Some(r) = &meta.resumed_from {
        println!("resumed  provider session from run {r}");
    }
    for a in &meta.agents {
        println!(
            "agent    {} {}{}{} session {}",
            a.role,
            a.provider,
            a.model
                .as_deref()
                .map(|m| format!(":{m}"))
                .unwrap_or_default(),
            a.effort
                .as_deref()
                .map(|e| format!("@{e}"))
                .unwrap_or_default(),
            a.session_id.as_deref().unwrap_or("-")
        );
    }
    println!("prompt   {}", meta.prompt.trim());
    println!("files    {}", dir.display());
    println!("---");
    let mut r = TextRenderer::replay();
    for e in &evs {
        r.render(e);
    }
    println!("---");
    if let Some(e) = &meta.error {
        println!("error: {e}");
    }
    if let Some(u) = &meta.usage {
        println!("usage: {u}");
    }
    Ok(0)
}

fn cmd_config(cwd: Option<PathBuf>) -> Result<i32> {
    let cwd = resolve_cwd(cwd)?;
    let (cfg, warnings) = Config::load(&cwd)?;
    println!(
        "# global:  {} ({})",
        config::global_path().display(),
        exists(&config::global_path())
    );
    println!(
        "# project: {} ({})",
        config::project_path(&cwd).display(),
        exists(&config::project_path(&cwd))
    );
    for w in warnings {
        println!("# warning: {w}");
    }
    print!("{}", toml::to_string_pretty(&cfg)?);
    Ok(0)
}
