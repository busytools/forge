//! `TasksFacade` - the seam between the task MCP tools and workspace
//! state. The production impl ([`ProdTasksFacade`]) resolves the caller's
//! project and drives the direct `Workspace` task methods; the mock
//! records calls for tool tests.
//!
//! Task-list mutations are direct `Workspace` methods (lock the `tasks`
//! mutex + persist), NOT Command-bus dispatches - a task is workspace
//! state, like the cron list beside it, not a session action.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_primitives::tasks::{
    By, Estimate, LinkKind, Task, TaskId, TaskLink, TaskStatus, WaitingKind,
};

use crate::SessionSlot;
use crate::mcp::caller_context::caller_context;
use crate::workspace::Workspace;

/// Why a `tasks__*` call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TasksError {
    /// The caller couldn't be mapped to a project (transient race, or the
    /// session ended). Shouldn't happen for a live session.
    UnknownCallerProject,
    /// The estimate words do not parse as a duration; named rather than
    /// stored so a caller hears it instead of losing the value.
    BadEstimate(String),
    /// The core refused the move (open children on a close).
    Refused(String),
}

/// One link as a caller states it: the kind is derived from the target's
/// shape when it is not given.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct TaskLinkDraft {
    #[serde(default)]
    pub kind: Option<LinkKind>,
    #[serde(default)]
    pub label: Option<String>,
    pub target: String,
}

impl TaskLinkDraft {
    fn into_link(&self, at: SystemTime) -> TaskLink {
        TaskLink {
            kind: self.kind.unwrap_or_else(|| LinkKind::for_target(&self.target)),
            label: self.label.clone(),
            target: self.target.clone(),
            state: None,
            added_at: at,
        }
    }
}

/// A task as `tasks__create` states it: the fields the caller supplies,
/// before the project, the id and the timestamps are stamped on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct TaskDraft {
    pub subject: String,
    #[serde(default)]
    pub active_form: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// The label of the session holding it, in the caller's project.
    /// Absent is `unclaimed`.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub estimate: Option<String>,
    /// Queue order; lower reads first.
    #[serde(default)]
    pub rank: Option<i64>,
    /// Whether completion waits on the user; the epic's default applies
    /// when unset.
    #[serde(default)]
    pub verify: Option<forge_primitives::tasks::Verify>,
    #[serde(default)]
    pub links: Vec<TaskLinkDraft>,
}

/// The tree a `tasks__delete` removed: the named task as it stood, and how
/// many descendants went with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemovedTaskTree {
    pub task: Task,
    pub descendants_removed: usize,
}

/// The fields `tasks__update` may change. An absent field is left alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct TaskPatch {
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub active_form: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub estimate: Option<String>,
    #[serde(default)]
    pub rank: Option<i64>,
    #[serde(default)]
    pub verify: Option<forge_primitives::tasks::Verify>,
    /// Links to attach; a target already present on the row is left as it
    /// is, so the same PR linked twice stays one link.
    #[serde(default)]
    pub links_add: Vec<TaskLinkDraft>,
    /// Links to take off, by exact target.
    #[serde(default)]
    pub links_remove: Vec<String>,
}

impl TaskPatch {
    /// Apply the stated fields to `task`. `org` and `project` name the
    /// caller's project, which is where an owner label is resolved;
    /// `estimate` arrives pre-parsed so an unparseable one is refused
    /// before any field moves.
    fn apply(
        &self,
        task: &mut Task,
        org: &str,
        project: &str,
        estimate: Option<Estimate>,
        at: SystemTime,
    ) {
        if let Some(subject) = &self.subject {
            task.subject.clone_from(subject);
        }
        if let Some(active_form) = &self.active_form {
            task.active_form = Some(active_form.clone());
        }
        if let Some(detail) = &self.detail {
            task.detail = Some(detail.clone());
        }
        if let Some(status) = self.status {
            task.status = status;
        }
        if let Some(owner) = &self.owner {
            task.owner = Some(SessionSlot::new(org, project, owner.clone()));
        }
        if let Some(parent) = &self.parent {
            task.parent = Some(TaskId::from(parent.as_str()));
        }
        if let Some(rank) = self.rank {
            task.rank = Some(rank);
        }
        if let Some(verify) = self.verify {
            task.verify = Some(verify);
        }
        for draft in &self.links_add {
            if !task.links.iter().any(|l| l.target == draft.target) {
                task.links.push(draft.into_link(at));
            }
        }
        task.links.retain(|l| !self.links_remove.contains(&l.target));
        if let Some(estimate) = estimate {
            task.estimate = Some(estimate);
        }
    }
}

/// The task tools' view of the workspace. Sync - task-list mutations are
/// direct state writes with no async handler to await.
pub(crate) trait TasksFacade: Send + Sync {
    /// Register a task for the caller's project and return the new
    /// record, with its id and timestamps stamped.
    fn create_task(&self, caller: &SessionSlot, draft: TaskDraft) -> Result<Task, TasksError>;

    /// The caller's project's tasks, narrowed by `owner` (a label) and
    /// `parent` (a task id) when supplied.
    fn list_tasks(
        &self,
        caller: &SessionSlot,
        owner: Option<&str>,
        parent: Option<&TaskId>,
    ) -> Vec<Task>;

    /// Apply `patch` to the task `id` in the caller's project and return
    /// the record as the write left it. `Ok(None)` if no such task is
    /// there. Open to every session in the project.
    fn update_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        patch: TaskPatch,
    ) -> Result<Option<Task>, TasksError>;

    /// Remove the task `id` and its descendants within the caller's
    /// project, returning the named task as it stood and how many
    /// descendants went with it. `Ok(None)` if no such task is there.
    fn delete_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
    ) -> Result<Option<RemovedTaskTree>, TasksError>;

    /// Claim `id`, or the first ready row of `epic` by rank, for the
    /// caller. Backs `tasks__claim`.
    fn claim_task(
        &self,
        caller: &SessionSlot,
        id: Option<&str>,
        epic: Option<&str>,
    ) -> Result<Task, TasksError>;

    /// Put the caller's row `id` into waiting. Backs `tasks__wait`.
    fn wait_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
    ) -> Result<Task, TasksError>;
}

/// Production facade over `Weak<Workspace>` (weak to avoid a cycle with
/// the MCP server the workspace owns).
pub(crate) struct ProdTasksFacade {
    workspace: Weak<Workspace>,
}

impl ProdTasksFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn TasksFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

impl TasksFacade for ProdTasksFacade {
    fn create_task(&self, caller: &SessionSlot, draft: TaskDraft) -> Result<Task, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        let now = SystemTime::now();
        let estimate = match draft.estimate.as_deref() {
            Some(words) => Some(
                Estimate::parse(words).ok_or_else(|| TasksError::BadEstimate(words.to_owned()))?,
            ),
            None => None,
        };
        let links = draft.links.iter().map(|draft| draft.into_link(now)).collect();
        let task = Task {
            id: TaskId::from(uuid::Uuid::new_v4().to_string()),
            project_name: cx.project_name.clone(),
            subject: draft.subject,
            active_form: draft.active_form,
            detail: draft.detail,
            status: draft.status.unwrap_or(TaskStatus::Pending),
            owner: draft
                .owner
                .map(|label| SessionSlot::new(&cx.project_org, &cx.project_name, label)),
            parent: draft.parent.as_deref().map(TaskId::from),
            waiting_on: None,
            estimate,
            rank: draft.rank,
            verify: draft.verify,
            links,
            attempt: 0,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        ws.push_task(task.clone());
        Ok(task)
    }

    fn list_tasks(
        &self,
        caller: &SessionSlot,
        owner: Option<&str>,
        parent: Option<&TaskId>,
    ) -> Vec<Task> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let Some(cx) = caller_context(&ws, caller) else { return Vec::new() };
        ws.tasks_for_project(&cx.project_name)
            .into_iter()
            .filter(|t| {
                owner.is_none_or(|label| t.owner.as_ref().is_some_and(|o| o.label() == label))
            })
            .filter(|t| parent.is_none_or(|id| t.parent.as_ref() == Some(id)))
            .collect()
    }

    fn update_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        patch: TaskPatch,
    ) -> Result<Option<Task>, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        let estimate = match patch.estimate.as_deref() {
            Some(words) => Some(
                Estimate::parse(words).ok_or_else(|| TasksError::BadEstimate(words.to_owned()))?,
            ),
            None => None,
        };
        let at = SystemTime::now();
        ws.update_task(&cx.project_name, id, By::Seat(caller.clone()), |task| {
            patch.apply(task, &cx.project_org, &cx.project_name, estimate, at);
        })
        .map_err(|refused| TasksError::Refused(format!("{refused:?}")))
    }

    fn delete_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
    ) -> Result<Option<RemovedTaskTree>, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        // The named task itself has to be there. The cascade also collects
        // orphans pointing at the id, so "something was removed" would be
        // true for an id that never existed.
        if !ws.tasks_for_project(&cx.project_name).iter().any(|t| t.id == *id) {
            return Ok(None);
        }
        let removed = ws.remove_task_tree(&cx.project_name, id);
        let Some(task) = removed.iter().find(|t| t.id == *id).cloned() else {
            return Ok(None);
        };
        Ok(Some(RemovedTaskTree { task, descendants_removed: removed.len() - 1 }))
    }

    fn claim_task(
        &self,
        caller: &SessionSlot,
        id: Option<&str>,
        epic: Option<&str>,
    ) -> Result<Task, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        let by_id = id.map(TaskId::from);
        let in_epic = epic.map(TaskId::from);
        ws.claim_task(&cx.project_name, caller, by_id.as_ref(), in_epic.as_ref())
            .map_err(|refused| TasksError::Refused(refused.to_string()))
    }

    fn wait_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
    ) -> Result<Task, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        ws.wait_task(&cx.project_name, id, caller, kind, detail, on)
            .map_err(|refused| TasksError::Refused(refused.to_string()))
    }
}

/// One recorded `create_task` call: caller, draft, the record returned.
#[cfg(test)]
type CreateCall = (SessionSlot, TaskDraft, Task);

/// A canned record for the mock's default result.
#[cfg(test)]
fn mock_task(caller: &SessionSlot) -> Task {
    let now = SystemTime::UNIX_EPOCH;
    Task {
        id: TaskId::from("mock-task-id"),
        project_name: caller.project().to_owned(),
        subject: "mock".to_owned(),
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
        created_at: now,
        updated_at: now,
    }
}

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool correctly parses args, resolves the caller, and surfaces
/// facade results/errors - without a real workspace.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockTasksFacade {
    pub created: parking_lot::Mutex<Vec<CreateCall>>,
    pub tasks: parking_lot::Mutex<Vec<Task>>,
    pub updated: parking_lot::Mutex<Vec<(SessionSlot, TaskId, TaskPatch)>>,
    pub update_result: parking_lot::Mutex<Option<Task>>,
    pub deleted: parking_lot::Mutex<Vec<(SessionSlot, TaskId)>>,
    pub delete_result: parking_lot::Mutex<Option<RemovedTaskTree>>,
    pub claimed: parking_lot::Mutex<Vec<(SessionSlot, Option<String>, Option<String>)>>,
    pub claim_result: parking_lot::Mutex<Option<Result<Task, TasksError>>>,
    pub waited:
        parking_lot::Mutex<Vec<(SessionSlot, TaskId, WaitingKind, Option<String>, Option<TaskId>)>>,
    pub wait_result: parking_lot::Mutex<Option<Result<Task, TasksError>>>,
}

#[cfg(test)]
impl MockTasksFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn into_arc(self) -> Arc<dyn TasksFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
impl TasksFacade for MockTasksFacade {
    fn create_task(&self, caller: &SessionSlot, draft: TaskDraft) -> Result<Task, TasksError> {
        let now = SystemTime::UNIX_EPOCH;
        let task = Task {
            id: TaskId::from("mock-task-id"),
            project_name: caller.project().to_owned(),
            subject: draft.subject.clone(),
            active_form: draft.active_form.clone(),
            detail: draft.detail.clone(),
            status: draft.status.unwrap_or(TaskStatus::Pending),
            owner: draft
                .owner
                .clone()
                .map(|label| SessionSlot::new(caller.org(), caller.project(), label)),
            parent: draft.parent.as_deref().map(TaskId::from),
            waiting_on: None,
            estimate: draft.estimate.as_deref().and_then(Estimate::parse),
            rank: draft.rank,
            verify: draft.verify,
            links: draft.links.iter().map(|draft| draft.into_link(now)).collect(),
            attempt: 0,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        self.created.lock().push((caller.clone(), draft, task.clone()));
        Ok(task)
    }

    fn claim_task(
        &self,
        caller: &SessionSlot,
        id: Option<&str>,
        epic: Option<&str>,
    ) -> Result<Task, TasksError> {
        self.claimed.lock().push((caller.clone(), id.map(str::to_owned), epic.map(str::to_owned)));
        self.claim_result.lock().clone().unwrap_or_else(|| Ok(mock_task(caller)))
    }

    fn wait_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
    ) -> Result<Task, TasksError> {
        self.waited.lock().push((caller.clone(), id.clone(), kind, detail, on.cloned()));
        self.wait_result.lock().clone().unwrap_or_else(|| Ok(mock_task(caller)))
    }

    fn list_tasks(
        &self,
        _caller: &SessionSlot,
        _owner: Option<&str>,
        _parent: Option<&TaskId>,
    ) -> Vec<Task> {
        self.tasks.lock().clone()
    }

    fn update_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        patch: TaskPatch,
    ) -> Result<Option<Task>, TasksError> {
        self.updated.lock().push((caller.clone(), id.clone(), patch));
        Ok(self.update_result.lock().clone())
    }

    fn delete_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
    ) -> Result<Option<RemovedTaskTree>, TasksError> {
        self.deleted.lock().push((caller.clone(), id.clone()));
        Ok(self.delete_result.lock().clone())
    }
}

/// Owner scoping and list filtering over a real `Workspace` (the mock
/// above can't exercise `caller_context`).
#[cfg(test)]
mod prod_facade_tests {
    use super::*;
    use crate::WorkerEntry;
    use forge_primitives::WorkerLiveness;

    fn worker_entry(project: &str, label: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: "review".to_owned(),
            slot: SessionSlot::worker("TestOrg", project, label),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", project),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    fn seeded_task(id: &str, subject: &str) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: "myproj".to_owned(),
            subject: subject.to_owned(),
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
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn draft(subject: &str, owner: Option<&str>, parent: Option<&str>) -> TaskDraft {
        TaskDraft {
            subject: subject.to_owned(),
            active_form: None,
            detail: None,
            status: None,
            owner: owner.map(str::to_owned),
            parent: parent.map(str::to_owned),
            estimate: None,
            rank: None,
            verify: None,
            links: Vec::new(),
        }
    }

    fn fixture() -> (Arc<Workspace>, Arc<dyn TasksFacade>, SessionSlot, SessionSlot) {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", "/tmp/b2-myproj");
        let key =
            ws.list_projects().into_iter().find(|v| v.name == "myproj").expect("seeded view").key;
        ws.record_connected_session("/tmp/b2-myproj", "lead-uuid", None);
        ws.insert_live_worker(&key, worker_entry("myproj", "reviewer"));
        let facade = ProdTasksFacade::from_arc(&ws);
        (
            ws,
            facade,
            SessionSlot::lead("TestOrg", "myproj"),
            SessionSlot::worker("TestOrg", "myproj", "reviewer"),
        )
    }

    #[test]
    fn a_draft_is_stamped_with_the_callers_project_and_id() {
        let (_ws, facade, lead, _worker) = fixture();
        let task = facade
            .create_task(&lead, draft("Merge peers and workers", None, None))
            .expect("create");
        assert_eq!(task.project_name, "myproj", "stamped with the caller's project");
        assert!(!task.id.as_str().is_empty(), "the facade mints the id");
        assert_eq!(task.status, TaskStatus::Pending, "an unstated status is pending");
        assert_eq!(task.owner, None, "an unstated owner is unclaimed");
        assert_eq!(task.created_at, task.updated_at, "a fresh task's stamps agree");
    }

    #[test]
    fn list_narrows_by_owner_and_parent_within_the_project() {
        let (_ws, facade, lead, worker) = fixture();
        let epic = facade.create_task(&lead, draft("epic", None, None)).expect("epic");
        facade
            .create_task(&lead, draft("mine", Some("reviewer"), Some(epic.id.as_str())))
            .expect("child");
        facade.create_task(&lead, draft("unclaimed", None, None)).expect("sibling");

        assert_eq!(
            facade.list_tasks(&lead, None, None).len(),
            3,
            "unfiltered is the whole project"
        );
        let by_owner = facade.list_tasks(&lead, Some("reviewer"), None);
        assert_eq!(by_owner.len(), 1, "owner narrows to that label");
        assert_eq!(by_owner[0].subject, "mine");
        let by_parent = facade.list_tasks(&lead, None, Some(&epic.id));
        assert_eq!(by_parent.len(), 1, "parent narrows to that task's children");
        assert_eq!(by_parent[0].subject, "mine");
        assert_eq!(
            facade.list_tasks(&worker, Some("reviewer"), None).len(),
            1,
            "a worker's list is the same project's set",
        );
    }

    /// A task whose parent is gone leaves an orphan pointing at an id that
    /// is not in the store. Deleting that id must report nothing removed,
    /// not succeed on the orphan's back - and must not collect the orphan
    /// on the way, which would destroy work under a name nothing owns.
    #[test]
    fn delete_reports_nothing_removed_for_an_id_that_was_never_there() {
        let (_ws, facade, lead, _worker) = fixture();
        facade
            .create_task(&lead, draft("orphan", None, Some("ghost")))
            .expect("a child pointing at a parent that never existed");
        assert!(
            facade.delete_task(&lead, &TaskId::from("ghost")).expect("delete").is_none(),
            "an id that was never there is not a successful delete, even with orphans pointing \
             at it",
        );
        assert_eq!(
            facade.list_tasks(&lead, None, None).len(),
            1,
            "the orphan is not collected under a name nothing owns",
        );
    }

    /// The update hands back the record it wrote, so the tool can echo a
    /// task rather than an id: the moved field, the row's own words, and
    /// the stamp the write just set.
    ///
    /// **The seeded row carries an epoch `updated_at`**, so a record
    /// echoed without the write's own stamp reads as a pass on a fresh
    /// task and fails here.
    #[test]
    fn update_returns_the_record_it_wrote() {
        let (ws, facade, lead, _worker) = fixture();
        // A child row: completing a ROOT would close and archive it, which
        // is a different behaviour with its own test.
        let mut seeded = seeded_task("t-1", "before");
        seeded.parent = Some(TaskId::from("epic"));
        ws.seed_test_task(seeded);

        let updated = facade
            .update_task(
                &lead,
                &TaskId::from("t-1"),
                TaskPatch { status: Some(TaskStatus::Completed), ..TaskPatch::default() },
            )
            .expect("update")
            .expect("the task is there to move");

        assert_eq!(updated.id, TaskId::from("t-1"), "the echoed record is the task that moved");
        assert_eq!(updated.subject, "before", "carrying the row's own words");
        assert_eq!(updated.status, TaskStatus::Completed, "and the state it now holds");
        assert!(
            updated.updated_at > SystemTime::UNIX_EPOCH,
            "the write stamps `updated_at`; a record echoed without it keeps the epoch: {:?}",
            updated.updated_at,
        );
        assert_eq!(
            ws.tasks_for_project("myproj")[0].status,
            TaskStatus::Completed,
            "and it is the stored record, not a copy the write left behind",
        );
    }

    /// The delete hands back the tree it removed: the named task as it
    /// stood and how many descendants went with it, so the tool can echo
    /// both rather than a bare id.
    ///
    /// **The store is insertion-ordered and the named task is created
    /// last of its tree on purpose.** `c` (the delete target) is created
    /// after `b`, and `b` is then re-parented under `c`, so the records
    /// removed are `[b, c, d]` - an implementation that answers with "the
    /// first thing removed" instead of the id the call named reads `b`
    /// here, which is reachable through the tool surface alone.
    #[test]
    fn delete_returns_the_tree_it_removed() {
        let (_ws, facade, lead, _worker) = fixture();
        facade.create_task(&lead, draft("sibling", None, None)).expect("sibling");
        let b = facade.create_task(&lead, draft("b", None, None)).expect("b");
        let c = facade.create_task(&lead, draft("c", None, None)).expect("c");
        facade.create_task(&lead, draft("d", None, Some(b.id.as_str()))).expect("d, b's child");
        facade
            .update_task(
                &lead,
                &b.id,
                TaskPatch { parent: Some(c.id.as_str().to_owned()), ..TaskPatch::default() },
            )
            .expect("re-parent b under c")
            .expect("b is there to move");

        let removed = facade
            .delete_task(&lead, &c.id)
            .expect("delete")
            .expect("the task the call named is there to remove");

        assert_eq!(
            removed.task.id, c.id,
            "the record is the task named by the call, not the first record the cascade removed",
        );
        assert_eq!(removed.task.subject, "c", "as it stood just before removal");
        assert_eq!(removed.descendants_removed, 2, "b and d went with it, and the count says so");
        assert_eq!(facade.list_tasks(&lead, None, None).len(), 1, "only the sibling survives");
    }

    /// An estimate that is not a duration is refused with its own words,
    /// and nothing else in the patch moves: the caller hears the mistake
    /// instead of the value being dropped on the floor.
    #[test]
    fn an_unparseable_estimate_is_refused_and_moves_nothing() {
        let (ws, facade, lead, _worker) = fixture();
        let task = facade.create_task(&lead, draft("before", None, None)).expect("create");
        let refused = facade.update_task(
            &lead,
            &task.id,
            TaskPatch {
                subject: Some("after".to_owned()),
                estimate: Some("soonish".to_owned()),
                ..TaskPatch::default()
            },
        );
        assert_eq!(
            refused,
            Err(TasksError::BadEstimate("soonish".to_owned())),
            "the refusal names the words it could not read",
        );
        assert_eq!(
            ws.tasks_for_project("myproj")[0].subject,
            "before",
            "and no field moved on the refused write",
        );
    }

    #[test]
    fn create_refuses_an_estimate_it_cannot_parse() {
        let (_ws, facade, lead, _worker) = fixture();
        assert_eq!(
            facade.create_task(
                &lead,
                TaskDraft { estimate: Some("tomorrow".to_owned()), ..draft("x", None, None) },
            ),
            Err(TasksError::BadEstimate("tomorrow".to_owned())),
        );
    }

    /// Every field `tasks__update` can state moves, not just the two the
    /// other tests happen to use.
    #[test]
    fn a_patch_moves_every_field_it_can_state() {
        let (ws, facade, lead, _worker) = fixture();
        let task = facade.create_task(&lead, draft("before", None, None)).expect("create");
        assert!(
            facade
                .update_task(
                    &lead,
                    &task.id,
                    TaskPatch {
                        subject: Some("after".to_owned()),
                        active_form: Some("doing".to_owned()),
                        detail: Some("why".to_owned()),
                        status: Some(TaskStatus::Waiting),
                        owner: Some("lead".to_owned()),
                        parent: Some("epic".to_owned()),
                        links_add: vec![TaskLinkDraft {
                            kind: None,
                            label: None,
                            target: "PR #9".to_owned(),
                        }],
                        estimate: Some("2d".to_owned()),
                        rank: Some(3),
                        verify: Some(forge_primitives::tasks::Verify::User),
                        ..TaskPatch::default()
                    },
                )
                .expect("update")
                .is_some(),
            "the task is there to move",
        );
        let stored = &ws.tasks_for_project("myproj")[0];
        assert_eq!(stored.subject, "after");
        assert_eq!(stored.active_form.as_deref(), Some("doing"));
        assert_eq!(stored.detail.as_deref(), Some("why"));
        assert_eq!(stored.status, TaskStatus::Waiting);
        assert_eq!(stored.owner.as_ref().map(SessionSlot::label), Some("lead"));
        assert_eq!(stored.parent.as_ref().map(TaskId::as_str), Some("epic"));
        assert_eq!(
            stored.links.first().map(|l| l.target.as_str()),
            Some("PR #9"),
            "the stated artifact lands as a link",
        );
        assert_eq!(
            stored.estimate.as_ref().map(|e| e.words.as_str()),
            Some("2d"),
            "and the estimate keeps its words with its seconds parsed",
        );
        assert_eq!(stored.estimate.as_ref().map(|e| e.secs), Some(172_800));
        assert_eq!(stored.rank, Some(3), "rank moves");
        assert_eq!(stored.verify, Some(forge_primitives::tasks::Verify::User), "verify moves",);

        // The same target twice stays one link, and remove takes it off.
        facade
            .update_task(
                &lead,
                &task.id,
                TaskPatch {
                    links_add: vec![TaskLinkDraft {
                        kind: None,
                        label: None,
                        target: "PR #9".to_owned(),
                    }],
                    ..TaskPatch::default()
                },
            )
            .expect("update")
            .expect("the task is there");
        assert_eq!(
            ws.tasks_for_project("myproj")[0].links.len(),
            1,
            "a re-added target stays one link",
        );
        facade
            .update_task(
                &lead,
                &task.id,
                TaskPatch { links_remove: vec!["PR #9".to_owned()], ..TaskPatch::default() },
            )
            .expect("update")
            .expect("the task is there");
        assert!(
            ws.tasks_for_project("myproj")[0].links.is_empty(),
            "remove takes it off by exact target",
        );
    }

    #[test]
    fn update_moves_a_task_owned_by_another_session() {
        let (ws, facade, lead, worker) = fixture();
        // A child row: a completing root closes and archives (own test).
        let task =
            facade.create_task(&worker, draft("review this", None, Some("epic"))).expect("create");
        assert!(
            facade
                .update_task(
                    &lead,
                    &task.id,
                    TaskPatch {
                        status: Some(TaskStatus::Completed),
                        owner: Some("lead".to_owned()),
                        ..TaskPatch::default()
                    }
                )
                .expect("update")
                .is_some(),
            "the lead completes a worker's task",
        );
        let stored = ws.tasks_for_project("myproj");
        assert_eq!(stored[0].status, TaskStatus::Completed, "the status moved");
        assert_eq!(
            stored[0].owner.as_ref().map(SessionSlot::label),
            Some("lead"),
            "the owner moved to the lead's label",
        );
    }
}
