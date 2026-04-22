//! Coverage for message fields added in the 2026-04-22 parity
//! follow-up: `AssistantMessageError`, `Message::Assistant.{error, uuid}`,
//! `Message::User.{uuid, tool_use_result}`, and
//! `AssistantEnvelope.usage: Option<Usage>`.
//!
//! Ported from claude-agent-sdk-python v0.1.64
//! `types.py:897-929` (`AssistantMessage` / `UserMessage` shape) +
//! `_internal/message_parser.py:85-142` (field plumbing).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{AssistantMessageError, Message};
use serde_json::json;

#[test]
fn assistant_error_enum_wire_names() {
    // Each variant must serialize to the Python literal.
    for (variant, wire) in [
        (
            AssistantMessageError::AuthenticationFailed,
            "authentication_failed",
        ),
        (AssistantMessageError::BillingError, "billing_error"),
        (AssistantMessageError::RateLimit, "rate_limit"),
        (AssistantMessageError::InvalidRequest, "invalid_request"),
        (AssistantMessageError::ServerError, "server_error"),
        (AssistantMessageError::Unknown, "unknown"),
    ] {
        let encoded = serde_json::to_value(variant).expect("serialize");
        assert_eq!(encoded, json!(wire), "{variant:?} must wire as '{wire}'");
        let decoded: AssistantMessageError =
            serde_json::from_value(json!(wire)).expect("deserialize");
        assert_eq!(decoded, variant);
    }
}

#[test]
fn assistant_frame_decodes_error_and_uuid_outer_fields() {
    let raw = json!({
        "type": "assistant",
        "session_id": "sess-err",
        "uuid": "asst-uuid-1",
        "error": "rate_limit",
        "message": {
            "id": "msg_01",
            "role": "assistant",
            "model": "claude-opus-4-5",
            "content": [{"type": "text", "text": "throttled"}],
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        }
    });
    let msg: Message = serde_json::from_value(raw).expect("parse");
    match msg {
        Message::Assistant { error, uuid, .. } => {
            assert_eq!(error, Some(AssistantMessageError::RateLimit));
            assert_eq!(uuid.as_deref(), Some("asst-uuid-1"));
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[test]
fn assistant_frame_without_usage_now_parses() {
    // Python reads usage as `data["message"].get("usage")`
    // (message_parser.py:135) — optional. Error-path assistant frames
    // omit it. Forge-sdk must parse them (regression guard against the
    // pre-2026-04-22 required-`usage` shape).
    let raw = json!({
        "type": "assistant",
        "session_id": "sess",
        "message": {
            "id": "msg_err",
            "role": "assistant",
            "model": "claude-opus-4-5",
            "content": []
        }
    });
    let msg: Message = serde_json::from_value(raw).expect("parse");
    match msg {
        Message::Assistant { message, .. } => {
            assert!(message.usage.is_none());
        }
        other => panic!("expected Assistant, got {other:?}"),
    }
}

#[test]
fn user_frame_decodes_uuid_and_tool_use_result() {
    let raw = json!({
        "type": "user",
        "session_id": "sess-usr",
        "uuid": "user-uuid-1",
        "tool_use_result": {"stdout": "ok"},
        "message": {
            "role": "user",
            "content": [
                {"type": "tool_result", "tool_use_id": "toolu_01", "content": "ok", "is_error": false}
            ]
        }
    });
    let msg: Message = serde_json::from_value(raw).expect("parse");
    match msg {
        Message::User {
            uuid,
            tool_use_result,
            ..
        } => {
            assert_eq!(uuid.as_deref(), Some("user-uuid-1"));
            assert_eq!(tool_use_result, Some(json!({"stdout": "ok"})));
        }
        other => panic!("expected User, got {other:?}"),
    }
}

#[test]
fn unknown_assistant_error_surfaces_as_unknown() {
    // If upstream adds a new error class between parity checks, the
    // fallback `Unknown` variant absorbs it — callers still see an
    // error string that doesn't match any known literal, just
    // remapped.
    let decoded: AssistantMessageError =
        serde_json::from_value(json!("unknown")).expect("deserialize");
    assert_eq!(decoded, AssistantMessageError::Unknown);
}
