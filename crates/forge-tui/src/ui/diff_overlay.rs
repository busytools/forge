//! Renderer for [`crate::app::ActiveView::Diff`].
//!
//! Full-screen takeover triggered by `/diff` or the Inspector GIT
//! `🦉` click. One continuous top-to-bottom scroll of every changed
//! file (flat git order), GitHub-style: a FILES jump rail on the left
//! (hidden below 120 cols, body then full-width) and the diff body on
//! the right. Each file is introduced by a sticky header (caret, path,
//! status badge, `+N -M` counts) that pins at the top of the viewport
//! while its body scrolls beneath it. Unified (one column) is the
//! default; `t` flips the whole document to split (side-by-side).
//! Long lines soft-wrap; deleted files collapse to a one-line notice.
//!
//! Per-line syntax highlighting reuses
//! [`crate::ui::highlight::LineHighlighter`] - two stateful passes per
//! file (old side: context + removed; new side: context + added) run
//! once on window-entry and cached as unwrapped spans, so a scroll
//! never re-runs syntect. Heights are measured lazily (with soft-wrap)
//! for the visible window and cached; off-screen files contribute a
//! cheap estimate to the scrollbar math until scrolled near.
//!
//! Click-to-comment lands here: `body_keys` is the parallel
//! per-rendered-row index the click handler reads to resolve
//! `mouse.row` → `BodyRowKey`. In unified a click anywhere on a row
//! opens the comment; in split the handler picks the old/new side by
//! the click column. Comment chips render after their anchor line; the
//! active editor's TextArea expands inline below its anchor.

mod pairing;

use forge_primitives::ReviewStatus;
use forge_workspace::env::git_diff::hunks::{DiffLineKind, FileHunks, FileStatus, Hunk};

use crate::app::diff_overlay::{
    ActiveCommentInput, BodyRowKey, DiffScope, DiffViewMode, FileHighlight, HunkComment, LineKey,
    RailRowKey, rail_width_for,
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
use crate::ui::chat_tree;
use crate::ui::highlight::LineHighlighter;
use crate::ui::theme;

/// Minimum body width (in columns) for the side-by-side split view -
/// two columns of readable code need the room. Unified renders at any
/// width (it soft-wraps), so below this the split toggle silently
/// falls back to unified rather than blocking the overlay.
const MIN_WIDTH_FOR_SPLIT: u16 = 100;

/// The layout to actually render: the user's stored choice, except a
/// body narrower than [`MIN_WIDTH_FOR_SPLIT`] forces unified (split's
/// two columns don't fit). The stored `view_mode` is untouched, so
/// widening the pane restores split.
fn effective_view_mode(stored: DiffViewMode, pane_width: u16) -> DiffViewMode {
    if pane_width < MIN_WIDTH_FOR_SPLIT { DiffViewMode::Unified } else { stored }
}

/// Named row offsets inside the commit stepper bar: the title on row 0,
/// the movement/controls row on row 2, with blank spacers between and
/// after so neither reads cramped against the diff.
const STEPPER_TITLE_ROW: u16 = 0;
const STEPPER_MOVE_ROW: u16 = 2;

/// Total rows the commit stepper reserves at the top of the overlay in
/// commit mode (title, gap, movement, gap-before-diff). Zero in
/// whole-diff-only mode, where the overlay is byte-identical to before.
const STEPPER_HEIGHT: u16 = 4;

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.cached_frame_area = area;

    if app.diff_overlay.is_none() {
        render_missing_state(frame, area);
        return;
    }

    // Reserve the top rows for the commit stepper (commit mode only)
    // and the bottom row for the key-hints bar; the rail + body fill
    // what's left.
    let stepper_h: u16 = if app.diff_overlay.as_ref().is_some_and(|o| !o.commits.is_empty()) {
        STEPPER_HEIGHT
    } else {
        0
    };
    let usable_height = area.height.saturating_sub(1).saturating_sub(stepper_h);
    let usable_area = Rect {
        x: area.x,
        y: area.y.saturating_add(stepper_h),
        width: area.width,
        height: usable_height,
    };

    // The jump rail shows at >= 120 cols (`rail_width_for`); below that
    // it hides and the continuous body takes the full width.
    let rail_width = rail_width_for(area.width);
    let (rail_area, sep_area, pane_area) = if rail_width > 0 {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(rail_width),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(usable_area);
        (Some(chunks[0]), Some(chunks[1]), chunks[2])
    } else {
        (None, None, usable_area)
    };

    if pane_area.height == 0 {
        if let Some(o) = app.diff_overlay.as_mut() {
            o.body_keys.clear();
            o.body_head_rows = 0;
        }
        return;
    }

    // A resize changes every file's wrapped row count, so the cached
    // heights are stale. Drop them before the offset table reads them
    // (the span cache is width-independent and stays put).
    if let Some(o) = app.diff_overlay.as_mut() {
        o.invalidate_heights_if_width_changed(pane_area.width);
    }

    // 1. Offset table (measured-or-estimate), clamp the document
    //    scroll, find the file at the top of the viewport + the row
    //    into it. In commit mode a message block leads the document, so
    //    the file sub-document starts `message_rows` below the top.
    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    let viewport_rows = u32::from(pane_area.height);
    // The message block only renders when the body itself does (not
    // during a scan / failure / empty tree). `commit_message_block_lines`
    // returns empty outside commit mode, so `message_rows` is 0 in
    // whole-diff and the math below reduces to the pre-enhancement path.
    let renders_body = overlay.scanner_ok && !overlay.commit_loading && !overlay.files.is_empty();
    let msg_lines = if renders_body {
        commit_message_block_lines(overlay, pane_area.width)
    } else {
        Vec::new()
    };
    let message_rows = u32::try_from(msg_lines.len()).unwrap_or(u32::MAX);
    let offsets = overlay.doc_offsets();
    let max_scroll = message_rows.saturating_add(offsets.total).saturating_sub(viewport_rows);
    let doc_scroll = overlay.doc_scroll.min(max_scroll);
    let in_message_block = message_rows > 0 && doc_scroll < message_rows;
    let file_scroll = doc_scroll.saturating_sub(message_rows);
    let first_visible = offsets.file_at_row(file_scroll);
    let local_offset =
        file_scroll.saturating_sub(offsets.starts.get(first_visible).copied().unwrap_or(0));

    // 2. Populate the height + span caches for the window of files
    //    overlapping the viewport (lazy on entry), storing the clamped
    //    scroll.
    if let Some(o) = app.diff_overlay.as_mut() {
        o.doc_scroll = doc_scroll;
        populate_window_cache(o, pane_area.width, viewport_rows, first_visible, local_offset);
    }

    // 3. Build the visible body from the populated caches. While the
    //    message block is (partially) on screen it leads the scroll with
    //    no pinned header; once it scrolls off, the file header pins as
    //    usual (and whole-diff always takes this second path).
    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    let ContinuousBody { lines, keys, tail_scroll } = if in_message_block {
        build_message_block_body(overlay, pane_area.width, pane_area.height, msg_lines, doc_scroll)
    } else {
        build_continuous_body(
            overlay,
            pane_area.width,
            pane_area.height,
            first_visible,
            local_offset,
        )
    };
    let head_count = if in_message_block { 0 } else { 1usize.min(lines.len()) };

    // 4. Stash geometry + the hit-test scroll for the click handler.
    if let Some(o) = app.diff_overlay.as_mut() {
        o.body_keys = keys;
        o.body_head_rows = head_count;
        o.body_tail_scroll = tail_scroll;
        o.pane_origin_row = pane_area.y;
        o.pane_origin_col = pane_area.x;
        o.pane_width = pane_area.width;
    }

    // 5. Draw: rail (if shown), separator, the pinned sticky header +
    //    the scrolling tail beneath it, then the key-hints bar.
    if let (Some(rail_area), Some(sep_area)) = (rail_area, sep_area) {
        render_rail(frame, rail_area, app);
        render_separator(frame, sep_area);
    }
    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    let head_height = u16::try_from(head_count).unwrap_or(1);
    let (head, tail) = lines.split_at(head_count);
    let head_rect =
        Rect { x: pane_area.x, y: pane_area.y, width: pane_area.width, height: head_height };
    let tail_rect = Rect {
        x: pane_area.x,
        y: pane_area.y.saturating_add(head_height),
        width: pane_area.width,
        height: pane_area.height.saturating_sub(head_height),
    };
    frame.render_widget(Paragraph::new(head.to_vec()), head_rect);
    let tail_scroll_u16 = u16::try_from(tail_scroll).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(tail.to_vec()).scroll((tail_scroll_u16, 0)), tail_rect);
    render_footer(frame, area, overlay, effective_view_mode(overlay.view_mode, pane_area.width));

    // Commit stepper on top (commit mode only), and the jump dropdown
    // painted over the body when open. Both draw last so they sit above
    // the rail / body. `as_mut` so the stepper can stash the `⌄ jump`
    // click span for the mouse handler.
    if stepper_h > 0 {
        let stepper_area = Rect { x: area.x, y: area.y, width: area.width, height: stepper_h };
        if let Some(o) = app.diff_overlay.as_mut() {
            render_stepper(frame, stepper_area, o);
            if o.jump_open {
                render_jump_dropdown(frame, area, o);
            }
        }
    }
}

fn render_missing_state(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("Diff overlay opened without state. This is a bug - press Esc to return.")
            .style(Style::default().fg(theme::STATUS_ERROR)),
        area,
    );
}

/// Render the commit stepper bar (commit mode only): a title row naming
/// the branch under review, then a controls row with the position, the
/// current sha + subject, the `◀ ▶` arrows, the `⌄ jump` affordance, and
/// the running pending-comment total. Stashes the `⌄ jump` screen span
/// so the mouse handler can open the dropdown on a click.
fn render_stepper(frame: &mut Frame, area: Rect, overlay: &mut DiffOverlayState) {
    if area.height < STEPPER_HEIGHT || area.width == 0 {
        overlay.jump_hint_span = None;
        return;
    }
    let dim = Style::default().fg(theme::DIM);
    let accent = Style::default().fg(theme::RUST_ORANGE);
    let accent_bold = accent.add_modifier(Modifier::BOLD);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    let n = overlay.commits.len();
    let branch = overlay.branch.clone().unwrap_or_else(|| "HEAD".to_owned());
    let title = Line::from(vec![
        Span::raw("  "),
        Span::styled("COMMITS", accent_bold),
        Span::styled(" · ", dim),
        Span::styled(branch, accent),
        Span::styled(" vs ", dim),
        Span::styled(overlay.target.clone(), accent),
        Span::styled(format!(" · {n} commit{}", if n == 1 { "" } else { "s" }), dim),
    ]);
    let title_y = area.y.saturating_add(STEPPER_TITLE_ROW);
    frame.render_widget(
        Paragraph::new(title),
        Rect { x: area.x, y: title_y, width: area.width, height: 1 },
    );

    let total = overlay.comments.len();
    let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    match overlay.scope {
        DiffScope::Commit(i) => {
            let short = overlay.commits.get(i).map(|c| c.short_sha.clone()).unwrap_or_default();
            let subject = overlay.commits.get(i).map(|c| c.subject.clone()).unwrap_or_default();
            spans.push(Span::styled("\u{25c0} ", accent));
            spans.push(Span::styled("[", dim));
            spans.push(Span::styled(format!("{} / {n}", i + 1), bold));
            spans.push(Span::styled("]  ", dim));
            spans.push(Span::styled(short, Style::default().fg(theme::STATUS_WARNING)));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(subject, bold));
            spans.push(Span::styled("  \u{25b6}", accent));
        }
        DiffScope::WholeDiff => {
            spans.push(Span::styled("All changes", bold));
            spans.push(Span::styled(format!("  (whole branch vs {})", overlay.target), dim));
        }
    }
    spans.push(Span::raw("   "));
    let jump_start: usize = spans.iter().map(Span::width).sum();
    let jump_label = "\u{2304} jump";
    spans.push(Span::styled(jump_label, if overlay.jump_open { accent_bold } else { dim }));
    let jump_end = jump_start + jump_label.width();
    if total > 0 {
        spans.push(Span::styled("   \u{b7}  ", dim));
        spans.push(Span::styled(
            format!("\u{25cf} {total} comment{} so far", if total == 1 { "" } else { "s" }),
            accent,
        ));
    }
    let controls_y = area.y.saturating_add(STEPPER_MOVE_ROW);
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect { x: area.x, y: controls_y, width: area.width, height: 1 },
    );

    let start = area.x.saturating_add(u16::try_from(jump_start).unwrap_or(u16::MAX));
    let end = area.x.saturating_add(u16::try_from(jump_end).unwrap_or(u16::MAX));
    overlay.jump_hint_span = Some((controls_y, start, end));
}

/// The right-aligned cluster for a jump-dropdown row: the `● N` comment
/// badge and/or the `◂` current-scope marker.
fn jump_row_marker(count: usize, current: bool) -> String {
    match (count, current) {
        (0, false) => String::new(),
        (0, true) => "\u{25c2}".to_owned(),
        (n, false) => format!("\u{25cf} {n}"),
        (n, true) => format!("\u{25cf} {n} \u{25c2}"),
    }
}

/// One commit row inside the jump dropdown: `k · <sha> <subject>` with a
/// right-aligned `● N ◂` cluster, fitted to the box's inner width. The
/// selected row renders in accent-bold; otherwise the index is dim, the
/// sha yellow, and the subject default.
fn jump_commit_line(
    inner: usize,
    index: usize,
    short_sha: &str,
    subject: &str,
    count: usize,
    current: bool,
    selected: bool,
) -> Line<'static> {
    let dim = Style::default().fg(theme::DIM);
    let accent = Style::default().fg(theme::RUST_ORANGE);
    let accent_bold = accent.add_modifier(Modifier::BOLD);
    let (idx_style, sha_style, subj_style) = if selected {
        (accent, accent_bold, accent_bold)
    } else {
        (dim, Style::default().fg(theme::STATUS_WARNING), Style::default())
    };
    let index_label = format!("{index} \u{b7} ");
    let right = jump_row_marker(count, current);
    let right_w = right.width();
    let fixed = index_label.width() + short_sha.width() + 1;
    let gap = if right_w > 0 { 2 } else { 0 };
    let subj_budget =
        inner.saturating_sub(fixed).saturating_sub(right_w).saturating_sub(gap).max(1);
    let subject_fitted = fit_box_content(subject, subj_budget);
    let used = fixed + subject_fitted.width() + right_w;
    let pad = inner.saturating_sub(used);
    let mut spans = vec![
        Span::styled("\u{2502} ", dim),
        Span::styled(index_label, idx_style),
        Span::styled(short_sha.to_owned(), sha_style),
        Span::raw(" "),
        Span::styled(subject_fitted, subj_style),
        Span::raw(" ".repeat(pad)),
    ];
    if !right.is_empty() {
        spans.push(Span::styled(right, accent));
    }
    spans.push(Span::styled(" \u{2502}", dim));
    Line::from(spans)
}

/// Render the jump dropdown over the body: `All changes` then the
/// commits oldest-first, each with its `● N` comment count, the current
/// scope marked `◂`, and the highlighted row in accent. Keyboard-driven
/// (see [`crate::app::diff_overlay`]); painted below the stepper.
fn render_jump_dropdown(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState) {
    let dim = Style::default().fg(theme::DIM);
    let accent = Style::default().fg(theme::RUST_ORANGE);
    let accent_bold = accent.add_modifier(Modifier::BOLD);

    // Per-scope comment tallies for the menu badges.
    let mut per_commit: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut whole_count = 0usize;
    for c in &overlay.comments {
        match c.commit.as_deref() {
            Some(sha) => *per_commit.entry(sha).or_insert(0) += 1,
            None => whole_count += 1,
        }
    }

    let indent: u16 = 8;
    let box_width = area.width.saturating_sub(indent).saturating_sub(2).clamp(24, 64);
    let bw = usize::from(box_width);
    let inner = bw.saturating_sub(4);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(bw.saturating_sub(2))),
        dim,
    )));

    // "All changes" row.
    {
        let selected = overlay.jump_selected == 0;
        let current = overlay.scope == DiffScope::WholeDiff;
        let base = "All changes";
        let right = jump_row_marker(whole_count, current);
        let right_w = right.width();
        let gap = if right_w > 0 { 2 } else { 0 };
        let hint_budget =
            inner.saturating_sub(base.width()).saturating_sub(right_w).saturating_sub(gap);
        let hint = fit_box_content(" (whole branch, one diff)", hint_budget);
        let used = base.width() + hint.width() + right_w;
        let pad = inner.saturating_sub(used);
        let mut spans = vec![
            Span::styled("\u{2502} ", dim),
            Span::styled(base, if selected { accent_bold } else { Style::default() }),
            Span::styled(hint, dim),
            Span::raw(" ".repeat(pad)),
        ];
        if !right.is_empty() {
            spans.push(Span::styled(right, accent));
        }
        spans.push(Span::styled(" \u{2502}", dim));
        lines.push(Line::from(spans));
    }

    // Divider.
    lines.push(Line::from(vec![
        Span::styled("\u{2502} ", dim),
        Span::styled("\u{2500}".repeat(inner), dim),
        Span::styled(" \u{2502}", dim),
    ]));

    for (k, commit) in overlay.commits.iter().enumerate() {
        let count = per_commit.get(commit.sha.as_str()).copied().unwrap_or(0);
        lines.push(jump_commit_line(
            inner,
            k + 1,
            &commit.short_sha,
            &commit.subject,
            count,
            overlay.scope == DiffScope::Commit(k),
            overlay.jump_selected == k + 1,
        ));
    }

    lines.push(Line::from(Span::styled(
        format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(bw.saturating_sub(2))),
        dim,
    )));

    // Under the movement row, not below the trailing gap - keeps the menu tied to `⌄ jump`.
    let menu_top = STEPPER_MOVE_ROW.saturating_add(1);
    let max_h = area.height.saturating_sub(menu_top);
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).min(max_h);
    if height == 0 {
        return;
    }
    let rect = Rect { x: area.x + indent, y: area.y + menu_top, width: box_width, height };
    frame.render_widget(Paragraph::new(lines), rect);
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
    // Highlight the leaf whose document range owns the top of the
    // viewport - the same file the sticky header pins.
    let current_idx = overlay.doc_offsets().file_at_row(overlay.doc_scroll);

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
    walk_rail_tree(
        &tree,
        "",
        true,
        &mut all_lines,
        &mut all_keys,
        overlay,
        current_idx,
        inner_width,
    );

    if overlay.untracked_suppressed > 0 {
        // Surface the cap overflow so a fresh-repo state with many
        // untracked files doesn't render identically to a clean
        // tree. Yellow signals "suppressed work-product, not a
        // failure" - matches the Untracked status glyph colour.
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

fn walk_rail_tree(
    node: &RailNode,
    prefix: &str,
    is_top_level: bool,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<RailRowKey>,
    overlay: &DiffOverlayState,
    current_idx: usize,
    inner_width: usize,
) {
    let count = node.children.len();
    for (idx, child) in node.children.iter().enumerate() {
        let is_last = idx + 1 == count;
        emit_rail_node(
            child,
            prefix,
            is_top_level,
            is_last,
            lines,
            keys,
            overlay,
            current_idx,
            inner_width,
        );
    }
}

fn emit_rail_node(
    node: &RailNode,
    prefix: &str,
    is_top_level: bool,
    is_last: bool,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<RailRowKey>,
    overlay: &DiffOverlayState,
    current_idx: usize,
    inner_width: usize,
) {
    let connector = if is_top_level {
        ""
    } else if is_last {
        chat_tree::LAST
    } else {
        chat_tree::BRANCH
    };
    let line_prefix = format!("{prefix}{connector}");
    match node.leaf.as_ref() {
        None => {
            // Directory - append trailing `/` so the eye reads it as
            // a folder, not a file with no extension.
            lines.push(rail_directory_row(&line_prefix, &node.label, inner_width));
            keys.push(RailRowKey::Directory);
        }
        Some(leaf) => {
            let is_current = leaf.file_idx == current_idx;
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
        format!("{prefix}{}  ", chat_tree::SPINE)
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
            current_idx,
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

/// Render the pinned key-hints bar along the bottom edge: scroll /
/// page / `t` toggle / click-to-comment / click-to-jump / Esc, with
/// the current mode (unified / split) right-justified. With a comment
/// editor open it shows the editor's Enter/Esc hints instead. Painted
/// over the last row of `area`, after the body, so it never overlaps.
fn render_footer(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState, mode: DiffViewMode) {
    if area.height < 2 {
        return;
    }
    let footer_rect = Rect { x: area.x, y: area.y + area.height - 1, width: area.width, height: 1 };
    let dim = Style::default().fg(theme::DIM);
    let orange = Style::default().fg(theme::RUST_ORANGE);
    let count = overlay.comments.len();
    let mut spans = vec![Span::raw("  ")];
    // In commit mode the running total already lives in the stepper
    // ("● N comments so far"), so the footer skips the redundant prefix
    // (and reclaims the width for the extra commit-nav hints).
    if count > 0 && overlay.commits.is_empty() {
        spans.push(Span::styled(
            format!("{count} comment{}", if count == 1 { "" } else { "s" }),
            orange,
        ));
        spans.push(Span::styled(" pending  ·  ", dim));
    }
    if overlay.active_input.is_some() {
        spans.push(Span::styled("Enter ", orange));
        spans.push(Span::styled("save", dim));
        spans.push(Span::styled("  ·  ", dim));
        spans.push(Span::styled("Esc ", orange));
        spans.push(Span::styled("cancel input", dim));
    } else {
        let commit_mode = !overlay.commits.is_empty();
        let esc_label = if count > 0 {
            if commit_mode { "submit all" } else { "save & close" }
        } else {
            "close"
        };
        // Commit mode swaps the page / rail-jump hints for the commit
        // navigation ones (matching the approved mockup); both still work.
        let mut hints: Vec<(&str, &str)> = vec![("\u{2191}\u{2193}", "scroll")];
        if commit_mode {
            hints.push(("\u{25c0}\u{25b6} / [ ]", "prev/next commit"));
            hints.push(("a", "all changes / back"));
            hints.push(("j", "jump"));
        } else {
            hints.push(("PgUp/Dn", "page"));
        }
        hints.push(("t", "split/unified"));
        hints.push(("click line", "comment"));
        hints.push(("r", "resolve"));
        hints.push(("o", "reopen"));
        if !commit_mode {
            hints.push(("click file", "jump"));
        }
        hints.push(("Esc", esc_label));
        for (idx, (key, label)) in hints.iter().enumerate() {
            if idx > 0 {
                spans.push(Span::styled("  ·  ", dim));
            }
            spans.push(Span::styled(format!("{key} "), orange));
            spans.push(Span::styled(*label, dim));
        }
    }
    // Right-justify the effective mode (what's actually rendered -
    // a narrow pane shows unified even when split is the stored choice).
    let mode_label = match mode {
        DiffViewMode::Unified => "unified",
        DiffViewMode::Split => "split",
    };
    let left_width: usize = spans.iter().map(Span::width).sum();
    let pad = usize::from(area.width)
        .saturating_sub(left_width)
        .saturating_sub(mode_label.width())
        .saturating_sub(2);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(mode_label, orange));
    spans.push(Span::raw(" "));
    frame.render_widget(Paragraph::new(Line::from(spans)), footer_rect);
}

/// One logical row of the unified body, before syntax highlighting
/// and soft-wrap. The renderer turns each into one or more visual
/// `Line`s - styled spans pulled from the per-file highlight cache,
/// then wrapped to the content width - stamping `key` onto every
/// visual row (including wrap continuations) so a click anywhere on
/// the logical line resolves to its `LineKey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnifiedRow {
    pub key: BodyRowKey,
    /// `'+'` added, `'-'` removed, `' '` context or hunk header.
    pub sign: char,
    /// Gutter line number: new-side for added / context, old-side for
    /// removed; `None` for hunk headers.
    pub line_no: Option<u32>,
    /// Raw line text (no marker). For a hunk header, the `@@ … @@`
    /// string.
    pub text: String,
}

/// Flatten one file's hunks into the unified body's logical rows: a
/// `@@ … @@` header row per hunk, then one row per diff line in
/// source order (which is already removed-then-added within a change
/// block). Pure - no highlighting, no wrap; those happen lazily at
/// render time against the per-file span cache + content width.
///
/// `BodyRowKey` keying mirrors the split pairing so the existing
/// click hit-test resolves: added / context carry the key on the
/// right, removed on the left. In unified the row is one column, so
/// the renderer's click handler resolves `left.or(right)` - either
/// side opens the comment.
pub(crate) fn unified_rows(file_idx: usize, file: &FileHunks) -> Vec<UnifiedRow> {
    let mut rows = Vec::new();
    for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
        rows.push(UnifiedRow {
            key: BodyRowKey::HunkHeader { file_idx, hunk_idx },
            sign: ' ',
            line_no: None,
            text: format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ),
        });
        for (line_idx, line) in hunk.lines.iter().enumerate() {
            let key = LineKey { file_idx, hunk_idx, line_idx };
            let (sign, line_no, body_key) = match line.kind {
                DiffLineKind::Added => {
                    ('+', line.new_line, BodyRowKey::HunkRow { left: None, right: Some(key) })
                }
                DiffLineKind::Removed => {
                    ('-', line.old_line, BodyRowKey::HunkRow { left: Some(key), right: None })
                }
                DiffLineKind::Context => {
                    (' ', line.new_line, BodyRowKey::HunkRow { left: None, right: Some(key) })
                }
            };
            rows.push(UnifiedRow { key: body_key, sign, line_no, text: line.text.clone() });
        }
    }
    rows
}

/// Content-column width for a unified row: the pane minus the leading
/// 2-col indent, the line-number gutter, and the 3-col sign zone
/// (space + sign + space). Wrap continuations align at this same
/// column, so they wrap at the same width.
fn unified_content_width(pane_width: u16, gutter_width: usize) -> usize {
    usize::from(pane_width).saturating_sub(2).saturating_sub(gutter_width).saturating_sub(3)
}

/// Measured document height of one file in rows: the sticky header (1)
/// plus the exact rows the renderer emits for the body. Built by
/// running the same `push_file_body` the renderer uses into a scratch
/// buffer and counting, so the measured height never drifts from what
/// is drawn - it accounts for soft-wrap (`wrap_spans_to_width`), split
/// pairing, the collapsed-deleted notice, and any inline comment chips
/// or open editor anchored in the file. The file's highlight spans
/// must be cached first (a collapsed deleted file needs none).
fn file_height(overlay: &DiffOverlayState, file_idx: usize, pane_width: u16) -> u32 {
    let comments_by_key = index_comments_by_key(&overlay.scoped_comments());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut keys: Vec<BodyRowKey> = Vec::new();
    // include_header = false: the sticky header is the pinned row,
    // counted as the +1 below, not part of the scrolling body.
    push_file_body(overlay, file_idx, false, pane_width, &comments_by_key, &mut lines, &mut keys);
    1u32.saturating_add(u32::try_from(lines.len()).unwrap_or(u32::MAX))
}

/// Highlight one file's diff lines into unwrapped styled spans,
/// indexed `[hunk_idx][line_idx]`. Two stateful highlighters track the
/// old side (context + removed) and new side (context + added)
/// independently so multi-line constructs colour correctly within
/// each side - the same per-side pass the split renderer used inline,
/// now run once and cached. A context line advances both sides but is
/// stored with its new-side spans (GitHub convention); a context line
/// is identical text on both sides, so split reuses the same spans for
/// its old column.
fn build_file_highlight(file: &FileHunks) -> FileHighlight {
    let mut old_hl = LineHighlighter::for_path(&file.path);
    let mut new_hl = LineHighlighter::for_path(&file.path);
    file.hunks
        .iter()
        .map(|hunk| {
            hunk.lines
                .iter()
                .map(|line| match line.kind {
                    DiffLineKind::Removed => old_hl.highlight(&line.text),
                    DiffLineKind::Added => new_hl.highlight(&line.text),
                    DiffLineKind::Context => {
                        let _ = old_hl.highlight(&line.text);
                        new_hl.highlight(&line.text)
                    }
                })
                .collect()
        })
        .collect()
}

/// Cached spans for one diff line, or an empty slice when the file
/// isn't cached yet / the key is out of range (defensive - the
/// renderer populates the cache before reading it).
fn cached_line_spans(cache: Option<&FileHighlight>, key: LineKey) -> &[Span<'static>] {
    match cache.and_then(|c| c.get(key.hunk_idx)).and_then(|h| h.get(key.line_idx)) {
        Some(spans) => spans.as_slice(),
        None => &[],
    }
}

/// Soft-wrap a styled span list into visual rows of at most
/// `content_width` display columns, splitting a span mid-content when
/// a token straddles the boundary. Always returns at least one row
/// (possibly empty). The row count matches [`wrap_count`] for
/// single-width text, which keeps the measured height and the
/// rendered rows in step.
fn wrap_spans_to_width(spans: &[Span<'static>], content_width: usize) -> Vec<Vec<Span<'static>>> {
    if content_width == 0 {
        return vec![Vec::new()];
    }
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;
    for span in spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width.saturating_add(ch_width) > content_width {
                if !buf.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                rows.push(std::mem::take(&mut current));
                current_width = 0;
            }
            buf.push(ch);
            current_width = current_width.saturating_add(ch_width);
        }
        if !buf.is_empty() {
            current.push(Span::styled(buf, span.style));
        }
    }
    if !current.is_empty() || rows.is_empty() {
        rows.push(current);
    }
    rows
}

/// Ensure file `idx`'s highlight spans + measured height are cached,
/// computing them only if absent (lazy on window-entry). Highlight
/// runs first because `file_height` counts the rendered (wrapped) rows
/// off those spans. A collapsed deleted file skips the highlight pass -
/// it shows no diff lines.
fn ensure_file_cached(overlay: &mut DiffOverlayState, idx: usize, pane_width: u16) {
    let collapsed = overlay.is_collapsed(idx);
    if !collapsed
        && overlay.highlighted.get(idx).is_some_and(Option::is_none)
        && let Some(file) = overlay.files.get(idx)
    {
        let spans = build_file_highlight(file);
        if let Some(slot) = overlay.highlighted.get_mut(idx) {
            *slot = Some(spans);
        }
    }
    if overlay.measured_heights.get(idx).copied().flatten().is_none() {
        let height = file_height(overlay, idx, pane_width);
        if let Some(slot) = overlay.measured_heights.get_mut(idx) {
            *slot = Some(height);
        }
    }
}

/// Measure + highlight the window of files overlapping the viewport,
/// from `first_visible` until the accumulated height covers the rows
/// needed (the scroll into the first file plus one viewport). Files
/// past the window keep their cheap estimate until scrolled near.
fn populate_window_cache(
    overlay: &mut DiffOverlayState,
    pane_width: u16,
    viewport_rows: u32,
    first_visible: usize,
    local_offset: u32,
) {
    let needed = local_offset.saturating_add(viewport_rows);
    let mut accumulated = 0u32;
    let mut idx = first_visible;
    while idx < overlay.files.len() && accumulated < needed {
        ensure_file_cached(overlay, idx, pane_width);
        accumulated = accumulated
            .saturating_add(overlay.measured_heights.get(idx).copied().flatten().unwrap_or(0));
        idx = idx.saturating_add(1);
    }
}

/// The visible body: `lines[0]` is the pinned sticky header (the file
/// at the top of the viewport), `lines[1..]` the scrolling tail (that
/// file's body, then following files). `keys` is parallel. `tail_scroll`
/// is the `Paragraph::scroll` to apply to the tail.
struct ContinuousBody {
    lines: Vec<Line<'static>>,
    keys: Vec<BodyRowKey>,
    tail_scroll: usize,
}

/// The commit-message block that leads the scrolling document in commit
/// mode: a full-pane rule, the subject (bold), then the body (dim,
/// soft-wrapped) each behind a RUST_ORANGE `│` rail. Empty in whole-diff
/// scope (no block - the overlay stays byte-identical) or when the
/// current commit can't be resolved. A subject-only commit yields just
/// the rule + subject.
fn commit_message_block_lines(overlay: &DiffOverlayState, pane_width: u16) -> Vec<Line<'static>> {
    let DiffScope::Commit(i) = overlay.scope else { return Vec::new() };
    let Some(commit) = overlay.commits.get(i) else { return Vec::new() };
    let dim = Style::default().fg(theme::DIM);
    let rail = Style::default().fg(theme::RUST_ORANGE);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    // Content width after the leading "│ " rail (2 cols).
    let content_width = usize::from(pane_width).saturating_sub(2).max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled("\u{2500}".repeat(usize::from(pane_width)), dim)));
    for row in wrap_chip_body(&commit.subject, content_width) {
        lines.push(Line::from(vec![Span::styled("\u{2502} ", rail), Span::styled(row, bold)]));
    }
    let body = commit.body.trim();
    if !body.is_empty() {
        lines.push(Line::from(Span::styled("\u{2502}", rail)));
        for row in wrap_chip_body(body, content_width) {
            if row.is_empty() {
                lines.push(Line::from(Span::styled("\u{2502}", rail)));
            } else {
                lines.push(Line::from(vec![
                    Span::styled("\u{2502} ", rail),
                    Span::styled(row, dim),
                ]));
            }
        }
    }
    lines
}

/// Build the body when the commit-message block is (partially) visible at
/// the top of the scroll - commit mode, scrolled above the first file.
/// The visible message rows lead, then each file's header-and-body
/// follows, all scrolling as one block (no pinned header, so the caller
/// pins zero rows). `doc_scroll` is the row offset into the whole
/// document (message block + files); `msg_lines` the full message block.
fn build_message_block_body(
    overlay: &DiffOverlayState,
    pane_width: u16,
    viewport_rows: u16,
    msg_lines: Vec<Line<'static>>,
    doc_scroll: u32,
) -> ContinuousBody {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut keys: Vec<BodyRowKey> = Vec::new();
    let skip = usize::try_from(doc_scroll).unwrap_or(usize::MAX);
    for line in msg_lines.into_iter().skip(skip) {
        lines.push(line);
        keys.push(BodyRowKey::CommitMessage);
    }
    let comments_by_key = index_comments_by_key(&overlay.scoped_comments());
    let needed = usize::from(viewport_rows);
    let mut file_idx = 0;
    while file_idx < overlay.files.len() && lines.len() <= needed {
        // Every file's header scrolls here (none is pinned while the
        // message block leads); the pin returns once it scrolls off.
        push_file_body(
            overlay,
            file_idx,
            true,
            pane_width,
            &comments_by_key,
            &mut lines,
            &mut keys,
        );
        file_idx = file_idx.saturating_add(1);
    }
    ContinuousBody { lines, keys, tail_scroll: 0 }
}

/// Build the continuous body for the current viewport: the pinned
/// sticky header for `first_visible`, then the scrolling tail starting
/// from that file's body and walking following files until the
/// viewport is filled. `local_offset` is the row into `first_visible`'s
/// extent that sits at the top of the viewport (0 = its header).
fn build_continuous_body(
    overlay: &DiffOverlayState,
    pane_width: u16,
    viewport_rows: u16,
    first_visible: usize,
    local_offset: u32,
) -> ContinuousBody {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut keys: Vec<BodyRowKey> = Vec::new();

    // Scanner failure trumps everything: surface it regardless of
    // whether any file entries came back (a partial failure leaves
    // every file's hunks empty). Spell out `target: agent.env_git` -
    // the actual tracing-target string an operator would grep for.
    if !overlay.scanner_ok {
        lines.push(Line::from(Span::styled(
            format!(
                "  Scan failed for `{}` - see tracing logs (target: agent.env_git). Press Esc to retry.",
                overlay.target,
            ),
            Style::default().fg(theme::STATUS_ERROR),
        )));
        keys.push(BodyRowKey::EmptyState);
        return ContinuousBody { lines, keys, tail_scroll: 0 };
    }
    if overlay.commit_loading {
        // A per-commit (or "All changes") scan is in flight - distinct
        // from an empty diff so the user doesn't read "(no changes)"
        // while the scan is still running.
        lines.push(Line::from(Span::styled(
            "  Loading commit diff...",
            Style::default().fg(theme::DIM),
        )));
        keys.push(BodyRowKey::EmptyState);
        return ContinuousBody { lines, keys, tail_scroll: 0 };
    }
    if overlay.files.is_empty() {
        lines.push(Line::from(Span::styled("  (no changes)", Style::default().fg(theme::DIM))));
        keys.push(BodyRowKey::EmptyState);
        return ContinuousBody { lines, keys, tail_scroll: 0 };
    }

    let comments_by_key = index_comments_by_key(&overlay.scoped_comments());

    // Pinned sticky header = the file owning the top of the viewport.
    let Some(header_file) = overlay.files.get(first_visible) else {
        return ContinuousBody { lines, keys, tail_scroll: 0 };
    };
    lines.push(file_header_line(header_file, pane_width, overlay.is_collapsed(first_visible)));
    keys.push(BodyRowKey::FileHeader { file_idx: first_visible });

    // Tail: first_visible's body (its header is pinned, so skip it
    // here), then following files header-and-body, until we have
    // enough rows past the tail scroll to fill the viewport.
    let tail_scroll = usize::try_from(local_offset.saturating_sub(1)).unwrap_or(usize::MAX);
    let needed = tail_scroll.saturating_add(usize::from(viewport_rows)).saturating_add(1);
    let mut file_idx = first_visible;
    while file_idx < overlay.files.len() && keys.len().saturating_sub(1) < needed {
        push_file_body(
            overlay,
            file_idx,
            file_idx != first_visible,
            pane_width,
            &comments_by_key,
            &mut lines,
            &mut keys,
        );
        file_idx = file_idx.saturating_add(1);
    }

    ContinuousBody { lines, keys, tail_scroll }
}

/// Append one file's rows to `lines`/`keys`: optionally its sticky
/// header (skipped for the top file, whose header is pinned), then the
/// collapsed notice for an unexpanded deleted file, an empty-state
/// notice for a binary/untracked file, or the diff body (unified or
/// split per `view_mode`) reading highlighted spans from the cache.
fn push_file_body(
    overlay: &DiffOverlayState,
    file_idx: usize,
    include_header: bool,
    pane_width: u16,
    comments_by_key: &std::collections::HashMap<LineKey, &HunkComment>,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let Some(file) = overlay.files.get(file_idx) else { return };
    if include_header {
        lines.push(file_header_line(file, pane_width, overlay.is_collapsed(file_idx)));
        keys.push(BodyRowKey::FileHeader { file_idx });
    }
    if overlay.is_collapsed(file_idx) {
        let removed = file.removed_count();
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                format!(
                    "File deleted - {removed} line{} removed",
                    if removed == 1 { "" } else { "s" }
                ),
                Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
        keys.push(BodyRowKey::DeletedCollapsed { file_idx });
        return;
    }
    if file.hunks.is_empty() {
        // An untracked file with no hunks was dropped by one of the
        // scan_untracked paths (size cap, non-regular, IO error), all
        // logged WARN under agent.env_git. A tracked file with no
        // hunks is a real binary diff. Differentiate so the user knows
        // whether to grep logs or accept the answer.
        let message = if file.status == FileStatus::Untracked {
            "    (untracked, content not surfaced - see logs (target: agent.env_git))"
        } else {
            "    (binary file or no diff content)"
        };
        lines.push(Line::from(Span::styled(message, Style::default().fg(theme::DIM))));
        keys.push(BodyRowKey::EmptyState);
        return;
    }
    let gutter_width = gutter_width_for(file);
    let cache = overlay.highlighted.get(file_idx).and_then(Option::as_ref);
    match effective_view_mode(overlay.view_mode, pane_width) {
        DiffViewMode::Unified => push_unified_body(
            overlay,
            file,
            file_idx,
            gutter_width,
            pane_width,
            cache,
            comments_by_key,
            lines,
            keys,
        ),
        DiffViewMode::Split => push_split_body(
            overlay,
            file,
            file_idx,
            gutter_width,
            pane_width,
            cache,
            comments_by_key,
            lines,
            keys,
        ),
    }
}

/// Append a file's unified body: each hunk's `@@` header, then each
/// diff line as `[gutter] [sign] [highlighted text]` soft-wrapped to
/// the content width, with the inline comment chip / editor after the
/// anchored line.
fn push_unified_body(
    overlay: &DiffOverlayState,
    file: &FileHunks,
    file_idx: usize,
    gutter_width: usize,
    pane_width: u16,
    cache: Option<&FileHighlight>,
    comments_by_key: &std::collections::HashMap<LineKey, &HunkComment>,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let content_width = unified_content_width(pane_width, gutter_width).max(1);
    for row in unified_rows(file_idx, file) {
        match row.key {
            BodyRowKey::HunkHeader { .. } => {
                lines.push(Line::from(Span::styled(
                    format!("  {}", row.text),
                    Style::default().fg(Color::Cyan),
                )));
                keys.push(row.key);
            }
            BodyRowKey::HunkRow { left, right } => {
                let line_key = left.or(right);
                let spans = line_key.map_or(&[][..], |key| cached_line_spans(cache, key));
                push_unified_diff_rows(&row, spans, gutter_width, content_width, lines, keys);
                if let Some(key) = line_key {
                    if let Some(comment) = comments_by_key.get(&key) {
                        render_comment_chip(comment, key, gutter_width, pane_width, lines, keys);
                    }
                    if let Some(input) = overlay.active_input.as_ref().filter(|i| i.key == key) {
                        render_active_input(
                            input,
                            gutter_width,
                            row.line_no.unwrap_or(0),
                            pane_width,
                            lines,
                            keys,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

/// Push the visual rows for one unified diff line, soft-wrapping its
/// cached spans to `content_width`. The first row carries the line
/// number + sign; continuation rows blank both and align the text
/// under the content column. Every row carries the line's `key` so a
/// click anywhere on the wrapped line resolves.
fn push_unified_diff_rows(
    row: &UnifiedRow,
    spans: &[Span<'static>],
    gutter_width: usize,
    content_width: usize,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let (sign_color, bg) = unified_sign_style(row.sign);
    let wrapped = wrap_spans_to_width(spans, content_width);
    for (seg_idx, segment) in wrapped.into_iter().enumerate() {
        let gutter = if seg_idx == 0 {
            match row.line_no {
                Some(n) => format!("{n:>gutter_width$}"),
                None => " ".repeat(gutter_width),
            }
        } else {
            " ".repeat(gutter_width)
        };
        let sign = if seg_idx == 0 { row.sign } else { ' ' };
        let cell_style = bg.map_or_else(Style::default, |bg| Style::default().bg(bg));
        let mut out: Vec<Span<'static>> = vec![
            Span::raw("  "),
            Span::styled(gutter, Style::default().fg(theme::DIM)),
            Span::raw(" "),
            Span::styled(
                sign.to_string(),
                bg.map_or_else(
                    || Style::default().fg(sign_color),
                    |bg| Style::default().fg(sign_color).bg(bg),
                ),
            ),
            Span::styled(" ", cell_style),
        ];
        for mut span in segment {
            if let Some(bg) = bg
                && span.style.bg.is_none()
            {
                span.style = span.style.bg(bg);
            }
            out.push(span);
        }
        lines.push(Line::from(out));
        keys.push(row.key);
    }
}

/// Append a file's split body: each hunk's `@@` header, then each
/// paired row side-by-side (old | new) reading cached spans, with the
/// inline comment chip / editor after an anchored line.
fn push_split_body(
    overlay: &DiffOverlayState,
    file: &FileHunks,
    file_idx: usize,
    gutter_width: usize,
    pane_width: u16,
    cache: Option<&FileHighlight>,
    comments_by_key: &std::collections::HashMap<LineKey, &HunkComment>,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
        lines.push(hunk_header_row(hunk));
        keys.push(BodyRowKey::HunkHeader { file_idx, hunk_idx });
        for pair in pair_hunk_lines(file_idx, hunk_idx, &hunk.lines) {
            lines.push(split_diff_row(file, pair, gutter_width, pane_width, cache));
            keys.push(BodyRowKey::HunkRow { left: pair.left, right: pair.right });
            // Chip + editor per anchored side; context points both
            // halves at one key, so dedupe before emitting.
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
                if let Some(comment) = comments_by_key.get(&side_key) {
                    render_comment_chip(comment, side_key, gutter_width, pane_width, lines, keys);
                }
                if let Some(input) = overlay.active_input.as_ref().filter(|i| i.key == side_key) {
                    let diff_line = &file.hunks[side_key.hunk_idx].lines[side_key.line_idx];
                    let anchor_line = match diff_line.kind {
                        DiffLineKind::Removed => diff_line.old_line.unwrap_or(0),
                        DiffLineKind::Added | DiffLineKind::Context => {
                            diff_line.new_line.unwrap_or(0)
                        }
                    };
                    render_active_input(input, gutter_width, anchor_line, pane_width, lines, keys);
                }
            }
        }
    }
}

/// Colour + optional background tint for a unified row's sign cell:
/// green/add-tint for `+`, red/del-tint for `-`, dim/none for context.
fn unified_sign_style(sign: char) -> (Color, Option<Color>) {
    match sign {
        '+' => (Color::Green, Some(theme::DIFF_ADDITION_BG)),
        '-' => (Color::Red, Some(theme::DIFF_DELETION_BG)),
        _ => (theme::DIM, None),
    }
}

/// Sticky file-divider header: caret + path (bold) + status badge,
/// with the `+N -M` totals right-justified. `collapsed` picks the
/// `▸` (collapsed) vs `▾` (expanded) caret.
fn file_header_line(file: &FileHunks, pane_width: u16, collapsed: bool) -> Line<'static> {
    let caret = if collapsed { "\u{25b8}" } else { "\u{25be}" };
    let (badge, badge_color) = status_badge(file.status);
    let mut left: Vec<Span<'static>> = vec![
        Span::raw("  "),
        Span::styled(caret, Style::default().fg(theme::DIM)),
        Span::raw(" "),
        Span::styled(file.path.clone(), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(badge, Style::default().fg(badge_color)),
    ];
    let added = file.added_count();
    let removed = file.removed_count();
    let counts = format!("+{added} -{removed}");
    let left_width: usize = left.iter().map(Span::width).sum();
    let pad = usize::from(pane_width)
        .saturating_sub(left_width)
        .saturating_sub(counts.width())
        .saturating_sub(2);
    left.push(Span::raw(" ".repeat(pad)));
    left.push(Span::styled(format!("+{added}"), Style::default().fg(Color::Green)));
    left.push(Span::raw(" "));
    left.push(Span::styled(format!("-{removed}"), Style::default().fg(Color::Red)));
    Line::from(left)
}

/// Status badge word + colour for a file header.
fn status_badge(status: FileStatus) -> (&'static str, Color) {
    match status {
        FileStatus::Modified => ("modified", theme::RUST_ORANGE),
        FileStatus::Added => ("added", Color::Green),
        FileStatus::Deleted => ("deleted", theme::STATUS_ERROR),
        FileStatus::Renamed => ("renamed", theme::RUST_ORANGE),
        FileStatus::Copied => ("copied", theme::RUST_ORANGE),
        FileStatus::Typechange => ("typechange", theme::RUST_ORANGE),
        FileStatus::Unmerged => ("unmerged", theme::STATUS_ERROR),
        FileStatus::Untracked => ("untracked", theme::STATUS_WARNING),
    }
}

/// Index `comments` by `LineKey` for O(1) chip lookup during row
/// emission. Used only inside `build_pane_lines`.
fn index_comments_by_key<'a>(
    comments: &[&'a HunkComment],
) -> std::collections::HashMap<LineKey, &'a HunkComment> {
    let mut map = std::collections::HashMap::with_capacity(comments.len());
    for c in comments {
        // Last-write-wins on duplicate keys (which shouldn't happen -
        // saving a comment on a line that already has one replaces
        // the existing entry - but stay defensive).
        map.insert(c.key, *c);
    }
    map
}

/// Render a saved comment as a bordered mini-box, mirroring the
/// active editor's dialog shape but smaller. Reading it as a box
/// instantly signals "annotation" - the previous single-line chip
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
/// Background tint for the comment-chip interior - very dark
/// warm-brown so the box reads as a contained annotation block
/// even when the right border slips off-screen. Picked to harmonize
/// with `RUST_ORANGE` borders without competing with the diff lines'
/// green/red tints.
const CHIP_BG: Color = Color::Rgb(35, 23, 10);

/// Border colour + uppercase state label for a comment box, keyed off
/// its durable review-thread status. `None` (an ephemeral commit-scoped
/// comment) keeps the classic orange box with no state label.
fn review_state_style(status: Option<ReviewStatus>) -> (Color, Option<&'static str>) {
    match status {
        None | Some(ReviewStatus::Open) => (theme::RUST_ORANGE, status.map(|_| "OPEN")),
        Some(ReviewStatus::Addressed) => (theme::SUBAGENT_TOKEN, Some("ADDRESSED")),
        Some(ReviewStatus::Resolved) => (theme::REVIEW_RESOLVED, Some("RESOLVED")),
        Some(ReviewStatus::Outdated) => (theme::STATUS_WARNING, Some("OUTDATED")),
    }
}

fn render_comment_chip(
    comment: &HunkComment,
    key: LineKey,
    gutter_width: usize,
    pane_width: u16,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let status = comment.thread.as_ref().map(|t| t.status);
    // Resolved threads collapse to a single green one-liner.
    if status == Some(ReviewStatus::Resolved) {
        render_resolved_chip(comment, key, gutter_width, pane_width, lines, keys);
        return;
    }
    let (accent, state_label) = review_state_style(status);
    let indent_cols = gutter_width + 4;
    let indent = " ".repeat(indent_cols);
    let left_offset = 2 + indent_cols;
    let right_pad = 2usize;
    let box_width = usize::from(pane_width).saturating_sub(left_offset + right_pad).max(20);
    let border_style = Style::default().fg(accent).bg(CHIP_BG);
    let body_style = Style::default().bg(CHIP_BG);
    let note_style = Style::default().fg(theme::DIM).bg(CHIP_BG);

    // Top border with embedded title. Whole row carries CHIP_BG so
    // the entire box surface is tinted - eye reads it as one block
    // regardless of whether the rightmost cells (incl. `┐`) end up
    // clipped on a narrow viewport.
    //
    // Width math: `💬` is a 2-cell glyph but `chars().count()`
    // returns 1, so we adjust by +1 to get the title's true visual
    // column width. Without this the top border would land 1 cell
    // further right than the body's `│` border, making the box
    // look stepped.
    let title = match state_label {
        Some(label) => format!(" 💬 line {} · {label} ", comment.line),
        None => format!(" 💬 Comment on line {} ", comment.line),
    };
    let title_visual = title.chars().count() + 1; // +1 for 💬's 2nd cell
    let dash_after = box_width.saturating_sub(3 + title_visual + 1);
    let top = format!("┌──{title}{}┐", "─".repeat(dash_after));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(indent.clone()),
        Span::styled(top, border_style),
    ]));
    keys.push(BodyRowKey::CommentChip(key));

    // Body - wrap the comment_text into rows that fit the box's
    // inner width (`│ ... │` consumes 4 cells of chrome). Keep the
    // wrap simple: break on the box width and on explicit newlines.
    let inner_width = box_width.saturating_sub(4);
    let mut body_rows = wrap_chip_body(&comment.comment_text, inner_width);
    if status == Some(ReviewStatus::Outdated) {
        // The anchored line drifted; the box renders against the
        // captured context, so name that instead of implying the line
        // is live.
        body_rows.push("line changed - resolve, or re-comment on a live line".to_owned());
    }
    for (idx, row) in body_rows.iter().enumerate() {
        let row_chars = row.chars().count();
        let pad = inner_width.saturating_sub(row_chars);
        let is_note = status == Some(ReviewStatus::Outdated) && idx == body_rows.len() - 1;
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(indent.clone()),
            Span::styled("│ ", border_style),
            Span::styled(row.clone(), if is_note { note_style } else { body_style }),
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

/// A resolved thread collapses to one green row: `└─ ✓ line N ·
/// RESOLVED  <text>  · [o] reopen`. Truncated to the pane width; the
/// whole row is a [`BodyRowKey::CommentChip`] so a click reopens it.
fn render_resolved_chip(
    comment: &HunkComment,
    key: LineKey,
    gutter_width: usize,
    pane_width: u16,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let indent_cols = gutter_width + 4;
    let indent = " ".repeat(indent_cols);
    let green = Style::default().fg(theme::REVIEW_RESOLVED);
    let dim = Style::default().fg(theme::DIM);
    let head = format!("└─ ✓ line {} · RESOLVED  ", comment.line);
    let avail = usize::from(pane_width).saturating_sub(2 + indent_cols + head.chars().count() + 12);
    let text = comment.comment_text.replace('\n', " ");
    let snippet: String = text.chars().take(avail.max(8)).collect();
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(indent),
        Span::styled(head, green),
        Span::styled(snippet, dim),
        Span::styled("  · [o] reopen", dim),
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

    // Body rows - one per editor line. Empty editor shows a single
    // placeholder row so the user sees where typing will land.
    let inner_width = box_width.saturating_sub(2); // `│ ... │`
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
/// Falls back to `...`-suffix when truncation is needed.
fn fit_box_content(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    // When the budget is too small to fit the 3-char `...` marker,
    // just take that many raw chars so the output still respects
    // `max_chars`. Skipping the marker is the lesser harm versus
    // overflowing the box width.
    if max_chars < 3 {
        return text.chars().take(max_chars).collect();
    }
    let take = max_chars.saturating_sub(3);
    let truncated: String = text.chars().take(take).collect();
    format!("{truncated}...")
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

/// Build one split-view body row: left column + divider + right
/// column. Each column carries `[gutter] [+/-] [highlighted text]`,
/// truncated to fit. Empty sides (unbalanced rows) render as blank
/// fillers so the divider position stays consistent down the body.
fn split_diff_row(
    file: &FileHunks,
    pair: PairedDiffRow,
    gutter_width: usize,
    pane_width: u16,
    cache: Option<&FileHighlight>,
) -> Line<'static> {
    // Per-side body width: pane minus 2-col leading indent minus the
    // 3-col divider zone (space + '│' + space). Splits as floor/ceil
    // so any leftover odd column goes to the right (additions) side
    // - gives the `+` half a touch more breathing room than the `-`
    // half, which mirrors how most users read a diff (focus right).
    let indent_cols: usize = 2;
    let divider_cols: usize = 3;
    let usable = usize::from(pane_width).saturating_sub(indent_cols).saturating_sub(divider_cols);
    let left_width = usable / 2;
    let right_width = usable - left_width;

    let left = build_split_half(file, pair.left, gutter_width, left_width, cache);
    let right = build_split_half(file, pair.right, gutter_width, right_width, cache);

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
/// means this side is blank - fill with spaces sized to match the
/// other side so columns stay aligned. Text spans come from the
/// per-file highlight cache (truncated to fit; long lines clip in
/// split, matching the mockup's `overflow:hidden` columns).
fn build_split_half(
    file: &FileHunks,
    key: Option<LineKey>,
    gutter_width: usize,
    text_width: usize,
    cache: Option<&FileHighlight>,
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
    let raw_spans = cached_line_spans(cache, key).to_vec();
    let mut text_spans = truncate_spans_to_width(raw_spans, text_width);
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
        DiffLineKind::Added => ("+", Color::Green, Some(theme::DIFF_ADDITION_BG)),
        DiffLineKind::Removed => ("-", Color::Red, Some(theme::DIFF_DELETION_BG)),
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

fn banner_row(label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(label, Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)),
    ])
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
    // When the budget is too small to fit the 3-char `...` marker,
    // just take the last `max_width` chars so the output still
    // respects `max_width`. Skipping the marker is the lesser harm
    // versus overflowing the rail width.
    if max_width < 3 {
        let skip = path.chars().count().saturating_sub(max_width);
        return path.chars().skip(skip).collect();
    }
    let keep = max_width - 3;
    let mut chars = path.chars();
    let skip = path.chars().count().saturating_sub(keep);
    for _ in 0..skip {
        chars.next();
    }
    let mut out = String::with_capacity(max_width);
    out.push_str("...");
    out.extend(chars);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // `rail_width_for` tests live next to the function definition in
    // `crate::app::diff_overlay::tests` - this module only tests the
    // renderer-local helpers below.

    #[test]
    fn truncate_path_front_keeps_short_paths_intact() {
        assert_eq!(truncate_path_front("a/b.rs", 20), "a/b.rs");
    }

    #[test]
    fn truncate_path_front_front_truncates_long_paths() {
        let out = truncate_path_front("crates/forge-tui/src/ui/inspector_pane.rs", 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.starts_with("..."));
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

    #[test]
    fn unified_rows_emits_header_then_signed_lines_with_keys() {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        let file = FileHunks {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 2,
                new_start: 1,
                new_count: 2,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Context,
                        text: "ctx".into(),
                        old_line: Some(1),
                        new_line: Some(1),
                    },
                    DiffLine {
                        kind: DiffLineKind::Removed,
                        text: "old".into(),
                        old_line: Some(2),
                        new_line: None,
                    },
                    DiffLine {
                        kind: DiffLineKind::Added,
                        text: "new".into(),
                        old_line: None,
                        new_line: Some(2),
                    },
                ],
            }],
        };
        let rows = unified_rows(0, &file);
        assert_eq!(rows.len(), 4, "one header + three diff lines");

        // Hunk header: no gutter number, @@ text.
        assert!(matches!(rows[0].key, BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 }));
        assert_eq!(rows[0].line_no, None);
        assert!(rows[0].text.starts_with("@@"));

        // Context: new-side line number, key on the right.
        assert_eq!(rows[1].sign, ' ');
        assert_eq!(rows[1].line_no, Some(1));
        assert_eq!(
            rows[1].key,
            BodyRowKey::HunkRow {
                left: None,
                right: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }),
            }
        );

        // Removed: old-side line number, key on the left.
        assert_eq!(rows[2].sign, '-');
        assert_eq!(rows[2].line_no, Some(2));
        assert_eq!(
            rows[2].key,
            BodyRowKey::HunkRow {
                left: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 }),
                right: None,
            }
        );

        // Added: new-side line number, key on the right.
        assert_eq!(rows[3].sign, '+');
        assert_eq!(rows[3].line_no, Some(2));
        assert_eq!(
            rows[3].key,
            BodyRowKey::HunkRow {
                left: None,
                right: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 2 }),
            }
        );
    }

    #[test]
    fn file_height_collapsed_deleted_is_two_rows() {
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![FileHunks {
                path: "gone.rs".into(),
                status: FileStatus::Deleted,
                hunks: Vec::new(),
            }],
        );
        ensure_file_cached(&mut state, 0, 120);
        // Sticky header + the one-line "file deleted" notice.
        assert_eq!(state.measured_heights[0], Some(2));
    }

    #[test]
    fn file_height_counts_rendered_wrapped_rows() {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        // gutter for line numbers up to 2 = 2 cols; at pane width 40
        // content width = 40 - 2 indent - 2 gutter - 3 sign zone = 33,
        // so the 70-col line wraps into ceil(70 / 33) = 3 visual rows.
        let long = "x".repeat(70);
        let file = FileHunks {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Context,
                        text: "short".into(),
                        old_line: Some(1),
                        new_line: Some(1),
                    },
                    DiffLine {
                        kind: DiffLineKind::Added,
                        text: long,
                        old_line: None,
                        new_line: Some(2),
                    },
                ],
            }],
        };
        let mut state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp"), "HEAD".to_owned(), vec![file]);
        // ensure_file_cached highlights, then measures off the rendered
        // (wrapped) rows: 1 sticky header + 1 @@ header + 1 short + 3
        // wrapped = 6.
        ensure_file_cached(&mut state, 0, 40);
        assert_eq!(state.measured_heights[0], Some(6));
    }

    #[test]
    fn effective_view_mode_forces_unified_below_split_threshold() {
        assert_eq!(effective_view_mode(DiffViewMode::Split, 80), DiffViewMode::Unified);
        assert_eq!(
            effective_view_mode(DiffViewMode::Split, MIN_WIDTH_FOR_SPLIT),
            DiffViewMode::Split,
        );
        assert_eq!(effective_view_mode(DiffViewMode::Unified, 200), DiffViewMode::Unified);
    }

    #[test]
    fn narrow_pane_renders_unified_even_with_split_stored() {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        // One removed + one added line. Unified emits both as separate
        // rows (header + @@ + removed + added = 4); split pairs them
        // into one row (header + @@ + paired = 3). Measuring off the
        // rendered rows lets us tell which layout actually drew.
        let make = || FileHunks {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Removed,
                        text: "old".into(),
                        old_line: Some(1),
                        new_line: None,
                    },
                    DiffLine {
                        kind: DiffLineKind::Added,
                        text: "new".into(),
                        old_line: None,
                        new_line: Some(1),
                    },
                ],
            }],
        };
        // Split is the stored choice, but a 60-col pane is below the
        // split threshold, so it falls back to unified (4 rows).
        let mut narrow = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![make()],
        );
        narrow.view_mode = DiffViewMode::Split;
        ensure_file_cached(&mut narrow, 0, 60);
        assert_eq!(narrow.measured_heights[0], Some(4), "narrow pane falls back to unified");
        // A wide pane honors the stored split (3 rows).
        let mut wide = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![make()],
        );
        wide.view_mode = DiffViewMode::Split;
        ensure_file_cached(&mut wide, 0, 160);
        assert_eq!(wide.measured_heights[0], Some(3), "wide pane honors split");
    }

    // ---- commit stepper + jump dropdown rendering ----

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn stepper_geometry_reserves_four_rows() {
        assert_eq!(STEPPER_TITLE_ROW, 0, "title on the first row");
        assert_eq!(STEPPER_MOVE_ROW, 2, "movement row after a blank spacer");
        assert_eq!(STEPPER_HEIGHT, 4, "title, gap, movement, gap-before-diff");
    }

    #[test]
    fn jump_row_marker_covers_badge_and_current() {
        assert_eq!(jump_row_marker(0, false), "");
        assert_eq!(jump_row_marker(0, true), "\u{25c2}");
        assert_eq!(jump_row_marker(3, false), "\u{25cf} 3");
        assert_eq!(jump_row_marker(2, true), "\u{25cf} 2 \u{25c2}");
    }

    #[test]
    fn jump_commit_line_shows_index_sha_subject_badge_marker() {
        let line = jump_commit_line(60, 2, "a3f9c1e", "fix the threshold", 1, true, true);
        let text = line_text(&line);
        assert!(text.contains("2 \u{b7} "), "index prefix");
        assert!(text.contains("a3f9c1e"), "short sha");
        assert!(text.contains("fix the threshold"), "subject");
        assert!(text.contains("\u{25cf} 1"), "comment badge");
        assert!(text.contains("\u{25c2}"), "current-scope marker");
    }

    #[test]
    fn jump_commit_line_truncates_long_subject() {
        let line = jump_commit_line(24, 1, "abc1234", &"x".repeat(80), 0, false, false);
        assert!(line_text(&line).contains("..."), "an over-long subject is fitted to the box");
    }

    #[test]
    fn render_stepper_shows_branch_position_and_stashes_jump_span() {
        use crate::app::diff_overlay::DiffScope;
        use forge_workspace::env::git_diff::hunks::CommitMeta;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "main".to_owned(),
            vec![FileHunks { path: "a.rs".into(), status: FileStatus::Modified, hunks: vec![] }],
        );
        state.branch = Some("feat/x".to_owned());
        state.commits = vec![
            CommitMeta {
                sha: "a".into(),
                short_sha: "a3f9c1e".into(),
                subject: "fix threshold".into(),
                body: String::new(),
            },
            CommitMeta {
                sha: "b".into(),
                short_sha: "b90bbef".into(),
                subject: "wire banner".into(),
                body: String::new(),
            },
        ];
        state.scope = DiffScope::Commit(0);

        let width = 100u16;
        let backend = TestBackend::new(width, STEPPER_HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = Rect { x: 0, y: 0, width, height: STEPPER_HEIGHT };
                render_stepper(frame, area, &mut state);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(width);
        let title_row: String = (0..w).map(|x| buffer.content[x].symbol()).collect();
        let move_row: String = (0..w)
            .map(|x| buffer.content[usize::from(STEPPER_MOVE_ROW) * w + x].symbol())
            .collect();
        assert!(title_row.contains("COMMITS"), "title names the section");
        assert!(title_row.contains("feat/x"), "branch under review");
        assert!(title_row.contains("main"), "target");
        assert!(title_row.contains("2 commits"), "commit count");
        assert!(move_row.contains("1 / 2"), "position");
        assert!(move_row.contains("a3f9c1e"), "current commit's sha");
        assert!(move_row.contains("jump"), "jump affordance");
        assert_eq!(
            state.jump_hint_span.map(|(r, _, _)| r),
            Some(STEPPER_MOVE_ROW),
            "the jump click span is stashed on the movement row",
        );
    }

    #[test]
    fn whole_diff_mode_renders_no_stepper() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // No commits → whole-diff-only mode: the top row is the overlay
        // body/rail, never a COMMITS stepper (the additive guarantee).
        let mut app = App::test_default();
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![FileHunks { path: "a.rs".into(), status: FileStatus::Modified, hunks: vec![] }],
        );
        state.scanner_ok = true;
        assert!(state.commits.is_empty());
        app.diff_overlay = Some(state);

        let width = 130u16;
        let height = 20u16;
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(width);
        let row0: String = (0..w).map(|x| buffer.content[x].symbol()).collect();
        assert!(!row0.contains("COMMITS"), "whole-diff mode never shows the stepper");
    }

    #[test]
    fn commit_stepper_spaces_title_movement_and_diff() {
        use crate::app::diff_overlay::DiffScope;
        use forge_workspace::env::git_diff::hunks::CommitMeta;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Commit mode with the rail shown (>= 120 cols): the top chrome
        // breathes - COMMITS title (row 0), blank (row 1), movement row
        // (row 2), blank (row 3), then the FILES rail + diff (row 4).
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "main".to_owned(),
            vec![FileHunks { path: "a.rs".into(), status: FileStatus::Modified, hunks: vec![] }],
        );
        state.branch = Some("feat/x".to_owned());
        state.commits = vec![CommitMeta {
            sha: "a".into(),
            short_sha: "a3f9c1e".into(),
            subject: "fix threshold".into(),
            body: String::new(),
        }];
        state.scope = DiffScope::Commit(0);
        let mut app = App::test_default();
        app.diff_overlay = Some(state);

        let (width, height) = (130u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(width);
        let row =
            |r: usize| -> String { (0..w).map(|x| buffer.content[r * w + x].symbol()).collect() };

        assert!(row(0).contains("COMMITS"), "title on row 0: {:?}", row(0));
        assert!(row(1).trim().is_empty(), "blank spacer on row 1: {:?}", row(1));
        assert!(
            row(2).contains("a3f9c1e") && row(2).contains("jump"),
            "movement row on row 2: {:?}",
            row(2)
        );
        assert!(row(3).trim().is_empty(), "blank spacer on row 3: {:?}", row(3));
        assert!(row(4).contains("FILES"), "FILES rail begins on row 4: {:?}", row(4));

        assert_eq!(
            app.diff_overlay.as_ref().and_then(|o| o.jump_hint_span).map(|(r, _, _)| r),
            Some(2),
            "the jump-hint click span sits on the movement row (row 2)",
        );
    }

    // ---- commit-message block ----

    fn commit_state_with_body(subject: &str, body: &str) -> DiffOverlayState {
        use crate::app::diff_overlay::DiffScope;
        use forge_workspace::env::git_diff::hunks::CommitMeta;
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "main".to_owned(),
            vec![FileHunks { path: "a.rs".into(), status: FileStatus::Modified, hunks: vec![] }],
        );
        state.commits = vec![CommitMeta {
            sha: "a".into(),
            short_sha: "a3f9c1e".into(),
            subject: subject.to_owned(),
            body: body.to_owned(),
        }];
        state.scope = DiffScope::Commit(0);
        state
    }

    #[test]
    fn commit_message_block_shows_subject_and_body() {
        let state =
            commit_state_with_body("fix the threshold check", "why we split it\ninto its own fn");
        let lines = commit_message_block_lines(&state, 80);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("fix the threshold check"), "subject shown: {text}");
        assert!(text.contains("why we split it"), "body first line: {text}");
        assert!(text.contains("into its own fn"), "body second line: {text}");
        assert!(text.contains('\u{2502}'), "rust-orange rail glyph present: {text}");
    }

    #[test]
    fn commit_message_block_subject_only_has_rule_and_subject_only() {
        let lines = commit_message_block_lines(&commit_state_with_body("just a subject", ""), 80);
        assert_eq!(lines.len(), 2, "subject-only commit: rule + subject, no body rows");
        assert!(line_text(&lines[1]).contains("just a subject"));
    }

    #[test]
    fn commit_message_block_empty_in_whole_diff_scope() {
        // No commits → whole-diff scope → no message block (byte-identical
        // to the pre-enhancement overlay).
        let state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![FileHunks { path: "a.rs".into(), status: FileStatus::Modified, hunks: vec![] }],
        );
        assert!(commit_message_block_lines(&state, 80).is_empty());
    }

    #[test]
    fn commit_mode_renders_message_block_above_the_diff() {
        use crate::app::diff_overlay::{CachedScan, DiffScope};
        use forge_workspace::env::git_diff::hunks::{CommitMeta, DiffLine, Hunk};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let file = FileHunks {
            path: "rate_limit.rs".into(),
            status: FileStatus::Modified,
            hunks: vec![Hunk {
                old_start: 65,
                old_count: 1,
                new_start: 65,
                new_count: 2,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    text: "fn is_near_threshold() {".into(),
                    old_line: None,
                    new_line: Some(66),
                }],
            }],
        };
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "main".to_owned(),
            vec![file.clone()],
        );
        state.commits = vec![CommitMeta {
            sha: "a".into(),
            short_sha: "a3f9c1e".into(),
            subject: "fix the threshold check".into(),
            body: "split the near-threshold predicate".into(),
        }];
        state.scope = DiffScope::Commit(0);
        state.commit_cache = vec![Some(CachedScan { files: vec![file], scanner_ok: true })];
        let mut app = App::test_default();
        app.diff_overlay = Some(state);

        let (width, height) = (130u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let full: String =
            terminal.backend().buffer().content.iter().map(ratatui::buffer::Cell::symbol).collect();
        assert!(full.contains("fix the threshold check"), "subject renders above the diff");
        assert!(full.contains("split the near-threshold predicate"), "body renders above the diff");
        assert!(full.contains("is_near_threshold"), "the commit's diff still renders below it");
        assert!(full.contains("all changes / back"), "footer shows the `a` toggle hint");
    }

    fn chip_comment(line: u32, text: &str, status: Option<ReviewStatus>) -> HunkComment {
        let thread = status.map(|status| forge_primitives::ReviewThread {
            id: "t1".to_owned(),
            anchor: forge_primitives::ReviewAnchor {
                path: "a.rs".to_owned(),
                side: forge_primitives::ReviewSide::New,
                line,
                content_hash: 0,
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![forge_primitives::ReviewComment {
                author: forge_primitives::ReviewAuthor::User,
                text: text.to_owned(),
                at: String::new(),
            }],
            status,
            created_at: String::new(),
            updated_at: String::new(),
        });
        HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".to_owned(),
            line,
            hunk_context: Vec::new(),
            comment_text: text.to_owned(),
            commit: None,
            thread,
            authored_this_session: false,
            persisted: true,
        }
    }

    fn render_chip(comment: &HunkComment) -> (Vec<Line<'static>>, Vec<BodyRowKey>) {
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        render_comment_chip(comment, key, 4, 80, &mut lines, &mut keys);
        (lines, keys)
    }

    #[test]
    fn comment_chip_open_box_titles_and_tints_by_state() {
        let (lines, _) =
            render_chip(&chip_comment(7, "needs a bound check", Some(ReviewStatus::Open)));
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("\u{1f4ac} line 7 \u{b7} OPEN"),
            "open title carries state; got:\n{joined}"
        );
        let top = lines.first().expect("top border");
        assert_eq!(
            top.spans.last().expect("border span").style.fg,
            Some(theme::RUST_ORANGE),
            "open border is rust-orange",
        );
    }

    #[test]
    fn comment_chip_outdated_box_is_yellow_with_a_note() {
        let (lines, _) =
            render_chip(&chip_comment(72, "guard the None case", Some(ReviewStatus::Outdated)));
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("\u{b7} OUTDATED"), "outdated title");
        assert!(joined.contains("line changed"), "outdated note names the drift");
        assert_eq!(
            lines.first().expect("top").spans.last().expect("border").style.fg,
            Some(theme::STATUS_WARNING),
            "outdated border is yellow",
        );
    }

    #[test]
    fn comment_chip_resolved_collapses_to_one_green_row() {
        let (lines, keys) =
            render_chip(&chip_comment(88, "rename tok to token", Some(ReviewStatus::Resolved)));
        assert_eq!(lines.len(), 1, "resolved collapses to a single row");
        assert_eq!(keys.len(), 1);
        let text = line_text(&lines[0]);
        assert!(
            text.contains("\u{2713} line 88 \u{b7} RESOLVED"),
            "resolved one-liner; got: {text}"
        );
        assert!(text.contains("[o] reopen"), "reopen hint present");
    }

    #[test]
    fn comment_chip_ephemeral_keeps_classic_title() {
        let (lines, _) = render_chip(&chip_comment(3, "commit-scoped", None));
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("Comment on line 3"), "no-thread comment keeps the classic title");
        assert!(!joined.contains("\u{b7} OPEN"), "and carries no state label");
    }
}
