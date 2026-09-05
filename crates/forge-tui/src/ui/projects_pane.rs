//! Projects pane (left side, Wide + Medium tiers).
//!
//! Renders projects from
//! [`forge_workspace::Workspace::list_projects`], grouped by org,
//! with the active project highlighted; a project's live workers
//! drill down immediately under its row. Each row stamps a
//! [`PaneHitTarget`] into [`App::pane_hit_targets`] for the mouse
//! handler to read on click events.
//!
//! Width handling: project + worker labels are head-truncated with
//! a trailing `…` when they overflow the available row width -
//! rare at Wide tier (32ch pane), routine at Medium (24ch).
//! Hit-target stamps always carry the *un-truncated* identifier so
//! click routing keeps working regardless of truncation.

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
/// of the pane. Shared by the inline and overlay renderers - the
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
    // `contains_y` test refuses them - the click would otherwise
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
            | crate::app::PaneHitTarget::InspectorGitPrOpen { .. }
            | crate::app::PaneHitTarget::InspectorMcpOpenStatus { .. }
            | crate::app::PaneHitTarget::InspectorAttentionRow { .. }
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
        // Thumb position in cells, re-clamped to `track` on the next line.
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
/// handler closes the overlay. Mouse-only - no keyboard navigation.
pub fn render_overlay(frame: &mut Frame, area: Rect, app: &mut App, projects: &[ProjectView]) {
    app.pane_hit_targets.clear();

    // Same two-region split as the inline pane - account panel docked
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
    // Stamp ✕ hit-target - last char on the banner row.
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
/// per-row glyph distinguishes live sessions (spinner - RUST_ORANGE
/// when focused, terminal-default otherwise) from idle catalog
/// entries (`○` DIM). Live rows carry an `x` close affordance at
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

/// Projects bucketed the way the pane draws them: orgs
/// alphabetically (the `BTreeMap` key order), then projects
/// alphabetically by name within each org. Shared with
/// [`drawn_session_rows`] so the row the focus pick calls adjacent
/// and the row the user is looking at cannot drift apart.
fn projects_by_org(
    projects: &[ProjectView],
) -> std::collections::BTreeMap<&str, Vec<&ProjectView>> {
    let mut by_org: std::collections::BTreeMap<&str, Vec<&ProjectView>> =
        std::collections::BTreeMap::new();
    for project in projects {
        by_org.entry(project.org.as_str()).or_default().push(project);
    }
    for bucket in by_org.values_mut() {
        bucket.sort_by(|a, b| a.name.cmp(&b.name));
    }
    by_org
}

/// Worker `SessionKey`s across every project. Used to skip worker
/// entries when picking each project's "live lead" - the catalog
/// includes worker JSONLs post-Connected, and naive iteration would
/// surface a worker as the project's live row once the worker's
/// mtime overtakes the lead's.
fn live_worker_keys(app: &App) -> std::collections::HashSet<forge_workspace::SessionKey> {
    app.workspace
        .as_ref()
        .map(|ws| ws.all_live_worker_session_keys().into_iter().collect())
        .unwrap_or_default()
}

/// The session a project's row represents. Anchored to a non-worker
/// pooled bucket whose `cwd_raw` matches the project path, which
/// sidesteps catalog ordering (a freshly-spawned worker can overtake
/// the lead by mtime) and the catalog-presence-of-the-lead question
/// entirely. Falls back to a catalog walk excluding workers, for when
/// the lead's bucket isn't yet pooled (cold project at launchpad
/// time) but its session has a `SessionView` entry.
fn live_lead_key(
    app: &App,
    project: &ProjectView,
    project_path: &str,
    worker_keys: &std::collections::HashSet<forge_workspace::SessionKey>,
) -> Option<forge_workspace::SessionKey> {
    app.sessions
        .iter()
        .find(|(k, s)| s.cwd_raw.as_str() == project_path && !worker_keys.contains(k))
        .map(|(k, _)| k.clone())
        .or_else(|| {
            project.sessions.iter().find_map(|s| {
                if worker_keys.contains(&s.session) {
                    return None;
                }
                app.sessions.get(&s.session).map(|_| s.session.clone())
            })
        })
}

/// Every session the pane draws as a focusable row, in the order it
/// draws them: each project's lead, then that project's live workers.
/// The focus pick after a close reads this so it lands on the row
/// next to the one that went away rather than on an arbitrary
/// [`App::sessions`] entry.
///
/// A project waking under a `__spawn_<name>__` synthetic contributes
/// nothing here. The pane does draw that row, but a click on it is
/// refused while the bucket is `Spawning`, so it is not a place focus
/// can be sent.
pub(crate) fn drawn_session_rows(
    app: &App,
    projects: &[ProjectView],
) -> Vec<forge_workspace::SessionKey> {
    let worker_keys = live_worker_keys(app);
    let mut rows = Vec::new();
    for bucket in projects_by_org(projects).values() {
        for project in bucket {
            let project_path = project.path.to_string_lossy();
            rows.extend(live_lead_key(app, project, &project_path, &worker_keys));
            if let Some(ws) = app.workspace.as_ref() {
                rows.extend(ws.list_live_workers(&project.key).into_iter().map(|w| w.session_key));
            }
        }
    }
    rows
}

fn append_project_rows(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    app: &mut App,
    projects: &[ProjectView],
) {
    let active_session_key = app.active_session_key.clone();
    let spinner_glyph = app.active_spinner_glyph();
    let worker_keys = live_worker_keys(app);
    let lifecycle_for = |key: &forge_workspace::SessionKey| -> SessionLifecycleState {
        app.sessions.get(key).map_or(SessionLifecycleState::default(), |s| s.lifecycle_state)
    };
    let badges_for = |key: &forge_workspace::SessionKey| -> PeerBadgeInput {
        app.sessions.get(key).map_or_else(PeerBadgeInput::default, |s| PeerBadgeInput {
            stats: s.peer_badges.clone(),
            last_failure_at: s.peer_badges_last_failure_at,
        })
    };

    // Row metadata per org, assembled in drawn order: each entry is
    // a Vec of (project, optional live session metadata + peer
    // badges).
    let ordered = projects_by_org(projects);
    let mut by_org: Vec<(&str, Vec<RowMeta<'_>>)> = Vec::with_capacity(ordered.len());
    for (org_name, bucket) in &ordered {
        let mut rows: Vec<RowMeta<'_>> = Vec::with_capacity(bucket.len());
        for project in bucket {
            let spawn_synthetic =
                forge_workspace::SessionKey::from_session_id(format!("__spawn_{}__", project.name));
            let project_path_str = project.path.to_string_lossy().into_owned();
            // The project row represents the LEAD.
            let live_session =
                live_lead_key(app, project, &project_path_str, &worker_keys).map(|key| {
                    let lifecycle = lifecycle_for(&key);
                    (key, lifecycle)
                });
            let synthetic = app
                .sessions
                .get(&spawn_synthetic)
                .map(|_| (spawn_synthetic.clone(), lifecycle_for(&spawn_synthetic)));
            // Project row is focused when the active session belongs to
            // this project - either as the lead OR as one of its
            // workers. The lead's bucket has cwd_raw matching the
            // project path; a worker's bucket likewise sits under the
            // project (via `live_workers[project.key]`); and the
            // catalog tracks the session_key explicitly. Match any of
            // the three signals so snapshot tests (which don't seed
            // cwd_raw on test UiSessions) and the production hot path
            // both highlight correctly.
            //
            // The third signal (`worker_match`) matters for **resumed
            // workers** specifically: their `cwd_raw` carries the
            // worktree path (claude chdir'd before writing the catalog
            // row that the resume path reads) rather than the project
            // root, and their JSONL is tagged under a per-worker project
            // key in the catalog rather than the parent. Fresh-spawned
            // workers accidentally pass `cwd_match` because their
            // pre-Connect bucket still has `cwd_raw == project.path`.
            let is_active_project = active_session_key.as_ref().is_some_and(|k| {
                let cwd_match = app
                    .sessions
                    .get(k)
                    .is_some_and(|s| s.cwd_raw.as_str() == project_path_str.as_str());
                let catalog_match = project.sessions.iter().any(|s| s.session == *k);
                let worker_match = app.workspace.as_ref().is_some_and(|ws| {
                    ws.list_live_workers(&project.key).iter().any(|w| w.session_key == *k)
                });
                cwd_match || catalog_match || worker_match
            });
            let live = live_session.or(synthetic).map(|(key, lifecycle)| {
                let badges = badges_for(&key);
                (key, lifecycle, is_active_project, badges)
            });
            rows.push((project, live));
        }
        by_org.push((org_name, rows));
    }

    let now = SystemTime::now();
    let org_count = by_org.len();
    for (org_idx, (org_name, rows)) in by_org.iter().enumerate() {
        // Org header row - DIM bold, no tree chrome.
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                (*org_name).to_owned(),
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
                spinner_glyph,
                now,
            );
            append_worker_tree_children(lines, area, app, project, is_last, spinner_glyph);
            // Deadzone gap row between adjacent projects in the
            // same org - emits the `│  ` tree continuation so the
            // connector lines visually link across the breathing
            // gap rather than breaking into floating fragments.
            // Skipped after the last project in the org - the
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
    spinner_glyph: char,
    now: SystemTime,
) {
    let row_y = area.y + line_count_as_u16(lines);
    let connector = if is_last { "\u{2514}\u{2500} " } else { "\u{251C}\u{2500} " };
    let total_name_budget = name_budget_org_row(area.width);

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw(" "));
    spans.push(Span::styled(connector.to_owned(), Style::default().fg(theme::DIM)));

    if let Some((session_key, lifecycle, is_focused, badge_input)) = live {
        // Background-row override: a non-active session with a pending
        // permission/question prompt surfaces yellow △ regardless of
        // lifecycle, so the user notices it without switching focus.
        // Focused rows keep their normal glyph - the yellow signal is
        // "background session needs you", not "the one you're looking at".
        let needs_attention = !*is_focused
            && app.sessions.get(session_key).is_some_and(|b| !b.prompt_queue.is_empty());
        // A background session whose turn died surfaces red `✕` - an
        // error is not a request for input, so it gets its own glyph and
        // outranks a prompt that can no longer be answered.
        let failed_turn =
            !*is_focused && app.sessions.get(session_key).is_some_and(|b| b.failed_turn.is_some());
        // A live backgrounded task keeps the row spinning even after its
        // turn settles to Idle - pending input still wins over both.
        let has_background_work = app
            .sessions
            .get(session_key)
            .is_some_and(crate::app::session::UiSession::has_live_background_work);
        let (glyph, glyph_color) = if failed_turn {
            ("\u{2715}".to_owned(), theme::STATUS_ERROR)
        } else if needs_attention {
            ("\u{25b3}".to_owned(), theme::STATUS_WARNING)
        } else {
            glyph_for_lifecycle(*lifecycle, *is_focused, has_background_work, spinner_glyph)
        };
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
        // 1-col separator before the button - matches the 1-col
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
            crate::app::ROW_CLOSE_BUTTON.to_owned(),
            Style::default().fg(Color::Gray).bg(theme::USER_MSG_BG).add_modifier(Modifier::BOLD),
        ));
        // 1-col right gutter - matches the inspector pane's GIT
        // section right edge AND the idle row's 1-col gutter.
        spans.push(Span::raw(" "));
        lines.push(Line::from(spans));
        // Hit targets: row body up to the control gutter →
        // focus/switch; the gutter itself → close session.
        let close_x_start = crate::app::control_gutter_start(area);
        app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
            project_name: project.key.as_str().to_owned(),
            y: row_y,
            height: 1,
            x_start: area.x,
            x_end: close_x_start,
        });
        // The ` x ` 3-cell span occupies (row_right - 4) to
        // (row_right - 2); the band adds 1-col tolerance on the left
        // and the rightmost gutter col stays inert.
        let close_x_end = area.x.saturating_add(area.width).saturating_sub(1);
        app.pane_hit_targets.push(PaneHitTarget::CloseSession {
            session_key: session_key.clone(),
            y: row_y,
            height: 1,
            x_start: close_x_start,
            x_end: close_x_end,
        });
    } else {
        // Idle (no live session) - `○` DIM glyph, name in DIM, last
        // activity time at the right edge in DIM. No close
        // affordance since there's nothing to close. Adds a 1-col
        // pad after `time` so the date column aligns with the
        // emoji column on active rows (emoji = 2 cells; time = 3
        // chars; the 1-col pad here equalises them in the same x
        // position). Plus the standard 1-col right gutter.
        //
        // No badge column on idle rows - peer in-flight state lives
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
        // Same body range as the live row above. The gutter carries
        // the relative-time label here and no control, but it stays
        // reserved: this row grows a close button the moment its
        // session lands, and a click aimed at the timestamp must not
        // mean "wake" one frame and "close" the next.
        app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
            project_name: project.key.as_str().to_owned(),
            y: row_y,
            height: 1,
            x_start: area.x,
            x_end: crate::app::control_gutter_start(area),
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
///
/// `parent_is_last` mirrors the project row's own last-in-org flag so
/// the subtree knows whether the org trunk is still open above it.
fn append_worker_tree_children(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    app: &mut App,
    project: &ProjectView,
    parent_is_last: bool,
    spinner_glyph: char,
) {
    let Some(workspace) = app.workspace.as_ref() else {
        return;
    };
    let workers = workspace.list_live_workers(&project.key);
    if workers.is_empty() {
        return;
    }

    let worker_count = workers.len();
    let active_session_key = app.active_session_key.clone();
    // Chrome: ` │  └─ <glyph> <label> <pad> <sp> x  ` ->
    // 1 left pad + 3 vertical indent + 3 tree connector + 1 glyph
    // + 1 sep + label + pad + 1 sep + 3 close (` x `) + 1 right
    // gutter = 14 cells of chrome. Matches the active project row's
    // close-button column so worker and lead `x` glyphs line up
    // vertically.
    let total_width = usize::from(area.width);
    for (idx, worker) in workers.iter().enumerate() {
        // Leading breathing gap above the FIRST worker so the tree
        // connector visually links the project lead row to probe-a.
        // The col-4 `│` is the worker-subtree continuation dropping
        // down to the first worker's `├─`; without it probe-a's
        // connector appears to start mid-air. The col-1 cell is the
        // org continuation, blank once the parent closed the trunk.
        if idx == 0 {
            lines.push(Line::from(vec![
                Span::raw(" "),
                org_trunk_span(parent_is_last),
                Span::styled("\u{2502}".to_owned(), Style::default().fg(theme::DIM)),
            ]));
        }
        let row_y = area.y + line_count_as_u16(lines);
        let is_last = idx + 1 == worker_count;
        let tree_glyph = if is_last { "\u{2514}\u{2500} " } else { "\u{251C}\u{2500} " };
        // Peer-activity badges mirror the project-lead row at :501 -
        // every worker's `session_key` carries its own `peer_badges`
        // populated by the `PeerInflightStatsChanged` reducer, so the
        // counter advances on the worker row when forge asks the
        // worker / when the worker asks a sibling.
        //
        // `unwrap_or_default()` handles the brief post-spawn window
        // before Connected lands: `peer_badge_spans` with default stats
        // returns empty spans + width=0, so the column collapses
        // cleanly.
        let (badge_stats, badge_last_failure_at) = app
            .sessions
            .get(&worker.session_key)
            .map(|s| (s.peer_badges.clone(), s.peer_badges_last_failure_at))
            .unwrap_or_default();
        let (badge_spans, badge_width) =
            peer_badge_spans(&badge_stats, badge_last_failure_at, Instant::now());
        let label_budget = total_width
            .saturating_sub(usize::from(WORKER_ROW_LEFT_CHROME))
            .saturating_sub(control_gutter_width())
            .saturating_sub(badge_width);
        let label = truncate_with_ellipsis(worker.label.as_str(), label_budget);
        let label_pad = label_budget.saturating_sub(label.chars().count());
        let is_focused = active_session_key.as_ref() == Some(&worker.session_key);
        let label_style = if is_focused {
            // Active worker - mirror the lead row's focused style
            // (RUST_ORANGE + bold) so the highlight semantics are
            // identical for both row kinds.
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            match worker.status {
                forge_primitives::WorkerLiveness::Running => Style::default(),
                forge_primitives::WorkerLiveness::Spawning => Style::default().fg(theme::DIM),
                // Failed - label renders in STATUS_ERROR so the
                // worker row's overall state reads as "broken" at
                // a glance; the diagnostic sub-row (below the row
                // body) carries the human-readable reason.
                forge_primitives::WorkerLiveness::Failed => {
                    Style::default().fg(theme::STATUS_ERROR)
                }
            }
        };

        // Lifecycle glyph in front of the label - matches the project
        // row's leading glyph column. Prefer the bucket's actual
        // lifecycle (Idle ●, Running spinner, etc.) when present in
        // `app.sessions`; fall back to a Spawning spinner when the
        // worker's Connected hasn't landed yet so the column never
        // collapses to a blank cell.
        //
        // Background-row override (#153, parity with the project-lead
        // row's #152/#137 fix): a non-active worker with a pending
        // permission/question prompt surfaces yellow △ regardless of
        // lifecycle, so the user notices the worker needs attention
        // without switching focus. Focused worker rows keep their
        // normal glyph - the yellow signal is "background worker
        // needs you", not "the one you're looking at."
        let lifecycle = app
            .sessions
            .get(&worker.session_key)
            .map_or(SessionLifecycleState::Spawning, |s| s.lifecycle_state);
        let needs_attention = !is_focused
            && app.sessions.get(&worker.session_key).is_some_and(|b| !b.prompt_queue.is_empty());
        // Same red `✕` the lead row uses for a dead turn - distinct from
        // the yellow `△`, and ahead of it because a prompt whose turn
        // died can no longer be answered.
        let failed_turn = !is_focused
            && app.sessions.get(&worker.session_key).is_some_and(|b| b.failed_turn.is_some());
        // A worker running its own backgrounded task (e.g. a `gh run watch`)
        // spins its row like a lead does - same Idle-only promotion.
        let has_background_work = app
            .sessions
            .get(&worker.session_key)
            .is_some_and(crate::app::session::UiSession::has_live_background_work);
        let (glyph, glyph_color) = if failed_turn {
            ("\u{2715}".to_owned(), theme::STATUS_ERROR)
        } else if needs_attention {
            ("\u{25b3}".to_owned(), theme::STATUS_WARNING)
        } else if matches!(worker.status, forge_primitives::WorkerLiveness::Failed) {
            // Failed worker (#245 Layer A): `✕` in STATUS_ERROR. The
            // diagnostic sub-row beneath this row carries the
            // human-readable reason (set by transition_worker_to_failed).
            ("\u{2715}".to_owned(), theme::STATUS_ERROR)
        } else {
            glyph_for_lifecycle(lifecycle, is_focused, has_background_work, spinner_glyph)
        };

        // Left-indent (1) + org trunk column (3) so the worker's tree
        // connector hangs off the active project's column rather than
        // the org column. Then connector, glyph, label, pad, close
        // button, gutter. Close affordance: ` x ` 3-cell button on
        // USER_MSG_BG slate. Same shape and column as the active
        // project row's close button so the worker rows visually align
        // with the parent.
        let mut spans: Vec<Span<'static>> = vec![
            Span::raw(" "),
            org_trunk_span(parent_is_last),
            Span::styled(tree_glyph.to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(glyph, Style::default().fg(glyph_color)),
            Span::raw(" "),
            Span::styled(label, label_style),
            Span::raw(" ".repeat(label_pad)),
        ];
        spans.extend(badge_spans);
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            crate::app::ROW_CLOSE_BUTTON.to_owned(),
            Style::default().fg(Color::Gray).bg(theme::USER_MSG_BG).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        lines.push(Line::from(spans));

        // Diagnostic sub-row for Failed workers (#245 Layer A): when
        // `worker.diagnostic` is `Some`, append a DIM line directly
        // below the worker row indented to align with the worker's
        // label column. Truncates to the row width with the standard
        // `…` ellipsis. Skipped when status != Failed or diagnostic
        // is None (idle workers + the post-flap window where the
        // recovery path cleared the diagnostic).
        if matches!(worker.status, forge_primitives::WorkerLiveness::Failed) {
            let diagnostic = worker.diagnostic.as_deref().unwrap_or("spawn failed");
            // Aligned to the worker's label column and stopping at the
            // gutter, from the same two values the label itself uses -
            // this row carries no hit target, so a drift here would be
            // silent text running under the close button.
            let indent = usize::from(WORKER_ROW_LEFT_CHROME);
            let total_width = usize::from(area.width);
            let budget = total_width.saturating_sub(indent).saturating_sub(control_gutter_width());
            let truncated = truncate_with_ellipsis(diagnostic, budget);
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(truncated, Style::default().fg(theme::DIM)),
            ]));
        }

        // Hit targets. The label area covers the row up to the
        // control gutter; click on it switches focus to the worker's
        // chat session. The gutter dispatches the close command
        // (mirrors `CloseSession`).
        let close_x_start = crate::app::control_gutter_start(area);
        app.pane_hit_targets.push(PaneHitTarget::WorkerRow {
            project_key: project.key.clone(),
            label: worker.label.clone(),
            session_key: worker.session_key.clone(),
            y: row_y,
            height: 1,
            x_start: area.x,
            x_end: close_x_start,
        });
        let close_x_end = area.x.saturating_add(area.width).saturating_sub(1);
        app.pane_hit_targets.push(PaneHitTarget::CloseWorker {
            project_key: project.key.clone(),
            label: worker.label.clone(),
            y: row_y,
            height: 1,
            x_start: close_x_start,
            x_end: close_x_end,
        });

        // Inter-row breathing gap: the worker-subtree continuation
        // (col 4) plus the org continuation (col 1) when the parent
        // still has siblings below it. Skipped after the last worker -
        // the project-to-project deadzone (or the org break) takes its
        // place, and the subtree has ended so only the org trunk can
        // still be owed.
        if !is_last {
            lines.push(Line::from(vec![
                Span::raw(" "),
                org_trunk_span(parent_is_last),
                Span::styled("\u{2502}".to_owned(), Style::default().fg(theme::DIM)),
            ]));
        }
    }
}

/// The 3-cell org-trunk column a worker subtree sits behind: `│  `
/// while more projects follow in the org, blank once the parent
/// project's `└─` closed the trunk.
fn org_trunk_span(parent_is_last: bool) -> Span<'static> {
    if parent_is_last {
        Span::raw("   ")
    } else {
        Span::styled("\u{2502}  ".to_owned(), Style::default().fg(theme::DIM))
    }
}

/// Left chrome on an org-grouped project row:
/// `<1 PANE_PAD><3 connector><1 glyph><1 sp>`.
const ORG_ROW_LEFT_CHROME: u16 = 6;

/// Left chrome on a worker tree-child row: the project row's, plus the
/// 3-cell org trunk the subtree is indented behind.
const WORKER_ROW_LEFT_CHROME: u16 = ORG_ROW_LEFT_CHROME + 3;

/// Name budget for an org-grouped row: the width less its left chrome
/// and the right-edge control gutter. Derived from
/// [`crate::app::control_gutter_start`]'s own reservation rather than
/// from a matching literal, so the name can never grow into the
/// columns the close band claims.
fn name_budget_org_row(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(ORG_ROW_LEFT_CHROME))
        .saturating_sub(control_gutter_width())
}

/// Cells the control gutter reserves, read back from the geometry the
/// hit band uses so the two cannot be changed apart.
fn control_gutter_width() -> usize {
    let probe = Rect { x: 0, y: 0, width: u16::MAX, height: 1 };
    usize::from(u16::MAX - crate::app::control_gutter_start(probe))
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
/// would already be wrong long before they overflow this cast - but
/// we saturate rather than panic so a runaway list at least caps
/// rather than aborting the renderer.
fn line_count_as_u16(lines: &[Line<'_>]) -> u16 {
    u16::try_from(lines.len()).unwrap_or(u16::MAX)
}

/// Find the `ProjectView` that owns `active_key` - handling the three
/// synthetic-key sentinels (`__conn_pending__`, `__spawn_<name>__`,
/// `__resume_<id>__`) in addition to real claude UUIDs. Without this,
/// every pane reader that does `sessions.iter().any(|s| &s.session
/// == key)` returns `None` during the Spawning window - leaving the
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

/// Glyph + foreground color for a session row based on its lifecycle
/// state. The session-is-active flag drives whether the
/// Running/Spawning spinner picks up the accent color (active +
/// running = `RUST_ORANGE`, background + running = terminal default).
/// `has_background_work` promotes an otherwise-settled session to the
/// spinner while a backgrounded task is live (see
/// [`crate::app::session::UiSession::has_live_background_work`]).
/// `spinner_glyph` is the active style's current frame, resolved by
/// the caller via `App::active_spinner_glyph`.
fn glyph_for_lifecycle(
    lifecycle: SessionLifecycleState,
    session_is_active: bool,
    has_background_work: bool,
    spinner_glyph: char,
) -> (String, Color) {
    // Spinner cases - an in-progress turn, or an otherwise-Idle session with
    // a live backgrounded task - come from the shared session_shows_spinner
    // predicate. The frame-tick gate keys off the same function, so the row
    // glyph and the animation gate never disagree about what animates.
    if crate::app::session::session_shows_spinner(lifecycle, has_background_work) {
        let color = if session_is_active { theme::RUST_ORANGE } else { Color::Reset };
        return (spinner_glyph.to_string(), color);
    }
    match lifecycle {
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
        // #143 item 3: AuthRequired needs distinct visual from
        // Sleeping so the user can tell at a glance which sessions
        // need `claude auth login` vs which are simply idle. ⚠ in
        // STATUS_WARNING mirrors the 5h/7d ETA column's "⚠ expired
        // - /login" treatment from #169.
        SessionLifecycleState::AuthRequired => ("\u{26a0}".to_owned(), theme::STATUS_WARNING),
        SessionLifecycleState::Sleeping
        | SessionLifecycleState::Failed
        | SessionLifecycleState::LoggedOut => ("·".to_owned(), theme::DIM),
        // session_shows_spinner returns true for Running / Spawning, so this
        // arm is unreachable in practice; kept so the match stays exhaustive
        // over the lifecycle enum, and renders the spinner if ever reached.
        SessionLifecycleState::Running | SessionLifecycleState::Spawning => {
            let color = if session_is_active { theme::RUST_ORANGE } else { Color::Reset };
            (spinner_glyph.to_string(), color)
        }
    }
}

/// Duration after which the transient delivery-failure badge (`·N✕`)
/// fades off the row. Counted from `peer_badges_last_failure_at`,
/// which is stamped each time the workspace reports a fresh
/// `delivery_failed` increment. Cumulative
/// outgoing/incoming counts have no fade - they reflect live state
/// while the in-flight asks are pending.
const PEER_FAILURE_FADE: std::time::Duration = std::time::Duration::from_secs(60);

/// Build the peer-activity badge cluster spans for a row. Returns the
/// spans plus the printed width so the caller can shrink `name_budget`
/// before truncating the project label - without this, badges on a
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
    if failures_fresh && stats.delivery_failed > 0 {
        push(Span::styled(
            format!("\u{00b7}{}\u{2715}", stats.delivery_failed),
            Style::default().fg(theme::STATUS_ERROR),
        ));
    }

    (spans, width)
}

// ---------------------------------------------------------------
// Account / status panel - pane footer.
//
// Hard-docked at the bottom of the Projects pane. Reads existing
// `App` accessors (no new wire data, no new reducers). Renders a
// stable-shape block:
//
//   ─────────────────────────
//     Profile  Stargate
//     Mode     Auto
//     Model    Opus 1M
//     Effort   Max
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
// (cells 1-3 green, 4-6 yellow, 7-9 orange, 10-12 red) so the
// rightmost filled cell tells you which zone the bar is in.
//
// Cwd + branch rows live in the Inspector pane's `GIT` section -
// see `crate::ui::inspector_pane`.
// ---------------------------------------------------------------

/// Rows the account panel reserves from the bottom of the pane.
/// Constant by design - values flip but shape stays put (see the
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
/// flow, so the account info is only available via this panel - when
/// it's skipped, the user loses visibility on Mode / Model / Ctx /
/// usage. Acceptable for the ultra-compact-pane edge case; the docked
/// alternative would push the project list out of meaningful range.
const ACCOUNT_PANEL_MIN_PANE_HEIGHT: u16 = 24;

/// Width of the identity-block label column (`Profile`, `Session`,
/// `Mode`, `Model`, `Effort`). Right-padded so the value column
/// aligns regardless of label length.
const ACCOUNT_PANEL_ID_LABEL_WIDTH: usize = 7;

/// Label column for the spend period rows. Five holds `month`, which
/// is the widest of `day` / `week` / `month`.
const SPEND_LABEL_WIDTH: usize = 5;

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
    // A clamped percentage of a cell count, re-clamped to `cells` below.
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

/// Padded label cell - `"<text>"` right-padded with spaces so the
/// next column aligns regardless of label length. Matches the dim
/// styling of the legacy footer's `Loc:` prefix.
fn label_span(text: &'static str, width: usize) -> Span<'static> {
    let mut s = text.to_owned();
    while s.chars().count() < width {
        s.push(' ');
    }
    Span::styled(s, Style::default().fg(theme::DIM))
}

/// Fit `v<version>+<sha>` into `budget` columns by shortening the sha
/// and never the version - a shorter sha still identifies the build,
/// while a clipped version number reads as a different release. The
/// `+` goes with the last hex digit, since it identifies nothing alone.
///
/// Below roughly 20 columns even one hex digit stops fitting; the stamp
/// is returned whole there and the paint clips it, as every other panel
/// row does at that width. An ellipsis would be worse than a clip: it
/// spends a column saying a sha was cut without leaving a matchable one.
fn fit_version_to_budget(version_short: &str, budget: usize) -> String {
    let full = format!("v{version_short}");
    if full.chars().count() <= budget {
        return full;
    }
    match full.split_once('+') {
        Some((base, sha)) if base.chars().count() + 2 <= budget => {
            let room = budget - base.chars().count() - 1;
            format!("{base}+{}", sha.chars().take(room).collect::<String>())
        }
        _ => full,
    }
}

/// Build the panel's lines. Layout is fixed at `ACCOUNT_PANEL_HEIGHT`
/// rows; missing data renders as a dim placeholder so the shape
/// doesn't shift between sessions.
fn build_account_panel_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(ACCOUNT_PANEL_HEIGHT as usize);

    // Row 0: dim rule. No blank after - the identity block sits flush
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
    let profile_value = app.active_account_display_name().unwrap_or_else(|| "\u{2014}".to_owned());
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
        .unwrap_or_else(|| "\u{2014}".to_owned());
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
    // with slate background - copies the FULL session id to the OS
    // clipboard. Button sits flush at the right gutter regardless of
    // value length so its hit column doesn't shift with pane width.
    // Click target stamped in `stamp_session_copy_hit_target`.
    let session_value = app.session_id().map_or_else(
        || "\u{2014}".to_owned(),
        |sid| sid.to_string().chars().take(8).collect::<String>(),
    );
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

    // Model. Display name only - effort lives on its own row below so
    // a long model name can't push it off-screen.
    let model_value = build_model_label(app).unwrap_or_else(|| "\u{2014}".to_owned());
    let model_fitted = truncate_with_ellipsis(&model_value, value_budget);
    lines.push(Line::from(vec![
        Span::raw(" "),
        label_span("Model", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(model_fitted),
    ]));

    // Effort. Always shown - the underlying `EffortLevel` always has
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
    // pattern - DIM, right-justified to the panel's content right
    // edge - but reads the model's raw context-window size
    // (`SessionUsageState.context_max_tokens`) instead of a reset
    // duration. ` - ` when the upstream probe hasn't reported a size
    // yet so the panel's row count stays constant.
    let bar_cells = bar_cells_for(width);
    let ctx_pct = app.session_usage().context_usage_percent.map_or(0.0, f64::from);
    let ctx_pct_str = format!("{:>3}%", app.session_usage().context_usage_percent.unwrap_or(0));
    let mut ctx_line = vec![Span::raw(" "), label_span("Ctx", 3), Span::raw("  ")];
    ctx_line.extend(bar_spans(ctx_pct, bar_cells));
    ctx_line.push(Span::raw("  "));
    ctx_line.push(Span::raw(ctx_pct_str));
    lines.push(Line::from(ctx_line));

    let ctx_size_text = app
        .session_usage()
        .context_max_tokens
        .map_or_else(|| "\u{2014}".to_owned(), format_token_count);
    let ctx_size_chars = ctx_size_text.chars().count();
    let ctx_size_budget = usize::from(width).saturating_sub(PANEL_RIGHT_GUTTER);
    // Shares this row's otherwise-blank left half so the panel's fixed
    // height holds.
    let compactions = app.session_usage().compaction_count;
    let compaction_text = match compactions {
        0 => String::new(),
        1 => " 1 compaction".to_owned(),
        n => format!(" {n} compactions"),
    };
    let ctx_size_fill = ctx_size_budget
        .saturating_sub(ctx_size_chars)
        .saturating_sub(compaction_text.chars().count());
    lines.push(Line::from(vec![
        Span::styled(compaction_text, Style::default().fg(theme::DIM)),
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

    // The account class decides what a failed-probe hint tells the
    // reader to do: `/login` only repairs the keychain class - for a
    // token or base-url account it would re-authenticate whichever
    // sibling owns the shared config dir.
    let account_auth = app
        .workspace
        .as_ref()
        .zip(app.active_account_display_name())
        .and_then(|(ws, name)| ws.account_auth_for(&name))
        .unwrap_or(forge_workspace::AccountAuth::Keychain);

    // 7d cap detection: when the 7d window is at-or-near 100%
    // utilization AND the usage probe hit a 429, the 429 is a
    // downstream consequence of the cap (Anthropic 429s the probe
    // for accounts past budget). Surface "7d cap" on the 5h
    // indicator instead of the generic "rate-limited" so the user
    // reads "budget exhausted, not transient throttle." 99% catches
    // floor + tiny inflight overage. Gating on resets_at > now keeps
    // a stale 100% reading from rendering "7d cap" forever after
    // the window has actually reset - matching the resets_at-driven
    // classification used by the account picker.
    let seven_day_at_cap =
        app.usage().snapshot.as_ref().and_then(|s| s.seven_day.as_ref()).is_some_and(|w| {
            w.utilization >= 99.0
                && w.resets_at.is_some_and(|when| when > std::time::SystemTime::now())
        });

    // Rows 9..=13: what this account bills in. A window-billed
    // account gets the 5h and 7d bars; a spend-billed one gets its
    // periods and cap in the same five rows, because
    // `ACCOUNT_PANEL_HEIGHT` is fixed and a differing row count would
    // move the project list above.
    let spend = app
        .usage()
        .snapshot
        .as_ref()
        .filter(|s| s.source == crate::app::UsageSourceKind::OpenRouterKey)
        .map(|s| s.spend.as_ref());
    if let Some(spend) = spend {
        push_spend_lines(&mut lines, spend, width, usage_error, account_auth);
    } else {
        // Rows 9..=10: 5h bar + ETA row.
        push_usage_window_lines(
            &mut lines,
            "5h",
            app.usage().snapshot.as_ref().and_then(|s| s.five_hour.as_ref()),
            width,
            usage_error,
            seven_day_at_cap,
            account_auth,
        );

        // Row 11: blank between 5h and 7d.
        lines.push(Line::default());

        // Rows 12..=13: 7d bar + ETA row. The "7d cap" label only
        // applies to the 5h indicator (it tells the user the 5h-side
        // 429 is downstream of the 7d cap, not its own thing). The 7d
        // row itself doesn't need that hint - its 100% bar already
        // tells the user the cap is hit. Pass false here so 7d's label
        // (if any) reads its own error class.
        push_usage_window_lines(
            &mut lines,
            "7d",
            app.usage().snapshot.as_ref().and_then(|s| s.seven_day.as_ref()),
            width,
            usage_error,
            false,
            account_auth,
        );
    }

    // Row 14: blank between usage and version rows.
    lines.push(Line::default());

    // Rows 15..=16: forge + claude versions. The claude row shows
    // a yellow `↑ vX.Y.Z` indicator when the npm registry probe
    // reports a strictly-newer published version. Both rows render
    // a DIM ` - ` placeholder when the corresponding probe failed so
    // the panel's row count stays constant.
    // Budgeted rather than fixed-length: the row is 1 pad + label + 2
    // gutter + version, and the version string grows every release, so
    // any constant sha length is wrong on a schedule. Keeps the same
    // right gutter the claude row below already respects.
    let version_budget = usize::from(width)
        .saturating_sub(1 + ACCOUNT_PANEL_ID_LABEL_WIDTH + 2 + PANEL_RIGHT_GUTTER);
    let forge_version = fit_version_to_budget(crate::FORGE_VERSION_SHORT, version_budget);
    lines.push(Line::from(vec![
        Span::raw(" "),
        label_span("forge", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(forge_version),
    ]));

    let cli_info = app.cli_version_info.as_ref();
    let installed = cli_info
        .and_then(|i| i.installed.as_deref())
        .map_or_else(|| "\u{2014}".to_owned(), |v| format!("v{v}"));
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
            // PANEL_RIGHT_GUTTER` cols - same as the bar rows / ETA
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
    seven_day_at_cap: bool,
    auth: forge_workspace::AccountAuth,
) {
    let bar_cells = bar_cells_for(width);
    let pct_value = window.map_or(0.0, |w| w.utilization);
    // A 0..=100 utilisation rounded for a 3-cell display field.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct_text = window.map_or_else(
        || "  \u{2014}%".to_owned(),
        |w| format!("{:>3}%", w.utilization.round() as i64),
    );
    let mut row = vec![Span::raw(" "), label_span(label, 3), Span::raw("  ")];
    row.extend(bar_spans(pct_value, bar_cells));
    row.push(Span::raw("  "));
    row.push(Span::raw(pct_text));
    lines.push(Line::from(row));

    // ETA - when a real reset window exists, show the duration only
    // (no "resets in " prose). When the window is missing, fall
    // back to the last poll-attempt failure label (`⚠ expired`,
    // `rate-limited`, …) so the user can tell an empty bar from an
    // upstream HTTP 429 / expired creds situation. With neither,
    // collapse to ` - ` to keep the row count constant.
    //
    // Color tier: success-path durations stay DIM. Probe-rate-limit
    // / network / fetch-failed labels stay DIM (transient). The two
    // statuses that need the user's attention to recover (Expired,
    // Unauthorized - the account literally can't serve a request
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
            let text = usage_error.map_or_else(
                || "\u{2014}".to_owned(),
                |s| usage_error_label(s, seven_day_at_cap, auth),
            );
            (text, style)
        },
        |duration| (duration, Style::default().fg(theme::DIM)),
    );
    let eta_chars = eta_text.chars().count();
    let right_edge = usize::from(width).saturating_sub(PANEL_RIGHT_GUTTER);
    let pad = right_edge.saturating_sub(eta_chars);
    lines.push(Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(eta_text, eta_style)]));
}

/// The five rows a spend-billed account gets where a window-billed one
/// gets its 5h and 7d bars: `day` / `week` / `month`, then the key's
/// cap, then a secondary line.
///
/// `spend` is `None` when no probe has landed. Every figure then reads
/// `$-` rather than `$0.00`, because a zero is a reading and forge has
/// none - the same distinction the account picker draws.
///
/// `cap` keeps a three-character label while the periods get whole
/// words, so its bar is the same 19 cells as the `Ctx` bar above rather
/// than a shorter one with a ragged edge. That is deliberate: do not
/// "fix" it by spelling the label out.
///
/// Excluded on purpose and re-litigated more than once: `byok_usage_*`
/// is inference billed to a different payer's account, so adding it to
/// these figures would produce a total reconcilable against neither
/// bill. `/v1/credits` is account-wide rather than per-key.
fn push_spend_lines(
    lines: &mut Vec<Line<'static>>,
    spend: Option<&forge_primitives::usage::ApiSpend>,
    width: u16,
    usage_error: Option<forge_workspace::UsageFetchStatus>,
    auth: forge_workspace::AccountAuth,
) {
    let right_edge = usize::from(width).saturating_sub(PANEL_RIGHT_GUTTER);
    let money = |amount: Option<f64>| {
        amount.map_or_else(|| "$-".to_owned(), |value| format!("${value:.2}"))
    };

    for (label, amount) in [
        ("day", spend.map(|s| s.daily)),
        ("week", spend.map(|s| s.weekly)),
        ("month", spend.map(|s| s.monthly)),
    ] {
        let value = money(amount);
        let left = 1 + SPEND_LABEL_WIDTH + 2;
        let pad = right_edge.saturating_sub(left + value.chars().count());
        lines.push(Line::from(vec![
            Span::raw(" "),
            label_span(label, SPEND_LABEL_WIDTH),
            Span::raw("  "),
            Span::raw(" ".repeat(pad)),
            Span::styled(value, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        ]));
    }

    // The cap row reuses the bar-row geometry so its cells line up with
    // `Ctx`. A key with no cap has no denominator to fill, so it says so
    // rather than drawing an empty bar that would read as 0% used.
    match spend.and_then(|s| s.limit.map(|limit| (limit, s.monthly))) {
        Some((limit, monthly)) if limit > 0.0 => {
            let pct = (monthly / limit * 100.0).clamp(0.0, 100.0);
            let cells = bar_cells_for(width);
            // A 0..=100 percentage rounded for a 3-cell display field.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pct_text = format!("{:>3}%", pct.round() as i64);
            let mut row = vec![Span::raw(" "), label_span("cap", 3), Span::raw("  ")];
            row.extend(bar_spans(pct, cells));
            row.push(Span::raw("  "));
            row.push(Span::raw(pct_text));
            lines.push(Line::from(row));
        }
        _ => {
            let text = if spend.is_some() { "not set" } else { "\u{2014}" };
            let left = 1 + 3 + 2;
            let pad = right_edge.saturating_sub(left + text.chars().count());
            lines.push(Line::from(vec![
                Span::raw(" "),
                label_span("cap", 3),
                Span::raw("  "),
                Span::raw(" ".repeat(pad)),
                Span::styled(text.to_owned(), Style::default().fg(theme::DIM)),
            ]));
        }
    }

    // Secondary row, same slot the reset ETA occupies on a window row:
    // what is left of the cap when there is one, otherwise whichever
    // probe failure explains the dashes above.
    let (text, style) = spend_secondary(spend, usage_error, auth);
    let pad = right_edge.saturating_sub(text.chars().count());
    lines.push(Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(text, style)]));
}

/// Text and style for the spend block's last row. Split out so the
/// choice between "what is left", an expiry and a probe failure is
/// testable without rendering a frame.
fn spend_secondary(
    spend: Option<&forge_primitives::usage::ApiSpend>,
    usage_error: Option<forge_workspace::UsageFetchStatus>,
    auth: forge_workspace::AccountAuth,
) -> (String, Style) {
    let dim = Style::default().fg(theme::DIM);
    let Some(spend) = spend else {
        // No snapshot: say why the figures are dashes.
        let style = usage_error.map_or(dim, |s| {
            if needs_user_recovery(s) { Style::default().fg(theme::STATUS_WARNING) } else { dim }
        });
        let text = usage_error
            .map_or_else(|| "no probe yet".to_owned(), |s| usage_error_label(s, false, auth));
        return (text, style);
    };
    if let Some(remaining) = spend.limit_remaining {
        // An expiry displaces the reset cadence rather than claiming a
        // sixth row: a key about to stop working outranks how often its
        // cap rolls over. The `capped` fallback is defensive - a cap
        // with no cadence is a shape the endpoint has not been observed
        // to return.
        let tail = spend.expires_at.as_deref().map_or_else(
            || spend.limit_reset.clone().unwrap_or_else(|| "capped".to_owned()),
            |when| format!("expires {when}"),
        );
        let style = if spend.expires_at.is_some() {
            Style::default().fg(theme::STATUS_WARNING)
        } else {
            dim
        };
        return (format!("${remaining:.2} left \u{00B7} {tail}"), style);
    }
    let style = usage_error.map_or(dim, |s| {
        if needs_user_recovery(s) { Style::default().fg(theme::STATUS_WARNING) } else { dim }
    });
    let text = usage_error
        .map_or_else(|| "no limit set".to_owned(), |s| usage_error_label(s, false, auth));
    (text, style)
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
/// returned 429. Two distinct shapes get surfaced:
///
/// - When the 7d window is at-or-near 100% (budget exhausted),
///   Anthropic 429s the usage probe as a downstream consequence of
///   the cap. The 5h indicator picks up the same 429 even though
///   the 5h budget itself is fine. Surface "7d cap" so the user
///   reads it as "budget exhaustion, not transient throttle."
/// - Otherwise (the typical multi-instance per-IP throttle) show
///   "rate-limited" so the user can tell an empty bar from an
///   upstream throttle.
///
/// Network failures and the catch-all Other class collapse to
/// " - " because they're transient and rarely require user action.
fn usage_error_label(
    status: forge_workspace::UsageFetchStatus,
    seven_day_at_cap: bool,
    auth: forge_workspace::AccountAuth,
) -> String {
    use forge_workspace::UsageFetchStatus;
    match status {
        UsageFetchStatus::Expired => format!("⚠ expired - {}", repair_hint(auth)),
        UsageFetchStatus::Unauthorized => format!("⚠ unauthorized - {}", repair_hint(auth)),
        UsageFetchStatus::RateLimited => {
            if seven_day_at_cap {
                "7d cap".to_owned()
            } else {
                "rate-limited".to_owned()
            }
        }
        UsageFetchStatus::NetworkFailed | UsageFetchStatus::Other => "\u{2014}".to_owned(),
    }
}

/// Where the repair lives, per account class. A keychain account
/// re-authenticates with `/login`; a base-url one re-keys its env
/// token; a token-mode one re-mints its setup token, since `/login`
/// would only re-authenticate whichever sibling owns the shared
/// config dir.
fn repair_hint(auth: forge_workspace::AccountAuth) -> &'static str {
    match auth {
        forge_workspace::AccountAuth::Keychain => "/login",
        forge_workspace::AccountAuth::BaseUrl => "[accounts.env]",
        forge_workspace::AccountAuth::Token => "setup token",
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
        return ("\u{2014}".to_owned(), theme::DIM);
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

/// Model label for the panel - `display_name_short` with the
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
/// - `"Sonnet 4.5"`           → `"Sonnet 4.5"` (no parens - unchanged)
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
/// `…` - there's no room for content + ellipsis at those budgets.
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
            // Losing precision is the point: this renders one decimal place.
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
            // Losing precision is the point: this renders one decimal place.
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
        assert_eq!(truncate_with_ellipsis("stargate-chain-pulse", 12), "stargate-ch…");
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
        let stats = PeerInflightStats { outgoing: 2, incoming: 1, delivery_failed: 0 };
        let (spans, width) = peer_badge_spans(&stats, None, Instant::now());
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("\u{2191}"), "outgoing arrow present: {text}");
        assert!(text.contains("\u{2193}"), "incoming arrow present: {text}");
        assert!(text.contains('2'), "outgoing count present: {text}");
        assert!(text.contains('1'), "incoming count present: {text}");
        // ·2↑·1↓ - 6 chars (· and arrow each count as 1 char).
        assert_eq!(width, 6);
    }

    #[test]
    fn peer_badge_spans_shows_failures_when_fresh() {
        let stats = PeerInflightStats { outgoing: 0, incoming: 0, delivery_failed: 1 };
        let now = Instant::now();
        let (spans, _) = peer_badge_spans(&stats, Some(now), now);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("\u{2715}"), "failure glyph present when fresh: {text}");
    }

    #[test]
    fn peer_badge_spans_fades_failures_after_60s() {
        let stats = PeerInflightStats { outgoing: 0, incoming: 0, delivery_failed: 1 };
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
        assert!(!text.contains("\u{2715}"), "failure glyph faded after 60 s: {text}");
    }

    /// Closes #308 Fix A: worker rows in the Projects pane MUST render
    /// the same peer-activity badge cluster the project-lead row shows.
    /// Bumps already fire correctly on the worker's `session_key`
    /// (`PeerInflightStatsChanged` lands them on
    /// `UiSession.peer_badges`); without this surface the user sees
    /// the counter advance on the lead row but never on the worker
    /// itself.
    #[test]
    fn worker_row_renders_peer_badge_when_stats_present() {
        use crate::app::session::UiSession;
        use forge_workspace::ProjectKey;
        use forge_workspace::SessionKey;
        use forge_workspace::WorkerEntry;
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");
        let project_key = ProjectKey::new_for_test("alice-project");
        let worker_session_key = SessionKey::from_session_id("worker-probe-a");
        let entry = WorkerEntry {
            label: "probe-a".into(),
            charter: "render-badge-test".into(),
            session_key: worker_session_key.clone(),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead".into(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        };
        workspace.insert_live_worker(&project_key, entry);
        // Seed the worker's UiSession with peer_badges so the renderer
        // has a non-default stats value to surface.
        let mut worker_session = UiSession::new(worker_session_key.clone());
        worker_session.peer_badges =
            PeerInflightStats { outgoing: 2, incoming: 1, delivery_failed: 0 };
        app.sessions.insert(worker_session_key.clone(), worker_session);

        let project = ProjectView::new_for_test(
            project_key.clone(),
            "alice-project",
            "/tmp/alice-project",
            Vec::new(),
        );
        let area = Rect { x: 0, y: 0, width: 40, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');

        let joined: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        let worker_row = joined
            .iter()
            .find(|line| {
                line.contains("probe-a")
                    && (line.contains("\u{2514}\u{2500}") || line.contains("\u{251C}\u{2500}"))
            })
            .expect("worker row should render with tree-connector + label");

        assert!(
            worker_row.contains('\u{2191}'),
            "worker row should carry the outgoing arrow ↑; got: {worker_row}"
        );
        assert!(
            worker_row.contains('\u{2193}'),
            "worker row should carry the incoming arrow ↓; got: {worker_row}"
        );
        assert!(
            worker_row.contains('2') && worker_row.contains('1'),
            "worker row should render outgoing=2 + incoming=1; got: {worker_row}"
        );
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    /// Renders on the Ctx bar's size row, whose left half is otherwise
    /// blank, so the panel's fixed row count does not move.
    #[test]
    fn the_ctx_size_row_carries_the_compaction_count() {
        let mut app = App::test_default();
        app.session_usage_mut().compaction_count = 54;
        let rendered = build_account_panel_lines(&app, 32)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("54 compactions"), "got:\n{rendered}");
    }

    #[test]
    fn one_compaction_reads_singular() {
        let mut app = App::test_default();
        app.session_usage_mut().compaction_count = 1;
        let rendered = build_account_panel_lines(&app, 32)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("1 compaction"), "got:\n{rendered}");
        assert!(!rendered.contains("1 compactions"), "got:\n{rendered}");
    }

    /// Paint the pane to a `TestBackend` and return its rows, trailing
    /// blanks trimmed. Painted rather than built, because a row that
    /// overruns the pane and one that fits are indistinguishable in the
    /// `Line` - only the buffer shows what the user actually sees.
    fn painted_rows(app: &mut App, projects: &[ProjectView], width: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, 30);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let area = Rect { x: 0, y: 0, width, height: 30 };
        terminal.draw(|frame| render(frame, area, app, projects)).expect("the pane paints");
        let buffer = terminal.backend().buffer().clone();
        (0..30)
            .map(|y| {
                (0..width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// The pane test derives its expected row from this same fitter, so a
    /// fitter that cuts the version instead of the sha still passes it.
    #[test]
    fn the_fitter_shortens_the_sha_never_the_version() {
        let fitted = fit_version_to_budget("1.0.36+abcdef0123", 13);
        assert_eq!(
            fitted, "v1.0.36+abcde",
            "the version survives whole, the sha carries the cut: [{fitted}]",
        );
    }

    /// #679: at Medium the forge version row ran to 26 columns in a
    /// 24-column pane, so the paint dropped the last two hex digits of
    /// the sha with no ellipsis and nothing to show it had happened.
    ///
    /// The width comes from `PANE_WIDTH_MEDIUM` and the budget from
    /// `PANEL_RIGHT_GUTTER`, so a tier resize cannot leave this test
    /// rendering at a width that is no longer Medium while still passing.
    ///
    /// `assert_eq!` rather than an upper bound: `<=` catches a clipped row
    /// but not an over-trimmed one, and a sha cut to a single hex digit
    /// fits any bound while failing the property this guards - that it
    /// still matches a build.
    ///
    /// The expected row is built through the same `fit_version_to_budget`
    /// call the render makes, so a tarball build - where build.rs emits no
    /// sha and the row is legitimately shorter than the budget - passes too.
    #[test]
    fn the_forge_version_row_fills_the_medium_budget_exactly() {
        let width = crate::ui::layout::PANE_WIDTH_MEDIUM;
        let budget = usize::from(width) - PANEL_RIGHT_GUTTER;
        let version_budget =
            usize::from(width) - (1 + ACCOUNT_PANEL_ID_LABEL_WIDTH + 2 + PANEL_RIGHT_GUTTER);
        let mut app = App::test_default();
        let project_key = forge_workspace::ProjectKey::new_for_test("forge");
        let projects =
            vec![ProjectView::new_for_test(project_key, "forge", "~/Projects/forge", Vec::new())];

        let rows = painted_rows(&mut app, &projects, width);
        let row = rows
            .iter()
            .find(|l| l.trim_start().starts_with("forge "))
            .expect("the forge version row paints");

        let unfitted = format!("v{}", crate::FORGE_VERSION_SHORT);
        let fitted = fit_version_to_budget(crate::FORGE_VERSION_SHORT, version_budget);
        let expected =
            format!(" {:<label$}  {}", "forge", fitted, label = ACCOUNT_PANEL_ID_LABEL_WIDTH);
        assert_eq!(
            *row, expected,
            "the version row renders the label and the fitted stamp, neither clipped nor over-trimmed: [{row}]",
        );

        // The fill and prefix properties only exist when the fitter had to
        // shorten; a short abbreviation (git's floor is 4 hex) fits outright
        // and paints whole, like the gitless stamp.
        if fitted == unfitted {
            assert!(row.ends_with(unfitted.as_str()), "the whole stamp paints: [{row}]");
        } else {
            assert_eq!(
                row.chars().count(),
                budget,
                "the version row fills the Medium budget exactly: [{row}]",
            );
            // Not `contains('+')`: an ellipsis fix satisfies that while leaving
            // the sha unusable. The property is that the shortened sha stays a
            // matchable PREFIX of the real one.
            let painted_sha = row.rsplit('+').next().expect("row splits");
            let (_, full_sha) = crate::FORGE_VERSION_SHORT
                .split_once('+')
                .expect("shortened, so the stamp carries a sha");
            assert!(
                !painted_sha.is_empty() && full_sha.starts_with(painted_sha),
                "the shortened sha stays a clean prefix of the real one, not an elided cut: \
                 painted [{painted_sha}] against [{full_sha}]",
            );
        }

        // Wide has room, so the fix must not reach it. Without this control
        // a budget applied at every tier would look identical at Medium.
        let wide = painted_rows(&mut app, &projects, crate::ui::layout::PANE_WIDTH_WIDE);
        let wide_row = wide
            .iter()
            .find(|l| l.trim_start().starts_with("forge "))
            .expect("the forge version row paints at Wide");
        assert!(
            wide_row.ends_with(&format!("v{}", crate::FORGE_VERSION_SHORT)),
            "Wide has room, so it keeps the untrimmed version stamp: [{wide_row}]",
        );
    }

    /// The compaction text and the context-window size share one row, so
    /// the fill between them has to be reserved for both. At Medium tier
    /// the pane is 24 columns and the two together leave about four
    /// spare, so an unreserved fill pushes the row past the pane instead
    /// of tightening between them.
    #[test]
    fn the_shared_ctx_size_row_stays_within_the_pane_width() {
        for width in [24_u16, 32] {
            let mut app = App::test_default();
            app.session_usage_mut().compaction_count = 54;
            app.session_usage_mut().context_max_tokens = Some(1_000_000);
            let row = build_account_panel_lines(&app, width)
                .iter()
                .map(line_text)
                .find(|t| t.contains("compactions"))
                .expect("the shared row renders");
            assert!(
                row.chars().count() <= usize::from(width),
                "shared row overruns a {width}-column pane at {} chars: [{row}]",
                row.chars().count()
            );
            assert!(row.ends_with("1M"), "the size it shares with is not clipped: [{row}]");
        }
    }

    /// A session that has never compacted shows nothing rather than a
    /// zero - the row is shared with the context-window size and an
    /// always-on `0 compactions` is noise on every fresh session.
    #[test]
    fn a_never_compacted_session_renders_no_compaction_text() {
        let app = App::test_default();
        let rendered = build_account_panel_lines(&app, 32)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("compaction"), "got:\n{rendered}");
    }

    fn spend_snapshot(
        spend: Option<forge_primitives::usage::ApiSpend>,
    ) -> crate::app::UsageSnapshot {
        crate::app::UsageSnapshot {
            source: crate::app::UsageSourceKind::OpenRouterKey,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
            five_hour: None,
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend,
        }
    }

    fn capped(monthly: f64, limit: f64, remaining: f64) -> forge_primitives::usage::ApiSpend {
        forge_primitives::usage::ApiSpend {
            daily: 0.04,
            weekly: 0.04,
            monthly,
            limit: Some(limit),
            limit_remaining: Some(remaining),
            limit_reset: Some("monthly".to_owned()),
            expires_at: None,
        }
    }

    fn uncapped() -> forge_primitives::usage::ApiSpend {
        forge_primitives::usage::ApiSpend {
            daily: 0.56,
            weekly: 4.10,
            monthly: 20.30,
            limit: None,
            limit_remaining: None,
            limit_reset: None,
            expires_at: None,
        }
    }

    /// The layout split subtracts `ACCOUNT_PANEL_HEIGHT` from the pane
    /// to size the project list, so a spend account rendering a
    /// different count shifts that list the moment the user switches
    /// account. `build_account_panel_lines` also carries a
    /// `debug_assert` on this, but the release profile sets no
    /// `debug-assertions` override and so compiles it out - this
    /// assertion is the one that holds in the shipped binary.
    #[test]
    fn every_billing_kind_renders_exactly_the_panel_height() {
        let expected = usize::from(ACCOUNT_PANEL_HEIGHT);
        assert_eq!(
            build_account_panel_lines(&App::test_default(), 32).len(),
            expected,
            "a window-billed account fills the panel exactly",
        );

        for (label, spend) in [
            ("capped", Some(capped(12.40, 20.0, 7.60))),
            ("uncapped", Some(uncapped())),
            ("unprobed", None),
        ] {
            let mut app = App::test_default();
            app.usage_mut().snapshot = Some(spend_snapshot(spend));
            assert_eq!(
                build_account_panel_lines(&app, 32).len(),
                expected,
                "a {label} spend account fills the panel exactly",
            );
        }
    }

    /// The `Ctx` bar draws the same glyphs, so every cap assertion is
    /// scoped to the cap row - checking the whole panel would pass on a
    /// bar the cap row never drew.
    fn cap_row(app: &App) -> String {
        build_account_panel_lines(app, 32)
            .iter()
            .map(line_text)
            .find(|l| l.starts_with(" cap"))
            .expect("cap row present")
    }

    /// A capped key has a denominator, so the cap row draws a bar; an
    /// uncapped one has none and must say so rather than drawing an
    /// empty bar, which would read as nothing used.
    #[test]
    fn the_cap_row_draws_a_bar_only_when_a_cap_exists() {
        let mut app = App::test_default();
        app.usage_mut().snapshot = Some(spend_snapshot(Some(capped(12.40, 20.0, 7.60))));
        let row = cap_row(&app);
        assert!(row.contains('\u{2593}'), "a cap gives the bar something to fill: {row}");
        assert!(row.contains("62%"), "the bar reports usage against the cap: {row}");

        let panel = build_account_panel_lines(&app, 32)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(panel.contains("$7.60 left"), "what is left is the useful number: {panel}");

        let mut app = App::test_default();
        app.usage_mut().snapshot = Some(spend_snapshot(Some(uncapped())));
        let row = cap_row(&app);
        assert!(row.contains("not set"), "an uncapped key says so: {row}");
        assert!(!row.contains('\u{2593}'), "no cap means no bar to fill: {row}");
    }

    /// Every period reads `$-` before a probe lands. `$0.00` is a
    /// reading, and forge has none - the same distinction the account
    /// picker draws, and the whole point of the sibling work.
    #[test]
    fn an_unprobed_spend_account_shows_dashes_not_zeroes() {
        let mut app = App::test_default();
        app.usage_mut().snapshot = Some(spend_snapshot(None));
        let rendered = build_account_panel_lines(&app, 32)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("day"), "the period labels are still drawn: {rendered}");
        assert!(rendered.contains("$-"), "an unread period shows a dash: {rendered}");
        assert!(!rendered.contains("$0.00"), "a zero would be a reading forge does not have");

        let row = cap_row(&app);
        assert!(
            !row.contains('%') && !row.contains('\u{2593}'),
            "an unprobed cap has neither a percentage nor a bar: {row}",
        );
    }

    /// Full words, because these are calendar periods rather than the
    /// rolling windows `5h` / `7d` name. Anchored to the label column:
    /// the secondary row carries the reset cadence, so a bare substring
    /// search finds "month" inside "monthly" and passes whatever the
    /// label says.
    #[test]
    fn the_spend_periods_are_spelled_out() {
        let mut app = App::test_default();
        app.usage_mut().snapshot = Some(spend_snapshot(Some(capped(0.04, 20.0, 19.96))));
        let rendered =
            build_account_panel_lines(&app, 32).iter().map(line_text).collect::<Vec<_>>();

        for word in ["day", "week", "month"] {
            let prefix = format!(" {word:<SPEND_LABEL_WIDTH$}  ");
            assert!(
                rendered.iter().any(|l| l.starts_with(&prefix)),
                "{word} is spelled out in the label column: {rendered:?}",
            );
        }
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
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');

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
        use forge_workspace::WorkerEntry;
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
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
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
            append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');
            // One worker → 2 lines: leading spacer (`│  ` bridge from
            // project lead) + worker row.
            assert_eq!(lines.len(), 2, "baseline: one worker = leading spacer + one row");
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
                worktree: forge_workspace::protocol::WorktreeDisposition::untouched(
                    entry.is_git_repo_at_spawn,
                ),
            },
        );
        assert!(app.needs_redraw, "Removed reducer must request a redraw");

        // Next render reads list_live_workers directly: zero rows.
        let project =
            ProjectView::new_for_test(project_key, "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 32, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');
        assert!(lines.is_empty(), "after Removed, render shows no worker rows");
    }

    #[test]
    fn worker_tree_children_render_with_glyphs_and_close_affordance() {
        use crate::app::PaneHitTarget;
        use forge_workspace::ProjectKey;
        use forge_workspace::SessionKey;
        use forge_workspace::WorkerEntry;
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
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
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
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        let project =
            ProjectView::new_for_test(project_key.clone(), "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 32, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');

        // Two workers → 4 lines: leading spacer (bridge from project
        // lead) + worker + inter-row spacer + worker.
        assert_eq!(lines.len(), 4, "leading spacer + two worker rows + one inter-row spacer");
        let leading: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let row0: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        let spacer: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        let row1: String = lines[3].spans.iter().map(|s| s.content.as_ref()).collect();
        // First worker → not-last → `├─`. Second worker → last → `└─`.
        assert!(row0.contains("\u{251C}\u{2500}"), "first worker row has ├─: {row0:?}");
        assert!(row1.contains("\u{2514}\u{2500}"), "last worker row has └─: {row1:?}");
        assert!(row0.contains("reviewer"), "first worker row has reviewer label: {row0:?}");
        assert!(row1.contains("doc-writer"), "last worker row has doc-writer label: {row1:?}");
        assert!(row0.contains(" x "), "first worker row has close button glyph: {row0:?}");
        assert!(row1.contains(" x "), "last worker row has close button glyph: {row1:?}");
        // Both leading + inter-row spacers carry TWO `│` glyphs so
        // the tree line bridges the gap at both the org column
        // (col 1) and the worker-subtree column (col 4). No `x`
        // button, no label.
        assert_eq!(
            leading.matches('\u{2502}').count(),
            2,
            "leading spacer must carry two │ (org + worker-subtree): {leading:?}",
        );
        assert_eq!(
            spacer.matches('\u{2502}').count(),
            2,
            "inter-row spacer must carry two │ (org + worker-subtree): {spacer:?}",
        );
        assert!(!leading.contains(" x "), "leading spacer has no close button: {leading:?}");
        assert!(!spacer.contains(" x "), "inter-row spacer has no close button: {spacer:?}");
        assert!(!leading.contains("reviewer"), "leading spacer has no label: {leading:?}");
        assert!(!spacer.contains("reviewer"), "inter-row spacer has no label: {spacer:?}");

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

    // ----------------------------------------------------------------
    // #153: worker rows mirror the project-lead row's pending-prompt
    // yellow △ override (#137 / #152). Non-active worker with a
    // pending interaction surfaces △; focused worker keeps its
    // normal lifecycle glyph.
    // ----------------------------------------------------------------

    /// Insert a `PromptState` directly into a worker's prompt_queue
    /// so the override fires without needing to construct a full
    /// PermissionRequest fixture. The override only reads
    /// `prompt_queue.is_empty()`, so any non-empty queue suffices.
    #[cfg(test)]
    fn seed_worker_prompt_queue(app: &mut App, key: &forge_workspace::SessionKey) {
        use crate::app::session::UiSession;
        use forge_primitives::ToolCall;
        use forge_primitives::permission_ui::{
            PermissionAction, PermissionOption, PermissionOptionKind, PermissionRequest,
        };
        let bucket = app.sessions.entry(key.clone()).or_insert_with(|| UiSession::new(key.clone()));
        let request = PermissionRequest {
            tool_call: ToolCall {
                tool_call_id: "tc-test".into(),
                title: "Bash".into(),
                kind: forge_primitives::ToolKind::Execute,
                status: forge_primitives::ToolCallStatus::Pending,
                content: vec![],
                raw_input: None,
                raw_output: None,
                output_metadata: None,
                task_metadata: None,
                locations: vec![],
                meta: None,
            },
            options: vec![PermissionOption {
                option_id: "allow".into(),
                name: "Allow".into(),
                kind: PermissionOptionKind::Allow,
                action: PermissionAction::Allow,
                recommended: false,
            }],
            display: None,
        };
        bucket
            .prompt_queue
            .push_back(crate::app::prompt::PromptState::from_permission("tc-test".into(), request));
    }

    #[test]
    fn worker_row_with_pending_prompt_renders_yellow_triangle() {
        use forge_workspace::ProjectKey;
        use forge_workspace::SessionKey;
        use forge_workspace::WorkerEntry;
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");
        let project_key = ProjectKey::new_for_test("forge");
        let worker_key = SessionKey::from_session_id("worker-1");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "reviewer".into(),
                charter: "be sharp".into(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
        // Active session is something else - the worker is
        // background. Seed its prompt_queue so the override gate
        // fires.
        app.active_session_key = Some(SessionKey::from_session_id("some-other-lead-session"));
        seed_worker_prompt_queue(&mut app, &worker_key);

        let project =
            ProjectView::new_for_test(project_key, "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 32, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');

        // First line is the leading spacer (bridge from project lead);
        // second line is the worker row.
        assert_eq!(lines.len(), 2, "leading spacer + one worker row");
        let glyph_present = lines[1].spans.iter().any(|s| s.content.contains('\u{25b3}'));
        assert!(glyph_present, "worker row must carry yellow △ when prompt_queue is non-empty");
        // Verify the △ span carries STATUS_WARNING color.
        let glyph_span =
            lines[1].spans.iter().find(|s| s.content.contains('\u{25b3}')).expect("△ span present");
        assert_eq!(
            glyph_span.style.fg,
            Some(theme::STATUS_WARNING),
            "△ glyph must use STATUS_WARNING color",
        );
    }

    /// A worker whose turn died carries red `✕` rather than the yellow
    /// `△`: nothing is being asked of the user, the turn is gone. The
    /// failure outranks a stale pending prompt on the same row.
    #[test]
    fn worker_row_with_failed_turn_renders_red_cross_over_triangle() {
        use forge_workspace::{ProjectKey, SessionKey, WorkerEntry};
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");
        let project_key = ProjectKey::new_for_test("forge");
        let worker_key = SessionKey::from_session_id("worker-1");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "reviewer".into(),
                charter: "be sharp".into(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
        app.active_session_key = Some(SessionKey::from_session_id("some-other-lead-session"));
        // Both signals present: the failure must win.
        seed_worker_prompt_queue(&mut app, &worker_key);
        app.sessions.get_mut(&worker_key).expect("seeded bucket").failed_turn =
            Some(crate::app::FailedTurn {
                error: forge_primitives::ApiRetryError::ServerError,
                status: Some(529),
                failed_at: SystemTime::UNIX_EPOCH,
            });

        let project =
            ProjectView::new_for_test(project_key, "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 32, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');

        assert_eq!(lines.len(), 2, "leading spacer + one worker row");
        let glyph_span = lines[1]
            .spans
            .iter()
            .find(|s| s.content.contains('\u{2715}'))
            .expect("failed worker row carries ✕");
        assert_eq!(glyph_span.style.fg, Some(theme::STATUS_ERROR), "✕ must use STATUS_ERROR");
        assert!(
            !lines[1].spans.iter().any(|s| s.content.contains('\u{25b3}')),
            "the failure replaces the yellow △ rather than sitting beside it",
        );
    }

    /// Mirror of `wide_tier_focused_session_with_pending_prompt_keeps_normal_glyph`
    /// for worker rows: an ACTIVE worker (matches `active_session_key`)
    /// with a pending prompt keeps its normal lifecycle glyph - the
    /// yellow signal is "background worker needs you", not "the one
    /// you're already looking at."
    #[test]
    fn active_worker_with_pending_prompt_keeps_normal_glyph() {
        use forge_workspace::ProjectKey;
        use forge_workspace::SessionKey;
        use forge_workspace::WorkerEntry;
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");
        let project_key = ProjectKey::new_for_test("forge");
        let worker_key = SessionKey::from_session_id("worker-1");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "reviewer".into(),
                charter: "be sharp".into(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
        // Active session IS the worker - override must NOT fire.
        app.active_session_key = Some(worker_key.clone());
        seed_worker_prompt_queue(&mut app, &worker_key);

        let project =
            ProjectView::new_for_test(project_key, "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 32, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');

        assert_eq!(lines.len(), 2);
        let any_triangle =
            lines.iter().any(|line| line.spans.iter().any(|s| s.content.contains('\u{25b3}')));
        assert!(!any_triangle, "focused worker with pending prompt must NOT flip to yellow △");
    }

    // ----------------------------------------------------------------
    // #241: resumed-worker project-row highlight. Same family as #232.
    // For resumed workers, cwd_raw carries the worktree path (not the
    // project root) and the catalog tags the JSONL under a per-worker
    // project key (not the parent). Both legacy is_active_project
    // signals (cwd_match, catalog_match) miss this shape, so the
    // project row would never highlight while focus is on a resumed
    // worker. The third signal `worker_match` covers this case via
    // workspace.list_live_workers(project_key).
    // ----------------------------------------------------------------

    #[test]
    fn project_row_highlights_when_active_session_is_a_resumed_worker() {
        use crate::app::session::UiSession;
        use forge_workspace::{ProjectKey, SessionKey, WorkerEntry};
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");

        let project_key = ProjectKey::new_for_test("forge");
        let lead_session_key = SessionKey::from_session_id("lead-session-1");
        let worker_session_key = SessionKey::from_session_id("worker-resume-session-1");

        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "reviewer".into(),
                charter: "be sharp".into(),
                session_key: worker_session_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        // Plant the lead's bucket so the project row has a live lead
        // to render (production state when the user has a project
        // open with at least one running session). Lead's cwd_raw
        // matches the project path so live_session resolution picks
        // it up - that's the LIVE branch (not the IDLE one) where
        // is_active_project drives the row's highlight style.
        let lead_bucket = app
            .sessions
            .entry(lead_session_key.clone())
            .or_insert_with(|| UiSession::new(lead_session_key.clone()));
        lead_bucket.cwd_raw = "~/Projects/forge".to_owned();

        // Active session IS the worker, NOT the lead; mirrors the
        // user pane-switching to a resumed worker. Worker's bucket
        // has cwd_raw pointing at the worktree path (NOT the project
        // root) so the cwd_match signal would miss.
        let worker_bucket = app
            .sessions
            .entry(worker_session_key.clone())
            .or_insert_with(|| UiSession::new(worker_session_key.clone()));
        worker_bucket.cwd_raw = "/Users/test/Projects/forge/.claude/worktrees/reviewer".to_owned();
        app.active_session_key = Some(worker_session_key.clone());

        // ProjectView with NO catalog session matching the worker's
        // session_key (mirrors the resumed-worker case: JSONL tagged
        // under a per-worker project key, not the parent). This rules
        // out the legacy catalog_match signal too.
        let project =
            ProjectView::new_for_test(project_key.clone(), "forge", "~/Projects/forge", Vec::new());

        let area = Rect { x: 0, y: 0, width: 32, height: 30 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_project_rows(&mut lines, area, &mut app, std::slice::from_ref(&project));

        // Focused project rows style the project name in RUST_ORANGE
        // (see append_org_project_row's name_style branch). If the
        // worker_match signal is missing, the row would render in the
        // default-bold style and the project name span would not
        // carry the accent foreground.
        let highlighted = lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.content.contains("forge") && s.style.fg == Some(theme::RUST_ORANGE))
        });
        assert!(
            highlighted,
            "project row must highlight when active session is a resumed worker (cwd_raw + catalog both miss); got lines: {lines:?}",
        );
    }

    // ----------------------------------------------------------------
    // #245 Layer A: WorkerLiveness::Failed renders with ✕ glyph in
    // STATUS_ERROR + a DIM diagnostic sub-row beneath the worker label.
    // ----------------------------------------------------------------

    #[test]
    fn failed_worker_renders_x_glyph_and_diagnostic_sub_row() {
        use forge_workspace::ProjectKey;
        use forge_workspace::SessionKey;
        use forge_workspace::WorkerEntry;
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");
        let project_key = ProjectKey::new_for_test("forge");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "reviewer".into(),
                charter: "be sharp".into(),
                session_key: SessionKey::from_session_id("worker-1"),
                status: forge_primitives::WorkerLiveness::Failed,
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: Some("No conversation found".into()),
                kick: None,
            },
        );

        let project =
            ProjectView::new_for_test(project_key, "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 40, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');

        // Expect: leading spacer + worker row + diagnostic sub-row.
        assert_eq!(lines.len(), 3, "Failed worker row should include a diagnostic sub-line");
        let worker_row: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            worker_row.contains('\u{2715}'),
            "worker row must carry ✕ glyph for Failed status: {worker_row:?}",
        );
        let sub_row: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            sub_row.contains("No conversation found"),
            "diagnostic sub-row must surface the captured stderr: {sub_row:?}",
        );
    }

    #[test]
    fn failed_worker_without_diagnostic_uses_spawn_failed_fallback() {
        // A worker that hit ConnectionFailed before stderr was
        // captured (or with empty stderr) still gets a sub-row so the
        // user sees the failure - just with a generic placeholder.
        use forge_workspace::ProjectKey;
        use forge_workspace::SessionKey;
        use forge_workspace::WorkerEntry;
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");
        let project_key = ProjectKey::new_for_test("forge");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "reviewer".into(),
                charter: "be sharp".into(),
                session_key: SessionKey::from_session_id("worker-1"),
                status: forge_primitives::WorkerLiveness::Failed,
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        let project =
            ProjectView::new_for_test(project_key, "forge", "~/Projects/forge", Vec::new());
        let area = Rect { x: 0, y: 0, width: 40, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, '\u{280B}');

        let sub_row: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            sub_row.contains("spawn failed"),
            "fallback diagnostic must render when diagnostic is None: {sub_row:?}",
        );
    }

    // ----------------------------------------------------------------
    // #160: usage_error_label distinguishes 429 from auth failures
    // and surfaces 7d-cap context when applicable.
    // ----------------------------------------------------------------

    fn keychain() -> forge_workspace::AccountAuth {
        forge_workspace::AccountAuth::Keychain
    }

    #[test]
    fn usage_error_label_rate_limited_says_rate_limited_when_7d_below_cap() {
        let label =
            usage_error_label(forge_workspace::UsageFetchStatus::RateLimited, false, keychain());
        assert_eq!(label, "rate-limited", "429 + 7d-below-cap → rate-limited");
    }

    #[test]
    fn usage_error_label_rate_limited_says_7d_cap_when_7d_at_cap() {
        let label =
            usage_error_label(forge_workspace::UsageFetchStatus::RateLimited, true, keychain());
        assert_eq!(
            label, "7d cap",
            "429 + 7d-at-cap → 7d cap (budget exhaustion, not transient throttle)",
        );
    }

    #[test]
    fn usage_error_label_unauthorized_unchanged() {
        let label =
            usage_error_label(forge_workspace::UsageFetchStatus::Unauthorized, false, keychain());
        assert_eq!(label, "⚠ unauthorized - /login");
        // 7d-at-cap flag must NOT affect auth errors - they need /login
        // regardless of the 7d window state.
        let with_cap =
            usage_error_label(forge_workspace::UsageFetchStatus::Unauthorized, true, keychain());
        assert_eq!(with_cap, "⚠ unauthorized - /login");
    }

    #[test]
    fn usage_error_label_expired_unchanged() {
        let label =
            usage_error_label(forge_workspace::UsageFetchStatus::Expired, false, keychain());
        assert_eq!(label, "⚠ expired - /login");
    }

    /// A token session sharing the config dir would re-authenticate
    /// whichever sibling owns it, so the hint names the setup token
    /// instead of `/login`.
    #[test]
    fn usage_error_label_token_account_names_the_setup_token() {
        let token = forge_workspace::AccountAuth::Token;
        assert_eq!(
            usage_error_label(forge_workspace::UsageFetchStatus::Unauthorized, false, token),
            "⚠ unauthorized - setup token",
        );
        assert_eq!(
            usage_error_label(forge_workspace::UsageFetchStatus::Expired, false, token),
            "⚠ expired - setup token",
        );
    }

    #[test]
    fn usage_error_label_base_url_account_names_accounts_env() {
        let base_url = forge_workspace::AccountAuth::BaseUrl;
        assert_eq!(
            usage_error_label(forge_workspace::UsageFetchStatus::Unauthorized, false, base_url),
            "⚠ unauthorized - [accounts.env]",
        );
        assert_eq!(
            usage_error_label(forge_workspace::UsageFetchStatus::Expired, false, base_url),
            "⚠ expired - [accounts.env]",
        );
    }

    #[test]
    fn usage_error_label_network_and_other_collapse_to_em_dash() {
        assert_eq!(
            usage_error_label(forge_workspace::UsageFetchStatus::NetworkFailed, false, keychain()),
            "\u{2014}",
        );
        assert_eq!(
            usage_error_label(forge_workspace::UsageFetchStatus::Other, false, keychain()),
            "\u{2014}",
        );
    }

    /// Seed a pooled lead bucket for `project_path` in the given lifecycle
    /// state, returning `(app, project)` ready to feed `append_project_rows`.
    /// Mirrors the production lead-resolution path (cwd_raw match, not a
    /// worker) so the row renders through the live branch.
    fn app_with_lead_bucket(
        project_path: &str,
        lifecycle: crate::app::session::SessionLifecycleState,
    ) -> (App, ProjectView, forge_workspace::SessionKey) {
        use crate::app::session::UiSession;
        use forge_workspace::{ProjectKey, SessionKey};

        let mut app = App::test_default();
        let lead_key = SessionKey::from_session_id("lead-bg");
        let mut lead = UiSession::new(lead_key.clone());
        lead.cwd_raw = project_path.to_owned();
        lead.lifecycle_state = lifecycle;
        app.sessions.insert(lead_key.clone(), lead);

        let project = ProjectView::new_for_test(
            ProjectKey::new_for_test("bg-activity-project"),
            "bg-activity-project",
            project_path,
            Vec::new(),
        );
        (app, project, lead_key)
    }

    /// Join the rendered project row that carries `needle` into one string.
    fn rendered_row(lines: &[Line<'static>], needle: &str) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .find(|l| l.contains(needle))
            .expect("project row renders")
    }

    /// Reproduce-first: an Idle lead bucket with a live backgrounded task
    /// must show the active spinner, not the idle bullet. Fails on the
    /// turn-only glyph logic (renders `●`); passes once background work
    /// feeds the glyph decision.
    #[test]
    fn idle_project_with_live_background_task_renders_spinner() {
        use crate::app::BackgroundTask;
        use crate::app::session::SessionLifecycleState;

        let project_path = "/tmp/bg-activity-project";
        let (mut app, project, lead_key) =
            app_with_lead_bucket(project_path, SessionLifecycleState::Idle);
        app.sessions.get_mut(&lead_key).expect("lead bucket").background_tasks.push(
            BackgroundTask {
                task_id: "t1".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "cargo build".to_owned(),
            },
        );
        let frames = app.spinner_style.frames();

        let area = Rect { x: 0, y: 0, width: 44, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_project_rows(&mut lines, area, &mut app, std::slice::from_ref(&project));

        let row = rendered_row(&lines, "bg-activity-project");
        assert!(
            !row.contains('\u{25cf}'),
            "Idle + live background task must not render the idle bullet ●; got: {row}"
        );
        assert!(
            row.chars().any(|c| frames.contains(&c)),
            "Idle + live background task must render the active spinner glyph; got: {row}"
        );
    }

    /// An Idle lead bucket with no background work keeps the idle bullet -
    /// the new signal does not disturb the settled-session glyph.
    #[test]
    fn idle_project_without_background_task_renders_idle_bullet() {
        use crate::app::session::SessionLifecycleState;

        let project_path = "/tmp/bg-activity-project";
        let (mut app, project, _lead_key) =
            app_with_lead_bucket(project_path, SessionLifecycleState::Idle);
        let frames = app.spinner_style.frames();

        let area = Rect { x: 0, y: 0, width: 44, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_project_rows(&mut lines, area, &mut app, std::slice::from_ref(&project));

        let row = rendered_row(&lines, "bg-activity-project");
        assert!(
            row.contains('\u{25cf}'),
            "Idle with no background work keeps the idle bullet ●; got: {row}"
        );
        assert!(
            !row.chars().any(|c| frames.contains(&c)),
            "no spinner when idle with no background work; got: {row}"
        );
    }

    /// A non-focused session with a pending prompt still wins with the
    /// yellow △ even when it also has live background work - attention
    /// override stays ahead of the background-work spinner.
    #[test]
    fn needs_attention_overrides_background_work_spinner() {
        use crate::app::BackgroundTask;
        use crate::app::session::SessionLifecycleState;

        let project_path = "/tmp/bg-activity-project";
        let (mut app, project, lead_key) =
            app_with_lead_bucket(project_path, SessionLifecycleState::Idle);
        // Not the focused row - the △ override only fires on background
        // sessions.
        app.active_session_key = None;
        {
            let lead = app.sessions.get_mut(&lead_key).expect("lead bucket");
            lead.background_tasks.push(BackgroundTask {
                task_id: "t1".to_owned(),
                task_type: "local_bash".to_owned(),
                description: "cargo build".to_owned(),
            });
            lead.prompt_queue.push_back(crate::app::prompt::PromptState::from_permission(
                "tc-bg".to_owned(),
                crate::app::prompt::tests::make_permission_request(),
            ));
        }
        let frames = app.spinner_style.frames();

        let area = Rect { x: 0, y: 0, width: 44, height: 20 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_project_rows(&mut lines, area, &mut app, std::slice::from_ref(&project));

        let row = rendered_row(&lines, "bg-activity-project");
        assert!(
            row.contains('\u{25b3}'),
            "pending prompt shows the yellow △ even with background work; got: {row}"
        );
        assert!(
            !row.chars().any(|c| frames.contains(&c)),
            "△ attention override wins over the background-work spinner; got: {row}"
        );
    }

    /// Background work promotes ONLY an Idle session to the spinner.
    /// Attention / AuthRequired must keep their own glyph so a live task
    /// never masks a session that needs the user (pending prompt / login).
    #[test]
    fn glyph_promotes_to_spinner_only_over_idle() {
        use crate::app::session::SessionLifecycleState;

        let (glyph, _) = glyph_for_lifecycle(SessionLifecycleState::Idle, false, true, 'X');
        assert_eq!(glyph, "X", "Idle + background work shows the spinner");

        let (glyph, _) = glyph_for_lifecycle(SessionLifecycleState::Idle, false, false, 'X');
        assert_eq!(glyph, "\u{25cf}", "Idle + no background work keeps the bullet");

        let (glyph, color) =
            glyph_for_lifecycle(SessionLifecycleState::Attention, false, true, 'X');
        assert_eq!(glyph, "\u{25b3}", "Attention keeps its triangle even with background work");
        assert_eq!(color, theme::STATUS_WARNING);

        let (glyph, color) =
            glyph_for_lifecycle(SessionLifecycleState::AuthRequired, false, true, 'X');
        assert_eq!(glyph, "\u{26a0}", "AuthRequired keeps its warning even with background work");
        assert_eq!(color, theme::STATUS_WARNING);
    }

    /// A worker whose own session is Idle but has a live backgrounded task
    /// spins its row like a lead, via the same Idle-only promotion.
    #[test]
    fn worker_row_spins_on_its_own_background_work() {
        use crate::app::BackgroundTask;
        use crate::app::session::{SessionLifecycleState, UiSession};
        use forge_workspace::{ProjectKey, SessionKey, WorkerEntry};
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("workspace stub");
        let project_key = ProjectKey::new_for_test("bg-worker-project");
        let worker_session_key = SessionKey::from_session_id("worker-bg");
        let entry = WorkerEntry {
            label: "runner".into(),
            charter: "bg-work-test".into(),
            session_key: worker_session_key.clone(),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead".into(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        };
        workspace.insert_live_worker(&project_key, entry);

        let mut worker_session = UiSession::new(worker_session_key.clone());
        worker_session.lifecycle_state = SessionLifecycleState::Idle;
        worker_session.background_tasks.push(BackgroundTask {
            task_id: "t1".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "gh run watch".to_owned(),
        });
        app.sessions.insert(worker_session_key, worker_session);

        let project = ProjectView::new_for_test(
            project_key,
            "bg-worker-project",
            "/tmp/bg-worker-project",
            Vec::new(),
        );
        let area = Rect { x: 0, y: 0, width: 44, height: 20 };
        let spinner = '\u{280B}';
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_worker_tree_children(&mut lines, area, &mut app, &project, false, spinner);

        let row = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .find(|l| l.contains("runner"))
            .expect("worker row renders");
        assert!(
            row.contains(spinner),
            "an Idle worker with a live background task must spin its row; got: {row}"
        );
        assert!(
            !row.contains('\u{25cf}'),
            "worker with background work must not show the idle bullet; got: {row}"
        );
    }

    const ORG_TREE_PANE_WIDTH: u16 = 44;

    /// Render one org ("Test", the `new_for_test` default) holding two
    /// projects - alphabetical order puts `zzz-project` last - with two
    /// live workers hung off `owner`. Returns the joined lines plus the
    /// index of `owner`'s project row, so a test can slice the worker
    /// subtree that follows it.
    fn render_two_project_org(owner: &str) -> (Vec<String>, usize) {
        use forge_workspace::{ProjectKey, SessionKey, WorkerEntry};
        use std::time::SystemTime;

        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test_default seeds a workspace stub");
        let projects: Vec<ProjectView> = ["aaa-project", "zzz-project"]
            .into_iter()
            .map(|name| {
                let key = ProjectKey::new_for_test(name);
                if name == owner {
                    for (idx, label) in ["probe-a", "probe-b"].into_iter().enumerate() {
                        workspace.insert_live_worker(
                            &key,
                            WorkerEntry {
                                label: label.into(),
                                charter: "org-trunk-test".into(),
                                session_key: SessionKey::from_session_id(format!("worker-{idx}")),
                                status: forge_primitives::WorkerLiveness::Running,
                                spawned_at: SystemTime::UNIX_EPOCH,
                                spawned_by_session_id: "lead".into(),
                                needs_tag: false,
                                is_git_repo_at_spawn: false,
                                diagnostic: None,
                                kick: None,
                            },
                        );
                    }
                }
                ProjectView::new_for_test(key, name, format!("/tmp/{name}"), Vec::new())
            })
            .collect();

        let area = Rect { x: 0, y: 0, width: ORG_TREE_PANE_WIDTH, height: 30 };
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_project_rows(&mut lines, area, &mut app, &projects);
        let joined: Vec<String> =
            lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        let owner_row =
            joined.iter().position(|l| l.contains(owner)).expect("owner project row renders");
        (joined, owner_row)
    }

    /// Reproduce-first: a project that is LAST in its org closed the org
    /// trunk with its own `└─`, so its worker rows and the breathing gaps
    /// around them must leave the org column blank. Fails today - the
    /// worker renderer paints `│` there unconditionally, leaving a
    /// vertical line hanging under the last worker.
    #[test]
    fn last_in_org_project_worker_subtree_drops_the_org_trunk() {
        let (lines, owner_row) = render_two_project_org("zzz-project");
        let subtree = &lines[owner_row + 1..];
        assert_eq!(subtree.len(), 4, "leading gap + two worker rows + inter-row gap: {subtree:?}");

        for (idx, line) in subtree.iter().enumerate() {
            assert_eq!(
                line.chars().nth(1),
                Some(' '),
                "org column must be blank once the parent's └─ closed the trunk \
                 (subtree line {idx}): {line:?}"
            );
        }
        // Breathing gaps keep the col-4 worker-subtree trunk - only the
        // org column goes blank.
        assert_eq!(subtree[0], "    \u{2502}", "leading gap: subtree trunk intact");
        assert_eq!(subtree[2], "    \u{2502}", "inter-row gap: subtree trunk intact");
        // Worker rows: connector still lands at col 4 and the row is
        // still exactly pane-wide, so dropping the trunk shifts nothing.
        assert!(
            subtree[1].starts_with("    \u{251C}\u{2500} "),
            "first worker row indent unchanged: {:?}",
            subtree[1]
        );
        assert!(
            subtree[3].starts_with("    \u{2514}\u{2500} "),
            "last worker row indent unchanged: {:?}",
            subtree[3]
        );
        assert_eq!(subtree[1].chars().count(), usize::from(ORG_TREE_PANE_WIDTH));
        assert_eq!(subtree[3].chars().count(), usize::from(ORG_TREE_PANE_WIDTH));
    }

    /// Regression guard: while more projects follow in the org, the
    /// worker subtree keeps painting the org trunk at col 1 alongside
    /// its own trunk at col 4.
    #[test]
    fn non_last_project_worker_subtree_keeps_the_org_trunk() {
        let (lines, owner_row) = render_two_project_org("aaa-project");
        let subtree = &lines[owner_row + 1..owner_row + 5];

        for (idx, line) in subtree.iter().enumerate() {
            assert_eq!(
                line.chars().nth(1),
                Some('\u{2502}'),
                "org trunk stays while more projects follow (subtree line {idx}): {line:?}"
            );
        }
        assert_eq!(subtree[0], " \u{2502}  \u{2502}", "leading gap carries both trunks");
        assert_eq!(subtree[2], " \u{2502}  \u{2502}", "inter-row gap carries both trunks");
        assert!(
            subtree[1].starts_with(" \u{2502}  \u{251C}\u{2500} "),
            "first worker row: {:?}",
            subtree[1]
        );
        assert!(
            subtree[3].starts_with(" \u{2502}  \u{2514}\u{2500} "),
            "last worker row: {:?}",
            subtree[3]
        );
    }
}
