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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginsInventorySnapshot {
    pub installed: Vec<InstalledPluginEntry>,
    pub marketplace: Vec<MarketplaceEntry>,
    pub marketplaces: Vec<MarketplaceSourceEntry>,
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
        if failed > 0 {
            format!("{updated} updated, {failed} failed, {current} current")
        } else if available > 0 {
            format!("{available} update(s) available")
        } else {
            format!("{updated} updated, {current} current")
        }
    }
}

/// The CLI marker printed when an update finds nothing to do.
const ALREADY_CURRENT_MARKER: &str = "is already at the latest version";

/// Classify one update call from its output and the observed version
/// change. The CLI exits 0 on some failures, so neither signal alone
/// decides: a non-zero exit is a failure, the marker means current, a
/// version change means updated, and anything else is a failure whose
/// detail is the output tail.
pub fn classify_update_row(
    plugin_id: &str,
    scope: &str,
    version_before: Option<&str>,
    version_after: Option<&str>,
    exit_ok: bool,
    output: &str,
) -> PluginUpdateRunRow {
    let mut row = PluginUpdateRunRow::queued(
        plugin_id.to_owned(),
        scope.to_owned(),
        String::new(),
        version_before.map(str::to_owned),
    );
    row.installed_version = version_after.map(str::to_owned);
    row.status = if !exit_ok {
        row.detail = Some(output_tail(output));
        PluginRunRowStatus::Failed
    } else if output.contains(ALREADY_CURRENT_MARKER) {
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
/// rollback checks out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginUpdateRecord {
    pub plugin_id: String,
    pub marketplace: String,
    pub scope: String,
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
            true,
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
            true,
            "hello is already at the latest version (0.2.0).",
        );
        assert_eq!(current.status, PluginRunRowStatus::AlreadyCurrent);

        // The CLI exits 0 with no marker when the update silently
        // fails: unchanged version reads as failure, not as current.
        let silent_failure = classify_update_row(
            "hello@probe-market",
            "user",
            Some("0.2.0"),
            Some("0.2.0"),
            true,
            "Something went wrong",
        );
        assert_eq!(silent_failure.status, PluginRunRowStatus::Failed);
        assert_eq!(silent_failure.detail.as_deref(), Some("Something went wrong"));

        let exit_failure = classify_update_row(
            "hello@probe-market",
            "user",
            Some("0.2.0"),
            None,
            false,
            "network unreachable",
        );
        assert_eq!(exit_failure.status, PluginRunRowStatus::Failed);
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
}
