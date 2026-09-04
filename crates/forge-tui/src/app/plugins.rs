use forge_workspace::userdata::plugins::cli;

use crate::app::App;
use crate::app::config::{
    AddMarketplaceOverlayState, ConfigOverlayState, InstalledPluginActionKind,
    InstalledPluginActionOverlayState, MarketplaceActionKind, MarketplaceActionsOverlayState,
    PluginInstallActionKind, PluginInstallOverlayState,
};
use crate::app::input::InputState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use forge_workspace::SessionUpdate;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{Instrument, info_span};

const INVENTORY_REFRESH_TTL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PluginsViewTab {
    #[default]
    Installed,
    Plugins,
    Marketplace,
}

impl PluginsViewTab {
    pub const ALL: [Self; 3] = [Self::Installed, Self::Plugins, Self::Marketplace];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Installed => "Installed",
            Self::Plugins => "Plugins",
            Self::Marketplace => "Marketplace",
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Installed => Self::Plugins,
            Self::Plugins => Self::Marketplace,
            Self::Marketplace => Self::Installed,
        }
    }

    pub const fn prev(self) -> Self {
        match self {
            Self::Installed => Self::Marketplace,
            Self::Plugins => Self::Installed,
            Self::Marketplace => Self::Plugins,
        }
    }
}

// Plugin registry types defined in forge_primitives::plugins;
// re-exported here so the existing forge-tui import paths resolve.
pub use forge_primitives::plugins::{
    InstalledPluginEntry, MarketplaceEntry, MarketplaceSourceEntry, PluginCapability,
    PluginRunRowStatus, PluginUpdateRecord, PluginUpdateRun, PluginUpdateRunRow,
    PluginUpdateTrigger, PluginsCliActionSuccess, PluginsInventorySnapshot, classify_update_row,
    update_availability,
};

#[derive(Debug, Clone, Default)]
pub struct PluginsState {
    pub active_tab: PluginsViewTab,
    pub search_focused: bool,
    pub installed_search_query: InputState,
    pub plugins_search_query: InputState,
    pub installed_selected_index: usize,
    pub plugins_selected_index: usize,
    pub marketplace_selected_index: usize,
    pub installed: Vec<InstalledPluginEntry>,
    pub marketplace: Vec<MarketplaceEntry>,
    pub marketplaces: Vec<MarketplaceSourceEntry>,
    pub loading: bool,
    pub status_message: Option<String>,
    pub last_error: Option<String>,
    pub last_inventory_refresh_at: Option<Instant>,
    pub claude_path: Option<PathBuf>,
    pub runtime_reload_after_refresh: bool,
    pub pending_runtime_reload_success_message: Option<String>,
    /// Live or last finished section-level update run / check report.
    pub update_run: Option<PluginUpdateRun>,
    /// Latest recorded update per installed entry, read from the
    /// store; feeds the rollback affordance.
    pub update_records: Vec<PluginUpdateRecord>,
}

impl PluginsState {
    pub fn selected_index_for(&self, tab: PluginsViewTab) -> usize {
        match tab {
            PluginsViewTab::Installed => self.installed_selected_index,
            PluginsViewTab::Plugins => self.plugins_selected_index,
            PluginsViewTab::Marketplace => self.marketplace_selected_index,
        }
    }

    pub fn set_selected_index_for(&mut self, tab: PluginsViewTab, index: usize) {
        match tab {
            PluginsViewTab::Installed => self.installed_selected_index = index,
            PluginsViewTab::Plugins => self.plugins_selected_index = index,
            PluginsViewTab::Marketplace => self.marketplace_selected_index = index,
        }
    }

    pub fn clear_feedback(&mut self) {
        self.status_message = None;
        self.last_error = None;
    }

    pub fn search_query_for(&self, tab: PluginsViewTab) -> String {
        match tab {
            PluginsViewTab::Installed => self.installed_search_query.text(),
            PluginsViewTab::Plugins => self.plugins_search_query.text(),
            PluginsViewTab::Marketplace => String::new(),
        }
    }

    pub fn active_search_query_mut(&mut self) -> Option<&mut InputState> {
        match self.active_tab {
            PluginsViewTab::Installed => Some(&mut self.installed_search_query),
            PluginsViewTab::Plugins => Some(&mut self.plugins_search_query),
            PluginsViewTab::Marketplace => None,
        }
    }
}

pub(crate) fn handle_paste(app: &mut App, text: &str) -> bool {
    if !search_enabled(app.plugins.active_tab) || !app.plugins.search_focused {
        return false;
    }
    let normalized = normalize_single_line_input(text);
    if normalized.is_empty() {
        return false;
    }
    if let Some(query) = app.plugins.active_search_query_mut() {
        query.insert_str(&normalized);
        reset_selection_for_active_tab(app);
        return true;
    }
    false
}

pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    // A live take owns the first Esc on this view: it is abandoned
    // before any closing semantics fire.
    if matches!(key.code, KeyCode::Esc) && crate::app::dictate::abandon_take(app) {
        app.needs_redraw = true;
        return true;
    }
    if matches!(key.code, KeyCode::Esc)
        && app.plugins.update_run.as_ref().is_some_and(|run| run.finished)
    {
        app.plugins.update_run = None;
        app.plugins.status_message = None;
        app.needs_redraw = true;
        return true;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Left, KeyModifiers::NONE) => {
            app.plugins.active_tab = app.plugins.active_tab.prev();
            app.plugins.search_focused = false;
            clamp_selection(app);
            true
        }
        (KeyCode::Right, KeyModifiers::NONE) => {
            app.plugins.active_tab = app.plugins.active_tab.next();
            app.plugins.search_focused = false;
            clamp_selection(app);
            true
        }
        (KeyCode::Up, KeyModifiers::NONE) => {
            if search_enabled(app.plugins.active_tab)
                && !app.plugins.search_focused
                && app.plugins.selected_index_for(app.plugins.active_tab) == 0
            {
                app.plugins.search_focused = true;
            } else if !app.plugins.search_focused {
                move_selection(app, -1);
            }
            true
        }
        (KeyCode::Down, KeyModifiers::NONE) => {
            if app.plugins.search_focused {
                app.plugins.search_focused = false;
            } else {
                move_selection(app, 1);
            }
            true
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            if app.plugins.search_focused {
                false
            } else {
                match app.plugins.active_tab {
                    PluginsViewTab::Installed => open_installed_actions_overlay(app),
                    PluginsViewTab::Plugins => open_plugin_install_overlay(app),
                    PluginsViewTab::Marketplace => open_marketplace_overlay(app),
                }
            }
        }
        (KeyCode::Backspace, KeyModifiers::NONE) => {
            if search_enabled(app.plugins.active_tab)
                && app.plugins.search_focused
                && let Some(query) = app.plugins.active_search_query_mut()
                && query.textarea_delete_char_before()
            {
                reset_selection_for_active_tab(app);
            }
            true
        }
        (KeyCode::Delete, KeyModifiers::NONE) => {
            if search_enabled(app.plugins.active_tab)
                && app.plugins.search_focused
                && let Some(query) = app.plugins.active_search_query_mut()
                && !query.is_empty()
            {
                query.clear();
                reset_selection_for_active_tab(app);
            }
            true
        }
        (KeyCode::Char(ch), modifiers)
            if matches!(ch, 'r' | 'R')
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
                && !app.plugins.search_focused =>
        {
            request_inventory_refresh_manual(app);
            true
        }
        (KeyCode::Char(ch), modifiers)
            if matches!(ch, 'u' | 'U')
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
                && !app.plugins.search_focused =>
        {
            start_update_run(app, PluginUpdateTrigger::Manual);
            true
        }
        (KeyCode::Char(ch), modifiers)
            if matches!(ch, 'c' | 'C')
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
                && !app.plugins.search_focused =>
        {
            start_check_run(app);
            true
        }
        (KeyCode::Char(ch), modifiers)
            if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
        {
            if search_enabled(app.plugins.active_tab)
                && app.plugins.search_focused
                && let Some(query) = app.plugins.active_search_query_mut()
                && !matches!(ch, '\n' | '\r')
            {
                query.insert_char(ch);
                reset_selection_for_active_tab(app);
            }
            true
        }
        _ => false,
    }
}

pub(crate) fn request_inventory_refresh_if_needed(app: &mut App) {
    if app.plugins.loading {
        return;
    }
    if app
        .plugins
        .last_inventory_refresh_at
        .is_some_and(|refreshed_at| refreshed_at.elapsed() < INVENTORY_REFRESH_TTL)
    {
        clamp_selection(app);
        return;
    }
    request_inventory_refresh(app);
}

pub(crate) fn request_inventory_refresh_manual(app: &mut App) {
    app.plugins.runtime_reload_after_refresh = true;
    request_inventory_refresh(app);
}

pub(crate) fn request_inventory_refresh(app: &mut App) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    app.plugins.loading = true;
    app.plugins.clear_feedback();
    app.plugins.status_message = Some("Refreshing plugin inventory...".to_owned());
    app.needs_redraw = true;
    let event_tx = app.update_tx.clone();
    let cwd_context = app.cwd_raw();
    let cwd_raw = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_inventory_refresh",
        cwd = %cwd_raw,
    );
    tokio::task::spawn_local(
        async move {
            match cli::refresh_inventory(cwd_raw, cached_claude_path).await {
                Ok((snapshot, claude_path)) => {
                    let _ = event_tx.send(SessionUpdate::PluginsInventoryUpdated {
                        cwd_raw: cwd_context,
                        snapshot,
                        claude_path,
                    });
                }
                Err(message) => {
                    let _ = event_tx.send(SessionUpdate::PluginsInventoryRefreshFailed {
                        cwd_raw: cwd_context,
                        message,
                    });
                }
            }
        }
        .instrument(span),
    );
}

pub(crate) fn apply_inventory_refresh_success(
    app: &mut App,
    snapshot: PluginsInventorySnapshot,
    claude_path: PathBuf,
) {
    let should_reload_runtime = std::mem::take(&mut app.plugins.runtime_reload_after_refresh);
    app.plugins.installed = snapshot.installed;
    app.plugins.marketplace = snapshot.marketplace;
    app.plugins.marketplaces = snapshot.marketplaces;
    app.plugins.loading = false;
    app.plugins.last_error = None;
    app.plugins.last_inventory_refresh_at = Some(Instant::now());
    app.plugins.claude_path = Some(claude_path);
    refresh_update_records(app);
    clamp_selection(app);
    if should_reload_runtime {
        start_runtime_reload(app, "Plugin inventory refreshed".to_owned());
    } else {
        app.plugins.status_message = Some("Plugin inventory refreshed".to_owned());
        app.config.last_error = None;
        app.config.status_message = Some("Plugin inventory refreshed".to_owned());
    }
}

pub(crate) fn apply_inventory_refresh_failure(app: &mut App, message: String) {
    app.plugins.loading = false;
    app.plugins.runtime_reload_after_refresh = false;
    app.plugins.pending_runtime_reload_success_message = None;
    app.plugins.status_message = None;
    // A check that could not refresh leaves no half-seen report behind.
    if let Some(run) = app.plugins.update_run.as_ref()
        && run.rows.iter().all(|row| row.status == PluginRunRowStatus::Queued)
    {
        app.plugins.update_run = None;
    }
    app.plugins.last_error = Some(message);
}

pub(crate) fn reset_for_session_change(app: &mut App) {
    app.plugins.loading = false;
    app.plugins.status_message = None;
    app.plugins.last_error = None;
    app.plugins.last_inventory_refresh_at = None;
    app.plugins.installed.clear();
    app.plugins.marketplace.clear();
    app.plugins.marketplaces.clear();
    app.plugins.claude_path = None;
    app.plugins.runtime_reload_after_refresh = false;
    app.plugins.pending_runtime_reload_success_message = None;
    app.plugins.update_run = None;
    app.plugins.update_records.clear();
    clamp_selection(app);
}

pub(crate) fn clamp_selection(app: &mut App) {
    let installed_len = filtered_installed(&app.plugins).len();
    let plugin_len = filtered_marketplace_plugins(&app.plugins).len();
    let marketplace_len = marketplace_row_count(&app.plugins);
    app.plugins.installed_selected_index =
        clamp_index(app.plugins.installed_selected_index, installed_len);
    app.plugins.plugins_selected_index =
        clamp_index(app.plugins.plugins_selected_index, plugin_len);
    app.plugins.marketplace_selected_index =
        clamp_index(app.plugins.marketplace_selected_index, marketplace_len);
}

pub(crate) fn filtered_installed(state: &PluginsState) -> Vec<&InstalledPluginEntry> {
    let query = state.search_query_for(PluginsViewTab::Installed);
    state.installed.iter().filter(|entry| installed_entry_matches(entry, &query)).collect()
}

pub(crate) fn ordered_installed<'a>(
    state: &'a PluginsState,
    current_project_raw: &str,
) -> Vec<&'a InstalledPluginEntry> {
    let current_project = normalize_project_path(current_project_raw);
    let mut relevant = Vec::new();
    let mut other = Vec::new();

    for entry in filtered_installed(state) {
        if is_relevant_installed_entry(entry, &current_project) {
            relevant.push(entry);
        } else {
            other.push(entry);
        }
    }

    relevant.extend(other);
    relevant
}

pub(crate) fn relevant_installed_count(state: &PluginsState, current_project_raw: &str) -> usize {
    let current_project = normalize_project_path(current_project_raw);
    filtered_installed(state)
        .into_iter()
        .filter(|entry| is_relevant_installed_entry(entry, &current_project))
        .count()
}

pub(crate) fn filtered_marketplace_plugins(state: &PluginsState) -> Vec<&MarketplaceEntry> {
    let query = state.search_query_for(PluginsViewTab::Plugins);
    state.marketplace.iter().filter(|entry| marketplace_plugin_matches(entry, &query)).collect()
}

pub(crate) fn visible_marketplaces(state: &PluginsState) -> Vec<&MarketplaceSourceEntry> {
    state.marketplaces.iter().collect()
}

pub(crate) fn display_label(raw: &str) -> String {
    let normalized = raw.replace('@', " from ").replace('-', " ");
    let mut result = String::with_capacity(normalized.len());
    let mut capitalize_next = true;

    for ch in normalized.chars() {
        if ch == ' ' {
            capitalize_next = true;
            result.push(ch);
            continue;
        }

        if capitalize_next {
            result.extend(ch.to_uppercase());
            capitalize_next = false;
        } else {
            result.extend(ch.to_lowercase());
        }
    }

    result
}

pub(crate) fn handle_installed_overlay_key(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) => app.config.overlay = None,
        (KeyCode::Up, KeyModifiers::NONE) => move_installed_overlay_selection(app, -1),
        (KeyCode::Down, KeyModifiers::NONE) => move_installed_overlay_selection(app, 1),
        (KeyCode::Enter, KeyModifiers::NONE) => execute_selected_installed_overlay_action(app),
        _ => {}
    }
}

pub(crate) fn handle_plugin_install_overlay_key(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) => app.config.overlay = None,
        (KeyCode::Up, KeyModifiers::NONE) => move_plugin_install_overlay_selection(app, -1),
        (KeyCode::Down, KeyModifiers::NONE) => move_plugin_install_overlay_selection(app, 1),
        (KeyCode::Enter, KeyModifiers::NONE) => execute_selected_plugin_install_action(app),
        _ => {}
    }
}

pub(crate) fn handle_marketplace_overlay_key(app: &mut App, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Esc, KeyModifiers::NONE) => app.config.overlay = None,
        (KeyCode::Up, KeyModifiers::NONE) => move_marketplace_overlay_selection(app, -1),
        (KeyCode::Down, KeyModifiers::NONE) => move_marketplace_overlay_selection(app, 1),
        (KeyCode::Enter, KeyModifiers::NONE) => execute_selected_marketplace_action(app),
        _ => {}
    }
}

pub(crate) fn handle_add_marketplace_overlay_key(app: &mut App, key: KeyEvent) {
    if let (KeyCode::Enter, KeyModifiers::NONE) = (key.code, key.modifiers) {
        confirm_add_marketplace_overlay(app);
        return;
    }
    if let (KeyCode::Esc, KeyModifiers::NONE) = (key.code, key.modifiers) {
        // A live take owns the first Esc here too: abandon, keep the
        // overlay up.
        if crate::app::dictate::abandon_take(app) {
            app.needs_redraw = true;
            return;
        }
        app.config.overlay = None;
        return;
    }
    let Some(overlay) = app.config.add_marketplace_overlay_mut() else {
        return;
    };
    match (key.code, key.modifiers) {
        (KeyCode::Left, KeyModifiers::NONE) => overlay.editor.move_left(),
        (KeyCode::Right, KeyModifiers::NONE) => overlay.editor.move_right(),
        (KeyCode::Home, KeyModifiers::NONE) => {
            let _ = overlay.editor.set_cursor(0, 0);
        }
        (KeyCode::End, KeyModifiers::NONE) => {
            let row = overlay.editor.lines().len().saturating_sub(1);
            let col = overlay.editor.lines().last().map_or(0, |line| line.chars().count());
            let _ = overlay.editor.set_cursor(row, col);
        }
        (KeyCode::Backspace, KeyModifiers::NONE) => overlay.editor.delete_char_before(),
        (KeyCode::Delete, KeyModifiers::NONE) => overlay.editor.delete_char_after(),
        (KeyCode::Char(ch), modifiers)
            if (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
                && !matches!(ch, '\n' | '\r') =>
        {
            overlay.editor.insert_char(ch);
        }
        _ => {}
    }
}

pub(crate) fn handle_add_marketplace_overlay_paste(app: &mut App, text: &str) {
    if let Some(overlay) = app.config.add_marketplace_overlay_mut() {
        overlay.editor.insert_str(&normalize_single_line_input(text));
    }
}

fn open_marketplace_overlay(app: &mut App) -> bool {
    if selected_add_marketplace_row(app) {
        open_add_marketplace_overlay(app)
    } else {
        open_marketplace_actions_overlay(app)
    }
}

fn open_installed_actions_overlay(app: &mut App) -> bool {
    let selected = selected_installed_entry(app).cloned();
    let Some(entry) = selected else {
        return false;
    };

    let title = display_label(&entry.id);
    let description = installed_overlay_description(app, &entry);
    let actions = installed_overlay_actions(app, &entry);
    app.config.overlay =
        Some(ConfigOverlayState::InstalledPluginActions(InstalledPluginActionOverlayState {
            plugin_id: entry.id,
            title,
            description,
            scope: entry.scope,
            project_path: entry.project_path,
            selected_index: 0,
            actions,
        }));
    true
}

fn open_plugin_install_overlay(app: &mut App) -> bool {
    let selected = selected_marketplace_plugin(app).cloned();
    let Some(entry) = selected else {
        return false;
    };

    app.config.overlay =
        Some(ConfigOverlayState::PluginInstallActions(PluginInstallOverlayState {
            plugin_id: entry.plugin_id,
            title: display_label(&entry.name),
            description: entry
                .description
                .unwrap_or_else(|| "Install this plugin into Claude Code.".to_owned()),
            selected_index: 0,
            actions: vec![
                PluginInstallActionKind::User,
                PluginInstallActionKind::Project,
                PluginInstallActionKind::Local,
            ],
        }));
    true
}

fn open_marketplace_actions_overlay(app: &mut App) -> bool {
    let selected = selected_marketplace_source(app).cloned();
    let Some(entry) = selected else {
        return false;
    };

    app.config.overlay =
        Some(ConfigOverlayState::MarketplaceActions(MarketplaceActionsOverlayState {
            name: entry.name.clone(),
            title: display_label(&entry.name),
            description: marketplace_overlay_description(&entry),
            selected_index: 0,
            actions: vec![MarketplaceActionKind::Update, MarketplaceActionKind::Remove],
        }));
    true
}

fn open_add_marketplace_overlay(app: &mut App) -> bool {
    app.config.overlay =
        Some(ConfigOverlayState::AddMarketplace(Box::new(AddMarketplaceOverlayState {
            editor: InputState::new(),
        })));
    app.config.last_error = None;
    true
}

fn move_installed_overlay_selection(app: &mut App, delta: isize) {
    let Some(overlay) = app.config.installed_plugin_actions_overlay_mut() else {
        return;
    };
    let len = overlay.actions.len();
    if len == 0 {
        overlay.selected_index = 0;
        return;
    }
    let current = overlay.selected_index;
    overlay.selected_index = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta.cast_unsigned()).min(len.saturating_sub(1))
    };
}

fn move_plugin_install_overlay_selection(app: &mut App, delta: isize) {
    let Some(overlay) = app.config.plugin_install_overlay_mut() else {
        return;
    };
    let len = overlay.actions.len();
    if len == 0 {
        overlay.selected_index = 0;
        return;
    }
    let current = overlay.selected_index;
    overlay.selected_index = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta.cast_unsigned()).min(len.saturating_sub(1))
    };
}

fn move_marketplace_overlay_selection(app: &mut App, delta: isize) {
    let Some(overlay) = app.config.marketplace_actions_overlay_mut() else {
        return;
    };
    let len = overlay.actions.len();
    if len == 0 {
        overlay.selected_index = 0;
        return;
    }
    let current = overlay.selected_index;
    overlay.selected_index = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta.cast_unsigned()).min(len.saturating_sub(1))
    };
}

fn execute_selected_installed_overlay_action(app: &mut App) {
    let Some(overlay) = app.config.installed_plugin_actions_overlay().cloned() else {
        return;
    };
    let Some(action) = overlay.actions.get(overlay.selected_index).copied() else {
        return;
    };

    if action == InstalledPluginActionKind::Rollback {
        start_rollback(app, overlay.plugin_id.clone(), overlay.scope.clone());
        return;
    }

    let (cwd_raw, args, status_message) = installed_action_command(app, &overlay, action);

    if tokio::runtime::Handle::try_current().is_err() {
        app.config.overlay = None;
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for plugin action".to_owned());
        return;
    }

    app.config.overlay = None;
    app.config.last_error = None;
    app.config.status_message = Some(status_message);
    app.plugins.loading = true;
    app.plugins.last_inventory_refresh_at = None;
    app.needs_redraw = true;
    let event_tx = app.update_tx.clone();
    let cwd_context = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_cli_action_installed",
        cwd = %cwd_raw,
    );
    tokio::task::spawn_local(
        async move {
            match cli::run_cli_command_and_refresh(cwd_raw, cached_claude_path, args).await {
                Ok((snapshot, claude_path)) => {
                    let message =
                        installed_action_success_message(action, &overlay.title, &overlay.scope);
                    let _ = event_tx.send(SessionUpdate::PluginsCliActionSucceeded {
                        cwd_raw: cwd_context,
                        result: PluginsCliActionSuccess { snapshot, message, claude_path },
                    });
                }
                Err(message) => {
                    let _ = event_tx.send(SessionUpdate::PluginsCliActionFailed {
                        cwd_raw: cwd_context,
                        message,
                    });
                }
            }
        }
        .instrument(span),
    );
}

fn execute_selected_plugin_install_action(app: &mut App) {
    let Some(overlay) = app.config.plugin_install_overlay().cloned() else {
        return;
    };
    let Some(action) = overlay.actions.get(overlay.selected_index).copied() else {
        return;
    };

    if tokio::runtime::Handle::try_current().is_err() {
        app.config.overlay = None;
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for plugin action".to_owned());
        return;
    }

    let scope = action.scope();
    let args = vec![
        "plugin".to_owned(),
        "install".to_owned(),
        overlay.plugin_id.clone(),
        "--scope".to_owned(),
        scope.to_owned(),
    ];
    let status_message = match action {
        PluginInstallActionKind::User => format!("Installing {} for user scope...", overlay.title),
        PluginInstallActionKind::Project => {
            format!("Installing {} for project scope...", overlay.title)
        }
        PluginInstallActionKind::Local => {
            format!("Installing {} locally...", overlay.title)
        }
    };

    app.config.overlay = None;
    app.config.last_error = None;
    app.config.status_message = Some(status_message);
    app.plugins.loading = true;
    app.plugins.last_inventory_refresh_at = None;
    app.needs_redraw = true;
    let event_tx = app.update_tx.clone();
    let cwd_raw = app.cwd_raw();
    let cwd_context = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_cli_action_install",
        cwd = %cwd_raw,
    );
    tokio::task::spawn_local(
        async move {
            match cli::run_cli_command_and_refresh(cwd_raw, cached_claude_path, args).await {
                Ok((snapshot, claude_path)) => {
                    let message = plugin_install_success_message(action, &overlay.title);
                    let _ = event_tx.send(SessionUpdate::PluginsCliActionSucceeded {
                        cwd_raw: cwd_context,
                        result: PluginsCliActionSuccess { snapshot, message, claude_path },
                    });
                }
                Err(message) => {
                    let _ = event_tx.send(SessionUpdate::PluginsCliActionFailed {
                        cwd_raw: cwd_context,
                        message,
                    });
                }
            }
        }
        .instrument(span),
    );
}

fn execute_selected_marketplace_action(app: &mut App) {
    let Some(overlay) = app.config.marketplace_actions_overlay().cloned() else {
        return;
    };
    let Some(action) = overlay.actions.get(overlay.selected_index).copied() else {
        return;
    };

    if tokio::runtime::Handle::try_current().is_err() {
        app.config.overlay = None;
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for marketplace action".to_owned());
        return;
    }

    let args = marketplace_action_command(&overlay, action);
    let status_message = marketplace_action_status_message(&overlay.title, action);

    app.config.overlay = None;
    app.config.last_error = None;
    app.config.status_message = Some(status_message);
    app.plugins.loading = true;
    app.plugins.last_inventory_refresh_at = None;
    app.needs_redraw = true;
    let event_tx = app.update_tx.clone();
    let cwd_raw = app.cwd_raw();
    let cwd_context = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_cli_action_marketplace",
        cwd = %cwd_raw,
    );
    tokio::task::spawn_local(
        async move {
            match cli::run_cli_command_and_refresh(cwd_raw, cached_claude_path, args).await {
                Ok((snapshot, claude_path)) => {
                    let message = marketplace_action_success_message(&overlay.title, action);
                    let _ = event_tx.send(SessionUpdate::PluginsCliActionSucceeded {
                        cwd_raw: cwd_context,
                        result: PluginsCliActionSuccess { snapshot, message, claude_path },
                    });
                }
                Err(message) => {
                    let _ = event_tx.send(SessionUpdate::PluginsCliActionFailed {
                        cwd_raw: cwd_context,
                        message,
                    });
                }
            }
        }
        .instrument(span),
    );
}

fn confirm_add_marketplace_overlay(app: &mut App) {
    let Some(overlay) = app.config.add_marketplace_overlay().cloned() else {
        return;
    };
    let source = overlay.editor.text().trim().to_owned();
    if source.is_empty() {
        app.config.last_error = Some("Marketplace source cannot be empty".to_owned());
        app.config.status_message = None;
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        app.config.overlay = None;
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for marketplace action".to_owned());
        return;
    }

    let args = vec![
        "plugin".to_owned(),
        "marketplace".to_owned(),
        "add".to_owned(),
        source.clone(),
        "--scope".to_owned(),
        "user".to_owned(),
    ];

    app.config.overlay = None;
    app.config.last_error = None;
    app.config.status_message = Some(format!("Adding marketplace {source}..."));
    app.plugins.loading = true;
    app.plugins.last_inventory_refresh_at = None;
    app.needs_redraw = true;
    let event_tx = app.update_tx.clone();
    let cwd_raw = app.cwd_raw();
    let cwd_context = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_cli_action_add_marketplace",
        cwd = %cwd_raw,
        source = %source,
    );
    tokio::task::spawn_local(
        async move {
            match cli::run_cli_command_and_refresh(cwd_raw, cached_claude_path, args).await {
                Ok((snapshot, claude_path)) => {
                    let _ = event_tx.send(SessionUpdate::PluginsCliActionSucceeded {
                        cwd_raw: cwd_context,
                        result: PluginsCliActionSuccess {
                            snapshot,
                            message: format!("Added marketplace {source}"),
                            claude_path,
                        },
                    });
                }
                Err(message) => {
                    let _ = event_tx.send(SessionUpdate::PluginsCliActionFailed {
                        cwd_raw: cwd_context,
                        message,
                    });
                }
            }
        }
        .instrument(span),
    );
}

pub(crate) fn apply_cli_action_success(app: &mut App, result: PluginsCliActionSuccess) {
    app.plugins.installed = result.snapshot.installed;
    app.plugins.marketplace = result.snapshot.marketplace;
    app.plugins.marketplaces = result.snapshot.marketplaces;
    app.plugins.last_error = None;
    app.plugins.last_inventory_refresh_at = Some(Instant::now());
    app.plugins.claude_path = Some(result.claude_path);
    refresh_update_records(app);
    clamp_selection(app);
    start_runtime_reload(app, result.message);
}

pub(crate) fn apply_cli_action_failure(app: &mut App, message: String) {
    app.plugins.loading = false;
    app.plugins.pending_runtime_reload_success_message = None;
    app.config.status_message = None;
    app.config.last_error = Some(message);
}

pub(crate) fn apply_runtime_reload_success(app: &mut App) {
    app.plugins.loading = false;
    app.plugins.last_error = None;
    if let Some(message) = app.plugins.pending_runtime_reload_success_message.take() {
        app.plugins.status_message = Some(message.clone());
        app.config.last_error = None;
        app.config.status_message = Some(message);
    }
}

pub(crate) fn apply_runtime_reload_failure(app: &mut App, message: &str) {
    app.plugins.loading = false;
    app.plugins.status_message = None;
    app.plugins.last_error = Some(message.to_owned());
    app.plugins.pending_runtime_reload_success_message = None;
    app.config.status_message = None;
    app.config.last_error = Some(format!("Failed to reload session plugins: {message}"));
}

fn start_runtime_reload(app: &mut App, success_message: String) {
    app.plugins.loading = true;
    app.plugins.status_message = Some("Reloading session plugins...".to_owned());
    app.plugins.last_error = None;
    app.plugins.pending_runtime_reload_success_message = Some(success_message);
    app.config.last_error = None;
    app.config.status_message = Some("Reloading session plugins...".to_owned());
    match crate::app::session_runtime::request_runtime_reload(app) {
        crate::app::session_runtime::RuntimeReloadRequestOutcome::Requested => {}
        crate::app::session_runtime::RuntimeReloadRequestOutcome::Unavailable => {
            apply_runtime_reload_success(app);
        }
        crate::app::session_runtime::RuntimeReloadRequestOutcome::Failed => {
            apply_runtime_reload_failure(app, "failed to request session runtime plugin reload");
        }
    }
}

/// Queue one row per installed entry. With the `Auto` trigger, rows a
/// plugin policy excludes (untrusted marketplace, pinned, or no
/// marketplace) are marked `Skipped` up front instead of queued.
fn build_update_rows(
    app: &App,
    trigger: PluginUpdateTrigger,
    settings: &forge_workspace::PluginSettings,
) -> Vec<PluginUpdateRunRow> {
    let cwd = app.cwd_raw();
    app.plugins
        .installed
        .iter()
        .map(|entry| {
            let mut row = PluginUpdateRunRow::queued(
                entry.id.clone(),
                entry.scope.clone(),
                action_cwd_for(&cwd, &entry.scope, entry.project_path.as_deref()),
                entry.version.clone(),
            );
            if trigger == PluginUpdateTrigger::Auto && !settings.allows_auto_update(&entry.id) {
                row.status = PluginRunRowStatus::Skipped;
                row.detail = Some(skip_reason(settings, &entry.id));
            }
            row
        })
        .collect()
}

fn skip_reason(settings: &forge_workspace::PluginSettings, plugin_id: &str) -> String {
    let marketplace = forge_primitives::plugins::plugin_marketplace(plugin_id);
    if settings.pins.iter().any(|pin| pin == plugin_id) {
        "pinned in forge.toml".to_owned()
    } else if !settings.trusted_marketplaces.iter().any(|t| t == marketplace) {
        format!("marketplace {marketplace} is not trusted for auto-update")
    } else {
        "not eligible for auto-update".to_owned()
    }
}

fn action_cwd_for(app_cwd: &str, scope: &str, project_path: Option<&str>) -> String {
    match scope {
        "local" | "project" => project_path.unwrap_or(app_cwd).to_owned(),
        _ => app_cwd.to_owned(),
    }
}

/// The `u` key: update every installed plugin, one CLI call per entry,
/// reporting per-plugin outcomes in the pane.
pub(crate) fn start_update_run(app: &mut App, trigger: PluginUpdateTrigger) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if app.plugins.loading {
        return;
    }
    let settings = app
        .workspace
        .as_ref()
        .map(|workspace| workspace.plugin_settings().clone())
        .unwrap_or_default();
    let rows = build_update_rows(app, trigger, &settings);
    if rows.iter().all(|row| row.status == PluginRunRowStatus::Skipped) {
        app.plugins.status_message =
            Some("No installed plugins are eligible for update".to_owned());
        return;
    }
    let runnable = rows.iter().filter(|row| row.status == PluginRunRowStatus::Queued).count();
    let run = PluginUpdateRun { trigger, finished: false, rows };
    app.plugins.update_run = Some(run.clone());
    app.plugins.loading = true;
    app.plugins.status_message = Some(format!("Updating {runnable} plugin(s)..."));
    app.plugins.last_error = None;
    app.needs_redraw = true;
    let plan = UpdateRunPlan {
        cwd_context: app.cwd_raw(),
        cached_claude_path: app.plugins.claude_path.clone(),
        marketplaces: app.plugins.marketplaces.clone(),
        run,
    };
    let update_tx = app.update_tx.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_section_update",
        cwd = %plan.cwd_context,
    );
    tokio::task::spawn_local(execute_update_plan(update_tx, plan).instrument(span));
}

/// The `c` key: refresh the inventory and report which installed
/// plugins have a newer marketplace version, without applying anything.
pub(crate) fn start_check_run(app: &mut App) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if app.plugins.loading {
        return;
    }
    app.plugins.loading = true;
    app.plugins.status_message = Some("Checking for plugin updates...".to_owned());
    app.plugins.last_error = None;
    app.needs_redraw = true;
    let update_tx = app.update_tx.clone();
    let cwd_context = app.cwd_raw();
    let cwd_raw = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_update_check",
        cwd = %cwd_raw,
    );
    tokio::task::spawn_local(
        async move {
            match cli::refresh_inventory(cwd_raw, cached_claude_path).await {
                Ok((snapshot, claude_path)) => {
                    let rows = update_availability(&snapshot.installed, &snapshot.marketplace)
                        .into_iter()
                        .map(|availability| PluginUpdateRunRow {
                            plugin_id: availability.plugin_id,
                            scope: availability.scope,
                            cwd_raw: String::new(),
                            marketplace: availability.marketplace,
                            status: PluginRunRowStatus::UpdateAvailable,
                            installed_version: availability.installed_version,
                            available_version: availability.available_version,
                            detail: None,
                        })
                        .collect();
                    let run = PluginUpdateRun {
                        trigger: PluginUpdateTrigger::Manual,
                        finished: true,
                        rows,
                    };
                    let _ = update_tx.send(SessionUpdate::PluginsUpdateRunFinished {
                        cwd_raw: cwd_context,
                        run,
                        snapshot: Some(snapshot),
                        claude_path: Some(claude_path),
                        records: Vec::new(),
                    });
                }
                Err(message) => {
                    let _ = update_tx.send(SessionUpdate::PluginsInventoryRefreshFailed {
                        cwd_raw: cwd_context,
                        message,
                    });
                }
            }
        }
        .instrument(span),
    );
}

/// Everything one update run needs, captured from `App` before the
/// task is spawned.
struct UpdateRunPlan {
    cwd_context: String,
    cached_claude_path: Option<PathBuf>,
    marketplaces: Vec<MarketplaceSourceEntry>,
    run: PluginUpdateRun,
}

/// The marketplace clone HEAD per marketplace name, so updated plugins
/// can record the ref a rollback later restores.
async fn capture_marketplace_refs(
    marketplaces: &[MarketplaceSourceEntry],
) -> HashMap<String, String> {
    let mut refs = HashMap::new();
    for marketplace in marketplaces {
        if let Some(location) = marketplace.install_location.as_deref()
            && let Some(head) = cli::marketplace_head(location.to_owned()).await
        {
            refs.insert(marketplace.name.clone(), head);
        }
    }
    refs
}

async fn execute_update_plan(
    update_tx: mpsc::UnboundedSender<SessionUpdate>,
    mut plan: UpdateRunPlan,
) {
    let refs = capture_marketplace_refs(&plan.marketplaces).await;
    let mut claude_path = plan.cached_claude_path.clone();

    let row_count = plan.run.rows.len();
    for index in 0..row_count {
        if plan.run.rows[index].status != PluginRunRowStatus::Queued {
            continue;
        }
        plan.run.rows[index].status = PluginRunRowStatus::Updating;
        let _ = update_tx.send(SessionUpdate::PluginsUpdateRunProgress {
            cwd_raw: plan.cwd_context.clone(),
            run: plan.run.clone(),
        });
        let row = &plan.run.rows[index];
        let result = cli::run_cli_command(
            row.cwd_raw.clone(),
            claude_path.clone(),
            vec![
                "plugin".to_owned(),
                "update".to_owned(),
                row.plugin_id.clone(),
                "--scope".to_owned(),
                row.scope.clone(),
            ],
        )
        .await;
        match result {
            Ok(path) => claude_path = Some(path),
            Err(message) => {
                plan.run.rows[index].status = PluginRunRowStatus::Failed;
                plan.run.rows[index].detail = Some(message);
            }
        }
    }

    let snapshot = match cli::refresh_inventory(plan.cwd_context.clone(), claude_path.clone()).await
    {
        Ok((snapshot, path)) => {
            claude_path = Some(path);
            Some(snapshot)
        }
        Err(message) => {
            for row in &mut plan.run.rows {
                if row.status == PluginRunRowStatus::Updating {
                    row.status = PluginRunRowStatus::Failed;
                    row.detail = Some(format!("post-update inventory refresh failed: {message}"));
                }
            }
            None
        }
    };

    let mut records = Vec::new();
    for row in &mut plan.run.rows {
        if row.status != PluginRunRowStatus::Updating {
            continue;
        }
        let version_after = snapshot.as_ref().and_then(|snapshot| {
            snapshot
                .installed
                .iter()
                .find(|entry| entry.id == row.plugin_id && entry.scope == row.scope)
                .and_then(|entry| entry.version.as_deref())
        });
        let before = row.installed_version.clone();
        let outcome = classify_update_row(
            &row.plugin_id,
            &row.scope,
            before.as_deref(),
            version_after,
            true,
            "",
        );
        row.status = outcome.status;
        row.installed_version.clone_from(&outcome.installed_version);
        if outcome.status == PluginRunRowStatus::Updated {
            records.push(PluginUpdateRecord {
                plugin_id: row.plugin_id.clone(),
                marketplace: row.marketplace.clone(),
                scope: row.scope.clone(),
                from_version: before,
                to_version: row.installed_version.clone(),
                marketplace_ref_before: refs.get(&row.marketplace).cloned(),
                updated_at: now_rfc3339(),
                trigger: plan.run.trigger,
            });
        }
    }
    plan.run.finished = true;
    let _ = update_tx.send(SessionUpdate::PluginsUpdateRunFinished {
        cwd_raw: plan.cwd_context,
        run: plan.run,
        snapshot,
        claude_path,
        records,
    });
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Boot hook: with `[plugins] auto_update = true`, refresh the
/// inventory and update every eligible plugin before the user has
/// spawned anything. Results surface in the plugins pane whenever it
/// is next opened.
pub fn maybe_spawn_boot_auto_update(
    workspace: &std::sync::Arc<forge_workspace::Workspace>,
    update_tx: mpsc::UnboundedSender<SessionUpdate>,
    cwd_raw: String,
) {
    let settings = workspace.plugin_settings().clone();
    if !settings.auto_update || settings.trusted_marketplaces.is_empty() {
        return;
    }
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_boot_auto_update",
        cwd = %cwd_raw,
    );
    tokio::task::spawn_local(
        async move {
            let Ok((snapshot, claude_path)) = cli::refresh_inventory(cwd_raw.clone(), None).await
            else {
                return;
            };
            let rows: Vec<PluginUpdateRunRow> = snapshot
                .installed
                .iter()
                .map(|entry| {
                    let mut row = PluginUpdateRunRow::queued(
                        entry.id.clone(),
                        entry.scope.clone(),
                        cwd_raw.clone(),
                        entry.version.clone(),
                    );
                    if !settings.allows_auto_update(&entry.id) {
                        row.status = PluginRunRowStatus::Skipped;
                        row.detail = Some(skip_reason(&settings, &entry.id));
                    }
                    row
                })
                .collect();
            if rows.iter().all(|row| row.status == PluginRunRowStatus::Skipped) {
                return;
            }
            let run = PluginUpdateRun { trigger: PluginUpdateTrigger::Auto, finished: false, rows };
            let plan = UpdateRunPlan {
                cwd_context: cwd_raw,
                cached_claude_path: Some(claude_path),
                marketplaces: snapshot.marketplaces.clone(),
                run,
            };
            execute_update_plan(update_tx, plan).await;
        }
        .instrument(span),
    );
}

/// Roll the selected plugin back to its recorded previous version.
pub(crate) fn start_rollback(app: &mut App, plugin_id: String, scope: String) {
    let Some(record) = app
        .plugins
        .update_records
        .iter()
        .find(|record| record.plugin_id == plugin_id && record.scope == scope)
        .cloned()
    else {
        app.plugins.last_error = Some("No recorded previous version for this plugin".to_owned());
        return;
    };
    let install_location = app
        .plugins
        .marketplaces
        .iter()
        .find(|marketplace| marketplace.name == record.marketplace)
        .and_then(|marketplace| marketplace.install_location.clone());
    let Some(install_location) = install_location else {
        app.plugins.last_error =
            Some("Rollback needs a git-backed marketplace clone; none found".to_owned());
        return;
    };
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if app.plugins.loading {
        return;
    }
    let label = display_label(&plugin_id);
    let to_version = record.from_version.clone().unwrap_or_else(|| "previous version".to_owned());
    app.config.overlay = None;
    app.plugins.loading = true;
    app.plugins.status_message = Some(format!("Rolling back {label} to {to_version}..."));
    app.plugins.last_error = None;
    app.needs_redraw = true;
    let update_tx = app.update_tx.clone();
    let cwd_context = app.cwd_raw();
    let cwd_raw = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_rollback",
        cwd = %cwd_raw,
        plugin = %plugin_id,
    );
    tokio::task::spawn_local(
        async move {
            let result = cli::run_plugin_rollback(
                cached_claude_path.clone().unwrap_or_default(),
                cwd_raw.clone(),
                record,
                install_location,
            )
            .await;
            match result {
                Ok(()) => match cli::refresh_inventory(cwd_raw, cached_claude_path).await {
                    Ok((snapshot, claude_path)) => {
                        let _ = update_tx.send(SessionUpdate::PluginsRollbackSucceeded {
                            cwd_raw: cwd_context,
                            plugin_id,
                            scope,
                            message: format!("Rolled back {label} to {to_version}"),
                            snapshot,
                            claude_path,
                        });
                    }
                    Err(message) => {
                        let _ = update_tx.send(SessionUpdate::PluginsRollbackFailed {
                            cwd_raw: cwd_context,
                            plugin_id,
                            message: format!(
                                "rollback ran but the inventory refresh failed: {message}"
                            ),
                        });
                    }
                },
                Err(message) => {
                    let _ = update_tx.send(SessionUpdate::PluginsRollbackFailed {
                        cwd_raw: cwd_context,
                        plugin_id,
                        message,
                    });
                }
            }
        }
        .instrument(span),
    );
}

pub(crate) fn apply_update_run_progress(app: &mut App, run: PluginUpdateRun) {
    app.plugins.update_run = Some(run);
    app.needs_redraw = true;
}

pub(crate) fn apply_update_run_finished(
    app: &mut App,
    run: &PluginUpdateRun,
    snapshot: Option<PluginsInventorySnapshot>,
    claude_path: Option<PathBuf>,
    records: &[PluginUpdateRecord],
) {
    app.plugins.update_run = Some(run.clone());
    if let Some(snapshot) = snapshot {
        app.plugins.installed = snapshot.installed;
        app.plugins.marketplace = snapshot.marketplace;
        app.plugins.marketplaces = snapshot.marketplaces;
        app.plugins.last_inventory_refresh_at = Some(Instant::now());
        clamp_selection(app);
    }
    if let Some(claude_path) = claude_path {
        app.plugins.claude_path = Some(claude_path);
    }
    if let Some(workspace) = app.workspace.clone()
        && !records.is_empty()
    {
        workspace.record_plugin_updates(records);
        refresh_update_records(app);
    }
    app.plugins.loading = false;
    app.needs_redraw = true;
    match run.trigger {
        PluginUpdateTrigger::Auto => {
            app.plugins.status_message =
                Some(format!("Plugin auto-update finished: {}", run.summary()));
        }
        PluginUpdateTrigger::Manual if is_check_run(run) => {
            app.plugins.status_message = Some(format!("Update check: {}", run.summary()));
        }
        PluginUpdateTrigger::Manual => {
            app.plugins.status_message = Some(format!("Update run finished: {}", run.summary()));
        }
    }
}

/// A finished run with only check rows (no queue/update outcomes) is
/// the report-only `c` flow.
fn is_check_run(run: &PluginUpdateRun) -> bool {
    run.rows.iter().all(|row| {
        matches!(
            row.status,
            PluginRunRowStatus::UpdateAvailable
                | PluginRunRowStatus::AlreadyCurrent
                | PluginRunRowStatus::Failed
        )
    })
}

pub(crate) fn apply_rollback_success(
    app: &mut App,
    plugin_id: &str,
    scope: &str,
    message: String,
    snapshot: PluginsInventorySnapshot,
    claude_path: PathBuf,
) {
    app.plugins.installed = snapshot.installed;
    app.plugins.marketplace = snapshot.marketplace;
    app.plugins.marketplaces = snapshot.marketplaces;
    app.plugins.last_inventory_refresh_at = Some(Instant::now());
    app.plugins.claude_path = Some(claude_path);
    if let Some(workspace) = app.workspace.clone() {
        workspace.clear_plugin_update_record(plugin_id, scope);
    }
    refresh_update_records(app);
    clamp_selection(app);
    start_runtime_reload(app, message);
}

pub(crate) fn apply_rollback_failure(app: &mut App, plugin_id: &str, message: &str) {
    app.plugins.loading = false;
    app.plugins.status_message = None;
    app.plugins.last_error =
        Some(format!("Rollback of {} failed: {message}", display_label(plugin_id)));
    app.needs_redraw = true;
}

/// Re-read the update records from the store into the pane's cache.
fn refresh_update_records(app: &mut App) {
    if let Some(workspace) = app.workspace.as_ref() {
        app.plugins.update_records = workspace.plugin_update_records();
    }
}

/// Rollback is offered for the selected entry when forge remembers a
/// previous version for it.
pub(crate) fn has_rollback_record(app: &App, plugin_id: &str, scope: &str) -> bool {
    app.plugins
        .update_records
        .iter()
        .any(|record| record.plugin_id == plugin_id && record.scope == scope)
}

fn installed_action_command(
    app: &App,
    overlay: &InstalledPluginActionOverlayState,
    action: InstalledPluginActionKind,
) -> (String, Vec<String>, String) {
    let cwd_raw = action_cwd(app, overlay);
    let plugin_id = overlay.plugin_id.clone();
    let scope = overlay.scope.clone();
    let action_label = display_label(&plugin_id);
    match action {
        InstalledPluginActionKind::Enable => (
            cwd_raw.clone(),
            vec![
                "plugin".to_owned(),
                "enable".to_owned(),
                plugin_id.clone(),
                "--scope".to_owned(),
                scope.clone(),
            ],
            format!("Enabling {action_label}..."),
        ),
        InstalledPluginActionKind::Disable => (
            cwd_raw.clone(),
            vec![
                "plugin".to_owned(),
                "disable".to_owned(),
                plugin_id.clone(),
                "--scope".to_owned(),
                scope.clone(),
            ],
            format!("Disabling {action_label}..."),
        ),
        InstalledPluginActionKind::Update => (
            cwd_raw.clone(),
            vec![
                "plugin".to_owned(),
                "update".to_owned(),
                plugin_id.clone(),
                "--scope".to_owned(),
                scope.clone(),
            ],
            format!("Updating {action_label}..."),
        ),
        // Rollback dispatches through `start_rollback` before this
        // builder runs; the empty plan is unreachable.
        InstalledPluginActionKind::Rollback => (cwd_raw, Vec::new(), String::new()),
        InstalledPluginActionKind::InstallInCurrentProject => (
            app.cwd_raw(),
            vec![
                "plugin".to_owned(),
                "install".to_owned(),
                plugin_id.clone(),
                "--scope".to_owned(),
                "local".to_owned(),
            ],
            format!("Installing {action_label} in the current project..."),
        ),
        InstalledPluginActionKind::Uninstall => (
            cwd_raw,
            vec![
                "plugin".to_owned(),
                "uninstall".to_owned(),
                plugin_id,
                "--scope".to_owned(),
                scope,
            ],
            format!("Uninstalling {action_label}..."),
        ),
    }
}

fn installed_action_success_message(
    action: InstalledPluginActionKind,
    title: &str,
    scope: &str,
) -> String {
    match action {
        InstalledPluginActionKind::Enable => format!("Enabled {title} in {scope} scope"),
        InstalledPluginActionKind::Disable => format!("Disabled {title} in {scope} scope"),
        InstalledPluginActionKind::Update => format!("Updated {title} in {scope} scope"),
        InstalledPluginActionKind::Rollback => format!("Rolled back {title}"),
        InstalledPluginActionKind::InstallInCurrentProject => {
            format!("Installed {title} in the current project")
        }
        InstalledPluginActionKind::Uninstall => format!("Uninstalled {title} from {scope} scope"),
    }
}

fn plugin_install_success_message(action: PluginInstallActionKind, title: &str) -> String {
    match action {
        PluginInstallActionKind::User => format!("Installed {title} for user scope"),
        PluginInstallActionKind::Project => format!("Installed {title} for project scope"),
        PluginInstallActionKind::Local => format!("Installed {title} locally"),
    }
}

fn marketplace_action_command(
    overlay: &MarketplaceActionsOverlayState,
    action: MarketplaceActionKind,
) -> Vec<String> {
    match action {
        MarketplaceActionKind::Update => vec![
            "plugin".to_owned(),
            "marketplace".to_owned(),
            "update".to_owned(),
            overlay.name.clone(),
        ],
        MarketplaceActionKind::Remove => vec![
            "plugin".to_owned(),
            "marketplace".to_owned(),
            "remove".to_owned(),
            overlay.name.clone(),
        ],
    }
}

fn marketplace_action_status_message(title: &str, action: MarketplaceActionKind) -> String {
    match action {
        MarketplaceActionKind::Update => format!("Updating {title} marketplace..."),
        MarketplaceActionKind::Remove => format!("Removing {title} marketplace..."),
    }
}

fn marketplace_action_success_message(title: &str, action: MarketplaceActionKind) -> String {
    match action {
        MarketplaceActionKind::Update => format!("Updated {title} marketplace"),
        MarketplaceActionKind::Remove => format!("Removed {title} marketplace"),
    }
}

fn action_cwd(app: &App, overlay: &InstalledPluginActionOverlayState) -> String {
    match overlay.scope.as_str() {
        "local" | "project" => overlay.project_path.clone().unwrap_or_else(|| app.cwd_raw()),
        _ => app.cwd_raw(),
    }
}

fn installed_overlay_actions(
    app: &App,
    entry: &InstalledPluginEntry,
) -> Vec<InstalledPluginActionKind> {
    let mut actions = Vec::new();
    match entry.scope.as_str() {
        "user" | "project" | "local" => {
            actions.push(if entry.enabled {
                InstalledPluginActionKind::Disable
            } else {
                InstalledPluginActionKind::Enable
            });
        }
        _ => {}
    }
    actions.push(InstalledPluginActionKind::Update);
    if has_rollback_record(app, &entry.id, &entry.scope) {
        actions.push(InstalledPluginActionKind::Rollback);
    }
    if can_install_in_current_project(app, entry) {
        actions.push(InstalledPluginActionKind::InstallInCurrentProject);
    }
    actions.push(InstalledPluginActionKind::Uninstall);
    actions
}

fn installed_overlay_description(app: &App, entry: &InstalledPluginEntry) -> String {
    if let Some(description) = app
        .plugins
        .marketplace
        .iter()
        .find(|candidate| candidate.plugin_id == entry.id)
        .and_then(|candidate| candidate.description.as_deref())
    {
        return description.to_owned();
    }

    match entry.project_path.as_deref() {
        Some(project_path) => format!("Installed in {} scope for {}.", entry.scope, project_path),
        None => format!("Installed in {} scope.", entry.scope),
    }
}

fn can_install_in_current_project(app: &App, entry: &InstalledPluginEntry) -> bool {
    let current_project = normalize_project_path(&app.cwd_raw());
    let selected_project = entry.project_path.as_deref().map(normalize_project_path);
    if matches!(entry.scope.as_str(), "local" | "project")
        && selected_project.as_deref() == Some(current_project.as_str())
    {
        return false;
    }

    !app.plugins.installed.iter().any(|candidate| {
        candidate.id == entry.id
            && matches!(candidate.scope.as_str(), "local" | "project")
            && candidate.project_path.as_deref().map(normalize_project_path).as_deref()
                == Some(current_project.as_str())
    })
}

fn selected_installed_entry(app: &App) -> Option<&InstalledPluginEntry> {
    ordered_installed(&app.plugins, &app.cwd_raw())
        .get(app.plugins.installed_selected_index)
        .copied()
}

fn selected_marketplace_plugin(app: &App) -> Option<&MarketplaceEntry> {
    filtered_marketplace_plugins(&app.plugins).get(app.plugins.plugins_selected_index).copied()
}

fn selected_marketplace_source(app: &App) -> Option<&MarketplaceSourceEntry> {
    visible_marketplaces(&app.plugins).get(app.plugins.marketplace_selected_index).copied()
}

fn selected_add_marketplace_row(app: &App) -> bool {
    app.plugins.marketplace_selected_index >= visible_marketplaces(&app.plugins).len()
}

fn marketplace_row_count(state: &PluginsState) -> usize {
    state.marketplaces.len().saturating_add(1)
}

fn marketplace_overlay_description(entry: &MarketplaceSourceEntry) -> String {
    let mut parts = Vec::new();
    if let Some(source) = entry.source.as_deref() {
        parts.push(format!("Source: {source}"));
    }
    if let Some(repo) = entry.repo.as_deref() {
        parts.push(format!("Repo: {repo}"));
    }
    if parts.is_empty() {
        "Manage this configured marketplace.".to_owned()
    } else {
        parts.join("\n")
    }
}

fn normalize_project_path(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase()
}

pub(crate) fn normalize_single_line_input(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n").replace('\n', " ")
}

fn reset_selection_for_active_tab(app: &mut App) {
    app.plugins.set_selected_index_for(app.plugins.active_tab, 0);
    clamp_selection(app);
}

fn move_selection(app: &mut App, delta: isize) {
    let tab = app.plugins.active_tab;
    let len = match tab {
        PluginsViewTab::Installed => filtered_installed(&app.plugins).len(),
        PluginsViewTab::Plugins => filtered_marketplace_plugins(&app.plugins).len(),
        PluginsViewTab::Marketplace => marketplace_row_count(&app.plugins),
    };
    if len == 0 {
        app.plugins.set_selected_index_for(tab, 0);
        return;
    }
    let current = app.plugins.selected_index_for(tab);
    let next = if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta.cast_unsigned()).min(len.saturating_sub(1))
    };
    app.plugins.set_selected_index_for(tab, next);
}

fn clamp_index(current: usize, len: usize) -> usize {
    if len == 0 { 0 } else { current.min(len.saturating_sub(1)) }
}

fn installed_entry_matches(entry: &InstalledPluginEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    entry.id.to_ascii_lowercase().contains(&query)
        || entry.scope.to_ascii_lowercase().contains(&query)
        || entry
            .version
            .as_deref()
            .is_some_and(|version| version.to_ascii_lowercase().contains(&query))
}

fn is_relevant_installed_entry(entry: &InstalledPluginEntry, current_project: &str) -> bool {
    match entry.scope.as_str() {
        "user" => true,
        "local" | "project" => entry
            .project_path
            .as_deref()
            .map(normalize_project_path)
            .is_some_and(|project| project == current_project),
        _ => false,
    }
}

fn marketplace_plugin_matches(entry: &MarketplaceEntry, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_ascii_lowercase();
    entry.plugin_id.to_ascii_lowercase().contains(&query)
        || entry.name.to_ascii_lowercase().contains(&query)
        || entry
            .description
            .as_deref()
            .is_some_and(|description| description.to_ascii_lowercase().contains(&query))
        || entry
            .marketplace_name
            .as_deref()
            .is_some_and(|marketplace| marketplace.to_ascii_lowercase().contains(&query))
        || entry
            .version
            .as_deref()
            .is_some_and(|version| version.to_ascii_lowercase().contains(&query))
}

pub(crate) const fn search_enabled(tab: PluginsViewTab) -> bool {
    !matches!(tab, PluginsViewTab::Marketplace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::model;
    use crate::app::events::apply_session_update;
    use forge_workspace::{DictateOutcome, SessionKey};

    fn plugins_view_with_live_take() -> (crate::app::App, SessionKey) {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        app.active_view = crate::app::ActiveView::Plugins;
        app.plugins.active_tab = PluginsViewTab::Installed;
        app.plugins.search_focused = true;
        apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
        );
        (app, key)
    }

    /// A take resolved while a plugins search field is focused lands
    /// its words into that field's query, newlines flattened like a
    /// paste so the field stays one line.
    #[test]
    fn a_take_lands_in_the_focused_search_field() {
        let (mut app, key) = plugins_view_with_live_take();
        apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key: key.clone(),
                generation: 1,
                outcome: DictateOutcome::Landed {
                    text: "retry guard".to_owned(),
                    truncated: false,
                },
            },
        );
        assert_eq!(app.plugins.installed_search_query.text(), "retry guard");
        assert!(app.input().text().is_empty(), "the chat draft keeps nothing");

        apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key,
                generation: 2,
                outcome: DictateOutcome::Landed {
                    text: " alpha\nbeta\r\ngamma\rdelta".to_owned(),
                    truncated: false,
                },
            },
        );
        assert_eq!(
            app.plugins.installed_search_query.text(),
            "retry guard alpha beta gamma delta",
            "dictated newlines flatten instead of entering the one-line query"
        );
    }

    /// Esc on the plugins view abandons the take before any closing
    /// semantics fire.
    #[test]
    fn esc_abandons_a_live_take_before_closing_the_view() {
        let (mut app, _key) = plugins_view_with_live_take();
        if let Some(ws) = app.workspace.as_ref() {
            ws.enable_test_dispatch_intercept();
        }

        assert!(handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(
            app.active_view,
            crate::app::ActiveView::Plugins,
            "the first Esc abandons the take, the view stands"
        );
        let dispatched = app.workspace.as_ref().map(|ws| ws.drain_test_dispatch_buffer());
        let Some(dispatched) = dispatched else { panic!("test_default carries a workspace") };
        assert!(
            dispatched
                .iter()
                .any(|command| matches!(command, forge_workspace::Command::DictateStop { .. })),
            "Esc dispatched the abandon: {dispatched:?}"
        );
    }

    /// A take resolved while the add-marketplace overlay is up lands
    /// its words into the marketplace field, newlines flattened like a
    /// paste so the draft stays one line.
    #[test]
    fn a_take_lands_in_the_marketplace_field() {
        let mut app = app_with_add_marketplace_open();
        app.active_view = crate::app::ActiveView::Plugins;
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
        );

        apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key: key.clone(),
                generation: 1,
                outcome: DictateOutcome::Landed { text: "owner/repo".to_owned(), truncated: false },
            },
        );
        let overlay = app.config.add_marketplace_overlay_mut().expect("overlay still up");
        assert_eq!(overlay.editor.text(), "owner/repo");

        apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key,
                generation: 2,
                outcome: DictateOutcome::Landed {
                    text: " alpha\nbeta\r\ngamma\rdelta".to_owned(),
                    truncated: false,
                },
            },
        );
        let overlay = app.config.add_marketplace_overlay_mut().expect("overlay still up");
        assert_eq!(
            overlay.editor.text(),
            "owner/repo alpha beta gamma delta",
            "dictated newlines flatten instead of splitting the draft"
        );
    }

    /// Esc on the add-marketplace overlay abandons a live take before
    /// the overlay closes.
    #[test]
    fn esc_abandons_a_live_take_before_closing_the_marketplace_overlay() {
        let mut app = app_with_add_marketplace_open();
        app.active_view = crate::app::ActiveView::Plugins;
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key, floor_db: -50.0, generation: 1 },
        );
        if let Some(ws) = app.workspace.as_ref() {
            ws.enable_test_dispatch_intercept();
        }

        handle_add_marketplace_overlay_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );

        assert!(
            app.config.overlay.is_some(),
            "the first Esc abandons the take, the overlay stands"
        );
        let dispatched = app.workspace.as_ref().map(|ws| ws.drain_test_dispatch_buffer());
        let Some(dispatched) = dispatched else { panic!("test_default carries a workspace") };
        assert!(
            dispatched
                .iter()
                .any(|command| matches!(command, forge_workspace::Command::DictateStop { .. })),
            "Esc dispatched the abandon: {dispatched:?}"
        );
    }

    fn query(text: &str) -> InputState {
        let mut editor = InputState::new();
        editor.set_text(text);
        editor
    }

    fn app_with_connection()
    -> (crate::app::App, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>) {
        let mut app = crate::app::App::test_default();
        let rx = app.install_testing_stub();
        app.set_session_id(Some(model::SessionId::new("session-1")));
        (app, rx)
    }

    fn app_with_add_marketplace_open() -> crate::app::App {
        let mut app = crate::app::App::test_default();
        app.config.overlay =
            Some(ConfigOverlayState::AddMarketplace(Box::new(AddMarketplaceOverlayState {
                editor: InputState::new(),
            })));
        app
    }

    fn add_marketplace_field(app: &crate::app::App) -> (String, usize) {
        let overlay = app.config.add_marketplace_overlay().expect("overlay open");
        (overlay.editor.text(), overlay.editor.cursor_char_offset())
    }

    fn press_add_marketplace(app: &mut crate::app::App, code: KeyCode) {
        handle_add_marketplace_overlay_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn add_marketplace_field_edits_mid_string() {
        let mut app = app_with_add_marketplace_open();

        for ch in ['a', 'c'] {
            press_add_marketplace(&mut app, KeyCode::Char(ch));
        }
        press_add_marketplace(&mut app, KeyCode::Left);
        press_add_marketplace(&mut app, KeyCode::Char('b'));
        assert_eq!(
            add_marketplace_field(&app),
            ("abc".to_owned(), 2),
            "a character typed after Left lands mid-string"
        );

        press_add_marketplace(&mut app, KeyCode::Home);
        press_add_marketplace(&mut app, KeyCode::Delete);
        assert_eq!(
            add_marketplace_field(&app),
            ("bc".to_owned(), 0),
            "Delete takes the character under the cursor and leaves the cursor put"
        );

        press_add_marketplace(&mut app, KeyCode::End);
        press_add_marketplace(&mut app, KeyCode::Backspace);
        assert_eq!(
            add_marketplace_field(&app),
            ("b".to_owned(), 1),
            "Backspace takes the character before the cursor"
        );

        press_add_marketplace(&mut app, KeyCode::Right);
        assert_eq!(
            add_marketplace_field(&app),
            ("b".to_owned(), 1),
            "Right stops at the end of the draft"
        );

        press_add_marketplace(&mut app, KeyCode::Home);
        press_add_marketplace(&mut app, KeyCode::Backspace);
        assert_eq!(
            add_marketplace_field(&app),
            ("b".to_owned(), 0),
            "Backspace at the start of the draft is a no-op"
        );
    }

    #[test]
    fn add_marketplace_field_pastes_at_the_cursor_with_newlines_flattened() {
        let mut app = app_with_add_marketplace_open();

        for ch in ['a', 'z'] {
            press_add_marketplace(&mut app, KeyCode::Char(ch));
        }
        press_add_marketplace(&mut app, KeyCode::Left);

        assert!(
            crate::app::config::handle_plugins_paste(&mut app, "b\nc"),
            "the open overlay takes the paste"
        );
        assert_eq!(
            add_marketplace_field(&app),
            ("ab cz".to_owned(), 4),
            "paste lands at the cursor with its newline flattened to a space"
        );
    }

    /// A newline key is rejected, so the draft stays one line and the
    /// cursor offset stays a plain character offset.
    #[test]
    fn add_marketplace_field_rejects_typed_newlines() {
        let mut app = app_with_add_marketplace_open();

        for ch in ['a', 'b'] {
            press_add_marketplace(&mut app, KeyCode::Char(ch));
        }
        for ch in ['\n', '\r'] {
            press_add_marketplace(&mut app, KeyCode::Char(ch));
            assert_eq!(
                add_marketplace_field(&app),
                ("ab".to_owned(), 2),
                "a typed {ch:?} never enters the draft nor moves the cursor"
            );
        }

        press_add_marketplace(&mut app, KeyCode::Char('c'));
        assert_eq!(
            add_marketplace_field(&app),
            ("abc".to_owned(), 3),
            "typing still appends after a rejection"
        );
    }

    fn sample_snapshot() -> PluginsInventorySnapshot {
        PluginsInventorySnapshot {
            installed: vec![InstalledPluginEntry {
                id: "frontend-design@claude-plugins-official".to_owned(),
                version: Some("1.0.0".to_owned()),
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: PluginCapability::Skill,
            }],
            marketplace: vec![],
            marketplaces: vec![],
        }
    }

    #[test]
    fn plugins_tabs_wrap_in_both_directions() {
        assert_eq!(PluginsViewTab::Installed.prev(), PluginsViewTab::Marketplace);
        assert_eq!(PluginsViewTab::Marketplace.next(), PluginsViewTab::Installed);
    }

    #[test]
    fn recent_inventory_snapshot_skips_refresh() {
        let mut app = crate::app::App::test_default();
        app.plugins.active_tab = PluginsViewTab::Installed;
        app.plugins.last_inventory_refresh_at = Some(Instant::now());

        request_inventory_refresh_if_needed(&mut app);

        assert!(!app.plugins.loading);
    }

    #[test]
    fn display_label_normalizes_plugin_and_marketplace_names() {
        assert_eq!(
            display_label("frontend-design@claude-plugins-official"),
            "Frontend Design From Claude Plugins Official"
        );
        assert_eq!(display_label("claude-plugins-official"), "Claude Plugins Official");
    }

    #[test]
    fn filtered_marketplace_plugins_match_on_name_description_and_marketplace() {
        let state = PluginsState {
            plugins_search_query: query("official"),
            marketplace: vec![MarketplaceEntry {
                plugin_id: "frontend-design@claude-plugins-official".to_owned(),
                name: "frontend-design".to_owned(),
                description: Some("Create distinctive interfaces".to_owned()),
                marketplace_name: Some("claude-plugins-official".to_owned()),
                version: Some("1.0.0".to_owned()),
                install_count: Some(42),
                source: None,
            }],
            ..PluginsState::default()
        };

        assert_eq!(filtered_marketplace_plugins(&state).len(), 1);

        // Negative case: a query matching none of the three fields
        // excludes the entry entirely.
        let mut none = state;
        none.plugins_search_query = query("zzz-no-match");
        assert!(
            filtered_marketplace_plugins(&none).is_empty(),
            "a non-matching query filters the row out"
        );
    }

    fn app_with_focused_search(tab: PluginsViewTab) -> crate::app::App {
        let mut app = crate::app::App::test_default();
        app.plugins.active_tab = tab;
        app.plugins.search_focused = true;
        app
    }

    fn press(app: &mut crate::app::App, code: KeyCode) -> bool {
        handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn search_filter_appends_pops_one_and_wipes_on_delete() {
        let mut app = app_with_focused_search(PluginsViewTab::Installed);

        for ch in ['a', 'b', 'c'] {
            let _ = press(&mut app, KeyCode::Char(ch));
        }
        assert_eq!(
            app.plugins.search_query_for(PluginsViewTab::Installed),
            "abc",
            "typing appends to the filter in order"
        );

        let _ = press(&mut app, KeyCode::Backspace);
        assert_eq!(
            app.plugins.search_query_for(PluginsViewTab::Installed),
            "ab",
            "Backspace drops exactly one character off the end"
        );

        let _ = press(&mut app, KeyCode::Delete);
        assert_eq!(
            app.plugins.search_query_for(PluginsViewTab::Installed),
            "",
            "Delete wipes the whole filter rather than one character"
        );
    }

    /// Home and End fall through this view's keymap, so the filter has
    /// no way to reposition where the next character lands. Routing
    /// either into the editor would make this insert mid-string.
    #[test]
    fn search_filter_has_no_reachable_cursor_movement() {
        let mut app = app_with_focused_search(PluginsViewTab::Installed);
        for ch in ['a', 'b'] {
            let _ = press(&mut app, KeyCode::Char(ch));
        }

        assert!(!press(&mut app, KeyCode::Home), "Home is unbound while the filter has focus");
        assert!(!press(&mut app, KeyCode::End), "End is unbound while the filter has focus");

        let _ = press(&mut app, KeyCode::Char('c'));
        assert_eq!(
            app.plugins.search_query_for(PluginsViewTab::Installed),
            "abc",
            "typing after Home/End still appends rather than inserting mid-string"
        );
    }

    /// Paste collapses every newline flavour to a space, and a newline
    /// delivered as a printable key is rejected, so both routes into
    /// the filter agree on one line.
    #[test]
    fn search_filter_rejects_typed_newlines_and_flattens_pasted_ones() {
        let mut app = app_with_focused_search(PluginsViewTab::Installed);

        assert!(handle_paste(&mut app, "a\nb\r\nc\rd"), "a focused filter accepts a paste");
        assert_eq!(
            app.plugins.search_query_for(PluginsViewTab::Installed),
            "a b c d",
            "pasted newlines collapse to spaces"
        );

        for ch in ['\n', '\r'] {
            assert!(press(&mut app, KeyCode::Char(ch)), "a rejected key is still consumed");
            assert_eq!(
                app.plugins.search_query_for(PluginsViewTab::Installed),
                "a b c d",
                "a typed {ch:?} never enters the filter"
            );
        }

        let _ = press(&mut app, KeyCode::Char('e'));
        assert_eq!(
            app.plugins.search_query_for(PluginsViewTab::Installed),
            "a b c de",
            "typing still appends after a rejection"
        );
    }

    #[test]
    fn installed_and_plugins_search_queries_are_independent() {
        let state = PluginsState {
            installed_search_query: query("installed"),
            plugins_search_query: query("plugins"),
            ..PluginsState::default()
        };

        assert_eq!(state.search_query_for(PluginsViewTab::Installed), "installed");
        assert_eq!(state.search_query_for(PluginsViewTab::Plugins), "plugins");
    }

    #[test]
    fn install_in_current_project_is_available_for_other_project_local_install() {
        let mut app = crate::app::App::test_default();
        app.set_cwd_raw("C:\\work\\project-b");
        let entry = InstalledPluginEntry {
            id: "frontend-design@claude-plugins-official".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "local".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: Some("C:\\work\\project-a".to_owned()),
            capability: PluginCapability::Skill,
        };

        assert!(can_install_in_current_project(&app, &entry));
    }

    #[test]
    fn install_in_current_project_is_hidden_when_already_installed_here() {
        let mut app = crate::app::App::test_default();
        app.set_cwd_raw("C:\\work\\project-b");
        app.plugins.installed.push(InstalledPluginEntry {
            id: "frontend-design@claude-plugins-official".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "local".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: Some("C:\\work\\project-b".to_owned()),
            capability: PluginCapability::Skill,
        });
        let entry = InstalledPluginEntry {
            id: "frontend-design@claude-plugins-official".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "local".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: Some("C:\\work\\project-a".to_owned()),
            capability: PluginCapability::Skill,
        };

        assert!(!can_install_in_current_project(&app, &entry));
    }

    #[test]
    fn ordered_installed_puts_current_project_and_user_entries_first() {
        let state = PluginsState {
            installed: vec![
                InstalledPluginEntry {
                    id: "other-local@claude-plugins-official".to_owned(),
                    version: None,
                    scope: "local".to_owned(),
                    enabled: true,
                    installed_at: None,
                    last_updated: None,
                    project_path: Some("C:\\work\\project-a".to_owned()),
                    capability: PluginCapability::Skill,
                },
                InstalledPluginEntry {
                    id: "user-plugin@claude-plugins-official".to_owned(),
                    version: None,
                    scope: "user".to_owned(),
                    enabled: true,
                    installed_at: None,
                    last_updated: None,
                    project_path: None,
                    capability: PluginCapability::Skill,
                },
                InstalledPluginEntry {
                    id: "current-local@claude-plugins-official".to_owned(),
                    version: None,
                    scope: "local".to_owned(),
                    enabled: true,
                    installed_at: None,
                    last_updated: None,
                    project_path: Some("C:\\work\\project-b".to_owned()),
                    capability: PluginCapability::Skill,
                },
            ],
            ..PluginsState::default()
        };

        let ordered = ordered_installed(&state, "C:\\work\\project-b");
        let ordered_ids = ordered.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>();

        assert_eq!(
            ordered_ids,
            vec![
                "user-plugin@claude-plugins-official",
                "current-local@claude-plugins-official",
                "other-local@claude-plugins-official",
            ]
        );
    }

    #[test]
    fn inventory_refresh_success_triggers_runtime_reload_when_requested() {
        let (mut app, mut rx) = app_with_connection();
        app.plugins.runtime_reload_after_refresh = true;

        apply_inventory_refresh_success(
            &mut app,
            sample_snapshot(),
            std::path::PathBuf::from("C:\\tools\\claude.exe"),
        );

        let envelope = rx.try_recv().expect("reload command");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::ReloadPlugins { session_id } if session_id == "session-1"
        ));
        assert!(!app.plugins.runtime_reload_after_refresh);
        assert_eq!(app.config.status_message.as_deref(), Some("Reloading session plugins..."));
        assert_eq!(
            app.plugins.pending_runtime_reload_success_message.as_deref(),
            Some("Plugin inventory refreshed")
        );
    }

    #[test]
    fn cli_action_success_triggers_runtime_reload() {
        let (mut app, mut rx) = app_with_connection();

        apply_cli_action_success(
            &mut app,
            PluginsCliActionSuccess {
                snapshot: sample_snapshot(),
                message: "Updated plugin".to_owned(),
                claude_path: std::path::PathBuf::from("C:\\tools\\claude.exe"),
            },
        );

        let envelope = rx.try_recv().expect("reload command");
        assert!(matches!(
            envelope,
            forge_primitives::AgentCommand::ReloadPlugins { session_id } if session_id == "session-1"
        ));
        assert_eq!(
            app.plugins.pending_runtime_reload_success_message.as_deref(),
            Some("Updated plugin")
        );
    }

    #[test]
    fn runtime_reload_success_applies_pending_success_message() {
        let mut app = App::test_default();
        app.plugins.loading = true;
        app.plugins.pending_runtime_reload_success_message = Some("Updated plugin".to_owned());

        apply_runtime_reload_success(&mut app);

        assert!(!app.plugins.loading);
        assert_eq!(app.config.status_message.as_deref(), Some("Updated plugin"));
        assert!(app.config.last_error.is_none());
        assert!(app.plugins.pending_runtime_reload_success_message.is_none());
    }

    #[test]
    fn runtime_reload_failure_surfaces_visible_error() {
        let mut app = App::test_default();
        app.plugins.loading = true;
        app.plugins.pending_runtime_reload_success_message = Some("Updated plugin".to_owned());

        apply_runtime_reload_failure(&mut app, "boom");

        assert!(!app.plugins.loading);
        assert_eq!(
            app.config.last_error.as_deref(),
            Some("Failed to reload session plugins: boom")
        );
        assert!(app.config.status_message.is_none());
        assert!(app.plugins.pending_runtime_reload_success_message.is_none());
    }

    #[test]
    fn cli_action_success_without_active_session_keeps_success_message() {
        let mut app = App::test_default();

        apply_cli_action_success(
            &mut app,
            PluginsCliActionSuccess {
                snapshot: sample_snapshot(),
                message: "Updated plugin".to_owned(),
                claude_path: std::path::PathBuf::from("C:\\tools\\claude.exe"),
            },
        );

        assert!(!app.plugins.loading);
        assert_eq!(app.config.status_message.as_deref(), Some("Updated plugin"));
        assert!(app.config.last_error.is_none());
        assert!(app.plugins.pending_runtime_reload_success_message.is_none());
    }

    fn seeded_installed(app: &mut App) {
        app.plugins.installed = vec![
            InstalledPluginEntry {
                id: "supabase@claude-plugins-official".to_owned(),
                version: Some("1.0.0".to_owned()),
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: PluginCapability::Skill,
            },
            InstalledPluginEntry {
                id: "pensive@claude-night-market".to_owned(),
                version: Some("1.7.2".to_owned()),
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: PluginCapability::Skill,
            },
            InstalledPluginEntry {
                id: "leyline@claude-night-market".to_owned(),
                version: Some("0.1.0".to_owned()),
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: PluginCapability::Skill,
            },
        ];
    }

    fn auto_settings() -> forge_workspace::PluginSettings {
        forge_workspace::PluginSettings {
            auto_update: true,
            trusted_marketplaces: vec!["claude-plugins-official".to_owned()],
            pins: vec!["pensive@claude-night-market".to_owned()],
        }
    }

    /// Boot auto-update queues trusted, unpinned plugins and marks the
    /// rest skipped with the reason, so the report shows why nothing
    /// happened to them.
    #[test]
    fn auto_rows_mark_policy_exclusions_skipped() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        let settings = auto_settings();

        let rows = build_update_rows(&app, PluginUpdateTrigger::Auto, &settings);

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].status, PluginRunRowStatus::Queued);
        assert_eq!(rows[1].status, PluginRunRowStatus::Skipped);
        assert_eq!(rows[1].detail.as_deref(), Some("pinned in forge.toml"));
        assert_eq!(rows[2].status, PluginRunRowStatus::Skipped);
        assert_eq!(
            rows[2].detail.as_deref(),
            Some("marketplace claude-night-market is not trusted for auto-update")
        );
    }

    /// The same entries under the manual `u` key queue without policy
    /// filtering: trust gates auto-update only.
    #[test]
    fn manual_rows_queue_everything() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        let settings = auto_settings();

        let rows = build_update_rows(&app, PluginUpdateTrigger::Manual, &settings);

        assert!(rows.iter().all(|row| row.status == PluginRunRowStatus::Queued));
    }

    #[test]
    fn esc_clears_a_finished_run_but_not_a_running_one() {
        let mut app = App::test_default();
        let mut run = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: false,
            rows: vec![PluginUpdateRunRow::queued(
                "supabase@claude-plugins-official".to_owned(),
                "user".to_owned(),
                app.cwd_raw(),
                Some("1.0.0".to_owned()),
            )],
        };
        app.plugins.update_run = Some(run.clone());

        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.plugins.update_run.is_some(), "a running run survives Esc");

        run.finished = true;
        app.plugins.update_run = Some(run);
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.plugins.update_run.is_none(), "a finished report clears on Esc");
    }

    /// A finished run settles the pane: the report stands, the loading
    /// flag drops and the summary names the outcomes. The testing stub
    /// carries no store, so the records land nowhere - persistence is
    /// the store module's own tests.
    #[test]
    fn a_finished_run_replaces_state_and_shows_the_summary() {
        let mut app = App::test_default();
        app.plugins.loading = true;
        let run = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Auto,
            finished: true,
            rows: vec![PluginUpdateRunRow {
                plugin_id: "supabase@claude-plugins-official".to_owned(),
                scope: "user".to_owned(),
                cwd_raw: app.cwd_raw(),
                marketplace: "claude-plugins-official".to_owned(),
                status: PluginRunRowStatus::Updated,
                installed_version: Some("1.1.0".to_owned()),
                available_version: None,
                detail: None,
            }],
        };

        apply_update_run_finished(&mut app, &run, None, None, &[]);

        assert!(!app.plugins.loading);
        assert_eq!(
            app.plugins.update_run.map(|run| run.summary()),
            Some("1 updated, 0 current".to_owned())
        );
        assert!(app.plugins.update_records.is_empty());
    }

    #[test]
    fn rollback_is_offered_only_when_a_record_exists() {
        let mut app = App::test_default();
        let entry = InstalledPluginEntry {
            id: "pensive@claude-night-market".to_owned(),
            version: Some("1.7.2".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        };
        assert!(
            !has_rollback_record(&app, &entry.id, &entry.scope),
            "no record, no rollback action"
        );

        app.plugins.update_records = vec![PluginUpdateRecord {
            plugin_id: entry.id.clone(),
            marketplace: "claude-night-market".to_owned(),
            scope: "user".to_owned(),
            from_version: Some("1.7.1".to_owned()),
            to_version: Some("1.7.2".to_owned()),
            marketplace_ref_before: Some("def456".to_owned()),
            updated_at: "2026-09-04T06:00:00Z".to_owned(),
            trigger: PluginUpdateTrigger::Manual,
        }];
        let actions = installed_overlay_actions(&app, &entry);
        assert!(
            actions.contains(&InstalledPluginActionKind::Rollback),
            "the overlay offers rollback with a record: {actions:?}"
        );
    }
}
