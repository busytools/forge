//! Conversation screen — chat history above, input field below.
//!
//! Vertical stack: header / chat body / input separator / input /
//! help / footer. Body scrolls; PgUp/PgDn/Home/End drive offset.
//! Auto-tails new messages while the user hasn't scrolled up.

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
            Constraint::Length(1), // input
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
    let scroll_hint = if app.conv_user_scrolled {
        "  (PgDn / End to follow)"
    } else {
        ""
    };
    let line = Line::from(vec![
        Span::styled(title, theme::heading()),
        Span::styled(format!("    {count}"), theme::dim()),
        Span::styled(scroll_hint, Style::default().fg(theme::WARN)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_body(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lines = build_message_lines(&app.messages, area.width);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "u16 truncation for terminal scroll offset is fine; max_scroll clamps"
    )]
    let total = lines.len().min(u16::MAX as usize) as u16;
    // Body has a top border (1 line); usable rows = area.height - 1.
    let viewport = area.height.saturating_sub(1);
    let max_scroll = total.saturating_sub(viewport);
    let scroll = if app.conv_user_scrolled {
        app.conv_scroll.min(max_scroll)
    } else {
        max_scroll
    };

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(theme::dim());
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
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
    let mut spans = vec![
        Span::styled("> ", Style::default().fg(theme::ACCENT)),
        Span::styled(app.draft.as_str(), theme::text()),
        Span::styled("▏", Style::default().add_modifier(Modifier::SLOW_BLINK)),
    ];
    if app.draft.is_empty() {
        spans.insert(
            2,
            Span::styled("type a message…", theme::dim()),
        );
    }
    let para = Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let line = Line::from(Span::styled(
        "Enter send   PgUp/PgDn scroll   End follow   Esc picker   q quit",
        theme::dim(),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

/// Build the full list of styled lines for the conversation. Width is
/// passed so we can render full-width separators between turns.
fn build_message_lines(messages: &[serde_json::Value], width: u16) -> Vec<Line<'_>> {
    let mut out: Vec<Line<'_>> = Vec::with_capacity(messages.len() * 4);
    let sep_width = usize::from(width.clamp(1, 120));
    let separator = Line::from(Span::styled("─".repeat(sep_width), theme::dim()));

    for (idx, msg) in messages.iter().enumerate() {
        if idx > 0 {
            out.push(Line::from(""));
            out.push(separator.clone());
        }
        out.extend(render_message(msg));
    }
    out
}

/// Convert one message JSON into styled lines: a role banner followed
/// by indented content. Tool calls collapse to a one-line preview.
fn render_message(msg: &serde_json::Value) -> Vec<Line<'_>> {
    let role = msg
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("system");
    let (label, label_color) = match role {
        "user" => ("user", theme::INFO),
        "assistant" => ("assistant", theme::ACCENT),
        other => (other, theme::DIM),
    };

    let mut out = vec![Line::from(Span::styled(
        format!("┃ {label}"),
        Style::default()
            .fg(label_color)
            .add_modifier(Modifier::BOLD),
    ))];
    out.push(Line::from(""));
    for line in content_lines(msg) {
        out.push(indent_line(line));
    }
    out
}

/// Prepend two spaces to a line so message content sits indented under
/// its role banner.
fn indent_line(line: Line<'_>) -> Line<'_> {
    let mut spans: Vec<Span<'_>> = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw("  "));
    spans.extend(line.spans);
    Line::from(spans)
}

fn content_lines(msg: &serde_json::Value) -> Vec<Line<'_>> {
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
                    let preview = tool_input_preview(item.get("input"));
                    lines.push(Line::from(vec![
                        Span::styled("▼ ", theme::dim()),
                        Span::styled(
                            name.to_string(),
                            Style::default()
                                .fg(theme::INFO)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("  {preview}"), theme::dim()),
                    ]));
                }
                "tool_result" => {
                    let snippet = item
                        .get("content")
                        .and_then(value_as_text)
                        .map_or_else(
                            || "(empty)".to_string(),
                            |s| truncate_one_line(&s, 80),
                        );
                    lines.push(Line::from(vec![
                        Span::styled("⤷ ", theme::dim()),
                        Span::styled(snippet, theme::dim()),
                    ]));
                }
                "thinking" => {
                    lines.push(Line::from(Span::styled(
                        "◇ thinking",
                        Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
                    )));
                }
                other => {
                    lines.push(Line::from(Span::styled(
                        format!("[{other}]"),
                        theme::dim(),
                    )));
                }
            }
        }
        return lines;
    }

    vec![Line::from(Span::styled(content.to_string(), theme::dim()))]
}

fn value_as_text(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = v.as_array() {
        let mut buf = String::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                buf.push_str(t);
                buf.push(' ');
            }
        }
        if buf.is_empty() {
            return None;
        }
        return Some(buf);
    }
    None
}

fn tool_input_preview(input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let s = input.to_string();
    truncate_one_line(&s, 80)
}

fn truncate_one_line(s: &str, max: usize) -> String {
    let one_line: String = s
        .chars()
        .take_while(|&c| c != '\n')
        .take(max + 1)
        .collect();
    if one_line.chars().count() > max {
        let mut t: String = one_line.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        one_line
    }
}
