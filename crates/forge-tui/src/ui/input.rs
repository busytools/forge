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
use tui_textarea::TextArea;

/// Horizontal padding to match header/footer inset.
const INPUT_PAD: u16 = 2;

/// Extra right-side breathing room so text doesn't touch the padded edge.
const INPUT_RIGHT_PAD: u16 = 1;

/// Prompt column width: "❯ " = 2 columns (icon + space)
const PROMPT_WIDTH: u16 = 2;

/// Rows reserved for the input box's chrome: top border + top inner
/// padding + bottom inner padding + bottom border. Both modes (chat
/// input and prompt) inherit the same 1-row top + 1-row bottom inner
/// padding from spec §3.
const INPUT_BORDER_LINES: u16 = 4;

/// Minimum text-area height inside the bordered box. The chat input
/// is the primary action surface, so the box never collapses to a
/// single line even when the draft is empty — gives the user a real
/// "type here" target on first glance.
const MIN_INPUT_INTERIOR_LINES: u16 = 2;

/// Maximum input area height (lines) to prevent the input from consuming the entire screen.
const MAX_INPUT_HEIGHT: u16 = 12;
const HIGHLIGHT_SLASH_PRIORITY: u8 = 6;
const HIGHLIGHT_MENTION_PRIORITY: u8 = 7;
const HIGHLIGHT_SUBAGENT_PRIORITY: u8 = 8;
const HIGHLIGHT_PASTE_PRIORITY: u8 = 9;
const HIGHLIGHT_IMAGE_BADGE_PRIORITY: u8 = 10;

/// Braille spinner frames (same as message.rs) for the connecting animation.
const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

/// Height of the login hint banner in lines (0 when no hint is active).
/// Used internally by `visual_line_count` and `render` so the layout
/// calculation and rendering stay in sync.
const LOGIN_HINT_LINES: u16 = 2;
const CANCEL_HINT_LINES: u16 = 1;
const PROMPT_SUGGESTION_HINT_LINES: u16 = 1;

#[derive(Clone, Copy)]
pub(crate) struct InputRenderGeometry {
    pub hint_pad: Option<Rect>,
    pub box_area: Rect,
    pub padded: Rect,
    pub prompt: Rect,
    pub text: Rect,
}

/// Whether a login hint banner is active.
fn has_login_hint(app: &App) -> bool {
    app.login_hint().is_some()
}

fn has_cancel_hint(app: &App) -> bool {
    app.pending_cancel()
}

fn has_prompt_suggestion_hint(app: &App) -> bool {
    app.input().is_empty()
        && app.focus_owner() == FocusOwner::Input
        && app.prompt_suggestion().is_some_and(|suggestion| !suggestion.trim().is_empty())
}

pub(crate) fn hint_line_count(app: &App) -> u16 {
    let login = if has_login_hint(app) { LOGIN_HINT_LINES } else { 0 };
    let cancel = if has_cancel_hint(app) { CANCEL_HINT_LINES } else { 0 };
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

    // Bordered box spans the full chat column width — the box's L/R
    // borders themselves are the visual margin, so the box sits flush
    // against the pane separators (or screen edges in narrow tier).
    // `box_area` is the full Rect the Block widget draws into
    // (borders + interior); `padded` is the interior where prompt +
    // text live, inset 1 cell L/R for the side borders and 2 rows top
    // + 2 rows bottom (border + inner padding row).
    let box_area = input_main_area;
    let padded = Rect {
        x: box_area.x.saturating_add(1),
        y: box_area.y.saturating_add(2),
        width: box_area.width.saturating_sub(2),
        height: box_area.height.saturating_sub(4),
    };
    let [prompt, text] =
        Layout::horizontal([Constraint::Length(PROMPT_WIDTH), Constraint::Min(1)]).areas(padded);

    InputRenderGeometry { hint_pad, box_area, padded, prompt, text }
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
        crate::ui::prompt::render(geometry.box_area, frame.buffer_mut(), prompt, queue_depth);
        return;
    }

    // Bordered frame around the input area — the chat input is THE
    // primary action surface, so the box renders with thick line
    // chrome in RUST_ORANGE + BOLD to grab the eye on first glance.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD));
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

        if has_cancel_hint(app) {
            let spinner_ch = SPINNER_FRAMES[app.spinner_frame % SPINNER_FRAMES.len()];
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
        let spinner_ch = SPINNER_FRAMES[app.spinner_frame % SPINNER_FRAMES.len()];
        let line = Line::from(vec![
            Span::styled(format!("{spinner_ch} "), Style::default().fg(theme::DIM)),
            Span::styled("Connecting to Claude Code...", Style::default().fg(theme::DIM)),
        ]);
        frame.render_widget(Paragraph::new(line), geometry.padded);
        return;
    }

    if app.status == AppStatus::CommandPending {
        let spinner_ch = SPINNER_FRAMES[app.spinner_frame % SPINNER_FRAMES.len()];
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

    // Render prompt icon
    let prompt = Line::from(Span::styled(
        format!("{} ", theme::PROMPT_CHAR),
        Style::default().fg(theme::RUST_ORANGE),
    ));
    frame.render_widget(Paragraph::new(prompt), geometry.prompt);

    if geometry.text.width == 0 {
        return;
    }

    configure_input_textarea(app);
    app.rendered_input_area = geometry.text;
    if app.selection().is_some_and(|selection| selection.kind == crate::app::SelectionKind::Input) {
        refresh_selection_snapshot(app);
    }
    frame.render_widget(app.input().editor(), geometry.text);

    if let Some(sel) = app.selection().copied()
        && sel.kind == crate::app::SelectionKind::Input
    {
        frame.render_widget(SelectionOverlay { selection: sel }, geometry.text);
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
/// layout to allocate the correct input area height. The text-area
/// portion never collapses below `MIN_INPUT_INTERIOR_LINES`.
pub fn visual_line_count(app: &mut App, area_width: u16) -> u16 {
    let hint = hint_line_count(app);
    // Content width sits inside the box's 1-col left/right borders.
    let content_width = area_width.saturating_sub(2).saturating_sub(PROMPT_WIDTH);
    let input_lines = app
        .input_mut()
        .measure_visual_lines(content_width, MAX_INPUT_HEIGHT)
        .max(MIN_INPUT_INTERIOR_LINES);
    hint + input_lines + INPUT_BORDER_LINES
}

#[cfg(test)]
mod tests {
    use super::{
        CANCEL_HINT_LINES, INPUT_BORDER_LINES, LOGIN_HINT_LINES, MAX_INPUT_HEIGHT,
        MIN_INPUT_INTERIOR_LINES, PROMPT_SUGGESTION_HINT_LINES, compute_render_geometry,
        slash_command_range, visual_line_count,
    };
    use crate::app::subagent::find_subagent_spans;
    use crate::app::{App, FocusTarget, LoginHint};
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
    fn visual_line_count_hides_prompt_suggestion_hint_when_input_lacks_focus() {
        let mut app = App::test_default();
        app.set_prompt_suggestion(Some("Write tests for the retry flow".to_owned()));
        // Claim Permission focus to take focus off the input — the
        // prompt-suggestion hint only renders when the input owns
        // focus. (The old TodoList focus target was used here before;
        // it's been removed along with the bottom todo panel.)
        *app.pending_interaction_ids_mut() = vec!["perm-1".into()];
        app.claim_focus_target(FocusTarget::Permission);
        assert_eq!(visual_line_count(&mut app, 80), MIN_INPUT_INTERIOR_LINES + INPUT_BORDER_LINES);
    }

    #[test]
    fn compute_render_geometry_reserves_top_and_bottom_padding_rows() {
        let area = Rect::new(0, 0, 80, 5);
        let geometry = compute_render_geometry(area, 0);
        // text area should start at y=2 (border y=0, padding y=1, text y=2).
        assert_eq!(geometry.text.y, 2);
        // text area should occupy a single row (y=2 only) — padding y=3, border y=4.
        assert_eq!(geometry.text.height, 1);
    }
}
