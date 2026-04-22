//! Mirrors `tests/test_message_parser.py` from `claude-agent-sdk-python` v0.1.64.
//!
//! Python uses a dedicated `parse_message(dict)` entry point; forge-sdk
//! routes the same shape through `serde_json::from_value::<Message>(...)`.
//! These tests assert the same per-variant field coverage: user,
//! assistant, and result messages with the full range of content blocks.

use forge_sdk::Message;
use forge_sdk::content::ContentBlock;
use serde_json::json;

/// Ported from `claude-agent-sdk-python` v0.1.64 `tests/test_message_parser.py::TestMessageParser::test_parse_valid_user_message`.
#[test]
fn parse_valid_user_message() {
    let data = json!({
        "type": "user",
        "session_id": "sess-uv",
        "message": {"role": "user", "content": [{"type": "text", "text": "Hello"}]}
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    assert_eq!(message.content.len(), 1);
    match &message.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Hello"),
        other => panic!("expected TextBlock, got {other:?}"),
    }
}

/// Ported from `test_parse_user_message_with_uuid` — issue #414 reminder
/// that the `uuid` field is needed for `rewind_files()` flows.
#[test]
fn parse_user_message_with_uuid() {
    let data = json!({
        "type": "user",
        "session_id": "sess-uuid",
        "uuid": "msg-abc123-def456",
        "message": {"role": "user", "content": [{"type": "text", "text": "Hello"}]}
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::User { uuid, .. } = msg else {
        panic!("expected User");
    };
    assert_eq!(uuid.as_deref(), Some("msg-abc123-def456"));
}

/// Ported from `test_parse_user_message_with_tool_use`.
#[test]
fn parse_user_message_with_tool_use() {
    let data = json!({
        "type": "user",
        "session_id": "sess",
        "message": {
            "role": "user",
            "content": [
                {"type": "text", "text": "Let me read this file"},
                {
                    "type": "tool_use",
                    "id": "tool_456",
                    "name": "Read",
                    "input": {"file_path": "/example.txt"}
                }
            ]
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    assert_eq!(message.content.len(), 2);
    assert!(matches!(message.content[0], ContentBlock::Text { .. }));
    match &message.content[1] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "tool_456");
            assert_eq!(name, "Read");
            assert_eq!(input, &json!({"file_path": "/example.txt"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

/// Ported from `test_parse_user_message_with_tool_result`.
#[test]
fn parse_user_message_with_tool_result() {
    let data = json!({
        "type": "user",
        "session_id": "sess",
        "message": {
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "tool_789", "content": "File contents here"}
            ]
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    match &message.content[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "tool_789");
            assert_eq!(content, &json!("File contents here"));
            // Python types is_error as `bool | None`; forge-sdk types as
            // bool with #[serde(default)] — an omitted field defaults
            // to false.
            assert!(!*is_error);
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

/// Ported from `test_parse_user_message_with_tool_result_error`.
#[test]
fn parse_user_message_with_tool_result_error() {
    let data = json!({
        "type": "user",
        "session_id": "sess",
        "message": {
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "tool_err", "content": "File not found", "is_error": true}
            ]
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    match &message.content[0] {
        ContentBlock::ToolResult {
            is_error, content, ..
        } => {
            assert!(*is_error);
            assert_eq!(content, &json!("File not found"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

/// Ported from `test_parse_user_message_with_mixed_content`.
#[test]
fn parse_user_message_with_mixed_content() {
    let data = json!({
        "type": "user",
        "session_id": "sess",
        "message": {
            "role": "user",
            "content": [
                {"type": "text", "text": "Here's what I found:"},
                {"type": "tool_use", "id": "use_1", "name": "Search", "input": {"query": "test"}},
                {"type": "tool_result", "tool_use_id": "use_1", "content": "Search results"},
                {"type": "text", "text": "What do you think?"}
            ]
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    assert_eq!(message.content.len(), 4);
    assert!(matches!(message.content[0], ContentBlock::Text { .. }));
    assert!(matches!(message.content[1], ContentBlock::ToolUse { .. }));
    assert!(matches!(
        message.content[2],
        ContentBlock::ToolResult { .. }
    ));
    assert!(matches!(message.content[3], ContentBlock::Text { .. }));
}

/// Ported from `test_parse_user_message_inside_subagent`. Matches sub-agent
/// turns where the outer `parent_tool_use_id` points back at the spawning
/// tool call.
#[test]
fn parse_user_message_inside_subagent() {
    let data = json!({
        "type": "user",
        "session_id": "sess",
        "parent_tool_use_id": "toolu_01Xrwd5Y13sEHtzScxR77So8",
        "message": {"role": "user", "content": [{"type": "text", "text": "Hello"}]}
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::User {
        parent_tool_use_id, ..
    } = msg
    else {
        panic!("expected User");
    };
    assert_eq!(
        parent_tool_use_id.as_deref(),
        Some("toolu_01Xrwd5Y13sEHtzScxR77So8")
    );
}

/// Ported from `test_parse_valid_assistant_message`.
#[test]
fn parse_valid_assistant_message() {
    let data = json!({
        "type": "assistant",
        "session_id": "sess",
        "message": {
            "id": "msg_01",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Hello"},
                {"type": "tool_use", "id": "tool_123", "name": "Read", "input": {"file_path": "/test.txt"}}
            ],
            "model": "claude-opus-4-1-20250805",
            "usage": {
                "input_tokens": 1,
                "output_tokens": 1,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Assistant { message, .. } = msg else {
        panic!("expected Assistant");
    };
    assert_eq!(message.content.len(), 2);
    assert!(matches!(message.content[0], ContentBlock::Text { .. }));
    assert!(matches!(message.content[1], ContentBlock::ToolUse { .. }));
}

/// Ported from `test_parse_assistant_message_with_thinking`.
#[test]
fn parse_assistant_message_with_thinking() {
    let data = json!({
        "type": "assistant",
        "session_id": "sess",
        "message": {
            "id": "msg_think",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "I'm thinking about the answer...", "signature": "sig-123"},
                {"type": "text", "text": "Here's my response"}
            ],
            "model": "claude-opus-4-1-20250805",
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Assistant { message, .. } = msg else {
        panic!("expected Assistant");
    };
    match &message.content[0] {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, "I'm thinking about the answer...");
            assert_eq!(signature, "sig-123");
        }
        other => panic!("expected Thinking, got {other:?}"),
    }
    match &message.content[1] {
        ContentBlock::Text { text } => assert_eq!(text, "Here's my response"),
        other => panic!("expected Text, got {other:?}"),
    }
}

/// Ported from `test_parse_assistant_message_with_usage` (issue #673) —
/// per-turn `usage` must round-trip without being dropped.
#[test]
fn parse_assistant_message_with_usage() {
    let data = json!({
        "type": "assistant",
        "session_id": "sess",
        "message": {
            "id": "msg_usage",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "claude-opus-4-5",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_read_input_tokens": 2000,
                "cache_creation_input_tokens": 500
            }
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Assistant { message, .. } = msg else {
        panic!("expected Assistant");
    };
    let usage = message.usage.expect("usage present");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 50);
    assert_eq!(usage.cache_read_input_tokens, 2000);
    assert_eq!(usage.cache_creation_input_tokens, 500);
}

/// Ported from `test_parse_assistant_message_without_usage` — Python
/// reads `usage` as optional; synthetic / error-path frames omit it.
/// Regression guard for the C2 fix that made `usage` Option<Usage>.
#[test]
fn parse_assistant_message_without_usage() {
    let data = json!({
        "type": "assistant",
        "session_id": "sess",
        "message": {
            "id": "msg_no_usage",
            "role": "assistant",
            "content": [{"type": "text", "text": "hi"}],
            "model": "claude-opus-4-5"
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Assistant { message, .. } = msg else {
        panic!("expected Assistant");
    };
    assert!(message.usage.is_none());
}

/// Ported from `test_parse_valid_system_message`. Python's
/// `SystemMessage.data` retains the full original dict — including
/// `type`, `subtype`, and `session_id` — so callers can pattern-match
/// on `msg.data.get("subtype")` per the Python idiom.
#[test]
fn parse_valid_system_message_preserves_full_data() {
    let data = json!({
        "type": "system",
        "subtype": "start",
        "session_id": "sess-sys-1",
        "some_extra": "bonus"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::System {
        subtype,
        session_id,
        data,
    } = msg
    else {
        panic!("expected System");
    };
    assert_eq!(subtype, "start");
    assert_eq!(session_id.as_deref(), Some("sess-sys-1"));
    // Full-dict shape from Python — data must include `type`, `subtype`,
    // `session_id`, plus any extra fields.
    assert_eq!(data["type"], "system");
    assert_eq!(data["subtype"], "start");
    assert_eq!(data["session_id"], "sess-sys-1");
    assert_eq!(data["some_extra"], "bonus");
}

/// Ported from `test_parse_user_message_with_string_content_and_tool_use_result`
/// (Python accepts a bare string for `UserMessage.content` — types.py:910).
#[test]
fn parse_user_message_with_string_content() {
    let data = json!({
        "type": "user",
        "session_id": "sess",
        "message": {"role": "user", "content": "bare string prompt"}
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::User { message, .. } = msg else {
        panic!("expected User");
    };
    assert_eq!(
        message.content.len(),
        1,
        "string must normalise to one block"
    );
    match &message.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "bare string prompt"),
        other => panic!("expected Text after string→block conversion, got {other:?}"),
    }
}

/// Ported from `test_parse_valid_result_message`.
#[test]
fn parse_valid_result_message() {
    let data = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1500,
        "duration_api_ms": 1200,
        "is_error": false,
        "num_turns": 2,
        "session_id": "session_123",
        "total_cost_usd": 0.01,
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Result {
        subtype,
        num_turns,
        duration_ms,
        ..
    } = msg
    else {
        panic!("expected Result");
    };
    assert_eq!(subtype, "success");
    assert_eq!(num_turns, 2);
    assert_eq!(duration_ms, 1500);
}

/// Ported from `test_parse_result_message_with_stop_reason`.
#[test]
fn parse_result_message_with_stop_reason() {
    let data = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1,
        "duration_api_ms": 1,
        "is_error": false,
        "num_turns": 1,
        "session_id": "sess",
        "stop_reason": "end_turn"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Result { stop_reason, .. } = msg else {
        panic!("expected Result");
    };
    assert_eq!(stop_reason.as_deref(), Some("end_turn"));
}

/// Ported from `test_parse_result_message_with_null_stop_reason` — a
/// JSON-null `stop_reason` must surface as `None`.
#[test]
fn parse_result_message_with_null_stop_reason() {
    let data = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1,
        "duration_api_ms": 1,
        "is_error": false,
        "num_turns": 1,
        "session_id": "sess",
        "stop_reason": null
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Result { stop_reason, .. } = msg else {
        panic!("expected Result");
    };
    assert!(stop_reason.is_none());
}

// --- Task lifecycle -------------------------------------------------

/// Ported from `test_parse_task_started_message`.
#[test]
fn parse_task_started_message() {
    let data = json!({
        "type": "system",
        "subtype": "task_started",
        "task_id": "task-abc",
        "tool_use_id": "toolu_01",
        "description": "Reticulating splines",
        "task_type": "background",
        "uuid": "uuid-1",
        "session_id": "session-1"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::TaskStarted {
        task_id,
        description,
        uuid,
        session_id,
        tool_use_id,
        task_type,
    } = msg
    else {
        panic!("expected TaskStarted");
    };
    assert_eq!(task_id, "task-abc");
    assert_eq!(description, "Reticulating splines");
    assert_eq!(uuid, "uuid-1");
    assert_eq!(session_id, "session-1");
    assert_eq!(tool_use_id.as_deref(), Some("toolu_01"));
    assert_eq!(task_type.as_deref(), Some("background"));
}

/// Ported from `test_parse_task_started_message_optional_fields_absent`.
#[test]
fn parse_task_started_message_optional_fields_absent() {
    let data = json!({
        "type": "system",
        "subtype": "task_started",
        "task_id": "task-abc",
        "description": "Working",
        "uuid": "uuid-1",
        "session_id": "session-1"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::TaskStarted {
        tool_use_id,
        task_type,
        ..
    } = msg
    else {
        panic!("expected TaskStarted");
    };
    assert!(tool_use_id.is_none());
    assert!(task_type.is_none());
}

/// Ported from `test_parse_task_progress_message`.
#[test]
fn parse_task_progress_message() {
    let data = json!({
        "type": "system",
        "subtype": "task_progress",
        "task_id": "task-abc",
        "tool_use_id": "toolu_01",
        "description": "Halfway there",
        "usage": {"total_tokens": 1234, "tool_uses": 5, "duration_ms": 9876},
        "last_tool_name": "Read",
        "uuid": "uuid-2",
        "session_id": "session-1"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::TaskProgress {
        task_id,
        description,
        usage,
        uuid,
        session_id,
        tool_use_id,
        last_tool_name,
    } = msg
    else {
        panic!("expected TaskProgress");
    };
    assert_eq!(task_id, "task-abc");
    assert_eq!(description, "Halfway there");
    assert_eq!(uuid, "uuid-2");
    assert_eq!(session_id, "session-1");
    assert_eq!(tool_use_id.as_deref(), Some("toolu_01"));
    assert_eq!(last_tool_name.as_deref(), Some("Read"));
    assert_eq!(usage.total_tokens, 1234);
    assert_eq!(usage.tool_uses, 5);
    assert_eq!(usage.duration_ms, 9876);
}

/// Ported from `test_parse_task_notification_message`.
#[test]
fn parse_task_notification_message() {
    use forge_sdk::TaskNotificationStatus;
    let data = json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": "task-abc",
        "tool_use_id": "toolu_01",
        "status": "completed",
        "output_file": "/tmp/out.md",
        "summary": "All done",
        "usage": {"total_tokens": 2000, "tool_uses": 7, "duration_ms": 12345},
        "uuid": "uuid-3",
        "session_id": "session-1"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::TaskNotification {
        task_id,
        status,
        output_file,
        summary,
        usage,
        ..
    } = msg
    else {
        panic!("expected TaskNotification");
    };
    assert_eq!(task_id, "task-abc");
    assert!(matches!(status, TaskNotificationStatus::Completed));
    assert_eq!(output_file, "/tmp/out.md");
    assert_eq!(summary, "All done");
    assert!(usage.is_some());
}

/// Ported from `test_parse_task_notification_message_optional_fields_absent`.
#[test]
fn parse_task_notification_message_optional_fields_absent() {
    use forge_sdk::TaskNotificationStatus;
    let data = json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": "task-abc",
        "status": "failed",
        "output_file": "/tmp/out.md",
        "summary": "Boom",
        "uuid": "uuid-3",
        "session_id": "session-1"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::TaskNotification {
        status,
        usage,
        tool_use_id,
        ..
    } = msg
    else {
        panic!("expected TaskNotification");
    };
    assert!(matches!(status, TaskNotificationStatus::Failed));
    assert!(usage.is_none());
    assert!(tool_use_id.is_none());
}

/// Ported from `test_unknown_system_subtype_yields_generic`.
#[test]
fn unknown_system_subtype_yields_generic() {
    let data = json!({
        "type": "system",
        "subtype": "some_future_subtype",
        "foo": "bar"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    match msg {
        Message::System { subtype, .. } => {
            assert_eq!(subtype, "some_future_subtype");
        }
        other => panic!("expected generic System, got {other:?}"),
    }
}

// --- Rate-limit event -----------------------------------------------

/// Ported from `test_parse_rate_limit_event`.
#[test]
fn parse_rate_limit_event() {
    use forge_sdk::{RateLimitStatus, RateLimitType};
    let data = json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": "allowed_warning",
            "resetsAt": 1_700_000_000,
            "rateLimitType": "five_hour",
            "utilization": 0.85
        },
        "uuid": "evt-1",
        "session_id": "sess"
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::RateLimitEvent {
        rate_limit_info,
        uuid,
        session_id,
    } = msg
    else {
        panic!("expected RateLimitEvent");
    };
    assert_eq!(uuid, "evt-1");
    assert_eq!(session_id, "sess");
    assert_eq!(rate_limit_info.status, RateLimitStatus::AllowedWarning);
    assert_eq!(
        rate_limit_info.rate_limit_type,
        Some(RateLimitType::FiveHour)
    );
    assert_eq!(rate_limit_info.utilization, Some(0.85));
}

// --- Assistant error variants ---------------------------------------

/// Ported from `test_parse_assistant_message_with_authentication_error`.
#[test]
fn parse_assistant_message_with_authentication_error() {
    use forge_sdk::AssistantMessageError;
    let data = json!({
        "type": "assistant",
        "session_id": "sess",
        "error": "authentication_failed",
        "message": {
            "id": "msg_1",
            "role": "assistant",
            "model": "claude-opus-4-5",
            "content": []
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Assistant { error, .. } = msg else {
        panic!("expected Assistant");
    };
    assert_eq!(error, Some(AssistantMessageError::AuthenticationFailed));
}

/// Ported from `test_parse_assistant_message_with_rate_limit_error`.
#[test]
fn parse_assistant_message_with_rate_limit_error() {
    use forge_sdk::AssistantMessageError;
    let data = json!({
        "type": "assistant",
        "session_id": "sess",
        "error": "rate_limit",
        "message": {
            "id": "msg_1",
            "role": "assistant",
            "model": "claude-opus-4-5",
            "content": []
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Assistant { error, .. } = msg else {
        panic!("expected Assistant");
    };
    assert_eq!(error, Some(AssistantMessageError::RateLimit));
}

/// Ported from `test_parse_assistant_message_without_error`.
#[test]
fn parse_assistant_message_without_error() {
    let data = json!({
        "type": "assistant",
        "session_id": "sess",
        "message": {
            "id": "msg_1",
            "role": "assistant",
            "model": "claude-opus-4-5",
            "content": []
        }
    });
    let msg: Message = serde_json::from_value(data).expect("parse");
    let Message::Assistant { error, .. } = msg else {
        panic!("expected Assistant");
    };
    assert!(error.is_none());
}

// --- Error / malformed paths ----------------------------------------

/// Ported from `test_parse_missing_type_field`.
#[test]
fn parse_missing_type_field() {
    let data = json!({
        "session_id": "sess",
        "message": {"role": "user", "content": "x"}
    });
    let result: Result<Message, _> = serde_json::from_value(data);
    assert!(result.is_err(), "missing 'type' must error");
}

/// Ported from `test_parse_unknown_message_type`.
#[test]
fn parse_unknown_message_type() {
    let data = json!({"type": "mystery_future_type", "x": 1});
    let result: Result<Message, _> = serde_json::from_value(data);
    assert!(result.is_err(), "unknown type must error");
}
