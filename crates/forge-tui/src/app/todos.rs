use super::{App, TodoItem, TodoStatus};

/// Parse a `TodoWrite` `raw_input` JSON value into a `Vec<TodoItem>`.
/// Expected shape: `{"todos": [{"content": "...", "status": "...", "activeForm": "..."}]}`
#[cfg(test)]
pub(super) fn parse_todos(raw_input: &serde_json::Value) -> Vec<TodoItem> {
    parse_todos_if_present(raw_input).unwrap_or_default()
}

/// Parse todos only when a concrete `todos` array is present in `raw_input`.
/// Returns `None` for transient/incomplete payloads (missing or non-array `todos`).
pub(crate) fn parse_todos_if_present(raw_input: &serde_json::Value) -> Option<Vec<TodoItem>> {
    let arr = raw_input.get("todos")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|item| {
                let content = item.get("content")?.as_str()?.to_owned();
                let status_str = item.get("status")?.as_str()?;
                let active_form =
                    item.get("activeForm").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let status = match status_str {
                    "in_progress" => TodoStatus::InProgress,
                    "completed" => TodoStatus::Completed,
                    _ => TodoStatus::Pending,
                };
                Some(TodoItem { content, status, active_form })
            })
            .collect(),
    )
}

/// Replace the active session's todo list. Empties or all-completed
/// lists clear the active list; the Inspector pane then renders just
/// its chrome (banner + rule) until the next `TodoWrite` arrives.
pub(crate) fn set_todos(app: &mut App, todos: Vec<TodoItem>) {
    if todos.is_empty() {
        app.todos_mut().clear();
        return;
    }
    let all_done = todos.iter().all(|t| t.status == TodoStatus::Completed);
    if all_done {
        app.todos_mut().clear();
    } else {
        *app.todos_mut() = todos;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use serde_json::json;

    fn todo(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem { content: content.to_owned(), status, active_form: String::new() }
    }

    #[test]
    fn parse_valid_items_preserve_fields_and_default_missing_active_form() {
        let input = json!({
            "todos": [
                {"content": "Task A", "status": "pending", "activeForm": "Doing A"},
                {"content": "Task B", "status": "in_progress"},
                {"content": "Task C", "status": "completed", "activeForm": "Done C"}
            ]
        });

        let todos = parse_todos(&input);

        assert_eq!(todos.len(), 3);
        assert_eq!(todos[0].content, "Task A");
        assert_eq!(todos[0].status, TodoStatus::Pending);
        assert_eq!(todos[0].active_form, "Doing A");
        assert_eq!(todos[1].status, TodoStatus::InProgress);
        assert_eq!(todos[1].active_form, "");
        assert_eq!(todos[2].status, TodoStatus::Completed);
        assert_eq!(todos[2].active_form, "Done C");
    }

    #[test]
    fn parse_todos_if_present_requires_a_concrete_array() {
        for input in [json!({"other": 1}), json!({"todos": {"not": "array"}})] {
            assert!(parse_todos_if_present(&input).is_none());
        }
        assert!(matches!(parse_todos_if_present(&json!({"todos": []})), Some(v) if v.is_empty()));
    }

    #[test]
    fn parse_skips_invalid_items_and_maps_unknown_statuses_to_pending() {
        let input = json!({
            "todos": [
                {"status": "pending"},
                {"content": "missing status"},
                {"content": 42, "status": "pending"},
                {"content": "Good", "status": "completed"},
                {"content": "Also good", "status": "in_progress"},
                {"content": "Fallback", "status": "banana"},
                {"content": "Case sensitive", "status": "COMPLETED"}
            ]
        });
        let todos = parse_todos(&input);

        assert_eq!(todos.len(), 4);
        assert_eq!(todos[0].content, "Good");
        assert_eq!(todos[0].status, TodoStatus::Completed);
        assert_eq!(todos[1].content, "Also good");
        assert_eq!(todos[1].status, TodoStatus::InProgress);
        assert_eq!(todos[2].status, TodoStatus::Pending);
        assert_eq!(todos[3].status, TodoStatus::Pending);
    }

    #[test]
    fn parse_returns_empty_for_non_todo_shapes() {
        let invalid_inputs = [
            serde_json::Value::Null,
            json!({"todos": null}),
            json!({"todos": 42}),
            json!({"todos": "not an array"}),
            json!([{"content": "Task", "status": "pending"}]),
            json!("just a string"),
            json!(true),
            json!(42),
        ];

        for input in invalid_inputs {
            assert!(parse_todos(&input).is_empty(), "unexpected todos for {input}");
        }
    }

    #[test]
    fn parse_preserves_special_content_without_shape_coupling() {
        let long_content = "A".repeat(10_000);
        let input = json!({
            "metadata": {"nested": {"deep": true}},
            "todos": [
                {"content": "\u{1F680} Deploy to prod", "status": "in_progress", "activeForm": "\u{1F525} Deploying"},
                {"content": "line1\nline2\ttab\r\nwindows", "status": "pending"},
                {"content": long_content, "status": "pending"}
            ],
            "other": [1, 2, 3]
        });

        let todos = parse_todos(&input);

        assert_eq!(todos.len(), 3);
        assert_eq!(todos[0].content, "\u{1F680} Deploy to prod");
        assert_eq!(todos[0].active_form, "\u{1F525} Deploying");
        assert!(todos[1].content.contains('\n'));
        assert!(todos[1].content.contains('\t'));
        assert_eq!(todos[2].content.len(), 10_000);
    }

    #[test]
    fn set_todos_clears_list_for_empty_or_all_completed_inputs() {
        for todos in [
            Vec::new(),
            vec![todo("done", TodoStatus::Completed), todo("done too", TodoStatus::Completed)],
        ] {
            let mut app = App::test_default();
            *app.todos_mut() = vec![todo("seed", TodoStatus::Pending)];

            set_todos(&mut app, todos);

            assert!(app.todos().is_empty());
        }
    }

    #[test]
    fn set_todos_retains_visible_items() {
        let mut app = App::test_default();
        *app.todos_mut() =
            vec![todo("old", TodoStatus::Pending), todo("old-2", TodoStatus::Pending)];

        set_todos(
            &mut app,
            vec![todo("new-a", TodoStatus::Pending), todo("new-b", TodoStatus::InProgress)],
        );

        assert_eq!(app.todos().len(), 2);
        assert_eq!(app.todos()[0].content, "new-a");
        assert_eq!(app.todos()[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn set_todos_clears_when_all_completed() {
        let mut app = App::test_default();
        let active = vec![
            todo("pending", TodoStatus::Pending),
            todo("running", TodoStatus::InProgress),
        ];
        set_todos(&mut app, active);
        assert_eq!(app.todos().len(), 2);
        assert_eq!(app.todos()[0].status, TodoStatus::Pending);
        assert_eq!(app.todos()[1].status, TodoStatus::InProgress);

        let completed = vec![todo("done", TodoStatus::Completed)];
        set_todos(&mut app, completed);
        assert!(app.todos().is_empty());
    }
}
