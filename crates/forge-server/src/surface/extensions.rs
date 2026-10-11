//! `extensions()`: the Extensions page's read.
//!
//! One disk scan per read (registry, marketplace manifests, the plugin
//! cache) plus the two things the scan cannot carry: the LSP servers'
//! PATH checks, which are facts about this machine, and the last update
//! run, which is workspace state so a page attaching mid-run draws it.

use forge_primitives::plugins::{PluginUpdateRun, PluginsInventorySnapshot};

use super::ViewSurface;

/// One declared LSP server and whether its binary resolves on PATH.
pub struct LspServerCheck {
    /// The plugin whose manifest declares the server (`name@marketplace`).
    pub plugin: String,
    pub server: String,
    /// The command the manifest declares; the check tests this, not the
    /// server key.
    pub command: String,
    pub on_path: bool,
}

/// The Extensions page's read.
pub struct ExtensionsView {
    /// `None` when no plugins root resolves (`CLAUDE_CONFIG_DIR` or
    /// `HOME`): an empty scan must not read as a clean install.
    pub snapshot: Option<PluginsInventorySnapshot>,
    /// The last update run or check, in flight or finished.
    pub update_run: Option<PluginUpdateRun>,
    pub lsp: Vec<LspServerCheck>,
}

impl ViewSurface {
    pub async fn extensions(&self) -> ExtensionsView {
        let snapshot = self.workspace.plugins_inventory().await;
        let path = std::env::var_os("PATH");
        let path = path.as_deref().map(|path| path.to_string_lossy().into_owned());
        let lsp = snapshot
            .as_ref()
            .map(|snapshot| lsp_checks(snapshot, path.as_deref()))
            .unwrap_or_default();
        ExtensionsView { snapshot, update_run: self.workspace.plugin_update_run(), lsp }
    }
}

/// One check per declared LSP server of an INSTALLED plugin, in scan
/// order. An uninstalled plugin's declared servers are not probed: the
/// page's LSP pane draws installed rows (the plugin is the install unit),
/// so a check for a plugin that is not here is weight nothing draws.
pub(crate) fn lsp_checks(
    snapshot: &PluginsInventorySnapshot,
    path_var: Option<&str>,
) -> Vec<LspServerCheck> {
    let mut checks = Vec::new();
    for components in snapshot.components.iter().filter(|entry| entry.installed) {
        for (server, command) in &components.lsp_servers {
            let probed = if command.is_empty() { server.as_str() } else { command.as_str() };
            checks.push(LspServerCheck {
                plugin: components.plugin.clone(),
                server: server.clone(),
                command: command.clone(),
                on_path: forge_workspace::env::lsp_path::lsp_binary_on_path(probed, path_var),
            });
        }
    }
    checks
}

#[cfg(test)]
mod tests {
    use forge_primitives::plugins::{PluginComponents, PluginsInventorySnapshot};

    use super::*;

    fn snapshot_with_lsp(servers: &[(&str, &str)]) -> PluginsInventorySnapshot {
        let mut components = PluginComponents {
            plugin: "lsp-plugin@probe-market".to_owned(),
            installed: true,
            enabled: true,
            ..PluginComponents::default()
        };
        for (server, command) in servers {
            components.lsp_servers.insert((*server).to_owned(), (*command).to_owned());
        }
        PluginsInventorySnapshot {
            components: vec![components],
            ..PluginsInventorySnapshot::default()
        }
    }

    /// Catches a read that reports a server as available without ever
    /// asking the machine: the row would draw `on PATH` for everything.
    #[test]
    fn the_check_reflects_the_injected_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("probe-ls");
        std::fs::write(&binary, b"#!/bin/sh").expect("write binary");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = dir.path().to_string_lossy().into_owned();

        let snapshot = snapshot_with_lsp(&[("probe-server", "probe-ls"), ("absent", "absent-bin")]);
        let checks = lsp_checks(&snapshot, Some(&path));

        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].server, "absent");
        assert!(!checks[0].on_path, "a binary not on the injected PATH is reported missing");
        assert_eq!(checks[1].server, "probe-server");
        assert_eq!(checks[1].command, "probe-ls");
        assert!(checks[1].on_path, "the declared command is what was probed");

        assert!(
            lsp_checks(&snapshot, None).iter().all(|check| !check.on_path),
            "no PATH at all reports every server missing rather than guessing"
        );
    }
}
