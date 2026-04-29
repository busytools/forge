//! Footer status bar — connection glyph, session id, role, cwd.
//!
//! Single horizontal line at the bottom of every screen. Shape per the
//! visual gallery:
//! `● daemon@127.0.0.1:7373 │ session: 33f4137f │ primary │ ~/forge`

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, ConnectionState, Role, Screen};
use crate::ui::theme;

/// Render the footer into `area` (1 line tall).
pub fn render(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let (glyph, glyph_color) = connection_glyph(app.connection);
    let separator = Span::styled(" │ ", crate::ui::style::dim());

    let mut spans: Vec<Span<'_>> = Vec::with_capacity(8);
    spans.push(Span::styled(format!("{glyph} "), Style::default().fg(glyph_color)));
    spans.push(Span::styled(daemon_label(app), crate::ui::style::text()));
    spans.push(separator.clone());

    spans.push(session_span(app));
    spans.push(separator.clone());

    spans.push(role_span(app));
    spans.push(separator.clone());

    spans.push(Span::styled(short_cwd(&app.cwd), crate::ui::style::dim()));

    if !app.status_msg.is_empty() {
        spans.push(separator);
        spans.push(Span::styled(app.status_msg.as_str(), crate::ui::style::dim()));
    }

    let para = Paragraph::new(Line::from(spans));
    frame.render_widget(para, area);
}

fn connection_glyph(state: ConnectionState) -> (&'static str, ratatui::style::Color) {
    match state {
        ConnectionState::Connected => ("●", ratatui::style::Color::Green),
        ConnectionState::Connecting | ConnectionState::Reconnecting { .. } => ("◌", theme::STATUS_WARNING),
        ConnectionState::Disconnected => ("✗", theme::STATUS_ERROR),
    }
}

fn daemon_label(app: &App) -> String {
    if app.daemon_url.is_empty() {
        "daemon".into()
    } else {
        format!("daemon@{}", strip_ws_scheme(&app.daemon_url))
    }
}

fn strip_ws_scheme(url: &str) -> &str {
    url.strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .unwrap_or(url)
        .trim_end_matches('/')
}

fn session_span(app: &App) -> Span<'_> {
    match (&app.active_view, &app.current_session) {
        (Screen::Chat, Some(sid)) => {
            Span::styled(format!("session: {}", short_sid(sid)), crate::ui::style::text())
        }
        _ => Span::styled("session: ─", crate::ui::style::dim()),
    }
}

fn role_span(app: &App) -> Span<'_> {
    match (&app.active_view, app.role) {
        (Screen::Chat, Role::Primary) => {
            Span::styled("primary", Style::default().fg(ratatui::style::Color::Green))
        }
        (Screen::Chat, Role::Viewer) => {
            Span::styled("viewer", Style::default().fg(theme::SUBAGENT_TOKEN))
        }
        _ => Span::styled("─", crate::ui::style::dim()),
    }
}

fn short_sid(sid: &str) -> String {
    sid.split('-').next().unwrap_or(sid).to_string()
}

fn short_cwd(cwd: &str) -> String {
    if cwd.is_empty() {
        return "─".into();
    }
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = cwd.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    cwd.to_string()
}
