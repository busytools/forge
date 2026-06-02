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
/// `<status_icon> = <summary>   ctrl+x to expand`.
///
/// Prepends a 2-space LEFT indent matching
/// `standard::render_tool_call_title`'s convention. The group kind-icon
/// is ASCII `=` followed by an explicit trailing space, giving a
/// deterministic `<icon>(1) + space(1)` 2-cell slot matching every
/// other tool-call row's chrome regardless of terminal EAW behavior.
/// The right edge is left as Paragraph-default cells.
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
) -> Vec<Line<'static>> {
    let (icon_glyph, icon_color) = status_icon(aggregate_status, spinner_frame);
    let summary = kind_count.format_summary();
    let dim = Style::default().fg(theme::DIM);
    vec![Line::from(vec![
        Span::raw("  ".to_owned()),
        Span::styled(
            format!("{icon_glyph} "),
            Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("= ".to_owned(), Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD)),
        Span::styled(summary, Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("   ctrl+x to expand".to_owned(), dim),
    ])]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    /// Regression guard: the right-edge pad span removed in #321 must
    /// not come back. The replay snapshot harness's `buffer_to_text`
    /// trims trailing spaces, so a re-introduced pad would NOT trip
    /// any snapshot test - this assertion inspects the rendered Line
    /// directly to close that gap.
    #[test]
    fn render_group_summary_line_has_no_trailing_whitespace_pad() {
        let k = KindCount { reads: 5, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0);
        let text = line_text(&lines[0]);
        assert!(
            !text.ends_with(' '),
            "group summary must not end with whitespace pad; got {text:?}",
        );
    }

    #[test]
    fn render_group_summary_line_completed_uses_checkmark_and_group_icon() {
        let k = KindCount { reads: 5, searches: 3, commands: 2, calls: 0 };
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(text.contains(theme::ICON_COMPLETED), "completed status_icon: {text:?}");
        assert!(text.contains('='), "group-icon `=` missing: {text:?}");
        assert!(text.contains("5 reads"));
        assert!(text.contains("3 searches"));
        assert!(text.contains("2 commands"));
        assert!(text.contains("ctrl+x to expand"));
        assert!(!text.contains("> "), "legacy `> ` prefix must be gone: {text:?}");
    }

    #[test]
    fn render_group_summary_line_drops_zero_kinds() {
        let k = KindCount { reads: 5, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0);
        let text = line_text(&lines[0]);
        assert!(text.contains("5 reads"));
        assert!(!text.contains("searches"));
        assert!(!text.contains("commands"));
    }

    #[test]
    fn render_group_summary_line_in_progress_uses_braille_spinner() {
        let k = KindCount { reads: 3, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::InProgress, 0);
        let text = line_text(&lines[0]);
        let has_braille = text.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c));
        assert!(has_braille, "InProgress must use a braille spinner glyph: {text:?}");
        assert!(text.contains('='));
    }

    #[test]
    fn render_group_summary_line_failed_uses_cross() {
        let k = KindCount { reads: 1, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Failed, 0);
        let text = line_text(&lines[0]);
        assert!(text.contains(theme::ICON_FAILED), "failed status_icon: {text:?}");
        assert!(text.contains('='));
    }

    #[test]
    fn render_group_summary_line_pending_uses_hollow_circle() {
        let k = KindCount { reads: 1, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Pending, 0);
        let text = line_text(&lines[0]);
        assert!(text.contains('\u{25CB}'), "Pending must use hollow circle: {text:?}");
        assert!(text.contains('='));
    }

    #[test]
    fn render_group_summary_line_spinner_frame_advances() {
        let k = KindCount { reads: 1, ..KindCount::default() };
        let text_a = line_text(&render_group_summary_line(k, ToolCallStatus::InProgress, 0)[0]);
        let text_b = line_text(&render_group_summary_line(k, ToolCallStatus::InProgress, 3)[0]);
        let icon_a = text_a.chars().nth(2).expect("status icon char");
        let icon_b = text_b.chars().nth(2).expect("status icon char");
        assert_ne!(icon_a, icon_b, "spinner frames 0 and 3 must produce different glyphs");
    }

    #[test]
    fn render_group_summary_line_has_two_space_left_indent() {
        let k = KindCount { reads: 5, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(
            text.starts_with("  "),
            "group summary must have 2-space LEFT indent matching standard tool-call title; got {text:?}"
        );
    }

    /// Group-summary alignment invariant: the line starts with the
    /// 2-space LEFT indent, the group kind-icon is ASCII `=`, and
    /// the char immediately after `=` is a space. The `<icon>(1) +
    /// space(1)` slot fills 2 cells matching standard tool-row
    /// chrome regardless of terminal EAW behavior.
    #[test]
    fn render_group_summary_line_glyph_alignment() {
        let k = KindCount { reads: 5, ..KindCount::default() };
        let lines = render_group_summary_line(k, ToolCallStatus::Completed, 0);
        let text = line_text(&lines[0]);
        assert!(
            text.starts_with("  "),
            "group summary must start with 2-space LEFT indent; got {text:?}",
        );
        assert!(
            !text.contains('\u{2630}'),
            "group summary must not contain the U+2630 glyph; got {text:?}",
        );
        let equals_pos =
            text.find('=').unwrap_or_else(|| panic!("ASCII `=` group-icon missing; got {text:?}"));
        assert!(
            equals_pos >= 2,
            "`=` must appear after the indent; got equals_pos={equals_pos}, text={text:?}",
        );
        let after_equals_idx = equals_pos + '='.len_utf8();
        let after_equals_char = text[after_equals_idx..].chars().next();
        assert_eq!(
            after_equals_char,
            Some(' '),
            "ASCII `=` must be followed by an explicit space (2-cell slot); got {text:?}",
        );
    }
}
