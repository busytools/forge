use super::{ConfigOverlayState, ConfigState};
use crate::app::App;
use crate::app::view::ActiveView;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpServerActionKind {
    RefreshSnapshot,
    Reconnect,
    Enable,
    Disable,
}

impl McpServerActionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::RefreshSnapshot => "Refresh",
            Self::Reconnect => "Reconnect server",
            Self::Enable => "Enable server",
            Self::Disable => "Disable server",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpDetailsOverlayState {
    pub server_name: String,
    pub selected_index: usize,
}

impl ConfigState {
    pub fn mcp_details_overlay(&self) -> Option<&McpDetailsOverlayState> {
        if let Some(ConfigOverlayState::McpDetails(overlay)) = &self.overlay {
            Some(overlay)
        } else {
            None
        }
    }

    pub fn mcp_details_overlay_mut(&mut self) -> Option<&mut McpDetailsOverlayState> {
        if let Some(ConfigOverlayState::McpDetails(overlay)) = &mut self.overlay {
            Some(overlay)
        } else {
            None
        }
    }
}

pub(super) fn handle_mcp_key(app: &mut App, key: KeyEvent) -> bool {
    if app.active_view != ActiveView::Mcp {
        return false;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char(ch), modifiers)
            if matches!(ch, 'r' | 'R')
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT) =>
        {
            crate::app::session_runtime::request_runtime_reload(app);
            refresh_mcp_snapshot(app);
            true
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            open_selected_mcp_server_details(app);
            true
        }
        (KeyCode::Up, KeyModifiers::NONE) => {
            app.config.mcp_selected_server_index =
                app.config.mcp_selected_server_index.saturating_sub(1);
            true
        }
        (KeyCode::Down, KeyModifiers::NONE) => {
            let last_index = app.mcp().servers.len().saturating_sub(1);
            app.config.mcp_selected_server_index =
                (app.config.mcp_selected_server_index + 1).min(last_index);
            true
        }
        _ => false,
    }
}

pub(crate) fn refresh_mcp_snapshot_if_needed(app: &mut App) {
    if app.active_view == ActiveView::Mcp {
        refresh_mcp_snapshot(app);
    }
}

pub(crate) fn refresh_mcp_snapshot(app: &mut App) {
    app.mcp_mut().servers.clear();
    app.mcp_mut().last_error = None;
    request_mcp_snapshot(app);
}

/// Re-poll cadence for the background snapshot refresh: fast while a
/// server is still `Pending`, since its handshake product and tool list
/// aren't on the wire until it connects.
const MCP_PENDING_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Slow cadence once nothing is pending, so a server that later fails or
/// reconnects still reaches the Inspector.
const MCP_SETTLED_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

/// Re-ask for the snapshot when the held one has aged past its cadence.
///
/// Unlike [`refresh_mcp_snapshot`] this neither clears `servers` nor
/// raises `in_flight`, so a background poll can't blank the Inspector's
/// MCP rows or flash the standalone view's loading line. Preserving a
/// failed reconnect's error is the response leg's job, not this one -
/// see `apply_mcp_snapshot_presentation`.
///
/// Reads the active bucket only, so a background session's rows stay as
/// its connect-time snapshot left them until the user focuses it.
pub(crate) fn request_mcp_snapshot_if_needed(app: &mut App, now: Instant) {
    let interval = if app
        .mcp()
        .servers
        .iter()
        .any(|server| server.status == forge_primitives::McpServerConnectionStatus::Pending)
    {
        MCP_PENDING_REFRESH_INTERVAL
    } else {
        MCP_SETTLED_REFRESH_INTERVAL
    };
    if app.mcp().last_refresh_requested.is_some_and(|last| now.duration_since(last) < interval) {
        return;
    }
    dispatch_mcp_snapshot_request(app, now, false);
}

pub(crate) fn request_mcp_snapshot(app: &mut App) {
    dispatch_mcp_snapshot_request(app, Instant::now(), true);
}

/// Ask the workspace for a fresh snapshot. `mark_in_flight` drives the
/// user-facing loading state, which only a user-initiated refresh owns.
fn dispatch_mcp_snapshot_request(app: &mut App, now: Instant, mark_in_flight: bool) {
    // Borrow-only check first. Pre-connect there is no session id and the
    // background poll reaches here on every 4ms loop tick, so the clones
    // below must not run just to be dropped.
    let Some(session_id) = app.session_id().map(|s| s.to_string()) else {
        if mark_in_flight {
            app.mcp_mut().in_flight = false;
        }
        return;
    };
    let Some(workspace) = app.workspace.clone() else {
        if mark_in_flight {
            app.mcp_mut().in_flight = false;
        }
        return;
    };
    let Some(key) = app.active_session_key.clone() else {
        if mark_in_flight {
            app.mcp_mut().in_flight = false;
        }
        return;
    };
    app.mcp_mut().last_refresh_requested = Some(now);
    if mark_in_flight {
        app.mcp_mut().in_flight = true;
        app.mcp_mut().last_error = None;
    }
    match workspace.refresh_mcp_snapshot(&key) {
        Ok(()) => tracing::debug!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "mcp_snapshot_requested",
            message = "MCP snapshot requested",
            outcome = "start",
            session_id = %session_id,
        ),
        Err(err) => {
            if mark_in_flight {
                app.mcp_mut().in_flight = false;
                app.mcp_mut().last_error = Some(err.to_string());
            }
            tracing::warn!(
                target: crate::logging::targets::APP_CONFIG,
                event_name = "mcp_snapshot_request_failed",
                message = "failed to request MCP snapshot",
                outcome = "failure",
                session_id = %session_id,
                error_message = %err,
            );
        }
    }
}

pub(crate) fn reconnect_mcp_server(app: &mut App, server_name: &str) {
    if !app.has_active_agent() {
        return;
    }
    let Some(session_id) = app.session_id() else {
        return;
    };
    let session_id = session_id.to_string();
    let server_name_owned = server_name.to_owned();
    match app.dispatch_command(|key| forge_workspace::Command::ReconnectMcpServer {
        key,
        server_name: server_name_owned,
    }) {
        Ok(()) => {
            tracing::info!(
                target: crate::logging::targets::APP_CONFIG,
                event_name = "mcp_reconnect_requested",
                message = "MCP reconnect requested",
                outcome = "start",
                session_id = %session_id,
                server_name = %server_name,
            );
            refresh_mcp_snapshot(app);
        }
        Err(error) => tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "mcp_reconnect_request_failed",
            message = "failed to request MCP reconnect",
            outcome = "failure",
            session_id = %session_id,
            server_name = %server_name,
            error_message = %error,
        ),
    }
}

pub(crate) fn set_mcp_server_enabled(app: &mut App, server_name: &str, enabled: bool) {
    if !app.has_active_agent() {
        return;
    }
    let Some(session_id) = app.session_id() else {
        return;
    };
    let session_id = session_id.to_string();
    let server_name_owned = server_name.to_owned();
    match app.dispatch_command(|key| forge_workspace::Command::ToggleMcpServer {
        key,
        server_name: server_name_owned,
        enabled,
    }) {
        Ok(()) => {
            tracing::info!(
                target: crate::logging::targets::APP_CONFIG,
                event_name = "mcp_toggle_requested",
                message = "MCP server toggle requested",
                outcome = "start",
                session_id = %session_id,
                server_name = %server_name,
                enabled,
            );
            refresh_mcp_snapshot(app);
        }
        Err(error) => tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "mcp_toggle_request_failed",
            message = "failed to request MCP server toggle",
            outcome = "failure",
            session_id = %session_id,
            server_name = %server_name,
            enabled,
            error_message = %error,
        ),
    }
}

fn open_selected_mcp_server_details(app: &mut App) {
    let Some(server_name) = app
        .mcp()
        .servers
        .get(app.config.mcp_selected_server_index)
        .map(|server| server.name.clone())
    else {
        return;
    };
    open_mcp_server_details(app, server_name, None);
}

pub(crate) fn open_mcp_server_details(
    app: &mut App,
    server_name: String,
    preferred_action: Option<McpServerActionKind>,
) {
    let selected_index =
        app.mcp().servers.iter().find(|server| server.name == server_name).map_or(0, |server| {
            preferred_action
                .and_then(|action| {
                    available_mcp_actions(server).iter().position(|candidate| *candidate == action)
                })
                .unwrap_or(0)
        });
    app.config.overlay = Some(ConfigOverlayState::McpDetails(McpDetailsOverlayState {
        server_name,
        selected_index,
    }));
    app.config.last_error = None;
}

pub(crate) fn available_mcp_actions(
    server: &forge_primitives::McpServerStatus,
) -> Vec<McpServerActionKind> {
    let mut actions = vec![McpServerActionKind::RefreshSnapshot];
    if matches!(server.status, forge_primitives::McpServerConnectionStatus::Disabled) {
        actions.push(McpServerActionKind::Enable);
    } else {
        actions.push(McpServerActionKind::Reconnect);
        actions.push(McpServerActionKind::Disable);
    }
    actions
}

pub(crate) fn is_mcp_action_available(
    _server: &forge_primitives::McpServerStatus,
    _action: McpServerActionKind,
) -> bool {
    true
}

pub(crate) fn handle_mcp_operation_error(
    app: &mut App,
    key: &forge_workspace::SessionKey,
    error: &forge_primitives::McpOperationError,
) {
    let formatted = format_mcp_operation_error(error);
    // Target the session the error belongs to, not whatever is focused
    // (mirrors the McpSnapshot leg). Only the active session touches the
    // config overlay's own error line; a background session's failure
    // writes its own bucket so it surfaces when the user switches to it.
    let is_active = app.active_session_key.as_ref() == Some(key);
    if is_active {
        app.mcp_mut().in_flight = false;
        app.mcp_mut().last_error = Some(formatted.clone());
        app.config.last_error = Some(formatted);
        app.config.status_message = None;
    } else if let Some(session) = app.session_mut(key) {
        session.mcp.in_flight = false;
        session.mcp.last_error = Some(formatted);
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "mcp_operation_error_dropped",
            message = "MCP operation error for an unknown session",
            outcome = "dropped",
            session_id = %key.as_str(),
        );
        return;
    }
    tracing::error!(
        target: crate::logging::targets::APP_CONFIG,
        event_name = "mcp_operation_error_applied",
        message = "MCP operation error applied",
        outcome = "failure",
        server_name = %error.server_name.as_deref().unwrap_or(""),
        operation = %error.operation,
        error_message = %error.message,
    );
}

fn format_mcp_operation_error(error: &forge_primitives::McpOperationError) -> String {
    let action = match error.operation.as_str() {
        "reconnect" => "reconnect",
        "toggle" => "update",
        other => other,
    };
    match error.server_name.as_deref() {
        Some(server_name) => {
            format!("Failed to {action} MCP server {server_name}: {}", error.message)
        }
        None => format!("MCP operation failed ({action}): {}", error.message),
    }
}
