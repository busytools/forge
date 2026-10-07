//! Git introspection data shapes.
//!
//! Type-only - branch resolution + diff scanning live in
//! `forge_agent::env::git_diff`. `GitBranch` is the wire shape
//! re-used by both the agent-side snapshot and the TUI-side render.

use serde::{Deserialize, Serialize};

/// Branch resolution states emitted by the git-diff scanner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum GitBranch {
    /// Branch is named - `.git/HEAD` resolved to `refs/heads/<name>`.
    Named(String),
    /// `.git/HEAD` points at a commit hash directly (detached HEAD).
    Detached,
    /// `cwd` isn't inside any git tree.
    #[default]
    NoRepo,
    /// Repo discovered but `.git/HEAD` couldn't be read or parsed.
    Unknown,
}

/// How git classified one file's change.
///
/// Lives here rather than in the scanner because it crosses: the
/// snapshot the terminal reads and the view a client draws carry the
/// same glyph, and one shape for both is what keeps them agreeing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    #[default]
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    /// File mode changed (regular to symlink, file to submodule).
    Typechange,
    /// Unmerged - caught mid-merge-conflict.
    Unmerged,
    Untracked,
}

/// One commit of a branch's chain ahead of its default branch, as the
/// chain draws it: the short sha, the subject line, and the files the
/// commit itself changed - the same shape a layer's stats take, bounded
/// the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommit {
    pub sha: String,
    pub subject: String,
    pub stats: crate::git_diff::GitDiffStats,
}

/// Open pull request associated with the current branch. Populated
/// by the git-diff scanner via `gh pr list --head <branch>`; surfaces
/// in the Inspector pane's GIT section as the `PR #N` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitPrInfo {
    pub number: u64,
    pub url: String,
    /// Whether GitHub reports it as a draft; an open PR is one or the
    /// other, and the row says which.
    #[serde(default)]
    pub draft: bool,
}

/// Issue closed by an open PR (parsed from `closingIssuesReferences`
/// on the GraphQL response, which is more reliable than scanning the
/// PR body for `Closes #N`). Surfaces in the Inspector pane's GIT
/// section as part of the `PR #N → closes #M #K` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIssueRef {
    pub number: u64,
    pub url: String,
}
