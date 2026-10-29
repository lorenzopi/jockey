//! Drawing. Pi-style layout: scrolling transcript, status line, editor, footer.

use super::app::{App, Entry, Level, Popup, SteerState, ToolState, Zoom};
use super::theme;
use super::track;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const TOOL_LINES: usize = 40;

fn dim() -> Style {
    Style::default().fg(theme::faint())
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let width = area.width.saturating_sub(2).max(1) as usize;
    let (rows, cursor) = app.editor.layout(width);
    let editor_h = rows.len().clamp(1, 8) as u16 + 2;
    let [main, track_area, status, editor, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(track::height(app)),
        Constraint::Length(1),
        Constraint::Length(editor_h),
        Constraint::Length(1),
    ])
    .areas(area);
    app.sup_area.set(Rect::default()); // set again if the supervisor pane is drawn
    if app.show_raw {
        draw_raw(f, app, main);
    } else if app.mode == "single" {
        draw_transcript(f, app, main, Pane::All);
    } else {
        draw_split(f, app, main);
    }
    track::draw(f, app, track_area);
    draw_status(f, app, status);
    draw_editor(f, app, editor, rows, cursor);
    draw_footer(f, app, footer);

    let menu = app.menu();
    if !menu.is_empty() {
        let items: Vec<(String, String, bool)> = menu
            .iter()
            .enumerate()
            .map(|(i, (c, d))| {
                (
                    c.clone(),
                    d.to_string(),
                    i == app.menu_selected.min(menu.len() - 1),
                )
            })
            .collect();
        draw_list(f, main, "commands", &items);
    }
    match &app.popup {
        Some(Popup::Picker(p)) => {
            let vis = p.visible();
            let items: Vec<_> = vis
                .iter()
                .enumerate()
                .map(|(i, (_, l))| (l.clone(), String::new(), i == p.selected))
                .collect();
            let title = if p.filter.is_empty() {
                format!("{} · type to filter", p.title)
            } else {
                format!("{} · {}", p.title, p.filter)
            };
            draw_list(f, main, &title, &items);
        }
        Some(Popup::Approval(req, _)) => draw_approval(f, main, &req.kind, &req.detail),
        None => {}
    }
}

/// Word-wraps `text` to `width` display columns, breaking long words.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![];
    for line in text.split('\n') {
        let mut cur = String::new();
        let mut w = 0;
        for word in line.split_inclusive(' ') {
            let ww = word.trim_end().width();
            if w + ww > width && w > 0 {
                out.push(cur.trim_end().to_string());
                cur.clear();
                w = 0;
            }
            for ch in word.chars() {
                let cw = ch.width().unwrap_or(0);
                if w + cw > width {
                    out.push(std::mem::take(&mut cur));
                    w = 0;
                    if ch == ' ' {
                        continue;
                    }
                }
                cur.push(ch);
                w += cw;
            }
        }
        out.push(cur.trim_end().to_string());
    }
    out
}

struct Out {
    lines: Vec<Line<'static>>,
    width: usize,
}

impl Out {
    /// One logical line: `prefix` on the first row, aligned indent after.
    fn push(&mut self, prefix: &str, pstyle: Style, text: &str, style: Style) {
        let pw = prefix.width();
        let indent = " ".repeat(pw);
        for (i, row) in wrap(text, self.width.saturating_sub(pw))
            .into_iter()
            .enumerate()
        {
            let p = if i == 0 {
                prefix.to_string()
            } else {
                indent.clone()
            };
            self.lines.push(Line::from(vec![
                Span::styled(p, pstyle),
                Span::styled(row, style),
            ]));
        }
    }

    /// One-row card: title clipped to fit, `meta` right-aligned (durations, +/− counts).
    fn card(
        &mut self,
        prefix: &str,
        pstyle: Style,
        title: &str,
        tstyle: Style,
        meta: &str,
        mstyle: Style,
    ) {
        let first = title.lines().next().unwrap_or("");
        let more = title.lines().nth(1).is_some();
        let room = self.width.saturating_sub(prefix.width() + meta.width() + 2);
        let mut shown: String = String::new();
        let mut w = 0;
        for ch in first.chars() {
            let cw = ch.width().unwrap_or(0);
            if w + cw > room.saturating_sub(1) {
                break;
            }
            shown.push(ch);
            w += cw;
        }
        if shown.len() < first.len() || more {
            shown.push('…');
            w += 1;
        }
        let pad = self
            .width
            .saturating_sub(prefix.width() + w + meta.width())
            .max(1);
        self.lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), pstyle),
            Span::styled(shown, tstyle),
            Span::raw(" ".repeat(pad)),
            Span::styled(meta.to_string(), mstyle),
        ]));
    }

    fn blank(&mut self) {
        if self.lines.last().is_some_and(|l| l.width() > 0) {
            self.lines.push(Line::default());
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    All,
    Primary,
    Supervisor,
}

/// Entries that belong to the supervisor feed in multi-agent modes.
fn is_supervisor(e: &Entry) -> bool {
    matches!(
        e,
        Entry::Feed(..)
            | Entry::Divider(..)
            | Entry::Decision { .. }
            | Entry::Critic { .. }
            | Entry::Findings { .. }
            | Entry::Verdict { .. }
            | Entry::Steer { .. }
    )
}

/// Primary stream and supervisor feed: side by side on wide terminals,
/// stacked otherwise. Ctrl+\ flips the orientation, Ctrl+Z zooms a pane.
fn draw_split(f: &mut Frame, app: &App, area: Rect) {
    match app.zoom {
        Zoom::Primary => return draw_transcript(f, app, area, Pane::Primary),
        Zoom::Supervisor => return draw_transcript(f, app, area, Pane::Supervisor),
        Zoom::None => {}
    }
    // Side by side by default; Ctrl+\ stacks them.
    let wide = !app.split_flip;
    let [left, right] = if wide {
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area)
    } else {
        Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area)
    };
    draw_transcript(f, app, left, Pane::Primary);
    draw_transcript(f, app, right, Pane::Supervisor);
}

fn transcript_lines(app: &App, width: usize, pane: Pane) -> Vec<Line<'static>> {
    let mut o = Out {
        lines: vec![],
        width,
    };
    for (i, e) in app.transcript.iter().enumerate() {
        let start = o.lines.len();
        // New entries ease in: faint for a moment, then full color.
        let fresh = app
            .born
            .get(i)
            .is_some_and(|b| b.elapsed() < std::time::Duration::from_millis(160));
        match pane {
            // The user's prompt already marks the turn in the single pane.
            Pane::All if matches!(e, Entry::Divider(_)) => continue,
            Pane::Primary if is_supervisor(e) => continue,
            Pane::Supervisor if !is_supervisor(e) => continue,
            _ => {}
        }
        match e {
            Entry::Feed(l, t) => {
                let st = match l {
                    Level::Warn => Style::default().fg(theme::warn()),
                    Level::Error => Style::default().fg(theme::err()),
                    Level::Info => Style::default().fg(theme::rider()),
                };
                o.blank();
                o.push("◆ ", st, t, st.add_modifier(Modifier::BOLD));
            }
            Entry::Decision {
                finding,
                action,
                reason,
            } => {
                let (mark, st) = match action.as_str() {
                    "steer" | "interrupt" | "queue" => ("  ⚑ ", Style::default().fg(theme::ok())),
                    "suggest" => ("  ◇ ", Style::default().fg(theme::accent())),
                    _ => ("  · ", dim()),
                };
                let loc = match (&finding.file, finding.line) {
                    (Some(p), Some(l)) => format!(" ({p}:{l})"),
                    (Some(p), None) => format!(" ({p})"),
                    _ => String::new(),
                };
                o.push(
                    mark,
                    st,
                    &format!(
                        "[{} {:.2}] {}{loc}",
                        finding.severity.as_str(),
                        finding.confidence,
                        finding.description
                    ),
                    Style::default().fg(theme::muted()),
                );
                o.push("      → ", st, &format!("{action}: {reason}"), st);
                if app.show_tools {
                    o.push("      evidence: ", dim(), &finding.evidence, dim());
                    o.push("      fix: ", dim(), &finding.proposed_fix, dim());
                }
            }
            Entry::User(t) => {
                o.blank();
                o.push(
                    "› ",
                    Style::default()
                        .fg(theme::accent())
                        .add_modifier(Modifier::BOLD),
                    t,
                    Style::default().add_modifier(Modifier::BOLD),
                );
                o.blank();
            }
            Entry::Assistant { text, .. } => o.push("", Style::default(), text, Style::default()),
            Entry::Reasoning { text, .. } => {
                if app.show_reasoning {
                    o.push("  ∴ ", dim(), text, dim().add_modifier(Modifier::ITALIC));
                }
            }
            Entry::Tool {
                title,
                state,
                output,
                duration,
                ..
            } => {
                let (mark, st) = match state {
                    ToolState::Running => ("  ▸ ".to_string(), Style::default().fg(theme::warn())),
                    ToolState::Ok => ("  ✓ ".to_string(), Style::default().fg(theme::ok())),
                    ToolState::Failed(c) => (
                        match c {
                            Some(c) => format!("  ✗ [{c}] "),
                            None => "  ✗ ".to_string(),
                        },
                        Style::default().fg(theme::err()),
                    ),
                };
                let meta = match (state, duration) {
                    (ToolState::Running, _) => "…".to_string(),
                    (_, Some(d)) => took(*d),
                    _ => String::new(),
                };
                o.card(
                    &mark,
                    st,
                    title,
                    Style::default().fg(theme::muted()),
                    &meta,
                    dim(),
                );
                if app.show_tools && title.contains('\n') {
                    for l in title.lines() {
                        o.push("    $ ", dim(), l, Style::default().fg(theme::muted()));
                    }
                }
                if app.show_tools && !output.trim().is_empty() {
                    let lines: Vec<_> = output.trim_end().lines().collect();
                    let skip = lines.len().saturating_sub(TOOL_LINES);
                    if skip > 0 {
                        o.push("    ", dim(), &format!("… {skip} earlier lines"), dim());
                    }
                    for l in &lines[skip..] {
                        o.push("    ", dim(), l, dim());
                    }
                }
            }
            Entry::File { path, change, diff } => {
                let (add, del) = diff_counts(diff);
                let meta = match (add, del) {
                    (0, 0) => String::new(),
                    (a, d) => format!("+{a} −{d}"),
                };
                o.card(
                    "  ✎ ",
                    Style::default().fg(theme::info()),
                    &format!("{change} {path}"),
                    Style::default().fg(theme::muted()),
                    &meta,
                    Style::default().fg(theme::ok()),
                );
                if app.show_tools {
                    for l in diff.lines().take(200) {
                        let st = match l.chars().next() {
                            Some('+') => Style::default().fg(theme::ok()),
                            Some('-') => Style::default().fg(theme::err()),
                            Some('@') => Style::default().fg(theme::accent()),
                            _ => dim(),
                        };
                        o.push("    ", dim(), l, st);
                    }
                }
            }
            Entry::Plan(steps) => {
                o.push("  ", dim(), "plan", dim().add_modifier(Modifier::BOLD));
                for s in steps {
                    let (m, st) = match s.status.as_str() {
                        "completed" => ("    ✓ ", Style::default().fg(theme::ok())),
                        "inProgress" | "in_progress" => {
                            ("    ◐ ", Style::default().fg(theme::warn()))
                        }
                        _ => ("    ○ ", dim()),
                    };
                    o.push(m, st, &s.step, Style::default().fg(theme::muted()));
                }
            }
            Entry::Steer { text, state, .. } => {
                let tag = match state {
                    SteerState::Sent => "sent…".to_string(),
                    SteerState::Delivered => "acknowledged".to_string(),
                    SteerState::Applied => "applied".to_string(),
                    SteerState::Rejected(r) => format!("not delivered: {r}"),
                };
                let st = Style::default().fg(theme::rider());
                // Automated corrections are long; show just the findings they carry.
                let shown = if text.starts_with("Correction from an independent reviewer") {
                    let items: Vec<_> = text
                        .lines()
                        .filter(|l| l.starts_with("- ["))
                        .map(|l| &l[2..])
                        .collect();
                    format!("correction: {}", items.join("; "))
                } else {
                    text.clone()
                };
                o.push("  ⇢ ", st, &format!("{shown}  [{tag}]"), st);
            }
            Entry::Approval {
                kind,
                detail,
                decision,
            } => {
                let d = decision.as_deref().unwrap_or("waiting for you");
                o.push(
                    "  ? ",
                    Style::default().fg(theme::warn()),
                    &format!("{kind}: {detail} → {d}"),
                    Style::default().fg(theme::warn()),
                );
            }
            Entry::Critic {
                critic,
                role,
                count,
                error,
            } => {
                let (text, st) = match error {
                    Some(e) => (
                        format!("{critic} ({role}) failed: {e}"),
                        Style::default().fg(theme::err()),
                    ),
                    None => (
                        format!("{critic} ({role}): {count} finding(s)"),
                        Style::default().fg(theme::rider()),
                    ),
                };
                o.push("  ◆ ", st, &text, st);
            }
            Entry::Findings {
                round,
                findings,
                duplicates,
            } => {
                let st = Style::default().fg(theme::rider());
                let head = if findings.is_empty() {
                    format!("round {round}: no findings, nothing to revise")
                } else {
                    format!(
                        "round {round}: {} finding(s) sent back for revision ({duplicates} duplicates merged)",
                        findings.len()
                    )
                };
                o.push("  ◆ ", st, &head, st.add_modifier(Modifier::BOLD));
                for f in findings {
                    let sev = match f.severity {
                        crate::critics::Severity::Critical | crate::critics::Severity::High => {
                            Style::default().fg(theme::err())
                        }
                        crate::critics::Severity::Medium => Style::default().fg(theme::warn()),
                        crate::critics::Severity::Low => dim(),
                    };
                    let loc = match (&f.file, f.line) {
                        (Some(p), Some(l)) => format!(" ({p}:{l})"),
                        (Some(p), None) => format!(" ({p})"),
                        _ => String::new(),
                    };
                    let who = if f.critics.len() > 1 {
                        format!(" ×{}", f.critics.len())
                    } else {
                        String::new()
                    };
                    o.push(
                        &format!("    [{} {:.2}] ", f.severity.as_str(), f.confidence),
                        sev,
                        &format!("{}{loc}{who}", f.description),
                        Style::default().fg(theme::muted()),
                    );
                    if app.show_tools {
                        o.push("        evidence: ", dim(), &f.evidence, dim());
                        o.push("        fix: ", dim(), &f.proposed_fix, dim());
                    }
                }
            }
            Entry::Verdict {
                attempt,
                score,
                accepted,
                issues,
                evidence,
            } => {
                let st = if *accepted {
                    Style::default().fg(theme::ok())
                } else {
                    Style::default().fg(theme::err())
                };
                let verdict = if *accepted {
                    "accepted"
                } else {
                    "not accepted"
                };
                o.push(
                    "  ⚖ ",
                    st,
                    &format!("judge · attempt {attempt} · score {score} · {verdict}"),
                    st.add_modifier(Modifier::BOLD),
                );
                for i in issues {
                    o.push("    - ", st, i, Style::default().fg(theme::muted()));
                }
                if app.show_tools {
                    for e in evidence {
                        o.push("      ", dim(), e, dim());
                    }
                }
            }
            Entry::Divider(t) => {
                let first = t.lines().next().unwrap_or_default();
                let mut clip: String = first.chars().take(60).collect();
                if clip.len() < t.trim_end().len() {
                    clip.push('…');
                }
                o.blank();
                o.push("── ", dim(), &format!("{clip} ──"), dim());
            }
            Entry::Notice(l, t) => {
                let st = match l {
                    Level::Info => dim(),
                    Level::Warn => Style::default().fg(theme::warn()),
                    Level::Error => Style::default().fg(theme::err()),
                };
                for (i, line) in t.lines().enumerate() {
                    o.push(if i == 0 { "  ! " } else { "    " }, st, line, st);
                }
            }
        }
        if fresh {
            for line in &mut o.lines[start..] {
                for span in line.spans.iter_mut() {
                    span.style = span.style.fg(theme::faint());
                }
            }
        }
    }
    o.lines
}

/// Durations like Claude Code shows them: 0.4s, 12s, 1m 3s.
fn took(d: std::time::Duration) -> String {
    let s = d.as_secs_f32();
    if s < 10.0 {
        format!("{s:.1}s")
    } else {
        elapsed(d.as_secs())
    }
}

/// Added and removed lines in a unified diff.
fn diff_counts(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(a, d), l| {
        if l.starts_with("+++") || l.starts_with("---") {
            (a, d)
        } else if l.starts_with('+') {
            (a + 1, d)
        } else if l.starts_with('-') {
            (a, d + 1)
        } else {
            (a, d)
        }
    })
}

/// The jockey mark: the user's ASCII horse and rider, shaded with the
/// wordmark's amber-to-rose gradient; light edge strokes stay faint.
fn emblem(art: &[&str]) -> Vec<Line<'static>> {
    let w = art.iter().map(|r| r.len()).max().unwrap_or(1);
    art.iter()
        .map(|row| {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(row.chars().enumerate().map(|(x, c)| {
                let fg = match c {
                    'o' | '#' | '@' => theme::gradient(x, w),
                    _ => theme::faint(),
                };
                let st = Style::default().fg(fg);
                Span::styled(
                    c.to_string(),
                    if c == '@' {
                        st.add_modifier(Modifier::BOLD)
                    } else {
                        st
                    },
                )
            }));
            Line::from(spans)
        })
        .collect()
}

/// Empty-screen splash: the emblem, the name, how to start.
fn splash(app: &App, height: u16) -> Vec<Line<'static>> {
    let mut lines = vec![];
    let title = "J O C K E Y";
    let n = title.chars().count();
    let mut spans = vec![Span::raw("  ")];
    for (i, ch) in title.chars().enumerate() {
        spans.push(Span::styled(
            ch.to_string(),
            Style::default()
                .fg(theme::gradient(i, n))
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(spans));
    lines.push(Line::from(vec![
        Span::styled(
            "  one saddle, many coding agents · ",
            Style::default().fg(theme::muted()),
        ),
        Span::styled(format!("v{}", env!("CARGO_PKG_VERSION")), dim()),
    ]));
    lines.push(Line::from(Span::styled(
        format!("  {}", app.cwd.display()),
        dim(),
    )));
    lines.push(Line::default());
    let key = |k: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<9}"), Style::default().fg(theme::accent())),
            Span::styled(what.to_string(), Style::default().fg(theme::muted())),
        ])
    };
    lines.push(key("Enter", "send a task · while it works, steer it"));
    lines.push(key("Ctrl+L", "lab › model › thinking"));
    lines.push(key("/mode", "single · cascade · critique · autosteer"));
    lines.push(key("/help", "every key and command"));
    // The biggest mark that fits above the full text, else the small one above
    // just the name (title, tagline, cwd), else the text alone.
    use super::horse_art::{MARK, MARK_SMALL};
    let room = (height as usize).saturating_sub(2); // blank line on top, gap below the mark
    let art: &[&str] = if room >= MARK.len() + lines.len() {
        &MARK
    } else if room >= MARK_SMALL.len() + lines.len() {
        &MARK_SMALL
    } else if room >= MARK_SMALL.len() + 3 {
        lines.truncate(3);
        &MARK_SMALL
    } else {
        &[]
    };
    let mut out = vec![Line::default()];
    if !art.is_empty() {
        out.extend(emblem(art));
        out.push(Line::default());
    }
    out.extend(lines);
    out
}

fn draw_transcript(f: &mut Frame, app: &App, area: Rect, pane: Pane) {
    let (area, lines) = if pane == Pane::All {
        (area, transcript_lines(app, area.width as usize, pane))
    } else {
        // Bordered pane with a title; the supervisor pane always follows the bottom.
        let title = match pane {
            Pane::Primary => format!(" primary · {} ", app.agent_label()),
            _ if app.scroll_sup > 0 => {
                format!(
                    " supervisor · ↑ {} lines (wheel here or Alt+PgDn) ",
                    app.scroll_sup
                )
            }
            _ => " supervisor · reviews, findings, steering ".to_string(),
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(dim())
            .title(Span::styled(title, dim()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if pane == Pane::Supervisor {
            app.sup_area.set(inner); // for routing mouse-wheel scrolls
        }
        (inner, transcript_lines(app, inner.width as usize, pane))
    };
    if lines.is_empty() && pane == Pane::Supervisor {
        let hint = "critic reviews, findings and steering decisions appear here";
        f.render_widget(Paragraph::new(Span::styled(hint, dim())), area);
        return;
    }
    if lines.is_empty() {
        f.render_widget(Paragraph::new(splash(app, area.height)), area);
        return;
    }
    let h = area.height as usize;
    let max_scroll = lines.len().saturating_sub(h);
    let scroll = if pane == Pane::Supervisor {
        app.scroll_sup.min(max_scroll)
    } else {
        app.scroll.min(max_scroll)
    };
    let start = lines.len().saturating_sub(h + scroll);
    let end = (start + h).min(lines.len());
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), area);
}

fn draw_raw(f: &mut Frame, app: &App, area: Rect) {
    let h = area.height.saturating_sub(2) as usize;
    let rows: Vec<Line> = app
        .raw
        .iter()
        .rev()
        .skip(app.scroll)
        .take(h)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|r| Line::from(Span::styled(r.clone(), dim())))
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" raw provider events · Ctrl+R to close ");
    f.render_widget(Paragraph::new(rows).block(block), area);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![];
    if let Some(t) = app.busy_since {
        let verb = if app.interrupting {
            "Interrupting".to_string()
        } else {
            match &app.activity {
                Some((s, d)) => match (s.as_str(), d) {
                    ("thinking", _) => "Thinking".into(),
                    ("writing", _) => "Writing".into(),
                    ("tool", Some(d)) => format!("Running {}", clip(d, 40)),
                    ("compacting", _) => "Compacting context".into(),
                    ("retrying", _) => "Retrying".into(),
                    ("waiting", _) => "Waiting for your approval".into(),
                    (other, _) => other.to_string(),
                },
                None => "Working".into(),
            }
        };
        let tokens = app.tokens_reported + (app.streamed_chars / 4) as u64;
        let mut stats = vec![elapsed(t.elapsed().as_secs())];
        if tokens > 0 {
            stats.push(format!("↓ {} tokens", human(tokens)));
        }
        if let Some(th) = app.thought_tokens.filter(|t| *t > 0) {
            stats.push(format!("thought {}", human(th)));
        }
        spans.push(Span::styled(
            format!("{} · ", app.agent.provider),
            Style::default().fg(theme::muted()),
        ));
        spans.extend(theme::shimmer(
            &format!("{verb}…"),
            t.elapsed().as_millis(),
            theme::accent(),
            theme::accent_hi(),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("({})", stats.join(" · ")), dim()));
        spans.push(Span::styled("  Esc interrupt · Enter steer", dim()));
    }
    let open = app.open_suggestion_count();
    if open > 0 {
        spans.push(Span::styled(
            format!(
                "  ◇ {open} steer suggestion{} (Ctrl+S)",
                if open == 1 { "" } else { "s" }
            ),
            Style::default().fg(theme::accent()),
        ));
    }
    if !app.queue.is_empty() {
        spans.push(Span::styled(
            format!("  {} queued (Alt+↑ edit)", app.queue.len()),
            Style::default().fg(theme::warn()),
        ));
    }
    if app.scroll > 0 {
        spans.push(Span::styled(
            format!("  ↑ scrolled {} lines (wheel/PgDn)", app.scroll),
            dim(),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn effort_color(e: Option<&str>) -> Color {
    match e {
        Some("low") | Some("minimal") | Some("none") => theme::faint(),
        Some("medium") => theme::info(),
        Some("high") => theme::rider(),
        Some(_) => theme::err(),
        None => theme::muted(),
    }
}

fn draw_editor(f: &mut Frame, app: &App, area: Rect, rows: Vec<String>, cursor: (usize, usize)) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let first = cursor.0.saturating_sub(inner_h.saturating_sub(1));
    let lines: Vec<Line> = rows
        .iter()
        .skip(first)
        .take(inner_h)
        .map(|r| Line::from(r.clone()))
        .collect();
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(effort_color(app.agent.effort.as_deref())));
    f.render_widget(Paragraph::new(lines).block(block), area);
    if app.popup.is_none() {
        f.set_cursor_position((
            area.x + 1 + cursor.1 as u16,
            area.y + 1 + (cursor.0 - first) as u16,
        ));
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let home = std::env::var("HOME").unwrap_or_default();
    let cwd = app.cwd.display().to_string();
    let cwd = match cwd.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => cwd,
    };
    let model = app.agent_label();
    let profile = serde_json::to_value(app.profile)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let left = format!("{cwd} · {} · {model} · {profile}", app.mode_badge());
    let mut right = vec![];
    if app.usage.tokens_in > 0 {
        right.push(format!(
            "↑{} ↓{}",
            human(app.usage.tokens_in),
            human(app.usage.tokens_out)
        ));
    }
    if let Some(c) = app.usage.context_pct {
        right.push(format!("ctx {c:.0}%"));
    }
    let mut right_spans = vec![Span::styled(right.join(" · "), dim())];
    if let Some(l) = app.usage.limit_pct {
        let st = if app.usage.limit_reached || l >= 100.0 {
            Style::default().fg(theme::err())
        } else if l >= 80.0 {
            Style::default().fg(theme::warn())
        } else {
            dim()
        };
        let sep = if right.is_empty() { "" } else { " · " };
        right_spans.push(Span::styled(format!("{sep}limit {l:.0}%"), st));
    }
    let rw: usize = right_spans.iter().map(|s| s.content.width()).sum();
    let avail = (area.width as usize).saturating_sub(rw + 1);
    let left: String = if left.width() > avail {
        let mut s: String = left.chars().take(avail.saturating_sub(1)).collect();
        s.push('…');
        s
    } else {
        left
    };
    let pad = (area.width as usize).saturating_sub(left.width() + rw);
    let mut spans = vec![Span::styled(left, dim()), Span::raw(" ".repeat(pad))];
    spans.extend(right_spans);
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// "45s", "10m 6s", "1h 2m": the same shape Claude Code uses.
pub fn elapsed(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

fn clip(s: &str, n: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= n {
        return line.to_string();
    }
    let mut out: String = line.chars().take(n).collect();
    out.push('…');
    out
}

pub fn human(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// A list anchored to the bottom of `area` (just above the editor).
fn draw_list(f: &mut Frame, area: Rect, title: &str, items: &[(String, String, bool)]) {
    let max = 10usize;
    let sel = items.iter().position(|i| i.2).unwrap_or(0);
    let first = sel.saturating_sub(max - 1);
    let shown: Vec<Line> = items
        .iter()
        .skip(first)
        .take(max)
        .map(|(a, b, s)| {
            let st = if *s {
                Style::default().fg(Color::Black).bg(theme::accent())
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!(" {a} "), st),
                Span::styled(format!(" {b}"), dim()),
            ])
        })
        .collect();
    let h = (shown.len().max(1) as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x,
        y: area.y + area.height - h,
        width: area.width,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "));
    let body = if shown.is_empty() {
        vec![Line::from(Span::styled(" no matches", dim()))]
    } else {
        shown
    };
    f.render_widget(Paragraph::new(body).block(block), rect);
}

fn draw_approval(f: &mut Frame, area: Rect, kind: &str, detail: &str) {
    let w = area.width.min(80);
    let text_rows = wrap(detail, w.saturating_sub(4) as usize);
    let h = (text_rows.len() as u16 + 5).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + area.height.saturating_sub(h),
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let mut lines = vec![Line::from(Span::styled(
        format!("The agent wants to run a {kind}:"),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    lines.extend(text_rows.into_iter().map(|r| {
        Line::from(Span::styled(
            format!("  {r}"),
            Style::default().fg(theme::warn()),
        ))
    }));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("[y]", Style::default().fg(theme::ok())),
        Span::raw(" allow  "),
        Span::styled("[a]", Style::default().fg(theme::ok())),
        Span::raw(" allow for this session  "),
        Span::styled("[n]", Style::default().fg(theme::err())),
        Span::raw(" deny"),
    ]));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::warn()))
        .title(" approval ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_width_and_breaks_long_words() {
        assert_eq!(wrap("hello big world", 9), vec!["hello big", "world"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert!(wrap("日本語のテキスト", 6).iter().all(|l| l.width() <= 6));
    }
}
