// ratatui geometry: terminal dims are u16, scroll math goes through f32
// for smooth-scroll. Casts (usize↔f32, f32→u16/usize) are inherent and bounded by
// terminal size.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use super::super::selection::clear_selection;
use super::super::state::ScrollbarDragState;
use super::super::{
    App, MessageBlock, PaneHitTarget, ScrollbarGeometry, SelectionKind, SelectionPoint,
};
use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

pub(super) const MOUSE_SCROLL_LINES: usize = 3;

struct MouseSelectionPoint {
    kind: SelectionKind,
    point: SelectionPoint,
}

pub(super) fn handle_mouse_event(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if handle_pane_click(app, mouse) {
                return;
            }
            if start_scrollbar_drag(app, mouse) {
                return;
            }
            app.scrollbar_drag = None;
            if try_toggle_tool_call_at_click(app, mouse) {
                return;
            }
            if try_toggle_peer_user_block_at_click(app, mouse) {
                return;
            }
            if try_toggle_stop_hook_summary_at_click(app, mouse) {
                return;
            }
            if let Some(pt) = mouse_point_to_selection(app, mouse) {
                *app.selection_mut() = Some(super::super::SelectionState {
                    kind: pt.kind,
                    start: pt.point,
                    end: pt.point,
                    dragging: true,
                });
            } else {
                clear_selection(app);
            }
        }
        MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
            if update_scrollbar_drag(app, mouse) {
                return;
            }
            let pt = mouse_point_to_selection(app, mouse);
            if let (Some(sel), Some(pt)) = (app.selection_mut().as_mut(), pt) {
                sel.end = pt.point;
            }
        }
        MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
            app.scrollbar_drag = None;
            if let Some(sel) = app.selection_mut().as_mut() {
                sel.dragging = false;
            }
        }
        _ => {}
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            // Inspector body claims the wheel when the cursor is
            // over it — both the inline pane (Wide/Medium) and the
            // Narrow-tier overlay use the same body rect.
            if mouse_in_inspector_body(app, mouse) {
                scroll_inspector(app, MOUSE_SCROLL_LINES, true);
                return;
            }
            if mouse_in_projects_pane_body(app, mouse) {
                scroll_projects_pane(app, MOUSE_SCROLL_LINES, true);
                return;
            }
            // While the Narrow-tier overlay is open the chat is
            // hidden behind it; scrolling the chat viewport would
            // silently move content the user can't see. Future:
            // scroll the overlay's project list when it overflows.
            if app.projects_pane_overlay_open && app.layout.top_bar.is_some() {
                return;
            }
            if app.inspector_pane_overlay_open && app.layout.top_bar.is_some() {
                return;
            }
            if app.selection().is_some() {
                clear_selection(app);
            }
            app.active_viewport_mut().scroll_up(MOUSE_SCROLL_LINES);
        }
        MouseEventKind::ScrollDown => {
            if mouse_in_inspector_body(app, mouse) {
                scroll_inspector(app, MOUSE_SCROLL_LINES, false);
                return;
            }
            if mouse_in_projects_pane_body(app, mouse) {
                scroll_projects_pane(app, MOUSE_SCROLL_LINES, false);
                return;
            }
            if app.projects_pane_overlay_open && app.layout.top_bar.is_some() {
                return;
            }
            if app.inspector_pane_overlay_open && app.layout.top_bar.is_some() {
                return;
            }
            if app.selection().is_some() {
                clear_selection(app);
            }
            app.active_viewport_mut().scroll_down(MOUSE_SCROLL_LINES);
        }
        _ => {}
    }
}

/// True when the cursor is inside the Projects pane's scrollable
/// body (NOT the pinned banner or the account footer).
fn mouse_in_projects_pane_body(app: &App, mouse: MouseEvent) -> bool {
    let rect = app.rendered_projects_pane_body_area;
    if rect.width == 0 || rect.height == 0 {
        return false;
    }
    mouse.column >= rect.x
        && mouse.column < rect.x.saturating_add(rect.width)
        && mouse.row >= rect.y
        && mouse.row < rect.y.saturating_add(rect.height)
}

fn scroll_projects_pane(app: &mut App, lines: usize, up: bool) {
    let delta = u16::try_from(lines).unwrap_or(u16::MAX);
    app.projects_pane_scroll_offset = if up {
        app.projects_pane_scroll_offset.saturating_sub(delta)
    } else {
        app.projects_pane_scroll_offset.saturating_add(delta)
    };
    app.needs_redraw = true;
}

/// True when the wheel event happened with the cursor inside the
/// Inspector pane's scrollable body (NOT the pinned banner). Lookup
/// is against the rect cached by the last inspector render —
/// `Rect::default()` while no inspector render has happened yet,
/// which collapses to `false` because zero-width rects don't
/// contain any point.
fn mouse_in_inspector_body(app: &App, mouse: MouseEvent) -> bool {
    let rect = app.rendered_inspector_body_area;
    if rect.width == 0 || rect.height == 0 {
        return false;
    }
    mouse.column >= rect.x
        && mouse.column < rect.x.saturating_add(rect.width)
        && mouse.row >= rect.y
        && mouse.row < rect.y.saturating_add(rect.height)
}

/// Adjust the active session's `inspector_scroll_offset` by `lines`
/// rows in the requested direction. Up = decrement (towards 0); down
/// = increment (clamped at u16::MAX — the actual upper bound is
/// re-clamped against the body's total line count on the next render).
fn scroll_inspector(app: &mut App, lines: usize, up: bool) {
    let Some(session) = app.try_active_bucket_mut() else { return };
    let delta = u16::try_from(lines).unwrap_or(u16::MAX);
    session.inspector_scroll_offset = if up {
        session.inspector_scroll_offset.saturating_sub(delta)
    } else {
        session.inspector_scroll_offset.saturating_add(delta)
    };
    app.needs_redraw = true;
}

#[derive(Clone, Copy)]
pub(super) struct ScrollbarMetrics {
    pub viewport_height: usize,
    pub target: ScrollbarGeometry,
}

fn start_scrollbar_drag(app: &mut App, mouse: MouseEvent) -> bool {
    if !mouse_on_scrollbar_rail(app, mouse) {
        return false;
    }
    let Some(metrics) = scrollbar_metrics(app) else {
        return false;
    };
    let Some(local_row) = mouse_row_on_chat_track(app, mouse) else {
        return false;
    };

    let geometry = current_thumb_geometry(app, metrics);
    let thumb_top = geometry.thumb_top;
    let thumb_size = geometry.thumb_size;
    let thumb_end = thumb_top.saturating_add(thumb_size);
    let grab_offset = if (thumb_top..thumb_end).contains(&local_row) {
        local_row.saturating_sub(thumb_top)
    } else {
        thumb_size / 2
    };

    set_scroll_from_thumb_top(
        app,
        local_row.saturating_sub(grab_offset),
        geometry.track_space,
        geometry.max_scroll,
    );
    app.scrollbar_drag = Some(ScrollbarDragState {
        thumb_grab_offset: grab_offset,
        track_space: geometry.track_space,
        max_scroll: geometry.max_scroll,
    });
    clear_selection(app);
    true
}

fn update_scrollbar_drag(app: &mut App, mouse: MouseEvent) -> bool {
    let Some(drag) = app.scrollbar_drag else {
        return false;
    };
    if scrollbar_metrics(app).is_none() {
        app.scrollbar_drag = None;
        return false;
    }
    let Some(local_row) = mouse_row_on_chat_track(app, mouse) else {
        return false;
    };

    set_scroll_from_thumb_top(
        app,
        local_row.saturating_sub(drag.thumb_grab_offset),
        drag.track_space,
        drag.max_scroll,
    );
    true
}

fn scrollbar_metrics(app: &App) -> Option<ScrollbarMetrics> {
    let area = app.rendered_chat_area;
    if area.width == 0 || area.height == 0 {
        return None;
    }

    let viewport_height = area.height as usize;
    let content_height = app.viewport().total_message_height();
    let target = crate::app::compute_scrollbar_geometry(
        content_height,
        viewport_height,
        app.viewport().scroll_pos,
    )?;
    Some(ScrollbarMetrics { viewport_height, target })
}

fn current_thumb_geometry(app: &App, metrics: ScrollbarMetrics) -> ScrollbarGeometry {
    let mut thumb_size = app.viewport().scrollbar_thumb_size.round() as usize;
    if thumb_size == 0 {
        thumb_size = metrics.target.thumb_size;
    }
    thumb_size = thumb_size.max(1).min(metrics.viewport_height);
    let max_top = metrics.viewport_height.saturating_sub(thumb_size);
    let thumb_top = app.viewport().scrollbar_thumb_top.round().clamp(0.0, max_top as f32) as usize;
    ScrollbarGeometry {
        thumb_top,
        thumb_size,
        track_space: metrics.viewport_height.saturating_sub(thumb_size),
        max_scroll: metrics.target.max_scroll,
    }
}

fn set_scroll_from_thumb_top(
    app: &mut App,
    thumb_top: usize,
    track_space: usize,
    max_scroll: usize,
) {
    let thumb_top = thumb_top.min(track_space);
    let target = if track_space == 0 {
        0
    } else {
        ((thumb_top as f32 / track_space as f32) * max_scroll as f32).round() as usize
    }
    .min(max_scroll);

    let vp = app.active_viewport_mut();
    vp.auto_scroll = false;
    vp.scroll_target = target;
    // Keep content movement responsive while dragging the thumb.
    vp.scroll_pos = target as f32;
    vp.scroll_offset = target;
}

fn mouse_on_scrollbar_rail(app: &App, mouse: MouseEvent) -> bool {
    let area = app.rendered_chat_area;
    if area.width == 0 || area.height == 0 {
        return false;
    }
    let rail_x = area.right();
    mouse.column == rail_x && mouse.row >= area.y && mouse.row < area.bottom()
}

fn mouse_row_on_chat_track(app: &App, mouse: MouseEvent) -> Option<usize> {
    let area = app.rendered_chat_area;
    if area.height == 0 {
        return None;
    }
    let max_row = area.height.saturating_sub(1) as usize;
    if mouse.row < area.y {
        return Some(0);
    }
    if mouse.row >= area.bottom() {
        return Some(max_row);
    }
    Some((mouse.row - area.y) as usize)
}

fn mouse_point_to_selection(app: &App, mouse: MouseEvent) -> Option<MouseSelectionPoint> {
    let input_area = app.rendered_input_area;
    if mouse.column >= input_area.x
        && mouse.column < input_area.right()
        && mouse.row >= input_area.y
        && mouse.row < input_area.bottom()
    {
        let row = (mouse.row - input_area.y) as usize;
        let col = (mouse.column - input_area.x) as usize;
        return Some(MouseSelectionPoint {
            kind: SelectionKind::Input,
            point: SelectionPoint { row, col },
        });
    }

    let chat_area = app.rendered_chat_area;
    if mouse.column >= chat_area.x
        && mouse.column < chat_area.right()
        && mouse.row >= chat_area.y
        && mouse.row < chat_area.bottom()
    {
        let row = (mouse.row - chat_area.y) as usize;
        let col = (mouse.column - chat_area.x) as usize;
        return Some(MouseSelectionPoint {
            kind: SelectionKind::Chat,
            point: SelectionPoint { row, col },
        });
    }
    None
}

/// If the click landed on a tool-call's rendered area inside the chat
/// pane, flip that tool call's per-tool collapse override and consume
/// the event. Returns `true` when a tool call was toggled (so the
/// caller can skip starting a text selection).
fn try_toggle_tool_call_at_click(app: &mut App, mouse: MouseEvent) -> bool {
    let Some((msg_idx, block_idx)) = locate_tool_call_block_at_click(app, mouse) else {
        return false;
    };
    let global_default = app.tools_collapsed;
    let Some(MessageBlock::ToolCall(tc)) =
        app.active_messages_mut()[msg_idx].blocks.get_mut(block_idx)
    else {
        return false;
    };
    let current = tc.collapsed_override.unwrap_or(global_default);
    let new_collapsed = !current;
    tc.collapsed_override = Some(new_collapsed);
    let tool_id = tc.id.clone();
    // Layout-dirty bumps both the layout epoch (forcing a remeasure) and
    // the render epoch (which is hashed into MessageRenderSignature),
    // invalidating the per-block + message-level render caches.
    tc.mark_tool_call_layout_dirty();
    app.invalidate_layout(crate::app::InvalidationLevel::MessageChanged(msg_idx));
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "tool_call_click_toggled",
        tool_id = %tool_id,
        msg_idx,
        block_idx,
        new_collapsed,
        "click toggled per-tool collapse override"
    );
    true
}

/// Map the chat-area click coordinate to a `(message_idx, block_idx)`
/// pair when the cell lands on a rendered tool-call's y-range.
///
/// Each tool call records its own `last_measured_y_in_msg` during the
/// assistant render pass, so this hit-test reads only data the renderer
/// just committed — no fragile peer-block height walks.
fn locate_tool_call_block_at_click(app: &App, mouse: MouseEvent) -> Option<(usize, usize)> {
    let chat_area = app.rendered_chat_area;
    if chat_area.width == 0 || chat_area.height == 0 {
        return None;
    }
    if mouse.column < chat_area.x
        || mouse.column >= chat_area.right()
        || mouse.row < chat_area.y
        || mouse.row >= chat_area.bottom()
    {
        return None;
    }

    // Absolute content-row of the click (== local row + scroll offset).
    let local_row = (mouse.row - chat_area.y) as usize;
    let absolute_row = local_row.checked_add(app.viewport().scroll_offset)?;

    // Find the message that owns this row via the existing prefix-sum
    // index, then walk only that message's tool-call blocks. Each tool
    // stores its own y-offset within the message and its measured
    // height, so the inclusion test is just an interval check.
    if app.messages().is_empty() {
        return None;
    }
    let msg_idx = app.viewport().find_first_visible(absolute_row);
    if msg_idx >= app.messages().len() {
        return None;
    }
    let msg_start = app.viewport().cumulative_height_before(msg_idx);
    let row_within_msg = absolute_row.checked_sub(msg_start)?;
    let width = chat_area.width;
    for (block_idx, block) in app.messages()[msg_idx].blocks.iter().enumerate() {
        let MessageBlock::ToolCall(tc) = block else {
            continue;
        };
        if tc.last_measured_height == 0 || tc.last_measured_width != width {
            continue;
        }
        let y_start = tc.last_measured_y_in_msg;
        let y_end = y_start.saturating_add(tc.last_measured_height);
        if row_within_msg >= y_start && row_within_msg < y_end {
            tracing::debug!(
                target: crate::logging::targets::APP_INPUT,
                event_name = "tool_call_hit_test",
                outcome = "hit",
                msg_idx,
                block_idx,
                tool_id = %tc.id,
                row_within_msg,
                y_start,
                y_end,
                "click landed inside tool-call rendered range",
            );
            return Some((msg_idx, block_idx));
        }
    }
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "tool_call_hit_test",
        outcome = "no_hit",
        mouse_row = mouse.row,
        mouse_column = mouse.column,
        scroll_offset = app.viewport().scroll_offset,
        absolute_row,
        msg_idx,
        msg_count = app.messages().len(),
        msg_start,
        row_within_msg,
        msg_height = app.viewport().message_height(msg_idx),
        "click did not match any tool's recorded y-range",
    );
    None
}

/// Peer-block (#114) inbound twin of [`try_toggle_tool_call_at_click`].
/// Inbound peer envelopes are user-message TextBlocks (the workspace's
/// synthetic Message::User echo, pattern-matched at render time by
/// `peer_block::detect_inbound`). They don't have a `ToolCallInfo`
/// to hang collapse state off of, so the relevant flag lives on
/// `TextBlock::peer_collapsed_override` and the renderer stamps the
/// same `peer_last_measured_y/height/width` triple a tool call gets.
fn try_toggle_peer_user_block_at_click(app: &mut App, mouse: MouseEvent) -> bool {
    let Some((msg_idx, block_idx)) = locate_peer_user_block_at_click(app, mouse) else {
        return false;
    };
    let global_default = app.tools_collapsed;
    let Some(MessageBlock::Text(text_block)) =
        app.active_messages_mut()[msg_idx].blocks.get_mut(block_idx)
    else {
        return false;
    };
    let current = text_block.peer_collapsed_override.unwrap_or(global_default);
    let new_collapsed = !current;
    text_block.peer_collapsed_override = Some(new_collapsed);
    app.invalidate_layout(crate::app::InvalidationLevel::MessageChanged(msg_idx));
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "peer_user_block_click_toggled",
        msg_idx,
        block_idx,
        new_collapsed,
        "click toggled per-row peer-block collapse override (inbound)"
    );
    true
}

/// #273: If the click landed on the stop_hook_summary chip rendered
/// at end of the active assistant turn, flip its expanded state and
/// consume the event. Returns `true` when a toggle fired.
fn try_toggle_stop_hook_summary_at_click(app: &mut App, mouse: MouseEvent) -> bool {
    let Some(msg_idx) = locate_stop_hook_summary_at_click(app, mouse) else {
        return false;
    };
    app.toggle_stop_hook_summary_expanded(msg_idx);
    app.invalidate_layout(crate::app::InvalidationLevel::MessageChanged(msg_idx));
    tracing::debug!(
        target: crate::logging::targets::APP_INPUT,
        event_name = "stop_hook_summary_click_toggled",
        msg_idx,
        "click toggled stop_hook_summary expanded state"
    );
    true
}

/// #273: Map the chat-area click to the `message_idx` whose
/// stop_hook_summary chip contains the click. The renderer stamps
/// `stop_hook_summary_y_in_msg / stop_hook_summary_height` on the
/// `ChatMessage` for the chip's line range only; clicks on the
/// expanded hook rows do NOT toggle.
fn locate_stop_hook_summary_at_click(app: &App, mouse: MouseEvent) -> Option<usize> {
    let chat_area = app.rendered_chat_area;
    if chat_area.width == 0 || chat_area.height == 0 {
        return None;
    }
    if mouse.column < chat_area.x
        || mouse.column >= chat_area.right()
        || mouse.row < chat_area.y
        || mouse.row >= chat_area.bottom()
    {
        return None;
    }
    let local_row = (mouse.row - chat_area.y) as usize;
    let absolute_row = local_row.checked_add(app.viewport().scroll_offset)?;
    if app.messages().is_empty() {
        return None;
    }
    let msg_idx = app.viewport().find_first_visible(absolute_row);
    if msg_idx >= app.messages().len() {
        return None;
    }
    let msg_start = app.viewport().cumulative_height_before(msg_idx);
    let row_within_msg = absolute_row.checked_sub(msg_start)?;
    let msg = &app.messages()[msg_idx];
    if msg.stop_hook_summary_height == 0 {
        return None;
    }
    let y_start = msg.stop_hook_summary_y_in_msg;
    let y_end = y_start.saturating_add(msg.stop_hook_summary_height);
    if row_within_msg >= y_start && row_within_msg < y_end { Some(msg_idx) } else { None }
}

/// Map the chat-area click to a `(msg_idx, block_idx)` pair for an
/// inbound peer TextBlock. Same shape as
/// [`locate_tool_call_block_at_click`] — walks `app.messages()[msg_idx].blocks`
/// looking for `MessageBlock::Text` whose `peer_last_measured_height >
/// 0` and whose recorded y-range contains the click.
fn locate_peer_user_block_at_click(app: &App, mouse: MouseEvent) -> Option<(usize, usize)> {
    let chat_area = app.rendered_chat_area;
    if chat_area.width == 0 || chat_area.height == 0 {
        return None;
    }
    if mouse.column < chat_area.x
        || mouse.column >= chat_area.right()
        || mouse.row < chat_area.y
        || mouse.row >= chat_area.bottom()
    {
        return None;
    }
    let local_row = (mouse.row - chat_area.y) as usize;
    let absolute_row = local_row.checked_add(app.viewport().scroll_offset)?;
    if app.messages().is_empty() {
        return None;
    }
    let msg_idx = app.viewport().find_first_visible(absolute_row);
    if msg_idx >= app.messages().len() {
        return None;
    }
    let msg_start = app.viewport().cumulative_height_before(msg_idx);
    let row_within_msg = absolute_row.checked_sub(msg_start)?;
    let width = chat_area.width;
    for (block_idx, block) in app.messages()[msg_idx].blocks.iter().enumerate() {
        let MessageBlock::Text(text_block) = block else {
            continue;
        };
        if text_block.peer_last_measured_height == 0 || text_block.peer_last_measured_width != width
        {
            continue;
        }
        let y_start = text_block.peer_last_measured_y_in_msg;
        let y_end = y_start.saturating_add(text_block.peer_last_measured_height);
        if row_within_msg >= y_start && row_within_msg < y_end {
            return Some((msg_idx, block_idx));
        }
    }
    None
}

/// Route a left-button click that may land on the Projects surface.
///
/// Three shapes share this entry point:
/// 1. Narrow-tier top bar — `▤` icon toggles the overlay; clicks
///    elsewhere on the bar are not interactive.
/// 2. Narrow-tier overlay — `✕` glyph dismisses without switching;
///    project header / session row clicks switch active session AND
///    close the overlay in one action.
/// 3. Wide / Medium inline pane — header / row clicks switch active
///    session; the overlay flag is irrelevant here.
///
/// Returns `true` when the click was consumed so the chat hit-test
/// path is skipped.
fn handle_pane_click(app: &mut App, mouse: MouseEvent) -> bool {
    // X+Y-bounded targets (top-bar icon, overlay ✕). These overlap
    // the chat body or share a one-row band with non-interactive
    // text, so we must constrain on column too — `contains_y`
    // alone would catch unrelated clicks on the same row.
    let xy_target = app
        .pane_hit_targets
        .iter()
        .find(|t| {
            matches!(
                t,
                PaneHitTarget::TopBarIcon { .. }
                    | PaneHitTarget::InspectorTopBarIcon { .. }
                    | PaneHitTarget::OverlayClose { .. }
                    | PaneHitTarget::CloseSession { .. }
                    | PaneHitTarget::InspectorGitOpenDiff { .. }
                    | PaneHitTarget::CopySessionId { .. }
                    | PaneHitTarget::CloseWorker { .. }
            ) && t.contains(mouse.column, mouse.row)
        })
        .cloned();
    if let Some(target) = xy_target {
        match target {
            PaneHitTarget::TopBarIcon { .. } => {
                // Mutually exclusive overlays — opening Projects
                // closes Inspector.
                if !app.projects_pane_overlay_open {
                    app.inspector_pane_overlay_open = false;
                }
                app.projects_pane_overlay_open = !app.projects_pane_overlay_open;
                app.needs_redraw = true;
                return true;
            }
            PaneHitTarget::InspectorTopBarIcon { .. } => {
                if !app.inspector_pane_overlay_open {
                    app.projects_pane_overlay_open = false;
                }
                app.inspector_pane_overlay_open = !app.inspector_pane_overlay_open;
                app.needs_redraw = true;
                return true;
            }
            PaneHitTarget::OverlayClose { .. } => {
                app.projects_pane_overlay_open = false;
                app.inspector_pane_overlay_open = false;
                app.needs_redraw = true;
                return true;
            }
            PaneHitTarget::CloseSession { session_key, .. } => {
                close_session(app, &session_key);
                app.needs_redraw = true;
                return true;
            }
            PaneHitTarget::InspectorGitOpenDiff { .. } => {
                crate::app::diff_overlay::open_default(app);
                app.needs_redraw = true;
                return true;
            }
            PaneHitTarget::CopySessionId { session_id, .. } => {
                copy_session_id_to_clipboard(&session_id);
                return true;
            }
            PaneHitTarget::CloseWorker { project_key, label, .. } => {
                close_worker(app, &project_key, &label);
                app.needs_redraw = true;
                return true;
            }
            PaneHitTarget::ProjectHeader { .. }
            | PaneHitTarget::SessionRow { .. }
            | PaneHitTarget::WorkerRow { .. } => {}
        }
    }

    // Overlay open: row clicks anywhere on the body switch + close
    // in one action. The overlay covers the whole body rect so we
    // skip the inline-pane gate.
    if app.projects_pane_overlay_open {
        let target = app.pane_hit_targets.iter().find(|t| t.contains_y(mouse.row)).cloned();
        let Some(target) = target else {
            // Click landed on overlay chrome (banner rule, blank
            // padding). Consume so chat hit-tests don't fire
            // through the overlay; leave the overlay open.
            return true;
        };
        return match target {
            PaneHitTarget::ProjectHeader { project_name, .. } => {
                switch_to_project_lead(app, &project_name);
                app.projects_pane_overlay_open = false;
                app.needs_redraw = true;
                true
            }
            PaneHitTarget::SessionRow { session_key, .. } => {
                switch_to_session_or_spawn(app, session_key);
                app.projects_pane_overlay_open = false;
                app.needs_redraw = true;
                true
            }
            PaneHitTarget::WorkerRow { session_key, .. } => {
                switch_to_worker(app, session_key);
                app.projects_pane_overlay_open = false;
                app.needs_redraw = true;
                true
            }
            // x+y-bounded glyphs handled above; reaching them here
            // means the y-only fallback matched a row stamped on
            // the same band as the glyph but the click missed the
            // glyph's x range — treat as "in-overlay no-op" so we
            // still consume.
            PaneHitTarget::TopBarIcon { .. }
            | PaneHitTarget::InspectorTopBarIcon { .. }
            | PaneHitTarget::OverlayClose { .. }
            | PaneHitTarget::CloseSession { .. }
            | PaneHitTarget::InspectorGitOpenDiff { .. }
            | PaneHitTarget::CopySessionId { .. }
            | PaneHitTarget::CloseWorker { .. } => true,
        };
    }

    // Inline pane (Wide / Medium): existing y-only routing gated by
    // the inline pane rect.
    let Some(pane) = app.layout.pane else {
        return false;
    };
    if !rect_contains(pane, mouse.column, mouse.row) {
        return false;
    }
    let target = app.pane_hit_targets.iter().find(|t| t.contains_y(mouse.row)).cloned();
    let Some(target) = target else {
        // Click landed in the pane area but outside any stamped row
        // (banner rule, blank line, padding). Consume so the chat
        // hit-test below can't accidentally fire on a pane click.
        return true;
    };
    match target {
        PaneHitTarget::ProjectHeader { project_name, .. } => {
            switch_to_project_lead(app, &project_name);
            true
        }
        PaneHitTarget::SessionRow { session_key, .. } => {
            switch_to_session_or_spawn(app, session_key);
            true
        }
        PaneHitTarget::WorkerRow { session_key, .. } => {
            switch_to_worker(app, session_key);
            true
        }
        // x+y-bounded glyphs are checked first; here for exhaustive
        // matching only.
        PaneHitTarget::TopBarIcon { .. }
        | PaneHitTarget::InspectorTopBarIcon { .. }
        | PaneHitTarget::OverlayClose { .. }
        | PaneHitTarget::CloseSession { .. }
        | PaneHitTarget::InspectorGitOpenDiff { .. }
        | PaneHitTarget::CopySessionId { .. }
        | PaneHitTarget::CloseWorker { .. } => true,
    }
}

/// Close an active session: drop the bucket from `app.sessions` and
/// release its pool entry on the workspace so the underlying
/// `claude` subprocess exits when the last `Arc<AgentHandle>` is
/// dropped. The project moves back to the INACTIVE list on the next
/// render (no real catalog entry change is needed — `list_projects`
/// re-partitions based on whether any of the project's sessions are
/// in `app.sessions`).
/// Write the active session's id to the OS clipboard via `arboard`.
/// No on-screen feedback — the click target's `⎘` glyph is its own
/// affordance. Logs success / failure to the tracing stream.
fn copy_session_id_to_clipboard(session_id: &str) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(session_id.to_owned())) {
        Ok(()) => tracing::info!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "session_id_copied",
            message = "session id copied to clipboard",
            outcome = "success",
            session_id = %session_id,
        ),
        Err(err) => tracing::warn!(
            target: crate::logging::targets::APP_INPUT,
            event_name = "session_id_copy_failed",
            message = "failed to copy session id to clipboard",
            outcome = "failure",
            session_id = %session_id,
            error_message = %err,
        ),
    }
}

fn close_session(app: &mut App, session_key: &forge_workspace::SessionKey) {
    if let Some(workspace) = app.workspace.as_ref() {
        // Cascade-aware: if the session is a project's lead, all
        // workers under that project terminate first.
        workspace.release_session_with_cascade(session_key);
    }
    app.sessions.remove(session_key);
    if app.active_session_key.as_ref() == Some(session_key) {
        let fallback = app.sessions.keys().next().cloned();
        if let Some(new_active) = fallback {
            app.switch_active_session(new_active);
        } else {
            app.active_session_key = None;
        }
    }
}

/// Close a worker via the workspace command bus. Workspace removes
/// the worker entry from `live_workers`, releases the underlying
/// session, and emits a `SessionUpdate::WorkerStatusChanged { Removed,
/// .. }` so the projects pane re-renders without the row.
fn close_worker(app: &mut App, project_key: &forge_workspace::ProjectKey, label: &str) {
    let Some(workspace) = app.workspace.as_ref() else {
        return;
    };
    if let Err(err) = workspace.dispatch(forge_workspace::Command::CloseWorker {
        project_key: project_key.clone(),
        label: label.to_owned(),
    }) {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            project = %project_key.as_str(),
            label = %label,
            error = %err,
            "close_worker: dispatch failed",
        );
    }
}

/// Switch the active session to a worker's chat. The worker's
/// `SessionKey` lives in `app.sessions` once Connected has landed
/// for the worker session (workers spawn through the standard
/// session lifecycle, same as project leads).
fn switch_to_worker(app: &mut App, session_key: forge_workspace::SessionKey) {
    if app.sessions.contains_key(&session_key) {
        app.switch_active_session(session_key);
    } else {
        // Spawning window: the worker's WorkerEntry has a session_key
        // but the bucket isn't in app.sessions yet because Connected
        // hasn't fired. Refuse the click silently - the next click
        // after the worker lands its bucket will succeed.
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            session_id = %session_key.as_str(),
            "switch_to_worker: bucket not yet present, click ignored",
        );
    }
}

/// Switch to the clicked session row's bucket, or kick off a fresh
/// resume spawn when the bucket isn't in `app.sessions` yet.
///
/// The Projects-pane drilldown lists every session for the active
/// project (lead + non-lead) from the on-disk catalog; non-lead
/// sessions typically aren't pooled in `app.sessions` until the
/// user clicks them. Without this fallback, clicking a non-lead row
/// would silently no-op (the closest thing to a useless button).
///
/// The lead-session row click still lands here too, but
/// `switch_active_session` returns immediately when the key matches,
/// and the spawn helper short-circuits when the key is already
/// present — so the lead-click path keeps its existing
/// switch-only semantics.
fn switch_to_session_or_spawn(app: &mut App, session_key: forge_workspace::SessionKey) {
    if app.sessions.contains_key(&session_key) {
        app.switch_active_session(session_key);
    } else if let Some(workspace) = app.workspace.as_ref() {
        let launch_settings = crate::app::connect::session_launch_settings_for_resume(app);
        if let Err(err) = workspace.dispatch(forge_workspace::Command::SpawnSession {
            session_id: session_key.as_str().to_owned(),
            launch_settings,
        }) {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                session_id = %session_key.as_str(),
                error = %err,
                "switch_to_session_or_spawn: dispatch failed",
            );
        }
    }
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

/// Switch to the lead session of `project_name`. If the project's
/// lead is already an in-process session in `app.sessions`, swap to
/// it; otherwise hand off to
/// [`crate::app::connect::spawn_for_sleeping_project`] which
/// synthesizes a spawning bucket and kicks off the async lookup of
/// the project's lead AgentHandle.
fn switch_to_project_lead(app: &mut App, project_name: &str) {
    // Resolve display name + project path up-front; both are used
    // in multiple branches below.
    let project_info = app.workspace.as_ref().and_then(|w| {
        w.list_projects()
            .into_iter()
            .find(|p| p.key.as_str() == project_name)
            .map(|p| (p.name.clone(), p.path.clone(), p.sessions))
    });
    let (resolved_name, project_path, catalog_sessions) = match project_info {
        Some((name, path, sessions)) => (name, Some(path), sessions),
        None => (project_name.to_owned(), None, Vec::new()),
    };
    let spawn_synthetic =
        forge_workspace::SessionKey::from_session_id(format!("__spawn_{resolved_name}__"));

    // Idempotency: if the active session is already the synthetic
    // spawn bucket for this project, the user is mid-wake — a second
    // click would queue a duplicate background connection task that
    // races `CONN_SLOT` and scrambles bucket state. Return early.
    if app.active_session_key.as_ref() == Some(&spawn_synthetic)
        && app.sessions.contains_key(&spawn_synthetic)
    {
        return;
    }

    // Mid-spawn block: if the resolved project's bucket is in the
    // `Spawning` lifecycle state, clicking it would land the user on
    // the connecting stub (no session_id yet, input renders
    // "Connecting to Claude Code…"). Refuse the click so the user
    // waits for the spawn to land instead. Same UX gate the
    // launchpad applies via `click_intent`. Covers both the
    // `__spawn_<name>__` synthetic and the real-UUID bucket that
    // KeyRenamed has migrated to but hasn't yet received `Connected`.
    let spawning_bucket = app
        .sessions
        .get(&spawn_synthetic)
        .or_else(|| {
            project_path.as_ref().and_then(|p| {
                let path_str = p.to_string_lossy();
                app.find_running_bucket_for_path(path_str.as_ref())
                    .and_then(|k| app.sessions.get(&k))
            })
        })
        .map(|s| s.lifecycle_state);
    if spawning_bucket == Some(forge_primitives::SessionLifecycleState::Spawning) {
        return;
    }

    // Mid-spawn switch (non-Spawning lifecycle): the spawn-synthetic
    // bucket still exists but has already reached a ready state
    // (e.g. `Idle` after Connected but before KeyRenamed migrates).
    // Switch into it; KeyRenamed will migrate the active key when
    // it lands.
    if app.sessions.contains_key(&spawn_synthetic) {
        app.switch_active_session(spawn_synthetic);
        return;
    }

    // Running-bucket match by cwd: an auto_start project's running
    // bucket lives in app.sessions keyed by the real session UUID
    // (post-KeyRenamed migration). If the on-disk catalog hasn't
    // yet picked up that UUID, `list_projects` won't include it in
    // catalog_sessions and the disk-catalog lookup below would miss
    // — so we walk app.sessions looking for one whose `cwd_raw`
    // matches the project's path. Cheap (typically <10 buckets) and
    // robust to the disk-catalog refresh delay that caused
    // "first-click spawns a duplicate" before this guard landed.
    // `find_running_bucket_for_path` excludes the pre-connect
    // sentinel so the lookup is deterministic when forge was
    // launched from inside this project's directory.
    if let Some(path) = project_path.as_ref() {
        let path_str = path.to_string_lossy();
        if let Some(key) = app.find_running_bucket_for_path(path_str.as_ref()) {
            app.switch_active_session(key);
            return;
        }
    }

    // Fallback to the disk catalog's most recent session for this
    // project. If it's pooled, switch; otherwise dispatch a fresh
    // SpawnProject (covers cold projects with no live bucket).
    let lead_session_key = catalog_sessions.into_iter().next().map(|s| s.session);
    match lead_session_key {
        Some(key) if app.sessions.contains_key(&key) => {
            app.switch_active_session(key);
        }
        _ => {
            if let Some(workspace) = app.workspace.as_ref() {
                let launch_settings = crate::app::connect::session_launch_settings_for_startup(app);
                if let Err(err) = workspace.dispatch(forge_workspace::Command::SpawnProject {
                    project_name: resolved_name,
                    launch_settings,
                }) {
                    tracing::warn!(
                        target: crate::logging::targets::APP_SESSION,
                        project = %project_name,
                        error = %err,
                        "switch_to_project_lead: dispatch failed",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::app::session::{SessionLifecycleState, UiSession};

    /// Per-project click-gate: clicking a sibling project in the
    /// projects pane while its bucket is mid-spawn would land the
    /// user on the chat-view connecting stub (no session_id yet,
    /// input renders "Connecting to Claude Code…"). The gate refuses
    /// the click so the user waits instead. This test seeds a
    /// spawning bucket and asserts the active key doesn't move when
    /// `switch_to_project_lead` is invoked for its project.
    #[test]
    fn switch_to_project_lead_blocks_on_spawning_bucket() {
        let mut app = App::test_default();
        // The test workspace stub doesn't carry projects, but the
        // gate fires on the lifecycle of the bucket keyed by
        // `__spawn_<name>__`, which we can seed directly. The
        // project-name lookup falls into the
        // "list_projects didn't find it → resolved_name =
        // project_name" branch; the spawn-synthetic check then
        // matches our seeded bucket.
        let project_name = "forge";
        let spawn_synth =
            forge_workspace::SessionKey::from_session_id(format!("__spawn_{project_name}__"));
        let mut bucket = UiSession::new(spawn_synth.clone());
        bucket.lifecycle_state = SessionLifecycleState::Spawning;
        app.sessions.insert(spawn_synth.clone(), bucket);

        let initial_active = app.active_session_key.clone();
        switch_to_project_lead(&mut app, project_name);
        assert_eq!(
            app.active_session_key, initial_active,
            "spawning-bucket click must not change the active session",
        );
    }

    /// Sanity: a non-Spawning spawn-synthetic bucket (already
    /// reached `Idle` post-Connected but pre-KeyRenamed) IS
    /// switchable — the gate is lifecycle-specific, not a blanket
    /// "synthetic key is untouchable" rule. Without this, the
    /// mid-Connected-mid-KeyRenamed window would leave the user
    /// unable to click into their session.
    #[test]
    fn switch_to_project_lead_allows_idle_spawn_synthetic() {
        let mut app = App::test_default();
        let project_name = "forge";
        let spawn_synth =
            forge_workspace::SessionKey::from_session_id(format!("__spawn_{project_name}__"));
        let mut bucket = UiSession::new(spawn_synth.clone());
        bucket.lifecycle_state = SessionLifecycleState::Idle;
        app.sessions.insert(spawn_synth.clone(), bucket);

        switch_to_project_lead(&mut app, project_name);
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&spawn_synth),
            "non-spawning synthetic bucket should still be switchable",
        );
    }

    /// Worker-row mouse click switches the active session to the
    /// worker's bucket. The worker bucket must exist in `app.sessions`
    /// already (Connected has landed); switch is a no-op otherwise.
    #[test]
    fn switch_to_worker_swaps_active_session_when_bucket_exists() {
        let mut app = App::test_default();
        let worker_key = forge_workspace::SessionKey::from_session_id("worker-uuid");
        let bucket = UiSession::new(worker_key.clone());
        app.sessions.insert(worker_key.clone(), bucket);

        switch_to_worker(&mut app, worker_key.clone());
        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&worker_key),
            "worker bucket should become the active session",
        );
    }

    /// Worker row click without a backing bucket is a silent no-op
    /// (the Spawning window between SpawnWorker dispatch and the
    /// worker's first `Connected` event).
    #[test]
    fn switch_to_worker_silent_when_no_bucket() {
        let mut app = App::test_default();
        let initial_active = app.active_session_key.clone();
        let unknown = forge_workspace::SessionKey::from_session_id("not-in-sessions");
        switch_to_worker(&mut app, unknown);
        assert_eq!(
            app.active_session_key, initial_active,
            "missing-bucket worker click must not change active session",
        );
    }

    /// CloseWorker click dispatches `Command::CloseWorker` to the
    /// workspace. The stub workspace swallows the dispatch so the
    /// test only verifies the helper doesn't panic and the active
    /// session doesn't change as a side effect.
    #[test]
    fn close_worker_dispatch_is_idempotent_on_test_stub() {
        let mut app = App::test_default();
        let initial_active = app.active_session_key.clone();
        close_worker(&mut app, &forge_workspace::ProjectKey::new_for_test("forge"), "reviewer");
        assert_eq!(
            app.active_session_key, initial_active,
            "close_worker dispatch is fire-and-forget",
        );
    }
}
