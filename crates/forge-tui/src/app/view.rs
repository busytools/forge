use crate::app::App;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Chat,
    /// Project picker shown when forge is invoked without an argv
    /// project, or when the user runs `/launchpad` mid-session. The
    /// launchpad is the floor of the UI  -  `Esc` is a no-op while
    /// it's up; the user picks a project (transitioning to `Chat`)
    /// or quits with `Ctrl+Q`.
    Launchpad,
    /// Full-screen diff overlay for reviewing changes with inline
    /// comments. Opened by `/diff [target]` or the Inspector GIT
    /// section's `🦉` click. `Esc` closes (and, once wired,
    /// one-shot-submits any pending comments).
    Diff,
    /// Full-screen Plugins view (installed plugins + marketplaces).
    /// Opened by `/plugins`. `Esc` closes back to chat.
    Plugins,
    /// Full-screen MCP server view. Opened by `/mcp`. `Esc` closes
    /// back to chat.
    Mcp,
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
    if matches!(app.active_view, ActiveView::Plugins | ActiveView::Mcp) {
        app.config.overlay = None;
    }
    if app.active_view == ActiveView::Diff {
        // Pending comments / in-progress editor are dropped on
        // indirect view transitions (e.g. session swap mid-review).
        // The normal Esc / banner ✕ paths go through
        // `close_with_submit` which bundles + submits comments
        // first; reaching THIS path means something external
        // forced the transition without giving the diff overlay
        // a chance to flush. Log so an operator can grep for it.
        let dropped_comments = app.diff_overlay.as_ref().map_or(0, |o| o.comments.len());
        let had_in_progress_editor =
            app.diff_overlay.as_ref().is_some_and(|o| o.active_input.is_some());
        if dropped_comments > 0 || had_in_progress_editor {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_force_cleared",
                message = "diff overlay cleared via view transition without close_with_submit; pending review state lost",
                outcome = "dropped",
                dropped_comments,
                had_in_progress_editor,
            );
        }
        app.diff_overlay = None;
    }
    app.release_focus_target(crate::app::FocusTarget::Help);
    app.release_focus_target(crate::app::FocusTarget::Mention);
    app.paste_burst.on_non_char_key(Instant::now());
}

#[cfg(test)]
mod tests;
