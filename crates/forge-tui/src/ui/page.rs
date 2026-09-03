//! Shared full-screen page scaffold for the standalone views
//! (`/mcp`, `/plugins`, `/usage`, `/diff`): a titled bordered box with a
//! full-width body, an optional status row, and a footer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::theme;

/// Draw the full-screen chrome and hand the body rect to `body`. The
/// body spans the full inner width; `status` renders on its own row
/// above the footer when present.
pub fn render_page(
    frame: &mut Frame,
    title: &str,
    status: Option<Line<'static>>,
    footer: Line<'static>,
    body: impl FnOnce(&mut Frame, Rect),
) {
    let area = frame.area();
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(title.to_owned())
        .border_style(Style::default().fg(theme::DIM));
    frame.render_widget(outer, area);

    let inner = area.inner(Margin { vertical: 1, horizontal: 1 });
    let chunks = if status.is_some() {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
            .split(inner)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner)
    };

    body(frame, chunks[0]);

    if let Some(status) = status {
        frame.render_widget(Paragraph::new(status), chunks[1]);
        frame.render_widget(Paragraph::new(footer), chunks[2]);
    } else {
        frame.render_widget(Paragraph::new(footer), chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use std::cell::Cell;

    fn buffer_text(buffer: &Buffer) -> String {
        let width = usize::from(buffer.area.width);
        buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(ratatui::buffer::Cell::symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_page_draws_titled_box_full_width_body_and_footer() {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        let captured: Cell<Rect> = Cell::new(Rect::default());
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_page(
                    frame,
                    "Test Page",
                    None,
                    Line::from(Span::raw("footer hint")),
                    |frame, body| {
                        captured.set(body);
                        frame.render_widget(Paragraph::new("body content"), body);
                    },
                );
                assert_eq!(captured.get().width, area.width - 2, "body spans the inner width");
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("Test Page"), "titled box: {rendered}");
        assert!(rendered.contains("body content"), "body: {rendered}");
        assert!(rendered.contains("footer hint"), "footer: {rendered}");
    }

    #[test]
    fn render_page_with_status_row_renders_status_above_footer() {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                render_page(
                    frame,
                    "Cfg",
                    Some(Line::from(Span::raw("status here"))),
                    Line::from(Span::raw("help there")),
                    |frame, body| frame.render_widget(Paragraph::new("cfg body"), body),
                );
            })
            .expect("draw");

        let rendered = buffer_text(terminal.backend().buffer());
        assert!(rendered.contains("status here"), "status: {rendered}");
        assert!(rendered.contains("help there"), "footer: {rendered}");
        // Both present is not the claim - the status row sits ABOVE the
        // footer, which a same-row swap would still satisfy.
        let status_row =
            rendered.lines().position(|l| l.contains("status here")).expect("status rendered");
        let footer_row =
            rendered.lines().position(|l| l.contains("help there")).expect("footer rendered");
        assert!(status_row < footer_row, "status must render above the footer");
    }
}
