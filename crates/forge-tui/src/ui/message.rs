use crate::app::{
    BlockCache, CachedMessageSegment, ChatMessage, IncrementalMarkdown, MarkdownRenderKey,
    MessageBlock, MessageRenderCache, MessageRenderCacheKey, MessageRenderSignature, MessageRole,
    StopHookEntry, SystemSeverity, TextBlock, WelcomeBlock, hash_text_block_content,
    hash_welcome_block_content,
};
use crate::ui::peer_block;
use crate::ui::theme;
use crate::ui::tool_call;

pub mod grouping;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

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
#[derive(Clone)]
pub struct SpinnerState {
    /// Current spinner glyph for the active style, resolved once per
    /// frame from `App::active_spinner_glyph`.
    pub glyph: char,
    /// True when this message owns the currently active assistant turn.
    pub is_active_turn_assistant: bool,
    /// True when this message should show the initial empty-turn thinking indicator.
    pub show_empty_thinking: bool,
    /// True when this message should show the thinking indicator.
    pub show_thinking: bool,
    /// True when this message should show the compaction indicator.
    pub show_compacting: bool,
    /// #273: Latest cumulative thinking-token count for the current
    /// turn. `None` when no `Message::ThinkingTokens` event has fired
    /// yet; the spinner falls back to bare `Thinking...`.
    pub thinking_tokens: Option<u64>,
    /// One-line chat indicator for the session waiting on >=1
    /// non-terminal `SubagentRoot`. `Some` whenever
    /// `App::subagents_view` is non-empty for the active session;
    /// `None` when no subagent is active. ADDITIVE to
    /// `show_thinking` (both lines render together when both apply).
    pub running_subagents: Option<RunningSubagentsLine>,
}

/// Snapshot of the active-subagent set surfaced by the chat
/// running-subagents indicator. Carries the total count + the
/// primary entry's label so [`subagent_running_line`] can format the
/// single / multi-subagent shape without re-reading session state.
#[derive(Clone)]
pub struct RunningSubagentsLine {
    pub count: usize,
    pub primary_label: Option<String>,
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
    /// #273: Hooks list for the active `Message::StopHookSummary` chip
    /// bound to this message. Empty slice when no summary applies.
    /// Lives on the context rather than `MessageRenderOptions` because
    /// `MessageRenderOptions` must stay `Copy + Default` for the cache
    /// key plumbing; the slice already inherits the `'a` lifetime here.
    stop_hook_summary_hooks: &'a [StopHookEntry],
    /// Per-group collapse-level overrides for the chat tool-call
    /// grouping feature. `None` (default) means every group renders at
    /// `GroupCollapseLevel::L2Summary`. Lives on the context so the
    /// cache signature folds it (a level flip invalidates the
    /// corresponding message's render) and the dispatch path can
    /// branch L2 / L1 / L0 without a separate `App` borrow.
    group_collapse_levels:
        Option<&'a std::collections::HashMap<grouping::GroupId, grouping::GroupCollapseLevel>>,
    /// Per-messaging-group collapse-level overrides, sibling of
    /// `group_collapse_levels` keyed on the messaging-group's
    /// `group_leader_id`. `None` (default) means every messaging
    /// group renders at L2 summary when the global directive is
    /// collapsed, L0 when expanded (via `resolve_group_level`).
    messaging_group_collapse_levels:
        Option<&'a std::collections::HashMap<grouping::GroupId, grouping::GroupCollapseLevel>>,
    /// Pre-computed render-unit list for THIS message produced by
    /// the session-walking partitioner. `Some` carries cross-message
    /// peer/worker run state - segments may have continuation flags
    /// and a shared `group_leader_id` with segments in sibling
    /// messages. `None` falls back to the per-message partition,
    /// which only sees within-message runs.
    session_message_units: Option<&'a [grouping::RenderUnit]>,
    /// Active session's raw cwd (project root), used to relativize the
    /// read-kind file paths in the chat tool-group L2 tree. `None`
    /// (default / empty cwd) renders read paths as-is (absolute).
    project_root: Option<&'a str>,
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
            stop_hook_summary_hooks: &[],
            group_collapse_levels: None,
            messaging_group_collapse_levels: None,
            session_message_units: None,
            project_root: None,
        }
    }

    /// Attach the active session's project root (raw cwd) so the chat
    /// tool-group tree can relativize read-kind file paths. An empty
    /// root leaves paths absolute.
    pub(crate) fn with_project_root(mut self, root: &'a str) -> Self {
        self.project_root = (!root.is_empty()).then_some(root);
        self
    }

    /// Attach a pre-computed render-unit list for THIS message
    /// (sliced out of `partition_session_into_render_units`'s output
    /// over the full session). Required for cross-message peer/worker
    /// run merging to fire; absent falls back to the per-message
    /// partition.
    pub(crate) fn with_session_message_units(mut self, units: &'a [grouping::RenderUnit]) -> Self {
        self.session_message_units = Some(units);
        self
    }

    /// Attach the active session's per-messaging-group collapse
    /// levels so the messaging-group dispatch sees L1/L0 overrides
    /// and the cache signature folds them. Default (no call) keeps
    /// every messaging group at its global-directive default.
    pub(crate) fn with_messaging_group_collapse_levels(
        mut self,
        levels: &'a std::collections::HashMap<grouping::GroupId, grouping::GroupCollapseLevel>,
    ) -> Self {
        self.messaging_group_collapse_levels = Some(levels);
        self
    }

    fn messaging_group_level(&self, id: &grouping::GroupId) -> grouping::GroupCollapseLevel {
        let per_group = self.messaging_group_collapse_levels.and_then(|m| m.get(id).copied());
        crate::ui::collapse::resolve_group_level(per_group, self.options.tools_collapsed)
    }

    /// Attach the active session's per-group collapse levels so the
    /// chat tool-call grouping dispatch sees L1/L0 overrides and the
    /// cache signature folds them. Default (no call) keeps every group
    /// at L2 summary.
    pub(crate) fn with_group_collapse_levels(
        mut self,
        levels: &'a std::collections::HashMap<grouping::GroupId, grouping::GroupCollapseLevel>,
    ) -> Self {
        self.group_collapse_levels = Some(levels);
        self
    }

    fn group_level(&self, id: &grouping::GroupId) -> grouping::GroupCollapseLevel {
        let per_group = self.group_collapse_levels.and_then(|m| m.get(id).copied());
        crate::ui::collapse::resolve_group_level(per_group, self.options.tools_collapsed)
    }

    /// #273: Attach a hooks list for the stop_hook_summary chip
    /// renderer. Caller is responsible for setting
    /// `options.stop_hook_summary_actions > 0` to actually surface
    /// the chip; an attached slice with `actions == 0` renders
    /// nothing.
    pub(crate) fn with_stop_hook_hooks(mut self, hooks: &'a [StopHookEntry]) -> Self {
        self.stop_hook_summary_hooks = hooks;
        self
    }
}

/// Format a milliseconds duration for the expanded `stop_hook_summary`
/// rows (`append_stop_hook_summary`). Buckets:
///   - `< 60_000` ms -> one-decimal seconds (`12.4s`).
///   - `60_000..3_600_000` -> integer `Xm Ys` (`1m 04s`).
///   - `>= 3_600_000` -> `Xh Ym Zs` (`1h 02m 04s`).
pub fn format_turn_duration(ms: u64) -> String {
    const SEC: u64 = 1_000;
    const MIN: u64 = 60 * SEC;
    const HOUR: u64 = 60 * MIN;
    if ms < MIN {
        // One decimal, e.g. 12_400 ms -> "12.4s".
        let whole = ms / SEC;
        let tenths = (ms % SEC) / 100;
        return format!("{whole}.{tenths}s");
    }
    if ms < HOUR {
        let minutes = ms / MIN;
        let seconds = (ms % MIN) / SEC;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = ms / HOUR;
    let minutes = (ms % HOUR) / MIN;
    let seconds = (ms % MIN) / SEC;
    format!("{hours}h {minutes:02}m {seconds:02}s")
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

/// True when an empty-blocks Assistant/System message would render only a
/// bare "Forge"/"Info" role label with no body.
fn renders_bare_role_label_only(
    msg: &ChatMessage,
    spinner: &SpinnerState,
    render_context: &MessageRenderContext<'_>,
) -> bool {
    if !msg.blocks.is_empty()
        || !matches!(msg.role, MessageRole::Assistant | MessageRole::System(_))
    {
        return false;
    }
    if spinner.show_empty_thinking
        || spinner.show_compacting
        || spinner.show_thinking
        || (spinner.running_subagents.is_some() && spinner.is_active_turn_assistant)
    {
        return false;
    }
    render_context.options.stop_hook_summary_actions == 0
}

fn build_message_layout(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
) -> MessageLayout {
    let mut layout = MessageLayout::new();
    if renders_bare_role_label_only(msg, spinner, &render_context) {
        return layout;
    }
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
            // #273: stop_hook_summary chip sits between the
            // assistant body and the trailing separator so the
            // visual order reads `body → hook summary chip →
            // (separator)`. Hidden when `actions == 0`.
            if render_context.options.stop_hook_summary_actions > 0 {
                let (chip_y, chip_h) = append_stop_hook_summary(
                    render_context.options.stop_hook_summary_actions,
                    render_context.options.stop_hook_summary_expanded,
                    render_context.stop_hook_summary_hooks,
                    render_context.width,
                    &mut layout,
                );
                msg.stop_hook_summary_y_in_msg = chip_y;
                msg.stop_hook_summary_height = chip_h;
            } else {
                msg.stop_hook_summary_y_in_msg = 0;
                msg.stop_hook_summary_height = 0;
            }
        }
        MessageRole::System(_) => append_system_blocks(msg, render_context.width, &mut layout),
    }

    if render_context.options.include_trailing_separator {
        layout.push_blank();
    }

    layout
}

/// #273: Render a `Message::StopHookSummary` as a collapsed
/// `↳ hook summary · N actions [▶ expand]` chip. When `expanded`,
/// follow with one DIM indented `command · duration` row per hook.
/// Caller already gated on `actions > 0` so this function assumes
/// the chip is wanted. Returns `(chip_y_in_msg, chip_height)` - the
/// wrapped-row offset and height of the clickable chip line(s),
/// excluding the leading blank and any expanded hook rows. Caller
/// stamps these on the `ChatMessage` so the mouse handler can route
/// clicks back to the toggle.
fn append_stop_hook_summary(
    actions: u32,
    expanded: bool,
    hooks: &[StopHookEntry],
    width: u16,
    layout: &mut MessageLayout,
) -> (usize, usize) {
    layout.push_blank();
    let chip_y = layout.height;
    let toggle_label = if expanded { "[▼ collapse]" } else { "[▶ expand]" };
    let action_word = if actions == 1 { "action" } else { "actions" };
    let chip = Line::from(vec![
        Span::styled(
            format!("↳ hook summary · {actions} {action_word} "),
            Style::default().fg(theme::DIM),
        ),
        Span::styled(toggle_label.to_owned(), Style::default().fg(theme::DIM)),
    ]);
    layout.push_wrapped_line(chip, width);
    let chip_height = layout.height.saturating_sub(chip_y);
    if expanded {
        for hook in hooks {
            let body = Line::from(Span::styled(
                format!("    {} · {}", hook.command, format_turn_duration(hook.duration_ms)),
                Style::default().fg(theme::DIM),
            ));
            layout.push_wrapped_line(body, width);
        }
    }
    (chip_y, chip_height)
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
                // Peer-coordination wrappers (#114) - when the
                // workspace injects a `[Question id=...]` /
                // `[Reply id=...]` / etc. user-turn, render a styled
                // peer block instead of the default user bubble.
                // Inbound peer blocks follow the global collapse
                // directive via `resolve_collapsed_bool`. Per-block
                // click override wins; absent falls through to
                // `tools_collapsed`.
                if let Some(kind) = peer_block::detect_inbound(&block.text) {
                    let trailing_gap = block.trailing_blank_lines();
                    let collapsed = crate::ui::collapse::resolve_collapsed_bool(
                        block.peer_collapsed_override,
                        tools_collapsed,
                    );
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
        layout.push_wrapped_line(compacting_line(spinner.glyph), render_context.width);
        return;
    }
    if msg.blocks.is_empty() && spinner.show_empty_thinking {
        layout.push_wrapped_line(
            thinking_line(spinner.glyph, spinner.thinking_tokens),
            render_context.width,
        );
        return;
    }

    let show_compacting = spinner.show_compacting;
    let mut state = AssistantLayoutState::default();
    // Session-walking partition wins when attached: it carries
    // cross-message peer/worker run state (shared group_leader_id,
    // continuation flags, group_total_count). Absent (no
    // `with_session_message_units` call) falls back to the
    // per-message partition, which only sees within-message runs.
    let owned_units: Vec<grouping::RenderUnit> =
        if let Some(slice) = render_context.session_message_units {
            slice.to_vec()
        } else {
            grouping::partition_blocks_into_render_units(&msg.blocks)
        };
    for unit in owned_units.iter().cloned() {
        match unit {
            grouping::RenderUnit::Individual(idx) => {
                append_assistant_block(
                    &mut msg.blocks[idx],
                    spinner,
                    render_context,
                    layout,
                    &mut state,
                );
            }
            grouping::RenderUnit::Group { range, leader_id, summary, aggregate_status } => {
                match render_context.group_level(&leader_id) {
                    grouping::GroupCollapseLevel::L2Summary => {
                        if state.has_body_content {
                            layout.push_blank();
                        }
                        // Stamp the leader's hit-test fields so a click
                        // on the summary line maps to `range.start` via
                        // the existing `locate_tool_call_block_at_click`
                        // walk. The mouse handler reclassifies via
                        // `grouping::group_leader_at` + level check
                        // and dispatches as a group-summary click when
                        // the position matches a group at L2.
                        let summary_lines = tool_call::render_group_summary_line(
                            &summary,
                            aggregate_status,
                            spinner.glyph,
                            render_context.width as usize,
                            render_context.project_root,
                        );
                        let y_in_msg = layout.height;
                        let height = rendered_lines_height(&summary_lines, render_context.width);
                        layout.push_wrapped_lines(summary_lines, render_context.width);
                        if let Some(MessageBlock::ToolCall(tc)) = msg.blocks.get_mut(range.start) {
                            tc.last_measured_y_in_msg = y_in_msg;
                            tc.last_measured_height = height;
                            tc.last_measured_width = render_context.width;
                        }
                        state.has_body_content = true;
                        state.has_visible_content = true;
                        state.prev_was_tool = true;
                    }
                    level @ (grouping::GroupCollapseLevel::L1Titles
                    | grouping::GroupCollapseLevel::L0Bodies) => {
                        let mut group_ctx = render_context;
                        group_ctx.options.tools_collapsed =
                            matches!(level, grouping::GroupCollapseLevel::L1Titles);
                        for idx in range {
                            append_assistant_block(
                                &mut msg.blocks[idx],
                                spinner,
                                group_ctx,
                                layout,
                                &mut state,
                            );
                        }
                    }
                }
            }
            grouping::RenderUnit::MessagingGroup { segments, group_leader_id } => {
                let level = render_context.messaging_group_level(&group_leader_id);
                match level {
                    grouping::GroupCollapseLevel::L2Summary => {
                        if state.has_body_content {
                            layout.push_blank();
                        }
                        for segment in &segments {
                            let summary_lines = peer_block::render_messaging_group_summary_line(
                                segment,
                                spinner.glyph,
                            );
                            // Stamp the leading peer-class block's
                            // hit-test fields so a click on the
                            // summary line maps back to the segment's
                            // first block via the existing
                            // `locate_tool_call_block_at_click` walk.
                            let y_in_msg = layout.height;
                            let height =
                                rendered_lines_height(&summary_lines, render_context.width);
                            layout.push_wrapped_lines(summary_lines, render_context.width);
                            if let Some(MessageBlock::ToolCall(tc)) =
                                msg.blocks.get_mut(segment.block_range.start)
                            {
                                tc.last_measured_y_in_msg = y_in_msg;
                                tc.last_measured_height = height;
                                tc.last_measured_width = render_context.width;
                            }
                            state.has_body_content = true;
                            state.has_visible_content = true;
                            state.prev_was_tool = true;
                        }
                    }
                    sub_level @ (grouping::GroupCollapseLevel::L1Titles
                    | grouping::GroupCollapseLevel::L0Bodies) => {
                        let mut group_ctx = render_context;
                        group_ctx.options.tools_collapsed =
                            matches!(sub_level, grouping::GroupCollapseLevel::L1Titles);
                        for segment in segments {
                            for idx in segment.block_range {
                                append_assistant_block(
                                    &mut msg.blocks[idx],
                                    spinner,
                                    group_ctx,
                                    layout,
                                    &mut state,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    if show_compacting {
        if state.has_body_content {
            layout.push_blank();
        }
        layout.push_wrapped_line(compacting_line(spinner.glyph), render_context.width);
    }
    if spinner.show_thinking && !show_compacting {
        if state.has_body_content {
            layout.push_blank();
        }
        layout.push_wrapped_line(
            thinking_line(spinner.glyph, spinner.thinking_tokens),
            render_context.width,
        );
    }
    // Additive to the thinking line: both can render simultaneously
    // when the assistant is mid-stream AND a subagent is non-terminal.
    if let Some(running) = spinner.running_subagents.as_ref()
        && !show_compacting
        && spinner.is_active_turn_assistant
    {
        if state.has_body_content || spinner.show_thinking {
            layout.push_blank();
        }
        layout.push_wrapped_line(
            subagent_running_line(spinner.glyph, running.count, running.primary_label.as_deref()),
            render_context.width,
        );
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
    // Answered AskUserQuestion: once the user responds, the dock prompt
    // is gone and the tool renders as a compact question -> answer card.
    // While unanswered it stays hidden (returns above), so this only
    // fires post-answer.
    if let Some(lines) = render_question_answered_card(tc) {
        if !state.prev_was_tool && state.has_body_content {
            layout.push_blank();
        }
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
    // Peer-coordination outbound (#114) - replace the default
    // tool_use card for `mcp__forge__peers__ask_agent` /
    // `peers__tell_agent` with a styled peer block in the same
    // tool-card shape (status icon + kind label + tree body).
    // Collapse state follows the standard tool-call rule: per-tc
    // `collapsed_override` wins, otherwise the global default.
    // Click-to-toggle on peer rows currently piggybacks on the
    // existing tool-call row hit-test in mouse.rs.
    // collapse Monitor + Workflow tool cards to a
    // single DIM one-liner. These tool calls carry their detail in
    // the Inspector MONITORS / WORKFLOWS sections; the chat surface
    // only needs the start/stop signal. Falls through to the
    // standard tool card when the raw_input is missing or malformed.
    if let Some(lines) = render_lifecycle_one_liner(tc) {
        if !state.prev_was_tool && state.has_body_content {
            layout.push_blank();
        }
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
    if let Some(kind) = peer_block::detect_outbound(tc) {
        if !state.prev_was_tool && state.has_body_content {
            layout.push_blank();
        }
        // Outbound peer-tool blocks (peers__* / workers__*) follow
        // the global collapse directive via the unified
        // `resolve_collapsed_bool`. Per-block click override wins;
        // absent falls through to `tools_collapsed`. The invariant:
        // every render-time collapsed-decision routes through a
        // resolver in `crate::ui::collapse`; no inline
        // `unwrap_or(<arbitrary>)`.
        let collapsed = crate::ui::collapse::resolve_collapsed_bool(
            tc.collapsed_override,
            render_context.options.tools_collapsed,
        );
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
        spinner.glyph,
        render_context.options.tools_collapsed,
        &mut lines,
    );
    let (height, wrapped_lines) = tool_call::measure_tool_call_height_cached_with_tools_collapsed(
        tc,
        render_context.tool_render_context,
        render_context.width,
        spinner.glyph,
        render_context.layout_generation,
        render_context.options.tools_collapsed,
    );
    // Capture the tool's wrapped-row offset within this message *after*
    // any leading blank from the prev-was-tool/has-body-content gap so
    // mouse hit-testing can locate the rendered row range directly
    // from the tool's own state - no need to walk text-block heights
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

/// Render the post-answer card for an answered AskUserQuestion. Returns
/// `None` while the question is unanswered (the dock prompt is the live
/// surface) or for any non-question tool. Each answered pair renders as
/// a `? <question>` line then an indented answer line; a typed "Other"
/// answer surfaces the literal text the user entered.
fn render_question_answered_card(tc: &crate::app::ToolCallInfo) -> Option<Vec<Line<'static>>> {
    if !tc.is_ask_question_tool() || tc.answered_questions.is_empty() {
        return None;
    }
    // Indent the question line 2 spaces so the `?` lands in the
    // tool-icon column (matching `standard::render_tool_call_title`'s
    // `format!("  {icon} ")` convention) and nest the answer line(s)
    // one level deeper so the `->` sits at column 4, under the
    // question text.
    let mut lines: Vec<Line<'static>> = Vec::new();
    for qa in &tc.answered_questions {
        lines.push(Line::from(vec![
            Span::styled(
                "  ? ".to_owned(),
                Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(qa.question.clone(), Style::default().fg(theme::DIM)),
        ]));
        if !qa.picked_labels.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("    \u{2192} ".to_owned(), Style::default().fg(theme::DIM)),
                Span::styled(qa.picked_labels.join(", "), Style::default().fg(Color::Green)),
            ]));
        }
        if let Some(typed) = qa.typed_note.as_ref().filter(|s| !s.is_empty()) {
            lines.push(Line::from(vec![
                Span::styled("    \u{2192} ".to_owned(), Style::default().fg(theme::DIM)),
                Span::styled("you typed: ".to_owned(), Style::default().fg(theme::DIM)),
                Span::styled(format!("\"{typed}\""), Style::default().add_modifier(Modifier::BOLD)),
            ]));
        }
    }
    Some(lines)
}

/// #273 Tasks 8 + 9: collapse Monitor / Workflow tool_use cards to a
/// single DIM one-liner. Returns the rendered lines when the tool is
/// Monitor or Workflow AND has a parseable input; otherwise `None`
/// (caller falls through to the standard tool-card render).
///
/// Render shapes:
/// - Monitor (running): `◉ Monitor started · <description> (persistent)`
/// - Monitor (running, non-persistent): `◉ Monitor started · <description>`
/// - Monitor (terminal): `◉ Monitor stopped · <description>` (or
///   `· timed out` when killed via timeout)
/// - Workflow (running): `◆ Workflow started · <meta.name | "Workflow">`
/// - Workflow (terminal): `◆ Workflow done · <meta.name | "Workflow">`
fn render_lifecycle_one_liner(tc: &crate::app::ToolCallInfo) -> Option<Vec<Line<'static>>> {
    use forge_primitives::ToolCallStatus;
    match tc.sdk_tool_name.as_str() {
        "Monitor" => {
            let parsed = tc
                .raw_input
                .as_ref()
                .and_then(forge_workspace::user_interaction::parse_monitor_input)?;
            let is_terminal = matches!(
                tc.status,
                ToolCallStatus::Completed | ToolCallStatus::Failed | ToolCallStatus::Killed
            );
            if is_terminal {
                // Collapsed one-liner: ✓ Monitor · <desc> · <status>
                // Per #277's wire mapping (sdk_message::handle_task_updated):
                // ToolCallStatus::Completed maps to MonitorStatus::Completed
                // ("completed"); ToolCallStatus::Killed maps to
                // MonitorStatus::Stopped ("stopped"). ToolCallStatus::Failed
                // would map to "timed out" if/when the wire produces it
                // (no current production path; kept for completeness).
                let status_word = match tc.status {
                    ToolCallStatus::Killed => "stopped",
                    ToolCallStatus::Failed => "timed out",
                    _ => "completed",
                };
                return Some(vec![Line::from(vec![
                    Span::styled(
                        format!("{} ", theme::ICON_COMPLETED),
                        Style::default().fg(Color::Green),
                    ),
                    Span::styled("Monitor".to_owned(), Style::default().fg(theme::DIM)),
                    Span::styled(
                        format!(" \u{b7} {} \u{b7} {status_word}", parsed.description),
                        Style::default().fg(theme::DIM),
                    ),
                ])]);
            }
            // Alive: header + $ command + last-5 tail tree.
            let suffix = if parsed.persistent { " \u{b7} persistent" } else { "" };
            let mut lines: Vec<Line<'static>> = Vec::new();
            // Header: ◉ Monitor · <desc> [· persistent]
            lines.push(Line::from(vec![
                Span::styled(
                    "\u{25c9} ".to_owned(),
                    Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
                ),
                Span::styled("Monitor".to_owned(), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(" \u{b7} {}{suffix}", parsed.description),
                    Style::default().fg(theme::DIM),
                ),
            ]));
            // Command line: │ $ <command>  (└ if tail empty, │ if tail follows).
            let cmd_connector =
                if tc.monitor_output_tail.is_empty() { "\u{2514} " } else { "\u{2502} " };
            lines.push(Line::from(Span::styled(
                format!("   {cmd_connector}$ {}", parsed.command),
                Style::default().fg(theme::DIM),
            )));
            // Tail lines: │ <line> ... └ <last line>
            let last_idx = tc.monitor_output_tail.len().saturating_sub(1);
            for (idx, line) in tc.monitor_output_tail.iter().enumerate() {
                let conn = if idx == last_idx { "\u{2514} " } else { "\u{2502} " };
                lines.push(Line::from(Span::styled(
                    format!("   {conn}{line}"),
                    Style::default().fg(theme::DIM),
                )));
            }
            Some(lines)
        }
        "Workflow" => {
            let parsed = tc
                .raw_input
                .as_ref()
                .and_then(forge_workspace::user_interaction::parse_workflow_input)?;
            let meta_name = workflow_meta_name(&parsed.script);
            let is_terminal = matches!(
                tc.status,
                ToolCallStatus::Completed | ToolCallStatus::Failed | ToolCallStatus::Killed
            );
            let text = if is_terminal {
                format!("\u{25c6} Workflow done \u{b7} {meta_name}")
            } else {
                format!("\u{25c6} Workflow started \u{b7} {meta_name}")
            };
            Some(vec![Line::from(Span::styled(text, Style::default().fg(theme::DIM)))])
        }
        _ => None,
    }
}

/// Extract the `name` field from a workflow `script`'s
/// `export const meta = { name: '...' }` block. Falls back to the
/// literal `"Workflow"` label when the block isn't present or
/// doesn't carry a name. Conservative substring-based parser
/// matches both single-quoted and double-quoted strings.
pub(crate) fn workflow_meta_name(script: &str) -> String {
    extract_meta_field(script, "name").unwrap_or_else(|| "Workflow".to_owned())
}

/// Extract both the `name` and `description` fields
/// from a workflow `script`'s meta block. Returns
/// `(name, description)` where `name` falls back to `"Workflow"`
/// when missing; `description` is `None` when the meta block lacks
/// it.
pub fn workflow_meta_fields(script: &str) -> (String, Option<String>) {
    let name = workflow_meta_name(script);
    let description = extract_meta_field(script, "description");
    (name, description)
}

/// Internal helper: find `<field>: '<value>'` or `<field>: "<value>"`
/// substring in the script body and return the unquoted value.
fn extract_meta_field(script: &str, field: &str) -> Option<String> {
    for prefix in [format!("{field}:"), format!("{field} :")] {
        let mut search_from = 0;
        while let Some(rel) = script[search_from..].find(&prefix) {
            let start = search_from + rel;
            // Reject matches that aren't at the start of a token
            // (e.g. `lastTooLname:` would match `name:` if we
            // didn't check). Token start = preceding char is
            // whitespace / `,` / `{` / newline / nothing.
            let preceding = script[..start].chars().next_back();
            let token_start =
                preceding.is_none_or(|c| c.is_whitespace() || c == ',' || c == '{' || c == ';');
            if !token_start {
                search_from = start + prefix.len();
                continue;
            }
            let after = &script[start + prefix.len()..];
            let trimmed = after.trim_start();
            let quote = trimmed.chars().next()?;
            if quote != '\'' && quote != '"' {
                search_from = start + prefix.len();
                continue;
            }
            let body = &trimmed[quote.len_utf8()..];
            let end = body.find(quote)?;
            let value = body[..end].trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
            search_from = start + prefix.len();
        }
    }
    None
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
            stop_hook_summary_actions: 0,
            stop_hook_summary_expanded: false,
        },
    )
}

/// Lowest-level measurement helper - accepts the full
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
    measure_message_height_cached_with_context(msg, spinner, render_context)
}

/// #273: Context-taking measurement helper. Callers that need to
/// thread state beyond `MessageRenderOptions` (today: the
/// stop_hook_summary hooks slice) build a `MessageRenderContext`
/// themselves and pass it in. Other callers use the simpler
/// `_with_options` variant which forwards an empty stop-hook slice.
pub(crate) fn measure_message_height_cached_with_context(
    msg: &mut ChatMessage,
    spinner: &SpinnerState,
    render_context: MessageRenderContext<'_>,
) -> (usize, usize) {
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
            stop_hook_summary_actions: 0,
            stop_hook_summary_expanded: false,
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
    /// #273: Action count from the `Message::StopHookSummary` bound
    /// to this message. `0` -> no chip rendered. Non-zero -> render
    /// the collapsed `↳ hook summary · N actions [▶ expand]` line at
    /// the end of the assistant turn. Folded into the cache key so a
    /// fresh summary event reliably invalidates the prior render.
    pub stop_hook_summary_actions: u32,
    /// #273: Toggle for the stop-hook-summary expanded body. When
    /// true and `stop_hook_summary_actions > 0`, the renderer also
    /// emits a DIM indented list of `command · duration` rows below
    /// the chip. Folded into the cache key so click-to-expand flips
    /// re-render cleanly.
    pub stop_hook_summary_expanded: bool,
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
        stop_hook_summary_actions: render_context.options.stop_hook_summary_actions,
        stop_hook_summary_expanded: render_context.options.stop_hook_summary_expanded,
        render_signature: build_message_render_signature(
            msg,
            spinner,
            render_context.tool_render_context,
            render_context.stop_hook_summary_hooks,
            render_context,
        ),
    }
}

fn build_message_render_signature(
    msg: &ChatMessage,
    spinner: &SpinnerState,
    tool_render_context: tool_call::ToolCallRenderContext<'_>,
    stop_hook_summary_hooks: &[StopHookEntry],
    render_context: MessageRenderContext<'_>,
) -> MessageRenderSignature {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    msg.role.hash(&mut hasher);
    msg.turn_duration_ms.hash(&mut hasher);
    spinner.show_empty_thinking.hash(&mut hasher);
    spinner.show_thinking.hash(&mut hasher);
    spinner.show_compacting.hash(&mut hasher);
    // Item 3's idle suppression and the running-subagents line both key off
    // these; fold them (line content included) so a flip invalidates the
    // cached layout.
    spinner.is_active_turn_assistant.hash(&mut hasher);
    spinner
        .running_subagents
        .as_ref()
        .map(|running| (running.count, running.primary_label.as_deref()))
        .hash(&mut hasher);
    let assistant_frame = if message_has_frame_dependent_assistant_lines(msg, spinner) {
        Some(spinner.glyph)
    } else {
        None
    };
    assistant_frame.hash(&mut hasher);
    for block in &msg.blocks {
        hash_message_block_into(&mut hasher, block, spinner, tool_render_context);
    }
    // #273: hook list contents drive the expanded body; fold them
    // into the signature so a fresh `StopHookSummary` with new hook
    // metadata invalidates the cached layout even when actions count
    // is unchanged.
    for hook in stop_hook_summary_hooks {
        hook.command.hash(&mut hasher);
        hook.duration_ms.hash(&mut hasher);
    }
    // Chat tool-call + messaging grouping: fold the level of each
    // group present in this message so a `cycle_*_collapse_level`
    // flip invalidates the cache. The session-walking partition
    // attached to the context (when present) folds cross-message
    // peer/worker continuation state too - segment counts, group
    // totals, leader ids - so a downstream turn's extension of an
    // in-flight messaging run invalidates this message's cache.
    // Read-kind paths render relative to the project root; fold it so a
    // cwd change (account switch / worktree) invalidates the cached
    // layout even when the blocks themselves are unchanged.
    render_context.project_root.hash(&mut hasher);
    let owned_units: Vec<grouping::RenderUnit> =
        if let Some(slice) = render_context.session_message_units {
            slice.to_vec()
        } else {
            grouping::partition_blocks_into_render_units(&msg.blocks)
        };
    for unit in &owned_units {
        match unit {
            grouping::RenderUnit::Group { leader_id, range, aggregate_status, .. } => {
                range.start.hash(&mut hasher);
                range.end.hash(&mut hasher);
                render_context.group_level(leader_id).hash(&mut hasher);
                aggregate_status.hash(&mut hasher);
            }
            grouping::RenderUnit::MessagingGroup { segments, group_leader_id } => {
                group_leader_id.as_str().hash(&mut hasher);
                render_context.messaging_group_level(group_leader_id).hash(&mut hasher);
                for segment in segments {
                    segment.msg_idx.hash(&mut hasher);
                    segment.block_range.start.hash(&mut hasher);
                    segment.block_range.end.hash(&mut hasher);
                    segment.segment_count.hash(&mut hasher);
                    segment.segment_continues_above.hash(&mut hasher);
                    segment.segment_continues_below.hash(&mut hasher);
                    segment.group_total_count.hash(&mut hasher);
                    segment.aggregate_status.hash(&mut hasher);
                    segment.segment_outbound_targets.targets.hash(&mut hasher);
                    segment.segment_outbound_targets.overflow_n.hash(&mut hasher);
                    segment.segment_inbound_targets.targets.hash(&mut hasher);
                    segment.segment_inbound_targets.overflow_n.hash(&mut hasher);
                }
            }
            grouping::RenderUnit::Individual(_) => {}
        }
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
            let frame = tool_call_needs_spinner_frame(tc).then_some(spinner.glyph);
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
            // turns), but they're agent-to-agent traffic - the chat
            // label "User" misrepresents them as human input.
            // Distinguish: real human input keeps the "User" label;
            // any User message whose first text block is a peer-
            // envelope bracket re-labels as `Forge` to match the
            // matching Assistant-side outbound label. Reserves the
            // "User" treatment for things actually typed by the
            // human at the prompt.
            if is_gotify_envelope_user_message(msg) {
                // An external Gotify notification, not agent traffic - a
                // distinct source label so it can't be mistaken for a
                // message the forge agent itself sent.
                Line::from(Span::styled(
                    "Gotify",
                    Style::default().fg(theme::GOTIFY).add_modifier(Modifier::BOLD),
                ))
            } else if is_cron_envelope_user_message(msg) {
                // A fired cron - a scheduled internal event, distinct from
                // typed input and from peer traffic.
                Line::from(Span::styled(
                    "Cron",
                    Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
                ))
            } else if is_peer_envelope_user_message(msg) {
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
        MessageRole::Assistant => {
            let mut spans = vec![Span::styled(
                "Forge",
                Style::default().fg(theme::ROLE_ASSISTANT).add_modifier(Modifier::BOLD),
            )];
            if let Some(ms) = msg.turn_duration_ms {
                spans.push(Span::styled(
                    format!(" \u{b7} {}", format_turn_duration(ms)),
                    Style::default().fg(theme::DIM),
                ));
            }
            Line::from(spans)
        }
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
/// The walk was a hot path under heavy envelope traffic - the role
/// label re-evaluates on every render of every chat message.
fn is_peer_envelope_user_message(msg: &ChatMessage) -> bool {
    msg.is_peer_envelope
}

/// True when this `MessageRole::User` carries a Gotify external
/// notification (`[Gotify - app '...']`). Reads the cached
/// `is_gotify_envelope` flag stamped at push time by the
/// `GotifyNotificationAppended` path, mirroring
/// [`is_peer_envelope_user_message`].
fn is_gotify_envelope_user_message(msg: &ChatMessage) -> bool {
    msg.is_gotify_envelope
}

/// True when this `MessageRole::User` carries a fired cron (`[Cron]`).
/// Reads the cached `is_cron_envelope` flag stamped at push time by the
/// `CronPromptAppended` path, mirroring [`is_gotify_envelope_user_message`].
fn is_cron_envelope_user_message(msg: &ChatMessage) -> bool {
    msg.is_cron_envelope
}

/// Extract the `sender_org` tag from this message's peer envelope,
/// if any. Drives the same-project envelope grouping at
/// `compute_suppress_group_header` (chat-iteration level).
///
/// Two envelope shapes count:
/// - **Inbound** (User role): a `[Question id=...]` / `[Message id=...]`
///   bracket whose `(org '...')` field is the wire-level sender_org.
/// - **Assistant peer-outbound**: an Assistant turn carrying a
///   `mcp__forge__peers__{ask,tell}_agent` /
///   `mcp__forge__workers__{ask,tell}` tool_use card. Synthesise the
///   lead's `PERSONAL_ORG` so the worker-chat case (inbound from
///   lead with `"Personal"` org, interleaved with outbound to lead)
///   folds under one header. The synthetic org is hard-coded rather
///   than derived from the call's target, so a cross-project peer
///   outbound to a non-Personal target won't fold against its own
///   surrounding inbound; that case is rare today and left to a
///   follow-up if it shows up in practice.
///
/// Returns `None` for everything else: plain user input, regular
/// assistant text, system notices, non-peer tool_use cards.
pub(crate) fn message_envelope_org(msg: &ChatMessage) -> Option<String> {
    use crate::ui::peer_block::{detect_inbound, detect_outbound};
    match msg.role {
        MessageRole::User => msg.blocks.iter().find_map(|block| match block {
            MessageBlock::Text(text) => {
                // Peer/worker envelopes group by sender_org; a Gotify
                // notification is an external event with no peer identity
                // (peer_sender_identity None), so it never folds under a
                // shared group header.
                let kind = detect_inbound(&text.text);
                kind.as_ref()
                    .filter(|k| k.peer_sender_identity().is_some())
                    .map(|k| k.org().to_owned())
            }
            _ => None,
        }),
        MessageRole::Assistant => msg.blocks.iter().find_map(|block| match block {
            MessageBlock::ToolCall(tc) => {
                detect_outbound(tc).map(|_| forge_workspace::PERSONAL_ORG.to_owned())
            }
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
    /// the worker label - body continues under the existing tag
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
        MessageBlock::Text(text) => detect_inbound(&text.text)
            .as_ref()
            .and_then(|kind| kind.peer_sender_identity().map(str::to_owned)),
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
        // different project - this envelope starts a fresh streak.
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

fn thinking_line(ch: char, thinking_tokens: Option<u64>) -> Line<'static> {
    // #273: when ThinkingTokens has fired for the current turn the
    // chip swaps from `Thinking...` to `thinking · N tok` (with k/M
    // abbreviation). Falls back to the bare `Thinking...` shape when
    // no count is available yet (Turn just started, mid-bootstrap).
    let body = match thinking_tokens {
        Some(n) => format!("{ch} thinking · {} tok", format_token_count_short(n)),
        None => format!("{ch} Thinking..."),
    };
    Line::from(Span::styled(body, Style::default().fg(theme::DIM)))
}

/// #273: Format a token count with k / M abbreviation. Threshold
/// rules:
///   - < 1_000 -> bare integer (`0`, `42`, `999`).
///   - 1_000..1_000_000 -> `Nk` or `N.Nk` (one decimal), e.g.
///     `1199 -> 1.1k`, `15_000 -> 15k`, `999_999 -> 999k`.
///   - >= 1_000_000 -> `NM` or `N.NM`, e.g. `1_500_000 -> 1.5M`.
///
/// The integer / one-decimal split matches the rendered chip width
/// (max 4 visible chars: `1.4M`, `999k`, `999`) so the spinner row
/// stays a stable size regardless of the turn's token volume.
pub fn format_token_count_short(n: u64) -> String {
    const K: u64 = 1_000;
    const M: u64 = 1_000_000;
    if n < K {
        return n.to_string();
    }
    if n < M {
        // < 10k -> one decimal (e.g. 1.2k, 9.9k); >= 10k -> integer
        // (e.g. 15k, 999k). Truncation via integer division keeps
        // the chip readable - 1199 reads as 1.1k not 1.2k.
        if n < 10 * K {
            let whole = n / K;
            let tenths = (n / (K / 10)) % 10;
            return format!("{whole}.{tenths}k");
        }
        return format!("{}k", n / K);
    }
    if n < 10 * M {
        let whole = n / M;
        let tenths = (n / (M / 10)) % 10;
        return format!("{whole}.{tenths}M");
    }
    format!("{}M", n / M)
}

/// One-line chat indicator for a session waiting on >=1 non-terminal
/// `SubagentRoot`. Subagents are Inspector-only, so without this line
/// the chat goes silent while a subagent runs. Sibling of
/// [`thinking_line`] / [`compacting_line`]; additive to `thinking_line`
/// (both render together when the assistant is mid-stream AND a
/// subagent is still going). Single shape:
/// `⠋ ◇ running subagent: <label>… (see Inspector)`; multi:
/// `⠋ ◇ running N subagents… (see Inspector)`. The label arg falls
/// back to the count form when absent.
fn subagent_running_line(spinner: char, count: usize, label: Option<&str>) -> Line<'static> {
    let body = match (count, label) {
        (n, _) if n > 1 => {
            format!("{spinner} \u{25c7} running {n} subagents\u{2026} (see Inspector)")
        }
        (_, Some(label)) if !label.is_empty() => {
            format!("{spinner} \u{25c7} running subagent: {label}\u{2026} (see Inspector)")
        }
        _ => format!("{spinner} \u{25c7} running subagent\u{2026} (see Inspector)"),
    };
    Line::from(Span::styled(body, Style::default().fg(theme::DIM)))
}

fn compacting_line(ch: char) -> Line<'static> {
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
    // Skip the line entirely when value is empty (no data yet) -
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
///
/// HTML tags outside fenced code blocks are stripped because
/// `tui_markdown::from_str` emits per-element WARN events for every
/// HTML element it encounters (peaks at 50K+/sec on streaming chats
/// with HTML content). `<br>` / `<br/>` / `<br />` become newlines
/// to preserve the author's line-break intent; other tags
/// (`<div>`, `<b>`, `<i>`, ...) drop the tag and keep the inner
/// content. Inside fenced code blocks (triple-backtick), HTML-like
/// text is preserved verbatim so Rust generics (`Vec<T>`), JSX, and
/// other code that LOOKS like HTML survives untouched.
fn preprocess_markdown(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            result.push_str(line);
            result.push('\n');
            continue;
        }
        if in_fence {
            result.push_str(line);
            result.push('\n');
            continue;
        }
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
                result.push_str(&strip_html_tags(content));
                result.push_str("**\n");
                continue;
            }
        }
        let stripped = strip_html_tags(line);
        result.push_str(&stripped);
        result.push('\n');
    }
    if !text.ends_with('\n') {
        result.pop();
    }
    result
}

/// Strip HTML tags from a single line. `<br>`, `<br/>`, `<br />` (and
/// trailing-attribute variants) become `\n` so the author's intended
/// line break still renders; other tags drop entirely and inner text
/// is preserved (`<div>foo</div>` -> `foo`, `<b>x</b>` -> `x`).
///
/// Conservative: a `<` is treated as a tag start only when followed
/// by an ASCII alphabetic character or `/` so `1 < 2` and `<<EOF`
/// stay literal. Unclosed `<...` (no `>` on the line) is preserved
/// verbatim. Inline backtick spans (single `\``) pass through
/// untouched so language generics in `Vec<T>` / `Map<K, V>` / JSX
/// (`<App />` shown as code) survive intact - the strip would
/// otherwise mistake them for HTML tags. Caller must already have
/// decided the line is OUTSIDE a fenced code block - this helper
/// does no triple-backtick fence tracking.
fn strip_html_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_backticks = false;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            in_backticks = !in_backticks;
            out.push('`');
            i += 1;
            continue;
        }
        if !in_backticks
            && bytes[i] == b'<'
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'/')
            && let Some(rel_close) = bytes[i + 1..].iter().position(|&b| b == b'>')
        {
            let close_idx = i + 1 + rel_close;
            let body = &line[i + 1..close_idx];
            let normalized = body.trim_start_matches('/').trim_end_matches('/').trim();
            let tag_name = normalized.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            if tag_name == "br" {
                out.push('\n');
            }
            // Any other recognised tag drops; inner content
            // between open + close already lies outside the
            // bracket pair on this line and is consumed by the
            // outer loop after we advance past the close.
            i = close_idx + 1;
            continue;
        }
        // Advance one char at a time so UTF-8 boundaries hold. The
        // outer `i < bytes.len()` guarantees `line[i..]` is non-empty
        // and yields at least one char.
        let Some(ch) = line[i..].chars().next() else { break };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
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
    fn preprocess_br_becomes_newline() {
        // `<br>` (and `<br/>`, `<br />`) renders today as a silent
        // gap because tui_markdown drops the tag. Strip it to `\n`
        // so the line break still appears in the rendered output.
        let result = preprocess_markdown("foo<br>bar");
        assert!(result.contains("foo\nbar"), "<br> must convert to newline, got: {result:?}");
        let result2 = preprocess_markdown("foo<br/>bar");
        assert!(result2.contains("foo\nbar"));
        let result3 = preprocess_markdown("foo<br />bar");
        assert!(result3.contains("foo\nbar"));
    }

    #[test]
    fn preprocess_block_html_drops_tag_keeps_content() {
        // `<div>foo</div>` becomes `foo` - tag silenced (no WARN
        // spam) and content preserved.
        let result = preprocess_markdown("<div>hello world</div>");
        assert!(result.contains("hello world"));
        assert!(!result.contains("<div>"));
        assert!(!result.contains("</div>"));
    }

    #[test]
    fn preprocess_inline_html_drops_tag_keeps_content() {
        // Inline `<b>...</b>` / `<i>...</i>` lose the tag but keep
        // the inner text. Markdown can re-bold via `**` if the
        // upstream prompt wants it; this layer doesn't translate.
        let result = preprocess_markdown("This is <b>bold</b> text");
        assert!(result.contains("This is bold text"), "got: {result:?}");
    }

    #[test]
    fn preprocess_preserves_html_inside_fenced_code() {
        // Rust generics, JSX, and other code that LOOKS like HTML
        // inside a triple-backtick block must survive untouched.
        // Otherwise we'd mangle `Vec<T>` -> `Vec` etc.
        let input = "```rust\nlet v: Vec<String> = vec![];\n```\n";
        let result = preprocess_markdown(input);
        assert!(
            result.contains("Vec<String>"),
            "code-fence content must preserve `<>`, got: {result:?}"
        );
        // And the fence markers themselves survive intact.
        assert!(result.contains("```rust"));
        assert!(result.contains("```\n"));
    }

    #[test]
    fn preprocess_preserves_html_inside_inline_backticks() {
        // Inline code spans (single backticks) carry the same
        // tag-shaped technical content as fenced blocks: `Vec<T>`,
        // `Map<K, V>`, `<App />`, `List<Integer>`, etc. Stripping
        // there mangles legitimate generics in chat output. Lock
        // the round-trip.
        let result = preprocess_markdown("The type is `Vec<T>` here.");
        assert!(
            result.contains("`Vec<T>`"),
            "single-backtick code must preserve `<>`, got: {result:?}"
        );
        let result2 = preprocess_markdown("JSX: `<App />` renders.");
        assert!(result2.contains("`<App />`"), "got: {result2:?}");
        let result3 = preprocess_markdown("Generic: `Map<K, V>` value.");
        assert!(result3.contains("`Map<K, V>`"), "got: {result3:?}");
    }

    #[test]
    fn preprocess_leaves_literal_lt_alone() {
        // `<` not followed by a tag character (alphabetic / `/`) is
        // preserved verbatim so `1 < 2` and `<<EOF` style stay
        // unmangled.
        let result = preprocess_markdown("if 1 < 2 then ok");
        assert!(result.contains("1 < 2"));
        let result2 = preprocess_markdown("here doc <<EOF");
        assert!(result2.contains("<<EOF"));
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
        // renders. Empty value would hide the line - see
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
        // all). The renderer hides the line entirely - better than
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
            monitor_output_tail: Vec::default(),
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    fn render_lines_to_strings(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn answered_question_renders_question_and_picked_label() {
        let mut tc = make_tool_call_info(
            "toolu_q",
            "AskUserQuestion",
            crate::agent::model::ToolCallStatus::Completed,
            "",
        );
        tc.answered_questions = vec![crate::app::AnsweredQuestion {
            question: "Which build path?".to_owned(),
            picked_labels: vec!["Clean answered-card".to_owned()],
            typed_note: None,
        }];
        let lines =
            render_question_answered_card(&tc).expect("answered AskUserQuestion produces lines");
        let joined = render_lines_to_strings(&lines).join("\n");
        assert!(joined.contains("Which build path?"), "question text: {joined:?}");
        assert!(joined.contains("Clean answered-card"), "picked label: {joined:?}");
        assert!(
            !joined.contains("you typed"),
            "picked-only card must not show the typed lead-in: {joined:?}",
        );
    }

    #[test]
    fn answered_question_typed_answer_surfaces_literal_text() {
        let mut tc = make_tool_call_info(
            "toolu_q",
            "AskUserQuestion",
            crate::agent::model::ToolCallStatus::Completed,
            "",
        );
        tc.answered_questions = vec![crate::app::AnsweredQuestion {
            question: "How should it look?".to_owned(),
            picked_labels: Vec::new(),
            typed_note: Some("Can you show me some visuals please?".to_owned()),
        }];
        let lines = render_question_answered_card(&tc).expect("typed answer produces lines");
        let joined = render_lines_to_strings(&lines).join("\n");
        assert!(
            joined.contains("Can you show me some visuals please?"),
            "literal typed text must be shown: {joined:?}",
        );
        assert!(joined.contains("you typed"), "typed lead-in must be shown: {joined:?}");
    }

    /// Fix 1 (data loss): the bug the plan targets. When the user
    /// picks one or more options in a multiSelect AND types text into
    /// the "Other" free-text, the card MUST surface BOTH on their
    /// own answer lines. The previous shape (`answer: String` +
    /// `typed: bool`) collapsed to either-or and dropped the typed
    /// text whenever picks were non-empty.
    #[test]
    fn answered_question_renders_picked_labels_and_typed_note_together() {
        let mut tc = make_tool_call_info(
            "toolu_q",
            "AskUserQuestion",
            crate::agent::model::ToolCallStatus::Completed,
            "",
        );
        tc.answered_questions = vec![crate::app::AnsweredQuestion {
            question: "Which areas need work?".to_owned(),
            picked_labels: vec!["Performance".to_owned(), "Documentation".to_owned()],
            typed_note: Some("and the bot reviewer reply etiquette".to_owned()),
        }];
        let lines = render_question_answered_card(&tc).expect("mixed answer produces lines");
        let strings = render_lines_to_strings(&lines);
        let joined = strings.join("\n");
        assert!(joined.contains("Which areas need work?"), "question text: {joined:?}");
        assert!(
            joined.contains("Performance, Documentation"),
            "picked-labels line must surface: {joined:?}",
        );
        assert!(
            joined.contains("and the bot reviewer reply etiquette"),
            "typed note must ALSO surface (the bug being fixed): {joined:?}",
        );
        assert!(
            joined.contains("you typed"),
            "typed line must carry the lead-in even alongside picks: {joined:?}",
        );
        // Both surfaces means 3 lines total: question + picked + typed.
        assert_eq!(
            strings.len(),
            3,
            "mixed card has question + picked + typed lines; got {strings:?}"
        );
    }

    /// Fix 2 (visual): the answered-card MUST align with the
    /// tool-icon column. Every standard tool row opens with a
    /// 2-space indent so the icon lands at column 2; the card's
    /// `?` should match that, and the `->` answer prefix nests one
    /// level deeper at column 4. Assert directly on the rendered
    /// Line's leading content (the snapshot harness's
    /// `buffer_to_text` trims TRAILING whitespace but leading
    /// indent survives).
    #[test]
    fn answered_question_card_indents_to_match_tool_icon_column() {
        let mut tc = make_tool_call_info(
            "toolu_q",
            "AskUserQuestion",
            crate::agent::model::ToolCallStatus::Completed,
            "",
        );
        tc.answered_questions = vec![crate::app::AnsweredQuestion {
            question: "Which build path?".to_owned(),
            picked_labels: vec!["Clean answered-card".to_owned()],
            typed_note: Some("with a side of toast".to_owned()),
        }];
        let lines = render_question_answered_card(&tc).expect("answered card produces lines");
        let strings = render_lines_to_strings(&lines);
        assert_eq!(strings.len(), 3, "question + picked + typed = 3 lines; got {strings:?}");
        assert!(
            strings[0].starts_with("  ? "),
            "question line must indent 2 spaces so `?` lands at the icon column; got {:?}",
            strings[0],
        );
        assert!(
            strings[1].starts_with("    \u{2192} "),
            "picked-answer line must nest one level deeper (4-space indent before →); got {:?}",
            strings[1],
        );
        assert!(
            strings[2].starts_with("    \u{2192} "),
            "typed-answer line must nest one level deeper (4-space indent before →); got {:?}",
            strings[2],
        );
    }

    #[test]
    fn answered_question_card_none_while_unanswered() {
        let tc = make_tool_call_info(
            "toolu_q",
            "AskUserQuestion",
            crate::agent::model::ToolCallStatus::InProgress,
            "",
        );
        assert!(
            render_question_answered_card(&tc).is_none(),
            "no card until the user has answered (dock is the live surface)",
        );
    }

    #[test]
    fn answered_question_card_none_for_non_question_tool() {
        let mut tc = make_tool_call_info(
            "toolu_r",
            "Read",
            crate::agent::model::ToolCallStatus::Completed,
            "",
        );
        tc.answered_questions = vec![crate::app::AnsweredQuestion {
            question: "q".to_owned(),
            picked_labels: vec!["a".to_owned()],
            typed_note: None,
        }];
        assert!(
            render_question_answered_card(&tc).is_none(),
            "only AskUserQuestion renders the card",
        );
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
            glyph: '\u{280B}',
            is_active_turn_assistant: false,
            show_empty_thinking: false,
            show_thinking: false,
            show_compacting: false,
            thinking_tokens: None,
            running_subagents: None,
        }
    }

    fn default_options() -> MessageRenderOptions {
        MessageRenderOptions {
            tools_collapsed: true,
            include_trailing_separator: true,
            suppress_group_header: false,
            envelope_streak_position: None,
            stop_hook_summary_actions: 0,
            stop_hook_summary_expanded: false,
        }
    }

    fn options_without_separator() -> MessageRenderOptions {
        MessageRenderOptions {
            tools_collapsed: true,
            include_trailing_separator: false,
            suppress_group_header: false,
            envelope_streak_position: None,
            stop_hook_summary_actions: 0,
            stop_hook_summary_expanded: false,
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
                tools_collapsed: true,
                include_trailing_separator: false,
                suppress_group_header: false,
                envelope_streak_position: None,
                stop_hook_summary_actions: 0,
                stop_hook_summary_expanded: false,
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
                tools_collapsed: true,
                include_trailing_separator: false,
                suppress_group_header: false,
                envelope_streak_position: None,
                stop_hook_summary_actions: 0,
                stop_hook_summary_expanded: false,
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
                        stop_hook_summary_actions: 0,
                        stop_hook_summary_expanded: false,
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
        let thinking_spinner =
            SpinnerState { show_thinking: true, glyph: '\u{2819}', ..idle_spinner() };
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
            tools_collapsed: true,
            include_trailing_separator: false,
            suppress_group_header: false,
            envelope_streak_position: None,
            stop_hook_summary_actions: 0,
            stop_hook_summary_expanded: false,
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
            tools_collapsed: true,
            include_trailing_separator: false,
            suppress_group_header: true,
            envelope_streak_position: None,
            stop_hook_summary_actions: 0,
            stop_hook_summary_expanded: false,
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

    // ----------------------------------------------------------------
    // #273: thinking_tokens spinner chip + format helper.
    // ----------------------------------------------------------------

    #[test]
    fn format_token_count_short_threshold_buckets() {
        // < 1k -> bare integer.
        assert_eq!(format_token_count_short(0), "0");
        assert_eq!(format_token_count_short(42), "42");
        assert_eq!(format_token_count_short(999), "999");
        // 1k..10k -> one-decimal abbreviation, truncating.
        assert_eq!(format_token_count_short(1_000), "1.0k");
        assert_eq!(format_token_count_short(1_199), "1.1k");
        assert_eq!(format_token_count_short(1_200), "1.2k");
        assert_eq!(format_token_count_short(9_900), "9.9k");
        // 10k..1M -> integer abbreviation.
        assert_eq!(format_token_count_short(15_000), "15k");
        assert_eq!(format_token_count_short(999_999), "999k");
        // 1M..10M -> one-decimal.
        assert_eq!(format_token_count_short(1_500_000), "1.5M");
        // >= 10M -> integer.
        assert_eq!(format_token_count_short(15_000_000), "15M");
    }

    #[test]
    fn thinking_line_renders_chip_when_token_count_provided() {
        let line = thinking_line('\u{280B}', Some(1_234));
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("thinking · 1.2k tok"),
            "expected chip with k abbreviation; got {rendered:?}",
        );
    }

    #[test]
    fn thinking_line_falls_back_to_bare_thinking_when_no_tokens_yet() {
        let line = thinking_line('\u{280B}', None);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("Thinking..."),
            "expected fallback shape when no count; got {rendered:?}",
        );
        assert!(!rendered.contains("tok"));
    }

    // ----------------------------------------------------------------
    // subagent_running_line: the chat-side "running subagent..." line
    // that surfaces while `App::subagents_view` is non-empty. Additive
    // to `thinking_line` - both render together when both apply.
    // ----------------------------------------------------------------

    #[test]
    fn subagent_running_line_single_uses_label_and_inspector_pointer() {
        let line =
            subagent_running_line('\u{280B}', 1, Some("Explore \u{b7} map hidden tool calls"));
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("\u{25c7}"),
            "expected the \u{25c7} subagent glyph; got {rendered:?}",
        );
        assert!(
            rendered.contains("running subagent: Explore \u{b7} map hidden tool calls"),
            "expected single-subagent label form; got {rendered:?}",
        );
        assert!(rendered.contains("see Inspector"), "expected Inspector pointer; got {rendered:?}");
    }

    #[test]
    fn subagent_running_line_multi_uses_count() {
        let line = subagent_running_line('\u{280B}', 3, Some("Explore"));
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("running 3 subagents"),
            "expected count form for >1 subagent; got {rendered:?}",
        );
        assert!(
            !rendered.contains("subagent:"),
            "expected the single-form `subagent:` label to be absent; got {rendered:?}",
        );
    }

    #[test]
    fn subagent_running_line_falls_back_when_label_is_unavailable() {
        let line = subagent_running_line('\u{280B}', 1, None);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("running subagent"),
            "expected fallback to the count form when label missing; got {rendered:?}",
        );
        assert!(rendered.contains("see Inspector"));
    }

    #[test]
    fn assistant_render_shows_subagent_line_when_only_subagent_active() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            running_subagents: Some(RunningSubagentsLine {
                count: 1,
                primary_label: Some("Explore".to_owned()),
            }),
            ..idle_spinner()
        };
        let mut msg = ChatMessage::new(MessageRole::Assistant, Vec::new(), None);
        let mut lines = Vec::new();

        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );

        let rendered = render_lines_to_strings(&lines);
        assert!(
            rendered.iter().any(|line| line.contains("running subagent: Explore")),
            "expected the running-subagent line; got {rendered:?}",
        );
        assert!(
            !rendered.iter().any(|line| line.contains("Thinking...")),
            "thinking_line absent when show_thinking is false; got {rendered:?}",
        );
    }

    #[test]
    fn assistant_render_stacks_subagent_line_with_thinking_when_both_active() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_thinking: true,
            running_subagents: Some(RunningSubagentsLine { count: 2, primary_label: None }),
            ..idle_spinner()
        };
        let mut msg = make_text_message(MessageRole::Assistant, "streaming");
        let mut lines = Vec::new();

        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );

        let rendered = render_lines_to_strings(&lines);
        let thinking_idx = rendered.iter().position(|line| line.contains("Thinking..."));
        let subagent_idx = rendered.iter().position(|line| line.contains("running 2 subagents"));
        assert!(
            thinking_idx.is_some(),
            "expected the thinking line alongside the subagent line; got {rendered:?}",
        );
        assert!(
            subagent_idx.is_some(),
            "expected the running-subagents line alongside the thinking line; got {rendered:?}",
        );
        assert!(
            thinking_idx < subagent_idx,
            "thinking line should appear above the subagent line; got {rendered:?}",
        );
    }

    #[test]
    fn assistant_render_keeps_thinking_when_no_subagent_active() {
        let spinner = SpinnerState {
            is_active_turn_assistant: true,
            show_thinking: true,
            running_subagents: None,
            ..idle_spinner()
        };
        let mut msg = make_text_message(MessageRole::Assistant, "streaming");
        let mut lines = Vec::new();

        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );

        let rendered = render_lines_to_strings(&lines);
        assert!(
            rendered.iter().any(|line| line.contains("Thinking...")),
            "expected the thinking line in the no-subagent baseline; got {rendered:?}",
        );
        assert!(
            !rendered.iter().any(|line| line.contains("running subagent")),
            "no running-subagent line when running_subagents is None; got {rendered:?}",
        );
    }

    #[test]
    fn assistant_render_skips_subagent_line_for_non_active_assistant() {
        let spinner = SpinnerState {
            is_active_turn_assistant: false,
            running_subagents: Some(RunningSubagentsLine {
                count: 1,
                primary_label: Some("Explore".to_owned()),
            }),
            ..idle_spinner()
        };
        let mut msg = make_text_message(MessageRole::Assistant, "older reply");
        let mut lines = Vec::new();

        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );

        let rendered = render_lines_to_strings(&lines);
        assert!(
            !rendered.iter().any(|line| line.contains("running subagent")),
            "non-active assistant messages must not render the chat-wide status line; got {rendered:?}",
        );
    }

    // ----------------------------------------------------------------
    // turn-duration formatter (used by `stop_hook_summary` rows) +
    // the post-#279 bare-`Forge` role label assertion.
    // ----------------------------------------------------------------

    #[test]
    fn format_turn_duration_buckets() {
        // < 60s -> one-decimal seconds.
        assert_eq!(format_turn_duration(0), "0.0s");
        assert_eq!(format_turn_duration(900), "0.9s");
        assert_eq!(format_turn_duration(12_400), "12.4s");
        assert_eq!(format_turn_duration(59_900), "59.9s");
        // 1m..1h -> integer Xm YYs (zero-padded seconds).
        assert_eq!(format_turn_duration(60_000), "1m 00s");
        assert_eq!(format_turn_duration(64_000), "1m 04s");
        assert_eq!(format_turn_duration(125_000), "2m 05s");
        assert_eq!(format_turn_duration(3_599_000), "59m 59s");
        // >= 1h -> Xh YYm ZZs (zero-padded minutes + seconds).
        assert_eq!(format_turn_duration(3_600_000), "1h 00m 00s");
        assert_eq!(format_turn_duration(3_724_000), "1h 02m 04s");
    }

    #[test]
    fn assistant_role_label_renders_bare_forge_when_turn_duration_absent() {
        let msg = make_text_message(MessageRole::Assistant, "anything");
        // turn_duration_ms defaults to None.
        let line = role_label_line(&msg);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "Forge");
    }

    #[test]
    fn assistant_role_label_renders_chip_when_turn_duration_present() {
        let mut msg = make_text_message(MessageRole::Assistant, "anything");
        msg.turn_duration_ms = Some(12_400);
        let line = role_label_line(&msg);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("Forge"), "role banner stays Forge: {rendered:?}");
        assert!(rendered.contains("12.4s"), "chip carries formatted duration: {rendered:?}");
        assert!(
            rendered.contains('\u{b7}'),
            "chip separator is the middle-dot \u{b7}: {rendered:?}"
        );
    }

    #[test]
    fn gotify_envelope_role_label_renders_distinct_gotify_source() {
        let msg = ChatMessage::new_gotify_envelope(MessageRole::User, vec![], None);
        let rendered: String =
            role_label_line(&msg).spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "Gotify", "an external notification is never labeled Forge");
    }

    #[test]
    fn peer_envelope_role_label_still_renders_forge() {
        let msg = ChatMessage::new_peer_envelope(MessageRole::User, vec![], None);
        let rendered: String =
            role_label_line(&msg).spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "Forge");
    }

    /// #383 follow-up (empty trailing bubble): an idle assistant with no
    /// blocks would otherwise render only a bare "Forge" role label with
    /// no body. Suppress it - an idle empty placeholder renders nothing.
    #[test]
    fn empty_idle_assistant_placeholder_renders_nothing() {
        let mut msg = ChatMessage::new(MessageRole::Assistant, vec![], None);
        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &idle_spinner(),
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );
        assert!(
            lines.is_empty(),
            "idle empty assistant placeholder renders nothing; got {:?}",
            render_lines_to_strings(&lines),
        );
    }

    /// The "Info" analog: an idle System message with no blocks must not
    /// render a bare "Info" label either.
    #[test]
    fn empty_idle_system_placeholder_renders_nothing() {
        let mut msg =
            ChatMessage::new(MessageRole::System(Some(SystemSeverity::Info)), vec![], None);
        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &idle_spinner(),
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );
        assert!(
            lines.is_empty(),
            "idle empty Info placeholder renders nothing; got {:?}",
            render_lines_to_strings(&lines),
        );
    }

    /// Suppression is idle-only: an actively-thinking empty placeholder is
    /// also empty-blocks but MUST still render its spinner.
    #[test]
    fn empty_thinking_placeholder_still_renders_spinner() {
        let spinner = SpinnerState {
            show_empty_thinking: true,
            is_active_turn_assistant: true,
            ..idle_spinner()
        };
        let mut msg = ChatMessage::new(MessageRole::Assistant, vec![], None);
        let mut lines = Vec::new();
        render_message(
            &mut msg,
            &spinner,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines,
        );
        assert!(
            !lines.is_empty(),
            "an actively-thinking empty placeholder still shows the spinner"
        );
    }

    /// The render-cache signature must fold running_subagents +
    /// is_active_turn_assistant: an empty assistant suppressed while idle
    /// must rebuild (not return the stale empty layout) when it flips into
    /// an active turn with a running subagent.
    #[test]
    fn subagent_flip_invalidates_empty_assistant_render_cache() {
        let mut msg = ChatMessage::new(MessageRole::Assistant, vec![], None);

        let mut lines_a = Vec::new();
        render_message(
            &mut msg,
            &idle_spinner(),
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines_a,
        );
        assert!(lines_a.is_empty(), "idle empty placeholder is suppressed");

        let active_with_subagent = SpinnerState {
            is_active_turn_assistant: true,
            running_subagents: Some(RunningSubagentsLine {
                count: 1,
                primary_label: Some("Explore".to_owned()),
            }),
            ..idle_spinner()
        };
        let mut lines_b = Vec::new();
        render_message(
            &mut msg,
            &active_with_subagent,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines_b,
        );
        let rendered = render_lines_to_strings(&lines_b);
        assert!(
            rendered.iter().any(|l| l.contains("subagent")),
            "cache rebuilds on the subagent flip instead of the stale empty render; got {rendered:?}",
        );
    }

    /// The signature folds the subagent line CONTENT: a count/label change
    /// on an empty active assistant must rebuild, not serve the stale line.
    #[test]
    fn subagent_count_change_invalidates_empty_assistant_render_cache() {
        let mut msg = ChatMessage::new(MessageRole::Assistant, vec![], None);

        let one = SpinnerState {
            is_active_turn_assistant: true,
            running_subagents: Some(RunningSubagentsLine {
                count: 1,
                primary_label: Some("Explore".to_owned()),
            }),
            ..idle_spinner()
        };
        let mut lines_a = Vec::new();
        render_message(
            &mut msg,
            &one,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines_a,
        );

        let many = SpinnerState {
            is_active_turn_assistant: true,
            running_subagents: Some(RunningSubagentsLine { count: 3, primary_label: None }),
            ..idle_spinner()
        };
        let mut lines_b = Vec::new();
        render_message(
            &mut msg,
            &many,
            MessageRenderContext::new(None, 120, 0, default_options()),
            &mut lines_b,
        );
        let rendered = render_lines_to_strings(&lines_b);
        assert!(
            rendered.iter().any(|l| l.contains("3 subagents")),
            "cache rebuilds on the count change instead of serving the stale line; got {rendered:?}",
        );
    }

    // ----------------------------------------------------------------
    // #273: stop_hook_summary collapsed chip + expanded body.
    // ----------------------------------------------------------------

    fn stop_hook_options(actions: u32, expanded: bool) -> MessageRenderOptions {
        MessageRenderOptions {
            stop_hook_summary_actions: actions,
            stop_hook_summary_expanded: expanded,
            ..options_without_separator()
        }
    }

    fn render_assistant_with_stop_hook(
        actions: u32,
        expanded: bool,
        hooks: &[StopHookEntry],
    ) -> Vec<String> {
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "done");
        let mut lines = Vec::new();
        let context = MessageRenderContext::new(None, 80, 0, stop_hook_options(actions, expanded))
            .with_stop_hook_hooks(hooks);
        render_message(&mut msg, &spinner, context, &mut lines);
        render_lines_to_strings(&lines)
    }

    #[test]
    fn stop_hook_summary_renders_collapsed_chip_when_actions_positive() {
        let rendered = render_assistant_with_stop_hook(3, false, &[]);
        // Forge label + "done" body + chip line.
        assert_eq!(rendered.first().map(String::as_str), Some("Forge"));
        assert!(rendered.iter().any(|line| line == "done"), "expected body line; got {rendered:?}");
        assert!(
            rendered.iter().any(|line| line.contains("↳ hook summary · 3 actions [▶ expand]")),
            "expected collapsed chip; got {rendered:?}",
        );
        // Body block must NOT render when collapsed.
        assert!(
            !rendered.iter().any(|line| line.contains("[▼ collapse]")),
            "collapsed chip must not show expand-state label; got {rendered:?}",
        );
    }

    #[test]
    fn stop_hook_summary_renders_singular_action_word() {
        let rendered = render_assistant_with_stop_hook(1, false, &[]);
        assert!(
            rendered.iter().any(|line| line.contains("↳ hook summary · 1 action [▶ expand]")),
            "expected `1 action` (singular); got {rendered:?}",
        );
    }

    #[test]
    fn stop_hook_summary_renders_nothing_when_actions_zero() {
        let rendered = render_assistant_with_stop_hook(0, false, &[]);
        // Forge label + "done" only - no chip, no expand state.
        assert!(
            rendered.iter().all(|line| !line.contains("↳ hook summary")),
            "actions==0 must produce no chip; got {rendered:?}",
        );
    }

    #[test]
    fn stop_hook_summary_expanded_renders_hook_rows() {
        let hooks = vec![
            StopHookEntry {
                command: "bash ~/.claude/hooks/notify.sh".to_owned(),
                duration_ms: 980,
            },
            StopHookEntry { command: "bash ~/.claude/hooks/log.sh".to_owned(), duration_ms: 1_500 },
        ];
        let rendered = render_assistant_with_stop_hook(2, true, &hooks);
        assert!(
            rendered.iter().any(|line| line.contains("↳ hook summary · 2 actions [▼ collapse]")),
            "expected expand-state label; got {rendered:?}",
        );
        assert!(
            rendered.iter().any(
                |line| line.contains("bash ~/.claude/hooks/notify.sh") && line.contains("0.9s")
            ),
            "expected first hook row; got {rendered:?}",
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("bash ~/.claude/hooks/log.sh") && line.contains("1.5s")),
            "expected second hook row; got {rendered:?}",
        );
    }

    #[test]
    fn stop_hook_summary_stamps_hit_test_fields_when_chip_rendered() {
        // The renderer must stamp `stop_hook_summary_y_in_msg /
        // stop_hook_summary_height` on the ChatMessage so the mouse
        // handler can route clicks back to the toggle. Stamping
        // happens during `build_message_layout` - invoked through
        // any render or measure call.
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "done");
        let mut out = Vec::new();
        let ctx = MessageRenderContext::new(None, 80, 0, stop_hook_options(2, false))
            .with_stop_hook_hooks(&[]);
        render_message(&mut msg, &spinner, ctx, &mut out);
        assert!(
            msg.stop_hook_summary_height > 0,
            "renderer must stamp non-zero height when chip is rendered",
        );
        assert!(
            msg.stop_hook_summary_y_in_msg > 0,
            "y_in_msg should be > 0 (chip sits after the assistant body)",
        );
    }

    #[test]
    fn stop_hook_summary_stamps_zero_when_no_chip() {
        // Inverse: when actions==0 the renderer resets the stamped
        // fields to zero so a previously-rendered chip can't ghost
        // a click target after the source data is gone.
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "done");
        msg.stop_hook_summary_y_in_msg = 999;
        msg.stop_hook_summary_height = 999;
        let mut out = Vec::new();
        let ctx = MessageRenderContext::new(None, 80, 0, stop_hook_options(0, false));
        render_message(&mut msg, &spinner, ctx, &mut out);
        assert_eq!(msg.stop_hook_summary_height, 0);
        assert_eq!(msg.stop_hook_summary_y_in_msg, 0);
    }

    #[test]
    fn stop_hook_summary_cache_invalidates_when_expand_toggles() {
        // Toggling `stop_hook_summary_expanded` must rebuild the
        // cache; the expanded view is taller than the collapsed view
        // because the hook rows lift below the chip.
        let spinner = idle_spinner();
        let mut msg = make_text_message(MessageRole::Assistant, "done");
        let hooks = vec![StopHookEntry { command: "bash hook.sh".to_owned(), duration_ms: 500 }];

        let collapsed_ctx = MessageRenderContext::new(None, 80, 0, stop_hook_options(1, false))
            .with_stop_hook_hooks(hooks.as_slice());
        let collapsed = get_or_build_message_render_cache(&mut msg, &spinner, collapsed_ctx);
        let collapsed_height = collapsed.height();

        let expanded_ctx = MessageRenderContext::new(None, 80, 0, stop_hook_options(1, true))
            .with_stop_hook_hooks(hooks.as_slice());
        let expanded = get_or_build_message_render_cache(&mut msg, &spinner, expanded_ctx);
        let expanded_height = expanded.height();

        assert!(
            expanded_height > collapsed_height,
            "expanded ({expanded_height}) must be taller than collapsed ({collapsed_height})",
        );
    }

    /// The render signature folds `project_root`, so re-rendering the
    /// same grouped-read message under a different root rebuilds the
    /// cached layout: the paths relativize against the new root instead
    /// of returning the stale (absolute) first render. Guards the
    /// signature fold that keeps the read tree honest after an account
    /// switch / worktree cwd change.
    #[test]
    fn project_root_change_invalidates_render_cache() {
        use crate::agent::model::ToolCallStatus;
        fn read_block(id: &str, abs_path: &str) -> MessageBlock {
            let mut tc = make_tool_call_info(id, "Read", ToolCallStatus::Completed, "");
            tc.raw_input = Some(serde_json::json!({ "file_path": abs_path }));
            MessageBlock::ToolCall(Box::new(tc))
        }
        fn render_under_root(msg: &mut ChatMessage, spinner: &SpinnerState, root: &str) -> String {
            let ctx =
                MessageRenderContext::new(None, 80, 0, default_options()).with_project_root(root);
            let mut out: Vec<Line<'static>> = Vec::new();
            render_message_from_offset_internal_with_mode(msg, spinner, ctx, 0, &mut out);
            render_lines_to_strings(&out).join("\n")
        }
        let spinner = idle_spinner();
        let mut msg = ChatMessage::new(
            MessageRole::Assistant,
            vec![
                read_block("r0", "/repo/crates/forge-tui/src/0.rs"),
                read_block("r1", "/repo/crates/forge-tui/src/1.rs"),
            ],
            None,
        );
        // An unrelated root leaves the paths absolute (nothing to strip).
        let under_other = render_under_root(&mut msg, &spinner, "/elsewhere");
        assert!(
            under_other.contains("/repo/crates/forge-tui/src/0.rs"),
            "under an unrelated root the path stays absolute; got:\n{under_other}",
        );
        // Switching to the real root must rebuild (not reuse the cache).
        let under_repo = render_under_root(&mut msg, &spinner, "/repo");
        assert!(
            under_repo.contains("crates/forge-tui/src/0.rs"),
            "under the real root the path relativizes; got:\n{under_repo}",
        );
        assert!(
            !under_repo.contains("/repo/crates"),
            "the cache rebuilt on the root change - no stale absolute prefix; got:\n{under_repo}",
        );
    }

    // ----------------------------------------------------------------
    // Monitor + Workflow lifecycle one-liner render.
    // ----------------------------------------------------------------

    #[test]
    fn monitor_alive_renders_block_with_header_command_and_tail() {
        let mut tc = make_tool_call_info(
            "toolu_mon",
            "Monitor",
            crate::agent::model::ToolCallStatus::InProgress,
            "",
        );
        tc.raw_input = Some(serde_json::json!({
            "description": "ci-watch",
            "command": "gh run watch 18234567",
            "persistent": true,
            "timeout_ms": 0,
        }));
        tc.monitor_output_tail = vec![
            "* build  \u{b7} in_progress".to_owned(),
            "\u{2713} lint   \u{b7} success".to_owned(),
            "* deploy \u{b7} queued".to_owned(),
            "* notify \u{b7} queued".to_owned(),
        ];
        let lines = render_lifecycle_one_liner(&tc).expect("Monitor produces lines");
        // 1 header + 1 command + 4 tail = 6 lines.
        assert_eq!(lines.len(), 6, "got {} lines", lines.len());
        let joined: String = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("\u{25c9}"), "header ◉ glyph: {joined:?}");
        assert!(joined.contains("Monitor"), "header label: {joined:?}");
        assert!(joined.contains("ci-watch"), "description: {joined:?}");
        assert!(joined.contains("persistent"), "persistent suffix: {joined:?}");
        assert!(joined.contains("$ gh run watch 18234567"), "command line: {joined:?}");
        assert!(joined.contains("\u{2502}"), "│ tree connector: {joined:?}");
        assert!(joined.contains("\u{2514}"), "└ tree connector for last row: {joined:?}");
    }

    #[test]
    fn monitor_alive_no_tail_yet_renders_just_header_and_command() {
        let mut tc = make_tool_call_info(
            "toolu_mon",
            "Monitor",
            crate::agent::model::ToolCallStatus::InProgress,
            "",
        );
        tc.raw_input = Some(serde_json::json!({
            "description": "forge-monitor-test",
            "command": "tail -F app.log",
            "persistent": false,
            "timeout_ms": 0,
        }));
        // monitor_output_tail stays default-empty.
        let lines = render_lifecycle_one_liner(&tc).expect("Monitor produces lines");
        assert_eq!(lines.len(), 2, "header + command only: {} lines", lines.len());
        let joined: String = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("forge-monitor-test"));
        // Non-persistent: no " · persistent" suffix.
        assert!(!joined.contains("persistent"), "persistent suffix suppressed: {joined:?}");
        assert!(joined.contains("$ tail -F app.log"), "command present: {joined:?}");
        // └ connector on the command row (no tail follows).
        assert!(joined.contains("\u{2514}"), "└ connector when no tail: {joined:?}");
        // No │ connector (would only appear if a tail row followed).
        assert!(!joined.contains("\u{2502}"), "no │ connector when tail empty: {joined:?}");
    }

    #[test]
    fn monitor_terminal_completed_renders_collapsed_one_liner() {
        let mut tc = make_tool_call_info(
            "toolu_mon",
            "Monitor",
            crate::agent::model::ToolCallStatus::Completed,
            "",
        );
        tc.raw_input = Some(serde_json::json!({
            "description": "ci-watch",
            "command": "gh run watch 1",
            "persistent": false,
            "timeout_ms": 60000,
        }));
        let lines = render_lifecycle_one_liner(&tc).expect("Monitor produces lines");
        assert_eq!(lines.len(), 1, "terminal collapses to one line");
        let rendered: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("\u{2713}"), "✓ glyph: {rendered:?}");
        assert!(rendered.contains("Monitor"));
        assert!(rendered.contains("ci-watch"));
        assert!(rendered.contains("completed"));
    }

    #[test]
    fn monitor_terminal_killed_renders_stopped() {
        let mut tc = make_tool_call_info(
            "toolu_mon",
            "Monitor",
            crate::agent::model::ToolCallStatus::Killed,
            "",
        );
        tc.raw_input = Some(serde_json::json!({
            "description": "ci-watch",
            "command": "gh run watch 1",
            "persistent": false,
            "timeout_ms": 0,
        }));
        let lines = render_lifecycle_one_liner(&tc).expect("Monitor produces lines");
        let rendered: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("stopped"), "Killed -> stopped: {rendered:?}");
    }

    #[test]
    fn monitor_terminal_failed_renders_timed_out() {
        let mut tc = make_tool_call_info(
            "toolu_mon",
            "Monitor",
            crate::agent::model::ToolCallStatus::Failed,
            "",
        );
        tc.raw_input = Some(serde_json::json!({
            "description": "ci-watch",
            "command": "gh run watch 1",
            "persistent": false,
            "timeout_ms": 1000,
        }));
        let lines = render_lifecycle_one_liner(&tc).expect("Monitor produces lines");
        let rendered: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("timed out"), "Failed -> timed out: {rendered:?}");
    }

    #[test]
    fn monitor_with_malformed_input_falls_through_to_default_render() {
        // Missing required `command` field - parser returns None,
        // helper returns None so the caller falls through to the
        // standard tool-card render path.
        let mut tc = make_tool_call_info(
            "toolu_mon",
            "Monitor",
            crate::agent::model::ToolCallStatus::InProgress,
            "",
        );
        tc.raw_input = Some(serde_json::json!({ "description": "x" }));
        assert!(render_lifecycle_one_liner(&tc).is_none());
    }

    #[test]
    fn workflow_meta_name_extracts_from_script_block() {
        let script = "export const meta = {\n  name: 'minimal-ping',\n  description: 'sanity'\n}\n\nphase('Ping')";
        assert_eq!(workflow_meta_name(script), "minimal-ping");
    }

    #[test]
    fn workflow_meta_name_handles_double_quoted_name() {
        let script = "export const meta = { name: \"snapshot-runner\" }";
        assert_eq!(workflow_meta_name(script), "snapshot-runner");
    }

    #[test]
    fn workflow_meta_name_falls_back_when_block_absent() {
        let script = "await agent('do thing')";
        assert_eq!(workflow_meta_name(script), "Workflow");
    }

    #[test]
    fn workflow_meta_fields_extracts_name_and_description() {
        let script = "export const meta = {\n  name: 'minimal-ping',\n  description: 'sanity'\n}";
        let (name, desc) = workflow_meta_fields(script);
        assert_eq!(name, "minimal-ping");
        assert_eq!(desc.as_deref(), Some("sanity"));
    }

    #[test]
    fn workflow_meta_fields_returns_none_description_when_absent() {
        let script = "export const meta = { name: 'short' }";
        let (name, desc) = workflow_meta_fields(script);
        assert_eq!(name, "short");
        assert!(desc.is_none());
    }

    #[test]
    fn non_lifecycle_tool_returns_none() {
        let tc = make_tool_call_info(
            "toolu_x",
            "Bash",
            crate::agent::model::ToolCallStatus::InProgress,
            "",
        );
        assert!(render_lifecycle_one_liner(&tc).is_none());
    }
}
