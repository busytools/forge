//! Full-screen diff overlay state + keyboard handling.
//!
//! The overlay is the floor of the `/diff` flow: a snapshot of
//! file-level hunks fetched via
//! [`forge_workspace::env::git_diff::hunks::scan`] rendered as a
//! single continuous scroll of every changed file with a FILES jump
//! rail. See [`crate::ui::diff_overlay`] for the renderer; this module
//! owns the transient state and the key / mouse dispatch.
//!
//! Key handling:
//! - With a comment editor open: Enter saves the text into
//!   [`DiffOverlayState::comments`] and closes the editor; Esc
//!   cancels the editor (restoring a saved comment if the editor
//!   was opened via re-clicking a chip).
//! - With no editor open: Esc seals this session's authored comments
//!   into a numbered review and nudges the agent (one line) to address
//!   it via the review MCP, then closes the overlay.
//!
//! Mouse handling: see [`handle_mouse`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use super::input::{InputState, TypedChar};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use forge_primitives::git_diff::RepoGate;
use forge_primitives::review::{
    ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
};
use forge_workspace::env::git_diff::hunks::ScanOutcome;
use forge_workspace::env::git_diff::hunks::{
    CommitMeta, DiffLine, DiffLineKind, FileHunks, FileStatus, Hunk,
};
use forge_workspace::env::git_diff::resolver::{
    self, AnchorResolution, CONTEXT_RADIUS, OutdatedReason,
};
use std::time::Instant;

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
/// `must_use` because dropping one skips both halves of a scope change:
/// the scan an uncached scope needs, and the card rebuild a cached one
/// needs. Neither failure is visible at the point it happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
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

/// Reorder a scanner-ordered file list into the FILES rail's folded-tree
/// traversal order - the one canonical display sequence the body, the
/// offset table, and the rail all walk, so the current-file arrow steps
/// monotonically down the rail as the body scrolls. Sorting the paths by
/// [`compare_tree_paths`] yields the rail's pre-order leaf sequence
/// because the rail re-sorts by the same per-level rule, so its tree is
/// a pure function of the path set (independent of input order).
fn reorder_files_to_tree(files: &mut [FileHunks]) {
    files.sort_by(|a, b| compare_tree_paths(&a.path, &b.path));
}

/// Order two diff paths by the rail's per-level rule, as a genuine total
/// order: at each segment, a directory (a segment that isn't this path's
/// leaf) sorts before a file, then alphabetically. Comparing the
/// `(is_leaf, name)` tuple per segment keeps it transitive even when a
/// name is both a file and a directory (a file->dir refactor git emits as
/// `D z` + `A z/a`), which a length-only fallback would make cyclic.
fn compare_tree_paths(a: &str, b: &str) -> std::cmp::Ordering {
    let a: Vec<&str> = a.split('/').filter(|c| !c.is_empty()).collect();
    let b: Vec<&str> = b.split('/').filter(|c| !c.is_empty()).collect();
    for i in 0..a.len().min(b.len()) {
        let ord = (i + 1 == a.len(), a[i]).cmp(&(i + 1 == b.len(), b[i]));
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

/// Narrow a file's full-context wide hunks down to `context` lines
/// around each change, reproducing git's `-U<context>` hunking in memory:
/// a line is kept when it is a change or within `context` of one, and
/// maximal runs of kept lines become hunks (so two changes whose gap the
/// context spans fold into one). The overlay captures the wide hunks once
/// at open and re-narrows from them on an expander click - no re-fetch.
fn narrow_hunks(wide: &[Hunk], context: usize) -> Vec<Hunk> {
    let mut out = Vec::new();
    for hunk in wide {
        let n = hunk.lines.len();
        let mut keep = vec![false; n];
        let mut has_change = false;
        for (i, line) in hunk.lines.iter().enumerate() {
            if line.kind != DiffLineKind::Context {
                has_change = true;
                let lo = i.saturating_sub(context);
                let hi = (i + context).min(n.saturating_sub(1));
                for slot in keep.iter_mut().take(hi + 1).skip(lo) {
                    *slot = true;
                }
            }
        }
        // A real diff hunk always carries a change; a changeless one (only
        // test fixtures build these) has nothing to narrow around, so keep
        // it whole rather than dropping it to an empty display.
        if !has_change {
            keep.fill(true);
        }
        let mut i = 0;
        while i < n {
            if !keep[i] {
                i += 1;
                continue;
            }
            let start = i;
            while i < n && keep[i] {
                i += 1;
            }
            out.push(hunk_from_lines(&hunk.lines[start..i]));
        }
    }
    out
}

/// Build a `Hunk` header from a contiguous run of diff lines: the start
/// line numbers are the first old-/new-side line present, the counts the
/// number of old-/new-side lines. Used by [`narrow_hunks`] to re-header a
/// slice of the wide snapshot.
fn hunk_from_lines(lines: &[DiffLine]) -> Hunk {
    let old_start = lines.iter().find_map(|l| l.old_line).unwrap_or(0);
    let new_start = lines.iter().find_map(|l| l.new_line).unwrap_or(0);
    let old_count =
        u32::try_from(lines.iter().filter(|l| l.old_line.is_some()).count()).unwrap_or(u32::MAX);
    let new_count =
        u32::try_from(lines.iter().filter(|l| l.new_line.is_some()).count()).unwrap_or(u32::MAX);
    Hunk { old_start, old_count, new_start, new_count, lines: lines.to_vec() }
}

/// Unwrapped, syntax-highlighted spans for one file's diff lines,
/// indexed `[hunk_idx][line_idx]` (the innermost `Vec` is one line's
/// spans). Cached on [`DiffOverlayState::highlighted`] and reused
/// across frames - a line's colour is layout-independent, so it
/// survives scroll, view_mode flip, and resize untouched, and a
/// plain scroll never re-runs syntect.
pub type FileHighlight = Vec<Vec<Vec<ratatui::text::Span<'static>>>>;

/// Rendered rows the end-of-file boundary adds after every file but the
/// last: the `└─ end <path> ──` cap row plus a blank spacer before the
/// next file's banded header. Measured heights (via the renderer's
/// `push_file_body`) already include it, so the off-screen estimate must
/// add it too or the offset table drifts by two rows per file boundary.
pub(crate) const END_CAP_ROWS: u32 = 2;

/// Unified-diff context radius the initial scan runs at (`git diff`'s
/// default `-U3`). The per-file expansion level starts here and grows by
/// [`CONTEXT_STEP`] per expander click.
pub(crate) const DEFAULT_CONTEXT: u32 = 3;

/// Lines of context an expander click adds per side (so a gap shrinks by
/// up to `2 * CONTEXT_STEP` per click, GitHub's ~20-lines-per-click feel).
pub(crate) const CONTEXT_STEP: u32 = 20;

/// Cheap height estimate for an off-screen file (no wrap, no pairing):
/// 1 sticky header row + per hunk (1 `@@` header + raw line count) + the
/// context-expander rows the renderer emits (one above the first hunk
/// when it starts past line 1, one per non-zero inter-hunk gap), or 2 for
/// a collapsed deleted file. The offset table uses this for not-yet-
/// measured files; the renderer's `file_height` replaces it (storing into
/// `measured_heights`) when the file enters the window.
pub(crate) fn estimated_height(file: &FileHunks, collapsed: bool) -> u32 {
    if collapsed {
        return 2;
    }
    // A sticky header, plus (oversize files only) the one-line
    // "too large" note the renderer shows instead of expanders.
    let mut rows: u32 = if file.oversize { 2 } else { 1 };
    let mut prev_end: Option<u32> = None;
    for hunk in &file.hunks {
        // Expander row where lines are hidden before this hunk - above the
        // first hunk (leading) or across a gap - but oversize files render
        // no expanders (their snapshot can't reveal more).
        if !file.oversize {
            let hidden = match prev_end {
                None => hunk.new_start.saturating_sub(1),
                Some(end) => hunk.new_start.saturating_sub(end),
            };
            if hidden > 0 {
                rows = rows.saturating_add(1);
            }
        }
        rows = rows.saturating_add(1); // @@ header
        rows = rows.saturating_add(u32::try_from(hunk.lines.len()).unwrap_or(u32::MAX));
        prev_end = Some(hunk.new_start.saturating_add(hunk.new_count));
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
    /// The dim `└─ end <path> ──` boundary row (and its blank spacer)
    /// that closes a file before the next file's banded header. Names
    /// the ending file; non-interactive.
    FileEndCap { file_idx: usize },
    /// A dim `┈ ↕ N lines ┈` context-expander row at a hunk's leading
    /// edge or an inter-hunk gap. Click re-slices the file from its pinned
    /// wide snapshot at a wider context level in memory (context is
    /// per-file, so every expander in a file bumps the same level).
    ContextExpander { file_idx: usize },
    /// `@@ -A,B +C,D @@` hunk header - non-interactive in v1.
    HunkHeader { file_idx: usize, hunk_idx: usize },
    /// A diff row in the split body. Carries both column keys - the
    /// click handler picks one by comparing the click column against
    /// [`split_layout`]'s `divider_col`. At least one side is `Some`
    /// (the pairing algorithm never emits both-None).
    HunkRow { left: Option<LineKey>, right: Option<LineKey> },
    /// A comment card row that is not itself a your-turn edit target:
    /// the header, blank spacers, an agent's (read-only) turn, the
    /// outdated note, and the bottom border. Non-interactive - your
    /// turns carry [`BodyRowKey::CommentTurn`] and the reply line
    /// [`BodyRowKey::CommentReply`].
    CommentChip(LineKey),
    /// One of the reviewer's own turns in a comment card. Click →
    /// reopen the editor seeded with that turn's text to rewrite it in
    /// place. Only `User`-authored turns emit this key.
    CommentTurn { at: CommentRef, turn_idx: usize },
    /// A comment card's reply line. Click → open an empty editor that
    /// appends a new user turn on save (no state change, no nudge).
    CommentReply { at: CommentRef },
    /// A resolved comment's collapsed one-line marker, or the header of
    /// one the reviewer expanded. Click toggles it.
    CommentCollapsed { at: CommentRef },
    /// A comment card's button row (`✓ Resolve` / `↺ Reopen`). Each
    /// action's pane-relative `[start, end)` column span is `Some` only
    /// when that action applies to the thread's current state; a click
    /// routes to whichever span it lands in, and a click on the padding or
    /// an inapplicable (dim) button no-ops.
    CommentButton { at: CommentRef, resolve: Option<(u16, u16)>, reopen: Option<(u16, u16)> },
    /// Inline editor row for the currently-open comment editor.
    /// Multiple consecutive rows when the comment spans more than
    /// one visual line.
    InputRow(LineKey),
    /// A row of the commit-message block shown above the diff in commit
    /// mode (the leading rule, subject, or a body line). Non-interactive.
    CommitMessage,
}

/// What the last re-anchor did to a comment, when it did not simply
/// leave it in place. Rendered on the card, so neither a relocation nor a
/// refusal to relocate is silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorNote {
    /// Re-anchored here from line `from`.
    Moved { from: u32 },
    /// Left where it was, and why.
    Outdated(OutdatedReason),
}

/// Which comment card a row belongs to. The whole diff is a union over
/// the branch, so several threads can anchor to one line and stack
/// there; `slot` is the card's position in that stack, counted over the
/// comments in render scope. Without it a click routes to whichever
/// thread the lookup reaches first, which is how a reviewer resolves a
/// comment they were not looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommentRef {
    pub line: LineKey,
    pub slot: usize,
}

/// The lifecycle transition a comment card's button fires. `Resolve`
/// moves an Open / Addressed / Outdated thread to Resolved; `Reopen`
/// moves an Addressed / Resolved thread back to Open and re-nudges the
/// worker to take another look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadAction {
    Resolve,
    Reopen,
}

/// A saved per-line comment. `path` / `line` are snapshotted at save
/// time so the anchor stays stable even if the user scrolls or switches
/// files before pressing Esc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkComment {
    pub key: LineKey,
    pub path: String,
    /// Line number from the relevant side of the diff (new-file
    /// line for context / added, old-file line for removed).
    pub line: u32,
    pub comment_text: String,
    /// Commit the comment was made against (the sha), or `None` in
    /// whole-diff scope. Together with `key`/`path`/`line` this scopes
    /// the comment: navigating between commits keeps every comment but
    /// only those matching the current scope render and count.
    pub commit: Option<String>,
    /// Durable review-thread record: the anchor, comment chain, and
    /// lifecycle state persisted to redb. The flat fields above carry the
    /// current-scan view (re-resolved each open); `thread.anchor` carries
    /// the durable last-known location and `thread.status` drives the
    /// box's state tint. Every comment carries one; `persisted` (below)
    /// separately tracks whether its redb write was confirmed.
    pub thread: forge_primitives::ReviewThread,
    /// Whether the user authored or edited this comment in THIS overlay
    /// session (vs a thread hydrated from redb for display). Only
    /// session-authored comments are sealed into a review on Esc, so a
    /// read-only reopen of a branch's history never re-nudges the agent.
    pub authored_this_session: bool,
    /// What the last re-anchor did to this comment, `None` when it was
    /// simply still in place.
    pub anchor_note: Option<AnchorNote>,
    /// Whether a redb write for this comment's thread has been confirmed.
    /// `false` for a comment whose write was skipped (no branch / no db)
    /// or failed - those stay in the at-risk bucket the force-clear path
    /// warns about.
    pub persisted: bool,
}

/// Currently-active comment input. Mounts inline below the clicked
/// line. The editor is the same [`InputState`] the chat draft uses, so
/// clipboard, bracketed paste, dictation-burst coalescing and paste
/// blocks all behave identically here; only the submit key differs.
///
/// `prior_comment` carries the saved comment when the editor was
/// opened by re-clicking an existing 💬 chip. On Esc-cancel, the
/// prior comment is restored to [`DiffOverlayState::comments`] so
/// a misclick on the chip + reflex Esc doesn't destroy the user's
/// review notes. `None` for fresh line-clicks where there's nothing
/// to restore.
///
/// `edit_turn` names which existing turn the editor rewrites:
/// `Some(idx)` edits that turn's text in place; `None` either starts
/// a fresh comment (when `prior_comment` is `None`) or appends a new
/// user turn as a reply (when `prior_comment` is `Some`).
#[derive(Debug, Clone)]
pub struct ActiveCommentInput {
    pub key: LineKey,
    pub editor: InputState,
    pub prior_comment: Option<HunkComment>,
    pub edit_turn: Option<usize>,
}

/// The Finish-review modal, opened on overlay close when this session
/// authored a comment. Seals the session's comments into a numbered
/// review on submit; `editor` holds the optional overview cover note.
#[derive(Debug, Clone)]
pub struct FinishReviewState {
    pub editor: InputState,
}

/// What a completed scan event carries beyond its files: either the
/// initial open (which builds a fresh overlay, with the commit stepper
/// list when the target has commits ahead) or a lazily-scanned scope
/// (a commit or "All changes") installed into an already-open overlay.
#[derive(Debug)]
pub enum DiffScanKind {
    /// The overlay's initial open. `scope` is the resolved landing scope:
    /// `Commit(i)` when opening on a commit (`files` is that commit's
    /// diff, cached at slot `i`), `WholeDiff` when opening "All changes"
    /// (`files` is the whole-branch diff). `commits` is the stepper list
    /// (empty in whole-diff-only mode); `branch` names the branch under
    /// review for the stepper header.
    Initial { commits: Vec<CommitMeta>, branch: Option<String>, scope: DiffScope },
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

/// Which scope the initial `/diff` open should land on, resolved from the
/// branch's persisted review threads so a reopen shows the user's
/// comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitialScope {
    /// A whole-diff thread exists; open on "All changes".
    WholeDiff,
    /// Only commit-scoped threads exist; open on the commit carrying the
    /// most-recently-updated one (its sha).
    Commit(String),
    /// No threads; open on the first commit when the branch has commits
    /// ahead, else whole-diff.
    Default,
}

/// Pick the initial scope from the branch's persisted threads: whole-diff
/// when any whole-diff thread exists (the pre-scope behavior), else the
/// commit carrying the most-recently-updated comment, else the default.
fn initial_scope_from_threads(threads: &[forge_primitives::ReviewThread]) -> InitialScope {
    if threads.iter().any(|t| t.commit.is_none()) {
        return InitialScope::WholeDiff;
    }
    threads
        .iter()
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
        .and_then(|t| t.commit.clone())
        .map_or(InitialScope::Default, InitialScope::Commit)
}

/// Resolve an [`InitialScope`] against a freshly-scanned commit list into
/// the commit to open (index + sha), or `None` for whole-diff. A chosen
/// sha no longer in the list falls back to the first commit.
fn resolve_initial_commit(
    initial: &InitialScope,
    commits: &[CommitMeta],
) -> Option<(usize, String)> {
    match initial {
        InitialScope::WholeDiff => None,
        InitialScope::Default => commits.first().map(|c| (0, c.sha.clone())),
        InitialScope::Commit(sha) => commits
            .iter()
            .position(|c| &c.sha == sha)
            .map(|idx| (idx, sha.clone()))
            .or_else(|| commits.first().map(|c| (0, c.sha.clone()))),
    }
}

/// The branch the overlay reviews under, read live from the checkout
/// being diffed. Same `git rev-parse` against the same
/// `git_scan_cwd_for_session`-resolved path the review MCP's
/// `resolve_scope` queries, so a review filed here is keyed exactly
/// where its reader looks. `None` on a detached HEAD or a failed read.
async fn review_branch(cwd: &Path) -> Option<String> {
    match forge_workspace::env::git_diff::current_branch(cwd).await {
        Ok(branch) => branch,
        Err(gate) => {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_branch_unresolved",
                message = "git reported no branch for the diff checkout; a review filed here cannot be keyed or read back",
                outcome = "degraded",
                cwd = %cwd.display(),
                gate = ?gate,
            );
            None
        }
    }
}

/// Spawn the initial `/diff` scan and post a [`DiffOverlayEvent`] when
/// it completes. Best-effort send - receiver going away (app shutdown)
/// just drops the result. Resolves the review branch off `cwd`, then
/// scans the commit list and picks the landing scope from that branch's
/// persisted threads: a commit scope scans that commit's diff upfront
/// (the rest lazily on navigation) and opens on it; otherwise it scans
/// the whole diff and opens whole-diff mode.
pub fn spawn_fetch(
    cwd: PathBuf,
    target: String,
    project: Option<String>,
    workspace: Option<Arc<forge_workspace::Workspace>>,
    seq: u64,
    tx: std_mpsc::Sender<DiffOverlayEvent>,
) {
    tokio::task::spawn_local(async move {
        let branch = review_branch(&cwd).await;
        // Open on the scope that holds the branch's persisted review
        // threads, so a reopen lands where the user's comments are
        // instead of the first commit. A load failure just falls back to
        // the default scope; the post-open `hydrate_threads` hits the
        // same error and surfaces the notice against the open overlay.
        let initial = match (&project, &branch, &workspace) {
            (Some(project), Some(branch), Some(workspace)) => workspace
                .load_review_threads(project, branch)
                .map_or(InitialScope::Default, |threads| initial_scope_from_threads(&threads)),
            _ => InitialScope::Default,
        };
        let commits = forge_workspace::env::git_diff::hunks::scan_commits(&cwd, &target).await;
        // Resolve the initial scope against the freshly-scanned commits:
        // `Some((idx, sha))` opens on that commit (its diff scanned
        // upfront), `None` opens the whole-branch diff.
        let open_commit = resolve_initial_commit(&initial, &commits);
        let (files, scanner_ok, untracked_suppressed, commit_body, scope) =
            if let Some((idx, sha)) = open_commit {
                let o = forge_workspace::env::git_diff::hunks::scan_commit(&cwd, &sha).await;
                let body =
                    forge_workspace::env::git_diff::hunks::scan_commit_body(&cwd, &sha).await;
                (o.files, o.scanner_ok, 0, Some(body), DiffScope::Commit(idx))
            } else {
                let ScanOutcome { files, scanner_ok, untracked_suppressed } =
                    forge_workspace::env::git_diff::hunks::scan(&cwd, &target).await;
                (files, scanner_ok, untracked_suppressed, None, DiffScope::WholeDiff)
            };
        let _ = tx.send(DiffOverlayEvent {
            cwd,
            target,
            files,
            scanner_ok,
            untracked_suppressed,
            seq,
            kind: DiffScanKind::Initial { commits, branch, scope },
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
    let project = app.active_session().and_then(|s| s.project.clone());
    let workspace = app.workspace.clone();
    // Bump the seq before spawning so the new scan's events
    // outrank anything still in flight from an earlier /diff call.
    // Old events arriving on the channel after this bump will be
    // dropped by drain_events as superseded.
    app.diff_scan_seq = app.diff_scan_seq.wrapping_add(1);
    let seq = app.diff_scan_seq;
    spawn_fetch(cwd, target, project, workspace, seq, app.diff_overlay_event_tx.clone());
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
            // Load + re-anchor persisted threads for whatever scope the
            // initial open landed on.
            hydrate_threads(app);
        } else if let DiffScanKind::Scope(scope) = event.kind {
            // A lazy per-scope scan lands into the already-open overlay
            // (view == Diff). If it closed while the scan ran, drop it.
            if let Some(overlay) = app.diff_overlay.as_mut() {
                // `install_scan` swaps the files into view only when the
                // landed scope is the one currently shown; hydrate that
                // scope's persisted threads against those files. An
                // out-of-order scan that only cached off-scope is skipped -
                // hydrating it would re-anchor against the wrong files.
                let landed_current = overlay.scope == scope;
                overlay.install_scan(scope, event.files, event.scanner_ok, event.commit_body);
                app.needs_redraw = true;
                if landed_current {
                    hydrate_threads(app);
                }
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
    /// SHA). Surfaced in the scan-failed notice and the stepper header.
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
    /// Thread ids of resolved comments the reviewer expanded back open.
    /// Resolved comments collapse by default: resolving is how something
    /// leaves the working set, so keeping it full-height puts it back in.
    pub resolved_expanded: std::collections::HashSet<String>,
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
    /// the renderer to wrap the editor and by the click handler
    /// for column bound checks.
    pub pane_width: u16,
    /// Left screen column of the diff content (rail or body) inside the
    /// page border; the rail/body column hit-test is relative to it.
    pub content_origin_col: u16,
    /// Top screen row of the FILES rail (below the page border and any
    /// commit stepper); the rail row hit-test is relative to it.
    pub rail_origin_row: u16,
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
    /// File index at the top of the viewport, message-adjusted (the
    /// same file the sticky header pins). Stashed each render so the
    /// rail highlights the file the body actually shows - in commit
    /// mode a message block leads the document, so the rail must offset
    /// by it rather than reading the raw `doc_scroll`.
    pub current_file_idx: usize,
    /// Rows the commit-message block occupies at the top of the document
    /// (0 in whole-diff mode). Stashed each render so a rail-click target
    /// in file-sub-document space is lifted back into full-document space.
    pub message_rows: u32,
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
    /// Per-file unified-diff context radius currently shown. Starts at
    /// the default context; an expander click bumps it by `CONTEXT_STEP`
    /// and re-narrows that file from [`Self::wide_hunks`] in memory.
    /// Length tracks `files`; reset on scope change.
    pub context_levels: Vec<u32>,
    /// Per-file full-context hunks captured once at scope open - the
    /// pinned snapshot the display is narrowed from and expanders reveal
    /// from, so an expander click never re-runs `git diff` (no
    /// fetch-failure, scope-switch race, or mid-review tree drift).
    /// Length tracks `files`.
    pub wide_hunks: Vec<Vec<Hunk>>,
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
    /// The commit the `a` toggle jumped away from, so a second `a`
    /// returns to it. `None` until the first commit→all-changes toggle
    /// (a fresh open, or after reaching all-changes via the jump menu);
    /// the return then falls back to the first commit.
    pub last_commit: Option<usize>,
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
    /// Set when loading this branch's persisted review threads failed
    /// (a decode / IO error on an existing redb row), cleared on a
    /// successful load. Drives a visible "review comments failed to load"
    /// notice so a genuine failure doesn't read as an empty review pane.
    pub review_load_error: Option<String>,
    /// The Finish-review modal when open (this session authored a comment
    /// and the user is closing the overlay); `None` otherwise. Captures
    /// keys/clicks and holds the optional overview editor.
    pub finish_review: Option<FinishReviewState>,
    /// Screen span `(row, col_start, col_end)` of the modal's
    /// `[ Submit review ]` button, stashed each render so a click can
    /// resolve onto it. `None` until the modal first renders.
    pub finish_submit_span: Option<(u16, u16, u16)>,
    /// Submitted reviews for `(project, branch)`, loaded alongside the
    /// threads on hydrate. Maps a comment's `review_id` to its `R{number}`
    /// chip tag and backs the `l` reviews list.
    pub reviews: Vec<forge_primitives::ReviewSet>,
    /// Whether the `l` REVIEWS list overlay is open.
    pub reviews_open: bool,
    /// Highlighted row in the open reviews list (index into `review_rows`).
    pub reviews_selected: usize,
    /// Snapshot rows for the reviews list, computed on open from every
    /// thread's current state (newest review first). Empty while closed.
    pub review_rows: Vec<ReviewListRow>,
    /// Footer tally for the reviews list, counting each filed comment once
    /// across the reviews it appears in. Zeroed while closed.
    pub review_totals: ReviewListTotals,
}

/// One rendered row of the `l` REVIEWS list: a review's number, relative
/// age, member-comment tally by state, optional overview, and the scope +
/// path of its first member comment (so Enter can navigate to it).
#[derive(Debug, Clone)]
pub struct ReviewListRow {
    pub number: u32,
    pub age: String,
    pub total: usize,
    pub open: usize,
    pub addressed: usize,
    pub resolved: usize,
    pub outdated: usize,
    pub summary: Option<String>,
    pub first_commit: Option<String>,
    pub first_path: Option<String>,
}

/// The reviews-list footer tally. Counts each filed comment once even when
/// its turns span several reviews, so the footer reports how many comments
/// exist rather than the sum of the per-review rows.
#[derive(Debug, Clone, Default)]
pub struct ReviewListTotals {
    pub comments: usize,
    pub open: usize,
    pub addressed: usize,
    pub resolved: usize,
    pub outdated: usize,
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

    /// Index into [`Self::comments`] of the card `at` refers to, counting
    /// stacked cards on that line in render order.
    pub fn comment_index_at(&self, at: CommentRef) -> Option<usize> {
        let sha = self.current_commit_sha();
        self.comments
            .iter()
            .enumerate()
            .filter(|(_, c)| c.commit == sha && c.key == at.line)
            .map(|(i, _)| i)
            .nth(at.slot)
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
    fn set_files(
        &mut self,
        mut files: Vec<FileHunks>,
        scanner_ok: bool,
        untracked_suppressed: usize,
    ) {
        reorder_files_to_tree(&mut files);
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
        self.context_levels = vec![DEFAULT_CONTEXT; n];
        self.body_keys.clear();
        self.commit_loading = false;
        self.capture_wide_and_narrow();
        self.recompute_comment_counts();
    }

    /// Capture each file's just-scanned full-context hunks as the pinned
    /// [`Self::wide_hunks`] snapshot, then narrow the displayed `files`
    /// down to their current context level. Runs once whenever a fresh
    /// scan is installed (open / scope switch), so display and expansion
    /// both derive from the one snapshot with no further `git` calls. An
    /// oversize file (bounded fallback, expansion disabled) is left as-is.
    fn capture_wide_and_narrow(&mut self) {
        self.wide_hunks = self.files.iter().map(|f| f.hunks.clone()).collect();
        let narrowed: Vec<Vec<Hunk>> = self
            .wide_hunks
            .iter()
            .enumerate()
            .map(|(i, wide)| {
                let oversize = self.files.get(i).is_some_and(|f| f.oversize);
                if oversize {
                    return wide.clone();
                }
                let level = self.context_levels.get(i).copied().unwrap_or(DEFAULT_CONTEXT);
                narrow_hunks(wide, usize::try_from(level).unwrap_or(usize::MAX))
            })
            .collect();
        for (file, hunks) in self.files.iter_mut().zip(narrowed) {
            file.hunks = hunks;
        }
    }

    /// Point the overlay at `scope`, closing the jump dropdown. If the
    /// scope's hunks are cached, swap them in and report `NavOutcome::Ready`;
    /// otherwise mark it loading and report `NavOutcome::NeedsScan` so
    /// the caller can spawn the lazy fetch. Commit scopes never carry
    /// untracked files (a commit's diff is closed over its own changes),
    /// so the untracked count is 0.
    /// The only place `scope` is assigned. Assigning it anywhere else
    /// renders the previous visit's cards against the new scope, and no
    /// test fails - the cards are rebuilt by [`after_nav`] consuming the
    /// outcome this returns, not by the assignment.
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

    /// Expand file `idx`'s shown context by one step, re-slicing its
    /// display hunks from the pinned wide snapshot in memory (no `git`):
    /// bump the level, re-narrow, drop the file's stale height + highlight
    /// caches, and re-anchor its comments onto the new hunk coordinates.
    /// Expansion only adds context lines, so every already-shown line
    /// survives - a comment's line always re-resolves.
    pub fn expand_file_context(&mut self, idx: usize) {
        // An oversize file has only a bounded fallback snapshot - nothing
        // more to reveal - so its expanders are suppressed; guard here too.
        if self.files.get(idx).is_some_and(|f| f.oversize) {
            return;
        }
        // Guard the snapshot BEFORE bumping the level so a desynced vec
        // can't silently advance the level with nothing to re-slice.
        let level = match self.context_levels.get(idx) {
            Some(current) => {
                usize::try_from(current.saturating_add(CONTEXT_STEP)).unwrap_or(usize::MAX)
            }
            None => return,
        };
        let Some(narrowed) = self.wide_hunks.get(idx).map(|wide| narrow_hunks(wide, level)) else {
            return;
        };
        if let Some(slot) = self.context_levels.get_mut(idx) {
            *slot = slot.saturating_add(CONTEXT_STEP);
        }
        if let Some(file) = self.files.get_mut(idx) {
            file.hunks = narrowed;
        }
        if let Some(slot) = self.measured_heights.get_mut(idx) {
            *slot = None;
        }
        if let Some(slot) = self.highlighted.get_mut(idx) {
            *slot = None;
        }
        self.reanchor_comments_in_file(idx);
    }

    /// Remap the `LineKey` of every comment anchored in file `idx` onto
    /// the file's current hunks by matching each comment's line number on
    /// its side. Used after a context expand rewrites the hunk structure
    /// (hunks merge, line indices shift) so a comment keeps pointing at
    /// its line rather than a stale coordinate.
    fn reanchor_comments_in_file(&mut self, idx: usize) {
        // Only the current scope's comments index this scope's file set;
        // an off-scope comment's key points at a different file layout.
        let sha = self.current_commit_sha();
        let Some(file) = self.files.get(idx) else { return };
        // Snapshot the remaps first so the immutable borrow of `file`
        // drops before mutating `self.comments`.
        let remaps: Vec<(usize, LineKey)> = self
            .comments
            .iter()
            .enumerate()
            .filter(|(_, c)| c.key.file_idx == idx && c.commit == sha)
            .filter_map(|(pos, c)| {
                let side = c.thread.anchor.side;
                find_line_key(file, idx, side, c.line).map(|key| (pos, key))
            })
            .collect();
        for (pos, key) in remaps {
            if let Some(c) = self.comments.get_mut(pos) {
                c.key = key;
            }
        }
        self.recompute_comment_counts();
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

    /// Toggle between the current commit and the whole-branch ("All
    /// changes") diff without opening the jump dropdown. From a commit it
    /// remembers the index and switches to whole-diff; from whole-diff it
    /// returns to the remembered commit (or the first commit when none is
    /// remembered). No-op (`None`) in whole-diff-only mode.
    pub fn toggle_all_changes(&mut self) -> Option<NavOutcome> {
        if self.commits.is_empty() {
            return None;
        }
        let target = match self.scope {
            DiffScope::Commit(i) => {
                self.last_commit = Some(i);
                DiffScope::WholeDiff
            }
            DiffScope::WholeDiff => {
                let i = self.last_commit.filter(|&i| i < self.commits.len()).unwrap_or(0);
                DiffScope::Commit(i)
            }
        };
        Some(self.select_scope(target))
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

    /// Whether the card for `comment` renders as a one-line marker.
    pub fn is_comment_collapsed(&self, comment: &HunkComment) -> bool {
        comment.thread.status == ReviewStatus::Resolved
            && !self.resolved_expanded.contains(&comment.thread.id)
    }

    /// Expand a collapsed resolved comment, or re-collapse an expanded
    /// one. Keyed on the thread id, so it survives a re-anchor moving the
    /// card to another line. Clears the file's measured height so the next
    /// frame re-measures it at the new row count.
    pub fn toggle_comment_collapse(&mut self, at: CommentRef) -> bool {
        let Some(id) = self.comment_index_at(at).map(|i| self.comments[i].thread.id.clone()) else {
            return false;
        };
        if !self.resolved_expanded.remove(&id) {
            self.resolved_expanded.insert(id);
        }
        if let Some(slot) = self.measured_heights.get_mut(at.line.file_idx) {
            *slot = None;
        }
        true
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
        let last = self.files.len().saturating_sub(1);
        let heights: Vec<u32> = self
            .files
            .iter()
            .enumerate()
            .map(|(idx, file)| {
                self.measured_heights.get(idx).copied().flatten().unwrap_or_else(|| {
                    // The measured height already folds in the trailing
                    // end-cap; mirror it in the estimate for every file
                    // but the last so an unmeasured file's start row lines
                    // up with what the renderer draws.
                    let cap = if idx < last { END_CAP_ROWS } else { 0 };
                    estimated_height(file, self.is_collapsed(idx)).saturating_add(cap)
                })
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
    pub fn new(cwd: PathBuf, target: String, mut files: Vec<FileHunks>) -> Self {
        reorder_files_to_tree(&mut files);
        let file_count = files.len();
        let mut state = Self {
            cwd,
            target,
            files,
            scanner_ok: true,
            untracked_suppressed: 0,
            doc_scroll: 0,
            view_mode: DiffViewMode::default(),
            deleted_expanded: std::collections::HashSet::new(),
            resolved_expanded: std::collections::HashSet::new(),
            rail_scroll: 0,
            comments: Vec::new(),
            active_input: None,
            body_keys: Vec::new(),
            pane_origin_row: 0,
            pane_origin_col: 0,
            pane_width: 0,
            content_origin_col: 0,
            rail_origin_row: 0,
            comment_counts: vec![0; file_count],
            rail_keys: Vec::new(),
            body_head_rows: 0,
            body_tail_scroll: 0,
            current_file_idx: 0,
            message_rows: 0,
            measured_heights: vec![None; file_count],
            highlighted: vec![None; file_count],
            context_levels: vec![DEFAULT_CONTEXT; file_count],
            wide_hunks: Vec::new(),
            commits: Vec::new(),
            branch: None,
            scope: DiffScope::WholeDiff,
            last_commit: None,
            commit_cache: Vec::new(),
            whole_diff_cache: None,
            commit_loading: false,
            jump_open: false,
            jump_selected: 0,
            jump_hint_span: None,
            review_load_error: None,
            finish_review: None,
            finish_submit_span: None,
            reviews: Vec::new(),
            reviews_open: false,
            reviews_selected: 0,
            review_rows: Vec::new(),
            review_totals: ReviewListTotals::default(),
        };
        state.capture_wide_and_narrow();
        state
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
            mut files,
            scanner_ok,
            untracked_suppressed,
            seq: _,
            kind,
            commit_body,
        } = event;
        reorder_files_to_tree(&mut files);
        // drain_events only routes Initial events here; a Scope event
        // would be a bug, so fall back to whole-diff defensively.
        let (mut commits, branch, initial_scope) = match kind {
            DiffScanKind::Initial { commits, branch, scope } => (commits, branch, scope),
            DiffScanKind::Scope(_) => (Vec::new(), None, DiffScope::WholeDiff),
        };
        // The opened commit's body arrives with its upfront-scanned diff.
        if let (Some(body), DiffScope::Commit(idx)) = (commit_body, initial_scope)
            && let Some(commit) = commits.get_mut(idx)
        {
            commit.body = body;
        }
        let file_count = files.len();
        // Open on the resolved scope: a commit caches `files` at its slot;
        // whole-diff (or a stale index) caches `files` as the whole diff.
        let (scope, commit_cache, whole_diff_cache) = match initial_scope {
            DiffScope::Commit(idx) if idx < commits.len() => {
                let mut cache = vec![None; commits.len()];
                cache[idx] = Some(CachedScan { files: files.clone(), scanner_ok });
                (DiffScope::Commit(idx), cache, None)
            }
            _ => (
                DiffScope::WholeDiff,
                vec![None; commits.len()],
                Some(CachedScan { files: files.clone(), scanner_ok }),
            ),
        };
        let mut state = Self {
            cwd,
            target,
            files,
            scanner_ok,
            untracked_suppressed,
            doc_scroll: 0,
            view_mode: DiffViewMode::default(),
            deleted_expanded: std::collections::HashSet::new(),
            resolved_expanded: std::collections::HashSet::new(),
            rail_scroll: 0,
            comments: Vec::new(),
            active_input: None,
            body_keys: Vec::new(),
            pane_origin_row: 0,
            pane_origin_col: 0,
            pane_width: 0,
            content_origin_col: 0,
            rail_origin_row: 0,
            comment_counts: vec![0; file_count],
            rail_keys: Vec::new(),
            body_head_rows: 0,
            body_tail_scroll: 0,
            current_file_idx: 0,
            message_rows: 0,
            measured_heights: vec![None; file_count],
            highlighted: vec![None; file_count],
            context_levels: vec![DEFAULT_CONTEXT; file_count],
            wide_hunks: Vec::new(),
            commits,
            branch,
            scope,
            last_commit: None,
            commit_cache,
            whole_diff_cache,
            commit_loading: false,
            jump_open: false,
            jump_selected: 0,
            jump_hint_span: None,
            review_load_error: None,
            finish_review: None,
            finish_submit_span: None,
            reviews: Vec::new(),
            reviews_open: false,
            reviews_selected: 0,
            review_rows: Vec::new(),
            review_totals: ReviewListTotals::default(),
        };
        state.capture_wide_and_narrow();
        state
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

/// Drop the overlay state and transition back to chat. The Esc submit
/// path lives in [`close_with_submit`] - call this directly only when
/// comments have already been handled (or the caller is the Esc-cancel
/// path for the active input editor).
pub(crate) fn close(app: &mut App) {
    app.diff_overlay = None;
    set_active_view(app, ActiveView::Chat);
    app.needs_redraw = true;
}

/// Placement for an outdated thread whose exact line may be gone,
/// avoiding any `occupied` key so it never lands on a line a live thread
/// already holds. Preference: the same line number on the same side,
/// else the nearest FREE surviving line in the file, else the file's
/// first free line, else the document's first free line; only when no
/// free line remains does it stack on the nearest occupied line. Returns
/// the key plus the anchored line's context (empty on a fallback line).
/// `None` only when the diff has no lines at all.
fn outdated_placement(
    files: &[FileHunks],
    path: &str,
    side: ReviewSide,
    line: u32,
    occupied: &std::collections::HashSet<LineKey>,
) -> Option<LineKey> {
    if let Some(file_idx) = files.iter().position(|f| f.path == path) {
        // Same-side candidates, nearest first (stable, so equal distances
        // keep document order).
        let mut candidates: Vec<(u32, LineKey)> = Vec::new();
        for (hunk_idx, hunk) in files[file_idx].hunks.iter().enumerate() {
            for (line_idx, diff_line) in hunk.lines.iter().enumerate() {
                let number = match side {
                    ReviewSide::Old => diff_line.old_line,
                    ReviewSide::New => diff_line.new_line,
                };
                if let Some(number) = number {
                    candidates
                        .push((number.abs_diff(line), LineKey { file_idx, hunk_idx, line_idx }));
                }
            }
        }
        candidates.sort_by_key(|(dist, _)| *dist);
        if let Some((_, key)) = candidates.iter().find(|(_, key)| !occupied.contains(key)) {
            return Some(*key);
        }
        // Same-side lines all taken: a free line anywhere in the file.
        if let Some(key) = first_free_line_in_file(&files[file_idx], file_idx, occupied) {
            return Some(key);
        }
        // Genuinely no free line in the file: stack on the nearest.
        if let Some((_, key)) = candidates.first() {
            return Some(*key);
        }
    }
    // File absent: the document's first free line, else stack on its first.
    first_free_line(files, occupied).or_else(|| first_line_key(files))
}

/// The first line's key in `file` not already in `occupied` (skipping
/// empty hunks), or `None` when every line is taken or absent.
fn first_free_line_in_file(
    file: &FileHunks,
    file_idx: usize,
    occupied: &std::collections::HashSet<LineKey>,
) -> Option<LineKey> {
    file.hunks.iter().enumerate().find_map(|(hunk_idx, hunk)| {
        (0..hunk.lines.len())
            .map(|line_idx| LineKey { file_idx, hunk_idx, line_idx })
            .find(|key| !occupied.contains(key))
    })
}

/// The first free line's key across the whole document.
fn first_free_line(
    files: &[FileHunks],
    occupied: &std::collections::HashSet<LineKey>,
) -> Option<LineKey> {
    files
        .iter()
        .enumerate()
        .find_map(|(file_idx, file)| first_free_line_in_file(file, file_idx, occupied))
}

/// The first line's key across the whole document, or `None` when the
/// diff has no lines. The last-resort stack anchor when no line is free.
fn first_line_key(files: &[FileHunks]) -> Option<LineKey> {
    files.iter().enumerate().find_map(|(file_idx, file)| {
        file.hunks
            .iter()
            .enumerate()
            .find(|(_, hunk)| !hunk.lines.is_empty())
            .map(|(hunk_idx, _)| LineKey { file_idx, hunk_idx, line_idx: 0 })
    })
}

/// The `LineKey` of the line in `file` whose number on `side` equals
/// `line`, or `None` when no such line is present. Used to re-anchor a
/// comment onto a file's hunks after they change.
fn find_line_key(
    file: &FileHunks,
    file_idx: usize,
    side: ReviewSide,
    line: u32,
) -> Option<LineKey> {
    for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
        for (line_idx, diff_line) in hunk.lines.iter().enumerate() {
            let number = match side {
                ReviewSide::Old => diff_line.old_line,
                ReviewSide::New => diff_line.new_line,
            };
            if number == Some(line) {
                return Some(LineKey { file_idx, hunk_idx, line_idx });
            }
        }
    }
    None
}

/// The user-authored text of a thread (first user comment), for the
/// existing chip/box render path.
fn thread_text(thread: &forge_primitives::ReviewThread) -> String {
    thread
        .comments
        .iter()
        .find(|c| matches!(c.author, ReviewAuthor::User))
        .or_else(|| thread.comments.first())
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

/// Whether a stored thread renders in the scope now on screen. The whole
/// diff is a union over the branch, so a rewrite that erases the commit a
/// thread was authored against cannot put it out of reach; a commit's own
/// view takes only what was authored there.
///
/// Only the whole diff checks `base_ref`, because only it is numbered
/// against the target: a thread counted from another base would land on
/// unrelated code. A commit's diff is `sha^..sha`, whose line numbers do
/// not depend on the target at all, so filtering it by base would hide a
/// thread from the one view that can place it correctly.
fn thread_in_scope(thread: &ReviewThread, scope_commit: Option<&str>, target: &str) -> bool {
    match scope_commit {
        Some(sha) => thread.commit.as_deref() == Some(sha),
        None => thread.anchor.base_ref == target,
    }
}

/// Load persisted review threads for the current scope (the active
/// commit's sha, or whole-diff when `None`), re-anchor each against the
/// fresh scan, and install them as the overlay's comments for that scope
/// (replacing the prior in-scope set, leaving other scopes' comments
/// untouched). Moved-line updates and drift-to-`Outdated` flips are
/// written back to redb. No-op without a workspace / project / branch.
fn hydrate_threads(app: &mut App) {
    let project = app.active_session().and_then(|s| s.project.clone());
    let workspace = app.workspace.clone();
    let Some(overlay) = app.diff_overlay.as_mut() else {
        return;
    };
    let (Some(project), Some(branch), Some(workspace)) =
        (project, overlay.branch.clone(), workspace)
    else {
        return;
    };

    // Reviews are branch-global (scope-independent); refresh them here so
    // chip tags and the `l` list reflect what's on disk. A corrupt reviews
    // row surfaces the same "failed to load" banner as the threads path -
    // the `reviews` table is a separate row, so its failure is independent.
    match workspace.load_reviews(&project, &branch) {
        Ok(reviews) => overlay.reviews = reviews,
        Err(error) => {
            overlay.review_load_error = Some(error);
            app.needs_redraw = true;
            return;
        }
    }

    // Surface a load failure as a visible notice rather than a silent
    // empty pane; a successful load clears any prior notice.
    let loaded = match workspace.load_review_threads(&project, &branch) {
        Ok(threads) => {
            overlay.review_load_error = None;
            threads
        }
        Err(error) => {
            overlay.review_load_error = Some(error);
            app.needs_redraw = true;
            return;
        }
    };
    // Threads are keyed by (project, branch) across every scope; process
    // only those in the current scope (the active commit's sha, or
    // whole-diff threads against the current target), keeping the rest
    // untouched so the whole-row writeback below preserves them instead of
    // silently dropping other scopes' threads.
    let scope_commit = overlay.current_commit_sha();
    let target = overlay.target.clone();
    let (mine, others): (Vec<_>, Vec<_>) =
        loaded.into_iter().partition(|t| thread_in_scope(t, scope_commit.as_deref(), &target));
    let had_in_scope = overlay.comments.iter().any(|c| c.commit == scope_commit);
    if mine.is_empty() && !had_in_scope {
        // Nothing in scope to re-anchor, so `others` is already every
        // thread on the branch at its final status.
        park_replies_waiting(app, &branch, &others);
        return;
    }

    let mut rebuilt = Vec::with_capacity(mine.len());
    let mut persist = others;
    let mut changed = false;
    // Live (in-place / moved) threads claim their real line in pass 1;
    // outdated fallbacks fill the remaining free lines in pass 2, so an
    // outdated box never lands on a key a live thread already holds
    // (which would route a click / edit to the wrong thread).
    let mut occupied: std::collections::HashSet<LineKey> = std::collections::HashSet::new();
    let mut deferred_outdated = Vec::new();
    for mut thread in mine {
        match resolver::resolve_anchor(&thread.anchor, &overlay.files) {
            AnchorResolution::InPlace { file_idx, hunk_idx, line_idx } => {
                let resolved = overlay
                    .files
                    .get(file_idx)
                    .and_then(|f| f.hunks.get(hunk_idx))
                    .and_then(|h| h.lines.get(line_idx));
                let line = resolved
                    .and_then(|dl| match thread.anchor.side {
                        ReviewSide::Old => dl.old_line,
                        ReviewSide::New => dl.new_line,
                    })
                    .unwrap_or(thread.anchor.line);
                if thread.anchor.line != line {
                    thread.anchor.line = line;
                    changed = true;
                }
                if thread.status == ReviewStatus::Outdated {
                    // The line came back; drop the drift flag.
                    thread.status = ReviewStatus::Open;
                    changed = true;
                }
                let key = LineKey { file_idx, hunk_idx, line_idx };
                occupied.insert(key);
                rebuilt.push(HunkComment {
                    key,
                    path: thread.anchor.path.clone(),
                    line,
                    comment_text: thread_text(&thread),
                    commit: scope_commit.clone(),
                    thread: thread.clone(),
                    authored_this_session: false,
                    anchor_note: None,
                    persisted: true,
                });
                persist.push(thread);
            }
            AnchorResolution::Moved { file_idx, hunk_idx, line_idx, from } => {
                let resolved = overlay
                    .files
                    .get(file_idx)
                    .and_then(|f| f.hunks.get(hunk_idx))
                    .and_then(|h| h.lines.get(line_idx));
                let line = resolved
                    .and_then(|dl| match thread.anchor.side {
                        ReviewSide::Old => dl.old_line,
                        ReviewSide::New => dl.new_line,
                    })
                    .unwrap_or(thread.anchor.line);
                if thread.anchor.line != line {
                    thread.anchor.line = line;
                    changed = true;
                }
                if thread.status == ReviewStatus::Outdated {
                    thread.status = ReviewStatus::Open;
                    changed = true;
                }
                let key = LineKey { file_idx, hunk_idx, line_idx };
                occupied.insert(key);
                rebuilt.push(HunkComment {
                    key,
                    path: thread.anchor.path.clone(),
                    line,
                    comment_text: thread_text(&thread),
                    commit: scope_commit.clone(),
                    thread: thread.clone(),
                    authored_this_session: false,
                    anchor_note: Some(AnchorNote::Moved { from }),
                    persisted: true,
                });
                persist.push(thread);
            }
            AnchorResolution::Outdated(reason) => {
                if !matches!(thread.status, ReviewStatus::Resolved | ReviewStatus::Outdated) {
                    thread.status = ReviewStatus::Outdated;
                    changed = true;
                }
                deferred_outdated.push((thread, reason));
            }
        }
    }
    // Pass 2: place outdated threads on a surviving FREE line so they
    // render (yellow, against their captured context) without clobbering
    // a co-located live thread.
    for (thread, reason) in deferred_outdated {
        let Some(key) = outdated_placement(
            &overlay.files,
            &thread.anchor.path,
            thread.anchor.side,
            thread.anchor.line,
            &occupied,
        ) else {
            // Empty diff this open: keep the thread durable (it re-anchors
            // when the diff returns) but skip rendering.
            persist.push(thread);
            continue;
        };
        occupied.insert(key);
        rebuilt.push(HunkComment {
            key,
            path: thread.anchor.path.clone(),
            line: thread.anchor.line,
            comment_text: thread_text(&thread),
            commit: scope_commit.clone(),
            thread: thread.clone(),
            authored_this_session: false,
            anchor_note: Some(AnchorNote::Outdated(reason)),
            persisted: true,
        });
        persist.push(thread);
    }

    overlay.comments.retain(|c| c.commit != scope_commit);
    overlay.comments.extend(rebuilt);
    overlay.recompute_comment_counts();
    if changed {
        workspace.save_review_threads(&project, &branch, &persist);
    }
    // `persist` is every thread on the branch, post re-anchoring - the
    // authoritative recompute that self-corrects a parked count drifted
    // from the store.
    park_replies_waiting(app, &branch, &persist);
    app.needs_redraw = true;
}

/// Park how many of `threads` still owe the reviewer a turn onto the
/// active session bucket, so the GIT badge and the NEEDS ATTENTION band
/// render from a field instead of querying the store per frame.
fn park_replies_waiting(app: &mut App, branch: &str, threads: &[ReviewThread]) {
    let count = threads.iter().filter(|t| t.awaits_reviewer()).count();
    if let Some(session) = app.try_active_bucket_mut() {
        session.review_replies_waiting = crate::app::ReviewRepliesWaiting::merge(
            session.review_replies_waiting.as_ref(),
            branch,
            count,
        );
    }
}

/// Re-park the waiting count after the reviewer mutated a thread. Reads
/// the store because the overlay only holds the current scope's threads;
/// safe on a keypress (the mutation itself already wrote), never on the
/// render path.
fn refresh_replies_waiting(app: &mut App) {
    let Some(branch) = app.diff_overlay.as_ref().and_then(|o| o.branch.clone()) else {
        return;
    };
    let Some(project) = app.active_session().and_then(|s| s.project.clone()) else {
        return;
    };
    let Some(workspace) = app.workspace.clone() else {
        return;
    };
    let Ok(threads) = workspace.load_review_threads(&project, &branch) else {
        return;
    };
    park_replies_waiting(app, &branch, &threads);
}

/// Handle a key while the diff overlay is active.
///
/// Routing depends on whether an inline comment editor is open:
/// - Editor open:
///   - `Esc` cancels the editor and returns focus to the diff.
///   - `Enter` (plain, no modifier) saves the edit.
///   - All other keys flow into the editor (typing, cursor
///     movement, paste-via-bracket, undo/redo, etc.).
/// - No editor open:
///   - `Esc` closes the overlay; a submit seals this session's authored
///     comments into a numbered review and nudges the agent (one line)
///     to address it via the review MCP. The nudge fires synchronously
///     through `input_submit::dispatch_review_nudge` so the user sees the
///     bubble appear immediately.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    // A paste queued earlier in this drain cycle owns any editing-like
    // key that follows it - without this a chunked paste's trailing
    // newline saves the comment instead of landing in the text.
    if app.has_focused_text_input() && super::keys::should_ignore_key_during_paste(app, key) {
        return;
    }
    // A picker left open by a mouse-driven editor close has nowhere to
    // insert into.
    if app.emoji.is_some() && !app.has_focused_text_input() {
        super::emoji::deactivate(app);
    }
    // The emoji picker is the innermost surface: while it is open it owns
    // Esc, Enter and the arrows. Without this, `:` then Esc would fall
    // through to the overlay's Esc - which submits the review.
    if app.emoji.is_some() && super::keys::handle_emoji_key(app, key) {
        app.needs_redraw = true;
        return;
    }
    // Finish-review modal captures keys while open: type the overview,
    // Ctrl+Enter submits, Esc dismisses back to the diff.
    if app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()) {
        handle_finish_review_key(app, key);
        return;
    }
    // The reviews list captures keys while open: navigate rows, Enter to
    // jump, `l` / Esc to close.
    if app.diff_overlay.as_ref().is_some_and(|o| o.reviews_open) {
        handle_reviews_list_key(app, key);
        return;
    }
    let has_input = app.diff_overlay.as_ref().is_some_and(|o| o.active_input.is_some());
    if has_input {
        match key.code {
            KeyCode::Esc => {
                app.paste_burst.on_non_char_key(Instant::now());
                cancel_active_input(app);
            }
            // Enter is only a save when it is really a keypress. Mid-burst
            // - or in the window right after one - it belongs to the
            // dictated / pasted payload, so it goes to the buffer instead.
            KeyCode::Enter if app.paste_burst.on_enter(Instant::now()) => {}
            KeyCode::Enter if !key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
                save_active_input(app);
            }
            _ => route_key_into_review_editor(app, key),
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
        KeyCode::Char('a') => toggle_all_changes(app),
        KeyCode::Char('j') => open_jump(app),
        KeyCode::Char('l') => toggle_reviews_list(app),
        _ => {}
    }
}

/// Route a key while the Finish-review modal is open: Esc dismisses it
/// back to the diff (keep editing), Ctrl+Enter submits, everything else
/// flows into the overview editor.
fn handle_finish_review_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.paste_burst.on_non_char_key(Instant::now());
            if let Some(o) = app.diff_overlay.as_mut() {
                o.finish_review = None;
                app.needs_redraw = true;
            }
        }
        KeyCode::Enter if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
            submit_finish_review(app);
        }
        _ => route_key_into_review_editor(app, key),
    }
}

/// Route a key the review editors don't claim for their own semantics
/// into whichever editor has focus. Printable characters go through the
/// shared paste-burst detector so a dictation run coalesces into one
/// payload; everything else is ordinary `TextArea` editing.
fn route_key_into_review_editor(app: &mut App, key: KeyEvent) {
    let printable = match (key.code, key.modifiers) {
        (KeyCode::Char(c), m) if super::keys::is_printable_text_modifiers(m) => Some(c),
        _ => None,
    };
    if let Some(c) = printable {
        // Only offer the picker for a character that actually landed - a
        // `:` swallowed into a paste burst is payload, not a trigger.
        if app.type_char(c, Instant::now()) == TypedChar::Inserted && c == ':' {
            super::emoji::activate(app);
        }
    } else {
        app.paste_burst.on_non_char_key(Instant::now());
        if let Some(input) = app.focused_input_mut() {
            let _ = input.handle_key(key);
        }
        super::emoji::sync_with_cursor(app);
    }
    app.needs_redraw = true;
}

/// Parse an rfc3339 timestamp into a `SystemTime`, or `None` when it is
/// empty / malformed.
fn parse_rfc3339(text: &str) -> Option<std::time::SystemTime> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(text, &Rfc3339).ok().map(std::time::SystemTime::from)
}

/// Build the reviews-list rows from the branch's reviews plus every
/// thread's current state, newest review first. Each row tallies the
/// review's member threads (those with a turn filed into it) by status and
/// records the first member's scope + path for navigation. A thread the
/// reviewer replied on across rounds is a member of each of them.
fn compute_review_rows(
    reviews: &[forge_primitives::ReviewSet],
    threads: &[ReviewThread],
    now: std::time::SystemTime,
) -> Vec<ReviewListRow> {
    reviews
        .iter()
        .rev()
        .map(|review| {
            let members: Vec<&ReviewThread> =
                threads.iter().filter(|t| t.is_in_review(&review.id)).collect();
            let mut open = 0;
            let mut addressed = 0;
            let mut resolved = 0;
            let mut outdated = 0;
            for t in &members {
                match t.status {
                    ReviewStatus::Open => open += 1,
                    ReviewStatus::Addressed => addressed += 1,
                    ReviewStatus::Resolved => resolved += 1,
                    ReviewStatus::Outdated => outdated += 1,
                }
            }
            let age = parse_rfc3339(&review.created_at)
                .map(|at| crate::ui::format::relative_time(at, now))
                .unwrap_or_default();
            let first = members.first();
            ReviewListRow {
                number: review.number,
                age,
                total: members.len(),
                open,
                addressed,
                resolved,
                outdated,
                summary: review.summary.clone().filter(|s| !s.trim().is_empty()),
                first_commit: first.and_then(|t| t.commit.clone()),
                first_path: first.map(|t| t.anchor.path.clone()),
            }
        })
        .collect()
}

/// Tally the branch's filed comments for the reviews-list footer, counting
/// a thread once however many reviews its turns span.
fn compute_review_totals(
    reviews: &[forge_primitives::ReviewSet],
    threads: &[ReviewThread],
) -> ReviewListTotals {
    let mut totals = ReviewListTotals::default();
    for thread in threads.iter().filter(|t| reviews.iter().any(|r| t.is_in_review(&r.id))) {
        totals.comments += 1;
        match thread.status {
            ReviewStatus::Open => totals.open += 1,
            ReviewStatus::Addressed => totals.addressed += 1,
            ReviewStatus::Resolved => totals.resolved += 1,
            ReviewStatus::Outdated => totals.outdated += 1,
        }
    }
    totals
}

/// Toggle the `l` REVIEWS list. Opening snapshots every thread's current
/// state into per-review rollups (newest first); closing drops the rows.
fn toggle_reviews_list(app: &mut App) {
    if app.diff_overlay.as_ref().is_some_and(|o| o.reviews_open) {
        if let Some(o) = app.diff_overlay.as_mut() {
            o.reviews_open = false;
            o.review_rows.clear();
            o.review_totals = ReviewListTotals::default();
        }
        app.needs_redraw = true;
        return;
    }
    let project = app.active_session().and_then(|s| s.project.clone());
    let workspace = app.workspace.clone();
    let branch = app.diff_overlay.as_ref().and_then(|o| o.branch.clone());
    let threads = match (project, branch, workspace) {
        (Some(project), Some(branch), Some(workspace)) => {
            match workspace.load_review_threads(&project, &branch) {
                Ok(threads) => threads,
                Err(error) => {
                    // Surface the failure via the banner rather than opening
                    // the list with silently-empty rollups.
                    if let Some(o) = app.diff_overlay.as_mut() {
                        o.review_load_error = Some(error);
                    }
                    app.needs_redraw = true;
                    return;
                }
            }
        }
        _ => Vec::new(),
    };
    let now = std::time::SystemTime::now();
    if let Some(o) = app.diff_overlay.as_mut() {
        o.review_rows = compute_review_rows(&o.reviews, &threads, now);
        o.review_totals = compute_review_totals(&o.reviews, &threads);
        o.reviews_selected = 0;
        o.reviews_open = true;
    }
    app.needs_redraw = true;
}

/// Route a key while the reviews list is open: `↑↓` move the highlight,
/// Enter navigates to the selected review's first comment, `l` / Esc
/// close the list.
fn handle_reviews_list_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('l') => toggle_reviews_list(app),
        KeyCode::Up => {
            if let Some(o) = app.diff_overlay.as_mut() {
                o.reviews_selected = o.reviews_selected.saturating_sub(1);
                app.needs_redraw = true;
            }
        }
        KeyCode::Down => {
            if let Some(o) = app.diff_overlay.as_mut() {
                let last = o.review_rows.len().saturating_sub(1);
                o.reviews_selected = o.reviews_selected.saturating_add(1).min(last);
                app.needs_redraw = true;
            }
        }
        KeyCode::Enter => navigate_to_selected_review(app),
        _ => {}
    }
}

/// Close the list and scroll the diff to the selected review's first
/// comment: jump to its file when it's in the current scope, else switch
/// to its scope (the comment surfaces once that scan + hydrate land).
fn navigate_to_selected_review(app: &mut App) {
    let target = app.diff_overlay.as_ref().and_then(|o| {
        o.review_rows
            .get(o.reviews_selected)
            .map(|r| (r.first_commit.clone(), r.first_path.clone()))
    });
    if let Some(o) = app.diff_overlay.as_mut() {
        o.reviews_open = false;
        o.review_rows.clear();
        o.review_totals = ReviewListTotals::default();
    }
    app.needs_redraw = true;
    let Some((first_commit, first_path)) = target else { return };

    let scopes = app.diff_overlay.as_ref().map(|o| {
        let target_scope = match &first_commit {
            None => DiffScope::WholeDiff,
            Some(sha) => {
                o.commits.iter().position(|c| &c.sha == sha).map_or(o.scope, DiffScope::Commit)
            }
        };
        (target_scope, o.scope)
    });
    let Some((target_scope, current_scope)) = scopes else { return };
    if target_scope != current_scope {
        let outcome = app.diff_overlay.as_mut().map(|o| o.select_scope(target_scope));
        if let Some(outcome) = outcome {
            after_nav(app, outcome);
        }
        return;
    }
    if let Some(path) = first_path
        && let Some(o) = app.diff_overlay.as_mut()
        && let Some(file_idx) = o.files.iter().position(|f| f.path == path)
    {
        let file_start = o.doc_offsets().starts.get(file_idx).copied().unwrap_or(0);
        o.doc_scroll = o.message_rows.saturating_add(file_start);
    }
}

/// Map a comment-button click to its transition and run it on the thread
/// at `key`, so a click resolves exactly the card it landed on. A Reopen
/// that actually flips re-nudges the worker to take another look.
fn apply_thread_action(app: &mut App, at: CommentRef, action: ThreadAction) {
    let (next, allowed_from): (ReviewStatus, &[ReviewStatus]) = match action {
        ThreadAction::Resolve => (
            ReviewStatus::Resolved,
            &[ReviewStatus::Open, ReviewStatus::Addressed, ReviewStatus::Outdated],
        ),
        ThreadAction::Reopen => {
            (ReviewStatus::Open, &[ReviewStatus::Addressed, ReviewStatus::Resolved])
        }
    };
    if set_thread_status_by_key(app, at, next, allowed_from) {
        if matches!(action, ThreadAction::Reopen) {
            renudge_reopened(app, at);
        }
        refresh_replies_waiting(app);
    }
}

/// Flip the thread the card at `at` carries to `next` when it is
/// currently in one of `allowed_from`, updating the in-memory card and
/// persisting the change. Returns whether it flipped. No-op (returns
/// `false`) when nothing is stacked there or its status isn't a legal
/// source.
fn set_thread_status_by_key(
    app: &mut App,
    at: CommentRef,
    next: ReviewStatus,
    allowed_from: &[ReviewStatus],
) -> bool {
    let project = app.active_session().and_then(|s| s.project.clone());
    let Some(overlay) = app.diff_overlay.as_mut() else {
        return false;
    };
    let Some(branch) = overlay.branch.clone() else {
        return false;
    };
    let Some(idx) = overlay.comment_index_at(at) else {
        return false;
    };
    let Some(thread) = overlay
        .comments
        .get_mut(idx)
        .map(|c| &mut c.thread)
        .filter(|t| allowed_from.contains(&t.status))
    else {
        return false;
    };
    thread.status = next;
    let id = thread.id.clone();
    if next != ReviewStatus::Resolved {
        // Expansion only means anything while a thread is collapsed by
        // default, and it is remembered per thread - so a thread that
        // leaves Resolved and comes back would otherwise return expanded
        // while every other resolved one is a marker.
        overlay.resolved_expanded.remove(&id);
    }
    // Entering or leaving Resolved swaps the card for a marker, so the
    // file's row count changed; clear its height like a collapse toggle.
    if let Some(slot) = overlay.measured_heights.get_mut(at.line.file_idx) {
        *slot = None;
    }
    app.needs_redraw = true;
    if let Some(project) = project
        && let Some(workspace) = app.workspace.as_ref()
    {
        workspace.set_review_thread_status(&project, &branch, &id, next);
    }
    true
}

/// Nudge the worker after a comment is reopened, so it re-reads the review
/// and addresses the reopened point. Names the review number when the
/// reopened thread is filed. A no-op when there's no agent/session to
/// receive it (the flip + persist already happened).
fn renudge_reopened(app: &mut App, at: CommentRef) {
    if !app.has_active_agent() || app.session_id().is_none() {
        return;
    }
    let review_tag = app.diff_overlay.as_ref().and_then(|overlay| {
        // The latest round, not the origin: that is the exchange the
        // reviewer is unhappy with.
        let review_id =
            overlay.comments.get(overlay.comment_index_at(at)?)?.thread.latest_review()?;
        overlay.reviews.iter().find(|r| r.id == review_id).map(|r| r.number)
    });
    let nudge = match review_tag {
        Some(number) => format!(
            "Reopened a comment in review #{number} - take another look via the review MCP (`review__get`)."
        ),
        None => {
            "Reopened a review comment - take another look via the review MCP (`review__list`)."
                .to_owned()
        }
    };
    super::input_submit::dispatch_review_nudge(app, nudge);
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

/// Toggle between the current commit and the whole-branch diff (`a`),
/// spawning the target scope's scan when it isn't cached. No-op in
/// whole-diff-only mode.
fn toggle_all_changes(app: &mut App) {
    let outcome = app.diff_overlay.as_mut().and_then(DiffOverlayState::toggle_all_changes);
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
    match outcome {
        NavOutcome::NeedsScan(scope) => spawn_scope_scan(app, scope),
        // A cached scope installs its files without a scan, so this is
        // the only chance to rebuild its cards. They are a projection of
        // the store, and the copy left over from the last visit predates
        // whatever happened in the scope just left.
        NavOutcome::Ready => hydrate_threads(app),
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

/// Queue bracketed paste for whichever review editor has focus - the
/// inline comment editor or the Finish-review overview. Returns `true`
/// when the paste was accepted. Pastes with no editor open are dropped -
/// there's nothing for them to land on - but a DEBUG log fires so a user
/// reporting "my paste disappeared" can be triaged from logs.
///
/// The payload goes through the same queue the chat draft uses, so a
/// large paste collapses to a `[Pasted Text N]` block here too instead
/// of unrolling hundreds of rows into the comment box.
pub(crate) fn handle_paste(app: &mut App, text: &str) -> bool {
    if !app.has_focused_text_input() {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_paste_dropped_no_editor",
            message = "paste in Diff view without an open review editor - dropped",
            outcome = "dropped",
            paste_chars = text.chars().count(),
        );
        return false;
    }
    app.queue_paste_text(text);
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
    let current_text = input.editor.text();
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
/// hunk context so the captured context stays stable even if the
/// user scrolls / switches files later.
///
/// Empty-text save semantics:
/// - Fresh line-click editor (no `prior_comment`): treated as
///   cancel - saving a blank comment would render an empty chip.
/// - Turn-edit editor (`prior_comment` + `edit_turn = Some(idx)` on a
///   `User` turn): clearing removes just THAT turn and re-saves the
///   surviving chain, so an earlier note and any worker replies stay.
///   The whole thread is deleted only when no `User` turn would remain
///   (so an orphaned agent reply never lingers).
/// - Reply editor (`prior_comment` + `edit_turn = None`), or a clear
///   aimed at a non-editable turn: restore the prior untouched (no
///   delete, no new turn).
fn save_active_input(app: &mut App) {
    persist_active_input(app);
    // A reviewer turn on an answered thread hands the ball back to the
    // worker, and a cleared turn can hand it the other way.
    refresh_replies_waiting(app);
}

fn persist_active_input(app: &mut App) {
    // Project name is read before the overlay borrow so the persist call
    // below can reach `app.workspace` without a borrow conflict.
    let project = app.active_session().and_then(|s| s.project.clone());
    let workspace = app.workspace.clone();
    let Some(overlay) = app.diff_overlay.as_mut() else { return };
    let branch = overlay.branch.clone();
    let Some(input) = overlay.active_input.take() else { return };
    let text = input.editor.text();
    if text.trim().is_empty() {
        let edit_turn = input.edit_turn;
        if let Some(mut prior) = input.prior_comment {
            let clears_user_turn = edit_turn
                .and_then(|idx| prior.thread.comments.get(idx))
                .is_some_and(|c| matches!(c.author, ReviewAuthor::User));
            if let (true, Some(idx)) = (clears_user_turn, edit_turn) {
                prior.thread.comments.remove(idx);
                let user_turn_remains =
                    prior.thread.comments.iter().any(|c| matches!(c.author, ReviewAuthor::User));
                if user_turn_remains {
                    // Trim just this turn; re-save the surviving chain.
                    let persisted = if let (Some(project), Some(branch), Some(workspace)) =
                        (project.as_deref(), branch.as_deref(), workspace.as_ref())
                    {
                        workspace.upsert_review_thread(project, branch, prior.thread.clone())
                    } else {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "diff_overlay_review_thread_not_persisted",
                            message = "trimmed review thread could not be persisted (no branch / project / store); kept in-memory only",
                            outcome = "at_risk",
                            has_branch = branch.is_some(),
                            has_project = project.is_some(),
                        );
                        false
                    };
                    prior.comment_text = prior
                        .thread
                        .comments
                        .iter()
                        .find(|c| matches!(c.author, ReviewAuthor::User))
                        .map_or_else(String::new, |c| c.text.clone());
                    prior.persisted = persisted;
                    overlay.comments.push(prior);
                    overlay.recompute_comment_counts();
                } else {
                    // No user turn left: delete the whole thread (durable too,
                    // else hydrate resurrects it next open).
                    if let (Some(project), Some(branch), Some(workspace)) =
                        (project.as_deref(), branch.as_deref(), workspace.as_ref())
                    {
                        workspace.remove_review_thread(project, branch, &prior.thread.id);
                    } else {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "diff_overlay_review_thread_not_removed",
                            message = "review thread delete skipped (no branch / project / store); may resurrect on next open",
                            outcome = "skipped",
                            has_branch = branch.is_some(),
                            has_project = project.is_some(),
                        );
                    }
                }
            } else {
                // Empty reply, or a clear aimed at a non-editable turn:
                // restore the prior untouched.
                overlay.comments.push(prior);
                overlay.recompute_comment_counts();
            }
        }
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
    // comment on a brand-new file would otherwise capture the entire
    // file (Added hunks span the file body); now the captured context
    // stays compact and the agent gets precise per-line context.
    let commit = overlay.current_commit_sha();
    // Snapshot everything off the anchored line into owned locals so the
    // `overlay.files` borrows drop before the comment is pushed.
    let target = overlay.target.clone();
    let path = file.path.clone();
    let side = anchor_side(diff_line.kind);
    let content_hash = resolver::anchor_hash(&diff_line.text);
    let context = resolver::capture_context(hunk, key.line_idx, CONTEXT_RADIUS);
    let prior_thread = input.prior_comment.as_ref().map(|c| c.thread.clone());
    // Every scope persists a durable thread; `commit` records the scope
    // (the current sha, or `None` in whole-diff). Editing an existing chip
    // reuses the prior thread's identity + comment chain.
    let anchor = ReviewAnchor {
        path: path.clone(),
        side,
        line: line_no,
        content_hash,
        context,
        base_ref: target,
    };
    let is_new = prior_thread.is_none();
    let mut thread = build_thread(prior_thread, anchor, &text, input.edit_turn);
    if is_new {
        // A thread's home is where it was authored, and the whole diff
        // shows threads homed on a commit - so restamping it with the
        // view being edited from would evict it from its own commit.
        thread.commit.clone_from(&commit);
    }
    // The chip snippet / editor fallback mirror the first user turn, which
    // stays stable whether this save edited a later turn or appended a reply.
    let comment_text = thread
        .comments
        .iter()
        .find(|c| matches!(c.author, ReviewAuthor::User))
        .map_or_else(|| text.clone(), |c| c.text.clone());
    // Persist FIRST so `persisted` reflects a confirmed write. A durable
    // comment whose write is skipped (no branch / project / store) or
    // fails stays at-risk - view.rs counts it as droppable - rather than
    // being marked durable on scope alone.
    let persisted = if let (Some(project), Some(branch), Some(workspace)) =
        (project.as_deref(), branch.as_deref(), workspace.as_ref())
    {
        workspace.upsert_review_thread(project, branch, thread.clone())
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_review_thread_not_persisted",
            message = "review comment could not be persisted (no branch / project / store); kept in-memory only",
            outcome = "at_risk",
            has_branch = branch.is_some(),
            has_project = project.is_some(),
        );
        false
    };
    let comment = HunkComment {
        key,
        path,
        line: line_no,
        comment_text,
        commit,
        thread,
        authored_this_session: true,
        // Just anchored on the line the reviewer clicked.
        anchor_note: None,
        persisted,
    };
    // Replace this thread's card in this scope, and only that one. A line
    // carries as many cards as it has threads, so matching on the key
    // alone takes the neighbours with it; and a thread authored on a
    // commit renders in that commit AND in the whole diff, which
    // `hydrate_threads` keeps on purpose, so matching on identity alone
    // takes the other scope's card instead.
    overlay.comments.retain(|c| c.commit != comment.commit || c.thread.id != comment.thread.id);
    overlay.comments.push(comment);
    overlay.recompute_comment_counts();
    app.needs_redraw = true;
}

/// Map a diff line's kind to the review side its line number lives on:
/// removed lines are the old side, added / context lines the new side.
fn anchor_side(kind: DiffLineKind) -> ReviewSide {
    match kind {
        DiffLineKind::Removed => ReviewSide::Old,
        DiffLineKind::Added | DiffLineKind::Context => ReviewSide::New,
    }
}

/// Build (or update) a durable [`ReviewThread`] for a review comment.
/// Reuses `prior`'s id / status / timestamps and comment chain:
/// `edit_turn = Some(idx)` rewrites that turn in place (only a
/// `User`-authored turn in range; an agent turn or out-of-range index
/// is left untouched), and `edit_turn = None` appends a new user turn
/// as a reply. Mints a fresh Open thread when there is no prior; the
/// caller stamps the scope `commit`. The store stamps `created_at` /
/// `updated_at` and any empty comment `at` on write, so they start
/// empty here.
fn build_thread(
    prior: Option<forge_primitives::ReviewThread>,
    anchor: ReviewAnchor,
    text: &str,
    edit_turn: Option<usize>,
) -> forge_primitives::ReviewThread {
    match prior {
        Some(mut thread) => {
            thread.anchor = anchor;
            match edit_turn {
                // Rewrite the targeted turn in place; an agent turn or an
                // out-of-range index is rejected - warn so the dropped text
                // is observable (unreachable via the UI, which only offers
                // your own turns as edit targets).
                Some(idx) => {
                    let turn_count = thread.comments.len();
                    let editable = thread
                        .comments
                        .get(idx)
                        .is_some_and(|c| matches!(c.author, ReviewAuthor::User));
                    if editable {
                        if let Some(turn) = thread.comments.get_mut(idx) {
                            text.clone_into(&mut turn.text);
                        }
                    } else {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "diff_overlay_edit_turn_rejected",
                            message = "edit targeted a non-editable turn (agent or out-of-range) - text dropped",
                            outcome = "skipped",
                            turn_idx = idx,
                            turn_count,
                            lost_chars = text.chars().count(),
                        );
                    }
                }
                // Reply: append the user's text as a new turn.
                None => thread.comments.push(ReviewComment {
                    author: ReviewAuthor::User,
                    text: text.to_owned(),
                    at: String::new(),
                    review_id: None,
                }),
            }
            thread
        }
        None => forge_primitives::ReviewThread {
            id: uuid::Uuid::new_v4().to_string(),
            anchor,
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: String::new(),
            updated_at: String::new(),
            commit: None,
        },
    }
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

/// Narrowest gutter, so single-digit line numbers don't shift the
/// marker column relative to two-digit ones in the same hunk.
const SPLIT_GUTTER_MIN: usize = 2;
/// Widest gutter. Beyond this the gutter is under-reserved and the
/// row's divider shifts right of where `split_layout` puts it.
const SPLIT_GUTTER_MAX: usize = 6;

/// Gutter width for a file's split / unified line numbers.
pub(crate) fn gutter_width_for(file: &FileHunks) -> usize {
    let max_line = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter_map(|l| l.new_line.or(l.old_line))
        .max()
        .unwrap_or(1);
    max_line.to_string().len().clamp(SPLIT_GUTTER_MIN, SPLIT_GUTTER_MAX)
}

/// Minimum body width (in columns) for the side-by-side split view -
/// two columns of readable code need the room. Unified renders at any
/// width (it soft-wraps), so below this the split toggle silently
/// falls back to unified rather than blocking the overlay.
pub(crate) const MIN_WIDTH_FOR_SPLIT: u16 = 100;

/// The layout actually painted: the stored choice, except a body
/// narrower than [`MIN_WIDTH_FOR_SPLIT`] forces unified. The stored
/// `view_mode` is untouched, so widening the pane restores split.
/// The hit-test reads this too, so a click resolves against the
/// layout on screen rather than the one the user last asked for.
pub(crate) fn effective_view_mode(stored: DiffViewMode, pane_width: u16) -> DiffViewMode {
    if pane_width < MIN_WIDTH_FOR_SPLIT { DiffViewMode::Unified } else { stored }
}

/// Leading indent before the left column of a split row.
const SPLIT_INDENT_COLS: usize = 2;
/// Space, `│`, space between the two columns.
const SPLIT_DIVIDER_COLS: usize = 3;
/// Space, marker, space between a column's gutter and its text.
pub(crate) const SPLIT_MARKER_COLS: usize = 3;

/// Column geometry of one split-view row.
pub(crate) struct SplitLayout {
    pub left_width: usize,
    pub right_width: usize,
    /// Pane-local column the `│` is painted in.
    pub divider_col: usize,
}

/// Split-row geometry, shared by the renderer and the click handler.
///
/// Each half's gutter and marker are reserved before the text columns
/// split what is left, so a row fits the pane and the divider sits on
/// the midpoint at even pane widths, one column right of it at odd
/// ones. Widening the gutter narrows both columns rather than moving
/// the divider.
pub(crate) fn split_layout(gutter_width: usize, pane_width: u16) -> SplitLayout {
    // Both halves carry a gutter and a marker zone of their own, so
    // those come out of the budget before the text columns split what
    // is left.
    let per_half_chrome = gutter_width.saturating_add(SPLIT_MARKER_COLS);
    let usable = usize::from(pane_width)
        .saturating_sub(SPLIT_INDENT_COLS)
        .saturating_sub(SPLIT_DIVIDER_COLS)
        .saturating_sub(per_half_chrome.saturating_mul(2));
    let left_width = usable / 2;
    SplitLayout {
        left_width,
        right_width: usable - left_width,
        divider_col: SPLIT_INDENT_COLS + gutter_width + SPLIT_MARKER_COLS + left_width + 1,
    }
}

/// Outcome of a mouse interaction. Some interactions need access
/// to the full App (key event needs to fire `dispatch_prompt` for
/// the Esc submit path) which the inner `handle_*` borrow doesn't
/// have - surface them as effects the outer `handle_mouse` runs.
#[derive(Debug, Default)]
struct MouseEffect {
    redraw: bool,
    /// A comment-button click: run `action` on the thread at this key.
    /// Surfaced to the outer handler because persisting the status needs
    /// the App's workspace, which the inner overlay borrow can't reach.
    thread_action: Option<(CommentRef, ThreadAction)>,
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
    // Finish-review modal: only its `[ Submit review ]` button is
    // clickable; every other click / scroll is swallowed so the diff
    // behind it can't be driven while the modal is up.
    if app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            let hit = app.diff_overlay.as_ref().and_then(|o| o.finish_submit_span).is_some_and(
                |(r, c0, c1)| mouse.row == r && mouse.column >= c0 && mouse.column < c1,
            );
            if hit {
                submit_finish_review(app);
            }
        }
        return;
    }
    // Reviews list open: any click closes it (click-away), matching the
    // jump dropdown; row selection stays keyboard-driven.
    if app.diff_overlay.as_ref().is_some_and(|o| o.reviews_open) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            toggle_reviews_list(app);
        }
        return;
    }
    // The diff renders inside the page border, so its content width is
    // the frame minus the two border columns.
    let content_width = app.cached_frame_area.width.saturating_sub(2);
    let effect = if let Some(overlay) = app.diff_overlay.as_mut() {
        match mouse.kind {
            MouseEventKind::ScrollUp => handle_scroll(overlay, mouse.column, content_width, false),
            MouseEventKind::ScrollDown => handle_scroll(overlay, mouse.column, content_width, true),
            MouseEventKind::Down(MouseButton::Left) => {
                handle_left_click(overlay, mouse.column, mouse.row, content_width)
            }
            // Drags, other buttons, and horizontal-wheel events have no
            // binding in the overlay.
            _ => MouseEffect::default(),
        }
    } else {
        MouseEffect::default()
    };
    if let Some((at, action)) = effect.thread_action {
        apply_thread_action(app, at, action);
    }
    if effect.redraw {
        app.needs_redraw = true;
    }
}

/// Whether a click column lands in the FILES rail: the rail spans
/// `[content_origin_col, content_origin_col + rail_width)` when shown.
fn column_in_rail(overlay: &DiffOverlayState, column: u16, content_width: u16) -> bool {
    let rail_width = rail_width_for(content_width);
    rail_width > 0
        && column >= overlay.content_origin_col
        && column < overlay.content_origin_col.saturating_add(rail_width)
}

fn handle_scroll(
    overlay: &mut DiffOverlayState,
    column: u16,
    content_width: u16,
    down: bool,
) -> MouseEffect {
    let in_rail = column_in_rail(overlay, column, content_width);
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
    MouseEffect { redraw: true, thread_action: None }
}

/// Resolve a left-click to an action. Returns the effect (redraw +
/// optional close-with-submit). Hits the rail, the narrow-tier
/// arrows, the pane body's banner ✕, a diff line, a chip, or a
/// hunk header in order.
fn handle_left_click(
    overlay: &mut DiffOverlayState,
    column: u16,
    row: u16,
    content_width: u16,
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
        return MouseEffect { redraw: true, thread_action: None };
    }
    // Any other click with the dropdown open closes it (click-away).
    if overlay.jump_open {
        overlay.jump_open = false;
        return MouseEffect { redraw: true, thread_action: None };
    }
    // Rail click: column inside the rail → rail row hit-test.
    if column_in_rail(overlay, column, content_width) {
        return handle_rail_click(overlay, row);
    }
    // Body click: column past rail+separator. Resolve via body_keys.
    // When the rail isn't rendered (terminal narrower than the split
    // threshold), the renderer paints a "too narrow" notice and
    // clears `body_keys` - clicks just no-op.
    handle_body_click(overlay, column, row)
}

fn handle_rail_click(overlay: &mut DiffOverlayState, row: u16) -> MouseEffect {
    // Rows are relative to the rail's top (below the page border and
    // any commit stepper).
    let row = row.saturating_sub(overlay.rail_origin_row);
    // The tree rail mixes directory headers (non-clickable) with
    // file leaves. We resolve the click by walking `rail_keys`
    // (parallel to the rendered rows) at offset `rail_scroll`.
    // The banner / rule / blank rows live at the head of the list
    // and don't scroll - they're at rows 0, 1, 2 relative to the
    // rail's top. The scrollable portion starts at row 3
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
    // Jump the document scroll to this file's first row. `starts` is in
    // file-sub-document space; add the commit-message block height so the
    // target lands in full-document space and the file actually pins in
    // commit mode (message_rows is 0 in whole-diff mode). Closing the
    // active editor on rail interaction preserves a reopened chip's prior.
    let file_start = overlay.doc_offsets().starts.get(file_idx).copied().unwrap_or(0);
    overlay.doc_scroll = overlay.message_rows.saturating_add(file_start);
    close_active_input_preserving_prior(overlay);
    MouseEffect { redraw: true, thread_action: None }
}

/// Resolve a left-click in the diff body to the row it landed on,
/// and for a split row to the old or new side of it.
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
            // line. Split picks by the painted divider. An empty
            // picked side (blank half of an unbalanced row) is a no-op.
            let key = match effective_view_mode(overlay.view_mode, overlay.pane_width) {
                DiffViewMode::Unified => left.or(right),
                DiffViewMode::Split => {
                    // Guards a body mutated mid-click, paralleling
                    // `save_active_input`'s out-of-bounds arm. The gutter feeds
                    // the column widths; the divider does not depend on it.
                    let Some(file) = left.or(right).and_then(|key| overlay.files.get(key.file_idx))
                    else {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "diff_overlay_click_oob_file_idx",
                            message = "split click hit oob file_idx - body mutated mid-click?",
                            outcome = "skipped",
                            file_count = overlay.files.len(),
                        );
                        return MouseEffect::default();
                    };
                    let pane_local_col =
                        usize::from(column.saturating_sub(overlay.pane_origin_col));
                    let divider =
                        split_layout(gutter_width_for(file), overlay.pane_width).divider_col;
                    if pane_local_col < divider { left } else { right }
                }
            };
            match key {
                Some(key) => open_input_for_key(overlay, key),
                None => MouseEffect::default(),
            }
        }
        BodyRowKey::CommentTurn { at, turn_idx } => {
            reopen_comment_for_turn(overlay, at, Some(turn_idx))
        }
        BodyRowKey::CommentReply { at } => reopen_comment_for_turn(overlay, at, None),
        BodyRowKey::CommentCollapsed { at } => {
            let toggled = overlay.toggle_comment_collapse(at);
            MouseEffect { redraw: toggled, thread_action: None }
        }
        BodyRowKey::CommentButton { at, resolve, reopen } => {
            // Route to whichever applicable button the click lands in; a
            // click on the padding or a dim (inapplicable) action no-ops.
            let pane_col = column.saturating_sub(overlay.pane_origin_col);
            let hits = |span: Option<(u16, u16)>| {
                span.is_some_and(|(start, end)| pane_col >= start && pane_col < end)
            };
            if hits(resolve) {
                MouseEffect { redraw: true, thread_action: Some((at, ThreadAction::Resolve)) }
            } else if hits(reopen) {
                MouseEffect { redraw: true, thread_action: Some((at, ThreadAction::Reopen)) }
            } else {
                MouseEffect::default()
            }
        }
        BodyRowKey::FileHeader { file_idx } | BodyRowKey::DeletedCollapsed { file_idx } => {
            toggle_deleted_collapse(overlay, file_idx)
        }
        BodyRowKey::ContextExpander { file_idx } => expand_context(overlay, file_idx),
        BodyRowKey::EmptyState
        | BodyRowKey::CommentChip(_)
        | BodyRowKey::HunkHeader { .. }
        | BodyRowKey::InputRow(_)
        | BodyRowKey::CommitMessage
        | BodyRowKey::FileEndCap { .. } => MouseEffect::default(),
    }
}

/// Handle a context-expander click: reveal more of the file's pinned
/// wide snapshot in memory (no `git`). Bumps the file's shown-context
/// level and re-narrows from the cached wide hunks.
fn expand_context(overlay: &mut DiffOverlayState, file_idx: usize) -> MouseEffect {
    overlay.expand_file_context(file_idx);
    MouseEffect { redraw: true, thread_action: None }
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
    MouseEffect { redraw: true, thread_action: None }
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
    let editor = InputState::new();
    overlay.active_input =
        Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
    MouseEffect { redraw: true, thread_action: None }
}

/// Reopen the saved comment at `key` for either a turn rewrite
/// (`edit_turn = Some(idx)` seeds the editor with that turn's text) or
/// a reply (`edit_turn = None` opens an empty editor that appends on
/// save). The saved entry is dropped so its chip vanishes WHILE editing
/// but stashed on `prior_comment` so Esc-cancel restores it - losing
/// review notes to a misclick-and-reflex-Esc would destroy the user's
/// work.
fn reopen_comment_for_turn(
    overlay: &mut DiffOverlayState,
    at: CommentRef,
    edit_turn: Option<usize>,
) -> MouseEffect {
    let Some(pos) = overlay.comment_index_at(at) else {
        return MouseEffect::default();
    };
    let comment = overlay.comments.remove(pos);
    overlay.recompute_comment_counts();
    // Close any pre-existing editor on a different line so its
    // prior_comment survives (without this, A's prior would be
    // silently dropped when B's reopen runs).
    close_active_input_preserving_prior(overlay);
    let mut editor = InputState::new();
    // Seed the editor with the targeted turn's text (rewrite); a reply
    // starts empty. `insert_str` respects newlines so a saved
    // turn's multi-line shape is preserved.
    if let Some(idx) = edit_turn
        && let Some(turn) = comment.thread.comments.get(idx)
    {
        editor.insert_str(&turn.text);
    }
    overlay.active_input = Some(ActiveCommentInput {
        key: comment.key,
        editor,
        prior_comment: Some(comment),
        edit_turn,
    });
    MouseEffect { redraw: true, thread_action: None }
}

/// Whether a comment is actionable on submit: authored or edited THIS
/// session (a hydrated thread from a prior review isn't re-nudged) and not
/// already Resolved / Outdated. A review with at least one such comment
/// nudges the agent on submit.
fn is_actionable(comment: &HunkComment) -> bool {
    comment.authored_this_session
        && !matches!(comment.thread.status, ReviewStatus::Resolved | ReviewStatus::Outdated)
}

/// Close path for the overlay (banner ✕ click and `handle_key`'s Esc).
/// Opens the Finish-review modal only when at least one comment WOULD file
/// into a new review - authored this session AND carrying a user turn no
/// review has sealed. A reply on a thread already filed into an earlier
/// review counts: the conversation moved on and the new turn needs a round
/// of its own. An edit-only session (every authored turn already sealed)
/// and a look-only session both skip the modal and take the plain close
/// path: neither mints a review nor nudges the agent (edits are already
/// persisted; the agent reads them via the review MCP).
pub(super) fn close_with_submit(app: &mut App) {
    // Flush the active editor first - a reopened chip parks its saved
    // comment on `active_input.prior_comment`, so `overlay.comments` is
    // incomplete while the editor is open; the helper restores it.
    if let Some(o) = app.diff_overlay.as_mut() {
        let _ = close_active_input_preserving_prior(o);
    }
    let would_file = app.diff_overlay.as_ref().is_some_and(|o| {
        o.comments.iter().any(|c| c.authored_this_session && c.thread.has_unfiled_user_turn())
    });
    if would_file {
        if let Some(o) = app.diff_overlay.as_mut() {
            o.finish_review = Some(FinishReviewState { editor: InputState::new() });
            app.needs_redraw = true;
        }
        return;
    }
    finalize_review_close(app, None, &[]);
}

/// Submit the Finish-review modal: seal this session's authored comments
/// into a fresh numbered review (with the optional overview), then nudge
/// the agent to address it via the review MCP and close.
pub(super) fn submit_finish_review(app: &mut App) {
    let overview =
        app.diff_overlay.as_ref().and_then(|o| o.finish_review.as_ref()).map(|f| f.editor.text());
    let overview = overview.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    let seal_ids: Vec<String> = app.diff_overlay.as_ref().map_or_else(Vec::new, |o| {
        o.comments.iter().filter(|c| c.authored_this_session).map(|c| c.thread.id.clone()).collect()
    });
    finalize_review_close(app, overview.as_deref(), &seal_ids);
}

/// Shared tail for the Finish-review submit and the plain close: hold when
/// the agent isn't ready (surface stays open so notes survive), else seal
/// the listed still-unfiled threads into a numbered review (skipped when
/// `seal_ids` is empty - the edit-only / look-only path mints nothing) and
/// nudge the agent to address it through the review MCP. The overview is
/// stored on the review, never put in the chat (the agent reads it, and the
/// comments, via `review__get`). Sealing is best-effort: a session without
/// a branch / store still closes; only the local reviews-list record and
/// the nudge are lost.
fn finalize_review_close(app: &mut App, overview: Option<&str>, seal_ids: &[String]) {
    let pending = app.diff_overlay.as_ref().is_some_and(|o| o.comments.iter().any(is_actionable));
    if pending && (!app.has_active_agent() || app.session_id().is_none()) {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_close_held_no_agent",
            message = "diff review close held: agent not ready, comments preserved",
            outcome = "held",
            has_agent = app.has_active_agent(),
            has_session_id = app.session_id().is_some(),
        );
        crate::app::slash::push_system_message(
            app,
            "Review submit held: agent not ready. Wait for the session to connect, then Esc again to submit.",
        );
        app.needs_redraw = true;
        return;
    }

    // The reviewer's session is the notice target when a worker later
    // addresses this review; record it as the submit origin.
    let origin = app
        .active_session_key
        .clone()
        .unwrap_or_else(|| forge_workspace::SessionKey::from_session_id(String::new()));
    let project = app.active_session().and_then(|s| s.project.clone());
    let branch = app.diff_overlay.as_ref().and_then(|o| o.branch.clone());
    let workspace = app.workspace.clone();
    let review_number = if seal_ids.is_empty() {
        None
    } else if let (Some(project), Some(branch), Some(workspace)) = (&project, &branch, &workspace) {
        let review =
            workspace.submit_review(project, branch, overview.map(str::to_owned), seal_ids, origin);
        if review.is_none() {
            // The store write failed. Comments are already persisted unfiled
            // at save-time; only the local reviews-list record and the agent
            // nudge are lost. Degrade like the comment-save path: warn, close.
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_review_not_sealed",
                message = "diff review could not be sealed locally",
                outcome = "degraded",
            );
            crate::app::slash::push_system_message(
                app,
                "Review couldn't be saved locally (store error) - it won't show in the reviews list or reach the agent.",
            );
        }
        review.map(|r| r.number)
    } else {
        // There are comments to file but no (project, branch, workspace) to
        // file them under. They can't persist or reach the agent, so warn
        // like the store-fail path rather than dropping them silently (the
        // pre-nudge bundle dispatched regardless of branch). Name the step
        // that came up empty: the three collapse to very different fixes,
        // and only the middle one is about HEAD.
        let missing = if project.is_none() {
            "this session is not under a forge project"
        } else if branch.is_none() {
            "the checkout has no branch name - a detached HEAD, or git could not read it (the log carries which)"
        } else {
            "forge is shutting down"
        };
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_review_scope_unresolved",
            message = "diff review not sealed: incomplete scope",
            outcome = "dropped",
            has_project = project.is_some(),
            has_branch = branch.is_some(),
            has_workspace = workspace.is_some(),
        );
        crate::app::slash::push_system_message(
            app,
            format!(
                "Can't file a review here: {missing} - comments won't persist or reach the agent."
            ),
        );
        None
    };

    // A freshly-sealed review with something to act on nudges the agent to
    // read + address it via the review MCP - one line, not the comments.
    if pending && let Some(number) = review_number {
        super::input_submit::dispatch_review_nudge(
            app,
            format!("Review #{number} ready - address it via the review MCP (`review__list`)."),
        );
    }
    close(app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_workspace::env::git_diff::hunks::FileStatus;
    use std::time::Duration;

    fn sample_state() -> DiffOverlayState {
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![
                FileHunks {
                    path: "a.rs".into(),
                    status: FileStatus::Modified,
                    hunks: vec![],
                    oversize: false,
                },
                FileHunks {
                    path: "b.rs".into(),
                    status: FileStatus::Added,
                    hunks: vec![],
                    oversize: false,
                },
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
        // Left half: well clear of the divider at pane-local 65.
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
        // Right half: past the divider at pane-local 65.
        let effect = handle_left_click(&mut state, 120, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(right_key));
    }

    #[test]
    fn effective_view_mode_forces_unified_below_split_threshold() {
        assert_eq!(effective_view_mode(DiffViewMode::Split, 80), DiffViewMode::Unified);
        assert_eq!(
            effective_view_mode(DiffViewMode::Split, MIN_WIDTH_FOR_SPLIT),
            DiffViewMode::Split,
        );
        assert_eq!(effective_view_mode(DiffViewMode::Unified, 200), DiffViewMode::Unified);
    }

    /// At an odd pane width the divider sits a column right of the
    /// midpoint, so a click between the two is visually on the old side.
    #[test]
    fn body_click_just_left_of_the_divider_resolves_the_old_side() {
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
        // Pane-local 59 is the midpoint; the divider is at 60.
        let effect = handle_left_click(&mut state, 41 + 59, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(left_key));

        // The divider cell itself is ambiguous; it goes to the new side.
        state.active_input = None;
        let effect = handle_left_click(&mut state, 41 + 60, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(right_key));
    }

    /// Each half's gutter comes out of its own text budget, so widening
    /// the gutter narrows both columns and leaves the divider where it
    /// was. A hit-test that assumed otherwise would drift per file.
    #[test]
    fn the_divider_does_not_move_with_gutter_width() {
        for pane_width in [101u16, 119, 160, 184] {
            let narrow = split_layout(SPLIT_GUTTER_MIN, pane_width);
            let wide = split_layout(SPLIT_GUTTER_MAX, pane_width);
            assert_eq!(narrow.divider_col, wide.divider_col, "pane_width={pane_width}");
            // Every gutter, not just the two a line-count fixture can reach.
            for gutter in SPLIT_GUTTER_MIN..=SPLIT_GUTTER_MAX {
                let layout = split_layout(gutter, pane_width);
                let row = SPLIT_INDENT_COLS
                    + (gutter + SPLIT_MARKER_COLS + layout.left_width)
                    + SPLIT_DIVIDER_COLS
                    + (gutter + SPLIT_MARKER_COLS + layout.right_width);
                assert_eq!(row, usize::from(pane_width), "gutter={gutter} pane={pane_width}");
            }
            // The wider gutter is paid for out of the text columns.
            assert!(
                wide.left_width < narrow.left_width,
                "pane_width={pane_width} {} {}",
                wide.left_width,
                narrow.left_width
            );
        }
    }

    /// Below `MIN_WIDTH_FOR_SPLIT` the renderer paints unified rows,
    /// which carry one side only. Resolving those as split returns the
    /// blank side and the click silently does nothing.
    #[test]
    fn body_click_in_a_narrow_pane_resolves_the_unified_row() {
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.body_keys = vec![BodyRowKey::HunkRow { left: None, right: Some(key) }];
        state.pane_origin_row = 0;
        state.pane_origin_col = 0;
        state.pane_width = 80;
        let effect = handle_left_click(&mut state, 5, 0, 80);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(key));
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
    fn handle_mouse_hit_tests_the_rail_at_the_inner_content_width() {
        // handle_mouse derives the rail width from the page's INNER width
        // (frame minus the two border columns), so a rail click resolves
        // against the same geometry the renderer stashed. Simulate a
        // rendered 160-wide frame: 158-wide content, rail at column 1.
        let mut state = sample_state(); // 2 files, flat rail_keys
        state.measured_heights = vec![Some(10), Some(4)];
        state.content_origin_col = 1;
        state.rail_origin_row = 1;
        let mut app = App::test_default();
        app.diff_overlay = Some(state);
        app.cached_frame_area = ratatui::layout::Rect::new(0, 0, 160, 40);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,  // inside the rail (spans col 1..1+rail_width)
                row: 1 + 4, // rail top (1) + banner/rule/blank + file 1
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );

        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").doc_scroll,
            10,
            "the inner-width rail hit-test resolves file 1",
        );
    }

    #[test]
    fn body_click_on_deleted_header_toggles_expand() {
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![FileHunks {
                path: "gone.rs".into(),
                status: FileStatus::Deleted,
                hunks: vec![],
                oversize: false,
            }],
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
    fn body_click_on_your_turn_reopens_that_turn() {
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key,
            path: "a.rs".into(),
            line: 7,
            comment_text: "needs unwrap fix".into(),
            commit: None,
            thread: user_thread("needs unwrap fix"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(key), right: Some(key) },
            BodyRowKey::CommentTurn { at: CommentRef { line: key, slot: 0 }, turn_idx: 0 },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 3, 160);
        assert!(effect.redraw);
        assert!(state.comments.is_empty(), "saved comment migrates back into the editor");
        let input = state.active_input.expect("editor reopened");
        assert_eq!(input.key, key);
        assert_eq!(input.edit_turn, Some(0), "the clicked turn is the edit target");
        assert_eq!(input.editor.lines().join("\n"), "needs unwrap fix");
    }

    #[test]
    fn recompute_comment_counts_zeroes_then_tallies() {
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            comment_text: "x".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 1, line_idx: 0 },
            path: "a.rs".into(),
            line: 2,
            comment_text: "y".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 1, hunk_idx: 0, line_idx: 0 },
            path: "b.rs".into(),
            line: 1,
            comment_text: "z".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
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
    fn close_with_submit_opens_finish_review_when_authored() {
        // A session that authored a comment opens the Finish-review modal
        // on close instead of closing - the pass seals into a review on
        // exit. Agent-agnostic: the modal opens whether or not the agent
        // is ready (the send happens on submit).
        let mut app = App::test_default();
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            comment_text: "needs unwrap fix".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        let overlay = app.diff_overlay.as_ref().expect("overlay stays open");
        assert!(overlay.finish_review.is_some(), "the Finish-review modal opened");
        assert_eq!(overlay.comments.len(), 1, "the authored comment is preserved");
        assert_eq!(app.active_view, ActiveView::Diff, "view stays on Diff");
    }

    #[test]
    fn close_with_submit_closes_directly_when_look_only() {
        // A look-only session (only hydrated comments, nothing authored
        // this session) closes straight through - no modal, no re-send.
        let mut app = App::test_default();
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            comment_text: "hydrated from a prior review".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "look-only close drops the overlay");
        assert_eq!(app.active_view, ActiveView::Chat, "view returns to chat");
    }

    #[test]
    fn close_with_submit_edit_only_no_ops() {
        // A session that only edits an already-filed comment must NOT mint a
        // review (the modal never opens) and must NOT dispatch anything: the
        // edit is already persisted, and the agent reads it via the review
        // MCP - there is nothing to nudge.
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        let mut thread = filed_thread("rev");
        thread.id = "filed".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "tweaked note".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        close_with_submit(&mut app);
        assert!(app.diff_overlay.is_none(), "edit-only close skips the modal and closes");
        assert!(rx.try_recv().is_err(), "an edit-only close dispatches nothing to the agent");
        assert!(
            ws.load_reviews("forge", "feat").expect("load").is_empty(),
            "no review was minted for an edit-only session",
        );
    }

    /// A review conversation spans several rounds: the reviewer comments,
    /// the agent answers, the reviewer answers back. That second reply is a
    /// new unfiled turn, so Esc must offer the modal and Submit must seal a
    /// second review the thread also belongs to.
    #[test]
    fn a_reply_on_a_filed_thread_seals_into_a_second_review() {
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let origin = forge_workspace::SessionKey::from_session_id("review-session");

        let mut thread = user_thread("does this handle the empty case?");
        thread.id = "t1".to_owned();
        ws.save_review_threads("forge", "feat", &[thread]);
        let r1 = ws
            .submit_review("forge", "feat", None, &["t1".to_owned()], origin.clone())
            .expect("first review sealed");

        // The agent answers, which flips the thread to Addressed.
        let status = ws
            .review_reply(&origin, "forge", "feat", "t1", "implementer", "fixed in b3f1", "")
            .expect("agent reply");
        assert_eq!(status, ReviewStatus::Addressed);

        // The reviewer answers back on the already-filed thread.
        let mut replied =
            ws.load_review_threads("forge", "feat").expect("load").pop().expect("thread");
        replied.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "the empty case is still unguarded".to_owned(),
            at: String::new(),
            review_id: None,
        });
        assert!(ws.upsert_review_thread("forge", "feat", replied.clone()), "reply persisted");

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.reviews = ws.load_reviews("forge", "feat").expect("load reviews");
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "does this handle the empty case?".into(),
            commit: None,
            thread: replied,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "a reply on a filed thread is unfiled work, so the modal opens",
        );
        submit_finish_review(&mut app);

        let reviews = ws.load_reviews("forge", "feat").expect("load reviews");
        assert_eq!(reviews.len(), 2, "the reply sealed a second review");
        let r2 = &reviews[1];
        assert_eq!(r2.number, 2);

        let stored = ws
            .load_review_threads("forge", "feat")
            .expect("load")
            .into_iter()
            .find(|t| t.id == "t1")
            .expect("thread");
        assert!(stored.is_in_review(&r1.id), "the thread stays in the first review");
        assert!(stored.is_in_review(&r2.id), "and now also belongs to the second");
        assert_eq!(
            stored.comments.iter().map(|c| c.review_id.as_deref()).collect::<Vec<_>>(),
            vec![Some(r1.id.as_str()), None, Some(r2.id.as_str())],
            "each turn carries the review that sealed it; the agent reply carries none",
        );
        assert_eq!(
            stored.status,
            ReviewStatus::Open,
            "the agent owes another answer, so sealing reopens the thread",
        );
        assert!(rx.try_recv().is_ok(), "the second review nudges the agent");
    }

    #[test]
    fn submit_finish_review_files_a_resolved_comment_without_dispatch() {
        // An authored NEW comment resolved before close still trips
        // would_file (it's unfiled), so the modal opens and Submit mints a
        // review filing the Resolved comment - but a resolved comment isn't
        // actionable, so no nudge is dispatched to the agent.
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let mut seeded = stock_thread();
        seeded.id = "r".to_owned();
        seeded.status = ReviewStatus::Resolved;
        ws.save_review_threads("forge", "feat", &[seeded.clone()]);

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "resolved before close".into(),
            commit: None,
            thread: seeded,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "an unfiled authored comment opens the modal even when resolved",
        );
        submit_finish_review(&mut app);

        assert!(app.diff_overlay.is_none(), "overlay closed on submit");
        let reviews = ws.load_reviews("forge", "feat").expect("load reviews");
        assert_eq!(reviews.len(), 1, "a review was minted");
        let filed = ws
            .load_review_threads("forge", "feat")
            .expect("load")
            .into_iter()
            .find(|t| t.id == "r")
            .expect("thread")
            .origin_review()
            .map(str::to_owned);
        assert_eq!(
            filed,
            Some(reviews[0].id.clone()),
            "the resolved comment filed into the review"
        );
        assert!(rx.try_recv().is_err(), "a resolved comment is not dispatched to the agent");
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
            comment_text: "I want to keep this".into(),
            commit: None,
            thread: user_thread("I want to keep this"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.recompute_comment_counts();
        state.body_keys =
            vec![BodyRowKey::CommentTurn { at: CommentRef { line: key, slot: 0 }, turn_idx: 0 }];
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
            comment_text: "saved".into(),
            commit: None,
            thread: user_thread("saved"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.recompute_comment_counts();
        // Body geometry: your-turn row at idx 0, hunk header at idx 1,
        // hunk line at idx 2.
        let key_b = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::CommentTurn { at: CommentRef { line: key_a, slot: 0 }, turn_idx: 0 },
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
            comment_text: "A".into(),
            commit: None,
            thread: user_thread("A"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.comments.push(HunkComment {
            key: key_b,
            path: "a.rs".into(),
            line: 5,
            comment_text: "B".into(),
            commit: None,
            thread: user_thread("B"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.recompute_comment_counts();
        state.body_keys = vec![
            BodyRowKey::CommentTurn { at: CommentRef { line: key_a, slot: 0 }, turn_idx: 0 },
            BodyRowKey::CommentTurn { at: CommentRef { line: key_b, slot: 0 }, turn_idx: 0 },
        ];
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
            comment_text: "A".into(),
            commit: None,
            thread: user_thread("A"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.recompute_comment_counts();
        state.body_keys =
            vec![BodyRowKey::CommentTurn { at: CommentRef { line: key_a, slot: 0 }, turn_idx: 0 }];
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
            comment_text: "original text".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        let mut editor = InputState::new();
        editor.insert_str("original text with user-typed edits");
        state.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior.clone()),
            edit_turn: Some(0),
        });
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
            comment_text: "exactly this".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        let mut editor = InputState::new();
        editor.insert_str("exactly this");
        state.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior),
            edit_turn: Some(0),
        });
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
        let mut editor = InputState::new();
        editor.insert_str("draft typed by user");
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
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
        state.active_input = Some(ActiveCommentInput {
            key,
            editor: InputState::new(),
            prior_comment: None,
            edit_turn: None,
        });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay still set");
        assert!(after.active_input.is_none(), "editor closed");
        assert!(after.comments.is_empty(), "no blank chip created");
    }

    /// Diff view with a comment editor opened the way a line click does,
    /// anchored on a real diff line so a save can resolve its anchor.
    fn app_with_comment_editor() -> App {
        let mut app = App::test_default();
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])],
        );
        let _ = open_input_for_key(&mut state, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 });
        app.diff_overlay = Some(state);
        crate::app::view::set_active_view(&mut app, ActiveView::Diff);
        app
    }

    /// Put the burst detector into the state mid-dictation produces:
    /// three characters at machine speed.
    fn start_dictation_burst(app: &mut App, base: std::time::Instant) {
        for (offset, ch) in [(0_u64, 'f'), (2, 'i'), (4, 'x')] {
            let _ = app.paste_burst.on_char(ch, base + Duration::from_millis(offset));
        }
        assert!(app.paste_burst.is_buffering(), "three machine-speed chars form a burst");
    }

    /// speech-to-text dictation arrives as individual keystrokes including
    /// Enter for sentence breaks. An Enter mid-burst used to hit
    /// `save_active_input`, closing the editor so the REST of the dictated
    /// sentence landed on the diff view's single-letter shortcuts - `t`
    /// toggling the view mode, `j` opening the jump menu, and so on.
    #[test]
    fn enter_during_a_dictation_burst_does_not_save_the_comment() {
        let mut app = app_with_comment_editor();
        start_dictation_burst(&mut app, Instant::now());

        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        let after = overlay(&app);
        assert!(after.active_input.is_some(), "the editor must stay open mid-burst");
        assert!(after.comments.is_empty(), "nothing is saved mid-burst");
    }

    /// The sister case: with no burst in flight, Enter still means save.
    /// This is the per-site semantic that must NOT be shared away.
    #[test]
    fn plain_enter_still_saves_the_comment() {
        let mut app = app_with_comment_editor();
        if let Some(input) = app.diff_overlay.as_mut().and_then(|o| o.active_input.as_mut()) {
            input.editor.insert_str("a real comment");
        }

        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        let after = overlay(&app);
        assert!(after.active_input.is_none(), "plain Enter closes the editor");
        assert_eq!(after.comments.len(), 1, "plain Enter saves the comment");
    }

    /// Typed characters have to reach the editor through the burst
    /// detector, so a dictated run coalesces instead of arriving as
    /// individual keystrokes.
    #[test]
    fn typed_characters_feed_the_burst_detector() {
        let mut app = app_with_comment_editor();

        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('b')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));

        assert!(
            app.paste_burst.is_buffering(),
            "three characters at test speed must register as a burst, not three inserts"
        );
    }

    /// Type `token` one character at a time at human speed. Consecutive
    /// test statements land microseconds apart, which the burst detector
    /// correctly reads as a paste; clearing its timing reference between
    /// keys is what "the user is typing" looks like to it.
    fn type_text(app: &mut App, token: &str) {
        for ch in token.chars() {
            app.paste_burst.on_non_char_key(Instant::now());
            handle_key(app, KeyEvent::from(KeyCode::Char(ch)));
        }
    }

    #[test]
    fn typing_a_shortcode_opens_the_picker_in_the_comment_editor() {
        let mut app = app_with_comment_editor();

        type_text(&mut app, ":roc");

        let state = app.emoji.as_ref().expect("picker open");
        assert_eq!(state.query, "roc");
        assert!(state.candidates.iter().any(|e| e.name == "rocket"));
    }

    /// The bite: in the /diff overlay Esc already means "finish review".
    /// With the picker open it must dismiss the PICKER and go no further,
    /// or typing `:` then Esc submits a review.
    #[test]
    fn esc_with_the_picker_open_dismisses_only_the_picker() {
        let mut app = app_with_comment_editor();
        type_text(&mut app, ":roc");

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

        assert!(app.emoji.is_none(), "Esc closes the picker");
        let after = overlay(&app);
        assert!(after.active_input.is_some(), "the comment editor stays open");
        assert!(after.finish_review.is_none(), "Esc must not reach finish-review");
        assert_eq!(app.active_view, ActiveView::Diff, "the overlay stays open");
    }

    /// A second Esc, with no picker in the way, resumes the normal
    /// meaning - cancel the editor.
    #[test]
    fn esc_after_dismissing_the_picker_cancels_the_editor() {
        let mut app = app_with_comment_editor();
        type_text(&mut app, ":roc");
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

        assert!(overlay(&app).active_input.is_none(), "the editor is cancelled");
    }

    /// Enter belongs to the picker while it is open, so it must not save
    /// the comment out from under a half-typed shortcode.
    #[test]
    fn enter_with_the_picker_open_inserts_the_emoji_and_keeps_editing() {
        let mut app = app_with_comment_editor();
        type_text(&mut app, ":rocket");

        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        assert!(app.emoji.is_none(), "the picker closes on confirm");
        let after = overlay(&app);
        assert!(after.active_input.is_some(), "the editor stays open so typing continues");
        assert!(after.comments.is_empty(), "Enter on the picker is not a save");
        let text = after.active_input.as_ref().expect("editor").editor.text();
        assert_eq!(text, "\u{1F680}", "the whole :rocket token became the glyph");
    }

    #[test]
    fn typing_the_closing_colon_lands_the_glyph() {
        let mut app = app_with_comment_editor();

        type_text(&mut app, ":tada:");

        assert!(app.emoji.is_none());
        let text = overlay(&app).active_input.as_ref().expect("editor").editor.text();
        assert_eq!(text, "\u{1F389}");
    }

    /// A URL in a review comment must not pop a picker.
    #[test]
    fn a_url_does_not_open_the_picker() {
        let mut app = app_with_comment_editor();

        type_text(&mut app, "see http://x.dev");

        assert!(app.emoji.is_none(), "`:` mid-word is not a trigger");
        let text = overlay(&app).active_input.as_ref().expect("editor").editor.text();
        assert_eq!(text, "see http://x.dev");
    }

    /// The picker has to work in the Finish-review overview too - that is
    /// the whole point of hanging it off the shared substrate.
    #[test]
    fn the_picker_works_in_the_finish_review_overview() {
        let mut app = app_with_comment_editor();
        if let Some(o) = app.diff_overlay.as_mut() {
            o.active_input = None;
            o.finish_review = Some(FinishReviewState { editor: InputState::new() });
        }

        type_text(&mut app, ":rocket");
        assert!(app.emoji.is_some(), "picker opens over the modal");
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        let after = overlay(&app);
        assert!(after.finish_review.is_some(), "Enter on the picker does not submit the review");
        let text = after.finish_review.as_ref().expect("modal").editor.text();
        assert_eq!(text, "\u{1F680}");
    }

    #[test]
    fn save_empty_reopened_chip_deletes_saved_comment() {
        // Clearing the only user turn + Enter removes the whole card.
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "soon-to-be-deleted".into(),
            commit: None,
            thread: user_thread("soon-to-be-deleted"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        state.active_input = Some(ActiveCommentInput {
            key,
            editor: InputState::new(), // empty editor
            prior_comment: Some(prior),
            edit_turn: Some(0),
        });
        app.diff_overlay = Some(state);
        save_active_input(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay still set");
        assert!(after.active_input.is_none());
        assert!(after.comments.is_empty(), "clearing the only user turn deletes the card");
    }

    #[test]
    fn submit_finish_review_degrades_when_the_seal_fails() {
        // When the seal write fails (here a corrupt threads row rolls back
        // the submit txn, so submit_review returns None) there is no review
        // number to nudge with, so submit degrades gracefully: it closes
        // (never holds - a store-down session would dead-end), dispatches no
        // nudge, and pushes a system message so the failure isn't silent.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("ws");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_row_for_test(&db, "forge", "feat")
            .expect("corrupt row");
        workspace.install_db_for_test(db);
        let mut rx = app.install_testing_stub();
        app.set_session_id(Some(crate::agent::model::SessionId::new("review-session")));
        if let Some(key) = app.active_session_key.clone()
            && let Some(session) = app.sessions.get_mut(&key)
        {
            session.project = Some("forge".to_owned());
            session.cwd_raw = "/tmp/repo".into();
        }

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.finish_review = Some(FinishReviewState { editor: InputState::new() });
        let mut thread = stock_thread();
        thread.id = "fresh".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "note".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        submit_finish_review(&mut app);

        assert!(
            app.diff_overlay.is_none(),
            "the overlay closes (no dead-end hold) on a seal failure"
        );
        assert!(
            rx.try_recv().is_err(),
            "a failed seal has no review number, so nothing is nudged to the agent",
        );
        assert!(
            app.messages().iter().any(|m| matches!(m.role, crate::app::MessageRole::System(None))),
            "a system message warns that the review wasn't saved locally",
        );
    }

    #[test]
    fn submit_finish_review_on_detached_head_warns_not_silently_drops() {
        // A detached HEAD leaves `overlay.branch == None`, so the review has
        // no (project, branch) to file under. With pending comments + a ready
        // agent it must NOT silently drop: it closes but pushes a system
        // message so the loss is visible (mirrors the store-fail branch).
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        // No branch set -> detached HEAD.
        overlay.finish_review = Some(FinishReviewState { editor: InputState::new() });
        let mut thread = stock_thread();
        thread.id = "fresh".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "note".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        submit_finish_review(&mut app);

        assert!(app.diff_overlay.is_none(), "the overlay closes (no dead-end hold)");
        assert!(rx.try_recv().is_err(), "no branch to file under, so nothing is dispatched");
        let notice = system_notice_text(&app).expect("a system message warns about the loss");
        assert!(
            notice.contains("branch name"),
            "the notice names the step that came up empty: {notice}",
        );
    }

    /// Every text block of the last System message, for asserting on a
    /// notice's wording rather than only its existence.
    fn system_notice_text(app: &App) -> Option<String> {
        app.messages()
            .iter()
            .rev()
            .find(|m| matches!(m.role, crate::app::MessageRole::System(None)))
            .map(|m| {
                m.blocks
                    .iter()
                    .filter_map(|b| match b {
                        crate::app::MessageBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
    }

    /// The three ways the submit scope comes up empty need three
    /// different fixes, and only the middle one is about HEAD. All three
    /// used to say "no branch - detached HEAD?", the same guess the read
    /// side dropped.
    #[test]
    fn an_unresolved_submit_scope_names_the_step_that_failed_not_head() {
        let (mut app, mut rx, _dir) = review_app_with_agent();
        // Project unset: the session is not under a forge project at all,
        // which has nothing to do with the checkout's HEAD.
        if let Some(key) = app.active_session_key.clone()
            && let Some(session) = app.sessions.get_mut(&key)
        {
            session.project = None;
        }
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.finish_review = Some(FinishReviewState { editor: InputState::new() });
        let mut thread = stock_thread();
        thread.id = "fresh".to_owned();
        overlay.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "src/x.rs".into(),
            line: 1,
            comment_text: "note".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        submit_finish_review(&mut app);

        assert!(rx.try_recv().is_err(), "nothing to file under, so nothing is dispatched");
        let notice = system_notice_text(&app).expect("a system message warns about the loss");
        assert!(notice.contains("forge project"), "the notice names the project step: {notice}");
        assert!(!notice.contains("detached"), "a missing project is not a detached HEAD: {notice}");
    }

    #[test]
    fn submit_finish_review_flushes_reopened_chip_before_seal() {
        // A chip-reopen with an open editor must restore the prior on close
        // so it counts as an actionable comment on submit; without the flush
        // `overlay.comments` is empty while the editor is open, the review
        // seals nothing actionable, and no nudge fires. The nudge dispatched
        // here proves the flush ran.
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let mut state = sample_state();
        state.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "important review note".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        let mut editor = InputState::new();
        editor.insert_str("important review note");
        // Editor open as a chip reopen - prior_comment Some, no
        // unsubmitted comments in overlay.comments yet.
        state.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior.clone()),
            edit_turn: Some(0),
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        // Close flushes the editor (restoring the prior) and opens the
        // modal because the prior is authored this session.
        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "the Finish-review modal opened",
        );
        submit_finish_review(&mut app);
        assert!(app.diff_overlay.is_none(), "overlay closed on submit");
        // The flush restored an actionable comment, so a nudge fired.
        match rx.try_recv().expect("a nudge was dispatched") {
            forge_primitives::AgentCommand::PromptWithImages { text, .. } => {
                assert!(
                    text.contains("Review #1") && text.contains("review__list"),
                    "the nudge points at the sealed review, got: {text}",
                );
            }
            other => panic!("expected PromptWithImages, got {other:?}"),
        }
    }

    #[test]
    fn submit_finish_review_holds_when_agent_not_ready() {
        // Submitting a review with sendable comments but no ready agent
        // must NOT close - it holds (modal stays, comments preserved,
        // nothing dispatched) so the notes survive until the session
        // connects. Mirrors the pre-modal no-agent guard at the new
        // submit layer.
        let mut app = App::test_default();
        // No install_testing_stub → has_active_agent = false.
        let mut state = sample_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            comment_text: "to be preserved".into(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "the modal opened",
        );
        submit_finish_review(&mut app);
        let after = app.diff_overlay.as_ref().expect("overlay held open");
        assert!(after.finish_review.is_some(), "modal stays open on the no-agent hold");
        assert_eq!(after.comments.len(), 1, "the comment is preserved");
        assert_eq!(after.comments[0].comment_text, "to be preserved");
        assert_eq!(app.active_view, ActiveView::Diff, "view stays on Diff");
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
        workspace.seed_test_project("forge", project_root);
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
        FileHunks { path: path.to_owned(), status, hunks: vec![], oversize: false }
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
        let _ = state.select_scope(DiffScope::Commit(2));
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
        let _ = state.select_scope(DiffScope::WholeDiff);
        assert_eq!(state.scope, DiffScope::WholeDiff);
        // Backward from the whole-diff view re-enters the stepper at the
        // first commit (the one WholeDiff → Commit path).
        assert_eq!(state.step_commit(false), Some(NavOutcome::Ready));
        assert_eq!(state.scope, DiffScope::Commit(0));
        assert_eq!(state.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["a.rs"]);
    }

    fn cached_whole_diff() -> CachedScan {
        CachedScan { files: vec![one_file("x.rs", FileStatus::Modified)], scanner_ok: true }
    }

    #[test]
    fn toggle_all_changes_from_commit_remembers_and_returns() {
        let mut state = commit_mode_state();
        state.whole_diff_cache = Some(cached_whole_diff()); // synchronous toggle
        let _ = state.select_scope(DiffScope::Commit(1));
        assert_eq!(state.toggle_all_changes(), Some(NavOutcome::Ready));
        assert_eq!(state.scope, DiffScope::WholeDiff, "toggling off a commit shows all changes");
        assert_eq!(state.last_commit, Some(1), "the commit is remembered");
        assert_eq!(state.toggle_all_changes(), Some(NavOutcome::Ready));
        assert_eq!(state.scope, DiffScope::Commit(1), "toggling back returns to the same commit");
    }

    #[test]
    fn toggle_all_changes_from_whole_diff_without_memory_goes_to_first_commit() {
        let mut state = commit_mode_state();
        state.whole_diff_cache = Some(cached_whole_diff());
        // Reach whole-diff via the jump path (not `a`), so no commit is
        // remembered - the toggle falls back to the first commit.
        let _ = state.select_scope(DiffScope::WholeDiff);
        assert_eq!(state.last_commit, None);
        assert_eq!(state.toggle_all_changes(), Some(NavOutcome::Ready));
        assert_eq!(state.scope, DiffScope::Commit(0), "no memory → first commit");
    }

    #[test]
    fn toggle_all_changes_is_noop_in_whole_diff_only_mode() {
        let mut state = sample_state();
        assert_eq!(state.toggle_all_changes(), None);
        assert_eq!(state.scope, DiffScope::WholeDiff);
    }

    #[test]
    fn comments_accumulate_across_commits_but_count_current_scope() {
        let mut state = commit_mode_state();
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".into(),
            line: 1,
            comment_text: "on first".into(),
            commit: Some("aaa".into()),
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
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
            comment_text: "a".into(),
            commit: Some("aaa".into()),
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.comments.push(HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "b.rs".into(),
            line: 1,
            comment_text: "b".into(),
            commit: Some("bbb".into()),
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        let scoped: Vec<&str> =
            state.scoped_comments().iter().map(|c| c.comment_text.as_str()).collect();
        assert_eq!(scoped, vec!["a"], "only the current commit's comment is in scope");
    }

    #[test]
    fn whole_diff_takes_every_thread_and_a_commit_takes_only_its_own() {
        let mut whole = stock_thread();
        whole.commit = None;
        let mut on_aaa = stock_thread();
        on_aaa.commit = Some("aaa".to_owned());
        // Authored against a commit a force-push rewrote away: its sha
        // matches no entry in the rescanned commit list.
        let mut orphan = stock_thread();
        orphan.commit = Some("rewritten-away".to_owned());

        for thread in [&whole, &on_aaa, &orphan] {
            assert!(
                thread_in_scope(thread, None, "main"),
                "the whole diff is a union, so it takes every thread on the branch",
            );
        }

        assert!(thread_in_scope(&on_aaa, Some("aaa"), "main"));
        assert!(
            !thread_in_scope(&whole, Some("aaa"), "main"),
            "a whole-diff thread does not descend into an individual commit's view",
        );
        assert!(
            !thread_in_scope(&orphan, Some("aaa"), "main"),
            "a thread authored elsewhere is not this commit's",
        );
    }

    #[test]
    fn a_commit_scope_ignores_the_diff_base() {
        // `sha^..sha` is numbered against the commit's own parent, not the
        // target, so a thread authored under another base still places
        // correctly here. Filtering it out would hide it from the only
        // view that can.
        let mut thread = stock_thread();
        thread.commit = Some("aaa".to_owned());
        thread.anchor.base_ref = "HEAD".to_owned();
        assert!(
            thread_in_scope(&thread, Some("aaa"), "main"),
            "a commit takes its own threads whatever base the overlay was opened against",
        );
    }

    #[test]
    fn a_thread_against_another_diff_base_stays_out_of_the_union() {
        let mut thread = stock_thread();
        thread.anchor.base_ref = "HEAD".to_owned();
        assert!(
            !thread_in_scope(&thread, None, "main"),
            "line numbers against another base would anchor onto unrelated code",
        );
    }

    #[test]
    fn resolving_a_comment_makes_its_file_re_measure() {
        // Resolving folds the card to a marker, so the file loses rows
        // exactly as a collapse toggle does. This is the commoner half:
        // reviewers resolve far more often than they expand a marker.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "rename tok to token");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        app.diff_overlay.as_mut().expect("overlay").measured_heights[0] = Some(40);

        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);

        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").measured_heights[0],
            None,
            "the file re-measures at its new row count, as a collapse toggle makes it",
        );
    }

    #[test]
    fn toggling_a_card_open_makes_its_file_re_measure() {
        // `file_height` counts comment rows and `ensure_file_cached` only
        // measures an empty slot, so a stale height leaves `doc_offsets`
        // short by the rows the expansion added and rail jumps land off.
        let mut state = commit_mode_state();
        state.scope = DiffScope::WholeDiff;
        let line = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut thread = stock_thread();
        thread.id = "r1".to_owned();
        thread.status = ReviewStatus::Resolved;
        state.comments.push(HunkComment {
            key: line,
            path: "a.rs".into(),
            line: 1,
            comment_text: "rename tok to token".into(),
            commit: None,
            thread,
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        state.measured_heights[0] = Some(40);

        assert!(state.toggle_comment_collapse(CommentRef { line, slot: 0 }));
        assert_eq!(
            state.measured_heights[0], None,
            "the file re-measures at its new row count, as a deleted-file toggle does",
        );
    }

    #[test]
    fn reopening_a_resolved_comment_lets_it_collapse_again_when_re_resolved() {
        // Expansion is remembered per thread. Without clearing it on
        // reopen, a thread that is resolved a second time renders as a
        // full card while every other resolved one is a marker.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "rename tok to token");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let at = CommentRef { line: key, slot: 0 };
        apply_thread_action(&mut app, at, ThreadAction::Resolve);
        let overlay = app.diff_overlay.as_mut().expect("overlay");
        assert!(overlay.toggle_comment_collapse(at), "the reviewer opens it to read the thread");

        apply_thread_action(&mut app, at, ThreadAction::Reopen);
        apply_thread_action(&mut app, at, ThreadAction::Resolve);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        assert!(
            overlay.is_comment_collapsed(&overlay.comments[0]),
            "resolving it again puts it away, as it would any other thread",
        );
    }

    #[test]
    fn only_resolved_collapses() {
        // Addressed carries an answer nobody has read, and Outdated is
        // how a comment reports that it lost its anchor. Folding either
        // away hides the two states a reviewer most needs to see.
        let mut state = commit_mode_state();
        state.scope = DiffScope::WholeDiff;
        let line = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut push = |id: &str, status: ReviewStatus| {
            let mut thread = stock_thread();
            thread.id = id.to_owned();
            thread.status = status;
            state.comments.push(HunkComment {
                key: line,
                path: "a.rs".into(),
                line: 1,
                comment_text: "note".into(),
                commit: None,
                thread,
                authored_this_session: false,
                anchor_note: None,
                persisted: true,
            });
        };
        push("open", ReviewStatus::Open);
        push("addressed", ReviewStatus::Addressed);
        push("outdated", ReviewStatus::Outdated);
        push("resolved", ReviewStatus::Resolved);

        let collapsed: Vec<&str> = state
            .comments
            .iter()
            .filter(|c| state.is_comment_collapsed(c))
            .map(|c| c.thread.id.as_str())
            .collect();
        assert_eq!(collapsed, vec!["resolved"], "resolved is the only state that folds away");
    }

    #[test]
    fn clicking_a_collapsed_resolved_comment_expands_it_and_clicking_again_recollapses() {
        let mut state = commit_mode_state();
        state.scope = DiffScope::WholeDiff;
        let line = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut thread = stock_thread();
        thread.id = "r1".to_owned();
        thread.status = ReviewStatus::Resolved;
        state.comments.push(HunkComment {
            key: line,
            path: "a.rs".into(),
            line: 1,
            comment_text: "rename tok to token".into(),
            commit: None,
            thread,
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        let comment = &state.comments[0];
        assert!(state.is_comment_collapsed(comment), "resolved starts collapsed");

        let at = CommentRef { line, slot: 0 };
        assert!(state.toggle_comment_collapse(at));
        assert!(
            !state.is_comment_collapsed(&state.comments[0]),
            "clicking the marker opens the thread back up, which is how Reopen is reachable",
        );
        assert!(state.toggle_comment_collapse(at));
        assert!(state.is_comment_collapsed(&state.comments[0]), "and clicking again puts it away");
    }

    #[test]
    fn collapse_follows_the_thread_not_the_line() {
        // Keyed on the thread id, so a re-anchor that moves the card to
        // another line does not silently fold it shut again.
        let mut state = commit_mode_state();
        state.scope = DiffScope::WholeDiff;
        let line = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut thread = stock_thread();
        thread.id = "r1".to_owned();
        thread.status = ReviewStatus::Resolved;
        state.comments.push(HunkComment {
            key: line,
            path: "a.rs".into(),
            line: 1,
            comment_text: "rename tok to token".into(),
            commit: None,
            thread,
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        assert!(state.toggle_comment_collapse(CommentRef { line, slot: 0 }));
        state.comments[0].key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 7 };
        state.comments[0].line = 41;
        assert!(
            !state.is_comment_collapsed(&state.comments[0]),
            "it is still the thread the reviewer opened",
        );
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

    fn diff_line(kind: DiffLineKind, old: Option<u32>, new: Option<u32>) -> DiffLine {
        DiffLine { kind, text: "x".to_owned(), old_line: old, new_line: new }
    }

    /// A full-context (wide) file: 30 new-file lines with additions at
    /// lines 5 and 25, leaving a wide unchanged middle. The overlay
    /// captures this at open; the default context narrows it to two hunks,
    /// and expanding folds them back into one.
    fn wide_file_with_two_changes() -> FileHunks {
        let mut lines = Vec::new();
        let mut old = 1u32;
        for new in 1..=30u32 {
            if new == 5 || new == 25 {
                lines.push(diff_line(DiffLineKind::Added, None, Some(new)));
            } else {
                lines.push(diff_line(DiffLineKind::Context, Some(old), Some(new)));
                old += 1;
            }
        }
        FileHunks {
            path: "a.rs".to_owned(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: old - 1,
                new_start: 1,
                new_count: 30,
                lines,
            }],
        }
    }

    #[test]
    fn estimated_height_counts_expander_rows() {
        // Two hunks: the first starts at line 5 (4 hidden above → a
        // leading expander) with a 14-line gap to the second (a gap
        // expander). The estimate must fold both in.
        let file = FileHunks {
            path: "a.rs".to_owned(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![
                Hunk {
                    old_start: 5,
                    old_count: 1,
                    new_start: 5,
                    new_count: 1,
                    lines: vec![diff_line(DiffLineKind::Context, Some(5), Some(5))],
                },
                Hunk {
                    old_start: 20,
                    old_count: 1,
                    new_start: 20,
                    new_count: 1,
                    lines: vec![diff_line(DiffLineKind::Context, Some(20), Some(20))],
                },
            ],
        };
        // 1 header + leading expander + (@@ + line) + gap expander + (@@ + line).
        assert_eq!(estimated_height(&file, false), 7);

        // A single hunk flush at line 1 hides nothing → no expander row.
        let flush = FileHunks {
            path: "b.rs".to_owned(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![diff_line(DiffLineKind::Context, Some(1), Some(1))],
            }],
        };
        assert_eq!(estimated_height(&flush, false), 3, "no expander when nothing is hidden");
    }

    #[test]
    fn narrow_hunks_reproduces_git_hunking() {
        let wide = wide_file_with_two_changes().hunks;
        assert_eq!(narrow_hunks(&wide, 3).len(), 2, "default context keeps the gap as two hunks");
        let merged = narrow_hunks(&wide, 23);
        assert_eq!(merged.len(), 1, "wide-enough context folds the two changes into one hunk");
        assert_eq!(merged[0].lines.len(), 30, "the merged hunk carries the whole file");
    }

    #[test]
    fn context_expander_expands_from_cached_snapshot() {
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![wide_file_with_two_changes()],
        );
        // Opened at the default context: two hunks, middle hidden; the wide
        // snapshot holds the full file for in-memory expansion.
        assert_eq!(state.files[0].hunks.len(), 2, "default context leaves the gap");
        assert_eq!(state.wide_hunks[0][0].lines.len(), 30, "wide snapshot pinned at open");
        let rows_before: usize = state.files[0].hunks.iter().map(|h| h.lines.len()).sum();
        state.measured_heights[0] = Some(99);

        // Expand: pure in-memory re-slice from the cached snapshot (no git).
        state.expand_file_context(0);
        let rows_after: usize = state.files[0].hunks.iter().map(|h| h.lines.len()).sum();
        assert!(
            rows_after > rows_before,
            "expansion reveals more lines ({rows_after} > {rows_before})"
        );
        assert_eq!(state.files[0].hunks.len(), 1, "wide-enough context folds the hunks into one");
        let revealed =
            state.files[0].hunks.iter().flat_map(|h| &h.lines).any(|l| l.new_line == Some(15));
        assert!(revealed, "a previously-hidden middle line is now shown");
        assert_eq!(state.measured_heights[0], None, "height cache invalidated on expand");
        assert_eq!(state.wide_hunks[0][0].lines.len(), 30, "the wide snapshot is untouched");
    }

    #[test]
    fn comment_reanchors_after_expand() {
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![wide_file_with_two_changes()],
        );
        // Comment on the first change (new line 5), keyed at its level-3
        // display coordinate.
        let key = find_line_key(&state.files[0], 0, ReviewSide::New, 5).expect("line 5 visible");
        state.comments.push(HunkComment {
            key,
            path: "a.rs".to_owned(),
            line: 5,
            comment_text: "note".to_owned(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });

        // Expanding merges the hunks and shifts line_idx; the comment must
        // follow its line.
        state.expand_file_context(0);
        let comment = &state.comments[0];
        let anchored = &state.files[0].hunks[comment.key.hunk_idx].lines[comment.key.line_idx];
        assert_eq!(anchored.new_line, Some(5), "comment re-anchored onto its line after expand");
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
        let _ = state.select_scope(DiffScope::Commit(1));
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
            oversize: false,
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
        let mut editor = InputState::new();
        editor.insert_str("note");
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
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
        let mut editor = InputState::new();
        editor.insert_str("note");
        state.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
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
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(overlay(&app).scope, DiffScope::WholeDiff);
        assert!(!overlay(&app).jump_open, "no dropdown without commits");
        assert!(app.diff_overlay.is_some());
    }

    #[test]
    fn a_key_toggles_between_commit_and_all_changes() {
        let mut app = app_with_commit_overlay();
        if let Some(o) = app.diff_overlay.as_mut() {
            o.whole_diff_cache = Some(CachedScan {
                files: vec![one_file("x.rs", FileStatus::Modified)],
                scanner_ok: true,
            });
        }
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(overlay(&app).scope, DiffScope::WholeDiff, "a from a commit → all changes");
        assert_eq!(overlay(&app).last_commit, Some(0), "the commit is remembered");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(0), "a again → back to the commit");
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
                scope: DiffScope::Commit(0),
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
            kind: DiffScanKind::Initial {
                commits: Vec::new(),
                branch: None,
                scope: DiffScope::WholeDiff,
            },
            commit_body: None,
        };
        let state = DiffOverlayState::new_initial(event);
        assert_eq!(state.scope, DiffScope::WholeDiff, "no commits ahead → whole-diff mode");
        assert!(state.commits.is_empty());
        assert!(state.commit_cache.is_empty());
        assert!(state.whole_diff_cache.is_some());
        assert_eq!(state.untracked_suppressed, 2, "whole-diff keeps the untracked cap count");
    }

    // ---- initial-scope selection from persisted threads ----

    fn scope_thread(
        id: &str,
        commit: Option<&str>,
        updated_at: &str,
    ) -> forge_primitives::ReviewThread {
        forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 1,
                content_hash: 0,
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: Vec::new(),
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: updated_at.to_owned(),
            commit: commit.map(str::to_owned),
        }
    }

    #[test]
    fn open_prefers_whole_diff_when_whole_diff_threads_exist() {
        // A whole-diff thread keeps priority even alongside commit-scoped
        // ones - the pre-scope behavior. No threads at all -> default.
        let threads = [scope_thread("cs", Some("sha1"), "t2"), scope_thread("wd", None, "t1")];
        assert_eq!(initial_scope_from_threads(&threads), InitialScope::WholeDiff);
        assert_eq!(initial_scope_from_threads(&[]), InitialScope::Default);
    }

    #[test]
    fn open_lands_on_commit_with_persisted_thread() {
        // No whole-diff thread: the most-recently-updated commit-scoped
        // thread's commit is chosen, and it maps to that commit's index.
        let threads = [
            scope_thread("a", Some("sha0"), "2026-07-20T10:00:00Z"),
            scope_thread("b", Some("sha1"), "2026-07-21T10:00:00Z"),
        ];
        let pref = initial_scope_from_threads(&threads);
        assert_eq!(pref, InitialScope::Commit("sha1".to_owned()), "newest commit-scoped wins");

        let commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        assert_eq!(
            resolve_initial_commit(&pref, &commits),
            Some((1, "sha1".to_owned())),
            "the chosen sha maps to Commit(1)",
        );
    }

    #[test]
    fn resolve_initial_commit_defaults_and_falls_back() {
        let commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        assert_eq!(
            resolve_initial_commit(&InitialScope::Default, &commits),
            Some((0, "sha0".to_owned())),
            "default opens the first commit when the branch has commits",
        );
        assert_eq!(
            resolve_initial_commit(&InitialScope::Default, &[]),
            None,
            "default with no commits opens whole-diff",
        );
        assert_eq!(
            resolve_initial_commit(&InitialScope::WholeDiff, &commits),
            None,
            "whole-diff never resolves to a commit",
        );
        assert_eq!(
            resolve_initial_commit(&InitialScope::Commit("gone".to_owned()), &commits),
            Some((0, "sha0".to_owned())),
            "a vanished commit sha falls back to the first commit",
        );
    }

    // ---- durable review threads (persist / re-anchor / drift) ----

    fn added_line(text: &str, new: u32) -> DiffLine {
        DiffLine {
            kind: DiffLineKind::Added,
            text: text.to_owned(),
            old_line: None,
            new_line: Some(new),
        }
    }

    fn single_hunk_file(path: &str, lines: Vec<DiffLine>) -> FileHunks {
        FileHunks {
            path: path.to_owned(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![forge_workspace::env::git_diff::hunks::Hunk {
                old_start: 1,
                old_count: 0,
                new_start: 1,
                new_count: 0,
                lines,
            }],
        }
    }

    /// A minimal Open review thread for tests that build a `HunkComment`
    /// without caring about the thread's own contents.
    /// A thread as a saved comment leaves it: one unfiled user turn, which
    /// is what every production save path writes.
    fn stock_thread() -> forge_primitives::ReviewThread {
        forge_primitives::ReviewThread {
            id: "stock".to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 0,
                content_hash: 0,
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: "stock note".to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: String::new(),
            updated_at: String::new(),
            commit: None,
        }
    }

    /// A stock thread whose single `User` turn carries `text`, so a
    /// per-turn reopen seeds the editor from `thread.comments[0]`.
    fn user_thread(text: &str) -> forge_primitives::ReviewThread {
        let mut thread = stock_thread();
        thread.comments[0].text = text.to_owned();
        thread
    }

    /// A thread whose one user turn is already sealed into `review_id`, as
    /// a submitted review leaves it.
    fn filed_thread(review_id: &str) -> forge_primitives::ReviewThread {
        let mut thread = user_thread("filed note");
        thread.comments[0].review_id = Some(review_id.to_owned());
        thread
    }

    /// App wired with a workspace + redb + an active session under
    /// project "forge", ready for review-thread persistence tests.
    fn review_app() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.install_db_for_test(
            forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);
        (app, dir)
    }

    fn git(dir: &Path, args: &[&str]) {
        let out =
            std::process::Command::new("git").arg("-C").arg(dir).args(args).output().expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repo at `dir` on `branch`, with one commit so HEAD resolves.
    /// `git init -b` needs git 2.28; CI runs 2.25, hence `symbolic-ref`.
    fn init_repo(dir: &Path, branch: &str) {
        git(dir, &["init", "-q"]);
        git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "hi\n").expect("write");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// The review store key comes from the checkout being diffed, never
    /// from the session's cached git snapshot. The two diverge whenever
    /// the snapshot is stale or belongs to a session that is no longer
    /// the one under review, and the reader (`ProdReviewFacade::
    /// resolve_scope`) queries git live - so a review filed under the
    /// cached name is written where nothing looks for it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_review_branch_comes_from_the_checkout_not_the_cached_snapshot() {
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "feat/live");
        let (mut app, _dir) = review_app();
        let key = app.active_session_key.clone().expect("active key");
        let session = app.sessions.get_mut(&key).expect("session");
        session.cwd_raw = repo.path().to_string_lossy().into_owned();
        session.git_diff_snapshot = Some(forge_primitives::git_diff::GitDiffSnapshot {
            branch: forge_primitives::git::GitBranch::Named("stale-cache".to_owned()),
            default_branch: Some("main".to_owned()),
            repo_gate: RepoGate::InRepo,
            worktree: forge_primitives::git_diff::LayerState::Clean,
            branch_ahead: forge_primitives::git_diff::LayerState::Clean,
            pr: None,
            closes: Vec::new(),
        });
        set_active_view(&mut app, ActiveView::Chat);

        tokio::task::LocalSet::new()
            .run_until(async {
                open_with_target(&mut app, "HEAD".to_owned());
                // Loop on the state, count as a cap only: the spawn
                // behind this runs five git subprocesses, each against a
                // 10s timeout, so any budget short of that is asserting
                // on how loaded the runner is.
                let mut opened = false;
                for _ in 0..1500 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    drain_events(&mut app);
                    if app.diff_overlay.is_some() {
                        opened = true;
                        break;
                    }
                }
                assert!(opened, "the diff scan never landed");
            })
            .await;

        assert_eq!(
            overlay(&app).branch.as_deref(),
            Some("feat/live"),
            "the overlay keys on the live checkout, not the snapshot's stale name",
        );
    }

    fn with_editor(overlay: &mut DiffOverlayState, key: LineKey, text: &str) {
        let mut editor = InputState::new();
        editor.insert_str(text);
        overlay.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
    }

    /// [`review_app`]-style workspace + redb, plus a live agent stub and a
    /// session id wired through `set_session_id` so `dispatch_command`
    /// reaches the stub. The Finish-review submit path then seals +
    /// dispatches instead of holding on the no-agent guard.
    fn review_app_with_agent() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.install_db_for_test(
            forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let rx = app.install_testing_stub();
        app.set_session_id(Some(crate::agent::model::SessionId::new("review-session")));
        if let Some(key) = app.active_session_key.clone()
            && let Some(session) = app.sessions.get_mut(&key)
        {
            session.project = Some("forge".to_owned());
            session.cwd_raw = "/tmp/repo".into();
        }
        (app, rx, dir)
    }

    #[test]
    fn finish_review_esc_dismisses_back_to_diff() {
        let mut app = App::test_default();
        let mut state = sample_state();
        state.finish_review = Some(FinishReviewState { editor: InputState::new() });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        let after = app.diff_overlay.as_ref().expect("overlay stays open");
        assert!(after.finish_review.is_none(), "Esc dismisses the modal");
        assert_eq!(app.active_view, ActiveView::Diff, "still reviewing the diff");
    }

    #[test]
    fn submit_finish_review_seals_files_and_nudges() {
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(
            &mut overlay,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "bound check?",
        );
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        close_with_submit(&mut app);
        if let Some(o) = app.diff_overlay.as_mut() {
            o.finish_review.as_mut().expect("modal open").editor.insert_str("Solid overall.");
        }
        submit_finish_review(&mut app);
        assert!(app.diff_overlay.is_none(), "overlay closed on submit");

        let reviews = ws.load_reviews("forge", "feat").expect("load reviews");
        assert_eq!(reviews.len(), 1, "a review was sealed");
        assert_eq!(reviews[0].number, 1);
        assert_eq!(reviews[0].summary.as_deref(), Some("Solid overall."), "overview stored");
        let threads = ws.load_review_threads("forge", "feat").expect("load threads");
        assert_eq!(
            threads[0].origin_review(),
            Some(reviews[0].id.as_str()),
            "the session comment filed into the review",
        );

        let dispatched = rx.try_recv().expect("nudge dispatched");
        match dispatched {
            forge_primitives::AgentCommand::PromptWithImages { text, .. } => {
                assert!(text.contains("Review #1"), "the nudge names the sealed review");
                assert!(text.contains("review__list"), "the nudge points at the review MCP");
                // The overview and comment text stay OUT of the chat - the
                // agent reads them via review__get.
                assert!(!text.contains("Solid overall."), "overview stays out of the chat");
                assert!(!text.contains("bound check?"), "comment text stays out of the chat");
            }
            other => panic!("expected PromptWithImages, got {other:?}"),
        }
    }

    #[test]
    fn submit_finish_review_files_only_this_sessions_comments() {
        let (mut app, _rx, _dir) = review_app_with_agent();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 10,
                content_hash: 0,
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: "note".to_owned(),
                at: "t0".to_owned(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: None,
        };
        // Both threads exist in redb; the overlay carries one authored this
        // session and one hydrated from a prior pass.
        ws.save_review_threads("forge", "feat", &[seed("authored"), seed("hydrated")]);
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let comment = |line_idx: usize, id: &str, authored: bool| HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx },
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "note".into(),
            commit: None,
            thread: seed(id),
            authored_this_session: authored,
            anchor_note: None,
            persisted: true,
        };
        overlay.comments.push(comment(0, "authored", true));
        overlay.comments.push(comment(1, "hydrated", false));
        app.diff_overlay = Some(overlay);

        close_with_submit(&mut app);
        assert!(
            app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()),
            "an authored comment opens the modal",
        );
        submit_finish_review(&mut app);

        let threads = ws.load_review_threads("forge", "feat").expect("load");
        let is_filed = |id: &str| {
            threads.iter().find(|t| t.id == id).expect("thread present").origin_review().is_some()
        };
        assert!(is_filed("authored"), "the session-authored comment filed into the review");
        assert!(!is_filed("hydrated"), "the hydrated comment was NOT swept into the review");
    }

    #[test]
    fn compute_review_rows_tallies_newest_first() {
        let reviews = vec![
            forge_primitives::ReviewSet {
                id: "r1".to_owned(),
                number: 1,
                summary: Some("first pass".to_owned()),
                created_at: "2026-07-23T08:00:00Z".to_owned(),
            },
            forge_primitives::ReviewSet {
                id: "r2".to_owned(),
                number: 2,
                summary: None,
                created_at: "2026-07-23T10:00:00Z".to_owned(),
            },
        ];
        let mk = |id: &str, review: &str, status: ReviewStatus| {
            let mut t = filed_thread(review);
            t.id = id.to_owned();
            t.status = status;
            t.anchor.path = "src/a.rs".to_owned();
            t
        };
        let threads = vec![
            mk("a", "r1", ReviewStatus::Resolved),
            mk("b", "r1", ReviewStatus::Open),
            mk("d", "r1", ReviewStatus::Addressed),
            mk("c", "r2", ReviewStatus::Outdated),
        ];
        let now = parse_rfc3339("2026-07-23T12:00:00Z").expect("now parses");
        let rows = compute_review_rows(&reviews, &threads, now);

        assert_eq!(rows.len(), 2);
        // Newest review first.
        assert_eq!(rows[0].number, 2, "review 2 leads");
        assert_eq!(rows[0].total, 1);
        assert_eq!(rows[0].outdated, 1);
        assert_eq!(rows[0].age, "2h", "created two hours before now");
        assert_eq!(rows[1].number, 1);
        assert_eq!(rows[1].total, 3, "all three r1 threads tally");
        assert_eq!(rows[1].open, 1);
        assert_eq!(rows[1].addressed, 1, "the addressed thread tallies into its own bucket");
        assert_eq!(rows[1].resolved, 1);
        assert_eq!(rows[1].summary.as_deref(), Some("first pass"));
        assert_eq!(rows[1].first_path.as_deref(), Some("src/a.rs"));

        let totals = compute_review_totals(&reviews, &threads);
        assert_eq!(totals.comments, 4, "four distinct filed comments");
        assert_eq!((totals.open, totals.addressed), (1, 1));
    }

    /// A thread the reviewer replied on across rounds is listed under every
    /// review it has a turn in, and counted once in the footer.
    #[test]
    fn a_multi_round_thread_is_listed_under_each_of_its_reviews() {
        let reviews = vec![
            forge_primitives::ReviewSet {
                id: "r1".to_owned(),
                number: 1,
                summary: None,
                created_at: "2026-07-23T08:00:00Z".to_owned(),
            },
            forge_primitives::ReviewSet {
                id: "r2".to_owned(),
                number: 2,
                summary: None,
                created_at: "2026-07-23T10:00:00Z".to_owned(),
            },
        ];
        let mut spanning = filed_thread("r1");
        spanning.id = "spanning".to_owned();
        spanning.comments.push(agent_turn("addressed"));
        spanning.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "still not right".to_owned(),
            at: String::new(),
            review_id: Some("r2".to_owned()),
        });
        let threads = vec![spanning];
        let now = parse_rfc3339("2026-07-23T12:00:00Z").expect("now parses");

        let rows = compute_review_rows(&reviews, &threads, now);
        assert_eq!(rows[0].total, 1, "r2 lists it");
        assert_eq!(rows[1].total, 1, "and so does r1");
        assert_eq!(
            compute_review_totals(&reviews, &threads).comments,
            1,
            "one comment, not one per review it appears in",
        );
    }

    #[test]
    fn toggle_reviews_list_opens_with_rows_then_closes() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let mut filed = filed_thread("rev");
        filed.id = "a".to_owned();
        ws.save_review_threads("forge", "feat", &[filed]);

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        overlay.reviews = vec![forge_primitives::ReviewSet {
            id: "rev".to_owned(),
            number: 1,
            summary: None,
            created_at: String::new(),
        }];
        app.diff_overlay = Some(overlay);

        toggle_reviews_list(&mut app);
        let o = app.diff_overlay.as_ref().expect("overlay");
        assert!(o.reviews_open, "the list opened");
        assert_eq!(o.review_rows.len(), 1);
        assert_eq!(o.review_rows[0].total, 1, "the filed thread tallies into the review");

        toggle_reviews_list(&mut app);
        assert!(!app.diff_overlay.as_ref().expect("overlay").reviews_open, "toggle closes it");
    }

    #[test]
    fn toggle_reviews_list_surfaces_a_load_error() {
        // The rollup needs every thread; a corrupt threads row must surface
        // the banner, not open a list with silently-empty rollups.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("ws");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_row_for_test(&db, "forge", "feat")
            .expect("corrupt row");
        workspace.install_db_for_test(db);
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), Vec::new());
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        toggle_reviews_list(&mut app);

        let o = app.diff_overlay.as_ref().expect("overlay");
        assert!(!o.reviews_open, "the list does not open on a thread-load failure");
        assert!(o.review_load_error.is_some(), "the failure surfaces via the banner");
    }

    #[test]
    fn save_active_input_persists_a_whole_diff_thread() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(
            &mut overlay,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "needs a bound check",
        );
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let threads = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(threads.len(), 1, "the whole-diff comment persisted a thread");
        assert_eq!(threads[0].anchor.line, 10);
        assert_eq!(threads[0].anchor.side, ReviewSide::New);
        assert_eq!(threads[0].status, ReviewStatus::Open);
        assert_eq!(threads[0].commit, None, "a whole-diff thread carries no commit scope");
        assert_eq!(threads[0].comments[0].text, "needs a bound check");
        assert!(!threads[0].created_at.is_empty(), "store stamped created_at");
        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(
            comment.thread.commit, None,
            "the in-memory comment carries a whole-diff thread"
        );
    }

    #[test]
    fn save_active_input_persists_a_commit_scoped_thread() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("z", 3)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.scope = DiffScope::Commit(0);
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "commit note");
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let threads = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(threads.len(), 1, "the commit-scoped comment persisted a thread");
        assert_eq!(threads[0].commit.as_deref(), Some("aaa"), "the thread carries the commit sha");
        assert_eq!(threads[0].comments[0].text, "commit note");
        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert!(comment.persisted, "the in-memory comment is a confirmed durable write");
        assert_eq!(
            comment.thread.commit.as_deref(),
            Some("aaa"),
            "the in-memory comment's thread carries the commit sha",
        );
    }

    fn test_anchor() -> ReviewAnchor {
        ReviewAnchor {
            path: "a.rs".to_owned(),
            side: ReviewSide::New,
            line: 1,
            content_hash: 0,
            context: Vec::new(),
            base_ref: "main".to_owned(),
        }
    }

    fn agent_turn(text: &str) -> ReviewComment {
        ReviewComment {
            author: ReviewAuthor::Agent { label: "impl".to_owned() },
            text: text.to_owned(),
            at: String::new(),
            review_id: None,
        }
    }

    #[test]
    fn build_thread_rewrites_only_the_targeted_turn() {
        let mut prior = user_thread("a");
        prior.comments.push(agent_turn("x"));
        prior.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "c".to_owned(),
            at: String::new(),
            review_id: None,
        });
        let thread = build_thread(Some(prior), test_anchor(), "C!", Some(2));
        assert_eq!(thread.comments[0].text, "a", "the first turn is untouched");
        assert_eq!(thread.comments[1].text, "x", "the agent turn is untouched");
        assert_eq!(thread.comments[2].text, "C!", "only the targeted turn is rewritten");
    }

    #[test]
    fn build_thread_rejects_editing_an_agent_turn() {
        let mut prior = user_thread("a");
        prior.comments.push(agent_turn("x"));
        let thread = build_thread(Some(prior), test_anchor(), "hijack", Some(1));
        assert_eq!(thread.comments.len(), 2, "no turn is added on a rejected edit");
        assert_eq!(thread.comments[1].text, "x", "an agent turn is not editable");
    }

    #[test]
    fn build_thread_appends_a_reply_when_edit_turn_is_none() {
        let mut prior = user_thread("a");
        prior.comments.push(agent_turn("x"));
        let thread = build_thread(Some(prior), test_anchor(), "thanks", None);
        assert_eq!(thread.comments.len(), 3, "a reply appends a new turn");
        assert!(matches!(thread.comments[2].author, ReviewAuthor::User));
        assert_eq!(thread.comments[2].text, "thanks");
    }

    #[test]
    fn build_thread_mints_a_fresh_thread_without_a_prior() {
        let thread = build_thread(None, test_anchor(), "new note", None);
        assert_eq!(thread.comments.len(), 1);
        assert!(matches!(thread.comments[0].author, ReviewAuthor::User));
        assert_eq!(thread.comments[0].text, "new note");
        assert_eq!(thread.status, ReviewStatus::Open);
    }

    #[test]
    fn save_edit_turn_rewrites_that_turn_only() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut prior_thread = user_thread("first note");
        prior_thread.comments.push(ReviewComment {
            author: ReviewAuthor::User,
            text: "second note".to_owned(),
            at: String::new(),
            review_id: None,
        });
        let prior = HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "first note".into(),
            commit: None,
            thread: prior_thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        };
        let mut editor = InputState::new();
        editor.insert_str("second note EDITED");
        overlay.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior),
            edit_turn: Some(1),
        });
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(comment.thread.comments[0].text, "first note", "turn 0 is untouched");
        assert_eq!(comment.thread.comments[1].text, "second note EDITED", "turn 1 was rewritten");
        assert_eq!(comment.comment_text, "first note", "the snippet still mirrors the first turn");
    }

    #[test]
    fn save_edit_turn_persists_through_redb_keeping_the_agent_reply() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut thread = user_thread("first");
        thread.id = "t-e2e".to_owned();
        thread.comments = vec![
            ReviewComment {
                author: ReviewAuthor::User,
                text: "first".into(),
                at: String::new(),
                review_id: None,
            },
            agent_turn("addressed"),
            ReviewComment {
                author: ReviewAuthor::User,
                text: "third".into(),
                at: String::new(),
                review_id: None,
            },
        ];
        ws.upsert_review_thread("forge", "feat", thread.clone());
        let prior = HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "first".into(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        };
        let mut editor = InputState::new();
        editor.insert_str("third EDITED");
        overlay.active_input = Some(ActiveCommentInput {
            key,
            editor,
            prior_comment: Some(prior),
            edit_turn: Some(2),
        });
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let threads = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(threads.len(), 1);
        let t = &threads[0];
        assert_eq!(t.comments.len(), 3, "the chain length is preserved through the reload");
        assert_eq!(t.comments[0].text, "first", "turn 0 intact");
        assert!(
            matches!(t.comments[1].author, ReviewAuthor::Agent { .. }),
            "the interleaved agent reply survived",
        );
        assert_eq!(t.comments[1].text, "addressed", "the agent reply text is intact");
        assert_eq!(t.comments[2].text, "third EDITED", "turn 2 was rewritten");
        let c = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(c.comment_text, "first", "comment_text mirrors the first user turn");
    }

    /// Build an overlay + persisted thread, then an empty editor over
    /// `edit_turn`, ready to exercise the clear-a-turn save path.
    fn clear_turn_setup(
        turns: Vec<ReviewComment>,
        edit_turn: usize,
    ) -> (App, std::sync::Arc<forge_workspace::Workspace>, tempfile::TempDir) {
        let (mut app, dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut thread = user_thread("seed");
        thread.id = "t-clear".to_owned();
        thread.comments = turns;
        ws.upsert_review_thread("forge", "feat", thread.clone());
        let prior = HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: thread.comments.first().map(|c| c.text.clone()).unwrap_or_default(),
            commit: None,
            thread,
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        };
        overlay.active_input = Some(ActiveCommentInput {
            key,
            editor: InputState::new(),
            prior_comment: Some(prior),
            edit_turn: Some(edit_turn),
        });
        app.diff_overlay = Some(overlay);
        (app, ws, dir)
    }

    #[test]
    fn clearing_a_middle_turn_trims_it_and_keeps_the_thread() {
        let (mut app, ws, _dir) = clear_turn_setup(
            vec![
                ReviewComment {
                    author: ReviewAuthor::User,
                    text: "first".into(),
                    at: String::new(),
                    review_id: None,
                },
                agent_turn("reply"),
                ReviewComment {
                    author: ReviewAuthor::User,
                    text: "third".into(),
                    at: String::new(),
                    review_id: None,
                },
            ],
            2,
        );

        save_active_input(&mut app);

        let o = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(o.comments.len(), 1, "the card survives");
        let c = &o.comments[0];
        assert_eq!(c.thread.comments.len(), 2, "only the cleared turn was removed");
        assert_eq!(c.thread.comments[0].text, "first");
        assert!(
            matches!(c.thread.comments[1].author, ReviewAuthor::Agent { .. }),
            "the agent reply survives",
        );
        assert_eq!(c.comment_text, "first", "comment_text still mirrors the first user turn");
        let threads = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(threads.len(), 1, "the thread survives in redb");
        assert_eq!(threads[0].comments.len(), 2, "redb thread trimmed to two turns");
    }

    #[test]
    fn clearing_the_last_user_turn_deletes_the_whole_thread() {
        let (mut app, ws, _dir) = clear_turn_setup(
            vec![
                ReviewComment {
                    author: ReviewAuthor::User,
                    text: "only".into(),
                    at: String::new(),
                    review_id: None,
                },
                agent_turn("reply"),
            ],
            0,
        );

        save_active_input(&mut app);

        let o = app.diff_overlay.as_ref().expect("overlay");
        assert!(o.comments.is_empty(), "no user turn remains, so the card is gone");
        assert!(
            ws.load_review_threads("forge", "feat").expect("load").is_empty(),
            "an orphaned agent reply is not left behind in redb",
        );
    }

    #[test]
    fn reply_appends_a_new_user_turn_without_changing_state() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "first note".into(),
            commit: None,
            thread: user_thread("first note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        };
        let mut editor = InputState::new();
        editor.insert_str("second thought");
        overlay.active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: Some(prior), edit_turn: None });
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(comment.thread.comments.len(), 2, "the reply appended a turn");
        assert_eq!(comment.thread.comments[0].text, "first note");
        assert_eq!(comment.thread.comments[1].text, "second thought");
        assert!(matches!(comment.thread.comments[1].author, ReviewAuthor::User));
        assert_eq!(comment.thread.status, ReviewStatus::Open, "a reply never changes state");
    }

    #[test]
    fn a_second_reply_appends_a_second_turn() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "note".into(),
            commit: None,
            thread: user_thread("note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        for reply in ["one", "two"] {
            if let Some(o) = app.diff_overlay.as_mut() {
                reopen_comment_for_turn(o, CommentRef { line: key, slot: 0 }, None);
                if let Some(input) = o.active_input.as_mut() {
                    input.editor.insert_str(reply);
                }
            }
            save_active_input(&mut app);
        }

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        let texts: Vec<&str> = comment.thread.comments.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["note", "one", "two"], "each reply appends another turn");
    }

    #[test]
    fn empty_reply_restores_the_thread_untouched() {
        let mut app = App::test_default();
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = HunkComment {
            key,
            path: "a.rs".into(),
            line: 1,
            comment_text: "keep me".into(),
            commit: None,
            thread: user_thread("keep me"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        };
        state.active_input = Some(ActiveCommentInput {
            key,
            editor: InputState::new(),
            prior_comment: Some(prior),
            edit_turn: None,
        });
        app.diff_overlay = Some(state);

        save_active_input(&mut app);

        let after = app.diff_overlay.as_ref().expect("overlay");
        assert!(after.active_input.is_none());
        assert_eq!(after.comments.len(), 1, "an empty reply restores the comment");
        assert_eq!(after.comments[0].thread.comments.len(), 1, "no empty turn appended");
        assert_eq!(after.comments[0].comment_text, "keep me");
    }

    #[test]
    fn body_click_on_reply_opens_an_empty_editor() {
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key,
            path: "a.rs".into(),
            line: 7,
            comment_text: "note".into(),
            commit: None,
            thread: user_thread("note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(key), right: Some(key) },
            BodyRowKey::CommentReply { at: CommentRef { line: key, slot: 0 } },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 3, 160);
        assert!(effect.redraw);
        let input = state.active_input.expect("reply editor opened");
        assert_eq!(input.edit_turn, None, "a reply has no edit target");
        assert!(input.prior_comment.is_some(), "the thread is stashed for restore");
        assert!(input.editor.lines().join("\n").is_empty(), "the reply editor starts empty");
    }

    #[test]
    fn saving_a_comment_leaves_the_other_cards_on_that_line_alone() {
        // The whole diff stacks a line's threads, so saving onto a line
        // that already carries one must replace that thread and nothing
        // else. Dropping the neighbours makes them vanish until the next
        // hydrate reinstates them from the store.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        for id in ["neighbour-a", "neighbour-b"] {
            let mut thread = stock_thread();
            thread.id = id.to_owned();
            overlay.comments.push(HunkComment {
                key,
                path: "src/x.rs".into(),
                line: 5,
                comment_text: id.into(),
                commit: None,
                thread,
                authored_this_session: false,
                anchor_note: None,
                persisted: true,
            });
        }
        with_editor(&mut overlay, key, "a third on the same line");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let mut ids: Vec<&str> = overlay.comments.iter().map(|c| c.thread.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids.iter().filter(|id| id.starts_with("neighbour")).count(),
            2,
            "both co-located cards survive a save on their line; got {ids:?}",
        );
        assert_eq!(overlay.comments.len(), 3, "and the new one joins them rather than replacing");
    }

    #[test]
    fn editing_a_comment_replaces_that_thread_rather_than_adding_one() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "first draft");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        let id = app.diff_overlay.as_ref().expect("overlay").comments[0].thread.id.clone();

        // Reopen that card and save it again.
        let overlay = app.diff_overlay.as_mut().expect("overlay");
        reopen_comment_for_turn(overlay, CommentRef { line: key, slot: 0 }, Some(0));
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor.insert_str("second draft");
        }
        save_active_input(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(overlay.comments.len(), 1, "an edit replaces its own card");
        assert_eq!(overlay.comments[0].thread.id, id, "and keeps the thread's identity");
    }

    #[test]
    fn saving_in_one_scope_keeps_the_same_threads_card_in_the_other() {
        // A thread authored on a commit is in scope for that commit AND
        // for the whole diff, and `hydrate_threads` deliberately keeps
        // both cards. Replacing by identity alone takes the other scope's
        // card with it, and a cached scope switch never re-hydrates, so
        // the comment is gone from the whole diff for the rest of the
        // overlay session.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files.clone());
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.commit_cache = vec![Some(CachedScan { files, scanner_ok: true })];
        overlay.scope = DiffScope::Commit(0);
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut card = |commit: Option<&str>| {
            let mut thread = stock_thread();
            thread.id = "shared".to_owned();
            thread.commit = Some("aaa".to_owned());
            overlay.comments.push(HunkComment {
                key,
                path: "src/x.rs".into(),
                line: 5,
                comment_text: "shared".into(),
                commit: commit.map(str::to_owned),
                thread,
                authored_this_session: false,
                anchor_note: None,
                persisted: true,
            });
        };
        card(None);
        card(Some("aaa"));

        // Reply on the thread from the commit's own view and save.
        reopen_comment_for_turn(&mut overlay, CommentRef { line: key, slot: 0 }, None);
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor.insert_str("still not right");
        }
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let scopes: Vec<Option<&str>> =
            overlay.comments.iter().map(|c| c.commit.as_deref()).collect();
        assert!(
            scopes.contains(&None),
            "the whole diff's card for this thread survives a save made in the commit's view; got {scopes:?}",
        );
        assert_eq!(
            overlay.comments.iter().filter(|c| c.commit.as_deref() == Some("aaa")).count(),
            1,
            "and the saved scope still holds exactly one card for it",
        );
    }


    #[test]
    fn replying_from_the_whole_diff_leaves_a_thread_in_its_own_commit() {
        // A thread's `commit` is where it was authored, not the view you
        // are looking at. The whole diff now shows commit-homed threads,
        // so restamping it on save evicts the thread from its own
        // commit's view - durably, since the save persists it.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.scope = DiffScope::WholeDiff;
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut thread = stock_thread();
        thread.id = "homed".to_owned();
        thread.commit = Some("aaa".to_owned());
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 5,
            comment_text: "why this cast?".into(),
            commit: None,
            thread,
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });

        // Reply to it from "All changes".
        reopen_comment_for_turn(&mut overlay, CommentRef { line: key, slot: 0 }, None);
        if let Some(input) = overlay.active_input.as_mut() {
            input.editor.insert_str("still not right");
        }
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let stored = ws.load_review_threads("forge", "feat").expect("load");
        let homed = stored.iter().find(|t| t.id == "homed").expect("thread persisted");
        assert_eq!(
            homed.commit.as_deref(),
            Some("aaa"),
            "the thread stays homed on the commit it was authored against",
        );
        assert!(
            thread_in_scope(homed, Some("aaa"), "main"),
            "so it still renders in that commit's own view",
        );
    }

    #[test]
    fn a_comment_authored_in_a_commit_is_homed_there() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files.clone());
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.commit_cache = vec![Some(CachedScan { files, scanner_ok: true })];
        overlay.scope = DiffScope::Commit(0);
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "a fresh comment here");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let ws = app.workspace.clone().expect("ws");
        let stored = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(
            stored[0].commit.as_deref(),
            Some("aaa"),
            "a new thread takes the scope it was authored in as its home",
        );
    }


    #[test]
    fn switching_back_to_a_cached_scope_rebuilds_its_cards_from_the_store() {
        // A thread rendered in two scopes is two cards, each owning its
        // own clone. Resolving through one leaves the other reading the
        // old status, and a cached scope switch installs files without a
        // scan - so nothing rebuilt the stale card for the rest of the
        // overlay session.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let ws = app.workspace.clone().expect("ws");
        let mut thread = stock_thread();
        thread.id = "shared".to_owned();
        thread.commit = Some("aaa".to_owned());
        thread.anchor = ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            line: 5,
            content_hash: resolver::anchor_hash("let a = 1;"),
            context: Vec::new(),
            base_ref: "main".to_owned(),
        };
        ws.save_review_threads("forge", "feat", &[thread]);

        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files.clone());
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.commit_cache = vec![Some(CachedScan { files: files.clone(), scanner_ok: true })];
        overlay.whole_diff_cache = Some(CachedScan { files, scanner_ok: true });
        overlay.scope = DiffScope::WholeDiff;
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        // Step into the commit, resolve there, and step back.
        let outcome = app.diff_overlay.as_mut().expect("overlay").select_scope(DiffScope::Commit(0));
        assert_eq!(outcome, NavOutcome::Ready, "the commit's diff is cached");
        after_nav(&mut app, outcome);
        let line = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        apply_thread_action(&mut app, CommentRef { line, slot: 0 }, ThreadAction::Resolve);

        let outcome =
            app.diff_overlay.as_mut().expect("overlay").select_scope(DiffScope::WholeDiff);
        assert_eq!(outcome, NavOutcome::Ready, "and so is the whole diff");
        after_nav(&mut app, outcome);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let card = overlay
            .scoped_comments()
            .into_iter()
            .find(|c| c.thread.id == "shared")
            .expect("the whole diff still shows it");
        assert_eq!(
            card.thread.status,
            ReviewStatus::Resolved,
            "the card is rebuilt from the store, so it carries the status resolved elsewhere",
        );
        assert!(
            overlay.is_comment_collapsed(card),
            "and therefore collapses, which is the bug this PR fixes still live in the other view",
        );
    }

    #[test]
    fn saved_thread_survives_overlay_drop() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(
            &mut overlay,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "durable note",
        );
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        // The overlay drops (close / session-swap force-clear); redb keeps the thread.
        app.diff_overlay = None;
        let ws = app.workspace.clone().expect("ws");
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load").len(),
            1,
            "the thread outlives the overlay"
        );
    }

    #[test]
    fn hydrate_reanchors_in_place_moved_and_outdated() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed =
            |id: &str, line: u32, text: &str, context: &[&str]| forge_primitives::ReviewThread {
                id: id.to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line,
                    content_hash: resolver::anchor_hash(text),
                    context: context.iter().map(|c| (*c).to_owned()).collect(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: text.to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: None,
            };
        ws.save_review_threads(
            "forge",
            "feat",
            &[
                seed("keep", 5, "let a = 1;", &["inserted"]),
                // Its neighbours on both sides survive the insertion above.
                seed("move", 6, "let b = 2;", &["inserted2", "let c = renamed();"]),
                seed("changed", 20, "let c = 3;", &["let b = 2;"]),
                seed("vanished", 99, "let d = 4;", &["gone one", "gone two"]),
            ],
        );

        // Fresh scan: "let a = 1;" in place at 5; "let b = 2;" shifted to 8;
        // no "let c = 3;" anywhere (its content changed).
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![
                added_line("let a = 1;", 5),
                added_line("inserted", 6),
                added_line("inserted2", 7),
                added_line("let b = 2;", 8),
                added_line("let c = renamed();", 20),
            ],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let by_id =
            |id: &str| overlay.comments.iter().find(|c| c.thread.id == id).expect("comment for id");
        let keep = by_id("keep");
        assert_eq!(keep.key, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "in place");
        assert_eq!(keep.line, 5);
        let moved = by_id("move");
        assert_eq!(moved.key, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 3 }, "re-anchored");
        assert_eq!(moved.line, 8, "display line follows the move");
        // Content changed but the line number survives: placed inline
        // (line 20 = line_idx 4) and flagged Outdated.
        let changed = by_id("changed");
        assert_eq!(
            changed.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 4 },
            "inline outdated"
        );
        assert_eq!(changed.thread.status, ReviewStatus::Outdated);
        // Line number gone (99): the nearest line (20 = line_idx 4) is
        // already taken by "changed", so it falls to the next free line
        // (line_idx 2), still rendered and flagged Outdated.
        let vanished = by_id("vanished");
        assert_eq!(
            vanished.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 2 },
            "next free line"
        );
        assert_eq!(vanished.thread.status, ReviewStatus::Outdated);
        assert_ne!(vanished.key, changed.key, "outdated threads do not collide");

        // The move + outdated flips are written back to redb.
        let reloaded = ws.load_review_threads("forge", "feat").expect("load");
        let find = |id: &str| reloaded.iter().find(|t| t.id == id).expect("thread");
        assert_eq!(find("move").anchor.line, 8, "moved line persisted");
        assert_eq!(find("changed").status, ReviewStatus::Outdated, "outdated flip persisted");
        assert_eq!(find("vanished").status, ReviewStatus::Outdated, "outdated flip persisted");
        assert_eq!(find("keep").anchor.line, 5, "in-place line unchanged");
    }

    #[test]
    fn resolving_a_stacked_comment_acts_on_the_card_that_was_clicked() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, commit: Option<&str>| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 5,
                content_hash: resolver::content_hash("let a = 1;"),
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: id.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: commit.map(str::to_owned),
        };
        // Two threads on the same line: the whole diff stacks them.
        ws.save_review_threads("forge", "feat", &[seed("first", None), seed("second", Some("c0"))]);
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let line = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        apply_thread_action(&mut app, CommentRef { line, slot: 1 }, ThreadAction::Resolve);

        let stored = ws.load_review_threads("forge", "feat").expect("load");
        let status = |id: &str| stored.iter().find(|t| t.id == id).expect("thread").status;
        assert_eq!(
            status("second"),
            ReviewStatus::Resolved,
            "the second card's button resolves the second card's thread",
        );
        assert_eq!(
            status("first"),
            ReviewStatus::Open,
            "the card above it is untouched - resolving the wrong thread is the bad failure",
        );
    }

    #[test]
    fn a_force_push_orphan_renders_in_the_whole_diff() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let mut thread = stock_thread();
        thread.id = "orphan".to_owned();
        // The commit this was authored against no longer exists.
        thread.commit = Some("rewritten-away".to_owned());
        thread.anchor = ReviewAnchor {
            path: "src/x.rs".to_owned(),
            side: ReviewSide::New,
            line: 5,
            content_hash: resolver::content_hash("let a = 1;"),
            context: Vec::new(),
            base_ref: "main".to_owned(),
        };
        ws.save_review_threads("forge", "feat", &[thread]);

        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.scope = DiffScope::WholeDiff;
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let orphan = overlay
            .comments
            .iter()
            .find(|c| c.thread.id == "orphan")
            .expect("the whole diff renders a comment whose commit was rewritten away");
        assert_eq!(
            orphan.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "re-anchored against the whole-diff scan, not its dead commit",
        );
        assert_eq!(orphan.thread.status, ReviewStatus::Open, "the line is still there");
        assert!(
            overlay.scoped_comments().iter().any(|c| c.thread.id == "orphan"),
            "and it survives the render-scope filter",
        );
    }

    #[test]
    fn outdated_placement_avoids_a_live_thread_key() {
        // A live thread holds line 10; an outdated thread whose content
        // was also at line 10 (now gone) must land on a DIFFERENT key so
        // clicking / editing one can't overwrite the other.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 10,
                content_hash: resolver::content_hash(text),
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: None,
        };
        ws.save_review_threads("forge", "feat", &[seed("live", "keep"), seed("stale", "old_body")]);
        // "keep" is live at line 10; "old_body" is gone.
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("keep", 10), added_line("neighbor", 11)],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let by_id = |id: &str| comments.iter().find(|c| c.thread.id == id).expect("comment");
        let live = by_id("live");
        let stale = by_id("stale");
        assert_eq!(
            live.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "live holds line 10"
        );
        assert_eq!(
            stale.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 },
            "outdated thread avoids the live key, taking the next free line",
        );
        assert_eq!(stale.thread.status, ReviewStatus::Outdated);
    }

    #[test]
    fn outdated_thread_with_absent_file_falls_back_to_document_start() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "gone".to_owned(),
                anchor: ReviewAnchor {
                    path: "removed.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: 1,
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "note".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: None,
            }],
        );
        // The commented file is no longer in the diff.
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("keep", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);
        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(
            comment.key,
            LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            "absent file falls back to the document's first line",
        );
        assert_eq!(comment.thread.status, ReviewStatus::Outdated);
    }

    #[test]
    fn new_initial_whole_diff_scope_opens_all_changes_with_commits() {
        let event = DiffOverlayEvent {
            cwd: PathBuf::from("/tmp"),
            target: "main".into(),
            files: vec![one_file("a.rs", FileStatus::Modified)],
            scanner_ok: true,
            untracked_suppressed: 0,
            seq: 1,
            kind: DiffScanKind::Initial {
                commits: vec![commit_meta("aaa", "first"), commit_meta("bbb", "second")],
                branch: Some("feat".into()),
                scope: DiffScope::WholeDiff,
            },
            commit_body: None,
        };
        let state = DiffOverlayState::new_initial(event);
        assert_eq!(state.scope, DiffScope::WholeDiff, "persisted threads open the whole diff");
        assert_eq!(state.commits.len(), 2, "the stepper stays available");
        assert!(state.whole_diff_cache.is_some(), "whole-diff files cached");
        assert!(state.commit_cache.iter().all(Option::is_none), "commits scanned lazily");
    }

    #[test]
    fn new_initial_opens_on_the_given_commit_index() {
        // A commit-scoped reopen lands on the carrying commit, not commit 0:
        // its diff caches at that slot and its body fills that commit.
        let event = DiffOverlayEvent {
            cwd: PathBuf::from("/tmp"),
            target: "main".into(),
            files: vec![one_file("b.rs", FileStatus::Modified)],
            scanner_ok: true,
            untracked_suppressed: 0,
            seq: 1,
            kind: DiffScanKind::Initial {
                commits: vec![commit_meta("aaa", "first"), commit_meta("bbb", "second")],
                branch: Some("feat".into()),
                scope: DiffScope::Commit(1),
            },
            commit_body: Some("second commit body".to_owned()),
        };
        let state = DiffOverlayState::new_initial(event);
        assert_eq!(state.scope, DiffScope::Commit(1), "opens on the carrying commit");
        assert!(state.commit_cache[0].is_none(), "commit 0 stays lazy");
        assert!(state.commit_cache[1].is_some(), "the opened commit's diff is cached");
        assert!(state.whole_diff_cache.is_none());
        assert_eq!(state.commits[1].body, "second commit body", "body filled on the opened commit");
    }

    fn thread_status(app: &App) -> ReviewStatus {
        app.diff_overlay
            .as_ref()
            .expect("overlay")
            .comments
            .first()
            .expect("a comment")
            .thread
            .status
    }

    #[test]
    fn comment_button_resolve_and_reopen_persist() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "needs a bound");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        let ws = app.workspace.clone().expect("ws");

        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);
        assert_eq!(thread_status(&app), ReviewStatus::Resolved, "in-memory resolves");
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load")[0].status,
            ReviewStatus::Resolved,
            "persisted"
        );

        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Reopen);
        assert_eq!(thread_status(&app), ReviewStatus::Open, "in-memory reopens");
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load")[0].status,
            ReviewStatus::Open,
            "persisted"
        );
    }

    #[test]
    fn reopen_flips_addressed_and_renudges_the_worker() {
        let (mut app, mut rx, _dir) = review_app_with_agent();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut overlay = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])],
        );
        overlay.branch = Some("feat".to_owned());
        overlay.reviews = vec![forge_primitives::ReviewSet {
            id: "rev".to_owned(),
            number: 1,
            summary: None,
            created_at: String::new(),
        }];
        let mut thread = filed_thread("rev");
        thread.status = ReviewStatus::Addressed;
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".into(),
            line: 10,
            comment_text: "look here".into(),
            commit: None,
            thread,
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Reopen);

        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").comments[0].thread.status,
            ReviewStatus::Open,
            "reopen flips an addressed thread back to open",
        );
        match rx.try_recv().expect("a re-nudge was dispatched") {
            forge_primitives::AgentCommand::PromptWithImages { text, .. } => {
                assert!(
                    text.contains("Reopened") && text.contains("review #1"),
                    "the re-nudge names the reopened review: {text}",
                );
            }
            other => panic!("expected PromptWithImages, got {other:?}"),
        }
    }

    /// A thread the worker answered, anchored on `let y = 1;` at line 10
    /// so it re-resolves cleanly through `hydrate_threads`.
    fn answered_thread(id: &str) -> forge_primitives::ReviewThread {
        let mut thread = user_thread("look here");
        thread.id = id.to_owned();
        thread.anchor.line = 10;
        thread.anchor.content_hash = resolver::content_hash("let y = 1;");
        thread.comments.push(ReviewComment {
            author: ReviewAuthor::Agent { label: "impl".to_owned() },
            text: "done".to_owned(),
            at: String::new(),
            review_id: None,
        });
        thread.status = ReviewStatus::Addressed;
        thread
    }

    /// Overlay over a one-line file, on branch `feat`, ready to hydrate
    /// `answered_thread`'s anchor.
    fn overlay_for_answered_threads() -> DiffOverlayState {
        let mut overlay = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])],
        );
        overlay.branch = Some("feat".to_owned());
        overlay
    }

    fn waiting_count(app: &App) -> Option<usize> {
        app.active_session().and_then(|s| s.review_replies_waiting.as_ref()).map(|w| w.count)
    }

    #[test]
    fn hydrating_diff_recomputes_the_waiting_count_from_the_store() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[answered_thread("a")]);
        app.diff_overlay = Some(overlay_for_answered_threads());

        hydrate_threads(&mut app);

        assert_eq!(waiting_count(&app), Some(1), "an answered thread awaits the reviewer");
        assert_eq!(
            app.active_session()
                .and_then(|s| s.review_replies_waiting.as_ref())
                .map(|w| w.branch.clone()),
            Some("feat".to_owned()),
        );
    }

    /// Only a reviewer turn retires an answer. Opening `/diff` on some
    /// other branch must not take one branch's empty result as licence
    /// to drop another branch's live count.
    #[test]
    fn hydrating_another_branch_leaves_a_live_count_alone() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[answered_thread("a")]);
        app.diff_overlay = Some(overlay_for_answered_threads());
        hydrate_threads(&mut app);
        assert_eq!(waiting_count(&app), Some(1));

        let mut elsewhere = overlay_for_answered_threads();
        elsewhere.branch = Some("main".to_owned());
        app.diff_overlay = Some(elsewhere);
        hydrate_threads(&mut app);

        assert_eq!(waiting_count(&app), Some(1), "feat's answers still await a look");
    }

    #[test]
    fn replying_to_a_worker_answer_clears_the_waiting_signal() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[answered_thread("a")]);
        app.diff_overlay = Some(overlay_for_answered_threads());
        hydrate_threads(&mut app);
        assert_eq!(waiting_count(&app), Some(1), "lit before the reviewer answers");

        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let prior = app.diff_overlay.as_ref().expect("overlay").comments[0].clone();
        let mut editor = InputState::new();
        editor.insert_str("still not right");
        app.diff_overlay.as_mut().expect("overlay").active_input =
            Some(ActiveCommentInput { key, editor, prior_comment: Some(prior), edit_turn: None });
        save_active_input(&mut app);

        assert_eq!(waiting_count(&app), None, "the reviewer's own turn clears the signal");
    }

    #[test]
    fn resolving_a_worker_answer_clears_the_waiting_signal() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads("forge", "feat", &[answered_thread("a"), answered_thread("b")]);
        app.diff_overlay = Some(overlay_for_answered_threads());
        hydrate_threads(&mut app);
        assert_eq!(waiting_count(&app), Some(2), "both answers await a look");

        let resolved_key = app.diff_overlay.as_ref().expect("overlay").comments[0].key;
        apply_thread_action(
            &mut app,
            CommentRef { line: resolved_key, slot: 0 },
            ThreadAction::Resolve,
        );

        assert_eq!(waiting_count(&app), Some(1), "resolve is how a read answer is dismissed");
    }

    #[test]
    fn comment_button_click_resolves_only_the_clicked_thread() {
        let (mut app, _dir) = review_app();
        let files =
            vec![single_hunk_file("src/x.rs", vec![added_line("a", 10), added_line("b", 11)])];
        let overlay = DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        app.diff_overlay = Some(overlay);
        app.diff_overlay.as_mut().expect("overlay").branch = Some("feat".to_owned());
        let ka = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let kb = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        with_editor(app.diff_overlay.as_mut().expect("overlay"), ka, "thread A");
        save_active_input(&mut app);
        with_editor(app.diff_overlay.as_mut().expect("overlay"), kb, "thread B");
        save_active_input(&mut app);

        // Click B's Resolve button: it targets B by key, leaving A
        // untouched.
        apply_thread_action(&mut app, CommentRef { line: kb, slot: 0 }, ThreadAction::Resolve);

        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let status_of =
            |key: LineKey| overlay.comments.iter().find(|c| c.key == key).map(|c| c.thread.status);
        assert_eq!(status_of(kb), Some(ReviewStatus::Resolved), "the clicked thread resolves");
        assert_eq!(status_of(ka), Some(ReviewStatus::Open), "the other thread is untouched");

        let ws = app.workspace.clone().expect("ws");
        let threads = ws.load_review_threads("forge", "feat").expect("load");
        let persisted =
            |line: u32| threads.iter().find(|t| t.anchor.line == line).map(|t| t.status);
        assert_eq!(persisted(11), Some(ReviewStatus::Resolved), "B persisted resolved");
        assert_eq!(persisted(10), Some(ReviewStatus::Open), "A stays open in redb");
    }

    #[test]
    fn reopen_is_noop_on_an_open_thread() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("z", 3)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        // Reopen only moves a Resolved thread; an Open one is left alone.
        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Reopen);
        assert_eq!(thread_status(&app), ReviewStatus::Open, "reopen does not touch an open thread");
    }

    #[test]
    fn resolve_is_noop_when_key_has_no_thread() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("z", 3)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        // No comment at the key: the button action must not panic or write.
        apply_thread_action(
            &mut app,
            CommentRef { line: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, slot: 0 },
            ThreadAction::Resolve,
        );
        let ws = app.workspace.clone().expect("ws");
        assert!(ws.load_review_threads("forge", "feat").expect("load").is_empty());
    }

    #[test]
    fn comment_button_routes_by_the_span_the_click_lands_in() {
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![single_hunk_file("a.rs", vec![added_line("x", 1)])],
        );
        state.pane_origin_col = 0;
        state.pane_origin_row = 0;
        state.body_head_rows = 0;
        state.body_tail_scroll = 0;
        // An addressed card offers both buttons at distinct spans.
        let at = CommentRef { line: key, slot: 0 };
        state.body_keys =
            vec![BodyRowKey::CommentButton { at, resolve: Some((10, 19)), reopen: Some((22, 30)) }];

        assert_eq!(
            handle_body_click(&mut state, 12, 0).thread_action,
            Some((at, ThreadAction::Resolve)),
            "a click in the Resolve span fires Resolve",
        );
        assert_eq!(
            handle_body_click(&mut state, 25, 0).thread_action,
            Some((at, ThreadAction::Reopen)),
            "a click in the Reopen span fires Reopen",
        );
        assert_eq!(
            handle_body_click(&mut state, 20, 0).thread_action,
            None,
            "a click in the gap between the buttons no-ops",
        );
    }

    #[test]
    fn comment_button_resolves_current_scope_thread_on_key_collision() {
        // On a single-commit branch the whole-diff and commit diffs share
        // a file layout, so a whole-diff comment and a commit-scoped one -
        // both durable now - can land on the same key. The button must act
        // on the current scope's thread, not whichever `.find` hits first.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.scope = DiffScope::WholeDiff;
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        // Commit-scoped comment pushed FIRST, with its own durable thread.
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 10,
            comment_text: "commit note".to_owned(),
            commit: Some("aaa".to_owned()),
            thread: forge_primitives::ReviewThread {
                id: "tc".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 10,
                    content_hash: 0,
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "commit note".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: String::new(),
                updated_at: String::new(),
                commit: Some("aaa".to_owned()),
            },
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        // Durable whole-diff thread at the SAME key.
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 10,
            comment_text: "durable".to_owned(),
            commit: None,
            thread: forge_primitives::ReviewThread {
                id: "t1".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 10,
                    content_hash: 0,
                    context: Vec::new(),
                    base_ref: "feat".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "durable".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: String::new(),
                updated_at: String::new(),
                commit: None,
            },
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        });
        app.diff_overlay = Some(overlay);

        // In whole-diff scope the button targets the commit==None thread.
        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let durable = comments.iter().find(|c| c.commit.is_none()).expect("whole-diff comment");
        assert_eq!(
            durable.thread.status,
            ReviewStatus::Resolved,
            "the current scope's thread resolved despite the key collision",
        );
        let commit_scoped = comments.iter().find(|c| c.commit.is_some()).expect("commit comment");
        assert_eq!(
            commit_scoped.thread.status,
            ReviewStatus::Open,
            "the other scope's thread is untouched",
        );
    }

    #[test]
    fn save_then_hydrate_round_trips_in_place() {
        // Save-side capture (hash / side / context) must round-trip: an
        // unchanged file re-anchors the saved thread InPlace, Open, and
        // as a hydrated (not-session-authored) comment.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = compute();", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "bound check");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        // Reopen: re-hydrate against the same (unchanged) files.
        hydrate_threads(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(comment.key, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "InPlace");
        assert_eq!(comment.thread.status, ReviewStatus::Open);
        assert!(!comment.authored_this_session, "hydrated, not authored this session");
        assert!(comment.persisted, "hydrated comment is durable");
    }

    #[test]
    fn empty_delete_removes_the_durable_thread_from_redb() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "delete me");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        let ws = app.workspace.clone().expect("ws");
        assert_eq!(ws.load_review_threads("forge", "feat").expect("load").len(), 1, "saved");

        // Reopen the chip, clear the text, save empty -> delete.
        if let Some(o) = app.diff_overlay.as_mut() {
            reopen_comment_for_turn(o, CommentRef { line: key, slot: 0 }, Some(0));
            if let Some(input) = o.active_input.as_mut() {
                input.editor = InputState::new();
            }
        }
        save_active_input(&mut app);

        assert!(
            ws.load_review_threads("forge", "feat").expect("load").is_empty(),
            "delete removed it from redb"
        );
        // A subsequent hydrate must not resurrect it.
        hydrate_threads(&mut app);
        assert!(app.diff_overlay.as_ref().expect("overlay").comments.is_empty(), "not resurrected");
    }

    #[test]
    fn unpersistable_whole_diff_comment_stays_at_risk() {
        // No branch (detached HEAD): the write is skipped, so the comment
        // is authored-this-session but NOT persisted - view.rs must count
        // it as droppable, not log a false "durable" success.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = None;
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert!(comment.authored_this_session);
        assert!(!comment.persisted, "no branch -> write skipped -> at risk");
        assert_eq!(comment.thread.commit, None, "still a whole-diff thread, just not durable");
    }

    #[test]
    fn actionable_excludes_hydrated_and_resolved_comments() {
        let make = |authored: bool, status: ReviewStatus| {
            let thread = forge_primitives::ReviewThread {
                id: "t".to_owned(),
                anchor: ReviewAnchor {
                    path: "a.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 1,
                    content_hash: 0,
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: Vec::new(),
                status,
                created_at: String::new(),
                updated_at: String::new(),
                commit: None,
            };
            HunkComment {
                key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
                path: "a.rs".to_owned(),
                line: 1,
                comment_text: "c".to_owned(),
                commit: None,
                thread,
                authored_this_session: authored,
                anchor_note: None,
                persisted: true,
            }
        };
        // Fresh open thread, authored this session: actionable.
        assert!(is_actionable(&make(true, ReviewStatus::Open)));
        // Hydrated from a prior session: never re-nudged.
        assert!(!is_actionable(&make(false, ReviewStatus::Open)));
        // Resolved / outdated: never actionable even if touched this session.
        assert!(!is_actionable(&make(true, ReviewStatus::Resolved)));
        assert!(!is_actionable(&make(true, ReviewStatus::Outdated)));
    }

    #[test]
    fn hydrate_scopes_to_target_and_preserves_other_target_threads() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, base_ref: &str, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 5,
                content_hash: resolver::anchor_hash(text),
                context: vec!["fn wrapper() {".to_owned(), "}".to_owned()],
                base_ref: base_ref.to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: None,
        };
        // Same branch, two whole-diff targets plus a commit-scoped thread.
        let mut c = seed("c", "main", "let c = 3;");
        c.commit = Some("deadbeef".to_owned());
        ws.save_review_threads(
            "forge",
            "feat",
            &[seed("a", "main", "let a = 1;"), seed("b", "HEAD", "let b = 2;"), c],
        );

        // Open against "main"; its thread drifts (line 5 -> 8), forcing a
        // writeback. The "HEAD"-target thread must survive that writeback.
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("fn wrapper() {", 7), added_line("let a = 1;", 8), added_line("}", 9)],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        // The union spans commits but not diff bases.
        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let mut ids: Vec<&str> = comments.iter().map(|c| c.thread.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["a", "c"], "both main-target threads render, whatever their commit");
        assert!(
            !ids.contains(&"b"),
            "a thread numbered against another base would land on unrelated code",
        );

        let reloaded = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(reloaded.len(), 3, "the other-target and commit-scoped threads survived");
        assert_eq!(
            reloaded.iter().find(|t| t.id == "a").expect("a").anchor.line,
            8,
            "the main-target thread re-anchored to the moved line",
        );
        assert!(reloaded.iter().any(|t| t.id == "b"), "the HEAD-target thread is preserved");
        assert!(
            reloaded.iter().any(|t| t.id == "c"),
            "the commit-scoped thread is preserved despite sharing the target base_ref",
        );
    }

    #[test]
    fn hydrate_shows_commit_scoped_thread_on_its_commit() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "c0".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: resolver::content_hash("let a = 1;"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "on commit zero".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: Some("sha0".to_owned()),
            }],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first")];
        overlay.scope = DiffScope::Commit(0);
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        assert_eq!(comments.len(), 1, "the commit-scoped thread hydrated onto its commit");
        let c = &comments[0];
        assert_eq!(c.thread.id, "c0");
        assert_eq!(c.commit.as_deref(), Some("sha0"), "rebuilt comment carries the scope sha");
        assert!(c.persisted, "hydrated comment is durable");
        assert!(!c.authored_this_session, "hydrated, not authored this session");
    }

    #[test]
    fn hydrate_isolates_by_commit_scope() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, sha: &str, line: u32, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line,
                content_hash: resolver::anchor_hash(text),
                context: vec!["fn wrapper() {".to_owned(), "}".to_owned()],
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: Some(sha.to_owned()),
        };
        // The sha0 thread drifts (line 5 -> 8) so a writeback fires; the
        // sha1 thread must survive that writeback untouched.
        ws.save_review_threads(
            "forge",
            "feat",
            &[seed("c0", "sha0", 5, "let a = 1;"), seed("c1", "sha1", 5, "let b = 2;")],
        );
        let files = vec![single_hunk_file(
            "src/x.rs",
            vec![added_line("fn wrapper() {", 7), added_line("let a = 1;", 8), added_line("}", 9)],
        )];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        overlay.scope = DiffScope::Commit(0);
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        assert_eq!(comments.len(), 1, "only the current commit's thread renders");
        assert_eq!(comments[0].thread.id, "c0");

        let reloaded = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(reloaded.len(), 2, "the other commit's thread survives the writeback");
        assert_eq!(
            reloaded.iter().find(|t| t.id == "c0").expect("c0").anchor.line,
            8,
            "the current commit's thread re-anchored to the moved line",
        );
        assert!(reloaded.iter().any(|t| t.id == "c1"), "the sha1 thread is preserved");
    }

    #[test]
    fn hydrate_isolates_by_commit_scope_reverse() {
        // The mirror of the above from the other side: in Commit(1) scope
        // only the sha1 thread renders; the sha0 thread stays out.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, sha: &str, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 5,
                content_hash: resolver::content_hash(text),
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: Some(sha.to_owned()),
        };
        ws.save_review_threads(
            "forge",
            "feat",
            &[seed("c0", "sha0", "let b = 2;"), seed("c1", "sha1", "let a = 1;")],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        overlay.scope = DiffScope::Commit(1);
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        assert_eq!(comments.len(), 1, "only the sha1 thread renders in Commit(1)");
        assert_eq!(comments[0].thread.id, "c1");
        assert!(
            comments.iter().all(|c| c.thread.id != "c0"),
            "the sha0 thread stays out of the Commit(1) scope",
        );
        assert!(
            ws.load_review_threads("forge", "feat").expect("load").iter().any(|t| t.id == "c0"),
            "the sha0 thread is preserved in the store",
        );
    }

    #[test]
    fn commit_scoped_thread_hydrates_back_resolved() {
        // End-to-end state survival: a Resolved commit-scoped thread
        // reopened on its commit hydrates back Resolved.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "c0".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: resolver::content_hash("let a = 1;"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "resolved earlier".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Resolved,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: Some("sha0".to_owned()),
            }],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first")];
        overlay.scope = DiffScope::Commit(0);
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert_eq!(
            comment.thread.status,
            ReviewStatus::Resolved,
            "the commit-scoped thread hydrated back Resolved",
        );
    }

    #[test]
    fn hydrate_surfaces_a_review_load_error() {
        // A corrupt persisted row makes the load fail; hydrate must set the
        // visible-notice state rather than leave a silently-empty pane.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_row_for_test(&db, "forge", "feat")
            .expect("write corrupt row");
        workspace.install_db_for_test(db);
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);

        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        assert!(
            app.diff_overlay.as_ref().expect("overlay").review_load_error.is_some(),
            "a load failure surfaces the review-load notice state, not a blank pane",
        );
    }

    #[test]
    fn hydrate_surfaces_a_corrupt_reviews_row() {
        // The `reviews` table is a separate row from `review_threads`; a
        // corrupt reviews blob must surface the same banner, not silently
        // degrade every chip to `· unfiled`.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        let db = forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        forge_workspace::store::review::write_corrupt_reviews_row_for_test(&db, "forge", "feat")
            .expect("write corrupt reviews row");
        workspace.install_db_for_test(db);
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);

        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        assert!(
            app.diff_overlay.as_ref().expect("overlay").review_load_error.is_some(),
            "a corrupt reviews row surfaces the load notice, not a silent unfiled degrade",
        );
    }

    #[test]
    fn hydrate_populates_reviews_from_the_store() {
        // The `· R#` chip tag + the `l` list both read `overlay.reviews`,
        // which hydrate fills from the store - pin that it actually lands.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.submit_review(
            "forge",
            "feat",
            Some("first pass".to_owned()),
            &[],
            forge_workspace::SessionKey::from_session_id("reviewer"),
        )
        .expect("seal a review");

        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let reviews = &app.diff_overlay.as_ref().expect("overlay").reviews;
        assert_eq!(reviews.len(), 1, "hydrate loaded the submitted review");
        assert_eq!(reviews[0].number, 1);
        assert_eq!(reviews[0].summary.as_deref(), Some("first pass"));
    }

    #[test]
    fn hydrate_whole_diff_takes_commit_scoped_threads_too() {
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        let seed = |id: &str, commit: Option<&str>, text: &str| forge_primitives::ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 5,
                content_hash: resolver::content_hash(text),
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            commit: commit.map(str::to_owned),
        };
        // Both threads drift (line 5 -> 8) forcing a writeback. Same path
        // and content, so they re-anchor onto one line and stack there.
        ws.save_review_threads(
            "forge",
            "feat",
            &[seed("wd", None, "let a = 1;"), seed("cs", Some("sha0"), "let a = 1;")],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 8)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let ids: Vec<&str> = comments.iter().map(|c| c.thread.id.as_str()).collect();
        assert_eq!(ids, vec!["wd", "cs"], "the whole diff renders both, commit-scoped included");
        assert!(
            comments.iter().all(|c| c.commit.is_none()),
            "a rendered comment carries the scope it is drawn in, not the one it was authored in",
        );
        assert_eq!(
            comments[0].key, comments[1].key,
            "same line, so they stack on one key and each needs its own click target",
        );

        let reloaded = ws.load_review_threads("forge", "feat").expect("load");
        assert_eq!(reloaded.len(), 2, "both threads survive");
        let cs = reloaded.iter().find(|t| t.id == "cs").expect("the commit-scoped thread");
        assert_eq!(
            cs.commit.as_deref(),
            Some("sha0"),
            "rendering it in the union does not rewrite which commit it was authored against",
        );
    }

    #[test]
    fn drain_hydrates_commit_scoped_thread_on_navigation() {
        // The REAL path: a lazy Commit(1) scan lands via drain_events after
        // the user stepped to that commit. Its persisted thread must
        // hydrate against the just-installed files - the bug was
        // drain_events gating hydration on whole-diff scope, so navigating
        // to a commit never re-anchored its threads.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "c1".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: resolver::content_hash("let a = 1;"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "on commit one".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: Some("sha1".to_owned()),
            }],
        );

        // Overlay open in commit mode; the user has navigated to Commit(1)
        // (scope already set), its scan in flight.
        let mut overlay = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![single_hunk_file("src/x.rs", vec![added_line("noop", 1)])],
        );
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        overlay.commit_cache = vec![None, None];
        overlay.scope = DiffScope::Commit(1);
        app.diff_overlay = Some(overlay);

        // The lazy Commit(1) scan lands with sha1's file content.
        app.diff_overlay_event_tx
            .send(DiffOverlayEvent {
                cwd: PathBuf::from("/tmp/repo"),
                target: "main".to_owned(),
                files: vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])],
                scanner_ok: true,
                untracked_suppressed: 0,
                seq: app.diff_scan_seq,
                kind: DiffScanKind::Scope(DiffScope::Commit(1)),
                commit_body: Some("second".to_owned()),
            })
            .expect("send scope event");

        drain_events(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        assert_eq!(comments.len(), 1, "the commit's persisted thread hydrated on navigation");
        assert_eq!(comments[0].thread.id, "c1");
        assert_eq!(comments[0].commit.as_deref(), Some("sha1"));
        assert!(comments[0].persisted, "hydrated from redb");
    }

    #[test]
    fn hydrate_replaces_only_the_current_scope_and_keeps_others() {
        // `retain(|c| c.commit != scope_commit)` must drop ONLY the current
        // scope's in-memory comments (replaced by the rebuilt set) and keep
        // other scopes' comments. An inverted retain or a blanket clear
        // would strand the other-scope comment.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "wd".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: resolver::content_hash("let a = 1;"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "hydrated".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: None,
            }],
        );

        let mut overlay = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])],
        );
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        // An OTHER-scope (commit sha1) comment that must survive, and a stale
        // current-scope (whole-diff) comment the hydrate replaces.
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 9,
            comment_text: "on sha1".to_owned(),
            commit: Some("sha1".to_owned()),
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "stale whole-diff".to_owned(),
            commit: None,
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        app.diff_overlay = Some(overlay);

        hydrate_threads(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let other = comments
            .iter()
            .find(|c| c.commit.as_deref() == Some("sha1"))
            .expect("the other-scope comment survives");
        assert_eq!(other.comment_text, "on sha1");
        let whole: Vec<_> = comments.iter().filter(|c| c.commit.is_none()).collect();
        assert_eq!(whole.len(), 1, "one whole-diff comment after hydrate");
        assert_eq!(
            whole[0].thread.id, "wd",
            "the stale in-memory whole-diff comment was replaced by the hydrated thread",
        );
    }

    #[test]
    fn save_leaves_a_different_thread_in_another_scope_alone() {
        // The save-path twin of the hydrate retain above. This is the
        // different-thread half; `saving_in_one_scope_keeps_the_same_
        // threads_card_in_the_other` covers the same thread rendered in
        // two scopes, which is the case a retain keyed on identity alone
        // gets wrong.
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "on sha1".to_owned(),
            commit: Some("sha1".to_owned()),
            thread: stock_thread(),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        let mut sibling = stock_thread();
        sibling.id = "sibling".to_owned();
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "another whole-diff thread".to_owned(),
            commit: None,
            thread: sibling,
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        // Whole-diff scope, so the save's own scope is `None`.
        with_editor(&mut overlay, key, "fresh whole-diff");
        app.diff_overlay = Some(overlay);

        save_active_input(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        let other = comments.iter().find(|c| c.commit.as_deref() == Some("sha1"));
        assert_eq!(
            other.map(|c| c.comment_text.as_str()),
            Some("on sha1"),
            "the commit-scoped comment at the same key survives a whole-diff save",
        );
        let whole: Vec<&str> = comments
            .iter()
            .filter(|c| c.commit.is_none())
            .map(|c| c.comment_text.as_str())
            .collect();
        assert_eq!(
            whole,
            vec!["another whole-diff thread", "fresh whole-diff"],
            "the save adds its own card without disturbing the thread beside it",
        );
    }

    #[test]
    fn reopen_takes_the_comment_in_the_current_scope() {
        // The reopen twin of `save_replaces_only_the_same_scope_at_a_key`.
        // Reopening resolves by key, so with a co-located comment in
        // another scope it can pull the wrong one: the editor is seeded
        // from it, and saving then stamps THIS scope onto that thread -
        // re-scoping it durably and orphaning the one the user clicked.
        // The whole-diff comment is pushed FIRST so a key-only lookup
        // finds it before the commit-scoped one.
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.scope = DiffScope::Commit(0);
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "whole-diff note".to_owned(),
            commit: None,
            thread: user_thread("whole-diff note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "commit note".to_owned(),
            commit: Some("aaa".to_owned()),
            thread: user_thread("commit note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });

        reopen_comment_for_turn(&mut overlay, CommentRef { line: key, slot: 0 }, Some(0));

        let input = overlay.active_input.as_ref().expect("the reopen opens an editor");
        assert_eq!(
            input.editor.text(),
            "commit note",
            "the editor is seeded from the comment in the current scope",
        );
        assert_eq!(
            input.prior_comment.as_ref().and_then(|c| c.commit.as_deref()),
            Some("aaa"),
            "the stashed prior is the current scope's comment, so saving cannot re-scope another",
        );
        assert!(
            overlay.comments.iter().any(|c| c.commit.is_none()),
            "the co-located comment in another scope stays in the list",
        );
    }

    #[test]
    fn resolve_flips_an_outdated_thread_to_resolved() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        with_editor(&mut overlay, key, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        // Simulate the thread having drifted to Outdated.
        if let Some(o) = app.diff_overlay.as_mut() {
            o.comments[0].thread.status = ReviewStatus::Outdated;
        }
        apply_thread_action(&mut app, CommentRef { line: key, slot: 0 }, ThreadAction::Resolve);
        assert_eq!(thread_status(&app), ReviewStatus::Resolved, "outdated resolves to resolved");
    }

    #[test]
    fn write_failure_with_all_present_keeps_comment_at_risk() {
        // Workspace + project + branch all present, but its store isn't
        // open, so upsert returns false - the comment must stay at-risk
        // (persisted = false), not be marked durable on scope alone.
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("review-session");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.cwd_raw = "/tmp/repo".into();
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);
        // Deliberately NO install_db_for_test: the write will fail.
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert!(comment.authored_this_session);
        assert!(!comment.persisted, "a failed write with all present stays at-risk");
        assert_eq!(comment.thread.commit, None, "still a whole-diff thread");
    }

    #[test]
    fn reopen_then_cancel_keeps_a_hydrated_chip_non_actionable() {
        // A read-only view of a prior review (hydrated threads) that the
        // user clicks then Esc-cancels must not become session-authored,
        // so closing the overlay re-prompts the agent with nothing.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "h".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 10,
                    content_hash: resolver::content_hash("keep"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "prior".to_owned(),
                    at: "t0".to_owned(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: None,
            }],
        );
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("keep", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        app.diff_overlay = Some(overlay);
        hydrate_threads(&mut app);

        let key = app.diff_overlay.as_ref().expect("overlay").comments[0].key;
        if let Some(o) = app.diff_overlay.as_mut() {
            reopen_comment_for_turn(o, CommentRef { line: key, slot: 0 }, Some(0));
        }
        cancel_active_input(&mut app);

        let comment = &app.diff_overlay.as_ref().expect("overlay").comments[0];
        assert!(!comment.authored_this_session, "reopen + cancel keeps the chip hydrated");
        assert!(!is_actionable(comment), "and never nudges the agent");
    }

    #[test]
    fn force_clear_keeps_persisted_threads_in_redb() {
        let (mut app, _dir) = review_app();
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let y = 1;", 10)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        with_editor(&mut overlay, LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }, "note");
        app.diff_overlay = Some(overlay);
        save_active_input(&mut app);
        app.active_view = crate::app::view::ActiveView::Diff;

        // A session-swap force-clear drops the overlay without going
        // through close_with_submit; the persisted thread must survive.
        crate::app::view::set_active_view(&mut app, crate::app::view::ActiveView::Launchpad);
        assert!(app.diff_overlay.is_none(), "overlay force-cleared");
        let ws = app.workspace.clone().expect("ws");
        assert_eq!(
            ws.load_review_threads("forge", "feat").expect("load").len(),
            1,
            "the persisted thread survives the force-clear",
        );
    }
}
