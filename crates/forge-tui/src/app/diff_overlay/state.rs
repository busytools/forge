//! The overlay's transient state: one [`DiffOverlayState`] live on
//! `App.diff_overlay` while the view is active, with the scope,
//! cache, and per-file bookkeeping it owns, plus the reviews-list row
//! shapes the `l` list renders.

use std::path::PathBuf;

use super::layout::{
    CONTEXT_STEP, DEFAULT_CONTEXT, END_CAP_ROWS, FileHighlight, estimated_height, narrow_hunks,
    reorder_files_to_tree,
};
use super::threads::find_line_key;
use super::types::{
    ActiveCommentInput, BodyRowKey, CachedScan, CommentRef, DiffOverlayEvent, DiffScanKind,
    DiffScope, DiffViewMode, DocOffsets, FinishReviewState, HunkComment, LineKey, NavOutcome,
    RailRowKey, file_offsets,
};
use forge_primitives::review::ReviewStatus;
use forge_workspace::env::git_diff::hunks::{CommitMeta, FileHunks, FileStatus, Hunk};

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
    /// A dictate warning stamped on the overlay - a truncated take
    /// landing its words in a review editor - cleared by the next
    /// overlay key.
    pub dictate_notice: Option<String>,
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
    /// The only place production assigns `scope`. The rest of a scope
    /// change happens when the caller passes on the returned outcome, so
    /// assigning it directly leaves the previous visit's cards rendering
    /// against the new scope with nothing to catch it.
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
            dictate_notice: None,
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
    pub(super) fn new_initial(event: DiffOverlayEvent) -> Self {
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
            dictate_notice: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diff_overlay::comments::save_active_input;
    use crate::app::diff_overlay::test_support::*;
    use crate::app::diff_overlay::threads::apply_thread_action;
    use crate::app::diff_overlay::types::{DiffScanKind, ThreadAction};
    use forge_primitives::review::ReviewSide;

    #[test]
    fn new_state_defaults_unified_and_doc_scroll_zero() {
        let state = sample_state();
        assert_eq!(state.view_mode, DiffViewMode::Unified);
        assert_eq!(state.doc_scroll, 0);
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
}
