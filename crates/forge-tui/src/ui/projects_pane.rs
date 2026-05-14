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
//!
//! Render-time-stamp pattern from PR #83. See specs at
//! `~/.claude-stargate/plans/2026-05-10-forge-tui-projects-pane-wide-design.md`
//! and `~/.claude-stargate/plans/2026-05-10-forge-tui-projects-pane-medium-design.md`.

use std::time::SystemTime;

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

    // Two anchored regions: top = project list, bottom = account /
    // status panel. The panel reserves a fixed N rows from the bottom
    // (see `ACCOUNT_PANEL_HEIGHT`); the list takes everything above.
    // When the pane is too short the panel is skipped and the list
    // gets the full area.
    let panel_reserved = render_account_status_footer(frame, area, app);
    let list_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(panel_reserved),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Pane name banner: PROJECTS at row 0, dim rule, then straight
    // into the first section header. The blank-between-section-header-
    // and-first-row is per section (in `append_project_rows`) so the
    // banner sits flush against the first section.
    lines.push(Line::from(Span::styled(
        "  PROJECTS".to_owned(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )));
    let rule_width = usize::from(list_area.width.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("─".repeat(rule_width), Style::default().fg(theme::DIM)),
    ]));

    append_project_rows(&mut lines, list_area, app, projects);

    frame.render_widget(Paragraph::new(lines), list_area);
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
type RowMeta<'p> =
    (&'p ProjectView, Option<(forge_workspace::SessionKey, SessionLifecycleState, bool)>);

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

    // Bucket projects by org name. Each bucket is a Vec of
    // (project, optional live session key, lifecycle, is_focused).
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
            (key, lifecycle, is_focused)
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
            Span::raw("  "),
            Span::styled(
                org_name.clone(),
                Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
            ),
        ]));
        // `│  ` continuation so the header visually links down to
        // the first project row's connector instead of floating
        // disconnected above an empty gap.
        lines.push(Line::from(vec![
            Span::raw("  "),
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
            // Deadzone gap row between adjacent projects in the
            // same org — emits the `│  ` tree continuation so the
            // connector lines visually link across the breathing
            // gap rather than breaking into floating fragments.
            // Skipped after the last project in the org — the
            // org-separator blanks take its place.
            if !is_last {
                lines.push(Line::from(vec![
                    Span::raw("  "),
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
#[allow(clippy::too_many_arguments)] // Render fn — args are layout state.
fn append_org_project_row(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    app: &mut App,
    project: &ProjectView,
    live: Option<&(forge_workspace::SessionKey, SessionLifecycleState, bool)>,
    is_last: bool,
    spinner_frame: usize,
    now: SystemTime,
) {
    let row_y = area.y + line_count_as_u16(lines);
    let connector = if is_last { "\u{2514}\u{2500} " } else { "\u{251C}\u{2500} " };
    let name_budget = name_budget_org_row(area.width);

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw("  "));
    spans.push(Span::styled(connector.to_owned(), Style::default().fg(theme::DIM)));

    if let Some((session_key, lifecycle, is_focused)) = live {
        let (glyph, glyph_color) = glyph_for_lifecycle(*lifecycle, *is_focused, spinner_frame);
        let name_style = if *is_focused {
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let label = truncate_with_ellipsis(project.name.as_str(), name_budget);
        let label_pad = name_budget.saturating_sub(label.chars().count());
        spans.push(Span::styled(glyph, Style::default().fg(glyph_color)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(label, name_style));
        spans.push(Span::raw(" ".repeat(label_pad)));
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
        // 2-col right gutter — matches the inspector pane's GIT
        // section right edge AND the idle row's 2-col gutter.
        spans.push(Span::raw("  "));
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
        // (row_right - 5) to (row_right - 3). 5-col hit band runs
        // (row_right - 6) to (row_right - 2) for 1-col tolerance
        // each side; the rightmost gutter cols stay inert.
        let close_x_start = row_right.saturating_sub(6);
        let close_x_end = row_right.saturating_sub(2);
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
        // position). Plus the standard 2-col right gutter.
        let label = truncate_with_ellipsis(project.name.as_str(), name_budget);
        let label_pad = name_budget.saturating_sub(label.chars().count());
        let time =
            format_relative_time(project.sessions.first().and_then(|s| s.last_activity), now);
        spans.push(Span::styled("\u{25CB}".to_owned(), Style::default().fg(theme::DIM)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(label, Style::default().fg(theme::DIM)));
        spans.push(Span::raw(" ".repeat(label_pad)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(time, Style::default().fg(theme::DIM)));
        spans.push(Span::raw("  "));
        lines.push(Line::from(spans));
        app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
            project_name: project.key.as_str().to_owned(),
            y: row_y,
            height: 1,
        });
    }
}

/// Chrome budget for an org-grouped row:
/// `<2 PANE_PAD><3 connector><1 glyph><1 sp><name><1 sp><RIGHT col><2 right pad>`
/// where RIGHT col = 3 cells (` ⏻ ` button for active rows / 3-char
/// `Xm`/`Xh`/`Xd` time for idle rows). Total = 7 left chrome + 1 sep
/// + 3 right col + 2 right pad = 13 chars per row.
fn name_budget_org_row(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(13))
}

/// Format `activity` as a short relative-time string anchored at
/// `now` (`now` / `Xm` / `Xh` / `Xd` / `Xw`), padded/truncated to a
/// stable 3-char column.
fn format_relative_time(activity: Option<SystemTime>, now: SystemTime) -> String {
    let Some(activity) = activity else {
        return "   ".to_owned();
    };
    let elapsed = now.duration_since(activity).unwrap_or_default();
    let secs = elapsed.as_secs();
    let raw = if secs < 60 {
        "now".to_owned()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 604_800 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}w", (secs / 604_800).min(99))
    };
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
/// See `~/.claude-stargate/plans/2026-05-10-forge-tui-projects-pane-wide-design.md`.
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

// ---------------------------------------------------------------
// Account / status panel — pane footer.
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
// The `📁 cwd` and `⎇ branch` rows that used to live here moved to
// the right-hand Inspector pane's `GIT` section in the 2026-05-13
// pane work — see `crate::ui::inspector_pane`.
// ---------------------------------------------------------------

/// Rows the account panel reserves from the bottom of the pane.
/// Constant by design — values flip but shape stays put (see the
/// "account chrome, not status row" intent in the design brief).
///
/// 17 rows: rule + 5 identity (Profile/Mode/Model/Effort/Fast) +
/// 1 blank + 1 Ctx + 1 blank + 2 (5h bar + ETA row) + 1 blank +
/// 2 (7d bar + ETA row) + 1 blank + 2 (forge + claude version
/// rows).
const ACCOUNT_PANEL_HEIGHT: u16 = 17;

/// Width (columns) the rule and content extend up to from the
/// pane's right edge. Matches the project-row right gutter so the
/// bottom-panel content visually aligns with the project list above.
const PANEL_RIGHT_GUTTER: usize = 2;

/// Per-row chrome inside the bar row: `2 indent + 3 label + 2 gap +
/// BAR + 2 gap + 4 pct`. The bar cell count is derived per render
/// so the bar stretches to fill (pane_width - PANEL_RIGHT_GUTTER -
/// chrome).
const BAR_ROW_FIXED_CHROME: usize = 2 + 3 + 2 + 2 + 4;

/// Below this pane height we skip the panel entirely (would crowd the
/// project list too aggressively). The chat footer is gone in this
/// flow, so the account info is only available via this panel — when
/// it's skipped, the user loses visibility on Mode / Model / Ctx /
/// usage. Acceptable for the ultra-compact-pane edge case; the docked
/// alternative would push the project list out of meaningful range.
const ACCOUNT_PANEL_MIN_PANE_HEIGHT: u16 = 24;

/// Width of the identity-block label column (`Profile`, `Mode`,
/// `Model`, `Fast`). Right-padded so the value column aligns
/// regardless of label length.
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
fn render_account_status_footer(frame: &mut Frame, area: Rect, app: &App) -> u16 {
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
    frame.render_widget(Paragraph::new(lines), panel_area);
    height
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
    // Two-space gutter before the value.
    let value_budget = usize::from(width).saturating_sub(2 + ACCOUNT_PANEL_ID_LABEL_WIDTH + 2);

    // Profile.
    let profile_value = app.active_account_display_name().unwrap_or_else(|| "—".to_owned());
    let profile_fitted = truncate_with_ellipsis(&profile_value, value_budget);
    lines.push(Line::from(vec![
        Span::raw("  "),
        label_span("Profile", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(profile_fitted),
    ]));

    // Mode.
    let (mode_label, mode_color) = mode_label_and_color(app);
    let mode_label_fitted = truncate_with_ellipsis(&mode_label, value_budget);
    lines.push(Line::from(vec![
        Span::raw("  "),
        label_span("Mode", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::styled(mode_label_fitted, Style::default().fg(mode_color)),
    ]));

    // Model. Display name only — effort lives on its own row below so
    // a long model name can't push it off-screen.
    let model_value = build_model_label(app).unwrap_or_else(|| "—".to_owned());
    let model_fitted = truncate_with_ellipsis(&model_value, value_budget);
    lines.push(Line::from(vec![
        Span::raw("  "),
        label_span("Model", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(model_fitted),
    ]));

    // Effort. Always shown — the underlying `EffortLevel` always has
    // a value (config carries a default). Keeping the row unconditional
    // means it doesn't appear / disappear as the user switches models.
    let effort = app.observed_effort().unwrap_or_else(|| app.config.thinking_effort_effective());
    lines.push(Line::from(vec![
        Span::raw("  "),
        label_span("Effort", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(effort_short_label(effort).to_owned()),
    ]));

    // Fast mode.
    let (fast_label, fast_color) = fast_mode_label_and_color(app);
    lines.push(Line::from(vec![
        Span::raw("  "),
        label_span("Fast", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::styled(fast_label.to_owned(), Style::default().fg(fast_color)),
    ]));

    // Row 6: blank separating identity from usage.
    lines.push(Line::default());

    // Row 7: Ctx bar. No ETA row — context has no reset window.
    let bar_cells = bar_cells_for(width);
    let ctx_pct = app.session_usage().context_usage_percent.map_or(0.0, f64::from);
    let ctx_pct_str = format!("{:>3}%", app.session_usage().context_usage_percent.unwrap_or(0));
    let mut ctx_line = vec![Span::raw("  "), label_span("Ctx", 3), Span::raw("  ")];
    ctx_line.extend(bar_spans(ctx_pct, bar_cells));
    ctx_line.push(Span::raw("  "));
    ctx_line.push(Span::raw(ctx_pct_str));
    lines.push(Line::from(ctx_line));

    // Row 8: blank between Ctx and 5h.
    lines.push(Line::default());

    // Rows 9..=10: 5h bar + ETA row.
    push_usage_window_lines(
        &mut lines,
        "5h",
        app.usage().snapshot.as_ref().and_then(|s| s.five_hour.as_ref()),
        width,
    );

    // Row 11: blank between 5h and 7d.
    lines.push(Line::default());

    // Rows 12..=13: 7d bar + ETA row.
    push_usage_window_lines(
        &mut lines,
        "7d",
        app.usage().snapshot.as_ref().and_then(|s| s.seven_day.as_ref()),
        width,
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
        Span::raw("  "),
        label_span("forge", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(forge_version),
    ]));

    let cli_info = app.cli_version_info.as_ref();
    let installed = cli_info
        .and_then(|i| i.installed.as_deref())
        .map_or_else(|| "—".to_owned(), |v| format!("v{v}"));
    let mut claude_spans = vec![
        Span::raw("  "),
        label_span("claude", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(installed),
    ];
    if let Some(info) = cli_info
        && info.has_update()
        && let Some(latest) = info.latest.as_deref()
    {
        claude_spans.push(Span::raw("  "));
        claude_spans.push(Span::styled(
            format!("\u{2191} v{latest}"),
            Style::default().fg(theme::STATUS_WARNING),
        ));
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
) {
    let bar_cells = bar_cells_for(width);
    let pct_value = window.map_or(0.0, |w| w.utilization);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct_text = window
        .map_or_else(|| "  —%".to_owned(), |w| format!("{:>3}%", w.utilization.round() as i64));
    let mut row = vec![Span::raw("  "), label_span(label, 3), Span::raw("  ")];
    row.extend(bar_spans(pct_value, bar_cells));
    row.push(Span::raw("  "));
    row.push(Span::raw(pct_text));
    lines.push(Line::from(row));

    // ETA — duration only (no "resets in " prose), DIM, right-justified
    // to the content right edge (pane width minus the 2-col right
    // gutter). Visually sits below the percent column of the bar row.
    let eta_text =
        window.and_then(format_window_reset_duration_only).unwrap_or_else(|| "—".to_owned());
    let eta_chars = eta_text.chars().count();
    let right_edge = usize::from(width).saturating_sub(PANEL_RIGHT_GUTTER);
    let pad = right_edge.saturating_sub(eta_chars);
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(pad)),
        Span::styled(eta_text, Style::default().fg(theme::DIM)),
    ]));
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
    Some(condense_model_name(&current.display_name_short))
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

/// Fast-mode label + color. Same set the legacy footer used. The
/// panel uses lowercase `off / cd / on` instead of `FAST:OFF` /
/// `FAST:CD` / `FAST:ON` because the row already has a `Fast` label.
fn fast_mode_label_and_color(app: &App) -> (&'static str, Color) {
    match app.fast_mode_state() {
        crate::agent::model::FastModeState::Off => ("off", theme::DIM),
        crate::agent::model::FastModeState::Cooldown => ("cd", Color::Yellow),
        crate::agent::model::FastModeState::On => ("on", theme::RUST_ORANGE),
    }
}

/// Head-truncate `s` to at most `max_chars` characters with a
/// trailing `…` ellipsis. Returns the original string if it
/// already fits. When `max_chars` is `0` or `1` the result is just
/// `…` — there's no room for content + ellipsis at those budgets.
///
/// Counts Unicode chars, not bytes, so multibyte labels truncate at
/// a sane visual position. Note that non-ASCII chars with display
/// width > 1 (CJK, some emoji) may still overflow visually; the
/// pane's content is project + session names which are overwhelmingly
/// ASCII or near-ASCII in practice.
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
        // Wide pane (32): 32 - 2 (right gutter) - 13 (chrome) = 17.
        assert_eq!(bar_cells_for(32), 17);
        // Medium pane (24): 24 - 2 - 13 = 9.
        assert_eq!(bar_cells_for(24), 9);
        // Narrower than the chrome+floor: clamps to 6.
        assert_eq!(bar_cells_for(10), 6);
    }

    #[test]
    fn account_panel_height_matches_row_count() {
        // The const + the debug_assert in build_account_panel_lines
        // co-anchor the layout. This test pins the constant explicitly
        // so a change to row count surfaces here too, not only at
        // runtime.
        assert_eq!(ACCOUNT_PANEL_HEIGHT, 17);
    }
}
