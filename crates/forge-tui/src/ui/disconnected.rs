//! Disconnected overlay — full-screen interstitial when WS drops.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, ConnectionState};
use crate::ui::footer;
use crate::ui::theme;

/// Render the disconnected screen into the full frame.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let body = chunks[0];
    let footer_area = chunks[1];

    // Centre a 7-line message box in body.
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(7),
            Constraint::Min(1),
        ])
        .split(body);

    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(56),
            Constraint::Min(1),
        ])
        .split(v[1]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::STATUS_ERROR));

    let inner = block.inner(h[1]);
    frame.render_widget(block, h[1]);

    let lines = vec![
        Line::from(Span::styled(
            "✗  Lost connection to forge-daemon",
            Style::default().fg(theme::STATUS_ERROR),
        )),
        Line::from(""),
        Line::from(Span::styled(
            retry_text(app.connection),
            crate::ui::style::dim(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "[r] retry now    [q] quit",
            crate::ui::style::dim(),
        )),
    ];

    let para = Paragraph::new(lines).alignment(Alignment::Center);
    frame.render_widget(para, inner);

    footer::render(frame, app, footer_area);
}

fn retry_text(state: ConnectionState) -> String {
    match state {
        ConnectionState::Reconnecting { next_retry_secs } => {
            format!("Retrying in {next_retry_secs}s…")
        }
        ConnectionState::Disconnected => "Retries paused.".into(),
        ConnectionState::Connecting => "Reconnecting…".into(),
        ConnectionState::Connected => "Connected — returning…".into(),
    }
}
