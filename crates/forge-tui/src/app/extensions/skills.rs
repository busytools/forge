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

use forge_primitives::plugins::{ExtensionRow, RowState};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
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

/// One width per column for the whole tab: content-sized between the
/// floor and the cap, shrinking together when the pane is narrow.
struct ColumnWidths {
    name: usize,
    source: usize,
    status: usize,
}

fn column_widths(rows: &[&ExtensionRow], width: usize) -> ColumnWidths {
    let clamp = |value: usize, min: usize, max: usize| value.clamp(min, max);
    let mut name = 0;
    let mut source = 0;
    let mut status = 0;
    for row in rows {
        let source_display = row.source.split_once('@').map_or(row.source.as_str(), |(n, _)| n);
        name = name.max(row.name.width());
        source = source.max(source_display.width());
        status = status.max(status_text(row).width());
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

/// The out-of-date marker the update report already uses.
const ICON_WARNING: &str = "\u{26a0}";

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
