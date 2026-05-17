use super::{AddMarketplaceOverlayState, ConfigOverlayState};
use crate::app::App;
use crossterm::event::{KeyEvent, KeyModifiers};

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
        Some(
            ConfigOverlayState::McpDetails(_)
            | ConfigOverlayState::McpCallbackUrl(_)
            | ConfigOverlayState::McpAuthRedirect(_)
            | ConfigOverlayState::McpElicitation(_),
        )
        | None => {}
    }
}

pub(super) fn handle_overlay_paste(app: &mut App, text: &str) -> bool {
    if super::mcp_overlay::handle_overlay_paste(app, text) {
        return true;
    }
    match app.config.overlay {
        Some(ConfigOverlayState::AddMarketplace(_)) => {
            insert_text_str(app.config.add_marketplace_overlay_mut(), text);
            true
        }
        Some(
            ConfigOverlayState::InstalledPluginActions(_)
            | ConfigOverlayState::PluginInstallActions(_)
            | ConfigOverlayState::MarketplaceActions(_)
            | ConfigOverlayState::McpDetails(_)
            | ConfigOverlayState::McpCallbackUrl(_)
            | ConfigOverlayState::McpAuthRedirect(_)
            | ConfigOverlayState::McpElicitation(_),
        )
        | None => false,
    }
}

pub(super) fn accepts_text_input(modifiers: KeyModifiers) -> bool {
    modifiers.is_empty() || modifiers == KeyModifiers::SHIFT
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

fn char_to_byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices().nth(char_index).map_or(text.len(), |(idx, _)| idx)
}

pub(super) fn move_text_cursor_left<T: TextInputOverlay>(overlay: Option<&mut T>) {
    let Some(overlay) = overlay else {
        return;
    };
    *overlay.cursor_mut() = overlay.cursor().saturating_sub(1);
}

pub(super) fn move_text_cursor_right<T: TextInputOverlay>(overlay: Option<&mut T>) {
    let Some(overlay) = overlay else {
        return;
    };
    let next = overlay.cursor().saturating_add(1).min(overlay.draft().chars().count());
    *overlay.cursor_mut() = next;
}

pub(super) fn move_text_cursor_to_end<T: TextInputOverlay>(overlay: Option<&mut T>) {
    let Some(overlay) = overlay else {
        return;
    };
    *overlay.cursor_mut() = overlay.draft().chars().count();
}

pub(super) fn set_text_cursor<T: TextInputOverlay>(overlay: Option<&mut T>, cursor: usize) {
    let Some(overlay) = overlay else {
        return;
    };
    *overlay.cursor_mut() = cursor.min(overlay.draft().chars().count());
}

pub(super) fn insert_text_char<T: TextInputOverlay>(overlay: Option<&mut T>, ch: char) {
    let Some(overlay) = overlay else {
        return;
    };
    let byte_index = char_to_byte_index(overlay.draft(), overlay.cursor());
    overlay.draft_mut().insert(byte_index, ch);
    *overlay.cursor_mut() += 1;
}

pub(super) fn insert_text_str<T: TextInputOverlay>(overlay: Option<&mut T>, text: &str) {
    let Some(overlay) = overlay else {
        return;
    };
    let byte_index = char_to_byte_index(overlay.draft(), overlay.cursor());
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n").replace('\n', " ");
    overlay.draft_mut().insert_str(byte_index, &normalized);
    *overlay.cursor_mut() += normalized.chars().count();
}

pub(super) fn delete_text_before_cursor<T: TextInputOverlay>(overlay: Option<&mut T>) {
    let Some(overlay) = overlay else {
        return;
    };
    if overlay.cursor() == 0 {
        return;
    }
    let end = char_to_byte_index(overlay.draft(), overlay.cursor());
    let start = char_to_byte_index(overlay.draft(), overlay.cursor() - 1);
    overlay.draft_mut().replace_range(start..end, "");
    *overlay.cursor_mut() -= 1;
}

pub(super) fn delete_text_at_cursor<T: TextInputOverlay>(overlay: Option<&mut T>) {
    let Some(overlay) = overlay else {
        return;
    };
    let char_count = overlay.draft().chars().count();
    if overlay.cursor() >= char_count {
        return;
    }
    let start = char_to_byte_index(overlay.draft(), overlay.cursor());
    let end = char_to_byte_index(overlay.draft(), overlay.cursor() + 1);
    overlay.draft_mut().replace_range(start..end, "");
}

pub(super) trait TextInputOverlay {
    fn draft(&self) -> &str;
    fn draft_mut(&mut self) -> &mut String;
    fn cursor(&self) -> usize;
    fn cursor_mut(&mut self) -> &mut usize;
}

impl TextInputOverlay for AddMarketplaceOverlayState {
    fn draft(&self) -> &str {
        &self.draft
    }

    fn draft_mut(&mut self) -> &mut String {
        &mut self.draft
    }

    fn cursor(&self) -> usize {
        self.cursor
    }

    fn cursor_mut(&mut self) -> &mut usize {
        &mut self.cursor
    }
}

impl AddMarketplaceOverlayState {
    pub(crate) fn from_text_input(draft: String, cursor: usize) -> Self {
        Self { draft, cursor }
    }
}
