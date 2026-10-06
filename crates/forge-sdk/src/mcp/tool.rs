//! The `Tool` trait and its I/O types.

use base64::Engine as _;
use serde_json::Value;

/// Structured input handed to a tool's `call`.
#[derive(Debug, Clone)]
pub struct ToolInput {
    /// The arguments object, exactly as the client sent it.
    pub value: Value,
}

/// Output a tool returns.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Content blocks - matches MCP `tools/call` response shape.
    pub blocks: Vec<ToolOutputBlock>,
    /// Whether this represents a tool failure.
    pub is_error: bool,
}

impl ToolOutput {
    /// Build a text-only successful output.
    pub fn text(s: impl Into<String>) -> Self {
        Self { blocks: vec![ToolOutputBlock::Text { text: s.into() }], is_error: false }
    }

    /// Build a text-only failed output: the reason the call did not do what
    /// it was asked.
    pub fn error(s: impl Into<String>) -> Self {
        Self { blocks: vec![ToolOutputBlock::Text { text: s.into() }], is_error: true }
    }

    /// Build an image output.
    pub fn image(mime_type: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            blocks: vec![ToolOutputBlock::Image { mime_type: mime_type.into(), data }],
            is_error: false,
        }
    }

    /// Serialise to the JSON shape MCP expects.
    pub(crate) fn to_mcp_content(&self) -> Vec<Value> {
        self.blocks
            .iter()
            .map(|b| match b {
                ToolOutputBlock::Text { text } => {
                    serde_json::json!({ "type": "text", "text": text })
                }
                // The MCP image shape, which carries the bytes base64 - the
                // encoding the CLI's own image blocks arrive in.
                ToolOutputBlock::Image { mime_type, data } => serde_json::json!({
                    "type": "image",
                    "data": base64::engine::general_purpose::STANDARD.encode(data),
                    "mimeType": mime_type,
                }),
            })
            .collect()
    }
}

/// A single content block returned from a tool. MCP supports resources
/// too; add a variant here when the binary actually asks for one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutputBlock {
    /// The text payload.
    Text { text: String },
    /// An image's bytes and the mime type they are in. Encoded as the MCP
    /// image content shape at the boundary, which carries `data` base64.
    Image { mime_type: String, data: Vec<u8> },
}

/// A registered MCP tool. Implementations describe themselves (name,
/// description, schema) and implement the async `call` method that
/// produces an output for the given input.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Tool name. Exposed to the model as `mcp__<server>__<tool>`.
    ///
    /// `'static` rather than borrowed from `&self`: a tool's identity is
    /// fixed at compile time, and eliding the lifetime here made every
    /// impl in the tree trip `clippy::unnecessary_literal_bound`.
    fn name(&self) -> &'static str;

    /// One-line description.
    fn description(&self) -> &'static str;

    /// JSON Schema describing the tool's arguments.
    fn input_schema(&self) -> Value;

    /// Execute the tool.
    async fn call(&self, input: ToolInput) -> ToolOutput;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image reaches the CLI as the MCP image content shape, which carries
    /// the bytes base64 under `data` and the mime beside them - the same shape
    /// a screenshot from any MCP server arrives in.
    #[test]
    fn an_image_block_crosses_as_an_mcp_image_content_block() {
        let output = ToolOutput::image("image/png", vec![0x00, 0xff, 0x10]);
        let content = output.to_mcp_content();
        assert_eq!(content.len(), 1, "one block in, one block out");
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["mimeType"], "image/png");
        assert_eq!(content[0]["data"], "AP8Q", "the bytes base64-encoded, in order");
        assert!(!output.is_error, "an image is a result, not a failure");
    }

    /// Text and failure keep the shapes they had: a text block, and the same
    /// block marked as the call's error.
    #[test]
    fn text_and_failure_cross_as_text_blocks() {
        let output = ToolOutput::error("no browser-capable client connected");
        assert!(output.is_error, "the failure arm marks the call failed");
        let content = output.to_mcp_content();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "no browser-capable client connected");
    }
}
