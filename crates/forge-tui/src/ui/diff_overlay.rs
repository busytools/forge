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
//! active editor expands inline below its anchor.

mod pairing;

use forge_primitives::{ReviewAuthor, ReviewSet, ReviewStatus};
use forge_workspace::env::git_diff::hunks::{DiffLineKind, FileHunks, FileStatus, Hunk};

use crate::app::diff_overlay::{
    ActiveCommentInput, AnchorNote, BodyRowKey, CommentRef, DiffScope, DiffViewMode, FileHighlight,
    HunkComment, LineKey, RailRowKey, SPLIT_MARKER_COLS, effective_view_mode, gutter_width_for,
    rail_width_for, split_layout,
};
use forge_workspace::env::git_diff::resolver::OutdatedReason;
use pairing::{PairedDiffRow, pair_hunk_lines};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::app::diff_overlay::DiffOverlayState;
use crate::app::emoji;
use crate::ui::autocomplete;
use crate::ui::chat_tree;
use crate::ui::composer::{self, ComposerChrome};
use crate::ui::highlight::LineHighlighter;
use crate::ui::theme;
use crate::ui::wrap::{expand_tabs, replace_control_chars, take_prefix_by_width};

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
    app.cached_frame_area = frame.area();

    let blip = blip_span_for(app);

    let Some(overlay) = app.diff_overlay.as_ref() else {
        super::page::render_page(frame, "Diff review", None, Line::default(), |frame, body| {
            render_missing_state(frame, body);
        });
        return;
    };

    // Build the key-hints footer before entering the scaffold body. The
    // effective view mode depends on the eventual pane width (body minus
    // the FILES rail), so derive it from the inner width up front.
    let body_width = frame.area().width.saturating_sub(2);
    let rail_width = rail_width_for(body_width);
    let sep = u16::from(rail_width > 0);
    let pane_width = body_width.saturating_sub(rail_width).saturating_sub(sep);
    // Asked of the same source Esc will consult, not of the cards: a
    // thread deleted from another view still has a card standing here,
    // and the hint must not offer a review that Esc will not open. The
    // card check is a necessary condition for the store one, so a session
    // that has written nothing never reaches the store.
    let seals = overlay.comments.iter().any(|c| c.authored_this_session)
        && crate::app::diff_overlay::would_file(app);
    let footer = footer_line(
        overlay,
        effective_view_mode(overlay.view_mode, pane_width),
        body_width,
        seals,
        blip.as_ref(),
    );

    super::page::render_page(frame, "Diff review", None, footer, |frame, body| {
        render_diff_body(frame, body, app);
    });
}

fn render_diff_body(frame: &mut Frame, area: Rect, app: &mut App) {
    // Reserve the top rows for the commit stepper (commit mode only);
    // the key-hints footer lives on the page scaffold, so the rail +
    // body fill the rest of the body rect.
    let stepper_h: u16 = if app.diff_overlay.as_ref().is_some_and(|o| !o.commits.is_empty()) {
        STEPPER_HEIGHT
    } else {
        0
    };
    let usable_height = area.height.saturating_sub(stepper_h);
    let mut usable_area = Rect {
        x: area.x,
        y: area.y.saturating_add(stepper_h),
        width: area.width,
        height: usable_height,
    };

    // A failed review-thread load draws a full-width notice above the
    // rail/body (and stays visible even if the body area collapses) so a
    // genuine load failure never reads as an empty review pane.
    if let Some(notice) = app.diff_overlay.as_ref().and_then(review_load_notice_line)
        && usable_area.height > 0
    {
        let notice_area = Rect { height: 1, ..usable_area };
        frame.render_widget(Paragraph::new(notice), notice_area);
        usable_area.y = usable_area.y.saturating_add(1);
        usable_area.height = usable_area.height.saturating_sub(1);
    }

    // A truncated take's warning, stamped where its words landed.
    if app.diff_overlay.as_ref().is_some_and(|o| o.dictate_notice.is_some())
        && usable_area.height > 0
    {
        let line = Line::from(Span::styled(
            format!(
                "  dictated words truncated - {}",
                crate::app::dictate::truncated_notice_text()
            ),
            Style::default().fg(theme::STATUS_WARNING),
        ));
        let notice_area = Rect { height: 1, ..usable_area };
        frame.render_widget(Paragraph::new(line), notice_area);
        usable_area.y = usable_area.y.saturating_add(1);
        usable_area.height = usable_area.height.saturating_sub(1);
    }

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
        // The rail highlights this same (message-adjusted) top file, so
        // its arrow tracks the body's pinned header in commit mode.
        o.current_file_idx = first_visible;
        // Leading commit-message rows the rail-click target must clear to
        // land in file-sub-document space (0 in whole-diff mode).
        o.message_rows = message_rows;
        o.pane_origin_row = pane_area.y;
        o.pane_origin_col = pane_area.x;
        o.pane_width = pane_area.width;
        o.content_origin_col = area.x;
        o.rail_origin_row = rail_area.map_or(usable_area.y, |r| r.y);
    }

    // 5. Draw: rail (if shown), separator, the pinned sticky header +
    //    the scrolling tail beneath it.
    if let (Some(rail_area), Some(sep_area)) = (rail_area, sep_area) {
        render_rail(frame, rail_area, app);
        render_separator(frame, sep_area);
    }
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

    // The Finish-review modal draws over the whole body when open.
    if app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some())
        && let Some(o) = app.diff_overlay.as_mut()
    {
        render_finish_review(frame, area, o);
    }
    // The reviews list takes over the body when open.
    if let Some(o) = app.diff_overlay.as_ref().filter(|o| o.reviews_open) {
        render_reviews_list(frame, area, o);
    }
    // The emoji picker sits above everything - it is the innermost
    // surface and its rows must not be painted over.
    render_emoji_dropdown(frame, area, app);
}

/// Paint the `:shortcode:` picker over the diff, anchored under whatever
/// editor it is filtering for: the Finish-review overview's submit row
/// when that modal is open, otherwise the inline comment editor's last
/// row (located via `body_keys`, the same parallel row index the click
/// handler reads).
fn render_emoji_dropdown(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.emoji.as_ref().filter(|e| !e.candidates.is_empty()) else {
        return;
    };
    let Some(overlay) = app.diff_overlay.as_ref() else {
        return;
    };

    let anchor_y = if let Some((btn_row, _, _)) = overlay.finish_submit_span {
        btn_row.saturating_add(1)
    } else if let Some(row) = editor_last_screen_row(overlay) {
        row.saturating_add(1)
    } else {
        return;
    };

    let rows = state.candidates.len().min(emoji::MAX_VISIBLE);
    let desired = u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2);
    // Prefer below the anchor; flip above when the bottom is too tight.
    let below = area.bottom().saturating_sub(anchor_y);
    let above = anchor_y.saturating_sub(area.y);
    let (y, height) = if desired <= below {
        (anchor_y, desired)
    } else if desired <= above {
        (anchor_y.saturating_sub(desired), desired)
    } else if below >= above {
        (anchor_y, below)
    } else {
        (area.y, above)
    };
    if height < 3 {
        return;
    }

    let width = area.width.saturating_sub(4).clamp(20, 44);
    let x = overlay.pane_origin_col.max(area.x).min(area.right().saturating_sub(width));
    let rect = Rect { x, y, width, height };
    let lines = autocomplete::emoji_dropdown_lines(state, usize::from(height.saturating_sub(2)));

    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(autocomplete::emoji_dropdown_block()), rect);
}

/// Screen row of the comment editor's last rendered row, or `None` when
/// the editor is off-screen. Mirrors the click handler's mapping:
/// `body_keys` runs parallel to the built body lines, the first
/// `body_head_rows` are pinned, and the remainder scrolls by
/// `body_tail_scroll`.
fn editor_last_screen_row(overlay: &DiffOverlayState) -> Option<u16> {
    let idx = overlay.body_keys.iter().rposition(|key| matches!(key, BodyRowKey::InputRow(_)))?;
    if idx < overlay.body_head_rows {
        return Some(overlay.pane_origin_row);
    }
    let tail_idx = idx - overlay.body_head_rows;
    let scrolled = tail_idx.checked_sub(overlay.body_tail_scroll)?;
    let head = u16::try_from(overlay.body_head_rows).unwrap_or(0);
    let offset = u16::try_from(scrolled).ok()?;
    Some(overlay.pane_origin_row.saturating_add(head).saturating_add(offset))
}

/// The full-width notice shown when this branch's persisted review
/// threads failed to load, so a decode / IO failure doesn't read as an
/// empty review pane. `None` when the load succeeded.
fn review_load_notice_line(overlay: &DiffOverlayState) -> Option<Line<'static>> {
    overlay.review_load_error.as_ref().map(|_| {
        Line::from(Span::styled(
            "  review comments failed to load - see logs",
            Style::default().fg(theme::STATUS_WARNING),
        ))
    })
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
    let branch =
        replace_control_chars(overlay.branch.clone().unwrap_or_else(|| "HEAD".to_owned()).into())
            .into_owned();
    let target = replace_control_chars(overlay.target.clone().into()).into_owned();
    let title = Line::from(vec![
        Span::raw("  "),
        Span::styled("COMMITS", accent_bold),
        Span::styled(" · ", dim),
        Span::styled(branch, accent),
        Span::styled(" vs ", dim),
        Span::styled(target.clone(), accent),
        Span::styled(format!(" · {n} commit{}", if n == 1 { "" } else { "s" }), dim),
    ]);
    let title_y = area.y.saturating_add(STEPPER_TITLE_ROW);
    frame.render_widget(
        Paragraph::new(title),
        Rect { x: area.x, y: title_y, width: area.width, height: 1 },
    );

    // Threads, not cards: one comment draws in every scope it belongs to,
    // so counting cards reports it once per scope visited.
    let total = {
        let mut ids: Vec<&str> = overlay.comments.iter().map(|c| c.thread.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    };
    let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
    match overlay.scope {
        DiffScope::Commit(i) => {
            let short = replace_control_chars(
                overlay.commits.get(i).map(|c| c.short_sha.clone()).unwrap_or_default().into(),
            )
            .into_owned();
            let subject = replace_control_chars(
                overlay.commits.get(i).map(|c| c.subject.clone()).unwrap_or_default().into(),
            )
            .into_owned();
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
            spans.push(Span::styled(format!("  (whole branch vs {target})"), dim));
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
    // Picture before the pad arithmetic: a raw control char charges a
    // column in `width()` and paints none, pulling the right cluster left.
    let short_sha = replace_control_chars(short_sha.into());
    let subject = replace_control_chars(subject.into());
    let index_label = format!("{index} \u{b7} ");
    let right = jump_row_marker(count, current);
    let right_w = right.width();
    let fixed = index_label.width() + short_sha.width() + 1;
    let gap = if right_w > 0 { 2 } else { 0 };
    let subj_budget =
        inner.saturating_sub(fixed).saturating_sub(right_w).saturating_sub(gap).max(1);
    let subject_fitted = fit_box_content(&subject, subj_budget);
    let used = fixed + subject_fitted.width() + right_w;
    let pad = inner.saturating_sub(used);
    let mut spans = vec![
        Span::styled("\u{2502} ", dim),
        Span::styled(index_label, idx_style),
        Span::styled(short_sha.into_owned(), sha_style),
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

/// Render the Finish-review modal centered over the diff: the session's
/// comment count + a short list, the optional overview editor, and the
/// `[ Submit review ]` button. Stashes the button's screen span on the
/// overlay so a click can resolve onto it. Keyboard-driven otherwise
/// (Ctrl+Enter submit, Esc back - see [`crate::app::diff_overlay`]).
fn render_finish_review(frame: &mut Frame, area: Rect, overlay: &mut DiffOverlayState) {
    // Caps on the two variable-length regions so a large review can't
    // grow the modal past the screen.
    const MAX_LIST: usize = 6;
    const EDITOR_ROWS: usize = 4;
    let dim = Style::default().fg(theme::DIM);
    let accent_bold = composer::border_style();

    // One row per thread, and the row has to be the card the last rebuild
    // touched: hydrate leaves out-of-scope cards first, so taking the
    // earliest shows the copy that never saw the reviewer's latest edit.
    let scope = overlay.current_commit_sha();
    let authored: Vec<&HunkComment> = overlay
        .comments
        .iter()
        .filter(|c| c.authored_this_session)
        .fold(Vec::new(), |mut acc: Vec<&HunkComment>, c| {
            match acc.iter_mut().find(|e| e.thread.id == c.thread.id) {
                // On-screen wins outright. With no card in this scope the
                // latest wins, since hydrate appends what it rebuilt and
                // the earlier entry is the one it did not touch.
                Some(seen) if c.commit == scope || seen.commit != scope => *seen = c,
                Some(_) => {}
                None => acc.push(c),
            }
            acc
        });
    let count = authored.len();

    let box_width = area.width.saturating_sub(8).clamp(44, 68);
    let bw = usize::from(box_width);
    let inner = bw.saturating_sub(4);
    let border = composer::border_style();

    let mut rows: Vec<Line<'static>> = Vec::new();
    // The count folds into the thick title.
    let title =
        format!("Finish review \u{b7} {count} comment{}", if count == 1 { "" } else { "s" });
    rows.push(composer::top_border(&title, bw, border));

    for c in authored.iter().take(MAX_LIST) {
        let name = c.path.rsplit('/').next().unwrap_or(c.path.as_str());
        let snippet = c.comment_text.lines().next().unwrap_or("");
        let list_row = fit_box_content(&format!("   \u{b7} {name}:{}   {snippet}", c.line), inner);
        rows.push(composer::side_bordered(Line::from(Span::styled(list_row, dim)), bw, border));
    }
    if count > MAX_LIST {
        rows.push(composer::side_bordered(
            Line::from(Span::styled(format!("   +{} more", count - MAX_LIST), dim)),
            bw,
            border,
        ));
    }

    rows.push(composer::side_bordered(Line::default(), bw, border));

    // The overview editor opens with the prompt glyph; the label row it
    // replaces is the placeholder now.
    let editor_lines =
        overlay.finish_review.as_ref().map(|f| f.editor.lines().to_vec()).unwrap_or_default();
    let (caret_row, caret_col) =
        overlay.finish_review.as_ref().map_or((0, 0), |f| f.editor.cursor());
    if editor_lines.iter().all(String::is_empty) {
        let mut content = vec![ComposerChrome::prompt_span()];
        // The caret cell rides this row too, so its budget is one
        // column tighter than a plain body row's.
        content.push(Span::styled(
            fit_box_content(PLACEHOLDER_OVERVIEW, inner.saturating_sub(3)),
            dim,
        ));
        content.push(ComposerChrome::caret_span());
        rows.push(composer::side_bordered(Line::from(content), bw, border));
        for _ in 1..EDITOR_ROWS {
            rows.push(composer::side_bordered(Line::default(), bw, border));
        }
    } else {
        for (idx, line) in editor_lines.iter().take(EDITOR_ROWS).enumerate() {
            let mut content: Vec<Span<'static>> = Vec::new();
            if idx == 0 {
                content.push(ComposerChrome::prompt_span());
            } else {
                content.push(Span::raw("  "));
            }
            // The glyph or indent costs the text two of the inner
            // columns, so a long overview line truncates inside the
            // box; the caret row is one column tighter still, or a
            // full-width line spills past the right border.
            let budget = if idx == caret_row { inner - 3 } else { inner - 2 };
            let fitted = fit_box_content(line, budget);
            if idx == caret_row {
                let (head, tail) = split_at_char(&fitted, caret_col.min(fitted.chars().count()));
                content.push(Span::raw(head.to_owned()));
                content.push(ComposerChrome::caret_span());
                content.push(Span::raw(tail.to_owned()));
            } else {
                content.push(Span::raw(fitted));
            }
            rows.push(composer::side_bordered(Line::from(content), bw, border));
        }
    }

    rows.push(composer::side_bordered(Line::default(), bw, border));

    // Button row, built with explicit spans so the submit hit-span is known.
    let btn = "[ Submit review ]";
    let hint = "Ctrl+Enter submit \u{b7} Esc back";
    rows.push(composer::side_bordered(
        Line::from(vec![
            Span::raw(" "),
            Span::styled(btn, accent_bold),
            Span::raw("     "),
            Span::styled(hint, dim),
        ]),
        bw,
        border,
    ));
    let button_row_idx = rows.len() - 1;

    rows.push(composer::bottom_border(bw, border));

    let box_height = u16::try_from(rows.len()).unwrap_or(u16::MAX).min(area.height);
    let x = area.x + area.width.saturating_sub(box_width) / 2;
    let y = area.y + area.height.saturating_sub(box_height) / 2;
    let rect = Rect { x, y, width: box_width, height: box_height };
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(rows), rect);

    // Submit span: "\u{2502} " (2) + leading space (1) precede the button.
    let btn_row_y = y.saturating_add(u16::try_from(button_row_idx).unwrap_or(0));
    let col_start = x.saturating_add(3);
    let col_end = col_start.saturating_add(u16::try_from(btn.width()).unwrap_or(0));
    overlay.finish_submit_span = Some((btn_row_y, col_start, col_end));
}

/// The `N open · M addressed · K resolved · J outdated` rollup, omitting
/// zero counts; empty when a review has no member comments.
fn rollup_str(open: usize, addressed: usize, resolved: usize, outdated: usize) -> String {
    let mut parts = Vec::new();
    if open > 0 {
        parts.push(format!("{open} open"));
    }
    if addressed > 0 {
        parts.push(format!("{addressed} addressed"));
    }
    if resolved > 0 {
        parts.push(format!("{resolved} resolved"));
    }
    if outdated > 0 {
        parts.push(format!("{outdated} outdated"));
    }
    parts.join(" \u{b7} ")
}

/// Render the `l` REVIEWS list over the diff body: newest review first,
/// each a header row (`#N  age  K comments  <rollup>`) plus a dim summary
/// line, framed by rules with a totals footer. The highlighted row is
/// accent-bold. Keyboard-driven (see [`crate::app::diff_overlay`]).
fn render_reviews_list(frame: &mut Frame, area: Rect, overlay: &DiffOverlayState) {
    let orange = Style::default().fg(theme::RUST_ORANGE);
    let dim = Style::default().fg(theme::DIM);
    let accent_bold = orange.add_modifier(Modifier::BOLD);
    let plain = Style::default();

    let width = usize::from(area.width);
    let rule = "\u{2500}".repeat(width);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let header_left = " REVIEWS";
    let header_right = "l  close ";
    let header_pad = width.saturating_sub(header_left.width() + header_right.width());
    lines.push(Line::from(vec![
        Span::styled(header_left, accent_bold),
        Span::raw(" ".repeat(header_pad)),
        Span::styled(header_right, dim),
    ]));
    lines.push(Line::from(Span::styled(rule.clone(), dim)));

    if overlay.review_rows.is_empty() {
        lines.push(Line::from(Span::styled("  no reviews yet", dim)));
    }

    for (idx, row) in overlay.review_rows.iter().enumerate() {
        let head = format!(
            "  #{:<3} {:<9} {} comment{}   {}",
            row.number,
            row.age,
            row.total,
            if row.total == 1 { "" } else { "s" },
            rollup_str(row.open, row.addressed, row.resolved, row.outdated),
        );
        let style = if idx == overlay.reviews_selected { accent_bold } else { plain };
        lines.push(Line::from(Span::styled(fit_box_content(&head, width), style)));
        if let Some(summary) = &row.summary {
            lines.push(Line::from(Span::styled(
                fit_box_content(&format!("       {summary}"), width),
                dim,
            )));
        }
    }

    lines.push(Line::from(Span::styled(rule, dim)));
    // The tally counts each comment once, so a thread carried across rounds
    // doesn't inflate the footer the way summing the rows would.
    let totals = &overlay.review_totals;
    let footer_rollup = rollup_str(totals.open, totals.addressed, totals.resolved, totals.outdated);
    let count = overlay.review_rows.len();
    let total = totals.comments;
    let footer = format!(
        "  {total} comment{} across {count} review{}{}",
        if total == 1 { "" } else { "s" },
        if count == 1 { "" } else { "s" },
        if footer_rollup.is_empty() { String::new() } else { format!("  \u{b7}  {footer_rollup}") },
    );
    lines.push(Line::from(Span::styled(fit_box_content(&footer, width), dim)));

    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).min(area.height);
    let rect = Rect { x: area.x, y: area.y, width: area.width, height };
    frame.render_widget(Clear, rect);
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
    // Highlight the leaf the sticky header pins - the body computes this
    // once (message-adjusted in commit mode) and stashes it, so the rail
    // never re-derives it from the raw `doc_scroll` and drifts by the
    // leading message block.
    let current_idx = overlay.current_file_idx;

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
    let prefix_cols = line_prefix.width();
    // Layout: "  " + tree_prefix + marker + " " + status + "  " + label + " " + chip
    // The chip lands at the right edge if it fits. `💬` is a two-column
    // glyph, so the chip is measured by display width plus the one-space
    // gap that precedes it.
    let chip_str = if comment_count > 0 { format!("💬 {comment_count}") } else { String::new() };
    let chip_cols = if comment_count > 0 { chip_str.width() + 1 } else { 0 };
    let fixed_cols = 1 + 1 + 1 + 2; // marker + " " + status_glyph + "  "
    let label_budget = inner_width
        .saturating_sub(prefix_cols)
        .saturating_sub(fixed_cols)
        .saturating_sub(chip_cols);
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

/// The pinned key-hints line for the page footer: scroll / page / `t`
/// toggle / click-to-comment / click-to-jump / Esc, with the effective
/// mode (unified / split) right-justified to `width`. With a comment
/// editor open it shows the editor's Enter/Esc hints instead.
///
/// `seals` has to be answered from the same source Esc consults; taken
/// from the cards it offers a review for a thread another view has
/// already deleted, and Esc then closes without one.
/// The key-hints bar. `blip` leads it when a take is live - the
/// overlay's fixed blip spot, visible however far the editor row has
/// scrolled.
fn footer_line(
    overlay: &DiffOverlayState,
    mode: DiffViewMode,
    width: u16,
    seals: bool,
    blip: Option<&Span<'static>>,
) -> Line<'static> {
    let dim = Style::default().fg(theme::DIM);
    let orange = Style::default().fg(theme::RUST_ORANGE);
    let count = overlay.comments.len();
    let mut spans = vec![Span::raw("  ")];
    if let Some(blip) = blip {
        spans.push(blip.clone());
    }
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
        let esc_label = if seals { "finish review" } else { "close" };
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
        if !commit_mode {
            hints.push(("click file", "jump"));
        }
        if !overlay.reviews.is_empty() {
            hints.push(("l", "reviews"));
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
    let pad = usize::from(width)
        .saturating_sub(left_width)
        .saturating_sub(mode_label.width())
        .saturating_sub(2);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(mode_label, orange));
    spans.push(Span::raw(" "));
    Line::from(spans)
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
    /// string. Body rows paint from the span cache, not from here, so
    /// this still carries unexpanded tabs.
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
                .map(|line| {
                    let text = replace_control_chars(expand_tabs(&line.text));
                    match line.kind {
                        DiffLineKind::Removed => old_hl.highlight(&text),
                        DiffLineKind::Added => new_hl.highlight(&text),
                        DiffLineKind::Context => {
                            let _ = old_hl.highlight(&text);
                            new_hl.highlight(&text)
                        }
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
    // Pictured per line before the chop, so the width budget is what
    // paints while newlines stay structural for the wrap.
    let pictured = |text: &str| {
        text.lines()
            .map(|l| replace_control_chars(l.into()).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    };
    for row in wrap_chip_body(&pictured(&commit.subject), content_width) {
        lines.push(Line::from(vec![Span::styled("\u{2502} ", rail), Span::styled(row, bold)]));
    }
    let body = commit.body.trim();
    if !body.is_empty() {
        lines.push(Line::from(Span::styled("\u{2502}", rail)));
        for row in wrap_chip_body(&pictured(body), content_width) {
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
                replace_control_chars(overlay.target.clone().into()),
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
    comments_by_key: &std::collections::HashMap<LineKey, Vec<&HunkComment>>,
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
    } else {
        if file.oversize {
            // The full-context snapshot tripped the size cap, so this
            // file shows a bounded diff and can't expand context.
            lines.push(Line::from(Span::styled(
                "    (file too large - context expansion disabled)",
                Style::default().fg(theme::STATUS_WARNING),
            )));
            keys.push(BodyRowKey::EmptyState);
        }
        if file.hunks.is_empty() {
            // An untracked file with no hunks was dropped by one of the
            // scan_untracked paths (size cap, non-regular, IO error), all
            // logged WARN under agent.env_git. A tracked file with no
            // hunks is a real binary diff. Differentiate so the user knows
            // whether to grep logs or accept the answer. An oversize file
            // whose bounded fallback was also too big keeps only the note.
            if !file.oversize {
                let message = if file.status == FileStatus::Untracked {
                    "    (untracked, content not surfaced - see logs (target: agent.env_git))"
                } else {
                    "    (binary file or no diff content)"
                };
                lines.push(Line::from(Span::styled(message, Style::default().fg(theme::DIM))));
                keys.push(BodyRowKey::EmptyState);
            }
        } else {
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
    }
    push_file_end_cap(overlay, file_idx, pane_width, lines, keys);
}

/// Close a file with the scroll-time boundary before the next file's
/// banded header: a dim `└─ end <path> ──` cap row naming the file that
/// just ended, then a blank spacer. Emitted after every file but the
/// last (the document just ends there). Both rows carry
/// [`BodyRowKey::FileEndCap`] so a click on them no-ops. The row count
/// matches [`crate::app::diff_overlay::layout::END_CAP_ROWS`].
fn push_file_end_cap(
    overlay: &DiffOverlayState,
    file_idx: usize,
    pane_width: u16,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    if file_idx + 1 >= overlay.files.len() {
        return;
    }
    let Some(file) = overlay.files.get(file_idx) else { return };
    lines.push(end_cap_line(&file.path, pane_width));
    keys.push(BodyRowKey::FileEndCap { file_idx });
    lines.push(Line::default());
    keys.push(BodyRowKey::FileEndCap { file_idx });
}

/// The `└─ end <path> ────────` boundary rule: the corner + label, then
/// dim dashes filling the pane width. The path front-truncates when the
/// label would overflow so the rule stays one row at any width.
fn end_cap_line(path: &str, pane_width: u16) -> Line<'static> {
    let dim = Style::default().fg(theme::DIM);
    let total = usize::from(pane_width);
    let prefix = "\u{2514}\u{2500} end ";
    let budget = total.saturating_sub(prefix.width()).saturating_sub(4).max(4);
    let shown = truncate_path_front(path, budget);
    let head = format!("{prefix}{shown} ");
    let dashes = total.saturating_sub(head.width());
    Line::from(vec![Span::styled(head, dim), Span::styled("\u{2500}".repeat(dashes), dim)])
}

/// Hidden new-side lines just before hunk `hunk_idx`, with the glyph to
/// draw: `↑` for the leading edge above the first hunk, `↕` for an
/// inter-hunk gap. `None` when nothing is hidden there (so no expander
/// renders at the true top of a file or between touching hunks).
fn hidden_lines_before_hunk(file: &FileHunks, hunk_idx: usize) -> Option<(&'static str, u32)> {
    let hunk = file.hunks.get(hunk_idx)?;
    if hunk_idx == 0 {
        let above = hunk.new_start.saturating_sub(1);
        (above > 0).then_some(("\u{2191}", above))
    } else {
        let prev = file.hunks.get(hunk_idx - 1)?;
        let gap = hunk.new_start.saturating_sub(prev.new_start.saturating_add(prev.new_count));
        (gap > 0).then_some(("\u{2195}", gap))
    }
}

/// Emit a dim `┈ ↕ N lines ┈` context-expander row before hunk `hunk_idx`
/// when lines are hidden there. Clicking it re-slices the file's pinned
/// wide snapshot at a wider level in memory (context is per-file), so
/// every expander in a file carries the same
/// [`BodyRowKey::ContextExpander`]. Suppressed for an `oversize` file,
/// whose snapshot is a bounded fallback with nothing more to reveal.
fn push_context_expander(
    file: &FileHunks,
    file_idx: usize,
    hunk_idx: usize,
    pane_width: u16,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    if file.oversize {
        return;
    }
    let Some((glyph, count)) = hidden_lines_before_hunk(file, hunk_idx) else { return };
    let dim = Style::default().fg(theme::DIM);
    let total = usize::from(pane_width);
    let label = format!(" {glyph} {count} line{} ", if count == 1 { "" } else { "s" });
    let lead = 3usize.min(total);
    let tail = total.saturating_sub(lead).saturating_sub(label.width());
    lines.push(Line::from(vec![
        Span::styled("\u{2508}".repeat(lead), dim),
        Span::styled(label, dim),
        Span::styled("\u{2508}".repeat(tail), dim),
    ]));
    keys.push(BodyRowKey::ContextExpander { file_idx });
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
    comments_by_key: &std::collections::HashMap<LineKey, Vec<&HunkComment>>,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let content_width = unified_content_width(pane_width, gutter_width).max(1);
    for row in unified_rows(file_idx, file) {
        match row.key {
            BodyRowKey::HunkHeader { hunk_idx, .. } => {
                push_context_expander(file, file_idx, hunk_idx, pane_width, lines, keys);
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
                    for (slot, comment) in
                        comments_by_key.get(&key).into_iter().flatten().enumerate()
                    {
                        render_comment_chip(
                            comment,
                            CommentRef { line: key, slot },
                            gutter_width,
                            pane_width,
                            &overlay.reviews,
                            overlay.is_comment_collapsed(comment),
                            lines,
                            keys,
                        );
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
    comments_by_key: &std::collections::HashMap<LineKey, Vec<&HunkComment>>,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    for (hunk_idx, hunk) in file.hunks.iter().enumerate() {
        push_context_expander(file, file_idx, hunk_idx, pane_width, lines, keys);
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
                for (slot, comment) in
                    comments_by_key.get(&side_key).into_iter().flatten().enumerate()
                {
                    render_comment_chip(
                        comment,
                        CommentRef { line: side_key, slot },
                        gutter_width,
                        pane_width,
                        &overlay.reviews,
                        overlay.is_comment_collapsed(comment),
                        lines,
                        keys,
                    );
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

/// Sticky file-divider header, rendered as a banded (filled-background)
/// bar so each file's start reads as a divider: caret + path (bold) +
/// status badge, with the `+N -M` totals right-justified. The band fills
/// the full pane width. `collapsed` picks the `▸` (collapsed) vs `▾`
/// (expanded) caret.
fn file_header_line(file: &FileHunks, pane_width: u16, collapsed: bool) -> Line<'static> {
    let band = Style::default().bg(theme::DIFF_FILE_HEADER_BG);
    let caret = if collapsed { "\u{25b8}" } else { "\u{25be}" };
    let (badge, badge_color) = status_badge(file.status);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("  ", band),
        Span::styled(caret, band.fg(theme::DIM)),
        Span::styled(" ", band),
        Span::styled(file.path.clone(), band.add_modifier(Modifier::BOLD)),
        Span::styled("  ", band),
        Span::styled(badge, band.fg(badge_color)),
    ];
    let added = file.added_count();
    let removed = file.removed_count();
    let counts = format!("+{added} -{removed}");
    let left_width: usize = spans.iter().map(Span::width).sum();
    // Fill to the full pane width so the band is one continuous bar, with
    // the totals right-justified behind a single trailing band cell.
    let pad = usize::from(pane_width)
        .saturating_sub(left_width)
        .saturating_sub(counts.width())
        .saturating_sub(1);
    spans.push(Span::styled(" ".repeat(pad), band));
    spans.push(Span::styled(format!("+{added}"), band.fg(Color::Green)));
    spans.push(Span::styled(" ", band));
    spans.push(Span::styled(format!("-{removed}"), band.fg(Color::Red)));
    spans.push(Span::styled(" ", band));
    Line::from(spans)
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
/// emission. Multiple comments can share a key when an outdated thread
/// is re-placed onto a surviving line that already carries one, so each
/// key maps to a list rendered top-to-bottom in insertion order.
fn index_comments_by_key<'a>(
    comments: &[&'a HunkComment],
) -> std::collections::HashMap<LineKey, Vec<&'a HunkComment>> {
    let mut map: std::collections::HashMap<LineKey, Vec<&'a HunkComment>> =
        std::collections::HashMap::with_capacity(comments.len());
    for c in comments {
        map.entry(c.key).or_default().push(*c);
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

/// The comment's first line, trimmed, for the collapsed marker.
fn first_line_of(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

/// The card's one-line account of what re-anchoring did to it, or `None`
/// when it simply stayed put. An `Outdated` thread always says something
/// even when it was loaded without a fresh re-anchor, so the state is
/// never shown without a reason.
fn anchor_note_text(note: Option<AnchorNote>, status: ReviewStatus) -> Option<String> {
    match note {
        Some(AnchorNote::Moved { from }) => Some(format!("moved from line {from}")),
        Some(AnchorNote::Outdated(OutdatedReason::Gone)) => {
            Some("the code this was on is gone".to_owned())
        }
        Some(AnchorNote::Outdated(OutdatedReason::Ambiguous { matches })) => {
            Some(format!("matched {matches} locations, not relocating"))
        }
        None if status == ReviewStatus::Outdated => {
            Some("line changed - resolve, or re-comment on a live line".to_owned())
        }
        None => None,
    }
}

/// Border colour + uppercase state label for a comment box, keyed off
/// its durable review-thread status.
fn review_state_style(status: ReviewStatus) -> (Color, &'static str) {
    match status {
        ReviewStatus::Open => (theme::RUST_ORANGE, "OPEN"),
        ReviewStatus::Addressed => (theme::REVIEW_ADDRESSED, "ADDRESSED"),
        ReviewStatus::Resolved => (theme::REVIEW_RESOLVED, "RESOLVED"),
        ReviewStatus::Outdated => (theme::STATUS_WARNING, "OUTDATED"),
    }
}

/// The dim chip tag for a comment: `R{number}` for the review the thread
/// first appeared in, else `unfiled` (not yet submitted, or a review id
/// with no matching review row). Anchored on the origin so the tag stays
/// put as later rounds add turns.
fn review_tag(origin_review: Option<&str>, reviews: &[ReviewSet]) -> String {
    origin_review
        .and_then(|id| reviews.iter().find(|r| r.id == id))
        .map_or_else(|| "unfiled".to_owned(), |r| format!("R{}", r.number))
}

/// The voice colour + label for one turn: the reviewer's own comments are
/// amber `you`; a worker reply is blue and carries its agent label.
fn turn_voice(author: &ReviewAuthor) -> (Color, String) {
    match author {
        ReviewAuthor::User => (theme::RUST_ORANGE, "you".to_owned()),
        ReviewAuthor::Agent { label } => (theme::REVIEW_ADDRESSED, label.clone()),
    }
}

/// Push one `│ <content> │` card row: pads `content` (already `vis` cells
/// wide) out to `content_width` and wraps it in the card's side borders.
fn push_card_row(
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
    indent: &str,
    card_style: Style,
    body_style: Style,
    content_width: usize,
    mut content: Vec<Span<'static>>,
    vis: usize,
    row_key: BodyRowKey,
) {
    let pad = content_width.saturating_sub(vis);
    let mut spans =
        vec![Span::raw("  "), Span::raw(indent.to_owned()), Span::styled("\u{2502} ", card_style)];
    spans.append(&mut content);
    spans.push(Span::styled(" ".repeat(pad), body_style));
    spans.push(Span::styled(" \u{2502}", card_style));
    lines.push(Line::from(spans));
    keys.push(row_key);
}

/// Render a review comment as a conversation card (mockup option I): a
/// header (`╭─ 💬 line N · R# ···· <state> ─╮`), an inner rail of turns -
/// a coloured dot per turn (amber `you`, blue worker) with the text hung
/// off the rail - then the `✓ Resolve` / `↺ Reopen` actions and a rounded
/// bottom border. Turns are the thread's comments in order.
fn render_comment_chip(
    comment: &HunkComment,
    at: CommentRef,
    gutter_width: usize,
    pane_width: u16,
    reviews: &[ReviewSet],
    collapsed: bool,
    lines: &mut Vec<Line<'static>>,
    keys: &mut Vec<BodyRowKey>,
) {
    let status = comment.thread.status;
    if collapsed {
        let indent = " ".repeat(gutter_width + 4);
        let summary = first_line_of(&comment.comment_text);
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::raw(indent),
            Span::styled(
                format!("\u{2570}\u{2500} \u{2713} line {} resolved \u{b7} ", comment.line),
                Style::default().fg(theme::REVIEW_RESOLVED),
            ),
            Span::styled(summary, Style::default().fg(theme::DIM)),
        ]));
        keys.push(BodyRowKey::CommentCollapsed { at });
        return;
    }
    let (accent, state_label) = review_state_style(status);
    let indent_cols = gutter_width + 4;
    let indent = " ".repeat(indent_cols);
    let left_offset = 2 + indent_cols;
    let right_pad = 2usize;
    let box_width = usize::from(pane_width).saturating_sub(left_offset + right_pad).max(24);
    // Neutral chrome (border + rail); the state label and turn dots carry
    // the colour. Whole card is tinted CHIP_BG so it reads as one block.
    let card_style = Style::default().fg(theme::DIM).bg(CHIP_BG);
    let body_style = Style::default().bg(CHIP_BG);
    let note_style = Style::default().fg(theme::DIM).bg(CHIP_BG);
    let content_width = box_width.saturating_sub(4);
    let text_width = content_width.saturating_sub(2);

    // Header: `╭─ 💬 line N · R# ···· <state> ─╮`. `·` fills between the
    // dim review tag on the left and the state label (accent) on the right.
    let title = format!("\u{1f4ac} line {}", comment.line);
    let tag = format!(" \u{b7} {} ", review_tag(comment.thread.origin_review(), reviews));
    let state = format!(" {state_label} ");
    let left_w = 3 + title.width() + tag.width();
    let right_w = state.width() + 2;
    let dots = box_width.saturating_sub(left_w + right_w).max(1);
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(indent.clone()),
        Span::styled("\u{256d}\u{2500} ", card_style),
        Span::styled(title, body_style.add_modifier(Modifier::BOLD)),
        Span::styled(tag, note_style),
        Span::styled("\u{b7}".repeat(dots), card_style),
        Span::styled(state, Style::default().fg(accent).bg(CHIP_BG).add_modifier(Modifier::BOLD)),
        Span::styled("\u{2500}\u{256e}", card_style),
    ]));
    keys.push(if status == ReviewStatus::Resolved {
        BodyRowKey::CommentCollapsed { at }
    } else {
        BodyRowKey::CommentChip(at.line)
    });

    let blank = |lines: &mut Vec<Line<'static>>, keys: &mut Vec<BodyRowKey>| {
        push_card_row(
            lines,
            keys,
            &indent,
            card_style,
            body_style,
            content_width,
            Vec::new(),
            0,
            BodyRowKey::CommentChip(at.line),
        );
    };
    blank(lines, keys);

    // Turns: one dot row (`● author`) then the wrapped text hung off the
    // rail (`│ text`), except the last turn whose rail ends (spaces). Fall
    // back to the editor text if the thread somehow carries no comments.
    let turns = &comment.thread.comments;
    let last = turns.len().saturating_sub(1);
    if turns.is_empty() {
        for row in wrap_chip_body(&comment.comment_text, text_width) {
            let vis = 2 + row.width();
            push_card_row(
                lines,
                keys,
                &indent,
                card_style,
                body_style,
                content_width,
                vec![Span::styled("  ", body_style), Span::styled(row, body_style)],
                vis,
                BodyRowKey::CommentChip(at.line),
            );
        }
    }
    for (i, turn) in turns.iter().enumerate() {
        let (voice, label) = turn_voice(&turn.author);
        // Your turns are clickable to edit in place (dim ✎ affordance);
        // an agent's turn is read-only chrome.
        let editable = matches!(turn.author, ReviewAuthor::User);
        let row_key = if editable {
            BodyRowKey::CommentTurn { at, turn_idx: i }
        } else {
            BodyRowKey::CommentChip(at.line)
        };
        let dot_style = Style::default().fg(voice).bg(CHIP_BG);
        let pencil = if editable { "  \u{270e}" } else { "" };
        let mut dot_spans = vec![
            Span::styled("\u{25cf} ", dot_style),
            Span::styled(label.clone(), dot_style.add_modifier(Modifier::BOLD)),
        ];
        if editable {
            dot_spans.push(Span::styled(pencil, note_style));
        }
        push_card_row(
            lines,
            keys,
            &indent,
            card_style,
            body_style,
            content_width,
            dot_spans,
            2 + label.width() + pencil.width(),
            row_key,
        );
        let rail = if i == last { " " } else { "\u{2502}" };
        for row in wrap_chip_body(&turn.text, text_width) {
            let vis = 2 + row.width();
            push_card_row(
                lines,
                keys,
                &indent,
                card_style,
                body_style,
                content_width,
                vec![Span::styled(format!("{rail} "), note_style), Span::styled(row, body_style)],
                vis,
                row_key,
            );
        }
    }

    if let Some(note) = anchor_note_text(comment.anchor_note, status) {
        let vis = 2 + note.width();
        push_card_row(
            lines,
            keys,
            &indent,
            card_style,
            body_style,
            content_width,
            vec![Span::styled("  ", note_style), Span::styled(note, note_style)],
            vis,
            BodyRowKey::CommentChip(at.line),
        );
    }

    // Reply line: appends a new user turn on click (no state change, no
    // nudge). A dim hint doubles as the affordance label.
    let reply_label = "\u{21b3} reply";
    let reply_hint = "  add a note";
    push_card_row(
        lines,
        keys,
        &indent,
        card_style,
        body_style,
        content_width,
        vec![
            Span::styled(reply_label, note_style.add_modifier(Modifier::BOLD)),
            Span::styled(reply_hint, note_style),
        ],
        reply_label.width() + reply_hint.width(),
        BodyRowKey::CommentReply { at },
    );

    blank(lines, keys);

    // Actions: `✓ Resolve   ↺ Reopen`. Resolve applies to Open / Addressed
    // / Outdated (accent, primary); Reopen applies to Addressed (secondary)
    // and Resolved (accent, primary). An inapplicable action is dim + not
    // clickable. Each clickable action carries its pane-relative span.
    let resolve_ok =
        matches!(status, ReviewStatus::Open | ReviewStatus::Addressed | ReviewStatus::Outdated);
    let reopen_ok = matches!(status, ReviewStatus::Addressed | ReviewStatus::Resolved);
    let resolve_label = "\u{2713} Resolve";
    let reopen_label = "\u{21ba} Reopen";
    let gap = "   ";
    let accent_style =
        Style::default().fg(theme::RUST_ORANGE).bg(CHIP_BG).add_modifier(Modifier::BOLD);
    let resolve_style = if resolve_ok { accent_style } else { note_style };
    let reopen_style = match status {
        ReviewStatus::Resolved => accent_style,
        ReviewStatus::Addressed => body_style,
        _ => note_style,
    };
    let base = u16::try_from(left_offset + 2).unwrap_or(u16::MAX);
    let resolve_end = base.saturating_add(u16::try_from(resolve_label.width()).unwrap_or(0));
    let reopen_start = resolve_end.saturating_add(u16::try_from(gap.width()).unwrap_or(0));
    let reopen_end = reopen_start.saturating_add(u16::try_from(reopen_label.width()).unwrap_or(0));
    let content_w = resolve_label.width() + gap.width() + reopen_label.width();
    push_card_row(
        lines,
        keys,
        &indent,
        card_style,
        body_style,
        content_width,
        vec![
            Span::styled(resolve_label, resolve_style),
            Span::styled(gap, body_style),
            Span::styled(reopen_label, reopen_style),
        ],
        content_w,
        BodyRowKey::CommentButton {
            at,
            resolve: resolve_ok.then_some((base, resolve_end)),
            reopen: reopen_ok.then_some((reopen_start, reopen_end)),
        },
    );

    // Rounded bottom border.
    let bottom = format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(box_width.saturating_sub(2)));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::raw(indent),
        Span::styled(bottom, card_style),
    ]));
    keys.push(BodyRowKey::CommentChip(at.line));
}

/// Wrap `text` into rows that fit within `max_chars`, respecting
/// explicit newlines. A line longer than `max_chars` is chopped at
/// the boundary (no word-aware soft-wrap in v1; the use case is
/// short review notes where character-based wrap is fine).
fn wrap_chip_body(text: &str, max_cols: usize) -> Vec<String> {
    if max_cols == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    for source_line in text.lines() {
        if source_line.is_empty() {
            out.push(String::new());
            continue;
        }
        // Chop by display width so a body of emoji or CJK can't render
        // twice as wide as the card it sits in.
        let mut rest = source_line.to_owned();
        while !rest.is_empty() {
            let (chunk, tail) = crate::ui::wrap::take_prefix_by_width(&rest, max_cols);
            out.push(chunk);
            rest = tail;
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
///   ┏━ Comment on line 371 ━━━━━━━━━━━━━━━━━━━┓
///   ┃ ➤ user typed text here                  ┃
///   ┃   another line of the comment           ┃
///   ┃ Enter save · Esc cancel                 ┃  (DIM hint)
///   ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
/// ```
const PLACEHOLDER_COMMENT: &str = "Add a comment\u{2026}";
const HINT_SAVE_CANCEL: &str = "Enter save \u{b7} Esc cancel";
const PLACEHOLDER_OVERVIEW: &str = "Overview (optional)\u{2026}";

/// The blip for the active session's live take, if any.
fn blip_span_for(app: &App) -> Option<Span<'static>> {
    crate::app::dictate::blip_span(app, app.spinner_epoch.elapsed().as_secs_f32() * 1000.0)
}

/// Split a row at the `n`th char for the caret insert, char-counted
/// like the editor's own caret columns.
fn split_at_char(text: &str, n: usize) -> (&str, &str) {
    let byte = text.char_indices().nth(n).map_or(text.len(), |(i, _)| i);
    text.split_at(byte)
}

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
    let border = crate::ui::composer::border_style();
    let dim = Style::default().fg(theme::DIM);

    let mut push_row = |spans: Vec<Span<'static>>, keys: &mut Vec<BodyRowKey>| {
        let mut row = vec![Span::raw("  "), Span::raw(indent.clone())];
        row.extend(spans);
        lines.push(Line::from(row));
        keys.push(BodyRowKey::InputRow(input.key));
    };

    // Top border with embedded title.
    let top = crate::ui::composer::top_border(
        &format!("Comment on line {anchor_line}"),
        box_width,
        border,
    );
    push_row(top.spans, keys);

    // Body rows - one per editor line. Empty editor shows a single
    // placeholder row so the user sees where typing will land. The
    // prompt glyph leads the draft and costs the body two columns.
    let inner_width = box_width.saturating_sub(4);
    let editor_lines = input.editor.lines();
    let (caret_row, caret_col) = input.editor.cursor();
    let empty = editor_lines.is_empty() || editor_lines.iter().all(String::is_empty);
    let body_rows: Vec<String> = if empty { Vec::new() } else { editor_lines.to_vec() };
    if empty {
        let mut content = vec![ComposerChrome::prompt_span()];
        // The caret cell rides this row too, so its budget is one
        // column tighter than a plain body row's.
        content.push(Span::styled(
            fit_box_content(PLACEHOLDER_COMMENT, inner_width.saturating_sub(3)),
            dim,
        ));
        content.push(ComposerChrome::caret_span());
        push_row(
            crate::ui::composer::side_bordered(Line::from(content), box_width, border).spans,
            keys,
        );
    }
    for (idx, body_row) in body_rows.iter().enumerate() {
        // The caret row carries the caret cell beside its text, so its
        // budget is one column tighter or a full-width line spills past
        // the right border.
        let budget = if idx == caret_row { inner_width - 3 } else { inner_width - 2 };
        let fitted = fit_box_content(body_row, budget);
        let mut content: Vec<Span<'static>> = Vec::new();
        if idx == 0 {
            content.push(ComposerChrome::prompt_span());
        } else {
            content.push(Span::raw("  "));
        }
        if idx == caret_row {
            let (head, tail) = split_at_char(&fitted, caret_col.min(fitted.chars().count()));
            content.push(Span::raw(head.to_owned()));
            content.push(ComposerChrome::caret_span());
            content.push(Span::raw(tail.to_owned()));
        } else {
            content.push(Span::raw(fitted));
        }
        push_row(
            crate::ui::composer::side_bordered(Line::from(content), box_width, border).spans,
            keys,
        );
    }

    // In-box hint row (DIM).
    let hint_fitted = fit_box_content(HINT_SAVE_CANCEL, inner_width.saturating_sub(2));
    push_row(
        crate::ui::composer::side_bordered(
            Line::from(vec![Span::styled(hint_fitted, dim)]),
            box_width,
            border,
        )
        .spans,
        keys,
    );

    // Bottom border.
    let bottom = crate::ui::composer::bottom_border(box_width, border);
    push_row(bottom.spans, keys);
}

/// Trim `text` to fit within `max_cols` terminal columns. Used by the
/// editor dialog body to keep rows from overflowing the box width.
/// Falls back to `...`-suffix when truncation is needed.
///
/// Budgets by display width, not character count - a comment carrying
/// emoji or CJK is twice as wide as its `chars().count()` suggests, and
/// measuring it by characters walks the box's right border off the box.
fn fit_box_content(text: &str, max_cols: usize) -> String {
    if crate::ui::wrap::display_width(text) <= max_cols {
        return text.to_owned();
    }
    // When the budget is too small to fit the 3-column `...` marker,
    // just take what fits so the output still respects `max_cols`.
    // Skipping the marker is the lesser harm versus overflowing.
    if max_cols < 3 {
        return crate::ui::wrap::truncate_to_width(text, max_cols);
    }
    let truncated = crate::ui::wrap::truncate_to_width(text, max_cols.saturating_sub(3));
    format!("{truncated}...")
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
    // Any leftover odd column goes to the right (additions) side,
    // giving the `+` half a touch more breathing room than the `-`
    // half, which mirrors how most users read a diff (focus right).
    let layout = split_layout(gutter_width, pane_width);

    let left = build_split_half(file, pair.left, gutter_width, layout.left_width, cache);
    let right = build_split_half(file, pair.right, gutter_width, layout.right_width, cache);

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
        // Blank side: pad to the same width a filled half occupies, so
        // the divider stays where `split_layout` says.
        let pad_width = gutter_width.saturating_add(SPLIT_MARKER_COLS).saturating_add(text_width);
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

/// Truncate a span list to `max_width` display columns, cutting the
/// last span by grapheme cluster if necessary. Returns an empty vec
/// when `max_width == 0`.
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
        // Measured the same way as the whole-span check above. A
        // per-char sum disagrees with it on multi-codepoint clusters, so
        // the kept prefix would overrun the column it was cut to fit.
        let (kept, _) = take_prefix_by_width(span.content.as_ref(), max_width - consumed);
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
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
    // `crate::app::diff_overlay::layout` - this module only tests the
    // renderer-local helpers below.

    #[test]
    fn review_tag_labels_filed_and_unfiled_comments() {
        let reviews = vec![
            ReviewSet { id: "r-a".to_owned(), number: 1, summary: None, created_at: String::new() },
            ReviewSet { id: "r-b".to_owned(), number: 3, summary: None, created_at: String::new() },
        ];
        assert_eq!(review_tag(Some("r-b"), &reviews), "R3", "a filed comment shows its number");
        assert_eq!(review_tag(None, &reviews), "unfiled", "an unfiled comment reads unfiled");
        assert_eq!(
            review_tag(Some("gone"), &reviews),
            "unfiled",
            "an orphan id degrades to unfiled"
        );
    }

    #[test]
    fn rollup_str_omits_zero_counts() {
        assert_eq!(
            rollup_str(2, 1, 1, 1),
            "2 open \u{b7} 1 addressed \u{b7} 1 resolved \u{b7} 1 outdated",
        );
        assert_eq!(rollup_str(0, 0, 3, 0), "3 resolved");
        assert_eq!(rollup_str(0, 4, 0, 0), "4 addressed");
        assert_eq!(rollup_str(0, 0, 0, 0), "", "a review with no members has an empty rollup");
    }

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

    /// Emoji occupy two terminal columns. Wrapping a card body by
    /// character count lets a row of them render twice as wide as the
    /// card, spilling past its right border.
    #[test]
    fn wrap_chip_body_budgets_by_display_width_not_char_count() {
        let rows = wrap_chip_body(&"\u{1F600}".repeat(20), 20);
        for row in &rows {
            assert!(
                crate::ui::wrap::display_width(row) <= 20,
                "row {row:?} is {} columns wide, budget was 20",
                crate::ui::wrap::display_width(row)
            );
        }
    }

    #[test]
    fn fit_box_content_budgets_by_display_width_not_char_count() {
        let fitted = fit_box_content("\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}", 6);
        assert!(
            crate::ui::wrap::display_width(&fitted) <= 6,
            "fitted {fitted:?} is {} columns wide, budget was 6",
            crate::ui::wrap::display_width(&fitted)
        );
    }

    #[test]
    fn the_comment_editor_wears_the_unified_thick_chrome() {
        let input = ActiveCommentInput {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            editor: crate::app::InputState::new(),
            prior_comment: None,
            edit_turn: None,
        };
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        render_active_input(&input, 4, 42, 80, &mut lines, &mut keys);

        let texts: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert!(
            texts[0].contains("\u{250f}\u{2501} Comment on line 42 ")
                && texts[0].ends_with("\u{2513}"),
            "the top border is thick with the title embedded, got: {}",
            texts[0]
        );
        assert!(
            texts.iter().all(|t| !t.contains('\u{250c}') && !t.contains("\u{2502}")),
            "no thin chrome remains, got: {texts:?}"
        );
        let border = lines[0].spans[2].style;
        assert_eq!(border.fg, Some(theme::RUST_ORANGE), "the chrome is orange");
        assert!(border.add_modifier.contains(ratatui::style::Modifier::BOLD), "and bold");

        let body = &texts[1];
        assert!(
            body.contains("\u{27a4}") && body.contains("Add a comment\u{2026}"),
            "the glyph leads and the placeholder reads the unified wording, got: {body}"
        );
        let placeholder_span = lines[1]
            .spans
            .iter()
            .find(|span| span.content.contains("Add a comment"))
            .expect("the placeholder span");
        assert_eq!(placeholder_span.style.fg, Some(theme::DIM), "the placeholder dims");
        let caret = lines[1]
            .spans
            .iter()
            .find(|span| span.style.add_modifier.contains(ratatui::style::Modifier::REVERSED))
            .expect("a caret cell rides the placeholder row");
        assert_eq!(caret.content, " ", "the caret is a cell, not a glyph");

        assert!(
            texts[2].contains("Enter save \u{b7} Esc cancel"),
            "the hint row stands, got: {}",
            texts[2]
        );
        let bottom = &texts[texts.len() - 1];
        assert!(
            bottom.contains("\u{2517}") && bottom.ends_with("\u{251b}"),
            "the bottom border is thick, got: {bottom}"
        );
    }

    #[test]
    fn the_comment_editor_shows_the_caret_in_its_box_rows() {
        let mut editor = crate::app::InputState::new();
        editor.insert_str("ship it");
        let input = ActiveCommentInput {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            editor,
            prior_comment: None,
            edit_turn: None,
        };
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        render_active_input(&input, 4, 42, 80, &mut lines, &mut keys);
        let body =
            lines[1].spans.iter().map(|span| span.content.as_ref()).collect::<Vec<_>>().join("");
        assert!(body.contains("ship it"), "the draft renders, got: {body}");
        let caret = lines[1]
            .spans
            .iter()
            .find(|span| span.style.add_modifier.contains(ratatui::style::Modifier::REVERSED))
            .expect("the caret cell rides the caret row");
        assert_eq!(caret.content, " ", "the caret sits after the last typed char");
        assert!(
            !lines[1].spans.iter().any(|span| span.content.contains("Add a comment")),
            "a non-empty draft carries no placeholder"
        );
    }

    /// The reviewer's reproduction, finish-review shape: a full-width
    /// overview line with the caret parked at its end must not push the
    /// caret cell past the modal's right border.
    #[test]
    fn a_full_width_overview_line_keeps_the_border() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp/repo"), "main".to_owned(), vec![]);
        state.finish_review = Some(crate::app::diff_overlay::types::FinishReviewState {
            editor: crate::app::input::InputState::new(),
        });
        state.finish_review.as_mut().expect("finish review").editor.insert_str(&"x".repeat(61));

        let (width, height) = (80u16, 20u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                render_finish_review(
                    frame,
                    ratatui::layout::Rect { x: 0, y: 0, width, height },
                    &mut state,
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(width);
        let modal_width = usize::from(width.saturating_sub(8).clamp(44, 68));
        let modal_x =
            usize::from(width.saturating_sub(u16::try_from(modal_width).unwrap_or(0)) / 2);
        for r in 0..usize::from(height) {
            let row: String = (0..w).map(|x| buffer.content[r * w + x].symbol()).collect();
            let inside: String = row.chars().skip(modal_x).take(modal_width).collect();
            assert!(
                crate::ui::wrap::display_width(inside.trim_end()) <= modal_width,
                "row {r} spills past the modal's {modal_width} columns: {inside:?}"
            );
        }
        let overview_row = (0..usize::from(height))
            .map(|r| (0..w).map(|x| buffer.content[r * w + x].symbol()).collect::<String>())
            .find(|row| row.contains('\u{27a4}'))
            .expect("the overview editor renders");
        assert!(
            overview_row.trim_end().ends_with('\u{2503}'),
            "the right border survives the caret cell, got: {overview_row:?}"
        );
    }

    /// The reviewer's reproduction: a line exactly as wide as the
    /// caret row's budget, caret parked at its end. The caret cell has
    /// to fit beside the text - a full-width line must not push the
    /// caret row past the right border.
    #[test]
    fn a_full_width_line_with_the_caret_at_its_end_keeps_the_border() {
        let mut editor = crate::app::InputState::new();
        // 61 chars: exactly the caret row's budget at this geometry.
        editor.insert_str(&"x".repeat(61));
        let input = ActiveCommentInput {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            editor,
            prior_comment: None,
            edit_turn: None,
        };
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        render_active_input(&input, 4, 42, 80, &mut lines, &mut keys);

        let widths: Vec<usize> = lines.iter().map(crate::ui::wrap::line_display_width).collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "every row keeps the box's width with the caret cell fitted in, got {widths:?}"
        );
        let caret_row = &lines[1];
        let text: String = caret_row.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.ends_with(" \u{2503}"), "the right border survives, got: {text:?}");
        assert!(
            caret_row
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(ratatui::style::Modifier::REVERSED)),
            "the caret cell rides the full-width row"
        );
    }

    /// With a take live, the overlay's key-hints bar carries the circle
    /// blip - the fixed spot, visible however far the editor row has
    /// scrolled - and the editor's glyph stands.
    #[test]
    fn the_key_hints_bar_blips_while_a_take_is_live() {
        use forge_workspace::SessionUpdate;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![FileHunks {
                path: "src/x.rs".into(),
                status: FileStatus::Added,
                oversize: false,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_count: 0,
                    new_start: 1,
                    new_count: 1,
                    lines: vec![forge_workspace::env::git_diff::hunks::DiffLine {
                        kind: DiffLineKind::Added,
                        text: "let y = compute();".into(),
                        old_line: None,
                        new_line: Some(1),
                    }],
                }],
            }],
        );
        state.active_input = Some(crate::app::diff_overlay::ActiveCommentInput {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            editor: crate::app::InputState::new(),
            prior_comment: None,
            edit_turn: None,
        });
        app.diff_overlay = Some(state);
        crate::app::view::set_active_view(&mut app, crate::app::ActiveView::Diff);
        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key, floor_db: -50.0, generation: 1 },
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let w = 100usize;
        let rows: Vec<String> = (0..30)
            .map(|y| (0..w).map(|x| buffer.content[y * w + x].symbol()).collect::<String>())
            .collect();
        assert!(
            rows.iter().any(|row| row.contains('\u{25cf}')),
            "the blip shows somewhere on the overlay while a take is live, got: {rows:?}"
        );
        let editor_row = rows
            .iter()
            .find(|row| row.contains(PLACEHOLDER_COMMENT))
            .expect("the comment editor renders");
        assert!(
            editor_row.contains('\u{27a4}') && !editor_row.contains('\u{25cf}'),
            "the editor's glyph stands; the blip lives on the chrome, got: {editor_row}"
        );
    }

    #[test]
    fn the_finish_review_wears_the_unified_thick_chrome() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp/repo"), "main".to_owned(), vec![]);
        state.finish_review = Some(crate::app::diff_overlay::types::FinishReviewState {
            editor: crate::app::input::InputState::new(),
        });
        let mut long = chip_comment(5, "FIRST", ReviewStatus::Open);
        long.thread.id = "thread-FIRST".to_owned();
        long.comment_text = format!("{} overflow", "a very long review snippet ".repeat(4));
        long.authored_this_session = true;
        state.comments.push(long);
        let mut c = chip_comment(5, "SECOND", ReviewStatus::Open);
        c.thread.id = "thread-SECOND".to_owned();
        c.comment_text = "SECOND".to_owned();
        c.authored_this_session = true;
        state.comments.push(c);

        let (width, height) = (80u16, 20u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                render_finish_review(
                    frame,
                    ratatui::layout::Rect { x: 0, y: 0, width, height },
                    &mut state,
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(width);
        let rows: String = (0..usize::from(height))
            .map(|r| (0..w).map(|x| buffer.content[r * w + x].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");

        // The modal is 68 wide at an 80-col area; no row may spill past
        // its right border, whatever the snippet length.
        let modal_width = usize::from(width.saturating_sub(8).clamp(44, 68));
        let modal_x =
            usize::from(width.saturating_sub(u16::try_from(modal_width).unwrap_or(0)) / 2);
        for r in 0..usize::from(height) {
            let row: String = (0..w).map(|x| buffer.content[r * w + x].symbol()).collect();
            let inside: String = row.chars().skip(modal_x).take(modal_width).collect();
            let row_width = crate::ui::wrap::display_width(inside.trim_end());
            assert!(
                row_width <= modal_width,
                "row {r} spills past the modal's {modal_width} columns: {inside:?}"
            );
        }

        assert!(
            rows.contains("\u{250f}\u{2501} Finish review \u{b7} 2 comments "),
            "the count folds into the thick title, got:\n{rows}"
        );
        assert!(
            !rows.contains("in this review"),
            "the old count row is absorbed into the title, got:\n{rows}"
        );
        assert!(
            rows.contains('\u{27a4}') && rows.contains("Overview (optional)\u{2026}"),
            "the overview editor opens with the glyph and the unified placeholder, got:\n{rows}"
        );
        assert!(!rows.contains('\u{250c}'), "no thin corners remain, got:\n{rows}");
        assert!(rows.contains("[ Submit review ]"), "the submit affordance stays, got:\n{rows}");
        assert!(
            rows.contains("\u{2517}") && rows.contains("\u{251b}"),
            "the bottom border is thick, got:\n{rows}"
        );
    }

    /// Emoji occupy two terminal columns. Wrapping a card body by
    /// character count lets a row of them render twice as wide as the
    /// card, spilling past its right border.
    #[test]
    fn comment_editor_box_rows_align_with_an_emoji_in_the_body() {
        let mut editor = crate::app::InputState::new();
        editor.insert_str("ship it \u{1F680} nice");
        let input = ActiveCommentInput {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            editor,
            prior_comment: None,
            edit_turn: None,
        };
        let mut lines = Vec::new();
        let mut keys = Vec::new();

        render_active_input(&input, 4, 371, 80, &mut lines, &mut keys);

        let widths: Vec<usize> = lines.iter().map(crate::ui::wrap::line_display_width).collect();
        assert!(widths.len() >= 4, "border, body, hint, border");
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "editor box rows must share one column count, got {widths:?}"
        );
    }

    #[test]
    fn unified_rows_emits_header_then_signed_lines_with_keys() {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        let file = FileHunks {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            oversize: false,
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
                oversize: false,
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
            oversize: false,
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
    fn narrow_pane_renders_unified_even_with_split_stored() {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        // One removed + one added line. Unified emits both as separate
        // rows (header + @@ + removed + added = 4); split pairs them
        // into one row (header + @@ + paired = 3). Measuring off the
        // rendered rows lets us tell which layout actually drew.
        let make = || FileHunks {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            oversize: false,
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
            vec![FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                hunks: vec![],
                oversize: false,
            }],
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
    fn review_load_notice_shows_only_on_error() {
        let mut state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp"), "main".to_owned(), vec![]);
        assert!(review_load_notice_line(&state).is_none(), "no notice when the load succeeded");
        state.review_load_error = Some("boom".to_owned());
        let line = review_load_notice_line(&state).expect("notice present on error");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("failed to load"), "the notice names the failure");
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
            vec![FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                hunks: vec![],
                oversize: false,
            }],
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
            vec![FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                hunks: vec![],
                oversize: false,
            }],
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
        // Interior of a row, stripping the page border columns so the
        // stepper's own layout is asserted (the box shifts everything
        // down one row and in one column).
        let row = |r: usize| -> String {
            (1..w - 1).map(|x| buffer.content[r * w + x].symbol()).collect()
        };

        assert!(row(0).contains("Diff review"), "the page title tops the box: {:?}", row(0));
        assert!(row(1).contains("COMMITS"), "title on row 1: {:?}", row(1));
        assert!(row(2).trim().is_empty(), "blank spacer on row 2: {:?}", row(2));
        assert!(
            row(3).contains("a3f9c1e") && row(3).contains("jump"),
            "movement row on row 3: {:?}",
            row(3)
        );
        assert!(row(4).trim().is_empty(), "blank spacer on row 4: {:?}", row(4));
        assert!(row(5).contains("FILES"), "FILES rail begins on row 5: {:?}", row(5));

        assert_eq!(
            app.diff_overlay.as_ref().and_then(|o| o.jump_hint_span).map(|(r, _, _)| r),
            Some(3),
            "the jump-hint click span sits on the movement row (row 3)",
        );
    }

    // ---- render -> click round-trip at the real border offset ----

    /// The overlay caches spans built here for both the unified and the
    /// split body, so this is the only place either can pick up a tab.
    #[test]
    fn build_file_highlight_expands_tabs() {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        let file = FileHunks {
            path: "main.go".into(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Added,
                    text: "\t\treturn err".into(),
                    old_line: None,
                    new_line: Some(1),
                }],
            }],
        };
        let highlight = build_file_highlight(&file);
        let text: String = highlight[0][0].iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(text, "        return err", "two tabs reach column 8");
    }

    /// A split row has to fit the pane. Reserving only the indent and
    /// the divider zone leaves each half's gutter and marker unbudgeted,
    /// so the row overruns and ratatui clips the new side's text.
    #[test]
    fn a_split_row_fits_the_pane() {
        for lines in [9usize, 120, 5000] {
            let file = multi_line_file("a.rs", lines);
            let gutter = gutter_width_for(&file);
            let both = PairedDiffRow {
                left: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }),
                right: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 }),
            };
            // A blank half pads itself by hand, so it can miss the budget
            // a filled one hits.
            let left_only = PairedDiffRow { right: None, ..both };
            let right_only = PairedDiffRow { left: None, ..both };
            // With no cache the text column is pure padding, so the
            // pad arithmetic is only ever exercised against an empty
            // one. Cutting is covered separately - these lines are far
            // too short to reach it.
            let cache = build_file_highlight(&file);
            for pair in [both, left_only, right_only] {
                for pane_width in [101u16, 119, 160, 184] {
                    let row = split_diff_row(&file, pair, gutter, pane_width, Some(&cache));
                    assert_eq!(
                        first_painted_column_past(&row, pane_width),
                        None,
                        "the row paints nothing past the pane: \
                         gutter={gutter} pane_width={pane_width} pair={pair:?}"
                    );
                }
            }
        }
    }

    /// A cluster straddling the cut is measured per-char by the walk but
    /// string-wide by the fit check above it, so the kept prefix can be
    /// wider than the column it was cut to fit.
    #[test]
    fn a_truncated_half_does_not_overrun_its_column() {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        // Plain text, so the line stays one span and the cut is the only
        // thing under test. Every unit is 3 columns wide.
        let text = "x\u{2764}\u{fe0f}".repeat(60);
        let line =
            |kind, old_line, new_line| DiffLine { kind, text: text.clone(), old_line, new_line };
        let file = FileHunks {
            path: "notes.txt".into(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![
                    line(DiffLineKind::Removed, Some(1), None),
                    line(DiffLineKind::Added, None, Some(1)),
                ],
            }],
        };
        let gutter = gutter_width_for(&file);
        let cache = build_file_highlight(&file);
        let pair = PairedDiffRow {
            left: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }),
            right: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 }),
        };
        // 103 is here because the others do not reach this test's own
        // subject: every unit is 3 columns, so the cut only straddles a
        // cluster when the half's text column is 3n+2, and 101/119/160/184
        // give 43/52/72/84 - none of them. 103 gives 44.
        for pane_width in [101u16, 103, 119, 160, 184] {
            let row = split_diff_row(&file, pair, gutter, pane_width, Some(&cache));
            assert_eq!(
                first_painted_column_past(&row, pane_width),
                None,
                "a cut cluster paints nothing past the pane: pane_width={pane_width}"
            );
            // A mid-cluster cut is exactly where a half can come out the
            // wrong painted width, which moves the divider the click
            // handler splits on.
            assert_eq!(
                painted_divider_col(&row, pane_width),
                Some(split_layout(gutter, pane_width).divider_col),
                "a cut cluster leaves the divider on its nominal column: \
                 pane_width={pane_width}"
            );
        }
    }

    /// The first column at or past `pane_width` the row actually paints
    /// into, measured in a buffer with slack so an overrun has somewhere
    /// to land. Summing `Span::width` cannot see one - it is the same
    /// measurement the fit check itself makes, so it agrees with a wrong
    /// answer.
    /// `build_split_half` re-measures after truncating and pads back up
    /// to the column, so a cut that lands short is repaired before the
    /// row is finished and no assertion on that row can see it. Pin the
    /// cut itself.
    #[test]
    fn a_truncated_span_fills_the_column_it_was_cut_to() {
        for max_width in [1usize, 7, 40, 83] {
            let plain = vec![Span::raw("x".repeat(200))];
            let kept: usize =
                truncate_spans_to_width(plain, max_width).iter().map(Span::width).sum();
            assert_eq!(
                kept, max_width,
                "a cut of plain text fills its column: max_width={max_width}"
            );

            // Every unit is `x` plus a 2-column presentation sequence, so
            // a budget of 3n+2 is the only one that straddles a cluster -
            // the case the function's own comment says a per-char sum
            // would get wrong. Asserted exactly, because `<=` cannot see
            // the short cut this test exists to pin.
            let clustered = vec![Span::raw("x\u{2764}\u{fe0f}".repeat(80))];
            let kept: usize =
                truncate_spans_to_width(clustered, max_width).iter().map(Span::width).sum();
            let expected = if max_width % 3 == 2 { max_width - 1 } else { max_width };
            assert_eq!(
                kept, expected,
                "a cut keeps every whole cluster its column allows: max_width={max_width}"
            );
        }
    }

    /// The buffer is pre-filled with a sentinel rather than left empty:
    /// a half pads itself with spaces, so an overrun made of padding is
    /// indistinguishable from untouched background if blankness is the
    /// test. Anything still carrying the sentinel was never painted.
    fn first_painted_column_past(row: &Line<'_>, pane_width: u16) -> Option<usize> {
        use ratatui::buffer::{Buffer, Cell};
        use ratatui::widgets::Widget;

        const SLACK: u16 = 24;
        const SENTINEL: &str = "\u{2588}";

        let area = Rect::new(0, 0, pane_width + SLACK, 1);
        let mut sentinel = Cell::EMPTY;
        sentinel.set_symbol(SENTINEL);
        let mut buffer = Buffer::filled(area, sentinel);
        Paragraph::new(row.clone()).render(area, &mut buffer);
        // Missing the budget has two directions. The overrun is what
        // the callers assert; a row stopping short of the edge leaves
        // the per-line tint not reaching it, and only the buffer shows
        // that either.
        assert_ne!(
            buffer[(pane_width - 1, 0)].symbol(),
            SENTINEL,
            "the row reaches the pane's right edge: pane_width={pane_width}"
        );
        (pane_width..pane_width + SLACK)
            .find(|&x| buffer[(x, 0)].symbol() != SENTINEL)
            .map(usize::from)
    }

    /// The click handler splits old from new on `divider_col`. If the
    /// painted divider ever lands anywhere else, clicks in the gap file
    /// a review comment against the wrong side and persist it.
    #[test]
    fn the_painted_divider_sits_where_split_layout_says() {
        // Line counts either side of a gutter-width step, so a wrong
        // gutter term in the formula shows up rather than cancelling.
        //
        // Both text lengths, because they reach the divider by different
        // routes: `multi_line_file`'s short `line {i}` leaves the filled
        // half padded up to the text column, while a line longer than
        // that column truncates to it and never pads at all.
        for (lines, long_text) in [(9usize, false), (9, true), (120, false), (120, true)] {
            let mut file = multi_line_file("a.rs", lines);
            if long_text {
                file.hunks[0].lines[0].text =
                    "let very_long_identifier_name = compute(other_long_name);".repeat(3);
            }
            let gutter = gutter_width_for(&file);
            let both = PairedDiffRow {
                left: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }),
                right: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 }),
            };
            // An unbalanced row pads its blank half by hand, so it can
            // drift from a filled one.
            let left_only = PairedDiffRow { right: None, ..both };
            let right_only = PairedDiffRow { left: None, ..both };
            // Real highlighted text rather than blank padding: with no
            // cache `cached_line_spans` returns an empty slice. The
            // divider lands on the nominal column either way, so it is
            // the control below that turns the cache load-bearing.
            let cache = build_file_highlight(&file);
            for pair in [both, left_only, right_only] {
                for pane_width in [101u16, 119, 160, 184] {
                    let row = split_diff_row(&file, pair, gutter, pane_width, Some(&cache));
                    assert_eq!(
                        painted_divider_col(&row, pane_width),
                        Some(split_layout(gutter, pane_width).divider_col),
                        "the painted divider sits on split_layout's divider_col: \
                         lines={lines} long_text={long_text} gutter={gutter} \
                         pane_width={pane_width} pair={pair:?}"
                    );
                }
            }
        }
    }

    /// The column the divider glyph occupies once painted. Production
    /// paints these rows through an unwrapped `Paragraph` (`render` at
    /// the top of this file), so the test reads back the same buffer the
    /// click handler's geometry is compared against.
    fn painted_divider_col(row: &Line<'_>, pane_width: u16) -> Option<usize> {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;

        let area = Rect::new(0, 0, pane_width, 1);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(row.clone()).render(area, &mut buffer);
        let hits: Vec<usize> = (0..pane_width)
            .filter(|&x| buffer[(x, 0)].symbol() == "\u{2502}")
            .map(usize::from)
            .collect();
        // Diff text can carry its own box-drawing glyphs. Returning the
        // first would quietly report a text column as the divider.
        assert!(
            hits.len() <= 1,
            "the row paints one divider glyph at pane_width={pane_width}, found columns {hits:?}"
        );
        hits.first().copied()
    }

    /// A control character reaches paint through the highlight cache, and
    /// `Span::styled_graphemes` drops it while `Span::width` still charges
    /// a column, so an unsanitised row under-fills and pulls the divider
    /// left of the column the click handler splits on.
    #[test]
    fn a_control_character_does_not_move_the_painted_divider() {
        use forge_workspace::env::git_diff::hunks::DiffLine;

        // Short bodies pad their half up to the text column; the long one
        // truncates to it and never pads. Both routes charge the dropped
        // column, in different places.
        let bodies = [
            "let x = \u{c}compute(y);".to_owned(),
            "let x = \u{c}\u{c}\u{c}\u{c}\u{c}\u{c}\u{c}\u{c}compute(y);".to_owned(),
            "\u{1b}[0m\u{7}\u{b}\u{d}\u{1}\u{7f}\u{80} every shape".to_owned(),
            "let very_long_identifier\u{c} = compute(other_long_name);".repeat(4),
        ];
        for body in bodies {
            let line = |kind, old_line, new_line| DiffLine {
                kind,
                text: body.clone(),
                old_line,
                new_line,
            };
            let file = FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                oversize: false,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_count: 1,
                    new_start: 1,
                    new_count: 1,
                    lines: vec![
                        line(DiffLineKind::Removed, Some(1), None),
                        line(DiffLineKind::Added, None, Some(1)),
                    ],
                }],
            };
            let gutter = gutter_width_for(&file);
            let cache = build_file_highlight(&file);
            let pair = PairedDiffRow {
                left: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 }),
                right: Some(LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 }),
            };
            for pane_width in [101u16, 119, 160, 184] {
                let row = split_diff_row(&file, pair, gutter, pane_width, Some(&cache));
                assert_eq!(
                    painted_divider_col(&row, pane_width),
                    Some(split_layout(gutter, pane_width).divider_col),
                    "a control character leaves the painted divider on \
                     split_layout's divider_col: pane_width={pane_width} body={body:?}"
                );
            }
        }
    }

    fn one_line_file(path: &str) -> FileHunks {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        FileHunks {
            path: path.into(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Context,
                    text: "x".into(),
                    old_line: Some(1),
                    new_line: Some(1),
                }],
            }],
        }
    }

    fn multi_line_file(path: &str, count: usize) -> FileHunks {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        let n = u32::try_from(count).unwrap_or(u32::MAX);
        let lines = (0..count)
            .map(|i| {
                let no = u32::try_from(i + 1).unwrap_or(u32::MAX);
                DiffLine {
                    kind: DiffLineKind::Context,
                    text: format!("line {i}"),
                    old_line: Some(no),
                    new_line: Some(no),
                }
            })
            .collect();
        FileHunks {
            path: path.into(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![Hunk { old_start: 1, old_count: n, new_start: 1, new_count: n, lines }],
        }
    }

    /// Render the overlay (so the renderer stashes the real border-offset
    /// geometry + the message-block height), left-click the second file's
    /// rail row through `handle_mouse`, then re-render and confirm file 1
    /// actually PINS at the top of the viewport. File 1 is tall enough to
    /// pin; in commit mode the click target must clear the message block
    /// or file 1 lands short and never pins. The rail rows are banner /
    /// rule / blank then file0 / file1, so file 1 sits four rows down.
    fn rail_click_round_trip(commit_mode: bool) {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![one_line_file("a.rs"), multi_line_file("b.rs", 60)],
        );
        state.scanner_ok = true;
        if commit_mode {
            state.commits = vec![forge_workspace::env::git_diff::hunks::CommitMeta {
                sha: "a".into(),
                short_sha: "a3f9c1e".into(),
                subject: "seed".into(),
                body: "why the change\nmatters here".into(),
            }];
            state.scope = crate::app::diff_overlay::DiffScope::Commit(0);
        }
        let mut app = App::test_default();
        app.active_view = crate::app::ActiveView::Diff;
        app.diff_overlay = Some(state);

        let mut terminal = Terminal::new(TestBackend::new(130, 24)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");

        let (rail_top, file1_offset) = {
            let overlay = app.diff_overlay.as_ref().expect("overlay");
            (overlay.rail_origin_row, overlay.doc_offsets().starts[1])
        };
        assert!(file1_offset > 0, "file 0 must occupy rows so the jump to file 1 is observable");

        crate::app::diff_overlay::handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row: rail_top + 4,
                modifiers: KeyModifiers::NONE,
            },
        );
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");

        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").current_file_idx,
            1,
            "a rail click pins file 1 at the top of the viewport (commit_mode={commit_mode})",
        );
    }

    #[test]
    fn rail_click_pins_target_file_plain_mode() {
        rail_click_round_trip(false);
    }

    #[test]
    fn rail_click_pins_target_file_commit_mode() {
        // Commit mode leads with a message block; the click target must
        // clear it (message_rows) so the file pins rather than landing
        // short - the symmetric half of the rail-highlight message-adjust.
        rail_click_round_trip(true);
    }

    // ---- one canonical file order (monotonic current-file arrow) ----

    #[test]
    fn rail_and_body_share_one_file_order() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Scanner order (as `git diff --name-status` might emit) puts a
        // file ahead of a sibling directory's contents and out of alpha.
        // The overlay reorders both the body and the rail into the
        // folded-tree traversal, so the current-file arrow can only step
        // monotonically down the rail as the body scrolls.
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![
                one_line_file("src/zzz.rs"),
                one_line_file("src/app.rs"),
                one_line_file("src/app/foo.rs"),
            ],
        );
        state.scanner_ok = true;
        let paths: Vec<&str> = state.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["src/app/foo.rs", "src/app.rs", "src/zzz.rs"],
            "files reordered into folded-tree traversal",
        );
        let mut app = App::test_default();
        app.active_view = crate::app::ActiveView::Diff;
        app.diff_overlay = Some(state);

        let mut terminal = Terminal::new(TestBackend::new(130, 24)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");

        // The rail's file leaves ascend in `file_idx`, so the rail row
        // order is the exact sequence `file_at_row(doc_scroll)` indexes.
        let overlay = app.diff_overlay.as_ref().expect("overlay");
        let leaves: Vec<usize> = overlay
            .rail_keys
            .iter()
            .filter_map(|k| match k {
                crate::app::diff_overlay::RailRowKey::File { file_idx } => Some(*file_idx),
                _ => None,
            })
            .collect();
        assert_eq!(leaves, vec![0, 1, 2], "rail leaves ascend in body file order");
    }

    #[test]
    fn rail_order_handles_file_dir_name_collision() {
        // A file→dir refactor makes git emit a name as BOTH a file and a
        // directory (`D z` + `A z/a`). The comparator must stay a total
        // order (dir before the same-named file) or `sort_by` mis-orders
        // and the arrow jumble returns.
        let state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![one_line_file("z"), one_line_file("m"), one_line_file("z/a")],
        );
        let paths: Vec<&str> = state.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["z/a", "m", "z"],
            "dir `z/` (from z/a) sorts before file `m`, then the file `z`",
        );
    }

    #[test]
    fn commit_mode_rail_highlight_is_message_adjusted() {
        use crate::app::diff_overlay::DiffScope;
        use forge_workspace::env::git_diff::hunks::CommitMeta;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // A commit's message block leads the scroll, so the rail's
        // current-file highlight must offset by it, not read the raw
        // `doc_scroll`. File 0 is short (so the message block is taller
        // than it); file 1 is taller than the viewport so its header can
        // pin at the very top.
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "main".to_owned(),
            vec![one_line_file("a.rs"), multi_line_file("b.rs", 60)],
        );
        state.scanner_ok = true;
        state.commits = vec![CommitMeta {
            sha: "a".into(),
            short_sha: "a3f9c1e".into(),
            subject: "seed".into(),
            body: "l1\nl2\nl3\nl4".into(),
        }];
        state.scope = DiffScope::Commit(0);
        let mut app = App::test_default();
        app.diff_overlay = Some(state);

        let mut terminal = Terminal::new(TestBackend::new(130, 24)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");

        let (pane_width, start1) = {
            let o = app.diff_overlay.as_ref().expect("overlay");
            (o.pane_width, o.doc_offsets().starts[1])
        };
        let msg_rows = u32::try_from(
            commit_message_block_lines(app.diff_overlay.as_ref().expect("overlay"), pane_width)
                .len(),
        )
        .expect("rows fit u32");
        // The message block must be taller than file 0, else a raw
        // `doc_scroll` read would also land on file 0 and the assertion
        // below wouldn't catch the regression.
        assert!(msg_rows >= start1, "message taller than file 0 (msg={msg_rows}, start1={start1})");

        // Parked at the message boundary: file 0 is pinned, but a raw
        // `file_at_row(doc_scroll)` would wrongly report file 1.
        app.diff_overlay.as_mut().expect("overlay").doc_scroll = msg_rows;
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").current_file_idx,
            0,
            "rail tracks the body's pinned file 0, not the raw doc_scroll",
        );

        // One file deeper pins file 1.
        app.diff_overlay.as_mut().expect("overlay").doc_scroll = msg_rows + start1;
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").current_file_idx,
            1,
            "scrolling one file down advances the rail highlight",
        );
    }

    // ---- banded file header + end-of-file boundary ----

    #[test]
    fn file_header_line_is_banded() {
        let line = file_header_line(&one_line_file("a.rs"), 80, false);
        assert!(
            line.spans.iter().all(|s| s.style.bg == Some(theme::DIFF_FILE_HEADER_BG)),
            "every header span carries the band background",
        );
        let text = line_text(&line);
        assert!(text.contains("a.rs"), "path shown");
        assert!(text.contains("modified"), "status badge word");
    }

    #[test]
    fn end_cap_precedes_each_non_first_file() {
        // Two-file body: file 0 closes with a `└─ end a.rs ──` cap + a
        // blank spacer before file 1's banded header; file 1 (last) has
        // no trailing cap - the document just ends.
        let state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![one_line_file("a.rs"), one_line_file("b.rs")],
        );
        let comments = std::collections::HashMap::new();
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        push_file_body(&state, 0, true, 80, &comments, &mut lines, &mut keys);
        let f0_len = lines.len();
        push_file_body(&state, 1, true, 80, &comments, &mut lines, &mut keys);

        let joined: Vec<String> = lines.iter().map(line_text).collect();
        let cap_idx = joined.iter().position(|l| l.contains("end a.rs")).expect("end cap present");
        assert!(joined[cap_idx].contains('\u{2514}'), "cap opens with the └ corner");
        assert_eq!(keys[cap_idx], BodyRowKey::FileEndCap { file_idx: 0 }, "cap keyed to file 0");
        assert!(joined[cap_idx + 1].trim().is_empty(), "blank spacer follows the cap");
        assert_eq!(keys[cap_idx + 1], BodyRowKey::FileEndCap { file_idx: 0 });
        assert_eq!(cap_idx + 2, f0_len, "cap + blank close file 0's block, then file 1 begins");
        assert!(
            !joined.iter().any(|l| l.contains("end b.rs")),
            "the last file emits no trailing cap",
        );
    }

    // ---- context expanders ----

    fn ctx(new_line: u32) -> forge_workspace::env::git_diff::hunks::DiffLine {
        use forge_workspace::env::git_diff::hunks::DiffLine;
        DiffLine {
            kind: DiffLineKind::Context,
            text: format!("line {new_line}"),
            old_line: Some(new_line),
            new_line: Some(new_line),
        }
    }

    #[test]
    fn context_expander_renders_above_first_hunk_and_at_gaps() {
        // Hunk 0 starts at new line 5 (4 hidden above); hunk 1 starts at
        // new line 20, leaving a gap after hunk 0 (which ends at line 6).
        let file = FileHunks {
            path: "a.rs".into(),
            status: FileStatus::Modified,
            oversize: false,
            hunks: vec![
                Hunk {
                    old_start: 5,
                    old_count: 2,
                    new_start: 5,
                    new_count: 2,
                    lines: vec![ctx(5), ctx(6)],
                },
                Hunk {
                    old_start: 20,
                    old_count: 2,
                    new_start: 20,
                    new_count: 2,
                    lines: vec![ctx(20), ctx(21)],
                },
            ],
        };
        let state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp"), "HEAD".to_owned(), vec![file]);
        let comments = std::collections::HashMap::new();
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        push_file_body(&state, 0, true, 80, &comments, &mut lines, &mut keys);
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        let expanders: Vec<usize> = keys
            .iter()
            .enumerate()
            .filter(|(_, k)| matches!(k, BodyRowKey::ContextExpander { file_idx: 0 }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(expanders.len(), 2, "one leading expander + one gap expander");
        // Leading: 4 lines hidden above line 5, drawn with ↑.
        assert!(joined[expanders[0]].contains('\u{2191}'), "leading uses ↑");
        assert!(joined[expanders[0]].contains("4 lines"), "leading hidden count");
        // Gap: 20 - (5 + 2) = 13 lines between the hunks, drawn with ↕.
        assert!(joined[expanders[1]].contains('\u{2195}'), "gap uses ↕");
        assert!(joined[expanders[1]].contains("13 lines"), "gap hidden count");
    }

    #[test]
    fn oversize_file_shows_note_and_no_expanders() {
        // Two hunks with a gap that would normally draw an expander, but
        // the file is flagged oversize: no expander renders and a "too
        // large" note appears instead.
        let file = FileHunks {
            path: "big.txt".into(),
            status: FileStatus::Modified,
            hunks: vec![
                Hunk {
                    old_start: 5,
                    old_count: 1,
                    new_start: 5,
                    new_count: 1,
                    lines: vec![ctx(5)],
                },
                Hunk {
                    old_start: 20,
                    old_count: 1,
                    new_start: 20,
                    new_count: 1,
                    lines: vec![ctx(20)],
                },
            ],
            oversize: true,
        };
        let state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp"), "HEAD".to_owned(), vec![file]);
        let comments = std::collections::HashMap::new();
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        push_file_body(&state, 0, true, 80, &comments, &mut lines, &mut keys);
        assert!(
            !keys.iter().any(|k| matches!(k, BodyRowKey::ContextExpander { .. })),
            "an oversize file renders no expanders",
        );
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        assert!(joined.iter().any(|l| l.contains("too large")), "the too-large note is shown");
    }

    #[test]
    fn no_context_expander_when_nothing_is_hidden() {
        // A single hunk starting at line 1 with no following hunk: nothing
        // is hidden above or between, so no expander renders (the
        // fully-expanded end state).
        let state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![one_line_file("a.rs")],
        );
        let comments = std::collections::HashMap::new();
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        push_file_body(&state, 0, true, 80, &comments, &mut lines, &mut keys);
        assert!(
            !keys.iter().any(|k| matches!(k, BodyRowKey::ContextExpander { .. })),
            "no expander when the hunk covers the file edge and there's no gap",
        );
    }

    // ---- commit-message block ----

    fn commit_state_with_body(subject: &str, body: &str) -> DiffOverlayState {
        use crate::app::diff_overlay::DiffScope;
        use forge_workspace::env::git_diff::hunks::CommitMeta;
        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "main".to_owned(),
            vec![FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                hunks: vec![],
                oversize: false,
            }],
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
            vec![FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                hunks: vec![],
                oversize: false,
            }],
        );
        assert!(commit_message_block_lines(&state, 80).is_empty());
    }

    #[test]
    fn commit_mode_renders_message_block_above_the_diff() {
        use crate::app::diff_overlay::DiffScope;
        use crate::app::diff_overlay::types::CachedScan;
        use forge_workspace::env::git_diff::hunks::{CommitMeta, DiffLine, Hunk};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let file = FileHunks {
            path: "rate_limit.rs".into(),
            status: FileStatus::Modified,
            oversize: false,
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

    /// Stepper header spans are metadata: a control character must paint
    /// what it charges, so the render pictures it rather than stripping.
    #[test]
    fn stepper_header_spans_picture_control_chars_so_they_paint_what_they_charge() {
        use crate::app::diff_overlay::DiffScope;
        use crate::app::diff_overlay::types::CachedScan;
        use forge_workspace::env::git_diff::hunks::{CommitMeta, DiffLine, Hunk};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let file = FileHunks {
            path: "rate_limit.rs".into(),
            status: FileStatus::Modified,
            oversize: false,
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
            "HEA\u{7}D".to_owned(),
            vec![file.clone()],
        );
        state.branch = Some("feat\u{7}ure".to_owned());
        state.commits = vec![CommitMeta {
            sha: "a".into(),
            short_sha: "a3f9\u{7}c1e".into(),
            subject: "sub\u{7}ject".into(),
            body: String::new(),
        }];
        state.scope = DiffScope::Commit(0);
        state.commit_cache = vec![Some(CachedScan { files: vec![file], scanner_ok: true })];
        let mut app = App::test_default();
        app.diff_overlay = Some(state);

        let backend = TestBackend::new(130, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let full: String =
            terminal.backend().buffer().content.iter().map(ratatui::buffer::Cell::symbol).collect();
        assert!(full.contains("feat\u{2407}ure"), "branch pictured in stepper title: {full:?}");
        assert!(full.contains("HEA\u{2407}D"), "target pictured in stepper title: {full:?}");
        assert!(full.contains("a3f9\u{2407}c1e"), "short sha pictured in stepper: {full:?}");
        assert!(full.contains("sub\u{2407}ject"), "subject pictured in stepper: {full:?}");
    }

    /// Jump-dropdown rows budget and pad from the raw `short_sha` and
    /// `subject`: a control character must picture there, or the row
    /// paints short and the border pulls left of the box edge.
    #[test]
    fn jump_dropdown_row_pictures_control_chars_and_keeps_the_box_edge() {
        use crate::app::diff_overlay::DiffScope;
        use crate::app::diff_overlay::types::CachedScan;
        use forge_workspace::env::git_diff::hunks::{CommitMeta, DiffLine, Hunk};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        const ROW_LEN: usize = 130;

        let file = FileHunks {
            path: "rate_limit.rs".into(),
            status: FileStatus::Modified,
            oversize: false,
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
            "HEAD".to_owned(),
            vec![file.clone()],
        );
        state.commits = vec![CommitMeta {
            sha: "a".into(),
            short_sha: "a3f9\u{7}c1e".into(),
            subject: "sub\u{7}ject".into(),
            body: String::new(),
        }];
        state.scope = DiffScope::Commit(0);
        state.commit_cache = vec![Some(CachedScan { files: vec![file], scanner_ok: true })];
        state.jump_open = true;
        let mut app = App::test_default();
        app.diff_overlay = Some(state);

        let mut terminal = Terminal::new(TestBackend::new(130, 20)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let symbols: Vec<&str> =
            terminal.backend().buffer().content.iter().map(ratatui::buffer::Cell::symbol).collect();
        let row_text = |row: usize| symbols[row * ROW_LEN..(row + 1) * ROW_LEN].concat();

        // Scope to the dropdown box: the stepper paints the same sha and
        // subject pictured, so whole-buffer assertions cannot tell the
        // two apart. `┌─` picks the box border, not the page header; the
        // corner is found in cells (byte offsets are multi-byte here).
        let top_row = (0..20)
            .find(|&r| row_text(r).contains("\u{250c}\u{2500}"))
            .expect("dropdown box top border");
        let top_text = row_text(top_row);
        let edge_col = top_text
            .char_indices()
            .find(|(_, c)| *c == '\u{2510}')
            .map(|(i, _)| top_text[..i].chars().count())
            .expect("box corner on the border row");
        let sha_row = (top_row + 1..=top_row + 4)
            .map(|r| (r, row_text(r)))
            .find(|(_, text)| text.contains("a3f9"))
            .map(|(r, _)| r)
            .expect("commit row inside the dropdown box");
        let row = row_text(sha_row);
        assert!(row.contains("a3f9\u{2407}c1e"), "sha pictured in the dropdown row: {row:?}");
        assert!(row.contains("sub\u{2407}ject"), "subject pictured in the dropdown row: {row:?}");

        // Measured == painted: a charged-but-dropped column under-fills
        // the row and pulls the border left of the box edge.
        assert_eq!(
            symbols[sha_row * ROW_LEN + edge_col],
            "\u{2502}",
            "dropdown row fills to the box edge: {row:?}"
        );
    }

    /// The commit-message block wraps free-form commit text, the most
    /// plausible control-char carrier in this file: subject and body must
    /// picture so the width chop budgets what actually paints.
    #[test]
    fn commit_message_block_pictures_control_chars_in_subject_and_body() {
        use crate::ui::wrap::{display_width, line_display_width};

        let state = commit_state_with_body("fix \u{7}bell", "see \u{b}logs");
        let lines = commit_message_block_lines(&state, 80);
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("fix \u{2407}bell"), "subject pictured: {text:?}");
        assert!(text.contains("see \u{240b}logs"), "body pictured: {text:?}");
        assert_eq!(
            line_display_width(&lines[1]),
            2 + display_width("fix \u{2407}bell"),
            "the subject row measures what the pictured text paints"
        );
    }

    /// The scanner-failure line interpolates `overlay.target` raw; it
    /// must picture, or the dropped column quotes a target that reads as
    /// a different string than the one tracing uses.
    #[test]
    fn scanner_failure_line_pictures_control_chars_in_the_target() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp"),
            "HEA\u{7}D".to_owned(),
            vec![FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                hunks: vec![],
                oversize: false,
            }],
        );
        state.scanner_ok = false;
        let mut app = App::test_default();
        app.diff_overlay = Some(state);

        let mut terminal = Terminal::new(TestBackend::new(130, 20)).expect("terminal");
        terminal.draw(|frame| render(frame, &mut app)).expect("draw");
        let full: String =
            terminal.backend().buffer().content.iter().map(ratatui::buffer::Cell::symbol).collect();
        assert!(
            full.contains("Scan failed for `HEA\u{2407}D`"),
            "target pictured in the failure line: {full:?}"
        );
    }

    fn chip_comment(line: u32, text: &str, status: ReviewStatus) -> HunkComment {
        let thread = forge_primitives::ReviewThread {
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
                review_id: None,
            }],
            status,
            created_at: String::new(),
            updated_at: String::new(),
            commit: None,
        };
        HunkComment {
            key: LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 },
            path: "a.rs".to_owned(),
            line,
            comment_text: text.to_owned(),
            commit: None,
            thread,
            authored_this_session: false,
            anchor_note: None,
            persisted: true,
        }
    }

    fn render_chip(comment: &HunkComment) -> (Vec<Line<'static>>, Vec<BodyRowKey>) {
        render_chip_with_reviews(comment, &[])
    }

    fn render_chip_with_reviews(
        comment: &HunkComment,
        reviews: &[ReviewSet],
    ) -> (Vec<Line<'static>>, Vec<BodyRowKey>) {
        render_chip_collapsed(comment, reviews, false)
    }

    fn render_chip_collapsed(
        comment: &HunkComment,
        reviews: &[ReviewSet],
        collapsed: bool,
    ) -> (Vec<Line<'static>>, Vec<BodyRowKey>) {
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        render_comment_chip(
            comment,
            CommentRef { line: key, slot: 0 },
            4,
            80,
            reviews,
            collapsed,
            &mut lines,
            &mut keys,
        );
        (lines, keys)
    }

    /// A comment whose thread carries the given turns (author label, text)
    /// in order, so tests can exercise the multi-turn conversation render.
    fn chip_comment_with_turns(
        line: u32,
        turns: &[(ReviewAuthor, &str)],
        status: ReviewStatus,
    ) -> HunkComment {
        let mut comment = chip_comment(line, turns.first().map_or("", |(_, t)| t), status);
        comment.thread.comments = turns
            .iter()
            .map(|(author, text)| forge_primitives::ReviewComment {
                author: author.clone(),
                text: (*text).to_owned(),
                at: String::new(),
                review_id: None,
            })
            .collect();
        comment
    }

    /// True when some span in `line` carries `fg`.
    fn line_has_fg(line: &Line, fg: Color) -> bool {
        line.spans.iter().any(|s| s.style.fg == Some(fg))
    }

    #[test]
    fn comment_card_header_carries_line_and_state_colour() {
        let (lines, _) = render_chip(&chip_comment(7, "needs a bound check", ReviewStatus::Open));
        let header = lines.first().expect("header");
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("\u{1f4ac} line 7"), "header names the line; got:\n{joined}");
        assert!(joined.contains("OPEN"), "header carries the state label");
        assert!(line_text(header).starts_with("  ") && line_text(header).contains('\u{256d}'));
        assert!(
            line_has_fg(header, theme::RUST_ORANGE),
            "the open state label is rust-orange in the header",
        );
    }

    #[test]
    fn comment_card_shows_review_tag() {
        let reviews = vec![ReviewSet {
            id: "r2".to_owned(),
            number: 2,
            summary: None,
            created_at: String::new(),
        }];
        let mut filed = chip_comment(41, "() on empty input?", ReviewStatus::Open);
        filed.thread.comments[0].review_id = Some("r2".to_owned());
        let (lines, _) = render_chip_with_reviews(&filed, &reviews);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("\u{b7} R2"),
            "a filed card shows its review number; got:\n{joined}"
        );

        let unfiled = chip_comment(41, "() on empty input?", ReviewStatus::Open);
        let (lines, _) = render_chip_with_reviews(&unfiled, &reviews);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("\u{b7} unfiled"), "an unfiled card reads unfiled; got:\n{joined}");
    }

    #[test]
    fn comment_card_outdated_is_yellow_with_a_note() {
        let (lines, _) =
            render_chip(&chip_comment(72, "guard the None case", ReviewStatus::Outdated));
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("OUTDATED"), "outdated state label");
        assert!(joined.contains("line changed"), "outdated note names the drift");
        assert!(
            line_has_fg(lines.first().expect("header"), theme::STATUS_WARNING),
            "outdated state label is yellow",
        );
    }

    #[test]
    fn comment_card_renders_a_multi_turn_conversation() {
        let comment = chip_comment_with_turns(
            41,
            &[
                (ReviewAuthor::User, "() on empty input - intended?"),
                (ReviewAuthor::Agent { label: "implementer".to_owned() }, "returns Err now."),
            ],
            ReviewStatus::Addressed,
        );
        let (lines, _) = render_chip(&comment);
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("you"), "the reviewer's turn is labelled 'you'");
        assert!(joined.contains("implementer"), "the worker's turn carries its label");
        assert!(joined.contains("() on empty input"), "the reviewer's text renders");
        assert!(joined.contains("returns Err now."), "the worker's reply renders");
        assert!(joined.contains('\u{25cf}'), "each turn hangs off a dot on the rail");
        // Turns render in chronological order - the reviewer's row precedes
        // the worker's reply (a reversed-turn impl would fail here).
        let row = |needle: &str| lines.iter().position(|l| line_text(l).contains(needle));
        assert!(
            row("() on empty input") < row("returns Err now."),
            "the reviewer's turn renders above the worker's reply",
        );
        // Voices are colour-coded: amber for you, blue for the worker.
        assert!(
            lines.iter().any(|l| line_has_fg(l, theme::RUST_ORANGE)),
            "a turn carries the you (amber) voice",
        );
        assert!(
            lines.iter().any(|l| line_has_fg(l, theme::REVIEW_ADDRESSED)),
            "a turn carries the worker (blue) voice",
        );
    }

    #[test]
    fn your_turns_are_editable_agent_turns_are_read_only() {
        let comment = chip_comment_with_turns(
            41,
            &[
                (ReviewAuthor::User, "first"),
                (ReviewAuthor::Agent { label: "implementer".to_owned() }, "reply"),
                (ReviewAuthor::User, "second"),
            ],
            ReviewStatus::Addressed,
        );
        let (lines, keys) = render_chip(&comment);
        // Each of your turns carries a CommentTurn edit key with its index;
        // the agent's turn carries none.
        assert!(keys.iter().any(|k| matches!(k, BodyRowKey::CommentTurn { turn_idx: 0, .. })));
        assert!(keys.iter().any(|k| matches!(k, BodyRowKey::CommentTurn { turn_idx: 2, .. })));
        assert!(
            !keys.iter().any(|k| matches!(k, BodyRowKey::CommentTurn { turn_idx: 1, .. })),
            "the agent turn is not an edit target",
        );
        // Clicking turn 2's text row resolves to turn_idx 2.
        let row = lines.iter().position(|l| line_text(l).contains("second")).expect("turn 2 row");
        assert!(
            matches!(keys[row], BodyRowKey::CommentTurn { turn_idx: 2, .. }),
            "the row rendering turn 2 targets turn 2",
        );
        // A dim ✎ marks each of your turns (two here); the agent gets none.
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert_eq!(
            joined.matches('\u{270e}').count(),
            2,
            "one ✎ per your-turn, none for the agent"
        );
    }

    #[test]
    fn card_shows_a_reply_affordance() {
        let (lines, keys) = render_chip(&chip_comment(9, "take a look", ReviewStatus::Open));
        assert!(
            keys.iter().any(|k| matches!(k, BodyRowKey::CommentReply { .. })),
            "the card carries a reply hit-region",
        );
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("reply"), "the card hints at reply; got:\n{joined}");
    }

    #[test]
    fn comment_card_addressed_offers_both_resolve_and_reopen() {
        let (lines, keys) = render_chip(&chip_comment(50, "look here", ReviewStatus::Addressed));
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("ADDRESSED"), "the addressed state label shows");
        assert!(
            lines.iter().any(|l| line_has_fg(l, theme::REVIEW_ADDRESSED)),
            "the addressed state label is blue",
        );
        assert!(joined.contains("\u{2713} Resolve") && joined.contains("\u{21ba} Reopen"));
        // An addressed thread can be resolved OR reopened, so both spans exist.
        assert!(
            keys.iter().any(|k| matches!(
                k,
                BodyRowKey::CommentButton { resolve: Some(_), reopen: Some(_), .. }
            )),
            "addressed offers both Resolve and Reopen",
        );
    }

    #[test]
    fn comment_card_resolved_offers_reopen_only() {
        let (_, keys) = render_chip(&chip_comment(88, "rename tok", ReviewStatus::Resolved));
        assert!(
            keys.iter().any(|k| matches!(
                k,
                BodyRowKey::CommentButton { resolve: None, reopen: Some(_), .. }
            )),
            "a resolved thread offers Reopen but not Resolve",
        );
    }

    #[test]
    fn comment_card_open_offers_resolve_only() {
        for status in [ReviewStatus::Open, ReviewStatus::Outdated] {
            let (_, keys) = render_chip(&chip_comment(1, "note", status));
            assert!(
                keys.iter().any(|k| matches!(
                    k,
                    BodyRowKey::CommentButton { resolve: Some(_), reopen: None, .. }
                )),
                "{status:?} offers Resolve but not Reopen",
            );
        }
    }

    #[test]
    fn comment_card_shape_is_consistent_across_states() {
        // Every durable state renders the same card: a rounded top border
        // (╭), a button row, and a rounded bottom border (╰).
        for status in [
            ReviewStatus::Open,
            ReviewStatus::Addressed,
            ReviewStatus::Resolved,
            ReviewStatus::Outdated,
        ] {
            let (lines, keys) = render_chip(&chip_comment(1, "note", status));
            assert!(line_text(&lines[0]).contains('\u{256d}'), "{status:?} opens with ╭");
            assert!(
                line_text(lines.last().expect("bottom")).contains('\u{2570}'),
                "{status:?} closes with ╰",
            );
            assert!(
                keys.iter().any(|k| matches!(k, BodyRowKey::CommentButton { .. })),
                "{status:?} carries a button row",
            );
        }
    }

    /// The visible text of a rendered card, rows joined by newlines.
    fn chip_text(comment: &HunkComment) -> String {
        let (lines, _) = render_chip(comment);
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_relocated_card_says_where_it_came_from() {
        // Silent relocation is how a comment ends up attached to code it
        // was never about while still looking anchored.
        let mut comment = chip_comment(58, "guard the None case", ReviewStatus::Open);
        comment.anchor_note = Some(AnchorNote::Moved { from: 41 });
        assert!(
            chip_text(&comment).contains("moved from line 41"),
            "a move names the line it left; got: {}",
            chip_text(&comment),
        );
    }

    #[test]
    fn an_ambiguous_card_says_it_refused_to_guess() {
        let mut comment = chip_comment(41, "guard the None case", ReviewStatus::Outdated);
        comment.anchor_note = Some(AnchorNote::Outdated(OutdatedReason::Ambiguous { matches: 2 }));
        let text = chip_text(&comment);
        assert!(
            text.contains("matched 2 locations"),
            "an ambiguous anchor reports the count; got: {text}",
        );
    }

    #[test]
    fn a_vanished_card_says_the_code_is_gone() {
        let mut comment = chip_comment(41, "guard the None case", ReviewStatus::Outdated);
        comment.anchor_note = Some(AnchorNote::Outdated(OutdatedReason::Gone));
        let text = chip_text(&comment);
        assert!(
            text.contains("the code this was on is gone"),
            "a vanished anchor says so plainly; got: {text}",
        );
    }

    #[test]
    fn an_undisturbed_card_says_nothing_about_anchoring() {
        let comment = chip_comment(41, "guard the None case", ReviewStatus::Open);
        let text = chip_text(&comment);
        assert!(!text.contains("moved from"), "the normal case is quiet");
        assert!(!text.contains("matched"), "the normal case is quiet");
        assert!(!text.contains("is gone"), "the normal case is quiet");
    }

    #[test]
    fn a_resolved_card_collapses_to_one_row() {
        // Twelve resolved comments beside two live ones should not read as
        // fourteen things wanting attention.
        let comment = chip_comment(88, "rename tok to token", ReviewStatus::Resolved);
        let (lines, keys) = render_chip_collapsed(&comment, &[], true);
        assert_eq!(lines.len(), 1, "a resolved comment collapses to a marker");
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("line 88"), "the marker still names its line; got: {text}");
        assert!(text.contains("resolved"), "and says why it is collapsed; got: {text}");
        assert!(
            keys.iter().any(|k| matches!(k, BodyRowKey::CommentCollapsed { .. })),
            "the marker is clickable, or a resolved thread could never be reopened",
        );
    }

    #[test]
    fn an_expanded_resolved_card_can_still_be_reopened() {
        let comment = chip_comment(88, "rename tok to token", ReviewStatus::Resolved);
        let (lines, keys) = render_chip_collapsed(&comment, &[], false);
        assert!(lines.len() > 1, "expanding shows the whole thread again");
        assert!(
            keys.iter().any(|k| matches!(k, BodyRowKey::CommentButton { reopen: Some(_), .. })),
            "Reopen lives on the card, so expanding has to bring it back",
        );
    }

    #[test]
    fn the_stepper_counts_comments_not_the_cards_they_draw_as() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // One thread shows in its commit and in the whole diff, so the
        // running total counted it once per scope the reviewer had
        // visited.
        let mut state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp/repo"), "main".to_owned(), vec![]);
        state.branch = Some("feat/x".to_owned());
        state.commits = vec![forge_workspace::env::git_diff::hunks::CommitMeta {
            sha: "aaa".into(),
            short_sha: "aaa".into(),
            subject: "first".into(),
            body: String::new(),
        }];
        state.scope = DiffScope::Commit(0);
        // Interleaved, as hydrate leaves them: dedup alone would not
        // collapse these, so the sort is load-bearing.
        for (id, scope) in [
            ("one", None),
            ("two", None),
            ("one", Some("aaa".to_owned())),
            ("two", Some("aaa".to_owned())),
        ] {
            let mut c = chip_comment(5, "a comment", ReviewStatus::Open);
            c.thread.id = id.to_owned();
            c.commit = scope;
            state.comments.push(c);
        }

        let width = 100u16;
        let backend = TestBackend::new(width, STEPPER_HEIGHT);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_stepper(
                    frame,
                    ratatui::layout::Rect { x: 0, y: 0, width, height: STEPPER_HEIGHT },
                    &mut state,
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(width);
        let rows: String = (0..usize::from(STEPPER_HEIGHT))
            .map(|r| (0..w).map(|x| buffer.content[r * w + x].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rows.contains("2 comments"),
            "two comments drawn twice each are still two; got:\n{rows}",
        );
    }

    #[test]
    fn the_finish_review_list_shows_each_comment_once_and_shows_the_current_text() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        // Two cards for one thread, the out-of-scope one first as hydrate
        // leaves them. The reviewer reads this list before sealing, so it
        // must show one row, carrying the edit they just made.
        let mut state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp/repo"), "main".to_owned(), vec![]);
        state.finish_review = Some(crate::app::diff_overlay::types::FinishReviewState {
            editor: crate::app::input::InputState::new(),
        });
        for (scope, text) in [(Some("aaa".to_owned()), "FIRSTTEXT"), (None, "SECONDTEXT")] {
            let mut c = chip_comment(5, text, ReviewStatus::Open);
            c.thread.id = "shared".to_owned();
            c.comment_text = text.to_owned();
            c.commit = scope;
            c.authored_this_session = true;
            state.comments.push(c);
        }

        let (width, height) = (80u16, 20u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_finish_review(
                    frame,
                    ratatui::layout::Rect { x: 0, y: 0, width, height },
                    &mut state,
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(width);
        let rows: String = (0..usize::from(height))
            .map(|r| (0..w).map(|x| buffer.content[r * w + x].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains("1 comment"), "one thread is one comment; got:\n{rows}");
        assert!(rows.contains("SECONDTEXT"), "and it reads as the scope on screen has it");
        assert!(!rows.contains("FIRSTTEXT"), "not as the card the rebuild did not touch");
    }

    #[test]
    fn with_no_card_in_this_scope_the_finish_review_list_takes_the_later_one() {
        // Reachable by commenting on one commit, editing from the whole
        // diff, then stepping to a third scope: neither card matches, and
        // "whichever came first" is what the winner rule exists to stop.
        let mut state =
            DiffOverlayState::new(std::path::PathBuf::from("/tmp/repo"), "main".to_owned(), vec![]);
        state.commits = vec![
            forge_workspace::env::git_diff::hunks::CommitMeta {
                sha: "aaa".into(),
                short_sha: "aaa".into(),
                subject: "first".into(),
                body: String::new(),
            },
            forge_workspace::env::git_diff::hunks::CommitMeta {
                sha: "bbb".into(),
                short_sha: "bbb".into(),
                subject: "second".into(),
                body: String::new(),
            },
        ];
        state.scope = DiffScope::Commit(1);
        state.finish_review = Some(crate::app::diff_overlay::types::FinishReviewState {
            editor: crate::app::input::InputState::new(),
        });
        for (scope, text) in [(Some("aaa".to_owned()), "FIRSTTEXT"), (None, "SECONDTEXT")] {
            let mut c = chip_comment(5, text, ReviewStatus::Open);
            c.thread.id = "shared".to_owned();
            c.comment_text = text.to_owned();
            c.commit = scope;
            c.authored_this_session = true;
            state.comments.push(c);
        }

        let (width, height) = (80u16, 20u16);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                render_finish_review(
                    frame,
                    ratatui::layout::Rect { x: 0, y: 0, width, height },
                    &mut state,
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(width);
        let rows: String = (0..usize::from(height))
            .map(|r| (0..w).map(|x| buffer.content[r * w + x].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains("SECONDTEXT"), "the later card wins; got:\n{rows}");
        assert!(!rows.contains("FIRSTTEXT"), "not the one hydrate left in front");
    }

    #[test]
    fn multiple_comments_on_one_line_all_index() {
        // An outdated thread re-placed onto a line that already carries a
        // comment must not clobber it - both live under the shared key.
        let a = chip_comment(5, "first", ReviewStatus::Open);
        let b = chip_comment(5, "drifted here", ReviewStatus::Outdated);
        let refs = vec![&a, &b];
        let map = index_comments_by_key(&refs);
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        assert_eq!(map.get(&key).map(Vec::len), Some(2), "both comments indexed at the shared key");
    }

    #[test]
    fn footer_omits_resolve_and_reopen_hints() {
        // Resolve / reopen are per-comment box buttons; the footer no
        // longer advertises them as global keys.
        let state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![FileHunks {
                path: "a.rs".into(),
                status: FileStatus::Modified,
                oversize: false,
                hunks: Vec::new(),
            }],
        );
        let text = line_text(&footer_line(&state, DiffViewMode::Unified, 160, false, None));
        assert!(text.contains("comment"), "still hints click-to-comment");
        assert!(!text.contains("resolve"), "no global resolve hint");
        assert!(!text.contains("reopen"), "no global reopen hint");
    }

    #[test]
    fn the_footer_esc_label_follows_the_seal_flag() {
        // Whether a review would seal is decided by the caller, against
        // the store - see the app-side test that the hint and Esc agree.
        // All this surface does is say which of the two it was.
        let state = DiffOverlayState::new(
            std::path::PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            Vec::new(),
        );
        let closing = line_text(&footer_line(&state, DiffViewMode::Unified, 200, false, None));
        assert!(closing.contains("close"), "nothing to seal reads close; got: {closing}");
        assert!(!closing.contains("finish review"), "and must not offer a review");

        let sealing = line_text(&footer_line(&state, DiffViewMode::Unified, 200, true, None));
        assert!(sealing.contains("finish review"), "work to seal reads finish review");
    }
}
