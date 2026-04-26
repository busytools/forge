//! Live-capture scenario: Bash tool use.
//!
//! Exercises the wire shape for a single Bash invocation. The CLI must
//! emit a `tool_use` block inside an assistant turn plus a `tool_result`
//! user turn — the scenarios covered here verify forge-sdk's decoder
//! handles those frame shapes cleanly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_bash_tool() {
    // Auto mode so the CLI executes the Bash call without invoking a
    // `can_use_tool` callback (which would need its own scenario).
    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .build();

    run_live_scenario("bash_tool", opts, |mut client| async move {
        client
            .send_user_message(
                "Run `echo forge-bash-scenario-ok` using the Bash tool \
                 and then reply with exactly what the command printed.",
            )
            .await?;
        Ok(client)
    })
    .await
    .expect("scenario run");
}
