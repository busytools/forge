use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PluginsViewTab {
    #[default]
    Installed,
    Plugins,
    Marketplace,
}

impl PluginsViewTab {
    pub const ALL: [Self; 3] = [Self::Installed, Self::Plugins, Self::Marketplace];

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Installed => "Installed",
            Self::Plugins => "Plugins",
            Self::Marketplace => "Marketplace",
        }
    }

    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Installed => Self::Plugins,
            Self::Plugins => Self::Marketplace,
            Self::Marketplace => Self::Installed,
        }
    }

    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::Installed => Self::Marketplace,
            Self::Plugins => Self::Installed,
            Self::Marketplace => Self::Plugins,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginCapability {
    Skill,
    Mcp,
}

impl PluginCapability {
    #[must_use]
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
