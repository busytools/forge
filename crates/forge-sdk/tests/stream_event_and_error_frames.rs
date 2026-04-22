//! Round-trip tests for `stream_event` and `error` stream-json frames.
//!
//! Ported from claude-agent-sdk-python v0.1.64 `types.py:1043-1050`
//! (`StreamEvent`) + `_internal/message_parser.py:229-240` and
//! `_internal/query.py:315` (the synthesised `error` frame).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::Message;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch, decode_line};
use serde_json::json;

#[test]
fn stream_event_minimal_parses() {
    let line = r#"{"type":"stream_event","uuid":"evt-1","session_id":"sess-1","event":{"type":"message_start"}}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::StreamEvent {
            uuid,
            session_id,
            event,
            parent_tool_use_id,
        } => {
            assert_eq!(uuid, "evt-1");
            assert_eq!(session_id, "sess-1");
            assert_eq!(event, json!({"type": "message_start"}));
            assert!(parent_tool_use_id.is_none());
        }
        other => panic!("expected StreamEvent, got {other:?}"),
    }
}

#[test]
fn stream_event_with_parent_tool_use_id_parses() {
    // Sub-agent stream events carry the parent tool_use_id so UI clients
    // can attribute chunks back to the right agent.
    let line = r#"{"type":"stream_event","uuid":"evt-2","session_id":"sess-2","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}},"parent_tool_use_id":"toolu_parent"}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::StreamEvent {
            parent_tool_use_id,
            event,
            ..
        } => {
            assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_parent"));
            assert_eq!(event["type"], "content_block_delta");
        }
        other => panic!("expected StreamEvent, got {other:?}"),
    }
}

#[test]
fn error_frame_parses() {
    // Emitted by Python `_internal/query.py:315` when the read loop
    // itself hits a fatal exception. forge-sdk preserves the shape.
    let line = r#"{"type":"error","error":"connection closed unexpectedly"}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::Error { error } => {
            assert_eq!(error, "connection closed unexpectedly");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn dispatch_surfaces_stream_event_as_message() {
    let line = r#"{"type":"stream_event","uuid":"evt-3","session_id":"sess-3","event":{"type":"message_stop"}}"#;
    match decode_dispatch(line, 1).expect("decode") {
        DecodedLine::Message(Message::StreamEvent { uuid, .. }) => {
            assert_eq!(uuid, "evt-3");
        }
        other => panic!("expected Message(StreamEvent), got {other:?}"),
    }
}

#[test]
fn dispatch_surfaces_error_frame_as_message() {
    let line = r#"{"type":"error","error":"boom"}"#;
    match decode_dispatch(line, 1).expect("decode") {
        DecodedLine::Message(Message::Error { error }) => {
            assert_eq!(error, "boom");
        }
        other => panic!("expected Message(Error), got {other:?}"),
    }
}

#[test]
fn stream_event_roundtrips_through_serde() {
    let original = Message::StreamEvent {
        uuid: "evt-rt".into(),
        session_id: "sess-rt".into(),
        event: json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        parent_tool_use_id: None,
    };
    let wire = serde_json::to_string(&original).expect("serialize");
    let decoded: Message = serde_json::from_str(&wire).expect("decode");
    assert_eq!(decoded, original);
}

#[test]
fn error_frame_roundtrips_through_serde() {
    let original = Message::Error {
        error: "pipe broken".into(),
    };
    let wire = serde_json::to_string(&original).expect("serialize");
    let decoded: Message = serde_json::from_str(&wire).expect("decode");
    assert_eq!(decoded, original);
}
