//! Render a single conversation transcript line.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Format one `session.event.message` payload as a single-line preview.
#[must_use]
pub fn render_message(msg: &serde_json::Value) -> Line<'_> {
    let kind = msg.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    let prefix = match kind {
        "assistant" => "A",
        "user" => "U",
        "result" => "R",
        "error" => "!",
        _ => "-",
    };
    let style = match kind {
        "assistant" => Style::default().fg(Color::Cyan),
        "user" => Style::default().fg(Color::Green),
        "result" => Style::default().fg(Color::Yellow),
        "error" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::Gray),
    };
    let preview = msg
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .map_or_else(
            || {
                serde_json::to_string(msg)
                    .unwrap_or_default()
                    .chars()
                    .take(120)
                    .collect()
            },
            |s| s.lines().next().unwrap_or("").to_string(),
        );
    Line::from(vec![
        Span::styled(format!("[{prefix}] "), style),
        Span::raw(preview),
    ])
}
