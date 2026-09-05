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
use forge_workspace::userdata::plugins::cli::PluginRollbackOutcome;
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
    PluginRunRowStatus, PluginUpdateAvailability, PluginUpdateRecord, PluginUpdateRun,
    PluginUpdateRunRow, PluginUpdateTrigger, PluginsCliActionSuccess, PluginsInventorySnapshot,
    classify_update_row, update_availability,
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
    /// Out-of-date entries in the current inventory; recomputed on
    /// every refresh so the row markers stay truthful.
    pub update_availability: Vec<PluginUpdateAvailability>,
    /// Test seam: the per-run CLI surface a run uses. `None` means
    /// the real `claude` subprocess calls.
    pub(crate) update_cli: Option<UpdateCli>,
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
        && search_enabled(app.plugins.active_tab)
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
        (KeyCode::Enter, _) if app.plugins.search_focused => {
            // Enter reaches the filter in several flavours, from the
            // \r newline to chords and paste-attached modifiers; none
            // of them may fall through to the view closer.
            true
        }
        (KeyCode::Enter, KeyModifiers::NONE) => match app.plugins.active_tab {
            PluginsViewTab::Installed => open_installed_actions_overlay(app),
            PluginsViewTab::Plugins => open_plugin_install_overlay(app),
            PluginsViewTab::Marketplace => open_marketplace_overlay(app),
        },
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
                && !app.plugins.search_focused
                && search_enabled(app.plugins.active_tab) =>
        {
            start_update_run(app, PluginUpdateTrigger::Manual);
            true
        }
        (KeyCode::Char(ch), modifiers)
            if matches!(ch, 'c' | 'C')
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
                && !app.plugins.search_focused
                && search_enabled(app.plugins.active_tab) =>
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
                        trigger: PluginUpdateTrigger::Manual,
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
    app.plugins.update_availability =
        update_availability(&app.plugins.installed, &app.plugins.marketplace);
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
    app.plugins.last_error = Some(message.clone());
    // The pane's footer renders the config feedback pair, so the
    // failure mirrors there like every sibling failure handler.
    app.config.status_message = None;
    app.config.last_error = Some(message);
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
    app.plugins.update_availability.clear();
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

/// The out-of-date marker a finished check left for one installed
/// entry, if any.
pub(crate) fn availability_for<'a>(
    state: &'a PluginsState,
    plugin_id: &str,
    scope: &str,
) -> Option<&'a PluginUpdateAvailability> {
    state
        .update_availability
        .iter()
        .find(|availability| availability.plugin_id == plugin_id && availability.scope == scope)
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
    app.plugins.update_availability =
        update_availability(&app.plugins.installed, &app.plugins.marketplace);
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

/// Queue one row per installed entry. With the `Auto` trigger, rows
/// whose plugin id carries no marketplace are marked `Skipped` up
/// front instead of queued - there is nothing to update them from.
/// Shared by the manual `u` run and boot auto-update so both shape
/// rows - including each entry's working directory - identically.
fn build_rows_from_entries(
    entries: &[InstalledPluginEntry],
    base_cwd: &str,
    trigger: PluginUpdateTrigger,
) -> Vec<PluginUpdateRunRow> {
    entries
        .iter()
        .map(|entry| {
            let mut row = PluginUpdateRunRow::queued(
                entry.id.clone(),
                entry.scope.clone(),
                action_cwd_for(base_cwd, &entry.scope, entry.project_path.as_deref()),
                entry.version.clone(),
            );
            if trigger == PluginUpdateTrigger::Auto
                && forge_primitives::plugins::plugin_marketplace(&entry.id).is_empty()
            {
                row.status = PluginRunRowStatus::Skipped;
                row.detail = Some("plugin id carries no marketplace".to_owned());
            }
            row
        })
        .collect()
}

fn build_update_rows(app: &App, trigger: PluginUpdateTrigger) -> Vec<PluginUpdateRunRow> {
    let cwd = app.cwd_raw();
    build_rows_from_entries(&app.plugins.installed, &cwd, trigger)
}

fn action_cwd_for(app_cwd: &str, scope: &str, project_path: Option<&str>) -> String {
    match scope {
        "local" | "project" => project_path.unwrap_or(app_cwd).to_owned(),
        _ => app_cwd.to_owned(),
    }
}

/// One future handed back by the [`UpdateCli`] seams.
type UpdateCliFut<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T>>>;
/// The refresh seam's result: the inventory plus a resolved claude path.
type RefreshResult = Result<(PluginsInventorySnapshot, PathBuf), String>;
/// The update seam's result: the resolved claude path plus the CLI's
/// combined stdout+stderr.
type UpdateResult = Result<(PathBuf, String), String>;
type SharedUpdateFn =
    std::sync::Arc<dyn Fn(Option<PathBuf>, String, Vec<String>) -> UpdateCliFut<UpdateResult>>;
type SharedRefreshFn =
    std::sync::Arc<dyn Fn(Option<PathBuf>, String) -> UpdateCliFut<RefreshResult>>;
type RollbackResult = Result<PluginRollbackOutcome, String>;
type SharedRollbackFn = std::sync::Arc<
    dyn Fn(Option<PathBuf>, String, PluginUpdateRecord, String) -> UpdateCliFut<RollbackResult>,
>;

/// Per-run CLI surface, injectable so tests drive a whole run without
/// shelling out. The production instance wraps the `claude` subprocess
/// calls.
#[derive(Clone)]
pub(crate) struct UpdateCli {
    run_update: SharedUpdateFn,
    refresh: SharedRefreshFn,
    rollback: SharedRollbackFn,
}

impl UpdateCli {
    pub(crate) fn real() -> Self {
        Self {
            run_update: std::sync::Arc::new(|cached, cwd, args| {
                Box::pin(cli::run_cli_command(cwd, cached, args))
            }),
            refresh: std::sync::Arc::new(|cached, cwd| {
                Box::pin(cli::refresh_inventory(cwd, cached))
            }),
            rollback: std::sync::Arc::new(|cached, cwd, record, install_location| {
                Box::pin(cli::run_plugin_rollback(cached, cwd, record, install_location))
            }),
        }
    }
}

impl std::fmt::Debug for UpdateCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateCli").finish_non_exhaustive()
    }
}

/// Upper bound on one `claude plugin` call inside a run (updates,
/// refresh, rollback); a hung CLI fails its row instead of pinning
/// the pane's loading flag forever. Expiry abandons the call without
/// killing the child: a subprocess that completes after the timeout
/// still applies its change on disk, its row already reads failed.
const UPDATE_CALL_TIMEOUT: Duration = Duration::from_secs(180);

/// The `u` key: update every installed plugin, one CLI call per entry,
/// reporting per-plugin outcomes in the pane.
pub(crate) fn start_update_run(app: &mut App, trigger: PluginUpdateTrigger) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    if app.plugins.loading || app.plugins.update_run.as_ref().is_some_and(|run| !run.finished) {
        return;
    }
    let rows = build_update_rows(app, trigger);
    if rows.is_empty() {
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
        claude_path: app.plugins.claude_path.clone(),
        marketplaces: app.plugins.marketplaces.clone(),
        run,
        cli: app.plugins.update_cli.clone().unwrap_or_else(UpdateCli::real),
        store: app.workspace.clone(),
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
    if app.plugins.loading || app.plugins.update_run.as_ref().is_some_and(|run| !run.finished) {
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
    let cli = app.plugins.update_cli.clone().unwrap_or_else(UpdateCli::real);
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_update_check",
        cwd = %cwd_raw,
    );
    tokio::task::spawn_local(
        async move {
            let refresh = (cli.refresh)(cached_claude_path, cwd_raw);
            match refresh.await {
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
                    });
                }
                Err(message) => {
                    let _ = update_tx.send(SessionUpdate::PluginsInventoryRefreshFailed {
                        cwd_raw: cwd_context,
                        message,
                        trigger: PluginUpdateTrigger::Manual,
                    });
                }
            }
        }
        .instrument(span),
    );
}

/// Everything one update run needs, captured before the task is
/// spawned. `store` is where applied-update records persist - written
/// here in the task, not in the event handler, so a dropped or
/// mismatched event still costs the record nothing.
struct UpdateRunPlan {
    cwd_context: String,
    /// A claude path resolved earlier in this process (the pane's last
    /// CLI action, or the boot refresh) so the run skips re-resolving.
    claude_path: Option<PathBuf>,
    marketplaces: Vec<MarketplaceSourceEntry>,
    run: PluginUpdateRun,
    cli: UpdateCli,
    store: Option<std::sync::Arc<forge_workspace::Workspace>>,
}

/// The marketplace clone HEAD per marketplace name, so updated plugins
/// can record the ref a rollback later restores.
async fn capture_marketplace_refs(
    marketplaces: &[MarketplaceSourceEntry],
) -> HashMap<String, String> {
    let mut refs = HashMap::new();
    for marketplace in marketplaces {
        if let Some(location) = marketplace.install_location.as_deref() {
            match cli::marketplace_head(location.to_owned()).await {
                Some(head) => {
                    refs.insert(marketplace.name.clone(), head);
                }
                None => {
                    tracing::warn!(
                        target: crate::logging::targets::APP_CONFIG,
                        marketplace = %marketplace.name,
                        "no git HEAD for a marketplace clone; rollback will not be offered for its plugins updated in this run",
                    );
                }
            }
        }
    }
    refs
}

async fn execute_update_plan(
    update_tx: mpsc::UnboundedSender<SessionUpdate>,
    mut plan: UpdateRunPlan,
) {
    let refs = capture_marketplace_refs(&plan.marketplaces).await;
    let mut claude_path = plan.claude_path.clone();

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
        let call = (plan.cli.run_update)(
            claude_path.clone(),
            row.cwd_raw.clone(),
            cli::plugin_update_args(&row.plugin_id, &row.scope),
        );
        let result = match tokio::time::timeout(UPDATE_CALL_TIMEOUT, call).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "`claude plugin update` timed out after {}s",
                UPDATE_CALL_TIMEOUT.as_secs()
            )),
        };
        match result {
            Ok((path, output)) => {
                claude_path = Some(path);
                plan.run.rows[index].detail = Some(output);
            }
            Err(message) => {
                plan.run.rows[index].status = PluginRunRowStatus::Failed;
                plan.run.rows[index].detail = Some(message);
            }
        }
    }

    let refresh = (plan.cli.refresh)(claude_path.clone(), plan.cwd_context.clone());
    let snapshot = match tokio::time::timeout(UPDATE_CALL_TIMEOUT, refresh).await {
        Ok(Ok((snapshot, path))) => {
            claude_path = Some(path);
            Some(snapshot)
        }
        Ok(Err(message)) => {
            for row in &mut plan.run.rows {
                if row.status == PluginRunRowStatus::Updating {
                    row.status = PluginRunRowStatus::Failed;
                    // The captured CLI output is the only evidence the
                    // update may have applied; keep it on the row.
                    let output = row.detail.take().unwrap_or_default();
                    row.detail = Some(if output.is_empty() {
                        format!("post-update inventory refresh failed: {message}")
                    } else {
                        format!("{output} | post-update inventory refresh failed: {message}")
                    });
                }
            }
            None
        }
        Err(_) => {
            for row in &mut plan.run.rows {
                if row.status == PluginRunRowStatus::Updating {
                    row.status = PluginRunRowStatus::Failed;
                    let output = row.detail.take().unwrap_or_default();
                    row.detail = Some(if output.is_empty() {
                        format!(
                            "post-update inventory refresh timed out after {}s",
                            UPDATE_CALL_TIMEOUT.as_secs()
                        )
                    } else {
                        format!(
                            "{output} | post-update inventory refresh timed out after {}s",
                            UPDATE_CALL_TIMEOUT.as_secs()
                        )
                    });
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
        let Some(version_after) = version_after else {
            // An entry that vanished from the inventory has no
            // observable outcome and must not yield a rollback record
            // naming a version nobody can see.
            row.status = PluginRunRowStatus::Failed;
            row.detail = Some("not found in post-update inventory".to_owned());
            continue;
        };
        let before = row.installed_version.clone();
        let output = row.detail.take().unwrap_or_default();
        let outcome = classify_update_row(
            &row.plugin_id,
            &row.scope,
            before.as_deref(),
            Some(version_after),
            &output,
        );
        row.status = outcome.status;
        row.installed_version.clone_from(&outcome.installed_version);
        row.detail = outcome.detail;
        if outcome.status == PluginRunRowStatus::Updated {
            records.push(PluginUpdateRecord {
                plugin_id: row.plugin_id.clone(),
                marketplace: row.marketplace.clone(),
                scope: row.scope.clone(),
                cwd_raw: row.cwd_raw.clone(),
                from_version: before,
                to_version: row.installed_version.clone(),
                marketplace_ref_before: refs.get(&row.marketplace).cloned(),
                updated_at: now_rfc3339(),
                trigger: plan.run.trigger,
            });
        }
    }

    // Persist in the task: the report event can be dropped on a cwd
    // mismatch, the record must not be.
    if !records.is_empty()
        && let Some(store) = plan.store.as_ref()
    {
        store.record_plugin_updates(&records);
    }

    plan.run.finished = true;
    let _ = update_tx.send(SessionUpdate::PluginsUpdateRunFinished {
        cwd_raw: plan.cwd_context,
        run: plan.run,
        snapshot,
        claude_path,
    });
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Boot hook: with `[plugins] auto_update = true`, refresh the
/// inventory and update every eligible plugin before the user has
/// spawned anything. The run is seeded into the pane synchronously so
/// a manual `u`/`c` cannot start a second run while boot is flying.
pub(crate) fn maybe_spawn_boot_auto_update(
    workspace: &std::sync::Arc<forge_workspace::Workspace>,
    app: &mut App,
    cwd_raw: String,
    settings: &forge_workspace::PluginSettings,
    cli: UpdateCli,
) {
    if !settings.auto_update {
        return;
    }
    if cwd_raw.is_empty() {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            "boot auto-update skipped: forge launched with no project cwd for the plugin CLI",
        );
        return;
    }
    app.plugins.update_run =
        Some(PluginUpdateRun { trigger: PluginUpdateTrigger::Auto, finished: false, rows: vec![] });
    app.plugins.loading = true;
    let update_tx = app.update_tx.clone();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_boot_auto_update",
        cwd = %cwd_raw,
    );
    let store = workspace.clone();
    tokio::task::spawn_local(
        async move {
            let refresh = (cli.refresh)(None, cwd_raw.clone());
            let (snapshot, claude_path) = match refresh.await {
                Ok(ok) => ok,
                Err(message) => {
                    // The pane's empty seeded run is cleared by the
                    // failed refresh event; nothing durable was
                    // attempted. Boot failures must not be silent:
                    // nothing else surfaces them.
                    tracing::warn!(
                        target: crate::logging::targets::APP_CONFIG,
                        error = %message,
                        "boot plugin auto-update could not refresh the plugin inventory",
                    );
                    let _ = update_tx.send(SessionUpdate::PluginsInventoryRefreshFailed {
                        cwd_raw,
                        message,
                        trigger: PluginUpdateTrigger::Auto,
                    });
                    return;
                }
            };
            let rows =
                build_rows_from_entries(&snapshot.installed, &cwd_raw, PluginUpdateTrigger::Auto);
            let plan = UpdateRunPlan {
                cwd_context: cwd_raw,
                claude_path: Some(claude_path),
                marketplaces: snapshot.marketplaces.clone(),
                run: PluginUpdateRun { trigger: PluginUpdateTrigger::Auto, finished: false, rows },
                cli,
                store: Some(store),
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
    if record.marketplace_ref_before.is_none() {
        app.plugins.last_error =
            Some("No pre-update marketplace ref was captured; rollback is unavailable".to_owned());
        return;
    }
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
    // A project/local entry updates from its own project; the rollback
    // and its verification must run there too, or they would inspect
    // the wrong install.
    let cwd_raw = if record.cwd_raw.is_empty() { app.cwd_raw() } else { record.cwd_raw.clone() };
    let cached_claude_path = app.plugins.claude_path.clone();
    let cli = app.plugins.update_cli.clone().unwrap_or_else(UpdateCli::real);
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_rollback",
        cwd = %cwd_raw,
        plugin = %plugin_id,
    );
    tokio::task::spawn_local(
        async move {
            let rollback = match tokio::time::timeout(
                UPDATE_CALL_TIMEOUT,
                (cli.rollback)(
                    cached_claude_path.clone(),
                    cwd_raw.clone(),
                    record.clone(),
                    install_location,
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "`claude plugin rollback` timed out after {}s",
                    UPDATE_CALL_TIMEOUT.as_secs()
                )),
            };
            // A rollback that claims success is verified against the
            // refreshed inventory: the old manifest only restores the
            // recorded version if it actually pins one. Unverified
            // rollbacks keep the record so the attempt can be retried.
            let verified = match &rollback {
                Ok(_) => {
                    match tokio::time::timeout(
                        UPDATE_CALL_TIMEOUT,
                        (cli.refresh)(cached_claude_path, cwd_raw),
                    )
                    .await
                    {
                        Ok(Ok((snapshot, claude_path))) => {
                            let post_version = snapshot
                                .installed
                                .iter()
                                .find(|entry| {
                                    entry.id == record.plugin_id && entry.scope == record.scope
                                })
                                .and_then(|entry| entry.version.clone());
                            let verified =
                                post_version.is_some() && post_version == record.from_version;
                            Ok((verified, snapshot, claude_path, post_version))
                        }
                        Ok(Err(message)) => Err(format!("the inventory refresh failed: {message}")),
                        Err(_) => Err(format!(
                            "the verification refresh timed out after {}s",
                            UPDATE_CALL_TIMEOUT.as_secs()
                        )),
                    }
                }
                Err(message) => Err(message.clone()),
            };
            match verified {
                Ok((true, snapshot, claude_path, _)) => {
                    let message = match rollback {
                        Ok(PluginRollbackOutcome::RolledBack) => {
                            format!("Rolled back {label} to {to_version}")
                        }
                        Ok(PluginRollbackOutcome::RolledBackCloneParked(_)) => {
                            format!(
                                "Rolled back {label} to {to_version}; the marketplace clone is \
                                 still parked - run `claude plugin marketplace update {}`",
                                record.marketplace
                            )
                        }
                        Err(message) => {
                            let _ = update_tx.send(SessionUpdate::PluginsRollbackFailed {
                                cwd_raw: cwd_context,
                                plugin_id,
                                message,
                                snapshot: Some(snapshot),
                            });
                            return;
                        }
                    };
                    let _ = update_tx.send(SessionUpdate::PluginsRollbackSucceeded {
                        cwd_raw: cwd_context,
                        plugin_id,
                        scope,
                        message,
                        snapshot,
                        claude_path,
                    });
                }
                Ok((false, snapshot, _, post_version)) => {
                    let still = post_version.unwrap_or_else(|| "unknown".to_owned());
                    let _ = update_tx.send(SessionUpdate::PluginsRollbackFailed {
                        cwd_raw: cwd_context,
                        plugin_id,
                        message: format!(
                            "the version did not move to {to_version} (still at {still}); \
                             the previous-version record is kept"
                        ),
                        snapshot: Some(snapshot),
                    });
                }
                Err(message) => {
                    let _ = update_tx.send(SessionUpdate::PluginsRollbackFailed {
                        cwd_raw: cwd_context,
                        plugin_id,
                        message,
                        snapshot: None,
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

/// A finished check's rows ARE the out-of-date set. For any other
/// finished run the post-run snapshot recomputes them; when that
/// refresh failed the versions are unknown, so the markers drop
/// rather than lie.
fn apply_check_markers(
    app: &mut App,
    run: &PluginUpdateRun,
    snapshot: Option<&PluginsInventorySnapshot>,
) {
    if is_check_run(run) {
        app.plugins.update_availability = run
            .rows
            .iter()
            .filter(|row| row.status == PluginRunRowStatus::UpdateAvailable)
            .map(|row| PluginUpdateAvailability {
                plugin_id: row.plugin_id.clone(),
                scope: row.scope.clone(),
                marketplace: row.marketplace.clone(),
                installed_version: row.installed_version.clone(),
                available_version: row.available_version.clone(),
            })
            .collect();
    } else if let Some(snapshot) = snapshot {
        app.plugins.update_availability =
            update_availability(&snapshot.installed, &snapshot.marketplace);
    } else {
        app.plugins.update_availability.clear();
    }
}

pub(crate) fn apply_update_run_finished(
    app: &mut App,
    run: &PluginUpdateRun,
    snapshot: Option<PluginsInventorySnapshot>,
    claude_path: Option<PathBuf>,
) {
    app.plugins.update_run = Some(run.clone());
    apply_check_markers(app, run, snapshot.as_ref());
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
    refresh_update_records(app);
    app.plugins.loading = false;
    app.needs_redraw = true;
    let applied = run.rows.iter().any(|row| row.status == PluginRunRowStatus::Updated);
    let summary = match run.trigger {
        // Boot runs never reload runtimes: no session may exist yet,
        // and the ones that spawn afterwards pick the new plugins up.
        PluginUpdateTrigger::Auto => {
            Some(format!("Plugin auto-update finished: {}", run.summary()))
        }
        PluginUpdateTrigger::Manual if applied => {
            start_runtime_reload(app, format!("Update run finished: {}", run.summary()));
            None
        }
        PluginUpdateTrigger::Manual if is_check_run(run) => {
            Some(format!("Update check: {}", run.summary()))
        }
        PluginUpdateTrigger::Manual => Some(format!("Update run finished: {}", run.summary())),
    };
    // Arms that skip the runtime reload sync the footer pair
    // themselves, so a mirrored failure does not outlive a successful
    // run.
    if let Some(message) = summary {
        app.plugins.status_message = Some(message.clone());
        app.config.last_error = None;
        app.config.status_message = Some(message);
    }
}

/// A finished run made only of check rows is the report-only `c`
/// flow; an update run whose plugins all failed is not a check. An
/// empty-rows run classifies as a check - the nothing-found `c`
/// result - so its markers read empty rather than going stale.
fn is_check_run(run: &PluginUpdateRun) -> bool {
    run.rows.iter().all(|row| row.status == PluginRunRowStatus::UpdateAvailable)
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
    app.plugins.update_availability =
        update_availability(&app.plugins.installed, &app.plugins.marketplace);
    clamp_selection(app);
    start_runtime_reload(app, message);
}

pub(crate) fn apply_rollback_failure(
    app: &mut App,
    plugin_id: &str,
    message: &str,
    snapshot: Option<PluginsInventorySnapshot>,
) {
    if let Some(snapshot) = snapshot {
        app.plugins.installed = snapshot.installed;
        app.plugins.marketplace = snapshot.marketplace;
        app.plugins.marketplaces = snapshot.marketplaces;
        app.plugins.last_inventory_refresh_at = Some(Instant::now());
        app.plugins.update_availability =
            update_availability(&app.plugins.installed, &app.plugins.marketplace);
        clamp_selection(app);
    }
    app.plugins.loading = false;
    app.plugins.status_message = None;
    app.config.status_message = None;
    let failure = format!("Rollback of {} failed: {message}", display_label(plugin_id));
    app.plugins.last_error = Some(failure.clone());
    app.config.last_error = Some(failure);
    app.needs_redraw = true;
}

/// Re-read the update records from the store into the pane's cache.
fn refresh_update_records(app: &mut App) {
    if let Some(workspace) = app.workspace.as_ref() {
        app.plugins.update_records = workspace.plugin_update_records();
    }
}

/// Rollback is offered for the selected entry when forge remembers a
/// previous version AND captured the marketplace ref the rollback
/// restores; a record without the ref cannot deliver.
pub(crate) fn has_rollback_record(app: &App, plugin_id: &str, scope: &str) -> bool {
    app.plugins.update_records.iter().any(|record| {
        record.plugin_id == plugin_id
            && record.scope == scope
            && record.marketplace_ref_before.is_some()
    })
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

pub(crate) fn reset_selection_for_active_tab(app: &mut App) {
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

    /// A take landing in the search field shrinks the filtered list
    /// like any other query edit, so the row selection resets too.
    #[test]
    fn a_take_landing_in_the_search_field_resets_the_selection() {
        let (mut app, key) = plugins_view_with_live_take();
        app.plugins.installed.clear();
        for id in ["sample-alpha@one", "sample-beta@two", "gamma@three"] {
            app.plugins.installed.push(InstalledPluginEntry {
                id: id.to_owned(),
                version: None,
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: PluginCapability::Skill,
            });
        }
        app.plugins.installed_selected_index = 2;

        apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key,
                generation: 1,
                outcome: DictateOutcome::Landed { text: "sample".to_owned(), truncated: false },
            },
        );

        assert_eq!(app.plugins.installed_search_query.text(), "sample");
        assert_eq!(
            app.plugins.installed_selected_index, 0,
            "the landing filtered the list to two rows; index 2 points past them"
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

    /// Enter reaches a focused filter in several flavours - the \r
    /// newline (Ctrl+M, or any pasted \r with bracketed paste off),
    /// chords, paste-attached modifiers - and none of them may fall
    /// through to the view closer; Esc alone closes.
    #[test]
    fn focused_filter_consumes_enter_and_esc_alone_closes() {
        let mut app = app_with_focused_search(PluginsViewTab::Installed);
        app.active_view = crate::app::ActiveView::Plugins;
        let _ = press(&mut app, KeyCode::Char('a'));

        crate::app::config::handle_plugins_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(
            app.active_view,
            crate::app::ActiveView::Plugins,
            "Enter with the filter focused does not close the view"
        );
        assert_eq!(
            app.plugins.search_query_for(PluginsViewTab::Installed),
            "a",
            "Enter inserts nothing into the filter"
        );

        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::SHIFT] {
            crate::app::config::handle_plugins_key(
                &mut app,
                KeyEvent::new(KeyCode::Enter, modifiers),
            );
            assert_eq!(
                app.active_view,
                crate::app::ActiveView::Plugins,
                "a modified Enter with the filter focused does not close the view"
            );
            assert_eq!(
                app.plugins.search_query_for(PluginsViewTab::Installed),
                "a",
                "a modified Enter inserts nothing into the filter"
            );
        }

        crate::app::config::handle_plugins_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert_eq!(app.active_view, crate::app::ActiveView::Chat, "Esc still closes the view");
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

    fn push_no_marketplace_entry(app: &mut App) {
        app.plugins.installed.push(InstalledPluginEntry {
            id: "scratch-tools".to_owned(),
            version: Some("0.2.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        });
    }

    /// Boot auto-update queues every marketplace-carrying entry and
    /// marks an entry with no marketplace skipped with the reason, so
    /// the report shows why nothing happened to it.
    #[test]
    fn auto_rows_skip_entries_with_no_marketplace() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        push_no_marketplace_entry(&mut app);

        let rows = build_update_rows(&app, PluginUpdateTrigger::Auto);

        assert_eq!(rows.len(), 4);
        assert!(rows[..3].iter().all(|row| row.status == PluginRunRowStatus::Queued));
        assert_eq!(rows[3].status, PluginRunRowStatus::Skipped);
        assert_eq!(rows[3].detail.as_deref(), Some("plugin id carries no marketplace"));
    }

    /// The same entries under the manual `u` key queue everything:
    /// the no-marketplace skip is an auto-update-only affordance.
    #[test]
    fn manual_rows_queue_everything() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        push_no_marketplace_entry(&mut app);

        let rows = build_update_rows(&app, PluginUpdateTrigger::Manual);

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

    /// The check's markers outlive the report (Esc) and die with an
    /// inventory refresh: rows keep naming the delta until the data
    /// underneath changes.
    #[test]
    fn check_markers_outlive_the_report_but_not_the_inventory() {
        let mut app = App::test_default();
        let run = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![PluginUpdateRunRow {
                plugin_id: "supabase@claude-plugins-official".to_owned(),
                scope: "user".to_owned(),
                cwd_raw: String::new(),
                marketplace: "claude-plugins-official".to_owned(),
                status: PluginRunRowStatus::UpdateAvailable,
                installed_version: Some("1.0.0".to_owned()),
                available_version: Some("2.0.0".to_owned()),
                detail: None,
            }],
        };

        apply_update_run_finished(&mut app, &run, None, None);
        assert_eq!(
            availability_for(&app.plugins, "supabase@claude-plugins-official", "user")
                .and_then(|availability| availability.available_version.as_deref()),
            Some("2.0.0"),
            "a finished check leaves the marker"
        );

        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.plugins.update_run.is_none(), "Esc clears the report");
        assert!(
            availability_for(&app.plugins, "supabase@claude-plugins-official", "user").is_some(),
            "the marker survives the report"
        );

        // A refresh that still finds the plugin stale recomputes the
        // marker to the fresh marketplace version - the moved version
        // rules out both a clear and a left-behind marker.
        apply_inventory_refresh_success(
            &mut app,
            PluginsInventorySnapshot {
                installed: vec![InstalledPluginEntry {
                    id: "supabase@claude-plugins-official".to_owned(),
                    version: Some("1.0.0".to_owned()),
                    scope: "user".to_owned(),
                    enabled: true,
                    installed_at: None,
                    last_updated: None,
                    project_path: None,
                    capability: PluginCapability::Skill,
                }],
                marketplace: vec![MarketplaceEntry {
                    plugin_id: "supabase@claude-plugins-official".to_owned(),
                    name: "Supabase".to_owned(),
                    description: None,
                    marketplace_name: Some("claude-plugins-official".to_owned()),
                    version: Some("2.1.0".to_owned()),
                    install_count: None,
                    source: None,
                }],
                marketplaces: Vec::new(),
            },
            PathBuf::new(),
        );
        assert_eq!(
            app.plugins
                .update_availability
                .iter()
                .map(|availability| (
                    availability.plugin_id.as_str(),
                    availability.scope.as_str(),
                    availability.installed_version.as_deref(),
                    availability.available_version.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![("supabase@claude-plugins-official", "user", Some("1.0.0"), Some("2.1.0"))],
            "a refresh recomputes the marker from its snapshot"
        );

        apply_inventory_refresh_success(
            &mut app,
            PluginsInventorySnapshot {
                installed: Vec::new(),
                marketplace: Vec::new(),
                marketplaces: Vec::new(),
            },
            PathBuf::new(),
        );
        assert!(
            app.plugins.update_availability.is_empty(),
            "an empty inventory truthfully yields no markers"
        );
    }

    /// The plugins failure handlers mirror into the config feedback
    /// pair, which is what the pane's footer renders;
    /// plugins.last_error has no reader.
    #[test]
    fn plugin_failures_surface_on_the_footer_pair() {
        let mut app = App::test_default();
        app.config.status_message = Some("stale".to_owned());

        apply_inventory_refresh_failure(&mut app, "refresh blew up".to_owned());
        assert_eq!(app.config.last_error.as_deref(), Some("refresh blew up"));
        assert!(app.config.status_message.is_none(), "the stale status clears");

        app.config.status_message = Some("stale".to_owned());
        apply_rollback_failure(&mut app, "p@market", "boom", None);
        assert_eq!(
            app.config.last_error.as_deref(),
            Some("Rollback of P From Market failed: boom")
        );
        assert!(app.config.status_message.is_none(), "the stale status clears");
    }

    /// After an update run the markers describe the run's post-run
    /// inventory; when that refresh failed the run could not see any
    /// version, so the markers drop rather than name stale deltas.
    #[test]
    fn an_update_run_recomputes_markers_from_its_post_run_inventory() {
        let mut app = App::test_default();
        let stale = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![
                PluginUpdateRunRow {
                    plugin_id: "supabase@claude-plugins-official".to_owned(),
                    scope: "user".to_owned(),
                    cwd_raw: String::new(),
                    marketplace: "claude-plugins-official".to_owned(),
                    status: PluginRunRowStatus::UpdateAvailable,
                    installed_version: Some("1.0.0".to_owned()),
                    available_version: Some("2.0.0".to_owned()),
                    detail: None,
                },
                PluginUpdateRunRow {
                    plugin_id: "pensive@claude-night-market".to_owned(),
                    scope: "user".to_owned(),
                    cwd_raw: String::new(),
                    marketplace: "claude-night-market".to_owned(),
                    status: PluginRunRowStatus::UpdateAvailable,
                    installed_version: Some("1.7.2".to_owned()),
                    available_version: Some("2.0.0".to_owned()),
                    detail: None,
                },
            ],
        };
        apply_update_run_finished(&mut app, &stale, None, None);
        assert_eq!(app.plugins.update_availability.len(), 2, "the check left both markers");

        // The update moved pensive; supabase stayed at 1.0.0.
        let snapshot = PluginsInventorySnapshot {
            installed: vec![
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
                    version: Some("2.0.0".to_owned()),
                    scope: "user".to_owned(),
                    enabled: true,
                    installed_at: None,
                    last_updated: None,
                    project_path: None,
                    capability: PluginCapability::Skill,
                },
            ],
            marketplace: vec![
                MarketplaceEntry {
                    plugin_id: "supabase@claude-plugins-official".to_owned(),
                    name: "Supabase".to_owned(),
                    description: None,
                    marketplace_name: Some("claude-plugins-official".to_owned()),
                    version: Some("2.0.0".to_owned()),
                    install_count: None,
                    source: None,
                },
                MarketplaceEntry {
                    plugin_id: "pensive@claude-night-market".to_owned(),
                    name: "Pensive".to_owned(),
                    description: None,
                    marketplace_name: Some("claude-night-market".to_owned()),
                    version: Some("2.0.0".to_owned()),
                    install_count: None,
                    source: None,
                },
            ],
            marketplaces: Vec::new(),
        };
        let update = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![PluginUpdateRunRow {
                plugin_id: "pensive@claude-night-market".to_owned(),
                scope: "user".to_owned(),
                cwd_raw: app.cwd_raw(),
                marketplace: "claude-night-market".to_owned(),
                status: PluginRunRowStatus::Updated,
                installed_version: Some("2.0.0".to_owned()),
                available_version: None,
                detail: None,
            }],
        };
        apply_update_run_finished(&mut app, &update, Some(snapshot.clone()), None);
        assert_eq!(
            app.plugins
                .update_availability
                .iter()
                .map(|availability| (availability.plugin_id.as_str(), availability.scope.as_str()))
                .collect::<Vec<_>>(),
            vec![("supabase@claude-plugins-official", "user")],
            "markers recompute: the updated plugin drops, the stale one stays"
        );

        apply_update_run_finished(&mut app, &update, None, None);
        assert!(
            app.plugins.update_availability.is_empty(),
            "no post-run snapshot means no truthful markers"
        );
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

        apply_update_run_finished(&mut app, &run, None, None);

        assert!(!app.plugins.loading);
        assert_eq!(
            app.plugins.update_run.map(|run| run.summary()),
            Some("1 updated, 0 failed, 0 current".to_owned())
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
            cwd_raw: String::new(),
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

    /// A record whose pre-update marketplace HEAD was never captured
    /// cannot deliver a rollback and must not offer one.
    #[test]
    fn a_refless_record_never_offers_rollback() {
        let mut app = App::test_default();
        app.plugins.update_records = vec![PluginUpdateRecord {
            plugin_id: "pensive@claude-night-market".to_owned(),
            marketplace: "claude-night-market".to_owned(),
            scope: "user".to_owned(),
            cwd_raw: String::new(),
            from_version: Some("1.7.1".to_owned()),
            to_version: Some("1.7.2".to_owned()),
            marketplace_ref_before: None,
            updated_at: "2026-09-04T06:00:00Z".to_owned(),
            trigger: PluginUpdateTrigger::Manual,
        }];

        assert!(!has_rollback_record(&app, "pensive@claude-night-market", "user"));
    }

    /// Shared fake CLI: records every update call as `cwd:args` and
    /// answers with `output`; the refresh always returns `snapshot`.
    fn fake_cli(
        output: &str,
        snapshot: &PluginsInventorySnapshot,
        calls: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> UpdateCli {
        UpdateCli {
            run_update: {
                let output = output.to_owned();
                let calls = calls.clone();
                std::sync::Arc::new(move |cached, cwd, args| {
                    let output = output.clone();
                    let calls = calls.clone();
                    Box::pin(async move {
                        calls.lock().expect("call log").push(format!("{cwd}:{}", args.join(" ")));
                        Ok((cached.unwrap_or_else(|| std::path::PathBuf::from("claude")), output))
                    })
                })
            },
            refresh: {
                let snapshot = snapshot.clone();
                let calls = calls.clone();
                std::sync::Arc::new(move |_cached, _cwd| {
                    let snapshot = snapshot.clone();
                    let calls = calls.clone();
                    Box::pin(async move {
                        calls.lock().expect("call log").push("refresh".to_owned());
                        Ok((snapshot, std::path::PathBuf::from("claude")))
                    })
                })
            },
            rollback: {
                let calls = calls.clone();
                std::sync::Arc::new(move |_, _, record, _| {
                    let calls = calls.clone();
                    Box::pin(async move {
                        calls
                            .lock()
                            .expect("call log")
                            .push(format!("rollback:{}", record.plugin_id));
                        Ok(PluginRollbackOutcome::RolledBack)
                    })
                })
            },
        }
    }

    fn two_plugin_snapshot() -> PluginsInventorySnapshot {
        PluginsInventorySnapshot {
            installed: vec![
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
            ],
            marketplace: vec![],
            marketplaces: vec![],
        }
    }

    fn call_log() -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
    }

    /// Skipped rows are decisions made before any CLI call; a run with
    /// one must invoke the CLI exactly once.
    #[tokio::test(flavor = "current_thread")]
    async fn skipped_rows_never_reach_the_cli() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let calls = call_log();
        let cli = fake_cli(
            "supabase is already at the latest version (1.0.0).",
            &two_plugin_snapshot(),
            &calls,
        );
        let mut skipped = PluginUpdateRunRow::queued(
            "scratch-tools".to_owned(),
            "user".to_owned(),
            "/proj".to_owned(),
            Some("0.2.0".to_owned()),
        );
        skipped.status = PluginRunRowStatus::Skipped;
        skipped.detail = Some("plugin id carries no marketplace".to_owned());
        let rows = vec![
            PluginUpdateRunRow::queued(
                "supabase@claude-plugins-official".to_owned(),
                "user".to_owned(),
                "/proj".to_owned(),
                Some("1.0.0".to_owned()),
            ),
            skipped,
        ];
        let run = PluginUpdateRun { trigger: PluginUpdateTrigger::Auto, finished: false, rows };
        let plan = UpdateRunPlan {
            cwd_context: "/proj".to_owned(),
            claude_path: None,
            marketplaces: vec![],
            run,
            cli,
            store: None,
        };
        execute_update_plan(tx, plan).await;

        let log = calls.lock().expect("call log").clone();
        let update_calls: Vec<&String> =
            log.iter().filter(|call| !call.starts_with("refresh")).collect();
        assert_eq!(update_calls.len(), 1, "only the queued row invokes the CLI: {log:?}");
        assert!(update_calls[0].contains("supabase@claude-plugins-official"));

        let mut finished = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PluginsUpdateRunFinished { run, .. } = update {
                finished = Some(run);
            }
        }
        let run = finished.expect("the finished event lands");
        let skipped =
            run.rows.iter().find(|row| row.plugin_id.starts_with("scratch-tools")).expect("row");
        assert_eq!(skipped.status, PluginRunRowStatus::Skipped);
        let ran = run.rows.iter().find(|row| row.plugin_id.starts_with("supabase")).expect("row");
        assert_eq!(ran.status, PluginRunRowStatus::AlreadyCurrent);
    }

    /// The `u` key inside a LocalSet: the run executes through the
    /// injected CLI, one call per entry, and the pane settles with the
    /// report instead of refusing silently.
    #[tokio::test(flavor = "current_thread")]
    async fn the_u_key_runs_every_installed_plugin() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        let calls = call_log();
        app.plugins.update_cli =
            Some(fake_cli("is already at the latest version.", &two_plugin_snapshot(), &calls));
        app.plugins.active_tab = PluginsViewTab::Installed;

        tokio::task::LocalSet::new()
            .run_until(async {
                handle_key(&mut app, KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
                for _ in 0..200 {
                    tokio::task::yield_now().await;
                    while let Ok(update) = app.update_rx.try_recv() {
                        apply_session_update(&mut app, update);
                    }
                    if app.plugins.update_run.as_ref().is_some_and(|run| run.finished) {
                        break;
                    }
                }
            })
            .await;

        let log = calls.lock().expect("call log").clone();
        assert_eq!(
            log.iter().filter(|call| !call.starts_with("refresh")).count(),
            3,
            "one update call per installed entry: {log:?}"
        );
        let run = app.plugins.update_run.as_ref().expect("the report stands");
        assert!(run.finished);
        assert!(run.rows.iter().all(|row| row.status == PluginRunRowStatus::AlreadyCurrent));
    }

    /// Boot auto-update with the switch off touches nothing: no
    /// seeded run, no CLI calls, no events.
    #[tokio::test(flavor = "current_thread")]
    async fn boot_auto_update_off_does_nothing() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace");
        let calls = call_log();
        let cli = fake_cli("unused", &two_plugin_snapshot(), &calls);

        tokio::task::LocalSet::new()
            .run_until(async {
                maybe_spawn_boot_auto_update(
                    &workspace,
                    &mut app,
                    "/proj".to_owned(),
                    &forge_workspace::PluginSettings::default(),
                    cli,
                );
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
            })
            .await;

        assert!(app.plugins.update_run.is_none());
        assert!(calls.lock().expect("call log").is_empty());
        assert!(app.update_rx.try_recv().is_err());
    }

    /// Boot auto-update updates every installed plugin from its own
    /// entry cwd. The run is seeded synchronously so a manual `u`
    /// cannot race it.
    #[tokio::test(flavor = "current_thread")]
    async fn boot_auto_update_runs_every_installed_plugin() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace");
        let calls = call_log();
        let cli = fake_cli(
            "supabase is already at the latest version (1.0.0).",
            &two_plugin_snapshot(),
            &calls,
        );
        let settings = forge_workspace::PluginSettings { auto_update: true };

        tokio::task::LocalSet::new()
            .run_until(async {
                maybe_spawn_boot_auto_update(
                    &workspace,
                    &mut app,
                    "/test".to_owned(),
                    &settings,
                    cli,
                );
                assert!(
                    app.plugins.update_run.as_ref().is_some_and(|run| !run.finished),
                    "the seeded run guards u/c before the first event lands"
                );
                for _ in 0..200 {
                    tokio::task::yield_now().await;
                    while let Ok(update) = app.update_rx.try_recv() {
                        apply_session_update(&mut app, update);
                    }
                    if app.plugins.update_run.as_ref().is_some_and(|run| run.finished) {
                        break;
                    }
                }
            })
            .await;

        let log = calls.lock().expect("call log").clone();
        let update_calls: Vec<&String> =
            log.iter().filter(|call| !call.starts_with("refresh")).collect();
        assert_eq!(update_calls.len(), 3, "every installed plugin updates: {log:?}");
        assert!(
            log.iter().any(|call| call.starts_with("/test:")),
            "user-scoped plugins update from the boot cwd: {log:?}"
        );

        let run = app.plugins.update_run.as_ref().expect("the report stands");
        assert!(run.finished);
        assert!(
            run.rows.iter().all(|row| row.status == PluginRunRowStatus::AlreadyCurrent),
            "the fake CLI reports every plugin current: {:?}",
            run.rows
        );
    }

    /// Records persist in the run task, not the event handler: a
    /// dropped report event cannot lose the rollback record.
    #[tokio::test(flavor = "current_thread")]
    async fn an_update_run_persists_records_in_the_task() {
        let mut app = App::test_default();
        let db_dir = tempfile::tempdir().expect("tempdir");
        app.workspace.as_ref().expect("workspace").install_db_for_test(
            forge_workspace::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "supabase@claude-plugins-official".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        let mut snapshot = two_plugin_snapshot();
        snapshot.installed = vec![InstalledPluginEntry {
            id: "supabase@claude-plugins-official".to_owned(),
            version: Some("2.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        let calls = call_log();
        app.plugins.update_cli = Some(fake_cli(
            "Plugin \"supabase\" updated from 1.0.0 to 2.0.0 for scope user.",
            &snapshot,
            &calls,
        ));

        tokio::task::LocalSet::new()
            .run_until(async {
                handle_key(&mut app, KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
                for _ in 0..200 {
                    tokio::task::yield_now().await;
                    if app.plugins.update_run.as_ref().is_some_and(|run| run.finished) {
                        break;
                    }
                }
            })
            .await;

        // The record must exist BEFORE any event handling: persistence
        // lives in the run task, not the pane's event handler.
        let workspace = app.workspace.as_ref().expect("workspace");
        let records = workspace.plugin_update_records();
        assert_eq!(records.len(), 1, "the record persisted with no events applied");
        assert_eq!(records[0].from_version.as_deref(), Some("1.0.0"));
        assert_eq!(records[0].to_version.as_deref(), Some("2.0.0"));
        assert_eq!(records[0].trigger, PluginUpdateTrigger::Manual);

        while let Ok(update) = app.update_rx.try_recv() {
            apply_session_update(&mut app, update);
        }
        assert_eq!(
            app.plugins.update_run.map(|run| run.summary()),
            Some("1 updated, 0 failed, 0 current".to_owned())
        );
    }

    /// An exit-0 failure keeps its prose: the classifier's failure
    /// detail lands on the row, not just its status.
    #[tokio::test(flavor = "current_thread")]
    async fn an_exit_zero_failure_row_keeps_the_cli_prose() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let calls = call_log();
        let cli = fake_cli(
            "✘ Failed to update plugin \"supabase\": Plugin \"supabase\" not found",
            &two_plugin_snapshot(),
            &calls,
        );
        let rows = build_rows_from_entries(
            &two_plugin_snapshot().installed,
            "/proj",
            PluginUpdateTrigger::Manual,
        );
        let plan = UpdateRunPlan {
            cwd_context: "/proj".to_owned(),
            claude_path: None,
            marketplaces: vec![],
            run: PluginUpdateRun { trigger: PluginUpdateTrigger::Manual, finished: false, rows },
            cli,
            store: None,
        };
        execute_update_plan(tx, plan).await;

        let mut finished = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PluginsUpdateRunFinished { run, .. } = update {
                finished = Some(run);
            }
        }
        let run = finished.expect("the finished event lands");
        let failed =
            run.rows.iter().find(|row| row.plugin_id.starts_with("supabase")).expect("row");
        assert_eq!(failed.status, PluginRunRowStatus::Failed);
        assert!(
            failed.detail.as_deref().is_some_and(|detail| detail.contains("not found")),
            "the exit-0 failure prose reaches the row: {:?}",
            failed.detail
        );
    }

    /// An entry the post-run inventory no longer lists is its own
    /// outcome: Failed with the reason, and no record naming a version
    /// nobody can see.
    #[tokio::test(flavor = "current_thread")]
    async fn an_entry_absent_from_the_post_run_snapshot_fails_without_a_record() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let calls = call_log();
        let mut snapshot = two_plugin_snapshot();
        snapshot.installed.retain(|entry| entry.id.starts_with("supabase"));
        let cli = fake_cli(
            "Plugin \"pensive\" updated from 1.7.2 to 1.8.0 for scope user.",
            &snapshot,
            &calls,
        );
        let rows = build_rows_from_entries(
            &two_plugin_snapshot().installed,
            "/proj",
            PluginUpdateTrigger::Manual,
        );
        let plan = UpdateRunPlan {
            cwd_context: "/proj".to_owned(),
            claude_path: None,
            marketplaces: vec![],
            run: PluginUpdateRun { trigger: PluginUpdateTrigger::Manual, finished: false, rows },
            cli,
            store: None,
        };
        execute_update_plan(tx, plan).await;

        let mut finished = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PluginsUpdateRunFinished { run, .. } = update {
                finished = Some(run);
            }
        }
        let run = finished.expect("the finished event lands");
        let vanished =
            run.rows.iter().find(|row| row.plugin_id.starts_with("pensive")).expect("row");
        assert_eq!(vanished.status, PluginRunRowStatus::Failed);
        assert_eq!(vanished.detail.as_deref(), Some("not found in post-update inventory"));
    }

    /// A hung CLI call fails its row when the bound expires instead of
    /// pinning the pane forever.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hung_update_call_times_out_its_row() {
        tokio::time::pause();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let snapshot = two_plugin_snapshot();
        let cli = UpdateCli {
            run_update: std::sync::Arc::new(|_cached, _cwd, _args| {
                Box::pin(std::future::pending::<UpdateResult>())
            }),
            refresh: {
                let snapshot = snapshot.clone();
                std::sync::Arc::new(move |_cached, _cwd| {
                    let snapshot = snapshot.clone();
                    Box::pin(async move { Ok((snapshot, std::path::PathBuf::from("claude"))) })
                })
            },
            rollback: std::sync::Arc::new(|_, _, _, _| {
                Box::pin(std::future::pending::<RollbackResult>())
            }),
        };
        let rows =
            build_rows_from_entries(&snapshot.installed, "/proj", PluginUpdateTrigger::Manual);
        let plan = UpdateRunPlan {
            cwd_context: "/proj".to_owned(),
            claude_path: None,
            marketplaces: vec![],
            run: PluginUpdateRun { trigger: PluginUpdateTrigger::Manual, finished: false, rows },
            cli,
            store: None,
        };
        execute_update_plan(tx, plan).await;

        let mut finished = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PluginsUpdateRunFinished { run, .. } = update {
                finished = Some(run);
            }
        }
        let run = finished.expect("the finished event lands");
        let row = &run.rows[0];
        assert_eq!(row.status, PluginRunRowStatus::Failed);
        assert!(
            row.detail.as_deref().is_some_and(|detail| detail.contains("timed out")),
            "the timeout names itself: {:?}",
            row.detail
        );
    }

    /// The deferred branch: updates land, then the post-run refresh
    /// hangs. Rows keep the captured CLI output AND the timeout
    /// reason, so the evidence of what may have applied survives.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hung_post_run_refresh_keeps_the_output_and_names_itself() {
        tokio::time::pause();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let snapshot = two_plugin_snapshot();
        let cli = UpdateCli {
            run_update: {
                let snapshot = snapshot.clone();
                std::sync::Arc::new(move |_cached, _cwd, _args| {
                    let _ = snapshot.clone();
                    Box::pin(async move {
                        Ok((
                            std::path::PathBuf::from("claude"),
                            "Plugin \"supabase\" updated from 1.0.0 to 2.0.0 for scope user."
                                .to_owned(),
                        ))
                    })
                })
            },
            refresh: std::sync::Arc::new(|_cached, _cwd| {
                Box::pin(std::future::pending::<RefreshResult>())
            }),
            rollback: std::sync::Arc::new(|_, _, _, _| {
                Box::pin(std::future::pending::<RollbackResult>())
            }),
        };
        let rows =
            build_rows_from_entries(&snapshot.installed, "/proj", PluginUpdateTrigger::Manual);
        let plan = UpdateRunPlan {
            cwd_context: "/proj".to_owned(),
            claude_path: None,
            marketplaces: vec![],
            run: PluginUpdateRun { trigger: PluginUpdateTrigger::Manual, finished: false, rows },
            cli,
            store: None,
        };
        execute_update_plan(tx, plan).await;

        let mut finished = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PluginsUpdateRunFinished { run, .. } = update {
                finished = Some(run);
            }
        }
        let run = finished.expect("the finished event lands");
        let row = &run.rows[0];
        assert_eq!(row.status, PluginRunRowStatus::Failed);
        let detail = row.detail.as_deref().expect("the row keeps its evidence");
        assert!(detail.contains("updated from 1.0.0 to 2.0.0"), "CLI output kept: {detail}");
        assert!(
            detail.contains("timed out"),
            "the refresh timeout names itself beside the output: {detail}"
        );
    }

    /// Boot failure routing: the failure event unpinns the seeded run
    /// even when the borrowed project cwd does not match the focused
    /// session.
    #[tokio::test(flavor = "current_thread")]
    async fn boot_refresh_failure_unpins_the_run_across_a_mismatched_cwd() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace");
        let calls = call_log();
        let cli = UpdateCli {
            run_update: std::sync::Arc::new(|_, _, _| {
                Box::pin(std::future::pending::<UpdateResult>())
            }),
            refresh: std::sync::Arc::new(|_cached, _cwd| {
                Box::pin(async move { Err("claude CLI not found".to_owned()) })
            }),
            rollback: std::sync::Arc::new(|_, _, _, _| {
                Box::pin(std::future::pending::<RollbackResult>())
            }),
        };
        let settings = forge_workspace::PluginSettings { auto_update: true };

        tokio::task::LocalSet::new()
            .run_until(async {
                maybe_spawn_boot_auto_update(
                    &workspace,
                    &mut app,
                    "/proj".to_owned(),
                    &settings,
                    cli,
                );
                for _ in 0..200 {
                    tokio::task::yield_now().await;
                    while let Ok(update) = app.update_rx.try_recv() {
                        apply_session_update(&mut app, update);
                    }
                    if !app.plugins.loading {
                        break;
                    }
                }
            })
            .await;

        assert!(!app.plugins.loading, "the seeded run does not pin the pane");
        assert!(app.plugins.update_run.is_none(), "the empty seeded run is cleared");
        assert!(
            app.plugins
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("claude CLI not found")),
            "the failure is visible in the pane: {:?}",
            app.plugins.last_error
        );
        assert!(
            calls.lock().expect("call log").is_empty(),
            "nothing ran beyond the failed refresh"
        );
    }

    /// The bypass is trigger-scoped: a Manual run's events still drop
    /// on a cwd mismatch.
    #[test]
    fn manual_run_events_still_respect_the_cwd_gate() {
        let mut app = App::test_default();
        let run = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![PluginUpdateRunRow {
                plugin_id: "supabase@claude-plugins-official".to_owned(),
                scope: "user".to_owned(),
                cwd_raw: String::new(),
                marketplace: "claude-plugins-official".to_owned(),
                status: PluginRunRowStatus::Updated,
                installed_version: Some("2.0.0".to_owned()),
                available_version: None,
                detail: None,
            }],
        };

        apply_session_update(
            &mut app,
            SessionUpdate::PluginsUpdateRunFinished {
                cwd_raw: "/elsewhere".to_owned(),
                run,
                snapshot: None,
                claude_path: None,
            },
        );

        assert!(
            app.plugins.update_run.is_none(),
            "a mismatched manual event must not seed the pane"
        );
    }

    /// A rollback that claims success but leaves the new version in
    /// place fails the verification and keeps its record.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unverified_rollback_fails_and_keeps_its_record() {
        let mut app = App::test_default();
        let db_dir = tempfile::tempdir().expect("tempdir");
        app.workspace.as_ref().expect("workspace").install_db_for_test(
            forge_workspace::store::Db::open(&db_dir.path().join("db.redb")).expect("open db"),
        );
        let record = PluginUpdateRecord {
            plugin_id: "pensive@claude-night-market".to_owned(),
            marketplace: "claude-night-market".to_owned(),
            scope: "user".to_owned(),
            cwd_raw: String::new(),
            from_version: Some("1.7.1".to_owned()),
            to_version: Some("1.7.2".to_owned()),
            marketplace_ref_before: Some("abc123".to_owned()),
            updated_at: "2026-09-04T06:00:00Z".to_owned(),
            trigger: PluginUpdateTrigger::Manual,
        };
        app.plugins.update_records = vec![record.clone()];
        let workspace = app.workspace.clone().expect("workspace");
        workspace.record_plugin_updates(std::slice::from_ref(&record));

        // The rollback "succeeds" but the post-rollback inventory still
        // shows the NEW version: the old manifest did not pin one.
        let mut snapshot = two_plugin_snapshot();
        snapshot.installed = vec![InstalledPluginEntry {
            id: "pensive@claude-night-market".to_owned(),
            version: Some("1.7.2".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        let cli = UpdateCli {
            run_update: std::sync::Arc::new(|_, _, _| {
                Box::pin(std::future::pending::<UpdateResult>())
            }),
            refresh: {
                let snapshot = snapshot.clone();
                std::sync::Arc::new(move |_cached, _cwd| {
                    let snapshot = snapshot.clone();
                    Box::pin(async move { Ok((snapshot, std::path::PathBuf::from("claude"))) })
                })
            },
            rollback: std::sync::Arc::new(|_, _, _, _| {
                Box::pin(async move { Ok(PluginRollbackOutcome::RolledBack) })
            }),
        };
        app.plugins.update_cli = Some(cli);
        app.plugins.marketplaces = vec![MarketplaceSourceEntry {
            name: "claude-night-market".to_owned(),
            source: Some("github".to_owned()),
            repo: Some("athola/claude-night-market".to_owned()),
            install_location: Some("/tmp/whatever".to_owned()),
        }];

        tokio::task::LocalSet::new()
            .run_until(async {
                start_rollback(
                    &mut app,
                    "pensive@claude-night-market".to_owned(),
                    "user".to_owned(),
                );
                for _ in 0..200 {
                    tokio::task::yield_now().await;
                    if !app.plugins.loading {
                        break;
                    }
                }
            })
            .await;

        let mut failed = None;
        while let Ok(update) = app.update_rx.try_recv() {
            if let SessionUpdate::PluginsRollbackFailed { message, .. } = update {
                failed = Some(message);
            }
        }
        let message = failed.expect("the divergence surfaces as a failed rollback");
        assert!(
            message.contains("did not move to 1.7.1"),
            "the divergence names the expected version: {message}"
        );
        assert!(
            message.contains("record is kept"),
            "the message says the record survives: {message}"
        );
        assert!(
            has_rollback_record(&app, "pensive@claude-night-market", "user"),
            "the record survives the unverified rollback"
        );
        assert_eq!(workspace.plugin_update_records().len(), 1, "the store record is untouched");
    }

    /// An unfinished run (a boot auto-update in flight) refuses u:
    /// no new CLI calls, no replacement run.
    #[tokio::test(flavor = "current_thread")]
    async fn u_refuses_while_a_run_is_in_flight() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        let calls = call_log();
        app.plugins.update_cli =
            Some(fake_cli("is already at the latest version.", &two_plugin_snapshot(), &calls));
        app.plugins.update_run = Some(PluginUpdateRun {
            trigger: PluginUpdateTrigger::Auto,
            finished: false,
            rows: vec![],
        });

        tokio::task::LocalSet::new()
            .run_until(async {
                handle_key(&mut app, KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
            })
            .await;

        assert!(
            calls.lock().expect("call log").is_empty(),
            "the in-flight run blocks a second one"
        );
        let run = app.plugins.update_run.as_ref().expect("the seeded run stands");
        assert!(!run.finished);
        assert!(run.rows.is_empty(), "the seeded run was not replaced");
    }

    /// Boot runs are app-scoped: their report lands even when the
    /// borrowed project cwd does not match the focused session's, so
    /// the launchpad case is not event-blind.
    #[tokio::test(flavor = "current_thread")]
    async fn boot_events_apply_regardless_of_the_focused_cwd() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace");
        let calls = call_log();
        let cli = fake_cli(
            "supabase is already at the latest version (1.0.0).",
            &two_plugin_snapshot(),
            &calls,
        );
        let settings = forge_workspace::PluginSettings { auto_update: true };

        tokio::task::LocalSet::new()
            .run_until(async {
                maybe_spawn_boot_auto_update(
                    &workspace,
                    &mut app,
                    "/proj".to_owned(),
                    &settings,
                    cli,
                );
                for _ in 0..200 {
                    tokio::task::yield_now().await;
                    while let Ok(update) = app.update_rx.try_recv() {
                        apply_session_update(&mut app, update);
                    }
                    if app.plugins.update_run.as_ref().is_some_and(|run| run.finished) {
                        break;
                    }
                }
            })
            .await;

        let run = app.plugins.update_run.as_ref().expect("the report lands");
        assert!(run.finished, "a mismatched session cwd must not drop the boot report");
        assert!(run.rows.iter().any(|row| row.status == PluginRunRowStatus::AlreadyCurrent));
    }
}
