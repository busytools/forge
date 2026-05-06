//! Live-capture scenario: outbound `reload_plugins` `control_request`.
//!
//! The CLI re-scans installed plugins / agents / MCP servers and
//! returns the refreshed inventory. Captures both the request shape
//! (`{subtype: "reload_plugins"}`) and the response payload.


use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_reload_plugins() {
    let opts =
        OptionsBuilder::new().max_turns(1).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("reload_plugins", opts, |client, events| async move {
        // Issue reload_plugins BEFORE a user message — the CLI's
        // control_response arrives quickly and keeps the trace focused
        // on the control round trip.
        let raw = client.reload_plugins().await?;
        eprintln!("reload_plugins captured: {raw:?}");

        // Trail with a trivial turn so the trace ends with a Result
        // frame (harness drain condition).
        client.send_user_message("Respond with only the word DONE.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
