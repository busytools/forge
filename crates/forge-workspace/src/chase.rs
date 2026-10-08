//! The chase sweep: the pass that makes the board drive instead of
//! report.
//!
//! Every pass derives crossings from the board's own marks and the chase
//! log: an in-progress row past its estimate asks its owner once, a lead
//! hears once at 1.5x, a stalled queue or a stuck wait lands with the
//! lead once, a dead row's death is named once, an epic's retro and its
//! close are each asked for once. `Unaccounted` is deduped in memory (a
//! restart may re-fire once); every other rung is deduped durably by the
//! chase log, so "once" survives a reboot.
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
    /// The seat the message prefers: the row's owner for a nudge, the
    /// lead for everything else. The ladder falls back to the lead.
    pub seat: Option<SessionSlot>,
    pub text: String,
}

/// In-memory dedup for the rung with no row to stamp: a worker holding
/// nothing is named once, and named again only after it accounts itself.
#[derive(Default)]
pub struct ChaseMemory {
    reported: std::collections::HashSet<(String, String)>,
}

impl ChaseMemory {
    /// The unaccounted workers not yet named this occurrence, and the
    /// bookkeeping that makes this naming the once.
    fn take_new(&mut self, project: &str, unaccounted: &[String]) -> Vec<String> {
        let current: std::collections::HashSet<(String, String)> =
            unaccounted.iter().map(|label| (project.to_owned(), label.clone())).collect();
        // A worker that accounted again may be re-reported if it drifts
        // once more; anything still missing keeps its report.
        self.reported.retain(|entry| current.contains(entry));
        let mut fresh = Vec::new();
        for label in unaccounted {
            let entry = (project.to_owned(), label.clone());
            if self.reported.insert(entry) {
                fresh.push(label.clone());
            }
        }
        fresh
    }
}

fn fired_already(fired: &[TaskChase], rung: ChaseRung, id: &TaskId) -> bool {
    fired.iter().any(|chase| chase.rung == rung && chase.task_id.as_ref() == Some(id))
}

/// Whether `rung` already fired for `id` at or after `at` - the shape a
/// rung recurring with a new occurrence needs.
fn fired_since(fired: &[TaskChase], rung: ChaseRung, id: &TaskId, at: SystemTime) -> bool {
    fired
        .iter()
        .any(|chase| chase.rung == rung && chase.task_id.as_ref() == Some(id) && chase.at >= at)
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
    project: &str,
    lead: &SessionSlot,
    rows: &[BoardRow],
    history: &[TaskTransition],
    unaccounted: &[String],
    fired: &[TaskChase],
    memory: &mut ChaseMemory,
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
            && !fired_already(fired, ChaseRung::EstimateNudge, id)
        {
            out.push(Crossing {
                rung: ChaseRung::EstimateNudge,
                task_id: Some(id.clone()),
                seat: row.task.owner.clone(),
                text: format!(
                    "task board: \"{}\" has worked {} against its estimate of {} - status? An \
                     update clears this.",
                    row.task.subject,
                    fmt_secs(row.worked_secs),
                    estimate.words,
                ),
            });
        }
        if running
            && let Some(estimate) = estimate
            && row.worked_secs > estimate.secs + estimate.secs / 2
            && !fired_already(fired, ChaseRung::Escalate, id)
        {
            out.push(Crossing {
                rung: ChaseRung::Escalate,
                task_id: Some(id.clone()),
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
            && !fired_already(fired, ChaseRung::QueueStall, id)
        {
            out.push(Crossing {
                rung: ChaseRung::QueueStall,
                task_id: Some(id.clone()),
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
            && !fired_already(fired, ChaseRung::WaitStall, id)
        {
            out.push(Crossing {
                rung: ChaseRung::WaitStall,
                task_id: Some(id.clone()),
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
        if (died || stranded) && !fired_already(fired, ChaseRung::Death, id) {
            out.push(Crossing {
                rung: ChaseRung::Death,
                task_id: Some(id.clone()),
                seat: Some(lead.clone()),
                text: format!(
                    "task board: \"{}\" {} - re-dispatch it or cancel it.",
                    row.task.subject,
                    if died { "failed" } else { "has no live owner" },
                ),
            });
        }
        if let Some(at) = released_at(history, id)
            && now.duration_since(at).map_or(u64::MAX, |d| d.as_secs()) <= WAIT_RESOLVED_WINDOW_SECS
            && !fired_since(fired, ChaseRung::WaitResolved, id, at)
        {
            out.push(Crossing {
                rung: ChaseRung::WaitResolved,
                task_id: Some(id.clone()),
                seat: row.task.owner.clone(),
                text: format!(
                    "task board: \"{}\" was waiting and the thing it waited on has ended - \
                     resume it, re-plan it, or wait again.",
                    row.task.subject,
                ),
            });
        }
        if row.marks.to_close && !fired_already(fired, ChaseRung::EpicClose, id) {
            out.push(Crossing {
                rung: ChaseRung::EpicClose,
                task_id: Some(id.clone()),
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
            && !fired_already(fired, ChaseRung::Retro, &row.task.id)
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
                seat: Some(lead.clone()),
                text: retro_prompt(row, rows),
            });
        }
    }

    for label in memory.take_new(project, unaccounted) {
        out.push(Crossing {
            rung: ChaseRung::Unaccounted,
            task_id: None,
            seat: Some(lead.clone()),
            text: format!("task board: worker {label} holds no row - feed it or despawn it."),
        });
    }
    let _ = now;
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

    /// One pass of the chase over every project: derive the crossings,
    /// walk the ladder for each, deliver, and record what fired. Returns
    /// how many messages reached a session.
    pub fn chase_sweep(
        &self,
        deliver: &dyn Deliver,
        memory: &mut ChaseMemory,
        now: SystemTime,
    ) -> usize {
        let mut delivered = 0;
        for view in self.list_projects() {
            let project = view.name.clone();
            let lead = SessionSlot::lead(&view.org, &project);
            let rows = self.board_rows(&project, now, crate::board::DEFAULT_STALE_SECS);
            let history = self.history_for_project(&project);
            let unaccounted = self.unaccounted(&project);
            let fired = self.chases_for_project(&project);
            let crossings =
                crossings(&project, &lead, &rows, &history, &unaccounted, &fired, memory, now);
            for crossing in crossings {
                let target = ladder(&crossing, &lead, |seat| self.seat_is_live(seat));
                if let Some(target) = &target
                    && deliver.to_seat(target, &crossing.text) == Delivery::Delivered
                {
                    delivered += 1;
                }
                if crossing.rung != ChaseRung::Unaccounted {
                    self.record_chase(&TaskChase {
                        task_id: crossing.task_id.clone(),
                        project_name: project.clone(),
                        rung: crossing.rung,
                        at: now,
                    });
                }
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
                let mut memory = ChaseMemory::default();
                loop {
                    interval.tick().await;
                    let Some(workspace) = weak.upgrade() else {
                        return;
                    };
                    let delivered = workspace.chase_sweep(
                        &PromptDeliver { workspace: Arc::clone(&workspace) },
                        &mut memory,
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

/// The production delivery: a prompt into a live session, and nothing
/// else. The seat is already known live by the ladder, so a failed
/// dispatch here is a race, not a reason to spawn.
struct PromptDeliver {
    workspace: Arc<Workspace>,
}

impl Deliver for PromptDeliver {
    fn to_seat(&self, slot: &SessionSlot, text: &str) -> Delivery {
        let uuid = forge_sdk::request_id::next_prompt_id();
        match self.workspace.dispatch_workspace_prompt_under(
            slot,
            text.to_owned(),
            crate::protocol::PromptSource::Forge,
            uuid,
        ) {
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
    use forge_primitives::tasks::{
        By, Estimate, LinkKind, Task, TaskId, TaskLink, TaskStatus, TaskTransition, Waiting,
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

    fn sweep(
        ws: &Arc<Workspace>,
        deliver: &dyn Deliver,
        memory: &mut ChaseMemory,
        now: u64,
    ) -> usize {
        ws.chase_sweep(deliver, memory, epoch(now))
    }

    /// A lead seat behind a live session, so lead-addressed rungs have
    /// somewhere to land. The returned receiver must be held for the
    /// stub to stay.
    fn live_lead(
        ws: &Workspace,
    ) -> tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand> {
        ws.install_testing_stub(&SessionSlot::lead("TestOrg", PROJECT))
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
                diagnostic: None,
                kick: None,
            },
        );
        running_over_estimate(&ws, "t-1", Some("w-1"));
        let deliver = Recording::default();
        let mut memory = ChaseMemory::default();

        assert_eq!(sweep(&ws, &deliver, &mut memory, 100), 1, "one nudge");
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 1, "exactly one message: {seen:?}");
        assert_eq!(seen[0].0, SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        assert!(seen[0].1.contains("subject t-1"), "the message names the row: {}", seen[0].1);
        drop(seen);

        assert_eq!(sweep(&ws, &deliver, &mut memory, 140), 0, "the rung never repeats");
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
        let mut memory = ChaseMemory::default();

        sweep(&ws, &deliver, &mut memory, 100);
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
        let mut memory = ChaseMemory::default();

        assert_eq!(sweep(&ws, &deliver, &mut memory, 100), 0, "nothing delivered");
        assert!(deliver.seen.lock().is_empty(), "no seat was reached");
        assert!(
            !ws.chases_for_project(PROJECT).is_empty(),
            "and the rung is still on the log, so it fired once and never repeats",
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
        });
        row.updated_at = epoch(0);
        ws.seed_test_task(row);
        let deliver = Recording::default();
        let mut memory = ChaseMemory::default();

        sweep(&ws, &deliver, &mut memory, WINDOW + 1);
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
        let mut memory = ChaseMemory::default();

        sweep(&ws, &deliver, &mut memory, 100);
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(seat, text)| seat == &SessionSlot::worker("TestOrg", PROJECT, "w-1")
                && text.contains("was waiting")),
            "the waiter's owner hears the release: {seen:?}",
        );
        let before = seen.len();
        drop(seen);
        sweep(&ws, &deliver, &mut memory, 130);
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
        let mut memory = ChaseMemory::default();

        sweep(&ws, &deliver, &mut memory, WINDOW + 1);
        let seen = deliver.seen.lock();
        assert_eq!(seen.len(), 1, "one message: {seen:?}");
        assert!(seen[0].1.contains("queue is stalling"), "{}", seen[0].1);
        drop(seen);
        sweep(&ws, &deliver, &mut memory, WINDOW + 10);
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
        let mut memory = ChaseMemory::default();

        sweep(&ws, &deliver, &mut memory, 100);
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
        sweep(&ws, &deliver, &mut memory, 140);
        let seen = deliver.seen.lock();
        assert!(
            seen.iter().any(|(_, text)| text.contains("close it when you agree")),
            "the close nudge fires: {seen:?}",
        );
        let before = seen.len();
        drop(seen);
        sweep(&ws, &deliver, &mut memory, 180);
        assert_eq!(deliver.seen.lock().len(), before, "neither rung repeats");
    }
}
