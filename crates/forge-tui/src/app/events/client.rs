use super::{App, session, turn};
use crate::agent::events::ClientEvent;
use forge_workspace::SessionKey;

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
    // INVARIANT: `is_active_or_global` is captured BEFORE the match, so
    // handlers that themselves mutate `active_session_key` (e.g.
    // `Connected`, `SessionReplaced`) must set `needs_redraw = true`
    // explicitly via their own side effects. The post-match flip below
    // sees the pre-handler active_session_key. Today the handlers that
    // change the active key already trip needs_redraw via
    // `reset_cache_and_footer_state_for_new_session`; new handlers in
    // that category must do the same, OR move the redraw flip back
    // inside each handler.
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
            // App-global UI overlay; only meaningful for the active
            // session. Drop for background sessions.
            if is_active_or_global {
                crate::app::config::present_mcp_elicitation_request(app, request);
            }
        }
        ClientEvent::McpAuthRedirect { redirect, .. } => {
            if is_active_or_global {
                crate::app::config::present_mcp_auth_redirect(app, redirect);
            }
        }
        ClientEvent::McpOperationError { error, .. } => {
            if is_active_or_global {
                crate::app::config::handle_mcp_operation_error(app, &error);
            }
        }
        ClientEvent::McpElicitationCompleted { elicitation_id, server_name, .. } => {
            if is_active_or_global {
                crate::app::config::handle_mcp_elicitation_completed(
                    app,
                    &elicitation_id,
                    server_name,
                );
            }
        }
        ClientEvent::TurnCancelled { session_key } => {
            turn::handle_turn_cancelled_event(app, &session_key);
        }
        ClientEvent::TurnComplete { session_key, terminal_reason } => {
            turn::handle_turn_complete_event(app, &session_key, terminal_reason);
        }
        ClientEvent::TurnError { session_key, message, terminal_reason } => {
            turn::handle_turn_error_event(app, &session_key, &message, None, terminal_reason);
        }
        ClientEvent::TurnErrorClassified { session_key, message, class, terminal_reason } => {
            turn::handle_turn_error_event(
                app,
                &session_key,
                &message,
                Some(class),
                terminal_reason,
            );
        }
        ClientEvent::Connected {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
            pre_connect_key,
        } => {
            session::handle_connected_client_event(
                app,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                &history_updates,
                pre_connect_key,
            );
            crate::app::config::refresh_mcp_snapshot(app);
            crate::app::session_runtime::request_status_snapshot_refresh(app);
            crate::app::session_runtime::request_oauth_credentials_snapshot_refresh(app);
            crate::app::session_runtime::request_context_usage_refresh(app);
        }
        ClientEvent::SessionsListed { sessions } => {
            session::handle_sessions_listed_event(app, sessions);
        }
        ClientEvent::AuthRequired { session_key, method_name, method_description } => {
            session::handle_auth_required_event(app, &session_key, method_name, method_description);
        }
        ClientEvent::ConnectionFailed { session_key, message } => {
            session::handle_connection_failed_event(app, &session_key, &message);
        }
        ClientEvent::SlashCommandError { session_key, message } => {
            session::handle_slash_command_error_event(app, &session_key, &message);
        }
        ClientEvent::RuntimeReloadCompleted { session_id } => {
            apply_runtime_reload_completed(app, &session_id);
        }
        ClientEvent::RuntimeReloadFailed { session_id, message } => {
            apply_runtime_reload_failed(app, &session_id, &message);
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
        ClientEvent::AuthCompleted { session_key, conn } => {
            session::handle_auth_completed_event(app, &session_key, &conn);
        }
        ClientEvent::LogoutCompleted { session_key } => {
            session::handle_logout_completed_event(app, &session_key);
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
            apply_oauth_credentials_snapshot(app, &session_id, credentials);
        }
        ClientEvent::GitContextSnapshotReceived { session_id, context } => {
            apply_git_context_snapshot_routed(app, &session_id, context);
        }
        ClientEvent::ContextUsageReceived { session_id, percentage } => {
            apply_context_usage_snapshot_routed(app, &session_id, percentage);
        }
        ClientEvent::McpSnapshotReceived { session_id, servers, error } => {
            apply_mcp_snapshot_routed(app, &session_id, servers, error);
        }
        ClientEvent::SdkMessageReceived { session_id, msg } => {
            apply_sdk_message_routed(app, &session_id, msg);
        }
        ClientEvent::HookObservation {
            session_id,
            tool_use_id,
            permission_mode,
            effort,
            agent_id,
            agent_type,
        } => {
            apply_hook_observation_routed(
                app,
                &session_id,
                tool_use_id.as_deref(),
                permission_mode.as_deref(),
                effort.as_deref(),
                agent_id.as_deref(),
                agent_type.as_deref(),
            );
        }
        ClientEvent::UsageRefreshStarted { epoch } => {
            if app.session_scope_epoch() != epoch {
                tracing::debug!(
                    target: crate::logging::targets::APP_CONFIG,
                    event_name = "usage_refresh_started_dropped",
                    expected_epoch = app.session_scope_epoch(),
                    received_epoch = epoch,
                    "stale usage refresh start dropped"
                );
                return;
            }
            crate::app::usage::apply_refresh_started(app);
        }
        ClientEvent::UsageSnapshotReceived { epoch, snapshot } => {
            if app.session_scope_epoch() != epoch {
                tracing::debug!(
                    target: crate::logging::targets::APP_CONFIG,
                    event_name = "usage_snapshot_dropped",
                    expected_epoch = app.session_scope_epoch(),
                    received_epoch = epoch,
                    "stale usage snapshot dropped"
                );
                return;
            }
            crate::app::usage::apply_refresh_success(app, snapshot);
        }
        ClientEvent::UsageRefreshFailed { epoch, message, source } => {
            if app.session_scope_epoch() != epoch {
                tracing::debug!(
                    target: crate::logging::targets::APP_CONFIG,
                    event_name = "usage_refresh_failure_dropped",
                    expected_epoch = app.session_scope_epoch(),
                    received_epoch = epoch,
                    "stale usage refresh failure dropped"
                );
                return;
            }
            crate::app::usage::apply_refresh_failure(app, message, source);
        }
        ClientEvent::PluginsInventoryUpdated { cwd_raw, snapshot, claude_path } => {
            if app.cwd_raw() != cwd_raw {
                tracing::debug!(
                    target: crate::logging::targets::APP_CONFIG,
                    event_name = "plugins_inventory_dropped",
                    expected_cwd = %app.cwd_raw(),
                    received_cwd = %cwd_raw,
                    "plugins inventory for stale cwd dropped"
                );
                return;
            }
            crate::app::plugins::apply_inventory_refresh_success(app, snapshot, claude_path);
        }
        ClientEvent::PluginsInventoryRefreshFailed { cwd_raw, message } => {
            if app.cwd_raw() != cwd_raw {
                tracing::debug!(
                    target: crate::logging::targets::APP_CONFIG,
                    event_name = "plugins_inventory_failure_dropped",
                    expected_cwd = %app.cwd_raw(),
                    received_cwd = %cwd_raw,
                    "plugins inventory failure for stale cwd dropped"
                );
                return;
            }
            crate::app::plugins::apply_inventory_refresh_failure(app, message);
        }
        ClientEvent::PluginsCliActionSucceeded { cwd_raw, result } => {
            if app.cwd_raw() != cwd_raw {
                tracing::debug!(
                    target: crate::logging::targets::APP_CONFIG,
                    event_name = "plugins_cli_success_dropped",
                    expected_cwd = %app.cwd_raw(),
                    received_cwd = %cwd_raw,
                    "plugins cli success for stale cwd dropped"
                );
                return;
            }
            crate::app::plugins::apply_cli_action_success(app, result);
        }
        ClientEvent::PluginsCliActionFailed { cwd_raw, message } => {
            if app.cwd_raw() != cwd_raw {
                tracing::debug!(
                    target: crate::logging::targets::APP_CONFIG,
                    event_name = "plugins_cli_failure_dropped",
                    expected_cwd = %app.cwd_raw(),
                    received_cwd = %cwd_raw,
                    "plugins cli failure for stale cwd dropped"
                );
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
        tracing::warn!(
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
        tracing::warn!(
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

/// Apply a [`ClientEvent::OauthCredentialsSnapshotReceived`] event
/// to the session bucket addressed by `session_id`. Active-session
/// targeting goes through [`crate::app::App::set_oauth_credentials`];
/// background-session targeting writes directly into the bucket.
fn apply_oauth_credentials_snapshot(
    app: &mut App,
    session_id: &str,
    credentials: Option<forge_agent::cloud::oauth_credentials::OauthCredentials>,
) {
    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let has_credentials = credentials.is_some();
    let has_expiry = credentials.as_ref().is_some_and(|info| info.expires_at.is_some());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        app.set_oauth_credentials(credentials);
    } else if let Some(session) = app.session_mut(&session_key) {
        session.oauth_credentials = credentials;
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "oauth_credentials_snapshot_dropped",
            message = "oauth credentials snapshot dropped for an unknown session",
            outcome = "dropped",
            session_id = %session_id,
            reason = "unknown_session",
        );
        return;
    }
    tracing::info!(
        target: crate::logging::targets::APP_AUTH,
        event_name = "oauth_credentials_snapshot_applied",
        message = "oauth credentials snapshot applied",
        outcome = "success",
        session_id = %session_id,
        is_active,
        has_credentials,
        has_expiry,
    );
}

/// Apply a [`ClientEvent::GitContextSnapshotReceived`] event to the
/// session bucket addressed by `session_id`. Active-session
/// targeting goes through [`crate::app::App::apply_git_context_snapshot`]
/// (which can flip `needs_redraw`); background-session targeting
/// writes directly into the bucket without flipping redraw.
fn apply_git_context_snapshot_routed(
    app: &mut App,
    session_id: &str,
    context: forge_agent::env::git::GitContext,
) {
    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        app.apply_git_context_snapshot(context);
    } else if let Some(session) = app.session_mut(&session_key) {
        // Apply the snapshot directly to the bucket. We deliberately
        // don't flip `needs_redraw` here — the multiplexer already
        // skips redraw for background-session events.
        let _changed = session.git_context.apply_snapshot(context);
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "git_context_snapshot_dropped",
            message = "git context snapshot dropped for an unknown session",
            outcome = "dropped",
            session_id = %session_id,
            reason = "unknown_session",
        );
    }
}

/// Apply a [`ClientEvent::ContextUsageReceived`] event to the
/// session bucket addressed by `session_id`. Active-session
/// targeting goes through
/// [`crate::app::session_runtime::apply_context_usage_snapshot`] so
/// the in-flight refresh chaining still kicks in. Background-session
/// targeting writes directly into the bucket and skips the refresh
/// chain (a background session re-requests on next active switch).
fn apply_context_usage_snapshot_routed(app: &mut App, session_id: &str, percentage: Option<u8>) {
    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        crate::app::session_runtime::apply_context_usage_snapshot(app, percentage);
    } else if let Some(session) = app.session_mut(&session_key) {
        session.session_usage.context_usage_percent = percentage;
        session.session_usage.context_usage_in_flight = false;
        // Drop the refresh-pending flag too — once a fresh value
        // landed, queueing another refresh is wasteful for a
        // background bucket.
        session.session_usage.context_usage_refresh_pending = false;
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "context_usage_dropped",
            message = "context usage dropped for an unknown session",
            outcome = "dropped",
            session_id = %session_id,
            reason = "unknown_session",
        );
    }
}

/// Apply a [`ClientEvent::McpSnapshotReceived`] event to the session
/// bucket addressed by `session_id`. Active-session targeting also
/// reconciles the App-global MCP auth-redirect overlay and selection
/// index. Background-session targeting only writes the per-session
/// MCP state into the bucket — the overlay reconciliation is
/// inherently active-session UI.
fn apply_mcp_snapshot_routed(
    app: &mut App,
    session_id: &str,
    servers: Vec<forge_primitives::McpServerStatus>,
    error: Option<String>,
) {
    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    let server_count = servers.len();
    let error_present = error.is_some();
    if is_active {
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
            if let Some(server) = app.mcp().servers.iter().find(|server| server.name == server_name)
                && !matches!(
                    server.status,
                    forge_primitives::McpServerConnectionStatus::NeedsAuth
                        | forge_primitives::McpServerConnectionStatus::Pending
                )
            {
                if matches!(server.status, forge_primitives::McpServerConnectionStatus::Connected) {
                    app.config.status_message =
                        Some(format!("{} authenticated successfully.", server.name));
                    app.config.last_error = None;
                }
                app.config.overlay = None;
            }
        }
    } else if let Some(session) = app.session_mut(&session_key) {
        session.mcp.servers = servers;
        session.mcp.in_flight = false;
        session.mcp.last_error = error;
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "mcp_snapshot_dropped",
            message = "MCP snapshot dropped for an unknown session",
            outcome = "dropped",
            session_id = %session_id,
            reason = "unknown_session",
        );
        return;
    }
    tracing::info!(
        target: crate::logging::targets::APP_CONFIG,
        event_name = "mcp_snapshot_applied",
        message = "MCP snapshot applied",
        outcome = "success",
        session_id = %session_id,
        is_active,
        server_count,
        error_present,
    );
}

/// Apply a [`ClientEvent::SdkMessageReceived`] event to the session
/// bucket addressed by `session_id`. The SDK message dispatcher is
/// deeply intertwined with active-session UI accessors (chat buffer,
/// tool-call indices, viewport) so background-session SDK messages
/// are dropped in this Phase 2a slice. The drop is logged so a
/// future single-bridge multi-session expansion can detect missed
/// frames.
fn apply_sdk_message_routed(app: &mut App, session_id: &str, msg: forge_primitives::Message) {
    // For new sessions the CLI doesn't emit `system/init` until AFTER
    // the first user message lands (per `spawn_inner` docs), so
    // `Client::session_id()` is empty at spawn time and that empty
    // value rides through `Connected` onto `app.session_id`. The
    // first wire message that DOES carry a real id (Assistant /
    // User / Result / System(init)) is the canonical source —
    // adopt it onto the active bucket. For resume the bridge
    // already used the resume_id for Connected, so adoption is a
    // no-op and the strict mismatch check covers stale-Client
    // races during session swap.
    let active_session_id_string = app.session_id().map(ToString::to_string);
    let active_session_id_str = active_session_id_string.as_deref().unwrap_or("");
    if active_session_id_str.is_empty() && !session_id.is_empty() {
        // The active bucket exists but has no id yet — adopt the
        // canonical id so subsequent dispatch resolves correctly.
        app.set_session_id(Some(crate::agent::model::SessionId::new(session_id.to_owned())));
    } else if !active_session_id_str.is_empty() && active_session_id_str != session_id {
        // SDK message for a non-active session. The current sub-
        // handler dispatch in `super::sdk_message::handle_sdk_message`
        // assumes the active bucket; routing it would require either
        // switching active state or refactoring every sub-handler to
        // take a `&mut Session`. Drop with a breadcrumb instead and
        // keep the active bucket clean. Phase 2b's multi-bridge
        // delivery will revisit this.
        let session_key = SessionKey::from_session_id(session_id.to_owned());
        if app.session_mut(&session_key).is_none() {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "sdk_message_dropped",
                message = "SDK message dropped for an unknown session",
                outcome = "dropped",
                session_id = %session_id,
                reason = "unknown_session",
            );
        } else {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "sdk_message_dropped",
                message = "SDK message dropped for a non-active session",
                outcome = "dropped",
                session_id = %session_id,
                reason = "non_active_session",
            );
        }
        return;
    }
    super::sdk_message::handle_sdk_message(app, msg);
}

/// Apply a [`ClientEvent::HookObservation`] event to the session
/// bucket addressed by `session_id`. Active-session targeting goes
/// through the App accessors so the mode/effort chips update
/// promptly. Background-session targeting writes directly into the
/// bucket so its observed values stay current for a future switch.
fn apply_hook_observation_routed(
    app: &mut App,
    session_id: &str,
    tool_use_id: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    agent_id: Option<&str>,
    agent_type: Option<&str>,
) {
    use crate::agent::model::EffortLevel;
    use crate::agent::state::PermissionMode;

    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);

    let parsed_permission_mode = permission_mode.and_then(PermissionMode::from_wire);
    let parsed_effort = effort.and_then(|effort_str| match effort_str {
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
    });

    if is_active {
        if let Some(mode) = parsed_permission_mode {
            app.set_observed_permission_mode(Some(mode));
        }
        if let Some(level) = parsed_effort {
            app.set_observed_effort(Some(level));
        }
        if let (Some(tool_use_id), Some(_agent_id), Some(agent_type)) =
            (tool_use_id, agent_id, agent_type)
        {
            app.subagent_attribution_mut().insert(tool_use_id.to_owned(), agent_type.to_owned());
        }
    } else if let Some(session) = app.session_mut(&session_key) {
        if let Some(mode) = parsed_permission_mode {
            session.observed_permission_mode = Some(mode);
        }
        if let Some(level) = parsed_effort {
            session.observed_effort = Some(level);
        }
        if let (Some(tool_use_id), Some(_agent_id), Some(agent_type)) =
            (tool_use_id, agent_id, agent_type)
        {
            session.subagent_attribution.insert(tool_use_id.to_owned(), agent_type.to_owned());
        }
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "hook_observation_dropped",
            message = "hook observation dropped for an unknown session",
            outcome = "dropped",
            session_id = %session_id,
            reason = "unknown_session",
        );
    }
}

/// Apply a [`ClientEvent::RuntimeReloadCompleted`] event for the
/// session bucket addressed by `session_id`. The plugins config tab
/// is App-global UI scoped to the active session — a background
/// reload that completes silently is a no-op on the UI but logged
/// so the operator can confirm the bridge dispatched it. Unknown-
/// session events log a warn-level breadcrumb.
fn apply_runtime_reload_completed(app: &mut App, session_id: &str) {
    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        crate::app::plugins::apply_runtime_reload_success(app);
    } else if app.sessions.contains_key(&session_key) {
        tracing::debug!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "runtime_reload_completed_background",
            message = "runtime reload completed for a background session; UI unaffected",
            outcome = "info",
            session_id = %session_id,
        );
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "runtime_reload_completed_dropped",
            message = "runtime reload completion dropped for an unknown session",
            outcome = "dropped",
            session_id = %session_id,
            reason = "unknown_session",
        );
    }
}

/// Apply a [`ClientEvent::RuntimeReloadFailed`] event for the
/// session bucket addressed by `session_id`. Same routing shape as
/// [`apply_runtime_reload_completed`].
fn apply_runtime_reload_failed(app: &mut App, session_id: &str, message: &str) {
    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let is_active = app.active_session_key.as_ref() == Some(&session_key);
    if is_active {
        crate::app::plugins::apply_runtime_reload_failure(app, message);
    } else if app.sessions.contains_key(&session_key) {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "runtime_reload_failed_background",
            message = "runtime reload failed for a background session; UI unaffected",
            outcome = "degraded",
            session_id = %session_id,
            error_message = %message,
        );
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "runtime_reload_failed_dropped",
            message = "runtime reload failure dropped for an unknown session",
            outcome = "dropped",
            session_id = %session_id,
            reason = "unknown_session",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::Session;

    fn seed_two_sessions(app: &mut App) -> (SessionKey, SessionKey) {
        let key_a = SessionKey::from_str_for_test("session-a");
        let key_b = SessionKey::from_str_for_test("session-b");
        let mut session_a = Session::new(key_a.clone());
        session_a.session_id = Some(crate::agent::model::SessionId::new("session-a"));
        let mut session_b = Session::new(key_b.clone());
        session_b.session_id = Some(crate::agent::model::SessionId::new("session-b"));
        app.sessions.insert(key_a.clone(), session_a);
        app.sessions.insert(key_b.clone(), session_b);
        app.active_session_key = Some(key_a.clone());
        app.needs_redraw = false;
        (key_a, key_b)
    }

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
        let (key_a, key_b) = seed_two_sessions(&mut app);
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

    #[test]
    fn status_snapshot_routes_to_active_session_and_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        let account = forge_primitives::AccountInfo {
            email: Some("a@example.com".to_owned()),
            ..Default::default()
        };
        handle_client_event(
            &mut app,
            ClientEvent::StatusSnapshotReceived {
                session_id: key_a.as_str().to_owned(),
                account,
                forge_account: None,
            },
        );
        assert_eq!(
            app.sessions
                .get(&key_a)
                .expect("a")
                .account_info
                .as_ref()
                .and_then(|a| a.email.as_deref()),
            Some("a@example.com"),
        );
        assert!(app.sessions.get(&key_b).expect("b").account_info.is_none());
        assert!(app.needs_redraw);
    }

    /// Single-session focused twin of
    /// [`background_event_updates_target_session_only`]: with only one
    /// real session in the map, an event tagged for the active key
    /// must still flip `needs_redraw`. Guards the routing rule
    /// (active-target events trigger redraw) without the multi-session
    /// noise of [`status_snapshot_routes_to_active_session_and_flips_redraw`].
    #[test]
    fn active_session_event_flips_needs_redraw() {
        let mut app = App::test_default();
        let key_a = SessionKey::from_str_for_test("a");
        let mut session_a = Session::new(key_a.clone());
        session_a.session_id = Some(crate::agent::model::SessionId::new("a"));
        app.sessions.insert(key_a.clone(), session_a);
        app.active_session_key = Some(key_a.clone());
        app.needs_redraw = false;

        handle_client_event(
            &mut app,
            ClientEvent::StatusSnapshotReceived {
                session_id: "a".to_owned(),
                account: forge_primitives::AccountInfo::default(),
                forge_account: None,
            },
        );

        assert!(app.needs_redraw, "active-session events must flip needs_redraw");
    }

    fn make_creds() -> forge_agent::cloud::oauth_credentials::OauthCredentials {
        // Round-trip through serde_json so the non-exhaustive
        // constructor doesn't trip in tests. The test crate doesn't
        // own forge_agent's internals so this is the lightest path.
        let json = serde_json::json!({
            "access_token": "tok",
            "expires_at": null
        });
        serde_json::from_value(json).expect("OauthCredentials JSON-deserialise")
    }

    #[test]
    fn oauth_credentials_snapshot_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        handle_client_event(
            &mut app,
            ClientEvent::OauthCredentialsSnapshotReceived {
                session_id: key_b.as_str().to_owned(),
                credentials: Some(make_creds()),
            },
        );
        assert!(app.sessions.get(&key_b).expect("b").oauth_credentials.is_some());
        assert!(app.sessions.get(&key_a).expect("a").oauth_credentials.is_none());
        assert!(!app.needs_redraw);
    }

    #[test]
    fn oauth_credentials_snapshot_for_active_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        handle_client_event(
            &mut app,
            ClientEvent::OauthCredentialsSnapshotReceived {
                session_id: key_a.as_str().to_owned(),
                credentials: Some(make_creds()),
            },
        );
        assert!(app.sessions.get(&key_a).expect("a").oauth_credentials.is_some());
        assert!(app.needs_redraw);
    }

    #[test]
    fn context_usage_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        handle_client_event(
            &mut app,
            ClientEvent::ContextUsageReceived {
                session_id: key_b.as_str().to_owned(),
                percentage: Some(42),
            },
        );
        assert_eq!(
            app.sessions.get(&key_b).expect("b").session_usage.context_usage_percent,
            Some(42),
        );
        assert!(app.sessions.get(&key_a).expect("a").session_usage.context_usage_percent.is_none());
        assert!(!app.needs_redraw);
    }

    #[test]
    fn context_usage_routes_to_active_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        handle_client_event(
            &mut app,
            ClientEvent::ContextUsageReceived {
                session_id: key_a.as_str().to_owned(),
                percentage: Some(7),
            },
        );
        assert_eq!(
            app.sessions.get(&key_a).expect("a").session_usage.context_usage_percent,
            Some(7),
        );
        assert!(app.needs_redraw);
    }

    #[test]
    fn mcp_snapshot_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        let servers = vec![forge_primitives::McpServerStatus {
            name: "test-mcp".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        }];
        handle_client_event(
            &mut app,
            ClientEvent::McpSnapshotReceived {
                session_id: key_b.as_str().to_owned(),
                servers,
                error: None,
            },
        );
        assert_eq!(app.sessions.get(&key_b).expect("b").mcp.servers.len(), 1);
        assert!(app.sessions.get(&key_a).expect("a").mcp.servers.is_empty());
        assert!(!app.needs_redraw);
    }

    #[test]
    fn mcp_snapshot_routes_to_active_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        let servers = vec![forge_primitives::McpServerStatus {
            name: "test-mcp".into(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: None,
            scope: None,
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        }];
        handle_client_event(
            &mut app,
            ClientEvent::McpSnapshotReceived {
                session_id: key_a.as_str().to_owned(),
                servers,
                error: None,
            },
        );
        assert_eq!(app.sessions.get(&key_a).expect("a").mcp.servers.len(), 1);
        assert!(app.needs_redraw);
    }

    #[test]
    fn hook_observation_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        handle_client_event(
            &mut app,
            ClientEvent::HookObservation {
                session_id: key_b.as_str().to_owned(),
                tool_use_id: Some("tool-1".into()),
                permission_mode: Some("acceptEdits".into()),
                effort: Some("max".into()),
                agent_id: Some("agent-1".into()),
                agent_type: Some("general-purpose".into()),
            },
        );
        let b = app.sessions.get(&key_b).expect("b");
        assert!(b.observed_permission_mode.is_some());
        assert!(b.observed_effort.is_some());
        assert_eq!(
            b.subagent_attribution.get("tool-1").map(String::as_str),
            Some("general-purpose")
        );
        let a = app.sessions.get(&key_a).expect("a");
        assert!(a.observed_permission_mode.is_none());
        assert!(a.observed_effort.is_none());
        assert!(a.subagent_attribution.is_empty());
        assert!(!app.needs_redraw);
    }

    #[test]
    fn hook_observation_routes_to_active_flips_redraw() {
        let mut app = App::test_default();
        let (key_a, _key_b) = seed_two_sessions(&mut app);
        handle_client_event(
            &mut app,
            ClientEvent::HookObservation {
                session_id: key_a.as_str().to_owned(),
                tool_use_id: None,
                permission_mode: Some("plan".into()),
                effort: None,
                agent_id: None,
                agent_type: None,
            },
        );
        assert!(app.sessions.get(&key_a).expect("a").observed_permission_mode.is_some());
        assert!(app.needs_redraw);
    }

    #[test]
    fn unknown_session_event_drops_cleanly() {
        let mut app = App::test_default();
        let (_key_a, _key_b) = seed_two_sessions(&mut app);
        let unknown = SessionKey::from_str_for_test("nope");
        let account = forge_primitives::AccountInfo {
            email: Some("ghost@example.com".to_owned()),
            ..Default::default()
        };
        handle_client_event(
            &mut app,
            ClientEvent::StatusSnapshotReceived {
                session_id: unknown.as_str().to_owned(),
                account,
                forge_account: None,
            },
        );
        // Sessions A and B both unaffected; redraw flag stays false;
        // the unknown key must NOT have been silently inserted.
        assert!(!app.needs_redraw);
        assert!(
            !app.sessions.contains_key(&unknown),
            "unknown session key must not be inserted by the routing path"
        );
    }
}
