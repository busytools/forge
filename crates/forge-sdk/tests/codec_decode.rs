//! Unit tests for the stream-json line codec.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::messages::Message;
use forge_sdk::transport::codec::{decode_line, encode_user_prompt};

#[test]
fn decode_valid_assistant_line() {
    let line = r#"{"type":"assistant","message":{"id":"m","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"s","parent_tool_use_id":null}"#;
    let msg = decode_line(line, 1).expect("decode");
    assert!(matches!(msg, Message::Assistant { .. }));
}

#[test]
fn decode_invalid_json_returns_json_decode_error() {
    let line = "not json";
    let err = decode_line(line, 7).expect_err("should fail");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("line 7"),
        "expected line number in error: {rendered}"
    );
}

#[test]
fn decode_unknown_type_returns_message_parse_error() {
    let line = r#"{"type":"quux","data":1}"#;
    let err = decode_line(line, 1).expect_err("should fail");
    assert!(matches!(err, forge_sdk::Error::MessageParse { .. }));
}

#[test]
fn encode_user_prompt_is_single_line_with_newline() {
    let out = encode_user_prompt("hello", "sess_01").expect("encode");
    assert!(
        out.ends_with('\n'),
        "must terminate with newline for stream-json"
    );
    assert_eq!(out.matches('\n').count(), 1, "must be exactly one line");
    let v: serde_json::Value = serde_json::from_str(out.trim_end_matches('\n')).unwrap();
    assert_eq!(v["type"], "user");
    assert_eq!(v["session_id"], "sess_01");
    assert_eq!(v["message"]["role"], "user");
    // Python sends `content` as a bare string for plain-text prompts
    // (client.py:260-267); forge-sdk matches byte-for-byte.
    assert_eq!(v["message"]["content"], "hello");
}

#[test]
fn dispatch_detects_control_request() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{},"permission_suggestions":[],"blocked_path":null,"tool_use_id":"toolu_dispatch","agent_id":null}}"#;
    let decoded = decode_dispatch(line, 1).expect("decode");
    assert!(matches!(decoded, DecodedLine::Control(_)));
}

#[test]
fn dispatch_detects_regular_message() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"assistant","message":{"id":"m","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"s","parent_tool_use_id":null}"#;
    let decoded = decode_dispatch(line, 1).expect("decode");
    assert!(matches!(decoded, DecodedLine::Message(_)));
}

#[test]
fn dispatch_rejects_missing_type() {
    use forge_sdk::transport::codec::decode_dispatch;

    let line = r#"{"nope":"yes"}"#;
    let err = decode_dispatch(line, 5).expect_err("should fail");
    assert!(format!("{err}").contains("missing `type`"));
}

#[test]
fn dispatch_detects_control_cancel_request() {
    // Upstream Python SDK `_internal/query.py:274-280` cancels the in-flight
    // control handler matching `request_id`. forge-sdk must at minimum
    // decode the frame without errors so the read loop keeps going.
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"control_cancel_request","request_id":"req_42"}"#;
    let decoded = decode_dispatch(line, 1).expect("decode");
    match decoded {
        DecodedLine::ControlCancel { request_id } => {
            assert_eq!(request_id, "req_42");
        }
        other => panic!("expected ControlCancel, got: {other:?}"),
    }
}

#[test]
fn dispatch_rejects_control_cancel_missing_request_id() {
    use forge_sdk::transport::codec::decode_dispatch;

    let line = r#"{"type":"control_cancel_request"}"#;
    let err = decode_dispatch(line, 1).expect_err("should fail without request_id");
    assert!(
        format!("{err}").contains("request_id"),
        "error should mention the missing field"
    );
}
