//! The chase sweep: the pass that makes the board ask instead of report.
//!
//! Every pass derives crossings from the board's own state and the chase
//! log, on four signals together: the row's KIND (finite work owes a
//! re-plan on overrun; standing work is checked on its cadence), MOVEMENT
//! (no touch past the band with a live owner is stuck), NEW INPUT (a
//! connector saw something after the last touch) and the WAIT's kind (a
//! decision on the user is never chased, a resource wait is checked, a
//! dependency chase goes to the blocker's owner).
//!
//! **One cadence, not three.** A finite overrun asks once and parks the
//! row, and the lead hears the escalation once if the ask goes unanswered;
//! every other held state repeats at `CHASE_CADENCE_SECS` until it
//! clears. **The chase log is the whole slate**: every naming is deduped
//! by it, so a restart re-reads it rather than re-firing, and a state that
//! clears has its naming forgotten - off the log, where a fresh process
//! reads - so a recurrence is named at once, whatever happened in between.
//!
//! Delivery degrades, never wakes: a live owner is asked; with a dead
//! owner the lead is; with every seat asleep the mark alone stands. A
//! chase NEVER spawns a session - the board already shows the state, and
//! waking a project is a cost the chase does not get to spend.

use std::sync::Arc;
use std::time::SystemTime;

use forge_primitives::tasks::{
    By, ChaseRung, TaskChase, TaskId, TaskStatus, TaskTransition, WaitingKind,
};
use tracing::Instrument;

use crate::SessionSlot;
use crate::board::BoardRow;
use crate::workspace::Workspace;

/// How long between sweeps. The same number bounds how stale a
/// time-derived mark can be in a held client snapshot.
pub const CHASE_INTERVAL_SECS: u64 = 30;

/// How long after a wait resolves its message is still worth sending.
const WAIT_RESOLVED_WINDOW_SECS: u64 = 600;

/// The one cadence every repeated rung waits: a held state is named again
/// this long after its last naming, and no rung has a cadence of its own.
/// A re-plan ask is one-shot per estimate, its escalation once, and the
/// rest repeat here until the state clears.
const CHASE_CADENCE_SECS: u64 = 30 * 60;

/// What a delivery attempt found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Delivered,
    /// The dispatch failed, carrying the error it returned: a caller that
    /// names why a delivery did not land reads it from here.
    NoSession(String),
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

pub(crate) fn fmt_secs(secs: u64) -> String {
    crate::board::fmt_secs(secs)
}

/// The crossings one project's board implies, given what already fired.
///
/// **Four signals, together rather than blindly.** A row's KIND decides
/// what an overrun means (finite work owes a re-plan; standing work is
/// checked on its cadence), MOVEMENT says whether the owner is working or
/// stuck, NEW INPUT - a connector's look at a link after the last touch -
/// may make the row obsolete or urgent, and the WAIT's kind decides who
/// hears: a decision on the user is never chased, a resource wait is
/// checked, and a dependency chase goes to the blocker's owner.
///
/// A finite overrun asks once and parks the row (the sweep does the
/// write); the lead hears once if the ask goes unanswered past the
/// cadence, and then the chase stops for that estimate.
pub(crate) fn crossings(
    lead: &SessionSlot,
    rows: &[BoardRow],
    history: &[TaskTransition],
    unaccounted: &[String],
    fired: &[TaskChase],
    now: SystemTime,
) -> Vec<Crossing> {
    let stamp = |at: SystemTime| {
        at.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs()).to_string()
    };
    let mut out = Vec::new();
    for row in rows {
        let id = &row.task.id;
        let estimate = row.task.estimate.as_ref().filter(|e| e.secs > 0);
        let running = row.task.status == TaskStatus::InProgress;
        let over = row.task.kind == forge_primitives::tasks::TaskKind::Finite
            && estimate.is_some_and(|estimate| row.worked_secs > estimate.secs);
        let wait = row.task.waiting_on.as_ref();
        let parked = wait.is_some_and(|wait| wait.parked);

        // Signal 1a: finite work past its estimate owes a re-plan. The ask
        // is one per estimate and parks the row; the sweep writes the park.
        if let Some(estimate) = estimate.filter(|_| over)
            && !fired_already(fired, ChaseRung::EstimateNudge, id, Some(&estimate.secs.to_string()))
        {
            out.push(Crossing {
                rung: ChaseRung::EstimateNudge,
                task_id: Some(id.clone()),
                subject: Some(estimate.secs.to_string()),
                seat: row.task.owner.clone(),
                text: format!(
                    "task board: \"{}\" has worked {} against an estimate of {}. It is parked \
                     waiting on you: rescope it, split it, or close it with the reason - or move \
                     it back in progress if it is still the work you meant.",
                    row.task.subject,
                    fmt_secs(row.worked_secs),
                    estimate.words,
                ),
            });
        }
        // The escalation: once per estimate, and only after the ask had its
        // cadence unanswered. Then this estimate is done being chased.
        if over
            && let Some(estimate) = estimate
            && !fired_already(fired, ChaseRung::Escalate, id, Some(&estimate.secs.to_string()))
            && let Some(asked) =
                last_fired(fired, ChaseRung::EstimateNudge, id, Some(&estimate.secs.to_string()))
            && since_secs(asked, now) >= CHASE_CADENCE_SECS
        {
            out.push(Crossing {
                rung: ChaseRung::Escalate,
                task_id: Some(id.clone()),
                subject: Some(estimate.secs.to_string()),
                seat: Some(lead.clone()),
                text: format!(
                    "task board: \"{}\" passed its estimate of {} and the re-plan ask is \
                     unanswered; it is at {}. Rescope it, reassign it, or close it with the \
                     reason.",
                    row.task.subject,
                    estimate.words,
                    fmt_secs(row.worked_secs),
                ),
            });
        }
        // Signal 1b: standing work is checked when its cadence comes due,
        // counted from the row's last touch - a check IS a touch.
        if row.task.kind == forge_primitives::tasks::TaskKind::Standing
            && !row.task.status.is_terminal()
            && let Some(cadence) = row.task.cadence.as_ref().filter(|cadence| cadence.secs > 0)
            && row.updated_secs_ago >= cadence.secs
            && !fired_already(fired, ChaseRung::Cadence, id, Some(&stamp(row.task.updated_at)))
        {
            out.push(Crossing {
                rung: ChaseRung::Cadence,
                task_id: Some(id.clone()),
                subject: Some(stamp(row.task.updated_at)),
                seat: row.task.owner.clone(),
                text: format!(
                    "task board: \"{}\" is due for its check (every {}). Is it still doing its \
                     job? Change its shape, or end it.",
                    row.task.subject, cadence.words,
                ),
            });
        }
        // Signal 2: a row that has not moved past its band with a LIVE
        // owner is stuck, not gone - the owner is asked, not the lead.
        if running
            && !over
            && row.owner_live
            && row.marks.stale
            && !parked
            && !fired_already(fired, ChaseRung::Stuck, id, Some(&stamp(row.task.updated_at)))
        {
            out.push(Crossing {
                rung: ChaseRung::Stuck,
                task_id: Some(id.clone()),
                subject: Some(stamp(row.task.updated_at)),
                seat: row.task.owner.clone(),
                text: format!(
                    "task board: \"{}\" has not changed in {}. What is it waiting on? Park it \
                     with the reason, hand it on, or say it is still moving.",
                    row.task.subject,
                    fmt_secs(row.updated_secs_ago),
                ),
            });
        }
        // Signal 3: a connector saw something after the row's last touch -
        // a merged pull request, a CI verdict, a state change - and the row
        // may now be obsolete or urgent.
        // A read that reported the same head again saw nothing new: the
        // input is measured from `changed_at`, not from the five-minute
        // refresh, so a steady connector does not re-fire the chase.
        let newest_check = row
            .task
            .links
            .iter()
            .filter(|link| link.kind == forge_primitives::tasks::LinkKind::Pr)
            .filter_map(|link| link.last_check.as_ref().map(|check| (link, check)))
            .filter_map(|(link, check)| check.changed_at.map(|at| (link, check, at)))
            .filter(|(_, _, at)| *at > row.task.updated_at)
            .max_by_key(|(_, _, at)| *at);
        if !row.task.status.is_terminal()
            && let Some((link, check, changed_at)) = newest_check
            && !fired_already(fired, ChaseRung::NewInput, id, Some(&stamp(changed_at)))
        {
            out.push(Crossing {
                rung: ChaseRung::NewInput,
                task_id: Some(id.clone()),
                subject: Some(stamp(check.at)),
                seat: row.task.owner.clone(),
                text: format!(
                    "task board: {} changed after \"{}\" was last touched ({}; CI {}). Does the \
                     row need to move?",
                    link.target,
                    row.task.subject,
                    check.state.as_deref().unwrap_or("state unknown"),
                    check.ci.as_deref().unwrap_or("unknown"),
                ),
            });
        }
        // Signal 4, resource: a wait on something outside the board is
        // checked at the cadence.
        if row.marks.overdue
            && !over
            && wait.and_then(|wait| wait.kind) == Some(WaitingKind::Resource)
            && !parked
            && due(fired, ChaseRung::WaitStall, id, None, CHASE_CADENCE_SECS, now)
        {
            out.push(Crossing {
                rung: ChaseRung::WaitStall,
                task_id: Some(id.clone()),
                subject: None,
                seat: Some(lead.clone()),
                text: format!(
                    "task board: \"{}\" has waited {} on a resource - unblock it or change its \
                     plan.",
                    row.task.subject,
                    fmt_secs(row.task.wait_age_secs(now).unwrap_or(row.updated_secs_ago)),
                ),
            });
        }
        // Signal 4, dependency: the chase goes to the BLOCKER's owner, who
        // is the one who can end it.
        if row.marks.overdue
            && wait.and_then(|wait| wait.kind) == Some(WaitingKind::Dependency)
            && !parked
            && let Some(blocker_id) = wait.and_then(|wait| wait.on.as_ref())
            && let Some(blocker) = rows.iter().find(|held| held.task.id == *blocker_id)
            && !blocker.task.status.is_terminal()
            && due(fired, ChaseRung::WaitStall, id, None, CHASE_CADENCE_SECS, now)
        {
            out.push(Crossing {
                rung: ChaseRung::WaitStall,
                task_id: Some(id.clone()),
                subject: None,
                seat: blocker.task.owner.clone(),
                text: format!(
                    "task board: \"{}\" has waited {} on your row \"{}\" - end it, unblock it, \
                     or say why it has to stay open.",
                    row.task.subject,
                    fmt_secs(row.task.wait_age_secs(now).unwrap_or(row.updated_secs_ago)),
                    blocker.task.subject,
                ),
            });
        }
        if row.marks.ready
            && row.task.owner.is_none()
            && row.marks.stale
            && due(fired, ChaseRung::QueueStall, id, None, CHASE_CADENCE_SECS, now)
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
        let died = row.task.status == TaskStatus::Failed;
        let stranded = running && !row.owner_live;
        // The death repeats at the cadence, counted from the death
        // itself: a row that moved and died again is a new occurrence.
        if (died || stranded)
            && due(fired, ChaseRung::Death, id, failed_at(history, id), CHASE_CADENCE_SECS, now)
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
        if row.marks.to_close && due(fired, ChaseRung::EpicClose, id, None, CHASE_CADENCE_SECS, now)
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
            && due(fired, ChaseRung::Retro, &row.task.id, None, CHASE_CADENCE_SECS, now)
            && rows.iter().any(|child| child.task.parent.as_ref() == Some(&row.task.id))
            && rows
                .iter()
                .filter(|child| child.task.parent.as_ref() == Some(&row.task.id))
                .all(|child| child.task.status.is_terminal())
        {
            out.push(Crossing {
                rung: ChaseRung::Retro,
                task_id: Some(row.task.id.clone()),
                subject: None,
                seat: Some(lead.clone()),
                text: retro_prompt(row, rows, history),
            });
        }
    }

    // **A wait on the user is never chased by an agent.** The board shows
    // it, the drive excludes it, and no rung names it: a decision, and a
    // look, are the user's to give when they come back.

    for label in unaccounted {
        // **The log is the whole slate**, this rung's included: the newest
        // naming for the worker, and the cadence is what a repeat waits. A
        // worker whose state cleared had its naming forgotten - the sweep
        // does that, durably - so its next drift is named at once, across a
        // restart too.
        if durable_last_named(fired, label)
            .is_none_or(|at| since_secs(at, now) >= CHASE_CADENCE_SECS)
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

/// The newest firing of one rung for one row and subject, if any: the
/// escalation counts its cadence from the re-plan ask's own firing.
fn last_fired(
    fired: &[TaskChase],
    rung: ChaseRung,
    id: &TaskId,
    subject: Option<&str>,
) -> Option<SystemTime> {
    fired
        .iter()
        .filter(|chase| {
            chase.rung == rung
                && chase.task_id.as_ref() == Some(id)
                && chase.subject.as_deref() == subject
        })
        .map(|chase| chase.at)
        .max()
}

/// What an epic's owner is asked when its children have all stopped: the
/// numbers, computed here and authoritative, and the ask.
fn retro_prompt(epic: &BoardRow, rows: &[BoardRow], history: &[TaskTransition]) -> String {
    let children: Vec<&BoardRow> =
        rows.iter().filter(|row| row.task.parent.as_ref() == Some(&epic.task.id)).collect();
    let worked: u64 = children.iter().map(|child| child.worked_secs).sum();
    let estimated: u64 = children
        .iter()
        .filter_map(|child| child.task.estimate.as_ref())
        .map(|estimate| estimate.secs)
        .sum();
    // Rework is read from the history rather than counted on the row: a
    // child sent back after a wait is the reopened one.
    let reopened = children
        .iter()
        .filter(|child| {
            history.iter().any(|transition| {
                transition.task_id == child.task.id
                    && transition.from == Some(TaskStatus::Waiting)
                    && transition.to == TaskStatus::InProgress
            })
        })
        .count();
    let mut failed = 0usize;
    for child in &children {
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
                // The re-plan ask is a state, not a message alone: the row
                // is parked waiting on its owner before the owner hears, so
                // the board shows the same thing the message says.
                if crossing.rung == ChaseRung::EstimateNudge
                    && let Some(id) = &crossing.task_id
                {
                    self.park_overrun(&project, id);
                }
                let target = ladder(&crossing, &lead, |seat| self.seat_is_live(seat));
                if let Some(target) = &target
                    && deliver.to_seat(target, &crossing.text) == Delivery::Delivered
                {
                    delivered += 1;
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
    /// [`CHASE_INTERVAL_SECS`] that delivers the crossings. The registry
    /// makes a second call a no-op; started once at boot. Mirrors
    /// [`Workspace::start_cron_scheduler`].
    pub fn start_chase_sweep(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("chase_sweep");
        self.supervisor.spawn("chase_sweep", forge_supervise::Policy::backoff(), move || {
            let weak = weak.clone();
            let span = span.clone();
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
                    let now = SystemTime::now();
                    // The retention rides the same beat: a terminal tree
                    // leaves the live set once its window has passed,
                    // without anyone asking it to.
                    let retained = workspace.retain_terminal_rows(now);
                    if !retained.is_empty() {
                        tracing::debug!(
                            target: "forge_workspace::tasks",
                            retained = retained.len(),
                            "terminal task trees archived by retention",
                        );
                    }
                    // A project that opted into a connector refreshes its
                    // pull-request links on the same beat; a project with
                    // none does nothing here.
                    workspace.refresh_connectors(now).await;
                    let delivered = workspace
                        .chase_sweep(&PromptDeliver { workspace: Arc::clone(&workspace) }, now);
                    if delivered > 0 {
                        tracing::debug!(
                            target: "forge_workspace::chase",
                            delivered,
                            "chase sweep delivered",
                        );
                    }
                }
            }
            .instrument(span)
        });
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
pub(crate) struct PromptDeliver {
    pub(crate) workspace: Arc<Workspace>,
}

impl Deliver for PromptDeliver {
    fn to_seat(&self, slot: &SessionSlot, text: &str) -> Delivery {
        let uuid = forge_sdk::request_id::next_forged_prompt_id();
        match self.workspace.dispatch_forged_prompt_under(slot, text.to_owned(), uuid) {
            Ok(()) => Delivery::Delivered,
            Err(error) => Delivery::NoSession(error.to_string()),
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
            kind: forge_primitives::tasks::TaskKind::Finite,
            owner: None,
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
        assert_eq!(sweep(&ws, &deliver, 100 + CHASE_CADENCE_SECS - 1), 0, "quiet inside it");
        assert_eq!(sweep(&ws, &deliver, 100 + CHASE_CADENCE_SECS), 1, "named again at it");
        assert_eq!(sweep(&ws, &deliver, 200 + CHASE_CADENCE_SECS), 0, "quiet again");
        assert_eq!(sweep(&ws, &deliver, 100 + 2 * CHASE_CADENCE_SECS), 1, "and at the next one");
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
        assert_eq!(
            sweep(&ws, &deliver, 100 + CHASE_CADENCE_SECS + 1),
            1,
            "the cadence runs off the logged naming",
        );
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
    fn the_escalation_fires_once_after_the_ask_waits_out_the_cadence() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        // The ask lands at once; the escalation waits the cadence from it.
        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "parked waiting on you"), 1, "{:?}", deliver.seen.lock());
        assert_eq!(says(&deliver, "the re-plan ask is unanswered"), 0, "not yet");
        sweep(&ws, &deliver, 200 + CHASE_CADENCE_SECS);
        assert_eq!(
            says(&deliver, "the re-plan ask is unanswered"),
            1,
            "the lead hears once the ask waited: {:?}",
            deliver.seen.lock(),
        );
        sweep(&ws, &deliver, 200 + 4 * CHASE_CADENCE_SECS);
        assert_eq!(
            says(&deliver, "the re-plan ask is unanswered"),
            1,
            "and the chase stops after that, however long the overrun holds",
        );
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
            kind: Some(WaitingKind::User),
            detail: None,
            on: None,
            verification,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        });
        row.updated_at = epoch(updated);
        ws.seed_test_task(row);
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
            kind: Some(WaitingKind::User),
            detail: Some("backlog at Ved's word".to_owned()),
            on: None,
            verification: false,
            parked: true,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
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

    /// A resource wait parked by design is left alone by the stall chase too;
    /// un-parked, its real age arms the stall and it fires at once (#2472).
    #[test]
    fn a_parked_wait_is_not_stall_chased() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Waiting);
        // The wait's own start, so un-parking restores an age the assertion
        // can name: the row's last edit is no longer what the age counts.
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Resource),
            detail: Some("needs the owner's dashboard".to_owned()),
            on: None,
            verification: false,
            parked: true,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(SystemTime::now()),
        });
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        let deliver = Recording::default();

        assert_eq!(
            sweep(&ws, &deliver, WINDOW + 1),
            0,
            "a parked wait is not a stall: {:?}",
            deliver.seen.lock(),
        );
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::WaitStall),
            "and no WaitStall mark is left for it",
        );

        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            if let Some(wait) = task.waiting_on.as_mut() {
                wait.parked = false;
            }
        })
        .expect("no refusal")
        .expect("the row is there");
        let updated_at = ws
            .tasks_for_project(PROJECT)
            .into_iter()
            .find(|task| task.id == TaskId::from("t-1"))
            .expect("the row survived")
            .updated_at;
        let later = updated_at + Duration::from_secs(WINDOW + 2);
        let later_secs =
            later.duration_since(SystemTime::UNIX_EPOCH).expect("the epoch is before it").as_secs();
        let rows = ws.board_rows(PROJECT, epoch(later_secs), WINDOW);
        let moved = rows.iter().find(|row| row.task.id == TaskId::from("t-1")).expect("the row");
        assert!(!moved.task.waiting_on.as_ref().expect("the wait holds").parked, "un-parked");
        assert!(
            moved.marks.overdue,
            "its real age is the stall again: status={:?} age={} updated={:?} now={:?} wait={:?}",
            moved.task.status,
            moved.updated_secs_ago,
            moved.task.updated_at,
            epoch(later_secs),
            moved.task.waiting_on,
        );
        assert_eq!(sweep(&ws, &deliver, later_secs), 1, "un-parked, the stall fires");
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 1, "one WaitStall: {seen:?}");
        assert_eq!(
            seen[0].1,
            "task board: \"subject t-1\" has waited 4h on a resource - unblock it or change its plan.",
            "the WaitStall names the row, its age and the kind it waits on",
        );
    }

    /// **A wait on the user is never chased by an agent.** A decision and a
    /// look are the user's to give when they come back: however old, however
    /// many, no nudge names them, no rung is logged for them, and a live lead
    /// is not asked to confirm them.
    #[test]
    fn a_wait_on_the_user_is_never_chased() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        waiting_on_user(&ws, "t-1", 0, false);
        waiting_on_user(&ws, "t-2", 0, true);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, WINDOW + 1), 0, "nothing is delivered for a user wait");
        assert!(deliver.seen.lock().is_empty(), "no seat is asked to act on it");
        assert_eq!(sweep(&ws, &deliver, WINDOW + 3600), 0, "and it stays quiet on the cadence");
        assert!(
            ws.chases_for_project(PROJECT).is_empty(),
            "no rung is logged for a wait on the user: {:?}",
            ws.chases_for_project(PROJECT),
        );
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
            seen[0].1.contains("It is parked waiting on you: rescope it, split it, or close it"),
            "the ask names the re-plan the overrun owes: {}",
            seen[0].1,
        );
        drop(seen);
        assert_eq!(
            ws.tasks_for_project(PROJECT)[0].status,
            TaskStatus::Waiting,
            "the ask is a state: the row is parked waiting on its owner",
        );

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

    /// Standing work is checked when its cadence comes due, counted from the
    /// row's last touch - a check IS a touch, so touching the row re-arms it.
    #[test]
    fn a_standing_row_is_checked_when_its_cadence_comes_due() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        let mut row = task("t-1", TaskStatus::InProgress);
        row.kind = forge_primitives::tasks::TaskKind::Standing;
        row.cadence = Some(Estimate { words: "1h".to_owned(), secs: 3_600 });
        row.owner = Some(owner.clone());
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 3_599), 0, "not due yet");
        assert_eq!(sweep(&ws, &deliver, 3_601), 1, "due: the owner is asked");
        let seen = deliver.seen.lock();
        assert_eq!(seen[0].0, owner, "the check goes to the row's owner");
        assert!(seen[0].1.contains("due for its check (every 1h)"), "{}", seen[0].1);
        drop(seen);
        assert_eq!(sweep(&ws, &deliver, 4_000), 0, "one ask per touch");

        // The owner checks it: the touch re-arms the cadence.
        ws.with_tasks_mut(|tasks| {
            let row = tasks.iter_mut().find(|task| task.id == TaskId::from("t-1")).expect("row");
            row.updated_at = epoch(4_000);
        });
        assert_eq!(sweep(&ws, &deliver, 4_100), 0, "a fresh touch is not due");
        assert_eq!(sweep(&ws, &deliver, 7_601), 1, "the next cadence is a new check");
    }

    /// An in-progress row with a LIVE owner and no movement is stuck, not
    /// gone: the owner is asked, and a new touch re-arms the ask.
    #[test]
    fn a_stuck_row_is_asked_about() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        let mut row = task("t-1", TaskStatus::InProgress);
        row.owner = Some(owner.clone());
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, WINDOW), 0, "inside the band, no ask");
        assert_eq!(sweep(&ws, &deliver, WINDOW + 1), 1, "past it, the owner hears");
        let seen = deliver.seen.lock();
        assert_eq!(seen[0].0, owner);
        assert!(seen[0].1.contains("has not changed in"), "{}", seen[0].1);
        drop(seen);
        assert_eq!(sweep(&ws, &deliver, WINDOW + 1_000), 0, "one ask per last touch");
    }

    /// A connector's look at a link AFTER the row's last touch is new
    /// input: the owner is asked whether the row needs to move, once per
    /// check - a fresher check is a new fact.
    #[test]
    fn a_check_after_the_last_touch_is_new_input() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        let check_at = epoch(2_000);
        let mut row = task("t-1", TaskStatus::Pending);
        row.owner = Some(owner.clone());
        row.updated_at = epoch(1_000);
        row.links.push(forge_primitives::tasks::TaskLink {
            kind: forge_primitives::tasks::LinkKind::Pr,
            label: None,
            target: "https://example.invalid/pull/7".to_owned(),
            state: Some("merged".to_owned()),
            last_check: Some(forge_primitives::tasks::LinkCheck {
                at: check_at,
                changed_at: Some(check_at),
                state: Some("merged".to_owned()),
                head: Some("abc123".to_owned()),
                ci: Some("passing".to_owned()),
                problem: None,
            }),
            added_at: epoch(0),
        });
        ws.seed_test_task(row);
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 3_000), 1, "the check is news");
        let seen = deliver.seen.lock();
        assert_eq!(seen[0].0, owner, "the owner is asked");
        assert!(seen[0].1.contains("pull/7"), "the link is named: {}", seen[0].1);
        assert!(seen[0].1.contains("merged; CI passing"), "with what it said: {}", seen[0].1);
        drop(seen);
        assert_eq!(sweep(&ws, &deliver, 3_100), 0, "one ask per check");

        // A check that found something DIFFERENT is a new fact: both the
        // read and the content move.
        ws.with_tasks_mut(|tasks| {
            let row = tasks.iter_mut().find(|task| task.id == TaskId::from("t-1")).expect("row");
            let check = row.links[0].last_check.as_mut().expect("check");
            check.at = epoch(3_200);
            check.changed_at = Some(epoch(3_200));
            check.state = Some("open".to_owned());
        });
        assert_eq!(sweep(&ws, &deliver, 3_300), 1, "the fresher check is asked about");
    }

    /// A dependency wait's chase goes to the BLOCKER's owner, never to the
    /// lead by default: the blocker is the one who can end it.
    #[test]
    fn a_dependency_chase_goes_to_the_blocker() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let blocker_slot = SessionSlot::worker("TestOrg", PROJECT, "w-blocker");
        let _blocker_rx = ws.install_testing_stub(&blocker_slot);
        let mut blocker = task("t-blocker", TaskStatus::InProgress);
        blocker.owner = Some(blocker_slot.clone());
        blocker.updated_at = epoch(0);
        ws.seed_test_task(blocker);
        let mut waiter = task("t-waiter", TaskStatus::Waiting);
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
            since: Some(epoch(0)),
        });
        waiter.updated_at = epoch(0);
        ws.seed_test_task(waiter);
        let deliver = Recording::default();

        sweep(&ws, &deliver, WINDOW + 1);
        let seen = deliver.seen.lock();
        let stalled = seen
            .iter()
            .find(|(_, text)| text.contains("on your row"))
            .unwrap_or_else(|| panic!("no dependency chase: {seen:?}"));
        assert_eq!(stalled.0, blocker_slot, "the blocker's owner hears it");
        assert!(
            stalled.1.contains("\"subject t-waiter\""),
            "naming the waiting row: {}",
            stalled.1
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
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
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
                    last_check: None,
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
        let drawn: Vec<SessionUpdate> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|frame| std::sync::Arc::unwrap_or_clone(frame.update))
            .collect();
        let frame = drawn.iter().find_map(|update| match update {
            SessionUpdate::ChatAppended { msg, .. } => Some(msg),
            _ => None,
        });
        let Some(Message::User { message, uuid, .. }) = frame else {
            panic!("the chase's delivery forges its words as the turn; got {drawn:?}")
        };
        assert_eq!(
            uuid.as_deref(),
            Some(wired.as_str()),
            "the frame is stamped with the id the prompt runs under",
        );
        assert!(
            matches!(
                message.content.first(),
                Some(forge_primitives::ContentBlock::Text { text: said, .. }) if said == text
            ),
            "the frame carries the prose the model received: {message:?}",
        );
    }

    /// **The chase no longer announces the board on a no-op pass** (#2540).
    /// The client's 30 s board beat moves the time-derived marks; a
    /// `TasksChanged` for an unchanged set made every board and home
    /// subscriber re-read its whole subject. A pass with nothing to deliver
    /// must put no task set on the stream.
    #[tokio::test(start_paused = true)]
    async fn a_noop_chase_pass_announces_no_tasks_changed() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project(PROJECT, "/tmp/tp-chase");
        ws.start_chase_sweep();
        tokio::time::advance(Duration::from_secs(CHASE_INTERVAL_SECS + 1)).await;
        // The spawned sweep runs on the runtime; give it the tick and the
        // updates it would have put on the stream.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(frame) = rx.try_recv() {
            assert!(
                !matches!(&*frame.update, SessionUpdate::TasksChanged { .. }),
                "a no-op chase pass announced a task set: {:?}",
                frame.update,
            );
        }
    }
}
