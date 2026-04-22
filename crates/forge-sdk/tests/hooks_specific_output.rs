//! Round-trip tests for typed `hookSpecificOutput` wrappers.
//!
//! Ported from claude-agent-sdk-python v0.1.64 `types.py:369-438`.
//! Every hook event has a `*HookSpecificOutput` `TypedDict` upstream with a
//! fixed `hookEventName` discriminator plus event-specific optional fields.
//! These structs replace forge-sdk's prior practice of building the
//! `hookSpecificOutput` JSON object inline inside `handle_hook_callback`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{
    HookSpecificOutput, NotificationHookSpecificOutput, PermissionRequestHookSpecificOutput,
    PostToolUseFailureHookSpecificOutput, PostToolUseHookSpecificOutput,
    PreToolUseHookSpecificOutput, PreToolUsePermissionDecision, SessionStartHookSpecificOutput,
    SubagentStartHookSpecificOutput, UserPromptSubmitHookSpecificOutput,
};
use serde_json::json;

#[test]
fn pre_tool_use_hook_specific_output_serialises_with_updated_input() {
    let out = PreToolUseHookSpecificOutput {
        permission_decision: Some(PreToolUsePermissionDecision::Allow),
        permission_decision_reason: Some("matched allowlist".into()),
        updated_input: Some(json!({"command": "echo safe"})),
        additional_context: None,
        ..Default::default()
    };
    let v = serde_json::to_value(&out).expect("serialise");
    assert_eq!(v["hookEventName"], "PreToolUse");
    assert_eq!(v["permissionDecision"], "allow");
    assert_eq!(v["permissionDecisionReason"], "matched allowlist");
    assert_eq!(v["updatedInput"], json!({"command": "echo safe"}));
    assert!(
        v.get("additionalContext").is_none(),
        "None-valued fields must not serialise"
    );
}

#[test]
fn pre_tool_use_hook_specific_output_permission_decision_enum_encodings() {
    for (variant, wire) in [
        (PreToolUsePermissionDecision::Allow, "allow"),
        (PreToolUsePermissionDecision::Deny, "deny"),
        (PreToolUsePermissionDecision::Ask, "ask"),
    ] {
        let out = PreToolUseHookSpecificOutput {
            permission_decision: Some(variant),
            ..Default::default()
        };
        let v = serde_json::to_value(&out).expect("serialise");
        assert_eq!(v["permissionDecision"], wire);
    }
}

#[test]
fn post_tool_use_hook_specific_output_round_trips() {
    let out = PostToolUseHookSpecificOutput {
        additional_context: Some("ran ok".into()),
        updated_mcp_tool_output: Some(json!({"stdout": "rewritten"})),
        ..Default::default()
    };
    let v = serde_json::to_value(&out).expect("serialise");
    assert_eq!(v["hookEventName"], "PostToolUse");
    assert_eq!(v["additionalContext"], "ran ok");
    assert_eq!(v["updatedMCPToolOutput"], json!({"stdout": "rewritten"}));
}

#[test]
fn post_tool_use_failure_hook_specific_output_minimal() {
    let out = PostToolUseFailureHookSpecificOutput {
        additional_context: None,
        ..Default::default()
    };
    let v = serde_json::to_value(&out).expect("serialise");
    assert_eq!(
        v,
        json!({"hookEventName": "PostToolUseFailure"}),
        "bare form should only carry the discriminator"
    );
}

#[test]
fn user_prompt_submit_hook_specific_output_round_trips() {
    let out = UserPromptSubmitHookSpecificOutput {
        additional_context: Some("context to inject".into()),
        ..Default::default()
    };
    let v = serde_json::to_value(&out).expect("serialise");
    assert_eq!(v["hookEventName"], "UserPromptSubmit");
    assert_eq!(v["additionalContext"], "context to inject");
}

#[test]
fn session_start_hook_specific_output_discriminator_only() {
    let out = SessionStartHookSpecificOutput {
        additional_context: None,
        ..Default::default()
    };
    let v = serde_json::to_value(&out).expect("serialise");
    assert_eq!(v, json!({"hookEventName": "SessionStart"}));
}

#[test]
fn notification_hook_specific_output_round_trips() {
    let out = NotificationHookSpecificOutput {
        additional_context: Some("heads-up".into()),
        ..Default::default()
    };
    let v = serde_json::to_value(&out).expect("serialise");
    assert_eq!(v["hookEventName"], "Notification");
    assert_eq!(v["additionalContext"], "heads-up");
}

#[test]
fn subagent_start_hook_specific_output_round_trips() {
    let out = SubagentStartHookSpecificOutput {
        additional_context: Some("agent starting".into()),
        ..Default::default()
    };
    let v = serde_json::to_value(&out).expect("serialise");
    assert_eq!(v["hookEventName"], "SubagentStart");
    assert_eq!(v["additionalContext"], "agent starting");
}

#[test]
fn permission_request_hook_specific_output_carries_decision() {
    let out = PermissionRequestHookSpecificOutput {
        decision: json!({"behavior": "allow"}),
        ..Default::default()
    };
    let v = serde_json::to_value(&out).expect("serialise");
    assert_eq!(v["hookEventName"], "PermissionRequest");
    assert_eq!(v["decision"], json!({"behavior": "allow"}));
}

#[test]
fn hook_specific_output_enum_tags_by_event_name() {
    let pre = HookSpecificOutput::PreToolUse(PreToolUseHookSpecificOutput {
        permission_decision: Some(PreToolUsePermissionDecision::Deny),
        permission_decision_reason: Some("nope".into()),
        updated_input: None,
        additional_context: None,
        ..Default::default()
    });
    let v = serde_json::to_value(&pre).expect("serialise");
    // The untagged enum forwards to the inner struct's own discriminator.
    assert_eq!(v["hookEventName"], "PreToolUse");
    assert_eq!(v["permissionDecision"], "deny");

    let noti = HookSpecificOutput::Notification(NotificationHookSpecificOutput {
        additional_context: None,
        ..Default::default()
    });
    assert_eq!(
        serde_json::to_value(&noti).expect("serialise")["hookEventName"],
        "Notification"
    );
}
