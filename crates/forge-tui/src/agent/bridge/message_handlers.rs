//! Live SDK-message dispatcher. Mirrors upstream's
//! `agent-sdk/src/bridge/message_handlers.ts`. The entry point
//! `handle_sdk_message(&mut BridgeSession, &Message, &mut Vec<BridgeEvent>)`
//! routes each `forge_sdk::Message` variant to the right per-subtype
//! handler.
//!
//! Coverage today (incremental — see plan stages):
//! - Assistant content blocks (text, thinking, tool_use, tool_result)
//!   via `handle_assistant_message`.
//! - User tool_result blocks via `handle_user_tool_result_blocks` plus
//!   `tool_use_result` envelope on parent_tool_use_id.
//! - Result subtype "success" / non-"success" with `terminal_reason`
//!   extraction + `finalize_open_tool_calls` via `handle_result_message`.
//! - System subtypes: api_retry, session_state_changed, compact_boundary,
//!   local_command_output, status, init (partial — slash_commands +
//!   agents fan-out + mode updates).
//! - prompt_suggestion / settings_parse_error / rate_limit_event /
//!   tool_progress / tool_use_summary top-level types.
//!
//! Variants still deferred (later stages): auth_status, stream_event
//! partial-messages, full task_started/progress/notification routing,
//! mcp_auth_redirect.

use forge_sdk::Message as SdkMessage;
use serde_json::{Map, Value};

use crate::agent::types::{ContentBlock, SessionUpdate, TerminalReason};
use crate::agent::wire::BridgeEvent;

use super::commands::refresh_supported_modes_for_session;
use super::session_lifecycle::refresh_current_model;
use super::state::{BridgeSession, PermissionMode};
use super::tool_calls::{
    emit_plan_if_todo_write, emit_tool_call, emit_tool_progress_update, emit_tool_result_update,
    emit_tool_summary_update, finalize_open_tool_calls,
};
use super::tooling::{TOOL_RESULT_TYPES, is_tool_use_block_type, unwrap_tool_use_result};

fn push_session_update(out: &mut Vec<BridgeEvent>, session_id: &str, update: SessionUpdate) {
    out.push(BridgeEvent::SessionUpdate { session_id: session_id.to_owned(), update });
}

// Phase 2: `emit_fast_mode_update_if_changed` removed. The App's
// `events::sdk_message::handle_sdk_message` now applies the
// `fast_mode_state` field directly from the raw `forge_sdk::Message`
// envelope on the `BridgeEvent::SdkMessage` parallel wire.

fn handle_content_block(
    session: &mut BridgeSession,
    block: &Value,
    parent_tool_use_id: Option<&str>,
    out: &mut Vec<BridgeEvent>,
) {
    let Some(block_record) = block.as_object() else { return };
    let block_type = block_record.get("type").and_then(Value::as_str).unwrap_or("");

    // text + thinking blocks moved to App's events::sdk_message
    // walk_assistant_text_and_thinking on the BridgeEvent::SdkMessage
    // parallel wire (Phase 2 cutover).
    if block_type == "text" || block_type == "thinking" {
        return;
    }
    if is_tool_use_block_type(block_type) {
        let Some(tool_use_id) = block_record.get("id").and_then(Value::as_str) else { return };
        if tool_use_id.is_empty() {
            return;
        }
        let name = block_record.get("name").and_then(Value::as_str).unwrap_or("Tool");
        let empty_input = Value::Object(Map::new());
        let input = block_record.get("input").unwrap_or(&empty_input);
        // Plan emission for TodoWrite moved to App's
        // events::sdk_message::apply_plan_if_todo_write on the
        // BridgeEvent::SdkMessage parallel wire (Phase 2 cutover).
        emit_tool_call(session, tool_use_id, name, input, parent_tool_use_id, out);
        return;
    }
    if TOOL_RESULT_TYPES.contains(&block_type) {
        let Some(tool_use_id) = block_record.get("tool_use_id").and_then(Value::as_str) else { return };
        if tool_use_id.is_empty() {
            return;
        }
        let is_error = block_record.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let raw_content = block_record.get("content");
        emit_tool_result_update(session, tool_use_id, is_error, raw_content, Some(block), out);
    }
}

fn handle_assistant_message(
    session: &mut BridgeSession,
    parent_tool_use_id: Option<&str>,
    error: Option<&str>,
    message: &Value,
    out: &mut Vec<BridgeEvent>,
) {
    if let Some(err) = error
        && !err.is_empty()
    {
        session.last_assistant_error = Some(err.to_owned());
    }
    let Some(message_record) = message.as_object() else { return };

    // Text + thinking blocks come through here too via handle_content_block.
    let Some(content) = message_record.get("content").and_then(Value::as_array) else { return };
    for block in content {
        let Some(block_record) = block.as_object() else { continue };
        let block_type = block_record.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(block_type, "text" | "thinking")
            || is_tool_use_block_type(block_type)
            || TOOL_RESULT_TYPES.contains(&block_type)
        {
            handle_content_block(session, block, parent_tool_use_id, out);
        }
    }
}

fn handle_user_tool_result_blocks(
    session: &mut BridgeSession,
    parent_tool_use_id: Option<&str>,
    message: &Value,
    out: &mut Vec<BridgeEvent>,
) {
    let Some(message_record) = message.as_object() else { return };
    let Some(content) = message_record.get("content").and_then(Value::as_array) else { return };
    for block in content {
        let Some(block_record) = block.as_object() else { continue };
        let block_type = block_record.get("type").and_then(Value::as_str).unwrap_or("");
        if TOOL_RESULT_TYPES.contains(&block_type) {
            handle_content_block(session, block, parent_tool_use_id, out);
        }
    }
}

fn terminal_reason_from_value(value: Option<&Value>) -> Option<TerminalReason> {
    serde_json::from_value(value?.clone()).ok()
}

fn handle_result_message(
    session: &mut BridgeSession,
    is_error: bool,
    subtype: &str,
    _fast_mode_state: Option<&Value>,
    terminal_reason: Option<&Value>,
    errors: Option<&Value>,
    out: &mut Vec<BridgeEvent>,
) {
    // fast_mode_state is now applied by the App's sdk_message handler
    // on the parallel BridgeEvent::SdkMessage wire (Phase 2 cutover).
    let terminal_reason_typed = terminal_reason_from_value(terminal_reason);

    if !is_error && subtype == "success" {
        session.last_assistant_error = None;
        finalize_open_tool_calls(session, "completed", out);
        out.push(BridgeEvent::TurnComplete {
            session_id: session.session_id.clone(),
            terminal_reason: terminal_reason_typed,
        });
        return;
    }

    let error_strings: Vec<String> = errors
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let assistant_error = session.last_assistant_error.clone();
    finalize_open_tool_calls(session, "failed", out);
    let message = if error_strings.is_empty() {
        if subtype.is_empty() { "turn failed".to_owned() } else { format!("turn failed: {subtype}") }
    } else {
        error_strings.join("\n")
    };
    let error_kind = classify_turn_error_kind(subtype, &error_strings, assistant_error.as_deref());
    out.push(BridgeEvent::TurnError {
        session_id: session.session_id.clone(),
        message,
        error_kind: Some(error_kind.to_owned()),
        sdk_result_subtype: if subtype.is_empty() { None } else { Some(subtype.to_owned()) },
        assistant_error,
        terminal_reason: terminal_reason_typed,
    });
    session.last_assistant_error = None;
}

fn looks_like_auth_required(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("authentication_failed")
        || lower.contains("not authenticated")
        || lower.contains("authentication required")
        || lower.contains("unauthenticated")
        || (lower.contains("401") && lower.contains("auth"))
}

fn classify_turn_error_kind(
    subtype: &str,
    errors: &[String],
    assistant_error: Option<&str>,
) -> &'static str {
    let plan_limit_signals = [
        "error_max_turns",
        "error_max_budget_usd",
        "billing_error",
        "rate_limit",
    ];
    if plan_limit_signals.iter().any(|s| subtype.contains(s)) {
        return "plan_limit";
    }
    if errors.iter().any(|e| plan_limit_signals.iter().any(|s| e.contains(s))) {
        return "plan_limit";
    }
    if assistant_error == Some("authentication_failed") {
        return "auth_required";
    }
    if errors.iter().any(|e| looks_like_auth_required(e)) {
        return "auth_required";
    }
    if assistant_error == Some("server_error") {
        return "internal";
    }
    "other"
}

fn handle_system_init(
    session: &mut BridgeSession,
    incoming_session_id: Option<&str>,
    msg_record: &Map<String, Value>,
    out: &mut Vec<BridgeEvent>,
) {
    if let Some(sid) = incoming_session_id
        && !sid.is_empty()
        && sid != session.session_id
    {
        sid.clone_into(&mut session.session_id);
    }
    if let Some(model) = msg_record.get("model").and_then(Value::as_str) {
        model.clone_into(&mut session.model_id);
    }
    // Internal session.current_model still tracked here so other bridge
    // readers (commands.rs auto-mode lookup, etc.) see fresh values.
    // The CurrentModelUpdate emission moved to the App's
    // events::sdk_message::apply_current_model_from_init.
    let _ = refresh_current_model(session, false, &mut Vec::new());
    if let Some(mode_str) = msg_record.get("permissionMode").and_then(Value::as_str)
        && let Some(mode) = PermissionMode::from_wire(mode_str)
    {
        session.mode = Some(mode);
    }
    refresh_supported_modes_for_session(session);
    // fast_mode_state moved to the App's sdk_message handler.

    // ModeStateUpdate moved to App's
    // events::sdk_message::apply_mode_state_from_init on the
    // BridgeEvent::SdkMessage parallel wire (Phase 2 cutover).
    let _ = out;

    // AvailableCommandsUpdate + AvailableAgentsUpdate moved to App's
    // events::sdk_message::handle_system init arm.

    // settings_errors moved to App's events::sdk_message::apply_settings_parse_errors
    // on the BridgeEvent::SdkMessage parallel wire (Phase 2 cutover).
}

fn handle_system_status(
    session: &mut BridgeSession,
    msg_record: &Map<String, Value>,
    _out: &mut Vec<BridgeEvent>,
) {
    if let Some(mode_str) = msg_record.get("permissionMode").and_then(Value::as_str)
        && let Some(mode) = PermissionMode::from_wire(mode_str)
    {
        session.mode = Some(mode);
        refresh_supported_modes_for_session(session);
        // CurrentModeUpdate moved to App's events::sdk_message handle_system status arm.
    }
    // SessionStatusUpdate (Compacting/Idle) and fast_mode_state both
    // moved to the App's events::sdk_message::handle_system status arm.
}

// handle_system_compact_boundary moved to App's
// events::sdk_message::apply_compaction_boundary on the
// BridgeEvent::SdkMessage parallel wire.

/// Top-level dispatcher. Routes one `forge_sdk::Message` to its
/// per-subtype handler. The bridge session captures continuous state
/// (open tool_calls, last assistant error, fast mode, supported modes)
/// across messages.
#[allow(clippy::too_many_lines)]
pub fn handle_sdk_message(
    session: &mut BridgeSession,
    msg: &SdkMessage,
    out: &mut Vec<BridgeEvent>,
) {
    match msg {
        SdkMessage::Assistant { message, parent_tool_use_id, error, .. } => {
            // Wrap the typed envelope back into a JSON record so the
            // generic content_block walker can handle every block type.
            // The `error` field travels with the outer envelope, not the
            // message body.
            let value = serde_json::to_value(message).unwrap_or(Value::Null);
            let error_str = error.as_ref().map(|e| serde_json::to_value(e).ok()
                .and_then(|v| v.as_str().map(str::to_owned)))
                .and_then(|opt| opt);
            handle_assistant_message(
                session,
                parent_tool_use_id.as_deref(),
                error_str.as_deref(),
                &value,
                out,
            );
        }
        SdkMessage::User { message, parent_tool_use_id, tool_use_result, .. } => {
            let msg_value = serde_json::to_value(message).unwrap_or(Value::Null);
            handle_user_tool_result_blocks(
                session,
                parent_tool_use_id.as_deref(),
                &msg_value,
                out,
            );
            // `tool_use_result` is a CLI-emitted side payload for
            // sub-agent results (parent_tool_use_id present).
            if let Some(result) = tool_use_result
                && let Some(tool_use_id) = parent_tool_use_id.as_deref()
                && !tool_use_id.is_empty()
            {
                let parsed = unwrap_tool_use_result(result);
                emit_tool_result_update(
                    session,
                    tool_use_id,
                    parsed.is_error,
                    Some(&parsed.content),
                    Some(result),
                    out,
                );
            }
        }
        SdkMessage::Result {
            subtype,
            is_error,
            ..
        } => {
            // Walk the raw JSON for terminal_reason / errors /
            // fast_mode_state since those aren't typed fields on
            // forge-sdk's Result variant today.
            let raw = serde_json::to_value(msg).unwrap_or(Value::Null);
            let raw_record = raw.as_object();
            handle_result_message(
                session,
                *is_error,
                subtype,
                raw_record.and_then(|r| r.get("fast_mode_state")),
                raw_record.and_then(|r| r.get("terminal_reason")),
                raw_record.and_then(|r| r.get("errors")),
                out,
            );
        }
        SdkMessage::System { subtype, session_id: msg_session_id, data } => {
            let msg_record = data.as_object();
            let Some(msg_record) = msg_record else { return };
            match subtype.as_str() {
                "api_retry" => {
                    // Phase 2 cutover: handled by App's
                    // events::sdk_message::handle_system.
                }
                "session_state_changed" => {
                    // Phase 2 cutover: handled by App's
                    // events::sdk_message::handle_system on the
                    // BridgeEvent::SdkMessage parallel wire.
                }
                "init" => {
                    handle_system_init(session, msg_session_id.as_deref(), msg_record, out);
                }
                "status" => {
                    handle_system_status(session, msg_record, out);
                }
                "compact_boundary" => {
                    // CompactionBoundary moved to App handler.
                }
                "local_command_output" => {
                    let content = msg_record.get("content").and_then(Value::as_str).unwrap_or("");
                    if !content.trim().is_empty() {
                        push_session_update(
                            out,
                            &session.session_id,
                            SessionUpdate::AgentMessageChunk {
                                content: ContentBlock::Text { text: content.to_owned() },
                            },
                        );
                    }
                }
                "elicitation_complete" => {
                    // Phase 2 cutover: handled by App's
                    // events::sdk_message::apply_elicitation_complete on
                    // the BridgeEvent::SdkMessage parallel wire.
                }
                _ => {
                    // task_started/progress/notification fall here in
                    // the upstream port; they need state coordination
                    // (taskToolUseIds map, tool_call lifecycle) wired
                    // in a follow-up. Hook is a no-op for now.
                }
            }
        }
        SdkMessage::RateLimitEvent { .. } => {
            // Phase 2 cutover: handled by App's
            // events::sdk_message::handle_rate_limit_event on the
            // BridgeEvent::SdkMessage parallel wire.
        }
        SdkMessage::TaskStarted { tool_use_id, task_id, .. } => {
            let id = tool_use_id.as_deref().unwrap_or("");
            if !id.is_empty() {
                emit_tool_progress_update(session, id, "Task", out);
                if !task_id.is_empty() {
                    session.task_tool_use_ids.insert(task_id.clone(), id.to_owned());
                }
            }
        }
        SdkMessage::TaskProgress { tool_use_id, .. } => {
            let id = tool_use_id.as_deref().unwrap_or("");
            if !id.is_empty() {
                emit_tool_progress_update(session, id, "Task", out);
            }
        }
        SdkMessage::TaskNotification { tool_use_id, summary, .. } => {
            let id = tool_use_id.as_deref().unwrap_or("");
            if !id.is_empty() {
                emit_tool_summary_update(session, id, summary, out);
            }
        }
        // StreamEvent + Error + Unknown are no-ops for now; future
        // stages can layer in.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::wire::BridgeEvent;
    use forge_sdk::{AssistantEnvelope, Message as SdkMessage};
    use serde_json::json;

    fn fresh_session() -> BridgeSession {
        BridgeSession::new("sess".to_owned(), "/tmp".to_owned())
    }

    fn assistant_msg(blocks: &serde_json::Value) -> SdkMessage {
        let env: AssistantEnvelope = serde_json::from_value(json!({
            "id": "msg_x",
            "role": "assistant",
            "model": "claude-sonnet-4-6",
            "content": blocks,
        }))
        .unwrap();
        SdkMessage::Assistant {
            message: env,
            session_id: "sess".to_owned(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
        }
    }

    #[test]
    fn assistant_text_no_longer_emits_chunk_from_bridge() {
        // Phase 2 cut: text + thinking blocks moved to App's
        // events::sdk_message::walk_assistant_text_and_thinking on the
        // BridgeEvent::SdkMessage parallel wire. The bridge's
        // handle_content_block now ignores them.
        let mut s = fresh_session();
        let mut out = Vec::new();
        let msg = assistant_msg(&json!([{"type":"text","text":"hi"}]));
        handle_sdk_message(&mut s, &msg, &mut out);
        assert!(
            out.is_empty(),
            "bridge should no longer emit AgentMessageChunk for text blocks"
        );
    }

    #[test]
    fn assistant_tool_use_then_user_tool_result_pairs() {
        let mut s = fresh_session();
        let mut out = Vec::new();
        // First, assistant emits a tool_use.
        let msg = assistant_msg(&json!([{
            "type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}
        }]));
        handle_sdk_message(&mut s, &msg, &mut out);
        assert!(s.tool_calls.contains_key("tu1"));
        let tu_calls_before = out.iter().filter(|e| matches!(e, BridgeEvent::SessionUpdate { update: SessionUpdate::ToolCall { .. }, .. })).count();
        assert_eq!(tu_calls_before, 1);
        out.clear();

        // Then user message with a tool_result block referring to tu1.
        let user_envelope: forge_sdk::UserEnvelope = serde_json::from_value(json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "tu1",
                "is_error": false,
                "content": "stdout text",
            }]
        }))
        .unwrap();
        let user_msg = SdkMessage::User {
            message: user_envelope,
            session_id: "sess".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        };
        handle_sdk_message(&mut s, &user_msg, &mut out);
        let updates: Vec<_> = out
            .iter()
            .filter_map(|e| match e {
                BridgeEvent::SessionUpdate { update: SessionUpdate::ToolCallUpdate { tool_call_update }, .. } => {
                    Some(tool_call_update)
                }
                _ => None,
            })
            .collect();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].tool_call_id, "tu1");
        assert_eq!(updates[0].fields.status.as_deref(), Some("completed"));
    }

    #[test]
    fn result_success_emits_turn_complete_and_finalizes_open_tools() {
        let mut s = fresh_session();
        let mut out = Vec::new();
        // Open a tool.
        let msg = assistant_msg(&json!([{
            "type":"tool_use","id":"tu1","name":"Bash","input":{"command":"sleep 1"}
        }]));
        handle_sdk_message(&mut s, &msg, &mut out);
        out.clear();

        let result: SdkMessage = serde_json::from_value(json!({
            "type": "result",
            "subtype": "success",
            "session_id": "sess",
            "is_error": false,
            "num_turns": 1,
            "duration_ms": 100,
            "duration_api_ms": 80,
        }))
        .unwrap();
        handle_sdk_message(&mut s, &result, &mut out);
        let has_finalize = out.iter().any(|e| matches!(e,
            BridgeEvent::SessionUpdate { update: SessionUpdate::ToolCallUpdate { tool_call_update }, .. }
                if tool_call_update.fields.status.as_deref() == Some("completed") && tool_call_update.tool_call_id == "tu1"));
        assert!(has_finalize, "expected tu1 finalize");
        let has_turn = out.iter().any(|e| matches!(e, BridgeEvent::TurnComplete { .. }));
        assert!(has_turn, "expected TurnComplete");
    }

    #[test]
    fn result_error_emits_turn_error_with_classify() {
        let mut s = fresh_session();
        let mut out = Vec::new();
        let result: SdkMessage = serde_json::from_value(json!({
            "type": "result",
            "subtype": "error_max_turns",
            "session_id": "sess",
            "is_error": true,
            "num_turns": 5,
            "duration_ms": 1,
            "duration_api_ms": 1,
        }))
        .unwrap();
        handle_sdk_message(&mut s, &result, &mut out);
        let BridgeEvent::TurnError { error_kind, .. } = out.last().unwrap() else {
            panic!("expected TurnError");
        };
        assert_eq!(error_kind.as_deref(), Some("plan_limit"));
    }

    #[test]
    fn classify_turn_error_kind_table() {
        assert_eq!(classify_turn_error_kind("error_max_turns", &[], None), "plan_limit");
        assert_eq!(classify_turn_error_kind("error_max_budget_usd", &[], None), "plan_limit");
        assert_eq!(classify_turn_error_kind("billing_error", &[], None), "plan_limit");
        assert_eq!(
            classify_turn_error_kind("internal", &[], Some("authentication_failed")),
            "auth_required"
        );
        assert_eq!(
            classify_turn_error_kind("internal", &[], Some("server_error")),
            "internal"
        );
        assert_eq!(classify_turn_error_kind("anything_else", &[], None), "other");
        assert_eq!(
            classify_turn_error_kind("internal", &["401: authentication required".to_owned()], None),
            "auth_required"
        );
    }
}
