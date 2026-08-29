use super::ConfigOverlayState;
use crate::app::App;
use crossterm::event::KeyEvent;

pub(super) fn handle_overlay_key(app: &mut App, key: KeyEvent) {
    if super::mcp_overlay::handle_overlay_key(app, key) {
        return;
    }
    match app.config.overlay.clone() {
        Some(ConfigOverlayState::InstalledPluginActions(_)) => {
            crate::app::plugins::handle_installed_overlay_key(app, key);
        }
        Some(ConfigOverlayState::PluginInstallActions(_)) => {
            crate::app::plugins::handle_plugin_install_overlay_key(app, key);
        }
        Some(ConfigOverlayState::MarketplaceActions(_)) => {
            crate::app::plugins::handle_marketplace_overlay_key(app, key);
        }
        Some(ConfigOverlayState::AddMarketplace(_)) => {
            crate::app::plugins::handle_add_marketplace_overlay_key(app, key);
        }
        Some(ConfigOverlayState::McpDetails(_)) | None => {}
    }
}

pub(super) fn handle_overlay_paste(app: &mut App, text: &str) -> bool {
    if super::mcp_overlay::handle_overlay_paste(app, text) {
        return true;
    }
    match app.config.overlay {
        Some(ConfigOverlayState::AddMarketplace(_)) => {
            crate::app::plugins::handle_add_marketplace_overlay_paste(app, text);
            true
        }
        Some(
            ConfigOverlayState::InstalledPluginActions(_)
            | ConfigOverlayState::PluginInstallActions(_)
            | ConfigOverlayState::MarketplaceActions(_)
            | ConfigOverlayState::McpDetails(_),
        )
        | None => false,
    }
}

pub(super) fn step_index_clamped(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs()).min(len.saturating_sub(1))
    } else {
        (current + delta.cast_unsigned()).min(len.saturating_sub(1))
    }
}
