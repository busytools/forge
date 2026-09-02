//! `/model` picker overlay render.
//!
//! A centered modal listing the session's available models (the curated
//! OpenRouter catalog on an `openrouter` account, the CLI-advertised
//! regular models elsewhere), each row showing the display name and its
//! DIM description, the highlighted row accented and the running model
//! dotted, plus a key-hints footer. State + key handling live in
//! [`crate::app::model_picker`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::ui::theme;

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.model_picker.as_ref() else {
        return;
    };

    // One row per model + a blank + a footer hint, inside a 1-cell border.
    let inner_height = u16::try_from(state.rows.len()).unwrap_or(0).saturating_add(2);
    let overlay = centered(area, 76, inner_height.saturating_add(2));

    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RUST_ORANGE))
        .title(" model ");
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);

    let running_id = app
        .current_model()
        .map(|model| model.requested_id.as_deref().unwrap_or(model.resolved_id.as_str()));
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(state.rows.len() + 2);
    for (idx, row) in state.rows.iter().enumerate() {
        let selected = idx == state.highlight;
        let marker = if selected { "\u{25B6}" } else { " " };
        // Case-insensitive like the other three id comparisons - row
        // filter, switch membership, highlight seeding.
        let running = running_id.is_some_and(|id| id.eq_ignore_ascii_case(&row.id));
        let row_style = if selected {
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let desc_style = if selected { row_style } else { Style::default().fg(theme::DIM) };
        let mut spans = vec![
            Span::styled(format!("{marker} "), Style::default().fg(theme::RUST_ORANGE)),
            Span::styled(format!("{} ", row.display_name), row_style),
        ];
        if running {
            spans.push(Span::styled("\u{25CF}", Style::default().fg(theme::RUST_ORANGE)));
        }
        if let Some(description) = row.description.as_deref().filter(|d| !d.is_empty()) {
            spans.push(Span::styled(format!("  {description}"), desc_style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "\u{2191}\u{2193} move   enter switch   esc cancel   \u{25CF} current",
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
