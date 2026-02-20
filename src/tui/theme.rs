#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};

// ── Color Palette ──────────────────────────────────────────
// Warm honey / amber tones for the bee theme.
// Pops against dark terminal backgrounds.

pub const HONEY: Color = Color::Rgb(255, 183, 77); // warm amber
pub const GOLD: Color = Color::Rgb(255, 215, 0); // bright gold
pub const NECTAR: Color = Color::Rgb(255, 138, 61); // deep orange
pub const POLLEN: Color = Color::Rgb(250, 230, 140); // soft yellow
pub const WAX: Color = Color::Rgb(60, 56, 48); // dark warm gray
pub const COMB: Color = Color::Rgb(40, 37, 32); // darker bg
pub const SMOKE: Color = Color::Rgb(140, 135, 125); // muted text
pub const ROYAL: Color = Color::Rgb(160, 120, 255); // purple accent
pub const MINT: Color = Color::Rgb(100, 230, 180); // green/success
pub const EMBER: Color = Color::Rgb(255, 90, 90); // red/error
pub const FROST: Color = Color::Rgb(220, 220, 225); // bright text

// ── Styles ─────────────────────────────────────────────────

pub fn title() -> Style {
    Style::default().fg(HONEY).add_modifier(Modifier::BOLD)
}

pub fn subtitle() -> Style {
    Style::default().fg(SMOKE)
}

pub fn text() -> Style {
    Style::default().fg(FROST)
}

pub fn muted() -> Style {
    Style::default().fg(SMOKE)
}

pub fn accent() -> Style {
    Style::default().fg(HONEY)
}

pub fn highlight() -> Style {
    Style::default().fg(COMB).bg(HONEY).add_modifier(Modifier::BOLD)
}

pub fn selected() -> Style {
    Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
}

pub fn success() -> Style {
    Style::default().fg(MINT)
}

pub fn error() -> Style {
    Style::default().fg(EMBER)
}

pub fn agent_color() -> Style {
    Style::default().fg(ROYAL)
}

pub fn key_hint() -> Style {
    Style::default().fg(HONEY).add_modifier(Modifier::BOLD)
}

pub fn key_desc() -> Style {
    Style::default().fg(SMOKE)
}

pub fn border() -> Style {
    Style::default().fg(WAX)
}

pub fn border_active() -> Style {
    Style::default().fg(HONEY)
}

pub fn input_cursor() -> Style {
    Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
}

pub fn status_running() -> Style {
    Style::default().fg(MINT)
}

pub fn status_idle() -> Style {
    Style::default().fg(SMOKE)
}

pub fn status_done() -> Style {
    Style::default().fg(POLLEN)
}

pub fn logo() -> Style {
    Style::default().fg(HONEY).add_modifier(Modifier::BOLD)
}
