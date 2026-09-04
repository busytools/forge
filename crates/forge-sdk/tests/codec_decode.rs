//! Unit tests for the stream-json line codec.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::Message;
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
    assert!(rendered.contains("line 7"), "expected line number in error: {rendered}");
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
    assert!(out.ends_with('\n'), "must terminate with newline for stream-json");
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
    let decoded = decode_dispatch(line, 1);
    assert!(matches!(decoded, DecodedLine::Control(_)));
}

#[test]
fn dispatch_detects_regular_message() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"assistant","message":{"id":"m","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":1,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"s","parent_tool_use_id":null}"#;
    let decoded = decode_dispatch(line, 1);
    assert!(matches!(decoded, DecodedLine::Message(_)));
}

#[test]
fn dispatch_classifies_missing_type_as_malformed() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"nope":"yes"}"#;
    match decode_dispatch(line, 5) {
        DecodedLine::Malformed { line: 5, reason } => {
            assert!(reason.contains("missing `type`"), "reason: {reason}");
        }
        other => panic!("expected Malformed, got: {other:?}"),
    }
}

#[test]
fn dispatch_detects_control_cancel_request() {
    // Upstream Python SDK `_internal/query.py:274-280` cancels the in-flight
    // control handler matching `request_id`. forge-sdk must at minimum
    // decode the frame without errors so the read loop keeps going.
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"control_cancel_request","request_id":"req_42"}"#;
    let decoded = decode_dispatch(line, 1);
    match decoded {
        DecodedLine::ControlCancel { request_id } => {
            assert_eq!(request_id, "req_42");
        }
        other => panic!("expected ControlCancel, got: {other:?}"),
    }
}

#[test]
fn dispatch_classifies_control_cancel_missing_request_id_as_malformed() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"control_cancel_request"}"#;
    match decode_dispatch(line, 1) {
        DecodedLine::Malformed { reason, .. } => {
            assert!(reason.contains("request_id"), "reason should mention the missing field");
        }
        other => panic!("expected Malformed, got: {other:?}"),
    }
}

#[test]
fn dispatch_routes_control_response_missing_request_id_to_unknown() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    // A `control_response` lacking `/response/request_id` is wire
    // corruption - it must NOT decode as a "valid" `ControlResponse`
    // with empty id (which would let the conformance harness count it
    // toward `control_responses` instead of flagging the drift).
    let line = r#"{"type":"control_response","response":{"subtype":"success","response":{}}}"#;
    let decoded = decode_dispatch(line, 1);
    match decoded {
        DecodedLine::Unknown { type_str, .. } => {
            assert!(
                type_str.contains("control_response"),
                "type_str should mention control_response, got: {type_str}"
            );
            assert!(
                type_str.contains("missing"),
                "type_str should mention the missing field, got: {type_str}"
            );
        }
        other => panic!("expected DecodedLine::Unknown, got: {other:?}"),
    }
}

#[test]
fn dispatch_routes_well_formed_control_response_to_control_response() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"r0","response":{}}}"#;
    let decoded = decode_dispatch(line, 1);
    match decoded {
        DecodedLine::ControlResponse { request_id, .. } => {
            assert_eq!(request_id, "r0");
        }
        other => panic!("expected ControlResponse, got: {other:?}"),
    }
}

/// The CLI's 30-second heartbeat during a long-running tool call.
/// Fixture is a raw specimen from the live log (2026-09-01 07:53:09),
/// not a constructed value - the point is that this exact wire shape
/// decodes.
#[test]
fn dispatch_models_the_tool_progress_heartbeat() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"tool_progress","tool_use_id":"toolu_01QhFqNDEgKeskhhiYpzeHnL-heartbeat-0","tool_name":"Bash","parent_tool_use_id":"toolu_01QhFqNDEgKeskhhiYpzeHnL","elapsed_time_seconds":30,"heartbeat":true,"session_id":"428903f7-79b3-46ed-aafc-86b0b02ad8b6","uuid":"35fb7d4d-831f-4ae3-9bf0-1740057edeeb"}"#;
    let decoded = decode_dispatch(line, 40);
    match decoded {
        DecodedLine::ToolProgress(progress) => {
            assert_eq!(progress.tool_use_id, "toolu_01QhFqNDEgKeskhhiYpzeHnL-heartbeat-0");
            assert_eq!(progress.tool_name, "Bash");
            assert!((progress.elapsed_time_seconds - 30.0).abs() < f64::EPSILON);
            assert!(progress.heartbeat, "the specimen is a heartbeat");
            assert_eq!(
                progress.parent_tool_use_id.as_deref(),
                Some("toolu_01QhFqNDEgKeskhhiYpzeHnL")
            );
        }
        other => panic!("expected ToolProgress, got: {other:?}"),
    }
}

/// A heartbeat for a top-level tool call has no parent tool use. The
/// specimen above is subagent-side; this is the absent-key case the
/// top-level shape would carry.
#[test]
fn tool_progress_without_a_parent_tool_still_decodes() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"tool_progress","tool_use_id":"toolu_01ABC-heartbeat-3","tool_name":"Bash","elapsed_time_seconds":60}"#;
    let decoded = decode_dispatch(line, 41);
    match decoded {
        DecodedLine::ToolProgress(progress) => {
            assert_eq!(progress.parent_tool_use_id, None);
            assert!(!progress.heartbeat, "absent flag defaults to false");
        }
        other => panic!("expected ToolProgress, got: {other:?}"),
    }
}

/// A `tool_progress` line whose payload does not fit must degrade to
/// `Unknown`, not `Malformed` - an unparseable heartbeat is an
/// unrecognised shape, not a corrupt line, and the harness counts the
/// two differently.
#[test]
fn a_malformed_tool_progress_degrades_to_unknown_not_an_error() {
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

    let line = r#"{"type":"tool_progress","tool_use_id":null,"tool_name":"Bash"}"#;
    let decoded = decode_dispatch(line, 41);
    match decoded {
        DecodedLine::Unknown { type_str, .. } => {
            assert!(
                type_str.contains("tool_progress"),
                "the type name should survive for triage, got: {type_str}"
            );
        }
        other => panic!("expected Unknown, got: {other:?}"),
    }
}
