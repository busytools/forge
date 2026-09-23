//! The Extensions page body: the tab bar with live counts, the filter,
//! the Available toggle and update-all action row, and the active tab's
//! rows. The row-backed tabs share
//! `app::extensions::skills::render_extension_rows`; MCPs and
//! Marketplaces draw their own.

use super::theme;
use crate::app::App;
use crate::app::extensions::skills::{
    render_extension_rows, render_marketplace_rows, render_mcp_rows,
};
use crate::app::extensions::{
    ExtensionsTab, PANEL_ROW_CAP, available_count_for_tab, count_for_tab, panel_block_height,
    update_all_count, updates, visible_rows, window_offset,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let body = area.inner(Margin { vertical: 1, horizontal: 1 });
    let top_height: u16 = if app.plugins.active_tab.filters_rows() { 2 } else { 1 };
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
    if app.plugins.active_tab.filters_rows() {
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
                ExtensionsTab::Mcps => app.mcp().map_or(0, |mcp| mcp.servers.len()),
                ExtensionsTab::Marketplaces => app.plugins.marketplaces.len(),
                other => count_for_tab(&app.plugins.installed_rows, other),
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

/// The action row: the update-all button with its stale-row count, the
/// Available toggle (every row-backed tab, carrying the tab's `+N` of
/// available rows), and the focused filter field.
fn action_row_line(app: &App) -> Line<'static> {
    let stale = update_all_count(&app.plugins.installed_rows);
    let mut spans = vec![Span::styled(
        format!(" Update all (u) ({stale}) "),
        if stale > 0 {
            Style::default().fg(Color::Black).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM)
        },
    )];

    if app.plugins.active_tab.filters_rows() {
        let available_count =
            available_count_for_tab(&app.plugins.available_rows, app.plugins.active_tab);
        let toggle_style = if app.plugins.hide_available {
            Style::default().fg(theme::AVAILABLE)
        } else {
            Style::default().fg(Color::Black).bg(theme::AVAILABLE).add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(format!(" Available (a) +{available_count} "), toggle_style));
    }

    let query = app.plugins.search_query_for(app.plugins.active_tab);
    let field_style = if app.plugins.search_focused {
        crate::ui::composer::border_style()
    } else {
        Style::default().fg(theme::DIM)
    };
    spans.push(Span::styled("  ", Style::default().fg(theme::DIM)));
    if query.is_empty() && !app.plugins.search_focused {
        spans.push(Span::styled(
            "Filter by name, plugin or marketplace",
            Style::default().fg(theme::DIM),
        ));
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
    let tab = app.plugins.active_tab;
    let width = usize::from(list_area.width.max(1));
    let rows = visible_rows(app, tab);
    let lines = if tab == ExtensionsTab::Mcps {
        mcp_list_lines(app, width)
    } else if tab == ExtensionsTab::Marketplaces {
        marketplace_list_lines(app, width)
    } else if rows.is_empty() {
        empty_tab_lines(app, tab)
    } else {
        render_extension_rows(&rows, width)
    };
    // Empty-state copy is not a row: nothing is selectable then. The
    // Marketplaces tab always keeps its add row.
    let selectable = match tab {
        ExtensionsTab::Mcps => app.mcp().is_some_and(|mcp| !mcp.servers.is_empty()),
        ExtensionsTab::Marketplaces => true,
        _ => !rows.is_empty(),
    };
    let selected = if app.plugins.search_focused || !selectable {
        None
    } else {
        Some(app.plugins.selected_index_for(tab))
    };
    let rendered: Vec<_> = lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            if Some(index) == selected
                && let Some(span) = line.spans.first_mut()
            {
                // The row's leading gutter takes the marker in
                // place, so the columns never shift.
                span.content = ">".into();
                span.style = Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD);
            }
            line
        })
        .collect();
    // The tab's scroll offset windows the list; re-derived against the
    // true viewport here so the selection can never leave the screen.
    let height = usize::from(list_area.height);
    let offset = window_offset(
        app.plugins.selected_index_for(tab),
        app.plugins.scroll_offset_for(tab),
        rendered.len(),
        height,
    );
    let windowed: Vec<_> = rendered.into_iter().skip(offset).take(height).collect();
    frame.render_widget(Paragraph::new(windowed).wrap(Wrap { trim: false }), list_area);
}

/// The Mcps tab's rows: one grammar row per live server, or the
/// session-state copy when there is nothing to list yet.
fn mcp_list_lines(app: &App, width: usize) -> Vec<ratatui::text::Line<'static>> {
    if app.session_id().is_none() {
        return vec![Line::from(Span::styled(
            "Open or resume a session to inspect MCP servers from the live SDK session.",
            Style::default().fg(theme::DIM),
        ))];
    }
    let Some(mcp) = app.mcp() else {
        return Vec::new();
    };
    if mcp.in_flight && mcp.servers.is_empty() {
        return vec![Line::from(Span::styled(
            "Loading MCP status...",
            Style::default().fg(theme::DIM),
        ))];
    }
    if mcp.servers.is_empty() {
        let body = mcp.last_error.as_deref().unwrap_or(
            "The current session did not report any MCP servers. This view only shows live session-backed MCP state.",
        );
        return vec![Line::from(Span::styled(body.to_owned(), Style::default().fg(theme::DIM)))];
    }
    render_mcp_rows(&mcp.servers, width)
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

/// The Marketplaces tab's rows: one grammar row per configured
/// marketplace, plus the always-present add row.
fn marketplace_list_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = render_marketplace_rows(&app.plugins.marketplaces, &app.plugins.health, width);
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("Add marketplace", Style::default().fg(theme::DIM)),
    ]));
    lines
}

#[cfg(test)]
mod snapshots;

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

        app.plugins.installed_rows = vec![forge_primitives::plugins::ExtensionRow {
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
