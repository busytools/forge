//! Task MCP - the project's live task list (`mcp__forge__tasks__*`).
//!
//! forge owns the task list its sessions work from: a task is a declared
//! piece of work in flight, stored in the machine-local redb store (see
//! [`crate::store::tasks`]) and rendered by the Inspector's TASKS
//! section. The CLI's own task tools are gated per model and do not
//! survive a session, so forge does not use them.
//!
//! The tools (`tasks__create` / `tasks__update` / `tasks__list` /
//! `tasks__delete`) are ANY-CALLER, scoped to the caller's own project -
//! a worker creating its own subtasks is the normal case, and any
//! session may move any task in its project. Task-list mutations are
//! direct `Workspace` methods (state writes), not Command-bus
//! dispatches.
//!
//! - [`facade`] - the `TasksFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

use std::sync::Arc;
use std::time::SystemTime;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};

use forge_primitives::tasks::{Task, TaskId, TaskStatus};

use crate::SessionSlot;
use crate::mcp::tasks::facade::{TaskDraft, TaskPatch, TasksError, TasksFacade};

pub(crate) mod facade;

/// Attach the four task-list tools to an existing [`McpServerBuilder`].
/// Called for BOTH lead and worker sessions (tasks are any-caller), so
/// `build_forge_server` invokes this unconditionally.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    let create = Create { facade: facade.clone(), slot: slot.clone() };
    let update = Update { facade: facade.clone(), slot: slot.clone() };
    let list = List { facade: facade.clone(), slot: slot.clone() };
    let delete = Delete { facade, slot };
    builder.tool(create).tool(update).tool(list).tool(delete)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![forge_sdk::mcp::tool::ToolOutputBlock { text }], is_error: true }
}

/// Format a `SystemTime` as a UTC RFC3339 string for tool output.
fn fmt_rfc3339(t: SystemTime) -> String {
    time::OffsetDateTime::from(t)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// The status as the schema spells it.
fn status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Completed => "completed",
    }
}

/// Readable JSON for one task (the tool-output shape the LLM sees).
/// Absent optional fields are omitted rather than sent as null.
fn task_to_json(task: &Task) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_owned(), serde_json::json!(task.id.as_str()));
    map.insert("project".to_owned(), serde_json::json!(task.project_name));
    map.insert("subject".to_owned(), serde_json::json!(task.subject));
    map.insert("status".to_owned(), serde_json::json!(status_str(task.status)));
    // An unclaimed task omits `owner` rather than writing a sentinel: a
    // caller that reads the field back and filters on it would otherwise
    // find none, and a session actually labelled `unclaimed` would read the
    // same as nobody.
    for (key, value) in [
        ("active_form", task.active_form.as_deref()),
        ("detail", task.detail.as_deref()),
        ("artifact", task.artifact.as_deref()),
        ("estimate", task.estimate.as_deref()),
        ("parent", task.parent.as_ref().map(TaskId::as_str)),
        ("owner", task.owner.as_ref().map(SessionSlot::label)),
    ] {
        if let Some(value) = value {
            map.insert(key.to_owned(), serde_json::json!(value));
        }
    }
    map.insert("created_at".to_owned(), serde_json::json!(fmt_rfc3339(task.created_at)));
    map.insert("updated_at".to_owned(), serde_json::json!(fmt_rfc3339(task.updated_at)));
    serde_json::Value::Object(map)
}

fn format_tasks_error(err: &TasksError) -> String {
    match err {
        TasksError::UnknownCallerProject => {
            "couldn't resolve your project; is this session attached to a forge.toml project?"
                .to_owned()
        }
    }
}

struct Create {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for Create {
    fn name(&self) -> &'static str {
        "tasks__create"
    }

    fn description(&self) -> &'static str {
        "Declare a piece of live work in YOUR project's task list. `subject` is the row's text \
         and the only required field; `active_form` is the in-progress wording (\"Adding tests\" \
         for \"Add tests\"), shown while the task is running. `owner` is the label of the session \
         holding it - a worker's label, or \"lead\" - and omitting it leaves the task unclaimed. \
         `parent` names another task in the same project this one belongs under, `artifact` a PR \
         or path, `estimate` a duration, `detail` free prose. `status` defaults to pending. \
         Returns the stored task with its id (use it with tasks__update / tasks__delete). Any \
         session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "description": "The task's row text.",
                },
                "active_form": {
                    "type": "string",
                    "description": "The in-progress wording, shown while the task is running.",
                },
                "detail": {
                    "type": "string",
                    "description": "Free prose: the why.",
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "blocked", "completed"],
                    "description": "Where the task starts. Defaults to pending.",
                },
                "owner": {
                    "type": "string",
                    "description": "The label of the session holding the task, in your project \
                                    (\"lead\" for the lead). Omit for unclaimed.",
                },
                "parent": {
                    "type": "string",
                    "description": "The id of the task this one belongs under, in the same \
                                    project.",
                },
                "artifact": {
                    "type": "string",
                    "description": "The PR or path this work produced.",
                },
                "estimate": {
                    "type": "string",
                    "description": "A duration estimate (e.g. \"1d\").",
                },
            },
            "required": ["subject"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let draft: TaskDraft = match serde_json::from_value(input.value) {
            Ok(draft) => draft,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.create_task(&self.slot, draft) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Update {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct UpdateArgs {
    id: String,
    #[serde(flatten)]
    patch: TaskPatch,
}

#[async_trait::async_trait]
impl Tool for Update {
    fn name(&self) -> &'static str {
        "tasks__update"
    }

    fn description(&self) -> &'static str {
        "Change a task in YOUR project by id, stating only the fields to move - everything else \
         is left alone. `status` is the usual one (pending / in_progress / blocked / completed), \
         and `owner` takes a session label (\"lead\" for the lead) or the label of a worker. Any \
         session in the project may move any of its tasks, so a lead can complete a worker's \
         task and a worker can block its own parent. An unknown id is an error, not a no-op."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The task id to change." },
                "subject": { "type": "string", "description": "New row text." },
                "active_form": {
                    "type": "string",
                    "description": "New in-progress wording.",
                },
                "detail": { "type": "string", "description": "New detail prose." },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "blocked", "completed"],
                    "description": "The task's new status.",
                },
                "owner": {
                    "type": "string",
                    "description": "The label of the session now holding the task.",
                },
                "parent": {
                    "type": "string",
                    "description": "The id of the task this one now belongs under.",
                },
                "artifact": { "type": "string", "description": "The PR or path produced." },
                "estimate": { "type": "string", "description": "A duration estimate." },
            },
            "required": ["id"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: UpdateArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.update_task(&self.slot, &TaskId::from(args.id.as_str()), args.patch) {
            Ok(true) => ToolOutput::text(format!("updated task {}", args.id)),
            Ok(false) => tool_error(format!("no task with id {} in your project", args.id)),
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct List {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct ListArgs {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    parent: Option<String>,
}

#[async_trait::async_trait]
impl Tool for List {
    fn name(&self) -> &'static str {
        "tasks__list"
    }

    fn description(&self) -> &'static str {
        "List the live tasks of YOUR project as whole records: id, subject and status always, \
         then `owner`, `parent`, `active_form`, `detail`, `artifact` and `estimate` on the tasks \
         that have them, plus the created and updated timestamps. Optionally narrow with `owner` \
         (a session label) or `parent` (a task id), so one task's detail or one session's rows \
         are a filter away. An empty array means your project has no tasks in flight, or that \
         your project could not be resolved, or that this run could not read its stored tasks. \
         Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": "Only tasks held by this session label.",
                },
                "parent": {
                    "type": "string",
                    "description": "Only the tasks under this task id.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ListArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let parent = args.parent.as_deref().map(TaskId::from);
        let tasks = self.facade.list_tasks(&self.slot, args.owner.as_deref(), parent.as_ref());
        let arr: Vec<serde_json::Value> = tasks.iter().map(task_to_json).collect();
        match serde_json::to_string_pretty(&serde_json::Value::Array(arr)) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("task-list serialization failed: {err}")),
        }
    }
}

struct Delete {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct DeleteArgs {
    id: String,
}

#[async_trait::async_trait]
impl Tool for Delete {
    fn name(&self) -> &'static str {
        "tasks__delete"
    }

    fn description(&self) -> &'static str {
        "Remove a task in YOUR project by id together with its children in the same call, so a \
         parent never leaves subtasks pointing at a task that is gone. Use this to clear work \
         that is over; nothing is archived. An unknown id is an error, not a no-op. Any session \
         in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The task id to remove." },
            },
            "required": ["id"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: DeleteArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.delete_task(&self.slot, &TaskId::from(args.id.as_str())) {
            Ok(true) => ToolOutput::text(format!("deleted task {}", args.id)),
            Ok(false) => tool_error(format!("no task with id {} in your project", args.id)),
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tasks::facade::MockTasksFacade;

    fn lead_slot() -> SessionSlot {
        SessionSlot::lead("TestOrg", "myproj")
    }

    fn input(value: serde_json::Value) -> ToolInput {
        ToolInput { value }
    }

    #[tokio::test]
    async fn create_persists_a_task_for_the_callers_project() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Create { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "subject": "Merge peers and workers into agents" })))
            .await;
        assert!(!out.is_error, "create succeeds: {out:?}");
        let calls = facade.created.lock();
        assert_eq!(calls.len(), 1, "one task created");
        assert_eq!(calls[0].1.subject, "Merge peers and workers into agents");
        assert_eq!(calls[0].2.project_name, "myproj", "stamped with the caller's project");
    }

    #[tokio::test]
    async fn create_threads_every_optional_field_to_the_facade() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Create { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({
                "subject": "Merge peers and workers",
                "active_form": "Merging peers and workers",
                "detail": "One agents family.",
                "status": "in_progress",
                "owner": "agents-merge",
                "parent": "epic",
                "artifact": "PR #1173",
                "estimate": "1d",
            })))
            .await;
        assert!(!out.is_error, "create succeeds: {out:?}");
        let calls = facade.created.lock();
        let draft = &calls[0].1;
        assert_eq!(draft.active_form.as_deref(), Some("Merging peers and workers"));
        assert_eq!(draft.detail.as_deref(), Some("One agents family."));
        assert_eq!(draft.status, Some(TaskStatus::InProgress));
        assert_eq!(draft.owner.as_deref(), Some("agents-merge"));
        assert_eq!(draft.parent.as_deref(), Some("epic"));
        assert_eq!(draft.artifact.as_deref(), Some("PR #1173"));
        assert_eq!(draft.estimate.as_deref(), Some("1d"));
    }

    #[tokio::test]
    async fn create_rejects_a_missing_subject_without_touching_the_facade() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Create { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "detail": "no subject" })))
            .await;
        assert!(out.is_error, "subject is required");
        assert!(facade.created.lock().is_empty(), "invalid input never reaches the facade");
    }

    #[tokio::test]
    async fn update_reports_a_missing_id_rather_than_succeeding_silently() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Update { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "nope", "status": "completed" })))
            .await;
        assert!(out.is_error, "an unknown id is an error, not a no-op");
    }

    #[tokio::test]
    async fn update_states_only_the_fields_it_was_given() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.update_result.lock() = Some(true);
        let out = Update { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "status": "blocked", "artifact": "x" })))
            .await;
        assert!(!out.is_error, "update succeeds: {out:?}");
        let patch = &facade.updated.lock()[0].2;
        assert_eq!(patch.status, Some(TaskStatus::Blocked));
        assert_eq!(patch.artifact.as_deref(), Some("x"));
        assert_eq!(patch.subject, None, "an unstated field is left alone");
        assert_eq!(patch.owner, None, "an unstated field is left alone");
    }

    fn sample_task() -> Task {
        Task {
            id: TaskId::from("t-1"),
            project_name: "myproj".to_owned(),
            subject: "Merge peers and workers".to_owned(),
            active_form: None,
            detail: None,
            status: TaskStatus::Pending,
            owner: None,
            parent: None,
            artifact: None,
            estimate: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    /// Every field the list description promises is in the block the model
    /// reads, spelled the way the description spells it.
    #[test]
    fn a_task_block_carries_the_named_fields() {
        let task = Task {
            active_form: Some("Merging".to_owned()),
            detail: Some("why".to_owned()),
            artifact: Some("PR #9".to_owned()),
            estimate: Some("1d".to_owned()),
            parent: Some(TaskId::from("epic")),
            owner: Some(SessionSlot::lead("TestOrg", "myproj")),
            ..sample_task()
        };
        let json = task_to_json(&task);
        for key in [
            "id",
            "project",
            "subject",
            "status",
            "active_form",
            "detail",
            "artifact",
            "estimate",
            "parent",
            "owner",
            "created_at",
            "updated_at",
        ] {
            assert!(json.get(key).is_some(), "the task block carries {key}: {json}");
        }
    }

    /// An unclaimed task carries no `owner` key at all. Writing a sentinel
    /// makes the field unreadable: a caller filtering on what it read back
    /// finds none, and a session actually labelled `unclaimed` reads the
    /// same as nobody.
    #[test]
    fn an_unclaimed_task_omits_its_owner() {
        let json = task_to_json(&sample_task());
        assert!(json.get("owner").is_none(), "an unclaimed task carries no owner key: {json}");
    }

    #[test]
    fn a_claimed_task_names_its_owner_by_label() {
        let task = Task {
            owner: Some(SessionSlot::worker("TestOrg", "myproj", "steward")),
            ..sample_task()
        };
        let json = task_to_json(&task);
        assert_eq!(json.get("owner").and_then(serde_json::Value::as_str), Some("steward"));
    }

    /// An empty list has three causes, and the description names all of
    /// them: a project with nothing in flight, a caller whose project could
    /// not be resolved, and a run that could not read the store.
    #[test]
    fn the_list_description_owns_up_to_every_empty_result() {
        let desc =
            List { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() }.description();
        for clause in [
            "An empty array means your project has no tasks in flight",
            "or that your project could not be resolved",
            "or that this run could not read its stored tasks",
        ] {
            assert!(desc.contains(clause), "the list description owes {clause:?}: {desc}");
        }
    }

    #[tokio::test]
    async fn list_returns_project_tasks() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
    }

    #[tokio::test]
    async fn delete_reports_a_missing_id_rather_than_succeeding_silently() {
        // The default mock finds nothing, so this is the not-found path.
        let facade = Arc::new(MockTasksFacade::default());
        let out = Delete { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "nope" })))
            .await;
        assert!(out.is_error, "an unknown id is an error, not a no-op");
    }

    #[test]
    fn tool_names_are_the_task_family() {
        // Assert the base tool names only. Combined with the `forge`
        // server name (proven in mcp::tests::build_forge_server_*) they
        // render as `mcp__forge__tasks__<x>` on the LLM side, which the SDK
        // auto-approve fast-path covers via the `mcp__forge__` prefix
        // (asserted in forge-sdk options.rs).
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let create = Create { facade: facade.clone(), slot: slot.clone() };
        let update = Update { facade: facade.clone(), slot: slot.clone() };
        let list = List { facade: facade.clone(), slot: slot.clone() };
        let delete = Delete { facade, slot };
        assert_eq!(create.name(), "tasks__create");
        assert_eq!(update.name(), "tasks__update");
        assert_eq!(list.name(), "tasks__list");
        assert_eq!(delete.name(), "tasks__delete");
    }
}
