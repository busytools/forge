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
use forge_workspace::env::git_diff::hunks::ScanOutcome;

use super::App;
use super::view::{ActiveView, set_active_view};

/// Event shuttled from the spawned scan task back to the main loop.
/// `cwd` and `target` are echoed back so the receiver can drop
/// stale results when the user switched sessions or navigated away
/// from chat while the scan was running (see [`drain_events`]).
/// `scanner_ok` propagates from `ScanOutcome::scanner_ok` so the
/// renderer can surface "scan failed" vs. "no changes" distinctly.
#[derive(Debug)]
pub struct DiffOverlayEvent {
    pub cwd: PathBuf,
    pub target: String,
    pub files: Vec<FileHunks>,
    pub scanner_ok: bool,
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
        let ScanOutcome { files, scanner_ok } = workspace.scan_git_diff_hunks(&cwd, &target).await;
        let _ = tx.send(DiffOverlayEvent { cwd, target, files, scanner_ok });
    });
}

/// Outcome of resolving the default `/diff` target from the active
/// session's Inspector GIT snapshot. Distinguishes every "nothing
/// to open" case so the caller can surface a specific system-
/// message rather than collapsing distinct failures onto a single
/// "no changes" line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultTarget {
    /// Resolved a concrete ref to diff against (`"HEAD"` for the
    /// worktree case, the default branch name for the clean
    /// feature-branch case).
    Ref(String),
    /// Inspector GIT scanner hasn't produced a snapshot yet. The
    /// poll fires ~10s after session start; a fresh-launch user
    /// who hits `/diff` immediately can land here.
    NoSnapshot,
    /// Active session's cwd isn't inside a git repository.
    NotARepo,
    /// Snapshot view is `BranchVsDefault` (so the scanner sees
    /// committed changes vs SOME default) but the default branch
    /// itself couldn't be resolved — no `origin/HEAD`, no local
    /// `main`, no local `master`. Distinct from `Clean` because
    /// there ARE changes; we just don't know which ref to compare
    /// against. User needs to pass an explicit `/diff <ref>`.
    NoDefault,
    /// Working tree is clean against the resolved default branch.
    /// Genuine "no changes". Branch name is surfaced in the
    /// system notice when known so the user knows what they're
    /// (not) diffing against.
    Clean { default_branch: Option<String> },
}

/// Resolve the default `/diff` target from the active session's
/// Inspector GIT snapshot. Mirrors the auto-detect logic the `/diff`
/// slash command uses; shared with the Inspector `⤢` click path.
pub fn resolve_default_target(app: &App) -> DefaultTarget {
    use forge_workspace::env::git_diff::GitDiffView;
    let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()) else {
        return DefaultTarget::NoSnapshot;
    };
    match (&snapshot.view, snapshot.default_branch.as_deref()) {
        (GitDiffView::Worktree { .. }, _) => DefaultTarget::Ref("HEAD".to_owned()),
        (GitDiffView::BranchVsDefault { .. }, Some(default)) => {
            DefaultTarget::Ref(default.to_owned())
        }
        (GitDiffView::BranchVsDefault { .. }, None) => DefaultTarget::NoDefault,
        (GitDiffView::NoRepo, _) => DefaultTarget::NotARepo,
        (GitDiffView::CleanDefault, _) => {
            DefaultTarget::Clean { default_branch: snapshot.default_branch.clone() }
        }
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
/// kick off a scan. Pushes a distinct system notice on each of the
/// "nothing to open" cases so the user sees something actionable
/// instead of a generic "no changes". Shared entry point for the
/// `/diff` slash command (no arg) and the Inspector `⤢` click.
pub fn open_default(app: &mut App) {
    match resolve_default_target(app) {
        DefaultTarget::Ref(target) => open_with_target(app, target),
        DefaultTarget::NoSnapshot => {
            crate::app::slash::push_system_message(
                app,
                "Git scanner hasn't run yet — try /diff again in a moment.",
            );
        }
        DefaultTarget::NotARepo => {
            crate::app::slash::push_system_message(app, "Not a git repository.");
        }
        DefaultTarget::NoDefault => {
            crate::app::slash::push_system_message(
                app,
                "Branch has changes but the default ref couldn't be resolved (no origin/HEAD, no main, no master). Run /diff <ref> with an explicit target.",
            );
        }
        DefaultTarget::Clean { default_branch } => {
            let message = match default_branch {
                Some(name) => format!("No changes vs {name}."),
                None => "No changes vs HEAD.".to_owned(),
            };
            crate::app::slash::push_system_message(app, message);
        }
    }
}

/// Max events drained per main-loop tick. At most one scan is in
/// flight per `/diff` invocation in practice, but the bounded loop
/// matches the established pattern in `app::git_diff::drain_events`
/// and `app::file_index::drain_events` so a stalled producer can't
/// block the render loop arbitrarily long.
const EVENT_DRAIN_BUDGET: usize = 8;

/// Drain pending scan results and install the overlay state. Called
/// from the main loop alongside the other event-channel consumers.
///
/// Events are dropped (rather than opening the overlay) when the
/// user has navigated away since the scan started:
/// - `app.active_view != ActiveView::Chat` — user opened config /
///   session picker / launchpad / another overlay while the scan
///   was running. Yanking them into the diff view would be
///   surprising.
/// - `event.cwd` doesn't match the active session's `cwd_raw` —
///   user switched sessions mid-scan; the result is for a stale
///   project.
///
/// Both cases log at DEBUG so a future "why didn't /diff open?"
/// triage can correlate the event without surfacing noise to the
/// user.
pub fn drain_events(app: &mut App) {
    for _ in 0..EVENT_DRAIN_BUDGET {
        let event = match app.diff_overlay_event_rx.try_recv() {
            Ok(event) => event,
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        if app.active_view != ActiveView::Chat {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_drain_skipped_view",
                message = "diff scan completed but active view changed; dropping result",
                outcome = "skipped",
                target_ref = %event.target,
                active_view = ?app.active_view,
            );
            crate::app::slash::push_system_message(
                app,
                format!(
                    "Diff scan for `{}` finished after you navigated away — run /diff again to view.",
                    event.target
                ),
            );
            continue;
        }
        // PathBuf comparison normalises trailing separators and
        // avoids the lossy String round-trip — `cwd_raw` is UTF-8
        // by construction, `event.cwd` is whatever the scanner
        // received, so converting `cwd_raw` to PathBuf gives an
        // exact match when they refer to the same directory.
        let active_cwd = app.active_session().map(|s| PathBuf::from(&s.cwd_raw));
        if active_cwd.as_deref() != Some(event.cwd.as_path()) {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_drain_skipped_cwd",
                message = "diff scan completed but session cwd changed; dropping result",
                outcome = "skipped",
                scan_cwd = %event.cwd.display(),
                active_cwd = ?active_cwd,
            );
            let notice = match active_cwd {
                Some(_) => format!(
                    "Diff scan for `{}` finished for a different session — run /diff again here.",
                    event.target
                ),
                None => format!(
                    "Diff scan for `{}` finished but the session closed — start a new session and re-run /diff.",
                    event.target
                ),
            };
            crate::app::slash::push_system_message(app, notice);
            continue;
        }
        let state = DiffOverlayState::new_with_status(
            event.cwd,
            event.target,
            event.files,
            event.scanner_ok,
        );
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
    /// Whether the scanner finished cleanly. `false` when one of
    /// the underlying `git` calls hit Failed / Oversize — the
    /// renderer surfaces a distinct empty-state message so the
    /// user knows to retry rather than concluding "no changes."
    pub scanner_ok: bool,
    /// Index into [`Self::files`] for the currently-viewed file in
    /// the right pane. Bounds-checked by [`Self::current_file`].
    pub current_file_idx: usize,
    /// Scroll offset (in lines) for the right pane's diff body.
    /// Resets to 0 when the user switches files. The FILES rail
    /// doesn't scroll yet — long file lists clip at the bottom of
    /// the rail; rail scrolling will be wired alongside body
    /// scrolling for the FILES side in a follow-up.
    pub body_scroll: u16,
}

impl DiffOverlayState {
    /// Build a fresh state for a newly-opened overlay. Cursor starts
    /// on file 0, scroll at 0. `scanner_ok = true` is the
    /// no-issues-known default; the spawn path sets it from
    /// `ScanOutcome::scanner_ok`.
    pub fn new(cwd: PathBuf, target: String, files: Vec<FileHunks>) -> Self {
        Self::new_with_status(cwd, target, files, true)
    }

    /// Build state and explicitly record whether the scanner ran
    /// cleanly. Used by the drain pump to thread the
    /// `DiffOverlayEvent::scanner_ok` flag into the rendered
    /// overlay.
    pub fn new_with_status(
        cwd: PathBuf,
        target: String,
        files: Vec<FileHunks>,
        scanner_ok: bool,
    ) -> Self {
        Self { cwd, target, files, scanner_ok, current_file_idx: 0, body_scroll: 0 }
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

/// Lines scrolled per wheel notch in the diff body. Same value as
/// `crate::app::events::mouse::MOUSE_SCROLL_LINES` (which is
/// `usize` because it feeds `Viewport::scroll_up/down`) — kept as
/// `u16` here because `body_scroll` is `u16` for ratatui's
/// `Paragraph::scroll`.
const SCROLL_LINES_PER_NOTCH: u16 = 3;

/// Wide-tier FILES rail width. Shared with the renderer.
pub(crate) const RAIL_WIDTH_WIDE: u16 = 40;
/// Medium-tier FILES rail width.
pub(crate) const RAIL_WIDTH_MEDIUM: u16 = 30;
/// Wide-tier terminal width threshold (≥ this → Wide).
pub(crate) const WIDE_MIN: u16 = 160;
/// Medium-tier terminal width threshold (≥ this → Medium).
pub(crate) const MEDIUM_MIN: u16 = 120;

/// First file row in the FILES rail. Rows above this are:
/// `0` banner (`FILES`), `1` DIM rule, `2` blank. File index 0
/// starts at `y == FIRST_FILE_ROW_Y`. The renderer at
/// `ui::diff_overlay::render_rail` chose this geometry; the click
/// handler uses it for the inverse mapping.
pub(crate) const FIRST_FILE_ROW_Y: u16 = 3;

/// Pick the FILES rail width for the current terminal width.
/// Returns `0` at Narrow tier (rail hidden). Shared with the
/// renderer at `crate::ui::diff_overlay::render` so the rail's
/// width and the click-handler's column threshold never drift.
pub(crate) fn rail_width_for(terminal_width: u16) -> u16 {
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
                overlay.body_scroll = overlay.body_scroll.saturating_sub(SCROLL_LINES_PER_NOTCH);
                true
            }
            MouseEventKind::ScrollDown => {
                overlay.body_scroll = overlay.body_scroll.saturating_add(SCROLL_LINES_PER_NOTCH);
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

    #[test]
    fn rail_width_picks_wide_at_160() {
        assert_eq!(rail_width_for(160), RAIL_WIDTH_WIDE);
        assert_eq!(rail_width_for(200), RAIL_WIDTH_WIDE);
    }

    #[test]
    fn rail_width_picks_medium_between_120_and_160() {
        assert_eq!(rail_width_for(120), RAIL_WIDTH_MEDIUM);
        assert_eq!(rail_width_for(159), RAIL_WIDTH_MEDIUM);
    }

    #[test]
    fn rail_width_collapses_at_narrow_tier() {
        assert_eq!(rail_width_for(119), 0);
        assert_eq!(rail_width_for(80), 0);
    }
}
