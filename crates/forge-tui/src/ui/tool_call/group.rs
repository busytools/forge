//! Render the L2 summary line for a grouped run of consecutive
//! collapsed-by-default tool calls. The L1 (title rows) and L0 (full
//! bodies) levels are produced by the standard per-tool render path
//! threaded with a `force_collapsed` flag from the caller.
//!
//! See `docs/superpowers/specs/2026-06-01-chat-tool-grouping.md`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::message::grouping::KindCount;
use crate::ui::theme;

/// Render the L2 summary line: `> 5 reads \u{b7} 3 searches \u{b7} 2 commands`.
/// DIM, with `>` as the left-gutter expand affordance.
pub fn render_group_summary_line(kind_count: KindCount) -> Vec<Line<'static>> {
    let summary = kind_count.format_summary();
    let dim = Style::default().fg(theme::DIM);
    vec![Line::from(vec![
        Span::styled("> ".to_owned(), dim.add_modifier(Modifier::BOLD)),
        Span::styled(summary, dim),
        Span::styled("  ctrl+x to expand".to_owned(), dim),
    ])]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect::<String>()
    }

    #[test]
    fn render_group_summary_line_has_expected_text() {
        let k = KindCount { reads: 5, searches: 3, commands: 2 };
        let lines = render_group_summary_line(k);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("> "), "got {text:?}");
        assert!(text.contains("5 reads"));
        assert!(text.contains("3 searches"));
        assert!(text.contains("2 commands"));
        assert!(text.contains("ctrl+x to expand"));
    }

    #[test]
    fn render_group_summary_line_drops_zero_kinds() {
        let k = KindCount { reads: 5, ..KindCount::default() };
        let lines = render_group_summary_line(k);
        let text = line_text(&lines[0]);
        assert!(text.contains("5 reads"));
        assert!(!text.contains("searches"));
        assert!(!text.contains("commands"));
    }
}
