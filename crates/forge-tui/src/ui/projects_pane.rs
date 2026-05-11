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

    // Pane name banner: PROJECTS at row 0 (no leading blank — the
    // terminal frame above is breathing room enough), dim rule, then
    // one blank before the first section.
    lines.push(Line::from(Span::styled(
        "  PROJECTS".to_owned(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    )));
    let rule_width = usize::from(area.width.saturating_sub(2));
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("─".repeat(rule_width), Style::default().fg(theme::DIM)),
    ]));
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
    for project in projects {
        let spawn_synthetic =
            forge_workspace::SessionKey::from_session_id(format!("__spawn_{}__", project.name));
        let live_session = project.sessions.iter().find_map(|s| {
            app.sessions.get(&s.session).map(|bucket| (s.session.clone(), bucket.lifecycle_state))
        });
        let synthetic = app
            .sessions
            .get(&spawn_synthetic)
            .map(|bucket| (spawn_synthetic.clone(), bucket.lifecycle_state));
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
        let name_budget = name_budget_active(area.width);
        for (project, lifecycle, is_focused, session_key) in &active {
            let row_y = area.y + line_count_as_u16(lines);
            let (glyph, glyph_color) = glyph_for_lifecycle(*lifecycle, *is_focused, spinner_frame);
            let label = truncate_with_ellipsis(project.name.as_str(), name_budget);
            let label_pad =
                name_budget.saturating_sub(label.chars().count());
            let name_style = if *is_focused {
                Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let time =
                format_relative_time(project.sessions.first().and_then(|s| s.last_activity), now);
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(glyph, Style::default().fg(glyph_color)),
                Span::raw(" "),
                Span::styled(label, name_style),
                Span::raw(" ".repeat(label_pad)),
                Span::raw(" "),
                Span::styled(time, Style::default().fg(theme::DIM)),
                Span::raw(" "),
                Span::styled("×".to_owned(), Style::default().fg(theme::DIM)),
                Span::raw("  "),
            ]));
            app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
                project_name: project.key.as_str().to_owned(),
                y: row_y,
                height: 1,
            });
            // Close-glyph hit target — 3-col band covering the space
            // before × (col area.right-4), the × itself
            // (area.right-3), and the first gutter col (area.right-2).
            // Wider than the literal glyph so accidental clicks one
            // column off still register; clicks on the rightmost
            // gutter col are reserved as "row click" (focus/switch)
            // to keep the visual gutter inert.
            let row_right = area.x.saturating_add(area.width);
            let close_x_start = row_right.saturating_sub(4);
            let close_x_end = row_right.saturating_sub(1);
            app.pane_hit_targets.push(PaneHitTarget::CloseSession {
                session_key: session_key.clone(),
                y: row_y,
                height: 1,
                x_start: close_x_start,
                x_end: close_x_end,
            });
        }
    }

    if !inactive.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  INACTIVE".to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        )));
        let name_budget = name_budget_inactive(area.width);
        for project in &inactive {
            let row_y = area.y + line_count_as_u16(lines);
            let label = truncate_with_ellipsis(project.name.as_str(), name_budget);
            let label_pad =
                name_budget.saturating_sub(label.chars().count());
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
        }
    }
}

/// Active-row layout:
/// `<2 indent><1 glyph><1 sp><name><1 sp><3 time><1 sp><1 ×><2 right gutter>`
/// = 11 chrome chars. The 2-col trailing gutter mirrors the chat
/// column's left gutter so the pane content reads as inset from
/// the separator on both sides.
fn name_budget_active(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(11))
}

/// Inactive-row layout:
/// `<2 indent><1 glyph><1 sp><name><1 sp><3 time><2 right gutter>`
/// = 9 chrome chars. No close column; same 2-col gutter.
fn name_budget_inactive(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(9))
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
    if raw.chars().count() > 3 {
        raw.chars().take(3).collect()
    } else {
        format!("{raw:>3}")
    }
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
        // Idle = "alive, no turn in progress". Use a filled bullet so
        // the row reads as occupied (the design spec calls for blank
        // here, but in practice an empty glyph column makes Active /
        // Inactive rows look interchangeable). Active-session bullet
        // picks up the accent colour to match its bold label.
        SessionLifecycleState::Idle => {
            let color = if session_is_active { theme::RUST_ORANGE } else { theme::DIM };
            ("●".to_owned(), color)
        }
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
    fn name_budget_active_matches_chrome() {
        // Wide tier (26): 26 - 11 chrome chars = 15.
        // Medium tier (20): 20 - 11 = 9.
        assert_eq!(name_budget_active(20), 9);
        assert_eq!(name_budget_active(26), 15);
    }

    #[test]
    fn name_budget_inactive_matches_chrome() {
        // Wide tier (26): 26 - 9 chrome chars = 17.
        // Medium tier (20): 20 - 9 = 11.
        assert_eq!(name_budget_inactive(20), 11);
        assert_eq!(name_budget_inactive(26), 17);
    }
}
