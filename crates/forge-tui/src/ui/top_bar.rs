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
/// either piece when it can't be resolved (e.g. no session focused,
/// no workspace, sleeping project). Truncated to fit `max_chars`.
fn build_active_context(app: &App, max_chars: usize) -> String {
    let project = active_project_label(app).unwrap_or_else(|| "\u{2014}".to_owned());
    let session = active_session_label(app).unwrap_or_else(|| "\u{2014}".to_owned());
    let raw = format!("{project}·{session}");
    projects_pane::truncate_with_ellipsis(&raw, max_chars)
}

/// Active project's user-facing `name` (from `forge.toml`), preferring
/// the catalog's answer and falling back to the bucket's own stamp - the
/// only source while a spawn's session is not catalogued yet, which is
/// the whole wake window.
fn active_project_label(app: &App) -> Option<String> {
    let active_key = app.active_session_key.as_ref()?;
    if let Some(roster) = app.surface().map(|surface| surface.roster()) {
        let refs: Vec<&ProjectView> = roster.projects.iter().collect();
        if let Some(view) = projects_pane::resolve_active_project_view(active_key, &refs) {
            return Some(view.name.clone());
        }
    }
    app.active_session().map(|s| s.project.clone()).filter(|project| !project.is_empty())
}

/// Compact representation of the active session for the top-bar
/// strip. Prefers the on-disk `SessionView::label` when one exists, says
/// `waking` while the bucket has not connected, and falls back to a
/// short-form session UUID.
fn active_session_label(app: &App) -> Option<String> {
    // A bucket mid-wake has no id and no catalog row yet; say what is
    // happening rather than leaving the strip blank until `Connected`.
    if let Some(active_key) = app.active_session_key.as_ref()
        && let Some(bucket) = app.sessions.get(active_key)
        && bucket.lifecycle_state == forge_primitives::SessionLifecycleState::Spawning
    {
        return Some("waking".to_owned());
    }
    if let Some(active_id) = app.session_id()
        && let Some(roster) = app.surface().map(|surface| surface.roster())
    {
        // A catalog row is named by the id the CLI wrote it under, so
        // the lookup follows the active bucket's occupant.
        for project in roster.projects {
            if let Some(view) = project.sessions.iter().find(|sv| sv.session == active_id)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::session::{SessionLifecycleState, UiSession};
    use forge_workspace::SessionSlot;

    /// The wake window is the one moment the strip has an id but no
    /// session behind it: the bucket exists under the id the spawn
    /// minted, and the CLI has not connected yet. It says what is
    /// happening rather than falling through to the em-dash placeholder.
    #[test]
    fn the_strip_says_waking_while_the_focused_bucket_has_not_connected() {
        let mut app = App::test_default();
        let key = SessionSlot::from_str_for_test("9f1c2b3a-4d5e-4f60-8a7b-0c1d2e3f4a5b");
        let mut bucket = UiSession::new(key.clone(), "forge");
        bucket.lifecycle_state = SessionLifecycleState::Spawning;
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        assert_eq!(active_session_label(&app).as_deref(), Some("waking"));
    }
}
