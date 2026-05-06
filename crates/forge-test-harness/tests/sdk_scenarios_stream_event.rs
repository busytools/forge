//! Live-capture scenario: `stream_event` message with
//! `include_partial_messages(true)`.
//!
//! The CLI emits `stream_event` frames (Anthropic-API streaming chunks)
//! only when started with `--include-partial-messages`. This scenario
//! turns that on and drives a prompt whose response is long enough to
//! produce at least one `stream_event`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_stream_event() {
    let opts = OptionsBuilder::new()
        .max_turns(1)
        .permission_mode(PermissionMode::AcceptEdits)
        .include_partial_messages(true)
        .build();

    run_live_scenario("stream_event", opts, |client, events| async move {
        client
            .send_user_message(
                "Count from 1 to 20 slowly, one number per line, \
                 with a word of description for each number.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
