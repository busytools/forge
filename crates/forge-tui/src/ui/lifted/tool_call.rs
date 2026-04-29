#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    clippy::disallowed_methods,
    clippy::while_let_loop,
    clippy::collapsible_if,
    reason = "lifted upstream from claude-code-rust"
)]

//! Tool-call rendering: entry points, caching, and shared helpers.
//!
//! Submodules handle specific rendering concerns:
//! - [`standard`] -- non-Execute tool calls (Read, Write, Glob, etc.)
//! - [`execute`] -- Execute/Bash two-layer bordered rendering
//! - [`interactions`] -- inline permissions, questions, and plan approvals
//! - [`errors`] -- error rendering and tool-use error extraction

mod errors;
mod execute;
mod interactions;
mod standard;

use std::borrow::Cow;

use crate::state::model;
use crate::state::tool_call_info::ToolCallInfo;
use crate::ui::markdown;
use crate::ui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Spinner frames as `&'static str` for use in `status_icon` return type.
const SPINNER_STRS: &[&str] = &[
    "\u{280B}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283C}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280F}",
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolCallRenderContext<'a> {
    pub current_mode_id: Option<&'a str>,
}

pub fn status_icon(status: model::ToolCallStatus, spinner_frame: usize) -> (&'static str, Color) {
    match status {
        model::ToolCallStatus::Pending => ("\u{25CB}", theme::RUST_ORANGE),
        model::ToolCallStatus::InProgress => {
            let s = SPINNER_STRS[spinner_frame % SPINNER_STRS.len()];
            (s, theme::RUST_ORANGE)
        }
        model::ToolCallStatus::Completed => (theme::ICON_COMPLETED, theme::RUST_ORANGE),
        model::ToolCallStatus::Failed | model::ToolCallStatus::Killed => {
            (theme::ICON_FAILED, theme::STATUS_ERROR)
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points (delegating to submodules)
// ---------------------------------------------------------------------------

/// Render a tool call with caching. Only re-renders when cache is stale.
///
/// For Execute/Bash tool calls, the cache stores **content only** (command, output,
/// permissions) without border decoration. Borders are applied at render time using
/// the current width, so they always fill the terminal correctly after resize.
/// Height for Execute = `content_lines + 2` (title border + bottom border).
///
/// For other tool calls, the title is rendered live and the expanded body is cached
/// independently, so session collapse preference can change without invalidating
/// every completed tool-call cache.
pub fn render_tool_call_cached_with_tools_collapsed(
    tc: &mut ToolCallInfo,
    render_context: ToolCallRenderContext<'_>,
    width: u16,
    spinner_frame: usize,
    tools_collapsed: bool,
    out: &mut Vec<Line<'static>>,
) {
    let is_execute = tc.is_execute_tool();

    // Execute/Bash: two-layer rendering (cache content, apply borders at render time)
    if is_execute {
        if tc.cache.get().is_none() {
            crate::perf::mark("tc::cache_miss_execute");
            let _t = crate::perf::start("tc::render_exec");
            let content = execute::render_execute_content(tc);
            tc.cache.store(content);
        } else {
            crate::perf::mark("tc::cache_hit_execute");
        }
        if let Some(content) = tc.cache.get() {
            let bordered = execute::render_execute_with_borders(
                tc,
                render_context,
                content,
                width,
                spinner_frame,
            );
            out.extend(bordered);
        }
        return;
    }

    let title = standard::render_tool_call_title(tc, render_context, width, spinner_frame);
    out.push(title);

    let has_body = !(tc.content.is_empty()
        && tc.pending_permission.is_none()
        && tc.pending_question.is_none());
    if !has_body {
        return;
    }

    if standard::tool_call_effectively_collapsed(tc, tools_collapsed) {
        standard::render_collapsed_tool_call_summary(tc, out);
        return;
    }

    let body_depends_on_width = standard::tool_call_body_depends_on_width(tc);

    // Expanded body: use cache if valid, otherwise render and cache.
    let cached_body = if body_depends_on_width {
        tc.cache.get_for_width(width)
    } else {
        tc.cache.get()
    };
    if let Some(cached_body) = cached_body {
        crate::perf::mark_with("tc::cache_hit_body", "lines", cached_body.len());
        out.extend_from_slice(cached_body);
    } else {
        crate::perf::mark("tc::cache_miss_body");
        let _t = crate::perf::start("tc::render_body");
        let body = standard::render_tool_call_body(tc, width);
        if body_depends_on_width {
            tc.cache.store_for_width(body, width);
        } else {
            tc.cache.store(body);
        }
        let stored = if body_depends_on_width {
            tc.cache.get_for_width(width)
        } else {
            tc.cache.get()
        };
        if let Some(stored) = stored {
            out.extend_from_slice(stored);
        }
    }
}

/// Ensure tool call caches are up-to-date and return visual wrapped height at `width`.
/// Returns `(height, lines_wrapped_for_measurement)`.
pub fn measure_tool_call_height_cached_with_tools_collapsed(
    tc: &mut ToolCallInfo,
    render_context: ToolCallRenderContext<'_>,
    width: u16,
    spinner_frame: usize,
    layout_generation: u64,
    tools_collapsed: bool,
) -> (usize, usize) {
    if tc.cache_measurement_key_matches(width, layout_generation) {
        crate::perf::mark("tc_measure_fast_path_hits");
        return (tc.last_measured_height, 0);
    }
    crate::perf::mark("tc_measure_recompute_count");

    let is_execute = tc.is_execute_tool();
    if is_execute {
        if tc.cache.get().is_none() {
            let content = execute::render_execute_content(tc);
            tc.cache.store(content);
        }
        if let Some(content) = tc.cache.get() {
            let bordered = execute::render_execute_with_borders(
                tc,
                render_context,
                content,
                width,
                spinner_frame,
            );
            let h = Paragraph::new(Text::from(bordered.clone()))
                .wrap(Wrap { trim: false })
                .line_count(width);
            tc.cache.set_height(h, width);
            tc.record_measured_height(width, h, layout_generation);
            return (h, bordered.len());
        }
        tc.record_measured_height(width, 0, layout_generation);
        return (0, 0);
    }

    let title = standard::render_tool_call_title(tc, render_context, width, spinner_frame);
    let title_h = Paragraph::new(Text::from(vec![title]))
        .wrap(Wrap { trim: false })
        .line_count(width);
    let has_body = !(tc.content.is_empty()
        && tc.pending_permission.is_none()
        && tc.pending_question.is_none());

    if !has_body {
        tc.record_measured_height(width, title_h, layout_generation);
        return (title_h, 1);
    }

    if standard::tool_call_effectively_collapsed(tc, tools_collapsed) {
        let mut summary = Vec::new();
        standard::render_collapsed_tool_call_summary(tc, &mut summary);
        let summary_h = Paragraph::new(Text::from(summary.clone()))
            .wrap(Wrap { trim: false })
            .line_count(width);
        let total = title_h + summary_h;
        tc.record_measured_height(width, total, layout_generation);
        return (total, 1 + summary.len());
    }

    let body_depends_on_width = standard::tool_call_body_depends_on_width(tc);
    let cached_body = if body_depends_on_width {
        tc.cache.get_for_width(width)
    } else {
        tc.cache.get()
    };
    if cached_body.is_some() {
        if let Some(body_h) = tc.cache.height_at(width) {
            let total = title_h + body_h;
            tc.record_measured_height(width, total, layout_generation);
            return (total, 1);
        }
        if let Some(body_h) = tc.cache.measure_and_set_height(width) {
            let total = title_h + body_h;
            tc.record_measured_height(width, total, layout_generation);
            let cached_len = if body_depends_on_width {
                tc.cache
                    .get_for_width(width)
                    .map_or(1, |body| body.len() + 1)
            } else {
                tc.cache.get().map_or(1, |body| body.len() + 1)
            };
            return (total, cached_len);
        }
    }

    let body = standard::render_tool_call_body(tc, width);
    let body_h = Paragraph::new(Text::from(body.clone()))
        .wrap(Wrap { trim: false })
        .line_count(width);
    if body_depends_on_width {
        tc.cache.store_for_width(body, width);
    } else {
        tc.cache.store(body);
    }
    tc.cache.set_height(body_h, width);
    let total = title_h + body_h;
    tc.record_measured_height(width, total, layout_generation);
    let cached_len = if body_depends_on_width {
        tc.cache
            .get_for_width(width)
            .map_or(1, |body| body.len() + 1)
    } else {
        tc.cache.get().map_or(1, |body| body.len() + 1)
    };
    (total, cached_len)
}

// ---------------------------------------------------------------------------
// Shared helpers (used by multiple submodules)
// ---------------------------------------------------------------------------

fn markdown_inline_spans(input: &str) -> Vec<Span<'static>> {
    markdown::render_markdown_safe(input, None)
        .into_iter()
        .next()
        .map_or_else(Vec::new, |line| {
            line.spans
                .into_iter()
                .map(|s| Span::styled(s.content.into_owned(), s.style))
                .collect()
        })
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

fn truncate_spans_to_width(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    if max_width == 0 {
        return Vec::new();
    }
    if spans_width(&spans) <= max_width {
        return spans;
    }

    let keep_width = max_width.saturating_sub(1);
    let mut used = 0usize;
    let mut out: Vec<Span<'static>> = Vec::new();

    for span in spans {
        if used >= keep_width {
            break;
        }
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + w > keep_width {
                break;
            }
            chunk.push(ch);
            used += w;
        }
        if !chunk.is_empty() {
            out.push(Span::styled(chunk, span.style));
        }
    }
    out.push(Span::styled("\u{2026}", Style::default().fg(theme::DIM)));
    out
}

fn tool_output_badge_spans(tc: &ToolCallInfo) -> Vec<Span<'static>> {
    let mut badges = Vec::new();

    if tc.assistant_auto_backgrounded() {
        badges.push(Span::styled(
            "  [assistant backgrounded]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if tc.task_is_backgrounded() {
        badges.push(Span::styled(
            "  [backgrounded]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if tc.verification_nudge_needed() {
        badges.push(Span::styled(
            "  [verification needed]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    badges
}

fn tool_display_title<'a>(
    tc: &'a ToolCallInfo,
    render_context: ToolCallRenderContext<'_>,
) -> Cow<'a, str> {
    if render_context.current_mode_id == Some("plan") {
        match tc.sdk_tool_name.as_str() {
            "Write" => return Cow::Borrowed("Create Plan"),
            "Edit" | "MultiEdit" => return Cow::Borrowed("Update Plan"),
            _ => {}
        }
    }

    Cow::Borrowed(&tc.title)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
