//! Git repository introspection — branch detection + change watching.
//!
//! Exposes a one-shot `git_context()` reader plus an async
//! `GitContextWatcher` that streams `GitContext` snapshots whenever
//! the underlying `.git` ref machinery changes (with a 75ms debounce
//! so a single `git checkout` doesn't fire dozens of events).
//!
//! Lifted from forge-sdk in 2026-05-05. The module's own
//! pre-restructure header read: *"This module is conceptually daemon
//! work, not SDK work — forge-sdk's main responsibility is wrapping
//! the `claude` CLI subprocess."* It now lives where it belongs —
//! agent-side, alongside the rest of the project's live-environment
//! state (`forge_agent::env::*`).
//!
//! Self-contained: depends only on `std`, `tokio`, `notify`, `serde`,
//! `tracing`, `thiserror`. No coupling to forge-sdk internals; local
//! error type ([`GitError`]) does not reuse `forge_sdk::Error`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::sync::mpsc as tokio_mpsc;

pub use forge_primitives::git::{GitBranch, GitContext};

const WATCH_DEBOUNCE: Duration = Duration::from_millis(75);

/// Failure modes for [`GitContextWatcher::new`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GitError {
    /// `notify` failed to set up its OS-level fs watch.
    #[error("failed to initialise git metadata watcher: {0}")]
    Watcher(#[from] notify::Error),
}

/// Read the current branch state for `cwd` once. Returns
/// `GitBranch::NoRepo` when no `.git` is found by walking ancestors;
/// `GitBranch::Detached` when `HEAD` points at a commit hash;
/// `GitBranch::Unknown` when `HEAD` exists but can't be parsed.
#[must_use]
pub fn git_context(cwd: &Path) -> GitContext {
    let branch = match ResolvedRepo::discover(cwd) {
        Some(repo) => repo.resolve_branch_state(),
        None => GitBranch::NoRepo,
    };
    let mut ctx = GitContext::default();
    ctx.branch = branch;
    ctx
}

/// Async watcher that streams `GitContext` snapshots whenever the
/// underlying ref machinery changes. The first snapshot fires
/// synchronously inside [`GitContextWatcher::new`]; subsequent
/// snapshots arrive via [`GitContextWatcher::next_snapshot`] only
/// when the resolved branch state actually changes (deduped against
/// the previous snapshot).
///
/// Drop the watcher to stop watching — the OS-level handle and the
/// background debounce thread tear down cleanly.
pub struct GitContextWatcher {
    rx: tokio_mpsc::UnboundedReceiver<GitContext>,
    /// `notify` watcher kept alive for the lifetime of this struct
    /// so the OS-level subscription stays open.
    _watcher: Option<RecommendedWatcher>,
}

impl GitContextWatcher {
    /// Set up a watcher for `cwd`'s git metadata. Always emits at
    /// least one initial snapshot (queued before this call returns)
    /// so callers can `.next_snapshot().await` immediately and get
    /// the starting state.
    ///
    /// # Errors
    ///
    /// Returns [`GitError::Watcher`] when `notify` fails to install
    /// its OS watcher (e.g. inotify limit reached on Linux).
    pub fn new(cwd: &Path) -> Result<Self, GitError> {
        let (snap_tx, snap_rx) = tokio_mpsc::unbounded_channel();

        // Always send the initial snapshot — even when there's no
        // repo, callers want the "branch: NoRepo" state to flow once.
        let initial = git_context(cwd);
        let _ = snap_tx.send(initial.clone());

        let repo = ResolvedRepo::discover(cwd);

        // Without a repo there's nothing to watch. The initial
        // NoRepo snapshot has been queued; future calls to
        // `next_snapshot()` block until the watcher is dropped.
        let Some(repo) = repo else {
            return Ok(Self { rx: snap_rx, _watcher: None });
        };

        let (notify_tx, notify_rx) = mpsc::channel::<notify::Result<Event>>();
        let mut watcher = notify::recommended_watcher(move |event| {
            // Best-effort: if the receiver is gone, drop events.
            let _ = notify_tx.send(event);
        })?;

        for (path, mode) in repo.watch_directories() {
            if let Err(err) = watcher.watch(&path, mode) {
                tracing::warn!(
                    target: crate::logging::targets::ENV_GIT,
                    path = %path.display(),
                    error = %err,
                    "failed to watch git metadata path",
                );
            }
        }

        // Drive the debounce loop on a blocking thread — `notify`'s
        // mpsc receiver is std-sync, so we'd block a tokio worker
        // thread otherwise. The blocking thread sends snapshots over
        // a tokio mpsc which the async API consumes.
        //
        // If the OS refuses the thread spawn (extremely rare — would
        // require a hard process-thread-limit hit), fall through and
        // return the watcher with just the initial snapshot. The
        // notify::Watcher will be dropped on `Self` drop, freeing
        // the OS handle. Live state stays consistent because the
        // initial snapshot has already been queued via `snap_tx`.
        let initial_branch = initial.branch.clone();
        if let Err(err) = std::thread::Builder::new()
            .name("forge-agent-git-debounce".to_owned())
            .spawn(move || {
                run_debounce_loop(&repo, &notify_rx, &snap_tx, initial_branch);
            })
        {
            tracing::error!(
                target: crate::logging::targets::ENV_GIT,
                error = %err,
                "failed to spawn git debounce thread; live updates disabled for this watcher",
            );
        }

        Ok(Self { rx: snap_rx, _watcher: Some(watcher) })
    }

    /// Wait for the next snapshot. Returns `None` only when the
    /// watcher has been dropped or the underlying notify subscription
    /// disconnected.
    pub async fn next_snapshot(&mut self) -> Option<GitContext> {
        self.rx.recv().await
    }
}

/// The debounce loop runs on a dedicated OS thread. It:
///
/// 1. Blocks on `notify_rx.recv()` — wakes on any fs event.
/// 2. When a relevant event arrives, drains additional events for
///    `WATCH_DEBOUNCE` (absorbing bursts from `git checkout` etc.).
/// 3. Re-reads the branch state. If it differs from the last sent
///    snapshot, sends a new `GitContext` over `snap_tx`.
/// 4. Loops until `notify_rx` disconnects (which happens when the
///    `notify::Watcher` is dropped — i.e. `GitContextWatcher` was
///    dropped).
fn run_debounce_loop(
    repo: &ResolvedRepo,
    notify_rx: &mpsc::Receiver<notify::Result<Event>>,
    snap_tx: &tokio_mpsc::UnboundedSender<GitContext>,
    mut last_branch: GitBranch,
) {
    loop {
        let Ok(event) = notify_rx.recv() else {
            return; // disconnected — watcher dropped
        };

        let relevant = match event {
            Ok(event) => event.need_rescan() || repo.is_relevant_event(&event),
            Err(err) => {
                tracing::debug!(
                    target: crate::logging::targets::ENV_GIT,
                    error = %err,
                    "git watcher reported an error; treating as a refresh trigger",
                );
                true
            }
        };

        if !relevant {
            continue;
        }

        // Debounce: drain follow-up events for WATCH_DEBOUNCE before
        // re-reading. Anything during this window is absorbed
        // regardless of relevance — the goal is "settle, then read."
        let deadline = Instant::now() + WATCH_DEBOUNCE;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match notify_rx.recv_timeout(remaining) {
                Ok(_) => {} // absorb
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        let new_branch = repo.resolve_branch_state();
        if new_branch != last_branch {
            last_branch = new_branch.clone();
            let mut snap = GitContext::default();
            snap.branch = new_branch;
            if snap_tx.send(snap).is_err() {
                return; // receiver dropped
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRepo {
    worktree_root: PathBuf,
    effective_git_dir: PathBuf,
    common_git_dir: PathBuf,
    head_path: PathBuf,
    heads_dir: PathBuf,
}

impl ResolvedRepo {
    fn discover(cwd: &Path) -> Option<Self> {
        let normalized_cwd = normalize_path(cwd);
        for ancestor in normalized_cwd.ancestors() {
            let dot_git_path = ancestor.join(".git");
            let Ok(metadata) = fs::metadata(&dot_git_path) else {
                continue;
            };

            let worktree_root = normalize_path(ancestor);
            let effective_git_dir = if metadata.is_dir() {
                normalize_path(&dot_git_path)
            } else if metadata.is_file() {
                let gitdir = parse_gitdir_target(&dot_git_path)?;
                resolve_relative_path(&worktree_root, &gitdir)
            } else {
                continue;
            };

            let commondir_path = effective_git_dir.join("commondir");
            let common_git_dir = read_optional_target(&commondir_path).map_or_else(
                || effective_git_dir.clone(),
                |target| resolve_relative_path(&effective_git_dir, &target),
            );
            let heads_dir = common_git_dir.join("refs").join("heads");

            return Some(Self {
                worktree_root,
                effective_git_dir: normalize_path(&effective_git_dir),
                common_git_dir: normalize_path(&common_git_dir),
                head_path: normalize_path(&effective_git_dir.join("HEAD")),
                heads_dir: normalize_path(&heads_dir),
            });
        }
        None
    }

    fn resolve_branch_state(&self) -> GitBranch {
        let Ok(head) = fs::read_to_string(&self.head_path) else {
            return GitBranch::Unknown;
        };

        let trimmed = head.trim();
        if trimmed.is_empty() {
            return GitBranch::Unknown;
        }

        let Some(reference) = trimmed.strip_prefix("ref:") else {
            return GitBranch::Detached;
        };
        let reference = reference.trim();
        if let Some(branch) = reference.strip_prefix("refs/heads/") {
            return GitBranch::Named(branch.to_owned());
        }

        GitBranch::Detached
    }

    fn is_relevant_event(&self, event: &Event) -> bool {
        event.paths.iter().any(|path| self.is_relevant_path(path))
    }

    fn is_relevant_path(&self, path: &Path) -> bool {
        let normalized = normalize_path(path);
        if normalized.starts_with(&self.heads_dir) {
            return true;
        }

        let Some(file_name) = normalized.file_name().and_then(|name| name.to_str()) else {
            return false;
        };

        if normalized.parent() == Some(self.worktree_root.as_path())
            && matches!(file_name, ".git" | ".git.lock")
        {
            return true;
        }

        if normalized.parent() == Some(self.effective_git_dir.as_path())
            && matches!(file_name, "HEAD" | "HEAD.lock" | "commondir")
        {
            return true;
        }

        normalized.parent() == Some(self.common_git_dir.as_path())
            && matches!(file_name, "packed-refs" | "packed-refs.lock")
    }

    fn watch_directories(&self) -> Vec<(PathBuf, RecursiveMode)> {
        let mut watched = BTreeMap::new();
        insert_watch_path(&mut watched, self.worktree_root.clone(), RecursiveMode::NonRecursive);
        insert_watch_path(
            &mut watched,
            self.effective_git_dir.clone(),
            RecursiveMode::NonRecursive,
        );
        insert_watch_path(&mut watched, self.common_git_dir.clone(), RecursiveMode::NonRecursive);

        if self.heads_dir.exists() {
            insert_watch_path(&mut watched, self.heads_dir.clone(), RecursiveMode::Recursive);
        }

        watched.into_iter().collect()
    }
}

fn insert_watch_path(
    watched: &mut BTreeMap<PathBuf, RecursiveMode>,
    path: PathBuf,
    recursive_mode: RecursiveMode,
) {
    match watched.get_mut(&path) {
        Some(mode) if recursive_mode == RecursiveMode::Recursive => {
            *mode = RecursiveMode::Recursive;
        }
        Some(_) => {}
        None => {
            watched.insert(path, recursive_mode);
        }
    }
}

fn parse_gitdir_target(dot_git_path: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(dot_git_path).ok()?;
    let raw = content.lines().find_map(|line| line.trim().strip_prefix("gitdir:"))?;
    let target = raw.trim();
    (!target.is_empty()).then(|| PathBuf::from(target))
}

fn read_optional_target(path: &Path) -> Option<PathBuf> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn resolve_relative_path(base: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() { normalize_path(target) } else { normalize_path(&base.join(target)) }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    if normalized.as_os_str().is_empty() { PathBuf::from(".") } else { normalized }
}

#[cfg(test)]
mod tests {

    use super::{GitBranch, GitContext, GitContextWatcher, ResolvedRepo, git_context};
    use notify::{Event, EventKind};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
    }

    fn create_standard_repo(root: &Path, branch: &str) -> PathBuf {
        let repo = root.join("repo");
        fs::create_dir_all(repo.join("src")).expect("create repo");
        write_file(&repo.join(".git").join("HEAD"), &format!("ref: refs/heads/{branch}\n"));
        write_file(&repo.join(".git").join("refs").join("heads").join(branch), "deadbeef\n");
        repo
    }

    fn create_worktree_repo(root: &Path, branch: &str) -> PathBuf {
        let repo = root.join("worktree");
        let effective = root.join("admin").join("worktrees").join("wt-1");
        let common = root.join("admin").join("common");
        fs::create_dir_all(repo.join("src")).expect("create worktree");
        write_file(&repo.join(".git"), "gitdir: ../admin/worktrees/wt-1\n");
        write_file(&effective.join("HEAD"), &format!("ref: refs/heads/{branch}\n"));
        write_file(&effective.join("commondir"), "../../common\n");
        write_file(&common.join("refs").join("heads").join(branch), "cafebabe\n");
        repo
    }

    #[test]
    fn discovers_repo_root_from_nested_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = create_standard_repo(dir.path(), "main");

        let resolved = ResolvedRepo::discover(&repo.join("src").join("nested")).expect("repo");

        assert_eq!(resolved.worktree_root, repo);
        assert_eq!(resolved.effective_git_dir, repo.join(".git"));
        assert_eq!(resolved.common_git_dir, repo.join(".git"));
    }

    #[test]
    fn resolves_git_file_and_commondir_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = create_worktree_repo(dir.path(), "feature/footer");

        let resolved = ResolvedRepo::discover(&repo.join("src")).expect("repo");

        assert_eq!(resolved.worktree_root, repo);
        assert_eq!(
            resolved.effective_git_dir,
            dir.path().join("admin").join("worktrees").join("wt-1")
        );
        assert_eq!(resolved.common_git_dir, dir.path().join("admin").join("common"));
        assert_eq!(
            resolved.heads_dir,
            dir.path().join("admin").join("common").join("refs").join("heads")
        );
    }

    #[test]
    fn resolves_named_branch_from_head() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = create_standard_repo(dir.path(), "feature/footer");
        let resolved = ResolvedRepo::discover(&repo).expect("repo");

        assert_eq!(resolved.resolve_branch_state(), GitBranch::Named("feature/footer".to_owned()));
    }

    #[test]
    fn resolves_detached_head_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = create_standard_repo(dir.path(), "main");
        write_file(&repo.join(".git").join("HEAD"), "0123456789abcdef\n");
        let resolved = ResolvedRepo::discover(&repo).expect("repo");

        assert_eq!(resolved.resolve_branch_state(), GitBranch::Detached);
    }

    #[test]
    fn returns_none_when_outside_repo() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert!(ResolvedRepo::discover(dir.path()).is_none());
    }

    #[test]
    fn filters_relevant_git_metadata_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = create_standard_repo(dir.path(), "main");
        let resolved = ResolvedRepo::discover(&repo).expect("repo");

        let head_event = Event::new(EventKind::Any).add_path(repo.join(".git").join("HEAD"));
        let packed_refs_event =
            Event::new(EventKind::Any).add_path(repo.join(".git").join("packed-refs"));
        let branch_ref_event = Event::new(EventKind::Any)
            .add_path(repo.join(".git").join("refs").join("heads").join("main"));
        let unrelated_event = Event::new(EventKind::Any).add_path(repo.join(".git").join("index"));

        assert!(resolved.is_relevant_event(&head_event));
        assert!(resolved.is_relevant_event(&packed_refs_event));
        assert!(resolved.is_relevant_event(&branch_ref_event));
        assert!(!resolved.is_relevant_event(&unrelated_event));
    }

    // ---- public-API one-shot reader ----

    #[test]
    fn git_context_returns_no_repo_outside_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = git_context(dir.path());
        assert_eq!(context.branch, GitBranch::NoRepo);
    }

    #[test]
    fn git_context_returns_named_branch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = create_standard_repo(dir.path(), "main");
        let context = git_context(&repo);
        assert_eq!(context.branch, GitBranch::Named("main".to_owned()));
    }

    // ---- watcher integration tests ----

    // Note: a `watcher_emits_initial_snapshot_for_repo` test would
    // belong here too, but on macOS notify v8's FSEvents teardown
    // takes ~50s when the watcher drops (held by FSEventStream
    // shutdown). That's fine in production — watchers live until
    // session end — but it makes the unit test ~50s wall-clock.
    // The deterministic ResolvedRepo tests above + the no-repo
    // initial-snapshot test below cover the core paths; the watcher
    // is exercised end-to-end via the TUI smoke test.

    #[tokio::test(flavor = "current_thread")]
    async fn watcher_emits_initial_snapshot_for_no_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut watcher = GitContextWatcher::new(dir.path()).expect("new");
        let snap = watcher.next_snapshot().await.expect("initial");
        assert_eq!(snap.branch, GitBranch::NoRepo);
    }

    // Note: an end-to-end "watcher fires on .git/HEAD edit" test
    // would belong here, but real-fs notify timing on macOS FSEvents
    // is too flaky for unit tests (events can take 1-30s). The
    // watcher's correctness is covered by:
    //   1. The ResolvedRepo tests above (discovery + parse + filter).
    //   2. The deterministic initial-snapshot tests below.
    //   3. Manual smoke testing in mcrs (footer chip updates on
    //      `git checkout`).

    #[test]
    fn git_branch_default_is_no_repo() {
        let branch = GitBranch::default();
        assert_eq!(branch, GitBranch::NoRepo);
    }

    #[test]
    fn git_branch_as_deref_only_named() {
        assert_eq!(GitBranch::Named("main".to_owned()).as_deref(), Some("main"));
        assert_eq!(GitBranch::Detached.as_deref(), None);
        assert_eq!(GitBranch::NoRepo.as_deref(), None);
        assert_eq!(GitBranch::Unknown.as_deref(), None);
    }

    #[test]
    fn git_context_serde_roundtrip() {
        let mut context = GitContext::default();
        context.branch = GitBranch::Named("main".to_owned());
        let json = serde_json::to_string(&context).expect("serialize");
        let parsed: GitContext = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, context);
    }
}
