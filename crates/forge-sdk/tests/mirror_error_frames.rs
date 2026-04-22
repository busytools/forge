//! Round-trip tests for the `mirror_error` system frame.
//!
//! Ported from claude-agent-sdk-python v0.1.64 `types.py:1005-1019` +
//! `_internal/message_parser.py:187-194`. Note that the CLI never emits this
//! frame — it is SDK-synthesised when a [`SessionStore::append`] call fails
//! inside the transcript-mirror batcher. Decoding it here is still required
//! for wire parity with any third-party injector.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::Message;
use forge_sdk::session_store::SessionKey;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch, decode_line};
use serde_json::json;

#[test]
fn mirror_error_with_key_parses() {
    let line = r#"{"type":"system","subtype":"mirror_error","key":{"project_key":"proj","session_id":"sess-1","subpath":null},"error":"disk full"}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::MirrorError { key, error } => {
            let key = key.expect("key present");
            assert_eq!(key.project_key, "proj");
            assert_eq!(key.session_id, "sess-1");
            assert!(key.subpath.is_none());
            assert_eq!(error, "disk full");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn mirror_error_null_key_parses() {
    let line =
        r#"{"type":"system","subtype":"mirror_error","key":null,"error":"store unreachable"}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::MirrorError { key, error } => {
            assert!(key.is_none());
            assert_eq!(error, "store unreachable");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn mirror_error_defaults_missing_error_to_empty_string() {
    // Python `data.get("error", "")` — missing field is fine, empties out.
    let line = r#"{"type":"system","subtype":"mirror_error","key":null}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::MirrorError { key, error } => {
            assert!(key.is_none());
            assert!(error.is_empty());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn mirror_error_with_subpath_key_parses() {
    let line = r#"{"type":"system","subtype":"mirror_error","key":{"project_key":"proj","session_id":"sess","subpath":"subagents/agent-xyz"},"error":"append timed out"}"#;
    let msg = decode_line(line, 1).expect("decode");
    match msg {
        Message::MirrorError { key, error } => {
            let key = key.expect("key present");
            assert_eq!(key.subpath.as_deref(), Some("subagents/agent-xyz"));
            assert_eq!(error, "append timed out");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn dispatch_recognises_mirror_error_as_message() {
    let line = r#"{"type":"system","subtype":"mirror_error","key":null,"error":"oops"}"#;
    let decoded = decode_dispatch(line, 1).expect("dispatch");
    match decoded {
        DecodedLine::Message(Message::MirrorError { .. }) => {}
        other => panic!("expected MirrorError, got: {other:?}"),
    }
}

#[test]
fn mirror_error_roundtrips_through_serde() {
    let raw = json!({
        "type": "system",
        "subtype": "mirror_error",
        "key": {
            "project_key": "proj-ser",
            "session_id": "sess-ser"
        },
        "error": "ser test"
    });
    let msg: Message = serde_json::from_value(raw.clone()).expect("deserialize");
    let re = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(raw, re);
}

#[test]
fn mirror_error_constructs_message_directly() {
    let key = SessionKey {
        project_key: "proj-x".into(),
        session_id: "sess-x".into(),
        subpath: None,
    };
    let msg = Message::MirrorError {
        key: Some(key.clone()),
        error: "hello".into(),
    };
    let v = serde_json::to_value(&msg).expect("serialize");
    assert_eq!(v["type"], "system");
    assert_eq!(v["subtype"], "mirror_error");
    assert_eq!(v["error"], "hello");
    assert_eq!(v["key"]["session_id"], "sess-x");
}
