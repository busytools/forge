//! Live-capture scenario: outbound user-turn with structured content
//! blocks (text + image).
//!
//! Exercises [`Client::send_user_message_with_content`] which emits
//! `{"type":"user","message":{"role":"user","content":[<blocks>]}}`
//! instead of the bare-string form. Captures both the request shape
//! (an inline 1×1 transparent PNG) and the assistant's response.


use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;

/// 1×1 transparent PNG, base64-encoded. Smallest plausible image
/// payload — keeps the captured baseline small and deterministic.
const TINY_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_user_message_blocks() {
    let opts =
        OptionsBuilder::new().max_turns(1).permission_mode(PermissionMode::AcceptEdits).build();

    run_live_scenario("user_message_blocks", opts, |client, events| async move {
        let content = vec![
            serde_json::json!({
                "type": "text",
                "text": "Reply with the single word DONE.",
            }),
            serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": TINY_PNG_BASE64,
                },
            }),
        ];
        client.send_user_message_with_content(&content).await?;
        eprintln!("user_message_blocks captured");
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}
