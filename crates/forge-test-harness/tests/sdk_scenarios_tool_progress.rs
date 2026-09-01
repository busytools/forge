//! Live-capture scenario: the CLI's `tool_progress` heartbeat.
//!
//! A Bash call that runs past 30 seconds in the foreground makes the
//! CLI emit a `tool_progress` heartbeat on stdout every 30 seconds
//! (`elapsed_time_seconds` counting up) until the tool returns. This
//! scenario holds a single Bash call across that cadence so the
//! baseline carries real heartbeats - forge-sdk decodes the frame and
//! the reader drops it, so without a baseline containing one, replay
//! could never prove the type round-trips.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_tool_progress_heartbeat() {
    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::AcceptEdits)
        .allowed_tools(vec!["Bash".to_string()])
        .build();

    run_live_scenario("tool_progress", opts, |client, events| async move {
        client
            .send_user_message(
                "Run `sleep 65 && echo forge-heartbeat-provoked` using the \
                 Bash tool, in the foreground (do NOT use run_in_background). \
                 When it finishes, reply with exactly what the command \
                 printed.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
