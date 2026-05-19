//! Unified prompt widget renderer. Renders the dock when a prompt is
//! active in the session's queue. See spec §3.

use crate::app::{PromptMode, PromptSource, PromptState};
use crate::ui::theme;
use forge_primitives::permission_ui::PermissionOptionKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

/// Render the prompt into `area` (the chat-input box's rect). The
/// orange thick chrome is drawn here too — the caller does NOT render
/// its own block first.
pub fn render(area: Rect, buf: &mut Buffer, prompt: &PromptState, queue_depth: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let lines = build_lines(prompt, queue_depth);
    Paragraph::new(lines).render(inner, buf);
}

/// Total rows the prompt widget needs when rendered for `prompt` at
/// `queue_depth`. Includes the inner content + 2 chrome rows (top
/// border + bottom border). Used by `ui::input::visual_line_count`
/// to grow the dock to fit the morphed prompt instead of clipping
/// it to the chat-input editor's default height.
pub fn prompt_required_lines(prompt: &PromptState, queue_depth: usize) -> u16 {
    let inner = build_lines(prompt, queue_depth).len();
    u16::try_from(inner.saturating_add(2)).unwrap_or(u16::MAX)
}

fn build_lines(prompt: &PromptState, queue_depth: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // 1 empty row top (spec §3 inner padding).
    lines.push(Line::default());

    // Queue-depth indicator (spec §3.7).
    if queue_depth > 1 {
        lines.push(Line::from(vec![Span::styled(
            format!("  ▼ {} more pending after this", queue_depth - 1),
            Style::default().fg(theme::DIM),
        )]));
    }

    // Header lines.
    lines.extend(build_header_lines(prompt));
    lines.push(Line::default());

    // Options stack.
    lines.extend(build_option_lines(prompt));

    // Footer hint.
    lines.push(Line::default());
    lines.push(build_footer_line(prompt));

    // 1 empty row bottom (spec §3 inner padding).
    lines.push(Line::default());
    lines
}

fn build_header_lines(prompt: &PromptState) -> Vec<Line<'static>> {
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
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    title_owned,
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]));
            // Yellow ⚠ decision_reason.
            if let Some(reason) = decision_reason.as_deref().filter(|r| !r.is_empty()) {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("⚠ {reason}"), Style::default().fg(Color::Yellow)),
                ]));
            }
            // Dim display_description.
            if let Some(desc) = display_description.as_deref().filter(|d| !d.is_empty()) {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(desc.to_owned(), Style::default().fg(theme::DIM)),
                ]));
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
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("? ", Style::default().fg(theme::RUST_ORANGE)),
                Span::styled(
                    format!("{}{}", q.header, progress),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]));
            for row in q.question.lines() {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(row.to_owned(), Style::default().fg(Color::White)),
                ]));
            }
            out
        }
    }
}

fn build_option_lines(prompt: &PromptState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let is_multi = prompt.is_multi_select();
    // For Question prompts, keep a reference to the wire options so
    // we can read .description and .preview by index.
    let question_options: Option<&[forge_primitives::question::QuestionOption]> =
        match &prompt.source {
            PromptSource::Question { prompt: q, .. } => Some(&q.options),
            PromptSource::Permission { .. } => None,
        };

    for (i, opt) in prompt.options.iter().enumerate() {
        let is_focused = i == prompt.focused_option_index;
        let is_toggled = prompt.selected_option_indices.contains(&i);
        let (icon, icon_color) = icon_for_kind(opt.kind);
        let pointer = if is_focused { "▸ " } else { "  " };
        let pointer_style = if is_focused {
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let name_style = if is_focused {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let mut spans: Vec<Span<'static>> =
            vec![Span::raw("  "), Span::styled(pointer.to_string(), pointer_style)];
        if is_multi {
            let checkbox = if is_toggled { "[x] " } else { "[ ] " };
            let checkbox_style = if is_toggled {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(theme::DIM)
            };
            spans.push(Span::styled(checkbox.to_string(), checkbox_style));
        }
        spans.push(Span::styled(format!("{icon} "), Style::default().fg(icon_color)));
        spans.push(Span::styled(opt.name.clone(), name_style));
        lines.push(Line::from(spans));

        // Question-specific: render the option's description as dim
        // subtext below the option label (if present and non-empty).
        if let Some(q_opts) = question_options
            && let Some(q_opt) = q_opts.get(i)
            && let Some(desc) = q_opt.description.as_deref().filter(|d| !d.is_empty())
        {
            lines.push(Line::from(vec![
                Span::raw("      "),
                Span::styled(desc.to_owned(), Style::default().fg(theme::DIM)),
            ]));
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
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Preview:".to_string(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
        ]));
        for row in preview.lines() {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(row.to_owned(), Style::default().fg(theme::DIM)),
            ]));
        }
    }

    lines
}

fn build_footer_line(prompt: &PromptState) -> Line<'static> {
    let text = match prompt.mode {
        PromptMode::OptionPicker => {
            if prompt.is_multi_select() {
                "  space toggle  ↑↓ move  ⏎ submit  esc cancel"
            } else {
                "  ↑↓ select  ⏎ confirm  esc reject"
            }
        }
        PromptMode::EditingInput => "  ⏎ submit  esc back to options",
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
        render(area, &mut buf, prompt, queue_depth);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
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
    fn multi_select_renders_checkbox_markers() {
        let request = make_question_request(true);
        let mut prompt = PromptState::from_question("tc-q".into(), request);
        prompt.selected_option_indices.insert(0);
        let out = render_to_string(&prompt, 1, 80, 14);
        assert!(out.contains("[x] ✓ Red"), "expected [x] on toggled option; got:\n{out}");
        assert!(out.contains("[ ] ✓ Blue"), "expected [ ] on untoggled option; got:\n{out}");
    }

    #[test]
    fn question_option_with_description_renders_dim_subtext() {
        let mut request = make_question_request(false);
        request.prompt.options[0].description = Some("matches chat input — loud, urgent".into());
        let prompt = PromptState::from_question("tc-q".into(), request);
        let out = render_to_string(&prompt, 1, 80, 18);
        assert!(
            out.contains("matches chat input — loud, urgent"),
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
    fn permission_prompt_does_not_render_preview_block() {
        let prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let out = render_to_string(&prompt, 1, 80, 14);
        assert!(!out.contains("Preview:"), "Permission prompts shouldn't render a Preview block");
    }
}
