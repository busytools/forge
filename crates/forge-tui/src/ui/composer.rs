//! The shared composer chrome: thick orange border with an optional
//! embedded title, the prompt glyph, and the placeholder and cursor
//! styles. The chat composer is the template; every text surface
//! switches the capabilities it needs.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};
use unicode_width::UnicodeWidthStr;

use crate::ui::theme;

/// The capabilities a composer surface switches on. The chat composer
/// enables everything; the review editors and single-line fields drop
/// what they do not carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerChrome {
    /// Title embedded in the thick top border, spaced off the corner
    /// by one fill glyph.
    pub(crate) title: Option<String>,
    /// The prompt glyph at the draft's left edge.
    pub(crate) prompt_glyph: bool,
    /// DIM text shown while the draft is empty.
    pub(crate) placeholder: &'static str,
}

impl ComposerChrome {
    /// The chat composer's own chrome - the template.
    pub(crate) fn chat() -> Self {
        Self { title: None, prompt_glyph: true, placeholder: "Type a message..." }
    }

    /// The thick orange border every surface draws; `border_fg` is the
    /// resolved colour, so a live dictate take can hand its own over.
    pub(crate) fn border_block(&self, border_fg: Color) -> Block<'static> {
        let style = Style::default().fg(border_fg).add_modifier(Modifier::BOLD);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Thick)
            .border_style(style);
        match &self.title {
            Some(title) => block.title(Span::styled(format!("\u{2501} {title} "), style)),
            None => block,
        }
    }

    /// The prompt glyph line, when the surface carries it.
    pub(crate) fn prompt_line(&self) -> Option<Line<'static>> {
        self.prompt_glyph.then(Self::prompt_span_line)
    }

    /// The prompt glyph as one span, for the Line face's first row.
    pub(crate) fn prompt_span() -> Span<'static> {
        Span::styled(format!("{} ", theme::PROMPT_CHAR), Style::default().fg(theme::RUST_ORANGE))
    }

    fn prompt_span_line() -> Line<'static> {
        Line::from(Self::prompt_span())
    }

    /// The cursor style textarea-backed surfaces share.
    pub(crate) fn cursor_style() -> Style {
        Style::default().add_modifier(Modifier::REVERSED).add_modifier(Modifier::SLOW_BLINK)
    }

    /// The caret cell the Line face draws at the editor's caret.
    pub(crate) fn caret_span() -> Span<'static> {
        Span::styled(" ", Self::cursor_style())
    }

    /// The placeholder style textarea-backed surfaces share.
    pub(crate) fn placeholder_style() -> Style {
        Style::default().fg(theme::DIM)
    }
}

/// The orange bold style the thick border glyphs carry.
pub(crate) fn border_style() -> Style {
    Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
}

/// Top border row of the Line face, title embedded, padded to
/// `width` display columns.
pub(crate) fn top_border(title: &str, width: usize, style: Style) -> Line<'static> {
    border_row(format!("\u{250f}\u{2501} {title} "), width, style, "\u{2513}")
}

/// Bottom border row of the Line face.
pub(crate) fn bottom_border(width: usize, style: Style) -> Line<'static> {
    border_row("\u{2517}".to_owned(), width, style, "\u{251b}")
}

/// Wrap one content row in the side borders, padding to `width`
/// display columns. Content wider than the box is the caller's to
/// truncate; the pad simply empties.
pub(crate) fn side_bordered(row: Line<'static>, width: usize, style: Style) -> Line<'static> {
    let row_width: usize = row.spans.iter().map(|span| span.content.width()).sum();
    let pad = width.saturating_sub(row_width + 4);
    let mut spans = vec![Span::styled("\u{2503} ", style)];
    spans.extend(row.spans);
    spans.push(Span::styled(" ".repeat(pad), style));
    spans.push(Span::styled(" \u{2503}", style));
    Line::from(spans)
}

/// The single-line field face: the content embedded in a one-row thick
/// border, for the compact text fields. A leading blip rides inside
/// the border, left of the content.
pub(crate) fn single_line_field(
    content: Line<'static>,
    width: usize,
    style: Style,
) -> Line<'static> {
    let content_width: usize = content.spans.iter().map(|span| span.content.width()).sum();
    let fill = width.saturating_sub(content_width + 4);
    let mut spans = vec![Span::styled("\u{250f}\u{2501} ", style)];
    spans.extend(content.spans);
    spans.push(Span::styled("\u{2501}".repeat(fill), style));
    spans.push(Span::styled("\u{2513}", style));
    Line::from(spans)
}

fn border_row(head: String, width: usize, style: Style, corner: &str) -> Line<'static> {
    let fill = width.saturating_sub(head.width() + corner.width());
    let mut spans = Vec::new();
    if !head.is_empty() {
        spans.push(Span::styled(head, style));
    }
    spans.push(Span::styled("\u{2501}".repeat(fill), style));
    spans.push(Span::styled(corner.to_owned(), style));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn the_border_block_carries_thick_bold_corners_and_an_embedded_title() {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 3)).expect("terminal");
        let chrome = ComposerChrome { title: None, prompt_glyph: true, placeholder: "" };
        terminal
            .draw(|frame| {
                frame.render_widget(chrome.border_block(theme::RUST_ORANGE), frame.area());
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        assert_eq!(buffer.cell((0, 0)).expect("corner").symbol(), "\u{250f}");
        assert_eq!(buffer.cell((11, 0)).expect("corner").symbol(), "\u{2513}");
        assert_eq!(buffer.cell((0, 2)).expect("corner").symbol(), "\u{2517}");
        let corner = buffer.cell((0, 0)).expect("corner").style();
        assert_eq!(corner.fg, Some(theme::RUST_ORANGE), "the idle border is orange");
        assert!(corner.add_modifier.contains(Modifier::BOLD), "the border keeps its bold");

        let titled = ComposerChrome {
            title: Some("Comment on line 42".to_owned()),
            prompt_glyph: true,
            placeholder: "",
        };
        let mut wide =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 3)).expect("terminal");
        wide.draw(|frame| {
            frame.render_widget(titled.border_block(theme::RUST_ORANGE), frame.area());
        })
        .expect("draw");
        let buffer = wide.backend().buffer().clone();
        let top: String = (0..30)
            .map(|x| {
                buffer
                    .cell((x, 0))
                    .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    .to_owned()
            })
            .collect();
        assert_eq!(
            top,
            "\u{250f}\u{2501} Comment on line 42 \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2513}",
            "the title embeds after one fill glyph, got: {top}"
        );
    }

    #[test]
    fn the_prompt_line_is_the_orange_glyph_and_a_space() {
        let chrome = ComposerChrome::chat();
        let line = chrome.prompt_line().expect("the chat composer carries the glyph");
        assert_eq!(row_text(&line), format!("{} ", theme::PROMPT_CHAR));
        assert_eq!(line.spans[0].style.fg, Some(theme::RUST_ORANGE));
        assert!(
            ComposerChrome { title: None, prompt_glyph: false, placeholder: "" }
                .prompt_line()
                .is_none(),
            "a surface without the capability draws no glyph"
        );
    }
}
