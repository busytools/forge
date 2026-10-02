//! Slash command executors: dispatching parsed commands to their handler functions.

use super::{
    parse, push_system_info, push_system_message, push_user_message, require_active_session,
    set_command_pending,
};
use crate::app::App;
use forge_workspace::SessionUpdate;

/// One command's handler.
type Handler = fn(&mut App, &[&str]) -> bool;

/// The commands this view answers itself: the name as it is typed, and the
/// handler for it, all of them terminal-side - an overlay, a picker, the
/// launchpad. Everything else a composer can type is either the CLI's or the
/// core's, and neither is decided here.
///
/// A name missing here falls to the unknown-command fallback, so this list
/// and `forge_server::commands::FORGE_COMMANDS` cover the same set between
/// them: the table is what both views offer, and a name it carries that
/// nothing dispatches would be advertised by both dropdowns and refused
/// when typed.
const HANDLERS: &[(&str, Handler)] = &[
    ("/compact", handle_compact_submit),
    ("/dictate", handle_dictate_submit),
    ("/diff", handle_diff_submit),
    ("/extensions", handle_extensions_submit),
    ("/gateway", handle_gateway_submit),
    ("/launchpad", handle_launchpad_submit),
    ("/spinner", handle_spinner_submit),
    ("/usage", handle_usage_submit),
];

/// The names [`HANDLERS`] answers, for the test that compares them with the
/// shared table.
#[cfg(test)]
pub(super) fn handled_names() -> Vec<&'static str> {
    HANDLERS.iter().map(|(name, _)| *name).collect()
}

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
    // A command the core answers is the core's wherever it is typed: send
    // the words as a prompt and let the one interception run them, which is
    // the same path a client's send takes.
    if forge_workspace::prompt::is_forge_prompt_name(parsed.name) {
        // Bare `/model` is the one place this view answers first, because
        // the answer is a picker: which model to run is the core's, but
        // offering the rows is presentation. A session that advertises no
        // models opens nothing and the core answers instead.
        if parsed.name == "/model" && parsed.args.is_empty() && crate::app::model_picker::open(app)
        {
            return true;
        }
        return forward_to_core(app, text);
    }
    match HANDLERS.iter().find(|(name, _)| *name == parsed.name) {
        Some((_, handler)) => handler(app, &parsed.args),
        None => handle_unknown_submit(app, parsed.name),
    }
}

/// Send one of forge's own commands to the core, drawing the words the way
/// this view draws its own submits.
///
/// The core acts on them, so nothing here decides what they do - which is
/// what keeps the terminal and a client on one path.
fn forward_to_core(app: &mut App, text: &str) -> bool {
    push_user_message(app, text);
    // These commands are not instant: the core re-spawns a session behind
    // them, so the input stays blocked until the replacement lands.
    set_command_pending(app, &format!("Running {text}..."), None);
    if let Err(err) = app.dispatch_command(|key| forge_workspace::Command::Prompt {
        key,
        text: text.to_owned(),
        attachments: Vec::new(),
    }) {
        if let Some(key) = app.active_session_key.clone() {
            let _ = app.update_tx.send(SessionUpdate::SlashCommandError {
                key,
                message: format!("Failed to run {text}: {err}"),
            });
        } else {
            tracing::warn!(
                target: crate::logging::targets::APP_COMMAND,
                event_name = "slash_error_without_session",
                message = "a forge command failed with no session to report it against",
                outcome = "skipped",
                error_message = %err,
            );
        }
    }
    true
}

/// Open the read-only `/gateway` view: every org the gateway holds,
/// its pins, and each account's live state. No in-flight gate - it
/// inspects and never acts, so it is safe to open mid-turn.
fn handle_gateway_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /gateway");
        return true;
    }
    let Some(workspace) = app.workspace.clone() else {
        return true;
    };
    let orgs = forge_server::surface::ViewSurface::new(workspace).accounts().orgs;
    crate::app::gateway_view::open(app, orgs);
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

fn handle_extensions_submit(app: &mut App, args: &[&str]) -> bool {
    if !args.is_empty() {
        push_system_message(app, "Usage: /extensions");
        return true;
    }

    if let Err(err) = crate::app::config::open_extensions(app) {
        push_system_message(app, format!("Failed to open extensions: {err}"));
    }
    true
}

/// Switch the session `session_key` to `model_name`: optimistic UI apply
/// plus the `SetModel` dispatch. The `/model` picker's Enter calls this;
/// the typed form goes to the core, which dispatches the same command.
///
/// A no-op with a system notice when the session advertises models and
/// `model_name` is not one of them; the picker's rows come from that same
/// list, so it always passes.
pub(crate) fn switch_model(
    app: &mut App,
    session_key: forge_workspace::SessionSlot,
    model_name: &str,
) {
    let models = app.available_models().unwrap_or_default();
    if !models.is_empty()
        && !models.iter().any(|candidate| candidate.id.eq_ignore_ascii_case(model_name))
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

/// `/spinner` - no arg opens the style picker; `/spinner <name>` sets the
/// style for this run. Works from chat and the launchpad - no active
/// session required.
fn handle_spinner_submit(app: &mut App, args: &[&str]) -> bool {
    use crate::ui::spinner_style::SpinnerStyle;

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

    // For this run only: a spinner is the terminal's presentation, and the
    // server keeps no key, no store row and no command for one.
    app.spinner_style = style;
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
    use crate::ui::spinner_style::SpinnerStyle;

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
    fn gateway_takes_no_arguments() {
        let mut app = App::test_default();
        assert!(handle_gateway_submit(&mut app, &["extra"]));
        assert!(app.gateway_view.is_none(), "a bad invocation must not open the view");
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
