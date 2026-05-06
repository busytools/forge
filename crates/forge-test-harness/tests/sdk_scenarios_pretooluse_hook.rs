//! Live-capture scenario: `PreToolUse` hook callback.
//!
//! Registers a callback that permits Bash tool invocations and tags a
//! marker in the response path. Exercises the `hook_callback`
//! `control_request` → SDK handler → outbound `control_response` round
//! trip on the wire.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{
    HookContext, HookDecision, HooksBuilder, OptionsBuilder, PermissionMode, PreToolUseInput,
};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_pretooluse_hook() {
    let hooks = HooksBuilder::new()
        .pre_tool_use(
            "Bash",
            // Pass-through allow: we want the CLI to emit the hook_callback
            // frame and see the SDK write a `control_response` with the
            // "allow" decision. The tool still runs.
            |_input: PreToolUseInput, _ctx: HookContext| async move { HookDecision::passthrough() },
        )
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .hooks(hooks)
        .build();

    run_live_scenario("pretooluse_hook", opts, |client, events| async move {
        client
            .send_user_message(
                "Run `echo forge-hook-scenario` with the Bash tool and \
                 then reply with exactly what it printed.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
