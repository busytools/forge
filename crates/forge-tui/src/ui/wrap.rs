use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub(crate) struct StyledChunk {
    pub text: String,
    pub style: Style,
}

struct StyledGrapheme<'a> {
    text: &'a str,
    style: Style,
    width: usize,
}

enum WrapToken {
    /// Both variants index the flattened grapheme run as `start..end`.
    Text {
        start: usize,
        end: usize,
        width: usize,
    },
    Space {
        start: usize,
        end: usize,
        width: usize,
    },
    Newline,
}

pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub(crate) fn line_display_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|span| display_width(span.content.as_ref())).sum()
}

pub(crate) fn truncate_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) <= width {
        return text.to_owned();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let grapheme_width = display_width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        out.push_str(grapheme);
        used += grapheme_width;
    }
    out
}

pub(crate) fn take_prefix_by_width(text: &str, width: usize) -> (String, String) {
    if width == 0 || text.is_empty() {
        return (String::new(), text.to_owned());
    }

    let mut used = 0usize;
    let mut split_at = 0usize;
    for (idx, grapheme) in UnicodeSegmentation::grapheme_indices(text, true) {
        let grapheme_width = display_width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        used += grapheme_width;
        split_at = idx + grapheme.len();
    }

    if split_at == 0 {
        return (String::new(), text.to_owned());
    }

    (text[..split_at].to_owned(), text[split_at..].to_owned())
}

pub(crate) fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    wrap_styled_chunks(&[StyledChunk { text: text.to_owned(), style: Style::default() }], width)
        .into_iter()
        .map(|line| line.spans.into_iter().map(|span| span.content.into_owned()).collect())
        .collect()
}

pub(crate) fn wrapped_line_count(text: &str, width: usize) -> usize {
    wrap_plain(text, width).len().max(1)
}

pub(crate) fn wrap_styled_chunks(chunks: &[StyledChunk], width: usize) -> Vec<Line<'static>> {
    if width == 0 || chunks.is_empty() {
        return vec![Line::default()];
    }

    let graphemes = flatten_chunks(chunks);
    let tokens = tokenize_graphemes(&graphemes);
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut line_width = 0usize;
    let mut pending_spaces = Vec::<(usize, usize, usize)>::new();

    for token in tokens {
        match token {
            WrapToken::Newline => {
                finish_wrapped_line(&mut lines, &mut spans, &mut line_width);
                pending_spaces.clear();
            }
            WrapToken::Space { start, end, width: space_width } => {
                if line_width > 0 {
                    pending_spaces.push((start, end, space_width));
                }
            }
            WrapToken::Text { start, end, width: text_width } => {
                let pending_width: usize =
                    pending_spaces.iter().map(|(_, _, space_width)| space_width).sum();
                if line_width > 0 && line_width + pending_width + text_width > width {
                    finish_wrapped_line(&mut lines, &mut spans, &mut line_width);
                    pending_spaces.clear();
                }

                if line_width > 0 {
                    for (space_start, space_end, space_width) in pending_spaces.drain(..) {
                        push_graphemes(&mut spans, &graphemes[space_start..space_end]);
                        line_width += space_width;
                    }
                }

                if text_width <= width.saturating_sub(line_width) {
                    push_graphemes(&mut spans, &graphemes[start..end]);
                    line_width += text_width;
                    continue;
                }

                wrap_long_token(
                    &graphemes[start..end],
                    width,
                    &mut lines,
                    &mut spans,
                    &mut line_width,
                );
            }
        }
    }

    finish_wrapped_line(&mut lines, &mut spans, &mut line_width);
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

pub(crate) fn pad_line_to_width(
    mut line: Line<'static>,
    width: usize,
    padding_style: Style,
) -> Line<'static> {
    let padding = width.saturating_sub(line_display_width(&line));
    if padding > 0 {
        line.spans.push(Span::styled(" ".repeat(padding), padding_style));
    }
    line
}

pub(crate) fn blank_line(width: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled(" ".repeat(width), style))
}

fn flatten_chunks(chunks: &[StyledChunk]) -> Vec<StyledGrapheme<'_>> {
    chunks
        .iter()
        .flat_map(|chunk| {
            UnicodeSegmentation::graphemes(chunk.text.as_str(), true).map(move |grapheme| {
                StyledGrapheme {
                    text: grapheme,
                    style: chunk.style,
                    width: display_width(grapheme),
                }
            })
        })
        .collect()
}

/// Break opportunities inside a run of non-whitespace. Syntax
/// highlighting splits a line into one chunk per scope, so tokenising
/// per chunk made the wrapped line count depend on whether a row was
/// highlighted (#522); deciding breaks from the characters alone keeps
/// the two identical. Apostrophes, hyphens and underscores are left out
/// so ordinary words, `->` and Rust lifetimes stay whole.
fn breaks_after(grapheme: &str) -> bool {
    matches!(
        grapheme,
        "." | ","
            | ";"
            | ":"
            | "/"
            | "\\"
            | "("
            | ")"
            | "["
            | "]"
            | "{"
            | "}"
            | "<"
            | ">"
            | "="
            | "&"
            | "|"
            | "+"
            | "*"
            | "#"
            | "?"
            | "!"
            | "%"
            | "@"
            | "~"
            | "^"
            | "\""
    )
}

fn tokenize_graphemes(graphemes: &[StyledGrapheme<'_>]) -> Vec<WrapToken> {
    let mut tokens = Vec::new();
    let mut start = 0usize;
    let mut width = 0usize;
    let mut is_space: Option<bool> = None;

    let flush = |tokens: &mut Vec<WrapToken>,
                 start: &mut usize,
                 end: usize,
                 width: &mut usize,
                 is_space: &mut Option<bool>| {
        if end == *start {
            return;
        }
        let token = if is_space.unwrap_or(false) {
            WrapToken::Space { start: *start, end, width: *width }
        } else {
            WrapToken::Text { start: *start, end, width: *width }
        };
        tokens.push(token);
        *start = end;
        *width = 0;
        *is_space = None;
    };

    for (index, grapheme) in graphemes.iter().enumerate() {
        if grapheme.text == "\n" {
            flush(&mut tokens, &mut start, index, &mut width, &mut is_space);
            start = index + 1;
            tokens.push(WrapToken::Newline);
            continue;
        }

        let grapheme_is_space = grapheme.text.chars().all(char::is_whitespace)
            && grapheme.text.chars().all(|ch| ch != '\n');
        if is_space.is_some_and(|value| value != grapheme_is_space) {
            flush(&mut tokens, &mut start, index, &mut width, &mut is_space);
        }

        is_space = Some(grapheme_is_space);
        width += grapheme.width;

        if !grapheme_is_space && breaks_after(grapheme.text) {
            flush(&mut tokens, &mut start, index + 1, &mut width, &mut is_space);
        }
    }

    flush(&mut tokens, &mut start, graphemes.len(), &mut width, &mut is_space);
    tokens
}

fn wrap_long_token(
    token: &[StyledGrapheme<'_>],
    width: usize,
    lines: &mut Vec<Line<'static>>,
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
) {
    let mut segment_start = 0usize;
    let mut segment_width = 0usize;

    for (index, grapheme) in token.iter().enumerate() {
        if *line_width > 0 && *line_width + segment_width + grapheme.width > width {
            if index > segment_start {
                push_graphemes(spans, &token[segment_start..index]);
                *line_width += segment_width;
                segment_start = index;
                segment_width = 0;
            }
            finish_wrapped_line(lines, spans, line_width);
        }

        if segment_width + grapheme.width > width && index > segment_start {
            push_graphemes(spans, &token[segment_start..index]);
            *line_width += segment_width;
            segment_start = index;
            segment_width = 0;
            finish_wrapped_line(lines, spans, line_width);
        }

        segment_width += grapheme.width;
    }

    if segment_start < token.len() {
        push_graphemes(spans, &token[segment_start..]);
        *line_width += segment_width;
    }
}

fn finish_wrapped_line(
    lines: &mut Vec<Line<'static>>,
    spans: &mut Vec<Span<'static>>,
    line_width: &mut usize,
) {
    lines.push(Line::from(std::mem::take(spans)));
    *line_width = 0;
}

fn push_graphemes(spans: &mut Vec<Span<'static>>, graphemes: &[StyledGrapheme<'_>]) {
    for grapheme in graphemes {
        push_styled_text(spans, grapheme.text, grapheme.style);
    }
}

fn push_styled_text(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
        return;
    }
    spans.push(Span::styled(text.to_owned(), style));
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::style::Modifier;

    #[test]
    fn take_prefix_by_width_handles_grapheme_clusters() {
        let text = "a👩‍💻b";
        let (chunk, rest) = take_prefix_by_width(text, 3);
        assert_eq!(chunk, "a👩‍💻");
        assert_eq!(rest, "b");
    }

    #[test]
    fn wrap_plain_preserves_explicit_newlines() {
        assert_eq!(wrap_plain("alpha\nbeta", 16), vec!["alpha".to_owned(), "beta".to_owned()]);
    }

    #[test]
    fn wrap_plain_handles_cjk_width() {
        assert_eq!(wrap_plain("你好 世界", 4), vec!["你好".to_owned(), "世界".to_owned()]);
    }

    #[test]
    fn wrap_plain_wraps_long_emoji_graphemes() {
        assert_eq!(wrap_plain("👩‍💻👩‍💻👩‍💻", 4), vec!["👩‍💻👩‍💻".to_owned(), "👩‍💻".to_owned()]);
    }

    /// Realistic code lines that all wrap at the widths below. Ordinary
    /// prose is in the set too: it is the case where syntect emits one
    /// scope for the whole line, so it must keep working unchanged.
    const CHUNK_INVARIANCE_LINES: [&str; 6] = [
        "let x0 = self.alpha().bravo().charlie().delta().echo().foxtrot();",
        "use aaaa0::bbbbbb::cccccc::dddddd::eeeeee::ffffff::gggggg::hhhhhh;",
        "const S0: &str = \"abcdefghij_klmnopqrst_uvwxyz0123_456789ABCD\";",
        "map.insert(format!(\"key-{}\", 0), Value::Object(nested.clone()));",
        "// a fairly ordinary comment with plenty of separate words in it",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ];

    fn line_texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn wrapping_is_independent_of_chunk_boundaries() {
        // #522: chunk boundaries come from syntect scopes, so as long as
        // tokenisation flushes at them, the wrapped line count depends
        // on whether the row was highlighted. Same text + same style,
        // only the chunking varies, so any difference is the bug.
        for text in CHUNK_INVARIANCE_LINES {
            for width in [20usize, 28, 33, 40, 47, 60] {
                let whole = wrap_styled_chunks(
                    &[StyledChunk { text: text.to_owned(), style: Style::default() }],
                    width,
                );
                for step in [1usize, 2, 3, 5, 7] {
                    let chars: Vec<char> = text.chars().collect();
                    let split: Vec<StyledChunk> = chars
                        .chunks(step)
                        .map(|piece| StyledChunk {
                            text: piece.iter().collect(),
                            style: Style::default(),
                        })
                        .collect();
                    let chunked = wrap_styled_chunks(&split, width);
                    assert_eq!(
                        line_texts(&whole),
                        line_texts(&chunked),
                        "chunking changed the wrap: width={width} step={step} text={text:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn chunk_boundaries_do_not_change_the_wrapped_line_count() {
        // The count on its own is what the height pass consumes, so it
        // gets its own assertion rather than riding on the text compare.
        for text in CHUNK_INVARIANCE_LINES {
            for width in [20usize, 28, 33, 40, 47, 60] {
                let whole = wrap_styled_chunks(
                    &[StyledChunk { text: text.to_owned(), style: Style::default() }],
                    width,
                );
                let per_char: Vec<StyledChunk> = text
                    .chars()
                    .map(|ch| StyledChunk { text: ch.to_string(), style: Style::default() })
                    .collect();
                assert_eq!(
                    whole.len(),
                    wrap_styled_chunks(&per_char, width).len(),
                    "chunking changed the line count: width={width} text={text:?}"
                );
            }
        }
    }

    #[test]
    fn wrap_styled_chunks_preserves_styles() {
        let lines = wrap_styled_chunks(
            &[StyledChunk {
                text: "bold text".to_owned(),
                style: Style::default().add_modifier(Modifier::BOLD),
            }],
            32,
        );
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }
}
