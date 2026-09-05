//! Tool-call rendering: entry points, caching, and shared helpers.
//!
//! Submodules handle specific rendering concerns:
//! - [`standard`] -- one render path for every tool (Read, Write, Bash,
//!   Glob, etc.) - title row plus optional indented body.
//! - [`errors`] -- error rendering and tool-use error extraction

mod errors;
mod group;
mod standard;

pub use group::SummaryChrome;
pub(crate) use group::clip_to_width;
pub use group::render_group_summary_line;

use std::borrow::Cow;

use crate::agent::model;
use crate::app::ToolCallInfo;
use crate::ui::markdown;
use crate::ui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

// Re-export submodule items used by tests.
#[cfg(test)]
use errors::{
    extract_tool_use_error_message, looks_like_internal_error, render_tool_use_error_content,
    summarize_internal_error,
};

#[cfg(test)]
use standard::{cap_write_diff_lines, content_summary};

/// Stable-hash a string into a `usize` for the perf-log's `extra`
/// field. Used by the `tc::render_body` instrumentation (#125
/// variant 2) so a slow-frame capture can correlate tool names + ids
/// across logs without leaking raw strings into the diagnostic
/// stream. The hash is `DefaultHasher`-based - stable per process,
/// not across forge versions; sufficient for in-session triage.
fn stable_hash_usize(s: &str) -> usize {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    // Narrowing a 64-bit hash is what this function is for.
    #[allow(clippy::cast_possible_truncation)]
    let truncated = h.finish() as usize;
    truncated
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolCallRenderContext<'a> {
    pub current_mode_id: Option<&'a str>,
}

pub fn status_icon(status: model::ToolCallStatus, spinner_glyph: char) -> (String, Color) {
    match status {
        model::ToolCallStatus::Pending => ("\u{25CB}".to_owned(), theme::RUST_ORANGE),
        model::ToolCallStatus::InProgress => (spinner_glyph.to_string(), theme::RUST_ORANGE),
        model::ToolCallStatus::Completed => (theme::ICON_COMPLETED.to_owned(), theme::RUST_ORANGE),
        model::ToolCallStatus::Failed | model::ToolCallStatus::Killed => {
            (theme::ICON_FAILED.to_owned(), theme::STATUS_ERROR)
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry points (delegating to submodules)
// ---------------------------------------------------------------------------

/// Render a tool call with caching. Only re-renders when cache is stale.
///
/// All tool kinds - Read, Write, Edit, Bash, Grep, etc. - share the same
/// shape: a single title row at column 2 (status icon + kind icon +
/// display title), followed when there's body content by lines prefixed
/// with `  │  ` (DIM). Body content varies by kind (terminal output for
/// Bash, syntax-highlighted code for Read, unified diff for
/// Edit/Write/MultiEdit).
///
/// The title is rendered live; the expanded body is cached. Session
/// collapse preference can change without invalidating completed
/// tool-call body caches.
pub fn render_tool_call_cached_with_tools_collapsed(
    tc: &mut ToolCallInfo,
    render_context: ToolCallRenderContext<'_>,
    width: u16,
    spinner_glyph: char,
    tools_collapsed: bool,
    out: &mut Vec<Line<'static>>,
) {
    let title = standard::render_tool_call_title(tc, render_context, width, spinner_glyph);
    out.push(title);

    let has_execute_body = tc.is_execute_tool()
        && (tc.terminal_output.is_some() || matches!(tc.status, model::ToolCallStatus::InProgress));
    let has_body = !tc.content.is_empty() || has_execute_body;
    if !has_body {
        return;
    }

    if standard::tool_call_effectively_collapsed(tc, tools_collapsed) {
        standard::render_collapsed_tool_call_summary(tc, out);
        return;
    }

    let body_depends_on_width = standard::tool_call_body_depends_on_width(tc);

    // Expanded body: use cache if valid, otherwise render and cache.
    let cached_body =
        if body_depends_on_width { tc.cache.get_for_width(width) } else { tc.cache.get() };
    if let Some(cached_body) = cached_body {
        crate::perf::mark_with("tc::cache_hit_body", "lines", cached_body.len());
        out.extend_from_slice(cached_body);
    } else {
        crate::perf::mark("tc::cache_miss_body");
        // Hash sdk_tool_name + tool-call id (perf extras are usize-only)
        // so a slow-frame capture can attribute body-render time to a
        // tool without leaking raw command strings.
        let _t = crate::perf::start_with("tc::render_body", "input_bytes", tc.raw_input_bytes);
        crate::perf::mark_with(
            "tc::render_body_tool",
            "name_hash",
            stable_hash_usize(&tc.sdk_tool_name),
        );
        crate::perf::mark_with("tc::render_body_id", "tc_id_hash", stable_hash_usize(&tc.id));
        let body = standard::render_tool_call_body(tc, width);
        if body_depends_on_width {
            tc.cache.store_for_width(body, width);
        } else {
            tc.cache.store(body);
        }
        let stored =
            if body_depends_on_width { tc.cache.get_for_width(width) } else { tc.cache.get() };
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
    spinner_glyph: char,
    layout_generation: u64,
    tools_collapsed: bool,
) -> (usize, usize) {
    if tc.cache_measurement_key_matches(width, layout_generation, tools_collapsed) {
        crate::perf::mark("tc_measure_fast_path_hits");
        return (tc.last_measured_height, 0);
    }
    crate::perf::mark("tc_measure_recompute_count");

    let title = standard::render_tool_call_title(tc, render_context, width, spinner_glyph);
    let title_h =
        Paragraph::new(Text::from(vec![title])).wrap(Wrap { trim: false }).line_count(width);
    let has_execute_body = tc.is_execute_tool()
        && (tc.terminal_output.is_some() || matches!(tc.status, model::ToolCallStatus::InProgress));
    let has_body = !tc.content.is_empty() || has_execute_body;

    if !has_body {
        tc.record_measured_height(width, title_h, layout_generation, tools_collapsed);
        return (title_h, 1);
    }

    if standard::tool_call_effectively_collapsed(tc, tools_collapsed) {
        let mut summary = Vec::new();
        standard::render_collapsed_tool_call_summary(tc, &mut summary);
        let summary_h = Paragraph::new(Text::from(summary.clone()))
            .wrap(Wrap { trim: false })
            .line_count(width);
        let total = title_h + summary_h;
        tc.record_measured_height(width, total, layout_generation, tools_collapsed);
        return (total, 1 + summary.len());
    }

    let body_depends_on_width = standard::tool_call_body_depends_on_width(tc);
    let cached_body =
        if body_depends_on_width { tc.cache.get_for_width(width) } else { tc.cache.get() };
    if cached_body.is_some() {
        if let Some(body_h) = tc.cache.height_at(width) {
            let total = title_h + body_h;
            tc.record_measured_height(width, total, layout_generation, tools_collapsed);
            return (total, 1);
        }
        if let Some(body_h) = tc.cache.measure_and_set_height(width) {
            let total = title_h + body_h;
            tc.record_measured_height(width, total, layout_generation, tools_collapsed);
            let cached_len = if body_depends_on_width {
                tc.cache.get_for_width(width).map_or(1, |body| body.len() + 1)
            } else {
                tc.cache.get().map_or(1, |body| body.len() + 1)
            };
            return (total, cached_len);
        }
    }

    let body = standard::render_tool_call_body(tc, width);
    let body_h =
        Paragraph::new(Text::from(body.clone())).wrap(Wrap { trim: false }).line_count(width);
    if body_depends_on_width {
        tc.cache.store_for_width(body, width);
    } else {
        tc.cache.store(body);
    }
    tc.cache.set_height(body_h, width);
    let total = title_h + body_h;
    tc.record_measured_height(width, total, layout_generation, tools_collapsed);
    let cached_len = if body_depends_on_width {
        tc.cache.get_for_width(width).map_or(1, |body| body.len() + 1)
    } else {
        tc.cache.get().map_or(1, |body| body.len() + 1)
    };
    (total, cached_len)
}

// ---------------------------------------------------------------------------
// Shared helpers (used by multiple submodules)
// ---------------------------------------------------------------------------

fn markdown_inline_spans(input: &str) -> Vec<Span<'static>> {
    markdown::render_markdown_safe(input, None).into_iter().next().map_or_else(Vec::new, |line| {
        line.spans.into_iter().map(|s| Span::styled(s.content.into_owned(), s.style)).collect()
    })
}

fn tool_output_badge_spans(tc: &ToolCallInfo) -> Vec<Span<'static>> {
    let mut badges = Vec::new();

    if tc.assistant_auto_backgrounded() {
        badges.push(Span::styled(
            "  [assistant backgrounded]",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }

    if tc.task_is_backgrounded() {
        badges.push(Span::styled(
            "  [backgrounded]",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::BlockCache;
    use crate::ui::wrap::display_width;
    use pretty_assertions::assert_eq;
    use std::fmt::Write as _;

    fn test_tool_call(
        id: &str,
        sdk_tool_name: &str,
        status: model::ToolCallStatus,
    ) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: id.to_owned(),
            sdk_tool_name: sdk_tool_name.to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_output: None,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    // status_icon

    #[test]
    fn status_icon_pending() {
        let (icon, color) = status_icon(model::ToolCallStatus::Pending, '\u{280B}');
        assert!(!icon.is_empty());
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn status_icon_in_progress_uses_active_glyph() {
        let (icon, color) = status_icon(model::ToolCallStatus::InProgress, '\u{280B}');
        assert_eq!(icon, "\u{280B}");
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn status_icon_completed() {
        let (icon, color) = status_icon(model::ToolCallStatus::Completed, '\u{280B}');
        assert_eq!(icon, theme::ICON_COMPLETED);
        assert_eq!(color, theme::RUST_ORANGE);
    }

    #[test]
    fn status_icon_failed() {
        let (icon, color) = status_icon(model::ToolCallStatus::Failed, '\u{280B}');
        assert_eq!(icon, theme::ICON_FAILED);
        assert_eq!(color, theme::STATUS_ERROR);
    }

    #[test]
    fn status_icon_killed() {
        let (icon, color) = status_icon(model::ToolCallStatus::Killed, '\u{280B}');
        assert_eq!(icon, theme::ICON_FAILED);
        assert_eq!(color, theme::STATUS_ERROR);
    }

    #[test]
    fn markdown_inline_spans_removes_markdown_syntax() {
        let spans = markdown_inline_spans("**Allow** _once_");
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("Allow"));
        assert!(rendered.contains("once"));
        assert!(!rendered.contains('*'));
        assert!(!rendered.contains('_'));
    }

    #[test]
    fn render_tool_call_title_shows_backgrounded_badge() {
        let mut tc = test_tool_call("tc-bg", "Agent", model::ToolCallStatus::InProgress);
        tc.task_metadata = Some(model::TaskMetadata::new().backgrounded(Some(true)));

        let line =
            standard::render_tool_call_title(&tc, ToolCallRenderContext::default(), 80, '\u{280B}');
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(rendered.contains("[backgrounded]"));
    }

    #[test]
    fn standard_title_renders_kind_label_for_bash() {
        // Bash title from claude is just the command (no "Bash " prefix).
        // The renderer should emit the "Bash" label so the column is
        // consistent with Read / Edit / Grep where claude already prefixes.
        let tc = test_tool_call("ls -la", "Bash", model::ToolCallStatus::Completed);

        let line =
            standard::render_tool_call_title(&tc, ToolCallRenderContext::default(), 80, '\u{280B}');
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        assert!(rendered.contains("Bash "), "expected 'Bash ' label in title; got: {rendered:?}");
        assert!(rendered.contains("ls -la"), "expected command text in title; got: {rendered:?}");
        // Make sure we don't double-render: title should have "Bash"
        // exactly once (no "Bash Bash …").
        assert_eq!(
            rendered.matches("Bash").count(),
            1,
            "expected exactly one 'Bash' occurrence; got: {rendered:?}"
        );
    }

    #[test]
    fn standard_title_strips_duplicate_kind_prefix_from_claude_title() {
        // Claude often sends Read titles as "Read /path/to/file.rs".
        // The renderer adds its own "Read" label; the duplicate prefix
        // on the title should be stripped so we render
        // "Read /path/to/file.rs" exactly once.
        let tc = test_tool_call(
            "Read /Users/developer/Projects/forge/Cargo.toml",
            "Read",
            model::ToolCallStatus::Completed,
        );

        let line = standard::render_tool_call_title(
            &tc,
            ToolCallRenderContext::default(),
            120,
            '\u{280B}',
        );
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();

        // "Read" appears once (from our label), the path follows,
        // not "Read Read /Users/..."
        assert_eq!(
            rendered.matches("Read").count(),
            1,
            "duplicate 'Read' prefix not stripped; got: {rendered:?}"
        );
        assert!(
            rendered.contains("/Users/developer/Projects/forge/Cargo.toml"),
            "expected path body in title; got: {rendered:?}"
        );
    }

    #[test]
    fn tool_display_title_uses_plan_aliases() {
        let write = test_tool_call("tc-plan-write", "Write", model::ToolCallStatus::Completed);
        let edit = test_tool_call("tc-plan-edit", "Edit", model::ToolCallStatus::Completed);
        let read = test_tool_call("tc-plan-read", "Read", model::ToolCallStatus::Completed);
        let plan = ToolCallRenderContext { current_mode_id: Some("plan") };

        assert_eq!(tool_display_title(&write, plan), "Create Plan");
        assert_eq!(tool_display_title(&edit, plan), "Update Plan");
        assert_eq!(tool_display_title(&read, plan), "tc-plan-read");
    }

    #[test]
    fn standard_title_uses_plan_alias_for_write() {
        let tc = test_tool_call("Write notes/plan.md", "Write", model::ToolCallStatus::Completed);

        let rendered = standard::render_tool_call_title(
            &tc,
            ToolCallRenderContext { current_mode_id: Some("plan") },
            80,
            '\u{280B}',
        );
        let text: String = rendered.spans.iter().map(|span| span.content.as_ref()).collect();

        assert!(text.contains("Create Plan"));
        assert!(!text.contains("Write notes/plan.md"));
    }

    #[test]
    fn bash_title_renders_assistant_backgrounded_badge() {
        let mut tc = test_tool_call("tc-bash-bg", "Bash", model::ToolCallStatus::Completed);
        tc.output_metadata =
            Some(model::ToolOutputMetadata::new().bash(Some(
                model::BashOutputMetadata::new().assistant_auto_backgrounded(Some(true)),
            )));

        let title = standard::render_tool_call_title(
            &tc,
            ToolCallRenderContext::default(),
            100,
            '\u{280B}',
        );
        let text: String = title.spans.iter().map(|span| span.content.as_ref()).collect();
        assert!(text.contains("[assistant backgrounded]"));
    }

    #[test]
    fn bash_title_preserves_command_in_plan_mode() {
        let tc = test_tool_call("echo hi", "Bash", model::ToolCallStatus::Completed);

        let title = standard::render_tool_call_title(
            &tc,
            ToolCallRenderContext { current_mode_id: Some("plan") },
            80,
            '\u{280B}',
        );
        let text: String = title.spans.iter().map(|span| span.content.as_ref()).collect();

        assert!(text.contains("echo hi"));
    }

    #[test]
    fn bash_title_renders_geometric_kind_icon_not_chevron() {
        // Issue #39: chevron ⟩ replaced with triangle ▶ in the Bash row title.
        let tc = test_tool_call("ls -la", "Bash", model::ToolCallStatus::Completed);

        let title =
            standard::render_tool_call_title(&tc, ToolCallRenderContext::default(), 80, '\u{280B}');
        let text: String = title.spans.iter().map(|span| span.content.as_ref()).collect();

        assert!(text.contains('\u{25B6}'), "expected ▶ (U+25B6) in Bash row title; got: {text:?}");
        assert!(
            !text.contains('\u{27E9}'),
            "did not expect ⟩ (U+27E9) chevron in Bash row title; got: {text:?}"
        );
    }

    #[test]
    fn bash_renders_through_standard_path_no_box_borders() {
        // Issue #39: Bash flows through the standard tool-call path -
        // no bordered card. Output should be a title row + body lines
        // prefixed with `  │  ` / `  └─ ` (DIM), like every other tool.
        let mut tc = test_tool_call("echo hi", "Bash", model::ToolCallStatus::Completed);
        tc.terminal_output = Some("hello\nworld".to_owned());

        let mut out = Vec::new();
        render_tool_call_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            false,
            &mut out,
        );

        let body: String =
            out.iter().flat_map(|line| line.spans.iter().map(|s| s.content.as_ref())).collect();
        assert!(
            !body.contains('\u{256D}'),
            "Bash row must not contain `╭` (top border) - got: {body:?}",
        );
        assert!(
            !body.contains('\u{256E}'),
            "Bash row must not contain `╮` (top border) - got: {body:?}",
        );
        assert!(
            !body.contains('\u{2570}'),
            "Bash row must not contain `╰` (bottom border) - got: {body:?}",
        );
        assert!(
            !body.contains('\u{256F}'),
            "Bash row must not contain `╯` (bottom border) - got: {body:?}",
        );
        // Body lines should have the standard `  │  ` / `  └─ ` prefix.
        assert!(
            body.contains("  \u{2514}\u{2500} ") || body.contains("  \u{2502}  "),
            "expected standard body prefix `  │  ` or `  └─ ` in Bash row body; got: {body:?}",
        );
    }

    #[test]
    fn execute_measure_fast_path_keeps_height_stable_across_repeated_measurement() {
        let mut tc = test_tool_call("tc-fast", "Bash", model::ToolCallStatus::InProgress);
        tc.terminal_output = Some("hello\nworld".to_owned());

        let (h1, lines1) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            1,
            false,
        );
        assert!(h1 > 0);
        assert!(lines1 > 0);

        let (h2, lines2) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{2839}',
            1,
            false,
        );
        assert_eq!(h2, h1);
        assert!(lines2 <= lines1);
    }

    #[test]
    fn execute_measure_recomputes_on_layout_generation_change() {
        let mut tc = test_tool_call("tc-layout-gen", "Bash", model::ToolCallStatus::InProgress);
        tc.terminal_output = Some("hello".to_owned());

        let (_, first_lines) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            1,
            false,
        );
        assert!(first_lines > 0);
        let (_, second_lines) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            2,
            false,
        );
        assert!(second_lines > 0);
    }

    #[test]
    fn layout_dirty_invalidates_measure_fast_path() {
        let mut tc = test_tool_call("tc-dirty", "Read", model::ToolCallStatus::Completed);
        tc.content = vec![model::RenderToolCallContent::from("one line")];

        let (first_height, first_lines) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            1,
            false,
        );
        assert!(first_lines > 0);
        let (cached_height, fast_lines) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            1,
            false,
        );
        assert_eq!(cached_height, first_height);
        assert!(fast_lines <= first_lines);

        tc.mark_tool_call_layout_dirty();
        let (recomputed_height, recompute_lines) =
            measure_tool_call_height_cached_with_tools_collapsed(
                &mut tc,
                ToolCallRenderContext::default(),
                80,
                '\u{280B}',
                1,
                false,
            );
        assert_eq!(recomputed_height, first_height);
        assert!(recompute_lines > 0);
    }

    #[test]
    fn mcp_resource_body_renders_saved_path_hint_when_text_omits_it() {
        let mut tc =
            test_tool_call("tc-mcp-resource", "ReadMcpResource", model::ToolCallStatus::Completed);
        tc.content = vec![model::RenderToolCallContent::McpResource(
            model::McpResource::new("file://manual.pdf")
                .mime_type(Some("application/pdf".to_owned()))
                .text(Some("Binary resource downloaded successfully.".to_owned()))
                .blob_saved_to(Some("C:\\tmp\\manual.pdf".to_owned())),
        )];

        let body = standard::render_tool_call_body(&tc, 80);
        let rendered: Vec<String> = body
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(
            rendered.iter().any(|line| line.contains("Binary resource downloaded successfully."))
        );
        assert!(rendered.iter().any(|line| line.contains("Saved to: C:\\tmp\\manual.pdf")));
    }

    #[test]
    fn mcp_resource_body_avoids_duplicate_saved_path_hint_when_text_already_mentions_it() {
        let mut tc = test_tool_call(
            "tc-mcp-resource-dupe",
            "ReadMcpResource",
            model::ToolCallStatus::Completed,
        );
        tc.content = vec![model::RenderToolCallContent::McpResource(
            model::McpResource::new("file://manual.pdf")
                .mime_type(Some("application/pdf".to_owned()))
                .text(Some(
                    "[Resource from docs at file://manual.pdf] Saved to C:\\tmp\\manual.pdf"
                        .to_owned(),
                ))
                .blob_saved_to(Some("C:\\tmp\\manual.pdf".to_owned())),
        )];

        let body = standard::render_tool_call_body(&tc, 80);
        let rendered: Vec<String> = body
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert_eq!(
            rendered.iter().filter(|line| line.contains("Saved to: C:\\tmp\\manual.pdf")).count(),
            0
        );
    }

    #[test]
    fn completed_non_execute_collapse_changes_visible_body_without_hiding_the_title() {
        let mut tc = test_tool_call("tc-collapse", "Read", model::ToolCallStatus::Completed);
        tc.content = vec![model::RenderToolCallContent::from("alpha\nbeta".to_owned())];

        let mut expanded = Vec::new();
        render_tool_call_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            false,
            &mut expanded,
        );
        let expanded_text: Vec<String> = expanded
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert!(expanded_text.iter().any(|line| line.contains("alpha")));
        assert!(expanded_text.first().is_some_and(|line| line.contains("tc-collapse")));

        let mut collapsed = Vec::new();
        render_tool_call_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            true,
            &mut collapsed,
        );
        let collapsed_text: Vec<String> = collapsed
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();
        assert_eq!(collapsed_text.first(), expanded_text.first());
        assert!(collapsed_text.iter().any(|line| line.contains("ctrl+x to expand")));
        assert!(!collapsed_text.iter().any(|line| line.contains("beta")));
        assert!(collapsed_text.len() < expanded_text.len());
    }

    #[test]
    fn completed_non_execute_measurement_changes_with_session_collapse_preference() {
        let mut tc =
            test_tool_call("tc-measure-collapse", "Read", model::ToolCallStatus::Completed);
        tc.content =
            vec![model::RenderToolCallContent::from("alpha\nbeta\ngamma\ndelta".to_owned())];

        let (expanded_h, _) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            24,
            '\u{280B}',
            1,
            false,
        );
        let (collapsed_h, _) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            24,
            '\u{280B}',
            2,
            true,
        );

        assert!(collapsed_h < expanded_h);
    }

    /// `layout_generation` is held fixed so the collapse flag is the
    /// only thing that moves. The test above varies the generation as
    /// well, which misses the measurement cache for a reason that is
    /// not the flag, and so cannot see the flag missing from the key.
    #[test]
    fn flipping_tools_collapsed_alone_remeasures() {
        let body = "alpha\nbeta\ngamma\ndelta".to_owned();

        // What an expanded measurement costs from cold.
        let mut cold = test_tool_call("tc-key-cold", "Read", model::ToolCallStatus::Completed);
        cold.content = vec![model::RenderToolCallContent::from(body.clone())];
        let (expanded_from_cold, _) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut cold,
            ToolCallRenderContext::default(),
            24,
            '\u{280B}',
            7,
            false,
        );

        // The same tool measured collapsed first, then expanded with
        // nothing else changed.
        let mut tc = test_tool_call("tc-key-reused", "Read", model::ToolCallStatus::Completed);
        tc.content = vec![model::RenderToolCallContent::from(body)];
        let (collapsed, _) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            24,
            '\u{280B}',
            7,
            true,
        );
        let (expanded_after_flip, _) = measure_tool_call_height_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            24,
            '\u{280B}',
            7,
            false,
        );

        assert_ne!(
            collapsed, expanded_from_cold,
            "the body has to change the height with the flag, or the assertion below is free",
        );
        assert_eq!(
            expanded_after_flip, expanded_from_cold,
            "the flag belongs to the measurement key, so flipping it re-measures",
        );
    }

    #[test]
    fn diff_tool_stays_expanded_when_session_prefers_collapsed() {
        let mut tc = test_tool_call("tc-diff", "Write", model::ToolCallStatus::Completed);
        tc.content = vec![model::RenderToolCallContent::Diff(
            model::Diff::new("src/main.rs", "new".to_owned()).old_text(Some("old".to_owned())),
        )];

        let mut rendered = Vec::new();
        render_tool_call_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            80,
            '\u{280B}',
            true,
            &mut rendered,
        );
        let text: Vec<String> = rendered
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(!text.iter().any(|line| line.contains("expand")));
        assert!(text.iter().any(|line| line.contains("lines ")));
        assert!(text.iter().any(|line| line.contains("+  new")));
        assert!(text.len() > 2);
    }

    #[test]
    fn diff_tool_body_adds_nested_indent_inside_tool_prefix() {
        let mut tc = test_tool_call("tc-diff-indent", "Edit", model::ToolCallStatus::Completed);
        tc.content = vec![model::RenderToolCallContent::Diff(
            model::Diff::new("src/main.rs", "new".to_owned())
                .old_text(Some("old".to_owned()))
                .repository(Some("stargate/project".to_owned())),
        )];

        let body = standard::render_tool_call_body(&tc, 80);
        let rendered: Vec<String> = body
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(rendered.iter().any(|line| line.starts_with("  │    [stargate/project]")));
        assert!(rendered.iter().any(|line| line.starts_with("  │    lines ")));
        assert!(rendered.iter().any(|line| {
            (line.starts_with("  │   ") || line.starts_with("  └─   ")) && line.contains("+  new")
        }));
    }

    #[test]
    fn diff_tool_body_preserves_source_code_indentation() {
        let mut tc =
            test_tool_call("tc-diff-code-indent", "Edit", model::ToolCallStatus::Completed);
        tc.content = vec![model::RenderToolCallContent::Diff(model::Diff::new(
            "src/main.rs",
            "fn main() {\n    if true {\n        return;\n    }\n}\n".to_owned(),
        ))];

        let body = standard::render_tool_call_body(&tc, 80);
        let rendered: Vec<String> = body
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(rendered.iter().any(|line| line.contains("+      if true {")));
        assert!(rendered.iter().any(|line| line.contains("+          return;")));
    }

    #[test]
    fn diff_tool_body_preserves_nested_indent_for_wrapped_continuations() {
        let mut tc = test_tool_call("tc-diff-wrap", "Edit", model::ToolCallStatus::Completed);
        tc.content = vec![model::RenderToolCallContent::Diff(model::Diff::new(
            "src/main.rs",
            "        This is a long added line that should wrap onto another visual line.\n"
                .to_owned(),
        ))];

        let body = standard::render_tool_call_body(&tc, 28);
        let rendered: Vec<String> = body
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(rendered.iter().any(|line| line.contains("+          This")));
        assert!(
            rendered.iter().any(|line| line.starts_with("  │                  "))
                || rendered.iter().any(|line| line.starts_with("  └─                  "))
        );
        assert!(rendered.iter().any(|line| line.contains("another")));
        assert!(rendered.iter().any(|line| line.contains("line.")));
    }

    #[test]
    fn write_diff_cap_keeps_omission_marker_nested_indented() {
        let new_text = (0..120).fold(String::new(), |mut text, idx| {
            let _ = writeln!(&mut text, "line {idx}");
            text
        });
        let mut tc = test_tool_call("tc-diff-cap", "Write", model::ToolCallStatus::Completed);
        tc.content =
            vec![model::RenderToolCallContent::Diff(model::Diff::new("src/main.rs", new_text))];

        let body = standard::render_tool_call_body(&tc, 80);
        let rendered: Vec<String> = body
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(
            rendered
                .iter()
                .any(|line| line.starts_with("  │    ... ") && line.contains("diff lines omitted"))
                || rendered
                    .iter()
                    .any(|line| line.starts_with("  └─    ... ")
                        && line.contains("diff lines omitted"))
        );
    }

    #[test]
    fn plan_files_render_diff_like_code() {
        let mut tc = test_tool_call(
            "Write .claude/plans/launch.md",
            "Write",
            model::ToolCallStatus::Completed,
        );
        tc.content = vec![model::RenderToolCallContent::Diff(
            model::Diff::new(
                ".claude/plans/launch.md",
                "# Launch Plan\n\n- Ship aliases\n- Render plan markdown\n".to_owned(),
            )
            .old_text(Some("# Old Plan\n".to_owned())),
        )];

        let body = standard::render_tool_call_body(&tc, 80);
        let rendered: Vec<String> = body
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect();

        assert!(
            rendered.iter().any(|line| line.contains("lines ")),
            "markdown edit must render the compact hunk header (`lines -N +M`); got {rendered:#?}",
        );
        assert!(
            rendered.iter().any(|line| line.contains('+')),
            "markdown edit must render added lines marked with +; got {rendered:#?}",
        );
        assert!(
            rendered.iter().any(|line| line.contains('-')),
            "markdown edit must render removed lines marked with -; got {rendered:#?}",
        );
    }

    #[test]
    fn internal_error_detection_accepts_xml_payload() {
        let payload =
            "<error><code>-32603</code><message>Adapter process crashed</message></error>";
        assert!(looks_like_internal_error(payload));
    }

    #[test]
    fn internal_error_detection_rejects_plain_bash_failure() {
        let payload = "bash: unknown_command: command not found";
        assert!(!looks_like_internal_error(payload));
    }

    #[test]
    fn summarize_internal_error_prefers_xml_message() {
        let payload =
            "<error><code>-32603</code><message>Adapter process crashed</message></error>";
        assert_eq!(summarize_internal_error(payload), "Adapter process crashed");
    }

    #[test]
    fn summarize_internal_error_reads_json_rpc_message() {
        let payload = r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"internal rpc fault"}}"#;
        assert_eq!(summarize_internal_error(payload), "internal rpc fault");
    }

    #[test]
    fn extract_tool_use_error_message_reads_inner_text() {
        let payload = "<tool_use_error>Sibling tool call errored</tool_use_error>";
        assert_eq!(
            extract_tool_use_error_message(payload).as_deref(),
            Some("Sibling tool call errored")
        );
    }

    #[test]
    fn render_tool_use_error_content_shows_only_inner_text_lines() {
        let lines = render_tool_use_error_content("Line A\nLine B");
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(rendered.len(), 2);
        assert!(rendered.iter().any(|line| line == "Line A"));
        assert!(rendered.iter().any(|line| line == "Line B"));
    }

    /// The ToolCallInfo literal behind the captured-terminal tests,
    /// differing only in the four fields that matter.
    fn terminal_tool_call(
        id: &str,
        status: model::ToolCallStatus,
        terminal_id: &str,
        terminal_output: &str,
    ) -> ToolCallInfo {
        let mut tc = test_tool_call(id, "Bash", status);
        tc.terminal_id = Some(terminal_id.to_owned());
        tc.terminal_output = Some(terminal_output.to_owned());
        tc
    }

    #[test]
    fn content_summary_only_extracts_tool_use_error_for_failed_execute() {
        let tc = terminal_tool_call(
            "tc-1",
            model::ToolCallStatus::Completed,
            "term-1",
            "<tool_use_error>bad</tool_use_error>\ndone",
        );
        assert_eq!(content_summary(&tc), "done");
    }

    #[test]
    fn content_summary_extracts_tool_use_error_for_failed_execute() {
        let tc = terminal_tool_call(
            "tc-1",
            model::ToolCallStatus::Failed,
            "term-1",
            "<tool_use_error>bad</tool_use_error>\ndone",
        );
        assert_eq!(content_summary(&tc), "bad");
    }

    #[test]
    fn content_summary_uses_first_terminal_line_for_failed_execute() {
        let tc = terminal_tool_call(
            "tc-2",
            model::ToolCallStatus::Failed,
            "term-2",
            "Exit code 1\n/usr/bin/bash: line 1: cd: too many arguments\nmore detail",
        );
        assert_eq!(content_summary(&tc), "Exit code 1");
    }

    /// The non-failed execute summary drops the last terminal line into
    /// the collapsed span raw; a control char must picture like the
    /// failed path does.
    #[test]
    fn content_summary_pictures_control_chars_in_last_terminal_line() {
        let tc = terminal_tool_call("tc-4", model::ToolCallStatus::Completed, "term-4", "a\rb");
        assert_eq!(content_summary(&tc), "a\u{240d}b");
    }

    #[test]
    fn content_summary_keeps_normal_limit_for_completed_agent() {
        let mut tc = test_tool_call("tc-agent-done", "Agent", model::ToolCallStatus::Completed);
        let long_line = "a".repeat(150);
        tc.content = vec![model::RenderToolCallContent::from(long_line)];

        let summary = content_summary(&tc);
        assert_eq!(summary.chars().count(), 60);
        assert!(summary.ends_with("..."));
    }

    #[test]
    fn failed_bash_body_surfaces_summary_only_not_full_output() {
        // For Failed/Killed Bash, the body shows the first non-empty
        // stderr-ish line via `failed_execute_first_line` instead of
        // dumping the whole captured output.
        let mut tc = terminal_tool_call(
            "tc-3",
            model::ToolCallStatus::Failed,
            "term-3",
            "Exit code 1\n/usr/bin/bash: line 1: cd: too many arguments\nmore detail",
        );

        let mut out = Vec::new();
        render_tool_call_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            120,
            '\u{280B}',
            false,
            &mut out,
        );

        let rendered: Vec<String> = out
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(rendered.iter().any(|line| line.contains("Exit code 1")));
        assert!(!rendered.iter().any(|line| line.contains("more detail")));
    }

    /// A failed run's first output line is often a progress meter: a
    /// raw control character is charged a column by `Span::width` and
    /// painted by nothing, so the line is pictured like the other
    /// metadata spans.
    #[test]
    fn failed_execute_error_line_pictures_control_chars_so_measured_width_equals_painted() {
        let mut tc = terminal_tool_call(
            "tc-4",
            model::ToolCallStatus::Failed,
            "term-4",
            "50%\r75%\r100%\nbash: line 1: cd: too many arguments",
        );

        let mut out = Vec::new();
        render_tool_call_cached_with_tools_collapsed(
            &mut tc,
            ToolCallRenderContext::default(),
            120,
            '\u{280B}',
            false,
            &mut out,
        );

        let rows: Vec<&Line<'static>> = out
            .iter()
            .filter(|line| line.spans.iter().any(|span| span.content.contains("50%")))
            .collect();
        assert!(!rows.is_empty(), "the failed run's first output line renders");
        for row in rows {
            let painted: usize = row
                .styled_graphemes(Style::default())
                .map(|grapheme| display_width(grapheme.symbol))
                .sum();
            let measured: usize = row.spans.iter().map(Span::width).sum();
            assert_eq!(measured, painted, "error line charges a column it does not paint: {row:?}");
            let joined: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
            assert!(
                joined.contains('\u{240d}'),
                "CR must paint its Control Pictures glyph, not ride raw: {joined:?}"
            );
        }
    }

    #[test]
    fn write_diff_cap_keeps_head_and_tail_with_omission_marker() {
        use standard::WRITE_DIFF_HEAD_LINES;
        use standard::WRITE_DIFF_MAX_LINES;

        let lines: Vec<Line<'static>> =
            (0..120).map(|idx| Line::from(format!("line {idx}"))).collect();
        let capped = cap_write_diff_lines(lines);
        let rendered: Vec<String> = capped
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();

        assert_eq!(rendered.len(), WRITE_DIFF_MAX_LINES);
        assert_eq!(rendered[0], "line 0");
        assert_eq!(rendered[WRITE_DIFF_HEAD_LINES - 1], "line 9");
        assert!(rendered.iter().any(|line| line.contains("diff lines omitted")));
        assert!(rendered.iter().any(|line| line == "line 83"));
        assert_eq!(rendered.last().map(String::as_str), Some("line 119"));
    }
}
