//! The board: one answer for every derived fact a reader of the task
//! list needs.
//!
//! Worked time, ages, and the five marks (ready, in review, overdue, stale,
//! to close) are derived here from the store, the history and session
//! liveness - never stored. The tools, the chase and the view surface all
//! read THIS answer, so a mark cannot disagree between two surfaces.

use std::collections::HashMap;
use std::time::SystemTime;

use forge_primitives::tasks::{LinkKind, Task, TaskId, TaskKind, TaskStatus, TaskTransition};

use crate::workspace::Workspace;

/// The no-estimate no-movement window when `forge.toml` says nothing.
pub const DEFAULT_STALE_SECS: u64 = 4 * 3600;

/// How many of a row's own transitions a board row carries.
///
/// The newest this many, at a measured ~250 bytes per transition - so a
/// capped row weighs ~5 KiB and a row has to move more than twenty times
/// before anything is withheld. The whole count rides beside the tail in
/// `history_total`, so what the cap withholds is stated rather than silent.
pub const HISTORY_CAP: usize = 20;

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Marks {
    /// Pending with nothing waiting on it and not reserved for the user.
    pub ready: bool,
    /// Has a pull-request link not known to be merged.
    pub in_review: bool,
    /// Does this row owe a re-plan? Its worked time is past its estimate,
    /// or it has been waiting past the window.
    pub overdue: bool,
    /// Is anyone actually on this? No touch past the band, or an owner
    /// with no session behind it.
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
    /// Whether the owner has a session behind it. The `stale` mark folds
    /// this in with the no-movement window, but the chase's death rung
    /// names only the dead holder - a live owner who has not touched the
    /// row is stuck, not gone.
    pub owner_live: bool,
    /// How long the row's current wait has run, measured from the wait's
    /// OWN start (`waiting_on.since`), never from the row's last edit. A
    /// row re-stating the same wait keeps its start, and a row edited
    /// while waiting does not reset its age. `None` when it is not waiting.
    #[serde(default)]
    pub wait_secs: Option<u64>,
    pub marks: Marks,
    /// A parent's children: how many are completed, of how many.
    pub rollup: Option<(usize, usize)>,
    /// A child's parent subject.
    pub parent_subject: Option<String>,
    /// The row's own transitions, in time order - the newest
    /// [`HISTORY_CAP`] of them, which is what the detail drawer draws when
    /// it opens. The tail beyond the cap is stated by `history_total` rather
    /// than carried: the walk below already groups the transitions for the
    /// worked time, and carrying the whole trail costs every `board_rows`
    /// caller - every board read and every `tasks__list` - the record's full
    /// weight.
    ///
    /// `default` is backward compatibility with older records, and nothing
    /// more: no server-side pin decodes a `BoardRow`, so nothing in the tree
    /// depends on it - a capture written before the field can still be read
    /// wherever one is.
    #[serde(default)]
    pub history: Vec<TaskTransition>,
    /// How many transitions the row has in all - never fewer than
    /// `history.len()`. The drawer words its `showing the last N of M` from
    /// it, so a capped tail is stated rather than silent.
    #[serde(default)]
    pub history_total: usize,
}

/// What changed on a project's board since the person last looked, one
/// count per line of the window's digest strip.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BoardDelta {
    /// Rows that changed state.
    pub moved: usize,
    /// Rows that ended.
    pub landed: usize,
    /// Asks whose deadline default ended them.
    pub expired: usize,
    /// Linked pull requests the connector saw merged.
    pub merged: usize,
    /// What `moved` counts, so the line opens onto its rows.
    #[serde(default)]
    pub moved_items: Vec<DeltaItem>,
    /// What `landed` counts.
    #[serde(default)]
    pub landed_items: Vec<DeltaItem>,
    /// What `expired` counts: the asks whose deadline ended them, with
    /// their records, so an expiry is drawn as itself rather than as a
    /// canceled row only.
    #[serde(default)]
    pub expired_items: Vec<DeltaItem>,
    /// What `merged` counts.
    #[serde(default)]
    pub merged_items: Vec<DeltaItem>,
}

/// One row behind a digest count, so the line that counts it opens onto
/// what it counts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DeltaItem {
    /// The row the item names, when one still exists.
    pub id: Option<String>,
    pub subject: String,
    /// What happened to it, in the board's own words.
    pub word: String,
    pub at: SystemTime,
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

/// One wait that is on the person, named for the surface that raises it.
///
/// **The board is the store; this is the read.** A wait on the person is
/// already a row with a `waiting_on` block, and the rail draws this beside the count so
/// the person can see what is being asked without opening the board page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AttentionItem {
    /// The task's id: the wait's identity, so a raise is a new id or a new
    /// `at` rather than a second item for the same ask.
    pub task: String,
    /// The seat that raised it, by its session label.
    pub owner: String,
    /// The row's subject, in the row's own words.
    pub subject: String,
    /// The ask's own words, verbatim.
    pub words: String,
    /// When the wait began, stamped as it is raised; a stored row without
    /// one falls back to its creation.
    pub at: SystemTime,
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
    /// The waits themselves, so the mark beside the count can say what is
    /// being asked without the board page.
    pub attention: Vec<AttentionItem>,
    /// Every row the project holds, which the home's header totals.
    pub rows: usize,
    pub misses: Vec<Miss>,
}

/// The sum of a row's in_progress intervals, from its transitions.
///
/// `mine` is that one row's transitions, in time order. They are grouped and
/// sorted once for the whole board rather than filtered per row: the log is
/// read on every board read and every task write, and scanning it once per row
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

/// The tail of a row's transitions a board row carries: the newest
/// [`HISTORY_CAP`], in time order. `mine` is that one row's transitions,
/// already sorted.
fn capped_history(mine: &[&TaskTransition]) -> Vec<TaskTransition> {
    let start = mine.len().saturating_sub(HISTORY_CAP);
    mine.get(start..).unwrap_or(mine).iter().map(|t| (*t).clone()).collect()
}

fn secs_ago(at: SystemTime, now: SystemTime) -> u64 {
    now.duration_since(at).map_or(0, |d| d.as_secs())
}

/// One row's own record, for a row the bulk board does not carry.
///
/// **A closed row's page is derived like a live row's**: its worked time and
/// its history come from the log already on disk, not from placeholders. A
/// terminal row carries no marks, by the board's own rule.
pub fn detail_row(
    task: Task,
    history: &[TaskTransition],
    now: SystemTime,
    owner_live: bool,
) -> BoardRow {
    let mut mine: Vec<&TaskTransition> =
        history.iter().filter(|step| step.task_id == task.id).collect();
    mine.sort_by_key(|step| step.at);
    let worked = worked_secs(&mine, now);
    let updated_secs_ago = secs_ago(task.updated_at, now);
    BoardRow {
        wait_secs: task.wait_age_secs(now),
        marks: Marks::default(),
        worked_secs: worked,
        updated_secs_ago,
        owner_live,
        rollup: None,
        parent_subject: None,
        history: capped_history(&mine),
        history_total: mine.len(),
        task,
    }
}

/// The retro marker: a link labelled `retro` on the epic.
fn retro_ran(task: &Task) -> bool {
    task.links.iter().any(|l| l.label.as_deref() == Some("retro"))
}

/// The ask's own words, as the mark and its popover word it.
///
/// A detail-less wait - and a verification park, which often carries none -
/// would otherwise draw a blank line and notify with an empty body; the row's
/// own detail, the look a park is owed, and the subject are each a truer
/// answer than nothing.
fn attention_words(task: &Task) -> String {
    let said = task.waiting_on.as_ref().and_then(|wait| wait.detail.as_deref());
    for words in [said, task.detail.as_deref()] {
        if let Some(words) = words.map(str::trim).filter(|words| !words.is_empty()) {
            return words.to_owned();
        }
    }
    if task.waiting_on.as_ref().is_some_and(|wait| wait.verification) {
        return "the look is owed".to_owned();
    }
    let subject = task.subject.trim();
    if subject.is_empty() {
        // Nothing requires a subject to be non-empty; the board words a
        // detail-less decision the same way rather than drawing a blank.
        "A decision is waiting on you.".to_owned()
    } else {
        subject.to_owned()
    }
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
        // The hierarchy in one pass: a row's children and its parent's
        // subject are lookups below, not a walk of the whole set per row.
        let mut children_by_parent: HashMap<&TaskId, Vec<&Task>> = HashMap::new();
        let mut subjects_by_id: HashMap<&TaskId, &str> = HashMap::new();
        for task in &tasks {
            subjects_by_id.entry(&task.id).or_insert(task.subject.as_str());
            if let Some(parent) = &task.parent {
                children_by_parent.entry(parent).or_default().push(task);
            }
        }
        // Liveness is read once per owner, not once per row: the lookup
        // resolves the project and the worker registry, so asking per row
        // made a sweep O(rows x projects) under the same locks.
        let mut liveness: HashMap<(String, String), bool> = HashMap::new();
        tasks
            .iter()
            .map(|task| {
                let mine = by_task.get(&task.id).map_or(&[][..], Vec::as_slice);
                let worked = worked_secs(mine, now);
                let updated_secs_ago = secs_ago(task.updated_at, now);
                // A wait parked by design is the user's own deferral: its age
                // is not a stall (#2472).
                let parked = task.waiting_on.as_ref().is_some_and(|wait| wait.parked);
                let children = children_by_parent.get(&task.id).map_or(&[][..], Vec::as_slice);
                let children_terminal =
                    !children.is_empty() && children.iter().all(|c| c.status.is_terminal());
                let owner_live = task.owner.as_ref().is_none_or(|owner| {
                    let key = (owner.project().to_owned(), owner.label().to_owned());
                    *liveness.entry(key).or_insert_with(|| self.seat_is_live(owner))
                });
                // Terminal rows carry no marks: they are history, and a mark
                // on one reads as work that still needs someone.
                let marks = if task.status.is_terminal() {
                    Marks::default()
                } else {
                    Marks {
                        ready: task.is_ready(),
                        in_review: task.links.iter().any(|l| {
                            l.kind == LinkKind::Pr && l.state.as_deref() != Some("merged")
                        }),
                        overdue: (task.kind == TaskKind::Finite
                            && task
                                .estimate
                                .as_ref()
                                .is_some_and(|e| e.secs > 0 && worked > e.secs))
                            || (task.status == TaskStatus::Waiting
                                && !parked
                                && task.wait_age_secs(now).is_some_and(|age| age > stale_secs)),
                        // The mark answers "is anyone actually on this?" - a
                        // held row nobody moves, or an owner with no session.
                        // An unowned ready row keeps it too: the chase's
                        // queue-stall signal reads it, and a window that
                        // draws every backlog row as an exception narrows
                        // that on its own side.
                        stale: !parked && (updated_secs_ago > stale_secs || !owner_live),
                        to_close: task.parent.is_none() && children_terminal && retro_ran(task),
                    }
                };
                BoardRow {
                    task: task.clone(),
                    worked_secs: worked,
                    updated_secs_ago,
                    owner_live,
                    wait_secs: task.wait_age_secs(now),
                    marks,
                    rollup: (!children.is_empty()).then(|| {
                        (
                            children.iter().filter(|c| c.status == TaskStatus::Completed).count(),
                            children.len(),
                        )
                    }),
                    parent_subject: task
                        .parent
                        .as_ref()
                        .and_then(|id| subjects_by_id.get(id).map(|subject| (*subject).to_owned())),
                    history: capped_history(mine),
                    history_total: mine.len(),
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
                    .config_arc()
                    .projects
                    .iter()
                    .find(|p| p.name == name)
                    .and_then(|p| p.max_workers)
                    .unwrap_or(crate::config::DEFAULT_MAX_WORKERS_PER_PROJECT);
                // The queue is the dispatchable slice of the one ready
                // predicate: nothing in the row's way, and nobody holding it.
                let queue = tasks.iter().filter(|t| t.is_ready() && t.owner.is_none()).count();
                let waiting_on_user = tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Waiting && crate::tasks::parked_on_user(t))
                    .count();
                // The wait's own start, stamped as the wait is raised:
                // `updated_at` moves on any edit of a waiting row, and a
                // notification keyed on it would fire again for an edit that
                // answered nothing. A row stored before the field existed
                // falls back to its creation, not to that edit.
                let attention = tasks
                    .iter()
                    .filter(|t| t.status == TaskStatus::Waiting && crate::tasks::parked_on_user(t))
                    .map(|t| AttentionItem {
                        task: t.id.as_str().to_owned(),
                        owner: t
                            .owner
                            .as_ref()
                            .map_or_else(|| "lead".to_owned(), |seat| seat.label().to_owned()),
                        subject: t.subject.clone(),
                        words: attention_words(t),
                        at: t
                            .waiting_on
                            .as_ref()
                            .and_then(|wait| wait.since)
                            .unwrap_or(t.created_at),
                    })
                    .collect();
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
                    attention,
                    rows: tasks.len(),
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
            .filter(|t| !t.status.is_terminal())
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
        By, Estimate, Evidence, LinkKind, Task, TaskId, TaskKind, TaskLink, TaskStatus,
        TaskTransition, Verify, Waiting, WaitingKind,
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
            kind: TaskKind::Finite,
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
            evidence: Some(Evidence {
                commit: Some("test-commit".to_owned()),
                path: None,
                excerpt: None,
                verified_at: None,
                problem: None,
            }),
            version: 0,
            archived_at: None,
            created_at: epoch(0),
            updated_at: epoch(0),
        }
    }

    fn named(id: &str, subject: &str, project: &str) -> Task {
        Task {
            subject: subject.to_owned(),
            project_name: project.to_owned(),
            ..task(id, TaskStatus::Pending)
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
                cwd: None,
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

    /// A row past the cap carries its newest [`HISTORY_CAP`] transitions with
    /// its true count beside them - and what the row DERIVES still reads the
    /// whole trail: the worked time is measured before the cap bites, so a
    /// cap applied to the walk's own list would lose the earliest interval.
    #[test]
    fn a_row_past_the_cap_carries_its_newest_and_its_true_count() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut running = task("t-1", TaskStatus::Pending);
        running.updated_at = epoch(1_000);
        ws.seed_test_task(running);
        // Thirty moves at ten-second beats, in progress on the even ones and
        // settled on the odd: fifteen ten-second intervals, the first of them
        // outside the newest twenty.
        let moves: Vec<TaskTransition> = (0..30)
            .map(|i| {
                let (from, to) = if i % 2 == 0 {
                    (TaskStatus::Pending, TaskStatus::InProgress)
                } else {
                    (TaskStatus::InProgress, TaskStatus::Pending)
                };
                transition("t-1", from, to, (i + 1) * 10)
            })
            .collect();
        {
            let db = ws.db.lock();
            crate::store::task_history::append(db.as_ref().expect("db"), &moves).expect("history");
        }

        let rows = ws.board_rows(PROJECT, epoch(1_000), WINDOW);
        let row = row(&rows, "t-1");
        assert_eq!(row.history_total, 30, "the count is the row's whole trail");
        assert_eq!(row.history.len(), HISTORY_CAP, "the carry is the newest {HISTORY_CAP}");
        assert_eq!(
            row.history.first().map(|t| t.at),
            Some(epoch(110)),
            "the newest {HISTORY_CAP} start at the eleventh move, not the first",
        );
        assert_eq!(row.worked_secs, 150, "the cap does not change what the row derives");
    }

    /// A row under the cap is untouched: every transition rides, and the
    /// count is the list's own length.
    #[test]
    fn a_short_history_rides_whole() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(task("t-1", TaskStatus::Pending));
        {
            let db = ws.db.lock();
            crate::store::task_history::append(
                db.as_ref().expect("db"),
                &[
                    transition("t-1", TaskStatus::Pending, TaskStatus::InProgress, 10),
                    transition("t-1", TaskStatus::InProgress, TaskStatus::Completed, 20),
                ],
            )
            .expect("history");
        }

        let rows = ws.board_rows(PROJECT, epoch(100), WINDOW);
        let row = row(&rows, "t-1");
        assert_eq!(row.history.len(), 2, "a short trail rides whole");
        assert_eq!(row.history_total, 2, "and its count is the list's own length");
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

    /// A row without a touch for longer than the window reads stale - the
    /// `no_movement` mark folded into it - and staleness is not an
    /// overrun: the two marks answer different questions.
    #[test]
    fn a_row_with_no_movement_reads_stale() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        // Held work: a row nobody owns is backlog, and backlog is not
        // stale - the mark answers whether anyone is on the row.
        live_worker(&ws, "w-1");
        let mut stale_row = task("old", TaskStatus::InProgress);
        stale_row.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        stale_row.updated_at = epoch(0);
        let mut fresh_row = task("new", TaskStatus::InProgress);
        fresh_row.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        fresh_row.updated_at = epoch(100);
        fresh_row.id = TaskId::from("new");
        ws.seed_test_task(stale_row);
        ws.seed_test_task(fresh_row);

        let now = epoch(WINDOW + 1);
        let rows = ws.board_rows(PROJECT, now, WINDOW);
        assert!(row(&rows, "old").marks.stale, "a day without a touch is no one on it");
        assert!(!row(&rows, "old").marks.overdue, "and it is not called overdue");
        assert!(!row(&rows, "new").marks.stale, "a fresh touch is movement");
    }

    /// A row whose owner has no session behind it reads stale whether or
    /// not it was touched: the mark answers "is anyone actually on
    /// this?", and a dead holder is a no.
    #[test]
    fn a_dead_owner_reads_stale_however_fresh_the_touch() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut dead = task("t-dead", TaskStatus::InProgress);
        dead.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-gone"));
        dead.updated_at = epoch(1_000);
        ws.seed_test_task(dead);
        let mut live = task("t-live", TaskStatus::InProgress);
        live.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        live.updated_at = epoch(1_000);
        ws.seed_test_task(live);
        live_worker(&ws, "w-1");

        let rows = ws.board_rows(PROJECT, epoch(1_001), WINDOW);
        assert!(row(&rows, "t-dead").marks.stale, "a dead holder is nobody on the row");
        assert!(!row(&rows, "t-live").marks.stale, "the live control keeps no mark");
    }

    /// The wait's own age is what makes a row overdue, not the row's last
    /// edit: a waiting row that gains a link or is dragged is no fresher a
    /// wait.
    #[test]
    fn a_waiting_row_past_the_window_reads_overdue() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut waiting = task("t-1", TaskStatus::Waiting);
        waiting.updated_at = epoch(WINDOW + 1);
        waiting.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: Some("which shape".to_owned()),
            on: None,
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(0)),
        });
        ws.seed_test_task(waiting);

        let rows = ws.board_rows(PROJECT, epoch(WINDOW + 1), WINDOW);
        assert!(
            row(&rows, "t-1").marks.overdue,
            "the wait's own age is the signal, however fresh the edit",
        );
    }

    /// A wait parked by design is the user's own deferral, so the row
    /// carries neither stalled mark however old it is; un-parking restores
    /// both from its real age (#2472).
    #[test]
    fn a_parked_wait_carries_neither_stalled_mark() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let wait = |parked: bool| Waiting {
            kind: Some(WaitingKind::Resource),
            detail: Some("needs the owner's dashboard".to_owned()),
            on: None,
            verification: false,
            parked,
            since: Some(epoch(0)),
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
        };
        live_worker(&ws, "w-1");
        let mut parked = task("t-parked", TaskStatus::Waiting);
        parked.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        parked.waiting_on = Some(wait(true));
        parked.updated_at = epoch(0);
        ws.seed_test_task(parked);
        let mut unsettled = task("t-unparked", TaskStatus::Waiting);
        unsettled.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        unsettled.waiting_on = Some(wait(false));
        unsettled.updated_at = epoch(0);
        ws.seed_test_task(unsettled);

        let rows = ws.board_rows(PROJECT, epoch(WINDOW + 1), WINDOW);
        assert!(
            !row(&rows, "t-parked").marks.stale,
            "a parked wait is a deferral, not a row nobody has moved",
        );
        assert!(
            !row(&rows, "t-parked").marks.overdue,
            "and its age is no overrun while it is parked"
        );
        assert!(row(&rows, "t-unparked").marks.stale, "the un-parked control keeps the stale mark");
        assert!(row(&rows, "t-unparked").marks.overdue, "and keeps the overdue mark");

        ws.update_task(PROJECT, &TaskId::from("t-parked"), By::System, |task| {
            if let Some(wait) = task.waiting_on.as_mut() {
                wait.parked = false;
            }
        })
        .expect("no refusal")
        .expect("the row is there");
        let updated_at = ws
            .tasks_for_project(PROJECT)
            .into_iter()
            .find(|task| task.id == TaskId::from("t-parked"))
            .expect("the row survived")
            .updated_at;
        let later = updated_at + Duration::from_secs(WINDOW + 1);
        let rows = ws.board_rows(PROJECT, later, WINDOW);
        let marks = row(&rows, "t-parked").marks;
        assert!(marks.stale, "un-parked, its real age is the mark again");
        assert!(marks.overdue, "both marks come back with the park gone");
    }

    /// A terminal row carries no marks at all: completed, failed and
    /// canceled rows are history, and an attention mark on one is what
    /// kept 37 finished October rows looking like work in play.
    #[test]
    fn a_terminal_row_carries_no_marks() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        for (id, status) in [
            ("t-done", TaskStatus::Completed),
            ("t-failed", TaskStatus::Failed),
            ("t-canceled", TaskStatus::Canceled),
        ] {
            let mut ended = task(id, status);
            // Every condition that raises a mark on a live row, set at
            // once: the terminal row still carries none.
            ended.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-gone"));
            ended.estimate = Some(Estimate { words: "1s".to_owned(), secs: 1 });
            ended.links.push(TaskLink {
                kind: LinkKind::Pr,
                label: None,
                target: "https://example.invalid/pull/1".to_owned(),
                state: Some("open".to_owned()),
                last_check: None,
                added_at: epoch(0),
            });
            ended.updated_at = epoch(0);
            ws.seed_test_task(ended);
        }

        let rows = ws.board_rows(PROJECT, epoch(WINDOW + 1), WINDOW);
        for id in ["t-done", "t-failed", "t-canceled"] {
            let marks = row(&rows, id).marks;
            assert_eq!(marks, Marks::default(), "{id} is over; it owes no attention mark");
        }
    }

    /// A standing row is checked on its cadence, never chased for
    /// overrun: worked time past an estimate is not a mistake it can make.
    #[test]
    fn a_standing_row_is_not_overdue_for_worked_time() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut standing = task("t-standing", TaskStatus::InProgress);
        standing.kind = TaskKind::Standing;
        standing.cadence = Estimate::parse("1d");
        standing.estimate = Some(Estimate { words: "1s".to_owned(), secs: 1 });
        standing.updated_at = epoch(1_000);
        ws.seed_test_task(standing);
        {
            let db = ws.db.lock();
            crate::store::task_history::append(
                db.as_ref().expect("db"),
                &[transition("t-standing", TaskStatus::Pending, TaskStatus::InProgress, 10)],
            )
            .expect("history");
        }

        let rows = ws.board_rows(PROJECT, epoch(1_000), WINDOW);
        let row = row(&rows, "t-standing");
        assert!(row.worked_secs > 1, "precondition: the worked time is past the estimate");
        assert!(!row.marks.overdue, "a standing row is never chased for overrun");
    }

    /// The board reads parents and children in one pass and still fills
    /// both facts a row draws: a grandchild's `parent_subject`, and the
    /// epic's rollup over its direct children only.
    #[test]
    fn the_board_fills_the_hierarchy_in_one_read() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(named("epic", "the epic", PROJECT));
        let mut child = named("sub-a", "the child", PROJECT);
        child.parent = Some(TaskId::from("epic"));
        child.status = TaskStatus::Completed;
        ws.seed_test_task(child);
        let mut grandchild = named("sub-a-1", "the grandchild", PROJECT);
        grandchild.parent = Some(TaskId::from("sub-a"));
        ws.seed_test_task(grandchild);

        let rows = ws.board_rows(PROJECT, epoch(10), WINDOW);
        assert_eq!(
            row(&rows, "sub-a-1").parent_subject.as_deref(),
            Some("the child"),
            "a grandchild names its own parent, not the epic above it",
        );
        assert_eq!(row(&rows, "epic").rollup, Some((1, 1)), "the rollup counts direct children");
        assert_eq!(row(&rows, "sub-a-1").rollup, None, "a leaf has no rollup");
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
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
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

    /// A pick reserved for the user is not ready and not in the queue the
    /// fleet counts (#2423/C): no seat can take it, so it is no work the
    /// project is failing to dispatch.
    #[test]
    fn a_reserved_pick_is_not_ready_and_not_in_the_queue() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        ws.seed_test_task(Task {
            reserved: Some(forge_primitives::tasks::ReservedBy::User),
            ..task("t-kept", TaskStatus::Pending)
        });
        ws.seed_test_task(task("t-plain", TaskStatus::Pending));

        let rows = ws.board_rows(PROJECT, epoch(10), WINDOW);
        assert!(!row(&rows, "t-kept").marks.ready, "a reserved pick never reads ready");
        assert!(row(&rows, "t-plain").marks.ready, "the plain row is the control");
        let fleet = ws.fleet_rows().into_iter().find(|r| r.project == PROJECT).expect("the row");
        assert_eq!(fleet.queue, 1, "only the plain row is dispatchable, so no stall is owed");
    }

    /// A withdrawn park leaves the waiting-on-you presentation (#2340):
    /// the fleet's count drops and the row reads canceled, its reason on
    /// the record.
    #[test]
    fn a_withdrawn_park_leaves_the_waiting_presentation() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut row = task("t-1", TaskStatus::Pending);
        row.verify = Some(Verify::User);
        ws.seed_test_task(row);
        ws.update_task(PROJECT, &TaskId::from("t-1"), By::System, |t| {
            t.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");

        let on_you = |ws: &Workspace| {
            ws.fleet_rows()
                .into_iter()
                .find(|r| r.project == PROJECT)
                .expect("the project's row")
                .waiting_on_user
        };
        assert_eq!(on_you(&ws), 1, "precondition: the park presents as waiting on the user");

        let withdrawn = ws
            .update_task(
                PROJECT,
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", PROJECT)),
                |t| {
                    t.status = TaskStatus::Canceled;
                    t.detail = Some("moot".to_owned());
                },
            )
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(withdrawn.status, TaskStatus::Canceled, "the row shows canceled");
        assert_eq!(on_you(&ws), 0, "and it leaves the waiting-on-you presentation");
        assert_eq!(withdrawn.detail.as_deref(), Some("moot"), "its reason travels on the record");
    }

    /// The fleet's count is the same set the chase digests (#2423): a
    /// verification park belongs to the row's own gate, so the shape an
    /// inherited gate left behind is not counted as waiting on the user.
    #[test]
    fn a_verification_park_without_an_own_gate_is_not_counted() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let parked = |id: &str, verify: Option<Verify>| {
            let mut row = task(id, TaskStatus::Waiting);
            row.verify = verify;
            row.waiting_on = Some(Waiting {
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
            row
        };
        ws.seed_test_task(parked("t-own", Some(Verify::User)));
        ws.seed_test_task(parked("t-inherited", None));

        let on_you = ws
            .fleet_rows()
            .into_iter()
            .find(|r| r.project == PROJECT)
            .expect("the project's row")
            .waiting_on_user;
        assert_eq!(on_you, 1, "the own-gate park counts; the inherited shape does not");
    }

    /// A project's fleet row carries the waits it counts, named for the
    /// surface that raises them (#2756): the row, its seat, its subject, the
    /// ask's own words, and when it began. A resource wait is counted by
    /// neither.
    #[test]
    fn a_fleet_row_carries_the_waits_it_counts() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut decision = task("t-decision", TaskStatus::Waiting);
        decision.subject = "ship the parser".to_owned();
        decision.updated_at = epoch(1_700_000_500);
        decision.owner = Some(SessionSlot::worker("TestOrg", PROJECT, "w-1"));
        decision.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: Some("sign this off".to_owned()),
            on: None,
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(1_700_000_000)),
        });
        ws.seed_test_task(decision);
        let mut resource = task("t-resource", TaskStatus::Waiting);
        resource.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Resource),
            detail: Some("a CI run".to_owned()),
            on: None,
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        });
        ws.seed_test_task(resource);

        let row =
            ws.fleet_rows().into_iter().find(|r| r.project == PROJECT).expect("the project's row");
        assert_eq!(row.waiting_on_user, 1, "only the decision wait is on the user");
        assert_eq!(row.attention.len(), 1, "and only it is carried");
        let item = &row.attention[0];
        assert_eq!(item.task, "t-decision", "the row's own id");
        assert_eq!(item.owner, "w-1", "the seat that raised it");
        assert_eq!(item.subject, "ship the parser", "the row's subject");
        assert_eq!(item.words, "sign this off", "the ask's own words");
        assert_eq!(item.at, epoch(1_700_000_000), "the moment the wait began");
    }

    /// **The wait is dated from its own start, not the row's last edit.** A
    /// waiting row that gains a link, is parked or is dragged moves
    /// `updated_at`, and a notification keyed on that fires again for an edit
    /// that answered nothing.
    #[test]
    fn a_wait_is_dated_from_its_own_start_not_its_last_edit() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut decision = task("t-decision", TaskStatus::Waiting);
        decision.updated_at = epoch(1_700_000_500);
        decision.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: Some("sign this off".to_owned()),
            on: None,
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: Some(epoch(1_700_000_000)),
        });
        ws.seed_test_task(decision);

        let row =
            ws.fleet_rows().into_iter().find(|r| r.project == PROJECT).expect("the project's row");
        assert_eq!(
            row.attention[0].at,
            epoch(1_700_000_000),
            "the wait's own start, not the edit that moved the row",
        );
    }

    /// **A wait stored before the field existed reads as its creation.** The
    /// fallback is `created_at`, never `updated_at`: a row edited after it
    /// began must not re-notify.
    #[test]
    fn a_wait_with_no_stamp_falls_back_to_its_creation() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());
        let mut decision = task("t-decision", TaskStatus::Waiting);
        decision.created_at = epoch(1_700_000_000);
        decision.updated_at = epoch(1_700_000_500);
        decision.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: Some("sign this off".to_owned()),
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

        let row =
            ws.fleet_rows().into_iter().find(|r| r.project == PROJECT).expect("the project's row");
        assert_eq!(row.attention[0].at, epoch(1_700_000_000), "the row's creation, not its edit");
    }

    /// **A wait with no words still says something.** A verification park and
    /// a detail-less decision wait would otherwise draw a blank line and
    /// notify with an empty body; the row's detail, the look a park is owed
    /// and the subject are each a truer answer than nothing.
    #[test]
    fn a_wait_without_words_falls_back_to_the_row() {
        let dir = tempdir().expect("tempdir");
        let ws = fixture(dir.path());

        let mut blank = task("t-blank", TaskStatus::Waiting);
        blank.detail = Some("the plan is in the row".to_owned());
        blank.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
            detail: Some("   ".to_owned()),
            on: None,
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        });
        ws.seed_test_task(blank);

        let mut look = task("t-look", TaskStatus::Waiting);
        look.verify = Some(Verify::User);
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

        let mut bare = task("t-bare", TaskStatus::Waiting);
        bare.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
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
        ws.seed_test_task(bare);

        // Nothing requires a subject to be non-empty: a blank one still has
        // words rather than an empty notification body.
        let mut nameless = task("t-nameless", TaskStatus::Waiting);
        nameless.subject = "   ".to_owned();
        nameless.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::User),
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
        ws.seed_test_task(nameless);

        let row =
            ws.fleet_rows().into_iter().find(|r| r.project == PROJECT).expect("the project's row");
        let words = |task: &str| {
            row.attention.iter().find(|item| item.task == task).expect("the item").words.clone()
        };
        assert_eq!(words("t-blank"), "the plan is in the row", "a blank detail takes the row's");
        assert_eq!(words("t-look"), "the look is owed", "a verification park says what it wants");
        assert_eq!(
            words("t-nameless"),
            "A decision is waiting on you.",
            "a blank subject still words the ask"
        );
        assert_eq!(words("t-bare"), "subject t-bare", "nothing anywhere takes the subject");
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
            last_check: None,
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
