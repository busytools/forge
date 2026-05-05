//! Live-capture scenario: outbound `get_context_usage` `control_request`.
//!
//! Exercises another outbound `control_request` subtype
//! (`get_context_usage`) alongside the full conversation path. Useful
//! for validating that forge-sdk's token/budget query path decodes its
//! `control_response` body cleanly against the real CLI.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_context_usage() {
    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .build();

    run_live_scenario("context_usage", opts, |client, mut events| async move {
        client
            .send_user_message("Reply with exactly the word OK.")
            .await?;

        // Drain until the first Result so the context-usage query runs
        // against a session that actually has usage to report.
        // Drain until Result.
        loop {
            match events.recv().await {
                Some(Ok(forge_sdk::Message::Result { .. })) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
            }
        }

        let usage = client.get_context_usage().await?;
        eprintln!(
            "context_usage: total={} max={} pct={:.1}",
            usage.total_tokens, usage.max_tokens, usage.percentage,
        );

        // The conversation already Result'd above — sending another
        // user message would kick off a second turn. Instead, hand the
        // client back to the harness which will close stdin and drain
        // to EOF. Without this the harness would hang waiting for
        // another Result that never arrives.
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
