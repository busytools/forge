mod autocomplete;
mod chat;
mod chat_view;
mod config;
mod diff;
mod diff_overlay;
mod document_table;
pub(crate) mod format;
pub(crate) mod help;
mod highlight;
mod input;
pub mod inspector_pane;
pub mod launchpad;
pub(crate) mod layout;
mod markdown;
mod message;
pub mod projects_pane;
mod session_picker;
pub mod theme;
mod tool_call;
pub mod top_bar;
mod two_column_list;
mod wrap;

pub use message::{SpinnerState, measure_message_height_cached};

use crate::app::ActiveView;
use crate::app::App;
use ratatui::Frame;

pub fn render(frame: &mut Frame, app: &mut App) {
    match app.active_view {
        ActiveView::Chat => chat_view::render(frame, app),
        ActiveView::Config => config::render(frame, app),
        ActiveView::SessionPicker => session_picker::render(frame, app),
        ActiveView::Launchpad => launchpad::render(frame, app),
        ActiveView::Diff => diff_overlay::render(frame, app),
    }
}

pub(crate) fn refresh_selection_snapshot(app: &mut App) {
    let Some(selection) = app.selection() else {
        return;
    };

    match (app.active_view, selection.kind) {
        (ActiveView::Chat, crate::app::SelectionKind::Chat) => {
            chat::refresh_selection_snapshot(app);
        }
        (ActiveView::Chat, crate::app::SelectionKind::Input) => {
            input::refresh_selection_snapshot(app);
        }
        _ => {}
    }
}
