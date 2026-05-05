//! Live-capture scenario: outbound `mcp_clear_auth` `control_request`.
//!
//! Clears stored OAuth credentials for an MCP server. Captured against
//! a non-existent server name so the CLI's "no auth to clear" path is
//! the locked behaviour — actual cleared-credential traces would
//! include user-specific tokens which we don't want in the baseline.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_mcp_clear_auth() {
    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .build();

    run_live_scenario("mcp_clear_auth", opts, |client, events| async move {
        // Use a clearly non-existent server name so the trace is
        // deterministic and free of real OAuth tokens.
        let _ = client
            .mcp_clear_auth("forge-test-harness-nonexistent")
            .await;
        eprintln!("mcp_clear_auth captured");

        client
            .send_user_message("Respond with only the word DONE.")
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
