//! The Extensions page body: the tab bar with live counts, the filter
//! plus update-all action row, and one tab's rows over the shared row
//! grammar (`app::extensions::skills::render_extension_rows`).

use super::theme;
use crate::app::App;
use crate::app::extensions::skills::render_extension_rows;
use crate::app::extensions::{
    ExtensionsTab, count_for_tab, search_enabled, update_all_count, updates, visible_rows,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let body = area.inner(Margin { vertical: 1, horizontal: 1 });
    let top_height: u16 = if search_enabled(app.plugins.active_tab) { 2 } else { 1 };
    let panel_height = panel_block_height(app);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(top_height.saturating_sub(1)),
            Constraint::Min(1),
            Constraint::Length(panel_height),
        ])
        .split(body);

    frame.render_widget(Paragraph::new(tab_header_line(app)), sections[0]);
    if search_enabled(app.plugins.active_tab) {
        frame.render_widget(Paragraph::new(action_row_line(app)), sections[1]);
    }
    render_list_region(frame, sections[2], app);
    if panel_height > 0 {
        render_updates_panel(frame, sections[3], app);
    }
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

/// Rows of the update panel shown at once; the rest collapse into a
/// count line so the panel cannot eat the list.
const PANEL_ROW_CAP: usize = 10;

/// The docked panel's height; zero while no run is on the page.
fn panel_block_height(app: &App) -> u16 {
    let Some(run) = app.plugins.update_run.as_ref() else {
        return 0;
    };
    let mut height = 2; // header + blank separator
    height += u16::try_from(run.rows.len().min(PANEL_ROW_CAP)).unwrap_or(u16::MAX);
    if run.rows.len() > PANEL_ROW_CAP {
        height += 1;
    }
    height
}

/// The docked Updates panel at the page's bottom: one row per action
/// with its state, the batch counter in the header, and the restart
/// aggregation when any applied update stated the contract. It renders
/// from the pane's own run state - the global top spinner is never
/// driven by extension actions.
fn render_updates_panel(frame: &mut Frame, area: Rect, app: &App) {
    let Some(run) = app.plugins.update_run.as_ref() else {
        return;
    };
    let rows = updates::panel_rows(run);
    let header_style = if !run.finished {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else if rows.iter().any(|row| row.failed) {
        Style::default().fg(theme::STATUS_ERROR).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::REVIEW_RESOLVED).add_modifier(Modifier::BOLD)
    };

    let mut lines = Vec::new();
    let mut header = vec![
        Span::styled(" Updates", header_style),
        Span::styled(format!(" - {} ", updates::panel_counter(run)), header_style),
    ];
    if !run.finished {
        header.push(Span::styled("updating...", Style::default().fg(theme::DIM)));
    } else if let Some(note) = updates::restart_note(run) {
        header.push(Span::styled(
            format!("\u{b7} {note}"),
            Style::default().fg(theme::STATUS_WARNING),
        ));
    }
    lines.push(Line::from(header));

    for row in rows.iter().take(PANEL_ROW_CAP) {
        let state_style = if row.failed {
            Style::default().fg(theme::STATUS_ERROR)
        } else if row.state_word == "done" {
            Style::default().fg(theme::REVIEW_RESOLVED)
        } else if row.state_word == "queued" {
            Style::default().fg(theme::DIM)
        } else {
            Style::default().fg(Color::White)
        };
        let mut spans =
            vec![Span::styled(format!("   {}", row.label), Style::default().fg(Color::White))];
        if let Some(delta) = row.delta.as_deref() {
            spans.push(Span::styled(
                format!("  {delta}"),
                Style::default().fg(theme::STATUS_WARNING),
            ));
        }
        spans.push(Span::styled(format!("  {}", row.state_word), state_style));
        if let Some(detail) = row.detail.as_deref() {
            spans.push(Span::styled(format!("  ({detail})"), Style::default().fg(theme::DIM)));
        }
        lines.push(Line::from(spans));
    }
    if run.rows.len() > PANEL_ROW_CAP {
        lines.push(Line::from(Span::styled(
            format!("   ...and {} more", run.rows.len() - PANEL_ROW_CAP),
            Style::default().fg(theme::DIM),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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
/// health from the scan - `healthy`, the drift notice, or the load
/// error - plus the add row.
fn marketplace_lines(app: &App, _viewport_width: u16) -> Vec<Line<'static>> {
    let selected = app.plugins.selected_index_for(ExtensionsTab::Marketplaces);
    let mut lines = Vec::new();
    for (index, marketplace) in app.plugins.marketplaces.iter().enumerate() {
        let selected = index == selected && !app.plugins.search_focused;
        let mut line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                marketplace.name.clone(),
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
        let health = app.plugins.health.iter().find(|health| health.name == marketplace.name);
        match health {
            Some(health) if health.drifted => {
                line.spans.push(Span::styled(
                    "  registry drift - installLocation outside the config dir",
                    Style::default().fg(theme::STATUS_WARNING),
                ));
                line.spans.push(Span::styled("  Repair", Style::default().fg(theme::RUST_ORANGE)));
            }
            Some(health) if health.load_error.is_some() => {
                let reason = health.load_error.clone().unwrap_or_default();
                line.spans.push(Span::styled(
                    format!("  load failed: {reason}"),
                    Style::default().fg(theme::STATUS_ERROR),
                ));
                line.spans.push(Span::styled("  Repair", Style::default().fg(theme::RUST_ORANGE)));
            }
            Some(health) => {
                line.spans.push(Span::styled(
                    format!("  healthy \u{b7} {} plugins", health.available),
                    Style::default().fg(theme::REVIEW_RESOLVED),
                ));
            }
            // No health entry yet: the pane's first disk scan has not
            // landed. Name it rather than rendering a bare name.
            None => {
                line.spans.push(Span::styled("  scan pending", Style::default().fg(theme::DIM)));
            }
        }
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
        "Add marketplace",
        if add_selected {
            Style::default().fg(Color::Black).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM)
        },
    )));
    lines
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
