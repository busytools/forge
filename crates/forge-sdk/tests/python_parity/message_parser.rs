//! Mirrors `tests/test_message_parser.py` from `claude-agent-sdk-python` v0.1.64.
//!
//! Python uses a dedicated `parse_message(dict)` entry point; forge-sdk
//! routes the same shape through `serde_json::from_value::<Message>(...)`.
//! These tests assert the same per-variant field coverage: user,
//! assistant, and result messages with the full range of content blocks.

use forge_sdk::content::ContentBlock;
use forge_sdk::messages::Message;
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
