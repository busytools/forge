//! `/spinner` picker overlay render.
//!
//! A centered modal listing every [`SpinnerStyle`] animating at its
//! own cadence (via [`App::spinner_glyph_for`]), each row showing the
//! live glyph + key + cadence, the highlighted row accented, plus a
//! key-hints footer. State + key handling live in
//! [`crate::app::spinner_picker`].

use forge_workspace::SpinnerStyle;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::ui::theme;

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.spinner_picker else {
        return;
    };

    let styles = SpinnerStyle::ALL_STYLES;
    // One row per style + a blank + a footer hint, inside a 1-cell border.
    let inner_height = u16::try_from(styles.len()).unwrap_or(0).saturating_add(2);
    let overlay = centered(area, 44, inner_height.saturating_add(2));

    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RUST_ORANGE))
        .title(" spinner ");
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(styles.len() + 2);
    for (idx, style) in styles.into_iter().enumerate() {
        let selected = idx == state.highlight;
        let glyph = app.spinner_glyph_for(style);
        let marker = if selected { "\u{25B6}" } else { " " };
        let row_style = if selected {
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let meta_style = if selected { row_style } else { Style::default().fg(theme::DIM) };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), Style::default().fg(theme::RUST_ORANGE)),
            Span::styled(format!("{glyph}  "), row_style),
            Span::styled(
                format!("{}  \u{00B7}  {}ms", style.key(), style.cadence_ms()),
                meta_style,
            ),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "\u{2191}\u{2193} preview   enter apply   esc cancel",
        Style::default().fg(theme::DIM),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Centre a `width` x `height` rect within `area`, clamped to fit.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}
