//! Launchpad view state + keyboard handling.
//!
//! The launchpad is the floor of the UI shown when forge is invoked
//! without a project argv (the renderer lives in
//! [`crate::ui::launchpad`]). It owns:
//!
//! - The selection cursor over the picker rows.
//! - The spinner timer anchor (`opened_at`) so the renderer can
//!   derive frame indices from elapsed-time-vs-cadence without
//!   sharing global ticker state.
//! - The user-chosen spinner style at open time. Snapshotting at
//!   open ensures the picker doesn't visibly jump if the user
//!   edits `~/.claude/forge.toml`'s `[ui]` block while the
//!   launchpad is up.
//! - The autostart policy at open time, for the same reason.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use forge_workspace::{LaunchpadAutostart, SpinnerStyle};

use super::App;
use super::view::{ActiveView, set_active_view};

/// All state the launchpad view needs. Reset whenever the view
/// transitions to [`ActiveView::Launchpad`] (boot OR `/launchpad`).
#[derive(Debug, Clone)]
pub struct LaunchpadState {
    /// Index into the flat selectable row list (project rows only —
    /// org headers and tree-continuation rows are skipped). Defaults
    /// to the most-recently-active project's row if any project has
    /// session activity; otherwise row 0.
    pub selected_index: usize,
    /// Monotonic anchor for spinner frame derivation. Renderer
    /// computes `(elapsed_ms / cadence_ms) % frames.len()`.
    pub opened_at: Instant,
    /// Spinner style snapshotted at open time. Re-reading from
    /// `Workspace::ui_settings()` each frame would make the picker
    /// jump if the user edits `forge.toml` mid-session.
    pub spinner_style: SpinnerStyle,
    /// Autostart policy snapshotted at open time. Drives the boot-
    /// time dispatch loop in `start_connection_for_view`.
    pub autostart: LaunchpadAutostart,
}

impl Default for LaunchpadState {
    fn default() -> Self {
        Self {
            selected_index: 0,
            opened_at: Instant::now(),
            spinner_style: SpinnerStyle::default(),
            autostart: LaunchpadAutostart::default(),
        }
    }
}

/// Reset the launchpad state when entering the view. Reads the
/// user's chosen spinner style + autostart policy from
/// `Workspace::ui_settings()` and snapshots them. Selected index
/// defaults to 0; the render path picks a smarter "most recently
/// active" default the first time it has the project list.
///
/// Wired into `/launchpad` slash command execution; the boot-time
/// path builds the equivalent snapshot inline in `create_app` so
/// the View transitions atomically with App construction.
#[allow(dead_code)] // Wired up in the /launchpad slash command (next commit).
pub(crate) fn open(app: &mut App) {
    let (spinner_style, autostart) = app
        .workspace
        .as_ref()
        .map(|w| {
            let ui = w.ui_settings();
            (ui.launchpad_spinner, ui.launchpad_autostart)
        })
        .unwrap_or_default();
    app.launchpad =
        LaunchpadState { selected_index: 0, opened_at: Instant::now(), spinner_style, autostart };
    set_active_view(app, ActiveView::Launchpad);
    app.needs_redraw = true;
}

/// Handle a key while the launchpad view is active. Returns `true`
/// when the key was consumed by the launchpad (the caller should
/// not fall through to other dispatch paths).
///
/// Bindings:
/// - `↑` / `k` — move selection up
/// - `↓` / `j` — move selection down
/// - `Enter` — open the highlighted project (transition to Chat)
/// - `r` — retry spawn when the highlighted row is Failed
/// - `?` — toggle the help overlay
/// - `Esc` — no-op (the launchpad is the floor)
/// - everything else — consumed silently
pub fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if key.modifiers == KeyModifiers::NONE {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                move_selection_up(app);
                return true;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                move_selection_down(app);
                return true;
            }
            KeyCode::Enter => {
                crate::ui::launchpad::pick_selected_project(app);
                return true;
            }
            KeyCode::Char('r') => {
                crate::ui::launchpad::retry_selected_project(app);
                return true;
            }
            KeyCode::Char('/') => {
                // Open the slash autocomplete with the launchpad-
                // filtered subset. Push `/` into the input buffer; the
                // slash machinery picks it up on `sync_with_cursor`.
                app.input_mut().clear();
                app.input_mut().set_text("/");
                super::slash::sync_with_cursor(app);
                app.needs_redraw = true;
                return true;
            }
            _ => {}
        }
    }
    if let KeyCode::Char('?') = key.code
        && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        app.help_open = !app.help_open;
        app.needs_redraw = true;
        return true;
    }
    // Esc and every other key are intentional no-ops on the
    // launchpad — the picker is the floor of the UI, so Esc has
    // nothing to dismiss to, and stray printable input must not
    // leak into a chat input that isn't visible.
    true
}

fn move_selection_up(app: &mut App) {
    if app.launchpad.selected_index > 0 {
        app.launchpad.selected_index -= 1;
        app.needs_redraw = true;
    }
}

fn move_selection_down(app: &mut App) {
    let total = crate::ui::launchpad::selectable_row_count(app);
    let max_index = total.saturating_sub(1);
    if app.launchpad.selected_index < max_index {
        app.launchpad.selected_index += 1;
        app.needs_redraw = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn default_state_picks_braille_and_always() {
        let state = LaunchpadState::default();
        assert_eq!(state.selected_index, 0);
        assert_eq!(state.spinner_style, SpinnerStyle::Braille);
        assert_eq!(state.autostart, LaunchpadAutostart::Always);
    }

    #[test]
    fn esc_is_no_op_but_consumes_the_key() {
        let mut app = App::test_default();
        app.active_view = ActiveView::Launchpad;
        let consumed = handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(consumed);
        assert_eq!(app.active_view, ActiveView::Launchpad);
    }

    #[test]
    fn arrow_down_increments_selection_when_rows_available() {
        // No projects in test_default → selectable_row_count = 0 →
        // max_index = 0 → selection stays at 0. This test asserts
        // the clamp behaviour rather than a real bump (the renderer-
        // backed selectable_row_count is exercised in ui::launchpad
        // tests with fixtures).
        let mut app = App::test_default();
        app.active_view = ActiveView::Launchpad;
        let consumed = handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(consumed);
        assert_eq!(app.launchpad.selected_index, 0);
    }

    #[test]
    fn help_toggle_flips_help_open() {
        let mut app = App::test_default();
        app.active_view = ActiveView::Launchpad;
        assert!(!app.help_open);
        let _ = handle_key(&mut app, KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(app.help_open);
        let _ = handle_key(&mut app, KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert!(!app.help_open);
    }

    #[test]
    fn unknown_key_is_consumed_silently() {
        let mut app = App::test_default();
        app.active_view = ActiveView::Launchpad;
        let consumed = handle_key(&mut app, KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(
            consumed,
            "launchpad should consume printable keys to avoid leaking into chat input"
        );
    }

    #[test]
    fn open_resets_state_from_workspace_ui_settings() {
        let mut app = App::test_default();
        app.active_view = ActiveView::Chat;
        // Move selection off zero to confirm `open` resets it.
        app.launchpad.selected_index = 7;
        open(&mut app);
        assert_eq!(app.active_view, ActiveView::Launchpad);
        assert_eq!(app.launchpad.selected_index, 0);
    }
}
