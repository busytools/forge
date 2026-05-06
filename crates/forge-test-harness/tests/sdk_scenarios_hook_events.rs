//! Live-capture scenarios: every hook event type the SDK supports.
//!
//! Each hook variant uses the same wire shape (inbound `hook_callback`
//! `control_request`, outbound `control_response`) but distinct
//! `hook_event_name` strings. Separate scenarios per event capture the
//! name-specific payload shapes and give the replay harness a
//! regression guard per hook surface.



#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{
    HookContext, HookDecision, HooksBuilder, NotificationInput, OptionsBuilder, PermissionMode,
    PermissionRequestInput, PostToolUseFailureInput, PostToolUseInput, StopInput,
    SubagentStartInput, SubagentStopInput, UserPromptSubmitInput,
};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_user_prompt_submit_hook() {
    let hooks = HooksBuilder::new()
        .user_prompt_submit(|_input: UserPromptSubmitInput, _ctx: HookContext| async move {
            HookDecision::passthrough()
        })
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .hooks(hooks)
        .build();

    run_live_scenario("user_prompt_submit_hook", opts, |client, events| async move {
        client.send_user_message("Reply with only the word PING.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_post_tool_use_hook() {
    let hooks = HooksBuilder::new()
        .post_tool_use("Bash", |_input: PostToolUseInput, _ctx: HookContext| async move {
            HookDecision::passthrough()
        })
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .hooks(hooks)
        .build();

    run_live_scenario("post_tool_use_hook", opts, |client, events| async move {
        client
            .send_user_message(
                "Run `echo forge-post-hook` with Bash, then reply with exactly what it printed.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_stop_hook() {
    let hooks = HooksBuilder::new()
        .stop(|_input: StopInput, _ctx: HookContext| async move { HookDecision::passthrough() })
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .hooks(hooks)
        .build();

    run_live_scenario("stop_hook", opts, |client, events| async move {
        client.send_user_message("Reply with only the word STOP.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_subagent_stop_hook() {
    let hooks = HooksBuilder::new()
        .subagent_stop(|_input: SubagentStopInput, _ctx: HookContext| async move {
            HookDecision::passthrough()
        })
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(6)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Task".to_string(), "Bash".to_string()])
        .hooks(hooks)
        .build();

    run_live_scenario("subagent_stop_hook", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Task tool with subagent_type=\"general-purpose\" to \
                 run `echo forge-subagent-stop-hook` via Bash and report \
                 the output back.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_post_tool_use_failure_hook() {
    // Fires only when the tool call fails. Run a Bash command that
    // deliberately exits non-zero (`false` / `exit 1`) so the CLI
    // routes to PostToolUseFailure instead of PostToolUse.
    let hooks = HooksBuilder::new()
        .post_tool_use_failure(
            "Bash",
            |_input: PostToolUseFailureInput, _ctx: HookContext| async move {
                HookDecision::passthrough()
            },
        )
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .hooks(hooks)
        .build();

    run_live_scenario("post_tool_use_failure_hook", opts, |client, events| async move {
        client
            .send_user_message(
                "Run `exit 1` with the Bash tool (it will fail), then \
                     just report back that the command failed.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_notification_hook() {
    // Notification hooks fire when the CLI posts a user-visible
    // status message (idle state, waiting on input, etc.). Register a
    // passthrough callback; a bare prompt may not always elicit one,
    // but the trace captures the initialize path with the Notification
    // entry in the hook registry.
    let hooks = HooksBuilder::new()
        .notification(|_input: NotificationInput, _ctx: HookContext| async move {
            HookDecision::passthrough()
        })
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .hooks(hooks)
        .build();

    run_live_scenario("notification_hook", opts, |client, events| async move {
        client.send_user_message("Reply with only the word OK.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_subagent_start_hook() {
    // Fires when the Task tool launches a sub-agent. Combine with a
    // Task dispatch so the hook gets a chance to fire.
    let hooks = HooksBuilder::new()
        .subagent_start(|_input: SubagentStartInput, _ctx: HookContext| async move {
            HookDecision::passthrough()
        })
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(6)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Task".to_string(), "Bash".to_string()])
        .hooks(hooks)
        .build();

    run_live_scenario("subagent_start_hook", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Task tool with subagent_type=\"general-purpose\" to \
                 run `echo forge-subagent-start-hook` via Bash and report \
                 the output back.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_permission_request_hook() {
    // PermissionRequest fires alongside the can_use_tool flow. Same
    // setup as `permission_deny`: Ask mode + permission_prompt_tool_name
    // so the CLI escalates instead of auto-handling, plus a tool not
    // in the developer's auto-allow list.
    let hooks = HooksBuilder::new()
        .permission_request(
            "Write",
            |_input: PermissionRequestInput, _ctx: HookContext| async move {
                HookDecision::passthrough()
            },
        )
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::Ask)
        .extra_arg("permission-mode", Some("default".to_string()))
        .permission_prompt_tool_name("stdio")
        .hooks(hooks)
        .build();

    run_live_scenario("permission_request_hook", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Write tool to create /tmp/forge-perm-hook.txt \
                 containing PING.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
