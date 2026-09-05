//! Error rendering and tool-use error extraction for failed tool calls.

use crate::agent::model;
use crate::app::ToolCallInfo;
use crate::ui::theme;
use crate::ui::wrap::replace_control_chars;
use forge_workspace::translate::error_handling::{extract_xml_tag_value, truncate_for_log};
pub(super) use forge_workspace::translate::error_handling::{
    looks_like_internal_error, summarize_internal_error,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub(super) fn render_internal_failure_content(payload: &str) -> Vec<Line<'static>> {
    let summary = summarize_internal_error(payload);
    let mut lines = vec![Line::from(Span::styled(
        "Internal Agent SDK error",
        Style::default().fg(theme::STATUS_ERROR).add_modifier(Modifier::BOLD),
    ))];
    if !summary.is_empty() {
        lines.push(Line::from(Span::styled(summary, Style::default().fg(theme::STATUS_ERROR))));
    }
    lines
}

pub(super) fn render_tool_use_error_content(message: &str) -> Vec<Line<'static>> {
    message
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            Line::from(Span::styled(line.to_owned(), Style::default().fg(theme::STATUS_ERROR)))
        })
        .collect()
}

pub(super) fn debug_failed_tool_render(tc: &ToolCallInfo) {
    if !matches!(tc.status, model::ToolCallStatus::Failed | model::ToolCallStatus::Killed) {
        return;
    }

    let Some(text_payload) = tc.content.iter().find_map(|content| match content {
        model::RenderToolCallContent::Content(c) => match &c.content {
            model::RenderContentBlock::Text(t) => Some(t.text.as_str().to_owned()),
            model::RenderContentBlock::Image(_) => None,
        },
        _ => None,
    }) else {
        return;
    };
    if !looks_like_internal_error(&text_payload) {
        return;
    }
    let text_preview = summarize_internal_error(&text_payload);

    let terminal_preview = tc
        .terminal_output
        .as_deref()
        .map_or_else(|| "<no terminal output>".to_owned(), truncate_for_log);

    tracing::debug!(
        target: crate::logging::targets::APP_TOOL,
        event_name = "tool_error_payload_detected",
        message = "failed tool call payload detected during rendering",
        outcome = "degraded",
        tool_call_id = %tc.id,
        title = %tc.title,
        sdk_tool_name = %tc.sdk_tool_name,
        content_blocks = tc.content.len(),
        text_preview = %text_preview,
        terminal_preview = %terminal_preview,
    );
}

// `preview_for_log`, the renaming wrappers, and `extract_xml_tag_value`
// originals live in `crate::agent::error_handling` (re-exported `pub`
// from there). Consume directly via the imports above.

pub(super) fn failed_execute_first_line(output: &str) -> Option<String> {
    let first_line = extract_tool_use_error_message(output).or_else(|| {
        output.lines().find(|line| !line.trim().is_empty()).map(str::trim).map(str::to_owned)
    })?;
    Some(replace_control_chars(first_line.into()).into_owned())
}

pub(super) fn extract_tool_use_error_message(input: &str) -> Option<String> {
    extract_xml_tag_value(input, "tool_use_error")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}
