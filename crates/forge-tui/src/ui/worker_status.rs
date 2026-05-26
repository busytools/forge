//! Worker-status text formatters. Pure string helpers that surface
//! the `is_git_repo_at_spawn` distinction in the worker close toast.

/// Format the close toast for a worker. When the worker was spawned
/// in a git repo, claude auto-created a worktree at
/// `.claude/worktrees/<label>/` and `--worktree` deletion is opt-in
/// (the worktree survives the close); surface that to the user so
/// they know it's still there.
#[must_use]
pub(crate) fn format_close_toast(label: &str, is_git_repo_at_spawn: bool) -> String {
    if is_git_repo_at_spawn {
        format!("Worker {label} closed. Worktree preserved at .claude/worktrees/{label}/")
    } else {
        format!("Worker {label} closed.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_toast_for_git_repo_worker_mentions_preserved_worktree() {
        let s = format_close_toast("reviewer", true);
        assert!(s.contains("Worker reviewer closed."), "{s:?}");
        assert!(s.contains("Worktree preserved at"), "{s:?}");
        assert!(s.contains(".claude/worktrees/reviewer/"), "{s:?}");
    }

    #[test]
    fn close_toast_for_non_git_repo_worker_keeps_plain_text() {
        let s = format_close_toast("notes", false);
        assert_eq!(s, "Worker notes closed.");
        assert!(!s.contains("worktree"), "{s:?}");
        assert!(!s.contains("Worktree"), "{s:?}");
    }
}
