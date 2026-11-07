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
    /// The member was stopped deliberately and will not be resurrected.
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
    /// A second call while the member is running is a no-op, which is what
    /// replaces each member's own idempotency flag. A call after a stop or a
    /// give-up starts a new run and resets the counter.
    pub fn spawn<F, Fut>(&self, name: &str, policy: Policy, factory: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.0.ensure_driver();
        let factory: Factory = Box::new(move || Box::pin(factory()));
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .members
            .get(name)
            .is_some_and(|member| matches!(&member.state, MemberState::Running { .. }))
        {
            return;
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
    }

    /// Stop `name` deliberately: cancel its run, abort it, and never
    /// resurrect it.
    pub fn stop(&self, name: &str) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(member) = state.members.get_mut(name) else { return };
        member.state = MemberState::Stopped;
        if let Some(token) = state.tokens.get(name) {
            token.cancel();
        }
        // **A stop during a backoff is still a stop.** The timer armed by the
        // death carries the old generation, so bumping it here is what makes
        // that timer's restart a no-op.
        let generation = state.generations.get(name).copied().unwrap_or(0) + 1;
        state.generations.insert(name.to_owned(), generation);
        drop(state);
        let _ = self.0.tx.send(Command::Stop { name: name.to_owned() });
        tracing::debug!(
            target: "forge_supervise",
            event_name = "subsystem_stopped",
            subsystem = %name,
            "the subsystem was stopped",
        );
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
/// fresh future from the same captured state.
type Factory = Box<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

enum Command {
    Start { name: String, generation: u64, token: CancellationToken },
    Restart { name: String, generation: u64, token: CancellationToken },
    Stop { name: String },
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
    aborts: HashMap<String, (u64, AbortHandle)>,
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
                        start_run(&inner, &mut set, &mut runs, &name, generation, token, false);
                    }
                    Command::Restart { name, generation, token } => {
                        start_run(&inner, &mut set, &mut runs, &name, generation, token, true);
                    }
                    Command::Stop { name } => abort_if_stopped(&inner, &name),
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

/// Spawn one run of `name`, unless a newer run has replaced it.
fn start_run(
    inner: &Arc<Inner>,
    set: &mut JoinSet<()>,
    runs: &mut HashMap<Id, Run>,
    name: &str,
    generation: u64,
    token: CancellationToken,
    restart: bool,
) {
    let future = {
        let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.generations.get(name).copied() != Some(generation) {
            return;
        }
        if restart && let Some(member) = state.members.get_mut(name) {
            member.state = MemberState::Running { since: SystemTime::now() };
        }
        let Some(factory) = state.factories.get(name) else { return };
        factory()
    };
    let handle = set.spawn(future);
    let id = handle.id();
    runs.insert(
        id,
        Run { name: name.to_owned(), generation, token, started_at: tokio::time::Instant::now() },
    );
    let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
    state.aborts.insert(name.to_owned(), (generation, handle));
    drop(state);
    if restart {
        tracing::warn!(
            target: "forge_supervise",
            event_name = "subsystem_restarted",
            subsystem = %name,
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

/// Abort the current run of `name` when its token says the stop is ours. A
/// newer run's token is not cancelled, so a stop that raced a fresh spawn
/// leaves the new run alone.
fn abort_if_stopped(inner: &Arc<Inner>, name: &str) {
    let abort = {
        let state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
        let cancelled = state.tokens.get(name).is_some_and(CancellationToken::is_cancelled);
        cancelled.then(|| state.aborts.get(name).map(|(_, abort)| abort.clone())).flatten()
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
    let reason = error_reason(error);
    let mut state = inner.state.lock().unwrap_or_else(PoisonError::into_inner);
    if state.aborts.get(&run.name).is_some_and(|(generation, _)| *generation == run.generation) {
        state.aborts.remove(&run.name);
    }
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
            member.state = MemberState::GivenUp { after: 0 };
            tracing::warn!(
                target: "forge_supervise",
                event_name = "subsystem_exited",
                subsystem = %run.name,
                reason = %reason,
                "the subsystem exited and is not restarted",
            );
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
                        let _ = inner.tx.send(Command::Restart { name, generation, token });
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A factory that counts its runs, so a test watches the restart without
    /// a member's own loop.
    fn counting(
        runs: &Arc<AtomicU32>,
    ) -> impl Fn() -> std::future::Ready<()> + Send + Sync + 'static {
        let runs = Arc::clone(runs);
        move || {
            runs.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        }
    }

    /// **An immediate return restarts per policy.** Catches a supervisor
    /// that observes the completion and never calls the factory again.
    #[tokio::test(start_paused = true)]
    async fn an_immediate_return_restarts_per_policy() {
        let supervisor = Supervisor::new();
        let runs = Arc::new(AtomicU32::new(0));
        supervisor.spawn("probe", Policy::backoff(), counting(&runs));

        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the first run started");

        tokio::time::advance(DEFAULT_START).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the immediate return restarted once");
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
        supervisor.spawn("boom", policy, move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
                panic!("the pump died");
            }
        });

        for _ in 0..3 {
            rx.recv().await.expect("a run started");
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        tokio::task::yield_now().await;

        let member = supervisor
            .snapshot()
            .into_iter()
            .find(|member| member.name == "boom")
            .expect("the member is registered");
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

    /// **A stopped member is never resurrected.** Catches a supervisor that
    /// restarts the run its own stop aborted.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_member_is_not_restarted() {
        let supervisor = Supervisor::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        supervisor.spawn("probe", Policy::backoff(), move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
                std::future::pending::<()>().await;
            }
        });
        rx.recv().await.expect("the run started");

        supervisor.stop("probe");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert!(rx.try_recv().is_err(), "a stopped member is never resurrected");
        let member = supervisor
            .snapshot()
            .into_iter()
            .find(|member| member.name == "probe")
            .expect("the member is registered");
        assert_eq!(member.state, MemberState::Stopped, "the stop is recorded");
    }

    /// **A stop during a backoff is still a stop.** The timer armed by the
    /// death must not resurrect the member the stop took down. Catches a
    /// supervisor whose stop only aborts a live run.
    #[tokio::test(start_paused = true)]
    async fn a_stop_during_backoff_is_not_resurrected() {
        let supervisor = Supervisor::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        supervisor.spawn("probe", Policy::backoff(), move || {
            let tx = tx.clone();
            async move {
                let _ = tx.send(());
            }
        });
        // The first run returns immediately, so the member is in backoff.
        rx.recv().await.expect("the first run started");
        tokio::task::yield_now().await;

        supervisor.stop("probe");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert!(rx.try_recv().is_err(), "a stop during backoff is never resurrected");
        let member = supervisor
            .snapshot()
            .into_iter()
            .find(|member| member.name == "probe")
            .expect("the member is registered");
        assert_eq!(member.state, MemberState::Stopped, "the stop is recorded");
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
        supervisor.spawn("probe", policy, move || {
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
        });

        assert_eq!(rx.recv().await, Some(0), "the first run returns immediately");
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(rx.recv().await, Some(1), "the second run starts");
        tokio::time::advance(Duration::from_secs(11)).await;
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(rx.recv().await, Some(2), "the third run starts");

        let member = supervisor
            .snapshot()
            .into_iter()
            .find(|member| member.name == "probe")
            .expect("the member is registered");
        assert_eq!(member.restarts, 1, "a run that outlived reset_after starts the count over");
    }
}
