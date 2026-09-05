//! Rendering for non-Execute tool calls (Read, Write, Glob, etc.) and
//! content summary for collapsed tool calls.

use crate::agent::model;
use crate::app::ToolCallInfo;
use crate::ui::chat_tree;
use crate::ui::diff::{
    self, is_markdown_file, lang_from_title, render_diff, strip_outer_code_fence,
};
use crate::ui::highlight;
use crate::ui::markdown;
use crate::ui::theme;
use crate::ui::wrap::replace_control_chars;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::errors::{
    debug_failed_tool_render, extract_tool_use_error_message, failed_execute_first_line,
    looks_like_internal_error, render_internal_failure_content, render_tool_use_error_content,
};
use super::{
    ToolCallRenderContext, markdown_inline_spans, status_icon, tool_display_title,
    tool_output_badge_spans,
};

/// Cap for terminal output lines emitted into a Bash tool body. When
/// the output exceeds this, render a "... N lines hidden ..." banner
/// in DIM and show only the most recent <code>TERMINAL_MAX_LINES</code>
/// rows below it.
pub(super) const TERMINAL_MAX_LINES: usize = 12;

pub(super) const WRITE_DIFF_MAX_LINES: usize = 50;
pub(super) const WRITE_DIFF_HEAD_LINES: usize = 10;
const DEFAULT_COLLAPSED_TEXT_SUMMARY_LIMIT: usize = 60;
const DIFF_BODY_INDENT: &str = "  ";
const DIFF_BODY_INDENT_WIDTH: u16 = 2;

/// Render the title line for any tool call. Format:
///
/// ```text
///   <status icon> <kind icon> <kind label> <display title> <badges>
/// ```
///
/// Status icon is colored by status; kind icon and kind label are
/// white bold. The kind label comes from `theme::tool_name_label`
/// (e.g. "Read", "Edit", "Bash", "Subagent", or the fallback
/// "Tool"). When `tc.title` from claude already starts with the
/// kind label (e.g. claude often sends "Read /path/to/file.rs"),
/// the duplicate prefix is stripped so the column reads cleanly
/// (no "Read Read /path").
pub(super) fn render_tool_call_title(
    tc: &ToolCallInfo,
    render_context: ToolCallRenderContext<'_>,
    _width: u16,
    spinner_glyph: char,
) -> Line<'static> {
    let (icon, icon_color) = status_icon(tc.status, spinner_glyph);
    let (kind_icon, kind_name) = theme::tool_name_label(&tc.sdk_tool_name);
    let bold_white = Style::default().fg(ratatui::style::Color::White).add_modifier(Modifier::BOLD);

    let mut title_spans = vec![
        Span::styled(format!("  {icon} "), Style::default().fg(icon_color)),
        Span::styled(format!("{kind_icon} "), bold_white),
        Span::styled(format!("{kind_name} "), bold_white),
    ];

    let display_title = tool_display_title(tc, render_context);
    let title_text = strip_kind_prefix(display_title.as_ref(), kind_name);
    title_spans.extend(markdown_inline_spans(title_text));
    title_spans.extend(tool_output_badge_spans(tc));

    Line::from(title_spans)
}

/// Strip a leading "<kind_name> " prefix from `title` so the rendered
/// column doesn't repeat the tool name (e.g. claude sends
/// `"Read /path/to/file.rs"` and we already render the "Read" label
/// from `theme::tool_name_label`). Returns the original title when no
/// prefix matches.
fn strip_kind_prefix<'a>(title: &'a str, kind_name: &str) -> &'a str {
    if let Some(rest) = title.strip_prefix(kind_name)
        && let Some(after_space) = rest.strip_prefix(' ')
    {
        return after_space;
    }
    title
}

/// Render the body lines (everything after the title) for a non-Execute tool call.
/// Used for in-progress tool calls where the body is cached separately from the title.
/// Execute tool calls are handled separately via `render_execute_with_borders`.
pub(super) fn render_tool_call_body(tc: &ToolCallInfo, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    render_standard_body(tc, width, &mut lines);
    lines
}

pub(super) fn tool_call_body_depends_on_width(tc: &ToolCallInfo) -> bool {
    tc.content.iter().any(|content| matches!(content, model::RenderToolCallContent::Diff(_)))
}

pub(super) fn tool_call_effectively_collapsed(tc: &ToolCallInfo, tools_collapsed: bool) -> bool {
    // Carve-out kinds (Execute / Diff content / Monitor / Workflow)
    // render expanded regardless of the global directive. Their chat
    // paths (Execute live-streaming, diff view, render_lifecycle_one_liner)
    // bypass this function by construction; the helper here is the
    // belt to the rest of the system's suspenders.
    if crate::ui::collapse::is_carved_out_from_global_directive(tc) {
        return false;
    }
    crate::ui::collapse::resolve_collapsed_bool(tc.collapsed_override, tools_collapsed)
}

pub(super) fn render_collapsed_tool_call_summary(
    tc: &ToolCallInfo,
    lines: &mut Vec<Line<'static>>,
) {
    let pipe_style = Style::default().fg(theme::DIM);
    lines.push(Line::from(vec![
        Span::styled(format!("  {}", chat_tree::LAST), pipe_style),
        Span::styled(content_summary(tc), Style::default().fg(theme::DIM)),
        Span::styled("  click or ctrl+x to expand", Style::default().fg(theme::DIM)),
    ]));
}

/// Render the body (everything after the title line) of a tool call.
fn render_standard_body(tc: &ToolCallInfo, width: u16, lines: &mut Vec<Line<'static>>) {
    let pipe_style = Style::default().fg(theme::DIM);
    let has_execute_output = tc.is_execute_tool()
        && (tc.terminal_output.is_some() || matches!(tc.status, model::ToolCallStatus::InProgress));

    if tc.content.is_empty() && !has_execute_output {
        return;
    }

    // Expanded: render full content with | prefix on each line
    let content_lines = render_tool_content(tc, width.saturating_sub(5));

    let last_idx = content_lines.len().saturating_sub(1);
    for (i, content_line) in content_lines.into_iter().enumerate() {
        let prefix = if i == last_idx {
            format!("  {}", chat_tree::LAST)
        } else {
            format!("  {}  ", chat_tree::SPINE)
        };
        let mut spans = vec![Span::styled(prefix, pipe_style)];
        spans.extend(content_line.spans);
        lines.push(Line::from(spans));
    }
}

/// One-line summary for collapsed tool calls.
pub(super) fn content_summary(tc: &ToolCallInfo) -> String {
    // For Execute tool calls, show last non-empty line of terminal output
    if tc.terminal_id.is_some() {
        if let Some(ref output) = tc.terminal_output {
            let stripped_output = highlight::strip_ansi(output);
            if matches!(tc.status, model::ToolCallStatus::Failed | model::ToolCallStatus::Killed)
                && let Some(first_line) = failed_execute_first_line(&stripped_output)
            {
                return if first_line.chars().count() > 80 {
                    let truncated: String = first_line.chars().take(77).collect();
                    format!("{truncated}...")
                } else {
                    first_line
                };
            }
            let last = stripped_output.lines().rev().find(|l| !l.trim().is_empty());
            if let Some(line) = last {
                let summary = if line.chars().count() > 80 {
                    let truncated: String = line.chars().take(77).collect();
                    format!("{truncated}...")
                } else {
                    line.to_owned()
                };
                return replace_control_chars(summary.into()).into_owned();
            }
        }
        return if matches!(tc.status, model::ToolCallStatus::InProgress) {
            "running...".to_owned()
        } else {
            String::new()
        };
    }

    for content in &tc.content {
        match content {
            model::RenderToolCallContent::Diff(diff) => {
                let name = diff.path.file_name().map_or_else(
                    || diff.path.to_string_lossy().into_owned(),
                    |f| f.to_string_lossy().into_owned(),
                );
                return name;
            }
            model::RenderToolCallContent::McpResource(resource) => {
                if let Some(path) = &resource.blob_saved_to {
                    return path.file_name().map_or_else(
                        || path.to_string_lossy().into_owned(),
                        |f| f.to_string_lossy().into_owned(),
                    );
                }
                if let Some(text) = resource.text.as_deref() {
                    let first = text.lines().find(|line| !line.trim().is_empty()).unwrap_or("");
                    return truncate_summary_line(first, DEFAULT_COLLAPSED_TEXT_SUMMARY_LIMIT);
                }
                return resource.uri.clone();
            }
            model::RenderToolCallContent::Content(c) => {
                if let model::RenderContentBlock::Text(text) = &c.content {
                    let stripped = strip_outer_code_fence(&text.text);
                    if matches!(
                        tc.status,
                        model::ToolCallStatus::Failed | model::ToolCallStatus::Killed
                    ) && let Some(msg) = extract_tool_use_error_message(&stripped)
                    {
                        return msg;
                    }
                    let first = stripped.lines().next().unwrap_or("");
                    return truncate_summary_line(first, DEFAULT_COLLAPSED_TEXT_SUMMARY_LIMIT);
                }
            }
            model::RenderToolCallContent::Terminal(_) => {}
        }
    }
    String::new()
}

fn truncate_summary_line(line: &str, max_chars: usize) -> String {
    if line.chars().count() > max_chars {
        let truncated: String = line.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{truncated}...")
    } else {
        line.to_owned()
    }
}

/// Render the full content of a tool call as lines.
fn render_tool_content(tc: &ToolCallInfo, width: u16) -> Vec<Line<'static>> {
    let is_execute = tc.is_execute_tool();
    let mut lines: Vec<Line<'static>> = Vec::new();

    // For Execute tool calls with terminal output, render the live output
    if is_execute {
        if let Some(ref output) = tc.terminal_output {
            let stripped_output = highlight::strip_ansi(output);
            if matches!(tc.status, model::ToolCallStatus::Failed | model::ToolCallStatus::Killed)
                && let Some(first_line) = failed_execute_first_line(&stripped_output)
            {
                lines.push(Line::from(Span::styled(
                    first_line,
                    Style::default().fg(theme::STATUS_ERROR),
                )));
            } else {
                let raw_lines = highlight::render_terminal_output(output);
                let total = raw_lines.len();
                if total > TERMINAL_MAX_LINES {
                    let skipped = total - TERMINAL_MAX_LINES;
                    lines.push(Line::from(Span::styled(
                        format!("... {skipped} lines hidden ..."),
                        Style::default().fg(theme::DIM),
                    )));
                    lines.extend(raw_lines.into_iter().skip(skipped));
                } else {
                    lines.extend(raw_lines);
                }
            }
        } else if matches!(tc.status, model::ToolCallStatus::InProgress) {
            lines.push(Line::from(Span::styled("running...", Style::default().fg(theme::DIM))));
        }
        debug_failed_tool_render(tc);
        return lines;
    }

    for content in &tc.content {
        match content {
            model::RenderToolCallContent::Diff(diff) => {
                let is_write = tc.sdk_tool_name == "Write";
                // A Write has no old text, so the whole file arrives as
                // one insert hunk and every line of it used to be
                // highlighted before the cap discarded all but
                // `WRITE_DIFF_MAX_LINES`. Both bounds are in rows and
                // each row wraps to at least one line, so this covers
                // everything the cap can keep.
                let window = is_write.then_some(diff::HighlightWindow {
                    head_rows: WRITE_DIFF_HEAD_LINES,
                    tail_rows: WRITE_DIFF_MAX_LINES,
                });
                let raw = render_diff(diff, width.saturating_sub(DIFF_BODY_INDENT_WIDTH), window);
                let raw = if is_write { cap_write_diff_lines(raw) } else { raw };
                lines.extend(indent_rendered_lines(raw, DIFF_BODY_INDENT));
            }
            model::RenderToolCallContent::McpResource(resource) => {
                lines.extend(render_mcp_resource_content(tc, resource));
            }
            model::RenderToolCallContent::Content(c) => {
                if let model::RenderContentBlock::Text(text) = &c.content {
                    render_text_content(tc, &text.text, &mut lines);
                }
            }
            model::RenderToolCallContent::Terminal(_) => {}
        }
    }

    debug_failed_tool_render(tc);
    lines
}

fn render_text_content(tc: &ToolCallInfo, text: &str, lines: &mut Vec<Line<'static>>) {
    let stripped = strip_outer_code_fence(text);
    if matches!(tc.status, model::ToolCallStatus::Failed | model::ToolCallStatus::Killed)
        && let Some(msg) = extract_tool_use_error_message(&stripped)
    {
        lines.extend(render_tool_use_error_content(&msg));
        return;
    }
    if matches!(tc.status, model::ToolCallStatus::Failed | model::ToolCallStatus::Killed)
        && looks_like_internal_error(&stripped)
    {
        lines.extend(render_internal_failure_content(&stripped));
        return;
    }
    let md_source = if is_markdown_file(&tc.title) {
        stripped
    } else {
        let lang = lang_from_title(&tc.title);
        lines.extend(highlight::highlight_code(
            &stripped,
            (!lang.is_empty()).then_some(lang.as_str()),
        ));
        return;
    };
    for line in markdown::render_markdown_safe(&md_source, None) {
        let owned: Vec<Span<'static>> =
            line.spans.into_iter().map(|s| Span::styled(s.content.into_owned(), s.style)).collect();
        lines.push(Line::from(owned));
    }
}

fn render_mcp_resource_content(
    tc: &ToolCallInfo,
    resource: &model::McpResource,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(text) = resource.text.as_deref() {
        render_text_content(tc, text, &mut lines);
    }
    if let Some(blob_saved_to) = &resource.blob_saved_to {
        let saved_path = blob_saved_to.to_string_lossy().into_owned();
        let text_mentions_path =
            resource.text.as_deref().is_some_and(|text| text.contains(saved_path.as_str()));
        if !text_mentions_path {
            lines.push(Line::from(vec![
                Span::styled(
                    "Saved to: ",
                    Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
                ),
                Span::styled(saved_path, Style::default().fg(theme::DIM)),
            ]));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(resource.uri.clone(), Style::default().fg(theme::DIM))));
    }
    lines
}

fn indent_rendered_lines(lines: Vec<Line<'static>>, indent: &str) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(indent.to_owned())];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

pub(super) fn cap_write_diff_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    if lines.len() <= WRITE_DIFF_MAX_LINES {
        return lines;
    }
    let total = lines.len();
    let separator_lines = 3usize; // blank + marker + blank
    let head = WRITE_DIFF_HEAD_LINES.min(WRITE_DIFF_MAX_LINES.saturating_sub(separator_lines));
    let tail = WRITE_DIFF_MAX_LINES.saturating_sub(head + separator_lines);
    let tail_start = total.saturating_sub(tail);
    let omitted = tail_start.saturating_sub(head);

    let mut out = Vec::with_capacity(WRITE_DIFF_MAX_LINES);
    out.extend(lines.iter().take(head).cloned());
    out.push(Line::default());
    out.push(Line::from(Span::styled(
        format!("... {omitted} diff lines omitted ..."),
        Style::default().fg(theme::DIM).add_modifier(Modifier::ITALIC),
    )));
    out.push(Line::default());
    out.extend(lines.iter().skip(tail_start).cloned());
    out
}
