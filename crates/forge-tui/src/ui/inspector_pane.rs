//! Inspector pane (right side, Wide + Medium tiers; full-screen
//! overlay at Narrow tier).
//!
//! Mirror of the left [`crate::ui::projects_pane`] in chrome and
//! tier behaviour. Two sections separated by a DIM `─` rule:
//!
//! - `GIT` — always rendered. Shows the active session's cwd, the
//!   current branch on its own row, a sub-label row carrying the
//!   diff context (`worktree` / `vs <default>`) with aggregate
//!   `+N -M` totals right-justified (when there's a diff), an
//!   optional `PR #N → closes #M #K` row when the scanner resolved
//!   an open pull request for the branch, and a box-drawing tree
//!   of the top-N changed files grouped by directory. Single-child
//!   directory chains fold so deep paths render as one row. Sourced
//!   from `UiSession.git_diff_snapshot`.
//! - `TASKS` — rendered when the active session has todos or a
//!   pending verification nudge. The live `TodoWrite` snapshot is
//!   the sole surface for the todo list; the chat-stream
//!   `TodoWrite` tool-call card is suppressed.
//! - `PROCESSES` — rendered when the active session has at least
//!   one currently-in-flight long-running tool call. Three kinds
//!   surface here: backgrounded `Bash` (via `run_in_background:
//!   true` OR `assistant_auto_backgrounded`), `Monitor` streaming-
//!   process watchers, and `CronCreate` scheduled prompts. Live
//!   monitor only — completed / failed / killed rows are filtered
//!   out at the collector level so the section disappears once
//!   work wraps up. Rows are built by
//!   `crate::app::processes::collect_active_processes` from each
//!   tool call's `raw_input` + status; the renderer chooses glyphs
//!   + colours per `ProcessKind`.
//!
//! Reads from per-session state on `UiSession.todos` (post PR #109)
//! and `UiSession.git_diff_snapshot`. The
//! `TodoWriteOutputMetadata.verification_nudge_needed` flag surfaces
//! as a dim-yellow notice above the `TASKS` header until the next
//! `TodoWrite` clears it.
//!
//! TASKS item rendering:
//! - `✓` green glyph + DIM crossed-out text for `Completed`
//! - `▸` RUST_ORANGE glyph + white bold text for `InProgress`
//!   (wraps onto continuation lines indented under the glyph;
//!   uses `active_form` when present, else `content`)
//! - `○` DIM glyph + gray text for `Pending` (truncates with `…`)

use forge_primitives::git::{GitBranch, GitIssueRef, GitPrInfo};
use forge_workspace::env::git_diff::{GitDiffFile, GitDiffSnapshot, GitDiffView};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::agent::model::ToolCallStatus;
use crate::app::App;
use crate::app::MessageBlock;
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
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let (banner_area, body_area) = split_banner_body(area);

    // Pinned banner: `INSPECTOR` in RUST_ORANGE bold + dim rule.
    let banner_lines = build_inline_banner(area.width);
    frame.render_widget(Paragraph::new(banner_lines), banner_area);

    // Scrollable body.
    render_scrollable_body(frame, body_area, app);
}

/// Render the Narrow-tier full-screen Inspector overlay into `area`.
/// Shares the body builder with the inline path, wrapped in an
/// overlay-specific banner with an `INSPECTOR ▦` label on the left
/// and a `✕` glyph on the right (stamped as
/// [`PaneHitTarget::OverlayClose`] for the click handler). The
/// banner + rule stay pinned; the body scrolls underneath them.
pub fn render_overlay(frame: &mut Frame, area: Rect, app: &mut App) {
    app.pane_hit_targets.clear();

    let (banner_area, body_area) = split_banner_body(area);

    // Banner row: `INSPECTOR ▦ … ✕` spanning the full overlay width.
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

    // Stamp ✕ hit-target — last char on the banner row.
    let close_x_start =
        area.x.saturating_add(area.width).saturating_sub(u16::try_from(close_chars).unwrap_or(1));
    let close_x_end = area.x.saturating_add(area.width);
    app.pane_hit_targets.push(PaneHitTarget::OverlayClose {
        y: area.y,
        height: 1,
        x_start: close_x_start,
        x_end: close_x_end,
    });

    render_scrollable_body(frame, body_area, app);
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

/// Render the inspector body (`GIT` → `TASKS` → `PROCESSES` …)
/// into `body_area` with the active session's scroll offset
/// applied. Clamps the offset to `[0, max]` (writing the clamped
/// value back so the wheel handler doesn't desync after the body
/// shrinks), stamps `body_area` onto `App.rendered_inspector_body_area`
/// for the mouse-wheel hit test, and overlays a vertical scrollbar
/// on the right edge whenever the body overflows.
fn render_scrollable_body(frame: &mut Frame, body_area: Rect, app: &mut App) {
    app.rendered_inspector_body_area = body_area;
    if body_area.height == 0 || body_area.width == 0 {
        return;
    }

    let mut body_lines: Vec<Line<'static>> = Vec::new();
    append_body(&mut body_lines, app, body_area.width);
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

    // Stamp the `⤢` open-diff hit target — GIT header is body
    // line 0, so the glyph is visible exactly when `offset == 0`.
    // The glyph sits 2 cells in from the right edge (matches the
    // header's `… + " "` trailing pad in `append_git_section`),
    // hit-test extends one cell left + right for forgiveness.
    if has_open_diff_glyph && offset == 0 {
        let glyph_x = body_area.x.saturating_add(body_area.width).saturating_sub(2);
        let x_start = glyph_x.saturating_sub(1);
        let x_end = glyph_x.saturating_add(2);
        app.pane_hit_targets.push(PaneHitTarget::InspectorGitOpenDiff {
            y: body_area.y,
            height: 1,
            x_start,
            x_end,
        });
    }

    // Scrollbar — thumb-only, no rail, painted as a block cell in
    // `ROLE_ASSISTANT` colour. Animated when work is in flight so
    // the indicator reads as "alive" vs. a static dot. Matches the
    // chat scrollbar's visual weight (small thumb) plus a subtle
    // breathing pulse.
    let pulse = inspector_thumb_pulse(app);
    render_inspector_thumb(frame, body_area, total, visible, offset, pulse);
}

/// Frame index for the inspector thumb's breathing pulse. Wraps to
/// `None` when the active session has no observable work (alive
/// task IDs empty AND no in-progress Bash/Monitor on the wire), so
/// the thumb sits still during idle periods and only pulses while
/// something is actually running.
fn inspector_thumb_pulse(app: &App) -> Option<usize> {
    let has_alive_task = app.with_turn_state(|ts| !ts.alive_task_ids.is_empty());
    if has_alive_task {
        return Some(app.spinner_frame);
    }
    let has_in_progress_tool = app.active_session().is_some_and(|session| {
        session.messages.iter().any(|msg| {
            msg.blocks.iter().any(|block| {
                matches!(
                    block,
                    MessageBlock::ToolCall(tc) if tc.status == ToolCallStatus::InProgress
                )
            })
        })
    });
    has_in_progress_tool.then_some(app.spinner_frame)
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
    pulse: Option<usize>,
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
    let symbol = thumb_symbol(pulse);
    let buf = frame.buffer_mut();
    for row in thumb_top..thumb_end {
        let y = body_area.y.saturating_add(u16::try_from(row).unwrap_or(u16::MAX));
        if let Some(cell) = buf.cell_mut((rail_x, y)) {
            cell.set_symbol(symbol);
            cell.set_style(thumb_style);
        }
    }
}

/// Glyph used for the inspector thumb cell. When `pulse` is `Some`
/// (work is in flight), cycle through a 4-frame breathing pattern
/// driven by `App.spinner_frame` so the thumb reads as "alive". When
/// `pulse` is `None`, stay on the static block glyph the chat
/// scrollbar uses — same look, no movement.
fn thumb_symbol(pulse: Option<usize>) -> &'static str {
    const STATIC_THUMB: &str = "\u{2590}"; // ▐ right half block — chat baseline
    const THIN_THUMB: &str = "\u{2595}"; // ▕ right one-eighth block — pulse-out frame
    match pulse {
        None => STATIC_THUMB,
        Some(frame) => match frame % 4 {
            0 | 1 => STATIC_THUMB,
            _ => THIN_THUMB,
        },
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
fn append_body(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    append_git_section(lines, app, width);

    let todos = app.todos();
    // Section visibility gates on PENDING/IN-PROGRESS tasks
    // (completed are hidden by the renderer anyway). The
    // verification nudge alone is enough to surface the section
    // even when there are no live items.
    let has_live_tasks = todos.iter().any(|t| t.status != TodoStatus::Completed);
    if has_live_tasks || app.todo_verification_nudge() {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_tasks_section(lines, app, width);
    }

    let processes = collect_active_processes(app);
    if !processes.is_empty() {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_processes_section(lines, &processes, width, app.spinner_frame);
    }
}

/// Width threshold above which the PROCESSES section appends `· 12 MB`
/// to each row's metadata. Wide tier (inspector at 40 cols) gets memory;
/// Medium tier (inspector at 30 cols) drops it so the metadata fits.
const PROCESSES_MEMORY_WIDTH_THRESHOLD: u16 = 36;

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

/// Append the GIT section to `lines`. Always renders header + path.
/// Branch (with right-justified aggregate totals when the snapshot
/// carries a diff) + file tree are gated on the active session's
/// `git_diff_snapshot` — `None` (no scan yet) stops after the path
/// row.
fn append_git_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    // Section header — DIM bold, flush against the rule above
    // (mirrors `TASKS`). When the snapshot has a diff to surface
    // (Worktree / BranchVsDefault), append the `⤢` glyph at the
    // right edge as the open-diff affordance.
    let has_glyph = snapshot_has_diff(app);
    let mut header_spans = vec![Span::styled(
        " GIT".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )];
    if has_glyph {
        // " GIT" is 4 cells; glyph + right padding takes 2 cells
        // (`⤢ ` — glyph then a single trailing space so the click
        // target sits one column in from the absolute right edge).
        let pad = usize::from(width).saturating_sub(4 + 2);
        header_spans.push(Span::raw(" ".repeat(pad)));
        header_spans.push(Span::styled("\u{2922}".to_owned(), Style::default().fg(theme::DIM)));
        header_spans.push(Span::raw(" "));
    }
    lines.push(Line::from(header_spans));
    // Blank between header and content.
    lines.push(Line::default());

    // Path row — always rendered. Head-truncated so the leaf
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

    // Pull the diff display info out of the snapshot. `None` for
    // `CleanDefault` / `NoRepo` — no subtitle, no totals, no files.
    let diff = build_diff_display(snapshot);

    // Branch row — just the branch glyph + name. Totals moved to
    // the subtitle row below so the worktree-vs-vs-default
    // distinction is unambiguous (the totals follow the label that
    // describes them).
    if let Some((label, color)) = branch_row_for(snapshot) {
        lines.push(branch_line(width, &label, color));
    }

    // Subtitle row — sits directly below the branch row, indented
    // under the branch name (PANE_PAD + glyph + space = 4 cols) so
    // it reads as a sub-label of the branch. Carries the diff
    // label (`worktree` / `vs <default>`) AND the right-justified
    // `+N -M` totals so both signals land on one line. Only
    // rendered when there's a diff to describe. DIM label so it
    // doesn't compete with the branch name for attention; stats
    // keep their green / red so the numbers stay scannable.
    if let Some(diff) = diff.as_ref() {
        lines.push(diff_subtitle_line(width, &diff.subtitle, diff.totals));
    }

    // PR row — sits below the subtitle (or directly under the
    // branch row when there's no diff). Same 4-col indent so it
    // reads as another sub-label of the branch. Renders only when
    // the scanner resolved a PR for the current branch; truncates
    // the closing-issue list with `…` when the row would overflow
    // the pane width.
    if let Some(pr) = snapshot.pr.as_ref() {
        lines.push(pr_line(width, pr, &snapshot.closes));
    }

    let files_slice = diff.as_ref().map_or(&[][..], |d| d.files);
    let total_files = diff.as_ref().map_or(0, |d| d.total_files);

    if files_slice.is_empty() {
        return;
    }

    // Blank between the subtitle row and the file tree.
    lines.push(Line::default());

    let tree = build_tree(files_slice);
    render_tree(lines, &tree, width);

    // Overflow row when the trimmed top-N is shorter than the total
    // changed-files count.
    if total_files > files_slice.len() {
        let more = total_files - files_slice.len();
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
/// Whether the active session's snapshot has a non-empty diff (used
/// to gate the GIT header's `⤢` open-diff glyph + its hit target).
fn snapshot_has_diff(app: &App) -> bool {
    let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()) else {
        return false;
    };
    matches!(snapshot.view, GitDiffView::Worktree { .. } | GitDiffView::BranchVsDefault { .. })
}

fn branch_row_for(snapshot: &GitDiffSnapshot) -> Option<(String, Color)> {
    match &snapshot.branch {
        GitBranch::Named(name) => {
            let on_default = snapshot.default_branch.as_deref().is_some_and(|d| d == name.as_str());
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
/// branch-vs-default branch row look identical in chrome — the
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
/// `RUST_ORANGE` (matches the feature-branch convention — the PR is
/// the headline); everything else stays DIM. The closing-issue list
/// is truncated with `…` when it would overflow the pane width;
/// when even one issue can't fit, the whole `→ closes …` tail
/// collapses to a single `…` suffix.
fn pr_line(width: u16, pr: &GitPrInfo, closes: &[GitIssueRef]) -> Line<'static> {
    let indent = "    "; // 4 cols, mirrors diff_subtitle_line
    let pr_number = format!("#{}", pr.number);

    // Closes-list disabled when empty — just `PR #N`.
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
    // to just `…`.
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
        // Nothing fit — show `PR #N → …` so the existence of a
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
    let indent_chrome = usize::from(PANE_PAD) + 2; // "    " (4 cols)
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

/// Diff-display info pulled out of a `GitDiffSnapshot` view. `None`
/// for `CleanDefault` / `NoRepo` (nothing to show); `Some` otherwise
/// with `subtitle` carrying `"worktree"` for `Worktree` and
/// `"vs <default>"` for `BranchVsDefault`.
struct DiffDisplay<'a> {
    files: &'a [GitDiffFile],
    total_files: usize,
    totals: (u32, u32),
    subtitle: String,
}

fn build_diff_display(snapshot: &GitDiffSnapshot) -> Option<DiffDisplay<'_>> {
    match &snapshot.view {
        GitDiffView::NoRepo | GitDiffView::CleanDefault => None,
        GitDiffView::Worktree { files, total_files, total_added, total_removed } => {
            Some(DiffDisplay {
                files,
                total_files: *total_files,
                totals: (*total_added, *total_removed),
                subtitle: "worktree".to_owned(),
            })
        }
        GitDiffView::BranchVsDefault { files, total_files, total_added, total_removed } => {
            // `BranchVsDefault` is only constructed when the scanner
            // resolved a default branch (see `git_diff.rs`'s let-else
            // that collapses to `CleanDefault` when `None`). The
            // `unwrap_or_default` is defensive: future drift renders
            // an empty `vs ` instead of panicking.
            let default = snapshot.default_branch.as_deref().unwrap_or_default();
            Some(DiffDisplay {
                files,
                total_files: *total_files,
                totals: (*total_added, *total_removed),
                subtitle: format!("vs {default}"),
            })
        }
    }
}

/// Tree node built from the diff's file list — a single trie node
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
        // Empty path — ignore (defensive; the scanner doesn't emit
        // empty paths but the renderer shouldn't panic if it ever
        // does).
        return;
    };
    let rest: Vec<&str> = components.collect();
    if rest.is_empty() {
        // Leaf — attach as a file child. Duplicate file names in the
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
    // directory. (Files cannot be folded onto their parent — that
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
        // Directory row — just the label, no stats. Truncate the
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
/// leading `…` ellipsis. Preserves the tail (so the leaf component
/// of a path / filename stays visible). When `s` contains `/`
/// separators the truncation prefers to drop whole leading
/// components (yielding `…/foo/bar.rs` rather than chopping
/// mid-name); falls back to character-level head-truncation when
/// even the basename is too long. Returns the original string when
/// it already fits; collapses to `…` at `max_chars` ≤ 1.
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
    // prefixed with `…/`. Lands at a `/` boundary so the result
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
    // Even the basename overflows — fall back to char-level cut
    // so we at least preserve the trailing characters.
    let keep = max_chars - 1;
    let skip = total - keep;
    let mut out = String::from("\u{2026}");
    out.extend(s.chars().skip(skip));
    out
}

fn append_tasks_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    let todos = app.todos();
    let spinner_frame = app.spinner_frame;

    // Verification nudge row sits between the rule and the TASKS
    // header when the flag is set. Dim-yellow one-liner.
    if app.todo_verification_nudge() {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "\u{26a0} verify before declaring complete".to_owned(),
                Style::default().fg(theme::STATUS_WARNING),
            ),
        ]));
        lines.push(Line::default());
    }

    if todos.is_empty() {
        return;
    }

    // Done / total counter for the header — m is completed, n is the
    // full todo list (including hidden completed and visible
    // pending/in-progress). Reads at a glance as a progress meter.
    let total = todos.len();
    let done = todos.iter().filter(|t| t.status == TodoStatus::Completed).count();

    // TASKS section header — DIM bold, 2-col indent (matches the
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

    // Item rendering budget: full width minus the 2-col left indent,
    // the 1-col glyph, the 1-col space after the glyph, AND a 2-col
    // right gutter so truncated `…` items don't butt up against the
    // pane edge. Continuation lines for the wrapped in-progress item
    // indent under the text column (start col 5 from the pane's x=0).
    // Right-gutter reservation here mirrors the GIT section's
    // `…stats column +  PANE_PAD` math so both sections honour the
    // same visual margin.
    let glyph_indent = PANE_PAD + 2; // "  " + glyph + " "
    let text_budget = usize::from(width)
        .saturating_sub(usize::from(glyph_indent))
        .saturating_sub(usize::from(PANE_PAD));

    // Visibility tiering: show as much as fits within TASKS_MAX (5).
    //
    // 1. **Everything fits** (total <= cap): show ALL tasks in their
    //    original order, completed included. So a 3-task list with
    //    1 done + 1 in-progress + 1 pending renders all three —
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
        // Tier 1 — original order, all included.
        visible_todos = todos.iter().collect();
        hidden = 0;
    } else if non_completed.len() <= TASKS_MAX {
        // Tier 2 — completed silently hidden; m/n header conveys
        // the missing count.
        visible_todos = non_completed;
        hidden = 0;
    } else {
        // Tier 3 — non-completed itself exceeds the cap. Top
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
        // practice — the visible_todos filter strips them).
        let (glyph, glyph_color) = match todo.status {
            TodoStatus::Completed => ("\u{2713}".to_owned(), Color::Green),
            TodoStatus::InProgress => (spinner_glyph(spinner_frame), theme::RUST_ORANGE),
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
                // Empty `display_text` — still render the glyph row
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
            // Truncate with `…` at the right edge.
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
///    command or cron prompt, DIM, truncated with `…` when it
///    overflows.
/// 3. **Metadata** (`└─` continuation): kind label · status · flags,
///    all DIM.
///
/// Glyphs mirror the TASKS convention but use a kind-distinct
/// palette for the headline so scanning the section visually
/// separates "what's running" from "what's queued in TodoWrite":
///
/// - `▸` RUST_ORANGE  — `BashBackgrounded` / `Monitor` while in-flight
/// - `\u{23F0}` (`⏰`) DIM — `Cron` (scheduled, not currently firing)
/// - `\u{2713}` (`✓`) green — completed tool call (any kind)
/// - `\u{2717}` (`✗`) red — failed / killed
/// - `\u{25CB}` (`○`) DIM — pending (queued, not yet started)
fn append_processes_section(
    lines: &mut Vec<Line<'static>>,
    collection: &ProcessCollection,
    width: u16,
    spinner_frame: usize,
) {
    lines.push(Line::from(Span::styled(
        " PROCESSES".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    let include_memory = width >= PROCESSES_MEMORY_WIDTH_THRESHOLD;
    let process_count = collection.rows.len();
    for (idx, process) in collection.rows.iter().enumerate() {
        append_process_row(lines, process, width, include_memory, spinner_frame);
        // Blank between SUPERVISOR rows (depth-0). Descendant rows
        // pack tight under their supervisor with no blanks so the
        // tree reads as one connected block.
        let next_is_supervisor = collection.rows.get(idx + 1).is_some_and(|next| next.depth == 0);
        if idx + 1 < process_count && next_is_supervisor {
            lines.push(Line::default());
        }
    }

    // No `+N more` footer — the inspector pane scrolls, so the
    // scrollbar IS the overflow indicator.
}

/// Render one process row as a single line. Same shape for
/// supervisors (depth 0) and descendants (depth ≥ 1) — the only
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
/// dropped — the glyph + colour already convey kind, "running" is
/// implicit (every shown row is running), and "· 83 MB" alone is
/// enough useful detail. Cron rows still show their `Cron ·
/// recurring · session-only` metadata because they have no memory
/// to display.
fn append_process_row(
    lines: &mut Vec<Line<'static>>,
    process: &ProcessRow,
    width: u16,
    include_memory: bool,
    spinner_frame: usize,
) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(" ".repeat(usize::from(PANE_PAD))));

    // Tree chrome (ancestor cols + connector) renders at depth ≥ 1
    // only. Depth-0 rows (supervisors + Cron) start flush with
    // PANE_PAD and skip directly to the glyph.
    //
    // Skip the FIRST ancestor entry — supervisors are visualised as
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
    // Glyph rendering is depth-0 only — supervisors carry the
    // spinner/loader glyph (RUST_ORANGE for wire-matched Bash /
    // Monitor; DIM for generic OS processes); descendants drop it
    // because the tree connector + indent already says "child of
    // the row above" and adding a glyph per child clutters the
    // view. Depth-0 chrome adds 2 cols for glyph+space; depth-≥1
    // adds 0 cols past the tree connector itself.
    //
    // Suffix priority:
    // 1. Memory (`12 MB`) — when `memory_bytes` is set + layout
    //    has room. Drops the redundant `Kind · running` metadata
    //    string since glyph + colour already convey kind and
    //    "running" is implicit.
    // 2. Cron's `Cron · recurring · session-only` metadata — used
    //    when memory is `None` (Cron rows are wire-only, no
    //    backing process). The kind/recurring info IS the useful
    //    signal there.
    // 3. Nothing — `+N more` overflow rows have no memory + empty
    //    metadata, so the row is just glyph + headline.
    let (glyph, glyph_color, headline_style) = glyph_and_style_for(process, spinner_frame);
    let glyph_cols = if process.depth == 0 {
        spans.push(Span::styled(glyph, Style::default().fg(glyph_color)));
        spans.push(Span::raw(" "));
        2
    } else {
        // Descendant — glyph + space dropped. Style is reused for
        // the headline below so wire-matched-vs-unmatched colour
        // still differentiates the row.
        let _ = (glyph, glyph_color);
        0
    };

    // Suffix is the useful signal — memory for process-backed rows,
    // Cron metadata for wire-only registrations. Always include it
    // when set; the headline truncates with `…` to make room.
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
    let chrome_chars = usize::from(PANE_PAD)
        + tree_chrome_cols
        + glyph_cols
        + suffix_chars
        + usize::from(PANE_PAD); // right gutter
    let headline_budget = usize::from(width).saturating_sub(chrome_chars).max(1);
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
fn glyph_and_style_for(process: &ProcessRow, spinner_frame: usize) -> (String, Color, Style) {
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
            ProcessKind::Cron => {
                // Cron registration completes the moment claude calls
                // CronCreate — InProgress is rare. Render with the
                // schedule glyph regardless.
                (
                    "\u{23F0}".to_owned(),
                    theme::DIM,
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                )
            }
            ProcessKind::Process => {
                // Unmatched OS process — same spinner as wire-tracked
                // rows but DIM so the user's eye picks out the
                // bright-coloured matched rows first. Still animates
                // because the row IS live work.
                (spinner_glyph(spinner_frame), theme::DIM, Style::default().fg(Color::Gray))
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
            _ => (
                // Wire-matched (Bash / Monitor) — RUST_ORANGE spinner
                // so the row stands out as "tracked work" against the
                // dim spinners of generic OS processes.
                spinner_glyph(spinner_frame),
                theme::RUST_ORANGE,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        },
    }
}

/// Braille spinner frames, kept in sync with the Projects pane and
/// chat-area spinners (same sequence in `ui::projects_pane`,
/// `ui::input`, `ui::message`). The pulse is what tells the user the
/// row is alive rather than a stale snapshot.
const PROCESS_SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

fn spinner_glyph(frame: usize) -> String {
    let ch = PROCESS_SPINNER_FRAMES[frame % PROCESS_SPINNER_FRAMES.len()];
    ch.to_string()
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
            // Long single token — flush current, then hard-cut the
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
/// `…` ellipsis. Returns the original string if it already fits.
/// When `max_chars` is `0` or `1` the result is just `…`.
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
        // always starts `…/` when at least one component was dropped.
        let out = fit_path_head_truncated("~/Projects/forge/crates/forge-tui", 16);
        assert!(out.chars().count() <= 16, "got {out:?}");
        assert!(out.starts_with("\u{2026}/"), "got {out:?}");
        assert!(out.ends_with("forge-tui"), "got {out:?}");
    }

    #[test]
    fn head_truncate_drops_leading_components_first() {
        // 29-char budget — too tight for the full path, but
        // `…/src/env/git_diff.rs` (21 chars) fits cleanly at a
        // component boundary.
        let out = fit_path_head_truncated("crates/forge-agent/src/env/git_diff.rs", 29);
        assert_eq!(out, "\u{2026}/src/env/git_diff.rs");
    }

    #[test]
    fn head_truncate_basename_overflow_falls_back_to_char_cut() {
        // No `/` separators at all — has to char-cut.
        let out = fit_path_head_truncated("supercalifragilisticexpialidocious", 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.starts_with('\u{2026}'));
        assert!(out.ends_with("docious"));
    }

    #[test]
    fn head_truncate_max_one_returns_just_ellipsis() {
        assert_eq!(fit_path_head_truncated("anything", 1), "\u{2026}");
    }

    fn snap(branch: GitBranch, default: Option<&str>, view: GitDiffView) -> GitDiffSnapshot {
        GitDiffSnapshot {
            branch,
            default_branch: default.map(str::to_owned),
            view,
            pr: None,
            closes: Vec::new(),
            scanner_ok: true,
        }
    }

    #[test]
    fn branch_row_named_default_renders_dim() {
        let s = snap(GitBranch::Named("main".into()), Some("main"), GitDiffView::CleanDefault);
        let (label, color) = branch_row_for(&s).expect("named branch should render a row");
        assert_eq!(label, "main");
        assert_eq!(color, theme::DIM);
    }

    #[test]
    fn branch_row_named_feature_renders_rust_orange() {
        let s = snap(GitBranch::Named("feat/x".into()), Some("main"), GitDiffView::CleanDefault);
        let (label, color) = branch_row_for(&s).expect("feature branch should render a row");
        assert_eq!(label, "feat/x");
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn branch_row_named_unknown_default_renders_rust_orange() {
        // `default_branch == None` means we can't prove the branch IS
        // the default, so the feature-branch styling applies.
        let s = snap(GitBranch::Named("main".into()), None, GitDiffView::CleanDefault);
        let (_label, color) = branch_row_for(&s).expect("named branch should render a row");
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn branch_row_detached_renders_yellow() {
        let s = snap(GitBranch::Detached, Some("main"), GitDiffView::CleanDefault);
        let (label, color) = branch_row_for(&s).expect("detached HEAD should render a row");
        assert_eq!(label, "HEAD");
        assert_eq!(color, theme::STATUS_WARNING);
    }

    #[test]
    fn branch_row_no_repo_collapses_to_none() {
        let s = snap(GitBranch::NoRepo, None, GitDiffView::NoRepo);
        assert!(branch_row_for(&s).is_none());
    }

    #[test]
    fn branch_row_unknown_collapses_to_none() {
        let s = snap(GitBranch::Unknown, None, GitDiffView::CleanDefault);
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
        // `ui` has one child but it's a file — fold rule is dir-dir
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
        // the chrome; the closes tail should collapse to a bare `…`.
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

    fn make_process_row(
        kind: ProcessKind,
        headline: &str,
        detail: Option<&str>,
        metadata: &str,
        status: ToolCallStatus,
    ) -> ProcessRow {
        ProcessRow {
            kind,
            headline: headline.to_owned(),
            detail: detail.map(str::to_owned),
            metadata: metadata.to_owned(),
            status,
            memory_bytes: None,
            depth: 0,
            is_last_sibling: true,
            ancestor_has_more: Vec::new(),
        }
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
        // rows were retired — the glyph + colour convey kind and the
        // memory suffix is the only useful per-row signal.
        let row = make_row_with_memory(
            ProcessKind::BashBackgrounded,
            "Run unit tests",
            "Bash · running",
            8 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 40, 0);

        // header + blank + single supervisor row = 3 lines.
        assert_eq!(lines.len(), 3, "expected 3 rendered lines, got {}", lines.len());

        let header = line_text(&lines[0]);
        assert_eq!(header, " PROCESSES");
        let row_text = line_text(&lines[2]);
        // Wire-matched (Bash) → spinner glyph instead of static `▸`.
        // Frame 0 picks `⠋` (the first braille spinner frame).
        assert!(row_text.starts_with(" \u{280B} Run unit tests"), "headline: {row_text:?}");
        assert!(row_text.contains("8 MB"), "memory suffix: {row_text:?}");
        // No `Bash · running` text — that's the regression check.
        assert!(!row_text.contains("running"), "kind/running text must be dropped: {row_text:?}");
    }

    #[test]
    fn processes_section_renders_persistent_monitor_as_single_line() {
        // Monitor supervisor: same single-line shape. The `persistent`
        // flag is part of the metadata string; for Monitor rows that
        // info is conveyed via the suffix when no memory is set, or
        // dropped when memory takes the suffix slot. Test pins the
        // memory-wins path.
        let row = make_row_with_memory(
            ProcessKind::Monitor,
            "PR #120 CI watch",
            "Monitor · running · persistent",
            4 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 40, 0);

        let row_text = line_text(&lines[2]);
        // Monitor is wire-matched → spinner glyph (frame 0 = `⠋`).
        assert!(row_text.starts_with(" \u{280B} PR #120 CI watch"), "headline: {row_text:?}");
        assert!(row_text.contains("4 MB"), "memory suffix wins: {row_text:?}");
    }

    #[test]
    fn processes_section_cron_supervisor_uses_metadata_suffix() {
        // Cron rows have no memory_bytes (wire-only registrations).
        // The metadata string IS the useful signal, so the suffix
        // slot falls through to it — but only when the row width
        // can fit BOTH the headline and the suffix without
        // truncation. Test at 50 cols where everything fits.
        let row = make_process_row(
            ProcessKind::Cron,
            "*/5 * * * *",
            Some("audit memory health"),
            "Cron · recurring",
            ToolCallStatus::Completed,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 50, 0);

        let row_text = line_text(&lines[2]);
        // ✓ is the completed-status glyph; the cron clock ⏰ only
        // fires for the (rare) InProgress case.
        assert!(row_text.starts_with(" \u{2713} */5 * * * *"), "got {row_text:?}");
        assert!(row_text.contains("recurring"), "metadata suffix present: {row_text:?}");
    }

    #[test]
    fn processes_section_renders_in_progress_cron_with_clock_glyph() {
        let row = make_process_row(
            ProcessKind::Cron,
            "daily 9am",
            None,
            "Cron · recurring",
            ToolCallStatus::InProgress,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 40, 0);

        let row_text = line_text(&lines[2]);
        assert!(row_text.starts_with(" \u{23F0} daily 9am"), "got {row_text:?}");
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
        append_processes_section(&mut lines, &collection(rows), 40, 0);

        let row_text = line_text(&lines[2]);
        assert!(row_text.starts_with(" \u{2717} Run integration tests"), "got {row_text:?}");
    }

    #[test]
    fn processes_section_three_kinds_together_renders_blank_between_rows() {
        let rows = vec![
            make_row_with_memory(
                ProcessKind::BashBackgrounded,
                "Run tests",
                "Bash · running",
                8 * 1024 * 1024,
            ),
            make_row_with_memory(
                ProcessKind::Monitor,
                "Watch CI",
                "Monitor · running · persistent",
                4 * 1024 * 1024,
            ),
            make_process_row(
                ProcessKind::Cron,
                "*/5 * * * *",
                None,
                "Cron · recurring",
                ToolCallStatus::Completed,
            ),
        ];
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(rows), 40, 0);

        // header + blank + 3 single-line rows + 2 blanks between
        // supervisor rows = 2 + 3 + 2 = 7 lines.
        assert_eq!(lines.len(), 7, "expected 7 rendered lines, got {}", lines.len());
        assert!(line_text(&lines[2]).contains("Run tests"));
        assert!(line_text(&lines[4]).contains("Watch CI"));
        assert!(line_text(&lines[6]).contains("*/5 * * * *"));
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
        append_processes_section(&mut lines, &collection(vec![row]), 40, 0);

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
        // 40-col Wide-tier inspector — width above the threshold so
        // the row carries a `· 12 MB` suffix inline with the headline.
        let row = make_row_with_memory(
            ProcessKind::Process,
            "cargo",
            "Process · running",
            12 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 40, 0);

        let row_text = line_text(&lines[2]);
        assert!(row_text.contains("12 MB"), "expected memory suffix on Wide tier: {row_text:?}");
    }

    #[test]
    fn processes_section_drops_memory_suffix_at_medium_width() {
        // 30-col Medium-tier inspector — width below threshold so
        // the row stays bare (no memory suffix).
        let row = make_row_with_memory(
            ProcessKind::Process,
            "cargo",
            "Process · running",
            12 * 1024 * 1024,
        );
        let mut lines = Vec::new();
        append_processes_section(&mut lines, &collection(vec![row]), 30, 0);

        let row_text = line_text(&lines[2]);
        assert!(!row_text.contains("MB"), "expected no memory suffix on Medium tier: {row_text:?}");
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
        append_processes_section(&mut lines, &collection(vec![row]), 40, 0);

        let headline = line_text(&lines[2]);
        // Unmatched `Process` kind also renders the spinner glyph
        // (frame 0 = `⠋`) but in DIM — the colour difference vs
        // RUST_ORANGE matched rows is the "is this tracked or not"
        // signal.
        assert!(headline.starts_with(" \u{280B} cargo"), "expected spinner glyph: {headline:?}");
        // Style assertion: pull the glyph span and check its colour.
        let glyph_span = &lines[2].spans[1];
        assert_eq!(glyph_span.style.fg, Some(theme::DIM));
    }
}
