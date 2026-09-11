use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::wrap::{self, StyledChunk};
use super::{highlight, theme};

#[derive(Debug)]
pub(crate) enum SegmentKind<'a> {
    Prose(&'a str),
    Code { language: &'a str, body: &'a str },
}

#[derive(Debug)]
pub(crate) struct Segment<'a> {
    pub(crate) kind: SegmentKind<'a>,
    pub(crate) range: Range<usize>,
}

/// An opening fence waiting for its closer.
struct OpenFence<'a> {
    marker: char,
    run: usize,
    language: &'a str,
    opener_start: usize,
    body_start: usize,
}

/// The fence marker run on a line, with the indentation already checked.
struct FenceLine<'a> {
    marker: char,
    run: usize,
    rest: &'a str,
}

/// Indentation is ignored rather than capped at CommonMark's three
/// spaces: a fence indented four spaces sits under a list item in real
/// output, and dropping it into prose mangles its content instead of
/// showing it.
fn fence_line(line: &str) -> Option<FenceLine<'_>> {
    let body = line.trim_start();
    let marker = body.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let run = body.chars().take_while(|ch| *ch == marker).count();
    if run < 3 {
        return None;
    }
    Some(FenceLine { marker, run, rest: &body[run..] })
}

fn open_fence(line: &str) -> Option<(char, usize, &str)> {
    let fence = fence_line(line)?;
    // A backtick fence's info string may not itself contain a backtick;
    // such a line is a paragraph rather than a fence.
    if fence.marker == '`' && fence.rest.contains('`') {
        return None;
    }
    Some((fence.marker, fence.run, fence.rest.trim()))
}

fn closes_fence(line: &str, open: &OpenFence<'_>) -> bool {
    fence_line(line).is_some_and(|fence| {
        fence.marker == open.marker && fence.run >= open.run && fence.rest.trim().is_empty()
    })
}

pub(crate) fn segments(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    walk_segments(text, |segment| out.push(segment));
    out
}

/// Byte spans of every fenced code block, fence lines included but not
/// the newline that ends the closing one.
pub(crate) fn code_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    walk_segments(text, |segment| {
        if matches!(segment.kind, SegmentKind::Code { .. }) {
            ranges.push(segment.range);
        }
    });
    ranges
}

fn walk_segments<'t>(text: &'t str, mut visit: impl FnMut(Segment<'t>)) {
    let mut prose_start = 0usize;
    let mut open: Option<OpenFence<'t>> = None;
    let mut offset = 0usize;

    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        match open.take() {
            None => {
                if let Some((marker, run, language)) = open_fence(content) {
                    if prose_start < offset {
                        visit(prose(text, prose_start..offset));
                    }
                    open = Some(OpenFence {
                        marker,
                        run,
                        language,
                        opener_start: offset,
                        body_start: offset + line.len(),
                    });
                }
            }
            Some(current) => {
                if closes_fence(content, &current) {
                    let end = offset + content.len();
                    visit(Segment {
                        kind: SegmentKind::Code {
                            language: current.language,
                            body: &text[current.body_start..offset],
                        },
                        range: current.opener_start..end,
                    });
                    prose_start = end;
                } else {
                    open = Some(current);
                }
            }
        }
        offset += line.len();
    }

    match open {
        Some(current) => visit(Segment {
            kind: SegmentKind::Code {
                language: current.language,
                body: &text[current.body_start..],
            },
            range: current.opener_start..text.len(),
        }),
        None if prose_start < text.len() => visit(prose(text, prose_start..text.len())),
        None => {}
    }
}

fn prose(text: &str, range: Range<usize>) -> Segment<'_> {
    Segment { kind: SegmentKind::Prose(&text[range.clone()]), range }
}

/// Columns of the panel kept clear left of the code.
const PANEL_PAD: usize = 2;

/// Render a fenced code block as a quiet panel: the info string dimmed on
/// the first row, the highlighted code wrapped to the panel's own width
/// under it. Fence delimiters are never part of the output.
pub(crate) fn render_code_panel(body: &str, language: &str, width: u16) -> Vec<Line<'static>> {
    let panel_width = usize::from(width);
    let content_width = panel_width.saturating_sub(PANEL_PAD).max(1);
    let mut lines = Vec::new();

    let language = language.trim();
    if !language.is_empty() {
        let label = wrap::truncate_to_width(language, content_width);
        lines.push(panel_row(
            vec![Span::styled(label, Style::default().fg(theme::CODE_PANEL_LABEL))],
            panel_width,
        ));
    }

    // `highlight_code` treats a trailing newline as a line of its own,
    // which would be a blank row the code does not have.
    let source = body.strip_suffix('\n').unwrap_or(body);
    for line in highlight::highlight_code(source, Some(language)) {
        let (indent, content) = split_line_indent(line.spans);
        // Tabs expand rather than picturing as U+2409, so a tab-indented
        // file reads at the same depth as its neighbours.
        let indent = wrap::replace_control_chars(wrap::expand_tabs(&indent)).into_owned();
        // An indent deeper than the panel has columns for is clipped, so
        // the row still ends at the panel edge instead of running past
        // it, and the content keeps a column of its own.
        let indent = wrap::truncate_to_width(&indent, content_width.saturating_sub(1));
        let wrap_width = content_width.saturating_sub(wrap::display_width(&indent)).max(1);
        let chunks: Vec<StyledChunk> = content
            .into_iter()
            .map(|span| StyledChunk {
                text: wrap::replace_control_chars(wrap::expand_tabs(span.content.as_ref()))
                    .into_owned(),
                style: span.style,
            })
            .collect();

        for wrapped in wrap::wrap_styled_chunks(&chunks, wrap_width) {
            let mut spans = Vec::with_capacity(wrapped.spans.len() + 1);
            if !indent.is_empty() {
                spans.push(Span::raw(indent.clone()));
            }
            spans.extend(wrapped.spans);
            lines.push(panel_row(spans, panel_width));
        }
    }

    lines
}

/// Peel a line's leading whitespace off its spans so the panel can paint
/// it explicitly: `wrap_styled_chunks` drops a leading whitespace token,
/// which is right for prose and flattens every level of code indentation.
fn split_line_indent(spans: Vec<Span<'static>>) -> (String, Vec<Span<'static>>) {
    let mut indent = String::new();
    let mut content = Vec::new();
    let mut peeling = true;

    for span in spans {
        if !peeling {
            content.push(span);
            continue;
        }
        let text = span.content.into_owned();
        let split = text
            .char_indices()
            .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
            .unwrap_or(text.len());
        indent.push_str(&text[..split]);
        if split < text.len() {
            peeling = false;
            content.push(Span::styled(text[split..].to_owned(), span.style));
        }
    }

    (indent, content)
}

/// One panel row: the pad, the wrapped code, then background to the right
/// edge so the panel reads as one block rather than per-span patches.
fn panel_row(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let style = Style::default().bg(theme::CODE_PANEL_BG);
    let mut row = vec![Span::styled(" ".repeat(PANEL_PAD), style)];
    row.extend(spans.into_iter().map(|mut span| {
        span.style = span.style.bg(theme::CODE_PANEL_BG);
        span
    }));

    let used: usize = row.iter().map(Span::width).sum();
    if used < width {
        row.push(Span::styled(" ".repeat(width - used), style));
    }
    Line::from(row).style(style)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn prose<'a>(segment: &Segment<'a>) -> &'a str {
        match segment.kind {
            SegmentKind::Prose(text) => text,
            SegmentKind::Code { .. } => panic!("expected a prose segment, got code"),
        }
    }

    fn code<'a>(segment: &Segment<'a>) -> (&'a str, &'a str) {
        match segment.kind {
            SegmentKind::Code { language, body } => (language, body),
            SegmentKind::Prose(text) => panic!("expected a code segment, got prose: {text:?}"),
        }
    }

    #[test]
    fn fence_language_is_split_out_of_the_surrounding_prose() {
        let text = "before\n```rust\nfn main() {}\n```\nafter";
        let segments = segments(text);
        assert_eq!(segments.len(), 3, "prose, code, prose: {segments:?}");
        assert_eq!(prose(&segments[0]), "before\n");
        assert_eq!(code(&segments[1]), ("rust", "fn main() {}\n"));
        assert_eq!(prose(&segments[2]), "\nafter");
    }

    #[test]
    fn fence_without_a_language_still_becomes_a_code_segment() {
        let text = "```\nplain text\n```";
        let segments = segments(text);
        assert_eq!(segments.len(), 1, "no prose either side: {segments:?}");
        assert_eq!(code(&segments[0]), ("", "plain text\n"));
    }

    #[test]
    fn tilde_fences_are_recognised() {
        let text = "before\n~~~python\nx = 1\n~~~\nafter";
        let segments = segments(text);
        assert_eq!(segments.len(), 3, "prose, code, prose: {segments:?}");
        assert_eq!(code(&segments[1]), ("python", "x = 1\n"));
    }

    #[test]
    fn a_backtick_fence_does_not_close_a_tilde_fence() {
        let text = "~~~\n```\ncode\n~~~";
        let segments = segments(text);
        assert_eq!(segments.len(), 1, "the backticks are content: {segments:?}");
        assert_eq!(code(&segments[0]), ("", "```\ncode\n"));
    }

    #[test]
    fn a_shorter_closing_run_does_not_close_a_longer_opener() {
        let text = "````\n```\nlet a: Vec<T> = vec![];\n````";
        let segments = segments(text);
        assert_eq!(segments.len(), 1, "the inner fence is content: {segments:?}");
        assert_eq!(code(&segments[0]), ("", "```\nlet a: Vec<T> = vec![];\n"));
    }

    #[test]
    fn a_longer_closing_run_closes_a_shorter_opener() {
        let text = "```\nlet a: Vec<T> = vec![];\n`````\nafter";
        let segments = segments(text);
        assert_eq!(segments.len(), 2, "code then prose: {segments:?}");
        assert_eq!(code(&segments[0]), ("", "let a: Vec<T> = vec![];\n"));
        assert_eq!(prose(&segments[1]), "\nafter");
    }

    #[test]
    fn an_indented_fence_still_takes_its_content_verbatim() {
        let text = "    ```rust\n    let a: Vec<T> = vec![];\n    ```";
        let segments = segments(text);
        assert_eq!(segments.len(), 1, "an indented fence is a fence: {segments:?}");
        assert_eq!(code(&segments[0]), ("rust", "    let a: Vec<T> = vec![];\n"));
    }

    #[test]
    fn a_closing_fence_carrying_an_info_string_is_code() {
        let text = "```\n```rust\nlet a = 1;\n```";
        let segments = segments(text);
        assert_eq!(segments.len(), 1, "the inner fence line is content: {segments:?}");
        assert_eq!(code(&segments[0]), ("", "```rust\nlet a = 1;\n"));
    }

    #[test]
    fn the_info_string_is_trimmed() {
        let text = "```  rust  \nlet a = 1;\n```";
        let segments = segments(text);
        assert_eq!(code(&segments[0]).0, "rust", "the info string is trimmed");
    }

    #[test]
    fn a_backtick_info_string_holding_a_backtick_is_not_a_fence() {
        let text = "```a`b\nlet a = 1;";
        let segments = segments(text);
        assert_eq!(segments.len(), 1, "the line is a paragraph: {segments:?}");
        assert_eq!(prose(&segments[0]), text);
    }

    #[test]
    fn an_unterminated_fence_stays_open_to_the_end_of_input() {
        let text = "before\n```rust\nfn main() {}";
        let segments = segments(text);
        assert_eq!(segments.len(), 2, "prose then code: {segments:?}");
        assert_eq!(prose(&segments[0]), "before\n");
        assert_eq!(code(&segments[1]), ("rust", "fn main() {}"));
    }

    #[test]
    fn segments_cover_the_whole_source_without_overlapping() {
        let text = "a\n```rust\ncode\n```\nb\n~~~\nmore\n~~~\nc";
        let segments = segments(text);
        let mut cursor = 0usize;
        for segment in &segments {
            assert_eq!(segment.range.start, cursor, "gap before {segment:?}");
            cursor = segment.range.end;
        }
        assert_eq!(cursor, text.len(), "segments must reach the end of the source");
    }

    fn row_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    /// Panel rows with their right-edge fill trimmed, so a row is
    /// compared by what it says rather than by how far it is padded.
    fn panel_rows(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(|line| row_text(line).trim_end().to_owned()).collect()
    }

    fn assert_paints_what_it_measures(lines: &[Line<'static>]) {
        for line in lines {
            let measured: usize = line.spans.iter().map(Span::width).sum();
            let painted: usize = line
                .spans
                .iter()
                .map(|span| {
                    span.styled_graphemes(Style::default())
                        .map(|grapheme| wrap::display_width(grapheme.symbol))
                        .sum::<usize>()
                })
                .sum();
            assert_eq!(
                measured,
                painted,
                "row charges a column it does not paint: {:?}",
                row_text(line)
            );
        }
    }

    #[test]
    fn the_panel_labels_the_code_with_its_info_string() {
        let lines = render_code_panel("fn main() {}\n", "rust", 40);
        assert_eq!(panel_rows(&lines), ["  rust", "  fn main() {}"]);
        assert_eq!(
            lines[0].spans[1].style.fg,
            Some(theme::CODE_PANEL_LABEL),
            "the label is dim against the code"
        );
    }

    #[test]
    fn the_panel_keeps_every_level_of_indentation() {
        let lines = render_code_panel("fn f() {\n    nested();\n}\n", "rust", 40);
        assert_eq!(panel_rows(&lines), ["  rust", "  fn f() {", "      nested();", "  }"]);
    }

    #[test]
    fn a_tab_paints_every_column_it_measures() {
        let indented = render_code_panel("\tfn main() {}\n", "rust", 40);
        assert_eq!(panel_rows(&indented), ["  rust", "      fn main() {}"]);
        assert_paints_what_it_measures(&indented);
        assert_paints_what_it_measures(&render_code_panel("let a = 1;\t// note\n", "", 40));
    }

    #[test]
    fn the_panel_prints_no_fence_delimiters() {
        let lines = render_code_panel("let a = 1;\n", "rust", 40);
        let text = panel_rows(&lines).join("\n");
        assert!(!text.contains("```"), "delimiters never reach the panel: {text:?}");
    }

    #[test]
    fn a_panel_without_a_language_starts_at_the_code() {
        let lines = render_code_panel("plain text\n", "", 40);
        assert_eq!(panel_rows(&lines), ["  plain text"], "no label row");
    }

    fn assert_panel_rows_fill(lines: &[Line<'static>], width: usize) {
        for line in lines {
            assert_eq!(
                wrap::line_display_width(line),
                width,
                "row does not end at the panel edge: {:?}",
                row_text(line)
            );
            assert!(
                line.spans.iter().all(|span| span.style.bg == Some(theme::CODE_PANEL_BG)),
                "a span escapes the panel background: {:?}",
                row_text(line)
            );
        }
        assert_paints_what_it_measures(lines);
    }

    #[test]
    fn every_panel_row_paints_the_background_to_the_full_width() {
        assert_panel_rows_fill(&render_code_panel("fn main() {}\n", "rust", 40), 40);
        assert_panel_rows_fill(&render_code_panel(&format!("{}x\n", " ".repeat(40)), "", 20), 20);
    }

    #[test]
    fn a_long_code_line_wraps_inside_the_panel() {
        let lines =
            render_code_panel("let value = some_function(argument_one, argument_two);\n", "", 20);
        assert!(
            lines.len() > 1,
            "the line wraps rather than overflowing: {:?}",
            panel_rows(&lines)
        );
        for line in &lines {
            assert_eq!(wrap::line_display_width(line), 20, "every row ends at the panel edge");
        }
    }

    #[test]
    fn a_blank_line_in_the_code_keeps_its_row() {
        let lines = render_code_panel("let a = 1;\n\nlet b = 2;\n", "", 40);
        assert_eq!(panel_rows(&lines), ["  let a = 1;", "", "  let b = 2;"]);
    }

    #[test]
    fn code_ranges_report_where_the_fences_are() {
        let text = "a\n```\ncode\n```\nb";
        assert_eq!(code_ranges(text), vec![2..14], "the closing newline stays out of the range");
    }
}
