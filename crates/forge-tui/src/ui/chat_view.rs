use super::{autocomplete, chat, footer, help, input, layout, projects_pane, theme, todo, top_bar};
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::Rect;
#[cfg(feature = "perf")]
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

    let todo_height = {
        let _t = app.perf.as_ref().map(|p| p.start("ui::todo_height"));
        todo::compute_height(app)
    };
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
            todo_height,
            help_height,
            app.projects_pane_visible,
        )
    };
    // Cache for the mouse handler: pane click math reads
    // `app.layout.pane` to decide whether a click landed inside the
    // Projects pane before consulting `pane_hit_targets`.
    app.layout = areas.clone();

    // Narrow tier with the overlay open replaces the chat body with
    // the Projects overlay. Wide / Medium tiers and Narrow-with-
    // overlay-closed render the chat normally.
    let overlay_active = app.projects_pane_overlay_open && areas.top_bar.is_some();

    if !overlay_active {
        let _t = app.perf.as_ref().map(|p| p.start("ui::chat"));
        chat::render(frame, areas.body, app);
    }

    if overlay_active {
        // Overlay path: the projects_pane::render_overlay clears
        // `pane_hit_targets` itself before stamping new ones.
        let _t = app.perf.as_ref().map(|p| p.start("ui::projects_overlay"));
        let projects = app.workspace.as_ref().map(|ws| ws.list_projects()).unwrap_or_default();
        projects_pane::render_overlay(frame, areas.body, app, &projects);
    } else if let Some(pane_area) = areas.pane {
        let _t = app.perf.as_ref().map(|p| p.start("ui::projects_pane"));
        let projects = app.workspace.as_ref().map(|ws| ws.list_projects()).unwrap_or_default();
        projects_pane::render(frame, pane_area, app, &projects);
    } else {
        // No pane and no overlay this frame; clear stamps so a stale
        // set from the previous (visible) frame can't be hit-tested.
        // The top-bar render below will re-stamp the icon target.
        app.pane_hit_targets.clear();
    }

    // Narrow-tier top bar. Always last so its `▤` icon hit-target
    // sits at the end of `pane_hit_targets` and doesn't get stomped
    // by the inline-pane / overlay clearing above.
    if let Some(top_bar_area) = areas.top_bar {
        let _t = app.perf.as_ref().map(|p| p.start("ui::top_bar"));
        top_bar::render(frame, top_bar_area, app);
    }

    render_separator(frame, areas.input_sep);

    if areas.todo.height > 0 {
        let _t = app.perf.as_ref().map(|p| p.start("ui::todo"));
        todo::render(frame, areas.todo, app);
    }

    {
        let _t = app.perf.as_ref().map(|p| p.start("ui::input"));
        input::render(frame, areas.input, app);
    }

    if autocomplete::is_active(app) {
        let _t = app.perf.as_ref().map(|p| p.start("ui::autocomplete"));
        autocomplete::render(frame, areas.input, app);
    }

    render_separator(frame, areas.input_bottom_sep);

    if areas.help.height > 0 {
        let _t = app.perf.as_ref().map(|p| p.start("ui::help"));
        help::render(frame, areas.help, app);
    }

    if let Some(footer_area) = areas.footer {
        let _t = app.perf.as_ref().map(|p| p.start("ui::footer"));
        footer::render(frame, footer_area, app);
    }

    render_perf_fps_overlay(frame, frame_area, frame_area.y, app);
}

fn render_separator(frame: &mut Frame, area: Rect) {
    if area.height == 0 {
        return;
    }
    let sep_str = theme::SEPARATOR_CHAR.repeat(area.width as usize);
    let line = Line::from(Span::styled(sep_str, Style::default().fg(theme::DIM)));
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(feature = "perf")]
fn render_perf_fps_overlay(frame: &mut Frame, frame_area: Rect, y: u16, app: &App) {
    if app.perf.is_none() || frame_area.height == 0 || y >= frame_area.y + frame_area.height {
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
    let x = frame_area.x + frame_area.width.saturating_sub(width);
    let area = Rect { x, y, width, height: 1 };
    let line = Line::from(Span::styled(
        text,
        Style::default().fg(color).add_modifier(ratatui::style::Modifier::BOLD),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

#[cfg(not(feature = "perf"))]
fn render_perf_fps_overlay(_frame: &mut Frame, _frame_area: Rect, _y: u16, _app: &App) {}
