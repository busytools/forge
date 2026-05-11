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
//! `~/.claude-subspace/plans/2026-05-10-forge-tui-projects-pane-wide-design.md`
//! and `~/.claude-subspace/plans/2026-05-10-forge-tui-projects-pane-medium-design.md`.

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

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Pane name banner: "PROJECTS" in accent bold + dim rule + blank.
    lines.push(Line::from(Span::styled(
        "PROJECTS".to_owned(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )));
    let rule_width = usize::from(area.width.saturating_sub(2));
    lines.push(Line::from(Span::styled("─".repeat(rule_width), Style::default().fg(theme::DIM))));
    lines.push(Line::default());

    append_project_rows(&mut lines, area, app, projects);

    frame.render_widget(Paragraph::new(lines), area);
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

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Banner row: `▤ PROJECTS … ✕` spanning the full overlay width.
    let banner_label = "▤ PROJECTS";
    let close_glyph = "✕";
    let banner_chars = banner_label.chars().count();
    let close_chars = close_glyph.chars().count();
    let pad = usize::from(area.width).saturating_sub(banner_chars).saturating_sub(close_chars);
    lines.push(Line::from(vec![
        Span::styled(
            banner_label.to_owned(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(pad)),
        Span::styled(close_glyph.to_owned(), Style::default().fg(theme::DIM)),
    ]));
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

    // Dim rule under the banner.
    let rule_width = usize::from(area.width);
    lines.push(Line::from(Span::styled("─".repeat(rule_width), Style::default().fg(theme::DIM))));
    lines.push(Line::default());

    append_project_rows(&mut lines, area, app, projects);

    frame.render_widget(Paragraph::new(lines), area);
}

/// Shared row-building helper used by [`render`] and [`render_overlay`].
/// Pushes one styled `Line` per project (header + drilldown sessions
/// for the active project) into `lines` and stamps the matching
/// hit-targets into `app.pane_hit_targets`. Hit-target y-positions
/// are computed from `area.y + lines.len()` as each row is appended.
fn append_project_rows(
    lines: &mut Vec<Line<'static>>,
    area: Rect,
    app: &mut App,
    projects: &[ProjectView],
) {
    // Sort: most-recently-active first; alphabetical tie-break on
    // project key. `sessions[0]` is the lead by `list_projects`
    // contract, so its `last_activity` carries the project-level
    // activity timestamp.
    let mut sorted: Vec<&ProjectView> = projects.iter().collect();
    sorted.sort_by(|a, b| {
        let a_act = a.sessions.first().and_then(|s| s.last_activity);
        let b_act = b.sessions.first().and_then(|s| s.last_activity);
        b_act.cmp(&a_act).then_with(|| a.key.as_str().cmp(b.key.as_str()))
    });

    let active_session_key = app.active_session_key.clone();
    let active_project_name: Option<String> = active_session_key
        .as_ref()
        .and_then(|key| sorted.iter().find(|p| p.sessions.iter().any(|s| &s.session == key)))
        .map(|p| p.key.as_str().to_owned());

    let project_budget = project_max_chars(area.width);
    let session_budget = session_max_chars(area.width);

    for project in &sorted {
        // `project.key` is the sanitised filesystem path (e.g.
        // `-Users-vedhavyas-Projects-dotfiles`) used as the catalog
        // index and the click-routing identifier. `project.name` is
        // the user-facing `name` from `forge.toml` (e.g. `dotfiles`).
        // Render the friendly name; stamp the key on the hit target
        // so `switch_to_project_lead` keeps finding it via
        // `workspace.list_projects().find(|p| p.key == project_name)`.
        let project_key = project.key.as_str().to_owned();
        let is_active = active_project_name.as_deref() == Some(project.key.as_str());

        // Project row. Hit-target stamps the un-truncated name so
        // click routing keeps working when the rendered label has
        // been head-truncated. Per
        // `~/.claude-subspace/plans/2026-05-08-forge-tui-side-panes-design.md`
        // §3.1 the project row is *name only* — no count, no time,
        // no aggregate state glyph. Per-session detail (state, time,
        // unread) lives on the drilldown rows below.
        let row_y = area.y + line_count_as_u16(lines);
        let project_label = truncate_with_ellipsis(project.name.as_str(), project_budget);
        lines.push(Line::from(Span::styled(
            format!("  {project_label}"),
            if is_active {
                Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            },
        )));
        app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
            project_name: project_key,
            y: row_y,
            height: 1,
        });

        // Drilldown rows: only the active project shows its sessions
        // at this fidelity. Background projects collapse to the
        // header row alone.
        //
        // last_activity rendering: the routed-event handler in
        // `app::events::client::handle_client_event` stamps
        // `last_activity_at` on every wire event applied to a session
        // bucket. The "2m / 1h / 5d" relative-time column called for
        // by the spec is intentionally deferred to a follow-up commit
        // — wiring it here would require shrinking `session_budget`
        // by ~4 chars and resnapshotting every Wide+Medium pane test.
        // Field is current; rendering is the only piece left.
        if is_active {
            // Cap the drilldown at DRILLDOWN_CAP sessions to keep the
            // pane usable for projects with long catalogs. The hidden
            // remainder is summarised inline as `+ N more`; the user
            // can still resume any session via the in-session
            // `/resume` picker if they need to reach back further.
            let total = project.sessions.len();
            let visible = total.min(DRILLDOWN_CAP);
            let now = SystemTime::now();
            let spinner_frame = app.spinner_frame;
            for (idx, session) in project.sessions.iter().take(visible).enumerate() {
                let row_y = area.y + line_count_as_u16(lines);
                let lifecycle = app
                    .sessions
                    .get(&session.session)
                    .map_or(SessionLifecycleState::Sleeping, |s| s.lifecycle_state);
                let session_is_active = Some(&session.session) == active_session_key.as_ref();
                let (glyph, glyph_color) =
                    glyph_for_lifecycle(lifecycle, session_is_active, spinner_frame);
                let lead_marker = if idx == 0 { "◆" } else { " " };
                let current_marker = if session_is_active { "•" } else { " " };
                let label = if session.label.is_empty() {
                    "main".to_owned()
                } else {
                    session.label.clone()
                };
                let session_label = truncate_with_ellipsis(&label, session_budget);
                // Pad the label out to its budget so the trailing
                // time column lands at a stable column for every row.
                let label_char_count = session_label.chars().count();
                let label_padding = session_budget.saturating_sub(label_char_count);
                let padded_label = format!("{session_label}{}", " ".repeat(label_padding));
                let time = format_relative_time(session.last_activity, now);
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(glyph, Style::default().fg(glyph_color)),
                    Span::raw(" "),
                    Span::styled(lead_marker.to_owned(), Style::default().fg(theme::DIM)),
                    Span::raw(" "),
                    Span::styled(
                        current_marker.to_owned(),
                        Style::default().fg(theme::RUST_ORANGE),
                    ),
                    Span::raw(" "),
                    Span::raw(padded_label),
                    Span::raw(" "),
                    Span::styled(time, Style::default().fg(theme::DIM)),
                ]));
                app.pane_hit_targets.push(PaneHitTarget::SessionRow {
                    session_key: session.session.clone(),
                    y: row_y,
                    height: 1,
                });
            }
            if total > visible {
                let remainder = total - visible;
                lines.push(Line::from(Span::styled(
                    format!("      + {remainder} more"),
                    Style::default().fg(theme::DIM),
                )));
            }
        }
    }
}

/// Active-project drilldown is capped at this many session rows. The
/// rest collapses to a `+ N more` indicator. Tuned so that a typical
/// 13-project `forge.toml` fits in one pane height alongside the
/// active project's recent context.
const DRILLDOWN_CAP: usize = 3;

/// Saturating cast of `lines.len()` to `u16`. The pane area's height
/// is `u16` and projects tall enough to overflow `u16::MAX` rows
/// would already be wrong long before they overflow this cast — but
/// we saturate rather than panic so a runaway list at least caps
/// rather than aborting the renderer.
fn line_count_as_u16(lines: &[Line<'_>]) -> u16 {
    u16::try_from(lines.len()).unwrap_or(u16::MAX)
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
/// See `~/.claude-subspace/plans/2026-05-10-forge-tui-projects-pane-wide-design.md`.
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
        SessionLifecycleState::Sleeping => ("·".to_owned(), theme::DIM),
        SessionLifecycleState::Idle => (" ".to_owned(), Color::Reset),
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

/// Max characters available for a project name on a single row.
/// Project rows have a 2-char indent before the name; the rest of
/// the row width is the budget.
fn project_max_chars(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(2))
}

/// Max characters available for a session label in the active-project
/// drilldown. Leading chrome: 2 indent + 1 lifecycle glyph + 1 sp + 1
/// lead marker (◆) + 1 sp + 1 current marker (•) + 1 sp = 8. Trailing
/// time column: 1 sp + 3 chars (`12m` / `1h` / `2d` / `1w`,
/// right-aligned) = 4. Label fits in the middle.
fn session_max_chars(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(8 + 4))
}

/// Width of the relative-time digits column on a session row (3
/// chars: `12m`, `1h`, `2d`, `1w`). Leading space and the row's right
/// edge bracket the column so it sits flush at every tier.
const TIME_DIGITS_WIDTH: usize = 3;

/// Format `activity` as a short relative-time string anchored at
/// `now`: `now`, `Xm`, `Xh`, `Xd`, or `Xw`, capped at 3 visible chars.
/// Anything older than 99 weeks clamps to `99w`. Returns 3 spaces
/// when activity is `None` so column alignment stays stable.
fn format_relative_time(activity: Option<SystemTime>, now: SystemTime) -> String {
    let Some(activity) = activity else {
        return " ".repeat(TIME_DIGITS_WIDTH);
    };
    let elapsed = now.duration_since(activity).unwrap_or_default();
    let secs = elapsed.as_secs();
    let formatted = if secs < 60 {
        "now".to_owned()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 604_800 {
        format!("{}d", secs / 86_400)
    } else {
        let weeks = (secs / 604_800).min(99);
        format!("{weeks}w")
    };
    if formatted.chars().count() > TIME_DIGITS_WIDTH {
        // Shouldn't happen with the caps above, but truncate defensively.
        formatted.chars().take(TIME_DIGITS_WIDTH).collect()
    } else {
        // Right-align so the unit suffix sits flush against the row's
        // right edge regardless of digit count.
        format!("{formatted:>TIME_DIGITS_WIDTH$}")
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
    fn project_max_chars_matches_indent() {
        assert_eq!(project_max_chars(20), 18);
        assert_eq!(project_max_chars(26), 24);
    }

    #[test]
    fn session_max_chars_accounts_for_chrome() {
        // Wide tier (26): 26 - 8 (left chrome) - 4 (time column) = 14.
        // Medium tier (20): 20 - 8 - 4 = 8.
        assert_eq!(session_max_chars(20), 8);
        assert_eq!(session_max_chars(26), 14);
    }
}
