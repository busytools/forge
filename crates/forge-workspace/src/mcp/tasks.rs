//! Task MCP - the project's live task list (`mcp__forge__tasks__*`).
//!
//! forge owns the task list its sessions work from: a task is a declared
//! piece of work in flight, stored in the machine-local redb store (see
//! [`crate::store::tasks`]) and rendered by the Inspector's TASKS
//! section. The CLI's own task tools are gated per model and do not
//! survive a session, so forge does not use them.
//!
//! The tools (`tasks__create` / `tasks__update` / `tasks__list` /
//! `tasks__delete` / `tasks__claim` / `tasks__wait`) are ANY-CALLER,
//! scoped to the caller's own project - a worker creating its own
//! subtasks is the normal case, and any session may move any task in its
//! project. Task-list mutations are direct `Workspace` methods (state
//! writes), not Command-bus dispatches.
//!
//! - [`facade`] - the `TasksFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

use std::sync::Arc;
use std::time::SystemTime;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};

use forge_primitives::tasks::{LinkKind, Task, TaskId, TaskStatus, WaitingKind};

use crate::SessionSlot;
use crate::mcp::tasks::facade::{TaskDraft, TaskPatch, TasksError, TasksFacade};

pub(crate) mod facade;

/// Attach the task tools to an existing [`McpServerBuilder`]. Called for
/// BOTH lead and worker sessions (tasks are any-caller), so
/// `build_forge_server` invokes this unconditionally.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    let create = Create { facade: facade.clone(), slot: slot.clone() };
    let update = Update { facade: facade.clone(), slot: slot.clone() };
    let list = List { facade: facade.clone(), slot: slot.clone() };
    let delete = Delete { facade: facade.clone(), slot: slot.clone() };
    let claim = Claim { facade: facade.clone(), slot: slot.clone() };
    let wait = Wait { facade, slot };
    builder.tool(create).tool(update).tool(list).tool(delete).tool(claim).tool(wait)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput::error(text)
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
        TaskStatus::Waiting => "waiting",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Canceled => "canceled",
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
    //
    // The artifact a caller read before links existed: the first
    // pull-request or path link, by its label when it has one.
    let artifact = task
        .links
        .iter()
        .find(|l| matches!(l.kind, LinkKind::Pr | LinkKind::Path))
        .map(|l| l.label.clone().unwrap_or_else(|| l.target.clone()));
    for (key, value) in [
        ("active_form", task.active_form.as_deref()),
        ("detail", task.detail.as_deref()),
        ("artifact", artifact.as_deref()),
        ("estimate", task.estimate.as_ref().map(|e| e.words.as_str())),
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
        TasksError::BadEstimate(words) => {
            format!("\"{words}\" is not a duration; use 30m, 2h, 1d or 1w")
        }
        TasksError::Refused(why) => {
            format!("the move was refused: {why}")
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
         `parent` names another task in the same project this one belongs under, `estimate` a \
         duration, `rank` the queue order, `verify` whether completion waits on the user, \
         `links` the references it carries (issues, PRs, specs, paths), `detail` free prose. \
         `status` defaults to pending. Returns the stored task with its id (use it with \
         tasks__update / tasks__delete). Any session in the project may call this."
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
                    "enum": ["pending", "in_progress", "waiting", "completed", "failed", "canceled"],
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
                "estimate": {
                    "type": "string",
                    "description": "A duration estimate (e.g. \"1d\").",
                },
                "rank": {
                    "type": "integer",
                    "description": "Queue order; lower reads first.",
                },
                "verify": {
                    "type": "string",
                    "enum": ["user", "none"],
                    "description": "Whether completion waits on the user's look. The epic's \
                                    default applies when unset.",
                },
                "links": {
                    "type": "array",
                    "description": "References this row carries: a spec, a plan, an issue, a PR, \
                                    a branch, a path. The kind is derived from the target when \
                                    omitted.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["spec", "plan", "issue", "pr", "branch", "path", "other"],
                            },
                            "label": { "type": "string" },
                            "target": { "type": "string" },
                        },
                        "required": ["target"],
                    },
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
         is left alone. `status` is the usual one (pending / in_progress / waiting / completed / \
         failed / canceled), and `owner` takes a session label (\"lead\" for the lead) or the \
         label of a worker. Any session in the project may move any of its tasks, so a lead can \
         complete a worker's task and a worker can put a row it holds into waiting. An unknown \
         id is an error, not a no-op. Returns the task as the write left it."
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
                    "enum": ["pending", "in_progress", "waiting", "completed", "failed", "canceled"],
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
                "rank": { "type": "integer", "description": "New queue order; lower reads first." },
                "verify": {
                    "type": "string",
                    "enum": ["user", "none"],
                    "description": "Whether completion waits on the user's look.",
                },
                "links_add": {
                    "type": "array",
                    "description": "Links to attach; a target already on the row is left alone.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["spec", "plan", "issue", "pr", "branch", "path", "other"],
                            },
                            "label": { "type": "string" },
                            "target": { "type": "string" },
                        },
                        "required": ["target"],
                    },
                },
                "links_remove": {
                    "type": "array",
                    "description": "Links to take off, by exact target.",
                    "items": { "type": "string" },
                },
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
            Ok(Some(task)) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Ok(None) => tool_error(format!("no task with id {} in your project", args.id)),
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
    #[serde(default)]
    status: Option<TaskStatus>,
    #[serde(default)]
    ready: Option<bool>,
}

fn waiting_kind_str(kind: WaitingKind) -> &'static str {
    match kind {
        WaitingKind::Decision => "decision",
        WaitingKind::Dependency => "dependency",
        WaitingKind::Resource => "resource",
    }
}

/// One board row as the tool output reads it: the record, what the board
/// derived, and the wait it is sitting on.
fn board_row_to_json(row: &crate::board::BoardRow) -> serde_json::Value {
    let mut map = match task_to_json(&row.task) {
        serde_json::Value::Object(map) => map,
        other => return other,
    };
    map.insert("worked".to_owned(), serde_json::json!(crate::board::fmt_secs(row.worked_secs)));
    map.insert(
        "updated_ago".to_owned(),
        serde_json::json!(crate::board::fmt_secs(row.updated_secs_ago)),
    );
    let mut marks: Vec<&str> = Vec::new();
    if row.marks.ready {
        marks.push("ready");
    }
    if row.marks.in_review {
        marks.push("in_review");
    }
    if row.marks.overdue {
        marks.push("overdue");
    }
    if row.marks.no_movement {
        marks.push("no_movement");
    }
    if row.marks.waiting_too_long {
        marks.push("waiting_too_long");
    }
    if row.marks.stale {
        marks.push("stale");
    }
    if row.marks.to_close {
        marks.push("to_close");
    }
    map.insert("marks".to_owned(), serde_json::json!(marks));
    if let Some(wait) = &row.task.waiting_on {
        map.insert(
            "waiting".to_owned(),
            serde_json::json!({
                "kind": wait.kind.map(waiting_kind_str),
                "detail": wait.detail,
                "on": wait.on.as_ref().map(TaskId::as_str),
                "verification": wait.verification,
            }),
        );
    }
    if let Some((done, total)) = row.rollup {
        map.insert("rollup".to_owned(), serde_json::json!(format!("{done}/{total}")));
    }
    if let Some(parent_subject) = &row.parent_subject {
        map.insert("parent_subject".to_owned(), serde_json::json!(parent_subject));
    }
    serde_json::Value::Object(map)
}

#[async_trait::async_trait]
impl Tool for List {
    fn name(&self) -> &'static str {
        "tasks__list"
    }

    fn description(&self) -> &'static str {
        "List the board of YOUR project: every live row as a whole record, plus what the board \
         derives - `worked` time against the estimate, how long since any touch, and the marks \
         (ready, in_review, overdue, no_movement, waiting_too_long, stale, to_close) - under a \
         dated line, so no session does date arithmetic. Narrow with `owner` (a session label), \
         `parent` (a task id), `status`, or `ready` (true = pending with nothing waiting on it). \
         An empty array means your project has no tasks in flight, or that your project could \
         not be resolved, or that this run could not read its stored tasks. Any session in the \
         project may call this."
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
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "waiting", "completed", "failed", "canceled"],
                    "description": "Only tasks in this state.",
                },
                "ready": {
                    "type": "boolean",
                    "description": "True lists only rows something can start: pending with \
                                    nothing waiting on it.",
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
        let rows = self.facade.list_tasks(
            &self.slot,
            args.owner.as_deref(),
            parent.as_ref(),
            args.status,
            args.ready,
        );
        let arr: Vec<serde_json::Value> = rows.iter().map(board_row_to_json).collect();
        match serde_json::to_string_pretty(&serde_json::Value::Array(arr)) {
            Ok(json) => ToolOutput::text(format!("now {}\n{json}", fmt_rfc3339(SystemTime::now()))),
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
         that is over; nothing is archived. An unknown id is an error, not a no-op. Returns the \
         removed task and how many descendants went with it. Any session in the project may \
         call this."
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
            Ok(Some(removed)) => {
                let mut envelope =
                    crate::mcp::deleted::removed_record(&task_to_json(&removed.task));
                envelope["descendants_removed"] = serde_json::json!(removed.descendants_removed);
                match serde_json::to_string_pretty(&envelope) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Ok(None) => tool_error(format!("no task with id {} in your project", args.id)),
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Claim {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct ClaimArgs {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    epic: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Claim {
    fn name(&self) -> &'static str {
        "tasks__claim"
    }

    fn description(&self) -> &'static str {
        "Claim a task in YOUR project: one atomic write that makes you its owner and marks it \
         in progress, ticking the attempt. Pass `id` for a named row, or `epic` to pull the top \
         ready row of that epic by rank - exactly one of the two. Refused when a live seat \
         holds the row (named), when it is not pending, or when you already have a row in \
         progress: one in progress per worker. Returns the claimed record."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The task id to claim." },
                "epic": {
                    "type": "string",
                    "description": "The epic whose top ready row to pull, by rank.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ClaimArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match (&args.id, &args.epic) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => return tool_error("state exactly one of id or epic".to_owned()),
        }
        match self.facade.claim_task(&self.slot, args.id.as_deref(), args.epic.as_deref()) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Wait {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct WaitArgs {
    id: String,
    kind: WaitingKind,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    on: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Wait {
    fn name(&self) -> &'static str {
        "tasks__wait"
    }

    fn description(&self) -> &'static str {
        "Put one of YOUR rows into waiting and state what it waits on. `kind` is decision \
         (which routes to the user), dependency (another task: put its id in `on`), or resource \
         (an account, a CI run, a machine). `detail` is the words. The row must be yours. \
         Refused for a row you do not hold, or one that is not there. Returns the record in \
         waiting."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The task id to put into waiting." },
                "kind": {
                    "type": "string",
                    "enum": ["decision", "dependency", "resource"],
                    "description": "What it waits on; the kind decides where the unblock lands.",
                },
                "detail": {
                    "type": "string",
                    "description": "The words: what exactly is being waited for.",
                },
                "on": {
                    "type": "string",
                    "description": "For a dependency: the task id it waits on.",
                },
            },
            "required": ["id", "kind"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: WaitArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        if args.kind == WaitingKind::Dependency && args.on.is_none() {
            return tool_error("a dependency wait needs `on`, the task it waits on".to_owned());
        }
        let on = args.on.as_deref().map(TaskId::from);
        match self.facade.wait_task(
            &self.slot,
            &TaskId::from(args.id.as_str()),
            args.kind,
            args.detail,
            on.as_ref(),
        ) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tasks::facade::{MockTasksFacade, RemovedTaskTree};
    use crate::mcp::test_support::text_of;
    use forge_primitives::tasks::{Estimate, TaskLink};

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
                "rank": 10,
                "verify": "user",
                "links": [
                    { "kind": "issue", "label": "PR #1173", "target": "https://example.invalid/pull/1173" },
                    { "target": "docs/plan.md" }
                ],
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
        assert_eq!(draft.rank, Some(10));
        assert_eq!(draft.verify, Some(forge_primitives::tasks::Verify::User));
        assert_eq!(draft.links.len(), 2);
        assert_eq!(draft.links[0].kind, Some(LinkKind::Issue));
        assert_eq!(
            draft.links[1].kind, None,
            "an omitted kind is derived at the facade, not guessed here",
        );
        assert_eq!(draft.links[1].target, "docs/plan.md");
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

    /// The update result is the record it wrote, so the caller can name
    /// the task it moved and read the state it now holds - `updated task
    /// <id>` named neither.
    #[tokio::test]
    async fn update_echoes_the_updated_record() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut task = sample_task();
        task.status = TaskStatus::InProgress;
        task.owner = Some(SessionSlot::lead("TestOrg", "myproj"));
        *facade.update_result.lock() = Some(task);
        let out = Update { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "status": "in_progress" })))
            .await;
        assert!(!out.is_error, "update succeeds: {out:?}");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the task record, not prose");
        assert_eq!(json["id"], "t-1", "the record names the task it moved: {json}");
        assert_eq!(json["subject"], "Merge peers and workers");
        assert_eq!(json["status"], "in_progress", "and the state it now holds");
        assert_eq!(json["owner"], "lead");
    }

    /// A delete echoes what went, in the adopted envelope: the record as
    /// it stood, plus the cascade's own count - the row has to say how
    /// much of the tree went with it. The record is compared to
    /// `task_to_json` whole, so an echo trimmed to a couple of fields
    /// cannot read as the record.
    #[tokio::test]
    async fn delete_echoes_the_removed_record_and_the_cascade_count() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.delete_result.lock() =
            Some(RemovedTaskTree { task: sample_task(), descendants_removed: 2 });
        let out = Delete { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1" })))
            .await;
        assert!(!out.is_error, "delete succeeds: {out:?}");
        assert_eq!(out.blocks.len(), 1, "the envelope stays one text block: {out:?}");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the adopted envelope");
        assert_eq!(json["status"], "deleted");
        assert_eq!(
            json["removed"],
            task_to_json(&sample_task()),
            "the echo is the whole record, not a hand-picked subset: {json}",
        );
        assert_eq!(json["descendants_removed"], 2);
    }

    /// The count is always there: a leaf delete says zero rather than
    /// leaving the field out, so a reader never has to tell an omitted
    /// count from a cascade that took nothing.
    #[tokio::test]
    async fn a_delete_with_no_children_still_states_the_count() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.delete_result.lock() =
            Some(RemovedTaskTree { task: sample_task(), descendants_removed: 0 });
        let out = Delete { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1" })))
            .await;
        assert!(!out.is_error, "delete succeeds: {out:?}");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the adopted envelope");
        assert_eq!(json["descendants_removed"], 0, "a leaf states its count: {json}");
    }

    #[tokio::test]
    async fn update_states_only_the_fields_it_was_given() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.update_result.lock() = Some(sample_task());
        let out = Update { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({
                "id": "t-1",
                "status": "waiting",
                "links_add": [{ "target": "x" }]
            })))
            .await;
        assert!(!out.is_error, "update succeeds: {out:?}");
        let patch = &facade.updated.lock()[0].2;
        assert_eq!(patch.status, Some(TaskStatus::Waiting));
        assert_eq!(patch.links_add.len(), 1, "the stated link reaches the facade");
        assert_eq!(patch.links_add[0].target, "x");
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

    /// The spelling a caller reads in a result, the spelling it must send
    /// back, and the spelling the description names are the same six. Every
    /// one of those sites writes them out by hand rather than deriving them
    /// from the enum, so nothing else pins them to it.
    #[test]
    fn the_status_spellings_agree_across_the_output_and_both_schemas() {
        for (status, spelling) in [
            (TaskStatus::Pending, "pending"),
            (TaskStatus::InProgress, "in_progress"),
            (TaskStatus::Waiting, "waiting"),
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Canceled, "canceled"),
        ] {
            assert_eq!(status_str(status), spelling, "a result reads {status:?} as {spelling}");
        }
        for schema in [
            Create { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() }.input_schema(),
            Update { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() }.input_schema(),
        ] {
            let spelled: Vec<&str> = schema["properties"]["status"]["enum"]
                .as_array()
                .expect("the status property carries an enum")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect();
            assert_eq!(
                spelled,
                ["pending", "in_progress", "waiting", "completed", "failed", "canceled"],
                "a caller must be able to send back what it read",
            );
        }

        // The description is the site an agent reads BEFORE it writes anything,
        // and the one nothing checks: it named `blocked` for a release after
        // the enum dropped it, so a session following it took a hard
        // `invalid arguments` where the schema would have said the same thing
        // more cheaply.
        let description =
            Update { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() }.description();
        for spelling in ["pending", "in_progress", "waiting", "completed", "failed", "canceled"] {
            assert!(
                description.contains(spelling),
                "the description does not name {spelling}, which the schema accepts",
            );
        }
        assert!(
            !description.contains("blocked"),
            "the description names a status the enum no longer carries",
        );
    }

    /// Every field the list description promises is in the block the model
    /// reads, spelled the way the description spells it, carrying the value
    /// its own field holds - the optional entries are one shared tuple, so
    /// a crossed pair of sources would otherwise read as correct.
    #[test]
    fn a_task_block_carries_the_named_fields() {
        let task = Task {
            active_form: Some("Merging".to_owned()),
            detail: Some("why".to_owned()),
            estimate: Some(Estimate { words: "1d".to_owned(), secs: 86_400 }),
            links: vec![TaskLink {
                kind: LinkKind::Pr,
                label: Some("PR #9".to_owned()),
                target: "https://example.invalid/pull/9".to_owned(),
                state: None,
                added_at: SystemTime::UNIX_EPOCH,
            }],
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
        for (key, value) in [
            ("id", "t-1"),
            ("project", "myproj"),
            ("subject", "Merge peers and workers"),
            ("status", "pending"),
            ("active_form", "Merging"),
            ("detail", "why"),
            ("artifact", "PR #9"),
            ("estimate", "1d"),
            ("parent", "epic"),
            ("owner", "lead"),
        ] {
            assert_eq!(
                json.get(key).and_then(serde_json::Value::as_str),
                Some(value),
                "the block's {key} is the task's own {key}: {json}",
            );
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
    /// them. This is load-bearing rather than a prose lock: the caller is a
    /// model deciding whether to wait, to fix its project, or to give up,
    /// and an empty array that reads as "nothing in flight" when the store
    /// could not be read tells it to wait for work that will never appear.
    /// The cron group's list description carries the same clauses and the
    /// same test.
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

    fn board_row(task: Task) -> crate::board::BoardRow {
        crate::board::BoardRow {
            task,
            worked_secs: 0,
            updated_secs_ago: 0,
            marks: crate::board::Marks {
                ready: false,
                in_review: false,
                overdue: false,
                no_movement: false,
                waiting_too_long: false,
                stale: false,
                to_close: false,
            },
            rollup: None,
            parent_subject: None,
        }
    }

    /// The list opens with the date, so a session reading it never does
    /// date arithmetic against bare timestamps.
    #[tokio::test]
    async fn the_list_leads_with_a_dated_line() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        let text = text_of(&out);
        let first = text.lines().next().expect("a first line");
        assert!(first.starts_with("now "), "the list opens with the date: {first}");
        assert!(first.contains('T'), "in RFC3339: {first}");
    }

    #[tokio::test]
    async fn a_listed_row_carries_worked_time_and_its_marks() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut row = board_row(sample_task());
        row.worked_secs = 3 * 3_600;
        row.updated_secs_ago = 12 * 60;
        row.marks.in_review = true;
        row.marks.overdue = true;
        facade.rows.lock().push(row);
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        let text = text_of(&out);
        let body = text.split_once('\n').expect("a header line then the rows").1;
        let json: serde_json::Value =
            serde_json::from_str(body).expect("the body is the rows array");
        assert_eq!(json[0]["worked"], "3h", "worked time in the board's words: {json}");
        assert_eq!(json[0]["updated_ago"], "12m");
        let marks: Vec<&str> = json[0]["marks"]
            .as_array()
            .expect("marks array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(marks.contains(&"overdue"), "marks name the facts: {marks:?}");
        assert!(marks.contains(&"in_review"), "marks name the facts: {marks:?}");
    }

    #[tokio::test]
    async fn ready_narrows_to_rows_nothing_holds() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut held = board_row(sample_task());
        held.task.owner = Some(SessionSlot::lead("TestOrg", "myproj"));
        let mut free = board_row(Task { id: TaskId::from("t-2"), ..sample_task() });
        free.marks.ready = true;
        facade.rows.lock().extend([held, free]);
        let out = List { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "ready": true })))
            .await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        let text = text_of(&out);
        assert!(text.contains("t-2"), "the ready row is listed: {text}");
        assert!(!text.contains("t-1"), "the held row is not: {text}");
        assert_eq!(facade.listed.lock()[0].3, Some(true), "and the filter was threaded");
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
        let delete = Delete { facade: facade.clone(), slot: slot.clone() };
        let claim = Claim { facade: facade.clone(), slot: slot.clone() };
        let wait = Wait { facade, slot };
        assert_eq!(create.name(), "tasks__create");
        assert_eq!(update.name(), "tasks__update");
        assert_eq!(list.name(), "tasks__list");
        assert_eq!(delete.name(), "tasks__delete");
        assert_eq!(claim.name(), "tasks__claim");
        assert_eq!(wait.name(), "tasks__wait");
    }

    #[tokio::test]
    async fn claim_by_epic_threads_the_epic_and_echoes_the_row() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut task = sample_task();
        task.status = TaskStatus::InProgress;
        task.owner = Some(SessionSlot::worker("TestOrg", "myproj", "w-1"));
        *facade.claim_result.lock() = Some(Ok(task));
        let slot = SessionSlot::worker("TestOrg", "myproj", "w-1");
        let out = Claim { facade: facade.clone(), slot }
            .call(input(serde_json::json!({
                "epic": "epic-1"
            })))
            .await;
        assert!(!out.is_error, "claim succeeds: {out:?}");
        let calls = facade.claimed.lock();
        assert_eq!(calls[0].1, None, "no id carried");
        assert_eq!(calls[0].2.as_deref(), Some("epic-1"));
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the claimed record");
        assert_eq!(json["status"], "in_progress");
        assert_eq!(json["owner"], "w-1");
    }

    #[tokio::test]
    async fn claim_with_both_id_and_epic_is_refused_without_touching_the_facade() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Claim { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "epic": "epic-1" })))
            .await;
        assert!(out.is_error, "exactly one of id or epic");
        let out = Claim { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({})))
            .await;
        assert!(out.is_error, "neither is not one either");
        assert!(facade.claimed.lock().is_empty(), "invalid input never reaches the facade");
    }

    /// The core's refusal is read by the caller: the seat that holds the
    /// row is named, not hidden behind a generic failure.
    #[tokio::test]
    async fn a_refused_claim_surfaces_the_cores_words() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.claim_result.lock() =
            Some(Err(TasksError::Refused("w-1 holds this row".to_owned())));
        let out = Claim { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1" })))
            .await;
        assert!(out.is_error, "the claim is refused");
        assert!(
            text_of(&out).contains("w-1 holds this row"),
            "and the refusal names the holder: {:?}",
            text_of(&out),
        );
    }

    #[tokio::test]
    async fn wait_threads_kind_detail_and_blocker() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Wait { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({
                "id": "t-1",
                "kind": "dependency",
                "detail": "the board read has to land first",
                "on": "t-9",
            })))
            .await;
        assert!(!out.is_error, "wait succeeds: {out:?}");
        let calls = facade.waited.lock();
        assert_eq!(calls[0].2, WaitingKind::Dependency);
        assert_eq!(calls[0].3.as_deref(), Some("the board read has to land first"));
        assert_eq!(calls[0].4, Some(TaskId::from("t-9")));
    }

    #[tokio::test]
    async fn a_dependency_wait_without_on_is_refused() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Wait { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "kind": "dependency" })))
            .await;
        assert!(out.is_error, "a dependency wait names its blocker");
        assert!(facade.waited.lock().is_empty(), "invalid input never reaches the facade");
    }
}
