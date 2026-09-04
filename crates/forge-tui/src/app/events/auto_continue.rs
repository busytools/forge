//! Bounded auto-continuation after a transient server error.
//!
//! A turn that dies on a 5xx the CLI already retried and gave up on is
//! recoverable: the conversation and every completed tool result are
//! still in history, so one more user turn asking the model to resume
//! gets the work moving again. Forge sends that turn itself instead of
//! leaving the session dead, up to [`MAX_ATTEMPTS`] times with growing
//! spacing, and says so in the chat each time.
//!
//! `ServerError` only. A `RateLimit` needs its window to reset, which
//! [`super::rate_limit::maybe_recover_from_rate_limit_lock`] owns;
//! auth / billing / invalid-request / max-output-tokens are not
//! transient at all, so continuing them only burns the budget.

use crate::app::{App, AppStatus, SystemSeverity};
use forge_primitives::ApiRetryError;
use forge_workspace::SessionKey;
use std::time::{Duration, SystemTime};

/// How many continuations forge sends before handing the session to
/// the user via the NEEDS ATTENTION band.
pub(crate) const MAX_ATTEMPTS: u32 = 3;

/// Spacing before each attempt. The CLI's own retries are seconds
/// apart and already failed, so forge waits longer - an overloaded API
/// needs room, while a brief blip still recovers on the first entry.
const BACKOFF: [Duration; MAX_ATTEMPTS as usize] =
    [Duration::from_secs(5), Duration::from_secs(20), Duration::from_secs(60)];

/// Arm a continuation for `key` when its turn died on a transient
/// server error and the budget has room. `true` means armed, and the
/// caller skips recording a `failed_turn` - the session is recovering,
/// not waiting on the user. `false` for every other classification and
/// once the budget is spent, so the failure reaches the band.
pub(crate) fn arm_if_transient(
    app: &mut App,
    key: &SessionKey,
    error: ApiRetryError,
    now: SystemTime,
) -> bool {
    if !matches!(error, ApiRetryError::ServerError) {
        return false;
    }
    let Some(bucket) = app.sessions.get_mut(key) else { return false };
    let Some(delay) = BACKOFF.get(bucket.auto_continue_attempts as usize) else {
        return false;
    };
    bucket.auto_continue_due_at = Some(now + *delay);
    true
}

/// Fire any continuation whose backoff has elapsed. Called once per
/// main-loop tick alongside
/// [`super::rate_limit::maybe_recover_from_rate_limit_lock`].
pub(crate) fn maybe_fire(app: &mut App) {
    let now = SystemTime::now();
    let due: Vec<SessionKey> = app
        .sessions
        .iter()
        .filter(|(_, session)| session.auto_continue_due_at.is_some_and(|at| now >= at))
        .map(|(key, _)| key.clone())
        .collect();
    for key in due {
        fire(app, &key);
    }
}

/// Send one continuation turn into `key`. Disarms first, so a burst of
/// ticks can only ever produce one dispatch per armed timer.
fn fire(app: &mut App, key: &SessionKey) {
    let Some(bucket) = app.sessions.get_mut(key) else { return };
    bucket.auto_continue_due_at = None;
    bucket.auto_continue_attempts = bucket.auto_continue_attempts.saturating_add(1);
    let attempt = bucket.auto_continue_attempts;
    let status = bucket.last_api_retry.and_then(|(_, status)| status);

    let Some(workspace) = app.workspace.as_ref() else { return };
    if let Err(err) = workspace.dispatch_workspace_prompt(key, continuation_prompt(status)) {
        // The session is gone rather than merely erroring: spend the
        // budget and let the band surface it.
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "auto_continue_dispatch_failed",
            message = "continuation prompt could not be dispatched",
            outcome = "failure",
            session_key = %key.as_str(),
            error = %err,
        );
        if let Some(bucket) = app.sessions.get_mut(key) {
            bucket.auto_continue_attempts = MAX_ATTEMPTS;
        }
        super::turn::record_failed_turn(app, key);
        return;
    }
    // The turn error locked input on this session; it is live again.
    if app.active_session_key.as_ref() == Some(key) && matches!(app.status, AppStatus::Error) {
        app.status = AppStatus::Ready;
        app.exit_error = None;
    }
    crate::app::active_bucket_scope::with_pivoted(app, key.clone(), |app| {
        super::push_system_message_with_severity(
            app,
            Some(SystemSeverity::Warning),
            &notice_text(status, attempt),
        );
    });
    app.needs_redraw = true;
    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "auto_continued_after_server_error",
        message = "dispatched a continuation turn after a transient server error",
        outcome = "success",
        session_key = %key.as_str(),
        attempt,
        max_attempts = MAX_ATTEMPTS,
    );
}

/// Clear the streak once a turn completes, so a later unrelated outage
/// gets the full budget. Deliberately not wired to the `Running`
/// transition: the continuation's own turn goes `Running`, and
/// resetting there would uncap the loop.
pub(crate) fn note_turn_completed(app: &mut App, key: &SessionKey) {
    if let Some(bucket) = app.sessions.get_mut(key) {
        bucket.auto_continue_attempts = 0;
        bucket.auto_continue_due_at = None;
    }
}

/// The continuation turn's text. Names what happened, points at the
/// history, and forbids redoing finished work - a bare `?` restarts the
/// model's reasoning and it repeats steps it already completed, which
/// is the defect this replaces.
fn continuation_prompt(status: Option<u16>) -> String {
    let detail = status.map_or_else(
        || "a transient server error".to_owned(),
        |code| format!("a transient server error (HTTP {code})"),
    );
    format!(
        "Your previous request failed with {detail} and forge has continued the session \
automatically. Everything you completed before the failure is in the history above. Pick up \
exactly where you stopped - do not restart the task, and do not repeat any step, tool call \
or side effect that already completed."
    )
}

/// The in-chat notice: forge did this, and which attempt it was.
fn notice_text(status: Option<u16>, attempt: u32) -> String {
    let detail = status
        .map_or_else(|| "Server error".to_owned(), |code| format!("Server error (HTTP {code})"));
    format!(
        "{detail} ended the turn - forge continued the session automatically \
(attempt {attempt}/{MAX_ATTEMPTS}), asking the model to resume rather than restart."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use forge_primitives::ApiRetryError;
    use forge_workspace::SessionKey;
    use std::time::{Duration, SystemTime};

    /// A live session bucket with a `session_id` registered against the
    /// testing stub, so a dispatched `Command::Prompt` reaches the wire
    /// and shows up on the returned receiver.
    fn app_with_session()
    -> (App, SessionKey, tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand>) {
        let mut app = App::test_default();
        let rx = app.install_testing_stub();
        app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
        let key = app.active_session_key.clone().expect("active key");
        (app, key, rx)
    }

    /// Record the classification the way the wire does - via the
    /// retry that preceded the failure.
    fn seed_retry(app: &mut App, key: &SessionKey, error: ApiRetryError, status: Option<u16>) {
        app.sessions.get_mut(key).expect("bucket").last_api_retry = Some((error, status));
    }

    #[test]
    fn server_error_arms_a_continuation_and_skips_the_attention_row() {
        let (mut app, key, _rx) = app_with_session();
        let now = SystemTime::UNIX_EPOCH;

        let armed = arm_if_transient(&mut app, &key, ApiRetryError::ServerError, now);

        assert!(armed, "a 5xx is transient - forge continues it rather than parking it");
        let bucket = app.sessions.get(&key).expect("bucket");
        assert_eq!(
            bucket.auto_continue_due_at,
            Some(now + Duration::from_secs(5)),
            "first attempt waits the first backoff entry",
        );
    }

    /// A 429 needs its window to reset, which
    /// `rate_limit::maybe_recover_from_rate_limit_lock` already owns.
    /// The rest are not transient at all, so continuing is wrong.
    #[test]
    fn non_transient_classifications_never_arm() {
        for error in [
            ApiRetryError::RateLimit,
            ApiRetryError::AuthenticationFailed,
            ApiRetryError::BillingError,
            ApiRetryError::InvalidRequest,
            ApiRetryError::MaxOutputTokens,
            ApiRetryError::Unknown,
        ] {
            let (mut app, key, _rx) = app_with_session();
            let armed = arm_if_transient(&mut app, &key, error, SystemTime::UNIX_EPOCH);
            assert!(!armed, "{error:?} must fall through to the attention band, not auto-continue");
            assert!(
                app.sessions.get(&key).expect("bucket").auto_continue_due_at.is_none(),
                "{error:?} must not arm a timer",
            );
        }
    }

    #[test]
    fn backoff_grows_with_each_spent_attempt() {
        let expected = [5_u64, 20, 60];
        for (spent, secs) in expected.iter().enumerate() {
            let (mut app, key, _rx) = app_with_session();
            app.sessions.get_mut(&key).expect("bucket").auto_continue_attempts =
                u32::try_from(spent).expect("small");
            arm_if_transient(&mut app, &key, ApiRetryError::ServerError, SystemTime::UNIX_EPOCH);
            assert_eq!(
                app.sessions.get(&key).expect("bucket").auto_continue_due_at,
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(*secs)),
                "attempt {} spacing",
                spent + 1,
            );
        }
    }

    /// Once the budget is spent the failure must reach the user rather
    /// than loop forever.
    #[test]
    fn cap_exhaustion_refuses_to_arm() {
        let (mut app, key, _rx) = app_with_session();
        app.sessions.get_mut(&key).expect("bucket").auto_continue_attempts = MAX_ATTEMPTS;

        let armed =
            arm_if_transient(&mut app, &key, ApiRetryError::ServerError, SystemTime::UNIX_EPOCH);

        assert!(!armed, "the {MAX_ATTEMPTS}-attempt cap is hard");
    }

    #[test]
    fn nothing_fires_before_the_backoff_elapses() {
        let (mut app, key, mut rx) = app_with_session();
        app.sessions.get_mut(&key).expect("bucket").auto_continue_due_at =
            Some(SystemTime::now() + Duration::from_secs(300));

        maybe_fire(&mut app);

        assert!(rx.try_recv().is_err(), "the timer has not elapsed - no prompt yet");
    }

    /// The continuation is a fresh forge-authored turn, never a replay
    /// of the user's original prompt, and it tells the model to resume
    /// rather than restart.
    #[test]
    fn firing_dispatches_a_continuation_prompt_that_forbids_redoing_work() {
        let (mut app, key, mut rx) = app_with_session();
        seed_retry(&mut app, &key, ApiRetryError::ServerError, Some(529));
        app.sessions.get_mut(&key).expect("bucket").auto_continue_due_at =
            Some(SystemTime::now() - Duration::from_secs(1));

        maybe_fire(&mut app);

        let cmd = rx.try_recv().expect("a continuation prompt is dispatched");
        let forge_primitives::AgentCommand::PromptWithImages { session_id, text: prompt, images } =
            cmd
        else {
            panic!("expected PromptWithImages, got {cmd:?}");
        };
        assert_eq!(session_id, "session-1");
        assert!(images.is_empty(), "a continuation carries no attachments");
        assert!(prompt.contains("HTTP 529"), "names the failure: {prompt}");
        assert!(prompt.contains("do not restart"), "forbids restarting: {prompt}");
        assert!(
            prompt.contains("do not repeat"),
            "forbids repeating completed side effects: {prompt}",
        );
        assert!(
            prompt.contains("history above"),
            "points the model at the work already in history: {prompt}",
        );
    }

    /// Ved's requirement: silent recovery is a non-goal. The chat must
    /// say forge did this, and which attempt it was.
    #[test]
    fn firing_pushes_a_visible_notice_naming_the_attempt() {
        let (mut app, key, _rx) = app_with_session();
        seed_retry(&mut app, &key, ApiRetryError::ServerError, Some(529));
        app.sessions.get_mut(&key).expect("bucket").auto_continue_due_at =
            Some(SystemTime::now() - Duration::from_secs(1));

        maybe_fire(&mut app);

        let text: String = app
            .messages()
            .iter()
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                crate::app::MessageBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("HTTP 529"), "notice names the status: {text}");
        assert!(text.contains("forge"), "notice attributes the retry to forge: {text}");
        assert!(text.contains("attempt 1/3"), "notice carries the attempt number: {text}");
    }

    #[test]
    fn firing_releases_the_input_lock_on_the_active_session() {
        let (mut app, key, _rx) = app_with_session();
        app.status = crate::app::AppStatus::Error;
        app.sessions.get_mut(&key).expect("bucket").auto_continue_due_at =
            Some(SystemTime::now() - Duration::from_secs(1));

        maybe_fire(&mut app);

        assert!(
            matches!(app.status, crate::app::AppStatus::Ready),
            "the session is live again, so the turn-error input lock must lift",
        );
    }

    /// One fire per armed timer - a burst of ticks must not fan out
    /// into a burst of turns.
    #[test]
    fn firing_disarms_so_a_second_tick_sends_nothing() {
        let (mut app, key, mut rx) = app_with_session();
        app.sessions.get_mut(&key).expect("bucket").auto_continue_due_at =
            Some(SystemTime::now() - Duration::from_secs(1));

        maybe_fire(&mut app);
        maybe_fire(&mut app);

        assert!(rx.try_recv().is_ok(), "first tick fires");
        assert!(rx.try_recv().is_err(), "second tick must not re-fire the same continuation");
    }

    /// A completed turn means the outage passed; a later unrelated one
    /// gets the full budget again.
    #[test]
    fn a_completed_turn_resets_the_streak() {
        let (mut app, key, _rx) = app_with_session();
        app.sessions.get_mut(&key).expect("bucket").auto_continue_attempts = 2;

        note_turn_completed(&mut app, &key);

        assert_eq!(app.sessions.get(&key).expect("bucket").auto_continue_attempts, 0);
    }

    /// A continuation is a mid-turn-capable dispatch: when any turn
    /// (typed, or a counted cron/peer/gotify fire) starts inside the
    /// backoff window, the helper signals `PromptQueuedWhileBusy` and
    /// the TUI counts the continuation into the bridge; an idle fire
    /// stays silent.
    #[test]
    fn mid_turn_continuation_counts_as_a_queued_send_and_idle_fire_is_silent() {
        use super::super::apply_session_update;
        use forge_workspace::SessionUpdate;

        let mut app = App::test_default();
        let (ws, mut updates) = forge_workspace::Workspace::testing_stub();
        app.workspace = Some(ws);
        let _rx = app.install_testing_stub();
        app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
        let key = app.active_session_key.clone().expect("active key");

        app.sessions.get_mut(&key).expect("bucket").auto_continue_due_at =
            Some(SystemTime::now() - Duration::from_secs(1));
        maybe_fire(&mut app);
        assert!(
            updates.try_recv().is_err(),
            "an idle continuation must not signal PromptQueuedWhileBusy",
        );

        // The counted case models a turn that started inside the
        // backoff window, so the bucket carries a live turn.
        app.sessions.get_mut(&key).expect("bucket").lifecycle_state =
            crate::app::session::SessionLifecycleState::Running;
        app.workspace
            .as_ref()
            .expect("workspace")
            .domain_session_for(&key)
            .expect("domain session")
            .lock()
            .turn_pending = true;
        app.sessions.get_mut(&key).expect("bucket").auto_continue_due_at =
            Some(SystemTime::now() - Duration::from_secs(1));
        maybe_fire(&mut app);

        let signal = updates.try_recv().expect("a mid-turn continuation signals");
        assert!(
            matches!(signal, SessionUpdate::PromptQueuedWhileBusy { .. }),
            "the signal carries the queue event, got {signal:?}",
        );
        apply_session_update(&mut app, signal);
        let bucket = app.sessions.get(&key).expect("bucket");
        assert_eq!(bucket.queued_turn_sends, 1, "the continuation counts as one queued send");
    }
}
