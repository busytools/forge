//! Inbound translator: forge-daemon `InboundEvent` -> upstream
//! `BridgeEvent`.
//!
//! The daemon emits a leaner wire shape than upstream's Node.js bridge
//! used to. This module synthesises the rich `BridgeEvent` values the
//! lifted UI expects (permission requests with `ToolCall` + options +
//! display, question requests with `QuestionPrompt`, etc.).
//!
//! Functions here are pure: they take `InboundEvent` (or its parts)
//! plus side-channel state (the `reverse_lookup` map keyed by
//! `tool_use_id` -> JSON-RPC request id) and return zero, one, or
//! many `EventEnvelope`s. A single `session.event` notification (an
//! SDK [`forge_sdk::Message`]) commonly fans out into several
//! `SessionUpdate` envelopes — one per content block.

#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::sync::Arc;

use forge_sdk::{AssistantEnvelope, ContentBlock as SdkContentBlock, Message as SdkMessage};
use parking_lot::Mutex;
use serde_json::Value;

use crate::agent::bridge::InboundEvent;
use crate::agent::types::{
    AccountInfo, ContentBlock as TuiContentBlock, CurrentModel, PermissionDisplay,
    PermissionOption, PermissionRequest, QuestionOption, QuestionPrompt, QuestionRequest,
    SessionUpdate, ToolCall,
};
use crate::agent::wire::{BridgeEvent, EventEnvelope};

/// Map tracking reverse-RPC ids by their tool-side correlation key.
/// The writer's `PermissionResponse` / `QuestionResponse` /
/// `ElicitationResponse` branches consume this when sending replies.
pub type ReverseLookup = Arc<Mutex<HashMap<String, Value>>>;

/// Translate one inbound daemon event to zero, one, or many
/// `BridgeEvent`s. An empty Vec means "ignore this event" — already
/// logged at debug/warn-level inside helper functions.
///
/// Side effect: may insert into `reverse_lookup` when the event is a
/// reverse-RPC request whose response will need to be matched back to
/// its JSON-RPC id.
pub fn translate(event: InboundEvent, reverse_lookup: &ReverseLookup) -> Vec<EventEnvelope> {
    match (event.id, event.method.as_str()) {
        (Some(rev_id), "permission.request") => {
            translate_permission_request(rev_id, &event.params, reverse_lookup)
                .into_iter()
                .collect()
        }
        (Some(rev_id), "session.question_request") => {
            translate_question_request(rev_id, &event.params, reverse_lookup)
                .into_iter()
                .collect()
        }
        (_, "session.closed") => translate_session_closed(&event.params)
            .into_iter()
            .collect(),
        (None, "session.event") => translate_session_event(&event.params),
        (id, method) => {
            tracing::debug!(?id, %method, "translate: ignoring inbound event");
            Vec::new()
        }
    }
}

/// `permission.request` reverse-RPC: synthesise a `PermissionRequest`
/// from the daemon's lean `{tool_name, tool_input, context}` shape.
/// Wraps `params` is `{session_id, prompt_id, params: {tool_name, ...}}`.
fn translate_permission_request(
    rev_id: Value,
    params: &Value,
    reverse_lookup: &ReverseLookup,
) -> Option<EventEnvelope> {
    let session_id = params.get("session_id").and_then(Value::as_str)?.to_owned();
    let inner = params.get("params")?;
    let tool_name = inner
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let tool_input = inner.get("tool_input").cloned();
    let context = inner.get("context");
    let tool_use_id = context
        .and_then(|c| c.get("tool_use_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let suggestions = context
        .and_then(|c| c.get("suggestions"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if tool_use_id.is_empty() {
        tracing::warn!("permission.request: missing tool_use_id; reverse_lookup not populated");
    } else {
        reverse_lookup.lock().insert(tool_use_id.clone(), rev_id);
    }

    let request = PermissionRequest {
        tool_call: synth_tool_call(tool_use_id, &tool_name, tool_input.as_ref()),
        options: synth_permission_options(&suggestions),
        display: Some(synth_permission_display(&tool_name)),
    };
    Some(EventEnvelope {
        request_id: None,
        event: BridgeEvent::PermissionRequest {
            session_id,
            request,
        },
    })
}

/// `session.question_request` reverse-RPC: synthesise a
/// `QuestionRequest` from `{tool_use_id, questions}`.
fn translate_question_request(
    rev_id: Value,
    params: &Value,
    reverse_lookup: &ReverseLookup,
) -> Option<EventEnvelope> {
    let session_id = params.get("session_id").and_then(Value::as_str)?.to_owned();
    let inner = params.get("params")?;
    let tool_use_id = inner
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let questions = inner
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if !tool_use_id.is_empty() {
        reverse_lookup.lock().insert(tool_use_id.clone(), rev_id);
    }

    // AskUserQuestion ships a `questions` array; the TUI shows them
    // one at a time (upstream's `question_index` / `total_questions`
    // surface paging). For the first emission we surface question 0;
    // subsequent ones are driven by the UI lift, which is why the
    // index/total fields are filled but conservative here.
    let total = u64::try_from(questions.len()).unwrap_or(0);
    let prompt = questions
        .first()
        .and_then(parse_question_prompt)
        .unwrap_or_else(empty_question_prompt);

    let request = QuestionRequest {
        tool_call: synth_tool_call(tool_use_id, "AskUserQuestion", None),
        prompt,
        question_index: 0,
        total_questions: total,
    };
    Some(EventEnvelope {
        request_id: None,
        event: BridgeEvent::QuestionRequest {
            session_id,
            request,
        },
    })
}

/// `session.closed` notification: surface as a `TurnError` with the
/// daemon-supplied reason. Upstream has no "session closed" event;
/// the closest equivalent is a terminal error on the active turn.
fn translate_session_closed(params: &Value) -> Option<EventEnvelope> {
    let session_id = params.get("session_id").and_then(Value::as_str)?.to_owned();
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("session closed")
        .to_owned();
    Some(EventEnvelope {
        request_id: None,
        event: BridgeEvent::TurnError {
            session_id,
            message: reason,
            error_kind: Some("session_closed".to_owned()),
            sdk_result_subtype: None,
            assistant_error: None,
            terminal_reason: None,
        },
    })
}

/// `session.event` notification: decode the inner SDK `Message` and
/// fan out into the corresponding `BridgeEvent` shape(s).
///
/// - `Assistant` -> one `SessionUpdate` per content block (text +
///   thinking land as chunks; `tool_use` blocks become `ToolCall`s).
/// - `Result` -> `TurnComplete` (or `TurnError` when `is_error`).
/// - `User`, `System`, `TaskStarted/Progress/Notification`,
///   `RateLimitEvent`, `StreamEvent`, `Error`, `Unknown` -> dropped
///   for now; structured handling lands when the lifted UI starts
///   consuming each variant.
fn translate_session_event(params: &Value) -> Vec<EventEnvelope> {
    let Some(session_id) = params
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        tracing::warn!(?params, "session.event: missing session_id");
        return Vec::new();
    };
    let Some(message_value) = params.get("message") else {
        tracing::warn!("session.event: missing message field");
        return Vec::new();
    };
    let message: SdkMessage = match serde_json::from_value(message_value.clone()) {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(error = %err, "session.event: failed to deserialize SDK Message");
            return Vec::new();
        }
    };

    match message {
        SdkMessage::Assistant {
            message: envelope, ..
        } => assistant_to_envelopes(&session_id, &envelope),
        SdkMessage::Result {
            subtype, is_error, ..
        } => {
            let event = if is_error {
                BridgeEvent::TurnError {
                    session_id,
                    message: subtype.clone(),
                    error_kind: Some(subtype.clone()),
                    sdk_result_subtype: Some(subtype),
                    assistant_error: None,
                    terminal_reason: None,
                }
            } else {
                BridgeEvent::TurnComplete {
                    session_id,
                    terminal_reason: None,
                }
            };
            vec![EventEnvelope {
                request_id: None,
                event,
            }]
        }
        _ => {
            tracing::debug!("session.event: variant not yet translated");
            Vec::new()
        }
    }
}

fn assistant_to_envelopes(session_id: &str, envelope: &AssistantEnvelope) -> Vec<EventEnvelope> {
    envelope
        .content
        .iter()
        .filter_map(|block| {
            content_block_to_update(block).map(|update| EventEnvelope {
                request_id: None,
                event: BridgeEvent::SessionUpdate {
                    session_id: session_id.to_owned(),
                    update,
                },
            })
        })
        .collect()
}

fn content_block_to_update(block: &SdkContentBlock) -> Option<SessionUpdate> {
    match block {
        SdkContentBlock::Text { text } => Some(SessionUpdate::AgentMessageChunk {
            content: TuiContentBlock::Text { text: text.clone() },
        }),
        SdkContentBlock::Thinking { thinking, .. } => Some(SessionUpdate::AgentThoughtChunk {
            content: TuiContentBlock::Text {
                text: thinking.clone(),
            },
        }),
        SdkContentBlock::ToolUse { id, name, input } => Some(SessionUpdate::ToolCall {
            tool_call: synth_tool_call(id.clone(), name, Some(input)),
        }),
        // ToolResult, ServerToolUse/Result, Document and Unknown
        // blocks are not surfaced to the UI yet — the lifted UI will
        // pick them up when its renderers learn the shapes.
        _ => None,
    }
}

fn synth_tool_call(tool_call_id: String, tool_name: &str, raw_input: Option<&Value>) -> ToolCall {
    ToolCall {
        tool_call_id,
        title: tool_name.to_owned(),
        kind: "execute".to_owned(),
        status: "pending".to_owned(),
        content: Vec::new(),
        raw_input: raw_input.cloned(),
        raw_output: None,
        output_metadata: None,
        task_metadata: None,
        locations: Vec::new(),
        meta: None,
    }
}

fn synth_permission_options(suggestions: &[Value]) -> Vec<PermissionOption> {
    if suggestions.is_empty() {
        return default_permission_options();
    }
    suggestions
        .iter()
        .filter_map(|s| {
            let option_id = s.get("option_id").and_then(Value::as_str)?.to_owned();
            let name = s
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&option_id)
                .to_owned();
            let description = s
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let kind = s
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("allow_once")
                .to_owned();
            Some(PermissionOption {
                option_id,
                name,
                description,
                kind,
            })
        })
        .collect()
}

fn default_permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            option_id: "allow_once".to_owned(),
            name: "Allow once".to_owned(),
            description: None,
            kind: "allow_once".to_owned(),
        },
        PermissionOption {
            option_id: "allow_always".to_owned(),
            name: "Allow always".to_owned(),
            description: None,
            kind: "allow_always".to_owned(),
        },
        PermissionOption {
            option_id: "deny".to_owned(),
            name: "Deny".to_owned(),
            description: None,
            kind: "reject_once".to_owned(),
        },
    ]
}

fn synth_permission_display(tool_name: &str) -> PermissionDisplay {
    PermissionDisplay {
        title: Some(format!("Allow {tool_name}?")),
        display_name: Some(tool_name.to_owned()),
        description: None,
    }
}

fn parse_question_prompt(value: &Value) -> Option<QuestionPrompt> {
    let question = value.get("question")?.as_str()?.to_owned();
    let header = value
        .get("header")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let multi_select = value
        .get("multiSelect")
        .or_else(|| value.get("multi_select"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let options = value
        .get("options")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(parse_question_option).collect())
        .unwrap_or_default();
    Some(QuestionPrompt {
        question,
        header,
        multi_select,
        options,
    })
}

fn parse_question_option(value: &Value) -> Option<QuestionOption> {
    // Upstream emits both `option_id` (snake) and `optionId` (camel) in
    // different paths; accept either.
    let option_id = value
        .get("option_id")
        .or_else(|| value.get("optionId"))
        .and_then(Value::as_str)?
        .to_owned();
    let label = value
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or(&option_id)
        .to_owned();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let preview = value
        .get("preview")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(QuestionOption {
        option_id,
        label,
        description,
        preview,
    })
}

fn empty_question_prompt() -> QuestionPrompt {
    QuestionPrompt {
        question: String::new(),
        header: String::new(),
        multi_select: false,
        options: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Outbound-response decoders.
//
// These translate the JSON `result` value from a daemon RPC call into
// the matching `BridgeEvent` for the TUI. They run inside the writer
// loop after `conn.call(...)` returns; the writer threads the result
// through `event_tx` the same way the reader does for notifications.
// Functions are pure; tests drive them with synthesised JSON.
// ---------------------------------------------------------------------------

/// `session.spawn` response (`{session_id}`) -> `BridgeEvent::Connected`.
/// `request_cwd` is the cwd from the original `BridgeCommand::NewSession`
/// / `CreateSession` / `ResumeSession` (the response doesn't include it).
/// `current_model` is left as a placeholder; the daemon will emit a
/// `SessionUpdate::CurrentModelUpdate` once the CLI's `system/init`
/// message arrives via `session.event`.
#[must_use]
pub fn decode_spawn_response(
    value: &Value,
    request_cwd: &str,
    request_id: Option<String>,
) -> Option<EventEnvelope> {
    let session_id = value.get("session_id").and_then(Value::as_str)?.to_owned();
    Some(EventEnvelope {
        request_id,
        event: BridgeEvent::Connected {
            session_id,
            cwd: request_cwd.to_owned(),
            current_model: placeholder_current_model(),
            available_models: Vec::new(),
            mode: None,
            history_updates: None,
        },
    })
}

/// `session.status_snapshot` response -> `BridgeEvent::StatusSnapshot`.
/// The daemon's `AccountSnapshot` is wire-compatible with TUI's
/// `AccountInfo` (`snake_case`, all-Option fields), so direct
/// deserialisation is sufficient.
#[must_use]
pub fn decode_status_snapshot(
    value: &Value,
    session_id: &str,
    request_id: Option<String>,
) -> Option<EventEnvelope> {
    let account: AccountInfo = match serde_json::from_value(value.clone()) {
        Ok(a) => a,
        Err(err) => {
            tracing::warn!(error = %err, "decode_status_snapshot: deserialize failed");
            return None;
        }
    };
    Some(EventEnvelope {
        request_id,
        event: BridgeEvent::StatusSnapshot {
            session_id: session_id.to_owned(),
            account,
        },
    })
}

/// `context.get` response -> `BridgeEvent::ContextUsage`. Only the
/// `percentage` field flows to the TUI; the rest of `ContextUsageResponse`
/// is daemon-internal detail.
#[must_use]
pub fn decode_context_usage(
    value: &Value,
    session_id: &str,
    request_id: Option<String>,
) -> Option<EventEnvelope> {
    let percentage = value
        .get("percentage")
        .and_then(Value::as_f64)
        .map(clamp_percentage_to_u8);
    Some(EventEnvelope {
        request_id,
        event: BridgeEvent::ContextUsage {
            session_id: session_id.to_owned(),
            percentage,
        },
    })
}

/// Clamp a 0..=100 floating-point percentage into a `u8`. Out-of-range
/// values get clamped to the nearest endpoint; NaN flows to 0.
fn clamp_percentage_to_u8(p: f64) -> u8 {
    if p.is_nan() {
        return 0;
    }
    let clamped = p.clamp(0.0, 100.0).round();
    // Safe: clamped is in [0.0, 100.0] post-clamp, so the cast is total.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let n = clamped as u8;
    n
}

fn placeholder_current_model() -> CurrentModel {
    CurrentModel {
        requested_id: None,
        resolved_id: String::new(),
        display_name_short: String::new(),
        display_name_long: String::new(),
        catalog_id: None,
        supports_effort: false,
        supported_effort_levels: Vec::new(),
        supports_fast_mode: None,
        supports_auto_mode: None,
        supports_adaptive_thinking: None,
        is_authoritative: false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    fn fresh_lookup() -> ReverseLookup {
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn one(envelopes: Vec<EventEnvelope>) -> EventEnvelope {
        assert_eq!(envelopes.len(), 1, "expected exactly one envelope");
        envelopes.into_iter().next().unwrap()
    }

    #[test]
    fn permission_request_synthesises_event_and_populates_lookup() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: Some(json!("rev_abc")),
            method: "permission.request".to_owned(),
            params: json!({
                "session_id": "sess_1",
                "prompt_id": "prompt_1",
                "params": {
                    "tool_name": "Bash",
                    "tool_input": {"command": "ls"},
                    "context": {
                        "tool_use_id": "tu_1",
                        "agent_id": "agent_a",
                        "suggestions": [],
                    },
                },
            }),
        };

        let envelope = one(translate(event, &lookup));
        let BridgeEvent::PermissionRequest {
            session_id,
            request,
        } = envelope.event
        else {
            panic!("wrong variant: {:?}", envelope.event);
        };
        assert_eq!(session_id, "sess_1");
        assert_eq!(request.tool_call.tool_call_id, "tu_1");
        assert_eq!(request.tool_call.title, "Bash");
        assert_eq!(request.tool_call.raw_input, Some(json!({"command": "ls"})));
        assert_eq!(request.options.len(), 3); // defaults
        assert!(request.display.is_some());

        // Reverse lookup populated for the writer.
        assert_eq!(lookup.lock().get("tu_1").cloned(), Some(json!("rev_abc")));
    }

    #[test]
    fn permission_request_uses_suggestions_when_provided() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: Some(json!("rev_xyz")),
            method: "permission.request".to_owned(),
            params: json!({
                "session_id": "sess_1",
                "prompt_id": "p",
                "params": {
                    "tool_name": "Bash",
                    "tool_input": {},
                    "context": {
                        "tool_use_id": "tu_2",
                        "agent_id": "a",
                        "suggestions": [
                            {"option_id": "trust", "name": "Trust", "kind": "allow_always"},
                        ],
                    },
                },
            }),
        };

        let envelope = one(translate(event, &lookup));
        let BridgeEvent::PermissionRequest { request, .. } = envelope.event else {
            panic!();
        };
        assert_eq!(request.options.len(), 1);
        assert_eq!(request.options[0].option_id, "trust");
        assert_eq!(request.options[0].kind, "allow_always");
    }

    #[test]
    fn question_request_synthesises_event_with_first_question() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: Some(json!("rev_q")),
            method: "session.question_request".to_owned(),
            params: json!({
                "session_id": "sess_1",
                "prompt_id": "p",
                "params": {
                    "tool_use_id": "tu_q",
                    "questions": [
                        {
                            "question": "Pick a colour",
                            "header": "Colour",
                            "multiSelect": false,
                            "options": [
                                {"option_id": "red", "label": "Red"},
                                {"option_id": "blue", "label": "Blue"},
                            ],
                        },
                        {
                            "question": "Pick a size",
                            "header": "Size",
                        },
                    ],
                },
            }),
        };

        let envelope = one(translate(event, &lookup));
        let BridgeEvent::QuestionRequest {
            session_id,
            request,
        } = envelope.event
        else {
            panic!();
        };
        assert_eq!(session_id, "sess_1");
        assert_eq!(request.tool_call.tool_call_id, "tu_q");
        assert_eq!(request.prompt.question, "Pick a colour");
        assert_eq!(request.prompt.header, "Colour");
        assert!(!request.prompt.multi_select);
        assert_eq!(request.prompt.options.len(), 2);
        assert_eq!(request.prompt.options[0].option_id, "red");
        assert_eq!(request.question_index, 0);
        assert_eq!(request.total_questions, 2);

        assert_eq!(lookup.lock().get("tu_q").cloned(), Some(json!("rev_q")));
    }

    #[test]
    fn session_closed_emits_turn_error() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: None,
            method: "session.closed".to_owned(),
            params: json!({"session_id": "sess_1", "reason": "result_frame"}),
        };
        let envelope = one(translate(event, &lookup));
        let BridgeEvent::TurnError {
            session_id,
            message,
            error_kind,
            ..
        } = envelope.event
        else {
            panic!();
        };
        assert_eq!(session_id, "sess_1");
        assert_eq!(message, "result_frame");
        assert_eq!(error_kind.as_deref(), Some("session_closed"));
    }

    #[test]
    fn unknown_method_returns_empty() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: None,
            method: "unrelated.notification".to_owned(),
            params: json!({}),
        };
        assert!(translate(event, &lookup).is_empty());
    }

    #[test]
    fn permission_request_missing_session_id_drops_event() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: Some(json!("rev_missing")),
            method: "permission.request".to_owned(),
            params: json!({
                "params": {"tool_name": "X", "tool_input": {}, "context": {"tool_use_id": "x"}},
            }),
        };
        assert!(translate(event, &lookup).is_empty());
        assert!(lookup.lock().is_empty());
    }

    #[test]
    fn session_event_assistant_emits_chunk_and_tool_call() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: None,
            method: "session.event".to_owned(),
            params: json!({
                "session_id": "sess_1",
                "event_id": "evt_1",
                "message": {
                    "type": "assistant",
                    "session_id": "sess_1",
                    "message": {
                        "id": "msg_1",
                        "role": "assistant",
                        "model": "claude-opus-4-7",
                        "content": [
                            {"type": "text", "text": "Hello"},
                            {
                                "type": "tool_use",
                                "id": "tu_x",
                                "name": "Bash",
                                "input": {"command": "ls"},
                            },
                        ],
                    },
                },
            }),
        };
        let envelopes = translate(event, &lookup);
        assert_eq!(envelopes.len(), 2);
        let BridgeEvent::SessionUpdate { update, .. } = &envelopes[0].event else {
            panic!();
        };
        let SessionUpdate::AgentMessageChunk { content } = update else {
            panic!();
        };
        let TuiContentBlock::Text { text } = content else {
            panic!();
        };
        assert_eq!(text, "Hello");

        let BridgeEvent::SessionUpdate { update, .. } = &envelopes[1].event else {
            panic!();
        };
        let SessionUpdate::ToolCall { tool_call } = update else {
            panic!();
        };
        assert_eq!(tool_call.tool_call_id, "tu_x");
        assert_eq!(tool_call.title, "Bash");
        assert_eq!(tool_call.raw_input, Some(json!({"command": "ls"})));
    }

    #[test]
    fn session_event_assistant_thinking_emits_thought_chunk() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: None,
            method: "session.event".to_owned(),
            params: json!({
                "session_id": "sess_1",
                "event_id": "evt_1",
                "message": {
                    "type": "assistant",
                    "session_id": "sess_1",
                    "message": {
                        "id": "msg_2",
                        "role": "assistant",
                        "model": "claude-opus-4-7",
                        "content": [
                            {"type": "thinking", "thinking": "Hmm.", "signature": "sig"},
                        ],
                    },
                },
            }),
        };
        let envelope = one(translate(event, &lookup));
        let BridgeEvent::SessionUpdate { update, .. } = envelope.event else {
            panic!();
        };
        let SessionUpdate::AgentThoughtChunk { content } = update else {
            panic!();
        };
        let TuiContentBlock::Text { text } = content else {
            panic!();
        };
        assert_eq!(text, "Hmm.");
    }

    #[test]
    fn session_event_result_success_emits_turn_complete() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: None,
            method: "session.event".to_owned(),
            params: json!({
                "session_id": "sess_1",
                "event_id": "evt_done",
                "message": {
                    "type": "result",
                    "subtype": "success",
                    "session_id": "sess_1",
                    "is_error": false,
                    "num_turns": 1,
                    "duration_ms": 100,
                    "duration_api_ms": 80,
                },
            }),
        };
        let envelope = one(translate(event, &lookup));
        let BridgeEvent::TurnComplete { session_id, .. } = envelope.event else {
            panic!();
        };
        assert_eq!(session_id, "sess_1");
    }

    #[test]
    fn spawn_response_emits_connected_with_request_cwd() {
        let value = json!({"session_id": "sess_99"});
        let envelope =
            decode_spawn_response(&value, "/tmp/proj", Some("req_1".into())).expect("envelope");
        assert_eq!(envelope.request_id.as_deref(), Some("req_1"));
        let BridgeEvent::Connected {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            ..
        } = envelope.event
        else {
            panic!();
        };
        assert_eq!(session_id, "sess_99");
        assert_eq!(cwd, "/tmp/proj");
        assert!(current_model.resolved_id.is_empty());
        assert!(available_models.is_empty());
        assert!(mode.is_none());
    }

    #[test]
    fn spawn_response_missing_session_id_drops() {
        let value = json!({});
        assert!(decode_spawn_response(&value, "/tmp", None).is_none());
    }

    #[test]
    fn status_snapshot_decodes_account_info() {
        let value = json!({
            "email": "user@example.com",
            "organization": "acme",
            "subscription_type": "team",
            "token_source": "oauth",
        });
        let envelope = decode_status_snapshot(&value, "sess_1", None).expect("envelope");
        let BridgeEvent::StatusSnapshot {
            session_id,
            account,
        } = envelope.event
        else {
            panic!();
        };
        assert_eq!(session_id, "sess_1");
        assert_eq!(account.email.as_deref(), Some("user@example.com"));
        assert_eq!(account.organization.as_deref(), Some("acme"));
        assert_eq!(account.subscription_type.as_deref(), Some("team"));
    }

    #[test]
    fn context_usage_extracts_percentage_and_clamps() {
        let value = json!({"percentage": 42.7, "total_tokens": 1000});
        let envelope = decode_context_usage(&value, "sess_1", None).expect("envelope");
        let BridgeEvent::ContextUsage {
            session_id,
            percentage,
        } = envelope.event
        else {
            panic!();
        };
        assert_eq!(session_id, "sess_1");
        assert_eq!(percentage, Some(43));

        // Out-of-range clamps to 100.
        let value = json!({"percentage": 150.0});
        let envelope = decode_context_usage(&value, "sess_1", None).unwrap();
        let BridgeEvent::ContextUsage { percentage, .. } = envelope.event else {
            panic!();
        };
        assert_eq!(percentage, Some(100));

        // Missing percentage -> None (UI shows "—").
        let value = json!({});
        let envelope = decode_context_usage(&value, "sess_1", None).unwrap();
        let BridgeEvent::ContextUsage { percentage, .. } = envelope.event else {
            panic!();
        };
        assert!(percentage.is_none());
    }

    #[test]
    fn session_event_result_error_emits_turn_error() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: None,
            method: "session.event".to_owned(),
            params: json!({
                "session_id": "sess_1",
                "event_id": "evt_err",
                "message": {
                    "type": "result",
                    "subtype": "error_during_execution",
                    "session_id": "sess_1",
                    "is_error": true,
                    "num_turns": 2,
                    "duration_ms": 50,
                    "duration_api_ms": 30,
                },
            }),
        };
        let envelope = one(translate(event, &lookup));
        let BridgeEvent::TurnError {
            session_id,
            error_kind,
            sdk_result_subtype,
            ..
        } = envelope.event
        else {
            panic!();
        };
        assert_eq!(session_id, "sess_1");
        assert_eq!(error_kind.as_deref(), Some("error_during_execution"));
        assert_eq!(
            sdk_result_subtype.as_deref(),
            Some("error_during_execution")
        );
    }
}
