//! Live-capture scenarios: compaction — `pre_compact` hook +
//! compaction lifecycle frames.
//!
//! Drives the `/compact` slash command as a user message. When
//! successful, the CLI emits a `compact_boundary` user-message chunk
//! and may call registered `PreCompact` hooks first.

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
        .max_turns(4)
        .permission_mode(PermissionMode::AcceptEdits)
        .hooks(hooks)
        .build();

    run_live_scenario("compact", opts, |client, mut events| async move {
        // First, a real turn so there's something to compact.
        client.send_user_message("Reply with only the word ALPHA.").await?;
        // Drain until Result.
        loop {
            match events.recv().await {
                Some(Ok(forge_sdk::Message::Result { .. })) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
            }
        }

        // Issue `/compact` — the CLI treats slash commands as normal
        // stream-json user messages whose content starts with `/`.
        client.send_user_message("/compact").await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
