// Permission grant/deny flow integration tests.
// Validates that PermissionRequest events are correctly attached to tool calls,
// that the pending_interaction_ids queue is maintained, and that responses
// flow through the workspace dispatch path.

use forge_tui::agent::model;
use forge_tui::app::{AppStatus, MessageBlock};
use forge_workspace::SessionUpdate;
use pretty_assertions::assert_eq;

use crate::helpers::{active_session_key, send_client_event, test_app};
use crate::message_helpers::{assistant_message, send_msg, text_block, tool_use_block};

/// Helper: create a tool call, send it, then send a permission request for it.
/// Returns the tool_id so tests can inspect outcomes via `App`'s test capture.
fn setup_permission(
    app: &mut forge_tui::app::App,
    tool_id: &str,
    options: Vec<model::PermissionOption>,
) {
    // First create the tool call so it exists in the index
    send_msg(
        app,
        assistant_message(vec![tool_use_block(
            tool_id,
            "Write",
            serde_json::json!({"file_path": "file"}),
        )]),
    );

    let session_key = forge_workspace::SessionKey::from_str_for_test("test-session");
    let tool_call_update =
        model::ToolCallUpdate::new(tool_id.to_owned(), model::ToolCallUpdateFields::new());
    let request =
        model::RequestPermissionRequest::new("test-session", tool_call_update, options, None);
    forge_tui::app::handle_permission_request_event(app, &session_key, tool_id, request);
}

fn allow_deny_options() -> Vec<model::PermissionOption> {
    vec![
        model::PermissionOption::new("allow", "Allow", model::PermissionOptionKind::AllowOnce),
        model::PermissionOption::new("deny", "Deny", model::PermissionOptionKind::RejectOnce),
    ]
}

// --- PermissionRequest attaches to tool call ---

#[tokio::test]
async fn permission_request_attaches_to_tool_call() {
    let mut app = test_app();
    setup_permission(&mut app, "tc-perm-1", allow_deny_options());

    assert_eq!(app.pending_interaction_ids().len(), 1);
    assert_eq!(app.pending_interaction_ids()[0], "tc-perm-1");

    // The tool call should have a pending_permission
    let (mi, bi) = app.lookup_tool_call("tc-perm-1").expect("missing tool index");
    if let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] {
        assert!(tc.pending_permission.is_some());
        let perm = tc.pending_permission.as_ref().unwrap();
        assert_eq!(perm.options.len(), 2);
        assert_eq!(perm.selected_index, 0);
        assert!(perm.focused, "first permission should be focused");
    } else {
        panic!("expected ToolCall block");
    }
}

#[tokio::test]
async fn permission_request_enables_auto_scroll() {
    let mut app = test_app();
    app.active_viewport_mut().auto_scroll = false;
    setup_permission(&mut app, "tc-scroll", allow_deny_options());
    assert!(app.viewport().auto_scroll, "permission request should enable auto_scroll");
}

// --- Permission for unknown tool call auto-rejects ---

#[tokio::test]
async fn permission_for_unknown_tool_call_auto_rejects() {
    let mut app = test_app();

    let session_key = forge_workspace::SessionKey::from_str_for_test("test-session");
    let tool_id = "nonexistent".to_owned();
    let tool_call_update =
        model::ToolCallUpdate::new(tool_id.clone(), model::ToolCallUpdateFields::new());
    let options = allow_deny_options();
    let request =
        model::RequestPermissionRequest::new("test-session", tool_call_update, options, None);
    forge_tui::app::handle_permission_request_event(&mut app, &session_key, &tool_id, request);

    // Should NOT be in pending queue
    assert!(app.pending_interaction_ids().is_empty());

    // The auto-reject path should have dispatched a permission outcome
    // via the workspace channel (captured by App's test-capture under
    // `cfg(test)`).
    let dispatched = app.test_dispatched_permission_outcomes.borrow();
    let entry = dispatched
        .iter()
        .find(|(tid, _)| tid == &tool_id)
        .expect("auto-reject should dispatch an outcome immediately");
    let forge_primitives::PermissionOutcome::Selected { option_id } = &entry.1 else {
        panic!("expected Selected outcome from auto-reject");
    };
    assert_eq!(option_id, "deny", "auto-reject should pick last option");
}

// --- Multiple permissions queue correctly ---

#[tokio::test]
async fn multiple_permissions_queue_in_order() {
    let mut app = test_app();
    setup_permission(&mut app, "tc-q1", allow_deny_options());
    setup_permission(&mut app, "tc-q2", allow_deny_options());

    assert_eq!(app.pending_interaction_ids().len(), 2);
    assert_eq!(app.pending_interaction_ids()[0], "tc-q1");
    assert_eq!(app.pending_interaction_ids()[1], "tc-q2");

    // First should be focused, second should not
    let (mi1, bi1) = app.lookup_tool_call("tc-q1").expect("missing tool index");
    if let MessageBlock::ToolCall(tc) = &app.messages()[mi1].blocks[bi1] {
        assert!(tc.pending_permission.as_ref().unwrap().focused);
    }
    let (mi2, bi2) = app.lookup_tool_call("tc-q2").expect("missing tool index");
    if let MessageBlock::ToolCall(tc) = &app.messages()[mi2].blocks[bi2] {
        assert!(!tc.pending_permission.as_ref().unwrap().focused);
    }
}

#[tokio::test]
async fn duplicate_permission_request_is_rejected_without_duplicate_queue_entry() {
    let mut app = test_app();
    setup_permission(&mut app, "tc-dup", allow_deny_options());

    let session_key = forge_workspace::SessionKey::from_str_for_test("test-session");
    let tool_call_update = model::ToolCallUpdate::new("tc-dup", model::ToolCallUpdateFields::new());
    let request = model::RequestPermissionRequest::new(
        "test-session",
        tool_call_update,
        allow_deny_options(),
        None,
    );
    forge_tui::app::handle_permission_request_event(&mut app, &session_key, "tc-dup", request);

    assert_eq!(app.pending_interaction_ids(), vec!["tc-dup"]);
    // The duplicate should have auto-rejected via workspace dispatch.
    // The first request is still pending — no outcome dispatched for it.
    let dispatched = app.test_dispatched_permission_outcomes.borrow();
    let entries: Vec<_> = dispatched.iter().filter(|(tid, _)| tid == "tc-dup").collect();
    // Expect exactly one auto-reject outcome (for the duplicate); the
    // first pending hasn't been resolved yet.
    assert_eq!(entries.len(), 1, "duplicate permission should produce one auto-reject");
    let forge_primitives::PermissionOutcome::Selected { option_id } = &entries[0].1 else {
        panic!("expected Selected outcome from duplicate auto-reject");
    };
    assert_eq!(option_id, "deny");
}

// --- Scroll interaction during streaming ---

#[tokio::test]
async fn scroll_target_preserved_across_text_chunks() {
    let mut app = test_app();
    app.active_viewport_mut().scroll_target = 42;
    app.active_viewport_mut().auto_scroll = false;

    send_msg(&mut app, assistant_message(vec![text_block("Some text")]));

    // Text chunks should NOT reset scroll when auto_scroll is off
    assert_eq!(app.viewport().scroll_target, 42, "scroll_target should be preserved");
    assert!(!app.viewport().auto_scroll, "auto_scroll should stay off");
}

#[tokio::test]
async fn tool_call_does_not_change_scroll_when_auto_scroll_off() {
    let mut app = test_app();
    app.active_viewport_mut().scroll_target = 10;
    app.active_viewport_mut().auto_scroll = false;

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-scroll",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );

    assert_eq!(app.viewport().scroll_target, 10, "tool calls shouldn't touch scroll_target");
    assert!(!app.viewport().auto_scroll);
}

// --- TurnComplete transient state reset ---

#[tokio::test]
async fn turn_complete_resets_transient_state() {
    let mut app = test_app();
    app.status = AppStatus::Running;
    app.set_files_accessed(5);
    app.spinner_frame = 42;

    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );

    assert!(matches!(app.status, AppStatus::Ready));
    assert_eq!(app.files_accessed(), 0, "files_accessed should reset");
    // spinner_frame is a UI detail, not reset by TurnComplete (it's driven by tick)
    // pending_interaction_ids should be empty (no permissions were pending)
    assert!(app.pending_interaction_ids().is_empty());
}

#[tokio::test]
async fn turn_complete_does_not_clear_messages() {
    let mut app = test_app();

    send_msg(&mut app, assistant_message(vec![text_block("hello")]));
    assert_eq!(app.messages().len(), 1);

    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );

    assert_eq!(app.messages().len(), 1, "messages should persist across turns");
}

#[tokio::test]
async fn turn_complete_does_not_clear_tool_call_index() {
    let mut app = test_app();

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-persist",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );
    assert!(app.tool_call_index().contains_key("tc-persist"));

    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );

    assert!(
        app.tool_call_index().contains_key("tc-persist"),
        "tool_call_index should persist across turns"
    );
}

#[tokio::test]
async fn turn_complete_does_not_clear_todos() {
    let mut app = test_app();

    // Simulate a TodoWrite by directly setting todos
    *app.todos_mut() = vec![forge_tui::app::TodoItem {
        content: "Test task".into(),
        status: forge_tui::app::TodoStatus::InProgress,
        active_form: "Testing".into(),
    }];
    app.set_todo_verification_nudge(true);

    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );

    assert_eq!(app.todos().len(), 1, "todos should persist across turns");
    assert!(app.todo_verification_nudge(), "nudge flag should persist across turns");
}

#[tokio::test]
async fn turn_complete_does_not_affect_mode() {
    let mut app = test_app();

    app.set_mode(Some(forge_tui::app::ModeState {
        current_mode_id: "plan".into(),
        current_mode_name: "Plan".into(),
        available_modes: vec![forge_tui::app::ModeInfo {
            id: "plan".into(),
            name: "Plan".into(),
            description: None,
        }],
    }));

    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );

    assert!(app.mode().is_some(), "mode should persist across turns");
}
