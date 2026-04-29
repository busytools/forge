//! Forge-internal style helpers used by the legacy hand-rolled UI
//! files (`conversation.rs`, `picker.rs`, `footer.rs`, `connecting.rs`,
//! `disconnected.rs`, `permission_modal.rs`).
//!
//! Wraps theme constants into convenience `Style` builders. Will be
//! deleted in Phase 4 once those legacy files are replaced with the
//! lifted upstream chat / footer / picker UI.

use ratatui::style::{Color, Modifier, Style};

use super::theme;

/// Default terminal text style — no fg/bg overrides.
#[must_use]
pub fn text() -> Style {
    Style::default()
}

/// Dim secondary text (timestamps, help bar, inactive borders).
#[must_use]
pub fn dim() -> Style {
    Style::default().fg(theme::DIM)
}

/// Selected row — reverse on the brand accent.
#[must_use]
pub fn selected() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(theme::RUST_ORANGE)
        .add_modifier(Modifier::BOLD)
}

/// Title / heading — accent color, bold.
#[must_use]
pub fn heading() -> Style {
    Style::default()
        .fg(theme::RUST_ORANGE)
        .add_modifier(Modifier::BOLD)
}
