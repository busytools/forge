use super::{ConfigOverlayState, ConfigState};
use crate::app::App;
use crate::app::view::ActiveView;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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

pub(crate) fn request_mcp_snapshot(app: &mut App) {
    let Some(workspace) = app.workspace.clone() else {
        app.mcp_mut().in_flight = false;
        return;
    };
    let Some(key) = app.active_session_key.clone() else {
        app.mcp_mut().in_flight = false;
        return;
    };
    let Some(session_id) = app.session_id().map(|s| s.to_string()) else {
        app.mcp_mut().in_flight = false;
        return;
    };
    app.mcp_mut().in_flight = true;
    app.mcp_mut().last_error = None;
    match workspace.refresh_mcp_snapshot(&key) {
        Ok(()) => tracing::debug!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "mcp_snapshot_requested",
            message = "MCP snapshot requested",
            outcome = "start",
            session_id = %session_id,
        ),
        Err(err) => {
            app.mcp_mut().in_flight = false;
            app.mcp_mut().last_error = Some(err.to_string());
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
    error: &forge_primitives::McpOperationError,
) {
    app.mcp_mut().in_flight = false;
    let formatted = format_mcp_operation_error(error);
    app.mcp_mut().last_error = Some(formatted.clone());
    app.config.last_error = Some(formatted);
    app.config.status_message = None;
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
