//! Top-level render dispatch.
//!
//! Picks a per-screen renderer from the app's [`Screen`] state and
//! overlays modals (permission, disconnected) on top.

pub mod connecting;
pub mod conversation;
pub mod disconnected;
pub mod footer;
pub mod permission_modal;
pub mod picker;
pub mod theme;

// Phase 1 — Tier A primitives lifted from `claude-code-rust`. Not yet
// wired into the render path; consumers come in Phase 3+.
pub mod document_table;
pub mod layout;
pub mod markdown;
pub mod two_column_list;
pub mod wrap;

use ratatui::Frame;

use crate::app::{App, Screen};

/// Render one frame for the current `app` state.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    match app.screen {
        Screen::Connecting => connecting::render(frame, app),
        Screen::Picker => picker::render(frame, app),
        Screen::Conversation => conversation::render(frame, app),
        Screen::Disconnected => disconnected::render(frame, app),
    }

    // Modals overlay any screen.
    if let Some(p) = &app.pending_permission {
        permission_modal::render(frame, p);
    }
}
