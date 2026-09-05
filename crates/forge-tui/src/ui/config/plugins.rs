use super::theme;
use crate::app::App;
use crate::app::plugins::{
    InstalledPluginEntry, PluginCapability, PluginRunRowStatus, PluginUpdateRunRow, PluginsViewTab,
    availability_for, display_label, filtered_installed, filtered_marketplace_plugins,
    ordered_installed, relevant_installed_count, search_enabled, visible_marketplaces,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

pub(super) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let body = area.inner(Margin { vertical: 1, horizontal: 1 });
    let top_height = top_region_height(app, body.width);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(top_height),
            Constraint::Min(1),
        ])
        .split(body);

    frame.render_widget(Paragraph::new(tab_header_line(app)), sections[0]);
    if search_enabled(app.plugins.active_tab) {
        frame.render_widget(Paragraph::new(action_row_line(app)), sections[1]);
    }
    render_top_region(frame, sections[2], app);
    render_list_region(frame, sections[3], app);
}

/// The action row: the visible update-all button plus the
/// `[plugins] auto_update` state, which is the whole of that config
/// surface - the switch alone governs.
fn action_row_line(app: &App) -> Line<'static> {
    let auto_update =
        app.workspace.as_ref().is_some_and(|workspace| workspace.plugin_settings().auto_update);
    let mut spans = vec![Span::styled(
        " Update all (u) ",
        Style::default().fg(Color::Black).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::styled("  ", Style::default().fg(theme::DIM)));
    if auto_update {
        spans.push(Span::styled(
            "auto-update: on",
            Style::default().fg(theme::REVIEW_RESOLVED).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled("auto-update: off", Style::default().fg(theme::DIM)));
    }
    Line::from(spans)
}

fn render_top_region(frame: &mut Frame, area: Rect, app: &App) {
    if search_enabled(app.plugins.active_tab) {
        let report_height = report_block_height(app);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(report_height),
                Constraint::Min(0),
            ])
            .split(area);
        // The unified single-line field: the query embedded in a one-row
        // thick border, orange while focused, DIM otherwise.
        let style = if app.plugins.search_focused {
            crate::ui::composer::border_style()
        } else {
            Style::default().fg(theme::DIM)
        };
        let content = search_field_line(app);
        frame.render_widget(
            Paragraph::new(crate::ui::composer::single_line_field(
                content,
                usize::from(area.width),
                style,
            )),
            sections[0],
        );
        if report_height > 0 {
            frame.render_widget(
                Paragraph::new(update_report_lines(app)).wrap(Wrap { trim: false }),
                sections[1],
            );
        }
        return;
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "Configured marketplaces",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ])),
        area,
    );
}

fn render_list_region(frame: &mut Frame, area: Rect, app: &App) {
    let list_area =
        if area.width > 1 { area.inner(Margin { vertical: 0, horizontal: 1 }) } else { area };
    let rendered = match app.plugins.active_tab {
        PluginsViewTab::Installed => installed_list(app, list_area.width, list_area.height),
        PluginsViewTab::Plugins => plugins_list(app, list_area.width, list_area.height),
        PluginsViewTab::Marketplace => marketplace_list(app, list_area.width, list_area.height),
    };
    frame.render_widget(
        Paragraph::new(rendered.lines).scroll((rendered.scroll, 0)).wrap(Wrap { trim: false }),
        list_area,
    );
}

fn top_region_height(app: &App, _width: u16) -> u16 {
    // The single-line field is one row whatever the query length; the
    // update report grows the region only while it is on screen.
    1 + report_block_height(app)
}

/// Rows of the update report shown at once; the rest collapse into a
/// count line so the report cannot eat the whole list.
const REPORT_ROW_CAP: usize = 10;

fn report_block_height(app: &App) -> u16 {
    let Some(run) = app.plugins.update_run.as_ref() else {
        return 0;
    };
    if !search_enabled(app.plugins.active_tab) {
        return 0;
    }
    let mut height = 2; // header + blank separator
    height += u16::try_from(run.rows.len().min(REPORT_ROW_CAP)).unwrap_or(u16::MAX);
    if run.rows.len() > REPORT_ROW_CAP {
        height += 1;
    }
    height
}

fn update_report_lines(app: &App) -> Vec<Line<'static>> {
    let Some(run) = app.plugins.update_run.as_ref() else {
        return Vec::new();
    };
    let mut lines = Vec::with_capacity(report_block_height(app) as usize);
    let head_style = if !run.finished {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else if run.rows.iter().any(|row| row.status == PluginRunRowStatus::Failed) {
        Style::default().fg(theme::STATUS_ERROR).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::REVIEW_RESOLVED).add_modifier(Modifier::BOLD)
    };
    let mut header = vec![Span::styled(" Plugin updates", head_style)];
    if run.finished {
        header.push(Span::styled(format!(" - {}", run.summary()), head_style));
        header.push(Span::styled("  (Esc clears)", Style::default().fg(theme::DIM)));
    } else {
        header.push(Span::styled(" - running...", Style::default().fg(theme::DIM)));
    }
    lines.push(Line::from(header));

    for row in run.rows.iter().take(REPORT_ROW_CAP) {
        lines.push(update_report_row_line(row));
    }
    if run.rows.len() > REPORT_ROW_CAP {
        lines.push(Line::from(Span::styled(
            format!("   ...and {} more", run.rows.len() - REPORT_ROW_CAP),
            Style::default().fg(theme::DIM),
        )));
    }
    lines
}

fn update_report_row_line(row: &PluginUpdateRunRow) -> Line<'static> {
    let (word, color) = match row.status {
        PluginRunRowStatus::Queued => ("queued", theme::DIM),
        PluginRunRowStatus::Updating => ("updating...", Color::White),
        PluginRunRowStatus::Updated => ("updated", theme::REVIEW_RESOLVED),
        PluginRunRowStatus::AlreadyCurrent => ("current", theme::DIM),
        PluginRunRowStatus::Failed => ("failed", theme::STATUS_ERROR),
        PluginRunRowStatus::Skipped => ("skipped", theme::DIM),
        PluginRunRowStatus::UpdateAvailable => ("update available", theme::STATUS_WARNING),
    };
    let mut text = format!("   {}  {}", row.plugin_id, word);
    if row.status == PluginRunRowStatus::UpdateAvailable {
        text.push_str(" - ");
        text.push_str(row.installed_version.as_deref().unwrap_or("?"));
        text.push_str(" -> ");
        text.push_str(row.available_version.as_deref().unwrap_or("?"));
    }
    if row.status == PluginRunRowStatus::Updated
        && let Some(version) = row.installed_version.as_deref()
    {
        text.push_str(" to ");
        text.push_str(version);
    }
    if let Some(detail) = row.detail.as_deref() {
        text.push_str("  (");
        text.push_str(detail);
        text.push(')');
    }
    Line::from(Span::styled(text, Style::default().fg(color)))
}

fn tab_header_line(app: &App) -> Line<'static> {
    let mut spans = Vec::new();
    // The view's fixed blip spot: visible on every sub-tab, whichever
    // row holds the focus.
    if let Some(blip) =
        crate::app::dictate::blip_span(app, app.spinner_epoch.elapsed().as_secs_f32() * 1000.0)
    {
        spans.push(blip);
    }
    let spans = spans
        .into_iter()
        .chain(PluginsViewTab::ALL.into_iter().enumerate().flat_map(|(index, tab)| {
            let active = tab == app.plugins.active_tab;
            let count = match tab {
                PluginsViewTab::Installed => filtered_installed(&app.plugins).len(),
                PluginsViewTab::Plugins => filtered_marketplace_plugins(&app.plugins).len(),
                PluginsViewTab::Marketplace => visible_marketplaces(&app.plugins).len(),
            };
            let label = format!(" {} ({count}) ", tab.title());
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
            if index + 1 < PluginsViewTab::ALL.len() {
                spans.push(Span::styled("  ", Style::default().fg(theme::DIM)));
            }
            spans
        }))
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn search_field_line(app: &App) -> Line<'static> {
    let cursor_style =
        Style::default().fg(Color::Black).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(Color::White);
    let hint_style = Style::default().fg(theme::DIM);
    let query = app.plugins.search_query_for(app.plugins.active_tab);

    let mut spans = Vec::new();
    if query.is_empty() {
        if app.plugins.search_focused {
            spans.push(Span::styled(" ".to_owned(), cursor_style));
        }
        spans.push(Span::styled("Type to filter this list".to_owned(), hint_style));
        return Line::from(spans);
    }

    if app.plugins.search_focused {
        spans.push(Span::styled(query, text_style));
        spans.push(Span::styled(" ".to_owned(), cursor_style));
        return Line::from(spans);
    }

    spans.push(Span::styled(query, text_style));
    Line::from(spans)
}

fn installed_list(app: &App, viewport_width: u16, viewport_height: u16) -> RenderedList {
    let entries = ordered_installed(&app.plugins, &app.cwd_raw());
    if entries.is_empty() {
        return RenderedList::single(
            if app.plugins.loading {
                "Loading installed plugins..."
            } else if app.plugins.search_query_for(PluginsViewTab::Installed).is_empty() {
                "No installed plugins found."
            } else {
                "No installed plugins match the current search."
            },
            viewport_height,
        );
    }

    let blocks = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let selected =
                index == app.plugins.installed_selected_index && !app.plugins.search_focused;
            let mut lines = vec![
                installed_title_line(app, entry, selected),
                meta_line(
                    &format!(
                        "{} | {}{}",
                        if entry.enabled { "enabled" } else { "disabled" },
                        entry.scope,
                        entry
                            .version
                            .as_deref()
                            .map_or_else(String::new, |version| format!(" | {version}"))
                    ),
                    selected,
                ),
            ];
            if let Some(project_path) = entry.project_path.as_deref() {
                lines.push(meta_line(&format!("project | {project_path}"), selected));
            }
            lines
        })
        .collect::<Vec<_>>();
    let relevant_count = relevant_installed_count(&app.plugins, &app.cwd_raw());
    let divider_after = if relevant_count > 0 && relevant_count < blocks.len() {
        Some(relevant_count.saturating_sub(1))
    } else {
        None
    };
    let top_label = divider_after.map(|_| section_label_line("Available here"));
    let divider = divider_line(viewport_width, "Installed elsewhere");

    RenderedList::from_blocks_with_sections(
        &blocks,
        app.plugins.installed_selected_index,
        viewport_width,
        viewport_height,
        top_label,
        divider_after,
        &divider,
    )
}

fn plugins_list(app: &App, viewport_width: u16, viewport_height: u16) -> RenderedList {
    let entries = filtered_marketplace_plugins(&app.plugins);
    if entries.is_empty() {
        return RenderedList::single(
            if app.plugins.loading {
                "Loading marketplace plugins..."
            } else if app.plugins.search_query_for(PluginsViewTab::Plugins).is_empty() {
                "No plugins are available from the configured marketplaces."
            } else {
                "No marketplace plugins match the current search."
            },
            viewport_height,
        );
    }

    let blocks = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let selected =
                index == app.plugins.plugins_selected_index && !app.plugins.search_focused;
            let mut lines = vec![title_line(&display_label(&entry.name), selected)];
            lines.push(meta_line(&format!("Plugin: {}", entry.plugin_id), selected));
            if let Some(description) = entry.description.as_deref() {
                lines.push(meta_line(description, selected));
            }
            if let Some(marketplace_name) = entry.marketplace_name.as_deref() {
                lines.push(meta_line(&format!("Marketplace: {marketplace_name}"), selected));
            }
            if let Some(version) = entry.version.as_deref() {
                lines.push(meta_line(&format!("Version: {version}"), selected));
            }
            lines
        })
        .collect::<Vec<_>>();
    RenderedList::from_blocks(
        &blocks,
        app.plugins.plugins_selected_index,
        viewport_width,
        viewport_height,
    )
}

fn marketplace_list(app: &App, viewport_width: u16, viewport_height: u16) -> RenderedList {
    let entries = visible_marketplaces(&app.plugins);
    if entries.is_empty() && app.plugins.loading {
        return RenderedList::single("Loading configured marketplaces...", viewport_height);
    }
    let mut blocks = entries
        .iter()
        .enumerate()
        .map(|(index, marketplace)| {
            let selected = index == app.plugins.marketplace_selected_index;
            let mut lines = vec![title_line(&display_label(&marketplace.name), selected)];
            if let Some(source) = marketplace.source.as_deref() {
                lines.push(meta_line(&format!("Source: {source}"), selected));
            }
            if let Some(repo) = marketplace.repo.as_deref() {
                lines.push(meta_line(&format!("Repo: {repo}"), selected));
            }
            lines
        })
        .collect::<Vec<_>>();

    blocks.push(vec![
        title_line("Add marketplace", app.plugins.marketplace_selected_index == entries.len()),
        meta_line(
            "Add a marketplace from a GitHub repo, URL, or local path.",
            app.plugins.marketplace_selected_index == entries.len(),
        ),
    ]);

    RenderedList::from_blocks(
        &blocks,
        app.plugins.marketplace_selected_index,
        viewport_width,
        viewport_height,
    )
}

fn title_line(text: &str, selected: bool) -> Line<'static> {
    title_line_with_badge(text, None, selected)
}

/// An installed row title: name, capability badge, and the
/// out-of-date marker a finished check left for this entry.
fn installed_title_line(app: &App, entry: &InstalledPluginEntry, selected: bool) -> Line<'static> {
    let mut line =
        title_line_with_badge(&display_label(&entry.id), Some(entry.capability), selected);
    if let Some(availability) = availability_for(&app.plugins, &entry.id, &entry.scope) {
        line.spans.push(Span::styled("  ", Style::default().fg(theme::DIM)));
        line.spans.push(Span::styled(
            format!(
                " {} -> {} ",
                availability.installed_version.as_deref().unwrap_or("?"),
                availability.available_version.as_deref().unwrap_or("?")
            ),
            Style::default()
                .fg(Color::Black)
                .bg(theme::STATUS_WARNING)
                .add_modifier(Modifier::BOLD),
        ));
    }
    line
}

fn title_line_with_badge(
    text: &str,
    capability: Option<PluginCapability>,
    selected: bool,
) -> Line<'static> {
    let mut spans = vec![Span::styled(
        text.to_owned(),
        if selected {
            Style::default().fg(Color::Black).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        },
    )];
    if let Some(capability) = capability {
        spans.push(Span::styled("  ", Style::default().fg(theme::DIM)));
        let (fg, bg) = capability_badge_colors(capability);
        spans.push(Span::styled(
            format!(" {} ", capability.label()),
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn meta_line(text: &str, selected: bool) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        if selected { Style::default().fg(Color::White) } else { Style::default().fg(theme::DIM) },
    ))
}

struct RenderedList {
    lines: Vec<Line<'static>>,
    scroll: u16,
}

impl RenderedList {
    fn single(message: &str, _viewport_height: u16) -> Self {
        Self {
            lines: vec![Line::from(Span::styled(
                message.to_owned(),
                Style::default().fg(theme::DIM),
            ))],
            scroll: 0,
        }
    }

    fn from_blocks(
        blocks: &[Vec<Line<'static>>],
        selected_index: usize,
        viewport_width: u16,
        viewport_height: u16,
    ) -> Self {
        let mut lines = Vec::new();
        let mut selected_start = 0usize;
        let mut selected_height = 1usize;
        let mut offset = 0usize;

        for (index, block) in blocks.iter().enumerate() {
            let block_height = visual_block_height(block, viewport_width).saturating_add(1);
            if index == selected_index {
                selected_start = offset;
                selected_height = block_height;
            }
            lines.extend(block.iter().cloned());
            lines.push(Line::default());
            offset = offset.saturating_add(block_height);
        }

        Self { lines, scroll: selected_scroll(selected_start, selected_height, viewport_height) }
    }

    fn from_blocks_with_sections(
        blocks: &[Vec<Line<'static>>],
        selected_index: usize,
        viewport_width: u16,
        viewport_height: u16,
        top_label: Option<Line<'static>>,
        divider_after: Option<usize>,
        divider: &Line<'static>,
    ) -> Self {
        let mut lines = Vec::new();
        let mut selected_start = 0usize;
        let mut selected_height = 1usize;
        let mut offset = 0usize;
        let divider_height = visual_line_height(divider, viewport_width).saturating_add(1);
        let top_label_height = top_label
            .as_ref()
            .map_or(0, |line| visual_line_height(line, viewport_width).saturating_add(1));

        if let Some(label) = top_label {
            lines.push(label);
            lines.push(Line::default());
            offset = offset.saturating_add(top_label_height);
        }

        for (index, block) in blocks.iter().enumerate() {
            let block_height = visual_block_height(block, viewport_width).saturating_add(1);
            if index == selected_index {
                selected_start = offset;
                selected_height = block_height;
            }
            lines.extend(block.iter().cloned());
            lines.push(Line::default());
            offset = offset.saturating_add(block_height);

            if divider_after == Some(index) {
                lines.push(divider.clone());
                lines.push(Line::default());
                offset = offset.saturating_add(divider_height);
            }
        }

        Self { lines, scroll: selected_scroll(selected_start, selected_height, viewport_height) }
    }
}

fn selected_scroll(selected_start: usize, selected_height: usize, viewport_height: u16) -> u16 {
    let viewport_height = usize::from(viewport_height.max(1));
    if selected_start.saturating_add(selected_height) <= viewport_height {
        0
    } else {
        u16::try_from(
            selected_start.saturating_add(selected_height).saturating_sub(viewport_height),
        )
        .unwrap_or(u16::MAX)
    }
}

fn visual_block_height(block: &[Line<'static>], viewport_width: u16) -> usize {
    block.iter().map(|line| visual_line_height(line, viewport_width)).sum::<usize>()
}

fn visual_line_height(line: &Line<'static>, viewport_width: u16) -> usize {
    let width = usize::from(viewport_width.max(1));
    let content = line.spans.iter().map(|span| span.content.as_ref()).collect::<String>();
    let visual_width = UnicodeWidthStr::width(content.as_str()).max(1);
    visual_width.div_ceil(width)
}

fn section_label_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    ))
}

fn capability_badge_colors(capability: PluginCapability) -> (Color, Color) {
    match capability {
        PluginCapability::Skill => (Color::White, Color::Rgb(64, 64, 64)),
        PluginCapability::Mcp => (Color::White, Color::Rgb(34, 92, 124)),
    }
}

fn divider_line(viewport_width: u16, label: &str) -> Line<'static> {
    let min_width = usize::from(viewport_width.max(20));
    let label_text = format!(" {label} ");
    let label_width = UnicodeWidthStr::width(label_text.as_str());
    let fill_width = min_width.saturating_sub(label_width).max(4);
    let left_width = fill_width / 2;
    let right_width = fill_width.saturating_sub(left_width);

    Line::from(vec![
        Span::styled("─".repeat(left_width), Style::default().fg(theme::DIM)),
        Span::styled(label_text, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(right_width), Style::default().fg(theme::DIM)),
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        action_row_line, installed_list, search_field_line, top_region_height, update_report_lines,
    };
    use crate::app::App;
    use crate::app::plugins::{
        InstalledPluginEntry, PluginCapability, PluginRunRowStatus, PluginUpdateAvailability,
        PluginUpdateRun, PluginUpdateRunRow, PluginUpdateTrigger, PluginsViewTab,
    };
    use crate::ui::theme;
    use ratatui::style::Style;
    use unicode_width::UnicodeWidthStr;

    /// The unified single-line field is one row whatever the query
    /// length; long queries clip instead of growing the region.
    #[test]
    fn top_region_height_stays_single_row_for_a_long_query() {
        let mut app = App::test_default();
        app.plugins.active_tab = PluginsViewTab::Installed;
        app.plugins.search_focused = true;
        app.plugins
            .installed_search_query
            .set_text("search query long enough to have wrapped multiple lines");

        assert_eq!(usize::from(top_region_height(&app, 12)), 1);
    }

    /// The update report names each plugin with its outcome and the
    /// marketplace its id came from, plus the run summary on the
    /// header line.
    #[test]
    fn the_update_report_renders_rows_and_a_summary() {
        let mut app = App::test_default();
        app.plugins.active_tab = PluginsViewTab::Installed;
        app.plugins.update_run = Some(PluginUpdateRun {
            trigger: PluginUpdateTrigger::Manual,
            finished: true,
            rows: vec![
                PluginUpdateRunRow {
                    plugin_id: "pensive@claude-night-market".to_owned(),
                    scope: "user".to_owned(),
                    cwd_raw: String::new(),
                    marketplace: "claude-night-market".to_owned(),
                    status: PluginRunRowStatus::Updated,
                    installed_version: Some("1.8.0".to_owned()),
                    available_version: None,
                    detail: None,
                },
                PluginUpdateRunRow {
                    plugin_id: "leyline@claude-night-market".to_owned(),
                    scope: "user".to_owned(),
                    cwd_raw: String::new(),
                    marketplace: "claude-night-market".to_owned(),
                    status: PluginRunRowStatus::Failed,
                    installed_version: Some("0.1.0".to_owned()),
                    available_version: None,
                    detail: Some("network unreachable".to_owned()),
                },
            ],
        });

        let text: String = update_report_lines(&app)
            .into_iter()
            .flat_map(|line| line.spans.into_iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Plugin updates"), "header present: {text}");
        assert!(text.contains("1 updated, 1 failed, 0 current"), "summary present: {text}");
        assert!(text.contains("pensive@claude-night-market"), "row names plugin + market: {text}");
        assert!(text.contains("updated to 1.8.0"), "updated row shows the new version: {text}");
        assert!(text.contains("(network unreachable)"), "failed row shows why: {text}");
    }

    /// The action row is the visible update-all affordance and names
    /// the auto-update switch state.
    #[test]
    fn the_action_row_carries_the_button_and_the_switch_state() {
        let app = App::test_default();
        let text: String =
            action_row_line(&app).spans.into_iter().map(|span| span.content.to_string()).collect();
        assert!(text.contains("Update all (u)"), "button present: {text}");
        assert!(text.contains("auto-update: off"), "switch state present: {text}");
    }

    /// An entry the last check found out of date wears the version
    /// delta on its title line; without check data there is no badge.
    #[test]
    fn installed_rows_wear_the_out_of_date_marker_from_the_last_check() {
        let mut app = App::test_default();
        app.plugins.installed = vec![InstalledPluginEntry {
            id: "supabase@claude-plugins-official".to_owned(),
            version: Some("2.0.9".to_owned()),
            scope: "user".to_owned(),
            enabled: true,
            installed_at: None,
            last_updated: None,
            project_path: None,
            capability: PluginCapability::Mcp,
        }];
        app.plugins.update_availability = vec![PluginUpdateAvailability {
            plugin_id: "supabase@claude-plugins-official".to_owned(),
            scope: "user".to_owned(),
            marketplace: "claude-plugins-official".to_owned(),
            installed_version: Some("2.0.9".to_owned()),
            available_version: Some("2.1.0".to_owned()),
        }];

        let text: String = installed_list(&app, 80, 24)
            .lines
            .into_iter()
            .flat_map(|line| line.spans.into_iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(" 2.0.9 -> 2.1.0 "), "marker badge: {text}");

        app.plugins.update_availability.clear();
        let text: String = installed_list(&app, 80, 24)
            .lines
            .into_iter()
            .flat_map(|line| line.spans.into_iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!text.contains("-> 2.1.0"), "no marker without check data: {text}");
    }

    /// A focused search field embeds the query in the one-row thick
    /// border; unfocused it dims.
    #[test]
    fn the_search_field_wears_the_single_line_chrome() {
        let mut app = App::test_default();
        app.plugins.active_tab = PluginsViewTab::Installed;
        app.plugins.search_focused = true;
        app.plugins.installed_search_query.set_text("retry");

        let row = crate::ui::composer::single_line_field(
            search_field_line(&app),
            40,
            crate::ui::composer::border_style(),
        );
        let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(
            text.starts_with("\u{250f}\u{2501} ") && text.ends_with("\u{2513}"),
            "the field is the one-row thick border, got: {text}"
        );
        assert!(text.contains("retry"), "the query rides inside the border, got: {text}");
        assert_eq!(
            text.width(),
            40,
            "the fill math assembles the row to the exact requested width, got {text:?}"
        );

        // Unfocused: same shape, DIM border.
        app.plugins.search_focused = false;
        let dim = Style::default().fg(theme::DIM);
        let row = crate::ui::composer::single_line_field(search_field_line(&app), 40, dim);
        assert_eq!(row.spans[0].style.fg, Some(theme::DIM), "unfocused dims the border");
    }
}
