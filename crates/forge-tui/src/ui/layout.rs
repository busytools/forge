//! Render the three-panel layout: session list (left), conversation
//! (centre), status bar (bottom). Permission modal overlays when
//! present.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::app::{App, Focus, Role};

/// Top-level renderer — layout three panels then maybe overlay the modal.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(0)])
        .split(outer[0]);

    // Session list.
    let items: Vec<ListItem> = app
        .session_list
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let title = s
                .get("custom_title")
                .and_then(|v| v.as_str())
                .or_else(|| s.get("summary").and_then(|v| v.as_str()))
                .unwrap_or("(no title)");
            let item = ListItem::new(title.to_string());
            if idx == app.session_list_cursor && app.focus == Focus::SessionList {
                item.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                item
            }
        })
        .collect();
    let list_title = match app.focus {
        Focus::SessionList => " Sessions [active] ",
        _ => " Sessions ",
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(list_title.to_string()),
    );
    frame.render_widget(list, main[0]);

    // Conversation.
    let convo: Vec<ratatui::text::Line> = app
        .messages
        .iter()
        .map(crate::ui::conversation::render_message)
        .collect();
    let convo_title = match (app.focus, &app.current_session) {
        (Focus::Conversation, Some(sid)) => format!(" Conversation — {sid} "),
        (_, Some(sid)) => format!(" Conversation ({sid}) "),
        _ => " Conversation ".into(),
    };
    let para = Paragraph::new(convo)
        .block(Block::default().borders(Borders::ALL).title(convo_title))
        .wrap(ratatui::widgets::Wrap { trim: false });
    frame.render_widget(para, main[1]);

    // Status bar.
    let role = match app.role {
        Role::Primary => "* primary",
        Role::Viewer => "o viewer (P to claim)",
        Role::Vacant => "- no session",
    };
    let status_text = if app.status_msg.is_empty() {
        format!("{role}  |  q quit")
    } else {
        format!("{role}  |  {}", app.status_msg)
    };
    let status = Paragraph::new(status_text).style(Style::default().fg(Color::Gray));
    frame.render_widget(status, outer[1]);

    // Permission modal overlay.
    if let Some(p) = &app.pending_permission {
        if app.focus == Focus::PermissionModal {
            crate::ui::permission_modal::render(frame, p);
        }
    }
}
