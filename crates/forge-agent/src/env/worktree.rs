//! Worktree-related env helpers. Currently exposes one function:
//! `is_git_repo(path)` - shells out to `git rev-parse --git-dir`
//! and returns whether the given path is inside a git repo.
//!
//! Used by the worker spawn path to decide whether to pass
//! `--worktree <label>` to claude. Synchronous because the answer
//! is fast (a single git subprocess) and the spawn-path caller is
//! already on a tokio task.

use std::path::Path;
use std::process::Command;

/// Check whether `path` is inside a git repository. Shells out to
/// `git rev-parse --git-dir`. Exit 0 -> true; anything else -> false.
///
/// Handles every git-recognised shape (normal repo, submodule,
/// worktree-in-worktree, detached HEAD, bare repo) by reading git's
/// own answer rather than re-implementing detection. Non-existent
/// paths return false because git itself errors on them.
#[must_use]
pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(path)
        .output()
        .is_ok_and(|out| out.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn returns_false_for_empty_dir() {
        let dir = tempdir().expect("tempdir");
        assert!(!is_git_repo(dir.path()));
    }

    #[test]
    fn returns_true_for_initialised_repo() {
        let dir = tempdir().expect("tempdir");
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(is_git_repo(dir.path()));
    }

    #[test]
    fn returns_false_for_nonexistent_path() {
        assert!(!is_git_repo(Path::new(
            "/nonexistent/path/that/does/not/exist"
        )));
    }

    #[test]
    fn returns_true_for_subdir_of_repo() {
        let dir = tempdir().expect("tempdir");
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).expect("mkdir");
        assert!(is_git_repo(&sub));
    }
}
