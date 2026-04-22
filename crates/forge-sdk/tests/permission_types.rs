//! Unit tests for permission types.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{PermissionDecision, ToolPermissionContext};
use serde_json::json;

#[test]
fn allow_decision_no_modifications() {
    let d = PermissionDecision::allow();
    assert!(d.is_allow());
    assert!(d.updated_input().is_none());
    assert!(d.reason().is_none());
}

#[test]
fn allow_decision_with_updated_input() {
    let d = PermissionDecision::allow_with_input(json!({"file_path": "/tmp/safe.txt"}));
    assert!(d.is_allow());
    assert_eq!(
        d.updated_input().unwrap(),
        &json!({"file_path": "/tmp/safe.txt"})
    );
}

#[test]
fn deny_decision_carries_reason() {
    let d = PermissionDecision::deny("not today");
    assert!(!d.is_allow());
    assert_eq!(d.reason(), Some("not today"));
}

#[test]
fn context_carries_tool_name_and_input() {
    let ctx = ToolPermissionContext::new("Edit", json!({"file_path": "/tmp/x"}), "toolu_01", None);
    assert_eq!(ctx.tool_name, "Edit");
    assert_eq!(ctx.tool_use_id, "toolu_01");
}
