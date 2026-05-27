//! Git diff snapshot types.
//!
//! Type-only - the scanner that produces these lives in
//! `forge_agent::env::git_diff::scan`. The four shapes cross every
//! forge-* crate boundary (agent produces, workspace mediates, TUI
//! renders), so they live in primitives per the crate placement guide.
//! Sibling git types (`GitBranch`, `GitPrInfo`, `GitIssueRef`) live in
//! [`crate::git`].

use crate::git::{GitBranch, GitIssueRef, GitPrInfo};

/// Per-layer scan state. Replaces the earlier parallel-bool encoding
/// (`Option<T>` paired with a `*_scan_ok: bool`) so the three legal
/// states are unrepresentable as illegal combinations.
///
/// - `Clean` - the layer ran cleanly with nothing to report (clean
///   worktree, branch at merge-base, on the default branch, detached
///   HEAD with no base, or not in a repo at all).
/// - `Populated(T)` - the layer ran and produced data the renderer
///   should show.
/// - `ScanFailed` - the underlying subprocess crashed, timed out, or
///   exceeded the stdout cap. The renderer surfaces a "(scan failed)"
///   stub so the user sees the failure rather than a silent
///   clean-tree render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerState<T> {
    Clean,
    Populated(T),
    ScanFailed,
}

impl<T> LayerState<T> {
    /// True when the layer has data the renderer should show.
    pub fn is_populated(&self) -> bool {
        matches!(self, LayerState::Populated(_))
    }

    /// Borrow the payload when populated.
    pub fn as_populated(&self) -> Option<&T> {
        match self {
            LayerState::Populated(t) => Some(t),
            _ => None,
        }
    }
}

/// Snapshot of one project's git state, suitable for rendering in
/// the Inspector pane's GIT section. Branch info is folded in here
/// so a single polled scan covers everything the renderer needs.
///
/// `worktree` and `branch_ahead` are independent: a worker on a
/// topic branch with uncommitted edits surfaces both layers, while
/// the lead on `main` with a clean tree surfaces neither.
///
/// # Field invariants
///
/// Cross-field invariants the scanner enforces (and the renderer
/// relies on). Use this as the contract when constructing a
/// snapshot field-by-field in tests or future call sites; the type
/// itself does not enforce them, the constructor in
/// `forge_agent::env::git_diff::scan` does.
///
/// Valid combinations:
/// - `in_repo: true,  scanner_ok: true` - the normal in-repo
///   states. `worktree` and `branch_ahead` may each be in any
///   [`LayerState`] independently. `pr` / `closes` may be populated
///   for named non-default branches.
/// - `in_repo: false, scanner_ok: true` - legitimate non-repo
///   cwd (`git rev-parse` returned Empty). Both layer states are
///   [`LayerState::Clean`]; `pr` / `closes` / `default_branch`
///   empty.
/// - `in_repo: false, scanner_ok: false` - failsafe collapse
///   after `git rev-parse` returned Failed / Oversize. Both layer
///   states are [`LayerState::Clean`]; the renderer surfaces a
///   "scanner unhealthy" banner from `scanner_ok` rather than
///   per-layer signals here.
///
/// Invalid combinations (do NOT construct):
/// - `in_repo: false` with EITHER layer state set to
///   [`LayerState::Populated`] or [`LayerState::ScanFailed`], or
///   `default_branch` Some, or `closes` non-empty.
/// - `scanner_ok: false` with `in_repo: true`. The two failure
///   states (sick scanner vs healthy non-repo) are mutually
///   exclusive at the rev-parse gate.
/// - `branch_ahead: LayerState::Populated(_)` with
///   `default_branch: None`. The layer is only populated when the
///   default branch resolved; the renderer's tuple-match enforces
///   this at the call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffSnapshot {
    /// Current branch (Named / Detached / NoRepo / Unknown).
    pub branch: GitBranch,
    /// Resolved default branch (e.g. `main`, `master`), if known.
    /// `None` when `origin/HEAD` is missing AND neither `main` nor
    /// `master` exists as a local ref.
    pub default_branch: Option<String>,
    /// `false` when `cwd` is not inside a git repository (rev-parse
    /// reported empty output). Combined with [`Self::scanner_ok`],
    /// lets consumers distinguish "not a repo" (in_repo=false,
    /// scanner_ok=true) from "scanner crashed" (in_repo=false,
    /// scanner_ok=false).
    pub in_repo: bool,
    /// Layer 1: uncommitted edits vs HEAD.
    pub worktree: LayerState<GitDiffStats>,
    /// Layer 2: commits the current branch has ahead of
    /// `default_branch`. The commit count is exposed separately
    /// from the file stats because `--numstat` collapses every
    /// commit into a single stat block; the count tells the
    /// renderer "this many commits produced these stats".
    pub branch_ahead: LayerState<GitBranchAhead>,
    /// Open pull request for the current branch, if one exists. Only
    /// populated for `Named` non-default branches; `None` otherwise.
    /// Cached across scans by branch name - refetched only when the
    /// branch changes (see the scanner's `prev` parameter).
    pub pr: Option<GitPrInfo>,
    /// Issues the open PR closes (from GitHub's
    /// `closingIssuesReferences`). Empty when there's no PR or the
    /// PR doesn't reference any issues. Cached alongside `pr`.
    pub closes: Vec<GitIssueRef>,
    /// `false` when the underlying scan hit a subprocess failure
    /// (Failed / Oversize / timeout) at the rev-parse gate. Combined
    /// with `in_repo` so the renderer can surface a "scanner
    /// unhealthy" banner that's distinct from a legitimate non-repo
    /// cwd.
    pub scanner_ok: bool,
}

/// Per-file numstat plus aggregate totals for one diff layer.
/// Shared between [`GitDiffSnapshot::worktree`] (layer 1, HEAD vs
/// workdir) and [`GitBranchAhead::stats`] (layer 2, default vs
/// branch tip).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitDiffStats {
    pub files: Vec<GitDiffFile>,
    pub total_files: usize,
    pub total_added: u32,
    pub total_removed: u32,
}

/// Layer 2 payload: how far the branch is ahead of the default
/// branch, alongside the corresponding numstat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBranchAhead {
    /// Number of commits between the merge-base with `default_branch`
    /// and the branch tip. The renderer surfaces this so the user
    /// can see "N commits ahead" without inferring it from the file
    /// list.
    pub commit_count: u32,
    /// File-level numstat for the same commit range.
    pub stats: GitDiffStats,
}

/// One file's diff stats. `added` / `removed` are git's `--numstat`
/// line counts; binary files (which numstat reports as `-`) are
/// dropped by the parser rather than appearing here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffFile {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_state_is_populated_returns_true_only_for_populated() {
        let clean: LayerState<u32> = LayerState::Clean;
        let populated: LayerState<u32> = LayerState::Populated(42);
        let failed: LayerState<u32> = LayerState::ScanFailed;
        assert!(!clean.is_populated());
        assert!(populated.is_populated());
        assert!(!failed.is_populated());
    }

    #[test]
    fn layer_state_as_populated_borrows_payload() {
        let populated: LayerState<u32> = LayerState::Populated(42);
        assert_eq!(populated.as_populated(), Some(&42));
        let clean: LayerState<u32> = LayerState::Clean;
        assert_eq!(clean.as_populated(), None);
        let failed: LayerState<u32> = LayerState::ScanFailed;
        assert_eq!(failed.as_populated(), None);
    }
}
