//! Worker-status text formatters. Pure string helpers that surface
//! the `is_git_repo_at_spawn` distinction into three user-visible
//! places: the spawning-status string, the running-status cell, and
//! the close toast. Centralising these lets the per-surface call
//! sites stay one-liners and the wording stay consistent.

/// Format the spawning-status string for a worker row. Workers in a
/// git-repo project pick up claude's auto-created worktree, so the
/// "in worktree" suffix surfaces that to the user; non-git-repo
/// workers run in the project cwd directly and get the plain form.
#[must_use]
pub(crate) fn format_spawning_status(label: &str, is_git_repo_at_spawn: bool) -> String {
    if is_git_repo_at_spawn {
        format!("spawning {label} in worktree \u{2026}")
    } else {
        format!("spawning {label} \u{2026}")
    }
}

/// Format the running-status cell text for a worker. Same shape as
/// `format_spawning_status` but for Running workers: git-repo workers
/// surface "running in worktree <label>"; non-git-repo workers stay
/// at the bare "running" string.
#[must_use]
pub(crate) fn format_running_status(label: &str, is_git_repo_at_spawn: bool) -> String {
    if is_git_repo_at_spawn { format!("running in worktree {label}") } else { "running".to_owned() }
}

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
    fn spawning_status_for_git_repo_worker_mentions_worktree() {
        let s = format_spawning_status("reviewer", true);
        assert!(s.contains("in worktree"), "{s:?}");
        assert!(s.contains("reviewer"), "{s:?}");
    }

    #[test]
    fn spawning_status_for_non_git_repo_worker_skips_worktree() {
        let s = format_spawning_status("reviewer", false);
        assert!(!s.contains("worktree"), "{s:?}");
        assert!(s.contains("spawning"), "{s:?}");
        assert!(s.contains("reviewer"), "{s:?}");
    }

    #[test]
    fn running_status_for_git_repo_worker_includes_worktree_label() {
        let s = format_running_status("reviewer", true);
        assert_eq!(s, "running in worktree reviewer");
    }

    #[test]
    fn running_status_for_non_git_repo_worker_is_bare_running() {
        let s = format_running_status("notes", false);
        assert_eq!(s, "running");
        assert!(!s.contains("worktree"), "{s:?}");
    }

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
