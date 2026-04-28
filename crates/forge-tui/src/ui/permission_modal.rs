//! Permission modal — overlay above any screen when a permission
//! request is pending.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::PendingPermission;
use crate::ui::theme;

/// Render the permission modal. Centred overlay; clears its area
/// before drawing so it sits cleanly above the screen below.
pub fn render(frame: &mut Frame<'_>, p: &PendingPermission) {
    let area = frame.area();

    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(10),
            Constraint::Min(1),
        ])
        .split(area);

    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(64),
            Constraint::Min(1),
        ])
        .split(v[1]);

    let modal_area = h[1];

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ACCENT))
        .title(" ⚡ Permission request ");
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    let tool_name = p
        .params
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let tool_input = p
        .params
        .get("tool_input")
        .map_or_else(|| "{}".into(), ToString::to_string);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Tool:    ", theme::dim()),
            Span::styled(tool_name.to_string(), Style::default()),
        ]),
        Line::from(vec![
            Span::styled("  Input:   ", theme::dim()),
            Span::styled(truncate(&tool_input, 56), Style::default()),
        ]),
        Line::from(""),
        Line::from(""),
        Line::from(Span::styled(
            "  [a] Accept    [d] Deny    [Esc] cancel",
            theme::dim(),
        )),
    ];

    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
