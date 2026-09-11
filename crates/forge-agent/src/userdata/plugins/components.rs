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
    let hooks = read_json(&dir.join("hooks/hooks.json"))
        .and_then(|doc| doc.get("hooks").and_then(|hooks| hooks.as_object().cloned()))
        .map(|hooks| hooks.keys().cloned().collect())
        .unwrap_or_default();
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
/// (marketplace name, plugin name).
struct Manifests {
    entries: BTreeMap<(String, String), ManifestPlugin>,
}

impl Manifests {
    fn load(marketplaces_root: &Path) -> Self {
        let mut entries = BTreeMap::new();
        let Ok(marketplaces) = std::fs::read_dir(marketplaces_root) else {
            return Self { entries };
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
                let Ok(manifest) = serde_json::from_value::<MarketplaceManifest>(doc) else {
                    break;
                };
                for plugin in manifest.plugins {
                    entries.insert((name.clone(), plugin.name.clone()), plugin);
                }
                break;
            }
        }
        Self { entries }
    }

    fn plugin(&self, marketplace: &str, name: &str) -> Option<&ManifestPlugin> {
        self.entries.get(&(marketplace.to_owned(), name.to_owned()))
    }
}

/// Scan the plugin cache and marketplace manifests into per-plugin
/// component inventories, one entry per plugin the registry or a
/// manifest knows about. Installed plugins scan their registry
/// `installPath`; uninstalled ones their marketplace clone source.
pub fn scan_components(plugins_root: &Path, marketplaces_root: &Path) -> Vec<PluginComponents> {
    let registry = read_json(&plugins_root.join("installed_plugins.json"))
        .and_then(|doc| serde_json::from_value::<PluginRegistry>(doc).ok())
        .unwrap_or(PluginRegistry { plugins: BTreeMap::new() });
    let enabled = enabled_plugins(plugins_root);
    let manifests = Manifests::load(marketplaces_root);
    let mut rows = BTreeMap::<String, PluginComponents>::new();

    // Registry installs are authoritative: the row scans the exact
    // installPath the CLI recorded.
    for (id, entries) in &registry.plugins {
        let Some(entry) = entries.first() else { continue };
        let marketplace = id.split_once('@').map_or("", |(_, marketplace)| marketplace);
        let components = entry
            .install_path
            .as_deref()
            .and_then(|path| scan_component_dir(Path::new(path), true));
        let Some(components) = components else { continue };
        rows.insert(
            id.clone(),
            PluginComponents {
                plugin: id.clone(),
                marketplace: marketplace.to_owned(),
                version: entry.version.clone(),
                installed: true,
                enabled: enabled.is_enabled(id),
                auto: entries.iter().any(|entry| entry.auto == Some(true)),
                available_version: manifests
                    .plugin(marketplace, plugin_name(id))
                    .and_then(|plugin| plugin.version.clone()),
                skills: components.skills,
                agents: components.agents,
                commands: components.commands,
                hooks: components.hooks,
                mcp: components.mcp,
                lsp_servers: manifest_lsp_servers(manifests.plugin(marketplace, plugin_name(id))),
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

    // Cache leftovers the registry and manifests both miss: a real
    // plugin dir with no registry entry. The newest version dir wins.
    let cache = plugins_root.join("cache");
    let Ok(marketplace_dirs) = std::fs::read_dir(&cache) else {
        return rows.into_values().collect();
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

    rows.into_values().collect()
}

fn plugin_name(id: &str) -> &str {
    id.split_once('@').map_or(id, |(name, _)| name)
}

fn manifest_lsp_servers(plugin: Option<&ManifestPlugin>) -> Vec<String> {
    plugin
        .and_then(|plugin| plugin.lsp_servers.as_ref())
        .map(|servers| servers.keys().cloned().collect())
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
    versions.sort();
    versions.reverse();
    versions.into_iter().find_map(|version| {
        scan_component_dir(&version, true).map(|components| (version, components))
    })
}

/// Scan the marketplace clones plus the CLI's registry
/// (`known_marketplaces.json` beside the clones) into per-marketplace
/// health. `drifted` flags a registry `installLocation` outside
/// `config_dir`; `load_error` a manifest that cannot be read.
pub fn scan_marketplaces(marketplaces_root: &Path, config_dir: &Path) -> Vec<MarketplaceHealth> {
    let registry_path =
        marketplaces_root.parent().map(|plugins_root| plugins_root.join("known_marketplaces.json"));
    let registry = registry_path
        .and_then(|path| read_json(&path))
        .and_then(|doc| serde_json::from_value::<KnownMarketplaces>(doc).ok())
        .unwrap_or(KnownMarketplaces { entries: BTreeMap::new() });

    registry
        .entries
        .into_iter()
        .map(|(name, entry)| {
            let install_location = entry.install_location.map(PathBuf::from).unwrap_or_default();
            let drifted = !install_location.as_os_str().is_empty()
                && !install_location.starts_with(config_dir);
            let manifest = manifest_path(&marketplaces_root.join(&name));
            let (available, load_error) = match manifest
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map(|contents| serde_json::from_str::<MarketplaceManifest>(&contents))
            {
                Some(Ok(manifest)) => (manifest.plugins.len(), None),
                Some(Err(error)) => (0, Some(format!("marketplace.json parse failed: {error}"))),
                None => (
                    0,
                    Some(if install_location.as_os_str().is_empty() {
                        "no marketplace clone on disk".to_owned()
                    } else {
                        "no marketplace.json found in the clone".to_owned()
                    }),
                ),
            };
            MarketplaceHealth {
                name,
                source: entry
                    .source
                    .as_ref()
                    .and_then(|source| source.get("source"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                available,
                load_error,
                install_location,
                drifted,
            }
        })
        .collect()
}

fn manifest_path(marketplace_dir: &Path) -> Option<PathBuf> {
    [
        marketplace_dir.join("marketplace.json"),
        marketplace_dir.join(".claude-plugin/marketplace.json"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
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
        let rows = scan_components(&fixture.plugins_root, &fixture.marketplaces_root);

        let full = by_plugin(&rows, "full@probe-market");
        assert_eq!(full.skills, vec!["a", "b"]);
        assert_eq!(full.agents, vec!["x", "y"]);
        assert_eq!(full.commands, vec!["c1", "c2"]);
        assert_eq!(full.hooks, vec!["SessionStart", "PreToolUse"]);
        assert!(full.mcp, ".mcp.json present");
        assert_eq!(full.lsp_servers, vec!["rust-analyzer"]);
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
        let rows = scan_components(&fixture.plugins_root, &fixture.marketplaces_root);
        assert!(
            rows.iter().all(|row| !row.plugin.starts_with("broken")),
            "no row may exist for the plugin.json-less dir: {rows:?}"
        );
    }

    #[test]
    fn a_skill_file_is_not_a_skill() {
        let fixture = fixture();
        let rows = scan_components(&fixture.plugins_root, &fixture.marketplaces_root);
        let gone = by_plugin(&rows, "gone@probe-market");
        assert_eq!(gone.skills, vec!["writing-skills"], "file-not-dir excluded; clone scanned");
    }

    #[test]
    fn an_uninstalled_plugin_scans_from_its_marketplace_clone() {
        let fixture = fixture();
        let rows = scan_components(&fixture.plugins_root, &fixture.marketplaces_root);
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
        let rows = scan_components(&fixture.plugins_root, &fixture.marketplaces_root);
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
        let rows = scan_components(&fixture.plugins_root, &fixture.marketplaces_root);
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
        let rows = scan_marketplaces(&fixture.marketplaces_root, &fixture.config_dir);
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
        let rows = scan_marketplaces(&fixture.marketplaces_root, &foreign);
        let probe = rows.iter().find(|row| row.name == "probe-market").expect("probe row");
        assert!(probe.drifted, "{probe:?}");
    }

    #[test]
    fn a_marketplace_missing_its_manifest_is_a_load_error() {
        let fixture = fixture();
        let rows = scan_marketplaces(&fixture.marketplaces_root, &fixture.config_dir);
        let ghost = rows.iter().find(|row| row.name == "ghost").expect("ghost row");
        assert_eq!(ghost.available, 0);
        assert!(ghost.load_error.is_some(), "cache-miss reads as a load error: {ghost:?}");
    }
}
