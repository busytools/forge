//! Unified prompt widget renderer. Renders the dock when a prompt is
//! active in the session's queue.

use crate::app::{PromptMode, PromptSource, PromptState};
use crate::ui::theme;
use crate::ui::wrap::wrap_plain;
use forge_primitives::permission_ui::PermissionOptionKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

/// Render the prompt into `area` (the chat-input box's rect). The
/// orange thick chrome is drawn here too - the caller does NOT render
/// its own block first.
pub fn render(
    area: Rect,
    buf: &mut Buffer,
    prompt: &PromptState,
    queue_depth: usize,
    notes_text: Option<&str>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD))
        .padding(Padding::horizontal(2));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let lines = build_lines(prompt, queue_depth, inner.width as usize, notes_text);
    Paragraph::new(lines).render(inner, buf);
}

/// Total rows the prompt widget needs when rendered for `prompt` at
/// `queue_depth` inside an area of `area_width` columns. Includes the
/// inner content (after pre-wrapping at the inner width) + 2 chrome
/// rows (top border + bottom border). Used by
/// `ui::input::visual_line_count` to grow the dock to fit the morphed
/// prompt instead of clipping it to the chat-input editor's default
/// height.
pub fn prompt_required_lines(
    prompt: &PromptState,
    queue_depth: usize,
    area_width: u16,
    notes_text: Option<&str>,
) -> u16 {
    // Block borders eat 2 cols; Padding::horizontal(2) eats 4 more.
    let inner_width = area_width.saturating_sub(6).max(1) as usize;
    let lines = build_lines(prompt, queue_depth, inner_width, notes_text);
    u16::try_from(lines.len().saturating_add(2)).unwrap_or(u16::MAX)
}

fn build_lines(
    prompt: &PromptState,
    queue_depth: usize,
    content_width: usize,
    notes_text: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // 1 empty row top.
    lines.push(Line::default());

    // Queue-depth indicator.
    if queue_depth > 1 {
        lines.push(Line::from(vec![Span::styled(
            format!("▼ {} more pending after this", queue_depth - 1),
            Style::default().fg(theme::DIM),
        )]));
    }

    // Header lines.
    lines.extend(build_header_lines(prompt, content_width));
    lines.push(Line::default());

    // Options stack.
    lines.extend(build_option_lines(prompt, content_width, notes_text));

    // Footer hint.
    lines.push(Line::default());
    lines.push(build_footer_line(prompt));

    // 1 empty row bottom.
    lines.push(Line::default());
    lines
}

fn build_header_lines(prompt: &PromptState, content_width: usize) -> Vec<Line<'static>> {
    match &prompt.source {
        PromptSource::Permission {
            display_title,
            decision_reason,
            display_description,
            tool_name,
            tool_args_summary,
            ..
        } => {
            let mut out = Vec::new();
            let header_text = if tool_args_summary.is_empty() {
                tool_name.clone()
            } else {
                format!("{tool_name} · {tool_args_summary}")
            };
            // display_title overrides the tool name when distinct.
            let title_owned: String = display_title
                .as_deref()
                .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case(tool_name))
                .map_or(header_text, String::from);
            for row in wrap_plain(&title_owned, content_width) {
                out.push(Line::from(Span::styled(
                    row,
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                )));
            }
            // Yellow ⚠ decision_reason.
            if let Some(reason) = decision_reason.as_deref().filter(|r| !r.is_empty()) {
                for row in wrap_plain(&format!("⚠ {reason}"), content_width) {
                    out.push(Line::from(Span::styled(row, Style::default().fg(Color::Yellow))));
                }
            }
            // Dim display_description.
            if let Some(desc) = display_description.as_deref().filter(|d| !d.is_empty()) {
                for row in wrap_plain(desc, content_width) {
                    out.push(Line::from(Span::styled(row, Style::default().fg(theme::DIM))));
                }
            }
            out
        }
        PromptSource::Question { prompt: q, question_index, total_questions } => {
            let mut out = Vec::new();
            let progress = if *total_questions > 1 {
                format!(" (Q{} of {})", question_index + 1, total_questions)
            } else {
                String::new()
            };
            // Header: "? <header><progress>". The "? " glyph is part of
            // the visible first row only; continuation rows of a wrapped
            // header indent past the "? " prefix to keep alignment.
            let prefix = "? ";
            let prefix_w = UnicodeWidthStr::width(prefix);
            let header_text = format!("{}{}", q.header, progress);
            let header_rows =
                wrap_plain(&header_text, content_width.saturating_sub(prefix_w).max(1));
            for (i, row) in header_rows.into_iter().enumerate() {
                if i == 0 {
                    out.push(Line::from(vec![
                        Span::styled(prefix.to_string(), Style::default().fg(theme::RUST_ORANGE)),
                        Span::styled(
                            row,
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                } else {
                    out.push(Line::from(vec![
                        Span::raw(" ".repeat(prefix_w)),
                        Span::styled(
                            row,
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
            }
            // Body: each newline-separated chunk is word-wrapped
            // independently to preserve user paragraph breaks.
            for chunk in q.question.lines() {
                if chunk.is_empty() {
                    out.push(Line::default());
                    continue;
                }
                for row in wrap_plain(chunk, content_width) {
                    out.push(Line::from(Span::styled(row, Style::default().fg(Color::White))));
                }
            }
            out
        }
    }
}

fn build_option_lines(
    prompt: &PromptState,
    content_width: usize,
    notes_text: Option<&str>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let is_multi = prompt.is_multi_select();
    // For Question prompts, keep a reference to the wire options so
    // we can read .description and .preview by index.
    let question_options: Option<&[forge_primitives::question::QuestionOption]> =
        match &prompt.source {
            PromptSource::Question { prompt: q, .. } => Some(&q.options),
            PromptSource::Permission { .. } => None,
        };

    // Display-only: in multi-select, the Notes/"Other" row reads as
    // checked whenever the user has typed non-empty text into the
    // free-text buffer, so the live option list confirms the typed
    // content will be included on submit. The wire (`annotation.notes`)
    // carries the typed text independently of `selected_option_indices`,
    // so this never mutates the selection set.
    let notes_has_text = notes_text.is_some_and(|t| !t.trim().is_empty());
    for (i, opt) in prompt.options.iter().enumerate() {
        let is_focused = i == prompt.focused_option_index;
        let is_notes_kind = matches!(opt.kind, PermissionOptionKind::Notes);
        let is_toggled = prompt.selected_option_indices.contains(&i)
            || (is_multi && is_notes_kind && notes_has_text);
        let (icon, icon_color) = icon_for_kind(opt.kind);
        let pointer = if is_focused { "▸ " } else { "  " };
        let pointer_style = if is_focused {
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // #273: bold recommended AskUserQuestion options even when
        // unfocused so the (Recommended) signal survives the suffix
        // strip. Focused options stay BOLD + white as before.
        let name_style = if is_focused {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else if opt.recommended {
            Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let mut spans: Vec<Span<'static>> = vec![Span::styled(pointer.to_string(), pointer_style)];
        let checkbox_str = if is_multi { if is_toggled { "[x] " } else { "[ ] " } } else { "" };
        if is_multi {
            let checkbox_style = if is_toggled {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(theme::DIM)
            };
            spans.push(Span::styled(checkbox_str.to_string(), checkbox_style));
        }
        let icon_str = format!("{icon} ");
        let prefix_width = UnicodeWidthStr::width(pointer)
            + UnicodeWidthStr::width(checkbox_str)
            + UnicodeWidthStr::width(icon_str.as_str());
        spans.push(Span::styled(icon_str.clone(), Style::default().fg(icon_color)));
        // Word-wrap the option name with hanging indent so a long name's
        // continuation rows sit under the name's first column.
        let name_rows = wrap_plain(&opt.name, content_width.saturating_sub(prefix_width).max(1));
        for (row_idx, row) in name_rows.into_iter().enumerate() {
            if row_idx == 0 {
                spans.push(Span::styled(row, name_style));
                lines.push(Line::from(std::mem::take(&mut spans)));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(prefix_width)),
                    Span::styled(row, name_style),
                ]));
            }
        }

        // Question-specific: render the option's description as dim
        // subtext below the option label (if present and non-empty).
        // Indent matches the display width of pointer + checkbox + icon
        // so the description aligns with the option name. Description
        // is pre-wrapped so continuation rows preserve the indent.
        if let Some(q_opts) = question_options
            && let Some(q_opt) = q_opts.get(i)
            && let Some(desc) = q_opt.description.as_deref().filter(|d| !d.is_empty())
        {
            let indent = " ".repeat(prefix_width);
            for row in wrap_plain(desc, content_width.saturating_sub(prefix_width).max(1)) {
                lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(row, Style::default().fg(theme::DIM)),
                ]));
            }
        }

        // Notes-kind focused: surface the canonical chat-input editor
        // inline under the option. Keystrokes are already routed to
        // App.input by dispatch_key; this just shows what the user
        // typed (or a placeholder when empty). Indent matches the
        // option name column.
        if is_focused && matches!(opt.kind, PermissionOptionKind::Notes) {
            let indent = " ".repeat(prefix_width);
            let inner_width = content_width.saturating_sub(prefix_width).max(1);
            let raw = notes_text.unwrap_or("");
            if raw.is_empty() {
                lines.push(Line::from(vec![
                    Span::raw(indent.clone()),
                    Span::styled(
                        "Type your message and press Enter to send.".to_string(),
                        Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
                    ),
                ]));
            } else {
                for chunk in raw.split('\n') {
                    if chunk.is_empty() {
                        lines.push(Line::from(Span::raw(indent.clone())));
                        continue;
                    }
                    for row in wrap_plain(chunk, inner_width) {
                        lines.push(Line::from(vec![
                            Span::raw(indent.clone()),
                            Span::styled(row, Style::default().fg(Color::White)),
                        ]));
                    }
                }
            }
        }
    }

    // Question-specific: render the per-focused-option preview block
    // AFTER all options. Renders only when the source is Question and
    // the focused option has a non-empty preview.
    if let Some(q_opts) = question_options
        && let Some(q_opt) = q_opts.get(prompt.focused_option_index)
        && let Some(preview) = q_opt.preview.as_deref().filter(|p| !p.trim().is_empty())
    {
        lines.push(Line::default());
        for row in wrap_plain("Preview:", content_width) {
            lines.push(Line::from(Span::styled(
                row,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )));
        }
        for chunk in preview.lines() {
            if chunk.is_empty() {
                lines.push(Line::default());
                continue;
            }
            for row in wrap_plain(chunk, content_width) {
                lines.push(Line::from(Span::styled(row, Style::default().fg(theme::DIM))));
            }
        }
    }

    lines
}

fn build_footer_line(prompt: &PromptState) -> Line<'static> {
    let text = match prompt.mode {
        PromptMode::OptionPicker => {
            if prompt.is_multi_select() {
                "space toggle  ↑↓ move  ⏎ submit  esc cancel"
            } else {
                "↑↓ select  ⏎ confirm  esc reject"
            }
        }
        PromptMode::EditingInput => "⏎ submit  esc back to options",
    };
    Line::from(Span::styled(text.to_owned(), Style::default().fg(theme::DIM)))
}

fn icon_for_kind(kind: PermissionOptionKind) -> (&'static str, Color) {
    match kind {
        PermissionOptionKind::Allow => ("✓", Color::Green),
        PermissionOptionKind::Deny => ("✗", Color::Red),
        PermissionOptionKind::Edit => ("✎", Color::Blue),
        PermissionOptionKind::Notes => ("…", theme::DIM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::prompt::tests::{make_permission_request, make_question_request};

    fn render_to_string(
        prompt: &PromptState,
        queue_depth: usize,
        width: u16,
        height: u16,
    ) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(area, &mut buf, prompt, queue_depth, None);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Display column of `target` in `s`, summing UnicodeWidthChar
    /// widths of every char before it.
    fn display_col(s: &str, target: char) -> Option<usize> {
        let mut col = 0;
        for c in s.chars() {
            if c == target {
                return Some(col);
            }
            col += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
        None
    }

    /// A question confirms on the first Enter, so both footers offer
    /// it with no arrow or space pressed first.
    #[test]
    fn footer_offers_enter_on_an_untouched_question() {
        let single = PromptState::from_question("tc-q".into(), make_question_request(false));
        let single_out = render_to_string(&single, 1, 70, 14);
        assert!(
            single_out.contains("⏎ confirm"),
            "an untouched single-select question offers Enter; got:\n{single_out}",
        );

        let multi = PromptState::from_question("tc-q".into(), make_question_request(true));
        let multi_out = render_to_string(&multi, 1, 70, 14);
        assert!(
            multi_out.contains("⏎ submit"),
            "an untouched multi-select question offers Enter; got:\n{multi_out}",
        );
    }

    #[test]
    fn renders_thick_orange_chrome_corners() {
        let prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let out = render_to_string(&prompt, 1, 60, 12);
        let lines: Vec<&str> = out.lines().collect();
        let first = lines.first().expect("first line");
        let last = lines.last().expect("last line");
        assert!(
            first.starts_with('┏'),
            "first line should start with thick top-left corner; got: {first}"
        );
        assert!(
            first.ends_with('┓'),
            "first line should end with thick top-right corner; got: {first}"
        );
        assert!(
            last.starts_with('┗'),
            "last line should start with thick bottom-left corner; got: {last}"
        );
        assert!(
            last.ends_with('┛'),
            "last line should end with thick bottom-right corner; got: {last}"
        );
    }

    #[test]
    fn renders_left_and_right_thick_borders_on_content_rows() {
        let prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let out = render_to_string(&prompt, 1, 60, 12);
        let lines: Vec<&str> = out.lines().collect();
        // Skip top + bottom rules.
        for line in lines.iter().skip(1).take(lines.len() - 2) {
            assert!(
                line.starts_with('┃'),
                "content row should start with thick vertical; got: {line}"
            );
            assert!(line.ends_with('┃'), "content row should end with thick vertical; got: {line}");
        }
    }

    #[test]
    fn renders_tool_name_and_args_in_header() {
        let prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let out = render_to_string(&prompt, 1, 80, 12);
        assert!(out.contains("Bash"), "expected Bash in output:\n{out}");
        assert!(out.contains("git push"), "expected git push in output:\n{out}");
    }

    #[test]
    fn renders_pointer_on_focused_option() {
        let prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let out = render_to_string(&prompt, 1, 80, 12);
        assert!(
            out.contains("▸ ✓ Allow once"),
            "expected ▸ ✓ Allow once on focused row; got:\n{out}"
        );
    }

    #[test]
    fn queue_depth_greater_than_one_renders_indicator() {
        let prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let out = render_to_string(&prompt, 3, 80, 14);
        assert!(
            out.contains("▼ 2 more pending after this"),
            "expected queue indicator; got:\n{out}"
        );
    }

    #[test]
    fn queue_depth_one_omits_indicator() {
        let prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let out = render_to_string(&prompt, 1, 80, 12);
        assert!(!out.contains("more pending"), "queue indicator should be hidden; got:\n{out}");
    }

    #[test]
    fn recommended_option_renders_with_bold_modifier_even_when_unfocused() {
        // #273: an option flagged `recommended` keeps BOLD styling on
        // its label even when the cursor is on a different option, so
        // the visual signal that survived the suffix strip stays
        // visible. Focused-row BOLD is already exercised by the
        // pointer test; this asserts the recommended-row BOLD path.
        let mut request = make_question_request(false);
        request.prompt.options[1].recommended = true; // Blue is recommended
        let mut prompt = PromptState::from_question("tc-q".into(), request);
        // Move focus to the first option (Red) so the recommended
        // option (Blue) is unfocused.
        prompt.focused_option_index = 0;
        // Render into a buffer so we can inspect cell modifiers.
        let area = Rect::new(0, 0, 80, 14);
        let mut buf = Buffer::empty(area);
        render(area, &mut buf, &prompt, 1, None);
        // Locate the `B` of "Blue" on its row.
        let mut found = false;
        for y in 0..area.height {
            for x in 0..area.width {
                if buf[(x, y)].symbol() == "B" {
                    let style = buf[(x, y)].style();
                    assert!(
                        style.add_modifier.contains(Modifier::BOLD),
                        "recommended unfocused row must be bold; got {style:?}",
                    );
                    found = true;
                    break;
                }
            }
            if found {
                break;
            }
        }
        assert!(found, "expected to find Blue option in rendered buffer");
    }

    #[test]
    fn multi_select_renders_checkbox_markers() {
        let request = make_question_request(true);
        let mut prompt = PromptState::from_question("tc-q".into(), request);
        prompt.selected_option_indices.insert(0);
        let out = render_to_string(&prompt, 1, 80, 14);
        assert!(out.contains("[x] ✓ Red"), "expected [x] on toggled option; got:\n{out}");
        assert!(out.contains("[ ] ✓ Blue"), "expected [ ] on untoggled option; got:\n{out}");
    }

    /// Fix 3: in multiSelect mode, when the user types non-empty text
    /// into the "Other" / Notes free-text field, the Notes option's
    /// row MUST render as checked so the user sees the typed content
    /// will be included on submit. The wire already carries
    /// `annotation.notes` independently, so this is display-only - the
    /// `selected_option_indices` set is NOT mutated.
    #[test]
    fn multi_select_notes_option_renders_checked_when_user_typed() {
        let request = make_question_request(true);
        let prompt = PromptState::from_question("tc-q".into(), request);
        // Sanity: no picks; only a non-empty notes buffer.
        assert!(prompt.selected_option_indices.is_empty());
        let area = Rect::new(0, 0, 80, 18);
        let mut buf = Buffer::empty(area);
        render(area, &mut buf, &prompt, 1, Some("a note about etiquette"));
        let out: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            out.contains("[x] … Tell Claude something else"),
            "the Notes/Other row must render as [x] when the user has typed text; got:\n{out}",
        );
    }

    /// Sibling regression: when the notes buffer is empty (default
    /// state with no typed text), the Notes row MUST stay unchecked.
    #[test]
    fn multi_select_notes_option_renders_unchecked_when_user_has_not_typed() {
        let request = make_question_request(true);
        let prompt = PromptState::from_question("tc-q".into(), request);
        let area = Rect::new(0, 0, 80, 18);
        let mut buf = Buffer::empty(area);
        render(area, &mut buf, &prompt, 1, None);
        let out: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            out.contains("[ ] … Tell Claude something else"),
            "Notes/Other row must stay [ ] when no text has been typed; got:\n{out}",
        );
    }

    #[test]
    fn dock_height_matches_prompt_required_lines_with_realistic_content() {
        // The dock area allocated by `prompt_required_lines(...)` must
        // exactly contain build_lines's output PLUS top + bottom thick
        // borders. The first content row (after the top border) and the
        // last content row (before the bottom border) must BOTH be the
        // top_empty / bottom_empty padding rows (i.e. blank inside the
        // box).
        let mut request = make_question_request(false);
        request.prompt.options[0].description = Some(
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu \
             nu xi omicron pi rho sigma tau upsilon"
                .into(),
        );
        request.prompt.options[1].description =
            Some("short description for second option that wraps just a little".into());
        let prompt = PromptState::from_question("tc-q".into(), request);
        let area_width = 60u16;
        let required = prompt_required_lines(&prompt, 1, area_width, None);
        let out = render_to_string(&prompt, 1, area_width, required);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            u16::try_from(lines.len()).expect("height fits u16"),
            required,
            "rendered height should match required"
        );
        // First row = top border (┏...┓). Second row = top padding (blank inside box).
        assert!(lines[0].starts_with('┏'), "row 0 should be top border, got: {}", lines[0]);
        let row1 = lines[1];
        assert!(
            row1.starts_with('┃')
                && row1.ends_with('┃')
                && row1[3..row1.len() - 3].trim().is_empty(),
            "row 1 should be top padding (blank inside the box), got: {row1}"
        );
        // Last row = bottom border. Second-to-last = bottom padding (blank inside box).
        let last = lines[lines.len() - 1];
        assert!(last.starts_with('┗'), "last row should be bottom border, got: {last}");
        let second_last = lines[lines.len() - 2];
        assert!(
            second_last.starts_with('┃')
                && second_last.ends_with('┃')
                && second_last[3..second_last.len() - 3].trim().is_empty(),
            "second-to-last row should be bottom padding (blank inside box), got: {second_last}"
        );
    }

    #[test]
    fn long_description_wraps_with_hanging_indent_matching_first_row() {
        // A long description that wraps onto 2+ rows should have every
        // continuation row's first non-blank column equal the first
        // row's first non-blank column. Asserts hanging-indent
        // preservation across the manual word-wrap path.
        let mut request = make_question_request(false);
        request.prompt.options[0].description = Some(
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu \
             nu xi omicron pi rho sigma tau upsilon phi chi psi omega"
                .into(),
        );
        let prompt = PromptState::from_question("tc-q".into(), request);
        // Narrow width to force wrap.
        let out = render_to_string(&prompt, 1, 50, 24);
        let lines: Vec<&str> = out.lines().collect();
        let desc_rows: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("alpha") || l.contains("lambda") || l.contains("upsilon"))
            .collect();
        assert!(
            desc_rows.len() >= 2,
            "expected the long description to wrap across 2+ visual rows; got:\n{out}"
        );
        let cols: Vec<usize> = desc_rows
            .iter()
            .map(|l| {
                l.chars()
                    .enumerate()
                    .find(|(_, c)| !matches!(*c, '┃' | ' '))
                    .map_or(usize::MAX, |(i, _)| i)
            })
            .collect();
        assert!(
            cols.iter().all(|&c| c == cols[0]),
            "description wrap continuation rows must share the same leading column; got {cols:?} for:\n{out}"
        );
    }

    #[test]
    fn description_left_column_equals_option_name_left_column() {
        // Description should align with the option NAME column, not the
        // icon column. Asserts both rows' first non-blank/non-border
        // character lands on the same DISPLAY column (not byte index).
        let mut request = make_question_request(false);
        request.prompt.options[0].description = Some("DESCSTART".into());
        let prompt = PromptState::from_question("tc-q".into(), request);
        let out = render_to_string(&prompt, 1, 80, 18);
        let lines: Vec<&str> = out.lines().collect();
        let opt_line = lines.iter().find(|l| l.contains("Red")).expect("option row");
        let desc_line = lines.iter().find(|l| l.contains("DESCSTART")).expect("desc row");
        let opt_name_col = display_col(opt_line, 'R').expect("Red in option row");
        let desc_col = display_col(desc_line, 'D').expect("DESCSTART in desc row");
        assert_eq!(
            opt_name_col, desc_col,
            "description first char must align with option name first char (display cols).\nname col {opt_name_col} in: {opt_line}\ndesc col {desc_col} in: {desc_line}"
        );
    }

    #[test]
    fn question_option_with_description_renders_dim_subtext() {
        let mut request = make_question_request(false);
        request.prompt.options[0].description = Some("matches chat input - loud, urgent".into());
        let prompt = PromptState::from_question("tc-q".into(), request);
        let out = render_to_string(&prompt, 1, 80, 18);
        assert!(
            out.contains("matches chat input - loud, urgent"),
            "expected option description in output:\n{out}"
        );
    }

    #[test]
    fn question_focused_option_with_preview_renders_inline_preview_block() {
        let mut request = make_question_request(false);
        request.prompt.options[0].preview =
            Some("Bash · git push origin polish\n▸ ✓ Allow once".into());
        let prompt = PromptState::from_question("tc-q".into(), request);
        let out = render_to_string(&prompt, 1, 80, 22);
        assert!(out.contains("Preview:"), "expected Preview header; got:\n{out}");
        assert!(
            out.contains("Bash · git push origin polish"),
            "expected preview content line 1; got:\n{out}"
        );
        assert!(out.contains("✓ Allow once"), "expected preview content line 2; got:\n{out}");
    }

    #[test]
    fn question_preview_only_shown_for_focused_option() {
        let mut request = make_question_request(false);
        request.prompt.options[0].preview = Some("First preview".into());
        request.prompt.options[1].preview = Some("Second preview".into());
        let mut prompt = PromptState::from_question("tc-q".into(), request);
        prompt.focused_option_index = 0;
        let out = render_to_string(&prompt, 1, 80, 20);
        assert!(out.contains("First preview"), "focused-option preview should render");
        assert!(!out.contains("Second preview"), "non-focused preview should NOT render");
    }

    #[test]
    fn long_question_body_wraps_with_aligned_continuation_indent() {
        // Single-line long body (no `\n`) must soft-wrap across multiple
        // visual rows whose leading non-blank column matches the first
        // row's - i.e. Block::Padding::horizontal(2) keeps every wrapped
        // continuation indented to the same left edge.
        let mut request = make_question_request(false);
        request.prompt.question =
            "AAA BBB CCC DDD EEE FFF GGG HHH III JJJ KKK LLL MMM NNN OOO PPP QQQ RRR SSS TTT UUU \
             VVV WWW XXX YYY ZZZ aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll mmm nnn ooo ppp \
             qqq rrr sss ttt uuu vvv www xxx yyy zzz 111 222 333 444 555 666 777 888 999 000."
                .to_string();
        let prompt = PromptState::from_question("tc-q".into(), request);
        let out = render_to_string(&prompt, 1, 60, 24);
        let lines: Vec<&str> = out.lines().collect();
        // A body row is any row containing one of the unique upper-case
        // word tokens AAA / BBB / ... ZZZ used in the question text.
        // AAA / NNN / aaa land on three distinct visual rows at width 60.
        let body_rows: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("AAA") || l.contains("NNN") || l.contains("aaa"))
            .collect();
        assert!(
            body_rows.len() >= 3,
            "expected the long body to wrap across 3+ visual rows at width 60; got:\n{out}"
        );
        let leading_cols: Vec<usize> = body_rows
            .iter()
            .map(|l| {
                l.chars()
                    .enumerate()
                    .find(|(_, c)| !matches!(*c, '┃' | ' '))
                    .map_or(usize::MAX, |(i, _)| i)
            })
            .collect();
        assert!(
            leading_cols.iter().all(|&c| c == leading_cols[0]),
            "wrapped continuation rows must share the same leading column; got {leading_cols:?} for:\n{out}"
        );
        assert_eq!(
            leading_cols[0], 3,
            "body should start at col 3 (border 0 + Padding::horizontal(2) cols 1-2); got col {} for:\n{out}",
            leading_cols[0]
        );
    }

    #[test]
    fn notes_option_focused_shows_inline_notes_text() {
        // When the "Tell Claude something else" option is focused, the
        // notes_text passed in by the caller renders inline under the
        // option label.
        let request = make_question_request(false);
        let mut prompt = PromptState::from_question("tc-q".into(), request);
        // Move focus to the last option (the synthesized Tell-Claude one).
        prompt.focused_option_index = prompt.options.len() - 1;
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render(area, &mut buf, &prompt, 1, Some("HELLO WORLD"));
        let out: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            out.contains("HELLO WORLD"),
            "notes text should render inline under focused Notes option; got:\n{out}"
        );
    }

    #[test]
    fn notes_option_focused_empty_shows_placeholder() {
        let request = make_question_request(false);
        let mut prompt = PromptState::from_question("tc-q".into(), request);
        prompt.focused_option_index = prompt.options.len() - 1;
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render(area, &mut buf, &prompt, 1, Some(""));
        let out: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            out.contains("Type your message"),
            "empty notes should render placeholder; got:\n{out}"
        );
    }

    #[test]
    fn notes_text_not_shown_when_notes_option_not_focused() {
        let request = make_question_request(false);
        let prompt = PromptState::from_question("tc-q".into(), request);
        // focus stays on option 0 (not Notes).
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        render(area, &mut buf, &prompt, 1, Some("SHOULD NOT APPEAR"));
        let out: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !out.contains("SHOULD NOT APPEAR"),
            "notes text must NOT render unless Notes option is focused; got:\n{out}"
        );
    }

    #[test]
    fn permission_prompt_does_not_render_preview_block() {
        let prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let out = render_to_string(&prompt, 1, 80, 14);
        assert!(!out.contains("Preview:"), "Permission prompts shouldn't render a Preview block");
    }
}
