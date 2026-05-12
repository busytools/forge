//! Git introspection data shapes.
//!
//! Type-only — the watcher impl (notify + tokio + filesystem) lives
//! in `forge_agent::env::git`. These are the wire shapes that cross
//! crate boundaries.

use serde::{Deserialize, Serialize};

/// A point-in-time snapshot of the git introspection state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitContext {
    /// Current branch resolution. `NoRepo` when `cwd` isn't inside a
    /// git tree at all.
    pub branch: GitBranch,
}

/// Branch resolution states emitted by `git_context` and the watcher.
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
