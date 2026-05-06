//! Live-capture scenario: two user messages across one spawn.
//!
//! Verifies wire shape for session continuity — the `session_id` must be
//! stable across turns, CLI processes the second user message without
//! needing a re-initialize, and the harness sees two `Result` frames.



#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_multi_turn() {
    let opts =
        OptionsBuilder::new().max_turns(4).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("multi_turn", opts, |client, mut events| async move {
        // Turn 1 — drain the Result in-scenario so the harness's
        // main drain picks up turn 2's Result instead.
        client.send_user_message("Reply with the single word: PINE").await?;
        // Drain until Result.
        loop {
            match events.recv().await {
                Some(Ok(forge_sdk::Message::Result { .. })) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
            }
        }

        // Turn 2 — harness drains this until Result.
        client.send_user_message("Now repeat the word you just said, in lowercase.").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
