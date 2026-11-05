//! Auto-heal for dead seats: the supervisor that brings a durable lead or
//! worker back when its subprocess closes while it should be up.

pub(crate) mod policy;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Instant, SystemTime};

use forge_primitives::{SessionId, SessionSlot};

use crate::Workspace;
use crate::protocol::Command;
use crate::workspace::DYNAMIC_WORKER_RESTART_NOTE;
use policy::{HealPolicy, SeatFacts};

/// How the supervisor brings a seat back. Production dispatches through
/// the workspace; a test records the call instead of spawning a child.
pub(crate) type Restart = Arc<dyn Fn(&SessionSlot) + Send + Sync>;

/// The supervisor's per-slot memory: deaths since the last stable run, when
/// the current occupant connected, and a token that makes a superseded
/// timer a no-op.
#[derive(Default)]
struct Seat {
    deaths: u32,
    connected_at: Option<Instant>,
    generation: u64,
}

/// The auto-heal supervisor. One per workspace, built lazily the first time
/// a death reaches it.
pub(crate) struct Healing {
    workspace: Weak<Workspace>,
    restart: Restart,
    seats: Mutex<HashMap<SessionSlot, Seat>>,
}

impl Healing {
    pub(crate) fn new(workspace: Weak<Workspace>, restart: Restart) -> Arc<Self> {
        Arc::new(Self { workspace, restart, seats: Mutex::new(HashMap::new()) })
    }

    /// Production wiring: the restart dispatches through the workspace.
    pub(crate) fn for_workspace(workspace: &Arc<Workspace>) -> Arc<Self> {
        let weak = Arc::downgrade(workspace);
        let restart_weak = Arc::downgrade(workspace);
        let restart: Restart = Arc::new(move |slot: &SessionSlot| {
            let Some(workspace) = restart_weak.upgrade() else { return };
            let slot = slot.clone();
            tokio::spawn(async move { restart_seat(&workspace, &slot).await });
        });
        Self::new(weak, restart)
    }

    /// A seat that had been up died and its connection failed. Walks the
    /// policy and either schedules the next attempt or leaves the seat
    /// failed with its diagnostic.
    pub(crate) fn note_death(
        self: &Arc<Self>,
        slot: &SessionSlot,
        _occupant: &SessionId,
        diagnostic: &str,
    ) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let Some(project_key) = workspace.project_key_for_name(slot.project()) else { return };
        let policy = HealPolicy::from_config(&workspace.config_arc().sessions);
        let facts = seat_facts(&workspace, slot, &project_key);
        if !policy.is_eligible(facts) {
            // A stop, a shutdown or an ad-hoc seat owns this seat now; a
            // stale attempt from an earlier death must not survive it.
            self.forget(slot);
            return;
        }
        let (deaths, generation) = {
            let mut seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let seat = seats.entry(slot.clone()).or_default();
            if seat.connected_at.is_some_and(|at| policy.resets_after(at.elapsed())) {
                seat.deaths = 0;
            }
            seat.deaths += 1;
            seat.connected_at = None;
            seat.generation += 1;
            (seat.deaths, seat.generation)
        };
        let Some(wait) = policy.next_after(deaths) else {
            // The budget is spent: the seat stays failed with the last
            // diagnostic, and this line is the record of the giving-up.
            workspace.clear_heal_progress(slot);
            tracing::warn!(
                target: "forge_workspace::healing",
                event_name = "seat_heal_exhausted",
                slot = %slot.display(),
                attempts = policy.attempts,
                reason = %diagnostic,
                "the seat died again after its restart budget; it is left failed",
            );
            return;
        };
        let next_at = SystemTime::now() + wait;
        workspace.set_heal_progress(slot, deaths, next_at);
        if !slot.is_lead() {
            workspace.mark_worker_restarting(&project_key, slot);
        }
        tracing::info!(
            target: "forge_workspace::healing",
            event_name = "seat_heal_scheduled",
            slot = %slot.display(),
            attempt = deaths,
            wait_ms = wait.as_millis(),
            reason = %diagnostic,
            "the seat died and forge is bringing it back",
        );
        let healing = Arc::clone(self);
        let slot = slot.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            healing.fire(&slot, generation);
        });
    }

    /// An occupant connected on `slot`: any scheduled attempt is cancelled,
    /// and the run's clock starts.
    pub(crate) fn note_connected(&self, slot: &SessionSlot) {
        let mut seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let seat = seats.entry(slot.clone()).or_default();
        seat.generation += 1;
        seat.connected_at = Some(Instant::now());
        drop(seats);
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.clear_heal_progress(slot);
        }
    }

    /// The seat was deliberately stopped (a close or a despawn). Its count,
    /// its pending attempt and its row mark all go with it.
    pub(crate) fn note_stopped(&self, slot: &SessionSlot) {
        let mut seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        seats.remove(slot);
        drop(seats);
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.clear_heal_progress(slot);
        }
    }

    /// Forget one seat's attempts without touching the row: used when a
    /// death turns out not to be heal-eligible.
    fn forget(&self, slot: &SessionSlot) {
        let mut seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        seats.remove(slot);
    }

    /// A scheduled attempt fired. Re-check that the seat is still the one
    /// that was scheduled, and still dead, then restart it.
    fn fire(&self, slot: &SessionSlot, generation: u64) {
        {
            let seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if seats.get(slot).is_none_or(|seat| seat.generation != generation) {
                return;
            }
        }
        let Some(workspace) = self.workspace.upgrade() else { return };
        // A reader opened the seat, a peer woke it, or a new worker took
        // the label: the race the re-check exists for.
        if workspace.session_is_live(slot) {
            self.forget(slot);
            workspace.clear_heal_progress(slot);
            return;
        }
        if !slot.is_lead() {
            let Some(project_key) = workspace.project_key_for_name(slot.project()) else {
                self.forget(slot);
                return;
            };
            let workers = workspace.list_live_workers(&project_key);
            if crate::mcp::workers::types::live_worker_with_label(&workers, slot.label()).is_some()
            {
                self.forget(slot);
                workspace.clear_heal_progress(slot);
                return;
            }
            // The row is the seat's durable handle: a close or despawn
            // deleted it, so there is nothing to bring back.
            if !matches!(workspace.stored_worker_row(&project_key, slot.label()), Ok(Some(_))) {
                self.forget(slot);
                workspace.clear_heal_progress(slot);
                return;
            }
            // The failed tombstone goes before the resume appends its own
            // entry, or the by-slot reads would move the tombstone instead.
            workspace.clear_failed_worker(&project_key, slot.label());
        }
        (self.restart)(slot);
    }
}

/// The facts eligibility is decided on, read from the workspace's current
/// state at each death, so a live config reload applies.
fn seat_facts(
    workspace: &Workspace,
    slot: &SessionSlot,
    project_key: &crate::ProjectKey,
) -> SeatFacts {
    let durable = if slot.is_lead() {
        true
    } else {
        matches!(workspace.stored_worker_row(project_key, slot.label()), Ok(Some(_)))
    };
    // A closed lead has already had its command sender taken by the close
    // path, while a dead one is still inside the session task that is
    // reporting its death.
    let stopped = workspace.despawn_cleanup_pending(project_key, slot.label())
        || (slot.is_lead() && !workspace.session_is_live(slot));
    SeatFacts {
        durable,
        // The supervisor is fed only by the death path, which fires after
        // `Connected`.
        connected_once: true,
        stopping: stopped,
        shutting_down: workspace.is_shutting_down(),
    }
}

/// The one restart call, for a lead or a worker.
async fn restart_seat(workspace: &Arc<Workspace>, slot: &SessionSlot) {
    if slot.is_lead() {
        // The same dispatch the cron wake uses: it resumes the lead under
        // its stored session and re-spawns its workers on connect.
        match workspace.dispatch(Command::SpawnProject {
            project_name: slot.project().to_owned(),
            launch_settings: forge_agent::client::SessionLaunchSettings::default(),
        }) {
            Ok(()) => {}
            Err(error) => tracing::warn!(
                target: "forge_workspace::healing",
                event_name = "seat_heal_lead_dispatch_failed",
                slot = %slot.display(),
                %error,
                "the heal could not dispatch the lead's wake; the next death will carry this",
            ),
        }
        return;
    }
    let Some(project_key) = workspace.project_key_for_name(slot.project()) else { return };
    let row = match workspace.stored_worker_row(&project_key, slot.label()) {
        Ok(Some(row)) => row,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::healing",
                event_name = "seat_heal_worker_row_unreadable",
                slot = %slot.display(),
                %error,
                "the worker's row could not be read; this attempt has nothing to resume",
            );
            return;
        }
    };
    let lead = SessionSlot::lead(slot.org(), slot.project());
    let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(workspace);
    let spawn = facade
        .spawn_worker(
            &lead,
            slot.label().to_owned(),
            row.charter.unwrap_or_default(),
            row.resume_kick.or_else(|| Some(DYNAMIC_WORKER_RESTART_NOTE.to_owned())),
            // The row's own resume kick stays the row's: this spawn only
            // delivers the live kick.
            None,
            row.interactive.unwrap_or(false),
            true,
            row.mcp_families,
            row.cwd,
        )
        .await;
    match spawn {
        Ok(_) => {}
        Err(error) => tracing::warn!(
            target: "forge_workspace::healing",
            event_name = "seat_heal_resume_failed",
            slot = %slot.display(),
            error = ?error,
            "the heal's resume was refused; the failure rides the next attempt",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use forge_primitives::{SessionId, SessionSlot};

    use super::*;

    /// Records what the supervisor would restart, so a test watches the
    /// decision without spawning a child.
    fn recorder() -> (Restart, Arc<Mutex<Vec<SessionSlot>>>) {
        let calls: Arc<Mutex<Vec<SessionSlot>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let restart: Restart =
            Arc::new(move |slot: &SessionSlot| seen.lock().expect("recorder").push(slot.clone()));
        (restart, calls)
    }

    /// A workspace with one configured project and a stored row for its
    /// worker `probe-heal`: the durable worker the supervisor may bring
    /// back.
    fn seeded_worker() -> (Arc<Workspace>, SessionSlot) {
        let dir = tempfile::tempdir().expect("tempdir");
        let (workspace, _rx) = Workspace::testing_stub();
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("the store");
        workspace.attach_store_for_test(db);
        workspace.seed_test_project("forge", "/tmp/forge-heal-test");
        let key = workspace
            .list_projects()
            .into_iter()
            .find(|view| view.name == "forge")
            .expect("the seeded project")
            .key;
        workspace.seed_test_worker_row(&key, "probe-heal");
        // The tempdir must outlive the store read below; the store holds
        // its own file handle, so dropping the guard here is safe.
        let slot = SessionSlot::worker("TestOrg", "forge", "probe-heal");
        (workspace, slot)
    }

    /// A death schedules one resume at the settled wait, and the row draws
    /// the attempt while it waits. Catches a supervisor that restarts
    /// immediately, or one that never records the state the row reads.
    #[tokio::test(start_paused = true)]
    async fn a_worker_death_schedules_one_resume() {
        let (workspace, slot) = seeded_worker();
        let (restart, calls) = recorder();
        let healing = Healing::new(Arc::downgrade(&workspace), restart);

        healing.note_death(
            &slot,
            &SessionId::new("dead-1"),
            "the claude subprocess closed its output stream",
        );

        let progress = workspace.heal_progress(&slot).expect("the row draws restarting");
        assert_eq!(progress.attempt, 1, "the first restart after the death");
        assert!(calls.lock().expect("recorder").is_empty(), "nothing fires before the wait");

        // The spawned timer arms its sleep on its first poll; advance
        // only after that, or it arms at the advanced instant instead.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            calls.lock().expect("recorder").as_slice(),
            std::slice::from_ref(&slot),
            "one resume, on the dead seat",
        );
    }

    /// A seat that connects before the timer fires cancels the attempt:
    /// a reader opening the seat, or a peer message waking it, got there
    /// first.
    #[tokio::test(start_paused = true)]
    async fn a_connected_seat_cancels_the_scheduled_attempt() {
        let (workspace, slot) = seeded_worker();
        let (restart, calls) = recorder();
        let healing = Healing::new(Arc::downgrade(&workspace), restart);

        healing.note_death(&slot, &SessionId::new("dead-1"), "the subprocess closed");
        // Arm the timer before the connect cancels it, or the test would
        // prove nothing about the cancel.
        tokio::task::yield_now().await;
        healing.note_connected(&slot);
        assert!(workspace.heal_progress(&slot).is_none(), "the row stops saying restarting");

        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(calls.lock().expect("recorder").is_empty(), "the cancelled attempt never fires");
    }

    /// Each death moves the attempt and its wait: +5 s, then +30 s. Catches
    /// a supervisor that counts deaths into the wrong slot or reuses the
    /// first interval forever.
    #[tokio::test(start_paused = true)]
    async fn the_attempt_and_its_wait_move_with_each_death() {
        let (workspace, slot) = seeded_worker();
        let (restart, calls) = recorder();
        let healing = Healing::new(Arc::downgrade(&workspace), restart);

        healing.note_death(&slot, &SessionId::new("dead-1"), "closed");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.lock().expect("recorder").len(), 1);

        healing.note_death(&slot, &SessionId::new("dead-2"), "closed again");
        let progress = workspace.heal_progress(&slot).expect("the second attempt is recorded");
        assert_eq!(progress.attempt, 2, "the second restart");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.lock().expect("recorder").len(), 1, "30 s has not passed yet");
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.lock().expect("recorder").len(), 2, "the second attempt fires at 30 s");
    }
}
