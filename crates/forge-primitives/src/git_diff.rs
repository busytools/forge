//! Git diff snapshot types.
//!
//! Type-only - the scanner that produces these lives in
//! `forge_agent::env::git_diff::scan`. The four shapes cross every
//! forge-* crate boundary (agent produces, workspace mediates, TUI
//! renders), so they live in primitives per the crate placement guide.
//! Sibling git types (`GitBranch`, `GitPrInfo`, `GitIssueRef`) live in
//! [`crate::git`].

use crate::git::{GitBranch, GitIssueRef, GitPrInfo};

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
///   states. `worktree` and `branch_ahead` may each be Some or
///   None independently. `pr` / `closes` may be populated for
///   named non-default branches.
/// - `in_repo: false, scanner_ok: true` - legitimate non-repo
///   cwd (`git rev-parse` returned Empty). All of `worktree`,
///   `branch_ahead`, `pr`, `closes`, `default_branch` empty.
/// - `in_repo: false, scanner_ok: false` - failsafe collapse
///   after `git rev-parse` returned Failed / Oversize. All other
///   payload fields empty; the renderer surfaces a
///   "scanner unhealthy" banner.
///
/// Invalid combinations (do NOT construct):
/// - `in_repo: false` with ANY of `worktree` / `branch_ahead` /
///   `pr` populated, or `default_branch` Some, or `closes`
///   non-empty.
/// - `scanner_ok: false` with `in_repo: true`. The two failure
///   states (sick scanner vs healthy non-repo) are mutually
///   exclusive at the rev-parse gate.
/// - `branch_ahead: Some(_)` with `default_branch: None`.
///   `branch_ahead` is only constructed when the default branch
///   resolved; the renderer's tuple-match enforces this at the
///   call site.
/// - `worktree_scan_ok: false` with `worktree: Some(_)`, or
///   `branch_ahead_scan_ok: false` with `branch_ahead: Some(_)`.
///   The per-layer flags only flip false when numstat returned
///   None (subprocess failure), in which case the layer's
///   payload is also None.
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
    /// Layer 1: uncommitted edits vs HEAD. `None` when the tree is
    /// clean, the cwd isn't in a repo, OR the per-layer scan hit a
    /// subprocess failure; check `worktree_scan_ok` to distinguish
    /// the three.
    pub worktree: Option<GitDiffStats>,
    /// `false` when the per-layer numstat for the worktree diff
    /// surfaced `Failed` / `Oversize`. Lets the renderer show
    /// "uncommitted (scan failed)" instead of silently dropping
    /// the layer to a clean-tree render. `true` for the legitimate
    /// "clean tree" / "not in repo" / "scan succeeded" cases.
    pub worktree_scan_ok: bool,
    /// Layer 2: commits the current branch has ahead of
    /// `default_branch`. `None` on the default branch, on detached
    /// HEAD, when `default_branch` is unknown, when the cwd isn't
    /// in a repo, OR when the per-layer scan hit a subprocess
    /// failure; check `branch_ahead_scan_ok` to distinguish. The
    /// commit count is exposed separately because `--numstat`
    /// collapses every commit into a single stat block; the count
    /// tells the renderer "this many commits produced these stats".
    pub branch_ahead: Option<GitBranchAhead>,
    /// `false` when the per-layer numstat for the branch-vs-default
    /// diff surfaced `Failed` / `Oversize`. Mirror of
    /// `worktree_scan_ok` for layer 2.
    pub branch_ahead_scan_ok: bool,
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
    /// (Failed / Oversize / timeout). Combined with `in_repo` so
    /// the renderer can surface a "scanner unhealthy" banner that's
    /// distinct from a legitimate non-repo cwd.
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
