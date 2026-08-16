//! Worker-status text formatters. Pure string helpers that surface
//! what became of a worker's worktree in the close toast.

use forge_workspace::protocol::WorktreeDisposition;

/// Format the close toast for a worker.
pub(crate) fn format_close_toast(label: &str, worktree: WorktreeDisposition) -> String {
    let closed = format!("Worker {label} closed.");
    let path = format!(".claude/worktrees/{label}/");
    match worktree {
        WorktreeDisposition::Absent => closed,
        WorktreeDisposition::Intact => format!("{closed} Worktree preserved at {path}"),
        WorktreeDisposition::Removed => format!("{closed} Worktree removed from {path}"),
        WorktreeDisposition::RemovalFailed => {
            format!("{closed} Worktree removal failed; it is still at {path}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_toast_for_git_repo_worker_mentions_preserved_worktree() {
        let s = format_close_toast("reviewer", WorktreeDisposition::Intact);
        assert!(s.contains("Worker reviewer closed."), "{s:?}");
        assert!(s.contains("Worktree preserved at"), "{s:?}");
        assert!(s.contains(".claude/worktrees/reviewer/"), "{s:?}");
    }

    #[test]
    fn close_toast_for_non_git_repo_worker_keeps_plain_text() {
        let s = format_close_toast("notes", WorktreeDisposition::Absent);
        assert_eq!(s, "Worker notes closed.");
        assert!(!s.contains("worktree"), "{s:?}");
        assert!(!s.contains("Worktree"), "{s:?}");
    }

    #[test]
    fn close_toast_for_a_removed_worktree_never_claims_preservation() {
        let s = format_close_toast("reviewer", WorktreeDisposition::Removed);
        assert!(s.contains("Worktree removed from .claude/worktrees/reviewer/"), "{s:?}");
        assert!(!s.contains("preserved"), "{s:?}");
    }

    #[test]
    fn close_toast_for_a_failed_removal_says_the_worktree_is_still_there() {
        let s = format_close_toast("reviewer", WorktreeDisposition::RemovalFailed);
        assert!(s.contains("removal failed"), "{s:?}");
        assert!(s.contains("still at .claude/worktrees/reviewer/"), "{s:?}");
        assert!(!s.contains("preserved"), "{s:?}");
    }
}
