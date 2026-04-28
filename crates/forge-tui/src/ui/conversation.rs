//! Conversation screen — chat history above, input field below.
//!
//! Vertical stack: header / chat body / input separator / input /
//! help / footer. Markdown + syntax highlight are Phase 2.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;
use crate::ui::footer;
use crate::ui::theme;

/// Render the conversation screen.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(3),    // body (chat)
            Constraint::Length(1), // input separator
            Constraint::Length(input_height(app)),
            Constraint::Length(1), // help
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_header(frame, app, chunks[0]);
    render_body(frame, app, chunks[1]);
    render_input_sep(frame, chunks[2]);
    render_input(frame, app, chunks[3]);
    render_help(frame, chunks[4]);
    footer::render(frame, app, chunks[5]);
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let title = app.current_session.as_deref().map_or_else(
        || "⏵ no session".to_string(),
        |s| format!("⏵ session {}", s.split('-').next().unwrap_or(s)),
    );
    let count = format!("{} messages", app.messages.len());
    let line = Line::from(vec![
        Span::styled(title, theme::heading()),
        Span::styled(format!("    {count}"), theme::dim()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_body(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines: Vec<Line<'_>> = Vec::new();
    for msg in &app.messages {
        for line in render_message(msg) {
            lines.push(line);
        }
        lines.push(Line::from(""));
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme::dim());
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_input_sep(frame: &mut Frame<'_>, area: Rect) {
    let line = Line::from(Span::styled(
        "─".repeat(usize::from(area.width.max(1))),
        theme::dim(),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_input(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let prompt = if app.draft.is_empty() {
        "> ".to_string()
    } else {
        format!("> {}", app.draft)
    };
    let mut spans = vec![Span::styled(prompt, theme::text())];
    spans.push(Span::styled("▏", Style::default().add_modifier(Modifier::SLOW_BLINK)));
    let para = Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let line = Line::from(Span::styled(
        "Enter send   Esc back to picker   Ctrl-P claim primary   q quit",
        theme::dim(),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

/// Single-line input area for now. Multi-line is a later refinement.
fn input_height(_: &App) -> u16 {
    1
}

/// Convert a `session.event` message JSON into a list of styled lines.
/// Minimal MVP: shows role + text content; tool calls collapsed to one
/// line. Markdown / syntax highlight come later.
fn render_message(msg: &serde_json::Value) -> Vec<Line<'_>> {
    let role = msg
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("system");
    let role_label = match role {
        "user" => Line::from(Span::styled("user", Style::default().fg(theme::INFO))),
        "assistant" => Line::from(Span::styled("assistant", Style::default().fg(theme::ACCENT))),
        other => Line::from(Span::styled(other.to_string(), theme::dim())),
    };

    let mut out = vec![role_label];
    out.extend(content_lines(msg));
    out
}

fn content_lines(msg: &serde_json::Value) -> Vec<Line<'_>> {
    // Content shape varies. Common cases:
    // - msg.content == "string"
    // - msg.content == [{ type: "text", text: "..." }, { type: "tool_use", ... }]
    // Fall back to JSON dump if unrecognised.
    let Some(content) = msg.get("content") else {
        return vec![Line::from(Span::styled(msg.to_string(), theme::dim()))];
    };

    if let Some(s) = content.as_str() {
        return s.lines().map(|l| Line::from(l.to_string())).collect();
    }

    if let Some(arr) = content.as_array() {
        let mut lines: Vec<Line<'_>> = Vec::new();
        for item in arr {
            let kind = item.get("type").and_then(|v| v.as_str()).unwrap_or("?");
            match kind {
                "text" => {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        for l in text.lines() {
                            lines.push(Line::from(l.to_string()));
                        }
                    }
                }
                "tool_use" => {
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    lines.push(Line::from(vec![
                        Span::styled("▼ ", theme::dim()),
                        Span::styled(name.to_string(), Style::default().fg(theme::INFO)),
                    ]));
                }
                "tool_result" => {
                    lines.push(Line::from(Span::styled(
                        "  (tool result)",
                        theme::dim(),
                    )));
                }
                other => {
                    lines.push(Line::from(Span::styled(
                        format!("  [{other}]"),
                        theme::dim(),
                    )));
                }
            }
        }
        return lines;
    }

    vec![Line::from(Span::styled(content.to_string(), theme::dim()))]
}
