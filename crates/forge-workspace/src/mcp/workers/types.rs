//! Workers MCP shared types. `WorkerStatus` is re-exported from
//! `forge-primitives`; this module owns the workspace-internal
//! `WorkerEntry` which adds routing metadata (session_key) on top
//! of the wire shape.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use forge_primitives::{WorkerLiveness, WorkerStatus};

use crate::SessionKey;

/// Cwd a worker's tag-write should land at. For git-repo workers,
/// claude's `--worktree <label>` flag forks the subprocess into
/// `<project_root>/.claude/worktrees/<label>/` and writes the session
/// JSONL under THAT sanitised path, not the project root. The
/// tag-write must follow.
///
/// `project_root` is whatever forge sources from `forge.toml`
/// (`forge-workspace::list_projects().path`); `label` is the worker's
/// charter label; `is_git_repo_at_spawn` is the cached flag on
/// `WorkerEntry`. Non-git workers run in the project root unmodified.
#[must_use]
pub fn worker_tag_dir(project_root: &Path, label: &str, is_git_repo_at_spawn: bool) -> PathBuf {
    if is_git_repo_at_spawn {
        project_root.join(".claude/worktrees").join(label)
    } else {
        project_root.to_path_buf()
    }
}

/// In-memory entry stored in `Workspace.live_workers[project_key]`.
/// `WorkerStatus` is the wire shape returned by `workers__list`;
/// `session_key` is the workspace-internal routing handle.
///
/// `needs_tag` is workspace-internal scratch state (not part of the
/// wire shape) that tracks whether the on-disk `forge:worker:<label>`
/// tag has been appended to the session's JSONL yet. claude CLI
/// writes the JSONL lazily on the first user turn, so an idle-spawned
/// worker has no JSONL at `Connected`. The tag-write is retried
/// opportunistically when the first `DeliverWorkerPrompt` arrives.
#[derive(Debug, Clone)]
pub struct WorkerEntry {
    pub label: String,
    pub charter: String,
    pub session_key: SessionKey,
    pub status: WorkerLiveness,
    pub spawned_at: SystemTime,
    pub spawned_by_session_id: String,
    pub needs_tag: bool,
    /// Cached at spawn time: was the project's path a git repo?
    /// Drives the TUI Inspector pane's WORKTREE section render and
    /// the "in worktree" status-string suffix. `true` means the
    /// worker was spawned with `--worktree=<label>` and is operating
    /// in claude's auto-created `<project>/.claude/worktrees/<label>/`;
    /// `false` means non-git-repo project (no worktree, plain cwd).
    pub is_git_repo_at_spawn: bool,
}

impl WorkerEntry {
    /// Project the workspace-internal entry to the wire shape.
    /// `session_id` is the worker's claude-issued session UUID (=
    /// `session_key.as_str().to_owned()` once Connected).
    #[must_use]
    pub fn to_status(&self) -> WorkerStatus {
        WorkerStatus {
            label: self.label.clone(),
            charter: self.charter.clone(),
            status: self.status,
            session_id: self.session_key.as_str().to_owned(),
            spawned_at: self.spawned_at,
            spawned_by_session_id: self.spawned_by_session_id.clone(),
        }
    }
}

#[cfg(test)]
mod is_git_repo_at_spawn_tests {
    use super::*;
    use crate::SessionKey;
    use std::time::SystemTime;

    fn fake_entry(is_git: bool) -> WorkerEntry {
        WorkerEntry {
            label: "reviewer".into(),
            charter: "review the diff".into(),
            session_key: SessionKey::from_session_id("uuid-1"),
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".into(),
            needs_tag: false,
            is_git_repo_at_spawn: is_git,
        }
    }

    #[test]
    fn worker_entry_carries_is_git_repo_at_spawn_true() {
        let entry = fake_entry(true);
        assert!(entry.is_git_repo_at_spawn);
    }

    #[test]
    fn worker_entry_carries_is_git_repo_at_spawn_false() {
        let entry = fake_entry(false);
        assert!(!entry.is_git_repo_at_spawn);
    }
}
