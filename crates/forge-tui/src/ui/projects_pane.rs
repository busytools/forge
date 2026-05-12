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

/// Two-section project list: ACTIVE (projects with an in-process
/// session — running/idle/attention) and INACTIVE (sleeping, click
/// to wake). The active section carries the lifecycle glyph; the
/// inactive section is name-only in `DIM`. The previously-shown
/// per-session drilldown was dropped — the pane is a project
/// navigator, not a session log. Per-session detail (and any "switch
/// between sessions within a project" UX) moves to the in-session
/// `/resume` picker.
fn append_project_rows(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    app: &mut App,
    projects: &[ProjectView],
) {
    let active_session_key = app.active_session_key.clone();

    // Partition into active / inactive. A project is active iff at
    // least one of its catalog session ids is in `app.sessions`, OR
    // a synthetic `__spawn_<name>__` bucket is in flight for it.
    let spinner_frame = app.spinner_frame;
    let mut active: Vec<(&ProjectView, SessionLifecycleState, bool, forge_workspace::SessionKey)> =
        Vec::new();
    let mut inactive: Vec<&ProjectView> = Vec::new();
    // Helper: read `lifecycle_state` for `key` from the bucket.
    // Falls back to `Idle` when no bucket is registered (e.g., the
    // catalog references a session the user has never woken).
    let lifecycle_for = |key: &forge_workspace::SessionKey| -> SessionLifecycleState {
        app.sessions.get(key).map_or(SessionLifecycleState::default(), |s| s.lifecycle_state)
    };

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
        if let Some((key, lifecycle)) = live_session.or(synthetic) {
            let is_focused = Some(&key) == active_session_key.as_ref();
            active.push((project, lifecycle, is_focused, key));
        } else {
            inactive.push(project);
        }
    }

    // Active first, sorted by most-recent activity.
    active.sort_by(|a, b| {
        let a_act = a.0.sessions.first().and_then(|s| s.last_activity);
        let b_act = b.0.sessions.first().and_then(|s| s.last_activity);
        b_act.cmp(&a_act).then_with(|| a.0.key.as_str().cmp(b.0.key.as_str()))
    });
    inactive.sort_by(|a, b| a.name.cmp(&b.name));

    let now = SystemTime::now();
    // Row chrome budget — see name_budget_active / name_budget_inactive helpers.

    if !active.is_empty() {
        lines.push(Line::from(Span::styled(
            "  ACTIVE".to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        )));
        // Blank between section header and the first row — gives the
        // header breathing room from the rows it labels.
        lines.push(Line::default());
        let name_budget = name_budget_active(area.width);
        let active_count = active.len();
        for (idx, (project, lifecycle, is_focused, session_key)) in active.iter().enumerate() {
            let row_y = area.y + line_count_as_u16(lines);
            let (glyph, glyph_color) = glyph_for_lifecycle(*lifecycle, *is_focused, spinner_frame);
            let label = truncate_with_ellipsis(project.name.as_str(), name_budget);
            let label_pad = name_budget.saturating_sub(label.chars().count());
            let name_style = if *is_focused {
                Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            // No time column on active rows — the lifecycle glyph
            // already says "alive", and the .jsonl mtime for a
            // running session tracks every wire event so it's
            // always `now`/`1m`/`2m` and adds no new signal.
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(glyph, Style::default().fg(glyph_color)),
                Span::raw(" "),
                Span::styled(label, name_style),
                Span::raw(" ".repeat(label_pad)),
                Span::raw(" "),
                // 3-char "[×]" close affordance — reads as a button
                // rather than a stray multiplication sign, and the
                // wider visual footprint matches the wider click band
                // stamped below. Bold so it pops against the DIM
                // metadata column to its left. 2-col gutter mirrors
                // the inactive row's time-column gutter so `[×]` and
                // the date sit in the same x column visually.
                Span::styled(
                    "[×]".to_owned(),
                    Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
            ]));
            app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
                project_name: project.key.as_str().to_owned(),
                y: row_y,
                height: 1,
            });
            // Close-glyph hit target — covers `[×]` (3 chars) plus
            // a one-col tolerance on either side, for a 5-col band.
            // The rightmost gutter col stays inert so accidental
            // edge-clicks resolve as row-click (focus/switch).
            // Layout positions: `... ${time} ${space} [ × ] ${gutter}`
            // → `[×]` occupies area.right-4..area.right-1; the 5-col
            // band runs area.right-5..area.right-1 (one space before
            // `[`, then the 3 chars of the button, then one col after).
            let row_right = area.x.saturating_add(area.width);
            let close_x_start = row_right.saturating_sub(5);
            let close_x_end = row_right.saturating_sub(1);
            app.pane_hit_targets.push(PaneHitTarget::CloseSession {
                session_key: session_key.clone(),
                y: row_y,
                height: 1,
                x_start: close_x_start,
                x_end: close_x_end,
            });
            // Deadzone gap row between adjacent projects so a tap
            // that lands between rows fires nothing rather than the
            // wrong row. No hit target is stamped for the blank, so
            // the gap is a true no-op band.
            if idx + 1 < active_count {
                lines.push(Line::default());
            }
        }
    }

    if !inactive.is_empty() {
        // Two blanks between the last active row and the INACTIVE
        // header so the section transition reads as a clear break
        // rather than a tight stack. Per-section blank-after-header
        // below keeps the header off the first row inside the
        // section.
        if !active.is_empty() {
            lines.push(Line::default());
            lines.push(Line::default());
        }
        lines.push(Line::from(Span::styled(
            "  INACTIVE".to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        )));
        // Same blank-after-header rhythm as ACTIVE.
        lines.push(Line::default());
        let name_budget = name_budget_inactive(area.width);
        let inactive_count = inactive.len();
        for (idx, project) in inactive.iter().enumerate() {
            let row_y = area.y + line_count_as_u16(lines);
            let label = truncate_with_ellipsis(project.name.as_str(), name_budget);
            let label_pad = name_budget.saturating_sub(label.chars().count());
            let time =
                format_relative_time(project.sessions.first().and_then(|s| s.last_activity), now);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("○".to_owned(), Style::default().fg(theme::DIM)),
                Span::raw(" "),
                Span::styled(label, Style::default().fg(theme::DIM)),
                Span::raw(" ".repeat(label_pad)),
                Span::raw(" "),
                Span::styled(time, Style::default().fg(theme::DIM)),
                Span::raw("  "),
            ]));
            app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
                project_name: project.key.as_str().to_owned(),
                y: row_y,
                height: 1,
            });
            // Deadzone gap between inactive rows — same rationale
            // as the active section. Tap on the blank row → nothing
            // fires; tap on the content row → focus/wake fires.
            if idx + 1 < inactive_count {
                lines.push(Line::default());
            }
        }
    }
}

/// Active-row layout:
/// `<2 indent><1 glyph><1 sp><name><1 sp><3 [×]><2 right gutter>`
/// = 10 chrome chars. 2-col gutter mirrors the inactive row so the
/// trailing `[×]` lands in the same x column as the inactive time
/// column.
fn name_budget_active(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(10))
}

/// Inactive-row layout:
/// `<2 indent><1 glyph><1 sp><name><1 sp><3 time><2 right gutter>`
/// = 10 chrome chars. Same budget as active so the trailing column
/// (`[×]` vs time) aligns at the same x.
fn name_budget_inactive(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(10))
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
//
//     Profile  Stargate
//     Mode     [Auto]
//     Model    sonnet-4.5/Max
//     Fast     off
//
//     Ctx   ████████░░░░  65%
//     5h    ███░░░░░░░░░  23%
//           resets in 2h
//     7d    ████░░░░░░░░  34%
//           resets in 4d
//
//     📁 ~/Projects/forge
//     ⎇  fix/instrumented-drops
//
// `ACCOUNT_PANEL_HEIGHT` is constant. The panel's render swallows the
// fixed N rows from the bottom of the pane; the project list takes
// everything above.
// ---------------------------------------------------------------

/// Rows the account panel reserves from the bottom of the pane.
/// Constant by design — values flip but shape stays put (see the
/// "account chrome, not status row" intent in the design brief).
const ACCOUNT_PANEL_HEIGHT: u16 = 15;

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

/// Cells in each usage filling bar (`Ctx` / `5h` / `7d`).
const ACCOUNT_PANEL_BAR_CELLS: usize = 12;

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

/// 12-cell filling bar for a 0..=100 utilisation. Returns a pair of
/// spans: the filled portion in the threshold colour, the empty
/// portion in DIM. Matches the `gauge_style` thresholds from
/// `ui/config/usage.rs` (orange < 65, yellow ≥ 65, red ≥ 85).
fn bar_spans(pct: f64) -> Vec<Span<'static>> {
    let pct = pct.clamp(0.0, 100.0);
    // `ACCOUNT_PANEL_BAR_CELLS` is a small constant (12); the cast to
    // f64 is exact within mantissa range.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let filled = ((pct / 100.0) * ACCOUNT_PANEL_BAR_CELLS as f64).round() as usize;
    let filled = filled.min(ACCOUNT_PANEL_BAR_CELLS);
    let empty = ACCOUNT_PANEL_BAR_CELLS - filled;
    let fill_color = if pct >= 85.0 {
        theme::STATUS_ERROR
    } else if pct >= 65.0 {
        theme::STATUS_WARNING
    } else {
        theme::RUST_ORANGE
    };
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(fill_color)),
        Span::styled("░".repeat(empty), Style::default().fg(theme::DIM)),
    ]
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

    // Row 0: dim rule.
    let rule_width = usize::from(width.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("─".repeat(rule_width), Style::default().fg(theme::DIM)),
    ]));
    // Row 1: blank.
    lines.push(Line::default());

    // Rows 2..=5: identity / posture block. Labels right-padded to
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
    let mode_label_fitted = truncate_with_ellipsis(&format!("[{mode_label}]"), value_budget);
    lines.push(Line::from(vec![
        Span::raw("  "),
        label_span("Mode", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::styled(mode_label_fitted, Style::default().fg(mode_color)),
    ]));

    // Model.
    let model_value = build_model_label(app).unwrap_or_else(|| "—".to_owned());
    let model_fitted = truncate_with_ellipsis(&model_value, value_budget);
    lines.push(Line::from(vec![
        Span::raw("  "),
        label_span("Model", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::raw(model_fitted),
    ]));

    // Fast mode.
    let (fast_label, fast_color) = fast_mode_label_and_color(app);
    lines.push(Line::from(vec![
        Span::raw("  "),
        label_span("Fast", ACCOUNT_PANEL_ID_LABEL_WIDTH),
        Span::raw("  "),
        Span::styled(fast_label.to_owned(), Style::default().fg(fast_color)),
    ]));

    // Row 6: blank.
    lines.push(Line::default());

    // Rows 7..=11: usage block. Labels right-padded to 3 chars ("Ctx",
    // "5h", "7d"). 2-space gutter before the bar; bar is 12 cells;
    // 2-space gutter before the percent.
    let ctx_pct = app.session_usage().context_usage_percent.map_or(0.0, f64::from);
    let ctx_pct_str = format!("{:>3}%", app.session_usage().context_usage_percent.unwrap_or(0));
    let mut ctx_line = vec![Span::raw("  "), label_span("Ctx", 3), Span::raw("  ")];
    ctx_line.extend(bar_spans(ctx_pct));
    ctx_line.push(Span::raw("  "));
    ctx_line.push(Span::raw(ctx_pct_str));
    lines.push(Line::from(ctx_line));

    push_usage_window_lines(
        &mut lines,
        "5h",
        app.usage.snapshot.as_ref().and_then(|s| s.five_hour.as_ref()),
    );
    push_usage_window_lines(
        &mut lines,
        "7d",
        app.usage.snapshot.as_ref().and_then(|s| s.seven_day.as_ref()),
    );

    // Row 12: blank.
    lines.push(Line::default());

    // Rows 13..=14: location + branch.
    let cwd_budget = usize::from(width).saturating_sub(2 + 2 + 1); // "  📁 "
    let cwd_value = fit_path_for_panel(app.cwd(), cwd_budget);
    lines.push(Line::from(vec![
        Span::styled("  📁 ".to_owned(), Style::default().fg(theme::DIM)),
        Span::styled(cwd_value, Style::default().fg(theme::DIM)),
    ]));

    let (branch_value, branch_color) = branch_label_and_color(app);
    let branch_budget = usize::from(width).saturating_sub(2 + 1 + 2); // "  ⎇  "
    let branch_value = truncate_with_ellipsis(&branch_value, branch_budget);
    lines.push(Line::from(vec![
        Span::styled("  ⎇  ".to_owned(), Style::default().fg(theme::DIM)),
        Span::styled(branch_value, Style::default().fg(branch_color)),
    ]));

    debug_assert_eq!(
        u16::try_from(lines.len()).unwrap_or(u16::MAX),
        ACCOUNT_PANEL_HEIGHT,
        "account panel must render exactly ACCOUNT_PANEL_HEIGHT rows so the layout split stays consistent",
    );
    lines
}

/// Append a 12-cell-bar row + an indented "resets in …" row for one
/// usage window. When the window is missing (no snapshot yet, account
/// has no Anthropic plan, etc.) both rows still render — bar at 0%,
/// reset line as `—` — so the panel's total row count stays at
/// `ACCOUNT_PANEL_HEIGHT`.
fn push_usage_window_lines(
    lines: &mut Vec<Line<'static>>,
    label: &'static str,
    window: Option<&crate::app::UsageWindow>,
) {
    let pct_value = window.map_or(0.0, |w| w.utilization);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct_text = window
        .map_or_else(|| "  —%".to_owned(), |w| format!("{:>3}%", w.utilization.round() as i64));
    let mut row = vec![Span::raw("  "), label_span(label, 3), Span::raw("  ")];
    row.extend(bar_spans(pct_value));
    row.push(Span::raw("  "));
    row.push(Span::raw(pct_text));
    lines.push(Line::from(row));

    let reset_text =
        window.and_then(crate::app::usage::format_window_reset).unwrap_or_else(|| "—".to_owned());
    lines.push(Line::from(vec![
        Span::raw("        "),
        Span::styled(reset_text, Style::default().fg(theme::DIM)),
    ]));
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

/// Model + effort badge for the panel — `display_name_short[/effort]`
/// when the model supports an effort scale, else just
/// `display_name_short`. Returns `None` when the CLI hasn't reported
/// a current model yet (early in spawn).
fn build_model_label(app: &App) -> Option<String> {
    let current = app.current_model()?;
    let mut badge = current.display_name_short.clone();
    if current.supports_effort {
        let effective =
            app.observed_effort().unwrap_or_else(|| app.config.thinking_effort_effective());
        badge.push('/');
        badge.push_str(effort_short_label(effective));
    }
    Some(badge)
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

/// Branch chip label + color. Detached HEAD highlights as
/// `STATUS_WARNING`. Empty result → "—" in DIM.
fn branch_label_and_color(app: &App) -> (String, Color) {
    match app.git_branch_chip() {
        Some(crate::app::git_context::BranchChip::Named(b)) => (b.to_owned(), theme::DIM),
        Some(crate::app::git_context::BranchChip::Detached) => {
            ("detached".to_owned(), theme::STATUS_WARNING)
        }
        None => ("—".to_owned(), theme::DIM),
    }
}

/// Head-truncate a filesystem path to fit `max_chars`. Prefers the
/// trailing path components (so the leaf project / file is preserved)
/// when full path doesn't fit. Mirrors the legacy footer's
/// `fit_location_value` shape but optimised for the 24-ish-cell
/// budget the panel offers.
fn fit_path_for_panel(cwd: &str, max_chars: usize) -> String {
    if cwd.chars().count() <= max_chars {
        return cwd.to_owned();
    }
    // Trailing-2-components fallback. If still too long, ellipsise.
    let trailing_two =
        cwd.split(['/', '\\']).filter(|c| !c.is_empty() && *c != "~").collect::<Vec<_>>();
    if let Some(tail) = trailing_two.last() {
        if let Some(second_last) = trailing_two.iter().rev().nth(1) {
            let joined = format!("{second_last}/{tail}");
            if joined.chars().count() <= max_chars {
                return joined;
            }
        }
        if tail.chars().count() <= max_chars {
            return (*tail).to_owned();
        }
    }
    truncate_with_ellipsis(cwd, max_chars)
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
    fn name_budget_active_matches_chrome() {
        // Wide tier (26): 26 - 10 chrome chars = 16.
        // Medium tier (20): 20 - 10 = 10.
        assert_eq!(name_budget_active(20), 10);
        assert_eq!(name_budget_active(26), 16);
    }

    #[test]
    fn name_budget_inactive_matches_chrome() {
        // Wide tier (26): 26 - 10 chrome chars = 16.
        // Medium tier (20): 20 - 10 = 10.
        assert_eq!(name_budget_inactive(20), 10);
        assert_eq!(name_budget_inactive(26), 16);
    }
}
