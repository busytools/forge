//! Git diff scanner - on-demand `git` subprocess invocations that
//! produce a [`GitDiffSnapshot`] describing the project's current
//! changeset.
//!
//! Polled (not watched): the TUI reaches [`scan`] as
//! `forge_workspace::env::git_diff::scan`, a wildcard re-export of this
//! module through the workspace facade, and drives it from its
//! `app::git_diff` ticker - a 1 s
//! poke runs the "snapshot is `None` OR age ≥ 10 s → fetch" rule
//! against the active session. The snapshot carries branch info
//! alongside diff stats, so this module replaces both the previous
//! file-watcher `GitContextWatcher` and the per-turn refresh hooks
//! that fed it.
//!
//! The snapshot exposes two independent diff layers so callers can
//! render both simultaneously when both apply (worker on a topic
//! branch with uncommitted edits, for instance). Each layer is a
//! [`LayerState`] with three variants:
//! - `worktree`: uncommitted edits vs HEAD (the dirty tree).
//!   `Clean` when the tree is clean.
//! - `branch_ahead`: the branch's commits ahead of the default
//!   branch. `Clean` on the default branch, on detached HEAD, or
//!   when there's no resolvable default. `ScanFailed` carries the
//!   per-layer subprocess-error signal.
//!
//! `scan` always returns a value. Subprocess failures, missing
//! repos, oversize output, and timeouts all collapse to a non-InRepo
//! `repo_gate` (`NotARepo` / `ScannerFailed`); the failure surfaces in
//! the trace log at WARN level so a real issue can be diagnosed
//! without breaking the rendering path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use forge_primitives::git::{GitBranch, GitIssueRef, GitPrInfo};
use forge_primitives::git_diff::{
    GitBranchAhead, GitDiffFile, GitDiffSnapshot, GitDiffStats, LayerState, RepoGate,
};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

pub mod hunks;
pub mod resolver;

/// Max bytes accepted from any single `git` subprocess invocation.
/// `--numstat` is small in practice; the per-file `git diff <target>
/// --no-ext-diff -- <path>` calls produced by [`hunks::scan`] are
/// each bounded by one file's worth of changes. 8 MiB is generous
/// headroom for genuinely huge single-file diffs (lockfiles,
/// generated source, vendored snapshots) without exposing the scan
/// to unbounded memory growth on a corrupted-git-output edge case.
const STDOUT_SIZE_CAP: usize = 8 * 1024 * 1024;

/// Per-subprocess timeout. Worst case across the full scan
/// sequence is ~50s (5 commands × 10s each), but in practice every
/// command returns in <100 ms on a local repo. Timeouts only fire
/// on hung network mounts or genuinely huge repositories.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Number of file rows surfaced in the rendered diff list. The
/// renderer collapses the rest into a "+N more" line. With the
/// box-drawing tree + single-child folding in `inspector_pane`,
/// 7 file leaves typically render as ~9-11 rows including
/// directory headers - matches the TASKS / PROCESSES section caps
/// (5) and keeps the GIT section roughly the same height as its
/// neighbours on tall trees.
const TOP_FILE_COUNT: usize = 7;

/// Whether a git repository is reachable from a scan cwd, and whether
/// git could answer the question at all. The third variant matters:
/// "git told me there is no repo" and "git would not run" must not
/// collapse together, or a missing git binary reads as a non-repo and
/// silently suppresses the GIT section instead of reporting itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoPresence {
    /// A repo exists here, even if it has no commits yet.
    Present,
    /// git ran, reported no repository, and there is no `.git` on disk
    /// to contradict it.
    Absent,
    /// git could not be run, or it refused a checkout that does have a
    /// `.git`. Either way the question went unanswered, so the caller
    /// must report rather than suppress.
    Unusable,
}

/// Ask whether `cwd` sits inside a git repo with `rev-parse --git-dir`,
/// whose exit status answers repo-existence alone: zero inside any work
/// tree including one with no commits, non-zero outside one and for a
/// path that does not exist. Matching git's `fatal:` prose would not
/// work here even setting version drift aside - a deleted `.git/HEAD`
/// and a plain non-repo emit the same line.
///
/// A non-zero exit is then refined by whether `.git` is on disk: a
/// pruned worktree entry, a deleted `HEAD` and a corrupt gitfile all
/// exit 128 with `.git` still present, and each is a fault to report
/// rather than an absent repo. Undeterminable biases to `Unusable`, so
/// a broken checkout is never silently suppressed.
///
/// Not routed through [`run_git`]: a non-zero exit is the answer here
/// rather than a fault, so it must neither be logged as a failure nor
/// collapsed with "git would not run", which the caller has already
/// logged with git's stderr.
async fn repo_presence(cwd: &Path) -> RepoPresence {
    let mut command = Command::new("git");
    command.arg("-C").arg(cwd).args(["rev-parse", "--git-dir"]).kill_on_drop(true);
    match timeout(COMMAND_TIMEOUT, command.output()).await {
        Ok(Ok(output)) if output.status.success() => RepoPresence::Present,
        Ok(Ok(_)) if dot_git_present(cwd) => RepoPresence::Unusable,
        Ok(Ok(_)) => RepoPresence::Absent,
        Ok(Err(_)) | Err(_) => RepoPresence::Unusable,
    }
}

/// Whether `.git` is on disk, without following symlinks - a dangling
/// `.git` symlink is a broken checkout rather than an absent repo, and
/// `Path::try_exists` reports it as missing because it stats the target.
/// Anything but a definite "not found" counts as present, so an
/// unreadable parent biases to [`RepoPresence::Unusable`] as well.
fn dot_git_present(cwd: &Path) -> bool {
    !matches!(
        cwd.join(".git").symlink_metadata(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    )
}

/// Classify `git rev-parse --abbrev-ref HEAD` output into either the
/// branch string to proceed with, or the terminal `repo_gate`. Empty
/// stdout is a clean "not a repo" signal (git ran and reported it);
/// Oversize means the scan subprocess itself misbehaved.
///
/// `presence` resolves the non-zero-exit case, which is ambiguous on
/// its own: git exits 128 both outside a repo and when it fails inside
/// one, so only the narrower repo-existence question separates a
/// legitimate non-repo from a sick scanner.
fn classify_rev_parse(output: GitOutput, presence: RepoPresence) -> Result<String, RepoGate> {
    match output {
        GitOutput::Ok(s) => Ok(s.trim().to_owned()),
        GitOutput::Empty => Err(RepoGate::NotARepo),
        GitOutput::Failed if presence == RepoPresence::Absent => Err(RepoGate::NotARepo),
        GitOutput::Failed | GitOutput::Oversize => Err(RepoGate::ScannerFailed),
    }
}

/// Resolve the rev-parse gate for `cwd`: the raw branch string, or the
/// terminal `repo_gate`. The repo-existence probe runs only on a
/// non-zero exit, so the healthy path still costs a single subprocess.
async fn rev_parse_gate(cwd: &Path) -> Result<String, RepoGate> {
    let output = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await;
    let presence = if matches!(output, GitOutput::Failed) {
        repo_presence(cwd).await
    } else {
        RepoPresence::Present
    };
    classify_rev_parse(output, presence)
}

/// Minimum gap between background `git fetch` kicks for one repo. The
/// scan piggybacks a fetch to keep origin/<default> fresh; this caps it
/// so a burst of scans (session switches, the 1s ticker's 10s refresh)
/// fetches at most once per window.
const FETCH_THROTTLE: Duration = Duration::from_secs(240);

/// Minimum gap between PR-lookup attempts for one branch+sha, even
/// when the attempt found nothing or failed - without this a cached
/// `None` would stick for the branch's whole life (a PR opened after
/// the session's first scan would never show) and a persistent `gh`
/// failure would hammer every 10 s scan.
const PR_REFETCH_INTERVAL: Duration = Duration::from_secs(300);

/// Max commits the pushed-sha walk examines. Bounds the pathological
/// case (a deep unpushed history) so one scan can't walk the whole
/// repo; 500 first-parent commits is generous headroom over any real
/// worktree's unpushed depth.
const PUSHED_SHA_WALK_CAP: usize = 500;

/// Per-repo last-fetch timestamps, keyed by the scan `cwd`. Module-level
/// because [`scan`] is a free function with no owning state; the ticker
/// scans one active session at a time, so keying by scan cwd gives
/// stable per-repo throttling. Mirrors the `OnceLock<Mutex<HashMap>>`
/// pattern in `cloud::oauth_credentials`.
static LAST_FETCH: OnceLock<Mutex<HashMap<PathBuf, Instant>>> = OnceLock::new();

/// Whether a background fetch is due: no prior fetch, or the last one is
/// at least `window` old. Pure so the throttle is unit-testable.
fn should_fetch(last: Option<Instant>, now: Instant, window: Duration) -> bool {
    match last {
        None => true,
        Some(last) => now.saturating_duration_since(last) >= window,
    }
}

/// Claim a fetch slot for `key`: returns true (stamping `now`) when a
/// fetch is due per [`should_fetch`], false when throttled. Stamp-on-
/// claim so two racing scans can't both kick.
fn claim_fetch_slot(key: &Path, now: Instant, window: Duration) -> bool {
    let mut map = LAST_FETCH.get_or_init(|| Mutex::new(HashMap::new())).lock();
    let due = should_fetch(map.get(key).copied(), now, window);
    if due {
        map.insert(key.to_path_buf(), now);
    }
    due
}

/// Kick a throttled, non-blocking `git fetch origin <default>` so the
/// remote-tracking ref the branch-ahead diff compares against stays
/// fresh. Only fires for a remote-tracking default (`origin/...`); a
/// purely-local default has no remote to fetch. The scan never awaits
/// it - this scan used the current origin/<default>; the fetch refreshes
/// it for the NEXT scan. Failures (offline, auth, no remote) warn and
/// are a no-op.
fn kick_background_fetch(cwd: &Path, default_branch: Option<&str>) {
    let Some(remote_branch) = default_branch.and_then(|d| d.strip_prefix("origin/")) else {
        return;
    };
    if !claim_fetch_slot(cwd, Instant::now(), FETCH_THROTTLE) {
        return;
    }
    let cwd = cwd.to_path_buf();
    let remote_branch = remote_branch.to_owned();
    tokio::spawn(async move {
        let output = Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["fetch", "origin", &remote_branch])
            .kill_on_drop(true)
            .output()
            .await;
        match output {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                tracing::warn!(
                    target: crate::logging::targets::ENV_GIT,
                    cwd = %cwd.display(),
                    event_name = "git_background_fetch_nonzero_exit",
                    message = "background git fetch exited non-zero",
                    outcome = "failure",
                    exit_code = out.status.code().unwrap_or(-1),
                    branch = %remote_branch,
                );
            }
            Err(err) => {
                tracing::warn!(
                    target: crate::logging::targets::ENV_GIT,
                    cwd = %cwd.display(),
                    event_name = "git_background_fetch_failed",
                    message = "background git fetch spawn/wait failed",
                    outcome = "failure",
                    error = %err,
                    branch = %remote_branch,
                );
            }
        }
    });
}

/// Run the full scan sequence against `cwd` and return a snapshot.
/// Always succeeds - every failure path collapses to a non-InRepo
/// `repo_gate` and a WARN log naming the step that failed. Callers
/// should treat the snapshot as authoritative for rendering
/// regardless of which variant came back.
///
/// `prev` is the most recent snapshot for this `cwd` (if any). It's
/// used to reuse cached PR info while the branch name, pushed sha
/// and fetch age (5 minutes) all say the cached answer still holds
/// (`pr_cache_fresh`). Pass `None` for cold starts.
/// The caller's current branch name via `git rev-parse --abbrev-ref HEAD`,
/// keeping a detached HEAD apart from a failed read: `Ok(Some(name))` on
/// a named branch, `Ok(None)` on a detached HEAD, `Err(gate)` when git
/// reported nothing usable. Mirrors the `GitBranch::Named` string
/// [`scan`] derives, so the review MCP can resolve a caller's `(project,
/// branch)` review scope to the same key the `/diff` overlay persists
/// under - and name the step that failed when it can't.
pub async fn current_branch(cwd: &Path) -> Result<Option<String>, RepoGate> {
    let name = rev_parse_gate(cwd).await?;
    Ok((name != "HEAD" && !name.is_empty()).then_some(name))
}

pub async fn scan(cwd: &Path, prev: Option<&GitDiffSnapshot>) -> GitDiffSnapshot {
    let raw_branch = match rev_parse_gate(cwd).await {
        Ok(branch) => branch,
        // Both the not-a-repo and scanner-crash cases collapse to the
        // same all-Clean snapshot, differing only by `repo_gate` so the
        // renderer can tell "not a git repository" from "scanner
        // unhealthy."
        Err(repo_gate) => {
            return GitDiffSnapshot {
                branch: GitBranch::NoRepo,
                default_branch: None,
                repo_gate,
                pushed_sha: None,
                worktree: LayerState::Clean,
                branch_ahead: LayerState::Clean,
                pr: None,
                closes: Vec::new(),
                pr_fetched_at: None,
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
    // Keep origin/<default> fresh for the NEXT scan without blocking this
    // one - throttled, non-blocking, failure-tolerant.
    kick_background_fetch(cwd, default_branch.as_deref());
    // `default_branch` may be a remote-tracking ref (`origin/main`); the
    // on-default check compares plain branch names, so strip the
    // `origin/` prefix - a checked-out `main` must still read as the
    // default rather than a feature branch ahead of `origin/main`.
    let on_default = default_branch
        .as_deref()
        .map(|d| d.strip_prefix("origin/").unwrap_or(d))
        .is_some_and(|d| d == raw_branch);
    let dirty_probe = is_worktree_dirty(cwd).await;

    // Layer 1: uncommitted edits vs HEAD. The three legal states
    // (clean tree, populated diff, scan failed) are unrepresentable
    // as illegal combinations because `LayerState` encodes them
    // directly. Either upstream failure (dirty probe OR numstat)
    // collapses to `ScanFailed` so the renderer surfaces "(scan
    // failed)" instead of silently dropping to clean-tree.
    let worktree: LayerState<GitDiffStats> = match dirty_probe {
        None => LayerState::ScanFailed,
        Some(false) => LayerState::Clean,
        Some(true) => match numstat(cwd, &["diff", "--numstat", "HEAD"]).await {
            Ok(stats) => LayerState::Populated(stats),
            Err(_) => LayerState::ScanFailed,
        },
    };

    // Layer 2: commits the branch has ahead of the default branch.
    // Skipped on detached HEAD (no meaningful "branch name" to be
    // ahead from), on the default branch itself (the diff against
    // itself is empty by construction), and when default_branch is
    // unknown (no sensible base). A clean branch sitting at the
    // merge-base with no commits ahead collapses to `Clean` so the
    // renderer doesn't show an empty layer-2 row. `ScanFailed`
    // carries the subprocess-error signal mirroring layer 1.
    let branch_ahead: LayerState<GitBranchAhead> = if !detached && !on_default {
        if let Some(default) = default_branch.as_deref() {
            let range = format!("{default}...HEAD");
            match numstat(cwd, &["diff", "--numstat", &range]).await {
                Ok(stats) => match commit_count_in_range(cwd, default, "HEAD").await {
                    None => LayerState::ScanFailed,
                    Some(0) => LayerState::Clean,
                    Some(commit_count) => {
                        // `commit_count == 0` is the cleanest signal
                        // that the branch has no commits ahead of
                        // default; gate on that alone. The earlier
                        // `&& stats.total_files == 0` clause masked
                        // the racy merge-base / force-push corner
                        // case where commit_count == 0 but stats
                        // showed files, producing a "0 commits vs
                        // main" subtitle that read oddly to the user.
                        LayerState::Populated(GitBranchAhead { commit_count, stats })
                    }
                },
                Err(_) => LayerState::ScanFailed,
            }
        } else {
            LayerState::Clean
        }
    } else {
        LayerState::Clean
    };

    // PR / closes only make sense for named non-default branches -
    // default branch never has a PR open against itself, detached /
    // unknown branches can't be queried by name. For eligible
    // branches, resolve HEAD's newest pushed ancestor (the lookup
    // key - never the branch name, which diverges from the PR head
    // ref for `HEAD:<name>` pushes) and reuse the prior snapshot's
    // data while the sha and fetch age say the cache still holds.
    let (pr, closes, pr_fetched_at, pushed_sha) = match &branch {
        GitBranch::Named(name) if !on_default => {
            let pushed_sha = resolve_pushed_sha(cwd, PUSHED_SHA_WALK_CAP).await;
            let (pr, closes, pr_fetched_at) =
                pr_for_head(cwd, name, pushed_sha.as_deref(), prev).await;
            (pr, closes, pr_fetched_at, pushed_sha)
        }
        _ => (None, Vec::new(), None, None),
    };

    // repo_gate=InRepo here - we've passed the rev-parse gate and
    // every downstream subprocess (default-branch resolution,
    // numstat, gh pr) is best-effort; failures collapse to safe
    // defaults plus per-layer `LayerState::ScanFailed` so the
    // renderer can surface them at the layer level rather than
    // poisoning the overall snapshot. The ScannerFailed gate is
    // only the rev-parse-failed return at the top of this function.
    GitDiffSnapshot {
        branch,
        default_branch,
        repo_gate: RepoGate::InRepo,
        pushed_sha,
        worktree,
        branch_ahead,
        pr,
        closes,
        pr_fetched_at,
    }
}

/// Count commits in `<base>..HEAD` via `git rev-list --count`.
/// Returns `None` on any failure path (subprocess error, parse
/// failure, anomalous empty output) so callers can route the
/// signal into `LayerState::ScanFailed` instead of masking the
/// failure as 0. Each non-Ok branch emits a WARN log mirroring the
/// `gh_pr_lookup_*` pattern.
async fn commit_count_in_range(cwd: &Path, base: &str, head: &str) -> Option<u32> {
    let range = format!("{base}..{head}");
    match run_git(cwd, &["rev-list", "--count", &range]).await {
        GitOutput::Ok(s) => match s.trim().parse::<u32>() {
            Ok(count) => Some(count),
            Err(err) => {
                tracing::warn!(
                    target: crate::logging::targets::ENV_GIT,
                    cwd = %cwd.display(),
                    event_name = "git_commit_count_parse_failed",
                    message = "rev-list --count stdout did not parse as u32",
                    outcome = "failure",
                    error = %err,
                    range = %range,
                    raw = %s.trim(),
                );
                None
            }
        },
        GitOutput::Empty => {
            // rev-list --count always emits `0\n` on success even for
            // an empty range; Empty here is anomalous (binary stdout,
            // truncation, etc.). Log separately so operators can tell
            // it apart from Failed/Oversize when triaging a vanished
            // layer-2 row.
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "git_commit_count_empty",
                message = "rev-list --count returned empty stdout (expected at minimum '0\\n')",
                outcome = "anomalous",
                range = %range,
            );
            None
        }
        GitOutput::Failed | GitOutput::Oversize => None,
    }
}

/// Resolve the ref a feature branch's "ahead" diff compares against.
/// Prefers the remote-tracking `origin/<default>` (via `origin/HEAD`,
/// else `origin/main` / `origin/master`) so a locally-stale `main`
/// can't fold already-merged commits into the diff; falls back to a
/// purely-local `main` / `master` for a repo with no `origin` remote.
/// The returned value IS the compare-ref - the `origin/` prefix carries
/// the remote-vs-local decision, so callers wanting the plain branch
/// name strip it. `None` when nothing resolves.
async fn resolve_default_branch(cwd: &Path) -> Option<String> {
    // `origin/HEAD` advertises the remote's default; `--short` yields
    // e.g. `origin/main`. Keep the prefix - it's the remote-tracking
    // ref we want to compare against (GitOutput::Ok is non-empty).
    if let GitOutput::Ok(s) =
        run_git(cwd, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await
    {
        return Some(s.trim().to_owned());
    }
    // No `origin/HEAD`: try the conventional remote-tracking refs, then
    // a purely-local `main` / `master` (a repo with no origin remote).
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if let GitOutput::Ok(_) = run_git(cwd, &["rev-parse", "--verify", candidate]).await {
            return Some(candidate.to_owned());
        }
    }
    // Nothing resolved. A feature-branch diff has no meaningful base,
    // so layer 2 collapses to `LayerState::Clean`. WARN so it's grep-
    // able when the user reports "the GIT section never shows my
    // branch's diff".
    tracing::warn!(
        target: crate::logging::targets::ENV_GIT,
        cwd = %cwd.display(),
        event_name = "git_default_branch_unknown",
        message = "default branch fallback exhausted (no origin/HEAD, origin/main, origin/master, main, or master)",
        outcome = "skipped",
    );
    None
}

/// Returns `Some(dirty)` when `git status` ran cleanly (`Ok` /
/// `Empty`); `None` when the subprocess hit `Failed` / `Oversize`.
/// Callers thread the `None` signal into `LayerState::ScanFailed`
/// so the renderer can surface "(scan failed)" for layer 1 instead
/// of silently rendering a clean tree when the probe upstream of
/// numstat collapsed.
async fn is_worktree_dirty(cwd: &Path) -> Option<bool> {
    match run_git(cwd, &["status", "--porcelain=v1", "--untracked-files=no"]).await {
        GitOutput::Ok(s) => Some(!s.trim().is_empty()),
        GitOutput::Empty => Some(false),
        GitOutput::Failed | GitOutput::Oversize => None,
    }
}

/// Underlying `git diff --numstat` subprocess didn't return a usable
/// result. Callers map this to `LayerState::ScanFailed` so the
/// renderer can surface the failure. The variants carry enough
/// classification to triage from the WARN log emitted by `run_git`;
/// downstream callers don't differentiate them today but the type
/// keeps the option open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumstatError {
    /// Subprocess crashed, timed out, or exited non-zero. See
    /// `git_subprocess_failed` / `git_subprocess_nonzero_exit` /
    /// `git_subprocess_timeout` WARN events.
    Subprocess,
    /// Stdout exceeded the per-command size cap. See
    /// `git_subprocess_oversize` WARN event.
    Oversize,
}

/// Run `git <args>` and parse the `--numstat` output. Returns the
/// top-`TOP_FILE_COUNT` files (sorted by total changes desc, alpha
/// tie-break) plus the full file count and overall add/remove
/// totals.
/// Returns `Err(NumstatError)` when the underlying `git` subprocess
/// hit `Failed` / `Oversize`; callers map that signal to
/// `LayerState::ScanFailed` so the renderer can distinguish "scan
/// failed" from "clean tree, nothing to show". `Empty` (exit 0, no
/// stdout) collapses to a zero-row stats block since that's the
/// legitimate "no diff to report" outcome.
async fn numstat(cwd: &Path, args: &[&str]) -> Result<GitDiffStats, NumstatError> {
    let raw = match run_git(cwd, args).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty => String::new(),
        GitOutput::Failed => return Err(NumstatError::Subprocess),
        GitOutput::Oversize => return Err(NumstatError::Oversize),
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
    Ok(GitDiffStats { files, total_files, total_added, total_removed })
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

/// Whether the previous snapshot's PR block is still reusable: the
/// branch name and pushed sha both match the current scan AND the
/// last lookup attempt is less than [`PR_REFETCH_INTERVAL`] old.
/// The sha leg - not the branch name - is what makes a PR opened or
/// pushed after an earlier scan show up; the timer leg unsticks a
/// cached `None` (a scan that ran before the PR existed would
/// otherwise hold `pr: None` for the branch's whole life).
fn pr_cache_fresh(
    prev: Option<&GitDiffSnapshot>,
    branch: &str,
    pushed_sha: Option<&str>,
    now: std::time::SystemTime,
) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    let GitBranch::Named(prev_name) = &prev.branch else {
        return false;
    };
    prev_name == branch
        && prev.pushed_sha.as_deref() == pushed_sha
        && prev.pr_fetched_at.is_some_and(|at| {
            now.duration_since(at).is_ok_and(|elapsed| elapsed < PR_REFETCH_INTERVAL)
        })
}

/// What one PR lookup produced. `Failed` is kept apart from `None`
/// so a transient `gh` blip cannot evict a known-good cached PR.
enum PrLookup {
    Found(GitPrInfo, Vec<GitIssueRef>),
    /// The query ran and answered "no open PR containing this sha".
    None,
    /// The query did not complete (gh missing / auth / rate limit /
    /// unparseable response).
    Failed,
}

/// Resolve PR info for the branch from `prev`'s cache when fresh,
/// else query fresh by the pushed sha. Stamps `pr_fetched_at` on the
/// returned attempt time so failed / empty lookups rate-limit too.
/// A `Failed` lookup keeps `prev`'s PR when branch and sha still
/// match - a gh blip must not flash the row off.
async fn pr_for_head(
    cwd: &Path,
    branch: &str,
    pushed_sha: Option<&str>,
    prev: Option<&GitDiffSnapshot>,
) -> (Option<GitPrInfo>, Vec<GitIssueRef>, Option<std::time::SystemTime>) {
    let now = std::time::SystemTime::now();
    let cached = match prev {
        Some(prev) if pr_cache_fresh(Some(prev), branch, pushed_sha, now) => Some(prev),
        _ => None,
    };
    if let Some(prev) = cached {
        return (prev.pr.clone(), prev.closes.clone(), prev.pr_fetched_at);
    }
    match fetch_pr_for_pushed_sha(cwd, pushed_sha).await {
        PrLookup::Found(pr, closes) => (Some(pr), closes, Some(now)),
        PrLookup::None => (None, Vec::new(), Some(now)),
        PrLookup::Failed => match prev {
            Some(prev) if same_branch_and_sha(prev, branch, pushed_sha) => {
                (prev.pr.clone(), prev.closes.clone(), Some(now))
            }
            _ => (None, Vec::new(), Some(now)),
        },
    }
}

/// Whether `prev` describes the same branch and pushed sha the
/// current scan resolved - the condition under which a failed
/// lookup may keep `prev`'s cached PR.
fn same_branch_and_sha(prev: &GitDiffSnapshot, branch: &str, pushed_sha: Option<&str>) -> bool {
    matches!(&prev.branch, GitBranch::Named(prev_name) if prev_name == branch)
        && prev.pushed_sha.as_deref() == pushed_sha
}

/// Resolve HEAD's newest pushed ancestor: walk HEAD's first-parent
/// history newest-first (bounded by `cap`) and stop at the first sha
/// the remote-tracking refs already contain. `None` when nothing of
/// HEAD's ancestry is pushed or no remote refs exist (a truthful
/// negative); a git failure also yields `None` but is an
/// UNRESOLVABLE lookup, not a negative - the row may flicker off for
/// one scan while git is broken, WARN-logged, and recovers on the
/// next scan. The walk happens inside git so a worktree ahead of the
/// PR tip by unpushed commits still resolves the commit the PR was
/// pushed from.
async fn resolve_pushed_sha(cwd: &Path, cap: usize) -> Option<String> {
    let remote_shas =
        match run_git(cwd, &["for-each-ref", "refs/remotes", "--format=%(objectname)"]).await {
            GitOutput::Ok(raw) => raw
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<std::collections::HashSet<_>>(),
            GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => {
                std::collections::HashSet::default()
            }
        };
    if remote_shas.is_empty() {
        return None;
    }
    let max_count = format!("--max-count={cap}");
    let walk = run_git(cwd, &["rev-list", "--first-parent", &max_count, "HEAD"]).await;
    let GitOutput::Ok(raw) = walk else {
        return None;
    };
    raw.lines().map(str::trim).find(|sha| remote_shas.contains(*sha)).map(str::to_owned)
}

/// Look up the open PR containing `pushed_sha`. Returns
/// [`PrLookup::None`] when the query ran and answered "no open PR"
/// (nothing pushed, or no open PR contains the sha) and
/// [`PrLookup::Failed`] on every failure path - `gh` missing,
/// unauthenticated, not a github repo, JSON parse error. Failures
/// log at WARN with a structured event so operators can grep for
/// `gh_pr_lookup_*` when triaging "PR row never shows"; the
/// commit-not-on-remote shape logs at DEBUG instead because an
/// unpushed HEAD is a normal mid-work state, not an operator signal.
async fn fetch_pr_for_pushed_sha(cwd: &Path, pushed_sha: Option<&str>) -> PrLookup {
    let Some(sha) = pushed_sha else {
        return PrLookup::None;
    };
    let endpoint = format!("repos/{{owner}}/{{repo}}/commits/{sha}/pulls");
    let raw = match run_gh(cwd, &["api", &endpoint], GhNotFound::Tolerate).await {
        GitOutput::Ok(s) => s,
        GitOutput::Empty => {
            // A completed query with a definitive negative: exit 0
            // with `[]`, or the commit-not-on-remote shape run_gh
            // folds into Empty. No open PR, no cache-keep.
            return PrLookup::None;
        }
        GitOutput::Failed | GitOutput::Oversize => return PrLookup::Failed,
    };
    match pick_open_pr(&raw) {
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "gh_pr_lookup_parse_failed",
                message = "gh api commits pulls returned unparseable json",
                outcome = "failure",
                error = %err,
                sha = %sha,
            );
            PrLookup::Failed
        }
        Ok(None) => PrLookup::None,
        Ok(Some((number, url))) => {
            let closes = fetch_closing_issues(cwd, number).await;
            PrLookup::Found(GitPrInfo { number, url }, closes)
        }
    }
}

/// Select the open PR from the `commits/<sha>/pulls` REST response.
/// Multiple open PRs can contain the same commit (stacked PRs,
/// cherry-picks); the most recently `updated_at` one wins - the
/// actively-evolving PR is the right answer in stacked workflows -
/// with the higher number breaking ties. One row renders, never a
/// list.
///
/// `updated_at` compares lexicographically, which is chronological
/// for GitHub's uniform RFC3339 `Z`-suffixed format.
fn pick_open_pr(raw: &str) -> Result<Option<(u64, String)>, serde_json::Error> {
    let entries: Vec<GhApiPull> = serde_json::from_str(raw)?;
    Ok(entries
        .into_iter()
        .filter(|pr| pr.state == "open")
        .max_by(|a, b| (&a.updated_at, a.number).cmp(&(&b.updated_at, b.number)))
        .map(|pr| (pr.number, pr.html_url)))
}

/// `commits/<sha>/pulls` REST entry shape. Only the fields the
/// selection rule reads are deserialised; `gh api` adds others
/// (`title`, `body`, …) that serde silently drops.
#[derive(Deserialize)]
struct GhApiPull {
    number: u64,
    state: String,
    html_url: String,
    updated_at: String,
}

/// Fetch the PR's closing-issue list via
/// `gh pr view <n> --json closingIssuesReferences`. The REST
/// `commits/<sha>/pulls` response doesn't carry closing issues (a
/// GraphQL-only field), so this second call keeps the
/// `→ closes #M` tail rendering. Degrades to an empty list on any
/// failure - the PR row is the headline, the closes are the tail.
async fn fetch_closing_issues(cwd: &Path, number: u64) -> Vec<GitIssueRef> {
    let number = number.to_string();
    let raw = match run_gh(
        cwd,
        &["pr", "view", &number, "--json", "closingIssuesReferences"],
        GhNotFound::Warn,
    )
    .await
    {
        GitOutput::Ok(raw) => raw,
        GitOutput::Empty | GitOutput::Failed | GitOutput::Oversize => return Vec::new(),
    };
    let parsed: GhPrView = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "gh_pr_lookup_parse_failed",
                message = "gh pr view returned unparseable json",
                outcome = "failure",
                error = %err,
                pr = %number,
            );
            return Vec::new();
        }
    };
    parsed
        .closing_issues
        .into_iter()
        .map(|issue| GitIssueRef { number: issue.number, url: issue.url })
        .collect()
}

/// `gh pr view --json` wrapper shape.
#[derive(Deserialize)]
struct GhPrView {
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
/// right context - `Failed` means non-zero exit with stderr that an
/// operator might need; `Empty` is a legitimate "ran fine, no
/// output" signal (the common case for `status --porcelain` on a
/// clean tree).
pub(super) enum GitOutput {
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
/// stdout cap because stderr is conversational - a couple of
/// `fatal:` lines is more than enough context.
const STDERR_LOG_CAP: usize = 1024;

/// Spawn `git <args>` against `cwd`, await with a per-command
/// timeout, return classified output. Non-zero exits log WARN with
/// the captured stderr so operators can distinguish "clean tree"
/// from "corrupt index / permissions / fatal: …" without re-running.
pub(super) async fn run_git(cwd: &Path, args: &[&str]) -> GitOutput {
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
        // zero stats - keep the surface failure-tolerant - but log
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

/// How a `gh` call's "not found" failure should log. `Tolerate`
/// covers the shape the endpoint legitimately produces for a
/// mid-work state (an unpushed HEAD): DEBUG, not WARN, so the
/// operator's `gh_pr_lookup_*` triage grep stays free of per-scan
/// noise.
enum GhNotFound {
    Warn,
    Tolerate,
}

/// Spawn `gh <args>` from `cwd` (gh derives the github repo from
/// the current working directory - there's no `-C` equivalent).
/// Mirrors [`run_git`]'s timeout / classification / WARN logging so
/// failures distinguish "gh: command not found" (binary missing)
/// from "gh: To use GitHub CLI in a Git repository, please run …"
/// (not a github remote) from "no pull requests found" (legitimate
/// empty result) when triaging "PR row never shows".
async fn run_gh(cwd: &Path, args: &[&str], not_found: GhNotFound) -> GitOutput {
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        // The commits/<sha>/pulls endpoint answers HTTP 422 with
        // this prose when the sha isn't on the remote - an unpushed
        // HEAD mid-work, not an operator fault. That is a completed
        // query with a definitive negative, not a failure, so it
        // returns Empty (PrLookup::None downstream) rather than
        // inheriting the keep-cached-PR semantics of Failed.
        if matches!(not_found, GhNotFound::Tolerate) && stderr.contains("No commit found for SHA") {
            tracing::debug!(
                target: crate::logging::targets::ENV_GIT,
                cwd = %cwd.display(),
                event_name = "gh_pr_lookup_unpushed_sha",
                message = "commit not on the remote; PR lookup skipped",
                outcome = "no_data",
                args = ?args,
            );
            return GitOutput::Empty;
        }
        // gh exits non-zero on: missing auth (4), not a github
        // remote (1), API error (1). All collapse to "no PR" for
        // the renderer, but the log captures stderr so an operator
        // can tell which case fired.
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
        // at the desired branch via `symbolic-ref` - works on every
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

    /// `tempfile::tempdir()` produces a dir with no `.git/`, so the
    /// repo-existence probe finds nothing and `repo_gate` is NotARepo.
    /// The distinction matters for the renderer: a healthy non-repo
    /// gets a clean hidden GIT section, while a sick scanner surfaces
    /// the unhealthy banner.
    #[tokio::test(flavor = "current_thread")]
    async fn scan_no_repo_collapses_to_not_in_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snap = scan(dir.path(), None).await;
        assert_eq!(snap.repo_gate, RepoGate::NotARepo);
        assert!(matches!(snap.worktree, LayerState::Clean));
        assert!(matches!(snap.branch_ahead, LayerState::Clean));
        assert!(snap.default_branch.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_branch_reports_named_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "feat/x");
        write_file(&dir, "README.md", "hi\n");
        commit_all(&dir, "init");
        assert_eq!(current_branch(dir.path()).await, Ok(Some("feat/x".to_owned())));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_branch_is_ok_none_on_detached_head() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "hi\n");
        commit_all(&dir, "init");
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["checkout", "--detach"])
            .output()
            .expect("git ok");
        assert!(out.status.success(), "detach: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            current_branch(dir.path()).await,
            Ok(None),
            "a detached HEAD is a repo with no branch name, not a failed read",
        );
    }

    /// `git rev-parse --abbrev-ref HEAD` exits 128 both outside a work
    /// tree and when it fails inside one, so the exit code alone can't
    /// separate them. The repo-existence probe does: a plain directory
    /// has no repo, so it is a clean NotARepo rather than a scanner
    /// failure, and the renderer suppresses the GIT section.
    #[tokio::test(flavor = "current_thread")]
    async fn current_branch_reports_not_a_repo_outside_a_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(current_branch(dir.path()).await, Err(RepoGate::NotARepo));
    }

    /// A path that does not exist has no repo either - git can't even
    /// chdir to it - so it lands on the same gate rather than claiming
    /// the scanner is sick. `run_git`'s WARN still carries git's stderr
    /// for triage.
    #[tokio::test(flavor = "current_thread")]
    async fn current_branch_reports_not_a_repo_for_a_missing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(current_branch(&dir.path().join("gone")).await, Err(RepoGate::NotARepo));
    }

    /// An unborn HEAD (`git init` with no commits) is the case the exit
    /// code cannot be trusted for: `rev-parse --abbrev-ref HEAD` fails
    /// because HEAD resolves to nothing, while `--git-dir` succeeds.
    #[tokio::test(flavor = "current_thread")]
    async fn current_branch_on_an_unborn_head_is_not_reported_as_a_non_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        assert_eq!(
            current_branch(dir.path()).await,
            Err(RepoGate::ScannerFailed),
            "a repo with no commits is still a repo, so it must not be suppressed as NotARepo",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_clean_default_branch_has_no_layers() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "hello\n");
        commit_all(&dir, "init");
        let snap = scan(dir.path(), None).await;
        assert_eq!(snap.repo_gate, RepoGate::InRepo);
        assert!(matches!(snap.worktree, LayerState::Clean), "clean tree → no layer 1");
        assert!(matches!(snap.branch_ahead, LayerState::Clean), "on default → no layer 2");
        assert_eq!(snap.default_branch.as_deref(), Some("main"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_dirty_default_branch_populates_worktree_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "first\n");
        commit_all(&dir, "init");
        // Dirty: modify the tracked file without committing.
        write_file(&dir, "README.md", "second\nthird\n");
        let snap = scan(dir.path(), None).await;
        let LayerState::Populated(stats) = &snap.worktree else {
            panic!("layer 1 populated on dirty tree");
        };
        assert!(
            matches!(snap.branch_ahead, LayerState::Clean),
            "on default branch → layer 2 stays empty"
        );
        assert_eq!(stats.total_files, 1);
        assert_eq!(stats.files[0].path, "README.md");
        assert!(stats.files[0].added >= 1);
        assert_eq!(stats.total_added, stats.files[0].added);
        assert_eq!(stats.total_removed, stats.files[0].removed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_clean_feature_branch_populates_branch_ahead_only() {
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
        assert!(matches!(snap.worktree, LayerState::Clean), "clean tree → no layer 1");
        let LayerState::Populated(ahead) = &snap.branch_ahead else {
            panic!("layer 2 populated on feature branch");
        };
        assert_eq!(ahead.commit_count, 1, "one commit on feature branch beyond main");
        assert_eq!(ahead.stats.total_files, 1);
        assert_eq!(ahead.stats.files[0].path, "feat.rs");
        assert_eq!(snap.default_branch.as_deref(), Some("main"));
        assert_eq!(ahead.stats.total_added, ahead.stats.files[0].added);
        assert_eq!(ahead.stats.total_removed, ahead.stats.files[0].removed);
    }

    /// Feature branch with uncommitted edits: both layers must
    /// populate independently. Layer 1 carries the dirty tree;
    /// layer 2 carries the commit(s) on the feature branch.
    #[tokio::test(flavor = "current_thread")]
    async fn scan_dirty_feature_branch_populates_both_layers() {
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
        // Dirty the worktree AFTER the feature commit.
        write_file(&dir, "feat.rs", "fn x() {}\nfn y() {}\n");

        let snap = scan(dir.path(), None).await;
        let LayerState::Populated(worktree) = &snap.worktree else {
            panic!("layer 1 populated on dirty tree");
        };
        let LayerState::Populated(ahead) = &snap.branch_ahead else {
            panic!("layer 2 populated on feature branch");
        };
        // Layer 1: in-progress edits to feat.rs.
        assert_eq!(worktree.total_files, 1);
        assert_eq!(worktree.files[0].path, "feat.rs");
        // Layer 2: the committed-and-unmerged work (the original
        // `feat commit` against main).
        assert_eq!(ahead.commit_count, 1);
        assert_eq!(ahead.stats.total_files, 1);
        assert_eq!(ahead.stats.files[0].path, "feat.rs");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_detached_dirty_populates_worktree_no_branch_ahead() {
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
        assert!(snap.worktree.is_populated(), "detached + dirty → layer 1 populated");
        assert!(
            matches!(snap.branch_ahead, LayerState::Clean),
            "detached HEAD has no branch name, layer 2 stays empty"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scan_detached_clean_has_no_layers() {
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
        assert_eq!(snap.repo_gate, RepoGate::InRepo);
        assert!(matches!(snap.worktree, LayerState::Clean));
        assert!(matches!(snap.branch_ahead, LayerState::Clean));
        assert_eq!(snap.pr, None, "detached HEAD resolves no PR row");
        assert_eq!(snap.pushed_sha, None, "the pushed-sha walk is skipped when detached");
    }

    /// The pushed-sha walk resolves a DIVERGED upstream: the remote
    /// has commits HEAD lacks (a sibling commit the remote gained),
    /// and the walk still stops at HEAD's newest pushed ancestor -
    /// never the sibling, which is outside HEAD's ancestry.
    #[tokio::test(flavor = "current_thread")]
    async fn pushed_sha_walk_resolves_pushed_ancestor_when_upstream_diverged() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "audit/x");
        write_file(&dir, "README.md", "one\n");
        commit_all(&dir, "pushed commit");
        let pushed = rev_parse_head(&dir);
        let run = |args: &[&str]| {
            StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git ok");
        };
        run(&["update-ref", "refs/remotes/origin/audit/x", &pushed]);
        // A remote-only sibling commit: same parent as the pushed
        // tip, so the remote history diverged - HEAD lacks it.
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["commit-tree", "HEAD^{tree}", "-p", &pushed, "-m", "remote-only"])
            .output()
            .expect("git ok");
        assert!(out.status.success(), "commit-tree failed");
        let remote_only = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        run(&["update-ref", "refs/remotes/origin/main", &remote_only]);
        write_file(&dir, "more.rs", "y\n");
        commit_all(&dir, "unpushed commit");

        assert_eq!(
            resolve_pushed_sha(dir.path(), 500).await.as_deref(),
            Some(pushed.as_str()),
            "the sibling remote-only commit is outside HEAD's ancestry and must not resolve"
        );
    }

    /// The unstick trap pinned at the cache-decision level: a cached
    /// `None` (scan ran before the PR was opened) goes stale on the
    /// same timer and refetches even with branch and sha unchanged.
    #[test]
    fn pr_cache_fresh_refetches_a_cached_none_when_stale() {
        let old = std::time::SystemTime::now()
            .checked_sub(PR_REFETCH_INTERVAL + std::time::Duration::from_secs(1))
            .expect("wall clock is at least PR_REFETCH_INTERVAL + 1s past boot");
        let prev = GitDiffSnapshot {
            branch: GitBranch::Named("feat/x".into()),
            default_branch: Some("main".into()),
            repo_gate: RepoGate::InRepo,
            pushed_sha: Some("aaaa".into()),
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: None,
            closes: Vec::new(),
            pr_fetched_at: Some(old),
        };
        assert!(
            !pr_cache_fresh(Some(&prev), "feat/x", Some("aaaa"), std::time::SystemTime::now()),
            "stale None must refetch so a PR opened after the first scan shows up"
        );
    }

    /// An unborn HEAD (fresh init, no commits): the scan collapses to
    /// the scanner-failed gate and the PR side stays dark - no row,
    /// no panic.
    #[tokio::test(flavor = "current_thread")]
    async fn scan_on_an_unborn_head_has_no_pr_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        let snap = scan(dir.path(), None).await;
        assert_eq!(snap.repo_gate, RepoGate::ScannerFailed);
        assert_eq!(snap.pr, None);
        assert_eq!(snap.pushed_sha, None);
        assert_eq!(snap.pr_fetched_at, None);
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
        let LayerState::Populated(stats) = &snap.worktree else {
            panic!("dirty tree → layer 1 populated");
        };
        assert_eq!(stats.total_files, 3);
        // Sum of per-file added/removed equals the overall total.
        let per_file_added: u32 = stats.files.iter().map(|f| f.added).sum();
        let per_file_removed: u32 = stats.files.iter().map(|f| f.removed).sum();
        assert_eq!(stats.total_added, per_file_added);
        assert_eq!(stats.total_removed, per_file_removed);
        // At least one row in each direction (we added 1+3+1 lines
        // and removed 1).
        assert!(stats.total_added >= 5);
        assert!(stats.total_removed >= 1);
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

    /// Cache hit: same branch, same pushed sha, fetched inside the
    /// 5-minute window. The reuse that keeps steady-state 10 s scans
    /// gh-free.
    #[test]
    fn pr_cache_fresh_reuses_when_branch_sha_and_age_hold() {
        let prev = snapshot_with_pr(42);
        assert!(pr_cache_fresh(Some(&prev), "feat/x", Some("aaaa"), std::time::SystemTime::now()));
    }

    /// Refetch trigger: the pushed sha moved (new push / rebase
    /// landed on the remote), so the cached PR may no longer be the
    /// one containing the branch's pushed tip.
    #[test]
    fn pr_cache_fresh_refetches_when_pushed_sha_changes() {
        let prev = snapshot_with_pr(42);
        assert!(
            !pr_cache_fresh(Some(&prev), "feat/x", Some("bbbb"), std::time::SystemTime::now()),
            "sha change must invalidate the cached PR"
        );
    }

    /// Refetch trigger: the fetch is older than the interval, even
    /// with branch and sha unchanged - unsticks a cached `None` from
    /// before the PR was opened.
    #[test]
    fn pr_cache_fresh_refetches_when_fetch_is_stale() {
        let prev = snapshot_with_pr(42);
        let old = std::time::SystemTime::now()
            .checked_sub(PR_REFETCH_INTERVAL + std::time::Duration::from_secs(1))
            .expect("monotonic wall clock is at least PR_REFETCH_INTERVAL + 1s past boot");
        assert!(
            !pr_cache_fresh(Some(&prev), "feat/x", Some("aaaa"), old),
            "stale fetch must refetch so a PR opened after the first scan shows up"
        );
    }

    /// `pr_for_head` short-circuits the `gh` calls when the cache is
    /// fresh - even when the `cwd` would make a real `gh` invocation
    /// fail (tempdir has no github remote). The returned `pr` /
    /// `closes` MUST be clones of `prev`'s, proving the cache hit.
    #[tokio::test(flavor = "current_thread")]
    async fn pr_for_head_reuses_prev_when_cache_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pr = GitPrInfo { number: 42, url: "https://example/pull/42".into() };
        let closes = vec![GitIssueRef { number: 7, url: "https://example/issues/7".into() }];
        let prev = GitDiffSnapshot {
            branch: GitBranch::Named("feat/x".into()),
            default_branch: Some("main".into()),
            repo_gate: RepoGate::InRepo,
            pushed_sha: Some("aaaa".into()),
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: Some(pr.clone()),
            closes: closes.clone(),
            pr_fetched_at: Some(std::time::SystemTime::now()),
        };

        let (got_pr, got_closes, got_at) =
            pr_for_head(dir.path(), "feat/x", Some("aaaa"), Some(&prev)).await;
        assert_eq!(got_pr, Some(pr));
        assert_eq!(got_closes, closes);
        assert_eq!(got_at, prev.pr_fetched_at);
    }

    /// A stale cache refetches - and when the refetch FAILS (tempdir
    /// makes `gh` fail whether or not it is installed), the known-good
    /// cached PR survives instead of flashing off: a gh blip must not
    /// evict the row.
    #[tokio::test(flavor = "current_thread")]
    async fn pr_for_head_keeps_cached_pr_when_refetch_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale_at = std::time::SystemTime::now()
            .checked_sub(PR_REFETCH_INTERVAL + std::time::Duration::from_secs(1))
            .expect("wall clock is at least PR_REFETCH_INTERVAL + 1s past boot");
        let prev = GitDiffSnapshot {
            branch: GitBranch::Named("feat/x".into()),
            default_branch: Some("main".into()),
            repo_gate: RepoGate::InRepo,
            pushed_sha: Some("aaaa".into()),
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: Some(GitPrInfo { number: 42, url: "https://example/pull/42".into() }),
            closes: Vec::new(),
            pr_fetched_at: Some(stale_at),
        };

        let (got_pr, _got_closes, got_at) =
            pr_for_head(dir.path(), "feat/x", Some("aaaa"), Some(&prev)).await;
        assert_eq!(got_pr.as_ref().map(|p| p.number), Some(42), "failed refetch keeps the row");
        let now = std::time::SystemTime::now();
        assert!(
            got_at.is_some_and(|at| now.duration_since(at).is_ok_and(|d| d.as_secs() < 60)),
            "the refetch attempt must be stamped so the next retry waits for the interval"
        );
    }

    /// A failed refetch after a BRANCH change has no matching cached
    /// PR to keep - the row clears rather than showing the previous
    /// branch's PR.
    #[tokio::test(flavor = "current_thread")]
    async fn pr_for_head_clears_failed_refetch_when_branch_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stale_at = std::time::SystemTime::now()
            .checked_sub(PR_REFETCH_INTERVAL + std::time::Duration::from_secs(1))
            .expect("wall clock is at least PR_REFETCH_INTERVAL + 1s past boot");
        let prev = GitDiffSnapshot {
            branch: GitBranch::Named("feat/x".into()),
            default_branch: Some("main".into()),
            repo_gate: RepoGate::InRepo,
            pushed_sha: Some("aaaa".into()),
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: Some(GitPrInfo { number: 42, url: "https://example/pull/42".into() }),
            closes: Vec::new(),
            pr_fetched_at: Some(stale_at),
        };

        let (got_pr, got_closes, got_at) =
            pr_for_head(dir.path(), "feat/y", Some("aaaa"), Some(&prev)).await;
        assert_eq!(got_pr, None, "a failed lookup must not show another branch's PR");
        assert!(got_closes.is_empty());
        assert!(got_at.is_some());
    }

    /// The incident shape: a worktree whose local branch name differs
    /// from the PR's head ref still resolves its PR, because the
    /// lookup key is the pushed ancestor sha, not the branch name.
    /// Real git, no network: `origin`'s tracking ref is simulated
    /// with `update-ref`, exactly what a `HEAD:<name>` push + fetch
    /// leaves behind.
    #[tokio::test(flavor = "current_thread")]
    async fn pushed_sha_walk_resolves_pushed_ancestor_when_worktree_is_ahead() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "worktree-inspector-pr-row");
        write_file(&dir, "README.md", "one\n");
        commit_all(&dir, "pushed commit");
        let pushed = rev_parse_head(&dir);
        let run = |args: &[&str]| {
            StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git ok");
        };
        // Simulate the push as `HEAD:audit/2026-09-03` + fetch: the
        // local branch stays `worktree-inspector-pr-row` while the
        // remote tracking ref names the pushed target.
        run(&["update-ref", "refs/remotes/origin/audit/2026-09-03", &pushed]);
        write_file(&dir, "more.rs", "fn y() {}\n");
        commit_all(&dir, "unpushed commit");
        write_file(&dir, "more2.rs", "fn z() {}\n");
        commit_all(&dir, "unpushed commit 2");

        assert_eq!(
            resolve_pushed_sha(dir.path(), 500).await.as_deref(),
            Some(pushed.as_str()),
            "the walk must stop at the pushed ancestor, not the unpushed tip"
        );
    }

    /// A worktree sitting exactly on its pushed tip resolves the tip
    /// itself (the names-match happy path).
    #[tokio::test(flavor = "current_thread")]
    async fn pushed_sha_walk_resolves_tip_when_pushed() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "one\n");
        commit_all(&dir, "pushed commit");
        let pushed = rev_parse_head(&dir);
        StdCommand::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["update-ref", "refs/remotes/origin/main", &pushed])
            .output()
            .expect("git ok");

        assert_eq!(resolve_pushed_sha(dir.path(), 500).await.as_deref(), Some(pushed.as_str()));
    }

    /// Nothing pushed - no remote refs share any of HEAD's history.
    /// No row, and the walk terminates rather than scanning the repo.
    #[tokio::test(flavor = "current_thread")]
    async fn pushed_sha_walk_returns_none_when_nothing_pushed() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "one\n");
        commit_all(&dir, "local commit");

        assert_eq!(resolve_pushed_sha(dir.path(), 500).await, None);
    }

    /// A pathological unpushed history terminates at the cap: shas
    /// beyond `cap` first-parent commits are never examined, so a
    /// pushed ancestor outside the cap does not resolve (truthful
    /// "no row" rather than an unbounded walk).
    #[tokio::test(flavor = "current_thread")]
    async fn pushed_sha_walk_caps_at_max_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        write_file(&dir, "README.md", "one\n");
        commit_all(&dir, "pushed commit");
        let pushed = rev_parse_head(&dir);
        StdCommand::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["update-ref", "refs/remotes/origin/main", &pushed])
            .output()
            .expect("git ok");
        for i in 0..4 {
            write_file(&dir, &format!("f{i}.rs"), "x\n");
            commit_all(&dir, &format!("unpushed {i}"));
        }

        // Cap 2 reaches only the newest 2 unpushed commits; the
        // pushed ancestor at depth 5 stays out of reach.
        assert_eq!(resolve_pushed_sha(dir.path(), 2).await, None);
        // Full cap reaches it.
        assert_eq!(resolve_pushed_sha(dir.path(), 500).await.as_deref(), Some(pushed.as_str()));
    }

    /// The open-filter + newest-updatedAt selection against the
    /// `commits/<sha>/pulls` REST shape: a closed PR with a NEWER
    /// updated_at must lose to an open PR, and two open PRs pick the
    /// most recently updated one (the actively-evolving stacked PR).
    #[test]
    fn pick_open_pr_filters_to_open_and_picks_newest_updated_at() {
        let raw = concat!(
            r#"[{"number":900,"state":"closed","html_url":"https://example/pull/900","updated_at":"2026-09-03T15:00:00Z"},"#,
            r#"{"number":858,"state":"open","html_url":"https://example/pull/858","updated_at":"2026-09-03T10:00:00Z"},"#,
            r#"{"number":859,"state":"open","html_url":"https://example/pull/859","updated_at":"2026-09-03T12:00:00Z"}]"#
        );
        let picked = pick_open_pr(raw).expect("parses");
        assert_eq!(
            picked,
            Some((859, "https://example/pull/859".to_owned())),
            "open filter first, then most recently updated wins"
        );
    }

    /// Equal `updated_at` breaks deterministically on the higher
    /// number, so the pick never depends on response ordering.
    #[test]
    fn pick_open_pr_ties_break_on_higher_number() {
        let raw = concat!(
            r#"[{"number":401,"state":"open","html_url":"https://example/pull/401","updated_at":"2026-09-03T12:00:00Z"},"#,
            r#"{"number":402,"state":"open","html_url":"https://example/pull/402","updated_at":"2026-09-03T12:00:00Z"}]"#
        );
        assert_eq!(
            pick_open_pr(raw).expect("parses"),
            Some((402, "https://example/pull/402".to_owned()))
        );
    }

    /// Only merged/closed PRs contain the sha: no row. The
    /// open-only invariant means a merged PR row can never render.
    #[test]
    fn pick_open_pr_returns_none_when_only_closed_prs() {
        let raw = r#"[{"number":857,"state":"closed","html_url":"https://example/pull/857","updated_at":"2026-09-03T13:16:33Z"}]"#;
        assert_eq!(pick_open_pr(raw).expect("parses"), None);
    }

    /// Garbage JSON is an Err (the caller logs
    /// `gh_pr_lookup_parse_failed`), never a silent None.
    #[test]
    fn pick_open_pr_reports_parse_error() {
        assert!(pick_open_pr("not json").is_err());
    }

    fn snapshot_with_pr(number: u64) -> GitDiffSnapshot {
        snapshot_with_pr_at(number, Some(0))
    }

    fn snapshot_with_pr_at(number: u64, fetched_secs_ago: Option<u64>) -> GitDiffSnapshot {
        let pr = GitPrInfo { number, url: format!("https://example/pull/{number}") };
        let pr_fetched_at = fetched_secs_ago
            .map(|secs| std::time::SystemTime::now() - std::time::Duration::from_secs(secs));
        GitDiffSnapshot {
            branch: GitBranch::Named("feat/x".into()),
            default_branch: Some("main".into()),
            repo_gate: RepoGate::InRepo,
            pushed_sha: Some("aaaa".into()),
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: Some(pr),
            closes: Vec::new(),
            pr_fetched_at,
        }
    }

    /// Full 40-hex sha of HEAD in the test repo.
    fn rev_parse_head(dir: &TempDir) -> String {
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git ok");
        assert!(out.status.success(), "rev-parse HEAD failed");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
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
            repo_gate: RepoGate::InRepo,
            pushed_sha: None,
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: Some(synthetic_pr.clone()),
            closes: synthetic_closes.clone(),
            pr_fetched_at: Some(std::time::SystemTime::now()),
        };

        let snap = scan(dir.path(), Some(&prev)).await;
        assert_eq!(snap.pr, Some(synthetic_pr), "cached PR must carry through scan");
        assert_eq!(snap.closes, synthetic_closes);
        assert_eq!(snap.pushed_sha, None, "nothing pushed in this repo");
    }

    /// On the default branch, `scan` skips the PR fetch entirely -
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
            repo_gate: RepoGate::InRepo,
            pushed_sha: None,
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: Some(GitPrInfo { number: 1, url: "url".into() }),
            closes: Vec::new(),
            pr_fetched_at: Some(std::time::SystemTime::now()),
        };
        // Even with a cache-hit-shaped prev, the default-branch gate
        // wins and the PR field clears.
        let snap = scan(dir.path(), Some(&prev)).await;
        assert_eq!(snap.pr, None);
        assert!(snap.closes.is_empty());
        assert_eq!(snap.pushed_sha, None, "the walk must not run on the default branch");
        assert_eq!(snap.pr_fetched_at, None);
    }

    #[test]
    fn classify_rev_parse_maps_output_to_repo_gate() {
        // Ok(stdout) trims to the branch; Empty is a clean non-repo
        // signal; Oversize is always a scanner fault.
        assert_eq!(
            classify_rev_parse(GitOutput::Ok("  main\n".to_owned()), RepoPresence::Present),
            Ok("main".to_owned())
        );
        assert_eq!(
            classify_rev_parse(GitOutput::Empty, RepoPresence::Absent),
            Err(RepoGate::NotARepo)
        );
        assert_eq!(
            classify_rev_parse(GitOutput::Oversize, RepoPresence::Present),
            Err(RepoGate::ScannerFailed)
        );
    }

    /// A non-zero exit is the ambiguous case, and the probe is what
    /// resolves it. `Unusable` must stay on the scanner gate: a git
    /// that would not run is a fault to report, not a licence to
    /// suppress the section as though there were no repo.
    #[test]
    fn a_non_zero_exit_is_resolved_by_the_repo_presence_probe() {
        assert_eq!(
            classify_rev_parse(GitOutput::Failed, RepoPresence::Absent),
            Err(RepoGate::NotARepo)
        );
        assert_eq!(
            classify_rev_parse(GitOutput::Failed, RepoPresence::Present),
            Err(RepoGate::ScannerFailed)
        );
        assert_eq!(
            classify_rev_parse(GitOutput::Failed, RepoPresence::Unusable),
            Err(RepoGate::ScannerFailed)
        );
    }

    /// The probe's own contract against real git: a plain directory has
    /// no repo, a fresh `git init` with no commits does. That second
    /// case is the one the branch rev-parse gets wrong on its own.
    #[tokio::test(flavor = "current_thread")]
    async fn repo_presence_separates_a_bare_directory_from_an_unborn_repo() {
        let plain = tempfile::tempdir().expect("tempdir");
        assert_eq!(repo_presence(plain.path()).await, RepoPresence::Absent);
        assert_eq!(repo_presence(&plain.path().join("gone")).await, RepoPresence::Absent);

        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(&repo, "main");
        assert_eq!(repo_presence(repo.path()).await, RepoPresence::Present);
    }

    /// A checkout git refuses is not an absent repo. Deleting `.git/HEAD`
    /// leaves `.git` on disk while `rev-parse` exits 128 with the *same*
    /// `fatal:` line a plain non-repo gives, so the on-disk check is the
    /// only thing separating them - and reading it as absent would
    /// suppress the GIT section for a broken checkout instead of
    /// reporting it. The pruned-worktree case forge produces routinely
    /// has this shape.
    #[tokio::test(flavor = "current_thread")]
    async fn a_checkout_git_refuses_is_unusable_not_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        std::fs::remove_file(dir.path().join(".git").join("HEAD")).expect("remove HEAD");

        assert_eq!(repo_presence(dir.path()).await, RepoPresence::Unusable);
        assert_eq!(
            current_branch(dir.path()).await,
            Err(RepoGate::ScannerFailed),
            "a checkout git refuses must surface the scanner banner, not read as a non-repo",
        );
        assert_eq!(scan(dir.path(), None).await.repo_gate, RepoGate::ScannerFailed);
    }

    /// The one shape where following symlinks reads a broken checkout as
    /// an absent one: git gives the same "not a git repository" line a
    /// plain non-repo gives, and `try_exists` says missing because it
    /// stats the vanished target rather than the link.
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_dangling_dot_git_symlink_is_unusable_not_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(dir.path().join("gone-target"), dir.path().join(".git"))
            .expect("symlink");

        assert_eq!(repo_presence(dir.path()).await, RepoPresence::Unusable);
        assert_eq!(scan(dir.path(), None).await.repo_gate, RepoGate::ScannerFailed);
    }

    /// A worker branch is cut from origin/main while local `main` lags
    /// origin/main (the usual state when the user only pulls merged
    /// PRs). The branch-ahead diff must compare against the remote-
    /// tracking origin/main - showing only the branch's OWN commit -
    /// not against a stale local main that would fold in already-merged
    /// work. Exercised through a real linked worktree.
    #[tokio::test(flavor = "current_thread")]
    async fn scan_feature_branch_compares_against_origin_main_not_stale_local_main() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        let run = |args: &[&str]| {
            let out =
                StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        write_file(&dir, "base.txt", "base\n");
        commit_all(&dir, "X base");
        // Y = a commit that lives on origin/main (already merged).
        run(&["checkout", "-q", "-b", "upstream"]);
        write_file(&dir, "merged.txt", "merged\n");
        commit_all(&dir, "Y merged");
        run(&["update-ref", "refs/remotes/origin/main", "upstream"]);
        run(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
        // Local main rewinds to X - it now lags origin/main by Y.
        run(&["checkout", "-q", "main"]);
        run(&["branch", "-D", "upstream"]);
        // Worker worktree cut from origin/main (Y), plus its own commit Z.
        std::fs::create_dir_all(dir.path().join(".claude/worktrees")).expect("mkdir");
        let wt = dir.path().join(".claude/worktrees/w");
        run(&["worktree", "add", "-q", wt.to_str().expect("utf8"), "-b", "feat/w", "origin/main"]);
        let wt_run = |args: &[&str]| {
            let out = StdCommand::new("git").arg("-C").arg(&wt).args(args).output().expect("git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::write(wt.join("z.txt"), "z\n").expect("write z");
        wt_run(&["add", "-A"]);
        wt_run(&["commit", "-q", "-m", "Z own"]);

        let snap = scan(&wt, None).await;
        assert_eq!(
            snap.default_branch.as_deref(),
            Some("origin/main"),
            "compares against the remote-tracking ref, not stripped local main",
        );
        let LayerState::Populated(ahead) = &snap.branch_ahead else {
            panic!("branch-ahead populated for the worker branch");
        };
        assert_eq!(
            ahead.commit_count, 1,
            "only the branch's own commit vs origin/main, not the already-merged one",
        );
        assert_eq!(ahead.stats.total_files, 1);
        assert_eq!(
            ahead.stats.files[0].path, "z.txt",
            "merged.txt (already on origin/main) is excluded from the branch-ahead diff",
        );
    }

    /// On `main` with the default resolved to the remote-tracking ref
    /// `origin/main`, the branch-ahead layer stays empty even when local
    /// main is AHEAD of origin/main (unpushed commits): the on-default
    /// check strips the remote prefix so a checked-out `main` reads as
    /// the default branch rather than a feature branch.
    #[tokio::test(flavor = "current_thread")]
    async fn scan_on_local_main_ahead_of_origin_skips_branch_ahead() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        let run = |args: &[&str]| {
            let out =
                StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        write_file(&dir, "base.txt", "base\n");
        commit_all(&dir, "base");
        run(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
        // Local main advances past origin/main (unpushed commit).
        write_file(&dir, "local.txt", "local\n");
        commit_all(&dir, "unpushed");

        let snap = scan(dir.path(), None).await;
        assert_eq!(snap.default_branch.as_deref(), Some("origin/main"));
        assert!(
            matches!(snap.branch_ahead, LayerState::Clean),
            "on `main` the branch-ahead layer stays empty even when ahead of origin/main",
        );
    }

    #[test]
    fn should_fetch_gates_on_window() {
        let window = Duration::from_secs(240);
        let now = Instant::now();
        assert!(should_fetch(None, now, window), "no prior fetch is due");
        assert!(!should_fetch(Some(now), now, window), "a just-now fetch is throttled");
        let stale = now
            .checked_sub(window + Duration::from_secs(1))
            .expect("monotonic clock is at least window+1s past boot");
        assert!(should_fetch(Some(stale), now, window), "a stale prior fetch is due again");
    }

    #[test]
    fn claim_fetch_slot_throttles_within_window() {
        // Unique tempdir path so the module-level LAST_FETCH map can't
        // collide with another test's key.
        let dir = tempfile::tempdir().expect("tempdir");
        let key = dir.path();
        let window = Duration::from_secs(240);
        let t0 = Instant::now();
        assert!(claim_fetch_slot(key, t0, window), "first claim is due");
        assert!(!claim_fetch_slot(key, t0, window), "an immediate re-claim is throttled");
        assert!(
            claim_fetch_slot(key, t0 + window, window),
            "a claim once the window elapses is due again",
        );
    }

    /// A scan on an origin-tracking repo KICKS a throttled background
    /// fetch (claims the slot) without awaiting it: the scan returns
    /// with the current snapshot and the fetch refreshes origin for the
    /// next scan. Observing the claimed slot proves the kick fired.
    #[tokio::test(flavor = "current_thread")]
    async fn scan_kicks_background_fetch_for_origin_tracking_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        init_repo(&dir, "main");
        let run = |args: &[&str]| {
            let out =
                StdCommand::new("git").arg("-C").arg(dir.path()).args(args).output().expect("git");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        write_file(&dir, "base.txt", "base\n");
        commit_all(&dir, "base");
        run(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        run(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);

        let _ = scan(dir.path(), None).await;
        let kicked = LAST_FETCH.get().is_some_and(|m| m.lock().contains_key(dir.path()));
        assert!(kicked, "a due scan on an origin-tracking repo claims (kicks) a background fetch");
    }
}
