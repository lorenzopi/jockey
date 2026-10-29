//! Plain-terminal rendering of the event stream (used by `run` and `show`).
//! Agent text goes to stdout; activity lines go to stderr so output pipes cleanly.

use crate::events::{Event, EventKind};
use std::collections::HashSet;
use std::io::Write;

#[derive(Default)]
pub struct TextRenderer {
    mid_line: bool,
    streamed: HashSet<String>,
    /// Replaying a saved run: everything goes to stdout, in order.
    pub replay: bool,
}

impl TextRenderer {
    pub fn replay() -> Self {
        Self {
            replay: true,
            ..Default::default()
        }
    }

    fn nl(&mut self) {
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
    }

    fn info(&mut self, ev: &Event, line: String) {
        self.nl();
        if self.replay {
            println!("  {} {line}", tag(ev));
        } else {
            let _ = std::io::stdout().flush();
            eprintln!("  {} {line}", tag(ev));
        }
    }

    pub fn render(&mut self, ev: &Event) {
        use EventKind::*;
        // Reviewers' raw text (e.g. a judge's JSON) is summarized by their verdict events.
        let reviewer = ev.agent.starts_with("judge") || ev.agent.starts_with("critic");
        if reviewer
            && matches!(
                ev.kind,
                MessageDelta { .. } | Message { .. } | ReasoningDelta { .. }
            )
        {
            return;
        }
        match &ev.kind {
            MessageDelta { item_id, text } => {
                self.streamed.insert(item_id.clone());
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.mid_line = !text.ends_with('\n');
            }
            Message { item_id, text, .. } => {
                if !self.streamed.contains(item_id) {
                    print!("{text}");
                    self.mid_line = !text.ends_with('\n');
                }
                self.nl();
            }
            AgentStarted {
                session_id,
                model,
                effort,
                ..
            } => {
                let model = model.as_deref().unwrap_or("default model");
                let effort = effort
                    .as_deref()
                    .map(|e| format!(" @{e}"))
                    .unwrap_or_default();
                let sid = session_id.as_deref().unwrap_or("-");
                self.info(ev, format!("● started {model}{effort} · session {sid}"));
            }
            ToolStarted { title, .. } => self.info(ev, format!("▸ {}", one_line(title))),
            ToolCompleted {
                title,
                ok,
                exit_code,
                ..
            } => {
                let mark = if *ok { "✓" } else { "✗" };
                let code = exit_code
                    .filter(|c| *c != 0)
                    .map(|c| format!(" (exit {c})"))
                    .unwrap_or_default();
                self.info(ev, format!("{mark} {}{code}", one_line(title)));
            }
            FileChanged { path, change, .. } => self.info(ev, format!("✎ {change} {path}")),
            Plan { steps, .. } => {
                let s: Vec<_> = steps
                    .iter()
                    .map(|s| {
                        let m = match s.status.as_str() {
                            "completed" => "✓",
                            "inProgress" | "in_progress" => "◐",
                            _ => "○",
                        };
                        format!("{m} {}", s.step)
                    })
                    .collect();
                self.info(ev, format!("plan: {}", s.join(" · ")));
            }
            ApprovalRequested { kind, detail, .. } => {
                self.info(ev, format!("? approval {kind}: {}", one_line(detail)))
            }
            ApprovalResolved { decision, by, .. } => {
                self.info(ev, format!("  → {decision} ({by})"))
            }
            SteeringSent { text, .. } => self.info(ev, format!("⇢ steer: {}", one_line(text))),
            SteeringDelivered { .. } => self.info(ev, "⇢ steer acknowledged".into()),
            SteeringApplied { .. } => self.info(ev, "⇢ steer applied".into()),
            SteeringRejected { reason, .. } => self.info(ev, format!("⇢ steer rejected: {reason}")),
            RateLimit {
                used_percent,
                limit_reached: true,
                ..
            } => self.info(
                ev,
                format!("! rate limit reached ({used_percent:.0}% used)"),
            ),
            Warning { message } => self.info(ev, format!("! {message}")),
            AgentInterrupted { reason } => self.info(ev, format!("■ interrupted: {reason}")),
            AgentFailed { error } => self.info(ev, format!("✗ failed: {error}")),
            CheckCompleted {
                command,
                ok,
                exit_code,
                ..
            } => {
                let mark = if *ok { "✓" } else { "✗" };
                let code = exit_code
                    .map(|c| format!(" (exit {c})"))
                    .unwrap_or_default();
                self.info(ev, format!("{mark} check `{command}`{code}"))
            }
            JudgeResult {
                attempt,
                score,
                accepted,
                issues,
                ..
            } => {
                let verdict = if *accepted { "accepted" } else { "rejected" };
                self.info(
                    ev,
                    format!("⚖ attempt {attempt}: score {score} · {verdict}"),
                );
                for i in issues {
                    self.info(ev, format!("   - {}", one_line(i)));
                }
            }
            ReviewTriggered {
                round,
                critics,
                models,
                ..
            } => self.info(
                ev,
                format!(
                    "◆ review round {round}: {} on one snapshot",
                    if models.is_empty() {
                        format!("{} critics", critics.len())
                    } else {
                        tally(models)
                    }
                ),
            ),
            CritiqueReceived {
                critic,
                role,
                findings,
                error,
            } => match error {
                Some(e) => self.info(ev, format!("◆ {critic} ({role}) failed: {}", one_line(e))),
                None => self.info(
                    ev,
                    format!("◆ {critic} ({role}): {} finding(s)", findings.len()),
                ),
            },
            FindingsMerged {
                findings,
                duplicates,
                ..
            } => {
                self.info(
                    ev,
                    format!(
                        "◆ {} finding(s) after merging ({duplicates} duplicates removed)",
                        findings.len()
                    ),
                );
                for f in findings {
                    let loc = f.file.as_deref().map(|p| match f.line {
                        Some(l) => format!(" ({p}:{l})"),
                        None => format!(" ({p})"),
                    });
                    self.info(
                        ev,
                        format!(
                            "   [{} {:.2}] {}{}",
                            f.severity.as_str(),
                            f.confidence,
                            one_line(&f.description),
                            loc.unwrap_or_default()
                        ),
                    );
                }
            }
            SteeringDecision {
                finding,
                action,
                reason,
                ..
            } => {
                let mark = match action.as_str() {
                    "steer" | "interrupt" | "queue" => "⚑",
                    "suggest" => "◇",
                    _ => "·",
                };
                self.info(
                    ev,
                    format!(
                        "{mark} [{} {:.2}] {} → {action} ({reason})",
                        finding.severity.as_str(),
                        finding.confidence,
                        one_line(&finding.description)
                    ),
                )
            }
            Escalated { from, to, reason } => {
                self.info(ev, format!("↗ escalating {from} → {to}: {reason}"))
            }
            AgentCompleted { .. } => self.nl(),
            _ => {}
        }
    }
}

fn tag(ev: &Event) -> String {
    if ev.agent == "primary" || ev.agent == "harness" {
        ev.provider.clone()
    } else {
        format!("{}/{}", ev.provider, ev.agent)
    }
}

fn one_line(s: &str) -> String {
    let l = s.lines().next().unwrap_or("");
    let mut out: String = l.chars().take(120).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}
/// "2× a" when every critic runs the same model, else "a, b".
pub fn tally(labels: &[String]) -> String {
    match labels {
        [first, rest @ ..] if rest.iter().all(|l| l == first) => {
            format!("{}× {first}", labels.len())
        }
        _ => labels.join(", "),
    }
}
