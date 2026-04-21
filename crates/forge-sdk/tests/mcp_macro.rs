//! Tests for the `tool!` declarative macro.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unnecessary_literal_bound
)]

use forge_sdk::mcp::{Tool, ToolInput, ToolOutput, ToolOutputBlock};
use forge_sdk::tool;
use serde_json::json;

tool! {
    name: "double",
    description: "Doubles an integer",
    schema: json!({
        "type": "object",
        "properties": {"n": {"type": "integer"}},
        "required": ["n"]
    }),
    call: |input: ToolInput| async move {
        let n = input.value["n"].as_i64().unwrap_or(0);
        ToolOutput::text((n * 2).to_string())
    },
    tool_type: DoubleTool,
}

#[tokio::test]
async fn macro_generated_tool_works() {
    let t = DoubleTool;
    assert_eq!(t.name(), "double");
    let out = t
        .call(ToolInput {
            value: json!({"n": 7}),
        })
        .await;
    assert!(!out.is_error);
    match &out.blocks[0] {
        ToolOutputBlock::Text { text } => assert_eq!(text, "14"),
    }
}
