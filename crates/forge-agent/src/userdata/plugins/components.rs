//! Per-plugin component + marketplace-manifest scan: the fast path
//! behind the Extensions page. Pure filesystem reads under the
//! plugins root (`$CLAUDE_CONFIG_DIR`/`~/.claude` + `plugins/`); no
//! CLI call. The plugin registry (`installed_plugins.json`) supplies
//! installed-ness, version and the auto-dependency marker;
//! `settings.json`'s `enabledPlugins` the enabled state; the
//! marketplace manifests the latest version and LSP server names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use forge_primitives::plugins::{MarketplaceHealth, PluginComponents};

/// The plugins subtree of the config dir the `claude` CLI itself
/// resolves: `$CLAUDE_CONFIG_DIR`, else `$HOME/.claude`. Both env reads
/// are launch-directory-independent; `None` when neither resolves.
pub fn plugins_root() -> Option<PathBuf> {
    let config_dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var_os("HOME").filter(|s| !s.is_empty()).map(PathBuf::from)?;
            home.join(".claude")
        }
    };
    Some(config_dir.join("plugins"))
}

#[derive(Deserialize)]
struct PluginRegistry {
    #[serde(default)]
    plugins: BTreeMap<String, Vec<RegistryEntry>>,
}

#[derive(Deserialize)]
struct RegistryEntry {
    #[serde(rename = "installPath", default)]
    install_path: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    auto: Option<bool>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(rename = "installedAt", default)]
    installed_at: Option<String>,
    #[serde(rename = "lastUpdated", default)]
    last_updated: Option<String>,
    #[serde(rename = "projectPath", default)]
    project_path: Option<String>,
}

#[derive(Deserialize)]
struct MarketplaceManifest {
    #[serde(default)]
    plugins: Vec<ManifestPlugin>,
}

#[derive(Deserialize)]
struct ManifestPlugin {
    name: String,
    #[serde(default)]
    version: Option<String>,
    /// Relative sources (`./plugins/foo`) resolve inside the
    /// marketplace clone; remote ones carry no local components.
    #[serde(default)]
    source: Option<String>,
    #[serde(rename = "lspServers", default)]
    lsp_servers: Option<BTreeMap<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct KnownMarketplaces {
    #[serde(flatten)]
    entries: BTreeMap<String, KnownMarketplaceEntry>,
}

#[derive(Deserialize)]
struct KnownMarketplaceEntry {
    #[serde(default)]
    source: Option<serde_json::Value>,
    #[serde(rename = "installLocation", default)]
    install_location: Option<String>,
}

/// One plugin dir's component inventory; `None` when the dir carries
/// no `plugin.json` and so is not a plugin.
struct DirComponents {
    skills: Vec<String>,
    agents: Vec<String>,
    commands: Vec<String>,
    hooks: Vec<String>,
    mcp: bool,
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

/// Read the component parts of one plugin dir. `gated` requires
/// `plugin.json` (cache version dirs); clone sources skip the gate
/// because a marketplace source dir is a plugin by declaration.
fn scan_component_dir(dir: &Path, gated: bool) -> Option<DirComponents> {
    if gated && !dir.join("plugin.json").is_file() {
        return None;
    }
    let skills = list_subdirs(&dir.join("skills"));
    let agents = list_md_files(&dir.join("agents"));
    let commands = list_md_files(&dir.join("commands"));
    let hooks_path = dir.join("hooks/hooks.json");
    let hooks = if hooks_path.is_file() {
        read_json(&hooks_path)
            .and_then(|doc| {
                doc.get("hooks")
                    .and_then(|hooks| hooks.as_object())
                    .map(|hooks| hooks.keys().cloned().collect::<Vec<_>>())
            })
            .unwrap_or_else(|| {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    path = %hooks_path.display(),
                    "hooks.json exists but does not parse; its triggers are not shown",
                );
                Vec::new()
            })
    } else {
        Vec::new()
    };
    Some(DirComponents { skills, agents, commands, hooks, mcp: dir.join(".mcp.json").is_file() })
}

fn list_subdirs(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn list_md_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md")))
        .map(|path| path.file_stem().unwrap_or_default().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Every marketplace manifest readable under the clones root, keyed by
/// (marketplace name, plugin name), plus per-marketplace parse errors
/// and which clones produced a readable manifest - the distinction a
/// zero-plugin manifest needs to avoid reading as a cache-miss.
struct Manifests {
    entries: BTreeMap<(String, String), ManifestPlugin>,
    errors: BTreeMap<String, String>,
    loaded: std::collections::BTreeSet<String>,
}

impl Manifests {
    fn load(marketplaces_root: &Path) -> Self {
        let mut entries = BTreeMap::new();
        let mut errors = BTreeMap::new();
        let mut loaded = std::collections::BTreeSet::new();
        let Ok(marketplaces) = std::fs::read_dir(marketplaces_root) else {
            return Self { entries, errors, loaded };
        };
        for marketplace in marketplaces.flatten() {
            if !marketplace.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = marketplace.file_name().to_string_lossy().into_owned();
            let dir = marketplace.path();
            for candidate in
                [dir.join("marketplace.json"), dir.join(".claude-plugin/marketplace.json")]
            {
                let Some(doc) = read_json(&candidate) else { continue };
                match serde_json::from_value::<MarketplaceManifest>(doc) {
                    Ok(manifest) => {
                        loaded.insert(name.clone());
                        for plugin in manifest.plugins {
                            entries.insert((name.clone(), plugin.name.clone()), plugin);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "forge_agent::userdata::plugins",
                            path = %candidate.display(),
                            error = %error,
                            "marketplace manifest failed to parse",
                        );
                        errors.insert(
                            name.clone(),
                            format!("marketplace.json parse failed: {error}"),
                        );
                    }
                }
                break;
            }
        }
        Self { entries, errors, loaded }
    }

    fn plugin(&self, marketplace: &str, name: &str) -> Option<&ManifestPlugin> {
        self.entries.get(&(marketplace.to_owned(), name.to_owned()))
    }
}

/// One combined disk scan: everything a refresh needs without a CLI
/// spawn - registry-derived installed entries, manifest-derived
/// available entries and marketplace sources, per-plugin component
/// inventories, and per-marketplace health, over one shared manifest
/// load so a corrupt manifest surfaces the same error on both halves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExtensionScan {
    pub installed: Vec<forge_primitives::plugins::InstalledPluginEntry>,
    pub marketplace: Vec<forge_primitives::plugins::MarketplaceEntry>,
    pub marketplace_sources: Vec<forge_primitives::plugins::MarketplaceSourceEntry>,
    pub components: Vec<PluginComponents>,
    pub marketplace_health: Vec<MarketplaceHealth>,
}

/// Read the plugin registry. A file that exists but does not parse is
/// warned and reads as empty - the distinction a fresh install (no
/// file) must not lose.
fn read_registry(plugins_root: &Path) -> PluginRegistry {
    let path = plugins_root.join("installed_plugins.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return PluginRegistry { plugins: BTreeMap::new() };
    };
    match serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|error| error.to_string())
        .and_then(|doc| {
            serde_json::from_value::<PluginRegistry>(doc).map_err(|error| error.to_string())
        }) {
        Ok(registry) => registry,
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %path.display(),
                error = %error,
                "installed_plugins.json exists but does not parse; reading as an empty registry",
            );
            PluginRegistry { plugins: BTreeMap::new() }
        }
    }
}

/// Scan the plugin cache and marketplace manifests into per-plugin
/// component inventories, one entry per plugin the registry or a
/// manifest knows about. Installed plugins scan their registry
/// `installPath`; uninstalled ones their marketplace clone source. A
/// registry entry whose directory is gone emits a LoadFailed row
/// rather than disappearing.
pub fn scan_extensions(
    plugins_root: &Path,
    marketplaces_root: &Path,
    config_dir: &Path,
) -> ExtensionScan {
    let registry = read_registry(plugins_root);
    let enabled = enabled_plugins(plugins_root);
    let manifests = Manifests::load(marketplaces_root);
    let marketplace_registry = read_marketplace_registry(marketplaces_root);
    let mut rows = BTreeMap::<String, PluginComponents>::new();

    // Registry installs are authoritative: the row scans the exact
    // installPath the CLI recorded.
    for (id, entries) in &registry.plugins {
        let Some(entry) = entries.first() else { continue };
        let marketplace = id.split_once('@').map_or("", |(_, marketplace)| marketplace);
        let manifest = manifests.plugin(marketplace, plugin_name(id));
        let components = entry
            .install_path
            .as_deref()
            .and_then(|path| scan_component_dir(Path::new(path), true));
        let Some(components) = components else {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                plugin = %id,
                install_path = entry.install_path.as_deref().unwrap_or(""),
                "registered plugin install dir is missing or unreadable",
            );
            rows.insert(
                id.clone(),
                PluginComponents {
                    plugin: id.clone(),
                    marketplace: marketplace.to_owned(),
                    version: entry.version.clone(),
                    installed: true,
                    enabled: enabled.is_enabled(id),
                    auto: entries.iter().any(|entry| entry.auto == Some(true)),
                    available_version: manifest.and_then(|plugin| plugin.version.clone()),
                    load_error: Some("registered install dir is missing on disk".to_owned()),
                    ..PluginComponents::default()
                },
            );
            continue;
        };
        rows.insert(
            id.clone(),
            PluginComponents {
                plugin: id.clone(),
                marketplace: marketplace.to_owned(),
                version: entry.version.clone(),
                installed: true,
                enabled: enabled.is_enabled(id),
                auto: entries.iter().any(|entry| entry.auto == Some(true)),
                available_version: manifest.and_then(|plugin| plugin.version.clone()),
                skills: components.skills,
                agents: components.agents,
                commands: components.commands,
                hooks: components.hooks,
                mcp: components.mcp,
                lsp_servers: manifest_lsp_servers(manifest),
                ..PluginComponents::default()
            },
        );
    }

    // Uninstalled plugins: components come from the marketplace clone
    // source when it declares a relative one, else a stale cache copy.
    for ((marketplace, name), plugin) in &manifests.entries {
        let id = format!("{name}@{marketplace}");
        if rows.contains_key(&id) {
            continue;
        }
        let components = plugin
            .source
            .as_deref()
            .filter(|source| source.starts_with("./"))
            .map(|source| marketplaces_root.join(marketplace).join(source))
            .and_then(|source| scan_component_dir(&source, false));
        let (skills, agents, commands, hooks, mcp) = components
            .map(|components| {
                (
                    components.skills,
                    components.agents,
                    components.commands,
                    components.hooks,
                    components.mcp,
                )
            })
            .unwrap_or_default();
        rows.insert(
            id.clone(),
            PluginComponents {
                plugin: id,
                marketplace: marketplace.clone(),
                installed: false,
                enabled: false,
                available_version: plugin.version.clone(),
                skills,
                agents,
                commands,
                hooks,
                mcp,
                lsp_servers: manifest_lsp_servers(Some(plugin)),
                ..PluginComponents::default()
            },
        );
    }

    // A marketplace whose manifest does not parse would silently drop
    // every uninstalled plugin it declares; one LoadFailed row per
    // corrupt manifest keeps the gap visible on the page.
    for (marketplace, error) in &manifests.errors {
        rows.entry(marketplace.clone()).or_insert_with(|| PluginComponents {
            plugin: marketplace.clone(),
            marketplace: marketplace.clone(),
            load_error: Some(error.clone()),
            ..PluginComponents::default()
        });
    }

    // Cache leftovers the registry and manifests both miss: a real
    // plugin dir with no registry entry. The newest version dir wins.
    let cache = plugins_root.join("cache");
    let Ok(marketplace_dirs) = std::fs::read_dir(&cache) else {
        return ExtensionScan {
            installed: installed_entries(&registry, &enabled),
            marketplace: manifest_marketplace_entries(&manifests),
            marketplace_sources: marketplace_source_entries(&marketplace_registry),
            components: rows.into_values().collect(),
            marketplace_health: marketplace_health(&marketplace_registry, &manifests, config_dir),
        };
    };
    for marketplace_dir in marketplace_dirs.flatten() {
        let marketplace = marketplace_dir.file_name().to_string_lossy().into_owned();
        let Ok(plugin_dirs) = std::fs::read_dir(marketplace_dir.path()) else { continue };
        for plugin_dir in plugin_dirs.flatten() {
            let name = plugin_dir.file_name().to_string_lossy().into_owned();
            let id = format!("{name}@{marketplace}");
            if rows.contains_key(&id) {
                continue;
            }
            let Some((_, components)) = newest_plugin_version(&plugin_dir.path()) else { continue };
            rows.insert(
                id.clone(),
                PluginComponents {
                    plugin: id,
                    marketplace: marketplace.clone(),
                    installed: false,
                    enabled: false,
                    skills: components.skills,
                    agents: components.agents,
                    commands: components.commands,
                    hooks: components.hooks,
                    mcp: components.mcp,
                    ..PluginComponents::default()
                },
            );
        }
    }

    ExtensionScan {
        installed: installed_entries(&registry, &enabled),
        marketplace: manifest_marketplace_entries(&manifests),
        marketplace_sources: marketplace_source_entries(&marketplace_registry),
        components: rows.into_values().collect(),
        marketplace_health: marketplace_health(&marketplace_registry, &manifests, config_dir),
    }
}

/// The CLI's marketplace registry (`known_marketplaces.json` beside
/// the clones), read once for both the source entries and the health.
fn read_marketplace_registry(marketplaces_root: &Path) -> KnownMarketplaces {
    let registry_path =
        marketplaces_root.parent().map(|plugins_root| plugins_root.join("known_marketplaces.json"));
    registry_path
        .and_then(|path| read_json(&path))
        .and_then(|doc| serde_json::from_value::<KnownMarketplaces>(doc).ok())
        .unwrap_or(KnownMarketplaces { entries: BTreeMap::new() })
}

/// Installed entries derived from the registry and settings, the disk
/// mirror of `claude plugin list --json`.
fn installed_entries(
    registry: &PluginRegistry,
    enabled: &EnabledMap,
) -> Vec<forge_primitives::plugins::InstalledPluginEntry> {
    registry
        .plugins
        .iter()
        .filter_map(|(id, entries)| {
            let entry = entries.first()?;
            Some(forge_primitives::plugins::InstalledPluginEntry {
                id: id.clone(),
                version: entry.version.clone(),
                scope: entry.scope.clone().unwrap_or_else(|| "user".to_owned()),
                enabled: enabled.is_enabled(id),
                installed_at: entry.installed_at.clone(),
                last_updated: entry.last_updated.clone(),
                project_path: entry.project_path.clone(),
                capability: forge_primitives::plugins::PluginCapability::Skill,
            })
        })
        .collect()
}

/// Available entries derived from the manifests, the disk mirror of
/// `claude plugin list --available --json`.
fn manifest_marketplace_entries(
    manifests: &Manifests,
) -> Vec<forge_primitives::plugins::MarketplaceEntry> {
    manifests
        .entries
        .iter()
        .map(|((marketplace, name), plugin)| forge_primitives::plugins::MarketplaceEntry {
            plugin_id: format!("{name}@{marketplace}"),
            name: name.clone(),
            description: None,
            marketplace_name: Some(marketplace.clone()),
            version: plugin.version.clone(),
            install_count: None,
            source: None,
        })
        .collect()
}

/// Configured marketplace sources derived from the registry, the disk
/// mirror of `claude plugin marketplace list --json`.
fn marketplace_source_entries(
    registry: &KnownMarketplaces,
) -> Vec<forge_primitives::plugins::MarketplaceSourceEntry> {
    registry
        .entries
        .iter()
        .map(|(name, entry)| forge_primitives::plugins::MarketplaceSourceEntry {
            name: name.clone(),
            source: entry
                .source
                .as_ref()
                .and_then(|source| source.get("source"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            repo: entry
                .source
                .as_ref()
                .and_then(|source| source.get("repo"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            install_location: entry.install_location.clone(),
        })
        .collect()
}

fn plugin_name(id: &str) -> &str {
    id.split_once('@').map_or(id, |(name, _)| name)
}

/// The manifest's LSP servers as server key -> binary command; the
/// command is what the PATH check tests, the key the fallback.
fn manifest_lsp_servers(plugin: Option<&ManifestPlugin>) -> BTreeMap<String, String> {
    plugin
        .and_then(|plugin| plugin.lsp_servers.as_ref())
        .map(|servers| {
            servers
                .iter()
                .map(|(key, value)| {
                    let command = value.get("command").and_then(serde_json::Value::as_str);
                    (key.clone(), command.unwrap_or(key).to_owned())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `enabledPlugins` from `settings.json` beside the plugins root; the
/// CLI's own enable state. An id absent from the map is enabled.
fn enabled_plugins(plugins_root: &Path) -> EnabledMap {
    let settings = plugins_root
        .parent()
        .map(|config_dir| config_dir.join("settings.json"))
        .and_then(|path| read_json(&path));
    let map = settings
        .and_then(|settings| {
            settings.get("enabledPlugins").and_then(|map| map.as_object()).cloned()
        })
        .unwrap_or_default();
    EnabledMap { map }
}

struct EnabledMap {
    map: serde_json::Map<String, serde_json::Value>,
}

impl EnabledMap {
    fn is_enabled(&self, id: &str) -> bool {
        self.map.get(id).and_then(serde_json::Value::as_bool) != Some(false)
    }
}

fn newest_plugin_version(plugin_dir: &Path) -> Option<(PathBuf, DirComponents)> {
    let Ok(entries) = std::fs::read_dir(plugin_dir) else { return None };
    let mut versions = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    // Numeric-aware: "10.0.0" sorts after "9.9.9", which a plain
    // string sort would not deliver.
    versions.sort_by_key(|path| {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        version_sort_key(&name)
    });
    versions.reverse();
    versions.into_iter().find_map(|version| {
        scan_component_dir(&version, true).map(|components| (version, components))
    })
}

/// A version directory name's numeric-aware sort key: numeric chunks
/// order before and inside text chunks, so "10.0.0" outranks "9.9.9".
fn version_sort_key(name: &str) -> Vec<(u8, u64, String)> {
    name.split(['.', '-', '_'])
        .map(|chunk| match chunk.parse::<u64>() {
            Ok(number) => (0_u8, number, String::new()),
            Err(_) => (1_u8, 0_u64, chunk.to_owned()),
        })
        .collect()
}

/// Per-marketplace health over the shared manifest load. A manifest
/// the load could not parse surfaces its recorded error; one that
/// parses but shows no plugins is healthy at zero.
fn marketplace_health(
    registry: &KnownMarketplaces,
    manifests: &Manifests,
    config_dir: &Path,
) -> Vec<MarketplaceHealth> {
    registry
        .entries
        .iter()
        .map(|(name, entry)| {
            let install_location =
                entry.install_location.clone().map(PathBuf::from).unwrap_or_default();
            let drifted = !install_location.as_os_str().is_empty()
                && !install_location.starts_with(config_dir);
            let load_error = manifests.errors.get(name).cloned();
            let available = if load_error.is_some() {
                0
            } else {
                manifests
                    .entries
                    .keys()
                    .filter(|(manifest_marketplace, _)| manifest_marketplace == name)
                    .count()
            };
            MarketplaceHealth {
                name: name.clone(),
                source: entry
                    .source
                    .as_ref()
                    .and_then(|source| source.get("source"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                available,
                load_error: load_error.or_else(|| {
                    (!manifests.loaded.contains(name)).then(|| {
                        if install_location.as_os_str().is_empty() {
                            "no marketplace clone on disk".to_owned()
                        } else {
                            "no marketplace.json found in the clone".to_owned()
                        }
                    })
                }),
                install_location,
                drifted,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture layout, all under one tempdir:
    ///
    /// home/.claude/
    ///   settings.json                (enabledPlugins: off -> false)
    ///   plugins/installed_plugins.json  (off carries auto)
    ///   plugins/cache/probe-market/
    ///     full/1.2.3/                every component kind
    ///     off/3.0.0/                 one skill
    ///     gone/1.0.0/                stale leftover, not installed
    ///     broken/9.9.9/              no plugin.json -> excluded
    ///   plugins/marketplaces/probe-market/
    ///     .claude-plugin/marketplace.json
    ///     full-src/, gone-src/       clone sources for components
    ///   plugins/known_marketplaces.json
    struct Fixture {
        root: tempfile::TempDir,
        plugins_root: PathBuf,
        marketplaces_root: PathBuf,
        config_dir: PathBuf,
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write fixture file");
    }

    fn skill(path: &Path) {
        write(path, "# skill");
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().expect("tempdir");
        let home = root.path().join("home");
        let config_dir = home.join(".claude");
        let plugins_root = config_dir.join("plugins");
        let marketplaces_root = plugins_root.join("marketplaces");
        let cache = plugins_root.join("cache");
        let mkt = marketplaces_root.join("probe-market");

        // full: every component kind, installed and enabled.
        let full = cache.join("probe-market").join("full").join("1.2.3");
        write(&full.join("plugin.json"), r#"{"name":"full","version":"1.2.3"}"#);
        skill(&full.join("skills/a/SKILL.md"));
        skill(&full.join("skills/b/SKILL.md"));
        write(&full.join("agents/x.md"), "# x");
        write(&full.join("agents/y.md"), "# y");
        write(&full.join("commands/c1.md"), "# c1");
        write(&full.join("commands/c2.md"), "# c2");
        write(&full.join("hooks/hooks.json"), r#"{"hooks":{"SessionStart":[],"PreToolUse":[]}}"#);
        write(&full.join(".mcp.json"), r#"{"mcpServers":{}}"#);

        // off: installed but disabled, registry marks auto.
        let off = cache.join("probe-market").join("off").join("3.0.0");
        write(&off.join("plugin.json"), r#"{"name":"off","version":"3.0.0"}"#);
        skill(&off.join("skills/one/SKILL.md"));

        // gone: stale cache copy; nothing in the registry. The
        // marketplace clone source carries its real components.
        let gone = cache.join("probe-market").join("gone").join("1.0.0");
        write(&gone.join("plugin.json"), r#"{"name":"gone","version":"1.0.0"}"#);
        skill(&gone.join("skills/stale/SKILL.md"));

        // broken: no plugin.json, excluded from the scan.
        let broken = cache.join("probe-market").join("broken").join("9.9.9");
        skill(&broken.join("skills/a/SKILL.md"));

        write(
            &config_dir.join("settings.json"),
            r#"{"enabledPlugins":{"off@probe-market":false}}"#,
        );
        write(
            &plugins_root.join("installed_plugins.json"),
            r#"{"version":2,"plugins":{
                "full@probe-market":[{"scope":"user","installPath":"CACHE_FULL","version":"1.2.3","installedAt":"","lastUpdated":""}],
                "off@probe-market":[{"scope":"user","installPath":"CACHE_OFF","version":"3.0.0","installedAt":"","lastUpdated":"","auto":true}]
            }}"#
            .replace("CACHE_FULL", &full.to_string_lossy())
            .replace("CACHE_OFF", &off.to_string_lossy())
            .as_str(),
        );

        // Marketplace manifest: full is at 2.0.0 with an LSP server;
        // gone is available at 6.4.0 with a relative clone source.
        write(
            &mkt.join(".claude-plugin/marketplace.json"),
            r#"{"name":"probe-market","plugins":[
                {"name":"full","version":"2.0.0","source":"./full-src",
                 "lspServers":{"rust-analyzer":{"command":"rust-analyzer"}}},
                {"name":"gone","version":"6.4.0","source":"./gone-src"}
            ]}"#,
        );
        skill(&mkt.join("full-src/skills/clone-skill/SKILL.md"));
        skill(&mkt.join("gone-src/skills/writing-skills/SKILL.md"));
        skill(&mkt.join("gone-src/skills/not-a-dir.md"));
        write(&mkt.join("gone-src/commands/do.md"), "# do");

        // A second marketplace whose clone is missing entirely: the
        // cache-miss health case.
        write(
            &plugins_root.join("known_marketplaces.json"),
            format!(
                r#"{{"probe-market":{{"source":{{"source":"github","repo":"a/b"}},
                     "installLocation":{:?},"lastUpdated":""}},
                   "ghost":{{"source":{{"source":"github","repo":"c/d"}},
                     "installLocation":{:?},"lastUpdated":""}}}}"#,
                mkt.to_string_lossy(),
                mkt.to_string_lossy()
            )
            .as_str(),
        );

        Fixture { root, plugins_root, marketplaces_root, config_dir }
    }

    fn by_plugin<'a>(rows: &'a [PluginComponents], id: &str) -> &'a PluginComponents {
        rows.iter().find(|row| row.plugin == id).unwrap_or_else(|| panic!("no row for {id}"))
    }

    // -- scan_components -------------------------------------------------

    #[test]
    fn a_plugin_with_every_component_kind_scans_exact_lists() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;

        let full = by_plugin(&rows, "full@probe-market");
        assert_eq!(full.skills, vec!["a", "b"]);
        assert_eq!(full.agents, vec!["x", "y"]);
        assert_eq!(full.commands, vec!["c1", "c2"]);
        assert_eq!(full.hooks, vec!["SessionStart", "PreToolUse"]);
        assert!(full.mcp, ".mcp.json present");
        assert_eq!(
            full.lsp_servers,
            BTreeMap::from([("rust-analyzer".to_owned(), "rust-analyzer".to_owned())])
        );
        assert!(full.installed);
        assert!(full.enabled);
        assert!(!full.auto);
        assert_eq!(full.version.as_deref(), Some("1.2.3"));
        assert_eq!(full.available_version.as_deref(), Some("2.0.0"));
        assert_eq!(full.marketplace, "probe-market");
    }

    #[test]
    fn a_plugin_missing_plugin_json_is_excluded() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;
        assert!(
            rows.iter().all(|row| !row.plugin.starts_with("broken")),
            "no row may exist for the plugin.json-less dir: {rows:?}"
        );
    }

    #[test]
    fn a_skill_file_is_not_a_skill() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;
        let gone = by_plugin(&rows, "gone@probe-market");
        assert_eq!(gone.skills, vec!["writing-skills"], "file-not-dir excluded; clone scanned");
    }

    #[test]
    fn an_uninstalled_plugin_scans_from_its_marketplace_clone() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;
        let gone = by_plugin(&rows, "gone@probe-market");
        assert!(!gone.installed);
        assert!(!gone.enabled);
        assert_eq!(gone.version, None);
        assert_eq!(gone.available_version.as_deref(), Some("6.4.0"));
        assert_eq!(gone.commands, vec!["do"]);
        assert!(!gone.mcp);
    }

    #[test]
    fn a_disabled_auto_installed_plugin_carries_both_markers() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;
        let off = by_plugin(&rows, "off@probe-market");
        assert!(off.installed);
        assert!(!off.enabled, "disabled in enabledPlugins");
        assert!(off.auto, "auto marker comes from the registry file");
        assert_eq!(off.skills, vec!["one"]);
        assert_eq!(off.available_version, None, "manifest names no off entry");
    }

    #[test]
    fn components_come_from_the_installed_copy_not_the_clone() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;
        let full = by_plugin(&rows, "full@probe-market");
        assert_eq!(
            full.skills,
            vec!["a", "b"],
            "the clone's clone-skill must not leak into the installed inventory"
        );
    }

    // -- scan_marketplaces -----------------------------------------------

    #[test]
    fn a_marketplace_inside_the_config_dir_is_healthy() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .marketplace_health;
        let probe = rows.iter().find(|row| row.name == "probe-market").expect("probe row");
        assert_eq!(probe.available, 2);
        assert_eq!(probe.source, "github");
        assert_eq!(probe.load_error, None);
        assert!(!probe.drifted);
        assert_eq!(probe.install_location, fixture.marketplaces_root.join("probe-market"));
    }

    #[test]
    fn an_install_location_outside_the_config_dir_is_drifted() {
        let fixture = fixture();
        // A foreign config dir: every registry location sits outside it.
        let foreign = fixture.root.path().join("elsewhere").join(".claude");
        let rows = scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &foreign)
            .marketplace_health;
        let probe = rows.iter().find(|row| row.name == "probe-market").expect("probe row");
        assert!(probe.drifted, "{probe:?}");
    }

    #[test]
    fn a_marketplace_missing_its_manifest_is_a_load_error() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .marketplace_health;
        let ghost = rows.iter().find(|row| row.name == "ghost").expect("ghost row");
        assert_eq!(ghost.available, 0);
        assert!(ghost.load_error.is_some(), "cache-miss reads as a load error: {ghost:?}");
    }
}
