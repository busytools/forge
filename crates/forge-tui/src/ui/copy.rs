//! Clean copy: the clipboard text behind the chat render.
//!
//! The screen grid cannot tell a soft wrap from a real newline, so copy
//! works from provenance recorded where each row is built: whether the row
//! joins the row above it (and with what separator) and how many leading
//! columns are render chrome.

use super::chat::{
    ScrolledRenderData, build_base_spinner, build_scrolled_render_data,
    chat_selection_snapshot_needed, render_lines_from_paragraph, sync_chat_layout,
};
use crate::app::selection::{normalize_selection, slice_by_display_cols};
use crate::app::{App, SelectionState};
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// How a rendered row joins the row above it when copied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CopyJoin {
    /// Row begins a source line: a newline joins it to the row above.
    HardLine,
    /// Row continues the row above. The separator is the whitespace the
    /// wrapper consumed at the break; empty when the break split a token.
    Soft { separator: String },
    /// Render chrome: the row copies as nothing.
    Chrome,
}

/// Per-row copy provenance, one entry per rendered row of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CopyRowMeta {
    pub(crate) join: CopyJoin,
    pub(crate) chrome_cols: u16,
}

impl CopyRowMeta {
    pub(crate) fn hard_line() -> Self {
        Self { join: CopyJoin::HardLine, chrome_cols: 0 }
    }

    pub(crate) fn soft(separator: String, chrome_cols: u16) -> Self {
        Self { join: CopyJoin::Soft { separator }, chrome_cols }
    }

    pub(crate) fn chrome(chrome_cols: u16) -> Self {
        Self { join: CopyJoin::Chrome, chrome_cols }
    }

    /// Widen the chrome prefix by `cols` columns; the gutter row prefix is
    /// added by the caller that prepends it.
    pub(crate) fn offset_chrome(mut self, cols: u16) -> Self {
        self.chrome_cols += cols;
        self
    }
}

/// Separators that rejoin a logical line's wrapped visual rows: the
/// whitespace the wrapper consumed at each break, empty for a break inside
/// a token. `rows` are the row texts in order; the result has one entry per
/// row after the first.
pub(crate) fn wrap_join_separators(logical: &str, rows: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rows.len().saturating_sub(1));
    let mut cursor = 0usize;
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            let separator = if row.chars().next().is_some_and(char::is_whitespace) {
                String::new()
            } else if char_at_display_col(logical, cursor).is_some_and(char::is_whitespace) {
                " ".to_owned()
            } else {
                String::new()
            };
            if separator == " " {
                let run: usize = logical[cursor.min(logical.len())..]
                    .chars()
                    .take_while(|ch| ch.is_whitespace())
                    .map(char::len_utf8)
                    .sum();
                cursor += run;
            }
            out.push(separator);
        }
        cursor = advance_display_cols(logical, cursor, super::wrap::display_width(row));
    }
    out
}

/// The character at a display column, or `None` past the end.
fn char_at_display_col(text: &str, col: usize) -> Option<char> {
    let mut width = 0usize;
    for ch in text.chars() {
        if width >= col {
            return Some(ch);
        }
        width += char_width(ch);
    }
    None
}

/// Advance a byte cursor into `text` by `cols` display columns.
fn advance_display_cols(text: &str, byte: usize, cols: usize) -> usize {
    let mut width = 0usize;
    let mut end = byte.min(text.len());
    for ch in text[end..].chars() {
        if width >= cols {
            break;
        }
        width += char_width(ch);
        end += ch.len_utf8();
    }
    end
}

fn char_width(ch: char) -> usize {
    unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// The clipboard text for a chat selection: the text behind the render.
pub(crate) fn chat_selection_text(app: &mut App, selection: SelectionState) -> Option<String> {
    if !chat_selection_snapshot_needed(Some(selection)) {
        return None;
    }
    let area = app.rendered_chat_area;
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let base_spinner = build_base_spinner(app);
    let content_height = sync_chat_layout(app, area, &base_spinner);
    let render_data = build_scrolled_render_data(
        app,
        &base_spinner,
        area.width,
        content_height,
        usize::from(area.height),
    );
    app.rendered_chat_area = area;

    let rows = viewport_copy_rows(&render_data, area);
    let text = join_selection(&rows, selection);
    (!text.is_empty()).then_some(text)
}

/// One viewport row ready to copy from.
struct ViewportRow {
    text: String,
    meta: Option<CopyRowMeta>,
}

/// Pair every viewport row with its copy provenance: builder metas for the
/// first visual row of each logical row, derived soft-join metas for rows
/// the paragraph wrapped at render time.
fn viewport_copy_rows(render_data: &ScrolledRenderData, area: Rect) -> Vec<ViewportRow> {
    let texts =
        render_lines_from_paragraph(&render_data.paragraph, area, render_data.stats.local_scroll);
    let mut metas: Vec<Option<CopyRowMeta>> = vec![None; area.height as usize];

    let width = area.width;
    let mut paragraph_row = 0usize;
    for (line, meta) in render_data.all_lines.iter().zip(render_data.stats.copy_rows.iter()) {
        let count = line_visual_row_count(line, width);
        let subrows = if count > 1 { wrapped_row_texts(line, width, count) } else { Vec::new() };
        let separators = wrap_join_separators(&line_text(line), &subrows);
        for s in 0..count {
            let global = paragraph_row + s;
            if global < render_data.stats.local_scroll {
                continue;
            }
            let viewport_row = global - render_data.stats.local_scroll;
            if viewport_row >= metas.len() {
                break;
            }
            metas[viewport_row] = Some(if s > 0 && !matches!(meta.join, CopyJoin::Chrome) {
                CopyRowMeta::soft(
                    separators.get(s - 1).cloned().unwrap_or_default(),
                    meta.chrome_cols,
                )
            } else {
                meta.clone()
            });
        }
        paragraph_row += count;
        if paragraph_row.saturating_sub(render_data.stats.local_scroll) >= metas.len() {
            break;
        }
    }

    texts.into_iter().zip(metas).map(|(text, meta)| ViewportRow { text, meta }).collect()
}

/// How many visual rows the paragraph renders `line` into at `width`.
fn line_visual_row_count(line: &ratatui::text::Line<'_>, width: u16) -> usize {
    Paragraph::new(Text::from(vec![line.clone()]))
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

/// The visual rows the paragraph wraps `line` into at `width`, read back
/// from a scratch buffer so they match the chat render exactly.
fn wrapped_row_texts(line: &ratatui::text::Line<'_>, width: u16, count: usize) -> Vec<String> {
    let height = u16::try_from(count).unwrap_or(u16::MAX);
    let area = Rect { x: 0, y: 0, width, height };
    let mut buf = ratatui::buffer::Buffer::empty(area);
    Paragraph::new(Text::from(vec![line.clone()]))
        .wrap(Wrap { trim: false })
        .render(area, &mut buf);
    (0..height)
        .map(|y| {
            let mut text = String::new();
            for x in 0..width {
                if let Some(cell) = buf.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
            text.trim_end().to_owned()
        })
        .collect()
}

fn line_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|span| span.content.as_ref()).collect()
}

/// Join the selected viewport rows into the clipboard payload.
fn join_selection(rows: &[ViewportRow], selection: SelectionState) -> String {
    let (start, end) = normalize_selection(selection.start, selection.end);
    if start.row >= rows.len() {
        return String::new();
    }
    let last_row = end.row.min(rows.len().saturating_sub(1));

    let mut out = String::new();
    for row in start.row..=last_row {
        let Some(entry) = rows.get(row) else { continue };
        let Some(meta) = &entry.meta else { continue };
        match &meta.join {
            CopyJoin::Chrome => continue,
            CopyJoin::HardLine => {
                if !out.is_empty() {
                    out.push('\n');
                }
            }
            CopyJoin::Soft { separator } => {
                if !out.is_empty() {
                    out.push_str(separator);
                }
            }
        }

        let row_start = if row == start.row { start.col } else { 0 };
        let row_end = if row == end.row { end.col } else { usize::MAX };
        // Selection columns are screen columns; the chrome prefix has no
        // place in the payload, so the slice starts past it.
        let chrome = usize::from(meta.chrome_cols);
        out.push_str(&slice_by_display_cols(
            &entry.text,
            row_start.max(chrome),
            row_end.max(chrome),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        MessageBlock, MessageRole, SelectionKind, SelectionPoint, SystemSeverity, TextBlock,
    };
    use crate::ui::chat::{chat_content_area, render_scrolled, update_visual_heights};
    use crate::ui::message::SpinnerState;
    use pretty_assertions::assert_eq;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    // =====
    // TESTS: 12
    // =====

    fn chat_message(role: MessageRole, text: &str) -> crate::app::ChatMessage {
        crate::app::ChatMessage::new(role, vec![MessageBlock::Text(TextBlock::from_complete(text))])
    }

    fn idle_spinner() -> SpinnerState {
        SpinnerState {
            glyph: '\u{280B}',
            is_active_turn_assistant: false,
            show_empty_thinking: false,
            show_thinking: false,
            show_compacting: false,
            live_turn_running: false,
        }
    }

    /// Draw the chat once so the app holds the rendered area and viewport
    /// state the copy path snapshots from, exactly as the live render does.
    fn draw_chat(app: &mut App, width: u16, height: u16) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let spinner = idle_spinner();
                let content_area = chat_content_area(Rect::new(0, 0, width, height));
                let _ = app.active_viewport_mut().on_frame(content_area.width, content_area.height);
                update_visual_heights(
                    app,
                    &spinner,
                    content_area.width,
                    usize::from(content_area.height),
                );
                app.active_viewport_mut().rebuild_prefix_sums();
                let total_h = app.viewport().total_message_height();
                render_scrolled(
                    frame,
                    content_area,
                    app,
                    &spinner,
                    content_area.width,
                    total_h,
                    usize::from(content_area.height),
                );
            })
            .expect("draw");
    }

    /// Copy the whole chat, top row to the last content row.
    fn copy_all(app: &mut App, height: u16) -> Option<String> {
        let selection = SelectionState {
            kind: SelectionKind::Chat,
            start: SelectionPoint { row: 0, col: 0 },
            end: SelectionPoint { row: usize::from(height), col: 400 },
            dragging: false,
        };
        chat_selection_text(app, selection)
    }

    fn tool_call_message(text: &str) -> crate::app::ChatMessage {
        let tc = crate::app::ToolCallInfo {
            id: "toolu_1".to_owned(),
            title: "toolu_1".to_owned(),
            sdk_tool_name: "Read".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: crate::agent::model::ToolCallStatus::Completed,
            content: vec![crate::agent::model::RenderToolCallContent::from(text.to_owned())],
            hidden: false,
            terminal_id: None,
            terminal_output: None,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
            cache: crate::app::BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        };
        crate::app::ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc))],
        )
    }

    /// A user paragraph that soft-wraps at the gutter-narrowed width copies
    /// as the sentence it renders, with the break space restored and no
    /// chrome.
    #[test]
    fn a_wrapped_user_paragraph_copies_as_one_line() {
        let mut app = App::test_default();
        *app.active_messages_mut() =
            vec![chat_message(MessageRole::User, "the quick brown fox jumps over the lazy dog")];
        draw_chat(&mut app, 31, 12);

        assert_eq!(
            copy_all(&mut app, 12),
            Some("the quick brown fox jumps over the lazy dog".to_owned()),
        );
    }

    /// A newline the user typed is a real line break on the grid and stays
    /// one on the clipboard.
    #[test]
    fn a_user_newline_survives_copy() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![chat_message(MessageRole::User, "one\ntwo")];
        draw_chat(&mut app, 31, 12);

        assert_eq!(copy_all(&mut app, 12), Some("one\ntwo".to_owned()));
    }

    /// A fenced block copies as its source: no panel pad, no language
    /// label, soft-wrapped code lines joined back into the source line,
    /// blank code lines kept.
    #[test]
    fn a_code_block_copies_verbatim() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![chat_message(
            MessageRole::User,
            "look:\n```rust\nlet value = some_function(argument_one, argument_two);\n\nlet b = 2;\n```",
        )];
        draw_chat(&mut app, 41, 24);

        assert_eq!(
            copy_all(&mut app, 24),
            Some(
                "look:\n\nlet value = some_function(argument_one, argument_two);\n\nlet b = 2;"
                    .to_owned()
            ),
        );
    }

    /// A wrapped code line's indentation survives once: the panel re-emits
    /// it on every wrapped row for alignment, and the copy must not repeat
    /// it at the join.
    #[test]
    fn a_wrapped_indented_code_line_keeps_its_indent_once() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![chat_message(
            MessageRole::User,
            "```rust\n    let value = some_function(argument_one, argument_two);\n```",
        )];
        draw_chat(&mut app, 31, 24);

        assert_eq!(
            copy_all(&mut app, 24),
            Some("    let value = some_function(argument_one, argument_two);".to_owned()),
        );
    }

    /// A wrapped nested list item's hanging indent is visual alignment:
    /// the copy rejoins the wrapped rows into the item's one line, with
    /// the indent kept once.
    #[test]
    fn a_wrapped_list_item_copies_as_its_source_line() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![chat_message(
            MessageRole::User,
            "- top level item\n  - nested child entry with a long text that wraps here",
        )];
        draw_chat(&mut app, 31, 24);

        assert_eq!(
            copy_all(&mut app, 24),
            Some(
                "- top level item\n    - nested child entry with a long text that wraps here"
                    .to_owned()
            ),
        );
    }

    /// The `User` label is chrome: selecting from the turn's first row
    /// copies only the turn's text.
    #[test]
    fn the_user_label_does_not_copy() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![chat_message(MessageRole::User, "hello")];
        draw_chat(&mut app, 31, 12);

        assert_eq!(copy_all(&mut app, 12), Some("hello".to_owned()));
    }

    /// Assistant prose the paragraph wrapped at render time copies as the
    /// flowing sentence, the break space restored.
    #[test]
    fn a_wrapped_assistant_paragraph_copies_as_one_line() {
        let mut app = App::test_default();
        *app.active_messages_mut() =
            vec![chat_message(MessageRole::Assistant, "alpha beta gamma delta")];
        draw_chat(&mut app, 13, 12);

        assert_eq!(copy_all(&mut app, 12), Some("alpha beta gamma delta".to_owned()));
    }

    /// Assistant paragraphs render as separate rows and copy with the real
    /// newline between them.
    #[test]
    fn assistant_paragraphs_copy_as_separate_lines() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![chat_message(MessageRole::Assistant, "one\n\ntwo")];
        draw_chat(&mut app, 31, 12);

        assert_eq!(copy_all(&mut app, 12), Some("one\ntwo".to_owned()));
    }

    /// Tool body rows carry the `  │  ` / `  └─ ` prefix as chrome; the
    /// copied body is the content under it.
    #[test]
    fn tool_body_prefixes_do_not_copy() {
        let mut app = App::test_default();
        app.tools_collapsed = false;
        *app.active_messages_mut() =
            vec![tool_call_message("first output line\nsecond output line")];
        draw_chat(&mut app, 41, 12);

        assert_eq!(
            copy_all(&mut app, 12),
            Some("first output line\nsecond output line".to_owned()),
        );
    }

    /// A selection that starts partway into a row slices the content past
    /// the chrome: column coordinates are screen columns, the payload is
    /// clean text.
    #[test]
    fn a_partial_row_selection_slices_past_the_chrome() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![chat_message(MessageRole::User, "hello world")];
        draw_chat(&mut app, 31, 12);

        let selection = SelectionState {
            kind: SelectionKind::Chat,
            start: SelectionPoint { row: 1, col: 0 },
            end: SelectionPoint { row: 1, col: 4 },
            dragging: false,
        };
        assert_eq!(chat_selection_text(&mut app, selection), Some("he".to_owned()));
    }

    /// The gutter's two columns are chrome on every user row, so a
    /// selection that starts inside them takes the row from its text.
    #[test]
    fn a_selection_starting_in_the_gutter_takes_the_whole_text() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![chat_message(MessageRole::User, "hello")];
        draw_chat(&mut app, 31, 12);

        let selection = SelectionState {
            kind: SelectionKind::Chat,
            start: SelectionPoint { row: 1, col: 1 },
            end: SelectionPoint { row: 1, col: 400 },
            dragging: false,
        };
        assert_eq!(chat_selection_text(&mut app, selection), Some("hello".to_owned()));
    }

    /// Selecting a wrapped paragraph's second visual row alone copies just
    /// that row's text: a partial-logical-line selection does not reach for
    /// the rows around it.
    #[test]
    fn a_selection_stopped_on_a_continuation_row_takes_just_that_row() {
        let mut app = App::test_default();
        *app.active_messages_mut() =
            vec![chat_message(MessageRole::User, "the quick brown fox jumps over the lazy dog")];
        draw_chat(&mut app, 31, 12);

        let selection = SelectionState {
            kind: SelectionKind::Chat,
            start: SelectionPoint { row: 2, col: 0 },
            end: SelectionPoint { row: 2, col: 400 },
            dragging: false,
        };
        assert_eq!(chat_selection_text(&mut app, selection), Some("over the lazy dog".to_owned()));
    }

    #[test]
    fn an_empty_chat_copies_nothing() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![crate::app::ChatMessage::new(
            MessageRole::System(Some(SystemSeverity::Info)),
            Vec::new(),
        )];
        draw_chat(&mut app, 31, 12);

        assert_eq!(copy_all(&mut app, 12), None);
    }

    /// Mid-token breaks join with nothing: a wrapped URL comes back as the
    /// single token it is.
    #[test]
    fn wrap_join_separators_restore_consumed_whitespace_only() {
        let rows = |items: &[&str]| items.iter().copied().map(str::to_string).collect::<Vec<_>>();
        assert_eq!(
            wrap_join_separators("alpha beta gamma", &rows(&["alpha", "beta", "gamma"])),
            vec![" ".to_owned(), " ".to_owned()]
        );
        assert_eq!(
            wrap_join_separators(
                "https://example.com/path",
                &rows(&["https://exam", "ple.com/path"])
            ),
            vec![String::new()]
        );
        assert_eq!(wrap_join_separators("single", &rows(&["single"])), Vec::<String>::new());
        // A continuation row that starts with the kept whitespace carries
        // its own separator.
        assert_eq!(
            wrap_join_separators("aaa   bbb", &rows(&["aaa", "   bbb"])),
            vec![String::new()]
        );
    }
}
