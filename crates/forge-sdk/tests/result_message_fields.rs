//! Regression coverage for the expanded `Message::Result` field set.
//!
//! Ported from claude-agent-sdk-python v0.1.64 `types.py:1023-1039` +
//! `_internal/message_parser.py:205-227`. Before 2026-04-22 the Rust
//! variant had only 8 fields and required `total_cost_usd` / `usage`
//! at the serde layer — both are `data.get(...)` in Python and can be
//! absent (free-tier / error-path frames). These tests guard against
//! regressing to the stricter shape.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::messages::Message;
use serde_json::json;

/// Minimum-viable result frame — only the six required fields. Python
/// accepts this; forge-sdk must too.
#[test]
fn minimal_result_parses_without_cost_or_usage() {
    let raw = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 10,
        "duration_api_ms": 8,
        "is_error": false,
        "num_turns": 1,
        "session_id": "sess-min",
    });
    let msg: Message = serde_json::from_value(raw).expect("parse");
    match msg {
        Message::Result {
            total_cost_usd,
            usage,
            stop_reason,
            result,
            structured_output,
            model_usage,
            permission_denials,
            errors,
            uuid,
            ..
        } => {
            assert!(total_cost_usd.is_none());
            assert!(usage.is_none());
            assert!(stop_reason.is_none());
            assert!(result.is_none());
            assert!(structured_output.is_none());
            assert!(model_usage.is_none());
            assert!(permission_denials.is_none());
            assert!(errors.is_none());
            assert!(uuid.is_none());
        }
        other => panic!("expected Result, got {other:?}"),
    }
}

/// Full payload — every optional field populated. Exercises the
/// `modelUsage` camelCase wire key and captures the result body, the
/// permission-denial vector, and the error vector.
#[test]
fn full_result_parses_and_surfaces_every_field() {
    let raw = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 5000,
        "duration_api_ms": 4200,
        "is_error": false,
        "num_turns": 7,
        "session_id": "sess-full",
        "stop_reason": "end_turn",
        "total_cost_usd": 0.123,
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        },
        "result": "hello world",
        "structured_output": { "answer": 42 },
        "modelUsage": {
            "claude-sonnet-4-6": { "input_tokens": 60, "output_tokens": 30 }
        },
        "permission_denials": [
            { "tool_name": "Bash", "reason": "dry run" }
        ],
        "errors": ["warning: slow"],
        "uuid": "res-1"
    });
    let msg: Message = serde_json::from_value(raw).expect("parse");
    let Message::Result {
        stop_reason,
        total_cost_usd,
        usage,
        result,
        structured_output,
        model_usage,
        permission_denials,
        errors,
        uuid,
        ..
    } = msg
    else {
        panic!("expected Result");
    };
    assert_eq!(stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(total_cost_usd, Some(0.123));
    assert!(usage.is_some());
    assert_eq!(result.as_deref(), Some("hello world"));
    assert_eq!(structured_output, Some(json!({ "answer": 42 })));
    assert!(model_usage.is_some(), "modelUsage should decode");
    assert_eq!(
        model_usage.as_ref().unwrap()["claude-sonnet-4-6"]["input_tokens"],
        60
    );
    assert_eq!(
        permission_denials.as_deref().map(<[_]>::len),
        Some(1),
        "permission_denials should surface"
    );
    assert_eq!(
        errors.as_deref(),
        Some(vec!["warning: slow".to_string()]).as_deref()
    );
    assert_eq!(uuid.as_deref(), Some("res-1"));
}

/// modelUsage must serialize back out as camelCase on the wire — the
/// typical caller-side scenario is round-tripping a decoded result
/// through session-store persistence.
#[test]
fn result_model_usage_roundtrips_as_camel_case() {
    let raw = json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1,
        "duration_api_ms": 1,
        "is_error": false,
        "num_turns": 1,
        "session_id": "sess-rt",
        "modelUsage": { "claude-sonnet-4-6": { "input_tokens": 1 } }
    });
    let msg: Message = serde_json::from_value(raw.clone()).expect("parse");
    let re = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(
        re["modelUsage"]["claude-sonnet-4-6"]["input_tokens"], 1,
        "must preserve camelCase on the way out"
    );
    assert!(
        re.get("model_usage").is_none(),
        "snake_case key must NOT leak"
    );
}
