//! Conversation screen — chat + input + help + footer, matching the
//! claude-code-rust visual layout (no top header; chat fills body;
//! 2-col horizontal padding; `❯` prompt char; tool-icon glyphs).
//!
//! Body scrolls; PgUp/PgDn/Home/End + mouse wheel drive offset.
//! Auto-tails new messages while the user hasn't scrolled up.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::App;
use crate::ui::footer;
use crate::ui::theme;

/// 2-column horizontal pad applied to body, input, and help.
const PAD_X: u16 = 2;

/// Render the conversation screen.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // body (chat)
            Constraint::Length(1), // input separator
            Constraint::Length(1), // input
            Constraint::Length(1), // input bottom separator
            Constraint::Length(1), // help
            Constraint::Length(1), // footer
        ])
        .split(area);

    render_body(frame, app, padded(chunks[0]));
    render_separator(frame, chunks[1]);
    render_input(frame, app, padded(chunks[2]));
    render_separator(frame, chunks[3]);
    render_help(frame, padded(chunks[4]));
    footer::render(frame, app, chunks[5]);
}

fn padded(rect: Rect) -> Rect {
    rect.inner(Margin {
        horizontal: PAD_X,
        vertical: 0,
    })
}

fn render_separator(frame: &mut Frame<'_>, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let line = Line::from(Span::styled(
        "─".repeat(usize::from(area.width)),
        theme::dim(),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_body(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lines = build_message_lines(&app.messages);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "u16 truncation for terminal scroll offset; clamping happens below"
    )]
    let total = lines.len().min(u16::MAX as usize) as u16;
    let viewport = area.height;
    let max_scroll = total.saturating_sub(viewport);
    // `conv_scroll_back` is distance from live tail in lines.
    // Clamp to max_scroll (can't scroll past oldest). Compute the
    // actual top-line offset for `Paragraph::scroll`.
    let scroll_back = app.conv_scroll_back.min(max_scroll);
    let scroll = max_scroll - scroll_back;

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(para, area);
}

fn render_input(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = vec![
        Span::styled("❯ ", Style::default().fg(theme::ACCENT)),
        Span::styled(app.draft.as_str(), theme::text()),
        Span::styled("▏", Style::default().add_modifier(Modifier::SLOW_BLINK)),
    ];
    if app.draft.is_empty() {
        spans.insert(2, Span::styled("type a message…", theme::dim()));
    }
    let para = Paragraph::new(Line::from(spans)).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let scroll_hint = " ".to_string();
    let line = Line::from(vec![
        Span::styled("Enter", theme::heading()),
        Span::styled(" send  ", theme::dim()),
        Span::styled("PgUp/PgDn/wheel", theme::heading()),
        Span::styled(" scroll  ", theme::dim()),
        Span::styled("End", theme::heading()),
        Span::styled(" follow  ", theme::dim()),
        Span::styled("Esc", theme::heading()),
        Span::styled(" picker  ", theme::dim()),
        Span::styled("q", theme::heading()),
        Span::styled(" quit", theme::dim()),
        Span::styled(scroll_hint, theme::dim()),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Build the styled lines for the conversation body. Minimal chrome:
/// each turn is content prefixed with a thin colored bar in the role's
/// color; turns separated by a single blank line.
fn build_message_lines(messages: &[serde_json::Value]) -> Vec<Line<'_>> {
    let mut out: Vec<Line<'_>> = Vec::with_capacity(messages.len() * 4);

    for (idx, msg) in messages.iter().enumerate() {
        if idx > 0 {
            out.push(Line::from(""));
        }
        out.extend(render_message(msg));
    }
    out
}

/// Convert one message JSON into styled lines. Format:
/// `▎ <content>` per line, where `▎` is colored by role.
/// No banner, no bg tint — just a vertical bar to delimit turns.
fn render_message(msg: &serde_json::Value) -> Vec<Line<'_>> {
    let role = msg
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("system");
    let bar_color = match role {
        "user" => theme::INFO,
        "assistant" => theme::ACCENT,
        _ => theme::DIM,
    };
    let bar_style = Style::default().fg(bar_color);

    content_lines(msg)
        .into_iter()
        .map(|line| prefix_with_bar(line, bar_style))
        .collect()
}

/// Prepend a colored bar `▎` and a space so message content sits in a
/// visually-grouped column.
fn prefix_with_bar(line: Line<'_>, bar_style: Style) -> Line<'_> {
    let mut spans = vec![Span::styled("▎ ", bar_style)];
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
                    let glyph = theme::tool_glyph(name);
                    lines.push(Line::from(vec![
                        Span::styled(format!("{glyph} "), Style::default().fg(theme::INFO)),
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
                        Span::styled("  ⤷ ", theme::dim()),
                        Span::styled(snippet, theme::dim()),
                    ]));
                }
                "thinking" => {
                    lines.push(Line::from(Span::styled(
                        "◇ thinking",
                        Style::default()
                            .fg(theme::DIM)
                            .add_modifier(Modifier::ITALIC),
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
    let one_line: String = s.chars().take_while(|&c| c != '\n').take(max + 1).collect();
    if one_line.chars().count() > max {
        let mut t: String = one_line.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        one_line
    }
}
