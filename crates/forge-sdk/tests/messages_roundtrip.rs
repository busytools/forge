//! Roundtrip tests for the top-level stream-json message shapes.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::{Message, StopReason};
use serde_json::json;

#[test]
fn assistant_message_roundtrip() {
    let raw = json!({
        "type": "assistant",
        "message": {
            "id": "msg_01",
            "role": "assistant",
            "model": "claude-opus-4-5",
            "content": [
                {"type": "text", "text": "Hi there."}
            ],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 12,
                "output_tokens": 3,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        },
        "session_id": "sess_01",
        "parent_tool_use_id": null,
    });
    let msg: Message = serde_json::from_value(raw.clone()).expect("parse");
    match &msg {
        Message::Assistant { message, session_id, .. } => {
            assert_eq!(session_id, "sess_01");
            assert_eq!(message.model, "claude-opus-4-5");
            assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
            assert_eq!(message.content.len(), 1);
        }
        _ => panic!("expected Assistant, got: {msg:?}"),
    }
    let re = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(raw, re, "roundtrip mismatch");
}

#[test]
fn system_init_message_roundtrip() {
    let raw = json!({
        "type": "system",
        "subtype": "init",
        "session_id": "sess_02",
        "cwd": "/tmp/project",
        "tools": ["Edit", "Read", "Bash"],
        "mcp_servers": [],
        "model": "claude-opus-4-5",
        "permissionMode": "default",
        "apiKeySource": "ANTHROPIC_API_KEY"
    });
    let msg: Message = serde_json::from_value(raw.clone()).expect("parse");
    match &msg {
        Message::System { subtype, session_id, data } => {
            assert_eq!(subtype, "init");
            assert_eq!(session_id.as_deref(), Some("sess_02"));
            assert!(data.get("tools").is_some());
        }
        _ => panic!("expected System, got: {msg:?}"),
    }
    let re = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(raw, re);
}

#[test]
fn result_message_roundtrip() {
    let raw = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1234,
        "duration_api_ms": 1100,
        "is_error": false,
        "num_turns": 2,
        "session_id": "sess_03",
        "total_cost_usd": 0.012,
        "usage": {
            "input_tokens": 500,
            "output_tokens": 200,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    });
    let msg: Message = serde_json::from_value(raw.clone()).expect("parse");
    assert!(matches!(msg, Message::Result { .. }));
    let re = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(raw, re);
}

#[test]
fn unknown_message_serializes_raw_verbatim() {
    let raw = json!({
        "type": "future_thing",
        "subtype": "experimental",
        "session_id": "sess_unknown",
        "payload": {"key": "value", "n": 42},
    });
    let msg = Message::Unknown { type_str: "future_thing".to_string(), raw: raw.clone() };
    let re = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(
        raw, re,
        "Message::Unknown must emit the original `raw` bytes verbatim — \
         this is the forward-compat round-trip contract"
    );
    assert!(msg.session_id().is_none(), "Unknown carries no session id");
}

#[test]
fn user_message_with_tool_result_roundtrip() {
    let raw = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_01",
                    "content": "ok",
                    "is_error": false
                }
            ]
        },
        "session_id": "sess_04",
        "parent_tool_use_id": null,
    });
    let msg: Message = serde_json::from_value(raw.clone()).expect("parse");
    assert!(matches!(msg, Message::User { .. }));
    let re = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(raw, re);
}
