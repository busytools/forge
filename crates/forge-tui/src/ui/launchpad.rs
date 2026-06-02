//! Launchpad view - project picker shown as the floor of the UI when
//! forge is invoked without a project argv.
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
use forge_workspace::{ProjectView, SessionChipInfo, SessionChipState, SessionKey, SpinnerStyle};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::app::App;
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
/// project row we expect (`├─ ⠋  data-modules    (gateway1)     spawning`)
/// with a generous margin. Constant so the layout is stable across
/// terminal widths.
const PICKER_WIDTH: u16 = 56;

/// Width (in cells) of the left-edge selection indicator column.
/// Reserved on every row so unselected rows align with the selected
/// row's arrow + space. The arrow itself is `▶` rendered in
/// `RUST_ORANGE` for clickable selections and `DIM` for Block
/// (Spawning) selections; unselected rows render two spaces.
const SELECTION_PREFIX_WIDTH: usize = 2;

/// What pressing Enter on this row does. Drives the click handlers
/// and the footer hint, so the user always sees the next action that
/// matches the row's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone)]
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
            let last_activity_label = format_activity(project, lifecycle, now);
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

fn format_activity(
    project: &ProjectView,
    lifecycle: SessionLifecycleState,
    now: SystemTime,
) -> String {
    match lifecycle {
        SessionLifecycleState::Spawning => "spawning".to_owned(),
        SessionLifecycleState::Failed => "failed".to_owned(),
        _ => match project.sessions.first().and_then(|s| s.last_activity) {
            Some(activity) => format_relative_time(activity, now),
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

    // Vertical layout: identity block at top (with breathing room),
    // picker frame in the middle, footer hint pinned to the last
    // row.
    let identity_height = identity_block_height(app);
    let picker_height = picker_frame_height(rows.len(), &rows);
    let footer_height: u16 = 1;
    let total_content = identity_height + picker_height + footer_height + 3; // 3 = breathing rows
    let top_padding =
        if area.height > total_content { ((area.height - total_content) / 4).max(1) } else { 0 };

    let mut current_y = area.y + top_padding;

    render_identity_block(frame, area, app, current_y);
    current_y += identity_height + 2;

    let picker_x = area.x + area.width.saturating_sub(picker_outer_width) / 2;
    let picker_area = Rect {
        x: picker_x,
        y: current_y,
        width: picker_outer_width,
        height: picker_height
            .min(area.height.saturating_sub(current_y - area.y).saturating_sub(footer_height)),
    };
    render_picker(frame, picker_area, app, &rows);

    let footer_y = area.y + area.height.saturating_sub(footer_height);
    render_footer(
        frame,
        Rect { x: area.x, y: footer_y, width: area.width, height: footer_height },
        app,
        &rows,
    );
}

/// Static line count for the identity block: 6 wordmark rows +
/// version line + claude line + optional update indicator.
fn identity_block_height(app: &App) -> u16 {
    let mut h: u16 = 6 + 1 + 1;
    if app
        .cli_version_info
        .as_ref()
        .is_some_and(forge_workspace::env::cli_version::CliVersionInfo::has_update)
    {
        h += 1;
    }
    h
}

fn render_identity_block(frame: &mut Frame, area: Rect, app: &App, y: u16) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let wordmark_style = Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD);
    for row in FORGE_WORDMARK {
        lines.push(centered_text_line(row, area.width, wordmark_style));
    }
    let dim = Style::default().fg(theme::DIM);
    let forge_line = format!("v{}", crate::FORGE_VERSION_SHORT);
    lines.push(centered_text_line(&forge_line, area.width, dim));
    let claude_label = match app.cli_version_info.as_ref().and_then(|c| c.installed.as_deref()) {
        Some(installed) => format!("claude {installed}"),
        None => "claude (unknown)".to_owned(),
    };
    lines.push(centered_text_line(&claude_label, area.width, dim));
    if let Some(cli) = app.cli_version_info.as_ref()
        && cli.has_update()
        && let Some(latest) = cli.latest.as_deref()
    {
        let update = format!("↑ v{latest} available");
        lines.push(centered_text_line(
            &update,
            area.width,
            Style::default().fg(theme::RUST_ORANGE),
        ));
    }
    // Per-account loading glyph row: one centred line with each
    // account's name + state glyph. Yellow `○` for Loading or
    // Refreshing (mid-flight), green `●` for Ready, red `⚠` for
    // Bailed. Order matches forge.toml's `[[accounts]]` so the user
    // can scan left-to-right against their mental layout.
    if let Some(workspace) = app.workspace.as_ref() {
        let snapshot = workspace.account_loading_snapshot();
        if !snapshot.is_empty() {
            lines.push(Line::default());
            lines.push(centered_account_status_line(&snapshot, area.width));
        }
    }
    let block_area =
        Rect { x: area.x, y, width: area.width, height: u16::try_from(lines.len()).unwrap_or(0) };
    frame.render_widget(Paragraph::new(lines), block_area);
}

/// Render a centred line of per-account state chips. Each chip is
/// `<glyph> <name>` separated by `  ` (two spaces) so the user can
/// distinguish chips at a glance without staring at the row.
fn centered_account_status_line(
    snapshot: &[(String, forge_workspace::LoadingState)],
    area_width: u16,
) -> Line<'static> {
    use forge_workspace::LoadingState;
    let dim = Style::default().fg(theme::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text_width: usize = 0;
    for (idx, (name, state)) in snapshot.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled("  ".to_owned(), dim));
            text_width += 2;
        }
        let (glyph, color) = match state {
            LoadingState::Loading | LoadingState::Refreshing => ("○", Color::Yellow),
            LoadingState::Ready => ("●", Color::Green),
            LoadingState::Bailed => ("⚠", theme::STATUS_WARNING),
        };
        spans.push(Span::styled(glyph.to_owned(), Style::default().fg(color)));
        spans.push(Span::styled(format!(" {name}"), dim));
        text_width += 1 + 1 + name.chars().count();
    }
    let pad = usize::from(area_width).saturating_sub(text_width) / 2;
    let mut out: Vec<Span<'static>> = Vec::with_capacity(spans.len() + 1);
    out.push(Span::raw(" ".repeat(pad)));
    out.extend(spans);
    Line::from(out)
}

fn centered_text_line(text: &str, area_width: u16, style: Style) -> Line<'static> {
    let text_width = text.chars().count();
    let pad = usize::from(area_width).saturating_sub(text_width) / 2;
    Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(text.to_owned(), style)])
}

/// Estimated height: top rule + cold-boot row (if applicable) + per-
/// org rows + per-org separators + bottom rule. Used to size the
/// picker_area Rect before render.
fn picker_frame_height(_total_rows: usize, rows: &[PickerRow]) -> u16 {
    let mut h: u16 = 2; // top + bottom rules
    let mut last_org: Option<&str> = None;
    for row in rows {
        if last_org != Some(row.org.as_str()) {
            if last_org.is_some() {
                h += 1; // blank between orgs
            }
            h += 1; // org header
            last_org = Some(row.org.as_str());
        }
        h += 1; // project row
        if row.error.is_some() {
            h += 1; // error description row beneath a failed project
        }
    }
    h
}

fn render_picker(frame: &mut Frame, area: Rect, app: &mut App, rows: &[PickerRow]) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let dim = Style::default().fg(theme::DIM);
    let rule_width = usize::from(area.width);
    let rule_line = || Line::from(Span::styled("─".repeat(rule_width), dim));

    lines.push(rule_line());

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
        push_project_row(&mut lines, row, selected, app, area.width);
        if let Some(err) = &row.error {
            push_error_row(&mut lines, err, area.width);
        }
        // Worker rows: show forge.toml team labels (if any) nested
        // under the project, each with its assigned-account chip
        // from the AssignmentPlan. Info-only, not selectable.
        let project_view = app
            .workspace
            .as_ref()
            .map(|ws| ws.list_projects())
            .and_then(|list| list.into_iter().find(|p| p.name == row.project_name));
        if let Some(project) = project_view.as_ref() {
            push_worker_rows(&mut lines, project, app);
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
            push_no_usable_accounts_row(&mut lines, area.width);
        }
    }

    lines.push(rule_line());

    let height = u16::try_from(lines.len()).unwrap_or(0).min(area.height);
    let render_area = Rect { x: area.x, y: area.y, width: area.width, height };
    frame.render_widget(Paragraph::new(lines), render_area);
}

fn push_project_row(
    lines: &mut Vec<Line<'static>>,
    row: &PickerRow,
    selected: bool,
    app: &App,
    _area_width: u16,
) {
    let connector = if row.is_last_in_org { "└─" } else { "├─" };
    let (glyph, glyph_color) =
        glyph_for_row(row.lifecycle, app.launchpad.spinner_style, app.launchpad.opened_at);
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
    let right_width: usize = 10;
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
/// (the worker subtree connector), so the name column is correspondingly
/// narrower to keep the trailing chip column aligned with project rows.
const WORKER_NAME_WIDTH: usize = 11;

/// Chip column width - the leading-space + `(<account>)` chip pads to
/// this width so the time column (project rows) lands at a fixed x.
/// 13 = 1 space + 12 (CHIP_MAX_WIDTH in `account_chip_spans`).
const CHIP_COLUMN_WIDTH: usize = 13;

/// Append one row per declared team worker (from forge.toml's
/// `team = [...]` for this project) directly below the project's
/// row. Each row shows the worker label + its assigned-account chip
/// from the AssignmentPlan, so the user can see the per-session
/// account mapping before clicking the project. Workers are
/// info-only on the launchpad - clicks land on the project lead
/// row; the worker rows are not selectable.
fn push_worker_rows(lines: &mut Vec<Line<'static>>, project: &ProjectView, app: &App) {
    if project.team.is_empty() {
        return;
    }
    let dim = Style::default().fg(theme::DIM);
    let workers = &project.team;
    let count = workers.len();
    for (idx, label) in workers.iter().enumerate() {
        let is_last = idx + 1 == count;
        let tree_glyph = if is_last { "└─" } else { "├─" };
        let chip_info =
            app.workspace.as_ref().and_then(|ws| ws.session_chip_for(&project.key, label));
        let (chip_spans, _chip_width) = account_chip_spans(chip_info.as_ref());
        let name_label = truncate_to(label, WORKER_NAME_WIDTH);
        let name_pad = WORKER_NAME_WIDTH.saturating_sub(name_label.chars().count());

        // Worker-row chrome: 2 (selection prefix) + 2 (`│ `) + 3
        // (vertical continuation gap) + 2 (`├─`/`└─` tree connector)
        // + 1 (sp) = 10 cells. Plus name column (WORKER_NAME_WIDTH),
        // chip rendered after. Project-row chrome is only 7 cells +
        // PROJECT_NAME_WIDTH (14) = 21 cells. So worker chrome+name
        // = 10 + 11 = 21 - same trailing column for the chip.
        let mut spans: Vec<Span<'static>> = vec![
            Span::raw(" ".repeat(SELECTION_PREFIX_WIDTH)),
            Span::styled("│ ".to_owned(), dim),
            Span::raw(" ".repeat(3)),
            Span::styled(tree_glyph.to_owned(), dim),
            Span::raw(" ".to_owned()),
            Span::styled(name_label, Style::default().fg(theme::DIM)),
            Span::raw(" ".repeat(name_pad)),
        ];
        spans.extend(chip_spans);
        lines.push(Line::from(spans));
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

fn glyph_for_row(
    lifecycle: SessionLifecycleState,
    style: SpinnerStyle,
    opened_at: std::time::Instant,
) -> (String, Color) {
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
            let glyph = spinner_glyph(style, opened_at).to_string();
            (glyph, theme::RUST_ORANGE)
        }
        SessionLifecycleState::Failed => ("✗".to_owned(), theme::STATUS_ERROR),
        SessionLifecycleState::Sleeping
        | SessionLifecycleState::AuthRequired
        | SessionLifecycleState::LoggedOut
        | SessionLifecycleState::Attention => ("○".to_owned(), theme::DIM),
    }
}

/// Pick the current frame glyph for `style` based on
/// `elapsed_since_open / cadence_ms`. `forge_dot` is a single-glyph
/// style - the opacity tween is handled separately at the colour
/// layer (currently rendered as a flat `●`; a follow-up can wire
/// the alpha walk in once a colour-blend helper exists in theme.rs).
fn spinner_glyph(style: SpinnerStyle, opened_at: std::time::Instant) -> char {
    let frames = style.frames();
    let cadence = u128::from(style.cadence_ms()).max(1);
    let elapsed_ms = opened_at.elapsed().as_millis();
    let frame_idx = ((elapsed_ms / cadence) as usize) % frames.len();
    frames[frame_idx]
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

fn truncate_to(text: &str, width: usize) -> String {
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
        assert_eq!(truncate_to("data-modules-extended", 10), "hub-modul…");
    }

    #[test]
    fn selectable_row_count_zero_when_no_workspace() {
        let mut app = App::test_default();
        app.workspace = None;
        assert_eq!(selectable_row_count(&app), 0);
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
}
