use super::{App, session, turn};
use crate::agent::events::ClientEvent;

/// Early-return from the enclosing function when `incoming` doesn't
/// match the App's current session id. Logs a stale-session drop
/// breadcrumb under the given `target` / `event_name` / `message`
/// before returning. Used by every per-session-id event handler in
/// this file to keep the guard from sprawling 7+ times.
macro_rules! drop_if_stale_session {
    ($app:expr, $session_id:expr, $target:expr, $event_name:expr, $message:expr) => {
        if $app.session_id.as_ref().map(ToString::to_string).as_deref()
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

#[allow(clippy::too_many_lines)]
pub fn handle_client_event(app: &mut App, event: ClientEvent) {
    app.needs_redraw = true;
    match event {
        ClientEvent::SessionUpdate(update) => super::handle_session_update_event(app, update),
        ClientEvent::PermissionRequest { request, response_tx } => {
            turn::handle_permission_request_event(app, request, response_tx);
        }
        ClientEvent::QuestionRequest { request, response_tx } => {
            turn::handle_question_request_event(app, request, response_tx);
        }
        ClientEvent::McpElicitationRequest { request } => {
            crate::app::config::present_mcp_elicitation_request(app, request);
        }
        ClientEvent::McpAuthRedirect { redirect } => {
            crate::app::config::present_mcp_auth_redirect(app, redirect);
        }
        ClientEvent::McpOperationError { error } => {
            crate::app::config::handle_mcp_operation_error(app, &error);
        }
        ClientEvent::McpElicitationCompleted { elicitation_id, server_name } => {
            crate::app::config::handle_mcp_elicitation_completed(app, &elicitation_id, server_name);
        }
        ClientEvent::TurnCancelled => turn::handle_turn_cancelled_event(app),
        ClientEvent::TurnComplete { terminal_reason } => {
            turn::handle_turn_complete_event(app, terminal_reason);
        }
        ClientEvent::TurnError { message, terminal_reason } => {
            turn::handle_turn_error_event(app, &message, None, terminal_reason);
        }
        ClientEvent::TurnErrorClassified { message, class, terminal_reason } => {
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
        ClientEvent::AuthRequired { method_name, method_description } => {
            session::handle_auth_required_event(app, method_name, method_description);
        }
        ClientEvent::ConnectionFailed(msg) => {
            session::handle_connection_failed_event(app, &msg);
        }
        ClientEvent::SlashCommandError(msg) => {
            session::handle_slash_command_error_event(app, &msg);
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
        ClientEvent::AuthCompleted { conn } => {
            session::handle_auth_completed_event(app, &conn);
        }
        ClientEvent::LogoutCompleted => {
            session::handle_logout_completed_event(app);
        }
        ClientEvent::StatusSnapshotReceived { session_id, account } => {
            drop_if_stale_session!(
                app,
                session_id,
                crate::logging::targets::APP_AUTH,
                "status_snapshot_dropped",
                "status snapshot dropped for a stale session"
            );
            let has_email = account.email.as_deref().is_some_and(|email| !email.trim().is_empty());
            let has_organization = account.organization.is_some();
            let subscription_type = account.subscription_type.clone();
            let token_source = account.token_source.clone();
            let api_key_source = account.api_key_source.clone();
            let api_provider = account.api_provider.clone();
            app.account_info = Some(account);
            app.sync_welcome_snapshot();
            app.needs_redraw = true;
            tracing::info!(
                target: crate::logging::targets::APP_AUTH,
                event_name = "status_snapshot_applied",
                message = "status snapshot applied",
                outcome = "success",
                session_id = %session_id,
                has_email,
                has_organization,
                subscription_type = ?subscription_type,
                token_source = ?token_source,
                api_key_source = ?api_key_source,
                api_provider = ?api_provider,
            );
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
            app.oauth_credentials = credentials;
            app.needs_redraw = true;
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
            app.mcp.servers = servers;
            app.mcp.in_flight = false;
            app.mcp.last_error = error;
            app.config.mcp_selected_server_index =
                app.config.mcp_selected_server_index.min(app.mcp.servers.len().saturating_sub(1));
            if let Some(overlay) = app.config.mcp_auth_redirect_overlay() {
                let server_name = overlay.redirect.server_name.clone();
                if let Some(server) =
                    app.mcp.servers.iter().find(|server| server.name == server_name)
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
            let Some(current) = app.session_id.as_ref() else {
                // No session yet — Connected hasn't been processed.
                // The bridge emits Connected before spawning the
                // reader, so this should be unreachable; drop
                // defensively.
                return;
            };
            let current_str = current.to_string();
            if current_str.is_empty() && !session_id.is_empty() {
                app.session_id = Some(crate::agent::model::SessionId::new(session_id.clone()));
            } else if !current_str.is_empty() && current_str != session_id {
                return;
            }
            super::sdk_message::handle_sdk_message(app, msg);
        }
        ClientEvent::UsageRefreshStarted { epoch } => {
            if app.session_scope_epoch != epoch {
                return;
            }
            crate::app::usage::apply_refresh_started(app);
        }
        ClientEvent::UsageSnapshotReceived { epoch, snapshot } => {
            if app.session_scope_epoch != epoch {
                return;
            }
            crate::app::usage::apply_refresh_success(app, snapshot);
        }
        ClientEvent::UsageRefreshFailed { epoch, message, source } => {
            if app.session_scope_epoch != epoch {
                return;
            }
            crate::app::usage::apply_refresh_failure(app, message, source);
        }
        ClientEvent::PluginsInventoryUpdated { cwd_raw, snapshot, claude_path } => {
            if app.cwd_raw != cwd_raw {
                return;
            }
            crate::app::plugins::apply_inventory_refresh_success(app, snapshot, claude_path);
        }
        ClientEvent::PluginsInventoryRefreshFailed { cwd_raw, message } => {
            if app.cwd_raw != cwd_raw {
                return;
            }
            crate::app::plugins::apply_inventory_refresh_failure(app, message);
        }
        ClientEvent::PluginsCliActionSucceeded { cwd_raw, result } => {
            if app.cwd_raw != cwd_raw {
                return;
            }
            crate::app::plugins::apply_cli_action_success(app, result);
        }
        ClientEvent::PluginsCliActionFailed { cwd_raw, message } => {
            if app.cwd_raw != cwd_raw {
                return;
            }
            crate::app::plugins::apply_cli_action_failure(app, message);
        }
        ClientEvent::FatalError(error) => session::handle_fatal_error_event(app, error),
    }
}
