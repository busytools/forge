//! Render the L2 summary line for a grouped run of consecutive
//! collapsed-by-default tool calls. The L1 (title rows) and L0 (full
//! bodies) levels are produced by the standard per-tool render path
//! threaded with a `force_collapsed` flag from the caller.
//!
//! See `docs/superpowers/specs/2026-06-01-chat-tool-grouping-v2.md`
//! decision 6.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::agent::model::ToolCallStatus;
use crate::ui::message::grouping::KindCount;
use crate::ui::theme;
use crate::ui::tool_call::status_icon;

/// Render the L2 summary line for a grouped run:
/// `<status_icon> ☰ <summary>   ctrl+x to expand`.
///
/// `aggregate_status` drives the leading status_icon (spinner for
/// InProgress, check for Completed, cross for Failed/Killed, hollow
/// circle for Pending) via the same `tool_call::status_icon` helper
/// the per-tool render uses. `spinner_frame` selects the braille
/// frame when the aggregate is InProgress; otherwise unused.
///
/// PERF: the spinner animates without a layout re-measure - only
/// the leading status_icon cell changes, line width is stable. The
/// per-tool spinner pattern in `message.rs::tool_call_needs_spinner_frame`
/// is the precedent.
pub fn render_group_summary_line(
    kind_count: KindCount,
    aggregate_status: ToolCallStatus,
    spinner_frame: usize,
    chat_content_width: u16,
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let (icon_glyph, icon_color) = status_icon(aggregate_status, spinner_frame);
    let summary = kind_count.format_summary();
    let dim = Style::default().fg(theme::DIM);
    // 2-space LEFT indent matches `standard::render_tool_call_title`.
    let mut spans = vec![
        Span::raw("  ".to_owned()),
        Span::styled(
            format!("{icon_glyph} "),
            Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "\u{2630} ".to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary, dim),
        Span::styled("   ctrl+x to expand".to_owned(), dim),
    ];
    // RIGHT-edge padding: pad the line out to `chat_content_width` so
    // no trailing blank cell reads as a shifted right-border via the
    // scrollbar-thumb-visibility cosmetic.
    let rendered_width: usize =
        spans.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
    let pad = usize::from(chat_content_width).saturating_sub(rendered_width);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    vec![Line::from(spans)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    const TEST_WIDTH: u16 = 80;

    #[test]
    fn render_group_summary_line_completed_uses_checkmark_and_group_icon() {
        let k = KindCount { reads: 5, searches: 3, commands: 2, calls: 0 };
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(text.contains(theme::ICON_COMPLETED), "completed status_icon: {text:?}");
        assert!(text.contains('\u{2630}'), "group-icon ☰ missing: {text:?}");
        assert!(text.contains("5 reads"));
        assert!(text.contains("3 searches"));
        assert!(text.contains("2 commands"));
        assert!(text.contains("ctrl+x to expand"));
        assert!(!text.contains("> "), "legacy `> ` prefix must be gone: {text:?}");
    }

    #[test]
    fn render_group_summary_line_drops_zero_kinds() {
        let k = KindCount { reads: 5, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0, TEST_WIDTH);
        let text = line_text(&lines[0]);
        assert!(text.contains("5 reads"));
        assert!(!text.contains("searches"));
        assert!(!text.contains("commands"));
    }

    #[test]
    fn render_group_summary_line_in_progress_uses_braille_spinner() {
        let k = KindCount { reads: 3, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::InProgress, 0, TEST_WIDTH);
        let text = line_text(&lines[0]);
        let has_braille = text.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c));
        assert!(has_braille, "InProgress must use a braille spinner glyph: {text:?}");
        assert!(text.contains('\u{2630}'));
    }

    #[test]
    fn render_group_summary_line_failed_uses_cross() {
        let k = KindCount { reads: 1, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Failed, 0, TEST_WIDTH);
        let text = line_text(&lines[0]);
        assert!(text.contains(theme::ICON_FAILED), "failed status_icon: {text:?}");
        assert!(text.contains('\u{2630}'));
    }

    #[test]
    fn render_group_summary_line_pending_uses_hollow_circle() {
        let k = KindCount { reads: 1, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Pending, 0, TEST_WIDTH);
        let text = line_text(&lines[0]);
        assert!(text.contains('\u{25CB}'), "Pending must use hollow circle: {text:?}");
        assert!(text.contains('\u{2630}'));
    }

    #[test]
    fn render_group_summary_line_spinner_frame_advances() {
        let k = KindCount { reads: 1, ..KindCount::default() };
        let text_a =
            line_text(&render_group_summary_line(k, ToolCallStatus::InProgress, 0, TEST_WIDTH)[0]);
        let text_b =
            line_text(&render_group_summary_line(k, ToolCallStatus::InProgress, 3, TEST_WIDTH)[0]);
        // Status icon sits at column 2 (after the 2-space LEFT indent
        // matching `standard::render_tool_call_title`'s convention).
        let icon_a = text_a.chars().nth(2).expect("status icon char");
        let icon_b = text_b.chars().nth(2).expect("status icon char");
        assert_ne!(icon_a, icon_b, "spinner frames 0 and 3 must produce different glyphs");
    }

    #[test]
    fn render_group_summary_line_has_two_space_left_indent() {
        let k = KindCount { reads: 5, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0, TEST_WIDTH);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(
            text.starts_with("  "),
            "group summary must have 2-space LEFT indent matching standard tool-call title; got {text:?}"
        );
    }

    #[test]
    fn render_group_summary_line_padded_to_chat_content_width() {
        use unicode_width::UnicodeWidthStr;
        let k = KindCount { reads: 5, ..KindCount::default() };
        let width: u16 = 80;
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0, width);
        let text = line_text(&lines[0]);
        let text_width = UnicodeWidthStr::width(text.as_str());
        assert_eq!(
            text_width,
            usize::from(width),
            "group summary must pad to chat_content_width={width}; got text_width={text_width} for {text:?}"
        );
    }
}
