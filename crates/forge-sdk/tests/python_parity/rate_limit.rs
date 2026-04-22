//! Mirrors `tests/test_rate_limit_event_repro.py` from
//! `claude-agent-sdk-python` v0.1.64.
//!
//! Port of all 5 upstream cases from `TestRateLimitEventHandling`.
//! CLI v2.1.45+ emits `rate_limit_event` messages when the rate-limit
//! status changes for claude.ai subscription users; both SDKs must
//! parse them into a typed variant (not silently drop, not crash).
//!
//! See upstream issue
//! <https://github.com/anthropics/claude-agent-sdk-python/issues/583>
//! for the original bug that motivated this test file.

use forge_sdk::{Message, RateLimitStatus, RateLimitType};
use serde_json::json;

/// Ported from `test_rate_limit_event_parsed_as_typed_message`.
/// `allowed_warning` should parse into a typed `RateLimitEvent` with
/// its `rate_limit_info` fields decoded and the unmodeled
/// `isUsingOverage` preserved on `raw`.
#[test]
fn rate_limit_event_parsed_as_typed_message() {
    let wire = json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": "allowed_warning",
            "resetsAt": 1_700_000_000,
            "rateLimitType": "five_hour",
            "utilization": 0.85,
            "isUsingOverage": false,
        },
        "uuid": "550e8400-e29b-41d4-a716-446655440000",
        "session_id": "test-session-id",
    });
    let msg: Message = serde_json::from_value(wire).expect("parse");
    let Message::RateLimitEvent {
        rate_limit_info,
        uuid,
        session_id,
    } = msg
    else {
        panic!("expected RateLimitEvent");
    };
    assert_eq!(uuid, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(session_id, "test-session-id");
    assert_eq!(rate_limit_info.status, RateLimitStatus::AllowedWarning);
    assert_eq!(rate_limit_info.resets_at, Some(1_700_000_000));
    assert_eq!(
        rate_limit_info.rate_limit_type,
        Some(RateLimitType::FiveHour)
    );
    assert_eq!(rate_limit_info.utilization, Some(0.85));
    // Unmodeled field preserved in raw — mirrors Python's
    // `info.raw["isUsingOverage"] is False`.
    assert_eq!(
        rate_limit_info.raw.get("isUsingOverage"),
        Some(&json!(false))
    );
}

/// Ported from `test_rate_limit_event_rejected_parsed`.
/// Hard rate-limit with overage context.
#[test]
fn rate_limit_event_rejected_parsed() {
    let wire = json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": "rejected",
            "resetsAt": 1_700_003_600,
            "rateLimitType": "seven_day",
            "isUsingOverage": false,
            "overageStatus": "rejected",
            "overageDisabledReason": "out_of_credits",
        },
        "uuid": "660e8400-e29b-41d4-a716-446655440001",
        "session_id": "test-session-id",
    });
    let msg: Message = serde_json::from_value(wire).expect("parse");
    let Message::RateLimitEvent {
        rate_limit_info, ..
    } = msg
    else {
        panic!("expected RateLimitEvent");
    };
    assert_eq!(rate_limit_info.status, RateLimitStatus::Rejected);
    assert_eq!(
        rate_limit_info.overage_status,
        Some(RateLimitStatus::Rejected)
    );
    assert_eq!(
        rate_limit_info.overage_disabled_reason.as_deref(),
        Some("out_of_credits")
    );
}

/// Ported from `test_rate_limit_event_minimal_fields`.
/// Only `status` is required; every other field defaults to `None`.
#[test]
fn rate_limit_event_minimal_fields() {
    let wire = json!({
        "type": "rate_limit_event",
        "rate_limit_info": {"status": "allowed"},
        "uuid": "770e8400-e29b-41d4-a716-446655440002",
        "session_id": "test-session-id",
    });
    let msg: Message = serde_json::from_value(wire).expect("parse");
    let Message::RateLimitEvent {
        rate_limit_info, ..
    } = msg
    else {
        panic!("expected RateLimitEvent");
    };
    assert_eq!(rate_limit_info.status, RateLimitStatus::Allowed);
    assert_eq!(rate_limit_info.resets_at, None);
    assert_eq!(rate_limit_info.rate_limit_type, None);
}

/// Ported from `test_unknown_message_type_returns_none`. Python
/// returns `None`; forge-sdk's codec rejects the frame with a parse
/// error on unknown `type`. The forward-compat guarantee is expressed
/// differently: upstream silently drops unknown, forge-sdk surfaces
/// it — both avoid the crash the original bug produced, but the
/// shape is inverted. Guard that the deserialize fails rather than
/// produces a spurious `Message` variant.
#[test]
fn unknown_message_type_rejected() {
    let wire = json!({
        "type": "some_future_event_type",
        "uuid": "880e8400-e29b-41d4-a716-446655440003",
        "session_id": "test-session-id",
    });
    let result: Result<Message, _> = serde_json::from_value(wire);
    assert!(
        result.is_err(),
        "unknown message types must not silently become a known variant"
    );
}

/// Ported from `test_known_message_types_still_parsed`.
/// Sanity-check: adding a `RateLimitEvent` handler didn't regress the
/// `assistant` path.
#[test]
fn known_message_types_still_parsed() {
    let wire = json!({
        "type": "assistant",
        "session_id": "sess-1",
        "message": {
            "id": "msg_01",
            "role": "assistant",
            "model": "claude-sonnet-4-6-20250929",
            "content": [{"type": "text", "text": "hello"}],
        },
    });
    let msg: Message = serde_json::from_value(wire).expect("parse");
    let Message::Assistant { message, .. } = msg else {
        panic!("expected Assistant variant");
    };
    let first = &message.content[0];
    let forge_sdk::ContentBlock::Text { text } = first else {
        panic!("expected text block");
    };
    assert_eq!(text, "hello");
}
