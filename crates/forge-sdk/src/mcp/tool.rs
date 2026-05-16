//! The `Tool` trait and its I/O types.

use serde_json::Value;

/// Structured input handed to a tool's `call`.
#[derive(Debug, Clone)]
pub struct ToolInput {
    /// The arguments object (already validated against the tool's schema).
    pub value: Value,
}

/// Output a tool returns.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Content blocks — matches MCP `tools/call` response shape.
    pub blocks: Vec<ToolOutputBlock>,
    /// Whether this represents a tool failure.
    pub is_error: bool,
}

impl ToolOutput {
    /// Build a text-only successful output.
    pub fn text(s: impl Into<String>) -> Self {
        Self { blocks: vec![ToolOutputBlock::Text { text: s.into() }], is_error: false }
    }

    /// Serialise to the JSON shape MCP expects.
    pub(crate) fn to_mcp_content(&self) -> Vec<Value> {
        self.blocks
            .iter()
            .map(|b| match b {
                ToolOutputBlock::Text { text } => serde_json::json!({
                    "type": "text",
                    "text": text,
                }),
            })
            .collect()
    }
}

/// Output block kinds. Currently only text; MCP supports images and
/// resources which we can add later if the binary ever asks for them.
#[derive(Debug, Clone)]
pub enum ToolOutputBlock {
    /// Plain text content.
    Text {
        /// The text payload.
        text: String,
    },
}

/// A registered MCP tool. Implementations describe themselves (name,
/// description, schema) and implement the async `call` method that
/// produces an output for the given input.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Tool name. Exposed to the model as `mcp__<server>__<tool>`.
    fn name(&self) -> &str;

    /// One-line description.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's arguments.
    fn input_schema(&self) -> Value;

    /// Execute the tool.
    async fn call(&self, input: ToolInput) -> ToolOutput;
}
