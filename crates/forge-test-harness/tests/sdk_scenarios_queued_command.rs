//! Live-capture scenario: `queued_command` content-block bundling.
//!
//! When the user dispatches `Command::Prompt` while a turn is in
//! flight, claude internally queues the input and packages it as a
//! `queued_command` content block on the next outbound user-message
//! envelope going to the model. See `gO6` in the CLI binary's
//! bundled JS, and issue #85 for the forge-side handling.
//!
//! Mechanism:
//! 1. Start a turn that triggers a tool call (Bash echo).
//! 2. Immediately dispatch a second user message via the SDK's
//!    `send_user_message` while claude is mid-stream.
//! 3. Claude buffers the second input internally, then bundles it
//!    as a `queued_command` block alongside the next `tool_result`
//!    user-message envelope to the model.
//! 4. Capture the wire trace — the `queued_command` content block
//!    is the artifact this scenario exists to record.
//!
//! Empirical note (2026-05-13): claude's wire shape for the
//! `queued_command` attachment is `{type, prompt, commandMode}` —
//! no `source_uuid` correlation id. Forge's content-block walker
//! matches by exact prompt text + FIFO ordering.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_queued_command() {
    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .build();

    run_live_scenario("queued_command", opts, |client, events| async move {
        // First user message: ask claude to run a Bash echo (gives
        // us a `tool_use → tool_result` boundary for the queued
        // command to attach to).
        client
            .send_user_message(
                "Run `sleep 1 && echo forge-queued-command-scenario` with the Bash \
                 tool and report what the command printed.",
            )
            .await?;

        // Brief pause so the first turn is mid-stream when we
        // dispatch the second prompt. The CLI then has a definite
        // in-flight turn to bundle our second input as a
        // `queued_command` attachment.
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Dispatch a second user message WHILE the first turn is
        // still running. Claude queues this and bundles it as a
        // `queued_command` block on the next outbound user-message
        // envelope.
        client.send_user_message("Also, briefly summarise the output in one sentence.").await?;

        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
