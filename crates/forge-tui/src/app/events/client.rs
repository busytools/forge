use super::{App, session, turn};
use crate::agent::events::ClientEvent;
use forge_workspace::SessionKey;

/// Early-return from the enclosing function when `incoming` doesn't
/// match the App's current session id. Logs a stale-session drop
/// breadcrumb under the given `target` / `event_name` / `message`
/// before returning. Used by every per-session-id event handler in
/// this file to keep the guard from sprawling 7+ times.
macro_rules! drop_if_stale_session {
    ($app:expr, $session_id:expr, $target:expr, $event_name:expr, $message:expr) => {
        if $app.session_id().map(ToString::to_string).as_deref()
            != Some($session_id.as_str())
        {
            tracing::debug!(
                target: $target,
                event_name = $event_name,
                message = $message,
                outcome = "dropped",
                session_id = %$session_id,
                reason = "stale_session",
            );
            return;
        }
    };
}

/// Per-session event multiplexer. Each [`ClientEvent`] is routed to
/// the [`crate::app::session::Session`] bucket it targets, derived
/// from its session_key (variants without inherent routing carry
/// `session_key` explicitly; variants with `session_id` derive at
/// routing time via [`ClientEvent::session_key`]).
///
/// `needs_redraw` is flipped only when the routed event targets the
/// active session — background-session events update their bucket
/// silently. App-global events (no `session_key`) flip the redraw
/// flag unconditionally because they affect the rendered view.
pub fn handle_client_event(app: &mut App, event: ClientEvent) {
    let target_key = event.session_key();
    let is_active_or_global = match &target_key {
        Some(key) => app.active_session_key.as_ref() == Some(key),
        None => true,
    };
    match event {
        ClientEvent::PermissionRequest { request, response_tx } => {
            turn::handle_permission_request_event(app, request, response_tx);
        }
        ClientEvent::QuestionRequest { request, response_tx } => {
            turn::handle_question_request_event(app, request, response_tx);
        }
        ClientEvent::McpElicitationRequest { request, .. } => {
            crate::app::config::present_mcp_elicitation_request(app, request);
        }
        ClientEvent::McpAuthRedirect { redirect, .. } => {
            crate::app::config::present_mcp_auth_redirect(app, redirect);
        }
        ClientEvent::McpOperationError { error, .. } => {
            crate::app::config::handle_mcp_operation_error(app, &error);
        }
        ClientEvent::McpElicitationCompleted { elicitation_id, server_name, .. } => {
            crate::app::config::handle_mcp_elicitation_completed(app, &elicitation_id, server_name);
        }
        ClientEvent::TurnCancelled { .. } => turn::handle_turn_cancelled_event(app),
        ClientEvent::TurnComplete { terminal_reason, .. } => {
            turn::handle_turn_complete_event(app, terminal_reason);
        }
        ClientEvent::TurnError { message, terminal_reason, .. } => {
            turn::handle_turn_error_event(app, &message, None, terminal_reason);
        }
        ClientEvent::TurnErrorClassified { message, class, terminal_reason, .. } => {
            turn::handle_turn_error_event(app, &message, Some(class), terminal_reason);
        }
        ClientEvent::Connected {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
        } => {
            session::handle_connected_client_event(
                app,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                &history_updates,
            );
            crate::app::config::refresh_mcp_snapshot(app);
            crate::app::session_runtime::request_status_snapshot_refresh(app);
            crate::app::session_runtime::request_oauth_credentials_snapshot_refresh(app);
            crate::app::session_runtime::request_context_usage_refresh(app);
        }
        ClientEvent::SessionsListed { sessions } => {
            session::handle_sessions_listed_event(app, sessions);
        }
        ClientEvent::AuthRequired { method_name, method_description, .. } => {
            session::handle_auth_required_event(app, method_name, method_description);
        }
        ClientEvent::ConnectionFailed { message, .. } => {
            session::handle_connection_failed_event(app, &message);
        }
        ClientEvent::SlashCommandError { message, .. } => {
            session::handle_slash_command_error_event(app, &message);
        }
        ClientEvent::RuntimeReloadCompleted { session_id } => {
            drop_if_stale_session!(
                app,
                session_id,
                crate::logging::targets::APP_CONFIG,
                "runtime_reload_completed_dropped",
                "runtime reload completion dropped for a stale session"
            );
            crate::app::plugins::apply_runtime_reload_success(app);
        }
        ClientEvent::RuntimeReloadFailed { session_id, message } => {
            drop_if_stale_session!(
                app,
                session_id,
                crate::logging::targets::APP_CONFIG,
                "runtime_reload_failed_dropped",
                "runtime reload failure dropped for a stale session"
            );
            crate::app::plugins::apply_runtime_reload_failure(app, &message);
        }
        ClientEvent::SessionReplaced {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
        } => {
            session::handle_session_replaced_event(
                app,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                &history_updates,
            );
            crate::app::config::refresh_mcp_snapshot(app);
            crate::app::session_runtime::request_status_snapshot_refresh(app);
            crate::app::session_runtime::request_oauth_credentials_snapshot_refresh(app);
            crate::app::session_runtime::request_context_usage_refresh(app);
        }
        ClientEvent::ServiceStatus { severity, message } => {
            session::handle_service_status_event(app, severity, &message);
        }
        ClientEvent::AuthCompleted { conn, .. } => {
            session::handle_auth_completed_event(app, &conn);
        }
        ClientEvent::LogoutCompleted { .. } => {
            session::handle_logout_completed_event(app);
        }
        ClientEvent::ForgeAccountIdentityReady { session_key, display_name } => {
            tracing::info!(
                target: crate::logging::targets::APP_AUTH,
                event_name = "forge_account_identity_ready",
                message = "forge-account display_name received pre-status-snapshot",
                outcome = "info",
                display_name = %display_name,
                session_key = %session_key.as_str(),
            );
            apply_forge_account_identity(app, &session_key, display_name);
        }
        ClientEvent::StatusSnapshotReceived { session_id, account, forge_account } => {
            apply_status_snapshot(app, &session_id, account, forge_account);
        }
        ClientEvent::OauthCredentialsSnapshotReceived { session_id, credentials } => {
            drop_if_stale_session!(
                app,
                session_id,
                crate::logging::targets::APP_AUTH,
                "oauth_credentials_snapshot_dropped",
                "oauth credentials snapshot dropped for a stale session"
            );
            let has_credentials = credentials.is_some();
            let has_expiry = credentials.as_ref().is_some_and(|info| info.expires_at.is_some());
            app.set_oauth_credentials(credentials);
            tracing::info!(
                target: crate::logging::targets::APP_AUTH,
                event_name = "oauth_credentials_snapshot_applied",
                message = "oauth credentials snapshot applied",
                outcome = "success",
                session_id = %session_id,
                has_credentials,
                has_expiry,
            );
        }
        ClientEvent::GitContextSnapshotReceived { session_id, context } => {
            drop_if_stale_session!(
                app,
                session_id,
                crate::logging::targets::APP_SESSION,
                "git_context_snapshot_dropped",
                "git context snapshot dropped for a stale session"
            );
            app.apply_git_context_snapshot(context);
        }
        ClientEvent::ContextUsageReceived { session_id, percentage } => {
            drop_if_stale_session!(
                app,
                session_id,
                crate::logging::targets::APP_SESSION,
                "context_usage_dropped",
                "context usage dropped for a stale session"
            );
            crate::app::session_runtime::apply_context_usage_snapshot(app, percentage);
        }
        ClientEvent::McpSnapshotReceived { session_id, servers, error } => {
            drop_if_stale_session!(
                app,
                session_id,
                crate::logging::targets::APP_CONFIG,
                "mcp_snapshot_dropped",
                "MCP snapshot dropped for a stale session"
            );
            let server_count = servers.len();
            let error_present = error.is_some();
            {
                let mcp = app.mcp_mut();
                mcp.servers = servers;
                mcp.in_flight = false;
                mcp.last_error = error;
            }
            app.config.mcp_selected_server_index =
                app.config.mcp_selected_server_index.min(app.mcp().servers.len().saturating_sub(1));
            if let Some(overlay) = app.config.mcp_auth_redirect_overlay() {
                let server_name = overlay.redirect.server_name.clone();
                if let Some(server) =
                    app.mcp().servers.iter().find(|server| server.name == server_name)
                    && !matches!(
                        server.status,
                        forge_primitives::McpServerConnectionStatus::NeedsAuth
                            | forge_primitives::McpServerConnectionStatus::Pending
                    )
                {
                    if matches!(
                        server.status,
                        forge_primitives::McpServerConnectionStatus::Connected
                    ) {
                        app.config.status_message =
                            Some(format!("{} authenticated successfully.", server.name));
                        app.config.last_error = None;
                    }
                    app.config.overlay = None;
                }
            }
            tracing::info!(
                target: crate::logging::targets::APP_CONFIG,
                event_name = "mcp_snapshot_applied",
                message = "MCP snapshot applied",
                outcome = "success",
                session_id = %session_id,
                server_count,
                error_present,
            );
        }
        ClientEvent::SdkMessageReceived { session_id, msg } => {
            // For new sessions the CLI doesn't emit `system/init`
            // until AFTER the first user message lands (per
            // `spawn_inner` docs), so `Client::session_id()` is empty
            // at spawn time and that empty value rides through
            // `Connected` onto `app.session_id`. The first wire
            // message that DOES carry a real id (Assistant / User /
            // Result / System(init)) is the canonical source — adopt
            // it. For resume the bridge already used the resume_id
            // for Connected, so adoption is a no-op and the strict
            // mismatch check covers stale-Client races during session
            // swap.
            let Some(current) = app.session_id() else {
                // No session yet — Connected hasn't been processed.
                // The bridge emits Connected before spawning the
                // reader, so this should be unreachable; drop
                // defensively.
                return;
            };
            let current_str = current.to_string();
            if current_str.is_empty() && !session_id.is_empty() {
                app.set_session_id(Some(crate::agent::model::SessionId::new(session_id.clone())));
            } else if !current_str.is_empty() && current_str != session_id {
                return;
            }
            super::sdk_message::handle_sdk_message(app, msg);
        }
        ClientEvent::HookObservation {
            session_id,
            tool_use_id,
            permission_mode,
            effort,
            agent_id,
            agent_type,
        } => {
            drop_if_stale_session!(
                app,
                session_id,
                crate::logging::targets::APP_SESSION,
                "hook_observation_dropped",
                "hook observation dropped for a stale session"
            );
            apply_hook_observation(
                app,
                tool_use_id.as_deref(),
                permission_mode.as_deref(),
                effort.as_deref(),
                agent_id.as_deref(),
                agent_type.as_deref(),
            );
        }
        ClientEvent::UsageRefreshStarted { epoch } => {
            if app.session_scope_epoch() != epoch {
                return;
            }
            crate::app::usage::apply_refresh_started(app);
        }
        ClientEvent::UsageSnapshotReceived { epoch, snapshot } => {
            if app.session_scope_epoch() != epoch {
                return;
            }
            crate::app::usage::apply_refresh_success(app, snapshot);
        }
        ClientEvent::UsageRefreshFailed { epoch, message, source } => {
            if app.session_scope_epoch() != epoch {
                return;
            }
            crate::app::usage::apply_refresh_failure(app, message, source);
        }
        ClientEvent::PluginsInventoryUpdated { cwd_raw, snapshot, claude_path } => {
            if app.cwd_raw() != cwd_raw {
                return;
            }
            crate::app::plugins::apply_inventory_refresh_success(app, snapshot, claude_path);
        }
        ClientEvent::PluginsInventoryRefreshFailed { cwd_raw, message } => {
            if app.cwd_raw() != cwd_raw {
                return;
            }
            crate::app::plugins::apply_inventory_refresh_failure(app, message);
        }
        ClientEvent::PluginsCliActionSucceeded { cwd_raw, result } => {
            if app.cwd_raw() != cwd_raw {
                return;
            }
            crate::app::plugins::apply_cli_action_success(app, result);
        }
        ClientEvent::PluginsCliActionFailed { cwd_raw, message } => {
            if app.cwd_raw() != cwd_raw {
                return;
            }
            crate::app::plugins::apply_cli_action_failure(app, message);
        }
        ClientEvent::FatalError(error) => session::handle_fatal_error_event(app, error),
    }
    if is_active_or_global {
        app.needs_redraw = true;
    }
}

/// Apply a [`ClientEvent::ForgeAccountIdentityReady`] event to the
/// session bucket addressed by `session_key`. Active-session
/// targeting goes through the existing
/// [`crate::app::App::set_active_account_display_name`] accessor +
/// [`crate::app::App::sync_welcome_snapshot`] so welcome rendering
/// updates promptly. Background-session targeting writes the
/// display name directly into the bucket without touching the
/// active-session welcome snapshot.
fn apply_forge_account_identity(app: &mut App, session_key: &SessionKey, display_name: String) {
    if app.active_session_key.as_ref() == Some(session_key) {
        app.set_active_account_display_name(Some(display_name));
        app.sync_welcome_snapshot();
        return;
    }
    let Some(session) = app.session_mut(session_key) else {
        tracing::debug!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "forge_account_identity_dropped",
            message = "forge-account identity dropped for an unknown session",
            outcome = "dropped",
            session_key = %session_key.as_str(),
            reason = "unknown_session",
        );
        return;
    };
    session.active_account_display_name = Some(display_name);
}

/// Apply a [`ClientEvent::StatusSnapshotReceived`] event to the
/// session bucket addressed by `session_id`. Routes through the
/// active-session accessors when targeting the rendered session
/// (so welcome + Status panel rerender promptly); writes directly
/// into the bucket otherwise so background sessions accumulate
/// state silently.
fn apply_status_snapshot(
    app: &mut App,
    session_id: &str,
    account: forge_primitives::AccountInfo,
    forge_account: Option<forge_primitives::ForgeAccountIdentity>,
) {
    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let has_email = account.email.as_deref().is_some_and(|email| !email.trim().is_empty());
    let has_organization = account.organization.is_some();
    let subscription_type = account.subscription_type.clone();
    let token_source = account.token_source.clone();
    let api_key_source = account.api_key_source.clone();
    let api_provider = account.api_provider.clone();
    let forge_display_name = forge_account.as_ref().map(|f| f.display_name.clone());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        app.set_account_info(Some(account));
        app.set_active_account_display_name(forge_account.map(|f| f.display_name));
        app.sync_welcome_snapshot();
    } else if let Some(session) = app.session_mut(&session_key) {
        session.account_info = Some(account);
        session.active_account_display_name = forge_account.map(|f| f.display_name);
    } else {
        tracing::debug!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "status_snapshot_dropped",
            message = "status snapshot dropped for an unknown session",
            outcome = "dropped",
            session_id = %session_id,
            reason = "unknown_session",
        );
        return;
    }
    tracing::info!(
        target: crate::logging::targets::APP_AUTH,
        event_name = "status_snapshot_applied",
        message = "status snapshot applied",
        outcome = "success",
        session_id = %session_id,
        is_active,
        has_email,
        has_organization,
        subscription_type = ?subscription_type,
        token_source = ?token_source,
        api_key_source = ?api_key_source,
        api_provider = ?api_provider,
        forge_display_name = ?forge_display_name,
    );
}

/// Apply a hook-input observation to App state. Called from
/// `ClientEvent::HookObservation` after the stale-session guard.
///
/// - `permission_mode`: typed via `PermissionMode::from_wire`. Stored
///   on `app.observed_permission_mode`; the mode chip prefers this
///   over `app.mode` when set.
/// - `effort`: typed via `EffortLevel`'s deserialiser. Stored on
///   `app.observed_effort`; the effort chip prefers this when set.
/// - `agent_id` + `agent_type`: when both are present, store the type
///   under the `tool_use_id` key. Tool-call rows render the type as a
///   suffix on subagent rows.
fn apply_hook_observation(
    app: &mut crate::app::App,
    tool_use_id: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    agent_id: Option<&str>,
    agent_type: Option<&str>,
) {
    use crate::agent::model::EffortLevel;
    use crate::agent::state::PermissionMode;

    if let Some(mode_str) = permission_mode
        && let Some(mode) = PermissionMode::from_wire(mode_str)
    {
        app.set_observed_permission_mode(Some(mode));
    }

    if let Some(effort_str) = effort {
        let level = match effort_str {
            "low" => Some(EffortLevel::Low),
            "medium" => Some(EffortLevel::Medium),
            "high" => Some(EffortLevel::High),
            "xhigh" => Some(EffortLevel::Xhigh),
            "max" => Some(EffortLevel::Max),
            _ => {
                tracing::warn!(
                    target: crate::logging::targets::APP_SESSION,
                    effort = %effort_str,
                    "hook_observation: unknown effort level; ignored",
                );
                None
            }
        };
        if let Some(level) = level {
            app.set_observed_effort(Some(level));
        }
    }

    if let (Some(tool_use_id), Some(_agent_id), Some(agent_type)) =
        (tool_use_id, agent_id, agent_type)
    {
        app.subagent_attribution_mut().insert(tool_use_id.to_owned(), agent_type.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::Session;

    /// Multiplexer-isolation test: a `StatusSnapshotReceived` event
    /// tagged for session B updates B's bucket without touching
    /// session A's bucket and without flipping `needs_redraw` —
    /// `needs_redraw` flips only for events that target the active
    /// session (A in this fixture). Proves the per-session
    /// multiplexer correctly routes background-session events.
    #[test]
    fn background_event_updates_target_session_only() {
        let mut app = App::test_default();

        // Two real session buckets keyed off claude-issued UUIDs.
        // The pre-Connect synthetic bucket from `App::test_default`
        // stays in the map; we only care that A and B are present.
        let key_a = SessionKey::from_str_for_test("session-a");
        let key_b = SessionKey::from_str_for_test("session-b");
        app.sessions.insert(key_a.clone(), Session::new(key_a.clone()));
        app.sessions.insert(key_b.clone(), Session::new(key_b.clone()));
        app.active_session_key = Some(key_a.clone());

        // Baseline: neither bucket has account_info; needs_redraw is
        // dropped on the floor before the event so we can detect a
        // false positive flip.
        app.needs_redraw = false;
        assert!(app.sessions.get(&key_a).expect("a").account_info.is_none());
        assert!(app.sessions.get(&key_b).expect("b").account_info.is_none());

        // Fire a state-change event tagged for B.
        let account = forge_primitives::AccountInfo {
            email: Some("b@example.com".to_owned()),
            ..Default::default()
        };
        handle_client_event(
            &mut app,
            ClientEvent::StatusSnapshotReceived {
                session_id: key_b.as_str().to_owned(),
                account,
                forge_account: None,
            },
        );

        // B's bucket reflects the change.
        let b = app.sessions.get(&key_b).expect("b");
        assert_eq!(b.account_info.as_ref().and_then(|a| a.email.as_deref()), Some("b@example.com"));

        // A's bucket is untouched.
        let a = app.sessions.get(&key_a).expect("a");
        assert!(a.account_info.is_none(), "session A's account_info must not be set");

        // Active session is A; redraw flag must NOT flip for an event
        // routed to a background bucket.
        assert!(!app.needs_redraw, "needs_redraw must stay false for background-session events");
    }
}
