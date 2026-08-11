//! Git-worktree helpers for the worker spawn and despawn paths:
//! detecting a repo, reading a worktree's branch, deciding whether a
//! worktree is safe to remove, removing it, and reaping the branch
//! behind it.
//!
//! Synchronous throughout because each answer is one fast git
//! subprocess and the callers are already on a tokio task.

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

/// Outcome of [`reap_worktree_branch`]. `Kept`, `KeptOnError` and
/// `DeleteFailed` each leave a branch behind and the caller warns
/// differently for each; `NotPresent` warns about nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum BranchReapOutcome {
    /// Deleted. No commit became unreachable.
    Reaped,
    /// No branch by that name, so nothing to do.
    NotPresent,
    /// `count` commits are reachable from this branch and nothing else, so
    /// deleting it would strand them. `tip` is the short sha.
    Kept { count: u64, tip: String },
    /// The check could not be completed, so whether the branch holds
    /// unique commits is unknown.
    KeptOnError { reason: String },
    /// The check passed and only the delete failed.
    DeleteFailed { reason: String },
}

/// Delete `branch` from the repo at `repo` if no commit would become
/// unreachable, i.e. every commit reachable from it is also reachable
/// from another ref or from a worktree's HEAD.
///
/// Asks about reachability, never merged-ness: nothing here consults
/// whether the branch landed, so a squash merge cannot mislead it. Any
/// failure keeps the branch.
pub fn reap_worktree_branch(repo: &Path, branch: &str) -> BranchReapOutcome {
    let refname = format!("refs/heads/{branch}");

    // Existence plus the tip in one call. `--quiet` exits 1 for a ref that
    // is missing or broken and 128 for a repo it cannot read, so "no such
    // branch" never absorbs "no such repo".
    let tip = match Command::new("git")
        .args(["rev-parse", "--short", "--verify", "--quiet", &refname])
        .current_dir(repo)
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) if out.status.code() == Some(1) => return BranchReapOutcome::NotPresent,
        Ok(out) => return BranchReapOutcome::KeptOnError { reason: git_error(&out) },
        Err(err) => return BranchReapOutcome::KeptOnError { reason: err.to_string() },
    };

    // `--exclude` filters what the NEXT `--all` considers, so it has to
    // precede it; placed after, git ignores it and every branch reads as
    // holding nothing.
    let exclude = format!("--exclude={refname}");
    let count = match Command::new("git")
        .args(["rev-list", "--count", &refname, "--not", &exclude, "--all"])
        .current_dir(repo)
        .output()
    {
        Ok(out) if out.status.success() => {
            match String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                Ok(count) => count,
                Err(err) => {
                    return BranchReapOutcome::KeptOnError {
                        reason: format!("unreadable commit count: {err}"),
                    };
                }
            }
        }
        Ok(out) => return BranchReapOutcome::KeptOnError { reason: git_error(&out) },
        Err(err) => return BranchReapOutcome::KeptOnError { reason: err.to_string() },
    };
    if count > 0 {
        return BranchReapOutcome::Kept { count, tip };
    }

    // `-D`, not `-d`: the count above IS the safety decision. `-d` would
    // re-derive merged-ness from history shape and refuses a pristine
    // branch whenever HEAD predates the commit it was created from.
    match Command::new("git").args(["branch", "-D", branch]).current_dir(repo).output() {
        Ok(out) if out.status.success() => BranchReapOutcome::Reaped,
        Ok(out) => BranchReapOutcome::DeleteFailed { reason: git_error(&out) },
        Err(err) => BranchReapOutcome::DeleteFailed { reason: err.to_string() },
    }
}

/// git's stderr, or its exit status when it said nothing.
fn git_error(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
    if stderr.is_empty() { out.status.to_string() } else { stderr }
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

    /// A repo with one commit. Never names a default branch - CI's git
    /// 2.25.1 predates `init -b` and defaults to `master`.
    fn init_repo_with_commit() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["config", "user.email", "t@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        fs::write(dir.path().join("README.md"), "seed").expect("write seed");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    /// Init a repo with one commit and add a worktree at `<repo>/wt`.
    /// Returns `(repo_tempdir, worktree_path)`.
    fn init_repo_with_worktree() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = init_repo_with_commit();
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

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .current_dir(repo)
            .output()
            .expect("spawn git");
        match out.status.code() {
            Some(0) => true,
            Some(1) => false,
            other => panic!("git rev-parse in {repo:?} exited {other:?}, so absence is unproven"),
        }
    }

    /// Init a repo with one commit plus a claude-shaped worker worktree:
    /// `worktree add -b worktree-<label>` at `.claude/worktrees/<label>`.
    /// The worktree is left in place; tests that reap remove it first,
    /// matching the despawn order.
    fn init_repo_with_worker_worktree(
        label: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf, String) {
        let dir = init_repo_with_commit();
        let branch = add_worker_worktree(dir.path(), label);
        let wt = dir.path().join(".claude").join("worktrees").join(label);
        (dir, wt, branch)
    }

    /// `worktree add -b worktree-<label>` off the current HEAD, as
    /// claude's `--worktree <label>` does. Returns the branch name.
    fn add_worker_worktree(repo: &Path, label: &str) -> String {
        let branch = format!("worktree-{label}");
        let wt = repo.join(".claude").join("worktrees").join(label);
        run_git(repo, &["worktree", "add", "-q", "-b", &branch, wt.to_str().expect("utf8 path")]);
        branch
    }

    fn git_stdout(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("spawn git");
        assert!(out.status.success(), "git {args:?} failed in {repo:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn drop_worktree(repo: &Path, wt: &Path) {
        run_git(repo, &["worktree", "remove", "--force", wt.to_str().expect("utf8 path")]);
    }

    #[test]
    fn reaps_a_pristine_worker_branch() {
        let (dir, wt, branch) = init_repo_with_worker_worktree("lbl");
        drop_worktree(dir.path(), &wt);
        assert_eq!(reap_worktree_branch(dir.path(), &branch), BranchReapOutcome::Reaped);
        assert!(!branch_exists(dir.path(), &branch), "the ref is actually gone");
    }

    /// The reachability premise itself: a branch whose commits survive only
    /// on a remote-tracking ref is still reapable, because deleting the
    /// local ref strands nothing.
    #[test]
    fn reaps_a_branch_whose_commits_live_on_a_remote_ref() {
        let (dir, wt, branch) = init_repo_with_worker_worktree("lbl");
        let remote = tempdir().expect("remote tempdir");
        run_git(remote.path(), &["init", "-q", "--bare"]);
        run_git(&wt, &["remote", "add", "origin", remote.path().to_str().expect("utf8 path")]);
        fs::write(wt.join("work.txt"), "pushed work").expect("write work");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "real work"]);
        run_git(&wt, &["push", "-q", "-u", "origin", &branch]);
        drop_worktree(dir.path(), &wt);

        assert_eq!(reap_worktree_branch(dir.path(), &branch), BranchReapOutcome::Reaped);
        assert!(!branch_exists(dir.path(), &branch), "the local ref goes, the commits do not");
    }

    #[test]
    fn keeps_a_branch_holding_unique_commits() {
        let (dir, wt, branch) = init_repo_with_worker_worktree("lbl");
        fs::write(wt.join("work.txt"), "a worker committed here").expect("write work");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "real work"]);
        drop_worktree(dir.path(), &wt);

        let outcome = reap_worktree_branch(dir.path(), &branch);
        let BranchReapOutcome::Kept { count, tip } = outcome else {
            panic!("a branch with its own commit is kept, got {outcome:?}");
        };
        assert_eq!(count, 1, "the one commit reachable from no other ref");
        assert_eq!(
            tip,
            git_stdout(dir.path(), &["rev-parse", "--short", &branch]),
            "the warning names the BRANCH tip, so 'git log <tip>' lands in the right history",
        );
        assert!(branch_exists(dir.path(), &branch), "the branch survives");
    }

    /// The squash trap: the content landed on the default branch but the
    /// shape did not, so `git cherry` reports nothing unique here and a
    /// patch-id gate would delete it.
    #[test]
    fn keeps_a_squash_merged_branch() {
        let (dir, wt, branch) = init_repo_with_worker_worktree("lbl");
        fs::write(wt.join("work.txt"), "a worker committed here").expect("write work");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "real work"]);
        run_git(dir.path(), &["merge", "-q", "--squash", &branch]);
        run_git(dir.path(), &["commit", "-q", "-m", "squashed the worker's work"]);
        drop_worktree(dir.path(), &wt);

        assert!(
            git_stdout(dir.path(), &["cherry", "HEAD", &branch]).starts_with("- "),
            "fixture precondition: the squash is patch-identical, so a patch-id gate reaps it",
        );
        assert!(
            matches!(reap_worktree_branch(dir.path(), &branch), BranchReapOutcome::Kept { .. }),
            "a squash-merged branch still holds its own commit object",
        );
        assert!(branch_exists(dir.path(), &branch));
    }

    /// `git branch -d` refuses this exact shape - pristine branch, tip not
    /// an ancestor of HEAD - so the reap must not delegate to git's
    /// merged-ness inference. The precondition is asserted so the fixture
    /// cannot stop exercising the shape silently.
    #[test]
    fn reaps_a_pristine_branch_whose_tip_is_not_an_ancestor_of_head() {
        let dir = init_repo_with_commit();
        // The operator's own branch, parked at the first commit, then the
        // worker's worktree branches off a newer one.
        run_git(dir.path(), &["branch", "operator-work"]);
        fs::write(dir.path().join("README.md"), "advanced").expect("write advance");
        run_git(dir.path(), &["commit", "-qam", "advance"]);
        let branch = add_worker_worktree(dir.path(), "lbl");
        drop_worktree(dir.path(), &dir.path().join(".claude").join("worktrees").join("lbl"));
        run_git(dir.path(), &["checkout", "-q", "operator-work"]);

        let not_ancestor = std::process::Command::new("git")
            .args(["merge-base", "--is-ancestor", &branch, "HEAD"])
            .current_dir(dir.path())
            .status()
            .expect("spawn git");
        assert!(!not_ancestor.success(), "fixture precondition: the tip is ahead of HEAD");

        assert_eq!(reap_worktree_branch(dir.path(), &branch), BranchReapOutcome::Reaped);
        assert!(!branch_exists(dir.path(), &branch));
    }

    #[test]
    fn missing_branch_is_a_noop() {
        let (dir, wt, _branch) = init_repo_with_worker_worktree("lbl");
        drop_worktree(dir.path(), &wt);
        assert_eq!(
            reap_worktree_branch(dir.path(), "worktree-never-existed"),
            BranchReapOutcome::NotPresent
        );
    }

    #[test]
    fn a_refused_delete_is_not_reported_as_a_verification_failure() {
        let (dir, _wt, branch) = init_repo_with_worker_worktree("lbl");
        let outcome = reap_worktree_branch(dir.path(), &branch);
        let BranchReapOutcome::DeleteFailed { reason } = outcome else {
            panic!("a refused delete reports DeleteFailed, got {outcome:?}");
        };
        assert!(!reason.is_empty(), "the warning can name git's complaint");
        assert!(branch_exists(dir.path(), &branch), "the branch survives a refused delete");
    }

    #[test]
    fn keeps_on_error_when_the_path_is_not_a_repo() {
        let dir = tempdir().expect("tempdir");
        assert!(
            matches!(
                reap_worktree_branch(dir.path(), "worktree-lbl"),
                BranchReapOutcome::KeptOnError { .. }
            ),
            "a repo we cannot read is never treated as a branch that does not exist",
        );
    }
}
