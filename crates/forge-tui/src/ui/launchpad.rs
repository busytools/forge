//! Launchpad view - the project picker, and the floor of the UI. Shown
//! once [`super::preflight`] has handed over, which it does on every
//! route.
//!
//! Three vertical zones: identity block (wordmark + version lines +
//! optional update indicator), picker frame (org-grouped project
//! rows with selection band), footer hint.
//!
//! The side panes hide while launchpad is up. The launchpad owns the
//! entire terminal width; the wordmark + picker stay centered
//! horizontally regardless of terminal width. No tier-specific
//! variants.

use std::time::SystemTime;

use forge_primitives::SessionLifecycleState;
use forge_workspace::{ProjectView, SessionChipInfo, SessionChipState, SessionKey};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::app::App;
use crate::app::launchpad::reconcile_scroll;
use crate::app::view::{ActiveView, set_active_view};

/// ANSI Shadow figlet for "forge". 6 rows × 43 cols. Locked by the
/// design spec - do not tweak.
const FORGE_WORDMARK: [&str; 6] = [
    "███████╗ ██████╗ ██████╗  ██████╗ ███████╗",
    "██╔════╝██╔═══██╗██╔══██╗██╔════╝ ██╔════╝",
    "█████╗  ██║   ██║██████╔╝██║  ███╗█████╗  ",
    "██╔══╝  ██║   ██║██╔══██╗██║   ██║██╔══╝  ",
    "██║     ╚██████╔╝██║  ██║╚██████╔╝███████╗",
    "╚═╝      ╚═════╝ ╚═╝  ╚═╝ ╚═════╝ ╚══════╝",
];

/// Picker frame inner width in cells. Sized to fit the longest
/// project row we expect (`├─ ⠋  service-api    (work-acct)     spawning`)
/// with a generous margin. Constant so the layout is stable across
/// terminal widths.
pub(super) const PICKER_WIDTH: u16 = 56;

/// Width (in cells) of the left-edge selection indicator column.
/// Reserved on every row so unselected rows align with the selected
/// row's arrow + space. The arrow itself is `▶` rendered in
/// `RUST_ORANGE` for clickable selections and `DIM` for Block
/// (Spawning) selections; unselected rows render two spaces.
const SELECTION_PREFIX_WIDTH: usize = 2;

/// Vertical gap between the identity block and the picker box. Zero:
/// the picker's top framing rule already separates it from the
/// identity block, so no spacer row is needed.
const IDENTITY_PICKER_GAP: u16 = 0;

/// Vertical breathing the picker box reserves on a full list: it caps
/// the box below the region height so the centered block keeps a
/// margin above the wordmark and below the box before the footer.
const PICKER_BOX_MARGIN: u16 = 4;

/// What pressing Enter on this row does. Drives the click handlers
/// and the footer hint, so the user always sees the next action that
/// matches the row's current state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClickIntent {
    /// Row is ready to receive input - switch the chat view to it.
    EnterChat,
    /// Cold row - dispatch `SpawnProject` and stay on the launchpad
    /// so the user sees the row transition through `Spawning` rather
    /// than landing on the chat-view connecting stub.
    SpawnAndWait,
    /// Mid-spawn - block the click entirely. Footer explains why.
    Block,
    /// Failed - the `r` key is the explicit retry path. Enter is a
    /// no-op so a stray Enter doesn't dispatch a half-cleaned retry
    /// that races the failed bucket still sitting in `app.sessions`.
    Retry,
}

fn click_intent(lifecycle: SessionLifecycleState) -> ClickIntent {
    use SessionLifecycleState as L;
    match lifecycle {
        L::Idle | L::Running | L::Attention | L::AuthRequired | L::LoggedOut => {
            ClickIntent::EnterChat
        }
        L::Sleeping => ClickIntent::SpawnAndWait,
        L::Spawning => ClickIntent::Block,
        L::Failed => ClickIntent::Retry,
    }
}

/// Effective click intent including the boot-time gate. When the
/// workspace's account-loading tasks haven't all settled, every
/// project row downgrades to `Block` so the launchpad can't spawn
/// against a partial assignment plan. Same downgrade when the
/// project's own pool resolves to empty (every allowed account
/// Bailed) - the row stays unclickable with a `no usable accounts`
/// hint.
fn effective_click_intent(
    app: &App,
    project_name: &str,
    lifecycle: SessionLifecycleState,
) -> ClickIntent {
    let Some(workspace) = app.workspace.as_ref() else {
        return click_intent(lifecycle);
    };
    if !workspace.all_accounts_loaded() {
        return ClickIntent::Block;
    }
    // Resolve the project's pool through the assignment plan. The
    // launchpad only shows projects from forge.toml, so the lookup
    // should always succeed; if it doesn't, conservatively Block.
    let project_key =
        workspace.list_projects().into_iter().find(|p| p.name == project_name).map(|p| p.key);
    let pool_ok = project_key.is_some_and(|k| workspace.project_has_assigned_account(&k));
    if !pool_ok {
        return ClickIntent::Block;
    }
    click_intent(lifecycle)
}

/// One selectable row in the picker - the data the renderer needs
/// to draw the row plus the metadata the keyboard handler needs to
/// resolve a pick into a project + lifecycle.
#[derive(Debug)]
struct PickerRow {
    project_name: String,
    org: String,
    last_activity_label: String,
    lifecycle: SessionLifecycleState,
    /// Last connection error message if the row is in Failed state,
    /// to render below the row.
    error: Option<String>,
    /// True when this row is the last project under its org (drives
    /// `├─` vs `└─`).
    is_last_in_org: bool,
}

/// Build the flat list of picker rows from `app.workspace.list_projects()`.
/// Returns rows grouped by org (sorted alphabetically by org, then by
/// project name). The renderer interleaves org headers + tree
/// connectors at draw time; selection only indexes into this flat
/// project-row list.
fn build_picker_rows(app: &App) -> Vec<PickerRow> {
    let Some(workspace) = app.workspace.as_ref() else {
        return Vec::new();
    };
    let projects = workspace.list_projects();
    if projects.is_empty() {
        return Vec::new();
    }

    let now = SystemTime::now();

    // Bucket by org → alpha order.
    let mut by_org: std::collections::BTreeMap<String, Vec<&ProjectView>> =
        std::collections::BTreeMap::new();
    for project in &projects {
        by_org.entry(project.org.clone()).or_default().push(project);
    }
    for bucket in by_org.values_mut() {
        bucket.sort_by(|a, b| a.name.cmp(&b.name));
    }

    let mut rows = Vec::new();
    for (org, bucket) in &by_org {
        let count = bucket.len();
        for (idx, project) in bucket.iter().enumerate() {
            let lifecycle = resolve_lifecycle(app, project);
            let error = resolve_error(app, project);
            let last_activity_label = format_activity(
                lifecycle,
                project.sessions.first().and_then(|s| s.last_activity).map(|a| (a, now)),
            );
            rows.push(PickerRow {
                project_name: project.name.clone(),
                org: org.clone(),
                last_activity_label,
                lifecycle,
                error,
                is_last_in_org: idx + 1 == count,
            });
        }
    }
    rows
}

/// Find the live `UiSession` bucket for `project`, if any. Three-step
/// resolution mirrors the projects-pane lookup:
///
/// 1. `__spawn_<name>__` synthetic - the pre-Connected placeholder.
/// 2. Catalog session UUIDs - the lead recorded on disk, if pooled.
/// 3. `cwd_raw` match - covers the post-KeyRenamed window when the
///    synthetic has migrated to the real session UUID but the
///    catalog scan hasn't refreshed yet.
fn find_live_bucket<'app>(
    app: &'app App,
    project: &ProjectView,
) -> Option<&'app crate::app::session::UiSession> {
    let spawn_synthetic = SessionKey::from_session_id(format!("__spawn_{}__", project.name));
    if let Some(session) = app.sessions.get(&spawn_synthetic) {
        return Some(session);
    }
    for sess in &project.sessions {
        if let Some(bucket) = app.sessions.get(&sess.session) {
            return Some(bucket);
        }
    }
    let path_str = project.path.to_string_lossy();
    app.sessions.values().find(|s| s.cwd_raw.as_str() == path_str.as_ref())
}

fn resolve_lifecycle(app: &App, project: &ProjectView) -> SessionLifecycleState {
    find_live_bucket(app, project).map_or(SessionLifecycleState::Sleeping, |s| s.lifecycle_state)
}

fn resolve_error(app: &App, project: &ProjectView) -> Option<String> {
    find_live_bucket(app, project).and_then(|s| s.last_connection_error.clone())
}

/// `since` is `(last_activity, now)`, absent for a row with no
/// timestamp to render - which is every worker row, since the clock is
/// only ever read against a timestamp.
fn format_activity(
    lifecycle: SessionLifecycleState,
    since: Option<(SystemTime, SystemTime)>,
) -> String {
    match lifecycle {
        SessionLifecycleState::Spawning => "spawning".to_owned(),
        SessionLifecycleState::Failed => "failed".to_owned(),
        _ => match since {
            Some((activity, now)) => format_relative_time(activity, now),
            None => "\u{2014}".to_owned(),
        },
    }
}

use super::format::relative_time as format_relative_time;

/// Render the launchpad view. Owns the full frame area; both side
/// panes are hidden upstream when `app.active_view == Launchpad`.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.cached_frame_area = area;
    let rows = build_picker_rows(app);
    // First-render selection default: pick the most recently active
    // project. After the first frame the user has moved the cursor;
    // we don't override their choice on subsequent renders. We detect
    // "first frame" by checking whether selected_index == 0 AND the
    // launchpad was opened less than a render-tick ago. Cheap proxy.
    if app.launchpad.selected_index == 0
        && app.launchpad.opened_at.elapsed().as_millis() < 50
        && let Some(idx) = most_recently_active_index(&rows)
    {
        app.launchpad.selected_index = idx;
    }
    // Clamp selection if the picker shrank since the user last moved.
    let max_index = rows.len().saturating_sub(1);
    if app.launchpad.selected_index > max_index {
        app.launchpad.selected_index = max_index;
    }

    // Clamp the picker frame width to the terminal width with a
    // 4-col side margin minimum.
    let picker_inner_width = PICKER_WIDTH.min(area.width.saturating_sub(8));
    let picker_outer_width = picker_inner_width;

    // Build the scrollable picker content once so render can size the
    // box before placing the whole block.
    let (content_lines, selected_flat) = build_picker_content(app, &rows, picker_outer_width);
    let content_count = content_lines.len();

    // Vertical layout: the identity block + picker box ride together as
    // one centered unit; the footer hint stays pinned to the last row.
    let identity_height = identity_block_height(app);
    let footer_height: u16 = 1;
    let footer_top = area.y + area.height.saturating_sub(footer_height);
    let available = footer_top.saturating_sub(area.y);

    let picker_region = available.saturating_sub(identity_height + IDENTITY_PICKER_GAP);
    let box_height = picker_box_height(content_count, picker_region);

    let block_top =
        area.y + block_top_offset(area.height, identity_height, box_height, footer_height);
    render_identity_block(frame, area, app, block_top);

    let region = Rect {
        x: area.x + area.width.saturating_sub(picker_outer_width) / 2,
        y: block_top + identity_height + IDENTITY_PICKER_GAP,
        width: picker_outer_width,
        height: box_height,
    };
    render_picker(frame, region, app, content_lines, selected_flat);

    render_footer(
        frame,
        Rect { x: area.x, y: footer_top, width: area.width, height: footer_height },
        app,
        &rows,
    );
}

/// Picker box height (the framed list area, including its two rule
/// rows) for `content_count` lines, given `region_height` - the most
/// vertical space the box may occupy. The box grows to the content
/// plus its two framing rules, then caps at
/// `region_height - PICKER_BOX_MARGIN` so a long list leaves the
/// centered block breathing, never exceeding `region_height` itself.
fn picker_box_height(content_count: usize, region_height: u16) -> u16 {
    let max_box = region_height.saturating_sub(PICKER_BOX_MARGIN).max(3);
    let desired_box = u16::try_from(content_count).unwrap_or(u16::MAX).saturating_add(2);
    desired_box.min(max_box).min(region_height)
}

/// Vertical offset (rows below `area.y`) at which the centered block -
/// identity + [`IDENTITY_PICKER_GAP`] + picker box - begins, so the
/// whole block sits centered in the space above the footer. A block
/// taller than that space clamps to 0 (top-aligned); the picker box
/// cap keeps it from overlapping the footer.
fn block_top_offset(
    area_height: u16,
    identity_height: u16,
    box_height: u16,
    footer_height: u16,
) -> u16 {
    let available = area_height.saturating_sub(footer_height);
    let total_block =
        identity_height.saturating_add(IDENTITY_PICKER_GAP).saturating_add(box_height);
    available.saturating_sub(total_block) / 2
}

/// `true` when `needle` appears in a wordmark row. Exists so a test
/// asserting the wordmark is absent can first prove its needle would
/// have matched a present one.
#[cfg(test)]
pub(super) fn wordmark_contains(needle: &str) -> bool {
    FORGE_WORDMARK.iter().any(|row| row.contains(needle))
}

/// `true` when the per-account glyph row has something to say: some
/// account is not `Ready`. All-`Ready` hides it rather than showing a
/// permanently green line.
///
/// **Deliberately wider than the click gate, not the same condition.**
/// `all_accounts_loaded()` counts `Bailed` as terminal, so a bailed
/// account lifts the gate and leaves the rows clickable - and is still
/// worth surfacing. The row covers that as well as the mid-flight
/// window, where the rows really are blocked and this is what says why.
///
/// Not a boot-time condition either. A token expiring mid-session takes
/// an account `Ready -> Bailed` on the usage poll, and the recovery poll
/// then takes it to `Loading`, so the row reappears whenever that
/// happens.
pub(super) fn account_row_visible(app: &App) -> bool {
    app.workspace.as_ref().is_some_and(|ws| {
        ws.account_loading_snapshot()
            .iter()
            .any(|row| row.state != forge_workspace::LoadingState::Ready)
    })
}

/// Line count for the identity block: 6 wordmark rows, the version
/// line, the claude line, an optional update indicator, and the
/// per-account status row (blank separator + chips) when
/// [`account_row_visible`] says so.
pub(super) fn identity_block_height(app: &App) -> u16 {
    // Counted off the lines themselves rather than restated, so the
    // layout and the paint cannot disagree about the update indicator.
    let mut h = identity_lines(app, 0).len();
    if account_row_visible(app) {
        h += 2;
    }
    u16::try_from(h).unwrap_or(u16::MAX)
}

/// Wordmark, version, claude version, and the update indicator when one
/// is available, centred in `width`. Shared with the preflight screen,
/// which draws the same header over a different body.
pub(super) fn identity_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let wordmark_style = Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD);
    for row in FORGE_WORDMARK {
        lines.push(centered_text_line(row, width, wordmark_style));
    }
    let dim = Style::default().fg(theme::DIM);
    let forge_line = format!("v{}", crate::FORGE_VERSION_SHORT);
    lines.push(centered_text_line(&forge_line, width, dim));
    let claude_label = match app.cli_version_info.as_ref().and_then(|c| c.installed.as_deref()) {
        Some(installed) => format!("claude {installed}"),
        None => "claude (unknown)".to_owned(),
    };
    lines.push(centered_text_line(&claude_label, width, dim));
    if let Some(cli) = app.cli_version_info.as_ref()
        && cli.has_update()
        && let Some(latest) = cli.latest.as_deref()
    {
        let update = format!("↑ v{latest} available");
        lines.push(centered_text_line(&update, width, Style::default().fg(theme::RUST_ORANGE)));
    }
    lines
}

fn render_identity_block(frame: &mut Frame, area: Rect, app: &App, y: u16) {
    let mut lines = identity_lines(app, area.width);
    // Per-account glyph row, present only while some account is
    // mid-flight - which is exactly when every project row is blocked,
    // so this is what says why. Order matches forge.toml's
    // `[[accounts]]` so the user can scan it left-to-right against
    // their own mental layout.
    if let Some(workspace) = app.workspace.as_ref()
        && account_row_visible(app)
    {
        lines.push(Line::default());
        lines.push(centered_account_status_line(&workspace.account_loading_snapshot(), area.width));
    }
    let block_area =
        Rect { x: area.x, y, width: area.width, height: u16::try_from(lines.len()).unwrap_or(0) };
    frame.render_widget(Paragraph::new(lines), block_area);
}

/// Render a centred line of per-account state chips. Each chip is
/// `<glyph> <name>` separated by `  ` (two spaces) so the user can
/// distinguish chips at a glance without staring at the row.
fn centered_account_status_line(
    snapshot: &[forge_workspace::AccountLoadingRow],
    area_width: u16,
) -> Line<'static> {
    let dim = Style::default().fg(theme::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text_width: usize = 0;
    for (idx, row) in snapshot.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("  ".to_owned(), dim));
            text_width += 2;
        }
        let (glyph, color) = super::preflight::account_glyph(row.state);
        spans.push(Span::styled(glyph.to_owned(), Style::default().fg(color)));
        spans.push(Span::styled(format!(" {}", row.display_name), dim));
        text_width += 1 + 1 + row.display_name.chars().count();
    }
    let pad = usize::from(area_width).saturating_sub(text_width) / 2;
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len() + 1);
    out.push(Span::raw(" ".repeat(pad)));
    out.extend(spans);
    Line::from(out)
}

pub(super) fn centered_text_line(text: &str, area_width: u16, style: Style) -> Line<'static> {
    let text_width = text.chars().count();
    let pad = usize::from(area_width).saturating_sub(text_width) / 2;
    Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(text.to_owned(), style)])
}

/// Build the scrollable picker content - everything that sits between
/// the two framing rules: org headers, project rows, and any
/// error/worker/no-usable-accounts rows. Returns the lines plus the
/// flat-line index of the selected project's row (so the scroll
/// window can follow the selection). `width` is the box inner width,
/// used to truncate row content.
fn build_picker_content(
    app: &App,
    rows: &[PickerRow],
    width: u16,
) -> (Vec<Line<'static>>, Option<usize>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_flat: Option<usize> = None;
    let dim = Style::default().fg(theme::DIM);
    // One store scan and one registry snapshot for the whole picker;
    // the per-project lookups below are map indexes.
    let worker_labels =
        app.workspace.as_ref().map(|ws| ws.dynamic_worker_labels_by_project()).unwrap_or_default();
    let live_workers =
        app.workspace.as_ref().map(|ws| ws.live_worker_states_by_project()).unwrap_or_default();

    let mut last_org: Option<String> = None;
    for (project_row_idx, row) in rows.iter().enumerate() {
        let org_change = last_org.as_deref() != Some(row.org.as_str());
        if org_change {
            if last_org.is_some() {
                lines.push(Line::default());
            }
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(row.org.clone(), dim.add_modifier(Modifier::BOLD)),
            ]));
            last_org = Some(row.org.clone());
        }
        let selected = project_row_idx == app.launchpad.selected_index;
        if selected {
            selected_flat = Some(lines.len());
        }
        push_project_row(&mut lines, row, selected, app, width);
        if let Some(err) = &row.error {
            push_error_row(&mut lines, err, width);
        }
        // Worker rows: the project's persisted dynamic workers nested
        // under it. Info-only, not selectable.
        let project_view = app
            .workspace
            .as_ref()
            .map(|ws| ws.list_projects())
            .and_then(|list| list.into_iter().find(|p| p.name == row.project_name));
        if let Some(project) = project_view.as_ref()
            && let Some(labels) = worker_labels.get(project.key.as_str())
        {
            let live = live_workers.get(&project.key).map_or(&[][..], Vec::as_slice);
            push_worker_rows(&mut lines, project, app, labels, live);
        }
        // Surface a "no usable accounts" hint when the project's
        // pool resolved to empty (every allowed account Bailed, or
        // forge.toml allow-list has no known accounts). The row
        // stays unclickable via `effective_click_intent`'s Block
        // downgrade; the hint explains why.
        if let Some(workspace) = app.workspace.as_ref()
            && workspace.all_accounts_loaded()
            && project_view
                .as_ref()
                .is_some_and(|p| !workspace.project_has_assigned_account(&p.key))
        {
            push_no_usable_accounts_row(&mut lines, width);
        }
    }

    (lines, selected_flat)
}

/// Paint the picker box into `area`, which `render` has already sized
/// and placed (so the box top-aligns within the rect). The first and
/// last rows are the top/bottom rules; the rest is the list viewport,
/// which windows `content_lines` by `scroll_offset` so the selected
/// project stays visible. A scrollbar thumb appears on overflow.
fn render_picker(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    content_lines: Vec<Line<'static>>,
    selected_flat: Option<usize>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let dim = Style::default().fg(theme::DIM);
    let content_count = content_lines.len();

    let box_top = area.y;
    let viewport_h = area.height.saturating_sub(2);

    let offset = reconcile_scroll(
        selected_flat.unwrap_or(0),
        usize::from(viewport_h),
        content_count,
        app.launchpad.scroll_offset,
    );
    app.launchpad.scroll_offset = offset;

    // Reserve a 1-col gutter at the right edge for the scrollbar when
    // the list overflows the box.
    let overflow = content_count > usize::from(viewport_h);
    let gutter = u16::from(overflow);

    let rule_width = usize::from(area.width);
    let rule = || Line::from(Span::styled("─".repeat(rule_width), dim));

    frame.render_widget(
        Paragraph::new(rule()),
        Rect { x: area.x, y: box_top, width: area.width, height: 1 },
    );
    if viewport_h > 0 {
        let list_area = Rect {
            x: area.x,
            y: box_top + 1,
            width: area.width.saturating_sub(gutter),
            height: viewport_h,
        };
        frame.render_widget(Paragraph::new(content_lines).scroll((offset, 0)), list_area);
    }
    frame.render_widget(
        Paragraph::new(rule()),
        Rect { x: area.x, y: box_top + 1 + viewport_h, width: area.width, height: 1 },
    );

    if overflow {
        render_picker_thumb(
            frame,
            area.x,
            box_top + 1,
            area.width,
            viewport_h,
            content_count,
            offset,
        );
    }
}

/// Paint the launchpad scrollbar in the box's right-edge gutter: a
/// DIM `│` track with a RUST_ORANGE `▐` thumb sized + positioned by
/// the shared [`crate::app::compute_scrollbar_geometry`]. The caller
/// gates this on the list overflowing the viewport.
fn render_picker_thumb(
    frame: &mut Frame,
    x: u16,
    top_y: u16,
    width: u16,
    viewport: u16,
    total: usize,
    offset: u16,
) {
    let Some(geometry) =
        crate::app::compute_scrollbar_geometry(total, usize::from(viewport), f32::from(offset))
    else {
        return;
    };
    let rail_x = x + width.saturating_sub(1);
    let track_style = Style::default().fg(theme::DIM);
    let thumb_style = Style::default().fg(theme::RUST_ORANGE);
    let buf = frame.buffer_mut();
    for row in 0..usize::from(viewport) {
        let in_thumb = row >= geometry.thumb_top && row < geometry.thumb_top + geometry.thumb_size;
        let y = top_y + u16::try_from(row).unwrap_or(u16::MAX);
        if let Some(cell) = buf.cell_mut((rail_x, y)) {
            if in_thumb {
                cell.set_symbol("▐");
                cell.set_style(thumb_style);
            } else {
                cell.set_symbol("│");
                cell.set_style(track_style);
            }
        }
    }
}

fn push_project_row(
    lines: &mut Vec<Line<'static>>,
    row: &PickerRow,
    selected: bool,
    app: &App,
    _area_width: u16,
) {
    let connector = if row.is_last_in_org { "└─" } else { "├─" };
    let (glyph, glyph_color) = glyph_for_row(row.lifecycle, app.active_spinner_glyph());
    let intent = effective_click_intent(app, &row.project_name, row.lifecycle);
    // Base name style - BOLD when the row is interactive (Idle /
    // Running / Sleeping / Failed), DIM when not (Spawning waits for
    // its subprocess). Selection on a clickable row layers on its
    // own emphasis (the arrow + the row staying BOLD); selection on
    // a Block row keeps the DIM name so the "not yet clickable"
    // signal isn't lost.
    let name_style = match intent {
        ClickIntent::EnterChat | ClickIntent::SpawnAndWait | ClickIntent::Retry => {
            Style::default().add_modifier(Modifier::BOLD)
        }
        ClickIntent::Block => Style::default().fg(theme::DIM),
    };

    // Account chip: the assignment-plan slot for this project's lead
    // session. Populated once accounts finish loading + the plan
    // computes. Padded to a fixed column (CHIP_COLUMN_WIDTH) so chips
    // land at the same x across every project and worker row.
    let chip_info = app
        .workspace
        .as_ref()
        .and_then(|ws| find_project_key(ws.list_projects().as_slice(), &row.project_name))
        .and_then(|key| app.workspace.as_ref().and_then(|ws| ws.session_chip_for(&key, "lead")));
    let (chip_spans, chip_width) = account_chip_spans(chip_info.as_ref());

    // Fixed column widths so rows align across projects + workers:
    //   chrome (7) + name (PROJECT_NAME_WIDTH) + chip column (padded
    //   to CHIP_COLUMN_WIDTH) + right (time).
    let name_label = truncate_to(&row.project_name, PROJECT_NAME_WIDTH);
    let name_pad = PROJECT_NAME_WIDTH.saturating_sub(name_label.chars().count());
    let chip_col_pad = CHIP_COLUMN_WIDTH.saturating_sub(chip_width);
    let right_width: usize = ACTIVITY_COLUMN_WIDTH;
    let right_label = truncate_to(&row.last_activity_label, right_width);

    let dim = Style::default().fg(theme::DIM);
    let prefix_span = if selected {
        // Selection indicator: `▶` followed by a space. Color matches
        // the row's click intent so the picker telegraphs both
        // "this row is focused" and "Enter does / does not do
        // anything here" in one glyph.
        let prefix_color =
            if matches!(intent, ClickIntent::Block) { theme::DIM } else { theme::RUST_ORANGE };
        Span::styled(
            "▶ ".to_owned(),
            Style::default().fg(prefix_color).add_modifier(Modifier::BOLD),
        )
    } else {
        // Reserve the same width so unselected rows align with the
        // arrow column on the selected row.
        Span::raw(" ".repeat(SELECTION_PREFIX_WIDTH))
    };

    let mut spans: Vec<Span<'static>> = vec![
        prefix_span,
        Span::styled(connector.to_owned(), dim),
        Span::raw(" ".to_owned()),
        Span::styled(glyph, Style::default().fg(glyph_color)),
        Span::raw(" ".to_owned()),
        Span::styled(name_label, name_style),
        Span::raw(" ".repeat(name_pad)),
    ];
    spans.extend(chip_spans);
    spans.push(Span::raw(" ".repeat(chip_col_pad)));
    spans.push(Span::raw("  ".to_owned()));
    spans.push(Span::styled(format!("{right_label:>right_width$}"), dim));
    lines.push(Line::from(spans));
}

/// Project-row name column width. Fixed so the trailing chip column
/// + time column land at the same x across every project row.
const PROJECT_NAME_WIDTH: usize = 14;

/// Worker-row name column width. Workers are indented 3 cells deeper
/// (the worker subtree connector) and carry the same lifecycle glyph +
/// separating space the project row does, so the name column is
/// correspondingly narrower to keep the trailing chip column aligned
/// with project rows.
const WORKER_NAME_WIDTH: usize = 9;

/// Right-aligned activity column width, shared by project and worker
/// rows so both end at the same x.
const ACTIVITY_COLUMN_WIDTH: usize = 10;

/// Chip column width - the leading-space + `(<account>)` chip pads to
/// this width so the time column (project rows) lands at a fixed x.
/// 13 = 1 space + 12 (CHIP_MAX_WIDTH in `account_chip_spans`).
const CHIP_COLUMN_WIDTH: usize = 13;

/// Append one row per persisted dynamic worker for this project,
/// directly below the project's row. Each row carries what the project
/// row carries - lifecycle glyph, name, assigned-account chip from the
/// AssignmentPlan, right-aligned activity - so the user can see the
/// per-session account mapping and each worker's state before clicking
/// the project. Workers are info-only on the launchpad: clicks land on
/// the project lead row, and the worker rows are not selectable.
///
/// `labels` are this project's persisted dynamic workers and `live` its
/// registry entries, both of which the caller reads for the whole picker
/// in one pass. The labels come from the persisted rows rather than the
/// registry, which is empty until the project launches - the state the
/// launchpad renders in.
fn push_worker_rows(
    lines: &mut Vec<Line<'static>>,
    project: &ProjectView,
    app: &App,
    labels: &[String],
    live: &[forge_workspace::LiveWorkerState],
) {
    let Some(workspace) = app.workspace.as_ref() else {
        return;
    };
    let dim = Style::default().fg(theme::DIM);
    let count = labels.len();
    for (idx, label) in labels.iter().enumerate() {
        let is_last = idx + 1 == count;
        let tree_glyph = if is_last { "└─" } else { "├─" };
        let lifecycle = worker_lifecycle(app, live, label);
        let (glyph, glyph_color) = glyph_for_row(lifecycle, app.active_spinner_glyph());
        let chip_info = workspace.session_chip_for(&project.key, label);
        let (chip_spans, chip_width) = account_chip_spans(chip_info.as_ref());
        let name_label = truncate_to(label, WORKER_NAME_WIDTH);
        let name_pad = WORKER_NAME_WIDTH.saturating_sub(name_label.chars().count());
        let chip_col_pad = CHIP_COLUMN_WIDTH.saturating_sub(chip_width);
        // Workers run in worktrees, which the catalog keys separately
        // from their project, so no per-worker last-activity reaches here.
        let right_label = truncate_to(&format_activity(lifecycle, None), ACTIVITY_COLUMN_WIDTH);

        // Worker-row chrome: 2 (selection prefix) + 2 (`│ `) + 3
        // (vertical continuation gap) + 2 (`├─`/`└─` tree connector)
        // + 1 (sp) + 1 (glyph) + 1 (sp) = 12 cells. Project-row chrome
        // is 7 cells + PROJECT_NAME_WIDTH (14) = 21, so worker
        // chrome+name = 12 + 9 = 21 - same trailing column for the chip.
        let mut spans: Vec<Span<'static>> = vec![
            Span::raw(" ".repeat(SELECTION_PREFIX_WIDTH)),
            Span::styled("│ ".to_owned(), dim),
            Span::raw(" ".repeat(3)),
            Span::styled(tree_glyph.to_owned(), dim),
            Span::raw(" ".to_owned()),
            Span::styled(glyph, Style::default().fg(glyph_color)),
            Span::raw(" ".to_owned()),
            Span::styled(name_label, Style::default().fg(theme::DIM)),
            Span::raw(" ".repeat(name_pad)),
        ];
        spans.extend(chip_spans);
        spans.push(Span::raw(" ".repeat(chip_col_pad)));
        spans.push(Span::raw("  ".to_owned()));
        spans.push(Span::styled(format!("{right_label:>ACTIVITY_COLUMN_WIDTH$}"), dim));
        lines.push(Line::from(spans));
    }
}

/// What the worker labelled `label` is doing right now. The two states
/// with no session to interrogate come from the live entry; the rest
/// resolves through the same `UiSession` bucket the project row reads.
/// `Sleeping` when the label has no live worker - a persisted row that
/// has not spawned this boot.
///
/// A Running worker whose bucket has not arrived falls back to
/// `Spawning`, matching what the Projects pane renders for the same
/// worker in the same instant (see `append_worker_tree_children`): the
/// gap is a `Connected` the TUI has not drained yet.
fn worker_lifecycle(
    app: &App,
    live: &[forge_workspace::LiveWorkerState],
    label: &str,
) -> SessionLifecycleState {
    use forge_primitives::WorkerLiveness;
    let Some(entry) = live.iter().find(|w| w.label == label) else {
        return SessionLifecycleState::Sleeping;
    };
    match entry.status {
        WorkerLiveness::Spawning => SessionLifecycleState::Spawning,
        WorkerLiveness::Failed => SessionLifecycleState::Failed,
        WorkerLiveness::Running => app
            .sessions
            .get(&entry.session_key)
            .map_or(SessionLifecycleState::Spawning, |s| s.lifecycle_state),
    }
}

/// Build the per-session account chip spans + printed width.
/// `(spans, width)`; on no-chip-yet returns `(vec![], 0)` so callers
/// can unconditionally extend.
///
/// Chip text is `[<account>]`; color tracks the underlying
/// `SessionChipState`: `Normal` = DIM, `AtCap` =
/// STATUS_WARNING, `Bailed` = STATUS_ERROR with a `⚠ ` prefix. The
/// account name truncates to fit within `CHIP_MAX_WIDTH - 2`
/// brackets minus the prefix.
fn account_chip_spans(chip: Option<&SessionChipInfo>) -> (Vec<Span<'static>>, usize) {
    const CHIP_MAX_WIDTH: usize = 12;
    let Some(chip) = chip else {
        return (Vec::new(), 0);
    };
    let (style, prefix) = match chip.state {
        SessionChipState::Normal => (Style::default().fg(theme::DIM), ""),
        SessionChipState::AtCap => (Style::default().fg(theme::STATUS_WARNING), ""),
        SessionChipState::Bailed => (Style::default().fg(theme::STATUS_ERROR), "\u{26a0} "),
    };
    let name_budget = CHIP_MAX_WIDTH.saturating_sub(2).saturating_sub(prefix.chars().count());
    let name = truncate_to(&chip.account_name, name_budget);
    let text = format!("({prefix}{name})");
    let width = text.chars().count();
    (vec![Span::raw(" "), Span::styled(text, style)], 1 + width)
}

/// Lookup helper: given a `ProjectView` list + a project name,
/// return the matching `ProjectKey`. Returns `None` when no project
/// is found.
fn find_project_key(projects: &[ProjectView], name: &str) -> Option<forge_workspace::ProjectKey> {
    projects.iter().find(|p| p.name == name).map(|p| p.key.clone())
}

fn push_error_row(lines: &mut Vec<Line<'static>>, error: &str, area_width: u16) {
    // 4 indent (2 pane + 2 connector) + 4 (glyph + name spacing) so
    // the error reads as attached to the project's name column.
    let style = Style::default().fg(theme::STATUS_ERROR);
    let pad: usize = 8;
    let budget = usize::from(area_width).saturating_sub(pad + 2);
    let truncated = truncate_to(error, budget);
    lines.push(Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(truncated, style)]));
}

/// Inline hint for a project whose AssignmentPlan pool is empty
/// (every allowed account ended in `Bailed`). Same indent + style
/// shape as `push_error_row`; uses DIM rather than STATUS_ERROR
/// because the condition is recoverable (user needs to `/login` an
/// account in the allow-list) rather than a hard error.
fn push_no_usable_accounts_row(lines: &mut Vec<Line<'static>>, area_width: u16) {
    let style = Style::default().fg(theme::DIM);
    let pad: usize = 8;
    let message = "no usable accounts";
    let budget = usize::from(area_width).saturating_sub(pad + 2);
    let truncated = truncate_to(message, budget);
    lines.push(Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(truncated, style)]));
}

fn glyph_for_row(lifecycle: SessionLifecycleState, spinner_glyph: char) -> (String, Color) {
    match lifecycle {
        // Idle = "alive, no turn in flight". `●` filled bullet in
        // `RUST_ORANGE` - same accent as the Projects pane uses for
        // its active-Idle glyph (see `glyph_for_lifecycle` over there).
        // Sharing the colour keeps the two surfaces visually coherent.
        SessionLifecycleState::Idle => ("●".to_owned(), theme::RUST_ORANGE),
        // Spawning + Running both animate the spinner. Spawning is
        // "subprocess starting up"; Running is "claude is mid-turn".
        // Both are transient busy states the picker should signal to
        // the user - picking a Running row should feel like jumping
        // into a session that's actively thinking, not one that's
        // already idle. Same `RUST_ORANGE` colour as the Projects
        // pane uses for its spinner glyph.
        SessionLifecycleState::Spawning | SessionLifecycleState::Running => {
            (spinner_glyph.to_string(), theme::RUST_ORANGE)
        }
        SessionLifecycleState::Failed => ("✗".to_owned(), theme::STATUS_ERROR),
        SessionLifecycleState::Sleeping
        | SessionLifecycleState::AuthRequired
        | SessionLifecycleState::LoggedOut
        | SessionLifecycleState::Attention => ("○".to_owned(), theme::DIM),
    }
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App, rows: &[PickerRow]) {
    let dim = Style::default().fg(theme::DIM);
    let selected_row = rows.get(app.launchpad.selected_index);
    // The Enter-action label tracks click intent so the hint always
    // matches what would happen if the user pressed Enter on the
    // currently-focused row. When the loading gate is down, every
    // row reads as Block - the label surfaces that as "loading
    // accounts" instead of the spawn-busy "spawning…" so the user
    // understands the wait is a one-off, not per-row.
    let loading = app.workspace.as_ref().is_some_and(|w| !w.all_accounts_loaded());
    let enter_label = if loading {
        "enter  ⏳ loading accounts…"
    } else {
        match selected_row.map(|r| effective_click_intent(app, &r.project_name, r.lifecycle)) {
            Some(ClickIntent::SpawnAndWait) => "enter  start",
            Some(ClickIntent::Block) => "enter  ⏳ spawning…",
            Some(ClickIntent::Retry) => "r  retry",
            Some(ClickIntent::EnterChat) | None => "enter  open",
        }
    };
    let hint = format!(" ↑↓  navigate     {enter_label}     ?  help     ctrl+q  quit");
    let line = Line::from(vec![Span::styled(hint, dim)]);
    frame.render_widget(Paragraph::new(line), area);
}

pub(super) fn truncate_to(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Number of selectable project rows. Used by the keyboard handler
/// to clamp `selected_index` to a valid range.
pub fn selectable_row_count(app: &App) -> usize {
    build_picker_rows(app).len()
}

/// Index of the project row whose lead session was most recently
/// active. `None` when no row carries a `last_activity_label` parseable
/// as a real timestamp (e.g. all rows show ` - `). Used to pick a sane
/// default selection on first render.
fn most_recently_active_index(rows: &[PickerRow]) -> Option<usize> {
    rows.iter().enumerate().find_map(|(idx, row)| match row.lifecycle {
        SessionLifecycleState::Idle | SessionLifecycleState::Running => Some(idx),
        _ => None,
    })
}

/// Resolve `selected_index` to the project name + lifecycle of the
/// row it points at. `None` when the picker is empty.
fn resolve_selection(app: &App) -> Option<(String, SessionLifecycleState)> {
    let rows = build_picker_rows(app);
    rows.get(app.launchpad.selected_index).map(|r| (r.project_name.clone(), r.lifecycle))
}

/// Handle Enter on the launchpad. Branches on the row's click
/// intent:
///
/// - `EnterChat` → switch to the resumed/running session.
/// - `SpawnAndWait` → dispatch `SpawnProject` and stay on the
///   launchpad. The row transitions Sleeping → Spawning → Idle in
///   the renderer; once it reaches a `EnterChat` state the user
///   presses Enter again to jump in. Avoids the chat-view
///   connecting stub for cold projects.
/// - `Block` → no-op. The row is mid-spawn; visual hint already
///   tells the user to wait.
/// - `Retry` → no-op. The `r` key is the explicit retry path so a
///   stray Enter on a failed row doesn't race a half-cleaned spawn.
pub fn pick_selected_project(app: &mut App) {
    let Some((project_name, lifecycle)) = resolve_selection(app) else {
        return;
    };
    match effective_click_intent(app, &project_name, lifecycle) {
        ClickIntent::EnterChat => switch_to_project_and_focus(app, &project_name),
        ClickIntent::SpawnAndWait => spawn_project_in_background(app, &project_name),
        ClickIntent::Block | ClickIntent::Retry => {}
    }
}

/// Dispatch `SpawnProject` for `project_name` without changing
/// views. Used by the launchpad's `SpawnAndWait` click path: cold
/// (Sleeping) rows become Spawning, the user waits on the
/// launchpad, and a second Enter once the row is ready takes them
/// into chat.
fn spawn_project_in_background(app: &mut App, project_name: &str) {
    let Some(workspace) = app.workspace.as_ref() else {
        return;
    };
    let launch_settings = crate::app::connect::session_launch_settings_for_startup(app);
    if let Err(err) = workspace.dispatch(forge_workspace::Command::SpawnProject {
        project_name: project_name.to_owned(),
        launch_settings,
    }) {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            project = project_name,
            error = %err,
            "launchpad spawn-and-wait: SpawnProject dispatch failed",
        );
    }
}

/// Handle `r` on a Failed row - drop the failed bucket and dispatch
/// a fresh spawn. Stays on the launchpad so the picker can visualise
/// the spawning row.
pub fn retry_selected_project(app: &mut App) {
    let Some((project_name, lifecycle)) = resolve_selection(app) else {
        return;
    };
    if lifecycle != SessionLifecycleState::Failed {
        return;
    }
    retry_project(app, &project_name);
}

/// Switch the active session to `project_name` and transition to
/// `ActiveView::Chat`. If no live bucket exists, dispatch a fresh
/// `SpawnProject` first. Mirror of the mouse-click flow in
/// `events/mouse.rs::switch_to_project_lead`; duplicated here rather
/// than refactored because the mouse path threads through hit-target
/// math that the keyboard path doesn't need.
fn switch_to_project_and_focus(app: &mut App, project_name: &str) {
    let project_info = app.workspace.as_ref().and_then(|w| {
        w.list_projects()
            .into_iter()
            .find(|p| p.name == project_name)
            .map(|p| (p.name.clone(), p.path.clone(), p.sessions))
    });
    let Some((resolved_name, project_path, catalog_sessions)) = project_info else {
        // The picker shouldn't be able to surface an unknown
        // project name, so log + bail rather than ignoring silently.
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            project = project_name,
            "launchpad pick: project not in workspace catalog",
        );
        return;
    };
    let spawn_synthetic = SessionKey::from_session_id(format!("__spawn_{resolved_name}__"));

    // Already-spawning bucket: switch to it; KeyRenamed migrates on
    // Connected.
    if app.sessions.contains_key(&spawn_synthetic) {
        app.switch_active_session(spawn_synthetic);
        set_active_view(app, ActiveView::Chat);
        return;
    }

    // Running bucket match by cwd - matches an auto_start project
    // whose session UUID has already arrived via KeyRenamed.
    let path_str = project_path.to_string_lossy();
    if let Some(key) = app.find_running_bucket_for_path(path_str.as_ref()) {
        app.switch_active_session(key);
        set_active_view(app, ActiveView::Chat);
        return;
    }

    // Catalog lead - switch if pooled, else dispatch SpawnProject.
    let lead_key = catalog_sessions.into_iter().next().map(|s| s.session);
    if let Some(key) = lead_key
        && app.sessions.contains_key(&key)
    {
        app.switch_active_session(key);
        set_active_view(app, ActiveView::Chat);
        return;
    }

    // Cold spawn - dispatch and transition. The synthetic bucket
    // will appear in `app.sessions` on the next event tick (via the
    // workspace's SessionTask emitting SessionUpdate::Connected /
    // KeyRenamed) and the chat view will pick it up automatically.
    // Until then the chat view renders against the pre-connect
    // bucket, matching the existing mouse-click → spawn flow in
    // `events/mouse.rs::switch_to_project_lead`.
    if let Some(workspace) = app.workspace.as_ref() {
        let launch_settings = crate::app::connect::session_launch_settings_for_startup(app);
        if let Err(err) = workspace.dispatch(forge_workspace::Command::SpawnProject {
            project_name: resolved_name,
            launch_settings,
        }) {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                project = project_name,
                error = %err,
                "launchpad pick: SpawnProject dispatch failed",
            );
            return;
        }
    }
    set_active_view(app, ActiveView::Chat);
}

/// Drop the failed bucket, clear its `last_connection_error`, and
/// dispatch a fresh `SpawnProject`. Stays on the launchpad so the
/// user sees the row flip from `✗` to the spinning glyph.
fn retry_project(app: &mut App, project_name: &str) {
    let spawn_synthetic = SessionKey::from_session_id(format!("__spawn_{project_name}__"));
    if let Some(workspace) = app.workspace.as_ref() {
        // Synthetic key cleanup. Cascade-detection inside
        // `release_session_with_cascade` no-ops for a synth_key (it
        // never reached Connected, so it isn't in any project's
        // catalog) - effectively a plain primitive release here.
        workspace.release_session_with_cascade(&spawn_synthetic);
    }
    app.sessions.remove(&spawn_synthetic);

    // Drop any non-synthetic failed bucket for the same project.
    let Some(workspace) = app.workspace.as_ref() else {
        return;
    };
    let projects = workspace.list_projects();
    if let Some(project) = projects.into_iter().find(|p| p.name == project_name) {
        for sess in project.sessions {
            if let Some(bucket) = app.sessions.get(&sess.session)
                && bucket.lifecycle_state == SessionLifecycleState::Failed
            {
                // Cascade-aware: a failed lead bucket may have workers
                // attached; closing the lead must drain them.
                workspace.release_session_with_cascade(&sess.session);
                app.sessions.remove(&sess.session);
            }
        }
    }

    // Dispatch a fresh spawn. Workspace will create a new synthetic
    // bucket on the next event tick.
    let launch_settings = crate::app::connect::session_launch_settings_for_startup(app);
    if let Some(workspace) = app.workspace.as_ref()
        && let Err(err) = workspace.dispatch(forge_workspace::Command::SpawnProject {
            project_name: project_name.to_owned(),
            launch_settings,
        })
    {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            project = project_name,
            error = %err,
            "launchpad retry: SpawnProject dispatch failed",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use std::time::Duration;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate_to("forge", 10), "forge");
    }

    #[test]
    fn truncate_appends_ellipsis() {
        assert_eq!(truncate_to("service-api-extended", 10), "service-a…");
    }

    #[test]
    fn selectable_row_count_zero_when_no_workspace() {
        let mut app = App::test_default();
        app.workspace = None;
        assert_eq!(selectable_row_count(&app), 0);
    }

    fn fixture_rows() -> Vec<PickerRow> {
        let mut rows = Vec::new();
        for (org, projects) in
            [("A", ["a1", "a2", "a3"]), ("B", ["b1", "b2", "b3"]), ("C", ["c1", "c2", "c3"])]
        {
            let n = projects.len();
            for (i, name) in projects.iter().enumerate() {
                rows.push(PickerRow {
                    project_name: (*name).to_owned(),
                    org: org.to_owned(),
                    last_activity_label: "now".to_owned(),
                    lifecycle: SessionLifecycleState::Sleeping,
                    error: None,
                    is_last_in_org: i + 1 == n,
                });
            }
        }
        rows
    }

    #[test]
    fn build_picker_content_window_follows_selection() {
        let mut app = App::test_default();
        app.workspace = None;
        let rows = fixture_rows();
        let view = 5usize;

        // Selecting the last project scrolls the window to the bottom.
        app.launchpad.selected_index = rows.len() - 1;
        let (lines, selected_flat) = build_picker_content(&app, &rows, 56);
        let total = lines.len();
        let selected_flat = selected_flat.expect("a project is selected");
        assert!(total > view, "fixture must overflow the viewport");
        let offset = reconcile_scroll(selected_flat, view, total, 0);
        assert!(offset > 0, "selecting the last project scrolls the window down");
        assert!(
            selected_flat >= usize::from(offset) && selected_flat < usize::from(offset) + view,
            "selected row stays inside the visible window",
        );

        // Moving back to the first project scrolls the window toward
        // the top, keeping the first row visible.
        app.launchpad.selected_index = 0;
        let (_, first_flat) = build_picker_content(&app, &rows, 56);
        let first_flat = first_flat.expect("a project is selected");
        let back = reconcile_scroll(first_flat, view, total, offset);
        assert!(back < offset, "moving back to the first project scrolls toward the top");
        assert!(
            first_flat >= usize::from(back) && first_flat < usize::from(back) + view,
            "first row stays inside the visible window",
        );
    }

    /// Render the picker over a one-project `forge.toml` with both
    /// `reviewer` and `scratch` persisted as workers, and only
    /// `reviewer` assigned in the plan: a label the plan knows (so it
    /// chips) alongside one it does not (so it does not).
    fn render_picker_rows() -> (Vec<String>, tempfile::TempDir, tempfile::TempDir) {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        let forge = config_dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/ dir");
        let project_path = project_dir.path().to_string_lossy().replace('\\', "/");
        std::fs::write(
            forge.join("forge.toml"),
            format!(
                "[[orgs]]\nname = \"Default\"\naccounts = [\"Stargate\"]\n\n\
                 [[orgs.projects]]\nname = \"picker\"\npath = \"{project_path}\"\n\
                 [[accounts]]\ndisplay_name = \"Stargate\"\nconfig_dir = \"~/.claude-stargate\"\nprovider = \"anthropic\"\n"
            ),
        )
        .expect("write forge.toml");

        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        let project = workspace.list_projects().into_iter().next().expect("one project");
        workspace.seed_test_dynamic_worker(&project.key, "reviewer");
        workspace.seed_test_dynamic_worker(&project.key, "scratch");
        workspace.seed_test_ready_account("Stargate");
        // `reviewer` spawned this boot, so it has a plan entry and a
        // chip; `scratch` has only a row and renders bare.
        workspace.seed_test_worker_assignment(&project.key, "reviewer");

        let mut app = App::test_default();
        app.workspace = Some(std::sync::Arc::new(workspace));
        let rows = build_picker_rows(&app);
        let (lines, _) = build_picker_content(&app, &rows, PICKER_WIDTH);
        let rendered = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect();
        (rendered, config_dir, project_dir)
    }

    /// The per-account glyph row appears only when it has something to
    /// say. All-Ready is the steady state, and a row that renders then
    /// is five permanent green dots; a row that does NOT render while an
    /// account is mid-flight leaves every project blocked with nothing
    /// on screen explaining why - and `all_accounts_loaded()` is global,
    /// so that happens even for an account no project uses.
    #[tokio::test]
    async fn the_account_row_appears_only_while_an_account_is_mid_flight() {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        let forge = config_dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/ dir");
        let project_path = project_dir.path().to_string_lossy().replace('\\', "/");
        std::fs::write(
            forge.join("forge.toml"),
            format!(
                "[[orgs]]\nname = \"Default\"\naccounts = [\"Stargate\"]\n\n\
                 [[orgs.projects]]\nname = \"picker\"\npath = \"{project_path}\"\n\
                 [[accounts]]\ndisplay_name = \"Stargate\"\nconfig_dir = \"~/.claude-stargate\"\nprovider = \"anthropic\"\n"
            ),
        )
        .expect("write forge.toml");

        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        let mut app = App::test_default();
        app.workspace = Some(std::sync::Arc::new(workspace));

        // A fresh account map starts every account Loading.
        assert!(
            account_row_visible(&app),
            "an account still resolving must be visible, or the blocked project rows have no \
             explanation on screen",
        );

        app.workspace.as_ref().expect("workspace").seed_test_ready_account("Stargate");
        assert!(
            !account_row_visible(&app),
            "with every account Ready the row has nothing to say and must not render",
        );
    }

    fn row_containing<'a>(rendered: &'a [String], needle: &str) -> &'a str {
        rendered
            .iter()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no rendered row contains {needle:?}; got {rendered:#?}"))
    }

    /// The geometry claim: a worker row's chip opens at the same column
    /// as the project row's. Equal row width alone would not pin this -
    /// widening the name column and narrowing the chip column keeps the
    /// total identical while moving the chips a column apart - so assert
    /// the index of the chip's own opening bracket.
    #[tokio::test]
    async fn worker_row_chip_opens_in_the_project_row_chip_column() {
        let (rendered, _config_dir, _project_dir) = render_picker_rows();
        let project_row = row_containing(&rendered, "picker");
        let worker_row = row_containing(&rendered, "reviewer");

        let chip_at = |row: &str| match row.char_indices().find(|(_, c)| *c == '(') {
            Some((i, _)) => row[..i].chars().count(),
            None => panic!("row carries an account chip; got {row:?}"),
        };
        assert_eq!(
            chip_at(worker_row),
            chip_at(project_row),
            "the worker row's chip must open in the project row's chip column; \
             worker {worker_row:?} vs project {project_row:?}",
        );
        assert_eq!(
            worker_row.chars().count(),
            project_row.chars().count(),
            "both rows end at the same column, so the activity field lines up too; \
             worker {worker_row:?} vs project {project_row:?}",
        );
    }

    /// Worker rows are sourced from the persisted workers, so a label
    /// that never spawned still gets a row - and with no
    /// assignment-plan entry it renders its lifecycle glyph and the
    /// activity placeholder with no chip at all.
    #[tokio::test]
    async fn a_never_spawned_dynamic_worker_renders_bare() {
        let (rendered, _config_dir, _project_dir) = render_picker_rows();
        let worker_row = row_containing(&rendered, "scratch");

        assert!(
            !worker_row.contains('('),
            "a label with no assignment-plan entry renders no chip; got {worker_row:?}",
        );
        assert!(
            worker_row.contains('\u{25cb}'),
            "a worker that has never spawned carries the Sleeping glyph; got {worker_row:?}",
        );
        assert!(
            worker_row.trim_end().ends_with('\u{2014}'),
            "no per-worker activity timestamp exists, so the activity column reads as the \
             em-dash placeholder; got {worker_row:?}",
        );
    }

    fn worker_entry(
        label: &str,
        session: &str,
        status: forge_primitives::WorkerLiveness,
    ) -> forge_workspace::LiveWorkerState {
        forge_workspace::LiveWorkerState {
            label: label.to_owned(),
            status,
            session_key: SessionKey::from_session_id(session.to_owned()),
        }
    }

    /// A live worker's glyph tracks its own session, and a Running one
    /// whose bucket has not arrived reads as `Spawning` - the same
    /// answer the Projects pane gives for that worker in that instant,
    /// so the two surfaces cannot disagree about it.
    #[test]
    fn a_running_worker_without_a_bucket_reads_as_spawning() {
        use crate::app::session::UiSession;
        use forge_primitives::WorkerLiveness;

        let mut app = App::test_default();
        let live = vec![
            worker_entry("settled", "worker-settled", WorkerLiveness::Running),
            worker_entry("unbucketed", "worker-unbucketed", WorkerLiveness::Running),
        ];
        let settled_key = SessionKey::from_session_id("worker-settled".to_owned());
        let mut bucket = UiSession::new(settled_key.clone());
        bucket.lifecycle_state = SessionLifecycleState::Running;
        app.sessions.insert(settled_key, bucket);

        assert_eq!(
            worker_lifecycle(&app, &live, "settled"),
            SessionLifecycleState::Running,
            "a worker with a bucket reports that bucket's lifecycle",
        );
        assert_eq!(
            worker_lifecycle(&app, &live, "unbucketed"),
            SessionLifecycleState::Spawning,
            "a Running worker whose bucket has not arrived reads as Spawning, the same answer \
             projects_pane.rs gives for it",
        );
        assert_eq!(
            worker_lifecycle(&app, &live, "never-spawned"),
            SessionLifecycleState::Sleeping,
            "a label with no live worker at all is sleeping",
        );
    }

    #[test]
    fn picker_thumb_paints_track_and_thumb_on_overflow() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let width: u16 = 8;
        let viewport: u16 = 12;
        let backend = TestBackend::new(width, viewport);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_picker_thumb(frame, 0, 0, width, viewport, 24, 0))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let w = usize::from(buffer.area.width);
        let rail_x = usize::from(width - 1);
        let mut thumb = 0usize;
        let mut track = 0usize;
        for y in 0..usize::from(viewport) {
            match buffer.content[y * w + rail_x].symbol() {
                "▐" => thumb += 1,
                "│" => track += 1,
                other => panic!("unexpected rail cell at row {y}: {other:?}"),
            }
        }
        // viewport² / total = 144 / 24 = 6 thumb cells; the rest is track.
        assert_eq!(thumb, 6, "thumb spans viewport² / total rows");
        assert_eq!(track, usize::from(viewport) - 6, "the remaining rail is track");
        // At offset 0 the thumb starts at the top of the rail.
        assert_eq!(buffer.content[rail_x].symbol(), "▐");
    }

    #[test]
    fn format_relative_time_short_intervals() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        assert_eq!(format_relative_time(now - Duration::from_secs(30), now), "now");
        assert_eq!(format_relative_time(now - Duration::from_secs(180), now), "3m");
        assert_eq!(format_relative_time(now - Duration::from_secs(7_200), now), "2h");
    }

    /// Per-project click-gate: Enter on a Spawning row is a no-op so
    /// the user never lands on the chat-view connecting stub. Cold
    /// rows (Sleeping / Failed) trigger a spawn-and-stay-on-launchpad
    /// path; ready states (Idle / Running / etc.) hand off to
    /// `switch_to_project_and_focus` which enters chat.
    #[test]
    fn click_intent_blocks_spawning_lifecycle() {
        use SessionLifecycleState as L;
        assert_eq!(click_intent(L::Spawning), ClickIntent::Block);
        assert_eq!(click_intent(L::Sleeping), ClickIntent::SpawnAndWait);
        assert_eq!(click_intent(L::Failed), ClickIntent::Retry);
        for ready in [L::Idle, L::Running, L::Attention, L::AuthRequired, L::LoggedOut] {
            assert_eq!(
                click_intent(ready),
                ClickIntent::EnterChat,
                "{ready:?} should be clickable into chat",
            );
        }
    }

    // Vertical placement: the whole block (identity + gap + picker box)
    // is centered in the space above the footer; a block taller than
    // that space top-aligns. (IDENTITY_PICKER_GAP is 0.)
    #[test]
    fn block_top_offset_centers_a_short_block() {
        // area 40, footer 1 -> available 39; block = 8 + 10 = 18;
        // offset = (39 - 18) / 2 = 10.
        assert_eq!(block_top_offset(40, 8, 10, 1), 10);
    }

    #[test]
    fn block_top_offset_top_aligns_a_tall_block() {
        // area 20, footer 1 -> available 19; block = 8 + 15 = 23 > 19
        // -> top-aligned at 0.
        assert_eq!(block_top_offset(20, 8, 15, 1), 0);
    }

    #[test]
    fn block_top_offset_never_overlaps_footer() {
        // A long list, capped by picker_box_height, still leaves the
        // placed block clear of the footer slot.
        let area_height = 40u16;
        let footer_height = 1u16;
        let identity_height = 8u16;
        let available = area_height - footer_height;
        let region = available - identity_height - IDENTITY_PICKER_GAP;
        let box_height = picker_box_height(1000, region);
        let offset = block_top_offset(area_height, identity_height, box_height, footer_height);
        let block_bottom = offset + identity_height + IDENTITY_PICKER_GAP + box_height;
        assert!(block_bottom <= available, "block must not reach into the footer row");
    }

    // Box sizing preserves the existing cap: grow to content + 2
    // framing rules, cap at region - PICKER_BOX_MARGIN, never exceed
    // the region.
    #[test]
    fn picker_box_height_fits_short_content() {
        assert_eq!(picker_box_height(5, 30), 7);
    }

    #[test]
    fn picker_box_height_caps_long_content() {
        // 30 - PICKER_BOX_MARGIN(4) = 26.
        assert_eq!(picker_box_height(100, 30), 26);
    }

    #[test]
    fn picker_box_height_floors_on_a_tiny_region() {
        // The 3-row minimum holds, but never beyond the region itself.
        assert_eq!(picker_box_height(100, 4), 3);
        assert_eq!(picker_box_height(100, 2), 2);
    }
}
