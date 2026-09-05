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
/// sits above it while still bounding a desync. Nothing on the wire
/// distinguishes dead from slow, so expiry settles but never
/// convicts: a live envelope after expiry re-opens the session (see
/// `note_turn_started`).
///
/// Two wire premises here are UNVERIFIED against captures: that each
/// queued send starts its own turn (if the CLI batches N sends into
/// one turn, the surplus sends phantom-reopen, bounded by this
/// constant), and that no live turn (a stop-hook continuation, say)
/// starts inside the gap and steals the envelope consumption. Both
/// shapes are pinned by tests (`queued_turn_start_envelope_consumes_the_count`
/// and `interleaved_live_turn_consumes_the_count_boundary`); re-verify
/// against a live capture before trusting either.
const FORCE_SETTLE_AFTER: Duration = Duration::from_secs(90);

/// Consumed one queued send: the next live assistant envelope after
/// a re-open is that turn starting.
pub(crate) fn note_turn_started(app: &mut App, key: &SessionKey) {
    let force_settled = {
        let Some(bucket) = app.sessions.get_mut(key) else {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "queued_turn_started_dropped",
                message = "queued-turn start dropped for an unknown session",
                outcome = "dropped",
                session_key = %key.as_str(),
            );
            return;
        };
        if bucket.queued_turn_force_settled {
            bucket.queued_turn_force_settled = false;
            true
        } else if bucket.queued_turn_awaiting_start {
            bucket.queued_turn_awaiting_start = false;
            bucket.queued_turn_force_settle_at = None;
            bucket.queued_turn_sends = bucket.queued_turn_sends.saturating_sub(1);
            false
        } else {
            false
        }
    };
    if force_settled {
        // The expiry settle judged this session dead; a live envelope
        // proves the queued turn was only slow. No deadline this time
        // - the turn is already running.
        reopen_spinner(app, key);
    }
}

/// Record a typed submit dispatched while the session was busy: one
/// queued turn the settling paths must bridge.
pub(crate) fn note_submit_while_busy(app: &mut App) {
    if let Some(key) = app.active_session_key.clone() {
        note_queued_dispatch(app, &key);
    }
}

/// Record a workspace-originated dispatch (cron fire, peer or gotify
/// delivery, kick) that landed while the session's turn was in
/// flight: the same queued-send count a busy typed submit arms,
/// keyed instead of active-scoped.
pub(crate) fn note_queued_dispatch(app: &mut App, key: &SessionKey) {
    let Some(bucket) = app.sessions.get_mut(key) else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "queued_send_dropped",
            message = "queued send dropped for an unknown session",
            outcome = "dropped",
            session_key = %key.as_str(),
        );
        return;
    };
    // The workspace busy-check can lose the race to the in-flight
    // turn's own Result (same channel, FIFO); counting into a settled
    // bucket phantom-reopens for the prompt's fresh turn.
    if !matches!(bucket.lifecycle_state, crate::app::session::SessionLifecycleState::Running) {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "queued_send_dropped",
            message = "queued send dropped: the bucket has no live turn",
            outcome = "dropped",
            session_key = %key.as_str(),
        );
        return;
    }
    bucket.queued_turn_sends = bucket.queued_turn_sends.saturating_add(1);
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
    let Some(bucket) = app.sessions.get_mut(key) else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "queued_turn_cancel_dropped",
            message = "queued-send clear dropped for an unknown session",
            outcome = "dropped",
            session_key = %key.as_str(),
        );
        return;
    };
    bucket.queued_turn_sends = 0;
    bucket.queued_turn_awaiting_start = false;
    bucket.queued_turn_force_settle_at = None;
    bucket.queued_turn_force_settled = false;
}

/// Active-session turn-complete consult. `true` when a queued send
/// re-opened a Thinking turn instead of settling: placeholder, live
/// clock, and lifecycle all ride into the queued turn.
pub(crate) fn reopen_queued(app: &mut App, key: &SessionKey) -> bool {
    if app.sessions.get(key).is_none_or(|s| s.queued_turn_sends == 0) {
        return false;
    }
    // Already re-opened and still waiting: a repeated turn-complete
    // must not stack a second placeholder on top of the first.
    if app.sessions.get(key).is_some_and(|s| s.queued_turn_awaiting_start) {
        return true;
    }
    let Some(bucket) = app.sessions.get_mut(key) else {
        return false;
    };
    bucket.queued_turn_awaiting_start = true;
    bucket.queued_turn_force_settle_at = Some(SystemTime::now() + FORCE_SETTLE_AFTER);
    bucket.queued_turn_force_settled = false;
    reopen_spinner(app, key);
    true
}

/// Push the empty placeholder + live clock the chat spinner needs,
/// and flip the session back to Thinking. Shared by the settle-time
/// re-open and the post-expiry self-heal.
fn reopen_spinner(app: &mut App, key: &SessionKey) {
    // Pushed at the tail; if the user pivots away mid-gap, the next
    // submit's strip is its cleanup.
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
    bucket.queued_turn_force_settled = false;
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
    let dropped;
    let mut stripped;
    {
        let Some(bucket) = app.sessions.get_mut(key) else {
            return;
        };
        dropped = bucket.queued_turn_sends;
        bucket.queued_turn_sends = 0;
        bucket.queued_turn_awaiting_start = false;
        bucket.queued_turn_force_settle_at = None;
        // Not a verdict: a live envelope after expiry re-opens the
        // session through `note_turn_started`.
        bucket.queued_turn_force_settled = true;
        bucket.live_turn = crate::app::state::messages::LiveTurn::default();
        // The re-open's placeholder is empty at expiry: any content
        // would have arrived with an envelope, which clears the
        // deadline first. Only the active bucket gets the strip -
        // background buckets never reach a re-open, and a background
        // strip has no tracked-remove primitive to keep the bucket's
        // indices honest.
        stripped = false;
        if is_active {
            let empty_tail = app
                .messages()
                .iter()
                .rposition(|m| matches!(m.role, crate::app::MessageRole::Assistant))
                .and_then(|idx| app.messages().get(idx).map(|msg| (idx, msg.blocks.is_empty())));
            if let Some((idx, true)) = empty_tail {
                app.remove_message_tracked(idx);
                stripped = true;
            }
        }
    }
    super::set_bucket_lifecycle_state(app, key, crate::app::session::SessionLifecycleState::Idle);
    if is_active && matches!(app.status, AppStatus::Thinking | AppStatus::Running) {
        app.status = AppStatus::Ready;
    }
    tracing::warn!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "queued_turn_force_settled",
        message = "queued send never started a turn; force-settling the session",
        outcome = "timeout",
        session_key = %key.as_str(),
        dropped_sends = dropped,
        placeholder_stripped = stripped,
    );
}

fn clear_active(app: &mut App) {
    if let Some(bucket) = active_bucket_mut(app) {
        bucket.queued_turn_sends = 0;
        bucket.queued_turn_awaiting_start = false;
        bucket.queued_turn_force_settle_at = None;
        bucket.queued_turn_force_settled = false;
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
    use super::super::apply_session_update;
    use super::super::handle_runtime_session_state_update;
    use super::super::session::apply_session_update_connected;
    use super::super::turn::{
        apply_session_update_turn_cancelled, apply_session_update_turn_complete,
        handle_turn_error_event,
    };
    use super::*;
    use crate::agent::model;
    use crate::app::SystemSeverity;
    use crate::app::session::SessionLifecycleState;
    use forge_primitives::AgentCommand;
    use forge_primitives::{AssistantEnvelope, ContentBlock, Message};
    use forge_workspace::SessionUpdate;

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
        assert_eq!(
            app.messages().len(),
            3,
            "the geometry net must not stack a second placeholder behind the re-open's"
        );
    }

    /// An envelope inside the still-running turn (awaiting not set)
    /// consumes nothing: the count is only spent by the envelope that
    /// starts the queued turn after a re-open.
    #[test]
    fn envelope_during_the_live_turn_does_not_consume() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);

        super::super::sdk_message::handle_sdk_message(&mut app, assistant_envelope("msg_turn1"));

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 1, "turn 1 is still producing; nothing is spent");
        assert!(!bucket.queued_turn_awaiting_start);

        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(
            matches!(app.status, AppStatus::Thinking),
            "turn-complete still re-opens for the queued send"
        );
    }

    /// The force-settle's background arm: the glyph settles to Idle,
    /// the focused session's status is untouched, nothing is
    /// stripped, and no expiry notice is pushed.
    #[test]
    fn force_settle_background_bucket_sets_down_quietly() {
        use crate::app::session::UiSession;
        let mut app = App::test_default();
        let bg_key = SessionKey::from_str_for_test("background-session");
        let mut bg = UiSession::new(bg_key.clone());
        bg.queued_turn_sends = 1;
        bg.queued_turn_awaiting_start = true;
        bg.queued_turn_force_settle_at = Some(SystemTime::now() - Duration::from_secs(1));
        bg.messages
            .push(crate::app::ChatMessage::new(crate::app::MessageRole::Assistant, Vec::new()));
        app.sessions.insert(bg_key.clone(), bg);

        force_settle_expired(&mut app);

        let bg = app.sessions.get(&bg_key).expect("bg present");
        assert_eq!(bg.lifecycle_state, SessionLifecycleState::Idle);
        assert_eq!(bg.queued_turn_sends, 0);
        assert!(bg.queued_turn_force_settled);
        assert!(
            matches!(app.status, AppStatus::Ready),
            "the focused session's own status is untouched"
        );
        assert_eq!(
            bg.messages.len(),
            1,
            "the background bucket settles quietly; no expiry notice is pushed"
        );
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
        assert!(bucket.queued_turn_force_settled, "the tombstone is set for a late turn");
        let last = app.messages().last().expect("user bubble stays");
        assert!(
            !matches!(last.role, crate::app::MessageRole::Assistant) || !last.blocks.is_empty(),
            "the empty re-open placeholder is stripped"
        );
        let dropped_notice = app.messages().iter().any(|m| {
            matches!(m.role, crate::app::MessageRole::System(Some(SystemSeverity::Warning)))
                && m.blocks.iter().any(|b| match b {
                    crate::app::MessageBlock::Text(t) => t.text.contains("did not start"),
                    _ => false,
                })
        });
        assert!(!dropped_notice, "expiry settles quietly; no resend nag is pushed");
    }

    /// Expiry is not a verdict: when the "dead" turn's envelope
    /// finally arrives, the session re-opens so the late turn is not
    /// streamed blind.
    #[test]
    fn force_settle_expiry_self_heals_when_the_turn_arrives() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);
        apply_session_update_turn_complete(&mut app, &key, None);
        if let Some(bucket) = app.sessions.get_mut(&key) {
            bucket.queued_turn_force_settle_at = Some(SystemTime::now() - Duration::from_secs(1));
        }
        force_settle_expired(&mut app);
        assert!(matches!(app.status, AppStatus::Ready));

        super::super::sdk_message::handle_sdk_message(&mut app, assistant_envelope("msg_late"));

        assert!(
            matches!(app.status, AppStatus::Thinking | AppStatus::Running),
            "a live envelope after expiry re-opens the session, got {:?}",
            app.status,
        );
        let bucket = app.sessions.get(&key).expect("bucket present");
        assert!(!bucket.queued_turn_force_settled, "the tombstone is consumed");
        assert!(
            bucket.queued_turn_force_settle_at.is_none(),
            "the late turn is already running; no new deadline"
        );
        assert!(bucket.live_turn.started_at.is_some(), "the late turn gets a live clock");

        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(matches!(app.status, AppStatus::Ready), "the late turn settles normally");
    }

    /// Known boundary, unverified in captures: a live turn starting
    /// inside the gap (e.g. a stop-hook continuation) consumes the
    /// send, and its settle shows Ready while the real queued send is
    /// left unbridged. Recorded beside `FORCE_SETTLE_AFTER`.
    #[test]
    fn interleaved_live_turn_consumes_the_count_boundary() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);
        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(super::active_has_queued(&app));

        super::super::sdk_message::handle_sdk_message(&mut app, assistant_envelope("msg_thief"));
        apply_session_update_turn_complete(&mut app, &key, None);

        assert!(
            matches!(app.status, AppStatus::Ready),
            "the interleaved turn's settle is not suppressed - the queued send rides unbridged"
        );
        assert_eq!(app.sessions.get(&key).expect("bucket present").queued_turn_sends, 0);
    }

    /// A submit typed while a cancel is pending is fused into the
    /// interrupted turn, whose own Result covers it. It must not
    /// count as queued: that would phantom-reopen at the fused
    /// turn's Result every time.
    #[test]
    fn cancel_then_type_submit_does_not_count_as_queued() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        app.set_pending_cancel(true);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(
            bucket.queued_turn_sends, 0,
            "the cancel-fused submit is a fresh turn, not a queued send"
        );

        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(
            matches!(app.status, AppStatus::Ready),
            "the fused turn's Result settles without a phantom re-open"
        );
    }

    /// The production cancel-then-type ordering: the TurnCancelled
    /// presentation is pumped before the retype. The presentation's
    /// own clear plus the retype's fresh-turn shape must leave
    /// nothing queued for the fused turn's Result to trip over.
    #[test]
    fn cancel_presentation_pumped_before_retype_leaves_nothing_queued() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);
        assert_eq!(
            app.sessions.get(&key).expect("bucket present").queued_turn_sends,
            1,
            "fixture: one send armed before the cancel"
        );

        // Esc, then the update loop pumps the local TurnCancelled echo.
        app.set_pending_cancel(true);
        apply_session_update_turn_cancelled(&mut app, &key);
        assert!(
            app.pending_cancel(),
            "the echo pump keeps the flag set - the busy-gate's discriminator still holds at retype"
        );

        // The retype under a pending cancel, then the fused turn's
        // only Result.
        set_input(&mut app, "retype");
        crate::app::input_submit::submit_input(&mut app);
        apply_session_update_turn_complete(&mut app, &key, None);

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 0);
        assert!(
            matches!(app.status, AppStatus::Ready),
            "no phantom re-open across the pumped cancel and the fused Result"
        );
    }

    /// A reconnect or resume lands on the same bucket: queued-send
    /// state armed by the previous CLI must not survive the connect.
    #[test]
    fn connected_clears_queued_send_state() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        if let Some(bucket) = app.sessions.get_mut(&key) {
            bucket.queued_turn_sends = 1;
            bucket.queued_turn_awaiting_start = true;
            bucket.queued_turn_force_settle_at = Some(SystemTime::now() + Duration::from_secs(90));
            bucket.queued_turn_force_settled = true;
        }

        apply_session_update_connected(
            &mut app,
            &key,
            model::SessionId::new("session-1"),
            "/test".to_owned(),
            model::CurrentModel::new("test-model", "test-model", "test-model").authoritative(true),
            Vec::new(),
            None,
            &[],
            0,
        );

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 0, "stale sends must not survive the connect");
        assert!(!bucket.queued_turn_awaiting_start);
        assert!(bucket.queued_turn_force_settle_at.is_none());
        assert!(!bucket.queued_turn_force_settled);
    }

    /// A submit typed inside the re-opened gap (status is Thinking,
    /// so busy) must not disarm the force-settle backstop: the
    /// awaiting flag and deadline stay armed, and the next envelope
    /// still consumes exactly one send.
    #[test]
    fn gap_submit_keeps_the_desync_backstop_armed() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        set_input(&mut app, "second");
        crate::app::input_submit::submit_input(&mut app);
        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(matches!(app.status, AppStatus::Thinking));

        set_input(&mut app, "gap");
        crate::app::input_submit::submit_input(&mut app);

        {
            let bucket = app.sessions.get(&key).expect("bucket present");
            assert_eq!(bucket.queued_turn_sends, 2);
            assert!(
                bucket.queued_turn_awaiting_start,
                "the gap submit must not clear the awaiting flag"
            );
            assert!(
                bucket.queued_turn_force_settle_at.is_some(),
                "the gap submit must not disarm the force-settle deadline"
            );
        }

        super::super::sdk_message::handle_sdk_message(&mut app, assistant_envelope("msg_gap_turn"));

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(bucket.queued_turn_sends, 1, "the started turn consumes one send");
        assert!(!bucket.queued_turn_awaiting_start);
    }

    /// The workspace's own dispatches (cron, peer, gotify, kick) arrive
    /// as `PromptQueuedWhileBusy` and count into the keyed bucket -
    /// active or background, live turns both - while an unknown key
    /// must not mint one.
    #[test]
    fn prompt_queued_while_busy_counts_into_the_keyed_bucket() {
        use crate::app::session::UiSession;
        let mut app = App::test_default();
        let active_key = SessionKey::from_str_for_test("active-queued");
        let bg_key = SessionKey::from_str_for_test("background-queued");
        app.sessions.insert(active_key.clone(), UiSession::new(active_key.clone()));
        app.sessions.insert(bg_key.clone(), UiSession::new(bg_key.clone()));
        for key in [&active_key, &bg_key] {
            app.sessions.get_mut(key).expect("seeded").lifecycle_state =
                SessionLifecycleState::Running;
        }

        apply_session_update(
            &mut app,
            SessionUpdate::PromptQueuedWhileBusy { key: active_key.clone() },
        );
        apply_session_update(
            &mut app,
            SessionUpdate::PromptQueuedWhileBusy { key: bg_key.clone() },
        );

        assert_eq!(
            app.sessions.get(&active_key).expect("active present").queued_turn_sends,
            1,
            "the active bucket counts one workspace dispatch",
        );
        assert_eq!(
            app.sessions.get(&bg_key).expect("bg present").queued_turn_sends,
            1,
            "the background bucket counts one workspace dispatch",
        );

        let unknown = SessionKey::from_str_for_test("no-such-session");
        apply_session_update(&mut app, SessionUpdate::PromptQueuedWhileBusy { key: unknown });
        assert!(
            !app.sessions.contains_key(&SessionKey::from_str_for_test("no-such-session")),
            "an unknown key must not mint a bucket",
        );
    }

    /// The race the helper cannot rule out: the busy-check passes, then
    /// the in-flight turn's Result settles the bucket before the signal
    /// lands (same channel, FIFO). Counting then would phantom-reopen
    /// for the prompt's own fresh turn and misattribute the 90s expiry,
    /// so a settled (Idle) bucket drops the signal.
    #[test]
    fn prompt_queued_while_busy_after_the_settle_is_dropped() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);
        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(matches!(app.status, AppStatus::Ready), "fixture: the turn settled");
        assert_eq!(
            app.sessions.get(&key).expect("bucket").lifecycle_state,
            SessionLifecycleState::Idle,
            "fixture: the settle idled the bucket",
        );

        apply_session_update(&mut app, SessionUpdate::PromptQueuedWhileBusy { key: key.clone() });

        let bucket = app.sessions.get(&key).expect("bucket");
        assert_eq!(bucket.queued_turn_sends, 0, "a settled bucket drops the signal");
        assert_eq!(bucket.lifecycle_state, SessionLifecycleState::Idle, "no re-open");
        assert!(
            matches!(app.status, AppStatus::Ready),
            "the settle is not suppressed by a dropped signal",
        );
    }

    /// The full workspace-side ride: a cron echo plus its
    /// `PromptQueuedWhileBusy` arrive while a typed turn runs. The
    /// turn-complete re-opens Thinking, the envelope consumes one
    /// send, a second queued dispatch re-opens again, and the final
    /// settle is Ready once the count is exhausted.
    #[test]
    fn workspace_queued_dispatch_bridges_like_a_queued_send() {
        let mut app = app_with_connection();
        let key = active_session_key(&app);

        app.status = AppStatus::Ready;
        set_input(&mut app, "first");
        crate::app::input_submit::submit_input(&mut app);

        apply_session_update(
            &mut app,
            SessionUpdate::CronPromptAppended {
                session_id: key.as_str().to_owned(),
                text: "morning".to_owned(),
            },
        );
        apply_session_update(&mut app, SessionUpdate::PromptQueuedWhileBusy { key: key.clone() });

        let bucket = app.sessions.get(&key).expect("bucket present");
        assert_eq!(
            bucket.queued_turn_sends, 1,
            "the mid-turn workspace dispatch counts as one queued send",
        );

        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(
            matches!(app.status, AppStatus::Thinking),
            "turn-complete re-opens Thinking for the queued workspace dispatch",
        );

        super::super::sdk_message::handle_sdk_message(&mut app, assistant_envelope("msg_cron"));
        apply_session_update(&mut app, SessionUpdate::PromptQueuedWhileBusy { key: key.clone() });
        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(
            matches!(app.status, AppStatus::Thinking),
            "the second dispatch is still queued; re-open again",
        );

        super::super::sdk_message::handle_sdk_message(&mut app, assistant_envelope("msg_cron_2"));
        apply_session_update_turn_complete(&mut app, &key, None);
        assert!(
            matches!(app.status, AppStatus::Ready),
            "final settle is Ready once the count is exhausted",
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
