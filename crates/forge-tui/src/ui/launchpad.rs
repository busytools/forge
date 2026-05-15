//! Launchpad view — project picker shown as the floor of the UI when
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

use std::time::{Duration, SystemTime};

use forge_primitives::SessionLifecycleState;
use forge_workspace::{ProjectView, SessionKey, SpinnerStyle};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::app::App;
use crate::app::view::{ActiveView, set_active_view};

/// ANSI Shadow figlet for "forge". 6 rows × 43 cols. Locked by the
/// design spec — do not tweak.
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

/// White-on-RUST_ORANGE selection band per the spec — matches the
/// `session_picker.rs` treatment.
fn selection_style() -> Style {
    Style::default().fg(Color::White).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
}

/// Muted variant of [`selection_style`] for rows that aren't
/// actionable yet (Spawning state). Same shape so the band aligns
/// with the chrome, but the bg drops to `DIM` so the user sees at a
/// glance that pressing Enter on this row won't take them anywhere.
fn waiting_selection_style() -> Style {
    Style::default().fg(Color::White).bg(theme::DIM).add_modifier(Modifier::BOLD)
}

/// What pressing Enter on this row does. Drives the click handlers
/// and the footer hint, so the user always sees the next action that
/// matches the row's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickIntent {
    /// Row is ready to receive input — switch the chat view to it.
    EnterChat,
    /// Cold row — dispatch `SpawnProject` and stay on the launchpad
    /// so the user sees the row transition through `Spawning` rather
    /// than landing on the chat-view connecting stub.
    SpawnAndWait,
    /// Mid-spawn — block the click entirely. Footer explains why.
    Block,
    /// Failed — the `r` key is the explicit retry path. Enter is a
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

/// One selectable row in the picker — the data the renderer needs
/// to draw the row plus the metadata the keyboard handler needs to
/// resolve a pick into a project + lifecycle.
#[derive(Debug, Clone)]
struct PickerRow {
    project_name: String,
    org: String,
    account_hint: String,
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
                account_hint: project.primary_account_hint(),
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
/// 1. `__spawn_<name>__` synthetic — the pre-Connected placeholder.
/// 2. Catalog session UUIDs — the lead recorded on disk, if pooled.
/// 3. `cwd_raw` match — covers the post-KeyRenamed window when the
///    synthetic has migrated to the real session UUID but the
///    catalog scan hasn't refreshed yet. Without this third step the
///    launchpad rendered every auto_start project as `Sleeping`
///    even after it had connected — the bucket was alive in
///    `app.sessions`, just under a UUID neither the synthetic nor
///    catalog lookups knew about. The `cwd_raw` walk filters the
///    pre-connect sentinel via [`App::find_running_bucket_for_path`]
///    so a project whose path matches `current_dir()` (the typical
///    "forge launched from inside that project's dir" case) doesn't
///    resolve to the boot stub.
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
    let key = app.find_running_bucket_for_path(path_str.as_ref())?;
    app.sessions.get(&key)
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
            None => "—".to_owned(),
        },
    }
}

fn format_relative_time(activity: SystemTime, now: SystemTime) -> String {
    let elapsed = now.duration_since(activity).unwrap_or(Duration::ZERO);
    let secs = elapsed.as_secs();
    if secs < 60 {
        return "now".to_owned();
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86_400 {
        return format!("{}h", secs / 3600);
    }
    if secs < 604_800 {
        return format!("{}d", secs / 86_400);
    }
    format!("{}w", (secs / 604_800).min(99))
}

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
    let block_area =
        Rect { x: area.x, y, width: area.width, height: u16::try_from(lines.len()).unwrap_or(0) };
    frame.render_widget(Paragraph::new(lines), block_area);
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
    area_width: u16,
) {
    let connector = if row.is_last_in_org { "└─" } else { "├─" };
    let (glyph, glyph_color) =
        glyph_for_row(row.lifecycle, app.launchpad.spinner_style, app.launchpad.opened_at);
    let intent = click_intent(row.lifecycle);
    // Name styling tracks click intent so a glance at the picker
    // tells the user which rows are interactive right now. BOLD =
    // pressing Enter does something useful (enter chat or kick off a
    // cold spawn). DIM = waiting (Spawning) or auth/logged-out
    // states where Enter does take the user somewhere but the row
    // itself isn't yet a "live session" they're jumping into.
    let name_style = match intent {
        ClickIntent::EnterChat | ClickIntent::SpawnAndWait | ClickIntent::Retry => {
            Style::default().add_modifier(Modifier::BOLD)
        }
        ClickIntent::Block => Style::default().fg(theme::DIM),
    };

    let name_width: usize = 14;
    let hint_width: usize = 12;
    let right_width: usize = 10;

    let name_label = truncate_to(&row.project_name, name_width);
    let name_pad = name_width.saturating_sub(name_label.chars().count());
    let hint_label = format!("({})", row.account_hint);
    let hint_label = truncate_to(&hint_label, hint_width);
    let hint_pad = hint_width.saturating_sub(hint_label.chars().count());
    let right_label = truncate_to(&row.last_activity_label, right_width);

    // The row content starts at col 4 (2 indent + 2 connector). Pad
    // any leftover area width with spaces so the selection band
    // covers the whole row.
    let content_width_estimate = 4 + 1 + 1 + 1 + name_width + 1 + hint_width + 2 + right_width;
    let trailing_pad = usize::from(area_width).saturating_sub(content_width_estimate);

    if selected {
        // Non-clickable rows get a muted gray band so the user sees
        // "yes, this row is focused, but Enter won't open it."
        let band = if matches!(intent, ClickIntent::Block) {
            waiting_selection_style()
        } else {
            selection_style()
        };
        lines.push(Line::from(vec![
            Span::styled("  ".to_owned(), band),
            Span::styled(connector.to_owned(), band),
            Span::styled(" ".to_owned(), band),
            Span::styled(glyph, band.fg(glyph_color)),
            Span::styled(" ".to_owned(), band),
            Span::styled(name_label, band),
            Span::styled(" ".repeat(name_pad), band),
            Span::styled(" ".to_owned(), band),
            Span::styled(hint_label, band),
            Span::styled(" ".repeat(hint_pad), band),
            Span::styled("  ".to_owned(), band),
            Span::styled(format!("{right_label:>right_width$}"), band),
            Span::styled(" ".repeat(trailing_pad), band),
        ]));
    } else {
        let dim = Style::default().fg(theme::DIM);
        lines.push(Line::from(vec![
            Span::raw("  ".to_owned()),
            Span::styled(connector.to_owned(), dim),
            Span::raw(" ".to_owned()),
            Span::styled(glyph, Style::default().fg(glyph_color)),
            Span::raw(" ".to_owned()),
            Span::styled(name_label, name_style),
            Span::raw(" ".repeat(name_pad)),
            Span::raw(" ".to_owned()),
            Span::styled(hint_label, dim),
            Span::raw(" ".repeat(hint_pad)),
            Span::raw("  ".to_owned()),
            Span::styled(format!("{right_label:>right_width$}"), dim),
            Span::raw(" ".repeat(trailing_pad)),
        ]));
    }
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

fn glyph_for_row(
    lifecycle: SessionLifecycleState,
    style: SpinnerStyle,
    opened_at: std::time::Instant,
) -> (String, Color) {
    match lifecycle {
        // Idle = "alive, no turn in flight". `●` filled bullet in
        // `RUST_ORANGE` — same accent as the Projects pane uses for
        // its active-Idle glyph (see `glyph_for_lifecycle` over there).
        // Sharing the colour keeps the two surfaces visually coherent.
        SessionLifecycleState::Idle => ("●".to_owned(), theme::RUST_ORANGE),
        // Spawning + Running both animate the spinner. Spawning is
        // "subprocess starting up"; Running is "claude is mid-turn".
        // Both are transient busy states the picker should signal to
        // the user — picking a Running row should feel like jumping
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
/// style — the opacity tween is handled separately at the colour
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
    // currently-focused row.
    let enter_label = match selected_row.map(|r| click_intent(r.lifecycle)) {
        Some(ClickIntent::SpawnAndWait) => "enter  start",
        Some(ClickIntent::Block) => "enter  ⏳ spawning…",
        Some(ClickIntent::Retry) => "r  retry",
        Some(ClickIntent::EnterChat) | None => "enter  open",
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
/// as a real timestamp (e.g. all rows show `—`). Used to pick a sane
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
    let intent = click_intent(lifecycle);
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "launchpad_pick_dispatched",
        project = project_name.as_str(),
        lifecycle = ?lifecycle,
        intent = ?intent,
        "launchpad Enter dispatched",
    );
    match intent {
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

/// Handle `r` on a Failed row — drop the failed bucket and dispatch
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
    let bucket_keys: Vec<String> = app.sessions.keys().map(|k| k.as_str().to_owned()).collect();
    let buckets_with_matching_cwd: Vec<String> = app
        .sessions
        .iter()
        .filter(|(_, s)| s.cwd_raw.as_str() == project_path.to_string_lossy().as_ref())
        .map(|(k, _)| k.as_str().to_owned())
        .collect();
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "launchpad_switch_to_project_entry",
        project = resolved_name.as_str(),
        spawn_synthetic = spawn_synthetic.as_str(),
        path = %project_path.to_string_lossy(),
        active_key = ?app.active_session_key.as_ref().map(forge_workspace::SessionKey::as_str),
        bucket_count = bucket_keys.len(),
        buckets = ?bucket_keys,
        buckets_with_matching_cwd = ?buckets_with_matching_cwd,
        "launchpad switch_to_project_and_focus entered",
    );

    // Already-spawning bucket: switch to it; KeyRenamed migrates on
    // Connected.
    if app.sessions.contains_key(&spawn_synthetic) {
        let lifecycle = app.sessions.get(&spawn_synthetic).map(|s| s.lifecycle_state);
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "launchpad_switch_step1_synthetic_match",
            project = resolved_name.as_str(),
            synthetic = spawn_synthetic.as_str(),
            lifecycle = ?lifecycle,
            "step 1 matched __spawn_<name>__ synthetic; switching",
        );
        app.switch_active_session(spawn_synthetic);
        set_active_view(app, ActiveView::Chat);
        return;
    }

    // Running bucket match by cwd — matches an auto_start project
    // whose session UUID has already arrived via KeyRenamed.
    // `find_running_bucket_for_path` excludes the pre-connect
    // sentinel so the lookup is deterministic when forge was
    // launched from inside this project's directory.
    let path_str = project_path.to_string_lossy();
    if let Some(key) = app.find_running_bucket_for_path(path_str.as_ref()) {
        let lifecycle = app.sessions.get(&key).map(|s| s.lifecycle_state);
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "launchpad_switch_step2_cwd_match",
            project = resolved_name.as_str(),
            matched_key = key.as_str(),
            lifecycle = ?lifecycle,
            "step 2 matched cwd_raw filter; switching",
        );
        app.switch_active_session(key);
        set_active_view(app, ActiveView::Chat);
        return;
    }

    // Catalog lead — switch if pooled, else dispatch SpawnProject.
    let lead_key = catalog_sessions.into_iter().next().map(|s| s.session);
    if let Some(key) = lead_key
        && app.sessions.contains_key(&key)
    {
        let lifecycle = app.sessions.get(&key).map(|s| s.lifecycle_state);
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "launchpad_switch_step3_catalog_lead_match",
            project = resolved_name.as_str(),
            matched_key = key.as_str(),
            lifecycle = ?lifecycle,
            "step 3 matched catalog lead; switching",
        );
        app.switch_active_session(key);
        set_active_view(app, ActiveView::Chat);
        return;
    }
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "launchpad_switch_fallthrough_to_cold_spawn",
        project = resolved_name.as_str(),
        "steps 1-3 missed; falling through to step 4 (cold spawn + Chat view)",
    );

    // Cold spawn — dispatch and transition. The synthetic bucket
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
        workspace.release_session(&spawn_synthetic);
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
                workspace.release_session(&sess.session);
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
