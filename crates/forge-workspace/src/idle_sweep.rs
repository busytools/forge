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
use crate::board::DEFAULT_STALE_SECS;
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

/// The one prompt an injected sweep delivers.
pub const IDLE_SWEEP_PROMPT: &str = "Nothing is in flight on this board. Sweep it now, and end \
     with the next three picks from the backlog.";

/// One lead's injected-sweep clock, held in memory so a restart re-arms
/// every lead.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SweepClock {
    /// When this workspace first saw the lead live, the baseline the
    /// fallback measures from before any sweep has landed.
    pub(crate) first_seen: SystemTime,
    /// When the last injected sweep was delivered.
    pub(crate) last_delivered: Option<SystemTime>,
}

impl Workspace {
    /// One pass over every project: deliver the injected sweep to each
    /// lead that is idle, or overdue for one. Returns how many prompts
    /// reached a session.
    pub fn idle_sweep(&self, deliver: &dyn Deliver, now: SystemTime) -> usize {
        let mut delivered = 0;
        for view in self.list_projects() {
            let lead = SessionSlot::lead(&view.org, &view.name);
            // Never wake a sleeping session: its next start sweeps, as
            // the charter says.
            if !self.seat_is_live(&lead) {
                self.idle_sweeps.lock().remove(&lead);
                continue;
            }
            let idle = lead_is_idle(self, &lead, now);
            let due = {
                let mut clocks = self.idle_sweeps.lock();
                let clock = clocks
                    .entry(lead.clone())
                    .or_insert(SweepClock { first_seen: now, last_delivered: None });
                let baseline = clock.last_delivered.unwrap_or(clock.first_seen);
                let age = now.duration_since(baseline).map_or(u64::MAX, |d| d.as_secs());
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
            if deliver.to_seat(&lead, IDLE_SWEEP_PROMPT) == Delivery::Delivered {
                let mut clocks = self.idle_sweeps.lock();
                if let Some(clock) = clocks.get_mut(&lead) {
                    clock.last_delivered = Some(now);
                }
                delivered += 1;
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
                    let delivered = workspace.idle_sweep(
                        &PromptDeliver { workspace: Arc::clone(&workspace) },
                        SystemTime::now(),
                    );
                    if delivered > 0 {
                        tracing::debug!(
                            target: "forge_workspace::idle_sweep",
                            event_name = "idle_sweep_injected",
                            delivered,
                            "injected the board sweep into idle leads",
                        );
                    }
                }
            }
            .instrument(span)
        });
    }
}

/// Whether `lead` is idle: it is the project's own agent, its session is
/// live, no turn is running, it owns no row in progress, and the project
/// has no live worker.
fn lead_is_idle(ws: &Workspace, lead: &SessionSlot, now: SystemTime) -> bool {
    if !lead.is_lead() || !ws.seat_is_live(lead) {
        return false;
    }
    if ws.domain_session_for(lead).is_some_and(|domain| domain.lock().turn_in_flight()) {
        return false;
    }
    let busy_row = ws.board_rows(lead.project(), now, DEFAULT_STALE_SECS).iter().any(|row| {
        row.task.status == TaskStatus::InProgress && row.task.owner.as_ref() == Some(lead)
    });
    if busy_row {
        return false;
    }
    let Some(view) = ws.list_projects().into_iter().find(|view| view.name == lead.project()) else {
        return false;
    };
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

    fn sweep(ws: &Arc<Workspace>, deliver: &dyn Deliver, at: u64) -> usize {
        ws.idle_sweep(deliver, epoch(at))
    }

    #[test]
    fn a_live_lead_with_nothing_in_flight_is_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);

        assert!(lead_is_idle(&ws, &lead(), epoch(1_000)));
    }

    #[test]
    fn a_running_turn_is_not_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let domain = ws.domain_session_for(&lead()).expect("domain");
        domain.lock().commit_turn();

        assert!(
            !lead_is_idle(&ws, &lead(), epoch(1_000)),
            "a lead with a turn running has something in flight",
        );
    }

    #[test]
    fn a_row_in_progress_under_the_lead_is_not_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::InProgress, Some(lead())));

        assert!(
            !lead_is_idle(&ws, &lead(), epoch(1_000)),
            "a lead holding an in-progress row has work in flight",
        );
    }

    #[test]
    fn a_live_worker_in_the_project_is_not_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        live_worker(&ws, "w-1");

        assert!(
            !lead_is_idle(&ws, &lead(), epoch(1_000)),
            "a project with a live worker has work in flight",
        );
    }

    #[test]
    fn a_worker_seat_is_not_idle() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let worker = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _rx = ws.install_testing_stub(&worker);

        assert!(
            !lead_is_idle(&ws, &worker, epoch(1_000)),
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
    fn a_live_idle_lead_is_swept_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 1_000), 1);
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, lead(), "the prompt goes to the project's lead");
        assert!(seen[0].1.contains("Sweep it now"), "the text names the sweep: {}", seen[0].1);
        assert!(
            seen[0].1.contains("next three picks from the backlog"),
            "the text names the next-pick rule: {}",
            seen[0].1,
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
        assert_eq!(deliver.seen.lock().len(), 2);
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
        assert_eq!(
            sweep(&ws, &deliver, 1_000 + IDLE_SWEEP_FALLBACK_SECS + 60),
            0,
            "one fallback, not a loop",
        );
    }
}
