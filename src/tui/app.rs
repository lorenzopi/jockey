//! TUI state and behavior. No terminal I/O here, so it can be driven by tests.

use super::editor::Editor;
use crate::config::Config;
use crate::events::{Event, EventKind, PlanStep};
use crate::providers::{self, AgentSession, AgentSpec, ModelInfo, Role, TurnOutcome, TurnStatus};
use crate::run::{Run, RunOptions};
use crate::security::{ApprovalRequest, Approver, Decision, Profile, command_allowed};
use crate::sessions::{self, AgentAssignment};
use anyhow::Result;
use async_trait::async_trait;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot};

pub enum Msg {
    Key(KeyEvent),
    /// Terminal size changed: redraw from scratch.
    Resize,
    Quit,
    Paste(String),
    Redraw,
    Bus(Event),
    SessionReady(Arc<dyn AgentSession>, AgentAssignment),
    TurnDone(Result<TurnOutcome>),
    Approval(ApprovalRequest, oneshot::Sender<Reply>),
    Models(String, Vec<ModelInfo>),
    Notice(Level, String),
    /// A newer release exists; shown on the home screen.
    Update(crate::update::Update),
    Shell {
        cmd: String,
        output: String,
        include: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reply {
    Yes,
    Always,
    No,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolState {
    Running,
    Ok,
    Failed(Option<i64>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SteerState {
    Sent,
    Delivered,
    Applied,
    Rejected(String),
}

#[derive(Clone, Debug)]
pub enum Entry {
    User(String),
    Assistant {
        item_id: String,
        text: String,
    },
    Reasoning {
        item_id: String,
        text: String,
    },
    Tool {
        id: String,
        title: String,
        state: ToolState,
        output: String,
        duration: Option<Duration>,
    },
    File {
        path: String,
        change: String,
        diff: String,
    },
    Plan(Vec<PlanStep>),
    Steer {
        id: String,
        text: String,
        state: SteerState,
    },
    Approval {
        kind: String,
        detail: String,
        decision: Option<String>,
    },
    Notice(Level, String),
    /// Marks where a new prompt's reviews start in the supervisor pane.
    Divider(String),
    /// Supervisor feed line (review rounds, escalations).
    Feed(Level, String),
    Decision {
        finding: crate::critics::Finding,
        action: String,
        reason: String,
    },
    Critic {
        critic: String,
        role: String,
        count: usize,
        error: Option<String>,
    },
    Findings {
        round: u32,
        findings: Vec<crate::critics::Finding>,
        duplicates: usize,
    },
    Verdict {
        attempt: u32,
        score: u8,
        accepted: bool,
        issues: Vec<String>,
        evidence: Vec<String>,
    },
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Zoom {
    None,
    Primary,
    Supervisor,
}

#[derive(Clone, Debug)]
pub struct Suggestion {
    pub id: String,
    pub finding: crate::critics::Finding,
    pub text: String,
    pub sent: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FinishKind {
    Won,
    PulledUp,
    Fell,
}

#[derive(Clone, Debug)]
pub struct Finish {
    pub at: Instant,
    pub kind: FinishKind,
    pub summary: String,
}

pub enum Popup {
    Picker(Picker),
    Approval(ApprovalRequest, oneshot::Sender<Reply>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PickKind {
    Mode,
    /// How many critics (critique, autosteer).
    Count,
    Policy,
    /// Autosteer suggestions to send as steers.
    Suggestion,
    /// Which role of a multi-model mode to (re)assign.
    Role,
    Lab,
    Model,
    Effort,
    Profile,
    History,
}

pub struct Picker {
    pub kind: PickKind,
    pub title: String,
    /// Lab › model › thinking context for the step this picker belongs to.
    pub provider: String,
    pub model: Option<String>,
    /// Role being assigned: "primary", "judge", "fallback" or "critics".
    pub role: String,
    /// (value, label)
    pub items: Vec<(String, String)>,
    pub selected: usize,
    pub filter: String,
}

impl Picker {
    pub fn visible(&self) -> Vec<&(String, String)> {
        let f = self.filter.to_lowercase();
        self.items
            .iter()
            .filter(|(_, l)| l.to_lowercase().contains(&f))
            .collect()
    }
}

/// (provider id, lab, product) in picker order.
pub const LABS: &[(&str, &str, &str)] = &[
    ("codex", "OpenAI", "Codex"),
    ("claude", "Anthropic", "Claude Code"),
    ("grok", "xAI", "Grok"),
    ("mock", "Mock", "built-in test agent"),
];

pub fn lab_name(provider: &str) -> &str {
    LABS.iter()
        .find(|(p, ..)| *p == provider)
        .map_or(provider, |(_, lab, _)| lab)
}

pub const COMMANDS: &[(&str, &str)] = &[
    ("/model", "pick lab › model › thinking (Ctrl+L)"),
    ("/effort", "pick reasoning effort (Shift+Tab cycles)"),
    (
        "/agent",
        "switch provider: /agent codex | claude | grok | mock",
    ),
    ("/profile", "permission profile for new sessions"),
    ("/mode", "pick a mode, then set up its models"),
    (
        "/critics",
        "critique critics: /critics provider[:model][@effort] | same",
    ),
    ("/judge", "cascade judge: /judge provider[:model][@effort]"),
    (
        "/fallback",
        "cascade fallback: /fallback provider[:model][@effort] | off",
    ),
    ("/new", "start a new session"),
    ("/history", "browse saved runs and resume one"),
    ("/resume", "resume the last saved run"),
    ("/session", "show run and session details"),
    ("/review", "autosteer: ask the critics to review now"),
    (
        "/suggestions",
        "autosteer: pick a critic suggestion to send (Ctrl+S)",
    ),
    (
        "/policy",
        "autosteer policy: observe | suggest | balanced | aggressive",
    ),
    ("/copy", "copy the last answer (Ctrl+X)"),
    ("/tools", "show or hide tool output and diffs (Ctrl+O)"),
    ("/thinking", "show or hide reasoning summaries (Ctrl+T)"),
    ("/raw", "show raw provider events (Ctrl+R)"),
    ("/help", "keys and commands"),
    ("/quit", "exit"),
];

pub struct Usage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub context_pct: Option<f64>,
    pub limit_pct: Option<f64>,
    pub limit_reached: bool,
}

pub struct App {
    pub cfg: Config,
    pub cwd: PathBuf,
    pub agent: AgentSpec,
    pub profile: Profile,
    pub allow_credits: bool,
    pub tx: mpsc::Sender<Msg>,

    pub editor: Editor,
    pub history: Vec<String>,
    history_pos: Option<usize>,
    pub transcript: Vec<Entry>,
    pub raw: VecDeque<String>,
    pub scroll: usize,
    pub show_tools: bool,
    pub show_reasoning: bool,
    pub show_raw: bool,
    pub popup: Option<Popup>,
    approvals: VecDeque<(ApprovalRequest, oneshot::Sender<Reply>)>,
    pub menu_selected: usize,

    pub run: Option<Run>,
    pub session: Option<Arc<dyn AgentSession>>,
    pub models: Vec<ModelInfo>,
    model_cache: std::collections::HashMap<String, Vec<ModelInfo>>,
    pub busy_since: Option<Instant>,
    pub interrupting: bool,
    pub queue: VecDeque<String>,
    pending_context: Vec<String>,
    pending_resume: Option<(String, String)>,
    last_status: TurnStatus,
    pub last_answer: Option<String>,
    pub usage: Usage,
    /// Versioned model id the running session reported (None: look it up).
    pub model_label: Option<String>,
    /// Effort the session reported when none was chosen (provider default).
    effort_label: Option<String>,
    plan_idx: Option<usize>,
    ctrl_c_at: Option<Instant>,
    pub quit: bool,
    /// Live status for the running turn: (state, detail) as the CLI reports it.
    pub activity: Option<(String, Option<String>)>,
    /// Provider-counted output tokens this turn, plus text streamed since that count.
    pub tokens_reported: u64,
    pub streamed_chars: usize,
    pub thought_tokens: Option<u64>,
    /// Racetrack animation state (interior-mutable: advanced while drawing).
    pub gallop_pos: std::cell::Cell<f32>,
    pub gallop_t: std::cell::Cell<Option<Instant>>,
    pub critic_pos: std::cell::Cell<f32>,
    /// The last turn's finish line, shown briefly after it ends.
    pub finish: Option<Finish>,
    /// Critics currently reviewing (for the second lane).
    pub reviewing: usize,
    tool_started: std::collections::HashMap<String, Instant>,
    /// When each transcript entry appeared, for the ease-in.
    pub born: Vec<Instant>,
    /// "single" or "cascade".
    pub mode: String,
    pub judge: Option<AgentSpec>,
    pub fallback: Option<AgentSpec>,
    /// Critique/autosteer critics' model; None = same as the primary.
    pub critic: Option<AgentSpec>,
    pub policy: crate::steering::Policy,
    /// Autosteer steer suggestions the user can send (Ctrl+S).
    pub suggestions: Vec<Suggestion>,
    /// Split-pane layout overrides: flip orientation, zoom one pane.
    pub split_flip: bool,
    pub zoom: Zoom,
    /// Supervisor pane scroll (lines up from the bottom).
    pub scroll_sup: usize,
    pub update: Option<crate::update::Update>,
    reviews: u32,
    /// No model chosen yet (no flag, no saved default): ask before running anything.
    pub needs_agent: bool,
    /// Critic count chosen in the setup menu (overrides config).
    pub critic_count: Option<usize>,
    /// Running cascade (no single session to interrupt, so it is aborted).
    cascade_task: Option<tokio::task::JoinHandle<()>>,
    pub external_edit: bool,
}

/// Interactive approvals: config rules first, then "always" answers, then ask.
pub struct TuiApprover {
    pub rules: Vec<String>,
    pub always: Mutex<HashSet<String>>,
    pub tx: mpsc::Sender<Msg>,
}

#[async_trait]
impl Approver for TuiApprover {
    async fn decide(&self, req: &ApprovalRequest) -> (Decision, &'static str) {
        if req.kind == "command" && command_allowed(&req.detail, &self.rules) {
            return (Decision::Accept, "rule");
        }
        let key = format!("{}:{}", req.kind, req.detail);
        if self.always.lock().unwrap().contains(&key) {
            return (Decision::Accept, "user (always)");
        }
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Approval(req.clone(), tx)).await.is_err() {
            return (Decision::Decline, "default");
        }
        match rx.await {
            Ok(Reply::Yes) => (Decision::Accept, "user"),
            Ok(Reply::Always) => {
                self.always.lock().unwrap().insert(key);
                (Decision::Accept, "user")
            }
            _ => (Decision::Decline, "user"),
        }
    }
}

impl App {
    pub fn new(
        cfg: Config,
        cwd: PathBuf,
        agent: AgentSpec,
        profile: Profile,
        allow_credits: bool,
        tx: mpsc::Sender<Msg>,
    ) -> Self {
        let cfg_cascade_judge = cfg.cascade.judge.clone();
        let cfg_policy = cfg.autosteer.policy;
        let cfg_cascade_fallback = cfg.cascade.fallback.clone();
        Self {
            cfg,
            cwd,
            agent,
            profile,
            allow_credits,
            tx,
            editor: Editor::default(),
            history: vec![],
            history_pos: None,
            transcript: vec![],
            raw: VecDeque::new(),
            scroll: 0,
            show_tools: false,
            show_reasoning: false,
            show_raw: false,
            popup: None,
            approvals: VecDeque::new(),
            menu_selected: 0,
            run: None,
            session: None,
            models: vec![],
            model_cache: Default::default(),
            busy_since: None,
            interrupting: false,
            queue: VecDeque::new(),
            pending_context: vec![],
            pending_resume: None,
            last_status: TurnStatus::Completed,
            last_answer: None,
            usage: Usage {
                tokens_in: 0,
                tokens_out: 0,
                context_pct: None,
                limit_pct: None,
                limit_reached: false,
            },
            model_label: None,
            effort_label: None,
            plan_idx: None,
            ctrl_c_at: None,
            quit: false,
            external_edit: false,
            activity: None,
            tokens_reported: 0,
            streamed_chars: 0,
            thought_tokens: None,
            gallop_pos: Default::default(),
            gallop_t: Default::default(),
            critic_pos: Default::default(),
            finish: None,
            reviewing: 0,
            tool_started: Default::default(),
            born: vec![],
            mode: "single".into(),
            judge: cfg_spec(&cfg_cascade_judge),
            fallback: cfg_spec(&cfg_cascade_fallback),
            cascade_task: None,
            critic: None,
            policy: cfg_policy,
            suggestions: vec![],
            split_flip: false,
            zoom: Zoom::None,
            scroll_sup: 0,
            update: None,
            reviews: 0,
            critic_count: None,
            needs_agent: false,
        }
    }

    fn begin_busy(&mut self) {
        self.busy_since = Some(Instant::now());
        self.finish = None;
        self.gallop_pos.set(0.0);
        self.gallop_t.set(None);
        self.scroll = 0;
        self.activity = None;
        self.tokens_reported = 0;
        self.streamed_chars = 0;
        self.thought_tokens = None;
    }

    /// Live status line inputs. Provider counts arrive per model call, so text
    /// streamed since the last count is estimated (about 4 chars per token).
    fn track_activity(&mut self, k: &EventKind) {
        use EventKind::*;
        let set = |a: &mut Option<(String, Option<String>)>, s: &str, d: Option<String>| {
            *a = Some((s.into(), d))
        };
        match k {
            Activity { state, detail } => set(&mut self.activity, state, detail.clone()),
            TokensLive {
                output_tokens,
                thinking_tokens,
            } => {
                self.tokens_reported = self.tokens_reported.max(*output_tokens);
                self.streamed_chars = 0;
                if thinking_tokens.is_some() {
                    self.thought_tokens = *thinking_tokens;
                }
            }
            MessageDelta { text, .. } => {
                self.streamed_chars += text.len();
                set(&mut self.activity, "writing", None);
            }
            ReasoningDelta { text, .. } => {
                self.streamed_chars += text.len();
                set(&mut self.activity, "thinking", None);
            }
            ToolStarted { title, .. } => set(&mut self.activity, "tool", Some(title.clone())),
            ReviewTriggered { critics, .. } => self.reviewing = critics.len(),
            CritiqueReceived { .. } => self.reviewing = self.reviewing.saturating_sub(1),
            FindingsMerged { .. } => self.reviewing = 0,
            ToolCompleted { .. } | ApprovalResolved { .. } => {
                set(&mut self.activity, "thinking", None)
            }
            _ => {}
        }
    }

    pub fn busy(&self) -> bool {
        self.busy_since.is_some()
    }

    pub fn notice(&mut self, level: Level, text: impl Into<String>) {
        self.transcript.push(Entry::Notice(level, text.into()));
    }

    pub fn load_models(&self) {
        self.load_models_for(&self.agent.provider.clone());
    }

    fn load_models_for(&self, provider: &str) {
        let Ok(adapter) = providers::adapter(provider, &self.cfg) else {
            return;
        };
        if !adapter.get_capabilities().list_models {
            return;
        }
        let tx = self.tx.clone();
        let provider = provider.to_string();
        tokio::spawn(async move {
            if let Ok(m) = adapter.list_models().await {
                let _ = tx.send(Msg::Models(provider, m)).await;
            }
        });
    }

    /// Slash-menu matches for the current editor text.
    pub fn menu(&self) -> Vec<(String, &'static str)> {
        let t = &self.editor.text;
        if !t.starts_with('/') || t.contains(' ') || t.contains('\n') || self.popup.is_some() {
            return vec![];
        }
        let mut items: Vec<(String, &'static str)> = COMMANDS
            .iter()
            .filter(|(c, _)| c.starts_with(t.as_str()))
            .map(|(c, d)| (c.to_string(), *d))
            .collect();
        if let Ok(a) = providers::adapter(&self.agent.provider, &self.cfg) {
            for n in a.get_capabilities().native_commands {
                let name = format!("/{}:{n}", self.agent.provider);
                if name.starts_with(t.as_str()) {
                    items.push((name, "provider-native command"));
                }
            }
        }
        // An exact match (e.g. `/mode` vs `/model`) is what Enter should run.
        items.sort_by_key(|(c, _)| c != t);
        items
    }

    pub async fn handle(&mut self, msg: Msg) {
        self.handle_inner(msg).await;
        // Stamp new transcript entries so they can ease in.
        let n = self.transcript.len();
        if self.born.len() > n {
            self.born.clear();
        }
        let now = Instant::now();
        self.born.resize(n, now);
    }

    async fn handle_inner(&mut self, msg: Msg) {
        match msg {
            Msg::Key(k) => self.on_key(k).await,
            Msg::Paste(s) => {
                if self.popup.is_none() {
                    self.editor.insert(&s);
                }
            }
            Msg::Redraw | Msg::Resize => {}
            Msg::Quit => self.quit = true,
            Msg::Bus(ev) => self.on_event(ev),
            Msg::SessionReady(s, a) => {
                self.session = Some(s);
                if let Some(run) = &mut self.run {
                    run.set_agents(vec![a]);
                }
            }
            Msg::TurnDone(r) => self.on_turn_done(r).await,
            Msg::Approval(req, tx) => {
                self.activity = Some(("waiting".into(), None));
                self.transcript.push(Entry::Approval {
                    kind: req.kind.clone(),
                    detail: req.detail.clone(),
                    decision: None,
                });
                if self.popup.is_none() {
                    self.popup = Some(Popup::Approval(req, tx));
                } else {
                    self.approvals.push_back((req, tx));
                }
            }
            Msg::Models(provider, m) => {
                if provider == self.agent.provider {
                    self.models = m.clone();
                }
                self.model_cache.insert(provider.clone(), m);
                // A model picker opened before the list arrived fills in now.
                let waiting = match &self.popup {
                    Some(Popup::Picker(p))
                        if p.kind == PickKind::Model
                            && p.provider == provider
                            && p.items.is_empty() =>
                    {
                        Some(p.role.clone())
                    }
                    _ => None,
                };
                if let Some(role) = waiting {
                    self.open_model_picker(&provider, &role);
                } else if matches!(&self.popup, Some(Popup::Picker(p)) if p.kind == PickKind::Role)
                {
                    self.open_role_menu(); // refresh labels with versioned ids
                }
            }
            Msg::Notice(l, t) => self.notice(l, t),
            Msg::Update(u) => self.update = Some(u),
            Msg::Shell {
                cmd,
                output,
                include,
            } => {
                self.notice(Level::Info, format!("$ {cmd}\n{}", output.trim_end()));
                if include {
                    self.pending_context.push(format!(
                        "Output of `{cmd}`:\n```\n{}\n```",
                        output.trim_end()
                    ));
                }
            }
        }
    }

    fn on_event(&mut self, ev: Event) {
        if !matches!(ev.agent.as_str(), "primary" | "fallback" | "harness") {
            return; // reviewers get their own pane in the multi-agent modes
        }
        self.track_activity(&ev.kind);
        use EventKind::*;
        match ev.kind {
            UserPrompt { text } => {
                if self.mode != "single" {
                    self.transcript.push(Entry::Divider(text.clone()));
                }
                self.transcript.push(Entry::User(text));
                self.plan_idx = None;
            }
            AgentStarted { model, effort, .. } => {
                self.model_label = model;
                self.effort_label = effort;
            }
            ModelChanged { model } => self.model_label = Some(model),
            MessageDelta { item_id, text } => {
                // Search a few entries back: a steer or notice can land mid-stream.
                let hit = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .take(12)
                    .find_map(|e| match e {
                        Entry::Assistant {
                            item_id: id,
                            text: t,
                        } if *id == item_id => Some(t),
                        _ => None,
                    });
                match hit {
                    Some(t) => t.push_str(&text),
                    None => self.transcript.push(Entry::Assistant { item_id, text }),
                }
            }
            Message { item_id, text, .. } => {
                let found = self.transcript.iter_mut().rev().any(|e| match e {
                    Entry::Assistant {
                        item_id: id,
                        text: t,
                    } if *id == item_id => {
                        *t = text.clone();
                        true
                    }
                    _ => false,
                });
                if !found {
                    self.transcript.push(Entry::Assistant { item_id, text });
                }
            }
            ReasoningDelta { item_id, text } => match self.transcript.last_mut() {
                Some(Entry::Reasoning {
                    item_id: id,
                    text: t,
                }) if *id == item_id => t.push_str(&text),
                _ => self.transcript.push(Entry::Reasoning { item_id, text }),
            },
            ToolStarted { id, title, .. } => {
                self.tool_started.insert(id.clone(), Instant::now());
                self.transcript.push(Entry::Tool {
                    id,
                    title,
                    state: ToolState::Running,
                    output: String::new(),
                    duration: None,
                })
            }
            ToolOutputDelta { id, text } => {
                if let Some(Entry::Tool { output, .. }) = self.find_tool(&id) {
                    output.push_str(&text);
                }
            }
            ToolCompleted {
                id,
                title,
                ok,
                exit_code,
                output,
                ..
            } => {
                let state = if ok {
                    ToolState::Ok
                } else {
                    ToolState::Failed(exit_code)
                };
                let took = self.tool_started.remove(&id).map(|t| t.elapsed());
                match self.find_tool(&id) {
                    Some(Entry::Tool {
                        state: s,
                        output: o,
                        duration: d,
                        ..
                    }) => {
                        *s = state;
                        *d = took;
                        if let Some(out) = output {
                            *o = out;
                        }
                    }
                    _ => self.transcript.push(Entry::Tool {
                        id,
                        title,
                        state,
                        output: output.unwrap_or_default(),
                        duration: took,
                    }),
                }
            }
            FileChanged { path, change, diff } => {
                self.transcript.push(Entry::File { path, change, diff })
            }
            Plan { steps, .. } => match self.plan_idx {
                Some(i) => self.transcript[i] = Entry::Plan(steps),
                None => {
                    self.plan_idx = Some(self.transcript.len());
                    self.transcript.push(Entry::Plan(steps));
                }
            },
            SteeringSent { steer_id, text } => self.transcript.push(Entry::Steer {
                id: steer_id,
                text,
                state: SteerState::Sent,
            }),
            SteeringDelivered { steer_id } => self.set_steer(&steer_id, SteerState::Delivered),
            SteeringApplied { steer_id } => self.set_steer(&steer_id, SteerState::Applied),
            SteeringRejected { steer_id, reason } => {
                if reason.starts_with(TURN_ENDED) {
                    self.requeue_steer(&steer_id);
                }
                self.set_steer(&steer_id, SteerState::Rejected(reason));
            }
            ApprovalResolved { decision, by, .. } => {
                if let Some(Entry::Approval { decision: d, .. }) = self
                    .transcript
                    .iter_mut()
                    .rev()
                    .find(|e| matches!(e, Entry::Approval { decision: None, .. }))
                {
                    *d = Some(format!("{decision} ({by})"));
                }
            }
            UsageUpdated {
                input_tokens,
                output_tokens,
                context_window,
                ..
            } => {
                self.usage.tokens_in = input_tokens;
                self.usage.tokens_out = output_tokens;
                self.usage.context_pct = context_window
                    .filter(|w| *w > 0)
                    .map(|w| 100.0 * input_tokens as f64 / w as f64);
            }
            RateLimit {
                used_percent,
                limit_reached,
                ..
            } => {
                self.usage.limit_pct = Some(used_percent);
                self.usage.limit_reached = limit_reached;
            }
            Warning { message } => self.notice(Level::Warn, message),
            CheckCompleted {
                command,
                ok,
                exit_code,
                output_tail,
            } => self.transcript.push(Entry::Tool {
                id: format!("check:{command}"),
                title: format!("check: {command}"),
                duration: None,
                state: if ok {
                    ToolState::Ok
                } else {
                    ToolState::Failed(exit_code.map(i64::from))
                },
                output: output_tail,
            }),
            JudgeResult {
                attempt,
                score,
                accepted,
                issues,
                evidence,
                ..
            } => self.transcript.push(Entry::Verdict {
                attempt,
                score,
                accepted,
                issues,
                evidence,
            }),
            ReviewTriggered {
                critics,
                reason,
                models,
                ..
            } => {
                // Numbered across the conversation, not per prompt.
                self.reviews += 1;
                let who = if models.is_empty() {
                    format!("{} critic(s)", critics.len())
                } else {
                    let labels: Vec<String> = models
                        .iter()
                        .map(|m| {
                            m.parse()
                                .map_or_else(|_| m.clone(), |s| self.spec_label(&s))
                        })
                        .collect();
                    crate::render::tally(&labels)
                };
                self.transcript.push(Entry::Feed(
                    Level::Info,
                    format!("review {} ({reason}): {who} on one snapshot", self.reviews),
                ))
            }
            SteeringDecision {
                finding,
                action,
                reason,
                suggestion_id,
                steer_text,
            } => {
                if let (Some(id), Some(text)) = (suggestion_id, steer_text) {
                    self.suggestions.push(Suggestion {
                        id,
                        finding: finding.clone(),
                        text,
                        sent: false,
                    });
                }
                self.transcript.push(Entry::Decision {
                    finding,
                    action,
                    reason,
                });
            }
            CritiqueReceived {
                critic,
                role,
                findings,
                error,
            } => self.transcript.push(Entry::Critic {
                critic,
                role,
                count: findings.len(),
                error,
            }), // Autosteer shows each finding with its decision instead.
            FindingsMerged { .. } if self.mode == "autosteer" => {}
            FindingsMerged {
                round,
                findings,
                duplicates,
            } => self.transcript.push(Entry::Findings {
                round,
                findings,
                duplicates,
            }),
            Escalated { from, to, reason } => self.transcript.push(Entry::Feed(
                Level::Warn,
                format!("↗ escalating {from} → {to}: {reason}"),
            )),
            AgentInterrupted { reason } => {
                self.notice(Level::Warn, format!("interrupted: {reason}"))
            }
            AgentFailed { error } => self.notice(Level::Error, error),
            Raw { value } => {
                self.raw.push_back(value.to_string());
                if self.raw.len() > 2000 {
                    self.raw.pop_front();
                }
            }
            _ => {}
        }
    }

    fn find_tool(&mut self, id: &str) -> Option<&mut Entry> {
        self.transcript
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Tool { id: i, .. } if i == id))
    }

    /// A steer that missed its turn becomes the next prompt instead of vanishing.
    fn requeue_steer(&mut self, id: &str) {
        let text = self.transcript.iter().rev().find_map(|e| match e {
            Entry::Steer { id: i, text, .. } if i == id => Some(text.clone()),
            _ => None,
        });
        let Some(text) = text else { return };
        // Autosteer's own corrections are re-delivered by the workflow as a follow-up;
        // only the user's typed steers are re-sent from here.
        if text.starts_with("Correction from an independent reviewer") {
            return;
        }
        if self.busy() {
            self.queue.push_front(text);
        } else {
            self.notice(
                Level::Info,
                "the correction arrived after the turn ended; sending it as a follow-up",
            );
            self.start_turn(text);
        }
    }

    fn set_steer(&mut self, id: &str, st: SteerState) {
        if let Some(Entry::Steer { state, .. }) = self
            .transcript
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Steer { id: i, .. } if i == id))
        {
            // Never move backwards (an ack can arrive after "applied" on some providers).
            let rank = |s: &SteerState| match s {
                SteerState::Sent => 0,
                SteerState::Delivered => 1,
                SteerState::Applied => 2,
                SteerState::Rejected(_) => 3,
            };
            if rank(&st) > rank(state) {
                *state = st;
            }
        }
    }

    async fn on_turn_done(&mut self, r: Result<TurnOutcome>) {
        self.cascade_task = None;
        // Multi-agent workflows ran on the run's primary; keep our handle in sync.
        if let Some(s) = self.run.as_ref().and_then(|r| r.ctx.session("primary")) {
            self.session = Some(s);
        }
        // The finish line: how the turn ended, how long it took, what it cost.
        if let Some(t) = self.busy_since {
            let tokens = self.tokens_reported + (self.streamed_chars / 4) as u64;
            let mut summary = crate::tui::ui::elapsed(t.elapsed().as_secs());
            if tokens > 0 {
                summary.push_str(&format!(" · ↓ {} tokens", crate::tui::ui::human(tokens)));
            }
            let kind = match &r {
                Ok(o) if o.status == TurnStatus::Completed => FinishKind::Won,
                Ok(o) if o.status == TurnStatus::Interrupted => FinishKind::PulledUp,
                _ => FinishKind::Fell,
            };
            self.finish = Some(Finish {
                at: Instant::now(),
                kind,
                summary,
            });
        }
        self.busy_since = None;
        self.interrupting = false;
        self.reviewing = 0;
        match r {
            Ok(o) => {
                self.last_status = o.status.clone();
                if o.status == TurnStatus::Completed && !o.final_text.is_empty() {
                    self.last_answer = Some(o.final_text);
                }
            }
            Err(e) => {
                self.last_status = TurnStatus::Failed;
                self.notice(Level::Error, format!("{e:#}"));
                if self.session.is_none() {
                    // Session never started: close the empty run.
                    self.finish_run().await;
                }
            }
        }
        if self.quit {
            return;
        }
        if let Some(next) = self.queue.pop_front() {
            self.start_turn(next);
        }
    }

    /// Ensures a run exists and starts a turn (starting the session if needed).
    pub fn start_turn(&mut self, mut prompt: String) {
        if !self.pending_context.is_empty() {
            prompt = format!("{}\n\n{prompt}", self.pending_context.join("\n\n"));
            self.pending_context.clear();
        }
        if self.run.is_none() {
            let approver = Arc::new(TuiApprover {
                rules: self.cfg.approvals.allow_commands.clone(),
                always: Mutex::default(),
                tx: self.tx.clone(),
            });
            let resumed_from = self.pending_resume.as_ref().map(|(run, _)| run.clone());
            let run = match Run::start(RunOptions {
                cfg: self.cfg.clone(),
                cwd: self.cwd.clone(),
                mode: self.mode.clone(),
                profile: self.profile,
                allow_credits: self.allow_credits,
                timeout: Duration::from_secs(self.cfg.defaults.timeout_secs),
                approver,
                first_prompt: prompt.clone(),
                resumed_from,
            }) {
                Ok(r) => r,
                Err(e) => {
                    self.notice(Level::Error, format!("{e:#}"));
                    return;
                }
            };
            forward(run.subscribe(), self.tx.clone());
            let root = run.root.clone();
            let mode = run.meta.mode.clone();
            let first = prompt.clone();
            tokio::spawn(async move {
                root.emit(EventKind::SessionStarted {
                    mode,
                    prompt: first,
                })
                .await;
            });
            self.run = Some(run);
        }
        let run = self.run.as_ref().unwrap();
        let ctx = run.ctx.clone();
        let root = run.root.clone();
        if self.mode != "single" {
            return self.start_multi(ctx, root, prompt);
        }
        let session = self.session.clone();
        let agent = self.agent.clone();
        let resume = self.pending_resume.take().map(|(_, sid)| sid);
        let tx = self.tx.clone();
        self.begin_busy();
        self.scroll = 0;
        tokio::spawn(async move {
            root.emit(EventKind::UserPrompt {
                text: prompt.clone(),
            })
            .await;
            let s = match session {
                Some(s) => s,
                None => match ctx
                    .start_agent(&agent, Role::Primary, "primary", resume)
                    .await
                {
                    Ok((s, a)) => {
                        let _ = tx.send(Msg::SessionReady(s.clone(), a)).await;
                        s
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::TurnDone(Err(e))).await;
                        return;
                    }
                },
            };
            let r = s.execute(&prompt).await;
            let _ = tx.send(Msg::TurnDone(r)).await;
        });
    }

    /// Cascade or critique: one workflow per prompt, fresh sessions each time.
    fn start_multi(
        &mut self,
        ctx: Arc<crate::workflows::RunCtx>,
        root: crate::events::Emitter,
        prompt: String,
    ) {
        use crate::workflows::{cascade, critique};
        enum W {
            Cascade(cascade::CascadeOpts),
            Critique(critique::CritiqueOpts),
            Autosteer(crate::workflows::autosteer::AutosteerOpts),
        }
        let w = if self.mode == "autosteer" {
            let flags = self.critic_flags(self.cfg.autosteer.count);
            crate::workflows::autosteer::AutosteerOpts::resolve(
                &self.cfg.autosteer,
                &self.agent,
                &flags,
                Some(self.policy),
            )
            .map(W::Autosteer)
        } else if self.mode == "cascade" {
            cascade::CascadeOpts::resolve(
                &self.cfg.cascade,
                self.judge.clone(),
                self.fallback.clone(),
            )
            .map(W::Cascade)
        } else {
            let flags = self.critic_flags(self.cfg.critique.count);
            critique::CritiqueOpts::resolve(&self.cfg.critique, &self.agent, &flags)
                .map(W::Critique)
        };
        let w = match w {
            Ok(w) => w,
            Err(e) => return self.notice(Level::Error, format!("{e:#}")),
        };
        let agent = self.agent.clone();
        let tx = self.tx.clone();
        self.begin_busy();
        self.scroll = 0;
        self.cascade_task = Some(tokio::spawn(async move {
            root.emit(EventKind::UserPrompt {
                text: prompt.clone(),
            })
            .await;
            let r = match w {
                W::Cascade(o) => cascade::run(&ctx, &agent, &o, &prompt).await,
                W::Critique(o) => critique::run(&ctx, &agent, &o, &prompt).await,
                W::Autosteer(o) => {
                    crate::workflows::autosteer::run(&ctx, &agent, &o, &prompt).await
                }
            };
            // The primary stays for the next prompt; judges and critics go.
            ctx.shutdown_helpers().await;
            let _ = tx.send(Msg::TurnDone(r.map(|w| w.outcome))).await;
        }));
    }

    pub async fn finish_run(&mut self) {
        // Drop our session handle first: it holds an emitter, and the event
        // log only closes once every emitter is gone.
        self.session = None;
        if let Some(run) = self.run.take() {
            let status = match self.last_status {
                TurnStatus::Completed => "completed",
                TurnStatus::Interrupted => "interrupted",
                TurnStatus::Failed => "failed",
            };
            let _ = run.finish(status, self.last_answer.clone(), None).await;
        }
        self.session = None;
    }

    fn interrupt(&mut self) {
        if let Some(task) = self.cascade_task.take().filter(|t| !t.is_finished()) {
            // Stop the workflow and its reviewers; interrupt (don't kill) the
            // primary so the conversation continues with the next prompt.
            task.abort();
            self.interrupting = true;
            let ctx = self.run.as_ref().map(|r| r.ctx.clone());
            let tx = self.tx.clone();
            tokio::spawn(async move {
                if let Some(ctx) = ctx {
                    if let Some(p) = ctx.session("primary") {
                        let _ = p.interrupt().await;
                    }
                    ctx.shutdown_helpers().await;
                }
                let _ = tx
                    .send(Msg::TurnDone(Ok(TurnOutcome {
                        status: TurnStatus::Interrupted,
                        final_text: String::new(),
                        error: None,
                    })))
                    .await;
            });
            return self.notice(
                Level::Warn,
                "stopped (file changes stay; the conversation continues)",
            );
        }
        if let Some(s) = self.session.clone() {
            self.interrupting = true;
            let tx = self.tx.clone();
            tokio::spawn(async move {
                if let Err(e) = s.interrupt().await {
                    let _ = tx
                        .send(Msg::Notice(Level::Warn, format!("interrupt failed: {e:#}")))
                        .await;
                }
            });
        }
    }

    fn submit(&mut self, text: String, follow_up: bool) {
        if text.trim().is_empty() {
            if self.needs_agent {
                self.open_lab_picker("primary");
            }
            return;
        }
        if self.needs_agent {
            // Keep what they typed; it runs once a model is chosen.
            self.editor.set(&text);
            self.notice(
                Level::Info,
                "choose a model first (lab › model › thinking); your prompt is kept",
            );
            return self.open_lab_picker("primary");
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.history_pos = None;
        if !self.busy() {
            self.start_turn(text);
            return;
        }
        // In autosteer the workflow owns the primary; steer it through the run.
        if self.session.is_none()
            && self.mode == "autosteer"
            && !follow_up
            && let Some(s) = self.run.as_ref().and_then(|r| r.ctx.session("primary"))
        {
            return self.steer_with(s, text);
        }
        let can_steer = self
            .session
            .as_ref()
            .and_then(|s| providers::adapter(s.provider(), &self.cfg).ok())
            .is_some_and(|a| a.get_capabilities().live_steer);
        if follow_up || !can_steer {
            if !follow_up {
                self.notice(
                    Level::Info,
                    "this provider can't take input mid-run; queued as a follow-up",
                );
            }
            self.queue.push_back(text);
            return;
        }
        let s = self.session.clone().unwrap();
        self.steer_with(s, text);
    }

    fn steer_with(&mut self, s: Arc<dyn AgentSession>, text: String) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(e) = s.steer(&text).await {
                let _ = tx
                    .send(Msg::Notice(
                        Level::Warn,
                        format!("steer not delivered: {e:#}"),
                    ))
                    .await;
            }
        });
    }

    async fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);

        if let Some(p) = self.popup.take() {
            self.popup_key(p, k).await;
            return;
        }

        match (k.code, ctrl, alt) {
            (KeyCode::Char('c'), true, _) => {
                if self.busy() {
                    self.interrupt();
                } else if !self.editor.is_empty() {
                    self.editor.take();
                } else if self
                    .ctrl_c_at
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
                {
                    self.quit = true;
                } else {
                    self.ctrl_c_at = Some(Instant::now());
                    self.notice(Level::Info, "press Ctrl+C again to quit");
                }
                return;
            }
            (KeyCode::Char('d'), true, _) if self.editor.is_empty() && !self.busy() => {
                self.quit = true;
                return;
            }
            (KeyCode::Esc, ..) => {
                if self.busy() {
                    self.interrupt();
                } else if self.show_raw {
                    self.show_raw = false;
                } else if self.editor.text.starts_with('/') {
                    self.editor.take(); // close the command menu
                }
                return;
            }
            (KeyCode::Char('l'), true, _) => {
                return if self.mode == "single" {
                    self.open_lab_picker("primary")
                } else {
                    self.open_role_menu()
                };
            }
            (KeyCode::BackTab, ..) => return self.cycle_effort(),
            (KeyCode::Char('o'), true, _) => return self.show_tools = !self.show_tools,
            (KeyCode::Char('s'), true, _) => return self.open_suggestions(),
            (KeyCode::Char('\\') | KeyCode::Char('4'), true, _) => {
                return self.split_flip = !self.split_flip;
            }
            (KeyCode::Char('z'), true, _) => {
                self.zoom = match self.zoom {
                    Zoom::None => Zoom::Primary,
                    Zoom::Primary => Zoom::Supervisor,
                    Zoom::Supervisor => Zoom::None,
                };
                return;
            }
            (KeyCode::Char('t'), true, _) => return self.show_reasoning = !self.show_reasoning,
            (KeyCode::Char('r'), true, _) => return self.show_raw = !self.show_raw,
            (KeyCode::Char('x'), true, _) => return self.copy_last(),
            (KeyCode::Char('g'), true, _) => {
                self.external_edit = true;
                return;
            }
            (KeyCode::PageUp, _, true) => return self.scroll_sup += 10,
            (KeyCode::PageDown, _, true) => {
                return self.scroll_sup = self.scroll_sup.saturating_sub(10);
            }
            (KeyCode::PageUp, ..) => return self.scroll += 10,
            (KeyCode::PageDown, ..) => return self.scroll = self.scroll.saturating_sub(10),
            (KeyCode::Up, _, true) => {
                // Pull queued follow-ups back into the editor.
                if !self.queue.is_empty() {
                    let q: Vec<_> = self.queue.drain(..).collect();
                    self.editor.set(&q.join("\n"));
                }
                return;
            }
            _ => {}
        }

        let menu = self.menu();
        if !menu.is_empty() {
            match k.code {
                KeyCode::Up => {
                    self.menu_selected = self.menu_selected.saturating_sub(1);
                    return;
                }
                KeyCode::Down => {
                    self.menu_selected = (self.menu_selected + 1).min(menu.len() - 1);
                    return;
                }
                KeyCode::Tab => {
                    let (c, _) = &menu[self.menu_selected.min(menu.len() - 1)];
                    self.editor.set(c);
                    self.menu_selected = 0;
                    return;
                }
                KeyCode::Enter if !shift && !alt => {
                    let (c, _) = menu[self.menu_selected.min(menu.len() - 1)].clone();
                    self.editor.take();
                    self.menu_selected = 0;
                    self.command(&c).await;
                    return;
                }
                _ => {}
            }
        }

        match k.code {
            KeyCode::Enter if shift => self.editor.insert("\n"),
            KeyCode::Char('j') if ctrl => self.editor.insert("\n"),
            KeyCode::Enter if self.editor.text.ends_with('\\') && !alt => {
                self.editor.backspace();
                self.editor.insert("\n");
            }
            KeyCode::Enter => {
                let text = self.editor.take();
                self.menu_selected = 0;
                if let Some(cmd) = text.strip_prefix('/') {
                    self.command(&format!("/{cmd}")).await;
                } else if let Some(cmd) = text.strip_prefix('!') {
                    let (cmd, include) = match cmd.strip_prefix('!') {
                        Some(c) => (c.trim().to_string(), false),
                        None => (cmd.trim().to_string(), true),
                    };
                    self.shell(cmd, include);
                } else {
                    self.submit(text, alt);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                self.editor.insert(c.encode_utf8(&mut [0; 4]));
                self.menu_selected = 0;
            }
            KeyCode::Char('a') if ctrl => self.editor.home(),
            KeyCode::Char('e') if ctrl => self.editor.end(),
            KeyCode::Char('k') if ctrl => self.editor.kill_to_end(),
            KeyCode::Char('u') if ctrl => self.editor.kill_to_start(),
            KeyCode::Char('w') if ctrl => self.editor.delete_word_back(),
            KeyCode::Char('b') if ctrl => self.editor.left(),
            KeyCode::Char('f') if ctrl => self.editor.right(),
            KeyCode::Backspace if alt || ctrl => self.editor.delete_word_back(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Left if alt || ctrl => self.editor.word_left(),
            KeyCode::Right if alt || ctrl => self.editor.word_right(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Up if shift => self.scroll += 1,
            KeyCode::Down if shift => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Up => {
                if !self.editor.up() {
                    self.history_prev();
                }
            }
            KeyCode::Down if !self.editor.down() => {
                self.history_next();
            }
            _ => {}
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let i = match self.history_pos {
            Some(i) => i.saturating_sub(1),
            None => self.history.len() - 1,
        };
        self.history_pos = Some(i);
        let h = self.history[i].clone();
        self.editor.set(&h);
    }

    fn history_next(&mut self) {
        match self.history_pos {
            Some(i) if i + 1 < self.history.len() => {
                self.history_pos = Some(i + 1);
                let h = self.history[i + 1].clone();
                self.editor.set(&h);
            }
            Some(_) => {
                self.history_pos = None;
                self.editor.take();
            }
            None => {}
        }
    }

    async fn popup_key(&mut self, p: Popup, k: KeyEvent) {
        match p {
            Popup::Approval(req, tx) => {
                let reply = match k.code {
                    KeyCode::Char('y') | KeyCode::Enter => Some(Reply::Yes),
                    KeyCode::Char('a') => Some(Reply::Always),
                    KeyCode::Char('n') | KeyCode::Esc => Some(Reply::No),
                    _ => None,
                };
                match reply {
                    Some(r) => {
                        let _ = tx.send(r);
                        self.popup = self
                            .approvals
                            .pop_front()
                            .map(|(r, t)| Popup::Approval(r, t));
                    }
                    None => self.popup = Some(Popup::Approval(req, tx)),
                }
            }
            Popup::Picker(mut pk) => match k.code {
                KeyCode::Esc => {}
                KeyCode::Up => {
                    pk.selected = pk.selected.saturating_sub(1);
                    self.popup = Some(Popup::Picker(pk));
                }
                KeyCode::Down => {
                    let n = pk.visible().len();
                    pk.selected = (pk.selected + 1).min(n.saturating_sub(1));
                    self.popup = Some(Popup::Picker(pk));
                }
                KeyCode::Enter => {
                    let v = pk.visible().get(pk.selected).map(|(v, _)| v.clone());
                    if let Some(v) = v {
                        self.picked(&pk, v).await;
                    }
                }
                // Backspace on an empty filter steps back: thinking → model → lab.
                // Backspace on an empty filter steps back: thinking → model → lab → roles.
                KeyCode::Backspace | KeyCode::Left if pk.filter.is_empty() => match pk.kind {
                    PickKind::Effort => self.open_model_picker(&pk.provider, &pk.role),
                    PickKind::Model => self.open_lab_picker(&pk.role),
                    PickKind::Lab if self.mode != "single" => self.open_role_menu(),
                    _ => self.popup = Some(Popup::Picker(pk)),
                },
                KeyCode::Backspace => {
                    pk.filter.pop();
                    pk.selected = 0;
                    self.popup = Some(Popup::Picker(pk));
                }
                KeyCode::Char(c) => {
                    pk.filter.push(c);
                    pk.selected = 0;
                    self.popup = Some(Popup::Picker(pk));
                }
                _ => self.popup = Some(Popup::Picker(pk)),
            },
        }
    }

    fn current_model(&self) -> Option<&ModelInfo> {
        match &self.agent.model {
            Some(id) => self.models.iter().find(|m| &m.id == id),
            None => self.models.iter().find(|m| m.is_default),
        }
    }

    /// The spec a role currently uses (critics fall back to the primary's).
    fn role_spec(&self, role: &str) -> Option<AgentSpec> {
        match role {
            "judge" => self.judge.clone(),
            "fallback" => self.fallback.clone(),
            "critics" => Some(self.critic.clone().unwrap_or_else(|| self.agent.clone())),
            _ => Some(self.agent.clone()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn show_picker(
        &mut self,
        kind: PickKind,
        title: String,
        provider: &str,
        model: Option<String>,
        role: &str,
        items: Vec<(String, String)>,
    ) {
        // Start on the current choice when it's in the list.
        let spec = self.role_spec(role);
        let current = match kind {
            PickKind::Mode => Some(self.mode.clone()),
            PickKind::Lab => spec.map(|s| s.provider),
            PickKind::Model => spec
                .filter(|s| s.provider == provider)
                .and_then(|s| s.model),
            PickKind::Effort => spec
                .filter(|s| s.provider == provider)
                .and_then(|s| s.effort),
            _ => None,
        };
        let selected = current
            .and_then(|c| items.iter().position(|(v, _)| *v == c))
            .unwrap_or(0);
        self.popup = Some(Popup::Picker(Picker {
            kind,
            title,
            provider: provider.into(),
            model,
            role: role.into(),
            items,
            selected,
            filter: String::new(),
        }));
    }

    pub fn open_mode_picker(&mut self) {
        let items = vec![
            (
                "single".to_string(),
                "single     one agent, live steering".to_string(),
            ),
            (
                "cascade".to_string(),
                "cascade    primary → checks → judge → fallback".to_string(),
            ),
            (
                "critique".to_string(),
                "critique   draft → parallel critics → revision".to_string(),
            ),
            (
                "autosteer".to_string(),
                "autosteer  critics watch live; steer or suggest mid-run".to_string(),
            ),
        ];
        let p = self.agent.provider.clone();
        self.show_picker(PickKind::Mode, "mode".into(), &p, None, "primary", items);
    }

    /// Setup menu for a multi-model mode: each role with its current model, then Done.
    pub fn open_role_menu(&mut self) {
        let unset = || "choose…".to_string();
        let primary = if self.needs_agent {
            unset()
        } else {
            self.spec_label(&self.agent)
        };
        let mut items = vec![("primary".to_string(), format!("Primary   {primary}"))];
        match self.mode.as_str() {
            "cascade" => {
                items.push((
                    "judge".into(),
                    format!(
                        "Judge     {}",
                        self.judge
                            .as_ref()
                            .map_or_else(unset, |s| self.spec_label(s))
                    ),
                ));
                // Optional: Enter turns it on (pick a model) or off.
                items.push((
                    "fallback".into(),
                    match &self.fallback {
                        Some(s) => format!("Fallback  {} · Enter to turn off", self.spec_label(s)),
                        None => "Fallback  off · primary retries · Enter to enable".into(),
                    },
                ));
            }
            "critique" => {
                items.push((
                    "critics".into(),
                    format!("Critics   {}", self.critics_label()),
                ));
                items.push((
                    "count".into(),
                    format!("Count     {} critic(s)", self.effective_critic_count()),
                ));
            }
            "autosteer" => {
                items.push((
                    "critics".into(),
                    format!("Critics   {}", self.critics_label()),
                ));
                items.push((
                    "count".into(),
                    format!("Count     {} critic(s)", self.effective_critic_count()),
                ));
                items.push((
                    "policy".into(),
                    format!("Policy    {}", policy_label(self.policy)),
                ));
            }
            _ => {}
        }
        items.push(("done".into(), "✓ Done".into()));
        // Resolve versions for the labels shown here.
        for spec in [&self.judge, &self.fallback, &self.critic]
            .into_iter()
            .flatten()
        {
            if !self.model_cache.contains_key(&spec.provider) {
                self.load_models_for(&spec.provider);
            }
        }
        let title = format!("{} setup · Enter to change a role", self.mode);
        let p = self.agent.provider.clone();
        self.show_picker(PickKind::Role, title, &p, None, "primary", items);
        // Land on the first role that still needs a choice, else Done.
        if let Some(Popup::Picker(pk)) = &mut self.popup {
            pk.selected = pk
                .items
                .iter()
                .position(|(_, l)| l.ends_with("choose…"))
                .unwrap_or(pk.items.len() - 1);
        }
    }

    pub fn open_lab_picker(&mut self, role: &str) {
        let current = self.role_spec(role).map(|s| s.provider);
        let mut items: Vec<(String, String)> = LABS
            .iter()
            .map(|(p, lab, product)| {
                let status = match providers::adapter(p, &self.cfg) {
                    Ok(_) if current.as_deref() == Some(p) => "  • current".to_string(),
                    Ok(_) => String::new(),
                    Err(_) => "  unavailable".to_string(),
                };
                (p.to_string(), format!("{lab:<10} {product}{status}"))
            })
            .collect();
        if role == "critics" {
            let mark = if self.critic.is_none() {
                "  • current"
            } else {
                ""
            };
            items.insert(
                0,
                (
                    "same".into(),
                    format!("Same as primary ({}){mark}", self.spec_label(&self.agent)),
                ),
            );
        }
        let title = if role == "primary" {
            "lab".to_string()
        } else {
            format!("{role} › lab")
        };
        let provider = current.unwrap_or_else(|| self.agent.provider.clone());
        self.show_picker(PickKind::Lab, title, &provider, None, role, items);
        if role == "critics"
            && self.critic.is_none()
            && let Some(Popup::Picker(pk)) = &mut self.popup
        {
            pk.selected = 0;
        }
    }

    fn open_model_picker(&mut self, provider: &str, role: &str) {
        let prefix = if role == "primary" {
            String::new()
        } else {
            format!("{role} › ")
        };
        let title = format!("{prefix}{} › model", lab_name(provider));
        let models = self.model_cache.get(provider).cloned().unwrap_or_default();
        if models.is_empty() {
            self.load_models_for(provider);
            // Empty picker shows "loading"; it refills when Msg::Models arrives.
            self.show_picker(
                PickKind::Model,
                format!("{title} · loading…"),
                provider,
                None,
                role,
                vec![],
            );
            return;
        }
        let items = models
            .iter()
            .map(|m| {
                let mut label = m
                    .display
                    .clone()
                    .filter(|d| d != &m.id)
                    .map_or(m.id.clone(), |d| format!("{d}  ({})", m.id));
                if m.is_default {
                    label.push_str("  default");
                }
                if let Some(n) = &m.note {
                    label.push_str(&format!("  ⚠ {n}"));
                }
                (m.id.clone(), label)
            })
            .collect();
        self.show_picker(PickKind::Model, title, provider, None, role, items);
    }

    fn open_effort_picker(&mut self, provider: &str, model: &str, role: &str) {
        let efforts = self
            .model_cache
            .get(provider)
            .and_then(|ms| ms.iter().find(|m| m.id == model))
            .map(|m| m.efforts.clone())
            .unwrap_or_default();
        if efforts.is_empty() {
            self.notice(
                Level::Info,
                format!("{model} has no thinking levels to choose"),
            );
            return;
        }
        let prefix = if role == "primary" {
            String::new()
        } else {
            format!("{role} › ")
        };
        let title = format!("{prefix}{} › {model} › thinking", lab_name(provider));
        let items = efforts.into_iter().map(|e| (e.clone(), e)).collect();
        self.show_picker(
            PickKind::Effort,
            title,
            provider,
            Some(model.into()),
            role,
            items,
        );
    }

    fn open_picker(&mut self, kind: PickKind) {
        let provider = self.agent.provider.clone();
        let items: Vec<(String, String)> = match kind {
            PickKind::Mode => return self.open_mode_picker(),
            PickKind::Policy => return self.open_policy_picker(),
            PickKind::Suggestion => return self.open_suggestions(),
            PickKind::Count => return self.open_role_menu(),
            PickKind::Role => return self.open_role_menu(),
            PickKind::Lab => return self.open_lab_picker("primary"),
            PickKind::Model => return self.open_model_picker(&provider, "primary"),
            PickKind::Effort => {
                if self.model_cache.get(&provider).is_none_or(|m| m.is_empty()) {
                    self.load_models();
                    return self.notice(Level::Info, "loading models…");
                }
                let model = self
                    .current_model()
                    .map(|m| m.id.clone())
                    .unwrap_or_default();
                return self.open_effort_picker(&provider, &model, "primary");
            }
            PickKind::Profile => ["read-only", "workspace-write", "auto"]
                .iter()
                .map(|p| (p.to_string(), p.to_string()))
                .collect(),
            PickKind::History => sessions::list_runs(50)
                .into_iter()
                .map(|m| {
                    let p: String = m
                        .prompt
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(50)
                        .collect();
                    let agents: Vec<_> = m.agents.iter().map(|a| a.provider.clone()).collect();
                    (
                        m.run_id.clone(),
                        format!(
                            "{}  {:<11} {:<6} {p}",
                            &m.run_id[..13],
                            m.status,
                            agents.join(",")
                        ),
                    )
                })
                .collect(),
        };
        let title = match kind {
            PickKind::Profile => "profile (new sessions)",
            _ => "resume a saved run",
        };
        self.show_picker(kind, title.into(), &provider, None, "primary", items);
    }

    async fn picked(&mut self, pk: &Picker, v: String) {
        let role = pk.role.clone();
        match pk.kind {
            PickKind::Mode => self.set_mode(&v).await,
            PickKind::Role => {
                if v == "done" {
                    if self.mode == "cascade" && self.judge.is_none() {
                        self.notice(Level::Warn, "cascade still needs: judge");
                        return self.open_role_menu();
                    }
                    return self.notice(Level::Info, self.mode_summary());
                }
                if v == "policy" {
                    return self.open_policy_picker();
                }
                if v == "fallback" && self.fallback.take().is_some() {
                    return self.open_role_menu();
                }
                if v == "count" {
                    let items = (1..=4)
                        .map(|n| {
                            (
                                n.to_string(),
                                format!("{n} critic{}", if n == 1 { "" } else { "s" }),
                            )
                        })
                        .collect();
                    let pr = self.agent.provider.clone();
                    self.show_picker(
                        PickKind::Count,
                        format!("{} › critic count", self.mode),
                        &pr,
                        None,
                        "primary",
                        items,
                    );
                    let cur = self.effective_critic_count().to_string();
                    if let Some(Popup::Picker(pk)) = &mut self.popup {
                        pk.selected = pk.items.iter().position(|(v, _)| *v == cur).unwrap_or(0);
                    }
                    return;
                }
                self.open_lab_picker(&v);
            }
            PickKind::Lab => {
                if v == "same" {
                    self.critic = None;
                    return self.open_role_menu();
                }
                if let Err(e) = providers::adapter(&v, &self.cfg) {
                    return self.notice(Level::Warn, format!("{e:#}"));
                }
                self.open_model_picker(&v, &role);
            }
            PickKind::Model => {
                let has_efforts = self
                    .model_cache
                    .get(&pk.provider)
                    .and_then(|ms| ms.iter().find(|m| m.id == v))
                    .is_some_and(|m| !m.efforts.is_empty());
                if has_efforts {
                    self.open_effort_picker(&pk.provider.clone(), &v, &role);
                } else {
                    self.choose(&role, &pk.provider.clone(), Some(v), None)
                        .await;
                }
            }
            PickKind::Effort => {
                self.choose(&role, &pk.provider.clone(), pk.model.clone(), Some(v))
                    .await;
            }
            PickKind::Profile => self.set_profile(&v),
            PickKind::History => self.resume(&v),
            PickKind::Policy => {
                if let Ok(p) = <crate::steering::Policy as clap::ValueEnum>::from_str(&v, true) {
                    self.policy = p;
                }
                if self.mode == "autosteer" {
                    self.open_role_menu();
                } else {
                    self.notice(
                        Level::Info,
                        format!("autosteer policy: {}", self.policy.as_str()),
                    );
                }
            }
            PickKind::Suggestion => self.send_suggestion(&v),
            PickKind::Count => {
                self.critic_count = v.parse().ok();
                self.open_role_menu();
            }
        }
    }

    fn open_policy_picker(&mut self) {
        use crate::steering::Policy::*;
        let items = [Observe, Suggest, Balanced, Aggressive]
            .iter()
            .map(|p| (p.as_str().to_string(), policy_label(*p)))
            .collect();
        let pr = self.agent.provider.clone();
        self.show_picker(
            PickKind::Policy,
            "autosteer › policy".into(),
            &pr,
            None,
            "primary",
            items,
        );
        let current = self.policy.as_str();
        if let Some(Popup::Picker(pk)) = &mut self.popup {
            pk.selected = pk.items.iter().position(|(v, _)| v == current).unwrap_or(0);
        }
    }

    pub fn open_suggestions(&mut self) {
        let items: Vec<(String, String)> = self
            .suggestions
            .iter()
            .rev()
            .filter(|s| !s.sent)
            .map(|s| {
                let f = &s.finding;
                let loc = f
                    .file
                    .as_deref()
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default();
                (
                    s.id.clone(),
                    format!(
                        "[{} {:.2}] {}{loc}",
                        f.severity.as_str(),
                        f.confidence,
                        f.description
                    ),
                )
            })
            .collect();
        if items.is_empty() {
            return self.notice(Level::Info, "no steer suggestions yet");
        }
        let title = if self.busy() {
            "steer suggestions · Enter sends to the running agent"
        } else {
            "steer suggestions · Enter puts it in the prompt box"
        };
        let pr = self.agent.provider.clone();
        self.show_picker(
            PickKind::Suggestion,
            title.into(),
            &pr,
            None,
            "primary",
            items,
        );
    }

    pub fn open_suggestion_count(&self) -> usize {
        self.suggestions.iter().filter(|s| !s.sent).count()
    }

    /// Sends a critic's suggestion as a live steer, or stages it as the next prompt.
    fn send_suggestion(&mut self, id: &str) {
        let Some(i) = self.suggestions.iter().position(|s| s.id == id) else {
            return;
        };
        let text = self.suggestions[i].text.clone();
        self.suggestions[i].sent = true;
        let session = self
            .session
            .clone()
            .or_else(|| self.run.as_ref().and_then(|r| r.ctx.session("primary")));
        match session.filter(|_| self.busy()) {
            Some(s) => self.steer_with(s, text),
            None => {
                self.editor.set(&text);
                self.notice(
                    Level::Info,
                    "suggestion placed in the prompt box; edit it or press Enter to send",
                );
            }
        }
    }

    /// Switching into a multi-model mode opens its setup menu.
    pub async fn set_mode(&mut self, m: &str) {
        if !matches!(m, "single" | "cascade" | "critique" | "autosteer") {
            return self.notice(
                Level::Warn,
                format!("{m} mode isn't built yet (single, cascade, critique)"),
            );
        }
        if self.busy() {
            return self.notice(Level::Warn, "busy; press Esc first");
        }
        // Same run, same primary conversation: only how each prompt is handled changes.
        self.mode = m.into();
        if m == "single" {
            self.notice(Level::Info, "mode: single");
        } else {
            self.open_role_menu();
        }
    }

    fn set_profile(&mut self, v: &str) {
        if let Ok(p) = <Profile as clap::ValueEnum>::from_str(v, true) {
            self.profile = p;
            let note = if self.session.is_some() {
                " (applies after /new)"
            } else {
                ""
            };
            self.notice(Level::Info, format!("profile: {v}{note}"));
        }
    }

    /// Final step of role › lab › model › thinking. The primary: same provider is
    /// applied to the next turn when possible, a different one starts a new session.
    /// Other roles just record the choice. Multi-model modes return to the setup menu.
    async fn choose(
        &mut self,
        role: &str,
        provider: &str,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let spec = AgentSpec {
            provider: provider.into(),
            model,
            effort,
        };
        // Refuse here rather than when the role first runs (a critic would fail mid-review).
        let bills = self
            .model_cache
            .get(provider)
            .and_then(|ms| ms.iter().find(|m| spec.model.as_ref() == Some(&m.id)))
            .is_some_and(|m| m.bills_credits);
        if bills && !(self.allow_credits || self.cfg.provider(provider).allow_credits) {
            let label = self.spec_label(&spec);
            self.notice(
                Level::Error,
                format!("{label} can bill usage credits; restart with --allow-credits to use it"),
            );
            if self.mode != "single" {
                self.open_role_menu();
            }
            return;
        }
        match role {
            "judge" => self.judge = Some(spec),
            "fallback" => self.fallback = Some(spec),
            "critics" => self.critic = Some(spec),
            _ => self.choose_primary(spec).await,
        }
        if self.mode != "single" {
            self.open_role_menu();
        }
    }

    async fn choose_primary(&mut self, spec: AgentSpec) {
        if self.needs_agent {
            self.needs_agent = false;
            self.models = self
                .model_cache
                .get(&spec.provider)
                .cloned()
                .unwrap_or_default();
            self.agent = spec;
            let label = self.spec_label(&self.agent.clone());
            return self.notice(
                Level::Info,
                format!("using {label}, saved as your default model"),
            );
        }
        if spec.provider == self.agent.provider {
            self.agent.model = spec.model;
            self.agent.effort = spec.effort;
            return self.apply_turn_options();
        }
        if self.busy() {
            return self.notice(Level::Warn, "busy; press Esc before switching labs");
        }
        self.finish_run().await;
        self.models = self
            .model_cache
            .get(&spec.provider)
            .cloned()
            .unwrap_or_default();
        self.agent = spec;
        self.model_label = None;
        self.effort_label = None;
        self.usage.limit_pct = None;
        self.usage.limit_reached = false;
        let label = self.spec_label(&self.agent.clone());
        self.notice(
            Level::Info,
            format!(
                "switched to {} {label} (new session)",
                lab_name(&self.agent.provider)
            ),
        );
    }

    fn apply_turn_options(&mut self) {
        self.model_label = None; // footer looks up the new choice's versioned id
        let label = self.agent_label();
        let Some(s) = self.session.clone() else {
            return;
        };
        let (m, e, tx) = (
            self.agent.model.clone(),
            self.agent.effort.clone(),
            self.tx.clone(),
        );
        tokio::spawn(async move {
            let msg = match s.set_turn_options(m, e).await {
                Ok(()) => (Level::Info, format!("next turns use {label}")),
                Err(err) => (Level::Warn, format!("{err:#}; use /new to apply {label}")),
            };
            let _ = tx.send(Msg::Notice(msg.0, msg.1)).await;
        });
    }

    fn cycle_effort(&mut self) {
        let efforts = self
            .current_model()
            .map(|m| m.efforts.clone())
            .unwrap_or_default();
        if efforts.is_empty() {
            if self.models.is_empty() {
                self.load_models();
            }
            return;
        }
        let next = match self
            .agent
            .effort
            .as_ref()
            .and_then(|e| efforts.iter().position(|x| x == e))
        {
            Some(i) => efforts[(i + 1) % efforts.len()].clone(),
            None => efforts[0].clone(),
        };
        self.agent.effort = Some(next);
        self.apply_turn_options();
    }

    fn copy_last(&mut self) {
        let Some(text) = self.last_answer.clone() else {
            self.notice(Level::Info, "nothing to copy yet");
            return;
        };
        // MacOS pbcopy only; add wl-copy/xclip/clip.exe with the Linux/Windows builds.
        let ok = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(text.as_bytes())?;
                c.wait()
            })
            .is_ok_and(|s| s.success());
        self.notice(
            Level::Info,
            if ok {
                "copied last answer"
            } else {
                "copy failed (pbcopy)"
            },
        );
    }

    fn shell(&mut self, cmd: String, include: bool) {
        if cmd.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        let cwd = self.cwd.clone();
        tokio::spawn(async move {
            let out = tokio::time::timeout(
                Duration::from_secs(60),
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .current_dir(cwd)
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            let output = match out {
                Ok(Ok(o)) => format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
                Ok(Err(e)) => format!("failed: {e}"),
                Err(_) => "timed out after 60s".into(),
            };
            let _ = tx
                .send(Msg::Shell {
                    cmd,
                    output,
                    include,
                })
                .await;
        });
    }

    fn resume(&mut self, id: &str) {
        if self.busy() {
            self.notice(
                Level::Warn,
                "busy; press Esc to stop the current turn first",
            );
            return;
        }
        let loaded = sessions::resolve(id)
            .and_then(|d| Ok((sessions::read_meta(&d)?, sessions::read_events(&d)?)));
        let (meta, events) = match loaded {
            Ok(x) => x,
            Err(e) => return self.notice(Level::Error, format!("{e:#}")),
        };
        let Some(p) = meta.agents.iter().find(|a| a.role == "primary") else {
            return self.notice(Level::Error, "that run has no primary agent");
        };
        let resumable = providers::adapter(&p.provider, &self.cfg)
            .map(|a| a.get_capabilities().resume)
            .unwrap_or(false);
        let Some(sid) = p.session_id.clone().filter(|_| resumable) else {
            return self.notice(
                Level::Error,
                format!("{} sessions can't be resumed", p.provider),
            );
        };
        self.agent = AgentSpec {
            provider: p.provider.clone(),
            model: p.model.clone(),
            effort: p.effort.clone(),
        };
        self.run = None;
        self.session = None;
        self.transcript.clear();
        for e in events {
            self.on_event(e);
        }
        self.pending_resume = Some((meta.run_id.clone(), sid));
        self.notice(
            Level::Info,
            format!(
                "resuming {} session from run {}; your next prompt continues it",
                p.provider,
                &meta.run_id[..13]
            ),
        );
        self.load_models();
    }

    pub async fn command(&mut self, line: &str) {
        let mut parts = line.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().map(str::trim).filter(|a| !a.is_empty());
        if let Some(native) = cmd
            .strip_prefix('/')
            .and_then(|c| c.strip_prefix(&format!("{}:", self.agent.provider)))
        {
            return self.native(native.to_string());
        }
        match cmd {
            "/model" => match arg {
                Some(m) => {
                    let provider = self.agent.provider.clone();
                    let effort = self.agent.effort.clone();
                    self.choose("primary", &provider, Some(m.into()), effort)
                        .await;
                }
                None if self.mode == "single" => self.open_lab_picker("primary"),
                None => self.open_role_menu(),
            },
            "/effort" => match arg {
                Some(e) => {
                    let provider = self.agent.provider.clone();
                    let model = self.agent.model.clone();
                    self.choose("primary", &provider, model, Some(e.into()))
                        .await;
                }
                None => self.open_picker(PickKind::Effort),
            },
            "/profile" => match arg {
                Some("full") => self.notice(
                    Level::Warn,
                    "the full profile needs `jockey --dangerously-allow-all` on the command line",
                ),
                Some(p) => self.set_profile(p),
                None => self.open_picker(PickKind::Profile),
            },
            "/agent" => match arg {
                Some(a) => match a.parse::<AgentSpec>().and_then(|s| {
                    providers::adapter(&s.provider, &self.cfg)?;
                    Ok(s)
                }) {
                    Ok(spec) => {
                        if self.busy() {
                            return self.notice(Level::Warn, "busy; press Esc first");
                        }
                        self.finish_run().await;
                        self.agent = spec;
                        self.models.clear();
                        self.load_models();
                        self.model_label = None;
                        self.notice(Level::Info, format!("agent: {a} (new session)"));
                    }
                    Err(e) => self.notice(Level::Error, format!("{e:#}")),
                },
                None => self.notice(
                    Level::Info,
                    format!(
                        "agent: {} · try /agent codex or /agent mock",
                        self.agent.provider
                    ),
                ),
            },
            "/mode" => match arg {
                Some(m) => self.set_mode(m).await,
                None => self.open_mode_picker(),
            },
            "/critics" => match arg {
                Some("same") => {
                    self.critic = None;
                    self.notice(Level::Info, self.mode_summary());
                }
                Some(a) => match a.parse::<AgentSpec>().and_then(|s| {
                    providers::adapter(&s.provider, &self.cfg)?;
                    Ok(s)
                }) {
                    Ok(spec) => {
                        self.critic = Some(spec);
                        self.notice(Level::Info, self.mode_summary());
                    }
                    Err(e) => self.notice(Level::Error, format!("{e:#}")),
                },
                None => self.open_lab_picker("critics"),
            },
            "/fallback" if arg == Some("off") => {
                self.fallback = None;
                self.notice(Level::Info, self.mode_summary());
            }
            "/judge" | "/fallback" => match arg.map(|a| a.parse::<AgentSpec>()) {
                Some(Ok(spec)) => {
                    if let Err(e) = providers::adapter(&spec.provider, &self.cfg) {
                        return self.notice(Level::Error, format!("{e:#}"));
                    }
                    if cmd == "/judge" {
                        self.judge = Some(spec);
                    } else {
                        self.fallback = Some(spec);
                    }
                    self.notice(Level::Info, self.mode_summary());
                }
                Some(Err(e)) => self.notice(Level::Error, format!("{e:#}")),
                None => self.open_lab_picker(&cmd[1..]),
            },
            "/new" => {
                if self.busy() {
                    return self.notice(Level::Warn, "busy; press Esc first");
                }
                self.finish_run().await;
                self.transcript.clear();
                self.raw.clear();
                self.queue.clear();
                self.pending_resume = None;
                self.usage = Usage {
                    tokens_in: 0,
                    tokens_out: 0,
                    context_pct: None,
                    limit_pct: self.usage.limit_pct,
                    limit_reached: self.usage.limit_reached,
                };
            }
            "/history" => self.open_picker(PickKind::History),
            "/resume" => self.resume(arg.unwrap_or("last")),
            "/session" => {
                let text = match &self.run {
                    Some(r) => format!(
                        "run {}\nfiles {}\nsession {}\ntokens in {} · out {}",
                        r.id,
                        r.dir.display(),
                        self.session
                            .as_ref()
                            .and_then(|s| s.session_id())
                            .unwrap_or("-".into()),
                        self.usage.tokens_in,
                        self.usage.tokens_out
                    ),
                    None => "no active run".into(),
                };
                self.notice(Level::Info, text);
            }
            "/review" => match self
                .run
                .as_ref()
                .filter(|_| self.mode == "autosteer" && self.busy())
            {
                Some(r) => {
                    r.ctx.review_now.notify_one();
                    self.notice(Level::Info, "review requested");
                }
                None => self.notice(
                    Level::Info,
                    "/review asks autosteer's critics to look now; start an autosteer run first",
                ),
            },
            "/suggestions" => self.open_suggestions(),
            "/policy" => match arg {
                Some(p) => match <crate::steering::Policy as clap::ValueEnum>::from_str(p, true) {
                    Ok(p) => {
                        self.policy = p;
                        self.notice(Level::Info, self.mode_summary());
                    }
                    Err(e) => self.notice(Level::Error, e),
                },
                None => self.open_policy_picker(),
            },
            "/copy" => self.copy_last(),
            "/tools" => self.show_tools = !self.show_tools,
            "/thinking" => self.show_reasoning = !self.show_reasoning,
            "/raw" => self.show_raw = !self.show_raw,
            "/help" => self.notice(Level::Info, HELP),
            "/quit" | "/exit" => self.quit = true,
            other => self.notice(
                Level::Warn,
                format!("unknown command {other}; /help lists them"),
            ),
        }
    }

    /// Versioned model id: what the session reported, else the CLI's model list.
    pub fn model_version(&self) -> String {
        if let Some(m) = &self.model_label {
            return m.clone();
        }
        let info = match &self.agent.model {
            Some(id) => self.models.iter().find(|m| &m.id == id),
            None => self.models.iter().find(|m| m.is_default),
        };
        info.map(|m| m.resolved.clone().unwrap_or_else(|| m.id.clone()))
            .or_else(|| self.agent.model.clone())
            .unwrap_or_else(|| "default".into())
    }

    /// e.g. `claude-haiku-5-5@low`, `codex:gpt-6.1-sol@high`.
    pub fn agent_label(&self) -> String {
        if self.needs_agent {
            return "no model selected (Ctrl+L)".into();
        }
        let v = self.model_version();
        let base = if v.starts_with(&self.agent.provider) {
            v
        } else {
            format!("{}:{v}", self.agent.provider)
        };
        match self.agent.effort.as_ref().or(self.effort_label.as_ref()) {
            Some(e) => format!("{base}@{e}"),
            None => base,
        }
    }

    /// `provider:model@effort` with the versioned id when the model list is known.
    /// Model choices saved to the global config when they change:
    /// `defaults.agent`, `cascade.judge`, `cascade.fallback`.
    pub fn saved_models(&self) -> [(&'static str, &'static str, Option<String>); 3] {
        let primary = (!self.needs_agent).then(|| self.agent.to_string());
        let s = |o: &Option<AgentSpec>| o.as_ref().map(|s| s.to_string());
        [
            ("defaults", "agent", primary),
            ("cascade", "judge", s(&self.judge)),
            ("cascade", "fallback", s(&self.fallback)),
        ]
    }

    pub fn spec_label(&self, s: &AgentSpec) -> String {
        let info = self
            .model_cache
            .get(&s.provider)
            .and_then(|ms| match &s.model {
                Some(id) => ms.iter().find(|m| &m.id == id),
                None => ms.iter().find(|m| m.is_default),
            });
        let v = info
            .map(|m| m.resolved.clone().unwrap_or_else(|| m.id.clone()))
            .or_else(|| s.model.clone())
            .unwrap_or_else(|| "default".into());
        let base = if v.starts_with(&s.provider) {
            v
        } else {
            format!("{}:{v}", s.provider)
        };
        match &s.effort {
            Some(e) => format!("{base}@{e}"),
            None => base,
        }
    }

    /// Explicit critic specs from the setup menu; empty = config/default.
    pub fn critic_flags(&self, cfg_count: usize) -> Vec<AgentSpec> {
        let n = self.critic_count.unwrap_or(cfg_count).max(1);
        match (&self.critic, self.critic_count) {
            (Some(c), _) => vec![c.clone(); n],
            (None, Some(_)) => vec![self.agent.clone(); n],
            (None, None) => vec![],
        }
    }

    pub fn critics_label(&self) -> String {
        let (cfg_n, configured) = if self.mode == "autosteer" {
            (
                self.cfg.autosteer.count.max(1),
                self.cfg.autosteer.critics.len(),
            )
        } else {
            (
                self.cfg.critique.count.max(1),
                self.cfg.critique.critics.len(),
            )
        };
        let n = self.critic_count.unwrap_or(cfg_n);
        match &self.critic {
            Some(c) => format!("{n}× {}", self.spec_label(c)),
            None if configured > 0 && self.critic_count.is_none() => {
                format!("{configured} from config")
            }
            None => format!("{n}× same as primary"),
        }
    }

    fn effective_critic_count(&self) -> usize {
        let cfg = if self.mode == "autosteer" {
            &self.cfg.autosteer.count
        } else {
            &self.cfg.critique.count
        };
        self.critic_count.unwrap_or(*cfg).max(1)
    }

    /// Short mode description for the footer.
    pub fn mode_badge(&self) -> String {
        let label =
            |s: &Option<AgentSpec>| s.as_ref().map_or("unset".into(), |s| self.spec_label(s));
        match self.mode.as_str() {
            "cascade" => match &self.fallback {
                Some(f) => format!(
                    "cascade (judge {}, fallback {})",
                    label(&self.judge),
                    self.spec_label(f)
                ),
                None => format!("cascade (judge {})", label(&self.judge)),
            },
            "critique" => format!("critique ({})", self.critics_label()),
            "autosteer" => format!(
                "autosteer ({} · {})",
                self.policy.as_str(),
                self.critics_label()
            ),
            m => m.to_string(),
        }
    }

    pub fn mode_summary(&self) -> String {
        let label =
            |s: &Option<AgentSpec>| s.as_ref().map_or("unset".into(), |s| self.spec_label(s));
        let checks = |c: &[String]| {
            if c.is_empty() {
                "none".to_string()
            } else {
                c.join(", ")
            }
        };
        match self.mode.as_str() {
            "cascade" => format!(
                "mode: cascade · primary {} · judge {} · fallback {} · threshold {} · max {} attempts · checks: {}",
                self.agent_label(),
                label(&self.judge),
                self.fallback
                    .as_ref()
                    .map_or("off".into(), |s| self.spec_label(s)),
                self.cfg.cascade.threshold,
                self.cfg.cascade.max_attempts,
                checks(&self.cfg.cascade.checks)
            ),
            "critique" => format!(
                "mode: critique · primary {} · critics {} · {} revision round(s) · checks: {}",
                self.agent_label(),
                self.critics_label(),
                self.cfg.critique.revisions,
                checks(&self.cfg.critique.checks)
            ),
            "autosteer" => format!(
                "mode: autosteer · primary {} · critics {} · policy {} · Enter steers · Ctrl+S suggestions · /review",
                self.agent_label(),
                self.critics_label(),
                self.policy.as_str()
            ),
            m => format!("mode: {m}"),
        }
    }

    fn native(&mut self, name: String) {
        if self.busy() {
            return self.notice(Level::Warn, "busy; press Esc first");
        }
        let Some(s) = self.session.clone() else {
            return self.notice(Level::Info, "send a prompt first to start a session");
        };
        self.begin_busy();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = s.native_command(&name).await;
            let done = match r {
                Ok(Some(o)) => Ok(o),
                Ok(None) => {
                    let _ = tx
                        .send(Msg::Notice(Level::Info, format!("{name} started")))
                        .await;
                    Ok(TurnOutcome {
                        status: TurnStatus::Completed,
                        final_text: String::new(),
                        error: None,
                    })
                }
                Err(e) => Err(e),
            };
            let _ = tx.send(Msg::TurnDone(done)).await;
        });
    }
}

/// Reason prefix adapters use for an acknowledged steer that never got applied.
pub const TURN_ENDED: &str = crate::providers::STEER_MISSED_TURN;
pub fn policy_label(p: crate::steering::Policy) -> String {
    use crate::steering::Policy::*;
    let d = match p {
        Observe => "record findings only",
        Suggest => "manual: critics suggest, you choose what to send (Ctrl+S)",
        Balanced => "steer strong, well-evidenced findings; suggest the rest",
        Aggressive => "steer readily; critical findings ask for a replan",
    };
    format!("{:<10} {d}", p.as_str())
}

fn cfg_spec(s: &Option<String>) -> Option<AgentSpec> {
    s.as_deref().and_then(|s| s.parse().ok())
}

pub const HELP: &str = "\
Enter send · Shift+Enter, Ctrl+J or \\+Enter newline · / commands · !cmd run shell (output added to next prompt; !! to not add)
While working: Enter steers the agent now · Alt+Enter queues a follow-up · Alt+Up edits the queue · Esc interrupts
Ctrl+L model · Shift+Tab effort · Ctrl+O tool output · Ctrl+T reasoning · Ctrl+R raw events · Ctrl+X copy answer
Ctrl+G external editor · PgUp/PgDn or Shift+Up/Down scroll · Ctrl+C interrupt/clear/quit · Ctrl+D quit";

/// Forwards a run's events into the app loop until the run closes.
fn forward(mut rx: broadcast::Receiver<Event>, tx: mpsc::Sender<Msg>) {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if tx.send(Msg::Bus(ev)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let _ = tx
                        .send(Msg::Notice(
                            Level::Warn,
                            format!("{n} events not displayed (saved in the run log)"),
                        ))
                        .await;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}
