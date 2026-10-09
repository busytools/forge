//! The board: one answer for every derived fact a reader of the task
//! list needs.
//!
//! Worked time, ages, and the marks (ready, in review, overdue, no
//! movement, waiting too long, stale, to close) are derived here from the
//! store, the history and session liveness - never stored. The tools, the
//! chase and the view surface all read THIS answer, so a mark cannot
//! disagree between two surfaces.

use std::collections::HashMap;
use std::time::SystemTime;

use forge_primitives::tasks::{LinkKind, Task, TaskId, TaskStatus, TaskTransition, WaitingKind};

use crate::workspace::Workspace;

/// The no-estimate no-movement window when `forge.toml` says nothing.
pub const DEFAULT_STALE_SECS: u64 = 4 * 3600;

/// A duration as the board's words: `3h`, `12m`, `45s`.
pub(crate) fn fmt_secs(secs: u64) -> String {
    if secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// The derived facts about one row, as the board draws them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Marks {
    /// Pending with nothing waiting on it.
    pub ready: bool,
    /// Has a pull-request link not known to be merged.
    pub in_review: bool,
    /// Its worked time is past its estimate.
    pub overdue: bool,
    /// No touch for longer than the window.
    pub no_movement: bool,
    /// Waiting for longer than the window.
    pub waiting_too_long: bool,
    /// Its owner has no session behind it.
    pub stale: bool,
    /// Every child terminal, its retro run - an epic the lead may close.
    pub to_close: bool,
}

/// One row as the board reads it: the record plus what is derived.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BoardRow {
    pub task: Task,
    /// The sum of its in_progress intervals, from the history.
    pub worked_secs: u64,
    /// Time since any touch.
    pub updated_secs_ago: u64,
    pub marks: Marks,
    /// A parent's children: how many are completed, of how many.
    pub rollup: Option<(usize, usize)>,
    /// A child's parent subject.
    pub parent_subject: Option<String>,
}

/// One named miss on a project's fleet row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Miss {
    /// Ready rows and a free worker slot: dispatch is the lead's job.
    StalledQueue,
    /// A live worker holding no row; it should be fed or despawned.
    UnaccountedWorker(String),
}

/// One project as the fleet page draws it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FleetRow {
    pub project: String,
    pub live_workers: usize,
    /// The project's worker cap, resolved (override or default).
    pub slots: Option<usize>,
    /// Ready unowned rows.
    pub queue: usize,
    /// Rows waiting on the user's decision.
    pub waiting_on_user: usize,
    pub misses: Vec<Miss>,
}

/// The sum of a row's in_progress intervals, from its transitions.
///
/// `mine` is that one row's transitions, in time order. They are grouped and
/// sorted once for the whole board rather than filtered per row: the log is
/// read on every home read and every task write, and scanning it once per row
/// makes each of those O(rows x history).
fn worked_secs(mine: &[&TaskTransition], now: SystemTime) -> u64 {
    let mut total = 0u64;
    let mut working_since: Option<SystemTime> = None;
    for transition in mine {
        match (working_since, transition.to) {
            (None, TaskStatus::InProgress) => working_since = Some(transition.at),
            (Some(since), to) if to != TaskStatus::InProgress => {
                total += transition.at.duration_since(since).map_or(0, |d| d.as_secs());
                working_since = None;
            }
            _ => {}
        }
    }
    if let Some(since) = working_since {
        total += now.duration_since(since).map_or(0, |d| d.as_secs());
    }
    total
}

fn secs_ago(at: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(at).map_or(0, |d| d.as_secs())
}

fn is_terminal(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Canceled)
}

/// The retro marker: a link labelled `retro` on the epic.
fn retro_ran(task: &Task) -> bool {
    task.links.iter().any(|l| l.label.as_deref() == Some("retro"))
}

impl Workspace {
    /// Every live row of `project`, with its derived facts.
    pub fn board_rows(&self, project: &str, now: SystemTime, stale_secs: u64) -> Vec<BoardRow> {
        let tasks = self.tasks_for_project(project);
        let history = {
            let guard = self.db.lock();
            match guard.as_ref() {
                Some(db) => {
                    crate::store::task_history::list_for_project(db, project).unwrap_or_default()
                }
                None => Vec::new(),
            }
        };
        // Every row's own transitions, in time order, from ONE walk of the
        // log: the read below is per row, and the log grows without bound.
        let mut by_task: HashMap<&TaskId, Vec<&TaskTransition>> = HashMap::new();
        for transition in &history {
            by_task.entry(&transition.task_id).or_default().push(transition);
        }
        for mine in by_task.values_mut() {
            mine.sort_by_key(|t| t.at);
        }
        tasks
            .iter()
            .map(|task| {
                let worked = worked_secs(by_task.get(&task.id).map_or(&[][..], Vec::as_slice), now);
                let updated_secs_ago = secs_ago(task.updated_at, now);
                let children: Vec<&Task> =
                    tasks.iter().filter(|c| c.parent.as_ref() == Some(&task.id)).collect();
                let children_terminal =
                    !children.is_empty() && children.iter().all(|c| is_terminal(c.status));
                let marks = Marks {
                    ready: task.status == TaskStatus::Pending && task.waiting_on.is_none(),
                    in_review: task
                        .links
                        .iter()
                        .any(|l| l.kind == LinkKind::Pr && l.state.as_deref() != Some("merged")),
                    overdue: task.estimate.as_ref().is_some_and(|e| e.secs > 0 && worked > e.secs),
                    no_movement: updated_secs_ago > stale_secs,
                    waiting_too_long: task.status == TaskStatus::Waiting
                        && updated_secs_ago > stale_secs,
                    stale: task.owner.as_ref().is_some_and(|owner| !self.seat_is_live(owner)),
                    to_close: task.parent.is_none() && children_terminal && retro_ran(task),
                };
                BoardRow {
                    task: task.clone(),
                    worked_secs: worked,
                    updated_secs_ago,
                    marks,
                    rollup: (!children.is_empty()).then(|| {
                        (
                            children.iter().filter(|c| c.status == TaskStatus::Completed).count(),
                            children.len(),
                        )
                    }),
                    parent_subject: task.parent.as_ref().and_then(|id| {
                        tasks.iter().find(|p| p.id == *id).map(|p| p.subject.clone())
                    }),
                }
            })
            .collect()
    }

    /// One row per project, for the fleet page.
    pub fn fleet_rows(&self) -> Vec<FleetRow> {
        self.list_projects()
            .into_iter()
            .map(|view| {
                let name = view.name.clone();
                let tasks = self.tasks_for_project(&name);
                let live_workers = self.list_live_workers(&view.key).len();
                let slots = self
                    .config
                    .projects
                    .iter()
                    .find(|p| p.name == name)
                    .and_then(|p| p.max_workers)
                    .unwrap_or(crate::config::DEFAULT_MAX_WORKERS_PER_PROJECT);
                let queue = tasks
                    .iter()
                    .filter(|t| {
                        t.status == TaskStatus::Pending
                            && t.owner.is_none()
                            && t.waiting_on.is_none()
                    })
                    .count();
                let waiting_on_user = tasks
                    .iter()
                    .filter(|t| {
                        t.status == TaskStatus::Waiting
                            && t.waiting_on.as_ref().and_then(|w| w.kind)
                                == Some(WaitingKind::Decision)
                    })
                    .count();
                let mut misses = Vec::new();
                if queue > 0 && live_workers < slots {
                    misses.push(Miss::StalledQueue);
                }
                misses.extend(self.unaccounted(&name).into_iter().map(Miss::UnaccountedWorker));
                FleetRow {
                    project: name,
                    live_workers,
                    slots: Some(slots),
                    queue,
                    waiting_on_user,
                    misses,
                }
            })
            .collect()
    }

    /// Live workers of `project` holding no open row.
    pub fn unaccounted(&self, project: &str) -> Vec<String> {
        let key =
            self.list_projects().into_iter().find(|view| view.name == project).map(|view| view.key);
        let Some(key) = key else { return Vec::new() };
        let mut open: Vec<String> = self
            .tasks_for_project(project)
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Pending | TaskStatus::InProgress | TaskStatus::Waiting
                )
            })
            .filter_map(|t| t.owner.as_ref().map(|o| o.label().to_owned()))
            .collect();
        open.sort();
        open.dedup();
        let mut labels: Vec<String> = self
            .list_live_workers(&key)
            .into_iter()
            .filter(|entry| !open.contains(&entry.label))
            .map(|entry| entry.label)
            .collect();
        labels.sort();
        labels
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::SessionSlot;
    use forge_primitives::tasks::{
        By, Estimate, LinkKind, Task, TaskId, TaskLink, TaskStatus, TaskTransition, Waiting,
        WaitingKind,
    };
    use tempfile::tempdir;

    const PROJECT: &str = "proj";
    const WINDOW: u64 = 4 * 3600;

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fixture(dir: &std::path::Path) -> std::sync::Arc<Workspace> {
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.to_owned());
        ws.seed_test_project(PROJECT, "/tmp/tp-board");
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

    fn transition(id: &str, from: TaskStatus, to: TaskStatus, at: u64) -> TaskTransition {
        TaskTransition {
            task_id: TaskId::from(id),
            project_name: PROJECT.to_owned(),
            from: Some(from),
            to,
            at: epoch(at),
            by: By::System,
        }
    }

    fn live_worker(ws: &Workspace, label: &str) {
        let key = ws
            .list_projects()
            .into_iter()
            .find(|view| view.name == PROJECT)
            .expect("seeded project")
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
                spawned_by: SessionSlot::lead("TestOrg", PROJECT),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
    }

    fn row<'a>(rows: &'a [BoardRow], id: &str) -> &'a BoardRow {
        rows.iter().find(|row| row.task.id == TaskId::from(id)).expect("the row is on the board")
    }

    /// Two in-progress intervals with a wait between them: only the
    /// intervals count, and the wait is what the estimate is spared.
    #[test]
    fn worked_time_is_the_sum_of_in_progress_intervals() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut running = task("t-1", TaskStatus::InProgress);
        running.estimate = Some(Estimate { words: "1m".to_owned(), secs: 60 });
        running.updated_at = epoch(45);
        ws.seed_test_task(running);
        {
            let db = ws.db.lock();
            crate::store::task_history::append(
                db.as_ref().expect("db"),
                &[
                    transition("t-1", TaskStatus::Pending, TaskStatus::InProgress, 10),
                    transition("t-1", TaskStatus::InProgress, TaskStatus::Waiting, 20),
                    transition("t-1", TaskStatus::Waiting, TaskStatus::InProgress, 30),
                ],
            )
            .expect("history");
        }

        // A second row, whose own intervals sit between the first's: the
        // transitions are grouped once for the whole board, so a grouping that
        // mixed two rows' logs would show up here and nowhere else.
        let mut other = task("t-2", TaskStatus::InProgress);
        other.updated_at = epoch(40);
        ws.seed_test_task(other);
        {
            let db = ws.db.lock();
            crate::store::task_history::append(
                db.as_ref().expect("db"),
                &[transition("t-2", TaskStatus::Pending, TaskStatus::InProgress, 35)],
            )
            .expect("history");
        }

        let rows = ws.board_rows(PROJECT, epoch(45), WINDOW);
        assert_eq!(
            row(&rows, "t-1").worked_secs,
            25,
            "10s in the first interval and 15s into the second; the wait is not worked time",
        );
        assert!(!row(&rows, "t-1").marks.overdue, "25s of a 60s estimate is not overdue");
        assert_eq!(
            row(&rows, "t-2").worked_secs,
            10,
            "the second row is measured from its own transitions, not its neighbour's",
        );
    }

    /// A fresh update does not clear an overrun: the clock is worked
    /// time, not time since the last touch.
    #[test]
    fn an_update_does_not_clear_an_overrun() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut running = task("t-1", TaskStatus::InProgress);
        running.estimate = Some(Estimate { words: "10s".to_owned(), secs: 10 });
        running.updated_at = epoch(100);
        ws.seed_test_task(running);
        {
            let db = ws.db.lock();
            crate::store::task_history::append(
                db.as_ref().expect("db"),
                &[transition("t-1", TaskStatus::Pending, TaskStatus::InProgress, 10)],
            )
            .expect("history");
        }

        let rows = ws.board_rows(PROJECT, epoch(100), WINDOW);
        assert!(
            row(&rows, "t-1").marks.overdue,
            "90s of worked time against a 10s estimate is overdue, however fresh the touch",
        );
    }

    /// A row without an estimate falls back to the no-movement window,
    /// which is its own mark - not an overdue one.
    #[test]
    fn a_row_without_an_estimate_uses_the_no_movement_window() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut stale_row = task("old", TaskStatus::Pending);
        stale_row.updated_at = epoch(0);
        let mut fresh_row = task("new", TaskStatus::Pending);
        fresh_row.updated_at = epoch(100);
        fresh_row.id = TaskId::from("new");
        ws.seed_test_task(stale_row);
        ws.seed_test_task(fresh_row);

        let now = epoch(WINDOW + 1);
        let rows = ws.board_rows(PROJECT, now, WINDOW);
        assert!(row(&rows, "old").marks.no_movement, "a day without a touch is no movement");
        assert!(!row(&rows, "old").marks.overdue, "and it is not called overdue");
        assert!(!row(&rows, "new").marks.no_movement, "a fresh touch is movement");
    }

    /// A row can be Pending and still not ready. The wait outlives the status
    /// a mover left on it - a `tasks__update` to `pending` does not clear
    /// `waiting_on` - and `claim_task` refuses exactly that row, so a mark
    /// that read the status alone would say ready where the claim says no.
    #[test]
    fn a_pending_row_holding_a_wait_is_not_ready() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut held = task("t-held", TaskStatus::Pending);
        held.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Dependency),
            detail: None,
            on: None,
            verification: false,
        });
        ws.seed_test_task(held);
        ws.seed_test_task(task("t-plain", TaskStatus::Pending));

        let rows = ws.board_rows(PROJECT, epoch(10), WINDOW);
        assert!(
            !row(&rows, "t-held").marks.ready,
            "a wait the status does not show still blocks a claim",
        );
        // The control: the mark can say ready, so the assertion above is
        // reading the conjunct rather than a mark that never fires.
        assert!(row(&rows, "t-plain").marks.ready, "the plain row is the one a seeker can take");
    }

    #[test]
    fn a_waiting_row_past_the_window_carries_waiting_too_long() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut waiting = task("t-1", TaskStatus::Waiting);
        waiting.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Decision),
            detail: Some("which shape".to_owned()),
            on: None,
            verification: false,
        });
        waiting.updated_at = epoch(0);
        ws.seed_test_task(waiting);

        let rows = ws.board_rows(PROJECT, epoch(WINDOW + 1), WINDOW);
        assert!(row(&rows, "t-1").marks.waiting_too_long, "the wait's own age is the signal");
    }

    #[test]
    fn a_worker_live_with_no_open_row_is_unaccounted() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        live_worker(&ws, "w-1");
        assert_eq!(
            ws.unaccounted(PROJECT),
            vec!["w-1".to_owned()],
            "a live worker holding no row is unaccounted",
        );

        let mut held = task("t-1", TaskStatus::Pending);
        held.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        ws.seed_test_task(held);
        assert!(ws.unaccounted(PROJECT).is_empty(), "a held row accounts for its worker");
    }

    /// An epic whose children have all stopped - completed and canceled
    /// alike - with its retro run reads to close; the rollup counts the
    /// completed ones only.
    #[test]
    fn an_epic_whose_children_are_all_terminal_reads_to_close() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut epic = task("epic", TaskStatus::InProgress);
        epic.links.push(TaskLink {
            kind: LinkKind::Path,
            label: Some("retro".to_owned()),
            target: "docs/retro.md".to_owned(),
            state: None,
            added_at: epoch(0),
        });
        ws.seed_test_task(epic);
        let mut done = task("sub-a", TaskStatus::Completed);
        done.parent = Some(TaskId::from("epic"));
        let mut dropped = task("sub-b", TaskStatus::Canceled);
        dropped.parent = Some(TaskId::from("epic"));
        ws.seed_test_task(done);
        ws.seed_test_task(dropped);

        let rows = ws.board_rows(PROJECT, epoch(10), WINDOW);
        let epic_row = row(&rows, "epic");
        assert!(epic_row.marks.to_close, "every child is terminal and the retro ran");
        assert_eq!(
            epic_row.rollup,
            Some((1, 2)),
            "canceled is terminal for closing but never counted done",
        );
        assert_eq!(row(&rows, "sub-a").parent_subject.as_deref(), Some("subject epic"));
    }

    /// A completed child keeps an open sibling from closing the epic, and
    /// the missing retro keeps it open too.
    #[test]
    fn an_epic_with_an_open_child_or_no_retro_does_not_read_to_close() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(task("epic", TaskStatus::InProgress));
        let mut done = task("sub-a", TaskStatus::Completed);
        done.parent = Some(TaskId::from("epic"));
        ws.seed_test_task(done);
        let mut open = task("sub-b", TaskStatus::Pending);
        open.parent = Some(TaskId::from("epic"));
        ws.seed_test_task(open);

        let rows = ws.board_rows(PROJECT, epoch(10), WINDOW);
        assert!(!row(&rows, "epic").marks.to_close, "an open child holds the epic open");

        // The other half: same epic, every child terminal, no retro yet.
        ws.update_task(PROJECT, &TaskId::from("sub-b"), By::System, |t| {
            t.status = TaskStatus::Canceled;
        })
        .expect("no refusal")
        .expect("the child is there");
        let rows = ws.board_rows(PROJECT, epoch(10), WINDOW);
        assert!(
            !row(&rows, "epic").marks.to_close,
            "terminal children without a retro still hold the epic open",
        );
    }

    /// Free slots beside a non-empty queue is the miss the board names.
    #[test]
    fn a_stalled_queue_is_a_miss_on_the_fleet() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        live_worker(&ws, "w-1");
        ws.seed_test_task(task("queued", TaskStatus::Pending));

        let fleet = ws.fleet_rows();
        let row = fleet.iter().find(|row| row.project == PROJECT).expect("the project's row");
        assert_eq!(row.queue, 1, "one ready unowned row is the queue");
        assert_eq!(row.slots, Some(2), "the default worker cap");
        assert!(
            row.misses.contains(&Miss::StalledQueue),
            "a free slot beside a queue is a miss: {row:?}",
        );
        assert!(
            row.misses.contains(&Miss::UnaccountedWorker("w-1".to_owned())),
            "and the worker holding nothing is named",
        );
    }
}
