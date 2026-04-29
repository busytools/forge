#![allow(
    missing_docs,
    clippy::pedantic,
    reason = "MVP adapter; full JSON->ChatMessage parser lives in upstream app/events/* (~1.7k LoC) which is not lifted"
)]

//! Minimal `serde_json::Value` → `ChatMessage` adapter, used by the
//! cutover to feed daemon `session.event` payloads into the lifted
//! `state::app::App`.
//!
//! Handles only the simple cases that forge's existing renderer
//! already covers (user/assistant/system text). Tool-use / tool-result
//! / image blocks defer to the full upstream `app/events/*` lift.

use crate::state::messages::{ChatMessage, MessageBlock, MessageRole, SystemSeverity, TextBlock};

/// Best-effort parse of a `session.event` `params.message.message`
/// value into a `ChatMessage`. Returns `None` for shapes we don't
/// recognise; callers should fall back to a system notice.
#[must_use]
pub fn json_to_chat_message(message: &serde_json::Value) -> Option<ChatMessage> {
    // Drill into the nested shape: { message: { role, content } }
    let inner = message.get("message").unwrap_or(message);

    let role_str = inner.get("role").and_then(serde_json::Value::as_str)?;
    let role = match role_str {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System(None),
        _ => return None,
    };

    let content = inner.get("content")?;
    let blocks = content_to_blocks(content, &role);
    if blocks.is_empty() {
        return None;
    }

    Some(ChatMessage::new(role, blocks, None))
}

fn content_to_blocks(content: &serde_json::Value, role: &MessageRole) -> Vec<MessageBlock> {
    match content {
        serde_json::Value::String(s) => vec![text_block_for_role(s.clone(), role)],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| content_block(item, role))
            .collect(),
        _ => Vec::new(),
    }
}

fn content_block(item: &serde_json::Value, role: &MessageRole) -> Option<MessageBlock> {
    let kind = item.get("type").and_then(serde_json::Value::as_str)?;
    match kind {
        "text" => {
            let text = item.get("text").and_then(serde_json::Value::as_str)?.to_owned();
            Some(text_block_for_role(text, role))
        }
        // tool_use / tool_result blocks need ToolCallInfo construction,
        // which is non-trivial. Defer to the full app/events/* lift.
        _ => None,
    }
}

fn text_block_for_role(text: String, role: &MessageRole) -> MessageBlock {
    match role {
        MessageRole::System(severity) => MessageBlock::Notice(
            crate::state::messages::NoticeBlock::new(severity.unwrap_or(SystemSeverity::Info), text),
        ),
        _ => MessageBlock::Text(TextBlock::new(text)),
    }
}
