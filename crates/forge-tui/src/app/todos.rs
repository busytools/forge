//! Inspector task list maintained by the `TaskCreate` / `TaskUpdate`
//! tool family (#268). CLI 2.1.156 deprecated `TodoWrite`; forge no
//! longer parses or renders TodoWrite output.

use super::{App, TodoItem, TodoStatus};

/// Input fields parsed from a `TaskCreate` tool call's `raw_input`
/// (#268). The CLI assigns the task `id` in the result text, NOT in
/// the input, so this struct doesn't carry one - callers pair it
/// with `parse_task_create_result_id` to learn the assigned id.
///
/// Fields ignored from the wire shape (per planner's V1 scope):
/// `description`, `metadata`, `owner`, `addBlocks`, `addBlockedBy`.
/// The Inspector renders only content + activeForm + status today;
/// the dropped fields have no analogue and would either grow the
/// renderer or sit unread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskCreateInput {
    pub content: String,     // wire: `subject`
    pub active_form: String, // wire: `activeForm`
}

/// Input fields parsed from a `TaskUpdate` tool call's `raw_input`
/// (#268). `task_id` is required (wire `taskId`); every other field
/// is optional (a TaskUpdate carries only the fields it mutates).
/// `status == Some(Deleted)` is the remove signal - the reducer
/// drops the matching item rather than transitioning it to a
/// "deleted" rendered state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskUpdateInput {
    pub task_id: String,
    pub status: Option<TaskStatusUpdate>,
    pub content: Option<String>,     // wire: `subject` (when present)
    pub active_form: Option<String>, // wire: `activeForm` (when present)
}

/// A `TaskUpdate.status` value. Mirrors `TodoStatus` plus the
/// `Deleted` removal signal the wire format adds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStatusUpdate {
    Pending,
    InProgress,
    Completed,
    Deleted,
}

/// Parse a `TaskCreate.raw_input` JSON value into the V1 projection.
/// Returns `None` when neither `subject` nor `activeForm` is a
/// non-empty string (a malformed or transient input where the
/// projection would be empty in both dimensions).
pub(crate) fn parse_task_create_input(raw_input: &serde_json::Value) -> Option<TaskCreateInput> {
    let content = raw_input.get("subject").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let active_form = raw_input.get("activeForm").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    if content.is_empty() && active_form.is_empty() {
        return None;
    }
    Some(TaskCreateInput { content, active_form })
}

/// Extract the assigned task id from a `TaskCreate` tool result's
/// text content. The CLI announces the id with the literal prefix
/// `"Task #<n> created successfully:"` per core-v1's wire capture;
/// any other shape (including no result text, unrelated text, or
/// a missing `#<n>` token) returns `None`.
///
/// The id is a numeric token in the CLI's emission today; we still
/// return it as a `String` because callers downstream treat the id
/// as opaque, which keeps the reducer agnostic to whether future
/// CLI versions widen the format (e.g. namespaced ids).
pub(crate) fn parse_task_create_result_id(result_text: &str) -> Option<String> {
    let trimmed = result_text.trim_start();
    let rest = trimmed.strip_prefix("Task #")?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    if end == 0 {
        return None;
    }
    let id = &rest[..end];
    let tail = &rest[end..];
    if !tail.starts_with(" created successfully:") {
        return None;
    }
    Some(id.to_owned())
}

/// Parse a `TaskUpdate.raw_input` JSON value into the V1 projection.
/// Returns `None` when `taskId` is missing or non-string - that
/// shape can't address an existing item, so there's nothing to
/// apply.
pub(crate) fn parse_task_update_input(raw_input: &serde_json::Value) -> Option<TaskUpdateInput> {
    let task_id = raw_input.get("taskId").and_then(|v| v.as_str())?.to_owned();
    let status = raw_input.get("status").and_then(|v| v.as_str()).map(parse_task_status_update);
    let content = raw_input.get("subject").and_then(|v| v.as_str()).map(str::to_owned);
    let active_form = raw_input.get("activeForm").and_then(|v| v.as_str()).map(str::to_owned);
    Some(TaskUpdateInput { task_id, status, content, active_form })
}

fn parse_task_status_update(status: &str) -> TaskStatusUpdate {
    match status {
        "in_progress" => TaskStatusUpdate::InProgress,
        "completed" => TaskStatusUpdate::Completed,
        "deleted" => TaskStatusUpdate::Deleted,
        _ => TaskStatusUpdate::Pending,
    }
}

/// Append a TaskCreate-shaped item to the active session's todo
/// list with the CLI-assigned `id`. Items start in `Pending` -
/// status transitions land via subsequent `TaskUpdate` calls. The
/// inspector immediately reflects the new row.
pub(crate) fn apply_task_create(app: &mut App, input: TaskCreateInput, id: String) {
    app.todos_mut().push(TodoItem {
        id,
        content: input.content,
        status: TodoStatus::Pending,
        active_form: input.active_form,
    });
}

/// Apply a TaskUpdate to the active session's todo list. Three
/// outcomes:
///
/// - `task_id` matches no item -> warn-log + no-op (the CLI's
///   ordering should make this impossible; logging it surfaces
///   any race between forge state and the upstream session).
/// - `status == Some(Deleted)` -> remove the matching item.
/// - Otherwise -> mutate matching item's fields in place (status,
///   content, active_form), each optional.
pub(crate) fn apply_task_update(app: &mut App, update: TaskUpdateInput) {
    let Some(idx) = app.todos().iter().position(|t| t.id == update.task_id) else {
        tracing::warn!(
            target: crate::logging::targets::APP_TOOL,
            event_name = "task_update_unknown_id",
            message = "TaskUpdate addresses an id absent from the inspector list; no-op",
            outcome = "skipped",
            task_id = %update.task_id,
        );
        return;
    };
    if matches!(update.status, Some(TaskStatusUpdate::Deleted)) {
        app.todos_mut().remove(idx);
        return;
    }
    let todos = app.todos_mut();
    if let Some(status) = update.status {
        todos[idx].status = match status {
            TaskStatusUpdate::Pending => TodoStatus::Pending,
            TaskStatusUpdate::InProgress => TodoStatus::InProgress,
            TaskStatusUpdate::Completed => TodoStatus::Completed,
            TaskStatusUpdate::Deleted => unreachable!("Deleted handled above"),
        };
    }
    if let Some(content) = update.content {
        todos[idx].content = content;
    }
    if let Some(active_form) = update.active_form {
        todos[idx].active_form = active_form;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use serde_json::json;

    #[test]
    fn parse_task_create_input_reads_subject_and_active_form() {
        let input = json!({
            "subject": "Read the file",
            "description": "ignored in V1",
            "activeForm": "Reading the file",
            "metadata": {"k": "v"}
        });
        let parsed = parse_task_create_input(&input).expect("subject + activeForm present");
        assert_eq!(parsed.content, "Read the file");
        assert_eq!(parsed.active_form, "Reading the file");
    }

    #[test]
    fn parse_task_create_input_returns_none_when_both_fields_missing() {
        let input = json!({"description": "no subject, no activeForm"});
        assert!(parse_task_create_input(&input).is_none());
    }

    #[test]
    fn parse_task_create_result_id_matches_canonical_prefix() {
        assert_eq!(
            parse_task_create_result_id("Task #42 created successfully: Read the file"),
            Some("42".to_owned()),
        );
        assert_eq!(
            parse_task_create_result_id("  Task #7 created successfully: x"),
            Some("7".to_owned()),
            "leading whitespace tolerated",
        );
    }

    #[test]
    fn parse_task_create_result_id_returns_none_for_malformed_text() {
        for text in [
            "",
            "Task created but missing #N",
            "Task # created successfully: x",
            "Task #abc created successfully: x",
            "Task #1 not created",
            "Some other message",
        ] {
            assert!(parse_task_create_result_id(text).is_none(), "expected None for {text:?}");
        }
    }

    #[test]
    fn apply_task_create_appends_item_with_id_and_pending_status() {
        let mut app = App::test_default();
        let input = json!({"subject": "Read", "activeForm": "Reading"});
        let parsed = parse_task_create_input(&input).expect("parsed");
        apply_task_create(&mut app, parsed, "42".to_owned());
        assert_eq!(app.todos().len(), 1);
        assert_eq!(app.todos()[0].id, "42");
        assert_eq!(app.todos()[0].content, "Read");
        assert_eq!(app.todos()[0].active_form, "Reading");
        assert_eq!(app.todos()[0].status, TodoStatus::Pending);
    }

    #[test]
    fn parse_task_update_input_returns_none_without_task_id() {
        assert!(parse_task_update_input(&json!({"status": "in_progress"})).is_none());
    }

    #[test]
    fn parse_task_update_input_reads_optional_fields() {
        let input = json!({
            "taskId": "1",
            "status": "completed",
            "subject": "Updated subject",
            "activeForm": "Updated active form",
            "description": "ignored in V1",
            "owner": "ignored in V1",
            "addBlockedBy": ["ignored"]
        });
        let parsed = parse_task_update_input(&input).expect("taskId present");
        assert_eq!(parsed.task_id, "1");
        assert_eq!(parsed.status, Some(TaskStatusUpdate::Completed));
        assert_eq!(parsed.content.as_deref(), Some("Updated subject"));
        assert_eq!(parsed.active_form.as_deref(), Some("Updated active form"));
    }

    #[test]
    fn apply_task_update_status_transition_mutates_in_place() {
        let mut app = App::test_default();
        apply_task_create(
            &mut app,
            TaskCreateInput { content: "Read".into(), active_form: "Reading".into() },
            "1".to_owned(),
        );
        apply_task_update(
            &mut app,
            TaskUpdateInput {
                task_id: "1".to_owned(),
                status: Some(TaskStatusUpdate::InProgress),
                content: None,
                active_form: None,
            },
        );
        assert_eq!(app.todos().len(), 1, "no new item added");
        assert_eq!(app.todos()[0].status, TodoStatus::InProgress);
        assert_eq!(app.todos()[0].content, "Read");
    }

    #[test]
    fn apply_task_update_status_deleted_removes_item() {
        let mut app = App::test_default();
        apply_task_create(
            &mut app,
            TaskCreateInput { content: "Read".into(), active_form: "Reading".into() },
            "1".to_owned(),
        );
        apply_task_create(
            &mut app,
            TaskCreateInput { content: "Write".into(), active_form: "Writing".into() },
            "2".to_owned(),
        );
        apply_task_update(
            &mut app,
            TaskUpdateInput {
                task_id: "1".to_owned(),
                status: Some(TaskStatusUpdate::Deleted),
                content: None,
                active_form: None,
            },
        );
        assert_eq!(app.todos().len(), 1, "deleted item is removed, the other stays");
        assert_eq!(app.todos()[0].id, "2");
    }

    #[test]
    fn apply_task_update_unknown_task_id_no_ops() {
        let mut app = App::test_default();
        apply_task_create(
            &mut app,
            TaskCreateInput { content: "Read".into(), active_form: "Reading".into() },
            "1".to_owned(),
        );
        apply_task_update(
            &mut app,
            TaskUpdateInput {
                task_id: "999".to_owned(),
                status: Some(TaskStatusUpdate::Completed),
                content: None,
                active_form: None,
            },
        );
        assert_eq!(app.todos().len(), 1, "no change when id misses");
        assert_eq!(app.todos()[0].status, TodoStatus::Pending);
    }
}
