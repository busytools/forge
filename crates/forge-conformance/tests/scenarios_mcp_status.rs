//! Live-capture scenario: outbound `mcp_status` `control_request`.
//!
//! Exercises the non-initialize outbound `control_request` path. After
//! spawn completes (initialize handshake done), the scenario issues
//! `mcp_status` — the CLI responds with its server-connection
//! snapshot. This is the simplest non-initialize `control_request` we can
//! probe cheaply.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_conformance::run_live_scenario;
use forge_sdk::{OptionsBuilder, PermissionMode};

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_mcp_status() {
    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .build();

    run_live_scenario("mcp_status", opts, |mut client| async move {
        // Call mcp_status BEFORE sending a user message so the CLI's
        // control_response arrives before any conversation frames —
        // keeps the trace focused on the control round trip.
        let status = client.mcp_status().await?;
        eprintln!("mcp_status captured {} servers", status.mcp_servers.len());

        // Drive a trivial conversation afterwards so the trace ends
        // with a Result frame (harness drain condition).
        client
            .send_user_message("Respond with only the word DONE.")
            .await?;
        Ok(client)
    })
    .await
    .expect("scenario run");
}
