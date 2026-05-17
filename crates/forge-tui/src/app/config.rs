mod edit;
mod mcp;
mod mcp_edit;
pub mod store;

use super::view::{self, ActiveView};
use crate::agent::model::EffortLevel;
use crate::app::App;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

pub(crate) use mcp::{
    McpAuthRedirectOverlayState, McpCallbackUrlOverlayState, McpDetailsOverlayState,
    McpElicitationOverlayState, available_mcp_actions, handle_mcp_elicitation_completed,
    handle_mcp_operation_error, is_mcp_action_available, present_mcp_auth_redirect,
    present_mcp_elicitation_request, refresh_mcp_snapshot,
};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigTab {
    Plugins,
    Mcp,
}

impl ConfigTab {
    pub const ALL: [Self; 2] = [Self::Plugins, Self::Mcp];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Plugins => "Plugins",
            Self::Mcp => "MCP",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Plugins => Self::Mcp,
            Self::Mcp => Self::Plugins,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Plugins => Self::Mcp,
            Self::Mcp => Self::Plugins,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DefaultPermissionMode {
    #[default]
    Default,
    Auto,
    AcceptEdits,
    Plan,
    DontAsk,
    BypassPermissions,
}

impl DefaultPermissionMode {
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Auto => "auto",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "auto" => Some(Self::Auto),
            "acceptEdits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "dontAsk" => Some(Self::DontAsk),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreferredNotifChannel {
    #[default]
    Iterm2,
    Iterm2WithBell,
    TerminalBell,
    NotificationsDisabled,
    Ghostty,
}

impl PreferredNotifChannel {
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::Iterm2 => "iterm2",
            Self::Iterm2WithBell => "iterm2_with_bell",
            Self::TerminalBell => "terminal_bell",
            Self::NotificationsDisabled => "notifications_disabled",
            Self::Ghostty => "ghostty",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "iterm2" => Some(Self::Iterm2),
            "iterm2_with_bell" => Some(Self::Iterm2WithBell),
            "terminal_bell" => Some(Self::TerminalBell),
            "notifications_disabled" => Some(Self::NotificationsDisabled),
            "ghostty" => Some(Self::Ghostty),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputStyle {
    #[default]
    Default,
    Explanatory,
    Learning,
}

impl OutputStyle {
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Explanatory => "Explanatory",
            Self::Learning => "Learning",
        }
    }

    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "Default" => Some(Self::Default),
            "Explanatory" => Some(Self::Explanatory),
            "Learning" => Some(Self::Learning),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketplaceActionKind {
    Update,
    Remove,
}

impl MarketplaceActionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Update => "Update",
            Self::Remove => "Remove",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstalledPluginActionKind {
    Enable,
    Disable,
    Update,
    InstallInCurrentProject,
    Uninstall,
}

impl InstalledPluginActionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Enable => "Enable",
            Self::Disable => "Disable",
            Self::Update => "Update",
            Self::InstallInCurrentProject => "Install in current project",
            Self::Uninstall => "Uninstall",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginInstallActionKind {
    User,
    Project,
    Local,
}

impl PluginInstallActionKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::User => "Install for user",
            Self::Project => "Install for project",
            Self::Local => "Install locally",
        }
    }

    pub const fn scope(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPluginActionOverlayState {
    pub plugin_id: String,
    pub title: String,
    pub description: String,
    pub scope: String,
    pub project_path: Option<String>,
    pub selected_index: usize,
    pub actions: Vec<InstalledPluginActionKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstallOverlayState {
    pub plugin_id: String,
    pub title: String,
    pub description: String,
    pub selected_index: usize,
    pub actions: Vec<PluginInstallActionKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceActionsOverlayState {
    pub name: String,
    pub title: String,
    pub description: String,
    pub selected_index: usize,
    pub actions: Vec<MarketplaceActionKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddMarketplaceOverlayState {
    pub draft: String,
    pub cursor: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigOverlayState {
    InstalledPluginActions(InstalledPluginActionOverlayState),
    PluginInstallActions(PluginInstallOverlayState),
    MarketplaceActions(MarketplaceActionsOverlayState),
    AddMarketplace(AddMarketplaceOverlayState),
    McpDetails(McpDetailsOverlayState),
    McpCallbackUrl(McpCallbackUrlOverlayState),
    McpElicitation(McpElicitationOverlayState),
    McpAuthRedirect(McpAuthRedirectOverlayState),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigState {
    pub active_tab: ConfigTab,
    pub mcp_selected_server_index: usize,
    pub overlay: Option<ConfigOverlayState>,
    pub committed_settings_document: Value,
    pub committed_local_settings_document: Value,
    pub committed_preferences_document: Value,
    pub settings_path: Option<PathBuf>,
    pub local_settings_path: Option<PathBuf>,
    pub preferences_path: Option<PathBuf>,
    pub status_message: Option<String>,
    pub last_error: Option<String>,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            active_tab: ConfigTab::Plugins,
            mcp_selected_server_index: 0,
            overlay: None,
            committed_settings_document: Value::Object(serde_json::Map::new()),
            committed_local_settings_document: Value::Object(serde_json::Map::new()),
            committed_preferences_document: Value::Object(serde_json::Map::new()),
            settings_path: None,
            local_settings_path: None,
            preferences_path: None,
            status_message: None,
            last_error: None,
        }
    }
}

impl ConfigState {
    pub fn fast_mode_effective(&self) -> bool {
        store::fast_mode(&self.committed_settings_document).unwrap_or(false)
    }

    pub fn always_thinking_effective(&self) -> bool {
        store::always_thinking_enabled(&self.committed_settings_document).unwrap_or(false)
    }

    pub fn model_effective(&self) -> Option<String> {
        store::model(&self.committed_settings_document).ok().flatten()
    }

    pub fn thinking_effort_effective(&self) -> EffortLevel {
        // Forge defaults to `max` effort when unset.
        store::thinking_effort_level(&self.committed_settings_document).unwrap_or(EffortLevel::Max)
    }

    pub fn default_permission_mode_effective(&self) -> DefaultPermissionMode {
        // Forge defaults to `Auto` permission mode; the CLI defaults
        // to `default`, so without this override every fresh forge
        // session would ship `permissions.defaultMode = "default"`.
        store::default_permission_mode(&self.committed_settings_document)
            .unwrap_or(DefaultPermissionMode::Auto)
    }

    pub fn respect_gitignore_effective(&self) -> bool {
        store::respect_gitignore(&self.committed_preferences_document).unwrap_or(true)
    }

    pub fn preferred_notification_channel_effective(&self) -> PreferredNotifChannel {
        store::preferred_notification_channel(&self.committed_preferences_document)
            .unwrap_or_default()
    }

    pub fn prefers_reduced_motion_effective(&self) -> bool {
        store::prefers_reduced_motion(&self.committed_local_settings_document).unwrap_or(false)
    }

    pub fn output_style_effective(&self) -> OutputStyle {
        store::output_style(&self.committed_local_settings_document).unwrap_or_default()
    }

    pub fn installed_plugin_actions_overlay(&self) -> Option<&InstalledPluginActionOverlayState> {
        match &self.overlay {
            Some(ConfigOverlayState::InstalledPluginActions(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn installed_plugin_actions_overlay_mut(
        &mut self,
    ) -> Option<&mut InstalledPluginActionOverlayState> {
        match &mut self.overlay {
            Some(ConfigOverlayState::InstalledPluginActions(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn plugin_install_overlay(&self) -> Option<&PluginInstallOverlayState> {
        match &self.overlay {
            Some(ConfigOverlayState::PluginInstallActions(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn plugin_install_overlay_mut(&mut self) -> Option<&mut PluginInstallOverlayState> {
        match &mut self.overlay {
            Some(ConfigOverlayState::PluginInstallActions(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn marketplace_actions_overlay(&self) -> Option<&MarketplaceActionsOverlayState> {
        match &self.overlay {
            Some(ConfigOverlayState::MarketplaceActions(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn marketplace_actions_overlay_mut(
        &mut self,
    ) -> Option<&mut MarketplaceActionsOverlayState> {
        match &mut self.overlay {
            Some(ConfigOverlayState::MarketplaceActions(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn add_marketplace_overlay(&self) -> Option<&AddMarketplaceOverlayState> {
        match &self.overlay {
            Some(ConfigOverlayState::AddMarketplace(overlay)) => Some(overlay),
            _ => None,
        }
    }

    pub fn add_marketplace_overlay_mut(&mut self) -> Option<&mut AddMarketplaceOverlayState> {
        match &mut self.overlay {
            Some(ConfigOverlayState::AddMarketplace(overlay)) => Some(overlay),
            _ => None,
        }
    }

    fn apply_loaded(&mut self, loaded: store::LoadedSettingsDocuments, preserve_status: bool) {
        self.settings_path = Some(loaded.paths.settings);
        self.local_settings_path = Some(loaded.paths.local_settings);
        self.preferences_path = Some(loaded.paths.preferences);
        self.committed_settings_document = loaded.settings_document;
        self.committed_local_settings_document = loaded.local_settings_document;
        self.committed_preferences_document = loaded.preferences_document;
        self.overlay = None;
        self.mcp_selected_server_index = 0;
        if !preserve_status {
            self.status_message = None;
            self.last_error = None;
        }
    }
}

pub fn initialize_shared_state(app: &mut App) -> Result<(), String> {
    let pr = project_root(app);
    let loaded = store::load(
        app.settings_home_override.as_deref(),
        pr.as_path(),
        store_workspace_bridge(app).as_ref().copied(),
    )?;
    app.config.apply_loaded(loaded, false);
    Ok(())
}

pub fn open(app: &mut App) -> Result<(), String> {
    let pr = project_root(app);
    let loaded = store::load(
        app.settings_home_override.as_deref(),
        pr.as_path(),
        store_workspace_bridge(app).as_ref().copied(),
    )?;
    app.config.apply_loaded(loaded, false);
    view::set_active_view(app, ActiveView::Config);
    request_active_tab_side_effects(app);
    Ok(())
}

pub(crate) fn refresh_runtime_tabs_for_session_change(app: &mut App) {
    if app.active_view != ActiveView::Config {
        return;
    }
    if app.config.active_tab == ConfigTab::Plugins {
        crate::app::plugins::request_inventory_refresh_if_needed(app);
    }
}

pub fn close(app: &mut App) {
    view::set_active_view(app, ActiveView::Chat);
}

pub(crate) fn activate_tab(app: &mut App, tab: ConfigTab) {
    app.config.active_tab = tab;
    app.config.status_message = None;
    app.config.last_error = None;
    request_active_tab_side_effects(app);
}

pub fn handle_key(app: &mut App, key: KeyEvent) {
    if is_ctrl_shortcut(key, 'q') || is_ctrl_shortcut(key, 'c') {
        app.should_quit = true;
        return;
    }

    if app.config.overlay.is_some() {
        edit::handle_overlay_key(app, key);
        return;
    }

    eprintln!("DEBUG handle_key: code={:?} mods={:?} active_tab={:?}", key.code, key.modifiers, app.config.active_tab);

    if app.config.active_tab == ConfigTab::Plugins && crate::app::plugins::handle_key(app, key) {
        eprintln!("  -> plugins consumed");
        return;
    }
    if mcp::handle_mcp_key(app, key) {
        eprintln!("  -> mcp consumed");
        return;
    }

    match (key.code, key.modifiers) {
        (KeyCode::Enter | KeyCode::Esc, KeyModifiers::NONE) => {
            close(app);
        }
        (KeyCode::BackTab, _) => {
            eprintln!("  -> BackTab matched");
            activate_tab(app, app.config.active_tab.prev());
            eprintln!("  -> active_tab now {:?}", app.config.active_tab);
        }
        (KeyCode::Tab, KeyModifiers::NONE) => {
            activate_tab(app, app.config.active_tab.next());
        }
        _ => {
            eprintln!("  -> no match");
        }
    }
}

pub fn handle_paste(app: &mut App, text: &str) -> bool {
    if app.config.overlay.is_some() {
        return edit::handle_overlay_paste(app, text);
    }
    if app.config.active_tab == ConfigTab::Plugins {
        return crate::app::plugins::handle_paste(app, text);
    }
    false
}

fn request_active_tab_side_effects(app: &mut App) {
    mcp::refresh_mcp_snapshot_if_needed(app);
    if app.config.active_tab == ConfigTab::Plugins {
        crate::app::plugins::request_inventory_refresh_if_needed(app);
    }
}

fn is_ctrl_shortcut(key: KeyEvent, ch: char) -> bool {
    matches!(key.code, KeyCode::Char(candidate) if candidate == ch)
        && key.modifiers == KeyModifiers::CONTROL
}

/// Build a `WorkspaceBridge` for the active session, or `None`
/// when no workspace / active session is set. Drives the new
/// `store::load` signature.
pub(crate) fn store_workspace_bridge(app: &App) -> Option<store::WorkspaceBridge<'_>> {
    let workspace = app.workspace.as_ref()?;
    let key = app.active_session_key.as_ref()?;
    Some(store::WorkspaceBridge { workspace, key })
}

fn project_root(app: &App) -> std::path::PathBuf {
    std::path::PathBuf::from(app.cwd_raw())
}

const LANGUAGE_MIN_CHARS: usize = 2;
const LANGUAGE_MAX_CHARS: usize = 30;

/// Validate a free-text language string before forwarding it to the
/// session-launch settings payload. Returns a static error message
/// when the value is out of range, otherwise `None` for "looks fine."
pub(crate) fn language_input_validation_message(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let length = trimmed.chars().count();
    if length < LANGUAGE_MIN_CHARS {
        Some("Language must be at least 2 characters.")
    } else if length > LANGUAGE_MAX_CHARS {
        Some("Language must be at most 30 characters.")
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
