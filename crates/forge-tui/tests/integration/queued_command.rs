// =====
// TESTS: 2
// =====
//
// Issue #85 - replay-path coverage for the `queued_command` content
// walker.
//
// IMPORTANT (verified by live capture 2026-05-13): claude does NOT
// emit `queued_command` or `attachment` envelopes on its stream-json
// stdout. It writes them only to the session JSONL on disk. The
// content-block walker exercised here therefore fires *only* during
// session resume, after the catalog/scan layer
// (`forge_agent::userdata::catalog::scan`) hoists a JSONL
// `type:"attachment"` row into a synthetic user envelope carrying a
// single `queued_command` content block.
//
// These tests cover that replay path. The live mid-turn un-dim is
// driven by `SessionUpdate::TurnComplete` (no wire echo exists)  -
// see the `un_dim_pending_on_turn_complete_*` unit tests in
// `crates/forge-tui/src/app/input_submit.rs`.

use forge_primitives::ContentBlock;
use forge_tui::app::MessageRole;
use pretty_assertions::assert_eq;

use crate::helpers::test_app;
use crate::message_helpers::{send_msg, user_message};

/// Build a `queued_command` content block with a plain-text prompt.
fn queued_command_block(prompt: &str) -> ContentBlock {
    ContentBlock::QueuedCommand {
        prompt: serde_json::Value::String(prompt.to_owned()),
        command_mode: Some("prompt".to_owned()),
        source_uuid: None,
    }
}

#[tokio::test]
async fn replay_synthesised_user_envelope_pushes_un_dimmed_bubble() {
    // Replay path: catalog/scan hoists a JSONL attachment row into a
    // user envelope carrying one `queued_command` content block. The
    // walker has no pending match (fresh App), so it pushes a fresh
    // un-dimmed user bubble.
    let mut app = test_app();
    let before = app.messages().len();

    send_msg(&mut app, user_message(vec![queued_command_block("from-history-replay")]));

    assert_eq!(app.messages().len(), before + 1, "replay bubble pushed");
    let new_msg = app.messages().last().expect("bubble");
    assert!(matches!(new_msg.role, MessageRole::User));
    // Replayed bubbles are plain user bubbles - no dim/queued state.
}

#[tokio::test]
async fn replay_multi_block_prompt_renders_text_with_image_placeholder() {
    // Replay variant: the persisted `prompt` field is a content-block
    // array (multi-modal input). The extractor joins text parts;
    // non-text blocks render as `[image]` / `[document]` placeholders
    // so the user sees something rather than blank.
    let mut app = test_app();
    let before = app.messages().len();

    let multi_prompt = serde_json::json!([
        {"type": "text", "text": "look at this"},
        {"type": "image", "source": {"type": "base64", "data": "..."}},
    ]);

    send_msg(
        &mut app,
        user_message(vec![ContentBlock::QueuedCommand {
            prompt: multi_prompt,
            command_mode: Some("prompt".to_owned()),
            source_uuid: None,
        }]),
    );

    assert_eq!(app.messages().len(), before + 1);
    let new_msg = app.messages().last().expect("bubble");
    let rendered: String = new_msg
        .blocks
        .iter()
        .filter_map(|b| {
            if let forge_tui::app::MessageBlock::Text(tb) = b {
                Some(tb.text.clone())
            } else {
                None
            }
        })
        .collect();
    assert!(rendered.contains("look at this"), "text part rendered");
    assert!(rendered.contains("[image]"), "image placeholder rendered");
}
