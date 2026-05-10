// =====
// TESTS: 10
// =====
//
// Tool call lifecycle integration tests.
// Validates the full create -> update -> complete flow for tool calls
// over the wire-message dispatch path.

use forge_tui::agent::model;
use forge_tui::app::{App, AppStatus, MessageBlock, ToolCallInfo, ToolCallScope};
use pretty_assertions::assert_eq;

use crate::helpers::test_app;
use crate::message_helpers::{
    assistant_message, assistant_message_with_parent, send_msg, tool_result_block,
    tool_result_error_block, tool_use_block, user_message,
};

fn tool_call_block<'a>(app: &'a App, id: &str) -> &'a ToolCallInfo {
    let (message_index, block_index) = app.tool_call_index[id];
    app.messages()
        .get(message_index)
        .and_then(|message| message.blocks.get(block_index))
        .and_then(|block| match block {
            MessageBlock::ToolCall(tool_call) => Some(tool_call.as_ref()),
            _ => None,
        })
        .expect("expected ToolCall block")
}

// --- ToolCallUpdate lifecycle ---

#[tokio::test]
async fn tool_call_updates_apply_terminal_statuses_and_title_fields() {
    let mut app = test_app();
    app.status = AppStatus::Running;

    // Tool_use carries the file_path that drives `tool_title("Read", input)`
    // → "Read src/lib.rs". Wire path sets the title at creation time;
    // completion via tool_result transitions status to Completed.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-update",
            "Read",
            serde_json::json!({"file_path": "src/lib.rs"}),
        )]),
    );
    send_msg(&mut app, user_message(vec![tool_result_block("tc-update", serde_json::json!("ok"))]));

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-fail",
            "Write",
            serde_json::json!({"file_path": "out.txt", "content": "x"}),
        )]),
    );
    // is_error=true in tool_result drives status→Failed via build_tool_result_fields.
    send_msg(
        &mut app,
        user_message(vec![tool_result_error_block("tc-fail", serde_json::json!("bang"))]),
    );

    let updated = tool_call_block(&app, "tc-update");
    assert_eq!(updated.title, "Read src/lib.rs");
    assert!(matches!(updated.status, model::ToolCallStatus::Completed));

    let failed = tool_call_block(&app, "tc-fail");
    assert!(matches!(failed.status, model::ToolCallStatus::Failed));
}

// --- All tools terminal -> Thinking ---

#[tokio::test]
async fn terminal_tool_statuses_transition_running_to_thinking_once_all_calls_finish() {
    let mut app = test_app();
    app.status = AppStatus::Running;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-a",
            "Read",
            serde_json::json!({"file_path": "a.txt"}),
        )]),
    );
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-b",
            "Read",
            serde_json::json!({"file_path": "b.txt"}),
        )]),
    );

    assert!(matches!(app.status, AppStatus::Running));

    send_msg(&mut app, user_message(vec![tool_result_block("tc-a", serde_json::json!("ok"))]));
    assert!(matches!(app.status, AppStatus::Running), "one still in progress");

    send_msg(&mut app, user_message(vec![tool_result_block("tc-b", serde_json::json!("ok"))]));
    assert!(matches!(app.status, AppStatus::Thinking), "all-complete should resume thinking");

    let mut mixed_app = test_app();
    mixed_app.status = AppStatus::Running;

    send_msg(
        &mut mixed_app,
        assistant_message(vec![tool_use_block(
            "tc-x",
            "Read",
            serde_json::json!({"file_path": "x.txt"}),
        )]),
    );
    send_msg(
        &mut mixed_app,
        assistant_message(vec![tool_use_block(
            "tc-y",
            "Read",
            serde_json::json!({"file_path": "y.txt"}),
        )]),
    );

    send_msg(
        &mut mixed_app,
        user_message(vec![tool_result_block("tc-x", serde_json::json!("ok"))]),
    );
    send_msg(
        &mut mixed_app,
        user_message(vec![tool_result_error_block("tc-y", serde_json::json!("bang"))]),
    );

    assert!(
        matches!(mixed_app.status, AppStatus::Thinking),
        "mixed terminal outcomes should also resume thinking"
    );
}

// --- Task tool call tracking ---

#[tokio::test]
async fn task_tool_calls_leave_active_set_only_on_terminal_statuses() {
    let mut app = test_app();

    // "Task" tool name normalises to ToolKind::Think and registers
    // the call as a Task in app.active_task_ids.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "task-pend",
            "Task",
            serde_json::json!({"description": "Running subtask"}),
        )]),
    );
    assert!(app.active_task_ids.contains("task-pend"), "new Task should be tracked");

    // The wire path has no equivalent of the SessionUpdate-only
    // intermediate "Pending" status; resending an open tool_use keeps
    // status=in_progress, which preserves active-task tracking.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "task-pend",
            "Task",
            serde_json::json!({"description": "Running subtask"}),
        )]),
    );
    assert!(app.active_task_ids.contains("task-pend"), "still in-progress should stay active");

    send_msg(&mut app, user_message(vec![tool_result_block("task-pend", serde_json::json!("ok"))]));
    assert!(!app.active_task_ids.contains("task-pend"), "completed Task should be removed");

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "task-fail",
            "Task",
            serde_json::json!({"description": "Subtask"}),
        )]),
    );
    assert!(app.active_task_ids.contains("task-fail"));

    send_msg(
        &mut app,
        user_message(vec![tool_result_error_block("task-fail", serde_json::json!("bang"))]),
    );
    assert!(!app.active_task_ids.contains("task-fail"), "failed Task should also be removed");
}

#[tokio::test]
async fn subagent_child_tools_use_explicit_parent_linkage_only() {
    let mut app = test_app();

    // Root Task tool, no parent linkage.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "task-root",
            "Task",
            serde_json::json!({"description": "Research"}),
        )]),
    );

    // Child Bash tool — parent_tool_use_id carried at the assistant
    // envelope level (the wire shape for sub-agent child tools).
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block(
                "child-bash",
                "Bash",
                serde_json::json!({"command": "echo child"}),
            )],
            "task-root",
        ),
    );

    // Main-agent Bash tool, no envelope-level parent.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "main-bash",
            "Bash",
            serde_json::json!({"command": "echo main"}),
        )]),
    );

    assert_eq!(app.tool_call_scope("task-root"), Some(ToolCallScope::SubagentRoot));
    assert_eq!(
        app.tool_call_scope("child-bash"),
        Some(ToolCallScope::SubagentChild { parent_tool_use_id: "task-root".to_owned() })
    );
    assert_eq!(app.tool_call_scope("main-bash"), Some(ToolCallScope::MainAgent));
    assert!(tool_call_block(&app, "child-bash").hidden);
    assert!(!tool_call_block(&app, "main-bash").hidden);
}

#[tokio::test]
async fn tool_call_update_parent_linkage_marks_existing_tool_hidden() {
    let mut app = test_app();

    // Initial Bash tool with no parent linkage.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "child-late",
            "Bash",
            serde_json::json!({"command": "echo child"}),
        )]),
    );
    assert!(!tool_call_block(&app, "child-late").hidden);

    // A subsequent tool_use with the same id and an envelope-level
    // parent re-runs apply_tool_use_block which re-passes meta
    // (now containing parentToolUseId) through the update path.
    send_msg(
        &mut app,
        assistant_message_with_parent(
            vec![tool_use_block(
                "child-late",
                "Bash",
                serde_json::json!({"command": "echo child"}),
            )],
            "task-root",
        ),
    );

    assert_eq!(
        app.tool_call_scope("child-late"),
        Some(ToolCallScope::SubagentChild { parent_tool_use_id: "task-root".to_owned() })
    );
    assert!(tool_call_block(&app, "child-late").hidden);
}

// --- Collapsed tool calls ---

#[tokio::test]
async fn session_collapse_preference_stays_stable_across_tool_call_lifecycle() {
    let mut app = test_app();
    app.tools_collapsed = true;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-col",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );
    assert!(app.tools_collapsed, "session preference should remain collapsed");
    assert!(matches!(tool_call_block(&app, "tc-col").status, model::ToolCallStatus::InProgress));

    // A duplicate tool_use re-fires apply_tool_use_block, which
    // forces status back to in_progress for already-open calls.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-col",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );
    assert!(app.tools_collapsed, "in-progress updates should not flip the preference");
    assert!(matches!(tool_call_block(&app, "tc-col").status, model::ToolCallStatus::InProgress));

    send_msg(&mut app, user_message(vec![tool_result_block("tc-col", serde_json::json!("ok"))]));
    assert!(app.tools_collapsed, "completed updates should keep the preference");
    assert!(matches!(tool_call_block(&app, "tc-col").status, model::ToolCallStatus::Completed));

    let mut expanded_app = test_app();
    expanded_app.tools_collapsed = false;

    send_msg(
        &mut expanded_app,
        assistant_message(vec![tool_use_block(
            "tc-exp",
            "Write",
            serde_json::json!({"file_path": "file"}),
        )]),
    );
    assert!(!expanded_app.tools_collapsed, "expanded preference should remain expanded");

    send_msg(
        &mut expanded_app,
        user_message(vec![tool_result_block("tc-exp", serde_json::json!("ok"))]),
    );
    assert!(!expanded_app.tools_collapsed);
    assert!(matches!(
        tool_call_block(&expanded_app, "tc-exp").status,
        model::ToolCallStatus::Completed
    ));
}

// --- Multiple tool calls indexed correctly ---

#[tokio::test]
async fn multiple_tool_calls_independently_indexed() {
    let mut app = test_app();

    for i in 0..5 {
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                &format!("tc-{i}"),
                "Read",
                serde_json::json!({"file_path": format!("file-{i}")}),
            )]),
        );
    }

    assert_eq!(app.tool_call_index.len(), 5);
    for i in 0..5 {
        let key = format!("tc-{i}");
        assert!(app.tool_call_index.contains_key(&key), "missing {key}");
    }
}

// --- Edge cases: tool call update propagation ---

#[tokio::test]
async fn tool_call_update_via_meta_sets_sdk_tool_name() {
    let mut app = test_app();

    // Initial tool_use with `name="WebSearch"` already produces
    // sdk_tool_name="WebSearch" via meta.claudeCode.toolName at
    // creation time on the wire path; no separate "meta update"
    // event is required.
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-meta",
            "WebSearch",
            serde_json::json!({"query": "rust"}),
        )]),
    );

    let (mi, bi) = app.tool_call_index["tc-meta"];
    if let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] {
        assert_eq!(tc.sdk_tool_name, "WebSearch");
    } else {
        panic!("expected ToolCall block");
    }
}

#[tokio::test]
async fn todowrite_via_update_raw_input_parses_todos() {
    let mut app = test_app();

    // Wire path: TodoWrite with todos input applies the plan via
    // apply_plan_if_todo_write at tool_use time. The original
    // SessionUpdate test split this into create+update; the wire
    // shape collapses to a single envelope.
    let raw = serde_json::json!({"todos": [
        {"content": "Step 1", "status": "pending", "activeForm": "Doing step 1"}
    ]});
    send_msg(&mut app, assistant_message(vec![tool_use_block("tc-todo-up", "TodoWrite", raw)]));

    assert_eq!(app.todos.len(), 1);
    assert_eq!(app.todos[0].content, "Step 1");
}

#[tokio::test]
async fn title_shortened_relative_to_cwd() {
    let mut app = test_app();
    app.cwd_raw = "/home/user/project".into();

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-shorten",
            "Read",
            serde_json::json!({"file_path": "/home/user/project/src/main.rs"}),
        )]),
    );

    let (mi, bi) = app.tool_call_index["tc-shorten"];
    if let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] {
        assert_eq!(tc.title, "Read src/main.rs", "absolute path shortened to relative");
    } else {
        panic!("expected ToolCall block");
    }
}
