//! Inspector pane (right side, Wide + Medium tiers; full-screen
//! overlay at Narrow tier).
//!
//! Mirror of the left [`crate::ui::projects_pane`] in chrome and
//! tier behaviour. Sections separated by a DIM `─` rule:
//!
//! - `GIT` - always rendered. Shows the focused session's cwd, the
//!   current branch on its own row, an optional `PR #N → closes #M`
//!   row, and up to two stacked diff sub-sections - `uncommitted`
//!   (layer 1, dirty/staged/unstaged tree vs HEAD; suppressed when
//!   clean) and `N commits vs <default>` (layer 2, branch commits
//!   ahead of default; suppressed on default branch, on detached
//!   HEAD, or when default can't be resolved). Each sub-section
//!   carries its own subtitle + `+A -R` totals + box-drawing tree
//!   of the top-N changed files grouped by directory. Single-child
//!   directory chains fold so deep paths render as one row. For a
//!   worker on a topic branch with in-progress edits, both layers
//!   populate so the user sees "all the work this worker has done
//!   versus main" at a glance. Sourced from
//!   `UiSession.git_diff_snapshot`.
//! - `TASKS` - rendered when the active session has at least one
//!   non-completed item. The live `TaskCreate` / `TaskUpdate`
//!   snapshot is the sole surface for the task list; the
//!   chat-stream `Task*` tool-call cards (`TaskCreate`,
//!   `TaskUpdate`, `TaskList`, `TaskGet`) are suppressed. #268.
//! - `MCP SERVERS` - rendered when the session's MCP snapshot has at
//!   least one server. Sourced entirely from the snapshot, so every
//!   configured server renders: connected (● green) with scope + tool
//!   count, pending (◌ blue), failed (✗ red) with its reason, and
//!   sdk/in-process servers that have no process at all. A
//!   subprocess-backed server carries a third line with the backing
//!   command, memory and pid, joined by
//!   `crate::app::mcp_servers::collect_mcp_servers`. The whole
//!   section is a click-through to the `/mcp` view.
//! - `PROCESSES` - rendered when the OS walk finds at least one
//!   non-MCP process alive under claude, or the CLI's registry carries
//!   a backgrounded `local_bash` the scan missed. Wire-tracked `Bash`
//!   calls overlay their description when their `command` substring-
//!   matches a process cmdline; the renderer chooses glyphs + colours
//!   per `ProcessKind` (rows built by
//!   `crate::app::processes::collect_active_processes`, which skips
//!   the pids the MCP SERVERS join claims).
//!
//! Reads from per-session state on `UiSession.todos` and
//! `UiSession.git_diff_snapshot`.
//!
//! TASKS item rendering:
//! - `✓` green glyph + DIM crossed-out text for `Completed`
//! - `▸` RUST_ORANGE glyph + white bold text for `InProgress`
//!   (wraps onto continuation lines indented under the glyph;
//!   uses `active_form` when present, else `content`)
//! - `○` DIM glyph + gray text for `Pending` (truncates with `...`)

use forge_primitives::git::{GitBranch, GitIssueRef, GitPrInfo};
use forge_primitives::git_diff::{
    GitBranchAhead, GitDiffFile, GitDiffSnapshot, GitDiffStats, LayerState, RepoGate,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::agent::model::ToolCallStatus;
use crate::app::App;
use crate::app::AttentionEntry;
use crate::app::AttentionKind;
use crate::app::PaneHitTarget;
use crate::app::TodoStatus;
use crate::app::processes::{
    ProcessCollection, ProcessKind, ProcessRow, collect_active_processes, format_memory_short,
};

/// Horizontal padding inside the pane (matches the left
/// `projects_pane`'s 1-col indent).
const PANE_PAD: u16 = 1;

/// Minimum gap (cols) between the path column and the stats column
/// in a per-file diff row. Reserves visual breathing room so the
/// truncated path never butts up against the `+N -M` numbers even at
/// the worst-case width.
const PATH_STATS_GAP: usize = 2;

/// Render the Inspector pane into `area` (inline at Wide/Medium).
///
/// Layout: the top 2 lines (banner + rule) stay pinned; everything
/// below scrolls based on the active session's `inspector_scroll_offset`.
/// A vertical scrollbar renders on the right edge when the body
/// overflows the visible area.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    subagents: &[crate::app::SubagentEntry],
) {
    let (banner_area, rest_area) = split_banner_body(area);

    // Pinned banner: `INSPECTOR` in RUST_ORANGE bold + dim rule.
    let banner_lines = build_inline_banner(area.width);
    frame.render_widget(Paragraph::new(banner_lines), banner_area);

    // Pinned NEEDS ATTENTION band (when any background session is waiting),
    // then the scrollable body in the space below it.
    let body_area = render_attention_band(frame, rest_area, app);
    render_scrollable_body(frame, body_area, app, subagents);
}

/// Render the Narrow-tier full-screen Inspector overlay into `area`.
/// Shares the body builder with the inline path, wrapped in an
/// overlay-specific banner with an `INSPECTOR ▦` label on the left
/// and a `✕` glyph on the right (stamped as
/// [`PaneHitTarget::OverlayClose`] for the click handler). The
/// banner + rule stay pinned; the body scrolls underneath them.
pub fn render_overlay(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    subagents: &[crate::app::SubagentEntry],
) {
    app.pane_hit_targets.clear();

    let (banner_area, body_area) = split_banner_body(area);

    // Banner row: `INSPECTOR ▦ ... ✕` spanning the full overlay width.
    let banner_label = "INSPECTOR \u{25a6}";
    let close_glyph = "\u{2715}";
    let banner_chars = banner_label.chars().count();
    let close_chars = close_glyph.chars().count();
    let pad = usize::from(area.width).saturating_sub(banner_chars).saturating_sub(close_chars);
    let banner_line = Line::from(vec![
        Span::styled(
            banner_label.to_owned(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(close_glyph.to_owned(), Style::default().fg(theme::DIM)),
    ]);
    let rule_line = Line::from(Span::styled(
        "\u{2500}".repeat(usize::from(area.width)),
        Style::default().fg(theme::DIM),
    ));
    frame.render_widget(Paragraph::new(vec![banner_line, rule_line]), banner_area);

    // Stamp ✕ hit-target - last char on the banner row.
    let close_x_start =
        area.x.saturating_add(area.width).saturating_sub(u16::try_from(close_chars).unwrap_or(1));
    let close_x_end = area.x.saturating_add(area.width);
    app.pane_hit_targets.push(PaneHitTarget::OverlayClose {
        y: area.y,
        height: 1,
        x_start: close_x_start,
        x_end: close_x_end,
    });

    let body_area = render_attention_band(frame, body_area, app);
    render_scrollable_body(frame, body_area, app, subagents);
}

/// Split the inspector area into the pinned banner (top 2 lines:
/// banner + rule) and the scrollable body underneath. Both are
/// clamped to fit when the supplied area is shorter than 2 rows.
fn split_banner_body(area: Rect) -> (Rect, Rect) {
    let banner_height = area.height.min(2);
    let banner_area = Rect { x: area.x, y: area.y, width: area.width, height: banner_height };
    let body_area = Rect {
        x: area.x,
        y: area.y.saturating_add(banner_height),
        width: area.width,
        height: area.height.saturating_sub(banner_height),
    };
    (banner_area, body_area)
}

/// Build the inline-pane's banner: ` INSPECTOR` heading + dim rule
/// under it. Two lines total, mirroring the projects pane's banner.
fn build_inline_banner(width: u16) -> Vec<Line<'static>> {
    let rule_width = usize::from(width.saturating_sub(2));
    vec![
        Line::from(Span::styled(
            " INSPECTOR".to_owned(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::raw(" "),
            Span::styled("\u{2500}".repeat(rule_width), Style::default().fg(theme::DIM)),
        ]),
    ]
}

/// Max session rows the pinned band renders before collapsing the
/// tail into a `+N more` line. Bounds the band's height regardless of
/// how many sessions are waiting so a burst can't crowd out GIT.
/// Matches the TASKS section's per-section cap.
const ATTENTION_MAX_ROWS: usize = 5;

/// Rows the band leaves for the scrollable body when the pane has the
/// height to spare, so the band never consumes the whole pane. On a
/// pane too short to honour this the band clips (and its clipped rows
/// are skipped when stamping) rather than hide the body entirely.
const ATTENTION_MIN_BODY_ROWS: u16 = 3;

/// Render the pinned NEEDS ATTENTION attention band into the top of
/// `area` when any background session is waiting, returning the body
/// rect below it (fed to the scrollable body). When nothing waits the
/// band is absent and `area` is returned unchanged - GIT then sits
/// directly under the banner rule, as before. The band never scrolls;
/// it stays fixed while the sections below it move.
fn render_attention_band(frame: &mut Frame, area: Rect, app: &mut App) -> Rect {
    if area.height == 0 || area.width == 0 {
        return area;
    }
    let entries = app.needs_attention_sessions();
    if entries.is_empty() {
        return area;
    }
    let total = entries.len();
    // Fixed row cap bounds the band regardless of waiter count; the
    // tail collapses to a `+N more` line.
    let shown = total.min(ATTENTION_MAX_ROWS);
    let overflow = total - shown;

    let now = std::time::SystemTime::now();
    let lines = build_attention_lines(&entries[..shown], total, overflow, area.width, now);
    // Clamp the band so the scrollable body keeps at least
    // ATTENTION_MIN_BODY_ROWS when the pane is tall enough; on a very
    // short pane the band shows what it can (at least the header) and
    // the body takes the rest.
    let natural = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let band_height =
        natural.min(area.height.saturating_sub(ATTENTION_MIN_BODY_ROWS)).min(area.height).max(1);
    let band_area = Rect { x: area.x, y: area.y, width: area.width, height: band_height };
    frame.render_widget(Paragraph::new(lines), band_area);

    // Stamp a click-to-jump target per visible row. The header is at
    // band_area.y (row 0), a blank spacer at row 1; session row i sits
    // at band_area.y + 2 + i. Rows clipped by a short pane are skipped
    // so a click can't resolve to an off-screen row.
    let band_bottom = band_area.y.saturating_add(band_height);
    let x_end = area.x.saturating_add(area.width);
    for (i, entry) in entries[..shown].iter().enumerate() {
        let offset = u16::try_from(i.saturating_add(2)).unwrap_or(u16::MAX);
        let row_y = band_area.y.saturating_add(offset);
        if row_y >= band_bottom {
            break;
        }
        app.pane_hit_targets.push(PaneHitTarget::InspectorAttentionRow {
            session_key: entry.session_key.clone(),
            y: row_y,
            height: 1,
            x_start: area.x,
            x_end,
        });
    }

    Rect {
        x: area.x,
        y: band_bottom,
        width: area.width,
        height: area.height.saturating_sub(band_height),
    }
}

/// Build the pinned NEEDS ATTENTION band's lines: the DIM-bold ` NEEDS ATTENTION`
/// header (with the full `total` waiter count), a blank spacer, one row
/// per `shown` session (already sorted stalest-first), an optional dim
/// `+N more` line when `overflow > 0`, then a blank + DIM `─` rule
/// bracketing the band off from the scrolling body.
fn build_attention_lines(
    shown: &[AttentionEntry],
    total: usize,
    overflow: usize,
    width: u16,
    now: std::time::SystemTime,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(shown.len() + 4);
    lines.push(attention_header_line(width, total));
    // Blank between header and first row, matching the TASKS / SUBAGENTS
    // section rhythm.
    lines.push(Line::default());
    for entry in shown {
        lines.push(attention_row_line(width, entry, now));
    }
    if overflow > 0 {
        lines.push(attention_overflow_line(overflow));
    }
    // Blank before the closing rule that separates the band from GIT.
    lines.push(Line::default());
    push_section_rule(&mut lines, width);
    lines
}

/// The band's `+N more` overflow line: dim italic, matching the
/// TASKS / GIT-layer overflow rows.
fn attention_overflow_line(overflow: usize) -> Line<'static> {
    Line::from(vec![
        Span::raw(" ".repeat(usize::from(PANE_PAD))),
        Span::styled(
            format!("+{overflow} more"),
            Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
        ),
    ])
}

/// Attention-band header: DIM-bold ` NEEDS ATTENTION` with a
/// right-justified DIM count of waiting sessions - styled exactly like
/// the other section headers (`GIT` / `TASKS` / `SUBAGENTS`). The
/// per-session `△` / `✕` lives on the rows, not the header.
fn attention_header_line(width: u16, count: usize) -> Line<'static> {
    const LABEL: &str = " NEEDS ATTENTION";
    let count_str = count.to_string();
    let chrome = LABEL.chars().count() + count_str.chars().count() + usize::from(PANE_PAD); // right gutter
    let pad = usize::from(width).saturating_sub(chrome).max(1);
    Line::from(vec![
        Span::styled(
            LABEL.to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(count_str, Style::default().fg(theme::DIM)),
    ])
}

/// One attention-band row: yellow `△` (pending prompt), red `✕` (dead
/// turn) or `💬` in the addressed accent (unread worker answers) +
/// white-bold project name + DIM ` (role)` (workers only) + the DIM
/// kind/tool/wait-age detail right-justified. The name truncates to
/// fit; the detail is capped so a long tool name can't crowd the name
/// out, and the row honours the 1-col right gutter every inspector
/// section observes.
fn attention_row_line(
    width: u16,
    entry: &AttentionEntry,
    now: std::time::SystemTime,
) -> Line<'static> {
    let inner = usize::from(width);
    let role_suffix = entry.role.as_deref().map(|role| format!(" ({role})")).unwrap_or_default();
    let role_width = role_suffix.chars().count();
    // `glyph_chrome` is the glyph plus its trailing space; `💬` is two
    // cells wide where `△` / `✕` are one.
    let (glyph, glyph_color, glyph_chrome) = match entry.kind {
        AttentionKind::Failed { .. } => ("\u{2715}", theme::STATUS_ERROR, 2),
        AttentionKind::ReviewReplies { .. } => ("\u{1F4AC}", theme::REVIEW_ADDRESSED, 3),
        AttentionKind::Permission { .. } | AttentionKind::Question => {
            ("\u{25b3}", theme::STATUS_WARNING, 2)
        }
    };
    // Cap the detail so a long MCP tool name can't leave the name with
    // zero room: reserve both gutters + glyph/space + role + 2 cols.
    let detail_cap =
        inner.saturating_sub(2 * usize::from(PANE_PAD) + glyph_chrome + role_width + 2).max(1);
    let detail = truncate_or_pass(&attention_detail(entry, now), detail_cap);
    let detail_width = detail.chars().count();

    let name_chrome = usize::from(PANE_PAD)
        + glyph_chrome
        + role_width
        + 1 // min gap before the detail
        + detail_width
        + usize::from(PANE_PAD); // right gutter
    let name_budget = row_text_budget(inner, name_chrome);
    let fitted_name = truncate_or_pass(&entry.name, name_budget);

    let used = 2 * usize::from(PANE_PAD)
        + glyph_chrome
        + fitted_name.chars().count()
        + role_width
        + detail_width;
    let pad = inner.saturating_sub(used).max(1);
    Line::from(vec![
        Span::raw(" ".repeat(usize::from(PANE_PAD))),
        Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
        Span::raw(" "),
        Span::styled(fitted_name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(role_suffix, Style::default().fg(theme::DIM)),
        Span::raw(" ".repeat(pad)),
        Span::styled(detail, Style::default().fg(theme::DIM)),
    ])
}

/// The DIM detail cluster for an attention row: the kind, the tool
/// name (permission prompts only) or the failure classification, and
/// the wait-age, joined by ` · ` (the inspector's separator). E.g.
/// `permission · Bash · 3m`, `question · 20s`, or
/// `failed · server_error HTTP 529 · 3m`.
fn attention_detail(entry: &AttentionEntry, now: std::time::SystemTime) -> String {
    let wait = fmt_countdown(now.duration_since(entry.enqueued_at).unwrap_or_default());
    match &entry.kind {
        AttentionKind::Permission { tool } if !tool.is_empty() => {
            format!("permission \u{00B7} {tool} \u{00B7} {wait}")
        }
        AttentionKind::Permission { .. } => format!("permission \u{00B7} {wait}"),
        AttentionKind::Question => format!("question \u{00B7} {wait}"),
        AttentionKind::Failed { error, status } => {
            let label = crate::app::events::api_retry::error_label(*error);
            let status = status.map_or_else(String::new, |code| format!(" HTTP {code}"));
            format!("failed \u{00B7} {label}{status} \u{00B7} {wait}")
        }
        AttentionKind::ReviewReplies { count } => {
            format!("review replies \u{00B7} {count} \u{00B7} {wait}")
        }
    }
}

/// Render the inspector body (`GIT` → `TASKS` → `PROCESSES` ...)
/// into `body_area` with the active session's scroll offset
/// applied. Clamps the offset to `[0, max]` (writing the clamped
/// value back so the wheel handler doesn't desync after the body
/// shrinks), stamps `body_area` onto `App.rendered_inspector_body_area`
/// for the mouse-wheel hit test, and overlays a vertical scrollbar
/// on the right edge whenever the body overflows.
fn render_scrollable_body(
    frame: &mut Frame,
    body_area: Rect,
    app: &mut App,
    subagents: &[crate::app::SubagentEntry],
) {
    app.rendered_inspector_body_area = body_area;
    if body_area.height == 0 || body_area.width == 0 {
        return;
    }

    let mut body_lines: Vec<Line<'static>> = Vec::new();
    let mcp_section_range = {
        let _t = crate::perf::start("ui::inspector_pane::append_body");
        append_body(&mut body_lines, app, body_area.width, subagents)
    };
    let has_open_diff_glyph = snapshot_has_diff(app);
    let total = body_lines.len();
    let visible = usize::from(body_area.height);
    let max_offset = total.saturating_sub(visible);
    let max_offset_u16 = u16::try_from(max_offset).unwrap_or(u16::MAX);

    // Read + clamp + write back the per-session scroll offset.
    let offset = if let Some(session) = app.try_active_bucket_mut() {
        let clamped = session.inspector_scroll_offset.min(max_offset_u16);
        session.inspector_scroll_offset = clamped;
        clamped
    } else {
        0
    };

    frame.render_widget(Paragraph::new(body_lines).scroll((offset, 0)), body_area);

    // Stamp the 🦉 open-review hit target - GIT header is body
    // line 0, so the glyph is visible exactly when `offset == 0`.
    // The 🦉 owl is 2 cells wide and sits with PANE_PAD (1 cell) of
    // trailing pad before the right edge (matches `append_git_section`'s
    // layout). Hit-test covers both glyph cells + 1 cell left/right
    // for forgiveness.
    if has_open_diff_glyph && offset == 0 {
        let right_edge = body_area.x.saturating_add(body_area.width);
        let glyph_x = right_edge.saturating_sub(3);
        let x_start = glyph_x.saturating_sub(1);
        let x_end = glyph_x.saturating_add(3);
        app.pane_hit_targets.push(PaneHitTarget::InspectorGitOpenDiff {
            y: body_area.y,
            height: 1,
            x_start,
            x_end,
        });
    }

    // Stamp the MCP SERVERS click-through: the whole section's
    // on-screen rect opens the /mcp view. The section scrolls with the
    // body, so the band clips to the visible rows (fully scrolled off
    // in either direction stamps nothing).
    if let Some((start, len)) = mcp_section_range {
        let off = usize::from(offset);
        let vis_top = start.saturating_sub(off);
        let vis_bottom =
            start.saturating_add(len).saturating_sub(off).min(usize::from(body_area.height));
        if vis_bottom > vis_top {
            app.pane_hit_targets.push(PaneHitTarget::InspectorMcpOpenStatus {
                y: body_area.y.saturating_add(u16::try_from(vis_top).unwrap_or(u16::MAX)),
                height: u16::try_from(vis_bottom - vis_top).unwrap_or(u16::MAX),
                x_start: body_area.x,
                x_end: body_area.x.saturating_add(body_area.width),
            });
        }
    }

    render_inspector_thumb(frame, body_area, total, visible, offset);
}

/// Paint the inspector body's scroll thumb. Mirrors
/// `ui::chat::render_scrollbar_overlay`: thumb-only (no rail), uses
/// `▐` (U+2590) cells styled `ROLE_ASSISTANT`, geometry via the
/// shared [`crate::app::compute_scrollbar_geometry`].
///
/// One deliberate difference from chat: the inspector body is much
/// shorter than the chat scrollback (tens of rows vs. hundreds),
/// so the `viewport² / content` formula produces a thumb that takes
/// up half the rail or more. Clamp to [`INSPECTOR_THUMB_MAX_CELLS`]
/// so the indicator stays visually subtle regardless of how short
/// the content is. The clamp moves the thumb's effective track
/// length up by `(thumb_size − clamped) / total_track` so the thumb
/// still rides the full vertical range when scrolling.
///
/// No-op when the body fits inside the visible area.
fn render_inspector_thumb(
    frame: &mut Frame,
    body_area: Rect,
    total: usize,
    visible: usize,
    offset: u16,
) {
    let Some(geometry) = crate::app::compute_scrollbar_geometry(total, visible, f32::from(offset))
    else {
        return;
    };
    let thumb_size = geometry.thumb_size.min(INSPECTOR_THUMB_MAX_CELLS);
    let area_h = usize::from(body_area.height);
    // Recompute thumb_top against the post-clamp track length so the
    // thumb still slides across the full visible range.
    let track = area_h.saturating_sub(thumb_size);
    let max_offset = total.saturating_sub(visible);
    let thumb_top = if max_offset == 0 || track == 0 {
        0
    } else {
        // Inspector content fits well inside f32's mantissa (50-row
        // sanity cap on PROCESSES) so the precision lints can be
        // suppressed here without risking overflow.
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let pos = (f32::from(offset) / max_offset as f32 * track as f32).round() as usize;
        pos
    };
    let thumb_top = thumb_top.min(area_h.saturating_sub(1));
    let thumb_end = thumb_top.saturating_add(thumb_size).min(area_h);
    let thumb_style = Style::default().fg(theme::ROLE_ASSISTANT);
    let rail_x = body_area.right().saturating_sub(1);
    let symbol = "\u{2590}"; // ▐ right half block
    let buf = frame.buffer_mut();
    for row in thumb_top..thumb_end {
        let y = body_area.y.saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if let Some(cell) = buf.cell_mut((rail_x, y)) {
            cell.set_symbol(symbol);
            cell.set_style(thumb_style);
        }
    }
}

/// Visual cap for the inspector scrollbar thumb. Inspector content
/// is tens of rows so `viewport² / content` gives a thumb that
/// dominates the rail. Hardcoding to a single cell matches the
/// "tiny dot" the chat scrollbar shows when chat content is long,
/// giving the two surfaces a consistent visual weight.
const INSPECTOR_THUMB_MAX_CELLS: usize = 1;

/// Append the body (GIT section + verification nudge + TASKS
/// section) to `lines`. Shared between the inline render and the
/// Narrow overlay render. GIT and TASKS are separated by a DIM
/// `─` rule mirroring the projects pane's project-list /
/// account-panel boundary, so the two surfaces read as visually
/// distinct rather than two `DIM bold` headers next to each other.
///
/// Returns the MCP SERVERS section's line range within `lines`, for
/// the click-through hit band (the whole section opens the `/mcp`
/// view).
fn append_body(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    width: u16,
    subagents: &[crate::app::SubagentEntry],
) -> Option<(usize, usize)> {
    {
        let _t = crate::perf::start("ui::inspector_pane::git_section");
        append_git_section(lines, app, width);
    }

    let todos = app.todos();
    // Section visibility gates on PENDING/IN-PROGRESS tasks
    // (completed are hidden by the renderer anyway).
    let has_live_tasks = todos.iter().any(|t| t.status != TodoStatus::Completed);
    if has_live_tasks {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        let _t = crate::perf::start("ui::inspector_pane::tasks_section");
        append_tasks_section(lines, app, width);
    }

    // WORKFLOWS section sits between TASKS and
    // MONITORS. Auto-clears once every workflow has reached
    // `Completed`.
    if !app.workflows().is_empty() {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_workflows_section(lines, app, width);
    }

    // SUBAGENTS sits between WORKFLOWS and SCHEDULES. Mirrors the
    // WORKFLOWS all-terminal-drain trigger: the slice empties once every
    // visible Task/Agent root in the session has reached a terminal
    // status, so the entire section disappears.
    if !subagents.is_empty() {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_subagents_section(lines, app, width, subagents);
    }

    // SCHEDULES sits between SUBAGENTS and PROCESSES. Pending
    // wakeups + crons; auto-clears entries on the ~1s prune tick
    // (passed wakeups, 7-day-expired recurring crons) and on
    // explicit `CronDelete`. The MONITORS section is gone; Monitor
    // tool calls now render their live tail directly in chat (see
    // `ui::message::render_lifecycle_one_liner`'s `"Monitor"` arm).
    if !app.schedules().is_empty() || !app.forge_schedule_rows.is_empty() {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_schedules_section(lines, app, width);
    }

    // GOTIFY sits between SCHEDULES and PROCESSES. Rendered only while the
    // stream is connected and the active project has a subscription; shows
    // the connection status + the project's inbound subscriptions.
    if gotify_section_visible(app) {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_gotify_section(lines, app, width);
    }

    // MCP SERVERS sits above PROCESSES and is sourced entirely from
    // the session's MCP snapshot, so every configured server renders -
    // sdk/in-process servers with no process, pending and failed ones
    // included. Whole section is a click-through to the /mcp view.
    let mcp_section = crate::app::mcp_servers::collect_mcp_servers(app);
    let mcp_range = if mcp_section.rows.is_empty() {
        None
    } else {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        let start = lines.len();
        append_mcp_servers_section(lines, &mcp_section.rows, width);
        Some((start, lines.len() - start))
    };

    // PROCESSES is the single activity lens below MCP SERVERS:
    // the OS process tree plus the CLI's authoritative backgrounded
    // `local_bash` registry (agents render in SUBAGENTS, workflows in
    // WORKFLOWS; MCP servers render in MCP SERVERS above). The join ran
    // once above; its claimed pids hand off here. Auto-hidden when
    // nothing is active.
    let processes = collect_active_processes(app, &mcp_section.claimed_pids);
    if !processes.is_empty() {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_processes_section(lines, &processes, width, app.active_spinner_glyph());
    }

    mcp_range
}

/// Width threshold above which the PROCESSES section appends `· 12 MB`
/// to each row's metadata. Wide tier (inspector at 40 cols) gets memory;
/// Medium tier (inspector at 30 cols) drops it so the metadata fits.
const PROCESSES_MEMORY_WIDTH_THRESHOLD: u16 = 36;

/// Append the MCP SERVERS section: header (with the `▦` open-view
/// affordance at the right edge, matching the GIT header's `🦉`) + one
/// tree per server. Each tree is the name line carrying the status
/// glyph, the DIM detail line under it, and - for a subprocess-backed
/// server - the process line with memory + pid, exactly as the old
/// PROCESSES tree rendered a matched server's backing process.
///
/// Rows pack tight (no blanks between servers) so the tree reads as
/// one connected block.
fn append_mcp_servers_section(
    lines: &mut Vec<Line<'static>>,
    rows: &[crate::app::mcp_servers::McpServerRow],
    width: u16,
) {
    const LABEL: &str = " MCP SERVERS";
    const AFFORDANCE: &str = "\u{25A6}"; // ▦ - the whole section opens /mcp
    let mut header_spans = vec![Span::styled(
        LABEL.to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )];
    let label_chars = LABEL.chars().count();
    let pad = usize::from(width)
        .saturating_sub(label_chars + AFFORDANCE.chars().count() + usize::from(PANE_PAD));
    header_spans.push(Span::raw(" ".repeat(pad)));
    header_spans.push(Span::styled(AFFORDANCE.to_owned(), Style::default().fg(theme::DIM)));
    header_spans.push(Span::raw(" ".repeat(usize::from(PANE_PAD))));
    lines.push(Line::from(header_spans));
    // Blank between header and content, matching every other section.
    lines.push(Line::default());

    let include_memory = width >= PROCESSES_MEMORY_WIDTH_THRESHOLD;
    let server_count = rows.len();
    for (idx, row) in rows.iter().enumerate() {
        let is_last = idx + 1 == server_count;
        // Continuation column under the name connector: `│   ` while
        // more servers follow, blank once the trunk closed.
        let trunk = if is_last { "    " } else { "\u{2502}   " };
        let connector = if is_last { "\u{2514}\u{2500} " } else { "\u{251C}\u{2500} " };
        let (glyph, glyph_color) = mcp_status_glyph(row.status);
        let name_chrome = usize::from(PANE_PAD)
            + 3 // connector
            + 1 // space before the glyph
            + glyph.chars().count()
            + usize::from(PANE_PAD); // right gutter
        let name = truncate_or_pass(&row.name, row_text_budget(usize::from(width), name_chrome));
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(usize::from(PANE_PAD))),
            Span::styled(connector.to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(name, Style::default().fg(theme::DIM)),
            Span::raw(" "),
            Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
        ]));

        let child_chrome = usize::from(PANE_PAD) + 4 + 3 + usize::from(PANE_PAD); // pad + trunk + connector + gutter
        let budget = row_text_budget(usize::from(width), child_chrome);
        // Detail line: ├─ when the process line follows it, └─ when the
        // detail is the tree's last child.
        let detail_connector =
            if row.process.is_some() { "\u{251C}\u{2500} " } else { "\u{2514}\u{2500} " };
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(usize::from(PANE_PAD))),
            Span::styled(trunk.to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(detail_connector.to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(truncate_or_pass(&row.detail, budget), Style::default().fg(theme::DIM)),
        ]));

        // Process line - subprocess-backed servers only. Memory + pid
        // ride the same threshold as PROCESSES so both tiers read
        // alike; the pid earns its place so a long-lived server can be
        // found with `ps`.
        if let Some(process) = row.process.as_ref() {
            let suffix = if include_memory {
                format!(
                    " \u{00B7} {} \u{00B7} {}",
                    format_memory_short(process.memory_bytes),
                    process.pid
                )
            } else {
                String::new()
            };
            let text_budget = budget.saturating_sub(suffix.chars().count());
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(usize::from(PANE_PAD))),
                Span::styled(trunk.to_owned(), Style::default().fg(theme::DIM)),
                Span::styled("\u{2514}\u{2500} ".to_owned(), Style::default().fg(theme::DIM)),
                Span::styled(
                    truncate_or_pass(&process.command, text_budget),
                    Style::default().fg(theme::DIM),
                ),
                Span::styled(suffix, Style::default().fg(theme::DIM)),
            ]));
        }
    }
}

/// Status glyph + colour for an MCP server row: `●` connected (green),
/// `◌` pending (blue), `✗` failed (red). NeedsAuth / Disabled never
/// appear in the --print snapshot today; they take the failed glyph
/// and name themselves on the detail line.
fn mcp_status_glyph(status: forge_primitives::McpServerConnectionStatus) -> (&'static str, Color) {
    use forge_primitives::McpServerConnectionStatus as Status;
    match status {
        Status::Connected => ("\u{25CF}", theme::REVIEW_RESOLVED),
        Status::Pending => ("\u{25CC}", theme::REVIEW_ADDRESSED),
        Status::Failed | Status::NeedsAuth | Status::Disabled => ("\u{2717}", theme::STATUS_ERROR),
    }
}

/// Append a dim `─` horizontal rule across `width − 2` cols with a
/// 1-col leading space, matching the banner-rule + projects-pane
/// section-separator shape so the inspector body reads consistently
/// with the rest of the chrome.
fn push_section_rule(lines: &mut Vec<Line<'static>>, width: u16) {
    let rule_width = usize::from(width.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("\u{2500}".repeat(rule_width), Style::default().fg(theme::DIM)),
    ]));
}

/// Append the GIT section to `lines`. Hidden entirely when the
/// active session's cwd is not inside a git repository
/// (`repo_gate == RepoGate::NotARepo`). For real repos the section
/// renders header + path + branch + diff + file tree as usual; for
/// scanner failures inside a real repo the unhealthy banner still
/// surfaces so the operator gets a triage signal.
fn append_git_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    // Suppress the whole section when the focused session's cwd
    // isn't inside a git repo (and the scanner ran cleanly so this
    // isn't a "scanner crashed" failsafe). Without this the user
    // sees an empty `GIT` header + path in every non-git project.
    // Pre-scan (`snapshot.is_none()`) keep rendering the header so
    // the row animates in once the scanner answers.
    if let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref())
        && matches!(snapshot.repo_gate, RepoGate::NotARepo)
    {
        return;
    }

    // Section header - DIM bold, flush against the rule above
    // (mirrors `TASKS`). When the snapshot has at least one layer
    // of diff to surface, append the `🦉` glyph at the right edge
    // as the open-diff affordance, with any `💬 N` waiting-reply
    // badge immediately left of it.
    let has_glyph = snapshot_has_diff(app);
    let badge = review_replies_badge(app).map(|count| format!("\u{1F4AC} {count}"));
    let mut header_spans = vec![Span::styled(
        " GIT".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )];
    if has_glyph || badge.is_some() {
        // " GIT" is 4 cells; the 🦉 owl and the badge's 💬 are 2 cells
        // each; trailing pad is PANE_PAD (1 cell) so the owl's right
        // edge aligns with the `-M` column on the diff-stats rows below.
        let trailing_pad = usize::from(PANE_PAD);
        let badge_width = badge.as_ref().map_or(0, |text| text.chars().count() + 1);
        let owl_width = if has_glyph { 2 } else { 0 };
        let gap = usize::from(badge_width > 0 && owl_width > 0);
        let pad =
            usize::from(width).saturating_sub(4 + badge_width + gap + owl_width + trailing_pad);
        header_spans.push(Span::raw(" ".repeat(pad)));
        if let Some(text) = badge {
            header_spans.push(Span::styled(text, Style::default().fg(theme::REVIEW_ADDRESSED)));
            header_spans.push(Span::raw(" ".repeat(gap)));
        }
        if has_glyph {
            header_spans.push(Span::styled("\u{1F989}".to_owned(), Style::default()));
        }
        header_spans.push(Span::raw(" ".repeat(trailing_pad)));
    }
    lines.push(Line::from(header_spans));
    // Blank between header and content.
    lines.push(Line::default());

    // Path row - always rendered. Head-truncated so the leaf
    // (project name) is preserved when the path overflows.
    let path_budget = usize::from(width).saturating_sub(usize::from(PANE_PAD));
    let path_value = fit_path_head_truncated(app.cwd(), path_budget);
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(path_value, Style::default().fg(theme::DIM)),
    ]));

    let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()) else {
        // Pre-first-scan window: only the path is known.
        return;
    };
    if matches!(snapshot.repo_gate, RepoGate::ScannerFailed) {
        // Scanner crashed (rev-parse Failed / Oversize) and the
        // snapshot is in the ScannerFailed gate. Without
        // this row the section renders identically to a real non-
        // repo directory - the user has no visual cue that git
        // itself is unhealthy. The `🦉` glyph still routes through
        // the ScannerFailed path so a click surfaces the trace
        // target.
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "git scanner unhealthy, see logs (target: agent.env_git)",
                Style::default().fg(theme::STATUS_WARNING),
            ),
        ]));
        return;
    }

    // Branch row - just the branch glyph + name. Diff totals live
    // on the layer subtitle rows below.
    if let Some((label, color)) = branch_row_for(snapshot) {
        lines.push(branch_line(width, &label, color));
    }

    // PR row - sits below the branch row when the scanner resolved
    // a PR. Renders BEFORE the layered diff sub-sections because PR
    // info is a property of the branch as a whole, not of either
    // diff layer.
    if let Some(pr) = snapshot.pr.as_ref() {
        lines.push(pr_line(width, pr, &snapshot.closes));
    }

    // Layer 1 sub-section - uncommitted edits vs HEAD. Three legal
    // states encoded as `LayerState`: Clean (skip), Populated (render),
    // ScanFailed (surface a "(scan failed)" stub so the user sees the
    // failure instead of a clean-tree render).
    match &snapshot.worktree {
        LayerState::Clean => {}
        LayerState::Populated(stats) => {
            append_diff_layer(lines, &uncommitted_display(stats), width);
        }
        LayerState::ScanFailed => {
            lines.push(diff_layer_failed_line(width, "uncommitted"));
        }
    }

    // Layer 2 sub-section - branch commits ahead of default.
    // Skipped on the default branch, on detached HEAD, or when
    // default isn't resolved. When BOTH layers are present, layer 2
    // renders directly below layer 1 so the user sees in-progress +
    // committed-but-unmerged work stacked in the same view. The
    // explicit `Some(default)` pair makes the assumption
    // load-bearing at the call site rather than hidden behind an
    // unwrap. The ScanFailed branch mirrors layer 1's surfacing.
    //
    // Tripwire for the F10 invariant: a future change that
    // constructs `branch_ahead = Populated(_)` with `default_branch
    // = None` would silently fall through to the no-op branch. The
    // assert catches it in debug builds; release builds drop to the
    // silent fallback, same shape as today.
    debug_assert!(
        snapshot.default_branch.is_some() || !snapshot.branch_ahead.is_populated(),
        "branch_ahead invariant: default_branch must be Some when branch_ahead is Populated",
    );
    match (&snapshot.branch_ahead, snapshot.default_branch.as_deref()) {
        (LayerState::Populated(ahead), Some(default)) => {
            append_diff_layer(lines, &branch_ahead_display(ahead, default), width);
        }
        (LayerState::ScanFailed, _) => {
            lines.push(diff_layer_failed_line(width, "vs default"));
        }
        _ => {}
    }
}

/// Render a single-line "(scan failed)" subtitle for a diff layer
/// whose per-layer numstat hit a subprocess failure. Same indent
/// chrome as [`diff_subtitle_line`] so the failure row sits where
/// the normal subtitle would.
fn diff_layer_failed_line(width: u16, label: &str) -> Line<'static> {
    let warn_text = "(scan failed)";
    let indent_chrome = usize::from(PANE_PAD) + 3;
    let label_chars = label.chars().count();
    let warn_chars = warn_text.chars().count();
    let pad = usize::from(width)
        .saturating_sub(indent_chrome)
        .saturating_sub(label_chars)
        .saturating_sub(1)
        .saturating_sub(warn_chars)
        .saturating_sub(usize::from(PANE_PAD));
    Line::from(vec![
        Span::raw("    "),
        Span::styled(label.to_owned(), Style::default().fg(theme::DIM)),
        Span::raw(" "),
        Span::raw(" ".repeat(pad)),
        Span::styled(warn_text.to_owned(), Style::default().fg(theme::STATUS_WARNING)),
        Span::raw(" "),
    ])
}

/// Worker answers on the active session's reviews still owed a
/// reviewer turn, for the `💬 N` header badge. Suppressed once the
/// header describes a different branch than the count was recorded
/// against - `/diff` would open on that one instead. Two bucket field
/// reads, no store query: this runs every frame.
fn review_replies_badge(app: &App) -> Option<usize> {
    let session = app.active_session()?;
    let waiting = session.review_replies_waiting.as_ref()?;
    match &session.git_diff_snapshot.as_ref()?.branch {
        GitBranch::Named(name) if *name == waiting.branch => Some(waiting.count),
        _ => None,
    }
}

/// Whether the active session's snapshot warrants the `🦉` open-diff
/// glyph in the GIT header. Two cases qualify:
/// - At least one diff layer (`worktree` / `branch_ahead`) is
///   populated - the normal "there's a diff to review" path.
/// - `repo_gate == RepoGate::ScannerFailed` - the Inspector scanner
///   crashed (distinct from a non-repo). The user
///   needs a way to escalate; clicking the glyph routes through
///   `open_default → DefaultTarget::ScannerFailed`, surfacing the
///   trace-target hint they need to triage.
fn snapshot_has_diff(app: &App) -> bool {
    let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()) else {
        return false;
    };
    if matches!(snapshot.repo_gate, RepoGate::ScannerFailed) {
        return true;
    }
    !matches!(snapshot.worktree, LayerState::Clean)
        || !matches!(snapshot.branch_ahead, LayerState::Clean)
}

/// Render one diff layer (subtitle + tree + optional overflow row)
/// into `lines`. Both `worktree` (layer 1) and `branch_ahead` (layer
/// 2) flow through this helper so the visual chrome stays in sync.
fn append_diff_layer(lines: &mut Vec<Line<'static>>, layer: &DiffDisplay<'_>, width: u16) {
    lines.push(diff_subtitle_line(width, &layer.subtitle, layer.totals));

    if layer.files.is_empty() {
        return;
    }

    // Blank between the subtitle row and the file tree.
    lines.push(Line::default());

    let tree = {
        let _t = crate::perf::start("ui::inspector_pane::build_tree");
        build_tree(layer.files)
    };
    {
        let _t = crate::perf::start("ui::inspector_pane::render_tree");
        render_tree(lines, &tree, width);
    }

    // Overflow row when the trimmed top-N is shorter than the total
    // changed-files count.
    if layer.total_files > layer.files.len() {
        let more = layer.total_files - layer.files.len();
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("+{more} more"),
                Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
}

/// Resolve the branch row's `(label, color)` for the snapshot.
/// `NoRepo` / `Unknown` collapse to `None` (no row).
fn branch_row_for(snapshot: &GitDiffSnapshot) -> Option<(String, Color)> {
    match &snapshot.branch {
        GitBranch::Named(name) => {
            // `default_branch` may be a remote-tracking ref (`origin/main`);
            // compare the plain branch name so a checked-out `main` still
            // reads as the default (DIM), not a feature branch.
            let on_default = snapshot
                .default_branch
                .as_deref()
                .map(|d| d.strip_prefix("origin/").unwrap_or(d))
                .is_some_and(|d| d == name.as_str());
            let color = if on_default { theme::DIM } else { theme::RUST_ORANGE };
            Some((name.clone(), color))
        }
        GitBranch::Detached => Some(("HEAD".to_owned(), theme::STATUS_WARNING)),
        GitBranch::NoRepo | GitBranch::Unknown => None,
    }
}

/// Render the branch row: `  ⎇ <label>`. Totals + the worktree /
/// vs-default subtitle land on the next line (see
/// [`diff_subtitle_line`]) so a worktree-dirty branch row and a
/// branch-vs-default branch row look identical in chrome - the
/// disambiguation is on the subtitle row directly below.
fn branch_line(width: u16, label: &str, label_color: Color) -> Line<'static> {
    let glyph_chrome = usize::from(PANE_PAD) + 2; // "  ⎇ "
    let label_budget = usize::from(width).saturating_sub(glyph_chrome);
    let fitted = truncate_with_ellipsis(label, label_budget);
    Line::from(vec![
        Span::styled("  \u{2387} ".to_owned(), Style::default().fg(theme::DIM)),
        Span::styled(fitted, Style::default().fg(label_color)),
    ])
}

/// PR row: `    PR #1234 → closes #1230 #1228`. Indented 4 cols to
/// align with the diff-subtitle row directly above so both read as
/// sub-labels of the branch. The PR number lights up in
/// `RUST_ORANGE` (matches the feature-branch convention - the PR is
/// the headline); everything else stays DIM. The closing-issue list
/// is truncated with `...` when it would overflow the pane width;
/// when even one issue can't fit, the whole `→ closes ...` tail
/// collapses to a single `...` suffix.
fn pr_line(width: u16, pr: &GitPrInfo, closes: &[GitIssueRef]) -> Line<'static> {
    let indent = "    "; // 4 cols, mirrors diff_subtitle_line
    let pr_number = format!("#{}", pr.number);

    // Closes-list disabled when empty - just `PR #N`.
    if closes.is_empty() {
        return Line::from(vec![
            Span::raw(indent.to_owned()),
            Span::styled("PR ".to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(pr_number, Style::default().fg(theme::RUST_ORANGE)),
        ]);
    }

    // Chrome consumed BEFORE the issue numbers: indent (4) +
    // "PR " (3) + "#N" + " → closes " (11). The trailing 2 cols
    // are the pane's right gutter (mirrors how diff_subtitle_line
    // budgets PANE_PAD on the right edge).
    let chrome_chars =
        indent.chars().count() + 3 + pr_number.chars().count() + 11 + usize::from(PANE_PAD);
    let budget = usize::from(width).saturating_sub(chrome_chars);

    // Greedily fit issue numbers, separated by single spaces. If
    // even the first issue doesn't fit, the closes tail collapses
    // to just `...`.
    let mut closes_str = String::new();
    let mut shown = 0usize;
    for issue in closes {
        let chunk_len = if closes_str.is_empty() {
            1 + count_digits(issue.number) // `#<n>`
        } else {
            1 + 1 + count_digits(issue.number) // ` #<n>`
        };
        if closes_str.chars().count().saturating_add(chunk_len) > budget {
            break;
        }
        if !closes_str.is_empty() {
            closes_str.push(' ');
        }
        closes_str.push('#');
        closes_str.push_str(&issue.number.to_string());
        shown = shown.saturating_add(1);
    }

    if shown == 0 {
        // Nothing fit - show `PR #N → ...` so the existence of a
        // closes list still surfaces even when we can't render any
        // of it.
        return Line::from(vec![
            Span::raw(indent.to_owned()),
            Span::styled("PR ".to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(pr_number, Style::default().fg(theme::RUST_ORANGE)),
            Span::styled(" \u{2192} \u{2026}".to_owned(), Style::default().fg(theme::DIM)),
        ]);
    }
    if shown < closes.len() {
        closes_str.push(' ');
        closes_str.push('\u{2026}');
    }

    Line::from(vec![
        Span::raw(indent.to_owned()),
        Span::styled("PR ".to_owned(), Style::default().fg(theme::DIM)),
        Span::styled(pr_number, Style::default().fg(theme::RUST_ORANGE)),
        Span::styled(format!(" \u{2192} closes {closes_str}"), Style::default().fg(theme::DIM)),
    ])
}

/// Decimal-digit count for `n`. Used by [`pr_line`] to budget the
/// closing-issue list against pane width without allocating a
/// temporary `to_string()` per issue. `0` is one digit.
fn count_digits(mut n: u64) -> usize {
    if n == 0 {
        return 1;
    }
    let mut digits = 0usize;
    while n > 0 {
        digits = digits.saturating_add(1);
        n /= 10;
    }
    digits
}

/// One-line display of the diff context: the `worktree` /
/// `vs <default>` label (DIM, indented under the branch name so it
/// reads as a sub-label of the branch row) plus right-justified
/// `+N -M` totals. Indent is `PANE_PAD + glyph + space` = 4 cols
/// so the label starts where the branch name does. Layout mirrors
/// the per-file diff rows so the totals column right-aligns with
/// every other stats column in the section.
fn diff_subtitle_line(width: u16, label: &str, totals: (u32, u32)) -> Line<'static> {
    let (added, removed) = totals;
    // Indent is `"    "` painted below at the start of the line -
    // 4 cells: PANE_PAD (1) + glyph (1) + 2-cell content gap. Keep
    // this in sync with the literal indent on the Span::raw row
    // below; an off-by-one here makes the line render 1 cell too
    // wide, which clips the trailing-pad space and the `-M` ends up
    // touching the right edge of the pane.
    let indent_chrome = usize::from(PANE_PAD) + 3;
    let added_str = format!("+{added}");
    let removed_str = format!("-{removed}");
    let stats_width = added_str.chars().count() + 1 + removed_str.chars().count();
    let label_budget = usize::from(width)
        .saturating_sub(indent_chrome)
        .saturating_sub(PATH_STATS_GAP)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    let fitted = truncate_with_ellipsis(label, label_budget);
    let label_chars = fitted.chars().count();
    let pad = usize::from(width)
        .saturating_sub(indent_chrome)
        .saturating_sub(label_chars)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    Line::from(vec![
        Span::raw("    "),
        Span::styled(fitted, Style::default().fg(theme::DIM)),
        Span::raw(" ".repeat(pad)),
        Span::styled(added_str, Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(removed_str, Style::default().fg(Color::Red)),
        Span::raw(" "),
    ])
}

/// One diff layer's display payload - the rendered subtitle plus
/// the top-N files + aggregate totals. Built by [`uncommitted_display`]
/// (layer 1) and [`branch_ahead_display`] (layer 2) so the renderer
/// can iterate both layers with identical chrome.
struct DiffDisplay<'a> {
    files: &'a [GitDiffFile],
    total_files: usize,
    totals: (u32, u32),
    subtitle: String,
}

/// Layer 1 (uncommitted edits vs HEAD): subtitle is the bare word
/// `uncommitted` so the user reads it as "the dirty tree" without
/// having to map "worktree" onto that mental model.
fn uncommitted_display(stats: &GitDiffStats) -> DiffDisplay<'_> {
    DiffDisplay {
        files: &stats.files,
        total_files: stats.total_files,
        totals: (stats.total_added, stats.total_removed),
        subtitle: "uncommitted".to_owned(),
    }
}

/// Layer 2 (branch ahead of default): subtitle carries the commit
/// count so the user knows how many commits produced the stats
/// (e.g. `3 commits vs main`). Singular form (`1 commit vs main`)
/// when the branch only has one commit ahead. `default` is
/// guaranteed non-empty by the scanner: `branch_ahead` is only
/// constructed when `default_branch` resolved.
fn branch_ahead_display<'a>(ahead: &'a GitBranchAhead, default: &str) -> DiffDisplay<'a> {
    let commit_label = if ahead.commit_count == 1 { "commit" } else { "commits" };
    let subtitle = format!("{} {commit_label} vs {default}", ahead.commit_count);
    DiffDisplay {
        files: &ahead.stats.files,
        total_files: ahead.stats.total_files,
        totals: (ahead.stats.total_added, ahead.stats.total_removed),
        subtitle,
    }
}

/// Tree node built from the diff's file list - a single trie node
/// with either a `file_stats` leaf or zero+ `children` (a
/// directory). After construction we fold any non-root dir whose
/// only child is itself a dir, collapsing chains like
/// `crates/forge-tui/src` into a single labelled row.
struct TreeNode {
    /// Display label for this node. Folded chains are joined with
    /// `/` here (e.g. `forge-agent/src/env`). The implicit root has
    /// an empty label and is never rendered.
    label: String,
    /// `Some` for file leaves (carries `(added, removed)`), `None`
    /// for directories.
    file_stats: Option<(u32, u32)>,
    /// Sorted children: directories first (alpha), then files
    /// (alpha). Empty for file leaves.
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn is_dir(&self) -> bool {
        self.file_stats.is_none()
    }
}

/// Build a tree from the (already top-N trimmed + change-sorted)
/// file list. Resulting tree has the implicit root with each
/// distinct top-level component as a direct child.
fn build_tree(files: &[GitDiffFile]) -> TreeNode {
    let mut root = TreeNode { label: String::new(), file_stats: None, children: Vec::new() };
    for file in files {
        insert_file(&mut root, &file.path, (file.added, file.removed));
    }
    sort_tree(&mut root);
    fold_single_child_dirs(&mut root);
    root
}

fn insert_file(node: &mut TreeNode, path: &str, stats: (u32, u32)) {
    let mut components = path.split('/').filter(|c| !c.is_empty());
    let Some(first) = components.next() else {
        // Empty path - ignore (defensive; the scanner doesn't emit
        // empty paths but the renderer shouldn't panic if it ever
        // does).
        return;
    };
    let rest: Vec<&str> = components.collect();
    if rest.is_empty() {
        // Leaf - attach as a file child. Duplicate file names in the
        // same directory would overwrite here, but git's diff output
        // can't produce that shape (every path is unique).
        node.children.push(TreeNode {
            label: first.to_owned(),
            file_stats: Some(stats),
            children: Vec::new(),
        });
        return;
    }
    // Find or insert the directory child for `first`.
    let dir_idx = node
        .children
        .iter()
        .position(|child| child.is_dir() && child.label == first)
        .unwrap_or_else(|| {
            node.children.push(TreeNode {
                label: first.to_owned(),
                file_stats: None,
                children: Vec::new(),
            });
            node.children.len() - 1
        });
    let remainder = rest.join("/");
    insert_file(&mut node.children[dir_idx], &remainder, stats);
}

/// Sort children: directories first (alpha), then files (alpha).
/// Recurses into directory children.
fn sort_tree(node: &mut TreeNode) {
    node.children.sort_by(|a, b| match (a.is_dir(), b.is_dir()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.label.cmp(&b.label),
    });
    for child in &mut node.children {
        sort_tree(child);
    }
}

/// Fold any directory whose only child is itself a directory by
/// concatenating labels with `/`. Repeatedly applies until no more
/// folding is possible. Recurses depth-first so deep chains
/// (`crates/forge-tui/src`) collapse in one pass.
fn fold_single_child_dirs(node: &mut TreeNode) {
    for child in &mut node.children {
        fold_single_child_dirs(child);
    }
    // Now fold this node's directory children whose only child is a
    // directory. (Files cannot be folded onto their parent - that
    // would lose the file's stats row.)
    for child in &mut node.children {
        while child.is_dir() && child.children.len() == 1 && child.children[0].is_dir() {
            let mut only = child.children.remove(0);
            child.label.push('/');
            child.label.push_str(&only.label);
            child.children = std::mem::take(&mut only.children);
        }
    }
}

/// Render the tree under `root`'s implicit-root children. Each
/// top-level entry renders flush with the pane indent (no connector);
/// deeper entries gain `├─` / `└─` connectors with `│  ` /  `   `
/// continuation prefixes for ancestor levels.
fn render_tree(lines: &mut Vec<Line<'static>>, root: &TreeNode, width: u16) {
    let count = root.children.len();
    for (idx, child) in root.children.iter().enumerate() {
        let is_last = idx + 1 == count;
        render_tree_node(lines, child, "", true, is_last, width);
    }
}

fn render_tree_node(
    lines: &mut Vec<Line<'static>>,
    node: &TreeNode,
    prefix: &str,
    is_top_level: bool,
    is_last: bool,
    width: u16,
) {
    let connector = if is_top_level {
        ""
    } else if is_last {
        "\u{2514}\u{2500} "
    } else {
        "\u{251c}\u{2500} "
    };
    let line_prefix = format!("{prefix}{connector}");
    lines.push(tree_row(width, &line_prefix, &node.label, node.file_stats));

    if node.children.is_empty() {
        return;
    }
    // Compute the prefix continuation for THIS node's children.
    // Top-level entries don't have a visible connector above them so
    // their children start with no continuation prefix; deeper nodes
    // append `│  ` (not-last) or `   ` (last) to mark which ancestor
    // columns still have unfinished siblings below.
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
        render_tree_node(lines, child, &continuation, false, child_is_last, width);
    }
}

/// One tree row: pane indent + tree prefix + label + (for file
/// leaves) right-justified `+N -M` stats. Directory rows render the
/// label DIM with no stats column.
fn tree_row(
    width: u16,
    tree_prefix: &str,
    label: &str,
    file_stats: Option<(u32, u32)>,
) -> Line<'static> {
    let prefix_chars = tree_prefix.chars().count();
    let mut spans =
        vec![Span::raw(" "), Span::styled(tree_prefix.to_owned(), Style::default().fg(theme::DIM))];

    let Some((added, removed)) = file_stats else {
        // Directory row - just the label, no stats. Truncate the
        // label if it overflows the available width (rare in
        // practice since folding keeps single-child chains
        // collapsed).
        let label_budget = usize::from(width)
            .saturating_sub(usize::from(PANE_PAD))
            .saturating_sub(prefix_chars)
            .saturating_sub(usize::from(PANE_PAD));
        let fitted = truncate_with_ellipsis(label, label_budget);
        spans.push(Span::styled(fitted, Style::default().fg(theme::DIM)));
        return Line::from(spans);
    };

    let added_str = format!("+{added}");
    let removed_str = format!("-{removed}");
    let stats_width = added_str.chars().count() + 1 + removed_str.chars().count();
    // Reserve `PATH_STATS_GAP` cols between label and stats so the
    // stats column always has breathing room even when the filename
    // is exactly at budget.
    let label_budget = usize::from(width)
        .saturating_sub(usize::from(PANE_PAD))
        .saturating_sub(prefix_chars)
        .saturating_sub(PATH_STATS_GAP)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    let fitted = truncate_with_ellipsis(label, label_budget);
    let label_chars = fitted.chars().count();
    let pad = usize::from(width)
        .saturating_sub(usize::from(PANE_PAD))
        .saturating_sub(prefix_chars)
        .saturating_sub(label_chars)
        .saturating_sub(stats_width)
        .saturating_sub(usize::from(PANE_PAD));
    spans.push(Span::styled(fitted, Style::default().fg(theme::DIM)));
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(added_str, Style::default().fg(Color::Green)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(removed_str, Style::default().fg(Color::Red)));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// Head-truncate `s` to at most `max_chars` characters with a
/// leading `...` ellipsis. Preserves the tail (so the leaf component
/// of a path / filename stays visible). When `s` contains `/`
/// separators the truncation prefers to drop whole leading
/// components (yielding `.../foo/bar.rs` rather than chopping
/// mid-name); falls back to character-level head-truncation when
/// even the basename is too long. Returns the original string when
/// it already fits; collapses to `...` at `max_chars` ≤ 1.
fn fit_path_head_truncated(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_owned();
    }
    if max_chars <= 1 {
        return "\u{2026}".to_owned();
    }
    // Try component-aware truncation: walk left-to-right over the
    // path components and return the first tail that fits when
    // prefixed with `.../`. Lands at a `/` boundary so the result
    // reads as a clean partial path rather than a chopped string.
    let components: Vec<&str> = s.split('/').collect();
    if components.len() > 1 {
        for start in 1..components.len() {
            let tail = components[start..].join("/");
            let with_prefix = format!("\u{2026}/{tail}");
            if with_prefix.chars().count() <= max_chars {
                return with_prefix;
            }
        }
    }
    // Even the basename overflows - fall back to char-level cut
    // so we at least preserve the trailing characters.
    let keep = max_chars - 1;
    let skip = total - keep;
    let mut out = String::from("\u{2026}");
    out.extend(s.chars().skip(skip));
    out
}

fn append_tasks_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    let todos = app.todos();
    let active_glyph = app.active_spinner_glyph();

    if todos.is_empty() {
        return;
    }

    // Done / total counter for the header - m is completed, n is the
    // full todo list (including hidden completed and visible
    // pending/in-progress). Reads at a glance as a progress meter.
    let total = todos.len();
    let done = todos.iter().filter(|t| t.status == TodoStatus::Completed).count();

    // TASKS section header - DIM bold, 2-col indent (matches the
    // left pane's `ACTIVE` / `INACTIVE` section headers). Trailing
    // ` · m/n` count is DIM, separator is the same `·` we use across
    // the rest of the inspector (GIT subtitle, PROCESSES suffixes).
    lines.push(Line::from(vec![
        Span::styled(
            " TASKS".to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" \u{00B7} {done}/{total}"), Style::default().fg(theme::DIM)),
    ]));
    // Blank between header and first item.
    lines.push(Line::default());

    // chrome accounting routed through the
    // single `row_text_budget` helper so every inspector section
    // honours the same 1-col right gutter (TASKS' convention).
    // Item chrome: 1-col left indent + glyph (1) + space (1) +
    // 1-col right gutter = 4 cols total. Continuation lines for
    // wrapped in-progress items indent under the text column
    // (start col 3 from the pane's x=0) but pay the same chrome
    // budget; that's by design so wrapped rows have the same
    // visual right edge as the headline.
    let glyph_indent = PANE_PAD + 2;
    let chrome_chars = usize::from(glyph_indent) + usize::from(PANE_PAD);
    let text_budget = row_text_budget(usize::from(width), chrome_chars);

    // Visibility tiering: show as much as fits within TASKS_MAX (5).
    //
    // 1. **Everything fits** (total <= cap): show ALL tasks in their
    //    original order, completed included. So a 3-task list with
    //    1 done + 1 in-progress + 1 pending renders all three -
    //    you can see what's behind you AND what's ahead, not just
    //    the current step.
    // 2. **Total exceeds cap but non-completed fits**: hide
    //    completed entirely, show non-completed. The `m/n` count in
    //    the section header still surfaces the done count so they
    //    aren't lost from the eye.
    // 3. **Non-completed itself overflows cap**: truncate at
    //    TASKS_MAX-1 and emit `+N more` for the remainder
    //    (completed counted as hidden too).
    let total_count = todos.len();
    let non_completed: Vec<&_> =
        todos.iter().filter(|t| t.status != TodoStatus::Completed).collect();
    let visible_todos: Vec<&_>;
    let hidden: usize;
    if total_count <= TASKS_MAX {
        // Tier 1 - original order, all included.
        visible_todos = todos.iter().collect();
        hidden = 0;
    } else if non_completed.len() <= TASKS_MAX {
        // Tier 2 - completed silently hidden; m/n header conveys
        // the missing count.
        visible_todos = non_completed;
        hidden = 0;
    } else {
        // Tier 3 - non-completed itself exceeds the cap. Top
        // TASKS_MAX-1 non-completed + `+N more` overflow row.
        let cap = TASKS_MAX.saturating_sub(1);
        visible_todos = non_completed.iter().copied().take(cap).collect();
        hidden = total_count - cap;
    }

    let shown_iter = visible_todos.iter().copied();
    let shown_count = visible_todos.len();
    for (idx, todo) in shown_iter.enumerate() {
        // Glyph language matches PROCESSES + Projects pane:
        // ○ DIM for pending, RUST_ORANGE braille spinner for the
        // currently-running task, ✓ green for completed (hidden in
        // practice - the visible_todos filter strips them).
        let (glyph, glyph_color) = match todo.status {
            TodoStatus::Completed => ("\u{2713}".to_owned(), Color::Green),
            TodoStatus::InProgress => (active_glyph.to_string(), theme::RUST_ORANGE),
            TodoStatus::Pending => ("\u{25cb}".to_owned(), theme::DIM),
        };
        let text_style = match todo.status {
            TodoStatus::Completed => {
                Style::default().fg(theme::DIM).add_modifier(Modifier::CROSSED_OUT)
            }
            TodoStatus::InProgress => {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            }
            TodoStatus::Pending => Style::default().fg(Color::Gray),
        };
        let display_text = if todo.status == TodoStatus::InProgress && !todo.active_form.is_empty()
        {
            todo.active_form.clone()
        } else {
            todo.content.clone()
        };

        if todo.status == TodoStatus::InProgress {
            // Wrap onto continuation lines, indented under the text
            // column so the glyph stays visually associated with the
            // first wrapped row.
            let wrapped = wrap_text(&display_text, text_budget);
            let mut iter = wrapped.into_iter();
            if let Some(first) = iter.next() {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(glyph.clone(), Style::default().fg(glyph_color)),
                    Span::raw(" "),
                    Span::styled(first, text_style),
                ]));
            } else {
                // Empty `display_text` - still render the glyph row
                // so the pane shape stays consistent.
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(glyph.clone(), Style::default().fg(glyph_color)),
                ]));
            }
            for rest in iter {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(usize::from(glyph_indent))),
                    Span::styled(rest, text_style),
                ]));
            }
        } else {
            // Truncate with `...` at the right edge.
            let truncated = truncate_with_ellipsis(&display_text, text_budget);
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(glyph.clone(), Style::default().fg(glyph_color)),
                Span::raw(" "),
                Span::styled(truncated, text_style),
            ]));
        }
        // Blank between tasks for breathing room. Skipped after the
        // last item so we don't leave a trailing blank at the end of
        // the TASKS section.
        if idx + 1 < shown_count || hidden > 0 {
            lines.push(Line::default());
        }
    }

    if hidden > 0 {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("+{hidden} more"),
                Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
}

/// Per-section cap on TASKS rows. Completed tasks are filtered out
/// before counting; beyond `TASKS_MAX - 1` remaining items the tail
/// collapses to a single `+N more` row. Matches the PROCESSES
/// per-parent cap so both surfaces feel consistent.
const TASKS_MAX: usize = 5;

/// Render the Inspector SCHEDULES section: header + one row per pending
/// `ScheduleWakeup` / `CronCreate` (chat-parsed cloud routines) AND per
/// durable forge cron (`mcp__forge__cron`, from the cached
/// `app.forge_crons` snapshot). The section hides entirely when no
/// entries are present. Header line, blank, per-entry rows with blank
/// separators.
fn append_schedules_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    // Two sources share this section: the chat-parsed cloud routines
    // (`ScheduleWakeup` / `CronCreate`, per-session) and the durable
    // forge crons (`mcp__forge__cron`). The forge-cron rows are humanized
    // once per ~1s tick into `app.forge_schedule_rows`, so the render
    // does no timezone syscall or humanize allocation per frame; the live
    // countdown still recomputes from each row's `fire_at` below.
    let now = std::time::SystemTime::now();
    let mut entries: Vec<crate::app::ScheduleEntry> = app.schedules().to_vec();
    entries.extend(app.forge_schedule_rows.iter().cloned());
    if entries.is_empty() {
        return;
    }

    lines.push(Line::from(Span::styled(
        " SCHEDULES".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    let inner_width = usize::from(width);
    let last_idx = entries.len().saturating_sub(1);
    for (idx, entry) in entries.iter().enumerate() {
        append_schedule_row(lines, entry, now, inner_width);
        if idx < last_idx {
            lines.push(Line::default());
        }
    }
}

/// The GOTIFY section shows whenever the active session owns at least
/// one subscription, connected or not - a dropped stream is exactly when
/// the user needs to see that the alerts they asked for have stopped
/// arriving, so hiding it there would be a silent failure. With no owned
/// subscription the section is omitted without consulting the connection
/// at all: there is nothing for this session to receive either way.
fn gotify_section_visible(app: &App) -> bool {
    !app.gotify_subs.is_empty()
}

/// Render the Inspector GOTIFY section: a status-carrying header then
/// the active session's own subscriptions. The snapshot is already
/// scoped by owner in `App::refresh_gotify`, so every row here belongs
/// to this session and none needs an owner label. Only invoked when
/// [`gotify_section_visible`] holds, so the subscription set is never
/// empty; the stream may be up or down.
fn append_gotify_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    lines.push(gotify_header_line(width, app.gotify_connected));
    lines.push(Line::default());

    let inner_width = usize::from(width);
    for sub in &app.gotify_subs {
        append_gotify_subscription(lines, sub, inner_width);
    }
}

/// GOTIFY section header: DIM-bold ` GOTIFY` with the live stream status
/// right-justified beside it, the same header-adornment shape
/// [`attention_header_line`] uses for its waiting count and the GIT
/// header for its `🦉` / `💬`. Unlike the GIT header this emits no
/// trailing pad span, because nothing here is a click target needing the
/// line to be exactly `width` - content simply stops [`PANE_PAD`] short
/// of the edge, observing the same right gutter every other row does.
///
/// Connected renders the Gotify `◈` in RUST_ORANGE; a dropped stream
/// swaps in the `⚠` + STATUS_WARNING pairing `projects_pane`'s
/// `glyph_for_lifecycle` already uses for `AuthRequired` - degraded and
/// worth noticing, distinct from the red `✗` of something broken.
fn gotify_header_line(width: u16, connected: bool) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;

    const LABEL: &str = " GOTIFY";

    let (glyph, glyph_color, status) = if connected {
        ("\u{25c8}", theme::RUST_ORANGE, "connected")
    } else {
        ("\u{26a0}", theme::STATUS_WARNING, "disconnected")
    };
    let status_width = UnicodeWidthStr::width(glyph) + 1 + UnicodeWidthStr::width(status);
    let chrome = UnicodeWidthStr::width(LABEL) + status_width + usize::from(PANE_PAD);
    let pad = usize::from(width).saturating_sub(chrome).max(1);
    Line::from(vec![
        Span::styled(
            LABEL.to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
        Span::raw(" ".to_owned()),
        Span::styled(status.to_owned(), Style::default().fg(theme::DIM)),
    ])
}

/// Render one GOTIFY subscription entry under its owner header: the
/// full app list in white bold (comma-joined, wrapped to the pane so
/// every name stays visible; `any` when the filter is empty), then a
/// DIM `priority >=N` / `priority any` line one step deeper. The `>=N`
/// floor renders in the default foreground, brighter than the DIM
/// caption. Subscriptions are never merged, so each keeps its own app
/// list + priority.
fn append_gotify_subscription(
    lines: &mut Vec<Line<'static>>,
    sub: &forge_primitives::GotifySubscription,
    inner_width: usize,
) {
    let app_indent = usize::from(PANE_PAD) + 2;
    let priority_indent = usize::from(PANE_PAD) + 4;

    // Chrome reserves the 1-col right gutter plus 1 col for the
    // trailing comma a wrapped line carries, so a flushed line never
    // overruns the gutter.
    let pieces = if sub.applications.is_empty() {
        vec!["any".to_owned()]
    } else {
        wrap_app_list(&sub.applications, row_text_budget(inner_width, app_indent + 2))
    };
    for piece in pieces {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(app_indent)),
            Span::styled(piece, Style::default().add_modifier(Modifier::BOLD)),
        ]));
    }

    let indent = Span::raw(" ".repeat(priority_indent));
    let priority = match sub.min_priority {
        Some(p) => Line::from(vec![
            indent,
            Span::styled("priority ".to_owned(), Style::default().fg(theme::DIM)),
            Span::raw(format!(">={p}")),
        ]),
        None => Line::from(vec![
            indent,
            Span::styled("priority any".to_owned(), Style::default().fg(theme::DIM)),
        ]),
    };
    lines.push(priority);
}

/// Wrap a comma-joined app-name list to `max_width` display columns,
/// breaking only at `, ` separators so no name is split. A name wider
/// than `max_width` still takes its own full line (never truncated). An
/// in-budget wrapped line keeps its trailing comma as a continuation
/// signal; an over-wide lone name flushes without one. An empty slice
/// yields no lines.
fn wrap_app_list(names: &[String], max_width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;

    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for name in names {
        if current.is_empty() {
            current.clone_from(name);
        } else if current.width() + 2 + name.width() <= max_width {
            current.push_str(", ");
            current.push_str(name);
        } else {
            // Continuation comma only on an in-budget line; an over-wide
            // lone name flushes bare so the comma never eats the gutter.
            if current.width() <= max_width {
                current.push(',');
            }
            lines.push(std::mem::take(&mut current));
            current.clone_from(name);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Build a SCHEDULES row from a durable forge cron. The headline is the
/// cron's description (falling back to the prompt's first line); the
/// schedule is humanized (`daily at 09:00` for recurring, a local
/// wall-clock time for run-once). `fire_at` carries the concrete
/// `next_fire` so [`append_schedule_row`] renders a live countdown.
pub(crate) fn forge_cron_to_schedule_entry(
    cron: &forge_primitives::CronEntry,
    now: std::time::SystemTime,
    tz: &time_tz::Tz,
) -> crate::app::ScheduleEntry {
    use crate::ui::schedule_format::{humanize_cron, humanize_once};
    use forge_primitives::cron::CronKind;
    let (recurring, schedule) = match &cron.kind {
        CronKind::Recurring(expr) => (true, humanize_cron(expr)),
        CronKind::Once(at) => (false, humanize_once(*at, now, tz)),
    };
    crate::app::ScheduleEntry {
        key: cron.id.as_str().to_owned(),
        cron_id: Some(cron.id.as_str().to_owned()),
        kind: crate::app::ScheduleKind::Cron { recurring },
        label: first_line(&cron.prompt),
        description: cron.description.clone(),
        schedule,
        fire_at: Some(cron.next_fire),
        created_at: cron.created_at,
    }
}

/// First non-blank line of a prompt, trimmed - the headline fallback
/// when a cron carries no description.
pub(crate) fn first_line(prompt: &str) -> String {
    prompt.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_owned()
}

/// Render a SCHEDULES row. A cron with a headline (its description, else
/// the prompt's first line) renders TWO lines: the headline, then a dim
/// `<humanized schedule> · <badge>` sub-line. Wakeups and headline-less
/// crons render one line (text + trailing badge). The badge is the live
/// `in <countdown>` for wakeups and one-shots, or `recurring` for a
/// repeating cron whose schedule already conveys the timing.
fn append_schedule_row(
    lines: &mut Vec<Line<'static>>,
    entry: &crate::app::ScheduleEntry,
    now: std::time::SystemTime,
    inner_width: usize,
) {
    use crate::app::ScheduleKind;

    // Alarm clock for wakeups; circle-with-upper-left-quadrant for crons
    // (distinct from the chat-label hourglass so the row glyph doesn't
    // double up with the chat-side tool label).
    let glyph = match entry.kind {
        ScheduleKind::Wakeup => "\u{23f0}",
        ScheduleKind::Cron { .. } => "\u{25f4}",
    };
    let countdown = entry.fire_at.and_then(|t| t.duration_since(now).ok());
    let badge = match entry.kind {
        // A recurring cron's schedule ("daily at 09:00") carries the
        // timing, so the badge just marks it as repeating.
        ScheduleKind::Cron { recurring: true, .. } => "recurring".to_owned(),
        ScheduleKind::Cron { recurring: false, .. } => {
            countdown.map_or_else(|| "one-shot".to_owned(), |d| format!("in {}", fmt_countdown(d)))
        }
        ScheduleKind::Wakeup => {
            countdown.map_or_else(|| "due".to_owned(), |d| format!("in {}", fmt_countdown(d)))
        }
    };

    let headline = match entry.kind {
        ScheduleKind::Cron { .. } => {
            entry.description.as_deref().map(str::trim).filter(|s| !s.is_empty()).or_else(|| {
                let l = entry.label.trim();
                (!l.is_empty()).then_some(l)
            })
        }
        ScheduleKind::Wakeup => None,
    };

    if let Some(head) = headline {
        append_schedule_two_line(lines, glyph, head, &entry.schedule, &badge, inner_width);
    } else {
        let text = match entry.kind {
            ScheduleKind::Wakeup => entry.label.as_str(),
            ScheduleKind::Cron { .. } => entry.schedule.as_str(),
        };
        append_schedule_one_line(lines, glyph, text, &badge, inner_width);
    }
}

/// One-line row: glyph + bold text + right-justified `· <badge>`
/// (#281's pad-spacer pattern). Used for wakeups and headline-less crons.
fn append_schedule_one_line(
    lines: &mut Vec<Line<'static>>,
    glyph: &str,
    text: &str,
    badge: &str,
    inner_width: usize,
) {
    let chrome = usize::from(PANE_PAD) + 1 + 1 + 3 + badge.chars().count() + usize::from(PANE_PAD);
    let budget = row_text_budget(inner_width, chrome);
    let headline = truncate_or_pass(text, budget);
    let pad = budget.saturating_sub(headline.chars().count());
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(usize::from(PANE_PAD))),
        Span::styled(glyph.to_owned(), Style::default().fg(theme::RUST_ORANGE)),
        Span::raw(" ".to_owned()),
        Span::styled(headline, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
        Span::styled(" \u{00B7} ".to_owned(), Style::default().fg(theme::DIM)),
        Span::styled(badge.to_owned(), Style::default().fg(theme::DIM)),
    ]));
}

/// Two-line cron row: glyph + bold headline, then a dim indented
/// `<schedule> · <badge>` sub-line with the badge right-justified.
fn append_schedule_two_line(
    lines: &mut Vec<Line<'static>>,
    glyph: &str,
    headline: &str,
    schedule: &str,
    badge: &str,
    inner_width: usize,
) {
    let head_chrome = usize::from(PANE_PAD) + 1 + 1 + usize::from(PANE_PAD);
    let head_budget = row_text_budget(inner_width, head_chrome);
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(usize::from(PANE_PAD))),
        Span::styled(glyph.to_owned(), Style::default().fg(theme::RUST_ORANGE)),
        Span::raw(" ".to_owned()),
        Span::styled(
            truncate_or_pass(headline, head_budget),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));

    let indent = usize::from(PANE_PAD) + 2;
    let chrome = indent + 3 + badge.chars().count() + usize::from(PANE_PAD);
    let budget = row_text_budget(inner_width, chrome);
    let sched = truncate_or_pass(schedule, budget);
    let pad = budget.saturating_sub(sched.chars().count());
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(sched, Style::default().fg(theme::DIM)),
        Span::raw(" ".repeat(pad)),
        Span::styled(" \u{00B7} ".to_owned(), Style::default().fg(theme::DIM)),
        Span::styled(badge.to_owned(), Style::default().fg(theme::DIM)),
    ]));
}

/// Compact human countdown for the SCHEDULES wakeup badge:
/// `45s`, `12m`, `1h32m`. Matches the rough shape of `format_turn_duration`
/// without sub-second precision (one-second tick granularity is the
/// finest the renderer ever sees).
fn fmt_countdown(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// Append the SUBAGENTS Inspector section. Header + one entry per
/// active `Task` / `Agent` dispatch, each followed (while running) by
/// a live tail of the last `SUBAGENT_TAIL_CAP` otherwise-hidden child
/// tool calls under that root. Terminal entries collapse their tail
/// to a `· N tools` summary on the header line. The whole section
/// disappears when every visible root reaches a terminal status - the
/// slice is empty in that case (mirroring
/// `clear_workflows_if_all_terminal`).
fn append_subagents_section(
    lines: &mut Vec<Line<'static>>,
    app: &App,
    width: u16,
    subagents: &[crate::app::SubagentEntry],
) {
    if subagents.is_empty() {
        return;
    }

    lines.push(Line::from(Span::styled(
        " SUBAGENTS".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    let inner_width = usize::from(width);
    let last_idx = subagents.len().saturating_sub(1);
    for (idx, entry) in subagents.iter().enumerate() {
        append_subagent_row(lines, entry, inner_width, app.active_spinner_glyph());
        if idx < last_idx {
            lines.push(Line::default());
        }
    }
}

/// Render one SUBAGENTS entry: header (status_icon + `◇` + label,
/// plus a `· N tools` trailing summary on terminal roots) and, for
/// a running root, an indented tail of the last 3-4 hidden child
/// tool calls. Each tail row reuses `theme::tool_name_label` so the
/// kind glyph + label match the standard chat tool row.
fn append_subagent_row(
    lines: &mut Vec<Line<'static>>,
    entry: &crate::app::SubagentEntry,
    inner_width: usize,
    active_glyph: char,
) {
    use crate::agent::model::ToolCallStatus;

    let in_progress = matches!(entry.status, ToolCallStatus::InProgress | ToolCallStatus::Pending);
    let (status_glyph, status_color) = match entry.status {
        ToolCallStatus::Completed => (theme::ICON_COMPLETED.to_owned(), Color::Green),
        ToolCallStatus::Failed | ToolCallStatus::Killed => {
            (theme::ICON_FAILED.to_owned(), theme::STATUS_ERROR)
        }
        ToolCallStatus::Pending => ("\u{25cb}".to_owned(), theme::DIM),
        ToolCallStatus::InProgress => (active_glyph.to_string(), theme::RUST_ORANGE),
    };
    // Terminal roots get a `  · N tools` summary right-justified on
    // the header (matches MONITORS / WORKFLOWS / SCHEDULES'
    // pad-spacer pattern). In-progress roots have no summary on the
    // header line; their tail rows render the live activity below.
    let trailing = if in_progress {
        String::new()
    } else {
        let noun = if entry.total_count == 1 { "tool" } else { "tools" };
        format!("{} {}", entry.total_count, noun)
    };
    let trailing_chrome = if trailing.is_empty() {
        0
    } else {
        3 /* " · " */ + trailing.chars().count()
    };
    let header_chrome = usize::from(PANE_PAD)
        + 1   // status glyph
        + 1   // space after status
        + 1   // ◇ kind glyph
        + 1   // space after ◇
        + trailing_chrome
        + usize::from(PANE_PAD);
    let header_budget = row_text_budget(inner_width, header_chrome);
    let label = truncate_or_pass(&entry.label, header_budget);
    let pad = header_budget.saturating_sub(label.chars().count());
    let mut header_spans = vec![
        Span::raw(" ".repeat(usize::from(PANE_PAD))),
        Span::styled(status_glyph, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        Span::raw(" ".to_owned()),
        Span::styled("\u{25c7}".to_owned(), Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".to_owned()),
        Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
    ];
    if !trailing.is_empty() {
        header_spans.push(Span::raw(" ".repeat(pad)));
        header_spans.push(Span::styled(" \u{00B7} ".to_owned(), Style::default().fg(theme::DIM)));
        header_spans.push(Span::styled(trailing, Style::default().fg(theme::DIM)));
    }
    lines.push(Line::from(header_spans));

    // Tail gated on `entry.tail` alone - the derive fills it only for a
    // running root, so render never recomputes a status predicate. 6-space
    // indent nests each row under the header like a chat tool-call row.
    let tail_indent = "      "; // 6 spaces - 2 pane pad + 4 for the nest.
    let fixed_chrome = tail_indent.chars().count()
        + 1   // kind glyph cell
        + 1   // space after kind glyph
        + 2   // "  " between kind label + title
        + usize::from(PANE_PAD); // right gutter
    for child in &entry.tail {
        let (kind_glyph, kind_label) = theme::tool_name_label(&child.sdk_tool_name);
        let title_budget =
            row_text_budget(inner_width, fixed_chrome.saturating_add(kind_label.chars().count()));
        let title = truncate_or_pass(&child.title, title_budget.max(1));
        lines.push(Line::from(vec![
            Span::raw(tail_indent.to_owned()),
            Span::styled(kind_glyph.to_owned(), Style::default().fg(theme::DIM)),
            Span::raw(" ".to_owned()),
            Span::styled(kind_label.to_owned(), Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("  ".to_owned()),
            Span::styled(title, Style::default().fg(theme::DIM)),
        ]));
    }
}

/// Append the WORKFLOWS Inspector section. Header +
/// one row per workflow entry with the meta name + status, then
/// (when running or expanded) a per-phase tree showing status
/// glyph + title + log tail. Section is hidden when
/// `UiSession.workflows` is empty (auto-clears once every entry
/// transitions to `Completed`).
fn append_workflows_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    let workflows = app.workflows();
    if workflows.is_empty() {
        return;
    }

    lines.push(Line::from(Span::styled(
        " WORKFLOWS".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    let inner_width = usize::from(width);
    let last_idx = workflows.len().saturating_sub(1);
    for (idx, workflow) in workflows.iter().enumerate() {
        append_workflow_row(lines, workflow, inner_width, app.active_spinner_glyph());
        // blank between entries (matches the MONITORS
        // section's inter-entry spacing).
        if idx < last_idx {
            lines.push(Line::default());
        }
    }
}

/// Render one Workflow entry into the Inspector body. Layout:
/// header (glyph + meta_name + status badge), optional description
/// subtitle, then a tree of phases with logs as continuation rows.
fn append_workflow_row(
    lines: &mut Vec<Line<'static>>,
    workflow: &crate::app::WorkflowEntry,
    inner_width: usize,
    active_glyph: char,
) {
    use crate::app::{PhaseStatus, WorkflowStatus};

    let (status_label, status_color) = match workflow.status {
        WorkflowStatus::InProgress => ("in progress", theme::RUST_ORANGE),
        WorkflowStatus::Completed => ("done", Color::Green),
    };
    let glyph =
        if workflow.is_in_progress() { active_glyph.to_string() } else { "\u{25c6}".to_owned() };
    let glyph_color = if workflow.is_in_progress() { theme::RUST_ORANGE } else { Color::Green };

    // same shape as MONITORS header. Badge follows
    // truncated text; count it in chrome up-front.
    let header_chrome = usize::from(PANE_PAD)
        + 1   // glyph
        + 1   // space
        + 3   // " · "
        + status_label.chars().count()
        + usize::from(PANE_PAD);
    let header_budget = row_text_budget(inner_width, header_chrome);
    let header_text = truncate_or_pass(&workflow.meta_name, header_budget);
    // #281: same pad-spacer shape as MONITORS - see comment there.
    let pad = header_budget.saturating_sub(header_text.chars().count());
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(usize::from(PANE_PAD))),
        Span::styled(glyph, Style::default().fg(glyph_color)),
        Span::raw(" ".to_owned()),
        Span::styled(header_text, Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(pad)),
        Span::styled(" \u{00B7} ".to_owned(), Style::default().fg(theme::DIM)),
        Span::styled(status_label.to_owned(), Style::default().fg(status_color)),
    ]));

    // Description subtitle: 4-col indent + 1-col right gutter.
    let desc_chrome = 4 + usize::from(PANE_PAD);
    let desc_budget = row_text_budget(inner_width, desc_chrome);
    if let Some(desc) = workflow.meta_description.as_deref().filter(|d| !d.is_empty()) {
        let row = truncate_or_pass(desc, desc_budget);
        lines.push(Line::from(vec![
            Span::raw("    ".to_owned()),
            Span::styled(row, Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC)),
        ]));
    }

    // Show the phase tree when the workflow is running OR the user
    // expanded it explicitly. A completed-and-collapsed workflow
    // shows just the header + (optional) final_result_summary.
    let show_tree = workflow.is_in_progress() || workflow.expanded_in_inspector;
    if show_tree {
        // Phase-row chrome: 1-col indent + connector glyph + space
        // + phase glyph + space + 1-col right gutter.
        let phase_chrome = usize::from(PANE_PAD)
            + 1   // connector (└ or ├)
            + 1   // space
            + 1   // phase glyph
            + 1   // space
            + usize::from(PANE_PAD);
        let phase_budget = row_text_budget(inner_width, phase_chrome);
        // Log-row chrome: 1-col indent + box-drawing column + 3-col
        // pad + 1-col gutter. Uses the same total chrome the
        // continuation indent emits (`"  │   "` or six spaces).
        let log_chrome = usize::from(PANE_PAD)
            + 1   // column glyph (│) or space
            + 3   // padding before text
            + usize::from(PANE_PAD);
        let log_budget = row_text_budget(inner_width, log_chrome);
        let phase_count = workflow.phases.len();
        for (i, phase) in workflow.phases.iter().enumerate() {
            let is_last = i + 1 == phase_count;
            let connector_glyph = if is_last { "\u{2514}" } else { "\u{251c}" };
            let (phase_glyph, phase_color) = match phase.status {
                PhaseStatus::Completed => ("\u{2713}".to_owned(), Color::Green),
                PhaseStatus::InProgress => (active_glyph.to_string(), theme::RUST_ORANGE),
                PhaseStatus::Pending => ("\u{25CB}".to_owned(), theme::DIM),
            };
            let row = truncate_or_pass(&phase.title, phase_budget);
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(usize::from(PANE_PAD))),
                Span::styled(connector_glyph.to_owned(), Style::default().fg(theme::DIM)),
                Span::raw(" ".to_owned()),
                Span::styled(phase_glyph, Style::default().fg(phase_color)),
                Span::raw(" ".to_owned()),
                Span::styled(row, Style::default().fg(theme::DIM)),
            ]));
            // Continuation indent: 1-col left + column-glyph (│ or
            // ' ') + 3-col pad = 5 cols before the log text. The
            // chrome accounting above matches this exactly.
            let column_glyph = if is_last { ' ' } else { '\u{2502}' };
            let logs_indent = format!("{}{column_glyph}   ", " ".repeat(usize::from(PANE_PAD)));
            for log in &phase.logs {
                let log_row = truncate_or_pass(log, log_budget);
                lines.push(Line::from(vec![
                    Span::styled(logs_indent.clone(), Style::default().fg(theme::DIM)),
                    Span::styled(log_row, Style::default().fg(theme::DIM)),
                ]));
            }
        }
    }

    if let Some(summary) = workflow.final_result_summary.as_deref().filter(|s| !s.is_empty()) {
        // Summary row chrome: 1-col indent + connector + space + 2-col `✓ `
        // prefix + 1-col right gutter.
        let summary_chrome = usize::from(PANE_PAD)
            + 1   // └
            + 1   // space
            + 2   // ✓ + space
            + usize::from(PANE_PAD);
        let summary_budget = row_text_budget(inner_width, summary_chrome);
        let row = truncate_or_pass(summary, summary_budget);
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(usize::from(PANE_PAD))),
            Span::styled("\u{2514} ".to_owned(), Style::default().fg(theme::DIM)),
            Span::styled("\u{2713} ".to_owned(), Style::default().fg(Color::Green)),
            Span::styled(row, Style::default().fg(theme::DIM)),
        ]));
    }
}

/// Single source of truth for inspector-row
/// width budgeting. Every row variant (TASKS / PROCESSES /
/// MONITORS header / MONITORS tail / WORKFLOWS header / WORKFLOWS
/// phase / WORKFLOWS log / final-result summary) feeds its actual
/// chrome glyph count into this helper so all sections observe
/// the same 1-col right gutter (TASKS' convention) and no variant
/// silently reintroduces divergence.
///
/// `chrome_chars` is the SUM of every non-content cell the row will
/// emit (left indent + glyph + spaces + connector + status badge +
/// `... right gutter`). Result is bounded at `max(1)` so a
/// pathologically narrow pane still produces a usable budget for
/// `truncate_with_ellipsis` rather than 0 (which would render as a
/// bare `...`).
///
/// Contract: for any row built with this helper,
/// `chrome_chars + rendered_text_width <= inner_width` always
/// holds. The 1-col right gutter is folded into `chrome_chars` by
/// the caller; callers MUST include it so the architectural
/// gutter-consistency test catches future row variants that forget.
fn row_text_budget(inner_width: usize, chrome_chars: usize) -> usize {
    inner_width.saturating_sub(chrome_chars).max(1)
}

/// Helper: truncate a string to `max_chars` columns, appending a
/// single `...` ellipsis when the input is longer. Returns the input
/// unchanged when it already fits.
fn truncate_or_pass(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('\u{2026}');
    out
}

/// Wrap `s` onto multiple lines so that each piece fits within
/// `max_chars` columns. Breaks on whitespace where possible; falls
/// back to hard-cut on long single tokens. Returns an empty `Vec`
/// for an empty / whitespace-only input.
/// Render the PROCESSES section: header + one row per
/// currently-in-flight long-running tool call. Each row spans up
/// to three lines:
///
/// 1. **Headline:** status glyph + headline text styled per
///    [`ProcessKind`].
/// 2. **Detail** (optional `└─` continuation): the underlying shell
///    command or cron prompt, DIM, truncated with `...` when it
///    overflows.
/// 3. **Metadata** (`└─` continuation): kind label · status · flags,
///    all DIM.
///
/// Glyphs mirror the TASKS convention but use a kind-distinct
/// palette for the headline so scanning the section visually
/// separates "what's running" from "what's queued in the Task* family":
///
/// - `▸` RUST_ORANGE  - `BashBackgrounded` / `Monitor` while in-flight
/// - `\u{23F0}` (`⏰`) DIM - `Cron` (scheduled, not currently firing)
/// - `\u{2713}` (`✓`) green - completed tool call (any kind)
/// - `\u{2717}` (`✗`) red - failed / killed
/// - `\u{25CB}` (`○`) DIM - pending (queued, not yet started)
fn append_processes_section(
    lines: &mut Vec<Line<'static>>,
    collection: &ProcessCollection,
    width: u16,
    active_glyph: char,
) {
    lines.push(Line::from(Span::styled(
        " PROCESSES".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    let include_memory = width >= PROCESSES_MEMORY_WIDTH_THRESHOLD;
    let process_count = collection.rows.len();
    for (idx, process) in collection.rows.iter().enumerate() {
        append_process_row(lines, process, width, include_memory, active_glyph);
        // Blank between SUPERVISOR rows (depth-0). Descendant rows
        // pack tight under their supervisor with no blanks so the
        // tree reads as one connected block.
        let next_is_supervisor = collection.rows.get(idx + 1).is_some_and(|next| next.depth == 0);
        if idx + 1 < process_count && next_is_supervisor {
            lines.push(Line::default());
        }
    }

    // No `+N more` footer - the inspector pane scrolls, so the
    // scrollbar IS the overflow indicator.
}

/// Render one process row as a single line. Same shape for
/// supervisors (depth 0) and descendants (depth ≥ 1) - the only
/// difference is depth-0 has no tree-connector chrome, while
/// depth-≥1 emits per-ancestor continuation cols + a connector.
///
/// Format: `<pane pad><ancestor cols><connector><glyph> <headline> · <memory>`
/// where:
/// - Ancestor cols: one 3-col chunk per ancestor depth (`│  `
///   when that ancestor has more siblings below, `   ` when not).
///   Empty at depth 0.
/// - Connector: `└─ ` (last sibling) or `├─ ` (more siblings).
///   Empty at depth 0.
/// - Memory suffix: `· NN MB` when `memory_bytes` is set AND the
///   layout has room; for Cron rows (no memory) the kind/recurring
///   metadata fills the slot instead.
///
/// The redundant `Bash · running` / `Process · running` metadata
/// dropped - the glyph + colour already convey kind, "running" is
/// implicit (every shown row is running), and "· 83 MB" alone is
/// enough useful detail. Cron rows still show their `Cron ·
/// recurring · session-only` metadata because they have no memory
/// to display.
fn append_process_row(
    lines: &mut Vec<Line<'static>>,
    process: &ProcessRow,
    width: u16,
    include_memory: bool,
    active_glyph: char,
) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(" ".repeat(usize::from(PANE_PAD))));

    // Tree chrome (ancestor cols + connector) renders at depth ≥ 1
    // only. Depth-0 rows (supervisors + Cron) start flush with
    // PANE_PAD and skip directly to the glyph.
    //
    // Skip the FIRST ancestor entry - supervisors are visualised as
    // section roots flush at col 2, not as ancestors needing a
    // continuation column. So depth-1 kids hang their `├─` from col 2
    // directly (matching the Projects pane org-grouped layout) and
    // deeper levels only add continuation chunks for genuine
    // grandparents and beyond.
    let tree_chrome_cols = if process.depth == 0 {
        0
    } else {
        let ancestors = process.ancestor_has_more.get(1..).unwrap_or(&[]);
        for has_more in ancestors {
            let chunk = if *has_more { "\u{2502}  " } else { "   " };
            spans.push(Span::styled(chunk.to_owned(), Style::default().fg(theme::DIM)));
        }
        let connector =
            if process.is_last_sibling { "\u{2514}\u{2500} " } else { "\u{251C}\u{2500} " };
        spans.push(Span::styled(connector.to_owned(), Style::default().fg(theme::DIM)));
        ancestors.len() * 3 + 3
    };

    // Glyph + space + headline + optional ` · <suffix>`.
    //
    // Glyph rendering is depth-0 only - supervisors carry the
    // spinner/loader glyph (RUST_ORANGE for wire-matched Bash /
    // Monitor; DIM for generic OS processes); descendants drop it
    // because the tree connector + indent already says "child of
    // the row above" and adding a glyph per child clutters the
    // view. Depth-0 chrome adds 2 cols for glyph+space; depth-≥1
    // adds 0 cols past the tree connector itself.
    //
    // Suffix priority:
    // 1. Memory (`12 MB`) - when `memory_bytes` is set + layout
    //    has room. Drops the redundant `Kind · running` metadata
    //    string since glyph + colour already convey kind and
    //    "running" is implicit.
    // 2. Cron's `Cron · recurring · session-only` metadata - used
    //    when memory is `None` (Cron rows are wire-only, no
    //    backing process). The kind/recurring info IS the useful
    //    signal there.
    // 3. Nothing - `+N more` overflow rows have no memory + empty
    //    metadata, so the row is just glyph + headline.
    let (glyph, glyph_color, headline_style) = glyph_and_style_for(process, active_glyph);
    let glyph_cols = if process.depth == 0 {
        spans.push(Span::styled(glyph, Style::default().fg(glyph_color)));
        spans.push(Span::raw(" "));
        2
    } else {
        // Descendant - glyph + space dropped. Style is reused for
        // the headline below so wire-matched-vs-unmatched colour
        // still differentiates the row.
        let _ = (glyph, glyph_color);
        0
    };

    // Suffix is the useful signal - memory for process-backed rows,
    // Cron metadata for wire-only registrations. Always include it
    // when set; the headline truncates with `...` to make room.
    let suffix_text: Option<String> = match (include_memory, process.memory_bytes) {
        (true, Some(bytes)) => Some(format_memory_short(bytes)),
        _ => {
            if process.metadata.is_empty() {
                None
            } else {
                Some(process.metadata.clone())
            }
        }
    };
    let suffix_chars = suffix_text.as_ref().map_or(0, |s| 3 + s.chars().count()); // " · " + value
    // every inspector row routes its chrome
    // budget through `row_text_budget` so PROCESSES + TASKS +
    // MONITORS + WORKFLOWS observe the same right-gutter contract.
    let chrome_chars = usize::from(PANE_PAD)
        + tree_chrome_cols
        + glyph_cols
        + suffix_chars
        + usize::from(PANE_PAD); // right gutter
    let headline_budget = row_text_budget(usize::from(width), chrome_chars);
    let headline_fitted = truncate_with_ellipsis(&process.headline, headline_budget);

    spans.push(Span::styled(headline_fitted, headline_style));
    if let Some(suffix) = suffix_text {
        spans.push(Span::styled(" \u{00B7} ".to_owned(), Style::default().fg(theme::DIM)));
        spans.push(Span::styled(suffix, Style::default().fg(theme::DIM)));
    }

    lines.push(Line::from(spans));
}

/// Pick the (glyph, glyph_color, headline_style) triple for a
/// process row based on its `kind` + `status`. Terminal statuses
/// (Completed / Failed / Killed) override the kind glyph so the
/// section reads accurately as a state monitor regardless of the
/// originating tool kind.
fn glyph_and_style_for(process: &ProcessRow, active_glyph: char) -> (String, Color, Style) {
    match process.status {
        ToolCallStatus::Completed => {
            ("\u{2713}".to_owned(), Color::Green, Style::default().fg(theme::DIM))
        }
        ToolCallStatus::Failed | ToolCallStatus::Killed => (
            "\u{2717}".to_owned(),
            Color::Red,
            Style::default().fg(theme::DIM).add_modifier(Modifier::CROSSED_OUT),
        ),
        ToolCallStatus::Pending => {
            ("\u{25CB}".to_owned(), theme::DIM, Style::default().fg(Color::Gray))
        }
        ToolCallStatus::InProgress => match process.kind {
            ProcessKind::Process => {
                // Unmatched OS process - same spinner as wire-tracked
                // rows but DIM so the user's eye picks out the
                // bright-coloured matched rows first. Still animates
                // because the row IS live work.
                (active_glyph.to_string(), theme::DIM, Style::default().fg(Color::Gray))
            }
            ProcessKind::Overflow => {
                // Synthetic `+N more` row. No glyph; the dim italic
                // text alone signals it's a placeholder.
                (
                    String::new(),
                    theme::DIM,
                    Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
                )
            }
            ProcessKind::BashBackgrounded => (
                // Backgrounded Bash - either an OS-matched wire row or a
                // registry-fed synthetic row - in a RUST_ORANGE spinner so
                // it stands out as "tracked work" against the dim spinners
                // of generic OS processes.
                active_glyph.to_string(),
                theme::RUST_ORANGE,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        },
    }
}

fn wrap_text(s: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in s.split_whitespace() {
        let word_chars = word.chars().count();
        if word_chars > max_chars {
            // Long single token - flush current, then hard-cut the
            // long word across multiple lines.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            let mut chars = word.chars().peekable();
            while chars.peek().is_some() {
                let piece: String = chars.by_ref().take(max_chars).collect();
                out.push(piece);
            }
            continue;
        }
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word_chars <= max_chars {
            current.push(' ');
            current.push_str(word);
        } else {
            out.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Truncate `s` to at most `max_chars` characters with a trailing
/// `...` ellipsis. Returns the original string if it already fits.
/// When `max_chars` is `0` or `1` the result is just `...`.
fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    if max_chars <= 1 {
        return "\u{2026}".to_owned();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn wrap_short_text_returns_single_line() {
        assert_eq!(wrap_text("hello world", 20), vec!["hello world".to_owned()]);
    }

    #[test]
    fn wrap_long_text_breaks_on_whitespace() {
        let wrapped = wrap_text("Adding tests for the near-threshold branch", 18);
        assert_eq!(
            wrapped,
            vec![
                "Adding tests for".to_owned(),
                "the near-threshold".to_owned(),
                "branch".to_owned()
            ]
        );
    }

    #[test]
    fn wrap_long_token_hard_cuts() {
        let wrapped = wrap_text("supercalifragilisticexpialidocious tail", 10);
        // The 34-char token cuts into 10+10+10+4. Remaining `tail`
        // starts its own line because the hard-cut path emits its
        // pieces directly without joining.
        assert_eq!(
            wrapped,
            vec![
                "supercalif".to_owned(),
                "ragilistic".to_owned(),
                "expialidoc".to_owned(),
                "ious".to_owned(),
                "tail".to_owned(),
            ]
        );
    }

    #[test]
    fn forge_cron_to_schedule_entry_recurring_carries_expr_and_next_fire() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        use std::time::{Duration, SystemTime};
        let created = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let next = created + Duration::from_secs(300);
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: created,
            description: None,
            last_fire: None,
            next_fire: next,
            team_role: None,
        };
        let entry = forge_cron_to_schedule_entry(&cron, next, time_tz::timezones::db::UTC);
        assert_eq!(entry.schedule, "daily at 09:00", "recurring cron humanizes its expression");
        assert_eq!(entry.label, "p", "the prompt first line is the headline fallback");
        assert_eq!(entry.description, None, "no description on this cron");
        assert_eq!(entry.fire_at, Some(next), "next_fire carried for the countdown");
        assert!(matches!(entry.kind, crate::app::ScheduleKind::Cron { recurring: true }));
        assert_eq!(entry.cron_id.as_deref(), Some("c1"));
    }

    #[test]
    fn forge_cron_to_schedule_entry_run_once_humanizes_the_time() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        use std::time::{Duration, SystemTime};
        // 2000s past the epoch is 00:33 UTC on the epoch day.
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        let cron = CronEntry {
            id: CronId::from("o1"),
            project_name: "forge".to_owned(),
            kind: CronKind::Once(at),
            prompt: "deploy staging\nsecond line".to_owned(),
            created_at: SystemTime::UNIX_EPOCH,
            description: Some("Staging deploy".to_owned()),
            last_fire: None,
            next_fire: at,
            team_role: None,
        };
        let entry = forge_cron_to_schedule_entry(&cron, at, time_tz::timezones::db::UTC);
        assert_eq!(entry.schedule, "today 00:33", "run-once cron shows a local wall-clock time");
        assert_eq!(entry.description.as_deref(), Some("Staging deploy"));
        assert_eq!(entry.label, "deploy staging", "label is the prompt's first line");
        assert!(matches!(entry.kind, crate::app::ScheduleKind::Cron { recurring: false, .. }));
    }

    fn cron_entry(
        recurring: bool,
        label: &str,
        description: Option<&str>,
        schedule: &str,
        fire_at: Option<std::time::SystemTime>,
    ) -> crate::app::ScheduleEntry {
        crate::app::ScheduleEntry {
            key: "c1".to_owned(),
            cron_id: Some("c1".to_owned()),
            kind: crate::app::ScheduleKind::Cron { recurring },
            label: label.to_owned(),
            description: description.map(str::to_owned),
            schedule: schedule.to_owned(),
            fire_at,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn recurring_cron_row_is_two_lines_headline_then_schedule_and_recurring_badge() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let entry = cron_entry(
            true,
            "stand-up",
            Some("Morning summary"),
            "daily at 09:00",
            Some(now + Duration::from_secs(300)),
        );
        let mut lines = Vec::new();
        append_schedule_row(&mut lines, &entry, now, 60);
        assert_eq!(lines.len(), 2, "a described cron renders two lines");
        let head = line_text(&lines[0]);
        let sub = line_text(&lines[1]);
        assert!(head.contains("Morning summary"), "line 1 is the description headline: {head}");
        assert!(sub.contains("daily at 09:00"), "line 2 shows the humanized schedule: {sub}");
        assert!(
            sub.contains("recurring"),
            "a recurring cron badges `recurring`, not a countdown: {sub}"
        );
        assert!(!sub.contains("in "), "no countdown on a recurring cron: {sub}");
    }

    #[test]
    fn once_cron_row_shows_absolute_time_and_live_countdown() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let entry = cron_entry(
            false,
            "deploy",
            Some("Staging deploy"),
            "today 14:30",
            Some(now + Duration::from_secs(300)),
        );
        let mut lines = Vec::new();
        append_schedule_row(&mut lines, &entry, now, 60);
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains("Staging deploy"));
        let sub = line_text(&lines[1]);
        assert!(sub.contains("today 14:30"), "sub-line shows the absolute time: {sub}");
        assert!(sub.contains("in 5m"), "a one-shot keeps its live countdown: {sub}");
    }

    #[test]
    fn cron_row_without_description_falls_back_to_prompt_first_line() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let entry = cron_entry(
            true,
            "deploy staging",
            None,
            "weekdays at 09:00",
            Some(now + Duration::from_secs(60)),
        );
        let mut lines = Vec::new();
        append_schedule_row(&mut lines, &entry, now, 60);
        assert_eq!(lines.len(), 2);
        assert!(
            line_text(&lines[0]).contains("deploy staging"),
            "the prompt-derived label headlines when there is no description",
        );
        assert!(line_text(&lines[1]).contains("weekdays at 09:00"));
    }

    #[test]
    fn wakeup_row_stays_one_line_with_reason_and_countdown() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let entry = crate::app::ScheduleEntry {
            key: "w1".to_owned(),
            cron_id: None,
            kind: crate::app::ScheduleKind::Wakeup,
            label: "watching CI run".to_owned(),
            description: None,
            schedule: String::new(),
            fire_at: Some(now + Duration::from_secs(480)),
            created_at: now,
        };
        let mut lines = Vec::new();
        append_schedule_row(&mut lines, &entry, now, 60);
        assert_eq!(lines.len(), 1, "wakeups remain a single line");
        let text = line_text(&lines[0]);
        assert!(text.contains("watching CI run"), "reason is the headline: {text}");
        assert!(text.contains("in 8m"), "wakeup keeps its countdown: {text}");
    }

    #[test]
    fn headline_less_cloud_cron_renders_one_line_with_schedule() {
        // A cloud CronCreate carries no description and no prompt, so the
        // headline is empty and the row collapses to a single line showing
        // the humanized schedule + badge.
        use std::time::SystemTime;
        let now = SystemTime::UNIX_EPOCH;
        let entry = cron_entry(true, "", None, "every 5 minutes", None);
        let mut lines = Vec::new();
        append_schedule_row(&mut lines, &entry, now, 60);
        assert_eq!(lines.len(), 1, "a headline-less cron is one line, not a blank headline");
        let text = line_text(&lines[0]);
        assert!(text.contains("every 5 minutes"), "the schedule is the row text: {text}");
        assert!(text.contains("recurring"), "with its recurrence badge: {text}");
    }

    #[test]
    fn two_line_cron_row_stays_within_a_narrow_pane() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::UNIX_EPOCH;
        let entry = cron_entry(
            false,
            "prompt fallback headline that is far too long for the pane",
            Some("An extremely long description headline that exceeds the pane budget"),
            "an unusually long humanized schedule that also exceeds the width",
            Some(now + Duration::from_secs(300)),
        );
        let width = 24usize;
        let mut lines = Vec::new();
        append_schedule_row(&mut lines, &entry, now, width);
        assert_eq!(lines.len(), 2, "a described cron renders two lines");
        for line in &lines {
            let cols = line_text(line).chars().count();
            assert!(
                cols <= width,
                "row overflows the {width}-col pane ({cols}): {:?}",
                line_text(line)
            );
        }
    }

    #[test]
    fn schedules_section_renders_forge_cron_row_humanized() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        use std::time::{Duration, SystemTime};

        let mut app = App::test_default();
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "cronproj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: SystemTime::now() + Duration::from_secs(3600),
            team_role: None,
        };
        app.forge_schedule_rows = vec![forge_cron_to_schedule_entry(
            &cron,
            SystemTime::now(),
            time_tz::timezones::db::UTC,
        )];

        let mut lines = Vec::new();
        append_schedules_section(&mut lines, &app, 60);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("SCHEDULES"), "section header renders: {text}");
        assert!(text.contains("stand-up"), "the prompt-derived headline renders: {text}");
        assert!(text.contains("daily at 09:00"), "forge cron row humanizes its schedule: {text}");
        assert!(text.contains("recurring"), "a recurring cron badges recurring: {text}");
    }

    #[test]
    fn append_body_draws_schedules_from_forge_crons_without_cloud_wakeups() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        use std::time::{Duration, SystemTime};

        // Regression: the section gate must draw SCHEDULES from durable
        // forge crons even when the cloud-wakeup list is empty.
        let mut app = App::test_default();
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "cronproj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: SystemTime::now() + Duration::from_secs(3600),
            team_role: None,
        };
        app.forge_schedule_rows = vec![forge_cron_to_schedule_entry(
            &cron,
            SystemTime::now(),
            time_tz::timezones::db::UTC,
        )];
        assert!(app.schedules().is_empty(), "precondition: no cloud wakeups");

        let mut lines = Vec::new();
        append_body(&mut lines, &app, 60, &app.subagents_view());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            text.contains("SCHEDULES"),
            "durable crons render SCHEDULES even with no cloud wakeups: {text}"
        );
        assert!(text.contains("daily at 09:00"), "the cron row is present: {text}");
    }

    /// SCHEDULES renders two sources. Only the forge crons are scoped by
    /// owner; the Claude-native wakeups live on the session's own bucket
    /// and still render when the session owns no forge cron.
    #[test]
    fn schedules_section_renders_native_wakeups_with_no_owned_forge_crons() {
        use std::time::{Duration, SystemTime};

        let mut app = App::test_default();
        app.upsert_wakeup_from_tool_input(
            "tu1",
            "watching CI",
            SystemTime::now() + Duration::from_secs(600),
        );
        assert!(app.forge_schedule_rows.is_empty(), "precondition: no owned forge crons");

        let mut lines = Vec::new();
        append_body(&mut lines, &app, 60, &app.subagents_view());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("SCHEDULES"), "the native wakeup still draws SCHEDULES: {text}");
        assert!(text.contains("watching CI"), "the wakeup reason renders: {text}");
    }

    #[test]
    fn append_body_omits_background_section() {
        // The standalone BACKGROUND section is gone: even a populated
        // background_tasks snapshot never renders a BACKGROUND header.
        use crate::app::BackgroundTask;
        let mut app = App::test_default();
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "b1".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "Run the integration suite".to_owned(),
        }];
        let mut lines = Vec::new();
        append_body(&mut lines, &app, 60, &app.subagents_view());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("BACKGROUND"), "BACKGROUND section removed: {text}");
    }

    #[test]
    fn append_body_surfaces_turn_outlived_backgrounded_bash_under_processes() {
        // A backgrounded Bash that outlived its spawning turn: the Bash is
        // in the transcript, its session-scoped task_id mapping survives,
        // the CLI registry still lists it, but turn_state is empty (turn
        // finalised) and there's no process snapshot (the ~1 s scan hasn't
        // caught it). It must still surface - under PROCESSES.
        use crate::app::{BackgroundTask, ChatMessage, MessageBlock, MessageRole};
        let mut app = App::test_default();
        let mut bash = subagent_test_child_info("tu-bash", "Bash", "npm run build");
        bash.raw_input =
            Some(serde_json::json!({ "command": "npm run build", "run_in_background": true }));
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(bash))],
        ));
        // Session-scoped mapping (from task_started) - turn_state is left
        // empty to simulate a finalised turn.
        app.insert_session_task_mapping("b1".to_owned(), "tu-bash".to_owned());
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "b1".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "Run the integration suite".to_owned(),
        }];
        let mut lines = Vec::new();
        append_body(&mut lines, &app, 60, &app.subagents_view());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("PROCESSES"), "PROCESSES section renders: {text}");
        assert!(
            text.contains("Run the integration suite"),
            "bash row renders under PROCESSES: {text}"
        );
        assert!(text.contains("local_bash"), "task_type tag renders: {text}");
        assert!(!text.contains("BACKGROUND"), "no BACKGROUND section: {text}");
    }

    #[test]
    fn append_body_omits_backgrounded_agent_from_processes() {
        // A backgrounded agent routes to SUBAGENTS, never a flat
        // PROCESSES row - the feed filters to local_bash.
        use crate::app::BackgroundTask;
        let mut app = App::test_default();
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "a1".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "Review conv-row animation".to_owned(),
        }];
        let mut lines = Vec::new();
        append_body(&mut lines, &app, 60, &app.subagents_view());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(!text.contains("BACKGROUND"), "no BACKGROUND section: {text}");
        assert!(
            !text.contains("Review conv-row animation"),
            "agent is not surfaced as a flat PROCESSES row: {text}"
        );
    }

    #[test]
    fn collect_processes_dedups_backgrounded_bash_to_exactly_one_row() {
        // The same backgrounded bash is in the transcript (wire-alive), the
        // OS snapshot (matching cmdline), and the CLI registry. It must
        // render EXACTLY once - the enriched OS row - not doubled by the
        // authoritative feed.
        use crate::app::{BackgroundTask, ChatMessage, MessageBlock, MessageRole};
        use forge_workspace::env::processes::{ProcessEntry, ProcessSnapshot};
        let mut app = App::test_default();
        let mut bash = subagent_test_child_info("tu-bash", "Bash", "cargo nextest run");
        bash.raw_input = Some(serde_json::json!({
            "command": "cargo nextest run",
            "description": "Run unit tests",
            "run_in_background": true,
        }));
        bash.status = crate::agent::model::ToolCallStatus::InProgress;
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(bash))],
        ));
        app.insert_session_task_mapping("b1".to_owned(), "tu-bash".to_owned());
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "b1".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "Run unit tests".to_owned(),
        }];
        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![ProcessEntry {
                pid: 42,
                parent_pid: 1,
                name: "zsh".to_owned(),
                command: "/bin/zsh -c -l eval 'cargo nextest run' < /dev/null".to_owned(),
                memory_bytes: 32 * 1024 * 1024,
            }],
        });

        let coll = crate::app::processes::collect_active_processes(&app, &HashSet::default());
        let count = coll.rows.iter().filter(|row| row.headline == "Run unit tests").count();
        assert_eq!(count, 1, "backgrounded bash renders exactly once; rows: {:?}", coll.rows);
    }

    #[test]
    fn append_body_routes_backgrounded_workflow_to_workflows_not_processes() {
        // A backgrounded local_workflow surfaces in WORKFLOWS (driven by
        // its session-scoped WorkflowEntry), never as a flat PROCESSES row.
        use crate::app::BackgroundTask;
        let mut app = App::test_default();
        app.upsert_workflow_from_tool_input("tu-wf", "nightly-audit".to_owned(), None);
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "wf1".to_owned(),
            task_type: "local_workflow".to_owned(),
            description: "nightly-audit run".to_owned(),
        }];
        let mut lines = Vec::new();
        append_body(&mut lines, &app, 60, &app.subagents_view());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("WORKFLOWS"), "workflow renders in WORKFLOWS: {text}");
        assert!(text.contains("nightly-audit"), "workflow name shows in WORKFLOWS: {text}");
        assert!(!text.contains("BACKGROUND"), "no BACKGROUND section: {text}");
        assert!(
            !text.contains("nightly-audit run"),
            "registry description not surfaced as a flat PROCESSES row: {text}"
        );
    }

    #[test]
    fn wrap_empty_returns_empty_vec() {
        assert!(wrap_text("", 10).is_empty());
        assert!(wrap_text("   ", 10).is_empty());
    }

    #[test]
    fn truncate_short_unchanged() {
        assert_eq!(truncate_with_ellipsis("hi", 10), "hi");
    }

    #[test]
    fn truncate_long_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("supercalifragilistic", 10), "supercali\u{2026}");
    }

    #[test]
    fn truncate_max_one_returns_just_ellipsis() {
        assert_eq!(truncate_with_ellipsis("anything", 1), "\u{2026}");
    }

    #[test]
    fn head_truncate_short_unchanged() {
        assert_eq!(fit_path_head_truncated("~/Projects/forge", 32), "~/Projects/forge");
    }

    #[test]
    fn head_truncate_keeps_tail_with_leading_ellipsis() {
        // Component-aware truncation lands at a `/` boundary, so the
        // result is `≤ max_chars` (not necessarily exactly equal) and
        // always starts `.../` when at least one component was dropped.
        let out = fit_path_head_truncated("~/Projects/forge/crates/forge-tui", 16);
        assert!(out.chars().count() <= 16, "got {out:?}");
        assert!(out.starts_with("\u{2026}/"), "got {out:?}");
        assert!(out.ends_with("forge-tui"), "got {out:?}");
    }

    #[test]
    fn head_truncate_drops_leading_components_first() {
        // 29-char budget - too tight for the full path, but
        // `.../src/env/git_diff.rs` (21 chars) fits cleanly at a
        // component boundary.
        let out = fit_path_head_truncated("crates/forge-agent/src/env/git_diff.rs", 29);
        assert_eq!(out, "\u{2026}/src/env/git_diff.rs");
    }

    #[test]
    fn head_truncate_basename_overflow_falls_back_to_char_cut() {
        // No `/` separators at all - has to char-cut.
        let out = fit_path_head_truncated("supercalifragilisticexpialidocious", 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.starts_with('\u{2026}'));
        assert!(out.ends_with("docious"));
    }

    #[test]
    fn head_truncate_max_one_returns_just_ellipsis() {
        assert_eq!(fit_path_head_truncated("anything", 1), "\u{2026}");
    }

    fn snap(branch: GitBranch, default: Option<&str>, in_repo: bool) -> GitDiffSnapshot {
        GitDiffSnapshot {
            branch,
            default_branch: default.map(str::to_owned),
            repo_gate: if in_repo { RepoGate::InRepo } else { RepoGate::NotARepo },
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: None,
            closes: Vec::new(),
        }
    }

    #[test]
    fn branch_row_named_default_renders_dim() {
        let s = snap(GitBranch::Named("main".into()), Some("main"), true);
        let (label, color) = branch_row_for(&s).expect("named branch should render a row");
        assert_eq!(label, "main");
        assert_eq!(color, theme::DIM);
    }

    #[test]
    fn branch_row_named_feature_renders_rust_orange() {
        let s = snap(GitBranch::Named("feat/x".into()), Some("main"), true);
        let (label, color) = branch_row_for(&s).expect("feature branch should render a row");
        assert_eq!(label, "feat/x");
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn branch_row_named_unknown_default_renders_rust_orange() {
        // `default_branch == None` means we can't prove the branch IS
        // the default, so the feature-branch styling applies.
        let s = snap(GitBranch::Named("main".into()), None, true);
        let (_label, color) = branch_row_for(&s).expect("named branch should render a row");
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn branch_row_on_main_with_origin_tracking_default_renders_dim() {
        // The default now resolves to the remote-tracking ref
        // `origin/main`; a checked-out `main` must still read as the
        // default (DIM), so the on-default check strips the remote
        // prefix before comparing to the branch name.
        let s = snap(GitBranch::Named("main".into()), Some("origin/main"), true);
        let (label, color) = branch_row_for(&s).expect("named branch should render a row");
        assert_eq!(label, "main");
        assert_eq!(color, theme::DIM);
    }

    #[test]
    fn branch_row_detached_renders_yellow() {
        let s = snap(GitBranch::Detached, Some("main"), true);
        let (label, color) = branch_row_for(&s).expect("detached HEAD should render a row");
        assert_eq!(label, "HEAD");
        assert_eq!(color, theme::STATUS_WARNING);
    }

    #[test]
    fn branch_row_no_repo_collapses_to_none() {
        let s = snap(GitBranch::NoRepo, None, false);
        assert!(branch_row_for(&s).is_none());
    }

    #[test]
    fn branch_row_unknown_collapses_to_none() {
        let s = snap(GitBranch::Unknown, None, true);
        assert!(branch_row_for(&s).is_none());
    }

    fn file(path: &str, added: u32, removed: u32) -> GitDiffFile {
        GitDiffFile { path: path.to_owned(), added, removed }
    }

    /// One row in the flattened tree shape used by the tree-builder
    /// tests: `(depth, label, file_stats)`. Aliased here to dodge
    /// clippy's `type_complexity` lint without an inline `#[allow]`.
    type FlatRow = (usize, String, Option<(u32, u32)>);

    /// Build the tree from a representative diff and walk it to a
    /// flat `(depth, label, file_stats)` list so the test reads as a
    /// structure-checking shape rather than poking at private fields.
    fn flatten(tree: &TreeNode, depth: usize, out: &mut Vec<FlatRow>) {
        if !tree.label.is_empty() {
            out.push((depth, tree.label.clone(), tree.file_stats));
        }
        for child in &tree.children {
            flatten(child, if tree.label.is_empty() { depth } else { depth + 1 }, out);
        }
    }

    #[test]
    fn build_tree_folds_single_child_directory_chains() {
        let files = vec![
            file("crates/forge-agent/src/env/git_diff.rs", 648, 0),
            file("crates/forge-agent/src/env/git.rs", 0, 559),
        ];
        let tree = build_tree(&files);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        assert_eq!(
            flat,
            vec![
                (0, "crates/forge-agent/src/env".to_owned(), None),
                (1, "git.rs".to_owned(), Some((0, 559))),
                (1, "git_diff.rs".to_owned(), Some((648, 0))),
            ]
        );
    }

    #[test]
    fn build_tree_stops_folding_at_first_split() {
        // `crates` has two dir children → don't fold.
        // `forge-tui` has one dir child (`src`) → fold into `forge-tui/src`.
        // `forge-tui/src` has two dir children (`app`, `ui`) → stop.
        let files = vec![
            file("crates/forge-agent/src/env/git_diff.rs", 648, 0),
            file("crates/forge-tui/src/app/git_diff.rs", 427, 0),
            file("crates/forge-tui/src/ui/inspector_pane.rs", 340, 21),
        ];
        let tree = build_tree(&files);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        assert_eq!(
            flat,
            vec![
                (0, "crates".to_owned(), None),
                (1, "forge-agent/src/env".to_owned(), None),
                (2, "git_diff.rs".to_owned(), Some((648, 0))),
                (1, "forge-tui/src".to_owned(), None),
                (2, "app".to_owned(), None),
                (3, "git_diff.rs".to_owned(), Some((427, 0))),
                (2, "ui".to_owned(), None),
                (3, "inspector_pane.rs".to_owned(), Some((340, 21))),
            ]
        );
    }

    #[test]
    fn build_tree_directories_sort_before_files_within_a_node() {
        let files = vec![
            file("Cargo.toml", 3, 1),
            file("crates/forge-tui/src/app.rs", 10, 2),
            file("README.md", 1, 0),
        ];
        let tree = build_tree(&files);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        // `crates/...` (folded) directory entry should come first;
        // then files in alpha order at the root level.
        assert_eq!(
            flat,
            vec![
                (0, "crates/forge-tui/src".to_owned(), None),
                (1, "app.rs".to_owned(), Some((10, 2))),
                (0, "Cargo.toml".to_owned(), Some((3, 1))),
                (0, "README.md".to_owned(), Some((1, 0))),
            ]
        );
    }

    #[test]
    fn build_tree_does_not_fold_dir_with_single_file_child() {
        // `ui` has one child but it's a file - fold rule is dir-dir
        // only, so `ui` stays as its own row above the file leaf.
        let files = vec![file("crates/forge-tui/src/ui/inspector_pane.rs", 340, 21)];
        let tree = build_tree(&files);
        let mut flat = Vec::new();
        flatten(&tree, 0, &mut flat);
        assert_eq!(
            flat,
            vec![
                // The whole chain folds because every directory has
                // a single dir child up to the leaf's parent.
                (0, "crates/forge-tui/src/ui".to_owned(), None),
                (1, "inspector_pane.rs".to_owned(), Some((340, 21))),
            ]
        );
    }

    /// Concatenate a `Line`'s span contents into a single string for
    /// assertion. The `pr_line` tests only care about textual layout
    /// (truncation, separators, issue ordering); styling assertions
    /// would be brittle and don't add coverage beyond what colour-table
    /// review provides.
    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    fn pr(number: u64) -> GitPrInfo {
        GitPrInfo { number, url: format!("https://github.com/example/repo/pull/{number}") }
    }

    fn issue(number: u64) -> GitIssueRef {
        GitIssueRef { number, url: format!("https://github.com/example/repo/issues/{number}") }
    }

    #[test]
    fn pr_line_no_closes_renders_pr_only() {
        let line = pr_line(40, &pr(1234), &[]);
        assert_eq!(line_text(&line), "    PR #1234");
    }

    #[test]
    fn pr_line_with_single_issue_renders_arrow_and_number() {
        let line = pr_line(40, &pr(1234), &[issue(1230)]);
        assert_eq!(line_text(&line), "    PR #1234 \u{2192} closes #1230");
    }

    #[test]
    fn pr_line_with_multiple_issues_renders_all_when_width_allows() {
        let line = pr_line(40, &pr(1234), &[issue(1230), issue(1228)]);
        assert_eq!(line_text(&line), "    PR #1234 \u{2192} closes #1230 #1228");
    }

    #[test]
    fn pr_line_truncates_overflowing_issue_list_with_ellipsis() {
        let closes: Vec<_> = (100..110).map(issue).collect();
        let line = pr_line(40, &pr(1234), &closes);
        let text = line_text(&line);
        // Must start with the standard PR prefix and end with the
        // ellipsis truncation marker.
        assert!(text.starts_with("    PR #1234 \u{2192} closes "), "unexpected prefix: {text:?}");
        assert!(text.ends_with(" \u{2026}"), "expected ellipsis suffix: {text:?}");
        // The visible row width must respect the pane budget (40
        // cols minus PANE_PAD's right gutter).
        let visible_chars = text.chars().count();
        assert!(visible_chars <= 38, "row overflows 40-col pane: {text:?} ({visible_chars} cols)");
    }

    #[test]
    fn pr_line_collapses_to_ellipsis_when_no_issue_fits() {
        // Pane narrower than even one issue number can fit alongside
        // the chrome; the closes tail should collapse to a bare `...`.
        let line = pr_line(20, &pr(9999), &[issue(987_654)]);
        let text = line_text(&line);
        assert!(text.ends_with("\u{2192} \u{2026}"), "expected collapse: {text:?}");
    }

    #[test]
    fn count_digits_handles_edges() {
        assert_eq!(count_digits(0), 1);
        assert_eq!(count_digits(9), 1);
        assert_eq!(count_digits(10), 2);
        assert_eq!(count_digits(99), 2);
        assert_eq!(count_digits(100), 3);
        assert_eq!(count_digits(9_999), 4);
        assert_eq!(count_digits(u64::MAX), 20);
    }

    /// Wrap test rows in a `ProcessCollection` so the existing tests
    /// stay readable. Memory rendering needs an explicit
    /// `memory_bytes`; tests opt in via [`make_process_row_with_memory`].
    fn collection(rows: Vec<ProcessRow>) -> ProcessCollection {
        ProcessCollection { rows }
    }

    #[test]
    fn processes_section_renders_bash_supervisor_as_single_line() {
        // Supervisor rows are single-line: glyph + headline + ` · <memory>`.
        // The verbose `Kind · running` metadata + cmdline continuation
        // rows were retired - the glyph + colour convey kind and the
        // memory suffix is the only useful per-row signal.
        let row = make_row_with_memory(
            ProcessKind::BashBackgrounded,
            "Run unit tests",
            "Bash · running",
            8 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 40, '\u{280B}');

        // header + blank + single supervisor row = 3 lines.
        assert_eq!(lines.len(), 3, "expected 3 rendered lines, got {}", lines.len());

        let header = line_text(&lines[0]);
        assert_eq!(header, " PROCESSES");
        let row_text = line_text(&lines[2]);
        // Wire-matched (Bash) → spinner glyph instead of static `▸`.
        // Frame 0 picks `⠋` (the first braille spinner frame).
        assert!(row_text.starts_with(" \u{280B} Run unit tests"), "headline: {row_text:?}");
        assert!(row_text.contains("8 MB"), "memory suffix: {row_text:?}");
        // No `Bash · running` text - that's the regression check.
        assert!(!row_text.contains("running"), "kind/running text must be dropped: {row_text:?}");
    }

    #[test]
    fn processes_section_renders_failed_with_cross_glyph() {
        let row = make_row_with_memory(
            ProcessKind::BashBackgrounded,
            "Run integration tests",
            "Bash · failed",
            16 * 1024 * 1024,
        );
        // Override status from the helper default.
        let mut rows = vec![row];
        rows[0].status = ToolCallStatus::Failed;
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(rows), 40, '\u{280B}');

        let row_text = line_text(&lines[2]);
        assert!(row_text.starts_with(" \u{2717} Run integration tests"), "got {row_text:?}");
    }

    #[test]
    fn processes_section_two_bash_rows_render_blank_between() {
        // Two same-kind Bash rows still render with a separating blank
        // (the per-entry rendering shape is independent of which kinds
        // are present). Cron used to be the second-kind partner here;
        // Inspector SCHEDULES owns crons now, so the pair below
        // exercises the more common "two backgrounded shells" case.
        let rows = vec![
            make_row_with_memory(
                ProcessKind::BashBackgrounded,
                "Run tests",
                "Bash · running",
                8 * 1024 * 1024,
            ),
            make_row_with_memory(
                ProcessKind::BashBackgrounded,
                "Run lints",
                "Bash · running",
                4 * 1024 * 1024,
            ),
        ];
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(rows), 40, '\u{280B}');

        // header + blank + 2 single-line rows + 1 blank between = 5 lines.
        assert_eq!(lines.len(), 5, "expected 5 rendered lines, got {}", lines.len());
        assert!(line_text(&lines[2]).contains("Run tests"));
        assert!(line_text(&lines[4]).contains("Run lints"));
    }

    #[test]
    fn processes_section_truncates_long_headline_with_ellipsis() {
        let row = make_row_with_memory(
            ProcessKind::BashBackgrounded,
            "Run a very long described task that will absolutely overflow the pane width",
            "Bash · running",
            8 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 40, '\u{280B}');

        let row_text = line_text(&lines[2]);
        assert!(row_text.contains('\u{2026}'), "expected ellipsis: {row_text:?}");
        let visible_chars = row_text.chars().count();
        assert!(visible_chars <= 39, "row overflows 40-col pane: {visible_chars} cols");
    }

    /// Helper for tests that need a row with explicit memory bytes
    /// so the Wide-tier suffix path can be exercised.
    fn make_row_with_memory(
        kind: ProcessKind,
        headline: &str,
        metadata: &str,
        memory_bytes: u64,
    ) -> ProcessRow {
        ProcessRow {
            kind,
            headline: headline.to_owned(),
            detail: None,
            metadata: metadata.to_owned(),
            status: ToolCallStatus::InProgress,
            memory_bytes: Some(memory_bytes),
            depth: 0,
            is_last_sibling: true,
            ancestor_has_more: Vec::new(),
        }
    }

    #[test]
    fn processes_section_appends_memory_suffix_at_wide_width() {
        // 40-col Wide-tier inspector - width above the threshold so
        // the row carries a `· 12 MB` suffix inline with the headline.
        let row = make_row_with_memory(
            ProcessKind::Process,
            "cargo",
            "Process · running",
            12 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 40, '\u{280B}');

        let row_text = line_text(&lines[2]);
        assert!(row_text.contains("12 MB"), "expected memory suffix on Wide tier: {row_text:?}");
    }

    #[test]
    fn processes_section_drops_memory_suffix_at_medium_width() {
        // 30-col Medium-tier inspector - width below threshold so
        // the row stays bare (no memory suffix).
        let row = make_row_with_memory(
            ProcessKind::Process,
            "cargo",
            "Process · running",
            12 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 30, '\u{280B}');

        let row_text = line_text(&lines[2]);
        assert!(!row_text.contains("MB"), "expected no memory suffix on Medium tier: {row_text:?}");
    }

    // ---------------------------------------------------------
    // WORKFLOWS Inspector section.
    // ---------------------------------------------------------

    fn make_workflow_entry(
        tool_use_id: &str,
        meta_name: &str,
        status: crate::app::WorkflowStatus,
    ) -> crate::app::WorkflowEntry {
        crate::app::WorkflowEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some("task_1".to_owned()),
            meta_name: meta_name.to_owned(),
            meta_description: None,
            phases: Vec::new(),
            status,
            final_result_summary: None,
            expanded_in_inspector: false,
        }
    }

    #[test]
    fn workflows_section_renders_in_progress_header_with_phase_tree() {
        let mut workflow =
            make_workflow_entry("tu", "minimal-ping", crate::app::WorkflowStatus::InProgress);
        workflow.phases = vec![crate::app::PhaseEntry {
            index: 1,
            title: "Ping".to_owned(),
            status: crate::app::PhaseStatus::InProgress,
            logs: std::collections::VecDeque::from(["running StructuredOutput".to_owned()]),
        }];
        let mut lines = Vec::new();
        append_workflow_row(&mut lines, &workflow, 60, '\u{280B}');
        assert!(lines.iter().any(|l| line_text(l).contains("minimal-ping")));
        assert!(
            lines
                .iter()
                .any(|l| line_text(l).contains("Ping") && line_text(l).contains("\u{251c}")
                    || line_text(l).contains("Ping") && line_text(l).contains("\u{2514}")),
            "expected phase row glyph; got {:?}",
            lines.iter().map(line_text).collect::<Vec<_>>(),
        );
        assert!(lines.iter().any(|l| line_text(l).contains("running StructuredOutput")));
    }

    #[test]
    fn workflows_section_collapses_completed_to_header_only_with_summary() {
        let mut workflow = make_workflow_entry("tu", "ping", crate::app::WorkflowStatus::Completed);
        workflow.phases = vec![crate::app::PhaseEntry {
            index: 1,
            title: "Ping".to_owned(),
            status: crate::app::PhaseStatus::Completed,
            logs: std::collections::VecDeque::new(),
        }];
        workflow.final_result_summary = Some("{\"answer\":\"pong\"}".to_owned());
        let mut lines = Vec::new();
        append_workflow_row(&mut lines, &workflow, 60, '\u{280B}');
        // Collapsed completed entry: header + summary line only;
        // phase tree suppressed because `expanded_in_inspector =
        // false` and `is_in_progress() = false`.
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains("ping") && line_text(&lines[0]).contains("done"));
        assert!(line_text(&lines[1]).contains("{\"answer\":\"pong\"}"));
    }

    #[test]
    fn workflows_section_shows_phase_tree_when_expanded_after_completion() {
        let mut workflow = make_workflow_entry("tu", "ping", crate::app::WorkflowStatus::Completed);
        workflow.phases = vec![crate::app::PhaseEntry {
            index: 1,
            title: "Ping".to_owned(),
            status: crate::app::PhaseStatus::Completed,
            logs: std::collections::VecDeque::new(),
        }];
        workflow.expanded_in_inspector = true;
        let mut lines = Vec::new();
        append_workflow_row(&mut lines, &workflow, 60, '\u{280B}');
        // Expanded → header + phase tree row.
        assert!(
            lines
                .iter()
                .any(|l| line_text(l).contains("Ping") && line_text(l).contains("\u{2514}")),
            "expected phase row in expanded view; got {:?}",
            lines.iter().map(line_text).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn workflows_section_renders_meta_description_as_dim_subtitle() {
        let mut workflow =
            make_workflow_entry("tu", "minimal-ping", crate::app::WorkflowStatus::InProgress);
        workflow.meta_description = Some("sanity".to_owned());
        let mut lines = Vec::new();
        append_workflow_row(&mut lines, &workflow, 60, '\u{280B}');
        assert!(lines.iter().any(|l| line_text(l).contains("sanity")));
    }

    #[test]
    fn truncate_or_pass_returns_input_when_under_budget() {
        assert_eq!(truncate_or_pass("abc", 10), "abc");
    }

    #[test]
    fn truncate_or_pass_appends_ellipsis_when_over_budget() {
        let out = truncate_or_pass("abcdefghij", 5);
        // 4 chars + 1 ellipsis = 5 columns; not the full input.
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('\u{2026}'));
        assert!(out.starts_with("abcd"));
    }

    #[test]
    fn processes_section_process_kind_uses_dim_glyph() {
        // OS-only `Process` rows render with a DIM ▸ glyph (vs the
        // RUST_ORANGE ▸ for wire-tracked Bash / Monitor) so the eye
        // separates "claude-described work" from "anonymous OS
        // process" at a glance.
        let row = make_row_with_memory(
            ProcessKind::Process,
            "cargo",
            "Process · running",
            8 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 40, '\u{280B}');

        let headline = line_text(&lines[2]);
        // Unmatched `Process` kind also renders the spinner glyph
        // (frame 0 = `⠋`) but in DIM - the colour difference vs
        // RUST_ORANGE matched rows is the "is this tracked or not"
        // signal.
        assert!(headline.starts_with(" \u{280B} cargo"), "expected spinner glyph: {headline:?}");
        // Style assertion: pull the glyph span and check its colour.
        let glyph_span = &lines[2].spans[1];
        assert_eq!(glyph_span.style.fg, Some(theme::DIM));
    }

    // ---------------------------------------------------------
    // architectural gutter-consistency
    // contract. Every inspector row variant must produce a
    // total rendered width <= inner_width. The 1-col right
    // gutter is what makes the section read cleanly against
    // the pane border; this test is the architectural backstop
    // that prevents a future row variant from silently
    // reintroducing the divergence Bug 4 exposed.
    // ---------------------------------------------------------

    fn rendered_width(line: &Line<'_>) -> usize {
        // Sum the display-column width of every span on the line.
        // Box-drawing + spinner chars are all 1-col; ASCII is 1-col;
        // we use UnicodeWidthStr to stay correct for any future
        // wider glyph that lands in chrome.
        use unicode_width::UnicodeWidthStr;
        line.spans.iter().map(|span| UnicodeWidthStr::width(span.content.as_ref())).sum()
    }

    #[test]
    fn row_text_budget_subtracts_chrome() {
        assert_eq!(row_text_budget(40, 4), 36);
        assert_eq!(row_text_budget(40, 39), 1);
        // Pathologically narrow: helper bounds at 1 so callers
        // always get a usable budget for `truncate_with_ellipsis`.
        assert_eq!(row_text_budget(2, 10), 1);
        assert_eq!(row_text_budget(0, 0), 1);
    }

    #[test]
    fn every_inspector_row_variant_fits_within_inner_width() {
        // Force-construct one row per section variant at a fixed
        // inner_width with content long enough to trigger
        // truncation. Then assert: every rendered row's display
        // width is <= inner_width (1-col right gutter preserved
        // because the budget helper accounts for it).
        let inner_width: usize = 60;
        let mut all_rows: Vec<Line<'static>> = Vec::new();

        // WORKFLOWS header + phase rows.
        let mut workflow = make_workflow_entry(
            "wf",
            "minimal-ping-with-a-rather-long-name-for-overflow-coverage",
            crate::app::WorkflowStatus::InProgress,
        );
        workflow.meta_description = Some(
            "An overlong description that should be truncated cleanly against the inner_width budget"
                .to_owned(),
        );
        workflow.phases = vec![crate::app::PhaseEntry {
            index: 1,
            title: "A phase with a long title that would otherwise overflow".to_owned(),
            status: crate::app::PhaseStatus::InProgress,
            logs: std::collections::VecDeque::from([
                "an extra-long log line that should also stay within the gutter contract"
                    .to_owned(),
            ]),
        }];
        let mut wf_lines = Vec::new();
        append_workflow_row(&mut wf_lines, &workflow, inner_width, '\u{280B}');
        all_rows.extend(wf_lines);

        // GOTIFY subscription: a long app set forcing the comma-joined
        // list to wrap, each wrapped line within the gutter budget.
        let mut gotify_lines = Vec::new();
        append_gotify_subscription(
            &mut gotify_lines,
            &forge_primitives::GotifySubscription {
                id: uuid::Uuid::from_u128(9),
                project: "p".to_owned(),
                team_role: None,
                applications: vec![
                    "an-extra-long-application-name-alpha".to_owned(),
                    "an-extra-long-application-name-beta".to_owned(),
                    "gamma".to_owned(),
                ],
                min_priority: Some(9),
                created_at: std::time::SystemTime::UNIX_EPOCH,
            },
            inner_width,
        );
        all_rows.extend(gotify_lines);

        for (i, line) in all_rows.iter().enumerate() {
            let w = rendered_width(line);
            assert!(
                w <= inner_width,
                "row #{i} exceeded inner_width ({w} > {inner_width}): {}",
                line_text(line),
            );
            // At least 1-col right gutter (matching TASKS' contract).
            assert!(
                w <= inner_width.saturating_sub(1) || w == 0,
                "row #{i} consumed the right gutter ({w} > {} = inner_width - 1): {}",
                inner_width - 1,
                line_text(line),
            );
        }
    }

    // ---------------------------------------------------------
    // #281: WORKFLOWS status-badge right-justify.
    // Trailing badge end column locks at `inner_width - PANE_PAD`
    // regardless of headline length. Mirrors GIT's
    // `diff_subtitle_line` pad-spacer pattern. (MONITORS variants
    // retired with the section's removal; the badge math itself
    // is exercised by the WORKFLOWS test below.)
    // ---------------------------------------------------------

    #[test]
    fn workflow_row_status_badge_right_justified_across_title_lengths() {
        let inner_width: usize = 38;
        let short = make_workflow_entry("wf_a", "ping", crate::app::WorkflowStatus::InProgress);
        let long = make_workflow_entry(
            "wf_b",
            "a-rather-long-workflow-name-that-needs-truncation",
            crate::app::WorkflowStatus::InProgress,
        );

        let mut short_lines = Vec::new();
        append_workflow_row(&mut short_lines, &short, inner_width, '\u{280B}');
        let mut long_lines = Vec::new();
        append_workflow_row(&mut long_lines, &long, inner_width, '\u{280B}');

        let short_w = rendered_width(&short_lines[0]);
        let long_w = rendered_width(&long_lines[0]);
        let target = inner_width.saturating_sub(usize::from(PANE_PAD));
        assert_eq!(
            short_w, target,
            "short WORKFLOWS row should pad out to inner_width - PANE_PAD; got {short_w}, want {target}",
        );
        assert_eq!(
            long_w, target,
            "long WORKFLOWS row should also end at inner_width - PANE_PAD; got {long_w}, want {target}",
        );
    }

    // ---------------------------------------------------------
    // SUBAGENTS section: live tail + terminal `· N tools` summary.
    // ---------------------------------------------------------

    fn subagents_test_app() -> App {
        use crate::agent::model::ToolCallStatus;
        use crate::app::{ChatMessage, MessageBlock, MessageRole, ToolCallScope};

        let mut app = App::test_default();
        // Active subagent: Task "Explore" with three SubagentChild
        // tool calls. The children are hidden in chat but should
        // surface as the live tail in the SUBAGENTS section.
        let mut blocks: Vec<MessageBlock> = Vec::new();
        let root_id = "tu-explore-root";
        app.register_tool_call_scope(root_id.to_owned(), ToolCallScope::SubagentRoot);
        let root = subagent_test_root_info(root_id, "Explore", "map hidden tool calls");
        blocks.push(MessageBlock::ToolCall(Box::new(root)));
        for (id, kind, title) in [
            ("tu-c-grep", "Grep", "SubagentChild"),
            ("tu-c-read", "Read", "inspector_pane.rs"),
            ("tu-c-bash", "Bash", "git log --oneline -3"),
        ] {
            app.register_tool_call_scope(
                id.to_owned(),
                ToolCallScope::SubagentChild { parent_tool_use_id: root_id.to_owned() },
            );
            blocks
                .push(MessageBlock::ToolCall(Box::new(subagent_test_child_info(id, kind, title))));
        }
        // Terminal subagent: code-reviewer that already finished
        // with a bunch of children. Should render the trailing
        // `· N tools` summary on the header (no tail rows).
        let done_id = "tu-review-root";
        app.register_tool_call_scope(done_id.to_owned(), ToolCallScope::SubagentRoot);
        let mut done = subagent_test_root_info(done_id, "code-reviewer", "review the diff");
        done.status = ToolCallStatus::Completed;
        blocks.push(MessageBlock::ToolCall(Box::new(done)));
        for i in 0..12_u32 {
            let id = format!("tu-review-c-{i}");
            app.register_tool_call_scope(
                id.clone(),
                ToolCallScope::SubagentChild { parent_tool_use_id: done_id.to_owned() },
            );
            blocks.push(MessageBlock::ToolCall(Box::new(subagent_test_child_info(
                &id,
                "Read",
                &format!("file-{i}.rs"),
            ))));
        }
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, blocks));
        app
    }

    fn subagent_test_root_info(
        id: &str,
        subagent_type: &str,
        description: &str,
    ) -> crate::app::ToolCallInfo {
        use crate::agent::model::ToolCallStatus;
        use crate::app::{BlockCache, TerminalSnapshotMode, ToolCallInfo};
        ToolCallInfo {
            id: id.to_owned(),
            title: "Task".to_owned(),
            sdk_tool_name: "Task".to_owned(),
            raw_input: Some(serde_json::json!({
                "subagent_type": subagent_type,
                "description": description,
                "prompt": description,
            })),
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_command: None,
            terminal_output: None,
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    fn subagent_test_child_info(
        id: &str,
        sdk_tool_name: &str,
        title: &str,
    ) -> crate::app::ToolCallInfo {
        use crate::agent::model::ToolCallStatus;
        use crate::app::{BlockCache, TerminalSnapshotMode, ToolCallInfo};
        ToolCallInfo {
            id: id.to_owned(),
            title: title.to_owned(),
            sdk_tool_name: sdk_tool_name.to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::Completed,
            content: Vec::new(),
            hidden: true,
            terminal_id: None,
            terminal_command: None,
            terminal_output: None,
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    #[test]
    fn subagents_section_renders_header_running_tail_and_done_summary() {
        let app = subagents_test_app();
        let mut lines = Vec::new();
        append_subagents_section(&mut lines, &app, 60, &app.subagents_view());

        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains(" SUBAGENTS"), "section header must be present; got:\n{joined}");
        // Running root: header + 3 tail rows.
        assert!(
            joined.contains("Explore \u{b7} map hidden tool calls"),
            "running root header must render the combined label; got:\n{joined}",
        );
        for kind_label in ["Grep", "Read", "Bash"] {
            assert!(
                joined.contains(kind_label),
                "live tail must surface {kind_label} kind label; got:\n{joined}",
            );
        }
        assert!(
            joined.contains("inspector_pane.rs"),
            "live tail must carry the child title; got:\n{joined}",
        );
        // Terminal root: header + trailing `· 12 tools`. No tail rows.
        assert!(
            joined.contains("code-reviewer \u{b7} review the diff"),
            "terminal root header must render the combined label; got:\n{joined}",
        );
        assert!(
            joined.contains("12 tools"),
            "terminal root must render the `· N tools` summary; got:\n{joined}",
        );
    }

    #[test]
    fn append_subagent_row_tail_gated_on_field_not_status() {
        // Contract guard for the derive/render decoupling: the derive owns
        // "is this root running", the render just draws the tail it's
        // handed. A terminal-status entry carrying a tail is not a state the
        // derive produces - it exists here only to prove the render keys the
        // tail on `entry.tail`, so a re-introduced status-based early-return
        // (which would drop this tail) fails loudly.
        use crate::agent::model::ToolCallStatus;
        use crate::app::{SubagentChildEntry, SubagentEntry};

        let entry = SubagentEntry {
            tool_use_id: "tu-root".to_owned(),
            label: "Explore".to_owned(),
            status: ToolCallStatus::Completed,
            tail: vec![SubagentChildEntry {
                sdk_tool_name: "Read".to_owned(),
                title: "probe.rs".to_owned(),
                status: ToolCallStatus::Completed,
            }],
            total_count: 5,
        };
        let mut lines = Vec::new();
        append_subagent_row(&mut lines, &entry, 60, '\u{2022}');
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("probe.rs"),
            "render must draw the derived tail regardless of terminal status; got:\n{joined}",
        );
        // Companion: the summary path is status-gated (terminal -> `· N
        // tools`), independent of the field-gated tail above.
        assert!(
            joined.contains("5 tools"),
            "terminal status still drives the `· N tools` summary; got:\n{joined}",
        );
    }

    #[test]
    fn append_subagent_row_pending_renders_queued_header_only() {
        // Pending render contract: a queued root shows the `○` glyph, no
        // `· N tools` summary, and no tail rows - the one render branch the
        // section cluster otherwise never exercises. Guards a future
        // narrowing of the summary's `in_progress` predicate that would
        // leak a spurious `· 0 tools` onto a queued root.
        use crate::agent::model::ToolCallStatus;
        use crate::app::SubagentEntry;

        let entry = SubagentEntry {
            tool_use_id: "tu-root".to_owned(),
            label: "Explore".to_owned(),
            status: ToolCallStatus::Pending,
            tail: Vec::new(),
            total_count: 0,
        };
        let mut lines = Vec::new();
        append_subagent_row(&mut lines, &entry, 60, '\u{2022}');
        assert_eq!(lines.len(), 1, "queued root renders a single header line; got {lines:?}");
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains('\u{25cb}'), "queued root shows the ○ glyph; got:\n{joined}");
        assert!(
            !joined.contains("tools"),
            "queued root shows no `· N tools` summary; got:\n{joined}",
        );
    }

    #[test]
    fn subagents_section_hidden_when_view_is_empty() {
        let app = App::test_default();
        let mut lines = Vec::new();
        append_subagents_section(&mut lines, &app, 60, &app.subagents_view());
        assert!(lines.is_empty(), "empty view -> section emits zero lines; got {lines:?}");
    }

    #[test]
    fn mcp_servers_section_renders_every_server_including_the_process_free_sdk_one() {
        // The regression this section closes: an sdk/in-process server has
        // no pid for the OS walk to find, so the old PROCESSES tier
        // silently dropped it. Snapshot-sourced means it renders with no
        // process snapshot at all, next to a pending server that has no
        // handshake either.
        use forge_primitives::McpToolInfo;
        let mut app = App::test_default();
        app.mcp_mut().servers = vec![
            forge_primitives::McpServerStatus {
                name: "forge".to_owned(),
                status: forge_primitives::McpServerConnectionStatus::Connected,
                config: Some(serde_json::json!({ "type": "sdk", "name": "forge" })),
                tools: Some(
                    (0..18)
                        .map(|i| McpToolInfo {
                            name: format!("t{i}"),
                            description: None,
                            annotations: None,
                        })
                        .collect(),
                ),
                ..Default::default()
            },
            forge_primitives::McpServerStatus {
                name: "greptile".to_owned(),
                status: forge_primitives::McpServerConnectionStatus::Pending,
                scope: Some("user".to_owned()),
                ..Default::default()
            },
        ];

        let mut lines = Vec::new();
        append_body(&mut lines, &app, 40, &[]);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        let forge_idx = texts.iter().position(|t| t.contains("forge")).expect("forge row renders");
        assert_eq!(texts[forge_idx], " \u{251C}\u{2500} forge \u{25CF}", "name + glyph line");
        assert_eq!(
            texts[forge_idx + 1],
            " \u{2502}   \u{2514}\u{2500} sdk \u{00B7} 18 tools",
            "sdk scope with the tool count, no process line",
        );

        let greptile_idx =
            texts.iter().position(|t| t.contains("greptile")).expect("pending server renders");
        assert_eq!(texts[greptile_idx], " \u{2514}\u{2500} greptile \u{25CC}", "pending glyph");
        assert_eq!(
            texts[greptile_idx + 1],
            "     \u{2514}\u{2500} user \u{00B7} pending",
            "the word pending rides the detail line",
        );

        assert!(
            texts.iter().any(|t| t.trim().starts_with("MCP SERVERS") && t.contains('▦')),
            "section header renders with the open-view affordance: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.trim().starts_with("PROCESSES")),
            "no processes, no section: {texts:?}"
        );
    }

    #[test]
    fn mcp_servers_failed_row_carries_the_failure_reason() {
        // Wire shape from the live mcp_status baseline: a failed server
        // with the CLI's error text. Wide enough that the reason fits;
        // narrower panes truncate it like every other pane cell.
        let mut app = App::test_default();
        app.mcp_mut().servers = vec![forge_primitives::McpServerStatus {
            name: "jetbrains".to_owned(),
            status: forge_primitives::McpServerConnectionStatus::Failed,
            error: Some("SSE error: Non-200 status code (502)".to_owned()),
            scope: Some("project".to_owned()),
            ..Default::default()
        }];

        let mut lines = Vec::new();
        append_body(&mut lines, &app, 60, &[]);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let idx =
            texts.iter().position(|t| t.contains("jetbrains")).expect("failed server renders");
        assert_eq!(texts[idx], " \u{2514}\u{2500} jetbrains \u{2717}", "failed glyph");
        assert_eq!(
            texts[idx + 1],
            "     \u{2514}\u{2500} project \u{00B7} SSE error: Non-200 status code (502)",
            "the failure reason rides the detail line",
        );
    }

    #[test]
    fn mcp_servers_subprocess_backed_server_renders_the_process_line() {
        // Third line only for a server the OS walk joined: cmd + subtree
        // memory + pid, exactly as the old PROCESSES tree rendered the
        // backing process under a matched server.
        use forge_workspace::env::processes::{ProcessEntry, ProcessSnapshot};
        let mut app = App::test_default();
        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                ProcessEntry {
                    pid: 300,
                    parent_pid: 1,
                    name: "npm".to_owned(),
                    command: "npm exec @upstash/context7-mcp".to_owned(),
                    memory_bytes: 81 * 1024 * 1024,
                },
                ProcessEntry {
                    pid: 301,
                    parent_pid: 300,
                    name: "node".to_owned(),
                    command: "node /x/.bin/context7-mcp".to_owned(),
                    memory_bytes: 81 * 1024 * 1024,
                },
                // A non-MCP process so PROCESSES renders below.
                ProcessEntry {
                    pid: 400,
                    parent_pid: 1,
                    name: "cargo".to_owned(),
                    command: "cargo build".to_owned(),
                    memory_bytes: 20 * 1024 * 1024,
                },
            ],
        });
        app.mcp_mut().servers = vec![forge_primitives::McpServerStatus {
            name: "context7".to_owned(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            scope: Some("user".to_owned()),
            config: Some(serde_json::json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@upstash/context7-mcp"],
            })),
            tools: Some(
                (0..2)
                    .map(|i| forge_primitives::McpToolInfo {
                        name: format!("t{i}"),
                        description: None,
                        annotations: None,
                    })
                    .collect(),
            ),
            ..Default::default()
        }];

        let mut lines = Vec::new();
        append_body(&mut lines, &app, 40, &[]);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let idx = texts.iter().position(|t| t.contains("context7")).expect("server row renders");
        assert_eq!(texts[idx], " \u{2514}\u{2500} context7 \u{25CF}");
        assert_eq!(texts[idx + 1], "     \u{251C}\u{2500} user \u{00B7} 2 tools");
        assert_eq!(
            texts[idx + 2],
            "     \u{2514}\u{2500} npm exec @upsta\u{2026} \u{00B7} 162 MB \u{00B7} 300",
            "process line carries subtree memory + pid; the command truncates to make room",
        );
        // The joined process left PROCESSES with it.
        let processes_hdr =
            texts.iter().position(|t| t.trim() == "PROCESSES").expect("PROCESSES renders");
        assert!(
            texts[processes_hdr..].iter().all(|t| !t.contains("context7")),
            "the server's process must not also render in PROCESSES: {:?}",
            &texts[processes_hdr..],
        );
    }

    #[test]
    fn non_mcp_processes_stay_in_processes_below_the_mcp_section() {
        use forge_workspace::env::processes::{ProcessEntry, ProcessSnapshot};
        let mut app = App::test_default();
        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: vec![
                ProcessEntry {
                    pid: 100,
                    parent_pid: 1,
                    name: "npm".to_owned(),
                    command: "npm exec @upstash/context7-mcp".to_owned(),
                    memory_bytes: 200 * 1024 * 1024,
                },
                ProcessEntry {
                    pid: 200,
                    parent_pid: 1,
                    name: "cargo".to_owned(),
                    command: "cargo build".to_owned(),
                    memory_bytes: 20 * 1024 * 1024,
                },
            ],
        });
        app.mcp_mut().servers = vec![forge_primitives::McpServerStatus {
            name: "context7".to_owned(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            config: Some(serde_json::json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@upstash/context7-mcp"],
            })),
            ..Default::default()
        }];

        let mut lines = Vec::new();
        append_body(&mut lines, &app, 40, &[]);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let mcp_hdr = texts
            .iter()
            .position(|t| t.trim().starts_with("MCP SERVERS"))
            .expect("MCP section renders");
        let processes_hdr = texts
            .iter()
            .position(|t| t.trim().starts_with("PROCESSES"))
            .expect("PROCESSES renders");
        assert!(mcp_hdr < processes_hdr, "MCP SERVERS sits above PROCESSES");
        let cargo_idx =
            texts.iter().position(|t| t.contains("cargo build")).expect("generic row stays");
        assert!(cargo_idx > processes_hdr, "the non-MCP process renders under PROCESSES");
        // Row shape pinned to its full extent - pad + spinner glyph + space +
        // headline + memory, the same exactness the MCP rows assert. The
        // spinner frame is the one char the test does not control, so the
        // anchor is ends_with + length rather than a bare contains.
        let cargo_row = &texts[cargo_idx];
        let tail = "cargo build \u{00B7} 20 MB";
        assert!(
            cargo_row.ends_with(tail) && cargo_row.chars().count() == tail.chars().count() + 3,
            "generic row renders as glyph + headline + memory; got {cargo_row:?}"
        );
        let processes_text: String = texts[processes_hdr..].iter().map(String::as_str).collect();
        assert!(
            !processes_text.contains("context7"),
            "the MCP server's process must leave PROCESSES: {processes_text:?}"
        );
    }

    #[test]
    fn mcp_click_band_clips_and_disappears_with_scroll() {
        // The band is the section's on-screen rect: clipped at either
        // edge when the section scrolls partially off, gone when it
        // scrolls fully off. render_scrollable_body runs directly so the
        // body rect is the test's own, not the layout's.
        use forge_workspace::env::processes::{ProcessEntry, ProcessSnapshot};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::test_default();
        app.mcp_mut().servers = vec![forge_primitives::McpServerStatus {
            name: "forge".to_owned(),
            status: forge_primitives::McpServerConnectionStatus::Connected,
            config: Some(serde_json::json!({ "type": "sdk", "name": "forge" })),
            ..Default::default()
        }];
        // Content below the section so the body can scroll it fully off.
        app.set_active_process_snapshot_for_test(ProcessSnapshot {
            scanned_at: std::time::SystemTime::now(),
            processes: (0..12u32)
                .map(|i| ProcessEntry {
                    pid: 100 + i,
                    parent_pid: 1,
                    name: "worker".to_owned(),
                    command: format!("worker{i} --serve"),
                    memory_bytes: 10 * 1024 * 1024,
                })
                .collect(),
        });

        let band = |app: &crate::app::App| {
            app.pane_hit_targets.iter().find_map(|t| match t {
                crate::app::PaneHitTarget::InspectorMcpOpenStatus { y, height, .. } => {
                    Some((*y, *height))
                }
                _ => None,
            })
        };
        let set_offset = |app: &mut App, offset: u16| {
            if let Some(bucket) = app.try_active_bucket_mut() {
                bucket.inspector_scroll_offset = offset;
            }
        };

        // Offset 0 over a tall body: the band covers the whole section.
        // render_scrollable_body stamps without clearing (the full
        // `render` entry owns the clear), so each draw clears first.
        let mut terminal = Terminal::new(TestBackend::new(40, 60)).expect("terminal");
        app.pane_hit_targets.clear();
        terminal
            .draw(|f| render_scrollable_body(f, Rect::new(0, 0, 40, 60), &mut app, &[]))
            .expect("draw");
        let (start, full_h) = band(&app).expect("band stamped at offset 0");
        assert!(full_h > 1, "band covers header + rows; got {full_h}");

        // Bottom edge: a body two rows too short clips the band there.
        let short_h = start + full_h - 2;
        let mut terminal = Terminal::new(TestBackend::new(40, short_h)).expect("terminal");
        set_offset(&mut app, 0);
        app.pane_hit_targets.clear();
        terminal
            .draw(|f| render_scrollable_body(f, Rect::new(0, 0, 40, short_h), &mut app, &[]))
            .expect("draw");
        assert_eq!(
            band(&app),
            Some((start, short_h - start)),
            "band clips at the pane's bottom edge",
        );

        // Top edge: one row past the section top clips the band there.
        set_offset(&mut app, start + 1);
        app.pane_hit_targets.clear();
        terminal
            .draw(|f| render_scrollable_body(f, Rect::new(0, 0, 40, short_h), &mut app, &[]))
            .expect("draw");
        assert_eq!(band(&app), Some((0, full_h - 1)), "band clips at the viewport's top edge");

        // Fully scrolled off: enough content below for the scroll range
        // to move the section past the viewport - nothing stamps.
        set_offset(&mut app, start + full_h);
        app.pane_hit_targets.clear();
        terminal
            .draw(|f| render_scrollable_body(f, Rect::new(0, 0, 40, short_h), &mut app, &[]))
            .expect("draw");
        assert!(band(&app).is_none(), "a fully scrolled-off section stamps nothing");
    }

    #[test]
    fn mcp_server_rows_do_not_wrap_at_40_cols() {
        // The name+glyph line is the scan target; the design drops the
        // version token so it never wraps at the real inspector width.
        use crate::app::mcp_servers::{McpProcessLine, McpServerRow};
        let rows = vec![
            McpServerRow {
                name: "playwright-local".to_owned(),
                status: forge_primitives::McpServerConnectionStatus::Connected,
                detail: "user \u{00B7} 24 tools".to_owned(),
                process: Some(McpProcessLine {
                    command: "npm exec @playwright/mcp@latest --cdp-endpoint".to_owned(),
                    memory_bytes: 184 * 1024 * 1024,
                    pid: 90585,
                }),
            },
            McpServerRow {
                name: "a-very-long-server-name-from-a-plugin-namespace".to_owned(),
                status: forge_primitives::McpServerConnectionStatus::Failed,
                detail: "user \u{00B7} SSE error: Non-200 status code (502) with extra words"
                    .to_owned(),
                process: None,
            },
        ];
        let mut lines = Vec::new();
        append_mcp_servers_section(&mut lines, &rows, 40);
        for line in &lines {
            let text = line_text(line);
            assert!(
                text.chars().count() <= 40,
                "every row fits the pane without wrapping; got {} cols in {text:?}",
                text.chars().count(),
            );
        }
        let name_line =
            lines.iter().find(|l| line_text(l).contains("playwright-local")).expect("name row");
        let name_row = line_text(name_line);
        assert!(name_row.contains('\u{25CF}'), "the glyph survives next to the name");
        // The name is section chrome; bright colour here is reserved for the status glyph.
        let name_span = name_line
            .spans
            .iter()
            .find(|s| s.content.contains("playwright-local"))
            .expect("name span");
        assert_eq!(name_span.style.fg, Some(theme::DIM), "server name renders in DIM chrome");
    }

    // ---------------------------------------------------------
    // blank-line spacing between WORKFLOWS entries.
    // ---------------------------------------------------------

    fn build_session_with_workflows(workflows: Vec<crate::app::WorkflowEntry>) -> App {
        let mut app = App::test_default();
        *app.workflows_mut() = workflows;
        app
    }

    #[test]
    fn workflows_section_inserts_blank_line_between_entries() {
        let entries = vec![
            make_workflow_entry("wf_a", "first-workflow", crate::app::WorkflowStatus::InProgress),
            make_workflow_entry("wf_b", "second-workflow", crate::app::WorkflowStatus::InProgress),
        ];
        let app = build_session_with_workflows(entries);
        let mut lines = Vec::new();
        append_workflows_section(&mut lines, &app, 60);
        // Layout shape: header + blank + row + blank-between + row.
        // Find the two workflow rows; assert there's a blank between them.
        let first_idx = lines
            .iter()
            .position(|l| line_text(l).contains("first-workflow"))
            .expect("first workflow row");
        let second_idx = lines
            .iter()
            .position(|l| line_text(l).contains("second-workflow"))
            .expect("second workflow row");
        assert!(second_idx > first_idx, "second comes after first");
        assert_eq!(
            second_idx,
            first_idx + 2,
            "Bug 6: exactly one blank line separates the two workflow rows",
        );
        assert!(line_text(&lines[first_idx + 1]).is_empty(), "Bug 6: blank between entries");
    }

    // ---------------------------------------------------------
    // GIT section repo_gate render: NotARepo hides the section,
    // ScannerFailed surfaces the unhealthy banner.
    // ---------------------------------------------------------

    fn app_with_git_gate(repo_gate: RepoGate) -> App {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("inspector-git-test");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.git_diff_snapshot = Some(forge_primitives::git_diff::GitDiffSnapshot {
            branch: forge_primitives::git::GitBranch::NoRepo,
            default_branch: None,
            repo_gate,
            worktree: forge_primitives::git_diff::LayerState::Clean,
            branch_ahead: forge_primitives::git_diff::LayerState::Clean,
            pr: None,
            closes: vec![],
        });
        app.sessions.insert(key.clone(), session);
        app.active_session_key = Some(key);
        app
    }

    #[test]
    fn git_section_hidden_when_not_a_repo() {
        let app = app_with_git_gate(RepoGate::NotARepo);
        let mut lines = Vec::new();
        append_git_section(&mut lines, &app, 60);
        assert!(lines.is_empty(), "a clean non-repo cwd suppresses the GIT section");
    }

    #[test]
    fn git_section_surfaces_unhealthy_banner_on_scanner_failure() {
        let app = app_with_git_gate(RepoGate::ScannerFailed);
        let mut lines = Vec::new();
        append_git_section(&mut lines, &app, 60);
        let joined = lines.iter().map(|l| line_text(l)).collect::<Vec<_>>().join("\n");
        assert!(
            joined.contains("git scanner unhealthy"),
            "ScannerFailed must surface the unhealthy banner; got:\n{joined}"
        );
    }

    /// Active session on branch `branch` with a clean tree (so the
    /// `🦉` is absent and the badge is the only thing on the header),
    /// carrying `count` waiting replies recorded against `waiting_on`.
    fn app_with_waiting_replies(branch: &str, waiting_on: &str, count: usize) -> App {
        let mut app = app_with_git_gate(RepoGate::InRepo);
        let key = app.active_session_key.clone().expect("active key");
        let session = app.sessions.get_mut(&key).expect("bucket");
        if let Some(snapshot) = session.git_diff_snapshot.as_mut() {
            snapshot.branch = forge_primitives::git::GitBranch::Named(branch.to_owned());
        }
        session.review_replies_waiting =
            crate::app::ReviewRepliesWaiting::merge(None, waiting_on, count);
        app
    }

    fn git_header_text(app: &App) -> String {
        let mut lines = Vec::new();
        append_git_section(&mut lines, app, 60);
        lines.first().map(line_text).unwrap_or_default()
    }

    #[test]
    fn git_header_badges_waiting_review_replies() {
        let app = app_with_waiting_replies("feat", "feat", 2);
        let header = git_header_text(&app);
        assert!(header.contains("\u{1F4AC} 2"), "the badge names the count: {header}");
    }

    #[test]
    fn git_header_badge_absent_without_waiting_replies() {
        let app = app_with_git_gate(RepoGate::InRepo);
        assert!(!git_header_text(&app).contains('\u{1F4AC}'), "no replies waiting, no badge");
    }

    /// The `🦉` click target is stamped at a fixed offset from the
    /// right edge, so the badge must sit LEFT of the owl and leave the
    /// header exactly `width` cells wide - otherwise the click lands
    /// somewhere else.
    #[test]
    fn git_header_badge_leaves_the_owl_at_its_hit_tested_column() {
        use unicode_width::UnicodeWidthStr;

        let mut app = app_with_waiting_replies("feat", "feat", 2);
        let key = app.active_session_key.clone().expect("active key");
        if let Some(snapshot) =
            app.sessions.get_mut(&key).and_then(|s| s.git_diff_snapshot.as_mut())
        {
            snapshot.worktree = LayerState::Populated(GitDiffStats {
                files: vec![],
                total_files: 1,
                total_added: 1,
                total_removed: 0,
            });
        }
        let header = git_header_text(&app);
        assert!(header.contains("\u{1F4AC} 2") && header.contains('\u{1F989}'), "both: {header}");
        assert_eq!(header.width(), 60, "the header fills the pane exactly: {header}");
        let owl_col = header.find('\u{1F989}').map(|byte| header[..byte].width()).expect("owl");
        assert_eq!(owl_col, 60 - 3, "owl stays where the hit test stamps it");
    }

    #[test]
    fn git_header_badge_hidden_once_the_header_describes_another_branch() {
        // The badge points at `/diff`, which opens on the CURRENT branch -
        // showing another branch's count there would be a lie.
        let app = app_with_waiting_replies("main", "feat", 3);
        let header = git_header_text(&app);
        assert!(!header.contains('\u{1F4AC}'), "stale-branch badge suppressed: {header}");
    }

    fn gotify_sub(
        id: u128,
        team_role: Option<&str>,
        apps: &[&str],
        min_priority: Option<u8>,
    ) -> forge_primitives::GotifySubscription {
        forge_primitives::GotifySubscription {
            id: uuid::Uuid::from_u128(id),
            project: "p".to_owned(),
            team_role: team_role.map(str::to_owned),
            applications: apps.iter().map(|s| (*s).to_owned()).collect(),
            min_priority,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// The snapshot is scoped to the session's own role before it
    /// reaches the render, so there is only ever one owner on screen and
    /// no owner header labels it.
    #[test]
    fn gotify_section_renders_own_subscriptions_without_an_owner_header() {
        let mut app = App::test_default();
        app.gotify_connected = true;
        app.gotify_subs = vec![
            gotify_sub(1, Some("steward"), &["Entertainment"], None),
            gotify_sub(2, Some("steward"), &["Alerts"], Some(5)),
        ];

        let mut lines = Vec::new();
        append_gotify_section(&mut lines, &app, 60);
        let texts = lines.iter().map(|l| line_text(l)).collect::<Vec<_>>();
        let joined = texts.join("\n");

        assert!(
            !texts.iter().any(|t| matches!(t.trim(), "lead" | "steward")),
            "no owner header renders; got:\n{joined}",
        );
        for name in ["Entertainment", "Alerts"] {
            assert!(joined.contains(name), "every owned subscription renders; got:\n{joined}");
        }
        assert!(!joined.contains('\u{2192}'), "no per-row role arrow either; got:\n{joined}");
        assert!(!joined.contains("app:"), "the old app: caption is dropped; got:\n{joined}");
    }

    #[test]
    fn gotify_multi_app_subscription_wraps_and_keeps_every_name() {
        let apps = [
            "Beszel",
            "Host",
            "Backups",
            "Security",
            "Media",
            "Alerts",
            "Deploys",
            "Entertainment",
        ];
        let mut app = App::test_default();
        app.gotify_connected = true;
        app.gotify_subs = vec![gotify_sub(1, None, &apps, Some(5))];

        let mut lines = Vec::new();
        // A narrow pane forces the comma-joined list to wrap.
        append_gotify_section(&mut lines, &app, 28);
        let texts = lines.iter().map(|l| line_text(l)).collect::<Vec<_>>();
        let joined = texts.join("\n");

        for name in apps {
            assert!(
                joined.contains(name),
                "app name {name} stays visible (never dropped); got:\n{joined}",
            );
        }

        let app_lines =
            texts.iter().filter(|t| apps.iter().any(|n| t.contains(*n))).collect::<Vec<_>>();
        assert!(app_lines.len() >= 2, "a long app list wraps across >=2 lines; got:\n{joined}");
        // Hang-indent: every wrapped app line shares the first line's
        // left indent (aligns under the first app name).
        let indents = app_lines.iter().map(|t| t.len() - t.trim_start().len()).collect::<Vec<_>>();
        assert!(
            indents.windows(2).all(|w| w[0] == w[1]),
            "wrapped app lines hang-indent-align; got indents {indents:?} in:\n{joined}",
        );
    }

    #[test]
    fn gotify_priority_line_renders_under_each_subscription() {
        let mut app = App::test_default();
        app.gotify_connected = true;
        app.gotify_subs = vec![
            gotify_sub(1, None, &["Alerts"], Some(5)),
            gotify_sub(2, None, &["Deploys"], None),
        ];

        let mut lines = Vec::new();
        append_gotify_section(&mut lines, &app, 60);
        let texts = lines.iter().map(|l| line_text(l)).collect::<Vec<_>>();
        let joined = texts.join("\n");

        let alerts_at = texts.iter().position(|t| t.contains("Alerts")).expect("alerts app line");
        let floor_at =
            texts.iter().position(|t| t.contains("priority >=5")).expect("priority floor line");
        let deploys_at =
            texts.iter().position(|t| t.contains("Deploys")).expect("deploys app line");
        let any_at =
            texts.iter().position(|t| t.contains("priority any")).expect("priority any line");

        assert_eq!(
            floor_at,
            alerts_at + 1,
            "priority >=N sits directly under its own app list; got:\n{joined}",
        );
        assert_eq!(
            any_at,
            deploys_at + 1,
            "priority any sits directly under its own app list; got:\n{joined}",
        );
    }

    #[test]
    fn gotify_single_app_and_empty_filter_render() {
        let mut app = App::test_default();
        app.gotify_connected = true;
        app.gotify_subs =
            vec![gotify_sub(1, None, &["Entertainment"], None), gotify_sub(2, None, &[], Some(3))];

        let mut lines = Vec::new();
        append_gotify_section(&mut lines, &app, 60);
        let texts = lines.iter().map(|l| line_text(l)).collect::<Vec<_>>();
        let joined = texts.join("\n");

        assert!(joined.contains("Entertainment"), "the single named app renders; got:\n{joined}");
        assert!(
            texts.iter().any(|t| t.trim() == "any"),
            "an empty filter renders the `any` app line; got:\n{joined}",
        );
        assert!(
            joined.contains("priority >=3"),
            "the empty-filter subscription keeps its own priority floor; got:\n{joined}",
        );
    }

    #[test]
    fn gotify_wrapped_app_line_reserves_the_trailing_comma_column() {
        // A run that packs to the wrap budget then breaks: the trailing
        // comma on the flushed line must not eat the 1-col right gutter.
        let inner_width: u16 = 40;
        let long = "a".repeat(32);
        let mut app = App::test_default();
        app.gotify_connected = true;
        app.gotify_subs = vec![gotify_sub(1, None, &[long.as_str(), "bb", "c"], Some(5))];

        let mut lines = Vec::new();
        append_gotify_section(&mut lines, &app, inner_width);
        for line in &lines {
            let w = rendered_width(line);
            assert!(
                w < usize::from(inner_width),
                "row consumed the right gutter ({w} >= {inner_width}): {}",
                line_text(line),
            );
        }
    }

    #[test]
    fn gotify_over_wide_app_name_drops_the_trailing_comma_at_the_gutter() {
        // A name wider than the wrap budget (max_width = inner_width - 5)
        // that is followed by another name must flush WITHOUT a trailing
        // comma; the comma alone would push it past the 1-col gutter.
        let inner_width: u16 = 40;
        let over_wide = "a".repeat(36); // max_width + 1
        let mut app = App::test_default();
        app.gotify_connected = true;
        app.gotify_subs = vec![gotify_sub(1, None, &[over_wide.as_str(), "tail"], None)];

        let mut lines = Vec::new();
        append_gotify_section(&mut lines, &app, inner_width);
        for line in &lines {
            let w = rendered_width(line);
            assert!(
                w < usize::from(inner_width),
                "row consumed the right gutter ({w} >= {inner_width}): {}",
                line_text(line),
            );
        }
    }

    /// The status rides the header line rather than costing its own row
    /// plus two blanks, and stays inside the pane's 1-col right gutter at
    /// both the Wide (32) and Medium (24) inspector widths. Columns are
    /// read from a rendered buffer, not a formatted string: a collected
    /// row misreports position for any wide glyph, so a printed dump
    /// cannot prove this fits.
    #[test]
    fn gotify_header_carries_status_and_fits_wide_and_medium() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // The disconnected word is the longer of the two, so Medium with
        // the stream down is the tightest case the header ever renders.
        for (width, connected, glyph, word) in [
            (32u16, true, '\u{25c8}', "connected"),
            (32, false, '\u{26a0}', "disconnected"),
            (24, true, '\u{25c8}', "connected"),
            (24, false, '\u{26a0}', "disconnected"),
        ] {
            let mut app = App::test_default();
            app.gotify_connected = connected;
            app.gotify_subs = vec![gotify_sub(1, None, &["Alerts"], Some(5))];

            let mut lines = Vec::new();
            append_gotify_section(&mut lines, &app, width);

            let height = 8u16;
            let mut term = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            term.draw(|f| {
                f.render_widget(Paragraph::new(lines.clone()), Rect { x: 0, y: 0, width, height });
            })
            .expect("draw");

            let buffer = term.backend().buffer().clone();
            let w = usize::from(buffer.area.width);
            let cell = |x: usize, y: usize| buffer.content[y * w + x].symbol().to_owned();
            let row = |y: usize| (0..w).map(|x| cell(x, y)).collect::<String>();

            let header = row(0);
            assert!(header.contains("GOTIFY"), "w={width}: header names the section: {header:?}");
            assert!(
                header.contains(word),
                "w={width} connected={connected}: the status word renders whole: {header:?}",
            );
            assert!(
                header.contains(glyph),
                "w={width} connected={connected}: the state glyph renders: {header:?}",
            );

            // True column of the last painted cell - the right gutter must
            // stay blank so nothing is clipped at the pane edge.
            let last_col = (0..w)
                .rposition(|x| !cell(x, 0).trim().is_empty())
                .expect("header row paints something");
            assert!(
                last_col < w - usize::from(PANE_PAD),
                "w={width}: status keeps the {PANE_PAD}-col right gutter (last painted col \
                 {last_col} of {w}): {header:?}",
            );

            // Two rows bought back: no standalone status line survives
            // anywhere below the header.
            for y in 1..usize::from(height) {
                assert!(
                    !row(y).contains(word),
                    "w={width}: the status renders once, on the header - found again at y={y}",
                );
            }
            // Header, one blank, then the subscription's app list.
            assert!(row(1).trim().is_empty(), "w={width}: one blank under the header");
            assert!(row(2).contains("Alerts"), "w={width}: the first subscription follows it");

            // Medium is the tightest pane the header has to survive, so
            // pin both literal layouts - these are what the forge-map
            // mockups reproduce. Spelt out rather than recomputed from the
            // production formula, which would assert nothing.
            if width == 24 {
                let expected = if connected {
                    " GOTIFY     \u{25c8} connected"
                } else {
                    " GOTIFY  \u{26a0} disconnected"
                };
                assert_eq!(
                    header.trim_end(),
                    expected,
                    "Medium-tier header layout (connected={connected})",
                );
            }
        }
    }

    /// Visibility keys on OWNED SUBSCRIPTIONS ALONE. A session that
    /// subscribed to something keeps the section whether or not the
    /// stream is up - hiding it exactly when the stream drops is the
    /// silent-failure shape, since the alerts the user asked for have
    /// stopped arriving and nothing on screen says so. A session that
    /// subscribed to nothing hides it without consulting the connection
    /// at all: there is nothing for it to receive either way.
    #[test]
    fn gotify_section_visibility_keys_on_owned_subscriptions_not_the_stream() {
        let sub = || forge_primitives::GotifySubscription {
            id: uuid::Uuid::from_u128(1),
            project: "p".to_owned(),
            team_role: None,
            applications: vec![],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let mut app = App::test_default();

        for connected in [true, false] {
            app.gotify_connected = connected;

            app.gotify_subs = vec![];
            assert!(
                !gotify_section_visible(&app),
                "no owned subscription hides the section (connected={connected})",
            );

            app.gotify_subs = vec![sub()];
            assert!(
                gotify_section_visible(&app),
                "an owned subscription shows the section (connected={connected})",
            );
        }
    }

    // ---------------------------------------------------------
    // NEEDS ATTENTION attention band (pinned above the scroll body).
    // ---------------------------------------------------------

    /// Seed one BACKGROUND session (not the active bucket) carrying a
    /// pending permission prompt, stamped with a project name so the
    /// row resolves without a workspace catalog.
    fn app_with_waiting_session(name: &str) -> App {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id(name);
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some(name.to_owned());
        let prompt = crate::app::prompt::PromptState::from_permission(
            format!("tc-{name}"),
            crate::app::prompt::tests::make_permission_request(),
        );
        session.prompt_queue.push_back(prompt);
        app.sessions.insert(key, session);
        app
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let width = usize::from(buffer.area.width);
        buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The band's lines for the current app state (all entries shown,
    /// no overflow), or an empty `Vec` when nothing is waiting.
    fn build_attention_band(app: &App, width: u16) -> Vec<Line<'static>> {
        let entries = app.needs_attention_sessions();
        if entries.is_empty() {
            return Vec::new();
        }
        build_attention_lines(&entries, entries.len(), 0, width, std::time::SystemTime::now())
    }

    #[test]
    fn attention_band_absent_when_no_session_waits() {
        let app = App::test_default();
        assert!(build_attention_band(&app, 40).is_empty(), "no waiting session -> empty band");
    }

    #[test]
    fn attention_band_renders_a_waiting_review_replies_row() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("reviewer");
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some("forge".to_owned());
        session.review_replies_waiting = crate::app::ReviewRepliesWaiting::merge(None, "feat", 2);
        app.sessions.insert(key, session);

        let text =
            build_attention_band(&app, 60).iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains('\u{1F4AC}'), "the speech-balloon glyph marks the row: {text}");
        assert!(text.contains("forge"), "project name in the row: {text}");
        assert!(text.contains("review replies \u{00B7} 2"), "detail names the count: {text}");
    }

    #[test]
    fn attention_band_renders_header_and_row_when_a_session_waits() {
        let app = app_with_waiting_session("gateway-backend");
        // Width 60 so the full name renders alongside the whole detail
        // (at the 40-col Wide pane the name legitimately truncates to
        // keep the kind/tool/wait detail whole - see the fit test).
        let lines = build_attention_band(&app, 60);
        assert!(!lines.is_empty(), "a waiting session produces a band");
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("NEEDS ATTENTION"), "header present: {text}");
        assert!(text.contains("gateway-backend"), "session name in a row: {text}");
        assert!(text.contains("permission"), "permission kind rendered: {text}");
        assert!(text.contains("Bash"), "tool name for a permission prompt: {text}");
    }

    #[test]
    fn attention_detail_formats_kind_tool_and_wait() {
        use std::time::{Duration, SystemTime};
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let perm = AttentionEntry {
            session_key: forge_workspace::SessionKey::from_session_id("p"),
            name: "gateway-backend".to_owned(),
            role: None,
            kind: AttentionKind::Permission { tool: "Bash".to_owned() },
            enqueued_at: base,
        };
        // 3m20s waited -> compact "3m".
        assert_eq!(
            attention_detail(&perm, base + Duration::from_secs(200)),
            "permission \u{00B7} Bash \u{00B7} 3m"
        );

        let question = AttentionEntry { kind: AttentionKind::Question, ..perm.clone() };
        assert_eq!(
            attention_detail(&question, base + Duration::from_secs(20)),
            "question \u{00B7} 20s"
        );

        let no_tool =
            AttentionEntry { kind: AttentionKind::Permission { tool: String::new() }, ..perm };
        assert_eq!(
            attention_detail(&no_tool, base + Duration::from_secs(20)),
            "permission \u{00B7} 20s"
        );
    }

    /// A failed row names the wire classification with the same labels
    /// the in-chat `api_retry` notice uses, plus the raw status when the
    /// CLI reported one.
    #[test]
    fn attention_detail_formats_failure_with_and_without_status() {
        use std::time::{Duration, SystemTime};
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let entry = AttentionEntry {
            session_key: forge_workspace::SessionKey::from_session_id("f"),
            name: "gateway-backend".to_owned(),
            role: None,
            kind: AttentionKind::Failed {
                error: forge_primitives::ApiRetryError::ServerError,
                status: Some(529),
            },
            enqueued_at: base,
        };
        assert_eq!(
            attention_detail(&entry, base + Duration::from_secs(200)),
            "failed \u{00B7} server_error HTTP 529 \u{00B7} 3m"
        );

        let no_status = AttentionEntry {
            kind: AttentionKind::Failed {
                error: forge_primitives::ApiRetryError::Unknown,
                status: None,
            },
            ..entry
        };
        assert_eq!(
            attention_detail(&no_status, base + Duration::from_secs(20)),
            "failed \u{00B7} connection error \u{00B7} 20s"
        );
    }

    /// The failure row must be visually distinct from the yellow
    /// permission/question triangle: red `✕`, the same glyph the
    /// Projects pane already uses for a dead worker.
    #[test]
    fn attention_row_renders_failure_in_red_cross() {
        let entry = AttentionEntry {
            session_key: forge_workspace::SessionKey::from_session_id("f"),
            name: "gateway-backend".to_owned(),
            role: None,
            kind: AttentionKind::Failed {
                error: forge_primitives::ApiRetryError::ServerError,
                status: Some(529),
            },
            enqueued_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let line = attention_row_line(60, &entry, std::time::SystemTime::UNIX_EPOCH);
        let glyph = line
            .spans
            .iter()
            .find(|s| s.content.contains('\u{2715}'))
            .expect("failure row carries the ✕ glyph");
        assert_eq!(glyph.style.fg, Some(theme::STATUS_ERROR), "✕ renders in STATUS_ERROR red");
        assert!(
            !line.spans.iter().any(|s| s.content.contains('\u{25b3}')),
            "a failure row must not also carry the yellow △",
        );
    }

    /// Band header wording: the band covers failures as well as pending
    /// prompts, so it reads NEEDS ATTENTION.
    #[test]
    fn attention_header_reads_needs_attention() {
        let line = attention_header_line(40, 2);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("NEEDS ATTENTION"), "header names the band: {text}");
    }

    #[test]
    fn attention_band_passes_through_empty_and_shrinks_body_when_present() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let area = Rect { x: 0, y: 2, width: 40, height: 20 };

        // Empty: the body rect passes through unchanged.
        let mut app = App::test_default();
        let mut term = Terminal::new(TestBackend::new(40, 24)).expect("terminal");
        let mut empty_body = Rect::default();
        term.draw(|f| empty_body = render_attention_band(f, area, &mut app)).expect("draw");
        assert_eq!(empty_body, area, "no band -> body rect unchanged");

        // Present: the band takes the top rows; the body shifts down and
        // shrinks so GIT (drawn into the returned rect) is pushed down.
        let mut app = app_with_waiting_session("gateway-backend");
        let mut term = Terminal::new(TestBackend::new(40, 24)).expect("terminal");
        let mut body = Rect::default();
        term.draw(|f| body = render_attention_band(f, area, &mut app)).expect("draw");
        assert!(body.y > area.y, "body pushed down below the band: {body:?}");
        assert!(body.height < area.height, "body shrank by the band height: {body:?}");
        assert_eq!(body.x, area.x, "body keeps the pane x");
        assert_eq!(body.width, area.width, "body keeps the pane width");
        assert_eq!(body.y + body.height, area.y + area.height, "band + body tile the area");
        let text = buffer_text(term.backend().buffer());
        assert!(text.contains("NEEDS ATTENTION"), "band header rendered into the buffer: {text}");
    }

    #[test]
    fn attention_rows_fit_within_inner_width() {
        let now = std::time::SystemTime::UNIX_EPOCH;
        let entry = AttentionEntry {
            session_key: forge_workspace::SessionKey::from_session_id("s"),
            name: "a-very-long-project-name-that-must-truncate".to_owned(),
            role: Some("steward".to_owned()),
            kind: AttentionKind::Permission { tool: "mcp__forge__workers__spawn".to_owned() },
            enqueued_at: now,
        };
        for width in [30_u16, 40, 60] {
            let header = attention_header_line(width, 3);
            let row = attention_row_line(width, &entry, now);
            for line in [&header, &row] {
                let w = rendered_width(line);
                assert!(
                    w <= usize::from(width),
                    "line exceeds width {width}: {w} cols in {:?}",
                    line_text(line),
                );
            }
        }
    }

    #[test]
    fn attention_row_stamps_clickable_jump_target() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app_with_waiting_session("gateway-backend");
        let bg = forge_workspace::SessionKey::from_session_id("gateway-backend");
        assert_ne!(
            app.active_session_key.as_ref(),
            Some(&bg),
            "precondition: the waiting session is a background session",
        );

        let area = Rect { x: 0, y: 2, width: 40, height: 20 };
        let mut term = Terminal::new(TestBackend::new(40, 24)).expect("terminal");
        term.draw(|f| {
            render_attention_band(f, area, &mut app);
        })
        .expect("draw");

        let (key, y, x_start) = app
            .pane_hit_targets
            .iter()
            .find_map(|t| match t {
                PaneHitTarget::InspectorAttentionRow { session_key, y, x_start, .. } => {
                    Some((session_key.clone(), *y, *x_start))
                }
                _ => None,
            })
            .expect("an attention-row hit target is stamped");
        assert_eq!(key, bg, "target carries the waiting session's key");
        assert!(y > area.y, "the row sits below the band header");

        // A click on the row resolves to this x+y-bounded target.
        let hit = app.pane_hit_targets.iter().find(|t| t.contains(x_start, y));
        assert!(
            matches!(hit, Some(PaneHitTarget::InspectorAttentionRow { .. })),
            "a click on the row hits the attention target",
        );

        // Switching (what the click handler does) makes it the active session.
        app.switch_active_session(key);
        assert_eq!(app.active_session_key.as_ref(), Some(&bg), "the click jumps to the session");
    }

    #[test]
    fn attention_row_hit_target_lands_on_its_own_rendered_row() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Two waiting sessions with distinct names: the stamped hit-target
        // `y` (render_attention_band) and the line layout
        // (build_attention_lines) are two separate magic numbers, so an
        // off-by-one lands a click on the WRONG session's row rather than a
        // blank - this pins each stamp to the buffer row it actually
        // targets. Guards the header/spacer/row offset against silent drift.
        let names = ["alpha-project", "beta-project"];
        let mut app = App::test_default();
        for name in names {
            let key = forge_workspace::SessionKey::from_session_id(name);
            let mut session = crate::app::session::UiSession::new(key.clone());
            session.project = Some(name.to_owned());
            let prompt = crate::app::prompt::PromptState::from_permission(
                format!("tc-{name}"),
                crate::app::prompt::tests::make_permission_request(),
            );
            session.prompt_queue.push_back(prompt);
            app.sessions.insert(key, session);
        }
        assert_eq!(app.needs_attention_sessions().len(), 2, "two background waiters");
        let expected: std::collections::HashMap<forge_workspace::SessionKey, &str> =
            names.iter().map(|n| (forge_workspace::SessionKey::from_session_id(*n), *n)).collect();

        let width = 60u16;
        let area = Rect { x: 0, y: 0, width, height: 24 };
        let mut term = Terminal::new(TestBackend::new(width, 24)).expect("terminal");
        term.draw(|f| {
            render_attention_band(f, area, &mut app);
        })
        .expect("draw");

        let buffer = term.backend().buffer().clone();
        let row_text = |y: u16| -> String {
            let w = usize::from(buffer.area.width);
            let start = usize::from(y) * w;
            buffer.content[start..start + w].iter().map(ratatui::buffer::Cell::symbol).collect()
        };

        let mut checked = 0;
        for target in &app.pane_hit_targets {
            let PaneHitTarget::InspectorAttentionRow { session_key, y, .. } = target else {
                continue;
            };
            let want = expected.get(session_key).expect("stamped key is a seeded session");
            let text = row_text(*y);
            assert!(text.contains('\u{25b3}'), "row at y={y} carries the △ marker; got {text:?}");
            assert!(
                text.contains(want),
                "hit-target y={y} must land on its own session's row ({want}); got {text:?}",
            );
            checked += 1;
        }
        assert_eq!(checked, 2, "both waiting sessions stamped a row target");
    }

    #[test]
    fn attention_row_target_is_pinned_regardless_of_body_scroll() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // The band is pinned above the scroll body, so scrolling the
        // active session's inspector body must not move the attention
        // row's hit target. Locks the invariant against a future
        // "fold the band into the scroll body" refactor.
        let mut app = app_with_waiting_session("gateway-backend");
        let bg = forge_workspace::SessionKey::from_session_id("gateway-backend");
        let area = Rect { x: 0, y: 0, width: 40, height: 24 };

        let row_y_at = |app: &mut App, offset: u16| -> u16 {
            if let Some(active) = app.try_active_bucket_mut() {
                active.inspector_scroll_offset = offset;
            }
            app.pane_hit_targets.clear();
            let mut term = Terminal::new(TestBackend::new(40, 24)).expect("terminal");
            let subagents = app.subagents_view();
            term.draw(|f| render(f, area, app, &subagents)).expect("draw");
            app.pane_hit_targets
                .iter()
                .find_map(|t| match t {
                    PaneHitTarget::InspectorAttentionRow { session_key, y, .. }
                        if *session_key == bg =>
                    {
                        Some(*y)
                    }
                    _ => None,
                })
                .expect("attention row target stamped")
        };

        let unscrolled = row_y_at(&mut app, 0);
        let scrolled = row_y_at(&mut app, 50);
        assert_eq!(unscrolled, scrolled, "the pinned row stays put while the body scrolls");
    }

    #[test]
    fn attention_band_caps_rows_clamps_body_and_skips_clipped_rows() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::test_default();
        for i in 0..6 {
            let key = forge_workspace::SessionKey::from_session_id(format!("bg-{i}"));
            let mut session = crate::app::session::UiSession::new(key.clone());
            session.project = Some(format!("proj-{i}"));
            let prompt = crate::app::prompt::PromptState::from_permission(
                format!("tc-{i}"),
                crate::app::prompt::tests::make_permission_request(),
            );
            session.prompt_queue.push_back(prompt);
            app.sessions.insert(key, session);
        }
        assert_eq!(app.needs_attention_sessions().len(), 6, "six background waiters");

        let count_rows = |app: &App| {
            app.pane_hit_targets
                .iter()
                .filter(|t| matches!(t, PaneHitTarget::InspectorAttentionRow { .. }))
                .count()
        };

        // Tall pane: the fixed cap shows ATTENTION_MAX_ROWS rows + `+N more`.
        app.pane_hit_targets.clear();
        let mut term = Terminal::new(TestBackend::new(40, 30)).expect("terminal");
        let mut body = Rect::default();
        term.draw(|f| body = render_attention_band(f, Rect::new(0, 2, 40, 24), &mut app))
            .expect("draw");
        assert_eq!(count_rows(&app), ATTENTION_MAX_ROWS, "cap: only K rows stamped");
        assert!(body.height > 0, "body kept on a tall pane");
        assert!(buffer_text(term.backend().buffer()).contains("+1 more"), "overflow tail renders");

        // Short pane: the band clamps to keep body rows, and clipped rows
        // are not stamped (the clip-skip branch).
        app.pane_hit_targets.clear();
        let mut term = Terminal::new(TestBackend::new(40, 30)).expect("terminal");
        let mut body = Rect::default();
        term.draw(|f| body = render_attention_band(f, Rect::new(0, 2, 40, 7), &mut app))
            .expect("draw");
        assert!(body.height > 0, "body stays non-empty on a short pane: {body:?}");
        let stamped = count_rows(&app);
        assert!(
            (1..ATTENTION_MAX_ROWS).contains(&stamped),
            "clip reduced the stamped rows below the cap: {stamped}",
        );
    }

    #[test]
    fn fmt_countdown_hours_and_attention_detail_clock_skew() {
        use std::time::{Duration, SystemTime};

        // Hours branch (>= 1h).
        assert_eq!(fmt_countdown(Duration::from_secs(3661)), "1h1m");
        assert_eq!(fmt_countdown(Duration::from_secs(7200)), "2h0m");

        // Clock skew: `now` earlier than `enqueued_at` -> duration_since
        // Err -> "0s", not a panic or a bogus huge age.
        let entry = AttentionEntry {
            session_key: forge_workspace::SessionKey::from_session_id("s"),
            name: "p".to_owned(),
            role: None,
            kind: AttentionKind::Question,
            enqueued_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1000),
        };
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(500);
        assert_eq!(attention_detail(&entry, earlier), "question \u{00B7} 0s");
    }

    /// The thumb reports scroll position, not liveness, so its glyph must
    /// not vary with the pulse counter even for a session whose in-flight
    /// tool call drives every other live-work surface.
    #[test]
    fn inspector_thumb_symbol_does_not_follow_the_pulse_counter() {
        use crate::app::{ChatMessage, MessageBlock, MessageRole};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let rail_symbols = |spinner_frame: usize| {
            let mut app = App::test_default();
            let mut bash = subagent_test_child_info("tu-bash", "Bash", "cargo nextest run");
            bash.raw_input = Some(serde_json::json!({
                "command": "cargo nextest run",
                "description": "Run unit tests",
                "run_in_background": true,
            }));
            bash.status = crate::agent::model::ToolCallStatus::InProgress;
            app.push_message_tracked(ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(bash))],
            ));
            app.insert_session_task_mapping("b1".to_owned(), "tu-bash".to_owned());
            *app.background_tasks_mut() = vec![crate::app::BackgroundTask {
                task_id: "b1".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "Run unit tests".to_owned(),
            }];
            app.spinner_frame = spinner_frame;

            let (width, height) = (32u16, 6u16);
            let mut term = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            term.draw(|f| render(f, Rect { x: 0, y: 0, width, height }, &mut app, &[]))
                .expect("draw");

            let body = app.rendered_inspector_body_area;
            let buffer = term.backend().buffer().clone();
            let w = usize::from(buffer.area.width);
            let rail_x = usize::from(body.right().saturating_sub(1));
            (body.y..body.bottom())
                .map(|y| buffer.content[usize::from(y) * w + rail_x].symbol().to_owned())
                .collect::<Vec<_>>()
        };

        let baseline = rail_symbols(0);
        assert!(
            baseline.iter().any(|s| s == "\u{2590}"),
            "fixture must overflow, and the thumb must paint ▐: {baseline:?}",
        );
        // 20 covers any cycle up to that length, including the ten-step
        // shape `tab_title::pulse_char` uses.
        for frame in 1..=20 {
            assert_eq!(
                rail_symbols(frame),
                baseline,
                "the thumb glyph must not vary with spinner_frame={frame}",
            );
        }
    }
}
