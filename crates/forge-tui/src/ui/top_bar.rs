//! Narrow-tier top bar.
//!
//! At terminal widths < `MEDIUM_TIER_MIN_WIDTH` (120 cols) both
//! inline side panes disappear and are replaced by this single-row
//! indicator at the top of the chat area. Format:
//!
//! ```text
//! ▤  <active-project>·<active-session>                       ▦
//! ```
//!
//! Clicking the leading `▤` icon (or pressing `Cmd+Left` on macOS,
//! `Ctrl+Left` elsewhere) toggles the Narrow-tier Projects overlay
//! rendered by [`crate::ui::projects_pane::render_overlay`]. Clicking
//! the trailing `▦` icon (or pressing `Cmd+Right` on macOS,
//! `Ctrl+Right` elsewhere) toggles the Inspector overlay. Both icons
//! are stamped as their own hit-target variants for the mouse handler.

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
/// hit-targets for both `▤` and `▦` icons.
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let projects_icon = "▤";
    let inspector_icon = "\u{25a6}"; // ▦

    let projects_icon_style = if app.projects_pane_overlay_open {
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::DIM)
    };
    let inspector_icon_style = if app.inspector_pane_overlay_open {
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::DIM)
    };

    // Layout: ▤ + 2 spaces + label + filler + ▦.
    // Reserve 1 col for each icon and 2 spaces after the left icon;
    // the trailing icon sits flush against the right edge with no
    // post-gutter so the label can stretch as wide as possible.
    let left_prefix_cols = 3u16; // "▤" + 2 spaces
    let right_suffix_cols = 1u16; // "▦"
    let label_budget = usize::from(area.width.saturating_sub(left_prefix_cols + right_suffix_cols));
    let active_context = build_active_context(app, label_budget);
    let context_chars = active_context.chars().count();
    let filler =
        usize::from(area.width).saturating_sub(usize::from(left_prefix_cols) + context_chars + 1);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(projects_icon.to_owned(), projects_icon_style),
            Span::raw("  "),
            Span::styled(active_context, Style::default().fg(theme::DIM)),
            Span::raw(" ".repeat(filler)),
            Span::styled(inspector_icon.to_owned(), inspector_icon_style),
        ])),
        area,
    );

    // Stamp the left ▤ hit-target.
    app.pane_hit_targets.push(PaneHitTarget::TopBarIcon {
        y: area.y,
        height: 1,
        x_start: area.x,
        x_end: area.x.saturating_add(1),
    });
    // Stamp the right ▦ hit-target - last column of the area.
    let right_end = area.x.saturating_add(area.width);
    app.pane_hit_targets.push(PaneHitTarget::InspectorTopBarIcon {
        y: area.y,
        height: 1,
        x_start: right_end.saturating_sub(1),
        x_end: right_end,
    });
}

/// Build the `<project>·<session>` label, falling back to ` - ` for
/// either piece when it can't be resolved (e.g. pre-Connect, no
/// workspace, sleeping project). Truncated to fit `max_chars`.
fn build_active_context(app: &App, max_chars: usize) -> String {
    let project = active_project_label(app).unwrap_or_else(|| "\u{2014}".to_owned());
    let session = active_session_label(app).unwrap_or_else(|| "\u{2014}".to_owned());
    let raw = format!("{project}·{session}");
    projects_pane::truncate_with_ellipsis(&raw, max_chars)
}

/// Active project's user-facing `name` (from `forge.toml`). Handles
/// the synthetic-key sentinels (`__spawn_<name>__`, `__resume_<id>__`,
/// `__conn_pending__`) so the top bar reflects the project the user
/// just clicked even during the Spawning window - before `Connected`
/// arrives and the bucket migrates to its real session id.
fn active_project_label(app: &App) -> Option<String> {
    let workspace = app.workspace.as_ref()?;
    let active_key = app.active_session_key.as_ref()?;
    let projects = workspace.list_projects();
    let refs: Vec<&ProjectView> = projects.iter().collect();
    projects_pane::resolve_active_project_view(active_key, &refs).map(|p| p.name.clone())
}

/// Compact representation of the active session for the top-bar
/// strip. Prefers the on-disk `SessionView::label` when one exists;
/// falls back to a short-form session UUID; finally `None` for the
/// pre-Connect bucket.
fn active_session_label(app: &App) -> Option<String> {
    if let Some(active_key) = app.active_session_key.as_ref() {
        let s = active_key.as_str();
        // Synthetic keys: surface a short status word rather than the
        // raw sentinel string. Lets the user see *what's happening*
        // (waking / resuming) instead of `__spawn_dotfiles__`.
        if s.starts_with("__spawn_") && s.ends_with("__") {
            return Some("waking".to_owned());
        }
        if let Some(id) = s.strip_prefix("__resume_").and_then(|r| r.strip_suffix("__"))
            && let Some(workspace) = app.workspace.as_ref()
        {
            for project in workspace.list_projects() {
                if let Some(view) = project.sessions.iter().find(|sv| sv.session.as_str() == id)
                    && !view.label.is_empty()
                {
                    return Some(view.label.clone());
                }
            }
        }
        if let Some(workspace) = app.workspace.as_ref() {
            for project in workspace.list_projects() {
                if let Some(view) = project.sessions.iter().find(|sv| &sv.session == active_key)
                    && !view.label.is_empty()
                {
                    return Some(view.label.clone());
                }
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
