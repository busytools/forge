//! The task cluster on [`Workspace`]: the durable task store accessor
//! and its mutation helpers, a second `impl` block beside `crons.rs` so
//! every caller keeps its path.
//!
//! The core polices the moves here, so no caller can talk a row into an
//! impossible state: the verify gate lives in the transition into
//! completed, closing is completing a root, a claim is one atomic write,
//! and every status change lands in the history.

use std::time::SystemTime;

use forge_primitives::tasks::Verify;
use forge_primitives::tasks::{By, Task, TaskId, TaskStatus, TaskTransition, Waiting, WaitingKind};

use crate::SessionSlot;
use crate::protocol::RankMove;
use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;

/// Why a claim was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimError {
    /// No row carries the id (or the epic has nothing ready).
    NotFound,
    /// The row is held by a live seat, named.
    OwnedByLiveSeat(String),
    /// The row is waiting on something, or nothing ready was there to take.
    NotReady,
    /// The caller already has a row in progress.
    AlreadyWorking,
}

/// Why a move was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveError {
    /// Completing a root row while its children are open; cancel them
    /// first to abandon the epic.
    OpenChildren(usize),
    /// The caller does not own the row it is trying to wait.
    NotOwner,
    /// No row carries the id.
    NotFound,
    /// The user's verdict landed on a row that is not waiting on their
    /// look.
    NotWaitingVerification,
    /// The user answered a row that is not waiting on a question.
    NotWaitingQuestion,
}

/// Append the user's words to a row's detail, keeping what was there.
fn append_words(task: &mut Task, words: &str) {
    let prior = task.detail.take().unwrap_or_default();
    task.detail =
        Some(if prior.is_empty() { words.to_owned() } else { format!("{prior}\n{words}") });
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no such row"),
            Self::OwnedByLiveSeat(label) => write!(f, "{label} holds this row"),
            Self::NotReady => write!(f, "nothing ready to claim there"),
            Self::AlreadyWorking => write!(f, "you already have a row in progress"),
        }
    }
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenChildren(open) => {
                write!(f, "{open} children are still open; cancel them first to abandon the epic")
            }
            Self::NotOwner => write!(f, "you do not own this row"),
            Self::NotFound => write!(f, "no such row"),
            Self::NotWaitingVerification => write!(f, "this row is not waiting on your look"),
            Self::NotWaitingQuestion => write!(f, "this row is not waiting on a question"),
        }
    }
}

/// Whether a status is an end: nothing more happens on the row.
fn is_terminal(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Canceled)
}

/// The verify a row is bound by: its own, else its epic's, else none.
fn effective_verify(tasks: &[Task], index: usize) -> Verify {
    if let Some(verify) = tasks[index].verify {
        return verify;
    }
    tasks[index]
        .parent
        .as_ref()
        .and_then(|parent| tasks.iter().find(|t| t.id == *parent))
        .and_then(|parent| parent.verify)
        .unwrap_or(Verify::None)
}

impl Workspace {
    /// Lock the task list, apply `f`, and persist. Every mutation routes
    /// through here so the in-memory set and the store never diverge.
    pub(crate) fn with_tasks_mut<R>(&self, f: impl FnOnce(&mut Vec<Task>) -> R) -> R {
        let mut tasks = self.tasks.lock();
        let result = f(&mut tasks);
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::tasks::replace_all(db, &tasks)
        {
            tracing::error!(
                target: "forge_workspace::tasks",
                %error,
                "persisting tasks to the store failed; a task may be lost on restart",
            );
        }
        result
    }

    /// Append a task and persist, recording its creation in the history.
    /// Backs `tasks__create`.
    pub(crate) fn push_task(&self, task: Task) {
        let project_name = task.project_name.clone();
        let recorded = task.clone();
        self.with_tasks_mut(|tasks| tasks.push(task));
        self.record_transition(&project_name, &recorded.id, None, recorded.status, By::System);
        self.announce_tasks_changed(&project_name);
    }

    /// Apply `f` to the task `id` in `project_name`, persist, record the
    /// transition, and return the record as the write left it (`None`
    /// when no such task is there). Open to every session in the project.
    ///
    /// Two rules ride on top of `f`, so no caller can forget them: a
    /// transition into `completed` on a verify=user row lands in
    /// `waiting` (the verify gate), and completing a ROOT row closes it -
    /// the whole tree moves to the archive, refused while any child is
    /// still open.
    pub(crate) fn update_task(
        &self,
        project_name: &str,
        id: &TaskId,
        by: By,
        f: impl FnOnce(&mut Task),
    ) -> Result<Option<Task>, MoveError> {
        self.move_task(project_name, id, by, true, |task| {
            f(task);
            Ok(())
        })
    }

    /// The one commit path every task move runs through. `gate` is the
    /// verify gate: it fires on a transition into `completed`, and the
    /// user's own approval is the one move that passes it (approving IS
    /// the verification). `f` may refuse before anything moves.
    fn move_task(
        &self,
        project_name: &str,
        id: &TaskId,
        by: By,
        gate: bool,
        f: impl FnOnce(&mut Task) -> Result<(), MoveError>,
    ) -> Result<Option<Task>, MoveError> {
        let at = SystemTime::now();
        let outcome = self.with_tasks_mut(|tasks| {
            let Some(index) =
                tasks.iter().position(|t| t.id == *id && t.project_name == project_name)
            else {
                return Ok(None);
            };
            let mut next = tasks[index].clone();
            let prior = next.status;
            f(&mut next)?;

            if gate
                && next.status == TaskStatus::Completed
                && effective_verify(tasks, index) == Verify::User
            {
                next.waiting_on = Some(Waiting {
                    kind: Some(WaitingKind::Decision),
                    detail: None,
                    on: None,
                    verification: true,
                });
                next.status = TaskStatus::Waiting;
            }

            let closing = next.parent.is_none() && next.status == TaskStatus::Completed;
            let mut removed: Vec<Task> = Vec::new();
            if closing {
                let open = tasks
                    .iter()
                    .filter(|t| {
                        t.project_name == project_name
                            && t.parent.as_ref() == Some(&next.id)
                            && !is_terminal(t.status)
                    })
                    .count();
                if open > 0 {
                    return Err(MoveError::OpenChildren(open));
                }
                next.archived_at = Some(at);
                // Collect the whole tree, deepest rows included.
                let mut doomed = vec![next.id.clone()];
                let mut cursor = 0;
                while cursor < doomed.len() {
                    let current = doomed[cursor].clone();
                    cursor += 1;
                    for task in tasks.iter() {
                        if task.project_name == project_name
                            && task.parent.as_ref() == Some(&current)
                            && !doomed.contains(&task.id)
                        {
                            doomed.push(task.id.clone());
                        }
                    }
                }
                removed = tasks
                    .iter()
                    .filter(|t| t.project_name == project_name && doomed.contains(&t.id))
                    .cloned()
                    .collect();
                for task in &mut removed {
                    task.archived_at = Some(at);
                }
                tasks.retain(|t| !(t.project_name == project_name && doomed.contains(&t.id)));
            }

            next.updated_at = at;
            if removed.is_empty() {
                tasks[index] = next.clone();
            } else if let Some(row) = removed.iter_mut().find(|t| t.id == next.id) {
                *row = next.clone();
            }
            Ok(Some((next, prior, removed)))
        })?;

        let Some((task, prior, removed)) = outcome else { return Ok(None) };
        if !removed.is_empty()
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::task_archive::append(db, &removed)
        {
            tracing::error!(
                target: "forge_workspace::tasks",
                %error,
                "archiving a closed tree failed; the rows are gone from the live set",
            );
        }
        if prior != task.status {
            self.record_transition(project_name, &task.id, Some(prior), task.status, by);
        }
        if is_terminal(task.status) {
            self.resolve_wait_on(project_name, &task.id);
        }
        self.announce_tasks_changed(project_name);
        Ok(Some(task))
    }

    /// Claim the top of a queue: the row `by_id`, or the first ready row
    /// of `in_epic` by rank. Owner and in_progress land in one write, the
    /// attempt ticks, and the claim is refused - by name - for a row a
    /// live seat holds, a row that is waiting, or a caller already
    /// working another row.
    /// The user's approval of a row waiting on their look: the wait
    /// clears, the verify gate is satisfied, and the row completes - a
    /// root closing its tree like any completion.
    pub(crate) fn approve_task(
        &self,
        project_name: &str,
        id: &TaskId,
    ) -> Result<Option<Task>, MoveError> {
        self.move_task(project_name, id, By::User, false, |task| {
            if task.status != TaskStatus::Waiting
                || !task.waiting_on.as_ref().is_some_and(|wait| wait.verification)
            {
                return Err(MoveError::NotWaitingVerification);
            }
            task.waiting_on = None;
            task.verify = Some(Verify::None);
            task.status = TaskStatus::Completed;
            Ok(())
        })
    }

    /// The user sends a verified row back: it returns to its owner in
    /// progress, the words land in the detail, and the attempt ticks.
    pub(crate) fn send_back_task(
        &self,
        project_name: &str,
        id: &TaskId,
        words: &str,
    ) -> Result<Option<Task>, MoveError> {
        self.move_task(project_name, id, By::User, false, |task| {
            if task.status != TaskStatus::Waiting
                || !task.waiting_on.as_ref().is_some_and(|wait| wait.verification)
            {
                return Err(MoveError::NotWaitingVerification);
            }
            task.waiting_on = None;
            task.status = TaskStatus::InProgress;
            task.attempt += 1;
            append_words(task, words);
            Ok(())
        })
    }

    /// The user answers a row waiting on a question: the row resumes in
    /// progress with the words in its detail.
    pub(crate) fn answer_task(
        &self,
        project_name: &str,
        id: &TaskId,
        words: &str,
    ) -> Result<Option<Task>, MoveError> {
        self.move_task(project_name, id, By::User, false, |task| {
            if task.status != TaskStatus::Waiting
                || task.waiting_on.as_ref().is_some_and(|wait| wait.verification)
            {
                return Err(MoveError::NotWaitingQuestion);
            }
            task.waiting_on = None;
            task.status = TaskStatus::InProgress;
            append_words(task, words);
            Ok(())
        })
    }

    /// Move a row in its queue: `Top` above everything, `Up` and `Down`
    /// one place. The whole queue is renumbered in one write, so the
    /// order a reader sees is the order the next read returns.
    pub(crate) fn rank_task(
        &self,
        project_name: &str,
        id: &TaskId,
        to: RankMove,
    ) -> Result<Option<Task>, MoveError> {
        let at = SystemTime::now();
        let outcome = self.with_tasks_mut(|tasks| {
            let index = tasks.iter().position(|t| t.id == *id && t.project_name == project_name)?;
            // The queue: this project's rows in the order the board reads
            // them (rank, then creation).
            let mut queue: Vec<usize> = tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| t.project_name == project_name)
                .map(|(i, _)| i)
                .collect();
            queue.sort_by_key(|i| (tasks[*i].rank.unwrap_or(i64::MAX), tasks[*i].created_at));
            let position = queue.iter().position(|i| *i == index)?;
            let target = match to {
                RankMove::Top => 0,
                RankMove::Up => position.saturating_sub(1),
                RankMove::Down => (position + 1).min(queue.len() - 1),
            };
            let moved = queue.remove(position);
            queue.insert(target, moved);
            for (rank, i) in queue.iter().enumerate() {
                tasks[*i].rank = Some(i64::try_from(rank).unwrap_or(i64::MAX));
            }
            tasks[index].updated_at = at;
            Some(tasks[index].clone())
        });
        if outcome.is_some() {
            self.announce_tasks_changed(project_name);
        }
        Ok(outcome)
    }

    /// Give a row an owner, or take it back.
    pub(crate) fn assign_task(
        &self,
        project_name: &str,
        id: &TaskId,
        owner: Option<SessionSlot>,
    ) -> Result<Option<Task>, MoveError> {
        self.move_task(project_name, id, By::User, false, |task| {
            task.owner = owner;
            Ok(())
        })
    }

    pub(crate) fn claim_task(
        &self,
        project_name: &str,
        slot: &SessionSlot,
        by_id: Option<&TaskId>,
        in_epic: Option<&TaskId>,
    ) -> Result<Task, ClaimError> {
        let outcome = self.with_tasks_mut(|tasks| {
            let already = tasks.iter().any(|t| {
                t.project_name == project_name
                    && t.status == TaskStatus::InProgress
                    && t.owner.as_ref().map(SessionSlot::label) == Some(slot.label())
            });
            if already {
                return Err(ClaimError::AlreadyWorking);
            }
            let index = match (by_id, in_epic) {
                (Some(id), _) => tasks
                    .iter()
                    .position(|t| t.project_name == project_name && t.id == *id)
                    .ok_or(ClaimError::NotFound)?,
                (None, Some(epic)) => {
                    let mut ready: Vec<usize> = tasks
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| {
                            t.project_name == project_name
                                && t.parent.as_ref() == Some(epic)
                                && t.status == TaskStatus::Pending
                                && t.waiting_on.is_none()
                                && t.owner.is_none()
                        })
                        .map(|(i, _)| i)
                        .collect();
                    ready.sort_by_key(|i| {
                        (tasks[*i].rank.unwrap_or(i64::MAX), tasks[*i].created_at)
                    });
                    ready.first().copied().ok_or(ClaimError::NotReady)?
                }
                (None, None) => return Err(ClaimError::NotReady),
            };
            let status = tasks[index].status;
            let waiting = tasks[index].waiting_on.is_some();
            if status != TaskStatus::Pending || waiting {
                return Err(ClaimError::NotReady);
            }
            let holder = tasks[index].owner.clone();
            if let Some(holder) = holder
                && holder.label() != slot.label()
                && self.seat_is_live(&holder)
            {
                return Err(ClaimError::OwnedByLiveSeat(holder.label().to_owned()));
            }
            let prior = tasks[index].status;
            tasks[index].owner = Some(slot.clone());
            tasks[index].status = TaskStatus::InProgress;
            tasks[index].attempt += 1;
            tasks[index].updated_at = SystemTime::now();
            Ok((tasks[index].clone(), prior))
        });
        let (task, prior) = outcome?;
        self.record_transition(
            project_name,
            &task.id,
            Some(prior),
            task.status,
            By::Seat(slot.clone()),
        );
        self.announce_tasks_changed(project_name);
        Ok(task)
    }

    /// Put a row the caller owns into `waiting`, stating what it waits on.
    pub(crate) fn wait_task(
        &self,
        project_name: &str,
        id: &TaskId,
        slot: &SessionSlot,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
    ) -> Result<Task, MoveError> {
        let at = SystemTime::now();
        let outcome = self.with_tasks_mut(|tasks| {
            let Some(index) =
                tasks.iter().position(|t| t.id == *id && t.project_name == project_name)
            else {
                return Err(MoveError::NotFound);
            };
            let owner = tasks[index].owner.as_ref().map(SessionSlot::label).map(str::to_owned);
            if owner.as_deref() != Some(slot.label()) {
                return Err(MoveError::NotOwner);
            }
            let prior = tasks[index].status;
            tasks[index].waiting_on =
                Some(Waiting { kind: Some(kind), detail, on: on.cloned(), verification: false });
            tasks[index].status = TaskStatus::Waiting;
            tasks[index].updated_at = at;
            Ok((tasks[index].clone(), prior))
        })?;
        let (task, prior) = outcome;
        if prior != task.status {
            self.record_transition(
                project_name,
                &task.id,
                Some(prior),
                task.status,
                By::Seat(slot.clone()),
            );
        }
        self.announce_tasks_changed(project_name);
        Ok(task)
    }

    /// Clear every wait on `blocker` - any terminal end of it counts, and
    /// so does its deletion - and hand the released rows back so the
    /// caller can message their owners.
    pub(crate) fn resolve_wait_on(&self, project_name: &str, blocker: &TaskId) -> Vec<Task> {
        let released = self.with_tasks_mut(|tasks| {
            let mut released: Vec<Task> = Vec::new();
            for task in tasks.iter_mut() {
                if task.project_name != project_name || task.status != TaskStatus::Waiting {
                    continue;
                }
                let waits_on_blocker =
                    task.waiting_on.as_ref().and_then(|w| w.on.as_ref()) == Some(blocker);
                if !waits_on_blocker {
                    continue;
                }
                task.waiting_on = None;
                task.status = TaskStatus::Pending;
                task.updated_at = SystemTime::now();
                released.push(task.clone());
            }
            released
        });
        for task in &released {
            self.record_transition(
                project_name,
                &task.id,
                Some(TaskStatus::Waiting),
                TaskStatus::Pending,
                By::System,
            );
        }
        if !released.is_empty() {
            self.announce_tasks_changed(project_name);
        }
        released
    }

    /// Whether a seat has something behind it right now: a registered
    /// domain or agent, or a live worker registry row.
    pub(crate) fn seat_is_live(&self, slot: &SessionSlot) -> bool {
        if self.domain_session_for(slot).is_some() || self.has_agent_for(slot) {
            return true;
        }
        let key = self
            .list_projects()
            .into_iter()
            .find(|view| view.name == slot.project())
            .map(|view| view.key);
        let Some(key) = key else { return false };
        self.list_live_workers(&key).iter().any(|entry| &entry.slot == slot)
    }

    /// Append one status transition to the durable history.
    fn record_transition(
        &self,
        project_name: &str,
        id: &TaskId,
        from: Option<TaskStatus>,
        to: TaskStatus,
        by: By,
    ) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        let transition = TaskTransition {
            task_id: id.clone(),
            project_name: project_name.to_owned(),
            from,
            to,
            at: SystemTime::now(),
            by,
        };
        if let Err(error) = crate::store::task_history::append(db, &[transition]) {
            tracing::error!(
                target: "forge_workspace::tasks",
                %error,
                "persisting task history failed; a move is missing from the history",
            );
        }
    }

    /// Remove the task `id` in `project_name` together with every
    /// descendant, persist, and return the records removed (empty when
    /// nothing was). Backs `tasks__delete`. The cascade happens here
    /// rather than at the call site so a parent can never be deleted out
    /// from under children that would then have nobody to close them.
    pub(crate) fn remove_task_tree(&self, project_name: &str, id: &TaskId) -> Vec<Task> {
        let removed = self.with_tasks_mut(|tasks| {
            let mut doomed = vec![id.clone()];
            let mut cursor = 0;
            while cursor < doomed.len() {
                let current = doomed[cursor].clone();
                cursor += 1;
                for task in tasks.iter() {
                    if task.project_name == project_name
                        && task.parent.as_ref() == Some(&current)
                        && !doomed.contains(&task.id)
                    {
                        doomed.push(task.id.clone());
                    }
                }
            }
            let mut removed = Vec::new();
            tasks.retain(|t| {
                let taking = t.project_name == project_name && doomed.contains(&t.id);
                if taking {
                    removed.push(t.clone());
                }
                !taking
            });
            removed
        });
        if !removed.is_empty() {
            // A removed row can never end, so its waiters are released
            // here - the other half of "any terminal end clears the wait".
            for task in &removed {
                self.resolve_wait_on(project_name, &task.id);
            }
            self.announce_tasks_changed(project_name);
        }
        removed
    }

    /// Tell every view to re-read its board: the set is unchanged, but
    /// time-derived marks (ages, overdue, no movement) move with the
    /// clock, and a held snapshot would otherwise draw them frozen.
    /// Called by the chase sweep on every pass.
    pub(crate) fn announce_board_refresh(&self) {
        for view in self.list_projects() {
            self.announce_tasks_changed(&view.name);
        }
    }

    /// Tell every view the project's task set moved, as the set the write
    /// just left. It routes on the project's lead seat, which is the seat a
    /// project-scoped section belongs to; a name no project carries has no
    /// seat to route on and announces nothing.
    pub(crate) fn announce_tasks_changed(&self, project_name: &str) {
        let Some(key) = self.lead_slot_for_project(project_name) else {
            tracing::debug!(
                target: "forge_workspace::tasks",
                event_name = "tasks_changed_unroutable",
                project = %project_name,
                "a task write named a project no project carries; nothing was announced",
            );
            return;
        };
        let tasks = self.tasks_for_project(project_name);
        let _ = self.update_tx.send(SessionUpdate::TasksChanged { key, tasks });
    }

    /// The tasks registered for `project_name`. Backs `tasks__list` and
    /// the Inspector TASKS snapshot.
    pub fn tasks_for_project(&self, project_name: &str) -> Vec<Task> {
        self.tasks.lock().iter().filter(|t| t.project_name == project_name).cloned().collect()
    }
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Register a task directly, bypassing the MCP create path.
    /// Cross-crate test access to the otherwise `pub(crate)` task store
    /// so forge-tui can exercise the Inspector's `refresh_tasks`
    /// resolution against a seeded task.
    ///
    /// The store write without the announcement: a fixture seeds state, it
    /// does not perform the write a view is owed a frame for.
    pub fn seed_test_task(&self, task: forge_primitives::tasks::Task) {
        self.with_tasks_mut(|tasks| tasks.push(task));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionSlot, SessionUpdate};
    use forge_primitives::tasks::{By, Task, TaskId, TaskStatus, Waiting, WaitingKind};
    use tempfile::tempdir;

    fn install_db(ws: &Workspace, dir: &std::path::Path) {
        let db = crate::store::Db::open(&dir.join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
    }

    fn live_worker(ws: &Workspace, project: &str, label: &str) {
        let key = ws
            .list_projects()
            .into_iter()
            .find(|view| view.name == project)
            .expect("seeded project")
            .key
            .clone();
        ws.insert_live_worker(
            &key,
            crate::WorkerEntry {
                label: label.to_owned(),
                charter: "charter".to_owned(),
                slot: SessionSlot::worker("TestOrg", project, label),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead("TestOrg", project),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
    }

    fn history_of(ws: &Workspace, project: &str) -> Vec<forge_primitives::tasks::TaskTransition> {
        let db = ws.db.lock();
        crate::store::task_history::list_for_project(db.as_ref().expect("db installed"), project)
            .expect("history")
    }

    fn archived_of(ws: &Workspace, project: &str) -> Vec<Task> {
        let db = ws.db.lock();
        crate::store::task_archive::list_for_project(db.as_ref().expect("db installed"), project)
            .expect("archive")
    }

    /// The one `TasksChanged` on a test's update stream, as its key and the
    /// set it announces.
    fn next_tasks_changed(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> (SessionSlot, Vec<Task>) {
        match rx.try_recv() {
            Ok(SessionUpdate::TasksChanged { key, tasks }) => (key, tasks),
            other => panic!("expected a TasksChanged on the stream, got {other:?}"),
        }
    }

    fn subjects(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|task| task.subject.clone()).collect()
    }

    fn sample_task(id: &str, project: &str) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: project.to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status: TaskStatus::Pending,
            owner: None,
            parent: None,
            waiting_on: None,
            estimate: None,
            rank: None,
            verify: None,
            links: Vec::new(),
            attempt: 0,
            archived_at: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn sample_task_with_parent(id: &str, parent: Option<&str>, project: &str) -> Task {
        Task { parent: parent.map(TaskId::from), ..sample_task(id, project) }
    }

    #[test]
    fn a_task_can_be_updated_by_a_session_that_does_not_own_it() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        // A child row: a completing ROOT closes and archives (own test).
        let mut seeded = sample_task("t-1", "forge");
        seeded.parent = Some(TaskId::from("epic"));
        ws.push_task(seeded);
        let changed = ws
            .update_task(
                "forge",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "forge")),
                |t| {
                    t.status = TaskStatus::Completed;
                },
            )
            .expect("no refusal");
        assert!(changed.is_some(), "any session in the project may move a task");
        assert_eq!(ws.tasks_for_project("forge")[0].status, TaskStatus::Completed);
    }

    #[test]
    fn tasks_for_project_does_not_leak_across_projects() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.push_task(sample_task("t-1", "forge"));
        assert_eq!(ws.tasks_for_project("forge").len(), 1, "listed for its own project");
        assert!(ws.tasks_for_project("other").is_empty(), "scoped by project name");
    }

    #[test]
    fn a_write_persists_to_the_store() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws.push_task(sample_task("t-1", "forge"));
        let stored =
            crate::store::tasks::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
        assert_eq!(stored.len(), 1, "the in-memory set and the store never diverge");
    }

    /// Deleting is durable, not just in-memory: the tree has to leave the
    /// store too, or the next boot reads it back.
    ///
    /// The two cross-project tasks are what pin the cascade's project
    /// guards, and nothing else reaches them: a same-project task outside
    /// the tree is never a cascade candidate, and its id is never doomed.
    /// One shares the doomed id, so the `retain` would take it without its
    /// own project check; the other is a cross-project child of a doomed id
    /// whose id collides with a task that has to survive here, so the
    /// collection would carry that collision into this project and the
    /// retain would then take the survivor.
    #[test]
    fn deleting_a_task_takes_its_children() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws.push_task(sample_task_with_parent("epic", None, "forge"));
        ws.push_task(sample_task_with_parent("sub-a", Some("epic"), "forge"));
        ws.push_task(sample_task_with_parent("sub-b", Some("sub-a"), "forge"));
        // A task in the SAME project but outside the tree: the cascade must
        // take the descendants and nothing else.
        ws.push_task(sample_task("sibling", "forge"));
        ws.push_task(sample_task("epic", "elsewhere"));
        ws.push_task(sample_task_with_parent("sibling", Some("epic"), "elsewhere"));
        assert_eq!(
            ws.remove_task_tree("forge", &TaskId::from("epic")).len(),
            3,
            "the tree is removed: the epic and its two descendants",
        );
        let left = ws.tasks_for_project("forge");
        assert_eq!(
            left.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            vec![TaskId::from("sibling")],
            "children and grandchildren go with the parent, and nothing else does",
        );
        let stored =
            crate::store::tasks::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
        assert!(
            stored.iter().all(|t| t.project_name != "forge" || t.id == TaskId::from("sibling")),
            "the deletion reached the store, not only the in-memory set: {stored:?}",
        );
        assert_eq!(
            ws.tasks_for_project("elsewhere").len(),
            2,
            "another project keeps both of its tasks, one of them sharing a doomed id",
        );
    }

    /// Every task write announces the project's set on the project's lead
    /// seat, so the inspector's TASKS section pops on the frame that moved it
    /// rather than on the next unrelated read.
    ///
    /// The three writers are pinned together because they are one section's
    /// three doors. Mutants: drop any one emission (the drain is empty), or
    /// announce a set taken before the write (the announced set disagrees
    /// with the store).
    #[test]
    fn every_task_write_announces_the_projects_set() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-tasks");
        let lead = SessionSlot::lead("TestOrg", "proj");

        // A child row: completing a ROOT would close and archive the tree,
        // which is its own test; this one is about the announcements.
        let mut seeded = sample_task("t-1", "proj");
        seeded.parent = Some(TaskId::from("epic"));
        ws.push_task(seeded);
        let (key, tasks) = next_tasks_changed(&mut rx);
        assert_eq!(key, lead, "a create routes on the project's lead seat");
        assert_eq!(subjects(&tasks), ["subject t-1"], "and carries the set the core now holds");

        assert!(
            ws.update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                }
            )
            .expect("no refusal")
            .is_some()
        );
        let (key, tasks) = next_tasks_changed(&mut rx);
        assert_eq!(key, lead, "an update routes the same way");
        assert_eq!(tasks.len(), 1, "and announces the whole set");
        assert_eq!(tasks[0].status, TaskStatus::Completed, "the write's own change included");

        assert!(!ws.remove_task_tree("proj", &TaskId::from("t-1")).is_empty());
        let (_, tasks) = next_tasks_changed(&mut rx);
        assert!(tasks.is_empty(), "a delete announces the set it left behind");
    }

    /// A write that changed nothing emits nothing: an update for an id no
    /// task carries and a delete of one nothing holds are both refusals, and
    /// an announcement would be news about a write that never happened.
    ///
    /// Mutant: announce unconditionally, where the refused pair is news the
    /// section never moved for.
    #[test]
    fn a_task_write_that_changed_nothing_announces_nothing() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-tasks-noop");

        assert!(
            ws.update_task(
                "proj",
                &TaskId::from("ghost"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                }
            )
            .expect("no refusal")
            .is_none(),
            "precondition: no task carries the id, so the update is refused",
        );
        assert!(
            ws.remove_task_tree("proj", &TaskId::from("ghost")).is_empty(),
            "precondition: nothing holds the id, so the delete is refused",
        );

        let announced = rx.try_recv();
        assert!(
            announced.is_err(),
            "a refused update and a refused delete announce nothing, and this was announced: \
             {announced:?}",
        );
    }

    /// A row waiting on the user's look, reached the way the gate produces
    /// one: a verify=user row completing.
    fn row_awaiting_look(ws: &Workspace, id: &str) {
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task(id, "proj") });
        ws.update_task("proj", &TaskId::from(id), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");
    }

    #[test]
    fn a_user_verdict_completes_with_by_user_in_the_history() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-approve");
        install_db(&ws, dir.path());
        row_awaiting_look(&ws, "t-1");

        let approved = ws
            .approve_task("proj", &TaskId::from("t-1"))
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(approved.status, TaskStatus::Completed, "approval completes the row");
        assert_eq!(approved.waiting_on, None, "and clears the wait");
        assert_eq!(approved.verify, Some(Verify::None), "the gate is satisfied, not re-armed");
        let history = history_of(&ws, "proj");
        let last = history.last().expect("a last transition");
        assert_eq!((last.from, last.to), (Some(TaskStatus::Waiting), TaskStatus::Completed));
        assert_eq!(last.by, By::User, "the verdict is stamped as the user's own move");
    }

    #[test]
    fn a_send_back_returns_the_row_and_ticks_the_attempt() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-sendback");
        install_db(&ws, dir.path());
        row_awaiting_look(&ws, "t-1");

        let sent = ws
            .send_back_task("proj", &TaskId::from("t-1"), "the rail elides wrong")
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(sent.status, TaskStatus::InProgress, "back to its owner's hands");
        assert_eq!(sent.attempt, 1, "the send-back ticks the attempt");
        assert_eq!(
            sent.detail.as_deref(),
            Some("the rail elides wrong"),
            "and the words are in the detail",
        );
    }

    #[test]
    fn an_answer_resumes_a_question_wait() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-answer");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.claim_task("proj", &worker, Some(&TaskId::from("t-1")), None).expect("claim");
        ws.wait_task("proj", &TaskId::from("t-1"), &worker, WaitingKind::Decision, None, None)
            .expect("wait");

        let answered = ws
            .answer_task("proj", &TaskId::from("t-1"), "keep the window raise")
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(answered.status, TaskStatus::InProgress, "the answer resumes the row");
        assert_eq!(answered.waiting_on, None);
        assert_eq!(answered.detail.as_deref(), Some("keep the window raise"));
    }

    #[test]
    fn a_verdict_on_a_row_not_waiting_for_look_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-refuse-verdict");
        ws.push_task(sample_task("t-1", "proj"));

        assert_eq!(
            ws.approve_task("proj", &TaskId::from("t-1")),
            Err(MoveError::NotWaitingVerification),
            "a pending row is not waiting on a look",
        );
        assert_eq!(
            ws.answer_task("proj", &TaskId::from("t-1"), "words"),
            Err(MoveError::NotWaitingQuestion),
        );
    }

    #[test]
    fn rank_up_swaps_against_the_neighbour() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-rank");
        for (id, rank) in [("first", 0), ("second", 1), ("third", 2)] {
            ws.push_task(Task { rank: Some(rank), ..sample_task(id, "proj") });
        }

        ws.rank_task("proj", &TaskId::from("third"), RankMove::Up).expect("no refusal");
        let order: Vec<String> = {
            let mut rows: Vec<(i64, String)> = ws
                .tasks_for_project("proj")
                .into_iter()
                .map(|t| (t.rank.unwrap_or(i64::MAX), t.id.as_str().to_owned()))
                .collect();
            rows.sort();
            rows.into_iter().map(|(_, id)| id).collect()
        };
        assert_eq!(
            order,
            vec!["first", "third", "second"],
            "up moves the row one place toward the front, past its neighbour",
        );

        ws.rank_task("proj", &TaskId::from("third"), RankMove::Top).expect("no refusal");
        let top = ws
            .tasks_for_project("proj")
            .into_iter()
            .min_by_key(|t| t.rank.unwrap_or(i64::MAX))
            .expect("a row");
        assert_eq!(top.id, TaskId::from("third"), "top puts it above everything");
    }

    #[test]
    fn claim_sets_owner_and_in_progress_in_one_write_and_ticks_the_attempt() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim");
        install_db(&ws, dir.path());
        ws.push_task(sample_task("t-1", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");

        let claimed = ws
            .claim_task("proj", &worker, Some(&TaskId::from("t-1")), None)
            .expect("the row is free");
        assert_eq!(claimed.owner.as_ref().map(SessionSlot::label), Some("w-1"));
        assert_eq!(claimed.status, TaskStatus::InProgress, "owner and status in one write");
        assert_eq!(claimed.attempt, 1, "each claim ticks the attempt");
        let history = history_of(&ws, "proj");
        assert_eq!(history.len(), 2, "creation and the claim: {history:?}");
        let claim = history.last().expect("the claim's transition");
        assert_eq!(claim.from, Some(TaskStatus::Pending));
        assert_eq!(claim.to, TaskStatus::InProgress);
        assert_eq!(claim.by, By::Seat(worker));
    }

    #[test]
    fn a_claim_of_a_live_seats_row_is_refused_by_name() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-live");
        live_worker(&ws, "proj", "w-1");
        let mut task = sample_task("t-1", "proj");
        task.owner = Some(SessionSlot::worker("TestOrg", "proj", "w-1"));
        ws.push_task(task);
        let other = SessionSlot::worker("TestOrg", "proj", "w-2");

        assert_eq!(
            ws.claim_task("proj", &other, Some(&TaskId::from("t-1")), None),
            Err(ClaimError::OwnedByLiveSeat("w-1".to_owned())),
            "a live seat's row is refused by name",
        );
    }

    /// A dead owner's row is nobody's: a claim on it is the explicit
    /// re-dispatch the design asks for, not a race.
    #[test]
    fn a_dead_owners_row_can_be_reclaimed() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-dead");
        let mut task = sample_task("t-1", "proj");
        task.owner = Some(SessionSlot::worker("TestOrg", "proj", "gone"));
        ws.push_task(task);
        let worker = SessionSlot::worker("TestOrg", "proj", "w-2");

        let claimed = ws
            .claim_task("proj", &worker, Some(&TaskId::from("t-1")), None)
            .expect("a dead owner does not hold a row");
        assert_eq!(claimed.owner.as_ref().map(SessionSlot::label), Some("w-2"));
    }

    #[test]
    fn a_second_in_progress_row_for_one_worker_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-wip");
        ws.push_task(sample_task("t-1", "proj"));
        ws.push_task(sample_task("t-2", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.claim_task("proj", &worker, Some(&TaskId::from("t-1")), None).expect("first claim");

        assert_eq!(
            ws.claim_task("proj", &worker, Some(&TaskId::from("t-2")), None),
            Err(ClaimError::AlreadyWorking),
            "one in progress per worker",
        );
    }

    #[test]
    fn claim_by_epic_takes_the_top_ready_row_by_rank() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-rank");
        ws.push_task(sample_task_with_parent("epic", None, "proj"));
        ws.push_task(Task {
            rank: Some(20),
            ..sample_task_with_parent("late", Some("epic"), "proj")
        });
        ws.push_task(Task {
            rank: Some(10),
            ..sample_task_with_parent("early", Some("epic"), "proj")
        });
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");

        let claimed = ws
            .claim_task("proj", &worker, None, Some(&TaskId::from("epic")))
            .expect("a ready row is there");
        assert_eq!(claimed.id, TaskId::from("early"), "the top ready row by rank");
    }

    #[test]
    fn a_waiting_row_is_not_claimable_until_its_wait_resolves() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-wait");
        ws.push_task(sample_task("blocker", "proj"));
        ws.push_task(sample_task("t-2", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.claim_task("proj", &worker, Some(&TaskId::from("t-2")), None)
            .expect("claim the row before waiting it");
        ws.wait_task(
            "proj",
            &TaskId::from("t-2"),
            &worker,
            WaitingKind::Dependency,
            Some("the board read has to land first".to_owned()),
            Some(&TaskId::from("blocker")),
        )
        .expect("the owner may wait its own row");
        assert_eq!(
            ws.claim_task("proj", &worker, Some(&TaskId::from("t-2")), None),
            Err(ClaimError::NotReady),
            "a waiting row is not claimable",
        );

        // The blocker ending - canceled, not completed - clears the wait.
        ws.update_task(
            "proj",
            &TaskId::from("blocker"),
            By::Seat(SessionSlot::lead("TestOrg", "proj")),
            |task| task.status = TaskStatus::Canceled,
        )
        .expect("no refusal")
        .expect("the blocker is there");
        let t2 = ws.tasks_for_project("proj").into_iter().find(|t| t.id == TaskId::from("t-2"));
        let t2 = t2.expect("t-2 is live");
        assert_eq!(t2.status, TaskStatus::Pending, "a canceled blocker still clears the wait");
        assert_eq!(t2.waiting_on, None);
        ws.claim_task("proj", &worker, Some(&TaskId::from("t-2")), None)
            .expect("and the row is claimable again");
    }

    #[test]
    fn a_verify_user_row_cannot_be_completed_by_any_caller() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-verify");
        ws.push_task(Task {
            verify: Some(forge_primitives::tasks::Verify::User),
            ..sample_task("t-1", "proj")
        });
        let lead = SessionSlot::lead("TestOrg", "proj");

        let moved = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(lead), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(moved.status, TaskStatus::Waiting, "the gate lands it in waiting");
        assert_eq!(
            moved.waiting_on,
            Some(Waiting {
                kind: Some(WaitingKind::Decision),
                detail: None,
                on: None,
                verification: true,
            }),
            "the wait is the verify gate, which the board draws as approve / send back",
        );
    }

    #[test]
    fn completing_a_root_archives_the_tree_and_a_child_completion_does_not() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-archive");
        install_db(&ws, dir.path());
        ws.push_task(sample_task_with_parent("epic", None, "proj"));
        ws.push_task(Task {
            status: TaskStatus::Completed,
            ..sample_task_with_parent("child", Some("epic"), "proj")
        });
        let lead = SessionSlot::lead("TestOrg", "proj");

        // A child completing on its own archives nothing.
        ws.update_task("proj", &TaskId::from("child"), By::Seat(lead.clone()), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal");
        assert_eq!(ws.tasks_for_project("proj").len(), 2, "a completed child stays live");

        // Completing the root closes it: the tree leaves the live set and
        // lands in the archive with its stamp.
        ws.update_task("proj", &TaskId::from("epic"), By::Seat(lead), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal");
        assert!(ws.tasks_for_project("proj").is_empty(), "the closed tree is off the live board");
        let archived = archived_of(&ws, "proj");
        assert_eq!(archived.len(), 2, "the epic and its child went to the archive: {archived:?}");
        assert!(archived.iter().all(|t| t.archived_at.is_some()), "each row carries its stamp");
    }

    #[test]
    fn completing_a_root_with_open_children_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-open-children");
        install_db(&ws, dir.path());
        ws.push_task(sample_task_with_parent("epic", None, "proj"));
        ws.push_task(sample_task_with_parent("child", Some("epic"), "proj"));
        let lead = SessionSlot::lead("TestOrg", "proj");

        assert_eq!(
            ws.update_task("proj", &TaskId::from("epic"), By::Seat(lead), |task| {
                task.status = TaskStatus::Completed;
            }),
            Err(MoveError::OpenChildren(1)),
            "cancel the child first to abandon the epic",
        );
        assert_eq!(ws.tasks_for_project("proj").len(), 2, "nothing moved");
        assert!(archived_of(&ws, "proj").is_empty(), "nothing archived");
    }

    #[test]
    fn every_move_lands_in_the_history_with_its_by() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-history");
        install_db(&ws, dir.path());
        ws.push_task(sample_task("t-1", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.claim_task("proj", &worker, Some(&TaskId::from("t-1")), None).expect("claim");
        ws.update_task("proj", &TaskId::from("t-1"), By::Seat(worker.clone()), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal");

        let history = history_of(&ws, "proj");
        assert_eq!(history.len(), 3, "creation, claim and completion: {history:?}");
        assert_eq!(
            (history[1].from, history[1].to, history[1].by.clone()),
            (Some(TaskStatus::Pending), TaskStatus::InProgress, By::Seat(worker.clone())),
        );
        assert_eq!(
            (history[2].from, history[2].to, history[2].by.clone()),
            (Some(TaskStatus::InProgress), TaskStatus::Completed, By::Seat(worker)),
        );
    }
}
