#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    clippy::disallowed_methods,
    clippy::while_let_loop,
    clippy::collapsible_if,
    reason = "lifted upstream from claude-code-rust"
)]

use crate::state::block_cache::BlockCache;
use crate::state::messages::{
    CachedMessageSegment, ChatMessage, IncrementalMarkdown, MarkdownRenderKey, MessageBlock,
    MessageBlockRenderSignature, MessageRenderCache, MessageRenderCacheKey, MessageRenderSignature,
    MessageRole, SystemSeverity, TextBlock, WelcomeBlock, hash_text_block_content,
    hash_welcome_block_content,
};
use crate::ui::lifted::tool_call;
use crate::ui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

const FERRIS_SAYS: &[&str] = &[
    r" --------------------------------- ",
    r"< Welcome back to Claude, in Rust! >",
    r" --------------------------------- ",
    r"        \             ",
    r"         \            ",
    r"            _~^~^~_  ",
    r"        \) /  o o  \ (/",
    r"          '_   -   _' ",
    r"          / '-----' \ ",
];

// Prepared for future randomized welcome-tip selection. Intentionally unused
// until the welcome UI is switched from a single hard-coded tip.
const WELCOME_TIPS: &[&str] = &[
    "Use /mode plan before larger changes, then switch back to code once the plan is clear",
    "Use /mcp to connect live tools and docs instead of pasting stale context into chat",
    "Keep repo instructions short in CLAUDE.md and update them when mistakes repeat",
    "Start prompts with the goal, relevant context, and constraints so Claude needs fewer corrections",
    "Ask Claude for a plan first on multi-step work instead of jumping straight to edits",
    "Give success criteria Claude can verify: tests, lint, screenshots, or exact outputs",
    "For visual work, paste screenshots or mockups so Claude can verify UI changes instead of guessing",
    "Start a fresh thread with /new-session when the task changes and old context is noise",
    "Use /compact when a session gets long and you want to keep the thread but trim context",
    "Use /resume <session_id> to jump back into earlier work without rebuilding context",
    "Use /docs shortcuts to see the live keyboard shortcuts for the current app state",
    "Use /docs commands to inspect the slash commands this app and the SDK expose",
    "If Claude drifts, refine or restate the plan early instead of piling on corrective prompts",
    "For tricky bugs, provide clear repro steps and runtime evidence instead of guessing fixes",
    "Point Claude at the relevant files, errors, and constraints instead of pasting everything",
    "If you do not know the exact file, let Claude search first and only pin the files that matter",
    "Ask codebase questions first in unfamiliar areas instead of coding blind",
    "Review diffs carefully even when the output looks plausible on first read",
    "Use hooks for checks that must run every time instead of relying on reminder text alone",
    "Turn repeated workflows into CLAUDE.md guidance only after they work reliably by hand",
    "For larger features, let Claude clarify requirements and edge cases through structured questions",
    "Use separate sessions for unrelated work so planning, debugging, and review stay clean",
];

/// Snapshot of the app state needed by the spinner -- extracted before
/// the message loop so we don't need `&App` (which conflicts with `&mut msg`).
#[derive(Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub struct SpinnerState {
    pub frame: usize,
    /// True when this message owns the currently active assistant turn.
    pub is_active_turn_assistant: bool,
    /// True when this message should show the initial empty-turn thinking indicator.
    pub show_empty_thinking: bool,
    /// True when this message should show the thinking indicator.
    pub show_thinking: bool,
    /// True when this message should show the compaction indicator.
    pub show_compacting: bool,
}

struct MessageLayout {
    segments: Vec<MessageLayoutSegment>,
    height: usize,
    wrapped_lines: usize,
}

impl MessageLayout {
    fn new() -> Self {
        Self {
            segments: Vec::new(),
            height: 0,
            wrapped_lines: 0,
        }
    }

    fn push_blank(&mut self) {
        self.segments.push(MessageLayoutSegment::Blank);
        self.height += 1;
    }

    fn push_wrapped_line(&mut self, line: Line<'static>, width: u16) {
        self.push_wrapped_lines(vec![line], width);
    }

    fn push_wrapped_lines(&mut self, lines: Vec<Line<'static>>, width: u16) {
        let height = rendered_lines_height(&lines, width);
        self.push_lines(lines, height, height);
    }

    fn push_lines(&mut self, lines: Vec<Line<'static>>, height: usize, wrapped_lines: usize) {
        if height == 0 {
            return;
        }
        self.segments
            .push(MessageLayoutSegment::Lines { lines, height });
        self.height += height;
        self.wrapped_lines += wrapped_lines;
    }
}

#[derive(Clone)]
enum MessageLayoutSegment {
    Blank,
    Lines {
        lines: Vec<Line<'static>>,
        height: usize,
    },
}

impl MessageLayoutSegment {
    fn into_cached(self) -> CachedMessageSegment {
        match self {
            Self::Blank => CachedMessageSegment::Blank,
            Self::Lines { lines, height } => CachedMessageSegment::Lines { lines, height },
        }
    }
}

struct RenderedBlockLayout {
    lines: Vec<Line<'static>>,
    height: usize,
    wrapped_lines: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct MessageRenderContext<'a> {
    tool_render_context: tool_call::ToolCallRenderContext<'a>,
    width: u16,
    layout_generation: u64,
    options: MessageRenderOptions,
}

impl<'a> MessageRenderContext<'a> {
    pub(crate) fn new(
        current_mode_id: Option<&'a str>,
        width: u16,
        layout_generation: u64,
        options: MessageRenderOptions,
    ) -> Self {
        Self {
            tool_render_context: tool_call::ToolCallRenderContext { current_mode_id },
            width,
            layout_generation,
            options,
        }
    }
}

fn assistant_role_label_line() -> Line<'static> {
    let spans = vec![Span::styled(
        "Claude",
        Style::default()
            .fg(theme::ROLE_ASSISTANT)
            .add_modifier(Modifier::BOLD),
    )];

    Line::from(spans)
}

#[cfg(test)]
pub(crate) fn render_message_with_tools_collapsed(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    tools_collapsed: bool,
    out: &mut Vec<Line<'static>>,
) {
    let render_context = MessageRenderContext::new(
        None,
        width,
        0,
        MessageRenderOptions {
            tools_collapsed,
            include_trailing_separator: true,
        },
    );
    render_message_internal(msg, spinner, render_context, out);
}

#[cfg(test)]
pub(crate) fn render_message_with_tools_collapsed_and_separator(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    tools_collapsed: bool,
    include_trailing_separator: bool,
    out: &mut Vec<Line<'static>>,
) {
    let render_context = MessageRenderContext::new(
        None,
        width,
        0,
        MessageRenderOptions {
            tools_collapsed,
            include_trailing_separator,
        },
    );
    render_message_internal(msg, spinner, render_context, out);
}

#[cfg(test)]
pub(crate) fn render_message_with_tools_collapsed_and_separator_and_layout_generation(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    layout_generation: u64,
    tools_collapsed: bool,
    include_trailing_separator: bool,
    out: &mut Vec<Line<'static>>,
) {
    let render_context = MessageRenderContext::new(
        None,
        width,
        layout_generation,
        MessageRenderOptions {
            tools_collapsed,
            include_trailing_separator,
        },
    );
    render_message_with_tools_collapsed_and_separator_and_layout_generation_with_mode(
        msg,
        spinner,
        render_context,
        out,
    );
}

pub(crate) fn render_message_with_tools_collapsed_and_separator_and_layout_generation_with_mode(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
    out: &mut Vec<Line<'static>>,
) {
    render_message_internal(msg, spinner, render_context, out);
}

fn render_message_internal(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
    out: &mut Vec<Line<'static>>,
) {
    let cache = get_or_build_message_render_cache(msg, spinner, render_context);
    render_cached_message(cache.segments(), out);
}

fn build_message_layout(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
) -> MessageLayout {
    let mut layout = MessageLayout::new();
    layout.push_wrapped_line(role_label_line(&msg.role), render_context.width);

    match msg.role {
        MessageRole::Welcome => append_welcome_blocks(msg, render_context.width, &mut layout),
        MessageRole::User => append_user_blocks(msg, render_context.width, &mut layout),
        MessageRole::Assistant => {
            append_assistant_blocks(msg, spinner, render_context, &mut layout);
        }
        MessageRole::System(_) => append_system_blocks(msg, render_context.width, &mut layout),
    }

    if render_context.options.include_trailing_separator {
        layout.push_blank();
    }

    layout
}

fn append_welcome_blocks(msg: &mut ChatMessage, width: u16, layout: &mut MessageLayout) {
    for block in &mut msg.blocks {
        if let MessageBlock::Welcome(welcome) = block {
            let rendered = welcome_block_layout(welcome, width);
            layout.push_lines(rendered.lines, rendered.height, rendered.wrapped_lines);
        }
    }
}

fn append_user_blocks(msg: &mut ChatMessage, width: u16, layout: &mut MessageLayout) {
    for block in &mut msg.blocks {
        match block {
            MessageBlock::Text(block) => {
                let trailing_gap = block.trailing_blank_lines();
                let rendered = text_block_layout(block, width, Some(theme::USER_MSG_BG), true);
                layout.push_lines(rendered.lines, rendered.height, rendered.wrapped_lines);
                for _ in 0..trailing_gap {
                    layout.push_blank();
                }
            }
            MessageBlock::ImageAttachment(img) => {
                let count = img.count;
                let label = if count == 1 {
                    " [img] 1 image attached ".to_owned()
                } else {
                    format!(" [img] {count} images attached ")
                };
                let line = Line::from(Span::styled(
                    label,
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                ));
                layout.push_wrapped_line(line, width);
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct AssistantLayoutState {
    prev_was_tool: bool,
    has_body_content: bool,
    has_visible_content: bool,
}

fn append_assistant_blocks(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
    layout: &mut MessageLayout,
) {
    if msg.blocks.is_empty() && spinner.show_compacting {
        layout.push_wrapped_line(compacting_line(spinner.frame), render_context.width);
        return;
    }
    if msg.blocks.is_empty() && spinner.show_empty_thinking {
        layout.push_wrapped_line(thinking_line(spinner.frame), render_context.width);
        return;
    }

    let show_compacting = spinner.show_compacting;
    let deferred_interaction = deferred_hidden_interaction_render_after(&msg.blocks);
    let mut state = AssistantLayoutState::default();
    for idx in 0..msg.blocks.len() {
        if deferred_interaction.is_some_and(|(deferred_idx, _)| deferred_idx == idx) {
            continue;
        }

        append_assistant_block(
            &mut msg.blocks[idx],
            spinner,
            render_context,
            layout,
            &mut state,
        );

        if let Some((deferred_idx, render_after_idx)) = deferred_interaction
            && render_after_idx == idx
        {
            append_assistant_block(
                &mut msg.blocks[deferred_idx],
                spinner,
                render_context,
                layout,
                &mut state,
            );
        }
    }

    if show_compacting {
        if state.has_body_content {
            layout.push_blank();
        }
        layout.push_wrapped_line(compacting_line(spinner.frame), render_context.width);
    }
    if spinner.show_thinking && !show_compacting {
        if state.has_body_content {
            layout.push_blank();
        }
        layout.push_wrapped_line(thinking_line(spinner.frame), render_context.width);
    }
}

fn deferred_hidden_interaction_render_after(blocks: &[MessageBlock]) -> Option<(usize, usize)> {
    let deferred_idx = blocks.iter().position(
        |block| matches!(block, MessageBlock::ToolCall(tc) if tc.is_hidden_focused_interaction()),
    )?;
    let render_after_idx = blocks
        .iter()
        .enumerate()
        .skip(deferred_idx.saturating_add(1))
        .filter_map(|(idx, block)| match block {
            MessageBlock::ToolCall(tc) if tc.is_subagent_root_tool() => Some(idx),
            _ => None,
        })
        .last()?;
    Some((deferred_idx, render_after_idx))
}

fn append_assistant_block(
    block: &mut MessageBlock,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
    layout: &mut MessageLayout,
    state: &mut AssistantLayoutState,
) {
    match block {
        MessageBlock::Text(block) => {
            append_assistant_text_block(block, render_context.width, layout, state);
        }
        MessageBlock::Notice(notice) => {
            append_assistant_notice_block(notice, render_context.width, layout, state);
        }
        MessageBlock::ToolCall(tc) => {
            append_assistant_tool_block(tc.as_mut(), spinner, render_context, layout, state);
        }
        MessageBlock::Welcome(_) | MessageBlock::ImageAttachment(_) => {}
    }
}

fn append_assistant_text_block(
    block: &mut TextBlock,
    width: u16,
    layout: &mut MessageLayout,
    state: &mut AssistantLayoutState,
) {
    if state.prev_was_tool {
        layout.push_blank();
    }
    let rendered = assistant_text_block_layout(block, width, !state.has_visible_content);
    let trailing_gap = trailing_gap_for_text_like_block(
        state.has_visible_content,
        rendered.height,
        block.trailing_blank_lines(),
    );
    layout.push_lines(rendered.lines, rendered.height, rendered.wrapped_lines);
    for _ in 0..trailing_gap {
        layout.push_blank();
    }
    if rendered.height > 0 {
        state.has_body_content = true;
        state.has_visible_content = true;
    }
    state.prev_was_tool = false;
}

fn append_assistant_notice_block(
    notice: &mut crate::state::messages::NoticeBlock,
    width: u16,
    layout: &mut MessageLayout,
    state: &mut AssistantLayoutState,
) {
    if state.prev_was_tool {
        layout.push_blank();
    }
    let rendered = notice_block_layout(notice, width, !state.has_visible_content, notice.severity);
    let trailing_gap = trailing_gap_for_text_like_block(
        state.has_visible_content,
        rendered.height,
        notice.trailing_blank_lines(),
    );
    layout.push_lines(rendered.lines, rendered.height, rendered.wrapped_lines);
    for _ in 0..trailing_gap {
        layout.push_blank();
    }
    if rendered.height > 0 {
        state.has_body_content = true;
        state.has_visible_content = true;
    }
    state.prev_was_tool = false;
}

fn append_assistant_tool_block(
    tc: &mut crate::state::tool_call_info::ToolCallInfo,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
    layout: &mut MessageLayout,
    state: &mut AssistantLayoutState,
) {
    if tc.hidden_unless_focused_interaction() {
        return;
    }
    if !state.prev_was_tool && state.has_body_content {
        layout.push_blank();
    }
    let mut lines = Vec::new();
    tool_call::render_tool_call_cached_with_tools_collapsed(
        tc,
        render_context.tool_render_context,
        render_context.width,
        spinner.frame,
        render_context.options.tools_collapsed,
        &mut lines,
    );
    let (height, wrapped_lines) = tool_call::measure_tool_call_height_cached_with_tools_collapsed(
        tc,
        render_context.tool_render_context,
        render_context.width,
        spinner.frame,
        render_context.layout_generation,
        render_context.options.tools_collapsed,
    );
    layout.push_lines(lines, height, wrapped_lines);
    if height > 0 {
        state.has_body_content = true;
    }
    state.has_visible_content = true;
    state.prev_was_tool = true;
}

fn trailing_gap_for_text_like_block(
    has_visible_content: bool,
    rendered_height: usize,
    trailing_blank_lines: usize,
) -> usize {
    if !has_visible_content && rendered_height == 0 {
        0
    } else {
        trailing_blank_lines
    }
}

fn append_system_blocks(msg: &mut ChatMessage, width: u16, layout: &mut MessageLayout) {
    let color = system_severity_color(system_severity_from_role(&msg.role));
    for block in &mut msg.blocks {
        match block {
            MessageBlock::Text(block) => {
                let trailing_gap = block.trailing_blank_lines();
                let mut rendered = text_block_layout(block, width, None, false);
                tint_lines(&mut rendered.lines, color);
                layout.push_lines(rendered.lines, rendered.height, rendered.wrapped_lines);
                for _ in 0..trailing_gap {
                    layout.push_blank();
                }
            }
            MessageBlock::Notice(notice) => {
                let trailing_gap = notice.trailing_blank_lines();
                let rendered = notice_block_layout(notice, width, false, notice.severity);
                layout.push_lines(rendered.lines, rendered.height, rendered.wrapped_lines);
                for _ in 0..trailing_gap {
                    layout.push_blank();
                }
            }
            MessageBlock::ToolCall(_)
            | MessageBlock::Welcome(_)
            | MessageBlock::ImageAttachment(_) => {}
        }
    }
}

fn system_severity_color(severity: SystemSeverity) -> Color {
    match severity {
        SystemSeverity::Info => theme::DIM,
        SystemSeverity::Warning => theme::STATUS_WARNING,
        SystemSeverity::Error => theme::STATUS_ERROR,
    }
}

fn system_severity_from_role(role: &MessageRole) -> SystemSeverity {
    match role {
        MessageRole::System(level) => level.unwrap_or(SystemSeverity::Error),
        _ => SystemSeverity::Error,
    }
}

/// Measure message height from block caches + width-aware wrapped heights.
/// Returns `(visual_height_rows, lines_wrapped_for_height_updates)`.
///
/// Accuracy is preserved because each block height is computed with
/// `Paragraph::line_count(width)` on the exact rendered `Vec<Line>`.
pub fn measure_message_height_cached(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    layout_generation: u64,
) -> (usize, usize) {
    measure_message_height_cached_with_tools_collapsed(
        msg,
        spinner,
        width,
        layout_generation,
        false,
    )
}

pub fn measure_message_height_cached_with_tools_collapsed(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    layout_generation: u64,
    tools_collapsed: bool,
) -> (usize, usize) {
    measure_message_height_cached_with_tools_collapsed_and_separator(
        msg,
        spinner,
        width,
        layout_generation,
        tools_collapsed,
        true,
    )
}

pub fn measure_message_height_cached_with_tools_collapsed_and_separator(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    layout_generation: u64,
    tools_collapsed: bool,
    include_trailing_separator: bool,
) -> (usize, usize) {
    measure_message_height_cached_with_tools_collapsed_and_separator_and_mode(
        msg,
        spinner,
        None,
        width,
        layout_generation,
        tools_collapsed,
        include_trailing_separator,
    )
}

pub fn measure_message_height_cached_with_tools_collapsed_and_separator_and_mode(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    current_mode_id: Option<&str>,
    width: u16,
    layout_generation: u64,
    tools_collapsed: bool,
    include_trailing_separator: bool,
) -> (usize, usize) {
    let render_context = MessageRenderContext::new(
        current_mode_id,
        width,
        layout_generation,
        MessageRenderOptions {
            tools_collapsed,
            include_trailing_separator,
        },
    );
    let cache = get_or_build_message_render_cache(msg, spinner, render_context);
    (cache.height(), cache.wrapped_lines())
}

/// Render a message while consuming as many whole leading rows as possible.
///
/// `skip_rows` is measured in wrapped visual rows. We skip entire structural parts
/// (label/separators/full blocks) without rendering them. If skipping lands inside
/// a block, that block is rendered in full and the remaining skip is returned so
/// the caller can apply `Paragraph::scroll()` for exact intra-block offset.
#[cfg(test)]
pub(crate) fn render_message_from_offset(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    layout_generation: u64,
    skip_rows: usize,
    out: &mut Vec<Line<'static>>,
) -> usize {
    render_message_from_offset_with_tools_collapsed(
        msg,
        spinner,
        width,
        layout_generation,
        false,
        skip_rows,
        out,
    )
}

#[cfg(test)]
pub(crate) fn render_message_from_offset_with_tools_collapsed(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    layout_generation: u64,
    tools_collapsed: bool,
    skip_rows: usize,
    out: &mut Vec<Line<'static>>,
) -> usize {
    render_message_from_offset_internal(
        msg,
        spinner,
        width,
        layout_generation,
        MessageRenderOptions {
            tools_collapsed,
            include_trailing_separator: true,
        },
        skip_rows,
        out,
    )
}

#[cfg(test)]
pub(crate) fn render_message_from_offset_internal(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    width: u16,
    layout_generation: u64,
    options: MessageRenderOptions,
    skip_rows: usize,
    out: &mut Vec<Line<'static>>,
) -> usize {
    let render_context = MessageRenderContext::new(None, width, layout_generation, options);
    render_message_from_offset_internal_with_mode(msg, spinner, render_context, skip_rows, out)
}

pub(crate) fn render_message_from_offset_internal_with_mode(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
    skip_rows: usize,
    out: &mut Vec<Line<'static>>,
) -> usize {
    let mut remaining_skip = skip_rows;
    let cache = get_or_build_message_render_cache(msg, spinner, render_context);
    let mut can_consume_skip = true;
    render_cached_message_from_offset(
        cache.segments(),
        render_context.width,
        out,
        &mut remaining_skip,
        &mut can_consume_skip,
    );
    remaining_skip
}

fn render_cached_message_from_offset(
    segments: &[CachedMessageSegment],
    width: u16,
    out: &mut Vec<Line<'static>>,
    remaining_skip: &mut usize,
    can_consume_skip: &mut bool,
) {
    for segment in segments {
        match segment {
            CachedMessageSegment::Blank => {
                if *can_consume_skip && *remaining_skip > 0 {
                    *remaining_skip -= 1;
                } else {
                    out.push(Line::default());
                }
            }
            CachedMessageSegment::Lines { lines, height } => {
                if should_skip_whole_block(*height, remaining_skip, can_consume_skip) {
                    continue;
                }
                render_cached_lines_from_offset(
                    lines,
                    width,
                    out,
                    remaining_skip,
                    can_consume_skip,
                );
            }
        }
    }
}

fn render_cached_lines_from_offset(
    lines: &[Line<'static>],
    width: u16,
    out: &mut Vec<Line<'static>>,
    remaining_skip: &mut usize,
    can_consume_skip: &mut bool,
) {
    if !*can_consume_skip || *remaining_skip == 0 {
        out.extend(lines.iter().cloned());
        return;
    }

    for line in lines {
        let logical_lines = split_line_on_newlines(line);
        for logical_line in logical_lines {
            if !*can_consume_skip {
                out.push(logical_line);
                continue;
            }
            let line_height = rendered_line_height(&logical_line, width);
            if *remaining_skip >= line_height {
                *remaining_skip -= line_height;
                continue;
            }
            *can_consume_skip = false;
            out.push(logical_line);
        }
    }
}

fn render_cached_message(segments: &[CachedMessageSegment], out: &mut Vec<Line<'static>>) {
    for segment in segments {
        match segment {
            CachedMessageSegment::Blank => out.push(Line::default()),
            CachedMessageSegment::Lines { lines, .. } => out.extend(lines.iter().cloned()),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MessageRenderOptions {
    pub tools_collapsed: bool,
    pub include_trailing_separator: bool,
}

fn get_or_build_message_render_cache<'a>(
    msg: &'a mut ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
) -> &'a MessageRenderCache {
    let key = build_message_render_cache_key(msg, spinner, render_context);
    if !msg.render_cache.matches(&key) {
        let layout = build_message_layout(msg, spinner, render_context);
        let height = layout.height;
        let wrapped_lines = layout.wrapped_lines;
        let segments = layout
            .segments
            .iter()
            .cloned()
            .map(MessageLayoutSegment::into_cached)
            .collect();
        msg.render_cache.store(key, segments, height, wrapped_lines);
    }
    &msg.render_cache
}

fn build_message_render_cache_key(
    msg: &ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
) -> MessageRenderCacheKey {
    MessageRenderCacheKey {
        width: render_context.width,
        layout_generation: render_context.layout_generation,
        tools_collapsed: render_context.options.tools_collapsed,
        include_trailing_separator: render_context.options.include_trailing_separator,
        render_signature: build_message_render_signature(
            msg,
            spinner,
            render_context.tool_render_context,
        ),
    }
}

fn build_message_render_signature(
    msg: &ChatMessage,
    spinner: &SpinnerState,
    tool_render_context: tool_call::ToolCallRenderContext<'_>,
) -> MessageRenderSignature {
    let assistant_frame = if message_has_frame_dependent_assistant_lines(msg, spinner) {
        Some(spinner.frame)
    } else {
        None
    };
    let blocks = msg
        .blocks
        .iter()
        .map(|block| build_message_block_render_signature(block, spinner, tool_render_context))
        .collect();
    MessageRenderSignature {
        role: msg.role.clone(),
        show_empty_thinking: spinner.show_empty_thinking,
        show_thinking: spinner.show_thinking,
        show_compacting: spinner.show_compacting,
        assistant_frame,
        blocks,
    }
}

fn build_message_block_render_signature(
    block: &MessageBlock,
    spinner: &SpinnerState,
    tool_render_context: tool_call::ToolCallRenderContext<'_>,
) -> MessageBlockRenderSignature {
    match block {
        MessageBlock::Text(block) => MessageBlockRenderSignature::Text {
            text_hash: hash_text_block_content(&block.text, block.trailing_spacing),
            trailing_spacing: block.trailing_spacing,
        },
        MessageBlock::Notice(block) => MessageBlockRenderSignature::Notice {
            severity: block.severity,
            text_hash: hash_text_block_content(&block.text.text, block.text.trailing_spacing),
            trailing_spacing: block.text.trailing_spacing,
        },
        MessageBlock::ToolCall(tc) => MessageBlockRenderSignature::ToolCall {
            render_epoch: tc.render_epoch,
            layout_epoch: tc.layout_epoch,
            hidden: tc.hidden,
            status: tc.status,
            sdk_tool_name: tc.sdk_tool_name.clone(),
            current_mode_id: tool_render_context.current_mode_id.map(str::to_owned),
            pending_permission: tc.pending_permission.is_some(),
            pending_question: tc.pending_question.is_some(),
            frame: tool_call_needs_spinner_frame(tc).then_some(spinner.frame),
        },
        MessageBlock::Welcome(block) => MessageBlockRenderSignature::Welcome {
            content_hash: hash_welcome_block_content(block),
        },
        MessageBlock::ImageAttachment(block) => {
            MessageBlockRenderSignature::ImageAttachment { count: block.count }
        }
    }
}

fn message_has_frame_dependent_assistant_lines(msg: &ChatMessage, spinner: &SpinnerState) -> bool {
    matches!(msg.role, MessageRole::Assistant)
        && (spinner.show_empty_thinking || spinner.show_thinking || spinner.show_compacting)
}

fn tool_call_needs_spinner_frame(tc: &crate::state::tool_call_info::ToolCallInfo) -> bool {
    matches!(
        tc.status,
        crate::state::model::ToolCallStatus::Pending
            | crate::state::model::ToolCallStatus::InProgress
    )
}

fn rendered_lines_height(lines: &[Line<'static>], width: u16) -> usize {
    if lines.is_empty() {
        return 0;
    }
    Paragraph::new(Text::from(lines.to_vec()))
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn rendered_line_height(line: &Line<'static>, width: u16) -> usize {
    Paragraph::new(Text::from(vec![line.clone()]))
        .wrap(Wrap { trim: false })
        .line_count(width)
        .max(1)
}

fn split_line_on_newlines(line: &Line<'static>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut current_spans = Vec::new();

    for span in &line.spans {
        for chunk in span.content.as_ref().split_inclusive('\n') {
            let ends_with_newline = chunk.ends_with('\n');
            let content = chunk.strip_suffix('\n').unwrap_or(chunk);
            if !content.is_empty() {
                let mut next_span = span.clone();
                next_span.content = content.to_owned().into();
                current_spans.push(next_span);
            }
            if ends_with_newline {
                lines.push(Line::from(std::mem::take(&mut current_spans)));
            }
        }
    }

    lines.push(Line::from(current_spans));
    lines
}

fn welcome_block_layout(block: &mut WelcomeBlock, width: u16) -> RenderedBlockLayout {
    let had_height = block.cache.height_at(width).is_some();
    let mut lines = Vec::new();
    render_welcome_cached(block, width, &mut lines);
    let height = block.cache.height_at(width).unwrap_or_else(|| {
        let height = rendered_lines_height(&lines, width);
        block.cache.set_height(height, width);
        height
    });
    let wrapped_lines = if had_height { 0 } else { lines.len() };
    RenderedBlockLayout {
        lines,
        height,
        wrapped_lines,
    }
}

fn text_block_layout(
    block: &mut TextBlock,
    width: u16,
    bg: Option<Color>,
    preserve_newlines: bool,
) -> RenderedBlockLayout {
    let had_height = block.cache.height_at(width).is_some();
    let mut lines = Vec::new();
    render_text_block_cached(block, width, bg, preserve_newlines, &mut lines);
    let height = block.cache.height_at(width).unwrap_or_else(|| {
        let height = rendered_lines_height(&lines, width);
        block.cache.set_height(height, width);
        height
    });
    let wrapped_lines = if had_height { 0 } else { lines.len() };
    RenderedBlockLayout {
        lines,
        height,
        wrapped_lines,
    }
}

fn assistant_text_block_layout(
    block: &mut TextBlock,
    width: u16,
    trim_leading_blank_lines: bool,
) -> RenderedBlockLayout {
    let mut rendered = text_block_layout(block, width, None, false);

    if trim_leading_blank_lines {
        let leading_blank_lines = count_leading_blank_lines(&rendered.lines);
        if leading_blank_lines > 0 {
            rendered.lines.drain(..leading_blank_lines);
            rendered.height = rendered.height.saturating_sub(leading_blank_lines);
            rendered.wrapped_lines = rendered.wrapped_lines.saturating_sub(leading_blank_lines);
        }
    }

    rendered
}

fn notice_block_layout(
    block: &mut crate::state::messages::NoticeBlock,
    width: u16,
    trim_leading_blank_lines: bool,
    severity: SystemSeverity,
) -> RenderedBlockLayout {
    let mut rendered =
        assistant_text_block_layout(&mut block.text, width, trim_leading_blank_lines);
    tint_lines(&mut rendered.lines, system_severity_color(severity));
    rendered
}

fn count_leading_blank_lines(lines: &[Line<'static>]) -> usize {
    lines.iter().take_while(|line| line_is_blank(line)).count()
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .all(|span| span.content.as_ref().chars().all(char::is_whitespace))
}

fn should_skip_whole_block(
    block_h: usize,
    remaining_skip: &mut usize,
    can_consume_skip: &mut bool,
) -> bool {
    if !*can_consume_skip {
        return false;
    }
    if *remaining_skip >= block_h {
        *remaining_skip -= block_h;
        return true;
    }
    if *remaining_skip > 0 {
        *can_consume_skip = false;
    }
    false
}

fn role_label_line(role: &MessageRole) -> Line<'static> {
    match role {
        MessageRole::Welcome => Line::from(Span::styled(
            "Overview",
            Style::default()
                .fg(theme::RUST_ORANGE)
                .add_modifier(Modifier::BOLD),
        )),
        MessageRole::User => Line::from(Span::styled(
            "User",
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        )),
        MessageRole::Assistant => assistant_role_label_line(),
        MessageRole::System(_) => system_role_label_line(system_severity_from_role(role)),
    }
}

fn system_role_label_line(severity: SystemSeverity) -> Line<'static> {
    let (label, color) = match severity {
        SystemSeverity::Info => ("Info", theme::DIM),
        SystemSeverity::Warning => ("Warning", theme::STATUS_WARNING),
        SystemSeverity::Error => ("Error", theme::STATUS_ERROR),
    };
    Line::from(Span::styled(
        label,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

fn thinking_line(frame: usize) -> Line<'static> {
    let ch = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
    Line::from(Span::styled(
        format!("{ch} Thinking..."),
        Style::default().fg(theme::DIM),
    ))
}

fn compacting_line(frame: usize) -> Line<'static> {
    let ch = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
    Line::from(Span::styled(
        format!("{ch} Compacting context..."),
        Style::default().fg(theme::RUST_ORANGE),
    ))
}

fn welcome_lines(block: &WelcomeBlock, _width: u16) -> Vec<Line<'static>> {
    let pad = "  ";
    let mut lines = Vec::new();
    for art_line in FERRIS_SAYS {
        lines.push(Line::from(Span::styled(
            format!("{pad}{art_line}"),
            Style::default().fg(theme::RUST_ORANGE),
        )));
    }

    lines.push(Line::default());
    lines.push(Line::default());

    lines.push(Line::from(vec![
        Span::styled(
            format!("{pad}Version:      "),
            Style::default().fg(theme::DIM),
        ),
        Span::styled(block.version.clone(), Style::default().fg(theme::DIM)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            format!("{pad}Subscription: "),
            Style::default().fg(theme::DIM),
        ),
        Span::styled(
            block.subscription.clone(),
            Style::default()
                .fg(theme::RUST_ORANGE)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!("{pad}cwd:          {}", block.cwd),
        Style::default().fg(theme::DIM),
    )));
    lines.push(Line::from(Span::styled(
        format!("{pad}Session ID:   {}", block.session_id),
        Style::default().fg(theme::DIM),
    )));

    lines.push(Line::default());
    // TODO: Replace the hard-coded tip text with a small array of welcome tips
    // and randomized selection once this becomes a first-class surface.
    lines.push(Line::from(Span::styled(
        format!("{pad}Tips: {}", selected_welcome_tip(block)),
        Style::default().fg(theme::DIM),
    )));
    lines.push(Line::default());

    lines
}

fn selected_welcome_tip(block: &WelcomeBlock) -> &'static str {
    let Some(first_tip) = WELCOME_TIPS.first().copied() else {
        return "Enter sends, Shift+Enter inserts a newline, and Ctrl+C copies a selection or quits";
    };
    let len_u64 = u64::try_from(WELCOME_TIPS.len()).unwrap_or(1);
    let idx_u64 = block.tip_seed % len_u64;
    let idx = usize::try_from(idx_u64).unwrap_or(0);
    WELCOME_TIPS.get(idx).copied().unwrap_or(first_tip)
}

fn render_welcome_cached(block: &mut WelcomeBlock, width: u16, out: &mut Vec<Line<'static>>) {
    if let Some(cached_lines) = block.cache.get() {
        out.extend_from_slice(cached_lines);
        return;
    }

    let fresh = welcome_lines(block, width);
    let h = {
        let _t = crate::perf::start_with("msg::wrap_height", "lines", fresh.len());
        Paragraph::new(Text::from(fresh.clone()))
            .wrap(Wrap { trim: false })
            .line_count(width)
    };
    block.cache.store(fresh);
    block.cache.set_height(h, width);
    if let Some(stored) = block.cache.get() {
        out.extend_from_slice(stored);
    }
}

fn tint_lines(lines: &mut [Line<'static>], color: Color) {
    for line in lines {
        for span in &mut line.spans {
            span.style = span.style.fg(color);
        }
    }
}

/// Preprocess markdown that `tui_markdown` doesn't handle well.
/// Headings (`# Title`) become `**Title**` (bold) with a blank line before.
/// Handles variations: `#Title`, `#  Title`, `  ## Title  `, etc.
/// Links are left as-is -- `tui_markdown` handles `[title](url)` natively.
fn preprocess_markdown(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            // Strip all leading '#' characters
            let after_hashes = trimmed.trim_start_matches('#');
            // Extract heading content (trim spaces between # and text, and trailing)
            let content = after_hashes.trim();
            if !content.is_empty() {
                // Blank line before heading for visual separation
                if !result.is_empty() && !result.ends_with("\n\n") {
                    result.push('\n');
                }
                result.push_str("**");
                result.push_str(content);
                result.push_str("**\n");
                continue;
            }
        }
        result.push_str(line);
        result.push('\n');
    }
    if !text.ends_with('\n') {
        result.pop();
    }
    result
}

/// Render a text block with caching. Uses paragraph-level incremental markdown
/// during streaming to avoid re-parsing the entire text every frame.
///
/// Cache hierarchy:
/// 1. `BlockCache` (full block) -- hit for completed messages (no changes).
/// 2. `IncrementalMarkdown` (per-paragraph) -- only tail paragraph re-parsed during streaming.
pub(super) fn render_text_cached(
    text: &str,
    cache: &mut BlockCache,
    incr: &mut IncrementalMarkdown,
    width: u16,
    bg: Option<Color>,
    preserve_newlines: bool,
    out: &mut Vec<Line<'static>>,
) {
    // Fast path only when the cached lines were measured at this width.
    // Markdown tables produce width-dependent logical lines before paragraph
    // wrapping, so a fresh cache from another width is not safe to reuse.
    if cache.height_at(width).is_some()
        && let Some(cached_lines) = cache.get()
    {
        crate::perf::mark_with("msg::cache_hit", "lines", cached_lines.len());
        out.extend_from_slice(cached_lines);
        return;
    }
    crate::perf::mark("msg::cache_miss");

    let _t = crate::perf::start("msg::render_text");

    // Build a render function that handles preprocessing + tui_markdown
    let render_fn = |src: &str| -> Vec<Line<'static>> {
        let mut preprocessed = preprocess_markdown(src);
        if preserve_newlines {
            preprocessed = force_markdown_line_breaks(&preprocessed);
        }
        crate::ui::document_table::render_markdown_with_tables(&preprocessed, width, bg)
    };
    let render_key = MarkdownRenderKey {
        width,
        bg,
        preserve_newlines,
    };

    // Ensure any previously invalidated paragraph caches are re-rendered
    let _ = text;
    incr.ensure_rendered(render_key, &render_fn);

    // Render: cached paragraphs + fresh tail
    let fresh = incr.lines(render_key, &render_fn);

    // Store in the full block cache with wrapped height.
    // For streaming messages this will be invalidated on the next chunk,
    // but for completed messages it persists.
    let h = {
        let _t = crate::perf::start_with("msg::wrap_height", "lines", fresh.len());
        Paragraph::new(Text::from(fresh.clone()))
            .wrap(Wrap { trim: false })
            .line_count(width)
    };
    cache.store(fresh);
    cache.set_height(h, width);
    if let Some(stored) = cache.get() {
        out.extend_from_slice(stored);
    }
}

fn render_text_block_cached(
    block: &mut TextBlock,
    width: u16,
    bg: Option<Color>,
    preserve_newlines: bool,
    out: &mut Vec<Line<'static>>,
) {
    render_text_cached(
        &block.text,
        &mut block.cache,
        &mut block.markdown,
        width,
        bg,
        preserve_newlines,
        out,
    );
}

/// Convert single line breaks into hard breaks so user-entered newlines persist.
fn force_markdown_line_breaks(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len());
    for (i, line) in lines.iter().enumerate() {
        if !line.is_empty() {
            out.push_str(line);
            out.push_str("  ");
        }
        if i + 1 < lines.len() || text.ends_with('\n') {
            out.push('\n');
        }
    }
    if text.ends_with('\n') {
        // preserve trailing newline
    }
    out
}
