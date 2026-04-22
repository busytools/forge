//! Round-trip tests for task-lifecycle stream-json frames.
//!
//! Ported from claude-agent-sdk-python v0.1.64 `types.py:939-1003` +
//! `_internal/message_parser.py:147-186`.

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
        Message::TaskStarted {
            tool_use_id,
            task_type,
            ..
        } => {
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
        let status: TaskNotificationStatus =
            serde_json::from_value(json!(wire)).unwrap_or_else(|e| panic!("status `{wire}`: {e}"));
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
    let u = TaskUsage {
        total_tokens: 10,
        tool_uses: 2,
        duration_ms: 500,
    };
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
        Message::System {
            subtype,
            session_id,
            ..
        } => {
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
