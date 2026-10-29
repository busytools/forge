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
//! [`SNAPSHOT_STALENESS`] while its tree is still, at most one per
//! [`POKE_INTERVAL`] while it moves, and one when its PR lookup is due - the
//! record a read serves carries the PR the last scan knew, so this loop is
//! where the refresh is paid. And a seat whose row has not moved
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
use forge_primitives::git_diff::{BaseState, GitDiffSnapshot, GitWorkView, PrRefresh};

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

/// The tree behind a row's depth, as both the record and its frames carry it:
/// the two layers a row draws, with `Clean` and `ScanFailed` both reading as
/// nothing to state - which is what the layer means, and the row above still
/// carries the gate that says which.
///
/// `base` is the state the seat's last watch refresh measured for the layer's
/// base. **It rides the row so a base-only move is news**: the range is
/// `base..HEAD`, so an upstream advance the branch does not contain moves
/// `origin/main` while the count and the chain stay exactly as they were, and
/// a review page drawing `current` over it would keep a label nobody
/// re-checked.
pub fn git_work_view(diff: &GitDiffSnapshot, base: Option<BaseState>) -> GitWorkView {
    let worktree = match &diff.worktree {
        forge_primitives::git_diff::LayerState::Populated(stats) => Some(stats.clone()),
        forge_primitives::git_diff::LayerState::Clean
        | forge_primitives::git_diff::LayerState::ScanFailed => None,
    };
    let ahead = match &diff.branch_ahead {
        forge_primitives::git_diff::LayerState::Populated(chain) => Some(chain.clone()),
        forge_primitives::git_diff::LayerState::Clean
        | forge_primitives::git_diff::LayerState::ScanFailed => None,
    };
    GitWorkView { default_branch: diff.default_branch.clone(), worktree, ahead, base }
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
    /// What the seat's last watch refresh measured for the base, carried
    /// across scans that do not refresh. The row draws it, so a base that
    /// moved without the count moving is still a frame.
    pub base: Option<BaseState>,
}

/// How long a scan answers for: the staleness rule for the snapshot every
/// view draws, so all views live with one freshness.
pub const SNAPSHOT_STALENESS: Duration = Duration::from_secs(10);

/// How long a drawing read waits for a scan already in flight.
///
/// The seat's gate is held across the watch's whole pass - its PR lookup and
/// its base refresh included - so a review page's read that queued behind one
/// would wait the watch's work rather than its own. Past this bound the read
/// is refused, and it re-reads the tree locally against the base it refreshed:
/// the store may predate that base.
pub const PINNED_GATE_WAIT: Duration = Duration::from_secs(3);

/// A shorter gate wait for a test, in milliseconds; `0` restores production's.
///
/// The bound is real time by nature, and a suite that spends three seconds on
/// it per case runs the gate on the clock rather than on what it pins.
#[cfg(any(test, feature = "testing"))]
pub fn set_pinned_gate_wait(ms: u64) {
    TEST_PINNED_GATE_WAIT.store(ms, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(any(test, feature = "testing"))]
static TEST_PINNED_GATE_WAIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The bound a drawing read uses: production's, or a test's shorter one.
fn pinned_gate_wait() -> Duration {
    #[cfg(any(test, feature = "testing"))]
    {
        let ms = TEST_PINNED_GATE_WAIT.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    PINNED_GATE_WAIT
}

/// How often a held seat looks at its tree.
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
    git: GitWorkView,
    pr: Option<GitPrInfo>,
    closes: Vec<GitIssueRef>,
}

impl Announced {
    fn of(held: &WorkSnapshot) -> Self {
        Self {
            work: work_from_scan(&held.diff, &held.cwd),
            git: git_work_view(&held.diff, held.base),
            pr: held.diff.pr.clone(),
            closes: held.diff.closes.clone(),
        }
    }
}

/// The seats a view is showing, and the loop each one runs.
#[derive(Default)]
pub(crate) struct HeldSeats {
    held: Mutex<HashMap<SessionSlot, Held>>,
    /// The gate a seat's reads share, by weak reference, so it outlives the
    /// hold exactly as long as a read still holds it. A re-hold during such a
    /// read upgrades this and shares the gate; once the read lets go the weak
    /// dies and the next hold starts a fresh one. Dead entries are pruned on
    /// the next look.
    gates: Mutex<HashMap<SessionSlot, std::sync::Weak<tokio::sync::Mutex<()>>>>,
}

struct Held {
    /// How many holders the seat has, so one view leaving does not stop a
    /// scan another still reads.
    count: usize,
    /// Dropped to stop the seat's loop, which also drops its watch.
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// One scan at a time for this seat: the loop's read, the record's read
    /// and any read a fixture drives are serialised, so a second one for the
    /// same tree is waste.
    scanning: Arc<tokio::sync::Mutex<()>>,
    /// Fired by the record's first read, which is what the page's snapshot
    /// carries. The watch's first pass starts from the stores that read left,
    /// so one read serves both halves and no value is sent twice.
    first_read: Arc<tokio::sync::Notify>,
}

/// A first hold's share of the seat: the mark the record's read fires, and
/// the sender that stops the loop.
struct Taken {
    first_read: Arc<tokio::sync::Notify>,
    stopped: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl HeldSeats {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionSlot, Held>> {
        self.held.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The gate for a seat's first hold: the one a read still holds, or a new
    /// one, remembered weakly for the next re-hold.
    fn gate_arc(&self, slot: &SessionSlot) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|_, weak| weak.strong_count() > 0);
        if let Some(gate) = gates.get(slot).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        gates.insert(slot.clone(), Arc::downgrade(&gate));
        gate
    }

    /// Take a hold on `slot`, answering the mark the record's read fires and
    /// the sender that stops the seat's loop when this is the first hold.
    fn acquire(&self, slot: &SessionSlot) -> Taken {
        let mut held = self.lock();
        // An entry lives only while a hold counts: the last release removes
        // it, so an existing one is a second viewer and owes no loop.
        if let Some(entry) = held.get_mut(slot) {
            entry.count += 1;
            return Taken { first_read: Arc::clone(&entry.first_read), stopped: None };
        }
        let scanning = self.gate_arc(slot);
        let first_read = Arc::new(tokio::sync::Notify::new());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        held.insert(
            slot.clone(),
            Held { count: 1, stop: Some(stop), scanning, first_read: Arc::clone(&first_read) },
        );
        Taken { first_read, stopped: Some(stopped) }
    }

    /// The seat's read gate, when one is there: a held seat's, or one a read
    /// still holds after its hold went. A seat neither held nor read has no
    /// loop to race, so the caller reads unguarded rather than waiting.
    fn gate(&self, slot: &SessionSlot) -> Option<Arc<tokio::sync::Mutex<()>>> {
        if let Some(held) = self.lock().get(slot) {
            return Some(Arc::clone(&held.scanning));
        }
        let mut gates = self.gates.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        gates.retain(|_, weak| weak.strong_count() > 0);
        gates.get(slot).and_then(std::sync::Weak::upgrade)
    }

    /// Say the record's first read is done, which is the mark the watch's
    /// first pass waits on. `notify_one` holds the permit for a watch that has
    /// not reached its wait yet, so the mark cannot be missed.
    fn note_record_read(&self, slot: &SessionSlot) {
        if let Some(held) = self.lock().get(slot) {
            held.first_read.notify_one();
        }
    }

    /// Give a hold back. The last hold stops the loop and drops the entry; a
    /// read still in flight keeps its gate alive through [`Self::gates`], so
    /// a re-hold before it finishes shares it rather than racing a second.
    fn release(&self, slot: &SessionSlot) {
        let mut held = self.lock();
        let Some(entry) = held.get_mut(slot) else {
            return;
        };
        entry.count = entry.count.saturating_sub(1);
        if entry.count == 0 {
            // The sender's drop is what stops the watch; taking it says so.
            drop(entry.stop.take());
            held.remove(slot);
        }
    }
}

impl Workspace {
    /// Show `slot`, which is what starts its working tree being watched.
    ///
    /// **A hold is what "somebody is looking" means**, and it is what keeps
    /// an idle seat from being scanned at all.
    ///
    /// **The hold does not wait for its reads.** The hold starts the seat's
    /// watch and answers; the record's own read follows it straight away (the
    /// subscribe path encodes from the stores), and the watch's first pass
    /// starts from what that read left. Those reads ran before the hold
    /// answered until #2506, so a warm open waited on every one of them.
    ///
    /// **The caller owes the read's mark.** [`Self::note_record_read`] is what
    /// releases the watch's first pass; a hold without it has a watch that
    /// waits and scans nothing.
    ///
    /// **A held seat waits for its session.** A seat with no session has no
    /// store for a read, so the watch's own loop goes quiet on it and wakes
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
    pub fn hold_seat(self: &Arc<Self>, slot: &SessionSlot) {
        // A hold is somebody looking: it ends the current failure episode,
        // so a later failure is a new one.
        if let Some(domain) = self.domain_session_for(slot) {
            domain.lock().auto_continue_spent = false;
        }
        let taken = self.held_work_seats.acquire(slot);
        let Some(stopped) = taken.stopped else {
            return;
        };
        spawn_work_watch(Arc::clone(self), slot.clone(), taken.first_read, stopped);
    }

    /// Mark the seat's record read: the snapshot has taken the stores, so the
    /// watch's first pass may start from them. The subscribe path calls this
    /// once its record is built; a seat nobody encodes has no watch reading.
    pub fn note_record_read(&self, slot: &SessionSlot) {
        self.held_work_seats.note_record_read(slot);
    }

    /// Wait at the fixture's gate when one is seeded. Test-only: production
    /// reads run as they always do.
    #[cfg(any(test, feature = "testing"))]
    async fn pass_test_scan_gate(&self) {
        let gate = self.test_scan_gate.lock().clone();
        if let Some(gate) = gate {
            // Held for the read's length: one permit is one read. A closed
            // gate means the fixture let it go, and the read runs.
            let _permit = gate.acquire().await;
        }
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
    /// **The held seat's own loop is the walker** - the socket's reads used
    /// to walk the seat a client read. This is that role moved to the loop,
    /// so a seat nobody holds is not walked at all and the walk costs what a
    /// held seat's tree costs rather than a whole record per read.
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
        #[cfg(any(test, feature = "testing"))]
        self.pass_test_scan_gate().await;
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
    ///
    /// **The record's read takes the same gate**, so a snapshot's first walk
    /// and the loop's first pass are one walk rather than two of the same
    /// tree at once.
    pub(crate) async fn walk_files_if_stale(&self, slot: &SessionSlot, dirty: bool) -> bool {
        let gate = self.held_work_seats.gate(slot);
        let _one_at_a_time = match &gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        self.walk_files_locked(slot, dirty).await
    }

    /// [`Self::walk_files_if_stale`] with the seat's gate already held.
    async fn walk_files_locked(&self, slot: &SessionSlot, dirty: bool) -> bool {
        let stale = reads_again(
            self.file_index(slot).map(|held| held.read_at),
            dirty,
            Instant::now(),
            INDEX_STALENESS,
        );
        if !stale {
            return false;
        }
        self.walk_files_now(slot).await.is_some()
    }

    /// The record's walk of `slot`'s tree: the store when there is one, else
    /// the walk the loop would take, under the same gate - so the snapshot's
    /// fallback and the first pass never walk at once. The pass walks again
    /// only when the tree moved.
    pub async fn walk_files_for_record(
        &self,
        slot: &SessionSlot,
    ) -> Arc<crate::file_index::FileIndex> {
        let gate = self.held_work_seats.gate(slot);
        let _one_at_a_time = match &gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        if let Some(held) = self.file_index(slot) {
            return held.index;
        }
        self.walk_files_now(slot).await.unwrap_or_default()
    }

    /// Walk `slot`'s tree now and store it, answering what it found. A seat
    /// with no session has nowhere to keep it, and the walk is answered
    /// anyway - a record still draws the tree it stands in.
    async fn walk_files_now(
        &self,
        slot: &SessionSlot,
    ) -> Option<Arc<crate::file_index::FileIndex>> {
        #[cfg(any(test, feature = "testing"))]
        self.pass_test_scan_gate().await;
        let cwd = self.cwd_for_session(slot)?;
        let respect = crate::file_index::respect_gitignore(self.user_preferences().as_ref());
        let root = PathBuf::from(cwd);
        let walked = root.clone();
        // The walk is a whole tree, so it runs on the blocking pool rather
        // than on the reactor.
        match tokio::task::spawn_blocking(move || {
            crate::file_index::FileIndex::scan(&root, respect)
        })
        .await
        {
            Ok(index) => {
                let index = Arc::new(index);
                self.store_file_index(slot, Arc::clone(&index));
                Some(index)
            }
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::work",
                    event_name = "file_index_walk_failed",
                    %error,
                    slot = %slot.display(),
                    path = %walked.display(),
                    "the file-index walk did not finish; the seat keeps the index it had",
                );
                None
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

    /// The seat's work gate, for a test that has to hold it across a read.
    /// `None` when the seat is not held.
    #[cfg(any(test, feature = "testing"))]
    pub fn hold_work_gate(&self, slot: &SessionSlot) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.held_work_seats.gate(slot)
    }

    /// Store a scan for `slot`, as the seat's loop and a fixture both do.
    pub fn store_work_snapshot(&self, slot: &SessionSlot, snapshot: WorkSnapshot) {
        if let Some(domain) = self.domain_session_for(slot) {
            domain.lock().work_snapshot = Some(snapshot);
        }
    }

    /// Answer the seat's scan, reading the tree first when the rule says to.
    ///
    /// **The rule is checked under the seat's own gate**, so the record's read,
    /// the loop's read and a fixture's read cannot both decide a stale tree
    /// needs two scans: the second one finds what the first stored.
    ///
    /// # Errors
    ///
    /// A seat with no session behind it has no directory to read, and forge
    /// answers that rather than scanning the process's own directory - the
    /// binary test every read here owes.
    pub async fn scan_work_if_stale(
        &self,
        slot: &SessionSlot,
        dirty: bool,
        pr: PrRefresh,
    ) -> Result<WorkSnapshot, String> {
        let gate = self.held_work_seats.gate(slot);
        let _one_at_a_time = match &gate {
            // **A read that must not wait does not queue behind a scan.**
            // The watch's Fetch scan holds this gate across its PR lookup,
            // so a second open (a phone beside the desktop, a re-subscribe)
            // would otherwise wait a network round trip it never asked for.
            // The store it already holds is what this read would carry
            // anyway.
            Some(gate) if pr == PrRefresh::ServeCached => {
                if let Ok(held) = gate.try_lock() {
                    Some(held)
                } else if let Some(held) = self.work_snapshot(slot) {
                    return Ok(held);
                } else {
                    // Nothing stored to serve: this is the seat's first read,
                    // and the scan in flight is the one that will store it.
                    Some(gate.lock().await)
                }
            }
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        self.scan_work_locked(slot, dirty, pr).await
    }

    /// [`Self::scan_work_if_stale`] with the seat's gate already held.
    async fn scan_work_locked(
        &self,
        slot: &SessionSlot,
        dirty: bool,
        pr: PrRefresh,
    ) -> Result<WorkSnapshot, String> {
        // A seat with no session has nowhere to store a scan, so the rule
        // would read "nothing has scanned this" on every poke. The loop's own
        // guard skips the call while sessionless, and this refusal is the
        // second layer behind it: a session can END under a held seat between
        // that check and this read, and then it must refuse here rather than
        // scan into nothing.
        if self.domain_session_for(slot).is_none() {
            return Err(format!("{} has no session to store a scan on", slot.display()));
        }
        let held = self.work_snapshot(slot);
        // **A scan that may look the PR up also runs when the PR is due.**
        // The read path serves the stored PR and never waits, so
        // the loop is what refreshes it; without this leg the correction
        // would wait for the tree's own 10 s window instead of its own.
        let pr_due = pr == PrRefresh::Fetch
            && held.as_ref().is_some_and(|held| {
                forge_agent::env::git_diff::pr_refresh_due(&held.diff, std::time::SystemTime::now())
            });
        if should_scan(held.as_ref(), dirty, Instant::now()) || pr_due {
            #[cfg(any(test, feature = "testing"))]
            self.pass_test_scan_gate().await;
            let Some(cwd) = self.cwd_for_session(slot) else {
                return Err(format!("{} has no directory to read", slot.display()));
            };
            // **The watch's pass is where the base fetch is paid.** Nobody
            // waits on it, so the row it pushes is computed from a ref this
            // refreshed; a record read never comes here with `Fetch` except a
            // sessionless seat's fallback, which has no store, no loop and
            // nothing to keep fresh.
            let mut measured = None;
            if pr == PrRefresh::Fetch
                && let Some(default) =
                    held.as_ref().and_then(|held| held.diff.default_branch.as_deref())
                && held.as_ref().is_some_and(|held| held.diff.branch_ahead.is_populated())
            {
                measured = forge_agent::env::git_diff::watch_refresh_base(Path::new(&cwd), default)
                    .await
                    .map(|refresh| refresh.state);
            }
            return Ok(self
                .scan_work_at_with_base(slot, Path::new(&cwd), pr, None, measured, false)
                .await);
        }
        self.work_snapshot(slot).ok_or_else(|| format!("{} has no stored scan", slot.display()))
    }

    /// Read `cwd` as `slot`'s working tree, store what it answered, and
    /// answer it.
    ///
    /// One body for both entry points - the record's fallback and the loop -
    /// because a second scan path is a second answer about one tree.
    pub async fn scan_work_at(
        &self,
        slot: &SessionSlot,
        cwd: &Path,
        pr: PrRefresh,
    ) -> WorkSnapshot {
        self.scan_work_at_with_base(slot, cwd, pr, None, None, false).await
    }

    /// A sessionless seat's fetching scan with **no background kick**: the
    /// read that will fetch the base itself a moment later takes this, so its
    /// own fetch is the only one and nothing of forge's can be queued ahead of
    /// it on the shared per-repository gate.
    pub async fn scan_work_at_without_base_fetch(
        &self,
        slot: &SessionSlot,
        cwd: &Path,
    ) -> WorkSnapshot {
        self.scan_work_at_with_base(slot, cwd, PrRefresh::Fetch, None, None, true).await
    }

    /// A local scan pinned to `base`, taken **without the seat's gate**: the
    /// drawing read's fallback when the gate is held past its bound. It writes
    /// the store like any scan, and the caller has already refreshed the base.
    pub async fn scan_work_at_pinned_local(
        &self,
        slot: &SessionSlot,
        cwd: &Path,
        base: Option<&str>,
        measured: Option<BaseState>,
    ) -> WorkSnapshot {
        self.scan_work_at_with_base(slot, cwd, PrRefresh::ServeCached, base, measured, false).await
    }

    /// [`Self::scan_work_at`] pinned to one base revision: the branch-ahead
    /// layer is computed against `base` (a sha a drawing read resolved)
    /// rather than the ref's name.
    ///
    /// `measured` is what a watch pass just refreshed the base to; `None`
    /// keeps the state the store last measured, so a scan that did not
    /// refresh does not erase it. `quiet` suppresses the background kick on a
    /// `Fetch` scan.
    async fn scan_work_at_with_base(
        &self,
        slot: &SessionSlot,
        cwd: &Path,
        pr: PrRefresh,
        base: Option<&str>,
        measured: Option<BaseState>,
        quiet: bool,
    ) -> WorkSnapshot {
        let held = self.work_snapshot(slot);
        let previous = held.as_ref().map(|held| &held.diff);
        let env = self.project_env(slot.project());
        let diff = match (pr, base) {
            (_, Some(base)) => {
                forge_agent::env::git_diff::scan_pinned(cwd, &env, previous, base).await
            }
            (PrRefresh::Fetch, None) if quiet => {
                forge_agent::env::git_diff::scan_without_base_fetch(cwd, &env, previous).await
            }
            (PrRefresh::Fetch, None) => forge_agent::env::git_diff::scan(cwd, &env, previous).await,
            (PrRefresh::ServeCached, None) => {
                forge_agent::env::git_diff::scan_serving_cached_pr(cwd, &env, previous).await
            }
        };
        let held = WorkSnapshot {
            diff,
            cwd: cwd.to_path_buf(),
            read_at: Instant::now(),
            base: measured.or_else(|| held.and_then(|held| held.base)),
        };
        self.store_work_snapshot(slot, held.clone());
        held
    }

    /// A scan a drawing read must take, pinned to one base revision: it
    /// **awaits the seat's gate** rather than serving the store a concurrent
    /// scan is replacing, so the count and chain it answers were read after
    /// the base moved. Local only - the caller has already refreshed the base.
    pub async fn scan_work_pinned(
        &self,
        slot: &SessionSlot,
        cwd: &Path,
        base: Option<&str>,
        measured: Option<BaseState>,
    ) -> Result<WorkSnapshot, String> {
        if self.domain_session_for(slot).is_none() {
            return Err(format!("{} has no session to store a scan on", slot.display()));
        }
        let gate = self.held_work_seats.gate(slot);
        let wait = pinned_gate_wait();
        let _one_at_a_time = match &gate {
            Some(gate) => {
                // **A drawing read waits for a scan, but not for ever.** The
                // gate is held across the watch's whole scan - its PR lookup
                // and its base refresh included - and a reader that queued
                // behind that would spend the page's open on a pass it never
                // asked for. Past the bound the read is refused and re-reads
                // locally against the base it refreshed.
                let Ok(held) = tokio::time::timeout(wait, gate.lock()).await else {
                    // DEBUG: the fallback is the design working, not a fault -
                    // the read answers a correct scan either way, one bound
                    // later, and a slow watch pass is the environment's.
                    tracing::debug!(
                        target: "forge_workspace::work",
                        event_name = "work_scan_gate_overrun",
                        slot = %slot.display(),
                        wait_ms = wait.as_secs_f64() * 1000.0,
                        "a drawing read waited the whole bound for the seat's work gate",
                    );
                    return Err(format!(
                        "{}'s work gate was held past {}ms",
                        slot.display(),
                        wait.as_millis(),
                    ));
                };
                Some(held)
            }
            None => None,
        };
        Ok(self
            .scan_work_at_with_base(slot, cwd, PrRefresh::ServeCached, base, measured, false)
            .await)
    }

    /// `project`'s own declared environment, `env_file` included, which
    /// is the identity a `gh` spawn on its behalf carries: the scan's PR
    /// row must read as the project, not as the server's ambient login.
    /// A name no project has declares none, so `gh` falls back to its
    /// own login.
    fn project_env(&self, project: &str) -> std::collections::HashMap<String, String> {
        self.config_arc()
            .projects
            .iter()
            .find(|candidate| candidate.name == project)
            .map(|candidate| candidate.env.clone())
            .unwrap_or_default()
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

/// Whether the seat's watch was stopped while it waited for the record's mark:
/// the holder went (the sender is dropped) or the stop fired. Either way the
/// reads it was about to take are owed to nobody.
fn stopped_while_waiting(stopped: &mut tokio::sync::oneshot::Receiver<()>) -> bool {
    matches!(stopped.try_recv(), Ok(()) | Err(tokio::sync::oneshot::error::TryRecvError::Closed))
}

/// The commands a walk hands the OS scan: each running `local_bash` task's own
/// command.
///
/// A task whose card forge has not seen carries no command and is skipped:
/// a rostered bash with no recorded command draws its registry row instead
/// of being adopted by the walk.
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
///
/// **The record's read is the first pass.** The subscribe path holds the seat
/// and then encodes its snapshot from the stores, so that read is what the
/// page's first frame carries; this task waits for its mark, seeds from the
/// stores it left, and announces a value the page does not already hold. The
/// gate keeps the two reads from running together; a pass the tree moved under
/// still reads and announces.
///
/// **The work scan comes first in every pass.** It is the cheapest of the
/// three reads and the row a reader watches, so the process walk and the
/// whole-tree index walk queue behind it rather than delay it.
fn spawn_work_watch(
    workspace: Arc<Workspace>,
    slot: SessionSlot,
    first_read: Arc<tokio::sync::Notify>,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
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
        tokio::select! {
            _ = &mut stopped => return,
            () = first_read.notified() => {}
        }
        // A page that opened and closed again leaves nothing to fill in, so
        // the expensive reads are not taken for it.
        if stopped_while_waiting(&mut stopped) {
            return;
        }
        // The stores are what the snapshot carried, so they seat each
        // baseline: the first pass announces only a value that differs.
        let mut announced = workspace.work_snapshot(&slot).map(|held| Announced::of(&held));
        let mut walked = workspace.process_snapshot(&slot).map(|held| held.processes);
        let mut indexed = workspace.file_index(&slot).map(|held| held.index);
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
            // Whatever the watch reported since the last look is one mark:
            // the tree moved, which is all a scan decision needs.
            let mut dirty = false;
            while changes.try_recv().is_ok() {
                dirty = true;
            }
            // The tree's own row first: the cheapest read, and the one a
            // reader waits for. A refusal leaves the process and file walks
            // to run their own rules.
            // **The loop is where the PR lookup is paid.** Nobody
            // waits on this pass; the snapshot a client's open reads serves
            // the stored PR instead, so the round trip lands here.
            if let Ok(held) = workspace.scan_work_if_stale(&slot, dirty, PrRefresh::Fetch).await {
                let row = Announced::of(&held);
                if announced.as_ref() != Some(&row) {
                    workspace.update_tx.send(SessionUpdate::WorkChanged {
                        key: slot.clone(),
                        work: row.work.clone(),
                        git: row.git.clone(),
                        pr: row.pr.clone(),
                        closes: row.closes.clone(),
                    });
                    announced = Some(row);
                }
            } else {
                tracing::debug!(
                    target: "forge_workspace::work",
                    event_name = "work_watch_scan_refused",
                    slot = %slot.display(),
                    "the seat's tree was not read this poke",
                );
            }
            // The walk, on its own rule: missing or older than
            // `SCAN_STALENESS`, which is this poke's interval.
            workspace.walk_processes_if_stale(&slot, workspace.claude_pid(&slot)).await;
            // **The compare runs whoever walked.** The store can be written by
            // a walk other than this loop's own, so it is read on every poke
            // rather than only after this loop walked.
            if let Some(walk) = workspace.process_snapshot(&slot)
                && walk_moved(walked.as_ref(), &walk)
            {
                walked = Some(walk.processes.clone());
                workspace
                    .update_tx
                    .send(SessionUpdate::ProcessesChanged { key: slot.clone(), snapshot: walk });
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
    use crate::SeatUpdate;

    fn seat() -> SessionSlot {
        SessionSlot::lead("TestOrg", "forge")
    }

    /// Hold the seat and mark the record read, the way a subscribe does: the
    /// page's snapshot is the first read and the watch's first pass starts
    /// from the stores it left.
    fn hold_for_a_page(workspace: &Arc<Workspace>, seat: &SessionSlot) {
        workspace.hold_seat(seat);
        workspace.note_record_read(seat);
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
    ) -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<SeatUpdate>, tempfile::TempDir) {
        a_declared_project_with_env(repo, &[])
    }

    /// [`a_declared_project`] with the project's own `[orgs.projects.env]`,
    /// which is the identity a scan's `git` and `gh` children carry.
    fn a_declared_project_with_env(
        repo: &Path,
        env: &[(&str, &str)],
    ) -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<SeatUpdate>, tempfile::TempDir) {
        use std::fmt::Write as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge dir");
        let mut project_env = String::new();
        if !env.is_empty() {
            project_env.push_str("\n  [orgs.projects.env]\n");
            for (key, value) in env {
                writeln!(project_env, "  {key} = {value:?}").expect("the env block writes");
            }
        }
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
{project_env}
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
        let updates = workspace.subscribe_answerer();
        (workspace, updates, dir)
    }

    /// [`a_declared_project`] with the seat started, which is what a hold
    /// needs: the store rides the seat's own record.
    fn a_workspace(
        repo: &Path,
    ) -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<SeatUpdate>, tempfile::TempDir) {
        let (workspace, updates, dir) = a_declared_project(repo);
        workspace.register_domain_session(seat(), None);
        (workspace, updates, dir)
    }

    /// [`a_workspace`] with the project's own env, which is where a scan's
    /// `gh` children get their identity.
    fn a_workspace_with_env(
        repo: &Path,
        env: &[(&str, &str)],
    ) -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<SeatUpdate>, tempfile::TempDir) {
        let (workspace, updates, dir) = a_declared_project_with_env(repo, env);
        workspace.register_domain_session(seat(), None);
        (workspace, updates, dir)
    }

    /// A repo on a pushed `feature` branch, and a `gh` on a PATH of its own
    /// that answers the scan's PR lookup with one open PR and records that it
    /// ran. The push is simulated the way the scanner sees one: a
    /// remote-tracking ref that already contains the tip.
    struct PushedBranch {
        repo: tempfile::TempDir,
        tip: String,
        /// The shim directory, as the project's `PATH`.
        path: String,
        marker: std::path::PathBuf,
    }

    /// One open PR, as GitHub's `commits/<sha>/pulls` answers it.
    const PR_JSON: &str = r#"[{"number":108,"state":"open","html_url":"https://example.test/pull/108","updated_at":"2026-10-24T00:00:00Z","draft":false}]"#;

    impl PushedBranch {
        /// The fixture and the shim's directory, which the caller has to keep
        /// alive for as long as `gh` may run.
        fn new() -> (Self, tempfile::TempDir) {
            use std::os::unix::fs::PermissionsExt;

            let repo = a_repo();
            git(repo.path(), &["checkout", "-q", "-b", "feature"]);
            let tip = rev_parse(repo.path(), "HEAD");
            git(repo.path(), &["update-ref", "refs/remotes/origin/main", &tip]);
            git(
                repo.path(),
                &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"],
            );

            let shim = tempfile::tempdir().expect("the gh shim's own directory");
            let marker = shim.path().join("gh-spawned");
            let gh = shim.path().join("gh");
            std::fs::write(
                &gh,
                format!(
                    "#!/bin/sh\nprintf x > {marker}\ncase \"$*\" in\n  *commits/*pulls) \
                     printf '%s' '{pr}' ;;\n  *\"pr view\"*) printf '%s' \
                     '{{\"closingIssuesReferences\":[]}}' ;;\n  *) printf '[]' ;;\nesac\n",
                    marker = marker.display(),
                    pr = PR_JSON,
                ),
            )
            .expect("write the shim");
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            let path = shim.path().to_string_lossy().into_owned();
            (Self { repo, tip, path, marker }, shim)
        }

        /// The project's env, which is the identity the scan's `gh` runs as.
        fn env(&self) -> [(&str, &str); 2] {
            [("PATH", self.path.as_str()), ("GH_TOKEN", "t")]
        }

        /// A stored scan of the branch, past the read window, carrying `pr`
        /// with its attempt taken at `fetched_at`.
        fn stored_scan(
            &self,
            pr: Option<forge_primitives::git::GitPrInfo>,
            fetched_at: std::time::SystemTime,
        ) -> WorkSnapshot {
            WorkSnapshot {
                read_at: Instant::now()
                    .checked_sub(SNAPSHOT_STALENESS + Duration::from_secs(1))
                    .expect("the test clock is past the read window"),
                diff: GitDiffSnapshot {
                    branch: GitBranch::Named("feature".to_owned()),
                    default_branch: Some("origin/main".to_owned()),
                    repo_gate: RepoGate::InRepo,
                    pushed_sha: Some(self.tip.clone()),
                    worktree: LayerState::Clean,
                    branch_ahead: LayerState::Clean,
                    pr,
                    closes: Vec::new(),
                    pr_fetched_at: Some(fetched_at),
                },
                cwd: self.repo.path().to_path_buf(),
                base: None,
            }
        }
    }

    /// The sha `rev` names in `dir`.
    fn rev_parse(dir: &Path, rev: &str) -> String {
        let out = Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", rev])
            .output()
            .expect("run git");
        assert!(out.status.success(), "git rev-parse {rev}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
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
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        // The commit below leaves git's auto-maintenance behind (`git
        // maintenance run --auto --detach`, gc --auto's modern form), which
        // writes and removes `.git/objects/maintenance.lock` - the measured
        // cause of the still-tree announce (#1818). A tree that has to be
        // still runs no maintenance.
        git(dir.path(), &["config", "maintenance.auto", "false"]);
        git(dir.path(), &["config", "gc.auto", "0"]);
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
            base: None,
        }
    }

    /// The rule the record's read and the loop both read: nothing read yet, a
    /// tree that moved, or a read that has gone stale.
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

    /// **The record's read never waits on the PR lookup.** The lookup
    /// is the scan's one piece of network - a TLS + API round trip measured at
    /// 0.9 s - and it sat on the record a client's open waits for. The record
    /// serves the PR its store holds and leaves the refresh to the seat's
    /// loop, which nobody waits on. The control below proves the fixture can
    /// see a spawn, so "no marker" is a policy and not a broken shim.
    #[tokio::test(flavor = "current_thread")]
    async fn the_records_scan_serves_the_stored_pr_without_spawning_gh() {
        use forge_primitives::git::GitPrInfo;

        let (branch, _shim) = PushedBranch::new();
        let (workspace, _updates, _config) =
            a_workspace_with_env(branch.repo.path(), &branch.env());

        // The store's scan is past the read window, so the record re-reads the
        // tree, and its PR attempt is past any refetch window - both reasons
        // the lookup would run today.
        let pr = GitPrInfo { number: 7, url: "https://example.test/pr/7".to_owned(), draft: false };
        let fetched_at = std::time::SystemTime::now() - Duration::from_secs(600);
        workspace.store_work_snapshot(&seat(), branch.stored_scan(Some(pr.clone()), fetched_at));

        let served = workspace
            .scan_work_if_stale(&seat(), false, PrRefresh::ServeCached)
            .await
            .expect("a live seat reads its tree");
        assert!(
            !branch.marker.exists(),
            "the record's read answers without spawning the PR lookup",
        );
        assert_eq!(served.diff.pr, Some(pr), "and it serves the PR the store already held");
        assert_eq!(
            served.diff.pr_fetched_at,
            Some(fetched_at),
            "the stored attempt time stands, so the loop still finds the cache stale",
        );

        let fetched = workspace
            .scan_work_if_stale(&seat(), true, PrRefresh::Fetch)
            .await
            .expect("a live seat reads its tree");
        assert!(branch.marker.exists(), "the loop's policy does reach the lookup");
        assert_eq!(
            fetched.diff.pr.map(|found| found.number),
            Some(108),
            "and the shim's answer is what the scan then holds",
        );
    }

    /// **The deferred lookup still reaches the viewer.** The record
    /// serves the PR its store holds and never waits; the seat's watch is
    /// where the lookup runs, and the row it moves is pushed - so the
    /// correction a reader sees is a frame, not a second read.
    #[tokio::test(flavor = "current_thread")]
    async fn the_watch_pushes_the_pr_a_record_served_from_its_store() {
        use forge_primitives::git::GitPrInfo;

        let (branch, _shim) = PushedBranch::new();
        let (workspace, mut updates, _config) =
            a_workspace_with_env(branch.repo.path(), &branch.env());
        let seat = seat();

        // The store's row held a PR, and the attempt that found it is long
        // past the refetch window: the lookup is due.
        let held =
            GitPrInfo { number: 7, url: "https://example.test/pr/7".to_owned(), draft: false };
        workspace.store_work_snapshot(
            &seat,
            branch.stored_scan(
                Some(held.clone()),
                std::time::SystemTime::now() - Duration::from_secs(600),
            ),
        );

        // The subscribe, as the transport runs it: hold, read the record (which
        // serves the stored PR and starts no lookup), then mark the read so the
        // watch's first pass begins from these stores.
        workspace.hold_seat(&seat);
        let served = workspace
            .scan_work_if_stale(&seat, false, PrRefresh::ServeCached)
            .await
            .expect("a live seat reads its tree");
        assert_eq!(served.diff.pr, Some(held), "the record serves what the store held");
        assert!(!branch.marker.exists(), "and the read ran no lookup to answer");
        workspace.note_record_read(&seat);

        // **The correction is the watch's next pass, not the tree's own
        // window.** The record's read left the store fresh, so a loop that
        // only scanned on tree staleness would take a whole `SNAPSHOT_STALENESS`
        // to push; the bound is derived from that window so the two cannot
        // drift apart, and the PR-due leg is what keeps this inside a poke.
        let deadline =
            tokio::time::Instant::now() + SNAPSHOT_STALENESS.saturating_sub(Duration::from_secs(2));
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("the watch did not push the correction its lookup made within a poke");
            };
            let SessionUpdate::WorkChanged { key, pr, .. } = moved.update else {
                continue;
            };
            assert_eq!(key, seat, "and the row is addressed to the seat that was held");
            assert_eq!(
                pr.map(|found| found.number),
                Some(108),
                "the pushed row carries the PR the deferred lookup found",
            );
            return;
        }
    }

    /// **A hold answers before its scans do** (#2506). The open used to pay
    /// three sequential stale reads before it could answer; the watch the hold
    /// starts takes them, so a slow read cannot hold a page.
    #[tokio::test]
    async fn a_hold_is_served_before_its_scans_complete() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        workspace.seed_test_scan_gate(Arc::clone(&gate));

        // The hold returns with the watch spawned; the reads are at the gate.
        workspace.hold_seat(&seat);

        assert!(workspace.work_snapshot(&seat).is_none(), "the tree's scan has not answered");
        assert!(workspace.process_snapshot(&seat).is_none(), "nor has the process walk");
        assert!(workspace.file_index(&seat).is_none(), "nor has the file index");

        gate.add_permits(8);
    }

    /// **The record's read and the first pass are one scan** (#2506 review).
    /// Both take the seat's gate, so a snapshot's fallback read and the loop's
    /// first pass cannot scan the same tree together: whichever gets there
    /// first stores, and the other answers that store.
    #[tokio::test]
    async fn one_scan_serves_the_records_read_and_the_first_pass() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat);
        // The scan itself is held, so both reads are inside the seat's gate
        // when it opens.
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        workspace.seed_test_scan_gate(Arc::clone(&gate));

        let read = |workspace: Arc<Workspace>, seat: SessionSlot| async move {
            workspace.scan_work_if_stale(&seat, false, PrRefresh::Fetch).await
        };
        let record = tokio::spawn(read(Arc::clone(&workspace), seat.clone()));
        let first_pass = tokio::spawn(read(Arc::clone(&workspace), seat.clone()));
        tokio::task::yield_now().await;
        gate.add_permits(8);

        let record = record.await.expect("the record's read").expect("a live seat scans");
        let first_pass =
            first_pass.await.expect("the first pass's read").expect("a live seat scans");
        assert_eq!(
            record.read_at, first_pass.read_at,
            "one scan served both: the second answered the store the first wrote",
        );
    }

    /// **A record read does not queue behind a scan somebody else is running.**
    /// The watch's Fetch scan holds the seat's gate across its PR lookup, so a
    /// second open (a phone beside the desktop, a re-subscribe) must answer the
    /// store it already holds rather than wait a network round trip it never
    /// asked for. The control below is the same read under the fetching policy,
    /// which is the one that waits - so the fast answer is the policy and not a
    /// gate nothing takes.
    #[tokio::test]
    async fn a_record_read_serves_the_store_while_a_scan_holds_the_gate() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat);
        let stored = a_snapshot(Instant::now(), dir.path());
        workspace.store_work_snapshot(&seat, stored.clone());

        // The scan in flight: the seat's own gate, held for as long as this
        // test keeps the guard.
        let gate = workspace.held_work_seats.gate(&seat).expect("a held seat has a gate");
        let _in_flight = gate.lock().await;

        let served = tokio::time::timeout(
            Duration::from_millis(500),
            workspace.scan_work_if_stale(&seat, false, PrRefresh::ServeCached),
        )
        .await
        .expect("a read that must not wait answers at once")
        .expect("the store is there");
        assert_eq!(served.read_at, stored.read_at, "and it answers the store's own scan");

        assert!(
            tokio::time::timeout(
                Duration::from_millis(500),
                workspace.scan_work_if_stale(&seat, true, PrRefresh::Fetch),
            )
            .await
            .is_err(),
            "a fetching scan is the one that queues on the gate in flight",
        );
    }

    /// **A drawing read waits out a scan in flight and then reads the tree.**
    /// The gate is the one the watch's scan holds, so a review read that took
    /// the store instead would draw a scan written before the base it just
    /// refreshed moved. The control is the store: it says "not a repo", which
    /// is what a read that did not wait would serve.
    #[tokio::test]
    async fn a_pinned_scan_waits_out_a_scan_in_flight_and_answers_a_fresh_one() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat);
        workspace.store_work_snapshot(&seat, a_snapshot(Instant::now(), dir.path()));
        let gate = workspace.held_work_seats.gate(&seat).expect("a held seat has a gate");
        let in_flight = gate.lock().await;

        let scanning = Arc::clone(&workspace);
        let at = dir.path().to_path_buf();
        let seat_for_task = seat.clone();
        let waiting = tokio::spawn(async move {
            scanning.scan_work_pinned(&seat_for_task, &at, None, None).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished(), "the drawing read queues on the scan in flight");

        drop(in_flight);
        let fresh =
            waiting.await.expect("the task joins").expect("the gate frees and the scan runs");
        assert_eq!(
            fresh.diff.repo_gate,
            RepoGate::InRepo,
            "the answer is the scan taken after the gate, not the stored non-repo",
        );
    }

    /// **The wait is bounded.** A gate held past `PINNED_GATE_WAIT` - a watch
    /// pass stuck on its own network work - answers the read with a refusal,
    /// so the page serves its store and labels the base unverified rather
    /// than spending the whole open on a scan it never asked for.
    #[tokio::test]
    async fn a_pinned_scan_gives_up_on_a_gate_held_past_its_bound() {
        // The bound is real time; the test's is short, and restored after.
        set_pinned_gate_wait(100);
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat);
        let gate = workspace.held_work_seats.gate(&seat).expect("a held seat has a gate");
        let in_flight = gate.lock().await;

        let started = Instant::now();
        let refused = workspace.scan_work_pinned(&seat, dir.path(), None, None).await;
        assert!(refused.is_err(), "a held gate past the bound refuses the read");
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "and the read waits its bound first, not a moment less",
        );

        drop(in_flight);
        assert!(
            workspace.scan_work_pinned(&seat, dir.path(), None, None).await.is_ok(),
            "a free gate scans at once",
        );
        set_pinned_gate_wait(0);
    }

    /// **The watch's measurement rides the row.** A base can move without the
    /// count or the chain moving, so the state the watch refreshed is carried
    /// on the snapshot - and a scan that did not refresh keeps the last one
    /// rather than erasing it.
    #[tokio::test]
    async fn the_watchs_measured_base_rides_the_row() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.store_work_snapshot(&seat, a_snapshot(Instant::now(), dir.path()));

        let measured = workspace
            .scan_work_at_with_base(
                &seat,
                dir.path(),
                PrRefresh::ServeCached,
                None,
                Some(BaseState::Behind { behind: 2 }),
                false,
            )
            .await;
        assert_eq!(measured.base, Some(BaseState::Behind { behind: 2 }), "the measurement lands");

        let inherited = workspace.scan_work_at(&seat, dir.path(), PrRefresh::ServeCached).await;
        assert_eq!(
            inherited.base,
            Some(BaseState::Behind { behind: 2 }),
            "and a later local scan keeps it instead of erasing it",
        );
        assert_eq!(
            git_work_view(&inherited.diff, inherited.base).base,
            Some(BaseState::Behind { behind: 2 }),
            "so the row a view draws says what the base is",
        );
    }

    /// **Only a fetching scan is forced by a due PR.** The read path's policy
    /// is to serve; a PR-due leg that ignored the policy would turn every
    /// record read on a due branch into a tree scan, which is the cost the
    /// serving policy exists to keep off the open.
    #[tokio::test]
    async fn a_due_pr_does_not_force_a_serving_read_to_scan() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        let read_at = Instant::now();
        let mut stored = a_snapshot(read_at, dir.path());
        // A feature branch past its refetch window: the PR is due.
        stored.diff.branch = GitBranch::Named("feature".to_owned());
        stored.diff.default_branch = Some("origin/main".to_owned());
        stored.diff.pr_fetched_at = Some(
            std::time::SystemTime::now()
                .checked_sub(Duration::from_secs(600))
                .expect("a wall clock at least ten minutes past boot"),
        );
        workspace.store_work_snapshot(&seat, stored.clone());

        let served = workspace
            .scan_work_if_stale(&seat, false, PrRefresh::ServeCached)
            .await
            .expect("a live seat reads its tree");
        assert_eq!(
            served.read_at, stored.read_at,
            "a serving read answers the store even when the PR is due",
        );

        let fetched = workspace
            .scan_work_if_stale(&seat, false, PrRefresh::Fetch)
            .await
            .expect("a live seat reads its tree");
        assert_ne!(
            fetched.read_at, stored.read_at,
            "a fetching scan is the one the due leg forces",
        );
    }

    /// **The record's index and the first pass are one walk** (#2506 review),
    /// on the same gate: the snapshot's fallback walk and the loop's first
    /// pass answer the same index rather than walking the tree twice.
    #[tokio::test]
    async fn one_walk_serves_the_records_index_and_the_first_pass() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        workspace.seed_test_scan_gate(Arc::clone(&gate));

        let record = tokio::spawn({
            let workspace = Arc::clone(&workspace);
            let seat = seat.clone();
            async move { workspace.walk_files_for_record(&seat).await }
        });
        let first_pass = tokio::spawn({
            let workspace = Arc::clone(&workspace);
            let seat = seat.clone();
            async move { workspace.walk_files_for_record(&seat).await }
        });
        tokio::task::yield_now().await;
        gate.add_permits(8);

        let record = record.await.expect("the record's walk");
        let first_pass = first_pass.await.expect("the first pass's walk");
        assert!(
            Arc::ptr_eq(&record, &first_pass),
            "one walk served both: the second answered the index the first stored",
        );
    }

    /// **The reads the hold gave up still run and their answers reach the
    /// seat's viewers** (rule 25): the first pass of the watch reads the tree,
    /// walks the processes and the files, and announces each of them.
    #[tokio::test]
    async fn the_first_pass_announces_the_tree_the_processes_and_the_files() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        // The process walk needs a seat pid; this test's own tree is one that
        // exists and needs no CLI behind it.
        workspace.seed_test_claude_pid(&seat, std::process::id());
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        workspace.seed_test_scan_gate(Arc::clone(&gate));

        // The hold returns with the watch spawned; the reads are at the gate.
        let started = tokio::time::Instant::now();
        hold_for_a_page(&workspace, &seat);
        assert!(
            tokio::time::timeout(Duration::from_millis(400), updates.recv()).await.is_err(),
            "nothing is announced while the scans are still held",
        );
        // **The stores say the reads are at the gate, not merely slow**: with
        // the seam removed, both would have landed inside the window above.
        assert!(workspace.work_snapshot(&seat).is_none(), "the tree's scan is held at the gate");
        assert!(workspace.file_index(&seat).is_none(), "and the walk with it");
        gate.add_permits(8);

        let (mut work, mut processes, mut files) = (false, false, false);
        let mut order = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !(work && processes && files) {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!(
                    "the first pass's answers never all arrived: tree {work}, processes {processes}, files {files}"
                );
            };
            match moved.update {
                SessionUpdate::WorkChanged { key, .. } => {
                    assert_eq!(key, seat, "the tree's row is addressed to the held seat");
                    work = true;
                    order.push("work");
                }
                SessionUpdate::ProcessesChanged { key, .. } => {
                    assert_eq!(key, seat, "the walk is addressed to the held seat");
                    processes = true;
                    order.push("processes");
                }
                SessionUpdate::FileIndexChanged { key, .. } => {
                    assert_eq!(key, seat, "the index is addressed to the held seat");
                    files = true;
                    order.push("files");
                }
                _ => {}
            }
        }
        assert_eq!(
            order.first(),
            Some(&"work"),
            "the row a reader watches is announced before the walks behind it",
        );
        // **The pass runs on the record's mark, not after a poke.** A pass
        // that waited the loop's interval would land past this bound.
        assert!(
            started.elapsed() < Duration::from_millis(950),
            "the first pass ran within one poke: {:?}",
            started.elapsed(),
        );
    }

    /// **The hold's scan answers as an update, and every row the tree moves to
    /// after it does too.** The open no longer waits for the read (#2506), so
    /// the first pass is what tells the page what the tree holds.
    #[tokio::test]
    async fn a_held_seat_announces_its_row_and_every_row_it_moves_to() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();

        hold_for_a_page(&workspace, &seat);

        let row = announced_work(&mut updates).await;
        assert_eq!(row.gate, Gate::InRepo, "a checkout is read as a repository");
        assert_eq!(row.changed, Some(0), "and a clean one as nothing changed");

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
            let SessionUpdate::WorkChanged { key, work, .. } = moved.update else {
                continue;
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

        hold_for_a_page(&workspace, &seat);
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
            let SessionUpdate::WorkChanged { key, work, .. } = moved.update else {
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
        hold_for_a_page(&workspace, &seat);

        // A TRACKED file: the row counts the worktree layer, which is
        // uncommitted edits against HEAD, and an untracked file is not one.
        std::fs::write(dir.path().join("kept.txt"), "two").expect("write");
        let held = workspace
            .scan_work_if_stale(&seat, true, PrRefresh::Fetch)
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
                is_backgrounded: true,
                task_type: "local_bash".to_owned(),
                description: "gh run watch".to_owned(),
                command: Some("gh run watch 123 --exit-status".to_owned()),
                tool_use_id: None,
            },
            // The agent task CARRIES a command: the hold that records one is
            // not scoped to a card's tool, so the type is the thing that keeps
            // it out of the walk.
            crate::BackgroundTask {
                task_id: "t2".to_owned(),
                is_backgrounded: true,
                task_type: "local_agent".to_owned(),
                description: "a sub-agent".to_owned(),
                command: Some("investigate".to_owned()),
                tool_use_id: None,
            },
            crate::BackgroundTask {
                task_id: "t3".to_owned(),
                is_backgrounded: true,
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

    /// The walk's window: nothing walked yet is
    /// walked, one inside the window is the answer rather than a reason to
    /// walk, and one past it is walked again and stored where views
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

    /// **A walk somebody else moved reaches the seat's viewers.** Another
    /// writer can put a walk in this same store, and while the store is
    /// fresh the loop's own rule walks nothing - so a loop that read the
    /// store only when its own walk ran would leave every client's section
    /// frozen.
    #[tokio::test]
    async fn a_walk_another_writer_moved_is_announced() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        // The seat's own pid, so the first pass walks a real tree.
        workspace.seed_test_claude_pid(&seat, std::process::id());
        // A walk from before the page, stale so the first pass replaces it and
        // announces the replacement: that frame says the pass is past its
        // baseline.
        let mut stale = walk_of(vec![entry_at(1)]);
        stale.scanned_at = std::time::SystemTime::now()
            .checked_sub(Duration::from_secs(60))
            .expect("a minute ago");
        workspace.store_process_snapshot(&seat, Some(stale));
        hold_for_a_page(&workspace, &seat);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("the first pass's own walk was never announced");
            };
            if matches!(moved.update, SessionUpdate::ProcessesChanged { .. }) {
                break;
            }
        }

        // A walk another writer made, writing the store the pass holds a
        // baseline of - and with the clock inside the window, so the loop's
        // own rule walks nothing this poke.
        let moved_walk = walk_of(vec![entry_at(4242)]);
        workspace.store_process_snapshot(&seat, Some(moved_walk));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("a walk the terminal moved never reached the seat's viewers");
            };
            let SessionUpdate::ProcessesChanged { key, snapshot } = moved.update else {
                continue;
            };
            assert_eq!(key, seat, "and it is addressed to the seat that was held");
            assert_eq!(
                snapshot.processes,
                vec![entry_at(4242)],
                "with the walk the store holds, not one this loop took",
            );
            return;
        }
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
        hold_for_a_page(&workspace, &seat);

        let child =
            Child(std::process::Command::new("sleep").arg("30").spawn().expect("a child to find"));
        let child_pid = child.0.id();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("a process arriving under the seat was never announced");
            };
            let SessionUpdate::ProcessesChanged { key, snapshot } = moved.update else {
                continue;
            };
            assert_eq!(key, seat);
            if snapshot.processes.iter().any(|entry| entry.pid == child_pid) {
                break;
            }
        }
        drop(child);
    }

    /// The watch's first walk, as the update that carries the hold's index
    /// now that the open no longer waits for it (#2506).
    async fn announced_index(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<SeatUpdate>,
    ) -> Arc<crate::file_index::FileIndex> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("the watch's first walk was never announced");
            };
            if let SessionUpdate::FileIndexChanged { index, .. } = moved.update {
                return index;
            }
        }
    }

    /// The watch's first row, as the update that carries the hold's scan now
    /// that the open no longer waits for it (#2506).
    async fn announced_work(
        updates: &mut tokio::sync::mpsc::UnboundedReceiver<SeatUpdate>,
    ) -> WorkState {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("the watch's first scan was never announced");
            };
            if let SessionUpdate::WorkChanged { work, .. } = moved.update {
                return work;
            }
        }
    }

    /// **The mark the watch sets is what makes the walk run inside the
    /// window.** A tree that moved is walked on the very next poke; one that
    /// did not waits for [`INDEX_STALENESS`] - and this drives the walk
    /// directly rather than racing the file watcher, whose promptness is the
    /// machine's business rather than this rule's.
    #[tokio::test]
    async fn the_watchs_mark_walks_inside_the_window() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        hold_for_a_page(&workspace, &seat);
        announced_index(&mut updates).await;
        let held_at = workspace.file_index(&seat).expect("the first pass stores it").read_at;

        assert!(
            !workspace.walk_files_if_stale(&seat, false).await,
            "a still tree inside the window is the answer rather than a reason to walk",
        );

        std::fs::write(dir.path().join("new.rs"), "").expect("write");
        assert!(workspace.walk_files_if_stale(&seat, true).await, "the mark walks at once");

        let walked = workspace.file_index(&seat).expect("the walk stored an index");
        assert!(walked.index.entries.contains_key("new.rs"), "with what the walk found");
        assert!(walked.read_at > held_at, "and the answer is the newer one");
    }

    /// **A file written into a held seat's tree reaches its viewers.** The walk
    /// is the loop's, so a composer's `@` list follows the tree the person is
    /// working in.
    #[tokio::test]
    async fn a_file_written_into_a_held_seat_reaches_its_viewers() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        hold_for_a_page(&workspace, &seat);

        std::fs::write(dir.path().join("new.rs"), "").expect("write");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("a file written into the seat's tree was never announced");
            };
            let SessionUpdate::FileIndexChanged { key, index } = moved.update else {
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
        // The first pass is held so the writes land before the walk it takes,
        // making the writes' own frame the only index frame there is.
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        workspace.seed_test_scan_gate(Arc::clone(&gate));
        // The hold returns with the watch spawned; the reads are at the gate.
        hold_for_a_page(&workspace, &seat);

        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(dir.path().join(name), "").expect("write");
        }
        gate.add_permits(8);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let first = loop {
            let Ok(Some(moved)) = tokio::time::timeout_at(deadline, updates.recv()).await else {
                panic!("the writes were never announced");
            };
            if let SessionUpdate::FileIndexChanged { index, .. } = moved.update {
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
            if matches!(moved.update, SessionUpdate::FileIndexChanged { .. }) {
                panic!("a second frame followed one poke's writes");
            }
        }
    }

    /// **Two walks that find the same index say nothing.** Unlike the process
    /// entries, a `FileCandidate` carries only paths and depths - no memory
    /// figure to drift as the tree works - so a still tree is genuinely still:
    /// the fixture repo runs no git maintenance, the one writer that moves
    /// `.git` under a tree nobody touched. The window is what makes this a
    /// walk: the poke at the far end of it takes one whether anything moved or
    /// not.
    #[tokio::test]
    async fn a_still_tree_announces_nothing() {
        let dir = a_repo();
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        hold_for_a_page(&workspace, &seat);
        // The first pass's own answer, drained so the window below opens after
        // it: the row is announced before the index in the same pass, so the
        // index's arrival says the pass is done.
        announced_work(&mut updates).await;
        let walked = announced_index(&mut updates).await;
        // The index the first pass left, so an announce names what moved rather
        // than counting it - triage is a read, not a re-derivation.
        let held = workspace.file_index(&seat).expect("the first pass stores the index");
        assert_eq!(held.index.entries.len(), walked.entries.len(), "and it is that index");
        let held_at = held.read_at;

        // The wait is for the seat's own second walk, not for a clock: the
        // property is that two walks of a still tree agree, and a re-walk that
        // slipped past a fixed window would leave that unexercised - a quiet
        // channel would then prove nothing. Every update before it is still a
        // failure; none is due for a tree nobody touched.
        let deadline = Instant::now() + Duration::from_secs(30);
        while workspace.file_index(&seat).is_none_or(|seen| seen.read_at <= held_at) {
            assert!(
                Instant::now() < deadline,
                "the seat walks its index again inside the deadline"
            );
            match tokio::time::timeout(Duration::from_millis(50), updates.recv()).await {
                Ok(Some(update)) => announce_failure(&update.update, &held.index),
                Ok(None) => panic!("the updates channel closed"),
                Err(_) => {}
            }
        }

        // And the spell past it, so the poke that took the walk is seen
        // through: the walk and the comparison are one body, and a frame this
        // tree never earned is due in neither.
        match tokio::time::timeout(Duration::from_secs(2), updates.recv()).await {
            Ok(Some(update)) => announce_failure(&update.update, &held.index),
            Ok(None) => panic!("the updates channel closed"),
            Err(_) => {}
        }
    }

    /// The words a live announce fails with: the update itself, and for an
    /// index change what entered or left the index the hold left behind.
    fn announce_message(update: &SessionUpdate, held: &crate::file_index::FileIndex) -> String {
        let SessionUpdate::FileIndexChanged { index, .. } = update else {
            return format!("a still tree announced something: {update:?}");
        };
        format!(
            "a still tree announced an index change: {} entries after {} before, added {} removed {}",
            index.entries.len(),
            held.entries.len(),
            name_list(index.entries.keys().filter(|key| !held.entries.contains_key(key.as_str()))),
            name_list(held.entries.keys().filter(|key| !index.entries.contains_key(key.as_str()))),
        )
    }

    /// A live announce on a still tree, named rather than counted.
    fn announce_failure(update: &SessionUpdate, held: &Arc<crate::file_index::FileIndex>) -> ! {
        panic!("{}", announce_message(update, held));
    }

    /// Up to [`NAMES_SHOWN`] names, and the rest counted rather than trailing
    /// off where the reader cannot tell a short list from a truncated one.
    fn name_list(names: impl Iterator<Item = impl std::fmt::Display>) -> String {
        const NAMES_SHOWN: usize = 30;
        let all: Vec<String> = names.map(|name| name.to_string()).collect();
        if all.len() > NAMES_SHOWN {
            let rest = all.len() - NAMES_SHOWN;
            return format!("{:?} and {rest} more", &all[..NAMES_SHOWN]);
        }
        format!("{all:?}")
    }

    /// **The announce names what moved**, which is what made this flake a
    /// read rather than a re-derivation, so the words are pinned.
    #[test]
    fn an_announce_names_what_moved() {
        let candidate = |rel: &str| crate::file_index::FileCandidate {
            rel_path: rel.to_owned(),
            rel_path_lower: rel.to_lowercase(),
            basename_lower: rel.to_lowercase(),
            depth: 0,
        };
        let held = crate::file_index::FileIndex {
            entries: [("gone.rs".to_owned(), candidate("gone.rs"))].into_iter().collect(),
        };
        let index = crate::file_index::FileIndex {
            entries: [("new.rs".to_owned(), candidate("new.rs"))].into_iter().collect(),
        };
        let message = announce_message(
            &SessionUpdate::FileIndexChanged { key: seat(), index: Arc::new(index) },
            &held,
        );
        assert!(message.contains("added [\"new.rs\"]"), "{message}");
        assert!(message.contains("removed [\"gone.rs\"]"), "{message}");

        let mut long = crate::file_index::FileIndex::default();
        for nth in 0..31 {
            long.entries.insert(format!("f{nth}.rs"), candidate(&format!("f{nth}.rs")));
        }
        let capped = announce_message(
            &SessionUpdate::FileIndexChanged { key: seat(), index: Arc::new(long) },
            &held,
        );
        assert!(capped.contains("and 1 more"), "a capped list counts the rest: {capped}");
    }

    /// A walk is an answer for the preference it was built under, and the
    /// seat's loop reads the preference per walk: a flip reaches the next one
    /// rather than waiting for a restart.
    #[tokio::test]
    async fn a_preference_flip_moves_the_next_walk() {
        let dir = a_repo();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").expect("write");
        std::fs::write(dir.path().join("ignored.rs"), "").expect("write");
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.seed_test_user_preferences(serde_json::json!({}));
        hold_for_a_page(&workspace, &seat);

        let held = announced_index(&mut updates).await;
        assert!(
            !held.entries.contains_key("ignored.rs"),
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

    /// **A watch stopped while it waited on the record's mark is noticed.**
    /// The release drops the stop sender, and a stop may fire beside the mark;
    /// either is a page that is no longer there, and the reads it was owed are
    /// not taken.
    #[test]
    fn a_watch_stopped_while_waiting_is_noticed_after_the_mark() {
        let (_open, mut waiting) = tokio::sync::oneshot::channel();
        assert!(!stopped_while_waiting(&mut waiting), "an open wait goes on");

        let (dropped, mut released) = tokio::sync::oneshot::channel::<()>();
        drop(dropped);
        assert!(stopped_while_waiting(&mut released), "a released seat is noticed");

        let (fired, mut stopped) = tokio::sync::oneshot::channel();
        fired.send(()).expect("the stop fires");
        assert!(stopped_while_waiting(&mut stopped), "and a fired stop is too");
    }

    /// **A release and re-hold share the gate a read still holds.** The
    /// record's read can store after its page has gone; a re-held seat that
    /// made a second gate would let that store land beside the new read's.
    #[tokio::test]
    async fn a_release_and_re_hold_share_the_gate_a_read_holds() {
        let dir = a_repo();
        let (workspace, _updates, _config) = a_workspace(dir.path());
        let seat = seat();
        workspace.hold_seat(&seat);
        let in_flight = workspace.held_work_seats.gate(&seat).expect("a held seat has one");

        workspace.release_seat(&seat);
        workspace.hold_seat(&seat);
        let reheld = workspace.held_work_seats.gate(&seat).expect("the seat is held again");
        assert!(
            Arc::ptr_eq(&in_flight, &reheld),
            "the gate outlived the hold while the read held it",
        );

        // With the read and the new hold both gone, the gate goes too: a later
        // hold starts a fresh one rather than inheriting a dead seat's.
        drop(in_flight);
        drop(reheld);
        workspace.release_seat(&seat);
        assert!(
            workspace.held_work_seats.gate(&seat).is_none(),
            "no read and no hold leaves no gate",
        );
        assert!(workspace.held_work_seats.lock().is_empty(), "and the entry goes with the hold");
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

        workspace.hold_seat(&seat);
        assert!(!workspace.held_work_seats.lock().is_empty(), "the first viewer holds");

        // The session ends under the hold, and the next viewer still holds:
        // the watch waits for the session rather than refusing (#1706).
        workspace.release_session_with_cascade(&seat);
        workspace.hold_seat(&seat);

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
        hold_for_a_page(&workspace, &seat);
        assert!(!workspace.held_work_seats.lock().is_empty(), "the hold is taken");
        // The watch's first pass settles before the session goes, so the
        // release's own frame is the next one and the window after it is the
        // quiet the loop's guard owes.
        announced_work(&mut updates).await;

        workspace.release_session_with_cascade(&seat);

        // The release announces itself (#1930), and that frame is where this
        // window opens: what must NOT follow it is anything about the seat's
        // work.
        let released = tokio::time::timeout(Duration::from_secs(3), updates.recv())
            .await
            .expect("the release announces itself")
            .expect("a live channel");
        assert!(
            matches!(released.update, SessionUpdate::Releasing { .. }),
            "the release's own frame is what opens this window: {released:?}",
        );

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
    /// **The mark starts the watch, and the guard is what keeps a sessionless
    /// seat quiet.** The read is marked here as a page's is, so the loop's
    /// first pass really runs and takes the sessionless branch - a plain hold
    /// would leave the watch waiting on its mark and prove nothing.
    ///
    /// The scan's own sessionless refusal is the second layer: a session can
    /// end between the guard and the read, and only the pair is observable -
    /// deleting either alone leaves this green.
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

        hold_for_a_page(&workspace, &seat);

        assert!(workspace.work_snapshot(&seat).is_none(), "nothing was read for it");
        assert!(
            !workspace.held_work_seats.lock().is_empty(),
            "and the hold stands, waiting for the session",
        );

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
        let (workspace, mut updates, _config) = a_workspace(dir.path());
        let seat = seat();

        hold_for_a_page(&workspace, &seat);
        workspace.hold_seat(&seat);
        announced_work(&mut updates).await;
        assert!(workspace.work_snapshot(&seat).is_some(), "the watch reads the seat's tree");

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

    /// The scan's `gh` spawn carries the identity of the seat's OWN
    /// project: a seat in one project must never read as another's, and a
    /// project that no longer resolves declares nothing, so `gh` falls
    /// back to its own login rather than to the server's environment.
    #[test]
    fn a_seats_scan_carries_its_own_projects_declared_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge dir");
        std::fs::write(
            forge.join("forge.toml"),
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]

[[orgs.projects]]
name = "alpha"
path = "/tmp/tp-alpha"
model = "claude-sonnet-5"

[orgs.projects.env]
GH_TOKEN = "alpha-token"

[[orgs.projects]]
name = "beta"
path = "/tmp/tp-beta"
model = "claude-sonnet-5"

[orgs.projects.env]
GH_TOKEN = "beta-token"

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("workspace");

        assert_eq!(
            workspace.project_env("alpha").get("GH_TOKEN").map(String::as_str),
            Some("alpha-token"),
            "alpha's scan carries alpha's own token",
        );
        assert_eq!(
            workspace.project_env("beta").get("GH_TOKEN").map(String::as_str),
            Some("beta-token"),
            "and beta's carries beta's, not the first project's",
        );
        assert!(
            workspace.project_env("gone").is_empty(),
            "a project that no longer resolves declares none",
        );
    }
}
