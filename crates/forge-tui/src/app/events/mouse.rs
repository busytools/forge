// ratatui geometry: terminal dims are u16, scroll math goes through f32
// for smooth-scroll. Casts (usize↔f32, f32→u16/usize) are inherent and bounded by
// terminal size.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss, clippy::cast_sign_loss)]

use super::super::selection::clear_selection;
use super::super::state::ScrollbarDragState;
use super::super::{App, MessageBlock, ScrollbarGeometry, SelectionKind, SelectionPoint};
use crossterm::event::{MouseEvent, MouseEventKind};

pub(super) const MOUSE_SCROLL_LINES: usize = 3;

struct MouseSelectionPoint {
    kind: SelectionKind,
    point: SelectionPoint,
}

pub(super) fn handle_mouse_event(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if start_scrollbar_drag(app, mouse) {
                return;
            }
            app.scrollbar_drag = None;
            if try_toggle_tool_call_at_click(app, mouse) {
                return;
            }
            if let Some(pt) = mouse_point_to_selection(app, mouse) {
                app.selection = Some(super::super::SelectionState {
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
            if let (Some(sel), Some(pt)) = (&mut app.selection, pt) {
                sel.end = pt.point;
            }
        }
        MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
            app.scrollbar_drag = None;
            if let Some(sel) = &mut app.selection {
                sel.dragging = false;
            }
        }
        _ => {}
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if app.selection.is_some() {
                clear_selection(app);
            }
            app.viewport_mut().scroll_up(MOUSE_SCROLL_LINES);
        }
        MouseEventKind::ScrollDown => {
            if app.selection.is_some() {
                clear_selection(app);
            }
            app.viewport_mut().scroll_down(MOUSE_SCROLL_LINES);
        }
        _ => {}
    }
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

    let vp = app.viewport_mut();
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
    let Some(MessageBlock::ToolCall(tc)) = app.messages_mut()[msg_idx].blocks.get_mut(block_idx)
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
