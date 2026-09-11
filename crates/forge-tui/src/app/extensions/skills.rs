//! The shared row grammar every Extensions tab renders:
//!
//! ```text
//! [state] name | source | installed vs available | update state | action
//! ```
//!
//! The status column is fixed and never wraps: a too-long name
//! truncates instead.

use forge_primitives::plugins::{ExtensionRow, RowState};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::ui::theme;

/// The action column reserves this many cells at the row's end.
const ACTION_COLUMN: usize = 12;
/// The status column never shrinks below this, whatever the width.
const MIN_STATUS_COLUMN: usize = 24;

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
/// rows; the name column truncates so every row stays one line.
pub fn render_extension_rows(rows: &[&ExtensionRow], width: usize) -> Vec<Line<'static>> {
    rows.iter().map(|row| extension_row_line(row, width)).collect()
}

fn extension_row_line(row: &ExtensionRow, width: usize) -> Line<'static> {
    let (glyph, state_color) = state_glyph(row);
    let status = status_text(row);

    let reserved = 2 + 2 + MIN_STATUS_COLUMN + ACTION_COLUMN;
    let name_budget = width.saturating_sub(reserved).max(8);
    let name = truncate_to_width(&row.name, name_budget);
    let source = truncate_to_width(&row.source, name_budget);

    let mut spans = vec![
        Span::styled(glyph.to_owned(), Style::default().fg(state_color)),
        Span::raw(" "),
        Span::styled(name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(source, Style::default().fg(theme::DIM)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(state_color)),
    ];

    if let Some(badge) = state_badge(row) {
        spans.push(Span::styled(format!(" [{badge}]"), Style::default().fg(theme::STATUS_WARNING)));
    }
    if let Some(detail) = row.detail.as_deref() {
        spans.push(Span::styled(format!("  {detail}"), Style::default().fg(theme::DIM)));
    }
    if let Some(action) = row_action(row) {
        spans.push(Span::styled(
            format!("  {action}"),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

/// The out-of-date marker the update report already uses.
const ICON_WARNING: &str = "\u{26a0}";

fn state_glyph(row: &ExtensionRow) -> (&'static str, Color) {
    match row.state {
        RowState::UpdateAvailable => (ICON_WARNING, theme::STATUS_WARNING),
        RowState::AvailableNotInstalled => ("-", theme::DIM),
        RowState::Disabled | RowState::LoadFailed(_) => (theme::ICON_FAILED, theme::STATUS_ERROR),
        RowState::Current | RowState::AutoDependency(_) | RowState::RestartRequired => {
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

fn state_badge(row: &ExtensionRow) -> Option<String> {
    match &row.state {
        RowState::AutoDependency(reason) => Some(reason.clone()),
        RowState::RestartRequired => Some("restart required".to_owned()),
        _ => None,
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

    /// The grammar's full-row shapes: three states, three distinct
    /// badges on one row each.
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
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert_eq!(text.len(), 3, "one state row renders exactly one line: {text:?}");
        assert_eq!(
            text[0], "\u{2713} brainstorming  superpowers  installed 6.3.0",
            "current: {text:?}"
        );
        assert_eq!(
            text[1], "\u{26a0} writing-plans  superpowers  6.3.0 -> 6.4.0 available  Update",
            "update available carries the delta and the action: {text:?}"
        );
        assert_eq!(
            text[2],
            "- blabbermouth  claude-night-market  available 1.9.19 - not installed  Install",
            "not installed: {text:?}"
        );
    }

    #[test]
    fn auto_dependency_and_restart_badges_ride_the_row() {
        let mut auto = row("leyline", "claude-night-market", RowState::Current, Some("1.9.19"));
        auto.state = RowState::AutoDependency("auto-installed".to_owned());
        let mut restart = row("superpowers", "probe", RowState::Current, Some("6.4.0"));
        restart.state = RowState::RestartRequired;
        let refs = vec![&auto, &restart];

        let lines = render_extension_rows(&refs, 100);
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text[0], "\u{2713} leyline  claude-night-market  installed 1.9.19 [auto-installed]",
            "the auto badge: {text:?}"
        );
        assert_eq!(
            text[1], "\u{2713} superpowers  probe  installed 6.4.0 [restart required]",
            "the restart badge: {text:?}"
        );
    }

    /// A disabled and a failed row carry their states; the disabled
    /// row offers no action.
    #[test]
    fn disabled_and_failed_rows_render_their_state() {
        let disabled = row("off", "probe", RowState::Disabled, Some("1.0.0"));
        let failed = row("broken", "probe", RowState::LoadFailed("cache-miss".to_owned()), None);
        let refs = vec![&disabled, &failed];

        let lines = render_extension_rows(&refs, 100);
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(text[0], "\u{2717} off  probe  disabled", "disabled: {text:?}");
        assert_eq!(text[1], "\u{2717} broken  probe  failed: cache-miss", "failed: {text:?}");
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
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(lines.len(), 1, "one row, one line: {text:?}");
        assert!(text[0].ends_with("1.0.0 -> 2.0.0 available  Update"), "got {:?}", text[0]);
        assert!(text[0].contains('\u{2026}'), "the name truncated: {:?}", text[0]);
    }

    /// Hook rows name their trigger events on the row.
    #[test]
    fn hook_rows_carry_their_trigger_events() {
        let mut hooks = row("superpowers", "superpowers@probe", RowState::Current, Some("6.3.0"));
        hooks.kind = forge_primitives::plugins::ExtensionKind::Hook;
        hooks.detail = Some("SessionStart, PreToolUse".to_owned());
        let refs = vec![&hooks];

        let lines = render_extension_rows(&refs, 100);
        let text: String = lines[0].spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(
            text,
            "\u{2713} superpowers  superpowers@probe  installed 6.3.0  SessionStart, PreToolUse",
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
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text[0],
            "\u{2713} rust-analyzer  lsp-plugin@probe  installed 1.0.0  rust-analyzer: on PATH",
            "present binary: {text:?}"
        );
        assert_eq!(
            text[1], "\u{2713} gopls  lsp-plugin@probe  installed 1.0.0  gopls: missing",
            "missing binary: {text:?}"
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
