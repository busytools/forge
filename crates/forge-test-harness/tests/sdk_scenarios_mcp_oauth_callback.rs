//! Live-capture scenario: outbound `mcp_oauth_callback_url`
//! `control_request`.
//!
//! Forwards an OAuth callback URL to complete authentication.
//! Captured with a fabricated callback against a non-existent server
//! to lock the error-path wire shape — real captures would embed
//! real tokens which we don't want in baselines.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_mcp_oauth_callback() {
    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .build();

    run_live_scenario("mcp_oauth_callback", opts, |client, events| async move {
        // Fabricated server + callback URL: locks the error-path wire
        // shape; avoids committing real OAuth state to the baseline.
        let _ = client
            .mcp_oauth_callback_url(
                "forge-test-harness-nonexistent",
                "https://forge.example/oauth/callback?code=test&state=test",
            )
            .await;
        eprintln!("mcp_oauth_callback_url captured (error-path)");

        client
            .send_user_message("Respond with only the word DONE.")
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
