use crate::agent::model;
use crate::ui::highlight::LineHighlighter;
use crate::ui::theme;
use crate::ui::wrap::{StyledChunk, display_width, expand_tabs, wrap_styled_chunks};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::TextDiff;

/// Rows outside this window render without syntax highlighting.
///
/// A Write body is capped to a head and a tail after rendering, so
/// syntect on the middle of a large file is thrown away before it
/// reaches the screen. Rows are still built and wrapped either way,
/// which keeps the omitted-line count exact. Colours in the tail can
/// differ from a full render: the skipped rows never advance the
/// highlighter, so a multi-line construct opened in the middle is not
/// carried across.
#[derive(Clone, Copy)]
pub struct HighlightWindow {
    pub head_rows: usize,
    pub tail_rows: usize,
}

fn row_is_highlighted(row_idx: usize, row_total: usize, window: Option<HighlightWindow>) -> bool {
    let Some(window) = window else { return true };
    row_idx < window.head_rows || row_idx + window.tail_rows >= row_total
}

/// Render a diff with proper unified-style output using the `similar` crate.
/// The model `Diff` struct provides `old_text`/`new_text` -- we compute the actual
/// line-level changes and show only changed lines with context.
pub fn render_diff(
    diff: &model::Diff,
    width: u16,
    highlight_window: Option<HighlightWindow>,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(repository) = diff.repository.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("[{repository}]"),
            Style::default().fg(theme::DIM),
        )));
    }

    let old = diff.old_text.as_deref().unwrap_or("");
    let new = &diff.new_text;
    let text_diff = TextDiff::from_lines(old, new);
    let line_number_width = old.lines().count().max(new.lines().count()).max(1).to_string().len();
    let content_width = usize::from(width).saturating_sub(line_number_width + 5).max(1);

    // One syntect highlighter per side so multi-line constructs (block
    // comments, strings spanning lines) carry state correctly. Context
    // lines feed BOTH so the old + new sides stay synchronized for the
    // next change that hits either side.
    let path_str = diff.path.to_string_lossy();
    let mut left_hl = LineHighlighter::for_path(&path_str);
    let mut right_hl = LineHighlighter::for_path(&path_str);

    // Use unified diff with 3 lines of context -- only shows changed hunks
    // instead of the full file content.
    let udiff = text_diff.unified_diff();
    // Only needed to place the tail edge of the highlight window; the
    // extra pass walks hunks without building or highlighting anything.
    let row_total = if highlight_window.is_some() {
        udiff.iter_hunks().map(|hunk| hunk.iter_changes().count()).sum()
    } else {
        0
    };
    let mut row_idx = 0usize;
    for hunk in udiff.iter_hunks() {
        // Extract the @@ header from the hunk's Display output (first line).
        let hunk_str = hunk.to_string();
        if let Some(header) = hunk_str.lines().next()
            && header.starts_with("@@")
        {
            lines.push(Line::from(Span::styled(
                format_compact_hunk_header(header),
                Style::default().fg(Color::Cyan),
            )));
        }

        for change in hunk.iter_changes() {
            let value = change.as_str().unwrap_or("").trim_end_matches('\n');
            let (marker, marker_style, line_number, row_bg) = match change.tag() {
                similar::ChangeTag::Delete => (
                    "-",
                    Style::default().fg(Color::Red),
                    change.old_index().map(|index| index + 1),
                    Some(theme::DIFF_DELETION_BG),
                ),
                similar::ChangeTag::Insert => (
                    "+",
                    Style::default().fg(Color::Green),
                    change.new_index().map(|index| index + 1),
                    Some(theme::DIFF_ADDITION_BG),
                ),
                similar::ChangeTag::Equal => (
                    " ",
                    Style::default().fg(theme::DIM),
                    change.new_index().map(|index| index + 1),
                    None,
                ),
            };

            // Split leading whitespace; `wrap_styled_chunks` drops
            // leading spaces at wrap boundaries, so the indent column
            // is rendered explicitly (matches pre-syntect behavior).
            let expanded = expand_tabs(value);
            let (leading_indent, content) = split_leading_whitespace(&expanded);
            let highlighted = row_is_highlighted(row_idx, row_total, highlight_window);
            row_idx += 1;
            let highlighted_spans = if highlighted {
                match change.tag() {
                    similar::ChangeTag::Delete => left_hl.highlight(content),
                    similar::ChangeTag::Insert => right_hl.highlight(content),
                    similar::ChangeTag::Equal => {
                        // Feed both sides to keep their state synchronized;
                        // use right_hl output for display and dim it so the
                        // context row stays visually distinct from changes.
                        let _ = left_hl.highlight(content);
                        right_hl.highlight(content)
                    }
                }
            } else if content.is_empty() {
                Vec::new()
            } else {
                vec![Span::raw(content.to_owned())]
            };
            let extra_modifier =
                matches!(change.tag(), similar::ChangeTag::Equal).then_some(Modifier::DIM);
            let content_chunks = spans_to_chunks(highlighted_spans, extra_modifier);

            lines.extend(render_wrapped_diff_row(
                line_number,
                line_number_width,
                marker,
                marker_style,
                leading_indent,
                &content_chunks,
                content_width,
                row_bg,
            ));
        }
    }

    lines
}

fn spans_to_chunks(
    spans: Vec<Span<'static>>,
    extra_modifier: Option<Modifier>,
) -> Vec<StyledChunk> {
    spans
        .into_iter()
        .map(|span| {
            let style = match extra_modifier {
                Some(modifier) => span.style.add_modifier(modifier),
                None => span.style,
            };
            StyledChunk { text: span.content.into_owned(), style }
        })
        .collect()
}

pub fn looks_like_unified_diff(text: &str) -> bool {
    let mut saw_hunk = false;
    let mut saw_file_header = false;
    let mut saw_metadata = false;

    for line in text.lines().take(64) {
        if line.starts_with("@@") {
            saw_hunk = true;
        } else if line.starts_with("--- ") || line.starts_with("+++ ") {
            saw_file_header = true;
        } else if line.starts_with("diff --git ")
            || line.starts_with("index ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
        {
            saw_metadata = true;
        }
    }

    saw_hunk && (saw_file_header || saw_metadata)
}

pub fn render_raw_unified_diff(text: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for line in text.split('\n') {
        lines.push(render_raw_diff_line(line));
    }

    if lines.is_empty() {
        lines.push(Line::default());
    }

    lines
}

fn render_raw_diff_line(line: &str) -> Line<'static> {
    // File-header lines stay un-tinted so the metadata band reads
    // distinct from the actual diff payload. Body `+` / `-` lines pick
    // up the GitHub-style row tint via the theme constants.
    //
    // The third field marks a row whose text after the one-char marker
    // is source, so tab stops are measured from the source column and
    // this path indents to the same depth as the other diff surfaces.
    let (style, row_bg, carries_source) = if line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
    {
        (Style::default().fg(Color::White).add_modifier(Modifier::BOLD), None, false)
    } else if line.starts_with("@@") {
        (Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD), None, false)
    } else if line.starts_with("+++ ") {
        (Style::default().fg(Color::Green), None, false)
    } else if line.starts_with("--- ") {
        (Style::default().fg(Color::Red), None, false)
    } else if line.starts_with('+') {
        (Style::default().fg(Color::Green), Some(theme::DIFF_ADDITION_BG), true)
    } else if line.starts_with('-') {
        (Style::default().fg(Color::Red), Some(theme::DIFF_DELETION_BG), true)
    } else if line.starts_with('\\') {
        (Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC), None, false)
    } else {
        // A context row carries source after its space marker. Anything
        // else reaching here is interleaved output with no marker to
        // skip, and peeling its first column would strip a leading tab.
        (Style::default().fg(theme::DIM), None, line.starts_with(' '))
    };

    // Every marker is one ASCII byte, so the split is always on a
    // boundary when `carries_source` holds.
    let text = match carries_source.then(|| line.split_at(1)) {
        Some((marker, body)) => format!("{marker}{}", expand_tabs(body)),
        None => expand_tabs(line).into_owned(),
    };
    let mut rendered = Line::from(Span::styled(text, style));
    if let Some(bg) = row_bg {
        rendered = rendered.style(Style::default().bg(bg));
    }
    rendered
}

#[derive(Clone, Copy)]
struct HunkRange {
    start: usize,
    count: usize,
}

fn format_compact_hunk_header(header: &str) -> String {
    parse_unified_hunk_header(header).map_or_else(
        || header.to_owned(),
        |(old_range, new_range)| {
            let mut parts = Vec::with_capacity(2);
            if old_range.count > 0 {
                parts.push(format_range("-", old_range));
            }
            if new_range.count > 0 {
                parts.push(format_range("+", new_range));
            }
            if parts.is_empty() { "lines".to_owned() } else { format!("lines {}", parts.join(" ")) }
        },
    )
}

fn parse_unified_hunk_header(header: &str) -> Option<(HunkRange, HunkRange)> {
    let body = header.strip_prefix("@@ ")?.split(" @@").next()?;
    let mut parts = body.split_whitespace();
    let old_range = parse_prefixed_hunk_range(parts.next()?, '-')?;
    let new_range = parse_prefixed_hunk_range(parts.next()?, '+')?;
    Some((old_range, new_range))
}

fn parse_prefixed_hunk_range(token: &str, prefix: char) -> Option<HunkRange> {
    let raw = token.strip_prefix(prefix)?;
    let (start, count) = raw.split_once(',').map_or((raw, "1"), |(start, count)| (start, count));
    Some(HunkRange { start: start.parse().ok()?, count: count.parse().ok()? })
}

fn format_range(prefix: &str, range: HunkRange) -> String {
    if range.count <= 1 {
        format!("{prefix}{}", range.start)
    } else {
        let end = range.start.saturating_add(range.count.saturating_sub(1));
        format!("{prefix}{}-{end}", range.start)
    }
}

fn render_wrapped_diff_row(
    line_number: Option<usize>,
    line_number_width: usize,
    marker: &str,
    marker_style: Style,
    leading_indent: &str,
    content_chunks: &[StyledChunk],
    content_width: usize,
    row_bg: Option<Color>,
) -> Vec<Line<'static>> {
    let number_style = Style::default().fg(theme::DIM);
    let leading_indent_width = display_width(leading_indent);
    let content_is_empty = content_chunks.iter().all(|chunk| chunk.text.is_empty());
    let content_lines = if content_is_empty {
        vec![Line::default()]
    } else {
        let wrapped_width = content_width.saturating_sub(leading_indent_width).max(1);
        wrap_styled_chunks(content_chunks, wrapped_width)
    };

    let line_number_text = line_number.map_or_else(
        || " ".repeat(line_number_width),
        |line_number| format!("{line_number:>line_number_width$}"),
    );
    let continuation_prefix = " ".repeat(line_number_width + 5);

    // Total row width to fill when `row_bg` is set. Mirrors the
    // diff_overlay.rs `build_split_half` pattern: ratatui's
    // `Line.style.bg` only paints behind the actual char cells the
    // spans emit; the gap between the last content char and the
    // right edge of the row stays terminal-default. To make the
    // tint visually extend across the whole row, we (a) propagate
    // `row_bg` onto every existing span that doesn't already carry
    // a bg, and (b) push a trailing bg-styled space-pad span sized
    // to the remaining width. Equal/context rows (`row_bg = None`)
    // skip both steps and render exactly as before.
    let total_row_width = line_number_width + 5 + content_width;

    content_lines
        .into_iter()
        .enumerate()
        .map(|(index, content_line)| {
            let mut spans = if index == 0 {
                vec![
                    Span::styled(line_number_text.clone(), number_style),
                    Span::styled("  ", number_style),
                    Span::styled(marker.to_owned(), marker_style),
                    Span::styled("  ", number_style),
                ]
            } else {
                vec![Span::styled(continuation_prefix.clone(), number_style)]
            };
            if !leading_indent.is_empty() {
                spans.push(Span::styled(leading_indent.to_owned(), marker_style));
            }
            spans.extend(content_line.spans);
            if let Some(bg) = row_bg {
                // Propagate the row bg onto every span that doesn't
                // already carry a bg. This fills the inter-span gaps
                // (e.g. between the line-number column's default-bg
                // gutter and the syntect-highlighted content) which
                // Line.style.bg alone wouldn't reach.
                for span in &mut spans {
                    if span.style.bg.is_none() {
                        span.style = span.style.bg(bg);
                    }
                }
                // Pad to full row width so the bg tint extends to the
                // right edge.
                let used: usize = spans.iter().map(Span::width).sum();
                if used < total_row_width {
                    spans.push(Span::styled(
                        " ".repeat(total_row_width - used),
                        Style::default().bg(bg),
                    ));
                }
            }
            let mut line = Line::from(spans);
            if let Some(bg) = row_bg {
                line = line.style(Style::default().bg(bg));
            }
            line
        })
        .collect()
}

fn split_leading_whitespace(text: &str) -> (&str, &str) {
    let split_at = text
        .char_indices()
        .find_map(|(idx, ch)| (!ch.is_whitespace()).then_some(idx))
        .unwrap_or(text.len());
    text.split_at(split_at)
}

/// Check if a tool call title references a markdown file.
// Markdown extensions are case-sensitive on case-sensitive filesystems; `eq_ignore_ascii_case` would mis-match `README.MD` on macOS APFS-default-CS.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub fn is_markdown_file(title: &str) -> bool {
    let lower = title.to_lowercase();
    lower.ends_with(".md") || lower.ends_with(".mdx") || lower.ends_with(".markdown")
}

/// Extract a language tag from the file extension in a tool call title.
/// Returns the raw extension (e.g. "rs", "py", "toml") which syntect
/// can resolve to the correct syntax definition. Falls back to empty string.
pub fn lang_from_title(title: &str) -> String {
    // Title may be "src/main.rs" or "Read src/main.rs" - find last path-like token
    title
        .split_whitespace()
        .rev()
        .find_map(|token| {
            let ext = token.rsplit('.').next()?;
            // Ignore if the "extension" is the whole token (no dot found)
            if ext.len() < token.len() { Some(ext.to_lowercase()) } else { None }
        })
        .unwrap_or_default()
}

/// Strip an outer markdown code fence if the text is entirely wrapped in one.
/// The bridge adapter often wraps file contents in ```` ``` ```` fences.
pub fn strip_outer_code_fence(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with("```") {
        // Find end of first line (the opening fence, possibly with a language tag)
        if let Some(first_newline) = trimmed.find('\n') {
            let after_opening = &trimmed[first_newline + 1..];
            // Check if it ends with a closing fence
            if let Some(body) = after_opening.strip_suffix("```") {
                return body.trim_end().to_owned();
            }
            // Also handle closing fence followed by newline
            let after_trimmed = after_opening.trim_end();
            if let Some(stripped) = after_trimmed.strip_suffix("```") {
                return stripped.trim_end().to_owned();
            }
        }
    }
    text.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn strip_outer_code_fence_handles_supported_and_passthrough_shapes() {
        let cases = [
            ("```rust\nfn main() {}\n```", "fn main() {}"),
            ("```\nhello world\n```", "hello world"),
            ("```\ncontent\n```  \n", "content"),
            ("```\n```\n", ""),
            ("```python\nline1\nline2\nline3\n```", "line1\nline2\nline3"),
            ("  ```\ncontent\n```", "content"),
            ("just plain text", "just plain text"),
            ("~~~\ncontent\n~~~", "~~~\ncontent\n~~~"),
            ("```rust\nfn main() {}", "```rust\nfn main() {}"),
        ];

        for (input, expected) in cases {
            assert_eq!(strip_outer_code_fence(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn strip_outer_code_fence_preserves_inner_fences_and_large_blocks() {
        let nested = "```\nsome code\n```\nmore code\n```";
        let nested_result = strip_outer_code_fence(nested);
        assert!(nested_result.contains("some code"));
        assert!(nested_result.contains("more code"));

        let quadruple = "````\ncontent here\n````";
        assert!(strip_outer_code_fence(quadruple).contains("content here"));

        let blank_lines = "```\n\n\n\n```";
        let blank_result = strip_outer_code_fence(blank_lines);
        assert!(blank_result.is_empty() || blank_result.chars().all(|c| c == '\n'));

        let big: String = (0..10_000).fold(String::new(), |mut s, i| {
            use std::fmt::Write;
            writeln!(s, "line {i}").unwrap();
            s
        });
        let input = format!("```\n{big}```");
        let result = strip_outer_code_fence(&input);
        assert!(result.contains("line 0"));
        assert!(result.contains("line 9999"));
    }

    #[test]
    fn highlight_window_covers_both_ends_and_skips_the_middle() {
        // #517: a Write body is capped to `WRITE_DIFF_MAX_LINES` after
        // rendering, so highlighting the middle of a large file is work
        // that is thrown away before it reaches the screen. The window
        // is expressed in rows from each end because the cap keeps a
        // head and a tail.
        let window = Some(HighlightWindow { head_rows: 10, tail_rows: 50 });
        assert!(row_is_highlighted(0, 5000, window));
        assert!(row_is_highlighted(9, 5000, window));
        assert!(!row_is_highlighted(10, 5000, window));
        assert!(!row_is_highlighted(4949, 5000, window));
        assert!(row_is_highlighted(4950, 5000, window));
        assert!(row_is_highlighted(4999, 5000, window));
    }

    #[test]
    fn no_highlight_window_highlights_every_row() {
        // Every caller other than Write renders uncapped, so it must
        // keep full highlighting.
        for row in [0, 10, 2500, 4999] {
            assert!(row_is_highlighted(row, 5000, None));
        }
    }

    #[test]
    fn a_window_smaller_than_the_diff_still_highlights_everything() {
        // Head and tail overlapping means the whole diff survives the
        // cap, so nothing should be skipped.
        let window = Some(HighlightWindow { head_rows: 10, tail_rows: 50 });
        for row in 0..20 {
            assert!(row_is_highlighted(row, 20, window));
        }
    }

    #[test]
    fn windowed_render_keeps_the_same_text_as_a_full_render() {
        // The window changes which rows get syntect colours, never the
        // rows themselves - so line count and text must not move.
        let new_text: String = (0..400).fold(String::new(), |mut s, i| {
            use std::fmt::Write;
            writeln!(s, "let value_{i} = compute({i});").unwrap();
            s
        });
        let diff = model::Diff::new("src/main.rs", new_text.as_str());

        let full = render_diff(&diff, 80, None);
        let windowed =
            render_diff(&diff, 80, Some(HighlightWindow { head_rows: 10, tail_rows: 50 }));

        let text_of = |lines: &[Line<'static>]| -> Vec<String> {
            lines
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect()
        };
        assert_eq!(text_of(&full), text_of(&windowed));
    }

    #[test]
    fn render_diff_includes_repository_label() {
        let lines = render_diff(
            &model::Diff::new("src/main.rs", "fn main() {}\n")
                .old_text(Some("fn old() {}\n"))
                .repository(Some("stargate/project".to_owned())),
            80,
            None,
        );
        let repository_line: String =
            lines[0].spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(repository_line.contains("[stargate/project]"));
    }

    #[test]
    fn looks_like_unified_diff_detects_git_style_payload() {
        let raw = "diff --git a/a.rs b/a.rs\nindex 111..222 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
        assert!(looks_like_unified_diff(raw));
    }

    #[test]
    fn render_raw_unified_diff_styles_hunks_and_additions() {
        let raw = "--- a/file.rs\n+++ b/file.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let lines = render_raw_unified_diff(raw);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[2].spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(lines[4].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn render_diff_adds_line_numbers_and_hanging_indent() {
        let lines = render_diff(
            &model::Diff::new(
                "tmp.md",
                "This is a long added line that should wrap onto another visual line.\n".to_owned(),
            ),
            28,
            None,
        );
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(rendered.iter().any(|line| line.contains("lines +1")));
        assert!(rendered.iter().any(|line| line.contains("1  +  This is a long")));
        assert!(rendered.iter().any(|line| line.starts_with("      ")));
        assert!(!rendered.iter().any(|line| line == "tmp.md"));
    }

    #[test]
    fn render_diff_preserves_source_indentation() {
        let lines = render_diff(
            &model::Diff::new(
                "tmp.rs",
                "fn main() {\n    if true {\n        return;\n    }\n}\n".to_owned(),
            ),
            80,
            None,
        );
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(rendered.iter().any(|line| line.contains("+      if true {")));
        assert!(rendered.iter().any(|line| line.contains("+          return;")));
    }

    #[test]
    fn render_diff_preserves_source_indentation_for_wrapped_lines() {
        let lines = render_diff(
            &model::Diff::new(
                "tmp.rs",
                "        This is a long added line that should wrap with indentation preserved.\n"
                    .to_owned(),
            ),
            28,
            None,
        );
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(rendered.iter().any(|line| line.contains("+          This is a")));
        assert!(rendered.iter().any(|line| line.contains("indentation")));
        assert!(rendered.iter().any(|line| line.starts_with("              ")));
    }

    /// A raw tab measured one column and painted none, so a Go indent
    /// disappeared and the row's wrap budget was over-charged for it.
    /// Tabs expand to 4-column stops, the depth a space-indented Rust
    /// file already renders at.
    #[test]
    fn render_diff_expands_tabs_to_match_a_space_indent() {
        let rendered = |source: &str| -> Vec<String> {
            render_diff(&model::Diff::new("tmp.go", source.to_owned()), 80, None)
                .iter()
                .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
                .collect()
        };

        let nested = rendered("func main() {\n\tif err != nil {\n\t\treturn err\n\t}\n}\n");
        assert!(
            nested.iter().all(|line| !line.contains('\t')),
            "no raw tab may reach the terminal: {nested:?}"
        );
        assert!(nested.iter().any(|line| line.contains("+      if err != nil {")));
        assert!(nested.iter().any(|line| line.contains("+          return err")));

        // Verbatim gofmt output: tab-indented, space-aligned. Expanding
        // the indent must not shear the alignment it pads out to.
        let aligned =
            rendered("type Foo struct {\n\tName          string\n\tLongFieldName bool\n}\n");
        let column_of = |needle: &str| {
            aligned
                .iter()
                .find_map(|line| line.find(needle))
                .unwrap_or_else(|| panic!("no row containing `{needle}`"))
        };
        assert_eq!(
            column_of("string"),
            column_of("bool"),
            "gofmt alignment must survive: {aligned:?}"
        );

        // Nothing else emits spaces for alignment, so an interior tab
        // has to expand too or it shears the row on its own.
        let hand_tabbed = rendered("var (\n\tx\t= 1\n)\n");
        assert!(
            hand_tabbed.iter().all(|line| !line.contains('\t')),
            "no raw tab may reach the terminal: {hand_tabbed:?}"
        );
        assert!(
            hand_tabbed.iter().any(|line| line.contains("x   = 1")),
            "a tab at column 5 advances to column 8: {hand_tabbed:?}"
        );

        // A 2-column grapheme ahead of a tab, where per-char and
        // string-level width measurement disagree.
        let grapheme = rendered("var (\n\t\u{2764}\u{fe0f}\tx = 1\n)\n");
        assert!(
            grapheme.iter().any(|line| line.contains("\u{2764}\u{fe0f}  x = 1")),
            "a 2-column grapheme leaves the tab advancing 2: {grapheme:?}"
        );
    }

    #[test]
    fn compact_hunk_header_omits_empty_side_and_uses_ranges() {
        assert_eq!(format_compact_hunk_header("@@ -0,0 +1,7 @@"), "lines +1-7");
        assert_eq!(format_compact_hunk_header("@@ -4,3 +4,5 @@"), "lines -4-6 +4-8");
        assert_eq!(format_compact_hunk_header("@@ -8 +8 @@"), "lines -8 +8");
    }

    /// A Rust insert line should pick up syntect tokenization: the
    /// body cell renders as multiple spans with distinct foreground
    /// colors (keyword vs. identifier vs. string literal), not a
    /// single uniform Insert-Green block.
    #[test]
    fn render_diff_applies_syntect_highlighting_to_change_body() {
        let lines = render_diff(
            &model::Diff::new(
                "src/lib.rs",
                "fn hello() -> &'static str {\n    \"world\"\n}\n".to_owned(),
            ),
            80,
            None,
        );
        let rust_line = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.contains("hello")))
            .expect("inserted rust line rendered");
        // Collect distinct fg colors across the body spans (anything
        // past the marker/gutter prefix). Without syntect, every
        // body span would carry the Insert Green color. With syntect,
        // tokenization yields at least two distinct fg colors for a
        // Rust function-declaration line.
        let body_colors: std::collections::HashSet<_> = rust_line
            .spans
            .iter()
            .filter(|span| !span.content.trim().is_empty())
            .filter_map(|span| span.style.fg)
            .collect();
        assert!(
            body_colors.len() >= 2,
            "rust insert line should expose >=2 distinct fg colors (syntect-tokenized), got: {body_colors:?}"
        );
    }

    /// Each row's line-level background carries the GitHub-style
    /// added / deleted tint - insert lines fill the row with
    /// `DIFF_ADDITION_BG`, delete lines with `DIFF_DELETION_BG`,
    /// context rows stay un-tinted. Covers both render paths
    /// (`render_diff` for the Edit-tool inline shape and
    /// `render_raw_unified_diff` for the slash-command path).
    #[test]
    fn render_diff_tints_change_rows_with_github_bg() {
        let lines = render_diff(
            &model::Diff::new("src/lib.rs", "fn one() {}\nfn TWO() {}\nfn three() {}\n".to_owned())
                .old_text(Some("fn one() {}\nfn two() {}\nfn three() {}\n")),
            80,
            None,
        );
        let line_for = |needle: &str| {
            lines
                .iter()
                .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
                .unwrap_or_else(|| panic!("no row containing `{needle}`"))
        };
        let insert_line = line_for("TWO");
        let delete_line = line_for("two");
        let context_line = line_for("one");
        assert_eq!(insert_line.style.bg, Some(theme::DIFF_ADDITION_BG));
        assert_eq!(delete_line.style.bg, Some(theme::DIFF_DELETION_BG));
        assert!(context_line.style.bg.is_none(), "context row stays un-tinted: {context_line:?}");
    }

    /// #211 regression: when row_bg is set, the row must be padded to
    /// full panel width with a trailing bg-styled space span so the
    /// tint visually extends across the whole row (ratatui's
    /// `Line.style.bg` alone only paints behind the actual span
    /// chars; the right-of-content gap stays terminal-default).
    /// Pre-#211, the bg was set on Line.style but no padding ran, so
    /// the user saw a thin sliver of tint behind the text instead of
    /// a full GitHub-style row band.
    #[test]
    fn render_diff_pads_tinted_rows_to_full_panel_width() {
        let width: u16 = 80;
        let lines = render_diff(
            &model::Diff::new("src/lib.rs", "fn TWO() {}\n".to_owned())
                .old_text(Some("fn two() {}\n")),
            width,
            None,
        );
        let usize_width = usize::from(width);

        let line_for = |needle: &str| {
            lines
                .iter()
                .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
                .unwrap_or_else(|| panic!("no row containing `{needle}`"))
        };

        for needle in ["TWO", "two"] {
            let row = line_for(needle);
            let total_width: usize = row.spans.iter().map(Span::width).sum();
            assert_eq!(
                total_width, usize_width,
                "tinted {needle} row must pad to full panel width ({usize_width}); got {total_width} via {row:?}"
            );
            let trailing = row.spans.last().expect("trailing pad span");
            assert!(
                trailing.style.bg.is_some(),
                "trailing pad span must carry the row bg so the tint extends to the right edge"
            );
        }
    }

    /// The Bash-tool path. Stops are measured past the marker, so one
    /// tab indents 4 columns here exactly as it does in the inline diff
    /// and the overlay.
    #[test]
    fn render_raw_unified_diff_expands_tabs() {
        let lines =
            render_raw_unified_diff("@@ -1 +1 @@\n+\tif err != nil {\n \tcontext\n\ttrailing\n");
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert!(
            rendered.iter().all(|line| !line.contains('\t')),
            "no raw tab may reach the terminal: {rendered:?}"
        );
        assert!(rendered.iter().any(|line| line == "+    if err != nil {"), "{rendered:?}");
        assert!(rendered.iter().any(|line| line == "     context"), "{rendered:?}");
        // An unclassified line has no marker to skip, so its own first
        // column is column 0.
        assert!(rendered.iter().any(|line| line == "    trailing"), "{rendered:?}");
    }

    #[test]
    fn render_raw_diff_line_tints_body_change_rows_not_headers() {
        let insert = render_raw_diff_line("+ added line");
        let delete = render_raw_diff_line("- removed line");
        let context = render_raw_diff_line(" context line");
        let hunk = render_raw_diff_line("@@ -1,3 +1,3 @@");
        let file_header = render_raw_diff_line("+++ b/src/lib.rs");
        assert_eq!(insert.style.bg, Some(theme::DIFF_ADDITION_BG));
        assert_eq!(delete.style.bg, Some(theme::DIFF_DELETION_BG));
        assert!(context.style.bg.is_none());
        assert!(hunk.style.bg.is_none(), "hunk header stays un-tinted");
        assert!(file_header.style.bg.is_none(), "+++ file header stays un-tinted");
    }

    /// Context lines (unchanged equal rows) must keep the DIM modifier
    /// on their body spans so they stay visually distinct from change
    /// rows even after syntect tokenization.
    #[test]
    fn render_diff_dims_context_line_bodies() {
        let lines = render_diff(
            &model::Diff::new("src/lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n".to_owned())
                .old_text(Some("fn one() {}\nfn two() {}\nfn THREE() {}\n")),
            80,
            None,
        );
        // The unchanged `fn one` line is a context row. Its body spans
        // should carry Modifier::DIM (composed with syntect colors).
        let context_line = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.contains("one")))
            .expect("context line rendered");
        let has_dim_body_span = context_line.spans.iter().any(|span| {
            !span.content.trim().is_empty() && span.style.add_modifier.contains(Modifier::DIM)
        });
        assert!(
            has_dim_body_span,
            "context line body should carry Modifier::DIM (composed with syntect), spans: {:?}",
            context_line.spans
        );
    }

    #[test]
    fn lang_from_title_handles_common_paths_and_edge_cases() {
        let cases = [
            ("src/main.rs", "rs"),
            ("Read foo.py", "py"),
            ("Cargo.toml", "toml"),
            ("Makefile", ""),
            ("", ""),
            ("file.RS", "rs"),
            ("archive.tar.gz", "gz"),
            ("Read some/dir/file.tsx", "tsx"),
            (".gitignore", "gitignore"),
            ("Read a.test.spec.ts", "ts"),
            ("file.", ""),
            ("   ", ""),
            ("Read src\\main.rs", "rs"),
        ];

        for (title, expected) in cases {
            assert_eq!(lang_from_title(title), expected, "title: {title:?}");
        }
    }

    #[test]
    fn is_markdown_file_matches_supported_extensions_only() {
        let supported = [
            "README.md",
            "component.mdx",
            "doc.markdown",
            "README.MD",
            "file.Md",
            "docs/getting-started.md",
            "Read /home/user/notes.md",
            "FILE.MARKDOWN",
        ];
        for path in supported {
            assert!(is_markdown_file(path), "path should be markdown: {path:?}");
        }

        let unsupported = ["main.rs", "style.css", "", "somemdx", "file.md.bak"];
        for path in unsupported {
            assert!(!is_markdown_file(path), "path should not be markdown: {path:?}");
        }
    }
}
