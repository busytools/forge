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
    let branch = git_in_repo(path, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_owned();
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

/// Whether `branch` resolves at `repo`, as a local head or as any
/// remote-tracking ref. A repo git cannot read answers `true`, so a
/// caller cleaning up on absence never discards state it failed to
/// inspect.
///
/// Membership in [`repo_branch_names`] rather than a targeted
/// `for-each-ref` pattern, so both callers share one parser. A pattern
/// cannot answer this safely on a delete path: `refs/remotes/*/<branch>`
/// has a `*` that does not cross `/`, so a remote named `gh/upstream`
/// puts its refs where the pattern cannot see them, and `a[b`, a leading
/// `-` or a space match nothing. Every one of those exits 0 with empty
/// stdout, and empty is what triggers the delete.
///
/// Remote-tracking refs count because [`reap_worktree_branch`] deletes a
/// local head whose commits are reachable from one, and a branch that
/// survives on a remote is still open for review.
pub fn branch_ref_exists(repo: &Path, branch: &str) -> bool {
    repo_branch_names(repo).is_none_or(|repo| repo.names.contains(branch))
}

/// Run `git -C repo <args>` with the ambient repo-location env scrubbed,
/// returning stdout on exit 0.
///
/// `GIT_DIR` / `GIT_WORK_TREE` / `GIT_COMMON_DIR` override `-C` outright,
/// so without the scrub a forge launched from a git hook, a `git rebase
/// --exec`, or any shell that exported them answers every question about
/// every repo from one foreign repo - at exit 0, with nothing in the
/// output to say so.
fn git_in_repo(repo: &Path, args: &[&str]) -> Option<String> {
    let out = scrubbed_git(repo, args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The command [`git_in_repo`] runs, built separately so a test can
/// assert the scrub is on it. Removing any of the three `env_remove`
/// calls leaves every other test in the crate passing.
fn scrubbed_git(repo: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR");
    command
}

/// What a repo knows about its branches, in two quantities that must not
/// be confused for each other.
///
/// [`Self::names`] deliberately over-includes and [`Self::ref_count`] is
/// exact. A caller sparing on membership wants the first; a caller
/// judging whether the repo can be trusted to answer wants the second.
/// They were one value once, and the over-inclusion silently inflated
/// the population: each slash in a branch name adds a phantom suffix, so
/// `release/2.0/rc1` reads as three names against two refs and a shallow
/// clone looks well-stocked. One slash does not show it - two names, two
/// refs.
pub struct RepoBranches {
    /// Every name a stored branch might legitimately match. Includes
    /// every `/`-suffix of a remote-tracking ref's tail, so it can only
    /// spare a branch, never condemn one - which is why nothing is
    /// dropped from it on a name test. NOT a population.
    pub names: std::collections::HashSet<String>,
    /// Branch refs the repo actually holds - local heads plus
    /// remote-tracking refs, excluding symrefs, which point at another
    /// ref in the same listing rather than being branches of their own.
    /// Independent of how any branch is named, which is the whole point:
    /// a count derived from names moves when a branch is renamed.
    pub ref_count: usize,
}

/// Every branch name the repo rooted exactly at `repo` knows, plus how
/// many branch refs it actually holds. `None` when `repo` is not itself
/// a work-tree root or git could not be asked.
///
/// One listing, then exact string membership. That is the difference
/// between this and asking [`branch_ref_exists`] per branch: a
/// `for-each-ref` *pattern* matches path prefixes, so a stored `fix`
/// matches `fix/anything`, an empty name matches every head, and a name
/// carrying `[`, a leading `-` or a space matches nothing and exits 0 -
/// indistinguishable from a branch that is genuinely gone. A caller that
/// deletes on absence cannot tell those apart; a set cannot produce them.
///
/// A remote-tracking ref is `refs/remotes/<remote>/<branch>`, and
/// `<remote>` may itself contain slashes - `git remote add gh/upstream`
/// is accepted - so where one segment ends and the other begins is not
/// recoverable from the refname. Every `/`-suffix of the tail joins
/// `names` rather than guessing: `gh/upstream/feat/pushed` contributes
/// `upstream/feat/pushed`, `feat/pushed` and `pushed`. That can only add
/// names, so its failure is to spare a branch whose name happens to be a
/// suffix of another ref's path - never to delete a live one, which
/// splitting once does. `ref_count` counts refs and is untouched by it.
///
/// The `--show-toplevel` equality check is what keeps discovery from
/// walking up: git answers happily from an ancestor repo when `repo` is
/// merely nested inside one. It is only meaningful because the helper
/// above scrubs the env first - with `GIT_DIR` set, `--show-toplevel`
/// reports the working directory and the check passes while the refs
/// come from elsewhere.
pub fn repo_branch_names(repo: &Path) -> Option<RepoBranches> {
    let toplevel = git_in_repo(repo, &["rev-parse", "--show-toplevel"])?;
    let toplevel = std::fs::canonicalize(toplevel.trim()).ok()?;
    if toplevel != std::fs::canonicalize(repo).ok()? {
        return None;
    }
    let listing = git_in_repo(
        repo,
        &["for-each-ref", "--format=%(refname) %(symref)", "refs/heads", "refs/remotes"],
    )?;
    let mut names = std::collections::HashSet::new();
    let mut ref_count = 0;
    for line in listing.lines() {
        // A ref name cannot contain a space, so the first one separates
        // it from `%(symref)`, which is empty for everything but a symref.
        let (refname, symref) = line.split_once(' ').unwrap_or((line, ""));
        // `origin/HEAD` points at another ref in this same listing, so
        // counting it would make every clone look one branch richer and
        // its target's name is already here. Tested by symref rather than
        // by the name: `release/HEAD` is a legal branch, and skipping it
        // on the name would drop a real branch out of `names` - which
        // deletes it.
        if !symref.trim().is_empty() {
            continue;
        }
        if let Some(head) = refname.strip_prefix("refs/heads/") {
            ref_count += 1;
            names.insert(head.to_owned());
        } else if let Some(tail) = refname.strip_prefix("refs/remotes/") {
            ref_count += 1;
            let mut rest = tail;
            while let Some((_, suffix)) = rest.split_once('/') {
                names.insert(suffix.to_owned());
                rest = suffix;
            }
        }
    }
    names.remove("");
    Some(RepoBranches { names, ref_count })
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
    fn repo_branch_names_lists_local_heads_and_remote_tracking_refs() {
        let dir = init_repo_with_commit();
        run_git(dir.path(), &["branch", "feat/live"]);
        run_git(dir.path(), &["update-ref", "refs/remotes/origin/feat/pushed", "HEAD"]);
        // A remote whose own name carries no slash, and one branch under a
        // nested path, so the two-segment split is exercised both ways.
        run_git(dir.path(), &["update-ref", "refs/remotes/upstream/main", "HEAD"]);

        let names = repo_branch_names(dir.path()).expect("a repo root answers").names;
        assert!(names.contains("feat/live"), "local heads are listed: {names:?}");
        assert!(names.contains("feat/pushed"), "remote-tracking refs count: {names:?}");
        assert!(names.contains("main") || names.contains("master"), "{names:?}");
        assert!(
            !names.iter().any(|n| n.starts_with("refs/")),
            "names are branch names, not refnames: {names:?}",
        );
    }

    /// The branch this returns is what the despawn delete is keyed on,
    /// so an unscrubbed read hands the wrong name to a correctly-scrubbed
    /// check: absent in the real repo, and its review state deleted. A
    /// scrubbed check beside an unscrubbed read is the asymmetry that
    /// produced the original defect.
    #[test]
    fn worktree_branch_is_scrubbed_like_every_other_repo_question() {
        let foreign = init_repo_with_commit();
        run_git(foreign.path(), &["checkout", "-q", "-b", "only-in-foreign"]);
        let host = init_repo_with_commit();
        run_git(host.path(), &["checkout", "-q", "-b", "the-real-branch"]);

        let command = scrubbed_git(host.path(), &["rev-parse", "--abbrev-ref", "HEAD"]);
        let removed: Vec<&str> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(key, _)| key.to_str())
            .collect();
        assert!(removed.contains(&"GIT_DIR"), "the read goes through the scrub: {removed:?}");
        assert_eq!(worktree_branch(host.path()).as_deref(), Some("the-real-branch"));
        let _ = foreign;
    }

    /// `release/HEAD` is a legal branch name - `git branch release/HEAD`
    /// succeeds - so excluding `origin/HEAD` by its last segment drops a
    /// real branch out of the alive set, and absent is what deletes. The
    /// symref is what distinguishes them: a clone's `origin/HEAD` carries
    /// one, a branch never does.
    #[test]
    fn a_branch_legally_named_head_is_not_mistaken_for_the_symref() {
        let dir = init_repo_with_commit();
        run_git(dir.path(), &["update-ref", "refs/remotes/origin/release/HEAD", "HEAD"]);
        run_git(dir.path(), &["update-ref", "refs/remotes/origin/feat/a", "HEAD"]);
        // What a clone leaves: a real symref, not a plain ref.
        run_git(
            dir.path(),
            &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/feat/a"],
        );

        let repo = repo_branch_names(dir.path()).expect("a repo root answers");
        assert!(
            repo.names.contains("release/HEAD"),
            "a branch that happens to end in HEAD is still a branch: {:?}",
            repo.names,
        );
        assert!(branch_ref_exists(dir.path(), "release/HEAD"), "and the delete gate agrees");
        // The symref itself is excluded from the population, its target
        // having been counted already.
        assert_eq!(repo.ref_count, 3, "local head + two remote branches: {:?}", repo.names);
    }

    /// A remote name may contain slashes - `git remote add gh/upstream`
    /// is accepted - so splitting the tail once leaves the remote's own
    /// tail on the front of the branch, and the live branch reads as
    /// absent. That is the only hazard in this area that points at
    /// DELETING state for a branch that still exists; every other one
    /// spares wrongly.
    #[test]
    fn a_slash_bearing_remote_still_yields_the_real_branch_name() {
        let dir = init_repo_with_commit();
        run_git(dir.path(), &["update-ref", "refs/remotes/gh/upstream/feat/pushed", "HEAD"]);
        run_git(dir.path(), &["update-ref", "refs/remotes/origin/feat/plain", "HEAD"]);

        let names = repo_branch_names(dir.path()).expect("a repo root answers").names;
        assert!(
            names.contains("feat/pushed"),
            "the branch behind a slash-bearing remote must not read as gone: {names:?}",
        );
        assert!(names.contains("feat/plain"), "the ordinary case still works: {names:?}");
        // Over-sparing is the deliberate direction: intermediate suffixes
        // join the alive set, which can only spare, never delete.
        assert!(names.contains("upstream/feat/pushed"), "{names:?}");
        assert!(names.contains("pushed"), "{names:?}");
    }

    /// Discovery walks up. Asking about a directory that merely sits
    /// inside a repo gets the ancestor's refs at exit 0, which for a
    /// caller deleting on absence means judging one project's rows
    /// against another repo's branches.
    #[test]
    fn repo_branch_names_refuses_a_directory_nested_inside_a_repo() {
        let dir = init_repo_with_commit();
        let nested = dir.path().join("crates");
        fs::create_dir_all(&nested).expect("mkdir");
        assert!(
            repo_branch_names(&nested).is_none(),
            "only the work-tree root answers for its own refs",
        );
        assert!(repo_branch_names(dir.path()).is_some(), "the root itself still answers");
    }

    #[test]
    fn repo_branch_names_is_none_outside_a_repo() {
        let dir = tempdir().expect("tempdir");
        assert!(repo_branch_names(dir.path()).is_none());
    }

    /// Pins the DEFENCE rather than only the hazard. The inherited case
    /// cannot be exercised in-process - the crate forbids `unsafe_code`,
    /// so a test cannot set `GIT_DIR` on itself - but `env_remove` beating
    /// an explicit `env` on the same `Command` is the same precedence that
    /// defeats inheritance, and it is what `git_in_repo` relies on.
    #[test]
    fn env_remove_beats_a_git_dir_set_on_the_same_command() {
        let host = init_repo_with_commit();
        run_git(host.path(), &["branch", "only-in-host"]);
        let foreign = init_repo_with_commit();
        run_git(foreign.path(), &["branch", "only-in-foreign"]);

        let out = Command::new("git")
            .arg("-C")
            .arg(host.path())
            .args(["for-each-ref", "--format=%(refname)", "refs/heads"])
            .env("GIT_DIR", foreign.path().join(".git"))
            .env_remove("GIT_DIR")
            .output()
            .expect("git runs");
        let listing = String::from_utf8_lossy(&out.stdout);

        assert!(out.status.success());
        assert!(
            listing.contains("only-in-host"),
            "the scrubbed command reads its -C repo: {listing:?}"
        );
        assert!(
            !listing.contains("only-in-foreign"),
            "and not the one GIT_DIR pointed at: {listing:?}",
        );
    }

    /// Pins the scrub at its CALL SITE. The precedence test above covers
    /// `env_remove` winning; this covers `git_in_repo` actually applying
    /// it, which nothing else does - all three calls can be deleted with
    /// the rest of the suite green.
    #[test]
    fn the_git_helper_scrubs_every_repo_location_variable() {
        let dir = tempdir().expect("tempdir");
        let command = scrubbed_git(dir.path(), &["rev-parse", "--show-toplevel"]);
        let removed: Vec<&str> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .filter_map(|(key, _)| key.to_str())
            .collect();
        for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"] {
            assert!(removed.contains(&key), "{key} is not scrubbed: {removed:?}");
        }
    }

    /// The hazard the scrub exists for: `-C` does not win against an
    /// inherited `GIT_DIR`, so an unscrubbed call answers about whatever
    /// repo the environment names.
    #[test]
    fn an_inherited_git_dir_beats_dash_c() {
        let foreign = init_repo_with_commit();
        run_git(foreign.path(), &["branch", "only-in-foreign"]);
        let elsewhere = tempdir().expect("tempdir");

        let out = Command::new("git")
            .env("GIT_DIR", foreign.path().join(".git"))
            .arg("-C")
            .arg(elsewhere.path())
            .args(["for-each-ref", "--format=%(refname)", "refs/heads"])
            .output()
            .expect("git runs");
        let listing = String::from_utf8_lossy(&out.stdout);

        assert!(out.status.success(), "the wrong answer arrives as a success");
        assert!(
            listing.contains("only-in-foreign"),
            "GIT_DIR overrides -C, so this reports the foreign repo's refs: {listing:?}",
        );
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

    #[test]
    fn branch_ref_exists_separates_present_from_absent() {
        let (dir, _wt, branch) = init_repo_with_worker_worktree("lbl");
        assert!(branch_ref_exists(dir.path(), &branch));
        assert!(!branch_ref_exists(dir.path(), "never-created"));
    }

    /// The reap counts a remote-tracking ref as reachability and so deletes
    /// a pushed local head. Existence has to count the same ref or the two
    /// disagree about a branch that is still open for review.
    #[test]
    fn branch_ref_exists_counts_a_branch_that_only_survives_on_a_remote() {
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

        assert!(!branch_exists(dir.path(), &branch), "the local head really is gone");
        assert!(
            branch_ref_exists(dir.path(), &branch),
            "a branch alive only on a remote still exists for review purposes",
        );
    }

    /// A slashed name must not let the remote-ref glob over-match.
    #[test]
    fn branch_ref_exists_handles_a_slashed_branch_name() {
        let (dir, wt, _branch) = init_repo_with_worker_worktree("lbl");
        let remote = tempdir().expect("remote tempdir");
        run_git(remote.path(), &["init", "-q", "--bare"]);
        run_git(&wt, &["remote", "add", "origin", remote.path().to_str().expect("utf8 path")]);
        run_git(&wt, &["checkout", "-q", "-b", "fix/deep/name"]);
        run_git(&wt, &["push", "-q", "-u", "origin", "fix/deep/name"]);
        run_git(&wt, &["checkout", "-q", "-"]);
        run_git(dir.path(), &["branch", "-D", "fix/deep/name"]);

        assert!(branch_ref_exists(dir.path(), "fix/deep/name"));
        assert!(!branch_ref_exists(dir.path(), "fix/deep"));
    }

    /// The existing coverage slashes the BRANCH name; this slashes the
    /// REMOTE, which is what the old `refs/remotes/*/<branch>` pattern
    /// could not see - `*` does not cross `/`. Empty stdout, exit 0, and
    /// on the despawn path empty is what deletes.
    #[test]
    fn branch_ref_exists_sees_a_branch_behind_a_slash_bearing_remote() {
        let dir = init_repo_with_commit();
        run_git(dir.path(), &["update-ref", "refs/remotes/gh/upstream/feat/pushed", "HEAD"]);
        assert!(
            branch_ref_exists(dir.path(), "feat/pushed"),
            "a branch surviving only on a slash-named remote is still present",
        );
        assert!(!branch_ref_exists(dir.path(), "feat/never-existed"));
    }

    /// A project path nested inside a repo cannot be judged, so it reads
    /// as present. Sharing the parser brings the work-tree-root check
    /// with it: previously this answered from the ancestor's refs and
    /// could delete on them.
    #[test]
    fn branch_ref_exists_reads_a_nested_path_as_present() {
        let dir = init_repo_with_commit();
        let nested = dir.path().join("crates");
        fs::create_dir_all(&nested).expect("mkdir");
        assert!(branch_ref_exists(&nested, "anything-at-all"));
    }

    #[test]
    fn branch_ref_exists_reads_an_unreadable_repo_as_present() {
        let dir = tempdir().expect("tempdir");
        assert!(
            branch_ref_exists(dir.path(), "worktree-lbl"),
            "a repo we cannot read must not read as a branch that is gone",
        );
    }
}
