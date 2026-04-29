#![allow(
    missing_docs,
    clippy::pedantic,
    reason = "MVP adapter; full JSON->ChatMessage parser lives in upstream app/events/* (~1.7k LoC) which is not lifted"
)]

//! Minimal `serde_json::Value` → `ChatMessage` adapter, used by the
//! cutover to feed daemon `session.event` payloads into the lifted
//! `state::app::App`.
//!
//! Handles user/assistant/system text, `tool_use` blocks (rendered as
//! pending tool-call cards), and `tool_result` blocks (folded into the
//! matching tool-call card by `tool_use_id`). Image / mcp_resource /
//! permission-prompt content blocks defer to the full upstream
//! `app/events/*` lift.

use crate::state::agent_types::{McpServerConnectionStatus, McpServerStatus};
use crate::state::app::App;
use crate::state::messages::{ChatMessage, MessageBlock, MessageRole, SystemSeverity, TextBlock};
use crate::state::model::{self, ToolCallContent};
use crate::state::tool_call_info::ToolCallInfo;
use crate::state::types::RecentSessionInfo;

/// Apply a daemon `session.event` payload to `app`. Pushes a new
/// `ChatMessage` for user/assistant/system text + `tool_use` content;
/// folds `tool_result` blocks into the matching existing tool-call.
///
/// Returns `true` when the event resulted in any state change (new
/// message pushed OR existing tool-call updated). `false` for shapes
/// we can't decode — caller should fall back to the legacy path.
pub fn apply_session_event(app: &mut App, message: &serde_json::Value) -> bool {
    let inner = message.get("message").unwrap_or(message);

    let Some(role_str) = inner.get("role").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let role = match role_str {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System(None),
        _ => return false,
    };

    let Some(content) = inner.get("content") else { return false };

    let mut blocks: Vec<MessageBlock> = Vec::new();
    let mut applied_any_tool_result = false;

    match content {
        serde_json::Value::String(s) => {
            blocks.push(text_block_for_role(s.clone(), &role));
        }
        serde_json::Value::Array(items) => {
            for item in items {
                let Some(kind) = item.get("type").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                match kind {
                    "text" => {
                        if let Some(text) = item.get("text").and_then(serde_json::Value::as_str) {
                            blocks.push(text_block_for_role(text.to_owned(), &role));
                        }
                    }
                    "tool_use" => {
                        if let Some(block) = parse_tool_use_block(item) {
                            blocks.push(block);
                        }
                    }
                    "tool_result" if apply_tool_result(app, item) => {
                        applied_any_tool_result = true;
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    if blocks.is_empty() {
        return applied_any_tool_result;
    }

    let chat_msg = ChatMessage::new(role, blocks, None);
    let msg_idx = app.messages.len();
    for (block_idx, block) in chat_msg.blocks.iter().enumerate() {
        if let MessageBlock::ToolCall(tool_call) = block {
            app.index_tool_call(tool_call.id.clone(), msg_idx, block_idx);
        }
    }
    app.messages.push(chat_msg);
    app.message_retained_bytes.push(0);
    true
}

/// Best-effort parse of a `session.event` `params.message.message`
/// value into a `ChatMessage`. Returns `None` for shapes we don't
/// recognise.
///
/// Convenience wrapper for places that don't have an `&mut App` —
/// strips `tool_result` blocks (which need app-state lookups). Tests
/// + historical preview paths that just want a renderable shape.
#[must_use]
pub fn json_to_chat_message(message: &serde_json::Value) -> Option<ChatMessage> {
    let inner = message.get("message").unwrap_or(message);

    let role_str = inner.get("role").and_then(serde_json::Value::as_str)?;
    let role = match role_str {
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System(None),
        _ => return None,
    };

    let content = inner.get("content")?;
    let blocks: Vec<MessageBlock> = match content {
        serde_json::Value::String(s) => vec![text_block_for_role(s.clone(), &role)],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                let kind = item.get("type").and_then(serde_json::Value::as_str)?;
                match kind {
                    "text" => item
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .map(|t| text_block_for_role(t.to_owned(), &role)),
                    "tool_use" => parse_tool_use_block(item),
                    _ => None,
                }
            })
            .collect(),
        _ => Vec::new(),
    };

    if blocks.is_empty() {
        return None;
    }

    Some(ChatMessage::new(role, blocks, None))
}

fn parse_tool_use_block(item: &serde_json::Value) -> Option<MessageBlock> {
    let id = item.get("id").and_then(serde_json::Value::as_str)?.to_owned();
    let name = item.get("name").and_then(serde_json::Value::as_str)?.to_owned();
    let input = item.get("input").cloned();
    let info = ToolCallInfo::from_tool_use(id, name, input);
    Some(MessageBlock::ToolCall(Box::new(info)))
}

fn apply_tool_result(app: &mut App, item: &serde_json::Value) -> bool {
    let Some(tool_use_id) = item.get("tool_use_id").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some((msg_idx, block_idx)) = app.lookup_tool_call(tool_use_id) else {
        return false;
    };
    let Some(message) = app.messages.get_mut(msg_idx) else { return false };
    let Some(MessageBlock::ToolCall(tool_call)) = message.blocks.get_mut(block_idx) else {
        return false;
    };

    let is_error = item
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    if let Some(content) = item.get("content") {
        match content {
            serde_json::Value::String(s) => {
                tool_call.content.push(ToolCallContent::from(s.as_str()));
            }
            serde_json::Value::Array(items) => {
                for entry in items {
                    let kind = entry.get("type").and_then(serde_json::Value::as_str);
                    if kind == Some("text")
                        && let Some(text) = entry.get("text").and_then(serde_json::Value::as_str)
                    {
                        tool_call.content.push(ToolCallContent::from(text));
                    }
                }
            }
            _ => {}
        }
    }

    tool_call.status = if is_error {
        model::ToolCallStatus::Failed
    } else {
        model::ToolCallStatus::Completed
    };
    tool_call.mark_tool_call_render_dirty();
    true
}

/// Build a minimal `state::model::CurrentModel` from a CLI model id
/// (e.g. `"claude-opus-4-7[1m]"`). Only the fields the lifted footer
/// reads (`display_name_short`, `supports_effort`) are populated;
/// everything else carries default-ish values until forge ships
/// a fuller catalog lookup.
#[must_use]
pub fn current_model_from_id(model_id: &str) -> model::CurrentModel {
    let display_name_short = derive_short_name(model_id);
    model::CurrentModel {
        requested_id: None,
        resolved_id: model_id.to_owned(),
        display_name_short: display_name_short.clone(),
        display_name_long: display_name_short,
        catalog_id: None,
        supports_effort: false,
        supported_effort_levels: Vec::new(),
        supports_fast_mode: None,
        supports_auto_mode: None,
        supports_adaptive_thinking: None,
        is_authoritative: true,
    }
}

fn derive_short_name(model_id: &str) -> String {
    let lower = model_id.to_ascii_lowercase();
    if lower.contains("opus") {
        "Opus".to_owned()
    } else if lower.contains("sonnet") {
        "Sonnet".to_owned()
    } else if lower.contains("haiku") {
        "Haiku".to_owned()
    } else {
        model_id.to_owned()
    }
}

/// Adapt a daemon `mcp.status` JSON reply into `Vec<McpServerStatus>`
/// shaped for the lifted footer's `app.mcp.servers`. The daemon emits
/// `serverInfo` / `name` / `status` (camelCase from the SDK); the TUI
/// type uses snake_case, so explicit field-by-field copy.
///
/// Only fields the footer + tool-call cards actually read are
/// populated; the rest default. Returns an empty vec on shape errors.
#[must_use]
pub fn json_to_mcp_servers(value: &serde_json::Value) -> Vec<McpServerStatus> {
    let Some(servers) = value.get("mcp_servers").or_else(|| value.get("mcpServers")) else {
        return Vec::new();
    };
    let Some(arr) = servers.as_array() else { return Vec::new() };
    arr.iter().filter_map(json_to_mcp_server).collect()
}

fn json_to_mcp_server(value: &serde_json::Value) -> Option<McpServerStatus> {
    let name = value
        .get("name")
        .and_then(serde_json::Value::as_str)?
        .to_owned();
    let status = match value.get("status").and_then(serde_json::Value::as_str)? {
        "connected" => McpServerConnectionStatus::Connected,
        "failed" => McpServerConnectionStatus::Failed,
        "needs-auth" | "needsAuth" => McpServerConnectionStatus::NeedsAuth,
        "pending" => McpServerConnectionStatus::Pending,
        "disabled" => McpServerConnectionStatus::Disabled,
        _ => return None,
    };
    let error = value
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let scope = value
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some(McpServerStatus {
        name,
        status,
        server_info: None,
        error,
        config: None,
        scope,
        tools: Vec::new(),
    })
}

/// Adapt a daemon `context.get` JSON reply into a `u8` percent
/// (0–100). Returns `None` when `percentage` is missing or out of range.
#[must_use]
pub fn json_to_context_usage_percent(value: &serde_json::Value) -> Option<u8> {
    let pct = value.get("percentage").and_then(serde_json::Value::as_f64)?;
    if !pct.is_finite() {
        return None;
    }
    let clamped = pct.clamp(0.0, 100.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(clamped.round() as u8)
}

/// Adapt a daemon `sessions.list` array into `Vec<RecentSessionInfo>`
/// so the lifted `ui::lifted::session_picker` can render the list.
///
/// Skips entries missing `session_id` / `summary`. Maps daemon
/// `last_modified` (ms) → `last_modified_ms`, `file_size` → `file_size_bytes`
/// (defaulting to `0` when absent).
#[must_use]
pub fn session_list_to_recent_sessions(items: &[serde_json::Value]) -> Vec<RecentSessionInfo> {
    items.iter().filter_map(json_to_recent_session).collect()
}

fn json_to_recent_session(value: &serde_json::Value) -> Option<RecentSessionInfo> {
    let session_id = value
        .get("session_id")
        .and_then(serde_json::Value::as_str)?
        .to_owned();
    let summary = value
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let last_modified_ms = value
        .get("last_modified")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let file_size_bytes = value
        .get("file_size")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cwd = value
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let git_branch = value
        .get("git_branch")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let custom_title = value
        .get("custom_title")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let first_prompt = value
        .get("first_prompt")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);

    Some(RecentSessionInfo {
        session_id,
        summary,
        last_modified_ms,
        file_size_bytes,
        cwd,
        git_branch,
        custom_title,
        first_prompt,
    })
}

fn text_block_for_role(text: String, role: &MessageRole) -> MessageBlock {
    match role {
        MessageRole::System(severity) => MessageBlock::Notice(
            crate::state::messages::NoticeBlock::new(severity.unwrap_or(SystemSeverity::Info), text),
        ),
        _ => MessageBlock::Text(TextBlock::new(text)),
    }
}
