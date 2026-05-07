//! Resume-history mapping. Converts a list of on-disk
//! `SessionMessage`s into the `SessionUpdate` stream the TUI
//! replays at resume time, via
//! `map_session_messages_to_updates(messages)`.

use std::collections::HashMap;

use serde_json::Value;

use forge_primitives::{
    AssistantEnvelope, ChunkContent, ContentBlock, Message, SessionUpdate, ToolCall,
    ToolCallUpdate, UserEnvelope,
};

use super::tooling::{
    TOOL_RESULT_TYPES, build_tool_result_fields, create_tool_call, is_tool_use_block_type,
};

fn message_candidates(raw: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    if !raw.is_object() {
        return out;
    }
    out.push(raw);
    if let Some(nested) = raw.get("message")
        && nested.is_object()
    {
        out.push(nested);
    }
    out
}

fn push_resume_text_chunk(updates: &mut Vec<SessionUpdate>, role: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let block = ChunkContent::Text { text: text.to_owned() };
    updates.push(if role == "assistant" {
        SessionUpdate::AgentMessageChunk { content: block }
    } else {
        SessionUpdate::UserMessageChunk { content: block }
    });
}

fn push_resume_tool_use(
    updates: &mut Vec<SessionUpdate>,
    tool_calls: &mut HashMap<String, ToolCall>,
    block: &Value,
    parent_tool_use_id: Option<&str>,
) {
    let Some(record) = block.as_object() else {
        return;
    };
    let Some(tool_use_id) = record.get("id").and_then(Value::as_str) else {
        return;
    };
    if tool_use_id.is_empty() {
        return;
    }
    let name = record.get("name").and_then(Value::as_str).unwrap_or("Tool");
    let empty_input = Value::Object(serde_json::Map::new());
    let input = record.get("input").unwrap_or(&empty_input);

    let mut tool_call = create_tool_call(tool_use_id, name, input, parent_tool_use_id);
    "in_progress".clone_into(&mut tool_call.status);
    tool_calls.insert(tool_use_id.to_owned(), tool_call.clone());
    updates.push(SessionUpdate::ToolCall { tool_call });
}

fn push_resume_tool_result(
    updates: &mut Vec<SessionUpdate>,
    tool_calls: &mut HashMap<String, ToolCall>,
    block: &Value,
) {
    let Some(record) = block.as_object() else {
        return;
    };
    let Some(tool_use_id) = record.get("tool_use_id").and_then(Value::as_str) else {
        return;
    };
    if tool_use_id.is_empty() {
        return;
    }
    let is_error = record.get("is_error").and_then(Value::as_bool).unwrap_or(false);
    let base = tool_calls.get(tool_use_id).cloned();
    let raw_content = record.get("content");
    let fields = build_tool_result_fields(is_error, raw_content, base.as_ref(), Some(block));
    updates.push(SessionUpdate::ToolCallUpdate {
        tool_call_update: ToolCallUpdate {
            tool_call_id: tool_use_id.to_owned(),
            fields: fields.clone(),
        },
    });

    let Some(base) = tool_calls.get_mut(tool_use_id) else {
        return;
    };
    if let Some(s) = fields.status {
        base.status = s;
    }
    if let Some(out) = fields.raw_output {
        base.raw_output = Some(out);
    }
    if let Some(content) = fields.content {
        base.content = content;
    }
    if let Some(om) = fields.output_metadata {
        base.output_metadata = Some(om);
    }
}

/// Mirrors `mapSessionMessagesToUpdates(messages)`. Iterates the
/// on-disk transcript and emits `SessionUpdate`s the TUI's history
/// renderer replays on resume.
///
/// `messages` is the flattened JSONL: each entry has `type` (assistant
/// / user), `parent_tool_use_id` (`Option<String>`), and an inner
/// `message` Value with the Anthropic-API-shaped envelope.
#[must_use]
pub fn map_session_messages_to_updates(messages: &[Value]) -> Vec<SessionUpdate> {
    let mut updates: Vec<SessionUpdate> = Vec::new();
    let mut tool_calls: HashMap<String, ToolCall> = HashMap::new();

    for entry in messages {
        let Some(entry_record) = entry.as_object() else {
            continue;
        };
        let entry_type = entry_record.get("type").and_then(Value::as_str).unwrap_or("");
        let fallback_role = if entry_type == "assistant" { "assistant" } else { "user" };
        let entry_parent = entry_record.get("parent_tool_use_id").and_then(Value::as_str);

        let Some(message_value) = entry_record.get("message") else {
            continue;
        };
        for message in message_candidates(message_value) {
            let Some(message_record) = message.as_object() else {
                continue;
            };
            let role = message_record
                .get("role")
                .and_then(Value::as_str)
                .filter(|r| matches!(*r, "assistant" | "user"))
                .unwrap_or(fallback_role);
            let parent_tool_use_id = entry_parent
                .or_else(|| message_record.get("parent_tool_use_id").and_then(Value::as_str));
            let Some(content) = message_record.get("content").and_then(Value::as_array) else {
                continue;
            };
            for item in content {
                let Some(block) = item.as_object() else {
                    continue;
                };
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                if block_type == "thinking" {
                    continue;
                }
                if block_type == "text"
                    && let Some(text) = block.get("text").and_then(Value::as_str)
                {
                    push_resume_text_chunk(&mut updates, role, text);
                    continue;
                }
                if is_tool_use_block_type(block_type) && role == "assistant" {
                    push_resume_tool_use(&mut updates, &mut tool_calls, item, parent_tool_use_id);
                    continue;
                }
                if TOOL_RESULT_TYPES.contains(&block_type) {
                    push_resume_tool_result(&mut updates, &mut tool_calls, item);
                    continue;
                }
                if block_type == "image" {
                    push_resume_text_chunk(&mut updates, role, "[image]");
                }
            }
        }
    }

    updates
}

/// Synthesise on-disk transcript JSONL into the `forge_primitives::Message`
/// envelopes the live wire stream produces. The TUI's raw walker
/// (`handle_sdk_message`) consumes these identically to live messages,
/// unifying replay + live code paths.
///
/// Replay-specific transforms applied here:
/// - Thinking blocks are filtered out — ephemeral mid-stream signals,
///   never re-rendered on resume.
/// - Image content blocks are replaced with a `[image]` text placeholder
///   — the binary payload isn't preserved on disk.
///
/// The `session_id` field on each Message is left empty; the caller
/// (`forge_sdk_worker::spawn_session`) is responsible for stamping the
/// correct id before emitting `AgentEvent::Connected`.
#[must_use]
pub fn synthesize_replay_messages(messages: &[Value]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();

    for entry in messages {
        let Some(entry_record) = entry.as_object() else {
            continue;
        };
        let entry_type = entry_record.get("type").and_then(Value::as_str).unwrap_or("");
        let parent_tool_use_id = entry_record
            .get("parent_tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(message_value) = entry_record.get("message") else {
            continue;
        };
        let Some(message_record) = message_value.as_object() else {
            continue;
        };

        // Treat the inner `message` as Anthropic-API-shaped: the on-disk
        // JSONL nests the same `{id, role, model, content, …}` envelope
        // the live CLI emits, so deserialize-then-transform is the
        // most direct path. Bail on any entry that fails to decode —
        // missing/malformed inner envelopes shouldn't crash a session
        // resume.
        let role = message_record.get("role").and_then(Value::as_str).unwrap_or(entry_type);

        match role {
            "assistant" => {
                let Ok(mut envelope) =
                    serde_json::from_value::<AssistantEnvelope>(message_value.clone())
                else {
                    tracing::warn!(
                        target: "agent.history",
                        ?message_value,
                        "synthesize_replay_messages: failed to decode AssistantEnvelope; entry skipped",
                    );
                    continue;
                };
                envelope.content = transform_replay_content(envelope.content);
                out.push(Message::Assistant {
                    message: envelope,
                    session_id: String::new(),
                    parent_tool_use_id,
                    error: None,
                    uuid: None,
                });
            }
            "user" => {
                let Ok(mut envelope) =
                    serde_json::from_value::<UserEnvelope>(message_value.clone())
                else {
                    tracing::warn!(
                        target: "agent.history",
                        ?message_value,
                        "synthesize_replay_messages: failed to decode UserEnvelope; entry skipped",
                    );
                    continue;
                };
                envelope.content = transform_replay_content(envelope.content);
                out.push(Message::User {
                    message: envelope,
                    session_id: String::new(),
                    parent_tool_use_id,
                    uuid: None,
                    tool_use_result: None,
                });
            }
            _ => {}
        }
    }

    out
}

/// Apply the two replay-specific content-block transforms:
/// drop `Thinking` blocks; replace `Image` blocks with a text `[image]`
/// placeholder. Other variants pass through unchanged.
fn transform_replay_content(content: Vec<ContentBlock>) -> Vec<ContentBlock> {
    content
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Thinking { .. } => None,
            ContentBlock::Image { .. } => {
                Some(ContentBlock::Text { text: "[image]".to_owned() })
            }
            other => Some(other),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_simple_text_history() {
        let messages = vec![
            json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}),
        ];
        let updates = map_session_messages_to_updates(&messages);
        assert_eq!(updates.len(), 2);
        assert!(matches!(updates[0], SessionUpdate::UserMessageChunk { .. }));
        assert!(matches!(updates[1], SessionUpdate::AgentMessageChunk { .. }));
    }

    #[test]
    fn map_tool_use_then_result_pairs_in_map() {
        let messages = vec![
            json!({"type":"assistant","message":{"role":"assistant","content":[
                {"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}
            ]}}),
            json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"tu1","is_error":false,"content":"file1\nfile2\n"}
            ]}}),
        ];
        let updates = map_session_messages_to_updates(&messages);
        assert_eq!(updates.len(), 2);
        let SessionUpdate::ToolCall { tool_call } = &updates[0] else { panic!() };
        assert_eq!(tool_call.title, "ls");
        assert_eq!(tool_call.status, "in_progress");
        let SessionUpdate::ToolCallUpdate { tool_call_update } = &updates[1] else { panic!() };
        assert_eq!(tool_call_update.tool_call_id, "tu1");
        assert_eq!(tool_call_update.fields.status.as_deref(), Some("completed"));
    }

    #[test]
    fn thinking_blocks_are_skipped() {
        let messages = vec![json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type":"thinking","thinking":"…"},
                    {"type":"text","text":"after thought"},
                ]
            }
        })];
        let updates = map_session_messages_to_updates(&messages);
        assert_eq!(updates.len(), 1);
        assert!(matches!(updates[0], SessionUpdate::AgentMessageChunk { .. }));
    }

    #[test]
    fn image_blocks_render_as_text_placeholder() {
        let messages = vec![json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type":"image","source":{"type":"base64","data":"..."}}]
            }
        })];
        let updates = map_session_messages_to_updates(&messages);
        assert_eq!(updates.len(), 1);
        let SessionUpdate::UserMessageChunk { content: ChunkContent::Text { text } } = &updates[0]
        else {
            panic!()
        };
        assert_eq!(text, "[image]");
    }

    #[test]
    fn synthesize_simple_text_history() {
        let messages = vec![
            json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type":"text","text":"hi"}]
                }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "id": "msg_01",
                    "role": "assistant",
                    "model": "claude-opus-4-5",
                    "content": [{"type":"text","text":"hello"}]
                }
            }),
        ];
        let synthesized = synthesize_replay_messages(&messages);
        assert_eq!(synthesized.len(), 2);
        let Message::User { message, session_id, .. } = &synthesized[0] else {
            panic!("expected first synthesized message to be Message::User");
        };
        assert!(session_id.is_empty(), "session_id must be left empty for the caller to stamp");
        assert_eq!(message.content.len(), 1);
        assert!(matches!(&message.content[0], ContentBlock::Text { text } if text == "hi"));
        let Message::Assistant { message, session_id, .. } = &synthesized[1] else {
            panic!("expected second synthesized message to be Message::Assistant");
        };
        assert!(session_id.is_empty(), "session_id must be left empty for the caller to stamp");
        assert_eq!(message.id, "msg_01");
        assert_eq!(message.model, "claude-opus-4-5");
        assert_eq!(message.content.len(), 1);
        assert!(matches!(&message.content[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn synthesize_thinking_blocks_are_skipped() {
        let messages = vec![json!({
            "type": "assistant",
            "message": {
                "id": "msg_02",
                "role": "assistant",
                "model": "claude-opus-4-5",
                "content": [
                    {"type":"thinking","thinking":"reasoning…","signature":"sig"},
                    {"type":"text","text":"after thought"},
                ]
            }
        })];
        let synthesized = synthesize_replay_messages(&messages);
        assert_eq!(synthesized.len(), 1);
        let Message::Assistant { message, .. } = &synthesized[0] else {
            panic!("expected Message::Assistant");
        };
        assert_eq!(message.content.len(), 1, "thinking block must be filtered out");
        assert!(
            matches!(&message.content[0], ContentBlock::Text { text } if text == "after thought"),
            "only the text block should survive",
        );
    }

    #[test]
    fn synthesize_image_blocks_become_text_placeholder() {
        let messages = vec![json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type":"image","source":{"type":"base64","data":"..."}}]
            }
        })];
        let synthesized = synthesize_replay_messages(&messages);
        assert_eq!(synthesized.len(), 1);
        let Message::User { message, .. } = &synthesized[0] else {
            panic!("expected Message::User");
        };
        assert_eq!(message.content.len(), 1);
        let ContentBlock::Text { text } = &message.content[0] else {
            panic!("expected image block to become a Text content block");
        };
        assert_eq!(text, "[image]");
    }

    #[test]
    fn synthesize_malformed_entry_is_skipped() {
        let messages = vec![
            // Missing required AssistantEnvelope fields (id/model) — must skip.
            json!({ "type": "assistant", "message": { "role": "assistant" } }),
            // Valid user message — must pass through.
            json!({
                "type": "user",
                "message": { "role": "user", "content": [{"type":"text","text":"valid"}] }
            }),
        ];
        let synthesized = synthesize_replay_messages(&messages);
        assert_eq!(synthesized.len(), 1, "malformed entry must be skipped");
        let Message::User { message, .. } = &synthesized[0] else {
            panic!("surviving entry must be the user one");
        };
        assert!(matches!(&message.content[0], ContentBlock::Text { text } if text == "valid"));
    }
}
