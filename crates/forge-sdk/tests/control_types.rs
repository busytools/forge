//! Roundtrip tests for control-protocol wire shapes.
//!
//! Shape matches Python SDK v0.1.64 wire protocol verified from source
//! (types.py:1283-1291 + _internal/query.py:302-324).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::control::{
    AllowBehavior, ControlRequest, ControlRequestKind, ControlResponse, ControlResponseKind,
    ControlResponseType,
};
use serde_json::json;

#[test]
fn parse_can_use_tool_request() {
    let raw = json!({
        "type": "control_request",
        "request_id": "req_1_a1b2c3d4",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Edit",
            "input": {"file_path": "/tmp/x"},
            "permission_suggestions": [],
            "blocked_path": null,
            "tool_use_id": "toolu_01abc",
            "agent_id": null
        }
    });
    let req: ControlRequest = serde_json::from_value(raw).expect("parse");
    assert_eq!(req.request_id, "req_1_a1b2c3d4");
    match req.request {
        ControlRequestKind::CanUseTool {
            tool_name,
            input,
            tool_use_id,
            ..
        } => {
            assert_eq!(tool_name, "Edit");
            assert_eq!(input["file_path"], "/tmp/x");
            assert_eq!(tool_use_id, "toolu_01abc");
        }
        other => panic!("expected CanUseTool, got: {other:?}"),
    }
}

#[test]
fn serialize_allow_response_uses_camelcase_updated_input() {
    let allow = AllowBehavior::Allow {
        updated_input: json!({"file_path": "/tmp/x"}),
        updated_permissions: None,
    };
    let raw = serde_json::to_value(&allow).expect("ser");
    assert_eq!(raw["behavior"], "allow");
    // CRITICAL: Python wire is camelCase `updatedInput`, not snake_case.
    assert_eq!(raw["updatedInput"]["file_path"], "/tmp/x");
    assert!(
        raw.get("updated_input").is_none(),
        "must NOT serialise snake_case"
    );
}

#[test]
fn serialize_deny_response_with_interrupt_true() {
    let deny = AllowBehavior::Deny {
        message: "not allowed".into(),
        interrupt: true,
    };
    let raw = serde_json::to_value(&deny).expect("ser");
    assert_eq!(raw["behavior"], "deny");
    assert_eq!(raw["message"], "not allowed");
    assert_eq!(raw["interrupt"], true);
}

#[test]
fn deny_with_interrupt_false_omits_field() {
    let deny = AllowBehavior::Deny {
        message: "nope".into(),
        interrupt: false,
    };
    let raw = serde_json::to_value(&deny).expect("ser");
    assert_eq!(raw["behavior"], "deny");
    assert!(
        raw.get("interrupt").is_none(),
        "interrupt must be absent when false"
    );
}

#[test]
fn full_control_response_envelope() {
    let resp = ControlResponse {
        ty: ControlResponseType::ControlResponse,
        response: ControlResponseKind::Success {
            request_id: "req_1_a1b2c3d4".into(),
            response: serde_json::to_value(AllowBehavior::Allow {
                updated_input: json!({"file_path": "/tmp/x"}),
                updated_permissions: None,
            })
            .unwrap(),
        },
    };
    let raw = serde_json::to_value(&resp).expect("serialize");
    assert_eq!(raw["type"], "control_response");
    assert_eq!(raw["response"]["subtype"], "success");
    assert_eq!(raw["response"]["request_id"], "req_1_a1b2c3d4");
    assert_eq!(raw["response"]["response"]["behavior"], "allow");
    assert_eq!(
        raw["response"]["response"]["updatedInput"]["file_path"],
        "/tmp/x"
    );
}
