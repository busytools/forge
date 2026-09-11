mod extensions;
mod input;
mod mcp;
mod overlay;

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

/// The Extensions page: one pane over everything installable, from
/// plugins through their components to MCP servers and marketplaces.
pub fn render_extensions(frame: &mut Frame, app: &mut App) {
    render_view(frame, app, "Extensions", extensions_help_text, extensions::render);
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
    if app.config.uninstall_confirm().is_some() {
        render_uninstall_confirm_overlay(frame, frame_area, app);
    } else if app.config.installed_plugin_actions_overlay().is_some() {
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

fn extensions_help_text(app: &App) -> String {
    if crate::app::extensions::search_enabled(app.plugins.active_tab) {
        if app.plugins.search_focused {
            "Left/Right switch tab | Down list | Type to filter | Backspace erase | Del clear | Esc close".to_owned()
        } else {
            "Left/Right switch tab | Up filter | Up/Down move | Enter actions | u update all | c check updates | Esc close"
                .to_owned()
        }
    } else {
        "Left/Right switch tab | Up/Down move | Enter actions | Esc close".to_owned()
    }
}

fn mcp_help_text() -> String {
    "Up/Down select | Enter actions | r refresh | Esc close".to_owned()
}

/// The uninstall confirm: Enter removes the whole bundle, Esc backs
/// out. The description names the plugin and its component count.
fn render_uninstall_confirm_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let Some(overlay) = app.config.uninstall_confirm() else {
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
            preferred_height: 12,
            fullscreen_below: Some((56, 16)),
            inner_margin: Margin { vertical: 1, horizontal: 2 },
        },
        OverlayChrome {
            title: "Uninstall plugin",
            subtitle: None,
            help: Some("Enter confirm | Esc cancel"),
        },
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
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
        Paragraph::new(Span::styled(
            overlay.description.clone(),
            Style::default().fg(theme::STATUS_WARNING),
        ))
        .wrap(Wrap { trim: false }),
        sections[1],
    );
    render_overlay_separator(frame, sections[2]);
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("Uninstall {} from {} scope?", overlay.title, overlay.scope),
            Style::default().fg(theme::DIM),
        )),
        sections[3],
    );
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

    /// The pane's full render path: the tab bar with counts, the
    /// update-all button and the filter placeholder all ride the
    /// production entry point.
    #[test]
    fn the_extensions_pane_renders_its_shell() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Extensions;
        app.plugins.rows = vec![crate::app::extensions::ExtensionRow {
            id: "superpowers@claude-plugins-official".to_owned(),
            kind: forge_primitives::plugins::ExtensionKind::Plugin,
            name: "superpowers".to_owned(),
            source: "claude-plugins-official".to_owned(),
            version: Some("6.3.0".to_owned()),
            available_version: None,
            state: forge_primitives::plugins::RowState::Current,
            detail: None,
        }];

        terminal
            .draw(|frame| {
                super::render_extensions(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Extensions"), "the page title: {rendered}");
        for tab in
            ["Installed", "Skills", "Agents", "Commands", "Hooks", "LSP", "MCPs", "Marketplaces"]
        {
            assert!(rendered.contains(tab), "tab {tab} in the bar: {rendered}");
        }
        assert!(rendered.contains("Installed 1 "), "the plugin count: {rendered}");
        assert!(
            rendered.contains("Update all (u) (0)"),
            "the action row rides the full render path: {rendered}"
        );
        assert!(rendered.contains("Type to filter this tab"), "the filter placeholder: {rendered}");
        assert!(
            rendered.contains("\u{2713} superpowers"),
            "the plugin row renders through the grammar: {rendered}"
        );
        assert!(rendered.contains("Left/Right switch tab"));
    }

    /// First paint never blocks on the CLI: with the inventory channel
    /// still empty and a refresh in flight, the pane renders its
    /// loading copy instead of an empty list.
    #[test]
    fn first_paint_renders_a_loading_state_with_no_inventory_yet() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Extensions;
        app.plugins.loading = true;

        terminal
            .draw(|frame| {
                super::render_extensions(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(
            rendered.contains("Loading Installed..."),
            "the loading copy names the tab: {rendered}"
        );
        assert!(
            !rendered.contains("No Installed yet."),
            "the empty copy must not shadow the loading state: {rendered}"
        );
    }

    /// The Skills tab renders the shared grammar: three states, three
    /// distinct rows, each row exactly one line.
    #[test]
    fn the_skills_tab_renders_its_rows_through_the_grammar() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Extensions;
        app.plugins.active_tab = crate::app::extensions::ExtensionsTab::Skills;
        let row = |name: &str, source: &str, state| crate::app::extensions::ExtensionRow {
            id: format!("skill:{source}:{name}"),
            kind: forge_primitives::plugins::ExtensionKind::Skill,
            name: name.to_owned(),
            source: source.to_owned(),
            version: Some("6.3.0".to_owned()),
            available_version: None,
            state,
            detail: None,
        };
        app.plugins.rows = vec![
            row("brainstorming", "superpowers", forge_primitives::plugins::RowState::Current),
            row(
                "executing-plans",
                "superpowers",
                forge_primitives::plugins::RowState::UpdateAvailable,
            ),
        ];
        app.plugins.rows[1].available_version = Some("6.4.0".to_owned());

        terminal
            .draw(|frame| {
                super::render_extensions(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(
            rendered.contains("\u{2713} brainstorming  superpowers  installed 6.3.0"),
            "current row: {rendered}"
        );
        assert!(
            rendered.contains("6.3.0 -> 6.4.0 available  Update"),
            "the stale row carries the delta and the action on ONE row: {rendered}"
        );
        assert!(rendered.contains("Skills 2 "), "the tab count: {rendered}");
    }

    /// The focused filter consumes Enter, so the hint bar must not
    /// advertise a close key it does not have.
    #[test]
    fn focused_filter_hint_advertises_esc_alone_to_close() {
        let mut app = App::test_default();
        app.plugins.active_tab = crate::app::extensions::ExtensionsTab::Installed;
        app.plugins.search_focused = true;

        assert_eq!(
            super::extensions_help_text(&app),
            "Left/Right switch tab | Down list | Type to filter | Backspace erase | Del clear | Esc close",
            "the focused filter hint offers Esc alone to close"
        );

        app.plugins.search_focused = false;
        assert_eq!(
            super::extensions_help_text(&app),
            "Left/Right switch tab | Up filter | Up/Down move | Enter actions | u update all | c check updates | Esc close",
            "the list hint names the update and check keys"
        );
    }

    /// The Marketplaces tab lists the configured sources with the add
    /// row beneath them.
    #[test]
    fn the_marketplaces_tab_renders_sources_and_the_add_row() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Extensions;
        app.plugins.active_tab = crate::app::extensions::ExtensionsTab::Marketplaces;
        app.plugins.marketplaces = vec![crate::app::extensions::MarketplaceSourceEntry {
            name: "claude-plugins-official".to_owned(),
            source: Some("github".to_owned()),
            repo: Some("anthropics/claude-plugins-official".to_owned()),
            install_location: None,
        }];

        terminal
            .draw(|frame| {
                super::render_extensions(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("claude-plugins-official"));
        assert!(rendered.contains("anthropics/claude-plugins-official"));
        assert!(rendered.contains("Add marketplace"));
    }

    #[test]
    fn installed_plugin_overlay_renders_title_description_and_actions() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Extensions;
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
                super::render_extensions(frame, &mut app);
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

        app.active_view = crate::app::ActiveView::Extensions;
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
                super::render_extensions(frame, &mut app);
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

        app.active_view = crate::app::ActiveView::Extensions;
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
                super::render_extensions(frame, &mut app);
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

        app.active_view = crate::app::ActiveView::Extensions;
        app.config.overlay = Some(crate::app::config::ConfigOverlayState::AddMarketplace(
            Box::new(crate::app::config::AddMarketplaceOverlayState {
                editor: crate::app::input::InputState::new(),
            }),
        ));

        terminal
            .draw(|frame| {
                super::render_extensions(frame, &mut app);
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

    /// The uninstall confirm names the whole bundle - the CLI cannot
    /// remove one component alone - with its component count.
    #[test]
    fn the_uninstall_confirm_names_the_plugin_and_its_component_count() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.active_view = crate::app::ActiveView::Extensions;
        let row = |kind: forge_primitives::plugins::ExtensionKind, name: &str| {
            crate::app::extensions::ExtensionRow {
                id: format!("{kind:?}:superpowers:{name}"),
                kind,
                name: name.to_owned(),
                source: "superpowers@claude-plugins-official".to_owned(),
                version: Some("6.3.0".to_owned()),
                available_version: None,
                state: forge_primitives::plugins::RowState::Current,
                detail: None,
            }
        };
        app.plugins.rows = vec![
            row(forge_primitives::plugins::ExtensionKind::Plugin, "superpowers"),
            row(forge_primitives::plugins::ExtensionKind::Skill, "brainstorming"),
            row(forge_primitives::plugins::ExtensionKind::Skill, "writing-plans"),
            row(forge_primitives::plugins::ExtensionKind::Hook, "superpowers"),
        ];
        crate::app::extensions::installed::open_uninstall_confirm(
            &mut app,
            &crate::app::config::InstalledPluginActionOverlayState {
                plugin_id: "superpowers@claude-plugins-official".to_owned(),
                title: "superpowers".to_owned(),
                description: String::new(),
                scope: "user".to_owned(),
                project_path: None,
                selected_index: 0,
                actions: vec![crate::app::config::InstalledPluginActionKind::Uninstall],
            },
        );

        terminal
            .draw(|frame| {
                super::render_extensions(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Uninstall plugin"), "the chrome: {rendered}");
        assert!(
            rendered.contains("Removes superpowers and its 2 skills, 1 hook sets."),
            "the bundle and its count: {rendered}"
        );
        assert!(rendered.contains("Enter confirm"), "the help: {rendered}");
    }

    #[test]
    fn config_footer_renders_status_message_when_present() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = App::test_default();

        app.config.status_message = Some("Renaming session...".to_owned());

        terminal
            .draw(|frame| {
                super::render_extensions(frame, &mut app);
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Renaming session..."));
    }
}
