//! The chase sweep: the pass that makes the board ask instead of report.
//!
//! Every pass derives crossings from the board's own state and the chase
//! log, on four signals together: the row's KIND (finite work owes a
//! re-plan on overrun; standing work is checked on its cadence), MOVEMENT
//! (no touch past the band with a live owner is stuck), NEW INPUT (a
//! connector saw something after the last touch) and the WAIT's kind (an
//! ask on the user is never chased, a resource wait is checked, a dependency
//! chase goes to the blocker's owner).
//!
//! **One cadence, not three.** An in-progress finite overrun asks once -
//! parking the row - and the lead hears the escalation once per estimate,
//! following the ask to the row still being worked and to the one still
//! sitting in the park the ask wrote. The nudge arms only while the row is
//! being worked; the escalation, while it is being worked or still in that
//! park. Any other waiting or pending row arms neither overrun signal, and
//! its wait's own rung is what applies - except to the chase's park, where
//! the escalation follows its ask. The stall, death, close, retro and
//! unaccounted rungs repeat at `CHASE_CADENCE_SECS` while their state
//! holds, and the movement and new-input namings fire once per touch or
//! check. **The chase log is the whole slate**: every naming is deduped by
//! it, so a restart re-reads it rather than re-firing, and a state that
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
    By, ChaseRung, Task, TaskChase, TaskId, TaskStatus, TaskTransition, WaitingKind,
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

/// Whether `rung` already fired for `id` and this subject - an estimate
/// restated to a different duration changes the subject, which arms the
/// nudge again.
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

/// Whether an overrun row is one the escalation can name: it is being
/// worked, or still sitting in the chase's own park, and it is open.
/// `over` is its worked time past the estimate. The park is the chase's
/// only when its marker stands and the owner has not parked it themselves.
fn escalation_armed(task: &Task, over: bool) -> bool {
    if task.status.is_terminal() || !over {
        return false;
    }
    let wait = task.waiting_on.as_ref();
    let parked = wait.is_some_and(|wait| wait.parked);
    let chase_park = wait.is_some_and(|wait| wait.replan) && !parked;
    task.status == TaskStatus::InProgress || chase_park
}

/// The crossings one project's board implies, given what already fired.
///
/// **Four signals, together rather than blindly.** A row's KIND decides
/// what an overrun means (finite work owes a re-plan; standing work is
/// checked on its cadence), MOVEMENT says whether the owner is working or
/// stuck, NEW INPUT - a connector's look at a link after the last touch -
/// may make the row obsolete or urgent, and the WAIT's kind decides who
/// hears: an ask on the user is never chased, a resource wait is checked,
/// and a dependency chase goes to the blocker's owner.
///
/// An in-progress finite overrun asks once, parking the row (the sweep
/// does the write); the lead hears the escalation once per estimate once
/// the ask has had its cadence, whether the row is still being worked or
/// still sits in the park the ask wrote, and then the chase stops for that
/// estimate. The nudge never fires on a waiting or pending row, and no
/// wait the chase did not write is ever escalated.
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
        // Worked time past the estimate. The nudge fires only on a running
        // row - a waiting or pending row is not being worked, so its clock
        // is history rather than an ongoing overrun - but the fact is read
        // status-independent here: the escalation follows its ask into the
        // park the chase wrote, which is a waiting row.
        let over = row.task.kind == forge_primitives::tasks::TaskKind::Finite
            && estimate.is_some_and(|estimate| row.worked_secs > estimate.secs);
        let wait = row.task.waiting_on.as_ref();
        let parked = wait.is_some_and(|wait| wait.parked);
        // The chase's own park, where the escalation follows its ask - and
        // only while the owner has not parked that wait itself.
        let chase_park = wait.is_some_and(|wait| wait.replan) && !parked;

        // Signal 1a: in-progress finite work past its estimate owes a
        // re-plan. A waiting, pending or terminal row is not being worked,
        // so its numbers are history rather than an overrun. The ask is one
        // per estimate and parks the row; the sweep writes the park.
        if running
            && let Some(estimate) = estimate.filter(|_| over)
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
        // The escalation: once per estimate, once the ask has had its
        // cadence. It follows the ask to a row still being worked and to one
        // still sitting in the park the ask wrote; a wait raised by anyone
        // else - the user's, a dependency, an owner's park - is never
        // escalated. An estimate restated upward while the row sits in that
        // park is the re-plan the ask wanted: `over` goes false and the
        // escalation stops. Then this estimate is done being chased.
        if escalation_armed(&row.task, over)
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
        // checked at the cadence. The chase's own park is exempt while its
        // ask still names the current estimate (the escalation follows that
        // ask) and while the restate has covered the worked time (the row is
        // no longer over). A park left over by a restate has no ask to
        // escalate, so the stall is the rung that surfaces it.
        let asked_this_estimate = estimate.is_some_and(|estimate| {
            last_fired(fired, ChaseRung::EstimateNudge, id, Some(&estimate.secs.to_string()))
                .is_some()
        });
        let park_is_quiet = chase_park && (!over || asked_this_estimate);
        if row.marks.overdue
            && !park_is_quiet
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

    /// Deliver one crossing, after writing the state its ask names.
    ///
    /// The crossings came from a board read, and the row may have moved
    /// since: the estimate nudge is a park before it is a message, and the
    /// escalation is true only while its row still holds the ask. A
    /// crossing whose state no longer holds is neither sent nor recorded,
    /// so an ask never names a park the board does not show and a stale
    /// escalation cannot age a new one. Returns whether a seat was reached.
    fn deliver_crossing(
        &self,
        deliver: &dyn Deliver,
        lead: &SessionSlot,
        project: &str,
        crossing: &Crossing,
        now: SystemTime,
    ) -> bool {
        match crossing.rung {
            ChaseRung::EstimateNudge => {
                let parked = crossing
                    .task_id
                    .as_ref()
                    .is_some_and(|id| self.park_overrun(project, id).is_some());
                if !parked {
                    return false;
                }
            }
            ChaseRung::Escalate => {
                let holds = crossing.task_id.as_ref().is_some_and(|id| {
                    self.escalation_still_holds(project, id, crossing.subject.as_deref(), now)
                });
                if !holds {
                    return false;
                }
            }
            _ => {}
        }
        let target = ladder(crossing, lead, |seat| self.seat_is_live(seat));
        let delivered = target
            .as_ref()
            .is_some_and(|target| deliver.to_seat(target, &crossing.text) == Delivery::Delivered);
        self.record_chase(&TaskChase {
            task_id: crossing.task_id.clone(),
            subject: crossing.subject.clone(),
            project_name: project.to_owned(),
            rung: crossing.rung,
            at: now,
        });
        delivered
    }

    /// Whether the row still holds what an escalation crossing names: it is
    /// being worked, or still waiting in the chase's own park, its worked
    /// time still passes the estimate, and that is the estimate the
    /// crossing's subject names. The crossing came from a board read, so
    /// the row may have moved since.
    fn escalation_still_holds(
        &self,
        project: &str,
        id: &TaskId,
        subject: Option<&str>,
        now: SystemTime,
    ) -> bool {
        let Some(subject) = subject else { return false };
        let rows = self.board_rows(project, now, crate::board::DEFAULT_STALE_SECS);
        let Some(row) = rows.iter().find(|row| row.task.id == *id) else { return false };
        let Some(estimate) = row.task.estimate.as_ref().filter(|estimate| estimate.secs > 0) else {
            return false;
        };
        if estimate.secs.to_string() != subject {
            return false;
        }
        let over = row.task.kind == forge_primitives::tasks::TaskKind::Finite
            && row.worked_secs > estimate.secs;
        escalation_armed(&row.task, over)
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
                if self.deliver_crossing(deliver, &lead, &project, &crossing, now) {
                    delivered += 1;
                }
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
/// **A board delivery is queue-first like every other.** The prompt is
/// recorded in the core's queue and its row draws at the CLI's `started`, so
/// it enters the chat only when it is sent - not forged at dispatch, retracted
/// into the pile, and redrawn at the start, which is the bounce this cut
/// removes.
///
/// **The id keeps the machinery's prefix.** A reloaded transcript row is read
/// back by that prefix alone (`forge_nudge_`), so a board row read from disk
/// still draws as forge's own words rather than as the reader's.
///
/// The seat is already known live by the ladder, so a failed dispatch here is
/// a race, not a reason to spawn; the route's own refusal draws the words.
pub(crate) struct PromptDeliver {
    pub(crate) workspace: Arc<Workspace>,
}

impl Deliver for PromptDeliver {
    fn to_seat(&self, slot: &SessionSlot, text: &str) -> Delivery {
        let uuid = forge_sdk::request_id::next_forged_prompt_id();
        match self.workspace.dispatch_workspace_prompt_under(
            slot,
            text.to_owned(),
            crate::protocol::PromptSource::Board,
            uuid,
        ) {
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

    /// The fixture is over its estimate at `now`, so a rung that declines
    /// is declining on its own condition rather than on a missing fact.
    fn assert_over_at(ws: &Workspace, id: &str, now: u64) {
        let rows = ws.board_rows(PROJECT, epoch(now), WINDOW);
        let held = rows.iter().find(|held| held.task.id == TaskId::from(id)).expect("row");
        assert!(
            held.worked_secs > held.task.estimate.as_ref().expect("estimate").secs,
            "{id}: the fixture is over its estimate at {now}",
        );
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

    /// The drifted worker's naming repeats at the 30-minute cadence while
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

    /// The overrun nudge is once per estimate: a row moved back in progress
    /// with a restated estimate of a different duration arms it again.
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
        // The owner moves it back in progress with a restated estimate; the
        // row is still being worked and still over the new one.
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::InProgress;
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

    /// The escalation fires once per estimate, a cadence after the ask,
    /// while the row is still being worked.
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
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .any(|chase| chase.rung == ChaseRung::EstimateNudge),
            "the ask is logged, not only delivered: {:?}",
            ws.chases_for_project(PROJECT),
        );
        // The ask parked it, and the owner moves it back in progress without
        // re-planning: the row is being worked and still over.
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::InProgress;
        })
        .expect("no refusal")
        .expect("the row is there");
        // One second inside the cadence is still too soon.
        sweep(&ws, &deliver, 200 + CHASE_CADENCE_SECS - 1);
        assert_eq!(
            says(&deliver, "the re-plan ask is unanswered"),
            0,
            "inside the cadence: {:?}",
            deliver.seen.lock(),
        );
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::Escalate),
            "no escalation is logged inside the cadence: {:?}",
            ws.chases_for_project(PROJECT),
        );
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

    /// The chase's own park is not an answer: the nudge parks an overrun row
    /// waiting on its owner, and if nobody picks it up the lead hears the
    /// escalation at the cadence, once per estimate. The park is not chased
    /// as a resource stall - the chase does not chase its own park.
    #[test]
    fn an_unanswered_auto_park_escalates_to_the_lead_once() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        assert_eq!(sweep(&ws, &deliver, 200), 1, "the owner is asked");
        let held = ws
            .tasks_for_project(PROJECT)
            .into_iter()
            .find(|task| task.id == TaskId::from("t-1"))
            .expect("the row is there");
        assert_eq!(held.status, TaskStatus::Waiting, "the ask parked it");
        let wait = held.waiting_on.as_ref().expect("a wait holds");
        assert!(wait.replan, "the park is the chase's own");
        assert!(!wait.parked, "not the owner's deferral");
        assert_eq!(says(&deliver, "the re-plan ask is unanswered"), 0, "not yet");
        // The park closed the row's in-progress interval, so the row is
        // still over when the escalation's cadence comes due.
        assert_over_at(&ws, "t-1", 200 + CHASE_CADENCE_SECS);
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .any(|chase| chase.rung == ChaseRung::EstimateNudge),
            "the ask is logged, not only delivered: {:?}",
            ws.chases_for_project(PROJECT),
        );

        // Inside the cadence the ask is still fresh: nothing escalates.
        assert_eq!(sweep(&ws, &deliver, 200 + CHASE_CADENCE_SECS - 1), 0, "not yet");
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::Escalate),
            "no escalation is logged inside the cadence: {:?}",
            ws.chases_for_project(PROJECT),
        );
        assert_eq!(
            sweep(&ws, &deliver, 200 + CHASE_CADENCE_SECS),
            1,
            "the ask went unanswered: the lead hears once",
        );
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(seat, text)| seat == &SessionSlot::lead("TestOrg", PROJECT)
                && text.contains("the re-plan ask is unanswered")),
            "the escalation reaches the lead: {seen:?}",
        );
        assert!(
            !seen.iter().any(|(_, text)| text.contains("has waited")),
            "the chase does not chase its own park as a resource stall: {seen:?}",
        );
        drop(seen);
        assert_eq!(
            sweep(&ws, &deliver, 200 + 4 * CHASE_CADENCE_SECS),
            0,
            "once per estimate, however long the park holds",
        );
    }

    /// A restate that leaves a chase park still over has no ask to
    /// escalate: the resource rung surfaces it at its cadence, once.
    #[test]
    fn a_restated_chase_park_still_over_surfaces_on_the_resource_rung() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "parked waiting on you"), 1, "{:?}", deliver.seen.lock());
        // The owner restates the estimate below the frozen worked time and
        // leaves the row in the park: still over, but the ask names the old
        // estimate, so neither the nudge nor the escalation can fire.
        let now = 200 + WINDOW + 1;
        let rows = ws.board_rows(PROJECT, epoch(now), WINDOW);
        let held = rows.iter().find(|held| held.task.id == TaskId::from("t-1")).expect("row");
        let restated = held.worked_secs - 1;
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.estimate = Some(Estimate { words: "most of it".to_owned(), secs: restated });
        })
        .expect("no refusal")
        .expect("the row is there");
        assert_over_at(&ws, "t-1", now);
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .any(|chase| chase.rung == ChaseRung::EstimateNudge),
            "the old ask is logged",
        );

        assert_eq!(
            sweep(&ws, &deliver, now),
            1,
            "the stall surfaces the stranded park: {:?}",
            deliver.seen.lock(),
        );
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(seat, text)| seat == &SessionSlot::lead("TestOrg", PROJECT)
                && text.contains("on a resource")),
            "the resource rung reaches the lead: {seen:?}",
        );
        drop(seen);
        assert_eq!(
            says(&deliver, "the re-plan ask is unanswered"),
            0,
            "the old ask cannot escalate the new estimate",
        );
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::Escalate),
            "no escalation is logged: {:?}",
            ws.chases_for_project(PROJECT),
        );
        // Inside the stall's cadence it does not repeat.
        assert_eq!(sweep(&ws, &deliver, now + CHASE_CADENCE_SECS - 1), 0, "once per cadence");
    }

    /// A chase park the restate has covered is quiet: the wait's age alone
    /// must not draw the stall the marker exists to prevent.
    #[test]
    fn a_chase_park_that_is_no_longer_over_draws_no_stall() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Waiting);
        row.estimate = Some(Estimate { words: "1m".to_owned(), secs: 60 });
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Resource),
            detail: Some(
                "over its estimate: re-plan it - rescope, split, or close it with the reason"
                    .to_owned(),
            ),
            on: None,
            verification: false,
            parked: false,
            replan: true,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(0)),
        });
        row.updated_at = epoch(30);
        ws.seed_test_task(row);
        append_transition(&ws, "t-1", TaskStatus::Pending, TaskStatus::InProgress, 10);
        append_transition(&ws, "t-1", TaskStatus::InProgress, TaskStatus::Waiting, 30);
        // Worked 20s against a 60s estimate: not over, but the wait is past
        // the window, so only the marker keeps the stall away.
        let rows = ws.board_rows(PROJECT, epoch(WINDOW + 1), WINDOW);
        let held = rows.iter().find(|held| held.task.id == TaskId::from("t-1")).expect("row");
        assert!(held.marks.overdue, "the wait is past the window");
        assert!(
            held.worked_secs <= held.task.estimate.as_ref().expect("estimate").secs,
            "the estimate covers the work",
        );
        let deliver = Recording::default();

        assert_eq!(
            sweep(&ws, &deliver, WINDOW + 1),
            0,
            "the marker keeps it quiet: {:?}",
            deliver.seen.lock(),
        );
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::WaitStall),
            "no stall is logged: {:?}",
            ws.chases_for_project(PROJECT),
        );
    }

    /// A restate that covers the worked time clears the chase park's
    /// overrun detail: the board must not keep claiming an overrun the new
    /// estimate retired.
    #[test]
    fn a_restate_that_covers_the_work_clears_the_parks_overrun_detail() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        sweep(&ws, &deliver, 200);
        let held = ws
            .tasks_for_project(PROJECT)
            .into_iter()
            .find(|task| task.id == TaskId::from("t-1"))
            .expect("the row is there");
        let wait = held.waiting_on.as_ref().expect("a wait holds");
        assert!(
            wait.detail.as_deref().is_some_and(|detail| detail.contains("over its estimate")),
            "the park starts with the overrun detail: {:?}",
            wait.detail,
        );
        let now = 200 + WINDOW + 1;
        let rows = ws.board_rows(PROJECT, epoch(now), WINDOW);
        let held = rows.iter().find(|held| held.task.id == TaskId::from("t-1")).expect("row");
        let worked = held.worked_secs;
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.estimate =
                Some(Estimate { words: "the rest of the day".to_owned(), secs: worked + 1 });
        })
        .expect("no refusal")
        .expect("the row is there");
        let held = ws
            .tasks_for_project(PROJECT)
            .into_iter()
            .find(|task| task.id == TaskId::from("t-1"))
            .expect("the row is there");
        let wait = held.waiting_on.as_ref().expect("a wait holds");
        assert!(wait.replan, "the park stands");
        assert_eq!(wait.detail, None, "the retired overrun is no longer drawn");
    }

    /// The escalation follows only the ask the chase wrote: a wait the user
    /// owns is never escalated, however far past its estimate the row's
    /// worked time is and however old the logged ask is. The ask is the real
    /// nudge, so the log carries the subject the production chase armed.
    #[test]
    fn a_user_parked_wait_never_escalates() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        // The real ask lands and parks the row.
        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "parked waiting on you"), 1, "{:?}", deliver.seen.lock());
        // The owner converts the wait to the user's own backlog.
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.waiting_on = Some(Waiting {
                kind: Some(WaitingKind::User),
                detail: Some("backlog at Ved's word".to_owned()),
                on: None,
                verification: false,
                parked: true,
                replan: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: Some(epoch(200)),
            });
        })
        .expect("no refusal")
        .expect("the row is there");
        assert_over_at(&ws, "t-1", 200 + CHASE_CADENCE_SECS);
        let before = deliver.seen.lock().len();

        sweep(&ws, &deliver, 200 + CHASE_CADENCE_SECS);
        assert_eq!(
            says(&deliver, "the re-plan ask is unanswered"),
            0,
            "a user wait is never escalated: {:?}",
            deliver.seen.lock(),
        );
        assert_eq!(deliver.seen.lock().len(), before, "the owner's wait is left alone");
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::Escalate),
            "no escalation is logged for a wait that is not the chase's: {:?}",
            ws.chases_for_project(PROJECT),
        );
    }

    /// A dependency wait is never escalated off an overrun ask either: the
    /// blocker's owner hears the wait's own rung instead, and that positive
    /// control keeps the silence from being a missing chase.
    #[test]
    fn a_dependency_wait_never_escalates() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-waiter", Some("w-1"));
        let deliver = Recording::default();

        // The real ask lands and parks the row.
        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "parked waiting on you"), 1, "{:?}", deliver.seen.lock());
        // The owner converts the wait to a dependency on another row, kept
        // fresh so the blocker's own stuck rung stays out of the count.
        let blocker_slot = SessionSlot::worker("TestOrg", PROJECT, "w-blocker");
        let _blocker_rx = ws.install_testing_stub(&blocker_slot);
        let mut blocker = task("t-blocker", TaskStatus::InProgress);
        blocker.owner = Some(blocker_slot.clone());
        blocker.updated_at = epoch(200 + CHASE_CADENCE_SECS - 60);
        ws.seed_test_task(blocker);
        ws.update_task(PROJECT, &TaskId::from("t-waiter"), By::System, |task| {
            task.waiting_on = Some(Waiting {
                kind: Some(WaitingKind::Dependency),
                detail: None,
                on: Some(TaskId::from("t-blocker")),
                verification: false,
                parked: false,
                replan: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: Some(epoch(200)),
            });
        })
        .expect("no refusal")
        .expect("the row is there");
        assert_over_at(&ws, "t-waiter", 200 + CHASE_CADENCE_SECS);
        let before = deliver.seen.lock().len();

        assert_eq!(
            sweep(&ws, &deliver, 200 + CHASE_CADENCE_SECS),
            1,
            "the blocker hears the wait's own rung: {:?}",
            deliver.seen.lock(),
        );
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), before + 1, "exactly one more message");
        let (seat, text) = seen.last().expect("the message");
        assert_eq!(seat, &blocker_slot, "the blocker's owner hears it");
        assert!(text.contains("on your row"), "the wait rung names the blocker: {text}");
        drop(seen);
        assert_eq!(says(&deliver, "the re-plan ask is unanswered"), 0, "and no escalation is said");
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::Escalate),
            "no escalation is logged for a dependency wait: {:?}",
            ws.chases_for_project(PROJECT),
        );
    }

    /// An owner can park the chase's own park as their deferral. The marker
    /// survives, and the owner's park silences the escalation and the stall.
    #[test]
    fn an_owner_parked_chase_park_never_escalates() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        // The real ask lands and parks the row.
        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "parked waiting on you"), 1, "{:?}", deliver.seen.lock());
        // The owner parks the chase's own wait as their deferral.
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            if let Some(wait) = task.waiting_on.as_mut() {
                wait.parked = true;
            }
        })
        .expect("no refusal")
        .expect("the row is there");
        let held = ws
            .tasks_for_project(PROJECT)
            .into_iter()
            .find(|task| task.id == TaskId::from("t-1"))
            .expect("the row is there");
        let wait = held.waiting_on.as_ref().expect("a wait holds");
        assert!(wait.replan, "the marker survives the owner's park");
        assert!(wait.parked, "the owner's park holds");
        assert_over_at(&ws, "t-1", 200 + CHASE_CADENCE_SECS);
        let before = deliver.seen.lock().len();

        assert_eq!(
            sweep(&ws, &deliver, 200 + CHASE_CADENCE_SECS),
            0,
            "the owner's park is left alone: {:?}",
            deliver.seen.lock(),
        );
        assert_eq!(deliver.seen.lock().len(), before, "nothing more is said");
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::Escalate),
            "no escalation is logged for an owner-parked park: {:?}",
            ws.chases_for_project(PROJECT),
        );
    }

    /// A terminal row over its estimate is never chased: a closed row's
    /// numbers are history. A failed row still draws its death.
    #[test]
    fn a_terminal_row_over_its_estimate_is_not_chased() {
        for status in [TaskStatus::Completed, TaskStatus::Canceled, TaskStatus::Failed] {
            let dir = tempdir().expect("tempdir");
            let ws = fixture(dir.path());
            let _lead_rx = live_lead(&ws);
            let mut row = task("t-1", status);
            row.estimate = Some(Estimate { words: "10s".to_owned(), secs: 10 });
            row.updated_at = epoch(100);
            ws.seed_test_task(row);
            append_transition(&ws, "t-1", TaskStatus::Pending, TaskStatus::InProgress, 10);
            append_transition(&ws, "t-1", TaskStatus::InProgress, status, 100);
            let rows = ws.board_rows(PROJECT, epoch(200), WINDOW);
            let held = rows.iter().find(|held| held.task.id == TaskId::from("t-1")).expect("row");
            assert!(
                held.worked_secs > held.task.estimate.as_ref().expect("estimate").secs,
                "{status:?}: the fixture is over its estimate",
            );
            let deliver = Recording::default();

            sweep(&ws, &deliver, 200);
            assert_eq!(
                says(&deliver, "estimate"),
                0,
                "{status:?}: no estimate text is delivered: {:?}",
                deliver.seen.lock(),
            );
            assert!(
                ws.chases_for_project(PROJECT)
                    .iter()
                    .all(|chase| chase.rung != ChaseRung::EstimateNudge),
                "{status:?}: no nudge is logged for a terminal row: {:?}",
                ws.chases_for_project(PROJECT),
            );
            if status == TaskStatus::Failed {
                assert_eq!(says(&deliver, "failed"), 1, "the death rung is the only delivery");
            } else {
                assert_eq!(deliver.seen.lock().len(), 0, "{status:?}: nothing is delivered");
            }

            // The ask landed while the row was being worked and the row
            // closed before the cadence ran out: the escalation reads the
            // logged ask.
            ws.record_chase(&TaskChase {
                task_id: Some(TaskId::from("t-1")),
                subject: Some("10".to_owned()),
                project_name: PROJECT.to_owned(),
                rung: ChaseRung::EstimateNudge,
                at: epoch(100),
            });
            assert!(
                ws.chases_for_project(PROJECT).iter().any(|chase| {
                    chase.rung == ChaseRung::EstimateNudge
                        && chase.task_id == Some(TaskId::from("t-1"))
                        && chase.subject.as_deref() == Some("10")
                }),
                "{status:?}: the logged ask is the escalation's precondition",
            );
            sweep(&ws, &deliver, 201 + CHASE_CADENCE_SECS);
            assert_eq!(
                says(&deliver, "estimate"),
                0,
                "{status:?}: no estimate text after the cadence: {:?}",
                deliver.seen.lock(),
            );
            assert!(
                ws.chases_for_project(PROJECT)
                    .iter()
                    .all(|chase| chase.rung != ChaseRung::Escalate),
                "{status:?}: no escalation is logged for a terminal row: {:?}",
                ws.chases_for_project(PROJECT),
            );
        }
    }

    /// The park refuses a row the owner already moved off `in_progress`:
    /// the board read and the park write are separate, so this guard keeps
    /// the chase from stamping its marker onto a wait it did not write.
    #[test]
    fn park_overrun_leaves_a_row_that_is_not_in_progress_alone() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut row = task("t-1", TaskStatus::Waiting);
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: Some("backlog at Ved's word".to_owned()),
            on: None,
            verification: false,
            parked: true,
            replan: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(30)),
        });
        row.updated_at = epoch(30);
        ws.seed_test_task(row);

        assert!(
            ws.park_overrun(PROJECT, &TaskId::from("t-1")).is_none(),
            "a row off in_progress is left alone",
        );
        let held = ws
            .tasks_for_project(PROJECT)
            .into_iter()
            .find(|task| task.id == TaskId::from("t-1"))
            .expect("the row is there");
        assert_eq!(held.status, TaskStatus::Waiting, "the owner's state stands");
        let wait = held.waiting_on.as_ref().expect("a wait holds");
        assert_eq!(wait.kind, Some(WaitingKind::User), "the owner's wait kind stands");
        assert!(wait.parked, "the owner's park stands");
        assert!(!wait.replan, "and it is not marked as the chase's");
        assert!(
            ws.history_for_project(PROJECT).is_empty(),
            "a refused park writes no history: {:?}",
            ws.history_for_project(PROJECT),
        );
    }

    /// An in-progress row is parked, and the wait says the chase wrote it.
    #[test]
    fn park_overrun_parks_an_in_progress_row() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        running_over_estimate(&ws, "t-1", None);

        let parked = ws.park_overrun(PROJECT, &TaskId::from("t-1")).expect("the row is parked");
        assert_eq!(parked.status, TaskStatus::Waiting, "the park moved it");
        let wait = parked.waiting_on.as_ref().expect("a wait holds");
        assert_eq!(wait.kind, Some(WaitingKind::Resource), "the owner must act");
        assert!(wait.replan, "the chase wrote it");
        assert!(!wait.parked, "not the owner's deferral");
        let history = ws.history_for_project(PROJECT);
        let recorded = history
            .iter()
            .find(|transition| {
                transition.task_id == TaskId::from("t-1") && transition.to == TaskStatus::Waiting
            })
            .expect("the park is in the history");
        assert_eq!(recorded.from, Some(TaskStatus::InProgress), "it closes the worked interval");
        assert_eq!(recorded.by, By::System, "the core wrote the park");
        let stored = {
            let guard = ws.db.lock();
            crate::store::tasks::list(guard.as_ref().expect("db")).expect("the store reads")
        };
        let stored = stored
            .iter()
            .find(|task| task.id == TaskId::from("t-1"))
            .expect("the parked row is persisted");
        assert_eq!(stored.status, TaskStatus::Waiting, "the store holds the park");
        assert!(stored.waiting_on.as_ref().expect("a wait holds").replan, "and its marker");
    }

    /// The nudge is a park before it is a message: when the row moved off
    /// `in_progress` between the board read and the write, the crossing is
    /// neither sent nor recorded, so no ask names a park the board lacks.
    #[test]
    fn a_nudge_crossing_is_dropped_when_the_row_is_no_longer_in_progress() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        let mut row = task("t-1", TaskStatus::Waiting);
        row.owner = Some(owner.clone());
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: Some("backlog at Ved's word".to_owned()),
            on: None,
            verification: false,
            parked: true,
            replan: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(30)),
        });
        row.updated_at = epoch(30);
        ws.seed_test_task(row);
        let crossing = Crossing {
            rung: ChaseRung::EstimateNudge,
            task_id: Some(TaskId::from("t-1")),
            subject: Some("10".to_owned()),
            seat: Some(owner),
            text: "task board: \"subject t-1\" has worked 20s against an estimate of 10s."
                .to_owned(),
        };
        let deliver = Recording::default();

        assert!(
            !ws.deliver_crossing(
                &deliver,
                &SessionSlot::lead("TestOrg", PROJECT),
                PROJECT,
                &crossing,
                epoch(200),
            ),
            "the dropped ask is not a delivery",
        );
        assert!(deliver.seen.lock().is_empty(), "nothing is said");
        assert!(
            ws.chases_for_project(PROJECT).is_empty(),
            "and nothing is logged: {:?}",
            ws.chases_for_project(PROJECT),
        );
    }

    /// The escalation is only true while its row still holds the ask: a row
    /// that ended between the board read and the write is not escalated.
    #[test]
    fn an_escalation_crossing_is_dropped_when_the_row_no_longer_holds_it() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Canceled);
        row.estimate = Some(Estimate { words: "10s".to_owned(), secs: 10 });
        row.updated_at = epoch(100);
        ws.seed_test_task(row);
        append_transition(&ws, "t-1", TaskStatus::Pending, TaskStatus::InProgress, 10);
        append_transition(&ws, "t-1", TaskStatus::InProgress, TaskStatus::Canceled, 100);
        let crossing = Crossing {
            rung: ChaseRung::Escalate,
            task_id: Some(TaskId::from("t-1")),
            subject: Some("10".to_owned()),
            seat: Some(SessionSlot::lead("TestOrg", PROJECT)),
            text: "task board: \"subject t-1\" passed its estimate of 10s and the re-plan ask \
                   is unanswered."
                .to_owned(),
        };
        let deliver = Recording::default();

        assert!(
            !ws.deliver_crossing(
                &deliver,
                &SessionSlot::lead("TestOrg", PROJECT),
                PROJECT,
                &crossing,
                epoch(1_000),
            ),
            "the stale escalation is not a delivery",
        );
        assert!(deliver.seen.lock().is_empty(), "nothing is said");
        assert!(
            ws.chases_for_project(PROJECT).is_empty(),
            "and nothing is logged: {:?}",
            ws.chases_for_project(PROJECT),
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
            replan: false,
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
            replan: false,
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
            replan: false,
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
            replan: false,
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

    /// A waiting row is never estimate-chased: while it is not being worked
    /// its clock is stopped, so worked time past its estimate is not an
    /// ongoing overrun. Its real wait is still checked - the wait's own rung
    /// carries it - and the fixture really is over its estimate, so the
    /// estimate signals decline on the state rather than on a missing fact.
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
            replan: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(30)),
        });
        row.updated_at = epoch(30);
        ws.seed_test_task(row);
        append_transition(&ws, "t-1", TaskStatus::Pending, TaskStatus::InProgress, 10);
        append_transition(&ws, "t-1", TaskStatus::InProgress, TaskStatus::Waiting, 30);
        let rows = ws.board_rows(PROJECT, epoch(WINDOW + 1), WINDOW);
        let held = rows.iter().find(|held| held.task.id == TaskId::from("t-1")).expect("row");
        assert!(
            held.worked_secs > held.task.estimate.as_ref().expect("estimate").secs,
            "the fixture worked past its estimate",
        );
        // The gate is the state: with `running` deleted at the nudge, the
        // park would refuse this row at delivery and the sweep would still
        // be silent. Read the crossings the state itself yields.
        let lead = SessionSlot::lead("TestOrg", PROJECT);
        let reached = crossings(&lead, &rows, &[], &[], &[], epoch(WINDOW + 1));
        assert!(
            reached.iter().all(|crossing| crossing.rung != ChaseRung::EstimateNudge
                && crossing.rung != ChaseRung::Escalate),
            "neither estimate crossing is reached: {reached:?}",
        );
        assert_eq!(
            reached.iter().filter(|crossing| crossing.rung == ChaseRung::WaitStall).count(),
            1,
            "the wait's own crossing carries it: {reached:?}",
        );
        let deliver = Recording::default();

        assert_eq!(
            sweep(&ws, &deliver, WINDOW + 1),
            1,
            "the wait's own rung carries it: {:?}",
            deliver.seen.lock(),
        );
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(seat, text)| seat == &SessionSlot::lead("TestOrg", PROJECT)
                && text.contains("on a resource")),
            "the genuine resource wait is checked with the lead: {seen:?}",
        );
        assert_eq!(
            seen.iter().filter(|(_, text)| text.contains("has worked")).count(),
            0,
            "no estimate nudge: {seen:?}",
        );
        assert_eq!(
            seen.iter().filter(|(_, text)| text.contains("the re-plan ask is unanswered")).count(),
            0,
            "no escalation: {seen:?}",
        );
        drop(seen);
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .all(|chase| chase.rung != ChaseRung::EstimateNudge
                    && chase.rung != ChaseRung::Escalate),
            "neither estimate rung is logged: {:?}",
            ws.chases_for_project(PROJECT),
        );
    }

    /// The same for a wait parked by design: the park is the user's own
    /// deferral, and a row that is not being worked has no ongoing overrun
    /// to ask about.
    #[test]
    fn a_parked_waiting_row_is_never_estimate_chased() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Waiting);
        row.estimate = Some(Estimate { words: "10s".to_owned(), secs: 10 });
        row.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Resource),
            detail: Some("backlog at Ved's word".to_owned()),
            on: None,
            verification: false,
            parked: true,
            replan: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(30)),
        });
        row.updated_at = epoch(30);
        ws.seed_test_task(row);
        append_transition(&ws, "t-1", TaskStatus::Pending, TaskStatus::InProgress, 10);
        append_transition(&ws, "t-1", TaskStatus::InProgress, TaskStatus::Waiting, 30);
        let rows = ws.board_rows(PROJECT, epoch(WINDOW + 1), WINDOW);
        let held = rows.iter().find(|held| held.task.id == TaskId::from("t-1")).expect("row");
        assert!(
            held.worked_secs > held.task.estimate.as_ref().expect("estimate").secs,
            "the fixture worked past its estimate",
        );
        // No ask is ever logged for this row, so an assertion on the log
        // could pass with the escalation armed. The crossing is the fact.
        let lead = SessionSlot::lead("TestOrg", PROJECT);
        let reached = crossings(&lead, &rows, &[], &[], &[], epoch(WINDOW + 1));
        assert!(
            reached.iter().all(|crossing| crossing.rung != ChaseRung::EstimateNudge
                && crossing.rung != ChaseRung::Escalate),
            "a parked wait reaches neither estimate crossing: {reached:?}",
        );
        let deliver = Recording::default();

        assert_eq!(
            sweep(&ws, &deliver, WINDOW + 1),
            0,
            "a parked waiting row is not chased: {:?}",
            deliver.seen.lock(),
        );
        assert!(deliver.seen.lock().is_empty(), "and nothing is said");
        assert!(
            ws.chases_for_project(PROJECT).is_empty(),
            "no rung is logged for it: {:?}",
            ws.chases_for_project(PROJECT),
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

    /// **A board delivery is queue-first, and forges no row at dispatch.**
    ///
    /// The prompt reaches the model under the machinery's own id prefix, so a
    /// reloaded transcript row still draws as forge's words. Its chat row is
    /// the one the session task draws at the CLI's `started`, carrying
    /// [`PromptSource::Board`] - the forged row at dispatch, retraction into
    /// the pile and redraw at the start are the bounce this cut removes.
    #[test]
    fn the_delivery_is_queue_first_and_forges_no_dispatch_row() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project(PROJECT, "/tmp/tp-chase");
        let key = SessionSlot::lead("TestOrg", PROJECT);
        ws.enable_test_dispatch_intercept();
        let deliver = PromptDeliver { workspace: Arc::clone(&ws) };

        let text = "task board: worker w-1 holds no row - feed it or despawn it.";
        assert_eq!(deliver.to_seat(&key, text), Delivery::Delivered);

        match ws.drain_test_dispatch_buffer().as_slice() {
            [crate::protocol::Command::PromptUnder { uuid, text: wired, source, .. }] => {
                assert_eq!(wired, text, "the model receives the chase's words");
                assert_eq!(
                    *source,
                    crate::protocol::PromptSource::Board,
                    "the board's row is its own kind, not the machinery's",
                );
                assert!(
                    uuid.starts_with("forge_nudge_"),
                    "the id keeps the prefix a reloaded row keys on: {uuid}",
                );
            }
            other => panic!("the delivery wires one prompt; got {other:?}"),
        }
        let drawn: Vec<SessionUpdate> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|frame| std::sync::Arc::unwrap_or_clone(frame.update))
            .collect();
        assert!(
            !drawn.iter().any(|update| matches!(update, SessionUpdate::ChatAppended { .. })),
            "the board's words enter the chat at `sent`, never at dispatch: {drawn:?}",
        );
    }

    /// A wait the owner re-states identically keeps the chase's marker and
    /// its start; a different wait is a new wait and clears the marker.
    #[test]
    fn a_restated_identical_wait_keeps_the_chase_marker() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let parked = ws.park_overrun(PROJECT, &TaskId::from("t-1")).expect("the row is parked");
        let wait = parked.waiting_on.clone().expect("a wait holds");
        assert!(wait.replan);

        let restated = ws
            .wait_task(
                PROJECT,
                &TaskId::from("t-1"),
                &owner,
                WaitingKind::Resource,
                wait.detail,
                None,
            )
            .expect("the owner may restate its wait");
        let restated_wait = restated.waiting_on.as_ref().expect("a wait holds");
        assert!(restated_wait.replan, "the same wait is still the chase's");
        assert_eq!(restated_wait.since, wait.since, "and it keeps its start");

        let changed = ws
            .wait_task(
                PROJECT,
                &TaskId::from("t-1"),
                &owner,
                WaitingKind::Resource,
                Some("a CI run".to_owned()),
                None,
            )
            .expect("the owner may change its wait");
        let changed_wait = changed.waiting_on.as_ref().expect("a wait holds");
        assert!(!changed_wait.replan, "a different wait is not the chase's");

        // A plain wait re-stated identically is still not the chase's: the
        // marker is minted by the park, never by a restate.
        let mut plain = task("t-plain", TaskStatus::Waiting);
        plain.owner = Some(owner.clone());
        plain.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Resource),
            detail: Some("a CI run".to_owned()),
            on: None,
            verification: false,
            parked: false,
            replan: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(30)),
        });
        plain.updated_at = epoch(30);
        ws.seed_test_task(plain);
        let restated_plain = ws
            .wait_task(
                PROJECT,
                &TaskId::from("t-plain"),
                &owner,
                WaitingKind::Resource,
                Some("a CI run".to_owned()),
                None,
            )
            .expect("the owner may restate a plain wait");
        let plain_wait = restated_plain.waiting_on.as_ref().expect("a wait holds");
        assert!(!plain_wait.replan, "a plain restate does not mint the marker");
    }

    /// A closed row's estimate ask is answered by its end: the logged rungs
    /// go with it, so a reopen with the same estimate asks again rather
    /// than reading a stale ask as unanswered.
    #[test]
    fn a_reopened_row_asks_again_for_the_same_estimate() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "parked waiting on you"), 1, "{:?}", deliver.seen.lock());
        // Both estimate rungs stand in the log: a forget narrowed to the
        // nudge would leave the escalation behind for the reopen to age.
        let subject = ws
            .chases_for_project(PROJECT)
            .into_iter()
            .find(|chase| chase.rung == ChaseRung::EstimateNudge)
            .and_then(|chase| chase.subject)
            .expect("the ask is logged");
        ws.record_chase(&TaskChase {
            task_id: Some(TaskId::from("t-1")),
            subject: Some(subject),
            project_name: PROJECT.to_owned(),
            rung: ChaseRung::Escalate,
            at: epoch(300),
        });
        // The row is canceled with both rungs standing.
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::Canceled;
        })
        .expect("no refusal")
        .expect("the row is there");
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .all(|chase| chase.rung != ChaseRung::EstimateNudge
                    && chase.rung != ChaseRung::Escalate),
            "the end answered the ask: {:?}",
            ws.chases_for_project(PROJECT),
        );

        // Reopened with the same estimate and still over it: a fresh ask.
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::InProgress;
        })
        .expect("no refusal")
        .expect("the row is there");
        assert_over_at(&ws, "t-1", 1_000);
        sweep(&ws, &deliver, 1_000);
        assert_eq!(
            says(&deliver, "has worked"),
            2,
            "the reopened row is asked again: {:?}",
            deliver.seen.lock(),
        );
    }

    /// A pending row is not being worked either: its worked time past an
    /// estimate arms no rung, and an assigned row keeps the queue rung out.
    #[test]
    fn a_pending_row_over_its_estimate_is_never_chased() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let mut row = task("t-1", TaskStatus::Pending);
        row.estimate = Some(Estimate { words: "10s".to_owned(), secs: 10 });
        row.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        row.updated_at = epoch(30);
        ws.seed_test_task(row);
        append_transition(&ws, "t-1", TaskStatus::Pending, TaskStatus::InProgress, 10);
        append_transition(&ws, "t-1", TaskStatus::InProgress, TaskStatus::Pending, 30);
        assert_over_at(&ws, "t-1", WINDOW + 1);
        // The gate is the state, not the park's refusal: the sweep below
        // would also be silent if a nudge crossing were dropped at delivery.
        let lead = SessionSlot::lead("TestOrg", PROJECT);
        let rows = ws.board_rows(PROJECT, epoch(WINDOW + 1), WINDOW);
        let reached = crossings(&lead, &rows, &[], &[], &[], epoch(WINDOW + 1));
        assert!(reached.is_empty(), "a pending row yields no crossing at all: {reached:?}");
        let deliver = Recording::default();

        assert_eq!(
            sweep(&ws, &deliver, WINDOW + 1),
            0,
            "a pending row is not chased: {:?}",
            deliver.seen.lock(),
        );
        assert!(deliver.seen.lock().is_empty(), "nothing is said");
        assert!(
            ws.chases_for_project(PROJECT).is_empty(),
            "no rung is logged for it: {:?}",
            ws.chases_for_project(PROJECT),
        );
    }

    /// An estimate restated upward while the row sits in the chase's park is
    /// the re-plan the ask wanted: the escalation stops, and the park's
    /// marker still keeps it out of the resource rung.
    #[test]
    fn an_upward_restated_estimate_ends_the_escalation() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();

        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "parked waiting on you"), 1, "{:?}", deliver.seen.lock());
        // The owner's re-plan raises the estimate above the worked time the
        // park froze, and leaves the row in the park.
        let now = 200 + WINDOW + 1;
        let rows = ws.board_rows(PROJECT, epoch(now), WINDOW);
        let held = rows.iter().find(|held| held.task.id == TaskId::from("t-1")).expect("row");
        let worked = held.worked_secs;
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |task| {
            task.estimate =
                Some(Estimate { words: "the rest of the day".to_owned(), secs: worked + 1 });
        })
        .expect("no refusal")
        .expect("the row is there");
        let rows = ws.board_rows(PROJECT, epoch(now), WINDOW);
        let held = rows.iter().find(|held| held.task.id == TaskId::from("t-1")).expect("row");
        assert!(
            held.worked_secs <= held.task.estimate.as_ref().expect("estimate").secs,
            "the restated estimate covers the frozen work",
        );
        let before = deliver.seen.lock().len();
        let logged = ws.chases_for_project(PROJECT).len();

        assert_eq!(
            sweep(&ws, &deliver, now),
            0,
            "the re-plan stopped the escalation: {:?}",
            deliver.seen.lock(),
        );
        assert_eq!(deliver.seen.lock().len(), before, "nothing more is said");
        assert_eq!(ws.chases_for_project(PROJECT).len(), logged, "and no rung is logged");
        assert!(
            ws.chases_for_project(PROJECT).iter().all(|chase| chase.rung != ChaseRung::Escalate),
            "no escalation survives the re-plan: {:?}",
            ws.chases_for_project(PROJECT),
        );
    }

    /// A deadline's cancel is a terminal move like any other: the row's
    /// estimate rungs go with it, so a reopen asks again.
    #[test]
    fn an_expired_ask_forgets_its_estimate_rungs() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let _lead_rx = live_lead(&ws);
        let owner = SessionSlot::worker("TestOrg", PROJECT, "w-1");
        let _owner_rx = ws.install_testing_stub(&owner);
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();
        sweep(&ws, &deliver, 200);
        assert_eq!(says(&deliver, "parked waiting on you"), 1, "{:?}", deliver.seen.lock());
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .any(|chase| chase.rung == ChaseRung::EstimateNudge),
            "the ask is logged",
        );

        let expired =
            ws.expire_ask(PROJECT, &TaskId::from("t-1"), epoch(400), "the deadline passed");
        assert!(expired.is_some(), "the row is canceled by the deadline");
        assert_eq!(expired.expect("the row").status, TaskStatus::Canceled, "the deadline ended it");
        assert!(
            ws.chases_for_project(PROJECT)
                .iter()
                .all(|chase| chase.rung != ChaseRung::EstimateNudge
                    && chase.rung != ChaseRung::Escalate),
            "the expiration answered the ask: {:?}",
            ws.chases_for_project(PROJECT),
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
