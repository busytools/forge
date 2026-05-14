//! Git diff scanner — on-demand `git` subprocess invocations that
//! produce a [`GitDiffSnapshot`] describing the project's current
//! changeset.
//!
//! Polled (not watched): callers (`forge_workspace::Workspace::scan_git_diff`)
//! invoke [`scan`] from the TUI's `app::git_diff` ticker — a 1 s
//! poke runs the "snapshot is `None` OR age ≥ 10 s → fetch" rule
//! against the active session. The snapshot carries branch info
//! alongside diff stats, so this module replaces both the previous
//! file-watcher `GitContextWatcher` and the per-turn refresh hooks
//! that fed it.
//!
//! `scan` always returns a value. Subprocess failures, missing
//! repos, oversize output, and timeouts all collapse to
//! [`GitDiffView::NoRepo`]; the failure surfaces in the trace log
//! at WARN level so a real issue can be diagnosed without breaking
//! the rendering path.

use std::path::Path;
use std::time::Duration;

use forge_primitives::git::{GitBranch, GitIssueRef, GitPrInfo};
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

/// Max bytes accepted from `git diff --numstat`. A 1 MiB output is
/// already deep in pathological territory (~10k+ files); past that
/// we bail rather than allocate.
const STDOUT_SIZE_CAP: usize = 1024 * 1024;

/// Per-subprocess timeout. Worst case across the full scan
/// sequence is ~50s (5 commands × 10s each), but in practice every
/// command returns in <100 ms on a local repo. Timeouts only fire
/// on hung network mounts or genuinely huge repositories.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Number of file rows surfaced in the rendered diff list. The
/// renderer collapses the rest into a "+N more" line. With the
/// box-drawing tree + single-child folding in `inspector_pane`,
/// 7 file leaves typically render as ~9-11 rows including
/// directory headers — matches the TASKS / PROCESSES section caps
/// (5) and keeps the GIT section roughly the same height as its
/// neighbours on tall trees.
const TOP_FILE_COUNT: usize = 7;

/// Snapshot of one project's git state, suitable for rendering in
/// the Inspector pane's GIT section. Branch info is folded in here
/// so a single polled scan covers everything the renderer needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffSnapshot {
    /// Current branch (Named / Detached / NoRepo / Unknown).
    pub branch: GitBranch,
    /// Resolved default branch (e.g. `main`, `master`), if known.
    /// `None` when `origin/HEAD` is missing AND neither `main` nor
    /// `master` exists as a local ref.
    pub default_branch: Option<String>,
    pub view: GitDiffView,
    /// Open pull request for the current branch, if one exists. Only
    /// populated for `Named` non-default branches; `None` otherwise.
    /// Cached across scans by branch name — refetched only when the
    /// branch changes (see [`scan`]'s `prev` parameter).
    pub pr: Option<GitPrInfo>,
    /// Issues the open PR closes (from GitHub's
    /// `closingIssuesReferences`). Empty when there's no PR or the
    /// PR doesn't reference any issues. Cached alongside `pr`.
    pub closes: Vec<GitIssueRef>,
}

/// What flavour of diff the snapshot represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitDiffView {
    /// Default branch + clean working tree, OR detached HEAD + clean.
    /// Renderer shows path + branch only — no subtitle, no file list.
    CleanDefault,
    /// Working tree has uncommitted changes (any branch, including
    /// detached HEAD). Renderer shows `worktree` subtitle + overall
    /// `+N -M` totals + per-file rows.
    Worktree { files: Vec<GitDiffFile>, total_files: usize, total_added: u32, total_removed: u32 },
    /// Feature branch + clean working tree. Renderer shows
    /// `vs <default>` subtitle + overall `+N -M` totals + per-file
    /// rows (commits on this branch relative to the default branch).
    BranchVsDefault {
        files: Vec<GitDiffFile>,
        total_files: usize,
        total_added: u32,
        total_removed: u32,
    },
    /// Cwd is not in a git repo, OR the scan failed (subprocess
    /// error, timeout, oversize output, …). Renderer shows path
    /// only.
    NoRepo,
}

/// One file's diff stats. `added` / `removed` are git's `--numstat`
/// line counts; binary files (which numstat reports as `-`) are
/// dropped by the parser rather than appearing here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffFile {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

/// Run the full scan sequence against `cwd` and return a snapshot.
/// Always succeeds — every failure path collapses to
/// [`GitDiffView::NoRepo`] with a WARN log naming the step that
/// failed. Callers should treat the snapshot as authoritative for
/// rendering regardless of which variant came back.
///
/// `prev` is the most recent snapshot for this `cwd` (if any). It's
/// used to reuse cached PR info: when `prev.branch` matches the
/// newly-resolved branch (same `Named(name)`), the prior
/// `pr` / `closes` carry over without re-running `gh`. Pass `None`
/// for cold starts.
#[must_use]
pub async fn scan(cwd: &Path, prev: Option<&GitDiffSnapshot>) -> GitDiffSnapshot {
    let raw_branch = match run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
        GitOutput::Ok(s) => s.trim().to_owned(),
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => {
            return GitDiffSnapshot {
                branch: GitBranch::NoRepo,
                default_branch: None,
                view: GitDiffView::NoRepo,
                pr: None,
                closes: Vec::new(),
            };
        }
    };
    let detached = raw_branch == "HEAD";
    let branch = if detached {
        GitBranch::Detached
    } else if raw_branch.is_empty() {
        GitBranch::Unknown
    } else {
        GitBranch::Named(raw_branch.clone())
    };

    let default_branch = resolve_default_branch(cwd).await;
    let on_default = default_branch.as_ref().is_some_and(|default| default == &raw_branch);
    let dirty = is_worktree_dirty(cwd).await;

    // View resolution mirrors the previous early-return ladder, just
    // collapsed into a single binding so the final snapshot is built
    // once at the bottom alongside `pr` / `closes`.
    let view = if detached {
        if dirty {
            let stats = numstat(cwd, &["diff", "--numstat", "HEAD"]).await;
            GitDiffView::Worktree {
                files: stats.files,
                total_files: stats.total_files,
                total_added: stats.total_added,
                total_removed: stats.total_removed,
            }
        } else {
            GitDiffView::CleanDefault
        }
    } else if dirty {
        let stats = numstat(cwd, &["diff", "--numstat", "HEAD"]).await;
        GitDiffView::Worktree {
            files: stats.files,
            total_files: stats.total_files,
            total_added: stats.total_added,
            total_removed: stats.total_removed,
        }
    } else if on_default {
        GitDiffView::CleanDefault
    } else if let Some(default) = default_branch.as_deref() {
        // Feature branch + clean → branch-vs-default diff. Skip if
        // the default branch is unknown (no sensible base to diff
        // against — collapses to CleanDefault).
        let range = format!("{default}...HEAD");
        let stats = numstat(cwd, &["diff", "--numstat", &range]).await;
        GitDiffView::BranchVsDefault {
            files: stats.files,
            total_files: stats.total_files,
            total_added: stats.total_added,
            total_removed: stats.total_removed,
        }
    } else {
        GitDiffView::CleanDefault
    };

    // PR / closes only make sense for named non-default branches —
    // default branch never has a PR open against itself, detached /
    // unknown branches can't be queried by name. For eligible
    // branches, reuse the prior snapshot's data when the branch name
    // matches; otherwise spawn a fresh `gh pr list` call.
    let (pr, closes) = match &branch {
        GitBranch::Named(name) if !on_default => pr_for_branch(cwd, name, prev).await,
        _ => (None, Vec::new()),
    };

    GitDiffSnapshot { branch, default_branch, view, pr, closes }
}

/// Per-scan aggregate the renderer needs. `files` is the top-N
/// trimmed for display; `total_*` cover ALL parsed files (binary
/// files excluded — they don't have meaningful line counts).
struct NumstatResult {
    files: Vec<GitDiffFile>,
    total_files: usize,
    total_added: u32,
    total_removed: u32,
}

/// `git symbolic-ref --short refs/remotes/origin/HEAD` with `main`
/// → `master` fallback. Returns `None` if no default can be
/// resolved (no remote HEAD, no `main`, no `master`).
async fn resolve_default_branch(cwd: &Path) -> Option<String> {
    match run_git(cwd, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await {
        GitOutput::Ok(s) => {
            // Output looks like `origin/main` — strip the remote
            // prefix to leave just the branch name.
            let trimmed = s.trim();
            return Some(trimmed.strip_prefix("origin/").unwrap_or(trimmed).to_owned());
        }
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => {}
    }
    for candidate in ["main", "master"] {
        if let GitOutput::Ok(_) = run_git(cwd, &["rev-parse", "--verify", candidate]).await {
            return Some(candidate.to_owned());
        }
    }
    // No `origin/HEAD`, no `main`, no `master`. A feature-branch
    // diff has no meaningful base, so the renderer collapses to
    // `CleanDefault` — meaning the user sees branch + path with no
    // diff stats even when the branch has commits relative to some
    // other default (e.g. `develop`). Surfacing WARN here so the
    // failure is grep-able when the user reports "the GIT section
    // never shows my branch's diff".
    tracing::warn!(
        target: crate::logging::targets::ENV_GIT,
        cwd = %cwd.display(),
        event_name = "git_default_branch_unknown",
        message = "default branch fallback exhausted (no origin/HEAD, main, or master)",
        outcome = "skipped",
    );
    None
}

async fn is_worktree_dirty(cwd: &Path) -> bool {
    match run_git(cwd, &["status", "--porcelain=v1", "--untracked-files=no"]).await {
        GitOutput::Ok(s) => !s.trim().is_empty(),
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => false,
    }
}

/// Run `git <args>` and parse the `--numstat` output. Returns the
/// top-`TOP_FILE_COUNT` files (sorted by total changes desc, alpha
/// tie-break) plus the full file count and overall add/remove
/// totals.
async fn numstat(cwd: &Path, args: &[&str]) -> NumstatResult {
    let raw = match run_git(cwd, args).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty => String::new(),
        GitOutput::Failed | GitOutput::Oversize => {
            return NumstatResult {
                files: Vec::new(),
                total_files: 0,
                total_added: 0,
                total_removed: 0,
            };
        }
    };
    let mut files = parse_numstat(&raw);
    let total_files = files.len();
    let total_added: u32 = files.iter().fold(0u32, |acc, f| acc.saturating_add(f.added));
    let total_removed: u32 = files.iter().fold(0u32, |acc, f| acc.saturating_add(f.removed));
    files.sort_by(|a, b| {
        let a_total = a.added.saturating_add(a.removed);
        let b_total = b.added.saturating_add(b.removed);
        b_total.cmp(&a_total).then_with(|| a.path.cmp(&b.path))
    });
    files.truncate(TOP_FILE_COUNT);
    NumstatResult { files, total_files, total_added, total_removed }
}

/// Parse `<added>\t<removed>\t<path>` lines. Skips binary entries
/// (`added` or `removed` reported as `-`). Uses `splitn(3, '\t')`
/// so paths containing tabs survive intact.
fn parse_numstat(raw: &str) -> Vec<GitDiffFile> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?;
            let removed = parts.next()?;
            let path = parts.next()?;
            if added == "-" || removed == "-" {
                return None;
            }
            let added = added.parse::<u32>().ok()?;
            let removed = removed.parse::<u32>().ok()?;
            Some(GitDiffFile { path: path.to_owned(), added, removed })
        })
        .collect()
}

/// Resolve PR info for a named branch, reusing `prev`'s cached
/// `pr` / `closes` when the prior snapshot's branch matches `branch`.
/// Otherwise spawns `gh pr list` to fetch fresh.
///
/// The cache key is the branch name. The implication: branch
/// rename / new branch / fresh session all force a refetch, but
/// staying on the same branch across the polled 10s scans is free.
async fn pr_for_branch(
    cwd: &Path,
    branch: &str,
    prev: Option<&GitDiffSnapshot>,
) -> (Option<GitPrInfo>, Vec<GitIssueRef>) {
    if let Some(prev) = prev
        && let GitBranch::Named(prev_name) = &prev.branch
        && prev_name == branch
    {
        return (prev.pr.clone(), prev.closes.clone());
    }
    fetch_pr_for_branch(cwd, branch).await
}

/// Shell out to `gh pr list --head <branch> --state open --json …`
/// and parse the first entry. Returns `(None, vec![])` on any
/// failure path — `gh` missing, unauthenticated, not a github
/// repo, no PR for the branch, JSON parse error. Failures log
/// at WARN with a structured event so operators can grep for
/// `gh_pr_lookup_*` when triaging "PR row never shows".
async fn fetch_pr_for_branch(cwd: &Path, branch: &str) -> (Option<GitPrInfo>, Vec<GitIssueRef>) {
    let args = [
        "pr",
        "list",
        "--head",
        branch,
        "--state",
        "open",
        "--limit",
        "1",
        "--json",
        "number,url,closingIssuesReferences",
    ];
    let raw = match run_gh(cwd, &args).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty => {
            // `gh` returned exit 0 with empty stdout — unusual for
            // `--json`, which always emits at least `[]`. Treat as
            // no PR rather than a hard failure.
            return (None, Vec::new());
        }
        GitOutput::Failed | GitOutput::Oversize => return (None, Vec::new()),
    };
    let entries: Vec<GhPrEntry> = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "gh_pr_lookup_parse_failed",
                message = "gh pr list returned unparseable json",
                outcome = "failure",
                error = %err,
                branch = %branch,
            );
            return (None, Vec::new());
        }
    };
    let Some(first) = entries.into_iter().next() else {
        // Empty array — no open PR for this branch. Cache as None so
        // subsequent scans on the same branch skip the gh call.
        return (None, Vec::new());
    };
    let pr = GitPrInfo { number: first.number, url: first.url };
    let closes = first
        .closing_issues
        .into_iter()
        .map(|issue| GitIssueRef { number: issue.number, url: issue.url })
        .collect();
    (Some(pr), closes)
}

/// `gh pr list --json` entry shape. Only the fields we actually
/// render are deserialised; `gh` adds others (`id`, `title`, …) that
/// serde silently drops.
#[derive(Deserialize)]
struct GhPrEntry {
    number: u64,
    url: String,
    #[serde(default, rename = "closingIssuesReferences")]
    closing_issues: Vec<GhIssueEntry>,
}

/// `closingIssuesReferences` element shape. Same selective-field
/// pattern as `GhPrEntry`.
#[derive(Deserialize)]
struct GhIssueEntry {
    number: u64,
    url: String,
}

/// Result of one `git` subprocess invocation. Callers treat all
/// failure variants the same way (collapse to `NoRepo` / zero
/// stats), but the variants are split so the WARN log captures the
/// right context — `Failed` means non-zero exit with stderr that an
/// operator might need; `Empty` is a legitimate "ran fine, no
/// output" signal (the common case for `status --porcelain` on a
/// clean tree).
enum GitOutput {
    Ok(String),
    /// Exit 0, empty stdout (legitimate "no output to report" case).
    Empty,
    /// Timeout, spawn error, interrupted, or non-zero exit. Logged
    /// at WARN with stderr (truncated) inside `run_git` so an
    /// operator can diagnose without reproducing.
    Failed,
    /// Stdout exceeded [`STDOUT_SIZE_CAP`].
    Oversize,
}

/// Cap on captured stderr surfaced into the WARN log. Far below the
/// stdout cap because stderr is conversational — a couple of
/// `fatal:` lines is more than enough context.
const STDERR_LOG_CAP: usize = 1024;

/// Spawn `git <args>` against `cwd`, await with a per-command
/// timeout, return classified output. Non-zero exits log WARN with
/// the captured stderr so operators can distinguish "clean tree"
/// from "corrupt index / permissions / fatal: …" without re-running.
async fn run_git(cwd: &Path, args: &[&str]) -> GitOutput {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(args).kill_on_drop(true);
    let fut = command.output();
    let output = match timeout(COMMAND_TIMEOUT, fut).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "git_subprocess_failed",
                message = "git subprocess spawn / wait failed",
                outcome = "failure",
                error = %err,
                args = ?args,
            );
            return GitOutput::Failed;
        }
        Err(_) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "git_subprocess_timeout",
                message = "git subprocess timed out",
                outcome = "timeout",
                args = ?args,
            );
            return GitOutput::Failed;
        }
    };
    if !output.status.success() {
        // Non-zero exit. The renderer still collapses to NoRepo /
        // zero stats — keep the surface failure-tolerant — but log
        // the exit code + truncated stderr so an operator can tell
        // "git: command not found" / "fatal: not a git repository" /
        // "fatal: index file corrupt" apart without reproducing.
        // Cwd-not-in-a-repo is one of the legitimate hits here; the
        // log volume stays low because `is_worktree_dirty` is the
        // only command in the sequence that runs unconditionally
        // (the rest are gated on `rev-parse --abbrev-ref HEAD`
        // succeeding).
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_truncated = if stderr.len() > STDERR_LOG_CAP {
            format!("{}…", &stderr[..STDERR_LOG_CAP])
        } else {
            stderr.into_owned()
        };
        tracing::warn!(
            target: crate::logging::targets::ENV_GIT,
            cwd = %cwd.display(),
            event_name = "git_subprocess_nonzero_exit",
            message = "git subprocess exited non-zero",
            outcome = "failure",
            exit_code = output.status.code().unwrap_or(-1),
            stderr = %stderr_truncated,
            args = ?args,
        );
        return GitOutput::Failed;
    }
    if output.stdout.len() > STDOUT_SIZE_CAP {
        tracing::warn!(
            target: crate::logging::targets::ENV_GIT,
            cwd = %cwd.display(),
            event_name = "git_subprocess_oversize",
            message = "git subprocess stdout exceeded size cap",
            outcome = "oversize",
            bytes = output.stdout.len(),
            args = ?args,
        );
        return GitOutput::Oversize;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if stdout.trim().is_empty() { GitOutput::Empty } else { GitOutput::Ok(stdout) }
}

/// Spawn `gh <args>` from `cwd` (gh derives the github repo from
/// the current working directory — there's no `-C` equivalent).
/// Mirrors [`run_git`]'s timeout / classification / WARN logging so
/// failures distinguish "gh: command not found" (binary missing)
/// from "gh: To use GitHub CLI in a Git repository, please run …"
/// (not a github remote) from "no pull requests found" (legitimate
/// empty result) when triaging "PR row never shows".
async fn run_gh(cwd: &Path, args: &[&str]) -> GitOutput {
    let mut command = Command::new("gh");
    command.current_dir(cwd).args(args).kill_on_drop(true);
    let fut = command.output();
    let output = match timeout(COMMAND_TIMEOUT, fut).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "gh_subprocess_failed",
                message = "gh subprocess spawn / wait failed",
                outcome = "failure",
                error = %err,
                args = ?args,
            );
            return GitOutput::Failed;
        }
        Err(_) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "gh_subprocess_timeout",
                message = "gh subprocess timed out",
                outcome = "timeout",
                args = ?args,
            );
            return GitOutput::Failed;
        }
    };
    if !output.status.success() {
        // gh exits non-zero on: missing auth (4), not a github
        // remote (1), API error (1). All collapse to "no PR" for
        // the renderer, but the log captures stderr so an operator
        // can tell which case fired.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_truncated = if stderr.len() > STDERR_LOG_CAP {
            format!("{}…", &stderr[..STDERR_LOG_CAP])
        } else {
            stderr.into_owned()
        };
        tracing::warn!(
            target: crate::logging::targets::ENV_GIT,
            cwd = %cwd.display(),
            event_name = "gh_subprocess_nonzero_exit",
            message = "gh subprocess exited non-zero",
            outcome = "failure",
            exit_code = output.status.code().unwrap_or(-1),
            stderr = %stderr_truncated,
            args = ?args,
        );
        return GitOutput::Failed;
    }
    if output.stdout.len() > STDOUT_SIZE_CAP {
        tracing::warn!(
            target: crate::logging::targets::ENV_GIT,
            cwd = %cwd.display(),
            event_name = "gh_subprocess_oversize",
            message = "gh subprocess stdout exceeded size cap",
            outcome = "oversize",
            bytes = output.stdout.len(),
            args = ?args,
        );
        return GitOutput::Oversize;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if stdout.trim().is_empty() { GitOutput::Empty } else { GitOutput::Ok(stdout) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn init_repo(dir: &TempDir, initial_branch: &str) {
        let path = dir.path();
        // The `git init -b <branch>` flag was added in git 2.28; CI
        // runners can still be on 2.25 (Ubuntu 20.04's default). To
        // stay compatible we `init` without `-b` and then point HEAD
        // at the desired branch via `symbolic-ref` — works on every
        // git version, including ones where `init.defaultBranch`
        // isn't recognised. Each call asserts `status.success()` so
        // a silent setup failure surfaces here rather than confusing
        // the assertion downstream.
        let run = |args: &[&str]| {
            let out =
                StdCommand::new("git").arg("-C").arg(path).args(args).output().expect("git ok");
            assert!(
                out.status.success(),
                "git {:?} failed in {}: stderr={}",
                args,
                path.display(),
                String::from_utf8_lossy(&out.stderr),
            );
        };
        run(&["init", "-q"]);
        run(&["symbolic-ref", "HEAD", &format!("refs/heads/{initial_branch}")]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "commit.gpgsign", "false"]);
    }

    fn write_file(dir: &TempDir, name: &str, contents: &str) {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, contents).expect("write");
    }

    fn commit_all(dir: &TempDir, message: &str) {
        let path = dir.path();
        let run = |args: &[&str]| {
            StdCommand::new("git").arg("-C").arg(path).args(args).output().expect("git ok");
        };
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", message]);
    }

    #[test]
    fn parse_numstat_handles_added_removed() {
        let raw = "12\t3\tsrc/foo.rs\n5\t0\tsrc/bar.rs\n";
        let parsed = parse_numstat(raw);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path, "src/foo.rs");
        assert_eq!(parsed[0].added, 12);
        assert_eq!(parsed[0].removed, 3);
        assert_eq!(parsed[1].added, 5);
        assert_eq!(parsed[1].removed, 0);
    }

    #[test]
    fn parse_numstat_skips_binary_files() {
        let raw = "-\t-\tbin/blob.png\n4\t1\tsrc/foo.rs\n";
        let parsed = parse_numstat(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].path, "src/foo.rs");
    }

    #[test]
    fn parse_numstat_handles_malformed_lines() {
        let raw = "12 missing tabs\n\n5\tnope\n4\t1\tsrc/foo.rs\n";
        let parsed = parse_numstat(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].path, "src/foo.rs");
    }

    #[test]
    fn parse_numstat_keeps_tabs_inside_path_via_splitn() {
        // `splitn(3, '\t')` should leave the rest-of-line intact as
        // the path, including any tabs inside it.
        let raw = "4\t1\tsrc/weird\tpath.rs\n";
        let parsed = parse_numstat(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].path, "src/weird\tpath.rs");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_no_repo_returns_no_repo_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = scan(dir.path(), None).await;
        assert!(matches!(snap.view, GitDiffView::NoRepo));
        assert!(snap.default_branch.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_clean_default_branch_returns_clean_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "hello\n");
        commit_all(&dir, "init");
        let snap = scan(dir.path(), None).await;
        assert!(matches!(snap.view, GitDiffView::CleanDefault), "got {:?}", snap.view);
        assert_eq!(snap.default_branch.as_deref(), Some("main"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_dirty_default_branch_returns_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "first\n");
        commit_all(&dir, "init");
        // Dirty: modify the tracked file without committing.
        write_file(&dir, "README.md", "second\nthird\n");
        let snap = scan(dir.path(), None).await;
        let GitDiffView::Worktree { files, total_files, total_added, total_removed } = snap.view
        else {
            panic!("expected Worktree, got {:?}", snap.view);
        };
        assert_eq!(total_files, 1);
        assert_eq!(files[0].path, "README.md");
        assert!(files[0].added >= 1);
        // Totals mirror the single file's stats since it's the only
        // change.
        assert_eq!(total_added, files[0].added);
        assert_eq!(total_removed, files[0].removed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_clean_feature_branch_returns_branch_vs_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "first\n");
        commit_all(&dir, "init");
        let run = |args: &[&str]| {
            StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git ok");
        };
        run(&["checkout", "-q", "-b", "feat/x"]);
        write_file(&dir, "feat.rs", "fn x() {}\n");
        commit_all(&dir, "feat commit");

        let snap = scan(dir.path(), None).await;
        let GitDiffView::BranchVsDefault { files, total_files, total_added, total_removed } =
            snap.view
        else {
            panic!("expected BranchVsDefault, got {:?}", snap.view);
        };
        assert_eq!(total_files, 1);
        assert_eq!(files[0].path, "feat.rs");
        assert_eq!(snap.default_branch.as_deref(), Some("main"));
        assert_eq!(total_added, files[0].added);
        assert_eq!(total_removed, files[0].removed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_dirty_feature_branch_returns_worktree_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "first\n");
        commit_all(&dir, "init");
        let run = |args: &[&str]| {
            StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git ok");
        };
        run(&["checkout", "-q", "-b", "feat/x"]);
        write_file(&dir, "feat.rs", "fn x() {}\n");
        commit_all(&dir, "feat commit");
        // Dirty the worktree AFTER the feature commit. The
        // expectation is worktree-only (no vs-default mixing).
        write_file(&dir, "feat.rs", "fn x() {}\nfn y() {}\n");

        let snap = scan(dir.path(), None).await;
        let GitDiffView::Worktree { files, total_files, total_added: _, total_removed: _ } =
            snap.view
        else {
            panic!("expected Worktree, got {:?}", snap.view);
        };
        assert_eq!(total_files, 1);
        assert_eq!(files[0].path, "feat.rs");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_detached_dirty_returns_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "first\n");
        commit_all(&dir, "init");
        write_file(&dir, "README.md", "second\n");
        commit_all(&dir, "second");
        let run = |args: &[&str]| {
            StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git ok");
        };
        run(&["checkout", "-q", "HEAD~1"]);
        // Dirty in detached state.
        write_file(&dir, "README.md", "third\n");

        let snap = scan(dir.path(), None).await;
        assert!(
            matches!(snap.view, GitDiffView::Worktree { .. }),
            "expected Worktree, got {:?}",
            snap.view,
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_detached_clean_returns_clean_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "first\n");
        commit_all(&dir, "init");
        write_file(&dir, "README.md", "second\n");
        commit_all(&dir, "second");
        let run = |args: &[&str]| {
            StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git ok");
        };
        run(&["checkout", "-q", "HEAD~1"]);
        let snap = scan(dir.path(), None).await;
        assert!(matches!(snap.view, GitDiffView::CleanDefault), "got {:?}", snap.view);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_dirty_default_branch_aggregates_totals_across_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "a.txt", "1\n2\n3\n");
        write_file(&dir, "b.txt", "1\n");
        commit_all(&dir, "init");
        // Add a line to a.txt, replace b.txt with a longer body, add
        // a new tracked file. Total adds should sum across all
        // changed files.
        write_file(&dir, "a.txt", "1\n2\n3\n4\n");
        write_file(&dir, "b.txt", "x\ny\nz\n");
        write_file(&dir, "c.txt", "new\n");
        // Stage c.txt so it shows up in `git diff HEAD`.
        StdCommand::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["add", "c.txt"])
            .output()
            .expect("git ok");

        let snap = scan(dir.path(), None).await;
        let GitDiffView::Worktree { files, total_files, total_added, total_removed } = snap.view
        else {
            panic!("expected Worktree, got {:?}", snap.view);
        };
        assert_eq!(total_files, 3);
        // Sum of per-file added/removed equals the overall total.
        let per_file_added: u32 = files.iter().map(|f| f.added).sum();
        let per_file_removed: u32 = files.iter().map(|f| f.removed).sum();
        assert_eq!(total_added, per_file_added);
        assert_eq!(total_removed, per_file_removed);
        // At least one row in each direction (we added 1+3+1 lines
        // and removed 1).
        assert!(total_added >= 5);
        assert!(total_removed >= 1);
    }

    #[test]
    fn top_files_sort_by_total_changes_then_alpha() {
        let mut files = [
            GitDiffFile { path: "small.rs".into(), added: 1, removed: 0 },
            GitDiffFile { path: "big-b.rs".into(), added: 50, removed: 10 },
            GitDiffFile { path: "big-a.rs".into(), added: 50, removed: 10 },
            GitDiffFile { path: "medium.rs".into(), added: 10, removed: 5 },
        ];
        files.sort_by(|a, b| {
            let a_total = a.added.saturating_add(a.removed);
            let b_total = b.added.saturating_add(b.removed);
            b_total.cmp(&a_total).then_with(|| a.path.cmp(&b.path))
        });
        assert_eq!(files[0].path, "big-a.rs");
        assert_eq!(files[1].path, "big-b.rs");
        assert_eq!(files[2].path, "medium.rs");
        assert_eq!(files[3].path, "small.rs");
    }

    /// `pr_for_branch` short-circuits the `gh pr list` call when
    /// `prev`'s branch matches the requested branch — even when the
    /// `cwd` would make a real `gh` invocation fail (tempdir has no
    /// git remote). The returned `pr` / `closes` MUST be clones of
    /// `prev`'s, proving the cache hit.
    #[tokio::test(flavor = "current_thread")]
    async fn pr_for_branch_reuses_prev_when_named_branch_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pr = GitPrInfo { number: 42, url: "https://example/pull/42".into() };
        let closes = vec![GitIssueRef { number: 7, url: "https://example/issues/7".into() }];
        let prev = GitDiffSnapshot {
            branch: GitBranch::Named("feat/x".into()),
            default_branch: Some("main".into()),
            view: GitDiffView::CleanDefault,
            pr: Some(pr.clone()),
            closes: closes.clone(),
        };

        let (got_pr, got_closes) = pr_for_branch(dir.path(), "feat/x", Some(&prev)).await;
        assert_eq!(got_pr, Some(pr));
        assert_eq!(got_closes, closes);
    }

    /// Cache miss when the requested branch differs from `prev`'s.
    /// `cwd` here isn't a github repo, so `gh pr list` collapses to
    /// `(None, vec![])` whether or not `gh` is installed — that's the
    /// expected miss outcome.
    #[tokio::test(flavor = "current_thread")]
    async fn pr_for_branch_refetches_when_branch_differs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = GitDiffSnapshot {
            branch: GitBranch::Named("feat/x".into()),
            default_branch: Some("main".into()),
            view: GitDiffView::CleanDefault,
            pr: Some(GitPrInfo { number: 42, url: "https://example/pull/42".into() }),
            closes: Vec::new(),
        };

        let (got_pr, got_closes) = pr_for_branch(dir.path(), "feat/y", Some(&prev)).await;
        assert_eq!(got_pr, None, "cache must not apply across branches");
        assert!(got_closes.is_empty());
    }

    /// Cache miss when `prev`'s branch is non-Named (Detached /
    /// NoRepo / Unknown). Defensive: by construction prev shouldn't
    /// carry pr data in those states, but we shouldn't trust the
    /// invariant from the cache path.
    #[tokio::test(flavor = "current_thread")]
    async fn pr_for_branch_refetches_when_prev_is_detached() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = GitDiffSnapshot {
            branch: GitBranch::Detached,
            default_branch: Some("main".into()),
            view: GitDiffView::CleanDefault,
            pr: Some(GitPrInfo { number: 42, url: "url".into() }),
            closes: Vec::new(),
        };

        let (got_pr, _got_closes) = pr_for_branch(dir.path(), "feat/x", Some(&prev)).await;
        assert_eq!(got_pr, None);
    }

    /// `scan` carries cached PR data through the cache-hit path. Set
    /// up a real git repo on a feature branch, build a `prev`
    /// snapshot for the same branch with a synthetic PR, and assert
    /// the returned snapshot mirrors prev's PR fields. Without the
    /// cache, `scan` would call `gh` against a non-github tempdir
    /// and the PR fields would come back empty.
    #[tokio::test(flavor = "current_thread")]
    async fn scan_carries_prev_pr_through_when_branch_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "first\n");
        commit_all(&dir, "init");
        let run = |args: &[&str]| {
            StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git ok");
        };
        run(&["checkout", "-q", "-b", "feat/cache"]);
        write_file(&dir, "feat.rs", "fn x() {}\n");
        commit_all(&dir, "feat commit");

        let synthetic_pr = GitPrInfo { number: 99, url: "https://example/pull/99".into() };
        let synthetic_closes =
            vec![GitIssueRef { number: 1, url: "https://example/issues/1".into() }];
        let prev = GitDiffSnapshot {
            branch: GitBranch::Named("feat/cache".into()),
            default_branch: Some("main".into()),
            view: GitDiffView::CleanDefault,
            pr: Some(synthetic_pr.clone()),
            closes: synthetic_closes.clone(),
        };

        let snap = scan(dir.path(), Some(&prev)).await;
        assert_eq!(snap.pr, Some(synthetic_pr), "cached PR must carry through scan");
        assert_eq!(snap.closes, synthetic_closes);
    }

    /// On the default branch, `scan` skips the PR fetch entirely —
    /// `pr` / `closes` stay empty. Mirrors the "no PR opens against
    /// main" assumption (true for personal repos; fork → upstream
    /// edge case is out of scope for v1).
    #[tokio::test(flavor = "current_thread")]
    async fn scan_skips_pr_fetch_on_default_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "hello\n");
        commit_all(&dir, "init");

        let prev = GitDiffSnapshot {
            branch: GitBranch::Named("main".into()),
            default_branch: Some("main".into()),
            view: GitDiffView::CleanDefault,
            pr: Some(GitPrInfo { number: 1, url: "url".into() }),
            closes: Vec::new(),
        };
        // Even with a cache-hit-shaped prev, the default-branch gate
        // wins and the PR field clears.
        let snap = scan(dir.path(), Some(&prev)).await;
        assert_eq!(snap.pr, None);
        assert!(snap.closes.is_empty());
    }
}
