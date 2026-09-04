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

/// One installed entry queued for a section-level update run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdateTarget {
    pub plugin_id: String,
    pub scope: String,
    /// Working directory the `claude plugin update` call runs in;
    /// project/local scope entries update from their own project.
    pub cwd_raw: String,
    pub version_before: Option<String>,
}

/// What a single plugin update came to. Version compare, not CLI
/// wording, decides `Updated` vs `AlreadyCurrent`: the CLI exits 0 on
/// some failures, so its output cannot be trusted for classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginUpdateOutcomeStatus {
    Updated,
    AlreadyCurrent,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpdateOutcome {
    pub plugin_id: String,
    pub scope: String,
    pub marketplace: String,
    pub status: PluginUpdateOutcomeStatus,
    pub version_before: Option<String>,
    pub version_after: Option<String>,
    pub detail: Option<String>,
}

impl PluginUpdateOutcome {
    /// Failure wins, then version change; an unobservable change reads
    /// as already current.
    pub fn classify(
        plugin_id: String,
        scope: String,
        version_before: Option<String>,
        version_after: Option<String>,
        failure: Option<String>,
    ) -> Self {
        let status = match failure {
            Some(_) => PluginUpdateOutcomeStatus::Failed,
            None if version_before != version_after => PluginUpdateOutcomeStatus::Updated,
            None => PluginUpdateOutcomeStatus::AlreadyCurrent,
        };
        Self {
            marketplace: plugin_marketplace(&plugin_id).to_owned(),
            plugin_id,
            scope,
            status,
            version_before,
            version_after,
            detail: failure,
        }
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
                .and_then(|candidate| candidate.version.clone());
            let Some(available) = available else {
                return None;
            };
            if Some(available.clone()) == entry.version {
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
    fn update_outcome_classifies_from_version_change() {
        let updated = PluginUpdateOutcome::classify(
            "hello@probe-market".to_owned(),
            "user".to_owned(),
            Some("0.2.0".to_owned()),
            Some("0.3.0".to_owned()),
            None,
        );
        assert_eq!(updated.status, PluginUpdateOutcomeStatus::Updated);
        assert_eq!(updated.marketplace, "probe-market");

        let current = PluginUpdateOutcome::classify(
            "hello@probe-market".to_owned(),
            "user".to_owned(),
            Some("0.2.0".to_owned()),
            Some("0.2.0".to_owned()),
            None,
        );
        assert_eq!(current.status, PluginUpdateOutcomeStatus::AlreadyCurrent);

        let failed = PluginUpdateOutcome::classify(
            "hello@probe-market".to_owned(),
            "user".to_owned(),
            Some("0.2.0".to_owned()),
            Some("0.2.0".to_owned()),
            Some("claude exited 1".to_owned()),
        );
        assert_eq!(failed.status, PluginUpdateOutcomeStatus::Failed);
        assert_eq!(failed.detail.as_deref(), Some("claude exited 1"));
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
