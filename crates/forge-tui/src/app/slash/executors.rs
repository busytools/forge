//! Slash command executors: dispatching parsed commands to their handler functions.

use super::{
    parse, push_system_message, push_user_message, require_active_session, require_connection,
    set_command_pending,
};
use crate::app::App;
use crate::app::connect::{SessionStartReason, begin_resume_session, start_new_session};
use forge_workspace::SessionUpdate;

/// Handle slash command submission.
///
/// Returns `true` if the slash input was fully handled and should not be sent as a prompt.
/// Returns `false` when the input should continue through the normal prompt path.
pub fn try_handle_submit(app: &mut App, text: &str) -> bool {
    let Some(parsed) = parse(text) else {
        return false;
    };

    match parsed.name {
        "/compact" => handle_compact_submit(app, &parsed.args),
        "/config" => handle_config_submit(app, &parsed.args),
        "/mcp" => handle_mcp_submit(app, &parsed.args),
        "/plugins" => handle_plugins_submit(app, &parsed.args),
        "/status" => handle_status_submit(app, &parsed.args),
        "/usage" => handle_usage_submit(app, &parsed.args),
        "/mode" => handle_mode_submit(app, &parsed.args),
        "/model" => handle_model_submit(app, &parsed.args),
        "/new" => handle_new_session_submit(app, &parsed.args),
        "/resume" => handle_resume_submit(app, &parsed.args),
        _ => handle_unknown_submit(app, parsed.name),
    }
}

fn handle_compact_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /compact");
        return true;
    }
    if require_active_session(
        app,
        "Cannot compact: not connected yet.",
        "Cannot compact: no active session.",
    )
    .is_none()
    {
        return true;
    }

    app.set_is_compacting(true);
    false
}

fn handle_config_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /config");
        return true;
    }

    if let Err(err) = crate::app::config::open(app) {
        push_system_message(app, format!("Failed to open settings: {err}"));
    }
    true
}

fn handle_plugins_submit(app: &mut App, args: &[&str]) -> bool {
    let _ = args;

    if let Err(err) = crate::app::config::open(app) {
        push_system_message(app, format!("Failed to open plugins: {err}"));
        return true;
    }
    crate::app::config::activate_tab(app, crate::app::ConfigTab::Plugins);
    true
}

fn handle_mcp_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /mcp");
        return true;
    }

    if let Err(err) = crate::app::config::open(app) {
        push_system_message(app, format!("Failed to open MCP: {err}"));
        return true;
    }
    crate::app::config::activate_tab(app, crate::app::ConfigTab::Mcp);
    true
}

fn handle_status_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /status");
        return true;
    }

    if let Err(err) = crate::app::config::open(app) {
        push_system_message(app, format!("Failed to open status: {err}"));
        return true;
    }
    crate::app::config::activate_tab(app, crate::app::ConfigTab::Status);
    true
}

fn handle_usage_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /usage");
        return true;
    }

    if let Err(err) = crate::app::config::open(app) {
        push_system_message(app, format!("Failed to open usage: {err}"));
        return true;
    }
    crate::app::config::activate_tab(app, crate::app::ConfigTab::Usage);
    true
}

fn handle_mode_submit(app: &mut App, args: &[&str]) -> bool {
    let [requested_mode_arg] = args else {
        push_system_message(app, "Usage: /mode <id>");
        return true;
    };
    let requested_mode = requested_mode_arg.trim();
    if requested_mode.is_empty() {
        push_system_message(app, "Usage: /mode <id>");
        return true;
    }

    let Some(sid) = require_active_session(
        app,
        "Cannot switch mode: not connected yet.",
        "Cannot switch mode: no active session.",
    ) else {
        return true;
    };

    if let Some(mode) = app.mode()
        && !mode.available_modes.iter().any(|m| m.id == requested_mode)
    {
        push_system_message(app, format!("Unknown mode: {requested_mode}"));
        return true;
    }
    let Some(parsed_mode) = forge_primitives::permission::PermissionMode::from_wire(requested_mode)
    else {
        push_system_message(app, format!("Unknown mode: {requested_mode}"));
        return true;
    };

    // Apply CurrentModeUpdate + ModeStateUpdate App-side immediately
    // so the footer chip refreshes without waiting for the worker
    // round-trip. The apply is synchronous, so no `CommandPending`
    // state is needed — the UI never sees a stale pending phase.
    apply_optimistic_mode_change(app, requested_mode);

    let session_key = forge_workspace::SessionKey::from_session_id(sid.to_string());
    if let Err(e) =
        app.dispatch_command(|key| forge_workspace::Command::SetMode { key, mode: parsed_mode })
    {
        let _ = app.update_tx.send(SessionUpdate::SlashCommandError {
            key: session_key,
            message: format!("Failed to run /mode: {e}"),
        });
    }
    true
}

fn apply_optimistic_mode_change(app: &mut App, requested_mode: &str) {
    use crate::agent::commands::{build_mode_state_from_supported, supported_mode_ids_filtered};
    use crate::agent::state::PermissionMode;
    use crate::app::connect::type_converters::convert_mode_state;

    let Some(parsed) = PermissionMode::from_wire(requested_mode) else { return };
    let _: () = app.with_turn_state_mut(|ts| ts.mode = Some(parsed));
    let supports_auto_mode =
        app.current_model().is_some_and(|m| m.supports_auto_mode == Some(true));
    let (supports_bypass, unavailable_modes) = app.with_turn_state(|ts| {
        (ts.supports_bypass_permissions_mode, ts.runtime_unavailable_mode_ids.clone())
    });
    let supported = supported_mode_ids_filtered(
        supports_auto_mode,
        supports_bypass,
        Some(parsed),
        &unavailable_modes,
    );
    let _: () = app.with_turn_state_mut(|ts| ts.supported_mode_ids.clone_from(&supported));

    let current_mode_update = crate::agent::model::CurrentModeUpdate::new(parsed.as_wire());
    crate::app::events::apply_current_mode_update(app, &current_mode_update);

    let wire_mode_state = build_mode_state_from_supported(parsed, &supported);
    let model_mode_state = convert_mode_state(wire_mode_state);
    crate::app::events::apply_mode_state_update(app, model_mode_state);
}

fn handle_model_submit(app: &mut App, args: &[&str]) -> bool {
    let [model_name_arg] = args else {
        push_system_message(app, "Usage: /model <id>");
        return true;
    };
    let model_name = model_name_arg.trim();
    if model_name.is_empty() {
        push_system_message(app, "Usage: /model <id>");
        return true;
    }

    let Some(sid) = require_active_session(
        app,
        "Cannot switch model: not connected yet.",
        "Cannot switch model: no active session.",
    ) else {
        return true;
    };

    if !app.available_models().is_empty()
        && !app.available_models().iter().any(|candidate| candidate.id == model_name)
    {
        push_system_message(app, format!("Unknown model: {model_name}"));
        return true;
    }

    // Apply CurrentModelUpdate (and a refreshed ModeStateUpdate
    // when the active mode is set) App-side immediately. The apply
    // is synchronous so no `CommandPending` state is needed.
    apply_optimistic_model_change(app, model_name);

    let model_name = model_name.to_owned();
    let session_key = forge_workspace::SessionKey::from_session_id(sid.to_string());
    if let Err(e) =
        app.dispatch_command(|key| forge_workspace::Command::SetModel { key, model: model_name })
    {
        let _ = app.update_tx.send(SessionUpdate::SlashCommandError {
            key: session_key,
            message: format!("Failed to run /model: {e}"),
        });
    }
    true
}

fn apply_optimistic_model_change(app: &mut App, model_name: &str) {
    use crate::agent::commands::{build_mode_state_from_supported, supported_mode_ids_filtered};
    use crate::agent::session_lifecycle::resolve_current_model_from_inputs;
    use crate::app::connect::type_converters::{convert_current_model, convert_mode_state};

    let _: () = app.with_turn_state_mut(|ts| ts.requested_model_id = Some(model_name.to_owned()));
    let (model_id, resolved_runtime) =
        app.with_turn_state(|ts| (ts.model_id.clone(), ts.resolved_runtime_model_id.clone()));
    let next_wire = resolve_current_model_from_inputs(
        &model_id,
        Some(model_name),
        resolved_runtime.as_deref(),
        &[],
    );
    let next_model = convert_current_model(next_wire);
    crate::app::events::apply_current_model_update(app, next_model);

    let mode_opt = app.with_turn_state(|ts| ts.mode);
    if let Some(mode) = mode_opt {
        let supports_auto_mode =
            app.current_model().is_some_and(|m| m.supports_auto_mode == Some(true));
        let (supports_bypass, unavailable_modes) = app.with_turn_state(|ts| {
            (ts.supports_bypass_permissions_mode, ts.runtime_unavailable_mode_ids.clone())
        });
        let supported = supported_mode_ids_filtered(
            supports_auto_mode,
            supports_bypass,
            Some(mode),
            &unavailable_modes,
        );
        let _: () = app.with_turn_state_mut(|ts| ts.supported_mode_ids.clone_from(&supported));
        let wire_mode_state = build_mode_state_from_supported(mode, &supported);
        let model_mode_state = convert_mode_state(wire_mode_state);
        crate::app::events::apply_mode_state_update(app, model_mode_state);
    }
}

fn handle_new_session_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /new");
        return true;
    }

    push_user_message(app, "/new");

    if !require_connection(app, "Cannot create new session: not connected yet.") {
        return true;
    }

    set_command_pending(app, "Starting new session...", None);

    if let Err(e) = start_new_session(app, SessionStartReason::NewSession) {
        let session_key = app
            .active_session_key
            .clone()
            .unwrap_or_else(|| forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY));
        let _ = app.update_tx.send(SessionUpdate::SlashCommandError {
            key: session_key,
            message: format!("Failed to run /new: {e}"),
        });
    }
    true
}

fn handle_resume_submit(app: &mut App, args: &[&str]) -> bool {
    let [session_id_arg] = args else {
        push_system_message(app, "Usage: /resume <session_id>");
        return true;
    };
    let session_id = session_id_arg.trim();
    if session_id.is_empty() {
        push_system_message(app, "Usage: /resume <session_id>");
        return true;
    }

    push_user_message(app, format!("/resume {session_id}"));
    if !require_connection(app, "Cannot resume session: not connected yet.") {
        return true;
    }

    set_command_pending(app, &format!("Resuming session {session_id}..."), None);
    let session_id = session_id.to_owned();
    if let Err(e) = begin_resume_session(app, session_id) {
        let session_key = app
            .active_session_key
            .clone()
            .unwrap_or_else(|| forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY));
        let _ = app.update_tx.send(SessionUpdate::SlashCommandError {
            key: session_key,
            message: format!("Failed to run /resume: {e}"),
        });
    }
    true
}

fn handle_unknown_submit(app: &mut App, command_name: &str) -> bool {
    if super::candidates::is_supported_command(app, command_name) {
        return false;
    }
    push_system_message(app, format!("{command_name} is not yet supported"));
    true
}
