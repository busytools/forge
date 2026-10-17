//! Resume-history synthesizer: converts on-disk transcript JSONL into the
//! `forge_primitives::Message` envelopes the live wire stream produces, so
//! consumers read replay and live messages identically.

use serde_json::{Value, json};

use forge_primitives::messages::Extras;
use forge_primitives::{AssistantEnvelope, ContentBlock, Message, UserEnvelope};

/// Synthesise on-disk transcript JSONL into the `forge_primitives::Message`
/// envelopes the live wire stream produces. Consumers read these
/// identically to live messages, unifying replay + live code paths.
///
/// Replay-specific transforms applied here:
/// - A TOP-LEVEL image block keeps its mime and the size its payload weighed,
///   and loses that payload: the read draws the same attachment row the live
///   path draws for it, and that row shows no bytes.
///
///   **Top-level blocks only, by choice.** A `Document` block and an image
///   nested in a tool result are passed through untouched - payload and all -
///   because a call row draws a result's image from its own data URL, and
///   nothing here should change what that gets.
///
/// **Thinking blocks are kept.** A thinking block is the row the chat draws
/// for a thought, and the live path draws it (a `thinking` block in an
/// `assistant` frame), so a read that dropped the block drew a different
/// conversation from the same turn - the read and live paths have to fold
/// the same frames (rule 25).
///
/// The `session_id` field on each Message is left empty; the caller
/// (`forge_sdk_worker::spawn_session`) is responsible for stamping the
/// correct id before emitting `AgentEvent::Connected`. The row's `uuid`
/// is carried across when it has one: it is the id the same message
/// arrives under on the wire, so a reader can tell the two apart.
pub fn synthesize_replay_messages(messages: &[Value]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();

    for entry in messages {
        let Some(entry_record) = entry.as_object() else {
            continue;
        };
        let entry_type = entry_record.get("type").and_then(Value::as_str).unwrap_or("");
        let parent_tool_use_id =
            entry_record.get("parent_tool_use_id").and_then(Value::as_str).map(str::to_owned);
        let uuid = entry_record
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let timestamp = entry_record.get("timestamp").and_then(Value::as_str).map(str::to_owned);
        let Some(message_value) = entry_record.get("message") else {
            continue;
        };
        let Some(message_record) = message_value.as_object() else {
            continue;
        };

        // Treat the inner `message` as Anthropic-API-shaped: the on-disk
        // JSONL nests the same `{id, role, model, content, …}` envelope
        // the live CLI emits, so deserialize-then-transform is the
        // most direct path. Bail on any entry that fails to decode -
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
                    uuid: uuid.clone(),
                    timestamp,
                    extras: forge_primitives::messages::Extras::new(),
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
                    uuid,
                    tool_use_result: entry_record
                        .get("tool_use_result")
                        .filter(|result| !result.is_null())
                        .cloned(),
                    timestamp,
                    synthetic: entry_record
                        .get("synthetic")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    extras: forge_primitives::messages::Extras::new(),
                });
            }
            // A system frame has no inner envelope: the scan hands the row
            // whole, and it is already the shape the wire sends, so the
            // same decoder turns it into the frame a reader draws. A row
            // missing a typed field decodes as a generic `Message::System`
            // rather than failing - a frame a view draws only where it has an
            // arm for the subtype, and the boundary row draws itself bare or
            // half-filled from the facts it kept - so the Err arm is for a
            // shape this cannot currently take.
            "system" => match serde_json::from_value::<Message>(message_value.clone()) {
                Ok(frame) => out.push(frame),
                Err(err) => tracing::warn!(
                    target: "agent.history",
                    event_name = "replay_system_frame_undecoded",
                    ?message_value,
                    %err,
                    "synthesize_replay_messages: failed to decode a system frame; entry skipped",
                ),
            },
            _ => {}
        }
    }

    out
}

/// Apply the replay-specific content-block transform: an `Image` block keeps
/// its mime and the size its payload weighed, and loses the payload itself.
/// Other variants pass through unchanged.
fn transform_replay_content(content: Vec<ContentBlock>) -> Vec<ContentBlock> {
    content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Image { source, extras } => attachment(source, extras),
            other => other,
        })
        .collect()
}

/// An image block as the read path keeps it.
///
/// The block stays an image so the view's own attachment row draws it, as the
/// live path already does. The payload dropped here is the block's OWN: the
/// attachment row prints the size and nothing else, so those bytes would ride
/// to a page that never draws them. What the block carries instead is that
/// size, under the `forge_bytes` name - forge's own, so it cannot be read as a
/// field the CLI wrote.
fn attachment(source: Value, extras: Extras) -> ContentBlock {
    let Some(data) = source.get("data").and_then(Value::as_str) else {
        return ContentBlock::Image { source, extras };
    };
    let size = payload_bytes(data);
    let mut keeps = source;
    if let Some(fields) = keeps.as_object_mut() {
        fields.remove("data");
    }
    let mut kept = extras;
    kept.insert("forge_bytes".to_owned(), json!(size));
    ContentBlock::Image { source: keeps, extras: kept }
}

/// The bytes a base64 payload stands for, by the same arithmetic the view's
/// own row uses: four characters carry three bytes, and padding is not a byte.
fn payload_bytes(data: &str) -> u64 {
    let padding = if data.ends_with("==") { 2 } else { u64::from(data.ends_with('=')) };
    (data.len() as u64 * 3 / 4).saturating_sub(padding)
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
        assert!(matches!(&message.content[0], ContentBlock::Text { text, .. } if text == "hi"));
        let Message::Assistant { message, session_id, .. } = &synthesized[1] else {
            panic!("expected second synthesized message to be Message::Assistant");
        };
        assert!(session_id.is_empty(), "session_id must be left empty for the caller to stamp");
        assert_eq!(message.id, "msg_01");
        assert_eq!(message.model, "claude-opus-4-5");
        assert_eq!(message.content.len(), 1);
        assert!(matches!(&message.content[0], ContentBlock::Text { text, .. } if text == "hello"));
    }

    #[test]
    fn synthesize_thinking_blocks_survive_the_read() {
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
        assert_eq!(
            message.content.len(),
            2,
            "the thinking block rides the read: the live path draws a row for it, so dropping \
             it here draws the same turn two ways",
        );
        assert!(
            matches!(
                &message.content[0],
                ContentBlock::Thinking { thinking, .. } if thinking == "reasoning…"
            ),
            "the thought's own words stay where the assistant frame carried them - a block kept \
             with its text blanked draws no row either",
        );
        assert!(
            matches!(&message.content[1], ContentBlock::Text { text, .. } if text == "after thought"),
            "and the text after it is untouched",
        );
    }

    #[test]
    fn synthesize_keeps_an_attachment_the_live_path_draws() {
        // The block stays an image: the view's attachment row is what the live
        // path draws from the same block, and a text `[image]` drew the words
        // instead of it. The payload itself does not ride to the page - the row
        // prints the size, not the picture - so the block carries the size this
        // side measured.
        let data = "A".repeat(400);
        let messages = vec![json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {"type":"text","text":"the mock I mean"},
                    {"type":"image","source":{"type":"base64","media_type":"image/png","data": data}}
                ]
            }
        })];
        let synthesized = synthesize_replay_messages(&messages);
        assert_eq!(synthesized.len(), 1);
        let Message::User { message, .. } = &synthesized[0] else {
            panic!("expected Message::User");
        };
        assert_eq!(message.content.len(), 2);
        let ContentBlock::Image { source, extras } = &message.content[1] else {
            panic!("expected the image block to stay an image");
        };
        assert_eq!(
            source.get("media_type").and_then(Value::as_str),
            Some("image/png"),
            "the row names the mime the block carried",
        );
        assert_eq!(
            source.get("data"),
            None,
            "the payload does not ride to a page that draws no bytes",
        );
        assert_eq!(
            extras.get("forge_bytes").and_then(Value::as_u64),
            Some(300),
            "the size the block weighed is what the row prints",
        );
    }

    #[test]
    fn synthesize_leaves_an_attachment_it_weighed_nothing_for_alone() {
        // A source with no base64 payload - a URL, or a block the CLI wrote
        // without data - has no size this side can measure, and the row draws
        // what it does have.
        let messages = vec![json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type":"image","source":{"type":"url","url":"https://example.test/a.png"}}]
            }
        })];
        let synthesized = synthesize_replay_messages(&messages);
        let Message::User { message, .. } = &synthesized[0] else {
            panic!("expected Message::User");
        };
        let ContentBlock::Image { source, extras } = &message.content[0] else {
            panic!("expected the image block to stay an image");
        };
        assert_eq!(
            source.get("url").and_then(Value::as_str),
            Some("https://example.test/a.png"),
            "a source with no payload is left as it came",
        );
        assert!(extras.is_empty(), "and nothing is invented for it");
    }

    #[test]
    fn synthesize_malformed_entry_is_skipped() {
        let messages = vec![
            // Missing required AssistantEnvelope fields (id/model) - must skip.
            json!({ "type": "assistant", "message": { "role": "assistant" } }),
            // Valid user message - must pass through.
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
        assert!(matches!(&message.content[0], ContentBlock::Text { text, .. } if text == "valid"));
    }
}
