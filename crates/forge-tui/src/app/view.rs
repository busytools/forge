use crate::app::App;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Chat,
    /// Project picker shown when forge is invoked without an argv
    /// project, or when the user runs `/launchpad` mid-session. The
    /// launchpad is the floor of the UI - `Esc` is a no-op while
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
    /// Full-screen token/cost overlay. Opened by `/usage`. `g` toggles
    /// grouping, `w` cycles the window, `↑↓` scroll, `Esc` closes.
    Usage,
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
    app.emoji = None;
    if matches!(app.active_view, ActiveView::Plugins | ActiveView::Mcp) {
        app.config.overlay = None;
    }
    if app.active_view == ActiveView::Diff {
        // Confirmed-persisted review threads (and hydrated history) are
        // durable in redb, so this forced transition (e.g. a session swap
        // mid-review) can't lose them. At risk here: any session-authored
        // comment whose write was skipped or failed (in any scope) plus an
        // in-progress editor's unsent text. Warn so those stay greppable.
        let dropped_at_risk = app.diff_overlay.as_ref().map_or(0, |o| {
            o.comments.iter().filter(|c| c.authored_this_session && !c.persisted).count()
        });
        let had_in_progress_editor =
            app.diff_overlay.as_ref().is_some_and(|o| o.active_input.is_some());
        if dropped_at_risk > 0 || had_in_progress_editor {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_force_cleared",
                message = "diff overlay cleared via view transition; persisted review threads survive, unpersisted comments/editor dropped",
                outcome = "dropped",
                dropped_at_risk,
                had_in_progress_editor,
            );
        }
        app.diff_overlay = None;
    }
    if app.active_view == ActiveView::Usage {
        app.usage_overlay = None;
    }
    app.release_focus_target(crate::app::FocusTarget::Help);
    app.release_focus_target(crate::app::FocusTarget::Mention);
    app.paste_burst.on_non_char_key(Instant::now());
}

#[cfg(test)]
mod tests;
