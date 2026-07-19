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
pub fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(path)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// The branch checked out at `path`, via `git rev-parse --abbrev-ref
/// HEAD`. `None` on detached HEAD, a git error, or non-utf8 output.
/// Read before a worktree is removed so its per-branch review threads
/// clean up under the same `(project, branch)` key the overlay saved
/// them with.
pub fn worktree_branch(path: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if branch.is_empty() || branch == "HEAD" { None } else { Some(branch) }
}

/// Errors from [`remove_worktree`].
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    /// The worktree path has no parent directory, so there is no
    /// in-repo location to run `git worktree remove` from.
    #[error("worktree path {0} has no parent directory to run git from")]
    NoParent(String),
    /// `git worktree remove` could not be spawned at all.
    #[error("failed to spawn git worktree remove: {0}")]
    GitSpawn(String),
    /// `git worktree remove` ran but exited non-zero (e.g. a dirty
    /// worktree without `--force`, or a transient git error). Carries
    /// git's stderr.
    #[error("git worktree remove failed: {0}")]
    RemoveFailed(String),
}

/// Why a worktree is not safe to remove, or `None` when it is clean.
///
/// "Dirty" means either uncommitted/untracked changes
/// (`git status --porcelain` non-empty) OR local commits ahead of the
/// branch's upstream (`git rev-list --count @{upstream}..HEAD` > 0).
/// A branch with no upstream configured is treated as not-unpushed -
/// there is nothing to compare against, and the porcelain check still
/// guards real uncommitted work. The despawn path calls this BEFORE
/// any teardown so a dirty worker is blocked without being killed.
pub fn worktree_dirty_reason(path: &Path) -> Option<String> {
    // Uncommitted or untracked changes.
    match Command::new("git").args(["status", "--porcelain"]).current_dir(path).output() {
        Ok(out) if out.status.success() => {
            if !out.stdout.is_empty() {
                return Some("uncommitted or untracked changes".to_owned());
            }
        }
        // Couldn't read the status - block rather than risk discarding
        // work we failed to inspect.
        _ => return Some("could not determine the worktree's git status".to_owned()),
    }

    // Unpushed commits, only when an upstream is configured. No
    // upstream -> `rev-list @{upstream}..HEAD` errors; treat as
    // not-unpushed (the porcelain check above already guarded
    // uncommitted work).
    if let Ok(out) = Command::new("git")
        .args(["rev-list", "--count", "@{upstream}..HEAD"])
        .current_dir(path)
        .output()
        && out.status.success()
    {
        let ahead: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0);
        if ahead > 0 {
            let plural = if ahead == 1 { "" } else { "s" };
            return Some(format!("{ahead} unpushed commit{plural}"));
        }
    }

    None
}

/// Remove the git worktree at `path` via `git worktree remove`. Run
/// from the worktree's PARENT (which lives inside the main worktree
/// for claude's `<repo>/.claude/worktrees/<label>` layout), since git
/// refuses to remove the worktree it is invoked from. `force` adds
/// `--force`, removing even a dirty worktree (the despawn path only
/// reaches this on a verified-clean worktree, or when the caller
/// explicitly passed `force`).
///
/// Without `--force`, git itself refuses to remove a worktree with
/// uncommitted/untracked changes; the unpushed-commits guard lives in
/// [`worktree_dirty_reason`] (git's own check does not cover unpushed).
pub fn remove_worktree(path: &Path, force: bool) -> Result<(), WorktreeError> {
    let parent =
        path.parent().ok_or_else(|| WorktreeError::NoParent(path.display().to_string()))?;
    // The worktree carries the claude session's own lock, which git
    // won't remove even with --force; unlock first, ignoring the result
    // since a not-locked worktree errors harmlessly.
    let _ =
        Command::new("git").arg("worktree").arg("unlock").arg(path).current_dir(parent).output();
    let mut cmd = Command::new("git");
    cmd.arg("worktree").arg("remove");
    if force {
        cmd.arg("--force");
    }
    cmd.arg(path).current_dir(parent);
    let output = cmd.output().map_err(|e| WorktreeError::GitSpawn(e.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(WorktreeError::RemoveFailed(String::from_utf8_lossy(&output.stderr).trim().to_owned()))
    }
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
        assert!(!is_git_repo(Path::new("/nonexistent/path/that/does/not/exist")));
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

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    /// Init a repo with one commit and add a worktree at `<repo>/wt`.
    /// Returns `(repo_tempdir, worktree_path)`.
    fn init_repo_with_worktree() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().expect("tempdir");
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["config", "user.email", "t@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        fs::write(dir.path().join("README.md"), "seed").expect("write seed");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "init"]);
        let wt = dir.path().join("wt");
        run_git(dir.path(), &["worktree", "add", "-q", wt.to_str().expect("utf8 path")]);
        (dir, wt)
    }

    #[test]
    fn clean_worktree_reports_not_dirty() {
        let (_dir, wt) = init_repo_with_worktree();
        assert_eq!(worktree_dirty_reason(&wt), None);
    }

    #[test]
    fn untracked_file_reports_dirty() {
        let (_dir, wt) = init_repo_with_worktree();
        fs::write(wt.join("scratch.txt"), "uncommitted").expect("write scratch");
        assert!(worktree_dirty_reason(&wt).is_some());
    }

    #[test]
    fn unpushed_commit_reports_dirty() {
        let (_dir, wt) = init_repo_with_worktree();
        // Establish an upstream via a bare remote, then commit locally
        // so HEAD is one ahead of @{upstream}. The working tree stays
        // clean (the change is committed), isolating the unpushed path.
        let remote = tempdir().expect("remote tempdir");
        run_git(remote.path(), &["init", "-q", "--bare"]);
        run_git(&wt, &["remote", "add", "origin", remote.path().to_str().expect("utf8 path")]);
        run_git(&wt, &["push", "-q", "-u", "origin", "HEAD"]);
        fs::write(wt.join("ahead.txt"), "local only").expect("write ahead");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "ahead of upstream"]);
        let reason = worktree_dirty_reason(&wt).expect("unpushed commit reports dirty");
        assert!(reason.contains("unpushed"), "reason names unpushed commits: {reason:?}");
    }

    #[test]
    fn worktree_branch_reports_checked_out_branch() {
        let (dir, wt) = init_repo_with_worktree();
        // The main worktree is on its init branch; the added worktree
        // gets its own detached-or-named branch. Name one explicitly.
        run_git(&wt, &["checkout", "-q", "-b", "review-loop/feat"]);
        assert_eq!(worktree_branch(&wt).as_deref(), Some("review-loop/feat"));
        assert!(worktree_branch(dir.path()).is_some(), "main worktree also reports a branch");
    }

    #[test]
    fn worktree_branch_none_for_non_repo() {
        let dir = tempdir().expect("tempdir");
        assert!(worktree_branch(dir.path()).is_none());
    }

    #[test]
    fn remove_worktree_removes_clean() {
        let (_dir, wt) = init_repo_with_worktree();
        assert!(wt.exists());
        remove_worktree(&wt, false).expect("clean worktree removes");
        assert!(!wt.exists(), "worktree dir gone after remove");
    }

    #[test]
    fn remove_worktree_removes_locked_clean() {
        let (dir, wt) = init_repo_with_worktree();
        run_git(dir.path(), &["worktree", "lock", wt.to_str().expect("utf8 path")]);
        assert!(wt.exists());
        remove_worktree(&wt, false).expect("locked clean worktree removes");
        assert!(!wt.exists(), "locked worktree dir gone after remove");
    }

    #[test]
    fn remove_worktree_errors_on_dirty_without_force() {
        let (_dir, wt) = init_repo_with_worktree();
        fs::write(wt.join("scratch.txt"), "uncommitted").expect("write scratch");
        run_git(&wt, &["add", "."]);
        assert!(remove_worktree(&wt, false).is_err(), "dirty worktree blocks removal");
        assert!(wt.exists(), "worktree survives a blocked removal");
    }

    #[test]
    fn remove_worktree_force_removes_dirty() {
        let (_dir, wt) = init_repo_with_worktree();
        fs::write(wt.join("scratch.txt"), "uncommitted").expect("write scratch");
        run_git(&wt, &["add", "."]);
        remove_worktree(&wt, true).expect("force removes a dirty worktree");
        assert!(!wt.exists(), "worktree dir gone after force remove");
    }
}
