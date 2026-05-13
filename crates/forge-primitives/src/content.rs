//! Content blocks inside assistant and user messages.
//!
//! SDK's `TextBlock`, `ThinkingBlock`, `ToolUseBlock`,
//! `ToolResultBlock`. Wire shape is `{"type": "...", ...}`. Unknown
//! block types land in [`ContentBlock::Unknown`] so the decoder is
//! forward-compatible with new Anthropic API blocks (`document`,
//! future types) without an SDK bump — mirrors the top-level
//! the SDK's `DecodedLine::Unknown`
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
    /// `ServerToolUseBlock`.
    ///
    /// `name` is a discriminator: typed as the Literal set
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
    /// `advisor_tool_result`). Same wire shape as `ToolResult` but
    /// the `content` dict is opaque — branch on `content["type"]`
    /// to tell which concrete server-tool result schema applies (e.g.
    /// `advisor_result` vs. `advisor_redacted_result`).
    ServerToolResult {
        /// The `id` from the corresponding [`ContentBlock::ServerToolUse`] block.
        tool_use_id: String,
        /// Raw server-tool result payload. Schema depends on the server
        /// tool — inspect `content["type"]` for the concrete shape.
        content: Value,
    },

    /// Document attachment (PDF, etc.) inlined into a user turn. The
    /// Anthropic API supports these directly — when the user attaches
    /// a file to a Claude Code conversation, the CLI emits
    /// `{"type":"document","source":{"type":"base64",
    /// "media_type":"<mime>","data":"<base64>"}}` (or `"type":"url"` /
    /// `"type":"text"` for the alternate source variants).
    ///
    /// `source` stays a [`Value`] rather than a typed struct because
    /// Anthropic's source schema branches on `source.type`
    /// (`base64` / `url` / `text` / `content`) and each variant
    /// carries different fields — keeping it as a JSON object lets
    /// new source types decode without an SDK bump.
    Document {
        /// Source of the document — JSON object whose `type` field
        /// discriminates between `base64`, `url`, `text`, `content`.
        source: Value,
    },

    /// Image attachment (JPEG / PNG / GIF / WEBP) inlined into a user
    /// turn. Same shape as [`ContentBlock::Document`] but with
    /// `"type":"image"` on the wire. Surfaced when a user pastes or
    /// attaches an image in a Claude Code conversation.
    Image {
        /// Source of the image — JSON object whose `type` field
        /// discriminates between `base64`, `url`, etc.
        source: Value,
    },

    /// User input that was typed while a previous turn was in flight
    /// and got "queued" by the CLI. Claude Code packages these as
    /// `queued_command`-type content blocks on the next outbound
    /// user-message envelope going to the model, so the model sees
    /// them as additional context.
    ///
    /// Wire shape (from the CLI binary's `gO6` function):
    /// ```json
    /// {"type": "queued_command",
    ///  "prompt": "<text or content-block list>",
    ///  "commandMode": "prompt" | "bash",
    ///  "source_uuid": "<optional>",
    ///  ...}
    /// ```
    ///
    /// Forge surfaces this so resumed sessions render the queued
    /// inputs as user bubbles in the chat. Without it, a session
    /// that had mid-turn queued inputs would replay missing those
    /// user messages — see issue #85.
    QueuedCommand {
        /// The queued user text (or content-block array — kept as
        /// `Value` so list-of-blocks variants pass through cleanly).
        prompt: Value,
        /// Mode the input was submitted in. Typically `"prompt"`;
        /// `"bash"` for `!`-prefixed shell commands.
        command_mode: Option<String>,
        /// Source UUID assigned by the CLI when the input was queued.
        /// Useful for de-duplication / threading; not load-bearing
        /// for rendering.
        source_uuid: Option<String>,
    },

    /// Forward-compat fallback for content block types forge-sdk
    /// doesn't model explicitly. Callers can branch on `type_str` +
    /// inspect `raw` — the decoder never errors on an unrecognised
    /// block. Mirrors the top-level
    /// the SDK's `DecodedLine::Unknown`
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
            ContentBlock::Thinking { thinking, signature } => {
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
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
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
            ContentBlock::ServerToolResult { tool_use_id, content } => {
                serde_json::json!({
                    "type": "advisor_tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                })
            }
            ContentBlock::Document { source } => {
                serde_json::json!({
                    "type": "document",
                    "source": source,
                })
            }
            ContentBlock::Image { source } => {
                serde_json::json!({
                    "type": "image",
                    "source": source,
                })
            }
            ContentBlock::QueuedCommand { prompt, command_mode, source_uuid } => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".into(), Value::String("queued_command".into()));
                obj.insert("prompt".into(), prompt.clone());
                if let Some(mode) = command_mode {
                    obj.insert("commandMode".into(), Value::String(mode.clone()));
                }
                if let Some(uuid) = source_uuid {
                    obj.insert("source_uuid".into(), Value::String(uuid.clone()));
                }
                Value::Object(obj)
            }
            ContentBlock::Unknown { raw, .. } => raw.clone(),
        };
        v.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        // Distinguish "missing" (wire corruption) from "unrecognised"
        // (forward-compat drift) so debug logs + `Unknown` payloads carry
        // the right signal. ControlRequestKind::Deserialize`'s
        // sentinel handling in `src/control.rs`.
        let ty = raw.get("type").and_then(Value::as_str).unwrap_or("<missing>").to_string();
        match ty.as_str() {
            "text" => Ok(ContentBlock::Text {
                text: raw.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
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
                id: raw.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                name: raw.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                input: raw.get("input").cloned().unwrap_or(Value::Null),
            }),
            "tool_result" => Ok(ContentBlock::ToolResult {
                tool_use_id: raw
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                content: raw.get("content").cloned().unwrap_or(Value::Null),
                is_error: raw.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            }),
            "server_tool_use" => Ok(ContentBlock::ServerToolUse {
                id: raw.get("id").and_then(Value::as_str).unwrap_or_default().to_string(),
                name: raw.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
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
            "document" | "image" => {
                let source = raw.get("source").cloned().unwrap_or(Value::Null);
                if source.is_null() {
                    // Required field per Anthropic API spec — log so a
                    // CLI bug emitting a partial block surfaces in
                    // logs instead of decoding silently to a
                    // semantically-broken `Document { source: Null }`.
                    tracing::warn!(
                        kind = %ty,
                        "content block decoded with missing/null `source`; \
                         consumer-side branching on `source[\"type\"]` will see Null"
                    );
                }
                Ok(if ty == "document" {
                    ContentBlock::Document { source }
                } else {
                    ContentBlock::Image { source }
                })
            }
            "queued_command" => Ok(ContentBlock::QueuedCommand {
                prompt: raw.get("prompt").cloned().unwrap_or(Value::Null),
                command_mode: raw.get("commandMode").and_then(Value::as_str).map(str::to_string),
                source_uuid: raw.get("source_uuid").and_then(Value::as_str).map(str::to_string),
            }),
            other => Ok(ContentBlock::Unknown { type_str: other.to_string(), raw }),
        }
    }
}

#[cfg(test)]
mod tests_content_roundtrip {
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use crate::content::ContentBlock;
    use serde_json::json;

    #[test]
    fn text_block_roundtrip() {
        let raw = json!({"type": "text", "text": "hello world"});
        let block: ContentBlock = serde_json::from_value(raw.clone()).expect("parse");
        matches!(block, ContentBlock::Text { .. }).then_some(()).expect("text variant");
        let re = serde_json::to_value(&block).expect("serialize");
        assert_eq!(raw, re);
    }

    #[test]
    fn thinking_block_roundtrip() {
        let raw =
            json!({"type": "thinking", "thinking": "let me think...", "signature": "sig-abc"});
        let block: ContentBlock = serde_json::from_value(raw.clone()).expect("parse");
        matches!(block, ContentBlock::Thinking { .. }).then_some(()).expect("thinking variant");
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
        matches!(block, ContentBlock::ToolUse { .. }).then_some(()).expect("tool_use variant");
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
        let ContentBlock::Unknown { type_str, raw: echoed } = block else {
            panic!("expected Unknown variant");
        };
        assert_eq!(type_str, "unknown_kind");
        assert_ne!(type_str, "<missing>");
        assert_eq!(echoed, raw);
    }

    #[test]
    fn missing_type_field_lands_in_unknown_with_missing_sentinel() {
        // Wire corruption (block payload with no `type` key at all) is
        // distinguishable from forward-compat drift (block with an
        // unrecognised `type` string) via the explicit `<missing>`
        // sentinel. Mirrors the same contract for
        // `ControlRequestKind::Deserialize` in `src/control.rs`. Pinned
        // here so a future refactor can't silently regress to
        // `unwrap_or("")`.
        let raw = json!({"text_field": "no type at all"});
        let block: ContentBlock = serde_json::from_value(raw.clone()).expect("parse");
        let ContentBlock::Unknown { type_str, .. } = block else {
            panic!("expected Unknown variant for missing type");
        };
        assert_eq!(
            type_str, "<missing>",
            "missing-type must use the explicit sentinel, not empty string"
        );
    }
}
