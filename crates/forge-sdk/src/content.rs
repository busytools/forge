//! Content blocks inside assistant and user messages.
//!
//! Mirrors Python SDK's `TextBlock`, `ThinkingBlock`, `ToolUseBlock`,
//! `ToolResultBlock`. Wire shape is `{"type": "...", ...}`. Unknown
//! block types land in [`ContentBlock::Unknown`] so the decoder is
//! forward-compatible with new Anthropic API blocks (`document`,
//! future types) without an SDK bump — mirrors the top-level
//! [`DecodedLine::Unknown`](crate::transport::codec::DecodedLine)
//! fallback.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A single block inside an assistant turn's `content` array or a user
/// message's tool-result envelope.
#[derive(Debug, Clone, PartialEq)]
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
    ServerToolResult {
        /// The `id` from the corresponding [`ContentBlock::ServerToolUse`] block.
        tool_use_id: String,
        /// Raw server-tool result payload. Schema depends on the server
        /// tool — inspect `content["type"]` for the concrete shape.
        content: Value,
    },

    /// Forward-compat fallback for content block types forge-sdk
    /// doesn't model explicitly (e.g. Anthropic API's `document`
    /// block used for PDF inputs, or future block types). Callers
    /// can branch on `type_str` + inspect `raw` — the decoder never
    /// errors on an unrecognised block. Mirrors the top-level
    /// [`DecodedLine::Unknown`](crate::transport::codec::DecodedLine)
    /// fallback pattern.
    Unknown {
        /// The unrecognised `type` field value.
        type_str: String,
        /// Full JSON payload for the block.
        raw: Value,
    },
}

/// Wire-shape `type` discriminator used by the custom (de)serialise
/// impls. The discriminator mirrors the original `#[serde(tag="type")]`
/// layout but lets us fall through to [`ContentBlock::Unknown`] for
/// anything we don't recognise.
impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let v = match self {
            ContentBlock::Text { text } => {
                serde_json::json!({"type": "text", "text": text})
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                serde_json::json!({
                    "type": "thinking",
                    "thinking": thinking,
                    "signature": signature,
                })
            }
            ContentBlock::ToolUse { id, name, input } => {
                serde_json::json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                })
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                })
            }
            ContentBlock::ServerToolUse { id, name, input } => {
                serde_json::json!({
                    "type": "server_tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                })
            }
            ContentBlock::ServerToolResult {
                tool_use_id,
                content,
            } => {
                serde_json::json!({
                    "type": "advisor_tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                })
            }
            ContentBlock::Unknown { raw, .. } => raw.clone(),
        };
        v.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        let ty = raw
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match ty.as_str() {
            "text" => Ok(ContentBlock::Text {
                text: raw
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            "thinking" => Ok(ContentBlock::Thinking {
                thinking: raw
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                signature: raw
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            "tool_use" => Ok(ContentBlock::ToolUse {
                id: raw
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: raw
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input: raw.get("input").cloned().unwrap_or(Value::Null),
            }),
            "tool_result" => Ok(ContentBlock::ToolResult {
                tool_use_id: raw
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content: raw.get("content").cloned().unwrap_or(Value::Null),
                is_error: raw
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }),
            "server_tool_use" => Ok(ContentBlock::ServerToolUse {
                id: raw
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: raw
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input: raw.get("input").cloned().unwrap_or(Value::Null),
            }),
            "advisor_tool_result" => Ok(ContentBlock::ServerToolResult {
                tool_use_id: raw
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content: raw.get("content").cloned().unwrap_or(Value::Null),
            }),
            other => Ok(ContentBlock::Unknown {
                type_str: other.to_string(),
                raw,
            }),
        }
    }
}
