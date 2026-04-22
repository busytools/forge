//! Round-trip tests for `rate_limit_event` stream-json frames.
//!
//! Ported from claude-agent-sdk-python v0.1.64 `types.py:1054-1107` +
//! `_internal/message_parser.py:242-262`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::transport::codec::{DecodedLine, decode_dispatch, decode_line};
use forge_sdk::{Message, RateLimitInfo, RateLimitStatus, RateLimitType};
use serde_json::json;

#[test]
fn rate_limit_event_minimal_parses() {
    let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"},"uuid":"evt-1","session_id":"sess"}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::RateLimitEvent {
            rate_limit_info,
            uuid,
            session_id,
        } => {
            assert_eq!(rate_limit_info.status, RateLimitStatus::Allowed);
            assert_eq!(uuid, "evt-1");
            assert_eq!(session_id, "sess");
            assert!(rate_limit_info.resets_at.is_none());
            assert!(rate_limit_info.rate_limit_type.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn rate_limit_event_full_payload_parses_with_camel_case_inner_fields() {
    // Upstream emits `rate_limit_info` inner fields as camelCase
    // (resetsAt, rateLimitType, overageStatus, overageResetsAt,
    // overageDisabledReason) while the outer frame uses snake_case.
    let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1745000000,"rateLimitType":"five_hour","utilization":0.82,"overageStatus":"allowed","overageResetsAt":1745100000,"overageDisabledReason":null},"uuid":"evt-7","session_id":"sess"}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::RateLimitEvent {
            rate_limit_info, ..
        } => {
            assert_eq!(rate_limit_info.status, RateLimitStatus::AllowedWarning);
            assert_eq!(rate_limit_info.resets_at, Some(1_745_000_000));
            assert_eq!(
                rate_limit_info.rate_limit_type,
                Some(RateLimitType::FiveHour)
            );
            assert_eq!(rate_limit_info.utilization, Some(0.82));
            assert_eq!(
                rate_limit_info.overage_status,
                Some(RateLimitStatus::Allowed)
            );
            assert_eq!(rate_limit_info.overage_resets_at, Some(1_745_100_000));
            assert!(rate_limit_info.overage_disabled_reason.is_none());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn rate_limit_event_rejected_status_and_type_variants() {
    for (wire_status, expected_status) in [
        ("allowed", RateLimitStatus::Allowed),
        ("allowed_warning", RateLimitStatus::AllowedWarning),
        ("rejected", RateLimitStatus::Rejected),
    ] {
        let info: RateLimitInfo = serde_json::from_value(json!({"status": wire_status}))
            .unwrap_or_else(|e| panic!("status `{wire_status}`: {e}"));
        assert_eq!(info.status, expected_status);
    }
    for (wire_type, expected_type) in [
        ("five_hour", RateLimitType::FiveHour),
        ("seven_day", RateLimitType::SevenDay),
        ("seven_day_opus", RateLimitType::SevenDayOpus),
        ("seven_day_sonnet", RateLimitType::SevenDaySonnet),
        ("overage", RateLimitType::Overage),
    ] {
        let info: RateLimitInfo =
            serde_json::from_value(json!({"status": "allowed", "rateLimitType": wire_type}))
                .unwrap_or_else(|e| panic!("rateLimitType `{wire_type}`: {e}"));
        assert_eq!(info.rate_limit_type, Some(expected_type));
    }
}

#[test]
fn dispatch_recognises_rate_limit_event_as_message() {
    let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"},"uuid":"evt","session_id":"s"}"#;
    let decoded = decode_dispatch(line, 1).expect("dispatch");
    match decoded {
        DecodedLine::Message(Message::RateLimitEvent { .. }) => {}
        other => panic!("expected RateLimitEvent message, got: {other:?}"),
    }
}
