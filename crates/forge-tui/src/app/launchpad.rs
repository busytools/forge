//! Launchpad view state + keyboard handling.
//!
//! The launchpad is the floor of the UI, and the second of its two
//! views - [`crate::app::preflight`] runs first on every route (the
//! renderer lives in [`crate::ui::launchpad`]). It owns:
//!
//! - The selection cursor over the picker rows.
//! - The open-time anchor (`opened_at`) used to detect the first
//!   frame after the view opens (for the default row selection).
//!
//! The spinner glyph is driven by the live `App::spinner_style`
//! through the shared `ui::spinner` helper, so the launchpad reflects
//! the active style without snapshotting its own copy.
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::App;
use super::view::{ActiveView, set_active_view};

/// All state the launchpad view needs. Reset whenever the view
/// transitions to [`crate::app::ActiveView::Launchpad`] (boot OR
/// `/launchpad`).
#[derive(Debug, Clone)]
pub struct LaunchpadState {
    /// Index into the flat selectable row list (project rows only -
    /// org headers and tree-continuation rows are skipped). Defaults
    /// to the most-recently-active project's row if any project has
    /// session activity; otherwise row 0.
    pub selected_index: usize,
    /// Open-time anchor. Used to detect the first frame after the
    /// view opens so the initial selection can default to the most
    /// recently active project.
    pub opened_at: Instant,
    /// Scroll offset (flat-row units) for the project list when it
    /// overflows the picker box. Follows the selection so the
    /// highlighted project stays visible; reconciled each render via
    /// `reconcile_scroll`.
    pub scroll_offset: u16,
}

impl Default for LaunchpadState {
    fn default() -> Self {
        Self { selected_index: 0, opened_at: Instant::now(), scroll_offset: 0 }
    }
}

/// Reconcile the launchpad scroll offset so the selected flat row
/// stays inside the visible window. `selected_flat` is the flat-row
/// index of the selected project (org headers + project + error/worker
/// rows all count), `view` the visible row count, `total` the flat row
/// count, `cur` the current offset. Returns the new offset clamped to
/// `[0, total - view]`; content that fits the view stays at 0.
pub(crate) fn reconcile_scroll(selected_flat: usize, view: usize, total: usize, cur: u16) -> u16 {
    if view == 0 || total <= view {
        return 0;
    }
    let max_offset = total - view;
    let cur = usize::from(cur);
    let offset = if selected_flat < cur {
        selected_flat
    } else if selected_flat >= cur + view {
        selected_flat + 1 - view
    } else {
        cur
    };
    u16::try_from(offset.min(max_offset)).unwrap_or(u16::MAX)
}

/// Reset the launchpad state when entering the view. Selected index
/// defaults to 0; the render path picks a smarter "most recently
/// active" default the first time it has the project list.
///
/// Wired into `/launchpad` slash command execution; the boot-time
/// path builds the equivalent state inline in `create_app` so the
/// View transitions atomically with App construction.
pub(crate) fn open(app: &mut App) {
    app.launchpad =
        LaunchpadState { selected_index: 0, opened_at: Instant::now(), scroll_offset: 0 };
    set_active_view(app, ActiveView::Launchpad);
    app.needs_redraw = true;
}

/// Handle a key while the launchpad view is active. Returns `true`
/// when the key was consumed by the launchpad (the caller should
/// not fall through to other dispatch paths).
///
/// Bindings:
/// - `↑` / `k` - move selection up
/// - `↓` / `j` - move selection down
/// - `Enter` - open the highlighted project (transition to Chat)
/// - `r` - retry spawn when the highlighted row is Failed
/// - `?` - toggle the help overlay
/// - `Esc` - no-op (the launchpad is the floor)
/// - everything else - consumed silently
///
/// Slash autocomplete via `/` is intentionally omitted on the
/// launchpad - the picker has no input area to render the dropdown
/// above. The four launchpad-relevant commands are reachable
/// directly: `/help` ≡ `?`, `/quit` ≡ `Ctrl+Q`, `/config` and
/// `/plugins` are reachable after picking a project (or by
/// running `forge <project>` and using the slash autocomplete in
/// chat).
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
    // launchpad - the picker is the floor of the UI, so Esc has
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
    fn default_state_selects_first_row() {
        let state = LaunchpadState::default();
        assert_eq!(state.selected_index, 0);
        assert_eq!(state.scroll_offset, 0);
    }

    // `reconcile_scroll(selected_flat, view, total, cur)` keeps the
    // selected flat row inside `[offset, offset+view)` and clamps to
    // `[0, total-view]`; content that fits stays at 0.
    #[test]
    fn scroll_follows_selection_down() {
        // 20 rows, view 12, selecting flat row 15 from offset 0 -> 15-12+1.
        assert_eq!(reconcile_scroll(15, 12, 20, 0), 4);
    }

    #[test]
    fn scroll_follows_selection_up() {
        // selected above the window -> offset snaps to it
        assert_eq!(reconcile_scroll(2, 12, 20, 6), 2);
    }

    #[test]
    fn scroll_noop_when_visible() {
        assert_eq!(reconcile_scroll(5, 12, 20, 0), 0);
    }

    #[test]
    fn scroll_clamps_to_max() {
        // never scroll past the last full window (total - view)
        assert_eq!(reconcile_scroll(19, 12, 20, 0), 8);
    }

    #[test]
    fn no_scroll_when_content_fits() {
        assert_eq!(reconcile_scroll(9, 12, 10, 0), 0);
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
    fn open_switches_to_launchpad_view_and_resets_selection() {
        let mut app = App::test_default();
        app.active_view = ActiveView::Chat;
        // Move selection off zero to confirm `open` resets it.
        app.launchpad.selected_index = 7;
        open(&mut app);
        assert_eq!(app.active_view, ActiveView::Launchpad);
        assert_eq!(app.launchpad.selected_index, 0);
    }
}
