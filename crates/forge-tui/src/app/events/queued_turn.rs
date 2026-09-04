//! Bridging the gap between a mid-turn submit and its queued turn.
//!
//! The CLI emits nothing on the wire for a queued turn until its
//! first token, so both idle-settling paths (`apply_turn_complete_presentation`
//! and the runtime `Idle` state event) consult
//! `UiSession.queued_turn_sends` and keep the spinner open instead of
//! settling to Ready/Idle. A re-open re-arms on the assistant
//! envelope that starts the queued turn; a force-settle sweep bounds
//! a desync.

use crate::app::{App, AppStatus};
use forge_workspace::SessionKey;
use std::time::{Duration, SystemTime};

/// How long a re-opened queued turn may sit with no assistant
/// envelope before the force-settle sweep closes it. The observed
/// pre-first-token wait under API queueing reaches ~75s, so this
/// sits above it while still bounding a desync.
const FORCE_SETTLE_AFTER: Duration = Duration::from_secs(90);

/// Consumed one queued send: the next live assistant envelope after
/// a re-open is that turn starting.
pub(crate) fn note_turn_started(app: &mut App, key: &SessionKey) {
    let Some(bucket) = app.sessions.get_mut(key) else {
        return;
    };
    if !bucket.queued_turn_awaiting_start {
        return;
    }
    bucket.queued_turn_awaiting_start = false;
    bucket.queued_turn_force_settle_at = None;
    bucket.queued_turn_sends = bucket.queued_turn_sends.saturating_sub(1);
}

/// Record a typed submit dispatched while the session was busy: one
/// queued turn the settling paths must bridge.
pub(crate) fn note_submit_while_busy(app: &mut App) {
    let Some(bucket) = active_bucket_mut(app) else {
        return;
    };
    bucket.queued_turn_sends = bucket.queued_turn_sends.saturating_add(1);
    bucket.queued_turn_awaiting_start = false;
    bucket.queued_turn_force_settle_at = None;
}

/// A fresh (idle-path) submit means any earlier queued-send bookkeeping
/// is moot - the user's new prompt is the turn.
pub(crate) fn note_idle_submit(app: &mut App) {
    clear_active(app);
}

/// Drop the queued-send state on a path that ends the turn without a
/// Result for the queued turn (cancel, error). A stale count would
/// re-open Thinking at the next settle for a turn that is not coming.
pub(crate) fn cancel(app: &mut App, key: &SessionKey) {
    if let Some(bucket) = app.sessions.get_mut(key) {
        bucket.queued_turn_sends = 0;
        bucket.queued_turn_awaiting_start = false;
        bucket.queued_turn_force_settle_at = None;
    }
}

/// Active-session turn-complete consult. `true` when a queued send
/// re-opened a Thinking turn instead of settling: placeholder, live
/// clock, and lifecycle all ride into the queued turn.
pub(crate) fn reopen_queued(app: &mut App, key: &SessionKey) -> bool {
    if app.sessions.get(key).is_none_or(|s| s.queued_turn_sends == 0) {
        return false;
    }
    let Some(bucket) = app.sessions.get_mut(key) else {
        return false;
    };
    bucket.queued_turn_awaiting_start = true;
    bucket.queued_turn_force_settle_at = Some(SystemTime::now() + FORCE_SETTLE_AFTER);
    app.push_message_tracked(crate::app::ChatMessage::new(
        crate::app::MessageRole::Assistant,
        Vec::new(),
    ));
    app.bind_active_turn_assistant_to_tail();
    app.enforce_history_retention_tracked();
    app.start_live_turn(std::time::Instant::now());
    app.status = AppStatus::Thinking;
    super::set_bucket_lifecycle_state(
        app,
        key,
        crate::app::session::SessionLifecycleState::Running,
    );
    tracing::debug!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "queued_turn_reopened",
        message = "turn complete suppressed for a queued send; spinner rides into the queued turn",
        outcome = "success",
        session_key = %key.as_str(),
    );
    true
}

/// Background-bucket turn-complete consult. No chat spinner to feed,
/// so this only holds the pane glyph at Running and arms the
/// force-settle.
pub(crate) fn hold_background_open(app: &mut App, key: &SessionKey) -> bool {
    if app.sessions.get(key).is_none_or(|s| s.queued_turn_sends == 0) {
        if let Some(bucket) = app.sessions.get_mut(key) {
            bucket.queued_turn_awaiting_start = false;
            bucket.queued_turn_force_settle_at = None;
        }
        return false;
    }
    let Some(bucket) = app.sessions.get_mut(key) else {
        return false;
    };
    bucket.queued_turn_awaiting_start = true;
    bucket.queued_turn_force_settle_at = Some(SystemTime::now() + FORCE_SETTLE_AFTER);
    super::set_bucket_lifecycle_state(
        app,
        key,
        crate::app::session::SessionLifecycleState::Running,
    );
    true
}

/// True while the active session still has queued sends the settling
/// paths must not settle past.
pub(crate) fn active_has_queued(app: &App) -> bool {
    active_bucket(app).is_some_and(|s| s.queued_turn_sends > 0)
}

/// Main-loop tick: force-settle any re-opened queued turn whose
/// force-settle deadline passed without a turn starting. Without
/// this, a desync (a queued send the CLI never ran) strands the
/// spinner forever.
pub(crate) fn force_settle_expired(app: &mut App) {
    let now = SystemTime::now();
    let due: Vec<SessionKey> = app
        .sessions
        .iter()
        .filter(|(_, session)| session.queued_turn_force_settle_at.is_some_and(|at| now >= at))
        .map(|(key, _)| key.clone())
        .collect();
    for key in due {
        force_settle(app, &key);
    }
}

fn force_settle(app: &mut App, key: &SessionKey) {
    let is_active = app.active_session_key.as_ref() == Some(key);
    let Some(bucket) = app.sessions.get_mut(key) else {
        return;
    };
    bucket.queued_turn_sends = 0;
    bucket.queued_turn_awaiting_start = false;
    bucket.queued_turn_force_settle_at = None;
    bucket.live_turn = crate::app::state::messages::LiveTurn::default();
    // The re-open's placeholder is empty at expiry: any content would
    // have arrived with an envelope, which clears the deadline first.
    let empty_tail = bucket
        .messages
        .iter()
        .rposition(|m| matches!(m.role, crate::app::MessageRole::Assistant))
        .and_then(|idx| bucket.messages.get(idx).map(|msg| (idx, msg.blocks.is_empty())));
    if let Some((idx, true)) = empty_tail {
        bucket.messages.remove(idx);
        bucket.message_retained_bytes.remove(idx);
    }
    super::set_bucket_lifecycle_state(app, key, crate::app::session::SessionLifecycleState::Idle);
    if is_active && matches!(app.status, AppStatus::Thinking | AppStatus::Running) {
        app.status = AppStatus::Ready;
    }
    tracing::warn!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "queued_turn_force_settled",
        message = "queued send never started a turn; force-settling the session",
        outcome = "recovered",
        session_key = %key.as_str(),
    );
}

fn clear_active(app: &mut App) {
    if let Some(bucket) = active_bucket_mut(app) {
        bucket.queued_turn_sends = 0;
        bucket.queued_turn_awaiting_start = false;
        bucket.queued_turn_force_settle_at = None;
    }
}

fn active_bucket(app: &App) -> Option<&crate::app::session::UiSession> {
    let key = app.active_session_key.as_ref()?;
    app.sessions.get(key)
}

fn active_bucket_mut(app: &mut App) -> Option<&mut crate::app::session::UiSession> {
    let key = app.active_session_key.clone()?;
    app.sessions.get_mut(&key)
}

#[cfg(test)]
mod tests {
    use super::super::handle_runtime_session_state_update;
    use super::super::turn::{
        apply_session_update_turn_cancelled, apply_session_update_turn_complete,
        handle_turn_error_event,
    };
    use super::*;
    use crate::agent::model;
    use crate::app::session::SessionLifecycleState;
    use forge_primitives::AgentCommand;
    use forge_primitives::{AssistantEnvelope, ContentBlock, Message};

    fn app_with_connection() -> App {
        let mut app = App::test_default();
        let _rx: tokio::sync::mpsc::UnboundedReceiver<AgentCommand> = app.install_testing_stub();
        app.set_session_id(Some(model::SessionId::new("session-1")));
        app
    }

    fn active_session_key(app: &App) -> SessionKey {
        app.active_session_key.clone().expect("active session key seeded by App::test_default")
    }

    fn set_input(app: &mut App, text: &str) {
        app.input_mut().set_text(text);
    }

    fn assistant_envelope(id: &str) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: id.to_owned(),
                role: "assistant".to_owned(),
                model: "claude-test".to_owned(),
                content: vec![ContentBlock::Text { text: "queued turn output".to_owned() }],
                stop_reason: None,
                stop_sequence: None,
                usage: None,
            },
            session_id: String::new(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
        }
    }

    /// Send a follow-up while turn 1 is still running, then let
    /// turn 1 complete. The queued turn must re-open a Thinking turn
    /// instead of settling to Ready/Idle.
    #[test]
    fn mid_turn_submit_then_turn_complete_reopens_thinking() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        assert!(matches!(app.status, AppStatus::Thinking));

        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 1, "mid-turn submit counts one queued send");

        apply_session_update_turn_complete(&mut app, &key, None);

        assert!(
            matches!(app.status, AppStatus::Thinking),
            "queued turn must re-open Thinking, got {:?}",
            app.status
        );
        let bucket = app.sessions.get(&key).expect("bucket present");
        assert!(
            bucket.live_turn.started_at.is_some(),
            "re-open must start the live clock so the row paints"
        );
        assert!(bucket.queued_turn_awaiting_start);
        assert!(bucket.queued_turn_force_settle_at.is_some());
        assert_eq!(
            bucket.queued_turn_sends, 1,
            "the count is consumed by the turn start, not the settle"
        );
        assert_eq!(
            bucket.lifecycle_state,
            SessionLifecycleState::Running,
            "pane glyph keeps spinning for the queued turn",
        );
        let last = app.messages().last().expect("placeholder pushed");
        assert!(matches!(last.role, crate::app::MessageRole::Assistant) && last.blocks.is_empty());
    }

    /// The second settling path: the wire `Idle` runtime event must
    /// not force Ready while a queued send is pending.
    #[test]
    fn runtime_idle_does_not_flip_ready_while_queued() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);
        apply_session_update_turn_complete(&mut app, &key, None);

        handle_runtime_session_state_update(&mut app, model::RuntimeSessionState::Idle);

        assert!(
            matches!(app.status, AppStatus::Thinking),
            "runtime Idle must not settle a session with a queued send, got {:?}",
            app.status,
        );
    }

    /// The queued count is consumed one send at a time by each queued
    /// turn starting (the first live assistant envelope), so a second
    /// queued message still re-opens after the first queued turn
    /// wraps; once the count reaches zero, a settle settles.
    #[test]
    fn queued_turn_start_envelope_consumes_the_count() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "third");
        crate::app::input_submit::submit_input(&mut app);
        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(super::active_has_queued(&app));

        super::super::sdk_message::handle_sdk_message(&mut app, assistant_envelope("msg_queued_1"));

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(
            bucket.queued_turn_sends, 1,
            "one turn started; exactly one send is consumed, not all"
        );
        assert!(!bucket.queued_turn_awaiting_start);
        assert!(
            bucket.queued_turn_force_settle_at.is_none(),
            "a started turn no longer needs the force-settle"
        );

        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(
            matches!(app.status, AppStatus::Thinking),
            "the second queued message must re-open after the first queued turn wraps"
        );

        super::super::sdk_message::handle_sdk_message(&mut app, assistant_envelope("msg_queued_2"));

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 0, "the last queued turn started");

        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(
            matches!(app.status, AppStatus::Ready),
            "with the count consumed, the settle must settle"
        );
    }

    /// A desynced re-open (the CLI never ran the queued send) must
    /// not strand the spinner: the sweep force-settles it.
    #[test]
    fn force_settle_expires_a_stranded_queued_turn() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);
        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(matches!(app.status, AppStatus::Thinking));

        if let Some(bucket) = app.sessions.get_mut(&key) {
            bucket.queued_turn_force_settle_at = Some(SystemTime::now() - Duration::from_secs(1));
        }
        force_settle_expired(&mut app);

        assert!(matches!(app.status, AppStatus::Ready));
        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 0);
        assert_eq!(bucket.lifecycle_state, SessionLifecycleState::Idle);
        assert!(bucket.live_turn.started_at.is_none());
        let last = app.messages().last().expect("user bubble stays");
        assert!(
            !matches!(last.role, crate::app::MessageRole::Assistant) || !last.blocks.is_empty(),
            "the empty re-open placeholder is stripped"
        );
    }

    /// An error ending the turn clears the queued-send state, so the
    /// error cannot leak a stale count into the next settle.
    #[test]
    fn turn_error_clears_the_queued_count() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);

        handle_turn_error_event(&mut app, &key, "boom", None, None);

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 0);
        assert!(!bucket.queued_turn_awaiting_start);
        assert!(bucket.queued_turn_force_settle_at.is_none());
    }

    /// The Esc-path cancel presentation clears the queued-send state
    /// too: a stale count would re-open Thinking at the next settle
    /// for a turn the interrupt dropped.
    #[test]
    fn turn_cancelled_clears_the_queued_count() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);

        apply_session_update_turn_cancelled(&mut app, &key);

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(
            bucket.queued_turn_sends, 0,
            "a stale count would re-open Thinking at the next settle"
        );
        assert!(!bucket.queued_turn_awaiting_start);
        assert!(bucket.queued_turn_force_settle_at.is_none());
    }

    /// The background arm of turn-complete also holds a queued send
    /// open instead of dropping the pane glyph to Idle for the gap.
    #[test]
    fn background_turn_complete_holds_open_for_a_queued_send() {
        use crate::app::session::UiSession;
        let mut app = App::test_default();
        let bg_key = SessionKey::from_str_for_test("background-session");
        let mut bg = UiSession::new(bg_key.clone());
        bg.queued_turn_sends = 1;
        app.sessions.insert(bg_key.clone(), bg);

        apply_session_update_turn_complete(&mut app, &bg_key, None);

        let bg = app.sessions.get(&bg_key).expect("bg present");
        assert_eq!(
            bg.lifecycle_state,
            SessionLifecycleState::Running,
            "the background bucket keeps its spinner glyph for the queued turn"
        );
        assert!(bg.queued_turn_awaiting_start);
        assert!(bg.queued_turn_force_settle_at.is_some());
        assert!(
            matches!(app.status, AppStatus::Ready),
            "the focused session's own status is untouched"
        );
    }

    /// An idle-path submit clears any stale queued-send state and does
    /// not count as queued itself, so a normal turn still settles.
    #[test]
    fn idle_submit_does_not_block_settling() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        if let Some(bucket) = app.sessions.get_mut(&key) {
            bucket.queued_turn_sends = 1;
        }

        app.status = AppStatus::Ready;
        set_input(&mut app, "only");
        crate::app::input_submit::submit_input(&mut app);

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 0, "the idle submit clears stale queued-send state");

        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(matches!(app.status, AppStatus::Ready));
    }
}
