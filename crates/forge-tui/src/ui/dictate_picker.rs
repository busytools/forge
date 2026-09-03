//! `/dictate` overlay render.
//!
//! A centered modal in the `/account` picker's chrome: 62 columns,
//! `Borders::ALL` in RUST_ORANGE, a header line with a right-justified
//! note, dim group headers that never take a highlight index, and a
//! footer of key hints. The `●` marks the value in force - the session
//! override where one is set, the default otherwise - and the INPUT
//! DEVICE row's right-justified tag names where that value came from.
//! State + key handling live in [`crate::app::dictate_picker`].

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::app::dictate_picker::{self, DeviceList, PickerMode, PickerRow, TagTone};
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

/// The pick-mode note: the pin follows the id, so an unplugged device
/// fails the take rather than quietly moving it, and a pick is a
/// session state, not a config edit.
const DEVICE_NOTE: [&str; 3] = [
    "A pin follows the device id: unplugging it fails the take",
    "instead of quietly recording on another input.",
    "A pick lasts this session; forge.toml keeps the default.",
];

/// The note that replaces it while the pin names a device the list
/// could not find: nothing will record until another row is chosen.
const STALE_PIN_NOTE: [&str; 2] = [
    "Nothing is in force: the pinned device is absent, so",
    "takes fail until another row is chosen.",
];

const NO_DEVICES: &str = "No input devices found.";
const NO_DEVICES_HINTS: [&str; 2] =
    ["Check that a microphone is attached", "and permitted for this terminal."];

pub(crate) fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(state) = app.dictate_picker.as_ref() else {
        return;
    };
    let lines = match state.mode {
        PickerMode::Options => options_lines(app, state.highlight),
        PickerMode::Devices => devices_lines(app, state.devices_highlight),
    };
    let height = u16::try_from(lines.len()).unwrap_or(0).saturating_add(2);
    let overlay = centered(area, WIDTH, height);

    frame.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::RUST_ORANGE));
    let inner = block.inner(overlay);
    frame.render_widget(block, overlay);
    let inner_w = usize::from(inner.width);

    let lines: Vec<Line<'static>> = lines.into_iter().map(|line| fit_line(line, inner_w)).collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn options_lines(app: &App, highlight: usize) -> Vec<Line<'static>> {
    let rows = dictate_picker::rows(app);
    let highlight = highlight.min(rows.len().saturating_sub(1));
    let mut lines = vec![header_line("Dictate", "this session only"), Line::default()];

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
                Span::styled(row.group.to_owned(), Style::default().fg(theme::DIM)),
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
    lines.push(footer_line(" \u{2191}\u{2193} move   enter set   esc close   \u{25cf} in force"));
    lines
}

fn devices_lines(app: &App, highlight: usize) -> Vec<Line<'static>> {
    let mut lines = vec![header_line("Input device", "reverts on restart"), Line::default()];
    match dictate_picker::device_list(app) {
        DeviceList::Rows(rows) => {
            let highlight = highlight.min(rows.len().saturating_sub(1));
            for (idx, row) in rows.iter().enumerate() {
                lines.push(device_row_line(row, idx == highlight));
            }
            lines.push(Line::default());
            let stale = rows.iter().any(|row| !row.selectable);
            let notes: &[&str] = if stale { &STALE_PIN_NOTE } else { &DEVICE_NOTE };
            let note_style = if stale {
                Style::default().fg(theme::STATUS_ERROR)
            } else {
                Style::default().fg(theme::DIM)
            };
            for note in notes {
                lines.push(Line::from(vec![
                    Span::raw(" "),
                    Span::styled((*note).to_owned(), note_style),
                ]));
            }
            lines.push(Line::default());
            let footer = if rows.iter().any(|row| row.in_force) {
                " \u{2191}\u{2193} move   enter select   esc back   \u{25cf} in force"
            } else {
                " \u{2191}\u{2193} move   enter select   esc back"
            };
            lines.push(footer_line(footer));
        }
        DeviceList::Listing => {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("listing input devices", Style::default().fg(theme::DIM)),
            ]));
            lines.push(Line::default());
            lines.push(footer_line(" esc back"));
        }
        DeviceList::NoDevices | DeviceList::Failed(_) => {
            let first = match dictate_picker::device_list(app) {
                DeviceList::Failed(error) => error,
                _ => NO_DEVICES.to_owned(),
            };
            lines.push(Line::from(vec![Span::raw("   "), Span::raw(first)]));
            for hint in NO_DEVICES_HINTS {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    Span::styled(hint.to_owned(), Style::default().fg(theme::DIM)),
                ]));
            }
            lines.push(Line::default());
            lines.push(footer_line(" esc back"));
        }
    }
    lines
}

fn header_line(title: &str, note: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            title.to_owned(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{note:>width$}", width = 60usize.saturating_sub(title.len())),
            Style::default().fg(theme::DIM),
        ),
    ])
}

fn footer_line(text: &str) -> Line<'static> {
    Line::from(vec![Span::styled(text.to_owned(), Style::default().fg(theme::DIM))])
}

fn tag_style(tone: TagTone) -> Style {
    match tone {
        TagTone::Dim => Style::default().fg(theme::DIM),
        TagTone::Accent => Style::default().fg(theme::RUST_ORANGE),
        TagTone::Error => Style::default().fg(theme::STATUS_ERROR),
    }
}

/// One options dialog row: a `●` on the value in force, then either
/// the highlight cursor or the plain label, the "· this session"
/// suffix when the session set it, and the INPUT DEVICE row's tag
/// right-justified to the dialog's edge.
fn row_line(row: &PickerRow, selected: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if row.marker {
        spans.push(Span::styled("\u{25cf} ", Style::default().fg(theme::RUST_ORANGE)));
    } else {
        spans.push(Span::raw("  "));
    }
    if selected {
        spans.push(Span::styled("\u{25b8} ", Style::default().fg(theme::RUST_ORANGE)));
        spans.push(Span::styled(
            row.label.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::raw("  "));
        let style =
            if row.selectable { Style::default().fg(Color::White) } else { Style::default().fg(theme::DIM) };
        spans.push(Span::styled(row.label.clone(), style));
    }
    if row.session_set {
        spans.push(Span::styled("  \u{b7} this session", Style::default().fg(theme::DIM)));
    }
    if let Some((text, tone)) = &row.tag {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(text.clone(), tag_style(*tone)));
    }
    Line::from(spans)
}

/// One pick-mode row: the `●` on the device in force, the highlight
/// cursor, the label, and the state tag inline after a gap. A tag on
/// a non-selectable row draws dim by construction (the stale pin).
fn device_row_line(row: &dictate_picker::DeviceRow, selected: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if row.in_force {
        spans.push(Span::styled("\u{25cf} ", Style::default().fg(theme::RUST_ORANGE)));
    } else {
        spans.push(Span::raw("  "));
    }
    if selected {
        spans.push(Span::styled("\u{25b8} ", Style::default().fg(theme::RUST_ORANGE)));
        spans.push(Span::styled(
            row.label.clone(),
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ));
    } else if row.selectable {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(row.label.clone(), Style::default().fg(Color::White)));
    } else {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(row.label.clone(), Style::default().fg(theme::DIM)));
    }
    if let Some((text, tone)) = &row.tag {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(text.clone(), tag_style(*tone)));
    }
    Line::from(spans)
}

/// Trim any row that overflows the dialog: device names are
/// unbounded, the 62-column chrome is not. The overflow comes off the
/// longest span (the label or an error tag), since every other part
/// is fixed chrome.
fn fit_line(line: Line<'static>, inner_w: usize) -> Line<'static> {
    let width: usize = line.spans.iter().map(|span| span.content.chars().count()).sum();
    if width <= inner_w {
        return line;
    }
    let overflow = width - inner_w;
    let mut spans = line.spans;
    if let Some(span) = spans.iter_mut().find(|span| span.content.chars().count() > overflow + 3) {
        let kept: String = span
            .content
            .chars()
            .take(span.content.chars().count().saturating_sub(overflow + 3))
            .collect();
        span.content = std::borrow::Cow::Owned(format!("{kept}..."));
    }
    Line::from(spans)
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
    use forge_workspace::DictateDeviceChoice;
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

    fn catalog_app(configured: Option<&str>) -> App {
        let mut app = App::test_default();
        app.dictate_devices = Some(Ok(forge_workspace::DictateDeviceCatalog {
            devices: vec![
                forge_workspace::Device {
                    id: "mbp-mic".into(),
                    name: "MacBook Pro Microphone".into(),
                    is_default: true,
                },
                forge_workspace::Device {
                    id: "shure-id".into(),
                    name: "Shure SM7B".into(),
                    is_default: false,
                },
            ],
            configured: configured.map(str::to_owned),
        }));
        app
    }

    #[test]
    fn the_dialog_draws_choices_hints_and_the_in_force_defaults() {
        let mut app = App::test_default();
        crate::app::dictate_picker::open(&mut app);

        let lines = render_overlay(&app, 80, 30);
        let joined: String = lines.join("\n");
        for fragment in [
            "Dictate",
            "this session only",
            "VOICE",
            "STRUCTURE",
            "DESTINATION",
            "INPUT DEVICE",
            "enter set",
        ] {
            assert!(joined.contains(fragment), "{fragment} is drawn: {joined}");
        }
        assert!(joined.contains("in force"), "the legend names the marker: {joined}");
        assert!(!joined.contains("· this session"), "a fresh session sets nothing: {joined}");
    }

    #[test]
    fn markers_appear_on_the_value_in_force_and_picks_name_their_source() {
        let mut app = App::test_default();
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_overrides =
            forge_workspace::DictateOverrides {
                context: Some(forge_workspace::Context::Email),
                ..Default::default()
            };
        crate::app::dictate_picker::open(&mut app);

        let lines = render_overlay(&app, 80, 30);
        let email_row = lines.iter().find(|l| l.contains("email layout")).expect("row");
        assert!(email_row.contains('\u{25cf}'), "the in-force row is marked: {email_row}");
        assert!(
            email_row.contains("this session"),
            "a session-set row names its source: {email_row}"
        );
        let prose_row = lines.iter().find(|l| l.contains("prose")).expect("row");
        assert!(
            prose_row.contains('\u{25cf}'),
            "the default in an untouched group is in force too: {prose_row}"
        );
        assert!(!prose_row.contains("this session"), "a default carries no source suffix");
        let formal_row = lines.iter().find(|l| l.contains("semi-formal")).expect("row");
        assert!(formal_row.contains('\u{25cf}'), "the voice default is in force: {formal_row}");
    }

    #[test]
    fn the_input_device_row_reads_the_configured_pin() {
        let mut app = catalog_app(Some("shure-id"));
        crate::app::dictate_picker::open(&mut app);

        let lines = render_overlay(&app, 80, 30);
        let device_row = lines.iter().find(|l| l.contains("Device: Shure SM7B")).expect("row");
        assert!(
            device_row.contains("configured default (forge.toml)"),
            "the tag names the pin: {device_row}"
        );
    }

    #[test]
    fn pick_mode_draws_the_list_the_notes_and_the_back_hint() {
        let mut app = catalog_app(Some("shure-id"));
        crate::app::dictate_picker::open(&mut app);
        let key =
            |code| crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..8 {
            crate::app::dictate_picker::handle_key(&mut app, key(crossterm::event::KeyCode::Down));
        }
        crate::app::dictate_picker::handle_key(&mut app, key(crossterm::event::KeyCode::Enter));

        let lines = render_overlay(&app, 80, 30);
        let joined: String = lines.join("\n");
        for fragment in [
            "Input device",
            "reverts on restart",
            "System default",
            "Shure SM7B",
            "esc back",
            "forge.toml keeps the default",
        ] {
            assert!(joined.contains(fragment), "{fragment} is drawn: {joined}");
        }
        assert!(joined.contains('\u{25cf}'), "the device in force is marked: {joined}");
    }

    #[test]
    fn a_gone_pin_draws_the_stale_note_and_no_in_force_legend() {
        let mut app = catalog_app(Some("unplugged-id"));
        crate::app::dictate_picker::open(&mut app);
        let key =
            |code| crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..8 {
            crate::app::dictate_picker::handle_key(&mut app, key(crossterm::event::KeyCode::Down));
        }
        crate::app::dictate_picker::handle_key(&mut app, key(crossterm::event::KeyCode::Enter));

        let lines = render_overlay(&app, 80, 30);
        let joined: String = lines.join("\n");
        assert!(
            joined.contains("unplugged-id") && joined.contains("not present"),
            "the stale pin is drawn by id: {joined}"
        );
        assert!(
            joined.contains("Nothing is in force"),
            "the stale note replaces the standard one: {joined}"
        );
        let footer = lines.iter().find(|l| l.contains("esc back")).expect("footer");
        assert!(
            !footer.contains("in force"),
            "no device is in force while the pin is stale: {footer}"
        );
    }

    #[test]
    fn a_machine_with_no_inputs_draws_the_empty_body() {
        let mut app = App::test_default();
        app.dictate_devices =
            Some(Ok(forge_workspace::DictateDeviceCatalog { devices: vec![], configured: None }));
        crate::app::dictate_picker::open(&mut app);
        let key =
            |code| crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..8 {
            crate::app::dictate_picker::handle_key(&mut app, key(crossterm::event::KeyCode::Down));
        }
        crate::app::dictate_picker::handle_key(&mut app, key(crossterm::event::KeyCode::Enter));

        let lines = render_overlay(&app, 80, 30);
        let joined: String = lines.join("\n");
        assert!(joined.contains(NO_DEVICES), "the empty body names itself: {joined}");
        assert!(joined.contains("esc back"), "only esc is offered: {joined}");
        assert!(!joined.contains("enter select"), "there is nothing to select: {joined}");
    }

    #[test]
    fn a_long_device_name_fits_the_dialog() {
        let mut app = App::test_default();
        let long_name =
            "Yet Another Extremely Long USB Audio Interface Name That No Dialog Was Built For";
        app.dictate_devices = Some(Ok(forge_workspace::DictateDeviceCatalog {
            devices: vec![forge_workspace::Device {
                id: "long".into(),
                name: long_name.to_owned(),
                is_default: true,
            }],
            configured: None,
        }));
        crate::app::dictate_picker::open(&mut app);
        let key =
            |code| crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..8 {
            crate::app::dictate_picker::handle_key(&mut app, key(crossterm::event::KeyCode::Down));
        }
        crate::app::dictate_picker::handle_key(&mut app, key(crossterm::event::KeyCode::Enter));

        let lines = render_overlay(&app, 80, 30);
        // The 62-column dialog sits 9 cells from the left edge, so a
        // full-width row ends at column 71; the trim must have landed
        // inside that, not merely inside the terminal.
        let body = lines.iter().find(|l| l.contains("...")).expect("a truncated row");
        assert!(
            body.trim_end().chars().count() <= 71,
            "the row stays inside the dialog, got {} chars: {body:?}",
            body.trim_end().chars().count()
        );
    }

    #[test]
    fn the_pin_session_state_renders_until_restart() {
        let mut app = catalog_app(Some("shure-id"));
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_device_pin =
            Some(DictateDeviceChoice::System);
        crate::app::dictate_picker::open(&mut app);

        let lines = render_overlay(&app, 80, 30);
        let device_row = lines.iter().find(|l| l.contains("Device: MacBook Pro Microphone"));
        assert!(
            device_row.is_some_and(|row| row.contains("active until restart")),
            "the system pick reads as until-restart: {lines:?}"
        );
    }
}
