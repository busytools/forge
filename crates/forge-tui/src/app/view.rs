use crate::app::App;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Chat,
    Config,
    Trusted,
    SessionPicker,
    /// Project picker shown when forge is invoked without an argv
    /// project, or when the user runs `/launchpad` mid-session. The
    /// launchpad is the floor of the UI — `Esc` is a no-op while
    /// it's up; the user picks a project (transitioning to `Chat`)
    /// or quits with `Ctrl+Q`.
    Launchpad,
    /// Full-screen diff overlay for reviewing changes with inline
    /// comments. Opened by `/diff [target]` or the Inspector GIT
    /// section's `⤢` click. `Esc` closes (and, once wired,
    /// one-shot-submits any pending comments).
    Diff,
}

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
    *app.selection_mut() = None;
    app.scrollbar_drag = None;
    *app.active_paste_session_mut() = None;
    *app.pending_paste_session_mut() = None;
    app.pending_paste_text_mut().clear();
    *app.pending_submit_mut() = None;
    app.help_open = false;
    app.help_view = crate::app::HelpView::default();
    app.help_dialog = crate::app::dialog::DialogState::default();
    app.help_visible_count = 0;
    *app.mention_mut() = None;
    *app.slash_mut() = None;
    *app.subagent_mut() = None;
    if app.active_view == ActiveView::Config {
        app.config.overlay = None;
    }
    if app.active_view == ActiveView::Diff {
        app.diff_overlay = None;
    }
    app.release_focus_target(crate::app::FocusTarget::Help);
    app.release_focus_target(crate::app::FocusTarget::Mention);
    app.paste_burst.on_non_char_key(Instant::now());
}

#[cfg(test)]
mod tests;
