//! Data shapes shared across the diff overlay: the view/scope enums,
//! the render-coordinate keys the renderer and the input handlers
//! exchange, and the scan event shuttled from the spawned scan task.

use std::path::PathBuf;

use crate::app::input::InputState;
use forge_workspace::env::git_diff::hunks::{CommitMeta, FileHunks};
use forge_workspace::env::git_diff::resolver::OutdatedReason;

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
/// `must_use` because dropping one skips the rest of a scope change -
/// the scan, or the card rebuild - and neither absence shows where it
/// happens.
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
    /// [`crate::app::diff_overlay::split_layout`]'s `divider_col`. At least one side is `Some`
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
/// prior comment is restored to
/// [`DiffOverlayState::comments`](crate::app::diff_overlay::DiffOverlayState::comments)
/// so
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
/// from chat while the scan was running
/// (see [`crate::app::diff_overlay::drain_events`]).
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
