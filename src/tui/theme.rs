//! Palette and small motion helpers. Calm base, one warm accent (the silks).
//! Truecolor when the terminal says so, the 16-color palette otherwise.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::sync::OnceLock;

fn truecolor() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| {
        std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit"))
    })
}

/// `JOCKEY_ASCII=1` swaps emoji (🏇 🏁) for plain glyphs on fonts without them.
pub fn ascii() -> bool {
    static A: OnceLock<bool> = OnceLock::new();
    *A.get_or_init(|| std::env::var("JOCKEY_ASCII").is_ok_and(|v| v != "0"))
}

fn pick(rgb: (u8, u8, u8), basic: Color) -> Color {
    if truecolor() {
        Color::Rgb(rgb.0, rgb.1, rgb.2)
    } else {
        basic
    }
}

/// Warm amber: the jockey's silks. Prompts, the rider, highlights.
pub fn accent() -> Color {
    pick((255, 176, 92), Color::Yellow)
}
/// Selected row in menus: amber at about a third strength over a dark
/// background (a "translucent" highlight), with light amber text on it.
/// Without truecolor: black on yellow.
pub fn highlight_bg() -> Color {
    pick((92, 66, 38), Color::Yellow)
}
pub fn highlight_fg() -> Color {
    pick((255, 228, 176), Color::Black)
}
/// Lighter amber for the shimmer's highlight.
pub fn accent_hi() -> Color {
    pick((255, 228, 176), Color::White)
}
/// Rose: critics and supervisors.
pub fn rider() -> Color {
    pick((222, 120, 160), Color::Magenta)
}
pub fn muted() -> Color {
    pick((150, 150, 160), Color::Gray)
}
pub fn faint() -> Color {
    pick((92, 92, 104), Color::DarkGray)
}
pub fn ok() -> Color {
    pick((126, 204, 144), Color::Green)
}
pub fn err() -> Color {
    pick((236, 112, 112), Color::Red)
}
pub fn warn() -> Color {
    pick((232, 196, 98), Color::Yellow)
}
pub fn info() -> Color {
    pick((132, 172, 255), Color::Blue)
}

/// A soft highlight band sweeping across `text` (the live activity word).
pub fn shimmer(text: &str, t_ms: u128, base: Color, hi: Color) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let period = chars.len() + 8;
    let head = (t_ms / 70) as usize % period;
    chars
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let d = (i as isize - head as isize).unsigned_abs();
            let st = match d {
                0 => Style::default().fg(hi).add_modifier(Modifier::BOLD),
                1 => Style::default().fg(hi),
                _ => Style::default().fg(base),
            };
            Span::styled(c.to_string(), st)
        })
        .collect()
}

/// Gradient across the splash wordmark, warm to rose.
pub fn gradient(i: usize, n: usize) -> Color {
    if !truecolor() {
        return if i * 2 < n {
            Color::Yellow
        } else {
            Color::Magenta
        };
    }
    let t = i as f32 / n.max(1) as f32;
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
    Color::Rgb(lerp(255, 222), lerp(176, 120), lerp(92, 160))
}
