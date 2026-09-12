//! Per-plugin component + marketplace-manifest scan: the fast path
//! behind the Extensions page. Pure filesystem reads under the config
//! dir (`$CLAUDE_CONFIG_DIR`/`~/.claude`) and its `plugins/` subtree,
//! `settings.json` included; no CLI call. The plugin registry
//! (`installed_plugins.json`) supplies installed-ness, version and the
//! auto-dependency marker; `settings.json`'s `enabledPlugins` the
//! enabled state; the marketplace manifests the latest version and LSP
//! server names.

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
    /// A clone-relative string (`./plugins/foo`) or a remote
    /// descriptor object (`url`, `git-subdir`); the official
    /// marketplace ships both. Only the string form names local
    /// components.
    #[serde(default)]
    source: Option<serde_json::Value>,
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

/// One plugin dir's component inventory.
struct DirComponents {
    skills: Vec<String>,
    agents: Vec<String>,
    commands: Vec<String>,
    hooks: Vec<String>,
    mcp: bool,
}

/// Read and parse a JSON file. All three outcomes return `None`, so the
/// caller cannot tell them apart from the value; the warn carries the
/// difference: a missing file is the expected fresh-install case and
/// stays silent, while an unreadable or unparseable one warns with the
/// reason.
fn read_json(path: &Path) -> Option<serde_json::Value> {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(doc) => Some(doc),
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    path = %path.display(),
                    error = %error,
                    "file exists but does not parse; reading as absent",
                );
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %path.display(),
                error = %error,
                "file exists but cannot be read; reading as absent",
            );
            None
        }
    }
}

/// Whether `path` is a regular file. `Path::is_file` folds every stat
/// error into "not there", which reads a permission or io failure as an
/// absent file; this reports it instead.
fn is_regular_file(path: &Path) -> Result<bool, std::io::Error> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Read the component parts of one plugin dir. `Err` is an io error
/// that stopped the dir being read - its own `read_dir`, or for a
/// `gated` dir the stat of its `.claude-plugin/plugin.json`; `Ok(None)`
/// when a gated dir carries no such manifest. Registered installs read
/// ungated because the registry's installPath is authoritative; clone
/// sources read ungated because a marketplace source dir is a plugin by
/// declaration.
fn read_component_dir(dir: &Path, gated: bool) -> Result<Option<DirComponents>, std::io::Error> {
    std::fs::read_dir(dir)?;
    if gated && !is_regular_file(&dir.join(".claude-plugin/plugin.json"))? {
        return Ok(None);
    }
    let skills = list_subdirs(&dir.join("skills"));
    let agents = list_md_files(&dir.join("agents"));
    let commands = list_md_files(&dir.join("commands"));
    let hooks_path = dir.join("hooks/hooks.json");
    // A hooks or MCP file that cannot be stat'd costs the plugin that
    // component, not its row: failing the whole row would hide whatever
    // the sibling reads did return.
    let hooks = match is_regular_file(&hooks_path) {
        Ok(true) => match read_json(&hooks_path) {
            Some(doc) => doc.get("hooks").and_then(|hooks| hooks.as_object()).map_or_else(
                || {
                    tracing::warn!(
                        target: "forge_agent::userdata::plugins",
                        path = %hooks_path.display(),
                        "hooks.json names no hooks object; its triggers are not shown",
                    );
                    Vec::new()
                },
                |hooks| hooks.keys().cloned().collect::<Vec<_>>(),
            ),
            None => Vec::new(),
        },
        Ok(false) => Vec::new(),
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %hooks_path.display(),
                error = %error,
                "hooks.json cannot be read; its triggers are not shown",
            );
            Vec::new()
        }
    };
    let mcp_path = dir.join(".mcp.json");
    let mcp = match is_regular_file(&mcp_path) {
        Ok(present) => present,
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %mcp_path.display(),
                error = %error,
                "the plugin's MCP config cannot be read; the row's MCP flag is not set",
            );
            false
        }
    };
    Ok(Some(DirComponents { skills, agents, commands, hooks, mcp }))
}

/// The reason shown for a registered install whose dir is unusable. A
/// vanished dir and an unreadable one point the reader at different
/// remedies, so the row says which; an entry recording no path reads
/// as the dir being missing.
fn install_dir_reason(error: Option<&std::io::Error>) -> String {
    match error.map(std::io::Error::kind) {
        Some(std::io::ErrorKind::NotFound) | None => {
            "registered install dir is missing on disk".to_owned()
        }
        Some(_) => "registered install dir cannot be read".to_owned(),
    }
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
/// (marketplace name, plugin name), plus per-marketplace load errors,
/// read or parse, and which clones produced a readable manifest - the
/// distinction a zero-plugin manifest needs to avoid reading as a
/// cache-miss.
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
        let marketplaces = match std::fs::read_dir(marketplaces_root) {
            Ok(marketplaces) => marketplaces,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self { entries, errors, loaded };
            }
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    path = %marketplaces_root.display(),
                    error = %error,
                    "marketplaces root cannot be read; every configured marketplace reads as a cache-miss",
                );
                return Self { entries, errors, loaded };
            }
        };
        for marketplace in marketplaces {
            let marketplace = match marketplace {
                Ok(marketplace) => marketplace,
                Err(error) => {
                    tracing::warn!(
                        target: "forge_agent::userdata::plugins",
                        path = %marketplaces_root.display(),
                        error = %error,
                        "marketplace entry cannot be read; its catalog is not listed",
                    );
                    continue;
                }
            };
            match marketplace.file_type() {
                Ok(file_type) if file_type.is_dir() => {}
                Ok(_) => continue,
                // FileType is readdir's d_type; only the stat fallback
                // fails, and a skipped clone then reads as a cache-miss.
                Err(error) => {
                    tracing::warn!(
                        target: "forge_agent::userdata::plugins",
                        path = %marketplace.path().display(),
                        error = %error,
                        "marketplace entry cannot be read; it reads as a cache-miss",
                    );
                    continue;
                }
            }
            let name = marketplace.file_name().to_string_lossy().into_owned();
            let dir = marketplace.path();
            for candidate in
                [dir.join("marketplace.json"), dir.join(".claude-plugin/marketplace.json")]
            {
                match is_regular_file(&candidate) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        tracing::warn!(
                            target: "forge_agent::userdata::plugins",
                            path = %candidate.display(),
                            error = %error,
                            "marketplace manifest cannot be read; this marketplace reads as a load failure",
                        );
                        errors.insert(
                            name.clone(),
                            format!("marketplace.json cannot be read: {error}"),
                        );
                        break;
                    }
                }
                // Read failure and parse failure are different classes:
                // a permission-denied manifest is not "failed to parse".
                match std::fs::read_to_string(&candidate) {
                    Err(error) => {
                        tracing::warn!(
                            target: "forge_agent::userdata::plugins",
                            path = %candidate.display(),
                            error = %error,
                            "marketplace manifest cannot be read",
                        );
                        errors.insert(
                            name.clone(),
                            format!("marketplace.json cannot be read: {error}"),
                        );
                        break;
                    }
                    Ok(text) => match serde_json::from_str::<MarketplaceManifest>(&text) {
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
                            // A present-but-broken manifest must never
                            // read as a cache-miss: that sends the user
                            // to Repair against a healthy clone.
                            errors.insert(
                                name.clone(),
                                format!("marketplace.json failed to parse: {error}"),
                            );
                        }
                    },
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
/// load so a failed manifest surfaces the same error on both halves.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExtensionScan {
    pub installed: Vec<forge_primitives::plugins::InstalledPluginEntry>,
    pub marketplace: Vec<forge_primitives::plugins::MarketplaceEntry>,
    pub marketplace_sources: Vec<forge_primitives::plugins::MarketplaceSourceEntry>,
    pub components: Vec<PluginComponents>,
    pub marketplace_health: Vec<MarketplaceHealth>,
}

/// Read the plugin registry. A missing file is the fresh install and
/// reads as empty in silence; an unreadable or unparseable one warns
/// with the reason and reads as empty too; the page renders
/// identically either way, so the warn is the only place the
/// difference shows.
fn read_registry(plugins_root: &Path) -> PluginRegistry {
    let path = plugins_root.join("installed_plugins.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return PluginRegistry { plugins: BTreeMap::new() };
        }
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %path.display(),
                error = %error,
                "installed_plugins.json cannot be read; reading as an empty registry",
            );
            return PluginRegistry { plugins: BTreeMap::new() };
        }
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
/// component inventories, one entry per plugin the registry, a
/// manifest or the cache knows about. Installed plugins scan their
/// registry `installPath`; uninstalled ones their marketplace clone
/// source. A registry entry with no readable install dir - gone,
/// unreadable, or absent from the entry itself - emits a LoadFailed
/// row carrying its reason rather than disappearing, and so does a
/// marketplace whose manifest could not load.
pub fn scan_extensions(
    plugins_root: &Path,
    marketplaces_root: &Path,
    config_dir: &Path,
) -> ExtensionScan {
    let registry = read_registry(plugins_root);
    let enabled = enabled_plugins(plugins_root);
    let manifests = Manifests::load(marketplaces_root);
    let (marketplace_registry, registry_error) = read_marketplace_registry(marketplaces_root);
    let mut rows = BTreeMap::<String, PluginComponents>::new();

    // Registry installs are authoritative: the row scans the exact
    // installPath the CLI recorded.
    for (id, entries) in &registry.plugins {
        let Some(entry) = entries.first() else { continue };
        let marketplace = id.split_once('@').map_or("", |(_, marketplace)| marketplace);
        let manifest = manifests.plugin(marketplace, plugin_name(id));
        let (components, reason) = if let Some(path) = entry.install_path.as_deref() {
            match read_component_dir(Path::new(path), false) {
                Ok(components) => (components, None),
                Err(error) => {
                    tracing::warn!(
                        target: "forge_agent::userdata::plugins",
                        plugin = %id,
                        path = %path,
                        error = %error,
                        "registered plugin install dir cannot be read",
                    );
                    (None, Some(install_dir_reason(Some(&error))))
                }
            }
        } else {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                plugin = %id,
                "registered plugin entry carries no installPath",
            );
            (None, Some(install_dir_reason(None)))
        };
        let Some(components) = components else {
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
                    load_error: reason,
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

    // Uninstalled plugins: components come from the clone source when
    // the manifest declares a relative one, and are empty otherwise -
    // an object source names a remote, and a missing clone dir yields
    // nothing. The cache walk below covers what neither list names.
    for ((marketplace, name), plugin) in &manifests.entries {
        let id = format!("{name}@{marketplace}");
        if rows.contains_key(&id) {
            continue;
        }
        let source_dir = plugin
            .source
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .filter(|source| source.starts_with("./"))
            .map(|source| marketplaces_root.join(marketplace).join(source));
        let components = source_dir.and_then(|source| match read_component_dir(&source, false) {
            Ok(components) => components,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    plugin = %id,
                    path = %source.display(),
                    error = %error,
                    "marketplace clone source cannot be read; its components are not shown",
                );
                None
            }
        });
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

    // A marketplace whose manifest does not load would silently drop
    // every uninstalled plugin it declares; one LoadFailed row per
    // failed manifest keeps the gap visible on the page.
    for (marketplace, error) in &manifests.errors {
        rows.entry(marketplace.clone()).or_insert_with(|| PluginComponents {
            plugin: marketplace.clone(),
            marketplace: marketplace.clone(),
            load_error: Some(error.clone()),
            ..PluginComponents::default()
        });
    }

    // Cache leftovers neither the registry nor a manifest names: a real
    // plugin dir with no entry on either side. The newest version dir
    // carrying a manifest wins.
    let cache = plugins_root.join("cache");
    let marketplace_dirs = std::fs::read_dir(&cache);
    if let Err(error) = &marketplace_dirs
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            target: "forge_agent::userdata::plugins",
            path = %cache.display(),
            error = %error,
            "plugin cache cannot be read; no cache rows are shown",
        );
    }
    let Ok(marketplace_dirs) = marketplace_dirs else {
        return ExtensionScan {
            installed: installed_entries(&registry, &enabled),
            marketplace: manifest_marketplace_entries(&manifests),
            marketplace_sources: match &registry_error {
                // An unreadable or unparseable registry cannot list the
                // sources; the clone dirs still name them.
                Some(_) => manifests
                    .loaded
                    .iter()
                    .chain(manifests.errors.keys())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .map(|name| forge_primitives::plugins::MarketplaceSourceEntry {
                        name: name.clone(),
                        source: None,
                        repo: None,
                        install_location: None,
                    })
                    .collect(),
                None => marketplace_source_entries(&marketplace_registry),
            },
            components: rows.into_values().collect(),
            marketplace_health: marketplace_health(
                &marketplace_registry,
                &manifests,
                config_dir,
                registry_error.as_deref(),
            ),
        };
    };
    for marketplace_dir in marketplace_dirs {
        let marketplace_dir = match marketplace_dir {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    path = %cache.display(),
                    error = %error,
                    "cache entry cannot be read; its leftovers are not shown",
                );
                continue;
            }
        };
        // Mirrors the manifest walk: only a dir is a marketplace, and a
        // stat that fails is not read as one.
        match marketplace_dir.file_type() {
            Ok(file_type) if file_type.is_dir() => {}
            Ok(_) => continue,
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    path = %marketplace_dir.path().display(),
                    error = %error,
                    "cache entry cannot be read; its leftovers are not shown",
                );
                continue;
            }
        }
        let marketplace = marketplace_dir.file_name().to_string_lossy().into_owned();
        let plugin_dirs = std::fs::read_dir(marketplace_dir.path());
        if let Err(error) = &plugin_dirs
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %marketplace_dir.path().display(),
                error = %error,
                "marketplace cache dir cannot be read; its leftovers are not shown",
            );
        }
        let Ok(plugin_dirs) = plugin_dirs else { continue };
        for plugin_dir in plugin_dirs {
            let plugin_dir = match plugin_dir {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(
                        target: "forge_agent::userdata::plugins",
                        path = %marketplace_dir.path().display(),
                        error = %error,
                        "cache plugin entry cannot be read; it is not listed",
                    );
                    continue;
                }
            };
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
        marketplace_sources: match &registry_error {
            // An unreadable or unparseable registry cannot list the
            // sources; the clone dirs still name them.
            Some(_) => manifests
                .loaded
                .iter()
                .chain(manifests.errors.keys())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|name| forge_primitives::plugins::MarketplaceSourceEntry {
                    name: name.clone(),
                    source: None,
                    repo: None,
                    install_location: None,
                })
                .collect(),
            None => marketplace_source_entries(&marketplace_registry),
        },
        components: rows.into_values().collect(),
        marketplace_health: marketplace_health(
            &marketplace_registry,
            &manifests,
            config_dir,
            registry_error.as_deref(),
        ),
    }
}

/// The CLI's marketplace registry (`known_marketplaces.json` beside
/// the clones), read once for both the source entries and the health.
/// A corrupt OR unreadable registry reads empty with its error carried
/// back - the health rows must name it, not render the tab as an
/// absence. Missing is the expected fresh-install case and stays
/// silent.
fn read_marketplace_registry(marketplaces_root: &Path) -> (KnownMarketplaces, Option<String>) {
    let registry_path =
        marketplaces_root.parent().map(|plugins_root| plugins_root.join("known_marketplaces.json"));
    let Some(path) = registry_path else {
        return (KnownMarketplaces { entries: BTreeMap::new() }, None);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (KnownMarketplaces { entries: BTreeMap::new() }, None);
        }
        Err(error) => {
            let message = format!("known_marketplaces.json cannot be read: {error}");
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %path.display(),
                error = %error,
                "marketplace registry cannot be read; the configured marketplaces cannot be listed",
            );
            return (KnownMarketplaces { entries: BTreeMap::new() }, Some(message));
        }
    };
    match serde_json::from_str::<KnownMarketplaces>(&text) {
        Ok(registry) => (registry, None),
        Err(error) => {
            let message = format!("known_marketplaces.json failed to parse: {error}");
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %path.display(),
                error = %error,
                "marketplace registry does not parse; the configured marketplaces cannot be listed",
            );
            (KnownMarketplaces { entries: BTreeMap::new() }, Some(message))
        }
    }
}

/// Installed entries derived from the registry and settings, the disk
/// mirror of `claude plugin list --json`. One entry per registry
/// record: a plugin installed in user AND project scope keeps both,
/// like the CLI list does.
fn installed_entries(
    registry: &PluginRegistry,
    enabled: &EnabledMap,
) -> Vec<forge_primitives::plugins::InstalledPluginEntry> {
    registry
        .plugins
        .iter()
        .flat_map(|(id, entries)| {
            entries.iter().map(move |entry| forge_primitives::plugins::InstalledPluginEntry {
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
/// CLI's own enable state. An id absent from the map is enabled. An
/// unreadable or unparseable settings file reads as all-enabled WITH a
/// warning - the page must not silently flip every disabled plugin to
/// Current.
fn enabled_plugins(plugins_root: &Path) -> EnabledMap {
    let settings_path = plugins_root.parent().map(|config_dir| config_dir.join("settings.json"));
    let map = settings_path
        .and_then(|path| read_json(&path))
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

/// The newest version dir of a cache plugin, with its inventory. A dir
/// that cannot be read - its own `read_dir`, or the stat of its
/// manifest - stops the search: an older version's inventory shown as
/// this plugin's own would be confidently wrong. Two arms fall through
/// instead: a vanished dir, and a dir carrying no manifest, which is
/// not a version dir at all.
fn newest_plugin_version(plugin_dir: &Path) -> Option<(PathBuf, DirComponents)> {
    let entries = match std::fs::read_dir(plugin_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::userdata::plugins",
                path = %plugin_dir.display(),
                error = %error,
                "cache plugin dir cannot be read; its versions are not shown",
            );
            return None;
        }
    };
    let mut versions = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    path = %plugin_dir.display(),
                    error = %error,
                    "cache dir entry cannot be read; no inventory is shown for the plugin",
                );
                return None;
            }
        };
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => versions.push(entry.path()),
            Ok(_) => {}
            // FileType is readdir's d_type, so only the stat fallback
            // fails; an unreadable version dir stops the search.
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    path = %entry.path().display(),
                    error = %error,
                    "cache entry cannot be read; no inventory is shown for the plugin",
                );
                return None;
            }
        }
    }
    versions.sort_by_key(|path| {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        version_sort_key(&name)
    });
    versions.reverse();
    for version in versions {
        match read_component_dir(&version, true) {
            Ok(Some(components)) => return Some((version, components)),
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    target: "forge_agent::userdata::plugins",
                    path = %version.display(),
                    error = %error,
                    "cache plugin version dir cannot be read; no inventory is shown for the plugin",
                );
                return None;
            }
        }
    }
    tracing::warn!(
        target: "forge_agent::userdata::plugins",
        path = %plugin_dir.display(),
        "cache plugin dir carries no version dir with a manifest; it lists nothing",
    );
    None
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
/// the load could not read or parse surfaces its recorded error; one
/// that loads but shows no plugins is healthy at zero.
fn marketplace_health(
    registry: &KnownMarketplaces,
    manifests: &Manifests,
    config_dir: &Path,
    registry_error: Option<&str>,
) -> Vec<MarketplaceHealth> {
    // An unreadable or unparseable known_marketplaces.json leaves the
    // registry empty; the clone dirs still exist, so each carries the
    // load error rather than the tab reading as an absence.
    if let (Some(error), true) = (registry_error, registry.entries.is_empty()) {
        let names = manifests
            .loaded
            .iter()
            .chain(manifests.errors.keys())
            .collect::<std::collections::BTreeSet<_>>();
        return names
            .into_iter()
            .map(|name| MarketplaceHealth {
                name: name.clone(),
                load_error: Some((*error).to_owned()),
                ..MarketplaceHealth::default()
            })
            .collect();
    }
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

    /// Fixture layout, all under one tempdir. A version dir carries its
    /// manifest at `.claude-plugin/plugin.json`.
    ///
    /// home/.claude/
    ///   settings.json                (enabledPlugins: off -> false)
    ///   plugins/installed_plugins.json  (off carries auto; nopath omits
    ///                                   installPath; filed points at
    ///                                   stray-file.txt)
    ///   plugins/cache/probe-market/
    ///     full/1.2.3/                every component kind
    ///     off/3.0.0/                 one skill
    ///     gone/1.0.0/                stale leftover, not installed
    ///     lsponly/1.0.0/             registered, no manifest -> LSP comes
    ///                                from the marketplace manifest
    ///     broken/9.9.9/              manifest-less cache leftover -> excluded
    ///     rootonly/9.9.9/            root-level manifest -> excluded
    ///     stray/1.0.0/               valid leftover -> available row
    ///     multi/1.0.0, 2.0.0, 3.0.0  three versions; 3.0.0 has none
    ///     unreadable/1.0.0/          registered, mode 000 -> LoadFailed
    ///     ghosted/1.0.0/missing-dir  registered, dir gone -> LoadFailed
    ///   plugins/marketplaces/probe-market/
    ///     .claude-plugin/marketplace.json
    ///     full-src/, gone-src/       clone sources for components
    ///     file-source                clone source that is a file
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
        write(&full.join(".claude-plugin/plugin.json"), r#"{"name":"full","version":"1.2.3"}"#);
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
        write(&off.join(".claude-plugin/plugin.json"), r#"{"name":"off","version":"3.0.0"}"#);
        skill(&off.join("skills/one/SKILL.md"));

        // gone: stale cache copy; nothing in the registry. The
        // marketplace clone source carries its real components.
        let gone = cache.join("probe-market").join("gone").join("1.0.0");
        write(&gone.join(".claude-plugin/plugin.json"), r#"{"name":"gone","version":"1.0.0"}"#);
        skill(&gone.join("skills/stale/SKILL.md"));

        // broken: a cache leftover carrying no manifest at all.
        let broken = cache.join("probe-market").join("broken").join("9.9.9");
        skill(&broken.join("skills/a/SKILL.md"));

        // rootonly: a cache leftover whose manifest sits at the dir
        // root, which is not the install layout.
        let rootonly = cache.join("probe-market").join("rootonly").join("9.9.9");
        write(&rootonly.join("plugin.json"), r#"{"name":"rootonly","version":"9.9.9"}"#);
        skill(&rootonly.join("skills/a/SKILL.md"));

        // stray: proves the cache-leftover walk emits rows, so the
        // exclusions in that test are not vacuous.
        let stray = cache.join("probe-market").join("stray").join("1.0.0");
        write(&stray.join(".claude-plugin/plugin.json"), r#"{"name":"stray","version":"1.0.0"}"#);
        skill(&stray.join("skills/a/SKILL.md"));

        // multi: a cache leftover with three version dirs. Tests chmod
        // 2.0.0, its .claude-plugin, and the plugin dir itself; 3.0.0
        // carries no manifest at all.
        let multi = cache.join("probe-market").join("multi");
        write(
            &multi.join("1.0.0/.claude-plugin/plugin.json"),
            r#"{"name":"multi","version":"1.0.0"}"#,
        );
        skill(&multi.join("1.0.0/skills/old/SKILL.md"));
        write(
            &multi.join("2.0.0/.claude-plugin/plugin.json"),
            r#"{"name":"multi","version":"2.0.0"}"#,
        );
        skill(&multi.join("2.0.0/skills/new/SKILL.md"));
        // 3.0.0 sorts newest and carries no manifest, so the walk skips
        // it for the 2.0.0 below - the fall-through its doc names.
        skill(&multi.join("3.0.0/skills/bare/SKILL.md"));

        // lsponly: registered with a real dir that ships nothing but a
        // README - the rust-analyzer-lsp/gopls-lsp shape.
        let lsponly = cache.join("probe-market").join("lsponly").join("1.0.0");
        write(&lsponly.join("README.md"), "# lsp only");

        // unreadable: registered with a real dir whose permissions one
        // test strips; mode 000 still passes is_dir, so only a
        // readability probe catches it.
        let unreadable = cache.join("probe-market").join("unreadable").join("1.0.0");
        write(
            &unreadable.join(".claude-plugin/plugin.json"),
            r#"{"name":"unreadable","version":"1.0.0"}"#,
        );
        skill(&unreadable.join("skills/a/SKILL.md"));

        // filed: a registry entry whose installPath is a regular file.
        let filed = plugins_root.join("stray-file.txt");
        write(&filed, "not a plugin dir");

        // ghosted: registered but its install dir does not exist -
        // the honest-scan LoadFailed row.
        let ghosted = cache.join("probe-market").join("ghosted").join("1.0.0");

        write(
            &config_dir.join("settings.json"),
            r#"{"enabledPlugins":{"off@probe-market":false}}"#,
        );
        write(
            &plugins_root.join("installed_plugins.json"),
            r#"{"version":2,"plugins":{
                "full@probe-market":[{"scope":"user","installPath":"CACHE_FULL","version":"1.2.3","installedAt":"","lastUpdated":""}],
                "off@probe-market":[{"scope":"user","installPath":"CACHE_OFF","version":"3.0.0","installedAt":"","lastUpdated":"","auto":true}],
                "lsponly@probe-market":[{"scope":"user","installPath":"CACHE_LSPONLY","version":"1.0.0","installedAt":"","lastUpdated":""}],
                "unreadable@probe-market":[{"scope":"user","installPath":"CACHE_UNREADABLE","version":"1.0.0","installedAt":"","lastUpdated":""}],
                "nopath@probe-market":[{"scope":"user","version":"1.0.0","installedAt":"","lastUpdated":""}],
                "filed@probe-market":[{"scope":"user","installPath":"CACHE_FILED","version":"1.0.0","installedAt":"","lastUpdated":""}],
                "ghosted@probe-market":[{"scope":"user","installPath":"CACHE_GHOSTED","version":"1.0.0","installedAt":"","lastUpdated":""}]
            }}"#
            .replace("CACHE_FULL", &full.to_string_lossy())
            .replace("CACHE_OFF", &off.to_string_lossy())
            .replace("CACHE_LSPONLY", &lsponly.to_string_lossy())
            .replace("CACHE_UNREADABLE", &unreadable.to_string_lossy())
            .replace("CACHE_FILED", &filed.to_string_lossy())
            .replace("CACHE_GHOSTED", &ghosted.join("missing-dir").to_string_lossy())
            .as_str(),
        );

        // Marketplace manifest: full is at 2.0.0 with an LSP server;
        // gone is available at 6.4.0 with a relative clone source;
        // lsponly declares only an LSP server, like the real -lsp
        // plugins do; fileclone's source is a file, and absentclone's
        // source directory is never created.
        write(
            &mkt.join(".claude-plugin/marketplace.json"),
            r#"{"name":"probe-market","plugins":[
                {"name":"full","version":"2.0.0","source":"./full-src",
                 "lspServers":{"rust-analyzer":{"command":"/opt/ra/current/bin/rust-analyzer"}}},
                {"name":"gone","version":"6.4.0","source":"./gone-src"},
                {"name":"lsponly","version":"1.0.0","source":"./lsponly-src",
                 "lspServers":{"gopls":{"command":"/opt/go/current/bin/gopls"}}},
                {"name":"fileclone","version":"1.0.0","source":"./file-source"},
                {"name":"absentclone","version":"1.0.0","source":"./absent-src"}
            ]}"#,
        );
        write(&mkt.join("file-source"), "not a directory");
        skill(&mkt.join("full-src/skills/clone-skill/SKILL.md"));
        skill(&mkt.join("gone-src/skills/writing-skills/SKILL.md"));
        skill(&mkt.join("gone-src/skills/not-a-dir.md"));
        write(&mkt.join("gone-src/commands/do.md"), "# do");

        // ghost: the cache-miss health case. Health matches clones to
        // registry entries by name rather than by installLocation, so
        // this row reads as missing even though its installLocation
        // points at the real probe-market clone.
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

    /// Buffer tracing output so an emitted record can be read back.
    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_logs(f: impl FnOnce()) -> String {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt().with_writer(capture.clone()).finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = capture.0.lock().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn by_plugin<'a>(rows: &'a [PluginComponents], id: &str) -> &'a PluginComponents {
        rows.iter().find(|row| row.plugin == id).unwrap_or_else(|| panic!("no row for {id}"))
    }

    fn scan_fixture(fixture: &Fixture) -> Vec<PluginComponents> {
        scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
            .components
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
            BTreeMap::from([(
                "rust-analyzer".to_owned(),
                "/opt/ra/current/bin/rust-analyzer".to_owned(),
            )])
        );
        assert!(full.installed);
        assert!(full.enabled);
        assert!(!full.auto);
        assert_eq!(full.version.as_deref(), Some("1.2.3"));
        assert_eq!(full.available_version.as_deref(), Some("2.0.0"));
        assert_eq!(full.marketplace, "probe-market");
    }

    /// A cache leftover without the manifest at the real path is not a
    /// plugin, whether it carries no manifest anywhere or one at the
    /// dir root, which is not the install layout.
    #[test]
    fn a_cache_leftover_without_the_real_manifest_is_not_a_plugin() {
        let fixture = fixture();
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        assert!(
            rows.iter().any(|row| row.plugin == "stray@probe-market"),
            "the cache-leftover branch must run for the exclusions below to mean anything: {rows:?}"
        );
        assert!(
            log.contains("carries no version dir with a manifest"),
            "a plugin dir that lists nothing says so rather than vanishing: {log}"
        );
        assert!(
            rows.iter().all(|row| !row.plugin.starts_with("broken")),
            "no row may exist for the manifest-less dir: {rows:?}"
        );
        assert!(
            rows.iter().all(|row| !row.plugin.starts_with("rootonly")),
            "the root-level plugin.json is not the install layout: {rows:?}"
        );
    }

    /// A registry entry whose install dir is gone must surface as a
    /// LoadFailed row, not vanish - the honest-scan contract.
    #[test]
    fn a_vanished_install_path_surfaces_as_a_load_failed_row() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;
        let ghosted = rows
            .iter()
            .find(|row| row.plugin == "ghosted@probe-market")
            .expect("the registered-but-vanished plugin must still have a row: {rows:?}");
        assert!(ghosted.installed);
        assert_eq!(
            ghosted.load_error.as_deref(),
            Some("registered install dir is missing on disk"),
            "the vanished dir is the row's stated reason: {ghosted:?}"
        );
        assert!(ghosted.skills.is_empty());
    }

    /// An install with no manifest of its own is a valid plugin
    /// (rust-analyzer-lsp and gopls-lsp on the real estate).
    #[test]
    fn a_registered_install_without_a_manifest_is_installed_not_failed() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;

        let lsponly = by_plugin(&rows, "lsponly@probe-market");
        assert!(
            lsponly.installed,
            "the registry branch must emit it, or it degrades to a catalog row: {lsponly:?}"
        );
        assert_eq!(
            lsponly.load_error, None,
            "no manifest of its own is not a load failure: {lsponly:?}"
        );
        assert_eq!(lsponly.skills, Vec::<String>::new(), "it ships no components");
        assert_eq!(
            lsponly.lsp_servers,
            BTreeMap::from([("gopls".to_owned(), "/opt/go/current/bin/gopls".to_owned())]),
            "its LSP servers come from the marketplace manifest: {lsponly:?}"
        );
    }

    /// A registered dir that exists but cannot be read is a load
    /// failure, not an empty healthy install.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_install_dir_surfaces_as_a_load_failed_row() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let dir = fixture.plugins_root.join("cache/probe-market/unreadable/1.0.0");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the install dir away");
        assert!(
            std::fs::read_dir(&dir).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );

        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore for tempdir cleanup");

        let unreadable = by_plugin(&rows, "unreadable@probe-market");
        assert_eq!(
            unreadable.load_error.as_deref(),
            Some("registered install dir cannot be read"),
            "the EACCES dir states the read failure, not the missing one: {unreadable:?}"
        );
        assert!(unreadable.skills.is_empty(), "nothing may be read out of it: {unreadable:?}");

        let dir_text = dir.to_string_lossy().into_owned();
        assert!(log.contains("unreadable@probe-market"), "the warn names the plugin: {log}");
        assert!(log.contains(&dir_text), "the warn carries the path: {log}");
        assert!(log.contains("Permission denied"), "the warn carries the io error: {log}");
    }

    /// A registry entry with no installPath never reaches the
    /// filesystem: it is an installed row carrying the missing-dir
    /// reason, with a warn of its own.
    #[test]
    fn a_registry_entry_without_an_install_path_surfaces_as_a_load_failed_row() {
        let fixture = fixture();
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));

        let nopath = by_plugin(&rows, "nopath@probe-market");
        assert!(nopath.installed, "a failed registry install is still an install: {nopath:?}");
        assert_eq!(
            nopath.load_error.as_deref(),
            Some("registered install dir is missing on disk"),
            "an absent installPath reads as no dir on disk: {nopath:?}"
        );
        assert!(nopath.skills.is_empty(), "nothing may be read out of it: {nopath:?}");
        assert!(log.contains("nopath@probe-market"), "the warn names the entry: {log}");
        assert!(
            log.contains("registered plugin entry carries no installPath"),
            "the warn says the entry records no path: {log}"
        );
    }

    /// An installPath that is a regular file, not a directory, carries
    /// the read-failure reason rather than the missing-dir one.
    #[test]
    fn an_install_path_pointing_at_a_file_surfaces_as_a_load_failed_row() {
        let fixture = fixture();
        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;

        let filed = by_plugin(&rows, "filed@probe-market");
        assert!(filed.installed, "a failed registry install is still an install: {filed:?}");
        assert_eq!(
            filed.load_error.as_deref(),
            Some("registered install dir cannot be read"),
            "an installPath that is a file states the read failure: {filed:?}"
        );
        assert!(filed.skills.is_empty(), "nothing may be read out of it: {filed:?}");
    }

    /// A cache leftover whose newest version dir cannot be read shows
    /// no inventory at all: falling through would present an older
    /// version's components as this plugin's own.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_newest_version_dir_stops_the_fall_through() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let newest = fixture.plugins_root.join("cache/probe-market/multi/2.0.0");

        // The readable scan is the control: the newest version's skill
        // is the inventory shown.
        let readable = scan_fixture(&fixture);
        let multi = by_plugin(&readable, "multi@probe-market");
        assert_eq!(multi.skills, vec!["new"], "the newest version wins: {multi:?}");

        std::fs::set_permissions(&newest, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the newest version away");
        assert!(
            std::fs::read_dir(&newest).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&newest, std::fs::Permissions::from_mode(0o755))
            .expect("restore for tempdir cleanup");

        assert!(
            rows.iter().all(|row| row.plugin != "multi@probe-market"),
            "no inventory is shown rather than the older version's: {rows:?}"
        );
        let version_text = newest.to_string_lossy().into_owned();
        assert!(
            log.contains("cache plugin version dir cannot be read"),
            "the warn says the version dir could not be read: {log}"
        );
        assert!(log.contains(&version_text), "the warn names the version dir: {log}");
        let plugin_dir_text =
            fixture.plugins_root.join("cache/probe-market/multi").to_string_lossy().into_owned();
        assert!(
            !log.lines().any(|line| line.contains("carries no version dir with a manifest")
                && line.contains(&plugin_dir_text)),
            "a read failure on this plugin is not reported as an absent version dir: {log}"
        );
    }

    /// A clone source that cannot be read only stays quiet when it is
    /// missing; any other io error warns with the plugin, the path and
    /// the error, and contributes no components.
    #[test]
    fn an_unreadable_clone_source_is_warned_and_contributes_nothing() {
        let fixture = fixture();
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));

        let fileclone = by_plugin(&rows, "fileclone@probe-market");
        assert!(!fileclone.installed, "it is a catalog entry: {fileclone:?}");
        assert!(fileclone.skills.is_empty(), "nothing may be read out of it: {fileclone:?}");

        // The manifest declares `./file-source`; the scan logs the path
        // as joined, without normalising it.
        let source_text = fixture
            .marketplaces_root
            .join("probe-market")
            .join("./file-source")
            .to_string_lossy()
            .into_owned();
        assert!(log.contains("fileclone@probe-market"), "the warn names the plugin: {log}");
        assert!(
            log.lines()
                .any(|line| line.contains("fileclone@probe-market") && line.contains(&source_text)),
            "the clone warn carries the path: {log}"
        );
        assert!(
            log.lines()
                .any(|line| line.contains("fileclone@probe-market")
                    && line.contains("Not a directory")),
            "the clone warn carries the io error: {log}"
        );
    }

    /// A clone source that is simply not there is the ordinary case: the
    /// row still lists as a catalog entry with nothing in it, and no log
    /// line is written for it.
    #[test]
    fn a_missing_clone_source_stays_quiet() {
        let fixture = fixture();
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));

        let absent = by_plugin(&rows, "absentclone@probe-market");
        assert!(!absent.installed, "it is a catalog entry: {absent:?}");
        assert!(absent.skills.is_empty(), "nothing may be read out of it: {absent:?}");
        assert!(
            !log.contains("absentclone"),
            "a missing clone source is not worth a log line: {log}"
        );
    }

    /// An unreadable registry warns and leaves catalog rows only; the
    /// page renders identically either way, so the warn is the only
    /// signal.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_registry_warns_and_leaves_only_catalog_rows() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let path = fixture.plugins_root.join("installed_plugins.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the registry away");
        assert!(
            std::fs::read_to_string(&path).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("restore for tempdir cleanup");

        assert!(
            rows.iter().all(|row| !row.installed),
            "no registry means no installed rows, only the catalog: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.plugin == "stray@probe-market"),
            "the catalog still renders, so the assertion above is not vacuous: {rows:?}"
        );
        assert!(
            log.contains("installed_plugins.json cannot be read"),
            "the warn says what happened: {log}"
        );
        assert!(
            log.contains(&path.to_string_lossy().into_owned()),
            "the warn carries the path: {log}"
        );
        assert!(log.contains("Permission denied"), "the warn carries the io error: {log}");
    }

    /// An unreadable marketplaces root warns, and the intact clones then
    /// read as cache-misses - the reason the warn exists, since the page
    /// would otherwise offer Repair against a healthy clone.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_marketplaces_root_warns_and_reads_as_a_cache_miss() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let root = &fixture.marketplaces_root;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the marketplaces root away");
        assert!(
            std::fs::read_dir(root).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );
        let mut scan = ExtensionScan::default();
        let log = capture_logs(|| {
            scan = scan_extensions(&fixture.plugins_root, root, &fixture.config_dir);
        });
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755))
            .expect("restore for tempdir cleanup");

        assert!(
            log.contains("marketplaces root cannot be read"),
            "the warn says what happened: {log}"
        );
        let root_text = root.to_string_lossy().into_owned();
        assert!(
            log.lines().any(|line| line.contains("marketplaces root cannot be read")
                && line.contains(&root_text)),
            "the root warn carries the path: {log}"
        );
        assert!(
            log.lines().any(|line| line.contains("marketplaces root cannot be read")
                && line.contains("Permission denied")),
            "the root warn carries the io error: {log}"
        );

        let probe = scan
            .marketplace_health
            .iter()
            .find(|row| row.name == "probe-market")
            .expect("probe row");
        assert_eq!(
            probe.load_error.as_deref(),
            Some("no marketplace.json found in the clone"),
            "the intact clone reads as a cache-miss, which is what the warn is for: {probe:?}"
        );
    }

    /// An unreadable plugin cache warns and its own rows do not
    /// appear; the registry rows survive as read failures, since their
    /// install paths sit under it.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_cache_warns_and_shows_no_cache_rows() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let cache = fixture.plugins_root.join("cache");

        // The readable scan is the control: the cache walk does emit
        // rows, which is what the assertion below denies.
        let readable = scan_fixture(&fixture);
        assert!(
            readable.iter().any(|row| row.plugin == "stray@probe-market"),
            "the cache walk emits rows while the cache is readable: {readable:?}"
        );

        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the cache away");
        assert!(
            std::fs::read_dir(&cache).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o755))
            .expect("restore for tempdir cleanup");

        assert!(
            rows.iter().all(|row| row.plugin != "stray@probe-market"),
            "no cache-leftover row is read without the cache: {rows:?}"
        );
        // full's own installPath is under the cache this test chmods,
        // so the registry loop still emits it, as a failure.
        let full = by_plugin(&rows, "full@probe-market");
        assert_eq!(
            full.load_error.as_deref(),
            Some("registered install dir cannot be read"),
            "the registry loop still runs with the cache unreadable: {full:?}"
        );
        let cache_text = cache.to_string_lossy().into_owned();
        assert!(log.contains("plugin cache cannot be read"), "the warn says what happened: {log}");
        assert!(
            log.lines()
                .any(|line| line.contains("plugin cache cannot be read")
                    && line.contains(&cache_text)),
            "the cache warn carries the path: {log}"
        );
        assert!(
            log.lines().any(|line| line.contains("plugin cache cannot be read")
                && line.contains("Permission denied")),
            "the cache warn carries the io error: {log}"
        );
    }

    /// A marketplace cache dir that cannot be read warns; the registry
    /// loop still resolves the install paths inside it, as load-failed
    /// rows, and only the leftover walk under it stops.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_marketplace_cache_dir_warns() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let dir = fixture.plugins_root.join("cache/probe-market");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the marketplace cache dir away");
        assert!(
            std::fs::read_dir(&dir).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore for tempdir cleanup");

        assert!(
            rows.iter().all(|row| row.plugin != "stray@probe-market"),
            "no leftover under it is read: {rows:?}"
        );
        let dir_text = dir.to_string_lossy().into_owned();
        assert!(
            log.contains("marketplace cache dir cannot be read"),
            "the unreadable marketplace dir is warned: {log}"
        );
        assert!(
            log.lines().any(|line| line.contains("marketplace cache dir cannot be read")
                && line.contains(&dir_text)),
            "the warn names that dir: {log}"
        );
    }

    /// A missing cache is the fresh install: no warn, and the manifest
    /// half of the scan still runs.
    #[test]
    fn a_missing_cache_is_silent() {
        let fixture = fixture();
        std::fs::remove_dir_all(fixture.plugins_root.join("cache")).expect("remove the cache");
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));

        assert!(
            !log.contains("plugin cache cannot be read"),
            "a fresh install is not worth a log line: {log}"
        );
        assert!(
            rows.iter().any(|row| row.plugin == "gone@probe-market"),
            "the manifest half still runs, so the silence is not vacuous: {rows:?}"
        );
    }

    /// A version dir whose manifest cannot be stat'd is a read failure,
    /// not an absent manifest: it must not fall through to an older
    /// version's inventory.
    #[test]
    #[cfg(unix)]
    fn a_version_dir_with_an_unreadable_manifest_dir_stops_the_fall_through() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let hidden = fixture.plugins_root.join("cache/probe-market/multi/2.0.0/.claude-plugin");
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the manifest dir away");
        assert!(
            std::fs::metadata(hidden.join("plugin.json")).is_err(),
            "this test needs a runner where mode 000 denies a stat; root bypasses the permission model"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o755))
            .expect("restore for tempdir cleanup");

        assert!(
            rows.iter().all(|row| row.plugin != "multi@probe-market"),
            "no inventory is shown rather than the older version's: {rows:?}"
        );
        assert!(
            log.contains("cache plugin version dir cannot be read"),
            "the read failure is warned rather than read as an absent manifest: {log}"
        );
    }

    /// A newest version dir carrying no manifest is not a version dir:
    /// the walk falls through to the one below it.
    #[test]
    fn a_manifest_less_newest_version_falls_through() {
        let fixture = fixture();
        let rows = scan_fixture(&fixture);
        let multi = by_plugin(&rows, "multi@probe-market");
        assert_eq!(
            multi.skills,
            vec!["new"],
            "manifest-less 3.0.0 is skipped for the 2.0.0 below it: {multi:?}"
        );
    }

    /// A cache plugin dir that cannot be read warns and shows no
    /// inventory for that plugin at all.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_cache_plugin_dir_warns_and_shows_no_row() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let plugin_dir = fixture.plugins_root.join("cache/probe-market/multi");
        std::fs::set_permissions(&plugin_dir, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the plugin dir away");
        assert!(
            std::fs::read_dir(&plugin_dir).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&plugin_dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore for tempdir cleanup");

        assert!(
            rows.iter().all(|row| row.plugin != "multi@probe-market"),
            "no inventory is shown when the plugin dir cannot be read: {rows:?}"
        );
        assert!(
            log.contains("cache plugin dir cannot be read"),
            "the warn says what happened: {log}"
        );
        assert!(
            log.contains(&plugin_dir.to_string_lossy().into_owned()),
            "the warn names the plugin dir: {log}"
        );
        assert!(log.contains("Permission denied"), "the warn carries the io error: {log}");
    }

    /// A missing registry is the fresh install: no warn at all, and the
    /// catalog still renders.
    #[test]
    fn a_missing_registry_is_a_silent_fresh_install() {
        let fixture = fixture();
        std::fs::remove_file(fixture.plugins_root.join("installed_plugins.json"))
            .expect("remove the registry");
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));

        assert!(
            rows.iter().all(|row| !row.installed),
            "no registry means no installed rows: {rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.plugin == "stray@probe-market"),
            "the catalog still renders, so the assertion above is not vacuous: {rows:?}"
        );
        assert!(
            !log.contains("installed_plugins.json"),
            "a fresh install is not worth a log line: {log}"
        );
    }

    /// A registry that does not parse warns with its path, and the
    /// manifest half of the scan is unaffected.
    #[test]
    fn an_unparseable_registry_warns_and_leaves_the_scan_intact() {
        let fixture = fixture();
        let path = fixture.plugins_root.join("installed_plugins.json");
        std::fs::write(&path, "{ torn").expect("break the registry");
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));

        assert!(
            log.contains("installed_plugins.json exists but does not parse"),
            "the warn says what happened: {log}"
        );
        assert!(
            log.contains(&path.to_string_lossy().into_owned()),
            "the warn carries the path: {log}"
        );
        assert!(
            rows.iter().any(|row| row.plugin == "full@probe-market"),
            "the manifest half of the scan still runs: {rows:?}"
        );
    }

    /// A missing marketplaces root is the fresh-install case too: silent,
    /// with the configured marketplaces still listed as cache-misses.
    #[test]
    fn a_missing_marketplaces_root_is_silent() {
        let fixture = fixture();
        std::fs::remove_dir_all(&fixture.marketplaces_root).expect("remove the clones root");
        let mut scan = ExtensionScan::default();
        let log = capture_logs(|| {
            scan = scan_extensions(
                &fixture.plugins_root,
                &fixture.marketplaces_root,
                &fixture.config_dir,
            );
        });

        assert!(
            !log.contains("marketplaces root"),
            "a fresh install is not worth a log line: {log}"
        );
        let probe = scan
            .marketplace_health
            .iter()
            .find(|row| row.name == "probe-market")
            .expect("the configured marketplace still lists");
        assert_eq!(
            probe.load_error.as_deref(),
            Some("no marketplace.json found in the clone"),
            "with no root every clone reads as a cache-miss: {probe:?}"
        );
    }

    /// Numeric-aware version ordering: "10.0.0" outranks "9.9.9" when
    /// a cache plugin has both dirs; a plain string sort would pick
    /// "9.9.9".
    #[test]
    fn a_two_digit_version_dir_outranks_a_nine() {
        let fixture = fixture();
        // An unregistered cache plugin (no registry entry, no manifest
        // entry) so the cache-leftover branch resolves it.
        let orphan_cache = fixture.plugins_root.join("cache/probe-market/orphan");
        let old_dir = orphan_cache.join("9.9.9");
        let new_dir = orphan_cache.join("10.0.0");
        skill(&old_dir.join("skills/nine/SKILL.md"));
        write(
            &old_dir.join(".claude-plugin/plugin.json"),
            r#"{"name":"orphan","version":"9.9.9"}"#,
        );
        skill(&new_dir.join("skills/ten/SKILL.md"));
        write(
            &new_dir.join(".claude-plugin/plugin.json"),
            r#"{"name":"orphan","version":"10.0.0"}"#,
        );

        let rows =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir)
                .components;
        let orphan =
            rows.iter().find(|row| row.plugin == "orphan@probe-market").expect("orphan row");
        assert_eq!(
            orphan.skills,
            vec!["ten"],
            "the newest (10.0.0) dir's components win over 9.9.9: {orphan:?}"
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
        assert_eq!(probe.available, 5);
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

    /// A manifest that EXISTS with a syntax error must read as a
    /// parse failure, never as the cache-miss reason that would send
    /// the user to Repair against a healthy clone.
    #[test]
    fn a_syntactically_broken_manifest_reads_as_a_parse_failure() {
        let fixture = fixture();
        let manifest =
            fixture.marketplaces_root.join("probe-market/.claude-plugin/marketplace.json");
        std::fs::write(&manifest, "{ not json").expect("break manifest");
        let scan =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir);
        let probe = scan
            .marketplace_health
            .iter()
            .find(|row| row.name == "probe-market")
            .expect("probe row");
        let error = probe.load_error.as_deref().expect("the parse error surfaces");
        assert!(
            error.contains("failed to parse"),
            "a broken manifest names the parse failure: {error}"
        );
        assert!(
            !error.contains("no marketplace.json found"),
            "a present manifest must not read as a cache-miss: {error}"
        );

        // The same failure reaches the Installed tab: a marketplace
        // that cannot load renders there as a load-failed row.
        let marketplace_row = scan
            .components
            .iter()
            .find(|row| row.plugin == "probe-market")
            .expect("the failed marketplace's row is keyed by its name");
        assert!(
            !marketplace_row.installed,
            "a marketplace that cannot load is not an install: {marketplace_row:?}"
        );
        assert!(
            marketplace_row.load_error.as_deref().unwrap_or_default().contains("failed to parse"),
            "the row names the parse failure: {marketplace_row:?}"
        );
    }

    /// The official marketplace's manifest declares per-plugin sources
    /// as OBJECTS (`url`, `git-subdir` remotes), not just relative
    /// strings. A string-typed source field fails the whole parse and
    /// the largest marketplace's catalog disappears behind one error.
    /// The fixture mirrors the real manifest's shapes.
    #[test]
    fn object_shaped_plugin_sources_parse_and_stay_available() {
        let fixture = fixture();
        write(
            &fixture.marketplaces_root.join("official/.claude-plugin/marketplace.json"),
            r#"{
              "$schema": "https://anthropic.com/claude-code/marketplace.schema.json",
              "name": "official",
              "owner": {"name": "Anthropic"},
              "renames": {"old-name": "renamed-plugin"},
              "plugins": [
                {
                  "name": "subdir-plugin",
                  "source": {
                    "source": "git-subdir",
                    "url": "https://github.com/example/plugins",
                    "path": "plugins/subdir-plugin",
                    "ref": "v1.5.5"
                  }
                },
                {
                  "name": "url-plugin",
                  "source": {
                    "source": "url",
                    "url": "https://github.com/example/single-plugin"
                  }
                },
                {"name": "relative-plugin", "source": "./plugins/relative-plugin"},
                {"name": "bare-string-plugin", "source": "plugins/bare-string"},
                {"name": "renamed-plugin", "version": "2.0.0"}
              ]
            }"#,
        );
        skill(
            &fixture.marketplaces_root.join("official/plugins/relative-plugin/skills/one/SKILL.md"),
        );
        // A string source WITHOUT the ./ prefix is not clone-relative:
        // this dir exists, and the guard is what keeps it unscanned.
        skill(&fixture.marketplaces_root.join("official/plugins/bare-string/skills/leak/SKILL.md"));
        write(
            &fixture.plugins_root.join("known_marketplaces.json"),
            format!(
                r#"{{"official":{{"source":{{"source":"github","repo":"anthropics/claude-plugins-official"}},
                     "installLocation":{:?},"lastUpdated":""}}}}"#,
                fixture.marketplaces_root.join("official").to_string_lossy()
            )
            .as_str(),
        );

        let scan =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir);

        let health =
            scan.marketplace_health.iter().find(|row| row.name == "official").expect("health row");
        assert_eq!(health.available, 5, "every manifest plugin counts: {health:?}");
        assert_eq!(
            health.load_error, None,
            "object sources must not fail the manifest parse: {health:?}"
        );

        let subdir = scan
            .components
            .iter()
            .find(|row| row.plugin == "subdir-plugin@official")
            .expect("the git-subdir plugin lists");
        assert!(!subdir.installed);
        assert_eq!(
            subdir.skills,
            Vec::<String>::new(),
            "a remote source carries no local components: {subdir:?}"
        );

        let relative = scan
            .components
            .iter()
            .find(|row| row.plugin == "relative-plugin@official")
            .expect("the relative plugin lists");
        assert_eq!(
            relative.skills,
            vec!["one"],
            "a relative source still resolves inside the clone: {relative:?}"
        );

        let bare = scan
            .components
            .iter()
            .find(|row| row.plugin == "bare-string-plugin@official")
            .expect("the bare-string plugin lists");
        assert_eq!(
            bare.skills,
            Vec::<String>::new(),
            "a string source without ./ is not clone-relative - the guard keeps its dir unscanned: {bare:?}"
        );
    }

    /// A corrupt known_marketplaces.json must not render the tab as an
    /// absence: each clone's health row carries the load error.
    #[test]
    fn a_corrupt_marketplace_registry_names_the_parse_error_on_the_clone_rows() {
        let fixture = fixture();
        write(&fixture.plugins_root.join("known_marketplaces.json"), "{ torn write");
        let scan =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir);
        let probe =
            scan.marketplace_health.iter().find(|row| row.name == "probe-market").expect("row");
        let error = probe.load_error.as_deref().expect("the registry error surfaces");
        assert!(
            error.contains("known_marketplaces.json failed to parse"),
            "the registry error, not a cache-miss: {error}"
        );
        assert!(
            !scan.marketplace_sources.is_empty(),
            "the configured sources still list by clone dir: {:?}",
            scan.marketplace_sources
        );
    }

    /// An UNREADABLE registry is the sibling class: it warns and the
    /// clone rows carry the error, exactly like the parse class.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_marketplace_registry_also_carries_the_error() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let path = fixture.plugins_root.join("known_marketplaces.json");
        std::fs::write(&path, "{ fine").expect("write registry");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("chmod away");
        let scan =
            scan_extensions(&fixture.plugins_root, &fixture.marketplaces_root, &fixture.config_dir);
        let probe =
            scan.marketplace_health.iter().find(|row| row.name == "probe-market").expect("row");
        let error = probe.load_error.as_deref().expect("the read error surfaces");
        assert!(
            error.contains("known_marketplaces.json cannot be read"),
            "the read class, not absence and not parse: {error}"
        );
        assert!(
            !scan.marketplace_sources.is_empty(),
            "the clone rows still list: {:?}",
            scan.marketplace_sources
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("restore for tempdir cleanup");
    }

    /// A manifest that exists but cannot be READ is its own class -
    /// never "failed to parse", which would misdiagnose healthy JSON.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_manifest_names_the_read_class() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let manifest =
            fixture.marketplaces_root.join("probe-market/.claude-plugin/marketplace.json");
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o000))
            .expect("chmod away");
        let mut scan = ExtensionScan::default();
        let log = capture_logs(|| {
            scan = scan_extensions(
                &fixture.plugins_root,
                &fixture.marketplaces_root,
                &fixture.config_dir,
            );
        });
        std::fs::set_permissions(&manifest, std::fs::Permissions::from_mode(0o644))
            .expect("restore for tempdir cleanup");
        let probe = scan
            .marketplace_health
            .iter()
            .find(|row| row.name == "probe-market")
            .expect("probe row");
        let error = probe.load_error.as_deref().expect("the read error surfaces");
        assert!(
            error.contains("cannot be read") && !error.contains("failed to parse"),
            "the read class is named, not a parse diagnosis: {error}"
        );

        let manifest_text = manifest.to_string_lossy().into_owned();
        assert!(
            log.lines().any(|line| line.contains("marketplace manifest cannot be read")
                && line.contains(&manifest_text)),
            "the manifest warn carries the path and the read class: {log}"
        );
    }

    /// A corrupt settings.json must not silently flip every disabled
    /// plugin to enabled; the scan still completes and warns.
    #[test]
    fn a_corrupt_settings_file_reads_all_enabled_with_the_scan_intact() {
        let fixture = fixture();
        let path = fixture.config_dir.join("settings.json");
        write(&path, "{ torn");
        let mut scan = ExtensionScan::default();
        let log = capture_logs(|| {
            scan = scan_extensions(
                &fixture.plugins_root,
                &fixture.marketplaces_root,
                &fixture.config_dir,
            );
        });
        let off =
            scan.components.iter().find(|row| row.plugin == "off@probe-market").expect("off row");
        assert!(off.installed);
        assert!(off.enabled, "absent from the map reads enabled; the corrupt file warned");
        assert!(off.auto, "the auto marker comes from the intact registry, not settings");
        assert!(log.contains("does not parse"), "the parse failure is warned: {log}");
        assert!(
            log.contains(&path.to_string_lossy().into_owned()),
            "the warn names the settings path: {log}"
        );
    }

    /// The read sibling: an unreadable settings.json warns with the read
    /// class and reads as all-enabled.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_settings_file_warns_and_reads_all_enabled() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let path = fixture.config_dir.join("settings.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the settings file away");
        assert!(
            std::fs::read_to_string(&path).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );
        let mut scan = ExtensionScan::default();
        let log = capture_logs(|| {
            scan = scan_extensions(
                &fixture.plugins_root,
                &fixture.marketplaces_root,
                &fixture.config_dir,
            );
        });
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("restore for tempdir cleanup");

        let off =
            scan.components.iter().find(|row| row.plugin == "off@probe-market").expect("off row");
        assert!(off.enabled, "an unreadable file reads as all-enabled: {off:?}");
        assert!(
            log.contains("file exists but cannot be read"),
            "the read arm's own phrase is used: {log}"
        );
        assert!(
            !log.contains("does not parse"),
            "a read failure is not reported as a parse failure: {log}"
        );
        assert!(
            log.contains(&path.to_string_lossy().into_owned()),
            "the warn names the settings path: {log}"
        );
    }

    /// No settings file is the fresh install: all-enabled, nothing logged.
    #[test]
    fn a_missing_settings_file_is_silent() {
        let fixture = fixture();
        std::fs::remove_file(fixture.config_dir.join("settings.json")).expect("remove settings");
        let mut scan = ExtensionScan::default();
        let log = capture_logs(|| {
            scan = scan_extensions(
                &fixture.plugins_root,
                &fixture.marketplaces_root,
                &fixture.config_dir,
            );
        });

        assert!(!log.contains("settings.json"), "a fresh install is not worth a log line: {log}");
        let off =
            scan.components.iter().find(|row| row.plugin == "off@probe-market").expect("off row");
        assert!(off.enabled, "no settings means enabled: {off:?}");
    }

    /// A hooks.json that parses but names no hooks object warns for
    /// itself, and costs the plugin only its triggers.
    #[test]
    fn a_hooks_file_without_a_hooks_object_warns() {
        let fixture = fixture();
        let hooks = fixture.plugins_root.join("cache/probe-market/full/1.2.3/hooks/hooks.json");
        write(&hooks, r#"{"hooks": []}"#);
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));

        let full = by_plugin(&rows, "full@probe-market");
        assert!(full.skills.contains(&"a".to_owned()), "its other components still list: {full:?}");
        assert!(full.hooks.is_empty(), "no triggers are shown: {full:?}");
        assert!(
            log.contains("names no hooks object"),
            "the absent hooks object is warned, not swallowed: {log}"
        );
    }

    /// A hooks.json that does not parse warns as a parse failure, and
    /// costs the plugin only its triggers.
    #[test]
    fn an_unparseable_hooks_file_names_the_parse_class() {
        let fixture = fixture();
        let hooks = fixture.plugins_root.join("cache/probe-market/full/1.2.3/hooks/hooks.json");
        write(&hooks, "{ torn");
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));

        let full = by_plugin(&rows, "full@probe-market");
        assert!(full.skills.contains(&"a".to_owned()), "its other components still list: {full:?}");
        assert!(full.hooks.is_empty(), "no triggers are shown: {full:?}");
        assert!(log.contains("does not parse"), "the parse class is named: {log}");
        assert!(
            log.contains(&hooks.to_string_lossy().into_owned()),
            "the warn names the hooks path: {log}"
        );
    }

    /// An unreadable hooks.json names the read class, never the parse
    /// one, and costs the plugin only its triggers.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_hooks_file_names_the_read_class() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let hooks = fixture.plugins_root.join("cache/probe-market/full/1.2.3/hooks/hooks.json");
        std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the hooks file away");
        assert!(
            std::fs::read_to_string(&hooks).is_err(),
            "this test needs a runner where mode 000 denies reads; root bypasses the permission model"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o644))
            .expect("restore for tempdir cleanup");

        let full = by_plugin(&rows, "full@probe-market");
        assert!(full.skills.contains(&"a".to_owned()), "its other components still list: {full:?}");
        assert!(full.hooks.is_empty(), "no triggers are shown: {full:?}");
        assert!(
            log.contains("file exists but cannot be read"),
            "the read arm's own phrase is used: {log}"
        );
        assert!(
            log.contains(&hooks.to_string_lossy().into_owned()),
            "the warn names the hooks path, not another file's: {log}"
        );
        assert!(
            !log.contains("does not parse"),
            "a read failure is not reported as a parse failure: {log}"
        );
    }

    /// A hooks DIR that cannot be stat'd costs the plugin its triggers,
    /// not its row: failing the row would hide its other components.
    #[test]
    #[cfg(unix)]
    fn an_unstatable_hooks_dir_keeps_the_plugin_row() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = fixture();
        let dir = fixture.plugins_root.join("cache/probe-market/full/1.2.3/hooks");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000))
            .expect("chmod the hooks dir away");
        assert!(
            std::fs::metadata(dir.join("hooks.json")).is_err(),
            "this test needs a runner where mode 000 denies a stat; root bypasses the permission model"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("restore for tempdir cleanup");

        let full = by_plugin(&rows, "full@probe-market");
        assert!(full.load_error.is_none(), "the plugin is not failed for its hooks: {full:?}");
        assert!(full.skills.contains(&"a".to_owned()), "its other components still list: {full:?}");
        assert!(full.hooks.is_empty(), "no triggers are shown: {full:?}");
        assert!(log.contains("hooks.json cannot be read"), "the stat failure is warned: {log}");
    }

    /// A .mcp.json that cannot be stat'd costs the plugin its MCP flag,
    /// not its row. It sits directly in the plugin dir, so the only way
    /// to deny a stat without denying the dir is a symlink loop.
    #[test]
    #[cfg(unix)]
    fn an_unstatable_mcp_file_keeps_the_plugin_row() {
        let fixture = fixture();
        let mcp = fixture.plugins_root.join("cache/probe-market/full/1.2.3/.mcp.json");
        std::fs::remove_file(&mcp).expect("remove the fixture MCP file");
        std::os::unix::fs::symlink(&mcp, &mcp).expect("self-referential symlink");
        assert!(
            std::fs::metadata(&mcp).is_err(),
            "this test needs a symlink loop, which fails a stat rather than denying a read"
        );
        let mut rows = Vec::new();
        let log = capture_logs(|| rows = scan_fixture(&fixture));
        std::fs::remove_file(&mcp).expect("restore for tempdir cleanup");

        let full = by_plugin(&rows, "full@probe-market");
        assert!(full.load_error.is_none(), "the plugin is not failed for its MCP file: {full:?}");
        assert!(!full.mcp, "no MCP flag is shown: {full:?}");
        assert!(full.skills.contains(&"a".to_owned()), "its other components still list: {full:?}");
        assert!(
            log.contains("the plugin's MCP config cannot be read"),
            "the stat failure is warned: {log}"
        );
        assert!(
            log.contains(&mcp.to_string_lossy().into_owned()),
            "the warn names the MCP path: {log}"
        );
    }
}
