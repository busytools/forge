use super::ConfigOverlayState;
use super::mcp::{
    McpServerActionKind, available_mcp_actions, is_mcp_action_available, reconnect_mcp_server,
    refresh_mcp_snapshot, set_mcp_server_enabled,
};
use super::overlay_input::step_index_clamped;
use crate::app::App;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(super) fn handle_overlay_key(app: &mut App, key: KeyEvent) -> bool {
    match app.config.overlay.clone() {
        Some(ConfigOverlayState::McpDetails(_)) => {
            handle_mcp_details_overlay_key(app, key);
            true
        }
        _ => false,
    }
}

pub(super) fn handle_overlay_paste(_app: &mut App, _text: &str) -> bool {
    false
}

fn handle_mcp_details_overlay_key(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) => app.config.overlay = None,
        (KeyCode::Up, KeyModifiers::NONE) => move_mcp_details_overlay_selection(app, -1),
        (KeyCode::Down, KeyModifiers::NONE) => move_mcp_details_overlay_selection(app, 1),
        (KeyCode::Enter, KeyModifiers::NONE) => execute_selected_mcp_overlay_action(app),
        _ => {}
    }
}

fn move_mcp_details_overlay_selection(app: &mut App, delta: isize) {
    let Some(overlay) = app.config.mcp_details_overlay().cloned() else {
        return;
    };
    let Some(server) = app.mcp().servers.iter().find(|server| server.name == overlay.server_name)
    else {
        return;
    };
    let actions = available_mcp_actions(server);
    if actions.is_empty() {
        return;
    }

    let next_index = step_index_clamped(overlay.selected_index, delta, actions.len());
    if let Some(state) = app.config.mcp_details_overlay_mut() {
        state.selected_index = next_index;
    }
}

fn execute_selected_mcp_overlay_action(app: &mut App) {
    let Some(overlay) = app.config.mcp_details_overlay().cloned() else {
        return;
    };
    let Some(server) = app.mcp().servers.iter().find(|server| server.name == overlay.server_name)
    else {
        app.config.overlay = None;
        return;
    };
    let actions = available_mcp_actions(server);
    let Some(action) = actions.get(overlay.selected_index).copied() else {
        return;
    };
    if !is_mcp_action_available(server, action) {
        return;
    }

    match action {
        McpServerActionKind::RefreshSnapshot => {
            crate::app::session_runtime::request_runtime_reload(app);
            refresh_mcp_snapshot(app);
        }
        McpServerActionKind::Reconnect => {
            reconnect_mcp_server(app, &overlay.server_name);
        }
        McpServerActionKind::Enable => {
            set_mcp_server_enabled(app, &overlay.server_name, true);
        }
        McpServerActionKind::Disable => {
            set_mcp_server_enabled(app, &overlay.server_name, false);
        }
    }

    app.config.overlay = None;
}
