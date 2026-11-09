//! The idle lead sweep: forge injects the board sweep when a project's
//! lead has nothing in flight.
//!
//! The trigger is this module's whole subject, not the procedure: the
//! lead charter already carries how a sweep is walked, so the injected
//! prompt only names the moment and the ending it owes. The pass never
//! wakes a sleeping session - a lead with no session is skipped, and its
//! next session start sweeps, as the charter says.

use std::sync::Arc;
use std::time::SystemTime;

use forge_primitives::tasks::TaskStatus;
use tracing::Instrument;

use crate::SessionSlot;
use crate::chase::{Deliver, Delivery, PromptDeliver};
use crate::mcp::workers::types::WorkerEntry;
use crate::workspace::Workspace;

/// How often the loop looks. A lead that goes idle is swept within this.
pub const IDLE_SWEEP_INTERVAL_SECS: u64 = 60;

/// At most one idle-triggered sweep per lead inside this window.
pub const IDLE_SWEEP_MIN_GAP_SECS: u64 = 2 * 60 * 60;

/// A lead that has stayed busy this long is swept anyway: the prompt
/// queues behind the running turn, so it is a nudge and not an
/// interruption.
pub const IDLE_SWEEP_FALLBACK_SECS: u64 = 6 * 60 * 60;

/// The idle path's prompt.
pub const IDLE_SWEEP_PROMPT: &str = "Nothing is in flight on this board. Sweep it now, and end \
     with the next three picks from the backlog.";

/// The fallback's own lead-in. The fallback fires precisely when the lead
/// is NOT idle, so it must not claim the board is quiet.
pub const IDLE_SWEEP_FALLBACK_PROMPT: &str = "Six hours have passed since the last sweep. Sweep \
     the board when you can, and end with the next three picks from the backlog.";

/// One lead's injected-sweep clock, held in memory so a restart re-arms
/// every lead.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SweepClock {
    /// When this workspace first saw the lead live, re-seeded on every
    /// tick that finds the seat asleep. The fallback measures from here
    /// only until a sweep has landed this process.
    pub(crate) first_seen: SystemTime,
    /// When the last injected sweep was delivered. Kept across a session
    /// restart: the two-hour gap belongs to the lead, and only a process
    /// restart re-arms it.
    pub(crate) last_delivered: Option<SystemTime>,
    /// Whether the last delivery attempt failed, so a failure episode is
    /// logged once rather than on every retry tick.
    pub(crate) failing: bool,
}

/// How long `now` is after `since`, and zero when the clock went
/// backwards. An age that cannot be measured is unknown, never
/// infinitely old: one NTP step must not sweep every lead.
fn age_secs(since: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(since).map_or(0, |d| d.as_secs())
}

impl Workspace {
    /// One pass over every project: deliver the injected sweep to each
    /// lead that is idle, or overdue for one. Returns how many prompts
    /// reached a session.
    pub fn idle_sweep(&self, deliver: &dyn Deliver, now: SystemTime) -> usize {
        let views = self.list_projects();
        // A project removed from the config takes its lead's clock with
        // it.
        self.idle_sweeps.lock().retain(|slot, _| {
            views.iter().any(|view| view.org == slot.org() && view.name == slot.project())
        });
        let mut delivered = 0;
        for view in views {
            let lead = SessionSlot::lead(&view.org, &view.name);
            // Never wake a sleeping session: its next start sweeps, as
            // the charter says. Its clock keeps `last_delivered` - a
            // session restart is not a process restart - while
            // `first_seen` re-arms from the wake.
            if !self.seat_is_live(&lead) {
                if let Some(clock) = self.idle_sweeps.lock().get_mut(&lead) {
                    clock.first_seen = now;
                }
                continue;
            }
            let idle = lead_is_idle(self, &lead);
            let due = {
                let mut clocks = self.idle_sweeps.lock();
                let clock = clocks.entry(lead.clone()).or_insert(SweepClock {
                    first_seen: now,
                    last_delivered: None,
                    failing: false,
                });
                let age = clock
                    .last_delivered
                    .map_or(age_secs(clock.first_seen, now), |last| age_secs(last, now));
                match clock.last_delivered {
                    None => idle || age >= IDLE_SWEEP_FALLBACK_SECS,
                    Some(_) => {
                        (idle && age >= IDLE_SWEEP_MIN_GAP_SECS) || age >= IDLE_SWEEP_FALLBACK_SECS
                    }
                }
            };
            if !due {
                continue;
            }
            let text = if idle { IDLE_SWEEP_PROMPT } else { IDLE_SWEEP_FALLBACK_PROMPT };
            match deliver.to_seat(&lead, text) {
                Delivery::Delivered => {
                    let recovered = {
                        let mut clocks = self.idle_sweeps.lock();
                        clocks.get_mut(&lead).is_some_and(|clock| {
                            clock.last_delivered = Some(now);
                            std::mem::replace(&mut clock.failing, false)
                        })
                    };
                    delivered += 1;
                    tracing::debug!(
                        target: "forge_workspace::idle_sweep",
                        event_name = "idle_sweep_injected",
                        slot = %lead.display(),
                        fallback = !idle,
                        "injected the board sweep",
                    );
                    if recovered {
                        tracing::info!(
                            target: "forge_workspace::idle_sweep",
                            event_name = "idle_sweep_recovered",
                            slot = %lead.display(),
                            "the board sweep reached the lead again",
                        );
                    }
                }
                Delivery::NoSession(reason) => {
                    let first_failure = {
                        let mut clocks = self.idle_sweeps.lock();
                        match clocks.get_mut(&lead) {
                            Some(clock) => !std::mem::replace(&mut clock.failing, true),
                            None => true,
                        }
                    };
                    if first_failure {
                        tracing::warn!(
                            target: "forge_workspace::idle_sweep",
                            event_name = "idle_sweep_not_delivered",
                            slot = %lead.display(),
                            reason = %reason,
                            "the board sweep could not be delivered; retrying each tick",
                        );
                    }
                }
            }
        }
        delivered
    }

    /// Spawn the idle sweep: a background pass every
    /// [`IDLE_SWEEP_INTERVAL_SECS`] that injects the board sweep into a
    /// lead with nothing in flight. The registry makes a second call a
    /// no-op; started once at boot. Mirrors
    /// [`Workspace::start_chase_sweep`].
    pub fn start_idle_sweep(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("idle_sweep");
        self.supervisor.spawn("idle_sweep", forge_supervise::Policy::backoff(), move || {
            let weak = weak.clone();
            let span = span.clone();
            async move {
                let period = std::time::Duration::from_secs(IDLE_SWEEP_INTERVAL_SECS);
                let mut interval =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(workspace) = weak.upgrade() else {
                        return;
                    };
                    workspace.idle_sweep(
                        &PromptDeliver { workspace: Arc::clone(&workspace) },
                        SystemTime::now(),
                    );
                }
            }
            .instrument(span)
        });
    }
}

/// Whether `lead` is idle: it is the project's own agent, its session is
/// live, no turn is running, it owns no row in progress, and the project
/// has no live worker.
fn lead_is_idle(ws: &Workspace, lead: &SessionSlot) -> bool {
    if !lead.is_lead() || !ws.seat_is_live(lead) {
        return false;
    }
    if ws.domain_session_for(lead).is_some_and(|domain| domain.lock().turn_in_flight()) {
        return false;
    }
    let busy_row = ws
        .tasks_for_project(lead.project())
        .iter()
        .any(|task| task.status == TaskStatus::InProgress && task.owner.as_ref() == Some(lead));
    if busy_row {
        return false;
    }
    let Some(view) = ws.list_projects().into_iter().find(|view| view.name == lead.project()) else {
        return false;
    };
    // `is_live` is "in flight": a `Restarting` worker is not running and
    // holds no work for the lead to wait on, though it still spends a cap
    // slot (`holds_slot`). The sweep may inject beside one.
    !ws.list_live_workers(&view.key).iter().any(WorkerEntry::is_live)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use parking_lot::Mutex;
    use tempfile::tempdir;

    use super::*;
    use crate::SessionSlot;
    use crate::chase::{Deliver, Delivery};
    use forge_primitives::tasks::{Task, TaskId, TaskStatus};

    const PROJECT: &str = "proj";

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fixture(dir: &std::path::Path) -> Arc<Workspace> {
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.to_owned());
        ws.seed_test_project(PROJECT, "/tmp/tp-idle-sweep");
        let db = crate::store::Db::open(&dir.join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws
    }

    fn lead() -> SessionSlot {
        SessionSlot::lead("TestOrg", PROJECT)
    }

    /// A lead seat behind a live session. The receiver must be held for
    /// the stub to stay.
    fn live_lead(
        ws: &Workspace,
    ) -> tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand> {
        ws.install_testing_stub(&lead())
    }

    /// Drop the seat's session, leaving the slot as a sleeping one.
    fn sleep(ws: &Workspace) {
        ws.domain_handles.lock().remove(&lead());
    }

    fn task(id: &str, status: TaskStatus, owner: Option<SessionSlot>) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: PROJECT.to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status,
            owner,
            parent: None,
            waiting_on: None,
            estimate: None,
            rank: None,
            verify: None,
            attested: None,
            reserved: None,
            links: Vec::new(),
            attempt: 0,
            archived_at: None,
            created_at: epoch(0),
            updated_at: epoch(0),
        }
    }

    fn live_worker(ws: &Workspace, label: &str) {
        let key = ws
            .list_projects()
            .into_iter()
            .find(|view| view.name == PROJECT)
            .expect("view")
            .key
            .clone();
        ws.insert_live_worker(
            &key,
            crate::WorkerEntry {
                label: label.to_owned(),
                charter: "charter".to_owned(),
                slot: SessionSlot::worker("TestOrg", PROJECT, label),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: epoch(0),
                spawned_by: lead(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: None,
                diagnostic: None,
                kick: None,
            },
        );
    }

    #[derive(Default)]
    struct Recording {
        seen: Mutex<Vec<(SessionSlot, String)>>,
    }

    impl Deliver for Recording {
        fn to_seat(&self, slot: &SessionSlot, text: &str) -> Delivery {
            self.seen.lock().push((slot.clone(), text.to_owned()));
            Delivery::Delivered
        }
    }

    /// A delivery that fails until `open` is set, so a retry can be
    /// watched without a live dispatch.
    #[derive(Default)]
    struct Flaky {
        open: std::sync::atomic::AtomicBool,
        seen: Mutex<usize>,
    }

    impl Deliver for Flaky {
        fn to_seat(&self, _slot: &SessionSlot, _text: &str) -> Delivery {
            *self.seen.lock() += 1;
            if self.open.load(std::sync::atomic::Ordering::SeqCst) {
                Delivery::Delivered
            } else {
                Delivery::NoSession("the seat's channel closed".to_owned())
            }
        }
    }

    fn sweep(ws: &Arc<Workspace>, deliver: &dyn Deliver, at: u64) -> usize {
        ws.idle_sweep(deliver, epoch(at))
    }

    #[test]
    fn a_live_lead_with_nothing_in_flight_is_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);

        assert!(lead_is_idle(&ws, &lead()));
    }

    #[test]
    fn a_running_turn_is_not_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let domain = ws.domain_session_for(&lead()).expect("domain");
        domain.lock().commit_turn();

        assert!(!lead_is_idle(&ws, &lead()), "a lead with a turn running has something in flight");
    }

    #[test]
    fn a_row_in_progress_under_the_lead_is_not_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::InProgress, Some(lead())));

        assert!(
            !lead_is_idle(&ws, &lead()),
            "a lead holding an in-progress row has work in flight",
        );
    }

    #[test]
    fn a_row_in_progress_under_a_worker_leaves_the_lead_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task(
            "t-1",
            TaskStatus::InProgress,
            Some(SessionSlot::worker("TestOrg", PROJECT, "w-1")),
        ));

        assert!(lead_is_idle(&ws, &lead()), "a worker's row is the worker's work, not the lead's");
    }

    #[test]
    fn a_live_worker_in_the_project_is_not_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        live_worker(&ws, "w-1");

        assert!(!lead_is_idle(&ws, &lead()), "a project with a live worker has work in flight");
    }

    #[test]
    fn a_worker_seat_is_not_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let worker = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _rx = ws.install_testing_stub(&worker);

        assert!(
            !lead_is_idle(&ws, &worker),
            "the sweep is for the project's own agent, never a worker",
        );
    }

    #[test]
    fn a_sleeping_lead_is_skipped_and_never_woken() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 1_000), 0);
        assert!(deliver.seen.lock().is_empty(), "a sleeping lead is never woken");
    }

    #[test]
    fn a_lead_that_was_swept_and_then_slept_is_not_woken() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let deliver = Recording::default();
        assert_eq!(sweep(&ws, &deliver, 1_000), 1, "the first sweep lands");

        sleep(&ws);
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_FALLBACK_SECS + 60),
            0,
            "a sleeping lead is skipped even when its fallback is long overdue",
        );
        assert_eq!(deliver.seen.lock().len(), 1);
    }

    #[test]
    fn a_live_idle_lead_is_swept_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 1_000), 1);
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, lead(), "the prompt goes to the project's lead");
        assert_eq!(
            seen[0].1,
            "Nothing is in flight on this board. Sweep it now, and end with the next three picks \
             from the backlog.",
            "the idle path carries the plan's prompt verbatim",
        );
    }

    #[test]
    fn two_ticks_inside_the_gap_deliver_one_prompt() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 1_000), 1);
        assert_eq!(sweep(&ws, &deliver, 1_060), 0, "inside the two-hour gap");
        assert_eq!(sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_MIN_GAP_SECS), 1, "the gap is spent");
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_MIN_GAP_SECS + 60),
            0,
            "a delivery re-arms the gap from the delivery, not from the first sighting",
        );
        assert_eq!(deliver.seen.lock().len(), 2);
    }

    #[test]
    fn a_lead_that_slept_keeps_its_gap_across_the_restart() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let lead_rx = live_lead(&ws);
        let deliver = Recording::default();
        assert_eq!(sweep(&ws, &deliver, 1_000), 1);

        drop(lead_rx);
        sleep(&ws);
        assert_eq!(sweep(&ws, &deliver, 1_100), 0, "asleep, nothing is delivered");

        let _rx = live_lead(&ws);
        assert_eq!(
            sweep(&ws, &deliver, 1_200),
            0,
            "the wake does not reset the lead's two-hour gap",
        );
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_MIN_GAP_SECS),
            1,
            "the gap still runs from the delivery before the sleep",
        );
    }

    #[test]
    fn a_busy_lead_is_swept_after_six_hours() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let domain = ws.domain_session_for(&lead()).expect("domain");
        domain.lock().commit_turn();
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 1_000), 0, "a busy lead is not idle");
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_FALLBACK_SECS - 1),
            0,
            "the fallback is not due yet",
        );
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_FALLBACK_SECS),
            1,
            "the fallback lands even mid-turn",
        );
        {
            let seen = deliver.seen.lock();
            assert_eq!(seen.len(), 1);
            assert_eq!(
                seen[0].1,
                "Six hours have passed since the last sweep. Sweep the board when you can, and \
                 end with the next three picks from the backlog.",
                "the fallback does not claim the board is quiet",
            );
        }
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_FALLBACK_SECS + 60),
            0,
            "one fallback, not a loop",
        );
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + 2 * IDLE_SWEEP_FALLBACK_SECS),
            1,
            "the next six-hour boundary delivers again",
        );
    }

    #[test]
    fn a_lead_busy_on_its_own_row_is_swept_after_six_hours() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::InProgress, Some(lead())));
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 1_000), 0, "a lead on its own row is not idle");
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_FALLBACK_SECS),
            1,
            "the fallback covers a busy row as well as a busy turn",
        );
    }

    #[test]
    fn a_backwards_clock_does_not_sweep() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 2_000), 1);
        assert_eq!(
            sweep(&ws, &deliver, 1_900),
            0,
            "a clock that went backwards is unknown, not infinitely old",
        );
        assert_eq!(deliver.seen.lock().len(), 1);
    }

    #[test]
    fn a_failed_delivery_is_retried_on_the_next_tick() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let deliver = Flaky::default();

        assert_eq!(sweep(&ws, &deliver, 1_000), 0, "the failed attempt delivers nothing");
        assert_eq!(*deliver.seen.lock(), 1);
        assert_eq!(
            sweep(&ws, &deliver, 1_060),
            0,
            "the failing seat is retried, not stamped as swept",
        );
        deliver.open.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(sweep(&ws, &deliver, 1_120), 1, "the retry succeeds");
        assert_eq!(*deliver.seen.lock(), 3);
    }

    /// The acceptance end to end: the boot member ticks and the prompt
    /// reaches the lead's dispatch path.
    #[tokio::test(start_paused = true)]
    async fn the_boot_member_injects_the_sweep_on_its_tick() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.enable_test_dispatch_intercept();
        ws.start_idle_sweep();

        let names: Vec<String> =
            ws.supervisor().snapshot().into_iter().map(|member| member.name).collect();
        assert!(
            names.contains(&"idle_sweep".to_owned()),
            "the sweep is a supervised member: {names:?}"
        );

        // The member's interval is built on its first poll, which may land
        // after the first advance; two periods leave the first tick due
        // whichever instant the interval was armed at.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(IDLE_SWEEP_INTERVAL_SECS + 1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(IDLE_SWEEP_INTERVAL_SECS + 1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let wired = ws.drain_test_dispatch_buffer();
        assert!(
            wired.iter().any(|command| matches!(
                command,
                crate::protocol::Command::PromptUnder { text, .. } if text == IDLE_SWEEP_PROMPT
            )),
            "the tick injects the sweep: {wired:?}",
        );
    }
}
