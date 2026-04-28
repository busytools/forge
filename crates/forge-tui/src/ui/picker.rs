//! Session picker screen — list of historical sessions for `cwd` plus
//! a "✨ New session" pseudo-row at top.
//!
//! Picker cursor 0 = "New session"; 1.. = `app.session_list[idx-1]`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::ui::footer;
use crate::ui::theme;

/// Render the picker screen.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header
            Constraint::Min(1),    // list
            Constraint::Length(1), // help
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_header(frame, app, chunks[0]);
    render_list(frame, app, chunks[1]);
    render_help(frame, chunks[2]);
    footer::render(frame, app, chunks[3]);
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let count = app.session_list.len();
    let line = Line::from(vec![
        Span::styled("Sessions for ", crate::ui::style::dim()),
        Span::styled(short_cwd(&app.cwd), crate::ui::style::text()),
        Span::styled(format!("    {count} session"), crate::ui::style::dim()),
        Span::styled(if count == 1 { "" } else { "s" }, crate::ui::style::dim()),
    ]);
    let para = Paragraph::new(vec![line, Line::from("")]);
    frame.render_widget(para, area);
}

fn render_list(frame: &mut Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(app.session_list.len() + 1);

    // Row 0 — "New session" pseudo-entry.
    let new_selected = app.picker_cursor == 0;
    lines.push(line_for_new_session(new_selected));
    lines.push(Line::from(""));

    for (idx, session) in app.session_list.iter().enumerate() {
        let selected = app.picker_cursor == idx + 1;
        lines.push(line_for_session(session, selected));
        lines.push(Line::from(""));
    }

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(crate::ui::style::dim());
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
}

fn line_for_new_session(selected: bool) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let style = if selected {
        crate::ui::style::selected()
    } else {
        Style::default().fg(theme::RUST_ORANGE)
    };
    Line::from(Span::styled(format!("{marker} ✨  New session"), style))
}

fn line_for_session(session: &serde_json::Value, selected: bool) -> Line<'_> {
    let title = session
        .get("custom_title")
        .and_then(|v| v.as_str())
        .or_else(|| session.get("summary").and_then(|v| v.as_str()))
        .or_else(|| session.get("first_prompt").and_then(|v| v.as_str()))
        .unwrap_or("(untitled)");
    let sid_short = session
        .get("session_id")
        .and_then(|v| v.as_str())
        .map_or_else(
            || "????????".to_string(),
            |s| s.split('-').next().unwrap_or(s).to_string(),
        );

    let marker = if selected { ">" } else { " " };
    let style = if selected {
        crate::ui::style::selected()
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled("○ ", crate::ui::style::dim()),
        Span::styled(format!("{sid_short}  "), crate::ui::style::dim()),
        Span::styled(truncate(title, 60), style),
    ])
}

fn render_help(frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let line = Line::from(Span::styled(
        "↑↓ navigate   Enter open   q quit",
        crate::ui::style::dim(),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn short_cwd(cwd: &str) -> String {
    if cwd.is_empty() {
        return "─".into();
    }
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = cwd.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    cwd.to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut buf: String = s.chars().take(max.saturating_sub(1)).collect();
        buf.push('…');
        buf
    }
}
