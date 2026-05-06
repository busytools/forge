//! Wire-decoding integration tests for the lifted message types.
//!
//! The `Message`, `RateLimitInfo`, etc. types live in
//! `forge-primitives` (lifted 2026-05-05). These tests exercise them
//! against `forge-sdk`'s `transport::codec` decoders — i.e. the path
//! the live `claude` subprocess actually takes. They moved here from
//! the messages.rs file in primitives because primitives has no
//! transport module.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(test)]
mod tests_rate_limit_frames {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch, decode_line};
    use forge_sdk::{Message, RateLimitInfo, RateLimitStatus, RateLimitType};
    use serde_json::json;

    #[test]
    fn rate_limit_event_minimal_parses() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"},"uuid":"evt-1","session_id":"sess"}"#;
        let msg = decode_line(line, 1).expect("decode");
        match msg {
            Message::RateLimitEvent { rate_limit_info, uuid, session_id } => {
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
            Message::RateLimitEvent { rate_limit_info, .. } => {
                assert_eq!(rate_limit_info.status, RateLimitStatus::AllowedWarning);
                assert_eq!(rate_limit_info.resets_at, Some(1_745_000_000));
                assert_eq!(rate_limit_info.rate_limit_type, Some(RateLimitType::FiveHour));
                assert_eq!(rate_limit_info.utilization, Some(0.82));
                assert_eq!(rate_limit_info.overage_status, Some(RateLimitStatus::Allowed));
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
}

#[cfg(test)]
mod tests_task_lifecycle_frames {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch, decode_line};
    use forge_sdk::{Message, TaskNotificationStatus, TaskUsage};
    use serde_json::json;

    #[test]
    fn task_started_minimal_parses() {
        let line = r#"{"type":"system","subtype":"task_started","task_id":"t-1","description":"do the thing","uuid":"u-1","session_id":"sess"}"#;
        let msg = decode_line(line, 1).expect("decode");
        match msg {
            Message::TaskStarted {
                task_id,
                description,
                uuid,
                session_id,
                tool_use_id,
                task_type,
            } => {
                assert_eq!(task_id, "t-1");
                assert_eq!(description, "do the thing");
                assert_eq!(uuid, "u-1");
                assert_eq!(session_id, "sess");
                assert!(tool_use_id.is_none());
                assert!(task_type.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn task_started_full_payload_parses() {
        let line = r#"{"type":"system","subtype":"task_started","task_id":"t-1","description":"do it","uuid":"u-1","session_id":"sess","tool_use_id":"toolu_abc","task_type":"general-purpose"}"#;
        let msg = decode_line(line, 1).expect("decode");
        match msg {
            Message::TaskStarted { tool_use_id, task_type, .. } => {
                assert_eq!(tool_use_id.as_deref(), Some("toolu_abc"));
                assert_eq!(task_type.as_deref(), Some("general-purpose"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn task_progress_with_usage_parses() {
        let line = r#"{"type":"system","subtype":"task_progress","task_id":"t-2","description":"halfway","usage":{"total_tokens":1500,"tool_uses":4,"duration_ms":12000},"uuid":"u-2","session_id":"sess","last_tool_name":"Grep"}"#;
        let msg = decode_line(line, 1).expect("decode");
        match msg {
            Message::TaskProgress {
                task_id,
                description,
                usage,
                uuid,
                session_id,
                tool_use_id,
                last_tool_name,
            } => {
                assert_eq!(task_id, "t-2");
                assert_eq!(description, "halfway");
                assert_eq!(usage.total_tokens, 1500);
                assert_eq!(usage.tool_uses, 4);
                assert_eq!(usage.duration_ms, 12_000);
                assert_eq!(uuid, "u-2");
                assert_eq!(session_id, "sess");
                assert!(tool_use_id.is_none());
                assert_eq!(last_tool_name.as_deref(), Some("Grep"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn task_notification_completed_parses() {
        let line = r#"{"type":"system","subtype":"task_notification","task_id":"t-3","status":"completed","output_file":"/tmp/out","summary":"all done","uuid":"u-3","session_id":"sess","usage":{"total_tokens":9000,"tool_uses":12,"duration_ms":45000}}"#;
        let msg = decode_line(line, 1).expect("decode");
        match msg {
            Message::TaskNotification {
                task_id,
                status,
                output_file,
                summary,
                uuid,
                session_id,
                tool_use_id,
                usage,
            } => {
                assert_eq!(task_id, "t-3");
                assert_eq!(status, TaskNotificationStatus::Completed);
                assert_eq!(output_file, "/tmp/out");
                assert_eq!(summary, "all done");
                assert_eq!(uuid, "u-3");
                assert_eq!(session_id, "sess");
                assert!(tool_use_id.is_none());
                let u = usage.expect("usage");
                assert_eq!(u.total_tokens, 9000);
                assert_eq!(u.tool_uses, 12);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn task_notification_status_variants() {
        for (wire, expected) in [
            ("completed", TaskNotificationStatus::Completed),
            ("failed", TaskNotificationStatus::Failed),
            ("stopped", TaskNotificationStatus::Stopped),
        ] {
            let status: TaskNotificationStatus = serde_json::from_value(json!(wire))
                .unwrap_or_else(|e| panic!("status `{wire}`: {e}"));
            assert_eq!(status, expected);
        }
    }

    #[test]
    fn task_notification_without_usage_parses() {
        let line = r#"{"type":"system","subtype":"task_notification","task_id":"t-4","status":"failed","output_file":"/tmp/out","summary":"bad","uuid":"u-4","session_id":"sess"}"#;
        let msg = decode_line(line, 1).expect("decode");
        match msg {
            Message::TaskNotification { status, usage, .. } => {
                assert_eq!(status, TaskNotificationStatus::Failed);
                assert!(usage.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn task_usage_roundtrip() {
        let u = TaskUsage { total_tokens: 10, tool_uses: 2, duration_ms: 500 };
        let v = serde_json::to_value(u).expect("ser");
        let back: TaskUsage = serde_json::from_value(v).expect("de");
        assert_eq!(u, back);
    }

    #[test]
    fn dispatch_recognises_task_started_as_message() {
        let line = r#"{"type":"system","subtype":"task_started","task_id":"t","description":"d","uuid":"u","session_id":"s"}"#;
        let decoded = decode_dispatch(line, 1).expect("dispatch");
        match decoded {
            DecodedLine::Message(Message::TaskStarted { .. }) => {}
            other => panic!("expected TaskStarted, got: {other:?}"),
        }
    }

    #[test]
    fn dispatch_recognises_task_progress_as_message() {
        let line = r#"{"type":"system","subtype":"task_progress","task_id":"t","description":"d","usage":{"total_tokens":1,"tool_uses":1,"duration_ms":1},"uuid":"u","session_id":"s"}"#;
        let decoded = decode_dispatch(line, 1).expect("dispatch");
        match decoded {
            DecodedLine::Message(Message::TaskProgress { .. }) => {}
            other => panic!("expected TaskProgress, got: {other:?}"),
        }
    }

    #[test]
    fn dispatch_recognises_task_notification_as_message() {
        let line = r#"{"type":"system","subtype":"task_notification","task_id":"t","status":"stopped","output_file":"/x","summary":"s","uuid":"u","session_id":"s"}"#;
        let decoded = decode_dispatch(line, 1).expect("dispatch");
        match decoded {
            DecodedLine::Message(Message::TaskNotification { .. }) => {}
            other => panic!("expected TaskNotification, got: {other:?}"),
        }
    }

    #[test]
    fn unknown_system_subtype_still_lands_in_system_variant() {
        // Init and other known CLI subtypes must keep landing in `Message::System`
        // so existing consumers (e.g. the client init handshake) keep working.
        let line =
            r#"{"type":"system","subtype":"init","session_id":"sess","model":"claude-opus-4-5"}"#;
        let msg = decode_line(line, 1).expect("decode");
        match msg {
            Message::System { subtype, session_id, .. } => {
                assert_eq!(subtype, "init");
                assert_eq!(session_id.as_deref(), Some("sess"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn task_started_roundtrips_through_serde() {
        let raw = json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "t-ser",
            "description": "ser test",
            "uuid": "u-ser",
            "session_id": "sess"
        });
        let msg: Message = serde_json::from_value(raw.clone()).expect("deserialize");
        let re = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(raw, re);
    }
}

#[cfg(test)]
mod tests_stream_event_and_error_frames {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use forge_sdk::Message;
    use forge_sdk::transport::codec::{DecodedLine, decode_dispatch, decode_line};
    use serde_json::json;

    #[test]
    fn stream_event_minimal_parses() {
        let line = r#"{"type":"stream_event","uuid":"evt-1","session_id":"sess-1","event":{"type":"message_start"}}"#;
        let msg = decode_line(line, 1).expect("decode");
        match msg {
            Message::StreamEvent { uuid, session_id, event, parent_tool_use_id } => {
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
            Message::StreamEvent { parent_tool_use_id, event, .. } => {
                assert_eq!(parent_tool_use_id.as_deref(), Some("toolu_parent"));
                assert_eq!(event["type"], "content_block_delta");
            }
            other => panic!("expected StreamEvent, got {other:?}"),
        }
    }

    #[test]
    fn error_frame_parses() {
        // The CLI emits this when its read loop hits a fatal
        // exception. forge-sdk preserves the shape on the wire.
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
        let original = Message::Error { error: "pipe broken".into() };
        let wire = serde_json::to_string(&original).expect("serialize");
        let decoded: Message = serde_json::from_str(&wire).expect("decode");
        assert_eq!(decoded, original);
    }
}
