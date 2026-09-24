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

use forge_primitives::tasks::{Task, TaskId, TaskStatus};

use crate::SessionSlot;
use crate::mcp::caller_context::caller_context;
use crate::workspace::Workspace;

/// Why a `tasks__*` call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TasksError {
    /// The caller couldn't be mapped to a project (transient race, or the
    /// session ended). Shouldn't happen for a live session.
    UnknownCallerProject,
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
    pub artifact: Option<String>,
    #[serde(default)]
    pub estimate: Option<String>,
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
    pub artifact: Option<String>,
    #[serde(default)]
    pub estimate: Option<String>,
}

impl TaskPatch {
    /// Apply the stated fields to `task`. `org` and `project` name the
    /// caller's project, which is where an owner label is resolved.
    fn apply(&self, task: &mut Task, org: &str, project: &str) {
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
        if let Some(artifact) = &self.artifact {
            task.artifact = Some(artifact.clone());
        }
        if let Some(estimate) = &self.estimate {
            task.estimate = Some(estimate.clone());
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

    /// Apply `patch` to the task `id` in the caller's project. `Ok(true)`
    /// if it was found, `Ok(false)` if no such task is there. Open to
    /// every session in the project.
    fn update_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        patch: TaskPatch,
    ) -> Result<bool, TasksError>;

    /// Remove the task `id` and its descendants within the caller's
    /// project. `Ok(true)` if anything was removed, `Ok(false)` if no
    /// such task is there.
    fn delete_task(&self, caller: &SessionSlot, id: &TaskId) -> Result<bool, TasksError>;
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
            artifact: draft.artifact,
            estimate: draft.estimate,
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
    ) -> Result<bool, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        Ok(ws.update_task(&cx.project_name, id, |task| {
            patch.apply(task, &cx.project_org, &cx.project_name);
        }))
    }

    fn delete_task(&self, caller: &SessionSlot, id: &TaskId) -> Result<bool, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        Ok(ws.remove_task_tree(&cx.project_name, id))
    }
}

/// One recorded `create_task` call: caller, draft, the record returned.
#[cfg(test)]
type CreateCall = (SessionSlot, TaskDraft, Task);

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool correctly parses args, resolves the caller, and surfaces
/// facade results/errors - without a real workspace.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockTasksFacade {
    pub created: parking_lot::Mutex<Vec<CreateCall>>,
    pub tasks: parking_lot::Mutex<Vec<Task>>,
    pub updated: parking_lot::Mutex<Vec<(SessionSlot, TaskId, TaskPatch)>>,
    pub update_result: parking_lot::Mutex<Option<bool>>,
    pub deleted: parking_lot::Mutex<Vec<(SessionSlot, TaskId)>>,
    pub delete_result: parking_lot::Mutex<Option<bool>>,
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
            artifact: draft.artifact.clone(),
            estimate: draft.estimate.clone(),
            created_at: now,
            updated_at: now,
        };
        self.created.lock().push((caller.clone(), draft, task.clone()));
        Ok(task)
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
    ) -> Result<bool, TasksError> {
        self.updated.lock().push((caller.clone(), id.clone(), patch));
        Ok(self.update_result.lock().unwrap_or(false))
    }

    fn delete_task(&self, caller: &SessionSlot, id: &TaskId) -> Result<bool, TasksError> {
        self.deleted.lock().push((caller.clone(), id.clone()));
        Ok(self.delete_result.lock().unwrap_or(false))
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

    fn draft(subject: &str, owner: Option<&str>, parent: Option<&str>) -> TaskDraft {
        TaskDraft {
            subject: subject.to_owned(),
            active_form: None,
            detail: None,
            status: None,
            owner: owner.map(str::to_owned),
            parent: parent.map(str::to_owned),
            artifact: None,
            estimate: None,
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

    #[test]
    fn update_moves_a_task_owned_by_another_session() {
        let (ws, facade, lead, worker) = fixture();
        let task = facade.create_task(&worker, draft("review this", None, None)).expect("create");
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
                .expect("update"),
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
