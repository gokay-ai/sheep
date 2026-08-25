//! Colour, kept to the sixteen names the user's terminal already defines.
//!
//! Sheep runs in whatever terminal the user has, docked next to an agent. A
//! hardcoded `#1e1e2e` looks like a bug on a light profile, so nothing here
//! names an RGB value: named ANSI colours resolve through the user's own theme,
//! and emphasis is carried by `BOLD` / `DIM` / `REVERSED`, which are readable on
//! any background by construction.
//!
//! The one rule worth stating: **never set a background colour except through
//! `REVERSED`**. Reversing swaps the terminal's own foreground and background,
//! so a badge contrasts correctly on light and dark alike.

use crate::store::TurnKind;
use ratatui::style::{Color, Modifier, Style};

/// Ordinary text — deliberately empty so the terminal's default wins.
pub fn plain() -> Style {
    Style::new()
}

pub fn strong() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

pub fn quiet_italic() -> Style {
    Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC)
}

/// Sheep's accent. Used for structure, never for warnings.
pub fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

pub fn accent_strong() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

/// A label that has to be seen: the product name, the restore key.
pub fn badge() -> Style {
    Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

pub fn added() -> Style {
    Style::new().fg(Color::Green)
}

pub fn removed() -> Style {
    Style::new().fg(Color::Red)
}

pub fn ok() -> Style {
    Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)
}

pub fn warn() -> Style {
    Style::new().fg(Color::Yellow)
}

pub fn danger() -> Style {
    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)
}

/// Each kind of turn gets its own hue so a timeline can be read by shape.
pub fn kind(kind: TurnKind) -> Style {
    match kind {
        TurnKind::Turn => Style::new().fg(Color::Cyan),
        TurnKind::Checkpoint => Style::new().fg(Color::Yellow),
        TurnKind::Manual => Style::new().fg(Color::Blue),
    }
}

/// The bar drawn down the left of the selected row. A coloured bar rather than
/// a filled background: a background block is the one thing that reliably looks
/// wrong on the half of terminals whose theme you did not test against.
pub const CURSOR: &str = "▌";
pub const GUTTER: &str = " ";

pub fn cursor_style() -> Style {
    accent().add_modifier(Modifier::BOLD)
}
