//! Live-capture scenarios: every hook event type the SDK supports.
//!
//! Each hook variant uses the same wire shape (inbound `hook_callback`
//! control_request, outbound `control_response`) but distinct
//! `hook_event_name` strings. Separate scenarios per event capture the
//! name-specific payload shapes and give the replay harness a
//! regression guard per hook surface.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_conformance::run_live_scenario;
use forge_sdk::{
    HookContext, HookDecision, HooksBuilder, OptionsBuilder, PermissionMode, PostToolUseInput,
    StopInput, SubagentStopInput, UserPromptSubmitInput,
};

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_user_prompt_submit_hook() {
    let hooks = HooksBuilder::new()
        .user_prompt_submit(
            |_input: UserPromptSubmitInput, _ctx: HookContext| async move {
                HookDecision::passthrough()
            },
        )
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .hooks(hooks)
        .build();

    run_live_scenario("user_prompt_submit_hook", opts, |mut client| async move {
        client
            .send_user_message("Reply with only the word PING.")
            .await?;
        Ok(client)
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_post_tool_use_hook() {
    let hooks = HooksBuilder::new()
        .post_tool_use(
            "Bash",
            |_input: PostToolUseInput, _ctx: HookContext| async move {
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

    run_live_scenario("post_tool_use_hook", opts, |mut client| async move {
        client
            .send_user_message(
                "Run `echo forge-post-hook` with Bash, then reply with exactly what it printed.",
            )
            .await?;
        Ok(client)
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

    run_live_scenario("stop_hook", opts, |mut client| async move {
        client
            .send_user_message("Reply with only the word STOP.")
            .await?;
        Ok(client)
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

    run_live_scenario("subagent_stop_hook", opts, |mut client| async move {
        client
            .send_user_message(
                "Use the Task tool with subagent_type=\"general-purpose\" to \
                 run `echo forge-subagent-stop-hook` via Bash and report \
                 the output back.",
            )
            .await?;
        Ok(client)
    })
    .await
    .expect("scenario run");
}
