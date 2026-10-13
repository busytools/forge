//! Plugin and marketplace actions over the command bus.
//!
//! The work is reachable through [`crate::Command`] so that any view can
//! act, rather than one view reaching for the `forge-agent` CLI functions
//! this module calls from inside its own layer (rule 24's case). The CLI
//! calls, the batch runner and the disk scan are `forge-agent`'s; what
//! lives here is the orchestration: resolving each action's working
//! directory from `forge.toml` rather than the launch directory (hard
//! rule 14), serializing actions one at a time, and reporting through the
//! `Plugins*` updates a view already listens to.
//!
//! One run at a time: an update-all, a check and an action all
//! serialize through [`Workspace::plugin_action_running`], and the latest
//! run stays readable mid-flight so a view attaching during one draws it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use forge_agent::userdata::plugins::cli::{self, UpdateRunner};
use forge_primitives::plugins::{
    InstalledPluginEntry, MarketplaceEntry, MarketplaceSourceEntry, PluginRunRowStatus,
    PluginUpdateRun, PluginUpdateRunRow, PluginUpdateTrigger, PluginsCliActionSuccess,
    PluginsInventorySnapshot, update_availability,
};

use crate::protocol::{DispatchError, SessionUpdate};
use crate::workspace::Workspace;

/// Upper bound on one CLI call inside a batch; a hung call fails its row
/// instead of pinning the run forever.
const PLUGIN_CALL_TIMEOUT: Duration = Duration::from_secs(180);

/// One future handed back by the plugin CLI seams.
type PluginCliFut<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;
type RunAndRefresh = Arc<
    dyn Fn(String, Vec<String>) -> PluginCliFut<Result<(PluginsInventorySnapshot, PathBuf), String>>
        + Send
        + Sync,
>;
type RefreshFn = Arc<
    dyn Fn(String) -> PluginCliFut<Result<(PluginsInventorySnapshot, PathBuf), String>>
        + Send
        + Sync,
>;
type ScanFn = Arc<dyn Fn() -> Option<PluginsInventorySnapshot> + Send + Sync>;

/// How the plugin actions reach `claude` and the disk. Real in every
/// production build.
#[derive(Clone)]
pub(crate) struct PluginCli {
    /// Run one `claude plugin ...` action and refresh the inventory after
    /// it, both in the given working directory.
    pub run_and_refresh: RunAndRefresh,
    /// Refresh the inventory without an action (the update check).
    pub refresh: RefreshFn,
    /// The batch runner one `PluginUpdateAll` drives.
    pub runner: UpdateRunner,
    /// The disk-only scan: registry, manifests and the plugin cache, no
    /// `claude` spawn. `None` when no plugins root resolves.
    pub scan: ScanFn,
}

impl PluginCli {
    pub(crate) fn real() -> Self {
        Self {
            run_and_refresh: Arc::new(|cwd, args| {
                Box::pin(cli::run_cli_command_and_refresh(cwd, None, args))
            }),
            refresh: Arc::new(|cwd| Box::pin(cli::refresh_inventory(cwd, None))),
            runner: UpdateRunner::real(),
            scan: Arc::new(cli::inventory_from_disk_blocking),
        }
    }
}

/// Clears the single-action gate however its task ends, panic included: a
/// crashed task that left the gate shut would refuse every later press
/// with no way back.
struct PluginActionGuard(Arc<Workspace>);

impl Drop for PluginActionGuard {
    fn drop(&mut self) {
        self.0.plugin_action_running.store(false, Ordering::Release);
    }
}

/// The working directory one plugin CLI call runs in.
///
/// `project`/`local` scopes resolve against a declared project - the
/// entry's own `project_path` for an update, the named project for an
/// install - so the call always runs somewhere `forge.toml` names. Every
/// other scope (the CLI's `user`, and anything a future CLI adds) runs in
/// the home directory. Never the process cwd: what a plugin call does must
/// not vary with where forge was launched (hard rule 14).
fn plugin_call_cwd(scope: &str, project_path: Option<&str>, home: &str) -> String {
    match scope {
        "local" | "project" => {
            project_path.filter(|path| !path.is_empty()).unwrap_or(home).to_owned()
        }
        _ => home.to_owned(),
    }
}

/// The disk scan, off the runtime.
///
/// The scan walks the plugin tree, so it runs on a blocking thread exactly
/// as the read path's own does - a dispatch reached from the connection
/// must never stall a shared worker on it. The two failures stay distinct:
/// a scan that could not run names the join, and a scan with no root to
/// walk says so rather than reading as an empty machine.
async fn scan_inventory(cli: &PluginCli) -> Result<PluginsInventorySnapshot, String> {
    let scan = Arc::clone(&cli.scan);
    tokio::task::spawn_blocking(move || scan())
        .await
        .map_err(|error| format!("the plugin disk scan failed to join: {error}"))?
        .ok_or_else(|| {
            "no plugins root resolves (no CLAUDE_CONFIG_DIR or HOME); the inventory cannot be read"
                .to_owned()
        })
}

/// Report a failed action, or a refusal found before one could run, on the
/// frame the page already listens for.
fn fail_action(ws: &Arc<Workspace>, cwd: String, message: String) {
    tracing::warn!(
        event_name = "plugin_cli_action_failed",
        cwd = %cwd,
        error = %message,
        "a plugin action failed",
    );
    let _ =
        ws.update_sender().send(SessionUpdate::PluginsCliActionFailed { cwd_raw: cwd, message });
}

/// Run one CLI action and report its outcome on the `Plugins` frames.
async fn run_action(
    ws: &Arc<Workspace>,
    cli: &PluginCli,
    cwd: String,
    args: Vec<String>,
    message: String,
) {
    match (cli.run_and_refresh)(cwd.clone(), args).await {
        Ok((snapshot, claude_path)) => {
            let _ = ws.update_sender().send(SessionUpdate::PluginsCliActionSucceeded {
                cwd_raw: cwd,
                result: PluginsCliActionSuccess { snapshot, message, claude_path },
            });
        }
        Err(message) => fail_action(ws, cwd, message),
    }
}

/// The run rows one update-all/check queues: exactly the stale installed
/// entries, one per registry entry, each with the cwd its own scope runs
/// in.
fn stale_rows(
    installed: &[InstalledPluginEntry],
    marketplace: &[MarketplaceEntry],
    home: &str,
) -> Vec<PluginUpdateRunRow> {
    update_availability(installed, marketplace)
        .into_iter()
        .map(|stale| {
            let project_path = installed
                .iter()
                .find(|entry| entry.id == stale.plugin_id && entry.scope == stale.scope)
                .and_then(|entry| entry.project_path.as_deref());
            let mut row = PluginUpdateRunRow::queued(
                stale.plugin_id,
                stale.scope.clone(),
                plugin_call_cwd(&stale.scope, project_path, home),
                stale.installed_version,
            );
            row.available_version = stale.available_version;
            row
        })
        .collect()
}

impl Workspace {
    /// The last plugin update run or check, in flight or finished, for the
    /// Extensions read. `None` until one has happened this process.
    pub fn plugin_update_run(&self) -> Option<PluginUpdateRun> {
        self.plugin_run.lock().clone()
    }

    /// The disk-only plugin inventory, scanned on the call: registry,
    /// marketplace manifests and the plugin cache, no `claude` spawn.
    /// `None` when no plugins root resolves.
    pub async fn plugins_inventory(&self) -> Option<PluginsInventorySnapshot> {
        let cli = self.plugin_cli();
        tokio::task::spawn_blocking(move || (cli.scan)()).await.ok().flatten()
    }

    #[cfg(test)]
    pub(crate) fn set_plugin_cli(&self, cli: PluginCli) {
        *self.plugin_cli.lock() = cli;
    }

    /// Replace the disk scan, so a cross-crate test can answer the
    /// Extensions read with a fixture instead of this machine's plugins
    /// root. Test-only. Takes a closure rather than the seam, so the
    /// crate's own types stay private.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_plugin_scan_for_test(
        &self,
        scan: impl Fn() -> Option<PluginsInventorySnapshot> + Send + Sync + 'static,
    ) {
        self.plugin_cli.lock().scan = Arc::new(scan);
    }

    fn plugin_cli(&self) -> PluginCli {
        self.plugin_cli.lock().clone()
    }

    /// Take the single-action gate, or refuse naming the one in flight.
    fn begin_plugin_action(&self) -> Result<(), DispatchError> {
        if self.plugin_action_running.swap(true, Ordering::AcqRel) {
            return Err(DispatchError::PluginActionRunning);
        }
        Ok(())
    }

    /// The home directory plugin calls with no project context run in.
    /// Unavailable means the action fails rather than substituting another
    /// directory (hard rule 14).
    fn plugin_home() -> Result<String, DispatchError> {
        dirs::home_dir()
            .map(|home| home.to_string_lossy().into_owned())
            .ok_or(DispatchError::PluginHomeUnavailable)
    }

    /// The declared project's path, by its `forge.toml` name.
    fn declared_project_path(&self, name: &str) -> Result<String, DispatchError> {
        self.list_projects()
            .into_iter()
            .find(|project| project.name == name)
            .map(|project| project.path.to_string_lossy().into_owned())
            .ok_or_else(|| DispatchError::ProjectNotFound { name: name.to_owned() })
    }

    /// Install one plugin at a scope.
    pub(crate) fn start_plugin_install(
        self: &Arc<Self>,
        plugin_id: String,
        scope: String,
        project: Option<&str>,
    ) -> Result<(), DispatchError> {
        let home = Self::plugin_home()?;
        let cwd = match scope.as_str() {
            "local" | "project" => match project {
                Some(name) => self.declared_project_path(name)?,
                None => {
                    return Err(DispatchError::PluginProjectRequired);
                }
            },
            _ => home,
        };
        let message = match scope.as_str() {
            "project" | "local" => format!("Installed {plugin_id} for {scope} scope"),
            _ => format!("Installed {plugin_id} for user scope"),
        };
        let args =
            vec!["plugin".to_owned(), "install".to_owned(), plugin_id, "--scope".to_owned(), scope];
        self.start_plugin_cli_action(cwd, args, message)
    }

    /// Update one installed plugin, in the scope and project the registry
    /// records for it.
    ///
    /// The registry lookup is a disk scan, so it runs inside the spawned
    /// task: the dispatch is fire-and-forget and its outcome rides the
    /// `Plugins` frames, a refusal included - a plugin the registry does
    /// not hold in that scope reports as a failed action naming it.
    pub(crate) fn start_plugin_update(
        self: &Arc<Self>,
        plugin_id: String,
        scope: String,
    ) -> Result<(), DispatchError> {
        let home = Self::plugin_home()?;
        self.begin_plugin_action()?;
        let ws = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = PluginActionGuard(Arc::clone(&ws));
            let cli = ws.plugin_cli();
            let snapshot = match scan_inventory(&cli).await {
                Ok(snapshot) => snapshot,
                Err(message) => return fail_action(&ws, home, message),
            };
            let Some(entry) = snapshot
                .installed
                .iter()
                .find(|entry| entry.id == plugin_id && entry.scope == scope)
                .cloned()
            else {
                return fail_action(
                    &ws,
                    home,
                    format!(
                        "plugin '{plugin_id}' is not installed in {scope} scope; refresh the page"
                    ),
                );
            };
            let cwd = plugin_call_cwd(&scope, entry.project_path.as_deref(), &home);
            let message = format!("Updated {plugin_id} in {scope} scope");
            let args = vec![
                "plugin".to_owned(),
                "update".to_owned(),
                plugin_id,
                "--scope".to_owned(),
                scope,
            ];
            run_action(&ws, &cli, cwd, args, message).await;
        });
        Ok(())
    }

    /// One generic CLI action: gate, spawn, report. The outcome rides
    /// [`SessionUpdate::PluginsCliActionSucceeded`] / `Failed`.
    fn start_plugin_cli_action(
        self: &Arc<Self>,
        cwd: String,
        args: Vec<String>,
        message: String,
    ) -> Result<(), DispatchError> {
        self.begin_plugin_action()?;
        let ws = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = PluginActionGuard(Arc::clone(&ws));
            let cli = ws.plugin_cli();
            run_action(&ws, &cli, cwd, args, message).await;
        });
        Ok(())
    }

    /// Update exactly the stale installed plugins, one CLI call per entry,
    /// continuing past failures. The set is computed from a disk scan taken
    /// inside the run's own task, so the dispatch never walks the tree on
    /// the runtime and the run updates what the marketplace reports now.
    ///
    /// Nothing stale is not a refusal: the run lands finished with no rows,
    /// exactly as a check with nothing to report does, and the page draws
    /// its ordinary state.
    pub(crate) fn start_plugin_update_all(self: &Arc<Self>) -> Result<(), DispatchError> {
        let home = Self::plugin_home()?;
        self.begin_plugin_action()?;
        let ws = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = PluginActionGuard(Arc::clone(&ws));
            let cli = ws.plugin_cli();
            let snapshot = match scan_inventory(&cli).await {
                Ok(snapshot) => snapshot,
                Err(message) => return fail_action(&ws, home, message),
            };
            let rows = stale_rows(&snapshot.installed, &snapshot.marketplace, &home);
            if rows.is_empty() {
                let run =
                    PluginUpdateRun { trigger: PluginUpdateTrigger::Manual, finished: true, rows };
                *ws.plugin_run.lock() = Some(run.clone());
                let _ = ws.update_sender().send(SessionUpdate::PluginsUpdateRunFinished {
                    cwd_raw: home,
                    run,
                    snapshot: Some(snapshot),
                    claude_path: None,
                });
                return;
            }
            let run =
                PluginUpdateRun { trigger: PluginUpdateTrigger::Manual, finished: false, rows };
            *ws.plugin_run.lock() = Some(run.clone());
            let marketplaces = snapshot.marketplaces;
            run_update_batch(ws, cli, home, marketplaces, run).await;
        });
        Ok(())
    }

    /// Refresh the inventory through the CLI and report which installed
    /// plugins have a newer marketplace version, applying nothing.
    pub(crate) fn start_plugin_check(self: &Arc<Self>) -> Result<(), DispatchError> {
        let home = Self::plugin_home()?;
        self.begin_plugin_action()?;
        let ws = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = PluginActionGuard(Arc::clone(&ws));
            let cli = ws.plugin_cli();
            match (cli.refresh)(home.clone()).await {
                Ok((snapshot, claude_path)) => {
                    let rows = update_availability(&snapshot.installed, &snapshot.marketplace)
                        .into_iter()
                        .map(|stale| PluginUpdateRunRow {
                            plugin_id: stale.plugin_id,
                            scope: stale.scope,
                            cwd_raw: String::new(),
                            marketplace: stale.marketplace,
                            status: PluginRunRowStatus::UpdateAvailable,
                            installed_version: stale.installed_version,
                            available_version: stale.available_version,
                            detail: None,
                        })
                        .collect();
                    let run = PluginUpdateRun {
                        trigger: PluginUpdateTrigger::Manual,
                        finished: true,
                        rows,
                    };
                    *ws.plugin_run.lock() = Some(run.clone());
                    let _ = ws.update_sender().send(SessionUpdate::PluginsUpdateRunFinished {
                        cwd_raw: home,
                        run,
                        snapshot: Some(snapshot),
                        claude_path: Some(claude_path),
                    });
                }
                Err(message) => {
                    tracing::warn!(
                        event_name = "plugin_update_check_failed",
                        error = %message,
                        "the plugin update check could not refresh the inventory",
                    );
                    let _ = ws.update_sender().send(SessionUpdate::PluginsInventoryRefreshFailed {
                        cwd_raw: home,
                        message,
                        trigger: PluginUpdateTrigger::Manual,
                    });
                }
            }
        });
        Ok(())
    }

    /// Add a marketplace by `owner/repo` or URL, at user scope.
    pub(crate) fn start_marketplace_add(
        self: &Arc<Self>,
        source: String,
    ) -> Result<(), DispatchError> {
        let home = Self::plugin_home()?;
        let message = format!("Added marketplace {source}");
        let args = vec![
            "plugin".to_owned(),
            "marketplace".to_owned(),
            "add".to_owned(),
            source,
            "--scope".to_owned(),
            "user".to_owned(),
        ];
        self.start_plugin_cli_action(home, args, message)
    }

    /// Repair a drifted marketplace with the CLI's remove-and-re-add pair.
    /// The re-add source is the repo for a git marketplace and the clone
    /// location for a directory one.
    /// The registry lookup is a disk scan, so it runs inside the task.
    pub(crate) fn start_marketplace_repair(
        self: &Arc<Self>,
        name: String,
    ) -> Result<(), DispatchError> {
        let home = Self::plugin_home()?;
        self.begin_plugin_action()?;
        let ws = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = PluginActionGuard(Arc::clone(&ws));
            let cli = ws.plugin_cli();
            let snapshot = match scan_inventory(&cli).await {
                Ok(snapshot) => snapshot,
                Err(message) => return fail_action(&ws, home, message),
            };
            let Some(marketplace) =
                snapshot.marketplaces.iter().find(|marketplace| marketplace.name == name).cloned()
            else {
                return fail_action(
                    &ws,
                    home,
                    format!("no marketplace named '{name}' is configured; refresh the page"),
                );
            };
            let readd_source = match marketplace.source.as_deref() {
                Some("directory") => marketplace
                    .install_location
                    .clone()
                    .or_else(|| marketplace.repo.clone())
                    .unwrap_or_else(|| name.clone()),
                _ => marketplace.repo.clone().unwrap_or_else(|| name.clone()),
            };
            let remove = vec![
                "plugin".to_owned(),
                "marketplace".to_owned(),
                "remove".to_owned(),
                name.clone(),
            ];
            let add = vec![
                "plugin".to_owned(),
                "marketplace".to_owned(),
                "add".to_owned(),
                readd_source,
                "--scope".to_owned(),
                "user".to_owned(),
            ];
            let outcome = match (cli.run_and_refresh)(home.clone(), remove).await {
                Ok(_) => (cli.run_and_refresh)(home.clone(), add).await,
                Err(message) => Err(message),
            };
            match outcome {
                Ok((snapshot, claude_path)) => {
                    let _ = ws.update_sender().send(SessionUpdate::PluginsCliActionSucceeded {
                        cwd_raw: home,
                        result: PluginsCliActionSuccess {
                            snapshot,
                            message: format!("Repaired {name} marketplace"),
                            claude_path,
                        },
                    });
                }
                Err(message) => fail_action(&ws, home, message),
            }
        });
        Ok(())
    }
}

/// Run one update batch, reporting progress per row, persisting the
/// rollback records it earns, and finishing with the refreshed inventory.
async fn run_update_batch(
    ws: Arc<Workspace>,
    cli: PluginCli,
    home: String,
    marketplaces: Vec<MarketplaceSourceEntry>,
    run: PluginUpdateRun,
) {
    // The clone HEADs before the run, so an applied update records the ref
    // a rollback later restores. Captured once, from the dispatch's own
    // scan, before any plugin moves.
    let mut marketplace_refs = std::collections::HashMap::new();
    for marketplace in marketplaces {
        if let Some(location) = marketplace.install_location
            && let Some(head) = cli::marketplace_head(location).await
        {
            marketplace_refs.insert(marketplace.name, head);
        }
    }
    let plan = cli::BatchPlan {
        cwd_context: home.clone(),
        claude_path: None,
        call_timeout: PLUGIN_CALL_TIMEOUT,
        marketplace_refs,
        run,
    };
    let progress_ws = Arc::clone(&ws);
    let progress_home = home.clone();
    let out = cli::execute_update_batch(plan, cli.runner, move |run| {
        *progress_ws.plugin_run.lock() = Some(run.clone());
        let _ = progress_ws
            .update_sender()
            .send(SessionUpdate::PluginsUpdateRunProgress { cwd_raw: progress_home.clone(), run });
    })
    .await;
    if !out.records.is_empty() {
        ws.record_plugin_updates(&out.records);
    }
    *ws.plugin_run.lock() = Some(out.run.clone());
    let _ = ws.update_sender().send(SessionUpdate::PluginsUpdateRunFinished {
        cwd_raw: home,
        run: out.run,
        snapshot: out.snapshot,
        claude_path: out.claude_path,
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use forge_primitives::plugins::{
        InstalledPluginEntry, MarketplaceSourceEntry, PluginCapability, PluginsInventorySnapshot,
    };

    use super::*;
    use crate::protocol::Command;
    use crate::workspace::Workspace;

    /// One recorded call into the fake CLI.
    #[derive(Debug, Clone, PartialEq)]
    struct CliCall {
        cwd: String,
        args: Vec<String>,
    }

    /// What the fake CLI records, shared with the test.
    #[derive(Default)]
    struct FakeCli {
        run: Mutex<Vec<CliCall>>,
        refresh: Mutex<Vec<CliCall>>,
        updates: Mutex<Vec<CliCall>>,
    }

    fn marketplace(
        name: &str,
        source: &str,
        repo: Option<&str>,
        location: Option<&str>,
    ) -> MarketplaceSourceEntry {
        MarketplaceSourceEntry {
            name: name.to_owned(),
            source: Some(source.to_owned()),
            repo: repo.map(str::to_owned),
            install_location: location.map(str::to_owned),
        }
    }

    fn installed(
        id: &str,
        version: &str,
        scope: &str,
        project_path: Option<&str>,
    ) -> InstalledPluginEntry {
        InstalledPluginEntry {
            id: id.to_owned(),
            version: Some(version.to_owned()),
            scope: scope.to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: project_path.map(str::to_owned),
            capability: PluginCapability::Skill,
        }
    }

    fn marketplace_entry(id: &str, version: &str) -> MarketplaceEntry {
        MarketplaceEntry {
            plugin_id: id.to_owned(),
            name: id.split('@').next().unwrap_or(id).to_owned(),
            description: None,
            marketplace_name: None,
            version: Some(version.to_owned()),
            install_count: None,
            source: None,
        }
    }

    /// A fake CLI whose scan answers `scan`, whose every action and
    /// refresh answers `refreshed`, and which records every call.
    fn fake_cli(
        scan: &PluginsInventorySnapshot,
        refreshed: &PluginsInventorySnapshot,
    ) -> (PluginCli, Arc<FakeCli>) {
        let calls = Arc::new(FakeCli::default());
        let run = {
            let calls = Arc::clone(&calls);
            let refreshed = refreshed.clone();
            Arc::new(move |cwd: String, args: Vec<String>| {
                calls.run.lock().expect("lock").push(CliCall { cwd, args });
                let refreshed = refreshed.clone();
                Box::pin(async move { Ok((refreshed, PathBuf::from("/fake/claude"))) })
                    as PluginCliFut<Result<(PluginsInventorySnapshot, PathBuf), String>>
            })
        };
        let refresh = {
            let calls = Arc::clone(&calls);
            let refreshed = refreshed.clone();
            Arc::new(move |cwd: String| {
                calls.refresh.lock().expect("lock").push(CliCall { cwd, args: vec![] });
                let refreshed = refreshed.clone();
                Box::pin(async move { Ok((refreshed, PathBuf::from("/fake/claude"))) })
                    as PluginCliFut<Result<(PluginsInventorySnapshot, PathBuf), String>>
            })
        };
        let updates = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_cached: Option<PathBuf>, cwd: String, args: Vec<String>| {
                calls.updates.lock().expect("lock").push(CliCall { cwd, args });
                Box::pin(async move {
                    Ok((PathBuf::from("/fake/claude"), "the CLI said so".to_owned()))
                })
                    as forge_agent::userdata::plugins::cli::RunnerFut<
                        Result<(PathBuf, String), String>,
                    >
            })
        };
        let batch_refresh = {
            let refreshed = refreshed.clone();
            std::sync::Arc::new(move |_cached: Option<PathBuf>, _cwd: String| {
                let refreshed = refreshed.clone();
                Box::pin(async move { Ok((refreshed, PathBuf::from("/fake/claude"))) })
                    as forge_agent::userdata::plugins::cli::RunnerFut<
                        Result<(PluginsInventorySnapshot, PathBuf), String>,
                    >
            })
        };
        let scan_snapshot = scan.clone();
        let cli = PluginCli {
            run_and_refresh: run,
            refresh,
            runner: UpdateRunner { run_update: updates, refresh: batch_refresh },
            scan: Arc::new(move || Some(scan_snapshot.clone())),
        };
        (cli, calls)
    }

    /// Wait for the update a dispatched action emits.
    async fn next_update(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> SessionUpdate {
        tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("an update lands")
            .expect("the stream is open")
    }

    /// The home directory every user-scope call runs in.
    fn home() -> String {
        dirs::home_dir().expect("home").to_string_lossy().into_owned()
    }

    /// **The working directory comes from the declaration, never the
    /// launch directory** (hard rule 14): a project-scoped install runs in
    /// the declared project's own path, and a name `forge.toml` does not
    /// declare is refused rather than substituted.
    #[tokio::test]
    async fn a_project_install_runs_in_the_declared_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(
            forge.join("forge.toml"),
            "\
[[orgs]]
name = \"Busytools\"
accounts = [\"acct-a\"]

[[orgs.projects]]
name = \"proj\"
path = \"/tmp/plugin-cwd-probe\"
model = \"claude-sonnet-5\"

[[accounts]]
display_name = \"acct-a\"
token = \"t\"
models = [\"claude-sonnet-5\"]
provider = \"anthropic\"
",
        )
        .expect("write forge.toml");
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        let (cli, calls) =
            fake_cli(&PluginsInventorySnapshot::default(), &PluginsInventorySnapshot::default());
        ws.set_plugin_cli(cli);
        let declared = ws.list_projects()[0].path.to_string_lossy().into_owned();
        let mut rx = ws.subscribe();

        ws.dispatch(Command::PluginInstall {
            plugin_id: "demo@probe".to_owned(),
            scope: "project".to_owned(),
            project: Some("proj".to_owned()),
        })
        .expect("the install dispatches");
        let _ = next_update(&mut rx).await;

        let call = calls.run.lock().expect("lock")[0].clone();
        assert_eq!(call.cwd, declared, "the install runs in the declared project's path");
        assert_eq!(
            call.args,
            vec!["plugin", "install", "demo@probe", "--scope", "project"],
            "the CLI call carries the scope"
        );

        let refusal = ws
            .dispatch(Command::PluginInstall {
                plugin_id: "demo@probe".to_owned(),
                scope: "local".to_owned(),
                project: Some("nowhere".to_owned()),
            })
            .expect_err("an undeclared project is refused");
        assert!(
            matches!(&refusal, DispatchError::ProjectNotFound { name } if name == "nowhere"),
            "the refusal names the project, got: {refusal:?}"
        );
        assert_eq!(calls.run.lock().expect("lock").len(), 1, "and nothing ran for it");

        let refusal = ws
            .dispatch(Command::PluginInstall {
                plugin_id: "demo@probe".to_owned(),
                scope: "local".to_owned(),
                project: None,
            })
            .expect_err("a project-scoped install with no project is refused");
        assert!(
            matches!(refusal, DispatchError::PluginProjectRequired),
            "the refusal says the project is required, got: {refusal:?}"
        );
    }

    /// User scope runs in the home directory: the one fixed place a plugin
    /// call with no project context can run.
    #[tokio::test]
    async fn a_user_install_runs_in_the_home_directory() {
        let (ws, mut rx) = Workspace::testing_stub();
        let (cli, calls) =
            fake_cli(&PluginsInventorySnapshot::default(), &PluginsInventorySnapshot::default());
        ws.set_plugin_cli(cli);

        ws.dispatch(Command::PluginInstall {
            plugin_id: "demo@probe".to_owned(),
            scope: "user".to_owned(),
            project: None,
        })
        .expect("the install dispatches");
        let _ = next_update(&mut rx).await;

        assert_eq!(
            calls.run.lock().expect("lock")[0].cwd,
            home(),
            "user scope runs in the home directory, not the launch directory"
        );
    }

    /// Update-all queues **exactly the stale set**, one row per stale
    /// entry, each in the cwd its own scope runs in; the batch records
    /// what a rollback restores, and the finished frame carries the
    /// refreshed inventory.
    #[tokio::test]
    async fn update_all_queues_exactly_the_stale_set() {
        let (ws, mut rx) = Workspace::testing_stub();
        let scan = PluginsInventorySnapshot {
            installed: vec![
                installed("stale@probe", "1.0.0", "user", None),
                installed("current@probe", "2.0.0", "user", None),
                installed("local@probe", "1.0.0", "local", Some("/tmp/wc-issues")),
            ],
            marketplace: vec![
                marketplace_entry("stale@probe", "1.1.0"),
                marketplace_entry("current@probe", "2.0.0"),
                marketplace_entry("local@probe", "1.2.0"),
            ],
            ..PluginsInventorySnapshot::default()
        };
        let refreshed = PluginsInventorySnapshot {
            installed: vec![
                installed("stale@probe", "1.1.0", "user", None),
                installed("current@probe", "2.0.0", "user", None),
                installed("local@probe", "1.2.0", "local", Some("/tmp/wc-issues")),
            ],
            ..PluginsInventorySnapshot::default()
        };
        let (cli, calls) = fake_cli(&scan, &refreshed);
        ws.set_plugin_cli(cli);
        // The rollback records land in the durable store, so the run needs
        // one open to persist into.
        let dir = tempfile::tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        ws.dispatch(Command::PluginUpdateAll).expect("the run dispatches");

        let finished = loop {
            match next_update(&mut rx).await {
                SessionUpdate::PluginsUpdateRunFinished { run, .. } => break run,
                SessionUpdate::PluginsUpdateRunProgress { .. } => {}
                other => panic!("unexpected update: {other:?}"),
            }
        };
        assert!(finished.finished, "the run is over");
        let ids: Vec<&str> = finished.rows.iter().map(|row| row.plugin_id.as_str()).collect();
        assert_eq!(ids, vec!["stale@probe", "local@probe"], "only the stale entries are queued");
        assert_eq!(
            finished.rows[0].cwd_raw,
            home(),
            "a user-scoped row runs in the home directory"
        );
        assert_eq!(
            finished.rows[1].cwd_raw, "/tmp/wc-issues",
            "a local-scoped row runs in its own project"
        );
        assert_eq!(
            finished.rows[0].status,
            PluginRunRowStatus::Updated,
            "a version that moved reads updated"
        );

        let updated: Vec<Vec<String>> =
            calls.updates.lock().expect("lock").iter().map(|call| call.args.clone()).collect();
        assert_eq!(
            updated,
            vec![
                vec!["plugin", "update", "stale@probe", "--scope", "user"],
                vec!["plugin", "update", "local@probe", "--scope", "local"],
            ],
            "one update call per stale row, in plan order"
        );
        let records = ws.plugin_update_records();
        assert_eq!(records.len(), 2, "each applied update recorded what a rollback restores");
        let stale = records
            .iter()
            .find(|record| record.plugin_id == "stale@probe")
            .expect("the applied update recorded its own row");
        assert_eq!(stale.from_version.as_deref(), Some("1.0.0"));
        assert_eq!(stale.to_version.as_deref(), Some("1.1.0"));
    }

    /// Nothing stale is not a refusal: the run lands finished with no rows
    /// (the check's own shape when it finds nothing), no CLI call goes out,
    /// and the gate opens again.
    #[tokio::test]
    async fn nothing_stale_lands_as_a_finished_empty_run() {
        let (ws, mut rx) = Workspace::testing_stub();
        let scan = PluginsInventorySnapshot {
            installed: vec![installed("current@probe", "2.0.0", "user", None)],
            marketplace: vec![marketplace_entry("current@probe", "2.0.0")],
            ..PluginsInventorySnapshot::default()
        };
        let (cli, calls) = fake_cli(&scan, &PluginsInventorySnapshot::default());
        ws.set_plugin_cli(cli);

        ws.dispatch(Command::PluginUpdateAll).expect("the run dispatches");

        let SessionUpdate::PluginsUpdateRunFinished { run, .. } = next_update(&mut rx).await else {
            panic!("the run reports its finish");
        };
        assert!(run.finished, "an empty run is a finished one");
        assert!(run.rows.is_empty(), "nothing stale is nothing to do");
        assert!(calls.updates.lock().expect("lock").is_empty(), "and no update call went out");
        assert!(
            !ws.plugin_action_running.load(Ordering::Acquire),
            "the gate opened when the run ended"
        );
    }

    /// A scan whose thread cannot run reports the join by name, distinct
    /// from a missing root: the two failure modes collapse silently under a
    /// `unwrap_or_default()` or a dropped `?`, and the page would then say
    /// "no plugins root resolves" about a machine that has one.
    #[tokio::test]
    async fn a_scan_that_cannot_run_reports_the_join() {
        let (ws, mut rx) = Workspace::testing_stub();
        let (cli, _calls) =
            fake_cli(&PluginsInventorySnapshot::default(), &PluginsInventorySnapshot::default());
        let panicking = PluginCli { scan: Arc::new(|| panic!("the scan thread died")), ..cli };
        ws.set_plugin_cli(panicking);

        ws.dispatch(Command::PluginUpdateAll).expect("the dispatch is accepted");

        let SessionUpdate::PluginsCliActionFailed { message, .. } = next_update(&mut rx).await
        else {
            panic!("the failure reports as a frame");
        };
        assert!(message.contains("failed to join"), "the join is named, got: {message}");
        assert!(
            !message.contains("no plugins root resolves"),
            "and not as a missing root, got: {message}"
        );
    }

    /// A scan with no root to walk reports its own words rather than reading
    /// as an empty machine, and it does so as a frame: the dispatch is
    /// fire-and-forget, so a refusal found inside the task lands the same
    /// way a failed action does.
    #[tokio::test]
    async fn a_missing_plugins_root_reports_as_a_failed_action() {
        let (ws, mut rx) = Workspace::testing_stub();
        let (cli, _calls) =
            fake_cli(&PluginsInventorySnapshot::default(), &PluginsInventorySnapshot::default());
        let no_root = PluginCli { scan: Arc::new(|| None), ..cli };
        ws.set_plugin_cli(no_root);

        ws.dispatch(Command::PluginUpdateAll).expect("the dispatch is accepted");

        let SessionUpdate::PluginsCliActionFailed { message, .. } = next_update(&mut rx).await
        else {
            panic!("the failure reports as a frame");
        };
        assert!(
            message.contains("no plugins root resolves"),
            "the message names the missing root, got: {message}"
        );
        assert!(
            !ws.plugin_action_running.load(Ordering::Acquire),
            "the gate opened when the task ended"
        );
    }

    /// A second action waits: one plugin CLI call at a time, refused by
    /// name while the first runs.
    #[test]
    fn a_second_action_is_refused_while_one_runs() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.plugin_action_running.store(true, Ordering::Release);
        let refusal = ws
            .dispatch(Command::PluginCheckUpdates)
            .expect_err("a running action refuses a second");
        assert!(
            matches!(refusal, DispatchError::PluginActionRunning),
            "the refusal names what is running, got: {refusal:?}"
        );
    }

    /// An update names a plugin the registry does not hold in that scope:
    /// the view's snapshot is stale, and the failure names it rather than
    /// running a call that would fail at the CLI.
    #[tokio::test]
    async fn an_update_of_an_uninstalled_plugin_fails_naming_it() {
        let (ws, mut rx) = Workspace::testing_stub();
        let (cli, calls) =
            fake_cli(&PluginsInventorySnapshot::default(), &PluginsInventorySnapshot::default());
        ws.set_plugin_cli(cli);

        ws.dispatch(Command::PluginUpdate {
            plugin_id: "ghost@probe".to_owned(),
            scope: "user".to_owned(),
        })
        .expect("the dispatch is accepted");

        let SessionUpdate::PluginsCliActionFailed { message, .. } = next_update(&mut rx).await
        else {
            panic!("the failure reports as a frame");
        };
        assert!(
            message.contains("ghost@probe") && message.contains("user"),
            "the failure names the plugin and the scope, got: {message}"
        );
        assert!(calls.run.lock().expect("lock").is_empty(), "and no CLI call went out");
    }

    /// The outcome of a landed action is the refreshed inventory plus the
    /// message the view draws - both on the frame the page listens for -
    /// and the gate opens again for the next press.
    #[tokio::test]
    async fn a_landed_install_reports_the_refreshed_inventory() {
        let (ws, mut rx) = Workspace::testing_stub();
        let refreshed = PluginsInventorySnapshot {
            installed: vec![installed("demo@probe", "1.0.0", "user", None)],
            ..PluginsInventorySnapshot::default()
        };
        let (cli, _calls) = fake_cli(&PluginsInventorySnapshot::default(), &refreshed);
        ws.set_plugin_cli(cli);

        ws.dispatch(Command::PluginInstall {
            plugin_id: "demo@probe".to_owned(),
            scope: "user".to_owned(),
            project: None,
        })
        .expect("the install dispatches");

        let SessionUpdate::PluginsCliActionSucceeded { result, .. } = next_update(&mut rx).await
        else {
            panic!("the action reports its outcome");
        };
        assert_eq!(result.message, "Installed demo@probe for user scope");
        assert_eq!(result.snapshot.installed.len(), 1, "the frame carries the refreshed inventory");
        assert!(
            !ws.plugin_action_running.load(Ordering::Acquire),
            "the gate opened when the action ended"
        );
    }

    /// A repair is the CLI's remove-and-re-add pair, and the re-add source
    /// is the repo for a git marketplace and the clone location for a
    /// directory one.
    #[tokio::test]
    async fn a_repair_is_the_remove_then_readd_pair() {
        let (ws, mut rx) = Workspace::testing_stub();
        let scan = PluginsInventorySnapshot {
            marketplaces: vec![
                marketplace("git-mp", "github", Some("owner/repo"), Some("/tmp/clone")),
                marketplace("dir-mp", "directory", None, Some("/tmp/dir-clone")),
            ],
            ..PluginsInventorySnapshot::default()
        };
        let (cli, calls) = fake_cli(&scan, &PluginsInventorySnapshot::default());
        ws.set_plugin_cli(cli);

        ws.dispatch(Command::MarketplaceRepair { name: "git-mp".to_owned() })
            .expect("the repair dispatches");
        let _ = next_update(&mut rx).await;
        let pair: Vec<Vec<String>> =
            calls.run.lock().expect("lock").iter().map(|call| call.args.clone()).collect();
        assert_eq!(
            pair,
            vec![
                vec!["plugin", "marketplace", "remove", "git-mp"],
                vec!["plugin", "marketplace", "add", "owner/repo", "--scope", "user"],
            ],
            "a git marketplace re-adds from its repo"
        );
        calls.run.lock().expect("lock").clear();

        ws.dispatch(Command::MarketplaceRepair { name: "dir-mp".to_owned() })
            .expect("the repair dispatches");
        let _ = next_update(&mut rx).await;
        let pair: Vec<Vec<String>> =
            calls.run.lock().expect("lock").iter().map(|call| call.args.clone()).collect();
        assert_eq!(
            pair[1],
            vec!["plugin", "marketplace", "add", "/tmp/dir-clone", "--scope", "user"],
            "a directory marketplace re-adds from its clone location"
        );

        ws.dispatch(Command::MarketplaceRepair { name: "absent".to_owned() })
            .expect("the dispatch is accepted");
        let SessionUpdate::PluginsCliActionFailed { message, .. } = next_update(&mut rx).await
        else {
            panic!("the unknown marketplace reports as a failure frame");
        };
        assert!(message.contains("absent"), "the failure names it, got: {message}");
    }

    /// The check applies nothing: no update call goes out, and the run it
    /// reports is the stale set as check-only rows.
    #[tokio::test]
    async fn the_check_applies_nothing() {
        let (ws, mut rx) = Workspace::testing_stub();
        let refreshed = PluginsInventorySnapshot {
            installed: vec![installed("stale@probe", "1.0.0", "user", None)],
            marketplace: vec![marketplace_entry("stale@probe", "1.1.0")],
            ..PluginsInventorySnapshot::default()
        };
        let (cli, calls) = fake_cli(&PluginsInventorySnapshot::default(), &refreshed);
        ws.set_plugin_cli(cli);

        ws.dispatch(Command::PluginCheckUpdates).expect("the check dispatches");

        let SessionUpdate::PluginsUpdateRunFinished { run, snapshot, .. } =
            next_update(&mut rx).await
        else {
            panic!("the check reports a finished run");
        };
        assert!(run.finished);
        assert_eq!(run.rows.len(), 1, "one check-only row for the stale entry");
        assert_eq!(
            run.rows[0].status,
            PluginRunRowStatus::UpdateAvailable,
            "a check reports availability and applies nothing"
        );
        assert!(snapshot.is_some(), "the check's refreshed inventory rides the frame");
        assert!(calls.updates.lock().expect("lock").is_empty(), "no update call went out");
        assert_eq!(
            calls.refresh.lock().expect("lock").len(),
            1,
            "the refresh is what the check runs"
        );
    }
}
