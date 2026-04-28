//! Color + style constants shared across screens.

use ratatui::style::{Color, Modifier, Style};

/// Accent — used for selection highlights, "new session" entry,
/// streaming cursor.
pub const ACCENT: Color = Color::Rgb(206, 66, 43); // rust orange

/// Dim — secondary text (timestamps, help bar, inactive borders).
pub const DIM: Color = Color::DarkGray;

/// Success / connected.
pub const OK: Color = Color::Green;

/// Warning / reconnecting.
pub const WARN: Color = Color::Yellow;

/// Error / disconnected.
pub const ERR: Color = Color::Red;

/// Viewer / informational accent.
pub const INFO: Color = Color::LightBlue;

/// Default text style (terminal foreground).
#[must_use]
pub fn text() -> Style {
    Style::default()
}

/// Dim text — for low-priority annotations.
#[must_use]
pub fn dim() -> Style {
    Style::default().fg(DIM)
}

/// Selected row (reverse-video on accent).
#[must_use]
pub fn selected() -> Style {
    Style::default().fg(Color::White).bg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Title / heading.
#[must_use]
pub fn heading() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}
