//! Full-frame render snapshots: every Extensions tab at a realistic
//! 160x40 geometry, pinned as exact text. These are the acceptance
//! artifacts for the page's grammar - tabs and counts, the installed
//! stream first, the Available toggle, the aligned row columns.

use crate::app::App;
use crate::app::extensions::{
    ExtensionKind, ExtensionRow, ExtensionsTab, MarketplaceHealth, MarketplaceSourceEntry, RowState,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// The page's fixture: a small estate that exercises every row state -
/// current, update-available, auto-installed, disabled, load-failed,
/// LSP with its binary check, and the available catalog behind the
/// toggle - at mock-like ratios.
pub(crate) fn snapshot_app() -> App {
    use forge_primitives::plugins::{InstalledPluginEntry, PluginCapability};

    let mut app = App::test_default();
    app.active_view = crate::app::ActiveView::Extensions;
    app.install_testing_stub();
    app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));

    let entry = |id: &str, version: &str| InstalledPluginEntry {
        id: id.to_owned(),
        version: Some(version.to_owned()),
        scope: "user".to_owned(),
        enabled: true,
        installed_at: None,
        last_updated: None,
        project_path: None,
        capability: PluginCapability::Skill,
    };
    app.plugins.installed = vec![
        entry("superpowers@superpowers-market", "6.3.0"),
        entry("rust-review@code-review-market", "1.1.0"),
        entry("leyline@claude-night-market", "0.1.0"),
        entry("off-plugin@probe-market", "1.0.0"),
        entry("broken-ghost@ghost-market", "2.0.0"),
        entry("lsp-support@probe-market", "1.0.0"),
    ];

    let plugin = |id: &str,
                  market: &str,
                  version: Option<&str>,
                  available: Option<&str>,
                  state: RowState,
                  detail: Option<&str>| ExtensionRow {
        id: id.to_owned(),
        kind: ExtensionKind::Plugin,
        name: id.split_once('@').map_or(id, |(name, _)| name).to_owned(),
        source: market.to_owned(),
        version: version.map(str::to_owned),
        available_version: available.map(str::to_owned),
        state,
        detail: detail.map(str::to_owned),
    };
    let component = |kind: ExtensionKind,
                     plugin: &str,
                     market: &str,
                     name: &str,
                     version: Option<&str>,
                     available: Option<&str>,
                     state: RowState| ExtensionRow {
        id: format!(
            "{}:{plugin}:{name}",
            match kind {
                ExtensionKind::Skill => "skill",
                ExtensionKind::Agent => "agent",
                ExtensionKind::Command => "command",
                ExtensionKind::Lsp => "lsp",
                _ => "component",
            }
        ),
        kind,
        name: name.to_owned(),
        source: format!("{plugin}@{market}"),
        version: version.map(str::to_owned),
        available_version: available.map(str::to_owned),
        state,
        detail: None,
    };

    app.plugins.installed_rows = vec![
        plugin(
            "superpowers@superpowers-market",
            "superpowers-market",
            Some("6.3.0"),
            None,
            RowState::Current,
            Some("~450 tok always-on"),
        ),
        component(
            ExtensionKind::Skill,
            "superpowers",
            "superpowers-market",
            "brainstorming",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Skill,
            "superpowers",
            "superpowers-market",
            "executing-plans",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Skill,
            "superpowers",
            "superpowers-market",
            "systematic-debugging",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Skill,
            "superpowers",
            "superpowers-market",
            "test-driven-development",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Skill,
            "superpowers",
            "superpowers-market",
            "using-superpowers",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Skill,
            "superpowers",
            "superpowers-market",
            "verification-before-completion",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Agent,
            "superpowers",
            "superpowers-market",
            "brainstormer",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Agent,
            "superpowers",
            "superpowers-market",
            "plan-writer",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Command,
            "superpowers",
            "superpowers-market",
            "brainstorm",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        component(
            ExtensionKind::Command,
            "superpowers",
            "superpowers-market",
            "write-plan",
            Some("6.3.0"),
            None,
            RowState::Current,
        ),
        ExtensionRow {
            id: "hooks:superpowers".to_owned(),
            kind: ExtensionKind::Hook,
            name: "superpowers".to_owned(),
            source: "superpowers@superpowers-market".to_owned(),
            version: Some("6.3.0".to_owned()),
            available_version: None,
            state: RowState::Current,
            detail: Some("SessionStart, PreToolUse".to_owned()),
        },
        plugin(
            "rust-review@code-review-market",
            "code-review-market",
            Some("1.1.0"),
            Some("1.2.0"),
            RowState::UpdateAvailable,
            Some("~120 tok always-on"),
        ),
        component(
            ExtensionKind::Skill,
            "rust-review",
            "code-review-market",
            "architecture-review",
            Some("1.1.0"),
            Some("1.2.0"),
            RowState::UpdateAvailable,
        ),
        component(
            ExtensionKind::Skill,
            "rust-review",
            "code-review-market",
            "rust-review",
            Some("1.1.0"),
            Some("1.2.0"),
            RowState::UpdateAvailable,
        ),
        component(
            ExtensionKind::Command,
            "rust-review",
            "code-review-market",
            "review",
            Some("1.1.0"),
            Some("1.2.0"),
            RowState::UpdateAvailable,
        ),
        plugin(
            "leyline@claude-night-market",
            "claude-night-market",
            Some("0.1.0"),
            None,
            RowState::AutoDependency("auto-installed".to_owned()),
            None,
        ),
        component(
            ExtensionKind::Skill,
            "leyline",
            "claude-night-market",
            "stewardship",
            Some("0.1.0"),
            None,
            RowState::Current,
        ),
        plugin(
            "off-plugin@probe-market",
            "probe-market",
            Some("1.0.0"),
            None,
            RowState::Disabled,
            None,
        ),
        component(
            ExtensionKind::Skill,
            "off-plugin",
            "probe-market",
            "old-thing",
            Some("1.0.0"),
            None,
            RowState::Disabled,
        ),
        plugin(
            "broken-ghost@ghost-market",
            "ghost-market",
            Some("2.0.0"),
            None,
            RowState::LoadFailed("registered install dir is missing on disk".to_owned()),
            None,
        ),
        plugin(
            "lsp-support@probe-market",
            "probe-market",
            Some("1.0.0"),
            None,
            RowState::Current,
            None,
        ),
        ExtensionRow {
            id: "lsp:lsp-support:rust-analyzer".to_owned(),
            kind: ExtensionKind::Lsp,
            name: "rust-analyzer".to_owned(),
            source: "lsp-support@probe-market".to_owned(),
            version: Some("1.0.0".to_owned()),
            available_version: None,
            state: RowState::Current,
            detail: Some("rust-analyzer: on PATH".to_owned()),
        },
    ];

    app.plugins.available_rows = vec![
        plugin(
            "blabbermouth@claude-night-market",
            "claude-night-market",
            None,
            Some("1.9.19"),
            RowState::AvailableNotInstalled,
            None,
        ),
        component(
            ExtensionKind::Skill,
            "blabbermouth",
            "claude-night-market",
            "announce",
            None,
            Some("1.9.19"),
            RowState::AvailableNotInstalled,
        ),
        component(
            ExtensionKind::Skill,
            "blabbermouth",
            "claude-night-market",
            "deduplicate",
            None,
            Some("1.9.19"),
            RowState::AvailableNotInstalled,
        ),
        plugin(
            "sec-audit@trailofbits",
            "trailofbits",
            None,
            Some("1.2.0"),
            RowState::AvailableNotInstalled,
            None,
        ),
        component(
            ExtensionKind::Skill,
            "sec-audit",
            "trailofbits",
            "sec-audit",
            None,
            Some("1.2.0"),
            RowState::AvailableNotInstalled,
        ),
    ];

    let source = |name: &str| MarketplaceSourceEntry {
        name: name.to_owned(),
        source: Some("github".to_owned()),
        repo: None,
        install_location: None,
    };
    app.plugins.marketplaces =
        vec![source("superpowers-market"), source("claude-night-market"), source("ghost-market")];
    app.plugins.health = vec![
        MarketplaceHealth {
            name: "superpowers-market".to_owned(),
            source: "github".to_owned(),
            available: 12,
            load_error: None,
            install_location: std::path::PathBuf::from(
                "/home/u/.claude/plugins/marketplaces/superpowers-market",
            ),
            drifted: false,
        },
        MarketplaceHealth {
            name: "claude-night-market".to_owned(),
            source: "github".to_owned(),
            available: 40,
            load_error: None,
            install_location: std::path::PathBuf::from("/external/night-market"),
            drifted: true,
        },
        MarketplaceHealth {
            name: "ghost-market".to_owned(),
            source: "github".to_owned(),
            available: 0,
            load_error: Some("no marketplace clone on disk".to_owned()),
            install_location: std::path::PathBuf::from(
                "/home/u/.claude/plugins/marketplaces/ghost-market",
            ),
            drifted: false,
        },
    ];

    app.mcp_mut().servers = vec![forge_primitives::McpServerStatus {
        name: "plugin:context7:context7".to_owned(),
        status: forge_primitives::McpServerConnectionStatus::Connected,
        server_info: Some(forge_primitives::McpServerInfo {
            name: "Context7".to_owned(),
            version: "1.0.0".to_owned(),
        }),
        error: None,
        config: Some(serde_json::json!({
            "type": "stdio",
            "command": "npx",
            "args": ["-y", "@upstash/context7-mcp"],
            "env": {},
        })),
        scope: Some("user".to_owned()),
        tools: Some(vec![forge_primitives::McpToolInfo {
            name: "resolve-library-id".to_owned(),
            description: None,
            annotations: None,
        }]),
        sampling_configured: None,
        sampling_required: None,
    }];

    app
}

/// The page rendered at 160x40, one string per frame row: trailing
/// whitespace trimmed and the page scaffold's right box edge dropped,
/// so a pin holds the page's own text and nothing else.
pub(crate) fn render_frame(mut app: App) -> Vec<String> {
    let backend = TestBackend::new(160, 40);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            crate::ui::config::render_extensions(frame, &mut app);
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let width = usize::from(buffer.area.width);
    buffer
        .content
        .chunks(width)
        .map(|row| {
            let line: String = row.iter().map(ratatui::buffer::Cell::symbol).collect();
            let line = line.trim_end();
            line.strip_suffix('\u{2502}').unwrap_or(line).trim_end().to_owned()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(tab: ExtensionsTab, show_available: bool) -> Vec<String> {
        let mut app = snapshot_app();
        app.plugins.active_tab = tab;
        app.plugins.show_available = show_available;
        render_frame(app)
    }

    fn pinned(tab: &str, lines: &[String]) {
        let expected = match tab {
            "installed" => INSTALLED,
            "skills" => SKILLS,
            "skills_available" => SKILLS_AVAILABLE,
            "agents" => AGENTS,
            "commands" => COMMANDS,
            "hooks" => HOOKS,
            "lsp" => LSP,
            "mcps" => MCPS,
            "marketplaces" => MARKETPLACES,
            "updates" => UPDATES,
            other => panic!("no snapshot named {other}"),
        };
        let expected: Vec<String> =
            expected.trim_matches('\n').split('\n').map(str::to_owned).collect();
        assert_eq!(lines.len(), expected.len(), "{tab}: frame height changed: {lines:?}");
        for (index, (got, want)) in lines.iter().zip(expected.iter()).enumerate() {
            assert_eq!(got, want, "{tab}: row {index} diverged");
        }
    }

    #[test]
    fn the_installed_tab_renders_the_registry_backed_plugins() {
        pinned("installed", &snapshot(ExtensionsTab::Installed, false));
    }

    #[test]
    fn the_skills_tab_renders_installed_components_only_by_default() {
        pinned("skills", &snapshot(ExtensionsTab::Skills, false));
    }

    #[test]
    fn the_skills_toggle_reveals_the_available_catalog_dim() {
        pinned("skills_available", &snapshot(ExtensionsTab::Skills, true));
    }

    #[test]
    fn the_agents_tab_snapshot() {
        pinned("agents", &snapshot(ExtensionsTab::Agents, false));
    }

    #[test]
    fn the_commands_tab_snapshot() {
        pinned("commands", &snapshot(ExtensionsTab::Commands, false));
    }

    #[test]
    fn the_hooks_tab_snapshot() {
        pinned("hooks", &snapshot(ExtensionsTab::Hooks, false));
    }

    #[test]
    fn the_lsp_tab_snapshot() {
        pinned("lsp", &snapshot(ExtensionsTab::Lsp, false));
    }

    #[test]
    fn the_mcps_tab_snapshot() {
        pinned("mcps", &snapshot(ExtensionsTab::Mcps, false));
    }

    #[test]
    fn the_marketplaces_tab_snapshot() {
        pinned("marketplaces", &snapshot(ExtensionsTab::Marketplaces, false));
    }

    #[test]
    fn the_installed_tab_renders_the_updates_panel() {
        let mut app = snapshot_app();
        app.plugins.active_tab = ExtensionsTab::Installed;
        let mut row = forge_primitives::plugins::PluginUpdateRunRow::queued(
            "rust-review@code-review-market".to_owned(),
            "user".to_owned(),
            String::new(),
            Some("1.1.0".to_owned()),
        );
        row.status = forge_primitives::plugins::PluginRunRowStatus::Updated;
        row.installed_version = Some("1.2.0".to_owned());
        row.detail = Some("Restart required to apply.".to_owned());
        app.plugins.update_run = Some(forge_primitives::plugins::PluginUpdateRun {
            trigger: forge_primitives::plugins::PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![row],
        });
        pinned("updates", &render_frame(app));
    }

    // The pinned snapshots live in the sibling module `expected`.
    mod expected;
    use expected::*;
}
