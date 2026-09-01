//! `/dictate` overlay render.
//!
//! A centered modal in the `/account` picker's chrome: 62 columns,
//! `Borders::ALL` in RUST_ORANGE, a header line with a right-justified
//! note, dim group headers that never take a highlight index, and a
//! footer of key hints. The `●` marks what this session has
//! overridden, not the value in force, and the dialog reads no state
//! back: it never displays what a value is, only what can be chosen.
//! State + key handling live in [`crate::app::dictate_picker`].

use forge_workspace::DictateOverrides;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::app::dictate_picker::{self, PickerRow};
use crate::ui::theme;

const WIDTH: u16 = 62;

/// The three lines above the key hints. Without them, choosing "may
/// bullet a list", dictating three items and getting a sentence back
/// is indistinguishable from a broken setting: the axes are
/// permissions, and the model may decline.
const NOTE: [&str; 3] = [
    "These let the cleanup model do something. They do not",
    "promise it will: text that comes back unchanged means",
    "it declined, not that the setting failed.",
];

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.dictate_picker.as_ref() else {
        return;
    };
    let overrides: DictateOverrides =
        app.active_session().map(|s| s.dictate_overrides).unwrap_or_default();
    let rows = dictate_picker::rows(overrides);
    let highlight = state.highlight.min(rows.len().saturating_sub(1));

    // header + blank + (group header + rows) x3 + inter-group blanks
    // + reset + blank + 3-line note + blank + footer, inside a border.
    let body_lines = 2 + 5 + 1 + 3 + 1 + 3 + 1 + 1 + 1 + 3 + 1 + 1;
    let height = u16::try_from(body_lines).unwrap_or(0).saturating_add(2);
    let overlay = centered(area, WIDTH, height);

    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RUST_ORANGE));
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);
    let inner_w = usize::from(inner.width);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(body_lines);
    lines.push(padded_line(
        vec![Span::styled(
            "Dictate",
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        )],
        "Dictate".len(),
        Span::styled("this session only", Style::default().fg(theme::DIM)),
        "this session only".len(),
        inner_w,
    ));
    lines.push(Line::default());

    let mut group_drawn = "";
    for (idx, row) in rows.iter().enumerate() {
        // The reset row carries no group: it sits under a blank line
        // below the last group, like the mock draws it.
        if !row.group.is_empty() && row.group != group_drawn {
            group_drawn = row.group;
            if idx > 0 {
                lines.push(Line::default());
            }
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(row.group, Style::default().fg(theme::DIM)),
            ]));
        }
        if row.group.is_empty() {
            lines.push(Line::default());
        }
        lines.push(row_line(row, idx == highlight));
    }

    lines.push(Line::default());
    for note in NOTE {
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(note.to_owned(), Style::default().fg(theme::DIM)),
        ]));
    }
    lines.push(Line::default());
    let any = !overrides.is_empty();
    let mut footer = String::from("\u{2191}\u{2193} move   enter set   esc close");
    if any {
        footer.push_str("   \u{25CF} set this session");
    }
    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled(footer, Style::default().fg(theme::DIM)),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
}

/// One dialog row: a `●` when this session set this value, then either
/// the highlight cursor or the plain label. The marker is the same
/// colour as the `/account` picker's: same glyph, same purpose. The
/// inert reset row draws DIM like a group header.
fn row_line(row: &PickerRow, selected: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if row.marker {
        spans.push(Span::styled("\u{25CF} ", Style::default().fg(theme::RUST_ORANGE)));
    } else {
        spans.push(Span::raw("  "));
    }
    if selected {
        spans.push(Span::styled("\u{25B8} ", Style::default().fg(theme::RUST_ORANGE)));
        spans.push(Span::styled(
            row.label.to_owned(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw("  "));
        let style = if row.selectable {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(theme::DIM)
        };
        spans.push(Span::styled(row.label.to_owned(), style));
    }
    Line::from(spans)
}

fn padded_line(
    mut left: Vec<Span<'static>>,
    left_w: usize,
    right: Span<'static>,
    right_w: usize,
    inner_w: usize,
) -> Line<'static> {
    let pad = inner_w.saturating_sub(left_w + right_w);
    if pad > 0 {
        left.push(Span::raw(" ".repeat(pad)));
    }
    left.push(right);
    Line::from(left)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use forge_workspace::Context;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_overlay(app: &App, w: u16, h: u16) -> Vec<String> {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| render(frame, frame.area(), app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn the_dialog_draws_choices_and_hints_but_no_state_readout() {
        let mut app = App::test_default();
        crate::app::dictate_picker::open(&mut app);

        let lines = render_overlay(&app, 80, 30);
        let joined: String = lines.join("\n");
        for fragment in
            ["Dictate", "this session only", "VOICE", "STRUCTURE", "DESTINATION", "enter set"]
        {
            assert!(joined.contains(fragment), "{fragment} is drawn: {joined}");
        }
        assert!(
            !joined.contains("\u{25CF} set this session"),
            "a fresh session shows no legend: {joined}"
        );
    }

    #[test]
    fn markers_appear_only_for_overridden_axes_and_the_legend_names_them() {
        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_overrides =
            DictateOverrides { context: Some(Context::Email), ..Default::default() };
        crate::app::dictate_picker::open(&mut app);

        let lines = render_overlay(&app, 80, 30);
        let email_row = lines.iter().find(|l| l.contains("email layout")).expect("row");
        assert!(email_row.contains('\u{25CF}'), "the overridden row is marked: {email_row}");
        let prose_row = lines.iter().find(|l| l.contains("may bullet a list")).expect("row");
        assert!(!prose_row.contains('\u{25CF}'), "an unset row is unmarked: {prose_row}");
        let footer = lines.iter().find(|l| l.contains("enter set")).expect("footer");
        assert!(
            footer.contains("\u{25CF} set this session"),
            "the legend names the marker: {footer}"
        );
    }

    #[test]
    fn the_inert_reset_row_draws_dim_and_the_note_warns_about_permissions() {
        let mut app = App::test_default();
        crate::app::dictate_picker::open(&mut app);
        let lines = render_overlay(&app, 80, 30);
        let reset = lines.iter().find(|l| l.contains("Reset all to defaults")).expect("reset");
        assert!(reset.contains("Reset all to defaults"), "reset is drawn: {reset}");
        let joined: String = lines.join("\n");
        assert!(
            joined.contains("it declined, not that the setting failed."),
            "the permission note is drawn: {joined}"
        );
    }
}
