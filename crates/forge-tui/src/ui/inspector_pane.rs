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
//! - `PROCESSES` - rendered when the active session has at least
//!   one currently-in-flight long-running tool call. Three kinds
//!   surface here: backgrounded `Bash` (via `run_in_background:
//!   true` OR `assistant_auto_backgrounded`), `Monitor` streaming-
//!   process watchers, and `CronCreate` scheduled prompts. Live
//!   monitor only - completed / failed / killed rows are filtered
//!   out at the collector level so the section disappears once
//!   work wraps up. Rows are built by
//!   `crate::app::processes::collect_active_processes` from each
//!   tool call's `raw_input` + status; the renderer chooses glyphs
//!   + colours per `ProcessKind`.
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
    GitBranchAhead, GitDiffFile, GitDiffSnapshot, GitDiffStats, LayerState,
};
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

/// Render the inspector body (`GIT` → `TASKS` → `PROCESSES` ...)
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
    {
        let _t = crate::perf::start("ui::inspector_pane::append_body");
        append_body(&mut body_lines, app, body_area.width);
    }
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

    // Scrollbar - thumb-only, no rail, painted as a block cell in
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
/// scrollbar uses - same look, no movement.
fn thumb_symbol(pulse: Option<usize>) -> &'static str {
    const STATIC_THUMB: &str = "\u{2590}"; // ▐ right half block - chat baseline
    const THIN_THUMB: &str = "\u{2595}"; // ▕ right one-eighth block - pulse-out frame
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

    // #273 Task 9: WORKFLOWS section sits between TASKS and
    // MONITORS. Auto-clears once every workflow has reached
    // `Completed`.
    if !app.workflows().is_empty() {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_workflows_section(lines, app, width);
    }

    // #273 Task 8: MONITORS section sits between WORKFLOWS and
    // PROCESSES; auto-clears when no entry is live (matches the
    // TASKS section's all-completed shape).
    if !app.monitors().is_empty() {
        lines.push(Line::default());
        push_section_rule(lines, width);
        lines.push(Line::default());
        append_monitors_section(lines, app, width);
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

/// Append the GIT section to `lines`. Hidden entirely when the
/// active session's cwd is not inside a git repository (`in_repo =
/// false` + `scanner_ok = true`). For real repos the section
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
        && snapshot.scanner_ok
        && !snapshot.in_repo
    {
        return;
    }

    // Section header - DIM bold, flush against the rule above
    // (mirrors `TASKS`). When the snapshot has at least one layer
    // of diff to surface, append the `🦉` glyph at the right edge
    // as the open-diff affordance.
    let has_glyph = snapshot_has_diff(app);
    let mut header_spans = vec![Span::styled(
        " GIT".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )];
    if has_glyph {
        // " GIT" is 4 cells; the 🦉 owl is 2 cells wide; trailing
        // pad is PANE_PAD (1 cell) so the owl's right edge aligns
        // with the `-M` column on the diff-stats rows below.
        let trailing_pad = usize::from(PANE_PAD);
        let pad = usize::from(width).saturating_sub(4 + 2 + trailing_pad);
        header_spans.push(Span::raw(" ".repeat(pad)));
        header_spans.push(Span::styled("\u{1F989}".to_owned(), Style::default()));
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
    if !snapshot.scanner_ok {
        // Scanner crashed (rev-parse Failed / Oversize) and the
        // snapshot collapsed to in_repo=false as a failsafe. Without
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

/// Whether the active session's snapshot warrants the `🦉` open-diff
/// glyph in the GIT header. Two cases qualify:
/// - At least one diff layer (`worktree` / `branch_ahead`) is
///   populated - the normal "there's a diff to review" path.
/// - `scanner_ok == false` - the Inspector scanner crashed and the
///   snapshot collapsed to in_repo=false as a failsafe. The user
///   needs a way to escalate; clicking the glyph routes through
///   `open_default → DefaultTarget::ScannerFailed`, surfacing the
///   trace-target hint they need to triage.
fn snapshot_has_diff(app: &App) -> bool {
    let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()) else {
        return false;
    };
    if !snapshot.scanner_ok {
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
    let spinner_frame = app.spinner_frame;

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

    // #275 Bug 4 / Task 5: chrome accounting routed through the
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

/// #273 Task 8: append the MONITORS Inspector section. Renders one
/// row per Monitor entry with the description headline, status
/// badge, and (when expanded OR currently-running) the tail of
/// captured `task_notification.summary` lines. Section is hidden
/// entirely when `UiSession.monitors` is empty (auto-clears once
/// every entry terminates) so the Inspector doesn't carry a stale
/// "MONITORS" header with no rows.
fn append_monitors_section(lines: &mut Vec<Line<'static>>, app: &App, width: u16) {
    let monitors = app.monitors();
    if monitors.is_empty() {
        return;
    }

    lines.push(Line::from(Span::styled(
        " MONITORS".to_owned(),
        Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    let inner_width = usize::from(width);
    let last_idx = monitors.len().saturating_sub(1);
    for (idx, monitor) in monitors.iter().enumerate() {
        append_monitor_row(lines, monitor, inner_width);
        // #277 Bug 6: blank between entries so multiple Monitor
        // rows don't visually crowd together. Skip after the last
        // row to avoid leaving a trailing blank at the end of the
        // section (matches TASKS' inter-entry spacing).
        if idx < last_idx {
            lines.push(Line::default());
        }
    }
}

/// Render one Monitor entry into the Inspector body. Layout:
/// headline row (glyph + description + status badge + persistent
/// flag), tail rows (`│   └─ <line>` style continuation) when
/// expanded or running.
fn append_monitor_row(
    lines: &mut Vec<Line<'static>>,
    monitor: &crate::app::MonitorEntry,
    inner_width: usize,
) {
    use crate::app::MonitorStatus;

    let (status_label, status_color) = match monitor.status {
        MonitorStatus::Running => ("running", theme::RUST_ORANGE),
        MonitorStatus::Completed => ("completed", Color::Green),
        MonitorStatus::Stopped => ("stopped", theme::DIM),
        MonitorStatus::TimedOut => ("timed out", theme::STATUS_WARNING),
    };
    let glyph = if monitor.is_running() { "\u{25c9}" } else { "\u{25cd}" };
    let glyph_color = if monitor.is_running() { theme::RUST_ORANGE } else { theme::DIM };
    let persistent_suffix = if monitor.persistent { " \u{00B7} persistent" } else { "" };

    // #275 Bug 4: chrome accumulator for the header row. The status
    // badge + persistent suffix are appended AFTER the truncated
    // headline, so the budget must count them up-front. Without this
    // accounting the badge overflows the pane and ratatui clips
    // it - which read as "no status badge ever appears" to the
    // user.
    let header_chrome = usize::from(PANE_PAD)           // left indent (matches TASKS)
        + 1                                              // glyph cell
        + 1                                              // space after glyph
        + 3                                              // " · " separator
        + status_label.chars().count()                   // status badge text
        + persistent_suffix.chars().count()              // optional " · persistent"
        + usize::from(PANE_PAD); // 1-col right gutter
    let header_budget = row_text_budget(inner_width, header_chrome);

    let headline = truncate_or_pass(&monitor.description, header_budget);
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(usize::from(PANE_PAD))),
        Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
        Span::raw(" ".to_owned()),
        Span::styled(headline, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(" \u{00B7} ".to_owned(), Style::default().fg(theme::DIM)),
        Span::styled(status_label.to_owned(), Style::default().fg(status_color)),
        Span::styled(persistent_suffix.to_owned(), Style::default().fg(theme::DIM)),
    ]));

    // Tail lines - show when:
    //  - the monitor is still running (live tail), OR
    //  - the user has explicitly expanded the entry, OR
    //  - #277 Bug 5b: the tail is non-empty for a completed entry.
    //    Without this branch, completed Monitors that landed their
    //    `task_notification.output_file` tail are hidden (the tail
    //    was the whole point of the file read). The empty-tail
    //    case still short-circuits above so silent-completed
    //    Monitors don't get a vestigial expanded view.
    if monitor.output_tail.is_empty() {
        return;
    }
    let show_tail =
        monitor.is_running() || monitor.expanded_in_inspector || !monitor.output_tail.is_empty();
    if !show_tail {
        return;
    }
    // Tail-row chrome: 1-col indent + box-drawing connector + space
    // + 1-col right gutter. Same budget for every row regardless of
    // whether the connector is `└` (last) or `├` (mid).
    let tail_chrome = usize::from(PANE_PAD)
        + 1   // connector glyph (└ or ├)
        + 1   // space after connector
        + usize::from(PANE_PAD);
    let tail_budget = row_text_budget(inner_width, tail_chrome);
    let last_idx = monitor.output_tail.len().saturating_sub(1);
    for (i, line) in monitor.output_tail.iter().enumerate() {
        let connector_glyph = if i == last_idx { "\u{2514}" } else { "\u{251c}" };
        let truncated = truncate_or_pass(line, tail_budget);
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(usize::from(PANE_PAD))),
            Span::styled(connector_glyph.to_owned(), Style::default().fg(theme::DIM)),
            Span::raw(" ".to_owned()),
            Span::styled(truncated, Style::default().fg(theme::DIM)),
        ]));
    }
}

/// #273 Task 9: append the WORKFLOWS Inspector section. Header +
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
        append_workflow_row(lines, workflow, inner_width, app.spinner_frame);
        // #277 Bug 6: blank between entries (matches the MONITORS
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
    spinner_frame: usize,
) {
    use crate::app::{PhaseStatus, WorkflowStatus};

    let (status_label, status_color) = match workflow.status {
        WorkflowStatus::InProgress => ("in progress", theme::RUST_ORANGE),
        WorkflowStatus::Completed => ("done", Color::Green),
    };
    let glyph =
        if workflow.is_in_progress() { spinner_frame_char(spinner_frame) } else { "\u{25c6}" };
    let glyph_color = if workflow.is_in_progress() { theme::RUST_ORANGE } else { Color::Green };

    // #275 Bug 4: same shape as MONITORS header. Badge follows
    // truncated text; count it in chrome up-front.
    let header_chrome = usize::from(PANE_PAD)
        + 1   // glyph
        + 1   // space
        + 3   // " · "
        + status_label.chars().count()
        + usize::from(PANE_PAD);
    let header_budget = row_text_budget(inner_width, header_chrome);
    let header_text = truncate_or_pass(&workflow.meta_name, header_budget);
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(usize::from(PANE_PAD))),
        Span::styled(glyph.to_owned(), Style::default().fg(glyph_color)),
        Span::raw(" ".to_owned()),
        Span::styled(header_text, Style::default().add_modifier(Modifier::BOLD)),
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
                PhaseStatus::Completed => ("\u{2713}", Color::Green),
                PhaseStatus::InProgress => (spinner_frame_char(spinner_frame), theme::RUST_ORANGE),
                PhaseStatus::Pending => ("\u{25CB}", theme::DIM),
            };
            let row = truncate_or_pass(&phase.title, phase_budget);
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(usize::from(PANE_PAD))),
                Span::styled(connector_glyph.to_owned(), Style::default().fg(theme::DIM)),
                Span::raw(" ".to_owned()),
                Span::styled(phase_glyph.to_owned(), Style::default().fg(phase_color)),
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

/// #275 Bug 4 / Task 5: single source of truth for inspector-row
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

/// Pick the active spinner frame character from the shared SPINNER
/// table. Exposed as a tiny helper so both monitor + workflow rows
/// use the same animation.
fn spinner_frame_char(frame: usize) -> &'static str {
    const FRAMES: &[&str] = &[
        "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}",
        "\u{2827}", "\u{2807}", "\u{280F}",
    ];
    FRAMES[frame % FRAMES.len()]
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
/// separates "what's running" from "what's queued in TodoWrite":
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
    spinner_frame: usize,
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
    let (glyph, glyph_color, headline_style) = glyph_and_style_for(process, spinner_frame);
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
    // #275 Bug 4 / Task 5: every inspector row routes its chrome
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
                // CronCreate - InProgress is rare. Render with the
                // schedule glyph regardless.
                (
                    "\u{23F0}".to_owned(),
                    theme::DIM,
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                )
            }
            ProcessKind::Process => {
                // Unmatched OS process - same spinner as wire-tracked
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
            ProcessKind::BashBackgrounded => (
                // Wire-matched Bash - RUST_ORANGE spinner so the row
                // stands out as "tracked work" against the dim
                // spinners of generic OS processes. (#273 Task 8
                // retired Monitor from PROCESSES, so this branch is
                // exclusively Bash.)
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
            in_repo,
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: None,
            closes: Vec::new(),
            scanner_ok: true,
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
        // rows were retired - the glyph + colour convey kind and the
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
        // No `Bash · running` text - that's the regression check.
        assert!(!row_text.contains("running"), "kind/running text must be dropped: {row_text:?}");
    }

    #[test]
    fn processes_section_cron_supervisor_uses_metadata_suffix() {
        // Cron rows have no memory_bytes (wire-only registrations).
        // The metadata string IS the useful signal, so the suffix
        // slot falls through to it - but only when the row width
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
    fn processes_section_two_kinds_together_renders_blank_between_rows() {
        // #273 Task 8 retired Monitor from PROCESSES - verify the
        // Bash + Cron pair still renders with a separating blank
        // between rows. Monitor rows are now surfaced by the
        // dedicated MONITORS section instead.
        let rows = vec![
            make_row_with_memory(
                ProcessKind::BashBackgrounded,
                "Run tests",
                "Bash · running",
                8 * 1024 * 1024,
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

        // header + blank + 2 single-line rows + 1 blank between = 5 lines.
        assert_eq!(lines.len(), 5, "expected 5 rendered lines, got {}", lines.len());
        assert!(line_text(&lines[2]).contains("Run tests"));
        assert!(line_text(&lines[4]).contains("*/5 * * * *"));
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
        // 40-col Wide-tier inspector - width above the threshold so
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
        // 30-col Medium-tier inspector - width below threshold so
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

    // ---------------------------------------------------------
    // #273 Task 8: MONITORS Inspector section.
    // ---------------------------------------------------------

    fn make_monitor_entry(
        tool_use_id: &str,
        description: &str,
        persistent: bool,
        status: crate::app::MonitorStatus,
    ) -> crate::app::MonitorEntry {
        crate::app::MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some("task_1".to_owned()),
            description: description.to_owned(),
            command: "tail -F app.log".to_owned(),
            persistent,
            timeout_ms: 0,
            status,
            output_file: None,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        }
    }

    #[test]
    fn monitors_section_renders_running_row_with_persistent_badge() {
        let entry = make_monitor_entry(
            "tu",
            "forge-monitor-test",
            true,
            crate::app::MonitorStatus::Running,
        );
        let mut lines = Vec::new();
        // #275 Task 5: pass inner_width (full pane), not text_budget.
        // The 1-col left indent matches TASKS / PROCESSES.
        append_monitor_row(&mut lines, &entry, 60);
        let row_text = line_text(&lines[0]);
        assert!(row_text.starts_with(" \u{25c9}"), "expected fisheye glyph; got {row_text:?}");
        assert!(row_text.contains("forge-monitor-test"), "headline missing; got {row_text:?}");
        assert!(row_text.contains("running"), "status badge missing; got {row_text:?}");
        assert!(row_text.contains("persistent"), "persistent badge missing; got {row_text:?}");
    }

    #[test]
    fn monitors_section_renders_stopped_row_with_dim_glyph() {
        let entry = make_monitor_entry("tu", "ci-watch", false, crate::app::MonitorStatus::Stopped);
        let mut lines = Vec::new();
        append_monitor_row(&mut lines, &entry, 60);
        let row_text = line_text(&lines[0]);
        // Terminal glyph (◍ U+25CD) for stopped entries.
        assert!(row_text.contains("\u{25cd}"), "expected stopped glyph; got {row_text:?}");
        assert!(row_text.contains("stopped"), "stopped badge missing; got {row_text:?}");
        // Non-persistent has no `persistent` badge.
        assert!(!row_text.contains("persistent"));
    }

    #[test]
    fn monitors_section_renders_output_tail_for_running_entry() {
        let mut entry = make_monitor_entry(
            "tu",
            "forge-monitor-test",
            true,
            crate::app::MonitorStatus::Running,
        );
        entry.output_tail.push_back("stream started".to_owned());
        entry.output_tail.push_back("first event landed".to_owned());
        let mut lines = Vec::new();
        append_monitor_row(&mut lines, &entry, 60);
        assert_eq!(lines.len(), 3, "headline + 2 tail rows; got {}", lines.len());
        assert!(line_text(&lines[1]).contains("stream started"));
        assert!(line_text(&lines[2]).contains("first event landed"));
    }

    #[test]
    fn monitors_section_shows_tail_for_stopped_entry_with_populated_tail() {
        // #277 Bug 5b: the prior gate hid the tail for any
        // non-running, non-expanded entry. After the relaxation,
        // a completed Monitor that has captured an output_tail
        // (from `task_notification.output_file`) renders its tail
        // by default - that's the whole point of capturing the
        // file contents. The expanded flag still surfaces the
        // tail for empty-or-otherwise edge cases.
        let mut entry =
            make_monitor_entry("tu", "ci-watch", false, crate::app::MonitorStatus::Stopped);
        entry.output_tail.push_back("stream ended".to_owned());
        let mut lines = Vec::new();
        append_monitor_row(&mut lines, &entry, 60);
        // Stopped + collapsed + non-empty tail → tail renders.
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[1]).contains("stream ended"));
    }

    #[test]
    fn monitors_section_hides_tail_for_stopped_entry_with_empty_tail() {
        // Empty `output_tail` short-circuits before the show_tail
        // predicate runs; ensure the no-output path still produces
        // just the headline (no vestigial empty-body row).
        let entry = make_monitor_entry("tu", "ci-watch", false, crate::app::MonitorStatus::Stopped);
        // No `output_tail` pushed.
        let mut lines = Vec::new();
        append_monitor_row(&mut lines, &entry, 60);
        assert_eq!(lines.len(), 1, "no tail content → headline only");
    }

    #[test]
    fn monitors_section_shows_tail_for_expanded_stopped_entry() {
        let mut entry =
            make_monitor_entry("tu", "ci-watch", false, crate::app::MonitorStatus::Stopped);
        entry.output_tail.push_back("stream ended".to_owned());
        entry.expanded_in_inspector = true;
        let mut lines = Vec::new();
        append_monitor_row(&mut lines, &entry, 40);
        // Expanded → headline + 1 tail row.
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[1]).contains("stream ended"));
    }

    // ---------------------------------------------------------
    // #273 Task 9: WORKFLOWS Inspector section.
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
        append_workflow_row(&mut lines, &workflow, 60, 0);
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
        append_workflow_row(&mut lines, &workflow, 60, 0);
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
        append_workflow_row(&mut lines, &workflow, 60, 0);
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
        append_workflow_row(&mut lines, &workflow, 60, 0);
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
        append_processes_section(&mut lines, &collection(vec![row]), 40, 0);

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
    // #275 Bug 4 / Task 5: architectural gutter-consistency
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

        // MONITORS header + tail rows (running, persistent, with output).
        let mut monitor = make_monitor_entry(
            "tu",
            "A super long Monitor description that overflows the pane width by a lot",
            true,
            crate::app::MonitorStatus::Running,
        );
        monitor.output_tail.push_back(
            "An equally long tail line that also overflows the pane width here too".to_owned(),
        );
        let mut mon_lines = Vec::new();
        append_monitor_row(&mut mon_lines, &monitor, inner_width);
        all_rows.extend(mon_lines);

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
        append_workflow_row(&mut wf_lines, &workflow, inner_width, 0);
        all_rows.extend(wf_lines);

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
    // #277 Bug 6: blank-line spacing between MONITORS / WORKFLOWS
    // entries.
    // ---------------------------------------------------------

    fn build_session_with_monitors(monitors: Vec<crate::app::MonitorEntry>) -> App {
        let mut app = App::test_default();
        *app.monitors_mut() = monitors;
        app
    }

    fn build_session_with_workflows(workflows: Vec<crate::app::WorkflowEntry>) -> App {
        let mut app = App::test_default();
        *app.workflows_mut() = workflows;
        app
    }

    #[test]
    fn monitors_section_inserts_blank_line_between_entries() {
        let entries = vec![
            make_monitor_entry("tu_a", "first-monitor", false, crate::app::MonitorStatus::Running),
            make_monitor_entry("tu_b", "second-monitor", false, crate::app::MonitorStatus::Running),
        ];
        let app = build_session_with_monitors(entries);
        let mut lines = Vec::new();
        append_monitors_section(&mut lines, &app, 60);
        // Expected layout:
        //   0: " MONITORS"
        //   1: blank (header -> first-row separator)
        //   2: first row headline
        //   3: blank (between entries - Bug 6)
        //   4: second row headline
        // No trailing blank after the last entry.
        assert_eq!(lines.len(), 5, "got {} lines: {lines:?}", lines.len());
        assert!(line_text(&lines[0]).contains("MONITORS"));
        assert!(line_text(&lines[1]).is_empty());
        assert!(line_text(&lines[2]).contains("first-monitor"));
        assert!(line_text(&lines[3]).is_empty(), "Bug 6: blank between entries");
        assert!(line_text(&lines[4]).contains("second-monitor"));
    }

    #[test]
    fn monitors_section_single_entry_has_no_trailing_blank() {
        let app = build_session_with_monitors(vec![make_monitor_entry(
            "tu_solo",
            "solo-monitor",
            false,
            crate::app::MonitorStatus::Running,
        )]);
        let mut lines = Vec::new();
        append_monitors_section(&mut lines, &app, 60);
        // 1 header + 1 blank + 1 row = 3 lines; no trailing blank.
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[2]).contains("solo-monitor"));
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
        // Layout shape matches MONITORS: header + blank + row + blank-between + row.
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
}
