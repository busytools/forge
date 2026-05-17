use super::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tempfile::TempDir;

fn open_settings_app_in_dir(dir: &TempDir) -> App {
    let mut app = App::test_default();
    app.settings_home_override = Some(dir.path().to_path_buf());
    app.set_cwd_raw(dir.path().to_string_lossy().to_string());
    open(&mut app).expect("open");
    app
}

fn open_settings_test_app() -> (TempDir, App) {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = open_settings_app_in_dir(&dir);
    (dir, app)
}

#[test]
fn open_loads_document_and_switches_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(".claude").join("settings.json");
    std::fs::create_dir_all(path.parent().expect("settings parent")).expect("create dir");
    std::fs::write(&path, r#"{"fastMode":true}"#).expect("write");
    let mut app = App::test_default();
    app.settings_home_override = Some(dir.path().to_path_buf());
    app.set_cwd_raw(dir.path().to_string_lossy().to_string());

    open(&mut app).expect("open");

    assert_eq!(app.active_view, ActiveView::Config);
    assert!(app.config.fast_mode_effective());
    assert!(app.config.settings_path.is_some());
    assert!(app.config.local_settings_path.is_some());
    assert!(app.config.preferences_path.is_some());
}

#[test]
fn activate_tab_clears_status_and_error_feedback() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.status_message = Some("saved".into());
    app.config.last_error = Some("failed".into());

    activate_tab(&mut app, ConfigTab::Plugins);

    assert!(app.config.status_message.is_none());
    assert!(app.config.last_error.is_none());
}

#[test]
fn tab_navigation_wraps_and_clears_status_message() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.status_message = Some("saved".to_owned());

    handle_key(&mut app, KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

    assert_eq!(app.config.active_tab, ConfigTab::Mcp);
    assert!(app.config.status_message.is_none());

    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert_eq!(app.config.active_tab, ConfigTab::Mcp);
}

#[test]
fn plugins_tab_uses_arrow_keys_for_inner_navigation() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.installed = vec![
        crate::app::plugins::InstalledPluginEntry {
            id: "frontend-design@claude-plugins-official".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: crate::app::plugins::PluginCapability::Skill,
        },
        crate::app::plugins::InstalledPluginEntry {
            id: "rust-analyzer-lsp@claude-plugins-official".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: crate::app::plugins::PluginCapability::Skill,
        },
    ];

    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));

    assert_eq!(app.config.active_tab, ConfigTab::Plugins);
    assert_eq!(app.plugins.installed_selected_index, 1);
    assert_eq!(app.plugins.active_tab, crate::app::plugins::PluginsViewTab::Plugins);
    assert_eq!(app.plugins.installed_search_query, "");
    assert_eq!(app.plugins.plugins_search_query, "");
    assert!(app.config.overlay.is_none());
}

#[test]
fn plugins_inner_tab_switch_does_not_trigger_refresh() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.loading = false;
    app.plugins.last_inventory_refresh_at = None;

    handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

    assert_eq!(app.plugins.active_tab, crate::app::plugins::PluginsViewTab::Plugins);
    assert!(!app.plugins.loading);
}

#[test]
fn installed_plugin_enter_opens_actions_overlay() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.installed = vec![crate::app::plugins::InstalledPluginEntry {
        id: "frontend-design@claude-plugins-official".to_owned(),
        version: Some("1.0.0".to_owned()),
        scope: "local".to_owned(),
        enabled: true,
        installed_at: None,
        last_updated: None,
        project_path: Some("C:\\work\\project-a".to_owned()),
        capability: crate::app::plugins::PluginCapability::Skill,
    }];
    app.plugins.marketplace = vec![crate::app::plugins::MarketplaceEntry {
        plugin_id: "frontend-design@claude-plugins-official".to_owned(),
        name: "frontend-design".to_owned(),
        description: Some("Create distinctive interfaces".to_owned()),
        marketplace_name: Some("claude-plugins-official".to_owned()),
        version: Some("1.0.0".to_owned()),
        install_count: Some(42),
        source: None,
    }];

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let overlay = app.config.installed_plugin_actions_overlay().expect("installed actions overlay");
    assert_eq!(overlay.title, "Frontend Design From Claude Plugins Official");
    assert_eq!(overlay.description, "Create distinctive interfaces");
    assert_eq!(
        overlay.actions,
        vec![
            InstalledPluginActionKind::Disable,
            InstalledPluginActionKind::Update,
            InstalledPluginActionKind::InstallInCurrentProject,
            InstalledPluginActionKind::Uninstall,
        ]
    );
}

#[test]
fn installed_plugin_overlay_uses_up_down_and_escape() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.installed = vec![crate::app::plugins::InstalledPluginEntry {
        id: "frontend-design@claude-plugins-official".to_owned(),
        version: Some("1.0.0".to_owned()),
        scope: "user".to_owned(),
        enabled: false,
        installed_at: None,
        last_updated: None,
        project_path: None,
        capability: crate::app::plugins::PluginCapability::Skill,
    }];

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

    assert_eq!(
        app.config.installed_plugin_actions_overlay().map(|overlay| overlay.selected_index),
        Some(1)
    );

    handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

    assert_eq!(
        app.config.installed_plugin_actions_overlay().map(|overlay| overlay.selected_index),
        Some(0)
    );

    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(app.config.overlay.is_none());
}

#[test]
fn plugin_enter_opens_install_overlay() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Plugins;
    app.plugins.marketplace = vec![crate::app::plugins::MarketplaceEntry {
        plugin_id: "frontend-design@claude-plugins-official".to_owned(),
        name: "frontend-design".to_owned(),
        description: Some("Create distinctive interfaces".to_owned()),
        marketplace_name: Some("claude-plugins-official".to_owned()),
        version: Some("1.0.0".to_owned()),
        install_count: Some(42),
        source: None,
    }];

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let overlay = app.config.plugin_install_overlay().expect("Plugin install overlay");
    assert_eq!(overlay.title, "Frontend Design");
    assert_eq!(overlay.description, "Create distinctive interfaces");
    assert_eq!(
        overlay.actions,
        vec![
            PluginInstallActionKind::User,
            PluginInstallActionKind::Project,
            PluginInstallActionKind::Local,
        ]
    );
}

#[test]
fn plugin_install_overlay_uses_up_down_and_escape() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Plugins;
    app.plugins.marketplace = vec![crate::app::plugins::MarketplaceEntry {
        plugin_id: "frontend-design@claude-plugins-official".to_owned(),
        name: "frontend-design".to_owned(),
        description: Some("Create distinctive interfaces".to_owned()),
        marketplace_name: Some("claude-plugins-official".to_owned()),
        version: Some("1.0.0".to_owned()),
        install_count: Some(42),
        source: None,
    }];

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

    assert_eq!(app.config.plugin_install_overlay().map(|overlay| overlay.selected_index), Some(1));

    handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

    assert_eq!(app.config.plugin_install_overlay().map(|overlay| overlay.selected_index), Some(0));

    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(app.config.overlay.is_none());
}

#[test]
fn marketplace_enter_opens_actions_overlay_for_configured_marketplace() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Marketplace;
    app.plugins.marketplaces = vec![crate::app::plugins::MarketplaceSourceEntry {
        name: "claude-plugins-official".to_owned(),
        source: Some("github".to_owned()),
        repo: Some("anthropics/claude-plugins-official".to_owned()),
    }];

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let overlay = app.config.marketplace_actions_overlay().expect("marketplace actions overlay");
    assert_eq!(overlay.title, "Claude Plugins Official");
    assert!(overlay.description.contains("Source: github"));
    assert!(overlay.description.contains("Repo: anthropics/claude-plugins-official"));
    assert_eq!(
        overlay.actions,
        vec![
            crate::app::config::MarketplaceActionKind::Update,
            crate::app::config::MarketplaceActionKind::Remove,
        ]
    );
}

#[test]
fn marketplace_add_row_opens_text_input_overlay() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Marketplace;
    app.plugins.marketplaces = vec![crate::app::plugins::MarketplaceSourceEntry {
        name: "claude-plugins-official".to_owned(),
        source: Some("github".to_owned()),
        repo: Some("anthropics/claude-plugins-official".to_owned()),
    }];

    handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let overlay = app.config.add_marketplace_overlay().expect("add marketplace overlay");
    assert_eq!(overlay.draft, "");
    assert_eq!(overlay.cursor, 0);
}

#[test]
fn add_marketplace_overlay_supports_editing_and_escape() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Marketplace;

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    handle_key(&mut app, KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

    let overlay = app.config.add_marketplace_overlay().expect("add marketplace overlay");
    assert_eq!(overlay.draft, "on");
    assert_eq!(overlay.cursor, 1);

    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert!(app.config.overlay.is_none());
}

#[test]
fn add_marketplace_overlay_accepts_paste() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Marketplace;

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    crate::app::events::handle_terminal_event(
        &mut app,
        Event::Paste("anthropics/claude-plugins-official".into()),
    );

    let overlay = app.config.add_marketplace_overlay().expect("add marketplace overlay");
    assert_eq!(overlay.draft, "anthropics/claude-plugins-official");
}

#[test]
fn plugins_search_accepts_paste_when_focused() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Plugins;
    app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Plugins;
    app.plugins.search_focused = true;

    crate::app::events::handle_terminal_event(
        &mut app,
        Event::Paste("frontend-design\nsupabase".into()),
    );

    assert_eq!(app.plugins.plugins_search_query, "frontend-design supabase");
}

#[test]
fn enter_closes_settings_view() {
    let (_dir, mut app) = open_settings_test_app();

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_view, ActiveView::Chat);
}

#[test]
fn esc_closes_settings_view() {
    let (_dir, mut app) = open_settings_test_app();

    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(app.active_view, ActiveView::Chat);
}

#[test]
fn mcp_enter_opens_details_overlay_instead_of_closing_config() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Mcp;
    app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
    app.mcp_mut().servers = vec![forge_primitives::McpServerStatus {
        name: "filesystem".to_owned(),
        status: forge_primitives::McpServerConnectionStatus::Connected,
        server_info: None,
        error: None,
        config: Some(serde_json::json!({
            "type": "stdio",
            "command": "npx",
            "args": ["@modelcontextprotocol/server-filesystem"],
            "env": {},
        })),
        scope: Some("project".to_owned()),
        tools: Some(vec![]),
        sampling_configured: None,
        sampling_required: None,
    }];

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_eq!(app.active_view, ActiveView::Config);
    assert_eq!(
        app.config.mcp_details_overlay().map(|overlay| overlay.server_name.as_str()),
        Some("filesystem")
    );
}

#[test]
fn mcp_details_overlay_enter_closes_overlay() {
    let (_dir, mut app) = open_settings_test_app();
    app.config.active_tab = ConfigTab::Mcp;
    app.config.overlay = Some(ConfigOverlayState::McpDetails(McpDetailsOverlayState {
        server_name: "filesystem".to_owned(),
        selected_index: 0,
    }));

    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert!(app.config.overlay.is_none());
    assert_eq!(app.active_view, ActiveView::Config);
}

#[test]
fn mcp_tab_refresh_key_requests_snapshot() {
    let (_dir, mut app) = open_settings_test_app();
    let mut rx = app.install_testing_stub();
    app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
    app.config.active_tab = ConfigTab::Mcp;
    app.mcp_mut().servers.push(forge_primitives::McpServerStatus {
        name: "stale".to_owned(),
        status: forge_primitives::McpServerConnectionStatus::NeedsAuth,
        server_info: None,
        error: None,
        config: None,
        scope: None,
        tools: None,
        sampling_configured: None,
        sampling_required: None,
    });

    handle_key(&mut app, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));

    let envelope = rx.try_recv().expect("runtime reload command");
    assert_eq!(
        envelope,
        forge_primitives::AgentCommand::ReloadPlugins {
            session_id: forge_primitives::SessionId::new("session-1".to_owned())
        }
    );
    let envelope = rx.try_recv().expect("mcp snapshot command");
    assert_eq!(
        envelope,
        forge_primitives::AgentCommand::GetMcpSnapshot {
            session_id: forge_primitives::SessionId::new("session-1".to_owned())
        }
    );
    assert!(app.mcp().in_flight);
    assert!(app.mcp().servers.is_empty());
}

#[test]
fn request_mcp_snapshot_sends_outside_mcp_tab() {
    let (_dir, mut app) = open_settings_test_app();
    let mut rx = app.install_testing_stub();
    app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
    app.config.active_tab = ConfigTab::Plugins;

    super::mcp::request_mcp_snapshot(&mut app);

    let envelope = rx.try_recv().expect("mcp snapshot command");
    assert_eq!(
        envelope,
        forge_primitives::AgentCommand::GetMcpSnapshot {
            session_id: forge_primitives::SessionId::new("session-1".to_owned())
        }
    );
    assert!(app.mcp().in_flight);
}

#[test]
fn refresh_mcp_snapshot_clears_existing_servers_before_request() {
    let (_dir, mut app) = open_settings_test_app();
    let mut rx = app.install_testing_stub();
    app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
    app.mcp_mut().servers.push(forge_primitives::McpServerStatus {
        name: "stale".to_owned(),
        status: forge_primitives::McpServerConnectionStatus::Connected,
        server_info: None,
        error: None,
        config: None,
        scope: None,
        tools: None,
        sampling_configured: None,
        sampling_required: None,
    });

    refresh_mcp_snapshot(&mut app);

    let envelope = rx.try_recv().expect("mcp snapshot command");
    assert_eq!(
        envelope,
        forge_primitives::AgentCommand::GetMcpSnapshot {
            session_id: forge_primitives::SessionId::new("session-1".to_owned())
        }
    );
    assert!(app.mcp().servers.is_empty());
    assert!(app.mcp().in_flight);
}

#[test]
fn refresh_mcp_snapshot_if_needed_skips_outside_mcp_tab() {
    let (_dir, mut app) = open_settings_test_app();
    let mut rx = app.install_testing_stub();
    app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
    app.config.active_tab = ConfigTab::Plugins;

    super::mcp::refresh_mcp_snapshot_if_needed(&mut app);

    assert!(rx.try_recv().is_err());
    assert!(!app.mcp().in_flight);
}

#[test]
fn claudeai_proxy_server_shows_disabled_authenticate_action() {
    let server = forge_primitives::McpServerStatus {
        name: "claude.ai Google Calendar".to_owned(),
        status: forge_primitives::McpServerConnectionStatus::NeedsAuth,
        server_info: None,
        error: Some(
            "MCP server requires authentication but no OAuth token is configured.".to_owned(),
        ),
        config: Some(serde_json::json!({
            "type": "claudeai-proxy",
            "url": "https://mcp-proxy.anthropic.com/v1/mcp/server",
            "id": "mcpsrv_test",
        })),
        scope: Some("session".to_owned()),
        tools: None,
        sampling_configured: None,
        sampling_required: None,
    };

    let actions = available_mcp_actions(&server);

    assert!(actions.contains(&super::mcp::McpServerActionKind::Authenticate));
    assert!(!super::mcp::is_mcp_action_available(
        &server,
        super::mcp::McpServerActionKind::Authenticate
    ));
    assert!(actions.contains(&super::mcp::McpServerActionKind::Reconnect));
}
