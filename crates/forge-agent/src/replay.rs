//! Resume-history synthesizer: converts on-disk transcript JSONL into the
//! `forge_primitives::Message` envelopes the live wire stream produces, so
//! the TUI's raw walker (`handle_sdk_message`) can consume replay and live
//! messages identically.

use serde_json::Value;

use forge_primitives::{AssistantEnvelope, ContentBlock, Message, UserEnvelope};

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
        let parent_tool_use_id =
            entry_record.get("parent_tool_use_id").and_then(Value::as_str).map(str::to_owned);
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
            ContentBlock::Image { .. } => Some(ContentBlock::Text { text: "[image]".to_owned() }),
            other => Some(other),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
