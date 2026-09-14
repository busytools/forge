//! The shared row grammar every Extensions tab renders - aligned
//! columns, nothing wraps:
//!
//! ```text
//! [sel] [state] name          source        installed vs available   action
//! ```
//!
//! The state, name, source and status columns hold one width for the
//! whole tab (derived from the tab's content, capped), so the rows
//! read as a table and a long name truncates instead of shifting the
//! columns. Available rows render dim over blue: visually secondary
//! to the installed tiers.

use forge_primitives::plugins::{
    ExtensionRow, MarketplaceHealth, MarketplaceSourceEntry, RowState,
};
use forge_primitives::{McpServerConnectionStatus, McpServerStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::theme;

/// The selection gutter ahead of the state column; the marker
/// overwrites it in place, so the columns never shift.
const SELECTION_GUTTER: usize = 2;
/// Column floors: the narrowest pane still renders every column.
const MIN_NAME_COLUMN: usize = 12;
const MIN_SOURCE_COLUMN: usize = 10;
/// The status column never shrinks below this, whatever the width.
const MIN_STATUS_COLUMN: usize = 24;
/// Column caps: a marketplace name does not buy a wider table.
const MAX_NAME_COLUMN: usize = 28;
const MAX_SOURCE_COLUMN: usize = 30;
const MAX_STATUS_COLUMN: usize = 52;
/// The action column reserves this many cells at the row's end.
const ACTION_COLUMN: usize = 12;

/// Whether an LSP server binary resolves on `path_var`. `path_var` is
/// injected so tests need not touch the process environment.
pub fn lsp_binary_on_path(server_binary: &str, path_var: Option<&str>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path_var) = path_var else { return false };
    path_var.split(':').any(|dir| {
        let candidate = std::path::Path::new(dir).join(server_binary);
        candidate
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

/// Build the tab's row lines. `rows` are the tab's already-filtered
/// rows; every column truncates so each row stays one line and the
/// columns stay aligned across the tab.
pub fn render_extension_rows(rows: &[&ExtensionRow], width: usize) -> Vec<Line<'static>> {
    let columns = column_widths(rows, width);
    rows.iter().map(|row| extension_row_line(row, &columns, width)).collect()
}

/// One line per MCP server over the shared grammar: state glyph from
/// the connection status, name, scope as source, the status column,
/// transport as a bracketed badge, summary as detail.
pub(crate) fn render_mcp_rows(servers: &[McpServerStatus], width: usize) -> Vec<Line<'static>> {
    let triples: Vec<(String, String, String)> = servers
        .iter()
        .map(|server| {
            (
                server.name.clone(),
                server.scope.clone().unwrap_or_else(|| "session".to_owned()),
                mcp_status_label(server.status).to_owned(),
            )
        })
        .collect();
    let columns = compute_columns(&triples, width);
    servers.iter().map(|server| mcp_row_line(server, &columns, width)).collect()
}

fn mcp_row_line(server: &McpServerStatus, columns: &ColumnWidths, width: usize) -> Line<'static> {
    let (glyph, color) = mcp_state_glyph(server.status);
    let scope = server.scope.clone().unwrap_or_else(|| "session".to_owned());
    let status = mcp_status_label(server.status);

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(glyph.to_owned(), Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(
            pad(&truncate_to_width(&server.name, columns.name), columns.name),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            pad(&truncate_to_width(&scope, columns.source), columns.source),
            Style::default().fg(theme::DIM),
        ),
        Span::raw("  "),
        Span::styled(
            pad(&truncate_to_width(status, columns.status), columns.status),
            Style::default().fg(color),
        ),
        Span::styled(
            format!("  [{}]", transport_label(server.config.as_ref())),
            Style::default().fg(theme::DIM),
        ),
    ];

    let detail = server_summary_line(server);
    let budget = width.saturating_sub(spans_width(&spans) + 2);
    if budget > 4 && !detail.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            truncate_to_width(&detail, budget),
            Style::default().fg(theme::DIM),
        ));
    }
    Line::from(spans)
}

/// One line per configured marketplace over the shared grammar: the
/// health as the state glyph, the name, the source kind as source,
/// `healthy - N plugins` (or the drift notice / failure reason) as the
/// status column, the repo as detail, and Repair right-aligned on the
/// rows that qualify.
pub(crate) fn render_marketplace_rows(
    marketplaces: &[MarketplaceSourceEntry],
    health: &[MarketplaceHealth],
    width: usize,
) -> Vec<Line<'static>> {
    let triples: Vec<(String, String, String)> = marketplaces
        .iter()
        .map(|marketplace| {
            (
                marketplace.name.clone(),
                marketplace.source.clone().unwrap_or_default(),
                marketplace_status_text(marketplace, health),
            )
        })
        .collect();
    let columns = compute_columns(&triples, width);
    marketplaces
        .iter()
        .map(|marketplace| marketplace_row_line(marketplace, health, &columns, width))
        .collect()
}

fn marketplace_health<'a>(
    name: &str,
    health: &'a [MarketplaceHealth],
) -> Option<&'a MarketplaceHealth> {
    health.iter().find(|health| health.name == name)
}

fn marketplace_status_text(
    marketplace: &MarketplaceSourceEntry,
    health: &[MarketplaceHealth],
) -> String {
    match marketplace_health(&marketplace.name, health) {
        Some(health) if health.drifted => {
            "registry drift - installLocation outside the config dir".to_owned()
        }
        Some(health) if health.load_error.is_some() => {
            format!("failed: {}", health.load_error.clone().unwrap_or_default())
        }
        Some(health) => format!("healthy - {} plugins", health.available),
        None => "scan pending".to_owned(),
    }
}

fn marketplace_row_line(
    marketplace: &MarketplaceSourceEntry,
    health: &[MarketplaceHealth],
    columns: &ColumnWidths,
    width: usize,
) -> Line<'static> {
    let entry = marketplace_health(&marketplace.name, health);
    let (glyph, color, repair) = match entry {
        Some(health) if health.drifted => (ICON_WARNING, theme::STATUS_WARNING, true),
        Some(health) if health.load_error.is_some() => {
            (theme::ICON_FAILED, theme::STATUS_ERROR, true)
        }
        Some(_) => (theme::ICON_COMPLETED, theme::REVIEW_RESOLVED, false),
        None => ("-", theme::DIM, false),
    };

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(glyph.to_owned(), Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(
            pad(&truncate_to_width(&marketplace.name, columns.name), columns.name),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            pad(
                &truncate_to_width(marketplace.source.as_deref().unwrap_or(""), columns.source),
                columns.source,
            ),
            Style::default().fg(theme::DIM),
        ),
        Span::raw("  "),
        Span::styled(
            pad(
                &truncate_to_width(&marketplace_status_text(marketplace, health), columns.status),
                columns.status,
            ),
            Style::default().fg(color),
        ),
    ];

    if let Some(repo) = marketplace.repo.as_deref() {
        let budget =
            width.saturating_sub(spans_width(&spans) + 2 + if repair { ACTION_COLUMN } else { 0 });
        if budget > 4 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                truncate_to_width(repo, budget),
                Style::default().fg(theme::DIM),
            ));
        }
    }
    if repair {
        let fill = width.saturating_sub(spans_width(&spans) + "Repair".len());
        spans.push(Span::raw(" ".repeat(fill)));
        spans.push(Span::styled(
            "Repair",
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn mcp_state_glyph(status: McpServerConnectionStatus) -> (&'static str, Color) {
    match status {
        McpServerConnectionStatus::Connected => (theme::ICON_COMPLETED, theme::REVIEW_RESOLVED),
        McpServerConnectionStatus::NeedsAuth => (ICON_WARNING, theme::STATUS_WARNING),
        McpServerConnectionStatus::Pending => ("-", theme::DIM),
        McpServerConnectionStatus::Disabled | McpServerConnectionStatus::Failed => {
            (theme::ICON_FAILED, theme::STATUS_ERROR)
        }
    }
}

/// The out-of-date marker the update report already uses.
const ICON_WARNING: &str = "\u{26a0}";

/// The human label of a connection status; shared with the details
/// overlay.
pub(crate) fn mcp_status_label(status: McpServerConnectionStatus) -> &'static str {
    match status {
        McpServerConnectionStatus::Connected => "connected",
        McpServerConnectionStatus::Failed => "failed",
        McpServerConnectionStatus::NeedsAuth => "needs auth",
        McpServerConnectionStatus::Pending => "pending",
        McpServerConnectionStatus::Disabled => "disabled",
    }
}

/// The transport a server's config names, or unknown; shared with the
/// details overlay.
pub(crate) fn transport_label(config: Option<&Value>) -> &'static str {
    match config.and_then(|c| c.get("type")).and_then(Value::as_str) {
        Some("stdio") => "stdio",
        Some("sse") => "sse",
        Some("http") => "http",
        Some("sdk") => "sdk",
        Some("claudeai-proxy") => "claudeai-proxy",
        _ => "unknown",
    }
}

/// The row's summary: the error when present, else server info, tool
/// count and the command or URL; shared with the details overlay.
pub(crate) fn server_summary_line(server: &McpServerStatus) -> String {
    if let Some(error) = server.error.as_deref()
        && !error.trim().is_empty()
    {
        return error.to_owned();
    }

    let mut parts = Vec::new();
    if let Some(info) = server.server_info.as_ref() {
        parts.push(format!("{} {}", info.name, info.version));
    }
    let tool_count = server.tools.as_deref().map_or(0, <[_]>::len);
    parts.push(tool_summary_line(tool_count));
    if let Some(config) = server.config.as_ref() {
        match config.get("type").and_then(Value::as_str) {
            Some("stdio") => {
                if let Some(cmd) = config.get("command").and_then(Value::as_str) {
                    parts.push(format!("cmd {cmd}"));
                }
            }
            Some("sse" | "http" | "claudeai-proxy") => {
                if let Some(url) = config.get("url").and_then(Value::as_str) {
                    parts.push(url.to_owned());
                }
            }
            Some("sdk") => {
                if let Some(name) = config.get("name").and_then(Value::as_str) {
                    parts.push(format!("sdk {name}"));
                }
            }
            _ => {}
        }
    }
    parts.join("  |  ")
}

fn tool_summary_line(count: usize) -> String {
    crate::ui::format::tool_summary(count)
}

/// One width per column for the whole tab: content-sized between the
/// floor and the cap, shrinking together when the pane is narrow.
struct ColumnWidths {
    name: usize,
    source: usize,
    status: usize,
}

fn column_widths(rows: &[&ExtensionRow], width: usize) -> ColumnWidths {
    let triples: Vec<(String, String, String)> = rows
        .iter()
        .map(|row| {
            let source_display = row.source.split_once('@').map_or(row.source.as_str(), |(n, _)| n);
            (row.name.clone(), source_display.to_owned(), status_text(row))
        })
        .collect();
    compute_columns(&triples, width)
}

/// One width per column for a tab's rows, content-sized between the
/// floor and the cap, shrinking together when the pane is narrow.
fn compute_columns(triples: &[(String, String, String)], width: usize) -> ColumnWidths {
    let clamp = |value: usize, min: usize, max: usize| value.clamp(min, max);
    let mut name = 0;
    let mut source = 0;
    let mut status = 0;
    for (row_name, source_display, status_text) in triples {
        name = name.max(row_name.width());
        source = source.max(source_display.width());
        status = status.max(status_text.width());
    }
    let mut widths = ColumnWidths {
        name: clamp(name, MIN_NAME_COLUMN, MAX_NAME_COLUMN),
        source: clamp(source, MIN_SOURCE_COLUMN, MAX_SOURCE_COLUMN),
        status: clamp(status, MIN_STATUS_COLUMN, MAX_STATUS_COLUMN),
    };
    // A narrow pane shrinks the elastic columns before it drops any;
    // neither column may cross its floor.
    let fixed = SELECTION_GUTTER + ACTION_COLUMN;
    let mut overflow = fixed + widths.name + 2 + widths.source + 2 + widths.status > width;
    while overflow && (widths.name > MIN_NAME_COLUMN || widths.source > MIN_SOURCE_COLUMN) {
        if widths.name > widths.source && widths.name > MIN_NAME_COLUMN {
            widths.name -= 1;
        } else if widths.source > MIN_SOURCE_COLUMN {
            widths.source -= 1;
        } else {
            widths.name -= 1;
        }
        overflow = fixed + widths.name + 2 + widths.source + 2 + widths.status > width;
    }
    widths
}

fn extension_row_line(row: &ExtensionRow, columns: &ColumnWidths, width: usize) -> Line<'static> {
    let (glyph, state_color) = state_glyph(row);
    let status = status_text(row);
    let name_color =
        if row.state == RowState::AvailableNotInstalled { theme::DIM } else { Color::White };
    // Component rows carry the full `name@marketplace` id as their
    // source; the mock's grammar shows the bare plugin name.
    let source_display = row.source.split_once('@').map_or(row.source.as_str(), |(name, _)| name);

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(glyph.to_owned(), Style::default().fg(state_color)),
        Span::raw(" "),
        Span::styled(
            pad(&truncate_to_width(&row.name, columns.name), columns.name),
            Style::default().fg(name_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            pad(&truncate_to_width(source_display, columns.source), columns.source),
            Style::default().fg(theme::DIM),
        ),
        Span::raw("  "),
        Span::styled(
            pad(&truncate_to_width(&status, columns.status), columns.status),
            Style::default().fg(state_color),
        ),
    ];

    let base = spans_width(&spans);
    let mut extras = 0usize;
    if let RowState::RestartRequired = row.state {
        let badge = "  [restart required]";
        extras += badge.len();
        spans.push(Span::styled(badge.to_owned(), Style::default().fg(theme::STATUS_WARNING)));
    }
    if let RowState::AutoDependency(reason) = &row.state {
        let badge = format!("  [{reason}]");
        extras += badge.len();
        spans.push(Span::styled(badge, Style::default().fg(theme::DIM)));
    }
    if let Some(detail) = row.detail.as_deref() {
        let budget = width.saturating_sub(base + extras + ACTION_COLUMN + 4);
        if budget > 4 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                truncate_to_width(detail, budget),
                Style::default().fg(theme::DIM),
            ));
        }
    }
    if let Some(action) = row_action(row) {
        let fill = width.saturating_sub(spans_width(&spans) + action.len());
        spans.push(Span::raw(" ".repeat(fill)));
        spans.push(Span::styled(
            action,
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

fn pad(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(pad))
}

fn state_glyph(row: &ExtensionRow) -> (&'static str, Color) {
    match row.state {
        RowState::UpdateAvailable => (ICON_WARNING, theme::STATUS_WARNING),
        RowState::AvailableNotInstalled => ("-", theme::AVAILABLE),
        RowState::Disabled | RowState::LoadFailed(_) => (theme::ICON_FAILED, theme::STATUS_ERROR),
        RowState::AutoDependency(_) => (theme::ICON_COMPLETED, theme::DIM),
        RowState::Current | RowState::RestartRequired => {
            (theme::ICON_COMPLETED, theme::REVIEW_RESOLVED)
        }
    }
}

fn status_text(row: &ExtensionRow) -> String {
    match &row.state {
        RowState::Current | RowState::RestartRequired | RowState::AutoDependency(_) => {
            match row.version.as_deref() {
                Some(version) => format!("installed {version}"),
                None => "installed".to_owned(),
            }
        }
        RowState::UpdateAvailable => format!(
            "{} -> {} available",
            row.version.as_deref().unwrap_or("?"),
            row.available_version.as_deref().unwrap_or("?")
        ),
        RowState::AvailableNotInstalled => {
            format!("available {} - not installed", row.available_version.as_deref().unwrap_or("?"))
        }
        RowState::Disabled => "disabled".to_owned(),
        RowState::LoadFailed(reason) => format!("failed: {reason}"),
    }
}

/// The row's primary action; the full action set lives in the Enter
/// overlay.
fn row_action(row: &ExtensionRow) -> Option<&'static str> {
    match row.state {
        RowState::UpdateAvailable => Some("Update"),
        RowState::AvailableNotInstalled => Some("Install"),
        _ => None,
    }
}

fn truncate_to_width(text: &str, budget: usize) -> String {
    if text.width() <= budget {
        return text.to_owned();
    }
    let mut cut = String::new();
    for ch in text.chars() {
        if cut.width() + ch.width().unwrap_or(0).max(1) > budget.saturating_sub(1) {
            break;
        }
        cut.push(ch);
    }
    cut.push('\u{2026}');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, source: &str, state: RowState, version: Option<&str>) -> ExtensionRow {
        ExtensionRow {
            id: id.to_owned(),
            kind: forge_primitives::plugins::ExtensionKind::Skill,
            name: id.to_owned(),
            source: source.to_owned(),
            version: version.map(str::to_owned),
            available_version: None,
            state,
            detail: None,
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    /// The grammar's full-row shapes: three states, three distinct
    /// rows, the columns aligned at one width for the tab.
    #[test]
    fn three_row_states_render_their_distinct_rows() {
        let mut rows = [
            row("brainstorming", "superpowers", RowState::Current, Some("6.3.0")),
            row("writing-plans", "superpowers", RowState::UpdateAvailable, Some("6.3.0")),
            row("blabbermouth", "claude-night-market", RowState::AvailableNotInstalled, None),
        ];
        rows[1].available_version = Some("6.4.0".to_owned());
        rows[2].available_version = Some("1.9.19".to_owned());
        let refs: Vec<&ExtensionRow> = rows.iter().collect();

        let lines = render_extension_rows(&refs, 100);
        let text: Vec<String> = lines.iter().map(line_text).collect();

        assert_eq!(text.len(), 3, "one state row renders exactly one line: {text:?}");
        assert_eq!(
            text[0],
            " \u{2713} brainstorming  superpowers          installed 6.3.0                 ",
            "current: {text:?}"
        );
        assert_eq!(
            text[1],
            " \u{26a0} writing-plans  superpowers          6.3.0 -> 6.4.0 available                               Update",
            "update available carries the delta and the right-aligned action: {text:?}"
        );
        assert_eq!(
            text[2],
            " - blabbermouth   claude-night-market  available 1.9.19 - not installed                      Install",
            "not installed: {text:?}"
        );
    }

    /// The columns align across the tab: every row's status starts at
    /// the same offset, whatever the name lengths above it.
    #[test]
    fn the_columns_align_across_the_tab() {
        let rows = [
            row("a", "superpowers", RowState::Current, Some("6.3.0")),
            row("a-much-longer-component-name", "rust-review", RowState::Current, Some("1.1.0")),
        ];
        let refs: Vec<&ExtensionRow> = rows.iter().collect();
        let lines = render_extension_rows(&refs, 100);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        let status_offset = |line: &str| line.find("installed").expect("status word");
        assert_eq!(status_offset(&text[0]), status_offset(&text[1]), "{text:?}");
    }

    #[test]
    fn auto_dependency_and_restart_badges_ride_the_row() {
        let mut auto = row("leyline", "claude-night-market", RowState::Current, Some("1.9.19"));
        auto.state = RowState::AutoDependency("auto-installed".to_owned());
        let mut restart = row("superpowers", "probe", RowState::Current, Some("6.4.0"));
        restart.state = RowState::RestartRequired;
        let refs = vec![&auto, &restart];

        let lines = render_extension_rows(&refs, 100);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            text[0].contains("installed 1.9.19") && text[0].contains("auto-installed"),
            "the auto row is labelled and still states its version: {text:?}"
        );
        assert!(
            text[1].contains("[restart required]"),
            "the restart contract rides the row: {text:?}"
        );
    }

    /// A disabled and a failed row carry their states; the failed row
    /// keeps its reason readable.
    #[test]
    fn disabled_and_failed_rows_render_their_state() {
        let disabled = row("off", "probe", RowState::Disabled, Some("1.0.0"));
        let failed = row("broken", "probe", RowState::LoadFailed("cache-miss".to_owned()), None);
        let refs = vec![&disabled, &failed];

        let lines = render_extension_rows(&refs, 100);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text[0].contains("\u{2717}"), "disabled wears the failed glyph: {text:?}");
        assert!(text[0].contains("disabled"), "{text:?}");
        assert!(text[1].contains("failed: cache-miss"), "the reason rides the row: {text:?}");
    }

    /// The status column never wraps: a name too long for the width
    /// truncates, and the full status stays on the same row.
    #[test]
    fn a_long_name_truncates_instead_of_wrapping_the_status() {
        let mut long = row(
            "a-very-long-plugin-name-that-could-never-fit-the-row",
            "probe-market",
            RowState::UpdateAvailable,
            Some("1.0.0"),
        );
        long.available_version = Some("2.0.0".to_owned());
        let refs = vec![&long];

        let lines = render_extension_rows(&refs, 60);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), 1, "one row, one line: {text:?}");
        assert!(text[0].ends_with("Update"), "the action survives the narrow pane: {text:?}");
        assert!(text[0].contains('\u{2026}'), "the name truncated: {:?}", text[0]);
        assert!(text[0].contains("1.0.0 -> 2.0.0 available"), "{text:?}");
    }

    /// Hook rows name their trigger events on the row.
    #[test]
    fn hook_rows_carry_their_trigger_events() {
        let mut hooks = row("superpowers", "superpowers@probe", RowState::Current, Some("6.3.0"));
        hooks.kind = forge_primitives::plugins::ExtensionKind::Hook;
        hooks.detail = Some("SessionStart, PreToolUse".to_owned());
        let refs = vec![&hooks];

        let lines = render_extension_rows(&refs, 100);
        let text = line_text(&lines[0]);
        assert!(
            text.contains("installed 6.3.0") && text.contains("SessionStart, PreToolUse"),
            "the triggers ride the row: {text}"
        );
    }

    /// An LSP row shows whether the server binary is on PATH.
    #[test]
    fn lsp_rows_state_the_binary_check() {
        let mut on = row("rust-analyzer", "lsp-plugin@probe", RowState::Current, Some("1.0.0"));
        on.kind = forge_primitives::plugins::ExtensionKind::Lsp;
        on.detail = Some("rust-analyzer: on PATH".to_owned());
        let mut missing = row("gopls", "lsp-plugin@probe", RowState::Current, Some("1.0.0"));
        missing.kind = forge_primitives::plugins::ExtensionKind::Lsp;
        missing.detail = Some("gopls: missing".to_owned());
        let refs = vec![&on, &missing];

        let lines = render_extension_rows(&refs, 100);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text[0].contains("rust-analyzer: on PATH"), "present binary: {text:?}");
        assert!(text[1].contains("gopls: missing"), "missing binary: {text:?}");
    }

    /// The name column never crosses its floor when the shrink loop
    /// evens out a narrow pane: with source at its floor, the source
    /// column absorbs the overflow, not the name.
    #[test]
    fn the_name_column_holds_its_floor_on_a_narrow_pane() {
        let rows = [row("twelve-chara", "probe-mkt-1", RowState::Current, Some("1.0.0"))];
        let refs: Vec<&ExtensionRow> = rows.iter().collect();

        // Content widths 12/11/24 leave exactly one overflowing cell
        // at width 64: the shrink loop must take it from source.
        let lines = render_extension_rows(&refs, 64);
        let text = line_text(&lines[0]);
        assert!(
            text.contains("twelve-chara  "),
            "the name column keeps its floor; the source column shrinks first: {text:?}"
        );
    }

    /// A Marketplaces row renders health as the glyph, the name, the
    /// source kind, `healthy - N plugins` as the status column, the
    /// repo as detail - and Repair right-aligned on the drift and
    /// load-failure rows.
    #[test]
    fn marketplace_rows_render_health_and_repair() {
        let source =
            |name: &str, repo: Option<&str>| forge_primitives::plugins::MarketplaceSourceEntry {
                name: name.to_owned(),
                source: Some("github".to_owned()),
                repo: repo.map(str::to_owned),
                install_location: None,
            };
        let marketplaces = vec![
            source("healthy-mkt", Some("anthropics/healthy")),
            source("drifted-mkt", Some("athola/drifted")),
            source("ghost-mkt", None),
        ];
        let health = vec![
            forge_primitives::plugins::MarketplaceHealth {
                name: "healthy-mkt".to_owned(),
                source: "github".to_owned(),
                available: 12,
                load_error: None,
                install_location: std::path::PathBuf::from(
                    "/home/u/.claude/plugins/marketplaces/healthy-mkt",
                ),
                drifted: false,
            },
            forge_primitives::plugins::MarketplaceHealth {
                name: "drifted-mkt".to_owned(),
                source: "github".to_owned(),
                available: 40,
                load_error: None,
                install_location: std::path::PathBuf::from("/external/drifted"),
                drifted: true,
            },
            forge_primitives::plugins::MarketplaceHealth {
                name: "ghost-mkt".to_owned(),
                source: "github".to_owned(),
                available: 0,
                load_error: Some("no marketplace clone on disk".to_owned()),
                install_location: std::path::PathBuf::from(
                    "/home/u/.claude/plugins/marketplaces/ghost-mkt",
                ),
                drifted: false,
            },
        ];

        let lines = render_marketplace_rows(&marketplaces, &health, 130);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text.len(), 3, "one row per marketplace: {text:?}");
        assert!(
            text[0].starts_with(" \u{2713} healthy-mkt")
                && text[0].contains("healthy - 12 plugins"),
            "healthy wears the ok glyph and the plugin count: {text:?}"
        );
        assert!(
            text[1].starts_with(" \u{26a0} drifted-mkt") && text[1].contains("registry drift"),
            "drift wears the warning glyph and the drift notice: {text:?}"
        );
        assert!(text[1].ends_with("Repair"), "Repair right-aligns on drift: {text:?}");
        assert!(
            text[2].starts_with(" \u{2717} ghost-mkt")
                && text[2].contains("failed: no marketplace clone on disk"),
            "a load failure wears the failed glyph and names the reason: {text:?}"
        );
        assert!(text[2].ends_with("Repair"), "Repair right-aligns on load failure: {text:?}");
        assert!(text[0].contains("anthropics/healthy"), "the repo rides as detail: {text:?}");
    }

    /// An MCP server row renders the grammar columns: the state glyph
    /// from the connection status, the server name, scope as source,
    /// the status column, the transport badge and the summary as
    /// detail.
    #[test]
    fn mcp_rows_render_the_grammar_columns() {
        use forge_primitives::McpServerConnectionStatus;
        let stdio = forge_primitives::McpServerStatus {
            name: "playwright".to_owned(),
            status: McpServerConnectionStatus::Connected,
            server_info: None,
            error: None,
            config: Some(serde_json::json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@playwright/mcp@latest"],
                "env": {},
            })),
            scope: Some("user".to_owned()),
            tools: Some(
                vec!["t1", "t2", "t3"]
                    .into_iter()
                    .map(|name| forge_primitives::McpToolInfo {
                        name: name.to_owned(),
                        description: None,
                        annotations: None,
                    })
                    .collect::<Vec<_>>(),
            ),
            sampling_configured: None,
            sampling_required: None,
        };
        let failed = forge_primitives::McpServerStatus {
            name: "broken".to_owned(),
            status: McpServerConnectionStatus::Failed,
            server_info: None,
            error: Some("connection refused".to_owned()),
            config: Some(serde_json::json!({
                "type": "http",
                "url": "https://mcp.example.com/mcp",
            })),
            scope: Some("project".to_owned()),
            tools: None,
            sampling_configured: None,
            sampling_required: None,
        };
        let servers = vec![stdio, failed];

        let lines = render_mcp_rows(&servers, 120);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(text.len(), 2, "one row per server: {text:?}");
        assert!(
            text[0].starts_with(" \u{2713} playwright"),
            "connected wears the ok glyph and the gutter: {text:?}"
        );
        assert!(text[0].contains("user"), "scope renders as source: {text:?}");
        assert!(text[0].contains("connected"), "the status column names the state: {text:?}");
        assert!(text[0].contains("[stdio]"), "transport rides as a badge: {text:?}");
        assert!(
            text[0].contains("3 tools") && text[0].contains("cmd npx"),
            "the summary detail carries tools and command: {text:?}"
        );
        assert!(text[1].starts_with(" \u{2717} broken"), "failed wears the failed glyph: {text:?}");
        assert!(
            text[1].contains("failed") && text[1].contains("connection refused"),
            "the failure reason rides the status or detail: {text:?}"
        );
        assert!(text[1].contains("[http]"), "{text:?}");
        let source_offset = |line: &str| line.find("user").or_else(|| line.find("project"));
        let status_offset = |line: &str| line.find("connected").or_else(|| line.find("failed"));
        assert_eq!(
            source_offset(text[0].as_str()),
            source_offset(text[1].as_str()),
            "the source column aligns across the tab: {text:?}"
        );
        assert_eq!(
            status_offset(text[0].as_str()),
            status_offset(text[1].as_str()),
            "the status column aligns across the tab: {text:?}"
        );
    }

    /// The PATH check walks `:`-separated dirs and requires an
    /// executable file; a missing PATH or a bare directory misses.
    #[test]
    fn the_path_check_accepts_only_executables_on_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("gopls");
        std::fs::write(&binary, b"#!/bin/sh").expect("write binary");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let path = format!("{}:{}", dir.path().display(), "/usr/bin");
        assert!(lsp_binary_on_path("gopls", Some(path.as_str())));
        assert!(!lsp_binary_on_path("rust-analyzer", Some(path.as_str())), "not present");
        assert!(!lsp_binary_on_path("gopls", None), "no PATH, no binary");

        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(
            !lsp_binary_on_path("gopls", Some(path.as_str())),
            "a non-executable file is not a server binary"
        );
    }
}
