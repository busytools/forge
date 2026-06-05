// ratatui geometry: terminal dims are u16, scroll math goes through f32
// for smooth-scroll. Casts (usize↔f32, f32→u16/usize) are inherent and bounded by
// terminal size.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use crate::app::cache_metrics;
use crate::app::{
    App, AppStatus, MessageBlock, MessageRole, ScrollbarGeometry, SelectionKind, SelectionState,
};
use crate::ui::message::{self, RunningSubagentsLine, SpinnerState};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Minimum number of messages to render above/below the visible range as a margin.
/// Heights are now exact (block-level wrapped heights), so no safety margin is needed.
const CULLING_MARGIN: usize = 0;
const CULLING_OVERSCAN_ROWS: usize = 100;
const SCROLLBAR_MIN_THUMB_HEIGHT: usize = 1;
/// Visual cap for the chat scrollbar thumb so a short scrollback
/// doesn't render a thumb that takes up most of the rail. The raw
/// `viewport² / content` formula grows the thumb as content shrinks
/// (fine in theory as a proportional indicator, distracting in
/// practice for a chat surface that briefly overflows by a handful
/// of rows). Matches the Inspector pane's `INSPECTOR_THUMB_MAX_CELLS`
/// cap so both surfaces read with the same visual weight.
const SCROLLBAR_MAX_THUMB_HEIGHT: usize = 1;
const SCROLLBAR_TOP_EASE: f32 = 0.35;
const SCROLLBAR_SIZE_EASE: f32 = 0.2;
const SCROLLBAR_EASE_EPSILON: f32 = 0.01;
const OVERSCROLL_CLAMP_EASE: f32 = 0.2;
const CHAT_SCROLLBAR_WIDTH: u16 = 1;

#[derive(Clone, Copy, Default)]
struct HeightUpdateStats {
    measured_msgs: usize,
    measured_lines: usize,
    reused_msgs: usize,
}

#[derive(Clone, Copy, Default)]
struct RemeasureBudget {
    remaining_msgs: usize,
    remaining_lines: usize,
}

impl RemeasureBudget {
    fn new(viewport_height: usize) -> Self {
        let viewport_floor = viewport_height.max(12);
        Self {
            remaining_msgs: viewport_floor,
            remaining_lines: viewport_floor.saturating_mul(8).max(256),
        }
    }

    fn exhausted(self) -> bool {
        self.remaining_msgs == 0 || self.remaining_lines == 0
    }

    fn consume(&mut self, wrapped_lines: usize) {
        self.remaining_msgs = self.remaining_msgs.saturating_sub(1);
        self.remaining_lines = self.remaining_lines.saturating_sub(wrapped_lines.max(1));
    }
}

#[derive(Clone, Copy, Default)]
struct CulledRenderStats {
    local_scroll: usize,
    first_visible: usize,
    render_start: usize,
    rendered_msgs: usize,
    last_rendered_idx: Option<usize>,
    rendered_line_count: usize,
}

struct ScrolledRenderData {
    paragraph: Paragraph<'static>,
    stats: CulledRenderStats,
    max_scroll: usize,
    scroll_offset: usize,
}

fn chat_content_area(area: Rect) -> Rect {
    Rect { width: area.width.saturating_sub(CHAT_SCROLLBAR_WIDTH), ..area }
}

fn chat_scrollbar_area(area: Rect) -> Option<Rect> {
    (area.width > 0).then(|| Rect {
        x: area.right().saturating_sub(CHAT_SCROLLBAR_WIDTH),
        y: area.y,
        width: CHAT_SCROLLBAR_WIDTH.min(area.width),
        height: area.height,
    })
}
/// Build a `SpinnerState` for a specific message index.
fn msg_spinner(
    base: &SpinnerState,
    index: usize,
    active_turn_assistant: Option<usize>,
    msg: &crate::app::ChatMessage,
) -> SpinnerState {
    let is_assistant = matches!(msg.role, MessageRole::Assistant);
    let is_active_turn_assistant = is_assistant && active_turn_assistant == Some(index);
    let has_blocks = !msg.blocks.is_empty();
    SpinnerState {
        is_active_turn_assistant,
        show_empty_thinking: is_active_turn_assistant && base.show_empty_thinking,
        show_thinking: is_active_turn_assistant && base.show_thinking && has_blocks,
        show_compacting: is_active_turn_assistant && base.show_compacting,
        ..base.clone()
    }
}

/// Ensure every message has an up-to-date height in the viewport at the given width.
/// The last message is always recomputed while streaming (content changes each frame).
///
/// Height is ground truth: each message is rendered into a scratch buffer via
/// `render_message()` and measured with `Paragraph::line_count(width)`. This uses
/// the exact same wrapping algorithm as the actual render path, so heights can
/// never drift from reality.
///
/// Iterates in reverse so we can break early: once we hit a message whose height
/// is already valid at this width, all earlier messages are also valid (content
/// only changes at the tail during streaming). This turns the common case from
/// O(n) to O(1).
fn update_visual_heights(
    app: &mut App,
    base: &SpinnerState,
    width: u16,
    viewport_height: usize,
) -> HeightUpdateStats {
    let msg_count = app.messages().len();
    let _t = app.perf.as_ref().map(|p| p.start_with("chat::update_heights", "msgs", msg_count));
    app.active_viewport_mut().sync_message_count(msg_count);

    let is_streaming = matches!(app.status, AppStatus::Thinking | AppStatus::Running);
    let active_turn_assistant = app.active_turn_assistant_idx();
    sync_active_turn_height_state(app, base, active_turn_assistant);
    let mut stats = HeightUpdateStats::default();

    if msg_count == 0 {
        app.active_viewport_mut().finalize_remeasure_if_clean();
        return stats;
    }

    // Snapshot loop-invariant fields once. `layout_generation` and
    // `tools_collapsed` are stable across the remeasure loops below
    // - bumping `layout_generation` requires a full
    // `sync_message_count` / resize, which already ran above.
    let mode_id_owned = app.mode().map(|mode| mode.current_mode_id.clone());
    let mode_id = mode_id_owned.as_deref();
    let layout_generation = app.viewport().layout_generation;
    let tools_collapsed = app.tools_collapsed;

    // The visible window for the priority + visible loops follows
    // the CURRENT scroll position, not the frozen scroll_anchor on
    // the remeasure plan. Without this, an off-screen
    // `invalidate_message` captured the scroll at invalidate-time,
    // and a subsequent user scroll left the stale message
    // permanently outside the anchor-window so it could never
    // re-measure (the lazy contract this PR enforces). The plan's
    // anchor still drives the resize loop's outward growth (below).
    //
    // Bootstrap degenerate case: with all heights == 0,
    // `current_visible_window` returns `(N-1, N-1)` because no
    // prefix-sum entry strictly exceeds scroll = 0. Fall back to
    // `(0, min(viewport_height - 1, msg_count - 1))` so the visible
    // loop measures roughly a viewport-worth of messages from the
    // TOP, including idx 0. The off-screen tail stays stale and is
    // converged by the budgeted resize loop over subsequent frames.
    let bootstrap = app.viewport().total_message_height() == 0 && msg_count > 0;
    let (visible_start, visible_end) = if bootstrap {
        let last = msg_count.saturating_sub(1);
        (0_usize, viewport_height.saturating_sub(1).min(last))
    } else {
        app.active_viewport_mut()
            .current_visible_window(viewport_height)
            .or_else(|| app.active_viewport_mut().remeasure_anchor_window(viewport_height))
            .unwrap_or((0, 0))
    };
    app.active_viewport_mut().ensure_remeasure_anchor(visible_start, visible_end, msg_count);

    // Priority loop: drain queued urgent indices, but only MEASURE
    // those that are currently visible. Off-screen entries keep
    // their stale bit set (we never call `mark_message_height_measured`
    // here) and re-measure lazily when they scroll into view via
    // the visible loop. This is half of the off-screen-laziness fix.
    while let Some(i) = app.active_viewport_mut().next_priority_remeasure() {
        if !(visible_start..=visible_end).contains(&i) {
            continue;
        }
        let is_last = i + 1 == msg_count;
        if !needs_height_measure(app, i, is_last, active_turn_assistant, is_streaming) {
            stats.reused_msgs += 1;
            continue;
        }
        measure_message_height_at(
            app,
            base,
            active_turn_assistant,
            width,
            i,
            mode_id,
            layout_generation,
            tools_collapsed,
            &mut stats,
        );
    }

    // Visible loop: every message in the live visible window must
    // be current by the end of this loop. The window itself is
    // narrow (find_first_visible / find_last_visible binary-search
    // prefix sums and stop once the viewport_height is covered) so
    // walking it in full is the correct upper bound. Bootstrap
    // (no prefix sums yet) hands off the off-screen tail to the
    // budgeted background loop below; we don't try to measure it
    // here.
    for i in visible_start..=visible_end {
        let is_last = i + 1 == msg_count;
        if !needs_height_measure(app, i, is_last, active_turn_assistant, is_streaming) {
            stats.reused_msgs += 1;
            continue;
        }
        measure_message_height_at(
            app,
            base,
            active_turn_assistant,
            width,
            i,
            mode_id,
            layout_generation,
            tools_collapsed,
            &mut stats,
        );
    }

    if is_streaming {
        let last = msg_count.saturating_sub(1);
        if needs_height_measure(app, last, true, active_turn_assistant, true) {
            measure_message_height_at(
                app,
                base,
                active_turn_assistant,
                width,
                last,
                mode_id,
                layout_generation,
                tools_collapsed,
                &mut stats,
            );
        }
    }

    // Background re-measure runs only when a Resize / Global /
    // MessagesFrom invalidation has set the sticky convergence flag.
    // Per-message tool events (`MessageChanged`) leave the flag
    // false, so a session with many Bash / Monitor / streaming
    // invalidations no longer chews up to viewport_height off-screen
    // messages every frame - those stay stale and re-measure lazily
    // when scrolled into view. Also skipped on the bootstrap frame
    // (visible loop already covered a viewport-worth; we don't want
    // to double the cost off-screen on the very first frame).
    let run_resize_loop = !bootstrap && app.viewport().background_convergence_pending;
    let mut budget = RemeasureBudget::new(viewport_height);
    while run_resize_loop
        && app.active_viewport_mut().remeasure_active()
        && !budget.exhausted()
    {
        let Some(i) = app.active_viewport_mut().next_remeasure_index(msg_count) else {
            break;
        };
        if (visible_start..=visible_end).contains(&i) {
            continue;
        }
        let is_last = i + 1 == msg_count;
        if !needs_height_measure(app, i, is_last, active_turn_assistant, is_streaming) {
            stats.reused_msgs += 1;
            continue;
        }
        let measured_lines_before = stats.measured_lines;
        measure_message_height_at(
            app,
            base,
            active_turn_assistant,
            width,
            i,
            mode_id,
            layout_generation,
            tools_collapsed,
            &mut stats,
        );
        budget.consume(stats.measured_lines.saturating_sub(measured_lines_before));
    }

    app.active_viewport_mut().finalize_remeasure_if_clean();
    stats
}

fn needs_height_measure(
    app: &App,
    idx: usize,
    is_last: bool,
    active_turn_assistant: Option<usize>,
    is_streaming: bool,
) -> bool {
    let _ = (is_last, active_turn_assistant, is_streaming);
    !app.viewport().message_height_is_current(idx)
}

fn sync_active_turn_height_state(
    app: &mut App,
    base: &SpinnerState,
    active_turn_assistant: Option<usize>,
) {
    let next = active_turn_assistant.and_then(|idx| {
        let message = app.messages().get(idx)?;
        let spinner = msg_spinner(base, idx, active_turn_assistant, message);
        let empty_indicator_visible =
            message.blocks.is_empty() && (spinner.show_compacting || spinner.show_empty_thinking);
        let trailing_indicator_visible =
            !message.blocks.is_empty() && (spinner.show_compacting || spinner.show_thinking);
        Some((idx, empty_indicator_visible, trailing_indicator_visible))
    });

    if app.last_active_turn_height_state() == next {
        return;
    }

    let mut affected = Vec::with_capacity(2);
    if let Some((prev_idx, _, _)) = app.last_active_turn_height_state() {
        affected.push(prev_idx);
    }
    if let Some((next_idx, _, _)) = next
        && affected.last().copied() != Some(next_idx)
    {
        affected.push(next_idx);
    }

    if !affected.is_empty() {
        app.invalidate_message_set(affected);
    }

    app.set_last_active_turn_height_state(next);
}

// `mode_id`, `layout_generation`, `tools_collapsed` are loop-invariant
// across `update_visual_heights`'s remeasure loops; callers snapshot
// them once above the loop and pass references / Copy values in. This
// avoids N String allocations on remeasure-heavy frames.
fn measure_message_height_at(
    app: &mut App,
    base: &SpinnerState,
    active_turn_assistant: Option<usize>,
    width: u16,
    idx: usize,
    mode_id: Option<&str>,
    layout_generation: u64,
    tools_collapsed: bool,
    stats: &mut HeightUpdateStats,
) {
    let msg_count = app.messages().len();
    let is_last_message = idx + 1 == msg_count;
    let sp = msg_spinner(base, idx, active_turn_assistant, &app.messages()[idx]);
    let suppress_group_header = message::compute_suppress_group_header(app.messages(), idx);
    let envelope_streak_position = message::compute_envelope_streak_position(app.messages(), idx);
    // #273: read the snapshot up-front so the immutable borrow of
    // `app` releases before the `active_messages_mut()` mutable
    // borrow further down. Owned clone of the hooks list keeps the
    // lifetime story flat. Same shape for the chat tool-call group
    // collapse levels.
    let stop_hook_snapshot = stop_hook_summary_for(app, idx);
    let group_collapse_levels =
        app.active_session().map(|s| s.group_collapse_levels.clone()).unwrap_or_default();
    let messaging_group_collapse_levels =
        app.active_session().map(|s| s.messaging_group_collapse_levels.clone()).unwrap_or_default();
    // Cross-message peer/worker run state: compute the session-level
    // partition once and lift this message's slice. Without this the
    // measure path's per-message partition wouldn't see segments that
    // continue from a sibling turn, and the measure-vs-render layout
    // would desync.
    let session_units = message::grouping::partition_session_into_render_units(app.messages());
    let owned_session_units = session_units.get(idx).cloned().unwrap_or_default();
    let (h, rendered_lines) = measure_message_height(
        &mut app.active_messages_mut()[idx],
        &sp,
        mode_id,
        width,
        layout_generation,
        message::MessageRenderOptions {
            tools_collapsed,
            include_trailing_separator: !is_last_message,
            suppress_group_header,
            envelope_streak_position,
            stop_hook_summary_actions: stop_hook_snapshot.actions,
            stop_hook_summary_expanded: stop_hook_snapshot.expanded,
        },
        stop_hook_snapshot.hooks.as_slice(),
        &group_collapse_levels,
        &messaging_group_collapse_levels,
        &owned_session_units,
    );
    app.sync_render_cache_message(idx);
    stats.measured_msgs += 1;
    stats.measured_lines += rendered_lines;
    let vp = app.active_viewport_mut();
    vp.set_message_height(idx, h);
    vp.mark_message_height_measured(idx);
}

/// #273: Snapshot of the per-message stop_hook_summary for the
/// render passes. Owned so the borrow of `app` releases before
/// downstream mutable borrows. `actions == 0` when no summary is
/// attached to `idx`; the renderer is responsible for skipping the
/// chip in that case.
#[derive(Default, Clone)]
struct StopHookSnapshot {
    actions: u32,
    expanded: bool,
    hooks: Vec<crate::app::StopHookEntry>,
}

fn stop_hook_summary_for(app: &App, idx: usize) -> StopHookSnapshot {
    let Some(summary) = app.last_stop_hook_summary() else {
        return StopHookSnapshot::default();
    };
    if summary.message_idx != idx {
        return StopHookSnapshot::default();
    }
    StopHookSnapshot {
        actions: summary.actions,
        expanded: app.stop_hook_summary_expanded(idx),
        hooks: summary.hooks.clone(),
    }
}

/// Measure message height using ground truth: render the message into a scratch
/// buffer and call `Paragraph::line_count(width)`.
///
/// This uses the exact same code path as actual rendering (`render_message()`),
/// so heights can never diverge from what appears on screen. The scratch vec is
/// temporary and discarded after measurement. Block-level caches are still
/// populated as a side effect (via `render_text_cached` / `render_tool_call_cached`),
/// so completed blocks remain O(1) on subsequent calls.
fn measure_message_height(
    msg: &mut crate::app::ChatMessage,
    spinner: &SpinnerState,
    current_mode_id: Option<&str>,
    width: u16,
    layout_generation: u64,
    options: message::MessageRenderOptions,
    stop_hook_hooks: &[crate::app::StopHookEntry],
    group_collapse_levels: &std::collections::HashMap<
        crate::ui::message::grouping::GroupId,
        crate::ui::message::grouping::GroupCollapseLevel,
    >,
    messaging_group_collapse_levels: &std::collections::HashMap<
        crate::ui::message::grouping::GroupId,
        crate::ui::message::grouping::GroupCollapseLevel,
    >,
    session_message_units: &[crate::ui::message::grouping::RenderUnit],
) -> (usize, usize) {
    let _t = crate::perf::start_with("chat::measure_msg", "blocks", msg.blocks.len());
    let render_context =
        message::MessageRenderContext::new(current_mode_id, width, layout_generation, options)
            .with_stop_hook_hooks(stop_hook_hooks)
            .with_group_collapse_levels(group_collapse_levels)
            .with_messaging_group_collapse_levels(messaging_group_collapse_levels)
            .with_session_message_units(session_message_units);
    let (h, wrapped_lines) =
        message::measure_message_height_cached_with_context(msg, spinner, render_context);
    crate::perf::mark_with("chat::measure_msg_wrapped_lines", "lines", wrapped_lines);
    (h, wrapped_lines)
}

fn build_base_spinner(app: &App) -> SpinnerState {
    // `show_thinking` fires on both `Thinking` (no body streamed yet)
    // and `Running` (mid-stream / tool execution) so the spinner keeps
    // ticking visibly across the whole turn - not just the pre-body
    // window. Without this, switching back into a still-running
    // session whose assistant placeholder has already streamed some
    // content shows the content frozen with no indicator that more is
    // coming.
    let turn_in_flight = matches!(app.status, AppStatus::Thinking | AppStatus::Running);
    SpinnerState {
        frame: app.spinner_frame,
        is_active_turn_assistant: false,
        show_empty_thinking: turn_in_flight,
        show_thinking: turn_in_flight,
        show_compacting: app.is_compacting(),
        // #273: only carry the chip during an in-flight turn - once
        // the turn ends the field will have been cleared by
        // `handle_result`, but gating here keeps the chip from
        // briefly flashing across the final layout pass.
        thinking_tokens: if turn_in_flight { app.latest_thinking_tokens() } else { None },
        running_subagents: derive_running_subagents(app),
    }
}

fn derive_running_subagents(app: &App) -> Option<RunningSubagentsLine> {
    let view = app.subagents_view();
    let running: Vec<&crate::app::SubagentEntry> = view
        .iter()
        .filter(|entry| {
            !matches!(
                entry.status,
                crate::agent::model::ToolCallStatus::Completed
                    | crate::agent::model::ToolCallStatus::Failed
                    | crate::agent::model::ToolCallStatus::Killed
            )
        })
        .collect();
    if running.is_empty() {
        return None;
    }
    let count = running.len();
    let primary_label = (count == 1).then(|| running[0].label.clone());
    Some(RunningSubagentsLine { count, primary_label })
}

fn sync_chat_layout(app: &mut App, area: Rect, base_spinner: &SpinnerState) -> usize {
    let width = area.width;
    let viewport_height = usize::from(area.height);

    {
        let _t = app.perf.as_ref().map(|p| p.start("chat::on_frame"));
        if app.active_viewport_mut().on_frame(width, area.height).resized() {
            app.cache_metrics_mut().record_resize();
        }
    }
    let height_stats = update_visual_heights(app, base_spinner, width, viewport_height);
    crate::perf::mark_with(
        "chat::update_heights_measured_msgs",
        "msgs",
        height_stats.measured_msgs,
    );
    crate::perf::mark_with("chat::update_heights_reused_msgs", "msgs", height_stats.reused_msgs);
    crate::perf::mark_with(
        "chat::update_heights_measured_lines",
        "lines",
        height_stats.measured_lines,
    );

    {
        let _t = app.perf.as_ref().map(|p| p.start("chat::prefix_sums"));
        app.active_viewport_mut().rebuild_prefix_sums();
    }
    {
        let vp = app.active_viewport_mut();
        if let Some((anchor_idx, anchor_offset)) = vp.ready_scroll_anchor_to_restore() {
            vp.restore_scroll_anchor(anchor_idx, anchor_offset);
        }
    }

    let content_height = app.active_viewport_mut().total_message_height();
    crate::perf::mark_with("chat::content_height", "rows", content_height);
    crate::perf::mark_with("chat::viewport_height", "rows", viewport_height);
    crate::perf::mark_with(
        "chat::content_overflow_rows",
        "rows",
        content_height.saturating_sub(viewport_height),
    );
    content_height
}

fn build_scrolled_render_data(
    app: &mut App,
    base: &SpinnerState,
    width: u16,
    content_height: usize,
    viewport_height: usize,
) -> ScrolledRenderData {
    let reduced_motion = app.config.prefers_reduced_motion_effective();
    let vp = app.active_viewport_mut();
    let max_scroll = content_height.saturating_sub(viewport_height);
    if vp.auto_scroll {
        vp.scroll_target = max_scroll;
        // Auto-scroll should stay pinned to the latest content without easing lag.
        vp.scroll_pos = vp.scroll_target as f32;
    }
    vp.scroll_target = vp.scroll_target.min(max_scroll);

    if !vp.auto_scroll {
        let target = vp.scroll_target as f32;
        let delta = target - vp.scroll_pos;
        if reduced_motion || delta.abs() < 0.01 {
            vp.scroll_pos = target;
        } else {
            vp.scroll_pos += delta * 0.3;
        }
    }
    vp.scroll_offset = vp.scroll_pos.round() as usize;
    clamp_scroll_to_content(vp, max_scroll, reduced_motion);

    let scroll_offset = vp.scroll_offset;
    crate::perf::mark_with("chat::max_scroll", "rows", max_scroll);
    crate::perf::mark_with("chat::scroll_offset", "rows", scroll_offset);

    let mut all_lines = Vec::new();
    let stats = {
        let _t = app
            .perf
            .as_ref()
            .map(|p| p.start_with("chat::render_msgs", "msgs", app.messages().len()));
        render_culled_messages(app, base, width, scroll_offset, viewport_height, &mut all_lines)
    };
    crate::perf::mark_with("chat::render_scrolled_lines", "lines", all_lines.len());
    crate::perf::mark_with("chat::render_scrolled_msgs", "msgs", stats.rendered_msgs);
    crate::perf::mark_with("chat::render_scrolled_first_visible", "idx", stats.first_visible);
    crate::perf::mark_with("chat::render_scrolled_start", "idx", stats.render_start);

    let paragraph = {
        let _t = app
            .perf
            .as_ref()
            .map(|p| p.start_with("chat::paragraph_build", "lines", all_lines.len()));
        Paragraph::new(Text::from(all_lines)).wrap(Wrap { trim: false })
    };

    ScrolledRenderData { paragraph, stats, max_scroll, scroll_offset }
}

/// Long content: smooth scroll + viewport culling.
fn render_scrolled(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    base: &SpinnerState,
    width: u16,
    content_height: usize,
    viewport_height: usize,
) {
    let _t = app.perf.as_ref().map(|p| p.start("chat::render_scrolled"));
    let render_data = build_scrolled_render_data(app, base, width, content_height, viewport_height);
    let pinned_to_bottom = render_data.scroll_offset == render_data.max_scroll;
    emit_render_summary(
        app,
        width,
        content_height,
        viewport_height,
        pinned_to_bottom,
        &render_data,
    );

    app.rendered_chat_area = area;
    {
        let _t = app
            .perf
            .as_ref()
            .map(|p| p.start_with("chat::render_widget", "scroll", render_data.stats.local_scroll));
        frame.render_widget(
            render_data
                .paragraph
                .scroll((paragraph_scroll_offset(render_data.stats.local_scroll), 0)),
            area,
        );
    }
}

pub(super) fn refresh_selection_snapshot(app: &mut App) {
    if !chat_selection_snapshot_needed(app.selection().copied()) {
        return;
    }

    let area = app.rendered_chat_area;
    if area.width == 0 || area.height == 0 {
        return;
    }

    let base_spinner = build_base_spinner(app);
    let content_height = sync_chat_layout(app, area, &base_spinner);
    let _t = app.perf.as_ref().map(|p| p.start("chat::selection_capture"));
    let render_data = build_scrolled_render_data(
        app,
        &base_spinner,
        area.width,
        content_height,
        usize::from(area.height),
    );
    app.rendered_chat_area = area;
    app.rendered_chat_lines =
        render_lines_from_paragraph(&render_data.paragraph, area, render_data.stats.local_scroll);
}

fn chat_selection_snapshot_needed(selection: Option<SelectionState>) -> bool {
    selection.is_some_and(|selection| selection.kind == SelectionKind::Chat)
}

fn paragraph_scroll_offset(scroll_offset: usize) -> u16 {
    u16::try_from(scroll_offset).unwrap_or_else(|_| {
        tracing::warn!(
            target: crate::logging::targets::APP_RENDER,
            event_name = "render_scroll_clamped",
            message = "chat paragraph scroll exceeded the ratatui u16 boundary",
            outcome = "clamped",
            scroll_offset,
            max_scroll = u16::MAX,
        );
        u16::MAX
    })
}

fn clamp_scroll_to_content(
    viewport: &mut crate::app::ChatViewport,
    max_scroll: usize,
    reduced_motion: bool,
) {
    viewport.scroll_target = viewport.scroll_target.min(max_scroll);

    // Shrinks can leave the smoothed scroll position beyond new content end.
    // Ease it back toward the valid bound while keeping rendered offset clamped.
    let max_scroll_f = max_scroll as f32;
    if viewport.scroll_pos > max_scroll_f {
        if reduced_motion {
            viewport.scroll_pos = max_scroll_f;
        } else {
            let overshoot = viewport.scroll_pos - max_scroll_f;
            viewport.scroll_pos = max_scroll_f + overshoot * OVERSCROLL_CLAMP_EASE;
            if (viewport.scroll_pos - max_scroll_f).abs() < SCROLLBAR_EASE_EPSILON {
                viewport.scroll_pos = max_scroll_f;
            }
        }
    }

    viewport.scroll_offset = (viewport.scroll_pos.round() as usize).min(max_scroll);
    if viewport.scroll_offset >= max_scroll {
        viewport.auto_scroll = true;
    }
}

fn ease_value(current: &mut f32, target: f32, factor: f32) {
    let delta = target - *current;
    if delta.abs() < SCROLLBAR_EASE_EPSILON {
        *current = target;
    } else {
        *current += delta * factor;
    }
}

/// Clamp the raw `viewport² / content` thumb to a fixed maximum and
/// rebuild `thumb_top` against the post-cap track length. Without
/// this, a chat with content just barely overflowing the viewport
/// renders a thumb that takes up half the rail - visually noisy and
/// inconsistent with the Inspector pane's tiny indicator. Capping
/// keeps the chat scrollbar a stable small dot regardless of how
/// much (or little) of the scrollback overflows.
fn cap_scrollbar_target(
    raw: ScrollbarGeometry,
    viewport_height: usize,
    scroll_pos: f32,
) -> ScrollbarGeometry {
    let thumb_size = raw.thumb_size.clamp(SCROLLBAR_MIN_THUMB_HEIGHT, SCROLLBAR_MAX_THUMB_HEIGHT);
    let track_space = viewport_height.saturating_sub(thumb_size);
    let max_scroll = raw.max_scroll;
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let thumb_top = if max_scroll == 0 || track_space == 0 {
        0
    } else {
        ((scroll_pos.clamp(0.0, max_scroll as f32) / max_scroll as f32) * track_space as f32)
            .round() as usize
    };
    ScrollbarGeometry { thumb_top, thumb_size, track_space, max_scroll }
}

fn smooth_scrollbar_geometry(
    viewport: &mut crate::app::ChatViewport,
    target: ScrollbarGeometry,
    viewport_height: usize,
    reduced_motion: bool,
) -> ScrollbarGeometry {
    let target_top = target.thumb_top as f32;
    let target_size = target.thumb_size as f32;

    if reduced_motion || viewport.scrollbar_thumb_size <= 0.0 {
        viewport.scrollbar_thumb_top = target_top;
        viewport.scrollbar_thumb_size = target_size;
    } else {
        ease_value(&mut viewport.scrollbar_thumb_top, target_top, SCROLLBAR_TOP_EASE);
        ease_value(&mut viewport.scrollbar_thumb_size, target_size, SCROLLBAR_SIZE_EASE);
    }

    let mut thumb_size = viewport.scrollbar_thumb_size.round() as usize;
    thumb_size = thumb_size.max(SCROLLBAR_MIN_THUMB_HEIGHT).min(viewport_height);
    let max_top = viewport_height.saturating_sub(thumb_size);
    let thumb_top = viewport.scrollbar_thumb_top.round().clamp(0.0, max_top as f32) as usize;

    ScrollbarGeometry {
        thumb_top,
        thumb_size,
        track_space: viewport_height.saturating_sub(thumb_size),
        max_scroll: target.max_scroll,
    }
}
fn render_scrollbar_overlay(
    frame: &mut Frame,
    viewport: &mut crate::app::ChatViewport,
    reduced_motion: bool,
    area: Rect,
    content_height: usize,
    viewport_height: usize,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Thumb only - no rail. The dim `▕` rail looked visually busy
    // sitting against the Inspector pane to its right; the thumb
    // alone is enough to indicate scroll position when content
    // overflows. When the content fits the whole right column is
    // empty.
    let Some(raw_target) = crate::app::compute_scrollbar_geometry(
        content_height,
        viewport_height,
        viewport.scroll_pos,
    ) else {
        viewport.scrollbar_thumb_top = 0.0;
        viewport.scrollbar_thumb_size = 0.0;
        return;
    };
    // Cap the thumb size and rebuild thumb_top against the
    // post-cap track length so a short scrollback doesn't render a
    // thumb that covers half the rail. Same pattern as the
    // Inspector pane's `INSPECTOR_THUMB_MAX_CELLS` clamp.
    let target = cap_scrollbar_target(raw_target, viewport_height, viewport.scroll_pos);
    let geometry = smooth_scrollbar_geometry(viewport, target, viewport_height, reduced_motion);
    let thumb_style = Style::default().fg(theme::ROLE_ASSISTANT);
    let rail_x = area.right().saturating_sub(1);
    let thumb_top = geometry.thumb_top.min(area.height.saturating_sub(1) as usize);
    let thumb_end = thumb_top.saturating_add(geometry.thumb_size).min(area.height as usize);
    let buf = frame.buffer_mut();
    for row in thumb_top..thumb_end {
        let y = area.y.saturating_add(row as u16);
        if let Some(cell) = buf.cell_mut((rail_x, y)) {
            cell.set_symbol("\u{2590}");
            cell.set_style(thumb_style);
        }
    }
}
/// Render only the visible message range into out (viewport culling).
/// Returns the local scroll offset to pass to `Paragraph::scroll()`.
fn render_culled_messages(
    app: &mut App,
    base: &SpinnerState,
    width: u16,
    scroll: usize,
    viewport_height: usize,
    out: &mut Vec<Line<'static>>,
) -> CulledRenderStats {
    let msg_count = app.messages().len();
    let active_turn_assistant = app.active_turn_assistant_idx();

    // O(log n) binary search via prefix sums to find first visible message.
    let first_visible = app.active_viewport_mut().find_first_visible(scroll);

    // Apply margin: render a few extra messages above/below for safety
    let render_start = first_visible.saturating_sub(CULLING_MARGIN);

    // O(1) cumulative height lookup via prefix sums
    let height_before_start = app.active_viewport_mut().cumulative_height_before(render_start);

    // Render messages from render_start onward, stopping once the exact wrapped
    // height in the output buffer covers the viewport plus a small overscan.
    let mut structural_skip = scroll.saturating_sub(height_before_start);
    let rows_needed = structural_skip + viewport_height + CULLING_OVERSCAN_ROWS;
    crate::perf::mark_with("chat::cull_lines_needed", "lines", rows_needed);
    let mut rendered_msgs = 0usize;
    let mut local_scroll = 0usize;
    let mut rendered_rows = 0usize;
    let mut last_rendered_idx = None;
    // Snapshot loop-invariant fields once - hoisting avoids N
    // String allocations on remeasure-heavy frames.
    let mode_id_owned = app.mode().map(|mode| mode.current_mode_id.clone());
    let mode_id = mode_id_owned.as_deref();
    let layout_generation = app.viewport().layout_generation;
    let tools_collapsed = app.tools_collapsed;
    let group_collapse_levels =
        app.active_session().map(|s| s.group_collapse_levels.clone()).unwrap_or_default();
    let messaging_group_collapse_levels =
        app.active_session().map(|s| s.messaging_group_collapse_levels.clone()).unwrap_or_default();
    // Cross-message peer/worker run state: compute the session-level
    // partition once per render pass. Each per-message render call
    // reads its own slice via the context's
    // `with_session_message_units` builder so MessagingGroup segments
    // know their cross-turn continuation flags + shared leader id.
    let session_units = message::grouping::partition_session_into_render_units(app.messages());
    for i in render_start..msg_count {
        let sp = msg_spinner(base, i, active_turn_assistant, &app.messages()[i]);
        let before = out.len();
        let message_height = app.viewport().message_height(i);
        let suppress_group_header = message::compute_suppress_group_header(app.messages(), i);
        let envelope_streak_position = message::compute_envelope_streak_position(app.messages(), i);
        let stop_hook = stop_hook_summary_for(app, i);
        let options = message::MessageRenderOptions {
            tools_collapsed,
            include_trailing_separator: i + 1 != msg_count,
            suppress_group_header,
            envelope_streak_position,
            stop_hook_summary_actions: stop_hook.actions,
            stop_hook_summary_expanded: stop_hook.expanded,
        };
        let empty_session_units: Vec<message::grouping::RenderUnit> = Vec::new();
        let session_message_units = session_units.get(i).unwrap_or(&empty_session_units);
        let ctx = message::MessageRenderContext::new(mode_id, width, layout_generation, options)
            .with_stop_hook_hooks(stop_hook.hooks.as_slice())
            .with_group_collapse_levels(&group_collapse_levels)
            .with_messaging_group_collapse_levels(&messaging_group_collapse_levels)
            .with_session_message_units(session_message_units);
        if structural_skip > 0 {
            let remaining_skip = message::render_message_from_offset_internal_with_mode(
                &mut app.active_messages_mut()[i],
                &sp,
                ctx,
                structural_skip,
                out,
            );
            let structural_rows_skipped = structural_skip.saturating_sub(remaining_skip);
            rendered_rows = rendered_rows
                .saturating_add(message_height.saturating_sub(structural_rows_skipped));
            local_scroll = remaining_skip;
            structural_skip = 0;
        } else {
            message::render_message(&mut app.active_messages_mut()[i], &sp, ctx, out);
            rendered_rows = rendered_rows.saturating_add(message_height);
        }
        app.sync_render_cache_message(i);
        if out.len() > before {
            rendered_msgs += 1;
            last_rendered_idx = Some(i);
        }
        if rendered_rows > rows_needed {
            break;
        }
    }

    CulledRenderStats {
        local_scroll,
        first_visible,
        render_start,
        rendered_msgs,
        last_rendered_idx,
        rendered_line_count: out.len(),
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let _t = app.perf.as_ref().map(|p| p.start("chat::render"));
    crate::perf::mark_with("chat::message_count", "msgs", app.messages().len());
    let content_area = chat_content_area(area);
    let width = content_area.width;
    let viewport_height = content_area.height as usize;
    let base_spinner = build_base_spinner(app);
    let content_height = sync_chat_layout(app, content_area, &base_spinner);

    if content_height <= viewport_height {
        crate::perf::mark_with("chat::path_short", "active", 1);
    } else {
        crate::perf::mark_with("chat::path_scrolled", "active", 1);
    }

    render_scrolled(
        frame,
        content_area,
        app,
        &base_spinner,
        width,
        content_height,
        viewport_height,
    );

    if let Some(sel) = app.selection().copied()
        && sel.kind == SelectionKind::Chat
    {
        frame.render_widget(SelectionOverlay { selection: sel }, app.rendered_chat_area);
    }

    if let Some(scrollbar_area) = chat_scrollbar_area(area) {
        let reduced_motion = app.config.prefers_reduced_motion_effective();
        render_scrollbar_overlay(
            frame,
            app.active_viewport_mut(),
            reduced_motion,
            scrollbar_area,
            content_height,
            viewport_height,
        );
    }

    enforce_and_emit_cache_metrics(app);
}

fn emit_render_summary(
    app: &mut App,
    width: u16,
    content_height: usize,
    viewport_height: usize,
    pinned_to_bottom: bool,
    render_data: &ScrolledRenderData,
) {
    let last_message_idx = app.messages().len().checked_sub(1);
    let last_message_height =
        last_message_idx.map(|idx| app.active_viewport_mut().message_height(idx));
    let trace_state = crate::app::ChatRenderTraceState {
        width,
        content_height,
        viewport_height,
        auto_scroll: app.viewport().auto_scroll,
        pinned_to_bottom,
        scroll_target: app.viewport().scroll_target,
        scroll_offset: render_data.scroll_offset,
        max_scroll: render_data.max_scroll,
        first_visible: render_data.stats.first_visible,
        render_start: render_data.stats.render_start,
        local_scroll: render_data.stats.local_scroll,
        rendered_msgs: render_data.stats.rendered_msgs,
        last_rendered_idx: render_data.stats.last_rendered_idx,
        rendered_line_count: render_data.stats.rendered_line_count,
        last_message_idx,
        last_message_height,
        selection_snapshot_active: chat_selection_snapshot_needed(app.selection().copied()),
    };
    if !remember_render_trace_state(app, trace_state) {
        return;
    }
    tracing::trace!(
        target: crate::logging::targets::APP_RENDER,
        event_name = "chat_render_summary",
        message = "chat render summary emitted",
        outcome = "success",
        width,
        content_height,
        viewport_height,
        auto_scroll = trace_state.auto_scroll,
        pinned_to_bottom = trace_state.pinned_to_bottom,
        scroll_target = ?app.viewport().scroll_target,
        scroll_pos = app.viewport().scroll_pos,
        scroll_offset = trace_state.scroll_offset,
        max_scroll = trace_state.max_scroll,
        first_visible = trace_state.first_visible,
        render_start = trace_state.render_start,
        local_scroll = trace_state.local_scroll,
        rendered_msgs = trace_state.rendered_msgs,
        last_rendered_idx = ?trace_state.last_rendered_idx,
        rendered_line_count = trace_state.rendered_line_count,
        last_message_idx = ?last_message_idx,
        last_message_height = ?last_message_height,
        selection_snapshot_active = trace_state.selection_snapshot_active,
    );
}

fn remember_render_trace_state(
    app: &mut App,
    trace_state: crate::app::ChatRenderTraceState,
) -> bool {
    if app.last_chat_render_trace_state() == Some(trace_state) {
        return false;
    }
    app.set_last_chat_render_trace_state(Some(trace_state));
    true
}

fn enforce_and_emit_cache_metrics(app: &mut App) {
    let budget_stats = app.enforce_render_cache_budget();
    crate::perf::mark_with("cache::bytes_before", "bytes", budget_stats.total_before_bytes);
    crate::perf::mark_with("cache::bytes_after", "bytes", budget_stats.total_after_bytes);
    crate::perf::mark_with("cache::protected_bytes", "bytes", budget_stats.protected_bytes);
    crate::perf::mark_with("cache::evicted_bytes", "bytes", budget_stats.evicted_bytes);
    crate::perf::mark_with("cache::evicted_blocks", "count", budget_stats.evicted_blocks);

    // -- Accumulate and conditionally log render cache metrics --
    let render_cache_budget = app.render_cache_budget;
    let history_policy = app.history_retention();
    let should_log =
        app.cache_metrics_mut().record_render_enforcement(&budget_stats, &render_cache_budget);

    let render_utilization_pct = if render_cache_budget.max_bytes > 0 {
        (render_cache_budget.last_total_bytes as f32 / render_cache_budget.max_bytes as f32) * 100.0
    } else {
        0.0
    };
    let history_utilization_pct = if history_policy.max_bytes > 0 {
        (app.history_retention_stats().total_after_bytes as f32 / history_policy.max_bytes as f32)
            * 100.0
    } else {
        0.0
    };

    if let Some(warn_kind) = app.cache_metrics_mut().check_warn_condition(
        render_utilization_pct,
        history_utilization_pct,
        budget_stats.evicted_blocks,
    ) {
        cache_metrics::emit_cache_warning(&warn_kind);
    }

    if should_log {
        let entry_count = count_populated_cache_slots(app.messages());
        let snap = cache_metrics::build_snapshot(
            &render_cache_budget,
            app.history_retention_stats(),
            history_policy,
            app.cache_metrics(),
            app.viewport(),
            entry_count,
            budget_stats.evicted_blocks,
            0,
            budget_stats.protected_bytes,
        );
        cache_metrics::emit_render_metrics(&snap);

        crate::perf::mark_with("cache::entry_count", "count", entry_count);
        crate::perf::mark_with(
            "cache::utilization_pct_x10",
            "pct",
            (snap.render_utilization_pct * 10.0) as usize,
        );
        crate::perf::mark_with("cache::peak_bytes", "bytes", snap.render_peak_bytes);
    }
}

/// Count cache slots with non-zero cached bytes across all message blocks.
///
/// Only called on log cadence (~every 60 frames), not per-frame.
fn count_populated_cache_slots(messages: &[crate::app::ChatMessage]) -> usize {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter(|block| match block {
            MessageBlock::Text(block) => block.cache.cached_bytes() > 0,
            MessageBlock::Notice(block) => block.text.cache.cached_bytes() > 0,
            MessageBlock::Welcome(w) => w.cache.cached_bytes() > 0,
            MessageBlock::ToolCall(tc) => tc.cache.cached_bytes() > 0,
            MessageBlock::ImageAttachment(img) => img.cache.cached_bytes() > 0,
        })
        .count()
}

struct SelectionOverlay {
    selection: SelectionState,
}

impl Widget for SelectionOverlay {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (start, end) =
            crate::app::normalize_selection(self.selection.start, self.selection.end);
        for row in start.row..=end.row {
            let y = area.y.saturating_add(row as u16);
            if y >= area.bottom() {
                break;
            }
            let row_start = if row == start.row { start.col } else { 0 };
            let row_end = if row == end.row { end.col } else { area.width as usize };
            for col in row_start..row_end {
                let x = area.x.saturating_add(col as u16);
                if x >= area.right() {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                }
            }
        }
    }
}

fn render_lines_from_paragraph(
    paragraph: &Paragraph,
    area: Rect,
    scroll_offset: usize,
) -> Vec<String> {
    let mut buf = Buffer::empty(area);
    let widget = paragraph.clone().scroll((paragraph_scroll_offset(scroll_offset), 0));
    widget.render(area, &mut buf);
    let mut lines = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        let mut line = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((area.x + x, area.y + y)) {
                line.push_str(cell.symbol());
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{
        SCROLLBAR_MIN_THUMB_HEIGHT, chat_content_area, clamp_scroll_to_content,
        paragraph_scroll_offset, render_culled_messages, render_lines_from_paragraph,
        render_scrolled, smooth_scrollbar_geometry, update_visual_heights,
    };
    use crate::app::{
        App, AppStatus, ChatMessage, ChatViewport, InvalidationLevel, MessageBlock, MessageRole,
        ScrollbarGeometry, SelectionKind, SelectionPoint, SelectionState, SystemSeverity,
        TextBlock, compute_scrollbar_geometry,
    };
    use crate::ui::message::{self, SpinnerState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::text::Text;
    use ratatui::widgets::{Paragraph, Wrap};

    fn assistant_text_message(text: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::Text(TextBlock::from_complete(text))],
            None,
        )
    }

    fn user_message(text: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete(text))],
            None,
        )
    }

    fn system_message(text: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::System(Some(SystemSeverity::Info)),
            vec![MessageBlock::Text(TextBlock::from_complete(text))],
            None,
        )
    }

    fn idle_spinner() -> SpinnerState {
        SpinnerState {
            frame: 0,
            is_active_turn_assistant: false,
            show_empty_thinking: false,
            show_thinking: false,
            show_compacting: false,
            thinking_tokens: None,
            running_subagents: None,
        }
    }

    fn render_selected_chat_snapshot(app: &mut App, width: u16, height: u16) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let spinner = idle_spinner();
                let content_area = chat_content_area(Rect::new(0, 0, width, height));
                let _ = app.active_viewport_mut().on_frame(content_area.width, content_area.height);
                update_visual_heights(
                    app,
                    &spinner,
                    content_area.width,
                    usize::from(content_area.height),
                );
                app.active_viewport_mut().rebuild_prefix_sums();
                let total_h = app.viewport().total_message_height();
                render_scrolled(
                    frame,
                    content_area,
                    app,
                    &spinner,
                    content_area.width,
                    total_h,
                    usize::from(content_area.height),
                );
            })
            .expect("draw");
        super::refresh_selection_snapshot(app);
    }

    #[test]
    fn derive_running_subagents_returns_none_without_dispatch() {
        let app = App::test_default();
        assert!(super::derive_running_subagents(&app).is_none());
    }

    #[test]
    fn build_base_spinner_carries_no_running_subagents_by_default() {
        let mut app = App::test_default();
        app.status = AppStatus::Running;
        let spinner = super::build_base_spinner(&app);
        assert!(spinner.running_subagents.is_none());
        assert!(spinner.show_thinking, "thinking remains independent of the subagent surface");
    }

    /// The bug this PR closes: an off-screen `MessageChanged`
    /// invalidation must NOT trigger per-frame background
    /// re-measurement work. Today (without the convergence-flag
    /// gate) the resize loop wakes on any active plan and chews up
    /// to `viewport_height` off-screen stale messages, paying full
    /// layout cost on each. In a streaming session with Bash output
    /// + Monitor refreshes that's ~20 measurements per frame for a
    /// user who only sees 1-3 messages.
    #[test]
    fn off_screen_invalidate_does_not_force_resize_loop_to_remeasure() {
        let mut app = App::test_default();
        app.status = AppStatus::Ready;
        let text = "assistant reply that wraps over a line or two for height\n\
                    so heights vary between consecutive messages";
        let history: Vec<ChatMessage> =
            (0..200).map(|_| assistant_text_message(text)).collect();
        *app.active_messages_mut() = history;

        let _ = app.active_viewport_mut().on_frame(80, 8);
        let spinner = idle_spinner();
        for _ in 0..64 {
            update_visual_heights(&mut app, &spinner, 80, 8);
            app.active_viewport_mut().rebuild_prefix_sums();
            if !app.active_viewport_mut().resize_remeasure_active() {
                break;
            }
        }
        assert!(
            !app.active_viewport_mut().resize_remeasure_active(),
            "setup must fully converge so background_convergence_pending is clear before the test fires",
        );

        let max_scroll = app.active_viewport_mut().total_message_height().saturating_sub(8);
        let vp = app.active_viewport_mut();
        vp.scroll_target = max_scroll;
        vp.scroll_pos = max_scroll as f32;
        vp.scroll_offset = max_scroll;
        vp.auto_scroll = true;

        let off_screen_idx = 12;
        app.invalidate_layout(InvalidationLevel::MessageChanged(off_screen_idx));

        let frame = update_visual_heights(&mut app, &spinner, 80, 8);
        assert_eq!(
            frame.measured_msgs, 0,
            "off-screen MessageChanged invalidate must not drive per-frame re-measurement; got measured={} reused={}",
            frame.measured_msgs,
            frame.reused_msgs,
        );
        assert!(
            !app.active_viewport_mut().message_height_is_current(off_screen_idx),
            "the off-screen target stays stale (re-measures lazily when scrolled in)",
        );
    }

    /// Bootstrap of a long session must NOT relocate the O(history)
    /// cost into the first frame: the priority + visible loops should
    /// stop after measuring roughly a viewport-worth of messages, and
    /// the remaining off-screen tail converges over later frames via
    /// the budgeted background loop. Without this, a fresh /resume
    /// of a 1000-message session would be one all-N-measured frame.
    #[test]
    fn bootstrap_measures_visible_only_not_all_history() {
        let mut app = App::test_default();
        app.status = AppStatus::Ready;
        let history: Vec<ChatMessage> = (0..60).map(|i| {
            assistant_text_message(&format!(
                "msg {i}: some content that wraps a bit so heights are non-trivial\nsecond line of content",
            ))
        }).collect();
        *app.active_messages_mut() = history;

        let _ = app.active_viewport_mut().on_frame(80, 24);
        let spinner = idle_spinner();
        let stats = update_visual_heights(&mut app, &spinner, 80, 24);
        assert!(stats.measured_msgs > 0, "must measure at least one message");
        assert!(
            stats.measured_msgs < 30,
            "bootstrap must be visible-first; expected fewer than half the history measured on frame 1, got {} of 60",
            stats.measured_msgs,
        );
    }

    #[test]
    fn spinner_only_frames_do_not_remeasure_active_assistant_height() {
        let mut app = App::test_default();
        app.status = AppStatus::Running;
        *app.active_messages_mut() = vec![assistant_text_message("streaming body")];
        app.bind_active_turn_assistant(0);

        let _ = app.active_viewport_mut().on_frame(40, 8);
        let first_spinner = SpinnerState { frame: 0, show_thinking: true, ..idle_spinner() };
        let first = update_visual_heights(&mut app, &first_spinner, 40, 8);
        assert_eq!(first.measured_msgs, 1);

        let second_spinner = SpinnerState { frame: 1, show_thinking: true, ..idle_spinner() };
        let second = update_visual_heights(&mut app, &second_spinner, 40, 8);
        assert_eq!(second.measured_msgs, 0);
    }

    #[test]
    fn scrollbar_hidden_when_content_fits() {
        assert_eq!(compute_scrollbar_geometry(10, 10, 0.0), None);
        assert_eq!(compute_scrollbar_geometry(8, 10, 0.0), None);
    }
    #[test]
    fn scrollbar_thumb_positions_are_stable() {
        assert_eq!(
            compute_scrollbar_geometry(50, 10, 0.0),
            Some(ScrollbarGeometry { thumb_top: 0, thumb_size: 2, track_space: 8, max_scroll: 40 })
        );
        assert_eq!(
            compute_scrollbar_geometry(50, 10, 20.0),
            Some(ScrollbarGeometry { thumb_top: 4, thumb_size: 2, track_space: 8, max_scroll: 40 })
        );
        assert_eq!(
            compute_scrollbar_geometry(50, 10, 40.0),
            Some(ScrollbarGeometry { thumb_top: 8, thumb_size: 2, track_space: 8, max_scroll: 40 })
        );
    }
    #[test]
    fn scrollbar_scroll_offset_is_clamped() {
        assert_eq!(
            compute_scrollbar_geometry(50, 10, 999.0),
            Some(ScrollbarGeometry { thumb_top: 8, thumb_size: 2, track_space: 8, max_scroll: 40 })
        );
    }
    #[test]
    fn scrollbar_handles_small_overflow() {
        assert_eq!(
            compute_scrollbar_geometry(11, 10, 1.0),
            Some(ScrollbarGeometry { thumb_top: 1, thumb_size: 9, track_space: 1, max_scroll: 1 })
        );
    }
    #[test]
    fn scrollbar_respects_min_thumb_height() {
        assert_eq!(
            compute_scrollbar_geometry(10_000, 10, 0.0),
            Some(ScrollbarGeometry {
                thumb_top: 0,
                thumb_size: SCROLLBAR_MIN_THUMB_HEIGHT,
                track_space: 9,
                max_scroll: 9_990,
            })
        );
    }

    #[test]
    fn chat_content_area_reserves_one_column_for_scrollbar() {
        assert_eq!(chat_content_area(Rect::new(0, 0, 20, 10)), Rect::new(0, 0, 19, 10));
        assert_eq!(chat_content_area(Rect::new(3, 4, 1, 5)), Rect::new(3, 4, 0, 5));
    }

    #[test]
    fn update_visual_heights_remeasures_dirty_non_tail_message() {
        let mut app = App::test_default();
        app.status = AppStatus::Ready;
        *app.active_messages_mut() =
            vec![assistant_text_message("short"), assistant_text_message("tail stays unchanged")];

        let _ = app.active_viewport_mut().on_frame(12, 8);
        let spinner = idle_spinner();

        update_visual_heights(&mut app, &spinner, 12, 8);
        let base_h = app.active_viewport_mut().message_height(0);
        assert!(base_h > 0);

        if let Some(MessageBlock::Text(block)) =
            app.active_messages_mut().get_mut(0).and_then(|m| m.blocks.get_mut(0))
        {
            let extra = " this now wraps across multiple lines";
            block.text.push_str(extra);
            block.markdown.append(extra);
            block.cache.invalidate();
        }
        app.invalidate_layout(InvalidationLevel::MessagesFrom(0));

        update_visual_heights(&mut app, &spinner, 12, 8);
        assert!(
            app.active_viewport_mut().message_height(0) > base_h,
            "dirty non-tail message should be remeasured"
        );
    }

    #[test]
    fn last_message_height_omits_trailing_separator() {
        let mut app = App::test_default();
        app.status = AppStatus::Ready;
        *app.active_messages_mut() = vec![assistant_text_message("hello")];

        let _ = app.active_viewport_mut().on_frame(40, 8);
        let spinner = idle_spinner();

        update_visual_heights(&mut app, &spinner, 40, 8);
        app.active_viewport_mut().rebuild_prefix_sums();

        assert_eq!(app.active_viewport_mut().message_height(0), 2);
        assert_eq!(app.active_viewport_mut().total_message_height(), 2);
    }

    #[test]
    fn active_turn_assistant_owns_thinking_when_system_message_trails() {
        let mut app = App::test_default();
        app.status = AppStatus::Thinking;
        *app.active_messages_mut() = vec![
            assistant_text_message("older reply"),
            user_message("next prompt"),
            ChatMessage::new(MessageRole::Assistant, Vec::new(), None),
            system_message("rate limit warning"),
        ];
        app.bind_active_turn_assistant(2);

        assert_eq!(app.active_turn_assistant_idx(), Some(2));

        let _ = app.active_viewport_mut().on_frame(40, 8);
        let spinner = SpinnerState { show_empty_thinking: true, ..idle_spinner() };

        update_visual_heights(&mut app, &spinner, 40, 8);
        app.active_viewport_mut().rebuild_prefix_sums();

        assert_eq!(
            app.active_viewport_mut().message_height(2),
            3,
            "active assistant should render label + thinking + separator even when a system row trails"
        );
    }

    #[test]
    fn active_turn_assistant_uses_explicit_owner_without_user_anchor() {
        let mut app = App::test_default();
        app.status = AppStatus::Thinking;
        *app.active_messages_mut() = vec![
            assistant_text_message("older reply"),
            ChatMessage::new(MessageRole::Assistant, Vec::new(), None),
            system_message("status"),
        ];
        app.bind_active_turn_assistant(1);

        assert_eq!(app.active_turn_assistant_idx(), Some(1));
    }

    #[test]
    fn appending_message_remeasures_previous_tail_separator() {
        let mut app = App::test_default();
        app.status = AppStatus::Ready;
        app.push_message_tracked(assistant_text_message("first reply"));

        let _ = app.active_viewport_mut().on_frame(40, 8);
        let spinner = idle_spinner();

        update_visual_heights(&mut app, &spinner, 40, 8);
        app.active_viewport_mut().rebuild_prefix_sums();
        assert_eq!(app.active_viewport_mut().message_height(0), 2);
        assert_eq!(app.active_viewport_mut().total_message_height(), 2);

        app.push_message_tracked(user_message("follow-up"));

        update_visual_heights(&mut app, &spinner, 40, 8);
        app.active_viewport_mut().rebuild_prefix_sums();
        assert_eq!(app.active_viewport_mut().message_height(0), 3);
        assert_eq!(app.active_viewport_mut().message_height(1), 2);
        assert_eq!(app.active_viewport_mut().total_message_height(), 5);
    }

    #[test]
    fn removing_tail_message_remeasures_new_last_separator() {
        let mut app = App::test_default();
        app.status = AppStatus::Ready;
        app.push_message_tracked(assistant_text_message("first reply"));
        app.push_message_tracked(user_message("follow-up"));

        let _ = app.active_viewport_mut().on_frame(40, 8);
        let spinner = idle_spinner();

        update_visual_heights(&mut app, &spinner, 40, 8);
        app.active_viewport_mut().rebuild_prefix_sums();
        assert_eq!(app.active_viewport_mut().message_height(0), 3);
        assert_eq!(app.active_viewport_mut().message_height(1), 2);

        let removed = app.remove_message_tracked(1);
        assert!(removed.is_some());

        update_visual_heights(&mut app, &spinner, 40, 8);
        app.active_viewport_mut().rebuild_prefix_sums();
        assert_eq!(app.active_viewport_mut().message_height(0), 2);
        assert_eq!(app.active_viewport_mut().total_message_height(), 2);
    }

    #[test]
    fn resize_remeasure_updates_visible_window_before_far_messages() {
        let mut app = App::test_default();
        let text = "This message should wrap after resize and stay expensive enough to measure. "
            .repeat(6);
        *app.active_messages_mut() = (0..32).map(|_| assistant_text_message(&text)).collect();

        let spinner = idle_spinner();

        let _ = app.active_viewport_mut().on_frame(48, 12);
        for _ in 0..16 {
            update_visual_heights(&mut app, &spinner, 48, 12);
            app.active_viewport_mut().rebuild_prefix_sums();
            if !app.active_viewport_mut().resize_remeasure_active() {
                break;
            }
        }
        assert!(
            !app.active_viewport_mut().resize_remeasure_active(),
            "frame-1 setup must fully measure the initial scrollback before mid-scroll resize",
        );
        let per_message_height = app.active_viewport_mut().message_height(0);
        assert!(per_message_height > 0);

        let visible_rows = per_message_height * 2;
        app.active_viewport_mut().scroll_offset = per_message_height * 15;
        app.active_viewport_mut().scroll_target = app.viewport().scroll_offset;
        app.active_viewport_mut().scroll_pos = app.viewport().scroll_offset as f32;

        assert!(app.active_viewport_mut().on_frame(18, 12).width_changed);
        update_visual_heights(&mut app, &spinner, 18, visible_rows);

        assert_eq!(app.viewport().message_heights_width, 0);
        assert!(app.active_viewport_mut().resize_remeasure_active());
        assert!(app.active_viewport_mut().message_height_is_current(15));
        assert!(app.active_viewport_mut().message_height_is_current(16));
        assert!(!app.active_viewport_mut().message_height_is_current(31));
    }

    #[test]
    fn resize_remeasure_converges_over_multiple_frames() {
        let mut app = App::test_default();
        let text = "This message should wrap after resize and stay expensive enough to measure. "
            .repeat(6);
        *app.active_messages_mut() = (0..40).map(|_| assistant_text_message(&text)).collect();

        let spinner = idle_spinner();

        let _ = app.active_viewport_mut().on_frame(48, 12);
        update_visual_heights(&mut app, &spinner, 48, 12);
        app.active_viewport_mut().rebuild_prefix_sums();
        let per_message_height = app.active_viewport_mut().message_height(0);
        app.active_viewport_mut().scroll_offset = per_message_height * 12;
        app.active_viewport_mut().scroll_target = app.viewport().scroll_offset;
        app.active_viewport_mut().scroll_pos = app.viewport().scroll_offset as f32;

        assert!(app.active_viewport_mut().on_frame(18, 12).width_changed);
        for _ in 0..8 {
            update_visual_heights(&mut app, &spinner, 18, per_message_height * 2);
            app.active_viewport_mut().rebuild_prefix_sums();
            if !app.active_viewport_mut().resize_remeasure_active() {
                break;
            }
        }

        assert_eq!(app.viewport().message_heights_width, 18);
        assert!(!app.active_viewport_mut().resize_remeasure_active());
        assert!(app.active_viewport_mut().message_height_is_current(0));
        assert!(app.active_viewport_mut().message_height_is_current(39));
    }

    #[test]
    fn resize_remeasure_does_not_repeat_dirty_suffix_after_measuring_it() {
        let mut app = App::test_default();
        let text = "This message should wrap after resize and stay expensive enough to measure. "
            .repeat(6);
        *app.active_messages_mut() = (0..8).map(|_| assistant_text_message(&text)).collect();

        let spinner = idle_spinner();

        let _ = app.active_viewport_mut().on_frame(48, 12);
        update_visual_heights(&mut app, &spinner, 48, 12);
        app.active_viewport_mut().rebuild_prefix_sums();
        let per_message_height = app.active_viewport_mut().message_height(0);
        app.active_viewport_mut().scroll_offset = per_message_height * 2;
        app.active_viewport_mut().scroll_target = app.viewport().scroll_offset;
        app.active_viewport_mut().scroll_pos = app.viewport().scroll_offset as f32;

        assert!(app.active_viewport_mut().on_frame(18, 12).width_changed);
        app.invalidate_layout(InvalidationLevel::MessagesFrom(0));

        let first = update_visual_heights(&mut app, &spinner, 18, per_message_height * 2);
        app.active_viewport_mut().rebuild_prefix_sums();
        let second = update_visual_heights(&mut app, &spinner, 18, per_message_height * 2);

        assert!(first.measured_msgs >= app.messages().len());
        assert_eq!(second.measured_msgs, 0);
        assert_eq!(app.viewport().message_heights_width, 18);
    }

    #[test]
    fn render_culled_messages_matches_full_render_when_scrolled_inside_message() {
        let mut app = App::test_default();
        let text = (0..160).map(|i| format!("line {i:03}")).collect::<Vec<_>>().join("\n");
        *app.active_messages_mut() = vec![assistant_text_message(&text)];
        let width = 24u16;
        let viewport_height_u16 = 8u16;
        let viewport_height = usize::from(viewport_height_u16);
        let area = Rect::new(0, 0, width, viewport_height_u16);
        let spinner = idle_spinner();

        let _ = app.active_viewport_mut().on_frame(width, viewport_height_u16);
        update_visual_heights(&mut app, &spinner, width, viewport_height);
        app.active_viewport_mut().rebuild_prefix_sums();

        let scroll = 60;
        let mut full_lines = Vec::new();
        let tools_collapsed = app.tools_collapsed;
        message::render_message(
            &mut app.active_messages_mut()[0],
            &spinner,
            message::MessageRenderContext::new(
                None,
                width,
                0,
                message::MessageRenderOptions {
                    tools_collapsed,
                    include_trailing_separator: false,
                    suppress_group_header: false,
                    envelope_streak_position: None,
                    stop_hook_summary_actions: 0,
                    stop_hook_summary_expanded: false,
                },
            ),
            &mut full_lines,
        );
        let full_preview = render_lines_from_paragraph(
            &Paragraph::new(Text::from(full_lines.clone())).wrap(Wrap { trim: false }),
            area,
            scroll,
        );

        let mut culled_lines = Vec::new();
        let stats = render_culled_messages(
            &mut app,
            &spinner,
            width,
            scroll,
            viewport_height,
            &mut culled_lines,
        );
        let culled_preview = render_lines_from_paragraph(
            &Paragraph::new(Text::from(culled_lines.clone())).wrap(Wrap { trim: false }),
            area,
            stats.local_scroll,
        );

        assert_eq!(culled_preview, full_preview);
        assert!(culled_lines.len() < full_lines.len());
        assert_eq!(stats.rendered_msgs, 1);
    }

    #[test]
    fn render_culled_messages_matches_full_render_when_scrolled_inside_wrapped_role_label() {
        let mut app = App::test_default();
        *app.active_messages_mut() = vec![user_message("ok")];
        let width = 2u16;
        let viewport_height_u16 = 4u16;
        let viewport_height = usize::from(viewport_height_u16);
        let area = Rect::new(0, 0, width, viewport_height_u16);
        let spinner = idle_spinner();

        let _ = app.active_viewport_mut().on_frame(width, viewport_height_u16);
        update_visual_heights(&mut app, &spinner, width, viewport_height);
        app.active_viewport_mut().rebuild_prefix_sums();

        assert!(app.active_viewport_mut().message_height(0) >= 3);

        let scroll = 1;
        let mut full_lines = Vec::new();
        let tools_collapsed = app.tools_collapsed;
        message::render_message(
            &mut app.active_messages_mut()[0],
            &spinner,
            message::MessageRenderContext::new(
                None,
                width,
                0,
                message::MessageRenderOptions {
                    tools_collapsed,
                    include_trailing_separator: false,
                    suppress_group_header: false,
                    envelope_streak_position: None,
                    stop_hook_summary_actions: 0,
                    stop_hook_summary_expanded: false,
                },
            ),
            &mut full_lines,
        );
        let full_preview = render_lines_from_paragraph(
            &Paragraph::new(Text::from(full_lines.clone())).wrap(Wrap { trim: false }),
            area,
            scroll,
        );

        let mut culled_lines = Vec::new();
        let stats = render_culled_messages(
            &mut app,
            &spinner,
            width,
            scroll,
            viewport_height,
            &mut culled_lines,
        );
        let culled_preview = render_lines_from_paragraph(
            &Paragraph::new(Text::from(culled_lines.clone())).wrap(Wrap { trim: false }),
            area,
            stats.local_scroll,
        );

        assert_eq!(culled_preview, full_preview);
        assert_eq!(stats.rendered_msgs, 1);
        assert_eq!(stats.local_scroll, 1);
    }

    #[test]
    fn render_culled_messages_stops_after_first_wrapped_message_when_viewport_is_covered() {
        let mut app = App::test_default();
        let huge_wrapped = "wrap ".repeat(2_000);
        *app.active_messages_mut() = vec![
            assistant_text_message(&huge_wrapped),
            assistant_text_message("this should remain offscreen"),
        ];
        let width = 20u16;
        let viewport_height_u16 = 8u16;
        let viewport_height = usize::from(viewport_height_u16);
        let spinner = idle_spinner();

        let _ = app.active_viewport_mut().on_frame(width, viewport_height_u16);
        update_visual_heights(&mut app, &spinner, width, viewport_height);
        app.active_viewport_mut().rebuild_prefix_sums();

        assert!(app.active_viewport_mut().message_height(0) > 200);

        let mut culled_lines = Vec::new();
        let stats = render_culled_messages(
            &mut app,
            &spinner,
            width,
            40,
            viewport_height,
            &mut culled_lines,
        );

        assert_eq!(stats.rendered_msgs, 1);
        assert_eq!(stats.last_rendered_idx, Some(0));
    }

    #[test]
    fn paragraph_scroll_offset_clamps_large_local_scroll_explicitly() {
        assert_eq!(paragraph_scroll_offset(42), 42);
        assert_eq!(paragraph_scroll_offset(usize::from(u16::MAX) + 123), u16::MAX);
    }

    #[test]
    fn chat_selection_snapshot_refreshes_without_dragging_after_streaming_change() {
        let mut app = App::test_default();
        app.status = AppStatus::Running;
        *app.active_messages_mut() = vec![assistant_text_message("hello")];
        app.bind_active_turn_assistant(0);
        *app.selection_mut() = Some(SelectionState {
            kind: SelectionKind::Chat,
            start: SelectionPoint { row: 0, col: 0 },
            end: SelectionPoint { row: 0, col: 5 },
            dragging: false,
        });

        render_selected_chat_snapshot(&mut app, 20, 6);
        let first_snapshot = app.rendered_chat_lines.clone();
        assert!(!first_snapshot.is_empty());

        if let Some(MessageBlock::Text(block)) =
            app.active_messages_mut().get_mut(0).and_then(|message| message.blocks.get_mut(0))
        {
            block.text.push_str("\nworld");
            block.markdown.append("\nworld");
            block.cache.invalidate();
        }
        app.invalidate_layout(InvalidationLevel::MessageChanged(0));

        render_selected_chat_snapshot(&mut app, 20, 6);

        assert_ne!(app.rendered_chat_lines, first_snapshot);
        assert!(app.rendered_chat_lines.iter().any(|line| line.contains("world")));
    }

    #[test]
    fn clamp_scroll_to_content_snaps_overscroll_after_shrink() {
        let mut viewport = ChatViewport::new();
        viewport.auto_scroll = false;
        viewport.scroll_target = 120;
        viewport.scroll_pos = 120.0;
        viewport.scroll_offset = 120;

        clamp_scroll_to_content(&mut viewport, 40, false);

        assert!(viewport.auto_scroll);
        assert_eq!(viewport.scroll_target, 40);
        assert!(viewport.scroll_pos > 40.0);
        assert!(viewport.scroll_pos < 120.0);
        assert_eq!(viewport.scroll_offset, 40);
    }

    #[test]
    fn clamp_scroll_to_content_preserves_in_range_scroll() {
        let mut viewport = ChatViewport::new();
        viewport.auto_scroll = false;
        viewport.scroll_target = 20;
        viewport.scroll_pos = 20.0;
        viewport.scroll_offset = 20;

        clamp_scroll_to_content(&mut viewport, 40, false);

        assert!(!viewport.auto_scroll);
        assert_eq!(viewport.scroll_target, 20);
        assert!((viewport.scroll_pos - 20.0).abs() < f32::EPSILON);
        assert_eq!(viewport.scroll_offset, 20);
    }

    #[test]
    fn remember_render_trace_state_suppresses_identical_repeats() {
        let mut app = App::test_default();
        let trace_state = crate::app::ChatRenderTraceState {
            width: 80,
            content_height: 120,
            viewport_height: 24,
            auto_scroll: true,
            pinned_to_bottom: true,
            scroll_target: 96,
            scroll_offset: 96,
            max_scroll: 96,
            first_visible: 3,
            render_start: 3,
            local_scroll: 0,
            rendered_msgs: 2,
            last_rendered_idx: Some(4),
            rendered_line_count: 42,
            last_message_idx: Some(4),
            last_message_height: Some(8),
            selection_snapshot_active: false,
        };

        assert!(super::remember_render_trace_state(&mut app, trace_state));
        assert!(!super::remember_render_trace_state(&mut app, trace_state));

        let changed = crate::app::ChatRenderTraceState {
            rendered_line_count: trace_state.rendered_line_count + 1,
            ..trace_state
        };
        assert!(super::remember_render_trace_state(&mut app, changed));
    }

    #[test]
    fn clamp_scroll_to_content_settles_to_max_over_frames() {
        let mut viewport = ChatViewport::new();
        viewport.auto_scroll = false;
        viewport.scroll_target = 120;
        viewport.scroll_pos = 120.0;
        viewport.scroll_offset = 120;

        for _ in 0..12 {
            clamp_scroll_to_content(&mut viewport, 40, false);
        }

        assert_eq!(viewport.scroll_target, 40);
        assert_eq!(viewport.scroll_offset, 40);
        assert!(viewport.scroll_pos >= 40.0);
        assert!(viewport.scroll_pos < 40.1);
    }

    #[test]
    fn clamp_scroll_to_content_snaps_overscroll_when_reduced_motion_enabled() {
        let mut viewport = ChatViewport::new();
        viewport.auto_scroll = false;
        viewport.scroll_target = 120;
        viewport.scroll_pos = 120.0;
        viewport.scroll_offset = 120;

        clamp_scroll_to_content(&mut viewport, 40, true);

        assert!(viewport.auto_scroll);
        assert_eq!(viewport.scroll_target, 40);
        assert!((viewport.scroll_pos - 40.0).abs() < f32::EPSILON);
        assert_eq!(viewport.scroll_offset, 40);
    }

    #[test]
    fn smooth_scrollbar_geometry_snaps_when_reduced_motion_enabled() {
        let mut viewport = ChatViewport::new();
        viewport.scrollbar_thumb_top = 2.0;
        viewport.scrollbar_thumb_size = 3.0;

        let geometry = smooth_scrollbar_geometry(
            &mut viewport,
            ScrollbarGeometry { thumb_top: 9, thumb_size: 5, track_space: 15, max_scroll: 40 },
            20,
            true,
        );

        assert_eq!(
            geometry,
            ScrollbarGeometry { thumb_top: 9, thumb_size: 5, track_space: 15, max_scroll: 40 }
        );
        assert!((viewport.scrollbar_thumb_top - 9.0).abs() < f32::EPSILON);
        assert!((viewport.scrollbar_thumb_size - 5.0).abs() < f32::EPSILON);
    }
}
