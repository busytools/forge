// ratatui Rect coords are u16; row/col positions are usize-bounded by
// terminal size and truncated to u16 here. The cast is inherent.
#![allow(clippy::cast_possible_truncation)]

use crate::app::input::parse_paste_placeholder_ranges;
use crate::app::mention;
use crate::app::subagent;
use crate::app::{App, AppStatus, FocusOwner};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::{Block, BorderType, Borders};
use std::time::Instant;

use tui_textarea::TextArea;

/// Horizontal padding to match header/footer inset.
const INPUT_PAD: u16 = 2;

/// Extra right-side breathing room so text doesn't touch the padded edge.
const INPUT_RIGHT_PAD: u16 = 1;

/// Prompt column width: "➤ " = 2 columns (icon + space)
const PROMPT_WIDTH: u16 = 2;

/// Rows reserved for the input box's chrome: top border + bottom
/// border. No internal vertical padding - the slim chat-input box
/// starts as a single-row interior and grows up to MAX_INPUT_HEIGHT
/// rows of interior content. The wider prompt-mode dock keeps its
/// own internal padding via Line::default() entries in build_lines.
const INPUT_BORDER_LINES: u16 = 2;

/// Minimum text-area height inside the bordered box. Single row so
/// the box is just tall enough for one typed line plus the orange
/// chrome - grows as the draft wraps.
const MIN_INPUT_INTERIOR_LINES: u16 = 1;

/// Maximum input area height (rows of interior content). The box
/// grows row by row as the user types - typed drafts almost never
/// exceed a few lines, so the cap exists only to prevent the box
/// from consuming the entire terminal in pathological cases.
/// Large pastes don't hit this cap because the paste handler
/// collapses anything past PASTE_PLACEHOLDER_{CHAR,LINE}_THRESHOLD
/// into a single placeholder token.
const MAX_INPUT_HEIGHT: u16 = 50;
const HIGHLIGHT_SLASH_PRIORITY: u8 = 6;
const HIGHLIGHT_MENTION_PRIORITY: u8 = 7;
const HIGHLIGHT_SUBAGENT_PRIORITY: u8 = 8;
const HIGHLIGHT_PASTE_PRIORITY: u8 = 9;
const HIGHLIGHT_IMAGE_BADGE_PRIORITY: u8 = 10;

/// Height of the login hint banner in lines (0 when no hint is active).
/// Used internally by `visual_line_count` and `render` so the layout
/// calculation and rendering stay in sync.
const LOGIN_HINT_LINES: u16 = 2;
const CANCEL_HINT_LINES: u16 = 1;
const PROMPT_SUGGESTION_HINT_LINES: u16 = 1;

#[derive(Clone)]
pub(crate) struct InputRenderGeometry {
    pub hint_pad: Option<Rect>,
    pub box_area: Rect,
    pub padded: Rect,
}

fn has_prompt_suggestion_hint(app: &App) -> bool {
    app.input().is_empty()
        && app.focus_owner() == FocusOwner::Input
        && app.prompt_suggestion().is_some_and(|suggestion| !suggestion.trim().is_empty())
}

pub(crate) fn hint_line_count(app: &App) -> u16 {
    let login = if app.login_hint().is_some() { LOGIN_HINT_LINES } else { 0 };
    let cancel = if app.pending_cancel() { CANCEL_HINT_LINES } else { 0 };
    let suggestion = if has_prompt_suggestion_hint(app) { PROMPT_SUGGESTION_HINT_LINES } else { 0 };
    login + cancel + suggestion
}

pub(crate) fn compute_render_geometry(area: Rect, hint_lines: u16) -> InputRenderGeometry {
    let (hint_area, input_main_area) = if hint_lines > 0 {
        let [hint, main] =
            Layout::vertical([Constraint::Length(hint_lines), Constraint::Min(1)]).areas(area);
        (Some(hint), main)
    } else {
        (None, area)
    };

    let hint_pad = hint_area.map(|hint| Rect {
        x: hint.x.saturating_add(INPUT_PAD),
        y: hint.y,
        width: hint.width.saturating_sub(INPUT_PAD * 2 + INPUT_RIGHT_PAD),
        height: hint.height,
    });

    // Bordered box spans the full chat column width - the box's L/R
    // borders themselves are the visual margin, so the box sits flush
    // against the pane separators (or screen edges in narrow tier).
    // `box_area` is the full Rect the Block widget draws into
    // (borders + interior); `padded` is the interior where prompt +
    // text live, inset 1 cell L/R for the side borders and 1 row top
    // + 1 row bottom for the top/bottom borders (no inner padding).
    let box_area = input_main_area;
    let padded = Rect {
        x: box_area.x.saturating_add(1),
        y: box_area.y.saturating_add(1),
        width: box_area.width.saturating_sub(2),
        height: box_area.height.saturating_sub(2),
    };

    InputRenderGeometry { hint_pad, box_area, padded }
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let hint_lines = hint_line_count(app);
    let geometry = compute_render_geometry(area, hint_lines);

    // Prompt mode: when the active session has a prompt at the head of
    // its queue, the dock morphs into the unified prompt widget. The
    // widget owns its own chrome (thick orange block) and inner
    // padding, so we skip the normal chat-input rendering entirely.
    if let Some(session) = app.active_session()
        && let Some(prompt) = session.prompt_queue.front()
    {
        let queue_depth = session.prompt_queue.len();
        let prompt = prompt.clone();
        let notes_text = app.input().text();
        crate::ui::prompt::render(
            geometry.box_area,
            frame.buffer_mut(),
            &prompt,
            queue_depth,
            Some(notes_text.as_str()),
        );
        return;
    }

    // Bordered frame around the input area - the chat input is THE
    // primary action surface, so the box renders with thick line
    // chrome in RUST_ORANGE + BOLD to grab the eye on first glance.
    // A live dictate take hands the border colour over through the
    // easing state; `None` means the plain orange stands untouched.
    let border_fg =
        crate::app::dictate::border_color(app, Instant::now()).unwrap_or(theme::RUST_ORANGE);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(border_fg).add_modifier(Modifier::BOLD));
    frame.render_widget(block, geometry.box_area);

    if let Some(hint_pad) = geometry.hint_pad {
        let mut hint_y = hint_pad.y;

        if let Some(hint) = &app.login_hint() {
            let lines = vec![
                Line::from(Span::styled(
                    format!(
                        "Authentication required: {} -- {}",
                        hint.method_name, hint.method_description
                    ),
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(Span::styled(
                    "Run `claude auth login` in another terminal to authenticate",
                    Style::default().fg(theme::DIM),
                )),
            ];
            let login_area =
                Rect { x: hint_pad.x, y: hint_y, width: hint_pad.width, height: LOGIN_HINT_LINES };
            frame.render_widget(Paragraph::new(lines), login_area);
            hint_y = hint_y.saturating_add(LOGIN_HINT_LINES);
        }

        if app.pending_cancel() {
            let spinner_ch = app.active_spinner_glyph();
            let cancel_line = Line::from(vec![
                Span::styled(format!("{spinner_ch} "), Style::default().fg(theme::DIM)),
                Span::styled("Cancelling current turn...", Style::default().fg(theme::DIM)),
            ]);
            let cancel_area =
                Rect { x: hint_pad.x, y: hint_y, width: hint_pad.width, height: CANCEL_HINT_LINES };
            frame.render_widget(Paragraph::new(cancel_line), cancel_area);
            hint_y = hint_y.saturating_add(CANCEL_HINT_LINES);
        }

        if has_prompt_suggestion_hint(app)
            && let Some(suggestion) = app.prompt_suggestion()
        {
            let suggestion_line = Line::from(vec![
                Span::styled("Suggestion: ", Style::default().fg(theme::DIM)),
                Span::styled(suggestion.trim().to_owned(), Style::default().fg(Color::White)),
                Span::styled("    Tab to accept", Style::default().fg(theme::DIM)),
            ]);
            let suggestion_area = Rect {
                x: hint_pad.x,
                y: hint_y,
                width: hint_pad.width,
                height: PROMPT_SUGGESTION_HINT_LINES,
            };
            frame.render_widget(Paragraph::new(suggestion_line), suggestion_area);
        }
    }

    // During Connecting state, show a spinner with static text
    if app.status == AppStatus::Connecting {
        let spinner_ch = app.active_spinner_glyph();
        let line = Line::from(vec![
            Span::styled(format!("{spinner_ch} "), Style::default().fg(theme::DIM)),
            Span::styled("Connecting to Claude Code...", Style::default().fg(theme::DIM)),
        ]);
        frame.render_widget(Paragraph::new(line), geometry.padded);
        return;
    }

    if app.status == AppStatus::CommandPending {
        let spinner_ch = app.active_spinner_glyph();
        let label = app.pending_command_label().unwrap_or("Processing command...");
        let line = Line::from(vec![
            Span::styled(format!("{spinner_ch} "), Style::default().fg(theme::DIM)),
            Span::styled(label.to_owned(), Style::default().fg(theme::DIM)),
        ]);
        frame.render_widget(Paragraph::new(line), geometry.padded);
        return;
    }

    if app.status == AppStatus::Error {
        let lines = vec![
            Line::from(Span::styled(
                "Input disabled due to error",
                Style::default().fg(theme::STATUS_ERROR),
            )),
            Line::from(Span::styled(
                "Press Ctrl+Q to quit and try again.",
                Style::default().fg(theme::DIM),
            )),
        ];
        frame.render_widget(Paragraph::new(lines), geometry.padded);
        return;
    }

    // Padded interior fits exactly the content lines (1 to
    // MAX_INPUT_HEIGHT). No vertical slack to center against - the
    // box height tracks the textarea row count directly. The one
    // exception is the dictate row, which grows the interior by a row
    // and pushes the draft down: a stamped notice, or the live take's
    // status row.
    let notice_visible = crate::app::dictate::dictate_row_visible(app);
    let (body, notice_area) = if notice_visible {
        let [top, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(geometry.padded);
        (rest, Some(top))
    } else {
        (geometry.padded, None)
    };
    let [prompt_rect, text_rect] =
        Layout::horizontal([Constraint::Length(PROMPT_WIDTH), Constraint::Min(1)]).areas(body);

    if let Some(notice_area) = notice_area {
        let line = crate::app::dictate::dictate_row_content(app, geometry.padded.width as usize);
        frame.render_widget(Paragraph::new(line), notice_area);
    }

    // Render prompt icon
    let prompt = Line::from(Span::styled(
        format!("{} ", theme::PROMPT_CHAR),
        Style::default().fg(theme::RUST_ORANGE),
    ));
    frame.render_widget(Paragraph::new(prompt), prompt_rect);

    if text_rect.width == 0 {
        return;
    }

    configure_input_textarea(app);
    app.rendered_input_area = text_rect;
    if app.selection().is_some_and(|selection| selection.kind == crate::app::SelectionKind::Input) {
        refresh_selection_snapshot(app);
    }
    frame.render_widget(app.input().editor(), text_rect);

    // While a take is recording, the cursor spot carries the live dB
    // figure instead of the blinking block. The caret cell is the one
    // the textarea drew with the cursor style; the readout overwrites
    // it and the cells after it.
    if let Some((text, colour)) = crate::app::dictate::active_db_readout(app, Instant::now())
        && let Some((x, y)) = caret_cell(frame, text_rect)
    {
        let width = u16::try_from(text.chars().count()).unwrap_or(u16::MAX);
        let area = Rect { x, y, width, height: 1 };
        // The caret cell carries the cursor's blink style; the readout
        // must fully own the cells it covers.
        frame.buffer_mut().set_style(area, Style::reset());
        let readout = Paragraph::new(Line::from(Span::styled(text, Style::default().fg(colour))));
        frame.render_widget(readout, area);
    }

    if let Some(sel) = app.selection().copied()
        && sel.kind == crate::app::SelectionKind::Input
    {
        frame.render_widget(SelectionOverlay { selection: sel }, text_rect);
    }
}

pub(super) fn refresh_selection_snapshot(app: &mut App) {
    if !app.selection().is_some_and(|selection| selection.kind == crate::app::SelectionKind::Input)
    {
        return;
    }

    let area = app.rendered_input_area;
    if area.width == 0 || area.height == 0 {
        return;
    }

    configure_input_textarea(app);
    app.rendered_input_lines = render_lines_from_textarea(app.input().editor(), area);
}

/// The rect the draft's textarea renders into, shifted down when the
/// dictate row is up. Dropdown anchoring shares this with the
/// renderer so the two never disagree.
pub(crate) fn draft_text_area(area: Rect, app: &App) -> Rect {
    let geometry = compute_render_geometry(area, hint_line_count(app));
    let body = if crate::app::dictate::dictate_row_visible(app) {
        let [_top, rest] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(geometry.padded);
        rest
    } else {
        geometry.padded
    };
    let [_prompt, text] =
        Layout::horizontal([Constraint::Length(PROMPT_WIDTH), Constraint::Min(1)]).areas(body);
    text
}

/// The caret's on-screen cell: the one cell the textarea drew with the
/// cursor's slow-blink style.
fn caret_cell(frame: &mut Frame, area: Rect) -> Option<(u16, u16)> {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = frame.buffer_mut().cell((x, y))
                && cell.style().add_modifier.contains(Modifier::SLOW_BLINK)
            {
                return Some((x, y));
            }
        }
    }
    None
}

fn configure_input_textarea(app: &mut App) {
    let needs_highlight_update = app.input().highlight_version != app.input().content_version;

    {
        let textarea = app.input_mut().editor_mut();
        textarea.set_placeholder_text("Type a message...");
        textarea.set_placeholder_style(Style::default().fg(theme::DIM));
        textarea.set_cursor_line_style(Style::default());
        textarea.set_cursor_style(
            Style::default().add_modifier(Modifier::REVERSED).add_modifier(Modifier::SLOW_BLINK),
        );
    }

    if needs_highlight_update {
        let lines = app.input().lines().to_vec();
        let content_version = app.input().content_version;
        let textarea = app.input_mut().editor_mut();
        textarea.clear_custom_highlight();
        apply_textarea_highlights(textarea, &lines);
        app.input_mut().highlight_version = content_version;
    }
}

fn apply_textarea_highlights(textarea: &mut TextArea<'_>, lines: &[String]) {
    let slash_style = Style::default().fg(theme::SLASH_COMMAND);
    let mention_style = Style::default().fg(Color::Cyan);
    let subagent_style = Style::default().fg(theme::SUBAGENT_TOKEN);
    let paste_style = Style::default().fg(Color::Green);
    let image_badge_style = Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD);

    for (row, line) in lines.iter().enumerate() {
        if let Some((start, end)) = slash_command_range(line) {
            textarea.custom_highlight(
                ((row, start), (row, end)),
                slash_style,
                HIGHLIGHT_SLASH_PRIORITY,
            );
        }

        for (start, end, _) in mention::find_mention_spans(line) {
            textarea.custom_highlight(
                ((row, start), (row, end)),
                mention_style,
                HIGHLIGHT_MENTION_PRIORITY,
            );
        }

        for (start, end, _) in subagent::find_subagent_spans(line) {
            textarea.custom_highlight(
                ((row, start), (row, end)),
                subagent_style,
                HIGHLIGHT_SUBAGENT_PRIORITY,
            );
        }

        for (start, end) in parse_paste_placeholder_ranges(line) {
            textarea.custom_highlight(
                ((row, start), (row, end)),
                paste_style,
                HIGHLIGHT_PASTE_PRIORITY,
            );
        }

        for (start, end, _) in crate::app::clipboard_image::find_image_badge_spans(line) {
            textarea.custom_highlight(
                ((row, start), (row, end)),
                image_badge_style,
                HIGHLIGHT_IMAGE_BADGE_PRIORITY,
            );
        }
    }
}

fn slash_command_range(line: &str) -> Option<(usize, usize)> {
    let start = line.find(|c: char| !c.is_whitespace())?;
    if line.as_bytes().get(start).copied() != Some(b'/') {
        return None;
    }
    let rel_end =
        line[start..].find(char::is_whitespace).unwrap_or_else(|| line.len().saturating_sub(start));
    let end = start + rel_end;
    if end <= start + 1 {
        return None;
    }
    Some((start, end))
}

struct SelectionOverlay {
    selection: crate::app::SelectionState,
}

impl Widget for SelectionOverlay {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (start, end) =
            crate::app::normalize_selection(self.selection.start, self.selection.end);
        for row in start.row..=end.row {
            let y = area.y.saturating_add(row as u16);
            if y >= area.bottom() {
                break;
            }
            let row_start = if row == start.row { start.col } else { 0 };
            let row_end = if row == end.row { end.col } else { area.width as usize };
            for col in row_start..row_end {
                let x = area.x.saturating_add(col as u16);
                if x >= area.right() {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

fn render_lines_from_textarea(textarea: &TextArea<'_>, area: Rect) -> Vec<String> {
    let mut buf = Buffer::empty(area);
    textarea.render(area, &mut buf);
    let mut lines = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((area.x + x, area.y + y)) {
                line.push_str(cell.symbol());
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    lines
}

/// Total visual height for the input area: input lines + hint
/// banners + the bordered box's top/bottom rows. Called by the
/// layout to allocate the correct input area height.
///
/// When the active session has a prompt at the head of its queue,
/// the height is dictated by the prompt widget's required lines
/// instead of the chat-input editor - otherwise the morphed dock
/// would clip the option list to the editor's tiny default height.
/// The text-area portion never collapses below `MIN_INPUT_INTERIOR_LINES`.
pub fn visual_line_count(app: &mut App, area_width: u16) -> u16 {
    let hint = hint_line_count(app);

    // Prompt-mode short-circuit: dock is morphed, height comes from
    // the prompt widget's required lines (header + options + footer +
    // padding + borders).
    if let Some(session) = app.active_session()
        && let Some(prompt) = session.prompt_queue.front()
    {
        let queue_depth = session.prompt_queue.len();
        let prompt = prompt.clone();
        let notes_text = app.input().text();
        return hint
            + crate::ui::prompt::prompt_required_lines(
                &prompt,
                queue_depth,
                area_width,
                Some(notes_text.as_str()),
            );
    }

    // Content width sits inside the box's 1-col left/right borders.
    let content_width = area_width.saturating_sub(2).saturating_sub(PROMPT_WIDTH);
    let input_lines = app
        .input_mut()
        .measure_visual_lines(content_width, MAX_INPUT_HEIGHT)
        .max(MIN_INPUT_INTERIOR_LINES);
    let notice_row = u16::from(crate::app::dictate::dictate_row_visible(app));
    hint + input_lines + notice_row + INPUT_BORDER_LINES
}

#[cfg(test)]
mod tests {
    use super::{
        CANCEL_HINT_LINES, INPUT_BORDER_LINES, LOGIN_HINT_LINES, MAX_INPUT_HEIGHT,
        MIN_INPUT_INTERIOR_LINES, PROMPT_SUGGESTION_HINT_LINES, compute_render_geometry,
        slash_command_range, visual_line_count,
    };
    use crate::app::subagent::find_subagent_spans;
    use crate::app::{App, LoginHint};
    use ratatui::layout::Rect;

    #[test]
    fn slash_range_matches_leading_command_token() {
        assert_eq!(slash_command_range("/mode plan"), Some((0, 5)));
        assert_eq!(slash_command_range("  /mode plan"), Some((2, 7)));
    }

    #[test]
    fn slash_range_ignores_non_command_lines() {
        assert_eq!(slash_command_range("hello /mode"), None);
        assert_eq!(slash_command_range("/"), None);
        assert_eq!(slash_command_range("   "), None);
    }

    #[test]
    fn subagent_spans_match_valid_ampersand_tokens() {
        let spans = find_subagent_spans("&reviewer and &explore");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].2, "reviewer");
        assert_eq!(spans[1].2, "explore");
    }

    #[test]
    fn subagent_spans_reject_double_ampersand_shell_syntax() {
        let spans = find_subagent_spans("cargo test && cargo clippy");
        assert!(spans.is_empty());
    }

    #[test]
    fn visual_line_count_uses_textarea_max_rows() {
        let mut app = App::test_default();
        app.input_mut().set_text(&"x".repeat(500));
        assert_eq!(visual_line_count(&mut app, 8), MAX_INPUT_HEIGHT + INPUT_BORDER_LINES);
    }

    #[test]
    fn visual_line_count_includes_login_hint_rows() {
        let mut app = App::test_default();
        *app.login_hint_mut() = Some(LoginHint {
            method_name: "oauth".to_owned(),
            method_description: "Sign in".to_owned(),
        });
        assert_eq!(
            visual_line_count(&mut app, 80),
            LOGIN_HINT_LINES + MIN_INPUT_INTERIOR_LINES + INPUT_BORDER_LINES
        );
    }

    #[test]
    fn visual_line_count_includes_cancel_hint_row() {
        let mut app = App::test_default();
        app.set_pending_cancel(true);
        assert_eq!(
            visual_line_count(&mut app, 80),
            CANCEL_HINT_LINES + MIN_INPUT_INTERIOR_LINES + INPUT_BORDER_LINES
        );
    }

    #[test]
    fn visual_line_count_includes_prompt_suggestion_hint_row() {
        let mut app = App::test_default();
        app.set_prompt_suggestion(Some("Write tests for the retry flow".to_owned()));
        assert_eq!(
            visual_line_count(&mut app, 80),
            PROMPT_SUGGESTION_HINT_LINES + MIN_INPUT_INTERIOR_LINES + INPUT_BORDER_LINES
        );
    }

    #[test]
    fn visual_line_count_hides_prompt_suggestion_hint_when_input_not_empty() {
        let mut app = App::test_default();
        app.set_prompt_suggestion(Some("Write tests for the retry flow".to_owned()));
        app.input_mut().set_text("draft");
        assert_eq!(visual_line_count(&mut app, 80), MIN_INPUT_INTERIOR_LINES + INPUT_BORDER_LINES);
    }

    #[test]
    fn compute_render_geometry_carves_borders_only() {
        // Slim chat-input box: top border at y=0, interior starts at
        // y=1, bottom border at y=area.height-1. With area.height=3,
        // interior is exactly 1 row at y=1.
        let area = Rect::new(0, 0, 80, 3);
        let geometry = compute_render_geometry(area, 0);
        assert_eq!(geometry.padded.y, 1, "interior starts immediately after top border");
        assert_eq!(geometry.padded.height, 1, "single-row interior for 3-row box");
    }

    mod dictate_indicator {
        use super::*;
        use crate::app::events::apply_session_update;
        use crate::ui::input::render;
        use forge_workspace::{DictateOutcome, SessionUpdate};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::time::{Duration, Instant};

        fn render_input(app: &mut App, w: u16, h: u16) -> Vec<String> {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
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

        fn active_key(app: &App) -> forge_workspace::SessionKey {
            app.active_session_key.clone().expect("test_default has an active bucket")
        }

        #[test]
        fn with_dictation_off_the_border_is_what_shipped_yesterday() {
            let mut app = App::test_default();
            let rows = render_input(&mut app, 80, 4);
            assert!(
                rows[0].starts_with("\u{250f}\u{2501}"),
                "no dictate support means a plain thick border, got: {}",
                rows[0]
            );
        }

        #[test]
        fn an_idle_dictate_available_composer_is_a_plain_box() {
            let mut app = App::test_default();
            let mut terminal = Terminal::new(TestBackend::new(80, 4)).expect("terminal");
            terminal.draw(|frame| render(frame, frame.area(), &mut app)).expect("draw");
            let buffer = terminal.backend().buffer().clone();
            let rows: Vec<String> = (0..4)
                .map(|y| {
                    (0..80)
                        .map(|x| {
                            buffer
                                .cell((x, y))
                                .map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                        })
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect();
            assert!(
                rows[0].starts_with("\u{250f}\u{2501}"),
                "idle reserves nothing on the border, got: {}",
                rows[0]
            );
            assert!(
                !rows.iter().any(|row| row.contains("\u{2581}")),
                "the old dim meter cells are gone at idle"
            );
            assert!(
                !rows.iter().any(|row| row.contains("esc")),
                "idle draws no esc hint anywhere in the box"
            );
            let corner = buffer.cell((0, 0)).expect("corner").style();
            assert_eq!(
                corner.fg,
                Some(crate::ui::theme::RUST_ORANGE),
                "the idle border is the composer's own orange, got {:?}",
                corner.fg
            );
            assert!(
                corner.add_modifier.contains(ratatui::style::Modifier::BOLD),
                "the idle border keeps its bold chrome"
            );
            assert_eq!(
                visual_line_count(&mut app, 80),
                MIN_INPUT_INTERIOR_LINES + INPUT_BORDER_LINES,
                "the box does not grow at idle"
            );
        }

        #[test]
        fn recording_draws_the_status_row_and_grows_the_box() {
            let mut app = App::test_default();
            let key = active_key(&app);
            apply_session_update(
                &mut app,
                SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
            );
            apply_session_update(&mut app, SessionUpdate::DictateLevel { key, peak_db: -6.0 });

            let rows = render_input(&mut app, 80, 4);
            assert!(
                rows[0].starts_with("\u{250f}\u{2501}"),
                "the border carries no meter cells, got: {}",
                rows[0]
            );
            assert!(
                rows[1].contains("listening") && rows[1].contains("0:00"),
                "the status row names the state and the elapsed time, got: {}",
                rows[1]
            );
            assert!(
                rows[1].contains("\u{25cf}") && rows[1].contains("\u{2588}"),
                "an orange dot and the live meter share the row, got: {}",
                rows[1]
            );
            assert!(
                rows[1].ends_with("esc cancel\u{2503}") && !rows[1].contains("esc to cancel"),
                "the esc hint rides the status row in its own wording, got: {}",
                rows[1]
            );
            assert_eq!(
                visual_line_count(&mut app, 80),
                MIN_INPUT_INTERIOR_LINES + 1 + INPUT_BORDER_LINES,
                "the status row is the row that grows the box"
            );
        }

        #[test]
        fn transcribing_hides_the_row_until_three_seconds_then_freezes_the_meter() {
            let mut app = App::test_default();
            let key = active_key(&app);
            apply_session_update(
                &mut app,
                SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
            );
            apply_session_update(
                &mut app,
                SessionUpdate::DictateLevel { key: key.clone(), peak_db: -6.0 },
            );
            apply_session_update(&mut app, SessionUpdate::DictateTranscribing { key: key.clone() });
            // A reading that races past the handoff must not redraw the row.
            apply_session_update(
                &mut app,
                SessionUpdate::DictateLevel { key: key.clone(), peak_db: -2.0 },
            );

            let rows = render_input(&mut app, 80, 5);
            assert!(
                !rows[1].contains("transcribing") && !rows[1].contains("listening"),
                "a warm take never flashes the row back, got: {}",
                rows[1]
            );

            let bucket = app.session_mut(&key).expect("bucket");
            let indicator = bucket.dictate.as_mut().expect("a take is in flight");
            indicator.transcribing_since = Some(
                Instant::now()
                    .checked_sub(Duration::from_millis(4000))
                    .expect("a 4 s backdate is safe"),
            );
            let rows = render_input(&mut app, 80, 5);
            assert!(
                rows[1].contains("transcribing") && rows[1].contains("esc cancel"),
                "past the threshold the row reappears in transcribing form, got: {}",
                rows[1]
            );
            assert!(
                rows[1].contains("\u{25cc}") && rows[1].contains("\u{2588}"),
                "the blue dot and the frozen meter hold the last recording frame, got: {}",
                rows[1]
            );
        }

        #[test]
        fn a_landed_take_beats_the_border_green_then_settles() {
            let mut app = App::test_default();
            let _clipboard = crate::app::keys::override_test_clipboard(
                crate::app::keys::TestClipboardMode::Succeed,
            );
            let key = active_key(&app);
            apply_session_update(
                &mut app,
                SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
            );
            apply_session_update(
                &mut app,
                SessionUpdate::DictateEnded {
                    key,
                    generation: 1,
                    outcome: DictateOutcome::Landed {
                        text: "run just check".to_owned(),
                        truncated: false,
                    },
                },
            );

            let mut terminal = Terminal::new(TestBackend::new(80, 4)).expect("terminal");
            terminal.draw(|frame| render(frame, frame.area(), &mut app)).expect("draw");
            let corner = terminal.backend().buffer().cell((0, 0)).expect("corner").style().fg;
            let Some(ratatui::style::Color::Rgb(_, g, b)) = corner else {
                panic!("the border is rgb during the handoff, got {corner:?}");
            };
            assert!(
                g > 118 && b > 0,
                "the landed take starts the border toward the green beat, got {corner:?}"
            );

            // Backdate the afterglow past its window and keep
            // rendering until the state settles away.
            {
                let bucket = app.session_mut(&active_key(&app)).expect("bucket");
                let border = bucket.dictate_border.as_mut().expect("border state");
                if let crate::app::dictate::DictateBorder::Afterglow { started, .. } = border {
                    *started = Instant::now()
                        .checked_sub(Duration::from_millis(5_000))
                        .expect("a 5 s backdate is safe");
                }
            }
            for _ in 0..120 {
                terminal.draw(|frame| render(frame, frame.area(), &mut app)).expect("draw");
            }
            let corner = terminal.backend().buffer().cell((0, 0)).expect("corner").style().fg;
            assert_eq!(
                corner,
                Some(ratatui::style::Color::Rgb(244, 118, 0)),
                "past the beat the border settles at the composer's normal orange"
            );
            let bucket = app.session_mut(&active_key(&app)).expect("bucket");
            assert!(bucket.dictate_border.is_none(), "the settled border leaves no state behind");
        }

        #[test]
        fn the_recording_cursor_spot_shows_the_live_db_figure() {
            let mut app = App::test_default();
            let key = active_key(&app);
            apply_session_update(
                &mut app,
                SessionUpdate::DictateStarted { key, floor_db: -50.0, generation: 1 },
            );

            let mut terminal = Terminal::new(TestBackend::new(80, 4)).expect("terminal");
            terminal.draw(|frame| render(frame, frame.area(), &mut app)).expect("draw");
            let buffer = terminal.backend().buffer().clone();
            let draft_row: String = (0..80)
                .map(|x| {
                    buffer.cell((x, 2)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            assert!(
                draft_row.contains("-50 dB"),
                "the caret cell carries the live dB figure, got: {draft_row}"
            );
            let caret = buffer.cell((3, 2)).expect("caret cell").style();
            assert!(
                !caret.add_modifier.contains(ratatui::style::Modifier::SLOW_BLINK),
                "the readout replaces the blinking block, not a bouncing block beside it"
            );

            // Mid-draft the readout would paint over real words, so it
            // stands down and the normal cursor returns.
            app.input_mut().set_text("hello world");
            app.input_mut().set_cursor_col(5);
            terminal.draw(|frame| render(frame, frame.area(), &mut app)).expect("draw");
            let buffer = terminal.backend().buffer().clone();
            let draft_row: String = (0..80)
                .map(|x| {
                    buffer.cell((x, 2)).map_or(' ', |c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect();
            assert!(
                !draft_row.contains("dB"),
                "a mid-draft caret keeps the readout off, got: {draft_row}"
            );
            let caret = buffer.cell((8, 2)).expect("caret cell mid-draft").style();
            assert!(
                caret.add_modifier.contains(ratatui::style::Modifier::SLOW_BLINK),
                "the blinking cursor stands in the readout's place"
            );
        }

        #[test]
        fn a_quiet_take_leaves_a_notice_row_until_the_next_keystroke() {
            let mut app = App::test_default();
            let key = active_key(&app);
            apply_session_update(
                &mut app,
                SessionUpdate::DictateEnded {
                    key: key.clone(),
                    generation: 1,
                    outcome: DictateOutcome::NoAudio { peak_db: -38.2, seconds: 4 },
                },
            );

            let rows = render_input(&mut app, 80, 5);
            assert!(
                rows[1].contains("-38.2") && rows[1].contains("try again"),
                "a quiet room quotes its own measurement and offers a retry, got: {}",
                rows[1]
            );
            assert_eq!(
                visual_line_count(&mut app, 80),
                MIN_INPUT_INTERIOR_LINES + 1 + INPUT_BORDER_LINES,
                "the notice row is the one row that grows the box"
            );

            app.input_mut().insert_str("k");
            let rows = render_input(&mut app, 80, 5);
            assert!(
                !rows[1].contains("try again"),
                "the next keystroke clears the notice, got: {}",
                rows[1]
            );
            assert_eq!(
                visual_line_count(&mut app, 80),
                MIN_INPUT_INTERIOR_LINES + INPUT_BORDER_LINES,
                "the box shrinks back once the notice clears"
            );
        }

        #[test]
        fn landed_words_insert_at_the_caret_of_the_session_that_started() {
            let mut app = App::test_default();
            let _clipboard = crate::app::keys::override_test_clipboard(
                crate::app::keys::TestClipboardMode::Succeed,
            );
            let key = active_key(&app);
            apply_session_update(
                &mut app,
                SessionUpdate::DictateEnded {
                    key: key.clone(),
                    generation: 1,
                    outcome: DictateOutcome::Landed {
                        text: "run just check".to_owned(),
                        truncated: false,
                    },
                },
            );
            assert_eq!(
                app.session_mut(&key).expect("bucket").input.text(),
                "run just check",
                "the words land in the owning bucket's draft, at its caret"
            );
            assert!(
                app.session_mut(&key).expect("bucket").dictate.is_none(),
                "the take is over; the indicator falls back to idle"
            );
        }

        #[test]
        fn a_stale_take_does_not_wipe_a_live_recording_on_the_same_key() {
            let mut app = App::test_default();
            let _clipboard = crate::app::keys::override_test_clipboard(
                crate::app::keys::TestClipboardMode::Succeed,
            );
            let key = active_key(&app);
            // Generation 2 is live and recording; generation 1 is an older
            // take of the same session still finishing.
            apply_session_update(
                &mut app,
                SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 2 },
            );

            apply_session_update(
                &mut app,
                SessionUpdate::DictateEnded {
                    key: key.clone(),
                    generation: 1,
                    outcome: DictateOutcome::Landed {
                        text: "stale words".to_owned(),
                        truncated: false,
                    },
                },
            );
            {
                let bucket = app.session_mut(&key).expect("bucket");
                assert_eq!(bucket.input.text(), "stale words", "the older take's words still land");
                assert!(
                    bucket.dictate.is_some(),
                    "the live recording must survive a stale outcome on the same key"
                );
            }
            assert!(
                crate::app::dictate::dictate_owns_esc(&app),
                "Esc keeps abandoning the live take"
            );

            apply_session_update(
                &mut app,
                SessionUpdate::DictateEnded {
                    key: key.clone(),
                    generation: 2,
                    outcome: DictateOutcome::Cancelled,
                },
            );
            assert!(
                app.session_mut(&key).expect("bucket").dictate.is_none(),
                "the take's own outcome still resets the indicator"
            );
        }
    }
}
