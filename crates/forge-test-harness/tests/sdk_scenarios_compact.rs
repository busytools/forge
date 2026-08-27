//! Live-capture scenarios: compaction - `pre_compact` hook +
//! compaction lifecycle frames.
//!
//! Drives the `/compact` slash command as a user message. When
//! successful, the CLI emits a `compact_boundary` user-message chunk
//! and may call registered `PreCompact` hooks first.
//!
//! The conversation ahead of `/compact` is deliberately several turns
//! long. A single exchange gets refused with "Not enough messages to
//! compact", and a capture of that refusal replays clean forever while
//! covering nothing - the boundary frame this scenario exists for is
//! only emitted when the compaction actually runs.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_sdk::{
    HookContext, HookDecision, HooksBuilder, OptionsBuilder, PermissionMode, PreCompactInput,
};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_compact() {
    let hooks = HooksBuilder::new()
        .pre_compact(|_input: PreCompactInput, _ctx: HookContext| async move {
            HookDecision::passthrough()
        })
        .build();

    let opts = OptionsBuilder::new()
        .max_turns(12)
        .permission_mode(PermissionMode::AcceptEdits)
        .hooks(hooks)
        .build();

    run_live_scenario("compact", opts, |client, mut events| async move {
        // Enough history that the CLI accepts the compaction. One
        // exchange is refused, and a refusal captures no boundary
        // frame. Single-word answers keep the token cost down while
        // still growing the transcript.
        for word in ["ALPHA", "BRAVO", "CHARLIE", "DELTA", "ECHO", "FOXTROT"] {
            client.send_user_message(&format!("Reply with only the word {word}.")).await?;
            loop {
                match events.recv().await {
                    Some(Ok(forge_primitives::Message::Result { .. })) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e),
                }
            }
        }

        // Issue `/compact` - the CLI treats slash commands as normal
        // stream-json user messages whose content starts with `/`.
        client.send_user_message("/compact").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
