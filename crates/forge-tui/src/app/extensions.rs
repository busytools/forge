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

pub mod installed;
pub mod skills;
pub mod state;
pub mod updates;

pub use state::{
    ExtensionsTab, TabState, available_count_for_tab, count_for_tab, row_matches, rows_for_tab,
    tab_takes_available, update_all_count,
};

// Plugin registry types defined in forge_primitives::plugins;
// re-exported here so the existing forge-tui import paths resolve.
pub use forge_primitives::plugins::{
    ExtensionKind, ExtensionRow, InstalledPluginEntry, MarketplaceEntry, MarketplaceHealth,
    MarketplaceSourceEntry, PluginComponents, PluginRunRowStatus, PluginUpdateAvailability,
    PluginUpdateRecord, PluginUpdateRun, PluginUpdateRunRow, PluginUpdateTrigger,
    PluginsCliActionSuccess, PluginsInventorySnapshot, RowState, extension_rows,
    update_availability,
};

#[derive(Debug, Clone, Default)]
pub struct PluginsState {
    pub active_tab: ExtensionsTab,
    pub search_focused: bool,
    /// Per-tab filter and selection, indexed by the tab's position in
    /// [`ExtensionsTab::ALL`].
    pub tab_state: TabState,
    pub installed: Vec<InstalledPluginEntry>,
    pub marketplace: Vec<MarketplaceEntry>,
    pub marketplaces: Vec<MarketplaceSourceEntry>,
    pub loading: bool,
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
    /// The pane's two row streams from the last inventory refresh,
    /// kept separate so a tab's count says what the tab shows: the
    /// registry-backed INSTALLED rows (plugins, their components, the
    /// load-failure rows), and the marketplace catalog's AVAILABLE
    /// rows, which render only behind the Available toggle.
    pub installed_rows: Vec<ExtensionRow>,
    pub available_rows: Vec<ExtensionRow>,
    /// The Available toggle: when set, component tabs append the
    /// available stream's rows, dim, after the installed ones.
    pub show_available: bool,
    pub health: Vec<MarketplaceHealth>,
    /// Always-on token cost per installed plugin id, version-keyed:
    /// id -> (installed version, cost). A version change refetches.
    pub token_costs: std::collections::BTreeMap<String, (String, u64)>,
    /// Declared LSP launch command per (plugin id, server key), from
    /// the manifests; the PATH check tests this, not the key.
    pub lsp_commands: std::collections::BTreeMap<(String, String), String>,
    /// Plugins whose applied update stated the restart contract; a
    /// successful runtime reload consumes it.
    pub restart_pending: std::collections::BTreeSet<String>,
    /// Test seam: the per-run CLI surface a run uses. `None` means
    /// the real `claude` subprocess calls.
    pub(crate) update_cli: Option<UpdateCli>,
}

impl PluginsState {
    pub fn selected_index_for(&self, tab: ExtensionsTab) -> usize {
        self.tab_state.selected[tab.index()]
    }

    pub fn set_selected_index_for(&mut self, tab: ExtensionsTab, index: usize) {
        self.tab_state.selected[tab.index()] = index;
    }

    pub fn search_query_for(&self, tab: ExtensionsTab) -> String {
        self.tab_state.search_queries[tab.index()].text()
    }

    pub fn active_search_query_mut(&mut self) -> Option<&mut InputState> {
        let index = self.active_tab.index();
        self.tab_state.search_queries.get_mut(index)
    }

    /// Both streams, installed first: the walk order for
    /// whole-inventory passes (annotation, restart stamps).
    pub fn all_rows(&self) -> impl Iterator<Item = &ExtensionRow> {
        self.installed_rows.iter().chain(self.available_rows.iter())
    }

    /// The row with this id across both streams, mutable.
    pub fn row_mut(&mut self, id: &str) -> Option<&mut ExtensionRow> {
        self.installed_rows
            .iter_mut()
            .find(|row| row.id == id)
            .or_else(|| self.available_rows.iter_mut().find(|row| row.id == id))
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
        app.needs_redraw = true;
        return true;
    }
    match (key.code, key.modifiers) {
        // The Mcps tab keeps the MCP page's own keys: Up/Down select,
        // Enter opens the server's actions, r refreshes the snapshot.
        (KeyCode::Up | KeyCode::Down, KeyModifiers::NONE)
            if app.plugins.active_tab == ExtensionsTab::Mcps =>
        {
            crate::app::config::mcp::handle_mcp_key(app, key)
        }
        (KeyCode::Enter, _)
            if app.plugins.active_tab == ExtensionsTab::Mcps && !app.plugins.search_focused =>
        {
            crate::app::config::mcp::handle_mcp_key(app, key)
        }
        (KeyCode::Char(ch), modifiers)
            if matches!(ch, 'r' | 'R')
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
                && app.plugins.active_tab == ExtensionsTab::Mcps =>
        {
            crate::app::config::mcp::handle_mcp_key(app, key)
        }
        (KeyCode::Left, KeyModifiers::NONE) => {
            app.plugins.active_tab = app.plugins.active_tab.prev();
            app.plugins.search_focused = false;
            clamp_selection(app);
            request_mcp_snapshot_if_newly_active(app);
            true
        }
        (KeyCode::Right, KeyModifiers::NONE) => {
            app.plugins.active_tab = app.plugins.active_tab.next();
            app.plugins.search_focused = false;
            clamp_selection(app);
            request_mcp_snapshot_if_newly_active(app);
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
            ExtensionsTab::Installed => open_installed_actions_overlay(app),
            ExtensionsTab::Skills
            | ExtensionsTab::Agents
            | ExtensionsTab::Commands
            | ExtensionsTab::Hooks
            | ExtensionsTab::Lsp => installed::open_component_actions_overlay(app),
            // The Mcps tab's actions arrive with its tab render.
            ExtensionsTab::Mcps => true,
            ExtensionsTab::Marketplaces => open_marketplace_overlay(app),
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
            if matches!(ch, 'a' | 'A')
                && (modifiers.is_empty() || modifiers == KeyModifiers::SHIFT)
                && !app.plugins.search_focused
                && tab_takes_available(app.plugins.active_tab) =>
        {
            app.plugins.show_available = !app.plugins.show_available;
            reset_selection_for_active_tab(app);
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

/// Landing on the Mcps tab asks for a snapshot if the held one has
/// aged past the MCP cadence, so the tab does not open stale.
fn request_mcp_snapshot_if_newly_active(app: &mut App) {
    if app.plugins.active_tab == ExtensionsTab::Mcps {
        crate::app::config::mcp::request_mcp_snapshot_if_needed(app, Instant::now());
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
    request_disk_refresh(app);
}

pub(crate) fn request_inventory_refresh_manual(app: &mut App) {
    // Same guard the `u` key carries: a manual refresh must not stack
    // behind an in-flight one or an unfinished run.
    if app.plugins.loading || app.plugins.update_run.as_ref().is_some_and(|run| !run.finished) {
        return;
    }
    app.plugins.runtime_reload_after_refresh = true;
    request_inventory_refresh(app);
}

/// The TTL path: the disk scan alone - registry, settings, manifests -
/// no `claude` spawn. First paint and every five-second refresh stay
/// off the CLI; the manual `r` keeps the full CLI refresh.
fn request_disk_refresh(app: &mut App) {
    request_disk_refresh_with(app, cli::inventory_from_disk_blocking);
}

/// The TTL path's scan is injectable so the failure arm is testable.
/// An unresolvable root or a failed join is a refresh FAILURE naming
/// why - never an all-empty snapshot applied as truth (hard rule 14:
/// fail rather than substitute).
fn request_disk_refresh_with(
    app: &mut App,
    scan: impl FnOnce() -> Option<PluginsInventorySnapshot> + Send + 'static,
) {
    if tokio::runtime::Handle::try_current().is_err() {
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for plugin action".to_owned());
        return;
    }
    app.plugins.loading = true;
    app.needs_redraw = true;
    let event_tx = app.update_tx.clone();
    let cwd_context = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    let disk_span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_disk_refresh",
    );
    tokio::task::spawn_local(
        async move {
            let scan_result = tokio::task::spawn_blocking(scan).await;
            match scan_result {
                Ok(Some(snapshot)) => {
                    let _ = event_tx.send(SessionUpdate::PluginsInventoryUpdated {
                        cwd_raw: cwd_context,
                        snapshot,
                        claude_path: cached_claude_path.unwrap_or_default(),
                    });
                }
                Ok(None) => {
                    let _ = event_tx.send(SessionUpdate::PluginsInventoryRefreshFailed {
                        cwd_raw: cwd_context,
                        message: "the plugin root could not be resolved (no CLAUDE_CONFIG_DIR \
                                  or HOME); the previous inventory is kept"
                            .to_owned(),
                        trigger: PluginUpdateTrigger::Manual,
                    });
                }
                Err(join) => {
                    let _ = event_tx.send(SessionUpdate::PluginsInventoryRefreshFailed {
                        cwd_raw: cwd_context,
                        message: format!("the extension disk scan failed to join: {join}"),
                        trigger: PluginUpdateTrigger::Manual,
                    });
                }
            }
        }
        .instrument(disk_span),
    );
}

/// The CLI refresh for the manual `r` and post-action paths. The
/// token-cost fetch is deliberately NOT awaited here: the inventory
/// event goes out first so paint never waits on a `claude` spawn, and
/// a follow-up task lands the costs through a costs-only event.
pub(crate) fn request_inventory_refresh(app: &mut App) {
    if tokio::runtime::Handle::try_current().is_err() {
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for plugin action".to_owned());
        return;
    }
    app.plugins.loading = true;
    app.config.last_error = None;
    app.config.status_message = Some("Refreshing plugin inventory...".to_owned());
    app.needs_redraw = true;
    let event_tx = app.update_tx.clone();
    let cwd_context = app.cwd_raw();
    let cwd_raw = app.cwd_raw();
    let cached_claude_path = app.plugins.claude_path.clone();
    // Plugins whose token cost is missing or stale under their current
    // version; the cache means a steady-state refresh fetches nothing.
    let cost_requests = app
        .plugins
        .installed
        .iter()
        .filter_map(|entry| {
            let version = entry.version.as_deref()?;
            let fresh =
                app.plugins.token_costs.get(&entry.id).is_some_and(|(cached, _)| cached == version);
            (!fresh).then(|| (entry.id.clone(), version.to_owned()))
        })
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .collect::<Vec<_>>();
    let span = info_span!(
        target: crate::logging::targets::APP_CONFIG,
        "plugin_inventory_refresh",
        cwd = %cwd_raw,
    );
    let cli = app.plugins.update_cli.clone().unwrap_or_else(UpdateCli::real);
    tokio::task::spawn_local(
        async move {
            match (cli.refresh)(cached_claude_path.clone(), cwd_raw.clone()).await {
                Ok((snapshot, claude_path)) => {
                    let _ = event_tx.send(SessionUpdate::PluginsInventoryUpdated {
                        cwd_raw: cwd_context.clone(),
                        snapshot,
                        claude_path,
                    });
                    // Lazy costs: after paint, so the fetch spawns never
                    // gate the rows on screen.
                    let costs = (cli.details)(cached_claude_path, cwd_raw, cost_requests).await;
                    if !costs.is_empty() {
                        let _ = event_tx.send(SessionUpdate::PluginsInventoryUpdated {
                            cwd_raw: cwd_context,
                            snapshot: PluginsInventorySnapshot {
                                token_costs: costs,
                                ..PluginsInventorySnapshot::default()
                            },
                            claude_path: PathBuf::new(),
                        });
                    }
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
    // A costs-only snapshot (all lists empty, costs populated) is the
    // lazy token-cost fetch landing after paint: merge the cache, keep
    // every list. A real refresh with nothing installed carries empty
    // costs too, so this arm cannot swallow one.
    if snapshot.installed.is_empty()
        && snapshot.marketplace.is_empty()
        && snapshot.marketplaces.is_empty()
        && snapshot.components.is_empty()
        && snapshot.marketplace_health.is_empty()
        && !snapshot.token_costs.is_empty()
    {
        merge_token_costs(app, snapshot.token_costs);
        annotate_rows(app);
        app.needs_redraw = true;
        return;
    }

    let should_reload_runtime = std::mem::take(&mut app.plugins.runtime_reload_after_refresh);
    app.plugins.installed = snapshot.installed;
    app.plugins.marketplace = snapshot.marketplace;
    app.plugins.marketplaces = snapshot.marketplaces;
    // Merge, not replace: the costs map is a version-keyed cache, so a
    // refresh that fetched nothing keeps every badge on screen.
    merge_token_costs(app, snapshot.token_costs);
    rebuild_rows(app, &snapshot.components, snapshot.marketplace_health);
    app.plugins.loading = false;
    app.plugins.last_inventory_refresh_at = Some(Instant::now());
    // The disk-spine path has no claude resolution; an empty path must
    // not clobber the cached one the CLI actions reuse.
    if !claude_path.as_os_str().is_empty() {
        app.plugins.claude_path = Some(claude_path);
    }
    refresh_update_records(app);
    app.plugins.update_availability =
        update_availability(&app.plugins.installed, &app.plugins.marketplace);
    apply_restart_overrides(app);
    clamp_selection(app);
    if should_reload_runtime {
        start_runtime_reload(app, "Plugin inventory refreshed".to_owned());
    } else {
        app.config.last_error = None;
        app.config.status_message = Some("Plugin inventory refreshed".to_owned());
    }
}

/// Fold a details batch into the version-keyed cost cache.
fn merge_token_costs(
    app: &mut App,
    costs: std::collections::BTreeMap<String, (String, forge_primitives::plugins::PluginDetails)>,
) {
    for (id, (version, details)) in costs {
        // The requested version travels with the cost: stamping the
        // installed version instead would pin an old cost under a new
        // version's key after an update lands mid-fetch.
        app.plugins.token_costs.insert(id, (version, details.token_cost_always_on));
    }
}

/// Recompute the row decorations the snapshot does not carry: the LSP
/// binary check (against the manifest's declared command) and the
/// plugin rows' token cost.
fn annotate_rows(app: &mut App) {
    annotate_rows_on_path(
        app,
        std::env::var_os("PATH").as_deref().map(|p| p.to_string_lossy()).as_deref(),
    );
}

/// The annotation with an injectable PATH so tests are hermetic.
fn annotate_rows_on_path(app: &mut App, path: Option<&str>) {
    let lsp: Vec<(String, String, Option<String>)> = app
        .plugins
        .all_rows()
        .filter(|row| row.kind == ExtensionKind::Lsp)
        .map(|row| {
            let command = app.plugins.lsp_commands.get(&(row.source.clone(), row.name.clone()));
            (row.id.clone(), row.name.clone(), command.cloned())
        })
        .collect();
    for (id, name, command) in lsp {
        let checked = command.unwrap_or_else(|| name.clone());
        let Some(row) = app.plugins.row_mut(&id) else { continue };
        let on_path = path.is_some_and(|path| skills::lsp_binary_on_path(&checked, Some(path)));
        row.detail =
            Some(if on_path { format!("{name}: on PATH") } else { format!("{name}: missing") });
    }
    let costs: Vec<(String, Option<u64>, bool)> = app
        .plugins
        .installed_rows
        .iter()
        .filter(|row| row.kind == ExtensionKind::Plugin)
        .map(|row| {
            (
                row.id.clone(),
                app.plugins.token_costs.get(&row.id).map(|(_, cost)| *cost),
                row.state == RowState::UpdateAvailable,
            )
        })
        .collect();
    for (id, cost, update_available) in costs {
        let Some(row) = app.plugins.row_mut(&id) else { continue };
        let mut parts: Vec<String> = Vec::new();
        if let Some(cost) = cost {
            parts.push(format!("~{cost} tok always-on"));
        }
        if update_available && row.detail.as_deref() == Some("auto-installed") {
            parts.push("auto-installed".to_owned());
        }
        match parts.as_slice() {
            [] => {}
            [only] => row.detail = Some(only.clone()),
            [..] => row.detail = Some(parts.join(" \u{b7} ")),
        }
    }
}

/// Re-stamp the RestartRequired state on plugin rows whose applied
/// update awaits a restart; the underlying scan reads Current until a
/// reload consumes the change. The stamp is derived, so it resets
/// before re-applying - a cleared pending set must un-stamp too.
fn apply_restart_overrides(app: &mut App) {
    for row in app.plugins.installed_rows.iter_mut().chain(app.plugins.available_rows.iter_mut()) {
        if row.kind == ExtensionKind::Plugin && row.state == RowState::RestartRequired {
            row.state = RowState::Current;
        }
    }
    for row in app.plugins.installed_rows.iter_mut().chain(app.plugins.available_rows.iter_mut()) {
        if row.kind == ExtensionKind::Plugin
            && row.state == RowState::Current
            && app.plugins.restart_pending.contains(&row.id)
        {
            row.state = RowState::RestartRequired;
        }
    }
}

pub(crate) fn apply_inventory_refresh_failure(app: &mut App, message: String) {
    app.plugins.loading = false;
    app.plugins.runtime_reload_after_refresh = false;
    app.plugins.pending_runtime_reload_success_message = None;
    // A check that could not refresh leaves no half-seen report behind.
    if let Some(run) = app.plugins.update_run.as_ref()
        && run.rows.iter().all(|row| row.status == PluginRunRowStatus::Queued)
    {
        app.plugins.update_run = None;
    }
    app.config.status_message = None;
    app.config.last_error = Some(message);
}

/// A manual refresh, check or inventory event dropped on a cwd
/// mismatch (the focused session moved mid-run): release the armed
/// loading flag without writing the other project's outcome into the
/// focused pane.
pub(crate) fn settle_dropped_refresh_failure(app: &mut App) {
    // Only an armed pane settles: a stale terminal on an idle pane
    // must not wipe a displayed status line or report.
    if !app.plugins.loading {
        return;
    }
    app.plugins.loading = false;
    app.config.status_message = None;
    app.plugins.runtime_reload_after_refresh = false;
    app.plugins.pending_runtime_reload_success_message = None;
}

/// Same release for a dropped run finish, plus the pane's unfinished
/// run copy: no event is left to finish it, and it is what latches
/// the `u`/`c`/`r` guard.
pub(crate) fn settle_dropped_manual_run(app: &mut App) {
    if !app.plugins.loading {
        return;
    }
    settle_dropped_refresh_failure(app);
    app.plugins.update_run = None;
}

pub(crate) fn reset_for_session_change(app: &mut App) {
    app.plugins.loading = false;
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
    app.plugins.installed_rows.clear();
    app.plugins.available_rows.clear();
    app.plugins.show_available = false;
    app.plugins.health.clear();
    app.plugins.token_costs.clear();
    app.plugins.lsp_commands.clear();
    app.plugins.restart_pending.clear();
    clamp_selection(app);
}

pub(crate) fn clamp_selection(app: &mut App) {
    for tab in ExtensionsTab::ALL {
        let len = visible_row_count(app, tab);
        let selected = app.plugins.selected_index_for(tab);
        app.plugins.set_selected_index_for(tab, clamp_index(selected, len));
    }
}

/// The rows one tab renders: the flattened extension rows for the
/// row-backed tabs, the MCP servers, or the marketplaces plus the
/// add row - each already filtered.
pub(crate) fn visible_row_count(app: &App, tab: ExtensionsTab) -> usize {
    match tab {
        ExtensionsTab::Mcps => app.mcp().servers.len(),
        ExtensionsTab::Marketplaces => app.plugins.marketplaces.len().saturating_add(1),
        _ => visible_rows(app, tab).len(),
    }
}

/// The rows a tab draws before filtering: the installed stream always,
/// plus the available stream's rows on component tabs behind the
/// Available toggle. The Installed tab never reveals the catalog - it
/// is the registry-backed tier alone.
pub(crate) fn tab_rows(app: &App, tab: ExtensionsTab) -> Vec<&ExtensionRow> {
    let mut rows = rows_for_tab(&app.plugins.installed_rows, tab);
    if tab_takes_available(tab) && app.plugins.show_available {
        rows.extend(rows_for_tab(&app.plugins.available_rows, tab));
    }
    rows
}

/// The tab's extension rows with the tab's filter applied.
pub(crate) fn visible_rows(app: &App, tab: ExtensionsTab) -> Vec<&ExtensionRow> {
    let query = app.plugins.search_query_for(tab);
    tab_rows(app, tab).into_iter().filter(|row| row_matches(row, &query)).collect()
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

/// Enter on the Installed tab resolves the row the pane RENDERS -
/// `visible_rows` positionally, matched back to its plugin by id - so
/// the overlay can never open a different plugin than the one
/// highlighted. A load-failed row states its reason instead of
/// opening an overlay nothing can act on.
fn open_installed_actions_overlay(app: &mut App) -> bool {
    let tab = ExtensionsTab::Installed;
    let selected = app.plugins.selected_index_for(tab);
    let Some(row) = visible_rows(app, tab).into_iter().nth(selected).cloned() else {
        return false;
    };
    if let RowState::LoadFailed(reason) = &row.state {
        // No overlay can act on a broken install; say why instead of
        // silently swallowing the keypress.
        app.config.last_error = Some(format!(
            "{name} failed to load ({reason}); fix the disk state first",
            name = row.name
        ));
        return true;
    }
    let Some(entry) = app.plugins.installed.iter().find(|entry| entry.id == row.id).cloned() else {
        return false;
    };
    open_installed_actions_for(app, entry)
}

/// The plugin action overlay for a known install.
pub(crate) fn open_installed_actions_for(app: &mut App, entry: InstalledPluginEntry) -> bool {
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

/// Install on an available row: the source plugin installs, so the
/// scope overlay opens for the row's owning plugin.
pub(crate) fn open_plugin_install_overlay(app: &mut App, plugin_id: &str) -> bool {
    app.config.overlay =
        Some(ConfigOverlayState::PluginInstallActions(PluginInstallOverlayState {
            plugin_id: plugin_id.to_owned(),
            title: display_label(plugin_id),
            description: "Install this plugin into Claude Code.".to_owned(),
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

    // An unhealthy marketplace offers Repair ahead of the rest: drift
    // or a failed manifest load is exactly what remove-and-re-add
    // fixes.
    let unhealthy =
        app.plugins.health.iter().any(|health| {
            health.name == entry.name && (health.drifted || health.load_error.is_some())
        });
    let mut actions = Vec::new();
    if unhealthy {
        actions.push(MarketplaceActionKind::Repair);
    }
    actions.push(MarketplaceActionKind::Update);
    actions.push(MarketplaceActionKind::Remove);

    app.config.overlay =
        Some(ConfigOverlayState::MarketplaceActions(MarketplaceActionsOverlayState {
            name: entry.name.clone(),
            title: display_label(&entry.name),
            description: marketplace_overlay_description(&entry),
            selected_index: 0,
            actions,
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

pub(crate) fn execute_selected_installed_overlay_action(app: &mut App) {
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

    // The uninstall confirm names the whole bundle before anything is
    // removed; the CLI has no per-component uninstall.
    if action == InstalledPluginActionKind::Uninstall {
        installed::open_uninstall_confirm(app, &overlay);
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

    // Repair is a pair: remove the drifted registry entry, then re-add
    // it from its source; only the trailing add carries the refresh.
    if action == MarketplaceActionKind::Repair {
        let entry = app
            .plugins
            .marketplaces
            .iter()
            .find(|marketplace| marketplace.name == overlay.name)
            .cloned();
        let Some(entry) = entry else {
            app.config.overlay = None;
            app.config.last_error =
                Some("No configured marketplace matches this repair".to_owned());
            return;
        };
        let (remove_args, add_args) = marketplace_repair_args(
            &entry.name,
            entry.source.as_deref(),
            entry.repo.as_deref(),
            entry.install_location.as_deref(),
        );
        let readd_source = match entry.source.as_deref() {
            Some("directory") => {
                entry.install_location.or(entry.repo).unwrap_or_else(|| entry.name.clone())
            }
            _ => entry.repo.clone().unwrap_or_else(|| entry.name.clone()),
        };

        app.config.overlay = None;
        app.config.last_error = None;
        app.config.status_message = Some(marketplace_action_status_message(&overlay.title, action));
        app.plugins.loading = true;
        app.plugins.last_inventory_refresh_at = None;
        app.needs_redraw = true;
        let event_tx = app.update_tx.clone();
        let cwd_raw = app.cwd_raw();
        let cwd_context = app.cwd_raw();
        let cached_claude_path = app.plugins.claude_path.clone();
        let title = overlay.title.clone();
        // The repair steps run through the same injectable seam the
        // update runs use, so both failure paths stay testable.
        let repair_cli = app.plugins.update_cli.clone().unwrap_or_else(UpdateCli::real);
        let span = info_span!(
            target: crate::logging::targets::APP_CONFIG,
            "plugin_marketplace_repair",
            cwd = %cwd_raw,
        );
        tokio::task::spawn_local(
            async move {
                let cli = repair_cli;
                // Remove first: a failure here changed nothing, so the
                // raw error is the whole story.
                let (path, _) = match (cli.run_update)(
                    cached_claude_path.clone(),
                    cwd_raw.clone(),
                    remove_args,
                )
                .await
                {
                    Ok(ok) => ok,
                    Err(message) => {
                        let _ = event_tx.send(SessionUpdate::PluginsCliActionFailed {
                            cwd_raw: cwd_context,
                            message: format!("The remove failed, nothing changed: {message}"),
                        });
                        return;
                    }
                };
                // The re-add is the step that can leave the
                // marketplace unregistered. Its failure refreshes the
                // pane FIRST, and the honest failure event lands last
                // so the follow-up refresh cannot erase it.
                match (cli.run_update)(Some(path.clone()), cwd_raw.clone(), add_args).await {
                    Ok((path, _)) => match (cli.refresh)(Some(path), cwd_raw).await {
                        Ok((snapshot, claude_path)) => {
                            let message = marketplace_action_success_message(&title, action);
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
                    },
                    Err(message) => {
                        let honest = format!(
                            "Removed {title} but the re-add failed; it is now unregistered - \
                             re-add with `claude plugin marketplace add {readd_source}`: {message}"
                        );
                        match (cli.refresh)(Some(path), cwd_raw).await {
                            Ok((snapshot, claude_path)) => {
                                let _ = event_tx.send(SessionUpdate::PluginsInventoryUpdated {
                                    cwd_raw: cwd_context.clone(),
                                    snapshot,
                                    claude_path,
                                });
                            }
                            Err(refresh_message) => {
                                let _ =
                                    event_tx.send(SessionUpdate::PluginsInventoryRefreshFailed {
                                        cwd_raw: cwd_context.clone(),
                                        message: refresh_message,
                                        trigger: PluginUpdateTrigger::Manual,
                                    });
                            }
                        }
                        let _ = event_tx.send(SessionUpdate::PluginsCliActionFailed {
                            cwd_raw: cwd_context,
                            message: honest,
                        });
                    }
                }
            }
            .instrument(span),
        );
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
    app.plugins.last_inventory_refresh_at = Some(Instant::now());
    app.plugins.claude_path = Some(result.claude_path);
    merge_token_costs(app, result.snapshot.token_costs);
    rebuild_rows(app, &result.snapshot.components, result.snapshot.marketplace_health);
    refresh_update_records(app);
    app.plugins.update_availability =
        update_availability(&app.plugins.installed, &app.plugins.marketplace);
    clamp_selection(app);
    start_runtime_reload(app, result.message);
}

/// Rebuild the render-side rows from a scan result: the rows and
/// health, the LSP command map for the annotation, then the
/// annotations themselves and the restart overrides.
fn rebuild_rows(app: &mut App, components: &[PluginComponents], health: Vec<MarketplaceHealth>) {
    app.plugins.lsp_commands = components
        .iter()
        .flat_map(|component| {
            component.lsp_servers.iter().map(move |(key, command)| {
                ((component.plugin.clone(), key.clone()), command.clone())
            })
        })
        .collect();
    // The scan carries installed and available plugins in one list;
    // the pane state must not. Registry-backed installs and
    // health-failure rows (a corrupt manifest must stay visible on the
    // page) form the installed stream; the marketplace catalog's
    // remaining entries form the available one.
    let (installed, available): (Vec<&PluginComponents>, Vec<&PluginComponents>) =
        components.iter().partition(|entry| entry.installed || entry.load_error.is_some());
    app.plugins.installed_rows = extension_rows(installed);
    app.plugins.available_rows = extension_rows(available);
    app.plugins.health = health;
    annotate_rows(app);
    apply_restart_overrides(app);
}

pub(crate) fn apply_cli_action_failure(app: &mut App, message: String) {
    app.plugins.loading = false;
    app.plugins.pending_runtime_reload_success_message = None;
    app.config.status_message = None;
    app.config.last_error = Some(message);
}

pub(crate) fn apply_runtime_reload_success(app: &mut App) {
    app.plugins.loading = false;
    // A reload is what consumes a pending restart contract.
    app.plugins.restart_pending.clear();
    apply_restart_overrides(app);
    if let Some(message) = app.plugins.pending_runtime_reload_success_message.take() {
        app.config.last_error = None;
        app.config.status_message = Some(message);
    }
}

pub(crate) fn apply_runtime_reload_failure(app: &mut App, message: &str) {
    app.plugins.loading = false;
    app.plugins.pending_runtime_reload_success_message = None;
    app.config.status_message = None;
    app.config.last_error = Some(format!("Failed to reload session plugins: {message}"));
}

fn start_runtime_reload(app: &mut App, success_message: String) {
    app.plugins.loading = true;
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
    if trigger == PluginUpdateTrigger::Manual {
        // Update all queues exactly the stale set - the rows wearing
        // the Update action - not every installed plugin.
        use forge_primitives::plugins::{ExtensionKind, RowState};
        let stale: Vec<&str> = app
            .plugins
            .installed_rows
            .iter()
            .filter(|row| {
                row.kind == ExtensionKind::Plugin && row.state == RowState::UpdateAvailable
            })
            .map(|row| row.id.as_str())
            .collect();
        let entries = app
            .plugins
            .installed
            .iter()
            .filter(|entry| stale.contains(&entry.id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        return build_rows_from_entries(&entries, &cwd, trigger);
    }
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
type DetailsResult =
    std::collections::BTreeMap<String, (String, forge_primitives::plugins::PluginDetails)>;
type SharedDetailsFn = std::sync::Arc<
    dyn Fn(Option<PathBuf>, String, Vec<(String, String)>) -> UpdateCliFut<DetailsResult>,
>;
type RollbackResult = Result<PluginRollbackOutcome, String>;
type SharedRollbackFn = std::sync::Arc<
    dyn Fn(Option<PathBuf>, String, PluginUpdateRecord, String) -> UpdateCliFut<RollbackResult>,
>;

/// Per-run CLI surface, injectable so tests drive a whole run without
/// shelling out. The production instance wraps the `claude` subprocess
/// calls; the refresh and details arms route through it too, so the
/// paint-ordering property (inventory event first, costs after) is
/// testable.
#[derive(Clone)]
pub(crate) struct UpdateCli {
    run_update: SharedUpdateFn,
    refresh: SharedRefreshFn,
    details: SharedDetailsFn,
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
            details: std::sync::Arc::new(|cached, cwd, requests| {
                Box::pin(cli::fetch_plugin_details(cwd, cached, requests))
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

impl UpdateCli {
    /// The batch runner view of this seam: the update and refresh
    /// arms, without rollback.
    fn runner(&self) -> cli::UpdateRunner {
        cli::UpdateRunner {
            run_update: std::sync::Arc::clone(&self.run_update),
            refresh: std::sync::Arc::clone(&self.refresh),
        }
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
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for plugin action".to_owned());
        return;
    }
    if app.plugins.loading || app.plugins.update_run.as_ref().is_some_and(|run| !run.finished) {
        return;
    }
    let rows = build_update_rows(app, trigger);
    if rows.is_empty() {
        app.config.last_error = None;
        app.config.status_message = Some("No installed plugins are eligible for update".to_owned());
        return;
    }
    let runnable = rows.iter().filter(|row| row.status == PluginRunRowStatus::Queued).count();
    let run = PluginUpdateRun { trigger, finished: false, rows };
    app.plugins.update_run = Some(run.clone());
    app.plugins.loading = true;
    app.config.last_error = None;
    app.config.status_message = Some(format!("Updating {runnable} plugin(s)..."));
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
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for plugin action".to_owned());
        return;
    }
    if app.plugins.loading || app.plugins.update_run.as_ref().is_some_and(|run| !run.finished) {
        return;
    }
    app.plugins.loading = true;
    app.config.last_error = None;
    app.config.status_message = Some("Checking for plugin updates...".to_owned());
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

async fn execute_update_plan(update_tx: mpsc::UnboundedSender<SessionUpdate>, plan: UpdateRunPlan) {
    let refs = capture_marketplace_refs(&plan.marketplaces).await;
    let batch_plan = cli::BatchPlan {
        cwd_context: plan.cwd_context.clone(),
        claude_path: plan.claude_path.clone(),
        call_timeout: UPDATE_CALL_TIMEOUT,
        marketplace_refs: refs,
        run: plan.run,
    };
    let progress_tx = update_tx.clone();
    let progress_cwd = plan.cwd_context.clone();
    let out = cli::execute_update_batch(batch_plan, plan.cli.runner(), move |run| {
        let _ = progress_tx
            .send(SessionUpdate::PluginsUpdateRunProgress { cwd_raw: progress_cwd.clone(), run });
    })
    .await;

    // Persist in the task: the report event can be dropped on a cwd
    // mismatch, the record must not be.
    if !out.records.is_empty()
        && let Some(store) = plan.store.as_ref()
    {
        store.record_plugin_updates(&out.records);
    }

    let _ = update_tx.send(SessionUpdate::PluginsUpdateRunFinished {
        cwd_raw: plan.cwd_context,
        run: out.run,
        snapshot: out.snapshot,
        claude_path: out.claude_path,
    });
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
        // The pane's footer renders the config feedback pair.
        let message = "No recorded previous version for this plugin";
        app.config.status_message = None;
        app.config.last_error = Some(message.to_owned());
        return;
    };
    if record.marketplace_ref_before.is_none() {
        let message = "No pre-update marketplace ref was captured; rollback is unavailable";
        app.config.status_message = None;
        app.config.last_error = Some(message.to_owned());
        return;
    }
    let install_location = app
        .plugins
        .marketplaces
        .iter()
        .find(|marketplace| marketplace.name == record.marketplace)
        .and_then(|marketplace| marketplace.install_location.clone());
    let Some(install_location) = install_location else {
        let message = "Rollback needs a git-backed marketplace clone; none found";
        app.config.status_message = None;
        app.config.last_error = Some(message.to_owned());
        return;
    };
    if tokio::runtime::Handle::try_current().is_err() {
        app.config.status_message = None;
        app.config.last_error = Some("No runtime available for plugin action".to_owned());
        return;
    }
    if app.plugins.loading {
        return;
    }
    let label = display_label(&plugin_id);
    let to_version = record.from_version.clone().unwrap_or_else(|| "previous version".to_owned());
    app.config.overlay = None;
    app.plugins.loading = true;
    app.config.last_error = None;
    app.config.status_message = Some(format!("Rolling back {label} to {to_version}..."));
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
        // A failed post-run refresh leaves the PRE-run rows on screen;
        // the markers describe that same data, so keep them rather than
        // dropping the pane into a markerless gap until reopen.
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
        merge_token_costs(app, snapshot.token_costs);
        rebuild_rows(app, &snapshot.components, snapshot.marketplace_health);
        app.plugins.last_inventory_refresh_at = Some(Instant::now());
        clamp_selection(app);
    }
    // The restart contract rides the row's captured output; the state
    // overrides re-stamp it on the pane after every rows rebuild. Only
    // an APPLIED update states the contract - a failed row whose prose
    // happens to contain the phrase must not stamp.
    for row in &run.rows {
        if row.status == PluginRunRowStatus::Updated
            && row.detail.as_deref().is_some_and(|detail| {
                forge_workspace::userdata::plugins::cli::output_states_restart_required(detail)
            })
        {
            app.plugins.restart_pending.insert(row.plugin_id.clone());
        }
    }
    apply_restart_overrides(app);
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
    // themselves, so a recorded failure does not outlive a successful
    // run.
    if let Some(message) = summary {
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
    merge_token_costs(app, snapshot.token_costs);
    rebuild_rows(app, &snapshot.components, snapshot.marketplace_health);
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
        merge_token_costs(app, snapshot.token_costs);
        rebuild_rows(app, &snapshot.components, snapshot.marketplace_health);
        app.plugins.last_inventory_refresh_at = Some(Instant::now());
        app.plugins.update_availability =
            update_availability(&app.plugins.installed, &app.plugins.marketplace);
        clamp_selection(app);
    }
    app.plugins.loading = false;
    app.config.status_message = None;
    let failure = format!("Rollback of {} failed: {message}", display_label(plugin_id));
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
        // Repair dispatches through marketplace_repair_args before
        // this builder runs; the empty plan is unreachable.
        MarketplaceActionKind::Repair => Vec::new(),
    }
}

/// The remove-and-re-add pair for a marketplace repair, in order. The
/// re-add source is the repo for a git marketplace and the clone
/// location for a directory one.
fn marketplace_repair_args(
    name: &str,
    source: Option<&str>,
    repo: Option<&str>,
    install_location: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let remove =
        vec!["plugin".to_owned(), "marketplace".to_owned(), "remove".to_owned(), name.to_owned()];
    let readd_source = match source {
        Some("directory") => install_location.or(repo).unwrap_or(name),
        _ => repo.unwrap_or(name),
    };
    let add = vec![
        "plugin".to_owned(),
        "marketplace".to_owned(),
        "add".to_owned(),
        readd_source.to_owned(),
        "--scope".to_owned(),
        "user".to_owned(),
    ];
    (remove, add)
}

fn marketplace_action_status_message(title: &str, action: MarketplaceActionKind) -> String {
    match action {
        MarketplaceActionKind::Update => format!("Updating {title} marketplace..."),
        MarketplaceActionKind::Remove => format!("Removing {title} marketplace..."),
        MarketplaceActionKind::Repair => {
            format!("Repairing {title} marketplace (remove and re-add)...")
        }
    }
}

fn marketplace_action_success_message(title: &str, action: MarketplaceActionKind) -> String {
    match action {
        MarketplaceActionKind::Update => format!("Updated {title} marketplace"),
        MarketplaceActionKind::Remove => format!("Removed {title} marketplace"),
        MarketplaceActionKind::Repair => format!("Repaired {title} marketplace"),
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

/// The component row the active component tab has selected, when it is
/// an extension row at all (MCPs and marketplaces select elsewhere).
pub(crate) fn selected_extension_row(app: &App) -> Option<&ExtensionRow> {
    let tab = app.plugins.active_tab;
    visible_rows(app, tab).into_iter().nth(app.plugins.selected_index_for(tab))
}

fn selected_marketplace_source(app: &App) -> Option<&MarketplaceSourceEntry> {
    let index = app.plugins.selected_index_for(ExtensionsTab::Marketplaces);
    visible_marketplaces(&app.plugins).get(index).copied()
}

fn selected_add_marketplace_row(app: &App) -> bool {
    app.plugins.selected_index_for(ExtensionsTab::Marketplaces)
        >= visible_marketplaces(&app.plugins).len()
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
    let len = visible_row_count(app, tab);
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

pub(crate) const fn search_enabled(tab: ExtensionsTab) -> bool {
    tab.filters_rows()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::model;
    use crate::app::events::apply_session_update;
    use forge_primitives::plugins::{ExtensionKind, PluginCapability, RowState};
    use forge_workspace::{DictateOutcome, SessionKey};

    fn plugins_view_with_live_take() -> (crate::app::App, SessionKey) {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        app.active_view = crate::app::ActiveView::Extensions;
        app.plugins.active_tab = ExtensionsTab::Installed;
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
        assert_eq!(app.plugins.search_query_for(ExtensionsTab::Installed), "retry guard");
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
            app.plugins.search_query_for(ExtensionsTab::Installed),
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
        app.plugins.set_selected_index_for(ExtensionsTab::Installed, 2);

        apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key,
                generation: 1,
                outcome: DictateOutcome::Landed { text: "sample".to_owned(), truncated: false },
            },
        );

        assert_eq!(app.plugins.search_query_for(ExtensionsTab::Installed), "sample");
        assert_eq!(
            app.plugins.selected_index_for(ExtensionsTab::Installed),
            0,
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
            crate::app::ActiveView::Extensions,
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
        app.active_view = crate::app::ActiveView::Extensions;
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
        app.active_view = crate::app::ActiveView::Extensions;
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

    #[test]
    fn repair_builds_a_remove_then_readd_pair_from_the_registry_entry() {
        let (remove, add) = marketplace_repair_args(
            "claude-night-market",
            Some("github"),
            Some("athola/claude-night-market"),
            Some("/clone/path"),
        );
        assert_eq!(
            remove,
            vec![
                "plugin".to_owned(),
                "marketplace".to_owned(),
                "remove".to_owned(),
                "claude-night-market".to_owned()
            ],
            "the remove step names the marketplace"
        );
        assert_eq!(
            add,
            vec![
                "plugin".to_owned(),
                "marketplace".to_owned(),
                "add".to_owned(),
                "athola/claude-night-market".to_owned(),
                "--scope".to_owned(),
                "user".to_owned(),
            ],
            "a git marketplace re-adds from its repo"
        );

        // A directory marketplace re-adds from its clone location.
        let (directory_remove, directory_add) = marketplace_repair_args(
            "stx-clarity",
            Some("directory"),
            None,
            Some("/Users/vedhavyas/.claude/plugins/marketplaces/stx-clarity"),
        );
        assert_eq!(directory_remove.len(), 4);
        assert!(
            directory_add
                .contains(&"/Users/vedhavyas/.claude/plugins/marketplaces/stx-clarity".to_owned()),
            "the directory path is the re-add source: {directory_add:?}"
        );
    }

    fn plugin_row(id: &str, state: RowState) -> ExtensionRow {
        ExtensionRow {
            id: id.to_owned(),
            kind: ExtensionKind::Plugin,
            name: id.split('@').next().unwrap_or(id).to_owned(),
            source: id.split('@').nth(1).unwrap_or_default().to_owned(),
            version: Some("1.0.0".to_owned()),
            available_version: None,
            state,
            detail: None,
        }
    }

    /// Enter on the Installed tab opens the plugin the RENDERED row
    /// carries, not a positional lookup into a differently-ordered
    /// list; past-the-end stays a no-op instead of closing the page.
    #[test]
    fn installed_tab_enter_opens_the_rendered_rows_plugin() {
        let mut app = App::test_default();
        app.plugins.installed = vec![
            InstalledPluginEntry {
                id: "alpha@probe".to_owned(),
                version: Some("1.0.0".to_owned()),
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: PluginCapability::Skill,
            },
            InstalledPluginEntry {
                id: "beta@probe".to_owned(),
                version: Some("1.0.0".to_owned()),
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: PluginCapability::Skill,
            },
        ];
        app.plugins.installed_rows = vec![
            plugin_row("beta@probe", RowState::Current),
            plugin_row("alpha@probe", RowState::Current),
        ];
        app.plugins.set_selected_index_for(ExtensionsTab::Installed, 0);

        assert!(open_installed_actions_overlay(&mut app));
        let overlay = app.config.installed_plugin_actions_overlay().expect("the overlay opens");
        assert_eq!(
            overlay.plugin_id, "beta@probe",
            "the first rendered row IS beta; a CLI-order lookup would open alpha"
        );

        // Past the end: a no-op, the view stays open.
        app.config.overlay = None;
        app.plugins.set_selected_index_for(ExtensionsTab::Installed, 5);
        assert!(!open_installed_actions_overlay(&mut app));
        assert!(app.config.overlay.is_none());
    }

    /// The streams stay separate: an available-not-installed plugin is
    /// catalog, not install - it never renders on the Installed tab,
    /// and Enter there can never open an install overlay for it. The
    /// catalog's install seam is the component rows behind the
    /// Available toggle.
    #[test]
    fn the_installed_tab_never_renders_the_available_stream() {
        let mut app = App::test_default();
        app.plugins.installed.push(InstalledPluginEntry {
            id: "superpowers@probe".to_owned(),
            version: Some("6.3.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        });
        app.plugins.installed_rows = vec![plugin_row("superpowers@probe", RowState::Current)];
        app.plugins.available_rows =
            vec![plugin_row("gone@claude-night-market", RowState::AvailableNotInstalled)];
        app.plugins.set_selected_index_for(ExtensionsTab::Installed, 0);

        assert_eq!(
            visible_rows(&app, ExtensionsTab::Installed)
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["superpowers@probe"],
            "the catalog row stays off the Installed tab"
        );
        assert!(open_installed_actions_overlay(&mut app));
        assert!(
            app.config.installed_plugin_actions_overlay().is_some(),
            "Enter opens the installed plugin's actions, not an install overlay"
        );

        // Behind the toggle, the Skills tab carries the catalog rows.
        assert!(visible_rows(&app, ExtensionsTab::Skills).is_empty());
        app.plugins.available_rows.push(ExtensionRow {
            id: "skill:gone:ghost-skill".to_owned(),
            kind: ExtensionKind::Skill,
            name: "ghost-skill".to_owned(),
            source: "gone@claude-night-market".to_owned(),
            version: None,
            available_version: Some("1.0.0".to_owned()),
            state: RowState::AvailableNotInstalled,
            detail: None,
        });
        app.plugins.show_available = true;
        assert_eq!(
            visible_rows(&app, ExtensionsTab::Skills)
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["skill:gone:ghost-skill"],
            "the toggle reveals the available stream on component tabs"
        );
        app.plugins.show_available = false;
        assert!(visible_rows(&app, ExtensionsTab::Skills).is_empty());
    }

    /// `a` flips the Available toggle on a component tab and resets the
    /// selection; on the Installed tab it does nothing - the catalog
    /// has no seam there.
    #[test]
    fn the_available_toggle_key_flips_only_component_tabs() {
        let mut app = app_with_focused_search(ExtensionsTab::Skills);
        app.plugins.search_focused = false;
        app.plugins.installed_rows = vec![plugin_row("superpowers@probe", RowState::Current)];
        app.plugins.set_selected_index_for(ExtensionsTab::Skills, 0);

        assert!(press(&mut app, KeyCode::Char('a')));
        assert!(app.plugins.show_available, "a reveals the available stream");
        assert_eq!(app.plugins.selected_index_for(ExtensionsTab::Skills), 0);

        app.plugins.active_tab = ExtensionsTab::Installed;
        app.plugins.show_available = false;
        assert!(press(&mut app, KeyCode::Char('a')));
        assert!(!app.plugins.show_available, "the Installed tab has no Available toggle");
    }

    /// The scan's `installed` flag decides the stream: a refresh lands
    /// registry-backed plugins (and their components) in the installed
    /// stream and the catalog's entries in the available one - the
    /// pane state never merges them back into one list.
    #[test]
    fn a_refresh_splits_the_scan_into_two_streams() {
        let mut app = App::test_default();
        let component =
            |plugin: &str, market: &str, name: &str, installed: bool| PluginComponents {
                plugin: format!("{plugin}@{market}"),
                marketplace: market.to_owned(),
                version: Some("1.0.0".to_owned()),
                installed,
                enabled: installed,
                auto: false,
                available_version: None,
                skills: vec![name.to_owned()],
                ..PluginComponents::default()
            };
        apply_inventory_refresh_success(
            &mut app,
            PluginsInventorySnapshot {
                components: vec![
                    component("superpowers", "probe", "brainstorming", true),
                    component("blabbermouth", "claude-night-market", "announce", false),
                    // The corrupt-manifest marker: not installed, but a
                    // health failure the page must surface.
                    PluginComponents {
                        plugin: "torn-market".to_owned(),
                        marketplace: "torn-market".to_owned(),
                        load_error: Some(
                            "marketplace.json failed to parse: invalid type".to_owned(),
                        ),
                        ..PluginComponents::default()
                    },
                ],
                ..PluginsInventorySnapshot::default()
            },
            PathBuf::new(),
        );

        assert_eq!(
            app.plugins.installed_rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["superpowers@probe", "skill:superpowers:brainstorming", "torn-market",],
            "the installed plugin, its skill AND the health-failure row land in the installed stream"
        );
        assert_eq!(
            app.plugins.available_rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec!["blabbermouth@claude-night-market", "skill:blabbermouth:announce"],
            "the catalog entry lands in the available stream: {:?}",
            app.plugins.available_rows
        );
        assert!(
            visible_rows(&app, ExtensionsTab::Installed).iter().any(|row| row.id == "torn-market"),
            "the health-failure row renders on the Installed tab"
        );
    }

    /// A session change clears BOTH row streams and the Available
    /// toggle: one session's catalog and toggle state must not leak
    /// into the next session's page.
    #[test]
    fn a_session_change_resets_the_streams_and_the_toggle() {
        let mut app = App::test_default();
        app.plugins.installed_rows = vec![plugin_row("superpowers@probe", RowState::Current)];
        app.plugins.available_rows =
            vec![plugin_row("gone@claude-night-market", RowState::AvailableNotInstalled)];
        app.plugins.show_available = true;

        reset_for_session_change(&mut app);

        assert!(app.plugins.installed_rows.is_empty(), "installed rows cleared");
        assert!(app.plugins.available_rows.is_empty(), "available rows cleared");
        assert!(!app.plugins.show_available, "the toggle resets");
    }

    /// Enter on a load-failed plugin states the failure instead of
    /// opening an overlay nothing can act on - no overlay may open.
    #[test]
    fn enter_on_a_load_failed_row_names_the_reason_and_opens_nothing() {
        let mut app = App::test_default();
        app.plugins.installed_rows = vec![plugin_row(
            "ghosted@probe",
            RowState::LoadFailed("registered install dir is missing on disk".to_owned()),
        )];
        app.plugins.set_selected_index_for(ExtensionsTab::Installed, 0);

        assert!(open_installed_actions_overlay(&mut app));
        let error = app.config.last_error.as_deref().expect("the failure surfaces");
        assert!(
            error.contains("ghosted")
                && error.contains("registered install dir is missing on disk"),
            "the error names the plugin and the reason: {error}"
        );
        assert!(
            app.config.overlay.is_none(),
            "no overlay opens for a broken install: {:?}",
            app.config.overlay
        );
    }

    /// The token cost rides the plugin row's detail; the cost-only
    /// event merges into the cache without touching any list.
    #[test]
    fn a_costs_only_event_merges_costs_and_annotates_the_rows() {
        let mut app = App::test_default();
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "superpowers@probe".to_owned(),
            version: Some("6.3.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        app.plugins.installed_rows = vec![plugin_row("superpowers@probe", RowState::Current)];

        apply_inventory_refresh_success(
            &mut app,
            PluginsInventorySnapshot {
                token_costs: std::collections::BTreeMap::from([(
                    "superpowers@probe".to_owned(),
                    (
                        "6.3.0".to_owned(),
                        forge_primitives::plugins::PluginDetails { token_cost_always_on: 450 },
                    ),
                )]),
                ..PluginsInventorySnapshot::default()
            },
            PathBuf::new(),
        );

        assert_eq!(app.plugins.installed.len(), 1, "the lists survive a costs-only event");
        assert!(!app.plugins.loading, "the costs-only landing does not touch the pane");
        assert_eq!(
            app.plugins.installed_rows[0].detail.as_deref(),
            Some("~450 tok always-on"),
            "the cost renders on the plugin row: {:?}",
            app.plugins.installed_rows[0].detail
        );
    }

    /// An applied update that stated the restart contract holds the
    /// RestartRequired state on the pane until a reload consumes it.
    #[test]
    fn a_restart_contract_holds_until_a_reload_consumes_it() {
        let mut app = App::test_default();
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "supabase@probe".to_owned(),
            version: Some("2.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        app.plugins.installed_rows = vec![plugin_row("supabase@probe", RowState::Current)];
        let mut row = PluginUpdateRunRow::queued(
            "supabase@probe".to_owned(),
            "user".to_owned(),
            String::new(),
            Some("1.0.0".to_owned()),
        );
        row.status = PluginRunRowStatus::Updated;
        row.installed_version = Some("2.0.0".to_owned());
        row.detail = Some("restart required to apply".to_owned());
        let run = PluginUpdateRun {
            // Auto: the boot arm skips the runtime reload, so the
            // contract holds on the pane - a live session's reload is
            // what consumes it.
            trigger: PluginUpdateTrigger::Auto,
            finished: true,
            rows: vec![row],
        };

        apply_update_run_finished(&mut app, &run, None, None);
        assert_eq!(
            app.plugins.installed_rows[0].state,
            RowState::RestartRequired,
            "the pane states the contract where the action was"
        );

        // A successful runtime reload consumes it.
        app.plugins.pending_runtime_reload_success_message = Some("done".to_owned());
        apply_runtime_reload_success(&mut app);
        assert!(app.plugins.restart_pending.is_empty());
        assert_eq!(app.plugins.installed_rows[0].state, RowState::Current);
    }

    /// A FAILED row whose prose happens to contain the restart phrase
    /// must not stamp the contract - only an applied update states it.
    #[test]
    fn a_failed_row_never_stamps_the_restart_contract() {
        let mut app = App::test_default();
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "supabase@probe".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        app.plugins.installed_rows = vec![plugin_row("supabase@probe", RowState::Current)];
        let mut row = PluginUpdateRunRow::queued(
            "supabase@probe".to_owned(),
            "user".to_owned(),
            String::new(),
            Some("1.0.0".to_owned()),
        );
        row.status = PluginRunRowStatus::Failed;
        row.detail = Some("update aborted: restart required by the harness".to_owned());
        let run =
            PluginUpdateRun { trigger: PluginUpdateTrigger::Auto, finished: true, rows: vec![row] };

        apply_update_run_finished(&mut app, &run, None, None);

        assert!(
            !app.plugins.restart_pending.contains("supabase@probe"),
            "a failed row's prose is not the contract: {:?}",
            app.plugins.restart_pending
        );
        assert_eq!(app.plugins.installed_rows[0].state, RowState::Current);
    }

    /// The stamp survives a rows REBUILD: an action between the stamped
    /// restart and its reload re-derives rows from a Current scan, and
    /// the pending set re-stamps them. The live-session stub keeps the
    /// reload REQUESTED (not instantly consumed) for the window.
    #[test]
    fn a_restart_stamp_survives_a_rows_rebuild() {
        let (mut app, mut rx) = app_with_connection();
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "supabase@probe".to_owned(),
            version: Some("2.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        app.plugins.installed_rows = vec![plugin_row("supabase@probe", RowState::Current)];
        let mut row = PluginUpdateRunRow::queued(
            "supabase@probe".to_owned(),
            "user".to_owned(),
            String::new(),
            Some("1.0.0".to_owned()),
        );
        row.status = PluginRunRowStatus::Updated;
        row.installed_version = Some("2.0.0".to_owned());
        row.detail = Some("restart required to apply".to_owned());
        let run =
            PluginUpdateRun { trigger: PluginUpdateTrigger::Auto, finished: true, rows: vec![row] };
        apply_update_run_finished(&mut app, &run, None, None);
        assert_eq!(app.plugins.installed_rows[0].state, RowState::RestartRequired);

        // Drain the run's reload request so the window is clean.
        while rx.try_recv().is_ok() {}

        // A successful action between the stamp and the reload applies
        // a fresh snapshot whose scan reads Current.
        let installed = app.plugins.installed.clone();
        apply_cli_action_success(
            &mut app,
            PluginsCliActionSuccess {
                snapshot: PluginsInventorySnapshot {
                    installed,
                    components: vec![forge_primitives::plugins::PluginComponents {
                        plugin: "supabase@probe".to_owned(),
                        installed: true,
                        enabled: true,
                        ..forge_primitives::plugins::PluginComponents::default()
                    }],
                    ..PluginsInventorySnapshot::default()
                },
                message: "Enabled".to_owned(),
                claude_path: PathBuf::new(),
            },
        );

        assert_eq!(
            app.plugins.installed_rows[0].state,
            RowState::RestartRequired,
            "the stamp re-derives across the rebuild: {:?}",
            app.plugins.installed_rows[0].state
        );
        assert!(app.plugins.restart_pending.contains("supabase@probe"));
    }

    /// The manual `r` carries the same in-flight guard the `u` key
    /// has: it must not stack behind an unfinished run.
    #[test]
    fn a_manual_refresh_refuses_while_a_run_is_in_flight() {
        let mut app = App::test_default();
        app.plugins.update_run = Some(PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: false,
            rows: vec![],
        });

        request_inventory_refresh_manual(&mut app);

        assert!(!app.plugins.loading, "the guard refuses, nothing starts");
        assert!(app.plugins.update_run.is_some(), "the running run stands");
        assert!(
            app.config.last_error.is_none() && app.config.status_message.is_none(),
            "the refusal is silent, like the u guard: {:?}",
            app.config.last_error
        );
    }

    /// Outside a tokio runtime the manual refresh names why it did
    /// nothing, on the footer pair the pane renders.
    #[test]
    fn a_manual_refresh_outside_a_runtime_surfaces_the_error() {
        let mut app = App::test_default();

        request_inventory_refresh(&mut app);

        assert!(!app.plugins.loading);
        assert_eq!(
            app.config.last_error.as_deref(),
            Some("No runtime available for plugin action")
        );
    }

    /// The LSP check tests the manifest's declared COMMAND, not the
    /// server key; the key is the fallback when no command is known.
    /// The PATH is injected, so the test is hermetic.
    #[test]
    fn the_lsp_check_tests_the_declared_command() {
        use std::os::unix::fs::PermissionsExt;
        let mut app = App::test_default();
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("probe-ls");
        std::fs::write(&binary, b"#!/bin/sh").expect("write binary");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let injected = dir.path().to_string_lossy().into_owned();

        let lsp_row = |id: &str, name: &str| ExtensionRow {
            id: id.to_owned(),
            kind: ExtensionKind::Lsp,
            name: name.to_owned(),
            source: "lsp-plugin@probe".to_owned(),
            version: Some("1.0.0".to_owned()),
            available_version: None,
            state: RowState::Current,
            detail: None,
        };
        app.plugins.installed_rows = vec![lsp_row("lsp:lsp-plugin:probe-ls", "probe-ls")];
        // A RELATIVE command: only the injected PATH can find it.
        app.plugins
            .lsp_commands
            .insert(("lsp-plugin@probe".to_owned(), "probe-ls".to_owned()), "probe-ls".to_owned());

        annotate_rows_on_path(&mut app, Some(injected.as_str()));

        assert_eq!(
            app.plugins.installed_rows[0].detail.as_deref(),
            Some("probe-ls: on PATH"),
            "the declared command is what was probed against the injected PATH: {:?}",
            app.plugins.installed_rows[0].detail
        );

        // No declared command: the key is the fallback - and this key
        // is NOT in the injected PATH, so it reads missing.
        app.plugins.lsp_commands.clear();
        app.plugins.installed_rows[0].name = "uninjected-server".to_owned();
        app.plugins.installed_rows[0].id = "lsp:lsp-plugin:uninjected-server".to_owned();
        annotate_rows_on_path(&mut app, Some(injected.as_str()));
        assert_eq!(
            app.plugins.installed_rows[0].detail.as_deref(),
            Some("uninjected-server: missing"),
            "the key falls back when no command is declared: {:?}",
            app.plugins.installed_rows[0].detail
        );
    }

    /// The cost cache stamps the version the cost was fetched FOR:
    /// an update landing between the request and the merge must not
    /// pin the old cost under the new version's key, or the stale cost
    /// reads fresh forever.
    #[test]
    fn a_cost_stamps_the_version_it_was_fetched_for() {
        let mut app = App::test_default();
        // The update already landed: the pane's install reads 2.0.0.
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "superpowers@probe".to_owned(),
            version: Some("2.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        app.plugins.installed_rows = vec![plugin_row("superpowers@probe", RowState::Current)];

        apply_inventory_refresh_success(
            &mut app,
            PluginsInventorySnapshot {
                token_costs: std::collections::BTreeMap::from([(
                    "superpowers@probe".to_owned(),
                    (
                        "1.0.0".to_owned(),
                        forge_primitives::plugins::PluginDetails { token_cost_always_on: 450 },
                    ),
                )]),
                ..PluginsInventorySnapshot::default()
            },
            PathBuf::new(),
        );

        assert_eq!(
            app.plugins.token_costs.get("superpowers@probe").map(|(version, _)| version.as_str()),
            Some("1.0.0"),
            "the cached version is the FETCHED one, so the next refresh refetches: {:?}",
            app.plugins.token_costs
        );
    }

    /// A repair fake: the remove step fails when `fail_remove` is
    /// set, the add step when `fail_add` is; both record their calls.
    fn repair_cli(
        fail_remove: bool,
        fail_add: bool,
        calls: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> UpdateCli {
        let remove_calls = std::sync::Arc::clone(calls);
        let add_calls = std::sync::Arc::clone(calls);
        let refresh_calls = std::sync::Arc::clone(calls);
        UpdateCli {
            run_update: std::sync::Arc::new(move |_cached, _cwd, args| {
                let failing = if args.contains(&"remove".to_owned()) {
                    let calls = std::sync::Arc::clone(&remove_calls);
                    calls.lock().expect("calls").push("remove".to_owned());
                    fail_remove
                } else {
                    let calls = std::sync::Arc::clone(&add_calls);
                    calls.lock().expect("calls").push("add".to_owned());
                    fail_add
                };
                let output = if failing {
                    Err("claude exited 1:boom".to_owned())
                } else {
                    Ok((std::path::PathBuf::from("claude"), "ok".to_owned()))
                };
                Box::pin(async move { output })
            }),
            refresh: std::sync::Arc::new(move |_cached, _cwd| {
                let calls = std::sync::Arc::clone(&refresh_calls);
                Box::pin(async move {
                    calls.lock().expect("calls").push("refresh".to_owned());
                    Ok((PluginsInventorySnapshot::default(), std::path::PathBuf::from("claude")))
                })
            }),
            details: std::sync::Arc::new(|_, _, _| {
                Box::pin(async { std::collections::BTreeMap::new() })
            }),
            rollback: std::sync::Arc::new(|_, _, _, _| {
                Box::pin(async { Ok(PluginRollbackOutcome::RolledBack) })
            }),
        }
    }

    /// A remove-step failure surfaces the raw error, claims nothing
    /// about the re-add, and leaves the marketplace registered.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_remove_reports_nothing_changed() {
        let mut app = App::test_default();
        app.plugins.marketplaces = vec![MarketplaceSourceEntry {
            name: "probe".to_owned(),
            source: Some("github".to_owned()),
            repo: Some("a/probe".to_owned()),
            install_location: None,
        }];
        let calls = call_log();
        app.plugins.update_cli = Some(repair_cli(true, false, &calls));
        app.config.overlay = Some(ConfigOverlayState::MarketplaceActions(
            crate::app::config::MarketplaceActionsOverlayState {
                name: "probe".to_owned(),
                title: "Probe".to_owned(),
                description: String::new(),
                selected_index: 0,
                actions: vec![MarketplaceActionKind::Repair],
            },
        ));

        tokio::task::LocalSet::new()
            .run_until(async {
                execute_selected_marketplace_action(&mut app);
                for _ in 0..50 {
                    tokio::task::yield_now().await;
                }
            })
            .await;

        let events = drain_plugin_events(&mut app);
        assert_eq!(events.len(), 1, "no refresh follows a remove failure: {events:?}");
        assert!(
            matches!(&events[0], SessionUpdate::PluginsCliActionFailed { message, .. }
                if message.contains("The remove failed, nothing changed")
                    && message.contains("boom")
                    && !message.contains("unregistered")),
            "the raw error is the whole story: {events:?}"
        );
        assert_eq!(*calls.lock().expect("calls"), vec!["remove".to_owned()]);
    }

    /// A re-add failure after a successful remove refreshes the pane
    /// and lands the honest "unregistered" message LAST, so the
    /// follow-up refresh cannot erase it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_readd_reports_unregistered_after_the_refresh() {
        let mut app = App::test_default();
        app.plugins.marketplaces = vec![MarketplaceSourceEntry {
            name: "probe".to_owned(),
            source: Some("github".to_owned()),
            repo: Some("a/probe".to_owned()),
            install_location: None,
        }];
        let calls = call_log();
        app.plugins.update_cli = Some(repair_cli(false, true, &calls));
        app.config.overlay = Some(ConfigOverlayState::MarketplaceActions(
            crate::app::config::MarketplaceActionsOverlayState {
                name: "probe".to_owned(),
                title: "Probe".to_owned(),
                description: String::new(),
                selected_index: 0,
                actions: vec![MarketplaceActionKind::Repair],
            },
        ));

        tokio::task::LocalSet::new()
            .run_until(async {
                execute_selected_marketplace_action(&mut app);
                for _ in 0..50 {
                    tokio::task::yield_now().await;
                }
            })
            .await;

        let events = drain_plugin_events(&mut app);
        assert_eq!(events.len(), 2, "refresh then failure: {events:?}");
        assert!(
            matches!(&events[0], SessionUpdate::PluginsInventoryUpdated { .. }),
            "the refresh lands first: {events:?}"
        );
        assert!(
            matches!(&events[1], SessionUpdate::PluginsCliActionFailed { message, .. }
                if message.contains("unregistered")
                    && message.contains("claude plugin marketplace add a/probe")),
            "the honest message lands last: {events:?}"
        );
        assert_eq!(
            *calls.lock().expect("calls"),
            vec!["remove".to_owned(), "add".to_owned(), "refresh".to_owned()]
        );
    }

    /// Drain the pane's plugin events in arrival order, applying none.
    fn drain_plugin_events(app: &mut App) -> Vec<SessionUpdate> {
        let mut events = Vec::new();
        while let Ok(update) = app.update_rx.try_recv() {
            events.push(update);
        }
        events
    }

    /// The paint-ordering contract: the inventory event is OUT before
    /// the token-cost fetch completes - first paint never waits on a
    /// `claude` spawn. The details fake blocks until released.
    #[tokio::test(flavor = "current_thread")]
    async fn the_inventory_event_lands_before_the_costs_fetch_completes() {
        let (mut app, _agent_rx) = app_with_connection();
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "supabase@probe".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        let proceed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let details_flag = std::sync::Arc::clone(&proceed);
        let calls = call_log();
        let details_calls = std::sync::Arc::clone(&calls);
        app.plugins.update_cli = Some(UpdateCli {
            run_update: std::sync::Arc::new(|_, _, _| {
                Box::pin(async { Err("the refresh path never updates".to_owned()) })
            }),
            refresh: {
                let calls = std::sync::Arc::clone(&calls);
                std::sync::Arc::new(move |_cached, _cwd| {
                    let calls = std::sync::Arc::clone(&calls);
                    Box::pin(async move {
                        calls.lock().expect("calls").push("refresh".to_owned());
                        Ok((
                            PluginsInventorySnapshot {
                                installed: vec![InstalledPluginEntry {
                                    id: "supabase@probe".to_owned(),
                                    version: Some("1.0.0".to_owned()),
                                    scope: "user".to_owned(),
                                    enabled: true,
                                    installed_at: None,
                                    last_updated: None,
                                    project_path: None,
                                    capability: PluginCapability::Skill,
                                }],
                                ..PluginsInventorySnapshot::default()
                            },
                            std::path::PathBuf::from("claude"),
                        ))
                    })
                })
            },
            details: std::sync::Arc::new(move |_, _, requests| {
                let calls = std::sync::Arc::clone(&details_calls);
                let flag = std::sync::Arc::clone(&details_flag);
                Box::pin(async move {
                    calls.lock().expect("calls").push(format!("details:{}", requests.len()));
                    while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                        tokio::task::yield_now().await;
                    }
                    std::collections::BTreeMap::from([(
                        "supabase@probe".to_owned(),
                        (
                            "1.0.0".to_owned(),
                            forge_primitives::plugins::PluginDetails { token_cost_always_on: 450 },
                        ),
                    )])
                })
            }),
            rollback: std::sync::Arc::new(|_, _, _, _| {
                Box::pin(async { Ok(PluginRollbackOutcome::RolledBack) })
            }),
        });

        tokio::task::LocalSet::new()
            .run_until(async {
                request_inventory_refresh(&mut app);
                // Yields enough for the refresh to complete and the
                // details fetch to enter its blocked spin.
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }

                // THE PIN: the inventory event is out while the details
                // fetch is still incomplete.
                let mut events = Vec::new();
                while let Ok(update) = app.update_rx.try_recv() {
                    events.push(update);
                }
                assert!(
                    matches!(&events[..], [SessionUpdate::PluginsInventoryUpdated { .. }]),
                    "exactly the inventory event, before the costs fetch finishes: {events:?}"
                );
                assert_eq!(
                    *calls.lock().expect("calls"),
                    vec!["refresh".to_owned(), "details:1".to_owned()],
                    "the costs fetch has entered but not completed"
                );

                // Release the fetch; the costs event follows.
                proceed.store(true, std::sync::atomic::Ordering::SeqCst);
                for _ in 0..20 {
                    tokio::task::yield_now().await;
                }
                let mut events = Vec::new();
                while let Ok(update) = app.update_rx.try_recv() {
                    events.push(update);
                }
                assert!(
                    matches!(&events[..], [SessionUpdate::PluginsInventoryUpdated { snapshot, .. }]
                        if !snapshot.token_costs.is_empty()),
                    "the costs event follows the release: {events:?}"
                );
            })
            .await;
    }

    /// The five-second TTL path invokes NEITHER the CLI refresh nor
    /// the details fetch - the disk scan alone. A regression folding
    /// the CLI back into the TTL path dies here.
    #[tokio::test(flavor = "current_thread")]
    async fn the_ttl_refresh_invokes_no_cli_call() {
        let (mut app, _agent_rx) = app_with_connection();
        let calls = call_log();
        app.plugins.update_cli = Some(repair_cli(false, false, &calls));
        // A stale TTL: the refresh is due.
        app.plugins.last_inventory_refresh_at = Some(
            Instant::now()
                .checked_sub(std::time::Duration::from_secs(6))
                .expect("a six-second offset is far inside Instant's range"),
        );

        tokio::task::LocalSet::new()
            .run_until(async {
                request_inventory_refresh_if_needed(&mut app);
                let mut spins = 0;
                while app.plugins.loading && spins < 10_000 {
                    tokio::task::yield_now().await;
                    spins += 1;
                }
            })
            .await;

        assert!(
            calls.lock().expect("calls").is_empty(),
            "the TTL path runs the disk scan, never the CLI seam: {:?}",
            calls.lock().expect("calls")
        );
        let mut saw_updated = false;
        while let Ok(update) = app.update_rx.try_recv() {
            saw_updated |= matches!(update, SessionUpdate::PluginsInventoryUpdated { .. });
        }
        assert!(saw_updated, "the disk scan lands as an inventory event");
    }

    /// An unresolvable plugins root is a refresh FAILURE naming why -
    /// the previous snapshot stays on screen, never an all-empty
    /// snapshot applied as truth.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unresolvable_disk_scan_fails_instead_of_emptying() {
        let (mut app, _agent_rx) = app_with_connection();
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "supabase@probe".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }];
        app.plugins.installed_rows = vec![plugin_row("supabase@probe", RowState::Current)];

        tokio::task::LocalSet::new()
            .run_until(async {
                request_disk_refresh_with(&mut app, || None);
                let mut spins = 0;
                while app.plugins.loading && spins < 10_000 {
                    tokio::task::yield_now().await;
                    spins += 1;
                }
            })
            .await;

        // Drain and APPLY the failure event like the event loop would.
        let mut failure = None;
        while let Ok(update) = app.update_rx.try_recv() {
            if let SessionUpdate::PluginsInventoryRefreshFailed { message, .. } = &update {
                failure = Some(message.clone());
            }
            crate::app::events::apply_session_update(&mut app, update);
        }
        assert!(!app.plugins.loading, "the failure settles the flag");
        assert_eq!(
            app.plugins.installed.len(),
            1,
            "the previous snapshot is kept, not replaced by an empty one"
        );
        assert!(
            failure.as_ref().is_some_and(|message| message.contains("could not be resolved")),
            "the unresolvable root is named: {failure:?}"
        );
    }

    /// The update path drives the page's own panel, never the global
    /// top spinner: `app.status` sits untouched through progress and
    /// finish events.
    #[test]
    fn an_update_run_never_writes_the_global_spinner_state() {
        let mut app = App::test_default();
        let before = app.status.clone();

        apply_update_run_progress(
            &mut app,
            PluginUpdateRun {
                trigger: PluginUpdateTrigger::Manual,
                finished: false,
                rows: vec![PluginUpdateRunRow::queued(
                    "pensive@claude-night-market".to_owned(),
                    "user".to_owned(),
                    String::new(),
                    Some("1.7.2".to_owned()),
                )],
            },
        );
        assert_eq!(app.status, before, "progress never touches the spinner");

        let run = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![finished_update_row("pensive@claude-night-market")],
        };
        apply_update_run_finished(&mut app, &run, None, None);
        assert_eq!(app.status, before, "the finished run never touches the spinner");
        assert!(app.plugins.update_run.is_some(), "the page's own panel holds the run");
    }

    fn finished_update_row(plugin_id: &str) -> PluginUpdateRunRow {
        let mut row = PluginUpdateRunRow::queued(
            plugin_id.to_owned(),
            "user".to_owned(),
            String::new(),
            Some("1.7.2".to_owned()),
        );
        row.status = PluginRunRowStatus::Updated;
        row.installed_version = Some("2.0.0".to_owned());
        row
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
            crate::app::config::handle_extensions_paste(&mut app, "b\nc"),
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
            components: vec![],
            marketplace_health: vec![],
            token_costs: std::collections::BTreeMap::default(),
        }
    }

    #[test]
    fn plugins_tabs_wrap_in_both_directions() {
        assert_eq!(ExtensionsTab::Installed.prev(), ExtensionsTab::Marketplaces);
        assert_eq!(ExtensionsTab::Marketplaces.next(), ExtensionsTab::Installed);
    }

    #[test]
    fn recent_inventory_snapshot_skips_refresh() {
        let mut app = crate::app::App::test_default();
        app.plugins.active_tab = ExtensionsTab::Installed;
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
    fn extension_rows_match_their_name_and_source() {
        let mut row = forge_primitives::plugins::ExtensionRow {
            id: "skill:superpowers:brainstorming".to_owned(),
            kind: forge_primitives::plugins::ExtensionKind::Skill,
            name: "brainstorming".to_owned(),
            source: "superpowers@probe-market".to_owned(),
            version: Some("6.3.0".to_owned()),
            available_version: None,
            state: forge_primitives::plugins::RowState::Current,
            detail: None,
        };

        assert!(row_matches(&row, ""));
        assert!(row_matches(&row, "brainstorm"), "the name matches");
        assert!(row_matches(&row, "superpowers"), "the source matches");
        row.name = "planning".to_owned();
        assert!(
            !row_matches(&row, "brainstorm"),
            "a query matching neither field filters the row out"
        );
    }

    fn app_with_focused_search(tab: ExtensionsTab) -> crate::app::App {
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
        let mut app = app_with_focused_search(ExtensionsTab::Installed);

        for ch in ['a', 'b', 'c'] {
            let _ = press(&mut app, KeyCode::Char(ch));
        }
        assert_eq!(
            app.plugins.search_query_for(ExtensionsTab::Installed),
            "abc",
            "typing appends to the filter in order"
        );

        let _ = press(&mut app, KeyCode::Backspace);
        assert_eq!(
            app.plugins.search_query_for(ExtensionsTab::Installed),
            "ab",
            "Backspace drops exactly one character off the end"
        );

        let _ = press(&mut app, KeyCode::Delete);
        assert_eq!(
            app.plugins.search_query_for(ExtensionsTab::Installed),
            "",
            "Delete wipes the whole filter rather than one character"
        );
    }

    /// Home and End fall through this view's keymap, so the filter has
    /// no way to reposition where the next character lands. Routing
    /// either into the editor would make this insert mid-string.
    #[test]
    fn search_filter_has_no_reachable_cursor_movement() {
        let mut app = app_with_focused_search(ExtensionsTab::Installed);
        for ch in ['a', 'b'] {
            let _ = press(&mut app, KeyCode::Char(ch));
        }

        assert!(!press(&mut app, KeyCode::Home), "Home is unbound while the filter has focus");
        assert!(!press(&mut app, KeyCode::End), "End is unbound while the filter has focus");

        let _ = press(&mut app, KeyCode::Char('c'));
        assert_eq!(
            app.plugins.search_query_for(ExtensionsTab::Installed),
            "abc",
            "typing after Home/End still appends rather than inserting mid-string"
        );
    }

    /// Paste collapses every newline flavour to a space, and a newline
    /// delivered as a printable key is rejected, so both routes into
    /// the filter agree on one line.
    #[test]
    fn search_filter_rejects_typed_newlines_and_flattens_pasted_ones() {
        let mut app = app_with_focused_search(ExtensionsTab::Installed);

        assert!(handle_paste(&mut app, "a\nb\r\nc\rd"), "a focused filter accepts a paste");
        assert_eq!(
            app.plugins.search_query_for(ExtensionsTab::Installed),
            "a b c d",
            "pasted newlines collapse to spaces"
        );

        for ch in ['\n', '\r'] {
            assert!(press(&mut app, KeyCode::Char(ch)), "a rejected key is still consumed");
            assert_eq!(
                app.plugins.search_query_for(ExtensionsTab::Installed),
                "a b c d",
                "a typed {ch:?} never enters the filter"
            );
        }

        let _ = press(&mut app, KeyCode::Char('e'));
        assert_eq!(
            app.plugins.search_query_for(ExtensionsTab::Installed),
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
        let mut app = app_with_focused_search(ExtensionsTab::Installed);
        app.active_view = crate::app::ActiveView::Extensions;
        let _ = press(&mut app, KeyCode::Char('a'));

        crate::app::config::handle_extensions_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(
            app.active_view,
            crate::app::ActiveView::Extensions,
            "Enter with the filter focused does not close the view"
        );
        assert_eq!(
            app.plugins.search_query_for(ExtensionsTab::Installed),
            "a",
            "Enter inserts nothing into the filter"
        );

        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::SHIFT] {
            crate::app::config::handle_extensions_key(
                &mut app,
                KeyEvent::new(KeyCode::Enter, modifiers),
            );
            assert_eq!(
                app.active_view,
                crate::app::ActiveView::Extensions,
                "a modified Enter with the filter focused does not close the view"
            );
            assert_eq!(
                app.plugins.search_query_for(ExtensionsTab::Installed),
                "a",
                "a modified Enter inserts nothing into the filter"
            );
        }

        crate::app::config::handle_extensions_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert_eq!(app.active_view, crate::app::ActiveView::Chat, "Esc still closes the view");
    }

    #[test]
    fn installed_and_skills_search_queries_are_independent() {
        let mut state = PluginsState::default();
        if let Some(query) =
            state.tab_state.search_queries.get_mut(ExtensionsTab::Installed.index())
        {
            query.set_text("installed");
        }
        if let Some(query) = state.tab_state.search_queries.get_mut(ExtensionsTab::Skills.index()) {
            query.set_text("skills");
        }

        assert_eq!(state.search_query_for(ExtensionsTab::Installed), "installed");
        assert_eq!(state.search_query_for(ExtensionsTab::Skills), "skills");
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
        app.plugins.update_availability = vec![PluginUpdateAvailability {
            plugin_id: "frontend-design@claude-plugins-official".to_owned(),
            scope: "user".to_owned(),
            marketplace: "claude-plugins-official".to_owned(),
            installed_version: Some("1.0.0".to_owned()),
            available_version: Some("2.0.0".to_owned()),
        }];

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
        assert!(
            app.plugins.update_availability.is_empty(),
            "markers recompute from the action's snapshot, whose marketplace copy is empty"
        );
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

    /// Seed one stale extension row so `Update all` has a queue.
    fn seed_stale_row(app: &mut App, id: &str, from: &str, to: &str) {
        app.plugins.installed_rows.push(ExtensionRow {
            id: id.to_owned(),
            kind: ExtensionKind::Plugin,
            name: id.split('@').next().unwrap_or(id).to_owned(),
            source: id.split('@').nth(1).unwrap_or_default().to_owned(),
            version: Some(from.to_owned()),
            available_version: Some(to.to_owned()),
            state: RowState::UpdateAvailable,
            detail: None,
        });
    }

    /// Under the manual `u` key only the stale set queues - the rows
    /// wearing the Update action - not every installed plugin.
    #[test]
    fn manual_rows_queue_exactly_the_stale_set() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        push_no_marketplace_entry(&mut app);
        seed_stale_row(&mut app, "supabase@claude-plugins-official", "1.0.0", "2.0.0");
        seed_stale_row(&mut app, "pensive@claude-night-market", "1.7.2", "2.0.0");

        let rows = build_update_rows(&app, PluginUpdateTrigger::Manual);

        assert_eq!(rows.len(), 2, "only the two stale plugins: {rows:?}");
        assert!(rows.iter().all(|row| row.status == PluginRunRowStatus::Queued));
        assert_eq!(rows[0].plugin_id, "supabase@claude-plugins-official");
        assert_eq!(rows[1].plugin_id, "pensive@claude-night-market");
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

        app.config.last_error = Some("stale".to_owned());
        apply_update_run_finished(&mut app, &run, None, None);
        assert_eq!(
            app.plugins
                .update_availability
                .iter()
                .find(|availability| availability.plugin_id == "supabase@claude-plugins-official")
                .and_then(|availability| availability.available_version.as_deref()),
            Some("2.0.0"),
            "a finished check leaves the marker"
        );
        assert!(app.config.last_error.is_none(), "a finished check clears a mirrored error");
        assert!(
            app.config
                .status_message
                .as_deref()
                .is_some_and(|message| message.starts_with("Update check: ")),
            "the check summary lands on the footer pair"
        );

        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.plugins.update_run.is_none(), "Esc clears the report");
        assert!(
            app.plugins
                .update_availability
                .iter()
                .any(|availability| availability.plugin_id == "supabase@claude-plugins-official"),
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
                components: Vec::new(),
                marketplace_health: Vec::new(),
                token_costs: std::collections::BTreeMap::default(),
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
                components: Vec::new(),
                marketplace_health: Vec::new(),
                token_costs: std::collections::BTreeMap::default(),
            },
            PathBuf::new(),
        );
        assert!(
            app.plugins.update_availability.is_empty(),
            "an empty inventory truthfully yields no markers"
        );

        // A zero-rows run classifies as the nothing-found check: the
        // footer says "Update check: ", not "Update run finished: ".
        let nothing = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: Vec::new(),
        };
        app.config.last_error = Some("stale".to_owned());
        apply_update_run_finished(&mut app, &nothing, None, None);
        assert!(
            app.config
                .status_message
                .as_deref()
                .is_some_and(|message| message.starts_with("Update check: ")),
            "a nothing-found run reports as a check"
        );
        assert!(app.config.last_error.is_none(), "the recorded error clears");
        assert!(app.plugins.update_availability.is_empty(), "nothing found: no markers");
    }

    /// The plugins failure handlers write the config feedback pair,
    /// which is what the pane's footer renders.
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

    /// A rollback guard refusal leaves the overlay open, so its
    /// message has to reach the footer pair or nothing shows it.
    #[test]
    fn rollback_guard_refusals_surface_on_the_footer_pair() {
        let mut app = App::test_default();

        start_rollback(&mut app, "pensive@claude-night-market".to_owned(), "user".to_owned());
        assert_eq!(
            app.config.last_error.as_deref(),
            Some("No recorded previous version for this plugin")
        );

        app.config.last_error = None;
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
        start_rollback(&mut app, "pensive@claude-night-market".to_owned(), "user".to_owned());
        assert_eq!(
            app.config.last_error.as_deref(),
            Some("No pre-update marketplace ref was captured; rollback is unavailable")
        );

        app.plugins.update_records[0].marketplace_ref_before = Some("abc123".to_owned());
        start_rollback(&mut app, "pensive@claude-night-market".to_owned(), "user".to_owned());
        assert_eq!(
            app.config.last_error.as_deref(),
            Some("Rollback needs a git-backed marketplace clone; none found")
        );
    }

    /// The `u` start message reaches the footer pair and clears a
    /// stale error with it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_update_start_message_surfaces_on_the_footer_pair() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        seed_stale_row(&mut app, "supabase@claude-plugins-official", "1.0.0", "2.0.0");
        app.plugins.active_tab = ExtensionsTab::Installed;

        tokio::task::LocalSet::new()
            .run_until(async {
                app.config.last_error = Some("stale".to_owned());
                handle_key(&mut app, KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
                assert_eq!(
                    app.config.status_message.as_deref(),
                    Some("Updating 1 plugin(s)..."),
                    "the start message reaches the footer pair: {:?}",
                    app.config.status_message
                );
                assert!(app.config.last_error.is_none(), "the stale error clears");
            })
            .await;
    }

    /// The `c` start message reaches the footer pair and clears a
    /// stale error with it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_check_start_message_surfaces_on_the_footer_pair() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        app.plugins.active_tab = ExtensionsTab::Installed;

        tokio::task::LocalSet::new()
            .run_until(async {
                app.config.last_error = Some("stale".to_owned());
                handle_key(&mut app, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
                assert_eq!(
                    app.config.status_message.as_deref(),
                    Some("Checking for plugin updates..."),
                    "the start message reaches the footer pair: {:?}",
                    app.config.status_message
                );
                assert!(app.config.last_error.is_none(), "the stale error clears");
            })
            .await;
    }

    /// The `r` start message reaches the footer pair and clears a
    /// stale error with it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_refresh_start_message_surfaces_on_the_footer_pair() {
        let mut app = App::test_default();
        app.plugins.active_tab = ExtensionsTab::Installed;

        tokio::task::LocalSet::new()
            .run_until(async {
                app.config.last_error = Some("stale".to_owned());
                handle_key(&mut app, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
                assert_eq!(
                    app.config.status_message.as_deref(),
                    Some("Refreshing plugin inventory..."),
                    "the start message reaches the footer pair: {:?}",
                    app.config.status_message
                );
                assert!(app.config.last_error.is_none(), "the stale error clears");
            })
            .await;
    }

    /// The rollback start message reaches the footer pair and clears
    /// a stale error with it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_rollback_start_message_surfaces_on_the_footer_pair() {
        let mut app = App::test_default();
        app.plugins.update_records = vec![PluginUpdateRecord {
            plugin_id: "pensive@claude-night-market".to_owned(),
            marketplace: "claude-night-market".to_owned(),
            scope: "user".to_owned(),
            cwd_raw: String::new(),
            from_version: Some("1.7.1".to_owned()),
            to_version: Some("1.7.2".to_owned()),
            marketplace_ref_before: Some("abc123".to_owned()),
            updated_at: "2026-09-04T06:00:00Z".to_owned(),
            trigger: PluginUpdateTrigger::Manual,
        }];
        app.plugins.marketplaces = vec![MarketplaceSourceEntry {
            name: "claude-night-market".to_owned(),
            source: None,
            repo: None,
            install_location: Some("/tmp/claude-night-market".to_owned()),
        }];

        tokio::task::LocalSet::new()
            .run_until(async {
                app.config.last_error = Some("stale".to_owned());
                start_rollback(
                    &mut app,
                    "pensive@claude-night-market".to_owned(),
                    "user".to_owned(),
                );
                assert!(
                    app.config
                        .status_message
                        .as_deref()
                        .is_some_and(|message| { message.starts_with("Rolling back ") }),
                    "the start message reaches the footer pair: {:?}",
                    app.config.status_message
                );
                assert!(app.config.last_error.is_none(), "the stale error clears");
            })
            .await;
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
            components: Vec::new(),
            marketplace_health: Vec::new(),
            token_costs: std::collections::BTreeMap::default(),
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
        assert_eq!(
            app.plugins
                .update_availability
                .iter()
                .map(|availability| (availability.plugin_id.as_str(), availability.scope.as_str()))
                .collect::<Vec<_>>(),
            vec![("supabase@claude-plugins-official", "user")],
            "a failed post-run refresh KEEPS the markers - they describe the same pre-run \
             rows on screen, and dropping them leaves a markerless gap until reopen"
        );
    }

    /// A rollback's post-rollback snapshot recomputes the markers like
    /// every sibling handler: the moved installed version shows until
    /// the marketplace copy catches up, and an equal version drops the
    /// marker.
    #[test]
    fn a_rollback_success_recomputes_markers_from_its_snapshot() {
        let mut app = App::test_default();
        app.plugins.update_availability = vec![PluginUpdateAvailability {
            plugin_id: "pensive@claude-night-market".to_owned(),
            scope: "user".to_owned(),
            marketplace: "claude-night-market".to_owned(),
            installed_version: Some("1.7.2".to_owned()),
            available_version: Some("2.0.0".to_owned()),
        }];
        let entry = |version: &str| PluginsInventorySnapshot {
            installed: vec![InstalledPluginEntry {
                id: "pensive@claude-night-market".to_owned(),
                version: Some(version.to_owned()),
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: PluginCapability::Skill,
            }],
            marketplace: vec![MarketplaceEntry {
                plugin_id: "pensive@claude-night-market".to_owned(),
                name: "Pensive".to_owned(),
                description: None,
                marketplace_name: Some("claude-night-market".to_owned()),
                version: Some("2.0.0".to_owned()),
                install_count: None,
                source: None,
            }],
            marketplaces: Vec::new(),
            components: Vec::new(),
            marketplace_health: Vec::new(),
            token_costs: std::collections::BTreeMap::default(),
        };

        apply_rollback_success(
            &mut app,
            "pensive@claude-night-market",
            "user",
            "Rolled back".to_owned(),
            entry("1.7.1"),
            PathBuf::new(),
        );
        assert_eq!(
            app.plugins
                .update_availability
                .iter()
                .map(|availability| (
                    availability.plugin_id.as_str(),
                    availability.installed_version.as_deref(),
                    availability.available_version.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![("pensive@claude-night-market", Some("1.7.1"), Some("2.0.0"))],
            "the marker recomputes to the rolled-back version"
        );

        apply_rollback_success(
            &mut app,
            "pensive@claude-night-market",
            "user",
            "Rolled forward".to_owned(),
            entry("2.0.0"),
            PathBuf::new(),
        );
        assert!(
            app.plugins.update_availability.is_empty(),
            "an installed version equal to the marketplace copy wears no marker"
        );

        // The failure twin: a failed rollback whose failure snapshot
        // moved the install recomputes the markers the same way.
        app.plugins.update_availability = vec![PluginUpdateAvailability {
            plugin_id: "pensive@claude-night-market".to_owned(),
            scope: "user".to_owned(),
            marketplace: "claude-night-market".to_owned(),
            installed_version: Some("2.0.0".to_owned()),
            available_version: Some("2.1.0".to_owned()),
        }];
        apply_rollback_failure(
            &mut app,
            "pensive@claude-night-market",
            "boom",
            Some(entry("2.0.0")),
        );
        assert!(
            app.plugins.update_availability.is_empty(),
            "the failure snapshot's install matches the marketplace copy: no marker"
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

        app.config.last_error = Some("stale".to_owned());
        apply_update_run_finished(&mut app, &run, None, None);

        assert!(!app.plugins.loading);
        assert_eq!(
            app.plugins.update_run.as_ref().map(PluginUpdateRun::summary),
            Some("1 updated, 0 failed, 0 current".to_owned())
        );
        assert!(app.plugins.update_records.is_empty());
        // The Auto arm skips the runtime reload, so it syncs the
        // footer pair itself: the recorded error clears and the
        // summary lands on the status line.
        assert!(app.config.last_error.is_none(), "a finished boot run clears a recorded error");
        assert_eq!(
            app.config.status_message.as_deref(),
            Some("Plugin auto-update finished: 1 updated, 0 failed, 0 current")
        );

        // The plain manual arm - nothing applied, not a check - syncs
        // the footer pair itself rather than riding the runtime reload.
        let current = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![PluginUpdateRunRow {
                plugin_id: "supabase@claude-plugins-official".to_owned(),
                scope: "user".to_owned(),
                cwd_raw: app.cwd_raw(),
                marketplace: "claude-plugins-official".to_owned(),
                status: PluginRunRowStatus::AlreadyCurrent,
                installed_version: Some("1.1.0".to_owned()),
                available_version: None,
                detail: None,
            }],
        };
        app.config.last_error = Some("stale".to_owned());
        apply_update_run_finished(&mut app, &current, None, None);
        assert!(app.config.last_error.is_none(), "a finished manual run clears a mirrored error");
        assert!(
            app.config
                .status_message
                .as_deref()
                .is_some_and(|message| message.starts_with("Update run finished: ")),
            "the manual summary lands on the footer pair"
        );
    }

    /// A session change drops the markers with the rest of the pane's
    /// inventory-derived state.
    #[test]
    fn a_session_change_clears_the_check_markers() {
        let mut app = App::test_default();
        app.plugins.update_availability = vec![PluginUpdateAvailability {
            plugin_id: "supabase@claude-plugins-official".to_owned(),
            scope: "user".to_owned(),
            marketplace: "claude-plugins-official".to_owned(),
            installed_version: Some("1.0.0".to_owned()),
            available_version: Some("2.0.0".to_owned()),
        }];

        reset_for_session_change(&mut app);

        assert!(app.plugins.update_availability.is_empty(), "markers die with the session");
    }

    /// A `u` with nothing installed refuses the run and syncs the
    /// footer pair itself: a recorded failure must not outlive the
    /// no-op.
    #[tokio::test]
    async fn an_empty_u_reports_nothing_eligible_and_syncs_the_footer() {
        let mut app = App::test_default();
        app.plugins.installed.clear();
        app.config.last_error = Some("stale".to_owned());

        start_update_run(&mut app, PluginUpdateTrigger::Manual);

        assert!(app.plugins.update_run.is_none(), "no run is seeded");
        assert!(app.config.last_error.is_none(), "the recorded error clears");
        assert_eq!(
            app.config.status_message.as_deref(),
            Some("No installed plugins are eligible for update")
        );
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
            details: {
                let calls = calls.clone();
                std::sync::Arc::new(move |_, _, requests| {
                    let calls = calls.clone();
                    Box::pin(async move {
                        calls.lock().expect("call log").push(format!("details:{}", requests.len()));
                        std::collections::BTreeMap::new()
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
            components: vec![],
            marketplace_health: vec![],
            token_costs: std::collections::BTreeMap::default(),
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
    async fn the_u_key_runs_exactly_the_stale_rows() {
        let mut app = App::test_default();
        seeded_installed(&mut app);
        seed_stale_row(&mut app, "supabase@claude-plugins-official", "1.0.0", "2.0.0");
        let calls = call_log();
        app.plugins.update_cli =
            Some(fake_cli("is already at the latest version.", &two_plugin_snapshot(), &calls));
        app.plugins.active_tab = ExtensionsTab::Installed;

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
        let update_calls =
            log.iter().filter(|call| !call.starts_with("refresh")).cloned().collect::<Vec<_>>();
        assert_eq!(
            update_calls.len(),
            1,
            "one update call, for the one stale plugin: {update_calls:?}"
        );
        assert!(update_calls[0].contains("supabase@claude-plugins-official"));
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
    /// entry cwd; a marketplace-less id skips without a CLI call and
    /// the footer summary stays honest. The run is seeded
    /// synchronously so a manual `u` cannot race it.
    #[tokio::test(flavor = "current_thread")]
    async fn boot_auto_update_runs_every_installed_plugin() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace");
        let calls = call_log();
        let mut snapshot = two_plugin_snapshot();
        snapshot.installed.push(InstalledPluginEntry {
            id: "bare-skill".to_owned(),
            version: Some("0.1.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        });
        let cli = fake_cli("supabase is already at the latest version (1.0.0).", &snapshot, &calls);
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
        assert_eq!(
            update_calls.len(),
            3,
            "every marketplace-backed plugin updates, the bare id never calls: {log:?}"
        );
        assert!(
            log.iter().any(|call| call.starts_with("/test:")),
            "user-scoped plugins update from the boot cwd: {log:?}"
        );

        let run = app.plugins.update_run.as_ref().expect("the report stands");
        assert!(run.finished);
        assert!(
            run.rows
                .iter()
                .any(|row| row.plugin_id == "bare-skill"
                    && row.status == PluginRunRowStatus::Skipped),
            "the marketplace-less entry skips on the auto arm: {:?}",
            run.rows
        );
        assert_eq!(
            app.config.status_message.as_deref(),
            Some("Plugin auto-update finished: all current"),
            "the footer summary counts the three current plugins, not the skipped one"
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
        seed_stale_row(&mut app, "supabase@claude-plugins-official", "1.0.0", "2.0.0");
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
            details: std::sync::Arc::new(|_, _, _| {
                Box::pin(async { std::collections::BTreeMap::new() })
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
            details: std::sync::Arc::new(|_, _, _| {
                Box::pin(async { std::collections::BTreeMap::new() })
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
            details: std::sync::Arc::new(|_, _, _| {
                Box::pin(async { std::collections::BTreeMap::new() })
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
            app.config
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("claude CLI not found")),
            "the failure is visible in the pane: {:?}",
            app.config.last_error
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
            details: std::sync::Arc::new(|_, _, _| {
                Box::pin(async { std::collections::BTreeMap::new() })
            }),
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
