//! Provider-independent event model and the central event bus.
//!
//! Every adapter emits `EventKind`s through an `Emitter`. A single recorder
//! task owns the receiving end: it stamps the sequence number, persists the
//! event, then fans it out to live subscribers (printer, TUI, autosteer).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub run_id: String,
    pub seq: u64,
    pub ts_ms: u64,
    pub provider: String,
    pub agent: String,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EventKind {
    SessionStarted {
        mode: String,
        prompt: String,
    },
    /// A prompt sent to the agent (each turn, including the first).
    UserPrompt {
        text: String,
    },
    AgentStarted {
        role: String,
        session_id: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    },
    /// What the agent is doing right now, as the CLI reports it
    /// ("thinking", "writing", "tool", "compacting", "retrying").
    Activity {
        state: String,
        detail: Option<String>,
    },
    /// Provider-counted tokens for the current turn so far.
    TokensLive {
        output_tokens: u64,
        thinking_tokens: Option<u64>,
    },
    /// The provider reports a different (usually versioned) model than before.
    ModelChanged {
        model: String,
    },
    MessageDelta {
        item_id: String,
        text: String,
    },
    Message {
        item_id: String,
        text: String,
        phase: Option<String>,
    },
    ReasoningDelta {
        item_id: String,
        text: String,
    },
    ToolStarted {
        id: String,
        kind: String,
        title: String,
    },
    ToolOutputDelta {
        id: String,
        text: String,
    },
    ToolCompleted {
        id: String,
        kind: String,
        title: String,
        ok: bool,
        exit_code: Option<i64>,
        output: Option<String>,
    },
    FileChanged {
        path: String,
        change: String,
        diff: String,
    },
    Plan {
        steps: Vec<PlanStep>,
        explanation: Option<String>,
    },
    Diff {
        unified: String,
    },
    UsageUpdated {
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        context_window: Option<u64>,
    },
    RateLimit {
        used_percent: f64,
        window_mins: Option<u64>,
        resets_at: Option<i64>,
        credits_balance: Option<String>,
        limit_reached: bool,
    },
    ApprovalRequested {
        id: String,
        kind: String,
        detail: String,
    },
    ApprovalResolved {
        id: String,
        decision: String,
        by: String,
    },
    /// Sent to the provider, no acknowledgement yet.
    SteeringSent {
        steer_id: String,
        text: String,
    },
    /// The provider acknowledged the instruction (e.g. Codex `turn/steer` response).
    SteeringDelivered {
        steer_id: String,
    },
    /// The instruction showed up in the agent's transcript.
    SteeringApplied {
        steer_id: String,
    },
    SteeringRejected {
        steer_id: String,
        reason: String,
    },
    AgentInterrupted {
        reason: String,
    },
    AgentCompleted {
        status: String,
        final_text: String,
    },
    AgentFailed {
        error: String,
    },
    Warning {
        message: String,
    },
    /// A deterministic check (cascade `checks`) finished.
    CheckCompleted {
        command: String,
        ok: bool,
        exit_code: Option<i32>,
        output_tail: String,
    },
    /// Structured judge verdict for one attempt.
    JudgeResult {
        attempt: u32,
        pass: bool,
        score: u8,
        accepted: bool,
        issues: Vec<String>,
        evidence: Vec<String>,
        recommended_action: String,
    },
    /// Critics were asked to review a snapshot.
    ReviewTriggered {
        round: u32,
        reason: String,
        critics: Vec<String>,
        /// `provider[:model][@effort]` per critic, same order as `critics`.
        #[serde(default)]
        models: Vec<String>,
    },
    /// One critic's findings (or why it produced none).
    CritiqueReceived {
        critic: String,
        role: String,
        findings: Vec<crate::critics::Finding>,
        error: Option<String>,
    },
    /// Deduplicated, ranked findings from one review round.
    FindingsMerged {
        round: u32,
        findings: Vec<crate::critics::Finding>,
        duplicates: usize,
    },
    /// What the steering controller did with one finding.
    /// `suggestion_id` is set for "suggest": the user can send it as a steer.
    SteeringDecision {
        finding: crate::critics::Finding,
        action: String,
        reason: String,
        suggestion_id: Option<String>,
        steer_text: Option<String>,
    },
    /// Work handed to another agent.
    Escalated {
        from: String,
        to: String,
        reason: String,
    },
    /// Untranslated provider message. Persisted to `raw/<agent>.jsonl`, never dropped.
    Raw {
        value: Value,
    },
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Handle used by adapters and workflows to publish events for one agent.
#[derive(Clone)]
pub struct Emitter {
    run_id: String,
    provider: String,
    agent: String,
    tx: mpsc::Sender<Event>,
}

impl Emitter {
    pub fn new(run_id: &str, provider: &str, agent: &str, tx: mpsc::Sender<Event>) -> Self {
        Self {
            run_id: run_id.into(),
            provider: provider.into(),
            agent: agent.into(),
            tx,
        }
    }

    pub fn for_agent(&self, provider: &str, agent: &str) -> Self {
        Self::new(&self.run_id, provider, agent, self.tx.clone())
    }

    pub async fn emit(&self, kind: EventKind) {
        let ev = Event {
            run_id: self.run_id.clone(),
            seq: 0, // stamped by the recorder so the persisted log is strictly ordered
            ts_ms: now_ms(),
            provider: self.provider.clone(),
            agent: self.agent.clone(),
            kind,
        };
        // A closed bus means the run is shutting down; dropping late events is fine.
        let _ = self.tx.send(ev).await;
    }
}

pub const BUS_CAPACITY: usize = 4096;

/// Creates the bus. Returns the root emitter, the receiver for the recorder,
/// and the broadcast sender the recorder forwards to.
pub fn bus(run_id: &str) -> (Emitter, mpsc::Receiver<Event>, broadcast::Sender<Event>) {
    let (tx, rx) = mpsc::channel(BUS_CAPACITY);
    let (out, _) = broadcast::channel(BUS_CAPACITY);
    (Emitter::new(run_id, "jockey", "harness", tx), rx, out)
}
