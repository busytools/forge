//! Renderer for [`crate::app::ActiveView::Diff`].
//!
//! Full-screen takeover triggered by `/diff` or the Inspector GIT
//! `⤢` click. Two-pane layout with chrome mirroring the Projects
//! pane: FILES rail on the left (banner + DIM rule + 2-col content
//! indent), DIFF body on the right (sibling banner with the
//! currently-viewed file's path + `✕`, same rule pattern). The two
//! rules sit at the same y-position so the `│` separator
//! interrupts what visually reads as one continuous line.
//!
//! This commit ships the scaffold (chrome + file rail + placeholder
//! body); the per-line diff renderer, click-to-comment, and Esc
//! one-shot submit land in follow-up commits.

use forge_workspace::env::git_diff::hunks::{FileHunks, FileStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::app::diff_overlay::DiffOverlayState;
use crate::ui::theme;

/// Wide-tier FILES rail width (matches the Projects pane).
const RAIL_WIDTH_WIDE: u16 = 40;
/// Medium-tier FILES rail width (matches the Projects pane).
const RAIL_WIDTH_MEDIUM: u16 = 30;
/// Wide tier starts at this terminal width.
const WIDE_MIN: u16 = 160;
/// Medium tier starts at this terminal width.
const MEDIUM_MIN: u16 = 120;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.cached_frame_area = area;

    let Some(overlay) = app.diff_overlay.as_ref() else {
        render_missing_state(frame, area);
        return;
    };

    let rail_width = rail_width_for(area.width);
    if rail_width == 0 {
        render_narrow(frame, area, overlay);
    } else {
        render_two_pane(frame, area, overlay, rail_width);
    }
}

fn rail_width_for(terminal_width: u16) -> u16 {
    if terminal_width >= WIDE_MIN {
        RAIL_WIDTH_WIDE
    } else if terminal_width >= MEDIUM_MIN {
        RAIL_WIDTH_MEDIUM
    } else {
        0
    }
}

fn render_missing_state(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("Diff overlay opened without state. This is a bug — press Esc to return.")
            .style(Style::default().fg(theme::STATUS_ERROR)),
        area,
    );
}

fn render_two_pane(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState, rail_width: u16) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(rail_width),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

    render_rail(frame, chunks[0], overlay);
    render_separator(frame, chunks[1]);
    render_pane(frame, chunks[2], overlay);
}

fn render_rail(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState) {
    if area.height < 3 {
        return;
    }
    let inner_width = usize::from(area.width.saturating_sub(6));
    let mut lines = Vec::with_capacity(overlay.files.len() + 4);
    lines.push(banner_row("FILES"));
    lines.push(rule_row(area.width));
    lines.push(Line::default());
    for (idx, file) in overlay.files.iter().enumerate() {
        lines.push(file_rail_row(file, idx == overlay.current_file_idx, inner_width));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_separator(frame: &mut Frame, area: Rect) {
    let style = Style::default().fg(theme::DIM);
    let lines: Vec<Line> =
        (0..area.height).map(|_| Line::from(Span::styled("│", style))).collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_pane(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState) {
    if area.height < 3 {
        return;
    }
    let dim = Style::default().fg(theme::DIM);
    let lines = vec![
        pane_banner_row(overlay),
        rule_row(area.width),
        Line::default(),
        Line::from(Span::styled(
            "  (per-line diff body rendering pending follow-up commit)",
            dim,
        )),
        Line::default(),
        Line::from(Span::styled("  Press Esc to return to chat.", dim)),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_narrow(frame: &mut Frame, area: Rect, _overlay: &DiffOverlayState) {
    frame.render_widget(
        Paragraph::new(
            "Diff overlay narrow-tier rendering pending follow-up commit.\n\n\
             Press Esc to return to chat.",
        )
        .style(Style::default().fg(theme::DIM)),
        area,
    );
}

fn banner_row(label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(label, Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
    ])
}

fn pane_banner_row(overlay: &DiffOverlayState) -> Line<'static> {
    let title = overlay.current_file().map_or("(no file)", |f| f.path.as_str()).to_owned();
    Line::from(vec![
        Span::raw("  "),
        Span::styled("DIFF", Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", Style::default().fg(theme::DIM)),
        Span::styled(title, Style::default().fg(theme::DIM)),
    ])
}

fn rule_row(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(usize::from(width)),
        Style::default().fg(theme::DIM),
    ))
}

fn file_rail_row(file: &FileHunks, current: bool, max_path_width: usize) -> Line<'static> {
    let (glyph_text, glyph_color) = status_glyph(file.status);
    let marker_glyph: &str = if current { "▸" } else { glyph_text };
    let marker_color = if current { theme::RUST_ORANGE } else { glyph_color };
    let path = truncate_path_front(&file.path, max_path_width);
    Line::from(vec![
        Span::raw("  "),
        Span::styled(marker_glyph.to_string(), Style::default().fg(marker_color)),
        Span::raw("  "),
        Span::raw(path),
    ])
}

fn status_glyph(status: FileStatus) -> (&'static str, Color) {
    match status {
        FileStatus::Modified => ("M", theme::RUST_ORANGE),
        FileStatus::Added => ("A", Color::Green),
        FileStatus::Deleted => ("D", theme::STATUS_ERROR),
        FileStatus::Renamed => ("R", theme::RUST_ORANGE),
        FileStatus::Copied => ("C", theme::RUST_ORANGE),
        FileStatus::Untracked => ("U", theme::STATUS_WARNING),
    }
}

fn truncate_path_front(path: &str, max_width: usize) -> String {
    if path.chars().count() <= max_width {
        return path.to_owned();
    }
    let keep = max_width.saturating_sub(1);
    let mut chars = path.chars();
    let skip = path.chars().count().saturating_sub(keep);
    for _ in 0..skip {
        chars.next();
    }
    let mut out = String::with_capacity(max_width);
    out.push('…');
    out.extend(chars);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_width_picks_wide_at_160() {
        assert_eq!(rail_width_for(160), RAIL_WIDTH_WIDE);
        assert_eq!(rail_width_for(200), RAIL_WIDTH_WIDE);
    }

    #[test]
    fn rail_width_picks_medium_between_120_and_160() {
        assert_eq!(rail_width_for(120), RAIL_WIDTH_MEDIUM);
        assert_eq!(rail_width_for(159), RAIL_WIDTH_MEDIUM);
    }

    #[test]
    fn rail_width_collapses_at_narrow_tier() {
        assert_eq!(rail_width_for(119), 0);
        assert_eq!(rail_width_for(80), 0);
    }

    #[test]
    fn truncate_path_front_keeps_short_paths_intact() {
        assert_eq!(truncate_path_front("a/b.rs", 20), "a/b.rs");
    }

    #[test]
    fn truncate_path_front_front_truncates_long_paths() {
        let out = truncate_path_front("crates/forge-tui/src/ui/inspector_pane.rs", 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("inspector_pane.rs"));
    }
}
