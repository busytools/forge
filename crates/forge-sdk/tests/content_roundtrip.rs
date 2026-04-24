//! Roundtrip tests for content block serde.
//!
//! The wire shape must match Python SDK exactly — these tests capture real
//! JSON the `claude` binary emits.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::content::ContentBlock;
use serde_json::json;

#[test]
fn text_block_roundtrip() {
    let raw = json!({"type": "text", "text": "hello world"});
    let block: ContentBlock = serde_json::from_value(raw.clone()).expect("parse");
    matches!(block, ContentBlock::Text { .. })
        .then_some(())
        .expect("text variant");
    let re = serde_json::to_value(&block).expect("serialize");
    assert_eq!(raw, re);
}

#[test]
fn thinking_block_roundtrip() {
    let raw = json!({"type": "thinking", "thinking": "let me think...", "signature": "sig-abc"});
    let block: ContentBlock = serde_json::from_value(raw.clone()).expect("parse");
    matches!(block, ContentBlock::Thinking { .. })
        .then_some(())
        .expect("thinking variant");
    let re = serde_json::to_value(&block).expect("serialize");
    assert_eq!(raw, re);
}

#[test]
fn tool_use_block_roundtrip() {
    let raw = json!({
        "type": "tool_use",
        "id": "toolu_01XyZ",
        "name": "Edit",
        "input": {"file_path": "/tmp/foo.rs", "old_string": "a", "new_string": "b"},
    });
    let block: ContentBlock = serde_json::from_value(raw.clone()).expect("parse");
    matches!(block, ContentBlock::ToolUse { .. })
        .then_some(())
        .expect("tool_use variant");
    let re = serde_json::to_value(&block).expect("serialize");
    assert_eq!(raw, re);
}

#[test]
fn tool_result_block_roundtrip() {
    let raw = json!({
        "type": "tool_result",
        "tool_use_id": "toolu_01XyZ",
        "content": "file edited successfully",
        "is_error": false,
    });
    let block: ContentBlock = serde_json::from_value(raw.clone()).expect("parse");
    matches!(block, ContentBlock::ToolResult { .. })
        .then_some(())
        .expect("tool_result variant");
    let re = serde_json::to_value(&block).expect("serialize");
    assert_eq!(raw, re);
}

#[test]
fn unknown_block_type_falls_back_to_unknown_variant() {
    // Forward-compat: unrecognised content block types (e.g. Anthropic
    // API's `document` block for PDF inputs, or any future block) must
    // land in ContentBlock::Unknown instead of erroring — real-session
    // probe uncovered this when a `document` block crashed the parser.
    let raw = json!({"type": "unknown_kind", "data": "..."});
    let block: ContentBlock = serde_json::from_value(raw.clone()).expect("parse");
    let ContentBlock::Unknown {
        type_str,
        raw: echoed,
    } = block
    else {
        panic!("expected Unknown variant");
    };
    assert_eq!(type_str, "unknown_kind");
    assert_eq!(echoed, raw);
}
