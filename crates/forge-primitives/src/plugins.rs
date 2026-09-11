//! Plugin inventory + CLI-action wire shapes. `SessionUpdate`
//! variants carry these directly; loaders (`refresh_inventory`,
//! the `claude plugin` shell-out, etc.) live in
//! `forge_agent::userdata::plugins`.

use std::path::PathBuf;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginCapability {
    Skill,
    Mcp,
}

impl PluginCapability {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Skill => "SKILL",
            Self::Mcp => "MCP",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPluginEntry {
    pub id: String,
    pub version: Option<String>,
    pub scope: String,
    pub enabled: bool,
    pub installed_at: Option<String>,
    pub last_updated: Option<String>,
    pub project_path: Option<String>,
    pub capability: PluginCapability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceEntry {
    pub plugin_id: String,
    pub name: String,
    pub description: Option<String>,
    pub marketplace_name: Option<String>,
    pub version: Option<String>,
    pub install_count: Option<u64>,
    pub source: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceSourceEntry {
    pub name: String,
    pub source: Option<String>,
    pub repo: Option<String>,
    /// On-disk clone or directory the CLI maintains for this
    /// marketplace; a git-backed one here is what makes a rollback
    /// possible.
    pub install_location: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginsInventorySnapshot {
    pub installed: Vec<InstalledPluginEntry>,
    pub marketplace: Vec<MarketplaceEntry>,
    pub marketplaces: Vec<MarketplaceSourceEntry>,
    /// Per-plugin component inventory read off disk by the catalog
    /// scan (`forge_agent::userdata::plugins::components`); empty when
    /// the producer does not scan.
    pub components: Vec<PluginComponents>,
    /// Marketplace load health for the Extensions page; empty when the
    /// producer does not scan.
    pub marketplace_health: Vec<MarketplaceHealth>,
}

/// Per-plugin component inventory read straight off disk (the plugin
/// cache plus marketplace manifests). Produced by the scan in
/// `forge_agent::userdata::plugins::components`; one entry per plugin
/// the cache or a manifest knows about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginComponents {
    /// Full installed id (`name@marketplace`).
    pub plugin: String,
    pub marketplace: String,
    /// Version of the installed copy, from the plugin registry.
    pub version: Option<String>,
    pub installed: bool,
    /// Every registry entry for the plugin is enabled. False for an
    /// uninstalled plugin.
    pub enabled: bool,
    /// The registry marks the install as an auto-installed dependency.
    pub auto: bool,
    /// Latest version the marketplace manifest declares.
    pub available_version: Option<String>,
    pub skills: Vec<String>,
    pub agents: Vec<String>,
    pub commands: Vec<String>,
    /// Hook trigger events (`SessionStart`, ...), from hooks.json.
    pub hooks: Vec<String>,
    /// The plugin ships `.mcp.json`.
    pub mcp: bool,
    /// LSP server names the marketplace manifest declares.
    pub lsp_servers: Vec<String>,
}

/// One marketplace's on-disk health for the Extensions page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarketplaceHealth {
    pub name: String,
    pub source: String,
    /// Plugin count in the marketplace's manifest; 0 when it cannot
    /// load.
    pub available: usize,
    /// Why the manifest could not be read (cache-miss, parse failure).
    pub load_error: Option<String>,
    pub install_location: PathBuf,
    /// The registry's installLocation sits outside the config dir.
    pub drifted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginsCliActionSuccess {
    pub snapshot: PluginsInventorySnapshot,
    pub message: String,
    pub claude_path: PathBuf,
}

/// Lifecycle of one row in a plugin update run or update check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRunRowStatus {
    /// Queued for the run, not started yet.
    Queued,
    /// The `claude plugin update` call is in flight.
    Updating,
    /// Installed version moved.
    Updated,
    /// No change to install.
    AlreadyCurrent,
    /// The update call failed.
    Failed,
    /// Auto-update did not touch this entry.
    Skipped,
    /// Check-only: the marketplace reports a different version.
    UpdateAvailable,
}

/// One row of a section-level update run or check report, carried to
/// the pane through `SessionUpdate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdateRunRow {
    pub plugin_id: String,
    pub scope: String,
    /// Working directory this row's `claude plugin update` runs in;
    /// project/local scope entries update from their own project.
    pub cwd_raw: String,
    pub marketplace: String,
    pub status: PluginRunRowStatus,
    pub installed_version: Option<String>,
    pub available_version: Option<String>,
    /// The failure text for `Failed`, or the reason for `Skipped`.
    pub detail: Option<String>,
}

impl PluginUpdateRunRow {
    /// A row queued from an installed entry.
    pub fn queued(
        plugin_id: String,
        scope: String,
        cwd_raw: String,
        installed_version: Option<String>,
    ) -> Self {
        Self {
            marketplace: plugin_marketplace(&plugin_id).to_owned(),
            plugin_id,
            scope,
            cwd_raw,
            status: PluginRunRowStatus::Queued,
            installed_version,
            available_version: None,
            detail: None,
        }
    }
}

/// A section-level update run or check report: the rows and whether it
/// has finished. Progress updates replace the whole run each time so
/// the pane never reconciles partial state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdateRun {
    pub trigger: PluginUpdateTrigger,
    pub finished: bool,
    pub rows: Vec<PluginUpdateRunRow>,
}

impl PluginUpdateRun {
    pub fn summary(&self) -> String {
        let mut updated = 0;
        let mut failed = 0;
        let mut current = 0;
        let mut available = 0;
        for row in &self.rows {
            match row.status {
                PluginRunRowStatus::Updated => updated += 1,
                PluginRunRowStatus::Failed => failed += 1,
                PluginRunRowStatus::AlreadyCurrent => current += 1,
                PluginRunRowStatus::UpdateAvailable => available += 1,
                _ => {}
            }
        }
        if updated > 0 || failed > 0 {
            format!("{updated} updated, {failed} failed, {current} current")
        } else if available > 0 {
            format!("{available} update(s) available")
        } else {
            "all current".to_owned()
        }
    }
}

/// The CLI marker printed when an update finds nothing to do.
const ALREADY_CURRENT_MARKER: &str = "is already at the latest version";

/// Classify one update call from the CLI's output and the observed
/// version change. The CLI exits 0 on some failures, so neither
/// signal alone decides: the marker means current, a version change
/// means updated, and anything else - including a silent exit 0 - is
/// a failure whose detail is the output tail. Failures that exit
/// non-zero never reach here: the runner reports them as errors.
pub fn classify_update_row(
    plugin_id: &str,
    scope: &str,
    version_before: Option<&str>,
    version_after: Option<&str>,
    output: &str,
) -> PluginUpdateRunRow {
    let mut row = PluginUpdateRunRow::queued(
        plugin_id.to_owned(),
        scope.to_owned(),
        String::new(),
        version_before.map(str::to_owned),
    );
    row.installed_version = version_after.map(str::to_owned);
    row.status = if output.contains(ALREADY_CURRENT_MARKER) {
        PluginRunRowStatus::AlreadyCurrent
    } else if version_before != version_after {
        PluginRunRowStatus::Updated
    } else {
        row.detail = Some(output_tail(output));
        PluginRunRowStatus::Failed
    };
    row
}

/// The last line of CLI output worth showing, trimmed. Long failure
/// prose clips so a report row stays one line.
fn output_tail(output: &str) -> String {
    let line = output.lines().rev().find(|line| !line.trim().is_empty());
    let line = line.unwrap_or_default().trim();
    if line.chars().count() > 120 {
        let cut: String = line.chars().take(117).collect();
        format!("{cut}...")
    } else {
        line.to_owned()
    }
}

/// The marketplace part of an installed id (`name@marketplace`), empty
/// when the id carries none.
pub fn plugin_marketplace(id: &str) -> &str {
    id.split_once('@').map_or("", |(_, marketplace)| marketplace)
}

/// An installed entry whose marketplace copy reports a different
/// version - the "check for updates" result, reported without
/// applying anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdateAvailability {
    pub plugin_id: String,
    pub scope: String,
    pub marketplace: String,
    pub installed_version: Option<String>,
    pub available_version: Option<String>,
}

/// Diff the installed list against the marketplace catalog. One row
/// per installed entry; an entry with no marketplace copy of a
/// different version is absent.
pub fn update_availability(
    installed: &[InstalledPluginEntry],
    marketplace: &[MarketplaceEntry],
) -> Vec<PluginUpdateAvailability> {
    installed
        .iter()
        .filter_map(|entry| {
            let available = marketplace
                .iter()
                .find(|candidate| candidate.plugin_id == entry.id)
                .and_then(|candidate| candidate.version.clone())?;
            if Some(&available) == entry.version.as_ref() {
                return None;
            }
            Some(PluginUpdateAvailability {
                marketplace: plugin_marketplace(&entry.id).to_owned(),
                plugin_id: entry.id.clone(),
                scope: entry.scope.clone(),
                installed_version: entry.version.clone(),
                available_version: Some(available),
            })
        })
        .collect()
}

/// What forge remembers after a plugin moved. `marketplace_ref_before`
/// is the marketplace clone's HEAD before the update - the ref a
/// rollback checks out. `cwd_raw` is where the update ran, so a
/// rollback of a project/local entry works on the same install.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginUpdateRecord {
    pub plugin_id: String,
    pub marketplace: String,
    pub scope: String,
    #[serde(default)]
    pub cwd_raw: String,
    pub from_version: Option<String>,
    pub to_version: Option<String>,
    pub marketplace_ref_before: Option<String>,
    pub updated_at: String,
    pub trigger: PluginUpdateTrigger,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUpdateTrigger {
    Manual,
    Auto,
}

/// One row of the Extensions page's shared grammar: a plugin, one of
/// its components, or an MCP server. Flattened from a component scan
/// by [`extension_rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionRow {
    /// Stable row id: the plugin id for plugin rows,
    /// `<kind>:<plugin-name>:<component>` for components.
    pub id: String,
    pub kind: ExtensionKind,
    pub name: String,
    /// The owning plugin for components, the marketplace for plugins.
    pub source: String,
    pub version: Option<String>,
    pub available_version: Option<String>,
    pub state: RowState,
    /// Extra row context: hook trigger events, failure reasons.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionKind {
    Plugin,
    Skill,
    Agent,
    Command,
    Hook,
    Lsp,
    Mcp,
}

/// The row's health, as the Extensions page renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowState {
    Current,
    UpdateAvailable,
    AvailableNotInstalled,
    Disabled,
    /// A health failure and its reason.
    LoadFailed(String),
    /// Installed as an auto-dependency; the payload is the reason
    /// shown on the row (the CLI exposes no consumer name).
    AutoDependency(String),
    /// Applied but not live until a restart consumes it.
    RestartRequired,
}

/// Flatten component scans into Extension page rows: one row per
/// plugin followed by its component rows, in scan order.
pub fn extension_rows(components: &[PluginComponents]) -> Vec<ExtensionRow> {
    let mut rows = Vec::new();
    for components in components {
        let plugin_name =
            components.plugin.split_once('@').map_or(components.plugin.as_str(), |(name, _)| name);
        let state = plugin_row_state(components);
        rows.push(ExtensionRow {
            id: components.plugin.clone(),
            kind: ExtensionKind::Plugin,
            name: plugin_name.to_owned(),
            source: components.marketplace.clone(),
            version: components.version.clone(),
            available_version: components.available_version.clone(),
            state: state.clone(),
            detail: match &state {
                RowState::AutoDependency(reason) => Some(reason.clone()),
                _ => None,
            },
        });
        let inherited = match state {
            // Auto-dependency provenance describes the plugin's
            // install, not its components.
            RowState::AutoDependency(_) => RowState::Current,
            other => other,
        };
        for skills in &components.skills {
            rows.push(component_row(
                ExtensionKind::Skill,
                &components.plugin,
                plugin_name,
                components,
                skills,
                inherited.clone(),
            ));
        }
        for agent in &components.agents {
            rows.push(component_row(
                ExtensionKind::Agent,
                &components.plugin,
                plugin_name,
                components,
                agent,
                inherited.clone(),
            ));
        }
        for command in &components.commands {
            rows.push(component_row(
                ExtensionKind::Command,
                &components.plugin,
                plugin_name,
                components,
                command,
                inherited.clone(),
            ));
        }
        if !components.hooks.is_empty() {
            rows.push(ExtensionRow {
                id: format!("hooks:{plugin_name}"),
                kind: ExtensionKind::Hook,
                name: plugin_name.to_owned(),
                source: components.plugin.clone(),
                version: components.version.clone(),
                available_version: components.available_version.clone(),
                state: inherited.clone(),
                detail: Some(components.hooks.join(", ")),
            });
        }
        for server in &components.lsp_servers {
            rows.push(component_row(
                ExtensionKind::Lsp,
                &components.plugin,
                plugin_name,
                components,
                server,
                inherited.clone(),
            ));
        }
    }
    rows
}

fn component_row(
    kind: ExtensionKind,
    plugin_id: &str,
    plugin_name: &str,
    components: &PluginComponents,
    name: &str,
    state: RowState,
) -> ExtensionRow {
    let kind_label = match kind {
        ExtensionKind::Skill => "skill",
        ExtensionKind::Agent => "agent",
        ExtensionKind::Command => "command",
        ExtensionKind::Lsp => "lsp",
        _ => "component",
    };
    ExtensionRow {
        id: format!("{kind_label}:{plugin_name}:{name}"),
        kind,
        name: name.to_owned(),
        source: plugin_id.to_owned(),
        version: components.version.clone(),
        available_version: components.available_version.clone(),
        state,
        detail: None,
    }
}

/// The plugin row's state from a scan result. Priority: not installed,
/// then disabled, then an update, then the auto-dependency marker.
fn plugin_row_state(components: &PluginComponents) -> RowState {
    if !components.installed {
        return RowState::AvailableNotInstalled;
    }
    if !components.enabled {
        return RowState::Disabled;
    }
    let update_available = components
        .available_version
        .as_deref()
        .is_some_and(|available| Some(available) != components.version.as_deref());
    if update_available {
        return RowState::UpdateAvailable;
    }
    if components.auto {
        return RowState::AutoDependency("auto-installed".to_owned());
    }
    RowState::Current
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed_entry(id: &str, version: Option<&str>) -> InstalledPluginEntry {
        InstalledPluginEntry {
            id: id.to_owned(),
            version: version.map(str::to_owned),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Skill,
        }
    }

    fn marketplace_entry(plugin_id: &str, version: Option<&str>) -> MarketplaceEntry {
        MarketplaceEntry {
            plugin_id: plugin_id.to_owned(),
            name: "hello".to_owned(),
            description: None,
            marketplace_name: None,
            version: version.map(str::to_owned),
            install_count: None,
            source: None,
        }
    }

    #[test]
    fn marketplace_parses_from_installed_id() {
        assert_eq!(plugin_marketplace("pensive@claude-night-market"), "claude-night-market");
        assert_eq!(plugin_marketplace("skills-dir-plugin"), "");
    }

    #[test]
    fn update_outcome_classifies_from_output_and_versions() {
        let updated = classify_update_row(
            "hello@probe-market",
            "user",
            Some("0.2.0"),
            Some("0.3.0"),
            "Plugin \"hello\" updated from 0.2.0 to 0.3.0 for scope user.",
        );
        assert_eq!(updated.status, PluginRunRowStatus::Updated);
        assert_eq!(updated.marketplace, "probe-market");
        assert_eq!(updated.installed_version.as_deref(), Some("0.3.0"));

        let current = classify_update_row(
            "hello@probe-market",
            "user",
            Some("0.2.0"),
            Some("0.2.0"),
            "hello is already at the latest version (0.2.0).",
        );
        assert_eq!(current.status, PluginRunRowStatus::AlreadyCurrent);

        // Probed CLI behaviour: a missing plugin exits 0 with failure
        // prose and no version change. That reads as failed, never as
        // current.
        let exit_zero_failure = classify_update_row(
            "hello@probe-market",
            "user",
            Some("0.2.0"),
            Some("0.2.0"),
            "✘ Failed to update plugin \"hello\": Plugin \"hello\" not found",
        );
        assert_eq!(exit_zero_failure.status, PluginRunRowStatus::Failed);
        assert!(
            exit_zero_failure.detail.as_deref().is_some_and(|detail| detail.contains("not found")),
            "the failure prose is the row detail"
        );

        // An empty success output with an unchanged version is
        // undecidable and reads as failed; the runner no longer
        // produces it (it always hands over the CLI's output).
        let silent_failure =
            classify_update_row("hello@probe-market", "user", Some("0.2.0"), Some("0.2.0"), "");
        assert_eq!(silent_failure.status, PluginRunRowStatus::Failed);
    }

    #[test]
    fn run_summary_counts_rows_by_status() {
        let run = PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![
                queued_row_with_status("a@probe-market", PluginRunRowStatus::Updated),
                queued_row_with_status("b@probe-market", PluginRunRowStatus::Failed),
                queued_row_with_status("c@probe-market", PluginRunRowStatus::AlreadyCurrent),
            ],
        };
        assert_eq!(run.summary(), "1 updated, 1 failed, 1 current");
    }

    fn queued_row_with_status(id: &str, status: PluginRunRowStatus) -> PluginUpdateRunRow {
        let mut row = PluginUpdateRunRow::queued(
            id.to_owned(),
            "user".to_owned(),
            String::new(),
            Some("1.0.0".to_owned()),
        );
        row.status = status;
        row
    }

    #[test]
    fn update_availability_reports_version_divergence_only() {
        let installed = vec![
            installed_entry("hello@probe-market", Some("0.2.0")),
            installed_entry("stale@probe-market", Some("1.0.0")),
            installed_entry("absent@probe-market", Some("1.0.0")),
        ];
        let marketplace = vec![
            marketplace_entry("hello@probe-market", Some("0.2.0")),
            marketplace_entry("stale@probe-market", Some("1.1.0")),
        ];

        let rows = update_availability(&installed, &marketplace);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].plugin_id, "stale@probe-market");
        assert_eq!(rows[0].installed_version.as_deref(), Some("1.0.0"));
        assert_eq!(rows[0].available_version.as_deref(), Some("1.1.0"));
        assert_eq!(rows[0].marketplace, "probe-market");
    }

    fn scan_plugin(id: &str) -> PluginComponents {
        PluginComponents {
            plugin: id.to_owned(),
            marketplace: "probe-market".to_owned(),
            installed: true,
            enabled: true,
            ..PluginComponents::default()
        }
    }

    #[test]
    fn a_plugin_flattens_into_component_rows_with_exact_ids() {
        let mut superpowers = scan_plugin("superpowers@probe-market");
        superpowers.version = Some("6.3.0".to_owned());
        superpowers.skills = vec!["brainstorming".to_owned(), "writing-plans".to_owned()];
        superpowers.agents = vec!["code-reviewer".to_owned()];
        superpowers.commands = vec!["review".to_owned()];
        superpowers.hooks = vec!["SessionStart".to_owned()];
        superpowers.lsp_servers = vec!["rust-analyzer".to_owned()];

        let rows = extension_rows(&[superpowers]);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "superpowers@probe-market",
                "skill:superpowers:brainstorming",
                "skill:superpowers:writing-plans",
                "agent:superpowers:code-reviewer",
                "command:superpowers:review",
                "hooks:superpowers",
                "lsp:superpowers:rust-analyzer",
            ],
            "plugin row first, then its components; got {ids:?}"
        );
        let hook = &rows[5];
        assert_eq!(hook.kind, ExtensionKind::Hook);
        assert_eq!(hook.detail.as_deref(), Some("SessionStart"));
        assert_eq!(hook.source, "superpowers@probe-market");
    }

    #[test]
    fn an_uninstalled_plugins_skills_render_available_not_installed() {
        let mut gone = scan_plugin("gone@probe-market");
        gone.installed = false;
        gone.enabled = false;
        gone.version = None;
        gone.available_version = Some("6.4.0".to_owned());
        gone.skills = vec!["writing-skills".to_owned()];

        let rows = extension_rows(&[gone]);
        assert_eq!(rows[0].state, RowState::AvailableNotInstalled);
        assert_eq!(rows[1].id, "skill:gone:writing-skills");
        assert_eq!(rows[1].state, RowState::AvailableNotInstalled);
        assert_eq!(rows[1].available_version.as_deref(), Some("6.4.0"));
    }

    #[test]
    fn an_auto_installed_plugin_wears_the_marker_on_its_plugin_row_only() {
        let mut leyline = scan_plugin("leyline@probe-market");
        leyline.auto = true;
        leyline.skills = vec!["utility".to_owned()];

        let rows = extension_rows(&[leyline]);
        assert_eq!(
            rows[0].state,
            RowState::AutoDependency("auto-installed".to_owned()),
            "the plugin row carries the marker"
        );
        assert_eq!(rows[1].state, RowState::Current, "components do not inherit it");
    }

    #[test]
    fn a_disabled_plugin_marks_every_row_disabled_and_an_update_outranks_current() {
        let mut off = scan_plugin("off@probe-market");
        off.enabled = false;
        off.version = Some("1.0.0".to_owned());
        off.skills = vec!["one".to_owned()];
        let mut stale = scan_plugin("stale@probe-market");
        stale.version = Some("1.0.0".to_owned());
        stale.available_version = Some("2.0.0".to_owned());
        stale.commands = vec!["do".to_owned()];

        let rows = extension_rows(&[off, stale]);
        assert_eq!(rows[0].state, RowState::Disabled);
        assert_eq!(rows[1].state, RowState::Disabled);
        assert_eq!(rows[2].state, RowState::UpdateAvailable);
        assert_eq!(rows[3].state, RowState::UpdateAvailable);
    }
}
