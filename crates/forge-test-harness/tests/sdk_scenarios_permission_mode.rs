//! Live-capture scenario: `set_permission_mode` outbound `control_request`.
//!
//! Exercises runtime mutation of the CLI's permission mode via the
//! `set_permission_mode` `control_request`. Captures an outbound
//! `control_request` + inbound `control_response` round trip for a
//! non-trivial subtype.



#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_set_permission_mode() {
    let opts =
        OptionsBuilder::new().max_turns(1).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("set_permission_mode", opts, |client, events| async move {
        // Swap permission mode mid-session. `BypassPermissions` is
        // rejected unless the session launched with
        // `--dangerously-skip-permissions`, so flip to `Plan` instead
        // (both accepted in any session).
        client.set_permission_mode(PermissionMode::Plan).await?;

        client.send_user_message("Respond with only the word DONE.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
