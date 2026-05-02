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
    let _ = msg;
    apply_fast_mode_update(app, raw);
}

fn handle_user(app: &mut App, msg: Message, raw: &Value) {
    let _ = app;
    let _ = msg;
    let _ = raw;
}

fn handle_system(app: &mut App, msg: Message, raw: &Value) {
    let Message::System { ref subtype, ref data, .. } = msg else { return };
    match subtype.as_str() {
        "status" => {
            apply_fast_mode_update(app, data);
            // session status: "compacting" → Compacting, null → Idle.
            // (Other string values silently ignored, matching upstream.)
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
        }
        _ => {}
    }
    let _ = raw;
}

/// Drain `settings_errors` / `settingsErrors` from a System(init)
/// data record and call the App's settings-parse-error notice handler
/// once per error.
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
    let _ = app;
    let _ = msg;
    let _ = raw;
}

fn handle_task_progress(app: &mut App, msg: Message, raw: &Value) {
    let _ = app;
    let _ = msg;
    let _ = raw;
}

fn handle_task_notification(app: &mut App, msg: Message, raw: &Value) {
    let _ = app;
    let _ = msg;
    let _ = raw;
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
    let _ = msg;
    apply_fast_mode_update(app, raw);
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
