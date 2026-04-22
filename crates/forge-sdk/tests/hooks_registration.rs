//! Verify hooks can be registered and counted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{HookContext, HookDecision, HooksBuilder, OptionsBuilder, PreToolUseInput};
use forge_sdk::{
    NotificationInput, PermissionRequestInput, PostToolUseFailureInput, SubagentStartInput,
};

#[test]
fn hooks_attach_to_options() {
    let hooks =
        HooksBuilder::new()
            .pre_tool_use(
                "*",
                |_input: PreToolUseInput, _ctx: HookContext| async move { HookDecision::allow() },
            )
            .pre_tool_use(
                "Bash",
                |_input: PreToolUseInput, _ctx: HookContext| async move {
                    HookDecision::deny("no bash")
                },
            )
            .build();

    let opts = OptionsBuilder::new().hooks(hooks).build();
    let desc = format!("{opts:?}");
    assert!(
        desc.contains("pre_tool_use_count: 2"),
        "expected pre_tool_use_count: 2, got: {desc}"
    );
}

#[test]
fn all_ten_hook_kinds_register_and_count() {
    // Confirm the builder exposes a registration method for every HookKind
    // variant the CLI emits, and that each ends up in the initialize payload.
    let hooks = HooksBuilder::new()
        .pre_tool_use("Bash", |_i: PreToolUseInput, _c| async move {
            HookDecision::allow()
        })
        .post_tool_use("Bash", |_i: forge_sdk::PostToolUseInput, _c| async move {
            HookDecision::passthrough()
        })
        .post_tool_use_failure("Bash", |_i: PostToolUseFailureInput, _c| async move {
            HookDecision::passthrough()
        })
        .user_prompt_submit(|_i: forge_sdk::UserPromptSubmitInput, _c| async move {
            HookDecision::allow()
        })
        .stop(|_i: forge_sdk::StopInput, _c| async move { HookDecision::passthrough() })
        .subagent_stop(
            |_i: forge_sdk::SubagentStopInput, _c| async move { HookDecision::passthrough() },
        )
        .subagent_start(|_i: SubagentStartInput, _c| async move { HookDecision::passthrough() })
        .pre_compact(
            |_i: forge_sdk::PreCompactInput, _c| async move { HookDecision::passthrough() },
        )
        .notification(|_i: NotificationInput, _c| async move { HookDecision::passthrough() })
        .permission_request("*", |_i: PermissionRequestInput, _c| async move {
            HookDecision::passthrough()
        })
        .build();

    let desc = format!("{hooks:?}");
    // All counts should be 1.
    for field in [
        "pre_tool_use_count: 1",
        "post_tool_use_count: 1",
        "post_tool_use_failure_count: 1",
        "user_prompt_submit_count: 1",
        "stop_count: 1",
        "subagent_stop_count: 1",
        "subagent_start_count: 1",
        "pre_compact_count: 1",
        "notification_count: 1",
        "permission_request_count: 1",
    ] {
        assert!(desc.contains(field), "missing {field} in debug: {desc}");
    }
}

#[test]
fn initialize_payload_lists_every_registered_kind() {
    // The initialize control_request's hooks payload groups by event name.
    // Every registered kind should appear as a key.
    let hooks = HooksBuilder::new()
        .pre_tool_use("Bash", |_i: PreToolUseInput, _c| async move {
            HookDecision::allow()
        })
        .post_tool_use_failure("*", |_i: PostToolUseFailureInput, _c| async move {
            HookDecision::passthrough()
        })
        .subagent_start(|_i: SubagentStartInput, _c| async move { HookDecision::passthrough() })
        .notification(|_i: NotificationInput, _c| async move { HookDecision::passthrough() })
        .permission_request("Bash", |_i: PermissionRequestInput, _c| async move {
            HookDecision::passthrough()
        })
        .build();

    let payload = hooks.to_initialize_payload_for_test();
    for key in [
        "PreToolUse",
        "PostToolUseFailure",
        "SubagentStart",
        "Notification",
        "PermissionRequest",
    ] {
        assert!(
            payload.get(key).is_some(),
            "expected `{key}` in initialize payload: {payload}"
        );
    }
}
