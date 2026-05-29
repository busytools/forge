// =====
// TESTS: 19
// =====
//
// State transition integration tests.
// Validates multi-event sequences and App state consistency.

use forge_tui::agent::model;
use forge_tui::app::{AppStatus, MessageBlock, MessageRole};
use forge_workspace::SessionUpdate;
use pretty_assertions::assert_eq;

use crate::helpers::{active_session_key, send_client_event, test_app};
use crate::message_helpers::{
    assistant_message, send_msg, system_message, text_block, tool_result_block, tool_use_block,
    user_message,
};

// --- Full turn lifecycle ---

#[tokio::test]
async fn full_turn_lifecycle_text_only() {
    let mut app = test_app();
    assert!(matches!(app.status, AppStatus::Ready));

    // Agent starts thinking (thought chunk)
    send_msg(
        &mut app,
        assistant_message(vec![forge_primitives::ContentBlock::Thinking {
            thinking: "Planning...".to_owned(),
            signature: String::new(),
        }]),
    );
    assert!(matches!(app.status, AppStatus::Thinking));

    // Agent streams text
    send_msg(&mut app, assistant_message(vec![text_block("Here is my answer.")]));
    assert!(matches!(app.status, AppStatus::Running));

    // Turn completes
    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );
    assert!(matches!(app.status, AppStatus::Ready));
    assert_eq!(app.messages().len(), 1);
}

#[tokio::test]
async fn full_turn_lifecycle_with_tool_calls() {
    let mut app = test_app();

    // Text chunk
    send_msg(&mut app, assistant_message(vec![text_block("Let me check.")]));

    // Tool call
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-flow",
            "Read",
            serde_json::json!({"file_path": "src/lib.rs"}),
        )]),
    );

    // Tool completes (tool result envelope)
    send_msg(&mut app, user_message(vec![tool_result_block("tc-flow", serde_json::json!("ok"))]));
    assert!(matches!(app.status, AppStatus::Thinking));

    // More text
    send_msg(&mut app, assistant_message(vec![text_block(" The file looks good.")]));

    // Turn completes
    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );
    assert!(matches!(app.status, AppStatus::Ready));
}

// --- Error recovery ---

#[tokio::test]
async fn error_then_new_turn_recovers() {
    let mut app = test_app();

    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnError {
            key: session_key,
            message: "timeout".into(),
            class: None,
            terminal_reason: None,
        },
    );
    assert!(matches!(app.status, AppStatus::Error));

    // New text chunk (simulates user retry) starts fresh
    send_msg(&mut app, assistant_message(vec![text_block("Retry answer")]));
    assert!(matches!(app.status, AppStatus::Running));
}

// --- Message accumulation ---

#[tokio::test]
async fn chunks_across_turns_open_a_new_assistant_message() {
    // Regression: previously, chunks arriving after `TurnComplete`
    // (no active turn bound, e.g. a Monitor / Task notification
    // firing on its own) merged into the prior turn's last assistant
    // message — producing rendered output like
    // "...gateway-backend/pull/107Monitor closed cleanly." with no
    // separator. Each unprompted assistant turn must now open its
    // own ChatMessage so the renderer can space them.
    let mut app = test_app();

    // First turn.
    send_msg(&mut app, assistant_message(vec![text_block("Turn 1")]));
    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );
    assert_eq!(app.messages().len(), 1);

    // Second turn (no user message between turns). Should open a
    // fresh assistant ChatMessage rather than appending to "Turn 1".
    send_msg(&mut app, assistant_message(vec![text_block("Turn 2")]));

    assert_eq!(app.messages().len(), 2, "second turn must NOT merge into the first");
    let first = app.messages().first().expect("first turn message");
    let MessageBlock::Text(first_block) = first.blocks.last().expect("first block") else {
        panic!("expected first turn text block");
    };
    assert!(first_block.text.contains("Turn 1"));
    assert!(!first_block.text.contains("Turn 2"), "Turn 2 must not have been merged in");

    let second = app.messages().last().expect("second turn message");
    assert!(matches!(second.role, MessageRole::Assistant));
    let MessageBlock::Text(second_block) = second.blocks.last().expect("second block") else {
        panic!("expected second turn text block");
    };
    assert_eq!(second_block.text, "Turn 2");
}

#[tokio::test]
async fn tool_call_content_update() {
    let mut app = test_app();

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-content",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );

    // Tool result with content payload
    send_msg(
        &mut app,
        user_message(vec![tool_result_block(
            "tc-content",
            serde_json::json!("file contents here"),
        )]),
    );

    let (mi, bi) = app.lookup_tool_call("tc-content").expect("missing tool index");
    if let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] {
        assert!(!tc.content.is_empty(), "content should be set");
    } else {
        panic!("expected ToolCall block");
    }
}

// --- Auto-scroll ---

#[tokio::test]
async fn auto_scroll_maintained_during_streaming() {
    let mut app = test_app();
    assert!(app.viewport().auto_scroll);

    for _ in 0..20 {
        send_msg(&mut app, assistant_message(vec![text_block("More text. ")]));
    }

    assert!(app.viewport().auto_scroll, "auto_scroll should stay true during streaming");
}

// --- Stress: many tool calls in one turn ---

#[tokio::test]
async fn stress_many_tool_calls_in_one_turn() {
    let mut app = test_app();
    app.status = AppStatus::Running;

    for i in 0..50 {
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                &format!("stress-{i}"),
                "Read",
                serde_json::json!({}),
            )]),
        );
    }

    assert_eq!(app.tool_call_index().len(), 50);

    // Complete all (tool result envelopes finalise each tool_use_id).
    for i in 0..50 {
        send_msg(
            &mut app,
            user_message(vec![tool_result_block(&format!("stress-{i}"), serde_json::json!("ok"))]),
        );
    }

    assert!(matches!(app.status, AppStatus::Thinking));
}

// --- CurrentModeUpdate ---

#[tokio::test]
async fn mode_updates_switch_known_modes_fall_back_for_unknown_ids_and_noop_without_state() {
    let mut app = test_app();

    app.set_mode(Some(forge_tui::app::ModeState {
        current_mode_id: "code".into(),
        current_mode_name: "Code".into(),
        available_modes: vec![
            forge_tui::app::ModeInfo { id: "code".into(), name: "Code".into(), description: None },
            forge_tui::app::ModeInfo { id: "plan".into(), name: "Plan".into(), description: None },
        ],
    }));

    send_msg(&mut app, system_message("status", serde_json::json!({"permissionMode": "plan"})));
    let mode = app.mode().expect("mode should still exist");
    assert_eq!(mode.current_mode_id, "plan");
    assert_eq!(mode.current_mode_name, "Plan");

    send_msg(
        &mut app,
        system_message("status", serde_json::json!({"permissionMode": "unknown-mode"})),
    );
    let mode = app.mode().expect("mode should still exist");
    assert_eq!(mode.current_mode_id, "unknown-mode");
    assert_eq!(mode.current_mode_name, "unknown-mode");

    let mut no_mode_app = test_app();
    send_msg(
        &mut no_mode_app,
        system_message("status", serde_json::json!({"permissionMode": "plan-mode"})),
    );
    assert!(no_mode_app.mode().is_none(), "update without existing mode state is a no-op");
}

// --- Edge cases: interleaved events ---

#[tokio::test]
async fn text_between_tool_calls_creates_separate_blocks() {
    let mut app = test_app();

    send_msg(&mut app, assistant_message(vec![text_block("Before tool")]));
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block("tc-inter", "Read", serde_json::json!({}))]),
    );
    send_msg(&mut app, assistant_message(vec![text_block("After tool")]));
    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block("tc-inter2", "Write", serde_json::json!({}))]),
    );
    send_msg(&mut app, assistant_message(vec![text_block("Final text")]));

    // Should be: Text, ToolCall, Text, ToolCall, Text = 5 blocks
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].blocks.len(), 5);
    assert!(matches!(app.messages()[0].blocks[0], MessageBlock::Text(..)));
    assert!(matches!(app.messages()[0].blocks[1], MessageBlock::ToolCall(_)));
    assert!(matches!(app.messages()[0].blocks[2], MessageBlock::Text(..)));
    assert!(matches!(app.messages()[0].blocks[3], MessageBlock::ToolCall(_)));
    assert!(matches!(app.messages()[0].blocks[4], MessageBlock::Text(..)));
}

#[tokio::test]
async fn rapid_turn_complete_then_new_streaming() {
    let mut app = test_app();

    // First turn
    send_msg(&mut app, assistant_message(vec![text_block("Turn 1")]));
    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );
    assert!(matches!(app.status, AppStatus::Ready));
    assert_eq!(app.files_accessed(), 0);

    // Immediately start second turn
    send_msg(&mut app, assistant_message(vec![text_block("Turn 2")]));
    assert!(matches!(app.status, AppStatus::Running));

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-t2",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );
    assert_eq!(app.files_accessed(), 1);

    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );
    assert!(matches!(app.status, AppStatus::Ready));
    assert_eq!(app.files_accessed(), 0, "reset again on second TurnComplete");
}

#[tokio::test]
async fn available_commands_update_replaces_previous() {
    let mut app = test_app();

    // System(init) wire envelope carries `slash_commands` as a string array;
    // the wire walker derives `AvailableCommand` envelopes from it. Description
    // and input_hint are dropped by that path, but this test only asserts count.
    send_msg(
        &mut app,
        system_message("init", serde_json::json!({"slash_commands": ["/help", "/clear"]})),
    );
    assert_eq!(app.available_commands().len(), 2);

    // New update replaces, not appends
    send_msg(&mut app, system_message("init", serde_json::json!({"slash_commands": ["/commit"]})));
    assert_eq!(app.available_commands().len(), 1, "replaced, not appended");
}

#[tokio::test]
async fn error_during_tool_calls_leaves_tool_calls_intact() {
    let mut app = test_app();

    send_msg(&mut app, assistant_message(vec![text_block("working")]));

    send_msg(
        &mut app,
        assistant_message(vec![tool_use_block(
            "tc-err",
            "Read",
            serde_json::json!({"file_path": "file"}),
        )]),
    );

    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnError {
            key: session_key,
            message: "crashed".into(),
            class: None,
            terminal_reason: None,
        },
    );

    assert!(matches!(app.status, AppStatus::Error));
    // Tool call should remain indexed and preserved in the original assistant message.
    assert!(app.tool_call_index().contains_key("tc-err"));
    assert_eq!(app.messages().len(), 2, "assistant message + system error message");
    assert!(matches!(app.messages()[0].role, MessageRole::Assistant));
    assert_eq!(app.messages()[0].blocks.len(), 2, "text + tool call preserved");
    let Some(MessageBlock::ToolCall(tc)) = app.messages()[0].blocks.get(1) else {
        panic!("expected preserved tool call block");
    };
    assert_eq!(tc.id, "tc-err");
    assert_eq!(tc.status, model::ToolCallStatus::Failed, "in-progress tool should be failed");

    assert!(matches!(app.messages()[1].role, MessageRole::System(_)));
    let Some(MessageBlock::Text(block)) = app.messages()[1].blocks.first() else {
        panic!("expected system error text block");
    };
    assert!(block.text.contains("Turn failed: crashed"));
}

#[tokio::test]
async fn files_accessed_accumulates_across_tool_calls_in_one_turn() {
    let mut app = test_app();

    for i in 0..3 {
        send_msg(
            &mut app,
            assistant_message(vec![tool_use_block(
                &format!("tc-acc-{i}"),
                "Read",
                serde_json::json!({"file_path": format!("file-{i}")}),
            )]),
        );
    }

    assert_eq!(app.files_accessed(), 3, "one per tool call");
    let session_key = active_session_key(&app);
    send_client_event(
        &mut app,
        SessionUpdate::TurnComplete { key: session_key, terminal_reason: None },
    );
    assert_eq!(app.files_accessed(), 0, "reset on turn complete");
}

// --- SdkMessageReceived session_id handling ---

/// Regression: an `SdkMessageReceived` envelope arriving while
/// `app.session_id` holds the empty placeholder (the value the bridge
/// captures from `Client::session_id()` at spawn time, before
/// `system/init` lands) used to be dropped silently — leaving the
/// chat unrendered and the spinner stuck on Thinking forever. The
/// handler now adopts the wire id onto `app.session_id` and processes
/// the message.
#[tokio::test]
async fn sdk_message_with_empty_app_session_id_adopts_wire_id() {
    let mut app = test_app();
    app.set_session_id(Some(model::SessionId::new("")));
    app.status = AppStatus::Thinking;
    // Empty assistant message slot, mimicking what `submit_input`
    // creates right before the first chunk arrives.
    app.active_messages_mut().push(forge_tui::app::ChatMessage::new(
        MessageRole::Assistant,
        Vec::new(),
        None,
    ));
    app.bind_active_turn_assistant_to_tail();

    let wire_msg: forge_primitives::Message = serde_json::from_value(serde_json::json!({
        "type": "assistant",
        "session_id": "real-session-abc",
        "message": {
            "id": "msg_test_1",
            "role": "assistant",
            "model": "test-model",
            "content": [{ "type": "text", "text": "Hello from the assistant." }],
            "stop_reason": null,
            "stop_sequence": null
        }
    }))
    .expect("assistant Message decodes");

    send_client_event(
        &mut app,
        SessionUpdate::ChatAppended { session_id: "real-session-abc".to_owned(), msg: wire_msg },
    );

    assert_eq!(
        app.session_id().map(|s| s.to_string()).as_deref(),
        Some("real-session-abc"),
        "App should have adopted the wire session id",
    );
    let assistant = app
        .messages()
        .iter()
        .rfind(|m| matches!(m.role, MessageRole::Assistant))
        .expect("assistant message present");
    let Some(MessageBlock::Text(block)) = assistant.blocks.first() else {
        panic!("expected the assistant chunk to render as a text block");
    };
    assert!(
        block.text.contains("Hello from the assistant."),
        "assistant chunk should have rendered, got {:?}",
        block.text,
    );
}

/// Once `app.session_id` is the real id, an `SdkMessageReceived` from
/// a *different* session is treated as a stale-Client race envelope
/// and dropped — neither the session id nor the chat moves.
#[tokio::test]
async fn sdk_message_with_mismatched_real_session_id_is_dropped() {
    let mut app = test_app();
    app.set_session_id(Some(model::SessionId::new("real-session-abc")));
    let initial_message_count = app.messages().len();

    let wire_msg: forge_primitives::Message = serde_json::from_value(serde_json::json!({
        "type": "assistant",
        "session_id": "stale-session-xyz",
        "message": {
            "id": "msg_test_2",
            "role": "assistant",
            "model": "test-model",
            "content": [{ "type": "text", "text": "from a stale Client" }],
            "stop_reason": null,
            "stop_sequence": null
        }
    }))
    .expect("assistant Message decodes");

    send_client_event(
        &mut app,
        SessionUpdate::ChatAppended { session_id: "stale-session-xyz".to_owned(), msg: wire_msg },
    );

    assert_eq!(
        app.session_id().map(|s| s.to_string()).as_deref(),
        Some("real-session-abc"),
        "session id must not change on stale envelope",
    );
    assert_eq!(
        app.messages().len(),
        initial_message_count,
        "stale envelope must not append to chat",
    );
}
