//! Git introspection data shapes.
//!
//! Type-only — branch resolution + diff scanning live in
//! `forge_agent::env::git_diff`. `GitBranch` is the wire shape
//! re-used by both the agent-side snapshot and the TUI-side render.

use serde::{Deserialize, Serialize};

/// Branch resolution states emitted by the git-diff scanner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum GitBranch {
    /// Branch is named — `.git/HEAD` resolved to `refs/heads/<name>`.
    Named(String),
    /// `.git/HEAD` points at a commit hash directly (detached HEAD).
    Detached,
    /// `cwd` isn't inside any git tree.
    #[default]
    NoRepo,
    /// Repo discovered but `.git/HEAD` couldn't be read or parsed.
    Unknown,
}

impl GitBranch {
    /// `Some(name)` for `Named`, `None` for everything else. Useful
    /// for chip-style display where only named branches surface.
    #[must_use]
    pub fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Named(name) => Some(name.as_str()),
            Self::Detached | Self::NoRepo | Self::Unknown => None,
        }
    }
}
