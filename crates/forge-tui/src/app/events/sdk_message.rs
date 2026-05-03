//! Direct `forge_sdk::Message` consumer for the App.
//!
//! Phase 1.2 of the bridge-collapse refactor. Today the
//! `agent::bridge::message_handlers` module owns SDK-message
//! unpacking — it walks `Message::Assistant.content`, pairs
//! `tool_use` ↔ `tool_result` across messages, and emits
//! `SessionUpdate` events the App consumes through `events::client`.
//!
//! This module introduces the App-side replacement: a top-level
//! [`handle_sdk_message`] dispatcher that the bridge worker (after
//! Phase 1.3) feeds raw `forge_sdk::Message` envelopes to. Per-variant
//! handlers below are no-op stubs in Phase 1; Phase 2 progressively
//! moves the unpacking + state-mutation logic out of the bridge into
//! these stubs, one variant per commit, until the bridge module is
//! dead code (Phase 3).
//!
//! See `~/.claude-nf/plans/pick-up-where-we-quirky-grove.md` for the
//! per-variant cutover order.
//!
//! # Temporary clippy allows
//!
//! - `needless_pass_by_value`: every per-variant handler takes
//!   `msg: Message` by value but doesn't consume it during Phase 1.
//!   Phase 2 destructures it for state mutation; the warning
//!   resolves naturally as each handler is filled in.
//! - `missing_panics_doc` / `missing_errors_doc`: handlers don't
//!   panic and aren't `Result`-returning, but doc-only lints inside
//!   `forge-tui`'s pedantic config flag the doc comments.
#![allow(clippy::needless_pass_by_value, clippy::doc_markdown)]
//!
//! # Why a parallel path during Phase 1?
//!
//! Phase 1 is compile-safe and behaviour-neutral: the bridge keeps
//! emitting `SessionUpdate`s as before, App keeps consuming them,
//! and `BridgeEvent::SdkMessage` events flow alongside as a no-op
//! double-feed. Phase 2 cutovers each variant atomically: the
//! bridge stops emitting the SessionUpdate variant, this module's
//! handler starts mutating App state. No double-write window per
//! variant.

use forge_sdk::Message;
use serde_json::Value;

use crate::app::App;
use crate::agent::bridge::state_parsing::{
    build_api_retry_update, build_rate_limit_update, normalize_settings_parse_errors,
    parse_fast_mode_state, parse_runtime_session_state,
};

/// Top-level entry point. Called from `events::client` after the
/// session-id check on `ClientEvent::SdkMessageReceived`. Dispatches
/// to per-variant handlers below.
///
/// During Phase 1 every handler is a no-op — the bridge module's
/// existing `handle_sdk_message` (in `agent::bridge::message_handlers`)
/// continues to do the real work. Phase 2 fills these in per variant.
pub(super) fn handle_sdk_message(app: &mut App, msg: Message) {
    // Mirrors the bridge's pattern: serialise the typed Message back
    // to JSON so per-variant handlers can read fields like
    // `fast_mode_state`, `terminal_reason`, `error` — which are not
    // first-class typed accessors on `forge_sdk::Message` but DO
    // appear in the wire JSON.
    let raw = serde_json::to_value(&msg).unwrap_or(Value::Null);
    match msg {
        Message::Assistant { .. } => handle_assistant(app, msg, &raw),
        Message::User { .. } => handle_user(app, msg, &raw),
        Message::System { .. } => handle_system(app, msg, &raw),
        Message::TaskStarted { .. } => handle_task_started(app, msg, &raw),
        Message::TaskProgress { .. } => handle_task_progress(app, msg, &raw),
        Message::TaskNotification { .. } => handle_task_notification(app, msg, &raw),
        Message::RateLimitEvent { .. } => handle_rate_limit_event(app, msg, &raw),
        Message::Result { .. } => handle_result(app, msg, &raw),
        Message::StreamEvent { .. } => handle_stream_event(app, msg, &raw),
        // forge_sdk::Message is `#[non_exhaustive]` — Error / Unknown
        // and any future variants fall through here.
        _ => handle_unknown(app, msg, &raw),
    }
}

/// Apply the optional `fast_mode_state` field from a wire JSON
/// envelope. Idempotent — same state in re-applies as a no-op.
///
/// Converts the wire-side `types::FastModeState` (returned by the
/// parser) to the App-side `model::FastModeState`. Both enums share
/// the same variant set; the conversion is a 1:1 match. Phase 3 may
/// consolidate to a single FastModeState type.
fn apply_fast_mode_update(app: &mut App, raw: &Value) {
    use crate::agent::model::FastModeState as Model;
    use crate::agent::types::FastModeState as Wire;
    let Some(wire_state) = parse_fast_mode_state(raw.get("fast_mode_state")) else {
        return;
    };
    let model_state = match wire_state {
        Wire::Off => Model::Off,
        Wire::Cooldown => Model::Cooldown,
        Wire::On => Model::On,
    };
    if app.fast_mode_state == model_state {
        return;
    }
    app.fast_mode_state = model_state;
}

// Per-variant handlers — Phase 1 stubs. Each takes ownership of the
// full `Message` so Phase 2 can destructure freely without revisiting
// the dispatcher. The `_ = app; _ = msg;` lines suppress unused-arg
// warnings until Phase 2.

fn handle_assistant(app: &mut App, msg: Message, raw: &Value) {
    apply_fast_mode_update(app, raw);
    // Mirror the bridge's `handle_assistant_message` outer-envelope
    // error capture — `app.turn_state.last_assistant_error` is consulted
    // by `apply_result_finalize` to classify TurnError variants.
    if let Message::Assistant { error: Some(err), .. } = &msg {
        let err_str = serde_json::to_value(err)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned));
        if let Some(s) = err_str
            && !s.is_empty()
        {
            app.turn_state.last_assistant_error = Some(s);
        }
    }
    walk_assistant_content(app, raw);
}

/// Walk the raw `Message::Assistant` JSON envelope, applying text,
/// thinking, and TodoWrite-Plan content blocks to App state directly.
/// Mirrors the corresponding branches of the bridge's
/// `handle_content_block`. The remaining tool_use (non-TodoWrite) and
/// tool_result branches still flow through the bridge until the
/// tool_call lifecycle cut lands.
fn walk_assistant_content(app: &mut App, raw: &Value) {
    use crate::agent::model;

    let Some(content) = raw
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        let Some(record) = block.as_object() else { continue };
        let block_type = record.get("type").and_then(Value::as_str).unwrap_or("");
        match block_type {
            "text" => {
                let text = record.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                super::clear_compaction_state(app, true);
                let chunk = model::ContentChunk::new(model::ContentBlock::Text(
                    model::TextContent::new(text.to_owned()),
                ));
                super::streaming::handle_agent_message_chunk(app, chunk);
            }
            "thinking" => {
                let text = record.get("thinking").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                let chunk_chars = text.chars().count();
                tracing::trace!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "agent_thought_chunk_applied",
                    message = "agent thought chunk applied",
                    outcome = "success",
                    chunk_chars,
                );
                app.status = crate::app::AppStatus::Thinking;
            }
            t if t == "tool_use" || t == "server_tool_use" => {
                let Some(tool_use_id) = record.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if tool_use_id.is_empty() {
                    continue;
                }
                let name = record
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Tool");
                let empty_input = Value::Object(serde_json::Map::new());
                let input = record.get("input").unwrap_or(&empty_input);
                let parent_id = parent_tool_use_id_from_envelope(raw);
                apply_plan_if_todo_write(app, name, input);
                apply_tool_use_block(app, tool_use_id, name, input, parent_id.as_deref());
            }
            t if is_bridge_tool_result_block_type(t) => {
                let Some(tool_use_id) = record.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                if tool_use_id.is_empty() {
                    continue;
                }
                let is_error = record
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let raw_content = record.get("content");
                apply_tool_result_block(app, tool_use_id, is_error, raw_content, Some(block));
            }
            _ => {}
        }
    }
}

/// Mirrors the bridge's `emit_plan_if_todo_write` against App state.
/// When the assistant invokes the TodoWrite tool with a `todos` array,
/// applies the plan via the existing `apply_plan_todos` handler.
fn apply_plan_if_todo_write(app: &mut App, name: &str, input: &Value) {
    use crate::agent::{model, types};
    use crate::app::connect::type_converters::convert_plan_entry;

    if name != "TodoWrite" {
        return;
    }
    let Some(todos) = input
        .as_object()
        .and_then(|r| r.get("todos"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let wire_entries: Vec<types::PlanEntry> = todos
        .iter()
        .filter_map(|todo| {
            let r = todo.as_object()?;
            let content = r.get("content").and_then(Value::as_str)?.to_owned();
            if content.is_empty() {
                return None;
            }
            let status = r
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending")
                .to_owned();
            let active_form = status.clone();
            Some(types::PlanEntry { content, status, active_form })
        })
        .collect();
    if wire_entries.is_empty() {
        return;
    }
    let entries: Vec<model::PlanEntry> =
        wire_entries.into_iter().map(convert_plan_entry).collect();
    let plan = model::Plan::new(entries);
    crate::app::todos::apply_plan_todos(app, &plan);
}

fn handle_user(app: &mut App, msg: Message, raw: &Value) {
    walk_user_tool_results(app, raw);
    // Sub-agent tool_use_result envelopes carry parent_tool_use_id at
    // the message level; mirror what the bridge's User branch does in
    // handle_sdk_message.
    if let Message::User { tool_use_result: Some(result), parent_tool_use_id, .. } = &msg
        && let Some(tool_use_id) = parent_tool_use_id.as_deref()
        && !tool_use_id.is_empty()
    {
        let parsed = crate::agent::bridge::tooling::unwrap_tool_use_result(result);
        apply_tool_result_block(
            app,
            tool_use_id,
            parsed.is_error,
            Some(&parsed.content),
            Some(result),
        );
    }
}

/// Walk a `Message::User` envelope's content array and apply
/// tool_result blocks via `apply_tool_result_block`. Mirrors the
/// bridge's `handle_user_tool_result_blocks`.
fn walk_user_tool_results(app: &mut App, raw: &Value) {
    let Some(content) = raw
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        let Some(record) = block.as_object() else { continue };
        let block_type = record.get("type").and_then(Value::as_str).unwrap_or("");
        if !is_bridge_tool_result_block_type(block_type) {
            continue;
        }
        let Some(tool_use_id) = record.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        if tool_use_id.is_empty() {
            continue;
        }
        let is_error = record.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let raw_content = record.get("content");
        apply_tool_result_block(app, tool_use_id, is_error, raw_content, Some(block));
    }
}

/// Wrapper around `bridge::tooling::is_tool_result_block_type` —
/// re-exported here so the App-side walker doesn't have to import
/// from the bridge module directly.
fn is_bridge_tool_result_block_type(block_type: &str) -> bool {
    crate::agent::bridge::tooling::is_tool_result_block_type(block_type)
}

/// Read `parent_tool_use_id` from the outer envelope (Assistant or
/// User Message). Mirrors how the bridge passes
/// `parent_tool_use_id.as_deref()` into `handle_assistant_message`
/// at the top level.
fn parent_tool_use_id_from_envelope(raw: &Value) -> Option<String> {
    raw.get("parent_tool_use_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Mirror of `bridge::tool_calls::emit_tool_call` against App state.
/// Inserts/updates `app.turn_state.tool_calls` and dispatches the
/// resulting initial `ToolCall` or `ToolCallUpdate` via the existing
/// App handlers.
fn apply_tool_use_block(
    app: &mut App,
    tool_use_id: &str,
    name: &str,
    input: &Value,
    parent_tool_use_id: Option<&str>,
) {
    use crate::agent::bridge::tool_calls::parent_tool_use_id_from_meta;
    use crate::agent::bridge::tooling::create_tool_call;
    use crate::agent::types::ToolCallUpdateFields;
    use crate::app::connect::type_converters::convert_tool_call;

    let existing = app.turn_state.tool_calls.get(tool_use_id).cloned();
    let resolved_parent = parent_tool_use_id
        .map(str::to_owned)
        .or_else(|| parent_tool_use_id_from_meta(existing.as_ref().and_then(|e| e.meta.as_ref())));
    let mut tool_call =
        create_tool_call(tool_use_id, name, input, resolved_parent.as_deref());
    "in_progress".clone_into(&mut tool_call.status);

    if existing.is_none() {
        app.turn_state
            .tool_calls
            .insert(tool_use_id.to_owned(), tool_call.clone());
        let model_tc = convert_tool_call(tool_call);
        super::tool_calls::handle_tool_call(app, model_tc);
        return;
    }

    let mut fields = ToolCallUpdateFields {
        title: Some(tool_call.title.clone()),
        kind: Some(tool_call.kind.clone()),
        status: Some("in_progress".to_owned()),
        raw_input: tool_call.raw_input.clone(),
        locations: Some(tool_call.locations.clone()),
        meta: tool_call.meta.clone(),
        ..Default::default()
    };
    if !tool_call.content.is_empty() {
        fields.content = Some(tool_call.content.clone());
    }
    apply_tool_call_update(app, tool_use_id, fields);
}

/// Mirror of `bridge::tool_calls::emit_tool_result_update` against App
/// state. Looks up the tool_call in `app.turn_state.tool_calls`,
/// builds result fields via the bridge's stateless field-builder, and
/// dispatches a `ToolCallUpdate` via the existing App handler.
fn apply_tool_result_block(
    app: &mut App,
    tool_use_id: &str,
    is_error: bool,
    raw_content: Option<&Value>,
    raw_block: Option<&Value>,
) {
    use crate::agent::bridge::tooling::build_tool_result_fields;

    let base = app.turn_state.tool_calls.get(tool_use_id).cloned();
    let fields = build_tool_result_fields(is_error, raw_content, base.as_ref(), raw_block);
    apply_tool_call_update(app, tool_use_id, fields);
}

/// Mirror of `bridge::tool_calls::emit_tool_call_update` against App
/// state. Mutates `app.turn_state.tool_calls` then dispatches a
/// `ToolCallUpdate` via the existing App handler.
fn apply_tool_call_update(
    app: &mut App,
    tool_use_id: &str,
    fields: crate::agent::types::ToolCallUpdateFields,
) {
    use crate::agent::bridge::tool_calls::apply_fields_to_base;
    use crate::agent::types::ToolCallUpdate;
    use crate::app::connect::type_converters::convert_tool_call_update;

    if let Some(base) = app.turn_state.tool_calls.get_mut(tool_use_id) {
        apply_fields_to_base(base, &fields);
    }
    let wire_update = ToolCallUpdate { tool_call_id: tool_use_id.to_owned(), fields };
    let model_update = convert_tool_call_update(wire_update);
    super::tool_updates::handle_tool_call_update_session(app, &model_update);
}

/// Mirror of `bridge::tool_calls::finalize_open_tool_calls` against
/// App state. Walks `app.turn_state.tool_calls` and emits a terminal
/// status update for every still-pending entry.
fn finalize_open_tool_calls(app: &mut App, status: &str) {
    use crate::agent::types::ToolCallUpdateFields;

    let pending: Vec<String> = app
        .turn_state
        .tool_calls
        .iter()
        .filter(|(_, t)| matches!(t.status.as_str(), "pending" | "in_progress"))
        .map(|(id, _)| id.clone())
        .collect();
    for id in pending {
        apply_tool_call_update(
            app,
            &id,
            ToolCallUpdateFields { status: Some(status.to_owned()), ..Default::default() },
        );
    }
}

fn handle_system(app: &mut App, msg: Message, raw: &Value) {
    let Message::System { ref subtype, ref data, .. } = msg else { return };
    match subtype.as_str() {
        "status" => {
            apply_fast_mode_update(app, data);
            // permissionMode → CurrentModeUpdate
            if let Some(mode_str) = data.get("permissionMode").and_then(Value::as_str) {
                super::apply_current_mode_update(
                    app,
                    crate::agent::model::CurrentModeUpdate::new(mode_str),
                );
            }
            // status: "compacting" → Compacting, null → Idle.
            if let Some(status_field) = data.get("status") {
                if status_field.as_str() == Some("compacting") {
                    super::apply_session_status_update(
                        app,
                        crate::agent::model::SessionStatus::Compacting,
                    );
                } else if status_field.is_null() {
                    super::apply_session_status_update(
                        app,
                        crate::agent::model::SessionStatus::Idle,
                    );
                }
            }
        }
        "session_state_changed" => {
            if let Some(wire_state) = parse_runtime_session_state(data.get("state")) {
                let model_state = convert_runtime_session_state(wire_state);
                super::handle_runtime_session_state_update(app, model_state);
            }
        }
        "api_retry" => {
            apply_api_retry_update(app, data);
        }
        "init" => {
            apply_settings_parse_errors(app, data);
            apply_available_commands_from_init(app, data);
            apply_available_agents_from_init(app, data);
            apply_current_model_from_init(app, data);
            apply_mode_state_from_init(app, data);
        }
        "compact_boundary" => {
            apply_compaction_boundary(app, data);
        }
        "elicitation_complete" => {
            apply_elicitation_complete(app, data);
        }
        _ => {}
    }
    let _ = raw;
}

/// Drain `settings_errors` / `settingsErrors` from a System(init)
/// data record and call the App's settings-parse-error notice handler
/// once per error.
/// Parse a System(compact_boundary) record and call the App's
/// rate_limit::handle_compaction_boundary_update with the typed
/// boundary value.
fn apply_compaction_boundary(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(meta) = record.get("compact_metadata").and_then(Value::as_object) else { return };
    let trigger = meta.get("trigger").and_then(Value::as_str).unwrap_or("");
    let Some(pre_tokens) = meta
        .get("pre_tokens")
        .or_else(|| meta.get("preTokens"))
        .and_then(Value::as_u64)
    else {
        return;
    };
    let model_trigger = match trigger {
        "manual" => crate::agent::model::CompactionTrigger::Manual,
        "auto" => crate::agent::model::CompactionTrigger::Auto,
        _ => return,
    };
    super::rate_limit::handle_compaction_boundary_update(
        app,
        crate::agent::model::CompactionBoundary {
            trigger: model_trigger,
            pre_tokens,
        },
    );
}

/// Build `AvailableCommandsUpdate` from System(init).slash_commands.
/// Mirrors bridge::message_handlers handle_system_init slash_commands branch.
fn apply_available_commands_from_init(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(arr) = record.get("slash_commands").and_then(Value::as_array) else { return };
    let commands: Vec<crate::agent::types::AvailableCommand> = arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(|name| crate::agent::types::AvailableCommand {
            name: name.to_owned(),
            description: String::new(),
            input_hint: None,
        })
        .collect();
    if commands.is_empty() {
        return;
    }
    let model_update = crate::app::connect::type_converters::map_available_commands_update(commands);
    super::apply_available_commands_update(app, model_update);
}

/// Build `AvailableAgentsUpdate` from System(init).agents with
/// last-signature change detection (so identical re-emits are
/// no-ops). Mirrors bridge::agents::emit_available_agents_if_changed.
fn apply_available_agents_from_init(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    if app.turn_state.last_agents_signature.is_some() {
        // bridge mirrors `if session.last_agents_signature.is_none()` — only
        // emit on first init.
        return;
    }
    let Some(agents_value) = record.get("agents") else { return };
    let agents = crate::agent::bridge::agents::map_available_agents_from_names(Some(agents_value));
    let signature = serde_json::to_string(&agents).unwrap_or_default();
    app.turn_state.last_agents_signature = Some(signature);
    let model_update = crate::app::connect::type_converters::map_available_agents_update(agents);
    super::apply_available_agents_update(app, model_update);
}

/// Resolve `current_model` from System(init) data and apply via the
/// App-side `apply_current_model_update` helper if it differs from the
/// existing `app.current_model`. Mirrors the bridge's
/// `handle_system_init` block that pushed `CurrentModelUpdate` after
/// `refresh_current_model` reported a change.
///
/// Today only `model_id` and the `models` catalogue are available
/// from System(init). `requested_model_id` and
/// `resolved_runtime_model_id` come from other paths (the SetModel
/// command + initialize control_response respectively); when those
/// are mirrored into `app.turn_state` later, the resolver picks them
/// up via the optional args.
fn apply_current_model_from_init(app: &mut App, data: &Value) {
    use crate::agent::bridge::session_lifecycle::{
        map_available_models, resolve_current_model_from_inputs,
    };
    use crate::app::connect::type_converters::convert_current_model;

    let Some(record) = data.as_object() else { return };
    let model_id = record.get("model").and_then(Value::as_str).unwrap_or("");
    let available_models = map_available_models(record.get("models"));
    let requested = app.turn_state.requested_model_id.as_deref();
    let resolved_runtime = app.turn_state.resolved_runtime_model_id.as_deref();
    if !model_id.is_empty() {
        model_id.clone_into(&mut app.turn_state.model_id);
    }

    let next_wire = resolve_current_model_from_inputs(
        model_id,
        requested,
        resolved_runtime,
        &available_models,
    );
    let next_model = convert_current_model(next_wire);
    if app.current_model.as_ref() == Some(&next_model) {
        return;
    }
    super::apply_current_model_update(app, next_model);
}

/// Resolve `mode_state` from System(init) data and apply via the
/// existing App-side ModeStateUpdate dispatch arm. Mirrors the
/// bridge's `handle_system_init` ModeStateUpdate emission.
///
/// Reads `permissionMode` from data, parses to a typed
/// `PermissionMode`, mirrors into `app.turn_state.mode`, recomputes
/// `supported_mode_ids` (using the App's current_model auto-mode
/// support + the bypass flag), then builds a `ModeState` and applies.
fn apply_mode_state_from_init(app: &mut App, data: &Value) {
    use crate::agent::bridge::commands::{
        build_mode_state_from_supported, supported_mode_ids_filtered,
    };
    use crate::agent::bridge::state::PermissionMode;
    use crate::app::connect::type_converters::convert_mode_state;

    let Some(record) = data.as_object() else { return };
    let Some(mode_str) = record.get("permissionMode").and_then(Value::as_str) else { return };
    let Some(mode) = PermissionMode::from_wire(mode_str) else { return };
    app.turn_state.mode = Some(mode);

    let supports_auto_mode = app
        .current_model
        .as_ref()
        .is_some_and(|m| m.supports_auto_mode == Some(true));
    let supported = supported_mode_ids_filtered(
        supports_auto_mode,
        app.turn_state.supports_bypass_permissions_mode,
        Some(mode),
        &app.turn_state.runtime_unavailable_mode_ids,
    );
    app.turn_state.supported_mode_ids = supported.clone();

    let wire_mode_state = build_mode_state_from_supported(mode, &supported);
    let model_mode_state = convert_mode_state(wire_mode_state);
    super::apply_mode_state_update(app, model_mode_state);
}

/// Drain a System(elicitation_complete) record and call the App's
/// MCP elicitation-completed handler (notice + overlay state).
fn apply_elicitation_complete(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(elicitation_id) = record
        .get("elicitation_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let server_name = record
        .get("mcp_server_name")
        .and_then(Value::as_str)
        .map(str::to_owned);
    crate::app::config::handle_mcp_elicitation_completed(app, elicitation_id, server_name);
}

fn apply_settings_parse_errors(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(errors) = record
        .get("settings_errors")
        .or_else(|| record.get("settingsErrors"))
    else {
        return;
    };
    for err in normalize_settings_parse_errors(errors) {
        super::handle_settings_parse_error(app, err.file.as_deref(), &err.path, &err.message);
    }
}

/// Apply an api_retry system message to the App. Wraps the bridge's
/// existing `build_api_retry_update` parser and calls into the
/// existing api_retry event handler.
fn apply_api_retry_update(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(crate::agent::types::SessionUpdate::ApiRetryUpdate {
        attempt,
        max_retries,
        retry_delay_ms,
        error_status,
        error,
    }) = build_api_retry_update(record) else {
        return;
    };
    let model_error = crate::app::connect::type_converters::map_api_retry_error(error);
    super::api_retry::handle_api_retry_update(
        app,
        attempt,
        max_retries,
        retry_delay_ms,
        error_status,
        model_error,
    );
}

fn convert_runtime_session_state(
    wire: crate::agent::types::RuntimeSessionState,
) -> crate::agent::model::RuntimeSessionState {
    use crate::agent::model::RuntimeSessionState as Model;
    use crate::agent::types::RuntimeSessionState as Wire;
    match wire {
        Wire::Idle => Model::Idle,
        Wire::Running => Model::Running,
        Wire::RequiresAction => Model::RequiresAction,
    }
}

fn handle_task_started(app: &mut App, msg: Message, raw: &Value) {
    let _ = raw;
    let Message::TaskStarted { tool_use_id, task_id, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if id.is_empty() {
        return;
    }
    apply_tool_progress_update(app, id, "Task");
    if !task_id.is_empty() {
        app.turn_state
            .task_tool_use_ids
            .insert(task_id.clone(), id.to_owned());
    }
}

fn handle_task_progress(app: &mut App, msg: Message, raw: &Value) {
    let _ = raw;
    let Message::TaskProgress { tool_use_id, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if !id.is_empty() {
        apply_tool_progress_update(app, id, "Task");
    }
}

fn handle_task_notification(app: &mut App, msg: Message, raw: &Value) {
    let _ = raw;
    let Message::TaskNotification { tool_use_id, summary, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if !id.is_empty() {
        apply_tool_summary_update(app, id, &summary);
    }
}

/// Mirror of `bridge::tool_calls::emit_tool_progress_update` against
/// App state.
fn apply_tool_progress_update(app: &mut App, tool_use_id: &str, name: &str) {
    use crate::agent::types::ToolCallUpdateFields;

    let existing = app.turn_state.tool_calls.get(tool_use_id).cloned();
    let Some(existing) = existing else {
        apply_tool_use_block(app, tool_use_id, name, &Value::Object(serde_json::Map::new()), None);
        return;
    };
    if matches!(
        existing.status.as_str(),
        "in_progress" | "completed" | "failed" | "killed"
    ) {
        return;
    }
    apply_tool_call_update(
        app,
        tool_use_id,
        ToolCallUpdateFields { status: Some("in_progress".to_owned()), ..Default::default() },
    );
}

/// Mirror of `bridge::tool_calls::emit_tool_summary_update` against
/// App state.
fn apply_tool_summary_update(app: &mut App, tool_use_id: &str, summary: &str) {
    use crate::agent::types::{ContentBlock, ToolCallContent, ToolCallUpdateFields};

    let Some(base) = app.turn_state.tool_calls.get(tool_use_id).cloned() else { return };
    let status = if matches!(base.status.as_str(), "failed" | "killed") {
        base.status
    } else {
        "completed".to_owned()
    };
    let fields = ToolCallUpdateFields {
        status: Some(status),
        raw_output: Some(summary.to_owned()),
        content: Some(vec![ToolCallContent::Content {
            content: ContentBlock::Text { text: summary.to_owned() },
        }]),
        ..Default::default()
    };
    apply_tool_call_update(app, tool_use_id, fields);
}

fn handle_rate_limit_event(app: &mut App, msg: Message, _raw: &Value) {
    let Message::RateLimitEvent { rate_limit_info, .. } = msg else { return };
    let value = serde_json::to_value(&rate_limit_info).unwrap_or(Value::Null);
    let Some(crate::agent::types::SessionUpdate::RateLimitUpdate {
        status,
        resets_at,
        utilization,
        rate_limit_type,
        overage_status,
        overage_resets_at,
        overage_disabled_reason,
        is_using_overage,
        surpassed_threshold,
    }) = build_rate_limit_update(Some(&value)) else {
        return;
    };
    // Convert wire-side types::RateLimitUpdate → model::RateLimitUpdate
    // via the existing converter, then call the App-side handler.
    let wire = crate::agent::types::RateLimitUpdate {
        status,
        resets_at,
        utilization,
        rate_limit_type,
        overage_status,
        overage_resets_at,
        overage_disabled_reason,
        is_using_overage,
        surpassed_threshold,
    };
    let update = crate::app::connect::type_converters::map_rate_limit_update(wire);
    super::rate_limit::handle_rate_limit_update(app, &update);
}

fn handle_result(app: &mut App, msg: Message, raw: &Value) {
    apply_fast_mode_update(app, raw);
    apply_result_finalize(app, &msg, raw);
}

/// Mirror of the bridge's `handle_result_message`. On a successful
/// Result, finalises any still-open tool_calls (terminal "completed")
/// and triggers the App's TurnComplete handler. On a failed Result,
/// finalises with "failed", classifies the error_kind, and triggers
/// the App's TurnError handler.
fn apply_result_finalize(app: &mut App, msg: &Message, raw: &Value) {
    let Message::Result { is_error, subtype, .. } = msg else { return };
    let raw_record = raw.as_object();
    let terminal_reason = raw_record
        .and_then(|r| r.get("terminal_reason"))
        .and_then(|v| serde_json::from_value::<crate::agent::types::TerminalReason>(v.clone()).ok());
    let errors_array: Vec<String> = raw_record
        .and_then(|r| r.get("errors"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    if !is_error && subtype == "success" {
        app.turn_state.last_assistant_error = None;
        finalize_open_tool_calls(app, "completed");
        super::turn::handle_turn_complete_event(app, terminal_reason);
        return;
    }

    let assistant_error = app.turn_state.last_assistant_error.clone();
    finalize_open_tool_calls(app, "failed");
    let message = if errors_array.is_empty() {
        if subtype.is_empty() {
            "turn failed".to_owned()
        } else {
            format!("turn failed: {subtype}")
        }
    } else {
        errors_array.join("\n")
    };
    let error_kind = classify_turn_error_kind(subtype, &errors_array, assistant_error.as_deref());
    let class = crate::agent::error_handling::parse_turn_error_class(error_kind);
    super::turn::handle_turn_error_event(app, &message, class, terminal_reason);
    app.turn_state.last_assistant_error = None;
}

/// Inline copy of bridge::message_handlers::classify_turn_error_kind
/// so the App-side path doesn't depend on a bridge-private function.
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
    if errors
        .iter()
        .any(|e| plan_limit_signals.iter().any(|s| e.contains(s)))
    {
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

fn looks_like_auth_required(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("authentication_failed")
        || lower.contains("not authenticated")
        || lower.contains("authentication required")
        || lower.contains("unauthenticated")
        || (lower.contains("401") && lower.contains("auth"))
}

fn handle_stream_event(app: &mut App, msg: Message, raw: &Value) {
    let _ = app;
    let _ = msg;
    let _ = raw;
}

fn handle_unknown(app: &mut App, msg: Message, raw: &Value) {
    let _ = app;
    let _ = msg;
    let _ = raw;
}
