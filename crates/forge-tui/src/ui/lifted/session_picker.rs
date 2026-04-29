#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    clippy::disallowed_methods,
    clippy::while_let_loop,
    clippy::collapsible_if,
    reason = "lifted upstream from claude-code-rust"
)]

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::state::app::App;
use crate::state::types::RecentSessionInfo;
use crate::ui::theme;

/// Maximum number of recent sessions shown in the picker.
const MAX_PICKER_SESSIONS: usize = 10;

/// Number of session entries the picker will render. Capped by
/// `MAX_PICKER_SESSIONS`.
fn picker_session_count(app: &App) -> usize {
    app.recent_sessions.len().min(MAX_PICKER_SESSIONS)
}

/// Whether the picker is still loading. Returns false in forge until
/// the startup-bootstrap fields lift; the daemon already streams
/// recent sessions on connect, so the picker is usable immediately.
const fn startup_picker_is_loading(_app: &App) -> bool {
    false
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.cached_frame_area = area;

    let outer = Block::default()
        .borders(Borders::ALL)
        .title("Resume Session")
        .border_style(Style::default().fg(theme::DIM));
    frame.render_widget(outer, area);

    let inner = area.inner(Margin { vertical: 1, horizontal: 2 });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Select a session to resume",
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new("Recent sessions for this project directory.")
            .style(Style::default().fg(theme::DIM)),
        chunks[1],
    );

    if startup_picker_is_loading(app) {
        frame.render_widget(
            Paragraph::new("Loading recent sessions...")
                .style(Style::default().fg(theme::DIM))
                .wrap(Wrap { trim: false }),
            chunks[2],
        );
    } else if picker_session_count(app) == 0 {
        frame.render_widget(
            Paragraph::new("No recent sessions found for this directory.")
                .style(Style::default().fg(theme::DIM))
                .wrap(Wrap { trim: false }),
            chunks[2],
        );
    } else {
        render_session_list(frame, chunks[2], app);
    }

    frame.render_widget(
        Paragraph::new(footer_text(app)).style(Style::default().fg(theme::DIM)),
        chunks[3],
    );
}

fn render_session_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let lines_per_item = 2;
    let visible_count = usize::from((area.height / lines_per_item).max(1));
    let session_count = picker_session_count(app);
    let max_offset = session_count.saturating_sub(visible_count);
    app.session_picker.scroll_offset = app.session_picker.scroll_offset.min(max_offset);
    if app.session_picker.selected < app.session_picker.scroll_offset {
        app.session_picker.scroll_offset = app.session_picker.selected;
    }
    if app.session_picker.selected >= app.session_picker.scroll_offset + visible_count {
        app.session_picker.scroll_offset = app.session_picker.selected + 1 - visible_count;
    }

    let start = app.session_picker.scroll_offset;
    let end = (start + visible_count).min(session_count);
    let mut lines = Vec::with_capacity((end - start) * usize::from(lines_per_item));
    for (idx, session) in app.recent_sessions[start..end].iter().enumerate() {
        let selected = start + idx == app.session_picker.selected;
        let base_style = if selected {
            Style::default().fg(ratatui::style::Color::White).bg(theme::RUST_ORANGE)
        } else {
            Style::default()
        };
        let marker = if selected { ">" } else { " " };
        lines.push(Line::from(Span::styled(
            format!("{marker} {}", display_primary(session)),
            base_style.add_modifier(Modifier::BOLD),
        )));
        if start + idx + 1 < end {
            lines.push(Line::default());
        }
    }

    frame.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), area);
}

fn footer_text(app: &App) -> &'static str {
    if startup_picker_is_loading(app) {
        "Preparing session picker | Ctrl+Q to quit"
    } else {
        "Enter to resume | Esc to start new session | Ctrl+Q to quit"
    }
}

fn display_primary(session: &RecentSessionInfo) -> String {
    format!("{} - {}", format_relative_age(session.last_modified_ms), display_title(session))
}

fn display_title(session: &RecentSessionInfo) -> String {
    let title = session
        .custom_title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| session.first_prompt.as_deref().filter(|value| !value.trim().is_empty()))
        .or_else(|| {
            let summary = session.summary.trim();
            (!summary.is_empty()).then_some(summary)
        })
        .unwrap_or(&session.session_id);
    truncate(title, 60)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() { format!("{truncated}...") } else { truncated }
}

fn format_relative_age(last_modified_ms: u64) -> String {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let then_secs = last_modified_ms / 1_000;
    if then_secs == 0 || then_secs >= now_secs {
        return "just now".to_owned();
    }

    let delta = now_secs - then_secs;
    if delta < 60 {
        return format!("{delta}s ago");
    }
    if delta < 60 * 60 {
        return format!("{}m ago", delta / 60);
    }
    if delta < 24 * 60 * 60 {
        return format!("{}h ago", delta / (60 * 60));
    }
    let days = delta / (24 * 60 * 60);
    let hours = (delta / (60 * 60)) % 24;
    format!("{days}d {hours}h ago")
}

