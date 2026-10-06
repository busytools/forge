//! The seat's working tree: what a scan of it found, when that was, and the
//! loop that keeps it fresh for the seats a view is showing.
//!
//! **The row is pushed, not polled.** A view used to read the tree through
//! whatever read happened to need it, which for a client meant a whole-record
//! encode every time it wanted one number. Here the scan runs for a seat
//! somebody is looking at, its answer is stored beside the process walk, and
//! the row moves by itself as [`SessionUpdate::WorkChanged`].
//!
//! A seat nobody holds is not scanned: the hold IS what looking means, so a
//! fleet of unheld seats costs nothing. A HELD seat costs one scan per
//! [`SNAPSHOT_STALENESS`] while its tree is still, and at most one per
//! [`POKE_INTERVAL`] while it moves. And a seat whose row has not moved
//! announces nothing, however often it is scanned - the frame says the tree
//! moved, never that time passed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_agent::env::file_index::start_change_watch;
use forge_agent::env::processes::SCAN_STALENESS;
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

/// How long a walked file index answers for: the window the socket's own read
/// served it for before the push landed, so a seat's `@` list is no fresher
/// and no staler than it was.
pub const INDEX_STALENESS: Duration = Duration::from_secs(5);

/// A seat's walked file index, and when it was taken.
#[derive(Clone)]
pub struct HeldFileIndex {
    pub index: Arc<crate::file_index::FileIndex>,
    pub read_at: Instant,
}

/// Whether the tree should be read again.
///
/// Three reasons, and the middle one is what watching buys: nothing read yet,
/// a tree that moved since the last read, and a read that has simply gone
/// stale - the last being what covers everything the watcher cannot report,
/// a commit or a stage among them.
pub fn should_scan(snapshot: Option<&WorkSnapshot>, dirty: bool, now: Instant) -> bool {
    reads_again(snapshot.map(|held| held.read_at), dirty, now, SNAPSHOT_STALENESS)
}

/// Whether a read whose answer was taken at `read_at` should be taken again,
/// on the same three reasons for every read this module owns.
fn reads_again(read_at: Option<Instant>, dirty: bool, now: Instant, window: Duration) -> bool {
    match read_at {
        None => true,
        Some(at) => dirty || now.saturating_duration_since(at) >= window,
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
    /// **A held seat waits for its session.** A seat with no session has no
    /// store for a scan, so the watch's own loop goes quiet on it and wakes
    /// when the session appears - the same rule that covers a session ending
    /// under a running loop.
    ///
    /// **What the old refusal cost was the viewer that never comes back.** A
    /// subscribe to a sessionless seat was refused its hold, and most viewers
    /// recovered by accident: a session starting is a REPLACES frame, the
    /// reread around it unsubscribe/subscribes, and the next subscribe took
    /// the hold - the `work_watch_quiet / session_gone` line in the live log
    /// is one of those later holds. A viewer that nothing drove to
    /// re-subscribe had no watch for as long as it showed the seat, and its
    /// pushed rows stood still; the hold is unconditional now, so the FIRST
    /// subscribe is the one that counts (#1706).
    ///
    /// The first read, walk and index run only when a session is there to
    /// store them under: a sessionless seat's stores drop what they are
    /// given, so the work would be a whole-tree walk with nowhere to land.
    pub async fn hold_seat(self: &Arc<Self>, slot: &SessionSlot) {
        let (scanning, stopped) = self.held_work_seats.acquire(slot);
        let Some(stopped) = stopped else {
            return;
        };
        let live = self.domain_session_for(slot).is_some();
        let announced = if live {
            match self.scan_work_if_stale(slot, &scanning, false).await {
                Ok(held) => Some(Announced::of(&held)),
                Err(refusal) => {
                    tracing::debug!(
                        target: "forge_workspace::work",
                        event_name = "work_scan_skipped",
                        slot = %slot.display(),
                        %refusal,
                        "a held seat's working tree was not read at the hold",
                    );
                    None
                }
            }
        } else {
            None
        };
        // The walk is the hold's too, for the same reason the tree's scan is:
        // the snapshot a page opens on is this seat's, and a walk left to the
        // loop would leave the first one as old as the last time anybody
        // looked.
        let walked = if live {
            self.walk_processes_if_stale(slot, self.claude_pid(slot)).await;
            self.process_snapshot(slot).map(|held| held.processes)
        } else {
            None
        };
        // The file index is the same bargain: a page that opens on the seat
        // reads the list its composer's `@` trigger offers, so the hold walks
        // it once rather than leaving the composer empty until the first poke.
        let indexed = if live {
            self.walk_files_if_stale(slot, false).await;
            self.file_index(slot).map(|held| held.index)
        } else {
            None
        };
        spawn_work_watch(
            Arc::clone(self),
            slot.clone(),
            scanning,
            stopped,
            announced,
            walked,
            indexed,
        );
    }

    /// Stop showing `slot`. The last hold stops its loop and its watch.
    pub fn release_seat(&self, slot: &SessionSlot) {
        self.held_work_seats.release(slot);
    }

    /// The seat's scan, when one has been taken.
    pub fn work_snapshot(&self, slot: &SessionSlot) -> Option<WorkSnapshot> {
        self.domain_session_for(slot)?.lock().work_snapshot.clone()
    }

    /// Walk `slot`'s process tree when the snapshot held is missing or older
    /// than [`SCAN_STALENESS`], and store what the walk found.
    ///
    /// **The held seat's own loop is the walker** - the terminal walks the one
    /// seat a person is on, and the socket's reads used to walk the seat a
    /// client read. This is that role moved to the loop, so a seat nobody
    /// holds is not walked at all and the walk costs what a held seat's tree
    /// costs rather than a whole record per read.
    ///
    /// The session's live backgrounded `local_bash` commands ride along, from
    /// the registry the core holds: a backgrounded bash is `setsid`-detached
    /// and sits outside claude's tree, so without them the walk misses the
    /// very processes the feed leads with.
    pub(crate) async fn walk_processes_if_stale(
        &self,
        slot: &SessionSlot,
        pid: Option<u32>,
    ) -> bool {
        let Some(pid) = pid else {
            return false;
        };
        let stale = self
            .process_snapshot(slot)
            .is_none_or(|held| held.scanned_at.elapsed().is_ok_and(|age| age >= SCAN_STALENESS));
        if !stale {
            return false;
        }
        let commands = local_bash_commands(
            &self
                .domain_session_for(slot)
                .map(|domain| domain.lock().background_tasks.clone())
                .unwrap_or_default(),
        );
        // `sysinfo`'s refresh is a CPU-bound system call rather than async I/O,
        // so it runs on the blocking pool.
        match tokio::task::spawn_blocking(move || forge_agent::env::processes::scan(pid, &commands))
            .await
        {
            Ok(snapshot) => {
                self.store_process_snapshot(slot, Some(snapshot));
                true
            }
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::work",
                    event_name = "process_walk_failed",
                    %error,
                    slot = %slot.display(),
                    "the process walk did not finish; the seat keeps the snapshot it had",
                );
                false
            }
        }
    }

    /// Walk `slot`'s tree into a file index when the one held is missing, the
    /// tree moved, or the answer is past [`INDEX_STALENESS`], and store what
    /// the walk found.
    ///
    /// **The held seat's own loop is the walker**, the role the process walk
    /// took: the socket's reads used to walk their seat, and this walks only
    /// the seats somebody is showing. The walk honours the user's own
    /// gitignore preference, read per walk so a flip reaches the next one.
    pub(crate) async fn walk_files_if_stale(&self, slot: &SessionSlot, dirty: bool) -> bool {
        let stale = reads_again(
            self.file_index(slot).map(|held| held.read_at),
            dirty,
            Instant::now(),
            INDEX_STALENESS,
        );
        if !stale {
            return false;
        }
        let Some(cwd) = self.cwd_for_session(slot) else {
            return false;
        };
        let respect = crate::file_index::respect_gitignore(self.user_preferences().as_ref());
        let root = PathBuf::from(cwd);
        // The walk is a whole tree, so it runs on the blocking pool rather
        // than on the reactor.
        match tokio::task::spawn_blocking(move || {
            crate::file_index::FileIndex::scan(&root, respect)
        })
        .await
        {
            Ok(index) => {
                self.store_file_index(slot, Arc::new(index));
                true
            }
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::work",
                    event_name = "file_index_walk_failed",
                    %error,
                    slot = %slot.display(),
                    "the file-index walk did not finish; the seat keeps the index it had",
                );
                false
            }
        }
    }

    /// Store a walked file index for `slot`, as the seat's loop does.
    pub fn store_file_index(&self, slot: &SessionSlot, index: Arc<crate::file_index::FileIndex>) {
        if let Some(domain) = self.domain_session_for(slot) {
            domain.lock().file_index = Some(HeldFileIndex { index, read_at: Instant::now() });
        }
    }

    /// The seat's walked file index, when one has been taken.
    pub fn file_index(&self, slot: &SessionSlot) -> Option<HeldFileIndex> {
        self.domain_session_for(slot)?.lock().file_index.clone()
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
        // A seat with no session has nowhere to store a scan, so the rule
        // would read "nothing has scanned this" on every poke. The loop's own
        // guard skips the call while sessionless, and this refusal is the
        // second layer behind it: the hold's first read is already gated on
        // `live`, but a session can END under a held seat between the two, and
        // then the next poke must refuse here rather than scan into nothing.
        if self.domain_session_for(slot).is_none() {
            return Err(format!("{} has no session to store a scan on", slot.display()));
        }
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

/// Whether a walk carries news: the entries the seat's viewers were last told
/// against the ones this walk found.
///
/// **`None` is "nothing ever announced", not "an empty tree"**, so the first
/// walk of a seat that holds nothing still announces.
///
/// **Equality is over the whole entry, so a merely running tree is news**:
/// `memory_bytes` drifts as processes work, so a held seat with a live tree
/// announces about once per poke rather than only when a process arrives.
/// Narrowing the comparison to ignore the memory figure would freeze the
/// number every viewer draws at whatever the first walk found.
fn walk_moved(
    walked: Option<&Vec<forge_agent::env::processes::ProcessEntry>>,
    snapshot: &forge_agent::env::processes::ProcessSnapshot,
) -> bool {
    walked != Some(&snapshot.processes)
}

/// The commands a walk hands the OS scan: each running `local_bash` task's own
/// command.
///
/// A task whose card forge has not seen carries no command and is skipped -
/// the terminal's own rule, where a rostered bash with no recorded command
/// draws its registry row instead of being adopted by the walk.
fn local_bash_commands(tasks: &[crate::BackgroundTask]) -> Vec<String> {
    tasks
        .iter()
        .filter(|task| task.task_type == "local_bash")
        .filter_map(|task| task.command.clone())
        .collect()
}

/// One seat's scan loop: the tree is watched, the poke reads it and walks the
/// seat's process tree and its files, and whatever moved goes to whoever is
/// showing the seat.
fn spawn_work_watch(
    workspace: Arc<Workspace>,
    slot: SessionSlot,
    scanning: Arc<tokio::sync::Mutex<()>>,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
    announced: Option<Announced>,
    walked: Option<Vec<forge_agent::env::processes::ProcessEntry>>,
    indexed: Option<Arc<crate::file_index::FileIndex>>,
) {
    tokio::spawn(async move {
        let Some(cwd) = workspace.cwd_for_session(&slot) else {
            tracing::debug!(
                target: "forge_workspace::work",
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
        let mut walked = walked;
        let mut indexed = indexed;
        // Whether the last poke found the seat's session gone, so the quiet
        // spell is reported once rather than once a second.
        let mut quiet = false;
        let mut poke = tokio::time::interval(POKE_INTERVAL);
        poke.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = &mut stopped => return,
                _ = poke.tick() => {}
            }
            // **A seat whose session is not there - not started yet, or ended
            // under its loop - goes quiet, and does not go away.** There is
            // nothing to store or announce for a session that is absent, so
            // this skips the whole body. But the LOOP stays, and so does the
            // hold: a session that comes back under the same seat resumes
            // being scanned under the viewer that was already showing it, with
            // no re-subscribe and no entry whose loop has gone - and that is
            // the wake the sessionless hold depends on.
            //
            // **This guard is load-bearing; do not delete it as redundant
            // with the scan's own refusal.** Deleting either quiet layer
            // alone leaves every test green (the other swallows the
            // emission), so no mutation says it - but without this one, a
            // sessionless seat walks+scans its whole tree every poke with
            // every result dropped, and nothing observable would say so.
            if workspace.domain_session_for(&slot).is_none() {
                if !quiet {
                    quiet = true;
                    tracing::debug!(
                        target: "forge_workspace::work",
                        event_name = "work_watch_quiet",
                        slot = %slot.display(),
                        reason = "no_session",
                        "the seat has no session - not started yet, or ended under its loop - so \
                         nothing is scanned until one is there",
                    );
                }
                continue;
            }
            quiet = false;
            // The walk, on its own rule: missing or older than the terminal's
            // own second, which is this poke's interval. It runs before the
            // tree's read and takes no part in that read's early exits.
            workspace.walk_processes_if_stale(&slot, workspace.claude_pid(&slot)).await;
            // **The compare runs whoever walked.** The terminal's scanner writes
            // this same store, and while it is showing the seat the store is
            // never stale - so a loop that read the store only when its own
            // walk ran would leave every client's walk frozen for as long as
            // the terminal held the seat.
            if let Some(walk) = workspace.process_snapshot(&slot)
                && walk_moved(walked.as_ref(), &walk)
            {
                walked = Some(walk.processes.clone());
                workspace
                    .update_tx
                    .send(SessionUpdate::ProcessesChanged { key: slot.clone(), snapshot: walk });
            }
            // Whatever the watch reported since the last look is one mark:
            // the tree moved, which is all a scan decision needs.
            let mut dirty = false;
            while changes.try_recv().is_ok() {
                dirty = true;
            }
            // The file index, on the same mark and the same poke: the watch
            // that moved the tree is also what a `@` list would have to
            // follow, and [`INDEX_STALENESS`] is what keeps a build
            // directory's churn from becoming a frame per write.
            workspace.walk_files_if_stale(&slot, dirty).await;
            if let Some(held) = workspace.file_index(&slot)
                && indexed.as_ref() != Some(&held.index)
            {
                indexed = Some(Arc::clone(&held.index));
                workspace
                    .update_tx
                    .send(SessionUpdate::FileIndexChanged { key: slot.clone(), index: held.index });
            }
            let Ok(held) = workspace.scan_work_if_stale(&slot, &scanning, dirty).await else {
                tracing::debug!(
                    target: "forge_workspace::work",
                    event_name = "work_watch_scan_refused",
                    slot = %slot.display(),
                    "the seat's tree was not read this poke",
                );
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
    /// behind it - the state a hold takes and waits on (#1706), and the shape
    /// most of these tests begin from.
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

    /// **A viewer that arrives before the seat's session does still gets the
    /// tree once it starts** (#1706).
    ///
    /// A viewer that nothing drives to RE-SUBSCRIBE is the one the old refusal
    /// stranded: most recovered by accident, because a session starting is a
    /// REPLACES frame and the reread around it takes the hold again (the live
    /// log's `work_watch_quiet / session_gone` is one of those later holds) -
    /// but a viewer whose seat simply starts (or stays asleep) under it had no
    /// watch, and its pushed rows stood still. The hold is unconditional, so
    /// the FIRST subscribe is the one that counts.
    #[tokio::test]
    async fn a_seat_held_before_its_session_starts_announces_the_row_once_it_does() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_declared_project(dir.path());
        let seat = seat();

        workspace.hold_seat(&seat).await;
        assert!(
            !workspace.held_work_seats.lock().is_empty(),
            "a sessionless seat is still held - the loop waits quietly for its session",
        );
        workspace.register_domain_session(seat.clone(), None);

        for edit in 1..12 {
            std::fs::write(dir.path().join("kept.txt"), "x".repeat(edit)).expect("write");
            let Ok(Some(moved)) =
                tokio::time::timeout(Duration::from_millis(700), updates.recv()).await
            else {
                continue;
            };
            // Other variants ride the same stream - the file index announces
            // too - and the row under test is the work one.
            let SessionUpdate::WorkChanged { key, work, .. } = moved else {
                continue;
            };
            assert_eq!(key, seat, "and it is addressed to the seat that was held");
            assert_eq!(work.changed, Some(1), "with the count the edit gives");
            return;
        }
        panic!("a seat held before its session started never announced its row");
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

    /// The commands a walk hands the OS scan: the running bash tasks, whose
    /// detached processes sit outside claude's tree, and not the agent tasks
    /// beside them, which the walk never adopts. A task whose card forge has
    /// not seen has no command and is left to the registry's own row.
    #[test]
    fn only_local_bash_tasks_hand_the_scan_a_command() {
        let tasks = vec![
            crate::BackgroundTask {
                task_id: "t1".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "gh run watch".to_owned(),
                command: Some("gh run watch 123 --exit-status".to_owned()),
                tool_use_id: None,
            },
            // The agent task CARRIES a command: the hold that records one is
            // not scoped to a card's tool, so the type is the thing that keeps
            // it out of the walk (the terminal's own rule and test).
            crate::BackgroundTask {
                task_id: "t2".to_owned(),
                task_type: "local_agent".to_owned(),
                description: "a sub-agent".to_owned(),
                command: Some("investigate".to_owned()),
                tool_use_id: None,
            },
            crate::BackgroundTask {
                task_id: "t3".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "a bash whose card forge never saw".to_owned(),
                command: None,
                tool_use_id: None,
            },
        ];

        assert_eq!(
            local_bash_commands(&tasks),
            vec!["gh run watch 123 --exit-status".to_owned()],
            "only a bash task's own command is handed to the walk",
        );
    }

    /// The walk's window, the terminal's own rule: nothing walked yet is
    /// walked, one inside the window is the answer rather than a reason to
    /// walk, and one past it is walked again and stored where both views
    /// read it.
    #[tokio::test]
    async fn a_walk_happens_only_once_the_window_has_passed() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        let pid = Some(std::process::id());

        assert!(
            workspace.walk_processes_if_stale(&seat, pid).await,
            "nothing walked yet is walked"
        );
        let walked = workspace.process_snapshot(&seat).expect("the walk stored a snapshot");

        assert!(
            !workspace.walk_processes_if_stale(&seat, pid).await,
            "a snapshot inside the window is the answer rather than a reason to walk",
        );

        let long_ago = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(60))
            .expect("a minute ago");
        workspace.store_process_snapshot(
            &seat,
            Some(forge_agent::env::processes::ProcessSnapshot {
                processes: walked.processes.clone(),
                scanned_at: long_ago,
            }),
        );
        assert!(
            workspace.walk_processes_if_stale(&seat, pid).await,
            "a snapshot past the window is walked again",
        );
        assert!(
            workspace.process_snapshot(&seat).is_some_and(|held| held.scanned_at > long_ago),
            "and stored where the views read it",
        );
    }

    /// A seat with no process to walk is not walked, and keeps the snapshot
    /// it had rather than having an empty walk put in its place: an invented
    /// empty snapshot would draw as `no processes` where the truth is
    /// `nothing known`.
    #[tokio::test]
    async fn a_seat_with_no_process_to_walk_keeps_what_it_had() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        let held = std::time::SystemTime::now();
        workspace.store_process_snapshot(
            &seat,
            Some(forge_agent::env::processes::ProcessSnapshot {
                processes: Vec::new(),
                scanned_at: held,
            }),
        );

        assert!(!workspace.walk_processes_if_stale(&seat, None).await, "nothing is walked");
        assert_eq!(
            workspace.process_snapshot(&seat).map(|snapshot| snapshot.scanned_at),
            Some(held),
            "and the snapshot it had is kept rather than replaced",
        );
    }

    /// One process as a walk reports it, for a fixture that only needs an entry
    /// that is there.
    fn entry_at(pid: u32) -> forge_agent::env::processes::ProcessEntry {
        forge_agent::env::processes::ProcessEntry {
            pid,
            parent_pid: 1,
            name: "claude".to_owned(),
            command: "claude".to_owned(),
            memory_bytes: 1,
        }
    }

    /// A walk of `entries`, taken now.
    fn walk_of(
        entries: Vec<forge_agent::env::processes::ProcessEntry>,
    ) -> forge_agent::env::processes::ProcessSnapshot {
        forge_agent::env::processes::ProcessSnapshot {
            processes: entries,
            scanned_at: std::time::SystemTime::now(),
        }
    }

    /// A child process that goes when the test does, however it ends - the
    /// tree a walk is driven against in these cases.
    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A walk carries news only when the tree moved, and the FIRST walk always
    /// does - `None` is nothing ever announced, not an empty tree.
    #[test]
    fn a_walk_carries_news_only_when_the_tree_moved() {
        assert!(walk_moved(None, &walk_of(Vec::new())), "the first walk is news whatever it found");
        assert!(walk_moved(None, &walk_of(vec![entry_at(1)])), "and so is a tree of one");

        let told = vec![entry_at(1)];
        assert!(
            !walk_moved(Some(&told), &walk_of(vec![entry_at(1)])),
            "the same tree says nothing",
        );
        assert!(
            walk_moved(Some(&told), &walk_of(vec![entry_at(1), entry_at(2)])),
            "a process arriving is news",
        );
        assert!(walk_moved(Some(&told), &walk_of(Vec::new())), "and so is the tree emptying");
    }

    /// **A walk somebody else moved reaches the seat's viewers.** The terminal's
    /// scanner writes this same store, and while it is showing the seat the
    /// store is never stale by the loop's own rule - so a loop that read the
    /// store only when its own walk ran would leave every client's walk frozen
    /// for as long as the terminal held the seat.
    #[tokio::test]
    async fn a_walk_another_writer_moved_is_announced() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat).await;

        // The terminal's scanner, writing the store the loop holds a baseline
        // of - and with the clock inside the window, so the loop's own rule
        // walks nothing this poke.
        let moved_walk = walk_of(vec![entry_at(4242)]);
        workspace.store_process_snapshot(&seat, Some(moved_walk));

        let Ok(Some(moved)) = tokio::time::timeout(Duration::from_secs(3), updates.recv()).await
        else {
            panic!("a walk the terminal moved never reached the seat's viewers");
        };
        let SessionUpdate::ProcessesChanged { key, snapshot } = moved else {
            panic!("a moved walk announces the walk, got {moved:?}");
        };
        assert_eq!(key, seat, "and it is addressed to the seat that was held");
        assert_eq!(
            snapshot.processes,
            vec![entry_at(4242)],
            "with the walk the store holds, not one this loop took",
        );
    }

    /// **The seat's own tree reaches its viewers.** The walk is the loop's, so
    /// a held seat whose tree gains a process must see it; the two ways that
    /// breaks are the loop walking nothing and the announcement being dropped,
    /// and either leaves every client's section stale for good.
    ///
    /// The announcement is waited for BY THE CHILD rather than as the next one
    /// off the stream: these legs walk a real tree, and a running tree drifts
    /// (`walk_moved`), so a poke that finds only the drift is a legitimate
    /// frame that carries no child yet.
    #[tokio::test]
    async fn a_process_arriving_under_a_held_seat_is_announced() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        // The pid the loop walks is this test's own, so a child spawned below
        // is a process under the seat's tree.
        workspace.seed_test_claude_pid(&seat, std::process::id());
        workspace.hold_seat(&seat).await;

        let child =
            Child(std::process::Command::new("sleep").arg("30").spawn().expect("a child to find"));
        let child_pid = child.0.id();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("a process arriving under the seat was never announced");
            };
            let SessionUpdate::ProcessesChanged { key, snapshot } = moved else {
                continue;
            };
            assert_eq!(key, seat);
            if snapshot.processes.iter().any(|entry| entry.pid == child_pid) {
                break;
            }
        }
        drop(child);
    }

    /// **The hold's walk is what the seat's viewers were already handed.** The
    /// subscription snapshot answers from the store the hold just wrote, so a
    /// baseline read taken before that write would re-announce the tree the
    /// page already holds at the loop's first compare.
    ///
    /// The pid is a `sleep` the test owns, not the test process itself: what
    /// is walked is that process's DESCENDANTS, and a `sleep` has none - so
    /// the tree is empty and stays empty, which is what makes "nothing is
    /// announced" a property of the code rather than of the machine's load.
    /// The test process's own tree drifts as the suite runs, and drift is news.
    #[tokio::test]
    async fn the_holds_walk_is_not_announced_again() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        let walked =
            Child(std::process::Command::new("sleep").arg("30").spawn().expect("a child to walk"));
        workspace.seed_test_claude_pid(&seat, walked.0.id());
        // A walk from before the hold, past the window so the hold's own walk
        // replaces it - which is the baseline the loop must be seeded with.
        let mut stale = walk_of(vec![entry_at(4242)]);
        stale.scanned_at = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(60))
            .expect("a minute ago");
        workspace.store_process_snapshot(&seat, Some(stale));

        workspace.hold_seat(&seat).await;

        assert!(
            tokio::time::timeout(Duration::from_secs(2), updates.recv()).await.is_err(),
            "the hold's walk is what the seat's viewers already hold, so nothing is announced",
        );
    }

    /// **The mark the watch sets is what makes the walk run inside the
    /// window.** A tree that moved is walked on the very next poke; one that
    /// did not waits for [`INDEX_STALENESS`] - and this drives the walk
    /// directly rather than racing the file watcher, whose promptness is the
    /// machine's business rather than this rule's.
    #[tokio::test]
    async fn the_watchs_mark_walks_inside_the_window() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat).await;
        let held = workspace.file_index(&seat).expect("the hold walks the index");

        assert!(
            !workspace.walk_files_if_stale(&seat, false).await,
            "a still tree inside the window is the answer rather than a reason to walk",
        );

        std::fs::write(dir.path().join("new.rs"), "").expect("write");
        assert!(workspace.walk_files_if_stale(&seat, true).await, "the mark walks at once");

        let walked = workspace.file_index(&seat).expect("the walk stored an index");
        assert!(walked.index.entries.contains_key("new.rs"), "with what the walk found");
        assert!(walked.read_at > held.read_at, "and the answer is the newer one");
    }

    /// **A file written into a held seat's tree reaches its viewers.** The walk
    /// is the loop's, so a composer's `@` list follows the tree the person is
    /// working in.
    #[tokio::test]
    async fn a_file_written_into_a_held_seat_reaches_its_viewers() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat).await;

        std::fs::write(dir.path().join("new.rs"), "").expect("write");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("a file written into the seat's tree was never announced");
            };
            let SessionUpdate::FileIndexChanged { key, index } = moved else {
                continue;
            };
            assert_eq!(key, seat);
            if index.entries.contains_key("new.rs") {
                break;
            }
        }
    }

    /// **Writes inside one poke arrive as ONE frame carrying all of them.**
    /// The watch's report is a mark, not a trigger: a walk per report would
    /// put one frame on the wire per file a build touches, which is the
    /// throttle a `@` list does not need and the socket will not merge.
    #[tokio::test]
    async fn writes_inside_one_poke_arrive_as_one_frame() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat).await;

        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(dir.path().join(name), "").expect("write");
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let first = loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("the writes were never announced");
            };
            if let SessionUpdate::FileIndexChanged { index, .. } = moved {
                break index;
            }
        };
        for name in ["a.rs", "b.rs", "c.rs"] {
            assert!(
                first.entries.contains_key(name),
                "one frame carries {name}: {:?}",
                first.entries.keys().collect::<Vec<_>>(),
            );
        }

        // And nothing follows: one mark, one walk, one frame.
        let quiet = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some(moved)) = tokio::time::timeout_at(quiet, updates.recv()).await {
            if matches!(moved, SessionUpdate::FileIndexChanged { .. }) {
                panic!("a second frame followed one poke's writes");
            }
        }
    }

    /// **Two walks that find the same index say nothing.** Unlike the process
    /// entries, a `FileCandidate` carries only paths and depths - no memory
    /// figure to drift as the tree works - so a still tree is genuinely still,
    /// and the comparison can promise silence rather than near-silence. The
    /// window is what makes this a walk: the poke at the far end of it takes
    /// one whether anything moved or not.
    #[tokio::test]
    async fn a_still_tree_announces_nothing() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat).await;

        assert!(
            tokio::time::timeout(Duration::from_secs(7), updates.recv()).await.is_err(),
            "a still tree announced something",
        );
    }

    /// A walk is an answer for the preference it was built under, and the
    /// seat's loop reads the preference per walk: a flip reaches the next one
    /// rather than waiting for a restart.
    #[tokio::test]
    async fn a_preference_flip_moves_the_next_walk() {
        let dir = a_repo();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").expect("write");
        std::fs::write(dir.path().join("ignored.rs"), "").expect("write");
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.seed_test_user_preferences(serde_json::json!({}));
        workspace.hold_seat(&seat).await;

        let held = workspace.file_index(&seat).expect("the hold walks the index");
        assert!(
            !held.index.entries.contains_key("ignored.rs"),
            "the walk respects the file while the preference says so",
        );

        workspace.seed_test_user_preferences(serde_json::json!({"respectGitignore": false}));
        assert!(workspace.walk_files_if_stale(&seat, true).await, "a flip is a walk");

        let walked = workspace.file_index(&seat).expect("the walk stored an index");
        assert!(
            walked.index.entries.contains_key("ignored.rs"),
            "and takes the preference read at the next walk",
        );
    }

    /// **A viewer's own hold is released exactly once.** A sessionless seat
    /// is held like any other since #1706 - its loop waits for the session -
    /// so the release is counted per seat and the accounting is the whole
    /// property: every viewer that subscribed holds once and gives back
    /// exactly once, and one viewer's clean-up never spends another's count.
    #[tokio::test]
    async fn a_hold_is_released_exactly_once_per_viewer() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();

        workspace.hold_seat(&seat).await;
        assert!(!workspace.held_work_seats.lock().is_empty(), "the first viewer holds");

        // The session ends under the hold, and the next viewer still holds:
        // the watch waits for the session rather than refusing (#1706).
        workspace.release_session_with_cascade(&seat);
        workspace.hold_seat(&seat).await;

        // Two taken holds: one release leaves the seat held, the second lets
        // it go.
        workspace.release_seat(&seat);
        assert!(
            !workspace.held_work_seats.lock().is_empty(),
            "one viewer is still holding the seat",
        );
        workspace.release_seat(&seat);
        assert!(
            workspace.held_work_seats.lock().is_empty(),
            "and the last release lets the seat go",
        );
    }

    /// A session that ends under a held seat goes quiet, and the hold stands.
    ///
    /// The burn this prevents has a second entrance: the store rides the
    /// seat's record, so a seat whose session ended has nowhere to write, and
    /// the rule would read "nothing has scanned this" on every poke - a `gh`
    /// lookup apiece, announcing a tree for a seat that is not there. What
    /// must NOT happen is the seat being let go: the viewer is still showing
    /// it, and a session that comes back resumes being scanned under them.
    #[tokio::test]
    async fn a_session_ending_quiets_the_seat_without_letting_it_go() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat).await;
        assert!(!workspace.held_work_seats.lock().is_empty(), "the hold is taken");

        workspace.release_session_with_cascade(&seat);

        // A move under the dead session says nothing: the rule refuses, so
        // nothing is scanned and nothing is announced.
        std::fs::write(dir.path().join("kept.txt"), "after the session").expect("write");
        assert!(
            tokio::time::timeout(Duration::from_secs(3), updates.recv()).await.is_err(),
            "a seat whose session ended must not announce anything",
        );
        assert!(
            !workspace.held_work_seats.lock().is_empty(),
            "and the hold stands, so the session coming back is scanned under it",
        );

        // The session returns: the tree moves again, and this time it is told.
        workspace.register_domain_session(seat.clone(), None);
        let told = std::fs::write(dir.path().join("kept.txt"), "the session is back").is_ok();
        let announced = tokio::time::timeout(Duration::from_secs(3), updates.recv()).await.is_ok();
        assert!(
            announced,
            "a session that comes back under a held seat resumes being scanned (write ok: {told})",
        );
    }

    /// **A sessionless seat is held, and its loop says nothing.** The hold
    /// waits for the session (#1706); the burn the old refusal guarded
    /// against is the quiet rules' business - no session means no record to
    /// store a scan in, so the loop waits instead of scanning once a second
    /// forever with a `gh` lookup apiece.
    ///
    /// **Both quiet layers are load-bearing and this test covers only the
    /// PAIR** (measured): the loop's guard and the scan's own sessionless
    /// refusal each swallow the other's absence - delete either alone and this
    /// stays green, and only both gone emit. The walk the guard skips is not
    /// observable either: a sessionless seat's stores drop what they are given,
    /// so there is no state a control could read. What this holds is that a
    /// sessionless seat is silent and held; the reason the guard can never be
    /// dropped as "redundant, the scan refuses anyway" lives in the comment
    /// there, not in a mutation.
    ///
    /// Reachable the moment a client subscribes to a declared-but-unstarted
    /// project, whose cwd resolves from the declaration alone - and that
    /// order is the everyday one for a client, so the hold has to be there
    /// when the session arrives.
    #[tokio::test]
    async fn a_sessionless_seat_is_held_and_says_nothing() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_declared_project(dir.path());
        let seat = seat();

        workspace.hold_seat(&seat).await;

        assert!(workspace.work_snapshot(&seat).is_none(), "nothing was read for it");
        assert!(
            !workspace.held_work_seats.lock().is_empty(),
            "and the hold stands, waiting for the session",
        );

        // A tree that moves says nothing while there is no session to store a
        // scan under. (Not phrased as a control on the guard alone: see the
        // doc above - only the pair is observable.)
        std::fs::write(dir.path().join("kept.txt"), "moved").expect("write");
        assert!(
            tokio::time::timeout(Duration::from_secs(2), updates.recv()).await.is_err(),
            "a sessionless seat's held loop must not announce anything",
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
