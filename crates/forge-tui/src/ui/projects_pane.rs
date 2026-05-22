//! Projects pane (left side, Wide + Medium tiers).
//!
//! Renders projects from
//! [`forge_workspace::Workspace::list_projects`] with the active
//! project highlighted; the active project's sessions drill down
//! immediately under its row. Each row stamps a [`PaneHitTarget`]
//! into [`App::pane_hit_targets`] for the mouse handler (next
//! commit) to read on click events.
//!
//! Width handling: project + session labels are head-truncated with
//! a trailing `…` when they overflow the available row width. At
//! Wide tier (26ch) truncation is rare; at Medium tier (20ch) it is
//! routine. Hit-target stamps always carry the *un-truncated*
//! identifier so click routing keeps working regardless of
//! truncation.

use std::time::{Instant, SystemTime};

use forge_primitives::PeerInflightStats;
use forge_workspace::ProjectView;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::app::App;
use crate::app::PaneHitTarget;
use crate::app::session::SessionLifecycleState;

/// Render the Projects pane into `area`. Takes `projects` as a slice
/// (rather than reaching into `app.workspace`) so unit tests can
/// pass synthetic fixtures without spinning up a real `Workspace`.
///
/// Used at Wide and Medium tiers where the pane is inline. The
/// Narrow-tier overlay reuses the shared row-building helper via
/// [`render_overlay`].
pub fn render(frame: &mut Frame, area: Rect, app: &mut App, projects: &[ProjectView]) {
    app.pane_hit_targets.clear();

    // Three anchored regions:
    //   - top 2 rows: `PROJECTS` banner + DIM rule (static)
    //   - middle:     project list (scrollable)
    //   - bottom:     account / status footer (static; reserves
    //                 `ACCOUNT_PANEL_HEIGHT` rows when the pane is
    //                 tall enough).
    let panel_reserved = render_account_status_footer(frame, area, app);
    let head_rows: u16 = 2;
    let list_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(panel_reserved),
    };

    render_pane_head(frame, list_area);

    let body_area = Rect {
        x: list_area.x,
        y: list_area.y.saturating_add(head_rows),
        width: list_area.width,
        height: list_area.height.saturating_sub(head_rows),
    };
    render_pane_body(frame, body_area, app, projects);
}

/// Paint the pinned `PROJECTS` banner + DIM rule on the top 2 rows
/// of the pane. Shared by the inline and overlay renderers — the
/// overlay variant overrides the banner via its caller; only the
/// rule is the same.
fn render_pane_head(frame: &mut Frame, area: Rect) {
    let head_rows = 2u16.min(area.height);
    if head_rows == 0 {
        return;
    }
    let mut head_lines: Vec<Line<'static>> = Vec::with_capacity(2);
    head_lines.push(Line::from(Span::styled(
        " PROJECTS".to_owned(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )));
    if head_rows >= 2 {
        let rule_width = usize::from(area.width.saturating_sub(2));
        head_lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("─".repeat(rule_width), Style::default().fg(theme::DIM)),
        ]));
    }
    let head_area = Rect { x: area.x, y: area.y, width: area.width, height: head_rows };
    frame.render_widget(Paragraph::new(head_lines), head_area);
}

/// Render the scrollable project list. Builds every row into a
/// `Vec<Line>`, then renders the Paragraph with the clamped scroll
/// offset stamped onto `App.projects_pane_scroll_offset`. The thumb
/// on the right edge mirrors the inspector pane's `▐` style so the
/// two panes look consistent.
fn render_pane_body(frame: &mut Frame, area: Rect, app: &mut App, projects: &[ProjectView]) {
    if area.height == 0 || area.width == 0 {
        app.rendered_projects_pane_body_area = Rect::default();
        return;
    }
    app.rendered_projects_pane_body_area = area;

    let mut body_lines: Vec<Line<'static>> = Vec::new();
    let body_hit_target_start = app.pane_hit_targets.len();
    append_project_rows(&mut body_lines, area, app, projects);
    let total = body_lines.len();
    let visible = usize::from(area.height);
    let max_offset = total.saturating_sub(visible);
    let max_offset_u16 = u16::try_from(max_offset).unwrap_or(u16::MAX);
    let offset = app.projects_pane_scroll_offset.min(max_offset_u16);
    app.projects_pane_scroll_offset = offset;

    // Translate body-local hit-target y coords by the scroll offset.
    // Targets stamped during `append_project_rows` use absolute y =
    // `body_area.y + line_idx` (with no scroll knowledge); after
    // scrolling, the line at that screen y is actually `offset` rows
    // higher. Subtracting `offset` re-aligns hit-tests with the
    // painted rows. Targets that would land above `body_area.y`
    // (scrolled off the top) get their `height` zeroed so the
    // `contains_y` test refuses them — the click would otherwise
    // register on whatever sits above the body (the static banner /
    // rule), wrongly switching projects.
    if offset > 0 {
        shift_body_hit_targets(app, body_hit_target_start, area.y, offset);
    }

    frame.render_widget(Paragraph::new(body_lines).scroll((offset, 0)), area);

    if total > visible {
        render_projects_scroll_thumb(frame, area, total, visible, offset);
    }
}

fn shift_body_hit_targets(app: &mut App, start_idx: usize, body_top: u16, offset: u16) {
    for target in app.pane_hit_targets.iter_mut().skip(start_idx) {
        match target {
            crate::app::PaneHitTarget::ProjectHeader { y, height, .. }
            | crate::app::PaneHitTarget::SessionRow { y, height, .. }
            | crate::app::PaneHitTarget::CloseSession { y, height, .. }
            | crate::app::PaneHitTarget::CloseWorker { y, height, .. }
            | crate::app::PaneHitTarget::WorkerRow { y, height, .. } => {
                if let Some(new_y) = y.checked_sub(offset) {
                    if new_y < body_top {
                        *height = 0;
                    } else {
                        *y = new_y;
                    }
                } else {
                    *height = 0;
                }
            }
            crate::app::PaneHitTarget::TopBarIcon { .. }
            | crate::app::PaneHitTarget::InspectorTopBarIcon { .. }
            | crate::app::PaneHitTarget::OverlayClose { .. }
            | crate::app::PaneHitTarget::InspectorGitOpenDiff { .. }
            | crate::app::PaneHitTarget::CopySessionId { .. } => {}
        }
    }
}

/// Paint the projects-pane scroll thumb on the right edge of the
/// body area. Mirrors the chat + inspector thumbs: a single `▐` cell
/// in `ROLE_ASSISTANT` colour. Thumb size is clamped to 1 (same as
/// `INSPECTOR_THUMB_MAX_CELLS` / `SCROLLBAR_MAX_THUMB_HEIGHT`) so the
/// three scrollbars look identical at a glance regardless of how
/// short the body is; `thumb_top` is recomputed against the
/// post-clamp track so the cell still slides across the full
/// vertical range while scrolling.
fn render_projects_scroll_thumb(
    frame: &mut Frame,
    body_area: Rect,
    total: usize,
    visible: usize,
    offset: u16,
) {
    if crate::app::compute_scrollbar_geometry(total, visible, f32::from(offset)).is_none() {
        return;
    }
    let area_h = usize::from(body_area.height);
    let max_offset = total.saturating_sub(visible);
    let track = area_h.saturating_sub(1); // post-clamp: thumb is 1 cell
    let thumb_top_usize = if max_offset == 0 || track == 0 {
        0
    } else {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let pos = (f32::from(offset) / max_offset as f32 * track as f32).round() as usize;
        pos.min(track)
    };
    let thumb_x = body_area.x.saturating_add(body_area.width).saturating_sub(1);
    let y = body_area.y.saturating_add(u16::try_from(thumb_top_usize).unwrap_or(u16::MAX));
    if y >= body_area.y.saturating_add(body_area.height) {
        return;
    }
    let mut cell = ratatui::buffer::Cell::new("▐");
    cell.set_style(Style::default().fg(theme::ROLE_ASSISTANT));
    if let Some(buf_cell) = frame.buffer_mut().cell_mut((thumb_x, y)) {
        *buf_cell = cell;
    }
}

/// Render the Narrow-tier full-screen Projects overlay into `area`.
/// Shares the row-building loop with the inline [`render`] path,
/// wrapped in an overlay-specific banner with a `▤ PROJECTS` label
/// on the left and a `✕` glyph on the right (stamped as
/// [`PaneHitTarget::OverlayClose`] for the click handler).
///
/// Picking a project / session row inside the overlay calls the
/// same `switch_*` paths the inline pane uses, plus the click
/// handler closes the overlay. Mouse-only — no keyboard navigation.
pub fn render_overlay(frame: &mut Frame, area: Rect, app: &mut App, projects: &[ProjectView]) {
    app.pane_hit_targets.clear();

    // Same two-region split as the inline pane — account panel docked
    // at the bottom of the overlay, project list takes the rest.
    let panel_reserved = render_account_status_footer(frame, area, app);
    let list_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(panel_reserved),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Banner row: `▤ PROJECTS … ✕` spanning the full overlay width.
    let banner_label = "▤ PROJECTS";
    let close_glyph = "✕";
    let banner_chars = banner_label.chars().count();
    let close_chars = close_glyph.chars().count();
    let pad = usize::from(list_area.width).saturating_sub(banner_chars).saturating_sub(close_chars);
    lines.push(Line::from(vec![
        Span::styled(
            banner_label.to_owned(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(close_glyph.to_owned(), Style::default().fg(theme::DIM)),
    ]));
    // Stamp ✕ hit-target — last char on the banner row.
    let close_x_start = list_area
        .x
        .saturating_add(list_area.width)
        .saturating_sub(u16::try_from(close_chars).unwrap_or(1));
    let close_x_end = list_area.x.saturating_add(list_area.width);
    app.pane_hit_targets.push(PaneHitTarget::OverlayClose {
        y: list_area.y,
        height: 1,
        x_start: close_x_start,
        x_end: close_x_end,
    });

    // Dim rule under the banner.
    let rule_width = usize::from(list_area.width);
    lines.push(Line::from(Span::styled("─".repeat(rule_width), Style::default().fg(theme::DIM))));
    lines.push(Line::default());

    append_project_rows(&mut lines, list_area, app, projects);

    frame.render_widget(Paragraph::new(lines), list_area);
}

/// Org-grouped project list. Projects render as tree-leaf rows
/// under their org's header (DIM bold). Within each org, projects
/// sort alphabetically; orgs themselves sort alphabetically. The
/// per-row glyph distinguishes live sessions (spinner — RUST_ORANGE
/// when focused, terminal-default otherwise) from idle catalog
/// entries (`○` DIM). Live rows carry a `⏻` close affordance at
/// the right edge; idle rows show last-activity timestamp instead.
///
/// Tree connectors mirror the GIT / PROCESSES sections (`├─` /
/// `└─`) so the inspector + projects pane read as one consistent
/// visual language across the workspace.
type LiveRowMeta = (forge_workspace::SessionKey, SessionLifecycleState, bool, PeerBadgeInput);
type RowMeta<'p> = (&'p ProjectView, Option<LiveRowMeta>);

/// Snapshot of the peer-activity counters + last-failure timestamp
/// captured from the row's `UiSession` bucket before row rendering.
/// Passed into [`append_org_project_row`] so the badge spans can be
/// inlined without re-borrowing `app.sessions`.
#[derive(Clone, Default)]
struct PeerBadgeInput {
    stats: PeerInflightStats,
    last_failure_at: Option<Instant>,
}

fn append_project_rows(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    app: &mut App,
    projects: &[ProjectView],
) {
    let active_session_key = app.active_session_key.clone();
    let spinner_frame = app.spinner_frame;
    let lifecycle_for = |key: &forge_workspace::SessionKey| -> SessionLifecycleState {
        app.sessions.get(key).map_or(SessionLifecycleState::default(), |s| s.lifecycle_state)
    };
    let badges_for = |key: &forge_workspace::SessionKey| -> PeerBadgeInput {
        app.sessions.get(key).map_or_else(PeerBadgeInput::default, |s| PeerBadgeInput {
            stats: s.peer_badges.clone(),
            last_failure_at: s.peer_badges_last_failure_at,
        })
    };

    // Bucket projects by org name. Each bucket is a Vec of
    // (project, optional live session metadata + peer badges).
    let mut by_org: std::collections::BTreeMap<String, Vec<RowMeta<'_>>> =
        std::collections::BTreeMap::new();
    for project in projects {
        let spawn_synthetic =
            forge_workspace::SessionKey::from_session_id(format!("__spawn_{}__", project.name));
        let live_session = project.sessions.iter().find_map(|s| {
            app.sessions.get(&s.session).map(|_| (s.session.clone(), lifecycle_for(&s.session)))
        });
        let synthetic = app
            .sessions
            .get(&spawn_synthetic)
            .map(|_| (spawn_synthetic.clone(), lifecycle_for(&spawn_synthetic)));
        let live = live_session.or(synthetic).map(|(key, lifecycle)| {
            let is_focused = Some(&key) == active_session_key.as_ref();
            let badges = badges_for(&key);
            (key, lifecycle, is_focused, badges)
        });
        by_org.entry(project.org.clone()).or_default().push((project, live));
    }
    // Alphabetical project order within each org for deterministic
    // ordering across refreshes.
    for bucket in by_org.values_mut() {
        bucket.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    }

    let now = SystemTime::now();
    let org_count = by_org.len();
    for (org_idx, (org_name, rows)) in by_org.iter().enumerate() {
        // Org header row — DIM bold, no tree chrome.
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                org_name.clone(),
                Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
            ),
        ]));
        // `│  ` continuation so the header visually links down to
        // the first project row's connector instead of floating
        // disconnected above an empty gap.
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("\u{2502}  ".to_owned(), Style::default().fg(theme::DIM)),
        ]));

        let row_count = rows.len();
        for (idx, (project, live)) in rows.iter().enumerate() {
            let is_last = idx + 1 == row_count;
            append_org_project_row(
                lines,
                area,
                app,
                project,
                live.as_ref(),
                is_last,
                spinner_frame,
                now,
            );
            append_worker_tree_children(lines, area, app, project);
            // Deadzone gap row between adjacent projects in the
            // same org — emits the `│  ` tree continuation so the
            // connector lines visually link across the breathing
            // gap rather than breaking into floating fragments.
            // Skipped after the last project in the org — the
            // org-separator blanks take its place.
            if !is_last {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled("\u{2502}  ".to_owned(), Style::default().fg(theme::DIM)),
                ]));
            }
        }

        // Two blanks between orgs for a visible section break.
        // Skipped after the last org so no trailing whitespace.
        if org_idx + 1 < org_count {
            lines.push(Line::default());
            lines.push(Line::default());
        }
    }
}

/// Render one project row under an org header. Tree connector
/// (`├─` or `└─`) sits at column 2; the live/idle glyph + name +
/// trailing close-affordance (or relative time) fill the rest of
/// the row. Hit targets are stamped relative to `area.y` + the
/// current line count.
fn append_org_project_row(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    app: &mut App,
    project: &ProjectView,
    live: Option<&LiveRowMeta>,
    is_last: bool,
    spinner_frame: usize,
    now: SystemTime,
) {
    let row_y = area.y + line_count_as_u16(lines);
    let connector = if is_last { "\u{2514}\u{2500} " } else { "\u{251C}\u{2500} " };
    let total_name_budget = name_budget_org_row(area.width);

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(" "));
    spans.push(Span::styled(connector.to_owned(), Style::default().fg(theme::DIM)));

    if let Some((session_key, lifecycle, is_focused, badge_input)) = live {
        let (glyph, glyph_color) = glyph_for_lifecycle(*lifecycle, *is_focused, spinner_frame);
        let name_style = if *is_focused {
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        // Reserve room for the peer-activity badge cluster between
        // the name and the close button so the badges sit flush
        // against the right column rather than off-screen on narrow
        // panes.
        let (badge_spans, badge_width) =
            peer_badge_spans(&badge_input.stats, badge_input.last_failure_at, Instant::now());
        let name_budget = total_name_budget.saturating_sub(badge_width);
        let label = truncate_with_ellipsis(project.name.as_str(), name_budget);
        let label_pad = name_budget.saturating_sub(label.chars().count());
        spans.push(Span::styled(glyph, Style::default().fg(glyph_color)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(label, name_style));
        spans.push(Span::raw(" ".repeat(label_pad)));
        // Peer-activity badges (·N↑ outgoing, ·N↓ incoming, ·N⌛
        // timed-out, ·N✕ delivery-failed). Failure badges fade after
        // 60 s so the sidebar doesn't stay red after a single hiccup.
        spans.extend(badge_spans);
        // 1-col separator before the button — matches the 1-col
        // separator before the `time` column on idle rows.
        spans.push(Span::raw(" "));
        // Close affordance: ` x ` 3-cell button on `USER_MSG_BG`
        // slate. 1-col bg-pad on each side of the lowercase `x`
        // glyph so the button reads as a proper rectangular
        // affordance with breathing room rather than a bare 1-cell
        // letter. Right edge of the button sits at row_right - 3,
        // exactly where the idle row's last time char ends, so
        // active + idle row right edges stay flush.
        spans.push(Span::styled(
            " x ".to_owned(),
            Style::default().fg(Color::Gray).bg(theme::USER_MSG_BG).add_modifier(Modifier::BOLD),
        ));
        // 1-col right gutter — matches the inspector pane's GIT
        // section right edge AND the idle row's 1-col gutter.
        spans.push(Span::raw(" "));
        lines.push(Line::from(spans));
        // Hit targets: whole row → focus/switch; button + 1-col
        // tolerance each side → close session.
        app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
            project_name: project.key.as_str().to_owned(),
            y: row_y,
            height: 1,
        });
        let row_right = area.x.saturating_add(area.width);
        // Close button: the ` x ` 3-cell span occupies
        // (row_right - 4) to (row_right - 2). 5-col hit band runs
        // (row_right - 5) to (row_right - 1) for 1-col tolerance
        // each side; the rightmost gutter col stays inert.
        let close_x_start = row_right.saturating_sub(5);
        let close_x_end = row_right.saturating_sub(1);
        app.pane_hit_targets.push(PaneHitTarget::CloseSession {
            session_key: session_key.clone(),
            y: row_y,
            height: 1,
            x_start: close_x_start,
            x_end: close_x_end,
        });
    } else {
        // Idle (no live session) — `○` DIM glyph, name in DIM, last
        // activity time at the right edge in DIM. No close
        // affordance since there's nothing to close. Adds a 1-col
        // pad after `time` so the date column aligns with the
        // emoji column on active rows (emoji = 2 cells; time = 3
        // chars; the 1-col pad here equalises them in the same x
        // position). Plus the standard 1-col right gutter.
        //
        // No badge column on idle rows — peer in-flight state lives
        // on the live `UiSession` bucket, and a sleeping project has
        // no bucket to read from. Full width goes to the name.
        let label = truncate_with_ellipsis(project.name.as_str(), total_name_budget);
        let label_pad = total_name_budget.saturating_sub(label.chars().count());
        let time =
            format_relative_time(project.sessions.first().and_then(|s| s.last_activity), now);
        spans.push(Span::styled("\u{25CB}".to_owned(), Style::default().fg(theme::DIM)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(label, Style::default().fg(theme::DIM)));
        spans.push(Span::raw(" ".repeat(label_pad)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(time, Style::default().fg(theme::DIM)));
        spans.push(Span::raw(" "));
        lines.push(Line::from(spans));
        app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
            project_name: project.key.as_str().to_owned(),
            y: row_y,
            height: 1,
        });
    }
}

/// Append worker tree-children rows immediately below a project's
/// row. Each row carries the worker's label, a tree-glyph connector
/// (`├─` or `└─` styled DIM), and a trailing `×` close affordance
/// right-justified to match the project's `x` button column. Hit
/// targets get stamped for both the label area (switches focus to
/// the worker's chat) and the `×` (dispatches `Command::CloseWorker`).
///
/// Source of truth is `workspace.list_live_workers(project_key)`;
/// the TUI never caches this so the snapshot stays fresh between
/// `SessionUpdate::WorkerStatusChanged` events.
fn append_worker_tree_children(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    app: &mut App,
    project: &ProjectView,
) {
    let Some(workspace) = app.workspace.as_ref() else {
        return;
    };
    let workers = workspace.list_live_workers(&project.key);
    if workers.is_empty() {
        return;
    }

    let worker_count = workers.len();
    // Chrome: ` │  └─ <label>            ×  ` -> 2 left-indent + 3
    // tree connector + 1 sep + label + pad + 1 close + 2 right
    // gutter (matches the active project row's close-button column).
    let total_width = usize::from(area.width);
    let label_budget = total_width.saturating_sub(2 + 3 + 1 + 3 + 1);
    for (idx, worker) in workers.iter().enumerate() {
        let row_y = area.y + line_count_as_u16(lines);
        let is_last = idx + 1 == worker_count;
        let tree_glyph = if is_last { "\u{2514}\u{2500} " } else { "\u{251C}\u{2500} " };
        let label = truncate_with_ellipsis(worker.label.as_str(), label_budget);
        let label_pad = label_budget.saturating_sub(label.chars().count());
        let label_style = match worker.status {
            forge_primitives::WorkerLiveness::Running => Style::default(),
            forge_primitives::WorkerLiveness::Spawning => Style::default().fg(theme::DIM),
        };

        // Left-indent (1) + `│  ` (3) so the worker's tree connector
        // hangs off the active project's column rather than the org
        // column. Then connector, label, pad, close button, gutter.
        // Close affordance: ` x ` 3-cell button on USER_MSG_BG slate.
        // Same shape and column as the active project row's close
        // button so the worker rows visually align with the parent.
        let spans: Vec<Span<'static>> = vec![
            Span::raw(" "),
            Span::styled("\u{2502}  ".to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(tree_glyph.to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(label, label_style),
            Span::raw(" ".repeat(label_pad)),
            Span::raw(" "),
            Span::styled(
                " x ".to_owned(),
                Style::default()
                    .fg(Color::Gray)
                    .bg(theme::USER_MSG_BG)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ];
        lines.push(Line::from(spans));

        // Hit targets. The label area covers the row from x_start at
        // the indent + connector through to before the close button.
        // Click on it switches focus to the worker's chat session.
        // The trailing 3-cell ` x ` button + 1-cell tolerance each
        // side dispatches the close command (mirrors `CloseSession`).
        app.pane_hit_targets.push(PaneHitTarget::WorkerRow {
            project_key: project.key.clone(),
            label: worker.label.clone(),
            session_key: worker.session_key.clone(),
            y: row_y,
            height: 1,
        });
        let row_right = area.x.saturating_add(area.width);
        let close_x_start = row_right.saturating_sub(5);
        let close_x_end = row_right.saturating_sub(1);
        app.pane_hit_targets.push(PaneHitTarget::CloseWorker {
            project_key: project.key.clone(),
            label: worker.label.clone(),
            y: row_y,
            height: 1,
            x_start: close_x_start,
            x_end: close_x_end,
        });
    }
}

/// Chrome budget for an org-grouped row:
/// `<1 PANE_PAD><3 connector><1 glyph><1 sp><name><1 sp><RIGHT col><1 right pad>`
/// where RIGHT col = 3 cells (` ⏻ ` button for active rows / 3-char
/// `Xm`/`Xh`/`Xd` time for idle rows). Total = 6 left chrome + 1 sep
/// + 3 right col + 1 right pad = 11 chars per row.
fn name_budget_org_row(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(11))
}

/// Format `activity` as a short relative-time string anchored at
/// `now`, padded/truncated to a stable 3-char column. None → 3 spaces.
fn format_relative_time(activity: Option<SystemTime>, now: SystemTime) -> String {
    let Some(activity) = activity else {
        return "   ".to_owned();
    };
    let raw = super::format::relative_time(activity, now);
    if raw.chars().count() > 3 { raw.chars().take(3).collect() } else { format!("{raw:>3}") }
}

/// Saturating cast of `lines.len()` to `u16`. The pane area's height
/// is `u16` and projects tall enough to overflow `u16::MAX` rows
/// would already be wrong long before they overflow this cast — but
/// we saturate rather than panic so a runaway list at least caps
/// rather than aborting the renderer.
fn line_count_as_u16(lines: &[Line<'_>]) -> u16 {
    u16::try_from(lines.len()).unwrap_or(u16::MAX)
}

/// Find the `ProjectView` that owns `active_key` — handling the three
/// synthetic-key sentinels (`__conn_pending__`, `__spawn_<name>__`,
/// `__resume_<id>__`) in addition to real claude UUIDs. Without this,
/// every pane reader that does `sessions.iter().any(|s| &s.session
/// == key)` returns `None` during the Spawning window — leaving the
/// pane and top bar with no project highlighted while the user
/// stares at a "Waking …" placeholder.
///
/// Resolution order:
/// 1. `__spawn_<name>__` → find by `p.name == name`.
/// 2. `__resume_<session_id>__` → find by any session matching id.
/// 3. `__conn_pending__` → fall through to default-project lookup;
///    pane callers can supply their own fallback (the default lead is
///    in the catalog so step 4 generally still finds it on startup).
/// 4. Real UUID → existing catalog scan.
pub(crate) fn resolve_active_project_view<'p>(
    active_key: &forge_workspace::SessionKey,
    projects: &'p [&ProjectView],
) -> Option<&'p ProjectView> {
    let s = active_key.as_str();
    if let Some(name) = s.strip_prefix("__spawn_").and_then(|r| r.strip_suffix("__")) {
        return projects.iter().copied().find(|p| p.name == name);
    }
    if let Some(id) = s.strip_prefix("__resume_").and_then(|r| r.strip_suffix("__")) {
        return projects
            .iter()
            .copied()
            .find(|p| p.sessions.iter().any(|sess| sess.session.as_str() == id));
    }
    projects.iter().copied().find(|p| p.sessions.iter().any(|sess| &sess.session == active_key))
}

/// Braille spinner frames — same sequence used by `ui::input` and
/// `ui::message`, kept in sync so every running indicator in the TUI
/// turns at the same pace. `app.spinner_frame` advances every
/// `SPINNER_FRAME_INTERVAL_NORMAL` per the render tick in `app.rs`.
const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Glyph + foreground color for a session row based on its lifecycle
/// state. The session-is-active flag drives whether the
/// Running/Spawning spinner picks up the accent color (active +
/// running = `RUST_ORANGE`, background + running = terminal default).
/// `spinner_frame` indexes into `SPINNER_FRAMES` so the spinner
/// actually animates instead of sitting on `⠋`.
fn glyph_for_lifecycle(
    lifecycle: SessionLifecycleState,
    session_is_active: bool,
    spinner_frame: usize,
) -> (String, Color) {
    match lifecycle {
        SessionLifecycleState::Running | SessionLifecycleState::Spawning => {
            let color = if session_is_active { theme::RUST_ORANGE } else { Color::Reset };
            let ch = SPINNER_FRAMES[spinner_frame % SPINNER_FRAMES.len()];
            (ch.to_string(), color)
        }
        SessionLifecycleState::Attention => ("△".to_owned(), theme::STATUS_WARNING),
        // Idle = "alive, no turn in progress". Use a filled bullet so
        // the row reads as occupied (the design spec calls for blank
        // here, but in practice an empty glyph column makes Active /
        // Inactive rows look interchangeable). Active-session bullet
        // picks up the accent colour to match its bold label.
        SessionLifecycleState::Idle => {
            let color = if session_is_active { theme::RUST_ORANGE } else { theme::DIM };
            ("●".to_owned(), color)
        }
        SessionLifecycleState::Sleeping
        | SessionLifecycleState::AuthRequired
        | SessionLifecycleState::Failed
        | SessionLifecycleState::LoggedOut => ("·".to_owned(), theme::DIM),
    }
}

/// Duration after which transient failure badges (`·N⌛` / `·N✕`)
/// fade off the row. Counted from `peer_badges_last_failure_at`,
/// which is stamped each time the workspace reports a fresh
/// `timed_out` or `delivery_failed` increment. Cumulative
/// outgoing/incoming counts have no fade — they reflect live state
/// while the in-flight asks are pending.
const PEER_FAILURE_FADE: std::time::Duration = std::time::Duration::from_secs(60);

/// Build the peer-activity badge cluster spans for a row. Returns the
/// spans plus the printed width so the caller can shrink `name_budget`
/// before truncating the project label — without this, badges on a
/// narrow pane would either push the close button off-screen or land
/// on top of the label.
///
/// Visual order matches the brainstorm spec: outgoing → incoming →
/// timed-out → delivery-failed. Each badge is `·<count><glyph>` and
/// gets a single foreground colour. Counts of 0 are omitted entirely
/// rather than rendered as `·0↑` (the goal is "noise only when there's
/// activity"). Failure badges (`⌛`, `✕`) disappear after
/// [`PEER_FAILURE_FADE`] so a one-time spawn hiccup doesn't keep the
/// sidebar painted red until the session is closed.
fn peer_badge_spans(
    stats: &PeerInflightStats,
    last_failure_at: Option<Instant>,
    now: Instant,
) -> (Vec<Span<'static>>, usize) {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut width: usize = 0;

    let mut push = |span: Span<'static>| {
        width += span.content.chars().count();
        spans.push(span);
    };

    if stats.outgoing > 0 {
        push(Span::styled(
            format!("\u{00b7}{}\u{2191}", stats.outgoing),
            Style::default().fg(theme::DIM),
        ));
    }
    if stats.incoming > 0 {
        push(Span::styled(
            format!("\u{00b7}{}\u{2193}", stats.incoming),
            Style::default().fg(theme::DIM),
        ));
    }

    // Failure badges fade after 60 s. `last_failure_at` is `None`
    // when no failure has ever fired for this session.
    let failures_fresh = last_failure_at.is_some_and(|when| {
        now.checked_duration_since(when).is_some_and(|d| d < PEER_FAILURE_FADE)
    });
    if failures_fresh {
        if stats.timed_out > 0 {
            push(Span::styled(
                format!("\u{00b7}{}\u{231b}", stats.timed_out),
                Style::default().fg(theme::STATUS_WARNING),
            ));
        }
        if stats.delivery_failed > 0 {
            push(Span::styled(
                format!("\u{00b7}{}\u{2715}", stats.delivery_failed),
                Style::default().fg(theme::STATUS_ERROR),
            ));
        }
    }

    (spans, width)
}

// ---------------------------------------------------------------
// Account / status panel — pane footer.
//
// Hard-docked at the bottom of the Projects pane. Reads existing
// `App` accessors (no new wire data, no new reducers). Renders a
// stable-shape block:
//
//   ─────────────────────────
//     Profile  Subspace
//     Mode     Auto
//     Model    Opus 1M
//     Effort   Max
//     Fast     off
//
//     Ctx   ▓▓▓▓▓░░░░░░░  39%
//
//     5h    ▓▓░░░░░░░░░░  15%
//                       1h 48m
//
//     7d    ▓▓▓▓▓▓▓▓▓▓▓░  89%
//                        4d 4h
//
//     forge    v0.15.1
//     claude   v2.0.45  ↑ v2.0.50
//
// `ACCOUNT_PANEL_HEIGHT` is constant. The panel's render swallows the
// fixed N rows from the bottom of the pane; the project list takes
// everything above. Bar fill colour is a per-cell position gradient
// (cells 1–3 green, 4–6 yellow, 7–9 orange, 10–12 red) so the
// rightmost filled cell tells you which zone the bar is in.
//
// Cwd + branch rows live in the Inspector pane's `GIT` section —
// see `crate::ui::inspector_pane`.
// ---------------------------------------------------------------

/// Rows the account panel reserves from the bottom of the pane.
/// Constant by design — values flip but shape stays put (see the
/// "account chrome, not status row" intent in the design brief).
///
/// 19 rows: rule + 6 identity (Profile/Org/Session/Mode/Model/Effort) +
/// 1 blank + 2 (Ctx bar + size row) + 1 blank + 2 (5h bar + ETA row) +
/// 1 blank + 2 (7d bar + ETA row) + 1 blank + 2 (forge + claude version
/// rows).
const ACCOUNT_PANEL_HEIGHT: u16 = 19;

/// Width (columns) the rule and content extend up to from the
/// pane's right edge. Matches the project-row right gutter so the
/// bottom-panel content visually aligns with the project list above.
const PANEL_RIGHT_GUTTER: usize = 1;

/// Per-row chrome inside the bar row: `1 indent + 3 label + 2 gap +
/// BAR + 2 gap + 4 pct`. The bar cell count is derived per render
/// so the bar stretches to fill (pane_width - PANEL_RIGHT_GUTTER -
/// chrome).
const BAR_ROW_FIXED_CHROME: usize = 1 + 3 + 2 + 2 + 4;

/// Below this pane height we skip the panel entirely (would crowd the
/// project list too aggressively). The chat footer is gone in this
/// flow, so the account info is only available via this panel — when
/// it's skipped, the user loses visibility on Mode / Model / Ctx /
/// usage. Acceptable for the ultra-compact-pane edge case; the docked
/// alternative would push the project list out of meaningful range.
const ACCOUNT_PANEL_MIN_PANE_HEIGHT: u16 = 24;

/// Width of the identity-block label column (`Profile`, `Session`,
/// `Mode`, `Model`, `Effort`). Right-padded so the value column
/// aligns regardless of label length.
const ACCOUNT_PANEL_ID_LABEL_WIDTH: usize = 7;

/// Bar cell count derived from the pane width so the row stretches
/// to fill the available content area (pane width minus the 2-col
/// left indent, fixed chrome from label/gaps/pct, and the right
/// gutter). At pane width 32 (Wide tier) this is 17 cells; at width
/// 24 (Medium tier) it's 9 cells. Floored at 6 so the gradient
/// remains visually meaningful even at very narrow panes.
fn bar_cells_for(pane_width: u16) -> usize {
    let pane = usize::from(pane_width);
    pane.saturating_sub(PANEL_RIGHT_GUTTER).saturating_sub(BAR_ROW_FIXED_CHROME).max(6)
}

/// Distribute `cells` over the 4 colour zones (green / yellow /
/// orange / red), placing the remainder in the lower-indexed zones
/// first. For 12 cells this yields `[3, 3, 3, 3]` (matching the
/// original polish brief); for 17 cells `[5, 4, 4, 4]`; etc.
const fn bar_zone_sizes(cells: usize) -> [usize; 4] {
    let base = cells / 4;
    let extra = cells % 4;
    [
        base + if extra >= 1 { 1 } else { 0 },
        base + if extra >= 2 { 1 } else { 0 },
        base + if extra >= 3 { 1 } else { 0 },
        base,
    ]
}

/// Render the account / status panel into the bottom of `area`.
/// Returns the number of rows the panel consumed (caller subtracts
/// from `area.height` to size the project-list region). Returns 0
/// when the pane is too short to fit the panel without crowding.
fn render_account_status_footer(frame: &mut Frame, area: Rect, app: &mut App) -> u16 {
    if area.height < ACCOUNT_PANEL_MIN_PANE_HEIGHT || area.width == 0 {
        return 0;
    }
    let height = ACCOUNT_PANEL_HEIGHT;
    let panel_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height),
        width: area.width,
        height,
    };
    let lines = build_account_panel_lines(app, area.width);
    stamp_session_copy_hit_target(app, panel_area);
    frame.render_widget(Paragraph::new(lines), panel_area);
    height
}

/// Stamp the click target for the ID row's trailing 4-cell copy
/// button. Row layout: row 0 rule, row 1 Profile, row 2 Org, row 3
/// ID. The button occupies the rightmost 4 cells before the 1-col
/// right gutter. Hit band has 1 cell of tolerance on the left so a
/// near-miss still registers. Only stamped when the active session
/// has an id.
fn stamp_session_copy_hit_target(app: &mut App, panel_area: Rect) {
    let Some(session_id) = app.session_id().map(|sid| sid.to_string()) else {
        return;
    };
    let panel_right = panel_area.x.saturating_add(panel_area.width);
    let x_start = panel_right.saturating_sub(6);
    let x_end = panel_right.saturating_sub(1);
    let y = panel_area.y.saturating_add(3); // rule + Profile + Org + ID
    app.pane_hit_targets.push(PaneHitTarget::CopySessionId {
        session_id,
        y,
        height: 1,
        x_start,
        x_end,
    });
}

/// Filling bar with a per-cell position colour gradient over the
/// 4 zones (green / yellow / orange / red). `cells` is the total
/// bar width; the function distributes filled cells across the zones
/// and uses `░` DIM for empty cells. The bar glyph is `▓` (DARK
/// SHADE) for every filled cell; gradient is fg-colour only, never
/// a glyph swap.
fn bar_spans(pct: f64, cells: usize) -> Vec<Span<'static>> {
    let pct = pct.clamp(0.0, 100.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let filled = ((pct / 100.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    let empty = cells - filled;

    let zones = bar_zone_sizes(cells);
    let green = filled.min(zones[0]);
    let yellow = filled.saturating_sub(zones[0]).min(zones[1]);
    let orange = filled.saturating_sub(zones[0] + zones[1]).min(zones[2]);
    let red = filled.saturating_sub(zones[0] + zones[1] + zones[2]).min(zones[3]);

    let mut spans = Vec::with_capacity(5);
    if green > 0 {
        spans.push(Span::styled("▓".repeat(green), Style::default().fg(Color::Green)));
    }
    if yellow > 0 {
        spans.push(Span::styled("▓".repeat(yellow), Style::default().fg(theme::STATUS_WARNING)));
    }
    if orange > 0 {
        spans.push(Span::styled("▓".repeat(orange), Style::default().fg(theme::RUST_ORANGE)));
    }
    if red > 0 {
        spans.push(Span::styled("▓".repeat(red), Style::default().fg(theme::STATUS_ERROR)));
    }
    if empty > 0 {
        spans.push(Span::styled("░".repeat(empty), Style::default().fg(theme::DIM)));
    }
    spans
}

/// Padded label cell — `"<text>"` right-padded with spaces so the
/// next column aligns regardless of label length. Matches the dim
/// styling of the legacy footer's `Loc:` prefix.
fn label_span(text: &'static str, width: usize) -> Span<'static> {
    let mut s = text.to_owned();
    while s.chars().count() < width {
        s.push(' ');
    }
    Span::styled(s, Style::default().fg(theme::DIM))
}

/// Build the panel's lines. Layout is fixed at `ACCOUNT_PANEL_HEIGHT`
/// rows; missing data renders as a dim placeholder so the shape
/// doesn't shift between sessions.
#[allow(clippy::too_many_lines)]
fn build_account_panel_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(ACCOUNT_PANEL_HEIGHT as usize);

    // Row 0: dim rule. No blank after — the identity block sits flush
    // against the rule, treating the rule as the panel's top edge
    // rather than a section separator.
    let rule_width = usize::from(width.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("─".repeat(rule_width), Style::default().fg(theme::DIM)),
    ]));

    // Rows 1..=5: identity / posture block. Labels right-padded to
    // `ACCOUNT_PANEL_ID_LABEL_WIDTH` chars ("Profile" is the longest).
    // 1-col left pad + 2-col gutter before the value.
    let value_budget = usize::from(width).saturating_sub(1 + ACCOUNT_PANEL_ID_LABEL_WIDTH + 2);

    // Profile.
    let profile_value = app.active_account_display_name().unwrap_or_else(|| "—".to_owned());
    let profile_fitted = truncate_with_ellipsis(&profile_value, value_budget);
    lines.push(Line::from(vec![
        Span::raw(" "),
        label_span("Profile", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(profile_fitted),
    ]));

    // Org. Reads from the active session's `account.organization`;
    // dim placeholder when the SDK hasn't reported one yet.
    let org_value = app
        .account_info()
        .and_then(|account| account.organization.clone())
        .unwrap_or_else(|| "—".to_owned());
    let org_fitted = truncate_with_ellipsis(&org_value, value_budget);
    lines.push(Line::from(vec![
        Span::raw(" "),
        label_span("Org", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(org_fitted),
    ]));

    // ID. First 8 chars of the active session's UUID when known
    // (labelled `ID` rather than `Session` so the short hex string
    // reads as an identifier, not a name). Trailing 4-cell button
    // with slate background — copies the FULL session id to the OS
    // clipboard. Button sits flush at the right gutter regardless of
    // value length so its hit column doesn't shift with pane width.
    // Click target stamped in `stamp_session_copy_hit_target`.
    let session_value = app
        .session_id()
        .map_or_else(|| "—".to_owned(), |sid| sid.to_string().chars().take(8).collect::<String>());
    // Reserve 5 cells at the right end of the value area: 4 for the
    // button + 1 for the right gutter.
    let session_value_budget = value_budget.saturating_sub(5);
    let session_fitted = truncate_with_ellipsis(&session_value, session_value_budget);
    let pad_cells = value_budget.saturating_sub(session_fitted.chars().count()).saturating_sub(5);
    let mut session_spans = vec![
        Span::raw(" "),
        label_span("ID", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::styled(session_fitted, Style::default().fg(theme::DIM)),
        Span::raw(" ".repeat(pad_cells)),
    ];
    if app.session_id().is_some() {
        session_spans.push(Span::styled(
            " \u{29C9}  ".to_owned(),
            Style::default().fg(Color::Gray).bg(theme::USER_MSG_BG).add_modifier(Modifier::BOLD),
        ));
    } else {
        session_spans.push(Span::raw("    "));
    }
    session_spans.push(Span::raw(" "));
    lines.push(Line::from(session_spans));

    // Mode.
    let (mode_label, mode_color) = mode_label_and_color(app);
    let mode_label_fitted = truncate_with_ellipsis(&mode_label, value_budget);
    lines.push(Line::from(vec![
        Span::raw(" "),
        label_span("Mode", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::styled(mode_label_fitted, Style::default().fg(mode_color)),
    ]));

    // Model. Display name only — effort lives on its own row below so
    // a long model name can't push it off-screen.
    let model_value = build_model_label(app).unwrap_or_else(|| "—".to_owned());
    let model_fitted = truncate_with_ellipsis(&model_value, value_budget);
    lines.push(Line::from(vec![
        Span::raw(" "),
        label_span("Model", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(model_fitted),
    ]));

    // Effort. Always shown — the underlying `EffortLevel` always has
    // a value (config carries a default). Keeping the row unconditional
    // means it doesn't appear / disappear as the user switches models.
    let effort = app.observed_effort().unwrap_or_else(|| app.config.thinking_effort_effective());
    lines.push(Line::from(vec![
        Span::raw(" "),
        label_span("Effort", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(effort_short_label(effort).to_owned()),
    ]));

    // Row 6: blank separating identity from usage.
    lines.push(Line::default());

    // Rows 7-8: Ctx bar + size row. Size mirrors the 5h/7d ETA
    // pattern — DIM, right-justified to the panel's content right
    // edge — but reads the model's raw context-window size
    // (`SessionUsageState.context_max_tokens`) instead of a reset
    // duration. `—` when the upstream probe hasn't reported a size
    // yet so the panel's row count stays constant.
    let bar_cells = bar_cells_for(width);
    let ctx_pct = app.session_usage().context_usage_percent.map_or(0.0, f64::from);
    let ctx_pct_str = format!("{:>3}%", app.session_usage().context_usage_percent.unwrap_or(0));
    let mut ctx_line = vec![Span::raw(" "), label_span("Ctx", 3), Span::raw("  ")];
    ctx_line.extend(bar_spans(ctx_pct, bar_cells));
    ctx_line.push(Span::raw("  "));
    ctx_line.push(Span::raw(ctx_pct_str));
    lines.push(Line::from(ctx_line));

    let ctx_size_text =
        app.session_usage().context_max_tokens.map_or_else(|| "—".to_owned(), format_token_count);
    let ctx_size_chars = ctx_size_text.chars().count();
    let ctx_size_budget = usize::from(width).saturating_sub(PANEL_RIGHT_GUTTER);
    let ctx_size_fill = ctx_size_budget.saturating_sub(ctx_size_chars);
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(ctx_size_fill)),
        Span::styled(ctx_size_text, Style::default().fg(theme::DIM)),
    ]));

    // Row 8: blank between Ctx and 5h.
    lines.push(Line::default());

    // Surface the latest poll-attempt failure (if any) so empty
    // bars carry a `rate-limited` / `expired` / … hint instead of
    // looking like a forge bug. Lookup is by active account display
    // name; the workspace returns `None` when the most recent poll
    // succeeded.
    let usage_error = app
        .workspace
        .as_ref()
        .zip(app.active_account_display_name())
        .and_then(|(ws, name)| ws.usage_error_for(&name));

    // Rows 9..=10: 5h bar + ETA row.
    push_usage_window_lines(
        &mut lines,
        "5h",
        app.usage().snapshot.as_ref().and_then(|s| s.five_hour.as_ref()),
        width,
        usage_error,
    );

    // Row 11: blank between 5h and 7d.
    lines.push(Line::default());

    // Rows 12..=13: 7d bar + ETA row.
    push_usage_window_lines(
        &mut lines,
        "7d",
        app.usage().snapshot.as_ref().and_then(|s| s.seven_day.as_ref()),
        width,
        usage_error,
    );

    // Row 14: blank between usage and version rows.
    lines.push(Line::default());

    // Rows 15..=16: forge + claude versions. The claude row shows
    // a yellow `↑ vX.Y.Z` indicator when the npm registry probe
    // reports a strictly-newer published version. Both rows render
    // a DIM `—` placeholder when the corresponding probe failed so
    // the panel's row count stays constant.
    let forge_version = format!("v{}", crate::FORGE_VERSION_SHORT);
    lines.push(Line::from(vec![
        Span::raw(" "),
        label_span("forge", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(forge_version),
    ]));

    let cli_info = app.cli_version_info.as_ref();
    let installed = cli_info
        .and_then(|i| i.installed.as_deref())
        .map_or_else(|| "—".to_owned(), |v| format!("v{v}"));
    // Build the claude row with a width-aware right gutter so the
    // optional update indicator never overflows past `pane_width -
    // PANEL_RIGHT_GUTTER`. Pre-compute the row's printed width as we
    // assemble it; if appending the indicator would push the row
    // past the budget, skip it (it's a hint, not load-bearing).
    let mut claude_spans = vec![
        Span::raw(" "),
        label_span("claude", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(installed.clone()),
    ];
    let claude_prefix_width = 1 + ACCOUNT_PANEL_ID_LABEL_WIDTH + 2 + installed.chars().count();
    if let Some(info) = cli_info
        && info.has_update()
        && let Some(latest) = info.latest.as_deref()
    {
        let indicator = format!("\u{2191} v{latest}");
        let indicator_chars = indicator.chars().count();
        let budget = usize::from(width).saturating_sub(PANEL_RIGHT_GUTTER);
        // At least one space must separate the installed version from
        // the indicator, otherwise they visually collide.
        if claude_prefix_width + 1 + indicator_chars <= budget {
            // Right-justify the indicator into the panel's right gutter
            // so the row terminates at exactly `pane_width -
            // PANEL_RIGHT_GUTTER` cols — same as the bar rows / ETA
            // rows above it. Otherwise the row's right edge slides
            // around depending on indicator length and the panel
            // looks ragged.
            let fill = budget.saturating_sub(claude_prefix_width + indicator_chars);
            claude_spans.push(Span::raw(" ".repeat(fill)));
            claude_spans.push(Span::styled(indicator, Style::default().fg(theme::STATUS_WARNING)));
        }
    }
    lines.push(Line::from(claude_spans));

    debug_assert_eq!(
        u16::try_from(lines.len()).unwrap_or(u16::MAX),
        ACCOUNT_PANEL_HEIGHT,
        "account panel must render exactly ACCOUNT_PANEL_HEIGHT rows so the layout split stays consistent",
    );
    lines
}

/// Append a 12-cell-bar row + a DIM right-justified ETA row for one
/// usage window. When the window is missing (no snapshot yet, account
/// has no Anthropic plan, etc.) the bar renders at 0% and the ETA
/// row shows `—` right-justified — so the panel's total row count
/// stays at `ACCOUNT_PANEL_HEIGHT`.
///
/// `width` is the full pane width so the ETA can be right-justified
/// against the right edge (matches the percent column of the bar row
/// above it).
/// Append two rows for one usage window: a bar+percent row, then a
/// DIM ETA row right-justified to the panel's content right edge
/// (col `width - PANEL_RIGHT_GUTTER`). The bar stretches to fill
/// the available content width.
fn push_usage_window_lines(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    window: Option<&crate::app::UsageWindow>,
    width: u16,
    usage_error: Option<forge_workspace::UsageFetchStatus>,
) {
    let bar_cells = bar_cells_for(width);
    let pct_value = window.map_or(0.0, |w| w.utilization);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct_text = window
        .map_or_else(|| "  —%".to_owned(), |w| format!("{:>3}%", w.utilization.round() as i64));
    let mut row = vec![Span::raw(" "), label_span(label, 3), Span::raw("  ")];
    row.extend(bar_spans(pct_value, bar_cells));
    row.push(Span::raw("  "));
    row.push(Span::raw(pct_text));
    lines.push(Line::from(row));

    // ETA — when a real reset window exists, show the duration only
    // (no "resets in " prose). When the window is missing, fall
    // back to the last poll-attempt failure label (`⚠ expired`,
    // `rate-limited`, …) so the user can tell an empty bar from an
    // upstream HTTP 429 / expired creds situation. With neither,
    // collapse to `—` to keep the row count constant.
    //
    // Color tier: success-path durations stay DIM. Probe-rate-limit
    // / network / fetch-failed labels stay DIM (transient). The two
    // statuses that need the user's attention to recover (Expired,
    // Unauthorized — the account literally can't serve a request
    // without /login) bump to STATUS_WARNING so the bottom-panel
    // bar carries an obvious yellow `⚠` mark instead of blending
    // into the rest of the DIM chrome.
    let (eta_text, eta_style) = window.and_then(format_window_reset_duration_only).map_or_else(
        || {
            let style = usage_error.map_or(Style::default().fg(theme::DIM), |s| {
                if needs_user_recovery(s) {
                    Style::default().fg(theme::STATUS_WARNING)
                } else {
                    Style::default().fg(theme::DIM)
                }
            });
            let text = usage_error.map_or_else(|| "—".to_owned(), usage_error_label);
            (text, style)
        },
        |duration| (duration, Style::default().fg(theme::DIM)),
    );
    let eta_chars = eta_text.chars().count();
    let right_edge = usize::from(width).saturating_sub(PANEL_RIGHT_GUTTER);
    let pad = right_edge.saturating_sub(eta_chars);
    lines.push(Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(eta_text, eta_style)]));
}

/// `true` when this status means the account can't serve requests
/// until the user takes recovery action (re-login). These labels
/// render in STATUS_WARNING yellow with a `⚠` prefix so they
/// visibly stand out from transient probe failures.
fn needs_user_recovery(status: forge_workspace::UsageFetchStatus) -> bool {
    use forge_workspace::UsageFetchStatus;
    matches!(status, UsageFetchStatus::Expired | UsageFetchStatus::Unauthorized)
}

/// Short label for an upstream usage-fetch failure. Kept terse so
/// it fits in the right-justified ETA column the success-path
/// duration uses. Statuses that need user recovery (Expired,
/// Unauthorized) carry a leading `⚠` so the meaning is obvious
/// even when the user is glancing past the panel.
///
/// `RateLimited` here means the `/api/oauth/usage` PROBE endpoint
/// returned 429, NOT that the user's account is rate-limited.
/// Surfacing "rate-limited" in the bar's hint column was
/// misleading — it read as "your account hit a limit" when really
/// it's "forge's bookkeeping request got throttled by Anthropic's
/// per-IP /usage rate." Show `—` (same as cold boot) instead so
/// the transient internal hiccup doesn't alarm the user. Network
/// hiccups likewise collapse to `—`; only real auth failures
/// surface with their loud yellow recovery hint.
fn usage_error_label(status: forge_workspace::UsageFetchStatus) -> String {
    use forge_workspace::UsageFetchStatus;
    match status {
        UsageFetchStatus::Expired => "⚠ expired — /login".to_owned(),
        UsageFetchStatus::Unauthorized => "⚠ unauthorized — /login".to_owned(),
        UsageFetchStatus::RateLimited
        | UsageFetchStatus::NetworkFailed
        | UsageFetchStatus::Other => "—".to_owned(),
    }
}

/// Strip the `"resets in "` prefix from
/// [`crate::app::usage::format_window_reset`]'s output, leaving just
/// the duration (e.g. `"1h 48m"`, `"4d 4h"`). Returns the full text
/// untouched when the helper produced something else (e.g. an account
/// custom description without the standard prefix).
fn format_window_reset_duration_only(window: &crate::app::UsageWindow) -> Option<String> {
    let full = crate::app::usage::format_window_reset(window)?;
    Some(full.strip_prefix("resets in ").map_or(full.clone(), str::to_owned))
}

/// Permission-mode label + color, mirroring the legacy footer's
/// `mode_color` logic. Falls back to `default` styling when the CLI
/// hasn't emitted a mode yet.
fn mode_label_and_color(app: &App) -> (String, Color) {
    let effective = app
        .observed_permission_mode()
        .map(|m| (m.as_wire().to_owned(), m.display_name().to_owned()))
        .or_else(|| app.mode().map(|m| (m.current_mode_id.clone(), m.current_mode_name.clone())));
    let Some((id, name)) = effective else {
        return ("—".to_owned(), theme::DIM);
    };
    let color = match id.as_str() {
        "default" => theme::DIM,
        "auto" | "acceptEdits" => Color::Yellow,
        "plan" => Color::Blue,
        "bypassPermissions" | "dontAsk" => Color::Red,
        _ => Color::Magenta,
    };
    (name, color)
}

/// Model label for the panel — `display_name_short` with the
/// `(… context)` wrapper stripped so a name like
/// `Opus (1M context)` renders as `Opus 1M` and fits the narrow
/// value column without truncation. Other callers of
/// `display_name_short` (welcome card, /config picker) keep the raw
/// value. Returns `None` when the CLI hasn't reported a current
/// model yet (early in spawn).
fn build_model_label(app: &App) -> Option<String> {
    let current = app.current_model()?;
    // Long form carries the model version (e.g. "Claude Opus 4.7"
    // rather than just "Opus"). Truncation downstream handles
    // overflow on the narrow panel.
    Some(condense_model_name(&current.display_name_long))
}

/// Condense a model display name for the panel's narrow column.
/// Strips a trailing parenthetical and folds it into the base name,
/// dropping any trailing `context` word inside the parens:
///
/// - `"Opus (1M context)"`   → `"Opus 1M"`
/// - `"Sonnet (200K context)"` → `"Sonnet 200K"`
/// - `"Foo (Bar)"`            → `"Foo Bar"` (no "context" word)
/// - `"Sonnet 4.5"`           → `"Sonnet 4.5"` (no parens — unchanged)
///
/// The inverse of "elegant" but predictable. Single-pass over chars
/// because the input is always short (< 32 chars in practice).
fn condense_model_name(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(open) = trimmed.rfind('(') else {
        return trimmed.to_owned();
    };
    if !trimmed.ends_with(')') {
        return trimmed.to_owned();
    }
    let base = trimmed[..open].trim_end();
    // Inner content excludes the parens themselves.
    let inner = trimmed[open + 1..trimmed.len() - 1].trim();
    let inner_no_context = inner.strip_suffix("context").map_or(inner, str::trim_end);
    if inner_no_context.is_empty() {
        return base.to_owned();
    }
    if base.is_empty() {
        return inner_no_context.to_owned();
    }
    format!("{base} {inner_no_context}")
}

/// Short effort label for the panel's Model row. Same set the legacy
/// footer used.
const fn effort_short_label(effort: crate::agent::model::EffortLevel) -> &'static str {
    use crate::agent::model::EffortLevel;
    match effort {
        EffortLevel::Low => "Low",
        EffortLevel::Medium => "Med",
        EffortLevel::High => "High",
        EffortLevel::Xhigh => "Xhi",
        EffortLevel::Max => "Max",
    }
}

/// Head-truncate `s` to at most `max_chars` characters with a
/// trailing `…` ellipsis. Returns the original string if it
/// already fits. When `max_chars` is `0` or `1` the result is just
/// `…` — there's no room for content + ellipsis at those budgets.
///
/// Counts Unicode chars, not bytes, so multibyte labels truncate at
/// a sane visual position. CJK / wide-emoji chars (display width > 1)
/// may still overflow visually; project + session names are
/// near-ASCII in practice.
///
/// Exposed as `pub(super)` so `top_bar` can reuse the same routine
/// for the active-context label without duplicating the logic.
pub(super) fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let mut out: String = s.chars().take(max_chars - 1).collect();
    out.push('…');
    out
}

/// Format a token count for the Ctx size row. Picks the largest
/// unit that produces a value ≥ 1: `1_000_000` → `1M`, `200_000` →
/// `200K`, anything under 1K returns the raw number. Exact multiples
/// of the unit drop the decimal (`1_000_000` → `1M`, not `1.0M`);
/// non-exact values round to one decimal (`1_200_000` → `1.2M`).
fn format_token_count(tokens: u64) -> String {
    const MILLION: u64 = 1_000_000;
    const THOUSAND: u64 = 1_000;
    if tokens >= MILLION {
        let whole = tokens / MILLION;
        let remainder = tokens % MILLION;
        if remainder == 0 {
            format!("{whole}M")
        } else {
            #[allow(clippy::cast_precision_loss)]
            let scaled = tokens as f64 / MILLION as f64;
            format!("{scaled:.1}M")
        }
    } else if tokens >= THOUSAND {
        let whole = tokens / THOUSAND;
        let remainder = tokens % THOUSAND;
        if remainder == 0 {
            format!("{whole}K")
        } else {
            #[allow(clippy::cast_precision_loss)]
            let scaled = tokens as f64 / THOUSAND as f64;
            format!("{scaled:.1}K")
        }
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_with_ellipsis("forge", 18), "forge");
    }

    #[test]
    fn truncate_long_string_head_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("subspace-chain-pulse", 12), "subspace-ch…");
    }

    #[test]
    fn truncate_unicode_counts_chars_not_bytes() {
        assert_eq!(truncate_with_ellipsis("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn truncate_max_one_returns_just_ellipsis() {
        assert_eq!(truncate_with_ellipsis("anything", 1), "…");
    }

    #[test]
    fn truncate_max_zero_returns_just_ellipsis() {
        assert_eq!(truncate_with_ellipsis("anything", 0), "…");
    }

    #[test]
    fn condense_model_name_drops_context_wrapper() {
        assert_eq!(condense_model_name("Opus (1M context)"), "Opus 1M");
        assert_eq!(condense_model_name("Sonnet (200K context)"), "Sonnet 200K");
    }

    #[test]
    fn condense_model_name_keeps_unwrapped_names() {
        assert_eq!(condense_model_name("Sonnet 4.5"), "Sonnet 4.5");
        assert_eq!(condense_model_name("Opus"), "Opus");
    }

    #[test]
    fn condense_model_name_folds_parens_without_context_word() {
        assert_eq!(condense_model_name("Foo (Bar)"), "Foo Bar");
    }

    #[test]
    fn condense_model_name_strips_whitespace() {
        assert_eq!(condense_model_name("  Opus (1M context)  "), "Opus 1M");
        assert_eq!(condense_model_name("Opus ( 1M context )"), "Opus 1M");
    }

    #[test]
    fn bar_spans_gradient_zone_counts_for_12_cells() {
        // 12-cell bar (the canonical 4×3 split): 25% → 3 green; 50%
        // → 3 green + 3 yellow; 75% → +3 orange; 100% → +3 red.
        let spans = bar_spans(25.0, 12);
        assert_eq!(spans.len(), 2); // green + empty

        let spans = bar_spans(50.0, 12);
        assert_eq!(spans.len(), 3); // green + yellow + empty

        let spans = bar_spans(75.0, 12);
        assert_eq!(spans.len(), 4); // green + yellow + orange + empty

        let spans = bar_spans(100.0, 12);
        assert_eq!(spans.len(), 4); // green + yellow + orange + red

        let spans = bar_spans(0.0, 12);
        assert_eq!(spans.len(), 1); // empty only
    }

    #[test]
    fn bar_zone_sizes_distributes_remainder() {
        assert_eq!(bar_zone_sizes(12), [3, 3, 3, 3]);
        assert_eq!(bar_zone_sizes(17), [5, 4, 4, 4]);
        assert_eq!(bar_zone_sizes(18), [5, 5, 4, 4]);
        assert_eq!(bar_zone_sizes(19), [5, 5, 5, 4]);
        assert_eq!(bar_zone_sizes(9), [3, 2, 2, 2]);
    }

    #[test]
    fn bar_cells_for_stretches_to_pane_width() {
        // Wide pane (32): 32 - 1 (right gutter) - 12 (chrome) = 19.
        assert_eq!(bar_cells_for(32), 19);
        // Medium pane (24): 24 - 1 - 12 = 11.
        assert_eq!(bar_cells_for(24), 11);
        // Narrower than the chrome+floor: clamps to 6.
        assert_eq!(bar_cells_for(10), 6);
    }

    #[test]
    fn peer_badge_spans_empty_when_no_activity() {
        let stats = PeerInflightStats::default();
        let (spans, width) = peer_badge_spans(&stats, None, Instant::now());
        assert!(spans.is_empty(), "no badges expected for default stats");
        assert_eq!(width, 0);
    }

    #[test]
    fn peer_badge_spans_renders_outgoing_and_incoming() {
        let stats =
            PeerInflightStats { outgoing: 2, incoming: 1, timed_out: 0, delivery_failed: 0 };
        let (spans, width) = peer_badge_spans(&stats, None, Instant::now());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("\u{2191}"), "outgoing arrow present: {text}");
        assert!(text.contains("\u{2193}"), "incoming arrow present: {text}");
        assert!(text.contains('2'), "outgoing count present: {text}");
        assert!(text.contains('1'), "incoming count present: {text}");
        // ·2↑·1↓ — 6 chars (· and arrow each count as 1 char).
        assert_eq!(width, 6);
    }

    #[test]
    fn peer_badge_spans_shows_failures_when_fresh() {
        let stats =
            PeerInflightStats { outgoing: 0, incoming: 0, timed_out: 1, delivery_failed: 1 };
        let now = Instant::now();
        let (spans, _) = peer_badge_spans(&stats, Some(now), now);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("\u{231b}"), "timeout glyph present when fresh: {text}");
        assert!(text.contains("\u{2715}"), "failure glyph present when fresh: {text}");
    }

    #[test]
    fn peer_badge_spans_fades_failures_after_60s() {
        let stats =
            PeerInflightStats { outgoing: 0, incoming: 0, timed_out: 1, delivery_failed: 1 };
        // Simulate `now` being 61 s past the failure timestamp by
        // pinning `last_failure_at` to a synthetic Instant and using
        // a `now` that's just after the fade window. Instant doesn't
        // accept arbitrary offsets, but `Instant::now() - 61s` is
        // valid via checked_sub.
        let later = Instant::now();
        let earlier = later.checked_sub(PEER_FAILURE_FADE + std::time::Duration::from_secs(1));
        let Some(stamped) = earlier else {
            // System clock can't go back that far on this platform;
            // skip the fade-window assertion rather than panic.
            return;
        };
        let (spans, _) = peer_badge_spans(&stats, Some(stamped), later);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("\u{231b}"), "timeout glyph faded after 60 s: {text}");
        assert!(!text.contains("\u{2715}"), "failure glyph faded after 60 s: {text}");
    }

    #[test]
    fn account_panel_height_matches_row_count() {
        // The const + the debug_assert in build_account_panel_lines
        // co-anchor the layout. This test pins the constant explicitly
        // so a change to row count surfaces here too, not only at
        // runtime.
        assert_eq!(ACCOUNT_PANEL_HEIGHT, 19);
    }

    /// A project with no live workers must produce zero tree-child
    /// rows and stamp no hit targets. Renders are driven directly
    /// from `workspace.list_live_workers`; the renderer's job is to
    /// branch cleanly on `is_empty`.
    #[test]
    fn worker_tree_children_render_no_rows_when_zero_workers() {
        use crate::app::PaneHitTarget;
        use forge_workspace::ProjectKey;

        let mut app = App::test_default();
        let project_key = ProjectKey::new_for_test("forge");
        let project =
            ProjectView::new_for_test(project_key.clone(), "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 32, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project);

        assert!(lines.is_empty(), "zero workers must render zero rows");
        let workers_targets: Vec<_> = app
            .pane_hit_targets
            .iter()
            .filter(|t| {
                matches!(t, PaneHitTarget::CloseWorker { .. } | PaneHitTarget::WorkerRow { .. })
            })
            .collect();
        assert!(workers_targets.is_empty(), "zero workers stamps no hit targets");
    }

    /// `WorkerStatusChanged { Removed }` only flags a redraw - the
    /// authoritative `live_workers` map lives on the workspace, so
    /// the next render reads it directly. This test asserts the
    /// pipeline shape: after the reducer fires AND the worker is
    /// dropped from `live_workers`, a subsequent render sees zero
    /// rows.
    #[test]
    fn worker_removed_reducer_drives_redraw_and_subsequent_render_omits_worker() {
        use crate::app::events::apply_session_update;
        use forge_workspace::ProjectKey;
        use forge_workspace::SessionKey;
        use forge_workspace::mcp::workers::types::WorkerEntry;
        use forge_workspace::protocol::{SessionUpdate, WorkerStatusAction};
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");
        let project_key = ProjectKey::new_for_test("forge");
        let entry = WorkerEntry {
            label: "reviewer".into(),
            charter: "be sharp".into(),
            session_key: SessionKey::from_session_id("worker-1"),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead".into(),
            needs_tag: false,
        };
        workspace.insert_live_worker(&project_key, entry.clone());

        // Baseline: render should show one row.
        {
            let project = ProjectView::new_for_test(
                project_key.clone(),
                "forge",
                "~/Projects/forge",
                Vec::new(),
            );
            let area = Rect { x: 0, y: 0, width: 32, height: 20 };
            let mut lines: Vec<Line<'static>> = Vec::new();
            append_worker_tree_children(&mut lines, area, &mut app, &project);
            assert_eq!(lines.len(), 1, "baseline: one worker = one row");
        }

        // Drop the worker from the source of truth, then fire the
        // Removed reducer the way the workspace would.
        workspace.remove_latest_worker(&project_key, "reviewer");
        app.needs_redraw = false;
        apply_session_update(
            &mut app,
            SessionUpdate::WorkerStatusChanged {
                project_key: project_key.clone(),
                action: WorkerStatusAction::Removed,
                status: entry.to_status(),
            },
        );
        assert!(app.needs_redraw, "Removed reducer must request a redraw");

        // Next render reads list_live_workers directly: zero rows.
        let project =
            ProjectView::new_for_test(project_key, "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 32, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project);
        assert!(lines.is_empty(), "after Removed, render shows no worker rows");
    }

    #[test]
    fn worker_tree_children_render_with_glyphs_and_close_affordance() {
        use crate::app::PaneHitTarget;
        use forge_workspace::ProjectKey;
        use forge_workspace::SessionKey;
        use forge_workspace::mcp::workers::types::WorkerEntry;
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test_default seeds a workspace stub");
        let project_key = ProjectKey::new_for_test("forge");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "reviewer".into(),
                charter: "be sharp".into(),
                session_key: SessionKey::from_session_id("worker-1"),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
            },
        );
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "doc-writer".into(),
                charter: "tone".into(),
                session_key: SessionKey::from_session_id("worker-2"),
                status: forge_primitives::WorkerLiveness::Spawning,
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
            },
        );

        let project =
            ProjectView::new_for_test(project_key.clone(), "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 32, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project);

        assert_eq!(lines.len(), 2, "two worker rows");
        let row0: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let row1: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        // First row → not-last → `├─`. Second row → last → `└─`.
        assert!(row0.contains("\u{251C}\u{2500}"), "first row has ├─: {row0:?}");
        assert!(row1.contains("\u{2514}\u{2500}"), "last row has └─: {row1:?}");
        assert!(row0.contains("reviewer"), "first row has reviewer label: {row0:?}");
        assert!(row1.contains("doc-writer"), "last row has doc-writer label: {row1:?}");
        assert!(row0.contains(" x "), "first row has close button glyph: {row0:?}");
        assert!(row1.contains(" x "), "last row has close button glyph: {row1:?}");

        // One CloseWorker + one WorkerRow per row → 4 hit targets.
        let workers_targets: Vec<_> = app
            .pane_hit_targets
            .iter()
            .filter(|t| {
                matches!(t, PaneHitTarget::CloseWorker { .. } | PaneHitTarget::WorkerRow { .. })
            })
            .collect();
        assert_eq!(workers_targets.len(), 4, "expected 4 worker targets, got {workers_targets:?}");
    }
}
