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
    assert_eq!(v["message"]["content"][0]["type"], "text");
    assert_eq!(v["message"]["content"][0]["text"], "hello");
}
