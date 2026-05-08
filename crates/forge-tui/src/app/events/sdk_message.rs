//! Direct `forge_primitives::Message` consumer for the App.
//!
//! [`handle_sdk_message`] is the App-side dispatcher that receives
//! raw `forge_primitives::Message` envelopes from the bridge worker
//! and routes them to per-variant handlers below. Each handler
//! destructures the typed message variant directly and mutates App
//! state. `Message::System { data: Value, .. }` is the one variant
//! that still walks JSON — its subtype shapes aren't first-class on
//! the typed envelope, so per-subtype handlers branch on
//! narrow `.get()` lookups against `data`.
//!
//! # Clippy allows
//!
//! - `needless_pass_by_value`: handlers take `msg: Message` by value
//!   so they can destructure ownership-tracked fields without copying.
//! - `doc_markdown`: keeps prose like `forge_primitives::Message` in
//!   doc comments without backticks tripping the lint.
#![allow(clippy::needless_pass_by_value)]

use forge_primitives::Message;
use serde_json::Value;

use crate::agent::state_parsing::{
    build_api_retry_update, build_rate_limit_update, normalize_settings_parse_errors,
    parse_runtime_session_state,
};
use crate::app::App;

/// Top-level entry point. Called from `events::client` after the
/// session-id check on `ClientEvent::SdkMessageReceived`. Dispatches
/// to per-variant handlers below.
pub(super) fn handle_sdk_message(app: &mut App, msg: Message) {
    match msg {
        Message::Assistant { .. } => handle_assistant(app, msg),
        Message::User { .. } => handle_user(app, msg),
        Message::System { .. } => handle_system(app, msg),
        Message::TaskStarted { .. } => handle_task_started(app, msg),
        Message::TaskProgress { .. } => handle_task_progress(app, msg),
        Message::TaskNotification { .. } => handle_task_notification(app, msg),
        Message::RateLimitEvent { .. } => handle_rate_limit_event(app, msg),
        Message::Result { .. } => handle_result(app, msg),
        // `Message::StreamEvent` and `_` (Error / Unknown / future
        // variants on the `#[non_exhaustive]` enum) — no-op today.
        // Re-add a handler if a downstream consumer needs to react
        // to partial-message frames.
        Message::StreamEvent { .. } | _ => {}
    }
}

/// Apply a typed `forge_primitives::FastModeState` to App state.
/// Idempotent — same state in re-applies as a no-op.
///
/// Converts the wire-side `forge_primitives::FastModeState` to the
/// App-side `model::FastModeState`. Both enums share the same
/// variant set; the conversion is a 1:1 match.
fn apply_fast_mode_state(app: &mut App, wire_state: forge_primitives::FastModeState) {
    use crate::agent::model::FastModeState as Model;
    use forge_primitives::FastModeState as Wire;
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

/// `apply_fast_mode_state` adapter for the System("status") path,
/// which still reads from a `Value` payload (the `data` field on the
/// generic system envelope). Drops silently when the field is absent
/// or doesn't deserialize to a known variant.
fn apply_fast_mode_state_from_value(app: &mut App, data: &Value) {
    let Some(wire_state) =
        crate::agent::state_parsing::parse_fast_mode_state(data.get("fast_mode_state"))
    else {
        return;
    };
    apply_fast_mode_state(app, wire_state);
}

// Per-variant handlers. Each takes ownership of the full `Message`
// so it can destructure ownership-tracked fields freely.

fn handle_assistant(app: &mut App, msg: Message) {
    let Message::Assistant { message, parent_tool_use_id, error, .. } = msg else {
        return;
    };
    // Per-turn model observation. The CLI's `system/init` carries the
    // resolved model id once per session; every subsequent Assistant
    // envelope re-states the model at `message.model`. Tracking the
    // most recent observed model lets the App verify that the chip
    // matches what the CLI is actually using on each turn.
    if !message.model.is_empty() {
        app.observed_assistant_model = Some(message.model.clone());
    }
    // Outer-envelope error capture — `app.turn_state.last_assistant_error`
    // is consulted by `apply_result_finalize` to classify TurnError
    // variants.
    if let Some(err) = error {
        let err_str = match err {
            forge_primitives::AssistantMessageError::AuthenticationFailed => {
                "authentication_failed"
            }
            forge_primitives::AssistantMessageError::BillingError => "billing_error",
            forge_primitives::AssistantMessageError::RateLimit => "rate_limit",
            forge_primitives::AssistantMessageError::InvalidRequest => "invalid_request",
            forge_primitives::AssistantMessageError::ServerError => "server_error",
            forge_primitives::AssistantMessageError::Unknown => "unknown",
        };
        app.turn_state.last_assistant_error = Some(err_str.to_owned());
    }
    walk_assistant_content(app, &message.content, parent_tool_use_id.as_deref());
}

/// Walk the typed `Message::Assistant` content blocks, applying text,
/// thinking, tool_use, and tool_result blocks to App state directly.
fn walk_assistant_content(
    app: &mut App,
    content: &[forge_primitives::ContentBlock],
    parent_tool_use_id: Option<&str>,
) {
    use crate::agent::model;
    use forge_primitives::ContentBlock;

    for block in content {
        match block {
            ContentBlock::Text { text } => {
                if text.is_empty() {
                    continue;
                }
                super::clear_compaction_state(app, true);
                let chunk = model::ContentChunk::new(model::ContentBlock::Text(
                    model::TextContent::new(text.clone()),
                ));
                super::streaming::handle_agent_message_chunk(app, chunk);
            }
            ContentBlock::Thinking { thinking, .. } => {
                if thinking.is_empty() {
                    continue;
                }
                let chunk_chars = thinking.chars().count();
                tracing::trace!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "agent_thought_chunk_applied",
                    message = "agent thought chunk applied",
                    outcome = "success",
                    chunk_chars,
                );
                app.status = crate::app::AppStatus::Thinking;
            }
            ContentBlock::ToolUse { id, name, input }
            | ContentBlock::ServerToolUse { id, name, input } => {
                if id.is_empty() {
                    continue;
                }
                apply_plan_if_todo_write(app, name, input);
                apply_tool_use_block(app, id, name, input, parent_tool_use_id);
            }
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                if tool_use_id.is_empty() {
                    continue;
                }
                let raw_block = serde_json::to_value(block).ok();
                apply_tool_result_block(
                    app,
                    tool_use_id,
                    *is_error,
                    Some(content),
                    raw_block.as_ref(),
                );
            }
            ContentBlock::Unknown { type_str, raw }
                if crate::agent::tooling::is_tool_result_block_type(type_str) =>
            {
                // Wire-side tool-result variants the typed enum
                // doesn't enumerate: `mcp_tool_result`,
                // `web_fetch_tool_result`, etc. (full set in
                // `forge_agent::tooling::TOOL_RESULT_TYPES`). They
                // share the `tool_use_id` + `content` + `is_error`
                // shape — pull those off the raw value.
                let Some(record) = raw.as_object() else { continue };
                let Some(tool_use_id) =
                    record.get("tool_use_id").and_then(Value::as_str).filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let is_error = record.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let raw_content = record.get("content");
                apply_tool_result_block(app, tool_use_id, is_error, raw_content, Some(raw));
            }
            _ => {}
        }
    }
}

/// When the assistant invokes the TodoWrite tool with a `todos`
/// array, apply the plan via the existing `apply_plan_todos` handler.
fn apply_plan_if_todo_write(app: &mut App, name: &str, input: &Value) {
    use crate::agent::model;
    use crate::app::connect::type_converters::convert_plan_entry;
    use forge_primitives as types;

    if name != "TodoWrite" {
        return;
    }
    let Some(todos) = input.as_object().and_then(|r| r.get("todos")).and_then(Value::as_array)
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
            let status = r.get("status").and_then(Value::as_str).unwrap_or("pending").to_owned();
            let active_form = status.clone();
            Some(types::PlanEntry { content, status, active_form })
        })
        .collect();
    if wire_entries.is_empty() {
        return;
    }
    let entries: Vec<model::PlanEntry> = wire_entries.into_iter().map(convert_plan_entry).collect();
    let plan = model::Plan::new(entries);
    crate::app::todos::apply_plan_todos(app, &plan);
}

fn handle_user(app: &mut App, msg: Message) {
    let Message::User { message, parent_tool_use_id, tool_use_result, .. } = msg else {
        return;
    };
    walk_user_tool_results(app, &message.content);
    // Sub-agent tool_use_result envelopes carry parent_tool_use_id at
    // the message level — wire the implicit parent linkage so the
    // tool_call lifecycle picks up sub-agent results correctly.
    if let Some(result) = tool_use_result.as_ref()
        && let Some(tool_use_id) = parent_tool_use_id.as_deref()
        && !tool_use_id.is_empty()
    {
        let parsed = crate::agent::tooling::unwrap_tool_use_result(result);
        apply_tool_result_block(
            app,
            tool_use_id,
            parsed.is_error,
            Some(&parsed.content),
            Some(result),
        );
    }
}

/// Walk the typed `Message::User` content blocks and apply
/// tool_result blocks via `apply_tool_result_block`.
fn walk_user_tool_results(app: &mut App, content: &[forge_primitives::ContentBlock]) {
    use forge_primitives::ContentBlock;

    for block in content {
        match block {
            ContentBlock::ToolResult { tool_use_id, content, is_error } => {
                if tool_use_id.is_empty() {
                    continue;
                }
                let raw_block = serde_json::to_value(block).ok();
                apply_tool_result_block(
                    app,
                    tool_use_id,
                    *is_error,
                    Some(content),
                    raw_block.as_ref(),
                );
            }
            ContentBlock::Unknown { type_str, raw }
                if crate::agent::tooling::is_tool_result_block_type(type_str) =>
            {
                // Same fallback as `walk_assistant_content` — wire
                // tool-result variants outside the typed enum.
                let Some(record) = raw.as_object() else { continue };
                let Some(tool_use_id) =
                    record.get("tool_use_id").and_then(Value::as_str).filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let is_error = record.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                let raw_content = record.get("content");
                apply_tool_result_block(app, tool_use_id, is_error, raw_content, Some(raw));
            }
            _ => {}
        }
    }
}

/// Reads `meta.claudeCode.parentToolUseId` from a tool_call's meta
/// blob.
fn parent_tool_use_id_from_meta(meta: Option<&Value>) -> Option<String> {
    let claude_code = meta?.get("claudeCode")?.as_object()?;
    let id = claude_code.get("parentToolUseId")?.as_str()?;
    if id.is_empty() { None } else { Some(id.to_owned()) }
}

/// Insert or update an entry in `app.turn_state.tool_calls` for a
/// `tool_use` content block, and dispatch the resulting initial
/// `ToolCall` or `ToolCallUpdate` via the existing App handlers.
fn apply_tool_use_block(
    app: &mut App,
    tool_use_id: &str,
    name: &str,
    input: &Value,
    parent_tool_use_id: Option<&str>,
) {
    use crate::agent::tooling::create_tool_call;
    use crate::app::connect::type_converters::convert_tool_call;
    use forge_primitives::ToolCallUpdateFields;

    let existing = app.turn_state.tool_calls.get(tool_use_id).cloned();
    let resolved_parent = parent_tool_use_id
        .map(str::to_owned)
        .or_else(|| parent_tool_use_id_from_meta(existing.as_ref().and_then(|e| e.meta.as_ref())));
    let mut tool_call = create_tool_call(tool_use_id, name, input, resolved_parent.as_deref());
    "in_progress".clone_into(&mut tool_call.status);

    if existing.is_none() {
        app.turn_state.tool_calls.insert(tool_use_id.to_owned(), tool_call.clone());
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

/// Apply a tool_result content block to App state. Looks up the
/// tool_call in `app.turn_state.tool_calls`, builds result fields
/// via `agent::tooling::build_tool_result_fields`, and dispatches a
/// `ToolCallUpdate` through the existing App handler.
fn apply_tool_result_block(
    app: &mut App,
    tool_use_id: &str,
    is_error: bool,
    raw_content: Option<&Value>,
    raw_block: Option<&Value>,
) {
    use crate::agent::tooling::build_tool_result_fields;

    let base = app.turn_state.tool_calls.get(tool_use_id).cloned();
    let fields = build_tool_result_fields(is_error, raw_content, base.as_ref(), raw_block);
    apply_tool_call_update(app, tool_use_id, fields);
}

/// Mutate `app.turn_state.tool_calls` with the supplied update
/// fields, then dispatch a `ToolCallUpdate` via the existing App
/// handler.
fn apply_tool_call_update(
    app: &mut App,
    tool_use_id: &str,
    fields: forge_primitives::ToolCallUpdateFields,
) {
    use crate::app::connect::type_converters::convert_tool_call_update;
    use forge_primitives::ToolCallUpdate;

    if let Some(base) = app.turn_state.tool_calls.get_mut(tool_use_id) {
        base.merge(fields.clone());
    }
    let wire_update = ToolCallUpdate { tool_call_id: tool_use_id.to_owned(), fields };
    let model_update = convert_tool_call_update(wire_update);
    super::tool_updates::handle_tool_call_update_session(app, &model_update);
}

/// Walk `app.turn_state.tool_calls` and emit a terminal status
/// update for every still-pending entry. Called from
/// `apply_result_finalize` when a turn ends.
fn finalize_open_tool_calls(app: &mut App, status: &str) {
    use forge_primitives::ToolCallUpdateFields;

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

fn handle_system(app: &mut App, msg: Message) {
    let Message::System { subtype, data, .. } = msg else { return };
    match subtype.as_str() {
        "status" => {
            apply_fast_mode_state_from_value(app, &data);
            // permissionMode → CurrentModeUpdate + typed turn_state.mode
            // mirror + supported_mode_ids recompute. The App-side
            // `apply_current_mode_update` only touches the display
            // struct, so we still need to update `turn_state.mode`
            // and refresh `supported_mode_ids` ourselves — otherwise
            // the typed mode the `/mode` picker reads goes stale on
            // server-side mode switches.
            if let Some(mode_str) = data.get("permissionMode").and_then(Value::as_str) {
                super::apply_current_mode_update(
                    app,
                    &crate::agent::model::CurrentModeUpdate::new(mode_str),
                );
                if let Some(parsed) = crate::agent::state::PermissionMode::from_wire(mode_str) {
                    use crate::agent::commands::supported_mode_ids_filtered;
                    app.turn_state.mode = Some(parsed);
                    let supports_auto_mode = app
                        .current_model
                        .as_ref()
                        .is_some_and(|m| m.supports_auto_mode == Some(true));
                    let supported = supported_mode_ids_filtered(
                        supports_auto_mode,
                        app.turn_state.supports_bypass_permissions_mode,
                        Some(parsed),
                        &app.turn_state.runtime_unavailable_mode_ids,
                    );
                    app.turn_state.supported_mode_ids = supported;
                }
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
            apply_api_retry_update(app, &data);
        }
        "init" => {
            apply_settings_parse_errors(app, &data);
            apply_available_commands_from_init(app, &data);
            apply_available_agents_from_init(app, &data);
            apply_current_model_from_init(app, &data);
            apply_mode_state_from_init(app, &data);
        }
        "compact_boundary" => {
            apply_compaction_boundary(app, &data);
        }
        "elicitation_complete" => {
            apply_elicitation_complete(app, &data);
        }
        "elicitation_request" => {
            apply_elicitation_request(app, &data);
        }
        "local_command_output" => {
            apply_local_command_output(app, &data);
        }
        _ => {}
    }
}

/// Drain `settings_errors` / `settingsErrors` from a System(init)
/// data record and call the App's settings-parse-error notice handler
/// once per error.
/// Parse a System(compact_boundary) record and call the App's
/// rate_limit::handle_compaction_boundary_update with the typed
/// boundary value.
fn apply_compaction_boundary(app: &mut App, data: &Value) {
    #[derive(serde::Deserialize)]
    struct Boundary {
        compact_metadata: Inner,
    }
    #[derive(serde::Deserialize)]
    struct Inner {
        trigger: String,
        // CLI emits both snake_case and camelCase shapes across versions.
        #[serde(alias = "preTokens")]
        pre_tokens: u64,
    }

    let Ok(boundary) = serde_json::from_value::<Boundary>(data.clone()) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            ?data,
            "apply_compaction_boundary: failed to decode compact_boundary record; skipped",
        );
        return;
    };
    let model_trigger = match boundary.compact_metadata.trigger.as_str() {
        "manual" => crate::agent::model::CompactionTrigger::Manual,
        "auto" => crate::agent::model::CompactionTrigger::Auto,
        _ => return,
    };
    super::rate_limit::handle_compaction_boundary_update(
        app,
        crate::agent::model::CompactionBoundary {
            trigger: model_trigger,
            pre_tokens: boundary.compact_metadata.pre_tokens,
        },
    );
}

/// Build `AvailableCommandsUpdate` from System(init).slash_commands.
fn apply_available_commands_from_init(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(arr) = record.get("slash_commands").and_then(Value::as_array) else { return };
    let commands: Vec<forge_primitives::AvailableCommand> = arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(|name| forge_primitives::AvailableCommand {
            name: name.to_owned(),
            description: String::new(),
            input_hint: None,
        })
        .collect();
    if commands.is_empty() {
        return;
    }
    let model_update =
        crate::app::connect::type_converters::map_available_commands_update(commands);
    super::apply_available_commands_update(app, model_update);
}

/// Build `AvailableAgentsUpdate` from System(init).agents with
/// last-signature change detection (so identical re-emits are no-ops).
fn apply_available_agents_from_init(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    if app.turn_state.last_agents_signature.is_some() {
        // Only emit on first init — subsequent inits with the same
        // agent set are silent no-ops.
        return;
    }
    let Some(agents_value) = record.get("agents") else { return };
    let agents = crate::agent::agents::map_available_agents_from_names(Some(agents_value));
    let signature = serde_json::to_string(&agents).unwrap_or_default();
    app.turn_state.last_agents_signature = Some(signature);
    let model_update = crate::app::connect::type_converters::map_available_agents_update(agents);
    super::apply_available_agents_update(app, model_update);
}

/// Resolve `current_model` from System(init) data and apply via the
/// App-side `apply_current_model_update` helper if it differs from
/// the existing `app.current_model`.
///
/// The CLI's `system/init` carries the resolved `model` id but
/// NOT the `models` catalogue (that lives in the initialize
/// control_response, which the App already absorbed via Connected).
/// Reuse `app.available_models` — re-deriving from the init payload's
/// missing `models` field would return an empty catalogue and reset
/// `current_model.supports_effort` (and friends) to `false`, dropping
/// the footer's effort chip and adaptive-thinking flags after the
/// first turn lands.
fn apply_current_model_from_init(app: &mut App, data: &Value) {
    use crate::agent::session_lifecycle::resolve_current_model_from_inputs;
    use crate::app::connect::type_converters::convert_current_model;
    use forge_primitives as wire;

    let Some(record) = data.as_object() else { return };
    let model_id = record.get("model").and_then(Value::as_str).unwrap_or("");
    let requested = app.turn_state.requested_model_id.as_deref();
    let resolved_runtime = app.turn_state.resolved_runtime_model_id.as_deref();
    if !model_id.is_empty() {
        model_id.clone_into(&mut app.turn_state.model_id);
    }

    // Round-trip the App's typed `model::AvailableModel` list back
    // into the wire shape the catalogue resolver expects. Cheap, runs
    // once on init.
    let available_models: Vec<wire::AvailableModel> = app
        .available_models
        .iter()
        .map(|m| wire::AvailableModel {
            id: m.id.clone(),
            display_name: m.display_name.clone(),
            description: m.description.clone(),
            supports_effort: m.supports_effort,
            supported_effort_levels: m
                .supported_effort_levels
                .iter()
                .map(|level| match level {
                    crate::agent::model::EffortLevel::Low => wire::EffortLevel::Low,
                    crate::agent::model::EffortLevel::Medium => wire::EffortLevel::Medium,
                    crate::agent::model::EffortLevel::High => wire::EffortLevel::High,
                    crate::agent::model::EffortLevel::Xhigh => wire::EffortLevel::Xhigh,
                    crate::agent::model::EffortLevel::Max => wire::EffortLevel::Max,
                })
                .collect(),
            supports_adaptive_thinking: m.supports_adaptive_thinking,
            supports_fast_mode: m.supports_fast_mode,
            supports_auto_mode: m.supports_auto_mode,
        })
        .collect();

    let next_wire =
        resolve_current_model_from_inputs(model_id, requested, resolved_runtime, &available_models);
    let next_model = convert_current_model(next_wire);
    if app.current_model.as_ref() == Some(&next_model) {
        return;
    }
    super::apply_current_model_update(app, next_model);
}

/// Resolve `mode_state` from System(init) data and apply via the
/// existing App-side ModeStateUpdate dispatch arm.
///
/// Reads `permissionMode` from data, parses to a typed
/// `PermissionMode`, mirrors into `app.turn_state.mode`, recomputes
/// `supported_mode_ids` (using the App's current_model auto-mode
/// support + the bypass flag), then builds a `ModeState` and applies.
fn apply_mode_state_from_init(app: &mut App, data: &Value) {
    use crate::agent::commands::{build_mode_state_from_supported, supported_mode_ids_filtered};
    use crate::agent::state::PermissionMode;
    use crate::app::connect::type_converters::convert_mode_state;

    let Some(record) = data.as_object() else { return };
    let Some(mode_str) = record.get("permissionMode").and_then(Value::as_str) else { return };
    let Some(mode) = PermissionMode::from_wire(mode_str) else { return };
    app.turn_state.mode = Some(mode);

    // System(init) is the canonical source for `supportsBypassPermissionsMode`.
    // Without this write the bypass chip / `/mode` option stays hidden even
    // when the CLI declares it supported.
    if let Some(supports_bypass) =
        record.get("supportsBypassPermissionsMode").and_then(Value::as_bool)
    {
        app.turn_state.supports_bypass_permissions_mode = supports_bypass;
    }

    let supports_auto_mode =
        app.current_model.as_ref().is_some_and(|m| m.supports_auto_mode == Some(true));
    let supported = supported_mode_ids_filtered(
        supports_auto_mode,
        app.turn_state.supports_bypass_permissions_mode,
        Some(mode),
        &app.turn_state.runtime_unavailable_mode_ids,
    );
    app.turn_state.supported_mode_ids.clone_from(&supported);

    let wire_mode_state = build_mode_state_from_supported(mode, &supported);
    let model_mode_state = convert_mode_state(wire_mode_state);
    super::apply_mode_state_update(app, model_mode_state);
}

/// When the SDK fires a System(local_command_output), forward the
/// content as an `AgentMessageChunk` so it appears inline in the
/// chat transcript.
fn apply_local_command_output(app: &mut App, data: &Value) {
    use crate::agent::model;
    let Some(record) = data.as_object() else { return };
    let content = record.get("content").and_then(Value::as_str).unwrap_or("");
    if content.trim().is_empty() {
        return;
    }
    super::clear_compaction_state(app, true);
    let chunk = model::ContentChunk::new(model::ContentBlock::Text(model::TextContent::new(
        content.to_owned(),
    )));
    super::streaming::handle_agent_message_chunk(app, chunk);
}

/// Build an `ElicitationRequest` from System(elicitation_request)
/// data and dispatch via the App's MCP overlay handler.
fn apply_elicitation_request(app: &mut App, data: &Value) {
    use forge_primitives::{ElicitationMode, ElicitationRequest};
    let Some(record) = data.as_object() else { return };
    let Some(request_id) = record.get("request_id").and_then(Value::as_str) else { return };
    let server_name = record.get("server_name").and_then(Value::as_str).unwrap_or("").to_owned();
    let message = record.get("message").and_then(Value::as_str).unwrap_or("").to_owned();
    let mode = match record.get("mode").and_then(Value::as_str) {
        Some("url") => ElicitationMode::Url,
        _ => ElicitationMode::Form,
    };
    let url = record.get("url").and_then(Value::as_str).map(str::to_owned);
    let elicitation_id = record.get("elicitation_id").and_then(Value::as_str).map(str::to_owned);
    let requested_schema = record.get("requested_schema").cloned();
    let request = ElicitationRequest {
        request_id: request_id.to_owned(),
        server_name,
        message,
        mode,
        url,
        elicitation_id,
        requested_schema,
    };
    crate::app::config::present_mcp_elicitation_request(app, request);
}

/// Drain a System(elicitation_complete) record and call the App's
/// MCP elicitation-completed handler (notice + overlay state).
fn apply_elicitation_complete(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(elicitation_id) =
        record.get("elicitation_id").and_then(Value::as_str).filter(|s| !s.is_empty())
    else {
        return;
    };
    let server_name = record.get("mcp_server_name").and_then(Value::as_str).map(str::to_owned);
    crate::app::config::handle_mcp_elicitation_completed(app, elicitation_id, server_name);
}

fn apply_settings_parse_errors(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(errors) = record.get("settings_errors").or_else(|| record.get("settingsErrors"))
    else {
        return;
    };
    for err in normalize_settings_parse_errors(errors) {
        super::handle_settings_parse_error(app, err.file.as_deref(), &err.path, &err.message);
    }
}

/// Apply an api_retry system message to the App. Parses via
/// `build_api_retry_update` and calls into the existing api_retry
/// event handler.
fn apply_api_retry_update(app: &mut App, data: &Value) {
    let Some(record) = data.as_object() else { return };
    let Some(forge_primitives::ApiRetryUpdate {
        attempt,
        max_retries,
        retry_delay_ms,
        error_status,
        error,
    }) = build_api_retry_update(record)
    else {
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
    wire: forge_primitives::RuntimeSessionState,
) -> crate::agent::model::RuntimeSessionState {
    use crate::agent::model::RuntimeSessionState as Model;
    use forge_primitives::RuntimeSessionState as Wire;
    match wire {
        Wire::Idle => Model::Idle,
        Wire::Running => Model::Running,
        Wire::RequiresAction => Model::RequiresAction,
    }
}

fn handle_task_started(app: &mut App, msg: Message) {
    let Message::TaskStarted { tool_use_id, task_id, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if id.is_empty() {
        return;
    }
    apply_tool_progress_update(app, id, "Task");
    if !task_id.is_empty() {
        app.turn_state.task_tool_use_ids.insert(task_id.clone(), id.to_owned());
    }
}

fn handle_task_progress(app: &mut App, msg: Message) {
    let Message::TaskProgress { tool_use_id, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if !id.is_empty() {
        apply_tool_progress_update(app, id, "Task");
    }
}

fn handle_task_notification(app: &mut App, msg: Message) {
    let Message::TaskNotification { tool_use_id, summary, .. } = msg else { return };
    let id = tool_use_id.as_deref().unwrap_or("");
    if !id.is_empty() {
        apply_tool_summary_update(app, id, &summary);
    }
}

/// Apply a `TaskProgress` notification to App state — bumps the
/// `tool_use_id`'s status to `in_progress` if it isn't already in a
/// terminal/active state.
fn apply_tool_progress_update(app: &mut App, tool_use_id: &str, name: &str) {
    use forge_primitives::ToolCallUpdateFields;

    let existing = app.turn_state.tool_calls.get(tool_use_id).cloned();
    let Some(existing) = existing else {
        apply_tool_use_block(app, tool_use_id, name, &Value::Object(serde_json::Map::new()), None);
        return;
    };
    if matches!(existing.status.as_str(), "in_progress" | "completed" | "failed" | "killed") {
        return;
    }
    apply_tool_call_update(
        app,
        tool_use_id,
        ToolCallUpdateFields { status: Some("in_progress".to_owned()), ..Default::default() },
    );
}

/// Apply a `TaskNotification` summary to App state — finalises the
/// matching `tool_use_id` with `completed` status (preserving any
/// existing terminal `failed`/`killed` status) and updates content.
fn apply_tool_summary_update(app: &mut App, tool_use_id: &str, summary: &str) {
    use forge_primitives::{ToolCallContent, ToolCallUpdateFields};

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
            content: forge_primitives::ChunkContent::Text { text: summary.to_owned() },
        }]),
        ..Default::default()
    };
    apply_tool_call_update(app, tool_use_id, fields);
}

fn handle_rate_limit_event(app: &mut App, msg: Message) {
    let Message::RateLimitEvent { rate_limit_info, .. } = msg else { return };
    let value = serde_json::to_value(&rate_limit_info).unwrap_or(Value::Null);
    // Raw payload at debug — useful for triaging whether a notice
    // surfaces from forge cache vs. an account-level Anthropic signal.
    tracing::debug!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "rate_limit_event_received",
        message = "raw RateLimitEvent payload from forge-sdk",
        outcome = "wire_evidence",
        config_dir = std::env::var("CLAUDE_CONFIG_DIR")
            .unwrap_or_else(|_| "(unset, falls back to ~/.claude)".to_owned()),
        session_id = app.session_id.as_ref().map(ToString::to_string).as_deref().unwrap_or(""),
        rate_limit_info = %value,
    );
    let Some(wire) = build_rate_limit_update(Some(&value)) else {
        return;
    };
    // Convert wire-side types::RateLimitUpdate → model::RateLimitUpdate
    // via the existing converter, then call the App-side handler.
    let update = crate::app::connect::type_converters::map_rate_limit_update(wire);
    super::rate_limit::handle_rate_limit_update(app, &update);
}

fn handle_result(app: &mut App, msg: Message) {
    let Message::Result { is_error, subtype, errors, terminal_reason, fast_mode_state, .. } = msg
    else {
        return;
    };
    if let Some(state) = fast_mode_state {
        apply_fast_mode_state(app, state);
    }
    apply_result_finalize(app, is_error, &subtype, errors.unwrap_or_default(), terminal_reason);
}

/// On a successful Result, finalise any still-open tool_calls
/// (terminal "completed") and trigger the App's TurnComplete handler.
/// On a failed Result, finalise with "failed", classify the
/// error_kind, and trigger the App's TurnError handler.
fn apply_result_finalize(
    app: &mut App,
    is_error: bool,
    subtype: &str,
    errors_array: Vec<String>,
    terminal_reason: Option<forge_primitives::TerminalReason>,
) {
    if !is_error && subtype == "success" {
        app.turn_state.last_assistant_error = None;
        finalize_open_tool_calls(app, "completed");
        super::turn::handle_turn_complete_event(app, terminal_reason);
        return;
    }

    let assistant_error = app.turn_state.last_assistant_error.clone();
    finalize_open_tool_calls(app, "failed");
    // Build a clean detail string for the renderer to use after its
    // canonical "Turn failed: " prefix. Drop the SDK's default
    // `subtype="success"` (an internal bookkeeping value, not a user
    // reason). Don't prepend "turn failed:" here — the renderer adds
    // the prefix once, and doubling it produces "Turn failed: turn
    // failed: ..." in the chat.
    let message = if !errors_array.is_empty() {
        errors_array.join("\n")
    } else if !subtype.is_empty() && subtype != "success" {
        subtype.to_owned()
    } else {
        String::new()
    };
    let class = classify_turn_error_kind(subtype, &errors_array, assistant_error.as_deref());
    super::turn::handle_turn_error_event(app, &message, Some(class), terminal_reason);
    app.turn_state.last_assistant_error = None;
}

/// App-side classifier for `TurnError` payloads — picks one of the
/// `TurnErrorClass` variants based on subtype + error strings, used to
/// drive UI rendering for the failure case.
fn classify_turn_error_kind(
    subtype: &str,
    errors: &[String],
    assistant_error: Option<&str>,
) -> crate::agent::error_handling::TurnErrorClass {
    use crate::agent::error_handling::TurnErrorClass;
    let plan_limit_signals =
        ["error_max_turns", "error_max_budget_usd", "billing_error", "rate_limit"];
    if plan_limit_signals.iter().any(|s| subtype.contains(s)) {
        return TurnErrorClass::PlanLimit;
    }
    if errors.iter().any(|e| plan_limit_signals.iter().any(|s| e.contains(s))) {
        return TurnErrorClass::PlanLimit;
    }
    if assistant_error == Some("authentication_failed") {
        return TurnErrorClass::AuthRequired;
    }
    if errors.iter().any(|e| {
        crate::agent::error_handling::looks_like_auth_required_error_lower(&e.to_ascii_lowercase())
    }) {
        return TurnErrorClass::AuthRequired;
    }
    if assistant_error == Some("server_error") {
        return TurnErrorClass::Internal;
    }
    TurnErrorClass::Other
}
