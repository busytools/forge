//! Auto-heal for dead seats: the supervisor that brings a durable lead or
//! worker back when its subprocess closes while it should be up.

pub(crate) mod policy;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Instant, SystemTime};

use forge_primitives::SessionSlot;

use crate::Workspace;
use crate::crons::IST_OFFSET_MINUTES;
use crate::mcp::peers::types::{MessageId, WrappedKind, WrappedPrompt, seat_name};
use crate::protocol::Command;
use crate::workspace::DYNAMIC_WORKER_RESTART_NOTE;
use policy::{HealPolicy, SeatFacts};

/// How the supervisor brings a seat back. Production dispatches through
/// the workspace; a test records the call instead of spawning a child.
#[async_trait::async_trait]
pub(crate) trait SeatRestart: Send + Sync {
    /// Bring `slot` back. `Ok` carries the session it came back on when
    /// one is known; `Err` is the refusal's reason, which the lead's
    /// notice says instead of a restart.
    async fn restart(&self, slot: &SessionSlot) -> Result<Option<String>, String>;
}

/// How the supervisor tells a project's lead about a worker. Production
/// dispatches a peer message; a test records the prose.
pub(crate) type Notify = Arc<dyn Fn(&SessionSlot, &str) + Send + Sync>;

/// The supervisor's per-slot memory: deaths since the last stable run, when
/// the current occupant connected, and a token that makes a superseded
/// timer a no-op.
#[derive(Default)]
struct Seat {
    deaths: u32,
    connected_at: Option<Instant>,
    generation: u64,
    /// Set by a close or a despawn before the seat's death can reach the
    /// supervisor. The next death consumes it and is not healed: the stop
    /// owns the seat, whatever its subprocess did on the way out.
    stopped: bool,
}

/// The auto-heal supervisor. One per workspace, built lazily the first time
/// a death reaches it.
pub(crate) struct Healing {
    workspace: Weak<Workspace>,
    restart: Arc<dyn SeatRestart>,
    notify: Notify,
    seats: Mutex<HashMap<SessionSlot, Seat>>,
}

impl Healing {
    pub(crate) fn new(
        workspace: Weak<Workspace>,
        restart: Arc<dyn SeatRestart>,
        notify: Notify,
    ) -> Arc<Self> {
        Arc::new(Self { workspace, restart, notify, seats: Mutex::new(HashMap::new()) })
    }

    /// Production wiring: the restart dispatches through the workspace,
    /// and a worker's notice is a peer message to its project's lead.
    pub(crate) fn for_workspace(workspace: &Arc<Workspace>) -> Arc<Self> {
        let weak = Arc::downgrade(workspace);
        let restart = Arc::new(WorkspaceRestart { workspace: Arc::downgrade(workspace) });
        let notify_weak = Arc::downgrade(workspace);
        let notify: Notify = Arc::new(move |slot: &SessionSlot, body: &str| {
            let Some(workspace) = notify_weak.upgrade() else { return };
            if let Err(error) = workspace.dispatch(lead_notice(slot, body.to_owned())) {
                tracing::warn!(
                    target: "forge_workspace::healing",
                    event_name = "seat_heal_notice_failed",
                    slot = %slot.display(),
                    %error,
                    "the heal's notice to the lead could not be dispatched",
                );
            }
        });
        Self::new(weak, restart, notify)
    }

    /// A seat that had been up died and its connection failed. Walks the
    /// policy and either schedules the next attempt or leaves the seat
    /// failed with its diagnostic.
    pub(crate) fn note_death(self: &Arc<Self>, slot: &SessionSlot, diagnostic: &str) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let Some(project_key) = workspace.project_key_for_name(slot.project()) else { return };
        // **The stop the seat's own door announced goes first.** A close or
        // a despawn snaps the subprocess, whose death then arrives here; it
        // is the stop's death, not a crash to heal.
        let stopped = {
            let mut seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut seats.entry(slot.clone()).or_default().stopped)
        };
        if stopped || workspace.despawn_cleanup_pending(&project_key, slot.label()) {
            self.forget(slot);
            workspace.clear_heal_progress(slot);
            return;
        }
        let policy = HealPolicy::from_config(&workspace.config_arc().sessions);
        let facts = seat_facts(&workspace, slot, &project_key);
        if !policy.is_eligible(facts) {
            // A shutdown or an ad-hoc seat owns this seat now; a stale
            // attempt from an earlier death must not survive it.
            self.forget(slot);
            return;
        }
        self.schedule(&workspace, slot, &project_key, diagnostic, SystemTime::now());
    }

    /// Count one death and either schedule the next attempt or leave the
    /// seat failed and escalate. A subprocess death and an attempt whose
    /// resume was refused both walk this path, so both spend the same
    /// budget and carry their own diagnostic.
    fn schedule(
        self: &Arc<Self>,
        workspace: &Arc<Workspace>,
        slot: &SessionSlot,
        project_key: &crate::ProjectKey,
        diagnostic: &str,
        died_at: SystemTime,
    ) {
        let policy = HealPolicy::from_config(&workspace.config_arc().sessions);
        let (deaths, generation) = {
            let mut seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let seat = seats.entry(slot.clone()).or_default();
            if seat.connected_at.is_some_and(|at| policy.resets_after(at.elapsed())) {
                seat.deaths = 0;
            }
            seat.deaths += 1;
            seat.connected_at = None;
            seat.stopped = false;
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
            if !slot.is_lead() {
                (self.notify)(slot, &escalation_notice(slot, died_at, diagnostic, policy.attempts));
            }
            return;
        };
        let next_at = SystemTime::now() + wait;
        workspace.set_heal_progress(slot, deaths, next_at);
        if !slot.is_lead() {
            workspace.mark_worker_restarting(project_key, slot);
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
        let diagnostic = diagnostic.to_owned();
        let attempts = policy.attempts;
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            healing.fire(&slot, generation, died_at, &diagnostic, deaths, attempts).await;
        });
    }

    /// An occupant connected on `slot`: any scheduled attempt is cancelled,
    /// and the run's clock starts.
    pub(crate) fn note_connected(&self, slot: &SessionSlot) {
        let mut seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let seat = seats.entry(slot.clone()).or_default();
        seat.generation += 1;
        seat.connected_at = Some(Instant::now());
        seat.stopped = false;
        drop(seats);
        if let Some(workspace) = self.workspace.upgrade() {
            workspace.clear_heal_progress(slot);
        }
    }

    /// The seat was deliberately stopped (a close or a despawn). Its count
    /// and its pending attempt go, and the stop is remembered so the
    /// subprocess death the stop causes is not mistaken for a crash.
    pub(crate) fn note_stopped(&self, slot: &SessionSlot) {
        let mut seats = self.seats.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let seat = seats.entry(slot.clone()).or_default();
        seat.generation += 1;
        seat.deaths = 0;
        seat.connected_at = None;
        seat.stopped = true;
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
    /// that was scheduled, and still dead, then restart it and tell the
    /// lead what happened.
    async fn fire(
        self: Arc<Self>,
        slot: &SessionSlot,
        generation: u64,
        died_at: SystemTime,
        diagnostic: &str,
        attempt: u32,
        attempts: usize,
    ) {
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
                workspace.clear_heal_progress(slot);
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
        let outcome = self.restart.restart(slot).await;
        if !slot.is_lead() {
            (self.notify)(
                slot,
                &attempt_notice(slot, died_at, diagnostic, attempt, attempts, &outcome),
            );
        }
        if let Err(reason) = &outcome {
            // **A refused resume is a failed attempt, not a quiet stop.**
            // The budget keeps burning, and the reason rides the next
            // attempt and the escalation exactly as a death's does.
            if let Some(project_key) = workspace.project_key_for_name(slot.project()) {
                self.schedule(&workspace, slot, &project_key, reason, SystemTime::now());
            }
        }
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
    SeatFacts {
        durable,
        // The supervisor is fed only by the death path, which fires after
        // `Connected`.
        connected_once: true,
        // The stop flag and the despawn cleanup are checked by `note_death`
        // before this is built; what is left here is a non-issue.
        stopping: false,
        shutting_down: workspace.is_shutting_down(),
    }
}

/// The production restart: the workspace's own spawn paths, which are the
/// ones the boot re-spawn and the cron wake already use.
struct WorkspaceRestart {
    workspace: Weak<Workspace>,
}

#[async_trait::async_trait]
impl SeatRestart for WorkspaceRestart {
    async fn restart(&self, slot: &SessionSlot) -> Result<Option<String>, String> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Err("the workspace is gone".to_owned());
        };
        if slot.is_lead() {
            // The same handler `Command::SpawnProject` runs (the cron wake's
            // own), called directly because the command bus drops the
            // refusal and a failed attempt must count. It resumes the lead
            // under its stored session and re-spawns its workers on
            // connect.
            crate::spawn::handle_spawn_project(
                &workspace,
                slot.project(),
                forge_agent::client::SessionLaunchSettings::default(),
            )
            .map(|()| None)
        } else {
            restart_worker(&workspace, slot).await
        }
    }
}

/// Resume one worker through the spawn its boot re-spawn uses: that path
/// ensures the worktree it needs, so a missing one is re-created rather
/// than refused.
async fn restart_worker(
    workspace: &Arc<Workspace>,
    slot: &SessionSlot,
) -> Result<Option<String>, String> {
    let project_key = workspace
        .project_key_for_name(slot.project())
        .ok_or_else(|| format!("no configured project named '{}'", slot.project()))?;
    let row = match workspace.stored_worker_row(&project_key, slot.label()) {
        Ok(Some(row)) => row,
        Ok(None) => return Err("its stored row is gone".to_owned()),
        Err(error) => return Err(format!("its stored row could not be read: {error}")),
    };
    let lead = SessionSlot::lead(slot.org(), slot.project());
    let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(workspace);
    facade
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
        .await
        .map(|reply| Some(reply.session_id))
        .map_err(|error| error.message())
}

/// The notice one worker death earns: the instant, the diagnostic, the
/// attempt and what it did. The seat is named the way an envelope names
/// it, so the lead can address it back.
fn attempt_notice(
    worker: &SessionSlot,
    died_at: SystemTime,
    diagnostic: &str,
    attempt: u32,
    attempts: usize,
    outcome: &Result<Option<String>, String>,
) -> String {
    let seat = seat_name(worker);
    let time = clock_text(died_at).unwrap_or_else(|| "an unrecorded time".to_owned());
    let outcome = match outcome {
        Ok(Some(session)) => {
            format!("Restarted, attempt {attempt} of {attempts}, session {session}.")
        }
        Ok(None) => format!("Restarted, attempt {attempt} of {attempts}."),
        Err(reason) => format!("Restart failed, attempt {attempt} of {attempts}: {reason}."),
    };
    format!("{seat} died at {time}: {diagnostic}. {outcome}")
}

/// The notice a spent budget earns: the restarts are over and the seat
/// stays down with the diagnostic that ended it.
fn escalation_notice(
    worker: &SessionSlot,
    died_at: SystemTime,
    diagnostic: &str,
    attempts: usize,
) -> String {
    let seat = seat_name(worker);
    let time = clock_text(died_at).unwrap_or_else(|| "an unrecorded time".to_owned());
    format!(
        "{seat} died again at {time}: {diagnostic}. Left Failed after {attempts} restarts; it \
         stays down until it is started again."
    )
}

/// The instant as the notice spells it: IST first and UTC in parentheses,
/// so the reader needs no arithmetic on either clock.
fn clock_text(at: SystemTime) -> Option<String> {
    let since_epoch = at.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    let utc = chrono::DateTime::from_timestamp(
        i64::try_from(since_epoch.as_secs()).ok()?,
        since_epoch.subsec_nanos(),
    )?;
    let ist = utc.checked_add_signed(chrono::TimeDelta::minutes(IST_OFFSET_MINUTES))?;
    Some(format!("{} IST ({}Z)", ist.format("%H:%M:%S"), utc.format("%H:%M:%S")))
}

/// The command that carries a worker's notice to its project's lead: the
/// same envelope path `agents__send_message` uses, so a delivery to a
/// sleeping lead parks and lands on wake.
fn lead_notice(worker: &SessionSlot, body: String) -> Command {
    Command::DeliverPeerPrompt {
        caller: worker.clone(),
        target_project: worker.project().to_owned(),
        wrapped: WrappedPrompt {
            id: MessageId::mint(),
            kind: WrappedKind::Message,
            sender_name: seat_name(worker),
            sender_org: worker.org().to_owned(),
            body,
        },
        respond: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use forge_primitives::SessionSlot;

    use super::*;

    type Calls = Arc<Mutex<Vec<SessionSlot>>>;
    type Notices = Arc<Mutex<Vec<(SessionSlot, String)>>>;

    /// Records each restart, so a test watches the decision without
    /// spawning a child, and answers every attempt with `outcome`.
    struct RecordingRestart {
        calls: Calls,
        outcome: Result<Option<String>, String>,
    }

    #[async_trait::async_trait]
    impl SeatRestart for RecordingRestart {
        async fn restart(&self, slot: &SessionSlot) -> Result<Option<String>, String> {
            self.calls.lock().expect("recorder").push(slot.clone());
            self.outcome.clone()
        }
    }

    fn recorder(outcome: Result<Option<String>, String>) -> (Arc<dyn SeatRestart>, Calls) {
        let calls: Calls = Arc::new(Mutex::new(Vec::new()));
        let restart = Arc::new(RecordingRestart { calls: Arc::clone(&calls), outcome });
        (restart, calls)
    }

    /// Records the notice the supervisor would send a project's lead.
    fn notifier() -> (Notify, Notices) {
        let notices: Notices = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&notices);
        let notify: Notify = Arc::new(move |slot: &SessionSlot, body: &str| {
            seen.lock().expect("notices").push((slot.clone(), body.to_owned()));
        });
        (notify, notices)
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
        let (restart, calls) = recorder(Ok(None));
        let (notify, _notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);

        healing.note_death(&slot, "the claude subprocess closed its output stream");

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
        let (restart, calls) = recorder(Ok(None));
        let (notify, _notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);

        healing.note_death(&slot, "the subprocess closed");
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
        let (restart, calls) = recorder(Ok(None));
        let (notify, _notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);

        healing.note_death(&slot, "closed");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.lock().expect("recorder").len(), 1);

        healing.note_death(&slot, "closed again");
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

    /// Every worker death reaches the project's lead once, with the time
    /// in IST and UTC, the diagnostic, the attempt and what the attempt
    /// did - and it goes out even when attempt 1 heals the seat. Catches a
    /// supervisor silent on success, or one that tells the lead before the
    /// attempt has an outcome.
    #[tokio::test(start_paused = true)]
    async fn a_worker_death_tells_the_lead_what_happened() {
        let (workspace, slot) = seeded_worker();
        let (restart, _calls) = recorder(Ok(Some("session-healed".to_owned())));
        let (notify, notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);

        healing.note_death(&slot, "the claude subprocess closed its output stream");
        assert!(notices.lock().expect("notices").is_empty(), "the outcome is not in yet");

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let notices = notices.lock().expect("notices");
        let (from, body) = notices.first().expect("the lead hears about the death");
        assert_eq!(from, &slot, "the dying worker is the sender the envelope names");
        assert!(body.contains("died at"), "the time the seat died: {body}");
        assert!(body.contains(" IST ("), "IST first, UTC in parentheses: {body}");
        assert!(body.contains('Z'), "the UTC half is spelled: {body}");
        assert!(
            body.contains("the claude subprocess closed its output stream"),
            "the diagnostic: {body}",
        );
        assert!(body.contains("attempt 1 of 3"), "the attempt: {body}");
        assert!(body.contains("session-healed"), "the outcome: {body}");
        assert_eq!(notices.len(), 1, "one message per death");
    }

    /// A spent budget escalates once, naming the restarts and the
    /// diagnostic that ended the run. Catches an escalation that reads as
    /// one more attempt, or a notice per refused attempt instead.
    #[tokio::test(start_paused = true)]
    async fn a_spent_budget_escalates_once() {
        let (workspace, slot) = seeded_worker();
        // Each restart works and the child dies again straight after it:
        // the crash loop, where the deaths - not the attempts - spend the
        // budget.
        let (restart, calls) = recorder(Ok(Some("session-healed".to_owned())));
        let (notify, notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);

        for attempt in 1..=3 {
            healing.note_death(&slot, "the subprocess closed");
            tokio::task::yield_now().await;
            let wait = match attempt {
                1 => 5,
                2 => 30,
                _ => 120,
            };
            tokio::time::advance(Duration::from_secs(wait)).await;
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.lock().expect("recorder").len(), 3, "three restarts were attempted");

        healing.note_death(&slot, "the subprocess closed again");
        let notices = notices.lock().expect("notices");
        let (_, body) = notices.last().expect("the escalation reaches the lead");
        assert!(
            body.contains("Left Failed after 3 restarts"),
            "the seat is done for this run: {body}",
        );
        assert!(body.contains("the subprocess closed again"), "the last diagnostic: {body}");
        assert!(!body.contains("attempt 4"), "the escalation is not a fourth attempt: {body}");
    }

    /// **A death that follows a stop is the stop's.** A close or a despawn
    /// kills the child itself, and the death that arrives afterwards must
    /// not schedule anything - the stop already owns the seat.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_seat_is_not_healed() {
        let (workspace, slot) = seeded_worker();
        let (restart, calls) = recorder(Ok(None));
        let (notify, notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);

        healing.note_stopped(&slot);
        healing.note_death(&slot, "the subprocess closed");

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(calls.lock().expect("recorder").is_empty(), "the stop owns the seat");
        assert!(notices.lock().expect("notices").is_empty(), "a stop's death is not news");
        assert!(workspace.heal_progress(&slot).is_none(), "the row does not say restarting");
    }

    /// A seat with no durable row is not forge's to bring back: the death
    /// schedules nothing at all, before any timer could re-check.
    #[tokio::test(start_paused = true)]
    async fn an_ad_hoc_death_schedules_nothing() {
        let (workspace, _slot) = seeded_worker();
        let (restart, calls) = recorder(Ok(None));
        let (notify, notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);
        let ad_hoc = SessionSlot::worker("TestOrg", "forge", "no-row");

        healing.note_death(&ad_hoc, "the subprocess closed");

        assert!(workspace.heal_progress(&ad_hoc).is_none(), "nothing is scheduled");
        assert!(notices.lock().expect("notices").is_empty(), "and nobody is told");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert!(calls.lock().expect("recorder").is_empty(), "no restart was ever attempted");
    }

    /// **A refused resume burns an attempt.** The seat does not stop at the
    /// first refusal: the next attempt is scheduled carrying the refusal's
    /// reason, and the budget still ends in the escalation. This is the
    /// repository-that-is-gone case.
    #[tokio::test(start_paused = true)]
    async fn a_refused_resume_burns_an_attempt_and_keeps_going() {
        let (workspace, slot) = seeded_worker();
        let (restart, calls) = recorder(Err("the worktree could not be re-created".to_owned()));
        let (notify, notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);

        healing.note_death(&slot, "the subprocess closed");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(calls.lock().expect("recorder").len(), 1, "the first attempt was made");
        let progress = workspace.heal_progress(&slot).expect("the second attempt is scheduled");
        assert_eq!(progress.attempt, 2, "the refusal counted as a death");

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert_eq!(calls.lock().expect("recorder").len(), 3, "all three attempts were made");
        let notices = notices.lock().expect("notices");
        assert!(
            notices.iter().any(|(_, body)| body.contains("Left Failed after 3 restarts")),
            "the budget still escalates: {notices:?}",
        );
        assert!(
            notices.iter().any(|(_, body)| body.contains("the worktree could not be re-created")),
            "the refusal's own reason rides a notice: {notices:?}",
        );
    }

    /// A lead has no lead above it: its death is drawn on the project row
    /// and is nobody's peer message.
    #[tokio::test(start_paused = true)]
    async fn a_lead_death_tells_no_one() {
        let (workspace, _worker) = seeded_worker();
        let (restart, calls) = recorder(Ok(None));
        let (notify, notices) = notifier();
        let healing = Healing::new(Arc::downgrade(&workspace), restart, notify);
        let lead = SessionSlot::lead("TestOrg", "forge");

        healing.note_death(&lead, "the subprocess closed");
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;

        assert_eq!(calls.lock().expect("recorder").len(), 1, "the lead still restarts");
        assert!(notices.lock().expect("notices").is_empty(), "no peer message for a lead");
    }

    /// The notice is a peer message to the project, sent as the worker, so
    /// the lead reads it in its own chat and can answer it back.
    #[test]
    fn the_notice_is_a_peer_message_to_the_projects_lead() {
        let worker = SessionSlot::worker("TestOrg", "forge", "probe-heal");
        let command = lead_notice(&worker, "the body".to_owned());
        let Command::DeliverPeerPrompt { caller, target_project, wrapped, .. } = command else {
            panic!("a worker notice must travel the peer path");
        };
        assert_eq!(caller, worker, "the dying worker is the sender");
        assert_eq!(target_project, "forge", "the project's own agent is the target");
        assert_eq!(wrapped.kind, WrappedKind::Message);
        assert_eq!(wrapped.sender_name, "forge/probe-heal");
        assert_eq!(wrapped.sender_org, "TestOrg");
        assert_eq!(wrapped.body, "the body");
    }
}
