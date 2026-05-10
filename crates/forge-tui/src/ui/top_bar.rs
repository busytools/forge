//! Narrow-tier top bar.
//!
//! At terminal widths < `MEDIUM_TIER_MIN_WIDTH` (120 cols) the
//! inline Projects pane disappears and is replaced by this single-
//! row indicator at the top of the chat area. Format:
//!
//! ```text
//! ▤  <active-project>·<active-session>
//! ```
//!
//! Clicking the leading `▤` icon (or pressing `Ctrl+B`) toggles the
//! Narrow-tier overlay rendered by
//! [`crate::ui::projects_pane::render_overlay`]. The icon is stamped
//! as a [`PaneHitTarget::TopBarIcon`] for the mouse handler.
//!
//! See spec at
//! `~/.claude-subspace/plans/2026-05-10-forge-tui-projects-pane-narrow-design.md`.

use forge_workspace::ProjectView;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::projects_pane;
use super::theme;
use crate::app::App;
use crate::app::PaneHitTarget;

/// Render the Narrow-tier top bar into `area` (a one-row rect at the
/// top of the chat area allocated by `layout::compute`). Stamps the
/// `▤` icon's hit-target so the mouse handler can route an icon
/// click to overlay-toggle.
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let icon = "▤";
    let icon_style = if app.projects_pane_overlay_open {
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::DIM)
    };

    // The icon takes 1 col; we then pad with 2 spaces before the
    // active-context label (matching the design mock). Reserve those
    // 3 cols when computing the label budget.
    let prefix_cols = 3u16;
    let label_budget = usize::from(area.width.saturating_sub(prefix_cols));
    let active_context = build_active_context(app, label_budget);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(icon.to_owned(), icon_style),
            Span::raw("  "),
            Span::styled(active_context, Style::default().fg(theme::DIM)),
        ])),
        area,
    );

    // Stamp the icon hit-target (column area.x, 1 char wide).
    app.pane_hit_targets.push(PaneHitTarget::TopBarIcon {
        y: area.y,
        height: 1,
        x_start: area.x,
        x_end: area.x.saturating_add(1),
    });
}

/// Build the `<project>·<session>` label, falling back to `—` for
/// either piece when it can't be resolved (e.g. pre-Connect, no
/// workspace, sleeping project). Truncated to fit `max_chars`.
fn build_active_context(app: &App, max_chars: usize) -> String {
    let project = active_project_label(app).unwrap_or_else(|| "—".to_owned());
    let session = active_session_label(app).unwrap_or_else(|| "—".to_owned());
    let raw = format!("{project}·{session}");
    projects_pane::truncate_with_ellipsis(&raw, max_chars)
}

/// Active project's `key.as_str()` — matches the identifier the
/// pane and click router use. Returns `None` when no workspace is
/// attached (test contexts) or no project owns the active session.
fn active_project_label(app: &App) -> Option<String> {
    let workspace = app.workspace.as_ref()?;
    let active_key = app.active_session_key.as_ref()?;
    workspace
        .list_projects()
        .into_iter()
        .find(|p: &ProjectView| p.sessions.iter().any(|s| &s.session == active_key))
        .map(|p| p.key.as_str().to_owned())
}

/// Compact representation of the active session for the top-bar
/// strip. Prefers the on-disk `SessionView::label` when one exists;
/// falls back to a short-form session UUID; finally `None` for the
/// pre-Connect bucket.
fn active_session_label(app: &App) -> Option<String> {
    if let Some(workspace) = app.workspace.as_ref()
        && let Some(active_key) = app.active_session_key.as_ref()
    {
        for project in workspace.list_projects() {
            if let Some(view) = project.sessions.iter().find(|s| &s.session == active_key)
                && !view.label.is_empty()
            {
                return Some(view.label.clone());
            }
        }
    }
    app.session_id().map(|sid| {
        let s = sid.to_string();
        if s.chars().count() > 8 {
            let mut short: String = s.chars().take(8).collect();
            short.push('…');
            short
        } else {
            s
        }
    })
}
