//! Mouse dispatch while the diff overlay is active: scroll wheel over
//! rail and body, rail file jumps, and the body hit-test that routes
//! a click onto a diff line, a comment chip / turn / reply / button,
//! an expander, or a deleted-file header.

use super::comments::close_active_input_preserving_prior;
use super::layout::{
    FIRST_FILE_ROW_Y, SCROLL_LINES_PER_NOTCH, effective_view_mode, gutter_width_for,
    rail_width_for, split_layout,
};
use super::reviews::{submit_finish_review, toggle_reviews_list};
use super::state::DiffOverlayState;
use super::threads::apply_thread_action;
use super::types::{
    ActiveCommentInput, BodyRowKey, CommentRef, DiffViewMode, LineKey, RailRowKey, ThreadAction,
};
use crate::app::App;
use crate::app::input::InputState;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use forge_workspace::env::git_diff::hunks::FileStatus;

/// Outcome of a mouse interaction. Some interactions need access
/// to the full App (key event needs to fire `dispatch_prompt` for
/// the Esc submit path) which the inner `handle_*` borrow doesn't
/// have - surface them as effects the outer `handle_mouse` runs.
#[derive(Debug, Default)]
pub(super) struct MouseEffect {
    pub(super) redraw: bool,
    /// A comment-button click: run `action` on the thread at this key.
    /// Surfaced to the outer handler because persisting the status needs
    /// the App's workspace, which the inner overlay borrow can't reach.
    pub(super) thread_action: Option<(CommentRef, ThreadAction)>,
}

/// Handle a mouse event while the diff overlay is active.
///
/// Bindings:
/// - Scroll wheel over the rail → advance `rail_scroll`.
/// - Scroll wheel over the body → advance `doc_scroll` (the single
///   document scroll across all files).
/// - Left click on a file row in the FILES rail → jump `doc_scroll`
///   to that file's first row.
/// - Left click on a diff line in the body → open an inline comment
///   input anchored at that line. (If an input is already open, the
///   click cancels it before opening the new one.)
/// - Left click on a saved-comment chip → re-open that comment for
///   editing.
/// - Left click on a collapsed deleted file's header → expand it.
pub(crate) fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    // Finish-review modal: only its `[ Submit review ]` button is
    // clickable; every other click / scroll is swallowed so the diff
    // behind it can't be driven while the modal is up.
    if app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            let hit = app.diff_overlay.as_ref().and_then(|o| o.finish_submit_span).is_some_and(
                |(r, c0, c1)| mouse.row == r && mouse.column >= c0 && mouse.column < c1,
            );
            if hit {
                submit_finish_review(app);
            }
        }
        return;
    }
    // Reviews list open: any click closes it (click-away), matching the
    // jump dropdown; row selection stays keyboard-driven.
    if app.diff_overlay.as_ref().is_some_and(|o| o.reviews_open) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            toggle_reviews_list(app);
        }
        return;
    }
    // The diff renders inside the page border, so its content width is
    // the frame minus the two border columns.
    let content_width = app.cached_frame_area.width.saturating_sub(2);
    let effect = if let Some(overlay) = app.diff_overlay.as_mut() {
        match mouse.kind {
            MouseEventKind::ScrollUp => handle_scroll(overlay, mouse.column, content_width, false),
            MouseEventKind::ScrollDown => handle_scroll(overlay, mouse.column, content_width, true),
            MouseEventKind::Down(MouseButton::Left) => {
                handle_left_click(overlay, mouse.column, mouse.row, content_width)
            }
            // Drags, other buttons, and horizontal-wheel events have no
            // binding in the overlay.
            _ => MouseEffect::default(),
        }
    } else {
        MouseEffect::default()
    };
    if let Some((at, action)) = effect.thread_action {
        apply_thread_action(app, at, action);
    }
    if effect.redraw {
        app.needs_redraw = true;
    }
}

/// Whether a click column lands in the FILES rail: the rail spans
/// `[content_origin_col, content_origin_col + rail_width)` when shown.
fn column_in_rail(overlay: &DiffOverlayState, column: u16, content_width: u16) -> bool {
    let rail_width = rail_width_for(content_width);
    rail_width > 0
        && column >= overlay.content_origin_col
        && column < overlay.content_origin_col.saturating_add(rail_width)
}

fn handle_scroll(
    overlay: &mut DiffOverlayState,
    column: u16,
    content_width: u16,
    down: bool,
) -> MouseEffect {
    let in_rail = column_in_rail(overlay, column, content_width);
    if in_rail {
        if down {
            overlay.rail_scroll = overlay.rail_scroll.saturating_add(SCROLL_LINES_PER_NOTCH);
        } else {
            overlay.rail_scroll = overlay.rail_scroll.saturating_sub(SCROLL_LINES_PER_NOTCH);
        }
    } else if down {
        overlay.doc_scroll = overlay.doc_scroll.saturating_add(u32::from(SCROLL_LINES_PER_NOTCH));
    } else {
        overlay.doc_scroll = overlay.doc_scroll.saturating_sub(u32::from(SCROLL_LINES_PER_NOTCH));
    }
    MouseEffect { redraw: true, thread_action: None }
}

/// Resolve a left-click to an action. Returns the effect (redraw +
/// optional close-with-submit). Hits the rail, the narrow-tier
/// arrows, the pane body's banner ✕, a diff line, a chip, or a
/// hunk header in order.
pub(super) fn handle_left_click(
    overlay: &mut DiffOverlayState,
    column: u16,
    row: u16,
    content_width: u16,
) -> MouseEffect {
    // `⌄ jump` control on the stepper row → toggle the dropdown.
    if let Some((jr, c0, c1)) = overlay.jump_hint_span
        && row == jr
        && column >= c0
        && column < c1
    {
        if overlay.jump_open {
            overlay.jump_open = false;
        } else {
            overlay.open_jump();
        }
        return MouseEffect { redraw: true, thread_action: None };
    }
    // Any other click with the dropdown open closes it (click-away).
    if overlay.jump_open {
        overlay.jump_open = false;
        return MouseEffect { redraw: true, thread_action: None };
    }
    // Rail click: column inside the rail → rail row hit-test.
    if column_in_rail(overlay, column, content_width) {
        return handle_rail_click(overlay, row);
    }
    // Body click: column past rail+separator. Resolve via body_keys.
    // When the rail isn't rendered (terminal narrower than the split
    // threshold), the renderer paints a "too narrow" notice and
    // clears `body_keys` - clicks just no-op.
    handle_body_click(overlay, column, row)
}

fn handle_rail_click(overlay: &mut DiffOverlayState, row: u16) -> MouseEffect {
    // Rows are relative to the rail's top (below the page border and
    // any commit stepper).
    let row = row.saturating_sub(overlay.rail_origin_row);
    // The tree rail mixes directory headers (non-clickable) with
    // file leaves. We resolve the click by walking `rail_keys`
    // (parallel to the rendered rows) at offset `rail_scroll`.
    // The banner / rule / blank rows live at the head of the list
    // and don't scroll - they're at rows 0, 1, 2 relative to the
    // rail's top. The scrollable portion starts at row 3
    // (== FIRST_FILE_ROW_Y).
    let row_idx_in_keys = if row < FIRST_FILE_ROW_Y {
        usize::from(row)
    } else {
        let scrollable_offset = usize::from(row - FIRST_FILE_ROW_Y);
        usize::from(FIRST_FILE_ROW_Y)
            .saturating_add(scrollable_offset)
            .saturating_add(usize::from(overlay.rail_scroll))
    };
    let Some(key) = overlay.rail_keys.get(row_idx_in_keys).copied() else {
        return MouseEffect::default();
    };
    let RailRowKey::File { file_idx } = key else {
        // Banner / rule / blank / directory / untracked-notice -
        // non-clickable in v1.
        return MouseEffect::default();
    };
    if file_idx >= overlay.files.len() {
        return MouseEffect::default();
    }
    // Jump the document scroll to this file's first row. `starts` is in
    // file-sub-document space; add the commit-message block height so the
    // target lands in full-document space and the file actually pins in
    // commit mode (message_rows is 0 in whole-diff mode). Closing the
    // active editor on rail interaction preserves a reopened chip's prior.
    let file_start = overlay.doc_offsets().starts.get(file_idx).copied().unwrap_or(0);
    overlay.doc_scroll = overlay.message_rows.saturating_add(file_start);
    close_active_input_preserving_prior(overlay);
    MouseEffect { redraw: true, thread_action: None }
}

/// Resolve a left-click in the diff body to the row it landed on,
/// and for a split row to the old or new side of it.
fn handle_body_click(
    overlay: &mut DiffOverlayState,
    column: u16,
    row: u16,
) -> MouseEffect {
    // Empty body_keys means the renderer hasn't drawn yet (or drew
    // the too-short fallback). A click before the first real render
    // can't resolve anything; drop it silently.
    if overlay.body_keys.is_empty() {
        return MouseEffect::default();
    }
    if row < overlay.pane_origin_row {
        return MouseEffect::default();
    }
    let local_row = usize::from(row - overlay.pane_origin_row);
    // The first `body_head_rows` rows are pinned (the sticky file
    // header) and don't scroll, so they map directly to
    // `body_keys[local_row]`. Rows past the head add the tail scroll
    // the renderer applied this frame.
    let head = overlay.body_head_rows;
    let body_idx = if local_row < head {
        Some(local_row)
    } else {
        local_row.checked_add(overlay.body_tail_scroll)
    };
    let Some(idx) = body_idx else {
        return MouseEffect::default();
    };
    let Some(key) = overlay.body_keys.get(idx).copied() else {
        return MouseEffect::default();
    };
    match key {
        BodyRowKey::HunkRow { left, right } => {
            // Unified is one column, so either side resolves the
            // line. Split picks by the painted divider. An empty
            // picked side (blank half of an unbalanced row) is a no-op.
            let key = match effective_view_mode(overlay.view_mode, overlay.pane_width) {
                DiffViewMode::Unified => left.or(right),
                DiffViewMode::Split => {
                    // Guards a body mutated mid-click, paralleling
                    // `save_active_input`'s out-of-bounds arm. The gutter feeds
                    // the column widths; the divider does not depend on it.
                    let Some(file) = left.or(right).and_then(|key| overlay.files.get(key.file_idx))
                    else {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "diff_overlay_click_oob_file_idx",
                            message = "split click hit oob file_idx - body mutated mid-click?",
                            outcome = "skipped",
                            file_count = overlay.files.len(),
                        );
                        return MouseEffect::default();
                    };
                    let pane_local_col =
                        usize::from(column.saturating_sub(overlay.pane_origin_col));
                    let divider =
                        split_layout(gutter_width_for(file), overlay.pane_width).divider_col;
                    if pane_local_col < divider { left } else { right }
                }
            };
            match key {
                Some(key) => open_input_for_key(overlay, key),
                None => MouseEffect::default(),
            }
        }
        BodyRowKey::CommentTurn { at, turn_idx } => {
            reopen_comment_for_turn(overlay, at, Some(turn_idx))
        }
        BodyRowKey::CommentReply { at } => reopen_comment_for_turn(overlay, at, None),
        BodyRowKey::CommentCollapsed { at } => {
            let toggled = overlay.toggle_comment_collapse(at);
            MouseEffect { redraw: toggled, thread_action: None }
        }
        BodyRowKey::CommentButton { at, resolve, reopen } => {
            // Route to whichever applicable button the click lands in; a
            // click on the padding or a dim (inapplicable) action no-ops.
            let pane_col = column.saturating_sub(overlay.pane_origin_col);
            let hits = |span: Option<(u16, u16)>| {
                span.is_some_and(|(start, end)| pane_col >= start && pane_col < end)
            };
            if hits(resolve) {
                MouseEffect { redraw: true, thread_action: Some((at, ThreadAction::Resolve)) }
            } else if hits(reopen) {
                MouseEffect { redraw: true, thread_action: Some((at, ThreadAction::Reopen)) }
            } else {
                MouseEffect::default()
            }
        }
        BodyRowKey::FileHeader { file_idx } | BodyRowKey::DeletedCollapsed { file_idx } => {
            toggle_deleted_collapse(overlay, file_idx)
        }
        BodyRowKey::ContextExpander { file_idx } => expand_context(overlay, file_idx),
        BodyRowKey::EmptyState
        | BodyRowKey::CommentChip(_)
        | BodyRowKey::HunkHeader { .. }
        | BodyRowKey::InputRow(_)
        | BodyRowKey::CommitMessage
        | BodyRowKey::FileEndCap { .. } => MouseEffect::default(),
    }
}

/// Handle a context-expander click: reveal more of the file's pinned
/// wide snapshot in memory (no `git`). Bumps the file's shown-context
/// level and re-narrows from the cached wide hunks.
fn expand_context(overlay: &mut DiffOverlayState, file_idx: usize) -> MouseEffect {
    overlay.expand_file_context(file_idx);
    MouseEffect { redraw: true, thread_action: None }
}

/// Toggle a deleted file's expanded state (collapse <-> full body).
/// Only deleted files collapse; a click on any other file's header is
/// a no-op. Clears the file's measured height so the next frame
/// re-measures it at the new row count.
fn toggle_deleted_collapse(overlay: &mut DiffOverlayState, file_idx: usize) -> MouseEffect {
    if overlay.files.get(file_idx).map(|f| f.status) != Some(FileStatus::Deleted) {
        return MouseEffect::default();
    }
    if !overlay.deleted_expanded.insert(file_idx) {
        overlay.deleted_expanded.remove(&file_idx);
    }
    if let Some(slot) = overlay.measured_heights.get_mut(file_idx) {
        *slot = None;
    }
    MouseEffect { redraw: true, thread_action: None }
}

pub(super) fn open_input_for_key(overlay: &mut DiffOverlayState, key: LineKey) -> MouseEffect {
    // If an editor is already open at the same key, no-op so the
    // click doesn't reset its in-progress text. If at a different
    // key, abandon the in-progress edit (UI matches what GitHub does
    // - clicking elsewhere closes the open editor without saving).
    if let Some(existing) = overlay.active_input.as_ref()
        && existing.key == key
    {
        return MouseEffect::default();
    }
    // Close any existing editor (different line) before opening the
    // new one - preserves its prior_comment if it was a reopen.
    close_active_input_preserving_prior(overlay);
    let editor = InputState::new();
    overlay.active_input =
        Some(ActiveCommentInput { key, editor, prior_comment: None, edit_turn: None });
    MouseEffect { redraw: true, thread_action: None }
}

/// Reopen the saved comment `at` names for either a turn rewrite
/// (`edit_turn = Some(idx)` seeds the editor with that turn's text) or
/// a reply (`edit_turn = None` opens an empty editor that appends on
/// save). The saved entry is dropped so its chip vanishes WHILE editing
/// but stashed on `prior_comment` so Esc-cancel restores it - losing
/// review notes to a misclick-and-reflex-Esc would destroy the user's
/// work.
pub(super) fn reopen_comment_for_turn(
    overlay: &mut DiffOverlayState,
    at: CommentRef,
    edit_turn: Option<usize>,
) -> MouseEffect {
    let Some(pos) = overlay.comment_index_at(at) else {
        return MouseEffect::default();
    };
    let comment = overlay.comments.remove(pos);
    overlay.recompute_comment_counts();
    // Close any pre-existing editor on a different line so its
    // prior_comment survives (without this, A's prior would be
    // silently dropped when B's reopen runs).
    close_active_input_preserving_prior(overlay);
    let mut editor = InputState::new();
    // Seed the editor with the targeted turn's text (rewrite); a reply
    // starts empty. `insert_str` respects newlines so a saved
    // turn's multi-line shape is preserved.
    if let Some(idx) = edit_turn
        && let Some(turn) = comment.thread.comments.get(idx)
    {
        editor.insert_str(&turn.text);
    }
    overlay.active_input = Some(ActiveCommentInput {
        key: comment.key,
        editor,
        prior_comment: Some(comment),
        edit_turn,
    });
    MouseEffect { redraw: true, thread_action: None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diff_overlay::test_support::*;
    use crate::app::diff_overlay::types::{DiffScope, HunkComment, LineKey};
    use forge_workspace::env::git_diff::hunks::FileHunks;
    use std::path::PathBuf;

    #[test]
    fn rail_click_outside_rail_routes_to_body() {
        // A click past the rail routes into handle_body_click, which
        // finds no body_keys in a freshly-constructed state - no-redraw.
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 50, 4, 160);
        assert!(!effect.redraw);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn rail_click_on_banner_returns_no_redraw() {
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 0, 160); // Banner row.
        assert!(!effect.redraw);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn rail_click_beyond_file_list_returns_no_redraw() {
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 99, 160); // No file at this row.
        assert!(!effect.redraw);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn rail_click_at_narrow_tier_routes_to_body() {
        // Narrow tier: rail_width == 0 → click routes to body
        // hit-test, which finds no body_keys in a fresh state.
        let mut state = sample_state();
        let effect = handle_left_click(&mut state, 5, 4, 100);
        assert!(!effect.redraw);
        assert_eq!(state.doc_scroll, 0);
    }

    #[test]
    fn body_click_left_column_opens_comment_input_on_left_key() {
        // Split row with both columns present; a click in the left
        // half resolves to the left key (split picks by column).
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let left_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(left_key), right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41; // Past rail + separator on wide.
        state.pane_width = 119;
        // Left half: well clear of the divider at pane-local 65.
        let effect = handle_left_click(&mut state, 60, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(left_key));
    }

    #[test]
    fn body_click_right_column_opens_comment_input_on_right_key() {
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let left_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(left_key), right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Right half: past the divider at pane-local 65.
        let effect = handle_left_click(&mut state, 120, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(right_key));
    }

    /// At an odd pane width the divider sits a column right of the
    /// midpoint, so a click between the two is visually on the old side.
    #[test]
    fn body_click_just_left_of_the_divider_resolves_the_old_side() {
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let left_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 1 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(left_key), right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Pane-local 59 is the midpoint; the divider is at 60.
        let effect = handle_left_click(&mut state, 41 + 59, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(left_key));

        // The divider cell itself is ambiguous; it goes to the new side.
        state.active_input = None;
        let effect = handle_left_click(&mut state, 41 + 60, 2, 160);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(right_key));
    }

    /// Below `MIN_WIDTH_FOR_SPLIT` the renderer paints unified rows,
    /// which carry one side only. Resolving those as split returns the
    /// blank side and the click silently does nothing.
    #[test]
    fn body_click_in_a_narrow_pane_resolves_the_unified_row() {
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.body_keys = vec![BodyRowKey::HunkRow { left: None, right: Some(key) }];
        state.pane_origin_row = 0;
        state.pane_origin_col = 0;
        state.pane_width = 80;
        let effect = handle_left_click(&mut state, 5, 0, 80);
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(key));
    }

    #[test]
    fn body_click_on_empty_side_is_noop() {
        // Split-only: clicking the blank half of an unbalanced row
        // (left = None) is a no-op. Unified would resolve right.
        let mut state = sample_state();
        state.view_mode = DiffViewMode::Split;
        let right_key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: None, right: Some(right_key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        // Click in the (blank) LEFT half - left=None, so no editor opens.
        let effect = handle_left_click(&mut state, 60, 2, 160);
        assert!(!effect.redraw);
        assert!(state.active_input.is_none());
    }

    #[test]
    fn body_click_unified_resolves_either_column_to_the_line() {
        // Unified is one column: a click anywhere on the row opens the
        // comment, even the left half of an added/context row whose
        // key sits on the right. (Split would no-op the empty left.)
        let mut state = sample_state(); // view_mode defaults to Unified
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: None, right: Some(key) },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 2, 160); // left half
        assert!(effect.redraw);
        assert_eq!(state.active_input.as_ref().map(|i| i.key), Some(key));
    }

    #[test]
    fn rail_click_jumps_doc_scroll_to_file_offset() {
        let mut state = sample_state(); // 2 files
        // Give file 0 a measured height of 10 so file 1 starts at row 10.
        state.measured_heights = vec![Some(10), Some(4)];
        let effect = handle_left_click(&mut state, 5, 4, 160); // rail row 4 = file idx 1
        assert!(effect.redraw);
        assert_eq!(state.doc_scroll, 10, "rail click jumps to the file's document offset");
    }

    #[test]
    fn handle_mouse_hit_tests_the_rail_at_the_inner_content_width() {
        // handle_mouse derives the rail width from the page's INNER width
        // (frame minus the two border columns), so a rail click resolves
        // against the same geometry the renderer stashed. Simulate a
        // rendered 160-wide frame: 158-wide content, rail at column 1.
        let mut state = sample_state(); // 2 files, flat rail_keys
        state.measured_heights = vec![Some(10), Some(4)];
        state.content_origin_col = 1;
        state.rail_origin_row = 1;
        let mut app = App::test_default();
        app.diff_overlay = Some(state);
        app.cached_frame_area = ratatui::layout::Rect::new(0, 0, 160, 40);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,  // inside the rail (spans col 1..1+rail_width)
                row: 1 + 4, // rail top (1) + banner/rule/blank + file 1
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
        );

        assert_eq!(
            app.diff_overlay.as_ref().expect("overlay").doc_scroll,
            10,
            "the inner-width rail hit-test resolves file 1",
        );
    }

    #[test]
    fn body_click_on_deleted_header_toggles_expand() {
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "HEAD".to_owned(),
            vec![FileHunks {
                path: "gone.rs".into(),
                status: FileStatus::Deleted,
                hunks: vec![],
                oversize: false,
            }],
        );
        state.measured_heights = vec![Some(2)];
        state.body_keys = vec![BodyRowKey::FileHeader { file_idx: 0 }];
        state.body_head_rows = 1;
        state.pane_origin_row = 0;
        state.pane_origin_col = 24;
        state.pane_width = 120;
        // Click the pinned header (row 0, within the head). Column past
        // the rail so it routes to the body.
        let effect = handle_left_click(&mut state, 50, 0, 160);
        assert!(effect.redraw);
        assert!(state.deleted_expanded.contains(&0), "deleted header click expands");
        assert!(state.measured_heights[0].is_none(), "height invalidated on toggle");
        // Click again collapses.
        let effect = handle_left_click(&mut state, 50, 0, 160);
        assert!(effect.redraw);
        assert!(!state.deleted_expanded.contains(&0), "second click collapses again");
    }

    #[test]
    fn body_click_on_your_turn_reopens_that_turn() {
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key,
            path: "a.rs".into(),
            line: 7,
            comment_text: "needs unwrap fix".into(),
            commit: None,
            thread: user_thread("needs unwrap fix"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(key), right: Some(key) },
            BodyRowKey::CommentTurn { at: CommentRef { line: key, slot: 0 }, turn_idx: 0 },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 3, 160);
        assert!(effect.redraw);
        assert!(state.comments.is_empty(), "saved comment migrates back into the editor");
        let input = state.active_input.expect("editor reopened");
        assert_eq!(input.key, key);
        assert_eq!(input.edit_turn, Some(0), "the clicked turn is the edit target");
        assert_eq!(input.editor.lines().join("\n"), "needs unwrap fix");
    }

    #[test]
    fn click_on_jump_hint_toggles_dropdown() {
        let mut state = commit_mode_state();
        state.jump_hint_span = Some((1, 40, 46));
        let effect = handle_left_click(&mut state, 42, 1, 160);
        assert!(effect.redraw);
        assert!(state.jump_open, "click on the ⌄ control opens the dropdown");
        let effect = handle_left_click(&mut state, 42, 1, 160);
        assert!(effect.redraw);
        assert!(!state.jump_open, "a second click closes it");
    }

    #[test]
    fn click_away_closes_open_dropdown() {
        let mut state = commit_mode_state();
        state.open_jump();
        state.jump_hint_span = Some((1, 40, 46));
        let effect = handle_left_click(&mut state, 5, 10, 160);
        assert!(effect.redraw);
        assert!(!state.jump_open, "a click off the control closes the menu");
    }

    // ---- enter mode: commit mode when the target has commits ahead ----

    #[test]
    fn body_click_on_reply_opens_an_empty_editor() {
        let mut state = sample_state();
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        state.comments.push(HunkComment {
            key,
            path: "a.rs".into(),
            line: 7,
            comment_text: "note".into(),
            commit: None,
            thread: user_thread("note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        state.body_keys = vec![
            BodyRowKey::FileHeader { file_idx: 0 },
            BodyRowKey::HunkHeader { file_idx: 0, hunk_idx: 0 },
            BodyRowKey::HunkRow { left: Some(key), right: Some(key) },
            BodyRowKey::CommentReply { at: CommentRef { line: key, slot: 0 } },
        ];
        state.pane_origin_row = 0;
        state.pane_origin_col = 41;
        state.pane_width = 119;
        let effect = handle_left_click(&mut state, 60, 3, 160);
        assert!(effect.redraw);
        let input = state.active_input.expect("reply editor opened");
        assert_eq!(input.edit_turn, None, "a reply has no edit target");
        assert!(input.prior_comment.is_some(), "the thread is stashed for restore");
        assert!(input.editor.lines().join("\n").is_empty(), "the reply editor starts empty");
    }

    #[test]
    fn comment_button_routes_by_the_span_the_click_lands_in() {
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        let mut state = DiffOverlayState::new(
            PathBuf::from("/tmp"),
            "HEAD".to_owned(),
            vec![single_hunk_file("a.rs", vec![added_line("x", 1)])],
        );
        state.pane_origin_col = 0;
        state.pane_origin_row = 0;
        state.body_head_rows = 0;
        state.body_tail_scroll = 0;
        // An addressed card offers both buttons at distinct spans.
        let at = CommentRef { line: key, slot: 0 };
        state.body_keys =
            vec![BodyRowKey::CommentButton { at, resolve: Some((10, 19)), reopen: Some((22, 30)) }];

        assert_eq!(
            handle_body_click(&mut state, 12, 0).thread_action,
            Some((at, ThreadAction::Resolve)),
            "a click in the Resolve span fires Resolve",
        );
        assert_eq!(
            handle_body_click(&mut state, 25, 0).thread_action,
            Some((at, ThreadAction::Reopen)),
            "a click in the Reopen span fires Reopen",
        );
        assert_eq!(
            handle_body_click(&mut state, 20, 0).thread_action,
            None,
            "a click in the gap between the buttons no-ops",
        );
    }

    #[test]
    fn reopen_takes_the_comment_in_the_current_scope() {
        // The reopen twin of `save_replaces_only_the_same_scope_at_a_key`.
        // Reopening resolves by key, so with a co-located comment in
        // another scope it can pull the wrong one: the editor is seeded
        // from it, and saving then stamps THIS scope onto that thread -
        // re-scoping it durably and orphaning the one the user clicked.
        // The whole-diff comment is pushed FIRST so a key-only lookup
        // finds it before the commit-scoped one.
        let files = vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])];
        let mut overlay =
            DiffOverlayState::new(PathBuf::from("/tmp/repo"), "main".to_owned(), files);
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("aaa", "first")];
        overlay.scope = DiffScope::Commit(0);
        let key = LineKey { file_idx: 0, hunk_idx: 0, line_idx: 0 };
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "whole-diff note".to_owned(),
            commit: None,
            thread: user_thread("whole-diff note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });
        overlay.comments.push(HunkComment {
            key,
            path: "src/x.rs".to_owned(),
            line: 5,
            comment_text: "commit note".to_owned(),
            commit: Some("aaa".to_owned()),
            thread: user_thread("commit note"),
            authored_this_session: true,
            anchor_note: None,
            persisted: false,
        });

        reopen_comment_for_turn(&mut overlay, CommentRef { line: key, slot: 0 }, Some(0));

        let input = overlay.active_input.as_ref().expect("the reopen opens an editor");
        assert_eq!(
            input.editor.text(),
            "commit note",
            "the editor is seeded from the comment in the current scope",
        );
        assert_eq!(
            input.prior_comment.as_ref().and_then(|c| c.commit.as_deref()),
            Some("aaa"),
            "the stashed prior is the current scope's comment, so saving cannot re-scope another",
        );
        assert!(
            overlay.comments.iter().any(|c| c.commit.is_none()),
            "the co-located comment in another scope stays in the list",
        );
    }
}
