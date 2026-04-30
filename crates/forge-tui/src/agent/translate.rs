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
//! `tool_use_id` -> JSON-RPC request id) and return either an
//! `EventEnvelope` ready to push to the TUI, or `None` for inbound
//! events that don't map to a bridge event (yet) or that are
//! unrelated.

#![allow(clippy::missing_errors_doc, clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::agent::bridge::InboundEvent;
use crate::agent::types::{
    PermissionDisplay, PermissionOption, PermissionRequest, QuestionOption, QuestionPrompt,
    QuestionRequest, ToolCall,
};
use crate::agent::wire::{BridgeEvent, EventEnvelope};

/// Map tracking reverse-RPC ids by their tool-side correlation key.
/// The writer's `PermissionResponse` / `QuestionResponse` /
/// `ElicitationResponse` branches consume this when sending replies.
pub type ReverseLookup = Arc<Mutex<HashMap<String, Value>>>;

/// Translate one inbound daemon event to a `BridgeEvent`. Returns
/// `None` for unrecognised methods or malformed payloads (logged at
/// warn-level inside the helper functions).
///
/// Side effect: may insert into `reverse_lookup` when the event is a
/// reverse-RPC request whose response will need to be matched back to
/// its JSON-RPC id.
pub fn translate(event: InboundEvent, reverse_lookup: &ReverseLookup) -> Option<EventEnvelope> {
    match (event.id, event.method.as_str()) {
        (Some(rev_id), "permission.request") => {
            translate_permission_request(rev_id, &event.params, reverse_lookup)
        }
        (Some(rev_id), "session.question_request") => {
            translate_question_request(rev_id, &event.params, reverse_lookup)
        }
        (_, "session.closed") => translate_session_closed(&event.params),
        (None, "session.event") => translate_session_event(&event.params),
        (id, method) => {
            tracing::debug!(?id, %method, "translate: ignoring inbound event");
            None
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

/// `session.event` notification: stub for now. Round 4 will translate
/// the full SDK `Message` shape into `SessionUpdate`/`TurnComplete`/
/// `TurnError`. For this commit we drop the event silently so the
/// channel doesn't back up; the TUI lift will replace this body.
fn translate_session_event(_params: &Value) -> Option<EventEnvelope> {
    None
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use serde_json::json;

    fn fresh_lookup() -> ReverseLookup {
        Arc::new(Mutex::new(HashMap::new()))
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

        let envelope = translate(event, &lookup).expect("event");
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

        let envelope = translate(event, &lookup).expect("event");
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

        let envelope = translate(event, &lookup).expect("event");
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
        let envelope = translate(event, &lookup).expect("event");
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
    fn unknown_method_returns_none() {
        let lookup = fresh_lookup();
        let event = InboundEvent {
            id: None,
            method: "unrelated.notification".to_owned(),
            params: json!({}),
        };
        assert!(translate(event, &lookup).is_none());
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
        assert!(translate(event, &lookup).is_none());
        assert!(lookup.lock().is_empty());
    }
}
