//! Live-capture scenario: outbound `mcp_authenticate` `control_request`.
//!
//! Begins OAuth for an MCP server. Captured against a non-existent
//! server name so the CLI's "unknown server" error path is the locked
//! shape. Real OAuth captures (URL handouts, callback completion)
//! require an installed OAuth-capable MCP backend and human
//! redirection — out of scope for an automated reproducible scenario.



#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_mcp_authenticate() {
    let opts =
        OptionsBuilder::new().max_turns(1).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("mcp_authenticate", opts, |client, events| async move {
        // Non-existent server: locks the error-path wire shape; avoids
        // committing real OAuth URLs / tokens to the baseline.
        let _ = client.mcp_authenticate("forge-test-harness-nonexistent").await;
        eprintln!("mcp_authenticate captured (error-path)");

        client.send_user_message("Respond with only the word DONE.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
