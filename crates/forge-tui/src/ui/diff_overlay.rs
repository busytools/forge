//! Renderer for [`crate::app::ActiveView::Diff`].
//!
//! Full-screen takeover triggered by `/diff` or the Inspector GIT
//! `🦉` click. Two-pane layout with chrome mirroring the Projects
//! pane: FILES rail on the left (banner + DIM rule + 2-col content
//! indent), DIFF body on the right (sibling banner showing the
//! currently-viewed file's path + per-file `+N -M` totals when
//! non-zero, same rule pattern). The two rules sit at the same
//! y-position so the `│` separator interrupts what visually reads
//! as one continuous line.
//!
//! The body itself renders a GitHub-style split (side-by-side) view:
//! left column is the OLD file (context + removed), right column is
//! the NEW file (context + added). Per-line syntax highlighting
//! attaches via [`crate::ui::highlight::LineHighlighter`], one
//! instance per (file, side) so multi-line constructs (strings,
//! block comments) carry state across hunk lines on the same side.
//!
//! Click-to-comment lands here: `body_keys` is the parallel
//! per-rendered-row index the click handler reads to resolve
//! `mouse.row` → `BodyRowKey`. The split-row variant carries both
//! column keys; the click handler picks left/right by comparing
//! the click column against the pane midpoint. Comment chips (💬
//! `L<line>`) render one row each after their anchor line; the
//! active editor's TextArea expands inline below its anchor.

mod pairing;

use forge_workspace::env::git_diff::hunks::{DiffLineKind, FileHunks, FileStatus, Hunk};

use crate::app::diff_overlay::{
    ActiveCommentInput, BODY_HEAD_ROWS, BodyRowKey, HunkComment, LineKey, RailRowKey,
    rail_width_for,
};
use pairing::{PairedDiffRow, pair_hunk_lines};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::app::diff_overlay::DiffOverlayState;
use crate::ui::highlight::LineHighlighter;
use crate::ui::theme;

/// Minimum terminal width to render the split view. Below this we
/// show a "resize" notice — the body needs room for the rail plus
/// two columns of readable code, and squeezing harder loses more
/// than it saves.
const MIN_WIDTH_FOR_SPLIT: u16 = 100;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.cached_frame_area = area;

    let Some(overlay) = app.diff_overlay.as_ref() else {
        render_missing_state(frame, area);
        return;
    };

    if area.width < MIN_WIDTH_FOR_SPLIT {
        render_too_narrow_notice(frame, area, app);
        return;
    }
    let rail_width = rail_width_for(area.width);
    if rail_width == 0 {
        // Defensive: `rail_width_for` returning 0 implies the area is
        // narrower than MIN_WIDTH_FOR_SPLIT would already have caught,
        // but keep the bail for safety.
        render_too_narrow_notice(frame, area, app);
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
        }
        return;
    }

    // Build the body line list up-front so we know its total
    // height; clamp body_scroll against (total - visible_tail) so
    // a wheel-past-end leaves a useful one-screen-of-tail visible.
    // Banner + rule + blank are PINNED — they don't scroll with the
    // body, so the diff target / per-file totals stay visible while
    // the user pages through hunks.
    let (body_lines, body_keys) = build_pane_lines(overlay, pane_area);
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
        spans.push(Span::styled("← / →  ", dim));
        spans.push(Span::styled("scroll", Style::default().fg(theme::RUST_ORANGE)));
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
            // One highlighter per side so multi-line constructs
            // (strings, block comments) carry state correctly within
            // each column independently of the other.
            let mut left_hl = LineHighlighter::for_path(&file.path);
            let mut right_hl = LineHighlighter::for_path(&file.path);
            for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
                if hunk_idx > 0 {
                    lines.push(Line::default());
                    keys.push(BodyRowKey::Blank);
                }
                lines.push(hunk_header_row(hunk));
                keys.push(BodyRowKey::HunkHeader { file_idx, hunk_idx });
                let pairs = pair_hunk_lines(file_idx, hunk_idx, &hunk.lines);
                for pair in pairs {
                    let row = split_diff_row(
                        file,
                        pair,
                        gutter_width,
                        area.width,
                        usize::from(overlay.body_scroll_x),
                        &mut left_hl,
                        &mut right_hl,
                    );
                    lines.push(row);
                    keys.push(BodyRowKey::HunkRow { left: pair.left, right: pair.right });
                    // Emit chip + active editor for each side that
                    // has one anchored on it. Context lines point
                    // both halves at the same LineKey, so dedupe
                    // before iterating to avoid duplicate chips.
                    let mut sides: Vec<LineKey> = Vec::new();
                    if let Some(k) = pair.left {
                        sides.push(k);
                    }
                    if let Some(k) = pair.right
                        && Some(k) != pair.left
                    {
                        sides.push(k);
                    }
                    for side_key in sides {
                        if let Some(c) = comments_by_key.get(&side_key) {
                            render_comment_chip(
                                c,
                                side_key,
                                gutter_width,
                                area.width,
                                &mut lines,
                                &mut keys,
                            );
                        }
                        if let Some(input) = overlay.active_input.as_ref()
                            && input.key == side_key
                        {
                            let diff_line = &file.hunks[side_key.hunk_idx].lines[side_key.line_idx];
                            let anchor_line = match diff_line.kind {
                                DiffLineKind::Removed => diff_line.old_line.unwrap_or(0),
                                DiffLineKind::Added | DiffLineKind::Context => {
                                    diff_line.new_line.unwrap_or(0)
                                }
                            };
                            render_active_input(
                                input,
                                gutter_width,
                                anchor_line,
                                area.width,
                                &mut lines,
                                &mut keys,
                            );
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

/// Render a saved comment as a bordered mini-box, mirroring the
/// active editor's dialog shape but smaller. Reading it as a box
/// instantly signals "annotation" — the previous single-line chip
/// (`💬 L<n> <text>`) blended into the surrounding diff context.
/// Multi-line comment text wraps into multiple body rows. All
/// emitted rows carry [`BodyRowKey::CommentChip`] so clicking
/// anywhere on the box reopens the editor.
///
/// ```text
///   ┌── 💬 Comment on line 371 ──────────┐
///   │ worth a TODO with the cleanup       │
///   │ deadline                            │
///   └─────────────────────────────────────┘
/// ```
/// Background tint for the comment-chip interior — very dark
/// warm-brown so the box reads as a contained annotation block
/// even when the right border slips off-screen. Picked to harmonize
/// with `RUST_ORANGE` borders without competing with the diff lines'
/// green/red tints.
const CHIP_BG: Color = Color::Rgb(35, 23, 10);

fn render_comment_chip(
    comment: &HunkComment,
    key: LineKey,
    gutter_width: usize,
    pane_width: u16,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let indent_cols = gutter_width + 4;
    let indent = " ".repeat(indent_cols);
    let left_offset = 2 + indent_cols;
    let right_pad = 2usize;
    let box_width = usize::from(pane_width).saturating_sub(left_offset + right_pad).max(20);
    let border_style = Style::default().fg(theme::RUST_ORANGE).bg(CHIP_BG);
    let body_style = Style::default().bg(CHIP_BG);

    // Top border with embedded title. Whole row carries CHIP_BG so
    // the entire box surface is tinted — eye reads it as one block
    // regardless of whether the rightmost cells (incl. `┐`) end up
    // clipped on a narrow viewport.
    //
    // Width math: `💬` is a 2-cell glyph but `chars().count()`
    // returns 1, so we adjust by +1 to get the title's true visual
    // column width. Without this the top border would land 1 cell
    // further right than the body's `│` border, making the box
    // look stepped.
    let title = format!(" 💬 Comment on line {} ", comment.line);
    let title_visual = title.chars().count() + 1; // +1 for 💬's 2nd cell
    let dash_after = box_width.saturating_sub(3 + title_visual + 1);
    let top = format!("┌──{title}{}┐", "─".repeat(dash_after));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(indent.clone()),
        Span::styled(top, border_style),
    ]));
    keys.push(BodyRowKey::CommentChip(key));

    // Body — wrap the comment_text into rows that fit the box's
    // inner width (`│ … │` consumes 4 cells of chrome). Keep the
    // wrap simple: break on the box width and on explicit newlines.
    let inner_width = box_width.saturating_sub(4);
    let wrapped = wrap_chip_body(&comment.comment_text, inner_width);
    for row in &wrapped {
        let row_chars = row.chars().count();
        let pad = inner_width.saturating_sub(row_chars);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(indent.clone()),
            Span::styled("│ ", border_style),
            Span::styled(row.clone(), body_style),
            Span::styled(" ".repeat(pad), body_style),
            Span::styled(" │", border_style),
        ]));
        keys.push(BodyRowKey::CommentChip(key));
    }

    // Bottom border.
    let bottom = format!("└{}┘", "─".repeat(box_width.saturating_sub(2)));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(indent),
        Span::styled(bottom, border_style),
    ]));
    keys.push(BodyRowKey::CommentChip(key));
}

/// Wrap `text` into rows that fit within `max_chars`, respecting
/// explicit newlines. A line longer than `max_chars` is chopped at
/// the boundary (no word-aware soft-wrap in v1; the use case is
/// short review notes where character-based wrap is fine).
fn wrap_chip_body(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for source_line in text.lines() {
        if source_line.is_empty() {
            out.push(String::new());
            continue;
        }
        let chars: Vec<char> = source_line.chars().collect();
        for chunk in chars.chunks(max_chars) {
            out.push(chunk.iter().collect::<String>());
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Render the active comment editor as a bordered box anchored
/// below the clicked diff line. Box title carries the anchor line
/// number; body rows mirror the TextArea's lines; a footer row
/// shows the key hints inside the box. Every row gets
/// `BodyRowKey::InputRow` so any click on the dialog resolves to
/// the editor (currently no-op; future: cursor positioning).
///
/// Box geometry:
/// ```text
///   ┌── Comment on line 371 ──────────────────┐
///   │ user typed text here                    │
///   │ another line of the comment             │
///   │ Enter save · Esc cancel                 │  (DIM hint)
///   └─────────────────────────────────────────┘
/// ```
fn render_active_input(
    input: &ActiveCommentInput,
    gutter_width: usize,
    anchor_line: u32,
    pane_width: u16,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let indent_cols = gutter_width + 4;
    let indent = " ".repeat(indent_cols);
    // Leading "  " + indent on the left, 2-col right pad → box
    // width is whatever's left of the pane.
    let left_offset = 2 + indent_cols;
    let right_pad = 2usize;
    let box_width = usize::from(pane_width).saturating_sub(left_offset + right_pad).max(20);
    let orange = Style::default().fg(theme::RUST_ORANGE);
    let dim = Style::default().fg(theme::DIM);

    // Top border with embedded title.
    let title = format!(" Comment on line {anchor_line} ");
    let title_chars = title.chars().count();
    let dash_after = box_width.saturating_sub(1 + 2 + title_chars + 1);
    let top = format!("┌──{title}{}┐", "─".repeat(dash_after));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(indent.clone()),
        Span::styled(top, orange),
    ]));
    keys.push(BodyRowKey::InputRow(input.key));

    // Body rows — one per editor line. Empty editor shows a single
    // placeholder row so the user sees where typing will land.
    let inner_width = box_width.saturating_sub(2); // `│ … │`
    let editor_lines = input.editor.lines();
    let body_rows: Vec<String> =
        if editor_lines.is_empty() || editor_lines.iter().all(String::is_empty) {
            vec!["(type your comment)".to_owned()]
        } else {
            editor_lines.to_vec()
        };
    for body_row in &body_rows {
        let placeholder = body_rows.len() == 1 && body_row == "(type your comment)";
        let fitted = fit_box_content(body_row, inner_width.saturating_sub(2));
        let fitted_chars = fitted.chars().count();
        let pad = inner_width.saturating_sub(2).saturating_sub(fitted_chars);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(indent.clone()),
            Span::styled("│ ", orange),
            if placeholder { Span::styled(fitted, dim) } else { Span::raw(fitted) },
            Span::raw(" ".repeat(pad)),
            Span::styled(" │", orange),
        ]));
        keys.push(BodyRowKey::InputRow(input.key));
    }

    // In-box hint row (DIM).
    let hint = "Enter save · Esc cancel";
    let hint_fitted = fit_box_content(hint, inner_width.saturating_sub(2));
    let hint_chars = hint_fitted.chars().count();
    let hint_pad = inner_width.saturating_sub(2).saturating_sub(hint_chars);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(indent.clone()),
        Span::styled("│ ", orange),
        Span::styled(hint_fitted, dim),
        Span::raw(" ".repeat(hint_pad)),
        Span::styled(" │", orange),
    ]));
    keys.push(BodyRowKey::InputRow(input.key));

    // Bottom border.
    let bottom = format!("└{}┘", "─".repeat(box_width.saturating_sub(2)));
    lines.push(Line::from(vec![Span::raw("  "), Span::raw(indent), Span::styled(bottom, orange)]));
    keys.push(BodyRowKey::InputRow(input.key));
}

/// Trim `text` to fit within `max_chars` columns. Used by the
/// editor dialog body to keep rows from overflowing the box width.
/// Falls back to `…`-suffix when truncation is needed.
fn fit_box_content(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 1 {
        return "\u{2026}".to_owned();
    }
    let take = max_chars.saturating_sub(1);
    let truncated: String = text.chars().take(take).collect();
    format!("{truncated}\u{2026}")
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

/// Build one split-view body row: left column + divider + right
/// column. Each column carries `[gutter] [+/-] [highlighted text]`,
/// truncated to fit. Empty sides (unbalanced rows) render as blank
/// fillers so the divider position stays consistent down the body.
fn split_diff_row(
    file: &FileHunks,
    pair: PairedDiffRow,
    gutter_width: usize,
    pane_width: u16,
    scroll_cols: usize,
    left_hl: &mut LineHighlighter,
    right_hl: &mut LineHighlighter,
) -> Line<'static> {
    // Per-side body width: pane minus 2-col leading indent minus the
    // 3-col divider zone (space + '│' + space), then halved.
    let indent_cols: usize = 2;
    let divider_cols: usize = 3;
    let usable = usize::from(pane_width).saturating_sub(indent_cols).saturating_sub(divider_cols);
    let per_side_width = usable / 2;

    let left =
        build_split_half(file, pair.left, gutter_width, per_side_width, scroll_cols, left_hl);
    let right =
        build_split_half(file, pair.right, gutter_width, per_side_width, scroll_cols, right_hl);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(left.len() + right.len() + 4);
    spans.push(Span::raw("  "));
    spans.extend(left);
    spans.push(Span::raw(" "));
    spans.push(Span::styled("│", Style::default().fg(theme::DIM)));
    spans.push(Span::raw(" "));
    spans.extend(right);
    Line::from(spans)
}

/// Build one half (left or right) of a split row. `key` of `None`
/// means this side is blank — fill with spaces sized to match the
/// other side so columns stay aligned.
fn build_split_half(
    file: &FileHunks,
    key: Option<LineKey>,
    gutter_width: usize,
    text_width: usize,
    scroll_cols: usize,
    highlighter: &mut LineHighlighter,
) -> Vec<Span<'static>> {
    let Some(key) = key else {
        // Blank side: gutter padding + space + marker padding + space
        // + text padding. Total = gutter_width + 3 + text_width.
        let pad_width = gutter_width.saturating_add(3).saturating_add(text_width);
        return vec![Span::raw(" ".repeat(pad_width))];
    };
    let line = &file.hunks[key.hunk_idx].lines[key.line_idx];
    let (marker, marker_color, bg) = marker_for_kind(line.kind);
    let line_num = match line.kind {
        DiffLineKind::Added | DiffLineKind::Context => line.new_line,
        DiffLineKind::Removed => line.old_line,
    };
    let gutter = match line_num {
        Some(n) => format!("{n:>gutter_width$}"),
        None => " ".repeat(gutter_width),
    };
    let marker_style = match bg {
        Some(bg) => Style::default().fg(marker_color).bg(bg),
        None => Style::default().fg(marker_color),
    };
    let raw_spans = highlighter.highlight(&line.text);
    let scrolled_spans = skip_spans_columns(raw_spans, scroll_cols);
    let mut text_spans = truncate_spans_to_width(scrolled_spans, text_width);
    // Pad text up to text_width so the right-side column starts at a
    // consistent x-coordinate. With per-line background tint, the
    // pad fills the tinted area to the full column width.
    let consumed: usize = text_spans.iter().map(Span::width).sum();
    if consumed < text_width {
        let pad_style = match bg {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };
        text_spans.push(Span::styled(" ".repeat(text_width - consumed), pad_style));
    }
    // Apply bg to existing spans so the tint covers the whole text
    // area, not just the literal characters. Skip if there's no bg.
    if let Some(bg) = bg {
        for span in &mut text_spans {
            if span.style.bg.is_none() {
                span.style = span.style.bg(bg);
            }
        }
    }
    let mut spans = Vec::with_capacity(text_spans.len() + 4);
    spans.push(Span::styled(gutter, Style::default().fg(theme::DIM)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(marker, marker_style));
    let space_style = match bg {
        Some(bg) => Style::default().bg(bg),
        None => Style::default(),
    };
    spans.push(Span::styled(" ", space_style));
    spans.extend(text_spans);
    spans
}

fn marker_for_kind(kind: DiffLineKind) -> (&'static str, Color, Option<Color>) {
    match kind {
        DiffLineKind::Added => ("+", Color::Green, Some(ADDED_BG)),
        DiffLineKind::Removed => ("-", Color::Red, Some(REMOVED_BG)),
        DiffLineKind::Context => (" ", theme::DIM, None),
    }
}

/// Truncate a span list to `max_width` display columns. Splits the
/// last span mid-token if necessary using `unicode-width` per
/// character. Returns an empty vec when `max_width == 0`.
fn truncate_spans_to_width(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    if max_width == 0 {
        return Vec::new();
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut consumed: usize = 0;
    for span in spans {
        let span_width = span.content.width();
        if consumed.saturating_add(span_width) <= max_width {
            consumed = consumed.saturating_add(span_width);
            out.push(span);
            continue;
        }
        let remaining = max_width - consumed;
        let mut buf = String::with_capacity(span.content.len());
        let mut span_consumed = 0usize;
        for c in span.content.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if span_consumed + cw > remaining {
                break;
            }
            buf.push(c);
            span_consumed += cw;
        }
        if !buf.is_empty() {
            out.push(Span::styled(buf, span.style));
        }
        break;
    }
    out
}

/// Drop the first `skip_cols` columns of `spans` and return the rest
/// in render order. Used by the horizontal-scroll path so both
/// halves of a split row can be scrolled by the same amount before
/// being truncated to the per-side width.
fn skip_spans_columns(spans: Vec<Span<'static>>, skip_cols: usize) -> Vec<Span<'static>> {
    if skip_cols == 0 {
        return spans;
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len());
    let mut remaining = skip_cols;
    for span in spans {
        if remaining == 0 {
            out.push(span);
            continue;
        }
        let span_width = span.content.width();
        if span_width <= remaining {
            remaining -= span_width;
            continue;
        }
        // Partial skip inside this span — slice off the leading
        // `remaining` cols and keep the tail.
        let mut buf = String::with_capacity(span.content.len());
        let mut to_skip = remaining;
        let mut started = false;
        for c in span.content.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            if !started && to_skip >= cw {
                to_skip -= cw;
                continue;
            }
            started = true;
            buf.push(c);
        }
        remaining = 0;
        if !buf.is_empty() {
            out.push(Span::styled(buf, span.style));
        }
    }
    out
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
/// Render the "terminal too narrow" notice in place of the body.
/// The split view needs both columns of readable code plus the rail;
/// below `MIN_WIDTH_FOR_SPLIT` we tell the user to resize rather
/// than squeeze the rendering harder. Clears every geometry field
/// the click handler reads so a click during this state can't
/// hit-test against stale wide-tier values.
fn render_too_narrow_notice(frame: &mut Frame, area: Rect, app: &mut App) {
    let msg =
        format!("Terminal too narrow — resize to ≥ {MIN_WIDTH_FOR_SPLIT} cols and re-open /diff.");
    frame
        .render_widget(Paragraph::new(msg).style(Style::default().fg(theme::STATUS_WARNING)), area);
    if let Some(o) = app.diff_overlay.as_mut() {
        o.body_keys.clear();
        o.body_head_rows = 0;
        o.pane_origin_row = area.y;
        o.pane_origin_col = area.x;
        o.pane_width = area.width;
    }
}

fn banner_row(label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(label, Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
    ])
}

/// Build the pane banner line. The footer hint already advertises
/// `Esc close`, so the banner is informational only — no in-banner
/// affordance to dismiss.
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
    fn wrap_chip_body_keeps_short_text_intact() {
        let rows = wrap_chip_body("hello", 40);
        assert_eq!(rows, vec!["hello"]);
    }

    #[test]
    fn wrap_chip_body_respects_explicit_newlines() {
        let rows = wrap_chip_body("line one\nline two", 40);
        assert_eq!(rows, vec!["line one", "line two"]);
    }

    #[test]
    fn wrap_chip_body_chops_long_lines_at_max_chars() {
        let long = "x".repeat(50);
        let rows = wrap_chip_body(&long, 20);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].chars().count(), 20);
        assert_eq!(rows[1].chars().count(), 20);
        assert_eq!(rows[2].chars().count(), 10);
    }

    #[test]
    fn wrap_chip_body_handles_empty_input() {
        let rows = wrap_chip_body("", 40);
        assert_eq!(rows, vec![String::new()]);
    }
}
