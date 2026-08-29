pub(crate) mod account_picker;
mod autocomplete;
pub(crate) mod chat;
pub(crate) mod chat_tree;
mod chat_view;
pub(crate) mod collapse;
mod config;
mod diff;
mod diff_overlay;
mod document_table;
pub(crate) mod format;
pub(crate) mod help;
pub(crate) mod highlight;
mod input;
pub(crate) mod inspector_pane;
pub mod launchpad;
pub(crate) mod layout;
pub(crate) mod markdown;
pub(crate) mod message;
pub(crate) mod page;
pub(crate) mod peer_block;
pub mod preflight;
pub mod projects_pane;
pub(crate) mod prompt;
pub(crate) mod schedule_format;
pub(crate) mod spinner;
pub(crate) mod spinner_picker;
pub(crate) mod theme;
mod tool_call;
pub mod top_bar;
mod two_column_list;
mod usage_overlay;
pub(crate) mod worker_status;
mod wrap;

pub use message::grouping;
#[cfg(any(test, feature = "testing"))]
pub use message::measure_message_height_cached;
pub use message::{SpinnerState, workflow_meta_fields};

use crate::app::ActiveView;
use crate::app::App;
use ratatui::Frame;

pub fn render(frame: &mut Frame, app: &mut App) {
    match app.active_view {
        ActiveView::Chat => chat_view::render(frame, app),
        ActiveView::Plugins => config::render_plugins(frame, app),
        ActiveView::Mcp => config::render_mcp(frame, app),
        ActiveView::Launchpad => {
            if crate::app::preflight::hand_over_when_ready(app) {
                launchpad::render(frame, app);
            } else {
                preflight::render(frame, app);
            }
        }
        ActiveView::Diff => diff_overlay::render(frame, app),
        ActiveView::Usage => usage_overlay::render(frame, app),
    }
    // Modal overlay drawn over whatever view rendered above.
    if app.spinner_picker.is_some() {
        let area = frame.area();
        spinner_picker::render(frame, area, app);
    }
    if app.account_picker.is_some() {
        let area = frame.area();
        account_picker::render(frame, area, app);
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
