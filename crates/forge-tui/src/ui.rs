//! Top-level render dispatch.

pub mod connecting;
pub mod conversation;
pub mod disconnected;
pub mod footer;
pub mod permission_modal;
pub mod picker;
pub mod theme;

// Tier A primitives lifted from claude-code-rust.
pub mod document_table;
pub mod layout;
pub mod markdown;
pub mod two_column_list;
pub mod wrap;

// Phase 3a — diff + highlight (depend on `state::model::Diff`).
pub mod diff;
pub mod highlight;

// Forge-internal style helpers consumed by the legacy hand-rolled UI;
// removed when those files are replaced.
pub mod style;

// Phase 3b.7+ — upstream UI lifted into ui::lifted::*. Active for
// Chat view; Picker / Connecting / Disconnected still on legacy.
pub mod lifted;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};

use crate::app::{App, Screen};

/// Render one frame for the current `app` state.
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    match app.active_view {
        Screen::Connecting => connecting::render(frame, app),
        Screen::SessionPicker => picker::render(frame, app),
        Screen::Chat => render_chat_lifted(frame, app),
        Screen::Disconnected => disconnected::render(frame, app),
    }

    if let Some(p) = &app.pending_permission {
        permission_modal::render(frame, p);
    }
}

/// Compose lifted chat + input + footer for the Chat view, plus the
/// optional todo and help overlays. Layout (top to bottom):
/// chat | todo | input | help | footer.
fn render_chat_lifted(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let todo_height = lifted::todo::compute_height(app);
    let help_height = lifted::help::compute_height(app, area.width);

    let mut constraints: Vec<Constraint> = Vec::with_capacity(5);
    constraints.push(Constraint::Min(3)); // chat body
    if todo_height > 0 {
        constraints.push(Constraint::Length(todo_height));
    }
    constraints.push(Constraint::Length(2)); // input (lifted is two rows)
    if help_height > 0 {
        constraints.push(Constraint::Length(help_height));
    }
    constraints.push(Constraint::Length(2)); // footer (lifted footer is two rows)

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut idx = 0;
    let chat_area = chunks[idx];
    idx += 1;
    lifted::chat::render(frame, chat_area, app);
    if todo_height > 0 {
        lifted::todo::render(frame, chunks[idx], app);
        idx += 1;
    }
    let input_area = chunks[idx];
    idx += 1;
    lifted::input::render(frame, input_area, app);
    if help_height > 0 {
        lifted::help::render(frame, chunks[idx], app);
        idx += 1;
    }
    let footer_area = chunks[idx];
    lifted::footer::render(frame, footer_area, app);
}
