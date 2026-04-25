//! Permission-request modal overlay.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::PendingPermission;

/// Render the permission modal centred on `frame.area()`.
pub fn render(frame: &mut Frame<'_>, p: &PendingPermission) {
    let area = centered(60, 40, frame.area());
    frame.render_widget(Clear, area);

    let tool = p
        .params
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let input = serde_json::to_string_pretty(
        p.params
            .get("tool_input")
            .unwrap_or(&serde_json::Value::Null),
    )
    .unwrap_or_default();
    // Footer first so the keybind hint stays visible when the input is
    // long enough to push past the modal height.
    let body = format!("[a] Allow  [d] Deny  [Esc] Dismiss\n\nTool: {tool}\n\n{input}");

    let para = Paragraph::new(body)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Permission Request "),
        )
        .style(Style::default().fg(Color::Yellow));
    frame.render_widget(para, area);
}

fn centered(w_pct: u16, h_pct: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - h_pct) / 2),
            Constraint::Percentage(h_pct),
            Constraint::Percentage((100 - h_pct) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - w_pct) / 2),
            Constraint::Percentage(w_pct),
            Constraint::Percentage((100 - w_pct) / 2),
        ])
        .split(v[1])[1]
}
