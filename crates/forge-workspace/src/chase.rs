//! The chase sweep: the pass that makes the board drive instead of
//! report.
//!
//! Every pass derives crossings from the board's own marks and the chase
//! log: a row past its estimate asks its owner once per estimate, a lead
//! hears at the rung's class cadence while its state holds, and a row
//! that died, moved and died again is a new occurrence, named at once.
//! The cadences are the settled two: five minutes for a state burning a
//! resource (a drifting worker, a row at 1.5x its estimate), thirty for
//! one parked and stuck (a dead row, a stalled queue, a stuck wait, a
//! retro or a close owed). **The chase log is the whole slate**: every
//! naming is deduped by it, so a restart re-reads it rather than
//! re-firing, and a state that clears has its naming forgotten - off the
//! log, where a fresh process reads - so a recurrence is named at once,
//! whatever happened in between.
//!
//! Delivery degrades, never wakes: a live owner is asked; with a dead
//! owner the lead is; with every seat asleep the mark alone stands - except
//! the user-wait nudge, whose one shot per row is kept unspent until a live
//! lead takes it. A chase NEVER spawns a session - the board already shows
//! the state, and waking a project is a cost the chase does not get to
//! spend.

use std::sync::Arc;
use std::time::SystemTime;

use forge_primitives::tasks::{
    By, ChaseRung, TaskChase, TaskId, TaskStatus, TaskTransition, WaitingKind,
};
use tracing::Instrument;

use crate::SessionSlot;
use crate::board::BoardRow;
use crate::tasks::parked_on_user;
use crate::workspace::Workspace;

/// How long between sweeps. The same number bounds how stale a
/// time-derived mark can be in a held client snapshot.
pub const CHASE_INTERVAL_SECS: u64 = 30;

/// How long after a wait resolves its message is still worth sending.
const WAIT_RESOLVED_WINDOW_SECS: u64 = 600;

/// The cadence of a rung whose state is burning a resource: a drifting
/// worker, a row past 1.5x its estimate.
const CADENCE_BURNING_SECS: u64 = 5 * 60;

/// The cadence of a rung whose state is parked and stuck: a dead row, a
/// stalled queue, a stuck wait, a retro or a close owed.
const CADENCE_STUCK_SECS: u64 = 30 * 60;

/// What a delivery attempt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Delivered,
    NoSession,
}

/// Where a chase message goes. A trait so tests can watch the ladder
/// without a live session; the production impl injects a prompt.
pub trait Deliver: Send + Sync {
    fn to_seat(&self, slot: &SessionSlot, text: &str) -> Delivery;
}

/// One message the sweep decided to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crossing {
    pub rung: ChaseRung,
    pub task_id: Option<TaskId>,
    /// The rung's own subject where a row id does not identify it: the
    /// worker a seat-level rung names, or the estimate a nudge fired
    /// against.
    pub subject: Option<String>,
    /// The seat the message prefers: the row's owner for a nudge, the
    /// lead for everything else. The ladder falls back to the lead.
    pub seat: Option<SessionSlot>,
    pub text: String,
}

/// The log's most recent naming of `label` by the seat-level rung.
fn durable_last_named(fired: &[TaskChase], label: &str) -> Option<SystemTime> {
    fired
        .iter()
        .filter(|chase| {
            chase.rung == ChaseRung::Unaccounted && chase.subject.as_deref() == Some(label)
        })
        .map(|chase| chase.at)
        .max()
}

/// How long `now` is after `at`, saturating: an `at` in the future reads
/// as the longest age, so a skewed stamp never silences a rung.
fn since_secs(at: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(at).map_or(u64::MAX, |d| d.as_secs())
}

/// Whether `rung` already fired for `id` and this subject - a restated
/// estimate changes the subject, which arms the nudge again.
fn fired_already(fired: &[TaskChase], rung: ChaseRung, id: &TaskId, subject: Option<&str>) -> bool {
    fired.iter().any(|chase| {
        chase.rung == rung
            && chase.task_id.as_ref() == Some(id)
            && chase.subject.as_deref() == subject
    })
}

/// Whether `rung` already fired for `id` at or after `at` - the shape a
/// rung recurring with a new occurrence needs.
fn fired_since(fired: &[TaskChase], rung: ChaseRung, id: &TaskId, at: SystemTime) -> bool {
    fired
        .iter()
        .any(|chase| chase.rung == rung && chase.task_id.as_ref() == Some(id) && chase.at >= at)
}

/// Whether a recurring rung has waited out its cadence: it is due when
/// nothing fired since `since` (the instant its state appeared, where
/// the history says), or when the newest firing since then is at least
/// `cadence` seconds old.
fn due(
    fired: &[TaskChase],
    rung: ChaseRung,
    id: &TaskId,
    since: Option<SystemTime>,
    cadence: u64,
    now: SystemTime,
) -> bool {
    let last = fired
        .iter()
        .filter(|chase| {
            chase.rung == rung
                && chase.task_id.as_ref() == Some(id)
                && since.is_none_or(|at| chase.at >= at)
        })
        .map(|chase| chase.at)
        .max();
    last.is_none_or(|at| since_secs(at, now) >= cadence)
}

/// When a row last died, if the history says: a row that moved and died
/// again is a new occurrence.
fn failed_at(history: &[TaskTransition], id: &TaskId) -> Option<SystemTime> {
    history
        .iter()
        .filter(|transition| transition.task_id == *id && transition.to == TaskStatus::Failed)
        .map(|transition| transition.at)
        .max()
}

/// When the row entered waiting, if the history says: the newest transition
/// into waiting is the episode a user nudge is deduped on.
fn waiting_since(history: &[TaskTransition], id: &TaskId) -> Option<SystemTime> {
    history
        .iter()
        .filter(|transition| transition.task_id == *id && transition.to == TaskStatus::Waiting)
        .map(|transition| transition.at)
        .max()
}

/// When a released wait landed, if one did: a transition out of waiting
/// into pending that the core drove.
fn released_at(history: &[TaskTransition], id: &TaskId) -> Option<SystemTime> {
    history
        .iter()
        .filter(|transition| {
            transition.task_id == *id
                && transition.from == Some(TaskStatus::Waiting)
                && transition.to == TaskStatus::Pending
                && transition.by == By::System
        })
        .map(|transition| transition.at)
        .max()
}

fn fmt_secs(secs: u64) -> String {
    crate::board::fmt_secs(secs)
}

/// The crossings one project's board implies, given what already fired.
pub(crate) fn crossings(
    lead: &SessionSlot,
    rows: &[BoardRow],
    history: &[TaskTransition],
    unaccounted: &[String],
    fired: &[TaskChase],
    now: SystemTime,
) -> Vec<Crossing> {
    let mut out = Vec::new();
    for row in rows {
        let id = &row.task.id;
        let estimate = row.task.estimate.as_ref().filter(|e| e.secs > 0);
        let running = row.task.status == TaskStatus::InProgress;

        if running
            && let Some(estimate) = estimate
            && row.worked_secs > estimate.secs
            && !fired_already(fired, ChaseRung::EstimateNudge, id, Some(&estimate.secs.to_string()))
        {
            out.push(Crossing {
                rung: ChaseRung::EstimateNudge,
                task_id: Some(id.clone()),
                subject: Some(estimate.secs.to_string()),
                seat: row.task.owner.clone(),
                text: format!(
                    "task board: \"{}\" has worked {} against its estimate of {} - status? A new \
                     estimate, completing it, or parking it clears this.",
                    row.task.subject,
                    fmt_secs(row.worked_secs),
                    estimate.words,
                ),
            });
        }
        if running
            && let Some(estimate) = estimate
            && row.worked_secs > estimate.secs + estimate.secs / 2
            && due(fired, ChaseRung::Escalate, id, None, CADENCE_BURNING_SECS, now)
        {
            out.push(Crossing {
                rung: ChaseRung::Escalate,
                task_id: Some(id.clone()),
                subject: None,
                seat: Some(lead.clone()),
                text: format!(
                    "task board: \"{}\" is at {} against an estimate of {} - rescope, reassign, \
                     take over, or restate the expectation.",
                    row.task.subject,
                    fmt_secs(row.worked_secs),
                    estimate.words,
                ),
            });
        }
        if row.marks.ready
            && row.task.owner.is_none()
            && row.marks.no_movement
            && due(fired, ChaseRung::QueueStall, id, None, CADENCE_STUCK_SECS, now)
        {
            out.push(Crossing {
                rung: ChaseRung::QueueStall,
                task_id: Some(id.clone()),
                subject: None,
                seat: Some(lead.clone()),
                text: format!(
                    "task board: \"{}\" has been ready and unowned for {} - the queue is \
                     stalling; dispatch it or rank it.",
                    row.task.subject,
                    fmt_secs(row.updated_secs_ago),
                ),
            });
        }
        let wait_kind = row.task.waiting_on.as_ref().and_then(|wait| wait.kind);
        if row.marks.waiting_too_long
            && matches!(wait_kind, Some(WaitingKind::Dependency | WaitingKind::Resource))
            && due(fired, ChaseRung::WaitStall, id, None, CADENCE_STUCK_SECS, now)
        {
            out.push(Crossing {
                rung: ChaseRung::WaitStall,
                task_id: Some(id.clone()),
                subject: None,
                seat: Some(lead.clone()),
                text: format!(
                    "task board: \"{}\" has waited {} on a {} - unblock it or change its plan.",
                    row.task.subject,
                    fmt_secs(row.updated_secs_ago),
                    match wait_kind {
                        Some(WaitingKind::Dependency) => "dependency",
                        Some(WaitingKind::Resource) => "resource",
                        _ => "wait",
                    },
                ),
            });
        }
        let died = row.task.status == TaskStatus::Failed;
        let stranded = running && row.marks.stale;
        // The death repeats at the stuck cadence, counted from the death
        // itself: a row that moved and died again is a new occurrence.
        if (died || stranded)
            && due(fired, ChaseRung::Death, id, failed_at(history, id), CADENCE_STUCK_SECS, now)
        {
            out.push(Crossing {
                rung: ChaseRung::Death,
                task_id: Some(id.clone()),
                subject: None,
                seat: Some(lead.clone()),
                text: format!(
                    "task board: \"{}\" {} - re-dispatch it or cancel it.",
                    row.task.subject,
                    if died { "failed" } else { "has no live owner" },
                ),
            });
        }
        if let Some(at) = released_at(history, id)
            && since_secs(at, now) <= WAIT_RESOLVED_WINDOW_SECS
            && !fired_since(fired, ChaseRung::WaitResolved, id, at)
        {
            out.push(Crossing {
                rung: ChaseRung::WaitResolved,
                task_id: Some(id.clone()),
                subject: None,
                seat: row.task.owner.clone(),
                text: format!(
                    "task board: \"{}\" was waiting and the thing it waited on has ended - \
                     resume it, re-plan it, or wait again.",
                    row.task.subject,
                ),
            });
        }
        if row.marks.to_close && due(fired, ChaseRung::EpicClose, id, None, CADENCE_STUCK_SECS, now)
        {
            out.push(Crossing {
                rung: ChaseRung::EpicClose,
                task_id: Some(id.clone()),
                subject: None,
                seat: Some(lead.clone()),
                text: format!(
                    "task board: \"{}\" has every child terminal and its retro run - close it \
                     when you agree: completing the root row archives the tree.",
                    row.task.subject,
                ),
            });
        }
    }

    // The retro: an epic whose children have all stopped and which has no
    // retro yet asks its owner for one.
    for row in rows {
        if row.task.parent.is_none()
            && row.rollup.is_some()
            && !row.task.links.iter().any(|l| l.label.as_deref() == Some("retro"))
            && due(fired, ChaseRung::Retro, &row.task.id, None, CADENCE_STUCK_SECS, now)
            && rows.iter().any(|child| child.task.parent.as_ref() == Some(&row.task.id))
            && rows.iter().filter(|child| child.task.parent.as_ref() == Some(&row.task.id)).all(
                |child| {
                    matches!(
                        child.task.status,
                        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Canceled
                    )
                },
            )
        {
            out.push(Crossing {
                rung: ChaseRung::Retro,
                task_id: Some(row.task.id.clone()),
                subject: None,
                seat: Some(lead.clone()),
                text: retro_prompt(row, rows),
            });
        }
    }

    // The rows parked on the user: ONE nudge per row per wait episode, so
    // a row the lead has acted on (attested, withdrawn, or explicitly
    // parked) drops out and is never named again. The set is the board's
    // own derivation (`parked_on_user`), so a verification park a row left
    // behind under an inherited gate is not chased as a user wait (#2423),
    // and a wait the lead parked by design is left alone (#2423/D). Rows
    // bounded by a dependency or a resource are not this list's either.
    for row in rows.iter().filter(|row| {
        row.task.status == TaskStatus::Waiting
            && parked_on_user(&row.task)
            && row.task.waiting_on.as_ref().is_some_and(|wait| !wait.parked)
            && row.updated_secs_ago > CADENCE_STUCK_SECS
    }) {
        let since = waiting_since(history, &row.task.id).unwrap_or(SystemTime::UNIX_EPOCH);
        if fired_since(fired, ChaseRung::UserWaitDigest, &row.task.id, since) {
            continue;
        }
        let what = if row.task.waiting_on.as_ref().is_some_and(|wait| wait.verification) {
            "a look - approve it on the board, or complete it with tasks__approve and the \
             approval quoted"
        } else {
            "a decision - answer it on the board"
        };
        out.push(Crossing {
            rung: ChaseRung::UserWaitDigest,
            task_id: Some(row.task.id.clone()),
            subject: None,
            seat: Some(lead.clone()),
            text: format!(
                "task board: \"{}\" ({}) is waiting on the user: {what}. Confirm it's really \
                 theirs, or re-plan.",
                row.task.subject,
                fmt_secs(row.updated_secs_ago),
            ),
        });
    }

    for label in unaccounted {
        // **The log is the whole slate**, this rung's included: the newest
        // naming for the worker, and the cadence is what a repeat waits. A
        // worker whose state cleared had its naming forgotten - the sweep
        // does that, durably - so its next drift is named at once, across a
        // restart too.
        if durable_last_named(fired, label)
            .is_none_or(|at| since_secs(at, now) >= CADENCE_BURNING_SECS)
        {
            out.push(Crossing {
                rung: ChaseRung::Unaccounted,
                task_id: None,
                subject: Some(label.clone()),
                seat: Some(lead.clone()),
                text: format!("task board: worker {label} holds no row - feed it or despawn it."),
            });
        }
    }
    out
}

/// What an epic's owner is asked when its children have all stopped: the
/// numbers, computed here and authoritative, and the ask.
fn retro_prompt(epic: &BoardRow, rows: &[BoardRow]) -> String {
    let children: Vec<&BoardRow> =
        rows.iter().filter(|row| row.task.parent.as_ref() == Some(&epic.task.id)).collect();
    let worked: u64 = children.iter().map(|child| child.worked_secs).sum();
    let estimated: u64 = children
        .iter()
        .filter_map(|child| child.task.estimate.as_ref())
        .map(|estimate| estimate.secs)
        .sum();
    let mut reopened = 0usize;
    let mut failed = 0usize;
    for child in &children {
        if child.task.attempt > 1 {
            reopened += 1;
        }
        if child.task.status == TaskStatus::Failed {
            failed += 1;
        }
    }
    format!(
        "task board: epic \"{}\" has every child terminal. Numbers (authoritative - do not \
         recompute): {} tasks, worked {} against {} estimated, {reopened} reopened, {failed} \
         failed. Narrate two to four findings and at most three recommendations, each one a \
         task under this epic with tasks__create; record it as a link labelled `retro` on the \
         epic.",
        epic.task.subject,
        children.len(),
        fmt_secs(worked),
        fmt_secs(estimated),
    )
}

/// The delivery ladder: the seat the crossing prefers when it is live,
/// else the lead when it is, else nobody - and nobody means the mark
/// alone stands.
fn ladder(
    crossing: &Crossing,
    lead: &SessionSlot,
    live: impl Fn(&SessionSlot) -> bool,
) -> Option<SessionSlot> {
    if let Some(seat) = &crossing.seat
        && live(seat)
    {
        return Some(seat.clone());
    }
    live(lead).then(|| lead.clone())
}

impl Workspace {
    /// Every chase already fired for `project`.
    fn chases_for_project(&self, project: &str) -> Vec<TaskChase> {
        let guard = self.db.lock();
        match guard.as_ref() {
            Some(db) => {
                crate::store::task_chases::list_for_project(db, project).unwrap_or_default()
            }
            None => Vec::new(),
        }
    }

    /// `project`'s transitions, for the rung that reads when a wait
    /// resolved.
    fn history_for_project(&self, project: &str) -> Vec<TaskTransition> {
        let guard = self.db.lock();
        match guard.as_ref() {
            Some(db) => {
                crate::store::task_history::list_for_project(db, project).unwrap_or_default()
            }
            None => Vec::new(),
        }
    }

    /// Record one fired chase, durably, so the rung never repeats.
    fn record_chase(&self, chase: &TaskChase) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        if let Err(error) = crate::store::task_chases::append(db, std::slice::from_ref(chase)) {
            tracing::error!(
                target: "forge_workspace::chase",
                %error,
                "persisting a fired chase failed; the rung may fire again next pass",
            );
        }
    }

    /// Forget one worker's naming: its slate cleared, so the record goes
    /// with the state and the next drift is named at once.
    fn forget_drift(&self, project: &str, label: &str) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        if let Err(error) = crate::store::task_chases::forget(
            db,
            project,
            ChaseRung::Unaccounted,
            None,
            Some(label),
        ) {
            tracing::error!(
                target: "forge_workspace::chase",
                event_name = "chase_slate_forget_failed",
                %project,
                %label,
                %error,
                "forgetting a cleared worker's naming failed; its next drift may wait out \
                 the cadence",
            );
        }
    }

    /// One pass of the chase over every project: derive the crossings,
    /// walk the ladder for each, deliver, and record what fired. Returns
    /// how many messages reached a session.
    pub fn chase_sweep(&self, deliver: &dyn Deliver, now: SystemTime) -> usize {
        let mut delivered = 0;
        for view in self.list_projects() {
            let project = view.name.clone();
            let lead = SessionSlot::lead(&view.org, &project);
            let rows = self.board_rows(&project, now, crate::board::DEFAULT_STALE_SECS);
            let history = self.history_for_project(&project);
            let unaccounted = self.unaccounted(&project);
            let fired = self.chases_for_project(&project);
            // A worker that accounted clears its slate here and not in some
            // process's memory: the naming goes durably, so a recurrence
            // after a restart is named at once rather than waiting out the
            // cadence.
            for chase in &fired {
                if chase.rung == ChaseRung::Unaccounted
                    && let Some(label) = chase.subject.as_deref()
                    && !unaccounted.iter().any(|held| held == label)
                {
                    self.forget_drift(&project, label);
                }
            }
            let crossings = crossings(&lead, &rows, &history, &unaccounted, &fired, now);
            for crossing in crossings {
                let target = ladder(&crossing, &lead, |seat| self.seat_is_live(seat));
                let mut landed = false;
                if let Some(target) = &target
                    && deliver.to_seat(target, &crossing.text) == Delivery::Delivered
                {
                    delivered += 1;
                    landed = true;
                }
                // A user nudge is one shot per row: a tick that found every
                // seat asleep must not consume it, or the row's only nudge is
                // lost. Every other rung keeps its mark-alone behaviour.
                if crossing.rung == ChaseRung::UserWaitDigest && !landed {
                    continue;
                }
                self.record_chase(&TaskChase {
                    task_id: crossing.task_id.clone(),
                    subject: crossing.subject.clone(),
                    project_name: project.clone(),
                    rung: crossing.rung,
                    at: now,
                });
            }
        }
        delivered
    }

    /// Spawn the chase sweep: a background pass every
    /// [`CHASE_INTERVAL_SECS`] that delivers the crossings and announces
    /// the board so time-derived marks move in a held client snapshot.
    /// Idempotent; started once at boot. Mirrors
    /// [`Workspace::start_cron_scheduler`].
    pub fn start_chase_sweep(self: &Arc<Self>) {
        if self.chase_sweep_started.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("chase_sweep");
        tokio::spawn(
            async move {
                let period = std::time::Duration::from_secs(CHASE_INTERVAL_SECS);
                let mut interval =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(workspace) = weak.upgrade() else {
                        return;
                    };
                    let delivered = workspace.chase_sweep(
                        &PromptDeliver { workspace: Arc::clone(&workspace) },
                        SystemTime::now(),
                    );
                    if delivered > 0 {
                        tracing::debug!(
                            target: "forge_workspace::chase",
                            delivered,
                            "chase sweep delivered",
                        );
                    }
                    workspace.announce_board_refresh();
                }
            }
            .instrument(span),
        );
    }
}

/// The production delivery: a prompt into a live session, and the turn
/// its words draw as.
///
/// **A chase carries no envelope of its own** - no delivery update draws
/// it and the CLI echoes no stdin prompt back - so the row is forged here
/// rather than left to `delivery_turn`, which has no arm for it. The seat
/// is already known live by the ladder, so a failed dispatch here is a
/// race, not a reason to spawn.
struct PromptDeliver {
    workspace: Arc<Workspace>,
}

impl Deliver for PromptDeliver {
    fn to_seat(&self, slot: &SessionSlot, text: &str) -> Delivery {
        let uuid = forge_sdk::request_id::next_forged_prompt_id();
        match self.workspace.dispatch_forged_prompt_under(slot, text.to_owned(), uuid) {
            Ok(()) => Delivery::Delivered,
            Err(_) => Delivery::NoSession,
        }
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::protocol::SessionUpdate;
    use forge_primitives::Message;
    use forge_primitives::tasks::{
        By, Estimate, LinkKind, Task, TaskId, TaskLink, TaskStatus, TaskTransition, Verify, Waiting,
    };
    use tempfile::tempdir;

    const PROJECT: &str = "proj";
    const WINDOW: u64 = 4 * 3600;

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fixture(dir: &std::path::Path) -> Arc<Workspace> {
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.to_owned());
        ws.seed_test_project(PROJECT, "/tmp/tp-chase");
        let db = crate::store::Db::open(&dir.join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws
    }

    fn task(id: &str, status: TaskStatus) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: PROJECT.to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status,
            owner: None,
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

    fn running_over_estimate(ws: &Workspace, id: &str, owner: Option<&str>) {
        let mut row = task(id, TaskStatus::InProgress);
        row.estimate = Some(Estimate { words: "10s".to_owned(), secs: 10 });
        row.owner = owner.map(|label| SessionSlot::worker("TestOrg", PROJECT, label));
        row.updated_at = epoch(100);
        ws.seed_test_task(row);
        let guard = ws.db.lock();
        crate::store::task_history::append(
            guard.as_ref().expect("db"),
            &[TaskTransition {
                task_id: TaskId::from(id),
                project_name: PROJECT.to_owned(),
                from: Some(TaskStatus::Pending),
                to: TaskStatus::InProgress,
                at: epoch(10),
                by: By::System,
            }],
        )
        .expect("history");
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

    fn sweep(ws: &Arc<Workspace>, deliver: &dyn Deliver, now: u64) -> usize {
        ws.chase_sweep(deliver, epoch(now))
    }

    /// A lead seat behind a live session, so lead-addressed rungs have
    /// somewhere to land. The returned receiver must be held for the
    /// stub to stay.
    fn live_lead(
        ws: &Workspace,
    ) -> tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand> {
        ws.install_testing_stub(&SessionSlot::lead("TestOrg", PROJECT))
    }

    /// A live worker in `project` holding no row: the shape the
    /// unaccounted rung names.
    fn worker_holding_no_row(ws: &Workspace, project: &str, label: &str) {
        let key = project_key(ws, project);
        ws.insert_live_worker(
            &key,
            crate::WorkerEntry {
                label: label.to_owned(),
                charter: "charter".to_owned(),
                slot: SessionSlot::worker("TestOrg", project, label),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: epoch(0),
                spawned_by: SessionSlot::lead("TestOrg", project),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: None,
                diagnostic: None,
                kick: None,
            },
        );
    }

    fn project_key(ws: &Workspace, project: &str) -> crate::ProjectKey {
        ws.list_projects().into_iter().find(|v| v.name == project).expect("view").key.clone()
    }

    /// One status transition, durably logged, at a synthetic instant.
    fn append_transition(ws: &Workspace, id: &str, from: TaskStatus, to: TaskStatus, at: u64) {
        let guard = ws.db.lock();
        crate::store::task_history::append(
            guard.as_ref().expect("db"),
            &[TaskTransition {
                task_id: TaskId::from(id),
                project_name: PROJECT.to_owned(),
                from: Some(from),
                to,
                at: epoch(at),
                by: By::System,
            }],
        )
        .expect("history");
    }

    /// How many delivered messages carry `needle`.
    fn says(deliver: &Recording, needle: &str) -> usize {
        deliver.seen.lock().iter().filter(|(_, text)| text.contains(needle)).count()
    }

    /// The unaccounted rung is per project: one project's pass never
    /// makes another's namings fresh again. Regression for the sweep
    /// that re-fired every worker of every project on every pass.
    #[test]
    fn two_projects_through_one_sweep_are_named_once_each() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_project("other", "/tmp/tp-chase-other");
        let _lead_rx = live_lead(&ws);
        let _other_rx = ws.install_testing_stub(&SessionSlot::lead("TestOrg", "other"));
        worker_holding_no_row(&ws, PROJECT, "w-1");
        worker_holding_no_row(&ws, "other", "w-2");
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 100), 2, "one nudge per project");
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(_, text)| text.contains("worker w-1 holds no row")),
            "proj's worker is named: {seen:?}",
        );
        assert!(
            seen.iter().any(|(_, text)| text.contains("worker w-2 holds no row")),
            "other's worker is named: {seen:?}",
        );
        drop(seen);

        assert_eq!(sweep(&ws, &deliver, 140), 0, "and neither repeats");
        assert_eq!(deliver.seen.lock().len(), 2, "still exactly the two namings");
    }

    /// An empty project - one holding no unaccounted worker - leaves
    /// the other projects' namings standing. Regression for the pass
    /// that wiped the whole memory whenever any project was empty.
    #[test]
    fn an_empty_project_does_not_wipe_another_projects_namings() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_project("empty", "/tmp/tp-chase-empty");
        let _lead_rx = live_lead(&ws);
        worker_holding_no_row(&ws, PROJECT, "w-1");
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 100), 1, "the worker is named");
        assert_eq!(sweep(&ws, &deliver, 140), 0, "the empty pass says nothing");
        assert_eq!(deliver.seen.lock().len(), 1, "and the naming stands");
    }

    /// The drifted worker's naming repeats at the 5-minute cadence while
    /// it stays drifted.
    #[test]
    fn the_unaccounted_naming_repeats_at_its_cadence() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        worker_holding_no_row(&ws, PROJECT, "w-1");
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 100), 1, "named on appearance");
        assert_eq!(sweep(&ws, &deliver, 399), 0, "quiet inside the cadence");
        assert_eq!(sweep(&ws, &deliver, 400), 1, "named again at it");
        assert_eq!(sweep(&ws, &deliver, 500), 0, "quiet again");
        assert_eq!(sweep(&ws, &deliver, 701), 1, "and again at the next one");
    }

    /// A worker that accounts resets its slate, durably: the naming goes
    /// off the log with the state, so a recurrence inside the old cadence
    /// is named at once - after a restart included, since the log is the
    /// only slate there is.
    #[test]
    fn a_cleared_unaccounted_worker_is_named_at_once_on_its_next_drift() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        worker_holding_no_row(&ws, PROJECT, "w-1");
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 100), 1, "named on appearance");
        // The worker despawns: the state cleared, and the clearing pass
        // takes its naming off the log rather than off a process's memory.
        ws.remove_latest_worker(&project_key(&ws, PROJECT), "w-1");
        assert_eq!(sweep(&ws, &deliver, 150), 0, "nothing drifted");
        assert!(
            !ws.chases_for_project(PROJECT)
                .iter()
                .any(|chase| chase.rung == ChaseRung::Unaccounted),
            "the cleared worker's naming is gone from the log",
        );
        // It drifts again, inside the old cadence.
        worker_holding_no_row(&ws, PROJECT, "w-1");
        assert_eq!(sweep(&ws, &deliver, 200), 1, "the clear reset the slate");
    }

    /// The naming is durable: it lives on the log alone, so any pass - a
    /// fresh process's first included - reads it rather than re-firing
    /// inside the cadence.
    #[test]
    fn a_restart_inside_the_cadence_does_not_rename_the_worker() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        worker_holding_no_row(&ws, PROJECT, "w-1");
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 100), 1, "named");
        assert!(
            ws.chases_for_project(PROJECT).iter().any(|chase| chase.rung == ChaseRung::Unaccounted
                && chase.subject.as_deref() == Some("w-1")),
            "and the naming is on the log, where a fresh process would read it",
        );
        assert_eq!(sweep(&ws, &deliver, 200), 0, "no pass re-names it inside the cadence");
        assert_eq!(sweep(&ws, &deliver, 401), 1, "the cadence runs off the logged naming");
    }

    /// The 30-minute class on a rung with no floor: a stalled queue
    /// repeats at the cadence while it stays stalled.
    #[test]
    fn the_queue_stall_repeats_every_thirty_minutes() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Pending);
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, WINDOW + 1), 1, "stalled: named");
        assert_eq!(sweep(&ws, &deliver, WINDOW + 1001), 0, "quiet inside the cadence");
        assert_eq!(sweep(&ws, &deliver, WINDOW + 1801), 1, "named again at it");
    }

    /// A failed row is named at the cadence while it stays failed, and a
    /// re-dispatch that fails again is a new occurrence - named at once,
    /// not held back by the window.
    #[test]
    fn a_failed_row_is_named_at_its_cadence_and_a_refail_is_a_new_occurrence() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Failed);
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        append_transition(&ws, "t-1", TaskStatus::Pending, TaskStatus::Failed, 100);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 100), 1, "the death is named");
        assert_eq!(sweep(&ws, &deliver, 1000), 0, "quiet inside the cadence");
        assert_eq!(sweep(&ws, &deliver, 1901), 1, "named again at it");
        // Re-dispatched and failed again: a new occurrence.
        append_transition(&ws, "t-1", TaskStatus::Pending, TaskStatus::InProgress, 5000);
        append_transition(&ws, "t-1", TaskStatus::InProgress, TaskStatus::Failed, 5000);
        assert_eq!(sweep(&ws, &deliver, 5010), 1, "the new failure is named at once");
    }

    /// The 1x overrun nudge is once per estimate: a restated estimate
    /// arms it again.
    #[test]
    fn a_restated_estimate_arms_the_overrun_nudge_again() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        running_over_estimate(&ws, "t-1", None);
        let deliver = Recording::default();

        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "has worked"), 1, "named once: {:?}", deliver.seen.lock());
        sweep(&ws, &deliver, 400);
        assert_eq!(says(&deliver, "has worked"), 1, "never twice for one estimate");
        // The owner restates the estimate; the row is still over it.
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.estimate = Some(Estimate { words: "1m".to_owned(), secs: 60 });
        })
        .expect("no refusal")
        .expect("the row is there");
        sweep(&ws, &deliver, 500);
        assert_eq!(
            says(&deliver, "has worked"),
            2,
            "the restated estimate is a new occurrence: {:?}",
            deliver.seen.lock(),
        );
        sweep(&ws, &deliver, 700);
        assert_eq!(says(&deliver, "has worked"), 2, "once per estimate, not per sweep");
    }

    /// The 1.5x escalation repeats every five minutes while the row
    /// stays over.
    #[test]
    fn the_escalation_repeats_at_its_cadence() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        running_over_estimate(&ws, "t-1", None);
        let deliver = Recording::default();

        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "is at"), 1, "escalated once: {:?}", deliver.seen.lock());
        sweep(&ws, &deliver, 400);
        assert_eq!(says(&deliver, "is at"), 1, "quiet inside the cadence");
        sweep(&ws, &deliver, 501);
        assert_eq!(says(&deliver, "is at"), 2, "escalated again at it");
    }

    /// A row parked on the user - a decision wait, or the look's own
    /// verification park - untouched since `updated`. A verification park
    /// belongs to a row carrying its own gate (#2423).
    fn waiting_on_user(ws: &Workspace, id: &str, updated: u64, verification: bool) {
        let mut row = task(id, TaskStatus::Waiting);
        if verification {
            row.verify = Some(Verify::User);
        }
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Decision),
            detail: None,
            on: None,
            verification,
            parked: false,
        });
        row.updated_at = epoch(updated);
        ws.seed_test_task(row);
    }

    /// Every row parked on the user gets ONE nudge of its own, and none
    /// repeats on the cadence.
    #[test]
    fn every_user_wait_takes_one_nudge_and_none_repeats() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        for id in ["t-1", "t-2", "t-3"] {
            waiting_on_user(&ws, id, 0, false);
        }
        let deliver = Recording::default();

        let now = WINDOW + 1;
        assert_eq!(sweep(&ws, &deliver, now), 3, "one message per row, not one digest");
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 3, "three rows, three nudges");
        for id in ["t-1", "t-2", "t-3"] {
            assert!(
                seen.iter().any(|(_, text)| text.contains(&format!("\"subject {id}\""))),
                "no nudge names {id}: {seen:?}",
            );
        }
        assert!(
            seen.iter().all(|(seat, _)| *seat == SessionSlot::lead("TestOrg", PROJECT)),
            "every nudge goes to the lead",
        );
        drop(seen);

        assert_eq!(sweep(&ws, &deliver, now + 1799), 0, "a row is nudged once");
        assert_eq!(sweep(&ws, &deliver, now + 1800), 0, "and not again on the cadence");
    }

    /// A verification park takes its own nudge, once; a row that starts
    /// waiting after the sweep gets one of its own.
    #[test]
    fn a_verify_park_takes_its_own_nudge_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        waiting_on_user(&ws, "t-1", 0, false);
        waiting_on_user(&ws, "t-2", 0, true);
        let deliver = Recording::default();

        let now = WINDOW + 1;
        assert_eq!(sweep(&ws, &deliver, now), 2, "a decision and a park, one nudge each");
        let seen = deliver.seen.lock();
        let park = seen.iter().find(|(_, text)| text.contains("subject t-2")).expect("t-2 named");
        assert!(park.1.contains("a look"), "the park's nudge names the look: {}", park.1);
        drop(seen);
        assert_eq!(sweep(&ws, &deliver, now + 1800), 0, "neither is nudged twice");

        // A row that starts waiting after this sweep is a new episode.
        waiting_on_user(&ws, "t-3", 0, false);
        assert_eq!(sweep(&ws, &deliver, now + 3600), 1, "the new wait gets its own nudge");
        assert!(deliver.seen.lock().last().expect("a nudge").1.contains("subject t-3"));
    }

    /// A wait the lead parked by design - the user's own deferral - is left
    /// alone: no nudge, no repeat (#2423/D).
    #[test]
    fn a_parked_wait_is_left_alone() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Waiting);
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Decision),
            detail: Some("backlog at Ved's word".to_owned()),
            on: None,
            verification: false,
            parked: true,
        });
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, WINDOW + 1), 0, "a parked wait is not the user's chase");
        assert!(deliver.seen.lock().is_empty());
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .all(|chase| chase.rung != ChaseRung::UserWaitDigest),
            "and no mark is left for it",
        );
    }

    /// A verification park whose row owns no gate - the shape an inherited
    /// gate left behind - is not a user wait (#2423): the digest does not
    /// name it, and no one is asked to confirm it.
    #[test]
    fn a_verification_park_without_an_own_gate_stays_out_of_the_digest() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        waiting_on_user(&ws, "t-1", 0, true);
        // The same wait on a row whose gate was never its own: the epic
        // carried it, so the park is not an ask on the user.
        let mut inherited = task("t-2", TaskStatus::Waiting);
        inherited.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Decision),
            detail: None,
            on: None,
            verification: true,
            parked: false,
        });
        inherited.updated_at = epoch(0);
        ws.seed_test_task(inherited);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, WINDOW + 1), 1, "only the genuine park is nudged");
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 1, "one nudge, for the row that owns the gate");
        let text = &seen[0].1;
        assert!(text.contains("\"subject t-1\""), "the own-gate park joins: {text}");
        assert!(!text.contains("\"subject t-2\""), "the inherited park stays out: {text}");
    }

    /// The user nudge never wakes a sleeping lead, and a tick that found it
    /// asleep does not consume the row's one nudge (#2423/D): nothing is
    /// delivered and nothing is marked, so the first tick with a live lead
    /// delivers it. Every other rung keeps its mark-alone behaviour.
    #[test]
    fn a_user_wait_nudge_waits_for_a_live_lead() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        waiting_on_user(&ws, "t-1", 0, false);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, WINDOW + 1), 0, "nothing delivered");
        assert!(deliver.seen.lock().is_empty(), "no seat was reached");
        assert!(
            !ws.chases_for_project(PROJECT)
                .iter()
                .any(|chase| chase.rung == ChaseRung::UserWaitDigest),
            "a tick that found the lead asleep must not spend the row's one nudge",
        );

        let _lead_rx = live_lead(&ws);
        assert_eq!(sweep(&ws, &deliver, WINDOW + 2), 1, "the first awake tick delivers it");
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .any(|chase| chase.rung == ChaseRung::UserWaitDigest),
            "and the landed nudge is on the log",
        );
        assert_eq!(sweep(&ws, &deliver, WINDOW + 1800), 0, "then it is spent");
    }

    /// The estimate crossing asks the owner once, ever - a later sweep
    /// with the same facts stays quiet because the rung is on the log.
    #[test]
    fn the_estimate_rung_fires_once_ever_per_row() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_project("proj", "/tmp/tp-chase"); // already seeded; harmless
        let key =
            ws.list_projects().into_iter().find(|v| v.name == PROJECT).expect("view").key.clone();
        ws.insert_live_worker(
            &key,
            crate::WorkerEntry {
                label: "w-1".to_owned(),
                charter: "charter".to_owned(),
                slot: SessionSlot::worker("TestOrg", PROJECT, "w-1"),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: epoch(0),
                spawned_by: SessionSlot::lead("TestOrg", PROJECT),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: None,
                diagnostic: None,
                kick: None,
            },
        );
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 100), 1, "one nudge");
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 1, "exactly one message: {seen:?}");
        assert_eq!(seen[0].0, SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        assert!(seen[0].1.contains("subject t-1"), "the message names the row: {}", seen[0].1);
        assert!(
            seen[0].1.contains("A new estimate, completing it, or parking it"),
            "the ask names what actually clears the overrun, not an update: {}",
            seen[0].1,
        );
        drop(seen);

        assert_eq!(sweep(&ws, &deliver, 140), 0, "the rung never repeats");
        assert_eq!(deliver.seen.lock().len(), 1, "and nothing more was said");
    }

    /// A dead owner does not swallow the nudge: it lands with the lead.
    #[test]
    fn a_dead_owners_nudge_goes_to_the_lead() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        running_over_estimate(&ws, "t-1", Some("gone"));
        let deliver = Recording::default();

        sweep(&ws, &deliver, 100);
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(seat, _)| seat == &SessionSlot::lead("TestOrg", PROJECT)),
            "the lead hears it: {seen:?}",
        );
    }

    /// Every seat asleep: the rungs still fire into the log - the mark
    /// alone stands - and NOTHING is delivered and nothing is spawned.
    #[test]
    fn with_every_seat_asleep_the_rung_is_mark_only_and_nothing_spawns() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        running_over_estimate(&ws, "t-1", Some("gone"));
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 100), 0, "nothing delivered");
        assert!(deliver.seen.lock().is_empty(), "no seat was reached");
        assert!(
            !ws.chases_for_project(PROJECT).is_empty(),
            "and the rung is still on the log, so it fired once and never repeats",
        );
    }

    /// One nudge per wait EPISODE: the row's own newest transition into
    /// waiting is the episode key, so a row that left waiting and came back
    /// takes a fresh nudge rather than inheriting the spent one (#2423/D).
    #[test]
    fn a_wait_re_entering_after_a_move_is_a_new_nudge() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Waiting);
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Decision),
            detail: None,
            on: None,
            verification: false,
            parked: false,
        });
        row.updated_at = epoch(100);
        ws.seed_test_task(row);
        let append = |from: Option<TaskStatus>, to: TaskStatus, at: u64| {
            let guard = ws.db.lock();
            crate::store::task_history::append(
                guard.as_ref().expect("db"),
                &[TaskTransition {
                    task_id: TaskId::from("t-1"),
                    project_name: PROJECT.to_owned(),
                    from,
                    to,
                    at: epoch(at),
                    by: By::System,
                }],
            )
            .expect("history");
        };
        append(Some(TaskStatus::Pending), TaskStatus::Waiting, 100);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 3000), 1, "the episode's first nudge");
        // Off waiting and back: a new episode at 4100.
        append(Some(TaskStatus::Waiting), TaskStatus::InProgress, 4000);
        append(Some(TaskStatus::InProgress), TaskStatus::Waiting, 4100);
        assert_eq!(sweep(&ws, &deliver, 6000), 1, "the new episode is nudged");
        assert_eq!(sweep(&ws, &deliver, 9000), 0, "and that nudge is spent in turn");
    }

    /// A dependency or a resource wait is not the user's: neither is nudged
    /// as a user wait however the rows around it are gated (#2415).
    #[test]
    fn resource_and_dependency_waits_are_not_the_users() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        for (id, kind) in [("t-1", WaitingKind::Dependency), ("t-2", WaitingKind::Resource)] {
            let mut row = task(id, TaskStatus::Waiting);
            row.waiting_on = Some(Waiting {
                kind: Some(kind),
                detail: None,
                on: None,
                verification: false,
                parked: false,
            });
            row.updated_at = epoch(0);
            ws.seed_test_task(row);
        }
        let deliver = Recording::default();

        sweep(&ws, &deliver, WINDOW + 1);
        // The stuck-wait rung may name them to the LEAD ("unblock it"), which
        // is the task's business; what neither may be is the user's.
        assert!(
            deliver.seen.lock().iter().all(|(_, text)| !text.contains("waiting on the user")),
            "a dependency or resource wait was listed as the user's: {:?}",
            deliver.seen.lock(),
        );
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .all(|chase| chase.rung != ChaseRung::UserWaitDigest),
            "and neither leaves a user-wait mark",
        );
    }

    /// A waiting row is never estimate-chased: its clock is stopped, and
    /// its wait's age is the signal - which has its own rung.
    #[test]
    fn a_waiting_row_is_never_estimate_chased() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Waiting);
        row.estimate = Some(Estimate { words: "10s".to_owned(), secs: 10 });
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Resource),
            detail: None,
            on: None,
            verification: false,
            parked: false,
        });
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        let deliver = Recording::default();

        sweep(&ws, &deliver, WINDOW + 1);
        let seen = deliver.seen.lock();
        assert!(
            !seen.iter().any(|(_, text)| text.contains("estimate")),
            "no estimate nudge for a waiting row: {seen:?}",
        );
        assert!(
            seen.iter().any(|(_, text)| text.contains("resource")),
            "the wait's own rung carries it: {seen:?}",
        );
    }

    /// A wait that resolved: the waiter's owner hears once, and only once
    /// for that resolution.
    #[test]
    fn a_resolved_wait_messages_the_waiter_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let key =
            ws.list_projects().into_iter().find(|v| v.name == PROJECT).expect("view").key.clone();
        ws.insert_live_worker(
            &key,
            crate::WorkerEntry {
                label: "w-1".to_owned(),
                charter: "charter".to_owned(),
                slot: SessionSlot::worker("TestOrg", PROJECT, "w-1"),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: epoch(0),
                spawned_by: SessionSlot::lead("TestOrg", PROJECT),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: None,
                diagnostic: None,
                kick: None,
            },
        );
        let mut row = task("t-1", TaskStatus::Pending);
        row.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        row.updated_at = epoch(100);
        ws.seed_test_task(row);
        {
            let guard = ws.db.lock();
            crate::store::task_history::append(
                guard.as_ref().expect("db"),
                &[TaskTransition {
                    task_id: TaskId::from("t-1"),
                    project_name: PROJECT.to_owned(),
                    from: Some(TaskStatus::Waiting),
                    to: TaskStatus::Pending,
                    at: epoch(90),
                    by: By::System,
                }],
            )
            .expect("history");
        }
        let deliver = Recording::default();

        sweep(&ws, &deliver, 100);
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(seat, text)| seat == &SessionSlot::worker("TestOrg", PROJECT, "w-1")
                && text.contains("was waiting")),
            "the waiter's owner hears the release: {seen:?}",
        );
        let before = seen.len();
        drop(seen);
        sweep(&ws, &deliver, 130);
        assert_eq!(deliver.seen.lock().len(), before, "and it never repeats");
    }

    /// A stalled queue is the lead's failure and is named once.
    #[test]
    fn a_stalled_queue_row_messages_the_lead_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Pending);
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        let deliver = Recording::default();

        sweep(&ws, &deliver, WINDOW + 1);
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 1, "one message: {seen:?}");
        assert!(seen[0].1.contains("queue is stalling"), "{}", seen[0].1);
        drop(seen);
        sweep(&ws, &deliver, WINDOW + 10);
        assert_eq!(deliver.seen.lock().len(), 1, "and never twice");
    }

    /// An epic with every child terminal and no retro asks for one, and
    /// once the retro link is on, it asks for the close instead.
    #[test]
    fn the_retro_fires_once_and_then_the_close_nudge_fires_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        ws.seed_test_task(task("epic", TaskStatus::InProgress));
        let mut child = task("sub-a", TaskStatus::Completed);
        child.parent = Some(TaskId::from("epic"));
        ws.seed_test_task(child);
        let deliver = Recording::default();

        sweep(&ws, &deliver, 100);
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(_, text)| {
                text.contains("every child terminal") && text.contains("Numbers")
            }),
            "the retro prompt carries the numbers: {seen:?}",
        );
        drop(seen);

        // The retro runs: the link lands, and the next pass asks for the
        // close - once.
        ws.update_task(
            PROJECT,
            &TaskId::from("epic"),
            By::Seat(SessionSlot::lead("TestOrg", PROJECT)),
            |task| {
                task.links.push(TaskLink {
                    kind: LinkKind::Path,
                    label: Some("retro".to_owned()),
                    target: "docs/retro.md".to_owned(),
                    state: None,
                    added_at: epoch(0),
                });
            },
        )
        .expect("no refusal")
        .expect("the epic is there");
        sweep(&ws, &deliver, 140);
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(_, text)| text.contains("close it when you agree")),
            "the close nudge fires: {seen:?}",
        );
        let before = seen.len();
        drop(seen);
        sweep(&ws, &deliver, 180);
        assert_eq!(deliver.seen.lock().len(), before, "neither rung repeats");
    }

    /// The delivery forges the chased words as the turn they reached the
    /// model as.
    ///
    /// **A chase has no envelope of its own**: no delivery update draws it,
    /// and the CLI echoes no stdin prompt back, so this frame is the only
    /// carrier the words have in a live view. Without it the turn opens on
    /// its first tool row, with nothing saying what started it.
    #[test]
    fn the_delivery_forges_the_chased_words_as_their_turn() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project(PROJECT, "/tmp/tp-chase");
        let key = SessionSlot::lead("TestOrg", PROJECT);
        ws.enable_test_dispatch_intercept();
        let deliver = PromptDeliver { workspace: Arc::clone(&ws) };

        let text = "task board: worker w-1 holds no row - feed it or despawn it.";
        assert_eq!(deliver.to_seat(&key, text), Delivery::Delivered);

        // The wired prompt and the forged frame are one thing by id: the
        // lifecycle frames name only the id, and a row minted apart from the
        // wire is one nothing can settle.
        let wired = match ws.drain_test_dispatch_buffer().as_slice() {
            [crate::protocol::Command::PromptUnder { uuid, text: wired, .. }] => {
                assert_eq!(wired, text, "the model receives the chase's words");
                assert!(
                    uuid.starts_with("forge_nudge_"),
                    "the id carries the machinery's prefix, which is what a reloaded row keys \
                     on: {uuid}",
                );
                uuid.clone()
            }
            other => panic!("the delivery wires one prompt; got {other:?}"),
        };
        let drawn: Vec<SessionUpdate> =
            std::iter::from_fn(|| rx.try_recv().ok()).map(|frame| frame.update).collect();
        let frame = drawn.iter().find_map(|update| match update {
            SessionUpdate::ChatAppended { msg, origin, .. } => Some((msg, origin)),
            _ => None,
        });
        let Some((Message::User { message, uuid, .. }, origin)) = frame else {
            panic!("the chase's delivery forges its words as the turn; got {drawn:?}")
        };
        assert_eq!(
            uuid.as_deref(),
            Some(wired.as_str()),
            "the frame is stamped with the id the prompt runs under",
        );
        assert!(origin.is_none(), "no view drew these words, so the frame claims no origin");
        assert!(
            matches!(
                message.content.first(),
                Some(forge_primitives::ContentBlock::Text { text: said, .. }) if said == text
            ),
            "the frame carries the prose the model received: {message:?}",
        );
    }
}
