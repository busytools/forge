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

use forge_workspace::ProjectView;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
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
        let project_name = project.key.as_str().to_owned();
        let is_active = active_project_name.as_deref() == Some(project.key.as_str());

        // Project row. Hit-target stamps the un-truncated name so
        // click routing keeps working when the rendered label has
        // been head-truncated.
        let row_y = area.y + line_count_as_u16(lines);
        let project_label = truncate_with_ellipsis(project_name.as_str(), project_budget);
        lines.push(Line::from(Span::styled(
            format!("  {project_label}"),
            if is_active {
                Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            },
        )));
        app.pane_hit_targets.push(PaneHitTarget::ProjectHeader {
            project_name: project_name.clone(),
            y: row_y,
            height: 1,
        });

        // Drilldown rows: only the active project shows its sessions
        // at this fidelity. Background projects collapse to the
        // header row alone.
        if is_active {
            for (idx, session) in project.sessions.iter().enumerate() {
                let row_y = area.y + line_count_as_u16(lines);
                let lifecycle = app
                    .sessions
                    .get(&session.session)
                    .map_or(SessionLifecycleState::Sleeping, |s| s.lifecycle_state);
                let glyph = match lifecycle {
                    SessionLifecycleState::Running | SessionLifecycleState::Spawning => "⠋",
                    SessionLifecycleState::Attention => "△",
                    SessionLifecycleState::Sleeping => "·",
                    SessionLifecycleState::Idle => " ",
                };
                let lead_marker = if idx == 0 { "◆" } else { " " };
                let current_marker =
                    if Some(&session.session) == active_session_key.as_ref() { "•" } else { " " };
                let label = if session.label.is_empty() {
                    "main".to_owned()
                } else {
                    session.label.clone()
                };
                let session_label = truncate_with_ellipsis(&label, session_budget);
                lines.push(Line::from(vec![
                    Span::styled(format!("  {glyph} "), Style::default().fg(theme::DIM)),
                    Span::styled(lead_marker.to_owned(), Style::default().fg(theme::DIM)),
                    Span::raw(" "),
                    Span::styled(
                        current_marker.to_owned(),
                        Style::default().fg(theme::RUST_ORANGE),
                    ),
                    Span::raw(" "),
                    Span::raw(session_label),
                ]));
                app.pane_hit_targets.push(PaneHitTarget::SessionRow {
                    session_key: session.session.clone(),
                    y: row_y,
                    height: 1,
                });
            }
        }
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
/// drilldown. The leading chrome before the label is 8 chars: 2
/// indent + 1 lifecycle glyph + 1 sp + 1 lead marker (◆) + 1 sp + 1
/// current marker (•) + 1 sp.
fn session_max_chars(area_width: u16) -> usize {
    usize::from(area_width.saturating_sub(8))
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
        assert_eq!(session_max_chars(20), 12);
        assert_eq!(session_max_chars(26), 18);
    }
}
