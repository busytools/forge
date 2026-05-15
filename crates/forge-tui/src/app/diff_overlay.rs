//! Full-screen diff overlay state + keyboard handling.
//!
//! The overlay is the floor of the `/diff` flow: a snapshot of
//! file-level hunks fetched via
//! [`forge_workspace::Workspace::scan_git_diff_hunks`] rendered as
//! a two-pane layout. See [`crate::ui::diff_overlay`] for the
//! renderer; this module owns the transient state and the key
//! dispatch (Esc closes; comment-input keys will land in a later
//! commit).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use forge_workspace::env::git_diff::hunks::FileHunks;

use super::App;
use super::view::{ActiveView, set_active_view};

/// Event shuttled from the spawned scan task back to the main loop.
/// `cwd` and `target` are echoed back so the receiver doesn't have
/// to track outstanding requests — when the user switches away
/// before the scan completes, `drain_events` simply skips the open
/// (active view changed).
#[derive(Debug)]
pub struct DiffOverlayEvent {
    pub cwd: PathBuf,
    pub target: String,
    pub files: Vec<FileHunks>,
}

/// Spawn a tokio local task that awaits
/// [`forge_workspace::Workspace::scan_git_diff_hunks`] and posts a
/// [`DiffOverlayEvent`] when the scan completes. Best-effort send —
/// receiver going away (app shutdown) just drops the result.
pub fn spawn_fetch(
    workspace: Arc<forge_workspace::Workspace>,
    cwd: PathBuf,
    target: String,
    tx: std_mpsc::Sender<DiffOverlayEvent>,
) {
    tokio::task::spawn_local(async move {
        let files = workspace.scan_git_diff_hunks(&cwd, &target).await;
        let _ = tx.send(DiffOverlayEvent { cwd, target, files });
    });
}

/// Resolve the default `/diff` target from the active session's
/// Inspector GIT snapshot. Mirrors the auto-detect logic the `/diff`
/// slash command uses; shared with the Inspector `⤢` click path.
///
/// Returns `None` when there's nothing to diff (no snapshot, no
/// repo, clean default branch). Callers render a "No changes"
/// notice and skip opening the overlay in that case.
pub fn resolve_default_target(app: &App) -> Option<String> {
    use forge_workspace::env::git_diff::GitDiffView;
    let snapshot = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref())?;
    match (&snapshot.view, snapshot.default_branch.as_deref()) {
        (GitDiffView::Worktree { .. }, _) => Some("HEAD".to_owned()),
        (GitDiffView::BranchVsDefault { .. }, Some(default)) => Some(default.to_owned()),
        _ => None,
    }
}

/// Kick off a diff scan against `target` and post the result
/// through the overlay event channel. Pushes a system message
/// (via `app::slash::push_system_message`) on every failure path —
/// workspace not ready, no active session, empty cwd — so callers
/// don't need to handle that themselves. Used by `/diff <target>`
/// directly; `open_default` builds on top of it for the auto-detect
/// path.
pub fn open_with_target(app: &mut App, target: String) {
    let Some(workspace) = app.workspace.clone() else {
        crate::app::slash::push_system_message(app, "Cannot open diff: workspace not ready.");
        return;
    };
    let Some(cwd_raw) = app.active_session().map(|s| s.cwd_raw.clone()) else {
        crate::app::slash::push_system_message(app, "Cannot open diff: no active session.");
        return;
    };
    if cwd_raw.is_empty() {
        crate::app::slash::push_system_message(app, "Cannot open diff: active session has no cwd.");
        return;
    }
    spawn_fetch(workspace, PathBuf::from(cwd_raw), target, app.diff_overlay_event_tx.clone());
}

/// Auto-detect the diff target from the Inspector GIT snapshot and
/// kick off a scan. Pushes "No changes" system notice when the
/// snapshot has nothing to surface. Shared entry point for the
/// `/diff` slash command (no arg) and the Inspector `⤢` click.
pub fn open_default(app: &mut App) {
    let Some(target) = resolve_default_target(app) else {
        crate::app::slash::push_system_message(app, "No changes vs HEAD.");
        return;
    };
    open_with_target(app, target);
}

/// Max events drained per main-loop tick. At most one scan is in
/// flight per `/diff` invocation in practice, but the bounded loop
/// matches the established pattern in `app::git_diff::drain_events`
/// and `app::file_index::drain_events` so a stalled producer can't
/// block the render loop arbitrarily long.
const EVENT_DRAIN_BUDGET: usize = 8;

/// Drain pending scan results and install the overlay state. Called
/// from the main loop alongside the other event-channel consumers.
pub fn drain_events(app: &mut App) {
    for _ in 0..EVENT_DRAIN_BUDGET {
        let event = match app.diff_overlay_event_rx.try_recv() {
            Ok(event) => event,
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        let state = DiffOverlayState::new(event.cwd, event.target, event.files);
        open(app, state);
    }
}

/// All state the diff overlay view needs. Lives on
/// `App.diff_overlay` (`Option<Self>`) — `Some` while the view is
/// active, dropped to `None` on close so a stale snapshot can't
/// leak into the next open.
#[derive(Debug, Clone)]
pub struct DiffOverlayState {
    /// Project root the scan was run against. Resolves relative
    /// paths inside hunks and labels the overlay so the user knows
    /// which project they're reviewing.
    pub cwd: PathBuf,
    /// Diff target passed to `git diff` (`"HEAD"`, branch name,
    /// SHA). Kept so the renderer can display it in the right-pane
    /// banner alongside the file path.
    pub target: String,
    /// Files in the diff, in the order the scanner returned them.
    pub files: Vec<FileHunks>,
    /// Index into [`Self::files`] for the currently-viewed file in
    /// the right pane. Bounds-checked by [`Self::current_file`].
    pub current_file_idx: usize,
    /// Scroll offset (in lines) for the left rail. Independent of
    /// the right pane.
    pub rail_scroll: u16,
    /// Scroll offset (in lines) for the right pane's diff body.
    /// Resets to 0 when the user switches files.
    pub body_scroll: u16,
}

impl DiffOverlayState {
    /// Build a fresh state for a newly-opened overlay. Cursor starts
    /// on file 0, both scroll offsets at 0.
    pub fn new(cwd: PathBuf, target: String, files: Vec<FileHunks>) -> Self {
        Self {
            cwd,
            target,
            files,
            current_file_idx: 0,
            rail_scroll: 0,
            body_scroll: 0,
        }
    }

    /// Currently-viewed file, or `None` when the diff is empty.
    pub fn current_file(&self) -> Option<&FileHunks> {
        self.files.get(self.current_file_idx)
    }
}

/// Install `state` on `app.diff_overlay` and transition the active
/// view to [`ActiveView::Diff`]. Wired up by the `/diff` slash
/// command's drain pump; the Inspector `⤢` click reuses the same
/// path in a follow-up commit.
pub(crate) fn open(app: &mut App, state: DiffOverlayState) {
    app.diff_overlay = Some(state);
    set_active_view(app, ActiveView::Diff);
    app.needs_redraw = true;
}

/// Drop the overlay state and transition back to chat. Pending
/// comments + one-shot submit land in a later commit; this stub
/// just dismisses the view.
pub(crate) fn close(app: &mut App) {
    app.diff_overlay = None;
    set_active_view(app, ActiveView::Chat);
    app.needs_redraw = true;
}

/// Handle a key while the diff overlay is active.
///
/// Bindings (v1):
/// - `Esc` — close the overlay and return to chat. With pending
///   comments the close will eventually bundle + submit them in one
///   shot; today it just dismisses.
///
/// Other keys are intentionally consumed and ignored so the chat
/// input behind the overlay can't accidentally pick them up.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Esc) {
        close(app);
    }
}

/// Lines scrolled per wheel notch in the diff body. Matches the
/// Inspector pane's `MOUSE_SCROLL_LINES` for consistency.
const SCROLL_LINES_PER_NOTCH: u16 = 3;

/// First file row in the FILES rail. Rows above this are:
/// `0` banner (`  FILES`), `1` DIM rule, `2` blank. File index 0
/// starts at `y == FIRST_FILE_ROW_Y`.
const FIRST_FILE_ROW_Y: u16 = 3;

/// Wide-tier FILES rail width (mirrors `ui::diff_overlay::RAIL_WIDTH_WIDE`).
const RAIL_WIDTH_WIDE: u16 = 40;
/// Medium-tier FILES rail width.
const RAIL_WIDTH_MEDIUM: u16 = 30;
/// Wide-tier terminal width threshold.
const WIDE_MIN: u16 = 160;
/// Medium-tier terminal width threshold.
const MEDIUM_MIN: u16 = 120;

/// Mirror of `ui::diff_overlay::rail_width_for` — duplicated here
/// so the click handler doesn't need to import the renderer module.
/// Same Wide/Medium/Narrow thresholds.
fn rail_width_for(terminal_width: u16) -> u16 {
    if terminal_width >= WIDE_MIN {
        RAIL_WIDTH_WIDE
    } else if terminal_width >= MEDIUM_MIN {
        RAIL_WIDTH_MEDIUM
    } else {
        0
    }
}

/// Handle a mouse event while the diff overlay is active.
///
/// Bindings (v1):
/// - Scroll wheel up / down → advance `body_scroll` by
///   [`SCROLL_LINES_PER_NOTCH`].
/// - Left click on a file row in the FILES rail → switch the right
///   pane to that file; resets `body_scroll` to 0.
///
/// Line clicks (commenting) land in a follow-up commit.
pub(crate) fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    let terminal_width = app.cached_frame_area.width;
    let changed = if let Some(overlay) = app.diff_overlay.as_mut() {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                overlay.body_scroll =
                    overlay.body_scroll.saturating_sub(SCROLL_LINES_PER_NOTCH);
                true
            }
            MouseEventKind::ScrollDown => {
                overlay.body_scroll =
                    overlay.body_scroll.saturating_add(SCROLL_LINES_PER_NOTCH);
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                handle_left_click(overlay, mouse.column, mouse.row, terminal_width)
            }
            _ => false,
        }
    } else {
        false
    };
    if changed {
        app.needs_redraw = true;
    }
}

/// Resolve a left-click to a file-rail action. Returns `true` when
/// the click hit a file row and the overlay state changed.
///
/// Hit geometry: rail occupies columns `0..rail_width`, file rows
/// start at row [`FIRST_FILE_ROW_Y`] (after banner + DIM rule +
/// blank). One row per file, no overflow handling yet (a long file
/// list extends below the visible area; clicks below it miss).
fn handle_left_click(
    overlay: &mut DiffOverlayState,
    column: u16,
    row: u16,
    terminal_width: u16,
) -> bool {
    let rail_width = rail_width_for(terminal_width);
    if rail_width == 0 {
        return false; // Narrow tier — rail is hidden.
    }
    if column >= rail_width {
        return false; // Click is in the separator or right pane.
    }
    if row < FIRST_FILE_ROW_Y {
        return false; // Banner / rule / blank.
    }
    let idx = usize::from(row - FIRST_FILE_ROW_Y);
    if idx >= overlay.files.len() {
        return false;
    }
    if overlay.current_file_idx == idx {
        return false; // No-op click.
    }
    overlay.current_file_idx = idx;
    overlay.body_scroll = 0;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_workspace::env::git_diff::hunks::FileStatus;

    fn sample_state() -> DiffOverlayState {
        DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![
                FileHunks { path: "a.rs".into(), status: FileStatus::Modified, hunks: vec![] },
                FileHunks { path: "b.rs".into(), status: FileStatus::Added, hunks: vec![] },
            ],
        )
    }

    #[test]
    fn new_sets_cursor_and_scrolls_to_zero() {
        let state = sample_state();
        assert_eq!(state.current_file_idx, 0);
        assert_eq!(state.rail_scroll, 0);
        assert_eq!(state.body_scroll, 0);
    }

    #[test]
    fn current_file_returns_indexed_file() {
        let mut state = sample_state();
        assert_eq!(state.current_file().map(|f| f.path.as_str()), Some("a.rs"));
        state.current_file_idx = 1;
        assert_eq!(state.current_file().map(|f| f.path.as_str()), Some("b.rs"));
    }

    #[test]
    fn current_file_is_none_when_idx_oob() {
        let mut state = sample_state();
        state.current_file_idx = 99;
        assert!(state.current_file().is_none());
    }

    #[test]
    fn current_file_is_none_on_empty_diff() {
        let state = DiffOverlayState::new(PathBuf::from("/tmp"), "HEAD".into(), vec![]);
        assert!(state.current_file().is_none());
    }

    #[test]
    fn rail_click_switches_current_file_at_wide_tier() {
        let mut state = sample_state();
        // Column inside rail (<40), row 4 = file index 1.
        let changed = handle_left_click(&mut state, 5, 4, 160);
        assert!(changed);
        assert_eq!(state.current_file_idx, 1);
        assert_eq!(state.body_scroll, 0);
    }

    #[test]
    fn rail_click_resets_body_scroll() {
        let mut state = sample_state();
        state.body_scroll = 12;
        let changed = handle_left_click(&mut state, 5, 4, 160);
        assert!(changed);
        assert_eq!(state.body_scroll, 0);
    }

    #[test]
    fn rail_click_outside_rail_returns_false() {
        let mut state = sample_state();
        let changed = handle_left_click(&mut state, 50, 4, 160); // Past Wide rail.
        assert!(!changed);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn rail_click_on_banner_returns_false() {
        let mut state = sample_state();
        let changed = handle_left_click(&mut state, 5, 0, 160); // Banner row.
        assert!(!changed);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn rail_click_beyond_file_list_returns_false() {
        let mut state = sample_state();
        let changed = handle_left_click(&mut state, 5, 99, 160); // No file at this row.
        assert!(!changed);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn rail_click_same_file_returns_false() {
        let mut state = sample_state();
        let changed = handle_left_click(&mut state, 5, 3, 160); // Already on file 0.
        assert!(!changed);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn rail_click_at_narrow_tier_returns_false() {
        let mut state = sample_state();
        let changed = handle_left_click(&mut state, 5, 4, 100); // Narrow — no rail.
        assert!(!changed);
        assert_eq!(state.current_file_idx, 0);
    }
}
