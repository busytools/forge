//! Full-screen diff overlay state + keyboard handling.
//!
//! The overlay is the floor of the `/diff` flow: a snapshot of
//! file-level hunks fetched via
//! [`forge_workspace::Workspace::scan_git_diff_hunks`] rendered as a
//! single continuous scroll of every changed file with a FILES jump
//! rail. See [`crate::ui::diff_overlay`] for the renderer; this module
//! owns the transient state and the key / mouse dispatch.
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
use std::sync::mpsc as std_mpsc;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use forge_primitives::git_diff::RepoGate;
use forge_workspace::env::git_diff::hunks::ScanOutcome;
use forge_workspace::env::git_diff::hunks::{
    CommitMeta, DiffLine, DiffLineKind, FileHunks, FileStatus,
};
use tui_textarea::TextArea;

use super::App;
use super::view::{ActiveView, set_active_view};

/// Diff body layout. Unified is the default GitHub-style inline view;
/// `t` toggles to side-by-side split. The toggle flips the whole
/// document, not a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffViewMode {
    #[default]
    Unified,
    Split,
}

/// Which slice of the branch the overlay currently shows. The stepper
/// (`commits` non-empty) walks `Commit(i)`; `WholeDiff` is the
/// whole-branch view - today's behavior, and the "All changes" entry in
/// the jump dropdown. When `commits` is empty the scope is always
/// `WholeDiff` and the stepper never renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffScope {
    #[default]
    WholeDiff,
    Commit(usize),
}

/// A completed scan for one scope, cached so re-navigating to it is
/// instant (and a comment's `LineKey` stays valid because the file set
/// for a scope never changes once scanned). `scanner_ok` mirrors the
/// scan outcome so a per-commit scan failure surfaces distinctly.
#[derive(Debug, Clone)]
pub struct CachedScan {
    pub files: Vec<FileHunks>,
    pub scanner_ok: bool,
}

/// Result of pointing the overlay at a scope: either its hunks were
/// cached (files already swapped) or an async scan must be spawned by
/// the caller (which has the event channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavOutcome {
    /// Scope's hunks were cached; the file set is already swapped in.
    Ready,
    /// Scope needs a scan; the caller should spawn one for this scope.
    NeedsScan(DiffScope),
}

/// Document offset table: `starts[i]` is file `i`'s first row in the
/// concatenated document; `total` is the document height in rows.
/// Drives the document scroll, the rail-jump, and the rail's
/// current-file highlight - all of which need to map a row offset to
/// a file (and back) across the whole flattened diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocOffsets {
    pub starts: Vec<u32>,
    pub total: u32,
}

impl DocOffsets {
    /// File index whose row-range contains `row`; clamps to the last
    /// file when `row` is past the end (empty doc returns 0).
    pub fn file_at_row(&self, row: u32) -> usize {
        match self.starts.binary_search(&row) {
            Ok(idx) => idx,
            Err(0) => 0,
            Err(idx) => idx - 1,
        }
    }
}

/// Prefix-sum the per-file heights into a [`DocOffsets`].
pub fn file_offsets(heights: &[u32]) -> DocOffsets {
    let mut starts = Vec::with_capacity(heights.len());
    let mut acc = 0u32;
    for &h in heights {
        starts.push(acc);
        acc = acc.saturating_add(h);
    }
    DocOffsets { starts, total: acc }
}

/// Unwrapped, syntax-highlighted spans for one file's diff lines,
/// indexed `[hunk_idx][line_idx]` (the innermost `Vec` is one line's
/// spans). Cached on [`DiffOverlayState::highlighted`] and reused
/// across frames - a line's colour is layout-independent, so it
/// survives scroll, view_mode flip, and resize untouched, and a
/// plain scroll never re-runs syntect.
pub type FileHighlight = Vec<Vec<Vec<ratatui::text::Span<'static>>>>;

/// Cheap height estimate for an off-screen file (no wrap, no pairing):
/// 1 sticky header row + per hunk (1 `@@` header + raw line count), or
/// 2 for a collapsed deleted file. The offset table uses this for
/// not-yet-measured files; the renderer's `file_height` replaces it
/// (storing into `measured_heights`) when the file enters the window.
pub(crate) fn estimated_height(file: &FileHunks, collapsed: bool) -> u32 {
    if collapsed {
        return 2;
    }
    let mut rows: u32 = 1; // file sticky header
    for hunk in &file.hunks {
        rows = rows.saturating_add(1); // @@ header
        rows = rows.saturating_add(u32::try_from(hunk.lines.len()).unwrap_or(u32::MAX));
    }
    rows
}

/// Identifies a single rendered diff line - `(file_idx, hunk_idx,
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
/// file index - or recognises the click as hitting non-interactive
/// chrome / a directory header - without re-walking the file list.
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
    /// File leaf - click switches the right pane to this file.
    File { file_idx: usize },
    /// `+N untracked suppressed (cap M)` notice row at the bottom
    /// of the rail when the scanner hit its untracked cap. Non-
    /// clickable.
    UntrackedNotice,
}

/// What a single rendered row in the right pane corresponds to.
/// Built by the renderer alongside the `Vec<Line>` it returns, and
/// stashed on `DiffOverlayState` so the mouse handler can resolve a
/// click (`row` + the tail scroll) → action without re-walking it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyRowKey {
    /// Empty-state notice (scan failed / no changes / binary / etc.).
    EmptyState,
    /// Sticky file-divider header (path + status badge + `+N -M`).
    /// Click on a Deleted file's header toggles its collapse; other
    /// statuses are non-interactive.
    FileHeader { file_idx: usize },
    /// The one-line "File deleted - N lines removed" notice shown for
    /// a collapsed deleted file. Click expands it.
    DeletedCollapsed { file_idx: usize },
    /// `@@ -A,B +C,D @@` hunk header - non-interactive in v1.
    HunkHeader { file_idx: usize, hunk_idx: usize },
    /// A diff row in the split body. Carries both column keys -
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
    /// A row of the commit-message block shown above the diff in commit
    /// mode (the leading rule, subject, or a body line). Non-interactive.
    CommitMessage,
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
    /// Full hunk the comment is anchored on - included verbatim in
    /// the markdown bundle so the agent sees the local context.
    pub hunk_context: Vec<DiffLine>,
    pub comment_text: String,
    /// Commit the comment was made against (the sha), or `None` in
    /// whole-diff scope. Together with `key`/`path`/`line` this scopes
    /// the comment: navigating between commits keeps every comment but
    /// only those matching the current scope render and count.
    pub commit: Option<String>,
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

/// What a completed scan event carries beyond its files: either the
/// initial open (which builds a fresh overlay, with the commit stepper
/// list when the target has commits ahead) or a lazily-scanned scope
/// (a commit or "All changes") installed into an already-open overlay.
#[derive(Debug)]
pub enum DiffScanKind {
    /// The overlay's initial open. `commits` empty ⇒ whole-diff mode
    /// (`files` is the whole-branch diff); non-empty ⇒ commit mode
    /// (`files` is the first commit's diff). `branch` names the branch
    /// under review for the stepper header.
    Initial { commits: Vec<CommitMeta>, branch: Option<String> },
    /// A lazily-scanned scope, installed into the open overlay's cache
    /// (and swapped into view if still current). `files` is that scope's
    /// diff.
    Scope(DiffScope),
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
/// `kind` distinguishes the initial open from a lazy per-scope scan.
/// `commit_body` carries the scanned commit's message body (`Some` for a
/// commit scope - the initial open's first commit, or a lazy per-commit
/// scan; `None` for whole-diff, which has no single commit).
#[derive(Debug)]
pub struct DiffOverlayEvent {
    pub cwd: PathBuf,
    pub target: String,
    pub files: Vec<FileHunks>,
    pub scanner_ok: bool,
    pub untracked_suppressed: usize,
    pub seq: u64,
    pub kind: DiffScanKind,
    pub commit_body: Option<String>,
}

/// Spawn the initial `/diff` scan and post a [`DiffOverlayEvent`] when
/// it completes. Best-effort send - receiver going away (app shutdown)
/// just drops the result. Scans the commit list first: when the target
/// has commits ahead the overlay opens in commit mode on the first
/// commit (its diff is scanned upfront; the rest lazily on navigation);
/// otherwise it scans the whole diff and opens whole-diff mode - today's
/// behavior. `branch` names the branch under review for the stepper.
pub fn spawn_fetch(
    cwd: PathBuf,
    target: String,
    branch: Option<String>,
    seq: u64,
    tx: std_mpsc::Sender<DiffOverlayEvent>,
) {
    tokio::task::spawn_local(async move {
        let commits = forge_workspace::env::git_diff::hunks::scan_commits(&cwd, &target).await;
        // Clone the first sha so `commits` isn't borrowed across the
        // scan (it moves into the event below).
        let first_sha = commits.first().map(|c| c.sha.clone());
        let (files, scanner_ok, untracked_suppressed, commit_body) = if let Some(sha) = first_sha {
            let o = forge_workspace::env::git_diff::hunks::scan_commit(&cwd, &sha).await;
            let body = forge_workspace::env::git_diff::hunks::scan_commit_body(&cwd, &sha).await;
            (o.files, o.scanner_ok, 0, Some(body))
        } else {
            let ScanOutcome { files, scanner_ok, untracked_suppressed } =
                forge_workspace::env::git_diff::hunks::scan(&cwd, &target).await;
            (files, scanner_ok, untracked_suppressed, None)
        };
        let _ = tx.send(DiffOverlayEvent {
            cwd,
            target,
            files,
            scanner_ok,
            untracked_suppressed,
            seq,
            kind: DiffScanKind::Initial { commits, branch },
            commit_body,
        });
    });
}

/// Spawn a lazy scan for one scope (a commit's own diff, or the whole
/// branch for "All changes") and post it back as a
/// [`DiffScanKind::Scope`] event. `sha` is `Some` for a commit scope,
/// `None` for whole-diff (which scans `target`). Reuses the current
/// `seq` (no bump) so a scope scan spawned during navigation is dropped
/// only if a fresh `/diff` supersedes the whole overlay.
fn spawn_scope_fetch(
    cwd: PathBuf,
    target: String,
    scope: DiffScope,
    sha: Option<String>,
    seq: u64,
    tx: std_mpsc::Sender<DiffOverlayEvent>,
) {
    tokio::task::spawn_local(async move {
        let (outcome, commit_body) = match &sha {
            Some(sha) => {
                let outcome = forge_workspace::env::git_diff::hunks::scan_commit(&cwd, sha).await;
                let body = forge_workspace::env::git_diff::hunks::scan_commit_body(&cwd, sha).await;
                (outcome, Some(body))
            }
            None => (forge_workspace::env::git_diff::hunks::scan(&cwd, &target).await, None),
        };
        let ScanOutcome { files, scanner_ok, untracked_suppressed } = outcome;
        let _ = tx.send(DiffOverlayEvent {
            cwd,
            target,
            files,
            scanner_ok,
            untracked_suppressed,
            seq,
            kind: DiffScanKind::Scope(scope),
            commit_body,
        });
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
    /// the user IS in a repo; git just couldn't run. The snapshot's
    /// `repo_gate` is `RepoGate::ScannerFailed`.
    ScannerFailed,
    /// Snapshot has `branch_ahead` populated (so the scanner sees
    /// committed work) but the default branch itself couldn't be
    /// resolved - no `origin/HEAD`, no local `main`, no local
    /// `master`. Distinct from `Clean` because there ARE changes;
    /// we just don't know which ref to compare against. User needs
    /// to pass an explicit `/diff <ref>`.
    ///
    /// In the current scan logic this is structurally unreachable
    /// because `branch_ahead` is only constructed when
    /// `default_branch` resolved. Kept as a defensive case so a
    /// future refactor that decouples the two doesn't accidentally
    /// collapse this into `Clean`.
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
/// itself ALWAYS runs fresh - only the *target ref* (e.g. `main` vs
/// `master`) can be wrong. Worst-case the user sees "no changes" and
/// reruns `/diff <ref>` explicitly. Not worth the synchronous
/// refresh cost on the click hot-path.
pub fn resolve_default_target(app: &App) -> DefaultTarget {
    let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()) else {
        return DefaultTarget::NoSnapshot;
    };
    // Scanner crash and not-a-repo are distinct surfaces; map the gate
    // before any layer check.
    match snapshot.repo_gate {
        RepoGate::ScannerFailed => return DefaultTarget::ScannerFailed,
        RepoGate::NotARepo => return DefaultTarget::NotARepo,
        RepoGate::InRepo => {}
    }
    // Layer 1 wins when both layers are populated: a dirty tree is
    // what the user clicks `🦉` to inspect, and `HEAD` covers the
    // uncommitted edits. The committed-but-unmerged work
    // (`branch_ahead`) is reachable via an explicit `/diff <default>`
    // - auto-detect prefers the more-recent surface.
    if snapshot.worktree.is_populated() {
        return DefaultTarget::Ref("HEAD".to_owned());
    }
    if snapshot.branch_ahead.is_populated() {
        return match snapshot.default_branch.as_deref() {
            Some(default) => DefaultTarget::Ref(default.to_owned()),
            None => DefaultTarget::NoDefault,
        };
    }
    // No layer populated: clean tree on the default branch (or on a
    // branch with no commits ahead). The renderer hands the user
    // back the resolved default for context.
    DefaultTarget::Clean { default_branch: snapshot.default_branch.clone() }
}

/// Kick off a diff scan against `target` and post the result
/// through the overlay event channel. Pushes a system message
/// (via `app::slash::push_system_message`) on every failure path -
/// workspace not ready, no active session, empty cwd - so callers
/// don't need to handle that themselves. Used by `/diff <target>`
/// directly; `open_default` builds on top of it for the auto-detect
/// path.
pub fn open_with_target(app: &mut App, target: String) {
    let Some(cwd_raw) = app.active_session().map(|s| s.cwd_raw.clone()) else {
        crate::app::slash::push_system_message(app, "Cannot open diff: no active session.");
        return;
    };
    if cwd_raw.is_empty() {
        crate::app::slash::push_system_message(app, "Cannot open diff: active session has no cwd.");
        return;
    }
    let cwd = resolve_active_diff_cwd(app, &cwd_raw);
    // Branch under review for the stepper header, from the Inspector
    // snapshot (best-effort; `None` on detached HEAD / no snapshot).
    let branch =
        app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()).and_then(
            |snap| match &snap.branch {
                forge_primitives::git::GitBranch::Named(name) => Some(name.clone()),
                _ => None,
            },
        );
    // Bump the seq before spawning so the new scan's events
    // outrank anything still in flight from an earlier /diff call.
    // Old events arriving on the channel after this bump will be
    // dropped by drain_events as superseded.
    app.diff_scan_seq = app.diff_scan_seq.wrapping_add(1);
    let seq = app.diff_scan_seq;
    spawn_fetch(cwd, target, branch, seq, app.diff_overlay_event_tx.clone());
}

/// Resolve the cwd a diff scan should run against for the active
/// session. Workers spawned in a git repo run inside claude's
/// `--worktree <label>` fork at
/// `<project_root>/.claude/worktrees/<label>`, but `cwd_raw` varies
/// by lifecycle: fresh spawns carry the project root, resumed
/// sessions carry the worktree path itself.
/// `git_scan_cwd_for_session` anchors on the worker's project_key so
/// both lifecycle states converge on the same final path. Mirror its
/// call from `git_diff::apply_timer_tick` so the overlay opens
/// against the worker's branch, not the lead's. For lead sessions,
/// non-git workers, or any session not registered as a live worker,
/// `git_scan_cwd_for_session` returns `cwd_raw` unchanged.
fn resolve_active_diff_cwd(app: &App, cwd_raw: &str) -> PathBuf {
    let cwd_raw_path = PathBuf::from(cwd_raw);
    let Some(active_key) = app.active_session_key.as_ref() else {
        return cwd_raw_path;
    };
    debug_assert!(
        app.workspace.is_some(),
        "workspace unset after init (diff_overlay::resolve_active_diff_cwd); MVVM contract violated",
    );
    if let Some(workspace) = app.workspace.as_ref() {
        workspace.git_scan_cwd_for_session(active_key, &cwd_raw_path)
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_workspace_unset",
            message = "App.workspace is None during diff overlay cwd resolution; using cwd_raw without worker-cwd resolution",
            outcome = "fallback",
            key = %active_key.as_str(),
        );
        cwd_raw_path
    }
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
                "Git scanner hasn't run yet - try /diff again in a moment.",
            );
        }
        DefaultTarget::NotARepo => {
            crate::app::slash::push_system_message(app, "Not a git repository.");
        }
        DefaultTarget::ScannerFailed => {
            crate::app::slash::push_system_message(
                app,
                "Git scanner hit an error - see tracing logs (target: agent.env_git). Try /diff again in a moment.",
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
/// - `app.active_view != ActiveView::Chat` - user opened config /
///   session picker / launchpad / another overlay while the scan
///   was running. Yanking them into the diff view would be
///   surprising.
/// - `event.cwd` doesn't match the active session's `cwd_raw` -
///   user switched sessions mid-scan; the result is for a stale
///   project, and crosstalking it into the new session would
///   confuse.
///
/// Both cases log at DEBUG so a future "why didn't /diff open?"
/// triage can correlate the event. No chat message is pushed -
/// the user explicitly navigated away, so a notice arriving later
/// would be noise. The user can rerun `/diff` if they want the
/// scan they kicked off.
pub fn drain_events(app: &mut App) {
    for _ in 0..EVENT_DRAIN_BUDGET {
        let event = match app.diff_overlay_event_rx.try_recv() {
            Ok(event) => event,
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        // Superseded by a newer /diff invocation - silent drop.
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
        // Comparison must use the SAME resolved cwd that the scan spawn
        // passed - the event's `cwd` echoes whatever the scanner
        // received. For worker sessions the scanner runs against the
        // worktree fork (`<project_root>/.claude/worktrees/<label>`),
        // not the raw `cwd_raw`, so comparing against `cwd_raw` would
        // silently drop every worker event.
        let active_cwd = app
            .active_session()
            .map(|s| s.cwd_raw.clone())
            .map(|raw| resolve_active_diff_cwd(app, &raw));
        if active_cwd.as_deref() != Some(event.cwd.as_path()) {
            // Silent drop - a scan for the OLD session crosstalking into
            // the now-active one would confuse. Rerun /diff explicitly.
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
        if matches!(event.kind, DiffScanKind::Initial { .. }) {
            // Initial open: only land it while the user is still in chat
            // (they'd be surprised to be yanked into the overlay after
            // navigating away). Silent drop + DEBUG otherwise.
            if app.active_view != ActiveView::Chat {
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
            let state = DiffOverlayState::new_initial(event);
            open(app, state);
        } else if let DiffScanKind::Scope(scope) = event.kind {
            // A lazy per-scope scan lands into the already-open overlay
            // (view == Diff). If it closed while the scan ran, drop it.
            if let Some(overlay) = app.diff_overlay.as_mut() {
                overlay.install_scan(scope, event.files, event.scanner_ok, event.commit_body);
                app.needs_redraw = true;
            } else {
                tracing::debug!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "diff_overlay_drain_skipped_closed",
                    message = "per-scope scan completed but the overlay was closed; dropping",
                    outcome = "skipped",
                    target_ref = %event.target,
                );
            }
        }
    }
}

/// All state the diff overlay view needs. Lives on
/// `App.diff_overlay` (`Option<Self>`) - `Some` while the view is
/// active, dropped to `None` on close so a stale snapshot can't
/// leak into the next open.
#[derive(Debug, Clone)]
pub struct DiffOverlayState {
    /// Project root the scan was run against. Resolves relative
    /// paths inside hunks and labels the overlay so the user knows
    /// which project they're reviewing.
    pub cwd: PathBuf,
    /// Diff target passed to `git diff` (`"HEAD"`, branch name,
    /// SHA). Surfaced in the scan-failed notice and in the markdown
    /// comment bundle so the agent sees what was reviewed.
    pub target: String,
    /// Files in the diff, in the order the scanner returned them.
    pub files: Vec<FileHunks>,
    /// Whether the scanner finished cleanly. `false` when one of
    /// the underlying `git` calls hit Failed / Oversize - the
    /// renderer surfaces a distinct empty-state message so the
    /// user knows to retry rather than concluding "no changes."
    pub scanner_ok: bool,
    /// Count of untracked files that were suppressed because the
    /// working tree exceeded `MAX_UNTRACKED_FILES` in the scanner.
    /// Zero when the tree was under the cap. Surfaced in the rail
    /// as a "+N untracked suppressed" row so a fresh-repo state
    /// doesn't render identically to a clean tree.
    pub untracked_suppressed: usize,
    /// Scroll offset (in rows) across the whole concatenated document
    /// of every file's diff. `u32` because a large multi-file diff can
    /// exceed `u16` rows. The single source of vertical scroll truth.
    pub doc_scroll: u32,
    /// Unified (default) vs split body layout. Toggled by `t`; flips
    /// the whole document. Invalidates the measured-height cache (the
    /// two modes have different row counts) but not the span cache.
    pub view_mode: DiffViewMode,
    /// File indices of deleted files the user has expanded. Deleted
    /// files render collapsed (a one-line notice) by default; a
    /// membership here means "expanded". Non-deleted files are always
    /// expanded and never appear here.
    pub deleted_expanded: std::collections::HashSet<usize>,
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
    /// scrolled). The renderer sets this to 1 - the sticky file header
    /// for the file at the top of the viewport stays pinned while its
    /// body scrolls beneath it. The click handler reads it to decide
    /// whether `body_tail_scroll` offsets a given row.
    pub body_head_rows: usize,
    /// Row offset the renderer applied to the scrolling tail this
    /// frame (the `Paragraph::scroll` amount). The click handler adds
    /// it to a body click past the pinned head rows to index
    /// `body_keys`. Distinct from `doc_scroll` (the document position):
    /// this is the within-window tail scroll derived from it each
    /// frame, stashed like the other geometry fields.
    pub body_tail_scroll: usize,
    /// Per-file measured document height in rows, at the current
    /// width + view_mode. `None` = not measured yet (off-screen, or
    /// invalidated); the offset table falls back to a cheap estimate
    /// for `None` entries so the scrollbar stays stable before a file
    /// is measured on window-entry. Length tracks `files`. Cleared on
    /// view_mode flip, deleted-collapse toggle, and width change - the
    /// three things that change a file's wrapped row count.
    pub measured_heights: Vec<Option<u32>>,
    /// Per-file unwrapped syntax-highlighted spans, built once when a
    /// file first enters the viewport window and reused every frame
    /// after. A line's colour is layout-independent, so this is NEVER
    /// invalidated during the overlay's life (scroll, view_mode flip,
    /// resize all leave it valid); a fresh scan is a fresh state with
    /// an empty cache. Length tracks `files`.
    pub highlighted: Vec<Option<FileHighlight>>,
    /// Ordered commit list for the stepper (oldest first). Empty in
    /// whole-diff-only mode (no commits ahead of the target) - the
    /// stepper never renders and the overlay behaves exactly as before.
    pub commits: Vec<CommitMeta>,
    /// Branch under review, for the stepper header. `None` when unknown
    /// (detached HEAD, no snapshot).
    pub branch: Option<String>,
    /// Which slice is currently shown. `WholeDiff` in whole-diff-only
    /// mode; starts at `Commit(0)` when opened in commit mode.
    pub scope: DiffScope,
    /// Lazily-filled per-commit hunk caches, parallel to `commits`.
    /// `None` until the user first navigates to that commit (its scan
    /// runs on first visit). Length tracks `commits`.
    pub commit_cache: Vec<Option<CachedScan>>,
    /// The whole-branch diff, cached for the "All changes" dropdown
    /// entry in commit mode. `None` until first requested.
    pub whole_diff_cache: Option<CachedScan>,
    /// True while the current scope's lazy scan is in flight - the body
    /// shows a "loading" notice instead of "(no changes)".
    pub commit_loading: bool,
    /// Jump-dropdown open state.
    pub jump_open: bool,
    /// Highlighted row in the jump dropdown: `0` = "All changes",
    /// `1..=commits.len()` = `commits[idx - 1]`.
    pub jump_selected: usize,
    /// Screen span of the `⌄ jump` stepper control - `(row, col_start,
    /// col_end)` - stashed each render so the mouse handler can resolve
    /// a click on it into "open the dropdown". `None` until the first
    /// commit-mode render.
    pub jump_hint_span: Option<(u16, u16, u16)>,
}

impl DiffOverlayState {
    /// Refresh [`Self::comment_counts`] after mutating
    /// [`Self::comments`]. Cheap (`O(comments)`) and only runs on
    /// save / cancel / file switch, not on render. Resets the
    /// per-file slot to zero before counting so removed comments
    /// don't linger. Counts only comments in the current scope - a
    /// comment's `file_idx` indexes its own scope's file set, so
    /// tallying an off-scope comment would point at the wrong file.
    pub fn recompute_comment_counts(&mut self) {
        self.comment_counts.clear();
        self.comment_counts.resize(self.files.len(), 0);
        let sha = self.current_commit_sha();
        for c in &self.comments {
            if c.commit != sha {
                continue;
            }
            if let Some(slot) = self.comment_counts.get_mut(c.key.file_idx) {
                *slot = slot.saturating_add(1);
            }
        }
    }

    /// The current scope's commit sha (`None` in whole-diff scope).
    /// Stamped onto new comments and used to scope rendering + counts.
    pub fn current_commit_sha(&self) -> Option<String> {
        match self.scope {
            DiffScope::WholeDiff => None,
            DiffScope::Commit(i) => self.commits.get(i).map(|c| c.sha.clone()),
        }
    }

    /// Comments belonging to the current scope (matching commit sha, or
    /// `None` in whole-diff). The renderer indexes only these so a
    /// comment's `LineKey` - relative to its own scope's file set -
    /// never renders against a different scope's files.
    pub fn scoped_comments(&self) -> Vec<&HunkComment> {
        let sha = self.current_commit_sha();
        self.comments.iter().filter(|c| c.commit == sha).collect()
    }

    /// Cached scan for `scope`, if it has been fetched.
    fn cached_for(&self, scope: DiffScope) -> Option<&CachedScan> {
        match scope {
            DiffScope::WholeDiff => self.whole_diff_cache.as_ref(),
            DiffScope::Commit(i) => self.commit_cache.get(i).and_then(Option::as_ref),
        }
    }

    /// Swap the displayed file set (on a scope change) and reset every
    /// per-file-indexed cache so a stale height / highlight / count from
    /// the previous scope can't leak. `doc_scroll` / `rail_scroll` reset
    /// to the top; `deleted_expanded` clears (indices were per-file-set);
    /// `comments` persist and are re-tallied for the new scope. Callers
    /// must close any open editor (preserving its prior) first.
    fn set_files(&mut self, files: Vec<FileHunks>, scanner_ok: bool, untracked_suppressed: usize) {
        let n = files.len();
        self.files = files;
        self.scanner_ok = scanner_ok;
        self.untracked_suppressed = untracked_suppressed;
        self.doc_scroll = 0;
        self.rail_scroll = 0;
        self.deleted_expanded.clear();
        self.active_input = None;
        self.measured_heights = vec![None; n];
        self.highlighted = vec![None; n];
        self.body_keys.clear();
        self.commit_loading = false;
        self.recompute_comment_counts();
    }

    /// Point the overlay at `scope`, closing the jump dropdown. If the
    /// scope's hunks are cached, swap them in and report `NavOutcome::Ready`;
    /// otherwise mark it loading and report `NavOutcome::NeedsScan` so
    /// the caller can spawn the lazy fetch. Commit scopes never carry
    /// untracked files (a commit's diff is closed over its own changes),
    /// so the untracked count is 0.
    pub fn select_scope(&mut self, scope: DiffScope) -> NavOutcome {
        self.scope = scope;
        self.jump_open = false;
        // Snapshot the cache to an owned value so the borrow ends before
        // set_files takes &mut self.
        let cached = self.cached_for(scope).map(|c| (c.files.clone(), c.scanner_ok));
        if let Some((files, scanner_ok)) = cached {
            self.set_files(files, scanner_ok, 0);
            NavOutcome::Ready
        } else {
            self.set_files(Vec::new(), true, 0);
            self.commit_loading = true;
            NavOutcome::NeedsScan(scope)
        }
    }

    /// Store a completed scan into the cache for `scope`, and (when it's
    /// still the current scope) swap it into view. Called from the drain
    /// pump when a lazy per-scope scan lands. A scan that arrives after
    /// the user moved on still caches, so returning is instant. Clones
    /// the files only when the scope is current (needed in both the
    /// cache and the view); an off-scope result moves straight in.
    /// `commit_body` (when the scope is a commit) fills that commit's
    /// message body for the message block above its diff.
    pub fn install_scan(
        &mut self,
        scope: DiffScope,
        files: Vec<FileHunks>,
        scanner_ok: bool,
        commit_body: Option<String>,
    ) {
        if let (DiffScope::Commit(i), Some(body)) = (scope, commit_body)
            && let Some(commit) = self.commits.get_mut(i)
        {
            commit.body = body;
        }
        if self.scope == scope {
            self.put_scope_cache(scope, CachedScan { files: files.clone(), scanner_ok });
            self.set_files(files, scanner_ok, 0);
        } else {
            self.put_scope_cache(scope, CachedScan { files, scanner_ok });
        }
    }

    /// Store `cached` in the slot for `scope`.
    fn put_scope_cache(&mut self, scope: DiffScope, cached: CachedScan) {
        match scope {
            DiffScope::WholeDiff => self.whole_diff_cache = Some(cached),
            DiffScope::Commit(i) => {
                if let Some(slot) = self.commit_cache.get_mut(i) {
                    *slot = Some(cached);
                }
            }
        }
    }

    /// Step to the previous / next commit, clamped at the ends. From the
    /// whole-diff ("All changes") view both arrows re-enter the stepper
    /// at the first commit. No-op (returns `None`) in whole-diff-only
    /// mode or when already at the clamped boundary.
    pub fn step_commit(&mut self, forward: bool) -> Option<NavOutcome> {
        if self.commits.is_empty() {
            return None;
        }
        let last = self.commits.len() - 1;
        let next = match self.scope {
            DiffScope::WholeDiff => 0,
            DiffScope::Commit(i) => {
                if forward {
                    (i + 1).min(last)
                } else {
                    i.saturating_sub(1)
                }
            }
        };
        if self.scope == DiffScope::Commit(next) {
            return None;
        }
        Some(self.select_scope(DiffScope::Commit(next)))
    }

    /// Number of jump-dropdown rows: "All changes" + one per commit.
    pub fn jump_row_count(&self) -> usize {
        self.commits.len() + 1
    }

    /// Scope a jump-dropdown row maps to (`0` = "All changes").
    pub fn scope_for_jump_row(row: usize) -> DiffScope {
        if row == 0 { DiffScope::WholeDiff } else { DiffScope::Commit(row - 1) }
    }

    /// Open the jump dropdown, seeding the highlight on the current scope.
    pub fn open_jump(&mut self) {
        self.jump_selected = match self.scope {
            DiffScope::WholeDiff => 0,
            DiffScope::Commit(i) => i + 1,
        };
        self.jump_open = true;
    }

    /// Move the dropdown highlight (clamped to the row range).
    pub fn jump_move(&mut self, down: bool) {
        let last = self.jump_row_count().saturating_sub(1);
        self.jump_selected = if down {
            (self.jump_selected + 1).min(last)
        } else {
            self.jump_selected.saturating_sub(1)
        };
    }

    /// Confirm the highlighted dropdown row: close the menu and navigate
    /// to its scope.
    pub fn jump_confirm(&mut self) -> NavOutcome {
        self.select_scope(Self::scope_for_jump_row(self.jump_selected))
    }

    /// Drop every measured height so the next frame re-measures
    /// lazily. Called on view_mode flip and width change - both
    /// change the wrapped row count for every file. The span cache is
    /// deliberately left intact (a line's colour is layout-independent).
    pub fn invalidate_measured_heights(&mut self) {
        for height in &mut self.measured_heights {
            *height = None;
        }
    }

    /// Drop the measured heights when the body width changed since the
    /// last frame (compared against the stashed `pane_width`). Soft-wrap
    /// makes every wrapped row count width-dependent, so a resize / pane
    /// reflow leaves them stale; the renderer calls this before reading
    /// the offset table. Span cache is width-independent and untouched.
    pub fn invalidate_heights_if_width_changed(&mut self, width: u16) {
        if self.pane_width != width {
            self.invalidate_measured_heights();
        }
    }

    /// True when file `idx` renders as the one-line collapsed notice -
    /// a deleted file the user hasn't expanded.
    pub fn is_collapsed(&self, idx: usize) -> bool {
        self.files.get(idx).is_some_and(|f| f.status == FileStatus::Deleted)
            && !self.deleted_expanded.contains(&idx)
    }

    /// Document offset table for the current frame: each file's height
    /// is its measured value when known, else the cheap estimate. Used
    /// to find the file at the top of the viewport, jump the scroll
    /// from the rail, and size the scrollbar.
    pub fn doc_offsets(&self) -> DocOffsets {
        let heights: Vec<u32> = self
            .files
            .iter()
            .enumerate()
            .map(|(idx, file)| {
                self.measured_heights
                    .get(idx)
                    .copied()
                    .flatten()
                    .unwrap_or_else(|| estimated_height(file, self.is_collapsed(idx)))
            })
            .collect();
        file_offsets(&heights)
    }

    /// Build a fresh state for a newly-opened overlay. Test-only -
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
            doc_scroll: 0,
            view_mode: DiffViewMode::default(),
            deleted_expanded: std::collections::HashSet::new(),
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
            body_tail_scroll: 0,
            measured_heights: vec![None; file_count],
            highlighted: vec![None; file_count],
            commits: Vec::new(),
            branch: None,
            scope: DiffScope::WholeDiff,
            commit_cache: Vec::new(),
            whole_diff_cache: None,
            commit_loading: false,
            jump_open: false,
            jump_selected: 0,
            jump_hint_span: None,
        }
    }

    /// Build state from a completed initial-scan event, threading
    /// scanner outcome flags through so the renderer can surface
    /// partial-failure and cap-overflow conditions. When the event
    /// carries a commit list, opens in commit mode on the first commit
    /// (its diff is `event.files`, cached at slot 0); otherwise
    /// whole-diff mode, exactly as before.
    fn new_initial(event: DiffOverlayEvent) -> Self {
        let DiffOverlayEvent {
            cwd,
            target,
            files,
            scanner_ok,
            untracked_suppressed,
            seq: _,
            kind,
            commit_body,
        } = event;
        // drain_events only routes Initial events here; a Scope event
        // would be a bug, so fall back to whole-diff defensively.
        let (mut commits, branch) = match kind {
            DiffScanKind::Initial { commits, branch } => (commits, branch),
            DiffScanKind::Scope(_) => (Vec::new(), None),
        };
        // First commit's body arrives with its upfront-scanned diff.
        if let Some(body) = commit_body
            && let Some(first) = commits.first_mut()
        {
            first.body = body;
        }
        let file_count = files.len();
        let (scope, commit_cache, whole_diff_cache) = if commits.is_empty() {
            (
                DiffScope::WholeDiff,
                Vec::new(),
                Some(CachedScan { files: files.clone(), scanner_ok }),
            )
        } else {
            let mut cache = vec![None; commits.len()];
            cache[0] = Some(CachedScan { files: files.clone(), scanner_ok });
            (DiffScope::Commit(0), cache, None)
        };
        Self {
            cwd,
            target,
            files,
            scanner_ok,
            untracked_suppressed,
            doc_scroll: 0,
            view_mode: DiffViewMode::default(),
            deleted_expanded: std::collections::HashSet::new(),
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
            body_tail_scroll: 0,
            measured_heights: vec![None; file_count],
            highlighted: vec![None; file_count],
            commits,
            branch,
            scope,
            commit_cache,
            whole_diff_cache,
            commit_loading: false,
            jump_open: false,
            jump_selected: 0,
            jump_hint_span: None,
        }
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
/// bundle submit path lives in [`close_with_submit`] - call this
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
    // Jump dropdown captures keys while open: move / confirm / close.
    // Esc closes the menu (not the overlay).
    if app.diff_overlay.as_ref().is_some_and(|o| o.jump_open) {
        handle_jump_key(app, key);
        return;
    }
    match key.code {
        KeyCode::Esc => close_with_submit(app),
        KeyCode::Char('t') => toggle_view_mode(app),
        KeyCode::Up => scroll_doc(app, false),
        KeyCode::Down => scroll_doc(app, true),
        KeyCode::PageUp => scroll_doc_page(app, false),
        KeyCode::PageDown => scroll_doc_page(app, true),
        // Commit stepper: prev/next commit + open the jump dropdown. All
        // no-ops in whole-diff-only mode (no commits), so they don't
        // shadow anything there.
        KeyCode::Char('[') | KeyCode::Left => step_commit(app, false),
        KeyCode::Char(']') | KeyCode::Right => step_commit(app, true),
        KeyCode::Char('j') => open_jump(app),
        _ => {}
    }
}

/// Route a key while the jump dropdown is open. `↑↓` move the
/// highlight, `Enter` navigates to the highlighted scope, and `Esc`
/// (or `j`) closes the menu without touching the overlay.
fn handle_jump_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up => {
            if let Some(o) = app.diff_overlay.as_mut() {
                o.jump_move(false);
                app.needs_redraw = true;
            }
        }
        KeyCode::Down => {
            if let Some(o) = app.diff_overlay.as_mut() {
                o.jump_move(true);
                app.needs_redraw = true;
            }
        }
        KeyCode::Enter => {
            let outcome = app.diff_overlay.as_mut().map(DiffOverlayState::jump_confirm);
            if let Some(outcome) = outcome {
                after_nav(app, outcome);
            }
        }
        KeyCode::Esc | KeyCode::Char('j') => {
            if let Some(o) = app.diff_overlay.as_mut() {
                o.jump_open = false;
                app.needs_redraw = true;
            }
        }
        _ => {}
    }
}

/// Step to the prev/next commit and spawn its scan if uncached.
fn step_commit(app: &mut App, forward: bool) {
    let outcome = app.diff_overlay.as_mut().and_then(|o| o.step_commit(forward));
    if let Some(outcome) = outcome {
        after_nav(app, outcome);
    }
}

/// Open the jump dropdown (commit mode only).
fn open_jump(app: &mut App) {
    if let Some(o) = app.diff_overlay.as_mut()
        && !o.commits.is_empty()
    {
        o.open_jump();
        app.needs_redraw = true;
    }
}

/// After a navigation, spawn the scope's scan when it wasn't cached, and
/// request a redraw. The scan lands back through the overlay event
/// channel (see [`spawn_scope_fetch`] / [`drain_events`]).
fn after_nav(app: &mut App, outcome: NavOutcome) {
    if let NavOutcome::NeedsScan(scope) = outcome {
        spawn_scope_scan(app, scope);
    }
    app.needs_redraw = true;
}

/// Kick off the lazy scan for `scope` against the overlay's cwd/target,
/// reusing the current scan seq (no bump - it's the same overlay
/// session, not a fresh `/diff`).
fn spawn_scope_scan(app: &mut App, scope: DiffScope) {
    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    let cwd = overlay.cwd.clone();
    let target = overlay.target.clone();
    let sha = match scope {
        DiffScope::WholeDiff => None,
        DiffScope::Commit(i) => overlay.commits.get(i).map(|c| c.sha.clone()),
    };
    let seq = app.diff_scan_seq;
    spawn_scope_fetch(cwd, target, scope, sha, seq, app.diff_overlay_event_tx.clone());
}

/// Flip the body layout (unified <-> split) and drop the measured
/// heights - the two modes have different row counts. The span cache
/// is layout-independent and stays intact, so the toggle is instant
/// (no re-highlight).
fn toggle_view_mode(app: &mut App) {
    if let Some(overlay) = app.diff_overlay.as_mut() {
        overlay.view_mode = match overlay.view_mode {
            DiffViewMode::Unified => DiffViewMode::Split,
            DiffViewMode::Split => DiffViewMode::Unified,
        };
        overlay.invalidate_measured_heights();
        app.needs_redraw = true;
    }
}

/// Step the document scroll by one row. The renderer clamps against
/// the document height and viewport each frame, so this just nudges
/// `doc_scroll` and lets render bound it.
fn scroll_doc(app: &mut App, down: bool) {
    if let Some(overlay) = app.diff_overlay.as_mut() {
        overlay.doc_scroll = if down {
            overlay.doc_scroll.saturating_add(1)
        } else {
            overlay.doc_scroll.saturating_sub(1)
        };
        app.needs_redraw = true;
    }
}

/// Page the document scroll by roughly a viewport (the last rendered
/// frame height minus the hint-bar row). Render clamps the result.
fn scroll_doc_page(app: &mut App, down: bool) {
    let page = u32::from(app.cached_frame_area.height.saturating_sub(1)).max(1);
    if let Some(overlay) = app.diff_overlay.as_mut() {
        overlay.doc_scroll = if down {
            overlay.doc_scroll.saturating_add(page)
        } else {
            overlay.doc_scroll.saturating_sub(page)
        };
        app.needs_redraw = true;
    }
}

/// Route bracketed paste into the active comment editor. Returns
/// `true` when the paste was consumed (editor present), `false`
/// otherwise so the caller can fall through. Plain pastes inside
/// the diff overlay outside a comment editor are dropped - there's
/// nothing for them to land on - but a DEBUG log fires so a user
/// reporting "my paste disappeared" can be triaged from logs.
pub(crate) fn handle_paste(app: &mut App, text: &str) -> bool {
    let Some(overlay) = app.diff_overlay.as_mut() else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_paste_dropped_no_overlay",
            message = "paste in Diff view without overlay state - dropped",
            outcome = "dropped",
            paste_chars = text.chars().count(),
        );
        return false;
    };
    let Some(input) = overlay.active_input.as_mut() else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_paste_dropped_no_editor",
            message = "paste in Diff view without an open comment editor - dropped",
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
/// Esc would silently destroy the saved comment - the exact bug
/// `prior_comment` was added to prevent.
///
/// Logs DEBUG with the abandoned char count when text is dropped
/// (fresh draft, or modifications layered on a reopened chip), so
/// a "where did my edit go?" triage can correlate from logs.
/// Returns the abandoned count as a Unicode scalar count for
/// callers that want it - most don't, but the central log fires
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
/// comment so the chip reappears - the user clicked to view/edit,
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
///   cancel - saving a blank comment would render an empty chip.
/// - Reopened chip editor (with `prior_comment`): treated as
///   delete - the user cleared all text and pressed Enter to
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
            message = "save_active_input hit oob file_idx - body mutated mid-open?",
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
            message = "save_active_input hit oob hunk_idx - body mutated mid-open?",
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
            message = "save_active_input hit oob line_idx - body mutated mid-open?",
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
    let commit = overlay.current_commit_sha();
    let comment = HunkComment {
        key,
        path: file.path.clone(),
        line: line_no,
        hunk_context: vec![diff_line.clone()],
        comment_text: text,
        commit,
    };
    // Replace any existing comment at the same key (saving an
    // edited reopen).
    overlay.comments.retain(|c| c.key != key);
    overlay.comments.push(comment);
    overlay.recompute_comment_counts();
    app.needs_redraw = true;
}

/// Lines scrolled per wheel notch in the diff body. Same value as
/// `crate::app::events::mouse::MOUSE_SCROLL_LINES`; applied to the
/// `u32` document scroll (`doc_scroll`).
const SCROLL_LINES_PER_NOTCH: u16 = 3;

/// Minimum FILES rail width when the rail is shown. Below this the
/// file list becomes unreadably narrow; we hide the rail entirely.
pub(crate) const RAIL_WIDTH_MIN: u16 = 20;
/// Rail width as a fraction of the terminal width: strict 15%. The
/// remaining 85% goes to the two-pane diff body, split evenly with
/// any odd leftover column handed to the right half so a `+`
/// addition column never reads as narrower than its `-` counterpart.
pub(crate) const RAIL_WIDTH_NUMER: u16 = 15;
pub(crate) const RAIL_WIDTH_DENOM: u16 = 100;
/// Medium-tier terminal width threshold (≥ this → rail visible).
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
    if terminal_width < MEDIUM_MIN {
        return 0;
    }
    let proportional = terminal_width.saturating_mul(RAIL_WIDTH_NUMER) / RAIL_WIDTH_DENOM;
    proportional.max(RAIL_WIDTH_MIN)
}

/// Outcome of a mouse interaction. Some interactions need access
/// to the full App (key event needs to fire `dispatch_prompt` for
/// the Esc-bundle path) which the inner `handle_*` borrow doesn't
/// have - surface them as effects the outer `handle_mouse` runs.
#[derive(Debug, Default)]
struct MouseEffect {
    redraw: bool,
}

/// Handle a mouse event while the diff overlay is active.
///
/// Bindings:
/// - Scroll wheel over the rail → advance `rail_scroll`.
/// - Scroll wheel over the body → advance `doc_scroll` (the single
///   document scroll across all files).
/// - Left click on a file row in the FILES rail → jump `doc_scroll`
///   to that file's first row.
/// - Left click on a diff line in the body → open an inline comment
///   input anchored at that line. (If an input is already open, the
///   click cancels it before opening the new one.)
/// - Left click on a saved-comment chip → re-open that comment for
///   editing.
/// - Left click on a collapsed deleted file's header → expand it.
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
            // Drags, other buttons, and horizontal-wheel events have no
            // binding in the overlay.
            _ => MouseEffect::default(),
        }
    } else {
        MouseEffect::default()
    };
    if effect.redraw {
        app.needs_redraw = true;
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
        overlay.doc_scroll = overlay.doc_scroll.saturating_add(u32::from(SCROLL_LINES_PER_NOTCH));
    } else {
        overlay.doc_scroll = overlay.doc_scroll.saturating_sub(u32::from(SCROLL_LINES_PER_NOTCH));
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
    // `⌄ jump` control on the stepper row → toggle the dropdown.
    if let Some((jr, c0, c1)) = overlay.jump_hint_span
        && row == jr
        && column >= c0
        && column < c1
    {
        if overlay.jump_open {
            overlay.jump_open = false;
        } else {
            overlay.open_jump();
        }
        return MouseEffect { redraw: true };
    }
    // Any other click with the dropdown open closes it (click-away).
    if overlay.jump_open {
        overlay.jump_open = false;
        return MouseEffect { redraw: true };
    }
    let rail_width = rail_width_for(terminal_width);
    // Rail click: column < rail_width → rail row hit-test.
    if rail_width > 0 && column < rail_width {
        return handle_rail_click(overlay, row);
    }
    // Body click: column past rail+separator. Resolve via body_keys.
    // When the rail isn't rendered (terminal narrower than the split
    // threshold), the renderer paints a "too narrow" notice and
    // clears `body_keys` - clicks just no-op.
    handle_body_click(overlay, column, row)
}

fn handle_rail_click(overlay: &mut DiffOverlayState, row: u16) -> MouseEffect {
    // The tree rail mixes directory headers (non-clickable) with
    // file leaves. We resolve the click by walking `rail_keys`
    // (parallel to the rendered rows) at offset `rail_scroll`.
    // The banner / rule / blank rows live at the head of the list
    // and don't scroll - they're always at the absolute screen
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
        // Banner / rule / blank / directory / untracked-notice -
        // non-clickable in v1.
        return MouseEffect::default();
    };
    if file_idx >= overlay.files.len() {
        return MouseEffect::default();
    }
    // Jump the document scroll to this file's first row. Closing the
    // active editor on rail interaction preserves a reopened chip's
    // prior comment.
    overlay.doc_scroll = overlay.doc_offsets().starts.get(file_idx).copied().unwrap_or(0);
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
    // The first `body_head_rows` rows are pinned (the sticky file
    // header) and don't scroll, so they map directly to
    // `body_keys[local_row]`. Rows past the head add the tail scroll
    // the renderer applied this frame.
    let head = overlay.body_head_rows;
    let body_idx = if local_row < head {
        Some(local_row)
    } else {
        local_row.checked_add(overlay.body_tail_scroll)
    };
    let Some(idx) = body_idx else {
        return MouseEffect::default();
    };
    let Some(key) = overlay.body_keys.get(idx).copied() else {
        return MouseEffect::default();
    };
    match key {
        BodyRowKey::HunkRow { left, right } => {
            // Unified is one column, so either side resolves the
            // line. Split has two columns: the divider sits at the
            // pane midpoint, so clicks left of it pick the old side,
            // right of it the new side. An empty picked side (blank
            // half of an unbalanced split row) is a no-op.
            let key = match overlay.view_mode {
                DiffViewMode::Unified => left.or(right),
                DiffViewMode::Split => {
                    let pane_local_col = column.saturating_sub(overlay.pane_origin_col);
                    let mid_col = overlay.pane_width / 2;
                    if pane_local_col < mid_col { left } else { right }
                }
            };
            match key {
                Some(key) => open_input_for_key(overlay, key),
                None => MouseEffect::default(),
            }
        }
        BodyRowKey::CommentChip(line_key) => reopen_comment_for_key(overlay, line_key),
        BodyRowKey::FileHeader { file_idx } | BodyRowKey::DeletedCollapsed { file_idx } => {
            toggle_deleted_collapse(overlay, file_idx)
        }
        BodyRowKey::EmptyState
        | BodyRowKey::HunkHeader { .. }
        | BodyRowKey::InputRow(_)
        | BodyRowKey::CommitMessage => MouseEffect::default(),
    }
}

/// Toggle a deleted file's expanded state (collapse <-> full body).
/// Only deleted files collapse; a click on any other file's header is
/// a no-op. Clears the file's measured height so the next frame
/// re-measures it at the new row count.
fn toggle_deleted_collapse(overlay: &mut DiffOverlayState, file_idx: usize) -> MouseEffect {
    if overlay.files.get(file_idx).map(|f| f.status) != Some(FileStatus::Deleted) {
        return MouseEffect::default();
    }
    if !overlay.deleted_expanded.insert(file_idx) {
        overlay.deleted_expanded.remove(&file_idx);
    }
    if let Some(slot) = overlay.measured_heights.get_mut(file_idx) {
        *slot = None;
    }
    MouseEffect { redraw: true }
}

fn open_input_for_key(overlay: &mut DiffOverlayState, key: LineKey) -> MouseEffect {
    // If an editor is already open at the same key, no-op so the
    // click doesn't reset its in-progress text. If at a different
    // key, abandon the in-progress edit (UI matches what GitHub does
    // - clicking elsewhere closes the open editor without saving).
    if let Some(existing) = overlay.active_input.as_ref()
        && existing.key == key
    {
        return MouseEffect::default();
    }
    // Close any existing editor (different line) before opening the
    // new one - preserves its prior_comment if it was a reopen.
    close_active_input_preserving_prior(overlay);
    let editor = TextArea::default();
    overlay.active_input = Some(ActiveCommentInput { key, editor, prior_comment: None });
    MouseEffect { redraw: true }
}

fn reopen_comment_for_key(overlay: &mut DiffOverlayState, key: LineKey) -> MouseEffect {
    // Find the saved comment, hydrate a fresh TextArea from its
    // text, drop the saved entry so the chip vanishes WHILE editing
    // (but stash it on `prior_comment` so Esc-cancel can restore it
    // - losing the saved comment to a misclick-and-reflex-Esc would
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
/// is REFUSED - a system message tells the user to retry once the
/// session connects + a WARN log lets an operator grep for the
/// held state. Without this, `dispatch_prompt`'s silent no-agent
/// path would drop the bundle on the floor and the user would
/// lose their review notes.
pub(super) fn close_with_submit(app: &mut App) {
    // Flush the active editor BEFORE the pending check - a reopened
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
    // in practice - confusing for future maintainers.
    let snapshot = app.diff_overlay.as_mut().map(|o| {
        let comments = std::mem::take(&mut o.comments);
        (
            comments,
            o.target.clone(),
            o.cwd.display().to_string(),
            o.branch.clone(),
            o.commits.clone(),
        )
    });
    if let Some((comments, target, cwd_display, branch, commits)) = snapshot
        && !comments.is_empty()
    {
        let markdown =
            format_diff_comments(&target, branch.as_deref(), &cwd_display, &commits, &comments);
        super::input_submit::dispatch_diff_comment_bundle(app, markdown);
    }
    close(app);
}

/// Build the markdown bundle for a set of pending comments. Public
/// for the Esc-submit path and the test suite. The shape depends on
/// whether ANY comment is commit-scoped:
/// - None are (a pure whole-diff session): today's shape exactly -
///   `## Diff review (target \`<target>\`)` then per-file
///   `### \`<path>\`` groups. This is the only byte-identical path.
/// - Some are (commit mode, including a mixed session that also left
///   comments on "All changes"): the commit-grouped shape - a
///   `<branch> vs <target>` header with the comment/commit totals, a
///   `### Commit \`<sha>\` - <subject>` section per commit in stepper
///   order, and any whole-diff comments trailing under `### All
///   changes`. An All-changes comment made in a commit-mode session
///   therefore renders grouped, not in the file-grouped whole-diff
///   shape.
pub(crate) fn format_diff_comments(
    target: &str,
    branch: Option<&str>,
    cwd_display: &str,
    commits: &[CommitMeta],
    comments: &[HunkComment],
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let commit_scoped = comments.iter().any(|c| c.commit.is_some());
    if !commit_scoped {
        // Byte-identical to the pre-stepper bundle.
        let _ = writeln!(out, "## Diff review (target `{target}`)");
        out.push('\n');
        write_repo_line(&mut out, cwd_display);
        write_by_file(&mut out, comments.iter());
        return out;
    }

    let total = comments.len();
    let commit_hits = commits
        .iter()
        .filter(|c| comments.iter().any(|x| x.commit.as_deref() == Some(&c.sha)))
        .count();
    let header_lead = match branch {
        Some(b) => format!("{b} vs {target}"),
        None => target.to_owned(),
    };
    let _ = writeln!(
        out,
        "## Diff review ({header_lead}, {total} comment{} across {commit_hits} commit{})",
        if total == 1 { "" } else { "s" },
        if commit_hits == 1 { "" } else { "s" },
    );
    out.push('\n');
    write_repo_line(&mut out, cwd_display);

    for commit in commits {
        let mut scoped =
            comments.iter().filter(|c| c.commit.as_deref() == Some(&commit.sha)).peekable();
        if scoped.peek().is_none() {
            continue;
        }
        let _ = writeln!(out, "### Commit `{}` - {}", commit.short_sha, commit.subject);
        out.push('\n');
        for c in scoped {
            let _ = writeln!(out, "**`{}` - line {}**", c.path, c.line);
            out.push('\n');
            write_anchor_and_text(&mut out, c);
        }
    }

    // Any comments left on the whole-branch "All changes" view.
    let mut whole = comments.iter().filter(|c| c.commit.is_none()).peekable();
    if whole.peek().is_some() {
        let _ = writeln!(out, "### All changes");
        out.push('\n');
        for c in whole {
            let _ = writeln!(out, "**`{}` - line {}**", c.path, c.line);
            out.push('\n');
            write_anchor_and_text(&mut out, c);
        }
    }
    out
}

/// Emit the ``Repo: `<cwd>` `` line (blank cwd suppresses it).
fn write_repo_line(out: &mut String, cwd_display: &str) {
    use std::fmt::Write as _;
    if !cwd_display.is_empty() {
        let _ = writeln!(out, "Repo: `{cwd_display}`");
        out.push('\n');
    }
}

/// Whole-diff bundle body: group comments by file path (first-seen
/// order), `### \`<path>\`` then `**Line N**` + the quoted anchor per
/// comment. This is the exact pre-stepper shape.
fn write_by_file<'a>(out: &mut String, comments: impl Iterator<Item = &'a HunkComment>) {
    use std::fmt::Write as _;
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
            write_anchor_and_text(out, c);
        }
    }
}

/// Emit the quoted anchor-line ```diff block plus the comment text -
/// the shared tail of both bundle shapes.
fn write_anchor_and_text(out: &mut String, c: &HunkComment) {
    use std::fmt::Write as _;
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
    fn new_state_defaults_unified_and_doc_scroll_zero() {
        let state = sample_state();
        assert_eq!(state.view_mode, DiffViewMode::Unified);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn file_offsets_are_prefix_sums_of_heights() {
        // heights: file0=10, file1=4, file2=7  -> offsets 0,10,14 ; total 21
        let offsets = file_offsets(&[10, 4, 7]);
        assert_eq!(offsets.starts, vec![0, 10, 14]);
        assert_eq!(offsets.total, 21);
    }

    #[test]
    fn file_index_at_row_finds_owning_file() {
        let offsets = file_offsets(&[10, 4, 7]); // ranges 0..10, 10..14, 14..21
        assert_eq!(offsets.file_at_row(0), 0);
        assert_eq!(offsets.file_at_row(9), 0);
        assert_eq!(offsets.file_at_row(10), 1);
        assert_eq!(offsets.file_at_row(13), 1);
        assert_eq!(offsets.file_at_row(14), 2);
        assert_eq!(offsets.file_at_row(100), 2); // past end clamps to last
    }

    #[test]
    fn file_offsets_empty_is_total_zero() {
        let offsets = file_offsets(&[]);
        assert!(offsets.starts.is_empty());
        assert_eq!(offsets.total, 0);
    }

    #[test]
    fn invalidate_measured_heights_clears_all_preserving_len() {
        let mut state = sample_state();
        state.measured_heights = vec![Some(10), Some(4)];
        state.invalidate_measured_heights();
        assert!(state.measured_heights.iter().all(Option::is_none));
        assert_eq!(state.measured_heights.len(), 2, "length tracks files");
    }

    #[test]
    fn width_change_invalidates_measured_heights() {
        let mut state = sample_state();
        state.pane_width = 80;
        state.measured_heights = vec![Some(10), Some(4)];
        // Same width: the wrapped-row counts are still valid.
        state.invalidate_heights_if_width_changed(80);
        assert_eq!(state.measured_heights, vec![Some(10), Some(4)], "same width keeps the cache");
        // Changed width (resize / pane reflow): drop the stale heights.
        state.invalidate_heights_if_width_changed(120);
        assert!(
            state.measured_heights.iter().all(Option::is_none),
            "a width change drops the height cache so the next frame re-measures",
        );
    }

    // The rail file-leaf click now JUMPS `doc_scroll` to the file's
    // document offset instead of switching `current_file_idx`. That
    // jump needs the offset table + height cache, so the positive
    // jump assertion lives with the continuous body. The no-op rail
    // paths below stay valid regardless.

    #[test]
    fn rail_click_outside_rail_routes_to_body() {
        // A click past the rail routes into handle_body_click, which
        // finds no body_keys in a freshly-constructed state - no-redraw.
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 50, 4, 160);
        assert!(!effect.redraw);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn rail_click_on_banner_returns_no_redraw() {
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 0, 160); // Banner row.
        assert!(!effect.redraw);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn rail_click_beyond_file_list_returns_no_redraw() {
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 99, 160); // No file at this row.
        assert!(!effect.redraw);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn rail_click_at_narrow_tier_routes_to_body() {
        // Narrow tier: rail_width == 0 → click routes to body
        // hit-test, which finds no body_keys in a fresh state.
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 4, 100);
        assert!(!effect.redraw);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn body_click_left_column_opens_comment_input_on_left_key() {
        // Split row with both columns present; a click in the left
        // half resolves to the left key (split picks by column).
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let left_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(left_key), right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41; // Past rail + separator on wide.
        state.pane_width = 119;
        // Left half: pane-local col in [0, 59) → click_col in [41, 100).
        let effect = handle_left_click(&mut state, 60, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(left_key));
    }

    #[test]
    fn body_click_right_column_opens_comment_input_on_right_key() {
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let left_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(left_key), right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Right half: pane-local col in [60, 119) → click_col in [101, 160).
        let effect = handle_left_click(&mut state, 120, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(right_key));
    }

    #[test]
    fn body_click_on_empty_side_is_noop() {
        // Split-only: clicking the blank half of an unbalanced row
        // (left = None) is a no-op. Unified would resolve right.
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: None, right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Click in the (blank) LEFT half - left=None, so no editor opens.
        let effect = handle_left_click(&mut state, 60, 2, 160);
        assert!(!effect.redraw);
        assert!(state.active_input.is_none());
    }

    #[test]
    fn body_click_unified_resolves_either_column_to_the_line() {
        // Unified is one column: a click anywhere on the row opens the
        // comment, even the left half of an added/context row whose
        // key sits on the right. (Split would no-op the empty left.)
        let mut state = sample_state(); // view_mode defaults to Unified
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: None, right: Some(key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 2, 160); // left half
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(key));
    }

    #[test]
    fn t_key_toggles_view_mode_and_clears_height_cache() {
        let mut app = App::test_default();
        let mut state = sample_state();
        state.measured_heights = vec![Some(10), Some(4)];
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('t')));
        let o = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(o.view_mode, DiffViewMode::Split);
        assert!(
            o.measured_heights.iter().all(Option::is_none),
            "height cache invalidated on toggle",
        );
    }

    #[test]
    fn down_key_advances_doc_scroll() {
        let mut app = App::test_default();
        app.diff_overlay = Some(sample_state());
        set_active_view(&mut app, ActiveView::Diff);
        handle_key(&mut app, KeyEvent::from(KeyCode::Down));
        assert_eq!(app.diff_overlay.as_ref().expect("overlay").doc_scroll, 1);
    }

    #[test]
    fn rail_click_jumps_doc_scroll_to_file_offset() {
        let mut state = sample_state(); // 2 files
        // Give file 0 a measured height of 10 so file 1 starts at row 10.
        state.measured_heights = vec![Some(10), Some(4)];
        let effect = handle_left_click(&mut state, 5, 4, 160); // rail row 4 = file idx 1
        assert!(effect.redraw);
        assert_eq!(state.doc_scroll, 10, "rail click jumps to the file's document offset");
    }

    #[test]
    fn body_click_on_deleted_header_toggles_expand() {
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![FileHunks { path: "gone.rs".into(), status: FileStatus::Deleted, hunks: vec![] }],
        );
        state.measured_heights = vec![Some(2)];
        state.body_keys = vec![BodyRowKey::FileHeader { file_idx: 0 }];
        state.body_head_rows = 1;
        state.pane_origin_row = 0;
        state.pane_origin_col = 24;
        state.pane_width = 120;
        // Click the pinned header (row 0, within the head). Column past
        // the rail so it routes to the body.
        let effect = handle_left_click(&mut state, 50, 0, 160);
        assert!(effect.redraw);
        assert!(state.deleted_expanded.contains(&0), "deleted header click expands");
        assert!(state.measured_heights[0].is_none(), "height invalidated on toggle");
        // Click again collapses.
        let effect = handle_left_click(&mut state, 50, 0, 160);
        assert!(effect.redraw);
        assert!(!state.deleted_expanded.contains(&0), "second click collapses again");
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
            commit: None,
        });
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(key), right: Some(key) },
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
                commit: None,
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
                commit: None,
            },
            HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 1, line_idx: 0 },
                path: "a.rs".into(),
                line: 30,
                hunk_context: vec![],
                comment_text: "missing rationale".into(),
                commit: None,
            },
        ];
        let md = format_diff_comments("HEAD", None, "/tmp/repo", &[], &comments);
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
        let md = format_diff_comments("main", None, "", &[], &[]);
        assert!(md.contains("## Diff review (target `main`)"));
        assert!(!md.contains("Repo: ``"), "blank cwd suppresses the Repo line");
    }

    #[test]
    fn format_diff_comments_groups_by_commit_when_scoped() {
        let commits = vec![
            commit_meta("a3f9c1e", "fix the threshold check"),
            commit_meta("e55f210", "wire the warning banner"),
        ];
        let comments = vec![
            HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
                path: "app/rate_limit.rs".into(),
                line: 66,
                hunk_context: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    text: "fn is_near_threshold() {".into(),
                    old_line: None,
                    new_line: Some(66),
                }],
                comment_text: "name it _without_overage".into(),
                commit: Some("a3f9c1e".into()),
            },
            HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
                path: "ui/banner.rs".into(),
                line: 12,
                hunk_context: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    text: "if snapshot.is_near_threshold() {".into(),
                    old_line: None,
                    new_line: Some(12),
                }],
                comment_text: "hoist this".into(),
                commit: Some("e55f210".into()),
            },
        ];
        let md =
            format_diff_comments("main", Some("worker/rate-limit"), "/repo", &commits, &comments);
        assert!(
            md.contains("## Diff review (worker/rate-limit vs main, 2 comments across 2 commits)"),
            "grouped header names the branch, target, and totals; got:\n{md}",
        );
        assert!(md.contains("Repo: `/repo`"));
        assert!(md.contains("### Commit `a3f9c1e` - fix the threshold check"));
        assert!(md.contains("### Commit `e55f210` - wire the warning banner"));
        assert!(md.contains("**`app/rate_limit.rs` - line 66**"), "per-comment path+line header");
        assert!(md.contains("+fn is_near_threshold() {"), "quotes the anchor line");
        assert!(md.contains("name it _without_overage"));
        // Commit order follows the stepper (a3f9c1e before e55f210).
        let first = md.find("a3f9c1e").expect("first commit");
        let second = md.find("e55f210").expect("second commit");
        assert!(first < second, "commits render oldest-first");
        // No whole-diff header when everything is commit-scoped.
        assert!(!md.contains("## Diff review (target"));
        assert!(!md.contains("### All changes"));
    }

    #[test]
    fn format_diff_comments_singular_counts() {
        let commits = vec![commit_meta("a3f9c1e", "fix it")];
        let comments = vec![HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "x".into(),
            commit: Some("a3f9c1e".into()),
        }];
        let md = format_diff_comments("main", Some("feat"), "", &commits, &comments);
        assert!(
            md.contains("1 comment across 1 commit)"),
            "singular comment/commit wording; got:\n{md}",
        );
    }

    #[test]
    fn format_diff_comments_trailing_all_changes_section() {
        let commits = vec![commit_meta("a3f9c1e", "fix it")];
        let comments = vec![
            HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
                path: "a.rs".into(),
                line: 1,
                hunk_context: vec![],
                comment_text: "on commit".into(),
                commit: Some("a3f9c1e".into()),
            },
            HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
                path: "b.rs".into(),
                line: 2,
                hunk_context: vec![],
                comment_text: "on whole diff".into(),
                commit: None,
            },
        ];
        let md = format_diff_comments("main", Some("feat"), "", &commits, &comments);
        assert!(md.contains("### Commit `a3f9c1e`"), "commit-scoped comment groups by commit");
        assert!(md.contains("### All changes"), "whole-diff comment trails under All changes");
        assert!(md.contains("on whole diff"));
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
            commit: None,
        });
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 1, line_idx: 0 },
            path: "a.rs".into(),
            line: 2,
            hunk_context: vec![],
            comment_text: "y".into(),
            commit: None,
        });
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 1, hunk_idx: 0, line_idx: 0 },
            path: "b.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "z".into(),
            commit: None,
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
        // No active agent in the test default - close_with_submit
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
            commit: None,
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        // Overlay still open + comment still alive - user can retry.
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
            commit: None,
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
            commit: None,
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
            commit: None,
        });
        state.comments.push(HunkComment {
            key: key_b,
            path: "a.rs".into(),
            line: 5,
            hunk_context: vec![],
            comment_text: "B".into(),
            commit: None,
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
            commit: None,
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
        // pre-edit state - the typed-over changes are intentionally
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
            commit: None,
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
        // - no telemetry log fires for "viewed and dismissed".
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "exactly this".into(),
            commit: None,
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
            commit: None,
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
            commit: None,
        };
        let mut editor = TextArea::default();
        editor.insert_str("important review note");
        // Editor open as a chip reopen - prior_comment Some, no
        // unsubmitted comments in overlay.comments yet.
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: Some(prior.clone()) });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "overlay closed");
        // The prior must have made it into the dispatched bundle -
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
            commit: None,
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
        // no-agent state shouldn't block closing - the user just
        // wants to dismiss the overlay.
        let mut app = App::test_default();
        app.diff_overlay = Some(sample_state());
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "empty-comments close still drops state");
        assert_eq!(app.active_view, ActiveView::Chat, "view returns to chat");
    }

    #[test]
    fn rail_width_is_strict_15_percent_on_wide_terminals() {
        // 200 × 15 / 100 = 30; 300 × 15 / 100 = 45.
        assert_eq!(rail_width_for(200), 30);
        assert_eq!(rail_width_for(300), 45);
    }

    #[test]
    fn rail_width_floors_at_min_on_narrow_borderline_widths() {
        // 120 × 15 / 100 = 18 → below MIN (20), floored to 20.
        assert_eq!(rail_width_for(120), RAIL_WIDTH_MIN);
        // 140 × 15 / 100 = 21 → above MIN, kept.
        assert_eq!(rail_width_for(140), 21);
        // Anything below MEDIUM_MIN still hides the rail entirely
        // regardless of what the percentage math would produce.
        assert_eq!(rail_width_for(100), 0);
    }

    #[test]
    fn rail_width_hidden_below_medium_threshold() {
        assert_eq!(rail_width_for(119), 0);
        assert_eq!(rail_width_for(80), 0);
    }

    #[test]
    fn resolve_active_diff_cwd_routes_git_worker_to_worktree_path() {
        // Bug #208: workers spawned with `is_git_repo_at_spawn = true`
        // run inside `.claude/worktrees/<label>/`, but `cwd_raw`
        // carries the lead's project root. The overlay must resolve
        // to the worker's worktree so the diff opens against the
        // worker's branch, not an empty lead diff.
        use forge_primitives::WorkerLiveness;
        use forge_workspace::{ProjectKey, SessionKey, WorkerEntry};

        let mut app = App::test_default();
        let workspace =
            app.workspace.clone().expect("App::test_default seeds a workspace via testing_stub");

        // Seed a loaded project so `git_scan_cwd_for_session` can
        // resolve the project_root via `project_root_for_key`. The
        // post-#232 implementation composes the worktree path from
        // the project_root rather than `cwd_raw`, so a worker entry
        // without a matching project would now fall back to cwd_raw.
        let project_root = "/tmp/project";
        workspace.seed_test_project_with_team("forge", project_root, &[]);
        let project_key = ProjectKey::new_for_test(
            forge_workspace::userdata::catalog::scan::project_key_for_directory(Some(project_root)),
        );
        let worker_key = SessionKey::from_session_id("worker-uuid");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "implementer".into(),
                charter: "test charter".into(),
                session_key: worker_key.clone(),
                status: WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead-uuid".into(),
                needs_tag: false,
                is_git_repo_at_spawn: true,
                diagnostic: None,
                kick: None,
            },
        );

        let mut session = crate::app::session::UiSession::new(worker_key.clone());
        session.cwd_raw = project_root.into();
        app.sessions.insert(worker_key.clone(), session);
        app.active_session_key = Some(worker_key);

        let resolved = resolve_active_diff_cwd(&app, project_root);
        assert_eq!(resolved, PathBuf::from("/tmp/project/.claude/worktrees/implementer"));
    }

    #[test]
    fn resolve_active_diff_cwd_returns_cwd_raw_for_lead_session() {
        // Lead sessions (and non-worker callers in general) get
        // `cwd_raw` back unchanged - the worker resolution short-
        // circuits via `worker_lookup_for_session` returning None.
        let mut app = App::test_default();
        let lead_key = forge_workspace::SessionKey::from_session_id("lead-uuid");
        let mut session = crate::app::session::UiSession::new(lead_key.clone());
        session.cwd_raw = "/tmp/project".into();
        app.sessions.insert(lead_key.clone(), session);
        app.active_session_key = Some(lead_key);

        let resolved = resolve_active_diff_cwd(&app, "/tmp/project");
        assert_eq!(resolved, PathBuf::from("/tmp/project"));
    }

    // ---- resolve_default_target: one test per DefaultTarget arm ----

    fn target_snapshot(
        repo_gate: RepoGate,
        worktree_populated: bool,
        branch_ahead_populated: bool,
        default_branch: Option<&str>,
    ) -> forge_primitives::git_diff::GitDiffSnapshot {
        use forge_primitives::git_diff::{GitBranchAhead, GitDiffStats, LayerState};
        let worktree = if worktree_populated {
            LayerState::Populated(GitDiffStats::default())
        } else {
            LayerState::Clean
        };
        let branch_ahead = if branch_ahead_populated {
            LayerState::Populated(GitBranchAhead {
                commit_count: 1,
                stats: GitDiffStats::default(),
            })
        } else {
            LayerState::Clean
        };
        forge_primitives::git_diff::GitDiffSnapshot {
            branch: forge_primitives::git::GitBranch::default(),
            default_branch: default_branch.map(str::to_owned),
            repo_gate,
            worktree,
            branch_ahead,
            pr: None,
            closes: vec![],
        }
    }

    fn app_with_target_snapshot(
        snapshot: Option<forge_primitives::git_diff::GitDiffSnapshot>,
    ) -> App {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("diff-target-test");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.git_diff_snapshot = snapshot;
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);
        app
    }

    #[test]
    fn resolve_default_target_no_snapshot_when_unscanned() {
        let app = app_with_target_snapshot(None);
        assert_eq!(resolve_default_target(&app), DefaultTarget::NoSnapshot);
    }

    #[test]
    fn resolve_default_target_not_a_repo() {
        let app =
            app_with_target_snapshot(Some(target_snapshot(RepoGate::NotARepo, false, false, None)));
        assert_eq!(resolve_default_target(&app), DefaultTarget::NotARepo);
    }

    #[test]
    fn resolve_default_target_scanner_failed() {
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::ScannerFailed,
            false,
            false,
            None,
        )));
        assert_eq!(resolve_default_target(&app), DefaultTarget::ScannerFailed);
    }

    #[test]
    fn resolve_default_target_dirty_worktree_diffs_head() {
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::InRepo,
            true,
            false,
            Some("main"),
        )));
        assert_eq!(resolve_default_target(&app), DefaultTarget::Ref("HEAD".to_owned()));
    }

    #[test]
    fn resolve_default_target_worktree_wins_over_branch_ahead() {
        // Layer 1 precedence: a dirty tree resolves to HEAD even when
        // the branch is also ahead of its default.
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::InRepo,
            true,
            true,
            Some("main"),
        )));
        assert_eq!(resolve_default_target(&app), DefaultTarget::Ref("HEAD".to_owned()));
    }

    #[test]
    fn resolve_default_target_branch_ahead_diffs_default() {
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::InRepo,
            false,
            true,
            Some("main"),
        )));
        assert_eq!(resolve_default_target(&app), DefaultTarget::Ref("main".to_owned()));
    }

    #[test]
    fn resolve_default_target_branch_ahead_without_default_is_nodefault() {
        let app =
            app_with_target_snapshot(Some(target_snapshot(RepoGate::InRepo, false, true, None)));
        assert_eq!(resolve_default_target(&app), DefaultTarget::NoDefault);
    }

    #[test]
    fn resolve_default_target_clean_tree_surfaces_default_branch() {
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::InRepo,
            false,
            false,
            Some("main"),
        )));
        assert_eq!(
            resolve_default_target(&app),
            DefaultTarget::Clean { default_branch: Some("main".to_owned()) }
        );
    }

    // ---- commit mode: scope, navigation, comment scoping ----

    fn commit_meta(sha: &str, subject: &str) -> CommitMeta {
        CommitMeta {
            sha: sha.to_owned(),
            short_sha: sha.to_owned(),
            subject: subject.to_owned(),
            body: String::new(),
        }
    }

    fn one_file(path: &str, status: FileStatus) -> FileHunks {
        FileHunks { path: path.to_owned(), status, hunks: vec![] }
    }

    /// Three-commit branch with every commit's hunks pre-cached (so
    /// navigation is synchronous). Commit 0 → a.rs, 1 → b.rs, 2 → c.rs.
    fn commit_mode_state() -> DiffOverlayState {
        let c0 = vec![one_file("a.rs", FileStatus::Added)];
        let c1 = vec![one_file("b.rs", FileStatus::Modified)];
        let c2 = vec![one_file("c.rs", FileStatus::Modified)];
        let mut state =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), c0.clone());
        state.commits = vec![
            commit_meta("aaa", "first"),
            commit_meta("bbb", "second"),
            commit_meta("ccc", "third"),
        ];
        state.branch = Some("feat".to_owned());
        state.scope = DiffScope::Commit(0);
        state.commit_cache = vec![
            Some(CachedScan { files: c0, scanner_ok: true }),
            Some(CachedScan { files: c1, scanner_ok: true }),
            Some(CachedScan { files: c2, scanner_ok: true }),
        ];
        state.recompute_comment_counts();
        state
    }

    #[test]
    fn commit_mode_starts_on_first_commit_files() {
        let state = commit_mode_state();
        assert_eq!(state.scope, DiffScope::Commit(0));
        assert_eq!(state.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.rs"]);
        assert_eq!(state.current_commit_sha(), Some("aaa".to_owned()));
    }

    #[test]
    fn step_commit_swaps_files_and_resets_scroll() {
        let mut state = commit_mode_state();
        state.doc_scroll = 42;
        assert_eq!(state.step_commit(true), Some(NavOutcome::Ready), "next commit cached");
        assert_eq!(state.scope, DiffScope::Commit(1));
        assert_eq!(state.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["b.rs"]);
        assert_eq!(state.doc_scroll, 0, "scroll resets on commit switch");
    }

    #[test]
    fn step_commit_clamps_at_both_ends() {
        let mut state = commit_mode_state();
        assert_eq!(state.step_commit(false), None, "prev at first is a no-op");
        assert_eq!(state.scope, DiffScope::Commit(0));
        state.select_scope(DiffScope::Commit(2));
        assert_eq!(state.step_commit(true), None, "next at last is a no-op");
        assert_eq!(state.scope, DiffScope::Commit(2));
    }

    #[test]
    fn step_commit_is_noop_in_whole_diff_only_mode() {
        let mut state = sample_state();
        assert_eq!(state.step_commit(true), None);
        assert_eq!(state.scope, DiffScope::WholeDiff);
    }

    #[test]
    fn step_commit_from_all_changes_reenters_first_commit() {
        let mut state = commit_mode_state();
        // Sit on "All changes" (cache it so the switch is synchronous).
        state.whole_diff_cache = Some(CachedScan {
            files: vec![one_file("x.rs", FileStatus::Modified)],
            scanner_ok: true,
        });
        state.select_scope(DiffScope::WholeDiff);
        assert_eq!(state.scope, DiffScope::WholeDiff);
        // Backward from the whole-diff view re-enters the stepper at the
        // first commit (the one WholeDiff → Commit path).
        assert_eq!(state.step_commit(false), Some(NavOutcome::Ready));
        assert_eq!(state.scope, DiffScope::Commit(0));
        assert_eq!(state.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.rs"]);
    }

    #[test]
    fn comments_accumulate_across_commits_but_count_current_scope() {
        let mut state = commit_mode_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "on first".into(),
            commit: Some("aaa".into()),
        });
        state.recompute_comment_counts();
        assert_eq!(state.comment_counts, vec![1], "commit 0 shows its comment");
        state.step_commit(true);
        assert_eq!(state.scope, DiffScope::Commit(1));
        assert_eq!(state.comments.len(), 1, "comment retained across navigation");
        assert_eq!(state.comment_counts, vec![0], "commit 1 counts none of its own");
        state.step_commit(false);
        assert_eq!(state.comment_counts, vec![1], "back on commit 0 the comment counts again");
    }

    #[test]
    fn scoped_comments_filters_by_current_commit() {
        let mut state = commit_mode_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "a".into(),
            commit: Some("aaa".into()),
        });
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "b.rs".into(),
            line: 1,
            hunk_context: vec![],
            comment_text: "b".into(),
            commit: Some("bbb".into()),
        });
        let scoped: Vec<&str> =
            state.scoped_comments().iter().map(|c| c.comment_text.as_str()).collect();
        assert_eq!(scoped, vec!["a"], "only the current commit's comment is in scope");
    }

    #[test]
    fn select_uncached_commit_needs_scan_then_installs() {
        let mut state = commit_mode_state();
        state.commit_cache[2] = None;
        assert_eq!(
            state.select_scope(DiffScope::Commit(2)),
            NavOutcome::NeedsScan(DiffScope::Commit(2)),
        );
        assert!(state.commit_loading, "loading while the scan is in flight");
        assert!(state.files.is_empty(), "no files shown until the scan lands");
        state.install_scan(
            DiffScope::Commit(2),
            vec![one_file("c.rs", FileStatus::Modified)],
            true,
            Some("body for the third commit".to_owned()),
        );
        assert!(!state.commit_loading, "loading cleared once installed");
        assert_eq!(state.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["c.rs"]);
        assert_eq!(state.commits[2].body, "body for the third commit", "body filled on install");
    }

    #[test]
    fn install_scan_off_scope_caches_without_swapping() {
        let mut state = commit_mode_state();
        state.commit_cache[2] = None;
        // Still on commit 0 when commit 2's scan lands.
        state.install_scan(
            DiffScope::Commit(2),
            vec![one_file("c.rs", FileStatus::Modified)],
            true,
            Some("off-scope body".to_owned()),
        );
        assert_eq!(state.commits[2].body, "off-scope body", "body filled even off-scope");
        assert_eq!(state.scope, DiffScope::Commit(0), "off-scope result doesn't swap the view");
        assert_eq!(state.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.rs"]);
        assert_eq!(
            state.select_scope(DiffScope::Commit(2)),
            NavOutcome::Ready,
            "but commit 2 is now cached, so a later visit is instant",
        );
    }

    #[test]
    fn jump_dropdown_rows_map_to_scopes() {
        let state = commit_mode_state();
        assert_eq!(state.jump_row_count(), 4, "All changes + 3 commits");
        assert_eq!(DiffOverlayState::scope_for_jump_row(0), DiffScope::WholeDiff);
        assert_eq!(DiffOverlayState::scope_for_jump_row(1), DiffScope::Commit(0));
        assert_eq!(DiffOverlayState::scope_for_jump_row(3), DiffScope::Commit(2));
    }

    #[test]
    fn open_jump_seeds_highlight_and_move_clamps() {
        let mut state = commit_mode_state();
        state.select_scope(DiffScope::Commit(1));
        state.open_jump();
        assert!(state.jump_open);
        assert_eq!(state.jump_selected, 2, "commit 1 → row 2 (row 0 is All changes)");
        state.jump_move(false);
        assert_eq!(state.jump_selected, 1);
        for _ in 0..5 {
            state.jump_move(true);
        }
        assert_eq!(state.jump_selected, 3, "clamps at the last row");
    }

    #[test]
    fn jump_confirm_all_changes_scans_then_installs() {
        let mut state = commit_mode_state();
        state.open_jump();
        state.jump_selected = 0; // All changes, not cached yet
        assert_eq!(state.jump_confirm(), NavOutcome::NeedsScan(DiffScope::WholeDiff));
        assert!(state.commit_loading);
        assert!(!state.jump_open, "confirm closes the dropdown");
        state.install_scan(
            DiffScope::WholeDiff,
            vec![one_file("a.rs", FileStatus::Added), one_file("b.rs", FileStatus::Modified)],
            true,
            None,
        );
        assert!(!state.commit_loading);
        assert_eq!(state.files.len(), 2, "the whole-branch diff is now shown");
        assert_eq!(state.current_commit_sha(), None, "whole-diff scope has no sha");
    }

    #[test]
    fn save_stamps_current_commit_sha_in_commit_mode() {
        use forge_workspace::env::git_diff::hunks::Hunk;
        let mut app = App::test_default();
        let file = FileHunks {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    text: "x".into(),
                    old_line: None,
                    new_line: Some(1),
                }],
            }],
        };
        let mut state =
            DiffOverlayState::new(PathBuf::from("/tmp"), "main".to_owned(), vec![file.clone()]);
        state.commits = vec![commit_meta("aaa", "s")];
        state.scope = DiffScope::Commit(0);
        state.commit_cache = vec![Some(CachedScan { files: vec![file], scanner_ok: true })];
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut editor = TextArea::default();
        editor.insert_str("note");
        state.active_input = Some(ActiveCommentInput { key, editor, prior_comment: None });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let o = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(o.comments.len(), 1);
        assert_eq!(o.comments[0].commit, Some("aaa".to_owned()), "commit sha stamped");
    }

    #[test]
    fn save_stamps_no_commit_in_whole_diff_mode() {
        let mut app = App::test_default();
        let file = one_file("a.rs", FileStatus::Modified);
        let mut file = file;
        file.hunks = vec![forge_workspace::env::git_diff::hunks::Hunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Added,
                text: "x".into(),
                old_line: None,
                new_line: Some(1),
            }],
        }];
        let mut state = DiffOverlayState::new(PathBuf::from("/tmp"), "HEAD".to_owned(), vec![file]);
        // Whole-diff-only mode: no commits, scope stays WholeDiff.
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut editor = TextArea::default();
        editor.insert_str("note");
        state.active_input = Some(ActiveCommentInput { key, editor, prior_comment: None });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let o = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(o.comments.len(), 1);
        assert_eq!(o.comments[0].commit, None, "whole-diff comments carry no commit");
    }

    // ---- key + mouse: commit navigation and the jump dropdown ----
    //
    // These drive cached navigation only (Ready outcomes) - the
    // NeedsScan → `spawn_local` glue needs a LocalSet runtime, and the
    // NeedsScan branch itself is covered by the state tests above.

    fn app_with_commit_overlay() -> App {
        let mut app = App::test_default();
        app.diff_overlay = Some(commit_mode_state());
        set_active_view(&mut app, ActiveView::Diff);
        app
    }

    fn overlay(app: &App) -> &DiffOverlayState {
        app.diff_overlay.as_ref().expect("overlay")
    }

    #[test]
    fn bracket_keys_step_commits() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char(']')));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(1));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('[')));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(0));
    }

    #[test]
    fn arrow_keys_step_commits() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Right));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(1));
        handle_key(&mut app, KeyEvent::from(KeyCode::Left));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(0));
    }

    #[test]
    fn j_opens_jump_dropdown_seeded_on_current() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        assert!(overlay(&app).jump_open);
        assert_eq!(overlay(&app).jump_selected, 1, "scope Commit(0) → dropdown row 1");
    }

    #[test]
    fn jump_dropdown_move_then_enter_navigates() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Down));
        assert_eq!(overlay(&app).jump_selected, 2);
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));
        assert!(!overlay(&app).jump_open, "confirm closes the menu");
        assert_eq!(overlay(&app).scope, DiffScope::Commit(1), "navigates to the picked commit");
    }

    #[test]
    fn jump_dropdown_esc_closes_menu_not_overlay() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(!overlay(&app).jump_open, "menu closed");
        assert!(app.diff_overlay.is_some(), "overlay stays open");
        assert_eq!(app.active_view, ActiveView::Diff, "still in the diff view");
    }

    #[test]
    fn bracket_and_j_are_noops_in_whole_diff_only_mode() {
        let mut app = App::test_default();
        app.diff_overlay = Some(sample_state());
        set_active_view(&mut app, ActiveView::Diff);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char(']')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(overlay(&app).scope, DiffScope::WholeDiff);
        assert!(!overlay(&app).jump_open, "no dropdown without commits");
        assert!(app.diff_overlay.is_some());
    }

    #[test]
    fn click_on_jump_hint_toggles_dropdown() {
        let mut state = commit_mode_state();
        state.jump_hint_span = Some((1, 40, 46));
        let effect = handle_left_click(&mut state, 42, 1, 160);
        assert!(effect.redraw);
        assert!(state.jump_open, "click on the ⌄ control opens the dropdown");
        let effect = handle_left_click(&mut state, 42, 1, 160);
        assert!(effect.redraw);
        assert!(!state.jump_open, "a second click closes it");
    }

    #[test]
    fn click_away_closes_open_dropdown() {
        let mut state = commit_mode_state();
        state.open_jump();
        state.jump_hint_span = Some((1, 40, 46));
        let effect = handle_left_click(&mut state, 5, 10, 160);
        assert!(effect.redraw);
        assert!(!state.jump_open, "a click off the control closes the menu");
    }

    // ---- enter mode: commit mode when the target has commits ahead ----

    #[test]
    fn new_initial_opens_commit_mode_when_commits_present() {
        let event = DiffOverlayEvent {
            cwd: PathBuf::from("/tmp"),
            target: "main".into(),
            files: vec![one_file("a.rs", FileStatus::Added)],
            scanner_ok: true,
            untracked_suppressed: 0,
            seq: 1,
            kind: DiffScanKind::Initial {
                commits: vec![commit_meta("aaa", "first"), commit_meta("bbb", "second")],
                branch: Some("feat".into()),
            },
            commit_body: Some("the first commit's body".to_owned()),
        };
        let state = DiffOverlayState::new_initial(event);
        assert_eq!(state.scope, DiffScope::Commit(0), "commits ahead → open on the first commit");
        assert_eq!(state.commits.len(), 2);
        assert_eq!(state.branch.as_deref(), Some("feat"));
        assert_eq!(state.commits[0].body, "the first commit's body", "first commit's body filled");
        assert!(state.commit_cache[0].is_some(), "first commit's diff cached upfront");
        assert!(state.commit_cache[1].is_none(), "later commits are scanned lazily");
        assert!(state.whole_diff_cache.is_none());
        assert_eq!(state.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.rs"]);
    }

    #[test]
    fn new_initial_opens_whole_diff_when_no_commits() {
        let event = DiffOverlayEvent {
            cwd: PathBuf::from("/tmp"),
            target: "HEAD".into(),
            files: vec![one_file("a.rs", FileStatus::Modified)],
            scanner_ok: true,
            untracked_suppressed: 2,
            seq: 1,
            kind: DiffScanKind::Initial { commits: Vec::new(), branch: None },
            commit_body: None,
        };
        let state = DiffOverlayState::new_initial(event);
        assert_eq!(state.scope, DiffScope::WholeDiff, "no commits ahead → whole-diff mode");
        assert!(state.commits.is_empty());
        assert!(state.commit_cache.is_empty());
        assert!(state.whole_diff_cache.is_some());
        assert_eq!(state.untracked_suppressed, 2, "whole-diff keeps the untracked cap count");
    }
}
