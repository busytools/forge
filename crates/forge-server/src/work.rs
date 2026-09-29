//! The agent's working tree, behind a cache: no render path shells out.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::file_index::FileIndex;
use crate::git_diff;
use crate::surface::ViewSurface;
use forge_primitives::SessionSlot;
use forge_primitives::git_diff::{GitDiffSnapshot, RepoGate};

/// How long a read answers for. Everything inside the window is served
/// from the cache, which is what keeps a page render off a subprocess.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// What one agent's working tree looks like, and what git said about it.
///
/// `branch` and `changed` are `None` when there is nothing to report, and
/// `gate` is what tells the two cases apart: a directory outside a
/// repository, a working tree that is gone, and a git that would not run
/// all leave both fields empty and want different lines on the row.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkState {
    pub branch: Option<String>,
    pub changed: Option<usize>,
    pub gate: Gate,
}

/// The repo gate, as a view reads it. Its own type so the view does not
/// have to name the scanner's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// Git answered: there is a repository here, and whatever the two
    /// fields say about it is the whole truth.
    #[default]
    InRepo,
    /// A directory with no repository behind it.
    NotARepository,
    /// The directory is not there at all. Git reports this as the case
    /// above, and it is true and misleading at once: a row saying a
    /// project is not a repository about a path that does not exist
    /// claims something it cannot know. A despawned worker's worktree is
    /// the usual one.
    Gone,
    /// Git would not run, or would not answer. Distinct from both above
    /// because it is forge's problem rather than the project's.
    ScannerFailed,
}

impl From<RepoGate> for Gate {
    fn from(gate: RepoGate) -> Self {
        match gate {
            RepoGate::InRepo => Self::InRepo,
            RepoGate::NotARepo => Self::NotARepository,
            RepoGate::ScannerFailed => Self::ScannerFailed,
        }
    }
}

/// Each session's working tree, so a caller reads a value instead of
/// spawning `git` per row per render.
pub struct WorkCache {
    entries: Mutex<HashMap<SessionSlot, Entry>>,
}

struct Entry {
    cwd: PathBuf,
    /// The last read, `None` until the first one lands.
    state: Option<WorkState>,
    read_at: Instant,
    /// The full scan the inspector draws: the files behind the count, and
    /// the open PR. Its own window, because it runs more of git than the
    /// row's own read does.
    diff: Option<GitDiffSnapshot>,
    diff_read_at: Instant,
    /// The file walk the composer's `@` list reads. Its own window again:
    /// a walk costs more than the row's read and less than the full scan.
    file_index: Option<Arc<FileIndex>>,
    /// The ignore preference read for that walk, `None` until one lands.
    /// The walk's answer depends on it, so it is part of what the cache is
    /// a cache of: a flip re-walks rather than serving the old answer for
    /// the rest of the window.
    files_respecting: Option<bool>,
    files_read_at: Instant,
    /// Held across a refresh so two callers for one slot do not both probe
    /// the same tree.
    refreshing: Arc<tokio::sync::Mutex<()>>,
}

impl Entry {
    fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_owned(),
            state: None,
            read_at: Instant::now(),
            diff: None,
            diff_read_at: Instant::now(),
            file_index: None,
            files_respecting: None,
            files_read_at: Instant::now(),
            refreshing: Arc::default(),
        }
    }

    /// The cached read, when it was taken of this same directory and is
    /// still inside the window.
    fn answers_for(&self, cwd: &Path) -> Option<&WorkState> {
        if self.cwd == cwd && self.read_at.elapsed() < REFRESH_INTERVAL {
            self.state.as_ref()
        } else {
            None
        }
    }
}

impl Default for WorkCache {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkCache {
    pub fn new() -> Self {
        Self { entries: Mutex::new(HashMap::new()) }
    }

    /// `cwd` as an agent's working tree, at most `REFRESH_INTERVAL` old.
    /// A read outside the window goes to git; one inside answers from the
    /// cache. A moved cwd - a `/new`, a worktree - is a different tree and
    /// is read afresh.
    pub async fn snapshot(&self, slot: &SessionSlot, cwd: &Path) -> WorkState {
        let refreshing = {
            let mut entries = self.entries();
            Arc::clone(&entries.entry(slot.clone()).or_insert_with(|| Entry::new(cwd)).refreshing)
        };
        // One refresh per slot: a second caller waits here, then finds the
        // read the first one has already stored.
        let _refreshing = refreshing.lock().await;
        {
            let entries = self.entries();
            if let Some(state) = entries.get(slot).and_then(|entry| entry.answers_for(cwd)) {
                return state.clone();
            }
        }
        let state = read(cwd).await;
        let mut entries = self.entries();
        let entry = entries.entry(slot.clone()).or_insert_with(|| Entry::new(cwd));
        cwd.clone_into(&mut entry.cwd);
        entry.state = Some(state.clone());
        entry.read_at = Instant::now();
        state
    }

    /// The full scan of `cwd`'s working tree, at most `REFRESH_INTERVAL`
    /// old. A view drawing the detail reads this; the row's own line reads
    /// [`Self::snapshot`], and the two share a slot's entry so neither
    /// holds a second opinion about which tree the slot is in.
    pub async fn diff(&self, slot: &SessionSlot, cwd: &Path) -> GitDiffSnapshot {
        let refreshing = {
            let mut entries = self.entries();
            Arc::clone(&entries.entry(slot.clone()).or_insert_with(|| Entry::new(cwd)).refreshing)
        };
        let _refreshing = refreshing.lock().await;
        {
            let entries = self.entries();
            if let Some(diff) = entries
                .get(slot)
                .filter(|entry| entry.cwd == cwd && entry.diff_read_at.elapsed() < REFRESH_INTERVAL)
                .and_then(|entry| entry.diff.as_ref())
            {
                return diff.clone();
            }
        }
        // The previous scan rides along so the PR lookup is reused rather
        // than repeated: the scan rate-limits its own lookups on it.
        let prev = self.entries().get(slot).and_then(|entry| entry.diff.clone());
        let diff = git_diff::scan(cwd, prev.as_ref()).await;
        let mut entries = self.entries();
        let entry = entries.entry(slot.clone()).or_insert_with(|| Entry::new(cwd));
        cwd.clone_into(&mut entry.cwd);
        entry.diff = Some(diff.clone());
        entry.diff_read_at = Instant::now();
        diff
    }

    /// One root's files, walked at most `REFRESH_INTERVAL` old. The `@`
    /// list reads this: a typeahead walks on every keystroke, and the tree
    /// a reader is naming a file in does not change between two of them.
    ///
    /// The walk is blocking, so it runs off the reactor. A walk that
    /// panicked reads as no files rather than as the page's problem: the
    /// same answer a root that is not there gives.
    ///
    /// The surface comes along because the walk runs with the user's own
    /// gitignore preference, which is the core's read rather than the
    /// view's: the terminal's `@` list answers from the same rule. That read
    /// is a file parse, so it happens off the reactor too, whether the call
    /// walks or answers from the cache.
    pub async fn files(
        &self,
        surface: &Arc<ViewSurface>,
        slot: &SessionSlot,
        root: &Path,
    ) -> Arc<FileIndex> {
        let reader = Arc::clone(surface);
        let respecting =
            tokio::task::spawn_blocking(move || reader.respect_gitignore()).await.unwrap_or(true);
        let refreshing = {
            let mut entries = self.entries();
            Arc::clone(&entries.entry(slot.clone()).or_insert_with(|| Entry::new(root)).refreshing)
        };
        let _refreshing = refreshing.lock().await;
        {
            let entries = self.entries();
            if let Some(index) = entries
                .get(slot)
                .filter(|entry| {
                    entry.cwd == root
                        && entry.files_respecting == Some(respecting)
                        && entry.files_read_at.elapsed() < REFRESH_INTERVAL
                })
                .and_then(|entry| entry.file_index.as_ref())
            {
                return Arc::clone(index);
            }
        }
        let walked = root.to_owned();
        let surface = Arc::clone(surface);
        let (walked_index, walked_under) = tokio::task::spawn_blocking(move || {
            // Read beside the walk rather than at the cache check below: the
            // two reads are a moment apart, and this is the one the entry
            // stores. A walk that panicked keeps the key it was asked for,
            // which is why this is not `unwrap_or_default`.
            let respecting = surface.respect_gitignore();
            (surface.file_index(&walked), respecting)
        })
        .await
        .unwrap_or_else(|_| (FileIndex::default(), respecting));
        let index = Arc::new(walked_index);
        let mut entries = self.entries();
        let entry = entries.entry(slot.clone()).or_insert_with(|| Entry::new(root));
        root.clone_into(&mut entry.cwd);
        entry.file_index = Some(Arc::clone(&index));
        entry.files_respecting = Some(walked_under);
        entry.files_read_at = Instant::now();
        index
    }

    /// A panicking task must not take the cache with it: the map holds no
    /// invariant a panic can break.
    fn entries(&self) -> MutexGuard<'_, HashMap<SessionSlot, Entry>> {
        self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Store a scan for `slot` as if the cache had taken it, so a fixture can
    /// pin a populated PR and worktree without `gh` and a pushed branch.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed_test_diff(&self, slot: &SessionSlot, cwd: &Path, diff: &GitDiffSnapshot) {
        let mut entries = self.entries();
        let entry = entries.entry(slot.clone()).or_insert_with(|| Entry::new(cwd));
        cwd.clone_into(&mut entry.cwd);
        entry.diff = Some(diff.clone());
        entry.diff_read_at = Instant::now();
    }
}

/// The same working tree, as the row's own read states it, derived from a scan
/// the view already took.
///
/// One scan rather than two reads: the branch, the gate and the count then
/// belong to the same instant as the PR the row's section draws beside them,
/// so a branch switch between two reads cannot render a PR for a branch the
/// wire does not name. The count is the scan's worktree layer, which is the
/// number the terminal's own GIT section shows, and `None` when that layer
/// failed rather than a zero a reader would take for a clean tree.
pub fn work_from_scan(diff: &GitDiffSnapshot, cwd: &Path) -> WorkState {
    let gate = match diff.repo_gate {
        // Git calls a missing directory "not a repository", so the path itself
        // decides between the two, exactly as the row's own read does.
        RepoGate::NotARepo if !cwd.exists() => Gate::Gone,
        other => Gate::from(other),
    };
    if gate != Gate::InRepo {
        return WorkState { branch: None, changed: None, gate };
    }
    let branch = match &diff.branch {
        forge_primitives::git::GitBranch::Named(name) => Some(name.clone()),
        _ => None,
    };
    let changed = match &diff.worktree {
        forge_primitives::git_diff::LayerState::Populated(stats) => Some(stats.total_files),
        forge_primitives::git_diff::LayerState::Clean => Some(0),
        forge_primitives::git_diff::LayerState::ScanFailed => None,
    };
    WorkState { branch, changed, gate }
}

/// One read of `cwd`. The count decides whether the directory is a
/// repository at all: outside one both fields are `None` rather than an
/// error or a stale branch, and the gate carries which of the two cases it
/// was so the row can say so.
async fn read(cwd: &Path) -> WorkState {
    let (branch, changed) =
        tokio::join!(git_diff::current_branch(cwd), git_diff::changed_file_count(cwd));
    match changed {
        Ok(changed) => {
            WorkState { branch: branch.ok().flatten(), changed: Some(changed), gate: Gate::InRepo }
        }
        Err(gate) => {
            // Git calls a missing directory "not a repository", so the
            // path itself decides between the two: the row can say a
            // worktree is gone, and it cannot say a project is not a
            // repository without looking.
            let gate = if cwd.exists() { gate.into() } else { Gate::Gone };
            WorkState { branch: None, changed: None, gate }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use forge_primitives::SessionSlot;

    use crate::work::{Gate, WorkCache};

    fn slot() -> SessionSlot {
        SessionSlot::lead("TestOrg", "forge")
    }

    /// Spawn git the way the product does, scrub included. The product's
    /// constructor is `env::git_command::command`, which removes the
    /// ambient repo-location variables before every spawn; a fixture that
    /// skips that answers about a foreign repository when the suite runs
    /// under a git hook, which is the bug the scrub exists for.
    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// A repo with one commit on `main`. `git init -b` needs git 2.28 and
    /// CI runs 2.25, so HEAD is pointed by `symbolic-ref` instead.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(dir.path(), &["config", "user.email", "t@e.com"]);
        git(dir.path(), &["config", "user.name", "T"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("tracked.txt"), "one\n").expect("write");
        git(dir.path(), &["add", "tracked.txt"]);
        git(dir.path(), &["commit", "-qm", "init"]);
        dir
    }

    /// A directory that is not a repository answers with nothing at all,
    /// never an error and never a stale branch, and says which of the two
    /// it was: `dotfiles` and a despawned worker's worktree both land here,
    /// and the row renders an empty `where` plus the line that says why.
    #[tokio::test]
    async fn a_directory_outside_a_repository_answers_with_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = WorkCache::new();

        let state = cache.snapshot(&slot(), dir.path()).await;

        assert_eq!(state.branch, None, "a directory outside a repo has no branch");
        assert_eq!(state.changed, None, "and no changed-file count");
        assert_eq!(
            state.gate,
            Gate::NotARepository,
            "and the gate says it is the project rather than the scanner",
        );
    }

    /// A path that is not there at all is its own case, not "not a
    /// repository": git reports both the same way, and only the path can
    /// tell them apart. A despawned worker's worktree is the usual one.
    #[tokio::test]
    async fn a_path_that_is_gone_is_not_a_repository_that_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("worktree-that-was-despawned");
        let cache = WorkCache::new();

        let state = cache.snapshot(&slot(), &gone).await;

        assert_eq!(
            state.gate,
            Gate::Gone,
            "a missing path is gone, not a directory without a repository",
        );

        // And a directory that IS there with no repository is the other
        // case, so the two cannot be the same arm.
        let state = cache.snapshot(&slot(), dir.path()).await;
        assert_eq!(state.gate, Gate::NotARepository);
    }

    /// A path git cannot answer about at all is the scanner's failure,
    /// not the project's. A file in place of a directory is the cheapest
    /// one to make: the repo-existence probe looks for `.git` under it,
    /// finds a path that cannot hold one, and cannot rule a checkout out.
    #[tokio::test]
    async fn a_path_git_cannot_answer_about_is_the_scanners_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, "x\n").expect("write");
        let cache = WorkCache::new();

        let state = cache.snapshot(&slot(), &file).await;

        assert_eq!(
            state.gate,
            Gate::ScannerFailed,
            "git would not run there, which is forge's problem rather than the project's",
        );
    }

    /// A repository answers its branch and how much has moved in it.
    #[tokio::test]
    async fn a_repository_answers_its_branch_and_its_changed_files() {
        let dir = repo();
        std::fs::write(dir.path().join("tracked.txt"), "two\n").expect("modify");
        std::fs::write(dir.path().join("untracked.txt"), "new\n").expect("add");
        let cache = WorkCache::new();

        let state = cache.snapshot(&slot(), dir.path()).await;

        assert_eq!(state.branch.as_deref(), Some("main"), "the row shows the branch it is on");
        assert_eq!(
            state.changed,
            Some(2),
            "the count is the one modification plus the one untracked file",
        );
    }

    /// The second read inside the window is served from the cache rather
    /// than re-probed, which is what keeps a page render off the
    /// subprocess. The file is changed between the two reads, so a cache
    /// that re-read every time would report the new count.
    #[tokio::test]
    async fn a_second_read_inside_the_window_is_served_from_the_cache() {
        let dir = repo();
        let cache = WorkCache::new();

        let first = cache.snapshot(&slot(), dir.path()).await;
        assert_eq!(first.changed, Some(0), "precondition: the tree starts clean");

        std::fs::write(dir.path().join("untracked.txt"), "new\n").expect("add");
        let second = cache.snapshot(&slot(), dir.path()).await;

        assert_eq!(
            second.changed,
            Some(0),
            "a read inside the refresh window answers what the cache holds",
        );
    }

    /// A walk is an answer for the preference it was built under, so a flip
    /// inside the window re-walks: the preference decides which files come
    /// back, and serving the old answer for the rest of the window is the two
    /// views disagreeing for five seconds.
    #[tokio::test]
    async fn a_flip_inside_the_window_re_walks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fleet =
            crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["tree"])]).expect("fleet");
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");
        std::fs::write(root.join(".gitignore"), "ignored.rs\n").expect("write");
        std::fs::write(root.join("ignored.rs"), "").expect("write");
        let cache = WorkCache::new();

        let first = cache.files(&fleet.surface(), &slot(), &root).await;
        assert!(
            !first.entries.contains_key("ignored.rs"),
            "precondition: the walk respects the file",
        );

        fleet.set_user_preferences(serde_json::json!({ "respectGitignore": false }));
        let second = cache.files(&fleet.surface(), &slot(), &root).await;

        assert!(
            second.entries.contains_key("ignored.rs"),
            "a flip inside the window re-walks rather than serving the old answer",
        );
    }
}
