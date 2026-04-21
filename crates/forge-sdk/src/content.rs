//! Content blocks inside assistant and user messages.
//!
//! Mirrors Python SDK's `TextBlock`, `ThinkingBlock`, `ToolUseBlock`,
//! `ToolResultBlock`. Wire shape is `{"type": "...", ...}` — `serde` emits
//! the discriminant via `#[serde(tag = "type")]`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single block inside an assistant turn's `content` array or a user
/// message's tool-result envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text from the assistant.
    Text {
        /// The text payload.
        text: String,
    },

    /// Extended-thinking reasoning block. Present when the model was run in
    /// thinking mode.
    Thinking {
        /// The reasoning text.
        thinking: String,
        /// Anthropic's signature for the thinking block.
        signature: String,
    },

    /// The model wants to invoke a tool.
    ToolUse {
        /// Opaque id Anthropic assigns to this tool invocation.
        id: String,
        /// Tool name (e.g. `"Edit"`, `"Bash"`, `"mcp__foo__bar"`).
        name: String,
        /// JSON input the model generated for this tool.
        input: Value,
    },

    /// A tool's output, sent back to the model in a user turn.
    ToolResult {
        /// The `id` from the corresponding `ToolUse` block.
        tool_use_id: String,
        /// Rendered tool output (often a string, may be a JSON object in the wire).
        content: Value,
        /// Whether the tool reported failure.
        #[serde(default)]
        is_error: bool,
    },
}
