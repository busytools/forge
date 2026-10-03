//! The seat's working tree: what a scan of it found, when that was, and the
//! loop that keeps it fresh for the seats a view is showing.
//!
//! **The row is pushed, not polled.** A view used to read the tree through
//! whatever read happened to need it, which for a client meant a whole-record
//! encode every time it wanted one number. Here the scan runs for a seat
//! somebody is looking at, its answer is stored beside the process walk, and
//! the row moves by itself as [`SessionUpdate::WorkChanged`].
//!
//! A seat nobody holds is not scanned at all: the hold IS what looking means,
//! so an idle seat costs nothing and a fleet of them costs nothing either.
//! And a seat whose row has not moved announces nothing, however often it is
//! scanned - the frame says the tree moved, never that time passed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_agent::env::file_index::start_change_watch;
use forge_primitives::SessionSlot;
use forge_primitives::git::{GitIssueRef, GitPrInfo};
use forge_primitives::git_diff::GitDiffSnapshot;

use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;

/// The repo gate, as a view reads it. Its own type so the view does not have
/// to name the scanner's.
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

impl From<forge_primitives::git_diff::RepoGate> for Gate {
    fn from(gate: forge_primitives::git_diff::RepoGate) -> Self {
        match gate {
            forge_primitives::git_diff::RepoGate::InRepo => Self::InRepo,
            forge_primitives::git_diff::RepoGate::NotARepo => Self::NotARepository,
            forge_primitives::git_diff::RepoGate::ScannerFailed => Self::ScannerFailed,
        }
    }
}

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

/// The row a scan answers, from the scan and the directory it was taken of.
///
/// The directory decides one case git cannot: git calls a missing path "not a
/// repository" too, and a row that said so about a despawned worker's
/// worktree would claim something it cannot know.
pub fn work_from_scan(diff: &GitDiffSnapshot, cwd: &Path) -> WorkState {
    let gate = match diff.repo_gate {
        forge_primitives::git_diff::RepoGate::NotARepo if !cwd.exists() => Gate::Gone,
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

/// A seat's last scan of its working tree, and when it was taken.
///
/// Held whole rather than as the row alone, because the next scan takes the
/// previous one: that is what lets the PR lookup be reused rather than
/// repeated, and it is the scan rate-limiting its own remote lookups.
#[derive(Clone)]
pub struct WorkSnapshot {
    pub diff: GitDiffSnapshot,
    /// The directory this scan was taken of. Carried because the row is not
    /// in the scan alone: whether a missing path is a gone worktree or a
    /// directory outside a repository is a fact about the path.
    pub cwd: PathBuf,
    pub read_at: Instant,
}

/// How long a scan answers for: the terminal's own rule for the snapshot it
/// draws (`tui/app/git_diff.rs`'s `SNAPSHOT_STALENESS`), so both views live
/// with one freshness.
pub const SNAPSHOT_STALENESS: Duration = Duration::from_secs(10);

/// How often a held seat looks at its tree: the terminal's ticker interval.
///
/// The poke is a timestamp check, and [`should_scan`] is what decides whether
/// a subprocess runs - so this is also the ceiling on how fast a tree under
/// sustained change can be scanned, one scan per poke per held seat.
pub const POKE_INTERVAL: Duration = Duration::from_secs(1);

/// Whether the tree should be read again.
///
/// Three reasons, and the middle one is what watching buys: nothing read yet,
/// a tree that moved since the last read, and a read that has simply gone
/// stale - the last being what covers everything the watcher cannot report,
/// a commit or a stage among them.
pub fn should_scan(snapshot: Option<&WorkSnapshot>, dirty: bool, now: Instant) -> bool {
    match snapshot {
        None => true,
        Some(held) => dirty || now.saturating_duration_since(held.read_at) >= SNAPSHOT_STALENESS,
    }
}

/// The row a seat's viewers were last told, which is what keeps a still seat
/// from announcing itself again every time it is read.
#[derive(Clone, PartialEq)]
struct Announced {
    work: WorkState,
    pr: Option<GitPrInfo>,
    closes: Vec<GitIssueRef>,
}

impl Announced {
    fn of(held: &WorkSnapshot) -> Self {
        Self {
            work: work_from_scan(&held.diff, &held.cwd),
            pr: held.diff.pr.clone(),
            closes: held.diff.closes.clone(),
        }
    }
}

/// The seats a view is showing, and the loop each one runs.
#[derive(Default)]
pub(crate) struct HeldSeats {
    held: Mutex<HashMap<SessionSlot, Held>>,
}

struct Held {
    /// How many holders the seat has, so one view leaving does not stop a
    /// scan another still reads.
    count: usize,
    /// Dropped to stop the seat's loop, which also drops its watch.
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// One scan at a time for this seat: the hold's read and the loop's read
    /// are the same read, and a second one for the same tree is waste.
    scanning: Arc<tokio::sync::Mutex<()>>,
}

impl HeldSeats {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionSlot, Held>> {
        self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take a hold on `slot`, answering the seat's scan gate and the sender
    /// that stops its loop when this is the first hold.
    fn acquire(
        &self,
        slot: &SessionSlot,
    ) -> (Arc<tokio::sync::Mutex<()>>, Option<tokio::sync::oneshot::Receiver<()>>) {
        let mut held = self.lock();
        let entry = held.entry(slot.clone()).or_insert_with(|| Held {
            count: 0,
            stop: None,
            scanning: Arc::default(),
        });
        entry.count += 1;
        let scanning = Arc::clone(&entry.scanning);
        if entry.count > 1 {
            return (scanning, None);
        }
        let (stop, stopped) = tokio::sync::oneshot::channel();
        entry.stop = Some(stop);
        (scanning, Some(stopped))
    }

    /// Give a hold back.
    fn release(&self, slot: &SessionSlot) {
        let mut held = self.lock();
        let Some(entry) = held.get_mut(slot) else {
            return;
        };
        entry.count = entry.count.saturating_sub(1);
        if entry.count == 0 {
            held.remove(slot);
        }
    }
}

impl Workspace {
    /// Show `slot`, which is what starts its working tree being scanned.
    ///
    /// **A hold is what "somebody is looking" means**, and it is what keeps
    /// an idle seat from being scanned at all. The first hold also reads the
    /// tree when what is stored is missing or stale, so a page never opens on
    /// an arbitrarily old row; what that read answered becomes the row its
    /// viewers are already holding, so the loop announces only a change.
    ///
    /// **A seat with no session is not held at all.** Its scan would have
    /// nowhere to land - the store rides the seat's own record - so the rule
    /// would read "nothing has scanned this" on every poke and scan forever,
    /// a `gh` lookup apiece. Refusing the hold is what makes a view of a
    /// declared-but-unstarted project cost nothing.
    pub async fn hold_seat(self: &Arc<Self>, slot: &SessionSlot) {
        if self.domain_session_for(slot).is_none() {
            tracing::debug!(
                event_name = "work_hold_refused",
                slot = %slot.display(),
                reason = "no_session",
                "a seat with no session has no store for a scan, so nothing is held",
            );
            return;
        }
        let (scanning, stopped) = self.held_work_seats.acquire(slot);
        let Some(stopped) = stopped else {
            return;
        };
        let announced = match self.scan_work_if_stale(slot, &scanning, false).await {
            Ok(held) => Some(Announced::of(&held)),
            Err(refusal) => {
                tracing::debug!(
                    event_name = "work_scan_skipped",
                    slot = %slot.display(),
                    %refusal,
                    "a held seat's working tree was not read at the hold",
                );
                None
            }
        };
        spawn_work_watch(Arc::clone(self), slot.clone(), scanning, stopped, announced);
    }

    /// Stop showing `slot`. The last hold stops its loop and its watch.
    pub fn release_seat(&self, slot: &SessionSlot) {
        self.held_work_seats.release(slot);
    }

    /// The seat's scan, when one has been taken.
    pub fn work_snapshot(&self, slot: &SessionSlot) -> Option<WorkSnapshot> {
        self.domain_session_for(slot)?.lock().work_snapshot.clone()
    }

    /// Store a scan for `slot`, as the seat's loop and a fixture both do.
    pub fn store_work_snapshot(&self, slot: &SessionSlot, snapshot: WorkSnapshot) {
        if let Some(domain) = self.domain_session_for(slot) {
            domain.lock().work_snapshot = Some(snapshot);
        }
    }

    /// Answer the seat's scan, reading the tree first when the rule says to.
    ///
    /// **The rule is checked under the seat's own gate**, so the hold's read
    /// and the loop's read cannot both decide a stale tree needs two scans:
    /// the second one finds what the first stored.
    ///
    /// # Errors
    ///
    /// A seat with no session behind it has no directory to read, and forge
    /// answers that rather than scanning the process's own directory - the
    /// binary test every read here owes.
    pub async fn scan_work_if_stale(
        &self,
        slot: &SessionSlot,
        gate: &tokio::sync::Mutex<()>,
        dirty: bool,
    ) -> Result<WorkSnapshot, String> {
        let _one_at_a_time = gate.lock().await;
        let held = self.work_snapshot(slot);
        if should_scan(held.as_ref(), dirty, Instant::now()) {
            let Some(cwd) = self.cwd_for_session(slot) else {
                return Err(format!("{} has no directory to read", slot.display()));
            };
            return Ok(self.scan_work_at(slot, Path::new(&cwd)).await);
        }
        self.work_snapshot(slot).ok_or_else(|| format!("{} has no stored scan", slot.display()))
    }

    /// Read `cwd` as `slot`'s working tree, store what it answered, and
    /// answer it.
    ///
    /// One body for both entry points - the hold and the loop - because a
    /// second scan path is a second answer about one tree.
    pub async fn scan_work_at(&self, slot: &SessionSlot, cwd: &Path) -> WorkSnapshot {
        let previous = self.work_snapshot(slot).map(|held| held.diff);
        let diff = forge_agent::env::git_diff::scan(cwd, previous.as_ref()).await;
        let held = WorkSnapshot { diff, cwd: cwd.to_path_buf(), read_at: Instant::now() };
        self.store_work_snapshot(slot, held.clone());
        held
    }
}

/// One seat's scan loop: the tree is watched, the poke reads it, and a row
/// that moved goes to whoever is showing the seat.
fn spawn_work_watch(
    workspace: Arc<Workspace>,
    slot: SessionSlot,
    scanning: Arc<tokio::sync::Mutex<()>>,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
    announced: Option<Announced>,
) {
    tokio::spawn(async move {
        let Some(cwd) = workspace.cwd_for_session(&slot) else {
            tracing::debug!(
                event_name = "work_watch_skipped",
                slot = %slot.display(),
                reason = "cwd_unresolved",
                "a held seat's directory is not resolvable, so nothing is watched",
            );
            return;
        };
        // The ignore filter is on whatever the file index's own preference
        // says: git's view of a tree never includes an ignored path, so one
        // of those moving cannot change the row this loop watches for.
        let (changes, _watch) = start_change_watch(PathBuf::from(cwd), true);
        let mut announced = announced;
        let mut poke = tokio::time::interval(POKE_INTERVAL);
        poke.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = &mut stopped => return,
                _ = poke.tick() => {}
            }
            // Whatever the watch reported since the last look is one mark:
            // the tree moved, which is all a scan decision needs.
            let mut dirty = false;
            while changes.try_recv().is_ok() {
                dirty = true;
            }
            let Ok(held) = workspace.scan_work_if_stale(&slot, &scanning, dirty).await else {
                continue;
            };
            let row = Announced::of(&held);
            if announced.as_ref() == Some(&row) {
                continue;
            }
            workspace.update_tx.send(SessionUpdate::WorkChanged {
                key: slot.clone(),
                work: row.work.clone(),
                pr: row.pr.clone(),
                closes: row.closes.clone(),
            });
            announced = Some(row);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::Duration;

    use forge_primitives::git::GitBranch;
    use forge_primitives::git_diff::{LayerState, RepoGate};

    use super::*;

    fn seat() -> SessionSlot {
        SessionSlot::lead("TestOrg", "forge")
    }

    /// A workspace whose one project IS the repository above, so a lead seat
    /// resolves to a tree that is really there. The config dir comes back
    /// with it: the store under it is open for as long as the workspace is.
    ///
    /// The seat is NOT started: the project is declared and nothing runs
    /// behind it, which is the state a hold has to refuse.
    fn a_declared_project(
        repo: &Path,
    ) -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>, tempfile::TempDir)
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge dir");
        std::fs::write(
            forge.join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]

[[orgs.projects]]
name = "forge"
path = "{}"
auto_start = true

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
                repo.display(),
            ),
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        let updates = workspace.subscribe();
        (workspace, updates, dir)
    }

    /// [`a_declared_project`] with the seat started, which is what a hold
    /// needs: the store rides the seat's own record.
    fn a_workspace(
        repo: &Path,
    ) -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>, tempfile::TempDir)
    {
        let (workspace, updates, dir) = a_declared_project(repo);
        workspace.register_domain_session(seat(), None);
        (workspace, updates, dir)
    }

    /// Spawn git the way the product does, scrub included: the ambient
    /// repo-location variables would otherwise answer about whatever
    /// repository this suite is itself running inside.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
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

    /// A repository with one commit, which is the state a held seat's tree
    /// is read from.
    fn a_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.test"]);
        git(dir.path(), &["config", "user.name", "test"]);
        std::fs::write(dir.path().join("kept.txt"), "one").expect("write");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-qm", "first"]);
        dir
    }

    fn a_snapshot(read_at: Instant, cwd: &Path) -> WorkSnapshot {
        WorkSnapshot {
            diff: GitDiffSnapshot {
                branch: GitBranch::Detached,
                pushed_sha: None,
                pr_fetched_at: None,
                default_branch: None,
                repo_gate: RepoGate::NotARepo,
                worktree: LayerState::Clean,
                branch_ahead: LayerState::Clean,
                pr: None,
                closes: Vec::new(),
            },
            cwd: cwd.to_path_buf(),
            read_at,
        }
    }

    /// The rule the hold and the loop both read: nothing read yet, a tree
    /// that moved, or a read that has gone stale.
    #[test]
    fn a_scan_happens_for_nothing_read_a_dirty_tree_or_a_stale_read() {
        let now = Instant::now();
        let held = a_snapshot(now, Path::new("/tmp/nowhere"));

        assert!(should_scan(None, false, now), "nothing read yet is read");
        assert!(should_scan(Some(&held), true, now), "a tree that moved is read at once");
        assert!(
            !should_scan(Some(&held), false, now),
            "a fresh read of a still tree is not read again",
        );
        assert!(
            should_scan(Some(&held), false, now + SNAPSHOT_STALENESS),
            "and a read past the window is read whatever the watch said",
        );
    }

    /// A seat's tree is read because somebody is showing it, and a row that
    /// moved reaches them - while the hold's own read is what they already
    /// hold, so a still tree says nothing at all.
    #[tokio::test]
    async fn a_held_seat_announces_a_moved_row_and_says_nothing_while_it_is_still() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();

        workspace.hold_seat(&seat).await;

        let held = workspace.work_snapshot(&seat).expect("the hold reads the tree");
        let row = work_from_scan(&held.diff, &held.cwd);
        assert_eq!(row.gate, Gate::InRepo, "a checkout is read as a repository");
        assert_eq!(row.changed, Some(0), "and a clean one as nothing changed");
        assert!(
            updates.try_recv().is_err(),
            "the hold's read is what the seat's viewers already hold, so nothing is announced",
        );

        // Edited in a loop, and read for less than the staleness window: the
        // watch arms on its own thread, so the first edit can land before
        // notify is listening - and a test that wrote once and waited would
        // pass on the 10s rule instead, proving nothing about the watch.
        for edit in 1..12 {
            std::fs::write(dir.path().join("kept.txt"), "x".repeat(edit)).expect("write");
            let Ok(Some(moved)) =
                tokio::time::timeout(Duration::from_millis(700), updates.recv()).await
            else {
                continue;
            };
            let SessionUpdate::WorkChanged { key, work, .. } = moved else {
                panic!("a moved tree announces the row, got {moved:?}");
            };
            assert_eq!(key, seat, "and it is addressed to the seat that was held");
            assert_eq!(work.changed, Some(1), "with the count the edit gives");
            return;
        }
        panic!("an edit inside the staleness window never reached the seat's viewers");
    }

    /// The read the loop drives answers the tree's new state.
    #[tokio::test]
    async fn a_write_moves_the_row_the_read_answers() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat).await;
        let (gate, _stopped) = workspace.held_work_seats.acquire(&seat);

        // A TRACKED file: the row counts the worktree layer, which is
        // uncommitted edits against HEAD, and an untracked file is not one.
        std::fs::write(dir.path().join("kept.txt"), "two").expect("write");
        let held = workspace
            .scan_work_if_stale(&seat, &gate, true)
            .await
            .expect("the seat's tree is readable");

        assert_eq!(
            work_from_scan(&held.diff, &held.cwd).changed,
            Some(1),
            "the scan answers the edit the tree picked up",
        );
    }

    /// A seat with no session is not held, so nothing watches its tree.
    ///
    /// **The alternative burns scans.** The store rides the seat's own record,
    /// so with no record every write lands nowhere, the rule reads "nothing
    /// has scanned this" on every poke, and the loop scans once a second
    /// forever - a `gh` lookup apiece on a lead seat. Reachable the moment a
    /// client subscribes to a declared-but-unstarted project, whose cwd
    /// resolves from the declaration alone.
    #[tokio::test]
    async fn a_seat_with_no_session_is_not_held() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_declared_project(dir.path());
        let seat = seat();

        workspace.hold_seat(&seat).await;

        assert!(workspace.work_snapshot(&seat).is_none(), "nothing was read for it");
        assert!(workspace.held_work_seats.lock().is_empty(), "and nothing holds it");

        // A tree that moves says nothing: no loop is watching it. The burn
        // this catches emits within about a second, so the wait is a control
        // on the loop not existing rather than on a slow one.
        std::fs::write(dir.path().join("kept.txt"), "moved").expect("write");
        assert!(
            tokio::time::timeout(Duration::from_secs(2), updates.recv()).await.is_err(),
            "a seat with no session must not announce anything",
        );
    }

    /// A hold is counted: one viewer leaving does not stop the scan another
    /// still reads, and the last one lets the seat's loop go.
    #[tokio::test]
    async fn the_last_release_lets_the_seat_go() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();

        workspace.hold_seat(&seat).await;
        workspace.hold_seat(&seat).await;
        assert!(workspace.work_snapshot(&seat).is_some(), "the hold reads the seat's tree");

        workspace.release_seat(&seat);
        assert!(
            !workspace.held_work_seats.lock().is_empty(),
            "one viewer leaving keeps the seat held for the other",
        );
        workspace.release_seat(&seat);
        assert!(
            workspace.held_work_seats.lock().is_empty(),
            "and the last one lets the seat and its loop go",
        );
        assert!(workspace.work_snapshot(&seat).is_some(), "while what was read stays readable");
    }
}
