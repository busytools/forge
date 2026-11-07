//! Supervision for forge's long-lived background tasks.
//!
//! One registry owns every member: it starts the member's future, observes
//! its completion through a [`JoinSet`], restarts a death with bounded
//! backoff, refuses to resurrect a deliberate stop, and answers a snapshot
//! of what is running. A member's own loop is untouched; only its spawn and
//! its idempotency live here.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, Id, JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

/// The first wait after a death.
pub const DEFAULT_START: Duration = Duration::from_secs(1);
/// The longest wait between restarts.
pub const DEFAULT_CAP: Duration = Duration::from_secs(60);
/// How many consecutive failures a member is restarted through.
pub const DEFAULT_GIVE_UP_AFTER: u32 = 10;
/// How long a run must last before its death starts the count over.
pub const DEFAULT_RESET_AFTER: Duration = Duration::from_secs(60);

/// How a member's death is handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    /// Report a death; never restart.
    Never,
    /// Restart with backoff, give up after a bounded number of attempts.
    Backoff { start: Duration, cap: Duration, give_up_after: u32, reset_after: Duration },
}

impl Policy {
    /// The crate's defaults: 1 s doubling to 60 s, ten failures, a 60 s
    /// stable run resetting the count.
    pub fn backoff() -> Self {
        Self::Backoff {
            start: DEFAULT_START,
            cap: DEFAULT_CAP,
            give_up_after: DEFAULT_GIVE_UP_AFTER,
            reset_after: DEFAULT_RESET_AFTER,
        }
    }
}

/// What a member is doing, as a snapshot reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberState {
    /// The member's future is running, or is waiting out a backoff before
    /// its next run.
    Running { since: SystemTime },
    /// The member died and its restart budget is spent.
    GivenUp { after: u32 },
    /// The member was stopped deliberately, or a report-only member returned
    /// on its own.
    Stopped,
}

/// One member as a snapshot reports it.
#[derive(Clone, Debug)]
pub struct Member {
    pub name: String,
    pub policy: Policy,
    pub state: MemberState,
    pub restarts: u32,
    pub last_error: Option<String>,
}

/// The process's supervisor. Cheap to clone; every clone reaches the same
/// registry.
#[derive(Clone)]
pub struct Supervisor(Arc<Inner>);

impl Supervisor {
    /// Build the registry and start its driver on the current runtime.
    ///
    /// A caller with no runtime gets a registry whose driver starts on the
    /// first [`Self::spawn`] inside one, so construction never needs a
    /// runtime of its own.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner =
            Arc::new(Inner { tx, rx: Mutex::new(Some(rx)), state: Mutex::new(State::default()) });
        inner.ensure_driver();
        Self(inner)
    }

    /// Register `name` and start its first run.
    ///
    /// Returns `true` when this call started the member and `false` when it
    /// was already running, so a caller that must know whether its factory
    /// was adopted can read the answer. A second call while the member runs
    /// is a no-op, which is what replaces each member's own idempotency flag.
    /// A call after a stop or a give-up starts a new run and resets the
    /// counter.
    pub fn spawn<F, Fut>(&self, name: &str, policy: Policy, factory: F) -> bool
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.0.ensure_driver();
        let factory: Factory = Arc::new(move || Box::pin(factory()));
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .members
            .get(name)
            .is_some_and(|member| matches!(&member.state, MemberState::Running { .. }))
        {
            return false;
        }
        let generation = state.generations.get(name).copied().unwrap_or(0) + 1;
        let token = CancellationToken::new();
        state.generations.insert(name.to_owned(), generation);
        state.tokens.insert(name.to_owned(), token.clone());
        state.factories.insert(name.to_owned(), factory);
        state.members.insert(
            name.to_owned(),
            Member {
                name: name.to_owned(),
                policy,
                state: MemberState::Running { since: SystemTime::now() },
                restarts: 0,
                last_error: None,
            },
        );
        drop(state);
        let _ = self.0.tx.send(Command::Start { name: name.to_owned(), generation, token });
        true
    }

    /// Stop `name` deliberately: cancel its run, abort it, and never
    /// resurrect it.
    ///
    /// The stop names the generation it took down, so a run replaced by a
    /// fresh `spawn` in the same window is left alone.
    pub fn stop(&self, name: &str) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(member) = state.members.get_mut(name) else { return };
        member.state = MemberState::Stopped;
        if let Some(token) = state.tokens.get(name) {
            token.cancel();
        }
        let generation = state.generations.get(name).copied().unwrap_or(0);
        drop(state);
        let _ = self.0.tx.send(Command::Stop { name: name.to_owned(), generation });
        tracing::debug!(
            target: "forge_supervise",
            event_name = "subsystem_stopped",
            subsystem = %name,
            "the subsystem was stopped",
        );
    }

    /// Remove `name` from the registry, abort its run, and forget its
    /// factory. A call after a stop is what keeps a per-host member name
    /// from accumulating dead rows.
    pub fn forget(&self, name: &str) {
        let aborts: Vec<AbortHandle> = {
            let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(token) = state.tokens.remove(name) {
                token.cancel();
            }
            state.members.remove(name);
            state.factories.remove(name);
            state.generations.remove(name);
            let keys: Vec<(String, u64)> = state
                .aborts
                .keys()
                .filter(|(member, _)| member.as_str() == name)
                .cloned()
                .collect();
            keys.into_iter().filter_map(|key| state.aborts.remove(&key)).collect()
        };
        for abort in aborts {
            abort.abort();
        }
    }

    /// Every member, ordered by name.
    pub fn snapshot(&self) -> Vec<Member> {
        let state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut members: Vec<Member> = state.members.values().cloned().collect();
        members.sort_by(|a, b| a.name.cmp(&b.name));
        members
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// A member's future factory: called once per run, so a restart builds a
/// fresh future from the same captured state. Shared rather than boxed so a
/// caller can clone it out from under the registry lock before invoking it.
type Factory = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

enum Command {
    Start { name: String, generation: u64, token: CancellationToken },
    Restart { name: String, generation: u64, token: CancellationToken, reason: String },
    Stop { name: String, generation: u64 },
}

struct Run {
    name: String,
    generation: u64,
    token: CancellationToken,
    /// The tokio clock, so a paused test's `advance` moves it with the
    /// member's own sleeps.
    started_at: tokio::time::Instant,
}

struct Inner {
    tx: mpsc::UnboundedSender<Command>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Command>>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    members: HashMap<String, Member>,
    factories: HashMap<String, Factory>,
    tokens: HashMap<String, CancellationToken>,
    generations: HashMap<String, u64>,
    /// **One handle per run, not one slot per name.** A stop then a spawn in
    /// the same window must abort the run it stopped, not the new one.
    aborts: HashMap<(String, u64), AbortHandle>,
}

impl Inner {
    /// Start the driver on the current runtime, once. A caller with no
    /// runtime leaves the receiver for the next call.
    fn ensure_driver(self: &Arc<Self>) {
        let mut slot = self.rx.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(rx) = slot.take() else { return };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(drive(Arc::downgrade(self), rx));
            }
            Err(_) => {
                *slot = Some(rx);
            }
        }
    }
}

/// The registry's one task: it owns the [`JoinSet`], so a completion is
/// observed with no per-member handle.
async fn drive(weak: Weak<Inner>, mut rx: mpsc::UnboundedReceiver<Command>) {
    let mut set = JoinSet::new();
    let mut runs: HashMap<Id, Run> = HashMap::new();
    loop {
        tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else { break };
                let Some(inner) = weak.upgrade() else { break };
                match command {
                    Command::Start { name, generation, token } => {
                        start_run(&inner, &mut set, &mut runs, &name, generation, token, None);
                    }
                    Command::Restart { name, generation, token, reason } => {
                        // **A stop during a backoff is still a stop.** The
                        // timer armed by the death carries the run's token,
                        // and a cancelled one must not resurrect the member.
                        if !token.is_cancelled() {
                            start_run(
                                &inner,
                                &mut set,
                                &mut runs,
                                &name,
                                generation,
                                token,
                                Some(&reason),
                            );
                        }
                    }
                    Command::Stop { name, generation } => {
                        abort_run(&inner, &name, generation);
                    }
                }
            }
            // **An empty set is not a ready branch.** `join_next_with_id`
            // answers `None` at once when nothing is running, and polling it
            // unguarded spins the driver at full CPU between runs.
            joined = set.join_next_with_id(), if !set.is_empty() => {
                let Some(joined) = joined else { continue };
                let Some(inner) = weak.upgrade() else { break };
                match joined {
                    Ok((id, ())) => on_exit(&inner, &mut runs, id, None),
                    Err(error) => {
                        let id = error.id();
                        on_exit(&inner, &mut runs, id, Some(error));
                    }
                }
            }
        }
    }
}

/// Spawn one run of `name`, unless a newer run has replaced it. `restart`
/// carries the death's reason when this is a restart.
fn start_run(
    inner: &Arc<Inner>,
    set: &mut JoinSet<()>,
    runs: &mut HashMap<Id, Run>,
    name: &str,
    generation: u64,
    token: CancellationToken,
    restart: Option<&str>,
) {
    let (factory, restarts) = {
        let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.generations.get(name).copied() != Some(generation) {
            return;
        }
        if restart.is_some()
            && let Some(member) = state.members.get_mut(name)
        {
            member.state = MemberState::Running { since: SystemTime::now() };
        }
        let factory = state.factories.get(name).cloned();
        let restarts = state.members.get(name).map_or(0, |member| member.restarts);
        (factory, restarts)
    };
    // **The factory runs with no registry lock held.** A factory that
    // re-enters the supervisor would deadlock every operation, and one that
    // panicked would kill the driver and every member with it.
    let Some(factory) = factory else { return };
    let handle = set.spawn(factory());
    let id = handle.id();
    runs.insert(
        id,
        Run { name: name.to_owned(), generation, token, started_at: tokio::time::Instant::now() },
    );
    let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
    state.aborts.insert((name.to_owned(), generation), handle);
    drop(state);
    if let Some(reason) = restart {
        tracing::warn!(
            target: "forge_supervise",
            event_name = "subsystem_restarted",
            subsystem = %name,
            reason = %reason,
            restarts,
            "the subsystem was restarted",
        );
    } else {
        tracing::debug!(
            target: "forge_supervise",
            event_name = "subsystem_started",
            subsystem = %name,
            "the subsystem started",
        );
    }
}

/// Abort the run of `name` at `generation`, if that run is still the one
/// registered. A newer run's handle is keyed by its own generation, so a
/// stop that raced a fresh spawn leaves the new run alone.
fn abort_run(inner: &Arc<Inner>, name: &str, generation: u64) {
    let abort = {
        let state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.aborts.get(&(name.to_owned(), generation)).cloned()
    };
    if let Some(abort) = abort {
        abort.abort();
    }
}

/// One run ended. A stop is recorded and left alone; a death is recorded and
/// restarted per policy.
fn on_exit(inner: &Arc<Inner>, runs: &mut HashMap<Id, Run>, id: Id, error: Option<JoinError>) {
    let Some(run) = runs.remove(&id) else { return };
    let ran_for = run.started_at.elapsed();
    let stopped = run.token.is_cancelled();
    let clean = error.is_none();
    let reason = error_reason(error);
    let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
    state.aborts.remove(&(run.name.clone(), run.generation));
    if stopped {
        return;
    }
    if state.generations.get(&run.name).copied() != Some(run.generation) {
        return;
    }
    let Some(member) = state.members.get_mut(&run.name) else { return };
    member.last_error = Some(reason.clone());
    match member.policy {
        Policy::Never => {
            // **A report-only member that returns is not a failure.** The
            // normal end of a per-host reader is a clean return; only a
            // panic claims forge's health. Rule 20 applies.
            if clean {
                member.state = MemberState::Stopped;
                tracing::debug!(
                    target: "forge_supervise",
                    event_name = "subsystem_exited",
                    subsystem = %run.name,
                    "the subsystem exited",
                );
            } else {
                member.state = MemberState::GivenUp { after: 0 };
                tracing::warn!(
                    target: "forge_supervise",
                    event_name = "subsystem_exited",
                    subsystem = %run.name,
                    reason = %reason,
                    "the subsystem exited and is not restarted",
                );
            }
        }
        Policy::Backoff { start, cap, give_up_after, reset_after } => {
            if ran_for >= reset_after {
                member.restarts = 0;
            }
            if member.restarts + 1 >= give_up_after {
                member.state = MemberState::GivenUp { after: member.restarts };
                tracing::error!(
                    target: "forge_supervise",
                    event_name = "subsystem_gave_up",
                    subsystem = %run.name,
                    restarts = member.restarts,
                    reason = %reason,
                    "the subsystem died again after its restart budget; it is left down",
                );
            } else {
                member.restarts += 1;
                let delay = backoff(start, cap, member.restarts);
                tracing::warn!(
                    target: "forge_supervise",
                    event_name = "subsystem_exited",
                    subsystem = %run.name,
                    reason = %reason,
                    restart_in_ms = delay.as_millis(),
                    "the subsystem died; it will be restarted",
                );
                let weak = Arc::downgrade(inner);
                let name = run.name.clone();
                let generation = run.generation;
                let token = run.token.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if let Some(inner) = weak.upgrade() {
                        let _ = inner.tx.send(Command::Restart { name, generation, token, reason });
                    }
                });
            }
        }
    }
}

/// The wait before restart number `restarts`: `start` doubling, capped.
fn backoff(start: Duration, cap: Duration, restarts: u32) -> Duration {
    let factor = 1u32.checked_shl(restarts.saturating_sub(1)).unwrap_or(u32::MAX);
    start.saturating_mul(factor).min(cap)
}

/// Why a run ended, in the words the snapshot and the log carry.
fn error_reason(error: Option<JoinError>) -> String {
    match error {
        Some(error) if error.is_panic() => {
            let payload = error.into_panic();
            let text = payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".to_owned());
            format!("panicked: {text}")
        }
        Some(error) if error.is_cancelled() => "cancelled".to_owned(),
        Some(error) => format!("the task failed: {error}"),
        None => "the task returned".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use super::*;

    /// A factory that counts its runs, so a test watches the restart without
    /// a member's own loop.
    fn counting(
        runs: Arc<AtomicU32>,
    ) -> impl Fn() -> std::future::Ready<()> + Send + Sync + 'static {
        move || {
            runs.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        }
    }

    fn member(supervisor: &Supervisor, name: &str) -> Member {
        supervisor
            .snapshot()
            .into_iter()
            .find(|member| member.name == name)
            .expect("the member is registered")
    }

    /// **An immediate return restarts per policy.** Catches a supervisor
    /// that observes the completion and never calls the factory again.
    #[tokio::test(start_paused = true)]
    async fn an_immediate_return_restarts_per_policy() {
        let supervisor = Supervisor::new();
        let runs = Arc::new(AtomicU32::new(0));
        assert!(supervisor.spawn("probe", Policy::backoff(), counting(Arc::clone(&runs))));

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the first run started");

        tokio::time::advance(DEFAULT_START).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the immediate return restarted once");
    }

    /// **The wait doubles from `start` and stops at `cap`.** Catches a
    /// supervisor that reuses the first interval, or one that grows without
    /// a ceiling.
    #[test]
    fn backoff_doubles_then_caps() {
        let start = Duration::from_secs(1);
        let cap = Duration::from_secs(60);
        let waits: Vec<u64> =
            (1..=8).map(|restarts| backoff(start, cap, restarts).as_secs()).collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 32, 60, 60], "start doubles and stops at the cap");
    }

    /// **A second spawn while the member runs is a no-op.** Catches a
    /// registry that starts a second run of the same name.
    #[tokio::test(start_paused = true)]
    async fn a_second_spawn_while_running_is_a_no_op() {
        let supervisor = Supervisor::new();
        let runs = Arc::new(AtomicU32::new(0));
        assert!(
            supervisor.spawn("probe", Policy::backoff(), counting(Arc::clone(&runs))),
            "the first starts"
        );
        assert!(
            !supervisor.spawn("probe", Policy::backoff(), counting(Arc::clone(&runs))),
            "the second is a no-op",
        );
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "only one run started");
        assert_eq!(supervisor.snapshot().len(), 1, "one member in the registry");
    }

    /// **A panicking task is restarted until the budget is spent.** Catches
    /// a supervisor that treats a panic as a stop, or one that never gives
    /// up.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_task_gives_up_after_the_budget() {
        let supervisor = Supervisor::new();
        let policy = Policy::Backoff {
            start: Duration::from_millis(1),
            cap: Duration::from_millis(1),
            give_up_after: 3,
            reset_after: DEFAULT_RESET_AFTER,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(supervisor.spawn("boom", policy, move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
                panic!("the pump died");
            }
        }));

        // **The last failure is not advanced past.** The budget is spent on
        // the third death, so a supervisor that restarts it instead would
        // still be running when the snapshot is read.
        for run in 0..3 {
            rx.recv().await.expect("a run started");
            if run < 2 {
                tokio::time::advance(Duration::from_millis(1)).await;
            }
        }
        tokio::task::yield_now().await;

        let member = member(&supervisor, "boom");
        assert!(
            matches!(member.state, MemberState::GivenUp { .. }),
            "the budget is spent: {:?}",
            member.state,
        );
        assert!(
            member.last_error.as_deref().is_some_and(|error| error.contains("the pump died")),
            "the panic payload is recorded: {:?}",
            member.last_error,
        );
    }

    /// **A spawn after a give-up starts a new run and resets the counter.**
    /// Catches a registry that leaves a given-up member dead for the process.
    #[tokio::test(start_paused = true)]
    async fn a_spawn_after_give_up_starts_a_new_run() {
        let supervisor = Supervisor::new();
        let policy = Policy::Backoff {
            start: Duration::from_millis(1),
            cap: Duration::from_millis(1),
            give_up_after: 1,
            reset_after: DEFAULT_RESET_AFTER,
        };
        let runs = Arc::new(AtomicU32::new(0));
        assert!(supervisor.spawn("probe", policy, counting(Arc::clone(&runs))));
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            matches!(member(&supervisor, "probe").state, MemberState::GivenUp { .. }),
            "the budget is spent",
        );

        // The new run stays up, so the counter it inherits is read before
        // its own first death would move it.
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(
            supervisor.spawn("probe", Policy::backoff(), move || {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(());
                    std::future::pending::<()>().await;
                }
            }),
            "a give-up can be re-spawned",
        );
        rx.recv().await.expect("the new run started");
        assert_eq!(member(&supervisor, "probe").restarts, 0, "the new run resets the counter");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the first factory ran once");
    }

    /// **A stop then a spawn of the same name aborts the run the stop took
    /// down.** The stop's command names the generation it stopped, so a
    /// fresh run registered under the same name in the same window is left
    /// alone and the old run does not live on as a leaked task.
    #[tokio::test(start_paused = true)]
    async fn a_stop_then_spawn_aborts_the_run_it_stopped() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let supervisor = Supervisor::new();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let first_freed = Arc::new(AtomicBool::new(false));
        let freed = Arc::clone(&first_freed);
        assert!(supervisor.spawn("probe", Policy::backoff(), move || {
            let started_tx = started_tx.clone();
            let freed = Arc::clone(&freed);
            async move {
                let _dropped = Dropped(freed);
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
            }
        }));
        started_rx.recv().await.expect("the first run started");

        supervisor.stop("probe");
        // The new run registers before the driver processes the stop's
        // command, which is the window the old one-slot map lost the old
        // run's handle in.
        assert!(supervisor.spawn("probe", Policy::backoff(), || async {
            std::future::pending::<()>().await;
        }));
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }

        assert!(
            first_freed.load(Ordering::SeqCst),
            "the stopped run was aborted even though a new run took its name",
        );
        assert!(
            matches!(member(&supervisor, "probe").state, MemberState::Running { .. }),
            "the new run is the one alive",
        );
    }

    /// **A report-only member that returns is stopped, not given up.** Its
    /// normal end is not a failure, so the snapshot must not draw it as one.
    #[tokio::test(start_paused = true)]
    async fn a_never_member_that_returns_is_stopped() {
        let supervisor = Supervisor::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(supervisor.spawn("probe", Policy::Never, move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
            }
        }));
        rx.recv().await.expect("the run started");
        tokio::task::yield_now().await;

        let member = member(&supervisor, "probe");
        assert_eq!(member.state, MemberState::Stopped, "a clean return is not a give-up");
        assert_eq!(member.restarts, 0, "nothing was restarted");
    }

    /// **A report-only member that panics is given up with its payload.**
    /// Catches a supervisor that treats a report-only death as a clean stop.
    #[tokio::test(start_paused = true)]
    async fn a_never_member_that_panics_is_given_up() {
        let supervisor = Supervisor::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(supervisor.spawn("boom", Policy::Never, move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
                panic!("the report-only member died");
            }
        }));
        rx.recv().await.expect("the run started");
        tokio::task::yield_now().await;

        let member = member(&supervisor, "boom");
        assert!(matches!(member.state, MemberState::GivenUp { .. }), "a panic is a death");
        assert!(
            member
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("the report-only member died")),
            "the panic payload is recorded: {:?}",
            member.last_error,
        );
    }

    /// **A stopped member is never resurrected.** The run completes after
    /// the stop, so a stop that failed to cancel it would restart it and the
    /// second run would show on the channel. A stop is also not a failed
    /// attempt: its `restarts` and `last_error` stay untouched, which is what
    /// makes the explicit stop check load-bearing.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_member_is_not_restarted() {
        let supervisor = Supervisor::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(supervisor.spawn("probe", Policy::backoff(), move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }));
        rx.recv().await.expect("the run started");

        supervisor.stop("probe");
        // The first advance lets the run finish; the second fires any restart
        // timer the death scheduled, so a stop that failed to cancel the run
        // shows its second run here.
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert!(rx.try_recv().is_err(), "a stopped member is never resurrected");
        let member = member(&supervisor, "probe");
        assert_eq!(member.state, MemberState::Stopped, "the stop is recorded");
        assert_eq!(member.restarts, 0, "a stop is not a failed attempt");
        assert!(member.last_error.is_none(), "a stop is not a death");
    }

    /// **A stop during a backoff is still a stop.** The timer armed by the
    /// death must not resurrect the member the stop took down. Catches a
    /// supervisor whose stop only aborts a live run.
    #[tokio::test(start_paused = true)]
    async fn a_stop_during_backoff_is_not_resurrected() {
        let supervisor = Supervisor::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(supervisor.spawn("probe", Policy::backoff(), move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
            }
        }));
        // The first run returns immediately, so the member is in backoff.
        rx.recv().await.expect("the first run started");
        tokio::task::yield_now().await;

        supervisor.stop("probe");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert!(rx.try_recv().is_err(), "a stop during backoff is never resurrected");
        assert_eq!(
            member(&supervisor, "probe").state,
            MemberState::Stopped,
            "the stop is recorded"
        );
    }

    /// **A run that outlives `reset_after` starts the count over.** Catches
    /// a supervisor that accumulates failures across a stable run.
    #[tokio::test(start_paused = true)]
    async fn a_run_that_outlives_reset_after_resets_the_counter() {
        let supervisor = Supervisor::new();
        let policy = Policy::Backoff {
            start: Duration::from_millis(1),
            cap: Duration::from_millis(1),
            give_up_after: 5,
            reset_after: Duration::from_secs(10),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runs = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&runs);
        assert!(supervisor.spawn("probe", policy, move || {
            let tx = tx.clone();
            let counter = Arc::clone(&counter);
            async move {
                let run = counter.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(run);
                match run {
                    0 => {}
                    1 => tokio::time::sleep(Duration::from_secs(11)).await,
                    _ => std::future::pending::<()>().await,
                }
            }
        }));

        assert_eq!(rx.recv().await, Some(0), "the first run returns immediately");
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(rx.recv().await, Some(1), "the second run starts");
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(rx.recv().await, Some(2), "the third run starts");

        assert_eq!(
            member(&supervisor, "probe").restarts,
            1,
            "a run that outlived reset_after starts the count over",
        );
    }
}
