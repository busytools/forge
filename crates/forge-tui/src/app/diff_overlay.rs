//! Full-screen diff overlay state + keyboard handling.
//!
//! The overlay is the floor of the `/diff` flow: a snapshot of
//! file-level hunks fetched via
//! [`forge_workspace::Workspace::scan_git_diff_hunks`] rendered as
//! a two-pane layout. See [`crate::ui::diff_overlay`] for the
//! renderer; this module owns the transient state and the key /
//! mouse dispatch.
//!
//! Key handling:
//! - With a comment editor open: Enter saves the text into
//!   [`DiffOverlayState::comments`] and closes the editor; Esc
//!   cancels the editor (restoring a saved comment if the editor
//!   was opened via re-clicking a chip).
//! - With no editor open: Esc bundles all pending comments into a
//!   single markdown chat message and dispatches it as a fresh
//!   prompt, then closes the overlay.
//!
//! Mouse handling: see [`handle_mouse`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
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

/// What a single rendered row in the left FILES rail corresponds
/// to. Built by `render_rail` and stashed on `DiffOverlayState` so
/// the mouse handler resolves a click (`row + rail_scroll`) into a
/// file index — or recognises the click as hitting non-interactive
/// chrome / a directory header — without re-walking the file list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailRowKey {
    /// `FILES` banner. Non-clickable.
    Banner,
    /// DIM rule under the banner. Non-clickable.
    Rule,
    /// Blank spacer. Non-clickable.
    Blank,
    /// Directory header in the tree (e.g. `crates/`,
    /// `forge-agent/src/env/`). Non-clickable in v1.
    Directory,
    /// File leaf — click switches the right pane to this file.
    File { file_idx: usize },
    /// `+N untracked suppressed (cap M)` notice row at the bottom
    /// of the rail when the scanner hit its untracked cap. Non-
    /// clickable.
    UntrackedNotice,
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
    /// A diff row in the split body. Carries both column keys —
    /// the click handler picks `left` or `right` by comparing the
    /// click column against the pane midpoint. At least one side
    /// is `Some` (the pairing algorithm never emits both-None).
    HunkRow { left: Option<LineKey>, right: Option<LineKey> },
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
///
/// `prior_comment` carries the saved comment when the editor was
/// opened by re-clicking an existing 💬 chip. On Esc-cancel, the
/// prior comment is restored to [`DiffOverlayState::comments`] so
/// a misclick on the chip + reflex Esc doesn't destroy the user's
/// review notes. `None` for fresh line-clicks where there's nothing
/// to restore.
#[derive(Debug, Clone)]
pub struct ActiveCommentInput {
    pub key: LineKey,
    pub editor: TextArea<'static>,
    pub prior_comment: Option<HunkComment>,
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
/// slash command uses; shared with the Inspector `🦉` click path.
///
/// Known race: the snapshot can be up to ~10 s stale because the
/// inspector's git-diff scanner polls on that cadence. If the user
/// switches branches and clicks `🦉` within that window, the resolved
/// target may not match the live working tree. Mitigation: the scan
/// itself ALWAYS runs fresh — only the *target ref* (e.g. `main` vs
/// `master`) can be wrong. Worst-case the user sees "no changes" and
/// reruns `/diff <ref>` explicitly. Not worth the synchronous
/// refresh cost on the click hot-path.
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
/// `/diff` slash command (no arg) and the Inspector `🦉` click.
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
/// Events are dropped (silently) when the user has navigated away
/// since the scan started:
/// - `app.active_view != ActiveView::Chat` — user opened config /
///   session picker / launchpad / another overlay while the scan
///   was running. Yanking them into the diff view would be
///   surprising.
/// - `event.cwd` doesn't match the active session's `cwd_raw` —
///   user switched sessions mid-scan; the result is for a stale
///   project, and crosstalking it into the new session would
///   confuse.
///
/// Both cases log at DEBUG so a future "why didn't /diff open?"
/// triage can correlate the event. No chat message is pushed —
/// the user explicitly navigated away, so a notice arriving later
/// would be noise. The user can rerun `/diff` if they want the
/// scan they kicked off.
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
            // Silent drop — the user explicitly navigated away, so
            // a chat message would be surprising noise. DEBUG log
            // remains for triage.
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_drain_skipped_view",
                message = "diff scan completed but active view changed; dropping result",
                outcome = "skipped",
                target_ref = %event.target,
                active_view = ?app.active_view,
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
            // Silent drop — pushing a chat message into the now-
            // active (different) session about a scan for the OLD
            // session would crosstalk. The user can rerun /diff
            // explicitly if they meant the new session.
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_drain_skipped_cwd",
                message = "diff scan completed but session cwd changed; dropping result",
                outcome = "skipped",
                scan_cwd = %event.cwd.display(),
                active_cwd = ?active_cwd,
            );
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
    /// Horizontal scroll offset (in columns) applied to both halves
    /// of the split diff body. Left/Right arrow keys advance and
    /// retreat this; resets to 0 when the user switches files. Both
    /// halves use the same offset so the split stays aligned.
    pub body_scroll_x: u16,
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
    /// Cached comment count per file index, indexed by file position
    /// in [`Self::files`]. Recomputed on every comment mutation via
    /// [`Self::recompute_comment_counts`]. Renderer reads it directly
    /// each frame so the hot path is O(1) per file row instead of
    /// O(comments) per render.
    pub comment_counts: Vec<u32>,
    /// Parallel index for the left FILES rail: for every rendered
    /// rail row, what does it represent. Click handler walks this
    /// (offset by `rail_scroll`) to resolve `mouse.row` → an action.
    /// Filled fresh on every render.
    pub rail_keys: Vec<RailRowKey>,
    /// Number of leading rows in `body_keys` that are pinned (not
    /// scrolled with `body_scroll`). Wide-tier sets this to
    /// `BODY_HEAD_ROWS` because the renderer keeps the banner +
    /// rule + blank pinned above the scrolling tail. The click
    /// handler reads it to decide whether `body_scroll` should
    /// offset a given row.
    pub body_head_rows: usize,
}

impl DiffOverlayState {
    /// Refresh [`Self::comment_counts`] after mutating
    /// [`Self::comments`]. Cheap (`O(comments)`) and only runs on
    /// save / cancel / file switch, not on render. Resets the
    /// per-file slot to zero before counting so removed comments
    /// don't linger.
    pub fn recompute_comment_counts(&mut self) {
        self.comment_counts.clear();
        self.comment_counts.resize(self.files.len(), 0);
        for c in &self.comments {
            if let Some(slot) = self.comment_counts.get_mut(c.key.file_idx) {
                *slot = slot.saturating_add(1);
            }
        }
    }

    /// Build a fresh state for a newly-opened overlay. Test-only —
    /// production uses [`Self::new_with_event`] so the scanner
    /// outcome flags (`scanner_ok`, `untracked_suppressed`) thread
    /// through from the underlying `ScanOutcome` and the renderer's
    /// failure / cap-overflow surfaces fire correctly. A non-test
    /// caller reaching for this constructor would silently lose
    /// both signals.
    #[cfg(test)]
    pub fn new(cwd: PathBuf, target: String, files: Vec<FileHunks>) -> Self {
        let file_count = files.len();
        Self {
            cwd,
            target,
            files,
            scanner_ok: true,
            untracked_suppressed: 0,
            current_file_idx: 0,
            body_scroll: 0,
            body_scroll_x: 0,
            rail_scroll: 0,
            comments: Vec::new(),
            active_input: None,
            body_keys: Vec::new(),
            pane_origin_row: 0,
            pane_origin_col: 0,
            pane_width: 0,
            comment_counts: vec![0; file_count],
            rail_keys: Vec::new(),
            body_head_rows: 0,
        }
    }

    /// Build state from a completed scan event, threading scanner
    /// outcome flags through to the overlay so the renderer can
    /// surface partial-failure and cap-overflow conditions.
    fn new_with_event(event: DiffOverlayEvent) -> Self {
        let file_count = event.files.len();
        Self {
            cwd: event.cwd,
            target: event.target,
            files: event.files,
            scanner_ok: event.scanner_ok,
            untracked_suppressed: event.untracked_suppressed,
            current_file_idx: 0,
            body_scroll: 0,
            body_scroll_x: 0,
            rail_scroll: 0,
            comments: Vec::new(),
            active_input: None,
            body_keys: Vec::new(),
            pane_origin_row: 0,
            pane_origin_col: 0,
            pane_width: 0,
            comment_counts: vec![0; file_count],
            rail_keys: Vec::new(),
            body_head_rows: 0,
        }
    }

    /// Currently-viewed file, or `None` when the diff is empty.
    pub fn current_file(&self) -> Option<&FileHunks> {
        self.files.get(self.current_file_idx)
    }
}

/// Install `state` on `app.diff_overlay` and transition the active
/// view to [`ActiveView::Diff`]. Wired up by the `/diff` slash
/// command's drain pump; the Inspector `🦉` click reuses the same
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
    match key.code {
        KeyCode::Esc => close_with_submit(app),
        KeyCode::Left => scroll_body_horizontal(app, -1),
        KeyCode::Right => scroll_body_horizontal(app, 1),
        _ => {}
    }
}

/// Step the diff body's horizontal scroll by `delta` columns
/// (`SCROLL_COLS_PER_STEP` per arrow press). Negative goes left,
/// positive goes right. Clamped at 0 on the left; the right has no
/// upper bound here because the renderer just truncates whatever
/// content extends past the available width.
fn scroll_body_horizontal(app: &mut App, delta: i32) {
    let Some(overlay) = app.diff_overlay.as_mut() else {
        return;
    };
    let step = i32::from(SCROLL_COLS_PER_STEP);
    let next = i32::from(overlay.body_scroll_x).saturating_add(delta.saturating_mul(step));
    let clamped = next.clamp(0, i32::from(u16::MAX));
    overlay.body_scroll_x = u16::try_from(clamped).unwrap_or(0);
    app.needs_redraw = true;
}

/// Columns advanced / retreated per Left / Right arrow press. 8 cols
/// is enough to reveal a typical token-or-two of context per press
/// without making short lines feel like they scroll forever.
const SCROLL_COLS_PER_STEP: u16 = 8;

/// Route bracketed paste into the active comment editor. Returns
/// `true` when the paste was consumed (editor present), `false`
/// otherwise so the caller can fall through. Plain pastes inside
/// the diff overlay outside a comment editor are dropped — there's
/// nothing for them to land on — but a DEBUG log fires so a user
/// reporting "my paste disappeared" can be triaged from logs.
pub(crate) fn handle_paste(app: &mut App, text: &str) -> bool {
    let Some(overlay) = app.diff_overlay.as_mut() else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_paste_dropped_no_overlay",
            message = "paste in Diff view without overlay state — dropped",
            outcome = "dropped",
            paste_chars = text.chars().count(),
        );
        return false;
    };
    let Some(input) = overlay.active_input.as_mut() else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_paste_dropped_no_editor",
            message = "paste in Diff view without an open comment editor — dropped",
            outcome = "dropped",
            paste_chars = text.chars().count(),
        );
        return false;
    };
    input.editor.insert_str(text);
    app.needs_redraw = true;
    true
}

/// Close the active comment editor (if any), restoring its
/// `prior_comment` when the editor was opened by re-clicking a saved
/// chip. Called everywhere `active_input` is dropped or replaced:
/// Esc-cancel, clicking a different diff line, clicking a different
/// chip, switching files via the rail, narrow-tier arrow clicks.
/// Without this centralization, every dismissal path that bypasses
/// Esc would silently destroy the saved comment — the exact bug
/// `prior_comment` was added to prevent.
///
/// Logs DEBUG with the abandoned char count when text is dropped
/// (fresh draft, or modifications layered on a reopened chip), so
/// a "where did my edit go?" triage can correlate from logs.
/// Returns the abandoned count as a Unicode scalar count for
/// callers that want it — most don't, but the central log fires
/// regardless.
fn close_active_input_preserving_prior(overlay: &mut DiffOverlayState) -> usize {
    let Some(input) = overlay.active_input.take() else { return 0 };
    let current_text = input.editor.lines().join("\n");
    // Two abandonment shapes:
    // - Fresh draft (`prior_comment = None`): every char is lost
    //   on dismissal.
    // - Reopened chip with user edits: the editor seeded from the
    //   prior, then diverged. We restore the prior verbatim on
    //   dismissal (matches GitHub edit-modal semantics: Esc =
    //   discard changes), so the divergence is the user's typed-
    //   over text that gets dropped.
    let abandoned = match input.prior_comment.as_ref() {
        Some(prior) if current_text != prior.comment_text => current_text.chars().count(),
        Some(_) => 0,
        None => current_text.chars().count(),
    };
    if abandoned > 0 {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_editor_dropped_in_progress",
            message = "comment editor closed with unsaved text",
            outcome = "dropped",
            abandoned_chars = abandoned,
            had_prior = input.prior_comment.is_some(),
        );
    }
    if let Some(prior) = input.prior_comment {
        overlay.comments.push(prior);
        overlay.recompute_comment_counts();
    }
    abandoned
}

/// Discard the active comment editor without saving. If the editor
/// was opened by re-clicking a saved 💬 chip, restore the original
/// comment so the chip reappears — the user clicked to view/edit,
/// not to destroy. Fresh line-click editors have no prior to
/// restore; their in-progress text is discarded (with the helper's
/// central DEBUG log noting the abandoned char count).
fn cancel_active_input(app: &mut App) {
    if let Some(overlay) = app.diff_overlay.as_mut() {
        let _ = close_active_input_preserving_prior(overlay);
        app.needs_redraw = true;
    }
}

/// Persist the active editor's text into [`DiffOverlayState::comments`]
/// and close the editor. The snapshot includes the anchor line's
/// hunk context so the markdown bundle stays stable even if the
/// user scrolls / switches files later.
///
/// Empty-text save semantics:
/// - Fresh line-click editor (no `prior_comment`): treated as
///   cancel — saving a blank comment would render an empty chip.
/// - Reopened chip editor (with `prior_comment`): treated as
///   delete — the user cleared all text and pressed Enter to
///   remove the saved comment. The prior is NOT restored.
fn save_active_input(app: &mut App) {
    let Some(overlay) = app.diff_overlay.as_mut() else { return };
    let Some(input) = overlay.active_input.take() else { return };
    let text = input.editor.lines().join("\n");
    if text.trim().is_empty() {
        // Empty-text branch: see docstring for fresh-vs-reopen
        // semantics. `comment_counts` already excludes the prior
        // (removed at reopen), so no recompute needed.
        app.needs_redraw = true;
        return;
    }
    // Resolve the line key into a snapshot. Under correct contract
    // (body immutable within one open) these get-branches are dead;
    // WARN-log them so a future regression that violates the
    // contract is observable, with the lengths in the log so a
    // post-mortem can quantify lost user text.
    let key = input.key;
    let Some(file) = overlay.files.get(key.file_idx) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_save_oob_file_idx",
            message = "save_active_input hit oob file_idx — body mutated mid-open?",
            outcome = "skipped",
            file_idx = key.file_idx,
            file_count = overlay.files.len(),
            lost_chars = text.chars().count(),
        );
        app.needs_redraw = true;
        return;
    };
    let Some(hunk) = file.hunks.get(key.hunk_idx) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_save_oob_hunk_idx",
            message = "save_active_input hit oob hunk_idx — body mutated mid-open?",
            outcome = "skipped",
            hunk_idx = key.hunk_idx,
            hunk_count = file.hunks.len(),
            lost_chars = text.chars().count(),
        );
        app.needs_redraw = true;
        return;
    };
    let Some(diff_line) = hunk.lines.get(key.line_idx) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_save_oob_line_idx",
            message = "save_active_input hit oob line_idx — body mutated mid-open?",
            outcome = "skipped",
            line_idx = key.line_idx,
            line_count = hunk.lines.len(),
            lost_chars = text.chars().count(),
        );
        app.needs_redraw = true;
        return;
    };
    let line_no = match diff_line.kind {
        DiffLineKind::Removed => diff_line.old_line,
        DiffLineKind::Added | DiffLineKind::Context => diff_line.new_line,
    }
    .unwrap_or(0);
    // Anchor on the single clicked line, not the whole hunk. A
    // comment on a brand-new file would otherwise pull the entire
    // file into the bundle (Added hunks span the file body); now
    // the bundle stays compact and the agent gets precise
    // per-line context. Matches GitHub's inline-review markdown
    // (quote just the line under the `**Line N**` heading).
    let comment = HunkComment {
        key,
        path: file.path.clone(),
        line: line_no,
        hunk_context: vec![diff_line.clone()],
        comment_text: text,
    };
    // Replace any existing comment at the same key (saving an
    // edited reopen).
    overlay.comments.retain(|c| c.key != key);
    overlay.comments.push(comment);
    overlay.recompute_comment_counts();
    app.needs_redraw = true;
}

/// Lines scrolled per wheel notch in the diff body. Same value as
/// `crate::app::events::mouse::MOUSE_SCROLL_LINES` (which is
/// `usize` because it feeds `Viewport::scroll_up/down`) — kept as
/// `u16` here because `body_scroll` is `u16` for ratatui's
/// `Paragraph::scroll`.
const SCROLL_LINES_PER_NOTCH: u16 = 3;

/// Maximum FILES rail width. The rail tops out here regardless of
/// terminal width; beyond ~30 columns the extra space goes to waste
/// because file paths are usually short.
pub(crate) const RAIL_WIDTH_MAX: u16 = 30;
/// Minimum FILES rail width when the rail is shown. Below this the
/// file list becomes unreadably narrow; we hide the rail entirely.
pub(crate) const RAIL_WIDTH_MIN: u16 = 20;
/// Fraction of the terminal width the rail aims for: ~22%. Picked so
/// a 120-col terminal lands at 26 (below max) and a 160-col terminal
/// lands at the max 30.
pub(crate) const RAIL_WIDTH_NUMER: u16 = 22;
pub(crate) const RAIL_WIDTH_DENOM: u16 = 100;
/// Medium-tier terminal width threshold (≥ this → rail visible).
pub(crate) const MEDIUM_MIN: u16 = 120;

/// First file row in the FILES rail. Rows above this are:
/// `0` banner (`FILES`), `1` DIM rule, `2` blank. File index 0
/// starts at `y == FIRST_FILE_ROW_Y`. The renderer at
/// `ui::diff_overlay::render_rail` chose this geometry; the click
/// handler uses it for the inverse mapping.
pub(crate) const FIRST_FILE_ROW_Y: u16 = 3;

/// Number of head rows (banner + rule + blank) at the top of the
/// DIFF body pane that DON'T scroll with `body_scroll`. The click
/// handler uses this to map `mouse.row` into the right `body_keys`
/// index without applying the scroll offset to head clicks.
pub(crate) const BODY_HEAD_ROWS: usize = 3;

/// Pick the FILES rail width for the current terminal width.
/// Returns `0` at Narrow tier (rail hidden). Shared with the
/// renderer at `crate::ui::diff_overlay::render` so the rail's
/// width and the click-handler's column threshold never drift.
pub(crate) fn rail_width_for(terminal_width: u16) -> u16 {
    if terminal_width < MEDIUM_MIN {
        return 0;
    }
    let proportional = terminal_width.saturating_mul(RAIL_WIDTH_NUMER) / RAIL_WIDTH_DENOM;
    proportional.clamp(RAIL_WIDTH_MIN, RAIL_WIDTH_MAX)
}

/// Outcome of a mouse interaction. Some interactions need access
/// to the full App (key event needs to fire `dispatch_prompt` for
/// the Esc-bundle path) which the inner `handle_*` borrow doesn't
/// have — surface them as effects the outer `handle_mouse` runs.
#[derive(Debug, Default)]
struct MouseEffect {
    redraw: bool,
}

/// Handle a mouse event while the diff overlay is active.
///
/// Bindings (v1):
/// - Scroll wheel over the rail → advance `rail_scroll`.
/// - Scroll wheel over the body → advance `body_scroll`.
/// - Shift + scroll wheel, or a native horizontal-scroll event
///   (trackpad two-finger sideways) → advance `body_scroll_x`.
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
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        match mouse.kind {
            // Shift+wheel maps to horizontal scroll for terminals
            // that don't emit native ScrollLeft / ScrollRight events
            // (most do one or the other depending on driver).
            MouseEventKind::ScrollUp if shift => handle_horizontal_scroll(overlay, -1),
            MouseEventKind::ScrollDown if shift => handle_horizontal_scroll(overlay, 1),
            MouseEventKind::ScrollLeft => handle_horizontal_scroll(overlay, -1),
            MouseEventKind::ScrollRight => handle_horizontal_scroll(overlay, 1),
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
}

/// Step `body_scroll_x` by `direction * SCROLL_COLS_PER_STEP` cols.
/// Negative goes left (clamped at 0), positive goes right.
fn handle_horizontal_scroll(overlay: &mut DiffOverlayState, direction: i32) -> MouseEffect {
    let step = i32::from(SCROLL_COLS_PER_STEP);
    let next = i32::from(overlay.body_scroll_x).saturating_add(direction.saturating_mul(step));
    let clamped = next.clamp(0, i32::from(u16::MAX));
    overlay.body_scroll_x = u16::try_from(clamped).unwrap_or(0);
    MouseEffect { redraw: true }
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
    MouseEffect { redraw: true }
}

/// Resolve a left-click to an action. Returns the effect (redraw +
/// optional close-with-submit). Hits the rail, the narrow-tier
/// arrows, the pane body's banner ✕, a diff line, a chip, or a
/// hunk header in order.
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
    // When the rail isn't rendered (terminal narrower than the split
    // threshold), the renderer paints a "too narrow" notice and
    // clears `body_keys` — clicks just no-op.
    handle_body_click(overlay, column, row)
}

fn handle_rail_click(overlay: &mut DiffOverlayState, row: u16) -> MouseEffect {
    // The tree rail mixes directory headers (non-clickable) with
    // file leaves. We resolve the click by walking `rail_keys`
    // (parallel to the rendered rows) at offset `rail_scroll`.
    // The banner / rule / blank rows live at the head of the list
    // and don't scroll — they're always at the absolute screen
    // rows 0, 1, 2. The scrollable portion starts at row 3
    // (== FIRST_FILE_ROW_Y).
    let row_idx_in_keys = if row < FIRST_FILE_ROW_Y {
        usize::from(row)
    } else {
        let scrollable_offset = usize::from(row - FIRST_FILE_ROW_Y);
        usize::from(FIRST_FILE_ROW_Y)
            .saturating_add(scrollable_offset)
            .saturating_add(usize::from(overlay.rail_scroll))
    };
    let Some(key) = overlay.rail_keys.get(row_idx_in_keys).copied() else {
        return MouseEffect::default();
    };
    let RailRowKey::File { file_idx } = key else {
        // Banner / rule / blank / directory / untracked-notice —
        // non-clickable in v1.
        return MouseEffect::default();
    };
    if file_idx >= overlay.files.len() {
        return MouseEffect::default();
    }
    if overlay.current_file_idx == file_idx {
        return MouseEffect::default();
    }
    overlay.current_file_idx = file_idx;
    overlay.body_scroll = 0;
    overlay.body_scroll_x = 0;
    // Close the active editor on file switch — the editor is
    // anchored to a specific line in the previous file, and the
    // helper preserves prior_comment if it was a chip-reopen.
    close_active_input_preserving_prior(overlay);
    MouseEffect { redraw: true }
}

/// Hit-test a left-click against the narrow-tier `◀ ▶` cycle
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
    let local_row = usize::from(row - overlay.pane_origin_row);
    // The first `body_head_rows` rows are pinned and don't scroll.
    // Wide tier sets this to BODY_HEAD_ROWS so a click in the
    // banner/rule/blank zone maps directly to body_keys[local_row].
    // Narrow tier sets it to 0 because the renderer already stripped
    // the head rows from body_keys, so every body row is scrollable.
    let head = overlay.body_head_rows;
    let body_idx = if local_row < head {
        Some(local_row)
    } else {
        local_row.checked_add(usize::from(overlay.body_scroll))
    };
    let Some(idx) = body_idx else {
        return MouseEffect::default();
    };
    let Some(key) = overlay.body_keys.get(idx).copied() else {
        return MouseEffect::default();
    };
    match key {
        BodyRowKey::HunkRow { left, right } => {
            // Pick the clicked column. The divider sits at pane
            // midpoint; clicks on it (or to the left) resolve to
            // the left key, clicks to the right resolve to the
            // right key. If the picked side is empty (blank half
            // of an unbalanced row), no-op.
            let pane_local_col = column.saturating_sub(overlay.pane_origin_col);
            let mid_col = overlay.pane_width / 2;
            let key = if pane_local_col < mid_col { left } else { right };
            match key {
                Some(key) => open_input_for_key(overlay, key),
                None => MouseEffect::default(),
            }
        }
        BodyRowKey::CommentChip(line_key) => reopen_comment_for_key(overlay, line_key),
        BodyRowKey::Banner
        | BodyRowKey::Rule
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
    // Close any existing editor (different line) before opening the
    // new one — preserves its prior_comment if it was a reopen.
    close_active_input_preserving_prior(overlay);
    let editor = TextArea::default();
    overlay.active_input = Some(ActiveCommentInput { key, editor, prior_comment: None });
    MouseEffect { redraw: true }
}

fn reopen_comment_for_key(overlay: &mut DiffOverlayState, key: LineKey) -> MouseEffect {
    // Find the saved comment, hydrate a fresh TextArea from its
    // text, drop the saved entry so the chip vanishes WHILE editing
    // (but stash it on `prior_comment` so Esc-cancel can restore it
    // — losing the saved comment to a misclick-and-reflex-Esc would
    // destroy review notes the user wrote).
    let position = overlay.comments.iter().position(|c| c.key == key);
    let Some(pos) = position else {
        return MouseEffect::default();
    };
    let comment = overlay.comments.remove(pos);
    overlay.recompute_comment_counts();
    // Close any pre-existing editor on a different line so its
    // prior_comment survives (without this, A's prior would be
    // silently dropped when B's reopen runs).
    close_active_input_preserving_prior(overlay);
    let mut editor = TextArea::default();
    // TextArea::insert_str respects newlines correctly so the
    // multi-line shape of the saved comment is preserved.
    editor.insert_str(&comment.comment_text);
    overlay.active_input =
        Some(ActiveCommentInput { key: comment.key, editor, prior_comment: Some(comment) });
    MouseEffect { redraw: true }
}

/// Close the overlay; if there are pending comments, bundle them
/// into a markdown user message and dispatch it as a prompt before
/// closing. Used by the banner ✕ click and by `handle_key`'s Esc
/// path.
///
/// Pre-flight: if comments are pending AND the agent isn't ready
/// to receive a prompt (no active session, pre-Connect), the close
/// is REFUSED — a system message tells the user to retry once the
/// session connects + a WARN log lets an operator grep for the
/// held state. Without this, `dispatch_prompt`'s silent no-agent
/// path would drop the bundle on the floor and the user would
/// lose their review notes.
pub(super) fn close_with_submit(app: &mut App) {
    // Flush the active editor BEFORE the pending check — a reopened
    // chip moves its saved comment onto `active_input.prior_comment`,
    // so `overlay.comments` is empty while the editor is open. If we
    // checked pending first, a held-no-agent state with only a
    // reopened-chip-in-flight would BYPASS the held branch (pending
    // = false), fall through to dispatch_prompt's silent no-agent
    // path, and lose the user's saved review note. The helper
    // restores the prior to `o.comments` so the post-flush pending
    // check sees the complete set.
    if let Some(o) = app.diff_overlay.as_mut() {
        let _ = close_active_input_preserving_prior(o);
    }
    let pending = app.diff_overlay.as_ref().is_some_and(|o| !o.comments.is_empty());
    if pending && (!app.has_active_agent() || app.session_id().is_none()) {
        let comment_count = app.diff_overlay.as_ref().map_or(0, |o| o.comments.len());
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_close_held_no_agent",
            message = "diff overlay close held: agent not ready, comments preserved",
            outcome = "held",
            has_agent = app.has_active_agent(),
            has_session_id = app.session_id().is_some(),
            pending_comments = comment_count,
        );
        crate::app::slash::push_system_message(
            app,
            "Diff overlay close held: agent not ready. Wait for the session to connect, then press Esc again to submit your comments.",
        );
        // Leave the overlay open so the user can retry. The user's
        // comments stay intact on `overlay.comments` (including any
        // prior restored by the flush above). They can also abandon
        // by clearing chips one by one (click chip + Esc-cancel-input),
        // though that's a long path.
        app.needs_redraw = true;
        return;
    }
    // Single-pass snapshot of everything we need from overlay state
    // BEFORE close() drops it. Avoids the previous two-step (take
    // comments, then re-read target/cwd) where the second read
    // relied on `unwrap_or_default()` for a value that's never None
    // in practice — confusing for future maintainers.
    let snapshot = app.diff_overlay.as_mut().map(|o| {
        let comments = std::mem::take(&mut o.comments);
        (comments, o.target.clone(), o.cwd.display().to_string())
    });
    if let Some((comments, target, cwd_display)) = snapshot
        && !comments.is_empty()
    {
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
    // Use the entry API's vacant branch to populate `order` exactly
    // once per file so we don't double-clone the path on every
    // comment beyond the first.
    let mut order: Vec<String> = Vec::new();
    let mut by_file: std::collections::HashMap<String, Vec<&HunkComment>> =
        std::collections::HashMap::new();
    for c in comments {
        by_file
            .entry(c.path.clone())
            .or_insert_with(|| {
                order.push(c.path.clone());
                Vec::new()
            })
            .push(c);
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
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![
                FileHunks { path: "a.rs".into(), status: FileStatus::Modified, hunks: vec![] },
                FileHunks { path: "b.rs".into(), status: FileStatus::Added, hunks: vec![] },
            ],
        );
        // Simulate what the renderer's tree pass would stash on
        // overlay state for the rail click handler. The two files
        // are top-level (no shared directory prefix) so the tree
        // is flat: banner/rule/blank then two file leaves.
        state.rail_keys = vec![
            RailRowKey::Banner,
            RailRowKey::Rule,
            RailRowKey::Blank,
            RailRowKey::File { file_idx: 0 },
            RailRowKey::File { file_idx: 1 },
        ];
        state
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
    fn body_click_left_column_opens_comment_input_on_left_key() {
        // Simulate a rendered split row with both columns present;
        // click in the left half resolves to the left key.
        let mut state = sample_state();
        let left_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::Banner,
            BodyRowKey::Rule,
            BodyRowKey::Blank,
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(left_key), right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41; // Past rail + separator on wide.
        state.pane_width = 119;
        // Left half: pane-local col in [0, 59) → click_col in [41, 100).
        let effect = handle_left_click(&mut state, 60, 4, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(left_key));
    }

    #[test]
    fn body_click_right_column_opens_comment_input_on_right_key() {
        let mut state = sample_state();
        let left_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::Banner,
            BodyRowKey::Rule,
            BodyRowKey::Blank,
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(left_key), right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Right half: pane-local col in [60, 119) → click_col in [101, 160).
        let effect = handle_left_click(&mut state, 120, 4, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(right_key));
    }

    #[test]
    fn body_click_on_empty_side_is_noop() {
        let mut state = sample_state();
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.body_keys = vec![
            BodyRowKey::Banner,
            BodyRowKey::Rule,
            BodyRowKey::Blank,
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: None, right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Click in the (blank) LEFT half — left=None, so no editor opens.
        let effect = handle_left_click(&mut state, 60, 4, 160);
        assert!(!effect.redraw);
        assert!(state.active_input.is_none());
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
    fn recompute_comment_counts_zeroes_then_tallies() {
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "x".into(),
        });
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 1, line_idx: 0 },
            path: "a.rs".into(),
            line: 2,
            hunk_context: vec![],
            comment_text: "y".into(),
        });
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 1, hunk_idx: 0, line_idx: 0 },
            path: "b.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "z".into(),
        });
        state.recompute_comment_counts();
        assert_eq!(state.comment_counts, vec![2, 1]);
    }

    #[test]
    fn recompute_comment_counts_handles_empty_comments() {
        let mut state = sample_state();
        state.recompute_comment_counts();
        assert_eq!(state.comment_counts, vec![0, 0]);
    }

    #[test]
    fn recompute_comment_counts_resizes_with_files() {
        let mut state = sample_state();
        // Stale comment_counts vec from a prior file set.
        state.comment_counts = vec![5, 5, 5];
        state.recompute_comment_counts();
        assert_eq!(state.comment_counts.len(), state.files.len());
    }

    #[test]
    fn close_with_submit_refuses_when_agent_not_ready() {
        // No active agent in the test default — close_with_submit
        // with pending comments must keep the overlay open + push
        // a system message rather than silently dropping comments.
        let mut app = App::test_default();
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "needs unwrap fix".into(),
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        // Overlay still open + comment still alive — user can retry.
        assert!(app.diff_overlay.is_some(), "overlay stays open when dispatch would silently fail");
        assert_eq!(
            app.diff_overlay.as_ref().map(|o| o.comments.len()),
            Some(1),
            "comments preserved on hold"
        );
        assert_eq!(app.active_view, ActiveView::Diff, "view stays on Diff");
    }

    #[test]
    fn reopen_then_cancel_restores_saved_comment() {
        // F1 fix: clicking a chip stashes the saved comment on
        // active_input.prior_comment; Esc-cancel must restore it
        // to overlay.comments so a misclick + reflex Esc doesn't
        // destroy review notes.
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "I want to keep this".into(),
        });
        state.recompute_comment_counts();
        state.body_keys = vec![BodyRowKey::CommentChip(key)];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        // Click the chip → editor opens with prior_comment Some.
        let effect = handle_left_click(app.diff_overlay.as_mut().expect("overlay"), 60, 0, 160);
        assert!(effect.redraw);
        assert!(app.diff_overlay.as_ref().expect("overlay").active_input.is_some());
        assert!(
            app.diff_overlay
                .as_ref()
                .expect("overlay")
                .active_input
                .as_ref()
                .unwrap()
                .prior_comment
                .is_some(),
            "prior_comment stashed on chip reopen"
        );
        // Now press Esc → cancel_active_input restores prior.
        cancel_active_input(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay");
        assert!(after.active_input.is_none(), "editor closed");
        assert_eq!(after.comments.len(), 1, "saved comment restored");
        assert_eq!(after.comments[0].comment_text, "I want to keep this");
    }

    #[test]
    fn reopen_then_click_other_line_preserves_prior() {
        // F7: opening editor B via line click while editor A (a
        // chip reopen) is open must preserve A's prior_comment.
        let mut state = sample_state();
        let key_a = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key: key_a,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "saved".into(),
        });
        state.recompute_comment_counts();
        // Body geometry: chip row at idx 0, hunk header at idx 1,
        // hunk line at idx 2.
        let key_b = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::CommentChip(key_a),
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(key_b), right: Some(key_b) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Click chip → editor opens with prior Some.
        let _ = handle_left_click(&mut state, 60, 0, 160);
        assert!(state.active_input.as_ref().unwrap().prior_comment.is_some());
        assert_eq!(state.comments.len(), 0, "comment moved into prior");
        // Click a different diff line → editor B opens; A's prior
        // must have been restored to overlay.comments.
        let _ = handle_left_click(&mut state, 60, 2, 160);
        assert_eq!(state.active_input.as_ref().unwrap().key, key_b);
        assert_eq!(state.comments.len(), 1, "A's prior restored before B opens");
        assert_eq!(state.comments[0].comment_text, "saved");
    }

    #[test]
    fn reopen_chip_then_click_other_chip_preserves_both() {
        let mut state = sample_state();
        let key_a = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let key_b = LineKey { file_idx: 0, hunk_idx: 1, line_idx: 0 };
        state.comments.push(HunkComment {
            key: key_a,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "A".into(),
        });
        state.comments.push(HunkComment {
            key: key_b,
            path: "a.rs".into(),
            line: 5,
            hunk_context: vec![],
            comment_text: "B".into(),
        });
        state.recompute_comment_counts();
        state.body_keys = vec![BodyRowKey::CommentChip(key_a), BodyRowKey::CommentChip(key_b)];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let _ = handle_left_click(&mut state, 60, 0, 160);
        let _ = handle_left_click(&mut state, 60, 1, 160);
        // Now editor is open on B with B as prior; A should be back
        // in overlay.comments.
        assert_eq!(state.active_input.as_ref().unwrap().key, key_b);
        assert_eq!(state.comments.len(), 1, "A restored, B in prior");
        assert_eq!(state.comments[0].key, key_a);
    }

    #[test]
    fn rail_switch_preserves_prior_comment() {
        let mut state = sample_state();
        let key_a = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key: key_a,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "A".into(),
        });
        state.recompute_comment_counts();
        state.body_keys = vec![BodyRowKey::CommentChip(key_a)];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Reopen chip A.
        let _ = handle_left_click(&mut state, 60, 0, 160);
        assert!(state.active_input.as_ref().unwrap().prior_comment.is_some());
        // Click file 1 in the rail (row 4 in sample geometry).
        let _ = handle_left_click(&mut state, 5, 4, 160);
        // Editor closed, A restored.
        assert!(state.active_input.is_none());
        assert_eq!(state.comments.len(), 1);
        assert_eq!(state.comments[0].key, key_a);
    }

    #[test]
    fn reopen_edit_then_cancel_drops_edits_and_restores_prior() {
        // F1: user reopens chip, types edits, then dismisses (Esc).
        // Per GitHub edit-modal semantics, the chip restores to its
        // pre-edit state — the typed-over changes are intentionally
        // dropped. Verify the prior is restored verbatim AND the
        // helper reports the divergence as abandoned chars (so the
        // central DEBUG log fires for telemetry).
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "original text".into(),
        };
        let mut editor = TextArea::default();
        editor.insert_str("original text with user-typed edits");
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: Some(prior.clone()) });
        let abandoned = close_active_input_preserving_prior(&mut state);
        assert!(abandoned > 0, "user's typed-over text counts as abandoned");
        assert_eq!(state.comments.len(), 1);
        assert_eq!(state.comments[0].comment_text, "original text", "prior restored verbatim");
    }

    #[test]
    fn reopen_no_edit_then_cancel_reports_zero_abandoned() {
        // F1 boundary: when the editor's content equals the prior
        // exactly (user reopened, didn't type), abandoned should be 0
        // — no telemetry log fires for "viewed and dismissed".
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "exactly this".into(),
        };
        let mut editor = TextArea::default();
        editor.insert_str("exactly this");
        state.active_input = Some(ActiveCommentInput { key, editor, prior_comment: Some(prior) });
        let abandoned = close_active_input_preserving_prior(&mut state);
        assert_eq!(abandoned, 0, "no divergence → no abandoned text");
        assert_eq!(state.comments.len(), 1);
    }

    #[test]
    fn fresh_editor_close_reports_abandoned_chars() {
        // F2 sister test: a fresh-editor dismissal via any of the
        // helper-using paths surfaces the abandoned count.
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut editor = TextArea::default();
        editor.insert_str("draft typed by user");
        state.active_input = Some(ActiveCommentInput { key, editor, prior_comment: None });
        let abandoned = close_active_input_preserving_prior(&mut state);
        assert_eq!(abandoned, "draft typed by user".chars().count());
        assert!(state.comments.is_empty(), "fresh editor's text is not saved");
    }

    #[test]
    fn save_empty_fresh_editor_creates_no_chip() {
        // F8: fresh editor (prior None) + Enter on blank text →
        // no chip, no comment.
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.active_input =
            Some(ActiveCommentInput { key, editor: TextArea::default(), prior_comment: None });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay still set");
        assert!(after.active_input.is_none(), "editor closed");
        assert!(after.comments.is_empty(), "no blank chip created");
    }

    #[test]
    fn save_empty_reopened_chip_deletes_saved_comment() {
        // F8: reopen chip (prior Some) + clear text + Enter →
        // saved comment deleted.
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "soon-to-be-deleted".into(),
        };
        state.active_input = Some(ActiveCommentInput {
            key,
            editor: TextArea::default(), // empty editor
            prior_comment: Some(prior),
        });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay still set");
        assert!(after.active_input.is_none());
        assert!(after.comments.is_empty(), "prior dropped via clear+save = delete");
    }

    #[test]
    fn close_with_submit_flushes_reopened_chip_to_bundle() {
        // SILENT-1 fix: banner ✕ / Esc with an open editor that's a
        // chip-reopen must restore the prior to the bundle. Without
        // the pre-flight flush, the prior would be dropped silently
        // because the snapshot's mem::take pulls only from
        // overlay.comments. Verify by inspecting the dispatched
        // Command::PromptWithImages's text for the prior's content.
        let mut app = App::test_default();
        let mut rx = app.install_testing_stub();
        app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "important review note".into(),
        };
        let mut editor = TextArea::default();
        editor.insert_str("important review note");
        // Editor open as a chip reopen — prior_comment Some, no
        // unsubmitted comments in overlay.comments yet.
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: Some(prior.clone()) });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "overlay closed");
        // The prior must have made it into the dispatched bundle —
        // inspect the Command::PromptWithImages text the stub
        // receiver picked up.
        let dispatched = rx.try_recv().expect("a prompt was dispatched");
        match dispatched {
            forge_primitives::AgentCommand::PromptWithImages { text, .. } => {
                assert!(
                    text.contains("important review note"),
                    "bundle markdown contains the prior comment text, got: {text}",
                );
            }
            other => panic!("expected PromptWithImages, got {other:?}"),
        }
    }

    #[test]
    fn close_with_submit_holds_on_reopened_chip_with_no_agent() {
        // Pre-flight-after-flush fix: if the agent isn't ready and
        // the only "pending" content is a reopened chip's prior
        // (so overlay.comments is empty at entry), the OLD pending
        // check would bypass the held branch and dispatch_prompt
        // would silently drop the note. The fixed order (flush
        // first, then pending check) restores the prior into
        // overlay.comments BEFORE the pending check, so the held
        // branch fires correctly.
        let mut app = App::test_default();
        // No install_testing_stub → has_active_agent = false.
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "to be preserved".into(),
        };
        let mut editor = TextArea::default();
        editor.insert_str("to be preserved");
        state.active_input = Some(ActiveCommentInput { key, editor, prior_comment: Some(prior) });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        // Overlay stays open + prior restored to comments so user
        // can retry once the agent connects.
        let after = app.diff_overlay.as_ref().expect("overlay held");
        assert_eq!(after.comments.len(), 1, "prior restored on held path");
        assert_eq!(after.comments[0].comment_text, "to be preserved");
        assert!(after.active_input.is_none(), "editor closed by the flush");
    }

    #[test]
    fn close_with_submit_no_comments_closes_cleanly_even_without_agent() {
        // Empty comments path skips the dispatch entirely, so the
        // no-agent state shouldn't block closing — the user just
        // wants to dismiss the overlay.
        let mut app = App::test_default();
        app.diff_overlay = Some(sample_state());
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "empty-comments close still drops state");
        assert_eq!(app.active_view, ActiveView::Chat, "view returns to chat");
    }

    #[test]
    fn rail_width_caps_at_max_on_wide_terminals() {
        assert_eq!(rail_width_for(160), RAIL_WIDTH_MAX);
        assert_eq!(rail_width_for(300), RAIL_WIDTH_MAX);
    }

    #[test]
    fn rail_width_scales_proportionally_in_medium_band() {
        // 120 × 22 / 100 = 26 (under MAX, over MIN → clamped to 26).
        assert_eq!(rail_width_for(120), 26);
        // 145 × 22 / 100 = 31, clamped down to MAX (30).
        assert_eq!(rail_width_for(145), RAIL_WIDTH_MAX);
    }

    #[test]
    fn rail_width_clamps_to_min_on_borderline_terminals() {
        // Anything below MEDIUM_MIN hides the rail entirely.
        assert_eq!(rail_width_for(119), 0);
        assert_eq!(rail_width_for(80), 0);
    }
}
