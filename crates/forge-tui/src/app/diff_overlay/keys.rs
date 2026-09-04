//! Key dispatch while the diff overlay is active: the routing order
//! across the dictate take, the emoji picker, the Finish-review modal,
//! the reviews list, and the comment editor, plus the no-editor
//! bindings (scroll, stepper, jump, view toggle) and bracketed paste.

use std::time::Instant;

use super::comments::{cancel_active_input, save_active_input};
use super::lifecycle::spawn_scope_scan;
use super::reviews::{
    close_with_submit, handle_reviews_list_key, submit_finish_review, toggle_reviews_list,
};
use super::state::DiffOverlayState;
use super::threads::hydrate_threads;
use super::types::{DiffViewMode, NavOutcome};
use crate::app::App;
use crate::app::input::TypedChar;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle a key while the diff overlay is active.
///
/// Routing depends on whether an inline comment editor is open:
/// - Editor open:
///   - `Esc` cancels the editor and returns focus to the diff.
///   - `Enter` (plain, no modifier) saves the edit.
///   - All other keys flow into the editor (typing, cursor
///     movement, paste-via-bracket, undo/redo, etc.).
/// - No editor open:
///   - `Esc` closes the overlay; a submit seals this session's authored
///     comments into a numbered review and nudges the agent (one line)
///     to address it via the review MCP. The nudge fires synchronously
///     through `input_submit::dispatch_review_nudge` so the user sees the
///     bubble appear immediately.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) {
    // A stamped dictate warning dies with the next overlay key.
    if let Some(overlay) = app.diff_overlay.as_mut().filter(|o| o.dictate_notice.is_some()) {
        overlay.dictate_notice = None;
        app.needs_redraw = true;
    }
    // A live take owns the first Esc on every surface it can be started
    // from: it is abandoned and the editor underneath stands.
    if matches!(key.code, KeyCode::Esc)
        && app.emoji.is_none()
        && crate::app::dictate::abandon_take(app)
    {
        app.needs_redraw = true;
        return;
    }
    // A paste queued earlier in this drain cycle owns any editing-like
    // key that follows it - without this a chunked paste's trailing
    // newline saves the comment instead of landing in the text.
    if app.has_focused_text_input() && crate::app::keys::should_ignore_key_during_paste(app, key) {
        return;
    }
    // A picker left open by a mouse-driven editor close has nowhere to
    // insert into.
    if app.emoji.is_some() && !app.has_focused_text_input() {
        crate::app::emoji::deactivate(app);
    }
    // The emoji picker is the innermost surface: while it is open it owns
    // Esc, Enter and the arrows. Without this, `:` then Esc would fall
    // through to the overlay's Esc - which submits the review.
    if app.emoji.is_some() && crate::app::keys::handle_emoji_key(app, key) {
        app.needs_redraw = true;
        return;
    }
    // Finish-review modal captures keys while open: type the overview,
    // Ctrl+Enter submits, Esc dismisses back to the diff.
    if app.diff_overlay.as_ref().is_some_and(|o| o.finish_review.is_some()) {
        handle_finish_review_key(app, key);
        return;
    }
    // The reviews list captures keys while open: navigate rows, Enter to
    // jump, `l` / Esc to close.
    if app.diff_overlay.as_ref().is_some_and(|o| o.reviews_open) {
        handle_reviews_list_key(app, key);
        return;
    }
    let has_input = app.diff_overlay.as_ref().is_some_and(|o| o.active_input.is_some());
    if has_input {
        match key.code {
            KeyCode::Esc => {
                app.paste_burst.on_non_char_key(Instant::now());
                cancel_active_input(app);
            }
            // Enter is only a save when it is really a keypress. Mid-burst
            // - or in the window right after one - it belongs to the
            // dictated / pasted payload, so it goes to the buffer instead.
            KeyCode::Enter if app.paste_burst.on_enter(Instant::now()) => {}
            KeyCode::Enter if !key.modifiers.contains(crossterm::event::KeyModifiers::SHIFT) => {
                // An explicit save is a deliberate act: the user
                // submitted without the take's words, so the take is
                // abandoned rather than orphaned to the fallback.
                crate::app::dictate::abandon_take(app);
                save_active_input(app);
            }
            _ => route_key_into_review_editor(app, key),
        }
        return;
    }
    // Jump dropdown captures keys while open: move / confirm / close.
    // Esc closes the menu (not the overlay).
    if app.diff_overlay.as_ref().is_some_and(|o| o.jump_open) {
        handle_jump_key(app, key);
        return;
    }
    match key.code {
        KeyCode::Esc => close_with_submit(app),
        KeyCode::Char('t') => toggle_view_mode(app),
        KeyCode::Up => scroll_doc(app, false),
        KeyCode::Down => scroll_doc(app, true),
        KeyCode::PageUp => scroll_doc_page(app, false),
        KeyCode::PageDown => scroll_doc_page(app, true),
        // Commit stepper: prev/next commit + open the jump dropdown. All
        // no-ops in whole-diff-only mode (no commits), so they don't
        // shadow anything there.
        KeyCode::Char('[') | KeyCode::Left => step_commit(app, false),
        KeyCode::Char(']') | KeyCode::Right => step_commit(app, true),
        KeyCode::Char('a') => toggle_all_changes(app),
        KeyCode::Char('j') => open_jump(app),
        KeyCode::Char('l') => toggle_reviews_list(app),
        _ => {}
    }
}

/// Route a key while the Finish-review modal is open: Esc dismisses it
/// back to the diff (keep editing), Ctrl+Enter submits, everything else
/// flows into the overview editor.
fn handle_finish_review_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.paste_burst.on_non_char_key(Instant::now());
            if let Some(o) = app.diff_overlay.as_mut() {
                o.finish_review = None;
                app.needs_redraw = true;
            }
        }
        KeyCode::Enter if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) => {
            // An explicit submit is a deliberate act: the take is
            // abandoned rather than orphaned to the fallback.
            crate::app::dictate::abandon_take(app);
            submit_finish_review(app);
        }
        _ => route_key_into_review_editor(app, key),
    }
}

/// Route a key the review editors don't claim for their own semantics
/// into whichever editor has focus. Printable characters go through the
/// shared paste-burst detector so a dictation run coalesces into one
/// payload; everything else is ordinary `TextArea` editing.
fn route_key_into_review_editor(app: &mut App, key: KeyEvent) {
    let printable = match (key.code, key.modifiers) {
        (KeyCode::Char(c), m) if crate::app::keys::is_printable_text_modifiers(m) => Some(c),
        _ => None,
    };
    if let Some(c) = printable {
        // Only offer the picker for a character that actually landed - a
        // `:` swallowed into a paste burst is payload, not a trigger.
        if app.type_char(c, Instant::now()) == TypedChar::Inserted && c == ':' {
            crate::app::emoji::activate(app);
        }
    } else {
        app.paste_burst.on_non_char_key(Instant::now());
        if let Some(input) = app.focused_input_mut() {
            let _ = input.handle_key(key);
        }
        crate::app::emoji::sync_with_cursor(app);
    }
    app.needs_redraw = true;
}

/// Route a key while the jump dropdown is open. `↑↓` move the
/// highlight, `Enter` navigates to the highlighted scope, and `Esc`
/// (or `j`) closes the menu without touching the overlay.
fn handle_jump_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up => {
            if let Some(o) = app.diff_overlay.as_mut() {
                o.jump_move(false);
                app.needs_redraw = true;
            }
        }
        KeyCode::Down => {
            if let Some(o) = app.diff_overlay.as_mut() {
                o.jump_move(true);
                app.needs_redraw = true;
            }
        }
        KeyCode::Enter => {
            let outcome = app.diff_overlay.as_mut().map(DiffOverlayState::jump_confirm);
            if let Some(outcome) = outcome {
                after_nav(app, outcome);
            }
        }
        KeyCode::Esc | KeyCode::Char('j') => {
            if let Some(o) = app.diff_overlay.as_mut() {
                o.jump_open = false;
                app.needs_redraw = true;
            }
        }
        _ => {}
    }
}

/// Step to the prev/next commit and spawn its scan if uncached.
pub(super) fn step_commit(app: &mut App, forward: bool) {
    let outcome = app.diff_overlay.as_mut().and_then(|o| o.step_commit(forward));
    if let Some(outcome) = outcome {
        after_nav(app, outcome);
    }
}

/// Toggle between the current commit and the whole-branch diff (`a`),
/// spawning the target scope's scan when it isn't cached. No-op in
/// whole-diff-only mode.
pub(super) fn toggle_all_changes(app: &mut App) {
    let outcome = app.diff_overlay.as_mut().and_then(DiffOverlayState::toggle_all_changes);
    if let Some(outcome) = outcome {
        after_nav(app, outcome);
    }
}

/// Open the jump dropdown (commit mode only).
pub(super) fn open_jump(app: &mut App) {
    if let Some(o) = app.diff_overlay.as_mut()
        && !o.commits.is_empty()
    {
        o.open_jump();
        app.needs_redraw = true;
    }
}

/// After a navigation, spawn the scope's scan when it wasn't cached, and
/// request a redraw. The scan lands back through the overlay event
/// channel (see [`super::lifecycle::spawn_scope_fetch`] /
/// [`super::lifecycle::drain_events`]).
pub(super) fn after_nav(app: &mut App, outcome: NavOutcome) {
    match outcome {
        NavOutcome::NeedsScan(scope) => spawn_scope_scan(app, scope),
        // A cached scope installs its files without a scan, so this is
        // the only chance to rebuild its cards. They are a projection of
        // the store, and the copy left over from the last visit predates
        // whatever happened in the scope just left.
        NavOutcome::Ready => hydrate_threads(app),
    }
    app.needs_redraw = true;
}

/// Flip the body layout (unified <-> split) and drop the measured
/// heights - the two modes have different row counts. The span cache
/// is layout-independent and stays intact, so the toggle is instant
/// (no re-highlight).
fn toggle_view_mode(app: &mut App) {
    if let Some(overlay) = app.diff_overlay.as_mut() {
        overlay.view_mode = match overlay.view_mode {
            DiffViewMode::Unified => DiffViewMode::Split,
            DiffViewMode::Split => DiffViewMode::Unified,
        };
        overlay.invalidate_measured_heights();
        app.needs_redraw = true;
    }
}

/// Step the document scroll by one row. The renderer clamps against
/// the document height and viewport each frame, so this just nudges
/// `doc_scroll` and lets render bound it.
fn scroll_doc(app: &mut App, down: bool) {
    if let Some(overlay) = app.diff_overlay.as_mut() {
        overlay.doc_scroll = if down {
            overlay.doc_scroll.saturating_add(1)
        } else {
            overlay.doc_scroll.saturating_sub(1)
        };
        app.needs_redraw = true;
    }
}

/// Page the document scroll by roughly a viewport (the last rendered
/// frame height minus the hint-bar row). Render clamps the result.
fn scroll_doc_page(app: &mut App, down: bool) {
    let page = u32::from(app.cached_frame_area.height.saturating_sub(1)).max(1);
    if let Some(overlay) = app.diff_overlay.as_mut() {
        overlay.doc_scroll = if down {
            overlay.doc_scroll.saturating_add(page)
        } else {
            overlay.doc_scroll.saturating_sub(page)
        };
        app.needs_redraw = true;
    }
}

/// Queue bracketed paste for whichever review editor has focus - the
/// inline comment editor or the Finish-review overview. Returns `true`
/// when the paste was accepted. Pastes with no editor open are dropped -
/// there's nothing for them to land on - but a DEBUG log fires so a user
/// reporting "my paste disappeared" can be triaged from logs.
///
/// The payload goes through the same queue the chat draft uses, so a
/// large paste collapses to a `[Pasted Text N]` block here too instead
/// of unrolling hundreds of rows into the comment box.
pub(crate) fn handle_paste(app: &mut App, text: &str) -> bool {
    if !app.has_focused_text_input() {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_paste_dropped_no_editor",
            message = "paste in Diff view without an open review editor - dropped",
            outcome = "dropped",
            paste_chars = text.chars().count(),
        );
        return false;
    }
    app.queue_paste_text(text);
    app.needs_redraw = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diff_overlay::test_support::*;
    use crate::app::diff_overlay::types::{CachedScan, DiffScope, FinishReviewState};
    use crate::app::input::InputState;
    use crate::app::view::{ActiveView, set_active_view};
    use forge_workspace::env::git_diff::hunks::FileStatus;
    use forge_workspace::{DictateOutcome, SessionUpdate};

    /// A take resolved while the diff comment editor is focused lands
    /// its words into the editor - not into the chat draft.
    #[test]
    fn a_take_lands_in_the_focused_comment_editor() {
        let mut app = app_with_comment_editor();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
        );

        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key: key.clone(),
                generation: 1,
                outcome: DictateOutcome::Landed {
                    text: "dictated words".to_owned(),
                    truncated: false,
                },
            },
        );
        let after = overlay(&app);
        assert_eq!(
            after.active_input.as_ref().map(|input| input.editor.text().clone()).as_deref(),
            Some("dictated words"),
            "the words land in the focused comment editor"
        );
        assert!(app.input().text().is_empty(), "the chat draft keeps nothing");
    }

    /// Esc ownership: with a take live, the first Esc abandons the take
    /// and the comment editor stands; only the next Esc cancels it.
    #[test]
    fn esc_abandons_the_take_before_cancelling_the_editor() {
        let mut app = app_with_comment_editor();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key, floor_db: -50.0, generation: 1 },
        );
        if let Some(ws) = app.workspace.as_ref() {
            ws.enable_test_dispatch_intercept();
        }

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

        let after = overlay(&app);
        assert!(after.active_input.is_some(), "the first Esc leaves the editor open");
        let dispatched = app.workspace.as_ref().map(|ws| ws.drain_test_dispatch_buffer());
        let Some(dispatched) = dispatched else { panic!("test_default carries a workspace") };
        assert!(
            dispatched
                .iter()
                .any(|command| matches!(command, forge_workspace::Command::DictateStop { .. })),
            "Esc dispatched the abandon: {dispatched:?}"
        );
    }

    /// The sister case: with no take live, Esc cancels the editor as
    /// before.
    #[test]
    fn esc_without_a_take_still_cancels_the_editor() {
        let mut app = app_with_comment_editor();

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

        let after = overlay(&app);
        assert!(after.active_input.is_none(), "no take, Esc cancels the editor");
    }

    /// The abandon clears the indicator optimistically, so a second
    /// Esc reaches the surface instead of being eaten by the still-live
    /// take while the async echo is in flight.
    #[test]
    fn the_second_esc_reaches_the_editor_once_the_take_is_abandoned() {
        let mut app = app_with_comment_editor();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
        );
        if let Some(ws) = app.workspace.as_ref() {
            ws.enable_test_dispatch_intercept();
        }

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(
            app.sessions.get(&key).expect("bucket").dictate.is_none(),
            "the abandon clears the indicator up front"
        );
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        let after = overlay(&app);
        assert!(after.active_input.is_none(), "the second Esc cancels the editor");
        let dispatched = app.workspace.as_ref().map(|ws| ws.drain_test_dispatch_buffer());
        let Some(dispatched) = dispatched else { panic!("test_default carries a workspace") };
        let stops = dispatched
            .iter()
            .filter(|command| matches!(command, forge_workspace::Command::DictateStop { .. }))
            .count();
        assert_eq!(stops, 1, "exactly one stop dispatches; the second Esc is the editor's");
    }

    /// An explicit save of the comment editor abandons its live take:
    /// the user submitted without the words, a deliberate act.
    #[test]
    fn saving_the_comment_abandons_the_live_take() {
        let mut app = app_with_comment_editor();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key, floor_db: -50.0, generation: 1 },
        );
        if let Some(ws) = app.workspace.as_ref() {
            ws.enable_test_dispatch_intercept();
        }
        if let Some(input) = app.diff_overlay.as_mut().and_then(|o| o.active_input.as_mut()) {
            input.editor.insert_str("typed first");
        }

        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        let after = overlay(&app);
        assert_eq!(after.comments.len(), 1, "the comment saves its own text");
        assert_eq!(
            after.comments[0].comment_text, "typed first",
            "the take's words are not in the saved comment, the take was still recording"
        );
        let key = app.active_session_key.clone().expect("active session");
        assert!(
            app.sessions.get(&key).expect("bucket").dictate.is_none(),
            "the save abandons the live take"
        );
        let dispatched = app.workspace.as_ref().map(|ws| ws.drain_test_dispatch_buffer());
        let Some(dispatched) = dispatched else { panic!("test_default carries a workspace") };
        assert!(
            dispatched
                .iter()
                .any(|command| matches!(command, forge_workspace::Command::DictateStop { .. })),
            "the abandon dispatches the stop: {dispatched:?}"
        );
    }

    /// An explicit Ctrl+Enter of the finish-review modal abandons its
    /// live take the same way.
    #[test]
    fn submitting_the_finish_review_abandons_the_live_take() {
        let mut app = app_with_comment_editor();
        app.diff_overlay.as_mut().expect("overlay").finish_review =
            Some(FinishReviewState { editor: crate::app::input::InputState::new() });
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key, floor_db: -50.0, generation: 1 },
        );
        if let Some(ws) = app.workspace.as_ref() {
            ws.enable_test_dispatch_intercept();
        }

        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::CONTROL),
        );

        let key = app.active_session_key.clone().expect("active session");
        assert!(
            app.sessions.get(&key).expect("bucket").dictate.is_none(),
            "the submit abandons the live take"
        );
    }

    /// A truncated take warns where its words landed: the overlay's
    /// notice line, not only the chat composer's notice row.
    #[test]
    fn a_truncated_take_warns_on_the_landing_overlay() {
        let mut app = app_with_comment_editor();
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
        );

        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key,
                generation: 1,
                outcome: DictateOutcome::Landed {
                    text: "half a thought".to_owned(),
                    truncated: true,
                },
            },
        );
        let after = overlay(&app);
        assert_eq!(
            after.active_input.as_ref().map(|input| input.editor.text().clone()).as_deref(),
            Some("half a thought"),
            "the truncated words land in the comment editor"
        );
        assert_eq!(
            after.dictate_notice.as_deref(),
            Some(crate::app::dictate::truncated_notice_text()),
            "the truncation warning rides the overlay"
        );
    }

    /// A take resolved while the Finish-review modal is open lands its
    /// words into the overview editor - not into the chat draft.
    #[test]
    fn a_take_lands_in_the_finish_review_overview() {
        let mut app = app_with_comment_editor();
        app.diff_overlay.as_mut().expect("overlay").finish_review =
            Some(FinishReviewState { editor: crate::app::input::InputState::new() });
        let key = app.active_session_key.clone().expect("test_default has an active bucket");
        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateStarted { key: key.clone(), floor_db: -50.0, generation: 1 },
        );

        crate::app::events::apply_session_update(
            &mut app,
            SessionUpdate::DictateEnded {
                key,
                generation: 1,
                outcome: DictateOutcome::Landed {
                    text: "overview words".to_owned(),
                    truncated: false,
                },
            },
        );
        let after = overlay(&app);
        assert_eq!(
            after.finish_review.as_ref().map(|finish| finish.editor.text().clone()).as_deref(),
            Some("overview words"),
            "the words land in the overview editor"
        );
        assert!(app.input().text().is_empty(), "the chat draft keeps nothing");
    }

    #[test]
    fn t_key_toggles_view_mode_and_clears_height_cache() {
        let mut app = App::test_default();
        let mut state = sample_state();
        state.measured_heights = vec![Some(10), Some(4)];
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('t')));
        let o = app.diff_overlay.as_ref().expect("overlay");
        assert_eq!(o.view_mode, DiffViewMode::Split);
        assert!(
            o.measured_heights.iter().all(Option::is_none),
            "height cache invalidated on toggle",
        );
    }

    #[test]
    fn down_key_advances_doc_scroll() {
        let mut app = App::test_default();
        app.diff_overlay = Some(sample_state());
        set_active_view(&mut app, ActiveView::Diff);
        handle_key(&mut app, KeyEvent::from(KeyCode::Down));
        assert_eq!(app.diff_overlay.as_ref().expect("overlay").doc_scroll, 1);
    }

    /// speech-to-text dictation arrives as individual keystrokes including
    /// Enter for sentence breaks. An Enter mid-burst used to hit
    /// `save_active_input`, closing the editor so the REST of the dictated
    /// sentence landed on the diff view's single-letter shortcuts - `t`
    /// toggling the view mode, `j` opening the jump menu, and so on.
    #[test]
    fn enter_during_a_dictation_burst_does_not_save_the_comment() {
        let mut app = app_with_comment_editor();
        start_dictation_burst(&mut app, Instant::now());

        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        let after = overlay(&app);
        assert!(after.active_input.is_some(), "the editor must stay open mid-burst");
        assert!(after.comments.is_empty(), "nothing is saved mid-burst");
    }

    /// The sister case: with no burst in flight, Enter still means save.
    /// This is the per-site semantic that must NOT be shared away.
    #[test]
    fn plain_enter_still_saves_the_comment() {
        let mut app = app_with_comment_editor();
        if let Some(input) = app.diff_overlay.as_mut().and_then(|o| o.active_input.as_mut()) {
            input.editor.insert_str("a real comment");
        }

        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        let after = overlay(&app);
        assert!(after.active_input.is_none(), "plain Enter closes the editor");
        assert_eq!(after.comments.len(), 1, "plain Enter saves the comment");
    }

    /// Typed characters have to reach the editor through the burst
    /// detector, so a dictated run coalesces instead of arriving as
    /// individual keystrokes.
    #[test]
    fn typed_characters_feed_the_burst_detector() {
        let mut app = app_with_comment_editor();

        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('b')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('c')));

        assert!(
            app.paste_burst.is_buffering(),
            "three characters at test speed must register as a burst, not three inserts"
        );
    }

    #[test]
    fn typing_a_shortcode_opens_the_picker_in_the_comment_editor() {
        let mut app = app_with_comment_editor();

        type_text(&mut app, ":roc");

        let state = app.emoji.as_ref().expect("picker open");
        assert_eq!(state.query, "roc");
        assert!(state.candidates.iter().any(|e| e.name == "rocket"));
    }

    /// The bite: in the /diff overlay Esc already means "finish review".
    /// With the picker open it must dismiss the PICKER and go no further,
    /// or typing `:` then Esc submits a review.
    #[test]
    fn esc_with_the_picker_open_dismisses_only_the_picker() {
        let mut app = app_with_comment_editor();
        type_text(&mut app, ":roc");

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

        assert!(app.emoji.is_none(), "Esc closes the picker");
        let after = overlay(&app);
        assert!(after.active_input.is_some(), "the comment editor stays open");
        assert!(after.finish_review.is_none(), "Esc must not reach finish-review");
        assert_eq!(app.active_view, ActiveView::Diff, "the overlay stays open");
    }

    /// A second Esc, with no picker in the way, resumes the normal
    /// meaning - cancel the editor.
    #[test]
    fn esc_after_dismissing_the_picker_cancels_the_editor() {
        let mut app = app_with_comment_editor();
        type_text(&mut app, ":roc");
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));

        assert!(overlay(&app).active_input.is_none(), "the editor is cancelled");
    }

    /// Enter belongs to the picker while it is open, so it must not save
    /// the comment out from under a half-typed shortcode.
    #[test]
    fn enter_with_the_picker_open_inserts_the_emoji_and_keeps_editing() {
        let mut app = app_with_comment_editor();
        type_text(&mut app, ":rocket");

        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        assert!(app.emoji.is_none(), "the picker closes on confirm");
        let after = overlay(&app);
        assert!(after.active_input.is_some(), "the editor stays open so typing continues");
        assert!(after.comments.is_empty(), "Enter on the picker is not a save");
        let text = after.active_input.as_ref().expect("editor").editor.text();
        assert_eq!(text, "\u{1F680}", "the whole :rocket token became the glyph");
    }

    #[test]
    fn typing_the_closing_colon_lands_the_glyph() {
        let mut app = app_with_comment_editor();

        type_text(&mut app, ":tada:");

        assert!(app.emoji.is_none());
        let text = overlay(&app).active_input.as_ref().expect("editor").editor.text();
        assert_eq!(text, "\u{1F389}");
    }

    /// A URL in a review comment must not pop a picker.
    #[test]
    fn a_url_does_not_open_the_picker() {
        let mut app = app_with_comment_editor();

        type_text(&mut app, "see http://x.dev");

        assert!(app.emoji.is_none(), "`:` mid-word is not a trigger");
        let text = overlay(&app).active_input.as_ref().expect("editor").editor.text();
        assert_eq!(text, "see http://x.dev");
    }

    /// The picker has to work in the Finish-review overview too - that is
    /// the whole point of hanging it off the shared substrate.
    #[test]
    fn the_picker_works_in_the_finish_review_overview() {
        let mut app = app_with_comment_editor();
        if let Some(o) = app.diff_overlay.as_mut() {
            o.active_input = None;
            o.finish_review = Some(FinishReviewState { editor: InputState::new() });
        }

        type_text(&mut app, ":rocket");
        assert!(app.emoji.is_some(), "picker opens over the modal");
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));

        let after = overlay(&app);
        assert!(after.finish_review.is_some(), "Enter on the picker does not submit the review");
        let text = after.finish_review.as_ref().expect("modal").editor.text();
        assert_eq!(text, "\u{1F680}");
    }

    #[test]
    fn bracket_keys_step_commits() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char(']')));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(1));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('[')));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(0));
    }

    #[test]
    fn arrow_keys_step_commits() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Right));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(1));
        handle_key(&mut app, KeyEvent::from(KeyCode::Left));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(0));
    }

    #[test]
    fn j_opens_jump_dropdown_seeded_on_current() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        assert!(overlay(&app).jump_open);
        assert_eq!(overlay(&app).jump_selected, 1, "scope Commit(0) → dropdown row 1");
    }

    #[test]
    fn jump_dropdown_move_then_enter_navigates() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Down));
        assert_eq!(overlay(&app).jump_selected, 2);
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter));
        assert!(!overlay(&app).jump_open, "confirm closes the menu");
        assert_eq!(overlay(&app).scope, DiffScope::Commit(1), "navigates to the picked commit");
    }

    #[test]
    fn jump_dropdown_esc_closes_menu_not_overlay() {
        let mut app = app_with_commit_overlay();
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        assert!(!overlay(&app).jump_open, "menu closed");
        assert!(app.diff_overlay.is_some(), "overlay stays open");
        assert_eq!(app.active_view, ActiveView::Diff, "still in the diff view");
    }

    #[test]
    fn bracket_and_j_are_noops_in_whole_diff_only_mode() {
        let mut app = App::test_default();
        app.diff_overlay = Some(sample_state());
        set_active_view(&mut app, ActiveView::Diff);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char(']')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('j')));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(overlay(&app).scope, DiffScope::WholeDiff);
        assert!(!overlay(&app).jump_open, "no dropdown without commits");
        assert!(app.diff_overlay.is_some());
    }

    #[test]
    fn a_key_toggles_between_commit_and_all_changes() {
        let mut app = app_with_commit_overlay();
        if let Some(o) = app.diff_overlay.as_mut() {
            o.whole_diff_cache = Some(CachedScan {
                files: vec![one_file("x.rs", FileStatus::Modified)],
                scanner_ok: true,
            });
        }
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(overlay(&app).scope, DiffScope::WholeDiff, "a from a commit → all changes");
        assert_eq!(overlay(&app).last_commit, Some(0), "the commit is remembered");
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('a')));
        assert_eq!(overlay(&app).scope, DiffScope::Commit(0), "a again → back to the commit");
    }

    #[test]
    fn finish_review_esc_dismisses_back_to_diff() {
        let mut app = App::test_default();
        let mut state = sample_state();
        state.finish_review = Some(FinishReviewState { editor: InputState::new() });
        app.diff_overlay = Some(state);
        set_active_view(&mut app, ActiveView::Diff);
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc));
        let after = app.diff_overlay.as_ref().expect("overlay stays open");
        assert!(after.finish_review.is_none(), "Esc dismisses the modal");
        assert_eq!(app.active_view, ActiveView::Diff, "still reviewing the diff");
    }
}
