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
use forge_workspace::env::git_diff::hunks::ScanOutcome;
use forge_workspace::env::git_diff::hunks::{DiffLine, DiffLineKind, FileHunks};
use tui_textarea::TextArea;

use super::App;
use super::view::{ActiveView, set_active_view};

/// Identifies a single rendered diff line — `(file_idx, hunk_idx,
/// line_idx_in_hunk)`. Comments attach to a `LineKey`; the body
/// hit-test resolves a mouse y-coordinate to a key by walking the
/// rendered body line list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineKey {
    pub file_idx: usize,
    pub hunk_idx: usize,
    pub line_idx: usize,
}

/// What a single rendered row in the right pane corresponds to.
/// Built by the renderer alongside the `Vec<Line>` it returns, and
/// stashed on `DiffOverlayState` so the mouse handler can resolve a
/// click (`row, body_scroll`) → action without re-walking the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyRowKey {
    /// Banner row (`DIFF · <path>  +N -M`). Click does nothing in v1.
    Banner,
    /// The DIM `─` rule under the banner.
    Rule,
    /// A blank spacer line.
    Blank,
    /// Empty-state notice (scan failed / no file / binary / etc.).
    EmptyState,
    /// `@@ -A,B +C,D @@` hunk header — non-interactive in v1.
    HunkHeader { file_idx: usize, hunk_idx: usize },
    /// An actual diff line inside a hunk. Click → open a comment
    /// input anchored at this key.
    HunkLine(LineKey),
    /// The single-line summary chip showing a saved comment ("💬
    /// L<line>: ..."). Click → re-open the saved comment for edit.
    CommentChip(LineKey),
    /// Inline TextArea row for the currently-open comment editor.
    /// Multiple consecutive rows when the comment spans more than
    /// one visual line.
    InputRow(LineKey),
}

/// A saved per-line comment. `path` / `line` / `hunk_context` are
/// snapshotted at submit time so the markdown bundle stays stable
/// even if the user scrolls or switches files before pressing Esc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkComment {
    pub key: LineKey,
    pub path: String,
    /// Line number from the relevant side of the diff (new-file
    /// line for context / added, old-file line for removed).
    pub line: u32,
    /// Full hunk the comment is anchored on — included verbatim in
    /// the markdown bundle so the agent sees the local context.
    pub hunk_context: Vec<DiffLine>,
    pub comment_text: String,
}

/// Currently-active comment input. Mounts inline below the clicked
/// line. The editor is `tui_textarea::TextArea` so paste / cursor /
/// multi-line work without re-implementing input plumbing.
#[derive(Debug, Clone)]
pub struct ActiveCommentInput {
    pub key: LineKey,
    pub editor: TextArea<'static>,
}

/// Event shuttled from the spawned scan task back to the main loop.
/// `cwd` and `target` are echoed back so the receiver can drop
/// stale results when the user switched sessions or navigated away
/// from chat while the scan was running (see [`drain_events`]).
/// `scanner_ok` propagates from `ScanOutcome::scanner_ok` so the
/// renderer can surface "scan failed" vs. "no changes" distinctly.
/// `untracked_suppressed` carries the cap-overflow count so the
/// rail can show a "+N untracked suppressed" notice.
/// `seq` is the monotonic counter captured at spawn time; the
/// drain pump uses it to drop events from a superseded scan
/// (rapid second `/diff` before the first finishes).
#[derive(Debug)]
pub struct DiffOverlayEvent {
    pub cwd: PathBuf,
    pub target: String,
    pub files: Vec<FileHunks>,
    pub scanner_ok: bool,
    pub untracked_suppressed: usize,
    pub seq: u64,
}

/// Spawn a tokio local task that awaits
/// [`forge_workspace::Workspace::scan_git_diff_hunks`] and posts a
/// [`DiffOverlayEvent`] when the scan completes. Best-effort send —
/// receiver going away (app shutdown) just drops the result.
pub fn spawn_fetch(
    workspace: Arc<forge_workspace::Workspace>,
    cwd: PathBuf,
    target: String,
    seq: u64,
    tx: std_mpsc::Sender<DiffOverlayEvent>,
) {
    tokio::task::spawn_local(async move {
        let ScanOutcome { files, scanner_ok, untracked_suppressed } =
            workspace.scan_git_diff_hunks(&cwd, &target).await;
        let _ =
            tx.send(DiffOverlayEvent { cwd, target, files, scanner_ok, untracked_suppressed, seq });
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
    /// The Inspector scanner itself failed (subprocess crash,
    /// timeout, oversize output). Distinct from `NotARepo` because
    /// the user IS in a repo; git just couldn't run. The snapshot
    /// collapses to `view = NoRepo` as a render failsafe but
    /// `scanner_ok=false` signals the real story.
    ScannerFailed,
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
    // Inspector scanner crashed — distinct from "not a repo".
    // Check before matching view so the failsafe NoRepo doesn't
    // mask a real subprocess failure.
    if !snapshot.scanner_ok {
        return DefaultTarget::ScannerFailed;
    }
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
    // Bump the seq before spawning so the new scan's events
    // outrank anything still in flight from an earlier /diff call.
    // Old events arriving on the channel after this bump will be
    // dropped by drain_events as superseded.
    app.diff_scan_seq = app.diff_scan_seq.wrapping_add(1);
    let seq = app.diff_scan_seq;
    spawn_fetch(workspace, PathBuf::from(cwd_raw), target, seq, app.diff_overlay_event_tx.clone());
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
        DefaultTarget::ScannerFailed => {
            crate::app::slash::push_system_message(
                app,
                "Git scanner hit an error — see tracing logs (target: agent.env_git). Try /diff again in a moment.",
            );
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
        // Superseded by a newer /diff invocation — silent drop.
        // No user notice because they didn't navigate away or
        // close anything; they just retriggered and the older
        // scan's result is no longer relevant.
        if event.seq != app.diff_scan_seq {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_drain_skipped_superseded",
                message = "diff scan completed after a newer /diff superseded it; dropping result",
                outcome = "skipped",
                target_ref = %event.target,
                event_seq = event.seq,
                latest_seq = app.diff_scan_seq,
            );
            continue;
        }
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
        let state = DiffOverlayState::new_with_event(event);
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
    /// Count of untracked files that were suppressed because the
    /// working tree exceeded `MAX_UNTRACKED_FILES` in the scanner.
    /// Zero when the tree was under the cap. Surfaced in the rail
    /// as a "+N untracked suppressed" row so a fresh-repo state
    /// doesn't render identically to a clean tree.
    pub untracked_suppressed: usize,
    /// Index into [`Self::files`] for the currently-viewed file in
    /// the right pane. Bounds-checked by [`Self::current_file`].
    pub current_file_idx: usize,
    /// Scroll offset (in lines) for the right pane's diff body.
    /// Resets to 0 when the user switches files.
    pub body_scroll: u16,
    /// Scroll offset (in lines) for the left FILES rail. Wheel
    /// events with the cursor over the rail advance this; the
    /// renderer clamps it against `max(0, file_count - visible)`.
    pub rail_scroll: u16,
    /// Saved comments, indexed by the order the user submitted
    /// them. Bundle-on-Esc walks this list to produce the markdown
    /// chat message.
    pub comments: Vec<HunkComment>,
    /// Active comment editor mounted inline below the clicked line,
    /// or `None` when nothing's being edited. Keys flow to this
    /// editor while it's open; Enter saves, Esc cancels.
    pub active_input: Option<ActiveCommentInput>,
    /// Parallel index to the renderer's body lines: for every row
    /// the right pane drew, what does that row represent. The mouse
    /// handler reads this to resolve a click into a `BodyRowKey`.
    /// Filled fresh on every render; consumers must NOT assume
    /// stability across frames.
    pub body_keys: Vec<BodyRowKey>,
    /// Row offset of the right pane's first line in screen
    /// coordinates (stashed at render time so the click handler can
    /// translate `mouse.row` → index into [`Self::body_keys`]).
    /// `0` until the first render; clicks before then miss safely.
    pub pane_origin_row: u16,
    /// Column at which the right pane starts on screen. The click
    /// handler uses this to gate clicks that fall in the rail or
    /// separator from the body hit-test path.
    pub pane_origin_col: u16,
    /// Width of the right pane (in columns) at last render. Used by
    /// the renderer to wrap the TextArea and by the click handler
    /// for column bound checks.
    pub pane_width: u16,
}

impl DiffOverlayState {
    /// Build a fresh state for a newly-opened overlay. Test-only —
    /// production uses [`Self::new_with_event`] so the scanner
    /// outcome flags (`scanner_ok`, `untracked_suppressed`) thread
    /// through from the underlying `ScanOutcome` and the renderer's
    /// failure / cap-overflow surfaces fire correctly. A non-test
    /// caller reaching for this constructor would silently lose
    /// both signals.
    #[cfg(test)]
    pub fn new(cwd: PathBuf, target: String, files: Vec<FileHunks>) -> Self {
        Self {
            cwd,
            target,
            files,
            scanner_ok: true,
            untracked_suppressed: 0,
            current_file_idx: 0,
            body_scroll: 0,
            rail_scroll: 0,
            comments: Vec::new(),
            active_input: None,
            body_keys: Vec::new(),
            pane_origin_row: 0,
            pane_origin_col: 0,
            pane_width: 0,
        }
    }

    /// Build state from a completed scan event, threading scanner
    /// outcome flags through to the overlay so the renderer can
    /// surface partial-failure and cap-overflow conditions.
    fn new_with_event(event: DiffOverlayEvent) -> Self {
        Self {
            cwd: event.cwd,
            target: event.target,
            files: event.files,
            scanner_ok: event.scanner_ok,
            untracked_suppressed: event.untracked_suppressed,
            current_file_idx: 0,
            body_scroll: 0,
            rail_scroll: 0,
            comments: Vec::new(),
            active_input: None,
            body_keys: Vec::new(),
            pane_origin_row: 0,
            pane_origin_col: 0,
            pane_width: 0,
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

/// Drop the overlay state and transition back to chat. The Esc-
/// bundle submit path lives in [`close_with_submit`] — call this
/// directly only when comments have already been handled (or the
/// caller is the Esc-cancel path for the active input editor).
pub(crate) fn close(app: &mut App) {
    app.diff_overlay = None;
    set_active_view(app, ActiveView::Chat);
    app.needs_redraw = true;
}

/// Handle a key while the diff overlay is active.
///
/// Routing depends on whether an inline comment editor is open:
/// - Editor open:
///   - `Esc` cancels the editor and returns focus to the diff.
///   - `Enter` (plain, no modifier) saves the edit.
///   - All other keys flow into the TextArea (typing, cursor
///     movement, paste-via-bracket, undo/redo, etc.).
/// - No editor open:
///   - `Esc` closes the overlay; pending comments are bundled into
///     a markdown chat message and submitted to the agent before
///     the close. The submit fires synchronously through
///     `input_submit::dispatch_diff_comment_bundle` so the user
///     sees the bubble appear immediately.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    let has_input = app.diff_overlay.as_ref().is_some_and(|o| o.active_input.is_some());
    if has_input {
        match key.code {
            KeyCode::Esc => cancel_active_input(app),
            KeyCode::Enter if !key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
                save_active_input(app);
            }
            _ => {
                if let Some(overlay) = app.diff_overlay.as_mut()
                    && let Some(input) = overlay.active_input.as_mut()
                {
                    input.editor.input(key);
                    app.needs_redraw = true;
                }
            }
        }
        return;
    }
    if matches!(key.code, KeyCode::Esc) {
        close_with_submit(app);
    }
}

/// Route bracketed paste into the active comment editor. Returns
/// `true` when the paste was consumed (editor present), `false`
/// otherwise so the caller can fall through. Plain pastes inside
/// the diff overlay outside a comment editor are dropped — there's
/// nothing for them to land on.
pub(crate) fn handle_paste(app: &mut App, text: &str) -> bool {
    let Some(overlay) = app.diff_overlay.as_mut() else { return false };
    let Some(input) = overlay.active_input.as_mut() else { return false };
    input.editor.insert_str(text);
    app.needs_redraw = true;
    true
}

/// Discard the active comment editor without saving.
fn cancel_active_input(app: &mut App) {
    if let Some(overlay) = app.diff_overlay.as_mut() {
        overlay.active_input = None;
        app.needs_redraw = true;
    }
}

/// Persist the active editor's text into [`DiffOverlayState::comments`]
/// and close the editor. The snapshot includes the anchor line's
/// hunk context so the markdown bundle stays stable even if the
/// user scrolls / switches files later.
fn save_active_input(app: &mut App) {
    let Some(overlay) = app.diff_overlay.as_mut() else { return };
    let Some(input) = overlay.active_input.take() else { return };
    let text = input.editor.lines().join("\n");
    if text.trim().is_empty() {
        // Treat Enter on an empty editor as cancel — saving a blank
        // comment would render a 💬 chip with nothing in it.
        app.needs_redraw = true;
        return;
    }
    // Resolve the line key into a snapshot. Files/hunks/lines may
    // theoretically have shifted on a re-scan, but inside one
    // overlay open the body is immutable, so the index is stable.
    let key = input.key;
    let Some(file) = overlay.files.get(key.file_idx) else {
        app.needs_redraw = true;
        return;
    };
    let Some(hunk) = file.hunks.get(key.hunk_idx) else {
        app.needs_redraw = true;
        return;
    };
    let Some(diff_line) = hunk.lines.get(key.line_idx) else {
        app.needs_redraw = true;
        return;
    };
    let line_no = match diff_line.kind {
        DiffLineKind::Removed => diff_line.old_line,
        DiffLineKind::Added | DiffLineKind::Context => diff_line.new_line,
    }
    .unwrap_or(0);
    let comment = HunkComment {
        key,
        path: file.path.clone(),
        line: line_no,
        hunk_context: hunk.lines.clone(),
        comment_text: text,
    };
    // Replace any existing comment at the same key (saving an
    // edited reopen).
    overlay.comments.retain(|c| c.key != key);
    overlay.comments.push(comment);
    app.needs_redraw = true;
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

/// Outcome of a mouse interaction. Some interactions need access
/// to the full App (key event needs to fire `dispatch_prompt` for
/// the Esc-bundle path) which the inner `handle_*` borrow doesn't
/// have — surface them as effects the outer `handle_mouse` runs.
#[derive(Debug, Default)]
struct MouseEffect {
    redraw: bool,
    /// Close the overlay (Esc-equivalent). Used by banner ✕ click.
    close: bool,
}

/// Handle a mouse event while the diff overlay is active.
///
/// Bindings (v1):
/// - Scroll wheel over the rail → advance `rail_scroll`.
/// - Scroll wheel over the body → advance `body_scroll`.
/// - Left click on a file row in the FILES rail → switch the right
///   pane to that file; resets `body_scroll` to 0.
/// - Left click on a diff line in the body → open an inline comment
///   input anchored at that line. (If an input is already open, the
///   click cancels it before opening the new one.)
/// - Left click on a saved-comment chip → re-open that comment for
///   editing.
/// - Left click on the banner `✕` → equivalent to Esc.
pub(crate) fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    let terminal_width = app.cached_frame_area.width;
    let effect = if let Some(overlay) = app.diff_overlay.as_mut() {
        match mouse.kind {
            MouseEventKind::ScrollUp => handle_scroll(overlay, mouse.column, terminal_width, false),
            MouseEventKind::ScrollDown => {
                handle_scroll(overlay, mouse.column, terminal_width, true)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                handle_left_click(overlay, mouse.column, mouse.row, terminal_width)
            }
            _ => MouseEffect::default(),
        }
    } else {
        MouseEffect::default()
    };
    if effect.redraw {
        app.needs_redraw = true;
    }
    if effect.close {
        close_with_submit(app);
    }
}

fn handle_scroll(
    overlay: &mut DiffOverlayState,
    column: u16,
    terminal_width: u16,
    down: bool,
) -> MouseEffect {
    let rail_width = rail_width_for(terminal_width);
    let in_rail = rail_width > 0 && column < rail_width;
    if in_rail {
        if down {
            overlay.rail_scroll = overlay.rail_scroll.saturating_add(SCROLL_LINES_PER_NOTCH);
        } else {
            overlay.rail_scroll = overlay.rail_scroll.saturating_sub(SCROLL_LINES_PER_NOTCH);
        }
    } else if down {
        overlay.body_scroll = overlay.body_scroll.saturating_add(SCROLL_LINES_PER_NOTCH);
    } else {
        overlay.body_scroll = overlay.body_scroll.saturating_sub(SCROLL_LINES_PER_NOTCH);
    }
    MouseEffect { redraw: true, close: false }
}

/// Resolve a left-click to an action. Returns the effect (redraw +
/// optional close-with-submit). Hits the rail, the pane body's
/// banner ✕, a diff line, a chip, or a hunk header in order.
fn handle_left_click(
    overlay: &mut DiffOverlayState,
    column: u16,
    row: u16,
    terminal_width: u16,
) -> MouseEffect {
    let rail_width = rail_width_for(terminal_width);
    // Rail click: column < rail_width → rail row hit-test.
    if rail_width > 0 && column < rail_width {
        return handle_rail_click(overlay, row);
    }
    // Body click: column past rail+separator. Resolve via body_keys.
    handle_body_click(overlay, column, row)
}

fn handle_rail_click(overlay: &mut DiffOverlayState, row: u16) -> MouseEffect {
    if row < FIRST_FILE_ROW_Y {
        return MouseEffect::default();
    }
    let offset = usize::from(row - FIRST_FILE_ROW_Y);
    let visible_idx = offset.checked_add(usize::from(overlay.rail_scroll));
    let Some(idx) = visible_idx else {
        return MouseEffect::default();
    };
    if idx >= overlay.files.len() {
        return MouseEffect::default();
    }
    if overlay.current_file_idx == idx {
        return MouseEffect::default();
    }
    overlay.current_file_idx = idx;
    overlay.body_scroll = 0;
    // Closing the active input on file switch keeps the inline
    // editor's geometry from anchoring to a row that's no longer
    // in the rendered body (the input belongs to a specific file).
    overlay.active_input = None;
    MouseEffect { redraw: true, close: false }
}

fn handle_body_click(overlay: &mut DiffOverlayState, column: u16, row: u16) -> MouseEffect {
    // Empty body_keys means the renderer hasn't drawn yet (or drew
    // the too-short fallback). A click before the first real render
    // can't resolve anything; drop it silently.
    if overlay.body_keys.is_empty() {
        return MouseEffect::default();
    }
    if row < overlay.pane_origin_row {
        return MouseEffect::default();
    }
    // Banner ✕ glyph: the renderer paints it on the banner row
    // (overlay.pane_origin_row) near the right edge after the file
    // path + +N -M totals. Geometry isn't exact (depends on path
    // length and totals), so we accept any banner-row click in the
    // final few columns as a close intent. This is a UI-stable
    // approximation: the close glyph is the only thing on the
    // banner that responds to clicks, so a far-right banner click
    // means "close" with no ambiguity.
    let local_row = row - overlay.pane_origin_row;
    let body_idx = usize::from(local_row).checked_add(usize::from(overlay.body_scroll));
    let Some(idx) = body_idx else {
        return MouseEffect::default();
    };
    let Some(key) = overlay.body_keys.get(idx).copied() else {
        return MouseEffect::default();
    };
    match key {
        BodyRowKey::Banner => {
            // Treat the rightmost cells of the banner row as the
            // close affordance. Padding (2 chars) keeps stray
            // clicks on the file-path tail from triggering close.
            let pane_end = overlay.pane_origin_col.saturating_add(overlay.pane_width);
            if column + 2 >= pane_end {
                return MouseEffect { redraw: true, close: true };
            }
            MouseEffect::default()
        }
        BodyRowKey::HunkLine(line_key) => open_input_for_key(overlay, line_key),
        BodyRowKey::CommentChip(line_key) => reopen_comment_for_key(overlay, line_key),
        BodyRowKey::Rule
        | BodyRowKey::Blank
        | BodyRowKey::EmptyState
        | BodyRowKey::HunkHeader { .. }
        | BodyRowKey::InputRow(_) => MouseEffect::default(),
    }
}

fn open_input_for_key(overlay: &mut DiffOverlayState, key: LineKey) -> MouseEffect {
    // If an editor is already open at the same key, no-op so the
    // click doesn't reset its in-progress text. If at a different
    // key, abandon the in-progress edit (UI matches what GitHub does
    // — clicking elsewhere closes the open editor without saving).
    if let Some(existing) = overlay.active_input.as_ref()
        && existing.key == key
    {
        return MouseEffect::default();
    }
    let editor = TextArea::default();
    overlay.active_input = Some(ActiveCommentInput { key, editor });
    MouseEffect { redraw: true, close: false }
}

fn reopen_comment_for_key(overlay: &mut DiffOverlayState, key: LineKey) -> MouseEffect {
    // Find the saved comment, hydrate a fresh TextArea from its
    // text, drop the saved entry so the chip vanishes (the user is
    // editing it again — committing puts it back, cancelling leaves
    // it gone, matching GitHub's edit semantics).
    let position = overlay.comments.iter().position(|c| c.key == key);
    let Some(pos) = position else {
        return MouseEffect::default();
    };
    let comment = overlay.comments.remove(pos);
    let mut editor = TextArea::default();
    // TextArea::insert_str respects newlines correctly so the
    // multi-line shape of the saved comment is preserved.
    editor.insert_str(&comment.comment_text);
    overlay.active_input = Some(ActiveCommentInput { key: comment.key, editor });
    MouseEffect { redraw: true, close: false }
}

/// Close the overlay; if there are pending comments, bundle them
/// into a markdown user message and dispatch it as a prompt before
/// closing. Used by the banner ✕ click and by `handle_key`'s Esc
/// path.
pub(super) fn close_with_submit(app: &mut App) {
    let comments: Vec<HunkComment> =
        app.diff_overlay.as_mut().map(|o| std::mem::take(&mut o.comments)).unwrap_or_default();
    if !comments.is_empty() {
        // Snapshot target + cwd from the overlay so we can build the
        // markdown header even after `close` drops the state.
        let (target, cwd_display) = app
            .diff_overlay
            .as_ref()
            .map(|o| (o.target.clone(), o.cwd.display().to_string()))
            .unwrap_or_default();
        let markdown = format_diff_comments(&target, &cwd_display, &comments);
        super::input_submit::dispatch_diff_comment_bundle(app, markdown);
    }
    close(app);
}

/// Build the markdown bundle for a set of pending comments. Public
/// for the Esc-submit path and the test suite.
pub(crate) fn format_diff_comments(
    target: &str,
    cwd_display: &str,
    comments: &[HunkComment],
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "## Diff review (target `{target}`)");
    out.push('\n');
    if !cwd_display.is_empty() {
        let _ = writeln!(out, "Repo: `{cwd_display}`");
        out.push('\n');
    }
    // Group comments by file path while preserving the order the
    // user added them (first appearance of a path wins for ordering).
    let mut order: Vec<String> = Vec::new();
    let mut by_file: std::collections::HashMap<String, Vec<&HunkComment>> =
        std::collections::HashMap::new();
    for c in comments {
        if !by_file.contains_key(&c.path) {
            order.push(c.path.clone());
        }
        by_file.entry(c.path.clone()).or_default().push(c);
    }
    for path in &order {
        let _ = writeln!(out, "### `{path}`");
        out.push('\n');
        for c in by_file.get(path).into_iter().flatten() {
            let _ = writeln!(out, "**Line {}**", c.line);
            out.push('\n');
            out.push_str("```diff\n");
            for line in &c.hunk_context {
                let marker = match line.kind {
                    DiffLineKind::Added => '+',
                    DiffLineKind::Removed => '-',
                    DiffLineKind::Context => ' ',
                };
                let _ = writeln!(out, "{marker}{}", line.text);
            }
            out.push_str("```\n\n");
            let _ = writeln!(out, "{}", c.comment_text.trim_end());
            out.push('\n');
        }
    }
    out
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
        let effect = handle_left_click(&mut state, 5, 4, 160);
        assert!(effect.redraw);
        assert!(!effect.close);
        assert_eq!(state.current_file_idx, 1);
        assert_eq!(state.body_scroll, 0);
    }

    #[test]
    fn rail_click_resets_body_scroll() {
        let mut state = sample_state();
        state.body_scroll = 12;
        let effect = handle_left_click(&mut state, 5, 4, 160);
        assert!(effect.redraw);
        assert_eq!(state.body_scroll, 0);
    }

    #[test]
    fn rail_click_outside_rail_routes_to_body() {
        // After the body hit-test was added, a click past the rail
        // routes into handle_body_click which finds no body_keys
        // in a freshly-constructed state — returns no-redraw.
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 50, 4, 160);
        assert!(!effect.redraw);
        assert!(!effect.close);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn rail_click_on_banner_returns_no_redraw() {
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 0, 160); // Banner row.
        assert!(!effect.redraw);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn rail_click_beyond_file_list_returns_no_redraw() {
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 99, 160); // No file at this row.
        assert!(!effect.redraw);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn rail_click_same_file_returns_no_redraw() {
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 3, 160); // Already on file 0.
        assert!(!effect.redraw);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn rail_click_at_narrow_tier_routes_to_body() {
        // Narrow tier: rail_width == 0 → click routes to body
        // hit-test, which finds no body_keys in a fresh state.
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 4, 100);
        assert!(!effect.redraw);
        assert_eq!(state.current_file_idx, 0);
    }

    #[test]
    fn body_click_opens_comment_input_on_hunk_line() {
        // Simulate a rendered body by stashing body_keys + pane
        // origin, then click into a HunkLine row.
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.body_keys = vec![
            BodyRowKey::Banner,
            BodyRowKey::Rule,
            BodyRowKey::Blank,
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkLine(key),
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41; // Past rail + separator on wide.
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 4, 160);
        assert!(effect.redraw);
        assert!(state.active_input.is_some());
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(key));
    }

    #[test]
    fn body_click_on_chip_reopens_saved_comment() {
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key,
            path: "a.rs".into(),
            line: 7,
            hunk_context: vec![],
            comment_text: "needs unwrap fix".into(),
        });
        state.body_keys = vec![
            BodyRowKey::Banner,
            BodyRowKey::Rule,
            BodyRowKey::Blank,
            BodyRowKey::CommentChip(key),
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 3, 160);
        assert!(effect.redraw);
        assert!(state.comments.is_empty(), "saved comment migrates back into the editor");
        let input = state.active_input.expect("editor reopened");
        assert_eq!(input.key, key);
        assert_eq!(input.editor.lines().join("\n"), "needs unwrap fix");
    }

    #[test]
    fn body_click_on_banner_far_right_triggers_close() {
        let mut state = sample_state();
        state.body_keys = vec![BodyRowKey::Banner];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119; // banner ends at col 160.
        // Click in the final 2 cols → close intent.
        let effect = handle_left_click(&mut state, 159, 0, 160);
        assert!(effect.redraw);
        assert!(effect.close);
    }

    #[test]
    fn body_click_on_banner_far_left_does_not_close() {
        let mut state = sample_state();
        state.body_keys = vec![BodyRowKey::Banner];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 0, 160);
        assert!(!effect.close);
    }

    #[test]
    fn format_diff_comments_groups_by_file_and_includes_hunk_context() {
        let comments = vec![
            HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
                path: "a.rs".into(),
                line: 12,
                hunk_context: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    text: "let x = unwrap_or_die();".into(),
                    old_line: None,
                    new_line: Some(12),
                }],
                comment_text: "use ? instead of unwrap_or_die".into(),
            },
            HunkComment {
                key: LineKey { file_idx: 1, hunk_idx: 0, line_idx: 1 },
                path: "b.rs".into(),
                line: 4,
                hunk_context: vec![DiffLine {
                    kind: DiffLineKind::Removed,
                    text: "panic!(\"unreachable\");".into(),
                    old_line: Some(4),
                    new_line: None,
                }],
                comment_text: "good, panic was unsafe".into(),
            },
            HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 1, line_idx: 0 },
                path: "a.rs".into(),
                line: 30,
                hunk_context: vec![],
                comment_text: "missing rationale".into(),
            },
        ];
        let md = format_diff_comments("HEAD", "/tmp/repo", &comments);
        assert!(md.contains("## Diff review (target `HEAD`)"));
        assert!(md.contains("Repo: `/tmp/repo`"));
        // Same-file comments group under one `### `a.rs`` header.
        let header_count = md.matches("### `a.rs`").count();
        assert_eq!(header_count, 1, "a.rs comments share one heading");
        assert!(md.contains("### `b.rs`"));
        assert!(md.contains("**Line 12**"));
        assert!(md.contains("**Line 30**"));
        assert!(md.contains("+let x = unwrap_or_die();"));
        assert!(md.contains("-panic!(\"unreachable\");"));
        assert!(md.contains("use ? instead of unwrap_or_die"));
    }

    #[test]
    fn format_diff_comments_empty_input_still_includes_header() {
        let md = format_diff_comments("main", "", &[]);
        assert!(md.contains("## Diff review (target `main`)"));
        assert!(!md.contains("Repo: ``"), "blank cwd suppresses the Repo line");
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
