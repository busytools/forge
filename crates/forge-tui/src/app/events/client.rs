use super::{App, session, turn};
use forge_workspace::{SessionKey, SessionUpdate};

/// Side-effects shared by `Connected` and `SessionReplaced`: refresh
/// MCP + status + oauth-credentials + context-usage snapshots, and
/// kick a usage poll so the Projects pane's 5h/7d bars land within
/// seconds of session start instead of staying on placeholder `—%`.
fn post_connect_refreshes(app: &mut App) {
    crate::app::config::refresh_mcp_snapshot(app);
    crate::app::session_runtime::request_status_snapshot_refresh(app);
    crate::app::session_runtime::request_oauth_credentials_snapshot_refresh(app);
    crate::app::session_runtime::request_context_usage_refresh(app);
    crate::app::usage::request_refresh_if_needed(app);
}

/// Apply `f` only when `cwd_raw` matches the app's current cwd; log
/// and drop the event otherwise. Plugin lifecycle events are
/// cwd-scoped and stale ones from a previous project must not affect
/// the active project's inventory.
fn dispatch_if_cwd_matches(app: &mut App, cwd_raw: &str, event_name: &str, f: impl FnOnce(&mut App)) {
    if app.cwd_raw() == cwd_raw {
        f(app);
    } else {
        tracing::debug!(
            target: crate::logging::targets::APP_CONFIG,
            event_name,
            expected_cwd = %app.cwd_raw(),
            received_cwd = %cwd_raw,
            "stale-cwd plugin event dropped"
        );
    }
}

/// Compact discriminant name for a wire `Message`. Used by the
/// `sdk_message_dropped` error log so a triage grep can see whether
/// the dropped envelope was a Result (TurnComplete carrier),
/// Assistant content, etc. — without dumping the full payload.
fn msg_variant_name(msg: &forge_primitives::Message) -> &'static str {
    match msg {
        forge_primitives::Message::Assistant { .. } => "Assistant",
        forge_primitives::Message::User { .. } => "User",
        forge_primitives::Message::System { .. } => "System",
        forge_primitives::Message::Result { .. } => "Result",
        forge_primitives::Message::TaskStarted { .. } => "TaskStarted",
        forge_primitives::Message::TaskUpdated { .. } => "TaskUpdated",
        forge_primitives::Message::TaskProgress { .. } => "TaskProgress",
        forge_primitives::Message::TaskNotification { .. } => "TaskNotification",
        forge_primitives::Message::RateLimitEvent { .. } => "RateLimitEvent",
        forge_primitives::Message::StreamEvent { .. } => "StreamEvent",
        forge_primitives::Message::Error { .. } => "Error",
        forge_primitives::Message::Unknown { .. } => "Unknown",
    }
}

/// Per-session event multiplexer. Each [`SessionUpdate`] is routed
/// to the [`crate::app::session::UiSession`] bucket it targets via the
/// envelope's [`SessionUpdate::session_key`] accessor.
///
/// `needs_redraw` is flipped only when the routed event targets the
/// active session — background-session events update their bucket
/// silently. App-global events (no `session_key`) flip the redraw
/// flag unconditionally because they affect the rendered view.
#[allow(clippy::if_not_else)]
pub fn apply_session_update(app: &mut App, update: SessionUpdate) {
    // INVARIANT: `is_active_or_global` is captured BEFORE the match
    // so reducers that themselves mutate `active_session_key` (e.g.
    // `Connected`, `SessionReplaced`) must set `needs_redraw = true`
    // explicitly via their own side effects. The post-match flip
    // sees the pre-handler active_session_key.
    let target_key = update.session_key();
    let is_active_or_global = match &target_key {
        Some(key) => app.active_session_key.as_ref() == Some(key),
        None => true,
    };
    // Stamp `last_activity_at` for the session this event targets so
    // the Projects pane's relative-time column has a ground truth.
    if let Some(key) = target_key.as_ref()
        && let Some(session) = app.session_mut(key)
    {
        session.last_activity_at = std::time::Instant::now();
    }
    match update {
        SessionUpdate::Spawning { key, project_name, cwd, display_name } => {
            apply_session_update_spawning(app, key, &project_name, &cwd, &display_name);
        }
        SessionUpdate::KeyRenamed { from, to } => {
            apply_session_update_key_renamed(app, &from, to);
        }
        SessionUpdate::Connected {
            key,
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history,
        } => {
            session::apply_session_update_connected(
                app,
                key,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                history,
            );
            post_connect_refreshes(app);
        }
        SessionUpdate::SessionReplaced {
            key,
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history,
        } => {
            session::apply_session_update_session_replaced(
                app,
                key,
                session_id,
                cwd,
                current_model,
                available_models,
                mode,
                history,
            );
            post_connect_refreshes(app);
        }
        SessionUpdate::SessionsListed { key, sessions } => {
            session::apply_session_update_sessions_listed(app, &key, sessions);
        }
        SessionUpdate::AuthRequired { key, method_name, method_description } => {
            session::apply_session_update_auth_required(app, key, method_name, method_description);
        }
        SessionUpdate::ConnectionFailed { key, message, fatal } => {
            session::apply_session_update_connection_failed(app, key, message, fatal);
        }
        SessionUpdate::SlashCommandError { key, message } => {
            session::apply_session_update_slash_command_error(app, key, message);
        }
        SessionUpdate::AuthCompleted { key } => {
            session::apply_session_update_auth_completed(app, key);
        }
        SessionUpdate::LogoutCompleted { key } => {
            session::apply_session_update_logout_completed(app, key);
        }
        SessionUpdate::ServiceStatus { severity, message } => {
            session::apply_session_update_service_status(app, severity, message);
        }
        SessionUpdate::FatalError(error) => {
            session::apply_session_update_fatal_error(app, error);
        }
        SessionUpdate::ForgeAccountIdentity { key, display_name } => {
            apply_session_update_forge_account_identity(app, key, display_name);
        }
        SessionUpdate::StatusSnapshot { session_id, account, forge_account } => {
            apply_session_update_status_snapshot(app, session_id, account, forge_account);
        }
        SessionUpdate::OauthCredentialsSnapshot { session_id, credentials } => {
            apply_session_update_oauth_credentials_snapshot(app, session_id, credentials);
        }
        SessionUpdate::ContextUsageSnapshot { session_id, percentage } => {
            apply_session_update_context_usage_snapshot(app, session_id, percentage);
        }
        SessionUpdate::McpSnapshot { session_id, servers, error } => {
            apply_session_update_mcp_snapshot(app, session_id, servers, error);
        }
        SessionUpdate::ChatAppended { session_id, msg } => {
            apply_session_update_chat_appended(app, session_id, msg);
        }
        SessionUpdate::HookObservation {
            session_id,
            tool_use_id,
            permission_mode,
            effort,
            agent_id,
            agent_type,
        } => {
            apply_session_update_hook_observation(
                app,
                session_id,
                tool_use_id,
                permission_mode,
                effort,
                agent_id,
                agent_type,
            );
        }
        SessionUpdate::RuntimeReloadCompleted { session_id } => {
            apply_session_update_runtime_reload_completed(app, session_id);
        }
        SessionUpdate::RuntimeReloadFailed { session_id, message } => {
            apply_session_update_runtime_reload_failed(app, session_id, message);
        }
        SessionUpdate::PermissionRequest { key, tool_id, request } => {
            turn::apply_session_update_permission_request(app, key, tool_id, request);
        }
        SessionUpdate::QuestionRequest { key, tool_id, request } => {
            turn::apply_session_update_question_request(app, key, tool_id, request);
        }
        SessionUpdate::McpElicitationRequest { key, elicitation_id, request } => {
            turn::apply_session_update_mcp_elicitation_request(app, key, elicitation_id, request);
        }
        SessionUpdate::McpElicitationCompleted { elicitation_id, server_name, .. } => {
            if is_active_or_global {
                crate::app::config::handle_mcp_elicitation_completed(
                    app,
                    &elicitation_id,
                    server_name,
                );
            }
        }
        SessionUpdate::McpAuthRedirect { redirect, .. } => {
            if is_active_or_global {
                crate::app::config::present_mcp_auth_redirect(app, redirect);
            }
        }
        SessionUpdate::McpOperationError { error, .. } => {
            if is_active_or_global {
                crate::app::config::handle_mcp_operation_error(app, &error);
            }
        }
        SessionUpdate::TurnComplete { key, terminal_reason } => {
            turn::apply_session_update_turn_complete(app, key, terminal_reason);
        }
        SessionUpdate::TurnCancelled { key } => {
            turn::apply_session_update_turn_cancelled(app, &key);
        }
        SessionUpdate::TurnError { key, message, class, terminal_reason } => {
            turn::apply_session_update_turn_error(app, key, message, class, terminal_reason);
        }
        SessionUpdate::UsageRefreshStarted { key } => {
            crate::app::usage::apply_refresh_started_for(app, &key);
        }
        SessionUpdate::UsageSnapshotReceived { key, snapshot } => {
            crate::app::usage::apply_refresh_success_for(app, &key, snapshot);
        }
        SessionUpdate::UsageRefreshFailed { key, message, source } => {
            crate::app::usage::apply_refresh_failure_for(app, &key, message, source);
        }
        SessionUpdate::PluginsInventoryUpdated { cwd_raw, snapshot, claude_path } => {
            dispatch_if_cwd_matches(app, &cwd_raw, "plugins_inventory_dropped", |app| {
                crate::app::plugins::apply_inventory_refresh_success(app, snapshot, claude_path);
            });
        }
        SessionUpdate::PluginsInventoryRefreshFailed { cwd_raw, message } => {
            dispatch_if_cwd_matches(app, &cwd_raw, "plugins_inventory_failure_dropped", |app| {
                crate::app::plugins::apply_inventory_refresh_failure(app, message);
            });
        }
        SessionUpdate::PluginsCliActionSucceeded { cwd_raw, result } => {
            dispatch_if_cwd_matches(app, &cwd_raw, "plugins_cli_success_dropped", |app| {
                crate::app::plugins::apply_cli_action_success(app, result);
            });
        }
        SessionUpdate::PluginsCliActionFailed { cwd_raw, message } => {
            dispatch_if_cwd_matches(app, &cwd_raw, "plugins_cli_failure_dropped", |app| {
                crate::app::plugins::apply_cli_action_failure(app, message);
            });
        }
    }
    if is_active_or_global {
        app.needs_redraw = true;
    }
}

/// `SessionUpdate::Spawning` reducer. Synthesize a placeholder
/// bucket under `key` with a "Waking {display_name}…" message, set
/// `cwd_raw`/`cwd` from the project's path, and (conditionally)
/// switch active focus.
///
/// **Focus rule:** auto-focus the new spawn ONLY when there's no
/// real focused session yet — i.e. `active_session_key` is `None`
/// or still pointing at the pre-Connect placeholder. Once a real
/// session is focused (the StartDefault target after `forge.toml`'s
/// `focus = true` project's Connected fires), subsequent
/// auto_start projects' `Spawning` events must NOT steal focus.
/// This is what made multi-project auto_start drift to whichever
/// project spawned last before the fix.
///
/// Existing buckets (user clicked a stale row to re-wake) still
/// switch focus — that's an explicit user action, not a passive
/// background spawn.
fn apply_session_update_spawning(
    app: &mut App,
    key: SessionKey,
    _project_name: &str,
    cwd: &str,
    display_name: &str,
) {
    if app.sessions.contains_key(&key) {
        // Existing bucket → user-triggered re-wake. Switch focus.
        app.switch_active_session(key);
        return;
    }
    let mut bucket = crate::app::session::UiSession::new(key.clone());
    bucket.cwd = shorten_cwd_display_path(cwd);
    cwd.clone_into(&mut bucket.cwd_raw);
    bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Spawning;
    bucket.messages.push(crate::app::ChatMessage::new(
        crate::app::MessageRole::System(Some(crate::app::SystemSeverity::Info)),
        vec![crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(&format!(
            "Waking {display_name}…"
        )))],
        None,
    ));
    bucket.message_retained_bytes.push(0);
    app.sessions.insert(key.clone(), bucket);

    // **Focus stays where it is.** Auto-focus over the pre-Connect
    // placeholder would let whichever auto_start project's Spawning
    // event arrives first steal the focused tab — but pre-Connect
    // is reserved for the `focus = true` project's StartDefault
    // migration (which doesn't go through this reducer at all; it
    // uses KeyRenamed to swap the pre-Connect bucket onto the real
    // key in-place). So a Spawning event for a non-focused
    // auto_start project must just register the bucket and trigger
    // a redraw — never move focus.
    //
    // The only case we'd switch focus from here is `active_session_key
    // == None`, which doesn't happen in practice (the App constructs
    // pre-Connect at startup). Kept defensively so a future flow that
    // skips the pre-Connect bucket still focuses its first spawn.
    if app.active_session_key.is_none() {
        app.switch_active_session(key);
    } else {
        app.needs_redraw = true;
    }
}

fn shorten_cwd_display_path(cwd: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy().to_string();
        if cwd.starts_with(&home_str) {
            return format!("~{}", &cwd[home_str.len()..]);
        }
    }
    cwd.to_owned()
}

/// Phase 3b — `SessionUpdate::ForgeAccountIdentity` reducer for the
/// session bucket addressed by `key`. Active-session targeting goes
/// through the existing
/// [`crate::app::App::set_active_account_display_name`] accessor +
/// [`crate::app::App::sync_welcome_snapshot`] so welcome rendering
/// updates promptly. Background-session targeting writes the
/// display name directly into the bucket without touching the
/// active-session welcome snapshot.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_forge_account_identity(
    app: &mut App,
    key: SessionKey,
    display_name: String,
) {
    apply_forge_account_identity_presentation(app, &key, display_name);
}

fn apply_forge_account_identity_presentation(
    app: &mut App,
    session_key: &SessionKey,
    display_name: String,
) {
    if app.active_session_key.as_ref() == Some(session_key) {
        app.set_active_account_display_name(Some(display_name));
        app.sync_welcome_snapshot();
        return;
    }
    // Background-session path: write directly to the bucket.
    if let Some(bucket) = app.sessions.get_mut(session_key) {
        bucket.active_account_display_name = Some(display_name);
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_AUTH,
            event_name = "forge_account_identity_dropped",
            message = "forge-account identity dropped for an unknown session",
            outcome = "dropped",
            session_key = %session_key.as_str(),
            reason = "unknown_session",
        );
    }
}

/// Phase 3b — `SessionUpdate::StatusSnapshot` reducer for the
/// session bucket addressed by `session_id`. Routes through the
/// active-session accessors when targeting the rendered session
/// (so welcome + Status panel rerender promptly); writes directly
/// into the bucket otherwise so background sessions accumulate
/// state silently.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_status_snapshot(
    app: &mut App,
    session_id: String,
    account: forge_primitives::AccountInfo,
    forge_account: Option<forge_primitives::ForgeAccountIdentity>,
) {
    apply_status_snapshot_presentation(app, &session_id, account, forge_account);
}

fn apply_status_snapshot_presentation(
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
    } else if let Some(bucket) = app.sessions.get_mut(&session_key) {
        // Background-session path: write directly to the bucket.
        bucket.account_info = Some(account);
        bucket.active_account_display_name = forge_account.map(|f| f.display_name);
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

/// Phase 3b — `SessionUpdate::OauthCredentialsSnapshot` reducer for
/// the session bucket addressed by `session_id`. Active-session
/// targeting goes through [`crate::app::App::set_oauth_credentials`];
/// background-session targeting writes directly into the bucket.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_oauth_credentials_snapshot(
    app: &mut App,
    session_id: String,
    credentials: Option<forge_primitives::cloud::oauth_credentials::OauthCredentials>,
) {
    apply_oauth_credentials_snapshot_presentation(app, &session_id, credentials);
}

fn apply_oauth_credentials_snapshot_presentation(
    app: &mut App,
    session_id: &str,
    credentials: Option<forge_primitives::cloud::oauth_credentials::OauthCredentials>,
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

/// Phase 3b — `SessionUpdate::ContextUsageSnapshot` reducer for the
/// session bucket addressed by `session_id`. Active-session
/// targeting goes through
/// [`crate::app::session_runtime::apply_context_usage_snapshot`] so
/// the in-flight refresh chaining still kicks in. Background-session
/// targeting writes directly into the bucket and skips the refresh
/// chain (a background session re-requests on next active switch).
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_context_usage_snapshot(
    app: &mut App,
    session_id: String,
    percentage: Option<u8>,
) {
    apply_context_usage_snapshot_presentation(app, &session_id, percentage);
}

fn apply_context_usage_snapshot_presentation(
    app: &mut App,
    session_id: &str,
    percentage: Option<u8>,
) {
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

/// Phase 3b — `SessionUpdate::McpSnapshot` reducer for the session
/// bucket addressed by `session_id`. Active-session targeting also
/// reconciles the App-global MCP auth-redirect overlay and selection
/// index. Background-session targeting only writes the per-session
/// MCP state into the bucket — the overlay reconciliation is
/// inherently active-session UI.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_mcp_snapshot(
    app: &mut App,
    session_id: String,
    servers: Vec<forge_primitives::McpServerStatus>,
    error: Option<String>,
) {
    apply_mcp_snapshot_presentation(app, &session_id, servers, error);
}

fn apply_mcp_snapshot_presentation(
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

/// Phase 3b — `SessionUpdate::ChatAppended` reducer for the session
/// bucket addressed by `session_id`. The SDK message dispatcher is
/// deeply intertwined with active-session UI accessors (chat buffer,
/// tool-call indices, viewport) so the active-session temp-swap
/// inside [`apply_sdk_message_presentation`] is the Phase 4 cleanup
/// target, not this phase's.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_chat_appended(
    app: &mut App,
    session_id: String,
    msg: forge_primitives::Message,
) {
    apply_sdk_message_presentation(app, &session_id, msg);
}

fn apply_sdk_message_presentation(app: &mut App, session_id: &str, msg: forge_primitives::Message) {
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
    let active_session_id_string = app.session_id().map(|s| s.to_string());
    let active_session_id_str = active_session_id_string.as_deref().unwrap_or("");
    if active_session_id_str.is_empty() && !session_id.is_empty() {
        // The active bucket exists but has no id yet — adopt the
        // canonical id so subsequent dispatch resolves correctly.
        app.set_session_id(Some(crate::agent::model::SessionId::new(session_id.to_owned())));
    } else if !active_session_id_str.is_empty() && active_session_id_str != session_id {
        // SDK message for a non-active session. The handlers in
        // `super::sdk_message::handle_sdk_message` reach for the
        // active bucket via the App-level accessors (chat buffer,
        // tool-call indices, viewport, …). Temporarily promote the
        // target bucket to active so those accessors land on the
        // right session, then dispatch and restore. The active
        // session's `App.input` and `App.status` are snapshotted +
        // restored across the swap so background routing doesn't
        // touch user-visible UI for the session the user is actually
        // looking at. Without this routing, background turns produce
        // events that update lifecycle state (via routed handlers in
        // `events/turn.rs`) but never land their `Message::Assistant`
        // payloads in the bucket — the user switches back to a
        // bucket whose pane glyph says Attention but whose chat
        // buffer still only shows what was on screen at switch-out.
        let session_key = SessionKey::from_session_id(session_id.to_owned());
        if app.session_mut(&session_key).is_none() {
            // Promoted to `error` so always-on debug logs make this
            // very visible. The wire `session_id` doesn't match any
            // known UiSession bucket — typically a key-drift race
            // (in-flight wire frame whose session_id was rekey'd /
            // dropped between the SessionTask emit and this reducer).
            // The dumped context lets us see exactly which key the
            // wire used vs what the TUI was tracking. If the
            // dropped msg is `Result`, TurnComplete never fires and
            // the spinner stays on "Thinking..." forever.
            let bucket_keys: Vec<String> =
                app.sessions.keys().map(|k| k.as_str().to_owned()).collect();
            tracing::error!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "sdk_message_dropped",
                message = "SDK message dropped for an unknown session",
                outcome = "dropped",
                wire_session_id = %session_id,
                active_session_id = %active_session_id_str,
                active_session_key = ?app.active_session_key.as_ref().map(|k| k.as_str().to_owned()),
                msg_variant = msg_variant_name(&msg),
                bucket_keys = ?bucket_keys,
                reason = "unknown_session",
            );
            return;
        }
        // Background SDK message routing: dispatch against the
        // target session's bucket without disturbing the active
        // session's user-visible UI state (`App.input`, `App.status`,
        // `App.active_session_key`).
        // `active_bucket_scope::with_pivoted` snapshots the visible
        // UI state, pivots `active_session_key`, runs the body, and
        // restores the snapshot.
        crate::app::active_bucket_scope::with_pivoted(app, session_key, |app| {
            super::sdk_message::handle_sdk_message(app, msg);
        });
        app.needs_redraw = true;
        return;
    }
    super::sdk_message::handle_sdk_message(app, msg);
}

/// Phase 3b — `SessionUpdate::HookObservation` reducer for the
/// session bucket addressed by `session_id`. Active-session
/// targeting goes through the App accessors so the mode/effort
/// chips update promptly. Background-session targeting writes
/// directly into the bucket so its observed values stay current
/// for a future switch. Wraps the `String` fields from the
/// workspace payload into `&str` borrows before delegating to the
/// shared presentation helper.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_hook_observation(
    app: &mut App,
    session_id: String,
    tool_use_id: Option<String>,
    permission_mode: Option<String>,
    effort: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
) {
    apply_hook_observation_presentation(
        app,
        &session_id,
        tool_use_id.as_deref(),
        permission_mode.as_deref(),
        effort.as_deref(),
        agent_id.as_deref(),
        agent_type.as_deref(),
    );
}

fn apply_hook_observation_presentation(
    app: &mut App,
    session_id: &str,
    tool_use_id: Option<&str>,
    permission_mode: Option<&str>,
    effort: Option<&str>,
    agent_id: Option<&str>,
    agent_type: Option<&str>,
) {
    use crate::agent::model::EffortLevel;
    use forge_workspace::PermissionMode;

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

/// Phase 3b — `SessionUpdate::RuntimeReloadCompleted` reducer for the
/// session bucket addressed by `session_id`. The plugins config tab
/// is App-global UI scoped to the active session — a background
/// reload that completes silently is a no-op on the UI but logged
/// so the operator can confirm the bridge dispatched it. Unknown-
/// session events log a warn-level breadcrumb.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_runtime_reload_completed(app: &mut App, session_id: String) {
    apply_runtime_reload_completed_presentation(app, &session_id);
}

fn apply_runtime_reload_completed_presentation(app: &mut App, session_id: &str) {
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

/// Phase 3b — `SessionUpdate::RuntimeReloadFailed` reducer for the
/// session bucket addressed by `session_id`. Same routing shape as
/// [`apply_session_update_runtime_reload_completed`].
#[allow(clippy::needless_pass_by_value)]
pub(super) fn apply_session_update_runtime_reload_failed(
    app: &mut App,
    session_id: String,
    message: String,
) {
    apply_runtime_reload_failed_presentation(app, &session_id, &message);
}

fn apply_runtime_reload_failed_presentation(app: &mut App, session_id: &str, message: &str) {
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

/// Phase 3a — migrate the bucket at `from` over to `to` when the
/// workspace renames a synthetic spawn key onto the real claude
/// session UUID. Updates `active_session_key` only when it
/// currently points at `from` (background-spawn case must NOT
/// hijack the user's deliberate session pick).
fn apply_session_update_key_renamed(app: &mut App, from: &SessionKey, to: SessionKey) {
    let already_under_to = app.sessions.contains_key(&to);
    if let Some(mut bucket) = app.sessions.remove(from) {
        if already_under_to {
            // `to` already exists (e.g. a Connected for the same
            // session UUID raced ahead and seeded the bucket); the
            // synthetic at `from` is now redundant. Drop it to match
            // the legacy `handle_connected_client_event` migration
            // semantics.
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "key_renamed_synthetic_dropped",
                message = "synthetic bucket dropped because real-key bucket already existed",
                outcome = "dropped",
                from = %from.as_str(),
                to = %to.as_str(),
                reason = "real_bucket_present",
            );
            let _ = bucket;
        } else {
            bucket.key = Some(to.clone());
            app.sessions.insert(to.clone(), bucket);
            super::set_bucket_lifecycle_state(
                app,
                &to,
                crate::app::session::SessionLifecycleState::Idle,
            );
        }
    } else if !already_under_to {
        // Neither `from` nor `to` exists. Seed a fresh idle bucket
        // at `to` so the subsequent Connected reducer can populate it.
        app.sessions.insert(to.clone(), crate::app::session::UiSession::new(to.clone()));
        super::set_bucket_lifecycle_state(
            app,
            &to,
            crate::app::session::SessionLifecycleState::Idle,
        );
    }
    if app.active_session_key.as_ref() == Some(from) {
        app.active_session_key = Some(to);
    }
    app.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::UiSession;

    fn seed_two_sessions(app: &mut App) -> (SessionKey, SessionKey) {
        let key_a = SessionKey::from_str_for_test("session-a");
        let key_b = SessionKey::from_str_for_test("session-b");
        let mut bucket_a = UiSession::new(key_a.clone());
        bucket_a.session_id = Some(forge_primitives::SessionId::new(key_a.as_str()));
        let mut bucket_b = UiSession::new(key_b.clone());
        bucket_b.session_id = Some(forge_primitives::SessionId::new(key_b.as_str()));
        app.sessions.insert(key_a.clone(), bucket_a);
        app.sessions.insert(key_b.clone(), bucket_b);
        // Register a DomainSession for each so AgentHandle dispatch
        // (which still needs an internal session_id mirror on the
        // workspace side) can route through.
        if let Some(ws) = app.workspace.as_ref() {
            for k in [&key_a, &key_b] {
                let (h, _) = forge_workspace::Workspace::testing_stub_handle();
                let dom = ws.register_domain_session(k.clone(), Some(std::sync::Arc::new(h)));
                dom.lock().session_id = Some(forge_primitives::SessionId::new(k.as_str()));
            }
        }
        app.active_session_key = Some(key_a.clone());
        app.needs_redraw = false;
        (key_a, key_b)
    }

    /// Read the `account_info` field on the bucket for `key`.
    fn bucket_account_info_for(
        app: &App,
        key: &SessionKey,
    ) -> Option<forge_primitives::AccountInfo> {
        app.sessions.get(key).and_then(|s| s.account_info.clone())
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
        assert!(bucket_account_info_for(&app, &key_a).is_none());
        assert!(bucket_account_info_for(&app, &key_b).is_none());

        // Fire a state-change event tagged for B.
        let account = forge_primitives::AccountInfo {
            email: Some("b@example.com".to_owned()),
            ..Default::default()
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                session_id: key_b.as_str().to_owned(),
                account,
                forge_account: None,
            },
        );

        // B's domain reflects the change.
        let b_info = bucket_account_info_for(&app, &key_b).expect("b account_info");
        assert_eq!(b_info.email.as_deref(), Some("b@example.com"));

        // A's domain is untouched.
        assert!(
            bucket_account_info_for(&app, &key_a).is_none(),
            "session A's account_info must not be set"
        );

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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                session_id: key_a.as_str().to_owned(),
                account,
                forge_account: None,
            },
        );
        let a_info = bucket_account_info_for(&app, &key_a).expect("a account_info");
        assert_eq!(a_info.email.as_deref(), Some("a@example.com"));
        assert!(bucket_account_info_for(&app, &key_b).is_none());
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
        app.sessions.insert(key_a.clone(), UiSession::new(key_a.clone()));
        if let Some(ws) = app.workspace.as_ref() {
            let (h, _) = forge_workspace::Workspace::testing_stub_handle();
            let dom = ws.register_domain_session(key_a.clone(), Some(std::sync::Arc::new(h)));
            dom.lock().session_id = Some(forge_primitives::SessionId::new("a"));
        }
        app.active_session_key = Some(key_a.clone());
        app.needs_redraw = false;

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                session_id: "a".to_owned(),
                account: forge_primitives::AccountInfo::default(),
                forge_account: None,
            },
        );

        assert!(app.needs_redraw, "active-session events must flip needs_redraw");
    }

    fn make_creds() -> forge_primitives::cloud::oauth_credentials::OauthCredentials {
        forge_primitives::cloud::oauth_credentials::OauthCredentials {
            access_token: "tok".to_owned(),
            expires_at: None,
        }
    }

    #[test]
    fn oauth_credentials_snapshot_routes_to_target_session_only() {
        let mut app = App::test_default();
        let (key_a, key_b) = seed_two_sessions(&mut app);
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::OauthCredentialsSnapshot {
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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::OauthCredentialsSnapshot {
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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::ContextUsageSnapshot {
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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::ContextUsageSnapshot {
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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::McpSnapshot {
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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::McpSnapshot {
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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::HookObservation {
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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::HookObservation {
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
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
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

    /// `SessionUpdate::Spawning` should seed a placeholder bucket
    /// under the synthetic key with `Spawning` lifecycle state, a
    /// "Waking …" system message, and switch the active session
    /// to the placeholder so the user sees the wake immediately.
    #[test]
    fn spawning_reducer_seeds_placeholder_bucket_and_switches_active() {
        let mut app = App::test_default();
        // Strip the pre-Connect bucket so the assertions are clean.
        app.sessions.clear();
        app.active_session_key = None;

        let synth_key = SessionKey::from_session_id("__spawn_forge__".to_owned());
        // Simulate the workspace's spawn-path: it would normally
        // register a DomainSession under `synth_key` before emitting
        // the SessionUpdate::Spawning. Tests bypass the workspace
        // spawn path and synthesize the SessionUpdate directly, so
        // pre-register the domain handle here to mirror production.
        {
            let ws = app.workspace.as_ref().expect("workspace stub present in test_default");
            let (stub_handle, _) = forge_workspace::Workspace::testing_stub_handle();
            ws.register_domain_session(
                synth_key.clone(),
                Some(std::sync::Arc::new(stub_handle)),
            );
        }

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: synth_key.clone(),
                project_name: "forge".to_owned(),
                cwd: "/Users/v/Projects/forge".to_owned(),
                display_name: "forge".to_owned(),
            },
        );

        let bucket = app.sessions.get(&synth_key).expect("spawn bucket created");
        assert!(
            matches!(bucket.lifecycle_state, crate::app::session::SessionLifecycleState::Spawning),
            "lifecycle state set to Spawning, got {:?}",
            bucket.lifecycle_state,
        );
        assert_eq!(bucket.cwd_raw, "/Users/v/Projects/forge");
        assert!(
            bucket.messages.iter().any(|m| matches!(m.role, crate::app::MessageRole::System(_))),
            "spawning placeholder system message present"
        );
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&synth_key),
            "active session switched to the placeholder",
        );
    }

    /// User-visible bug fix verification: after the spawning reducer
    /// runs, `is_animating` (the render-loop probe) sees a session in
    /// `Spawning` lifecycle and keeps the spinner ticking. Pre-fix,
    /// the lifecycle state lived on `DomainSession` and a synth-key
    /// race left it stuck at `Idle`, freezing the spinner on wake-up.
    #[test]
    fn spinner_animates_during_spawning() {
        let mut app = App::test_default();
        app.sessions.clear();
        app.active_session_key = None;

        let synth_key = SessionKey::from_session_id("__spawn_forge__".to_owned());
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: synth_key.clone(),
                project_name: "forge".to_owned(),
                cwd: "/Users/v/Projects/forge".to_owned(),
                display_name: "forge".to_owned(),
            },
        );

        let any_spawning_or_running = app.sessions.values().any(|s| {
            matches!(
                s.lifecycle_state,
                crate::app::session::SessionLifecycleState::Running
                    | crate::app::session::SessionLifecycleState::Spawning
            )
        });
        assert!(
            any_spawning_or_running,
            "Spawning bucket should drive the spinner via direct UiSession read",
        );
    }

    /// `SessionUpdate::Spawning` writes `cwd_raw` directly onto the
    /// bucket. Previously this was mirrored onto a workspace
    /// `DomainSession`; now the bucket is the only owner.
    #[test]
    fn spawning_reducer_writes_cwd_raw_onto_bucket() {
        let mut app = App::test_default();
        app.sessions.clear();
        app.active_session_key = None;
        let key = SessionKey::from_session_id("__spawn_a__".to_owned());
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: key.clone(),
                project_name: "a".to_owned(),
                cwd: "/p/a".to_owned(),
                display_name: "a".to_owned(),
            },
        );
        let bucket = app.sessions.get(&key).expect("bucket created");
        assert_eq!(bucket.cwd_raw, "/p/a");
    }

    /// `SessionUpdate::StatusSnapshot` for a background session writes
    /// `account_info` + `active_account_display_name` directly onto
    /// the target bucket without touching the active session's bucket.
    #[test]
    fn status_snapshot_background_writes_account_fields_to_bucket() {
        let mut app = App::test_default();
        let (active, background) = seed_two_sessions(&mut app);
        let account = forge_primitives::AccountInfo {
            email: Some("bg@example.com".to_owned()),
            ..Default::default()
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::StatusSnapshot {
                session_id: background.as_str().to_owned(),
                account: account.clone(),
                forge_account: Some(forge_primitives::ForgeAccountIdentity::new(
                    "Background".to_owned(),
                )),
            },
        );
        assert!(bucket_account_info_for(&app, &background).is_some());
        assert_eq!(
            app.sessions.get(&background).and_then(|s| s.active_account_display_name.as_deref()),
            Some("Background"),
        );
        // Active bucket left untouched.
        assert!(bucket_account_info_for(&app, &active).is_none());
    }

    /// `SessionUpdate::Connected` on a background bucket writes
    /// session_id + cwd_raw + lifecycle Idle directly onto the bucket
    /// while also mirroring session_id onto the workspace's
    /// DomainSession for AgentHandle dispatch.
    #[test]
    fn connected_background_writes_session_id_onto_bucket_and_domain() {
        let mut app = App::test_default();
        let (_active, background) = seed_two_sessions(&mut app);
        // Clear the bucket's session_id so we can verify the reducer
        // re-stamps it.
        if let Some(b) = app.sessions.get_mut(&background) {
            b.session_id = None;
            b.cwd_raw = String::new();
            b.lifecycle_state = crate::app::session::SessionLifecycleState::Spawning;
        }
        let current_model = forge_primitives::CurrentModel {
            resolved_id: "claude".to_owned(),
            display_name_short: "claude".to_owned(),
            display_name_long: "claude".to_owned(),
            requested_id: None,
            catalog_id: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_fast_mode: None,
            supports_auto_mode: None,
            supports_adaptive_thinking: None,
            is_authoritative: true,
        };
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Connected {
                key: background.clone(),
                session_id: forge_primitives::SessionId::new(background.as_str()),
                cwd: "/bg".to_owned(),
                current_model,
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
            },
        );
        let bucket = app.sessions.get(&background).expect("bucket present");
        assert_eq!(bucket.cwd_raw, "/bg");
        assert!(matches!(bucket.lifecycle_state, crate::app::session::SessionLifecycleState::Idle));
        assert_eq!(
            bucket.session_id.as_ref().map(std::string::ToString::to_string),
            Some(background.as_str().to_owned()),
        );
        // The reducer also mirrors session_id onto the workspace's
        // DomainSession so AgentHandle dispatch routes through the
        // claude-issued UUID. Without this, `Command::Cancel` /
        // `Prompt` for the background session would carry the wrong
        // (or no) session_id when SessionTask processes them.
        let domain = app
            .workspace
            .as_ref()
            .and_then(|ws| ws.domain_session_for(&background))
            .expect("domain registered by seed_two_sessions");
        let domain_sid = domain.lock().session_id.as_ref().map(std::string::ToString::to_string);
        assert_eq!(domain_sid, Some(background.as_str().to_owned()));
    }

    /// `SessionUpdate::Spawning` should be idempotent: a second
    /// Spawning for the same key (rapid double-click) must NOT
    /// reset the bucket — just switch active. Without this, a
    /// duplicate Spawning event would erase any state already
    /// accumulated under the synthetic key.
    #[test]
    fn spawning_reducer_is_idempotent_for_repeat_keys() {
        let mut app = App::test_default();
        app.sessions.clear();
        let key = SessionKey::from_session_id("__spawn_proj__".to_owned());
        // First Spawning seeds the bucket.
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: key.clone(),
                project_name: "proj".to_owned(),
                cwd: "/proj".to_owned(),
                display_name: "proj".to_owned(),
            },
        );
        let messages_after_first = app.sessions.get(&key).expect("bucket").messages.len();
        // User clicks elsewhere; we mark this by appending state.
        if let Some(b) = app.sessions.get_mut(&key) {
            b.messages.push(crate::app::ChatMessage::new(
                crate::app::MessageRole::System(Some(crate::app::SystemSeverity::Info)),
                vec![crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(
                    "intermediate state",
                ))],
                None,
            ));
            b.message_retained_bytes.push(0);
        }
        let messages_after_second_state = app.sessions.get(&key).expect("bucket").messages.len();
        assert!(messages_after_second_state > messages_after_first);
        // Second Spawning for the same key (rapid double click).
        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Spawning {
                key: key.clone(),
                project_name: "proj".to_owned(),
                cwd: "/proj".to_owned(),
                display_name: "proj".to_owned(),
            },
        );
        // Bucket state preserved (idempotent — no re-seed).
        assert_eq!(
            app.sessions.get(&key).expect("bucket").messages.len(),
            messages_after_second_state,
        );
        // Still switched to active.
        assert_eq!(app.active_session_key.as_ref(), Some(&key));
    }

    /// `SessionUpdate::KeyRenamed { from, to }` should migrate a
    /// bucket from the synthetic spawn key to the real claude
    /// session UUID. The bucket's contents (messages, cwd) must
    /// survive the migration. If `active_session_key` points at
    /// `from`, it must follow to `to`.
    #[test]
    fn key_renamed_migrates_bucket_and_follows_active() {
        let mut app = App::test_default();
        app.sessions.clear();

        let from = SessionKey::from_session_id("__spawn_proj__".to_owned());
        let to = SessionKey::from_session_id("real-uuid-9000".to_owned());

        // Seed a placeholder bucket under `from` with some content
        // so we can verify the migration preserves state.
        let mut bucket = UiSession::new(from.clone());
        bucket.cwd_raw = "/proj".to_owned();
        bucket.messages.push(crate::app::ChatMessage::new(
            crate::app::MessageRole::System(Some(crate::app::SystemSeverity::Info)),
            vec![crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(
                "Waking proj…",
            ))],
            None,
        ));
        app.sessions.insert(from.clone(), bucket);
        app.active_session_key = Some(from.clone());

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::KeyRenamed { from: from.clone(), to: to.clone() },
        );

        // Old key gone, new key present.
        assert!(!app.sessions.contains_key(&from), "from key removed");
        let migrated = app.sessions.get(&to).expect("bucket migrated to real key");
        assert_eq!(migrated.messages.len(), 1);
        // Active follows.
        assert_eq!(app.active_session_key.as_ref(), Some(&to));
    }

    /// `SessionUpdate::KeyRenamed` must NOT hijack the active
    /// session when the user has already switched away from the
    /// spawning bucket. The migration still happens, but active
    /// stays on whatever the user picked.
    #[test]
    fn key_renamed_does_not_hijack_user_active_pick() {
        let mut app = App::test_default();
        app.sessions.clear();

        let from = SessionKey::from_session_id("__spawn_bg__".to_owned());
        let to = SessionKey::from_session_id("real-uuid-bg".to_owned());
        let user_pick = SessionKey::from_str_for_test("user-pick");

        app.sessions.insert(from.clone(), UiSession::new(from.clone()));
        app.sessions.insert(user_pick.clone(), UiSession::new(user_pick.clone()));
        app.active_session_key = Some(user_pick.clone());

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::KeyRenamed { from: from.clone(), to: to.clone() },
        );

        // Migration happened.
        assert!(!app.sessions.contains_key(&from));
        assert!(app.sessions.contains_key(&to));
        // Active stayed where the user put it.
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&user_pick),
            "active_session_key must stay on user_pick after a background KeyRenamed",
        );
    }

    /// Build a minimal `forge_primitives::CurrentModel` for tests
    /// that need to fire a `SessionUpdate::Connected` /
    /// `SessionUpdate::SessionReplaced` envelope. Field values are
    /// deliberately uninteresting — the assertion target is the
    /// file_index side effect, not the model state.
    fn test_current_model() -> forge_primitives::CurrentModel {
        forge_primitives::CurrentModel {
            requested_id: None,
            resolved_id: "test-model".to_owned(),
            display_name_short: "test-model".to_owned(),
            display_name_long: "test-model".to_owned(),
            catalog_id: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_fast_mode: None,
            supports_auto_mode: None,
            supports_adaptive_thinking: None,
            is_authoritative: true,
        }
    }

    /// `SessionUpdate::Connected` for the active session reaches the
    /// active apply-chain path which restarts `app.file_index` with
    /// the new cwd. After the event lands, the file_index root must
    /// match the new cwd, the generation must have advanced, and the
    /// stale `entries` map must have been cleared so the next scan
    /// starts from a clean slate. The asynchronous scan completion
    /// itself isn't asserted — only that the synchronous restart side
    /// effects fired against the production reducer path.
    #[test]
    fn connected_refreshes_file_index_candidates_for_new_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonicalize");
        let mut app = App::test_default();
        // Seed stale file_index state to verify the restart wipes it.
        app.file_index_mut().generation = 3;
        app.file_index_mut().root = Some(std::path::PathBuf::from("/old/path"));
        app.file_index_mut().entries.insert(
            "stale.rs".to_owned(),
            crate::app::file_index::FileCandidate {
                rel_path: "stale.rs".to_owned(),
                rel_path_lower: "stale.rs".to_owned(),
                basename_lower: "stale.rs".to_owned(),
                depth: 0,
                modified: std::time::SystemTime::UNIX_EPOCH,
                is_dir: false,
            },
        );
        app.file_index_mut().scan_finished = true;

        let pending_key = app.active_session_key.clone().expect("pending active key");
        let new_cwd = canonical.to_string_lossy().into_owned();

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::Connected {
                key: pending_key,
                session_id: forge_primitives::SessionId::new("session-1"),
                cwd: new_cwd.clone(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
            },
        );

        assert_eq!(
            app.file_index_mut().root.as_deref(),
            Some(canonical.as_path()),
            "file_index root must follow the Connected cwd",
        );
        assert!(
            app.file_index_mut().generation > 3,
            "file_index generation must advance on restart"
        );
        assert!(app.file_index_mut().entries.is_empty(), "stale entries cleared on restart");
        assert!(!app.file_index_mut().scan_finished, "scan_finished reset on restart");
    }

    /// `SessionUpdate::SessionReplaced` shares the
    /// `handle_session_replaced_event` path which restarts the
    /// `file_index` against the replaced cwd. Same assertion shape as
    /// the `Connected` test — production code path runs through
    /// `apply_session_update_session_replaced` →
    /// `handle_session_replaced_event` → `file_index::restart`.
    #[test]
    fn session_replaced_refreshes_file_index_candidates_for_replaced_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonicalize");
        let mut app = App::test_default();
        app.file_index_mut().generation = 8;
        app.file_index_mut().root = Some(std::path::PathBuf::from("/before"));
        app.file_index_mut().entries.insert(
            "before.rs".to_owned(),
            crate::app::file_index::FileCandidate {
                rel_path: "before.rs".to_owned(),
                rel_path_lower: "before.rs".to_owned(),
                basename_lower: "before.rs".to_owned(),
                depth: 0,
                modified: std::time::SystemTime::UNIX_EPOCH,
                is_dir: false,
            },
        );
        app.file_index_mut().scan_finished = true;

        let pending_key = app.active_session_key.clone().expect("pending active key");
        let replaced_cwd = canonical.to_string_lossy().into_owned();

        apply_session_update(
            &mut app,
            forge_workspace::SessionUpdate::SessionReplaced {
                key: pending_key,
                session_id: forge_primitives::SessionId::new("session-2"),
                cwd: replaced_cwd.clone(),
                current_model: test_current_model(),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
            },
        );

        assert_eq!(
            app.file_index_mut().root.as_deref(),
            Some(canonical.as_path()),
            "file_index root must follow the SessionReplaced cwd",
        );
        assert!(
            app.file_index_mut().generation > 8,
            "file_index generation must advance on restart"
        );
        assert!(app.file_index_mut().entries.is_empty(), "stale entries cleared on restart");
        assert!(!app.file_index_mut().scan_finished, "scan_finished reset on restart");
    }
}
