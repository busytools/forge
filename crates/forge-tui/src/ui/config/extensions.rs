//! The Extensions page body: the tab bar with live counts, the filter
//! plus update-all action row, and one tab's rows over the shared row
//! grammar (`app::extensions::skills::render_extension_rows`).

use super::theme;
use crate::app::App;
use crate::app::extensions::skills::render_extension_rows;
use crate::app::extensions::{
    ExtensionsTab, count_for_tab, search_enabled, update_all_count, visible_rows,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let body = area.inner(Margin { vertical: 1, horizontal: 1 });
    let top_height: u16 = if search_enabled(app.plugins.active_tab) { 2 } else { 1 };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(top_height.saturating_sub(1)),
            Constraint::Min(1),
        ])
        .split(body);

    frame.render_widget(Paragraph::new(tab_header_line(app)), sections[0]);
    if search_enabled(app.plugins.active_tab) {
        frame.render_widget(Paragraph::new(action_row_line(app)), sections[1]);
    }
    render_list_region(frame, sections[2], app);
}

/// The tab bar: every tab flat beside its siblings, each with its live
/// count; the active tab inverts.
fn tab_header_line(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
    if let Some(blip) =
        crate::app::dictate::blip_span(app, app.spinner_epoch.elapsed().as_secs_f32() * 1000.0)
    {
        spans.push(blip);
    }
    let spans = spans
        .into_iter()
        .chain(ExtensionsTab::ALL.into_iter().enumerate().flat_map(|(index, tab)| {
            let active = tab == app.plugins.active_tab;
            let count = match tab {
                ExtensionsTab::Mcps => app.mcp().servers.len(),
                ExtensionsTab::Marketplaces => app.plugins.marketplaces.len(),
                other => count_for_tab(&app.plugins.rows, other),
            };
            let label = format!(" {} {count} ", tab.title());
            let mut spans = vec![Span::styled(
                label,
                if active {
                    Style::default()
                        .fg(Color::Black)
                        .bg(theme::RUST_ORANGE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                },
            )];
            if index + 1 < ExtensionsTab::ALL.len() {
                spans.push(Span::styled(" ", Style::default().fg(theme::DIM)));
            }
            spans
        }))
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// The action row: the focused filter field and the update-all button
/// with its stale-row count; the button dims to `Update all (0)` when
/// nothing is stale.
fn action_row_line(app: &App) -> Line<'static> {
    let stale = update_all_count(&app.plugins.rows);
    let mut spans = vec![Span::styled(
        format!(" Update all (u) ({stale}) "),
        if stale > 0 {
            Style::default().fg(Color::Black).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM)
        },
    )];

    let query = app.plugins.search_query_for(app.plugins.active_tab);
    let field_style = if app.plugins.search_focused {
        crate::ui::composer::border_style()
    } else {
        Style::default().fg(theme::DIM)
    };
    spans.push(Span::styled("  ", Style::default().fg(theme::DIM)));
    if query.is_empty() && !app.plugins.search_focused {
        spans.push(Span::styled("Type to filter this tab", Style::default().fg(theme::DIM)));
    } else {
        spans.push(Span::styled(query, Style::default().fg(Color::White)));
        if app.plugins.search_focused {
            spans.push(Span::styled(" ", field_style));
        }
    }
    Line::from(spans)
}

fn render_list_region(frame: &mut Frame, area: Rect, app: &App) {
    let list_area =
        if area.width > 1 { area.inner(Margin { vertical: 0, horizontal: 1 }) } else { area };
    match app.plugins.active_tab {
        // The MCP page moves under this tab unchanged in behaviour:
        // its own summary, list, states and actions render here.
        ExtensionsTab::Mcps => {
            super::mcp::render(frame, list_area, app);
            return;
        }
        ExtensionsTab::Marketplaces => {
            frame.render_widget(
                Paragraph::new(marketplace_lines(app, list_area.width)).wrap(Wrap { trim: false }),
                list_area,
            );
            return;
        }
        _ => {}
    }
    let tab = app.plugins.active_tab;
    let rows = visible_rows(app, tab);
    let rendered = if rows.is_empty() {
        empty_tab_lines(app, tab)
    } else {
        let selected = if app.plugins.search_focused {
            None
        } else {
            Some(app.plugins.selected_index_for(tab))
        };
        render_extension_rows(&rows, usize::from(list_area.width.max(1)))
            .into_iter()
            .enumerate()
            .map(|(index, mut line)| {
                if Some(index) == selected
                    && let Some(span) = line.spans.first_mut()
                {
                    span.content = format!("> {}", span.content).into();
                }
                line
            })
            .collect::<Vec<_>>()
    };
    frame.render_widget(Paragraph::new(rendered).wrap(Wrap { trim: false }), list_area);
}

/// First paint never blocks: the pane's copy names what is loading
/// instead of rendering an empty list while the inventory flies.
fn empty_tab_lines(app: &App, tab: ExtensionsTab) -> Vec<Line<'static>> {
    let text = if app.plugins.loading {
        format!("Loading {}...", tab.title())
    } else if !app.plugins.search_query_for(tab).is_empty() {
        "No rows match the current filter.".to_owned()
    } else {
        format!("No {} yet.", tab.title())
    };
    vec![Line::from(Span::styled(text, Style::default().fg(theme::DIM)))]
}

/// The Marketplaces tab: one row per configured marketplace with its
/// source and repo, plus the add row.
fn marketplace_lines(app: &App, viewport_width: u16) -> Vec<Line<'static>> {
    let selected = app.plugins.selected_index_for(ExtensionsTab::Marketplaces);
    let mut lines = Vec::new();
    for (index, marketplace) in app.plugins.marketplaces.iter().enumerate() {
        let selected = index == selected && !app.plugins.search_focused;
        let mut line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                display_row_name(&marketplace.name),
                if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(theme::RUST_ORANGE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                },
            ),
        ]);
        if let Some(source) = marketplace.source.as_deref() {
            line.spans.push(Span::styled(format!("  {source}"), Style::default().fg(theme::DIM)));
        }
        if let Some(repo) = marketplace.repo.as_deref() {
            line.spans.push(Span::styled(format!("  {repo}"), Style::default().fg(theme::DIM)));
        }
        lines.push(line);
    }
    let add_selected = selected == app.plugins.marketplaces.len() && !app.plugins.search_focused;
    lines.push(Line::from(Span::styled(
        display_row_name("Add marketplace"),
        if add_selected {
            Style::default().fg(Color::Black).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM)
        },
    )));
    let _ = viewport_width;
    lines
}

fn display_row_name(text: &str) -> String {
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tab bar renders every tab flat with its live count and the
    /// active tab's count inverted in the theme's accent.
    #[test]
    fn the_tab_bar_renders_all_eight_tabs_with_counts() {
        let mut app = App::test_default();
        app.plugins.active_tab = ExtensionsTab::Skills;

        let text: String =
            tab_header_line(&app).spans.into_iter().map(|span| span.content.to_string()).collect();
        for title in
            ["Installed", "Skills", "Agents", "Commands", "Hooks", "LSP", "MCPs", "Marketplaces"]
        {
            assert!(text.contains(title), "tab {title} present: {text}");
        }
        assert!(text.contains("Installed 0"), "counts ride the tab: {text}");
    }

    /// The update-all button counts the rows it would queue.
    #[test]
    fn the_action_row_counts_stale_rows() {
        let mut app = App::test_default();

        let text: String =
            action_row_line(&app).spans.into_iter().map(|span| span.content.to_string()).collect();
        assert!(text.contains("Update all (u) (0)"), "nothing stale: {text}");

        app.plugins.rows = vec![forge_primitives::plugins::ExtensionRow {
            id: "stale@probe".to_owned(),
            kind: forge_primitives::plugins::ExtensionKind::Plugin,
            name: "stale".to_owned(),
            source: "probe".to_owned(),
            version: Some("1.0.0".to_owned()),
            available_version: Some("2.0.0".to_owned()),
            state: forge_primitives::plugins::RowState::UpdateAvailable,
            detail: None,
        }];
        let text: String =
            action_row_line(&app).spans.into_iter().map(|span| span.content.to_string()).collect();
        assert!(text.contains("Update all (u) (1)"), "one stale row: {text}");
    }
}
