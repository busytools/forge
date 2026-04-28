//! Connecting screen — shown until the WS handshake completes and the
//! initial `sessions.list` arrives.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, ConnectionState};
use crate::ui::footer;
use crate::ui::theme;

/// Render the connecting screen into the full frame.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();

    // Vertical: body fills, footer = 1 line.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let body = chunks[0];
    let footer_area = chunks[1];

    // Centre a small block of text in `body`.
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(7),
            Constraint::Min(1),
        ])
        .split(body);

    let title = Paragraph::new(Line::from(Span::styled("forge", theme::heading())))
        .alignment(Alignment::Center);

    let url_line = Paragraph::new(Line::from(vec![
        Span::styled("connecting to ", theme::dim()),
        Span::styled(app.daemon_url.as_str(), Style::default()),
    ]))
    .alignment(Alignment::Center);

    let attempt_line = Paragraph::new(Line::from(Span::styled(
        attempt_text(app.connection),
        theme::dim(),
    )))
    .alignment(Alignment::Center);

    let centre = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(v[1]);

    frame.render_widget(title, centre[0]);
    frame.render_widget(url_line, centre[2]);
    frame.render_widget(attempt_line, centre[3]);

    footer::render(frame, app, footer_area);
}

fn attempt_text(state: ConnectionState) -> String {
    match state {
        ConnectionState::Connecting => "◌  connecting…".into(),
        ConnectionState::Connected => "●  connected".into(),
        ConnectionState::Reconnecting { next_retry_secs } => {
            format!("◌  retrying in {next_retry_secs}s…")
        }
        ConnectionState::Disconnected => "✗  disconnected".into(),
    }
}
