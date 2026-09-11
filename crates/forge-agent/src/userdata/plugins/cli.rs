use super::components::{scan_components, scan_marketplaces};
use super::{
    InstalledPluginEntry, MarketplaceEntry, MarketplaceSourceEntry, PluginCapability,
    PluginRunRowStatus, PluginUpdateRecord, PluginUpdateRun, PluginsInventorySnapshot,
    classify_update_row,
};
use crate::env::git_command;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Deserialize)]
struct InstalledPluginJson {
    id: String,
    version: Option<String>,
    scope: String,
    enabled: bool,
    #[serde(rename = "installedAt")]
    installed_at: Option<String>,
    #[serde(rename = "lastUpdated")]
    last_updated: Option<String>,
    #[serde(rename = "projectPath")]
    project_path: Option<String>,
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct MarketplaceListJson {
    available: Vec<AvailablePluginJson>,
}

#[derive(Debug, Deserialize)]
struct AvailablePluginJson {
    #[serde(rename = "pluginId")]
    plugin_id: String,
    name: String,
    description: Option<String>,
    #[serde(rename = "marketplaceName")]
    marketplace_name: Option<String>,
    version: Option<String>,
    #[serde(rename = "installCount")]
    install_count: Option<u64>,
    source: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct MarketplaceSourceJson {
    name: String,
    source: Option<String>,
    repo: Option<String>,
    #[serde(rename = "installLocation")]
    install_location: Option<String>,
}

pub async fn refresh_inventory(
    cwd_raw: String,
    cached_claude_path: Option<PathBuf>,
) -> Result<(PluginsInventorySnapshot, PathBuf), String> {
    tokio::task::spawn_blocking(move || {
        let claude_path = resolve_claude_path(cached_claude_path)?;
        let snapshot = refresh_inventory_blocking(&claude_path, &cwd_raw)?;
        Ok((snapshot, claude_path))
    })
    .await
    .map_err(|error| format!("Plugin inventory task failed: {error}"))?
}

pub async fn run_cli_command_and_refresh(
    cwd_raw: String,
    cached_claude_path: Option<PathBuf>,
    args: Vec<String>,
) -> Result<(PluginsInventorySnapshot, PathBuf), String> {
    tokio::task::spawn_blocking(move || {
        let claude_path = resolve_claude_path(cached_claude_path)?;
        run_command(&claude_path, &cwd_raw, &args)?;
        let snapshot = refresh_inventory_blocking(&claude_path, &cwd_raw)?;
        Ok((snapshot, claude_path))
    })
    .await
    .map_err(|error| format!("Plugin CLI action task failed: {error}"))?
}

fn resolve_claude_path(cached_claude_path: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = cached_claude_path
        && path.is_file()
    {
        return Ok(path);
    }
    which::which("claude").map_err(|_| "claude CLI not found in PATH".to_owned())
}

fn refresh_inventory_blocking(
    claude_path: &Path,
    cwd_raw: &str,
) -> Result<PluginsInventorySnapshot, String> {
    let installed = parse_json_command::<Vec<InstalledPluginJson>>(
        claude_path,
        cwd_raw,
        &["plugin", "list", "--json"],
    )?;
    let available = parse_json_command::<MarketplaceListJson>(
        claude_path,
        cwd_raw,
        &["plugin", "list", "--available", "--json"],
    )?;
    let marketplaces = parse_json_command::<Vec<MarketplaceSourceJson>>(
        claude_path,
        cwd_raw,
        &["plugin", "marketplace", "list", "--json"],
    )?;

    let mut installed_entries = installed
        .into_iter()
        .map(|entry| InstalledPluginEntry {
            id: entry.id,
            version: entry.version,
            scope: entry.scope,
            enabled: entry.enabled,
            installed_at: entry.installed_at,
            last_updated: entry.last_updated,
            project_path: entry.project_path,
            capability: if entry.mcp_servers.is_some() {
                PluginCapability::Mcp
            } else {
                PluginCapability::Skill
            },
        })
        .collect::<Vec<_>>();
    installed_entries.sort_by_cached_key(|entry| entry.id.to_ascii_lowercase());

    let mut marketplace_entries = available
        .available
        .into_iter()
        .map(|entry| MarketplaceEntry {
            plugin_id: entry.plugin_id,
            name: entry.name,
            description: entry.description,
            marketplace_name: entry.marketplace_name,
            version: entry.version,
            install_count: entry.install_count,
            source: entry.source,
        })
        .collect::<Vec<_>>();
    marketplace_entries.sort_by_cached_key(|entry| {
        (
            entry.marketplace_name.as_deref().unwrap_or_default().to_ascii_lowercase(),
            entry.name.to_ascii_lowercase(),
        )
    });

    let mut marketplace_sources = marketplaces
        .into_iter()
        .map(|entry| MarketplaceSourceEntry {
            name: entry.name,
            source: entry.source,
            repo: entry.repo,
            install_location: entry.install_location,
        })
        .collect::<Vec<_>>();
    marketplace_sources.sort_by_cached_key(|entry| entry.name.to_ascii_lowercase());

    // The component scan reads the same config dir the CLI calls
    // above resolved against (process-level env), so both halves of
    // the snapshot describe one installation.
    let (components, marketplace_health) = match super::components::plugins_root() {
        Some(root) => {
            let started = std::time::Instant::now();
            let marketplaces_root = root.join("marketplaces");
            let config_dir = root.parent().unwrap_or(root.as_path()).to_path_buf();
            let components = scan_components(&root, &marketplaces_root);
            let marketplace_health = scan_marketplaces(&marketplaces_root, &config_dir);
            tracing::info!(
                target: "forge_agent::userdata::plugins",
                duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                installed = components.iter().filter(|row| row.installed).count(),
                available = components.iter().filter(|row| !row.installed).count(),
                skills = components.iter().map(|row| row.skills.len()).sum::<usize>(),
                marketplaces = marketplace_health.len(),
                "extension inventory scan"
            );
            (components, marketplace_health)
        }
        None => (Vec::new(), Vec::new()),
    };

    Ok(PluginsInventorySnapshot {
        installed: installed_entries,
        marketplace: marketplace_entries,
        marketplaces: marketplace_sources,
        components,
        marketplace_health,
        token_costs: std::collections::BTreeMap::new(),
    })
}

fn parse_json_command<T>(claude_path: &Path, cwd_raw: &str, args: &[&str]) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let output = Command::new(claude_path)
        .args(args)
        .current_dir(cwd_raw)
        .output()
        .map_err(|error| format!("Failed to run `claude {}`: {error}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let exit_code =
            output.status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
        let detail = if stderr.is_empty() {
            format!("exit code {exit_code}")
        } else {
            format!("exit code {exit_code}: {stderr}")
        };
        return Err(format!("`claude {}` failed: {detail}", args.join(" ")));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Failed to parse JSON from `claude {}`: {error}", args.join(" ")))
}

fn run_command(claude_path: &Path, cwd_raw: &str, args: &[String]) -> Result<(), String> {
    let output = Command::new(claude_path)
        .args(args)
        .current_dir(cwd_raw)
        .output()
        .map_err(|error| format!("Failed to run `claude {}`: {error}", args.join(" ")))?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let exit_code =
        output.status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    let detail = if stderr.is_empty() {
        format!("exit code {exit_code}")
    } else {
        format!("exit code {exit_code}: {stderr}")
    };
    Err(format!("`claude {}` failed: {detail}", args.join(" ")))
}

/// Run one `claude plugin ...` action without the trailing inventory
/// refresh, so a section-level run can interleave per-plugin work with
/// progress updates and refresh once at the end. Returns the resolved
/// claude path (reuse it across a run) plus the CLI's combined
/// stdout+stderr - the update classifier needs the output, because the
/// CLI exits 0 on some failures.
pub async fn run_cli_command(
    cwd_raw: String,
    cached_claude_path: Option<PathBuf>,
    args: Vec<String>,
) -> Result<(PathBuf, String), String> {
    tokio::task::spawn_blocking(move || {
        let claude_path = resolve_claude_path(cached_claude_path)?;
        let output = Command::new(&claude_path)
            .args(&args)
            .current_dir(&cwd_raw)
            .output()
            .map_err(|error| format!("Failed to run `claude {}`: {error}", args.join(" ")))?;
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        if output.status.success() {
            return Ok((claude_path, combined));
        }
        let stderr = combined.trim().to_owned();
        let exit_code =
            output.status.code().map_or_else(|| "unknown".to_owned(), |code| code.to_string());
        let detail = if stderr.is_empty() { format!("exit code {exit_code}") } else { stderr };
        Err(format!("`claude {}` failed: {detail}", args.join(" ")))
    })
    .await
    .map_err(|error| format!("Plugin CLI task failed: {error}"))?
}

/// The `claude plugin update` invocation for one installed entry.
pub fn plugin_update_args(plugin_id: &str, scope: &str) -> Vec<String> {
    vec![
        "plugin".to_owned(),
        "update".to_owned(),
        plugin_id.to_owned(),
        "--scope".to_owned(),
        scope.to_owned(),
    ]
}

/// The marketplace clone's HEAD, or `None` when the location is not a
/// git checkout (a directory-sourced marketplace) or git fails. This
/// ref is what a rollback later restores.
pub async fn marketplace_head(install_location: String) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let output = git_command::command("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&install_location)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let head = String::from_utf8(output.stdout).ok()?;
        let head = head.trim().to_owned();
        (!head.is_empty()).then_some(head)
    })
    .await
    .ok()
    .flatten()
}

/// The two git steps of a rollback, in order: fetch the pre-update
/// ref, then detach the marketplace clone onto it so `claude plugin
/// update` resolves the version that manifest points at.
fn rollback_git_args(install_location: &str, ref_before: &str) -> Vec<Vec<String>> {
    vec![
        vec![
            "-C".to_owned(),
            install_location.to_owned(),
            "fetch".to_owned(),
            "origin".to_owned(),
            ref_before.to_owned(),
        ],
        vec![
            "-C".to_owned(),
            install_location.to_owned(),
            "checkout".to_owned(),
            "--detach".to_owned(),
            ref_before.to_owned(),
        ],
    ]
}

/// The two `claude plugin` steps of a rollback, in order: install the
/// version the rolled-back manifest points at, then move the
/// marketplace clone forward again so other plugins keep tracking the
/// latest. Swapping them would silently re-pin the clone at the old
/// ref.
fn rollback_claude_args(record: &PluginUpdateRecord) -> Vec<Vec<String>> {
    vec![
        plugin_update_args(&record.plugin_id, &record.scope),
        vec![
            "plugin".to_owned(),
            "marketplace".to_owned(),
            "update".to_owned(),
            record.marketplace.clone(),
        ],
    ]
}

/// How a rollback's CLI steps landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRollbackOutcome {
    /// The plugin reinstalled and the marketplace clone moved forward.
    RolledBack,
    /// The plugin reinstalled but the clone could not move forward;
    /// it is still parked at the old ref. A later `claude plugin
    /// marketplace update <name>` repairs it.
    RolledBackCloneParked(String),
}

/// Roll one plugin back to its recorded previous version: restore the
/// marketplace clone to the pre-update ref, let `claude plugin update`
/// install the version that manifest points at, then move the clone
/// forward again so other plugins keep tracking the latest. A failed
/// git or plugin-update step aborts with `Err`; only the trailing
/// clone-restore failure lands as [`PluginRollbackOutcome::
/// RolledBackCloneParked`].
pub async fn run_plugin_rollback(
    claude_path: Option<PathBuf>,
    cwd_raw: String,
    record: PluginUpdateRecord,
    install_location: String,
) -> Result<PluginRollbackOutcome, String> {
    let ref_before = record
        .marketplace_ref_before
        .clone()
        .ok_or_else(|| "no pre-update marketplace ref recorded for this plugin".to_owned())?;
    let git_steps = rollback_git_args(&install_location, &ref_before);
    let claude_steps = rollback_claude_args(&record);
    tokio::task::spawn_blocking(move || {
        let claude_path = resolve_claude_path(claude_path)?;
        for args in &git_steps {
            let output = git_command::command("git")
                .args(args)
                .output()
                .map_err(|error| format!("Failed to run git {}: {error}", args.join(" ")))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                return Err(format!("git {} failed: {stderr}", args.join(" ")));
            }
        }
        // The plugin update decides the rollback; only the trailing
        // clone restore is allowed to half-land.
        run_command(&claude_path, &cwd_raw, &claude_steps[0])?;
        match run_command(&claude_path, &cwd_raw, &claude_steps[1]) {
            Ok(()) => Ok(PluginRollbackOutcome::RolledBack),
            Err(message) => Ok(PluginRollbackOutcome::RolledBackCloneParked(message)),
        }
    })
    .await
    .map_err(|error| format!("Plugin rollback task failed: {error}"))?
}

/// A plugin's `claude plugin details` projection: the always-on token
/// cost every session pays for the plugin being installed. The shape
/// lives in forge_primitives::plugins beside the snapshot that
/// carries it.
pub use forge_primitives::plugins::PluginDetails;

/// The always-on token cost parsed from a details output, recorded
/// from the CLI ("  Always-on:   ~450 tok   added to every session").
fn parse_always_on_tokens(output: &str) -> Option<u64> {
    let line = output.lines().find(|line| line.trim_start().starts_with("Always-on:"))?;
    line.split(':')
        .nth(1)?
        .split_whitespace()
        .find_map(|part| part.trim_start_matches('~').parse::<u64>().ok())
}

/// The CLI's own contract: an applied update lands only after a
/// restart, stated in the update output.
pub fn output_states_restart_required(output: &str) -> bool {
    output.to_ascii_lowercase().contains("restart required")
}

/// One `claude plugin update` call: the combined CLI output (the
/// update classifier reads it, the CLI exits 0 on some failures) plus
/// whether the output states the restart contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub output: String,
    pub restart_required: bool,
}

/// Update one installed plugin in one scope.
pub async fn update_plugin(
    claude_path: Option<PathBuf>,
    cwd_raw: String,
    plugin_id: &str,
    scope: &str,
) -> Result<UpdateOutcome, String> {
    let args = plugin_update_args(plugin_id, scope);
    let (_, output) = run_cli_command(cwd_raw, claude_path, args).await?;
    let restart_required = output_states_restart_required(&output);
    Ok(UpdateOutcome { output, restart_required })
}

/// One plugin's `claude plugin details` projection.
pub async fn plugin_details(
    claude_path: Option<PathBuf>,
    cwd_raw: String,
    plugin_id: String,
) -> Result<PluginDetails, String> {
    tokio::task::spawn_blocking(move || {
        let claude_path = resolve_claude_path(claude_path)?;
        plugin_details_blocking(&claude_path, &cwd_raw, &plugin_id)
    })
    .await
    .map_err(|error| format!("Plugin details task failed: {error}"))?
}

fn plugin_details_blocking(
    claude_path: &Path,
    cwd_raw: &str,
    plugin_id: &str,
) -> Result<PluginDetails, String> {
    let output = Command::new(claude_path)
        .args(["plugin", "details", plugin_id])
        .current_dir(cwd_raw)
        .output()
        .map_err(|error| format!("Failed to run `claude plugin details`: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!("`claude plugin details` failed: {stderr}"));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let token_cost_always_on = parse_always_on_tokens(&text)
        .ok_or_else(|| "no Always-on cost in the `claude plugin details` output".to_owned())?;
    Ok(PluginDetails { token_cost_always_on })
}

/// Fetch the always-on cost for `(plugin id, version)` requests,
/// sequentially, one `claude plugin details` call each. A plugin whose
/// details fail is simply absent from the map - a missing cost reads
/// as no badge, never as an error on the pane.
pub async fn fetch_plugin_details(
    cwd_raw: String,
    cached_claude_path: Option<PathBuf>,
    requests: Vec<(String, String)>,
) -> std::collections::BTreeMap<String, PluginDetails> {
    if requests.is_empty() {
        return std::collections::BTreeMap::new();
    }
    tokio::task::spawn_blocking(move || {
        let mut costs = std::collections::BTreeMap::new();
        let Ok(claude_path) = resolve_claude_path(cached_claude_path) else {
            return costs;
        };
        for (plugin_id, _version) in requests {
            if let Ok(details) = plugin_details_blocking(&claude_path, &cwd_raw, &plugin_id) {
                costs.insert(plugin_id, details);
            }
        }
        costs
    })
    .await
    .unwrap_or_default()
}

/// Token cost per plugin, keyed by the installed version it was
/// fetched for: a plugin's cost changes only when its version does.
#[derive(Default)]
pub struct TokenCostCache {
    entries: std::collections::HashMap<String, (String, PluginDetails)>,
}

impl TokenCostCache {
    /// The plugin's always-on cost, fetched through `fetch` only when
    /// no entry for the current version exists. A versionless plugin
    /// (or a version whose details cannot be fetched) carries no cost.
    pub async fn details_for<F, Fut>(
        &mut self,
        plugin_id: &str,
        version: Option<&str>,
        mut fetch: F,
    ) -> Result<Option<PluginDetails>, String>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<PluginDetails, String>>,
    {
        let Some(version) = version else { return Ok(None) };
        if let Some((cached_version, details)) = self.entries.get(plugin_id)
            && cached_version == version
        {
            return Ok(Some(details.clone()));
        }
        let details = fetch(plugin_id.to_owned()).await?;
        self.entries.insert(plugin_id.to_owned(), (version.to_owned(), details.clone()));
        Ok(Some(details))
    }
}

/// One future handed back by the [`UpdateRunner`] seams.
pub type RunnerFut<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T>>>;
/// The update seam's result: the resolved claude path plus the CLI's
/// combined stdout+stderr.
pub type RunnerUpdateResult = Result<(PathBuf, String), String>;
/// The refresh seam's result: the inventory plus a resolved claude path.
pub type RunnerRefreshResult = Result<(PluginsInventorySnapshot, PathBuf), String>;
type SharedUpdateFn =
    std::sync::Arc<dyn Fn(Option<PathBuf>, String, Vec<String>) -> RunnerFut<RunnerUpdateResult>>;
type SharedRefreshFn =
    std::sync::Arc<dyn Fn(Option<PathBuf>, String) -> RunnerFut<RunnerRefreshResult>>;

/// The CLI surface one update batch runs through; injectable so tests
/// drive a whole batch without shelling out. The production instance
/// wraps the `claude` subprocess calls.
#[derive(Clone)]
pub struct UpdateRunner {
    pub run_update: SharedUpdateFn,
    pub refresh: SharedRefreshFn,
}

impl std::fmt::Debug for UpdateRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateRunner").finish_non_exhaustive()
    }
}

impl UpdateRunner {
    /// The real runner: one `claude plugin update` call per row plus
    /// the closing inventory refresh.
    pub fn real() -> Self {
        Self {
            run_update: std::sync::Arc::new(|cached, cwd, args| {
                Box::pin(run_cli_command(cwd, cached, args))
            }),
            refresh: std::sync::Arc::new(|cached, cwd| Box::pin(refresh_inventory(cwd, cached))),
        }
    }
}

/// Everything one update batch needs beyond its queued rows. The
/// caller owns plan capture: which plugins, their per-row cwd, and the
/// marketplace clone HEADs a later rollback restores.
pub struct BatchPlan {
    pub cwd_context: String,
    pub claude_path: Option<PathBuf>,
    /// Upper bound on one CLI call inside the batch; a hung call fails
    /// its row instead of pinning the batch forever.
    pub call_timeout: std::time::Duration,
    pub marketplace_refs: std::collections::HashMap<String, String>,
    pub run: PluginUpdateRun,
}

/// The batch's outcome: the finished run, the rollback records for the
/// rows that moved, the plugins whose update stated the restart
/// contract, and the post-batch inventory when the refresh landed.
pub struct BatchOut {
    pub run: PluginUpdateRun,
    pub records: Vec<PluginUpdateRecord>,
    pub restart_required: Vec<String>,
    pub snapshot: Option<PluginsInventorySnapshot>,
    pub claude_path: Option<PathBuf>,
}

/// Run a queued update batch: one CLI call per row, sequentially,
/// continuing past failures. Every row reaches a terminal status in
/// plan order; `on_progress` observes each intermediate run state.
pub async fn execute_update_batch(
    mut plan: BatchPlan,
    runner: UpdateRunner,
    mut on_progress: impl FnMut(PluginUpdateRun),
) -> BatchOut {
    let row_count = plan.run.rows.len();
    for index in 0..row_count {
        if plan.run.rows[index].status != PluginRunRowStatus::Queued {
            continue;
        }
        plan.run.rows[index].status = PluginRunRowStatus::Updating;
        on_progress(plan.run.clone());
        let row = &plan.run.rows[index];
        let call = (runner.run_update)(
            plan.claude_path.clone(),
            row.cwd_raw.clone(),
            plugin_update_args(&row.plugin_id, &row.scope),
        );
        let result = match tokio::time::timeout(plan.call_timeout, call).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "`claude plugin update` timed out after {}s",
                plan.call_timeout.as_secs()
            )),
        };
        match result {
            Ok((path, output)) => {
                plan.claude_path = Some(path);
                plan.run.rows[index].detail = Some(output);
            }
            Err(message) => {
                plan.run.rows[index].status = PluginRunRowStatus::Failed;
                plan.run.rows[index].detail = Some(message);
            }
        }
    }

    let refresh = (runner.refresh)(plan.claude_path.clone(), plan.cwd_context.clone());
    let snapshot = match tokio::time::timeout(plan.call_timeout, refresh).await {
        Ok(Ok((snapshot, path))) => {
            plan.claude_path = Some(path);
            Some(snapshot)
        }
        Ok(Err(message)) => {
            fail_still_running(
                &mut plan.run,
                &format!("post-update inventory refresh failed: {message}"),
            );
            None
        }
        Err(_) => {
            fail_still_running(
                &mut plan.run,
                &format!(
                    "post-update inventory refresh timed out after {}s",
                    plan.call_timeout.as_secs()
                ),
            );
            None
        }
    };

    let mut records = Vec::new();
    let mut restart_required = Vec::new();
    for row in &mut plan.run.rows {
        if row.status != PluginRunRowStatus::Updating {
            continue;
        }
        if row.detail.as_deref().is_some_and(output_states_restart_required) {
            restart_required.push(row.plugin_id.clone());
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
        // The classifier drops the output on an applied update; state
        // the restart contract back on the row so the pane can show it
        // where the action was.
        if restart_required.contains(&row.plugin_id) && row.detail.is_none() {
            row.detail = Some("restart required to apply".to_owned());
        }
        if outcome.status == PluginRunRowStatus::Updated {
            records.push(PluginUpdateRecord {
                plugin_id: row.plugin_id.clone(),
                marketplace: row.marketplace.clone(),
                scope: row.scope.clone(),
                cwd_raw: row.cwd_raw.clone(),
                from_version: before,
                to_version: row.installed_version.clone(),
                marketplace_ref_before: plan.marketplace_refs.get(&row.marketplace).cloned(),
                updated_at: now_rfc3339(),
                trigger: plan.run.trigger,
            });
        }
    }

    plan.run.finished = true;
    on_progress(plan.run.clone());
    BatchOut { run: plan.run, records, restart_required, snapshot, claude_path: plan.claude_path }
}

fn fail_still_running(run: &mut PluginUpdateRun, message: &str) {
    for row in &mut run.rows {
        if row.status != PluginRunRowStatus::Updating {
            continue;
        }
        row.status = PluginRunRowStatus::Failed;
        // The captured CLI output is the only evidence the update may
        // have applied; keep it on the row.
        let output = row.detail.take().unwrap_or_default();
        row.detail = Some(if output.is_empty() {
            message.to_owned()
        } else {
            format!("{output} | {message}")
        });
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    use forge_primitives::plugins::{PluginUpdateRunRow, PluginUpdateTrigger};

    // -- sequential batch -------------------------------------------------

    fn queued_row(id: &str, version: &str) -> PluginUpdateRunRow {
        PluginUpdateRunRow::queued(
            format!("{id}@probe-market"),
            "user".to_owned(),
            String::new(),
            Some(version.to_owned()),
        )
    }

    /// A runner whose update calls fail for plugins named in `failing`
    /// and otherwise report an applied update carrying the restart
    /// contract; the refresh reports every plugin moved one version.
    fn fake_runner(
        failing: &[&str],
    ) -> (UpdateRunner, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let failing: Vec<String> = failing.iter().map(|s| format!("{s}@probe-market")).collect();
        let calls: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_update = std::sync::Arc::clone(&calls);
        let runner = UpdateRunner {
            run_update: std::sync::Arc::new(move |_, _, args| {
                let id = args[2].clone();
                let failing = failing.clone();
                let calls = std::sync::Arc::clone(&calls_update);
                Box::pin(async move {
                    calls.lock().expect("call log").push(id.clone());
                    if failing.contains(&id) {
                        Err(format!("`claude plugin update` failed for {id}"))
                    } else {
                        Ok((
                            PathBuf::from("claude"),
                            format!(
                                "Plugin \"{id}\" updated from 1.0.0 to 2.0.0 for scope user. \
                                     Restart required to apply."
                            ),
                        ))
                    }
                })
            }),
            refresh: std::sync::Arc::new(|_, _| {
                Box::pin(async {
                    let entry =
                        |id: &str, version: &str| forge_primitives::plugins::InstalledPluginEntry {
                            id: format!("{id}@probe-market"),
                            version: Some(version.to_owned()),
                            scope: "user".to_owned(),
                            enabled: true,
                            installed_at: None,
                            last_updated: None,
                            project_path: None,
                            capability: forge_primitives::plugins::PluginCapability::Skill,
                        };
                    let snapshot = forge_primitives::plugins::PluginsInventorySnapshot {
                        installed: vec![
                            entry("first", "2.0.0"),
                            entry("second", "1.0.0"),
                            entry("third", "2.0.0"),
                        ],
                        ..Default::default()
                    };
                    Ok((snapshot, PathBuf::from("claude")))
                })
            }),
        };
        (runner, calls)
    }

    fn three_row_plan() -> BatchPlan {
        BatchPlan {
            cwd_context: "/proj".to_owned(),
            claude_path: None,
            call_timeout: std::time::Duration::from_secs(5),
            marketplace_refs: std::collections::HashMap::new(),
            run: PluginUpdateRun {
                trigger: PluginUpdateTrigger::Manual,
                finished: false,
                rows: vec![
                    queued_row("first", "1.0.0"),
                    queued_row("second", "1.0.0"),
                    queued_row("third", "1.0.0"),
                ],
            },
        }
    }

    #[tokio::test]
    async fn a_mid_batch_failure_does_not_stop_the_batch() {
        let (runner, calls) = fake_runner(&["second"]);
        let progress: std::sync::Arc<std::sync::Mutex<Vec<Vec<PluginRunRowStatus>>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&progress);

        let out = execute_update_batch(three_row_plan(), runner, move |run| {
            sink.lock()
                .expect("progress log")
                .push(run.rows.iter().map(|row| row.status).collect());
        })
        .await;

        assert!(out.run.finished);
        let statuses: Vec<PluginRunRowStatus> = out.run.rows.iter().map(|row| row.status).collect();
        assert_eq!(
            statuses,
            vec![
                PluginRunRowStatus::Updated,
                PluginRunRowStatus::Failed,
                PluginRunRowStatus::Updated
            ],
            "every row reached a terminal status in plan order; got {statuses:?}"
        );
        assert_eq!(
            *calls.lock().expect("call log"),
            vec![
                "first@probe-market".to_owned(),
                "second@probe-market".to_owned(),
                "third@probe-market".to_owned(),
            ],
            "the later rows were still attempted"
        );
        assert_eq!(out.run.summary(), "2 updated, 1 failed, 0 current");
        assert_eq!(out.run.finished_count(), 2, "the panel's done count for the failed batch");
        let last = progress.lock().expect("progress frames").last().cloned().unwrap_or_default();
        assert_eq!(last.len(), 3, "the final progress frame carries all rows");
        assert!(out.snapshot.is_some(), "the post-batch refresh landed");
    }

    #[tokio::test]
    async fn restart_required_is_parsed_onto_the_row() {
        let (runner, _) = fake_runner(&[]);
        let out = execute_update_batch(three_row_plan(), runner, |_| {}).await;
        let mut ids = out.restart_required.clone();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "first@probe-market".to_owned(),
                "second@probe-market".to_owned(),
                "third@probe-market".to_owned()
            ],
            "every applied update in the fake states the contract; got {:?}",
            out.restart_required
        );
    }

    #[tokio::test]
    async fn a_refresh_failure_fails_only_the_still_running_rows() {
        let runner = UpdateRunner {
            run_update: std::sync::Arc::new(|_, _, args| {
                let id = args[2].clone();
                Box::pin(async move {
                    Ok((
                        PathBuf::from("claude"),
                        format!(
                            "Plugin \"{id}\" updated from 1.0.0 to 2.0.0 for scope user. \
                                 Restart required to apply."
                        ),
                    ))
                })
            }),
            refresh: std::sync::Arc::new(|_, _| {
                Box::pin(async { Err("claude not found".to_owned()) })
            }),
        };
        let out = execute_update_batch(three_row_plan(), runner, |_| {}).await;
        assert!(out.snapshot.is_none());
        assert!(
            out.run.rows.iter().all(|row| row.status == PluginRunRowStatus::Failed),
            "with no post-run inventory every row is undecidable and reads as failed: {:?}",
            out.run.rows
        );
        assert!(
            out.run
                .rows
                .iter()
                .all(|row| row.detail.as_deref().is_some_and(|d| d.contains("refresh failed"))),
            "the failure names the refresh: {:?}",
            out.run.rows.iter().map(|row| row.detail.clone()).collect::<Vec<_>>()
        );
        assert_eq!(out.run.finished_count(), 0);
    }

    const DETAILS_OUTPUT: &str = "superpowers 6.3.0\n  Description: Core skills library\n  Source: superpowers@claude-plugins-official\n\nComponent inventory\n  Skills (14)  brainstorming, test-driven-development\n  Agents (0)\n  Hooks (1)  SessionStart  (harness-only - no model context cost)\n  MCP servers (0)\n  LSP servers (0)\n\nProjected token cost\n  Always-on:   ~450 tok   added to every session\n\nPer-component (rounded)\n  component                       always-on  on-invoke\n  using-git-worktrees                   ~40      ~1.5k\n";

    #[test]
    fn always_on_tokens_parse_from_recorded_details_output() {
        assert_eq!(parse_always_on_tokens(DETAILS_OUTPUT), Some(450));
        assert_eq!(parse_always_on_tokens("no cost section"), None);
    }

    #[test]
    fn restart_required_reads_the_cli_contract_line() {
        assert!(output_states_restart_required(
            "Plugin \"hello\" updated from 0.2.0 to 0.3.0 for scope user. Restart required to apply."
        ));
        assert!(!output_states_restart_required("hello is already at the latest version (0.2.0)."));
    }

    #[tokio::test]
    async fn the_token_cost_cache_refetches_only_on_a_version_change() {
        let mut cache = TokenCostCache::default();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0_u32));
        let mut fetch = |plugin: String| {
            calls.set(calls.get() + 1);
            assert_eq!(plugin, "superpowers@probe-market");
            let version = if calls.get() == 1 { "6.3.0" } else { "6.4.0" };
            let version = version.to_owned();
            async move {
                Ok(PluginDetails {
                    token_cost_always_on: if version == "6.3.0" { 450 } else { 460 },
                })
            }
        };

        let first = cache.details_for("superpowers@probe-market", Some("6.3.0"), &mut fetch).await;
        assert_eq!(first.expect("first fetch").expect("some cost").token_cost_always_on, 450);
        // Same version again: served from the cache, the fetcher never runs.
        let second = cache.details_for("superpowers@probe-market", Some("6.3.0"), &mut fetch).await;
        assert_eq!(second.expect("cached").expect("some cost").token_cost_always_on, 450);
        assert_eq!(calls.get(), 1, "no refetch on an unchanged version");
        // A moved version refetches.
        let third = cache.details_for("superpowers@probe-market", Some("6.4.0"), &mut fetch).await;
        assert_eq!(third.expect("refetched").expect("some cost").token_cost_always_on, 460);
        assert_eq!(calls.get(), 2, "the version change drives exactly one refetch");
    }

    #[test]
    fn a_versionless_plugin_never_fetches_token_cost() {
        let mut cache = TokenCostCache::default();
        let mut fetch = |_: String| async { Ok(PluginDetails { token_cost_always_on: 1 }) };
        let result =
            futures::executor::block_on(cache.details_for("x@probe-market", None, &mut fetch));
        assert_eq!(result.expect("no version reads as no cost"), None);
    }

    #[test]
    fn plugin_update_args_pin_the_cli_invocation() {
        assert_eq!(
            plugin_update_args("hello@probe-market", "user"),
            vec![
                "plugin".to_owned(),
                "update".to_owned(),
                "hello@probe-market".to_owned(),
                "--scope".to_owned(),
                "user".to_owned(),
            ]
        );
    }

    #[test]
    fn rollback_git_steps_fetch_then_detach_the_recorded_ref() {
        let steps = rollback_git_args("/clone/path", "2d7d4c6");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], vec!["-C", "/clone/path", "fetch", "origin", "2d7d4c6"]);
        assert_eq!(steps[1], vec!["-C", "/clone/path", "checkout", "--detach", "2d7d4c6"]);
    }

    /// Install-from-the-old-manifest must run BEFORE the marketplace
    /// clone moves forward again; swapped, the rollback would silently
    /// re-update the plugin to the new version.
    #[test]
    fn rollback_claude_steps_update_then_restore_the_clone() {
        let record = PluginUpdateRecord {
            plugin_id: "hello@probe-market".to_owned(),
            marketplace: "probe-market".to_owned(),
            scope: "user".to_owned(),
            cwd_raw: "/proj".to_owned(),
            from_version: Some("0.1.0".to_owned()),
            to_version: Some("0.2.0".to_owned()),
            marketplace_ref_before: Some("2d7d4c6".to_owned()),
            updated_at: String::new(),
            trigger: forge_primitives::plugins::PluginUpdateTrigger::Manual,
        };
        let steps = rollback_claude_args(&record);
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[0],
            vec![
                "plugin".to_owned(),
                "update".to_owned(),
                "hello@probe-market".to_owned(),
                "--scope".to_owned(),
                "user".to_owned(),
            ]
        );
        assert_eq!(
            steps[1],
            vec![
                "plugin".to_owned(),
                "marketplace".to_owned(),
                "update".to_owned(),
                "probe-market".to_owned(),
            ]
        );
    }

    #[test]
    fn parses_installed_plugin_entries() {
        let json = r#"
[
  {
    "id": "frontend-design@claude-plugins-official",
    "version": "55b58ec6e564",
    "scope": "local",
    "enabled": false,
    "installedAt": "2026-02-05T15:37:39.555Z",
    "lastUpdated": "2026-03-02T18:10:00.820Z",
    "projectPath": "C:\\work"
  }
]
"#;

        let parsed = serde_json::from_str::<Vec<InstalledPluginJson>>(json).expect("parse json");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "frontend-design@claude-plugins-official");
        assert_eq!(parsed[0].scope, "local");
        assert!(!parsed[0].enabled);
        assert_eq!(parsed[0].project_path.as_deref(), Some("C:\\work"));
    }

    #[test]
    fn detects_mcp_plugins_from_installed_payload() {
        let json = r#"
[
  {
    "id": "supabase@claude-plugins-official",
    "scope": "local",
    "enabled": true,
    "mcpServers": {
      "supabase": {
        "type": "http",
        "url": "https://mcp.supabase.com/mcp"
      }
    }
  }
]
"#;

        let parsed = serde_json::from_str::<Vec<InstalledPluginJson>>(json).expect("parse json");
        let entry = InstalledPluginEntry {
            id: parsed[0].id.clone(),
            version: parsed[0].version.clone(),
            scope: parsed[0].scope.clone(),
            enabled: parsed[0].enabled,
            installed_at: parsed[0].installed_at.clone(),
            last_updated: parsed[0].last_updated.clone(),
            project_path: parsed[0].project_path.clone(),
            capability: if parsed[0].mcp_servers.is_some() {
                PluginCapability::Mcp
            } else {
                PluginCapability::Skill
            },
        };

        assert_eq!(entry.capability, PluginCapability::Mcp);
    }

    #[test]
    fn parses_marketplace_entries_and_sources() {
        let available_json = r#"
{
  "installed": [],
  "available": [
    {
      "pluginId": "frontend-design@claude-plugins-official",
      "name": "frontend-design",
      "description": "Create distinctive interfaces",
      "marketplaceName": "claude-plugins-official",
      "version": "1.0.0",
      "source": "./plugins/frontend-design",
      "installCount": 42
    }
  ]
}
"#;
        let source_json = r#"
[
  {
    "name": "claude-plugins-official",
    "source": "github",
    "repo": "anthropics/claude-plugins-official",
    "installLocation": "/tmp/claude/plugins/marketplaces/claude-plugins-official"
  }
]
"#;

        let parsed_available =
            serde_json::from_str::<MarketplaceListJson>(available_json).expect("parse available");
        let parsed_sources =
            serde_json::from_str::<Vec<MarketplaceSourceJson>>(source_json).expect("parse sources");

        assert_eq!(parsed_available.available.len(), 1);
        assert_eq!(
            parsed_available.available[0].marketplace_name.as_deref(),
            Some("claude-plugins-official")
        );
        assert_eq!(parsed_available.available[0].install_count, Some(42));
        assert_eq!(parsed_sources[0].repo.as_deref(), Some("anthropics/claude-plugins-official"));
        assert_eq!(
            parsed_sources[0].install_location.as_deref(),
            Some("/tmp/claude/plugins/marketplaces/claude-plugins-official"),
            "rollback and ref recording need the clone location"
        );
    }
}
