#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    clippy::disallowed_methods,
    clippy::while_let_loop,
    clippy::collapsible_if,
    reason = "lifted upstream from claude-code-rust"
)]

use crate::state::app::{ActiveView, App};
use crate::state::dialog;
use crate::state::focus::FocusTarget;
use crate::state::types::HelpView;
use std::time::Instant;

pub fn set_active_view(app: &mut App, next: ActiveView) {
    if app.active_view == next {
        return;
    }

    clear_transient_view_state(app);
    app.active_view = next;
    if next == ActiveView::Chat {
        app.rebuild_chat_focus_from_state();
    }
    app.needs_redraw = true;
}

fn clear_transient_view_state(app: &mut App) {
    app.selection = None;
    app.scrollbar_drag = None;
    app.active_paste_session = None;
    app.pending_paste_session = None;
    app.pending_paste_text.clear();
    app.pending_submit = None;
    app.help_open = false;
    app.help_view = HelpView::default();
    app.help_dialog = dialog::DialogState::default();
    app.help_visible_count = 0;
    // Upstream also clears `app.mention`, `app.slash`, `app.subagent`,
    // and `app.config.overlay` here. Those fields are deferred per the
    // forge-tui cuts list; reinstate the calls when the corresponding
    // modules lift.
    app.release_focus_target(FocusTarget::Help);
    app.release_focus_target(FocusTarget::Mention);
    app.paste_burst.on_non_char_key(Instant::now());
}
