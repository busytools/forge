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

use crossterm::event::{KeyCode, KeyEvent};
use forge_workspace::env::git_diff::hunks::FileHunks;

use super::App;
use super::view::{ActiveView, set_active_view};

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
    #[must_use]
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
    #[must_use]
    pub fn current_file(&self) -> Option<&FileHunks> {
        self.files.get(self.current_file_idx)
    }
}

/// Install `state` on `app.diff_overlay` and transition the active
/// view to [`ActiveView::Diff`]. Marked `#[allow(dead_code)]` because
/// the entry points (`/diff` slash command + Inspector `⤢` click)
/// land in subsequent commits — once those are wired, this attribute
/// goes away.
#[allow(dead_code)]
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
}
