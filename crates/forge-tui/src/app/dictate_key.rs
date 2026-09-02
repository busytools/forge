//! Push-to-talk dictation: the configured dictate key's press/release
//! and the bare-modifier events that only exist because the same kitty
//! flag reports every key.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use std::time::{Duration, Instant};

use super::App;
use forge_workspace::{DictateBind, DictateMode};

/// A clean press-release shorter than this keeps recording (toggle);
/// longer is a hold whose release transcribes. No measured constant
/// backs the boundary, so it is the plain midpoint between a tap and a
/// deliberate hold, tunable from here.
const TOGGLE_TAP_WINDOW: Duration = Duration::from_millis(300);

/// What the dictate key's press/release sequence asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DictateAction {
    /// The key went down with nothing recording: ask for a capture.
    Begin,
    /// A clean release that ends the recording and transcribes: a hold
    /// released, or the tap that stops a toggled recording.
    Finish,
    /// A chorded release discarding the speculative recording the
    /// press started.
    Cancel,
}

/// Tracking for one dictate-key press, from its press to its release.
#[derive(Debug, Default)]
pub(crate) struct DictateKeyState {
    held: Option<Held>,
}

#[derive(Debug)]
struct Held {
    since: Instant,
    chorded: bool,
    /// Whether the recording that is running was started by THIS press.
    /// `false` means a toggle was already engaged and this press only
    /// arms its stopping release.
    started_recording: bool,
}

impl DictateKeyState {
    fn classify(
        &mut self,
        kind: KeyEventKind,
        recording_active: bool,
        now: Instant,
        mode: DictateMode,
    ) -> (bool, Option<DictateAction>) {
        match (kind, self.held.as_mut()) {
            (KeyEventKind::Press, None) => {
                // In toggle mode the press IS the stop: the take that is
                // live ends here, before any release is seen.
                if recording_active && mode == DictateMode::Toggle {
                    return (true, Some(DictateAction::Finish));
                }
                let started = !recording_active;
                self.held = Some(Held { since: now, chorded: false, started_recording: started });
                (true, (!recording_active).then_some(DictateAction::Begin))
            }
            // A second press or a repeat while the key is down carries
            // no new instruction; the press in flight is the truth.
            // A release with no tracked press is a stray.
            (KeyEventKind::Press, Some(_))
            | (KeyEventKind::Repeat, _)
            | (KeyEventKind::Release, None) => (true, None),
            (KeyEventKind::Release, Some(held)) => {
                let held = Held {
                    since: held.since,
                    chorded: held.chorded,
                    started_recording: held.started_recording,
                };
                self.held = None;
                // Toggle ignores releases as stops entirely: the press
                // that starts or stops has already acted.
                if mode == DictateMode::Toggle {
                    return (true, None);
                }
                if held.chorded {
                    return (true, held.started_recording.then_some(DictateAction::Cancel));
                }
                if held.started_recording {
                    let held_long_enough = match mode {
                        // A hold is a hold however brief.
                        DictateMode::Hold => true,
                        DictateMode::Auto | DictateMode::Toggle => {
                            now.duration_since(held.since) >= TOGGLE_TAP_WINDOW
                        }
                    };
                    return (true, held_long_enough.then_some(DictateAction::Finish));
                }
                (true, Some(DictateAction::Finish))
            }
        }
    }

    /// Any other key while the press is in flight marks it a chord, and
    /// the event still flows to normal dispatch.
    fn mark_chorded(&mut self) {
        if let Some(held) = &mut self.held {
            held.chorded = true;
        }
    }
}

/// The `KeyCode::Modifier` variant the configured binding delivers.
#[cfg(target_os = "macos")]
fn bound_modifier(bind: DictateBind) -> Option<crossterm::event::ModifierKeyCode> {
    use crossterm::event::ModifierKeyCode as M;
    match bind {
        DictateBind::RightCmd => Some(M::RightSuper),
        DictateBind::LeftCmd => Some(M::LeftSuper),
        DictateBind::Off => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn bound_modifier(bind: DictateBind) -> Option<crossterm::event::ModifierKeyCode> {
    use crossterm::event::ModifierKeyCode as M;
    match bind {
        DictateBind::RightCmd => Some(M::RightControl),
        DictateBind::LeftCmd => Some(M::LeftControl),
        DictateBind::Off => None,
    }
}

fn configured_bind(app: &App) -> DictateBind {
    app.workspace.as_ref().map(|w| w.dictate_bind()).unwrap_or_default()
}

fn configured_mode(app: &App) -> DictateMode {
    app.workspace.as_ref().map(|w| w.dictate_mode()).unwrap_or_default()
}

fn dictate_enabled(app: &App) -> bool {
    app.workspace.as_ref().is_some_and(|w| w.dictate_enabled())
}

/// Whether a take is live anywhere in the UI. The composer indicator
/// the `DictateStarted` reducer installed is the truth, and the
/// microphone it mirrors is process-wide, so any bucket's take counts.
/// A start dispatched but not yet echoed counts too, so a fast second
/// tap cannot dispatch a duplicate start.
fn recording_active(app: &App) -> bool {
    app.dictate_take_pending || app.sessions.values().any(|bucket| bucket.dictate.is_some())
}

/// Consume the dictate key's own events and every other bare modifier
/// key, returning `true` when the event was absorbed here. A bare
/// modifier is not text and not a shortcut; letting it through tears
/// down autocomplete and disturbs a queued paste burst. Any other key
/// while the dictate key is down marks the hold a chord but still
/// dispatches normally, so Right Cmd + V still pastes. `now` is the
/// event's arrival time, threaded in so the tap window is testable.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent, now: Instant) -> bool {
    let Some(bound) = bound_modifier(configured_bind(app)) else {
        return matches!(key.code, KeyCode::Modifier(_));
    };
    if !matches!(key.code, KeyCode::Modifier(m) if m == bound) {
        if matches!(key.code, KeyCode::Modifier(_)) {
            return true;
        }
        app.dictate_key.mark_chorded();
        return false;
    }
    // [dictate] off is the box doc's S0: the key is dead, not a refusal.
    // Without this gate the section's absence would turn every Cmd chord's
    // modifier half into an error notice in the composer.
    if !dictate_enabled(app) {
        return true;
    }
    let (consumed, action) =
        app.dictate_key.classify(key.kind, recording_active(app), now, configured_mode(app));
    match action {
        Some(DictateAction::Begin) => request_start(app),
        Some(DictateAction::Finish) => request_stop(app, false),
        Some(DictateAction::Cancel) => request_stop(app, true),
        None => {}
    }
    consumed
}

/// Ask the workspace for a capture. What happens - the device opening,
/// a refusal, the take running - is the `DictateStarted` /
/// `DictateEnded` updates' story, and the composer indicator reads
/// those. The take counts as live from here, not from the echo, so the
/// classification cannot race a fast second press.
fn request_start(app: &mut App) {
    app.dictate_take_pending = true;
    if let Err(error) = app.dispatch_command(|key| forge_workspace::Command::DictateStart { key }) {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "dictate_start_dispatch_failed",
            message = "could not dispatch the dictate start",
            outcome = "failure",
            error_message = %error,
        );
    }
}

fn request_stop(app: &mut App, cancelled: bool) {
    app.dictate_take_pending = false;
    // A live take may belong to a background session: the microphone is
    // process-wide, so the stop routes to the bucket holding the
    // indicator, falling back to the active session when nothing is
    // live yet (a release racing its start's echo).
    let target = app
        .sessions
        .iter()
        .find(|(_, bucket)| bucket.dictate.is_some())
        .map(|(key, _)| key.clone())
        .or_else(|| app.active_session_key.clone());
    let Some(key) = target else { return };
    let Some(workspace) = app.workspace.as_ref() else { return };
    if let Err(error) =
        workspace.dispatch(forge_workspace::Command::DictateStop { key, submit: !cancelled })
    {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "dictate_stop_dispatch_failed",
            message = "could not dispatch the dictate stop",
            outcome = "failure",
            error_message = %error,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::ModifierKeyCode as M;

    /// Pins each cfg arm's exact resolution: the end-to-end tests press
    /// whatever this mapping returns, so a flipped variant inside
    /// either arm would stay self-consistent and pass them.
    #[test]
    fn the_cmd_bindings_resolve_to_the_platforms_modifier_keys() {
        if cfg!(target_os = "macos") {
            assert_eq!(bound_modifier(DictateBind::RightCmd), Some(M::RightSuper));
            assert_eq!(bound_modifier(DictateBind::LeftCmd), Some(M::LeftSuper));
        } else {
            assert_eq!(bound_modifier(DictateBind::RightCmd), Some(M::RightControl));
            assert_eq!(bound_modifier(DictateBind::LeftCmd), Some(M::LeftControl));
        }
    }

    fn press(key: crossterm::event::ModifierKeyCode) -> KeyEvent {
        KeyEvent::new(KeyCode::Modifier(key), crossterm::event::KeyModifiers::NONE)
    }

    fn with_kind(key: crossterm::event::ModifierKeyCode, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Modifier(key), crossterm::event::KeyModifiers::NONE, kind)
    }

    fn base() -> Instant {
        Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now)
    }

    /// The modifier the default `right_cmd` binding resolves to on the
    /// platform running the test, read through the production mapping:
    /// the binding is Right Cmd on macOS and the right Control key
    /// elsewhere, so hardcoding either side breaks the end-to-end
    /// tests on the other.
    fn right_cmd_key() -> crossterm::event::ModifierKeyCode {
        bound_modifier(DictateBind::RightCmd).expect("right_cmd binds to a key")
    }

    /// The `left_cmd` equivalent, which the tests use as an UNBOUND
    /// modifier against the default right-side binding.
    fn left_cmd_key() -> crossterm::event::ModifierKeyCode {
        bound_modifier(DictateBind::LeftCmd).expect("left_cmd binds to a key")
    }

    /// The existing classification tests exercise the default `auto`
    /// mode; forced-mode tests call `classify` directly with a mode.
    fn classify(
        state: &mut DictateKeyState,
        kind: KeyEventKind,
        recording_active: bool,
        now: Instant,
    ) -> (bool, Option<DictateAction>) {
        state.classify(kind, recording_active, now, DictateMode::Auto)
    }

    /// An app whose workspace stub has `[dictate] enabled` and the
    /// dispatch intercept armed: the setup every dispatch-asserting
    /// test shares.
    fn app_with_dictate_enabled() -> (App, std::sync::Arc<forge_workspace::Workspace>) {
        let mut app = App::test_default();
        let (workspace, _updates) = forge_workspace::Workspace::testing_stub_with_dictate_enabled();
        workspace.enable_test_dispatch_intercept();
        app.workspace = Some(workspace.clone());
        (app, workspace)
    }

    #[test]
    fn a_press_without_a_recording_asks_for_one() {
        let mut state = DictateKeyState::default();
        let (consumed, action) = classify(&mut state, KeyEventKind::Press, false, base());
        assert!(consumed);
        assert_eq!(action, Some(DictateAction::Begin));
    }

    #[test]
    fn a_clean_hold_release_transcribes_and_a_clean_tap_toggles() {
        let mut state = DictateKeyState::default();
        let start = base();
        let (consumed, action) = classify(&mut state, KeyEventKind::Press, false, start);
        assert_eq!((consumed, action), (true, Some(DictateAction::Begin)));

        // Held past the tap window: the release is the whole utterance.
        let late = start + Duration::from_millis(400);
        let (consumed, action) = classify(&mut state, KeyEventKind::Release, true, late);
        assert_eq!((consumed, action), (true, Some(DictateAction::Finish)));

        // A quick tap engages the toggle: recording continues, and the
        // next clean release - however long - ends it.
        let start = base();
        let (consumed, action) = classify(&mut state, KeyEventKind::Press, false, start);
        assert_eq!((consumed, action), (true, Some(DictateAction::Begin)));
        let soon = start + Duration::from_millis(200);
        let (consumed, action) = classify(&mut state, KeyEventKind::Release, true, soon);
        assert_eq!((consumed, action), (true, None), "a clean tap keeps recording");

        let (consumed, action) = classify(&mut state, KeyEventKind::Release, true, soon);
        assert_eq!((consumed, action), (true, None), "a stray release does nothing");

        let press = base();
        let (consumed, action) = classify(&mut state, KeyEventKind::Press, true, press);
        assert_eq!((consumed, action), (true, None), "a press while toggled asks for nothing");

        let release = press + Duration::from_millis(50);
        let (consumed, action) = classify(&mut state, KeyEventKind::Release, true, release);
        assert_eq!((consumed, action), (true, Some(DictateAction::Finish)));
    }

    #[test]
    fn a_chorded_release_cancels_only_the_recording_its_own_press_started() {
        let mut state = DictateKeyState::default();
        let start = base();
        let (_, action) = classify(&mut state, KeyEventKind::Press, false, start);
        assert_eq!(action, Some(DictateAction::Begin));

        state.mark_chorded();
        let (consumed, action) = classify(&mut state, KeyEventKind::Release, true, start);
        assert_eq!((consumed, action), (true, Some(DictateAction::Cancel)));

        // The same chord while a toggle already runs must not end it:
        // the release belongs to a shortcut, not to dictation.
        let press = base();
        let (consumed, action) = classify(&mut state, KeyEventKind::Press, true, press);
        assert_eq!((consumed, action), (true, None));
        state.mark_chorded();
        let (_, action) = classify(&mut state, KeyEventKind::Release, true, press);
        assert_eq!(action, None);
    }

    #[test]
    fn a_repeat_of_the_held_key_is_a_no_op() {
        let mut state = DictateKeyState::default();
        let start = base();
        let _ = classify(&mut state, KeyEventKind::Press, false, start);
        let (consumed, action) = classify(&mut state, KeyEventKind::Repeat, true, start);
        assert_eq!((consumed, action), (true, None));
        let (_, action) =
            classify(&mut state, KeyEventKind::Release, true, start + Duration::from_millis(400));
        assert_eq!(action, Some(DictateAction::Finish), "the hold still ends on release");
    }

    #[test]
    fn toggle_mode_stops_on_the_press_and_ignores_releases() {
        let mut state = DictateKeyState::default();
        let start = base();
        let (consumed, action) =
            state.classify(KeyEventKind::Press, false, start, DictateMode::Toggle);
        assert_eq!((consumed, action), (true, Some(DictateAction::Begin)));

        // The first release is ignored as a stop but still ends the
        // press tracking; the stopping press acts immediately, and its
        // own release does nothing.
        let (consumed, action) = state.classify(
            KeyEventKind::Release,
            true,
            start + Duration::from_millis(60),
            DictateMode::Toggle,
        );
        assert_eq!((consumed, action), (true, None), "toggle ignores releases as stops");
        let (consumed, action) = state.classify(
            KeyEventKind::Press,
            true,
            start + Duration::from_millis(500),
            DictateMode::Toggle,
        );
        assert_eq!((consumed, action), (true, Some(DictateAction::Finish)));
        let (consumed, action) = state.classify(
            KeyEventKind::Release,
            true,
            start + Duration::from_millis(520),
            DictateMode::Toggle,
        );
        assert_eq!((consumed, action), (true, None), "toggle ignores releases as stops");

        // Starting again, then stopping once more.
        let (consumed, action) = state.classify(
            KeyEventKind::Press,
            false,
            start + Duration::from_millis(600),
            DictateMode::Toggle,
        );
        assert_eq!((consumed, action), (true, Some(DictateAction::Begin)));
        let _ = state.classify(
            KeyEventKind::Release,
            true,
            start + Duration::from_millis(620),
            DictateMode::Toggle,
        );
        let (consumed, action) = state.classify(
            KeyEventKind::Press,
            true,
            start + Duration::from_millis(700),
            DictateMode::Toggle,
        );
        assert_eq!((consumed, action), (true, Some(DictateAction::Finish)));
    }

    #[test]
    fn hold_mode_finishes_on_every_clean_release_however_brief() {
        let mut state = DictateKeyState::default();
        let start = base();
        let (consumed, action) =
            state.classify(KeyEventKind::Press, false, start, DictateMode::Hold);
        assert_eq!((consumed, action), (true, Some(DictateAction::Begin)));

        // A quick tap is a hold that got released: it submits.
        let (consumed, action) = state.classify(
            KeyEventKind::Release,
            true,
            start + Duration::from_millis(50),
            DictateMode::Hold,
        );
        assert_eq!((consumed, action), (true, Some(DictateAction::Finish)));

        // A second press starts nothing while a take is live; its
        // release still finishes.
        let (consumed, action) = state.classify(
            KeyEventKind::Press,
            true,
            start + Duration::from_millis(100),
            DictateMode::Hold,
        );
        assert_eq!((consumed, action), (true, None));
        state.mark_chorded();
        let (consumed, action) = state.classify(
            KeyEventKind::Release,
            true,
            start + Duration::from_millis(150),
            DictateMode::Hold,
        );
        assert_eq!((consumed, action), (true, None), "a chorded release is not a dictate release");
    }

    #[test]
    fn other_bare_modifiers_are_consumed_and_the_bound_key_is_tracked() {
        let (mut app, workspace) = app_with_dictate_enabled();
        let now = Instant::now();
        // No workspace-visible override in a test app resolves the
        // default binding, so Right Cmd is the dictate key and every
        // other bare modifier is absorbed here.
        assert!(
            handle_key(&mut app, press(left_cmd_key()), now),
            "an unbound modifier is consumed"
        );
        assert!(
            handle_key(&mut app, press(right_cmd_key()), now),
            "the bound key press is consumed"
        );
        assert!(
            !handle_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('v'), crossterm::event::KeyModifiers::NONE),
                now
            ),
            "a plain key still dispatches"
        );
        assert!(handle_key(&mut app, with_kind(right_cmd_key(), KeyEventKind::Release), now));
        assert_eq!(
            workspace.drain_test_dispatch_buffer().len(),
            2,
            "the tracked press+release dispatched start and abandon"
        );
    }

    #[test]
    fn the_end_to_end_hold_dispatches_start_then_submit() {
        let (mut app, workspace) = app_with_dictate_enabled();
        let start = base();
        assert!(handle_key(&mut app, press(right_cmd_key()), start));

        let release = KeyEvent::new_with_kind(
            KeyCode::Modifier(right_cmd_key()),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(handle_key(&mut app, release, start + Duration::from_millis(400)));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 2, "one start, one stop: {dispatched:?}");
        assert!(matches!(&dispatched[0], forge_workspace::Command::DictateStart { .. }));
        match &dispatched[1] {
            forge_workspace::Command::DictateStop { submit, .. } => {
                assert!(submit, "a held release submits the take");
            }
            other => panic!("a stop, got {other:?}"),
        }
    }

    #[test]
    fn a_chorded_end_to_end_press_dispatches_an_abandon() {
        let (mut app, workspace) = app_with_dictate_enabled();
        let now = Instant::now();
        assert!(handle_key(&mut app, press(right_cmd_key()), now));
        // A plain key arrives while the dictate key is down: it marks
        // the chord AND still dispatches.
        assert!(!handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('v'), crossterm::event::KeyModifiers::NONE),
            now
        ));
        let release = KeyEvent::new_with_kind(
            KeyCode::Modifier(right_cmd_key()),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(handle_key(&mut app, release, now));

        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 2, "start then abandon: {dispatched:?}");
        match &dispatched[1] {
            forge_workspace::Command::DictateStop { submit, .. } => {
                assert!(!submit, "a chorded release discards the speculative take");
            }
            other => panic!("a stop, got {other:?}"),
        }
    }

    #[test]
    fn a_clean_tap_dispatches_no_stop_until_the_next_release() {
        let (mut app, workspace) = app_with_dictate_enabled();
        let start = base();
        assert!(handle_key(&mut app, press(right_cmd_key()), start));
        let tap = KeyEvent::new_with_kind(
            KeyCode::Modifier(right_cmd_key()),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(handle_key(&mut app, tap, start + Duration::from_millis(200)));
        assert_eq!(
            workspace.drain_test_dispatch_buffer().len(),
            1,
            "a clean tap engages the toggle: only the start went out"
        );

        // The next press+release ends it.
        assert!(handle_key(&mut app, press(right_cmd_key()), start + Duration::from_millis(220)));
        let tap = KeyEvent::new_with_kind(
            KeyCode::Modifier(right_cmd_key()),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(handle_key(&mut app, tap, start + Duration::from_millis(250)));
        let dispatched = workspace.drain_test_dispatch_buffer();
        match &dispatched[0] {
            forge_workspace::Command::DictateStop { submit, .. } => {
                assert!(submit, "the toggle's stopping tap submits");
            }
            other => panic!("a stop, got {other:?}"),
        }
    }

    #[test]
    fn a_live_take_anywhere_means_a_press_starts_nothing() {
        let (mut app, workspace) = app_with_dictate_enabled();
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate =
            Some(crate::app::dictate::DictateIndicator::recording(-50.0, 1));

        let start = base();
        assert!(handle_key(&mut app, press(right_cmd_key()), start));
        assert!(
            workspace.drain_test_dispatch_buffer().is_empty(),
            "a press while a take is live asks for nothing"
        );

        let release = KeyEvent::new_with_kind(
            KeyCode::Modifier(right_cmd_key()),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(handle_key(&mut app, release, start + Duration::from_millis(400)));
        assert_eq!(workspace.drain_test_dispatch_buffer().len(), 1, "the release ends it");
    }

    #[test]
    fn a_press_with_dictation_disabled_is_dead_not_an_error() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        // The testing stub's config is the default: `[dictate]` absent,
        // which is disabled. The press stays consumed (a bare modifier)
        // but dispatches nothing and records nothing.
        let now = Instant::now();
        assert!(handle_key(&mut app, press(right_cmd_key()), now));
        assert!(!handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('v'), crossterm::event::KeyModifiers::NONE),
            now
        ));
        assert!(handle_key(&mut app, with_kind(right_cmd_key(), KeyEventKind::Release), now));
        assert!(
            workspace.drain_test_dispatch_buffer().is_empty(),
            "a disabled dictation never dispatches"
        );
        assert!(!recording_active(&app));
    }

    #[test]
    fn the_stop_routes_to_the_session_holding_the_live_take() {
        let (mut app, workspace) = app_with_dictate_enabled();
        let active = app.active_session_key.clone().expect("active session");
        let background = forge_workspace::SessionKey::from_session_id("background-take");
        app.sessions
            .insert(background.clone(), crate::app::session::UiSession::new(background.clone()));
        app.sessions.get_mut(&background).expect("bucket").dictate =
            Some(crate::app::dictate::DictateIndicator::recording(-50.0, 1));

        let start = base();
        assert!(handle_key(&mut app, press(right_cmd_key()), start));
        let release = KeyEvent::new_with_kind(
            KeyCode::Modifier(right_cmd_key()),
            crossterm::event::KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(handle_key(&mut app, release, start + Duration::from_millis(400)));

        let dispatched = workspace.drain_test_dispatch_buffer();
        match dispatched.last() {
            Some(forge_workspace::Command::DictateStop { key, submit }) => {
                assert_eq!(key, &background, "the stop names the take's own session");
                assert!(submit);
            }
            other => panic!("a stop for the live take, got {other:?}"),
        }
        assert_eq!(active, app.active_session_key.clone().expect("active session"));
    }
}
