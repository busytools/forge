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

    /// Server-side tool invocation the API executed on the model's behalf
    /// (e.g. `advisor`, `web_search`, `web_fetch`). Surfaces alongside
    /// regular `ToolUse` blocks but the caller never returns a result —
    /// the server supplies one in a matching [`ContentBlock::ServerToolResult`].
    /// Mirrors Python SDK v0.1.64 `ServerToolUseBlock` (`types.py:904-916`).
    ///
    /// `name` is a discriminator: Python types it as the Literal set
    /// `{"advisor", "web_search", "web_fetch", "code_execution",
    /// "bash_code_execution", "text_editor_code_execution",
    /// "tool_search_tool_regex", "tool_search_tool_bm25"}`. forge-sdk
    /// keeps it as [`String`] so new server tools don't require an SDK
    /// bump to deserialise.
    ServerToolUse {
        /// Opaque id Anthropic assigns to this server-tool invocation.
        id: String,
        /// Server-tool name (see type-level docs for the current Literal set).
        name: String,
        /// JSON input the model generated for this call.
        input: Value,
    },

    /// Result block for a server-side tool call (wire type
    /// `advisor_tool_result`). Mirrors `ToolResult`'s shape but the
    /// `content` dict is opaque — branch on `content["type"]` to tell
    /// which concrete server-tool result schema applies (e.g.
    /// `advisor_result` vs. `advisor_redacted_result`). Mirrors Python
    /// SDK v0.1.64 `ServerToolResultBlock` (`types.py:919-929`).
    #[serde(rename = "advisor_tool_result")]
    ServerToolResult {
        /// The `id` from the corresponding [`ContentBlock::ServerToolUse`] block.
        tool_use_id: String,
        /// Raw server-tool result payload. Schema depends on the server
        /// tool — inspect `content["type"]` for the concrete shape.
        content: Value,
    },
}
