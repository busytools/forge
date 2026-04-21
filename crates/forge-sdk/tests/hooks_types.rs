//! Unit tests for hook payload types.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::hooks::{HookDecision, HookKind, PreToolUseInput};
use serde_json::json;

#[test]
fn pre_tool_use_input_parse() {
    let raw = json!({
        "tool_name": "Bash",
        "tool_input": {"command": "echo hi"}
    });
    let input: PreToolUseInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.tool_name, "Bash");
}

#[test]
fn hook_decision_allow() {
    let d = HookDecision::allow();
    assert!(d.is_allow());
}

#[test]
fn hook_decision_deny() {
    let d = HookDecision::deny("no go");
    assert!(!d.is_allow());
    assert_eq!(d.reason(), Some("no go"));
}

#[test]
fn hook_decision_replace_input() {
    let d = HookDecision::replace_input(json!({"command": "echo safe"}));
    assert!(d.is_allow());
    assert!(d.updated_input().is_some());
}

#[test]
fn hook_kind_discriminants_cover_all_ten_plus_unknown() {
    assert_eq!(HookKind::PreToolUse.as_str(), "PreToolUse");
    assert_eq!(HookKind::PostToolUse.as_str(), "PostToolUse");
    assert_eq!(HookKind::PostToolUseFailure.as_str(), "PostToolUseFailure");
    assert_eq!(HookKind::UserPromptSubmit.as_str(), "UserPromptSubmit");
    assert_eq!(HookKind::Stop.as_str(), "Stop");
    assert_eq!(HookKind::SubagentStop.as_str(), "SubagentStop");
    assert_eq!(HookKind::SubagentStart.as_str(), "SubagentStart");
    assert_eq!(HookKind::PreCompact.as_str(), "PreCompact");
    assert_eq!(HookKind::Notification.as_str(), "Notification");
    assert_eq!(HookKind::PermissionRequest.as_str(), "PermissionRequest");
    assert_eq!(HookKind::Unknown.as_str(), "Unknown");
}

#[test]
fn hook_kind_from_wire_round_trips() {
    for kind in [
        HookKind::PreToolUse,
        HookKind::PostToolUse,
        HookKind::PostToolUseFailure,
        HookKind::UserPromptSubmit,
        HookKind::Stop,
        HookKind::SubagentStop,
        HookKind::SubagentStart,
        HookKind::PreCompact,
        HookKind::Notification,
        HookKind::PermissionRequest,
    ] {
        assert_eq!(HookKind::from_wire(kind.as_str()), kind);
    }
    assert_eq!(HookKind::from_wire("BrandNewKind"), HookKind::Unknown);
}
