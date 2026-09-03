//! Slash command executors: dispatching parsed commands to their handler functions.

use super::{
    parse, push_system_info, push_system_message, push_user_message, require_active_session,
    require_connection, set_command_pending,
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

    // On the launchpad view, `/help` and `/quit` are local affordances
    // (the launchpad has no active session for SDK commands to forward
    // to). In the chat view these names may be SDK-advertised commands
    // that should forward to the model - the chat handler skips them
    // here so the unknown-submit fallback resolves the advertised
    // command. `/launchpad` is global (works from chat) and never
    // forwards.
    if app.active_view == crate::app::ActiveView::Launchpad {
        match parsed.name {
            "/help" => return handle_help_submit(app, &parsed.args),
            "/quit" => return handle_quit_submit(app, &parsed.args),
            _ => {}
        }
    }
    match parsed.name {
        "/account" => handle_account_submit(app, &parsed.args),
        "/dictate" => handle_dictate_submit(app, &parsed.args),
        "/compact" => handle_compact_submit(app, &parsed.args),
        "/diff" => handle_diff_submit(app, &parsed.args),
        "/effort" => handle_effort_submit(app, &parsed.args),
        "/launchpad" => handle_launchpad_submit(app, &parsed.args),
        "/mcp" => handle_mcp_submit(app, &parsed.args),
        "/plugins" => handle_plugins_submit(app, &parsed.args),
        "/mode" => handle_mode_submit(app, &parsed.args),
        "/model" => handle_model_submit(app, &parsed.args),
        "/new" => handle_new_session_submit(app, &parsed.args),
        "/resume" => handle_resume_submit(app, &parsed.args),
        "/spinner" => handle_spinner_submit(app, &parsed.args),
        "/usage" => handle_usage_submit(app, &parsed.args),
        _ => handle_unknown_submit(app, parsed.name),
    }
}

/// `/account` - open the account picker to switch the active session
/// to a different account for its project. Available only when the
/// session is idle (no in-flight turn); mid-turn it is a no-op with a
/// short notice. The picker lists the project's allowed accounts plus
/// their live rate-limit state; picking one re-spawns the session
/// under that account and resumes the same conversation.
fn handle_account_submit(app: &mut App, args: &[&str]) -> bool {
    use crate::agent::model::RuntimeSessionState;

    if !args.is_empty() {
        push_system_message(app, "Usage: /account");
        return true;
    }
    // Block only a known in-flight turn. `None` (freshly connected, no
    // state message yet) and `Some(Idle)` both allow - the workspace
    // backstop is the authoritative guard, so a false-refuse here on a
    // genuinely-idle session would just be a misleading notice.
    if matches!(
        app.runtime_session_state(),
        Some(RuntimeSessionState::Running | RuntimeSessionState::RequiresAction)
    ) {
        push_system_message(app, "Finish or cancel the current turn before switching accounts.");
        return true;
    }
    let Some(project_name) = app.active_project_name() else {
        push_system_message(app, "No active project to switch accounts for.");
        return true;
    };
    let Some(workspace) = app.workspace.clone() else {
        return true;
    };
    let allowed = workspace
        .list_projects()
        .into_iter()
        .find(|view| view.name == project_name)
        .map(|view| view.accounts)
        .unwrap_or_default();
    let current = app.active_account_display_name();
    let rows = workspace.project_accounts_snapshot(&allowed, current.as_deref());
    if rows.is_empty() {
        push_system_message(app, "No accounts configured for this project.");
        return true;
    }
    crate::app::account_picker::open(app, rows);
    true
}

/// `/dictate` - open the dictation cleanup overlay. No args: the
/// dialog deliberately reads no state back, so there is nothing to
/// show as one.
fn handle_dictate_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /dictate");
        return true;
    }
    crate::app::dictate_picker::open(app);
    true
}

/// `/diff [target]` - open the full-screen diff overlay.
///
/// No arg → delegate to `diff_overlay::open_default`, which mirrors
/// the Inspector GIT section's auto-detect: layer 1 populated
/// (`worktree`) ⇒ `HEAD`, layer 2 populated (`branch_ahead`) ⇒ the
/// default branch, both layers `Clean` ⇒ system notice "No changes"
/// with no overlay opened.
/// One positional arg → passed verbatim as the two-dot `git diff
/// <target>` ref (so `/diff main` shows committed + uncommitted on
/// a feature branch in one view).
///
/// Async: the scan runs in a tokio local task that posts back via
/// `app.diff_overlay_event_tx`; the drain pump consumes the event
/// and transitions to `ActiveView::Diff`.
fn handle_diff_submit(app: &mut App, args: &[&str]) -> bool {
    if args.len() > 1 {
        push_system_message(app, "Usage: /diff [target]");
        return true;
    }
    let Some(arg) = args.first() else {
        crate::app::diff_overlay::open_default(app);
        return true;
    };
    let target = (*arg).trim().to_owned();
    if target.is_empty() {
        push_system_message(app, "Usage: /diff [target]");
        return true;
    }
    crate::app::diff_overlay::open_with_target(app, target);
    true
}

/// `/usage` - open the full-screen token/cost overlay. No args; the
/// scan runs off-thread and posts back via `app.usage_overlay_event_tx`.
fn handle_usage_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /usage");
        return true;
    }
    crate::app::usage_overlay::open(app);
    true
}

/// `/launchpad` - return to the project picker. Available from chat;
/// the launchpad's own slash autocomplete filters it out (you can't
/// open the surface you're already on).
fn handle_launchpad_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /launchpad");
        return true;
    }
    crate::app::launchpad::open(app);
    true
}

/// `/help` - toggle the help overlay. Parallel to the `?` binding;
/// surfaced as a slash command for discoverability from the launchpad
/// (where `?` and `/help` both produce the same overlay).
fn handle_help_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /help");
        return true;
    }
    app.help_open = !app.help_open;
    app.needs_redraw = true;
    true
}

/// `/quit` - exit forge. Parallel to the `Ctrl+Q` binding; surfaced
/// as a slash command so the launchpad's keyboard-only floor has
/// every essential affordance accessible via the slash autocomplete.
fn handle_quit_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /quit");
        return true;
    }
    app.should_quit = true;
    true
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
    // The `/compact` text falls through as a normal user message -
    // the CLI emits `status:"compacting"` as its first response
    // frame, which `apply_session_status_update` translates into
    // `is_compacting = true` via the wire path. No optimistic-set
    // needed; verified reliable against the sdk_compact baseline.
    false
}

fn handle_plugins_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /plugins");
        return true;
    }

    if let Err(err) = crate::app::config::open_plugins(app) {
        push_system_message(app, format!("Failed to open plugins: {err}"));
    }
    true
}

fn handle_mcp_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /mcp");
        return true;
    }

    if let Err(err) = crate::app::config::open_mcp(app) {
        push_system_message(app, format!("Failed to open MCP: {err}"));
    }
    true
}

fn handle_mode_submit(app: &mut App, args: &[&str]) -> bool {
    if args.is_empty() {
        let label = app.mode().map_or_else(
            || "no active mode".to_owned(),
            |state| {
                if state.current_mode_name.is_empty() {
                    state.current_mode_id.clone()
                } else {
                    format!("{} ({})", state.current_mode_name, state.current_mode_id)
                }
            },
        );
        push_system_info(app, format!("Mode: {label}"));
        return true;
    }
    let [requested_mode_arg] = args else {
        push_system_info(app, "Usage: /mode [id]");
        return true;
    };
    let requested_mode = requested_mode_arg.trim();
    if requested_mode.is_empty() {
        push_system_info(app, "Usage: /mode [id]");
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
    // state is needed - the UI never sees a stale pending phase.
    apply_optimistic_mode_change(app, requested_mode);

    let session_key = forge_workspace::SessionKey::from_session_id(sid.to_string());
    if let Err(e) =
        app.dispatch_command(|key| forge_workspace::Command::SetMode { key, mode: parsed_mode })
    {
        // The command never left, so no SetModeFailed can arrive;
        // undo the optimistic apply here.
        if app.rollback_pending_mode() {
            app.invalidate_layout(crate::app::state::LayoutInvalidation::Global);
        }
        let _ = app.update_tx.send(SessionUpdate::SlashCommandError {
            key: session_key,
            message: format!("Failed to run /mode: {e}"),
        });
    }
    true
}

fn apply_optimistic_mode_change(app: &mut App, requested_mode: &str) {
    use forge_workspace::PermissionMode;
    use forge_workspace::commands::{build_mode_state_from_supported, supported_mode_ids_filtered};

    let Some(parsed) = PermissionMode::from_wire(requested_mode) else { return };
    // Rapid submits overlap: park only the FIRST pre-apply state, so a
    // rejection of any in-flight request restores the true original and
    // a refusal for a superseded request cannot consume a newer one.
    let rollback = if app.pending_mode_rollback().is_none() {
        Some(crate::app::session::ModeRollback {
            mode_state: app.mode().cloned(),
            turn_mode: app.with_turn_state(|ts| ts.mode),
            supported_mode_ids: app.with_turn_state(|ts| ts.supported_mode_ids.clone()),
        })
    } else {
        None
    };
    let _: () = app.with_turn_state_mut(|ts| ts.mode = Some(parsed));
    let supports_auto_mode =
        app.current_model().is_some_and(|m| m.supports_auto_mode == Some(true));
    let unavailable_modes = app.with_turn_state(|ts| ts.runtime_unavailable_mode_ids.clone());
    let bypass_offered = crate::app::events::bypass_mode_offered(app);
    let supported = supported_mode_ids_filtered(
        supports_auto_mode,
        bypass_offered,
        Some(parsed),
        &unavailable_modes,
    );
    let _: () = app.with_turn_state_mut(|ts| ts.supported_mode_ids.clone_from(&supported));

    let current_mode_update = crate::agent::model::CurrentModeUpdate::new(parsed.as_wire());
    crate::app::events::apply_current_mode_update(app, &current_mode_update);

    let wire_mode_state = build_mode_state_from_supported(parsed, &supported);
    let model_mode_state = wire_mode_state;
    crate::app::events::apply_mode_state_update(app, model_mode_state);
    if let Some(rollback) = rollback {
        app.set_pending_mode_rollback(Some(rollback));
    }
}

fn handle_model_submit(app: &mut App, args: &[&str]) -> bool {
    if args.is_empty() {
        if crate::app::model_picker::open(app) {
            return true;
        }
        let label = app.current_model().map_or_else(
            || "no active model".to_owned(),
            |model| {
                let display = if model.display_name_long.is_empty() {
                    model.resolved_id.clone()
                } else {
                    model.display_name_long.clone()
                };
                if model.resolved_id.is_empty() || display == model.resolved_id {
                    display
                } else {
                    format!("{display} ({})", model.resolved_id)
                }
            },
        );
        push_system_info(app, format!("Model: {label}"));
        return true;
    }
    let [model_name_arg] = args else {
        push_system_info(app, "Usage: /model [id]");
        return true;
    };
    let model_name = model_name_arg.trim();
    if model_name.is_empty() {
        push_system_info(app, "Usage: /model [id]");
        return true;
    }

    let Some(sid) = require_active_session(
        app,
        "Cannot switch model: not connected yet.",
        "Cannot switch model: no active session.",
    ) else {
        return true;
    };

    switch_model(app, forge_workspace::SessionKey::from_session_id(sid.to_string()), model_name);
    true
}

/// Switch the session `session_key` to `model_name`: optimistic UI apply
/// plus the `SetModel` dispatch. Shared by the `/model <id>` submit path
/// and the `/model` picker's Enter; both validate the session before
/// calling and hand the key down. A no-op with a system notice when the
/// session advertises models and `model_name` is not one of them; the
/// picker's rows come from that same list, so it always passes.
pub(crate) fn switch_model(
    app: &mut App,
    session_key: forge_workspace::SessionKey,
    model_name: &str,
) {
    if !app.available_models().is_empty()
        && !app
            .available_models()
            .iter()
            .any(|candidate| candidate.id.eq_ignore_ascii_case(model_name))
    {
        push_system_message(app, format!("Unknown model: {model_name}"));
        return;
    }

    // Apply CurrentModelUpdate (and a refreshed ModeStateUpdate
    // when the active mode is set) App-side immediately. The apply
    // is synchronous so no `CommandPending` state is needed.
    apply_optimistic_model_change(app, model_name);

    if let Err(e) = app.dispatch_command(|key| forge_workspace::Command::SetModel {
        key,
        model: model_name.to_owned(),
    }) {
        // The command never left, so no SetModelFailed can arrive;
        // undo the optimistic apply here.
        if app.rollback_pending_model() {
            app.invalidate_layout(crate::app::state::LayoutInvalidation::Global);
        }
        let _ = app.update_tx.send(SessionUpdate::SlashCommandError {
            key: session_key,
            message: format!("Failed to run /model: {e}"),
        });
    }
}

fn apply_optimistic_model_change(app: &mut App, model_name: &str) {
    use forge_workspace::commands::{build_mode_state_from_supported, supported_mode_ids_filtered};
    use forge_workspace::session_lifecycle::resolve_current_model_from_inputs;

    // Rapid submits overlap: park only the FIRST pre-apply state, so a
    // rejection of any in-flight request restores the true original and
    // a refusal for a superseded request cannot consume a newer one.
    let rollback = if app.pending_model_rollback().is_none() {
        Some(crate::app::session::ModelRollback {
            current_model: app.current_model().cloned(),
            requested_model_id: app.with_turn_state(|ts| ts.requested_model_id.clone()),
        })
    } else {
        None
    };
    let _: () = app.with_turn_state_mut(|ts| ts.requested_model_id = Some(model_name.to_owned()));
    let (model_id, resolved_runtime) =
        app.with_turn_state(|ts| (ts.model_id.clone(), ts.resolved_runtime_model_id.clone()));
    let next_wire = resolve_current_model_from_inputs(
        &model_id,
        Some(model_name),
        resolved_runtime.as_deref(),
        &[],
    );
    let next_model = next_wire;
    crate::app::events::apply_current_model_update(app, next_model);

    let mode_opt = app.with_turn_state(|ts| ts.mode);
    if let Some(mode) = mode_opt {
        let supports_auto_mode =
            app.current_model().is_some_and(|m| m.supports_auto_mode == Some(true));
        let unavailable_modes = app.with_turn_state(|ts| ts.runtime_unavailable_mode_ids.clone());
        let bypass_offered = crate::app::events::bypass_mode_offered(app);
        let supported = supported_mode_ids_filtered(
            supports_auto_mode,
            bypass_offered,
            Some(mode),
            &unavailable_modes,
        );
        let _: () = app.with_turn_state_mut(|ts| ts.supported_mode_ids.clone_from(&supported));
        let wire_mode_state = build_mode_state_from_supported(mode, &supported);
        let model_mode_state = wire_mode_state;
        crate::app::events::apply_mode_state_update(app, model_mode_state);
    }
    if let Some(rollback) = rollback {
        app.set_pending_model_rollback(Some(rollback));
    }
}

/// `/effort` no-arg → show current effort level.
/// `/effort <level>` → persist the level into ~/.claude/settings.json
/// so the next session launch picks it up (mirrors today's overlay
/// path). There's no live SDK command for effort; the change
/// surfaces on the next session restart.
fn handle_effort_submit(app: &mut App, args: &[&str]) -> bool {
    use crate::agent::model::EffortLevel;

    if args.is_empty() {
        let level = app.config.thinking_effort_effective();
        push_system_info(app, format!("Effort: {} ({})", level.label(), level.as_stored()));
        return true;
    }
    let [requested_arg] = args else {
        push_system_info(app, "Usage: /effort [low|medium|high|xhigh|max]");
        return true;
    };
    let requested = requested_arg.trim();
    if requested.is_empty() {
        push_system_info(app, "Usage: /effort [low|medium|high|xhigh|max]");
        return true;
    }
    let Some(level) = EffortLevel::from_stored(requested) else {
        push_system_message(app, format!("Unknown effort level: {requested}"));
        return true;
    };

    let Some(path) = app.config.settings_path.clone() else {
        push_system_message(app, "Effort: settings path is unavailable");
        return true;
    };
    let mut next_document = app.config.committed_settings_document.clone();
    crate::app::config::store::set_thinking_effort_level(&mut next_document, level);
    match crate::app::config::store::save(&path, &next_document) {
        Ok(()) => {
            app.config.committed_settings_document = next_document;
            push_system_info(app, format!("Effort: {} (takes effect next session)", level.label()));
        }
        Err(err) => push_system_message(app, format!("Failed to save effort: {err}")),
    }
    true
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

/// `/spinner` - no arg shows the current style + the valid keys;
/// `/spinner <name>` sets the active style live across every surface
/// and persists it to the redb store (survives restart, layered over
/// the forge.toml `[ui] spinner` default). Works from chat
/// and the launchpad - no active session required. The no-arg picker
/// overlay lands in the follow-up task.
fn handle_spinner_submit(app: &mut App, args: &[&str]) -> bool {
    use forge_workspace::SpinnerStyle;

    if args.is_empty() {
        crate::app::spinner_picker::open(app);
        return true;
    }
    let valid_keys =
        || SpinnerStyle::ALL_STYLES.iter().map(|s| s.key()).collect::<Vec<_>>().join(", ");
    let [name_arg] = args else {
        push_system_message(app, "Usage: /spinner [name]");
        return true;
    };
    let name = name_arg.trim();
    let Some(style) = SpinnerStyle::from_key(name) else {
        push_system_message(app, format!("Unknown spinner: {name} (valid: {})", valid_keys()));
        return true;
    };

    app.spinner_style = style;
    if let Some(ws) = app.workspace.as_ref() {
        let _ = ws.dispatch(forge_workspace::Command::PersistSpinner { style });
    }
    app.needs_redraw = true;
    push_system_info(app, format!("Spinner: {}", style.key()));
    true
}

fn handle_unknown_submit(app: &mut App, command_name: &str) -> bool {
    if super::candidates::is_supported_command(app, command_name) {
        return false;
    }
    push_system_message(app, format!("{command_name} is not yet supported"));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_workspace::SpinnerStyle;

    #[test]
    fn spinner_name_sets_active_style() {
        let mut app = App::test_default();
        assert_eq!(app.spinner_style, SpinnerStyle::Braille);
        assert!(handle_spinner_submit(&mut app, &["ember"]));
        assert_eq!(app.spinner_style, SpinnerStyle::Ember);
    }

    #[test]
    fn spinner_unknown_name_leaves_style_unchanged() {
        let mut app = App::test_default();
        app.spinner_style = SpinnerStyle::Ember;
        assert!(handle_spinner_submit(&mut app, &["corkscrew"]));
        assert_eq!(
            app.spinner_style,
            SpinnerStyle::Ember,
            "an unknown name must not change the active style",
        );
    }

    #[test]
    fn spinner_no_arg_opens_picker_without_changing_style() {
        let mut app = App::test_default();
        app.spinner_style = SpinnerStyle::PhaseOfMoon;
        assert!(handle_spinner_submit(&mut app, &[]));
        assert!(app.spinner_picker.is_some(), "no-arg opens the picker overlay");
        assert_eq!(
            app.spinner_style,
            SpinnerStyle::PhaseOfMoon,
            "opening the picker must not change the active style",
        );
    }
}
