//! Wire-shape type for git worktree context. Decoded from the
//! `worktree: {...}` field claude emits on the init / init-replace
//! messages of a session spawned with `--worktree <name>`.
//!
//! Embedded on `mcp::workers::types::WorkerEntry` (workspace-side)
//! and surfaced to forge-tui via
//! `SessionUpdate::WorktreeContextChanged`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Worktree label as passed to `--worktree <name>`.
    pub name: String,
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Branch the worktree is on (claude default: `worktree-<name>`).
    pub branch: String,
    /// Parent session's cwd at worktree creation time.
    pub original_cwd: PathBuf,
    /// Parent session's branch at worktree creation time.
    pub original_branch: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let info = WorktreeInfo {
            name: "reviewer".into(),
            path: "/Users/me/Projects/forge/.claude/worktrees/reviewer".into(),
            branch: "worktree-reviewer".into(),
            original_cwd: "/Users/me/Projects/forge".into(),
            original_branch: "main".into(),
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let decoded: WorktreeInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(info, decoded);
    }
}
