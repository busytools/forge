mod input;
mod mcp;
mod overlay;
mod plugins;

use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::theme;
use input::{add_marketplace_example_lines, render_text_input_field};
use overlay::{
    OverlayChrome, OverlayLayoutSpec, overlay_line_style, render_overlay_separator,
    render_overlay_shell,
};

/// Standalone Plugins view. Renders the same chrome the old Config
/// screen drew (outer titled box + status footer + help line) but
/// wraps the plugins body directly with no tab header.
pub fn render_plugins(frame: &mut Frame, app: &mut App) {
    render_view(frame, app, "Plugins", plugins_help_text, plugins::render);
}

/// Standalone MCP view. Same chrome pattern as `render_plugins`.
pub fn render_mcp(frame: &mut Frame, app: &mut App) {
    render_view(frame, app, "MCP", |_| mcp_help_text(), mcp::render);
}

fn render_view(
    frame: &mut Frame,
    app: &mut App,
    title: &'static str,
    help: impl FnOnce(&App) -> String,
    body: impl FnOnce(&mut Frame, Rect, &App),
) {
    let frame_area = frame.area();
    app.cached_frame_area = frame_area;

    let (message, is_error) = if let Some(error) = app.config.last_error.clone() {
        (error, true)
    } else if let Some(status) = app.config.status_message.clone() {
        (status, false)
    } else {
        (String::new(), false)
    };
    let status = Line::from(Span::styled(
        message,
        Style::default().fg(if is_error { theme::STATUS_ERROR } else { theme::DIM }),
    ));

    let help_text = if app.config.overlay.is_some() { String::new() } else { help(app) };
    let footer = Line::from(Span::styled(help_text, Style::default().fg(theme::RUST_ORANGE)));

    super::page::render_page(frame, title, Some(status), footer, |frame, body_rect| {
        body(frame, body_rect, app);
    });

    // Modal overlays paint over the full frame, above the scaffold.
    if app.config.installed_plugin_actions_overlay().is_some() {
        render_installed_plugin_actions_overlay(frame, frame_area, app);
    } else if app.config.plugin_install_overlay().is_some() {
        render_plugin_install_overlay(frame, frame_area, app);
    } else if app.config.marketplace_actions_overlay().is_some() {
        render_marketplace_actions_overlay(frame, frame_area, app);
    } else if app.config.add_marketplace_overlay().is_some() {
        render_add_marketplace_overlay(frame, frame_area, app);
    } else if app.config.mcp_details_overlay().is_some() {
        mcp::render_details_overlay(frame, frame_area, app);
    }
}

fn plugins_help_text(app: &App) -> String {
    if crate::app::plugins::search_enabled(app.plugins.active_tab) {
        if app.plugins.search_focused {
            "Left/Right switch list | Down list | Type search | Backspace erase | Del clear | Esc close".to_owned()
        } else if matches!(
            app.plugins.active_tab,
            crate::app::plugins::PluginsViewTab::Installed
                | crate::app::plugins::PluginsViewTab::Plugins
        ) {
            "Left/Right switch list | Up search | Up/Down move | Enter actions | u update all | c check updates | Esc close"
                .to_owned()
        } else {
            "Left/Right switch list | Up search | Up/Down move | Enter close | Esc close".to_owned()
        }
    } else if matches!(app.plugins.active_tab, crate::app::plugins::PluginsViewTab::Marketplace) {
        "Left/Right switch list | Up/Down move | Enter actions | Esc close".to_owned()
    } else {
        "Left/Right switch list | Up/Down move | Enter close | Esc close".to_owned()
    }
}

fn mcp_help_text() -> String {
    "Up/Down select | Enter actions | r refresh | Esc close".to_owned()
}

fn render_installed_plugin_actions_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let Some(overlay) = app.config.installed_plugin_actions_overlay() else {
        return;
    };
    let rendered = render_overlay_shell(
        frame,
        area,
        OverlayLayoutSpec {
            min_width: 56,
            min_height: 10,
            width_percent: 70,
            height_percent: 62,
            preferred_height: 14,
            fullscreen_below: Some((56, 16)),
            inner_margin: Margin { vertical: 1, horizontal: 2 },
        },
        OverlayChrome {
            title: "Installed plugin",
            subtitle: None,
            help: Some("Up/Down select | Enter run | Esc cancel"),
        },
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(rendered.body_area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            overlay.title.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            overlay.description.clone(),
            Style::default().fg(theme::DIM),
        )))
        .wrap(Wrap { trim: false }),
        sections[1],
    );
    render_overlay_separator(frame, sections[2]);
    frame.render_widget(
        Paragraph::new(installed_plugin_action_overlay_lines(app)).wrap(Wrap { trim: false }),
        sections[3],
    );
}

fn render_plugin_install_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let Some(overlay) = app.config.plugin_install_overlay() else {
        return;
    };
    let rendered = render_overlay_shell(
        frame,
        area,
        OverlayLayoutSpec {
            min_width: 56,
            min_height: 10,
            width_percent: 70,
            height_percent: 62,
            preferred_height: 14,
            fullscreen_below: Some((56, 16)),
            inner_margin: Margin { vertical: 1, horizontal: 2 },
        },
        OverlayChrome {
            title: "Install plugin",
            subtitle: None,
            help: Some("Up/Down select | Enter run | Esc cancel"),
        },
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(rendered.body_area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            overlay.title.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            overlay.description.clone(),
            Style::default().fg(theme::DIM),
        )))
        .wrap(Wrap { trim: false }),
        sections[1],
    );
    render_overlay_separator(frame, sections[2]);
    frame.render_widget(
        Paragraph::new(plugin_install_overlay_lines(app)).wrap(Wrap { trim: false }),
        sections[3],
    );
}

fn render_marketplace_actions_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let Some(overlay) = app.config.marketplace_actions_overlay() else {
        return;
    };
    let rendered = render_overlay_shell(
        frame,
        area,
        OverlayLayoutSpec {
            min_width: 56,
            min_height: 10,
            width_percent: 70,
            height_percent: 62,
            preferred_height: 14,
            fullscreen_below: Some((56, 16)),
            inner_margin: Margin { vertical: 1, horizontal: 2 },
        },
        OverlayChrome {
            title: "Marketplace",
            subtitle: None,
            help: Some("Up/Down select | Enter run | Esc cancel"),
        },
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(rendered.body_area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            overlay.title.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            overlay.description.clone(),
            Style::default().fg(theme::DIM),
        )))
        .wrap(Wrap { trim: false }),
        sections[1],
    );
    render_overlay_separator(frame, sections[2]);
    frame.render_widget(
        Paragraph::new(marketplace_action_overlay_lines(app)).wrap(Wrap { trim: false }),
        sections[3],
    );
}

fn render_add_marketplace_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let Some(overlay) = app.config.add_marketplace_overlay() else {
        return;
    };
    let rendered = render_overlay_shell(
        frame,
        area,
        OverlayLayoutSpec {
            min_width: 60,
            min_height: 13,
            width_percent: 72,
            height_percent: 66,
            preferred_height: 15,
            fullscreen_below: Some((60, 18)),
            inner_margin: Margin { vertical: 1, horizontal: 2 },
        },
        OverlayChrome {
            title: "Add Marketplace",
            subtitle: None,
            help: Some("Enter add | Esc cancel"),
        },
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(rendered.body_area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Enter marketplace source:",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ))),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(add_marketplace_example_lines()).wrap(Wrap { trim: false }),
        sections[1],
    );
    let blip =
        crate::app::dictate::blip_span(app, app.spinner_epoch.elapsed().as_secs_f32() * 1000.0);
    render_text_input_field(
        frame,
        sections[3],
        &overlay.editor,
        "owner/repo or URL",
        blip.as_ref(),
    );
}

fn installed_plugin_action_overlay_lines(app: &App) -> Vec<Line<'static>> {
    let Some(overlay) = app.config.installed_plugin_actions_overlay() else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    for (index, action) in overlay.actions.iter().copied().enumerate() {
        let selected = index == overlay.selected_index;
        lines.push(Line::from(Span::styled(
            format!("{} {}", if selected { ">" } else { " " }, action.label()),
            overlay_line_style(selected, true),
        )));
        if index + 1 < overlay.actions.len() {
            lines.push(Line::default());
        }
    }
    lines
}

fn plugin_install_overlay_lines(app: &App) -> Vec<Line<'static>> {
    let Some(overlay) = app.config.plugin_install_overlay() else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    for (index, action) in overlay.actions.iter().copied().enumerate() {
        let selected = index == overlay.selected_index;
        lines.push(Line::from(Span::styled(
            format!("{} {}", if selected { ">" } else { " " }, action.label()),
            overlay_line_style(selected, true),
        )));
        if index + 1 < overlay.actions.len() {
            lines.push(Line::default());
        }
    }
    lines
}

fn marketplace_action_overlay_lines(app: &App) -> Vec<Line<'static>> {
    let Some(overlay) = app.config.marketplace_actions_overlay() else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    for (index, action) in overlay.actions.iter().copied().enumerate() {
        let selected = index == overlay.selected_index;
        lines.push(Line::from(Span::styled(
            format!("{} {}", if selected { ">" } else { " " }, action.label()),
            overlay_line_style(selected, true),
        )));
        if index + 1 < overlay.actions.len() {
            lines.push(Line::default());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use crate::app::App;
    use crate::app::config::{
        InstalledPluginActionKind, InstalledPluginActionOverlayState, PluginInstallActionKind,
        PluginInstallOverlayState,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    fn buffer_text(buffer: &Buffer) -> String {
        let width = usize::from(buffer.area.width);
        buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn plugins_tab_renders_inventory_shell() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Plugins;
        app.plugins.installed = vec![crate::app::plugins::InstalledPluginEntry {
            id: "frontend-design@claude-plugins-official".to_owned(),
            version: Some("1.0.0".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
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
        app.plugins.marketplaces = vec![crate::app::plugins::MarketplaceSourceEntry {
            name: "claude-plugins-official".to_owned(),
            source: Some("github".to_owned()),
            repo: Some("anthropics/claude-plugins-official".to_owned()),
            install_location: None,
        }];

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Installed (1)"));
        assert!(rendered.contains("Plugins (1)"));
        assert!(rendered.contains("Marketplace (1)"));
        assert!(
            rendered.contains("Type to filter this list"),
            "the search field reads its placeholder now that the title row is gone"
        );
        assert!(rendered.contains("Frontend Design From Claude Plugins Official"));
        assert!(rendered.contains("SKILL"));
        assert!(rendered.contains("Left/Right switch list"));
    }

    /// The focused filter consumes Enter, so the hint bar must not
    /// advertise a close key it does not have.
    #[test]
    fn focused_plugins_filter_hint_advertises_esc_alone_to_close() {
        let mut app = App::test_default();
        app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Installed;
        app.plugins.search_focused = true;

        assert_eq!(
            super::plugins_help_text(&app),
            "Left/Right switch list | Down list | Type search | Backspace erase | Del clear | Esc close",
            "the focused filter hint offers Esc alone to close"
        );

        app.plugins.search_focused = false;
        assert_eq!(
            super::plugins_help_text(&app),
            "Left/Right switch list | Up search | Up/Down move | Enter actions | u update all | c check updates | Esc close",
            "the list hint names the update and check keys"
        );
    }

    #[test]
    fn plugins_tab_renders_marketplace_plugin_title_and_plugin_id() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Plugins;
        app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Plugins;
        app.plugins.marketplace = vec![crate::app::plugins::MarketplaceEntry {
            plugin_id: "frontend-design@claude-plugins-official".to_owned(),
            name: "frontend-design".to_owned(),
            description: Some("Review UI".to_owned()),
            marketplace_name: Some("claude-plugins-official".to_owned()),
            version: Some("1.0.0".to_owned()),
            install_count: Some(42),
            source: None,
        }];

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Frontend Design"));
        assert!(rendered.contains("Plugin: frontend-design@claude-plugins-official"));
    }

    #[test]
    fn plugins_tab_groups_relevant_installed_plugins_above_other_projects() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();
        app.set_cwd_raw("C:\\work\\project-b");

        app.active_view = crate::app::ActiveView::Plugins;
        app.plugins.installed = vec![
            crate::app::plugins::InstalledPluginEntry {
                id: "other-local@claude-plugins-official".to_owned(),
                version: Some("1.0.0".to_owned()),
                scope: "local".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: Some("C:\\work\\project-a".to_owned()),
                capability: crate::app::plugins::PluginCapability::Skill,
            },
            crate::app::plugins::InstalledPluginEntry {
                id: "user-plugin@claude-plugins-official".to_owned(),
                version: Some("1.0.0".to_owned()),
                scope: "user".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: None,
                capability: crate::app::plugins::PluginCapability::Skill,
            },
            crate::app::plugins::InstalledPluginEntry {
                id: "current-local@claude-plugins-official".to_owned(),
                version: Some("1.0.0".to_owned()),
                scope: "local".to_owned(),
                enabled: true,
                installed_at: None,
                last_updated: None,
                project_path: Some("C:\\work\\project-b".to_owned()),
                capability: crate::app::plugins::PluginCapability::Skill,
            },
        ];

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        let user_index =
            rendered.find("User Plugin From Claude Plugins Official").expect("user plugin");
        let current_index = rendered
            .find("Current Local From Claude Plugins Official")
            .expect("current project plugin");
        let other_index = rendered
            .find("Other Local From Claude Plugins Official")
            .expect("other project plugin");

        assert!(user_index < other_index);
        assert!(current_index < other_index);
        assert!(rendered.contains("Available here"));
        assert!(rendered.contains("Installed elsewhere"));
    }

    #[test]
    fn plugins_tab_shows_loading_copy_instead_of_empty_state_during_refresh() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Plugins;
        app.plugins.loading = true;

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Loading installed plugins..."));
        assert!(!rendered.contains("No installed plugins found."));
    }

    #[test]
    fn marketplace_tab_renders_configured_heading_and_add_placeholder() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Plugins;
        app.plugins.active_tab = crate::app::plugins::PluginsViewTab::Marketplace;

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Configured marketplaces"));
        assert!(rendered.contains("Add marketplace"));
    }

    #[test]
    fn installed_plugin_overlay_renders_title_description_and_actions() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Plugins;
        app.config.overlay = Some(crate::app::config::ConfigOverlayState::InstalledPluginActions(
            InstalledPluginActionOverlayState {
                plugin_id: "frontend-design@claude-plugins-official".to_owned(),
                title: "Frontend Design From Claude Plugins Official".to_owned(),
                description: "Create distinctive interfaces".to_owned(),
                scope: "local".to_owned(),
                project_path: Some("C:\\work\\project-a".to_owned()),
                selected_index: 0,
                actions: vec![
                    InstalledPluginActionKind::Disable,
                    InstalledPluginActionKind::Update,
                    InstalledPluginActionKind::InstallInCurrentProject,
                    InstalledPluginActionKind::Uninstall,
                ],
            },
        ));

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Installed plugin"));
        assert!(rendered.contains("Frontend Design From Claude Plugins Official"));
        assert!(rendered.contains("Create distinctive interfaces"));
        assert!(rendered.contains("Install in current project"));
        assert!(rendered.contains("Up/Down select"));
    }

    #[test]
    fn plugin_install_overlay_renders_title_description_and_actions() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Plugins;
        app.config.overlay = Some(crate::app::config::ConfigOverlayState::PluginInstallActions(
            PluginInstallOverlayState {
                plugin_id: "frontend-design@claude-plugins-official".to_owned(),
                title: "Frontend Design".to_owned(),
                description: "Create distinctive interfaces".to_owned(),
                selected_index: 0,
                actions: vec![
                    PluginInstallActionKind::User,
                    PluginInstallActionKind::Project,
                    PluginInstallActionKind::Local,
                ],
            },
        ));

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Install plugin"));
        assert!(rendered.contains("Frontend Design"));
        assert!(rendered.contains("Create distinctive interfaces"));
        assert!(rendered.contains("Install for project"));
        assert!(rendered.contains("Up/Down select"));
    }

    #[test]
    fn marketplace_actions_overlay_renders_title_description_and_actions() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Plugins;
        app.config.overlay = Some(crate::app::config::ConfigOverlayState::MarketplaceActions(
            crate::app::config::MarketplaceActionsOverlayState {
                name: "claude-plugins-official".to_owned(),
                title: "Claude Plugins Official".to_owned(),
                description: "Source: github\nRepo: anthropics/claude-plugins-official".to_owned(),
                selected_index: 0,
                actions: vec![
                    crate::app::config::MarketplaceActionKind::Update,
                    crate::app::config::MarketplaceActionKind::Remove,
                ],
            },
        ));

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Marketplace"));
        assert!(rendered.contains("Claude Plugins Official"));
        assert!(rendered.contains("Source: github"));
        assert!(rendered.contains("Remove"));
    }

    #[test]
    fn add_marketplace_overlay_renders_examples() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Plugins;
        app.config.overlay = Some(crate::app::config::ConfigOverlayState::AddMarketplace(
            Box::new(crate::app::config::AddMarketplaceOverlayState {
                editor: crate::app::input::InputState::new(),
            }),
        ));

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Add Marketplace"));
        assert!(rendered.contains("Enter marketplace source:"));
        assert!(rendered.contains("owner/repo (GitHub)"));
        assert!(rendered.contains("Enter add"));
    }

    #[test]
    fn mcp_details_overlay_renders_selected_server_details() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Mcp;
        app.config.overlay = Some(crate::app::config::ConfigOverlayState::McpDetails(
            crate::app::config::McpDetailsOverlayState {
                server_name: "filesystem".to_owned(),
                selected_index: 0,
            },
        ));

        // Switch render below from `render_plugins` to `render_mcp`
        // - this test exercises the MCP detail overlay.
        app.mcp_mut().servers = vec![forge_primitives::McpServerStatus {
            name: "filesystem".to_owned(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            server_info: Some(forge_primitives::McpServerInfo {
                name: "Filesystem".to_owned(),
                version: "1.2.3".to_owned(),
            }),
            error: None,
            config: Some(serde_json::json!({
                "type": "stdio",
                "command": "npx",
                "args": ["@modelcontextprotocol/server-filesystem"],
                "env": {},
            })),
            scope: Some("project".to_owned()),
            tools: Some(vec![forge_primitives::McpToolInfo {
                name: "read_file".to_owned(),
                description: Some("Read a file".to_owned()),
                annotations: Some(forge_primitives::McpToolAnnotations {
                    read_only: Some(true),
                    destructive: Some(false),
                    open_world: Some(false),
                }),
            }]),
            sampling_configured: None,
            sampling_required: None,
        }];

        terminal
            .draw(|frame| {
                super::render_mcp(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("filesystem"));
        assert!(rendered.contains("project"));
        assert!(rendered.contains("stdio"));
        assert!(rendered.contains("Reconnect server"));
        assert!(rendered.contains("Disable server"));
        assert!(rendered.contains("Enter run"));
    }

    #[test]
    fn config_footer_renders_status_message_when_present() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.config.status_message = Some("Renaming session...".to_owned());

        terminal
            .draw(|frame| {
                super::render_plugins(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Renaming session..."));
    }
}
