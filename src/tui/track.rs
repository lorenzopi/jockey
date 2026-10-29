//! The racetrack: while an agent works, a jockey gallops along a rail above
//! the status line. Pace follows the live token rate; critics ride a second
//! lane while they review; a finished turn crosses the line.

use super::app::{App, Finish};
use super::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::time::{Duration, Instant};

/// How long the finish line stays up after a turn.
pub const FINISH_SHOWN: Duration = Duration::from_millis(3200);

/// Rows the track needs right now (0 when idle).
pub fn height(app: &App) -> u16 {
    let finishing = app
        .finish
        .as_ref()
        .is_some_and(|f| f.at.elapsed() < FINISH_SHOWN);
    match (app.busy(), finishing) {
        (true, _) => 1 + u16::from(app.reviewing > 0),
        (false, true) => 1,
        _ => 0,
    }
}

fn glyphs() -> (&'static str, &'static str, &'static str) {
    if theme::ascii() {
        (">", "|", "n")
    } else {
        ("🏇", "🏁", "♞")
    }
}

/// Cells per second: a trot when quiet, a gallop when tokens are flowing.
fn pace(app: &App) -> f32 {
    let secs = app
        .busy_since
        .map_or(1.0, |t| t.elapsed().as_secs_f32().max(1.0));
    let tokens = app.tokens_reported as f32 + app.streamed_chars as f32 / 4.0;
    6.0 + (tokens / secs / 3.0).min(34.0)
}

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let lane = (area.width as usize).saturating_sub(8).max(8);
    let now = Instant::now();
    let dt = app
        .gallop_t
        .replace(Some(now))
        .map_or(0.0, |t| (now - t).as_secs_f32().min(0.25));
    let mut rows = vec![];
    if app.busy() {
        let pos = (app.gallop_pos.get() + dt * pace(app)) % lane as f32;
        app.gallop_pos.set(pos);
        let tick = app
            .busy_since
            .map_or(0, |t| t.elapsed().as_millis() as usize / 90);
        rows.push(rider_lane(lane, pos as usize, tick));
        if app.reviewing > 0 {
            let cp = (app.critic_pos.get() + dt * 5.0) % lane as f32;
            app.critic_pos.set(cp);
            rows.push(critic_lane(lane, cp as usize, app.reviewing));
        }
    } else if let Some(fin) = &app.finish {
        rows.push(finish_line(lane, fin));
    }
    f.render_widget(Paragraph::new(rows), area);
}

fn rail(n: usize) -> Span<'static> {
    Span::styled("─".repeat(n), Style::default().fg(theme::faint()))
}

/// `──────∙⠄⠂🏇····──────🏁`: dust behind, a few dots of open track ahead.
fn rider_lane(lane: usize, pos: usize, tick: usize) -> Line<'static> {
    let (horse, flag, _) = glyphs();
    // The dust trail shuffles every frame so the rider reads as galloping.
    let frames = [["∙", "⠄", "⠂"], ["⠄", "⠂", "∙"], ["⠂", "∙", "⠄"]];
    let dust: Vec<&str> = frames[tick % 3].to_vec();
    let trail = pos.min(3);
    let mut spans = vec![Span::raw("  "), rail(pos - trail)];
    for (i, d) in dust.iter().skip(3 - trail).enumerate() {
        let c = if i + 1 == trail {
            theme::muted()
        } else {
            theme::faint()
        };
        spans.push(Span::styled(*d, Style::default().fg(c)));
    }
    spans.push(Span::styled(
        horse,
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    ));
    let ahead = lane.saturating_sub(pos + 1);
    let dots = ahead.min(4);
    spans.push(Span::styled(
        "·".repeat(dots),
        Style::default().fg(theme::faint()),
    ));
    spans.push(rail(ahead - dots));
    spans.push(Span::raw(" "));
    spans.push(Span::raw(flag));
    Line::from(spans)
}

/// Critics trail behind on their own lane while a review runs.
fn critic_lane(lane: usize, pos: usize, n: usize) -> Line<'static> {
    let (_, _, knight) = glyphs();
    let label = format!(" {n} critic{} reviewing", if n == 1 { "" } else { "s" });
    let lane = lane.saturating_sub(label.chars().count()).max(4);
    let riders: Vec<usize> = (0..n.min(4))
        .map(|i| (pos + lane * 4 - i * 5) % lane)
        .collect();
    let mut cells: Vec<Span<'static>> = vec![Span::raw("  ")];
    for x in 0..lane {
        cells.push(if riders.contains(&x) {
            Span::styled(knight, Style::default().fg(theme::rider()))
        } else {
            Span::styled("┈", Style::default().fg(theme::faint()))
        });
    }
    cells.push(Span::styled(label, Style::default().fg(theme::rider())));
    Line::from(cells)
}

/// The rider past the post, with the result.
fn finish_line(lane: usize, fin: &Finish) -> Line<'static> {
    let (horse, flag, _) = glyphs();
    let (msg, color) = match fin.kind {
        super::app::FinishKind::Won => (format!("finished · {}", fin.summary), theme::accent()),
        super::app::FinishKind::PulledUp => (format!("pulled up · {}", fin.summary), theme::warn()),
        super::app::FinishKind::Fell => (format!("did not finish · {}", fin.summary), theme::err()),
    };
    // A short "photo finish" glow right after the line is crossed.
    let glow = fin.at.elapsed() < Duration::from_millis(500);
    let st = if glow {
        Style::default()
            .fg(color)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    let room = lane.saturating_sub(msg.chars().count() + 4);
    Line::from(vec![
        Span::raw("  "),
        rail(room),
        Span::raw(" "),
        Span::raw(flag),
        Span::styled(horse, Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(format!(" {msg} "), st),
    ])
}
