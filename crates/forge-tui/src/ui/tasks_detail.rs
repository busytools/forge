//! The TASKS detail overlay: one task's whole record, opened by clicking
//! its row in the Inspector's TASKS section.
//!
//! Full width, centred vertically, drawn over whatever view rendered
//! below. `Esc` or a click outside the panel closes it, and the pane
//! keeps no selection state - the row's own click target is the only way
//! in.

use std::time::SystemTime;

use forge_primitives::tasks::{Task, TaskStatus};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::ui::theme;

/// The overlay's rect inside `area`: the full width, three quarters of
/// the height, centred - so the bands above and below it stay clickable,
/// and a click there is what "outside" means.
pub(crate) fn panel_rect(area: Rect) -> Rect {
    let height = u16::try_from(usize::from(area.height) * 3 / 4).unwrap_or(area.height);
    Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: area.width,
        height,
    }
}

/// Render the overlay for the task [`App::task_detail`] names, if any.
/// Reads the project's whole task set, so the subtask list can reach a
/// child the session's own scoped rows do not carry.
pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(task_id) = app.task_detail.as_ref() else {
        return;
    };
    let all = &app.forge_project_tasks;
    let Some(task) = all.iter().find(|t| t.id == *task_id) else {
        return;
    };

    let panel = panel_rect(area);
    if panel.width == 0 || panel.height == 0 {
        return;
    }
    frame.render_widget(Clear, panel);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::DIM))
        .title(" TASK ");
    let inner = block.inner(panel);
    frame.render_widget(block, panel);

    let running_glyph = app.active_spinner_glyph();
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(subject_line(task, running_glyph));
    lines.push(Line::default());
    lines.extend(identity_lines(task, all));
    if let Some(detail) = &task.detail {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("detail   ", Style::default().fg(theme::DIM)),
            Span::styled(detail.clone(), Style::default().fg(Color::Gray)),
        ]));
    }
    let children: Vec<&Task> = all.iter().filter(|t| t.parent.as_ref() == Some(&task.id)).collect();
    if !children.is_empty() {
        lines.push(Line::default());
        lines.push(label_line("subtasks", &children.len().to_string()));
        for child in children {
            lines.push(child_line(child, running_glyph));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("esc close", Style::default().fg(theme::DIM))));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The overlay's headline: the row's own text, then its status and
/// estimate.
fn subject_line(task: &Task, running_glyph: char) -> Line<'static> {
    let mut spans = vec![
        Span::styled(glyph(task.status, running_glyph), Style::default().fg(color(task.status))),
        Span::raw(" "),
        Span::styled(task.subject.clone(), Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  \u{00B7}  {}", status_text(task.status)),
            Style::default().fg(theme::DIM),
        ),
    ];
    if let Some(estimate) = &task.estimate {
        spans
            .push(Span::styled(format!("  \u{00B7}  {estimate}"), Style::default().fg(theme::DIM)));
    }
    Line::from(spans)
}

/// The identity block: one labelled row per field the record carries. A
/// field the task does not have is absent rather than blank, and a parent
/// that has left the store renders as absent rather than as a dangling
/// id.
fn identity_lines(task: &Task, all: &[Task]) -> Vec<Line<'static>> {
    let mut lines = vec![label_line(
        "owner",
        task.owner.as_ref().map_or("unclaimed", forge_workspace::SessionSlot::label),
    )];
    if let Some(parent) = task
        .parent
        .as_ref()
        .and_then(|id| all.iter().find(|p| p.id == *id && p.project_name == task.project_name))
    {
        lines.push(label_line("parent", &parent.subject));
    }
    if let Some(artifact) = &task.artifact {
        lines.push(label_line("artifact", artifact));
    }
    lines.push(label_line("created", &rfc3339(task.created_at)));
    lines.push(label_line("updated", &rfc3339(task.updated_at)));
    lines.push(label_line("status", status_text(task.status)));
    lines
}

/// One `label  value` row of the identity block.
fn label_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(theme::DIM)),
        Span::styled(value.to_owned(), Style::default().fg(Color::Gray)),
    ])
}

/// One subtask row, indented under its heading.
fn child_line(child: &Task, running_glyph: char) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(glyph(child.status, running_glyph), Style::default().fg(color(child.status))),
        Span::raw(" "),
        Span::styled(child.subject.clone(), Style::default().fg(Color::Gray)),
    ])
}

/// The status's row glyph in the section's language.
fn glyph(status: TaskStatus, running_glyph: char) -> String {
    match status {
        TaskStatus::Completed => "\u{2713}".to_owned(),
        TaskStatus::InProgress => running_glyph.to_string(),
        TaskStatus::Blocked | TaskStatus::Pending => "\u{25cb}".to_owned(),
    }
}

fn color(status: TaskStatus) -> Color {
    match status {
        TaskStatus::Completed => Color::Green,
        TaskStatus::InProgress => theme::RUST_ORANGE,
        TaskStatus::Blocked | TaskStatus::Pending => theme::DIM,
    }
}

fn status_text(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Completed => "completed",
    }
}

fn rfc3339(t: SystemTime) -> String {
    time::OffsetDateTime::from(t)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_owned())
}
