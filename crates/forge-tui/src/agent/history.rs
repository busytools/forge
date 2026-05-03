//! Resume-history mapping. Converts a list of on-disk
//! `SessionMessage`s into the `SessionUpdate` stream the TUI
//! replays at resume time, via
//! `map_session_messages_to_updates(messages)`.

use std::collections::HashMap;

use serde_json::Value;

use crate::agent::types::{ContentBlock, SessionUpdate, ToolCall, ToolCallUpdate};

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
    let block = ContentBlock::Text { text: text.to_owned() };
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
    let Some(record) = block.as_object() else { return };
    let Some(tool_use_id) = record.get("id").and_then(Value::as_str) else { return };
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
    let Some(record) = block.as_object() else { return };
    let Some(tool_use_id) = record.get("tool_use_id").and_then(Value::as_str) else { return };
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

    let Some(base) = tool_calls.get_mut(tool_use_id) else { return };
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
/// / user), `parent_tool_use_id` (Option<String>), and an inner
/// `message` Value with the Anthropic-API-shaped envelope.
#[must_use]
pub fn map_session_messages_to_updates(messages: &[Value]) -> Vec<SessionUpdate> {
    let mut updates: Vec<SessionUpdate> = Vec::new();
    let mut tool_calls: HashMap<String, ToolCall> = HashMap::new();

    for entry in messages {
        let Some(entry_record) = entry.as_object() else { continue };
        let entry_type = entry_record.get("type").and_then(Value::as_str).unwrap_or("");
        let fallback_role = if entry_type == "assistant" { "assistant" } else { "user" };
        let entry_parent = entry_record.get("parent_tool_use_id").and_then(Value::as_str);

        let Some(message_value) = entry_record.get("message") else { continue };
        for message in message_candidates(message_value) {
            let Some(message_record) = message.as_object() else { continue };
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
                let Some(block) = item.as_object() else { continue };
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
        let SessionUpdate::UserMessageChunk { content: ContentBlock::Text { text } } = &updates[0]
        else {
            panic!()
        };
        assert_eq!(text, "[image]");
    }
}
