//! Shared wire-message helpers used by multiple integration test
//! suites that exercise the `SessionUpdate::ChatAppended` path.
//!
//! Extracted from `state_transitions.rs` (Task 4 of the dispatcher
//! collapse refactor; issue #67) so subsequent migration commits
//! (`tool_lifecycle.rs` etc.) can share the same envelope builders.

use forge_tui::agent::model;
use forge_workspace::SessionUpdate;

use crate::helpers::send_client_event;

/// Build a wire `Message::Assistant` envelope carrying the supplied
/// content blocks.
pub fn assistant_message(
    content: Vec<forge_primitives::ContentBlock>,
) -> forge_primitives::Message {
    forge_primitives::Message::Assistant {
        message: forge_primitives::AssistantEnvelope {
            id: "msg_test".to_owned(),
            role: "assistant".to_owned(),
            model: "claude-test".to_owned(),
            content,
            stop_reason: None,
            stop_sequence: None,
            usage: None,
        },
        session_id: "test-session".to_owned(),
        parent_tool_use_id: None,
        error: None,
        uuid: None,
    }
}

/// Build a wire `Message::Assistant` envelope with an explicit
/// `parent_tool_use_id` on the outer envelope. Used by sub-agent
/// child tool tests where the parent linkage is carried at the
/// envelope level (not embedded in tool_use meta).
pub fn assistant_message_with_parent(
    content: Vec<forge_primitives::ContentBlock>,
    parent_tool_use_id: &str,
) -> forge_primitives::Message {
    forge_primitives::Message::Assistant {
        message: forge_primitives::AssistantEnvelope {
            id: "msg_test".to_owned(),
            role: "assistant".to_owned(),
            model: "claude-test".to_owned(),
            content,
            stop_reason: None,
            stop_sequence: None,
            usage: None,
        },
        session_id: "test-session".to_owned(),
        parent_tool_use_id: Some(parent_tool_use_id.to_owned()),
        error: None,
        uuid: None,
    }
}

/// Build a wire `Message::User` envelope carrying the supplied
/// content blocks (typically tool_result blocks).
pub fn user_message(content: Vec<forge_primitives::ContentBlock>) -> forge_primitives::Message {
    forge_primitives::Message::User {
        message: forge_primitives::UserEnvelope { role: "user".to_owned(), content },
        session_id: "test-session".to_owned(),
        parent_tool_use_id: None,
        uuid: None,
        tool_use_result: None,
    }
}

/// Build a wire `Message::System` envelope (`status` / `init` /
/// etc.).
pub fn system_message(subtype: &str, data: serde_json::Value) -> forge_primitives::Message {
    forge_primitives::Message::System {
        subtype: subtype.to_owned(),
        session_id: Some("test-session".to_owned()),
        data,
    }
}

/// Dispatch a wire `Message` envelope. Adopts `"test-session"` as
/// the app's session id on first use so the `ChatAppended`
/// session-id guard accepts the envelope (`test_app()` defaults
/// `session_id` to `None`).
pub fn send_msg(app: &mut forge_tui::app::App, msg: forge_primitives::Message) {
    if app.session_id().is_none() {
        app.set_session_id(Some(model::SessionId::new("test-session")));
    }
    send_client_event(
        app,
        SessionUpdate::ChatAppended { session_id: "test-session".to_owned(), msg },
    );
}

/// Convenience: build a wire `tool_use` content block.
pub fn tool_use_block(
    id: &str,
    name: &str,
    input: serde_json::Value,
) -> forge_primitives::ContentBlock {
    forge_primitives::ContentBlock::ToolUse { id: id.to_owned(), name: name.to_owned(), input }
}

/// Convenience: build a wire `text` content block.
pub fn text_block(text: &str) -> forge_primitives::ContentBlock {
    forge_primitives::ContentBlock::Text { text: text.to_owned() }
}

/// Convenience: build a wire `tool_result` content block (success).
pub fn tool_result_block(
    tool_use_id: &str,
    content: serde_json::Value,
) -> forge_primitives::ContentBlock {
    forge_primitives::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_owned(),
        content,
        is_error: false,
    }
}

/// Convenience: build a wire `tool_result` content block flagged as
/// an error (status maps to "failed" on the receiving end).
pub fn tool_result_error_block(
    tool_use_id: &str,
    content: serde_json::Value,
) -> forge_primitives::ContentBlock {
    forge_primitives::ContentBlock::ToolResult {
        tool_use_id: tool_use_id.to_owned(),
        content,
        is_error: true,
    }
}
