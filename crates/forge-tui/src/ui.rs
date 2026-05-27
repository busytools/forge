mod autocomplete;
mod chat;
mod chat_view;
mod config;
mod diff;
mod diff_overlay;
mod document_table;
pub(crate) mod format;
pub(crate) mod help;
mod highlight;
mod input;
pub(crate) mod inspector_pane;
pub mod launchpad;
pub(crate) mod layout;
mod markdown;
mod message;
pub(crate) mod peer_block;
pub mod projects_pane;
pub(crate) mod prompt;
pub(crate) mod theme;
mod tool_call;
pub mod top_bar;
mod two_column_list;
pub(crate) mod worker_status;
mod wrap;

pub use message::{SpinnerState, measure_message_height_cached};

use crate::app::ActiveView;
use crate::app::App;
use ratatui::Frame;
use ratatui::buffer::Buffer;

pub fn render(frame: &mut Frame, app: &mut App) {
    // Per-frame idempotency guard for `pane_hit_targets`. The
    // individual pane / overlay renderers push targets without first
    // clearing on inline-render branches; a double render pass (e.g.
    // the #217 throttle's scratch pass followed by the real
    // `terminal.draw` on a change frame) would otherwise accumulate
    // duplicates and corrupt the mouse handler's hit-test.
    app.pane_hit_targets.clear();

    match app.active_view {
        ActiveView::Chat => chat_view::render(frame, app),
        ActiveView::Plugins => config::render_plugins(frame, app),
        ActiveView::Mcp => config::render_mcp(frame, app),
        ActiveView::Launchpad => launchpad::render(frame, app),
        ActiveView::Diff => diff_overlay::render(frame, app),
    }
}

/// Stable hash of a rendered `Buffer`'s cells. Drives the app-level
/// render throttle (#217): the run loop renders into a scratch
/// `TestBackend`, hashes the result, and skips the real
/// `terminal.draw` flush when the hash matches the previously drawn
/// frame's. Includes the buffer area dimensions so a resize is
/// detected even when the visible cells happen to coincide.
#[must_use]
pub(crate) fn buffer_signature(buf: &Buffer) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    buf.area.width.hash(&mut hasher);
    buf.area.height.hash(&mut hasher);
    for cell in &buf.content {
        cell.symbol().hash(&mut hasher);
        // `Color` and `Modifier` derive `Hash` in ratatui; including
        // them so a same-text-different-colour frame still counts as
        // changed (e.g. a status row flipping from green to red).
        cell.fg.hash(&mut hasher);
        cell.bg.hash(&mut hasher);
        cell.modifier.hash(&mut hasher);
    }
    hasher.finish()
}

/// Side-effect hook fired by the run loop AFTER a real
/// `terminal.draw`, never after a scratch throttle render. Chat
/// cache-metric enforcement walks the cache + bumps log / warn
/// counters that the throttle's scratch render must not double on
/// change frames or advance "for free" on skip frames. The render
/// path itself stays free of these side effects.
pub fn emit_post_draw_metrics(app: &mut App) {
    if matches!(app.active_view, ActiveView::Chat) {
        chat::enforce_and_emit_cache_metrics(app);
    }
}

pub(crate) fn refresh_selection_snapshot(app: &mut App) {
    let Some(selection) = app.selection() else {
        return;
    };

    match (app.active_view, selection.kind) {
        (ActiveView::Chat, crate::app::SelectionKind::Chat) => {
            chat::refresh_selection_snapshot(app);
        }
        (ActiveView::Chat, crate::app::SelectionKind::Input) => {
            input::refresh_selection_snapshot(app);
        }
        _ => {}
    }
}

#[cfg(test)]
mod throttle_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_into_test_backend(width: u16, height: u16, app: &mut App) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(f, app)).expect("draw");
        terminal
    }

    #[test]
    fn buffer_signature_stable_for_unchanged_app() {
        // Two consecutive renders of a stable App must produce the
        // same signature - the contract the #217 throttle relies on
        // to decide whether to skip the real `terminal.draw`.
        let mut app = App::test_default();
        let term_a = render_into_test_backend(120, 40, &mut app);
        let sig_a = buffer_signature(term_a.backend().buffer());
        let term_b = render_into_test_backend(120, 40, &mut app);
        let sig_b = buffer_signature(term_b.backend().buffer());
        assert_eq!(sig_a, sig_b, "stable App must produce equal render signatures");
    }

    #[test]
    fn buffer_signature_differs_after_state_mutation() {
        // Companion to `buffer_signature_stable_for_unchanged_app`.
        // Stability alone passes for any pure function; the
        // throttle's correctness also depends on a state mutation
        // that visibly changes rendered cells producing a different
        // signature. Without this, a regression that returned a
        // constant hash would skip every frame and freeze the UI.
        let mut app = App::test_default();
        let term_a = render_into_test_backend(120, 40, &mut app);
        let sig_a = buffer_signature(term_a.backend().buffer());
        app.help_open = true;
        let term_b = render_into_test_backend(120, 40, &mut app);
        let sig_b = buffer_signature(term_b.backend().buffer());
        assert_ne!(sig_a, sig_b, "visible-state mutation must change the signature");
    }

    #[test]
    fn buffer_signature_differs_on_resize() {
        // Different terminal dimensions must produce different
        // signatures so a resize forces the real draw even if the
        // cell content happens to overlap.
        let mut app = App::test_default();
        let small = render_into_test_backend(80, 24, &mut app);
        let large = render_into_test_backend(120, 40, &mut app);
        assert_ne!(
            buffer_signature(small.backend().buffer()),
            buffer_signature(large.backend().buffer()),
            "resize must invalidate the signature"
        );
    }

    #[test]
    fn pane_hit_targets_stable_under_double_render() {
        // Without the idempotency guard at the top of `render`, the
        // inline-pane branches' `pane_hit_targets.push(...)` would
        // double on a second render pass and corrupt the mouse
        // handler's hit-test. This pins the guard.
        let mut app = App::test_default();
        let _t = render_into_test_backend(120, 40, &mut app);
        let snapshot_first = app.pane_hit_targets.clone();
        let _t = render_into_test_backend(120, 40, &mut app);
        let snapshot_second = app.pane_hit_targets.clone();
        assert_eq!(
            snapshot_first.len(),
            snapshot_second.len(),
            "double render must not accumulate pane_hit_targets"
        );
    }
}
