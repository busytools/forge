use crate::app::{
    BlockCache, CachedMessageSegment, ChatMessage, IncrementalMarkdown, MarkdownRenderKey,
    MessageBlock, MessageRenderCache, MessageRenderCacheKey, MessageRenderSignature, MessageRole,
    SystemSeverity, TextBlock, WelcomeBlock, hash_text_block_content, hash_welcome_block_content,
};
use crate::ui::peer_block;
use crate::ui::theme;
use crate::ui::tool_call;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];

const FERRIS_SAYS: &[&str] = &[
    r" ------------------------ ",
    r"< Welcome back to Forge! >",
    r" ------------------------ ",
    r"        \             ",
    r"         \            ",
    r"            _~^~^~_  ",
    r"        \) /  o o  \ (/",
    r"          '_   -   _' ",
    r"          / '-----' \ ",
];

const WELCOME_TIPS: &[&str] = &[
    "Use /mode plan before larger changes, then switch back to code once the plan is clear",
    "Use /mcp to connect live tools and docs instead of pasting stale context into chat",
    "Keep repo instructions short in CLAUDE.md and update them when mistakes repeat",
    "Start prompts with the goal, relevant context, and constraints so Claude needs fewer corrections",
    "Ask Claude for a plan first on multi-step work instead of jumping straight to edits",
    "Give success criteria Claude can verify: tests, lint, screenshots, or exact outputs",
    "For visual work, paste screenshots or mockups so Claude can verify UI changes instead of guessing",
    "Start a fresh thread with /new when the task changes and old context is noise",
    "Use /compact when a session gets long and you want to keep the thread but trim context",
    "Use /resume <session_id> to jump back into earlier work without rebuilding context",
    "Use /config or F1 to open settings; usage, plugins, MCP, and status are tabs in there",
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
// Spinner state — bools track frame ticks, blink flag, halted, idle, etc. — separate flags read better than a packed bitmask at call sites.
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
        Self { segments: Vec::new(), height: 0, wrapped_lines: 0 }
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
        self.segments.push(MessageLayoutSegment::Lines { lines, height });
        self.height += height;
        self.wrapped_lines += wrapped_lines;
    }
}

#[derive(Clone)]
enum MessageLayoutSegment {
    Blank,
    Lines { lines: Vec<Line<'static>>, height: usize },
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
        "Forge",
        Style::default().fg(theme::ROLE_ASSISTANT).add_modifier(Modifier::BOLD),
    )];

    Line::from(spans)
}

pub(crate) fn render_message(
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
    if !render_context.options.suppress_group_header {
        layout.push_wrapped_line(role_label_line(msg), render_context.width);
    }

    match msg.role {
        MessageRole::Welcome => append_welcome_blocks(msg, render_context.width, &mut layout),
        MessageRole::User => append_user_blocks(
            msg,
            render_context.width,
            render_context.options.tools_collapsed,
            render_context.options.envelope_streak_position,
            &mut layout,
        ),
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

fn append_user_blocks(
    msg: &mut ChatMessage,
    width: u16,
    tools_collapsed: bool,
    envelope_streak_position: Option<EnvelopeStreakPosition>,
    layout: &mut MessageLayout,
) {
    for block in &mut msg.blocks {
        match block {
            MessageBlock::Text(block) => {
                // Peer-coordination wrappers (#114) — when the
                // workspace injects a `[Question id=…]` /
                // `[Reply id=…]` / etc. user-turn, render a styled
                // peer block instead of the default user bubble.
                // Collapse state mirrors the global tool-card
                // preference so Ctrl+X flips peer rows and tool rows
                // together. Inbound peer turns don't (yet) have a
                // per-row override the way ToolCallInfo does — the
                // global default is the only knob.
                if let Some(kind) = peer_block::detect_inbound(&block.text) {
                    let trailing_gap = block.trailing_blank_lines();
                    let collapsed = block.peer_collapsed_override.unwrap_or(tools_collapsed);
                    // #163 + #189: same-worker streak followers drop
                    // the `▶ Verb name` header line and just stack body
                    // lines under the previous envelope. Different-worker
                    // followers and streak-starters get the full header
                    // shape - one identity per row.
                    let suppress_header = matches!(
                        envelope_streak_position,
                        Some(EnvelopeStreakPosition::FollowerSameWorker)
                    );
                    let suppress_trailing_gap = matches!(
                        envelope_streak_position,
                        Some(
                            EnvelopeStreakPosition::FollowerNewWorker
                                | EnvelopeStreakPosition::FollowerSameWorker
                        )
                    );
                    let lines = peer_block::render_inbound(&kind, suppress_header, collapsed);
                    let y_in_msg = layout.height;
                    let height = rendered_lines_height(&lines, width);
                    layout.push_wrapped_lines(lines, width);
                    // Stamp hit-target fields so `mouse::locate_
                    // peer_user_block_at_click` can route clicks on
                    // this inbound peer row back to this TextBlock
                    // and flip `peer_collapsed_override`.
                    block.peer_last_measured_y_in_msg = y_in_msg;
                    block.peer_last_measured_height = height;
                    block.peer_last_measured_width = width;
                    if !suppress_trailing_gap {
                        for _ in 0..trailing_gap {
                            layout.push_blank();
                        }
                    }
                    continue;
                }
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
    let mut state = AssistantLayoutState::default();
    for idx in 0..msg.blocks.len() {
        append_assistant_block(&mut msg.blocks[idx], spinner, render_context, layout, &mut state);
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
    notice: &mut crate::app::NoticeBlock,
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
    tc: &mut crate::app::ToolCallInfo,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
    layout: &mut MessageLayout,
    state: &mut AssistantLayoutState,
) {
    if tc.hidden_unless_focused_interaction() {
        return;
    }
    // Peer-coordination outbound (#114) — replace the default
    // tool_use card for `mcp__forge__peers__ask_agent` /
    // `peers__tell_agent` with a styled peer block in the same
    // tool-card shape (status icon + kind label + tree body).
    // Collapse state follows the standard tool-call rule: per-tc
    // `collapsed_override` wins, otherwise the global default.
    // Click-to-toggle on peer rows currently piggybacks on the
    // existing tool-call row hit-test in mouse.rs.
    if let Some(kind) = peer_block::detect_outbound(tc) {
        if !state.prev_was_tool && state.has_body_content {
            layout.push_blank();
        }
        // #143 item 5: routine `mcp__forge__*` calls collapse to a
        // one-line summary by default — the wire shape is
        // predictable and the user-facing intent is target +
        // correlation_id, not the JSON args. A per-tc
        // `collapsed_override` (set by clicking on the row) still
        // wins so the user can expand for a specific call when
        // they want the body preview. The global `tools_collapsed`
        // setting is ignored on this path because these cards are
        // intentionally compact by default; it'd be confusing to
        // make them honor a setting whose name implies the
        // OPPOSITE default behaviour ("tools_collapsed=false" =
        // "show full tool cards" = wrong for these).
        let collapsed = tc.collapsed_override.unwrap_or(true);
        let lines = peer_block::render_outbound(&kind, collapsed);
        // Same hit-target stamping the standard tool-call branch
        // below does so `mouse::locate_tool_call_block_at_click` can
        // map a click on a peer row back to this ToolCallInfo and
        // flip `collapsed_override`. Without these fields set the
        // hit-test in mouse.rs short-circuits at `last_measured_height
        // == 0` and the click falls through to text selection.
        let y_in_msg = layout.height;
        let height = rendered_lines_height(&lines, render_context.width);
        layout.push_wrapped_lines(lines, render_context.width);
        tc.last_measured_y_in_msg = y_in_msg;
        tc.last_measured_height = height;
        tc.last_measured_width = render_context.width;
        state.has_body_content = true;
        state.has_visible_content = true;
        state.prev_was_tool = true;
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
    // Capture the tool's wrapped-row offset within this message *after*
    // any leading blank from the prev-was-tool/has-body-content gap so
    // mouse hit-testing can locate the rendered row range directly
    // from the tool's own state — no need to walk text-block heights
    // (which can return None when their cache version is stale).
    let y_in_msg = layout.height;
    layout.push_lines(lines, height, wrapped_lines);
    tc.last_measured_y_in_msg = y_in_msg;
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
    if !has_visible_content && rendered_height == 0 { 0 } else { trailing_blank_lines }
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
    measure_message_height_cached_with_options(
        msg,
        spinner,
        current_mode_id,
        width,
        layout_generation,
        MessageRenderOptions {
            tools_collapsed,
            include_trailing_separator,
            suppress_group_header: false,
            envelope_streak_position: None,
        },
    )
}

/// Lowest-level measurement helper — accepts the full
/// `MessageRenderOptions` so callers that compute
/// `suppress_group_header` (chat.rs's measure + render passes for
/// same-project envelope grouping) can thread it through without
/// growing the granular helper's parameter list further.
pub fn measure_message_height_cached_with_options(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    current_mode_id: Option<&str>,
    width: u16,
    layout_generation: u64,
    options: MessageRenderOptions,
) -> (usize, usize) {
    let render_context =
        MessageRenderContext::new(current_mode_id, width, layout_generation, options);
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
            suppress_group_header: false,
            envelope_streak_position: None,
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

#[derive(Clone, Copy, Default)]
pub(crate) struct MessageRenderOptions {
    pub tools_collapsed: bool,
    pub include_trailing_separator: bool,
    /// True when this message is a peer-MCP / worker-MCP envelope and
    /// the prior message in the FULL chat list had the same
    /// `sender_org`. Suppresses the `forge` role label at the top so
    /// consecutive same-project envelopes read as a group-chat-style
    /// streak (one label, N bodies) instead of repeating the label per
    /// envelope. Computed by the chat iterator (see `crate::ui::chat`)
    /// from the chat-wide previous message's envelope org.
    ///
    /// Sticky-header for scroll-back is NOT implemented: when the user
    /// scrolls past a streak's first envelope, the new first-visible
    /// envelope shows no label. The viewport-anchor alternative
    /// (re-render the header for the first visible mid-group envelope)
    /// would split the cache key per scroll position and flap entries
    /// on every viewport change. Filed as v2 if user feedback warrants.
    pub suppress_group_header: bool,
    /// Position of this message inside its envelope streak when it
    /// IS an envelope. Drives the peer-block renderer's branch
    /// between full streak-starter chrome and compact follower
    /// shape (#163). `None` for non-envelope messages.
    pub envelope_streak_position: Option<EnvelopeStreakPosition>,
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
        let segments =
            layout.segments.iter().cloned().map(MessageLayoutSegment::into_cached).collect();
        msg.render_cache.store(key, segments, height, wrapped_lines);
    }
    &msg.render_cache
}

fn build_message_render_cache_key(
    msg: &ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
) -> MessageRenderCacheKey {
    let envelope_streak_position_ord = match render_context.options.envelope_streak_position {
        None => 0,
        Some(EnvelopeStreakPosition::Start) => 1,
        Some(EnvelopeStreakPosition::FollowerNewWorker) => 2,
        Some(EnvelopeStreakPosition::FollowerSameWorker) => 3,
    };
    MessageRenderCacheKey {
        width: render_context.width,
        layout_generation: render_context.layout_generation,
        tools_collapsed: render_context.options.tools_collapsed,
        include_trailing_separator: render_context.options.include_trailing_separator,
        suppress_group_header: render_context.options.suppress_group_header,
        envelope_streak_position_ord,
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
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    msg.role.hash(&mut hasher);
    spinner.show_empty_thinking.hash(&mut hasher);
    spinner.show_thinking.hash(&mut hasher);
    spinner.show_compacting.hash(&mut hasher);
    let assistant_frame = if message_has_frame_dependent_assistant_lines(msg, spinner) {
        Some(spinner.frame)
    } else {
        None
    };
    assistant_frame.hash(&mut hasher);
    for block in &msg.blocks {
        hash_message_block_into(&mut hasher, block, spinner, tool_render_context);
    }
    MessageRenderSignature(hasher.finish())
}

/// Discriminant tags used while folding `MessageBlock` variants into
/// the message-level signature hash. Stable values matter: changing
/// any tag invalidates every previously-cached render, but order
/// independence between variants is what stops a Text-with-N bytes
/// from ever colliding with a Notice with the same N bytes.
mod block_tag {
    pub const TEXT: u8 = 0;
    pub const NOTICE: u8 = 1;
    pub const TOOL_CALL: u8 = 2;
    pub const WELCOME: u8 = 3;
    pub const IMAGE_ATTACHMENT: u8 = 4;
}

fn hash_message_block_into<H: std::hash::Hasher>(
    hasher: &mut H,
    block: &MessageBlock,
    spinner: &SpinnerState,
    tool_render_context: tool_call::ToolCallRenderContext<'_>,
) {
    use std::hash::Hash;
    match block {
        MessageBlock::Text(block) => {
            block_tag::TEXT.hash(hasher);
            hash_text_block_content(&block.text, block.trailing_spacing).hash(hasher);
            block.trailing_spacing.hash(hasher);
            // Peer-block collapse state (#114). Without this in the
            // signature, flipping `peer_collapsed_override` from a
            // click handler is a no-op visually because the message
            // render cache reuses the previous layout.
            block.peer_collapsed_override.hash(hasher);
        }
        MessageBlock::Notice(block) => {
            block_tag::NOTICE.hash(hasher);
            block.severity.hash(hasher);
            hash_text_block_content(&block.text.text, block.text.trailing_spacing).hash(hasher);
            block.text.trailing_spacing.hash(hasher);
        }
        MessageBlock::ToolCall(tc) => {
            block_tag::TOOL_CALL.hash(hasher);
            tc.render_epoch.hash(hasher);
            tc.layout_epoch.hash(hasher);
            tc.hidden.hash(hasher);
            tc.status.hash(hasher);
            tc.sdk_tool_name.hash(hasher);
            tool_render_context.current_mode_id.hash(hasher);
            // Per-tool collapse override flips the rendered shape, so it
            // has to be folded into the signature alongside the global
            // tools_collapsed bit (which lives on MessageRenderCacheKey).
            tc.collapsed_override.hash(hasher);
            let frame = tool_call_needs_spinner_frame(tc).then_some(spinner.frame);
            frame.hash(hasher);
        }
        MessageBlock::Welcome(block) => {
            block_tag::WELCOME.hash(hasher);
            hash_welcome_block_content(block).hash(hasher);
        }
        MessageBlock::ImageAttachment(block) => {
            block_tag::IMAGE_ATTACHMENT.hash(hasher);
            block.count.hash(hasher);
        }
    }
}

fn message_has_frame_dependent_assistant_lines(msg: &ChatMessage, spinner: &SpinnerState) -> bool {
    matches!(msg.role, MessageRole::Assistant)
        && (spinner.show_empty_thinking || spinner.show_thinking || spinner.show_compacting)
}

fn tool_call_needs_spinner_frame(tc: &crate::app::ToolCallInfo) -> bool {
    matches!(
        tc.status,
        crate::agent::model::ToolCallStatus::Pending
            | crate::agent::model::ToolCallStatus::InProgress
    )
}

fn rendered_lines_height(lines: &[Line<'static>], width: u16) -> usize {
    if lines.is_empty() {
        return 0;
    }
    Paragraph::new(Text::from(lines.to_vec())).wrap(Wrap { trim: false }).line_count(width)
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
    RenderedBlockLayout { lines, height, wrapped_lines }
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
    RenderedBlockLayout { lines, height, wrapped_lines }
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
    block: &mut crate::app::NoticeBlock,
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
    line.spans.iter().all(|span| span.content.as_ref().chars().all(char::is_whitespace))
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

fn role_label_line(msg: &ChatMessage) -> Line<'static> {
    match msg.role {
        MessageRole::Welcome => Line::from(Span::styled(
            "Overview",
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        )),
        MessageRole::User => {
            // Peer / worker MCP envelopes ride on the User role at
            // the SDK protocol level (claude treats them as user
            // turns), but they're agent-to-agent traffic — the chat
            // label "User" misrepresents them as human input.
            // Distinguish: real human input keeps the "User" label;
            // any User message whose first text block is a peer-
            // envelope bracket re-labels as `Forge` to match the
            // matching Assistant-side outbound label. Reserves the
            // "User" treatment for things actually typed by the
            // human at the prompt.
            if is_peer_envelope_user_message(msg) {
                Line::from(Span::styled(
                    "Forge",
                    Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(Span::styled(
                    "User",
                    Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
                ))
            }
        }
        MessageRole::Assistant => assistant_role_label_line(),
        MessageRole::System(_) => system_role_label_line(system_severity_from_role(&msg.role)),
    }
}

/// True when this `MessageRole::User` carries a peer / worker MCP
/// inbound envelope (a `[Question id=q-...]`, `[Message id=t-...]`,
/// `[Reply id=t-...]`, or one of the timeout/expired/failed
/// notification shapes).
///
/// #143 item 2: reads the cached `is_peer_envelope` flag on
/// `ChatMessage` (stamped at push time by the
/// `PeerEnvelopeAppended` path via `ChatMessage::new_peer_envelope`)
/// rather than walking blocks + running `detect_inbound` per frame.
/// The walk was a hot path under heavy envelope traffic — the role
/// label re-evaluates on every render of every chat message.
fn is_peer_envelope_user_message(msg: &ChatMessage) -> bool {
    msg.is_peer_envelope
}

/// Extract the `sender_org` tag from this message's peer envelope,
/// if any. Drives the same-project envelope grouping at
/// `compute_suppress_group_header` (chat-iteration level).
///
/// Two envelope shapes count:
/// - **Inbound** (User role): a `[Question id=...]` / `[Message id=...]`
///   bracket whose `(org '...')` field is the wire-level sender_org.
/// - **Assistant peer-outbound**: an Assistant turn carrying a
///   `mcp__forge__peers__*` / `mcp__forge__workers__{ask,tell}`
///   tool_use card. These have no native org, but they belong to
///   the same Forge-mediated conversation as the surrounding
///   inbound envelopes - synthesise `"Personal"` so streak detection
///   folds across them (the most common case is a worker's chat
///   interleaving inbound from lead with outbound to lead, both of
///   which share the lead's `"Personal"` org).
///
/// Returns `None` for everything else: plain user input, regular
/// assistant text, system notices, non-peer tool_use cards.
pub(crate) fn message_envelope_org(msg: &ChatMessage) -> Option<String> {
    use crate::ui::peer_block::{detect_inbound, detect_outbound};
    match msg.role {
        MessageRole::User => msg.blocks.iter().find_map(|block| match block {
            MessageBlock::Text(text) => {
                detect_inbound(&text.text).map(|kind| kind.org().to_owned())
            }
            _ => None,
        }),
        MessageRole::Assistant => msg.blocks.iter().find_map(|block| match block {
            MessageBlock::ToolCall(tc) => detect_outbound(tc).map(|_| "Personal".to_owned()),
            _ => None,
        }),
        _ => None,
    }
}

/// Decide whether `messages[idx]` should suppress its role-label line.
/// Group-chat-style collapse: returns `true` iff the message AND its
/// immediate predecessor are both peer/worker envelopes carrying the
/// same `sender_org`. Used by chat.rs to thread `suppress_group_header`
/// through both the measure and render passes consistently so the
/// `MessageRenderCacheKey` stays stable per message.
pub(crate) fn compute_suppress_group_header(messages: &[ChatMessage], idx: usize) -> bool {
    if idx == 0 {
        return false;
    }
    let Some(cur) = message_envelope_org(&messages[idx]) else {
        return false;
    };
    let Some(prev) = message_envelope_org(&messages[idx - 1]) else {
        return false;
    };
    cur == prev
}

/// Position of one envelope inside a same-project envelope streak.
/// Drives the peer-block renderer's branch between the full
/// streak-starter shape (existing `render_peer_card` chrome with
/// kind label + sender + id + org meta) and the compact follower
/// shape introduced for #163 (worker label inline with body, no
/// id/org meta, no blank line between followers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvelopeStreakPosition {
    /// First envelope in a streak (or solo envelope, or post-break
    /// envelope). Renders with the full peer-block card.
    Start,
    /// Subsequent envelope in a same-project streak whose
    /// `sender_name` differs from the previous envelope's sender.
    /// Renders compactly with the worker label prefix.
    FollowerNewWorker,
    /// Subsequent envelope in a same-project streak from the SAME
    /// worker as the previous envelope. Renders compactly without
    /// the worker label — body continues under the existing tag
    /// column so a contiguous run of one worker's messages reads as
    /// one paragraph.
    FollowerSameWorker,
}

/// Extract this message's envelope sender_name (the worker label
/// or the project name for lead-to-lead peer messages). Returns
/// `None` for non-envelope messages.
fn message_envelope_sender(msg: &ChatMessage) -> Option<String> {
    use crate::ui::peer_block::detect_inbound;
    if !matches!(msg.role, MessageRole::User) {
        return None;
    }
    msg.blocks.iter().find_map(|block| match block {
        MessageBlock::Text(text) => detect_inbound(&text.text).map(|kind| match kind {
            crate::ui::peer_block::PeerInboundKind::Question { from, .. }
            | crate::ui::peer_block::PeerInboundKind::Message { from, .. }
            | crate::ui::peer_block::PeerInboundKind::Reply { from, .. }
            | crate::ui::peer_block::PeerInboundKind::LateReply { from, .. }
            | crate::ui::peer_block::PeerInboundKind::RecipientExpired { from, .. } => from,
            crate::ui::peer_block::PeerInboundKind::CallerTimeout { target, .. }
            | crate::ui::peer_block::PeerInboundKind::DeliveryFailure { target, .. } => target,
            crate::ui::peer_block::PeerInboundKind::WorkerSpawnFailed { label, .. } => label,
        }),
        _ => None,
    })
}

/// Decide where `messages[idx]` sits inside its envelope streak.
/// Returns `None` for non-envelope messages (peer-block renderer is
/// not invoked for them).
///
/// Position depends on the FULL chat list (matching #161's
/// chat-wide grouping). The first-visible streak follower in a
/// scroll-back view still renders as a follower; sticky-header
/// is intentionally deferred per #158.
pub(crate) fn compute_envelope_streak_position(
    messages: &[ChatMessage],
    idx: usize,
) -> Option<EnvelopeStreakPosition> {
    let cur_org = message_envelope_org(&messages[idx])?;
    if idx == 0 {
        return Some(EnvelopeStreakPosition::Start);
    }
    let prev_org = message_envelope_org(&messages[idx - 1]);
    if prev_org.as_deref() != Some(cur_org.as_str()) {
        // Previous message either isn't an envelope or is from a
        // different project — this envelope starts a fresh streak.
        return Some(EnvelopeStreakPosition::Start);
    }
    // Same-project streak follower. Distinguish same-worker
    // (continuation under existing tag) from new-worker (own tag).
    let cur_sender = message_envelope_sender(&messages[idx]);
    let prev_sender = message_envelope_sender(&messages[idx - 1]);
    if cur_sender.is_some() && cur_sender == prev_sender {
        Some(EnvelopeStreakPosition::FollowerSameWorker)
    } else {
        Some(EnvelopeStreakPosition::FollowerNewWorker)
    }
}

fn system_role_label_line(severity: SystemSeverity) -> Line<'static> {
    let (label, color) = match severity {
        SystemSeverity::Info => ("Info", theme::DIM),
        SystemSeverity::Warning => ("Warning", theme::STATUS_WARNING),
        SystemSeverity::Error => ("Error", theme::STATUS_ERROR),
    };
    Line::from(Span::styled(label, Style::default().fg(color).add_modifier(Modifier::BOLD)))
}

fn thinking_line(frame: usize) -> Line<'static> {
    let ch = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
    Line::from(Span::styled(format!("{ch} Thinking..."), Style::default().fg(theme::DIM)))
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
        Span::styled(format!("{pad}Version:      "), Style::default().fg(theme::DIM)),
        Span::styled(block.version.clone(), Style::default().fg(theme::DIM)),
    ]));
    // Label is dynamic: "Account" when forge-workspace picked the
    // account, "Subscription" for direct Agent::spawn callers
    // (tests / smoke). Width-pad to 13 chars + 1 space = 14 chars
    // total to align with Version/cwd/Session ID rows.
    //
    // Skip the line entirely when value is empty (no data yet) —
    // avoids flashing a placeholder while the workspace picker /
    // status snapshot are still in flight.
    if !block.subscription.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{pad}{:<13} ", format!("{}:", block.account_label)),
                Style::default().fg(theme::DIM),
            ),
            Span::styled(
                block.subscription.clone(),
                Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled(
        format!("{pad}cwd:          {}", block.cwd),
        Style::default().fg(theme::DIM),
    )));
    lines.push(Line::from(Span::styled(
        format!("{pad}Session ID:   {}", block.session_id),
        Style::default().fg(theme::DIM),
    )));

    lines.push(Line::default());
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
        Paragraph::new(Text::from(fresh.clone())).wrap(Wrap { trim: false }).line_count(width)
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
        super::document_table::render_markdown_with_tables(&preprocessed, width, bg)
    };
    let render_key = MarkdownRenderKey { width, bg, preserve_newlines };

    // Ensure any previously invalidated paragraph caches are re-rendered
    incr.ensure_rendered(render_key, &render_fn);

    // Render: cached paragraphs + fresh tail
    let fresh = incr.lines(render_key, &render_fn);

    // Store in the full block cache with wrapped height.
    // For streaming messages this will be invalidated on the next chunk,
    // but for completed messages it persists.
    let h = {
        let _t = crate::perf::start_with("msg::wrap_height", "lines", fresh.len());
        Paragraph::new(Text::from(fresh.clone())).wrap(Wrap { trim: false }).line_count(width)
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
    render_text_cached(&mut block.cache, &mut block.markdown, width, bg, preserve_newlines, out);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ChatMessage, MessageBlock, NoticeBlock, TextBlock, TextBlockSpacing};
    use pretty_assertions::assert_eq;
    use ratatui::widgets::{Paragraph, Wrap};

    // preprocess_markdown

    #[test]
    fn preprocess_h1_heading() {
        let result = preprocess_markdown("# Hello");
        assert!(result.contains("**Hello**"));
        assert!(!result.contains('#'));
    }

    #[test]
    fn preprocess_h3_heading() {
        let result = preprocess_markdown("### Deeply Nested");
        assert!(result.contains("**Deeply Nested**"));
    }

    #[test]
    fn preprocess_non_heading_passthrough() {
        let input = "Just normal text\nwith multiple lines";
        let result = preprocess_markdown(input);
        assert_eq!(result, input);
    }

    #[test]
    fn preprocess_mixed_headings_and_text() {
        let input = "# Title\nSome text\n## Subtitle\nMore text";
        let result = preprocess_markdown(input);
        assert!(result.contains("**Title**"));
        assert!(result.contains("Some text"));
        assert!(result.contains("**Subtitle**"));
        assert!(result.contains("More text"));
    }

    #[test]
    fn preprocess_heading_no_space() {
        let result = preprocess_markdown("#Title");
        assert!(result.contains("**Title**"));
    }

    #[test]
    fn preprocess_heading_extra_spaces() {
        let result = preprocess_markdown("#   Spaced Out   ");
        assert!(result.contains("**Spaced Out**"));
    }

    #[test]
    fn preprocess_indented_heading() {
        let result = preprocess_markdown("  ## Indented");
        assert!(result.contains("**Indented**"));
    }

    #[test]
    fn preprocess_empty_heading() {
        let result = preprocess_markdown("# ");
        assert_eq!(result, "# ");
    }

    #[test]
    fn preprocess_empty_string() {
        assert_eq!(preprocess_markdown(""), "");
    }

    #[test]
    fn preprocess_preserves_trailing_newline() {
        let result = preprocess_markdown("hello\n");
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn preprocess_no_trailing_newline() {
        let result = preprocess_markdown("hello");
        assert!(!result.ends_with('\n'));
    }

    #[test]
    fn preprocess_blank_line_before_heading() {
        let input = "text\n\n# Heading";
        let result = preprocess_markdown(input);
        assert!(!result.contains("\n\n\n"));
        assert!(result.contains("**Heading**"));
    }

    #[test]
    fn preprocess_consecutive_headings() {
        let input = "# First\n# Second";
        let result = preprocess_markdown(input);
        assert!(result.contains("**First**"));
        assert!(result.contains("**Second**"));
    }

    #[test]
    fn preprocess_hash_in_code_not_heading() {
        let result = preprocess_markdown("# actual heading");
        assert!(result.contains("**actual heading**"));
    }

    /// H6 heading (6 `#` chars).
    #[test]
    fn preprocess_h6_heading() {
        let result = preprocess_markdown("###### Deep H6");
        assert!(result.contains("**Deep H6**"));
        assert!(!result.contains('#'));
    }

    /// Heading with markdown formatting inside.
    #[test]
    fn preprocess_heading_with_bold_inside() {
        let result = preprocess_markdown("# **bold** and *italic*");
        assert!(result.contains("****bold** and *italic***"));
    }

    /// Heading at end of file with no trailing newline.
    #[test]
    fn preprocess_heading_at_eof_no_newline() {
        let result = preprocess_markdown("text\n# Final");
        assert!(result.contains("**Final**"));
        assert!(!result.ends_with('\n'));
    }

    /// Only hashes with no text: `###` - content after stripping is empty, passthrough.
    #[test]
    fn preprocess_only_hashes() {
        let result = preprocess_markdown("###");
        assert_eq!(result, "###");
    }

    /// Very long heading.
    #[test]
    fn preprocess_very_long_heading() {
        let long_text = "A".repeat(1000);
        let input = format!("# {long_text}");
        let result = preprocess_markdown(&input);
        assert!(result.starts_with("**"));
        assert!(result.contains(&long_text));
    }

    /// Unicode emoji in heading.
    #[test]
    fn preprocess_unicode_heading() {
        let result = preprocess_markdown("# \u{1F680} Launch \u{4F60}\u{597D}");
        assert!(result.contains("**\u{1F680} Launch \u{4F60}\u{597D}**"));
    }

    /// Quoted heading: `> # Heading` - starts with `>` not `#`, so passthrough.
    #[test]
    fn preprocess_blockquote_heading_passthrough() {
        let result = preprocess_markdown("> # Quoted heading");
        // Line starts with `>`, not `#`, so trimmed starts with `>` not `#`
        assert!(!result.contains("**"));
        assert!(result.contains("> # Quoted heading"));
    }

    /// All heading levels in sequence.
    #[test]
    fn preprocess_all_heading_levels() {
        let input = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6";
        let result = preprocess_markdown(input);
        for label in ["H1", "H2", "H3", "H4", "H5", "H6"] {
            assert!(result.contains(&format!("**{label}**")), "missing {label}");
        }
    }

    #[test]
    fn welcome_lines_render_expected_fields() {
        // Pass a non-empty subscription value so the account line
        // renders. Empty value would hide the line — see
        // `welcome_lines_skip_account_line_when_value_empty`.
        let message = ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "Pro", "/cwd", "-");
        let MessageBlock::Welcome(block) = &message.blocks[0] else {
            panic!("expected welcome block");
        };
        let rendered = welcome_lines(block, 120);
        let lines: Vec<String> = rendered
            .into_iter()
            .map(|line| line.spans.into_iter().map(|s| s.content).collect())
            .collect();
        assert!(lines.iter().any(|line| line.contains("Version:")));
        assert!(lines.iter().any(|line| line.contains("Subscription:") && line.contains("Pro")));
        assert!(lines.iter().any(|line| line.contains("cwd:          /cwd")));
        assert!(lines.iter().any(|line| line.contains("Session ID:   -")));
        assert!(lines.iter().any(|line| line.contains("Tips: ")));
        assert!(
            WELCOME_TIPS.iter().any(|tip| lines.iter().any(|line| line.contains(tip))),
            "expected one welcome tip to be rendered"
        );
    }

    #[test]
    fn welcome_lines_skip_account_line_when_value_empty() {
        // Empty subscription value means no data has loaded yet
        // (workspace picker still in flight or no workspace at
        // all). The renderer hides the line entirely — better than
        // showing a "-" placeholder that flickers when the real
        // value lands.
        let message = ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "", "/cwd", "-");
        let MessageBlock::Welcome(block) = &message.blocks[0] else {
            panic!("expected welcome block");
        };
        let rendered = welcome_lines(block, 120);
        let lines: Vec<String> = rendered
            .into_iter()
            .map(|line| line.spans.into_iter().map(|s| s.content).collect())
            .collect();
        assert!(lines.iter().any(|line| line.contains("Version:")));
        assert!(
            !lines.iter().any(|line| line.contains("Subscription") || line.contains("Account:")),
            "account/subscription line must not render when value is empty, got: {lines:?}"
        );
        assert!(lines.iter().any(|line| line.contains("cwd:          /cwd")));
    }

    // force_markdown_line_breaks

    #[test]
    fn force_breaks_adds_trailing_spaces() {
        let result = force_markdown_line_breaks("line1\nline2");
        assert!(result.contains("line1  \n"));
        assert!(result.contains("line2  "));
    }

    #[test]
    fn force_breaks_preserves_trailing_newline() {
        let result = force_markdown_line_breaks("hello\n");
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn force_breaks_empty_lines_no_trailing_spaces() {
        let result = force_markdown_line_breaks("a\n\nb");
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].ends_with("  "));
        assert_eq!(lines[1], "");
        assert!(lines[2].ends_with("  "));
    }

    #[test]
    fn force_breaks_single_line_no_trailing_newline() {
        let result = force_markdown_line_breaks("hello");
        assert_eq!(result, "hello  ");
    }

    #[test]
    fn force_breaks_many_consecutive_empty_lines() {
        let result = force_markdown_line_breaks("a\n\n\nb");
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines.len(), 4);
    }

    /// Empty input.
    #[test]
    fn force_breaks_empty_input() {
        let result = force_markdown_line_breaks("");
        assert_eq!(result, "");
    }

    /// Only empty lines.
    #[test]
    fn force_breaks_only_empty_lines() {
        let result = force_markdown_line_breaks("\n\n\n");
        let lines: Vec<&str> = result.lines().collect();
        // All lines are empty, so no trailing spaces added
        for line in &lines {
            assert!(line.is_empty(), "empty line got content: {line:?}");
        }
    }

    /// Line already ending with two spaces - gets two more.
    #[test]
    fn force_breaks_already_has_trailing_spaces() {
        let result = force_markdown_line_breaks("hello  \nworld");
        // "hello  " + "  " = "hello    "
        assert!(result.starts_with("hello    "));
    }

    /// Single newline (no content).
    #[test]
    fn force_breaks_single_newline() {
        let result = force_markdown_line_breaks("\n");
        // One empty line, should stay empty with trailing newline
        assert_eq!(result, "\n");
    }

    fn make_text_message(role: MessageRole, text: &str) -> ChatMessage {
        ChatMessage::new(role, vec![MessageBlock::Text(TextBlock::from_complete(text))], None)
    }

    fn make_assistant_split_message(first: &str, second: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::Assistant,
            vec![
                MessageBlock::Text(
                    TextBlock::from_complete(first)
                        .with_trailing_spacing(TextBlockSpacing::ParagraphBreak),
                ),
                MessageBlock::Text(TextBlock::from_complete(second)),
            ],
            None,
        )
    }

    fn make_assistant_notice_message() -> ChatMessage {
        ChatMessage::new(
            MessageRole::Assistant,
            vec![
                MessageBlock::Text(TextBlock::from_complete("Before notice")),
                MessageBlock::Notice(NoticeBlock::from_complete(
                    SystemSeverity::Warning,
                    "Warning inline",
                )),
                MessageBlock::Text(TextBlock::from_complete("After notice")),
            ],
            None,
        )
    }

    fn make_tool_call_info(
        id: &str,
        sdk_tool_name: &str,
        status: crate::agent::model::ToolCallStatus,
        text: &str,
    ) -> crate::app::ToolCallInfo {
        crate::app::ToolCallInfo {
            id: id.to_owned(),
            title: id.to_owned(),
            sdk_tool_name: sdk_tool_name.to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status,
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![crate::agent::model::ToolCallContent::from(text.to_owned())]
            },
            hidden: false,
            terminal_id: None,
            terminal_command: None,
            terminal_output: None,
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: crate::app::TerminalSnapshotMode::AppendOnly,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
        }
    }

    fn render_lines_to_strings(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    fn make_welcome_message(subscription: &str, cwd: &str, session_id: &str) -> ChatMessage {
        let mut message =
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), subscription, cwd, session_id);
        let Some(MessageBlock::Welcome(block)) = message.blocks.first_mut() else {
            panic!("expected welcome block");
        };
        block.tip_seed = 0;
        message
    }

    fn idle_spinner() -> SpinnerState {
        SpinnerState {
            frame: 0,
            is_active_turn_assistant: false,
            show_empty_thinking: false,
            show_thinking: false,
            show_compacting: false,
        }
    }

    fn default_options() -> MessageRenderOptions {
        MessageRenderOptions {
            tools_collapsed: false,
            include_trailing_separator: true,
            suppress_group_header: false,
            envelope_streak_position: None,
        }
    }

    fn options_without_separator() -> MessageRenderOptions {
        MessageRenderOptions {
            tools_collapsed: false,
            include_trailing_separator: false,
            suppress_group_header: false,
            envelope_streak_position: None,
        }
    }

    fn ground_truth_height(msg: &mut ChatMessage, spinner: &SpinnerState, width: u16) -> usize {
        let mut lines = Vec::new();
        render_message(
            msg,
            spinner,
            MessageRenderContext::new(None, width, 0, default_options()),
            &mut lines,
        );
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).line_count(width)
    }

    #[test]
    fn measure_height_matches_ground_truth_for_long_soft_wrap() {
        let text = "A".repeat(500);
        let spinner = idle_spinner();

        let mut measured_msg = make_text_message(MessageRole::User, &text);
        let mut truth_msg = make_text_message(MessageRole::User, &text);

        let (h, _) = measure_message_height_cached(&mut measured_msg, &spinner, 32, 1);
        let truth = ground_truth_height(&mut truth_msg, &spinner, 32);

        assert_eq!(h, truth);
    }

    #[test]
    fn user_role_label_wrap_height_matches_ground_truth() {
        let spinner = idle_spinner();
        let mut measured_msg = make_text_message(MessageRole::User, "ok");
        let mut truth_msg = make_text_message(MessageRole::User, "ok");

        let (h, _) = measure_message_height_cached(&mut measured_msg, &spinner, 2, 1);
        let truth = ground_truth_height(&mut truth_msg, &spinner, 2);

        assert_eq!(h, truth);
        assert!(h >= 3);
    }

    #[test]
    fn system_role_label_wrap_height_matches_ground_truth() {
        let spinner = idle_spinner();
        let mut measured_msg =
            make_text_message(MessageRole::System(Some(SystemSeverity::Warning)), "rate limit");
        let mut truth_msg =
            make_text_message(MessageRole::System(Some(SystemSeverity::Warning)), "rate limit");

        let (h, _) = measure_message_height_cached(&mut measured_msg, &spinner, 4, 1);
        let truth = ground_truth_height(&mut truth_msg, &spinner, 4);

        assert_eq!(h, truth);
        assert!(h >= 4);
    }

    #[test]
    fn welcome_role_label_wrap_height_matches_ground_truth() {
        let spinner = idle_spinner();
        let mut measured_msg = make_welcome_message("Max", "~/project", "session-1");
        let mut truth_msg = make_welcome_message("Max", "~/project", "session-1");

        let (h, _) = measure_message_height_cached(&mut measured_msg, &spinner, 4, 1);
        let truth = ground_truth_height(&mut truth_msg, &spinner, 4);

        assert_eq!(h, truth);
    }

    #[test]
    fn assistant_split_paragraph_inserts_a_structural_blank_line_between_blocks() {
        let spinner = idle_spinner();
        let mut msg = make_assistant_split_message("First paragraph", "Second paragraph");
        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, default_options()),
            &mut lines,
        );

        let rendered = render_lines_to_strings(&lines);
        let first_idx =
            rendered.iter().position(|line| line.contains("First paragraph")).expect("first block");
        let second_idx = rendered
            .iter()
            .position(|line| line.contains("Second paragraph"))
            .expect("second block");

        assert_eq!(rendered.first().map(String::as_str), Some("Forge"));
        assert!(second_idx > first_idx + 1);
        assert!(rendered[first_idx + 1].is_empty());
    }

    #[test]
    fn assistant_notice_block_renders_inline_between_neighboring_text_blocks() {
        let spinner = idle_spinner();
        let mut msg = make_assistant_notice_message();
        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, default_options()),
            &mut lines,
        );

        let rendered = render_lines_to_strings(&lines);
        let before_idx =
            rendered.iter().position(|line| line.contains("Before notice")).expect("before text");
        let notice_idx =
            rendered.iter().position(|line| line.contains("Warning inline")).expect("notice");
        let after_idx =
            rendered.iter().position(|line| line.contains("After notice")).expect("after text");

        assert_eq!(rendered.first().map(String::as_str), Some("Forge"));
        assert!(before_idx < notice_idx && notice_idx < after_idx);
    }

    #[test]
    fn assistant_notice_block_is_tinted_by_severity() {
        let spinner = idle_spinner();
        let mut msg = make_assistant_notice_message();
        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, default_options()),
            &mut lines,
        );

        let notice_line = lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content == "Warning inline"))
            .expect("expected notice line");
        assert!(
            notice_line
                .spans
                .iter()
                .filter(|span| !span.content.is_empty())
                .all(|span| span.style.fg == Some(theme::STATUS_WARNING))
        );
    }

    #[test]
    fn assistant_notice_height_matches_ground_truth() {
        let spinner = idle_spinner();
        let mut measured_msg = make_assistant_notice_message();
        let mut truth_msg = make_assistant_notice_message();

        let (h, _) = measure_message_height_cached(&mut measured_msg, &spinner, 16, 1);
        let truth = ground_truth_height(&mut truth_msg, &spinner, 16);

        assert_eq!(h, truth);
    }

    #[test]
    fn assistant_split_paragraph_height_matches_rendered_gap() {
        let spinner = idle_spinner();
        let mut measured = make_assistant_split_message("First paragraph", "Second paragraph");
        let mut truth = make_assistant_split_message("First paragraph", "Second paragraph");

        let (h, _) = measure_message_height_cached(&mut measured, &spinner, 80, 1);
        let truth_h = ground_truth_height(&mut truth, &spinner, 80);
        assert_eq!(h, truth_h);
        assert_eq!(h, 5);
    }

    #[test]
    fn assistant_message_can_render_without_trailing_separator() {
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "hello");
        let mut lines = Vec::new();

        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, options_without_separator()),
            &mut lines,
        );

        assert_eq!(render_lines_to_strings(&lines), vec!["Forge".to_owned(), "hello".to_owned()]);

        let (h, _) = measure_message_height_cached_with_tools_collapsed_and_separator(
            &mut msg, &spinner, 80, 1, false, false,
        );
        assert_eq!(h, 2);
    }

    #[test]
    fn empty_last_assistant_thinking_omits_trailing_separator() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_empty_thinking: true,
            ..idle_spinner()
        };
        let mut msg = ChatMessage::new(MessageRole::Assistant, Vec::new(), None);
        let mut lines = Vec::new();

        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, options_without_separator()),
            &mut lines,
        );

        let rendered = render_lines_to_strings(&lines);
        assert_eq!(rendered.len(), 2);
        assert_eq!(rendered[0], "Forge");
        assert!(rendered[1].contains("Thinking..."));

        let (h, _) = measure_message_height_cached_with_tools_collapsed_and_separator(
            &mut msg, &spinner, 80, 1, false, false,
        );
        assert_eq!(h, 2);
    }

    #[test]
    fn empty_last_assistant_thinking_wrap_height_matches_ground_truth() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_empty_thinking: true,
            ..idle_spinner()
        };
        let mut measured_msg = ChatMessage::new(MessageRole::Assistant, Vec::new(), None);
        let mut truth_msg = ChatMessage::new(MessageRole::Assistant, Vec::new(), None);

        let (h, _) = measure_message_height_cached_with_tools_collapsed_and_separator(
            &mut measured_msg,
            &spinner,
            6,
            1,
            false,
            false,
        );
        let mut truth_lines = Vec::new();
        render_message(
            &mut truth_msg,
            &spinner,
            MessageRenderContext::new(None, 6, 0, options_without_separator()),
            &mut truth_lines,
        );
        let truth =
            Paragraph::new(Text::from(truth_lines)).wrap(Wrap { trim: false }).line_count(6);

        assert_eq!(h, truth);
        assert!(h > 2);
    }

    #[test]
    fn empty_last_assistant_compacting_omits_trailing_separator() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_compacting: true,
            ..idle_spinner()
        };
        let mut msg = ChatMessage::new(MessageRole::Assistant, Vec::new(), None);
        let mut lines = Vec::new();

        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, options_without_separator()),
            &mut lines,
        );

        let rendered = render_lines_to_strings(&lines);
        assert_eq!(rendered.len(), 2);
        assert_eq!(rendered[0], "Forge");
        assert!(rendered[1].contains("Compacting context..."));

        let (h, _) = measure_message_height_cached_with_tools_collapsed_and_separator(
            &mut msg, &spinner, 80, 1, false, false,
        );
        assert_eq!(h, 2);
    }

    #[test]
    fn empty_last_assistant_thinking_offset_render_omits_trailing_separator() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_empty_thinking: true,
            ..idle_spinner()
        };
        let mut msg = ChatMessage::new(MessageRole::Assistant, Vec::new(), None);
        let mut out = Vec::new();

        let remaining = render_message_from_offset_internal(
            &mut msg,
            &spinner,
            80,
            1,
            MessageRenderOptions {
                tools_collapsed: false,
                include_trailing_separator: false,
                suppress_group_header: false,
                envelope_streak_position: None,
            },
            0,
            &mut out,
        );

        assert_eq!(remaining, 0);
        let rendered = render_lines_to_strings(&out);
        assert_eq!(rendered.first().map(String::as_str), Some("Forge"));
        assert!(rendered[1].contains("Thinking..."));
        assert!(!rendered.last().is_some_and(String::is_empty));
    }

    #[test]
    fn empty_last_assistant_compacting_offset_render_omits_trailing_separator() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_compacting: true,
            ..idle_spinner()
        };
        let mut msg = ChatMessage::new(MessageRole::Assistant, Vec::new(), None);
        let mut out = Vec::new();

        let remaining = render_message_from_offset_internal(
            &mut msg,
            &spinner,
            80,
            1,
            MessageRenderOptions {
                tools_collapsed: false,
                include_trailing_separator: false,
                suppress_group_header: false,
                envelope_streak_position: None,
            },
            0,
            &mut out,
        );

        assert_eq!(remaining, 0);
        let rendered = render_lines_to_strings(&out);
        assert_eq!(rendered.first().map(String::as_str), Some("Forge"));
        assert!(rendered[1].contains("Compacting context..."));
        assert!(!rendered.last().is_some_and(String::is_empty));
    }

    #[test]
    fn render_from_offset_handles_paragraph_gap_as_structural_rows() {
        let spinner = idle_spinner();
        let mut msg = make_assistant_split_message("First paragraph", "Second paragraph");
        let mut out = Vec::new();

        let remaining = render_message_from_offset(&mut msg, &spinner, 80, 1, 2, &mut out);

        assert_eq!(remaining, 0);
        let rendered = render_lines_to_strings(&out);
        assert_eq!(rendered.first().map(String::as_str), Some(""));
        assert!(rendered.iter().any(|line| line.contains("Second paragraph")));
        assert_eq!(rendered.last().map(String::as_str), Some(""));
    }

    #[test]
    fn measure_height_matches_ground_truth_after_resize() {
        let text =
            "This is a single very long line without explicit line breaks to stress soft wrapping."
                .repeat(20);
        let spinner = idle_spinner();

        let mut measured_msg = make_text_message(MessageRole::Assistant, &text);
        let mut truth_wide = make_text_message(MessageRole::Assistant, &text);
        let mut truth_narrow = make_text_message(MessageRole::Assistant, &text);

        let (h_wide, _) = measure_message_height_cached(&mut measured_msg, &spinner, 100, 1);
        let wide_truth = ground_truth_height(&mut truth_wide, &spinner, 100);
        assert_eq!(h_wide, wide_truth);

        // Reuse the same message to hit width-mismatch cache path.
        let (h_narrow, _) = measure_message_height_cached(&mut measured_msg, &spinner, 28, 2);
        let narrow_truth = ground_truth_height(&mut truth_narrow, &spinner, 28);
        assert_eq!(h_narrow, narrow_truth);
    }

    #[test]
    fn markdown_table_rerenders_when_width_changes_in_both_directions() {
        let spinner = idle_spinner();
        let table = concat!(
            "| Name | Description |\n",
            "| --- | --- |\n",
            "| foo | long wrapped value |\n",
        );
        let mut msg = make_text_message(MessageRole::Assistant, table);

        let mut wide_lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 40, 1, default_options()),
            &mut wide_lines,
        );
        let wide_rendered = render_lines_to_strings(&wide_lines);
        assert!(wide_rendered.iter().any(|line| line.contains("Name")));
        assert!(wide_rendered.iter().any(|line| line.contains('─')));
        assert!(!wide_rendered.iter().any(|line| line.contains("Name:")));

        let mut narrow_lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 12, 2, default_options()),
            &mut narrow_lines,
        );
        let narrow_rendered = render_lines_to_strings(&narrow_lines);
        assert!(narrow_rendered.iter().any(|line| line.contains("Name:")));
        assert!(narrow_rendered.iter().any(|line| line.contains("Description")));
        assert!(!narrow_rendered.iter().any(|line| line.contains('─')));

        let mut wide_again_lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 40, 3, default_options()),
            &mut wide_again_lines,
        );
        let wide_again_rendered = render_lines_to_strings(&wide_again_lines);
        assert!(wide_again_rendered.iter().any(|line| line.contains("Name")));
        assert!(wide_again_rendered.iter().any(|line| line.contains('─')));
        assert!(!wide_again_rendered.iter().any(|line| line.contains("Name:")));
    }

    #[test]
    fn render_from_offset_can_skip_entire_message() {
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::User, "hello\nworld");
        let mut truth_msg = make_text_message(MessageRole::User, "hello\nworld");
        let total = ground_truth_height(&mut truth_msg, &spinner, 120);

        let mut out = Vec::new();
        let rem = render_message_from_offset(&mut msg, &spinner, 120, 1, total + 3, &mut out);

        assert!(out.is_empty());
        assert_eq!(rem, 3);
    }

    #[test]
    fn render_cached_lines_from_offset_consumes_skip_across_cached_lines() {
        let skip = usize::from(u16::MAX) + 5;
        let lines =
            (0..skip + 3).map(|idx| Line::from(format!("line {idx:05}"))).collect::<Vec<_>>();
        let mut out = Vec::new();
        let mut remaining = skip;
        let mut can_consume_skip = true;

        render_cached_lines_from_offset(
            &lines,
            40,
            &mut out,
            &mut remaining,
            &mut can_consume_skip,
        );

        assert_eq!(remaining, 0);
        assert!(!can_consume_skip);
        assert_eq!(
            render_lines_to_strings(&out),
            vec![
                format!("line {skip:05}"),
                format!("line {:05}", skip + 1),
                format!("line {:05}", skip + 2),
            ]
        );
    }

    #[test]
    fn welcome_height_matches_ground_truth() {
        let spinner = idle_spinner();
        let mut measured_msg = make_welcome_message("Max", "~/project", "session-1");
        let mut truth_msg = make_welcome_message("Max", "~/project", "session-1");

        let (h, _) = measure_message_height_cached(&mut measured_msg, &spinner, 52, 1);
        let truth = ground_truth_height(&mut truth_msg, &spinner, 52);
        assert_eq!(h, truth);
    }

    #[test]
    fn system_warning_severity_renders_warning_label() {
        let spinner = idle_spinner();
        let mut msg = make_text_message(
            MessageRole::System(Some(SystemSeverity::Warning)),
            "Rate limit warning",
        );
        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );
        let rendered = render_lines_to_strings(&lines);

        assert!(rendered.iter().any(|line| line.contains("Warning")));
        assert!(rendered.iter().any(|line| line.contains("Rate limit warning")));
    }

    #[test]
    fn assistant_message_suppresses_hidden_subagent_child_tools() {
        let spinner = idle_spinner();

        for tools_collapsed in [false, true] {
            let mut hidden_tool = make_tool_call_info(
                "hidden-child",
                "Bash",
                crate::agent::model::ToolCallStatus::Completed,
                "child output",
            );
            hidden_tool.hidden = true;
            let mut msg = ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(hidden_tool))],
                None,
            );

            let mut lines = Vec::new();
            render_message(
                &mut msg,
                &spinner,
                MessageRenderContext::new(
                    None,
                    120,
                    0,
                    MessageRenderOptions {
                        tools_collapsed,
                        include_trailing_separator: true,
                        suppress_group_header: false,
                        envelope_streak_position: None,
                    },
                ),
                &mut lines,
            );
            let rendered = render_lines_to_strings(&lines);

            assert!(!rendered.iter().any(|line| line.contains("hidden-child")));
            assert!(!rendered.iter().any(|line| line.contains("child output")));
        }
    }

    #[test]
    fn assistant_heading_at_start_does_not_render_blank_line_after_label() {
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "\n# Heading\nBody");

        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, default_options()),
            &mut lines,
        );
        let rendered = render_lines_to_strings(&lines);

        assert_eq!(rendered.first().map(String::as_str), Some("Forge"));
        let heading_idx =
            rendered.iter().position(|line| line.contains("Heading")).expect("heading");
        assert_eq!(heading_idx, 1);
        assert!(!rendered[heading_idx].is_empty());
    }

    #[test]
    fn assistant_heading_at_start_height_matches_rendered_output() {
        let spinner = idle_spinner();
        let mut measured = make_text_message(MessageRole::Assistant, "\n# Heading\nBody");
        let mut truth = make_text_message(MessageRole::Assistant, "\n# Heading\nBody");

        let (h, _) = measure_message_height_cached(&mut measured, &spinner, 80, 1);
        let truth_h = ground_truth_height(&mut truth, &spinner, 80);

        assert_eq!(h, truth_h);
    }

    #[test]
    fn assistant_heading_at_start_offset_render_omits_leading_blank_row() {
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "\n# Heading\nBody");
        let mut out = Vec::new();

        let remaining = render_message_from_offset(&mut msg, &spinner, 80, 1, 0, &mut out);
        let rendered = render_lines_to_strings(&out);

        assert_eq!(remaining, 0);
        assert_eq!(rendered.first().map(String::as_str), Some("Forge"));
        let heading_idx =
            rendered.iter().position(|line| line.contains("Heading")).expect("heading");
        assert_eq!(heading_idx, 1);
        assert!(!rendered[heading_idx].is_empty());
    }

    #[test]
    fn assistant_message_does_not_show_empty_turn_thinking_after_content_exists() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_empty_thinking: true,
            ..idle_spinner()
        };
        let mut msg = make_text_message(MessageRole::Assistant, "done");

        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );
        let rendered = render_lines_to_strings(&lines);

        assert!(!rendered.iter().any(|line| line.contains("Thinking...")));
    }

    #[test]
    fn assistant_message_suppresses_thinking_line_while_compacting() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_thinking: true,
            show_compacting: true,
            ..idle_spinner()
        };
        let mut msg = make_text_message(MessageRole::Assistant, "done");

        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );
        let rendered = render_lines_to_strings(&lines);

        assert!(rendered.iter().any(|line| line.contains("Compacting context...")));
        assert!(!rendered.iter().any(|line| line.contains("Thinking...")));
    }

    #[test]
    fn assistant_offset_render_suppresses_thinking_line_while_compacting() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_thinking: true,
            show_compacting: true,
            ..idle_spinner()
        };
        let mut msg = make_text_message(MessageRole::Assistant, "done");

        let mut lines = Vec::new();
        let remaining = render_message_from_offset(&mut msg, &spinner, 120, 1, 0, &mut lines);
        let rendered = render_lines_to_strings(&lines);

        assert_eq!(remaining, 0);
        assert!(rendered.iter().any(|line| line.contains("Compacting context...")));
        assert!(!rendered.iter().any(|line| line.contains("Thinking...")));
    }

    #[test]
    fn message_render_cache_reuses_segments_for_repeated_render_with_same_inputs() {
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "cached");
        let options = default_options();

        let cache = get_or_build_message_render_cache(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 1, options),
        );
        let first_segments = cache.segments().to_vec();
        let first_height = cache.height();

        let cache = get_or_build_message_render_cache(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 1, options),
        );
        assert_eq!(cache.segments().len(), first_segments.len());
        assert_eq!(cache.height(), first_height);
        assert_eq!(cache.height(), rendered_segment_height(&first_segments));
    }

    #[test]
    fn message_render_cache_rebuilds_when_indicator_visibility_changes() {
        let mut msg = make_text_message(MessageRole::Assistant, "cached");
        let base_spinner = idle_spinner();
        let thinking_spinner = SpinnerState { show_thinking: true, frame: 1, ..idle_spinner() };
        let options = default_options();

        let base_cache = get_or_build_message_render_cache(
            &mut msg,
            &base_spinner,
            MessageRenderContext::new(None, 80, 1, options),
        );
        let base_height = base_cache.height();
        let base_segments = base_cache.segments().to_vec();

        let thinking_cache = get_or_build_message_render_cache(
            &mut msg,
            &thinking_spinner,
            MessageRenderContext::new(None, 80, 1, options),
        );
        assert!(thinking_cache.height() >= base_height);
        assert!(
            thinking_cache.height() != base_height
                || thinking_cache.segments().len() != base_segments.len()
        );
    }

    #[test]
    fn message_render_cache_rebuilds_when_trailing_separator_visibility_changes() {
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "cached");
        let with_separator =
            MessageRenderOptions { include_trailing_separator: true, ..default_options() };
        let without_separator =
            MessageRenderOptions { include_trailing_separator: false, ..default_options() };

        let with_cache = get_or_build_message_render_cache(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 1, with_separator),
        );
        let with_height = with_cache.height();
        let with_segments = with_cache.segments().len();

        let without_cache = get_or_build_message_render_cache(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 1, without_separator),
        );
        assert!(without_cache.height() <= with_height);
        assert!(
            without_cache.height() != with_height
                || without_cache.segments().len() != with_segments
        );
    }

    #[test]
    fn message_render_cache_rebuilds_when_mode_changes_for_tool_calls() {
        let spinner = idle_spinner();
        let tool = make_tool_call_info(
            "Write notes/plan.md",
            "Write",
            crate::agent::model::ToolCallStatus::Completed,
            "created plan",
        );
        let mut msg = ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tool))],
            None,
        );
        let options = default_options();

        let code_cache = get_or_build_message_render_cache(
            &mut msg,
            &spinner,
            MessageRenderContext::new(Some("code"), 80, 1, options),
        );
        let code_lines: Vec<String> = code_cache
            .segments()
            .iter()
            .flat_map(|segment| match segment {
                CachedMessageSegment::Blank => vec![String::new()],
                CachedMessageSegment::Lines { lines, .. } => lines
                    .iter()
                    .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
                    .collect(),
            })
            .collect();
        assert!(code_lines.iter().any(|line| line.contains("Write notes/plan.md")));

        let plan_cache = get_or_build_message_render_cache(
            &mut msg,
            &spinner,
            MessageRenderContext::new(Some("plan"), 80, 1, options),
        );
        let plan_lines: Vec<String> = plan_cache
            .segments()
            .iter()
            .flat_map(|segment| match segment {
                CachedMessageSegment::Blank => vec![String::new()],
                CachedMessageSegment::Lines { lines, .. } => lines
                    .iter()
                    .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
                    .collect(),
            })
            .collect();
        assert!(plan_lines.iter().any(|line| line.contains("Create Plan")));
        assert!(!plan_lines.iter().any(|line| line.contains("Write notes/plan.md")));
    }

    fn rendered_segment_height(segments: &[CachedMessageSegment]) -> usize {
        segments
            .iter()
            .map(|segment| match segment {
                CachedMessageSegment::Blank => 1,
                CachedMessageSegment::Lines { height, .. } => *height,
            })
            .sum()
    }

    // -----------------------------------------------------------------
    // Same-project envelope grouping (#158). Tests cover the
    // grouping-decision helper + the render-time suppression flow.
    // -----------------------------------------------------------------

    /// Build a User-role message whose first text block is a peer
    /// envelope so `detect_inbound` + `message_envelope_org` recognise
    /// it. Reuses the existing `peer_block` to_prose shape so the test
    /// doesn't have to hand-roll the bracket format.
    fn make_peer_envelope_message(sender: &str, org: &str, body: &str) -> ChatMessage {
        let text = format!(
            "[Message id=t-12345678 hop=1/10 from agent '{sender}' (org '{org}')]\n\n{body}"
        );
        ChatMessage::new_peer_envelope(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete(&text))],
            None,
        )
    }

    #[test]
    fn compute_suppress_group_header_is_false_for_first_message() {
        let messages = vec![make_peer_envelope_message("forge", "Personal", "hi")];
        assert!(!compute_suppress_group_header(&messages, 0));
    }

    #[test]
    fn compute_suppress_group_header_is_false_for_non_envelope() {
        let messages = vec![
            make_peer_envelope_message("forge", "Personal", "first"),
            make_text_message(MessageRole::User, "plain user text"),
        ];
        assert!(!compute_suppress_group_header(&messages, 1));
    }

    #[test]
    fn compute_suppress_group_header_is_false_when_prev_is_not_envelope() {
        let messages = vec![
            make_text_message(MessageRole::User, "first user input"),
            make_peer_envelope_message("forge", "Personal", "envelope"),
        ];
        assert!(!compute_suppress_group_header(&messages, 1));
    }

    #[test]
    fn compute_suppress_group_header_is_true_for_consecutive_same_org() {
        let messages = vec![
            make_peer_envelope_message("forge", "Personal", "first"),
            make_peer_envelope_message("forge", "Personal", "second"),
            make_peer_envelope_message("forge", "Personal", "third"),
        ];
        assert!(!compute_suppress_group_header(&messages, 0));
        assert!(compute_suppress_group_header(&messages, 1));
        assert!(compute_suppress_group_header(&messages, 2));
    }

    #[test]
    fn compute_suppress_group_header_is_false_when_org_differs() {
        let messages = vec![
            make_peer_envelope_message("forge", "Personal", "lead msg"),
            make_peer_envelope_message("reviewer", "worker in forge", "worker msg"),
            make_peer_envelope_message("forge", "Personal", "lead again"),
        ];
        // (1) is an envelope with a different org from (0) → no suppress
        assert!(!compute_suppress_group_header(&messages, 1));
        // (2) is an envelope with a different org from (1) → no suppress
        assert!(!compute_suppress_group_header(&messages, 2));
    }

    // -----------------------------------------------------------------
    // #163: envelope streak position helpers.
    // -----------------------------------------------------------------

    fn make_envelope_msg(sender: &str, org: &str, body: &str) -> ChatMessage {
        let text = format!(
            "[Message id=t-12345678 hop=1/10 from agent '{sender}' (org '{org}')]\n\n{body}"
        );
        ChatMessage::new_peer_envelope(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete(&text))],
            None,
        )
    }

    #[test]
    fn compute_envelope_streak_position_first_envelope_is_start() {
        let messages = vec![make_envelope_msg("planner", "worker in forge", "first")];
        assert_eq!(
            compute_envelope_streak_position(&messages, 0),
            Some(EnvelopeStreakPosition::Start),
        );
    }

    #[test]
    fn compute_envelope_streak_position_after_non_envelope_is_start() {
        let messages = vec![
            make_text_message(MessageRole::User, "plain user text"),
            make_envelope_msg("planner", "worker in forge", "envelope"),
        ];
        assert_eq!(
            compute_envelope_streak_position(&messages, 1),
            Some(EnvelopeStreakPosition::Start),
        );
    }

    #[test]
    fn compute_envelope_streak_position_different_worker_same_project_is_follower_new_worker() {
        let messages = vec![
            make_envelope_msg("planner", "worker in forge", "first"),
            make_envelope_msg("implementer", "worker in forge", "second"),
        ];
        assert_eq!(
            compute_envelope_streak_position(&messages, 1),
            Some(EnvelopeStreakPosition::FollowerNewWorker),
        );
    }

    #[test]
    fn compute_envelope_streak_position_same_worker_is_follower_same_worker() {
        let messages = vec![
            make_envelope_msg("planner", "worker in forge", "first"),
            make_envelope_msg("planner", "worker in forge", "second"),
        ];
        assert_eq!(
            compute_envelope_streak_position(&messages, 1),
            Some(EnvelopeStreakPosition::FollowerSameWorker),
        );
    }

    #[test]
    fn compute_envelope_streak_position_different_project_is_start() {
        let messages = vec![
            make_envelope_msg("forge", "Personal", "first"),
            make_envelope_msg("granite-backend", "Granite", "second"),
        ];
        assert_eq!(
            compute_envelope_streak_position(&messages, 1),
            Some(EnvelopeStreakPosition::Start),
        );
    }

    #[test]
    fn compute_envelope_streak_position_non_envelope_returns_none() {
        let messages = vec![make_text_message(MessageRole::User, "plain user text")];
        assert_eq!(compute_envelope_streak_position(&messages, 0), None);
    }

    #[test]
    fn compute_suppress_group_header_breaks_on_assistant_turn() {
        let messages = vec![
            make_peer_envelope_message("forge", "Personal", "first envelope"),
            make_text_message(MessageRole::Assistant, "assistant reply"),
            make_peer_envelope_message("forge", "Personal", "second envelope"),
        ];
        // Non-User assistant turn between envelopes breaks the group.
        assert!(!compute_suppress_group_header(&messages, 2));
    }

    /// Render-time effect: with `suppress_group_header = true`, the
    /// envelope render does NOT include the `Forge` role label line.
    /// With it false, the label IS present. Same envelope text, only
    /// the flag changes.
    #[test]
    fn render_envelope_with_suppress_group_header_omits_role_label() {
        let mut msg = make_peer_envelope_message("forge", "Personal", "hello");
        let spinner = idle_spinner();
        let options_with_label = MessageRenderOptions {
            tools_collapsed: false,
            include_trailing_separator: false,
            suppress_group_header: false,
            envelope_streak_position: None,
        };
        let mut lines_with = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, options_with_label),
            &mut lines_with,
        );
        let with_label = render_lines_to_strings(&lines_with);
        // Invalidate the cache between the two render calls so the
        // second one rebuilds against the new options (the cache key
        // already distinguishes the two, but we want to be explicit).
        msg.invalidate_render_cache();
        let options_no_label = MessageRenderOptions {
            tools_collapsed: false,
            include_trailing_separator: false,
            suppress_group_header: true,
            envelope_streak_position: None,
        };
        let mut lines_without = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 80, 0, options_no_label),
            &mut lines_without,
        );
        let without_label = render_lines_to_strings(&lines_without);

        assert_eq!(with_label.first().map(String::as_str), Some("Forge"));
        assert_ne!(without_label.first().map(String::as_str), Some("Forge"));
        assert!(lines_without.len() < lines_with.len(), "suppressing the label drops one line");
    }

    /// Build an Assistant message that carries a peer-outbound
    /// `mcp__forge__workers__tell` tool_use card. Mirrors what the
    /// worker emits when its LLM calls `workers__tell(target, msg)`.
    fn make_assistant_with_workers_tell(target: &str, body: &str) -> ChatMessage {
        let mut tc = make_tool_call_info(
            "tc-tell",
            "mcp__forge__workers__tell",
            crate::agent::model::ToolCallStatus::Completed,
            "",
        );
        tc.raw_input = Some(serde_json::json!({ "label": target, "message": body }));
        ChatMessage::new(MessageRole::Assistant, vec![MessageBlock::ToolCall(Box::new(tc))], None)
    }

    #[test]
    fn compute_suppress_group_header_folds_across_assistant_peer_outbound() {
        // A worker's chat interleaves inbound envelopes (from lead)
        // with Assistant turns carrying the worker's own outbound
        // `workers__tell` / `workers__ask` tool_use cards. The
        // streak should fold under one `Forge` header rather than
        // re-printing the role label on every other row.
        let messages = vec![
            make_peer_envelope_message("forge", "Personal", "first inbound"),
            make_assistant_with_workers_tell("lead", "outbound to lead"),
            make_peer_envelope_message("forge", "Personal", "second inbound"),
        ];
        assert!(compute_suppress_group_header(&messages, 1));
        assert!(compute_suppress_group_header(&messages, 2));
    }
}
