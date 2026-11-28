//! The drive: the pass that keeps a stopped lead moving.
//!
//! It fires when a project's lead has stopped without cause - no turn is
//! running - and the board still owes something it alone can do:
//!
//! - a row it owns sits in progress (it should be working it, or hand it
//!   on);
//! - the project has ready, unowned work (dispatch is the lead's job);
//! - a row waits on a row the lead owns (the blocker is the lead's);
//! - a live worker holds no row (feed it or despawn it).
//!
//! The nudge carries those facts and one ask: continue, hand off, or say
//! why not. It is never sent for a project whose only claim on the lead is
//! a wait on the user - an ask and a look are the user's to give, and no
//! agent chases them.
//!
//! Caps: one outstanding nudge per project - the same claim is never
//! re-sent, and a CHANGED claim waits out [`DRIVE_MIN_GAP_SECS`] so a
//! flapping board cannot become a stream of nudges. The nudge clears
//! when the claim set empties - the lead acted, or the board moved - and a
//! fresh claim starts its own clock. A sleeping lead is never woken: its
//! next session start reads the board, and the clock is dropped so that
//! start is fresh.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use forge_primitives::tasks::{DefaultIfIgnored, TaskStatus, WaitingKind};
use tracing::Instrument;

use crate::SessionSlot;
use crate::chase::{Deliver, Delivery, PromptDeliver, fmt_secs};
use crate::workspace::Workspace;

/// How often the drive looks. A lead that stops is nudged within this.
pub const DRIVE_INTERVAL_SECS: u64 = 60;

/// How close together two DIFFERENT claims may be nudged in one project.
pub const DRIVE_MIN_GAP_SECS: u64 = 30 * 60;

/// How many subjects of each kind a nudge names before it counts the
/// rest. The message is read by a model deciding what to do next, so it
/// stays one screen.
const DRIVE_NAMES: usize = 3;

/// How long an acknowledgement may stand unanswered before the lead is
/// told the person marked it seen and forgot it, and before the board
/// draws the card as forgotten.
pub const FORGOTTEN_ACK_SECS: u64 = 4 * 3_600;

/// The period a `reask` default re-arms when the ask's own wait does not
/// carry one.
const REASK_PERIOD_SECS: u64 = 30 * 60;

/// One lead's drive clock, held in memory so a restart re-arms every lead.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DriveClock {
    /// When the last nudge landed. `None` means nothing is outstanding and
    /// the next claim may speak at once.
    pub(crate) last_nudge: Option<SystemTime>,
    /// A digest of the claims the outstanding nudge named. While it is
    /// unchanged, the nudge is already outstanding and says nothing new.
    pub(crate) last_claim: Option<u64>,
    /// Whether the last delivery attempt failed, so a failure episode is
    /// logged once rather than on every retry tick.
    pub(crate) failing: bool,
}

/// A digest of one nudge's claim set, so the drive can tell a changed
/// board from the one it already spoke about.
fn claim_stamp(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// How long `now` is after `since`, and zero when the clock went
/// backwards: one NTP step must not re-nudge every project.
fn age_secs(since: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(since).map_or(0, |d| d.as_secs())
}

/// What the board owes a stopped lead, as the nudge states it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Claims {
    owned_running: Vec<String>,
    ready: Vec<String>,
    /// `(waiting row, the lead's row it waits on)`.
    blocked_on_lead: Vec<(String, String)>,
    stopped_workers: Vec<String>,
}

impl Claims {
    fn is_empty(&self) -> bool {
        self.owned_running.is_empty()
            && self.ready.is_empty()
            && self.blocked_on_lead.is_empty()
            && self.stopped_workers.is_empty()
    }

    /// The nudge: the board's facts over [`DRIVE_NAMES`] named each, one
    /// ask, and the ending the lead owes.
    fn nudge(&self, project: &str) -> String {
        let named = |subjects: &[String]| {
            let shown: Vec<String> =
                subjects.iter().take(DRIVE_NAMES).map(|subject| format!("\"{subject}\"")).collect();
            let rest = subjects.len().saturating_sub(DRIVE_NAMES);
            if rest == 0 {
                shown.join(", ")
            } else {
                format!("{} and {rest} more", shown.join(", "))
            }
        };
        let mut facts = Vec::new();
        if !self.owned_running.is_empty() {
            facts.push(format!(
                "{} row{} you hold in progress ({})",
                self.owned_running.len(),
                if self.owned_running.len() == 1 { "" } else { "s" },
                named(&self.owned_running),
            ));
        }
        if !self.ready.is_empty() {
            facts.push(format!("{} ready and unowned ({})", self.ready.len(), named(&self.ready)));
        }
        if !self.blocked_on_lead.is_empty() {
            let pairs: Vec<String> = self
                .blocked_on_lead
                .iter()
                .take(DRIVE_NAMES)
                .map(|(waiting, blocker)| format!("\"{waiting}\" on \"{blocker}\""))
                .collect();
            let rest = self.blocked_on_lead.len().saturating_sub(DRIVE_NAMES);
            let more = if rest == 0 { String::new() } else { format!(" and {rest} more") };
            facts.push(format!(
                "{} row(s) waiting on yours ({}{more})",
                self.blocked_on_lead.len(),
                pairs.join(", ")
            ));
        }
        if !self.stopped_workers.is_empty() {
            let labels: Vec<String> =
                self.stopped_workers.iter().take(DRIVE_NAMES).cloned().collect();
            let rest = self.stopped_workers.len().saturating_sub(DRIVE_NAMES);
            let more = if rest == 0 { String::new() } else { format!(" and {rest} more") };
            facts.push(format!(
                "{} live worker{} holding no row ({}{more})",
                self.stopped_workers.len(),
                if self.stopped_workers.len() == 1 { "" } else { "s" },
                labels.join(", "),
            ));
        }
        format!(
            "task board: \"{project}\" is stopped with work on the board. Facts: {}. One ask: \
             continue one of these now, hand it to a worker, or say why the project should stay \
             stopped.",
            facts.join("; "),
        )
    }
}

/// `at` truncated to whole seconds, the precision the acknowledgement store
/// keeps, so a persisted stamp compares equal to the row it was taken from.
fn whole_secs(at: SystemTime) -> SystemTime {
    SystemTime::UNIX_EPOCH
        + at.duration_since(SystemTime::UNIX_EPOCH)
            .map_or(Duration::ZERO, |since| Duration::from_secs(since.as_secs()))
}

impl Workspace {
    /// What the board owes `lead` right now. A wait on the user is not in
    /// this set: a decision or a look is the user's to give, and a project
    /// whose only claim is one is never driven.
    fn claims_on(&self, lead: &SessionSlot) -> Claims {
        let tasks = self.tasks_for_project(lead.project());
        let mut claims = Claims::default();
        for task in &tasks {
            if task.owner.as_ref() == Some(lead) && task.status == TaskStatus::InProgress {
                claims.owned_running.push(task.subject.clone());
            }
            if task.is_ready() && task.owner.is_none() {
                claims.ready.push(task.subject.clone());
            }
            if task.status == TaskStatus::Waiting
                && let Some(blocker_id) = task.waiting_on.as_ref().and_then(|wait| wait.on.as_ref())
                && let Some(blocker) = tasks.iter().find(|blocker| blocker.id == *blocker_id)
                && blocker.owner.as_ref() == Some(lead)
                && !blocker.status.is_terminal()
            {
                claims.blocked_on_lead.push((task.subject.clone(), blocker.subject.clone()));
            }
        }
        claims.stopped_workers = self.unaccounted(lead.project());
        claims
    }

    /// **The item lifecycle, on the same pass, in this order**: the deadline
    /// first - it is the item's own contract, so a snoozed or
    /// policy-settled ask still runs its default; then snooze quieting; then
    /// the policy note, because a settled class is already answered; then
    /// the acknowledged-but-forgotten state, named to the lead once.
    /// Returns how many messages reached a session.
    fn run_ask_lifecycle(
        &self,
        deliver: &dyn Deliver,
        view: &crate::views::ProjectView,
        now: SystemTime,
    ) -> usize {
        let lead = SessionSlot::lead(&view.org, &view.name);
        let mut delivered = 0;
        for task in self.tasks_for_project(&view.name) {
            if task.status != TaskStatus::Waiting {
                continue;
            }
            let Some(wait) = task.waiting_on.as_ref() else { continue };
            if wait.kind != Some(WaitingKind::User) || wait.parked {
                continue;
            }
            // **The deadline runs before every quieting rule**: a deferral
            // or a policy note is about attention, not about time, and an
            // ask that passed its deadline still runs its named default -
            // escalate, expire or reask, never approve.
            if let Some(due) = wait.due.as_ref()
                && due.ran_at.is_none()
                && due.at <= now
            {
                match due.default {
                    DefaultIfIgnored::Expire => {
                        let reason = "expired: no answer within its deadline, and the default was \
                                      expire - the ask and its record stay on the row";
                        self.expire_ask(&view.name, &task.id, now, reason);
                    }
                    DefaultIfIgnored::Escalate => {
                        let text = format!(
                            "task board: \"{}\" went unanswered past its deadline; the default is \
                             escalate, so it stays waiting for the user. Raise it again or withdraw \
                             it with the reason.",
                            task.subject,
                        );
                        // **Only a delivered default is spent.** A lead with
                        // no session leaves the row due, so the next pass
                        // tries again rather than dropping the escalation.
                        if deliver.to_seat(&lead, &text) == Delivery::Delivered {
                            delivered += 1;
                            self.mark_ask_default_ran(&view.name, &task.id, now);
                        }
                    }
                    DefaultIfIgnored::Reask => {
                        let text = format!(
                            "task board: \"{}\" went unanswered past its deadline; the default is \
                             reask, so ask the user again.",
                            task.subject,
                        );
                        if deliver.to_seat(&lead, &text) == Delivery::Delivered {
                            delivered += 1;
                            let period = wait
                                .since
                                .and_then(|since| due.at.duration_since(since).ok())
                                .map_or(REASK_PERIOD_SECS, |period| period.as_secs())
                                .max(60);
                            self.rearm_ask(&view.name, &task.id, now + Duration::from_secs(period));
                        }
                    }
                }
                continue;
            }
            // A snoozed ask stays quiet: no forgotten nudge while it is
            // deferred.
            if crate::tasks::snooze_active(wait, task.updated_at, now) {
                continue;
            }
            // A settled class is already answered: the note is the answer,
            // so no escalation is made.
            let settled = task
                .request
                .as_ref()
                .and_then(|request| request.class.as_deref())
                .is_some_and(|class| self.policy_settles(&view.name, class));
            if settled {
                continue;
            }
            // The acknowledged-but-forgotten state, named once per
            // acknowledgement, with the named one persisted beside the row.
            // **Whole seconds on both sides**: the store keeps the stamp to
            // the second, so the row's sub-second part is truncated before
            // the compare and the stamp - otherwise they never match and the
            // nudge fires on every pass.
            if let Some(acknowledged) = wait.acknowledged_at.map(whole_secs)
                && age_secs(acknowledged, now) >= FORGOTTEN_ACK_SECS
                && self.ack_named(&view.name, &task.id) != Some(acknowledged)
            {
                let text = format!(
                    "task board: \"{}\" was acknowledged {} ago and is still unanswered; \
                     answer it, withdraw it with a reason, or leave it and say why.",
                    task.subject,
                    fmt_secs(age_secs(acknowledged, now)),
                );
                // A lead with no session leaves the row unnamed, so the next
                // pass says it rather than losing it.
                if deliver.to_seat(&lead, &text) == Delivery::Delivered {
                    delivered += 1;
                    self.mark_ack_named(&view.name, &task.id, acknowledged);
                }
            }
        }
        delivered
    }

    /// The acknowledgement the drive has already named for this row, from
    /// the store: a restart must not lose it and re-name the forgetting.
    fn ack_named(&self, project: &str, id: &forge_primitives::tasks::TaskId) -> Option<SystemTime> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        match crate::store::ack_named::named(db, project, id.as_str()) {
            Ok(named) => named,
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::drive",
                    event_name = "ack_named_read_failed",
                    project = %project,
                    id = %id.as_str(),
                    %error,
                    "reading the named forgotten acknowledgement failed",
                );
                None
            }
        }
    }

    /// Record that the drive has named this row's acknowledgement.
    fn mark_ack_named(
        &self,
        project: &str,
        id: &forge_primitives::tasks::TaskId,
        acknowledged: SystemTime,
    ) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        if let Err(error) = crate::store::ack_named::mark(db, project, id.as_str(), acknowledged) {
            tracing::warn!(
                target: "forge_workspace::drive",
                event_name = "ack_named_write_failed",
                project = %project,
                id = %id.as_str(),
                %error,
                "recording the named forgotten acknowledgement failed",
            );
        }
    }

    /// One pass over every project: nudge each lead that has stopped with
    /// a claim on it. Returns how many nudges reached a session.
    pub fn drive(&self, deliver: &dyn Deliver, now: SystemTime) -> usize {
        let views = self.list_projects();
        // A project removed from the config takes its lead's clock with it.
        self.drive_clocks.lock().retain(|slot, _| {
            views.iter().any(|view| view.org == slot.org() && view.name == slot.project())
        });
        let mut delivered = 0;
        for view in views {
            let lead = SessionSlot::lead(&view.org, &view.name);
            // The asks' own clocks run whatever the lead is doing: a
            // deadline default is the item's contract, not the lead's.
            delivered += self.run_ask_lifecycle(deliver, &view, now);
            // Never wake a sleeping session: its next start reads the
            // board. Dropping the clock means that start is not met by an
            // old gap.
            if !self.seat_is_live(&lead) {
                self.drive_clocks.lock().remove(&lead);
                continue;
            }
            // A turn in flight is the lead working, not stopping.
            if self.domain_session_for(&lead).is_some_and(|domain| domain.lock().turn_in_flight()) {
                continue;
            }
            let claims = self.claims_on(&lead);
            if claims.is_empty() {
                // Nothing claims the lead: the nudge is answered or moot,
                // and a fresh claim starts its own clock.
                self.drive_clocks.lock().remove(&lead);
                continue;
            }
            let text = claims.nudge(&view.name);
            let stamp = claim_stamp(&text);
            let due = {
                let mut clocks = self.drive_clocks.lock();
                let clock = clocks.entry(lead.clone()).or_default();
                match clock.last_nudge {
                    // Nothing outstanding: the next claim may speak at once.
                    None => true,
                    // **One outstanding nudge per claim.** The same claim
                    // is already on the lead's record; repeating it every
                    // half hour says nothing new. Only a changed claim
                    // speaks again, and not inside the gap.
                    Some(at) => {
                        clock.last_claim != Some(stamp) && age_secs(at, now) >= DRIVE_MIN_GAP_SECS
                    }
                }
            };
            if !due {
                continue;
            }
            match deliver.to_seat(&lead, &text) {
                Delivery::Delivered => {
                    let recovered = {
                        let mut clocks = self.drive_clocks.lock();
                        clocks.get_mut(&lead).is_some_and(|clock| {
                            clock.last_nudge = Some(now);
                            clock.last_claim = Some(stamp);
                            std::mem::replace(&mut clock.failing, false)
                        })
                    };
                    delivered += 1;
                    tracing::debug!(
                        target: "forge_workspace::drive",
                        event_name = "drive_nudged",
                        slot = %lead.display(),
                        owned_running = claims.owned_running.len(),
                        ready = claims.ready.len(),
                        blocked_on_lead = claims.blocked_on_lead.len(),
                        stopped_workers = claims.stopped_workers.len(),
                        "nudged a stopped lead",
                    );
                    if recovered {
                        tracing::info!(
                            target: "forge_workspace::drive",
                            event_name = "drive_recovered",
                            slot = %lead.display(),
                            "the drive reached the lead again",
                        );
                    }
                }
                Delivery::NoSession(reason) => {
                    let first_failure = {
                        let mut clocks = self.drive_clocks.lock();
                        match clocks.get_mut(&lead) {
                            Some(clock) => !std::mem::replace(&mut clock.failing, true),
                            None => true,
                        }
                    };
                    if first_failure {
                        tracing::warn!(
                            target: "forge_workspace::drive",
                            event_name = "drive_not_delivered",
                            slot = %lead.display(),
                            reason = %reason,
                            "the drive's nudge could not be delivered; retrying each tick",
                        );
                    }
                }
            }
        }
        delivered
    }

    /// Spawn the drive: a background pass every [`DRIVE_INTERVAL_SECS`]
    /// that nudges a lead which has stopped with work on the board. The
    /// registry makes a second call a no-op; started once at boot.
    pub fn start_drive(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("drive");
        self.supervisor.spawn("drive", forge_supervise::Policy::backoff(), move || {
            let weak = weak.clone();
            let span = span.clone();
            async move {
                let period = std::time::Duration::from_secs(DRIVE_INTERVAL_SECS);
                let mut interval =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(workspace) = weak.upgrade() else {
                        return;
                    };
                    workspace.drive(
                        &PromptDeliver { workspace: Arc::clone(&workspace) },
                        SystemTime::now(),
                    );
                }
            }
            .instrument(span)
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use parking_lot::Mutex;
    use tempfile::tempdir;

    use super::*;
    use crate::chase::{Deliver, Delivery};
    use forge_primitives::tasks::{Task, TaskId, TaskStatus, Waiting, WaitingKind};

    const PROJECT: &str = "proj";

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fixture(dir: &std::path::Path) -> Arc<Workspace> {
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.to_owned());
        ws.seed_test_project(PROJECT, "/tmp/tp-drive");
        let db = crate::store::Db::open(&dir.join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws
    }

    fn lead() -> SessionSlot {
        SessionSlot::lead("TestOrg", PROJECT)
    }

    /// `project`'s lead seat behind a live session. The receiver must be
    /// held for the stub to stay.
    fn live_lead_for(
        ws: &Workspace,
        project: &str,
    ) -> tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand> {
        ws.install_testing_stub(&SessionSlot::lead("TestOrg", project))
    }

    fn live_lead(
        ws: &Workspace,
    ) -> tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand> {
        live_lead_for(ws, PROJECT)
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
            kind: forge_primitives::tasks::TaskKind::Finite,
            owner,
            parent: None,
            waiting_on: None,
            request: None,
            estimate: None,
            cadence: None,
            rank: None,
            verify: None,
            attested: None,
            reserved: None,
            links: Vec::new(),
            evidence: None,
            version: 0,
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

    fn pass(ws: &Arc<Workspace>, deliver: &dyn Deliver, at: u64) -> usize {
        ws.drive(deliver, epoch(at))
    }

    /// One captured tracing record: its level, its target and its fields.
    #[derive(Clone, Default)]
    struct Caught(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String, String)>>>);

    #[derive(Default)]
    struct CaughtFields(String);

    impl tracing::field::Visit for CaughtFields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            let _ = std::fmt::write(&mut self.0, format_args!("{}={value:?}", field.name()));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Caught {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = CaughtFields::default();
            event.record(&mut fields);
            self.0.lock().expect("capture").push((
                *event.metadata().level(),
                event.metadata().target().to_owned(),
                fields.0,
            ));
        }
    }

    fn warnings(caught: &Caught) -> Vec<(tracing::Level, String, String)> {
        caught
            .0
            .lock()
            .expect("capture")
            .iter()
            .filter(|(level, target, _)| {
                *level == tracing::Level::WARN && target == "forge_workspace::drive"
            })
            .cloned()
            .collect()
    }

    fn recoveries(caught: &Caught) -> Vec<(tracing::Level, String, String)> {
        caught
            .0
            .lock()
            .expect("capture")
            .iter()
            .filter(|(level, target, fields)| {
                *level == tracing::Level::INFO
                    && target == "forge_workspace::drive"
                    && fields.contains("drive_recovered")
            })
            .cloned()
            .collect()
    }

    /// A stopped lead holding a row in progress is the first cause: the
    /// nudge names the row and asks it to continue, hand off, or say why.
    #[test]
    fn a_stopped_lead_holding_a_row_is_nudged() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::InProgress, Some(lead())));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 1);
        let seen = deliver.seen.lock();
        assert_eq!(seen[0].0, lead(), "the nudge goes to the project's lead");
        let text = &seen[0].1;
        assert!(text.contains("1 row you hold in progress (\"subject t-1\")"), "{text}");
        assert!(text.contains("One ask:"), "the nudge carries one ask: {text}");
        assert!(text.contains("say why"), "including the reason door: {text}");
    }

    /// A ready unowned row is the second cause: dispatch is the lead's.
    #[test]
    fn a_stopped_lead_with_ready_work_is_nudged() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::Pending, None));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 1);
        let text = &deliver.seen.lock()[0].1;
        assert!(text.contains("1 ready and unowned (\"subject t-1\")"), "{text}");
    }

    /// A row waiting on the lead's own row is the third: the blocker is the
    /// lead's to end.
    #[test]
    fn a_stopped_lead_blocking_a_row_is_nudged() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-blocker", TaskStatus::InProgress, Some(lead())));
        let mut waiter = task("t-waiter", TaskStatus::Waiting, None);
        waiter.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Dependency),
            detail: None,
            on: Some(TaskId::from("t-blocker")),
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        });
        ws.seed_test_task(waiter);
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 1);
        let text = &deliver.seen.lock()[0].1;
        assert!(text.contains("waiting on yours"), "{text}");
        assert!(text.contains("\"subject t-waiter\" on \"subject t-blocker\""), "{text}");
    }

    /// A live worker holding nothing is the fourth: feed it or despawn it.
    #[test]
    fn a_stopped_lead_with_an_idle_worker_is_nudged() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        live_worker(&ws, "w-1");
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 1);
        let text = &deliver.seen.lock()[0].1;
        assert!(text.contains("1 live worker holding no row (w-1)"), "{text}");
    }

    /// **A wait on the user is not a cause.** A project whose only row is
    /// a decision left for Ved is never driven - the agent does not chase
    /// his answer - and a look is the same.
    #[test]
    fn a_wait_on_the_user_never_drives_the_lead() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        let mut decision = task("t-1", TaskStatus::Waiting, Some(lead()));
        decision.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: Some("which shape".to_owned()),
            on: None,
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        });
        ws.seed_test_task(decision);
        let mut look = task("t-2", TaskStatus::Waiting, Some(lead()));
        look.verify = Some(forge_primitives::tasks::Verify::User);
        look.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: None,
            on: None,
            verification: true,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        });
        ws.seed_test_task(look);
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 0);
        assert!(deliver.seen.lock().is_empty(), "no nudge for a wait on the user");
    }

    #[test]
    fn a_running_turn_is_not_a_stop() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::InProgress, Some(lead())));
        ws.domain_session_for(&lead()).expect("domain").lock().commit_turn();
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 0, "a lead mid-turn has not stopped");
    }

    /// One outstanding nudge per project: the SAME claim is never re-sent,
    /// however long it holds; a changed claim is the one that speaks again,
    /// and not inside the gap.
    #[test]
    fn one_outstanding_nudge_per_claim() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::Pending, None));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 1);
        assert_eq!(pass(&ws, &deliver, 1_060), 0, "inside the gap");
        assert_eq!(
            pass(&ws, &deliver, 1_000 + DRIVE_MIN_GAP_SECS),
            0,
            "the same claim is already outstanding; the gap does not re-open it",
        );

        // A changed board is a new fact, and the gap still bounds it.
        ws.seed_test_task(task("t-2", TaskStatus::Pending, None));
        assert_eq!(
            pass(&ws, &deliver, 1_000 + DRIVE_MIN_GAP_SECS + 60),
            1,
            "a changed claim speaks once the gap is spent",
        );
        assert_eq!(
            pass(&ws, &deliver, 4_000 + DRIVE_MIN_GAP_SECS),
            0,
            "and then it holds like any other claim",
        );
        assert_eq!(deliver.seen.lock().len(), 2);
    }

    /// The claim clearing drops the clock: a fresh claim is nudged at once
    /// rather than waiting out the old gap.
    #[test]
    fn a_cleared_claim_starts_a_fresh_clock() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::Pending, None));
        let deliver = Recording::default();
        assert_eq!(pass(&ws, &deliver, 1_000), 1);

        // The lead took the row and ended it: nothing claims the lead any more.
        ws.update_task(PROJECT, &TaskId::from("t-1"), forge_primitives::tasks::By::User, |task| {
            task.status = TaskStatus::Canceled;
            task.detail = Some("moot".to_owned());
        })
        .expect("no refusal");
        assert_eq!(pass(&ws, &deliver, 1_030), 0, "nothing claims the lead; the clock clears");

        ws.seed_test_task(task("t-2", TaskStatus::Pending, None));
        assert_eq!(pass(&ws, &deliver, 1_060), 1, "a fresh claim speaks at once");
    }

    #[test]
    fn a_sleeping_lead_is_never_woken() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(task("t-1", TaskStatus::Pending, None));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 0);
        assert!(deliver.seen.lock().is_empty(), "a sleeping lead is never woken");
    }

    #[test]
    fn a_lead_that_slept_mid_claim_is_skipped() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::Pending, None));
        let deliver = Recording::default();
        assert_eq!(pass(&ws, &deliver, 1_000), 1);

        sleep(&ws);
        assert_eq!(pass(&ws, &deliver, 1_000 + DRIVE_MIN_GAP_SECS + 60), 0);
        assert_eq!(deliver.seen.lock().len(), 1);
    }

    /// A failed delivery is not a nudge: the clock stays open, so the next
    /// tick tries again, and the failure episode logs once.
    #[test]
    fn a_failed_delivery_retries_and_logs_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(task("t-1", TaskStatus::Pending, None));
        let deliver = Flaky::default();
        let caught = Caught::default();
        let subscriber = tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::Registry::default(),
            caught.clone(),
        );

        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(pass(&ws, &deliver, 1_000), 0, "nothing landed");
            assert_eq!(pass(&ws, &deliver, 1_060), 0, "and it retries each tick");
        });
        assert_eq!(*deliver.seen.lock(), 2, "two attempts");
        assert_eq!(warnings(&caught).len(), 1, "one warning for the episode");

        let landed = Caught::default();
        let subscriber = tracing_subscriber::layer::SubscriberExt::with(
            tracing_subscriber::Registry::default(),
            landed.clone(),
        );
        deliver.open.store(true, std::sync::atomic::Ordering::SeqCst);
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(pass(&ws, &deliver, 1_120), 1, "the retry lands");
        });
        assert_eq!(
            recoveries(&landed).len(),
            1,
            "the recovery is noted once: {:?}",
            recoveries(&landed),
        );
    }

    /// A waiting ask on the person, with the item fields the lifecycle
    /// reads.
    fn ask(
        id: &str,
        kind: forge_primitives::tasks::AskKind,
        due: Option<forge_primitives::tasks::Deadline>,
        acknowledged_at: Option<SystemTime>,
        snoozed: Option<forge_primitives::tasks::Snooze>,
    ) -> Task {
        let mut task = task(id, TaskStatus::Waiting, Some(lead()));
        task.request = Some(forge_primitives::tasks::Request {
            kind,
            question: format!("question {id}"),
            options: Vec::new(),
            class: None,
            answer: None,
        });
        task.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: None,
            on: None,
            verification: false,
            parked: false,
            since: Some(epoch(0)),
            due,
            acknowledged_at,
            snoozed,
            expired_at: None,
        });
        task
    }

    fn deadline(at: u64, default: DefaultIfIgnored) -> forge_primitives::tasks::Deadline {
        forge_primitives::tasks::Deadline { at: epoch(at), default, ran_at: None }
    }

    /// **The expire default ends the row and keeps its record** - the
    /// person's silence is not approval, and the ask stays readable.
    #[test]
    fn an_expired_ask_ends_canceled_with_its_record() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(ask(
            "t-1",
            forge_primitives::tasks::AskKind::Clarification,
            Some(deadline(500, DefaultIfIgnored::Expire)),
            None,
            None,
        ));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 0, "expiry speaks to no one");
        let rows = ws.tasks_for_project(PROJECT);
        assert_eq!(rows[0].status, TaskStatus::Canceled, "the row ended canceled");
        let wait = rows[0].waiting_on.as_ref().expect("the wait's record is kept");
        assert!(wait.expired_at.is_some(), "and says when it expired");
        assert!(rows[0].request.is_some(), "the ask itself is kept");
    }

    /// **The escalate default names the ask once**, leaves it waiting, and
    /// does not repeat on the next pass.
    #[test]
    fn an_escalated_ask_is_named_once_and_stays_waiting() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(ask(
            "t-1",
            forge_primitives::tasks::AskKind::Decision,
            Some(deadline(500, DefaultIfIgnored::Escalate)),
            None,
            None,
        ));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 1, "the lead hears once");
        let seen = deliver.seen.lock();
        assert_eq!(seen[0].0, lead());
        assert!(seen[0].1.contains("went unanswered"), "{}", seen[0].1);
        drop(seen);
        assert_eq!(
            ws.tasks_for_project(PROJECT)[0].status,
            TaskStatus::Waiting,
            "escalate keeps the ask waiting",
        );
        assert_eq!(pass(&ws, &deliver, 1_060), 0, "and never repeats");
    }

    /// **The reask default re-arms the deadline** one period out, so the
    /// ask keeps its own clock rather than expiring by accident.
    #[test]
    fn a_reask_default_rearms_the_deadline() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(ask(
            "t-1",
            forge_primitives::tasks::AskKind::Clarification,
            Some(deadline(500, DefaultIfIgnored::Reask)),
            None,
            None,
        ));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 1_000), 1, "the lead is told to ask again");
        let rows = ws.tasks_for_project(PROJECT);
        assert_eq!(rows[0].status, TaskStatus::Waiting, "the ask stays");
        let due = rows[0].waiting_on.as_ref().and_then(|wait| wait.due).expect("a deadline");
        assert!(due.at > epoch(1_000), "re-armed ahead of now");
        assert_eq!(due.ran_at, None, "and armed again");
    }

    /// **A snoozed ask stays quiet, and its deadline still runs**: a
    /// deferral is about attention, not about time, so the named default
    /// lands on schedule rather than waiting for the person's snooze.
    #[test]
    fn a_snoozed_ask_stays_quiet_and_its_deadline_still_runs() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(ask(
            "t-1",
            forge_primitives::tasks::AskKind::Decision,
            Some(deadline(500, DefaultIfIgnored::Expire)),
            Some(epoch(100)),
            Some(forge_primitives::tasks::Snooze {
                until: forge_primitives::tasks::SnoozeUntil::At(epoch(9_999)),
                since: epoch(0),
            }),
        ));
        let deliver = Recording::default();

        assert_eq!(
            pass(&ws, &deliver, 1_000),
            0,
            "a deferral is quiet, and an expiry speaks to no one",
        );
        assert!(deliver.seen.lock().is_empty(), "no forgotten nudge while the deferral holds");
        assert_eq!(
            ws.tasks_for_project(PROJECT)[0].status,
            TaskStatus::Canceled,
            "the deadline ran ahead of the deferral",
        );
    }

    /// **An acknowledged ask standing unanswered is named once**: seen is
    /// not answered, and a forgotten acknowledgement is the person's own
    /// gap made visible to the lead.
    #[test]
    fn a_forgotten_acknowledgement_is_named_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(ask(
            "t-1",
            forge_primitives::tasks::AskKind::Decision,
            None,
            Some(epoch(0)),
            None,
        ));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 4 * 3_600 + 1), 1, "the lead hears once");
        let seen = deliver.seen.lock();
        assert!(seen[0].1.contains("acknowledged"), "{}", seen[0].1);
        drop(seen);
        assert_eq!(pass(&ws, &deliver, 4 * 3_600 + 60), 0, "and not every pass");
    }

    /// **A sub-second acknowledgement is still named once.** The store keeps
    /// the stamp to whole seconds, so the row's `acknowledged_at` is
    /// truncated before it is compared - otherwise the stored and live
    /// values never match and the nudge repeats every pass.
    #[test]
    fn a_sub_second_acknowledgement_is_named_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _rx = live_lead(&ws);
        ws.seed_test_task(ask(
            "t-1",
            forge_primitives::tasks::AskKind::Decision,
            None,
            Some(epoch(0) + Duration::from_millis(500)),
            None,
        ));
        let deliver = Recording::default();

        assert_eq!(pass(&ws, &deliver, 4 * 3_600 + 1), 1, "the lead hears once");
        assert_eq!(pass(&ws, &deliver, 4 * 3_600 + 60), 0, "and not every pass");
    }

    /// A deliver that finds no session behind the lead, watching what it was
    /// asked to send.
    #[derive(Default)]
    struct Offline {
        attempts: Mutex<Vec<(SessionSlot, String)>>,
    }

    impl Deliver for Offline {
        fn to_seat(&self, slot: &SessionSlot, text: &str) -> Delivery {
            self.attempts.lock().push((slot.clone(), text.to_owned()));
            Delivery::NoSession("no session behind the lead".to_owned())
        }
    }

    /// **A default that reached no session is not spent**: the lead hears
    /// the escalation on the next pass, when it has one, rather than the
    /// deadline being marked run against an empty room.
    #[test]
    fn an_escalation_with_no_live_session_is_not_spent() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(ask(
            "t-1",
            forge_primitives::tasks::AskKind::Decision,
            Some(deadline(500, DefaultIfIgnored::Escalate)),
            None,
            None,
        ));
        let offline = Offline::default();
        assert_eq!(pass(&ws, &offline, 1_000), 0, "nothing was delivered");
        let due = ws.tasks_for_project(PROJECT)[0]
            .waiting_on
            .as_ref()
            .and_then(|wait| wait.due)
            .expect("a deadline");
        assert!(due.ran_at.is_none(), "the escalation is still owed");

        let _rx = live_lead(&ws);
        let deliver = Recording::default();
        assert_eq!(pass(&ws, &deliver, 1_060), 1, "the next pass says it");
    }

    /// A reask default is re-armed only once its message reached the lead: an
    /// offline pass leaves the deadline due, so the reask is not moved a
    /// period into the future against nobody.
    #[test]
    fn a_reask_with_no_live_session_is_not_rearmed() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(ask(
            "t-1",
            forge_primitives::tasks::AskKind::Clarification,
            Some(deadline(500, DefaultIfIgnored::Reask)),
            None,
            None,
        ));
        let offline = Offline::default();
        assert_eq!(pass(&ws, &offline, 1_000), 0, "nothing was delivered");
        let due = ws.tasks_for_project(PROJECT)[0]
            .waiting_on
            .as_ref()
            .and_then(|wait| wait.due)
            .expect("a deadline");
        assert_eq!(due.at, epoch(500), "the deadline is where it was");
        assert!(due.ran_at.is_none(), "and still owed");

        let _rx = live_lead(&ws);
        let deliver = Recording::default();
        assert_eq!(pass(&ws, &deliver, 1_060), 1, "the next pass says it");
        let due = ws.tasks_for_project(PROJECT)[0]
            .waiting_on
            .as_ref()
            .and_then(|wait| wait.due)
            .expect("a deadline");
        assert!(due.at > epoch(1_060), "and re-armed ahead of now");
    }
}
