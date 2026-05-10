//! Projects pane (left side, Wide tier ≥160 cols).
//!
//! Renders projects from
//! [`forge_workspace::Workspace::list_projects`] with the active
//! project highlighted; the active project's sessions drill down
//! immediately under its row. Each row stamps a [`PaneHitTarget`]
//! into [`App::pane_hit_targets`] for the mouse handler (next
//! commit) to read on click events.
//!
//! Render-time-stamp pattern from PR #83. See spec at
//! `~/.claude-subspace/plans/2026-05-10-forge-tui-projects-pane-wide-design.md`.

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

    for project in &sorted {
        let project_name = project.key.as_str().to_owned();
        let is_active = active_project_name.as_deref() == Some(project.key.as_str());

        // Project row.
        let row_y = area.y + line_count_as_u16(&lines);
        lines.push(Line::from(Span::styled(
            format!("  {project_name}"),
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
                let row_y = area.y + line_count_as_u16(&lines);
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
                lines.push(Line::from(vec![
                    Span::styled(format!("  {glyph} "), Style::default().fg(theme::DIM)),
                    Span::styled(lead_marker.to_owned(), Style::default().fg(theme::DIM)),
                    Span::raw(" "),
                    Span::styled(
                        current_marker.to_owned(),
                        Style::default().fg(theme::RUST_ORANGE),
                    ),
                    Span::raw(" "),
                    Span::raw(label),
                ]));
                app.pane_hit_targets.push(PaneHitTarget::SessionRow {
                    session_key: session.session.clone(),
                    y: row_y,
                    height: 1,
                });
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Saturating cast of `lines.len()` to `u16`. The pane area's height
/// is `u16` and projects tall enough to overflow `u16::MAX` rows
/// would already be wrong long before they overflow this cast — but
/// we saturate rather than panic so a runaway list at least caps
/// rather than aborting the renderer.
fn line_count_as_u16(lines: &[Line<'_>]) -> u16 {
    u16::try_from(lines.len()).unwrap_or(u16::MAX)
}
