//! Renderer for [`crate::app::ActiveView::Diff`].
//!
//! Full-screen takeover triggered by `/diff` or the Inspector GIT
//! `⤢` click. Two-pane layout with chrome mirroring the Projects
//! pane: FILES rail on the left (banner + DIM rule + 2-col content
//! indent), DIFF body on the right (sibling banner showing the
//! currently-viewed file's path + per-file `+N -M` totals when
//! non-zero, same rule pattern). The two rules sit at the same
//! y-position so the `│` separator interrupts what visually reads
//! as one continuous line.
//!
//! Click-to-comment lands here: `body_keys` is the parallel
//! per-rendered-row index the click handler reads to resolve
//! `mouse.row` → `BodyRowKey`. Comment chips (💬 L<line>) render
//! one row each after their anchor line; the active editor's
//! TextArea expands inline below its anchor.

use forge_workspace::env::git_diff::hunks::{DiffLine, DiffLineKind, FileHunks, FileStatus, Hunk};

use crate::app::diff_overlay::{ActiveCommentInput, BodyRowKey, HunkComment, LineKey, rail_width_for};
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
        // Stash geometry even on the too-short path so a click that
        // races a resize back-up doesn't read stale dimensions.
        if let Some(o) = app.diff_overlay.as_mut() {
            o.pane_origin_row = pane_area.y;
            o.pane_origin_col = pane_area.x;
            o.pane_width = pane_area.width;
            o.body_keys.clear();
        }
        return;
    }

    // Build the body line list up-front so we know its total
    // height; clamp body_scroll against (total - visible) so a
    // wheel-past-end leaves a useful one-screen-of-tail visible
    // instead of a blank pane. Writeback to overlay state keeps
    // the wheel handler in sync with whatever the renderer last
    // saw, and stashes the parallel BodyRowKey list + pane geometry
    // for the mouse hit-tester.
    let (body_lines, body_keys) = build_pane_lines(overlay, pane_area);
    let max_offset = body_lines.len().saturating_sub(usize::from(pane_area.height));
    let max_offset_u16 = u16::try_from(max_offset).unwrap_or(u16::MAX);
    let body_scroll = if let Some(overlay_mut) = app.diff_overlay.as_mut() {
        let clamped = overlay_mut.body_scroll.min(max_offset_u16);
        overlay_mut.body_scroll = clamped;
        overlay_mut.body_keys = body_keys;
        overlay_mut.pane_origin_row = pane_area.y;
        overlay_mut.pane_origin_col = pane_area.x;
        overlay_mut.pane_width = pane_area.width;
        clamped
    } else {
        0
    };

    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    render_rail(frame, rail_area, overlay);
    render_separator(frame, sep_area);
    frame.render_widget(Paragraph::new(body_lines).scroll((body_scroll, 0)), pane_area);
    render_footer(frame, area, overlay);
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
    // Banner + rule + blank consume 3 rows; the file list scrolls
    // through whatever remains. `rail_scroll` advances the visible
    // window; the click handler reads it for the inverse mapping.
    let visible = usize::from(area.height.saturating_sub(3));
    let max_offset = overlay.files.len().saturating_sub(visible);
    let max_offset_u16 = u16::try_from(max_offset).unwrap_or(u16::MAX);
    let scroll = overlay.rail_scroll.min(max_offset_u16);
    let mut lines = Vec::with_capacity(visible + 5);
    lines.push(banner_row("FILES"));
    lines.push(rule_row(area.width));
    lines.push(Line::default());
    let start = usize::from(scroll);
    let end = (start + visible).min(overlay.files.len());
    let comment_count_per_file = comment_counts_per_file(overlay);
    for idx in start..end {
        let file = &overlay.files[idx];
        let comments = comment_count_per_file.get(idx).copied().unwrap_or(0);
        lines.push(file_rail_row(
            file,
            idx == overlay.current_file_idx,
            inner_width,
            comments,
        ));
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

/// Render a one-line footer along the bottom edge showing the
/// pending comment count + key hints. Painted over the last row of
/// `area`; the body Paragraph occupies the same row but Paragraph's
/// last-line clipping inside a Layout still produces overlap-free
/// output because we paint after it.
fn render_footer(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState) {
    if area.height < 2 {
        return;
    }
    let footer_rect = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    let count = overlay.comments.len();
    let dim = Style::default().fg(theme::DIM);
    let mut spans = vec![Span::raw("  ")];
    if count > 0 {
        spans.push(Span::styled(
            format!("{count} comment{}", if count == 1 { "" } else { "s" }),
            Style::default().fg(theme::RUST_ORANGE),
        ));
        spans.push(Span::styled(" pending  ·  ", dim));
    }
    if overlay.active_input.is_some() {
        spans.push(Span::styled("Enter ", dim));
        spans.push(Span::styled("save", Style::default().fg(theme::RUST_ORANGE)));
        spans.push(Span::styled("   ·  ", dim));
        spans.push(Span::styled("Esc ", dim));
        spans.push(Span::styled("cancel input", Style::default().fg(theme::RUST_ORANGE)));
    } else {
        spans.push(Span::styled("click a diff line ", dim));
        spans.push(Span::styled("to comment", Style::default().fg(theme::RUST_ORANGE)));
        spans.push(Span::styled("   ·  ", dim));
        if count > 0 {
            spans.push(Span::styled("Esc ", dim));
            spans.push(Span::styled("submit & close", Style::default().fg(theme::RUST_ORANGE)));
        } else {
            spans.push(Span::styled("Esc ", dim));
            spans.push(Span::styled("close", Style::default().fg(theme::RUST_ORANGE)));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), footer_rect);
}

/// Count comments anchored at each file index. Used by the rail to
/// render a `💬 N` badge next to file rows with pending comments.
fn comment_counts_per_file(overlay: &DiffOverlayState) -> Vec<u32> {
    let mut counts = vec![0u32; overlay.files.len()];
    for c in &overlay.comments {
        if let Some(slot) = counts.get_mut(c.key.file_idx) {
            *slot = slot.saturating_add(1);
        }
    }
    counts
}

/// Build the right pane's body lines (banner + rule + per-hunk
/// content). Lifted out of the renderer so the top-level `render`
/// can compute total height and clamp `body_scroll` before drawing.
/// Returns the lines + a parallel `BodyRowKey` vector indexed by
/// row offset — the click handler reads it to resolve a mouse y
/// coordinate into an action.
fn build_pane_lines(
    overlay: &DiffOverlayState,
    area: Rect,
) -> (Vec<Line<'static>>, Vec<BodyRowKey>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut keys: Vec<BodyRowKey> = Vec::new();
    lines.push(pane_banner_row(overlay));
    keys.push(BodyRowKey::Banner);
    lines.push(rule_row(area.width));
    keys.push(BodyRowKey::Rule);
    lines.push(Line::default());
    keys.push(BodyRowKey::Blank);

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
        // chat. Spell out `target: agent.env_git` because that's the
        // actual tracing-target string an operator would grep for —
        // `ENV_GIT` was the const identifier, which doesn't match
        // anything in the log stream.
        lines.push(Line::from(Span::styled(
            format!(
                "  Scan failed for `{}` — see tracing logs (target: agent.env_git). Press Esc to retry.",
                overlay.target,
            ),
            Style::default().fg(theme::STATUS_ERROR),
        )));
        keys.push(BodyRowKey::EmptyState);
        return (lines, keys);
    }
    match overlay.current_file() {
        None => {
            lines.push(Line::from(Span::styled(
                "  (no file selected)",
                Style::default().fg(theme::DIM),
            )));
            keys.push(BodyRowKey::EmptyState);
        }
        Some(file) if file.hunks.is_empty() => {
            // An Untracked file with no hunks comes from one of
            // the scan_untracked drop paths (size-cap exceeded,
            // non-regular file, IO error) — all of which log WARN
            // under the agent.env_git tracing target. The
            // tracked-file case is a real binary diff from git.
            // Differentiate so the user knows whether to grep logs
            // vs accept the answer.
            let message = if file.status == FileStatus::Untracked {
                "  (untracked, content not surfaced — see logs (target: agent.env_git))"
            } else {
                "  (binary file or no diff content)"
            };
            lines.push(Line::from(Span::styled(message, Style::default().fg(theme::DIM))));
            keys.push(BodyRowKey::EmptyState);
        }
        Some(file) => {
            let file_idx = overlay.current_file_idx;
            let gutter_width = gutter_width_for(file);
            // Pre-index saved comments by line key so chip rendering
            // is O(1) per line instead of O(comments) per line. Net
            // win once comments > 4 or so; harmless at 0.
            let comments_by_key = index_comments_by_key(&overlay.comments);
            for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
                if hunk_idx > 0 {
                    lines.push(Line::default());
                    keys.push(BodyRowKey::Blank);
                }
                lines.push(hunk_header_row(hunk));
                keys.push(BodyRowKey::HunkHeader { file_idx, hunk_idx });
                for (line_idx, diff_line) in hunk.lines.iter().enumerate() {
                    let line_key = LineKey { file_idx, hunk_idx, line_idx };
                    lines.push(diff_line_row(diff_line, gutter_width));
                    keys.push(BodyRowKey::HunkLine(line_key));
                    // After this line: render any saved comment chip
                    // for it, then the active editor if it's anchored
                    // here. Both happen on the SAME line key so the
                    // chip is what the user sees as the saved-and-
                    // closed view.
                    if let Some(c) = comments_by_key.get(&line_key) {
                        lines.push(comment_chip_row(c, gutter_width));
                        keys.push(BodyRowKey::CommentChip(line_key));
                    }
                    if let Some(input) = overlay.active_input.as_ref() {
                        if input.key == line_key {
                            render_active_input(input, gutter_width, &mut lines, &mut keys);
                        }
                    }
                }
            }
        }
    }

    (lines, keys)
}

/// Index `comments` by `LineKey` for O(1) chip lookup during row
/// emission. Used only inside `build_pane_lines`.
fn index_comments_by_key(
    comments: &[HunkComment],
) -> std::collections::HashMap<LineKey, &HunkComment> {
    let mut map = std::collections::HashMap::with_capacity(comments.len());
    for c in comments {
        // Last-write-wins on duplicate keys (which shouldn't happen —
        // saving a comment on a line that already has one replaces
        // the existing entry — but stay defensive).
        map.insert(c.key, c);
    }
    map
}

fn comment_chip_row(comment: &HunkComment, gutter_width: usize) -> Line<'static> {
    let indent = " ".repeat(gutter_width + 4);
    let summary = first_line_summary(&comment.comment_text);
    Line::from(vec![
        Span::raw("  "),
        Span::raw(indent),
        Span::styled("💬 ", Style::default().fg(theme::RUST_ORANGE)),
        Span::styled(format!("L{} ", comment.line), Style::default().fg(theme::DIM)),
        Span::raw(summary),
    ])
}

/// Trim multi-line comment text to a single-line summary for the
/// chip. Keeps the chip rail one row tall regardless of edit length.
fn first_line_summary(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    const MAX: usize = 72;
    if line.chars().count() <= MAX {
        line.to_owned()
    } else {
        let truncated: String = line.chars().take(MAX.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

/// Render the active comment editor as 1+ inline rows. The TextArea
/// holds the source-of-truth text; we paint a static view of it
/// here because Paragraph can render strings, not widgets-inside-
/// widgets. Each visual row gets its own `BodyRowKey::InputRow` so
/// clicks anywhere on the editor surface still resolve to the
/// editor (click currently no-ops; future: move cursor).
fn render_active_input(
    input: &ActiveCommentInput,
    gutter_width: usize,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let indent = " ".repeat(gutter_width + 4);
    let editor_lines = input.editor.lines();
    if editor_lines.is_empty() {
        // Render an empty placeholder so the user sees where they're
        // typing even before the first keystroke.
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(indent.clone()),
            Span::styled("│ ", Style::default().fg(theme::RUST_ORANGE)),
            Span::styled(
                "(type your comment, Enter to save, Esc to cancel)",
                Style::default().fg(theme::DIM),
            ),
        ]));
        keys.push(BodyRowKey::InputRow(input.key));
        return;
    }
    for editor_line in editor_lines {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(indent.clone()),
            Span::styled("│ ", Style::default().fg(theme::RUST_ORANGE)),
            Span::raw(editor_line.clone()),
        ]));
        keys.push(BodyRowKey::InputRow(input.key));
    }
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

/// Narrow-tier renderer (terminal width < 120): drops the rail and
/// renders just the body, with a one-line header carrying the
/// current file's path + `◀ N/M ▶` cycle controls. Mouse-click on
/// the controls advances `current_file_idx`; key handlers can also
/// drive it. Footer keys mirror the wide-tier layout.
fn render_narrow(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState) {
    if area.height < 3 {
        // Too small even for the narrow header; render a notice and
        // bail.
        frame.render_widget(
            Paragraph::new("Terminal too small — resize and re-open /diff.")
                .style(Style::default().fg(theme::STATUS_WARNING)),
            area,
        );
        return;
    }
    let header_rect = Rect { x: area.x, y: area.y, width: area.width, height: 1 };
    let rule_rect = Rect { x: area.x, y: area.y + 1, width: area.width, height: 1 };
    let body_rect = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(3),
    };

    frame.render_widget(Paragraph::new(narrow_header_row(overlay)), header_rect);
    frame.render_widget(Paragraph::new(rule_row(area.width)), rule_rect);

    let (body_lines, body_keys) = build_pane_lines(overlay, body_rect);
    // Strip the banner + rule + blank rows (the first 3 entries) so
    // the narrow body doesn't re-paint headers the narrow_header
    // already covered.
    let mut body_lines = body_lines;
    let mut body_keys = body_keys;
    let skip = body_lines.len().min(3);
    for _ in 0..skip {
        body_lines.remove(0);
        body_keys.remove(0);
    }
    frame.render_widget(Paragraph::new(body_lines), body_rect);
    render_footer(frame, area, overlay);
}

fn narrow_header_row(overlay: &DiffOverlayState) -> Line<'static> {
    let dim = Style::default().fg(theme::DIM);
    let total = overlay.files.len();
    let current = if total == 0 { 0 } else { overlay.current_file_idx + 1 };
    let path = overlay
        .current_file()
        .map_or_else(|| "(no file)".to_owned(), |f| f.path.clone());
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("DIFF", Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", dim),
        Span::styled(path, dim),
        Span::raw("  "),
        Span::styled("◀", Style::default().fg(theme::RUST_ORANGE)),
        Span::styled(format!(" {current}/{total} "), dim),
        Span::styled("▶", Style::default().fg(theme::RUST_ORANGE)),
    ];
    if let Some(f) = overlay.current_file() {
        let added = f.added_count();
        let removed = f.removed_count();
        if added > 0 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(format!("+{added}"), Style::default().fg(Color::Green)));
        }
        if removed > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!("-{removed}"), Style::default().fg(Color::Red)));
        }
    }
    Line::from(spans)
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
    // Trailing close glyph — clicking it is the mouse equivalent of
    // Esc (submit + close on pending comments, plain close otherwise).
    spans.push(Span::raw("  "));
    spans.push(Span::styled("✕", Style::default().fg(theme::DIM)));
    Line::from(spans)
}

fn rule_row(width: u16) -> Line<'static> {
    Line::from(Span::styled("─".repeat(usize::from(width)), Style::default().fg(theme::DIM)))
}

fn file_rail_row(
    file: &FileHunks,
    current: bool,
    max_path_width: usize,
    comment_count: u32,
) -> Line<'static> {
    let (glyph_text, glyph_color) = status_glyph(file.status);
    let marker_glyph: &str = if current { "▸" } else { glyph_text };
    let marker_color = if current { theme::RUST_ORANGE } else { glyph_color };
    let path_width =
        if comment_count > 0 { max_path_width.saturating_sub(6) } else { max_path_width };
    let path = truncate_path_front(&file.path, path_width);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(marker_glyph.to_string(), Style::default().fg(marker_color)),
        Span::raw("  "),
        Span::raw(path),
    ];
    if comment_count > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("💬 {comment_count}"),
            Style::default().fg(theme::RUST_ORANGE),
        ));
    }
    Line::from(spans)
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

    #[test]
    fn first_line_summary_keeps_short_lines() {
        assert_eq!(first_line_summary("hello"), "hello");
    }

    #[test]
    fn first_line_summary_takes_first_line_only() {
        assert_eq!(first_line_summary("line one\nline two"), "line one");
    }

    #[test]
    fn first_line_summary_truncates_long_lines() {
        let long = "x".repeat(200);
        let s = first_line_summary(&long);
        assert_eq!(s.chars().count(), 72);
        assert!(s.ends_with('…'));
    }
}
