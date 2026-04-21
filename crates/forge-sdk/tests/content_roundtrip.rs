//! Roundtrip tests for content block serde.
//!
//! The wire shape must match Python SDK exactly — these tests capture real
//! JSON the `claude` binary emits.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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
fn unknown_block_type_rejects_parse() {
    let raw = json!({"type": "unknown_kind", "data": "..."});
    let result: Result<ContentBlock, _> = serde_json::from_value(raw);
    assert!(result.is_err(), "unknown block type should reject parse");
}
