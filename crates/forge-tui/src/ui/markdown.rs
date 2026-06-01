// `catch_unwind` is allow-listed here: `tui_markdown::from_str` can
// panic on malformed input, and the TUI legitimately needs to fall
// back to plain text rather than crash the user's session. The
// workspace `clippy.toml` disallows `catch_unwind` in library code;
// this binary-crate UI fallback is the documented exception.
#![allow(clippy::disallowed_methods)]

use std::borrow::Cow;
use std::cell::Cell;
use std::panic::{self, AssertUnwindSafe};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

thread_local! {
    /// Set while a markdown render runs inside the `catch_unwind` guard
    /// below. The process-wide panic hook (see `app::install_panic_hook`)
    /// reads it to tell a panic forge will swallow here from a genuine
    /// crash: the former must leave the terminal untouched (ratatui's
    /// own hook restores it, which is what corrupts the live TUI), the
    /// latter must tear the terminal down before printing the backtrace.
    static IN_GUARDED_RENDER: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current thread is mid-`catch_unwind` over a markdown
/// render. Read by the panic hook to decide whether to keep its hands
/// off the terminal.
pub(crate) fn in_guarded_render() -> bool {
    IN_GUARDED_RENDER.with(Cell::get)
}

/// RAII flag for [`IN_GUARDED_RENDER`]. Saves and restores the prior
/// value so nested renders compose correctly.
struct RenderGuard(bool);

impl RenderGuard {
    fn enter() -> Self {
        Self(IN_GUARDED_RENDER.with(|c| c.replace(true)))
    }
}

impl Drop for RenderGuard {
    fn drop(&mut self) {
        IN_GUARDED_RENDER.with(|c| c.set(self.0));
    }
}

pub(super) fn render_markdown_safe(text: &str, bg: Option<Color>) -> Vec<Line<'static>> {
    render_markdown_safe_with(text, bg, render_with_tui_markdown)
}

fn render_markdown_safe_with<F>(text: &str, bg: Option<Color>, renderer: F) -> Vec<Line<'static>>
where
    F: FnOnce(&str, Option<Color>) -> Vec<Line<'static>>,
{
    let _guard = RenderGuard::enter();
    if let Ok(lines) = panic::catch_unwind(AssertUnwindSafe(|| renderer(text, bg))) {
        lines
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_RENDER,
            event_name = "markdown_render_failed",
            message = "markdown renderer panicked; falling back to plain text",
            outcome = "fallback",
        );
        plain_text_fallback(text, bg)
    }
}

fn render_with_tui_markdown(text: &str, bg: Option<Color>) -> Vec<Line<'static>> {
    let normalized = normalize_task_list_markers(text);
    let rendered = tui_markdown::from_str(&normalized);
    rendered
        .lines
        .into_iter()
        .map(|line| {
            let owned_spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| {
                    let style =
                        if let Some(bg_color) = bg { span.style.bg(bg_color) } else { span.style };
                    Span::styled(span.content.into_owned(), style)
                })
                .collect();
            let line_style =
                if let Some(bg_color) = bg { line.style.bg(bg_color) } else { line.style };
            Line::from(owned_spans).style(line_style)
        })
        .collect()
}

/// Checkbox glyphs substituted for GFM task-list markers. U+2610 BALLOT
/// BOX / U+2611 BALLOT BOX WITH CHECK, written as escapes so the source
/// stays ASCII (the Unicode-punctuation gate's preferred form).
const UNCHECKED_GLYPH: char = '\u{2610}';
const CHECKED_GLYPH: char = '\u{2611}';

/// Rewrite `- [ ]` / `- [x]` bullet task-list markers to plain text
/// checkbox glyphs before handing the source to `tui_markdown`.
///
/// `tui-markdown` 0.3.7 panics on a *loose* task list (items separated
/// by blank lines): its `task_list_marker` handler inserts the `[ ]`
/// span at index 1 of a line whose span vec is empty, tripping
/// `Vec::insert`'s bounds check (`lib.rs:469`). Turning the marker into
/// ordinary text means the crate never emits a `TaskListMarker` event,
/// so the buggy path is never reached - and the rendered checkbox reads
/// better than the raw `[ ]` did. Only bullet lists (`- * +`) are
/// touched; ordered task lists don't hit the panic. Lines inside fenced
/// code blocks are left verbatim.
fn normalize_task_list_markers(text: &str) -> Cow<'_, str> {
    if !text.contains('[') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let (content, newline) =
            line.strip_suffix('\n').map_or((line, ""), |stripped| (stripped, "\n"));
        let trimmed = content.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        if let Some(rewritten) = rewrite_task_line(content) {
            changed = true;
            out.push_str(&rewritten);
            out.push_str(newline);
        } else {
            out.push_str(line);
        }
    }
    if changed { Cow::Owned(out) } else { Cow::Borrowed(text) }
}

/// Rewrite a single line if it is a bullet task-list item, else `None`.
fn rewrite_task_line(content: &str) -> Option<String> {
    let indent_len = content.len() - content.trim_start().len();
    let (indent, rest) = content.split_at(indent_len);

    let bullet = rest.chars().next()?;
    if !matches!(bullet, '-' | '*' | '+') {
        return None;
    }
    let after_bullet = &rest[bullet.len_utf8()..];
    let ws_len = after_bullet.len() - after_bullet.trim_start_matches([' ', '\t']).len();
    if ws_len == 0 {
        return None;
    }
    let marker = &after_bullet[ws_len..];

    let bytes = marker.as_bytes();
    if bytes.len() < 3 || bytes[0] != b'[' || bytes[2] != b']' {
        return None;
    }
    let glyph = match bytes[1] {
        b' ' => UNCHECKED_GLYPH,
        b'x' | b'X' => CHECKED_GLYPH,
        _ => return None,
    };
    // GFM requires whitespace (or end of line) after the marker; the
    // same check disambiguates a real marker from a link like
    // `- [x](url)`, whose `(` fails here and is left untouched.
    let tail = &marker[3..];
    if !(tail.is_empty() || tail.starts_with([' ', '\t'])) {
        return None;
    }

    let mut rewritten = String::with_capacity(content.len());
    rewritten.push_str(indent);
    rewritten.push(bullet);
    rewritten.push(' ');
    rewritten.push(glyph);
    rewritten.push_str(tail);
    Some(rewritten)
}

fn plain_text_fallback(text: &str, bg: Option<Color>) -> Vec<Line<'static>> {
    let style =
        if let Some(bg_color) = bg { Style::default().bg(bg_color) } else { Style::default() };

    text.split('\n').map(|line| Line::from(Span::styled(line.to_owned(), style))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::catch_unwind;

    #[test]
    fn render_markdown_safe_handles_common_and_edge_case_inputs_without_panicking() {
        let inputs = [
            "- [ ] one\n- [x] two",
            "- [ ] Move todos below input top line",
            "- [ ]\n- [x]\n- [ ]",
            "- [x] done\n  - [ ] child",
            "1. [ ] numbered checklist marker",
            "[]()[]()[]()",
            "```md\n- [ ] fenced checklist\n```",
            "> - [ ] blockquote checklist\n>\n> text",
            "# Heading\n- [ ] item\n\n| a | b |\n|---|---|\n| x | y |",
            "- [ ] [link](https://example.com) [",
            "- [ ] \u{200d}\u{200d}\u{200d}",
        ];

        for input in inputs {
            let result = catch_unwind(|| render_markdown_safe(input, None));
            assert!(result.is_ok(), "input triggered panic: {input}");
            assert!(!result.unwrap().is_empty(), "input rendered zero lines: {input}");
        }
    }

    /// The bug that started this: a *loose* task list (blank line between
    /// items) panics `tui_markdown::from_str` at `lib.rs:469`. Feed the
    /// normalized form to `from_str` DIRECTLY (no `catch_unwind`) so a
    /// regression of `normalize_task_list_markers` surfaces as a real
    /// test panic rather than being masked by the fallback path.
    #[test]
    fn normalized_loose_task_lists_do_not_panic_tui_markdown() {
        let loose_inputs = [
            "- [ ] one\n\n- [x] two",
            "- [ ] one\n\n  para\n\n- [ ] two",
            "* [ ] star\n\n* [x] star two",
            "+ [x] plus\n\n+ [ ] plus two",
        ];
        for input in loose_inputs {
            let normalized = normalize_task_list_markers(input);
            // Would panic pre-fix; the assertion is "this line returns".
            let rendered = tui_markdown::from_str(&normalized);
            assert!(!rendered.lines.is_empty(), "rendered zero lines: {input}");
        }
    }

    #[test]
    fn normalize_converts_bullet_task_markers_to_glyphs() {
        let out = normalize_task_list_markers("- [ ] todo\n- [x] done\n- [X] also done");
        assert_eq!(out.as_ref(), "- \u{2610} todo\n- \u{2611} done\n- \u{2611} also done");
    }

    #[test]
    fn normalize_leaves_links_and_non_task_brackets_alone() {
        // A link whose text is `x` must not be mistaken for a checkbox.
        assert_eq!(normalize_task_list_markers("- [x](url)").as_ref(), "- [x](url)");
        assert_eq!(normalize_task_list_markers("- [link](url)").as_ref(), "- [link](url)");
        assert_eq!(normalize_task_list_markers("plain [x] inline").as_ref(), "plain [x] inline");
    }

    #[test]
    fn normalize_skips_fenced_code_blocks() {
        let input = "```\n- [ ] not a task in code\n```\n- [ ] real task";
        let out = normalize_task_list_markers(input);
        assert_eq!(out.as_ref(), "```\n- [ ] not a task in code\n```\n- \u{2610} real task");
    }

    #[test]
    fn normalize_borrows_when_no_markers_present() {
        assert!(matches!(normalize_task_list_markers("plain text\nno markers"), Cow::Borrowed(_)));
        assert!(matches!(normalize_task_list_markers("- bullet, no checkbox"), Cow::Borrowed(_)));
    }

    #[test]
    fn normalize_handles_bare_marker_without_trailing_text() {
        assert_eq!(normalize_task_list_markers("- [ ]").as_ref(), "- \u{2610}");
        assert_eq!(normalize_task_list_markers("  - [x]").as_ref(), "  - \u{2611}");
    }

    #[test]
    fn render_guard_sets_and_restores_flag() {
        assert!(!in_guarded_render());
        {
            let _outer = RenderGuard::enter();
            assert!(in_guarded_render());
            {
                let _inner = RenderGuard::enter();
                assert!(in_guarded_render());
            }
            // Inner drop restores the prior (still-guarded) value.
            assert!(in_guarded_render());
        }
        assert!(!in_guarded_render());
    }

    #[test]
    fn render_markdown_safe_clears_guard_after_panic() {
        let result = render_markdown_safe_with("anything", None, |_text, _bg| {
            panic!("forced renderer panic")
        });
        assert_eq!(result.len(), 1);
        // A caught panic must not leave the guard stuck on.
        assert!(!in_guarded_render());
    }

    #[test]
    fn render_markdown_safe_falls_back_to_plain_text_and_preserves_requested_bg() {
        let lines = render_markdown_safe_with("line1\nline2", Some(Color::Blue), |_text, _bg| {
            panic!("forced renderer panic for fallback path")
        });
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[0].content.as_ref(), "line1");
        assert_eq!(lines[1].spans[0].content.as_ref(), "line2");
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Blue));
        assert_eq!(lines[1].spans[0].style.bg, Some(Color::Blue));
    }
}
