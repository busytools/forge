// =====
// TESTS: 3
// =====
//
// Issue #85 integration tests — wire-side behaviour for the
// `queued_command` content-block walker.
//
// These exercise the path:
//   `SessionUpdate::ChatAppended` carrying a user message with a
//   `queued_command` block → forge's content walker (no pending
//   match, since this test crate can't pre-populate the
//   per-session `pending_echo_bubbles` deque from outside the
//   crate) → push fresh un-dimmed user bubble (replay path).
//
// The live mid-turn un-dim path is covered by the unit tests in
// `crates/forge-tui/src/app/input_submit.rs` (which can poke
// `pub(crate)` internals like `push_message_tracked` +
// `pending_echo_bubbles`).

use forge_primitives::ContentBlock;
use forge_tui::app::MessageRole;
use pretty_assertions::assert_eq;

use crate::helpers::test_app;
use crate::message_helpers::{send_msg, tool_result_block, user_message};

/// Build a `queued_command` content block with a plain-text prompt.
fn queued_command_block(prompt: &str) -> ContentBlock {
    ContentBlock::QueuedCommand {
        prompt: serde_json::Value::String(prompt.to_owned()),
        command_mode: Some("prompt".to_owned()),
        source_uuid: None,
    }
}

#[tokio::test]
async fn queued_command_on_wire_pushes_replay_user_bubble() {
    // Replay path: a session-resume emits a user message with a
    // `queued_command` content block. forge has no pending match
    // (fresh App), so it should push a fresh un-dimmed user bubble.
    let mut app = test_app();
    let before = app.messages().len();

    send_msg(&mut app, user_message(vec![queued_command_block("from-history-replay")]));

    assert_eq!(app.messages().len(), before + 1, "replay bubble pushed");
    let new_msg = app.messages().last().expect("bubble");
    assert!(matches!(new_msg.role, MessageRole::User));
    assert!(!new_msg.queued, "replay bubbles are NOT dimmed");
}

#[tokio::test]
async fn queued_command_alongside_tool_result_renders_both() {
    // Wire shape claude actually emits: tool_result + queued_command
    // bundled in the same user-message content array. forge should
    // process BOTH — the tool_result drives tool-call lifecycle, and
    // the queued_command pushes the user bubble (replay path).
    let mut app = test_app();
    let before = app.messages().len();

    send_msg(
        &mut app,
        user_message(vec![
            tool_result_block("tool-1", serde_json::json!("ok")),
            queued_command_block("interjected steering"),
        ]),
    );

    // The replay-path user bubble is appended.
    assert!(app.messages().len() > before, "queued_command pushed bubble");
    let last = app.messages().last().expect("bubble");
    assert!(matches!(last.role, MessageRole::User));
    assert!(!last.queued);
}

#[tokio::test]
async fn multi_block_queued_command_renders_text_with_image_placeholder() {
    // Wire variant: prompt is a content-block array (multi-modal
    // input). The extractor joins text parts; non-text blocks render
    // as `[image]` / `[document]` placeholders so the user sees
    // something rather than blank.
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
