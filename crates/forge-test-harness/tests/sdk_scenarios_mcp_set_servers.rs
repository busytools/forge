//! Live-capture scenario: outbound `mcp_set_servers` `control_request`.
//!
//! The CLI replaces its current MCP server set with the supplied map.
//! Captured with an empty map to keep the scenario reproducible — the
//! request/response wire shape is what we want to lock down, not the
//! contents.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_mcp_set_servers() {
    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .build();

    run_live_scenario("mcp_set_servers", opts, |client, events| async move {
        // Empty map: no servers. The CLI will accept it and clear any
        // active server set. Keeps the scenario reproducible without
        // depending on installed MCP backends.
        client.mcp_set_servers(serde_json::json!({})).await?;
        eprintln!("mcp_set_servers captured (empty map)");

        // Trail with a trivial turn so the trace ends with a Result
        // frame.
        client
            .send_user_message("Respond with only the word DONE.")
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
