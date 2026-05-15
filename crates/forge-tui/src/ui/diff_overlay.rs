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
//! `mouse.row` → `BodyRowKey`. Comment chips (💬 `L<line>`) render
//! one row each after their anchor line; the active editor's
//! TextArea expands inline below its anchor.

use forge_workspace::env::git_diff::hunks::{DiffLine, DiffLineKind, FileHunks, FileStatus, Hunk};

use crate::app::diff_overlay::{
    ActiveCommentInput, BODY_HEAD_ROWS, BodyRowKey, HunkComment, LineKey, RailRowKey,
    rail_width_for,
};
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
        // Need a mut borrow for the narrow-tier writeback (pane
        // geometry + body_keys). Re-borrow via app.diff_overlay
        // after the immutable peek above.
        render_narrow(frame, area, app);
        return;
    }

    // Reserve the bottom row of the overlay for the footer (key
    // hints + comment count). Without this, the body Paragraph
    // would paint into that row first and the footer would draw
    // on top, visually overwriting whatever diff content was the
    // last visible line.
    let usable_height = area.height.saturating_sub(1);
    let usable_area = Rect { x: area.x, y: area.y, width: area.width, height: usable_height };
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(rail_width), Constraint::Length(1), Constraint::Min(0)])
        .split(usable_area);
    let rail_area = chunks[0];
    let sep_area = chunks[1];
    let pane_area = chunks[2];

    // Short-circuit on a too-short pane: skip building the body
    // lines (allocating Vec<Line> + per-line spans only to drop
    // them is wasted work) and surface a "terminal too short"
    // notice so the user knows why the body is empty.
    if pane_area.height < 3 {
        render_rail(frame, rail_area, app);
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
            o.body_head_rows = 0;
            o.banner_close_col_range = None;
            o.narrow_header_row_y = None;
            o.narrow_arrow_cols = None;
        }
        return;
    }

    // Build the body line list up-front so we know its total
    // height; clamp body_scroll against (total - visible_tail) so
    // a wheel-past-end leaves a useful one-screen-of-tail visible.
    // Banner + rule + blank are PINNED — they don't scroll with the
    // body, so the diff target / per-file totals / ✕ close
    // affordance stay visible while the user pages through hunks.
    let (body_lines, body_keys, banner_close_range) = build_pane_lines(overlay, pane_area);
    let head_count = BODY_HEAD_ROWS.min(body_lines.len());
    let tail_count = body_lines.len().saturating_sub(head_count);
    let head_height = u16::try_from(head_count).unwrap_or(u16::MAX);
    let tail_height = pane_area.height.saturating_sub(head_height);
    let max_offset = tail_count.saturating_sub(usize::from(tail_height));
    let max_offset_u16 = u16::try_from(max_offset).unwrap_or(u16::MAX);
    let body_scroll = if let Some(overlay_mut) = app.diff_overlay.as_mut() {
        let clamped = overlay_mut.body_scroll.min(max_offset_u16);
        overlay_mut.body_scroll = clamped;
        overlay_mut.body_keys = body_keys;
        overlay_mut.body_head_rows = BODY_HEAD_ROWS;
        overlay_mut.pane_origin_row = pane_area.y;
        overlay_mut.pane_origin_col = pane_area.x;
        overlay_mut.pane_width = pane_area.width;
        overlay_mut.banner_close_col_range = banner_close_range;
        // Wide tier is mutually exclusive with narrow tier; clear
        // narrow-tier-only fields so a click on the wide layout
        // can't hit-test against stale narrow header coords.
        overlay_mut.narrow_header_row_y = None;
        overlay_mut.narrow_arrow_cols = None;
        clamped
    } else {
        0
    };

    render_rail(frame, rail_area, app);
    render_separator(frame, sep_area);
    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    // Split into pinned head + scrolling tail.
    let (head, tail) = body_lines.split_at(head_count);
    let head_rect =
        Rect { x: pane_area.x, y: pane_area.y, width: pane_area.width, height: head_height };
    let tail_rect = Rect {
        x: pane_area.x,
        y: pane_area.y.saturating_add(head_height),
        width: pane_area.width,
        height: tail_height,
    };
    frame.render_widget(Paragraph::new(head.to_vec()), head_rect);
    frame.render_widget(Paragraph::new(tail.to_vec()).scroll((body_scroll, 0)), tail_rect);
    render_footer(frame, area, overlay);
}

fn render_missing_state(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("Diff overlay opened without state. This is a bug — press Esc to return.")
            .style(Style::default().fg(theme::STATUS_ERROR)),
        area,
    );
}

/// Render the FILES rail as a box-drawing tree (mirroring the
/// Inspector GIT section's shape). Builds a tree from `overlay.files`,
/// folds single-child directory chains, then walks the tree to
/// emit one [`ratatui::text::Line`] per directory header and one per
/// file leaf. Writes the parallel `rail_keys` index + clamped
/// `rail_scroll` back to overlay state so the click handler can
/// resolve `mouse.row` → action without re-walking.
fn render_rail(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height < 3 {
        return;
    }
    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    let inner_width = usize::from(area.width.saturating_sub(4));

    // Build the full row list (banner + rule + blank + tree rows +
    // optional untracked notice). We materialise everything because
    // rail_scroll clamps against the total row count, and we need
    // the full Vec<Line> for ratatui's Paragraph anyway.
    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(overlay.files.len() + 6);
    let mut all_keys: Vec<RailRowKey> = Vec::with_capacity(overlay.files.len() + 6);
    all_lines.push(banner_row("FILES"));
    all_keys.push(RailRowKey::Banner);
    all_lines.push(rule_row(area.width));
    all_keys.push(RailRowKey::Rule);
    all_lines.push(Line::default());
    all_keys.push(RailRowKey::Blank);

    let tree = build_rail_tree(&overlay.files);
    walk_rail_tree(&tree, "", true, &mut all_lines, &mut all_keys, overlay, inner_width);

    if overlay.untracked_suppressed > 0 {
        // Surface the cap overflow so a fresh-repo state with many
        // untracked files doesn't render identically to a clean
        // tree. Yellow signals "suppressed work-product, not a
        // failure" — matches the Untracked status glyph colour.
        all_lines.push(Line::from(Span::styled(
            format!(
                "  +{} untracked suppressed (cap {})",
                overlay.untracked_suppressed,
                forge_workspace::env::git_diff::hunks::MAX_UNTRACKED_FILES,
            ),
            Style::default().fg(theme::STATUS_WARNING),
        )));
        all_keys.push(RailRowKey::UntrackedNotice);
    }

    // The first 3 rows (banner / rule / blank) don't scroll. Whatever
    // remains scrolls; clamp `rail_scroll` so wheel-past-end leaves
    // the tail visible. The Paragraph's `scroll((y, 0))` works on
    // the full Vec, so we pass it the offset directly.
    let scrollable_rows = all_lines.len().saturating_sub(3);
    let visible = usize::from(area.height.saturating_sub(3));
    let max_offset = scrollable_rows.saturating_sub(visible);
    let max_offset_u16 = u16::try_from(max_offset).unwrap_or(u16::MAX);
    let scroll = if let Some(o) = app.diff_overlay.as_mut() {
        let clamped = o.rail_scroll.min(max_offset_u16);
        o.rail_scroll = clamped;
        o.rail_keys = all_keys;
        clamped
    } else {
        0
    };

    // Scroll-aware Paragraph: head (banner+rule+blank) stays pinned,
    // the rest scrolls. ratatui's Paragraph.scroll((y, 0)) scrolls
    // the WHOLE content, which would hide the banner. Instead, paint
    // the head pinned at the top of `area`, then a scroll-aware
    // sub-area for the remainder.
    let head_rect = Rect { x: area.x, y: area.y, width: area.width, height: 3 };
    let body_rect =
        Rect { x: area.x, y: area.y + 3, width: area.width, height: area.height.saturating_sub(3) };
    let (head, tail) = all_lines.split_at(3.min(all_lines.len()));
    frame.render_widget(Paragraph::new(head.to_vec()), head_rect);
    frame.render_widget(Paragraph::new(tail.to_vec()).scroll((scroll, 0)), body_rect);
}

/// Tree node for the diff overlay's FILES rail. Mirrors the
/// Inspector GIT section's tree pattern: directories are inner
/// nodes (no leaf data), files are leaves carrying their
/// `file_idx` (so the click handler can resolve the row back to
/// `overlay.files[idx]`) + status + comment count.
#[derive(Debug)]
struct RailNode {
    label: String,
    leaf: Option<RailLeaf>,
    children: Vec<RailNode>,
}

#[derive(Debug)]
struct RailLeaf {
    file_idx: usize,
    status: FileStatus,
}

impl RailNode {
    fn is_dir(&self) -> bool {
        self.leaf.is_none()
    }
}

fn build_rail_tree(files: &[FileHunks]) -> RailNode {
    let mut root = RailNode { label: String::new(), leaf: None, children: Vec::new() };
    for (file_idx, file) in files.iter().enumerate() {
        insert_rail_file(&mut root, &file.path, file_idx, file.status);
    }
    sort_rail_tree(&mut root);
    fold_rail_chains(&mut root);
    root
}

fn insert_rail_file(node: &mut RailNode, path: &str, file_idx: usize, status: FileStatus) {
    let mut comps = path.split('/').filter(|c| !c.is_empty());
    let Some(first) = comps.next() else { return };
    let rest: Vec<&str> = comps.collect();
    if rest.is_empty() {
        node.children.push(RailNode {
            label: first.to_owned(),
            leaf: Some(RailLeaf { file_idx, status }),
            children: Vec::new(),
        });
        return;
    }
    let dir_idx =
        node.children.iter().position(|c| c.is_dir() && c.label == first).unwrap_or_else(|| {
            node.children.push(RailNode {
                label: first.to_owned(),
                leaf: None,
                children: Vec::new(),
            });
            node.children.len() - 1
        });
    let remainder = rest.join("/");
    insert_rail_file(&mut node.children[dir_idx], &remainder, file_idx, status);
}

fn sort_rail_tree(node: &mut RailNode) {
    node.children.sort_by(|a, b| match (a.is_dir(), b.is_dir()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.label.cmp(&b.label),
    });
    for c in &mut node.children {
        sort_rail_tree(c);
    }
}

/// Collapse single-child directory chains so `crates/forge-tui/src/app/`
/// renders as one label instead of four nested levels.
fn fold_rail_chains(node: &mut RailNode) {
    for c in &mut node.children {
        fold_rail_chains(c);
    }
    for c in &mut node.children {
        while c.is_dir() && c.children.len() == 1 && c.children[0].is_dir() {
            let mut only = c.children.remove(0);
            c.label.push('/');
            c.label.push_str(&only.label);
            c.children = std::mem::take(&mut only.children);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_rail_tree(
    node: &RailNode,
    prefix: &str,
    is_top_level: bool,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<RailRowKey>,
    overlay: &DiffOverlayState,
    inner_width: usize,
) {
    let count = node.children.len();
    for (idx, child) in node.children.iter().enumerate() {
        let is_last = idx + 1 == count;
        emit_rail_node(child, prefix, is_top_level, is_last, lines, keys, overlay, inner_width);
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_rail_node(
    node: &RailNode,
    prefix: &str,
    is_top_level: bool,
    is_last: bool,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<RailRowKey>,
    overlay: &DiffOverlayState,
    inner_width: usize,
) {
    let connector = if is_top_level {
        ""
    } else if is_last {
        "\u{2514}\u{2500} "
    } else {
        "\u{251c}\u{2500} "
    };
    let line_prefix = format!("{prefix}{connector}");
    match node.leaf.as_ref() {
        None => {
            // Directory — append trailing `/` so the eye reads it as
            // a folder, not a file with no extension.
            lines.push(rail_directory_row(&line_prefix, &node.label, inner_width));
            keys.push(RailRowKey::Directory);
        }
        Some(leaf) => {
            let is_current = overlay.current_file_idx == leaf.file_idx;
            let comment_count = overlay.comment_counts.get(leaf.file_idx).copied().unwrap_or(0);
            lines.push(rail_file_row(
                &line_prefix,
                &node.label,
                leaf.status,
                is_current,
                comment_count,
                inner_width,
            ));
            keys.push(RailRowKey::File { file_idx: leaf.file_idx });
        }
    }
    if node.children.is_empty() {
        return;
    }
    let continuation = if is_top_level {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}\u{2502}  ")
    };
    let count = node.children.len();
    for (idx, child) in node.children.iter().enumerate() {
        let child_is_last = idx + 1 == count;
        emit_rail_node(
            child,
            &continuation,
            false,
            child_is_last,
            lines,
            keys,
            overlay,
            inner_width,
        );
    }
}

fn rail_directory_row(line_prefix: &str, label: &str, _inner_width: usize) -> Line<'static> {
    let dim = Style::default().fg(theme::DIM);
    Line::from(vec![
        Span::raw("  "),
        Span::styled(line_prefix.to_owned(), dim),
        Span::styled(format!("{label}/"), dim),
    ])
}

fn rail_file_row(
    line_prefix: &str,
    label: &str,
    status: FileStatus,
    is_current: bool,
    comment_count: u32,
    inner_width: usize,
) -> Line<'static> {
    let dim = Style::default().fg(theme::DIM);
    let (status_glyph, status_color) = status_glyph(status);
    let marker = if is_current { "▸" } else { " " };
    let marker_color = if is_current { theme::RUST_ORANGE } else { theme::DIM };
    let prefix_chars = line_prefix.chars().count();
    // Layout: "  " + tree_prefix + marker + " " + status + "  " + label + " " + chip
    // The chip lands at the right edge if it fits.
    let chip_str = if comment_count > 0 { format!("💬 {comment_count}") } else { String::new() };
    let chip_chars = if comment_count > 0 { chip_str.chars().count() + 2 } else { 0 };
    let fixed_chars = 1 + 1 + 1 + 2; // marker + " " + status_glyph + "  "
    let label_budget = inner_width
        .saturating_sub(prefix_chars)
        .saturating_sub(fixed_chars)
        .saturating_sub(chip_chars);
    let fitted = truncate_path_front(label, label_budget.max(1));
    Line::from(vec![
        Span::raw("  "),
        Span::styled(line_prefix.to_owned(), dim),
        Span::styled(marker.to_owned(), Style::default().fg(marker_color)),
        Span::raw(" "),
        Span::styled(status_glyph.to_owned(), Style::default().fg(status_color)),
        Span::raw("  "),
        Span::raw(fitted),
        if comment_count > 0 {
            Span::styled(format!("  {chip_str}"), Style::default().fg(theme::RUST_ORANGE))
        } else {
            Span::raw("")
        },
    ])
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
    let footer_rect = Rect { x: area.x, y: area.y + area.height - 1, width: area.width, height: 1 };
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

/// Build the right pane's body lines (banner + rule + per-hunk
/// content). Lifted out of the renderer so the top-level `render`
/// can compute total height and clamp `body_scroll` before drawing.
/// Returns the lines + a parallel `BodyRowKey` vector indexed by
/// row offset — the click handler reads it to resolve a mouse y
/// coordinate into an action.
fn build_pane_lines(
    overlay: &DiffOverlayState,
    area: Rect,
) -> (Vec<Line<'static>>, Vec<BodyRowKey>, Option<(u16, u16)>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut keys: Vec<BodyRowKey> = Vec::new();
    let (banner_line, banner_close_range) = pane_banner_row(overlay, area.x, area.width);
    lines.push(banner_line);
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
        return (lines, keys, banner_close_range);
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
                    if let Some(input) = overlay.active_input.as_ref()
                        && input.key == line_key
                    {
                        render_active_input(input, gutter_width, &mut lines, &mut keys);
                    }
                }
            }
        }
    }

    (lines, keys, banner_close_range)
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

/// Max characters the chip summary holds before truncation. Beyond
/// this the chip would either wrap or push the rail too wide; 72
/// matches GitHub's comment-collapsed preview length.
const CHIP_SUMMARY_MAX: usize = 72;

/// Trim multi-line comment text to a single-line summary for the
/// chip. Keeps the chip rail one row tall regardless of edit length.
fn first_line_summary(text: &str) -> String {
    let line = text.lines().next().unwrap_or("");
    if line.chars().count() <= CHIP_SUMMARY_MAX {
        line.to_owned()
    } else {
        let truncated: String = line.chars().take(CHIP_SUMMARY_MAX.saturating_sub(1)).collect();
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

/// Background tint for added lines — dark green matching GitHub's
/// dark-mode added-line surface.
const ADDED_BG: Color = Color::Rgb(3, 58, 22);
/// Background tint for removed lines — dark red matching GitHub's
/// dark-mode removed-line surface.
const REMOVED_BG: Color = Color::Rgb(103, 6, 12);

fn diff_line_row(line: &DiffLine, gutter_width: usize) -> Line<'static> {
    let (marker, marker_color, line_bg) = match line.kind {
        DiffLineKind::Added => ("+", Color::Green, Some(ADDED_BG)),
        DiffLineKind::Removed => ("-", Color::Red, Some(REMOVED_BG)),
        DiffLineKind::Context => (" ", theme::DIM, None),
    };
    let line_num = match line.kind {
        DiffLineKind::Added | DiffLineKind::Context => line.new_line,
        DiffLineKind::Removed => line.old_line,
    };
    let gutter = match line_num {
        Some(n) => format!("{n:>gutter_width$}"),
        None => " ".repeat(gutter_width),
    };
    // Per-line background tint mimics GitHub's added/removed surfaces.
    // The whole text-side of the row carries the tint (including the
    // marker and a trailing space); the leading indent + gutter stay
    // on the default background so the eye sees the change boundary
    // start exactly where the marker does.
    // `line.text.clone()` is a per-frame cost flagged but acceptable
    // — bounded by hunk size and dominated by per-frame layout cost.
    let marker_style = match line_bg {
        Some(bg) => Style::default().fg(marker_color).bg(bg),
        None => Style::default().fg(marker_color),
    };
    let text_style = match line_bg {
        Some(bg) => Style::default().bg(bg),
        None => Style::default(),
    };
    Line::from(vec![
        Span::raw("  "),
        Span::styled(gutter, Style::default().fg(theme::DIM)),
        Span::raw(" "),
        Span::styled(marker, marker_style),
        Span::styled(" ", text_style),
        Span::styled(line.text.clone(), text_style),
    ])
}

/// Narrow-tier renderer (terminal width < 120): drops the rail and
/// renders just the body, with a one-line header carrying the
/// current file's path + `◀ N/M ▶` cycle controls. Clicks on the
/// arrows advance / retreat `current_file_idx`; clicks on body
/// lines open the comment editor as in the wide tier. The mouse
/// handler at `app::diff_overlay::handle_narrow_arrow_click` reads
/// the arrow positions stashed during this render.
///
/// Takes `&mut App` so the renderer can write the pane geometry
/// (`pane_origin_*`, `pane_width`) and the parallel `body_keys`
/// index back to overlay state — without this writeback, the
/// click hit-tester finds an empty `body_keys` and silently no-ops.
fn render_narrow(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height < 3 {
        // Too small even for the narrow header; render a notice and
        // bail. Clear body_keys + geometry so a click during this
        // state can't hit-test against stale wide-tier values.
        frame.render_widget(
            Paragraph::new("Terminal too small — resize and re-open /diff.")
                .style(Style::default().fg(theme::STATUS_WARNING)),
            area,
        );
        if let Some(o) = app.diff_overlay.as_mut() {
            o.body_keys.clear();
            o.pane_origin_row = area.y;
            o.pane_origin_col = area.x;
            o.pane_width = area.width;
            // Clear all geometry-stamped fields so a click at the
            // prior arrow / banner / chip coordinates can't advance
            // state against a "Terminal too small" notice.
            o.narrow_header_row_y = None;
            o.narrow_arrow_cols = None;
            o.banner_close_col_range = None;
        }
        return;
    }
    let header_rect = Rect { x: area.x, y: area.y, width: area.width, height: 1 };
    let rule_rect = Rect { x: area.x, y: area.y + 1, width: area.width, height: 1 };
    let body_rect =
        Rect { x: area.x, y: area.y + 2, width: area.width, height: area.height.saturating_sub(3) };

    // Build the header and stash arrow column positions so the
    // click handler can hit-test them. Done before any other
    // rendering because the writeback needs the same `&mut App`.
    let Some(overlay_ref) = app.diff_overlay.as_ref() else { return };
    let (header_line, arrow_cols) = narrow_header_row(overlay_ref, area.x);
    // Narrow tier has no wide-style banner ✕, so build_pane_lines'
    // banner_close_range is unused — the narrow header has its own
    // close affordance (none in v1; users press Esc).
    let (mut body_lines, mut body_keys, _) = build_pane_lines(overlay_ref, body_rect);

    // Strip the banner + rule + blank rows (the first 3 entries) so
    // the narrow body doesn't re-paint headers the narrow_header
    // already covered. Body keys must drop in lockstep. `drain` is
    // O(1) shift compared to repeated `remove(0)` which is O(n)
    // per call.
    let skip = body_lines.len().min(3);
    body_lines.drain(0..skip);
    body_keys.drain(0..skip);

    // Writeback geometry + body_keys + arrow positions. Body origin
    // is body_rect (not area) so click rows align with the body
    // line index after the stripped header. Clear wide-tier-only
    // `banner_close_col_range` so a click on the narrow layout
    // can't trigger close against stale wide-tier coords.
    if let Some(o) = app.diff_overlay.as_mut() {
        o.body_keys = body_keys;
        // Narrow stripped the banner+rule+blank head rows from
        // body_keys; every remaining row scrolls with body_scroll.
        o.body_head_rows = 0;
        o.pane_origin_row = body_rect.y;
        o.pane_origin_col = body_rect.x;
        o.pane_width = body_rect.width;
        o.narrow_header_row_y = Some(header_rect.y);
        o.narrow_arrow_cols = arrow_cols;
        o.banner_close_col_range = None;
    }

    frame.render_widget(Paragraph::new(header_line), header_rect);
    frame.render_widget(Paragraph::new(rule_row(area.width)), rule_rect);
    frame.render_widget(Paragraph::new(body_lines), body_rect);
    let Some(overlay_after) = app.diff_overlay.as_ref() else { return };
    render_footer(frame, area, overlay_after);
}

/// Build the narrow-tier header line and return the screen-column
/// positions of the `◀` and `▶` glyphs so the mouse handler can
/// hit-test clicks on them. Columns are absolute screen coordinates
/// (taking `area.x` into account). Returns `None` when no files
/// exist; the arrows still render but are no-op so positions aren't
/// exposed.
fn narrow_header_row(
    overlay: &DiffOverlayState,
    origin_col: u16,
) -> (Line<'static>, Option<(u16, u16)>) {
    let dim = Style::default().fg(theme::DIM);
    let total = overlay.files.len();
    let current = if total == 0 { 0 } else { overlay.current_file_idx + 1 };
    let path = overlay.current_file().map_or_else(|| "(no file)".to_owned(), |f| f.path.clone());
    // Track running column width so we know where ◀ and ▶ land.
    let prefix = "  DIFF · ";
    let prefix_width = u16::try_from(prefix.chars().count()).unwrap_or(u16::MAX);
    let path_width = u16::try_from(path.chars().count()).unwrap_or(u16::MAX);
    let gap_width: u16 = 2; // "  " between path and arrows
    let left_arrow_col = origin_col
        .saturating_add(prefix_width)
        .saturating_add(path_width)
        .saturating_add(gap_width);
    let counter = format!(" {current}/{total} ");
    let counter_width = u16::try_from(counter.chars().count()).unwrap_or(u16::MAX);
    let right_arrow_col = left_arrow_col.saturating_add(1).saturating_add(counter_width);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("DIFF", Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", dim),
        Span::styled(path, dim),
        Span::raw("  "),
        Span::styled("◀", Style::default().fg(theme::RUST_ORANGE)),
        Span::styled(counter, dim),
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
        if added > 0 || removed > 0 {
            // 1-cell trailing pad so totals don't touch the right
            // edge — matches the Inspector GIT panel convention.
            spans.push(Span::raw(" "));
        }
    }
    let arrow_cols = if total > 0 { Some((left_arrow_col, right_arrow_col)) } else { None };
    (Line::from(spans), arrow_cols)
}

fn banner_row(label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(label, Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
    ])
}

/// Build the wide-tier pane banner line + return the column range
/// where the trailing `✕` glyph sits relative to the pane's left
/// edge. The mouse handler reads this range to gate banner-close
/// clicks: if the banner clipped past `pane_width` (long path +
/// totals consumed the budget), returns `None` and the click
/// handler refuses banner-row clicks rather than treating
/// arbitrary clipped path text as close intent.
///
/// `pane_origin_col` is the absolute screen column where the pane
/// starts; returned range is in absolute screen coordinates.
fn pane_banner_row(
    overlay: &DiffOverlayState,
    pane_origin_col: u16,
    pane_width: u16,
) -> (Line<'static>, Option<(u16, u16)>) {
    let dim = Style::default().fg(theme::DIM);
    let (title, added, removed) = overlay.current_file().map_or_else(
        || ("(no file)".to_owned(), 0u32, 0u32),
        |f| (f.path.clone(), f.added_count(), f.removed_count()),
    );
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("DIFF", Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
        Span::styled(" · ", dim),
        Span::styled(title.clone(), dim),
    ];
    if added > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("+{added}"), Style::default().fg(Color::Green)));
    }
    if removed > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("-{removed}"), Style::default().fg(Color::Red)));
    }
    // Compute where ✕ would land: "  DIFF · <path>" prefix +
    // "  +N" + " -M" when present. Done before pushing the close
    // spans so the geometry math doesn't have to walk Span widths.
    let prefix_chars = 2 + 4 + 3; // "  " + "DIFF" + " · "
    let path_chars = title.chars().count();
    let plus_chars = if added > 0 { 2 + 1 + count_digits(added) } else { 0 }; // "  " + "+" + digits
    let minus_chars = if removed > 0 { 1 + 1 + count_digits(removed) } else { 0 }; // " " + "-" + digits
    let close_pad_chars = 2; // "  " before ✕
    let close_glyph_chars = 1; // ✕
    let consumed_before_close =
        prefix_chars + path_chars + plus_chars + minus_chars + close_pad_chars;
    let close_start_col =
        pane_origin_col.saturating_add(u16::try_from(consumed_before_close).unwrap_or(u16::MAX));
    let close_end_col =
        close_start_col.saturating_add(u16::try_from(close_glyph_chars).unwrap_or(1));
    let pane_end_col = pane_origin_col.saturating_add(pane_width);
    // Always push the close spans — they may clip but the column
    // budget check below decides whether the click handler trusts
    // the glyph position. Keeping the spans means the user sees
    // the ✕ when the terminal is wide enough. Trailing 1-cell pad
    // matches the Inspector GIT panel convention so banner totals
    // / close glyph don't touch the pane's right edge.
    spans.push(Span::raw("  "));
    spans.push(Span::styled("✕", Style::default().fg(theme::DIM)));
    spans.push(Span::raw(" "));
    let visible_range =
        if close_end_col <= pane_end_col { Some((close_start_col, close_end_col)) } else { None };
    (Line::from(spans), visible_range)
}

fn count_digits(mut n: u32) -> usize {
    if n == 0 {
        return 1;
    }
    let mut digits = 0;
    while n > 0 {
        digits += 1;
        n /= 10;
    }
    digits
}

fn rule_row(width: u16) -> Line<'static> {
    Line::from(Span::styled("─".repeat(usize::from(width)), Style::default().fg(theme::DIM)))
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
