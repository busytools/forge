use crate::app::input::InputState;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub(super) fn text_input_line(draft: &str, cursor: usize, placeholder: &str) -> Line<'static> {
    let cursor_style =
        Style::default().fg(Color::Black).bg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(Color::White);
    let placeholder_style = Style::default().fg(theme::DIM);

    if draft.is_empty() {
        return Line::from(vec![
            Span::styled(" ".to_owned(), cursor_style),
            Span::styled(placeholder.to_owned(), placeholder_style),
        ]);
    }

    let cursor = cursor.min(draft.chars().count());
    let chars = draft.chars().collect::<Vec<_>>();
    let prefix = chars[..cursor].iter().collect::<String>();
    let mut spans = Vec::new();

    if !prefix.is_empty() {
        spans.push(Span::styled(prefix, text_style));
    }

    if cursor < chars.len() {
        spans.push(Span::styled(chars[cursor].to_string(), cursor_style));
        let suffix = chars[cursor + 1..].iter().collect::<String>();
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix, text_style));
        }
    } else {
        spans.push(Span::styled(" ".to_owned(), cursor_style));
    }

    Line::from(spans)
}

pub(super) fn render_text_input_field(
    frame: &mut Frame,
    area: Rect,
    editor: &InputState,
    placeholder: &str,
    blip: Option<&ratatui::text::Span<'static>>,
) {
    let mut content = Vec::new();
    if let Some(blip) = blip {
        content.push(blip.clone());
    }
    content.extend(text_input_line(&editor.text(), editor.cursor_char_offset(), placeholder).spans);
    frame.render_widget(
        Paragraph::new(crate::ui::composer::single_line_field(
            Line::from(content),
            usize::from(area.width),
            crate::ui::composer::border_style(),
        )),
        area,
    );
}

pub(super) fn add_marketplace_example_lines() -> Vec<Line<'static>> {
    let dim = Style::default().fg(theme::DIM);
    vec![
        Line::from(Span::styled("Examples:", dim.add_modifier(Modifier::BOLD))),
        Line::from(Span::styled("  - owner/repo (GitHub)", dim)),
        Line::from(Span::styled("  - git@github.com:owner/repo.git (SSH)", dim)),
        Line::from(Span::styled("  - https://example.com/marketplace.json", dim)),
        Line::from(Span::styled("  - ./path/to/marketplace", dim)),
    ]
}
