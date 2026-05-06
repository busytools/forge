//! Live-capture scenario: discover the wire shape of `AskUserQuestion`.
//!
//! Forces Claude to invoke the built-in `AskUserQuestion` tool and
//! captures every `control_request` the CLI emits during the
//! exchange. The `can_use_tool` callback allows the call to proceed
//! so we can observe the full handshake — request shape from the CLI,
//! and what the SDK is expected to send back.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionDecision, PermissionMode, ToolPermissionContext};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_ask_user_question() {
    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::Ask)
        .extra_arg("permission-mode", Some("default".to_string()))
        .permission_prompt_tool_name("stdio")
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            eprintln!(
                "can_use_tool fired for tool={} input={}",
                ctx.tool_name,
                serde_json::to_string(&ctx.tool_input).unwrap_or_default()
            );
            // Allow every tool. For AskUserQuestion specifically the
            // CLI's response handling is what we want to observe.
            PermissionDecision::allow()
        })
        .build();

    run_live_scenario("ask_user_question", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the AskUserQuestion tool right now to ask me whether I prefer the colour red, blue, or green. Single question, three options. Do NOT answer it for me — just call the tool and stop.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
