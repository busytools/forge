//! Unit tests for hook payload types.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::hooks::{
    BaseHookInput, HookDecision, HookKind, NotificationInput, PermissionRequestInput,
    PostToolUseFailureInput, PostToolUseInput, PreCompactInput, PreToolUseInput, StopInput,
    SubagentContext, SubagentStartInput, SubagentStopInput, UserPromptSubmitInput,
};
use serde_json::json;

/// Shared `BaseHookInput` fields the CLI emits on every hook input.
///
/// Ported from claude-agent-sdk-python v0.1.64 `src/claude_agent_sdk/types.py:231-237`.
#[test]
fn base_hook_input_parses_required_fields() {
    let raw = json!({
        "session_id": "sess-123",
        "transcript_path": "/tmp/t.jsonl",
        "cwd": "/home/u",
        "permission_mode": "default"
    });
    let base: BaseHookInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(base.session_id, "sess-123");
    assert_eq!(base.transcript_path, "/tmp/t.jsonl");
    assert_eq!(base.cwd, "/home/u");
    assert_eq!(base.permission_mode.as_deref(), Some("default"));
}

/// `permission_mode` is `NotRequired` upstream — must accept frames that omit it.
#[test]
fn base_hook_input_permission_mode_optional() {
    let raw = json!({
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c"
    });
    let base: BaseHookInput = serde_json::from_value(raw).expect("parse");
    assert!(base.permission_mode.is_none());
}

#[test]
fn pre_tool_use_input_carries_base_and_subagent_fields() {
    let raw = json!({
        "hook_event_name": "PreToolUse",
        "session_id": "sess",
        "transcript_path": "/t",
        "cwd": "/c",
        "permission_mode": "default",
        "agent_id": "agent-1",
        "agent_type": "general-purpose",
        "tool_name": "Bash",
        "tool_input": {"command": "echo hi"},
        "tool_use_id": "toolu_1"
    });
    let input: PreToolUseInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.base.session_id, "sess");
    assert_eq!(input.base.transcript_path, "/t");
    assert_eq!(input.base.cwd, "/c");
    assert_eq!(input.subagent.agent_id.as_deref(), Some("agent-1"));
    assert_eq!(
        input.subagent.agent_type.as_deref(),
        Some("general-purpose")
    );
    assert_eq!(input.tool_name, "Bash");
    assert_eq!(input.tool_use_id, "toolu_1");
}

#[test]
fn pre_tool_use_input_subagent_fields_optional() {
    // Main-thread hook fires with no agent_id/agent_type.
    let raw = json!({
        "hook_event_name": "PreToolUse",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
        "tool_use_id": "toolu_1"
    });
    let input: PreToolUseInput = serde_json::from_value(raw).expect("parse");
    assert!(input.subagent.agent_id.is_none());
    assert!(input.subagent.agent_type.is_none());
}

#[test]
fn post_tool_use_input_carries_tool_response_and_tool_use_id() {
    let raw = json!({
        "hook_event_name": "PostToolUse",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
        "tool_response": {"stdout": "a\nb\n"},
        "tool_use_id": "toolu_9"
    });
    let input: PostToolUseInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.tool_name, "Bash");
    assert_eq!(input.tool_use_id, "toolu_9");
    assert!(input.tool_response.get("stdout").is_some());
}

#[test]
fn user_prompt_submit_input_carries_base_and_prompt() {
    let raw = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "prompt": "hi there"
    });
    let input: UserPromptSubmitInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.prompt, "hi there");
    assert_eq!(input.base.session_id, "s");
}

#[test]
fn stop_input_has_stop_hook_active_not_num_turns() {
    // Upstream field is `stop_hook_active: bool`. forge-sdk previously modelled
    // `num_turns: u64`, which would drop on valid CLI frames.
    let raw = json!({
        "hook_event_name": "Stop",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "stop_hook_active": true
    });
    let input: StopInput = serde_json::from_value(raw).expect("parse");
    assert!(input.stop_hook_active);
}

#[test]
fn subagent_stop_input_carries_agent_metadata() {
    let raw = json!({
        "hook_event_name": "SubagentStop",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "stop_hook_active": false,
        "agent_id": "agent-1",
        "agent_transcript_path": "/tmp/agent-1.jsonl",
        "agent_type": "code-reviewer"
    });
    let input: SubagentStopInput = serde_json::from_value(raw).expect("parse");
    assert!(!input.stop_hook_active);
    assert_eq!(input.agent_id, "agent-1");
    assert_eq!(input.agent_transcript_path, "/tmp/agent-1.jsonl");
    assert_eq!(input.agent_type, "code-reviewer");
}

#[test]
fn pre_compact_input_has_trigger_and_custom_instructions() {
    let raw = json!({
        "hook_event_name": "PreCompact",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "trigger": "auto",
        "custom_instructions": null
    });
    let input: PreCompactInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.trigger, "auto");
    assert!(input.custom_instructions.is_none());
}

#[test]
fn post_tool_use_failure_input_carries_error_field() {
    let raw = json!({
        "hook_event_name": "PostToolUseFailure",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "tool_name": "Bash",
        "tool_input": {"command": "false"},
        "tool_use_id": "toolu_x",
        "error": "exit 1",
        "is_interrupt": false
    });
    let input: PostToolUseFailureInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.error, "exit 1");
    assert_eq!(input.is_interrupt, Some(false));
    assert_eq!(input.tool_use_id, "toolu_x");
}

#[test]
fn post_tool_use_failure_input_is_interrupt_optional() {
    // `is_interrupt` is NotRequired upstream — absent frame must parse.
    let raw = json!({
        "hook_event_name": "PostToolUseFailure",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "tool_name": "Bash",
        "tool_input": {"command": "false"},
        "tool_use_id": "toolu_x",
        "error": "boom"
    });
    let input: PostToolUseFailureInput = serde_json::from_value(raw).expect("parse");
    assert!(input.is_interrupt.is_none());
}

#[test]
fn notification_input_parses() {
    let raw = json!({
        "hook_event_name": "Notification",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "message": "Tool needs approval",
        "title": "Permission",
        "notification_type": "permission_request"
    });
    let input: NotificationInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.message, "Tool needs approval");
    assert_eq!(input.title.as_deref(), Some("Permission"));
    assert_eq!(input.notification_type, "permission_request");
}

#[test]
fn subagent_start_input_parses() {
    let raw = json!({
        "hook_event_name": "SubagentStart",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "agent_id": "agent-1",
        "agent_type": "code-reviewer"
    });
    let input: SubagentStartInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.agent_id, "agent-1");
    assert_eq!(input.agent_type, "code-reviewer");
}

#[test]
fn permission_request_input_parses() {
    let raw = json!({
        "hook_event_name": "PermissionRequest",
        "session_id": "s",
        "transcript_path": "/t",
        "cwd": "/c",
        "tool_name": "Bash",
        "tool_input": {"command": "rm -rf /"},
        "permission_suggestions": [{"behavior":"ask"}]
    });
    let input: PermissionRequestInput = serde_json::from_value(raw).expect("parse");
    assert_eq!(input.tool_name, "Bash");
    assert_eq!(input.permission_suggestions.as_ref().map(Vec::len), Some(1));
}

#[test]
fn subagent_context_round_trips() {
    let ctx = SubagentContext {
        agent_id: Some("a".into()),
        agent_type: Some("t".into()),
    };
    let v = serde_json::to_value(&ctx).expect("serialise");
    assert_eq!(v, json!({"agent_id": "a", "agent_type": "t"}));
    // None fields must not be emitted so flatten stays minimal.
    let empty = SubagentContext::default();
    let v2 = serde_json::to_value(empty).expect("serialise");
    assert_eq!(v2, json!({}));
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
