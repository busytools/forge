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

use forge_workspace::env::git_diff::hunks::{DiffLine, DiffLineKind, FileHunks, FileStatus, Hunk};

use crate::app::diff_overlay::rail_width_for;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::app::diff_overlay::DiffOverlayState;
use crate::ui::theme;

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
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(rail_width), Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let rail_area = chunks[0];
    let sep_area = chunks[1];
    let pane_area = chunks[2];

    // Short-circuit on a too-short pane: skip building the body
    // lines (allocating Vec<Line> + per-line spans only to drop
    // them is wasted work) and surface a "terminal too short"
    // notice so the user knows why the body is empty.
    if pane_area.height < 3 {
        render_rail(frame, rail_area, overlay);
        render_separator(frame, sep_area);
        if pane_area.height >= 1 {
            frame.render_widget(
                Paragraph::new("  Terminal too short — resize and re-open /diff.")
                    .style(Style::default().fg(theme::STATUS_WARNING)),
                pane_area,
            );
        }
        return;
    }

    // Build the body line list up-front so we know its total
    // height; clamp body_scroll against (total - visible) so a
    // wheel-past-end leaves a useful one-screen-of-tail visible
    // instead of a blank pane. Writeback to overlay state keeps
    // the wheel handler in sync with whatever the renderer last
    // saw.
    let body_lines = build_pane_lines(overlay, pane_area);
    let max_offset = body_lines.len().saturating_sub(usize::from(pane_area.height));
    let max_offset_u16 = u16::try_from(max_offset).unwrap_or(u16::MAX);
    let body_scroll = if let Some(overlay_mut) = app.diff_overlay.as_mut() {
        let clamped = overlay_mut.body_scroll.min(max_offset_u16);
        overlay_mut.body_scroll = clamped;
        clamped
    } else {
        0
    };

    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    render_rail(frame, rail_area, overlay);
    render_separator(frame, sep_area);
    frame.render_widget(Paragraph::new(body_lines).scroll((body_scroll, 0)), pane_area);
}

fn render_missing_state(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("Diff overlay opened without state. This is a bug — press Esc to return.")
            .style(Style::default().fg(theme::STATUS_ERROR)),
        area,
    );
}

fn render_rail(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState) {
    if area.height < 3 {
        return;
    }
    let inner_width = usize::from(area.width.saturating_sub(6));
    let mut lines = Vec::with_capacity(overlay.files.len() + 5);
    lines.push(banner_row("FILES"));
    lines.push(rule_row(area.width));
    lines.push(Line::default());
    for (idx, file) in overlay.files.iter().enumerate() {
        lines.push(file_rail_row(file, idx == overlay.current_file_idx, inner_width));
    }
    if overlay.untracked_suppressed > 0 {
        // Surface the cap overflow so a fresh-repo state with many
        // untracked files doesn't render identically to a clean
        // tree. Yellow signals "suppressed work-product, not a
        // failure" — matches the Untracked status glyph colour.
        lines.push(Line::from(Span::styled(
            format!(
                "  +{} untracked suppressed (cap {})",
                overlay.untracked_suppressed,
                forge_workspace::env::git_diff::hunks::MAX_UNTRACKED_FILES,
            ),
            Style::default().fg(theme::STATUS_WARNING),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_separator(frame: &mut Frame, area: Rect) {
    let style = Style::default().fg(theme::DIM);
    let lines: Vec<Line> = (0..area.height).map(|_| Line::from(Span::styled("│", style))).collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Build the right pane's body lines (banner + rule + per-hunk
/// content). Lifted out of the renderer so the top-level `render`
/// can compute total height and clamp `body_scroll` before drawing.
fn build_pane_lines(overlay: &DiffOverlayState, area: Rect) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(pane_banner_row(overlay));
    lines.push(rule_row(area.width));
    lines.push(Line::default());

    // Precedence is intentional: when the scanner failed we MUST
    // show the failure regardless of whether any files came back.
    // The partial-failure case is `name-status` ran fine (so we
    // have file entries) but `--no-ext-diff` failed (so every
    // file's `hunks` is empty). Without this guard the renderer
    // would fall into `Some(file) if file.hunks.is_empty()` and
    // print "(binary file or no diff content)" — a lie that
    // trains the user to ignore a real subprocess crash.
    if !overlay.scanner_ok {
        // Include the target ref so a user who typoed (`/diff develpoment`)
        // can spot the mistake without dismissing the overlay to scroll
        // chat. "tracing target: ENV_GIT" spells out what kind of target
        // it is so it doesn't read as a git ref.
        lines.push(Line::from(Span::styled(
            format!(
                "  Scan failed for `{}` — see tracing logs under ENV_GIT. Press Esc to retry.",
                overlay.target,
            ),
            Style::default().fg(theme::STATUS_ERROR),
        )));
        return lines;
    }
    match overlay.current_file() {
        None => {
            lines.push(Line::from(Span::styled(
                "  (no file selected)",
                Style::default().fg(theme::DIM),
            )));
        }
        Some(file) if file.hunks.is_empty() => {
            // An Untracked file with no hunks comes from one of
            // the scan_untracked drop paths (size-cap exceeded,
            // non-regular file, IO error) — all of which log WARN
            // under ENV_GIT. The tracked-file case is a real
            // binary diff from git. Differentiate so the user
            // knows whether to grep logs vs accept the answer.
            let message = if file.status == FileStatus::Untracked {
                "  (untracked, content not surfaced — see ENV_GIT logs)"
            } else {
                "  (binary file or no diff content)"
            };
            lines.push(Line::from(Span::styled(
                message,
                Style::default().fg(theme::DIM),
            )));
        }
        Some(file) => {
            let gutter_width = gutter_width_for(file);
            for (idx, hunk) in file.hunks.iter().enumerate() {
                if idx > 0 {
                    lines.push(Line::default());
                }
                lines.push(hunk_header_row(hunk));
                for diff_line in &hunk.lines {
                    lines.push(diff_line_row(diff_line, gutter_width));
                }
            }
        }
    }

    lines
}

fn gutter_width_for(file: &FileHunks) -> usize {
    let max_line = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter_map(|l| l.new_line.or(l.old_line))
        .max()
        .unwrap_or(1);
    // Min width 2 so single-digit line numbers don't shift the
    // marker column relative to two-digit ones inside the same
    // hunk; cap at 6 for sanity (10⁶ lines is well beyond what
    // anyone reviews in one pane).
    max_line.to_string().len().clamp(2, 6)
}

fn hunk_header_row(hunk: &Hunk) -> Line<'static> {
    let text = format!(
        "  @@ -{},{} +{},{} @@",
        hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
    );
    Line::from(Span::styled(text, Style::default().fg(Color::Cyan)))
}

fn diff_line_row(line: &DiffLine, gutter_width: usize) -> Line<'static> {
    let (marker, marker_color) = match line.kind {
        DiffLineKind::Added => ("+", Color::Green),
        DiffLineKind::Removed => ("-", Color::Red),
        DiffLineKind::Context => (" ", theme::DIM),
    };
    let line_num = match line.kind {
        DiffLineKind::Added | DiffLineKind::Context => line.new_line,
        DiffLineKind::Removed => line.old_line,
    };
    let gutter = match line_num {
        Some(n) => format!("{n:>gutter_width$}"),
        None => " ".repeat(gutter_width),
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(gutter, Style::default().fg(theme::DIM)),
        Span::raw(" "),
        Span::styled(marker, Style::default().fg(marker_color)),
        Span::raw(" "),
        Span::raw(line.text.clone()),
    ])
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
    let dim = Style::default().fg(theme::DIM);
    let (title, added, removed) = overlay.current_file().map_or_else(
        || ("(no file)".to_owned(), 0u32, 0u32),
        |f| (f.path.clone(), f.added_count(), f.removed_count()),
    );
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("DIFF", Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", dim),
        Span::styled(title, dim),
    ];
    if added > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("+{added}"), Style::default().fg(Color::Green)));
    }
    if removed > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("-{removed}"), Style::default().fg(Color::Red)));
    }
    Line::from(spans)
}

fn rule_row(width: u16) -> Line<'static> {
    Line::from(Span::styled("─".repeat(usize::from(width)), Style::default().fg(theme::DIM)))
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
        FileStatus::Typechange => ("T", theme::RUST_ORANGE),
        FileStatus::Unmerged => ("!", theme::STATUS_ERROR),
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

    // `rail_width_for` tests live next to the function definition in
    // `crate::app::diff_overlay::tests` — this module only tests the
    // renderer-local helpers below.

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
