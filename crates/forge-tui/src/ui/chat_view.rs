use super::{
    autocomplete, chat, help, input, inspector_pane, layout, projects_pane, theme, top_bar,
};
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub fn render(frame: &mut Frame, app: &mut App) {
    let _t = app.perf.as_ref().map(|p| p.start("ui::render"));
    let frame_area = frame.area();
    app.cached_frame_area = frame_area;
    crate::perf::mark_with("ui::frame_width", "cols", usize::from(frame_area.width));
    crate::perf::mark_with("ui::frame_height", "rows", usize::from(frame_area.height));

    help::sync_geometry_state(app, frame_area.width);
    let help_height = {
        let _t = app.perf.as_ref().map(|p| p.start("ui::help_height"));
        help::compute_height(app, frame_area.width)
    };
    let input_visual_lines = {
        let _t = app.perf.as_ref().map(|p| p.start("ui::input_visual_lines"));
        input::visual_line_count(app, frame_area.width)
    };
    let areas = {
        let _t = app.perf.as_ref().map(|p| p.start("ui::layout"));
        layout::compute(
            frame_area,
            input_visual_lines,
            help_height,
            app.projects_pane_visible,
            app.inspector_pane_visible,
        )
    };
    // Cache for the mouse handler: pane click math reads
    // `app.layout.pane` to decide whether a click landed inside the
    // Projects pane before consulting `pane_hit_targets`.
    app.layout = areas.clone();

    // Narrow tier with either overlay open replaces the chat body
    // with the overlay's full-screen content. Wide / Medium tiers
    // and Narrow-with-no-overlay render the chat normally.
    let projects_overlay = app.projects_pane_overlay_open && areas.top_bar.is_some();
    let inspector_overlay = app.inspector_pane_overlay_open && areas.top_bar.is_some();

    if !projects_overlay && !inspector_overlay {
        let _t = app.perf.as_ref().map(|p| p.start("ui::chat"));
        chat::render(frame, areas.body, app);
    }

    if projects_overlay {
        let _t = app.perf.as_ref().map(|p| p.start("ui::projects_overlay"));
        let projects = app.workspace.as_ref().map(|ws| ws.list_projects()).unwrap_or_default();
        projects_pane::render_overlay(frame, areas.body, app, &projects);
    } else if inspector_overlay {
        let _t = app.perf.as_ref().map(|p| p.start("ui::inspector_overlay"));
        inspector_pane::render_overlay(frame, areas.body, app);
    } else {
        // No overlay this frame. Render any visible inline side panes.
        // Each pane renderer manages its own hit-target stamping.
        if let Some(pane_area) = areas.pane {
            let _t = app.perf.as_ref().map(|p| p.start("ui::projects_pane"));
            let projects = app.workspace.as_ref().map(|ws| ws.list_projects()).unwrap_or_default();
            projects_pane::render(frame, pane_area, app, &projects);
            if let Some(sep_area) = areas.pane_separator {
                render_pane_separator(frame, sep_area);
            }
        } else if areas.top_bar.is_none() {
            // No pane and no overlay this frame; clear stamps so a stale
            // set from the previous (visible) frame can't be hit-tested.
            // The top-bar render below will re-stamp the icon target.
            app.pane_hit_targets.clear();
        }
        if let Some(pane_right_area) = areas.pane_right {
            let _t = app.perf.as_ref().map(|p| p.start("ui::inspector_pane"));
            inspector_pane::render(frame, pane_right_area, app);
            if let Some(sep_area) = areas.pane_right_separator {
                render_pane_separator(frame, sep_area);
            }
        }
    }

    // Narrow-tier top bar. Always last so its icon hit-targets sit at
    // the end of `pane_hit_targets` and don't get stomped by the
    // inline-pane / overlay clearing above.
    if let Some(top_bar_area) = areas.top_bar {
        let _t = app.perf.as_ref().map(|p| p.start("ui::top_bar"));
        top_bar::render(frame, top_bar_area, app);
    }

    {
        let _t = app.perf.as_ref().map(|p| p.start("ui::input"));
        input::render(frame, areas.input, app);
    }

    if autocomplete::is_active(app) {
        let _t = app.perf.as_ref().map(|p| p.start("ui::autocomplete"));
        autocomplete::render(frame, areas.input, app);
    }

    if areas.help.height > 0 {
        let _t = app.perf.as_ref().map(|p| p.start("ui::help"));
        help::render(frame, areas.help, app);
    }

    // Chat footer renders at the bottom of the Projects pane
    // (`projects_pane::render_account_status_footer`); todos render
    // in the Inspector pane (`inspector_pane::render`).
    render_perf_fps_overlay(frame, frame_area, frame_area.y, app);
}

/// Render the full-height `│` column between a side pane and the
/// chat column in DIM (DarkGray) — matches the rest of the pane's
/// structural chrome (the underline rule + section headers).
fn render_pane_separator(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines: Vec<Line<'static>> = (0..area.height)
        .map(|_| Line::from(Span::styled("│".to_owned(), Style::default().fg(theme::DIM))))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Render the live FPS indicator at the top-right of `frame_area`.
/// Always on — the FPS counter is computed unconditionally
/// (`App::mark_frame_presented` / `App::frame_fps`) and the overlay
/// is cheap (one styled `Line`, one rect).
fn render_perf_fps_overlay(frame: &mut Frame, frame_area: Rect, y: u16, app: &App) {
    if frame_area.height == 0 || y >= frame_area.y + frame_area.height {
        return;
    }
    let Some(fps) = app.frame_fps() else {
        return;
    };

    let color = if fps >= 55.0 {
        Color::Green
    } else if fps >= 45.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    let text = format!("[{fps:>5.1} FPS]");
    let width = u16::try_from(text.len()).unwrap_or(frame_area.width).min(frame_area.width);
    // Leave a 1-col right gutter to match the rest of the UI's
    // padding (chat box / panes / inspector all sit 1 col off their
    // surrounding edge). Drop the gutter if the frame is too narrow
    // to fit the FPS text + gutter — better to show the FPS hugging
    // the edge than to clip it entirely.
    let right_gutter: u16 = u16::from(frame_area.width > width);
    let x = frame_area.x + frame_area.width.saturating_sub(width + right_gutter);
    let area = Rect { x, y, width, height: 1 };
    let line = Line::from(Span::styled(
        text,
        Style::default().fg(color).add_modifier(ratatui::style::Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(line), area);
}
