use forge_workspace::Osc9NotificationMode;
use forge_workspace::SessionKey;
use std::borrow::Cow;

/// Events that can trigger a user notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyEvent {
    /// A tool call requires explicit user approval.
    PermissionRequired,
    /// `AskUserQuestion` is waiting for structured input.
    QuestionRequired,
    /// The agent finished its turn.
    TurnComplete,
}

/// What the notification text is built from, resolved from the
/// event's own session at notify time - never the active tab's, so a
/// worker's question names the worker while the user reads another
/// session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifyContext {
    /// The session's forge.toml project name (`UiSession.project`).
    /// Every session carries one; there is no pre-Connect window in
    /// which a bucket exists without it.
    pub project: String,
    /// The session's live-worker label. `None` for a lead session.
    /// Resolved from the live-worker registry, never the sessions
    /// catalog - workers are deliberately absent from it.
    pub worker_label: Option<String>,
}

/// The strings one notification delivers: a short title (the project)
/// and the detail line (session kind + event phrase).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotificationText {
    pub title: String,
    pub detail: String,
}

impl NotificationText {
    /// The single-line form the OSC 9 escape carries.
    fn osc9_line(&self) -> String {
        format!("{} - {}", self.title, self.detail)
    }
}

/// What one unfocused notify() delivered, recorded instead of sent
/// when the `testing` feature is on: the OSC 9 line, in delivery order.
#[cfg(feature = "testing")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredNotification {
    pub osc9_line: String,
}

/// Central notification manager.
///
/// Tracks whether the terminal window is focused (via crossterm
/// `FocusGained`/`FocusLost` events backed by DECSET 1004) and dispatches
/// notifications only when the window is **not** focused.
///
/// One delivery: the OSC 9 escape, always written, whether or not the
/// host terminal is known to render it. A terminal that ignores the
/// sequence is harmless, so nothing is planned around the answer.
#[derive(Debug)]
pub struct NotificationManager {
    terminal_focused: bool,
    osc9_mode: Osc9NotificationMode,
    #[cfg(feature = "testing")]
    delivered: std::cell::RefCell<Vec<DeliveredNotification>>,
}

impl Default for NotificationManager {
    fn default() -> Self {
        Self::new(Osc9NotificationMode::default())
    }
}

impl NotificationManager {
    pub const fn new(osc9_mode: Osc9NotificationMode) -> Self {
        // Default to `true` (focused) so that terminals which do not support
        // DECSET 1004 never fire spurious notifications.
        Self {
            terminal_focused: true,
            osc9_mode,
            #[cfg(feature = "testing")]
            delivered: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// Call when the terminal emits a `FocusGained` event.
    pub fn on_focus_gained(&mut self) {
        self.terminal_focused = true;
    }

    /// Call when the terminal emits a `FocusLost` event.
    pub fn on_focus_lost(&mut self) {
        self.terminal_focused = false;
    }

    /// Whether the terminal window currently has OS focus.
    pub const fn is_focused(&self) -> bool {
        self.terminal_focused
    }

    #[cfg(test)]
    pub(crate) const fn osc9_mode(&self) -> Osc9NotificationMode {
        self.osc9_mode
    }

    /// Send a notification if the terminal is not focused.
    ///
    /// This is the single entry-point that all event handlers should call.
    /// It is intentionally cheap when focused (just a bool check).
    /// `session_key` is the event's own session, logged beside the
    /// resolved context so a wrong title is diagnosable from the log.
    pub fn notify(&self, event: NotifyEvent, session_key: &SessionKey, context: &NotifyContext) {
        if self.terminal_focused {
            return;
        }
        if matches!(self.osc9_mode, Osc9NotificationMode::Off) {
            tracing::info!(
                target: crate::logging::targets::APP_NOTIFY,
                event_name = "notification_planned_no_channels",
                message = "notification channel disabled; nothing dispatched",
                outcome = "skipped",
                session_key = %session_key.as_str(),
                event = ?event,
            );
            return;
        }
        let text = notification_text(event, &context.project, context.worker_label.as_deref());
        let line = text.osc9_line();
        send_osc9_notification(&line);
        tracing::info!(
            target: crate::logging::targets::APP_NOTIFY,
            event_name = "notification_fired",
            message = "unfocused notification dispatched",
            outcome = "success",
            session_key = %session_key.as_str(),
            resolved_project = ?context.project,
            resolved_worker_label = ?context.worker_label,
            event = ?event,
            title = %text.title,
            detail = %text.detail,
        );
        // The `testing` feature records what was delivered so tests
        // can assert it; the send above still runs.
        #[cfg(feature = "testing")]
        self.delivered.borrow_mut().push(DeliveredNotification { osc9_line: line });
    }

    /// Test-only: drain what the unfocused notify()s delivered, in
    /// order. Populated only with the `testing` feature on.
    #[cfg(feature = "testing")]
    pub fn take_delivered(&self) -> Vec<DeliveredNotification> {
        self.delivered.borrow_mut().drain(..).collect()
    }
}

impl crate::app::App {
    /// Raise `event` for `session_key`'s session through this app's
    /// notification manager, using the app's configured channel. The
    /// single call site for every notification, so nothing grows a
    /// second policy about when to notify: the manager's own
    /// terminal-focus check decides that. The notification text comes
    /// from the event session's project + worker label.
    pub(crate) fn notify(&self, event: NotifyEvent, session_key: &SessionKey) {
        // A session with no bucket has nothing to notify about, so this
        // is where an event for a closed or never-spawned key stops.
        let Some(context) = self.notification_context(session_key) else {
            tracing::warn!(
                target: crate::logging::targets::APP_NOTIFY,
                event_name = "notification_session_missing",
                message = "no session bucket for the event's key; nothing to notify about",
                outcome = "skipped",
                session_key = %session_key.as_str(),
                event = ?event,
            );
            return;
        };
        // The test capture drains regardless of focus (tests run
        // focused); production skips the delivery while focused.
        #[cfg(feature = "testing")]
        self.test_notifications.borrow_mut().push((event, context.clone()));
        if self.notifications.is_focused() {
            tracing::info!(
                target: crate::logging::targets::APP_NOTIFY,
                event_name = "notification_suppressed_focused",
                message = "notification suppressed because terminal is focused",
                outcome = "skipped",
                session_key = %session_key.as_str(),
                event = ?event,
            );
            return;
        }
        self.notifications.notify(event, session_key, &context);
    }

    /// The event session's project + worker label, read at notify time.
    /// `None` when the key has no bucket, which is the only way a
    /// notification has nothing to name. The worker label comes from
    /// the live-worker registry: a worker's bucket is minted by its
    /// `Connected`, which carries no label, so the registry is the only
    /// source.
    fn notification_context(&self, session_key: &SessionKey) -> Option<NotifyContext> {
        let bucket = self.sessions.get(session_key)?;
        Some(NotifyContext {
            project: bucket.project.clone(),
            worker_label: self
                .workspace
                .as_ref()
                .and_then(|ws| ws.worker_lookup_for_session(session_key))
                .map(|(_, label, _)| label),
        })
    }
}

/// Same-crate test helper for the `testing`-feature notification
/// capture, mirroring `events::turn::test_capture`.
#[cfg(test)]
pub(crate) mod test_capture {
    use super::NotifyContext;
    use super::NotifyEvent;

    /// Test-only: drain every notification raised so far, in order.
    pub fn take_notifications(app: &crate::app::App) -> Vec<(NotifyEvent, NotifyContext)> {
        #[rustfmt::skip] #[cfg(feature = "testing")] let drained = app.test_notifications.borrow_mut().drain(..).collect();
        drained
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn send_osc9_notification(message: &str) {
    use std::io::Write;

    let sequence = osc9_escape_sequence(message);
    let result =
        std::io::stdout().write_all(sequence.as_bytes()).and_then(|()| std::io::stdout().flush());
    if let Err(error) = result {
        tracing::warn!(
            target: crate::logging::targets::APP_NOTIFY,
            event_name = "osc9_send_failed",
            message = "could not write the OSC 9 notification sequence",
            outcome = "failure",
            error_message = %error,
        );
    }
}

/// Build the delivered strings for one event from the session's
/// project + worker label. The title is the project; the detail names
/// the session's kind and the event, because on the OSC 9 path this
/// line is the only thing forge controls - the terminal supplies the
/// rest of the banner.
fn notification_text(
    event: NotifyEvent,
    project: &str,
    worker_label: Option<&str>,
) -> NotificationText {
    let title = project.to_owned();
    let kind = match worker_label {
        Some(label) => format!("worker {label}"),
        None => "lead".to_owned(),
    };
    let happened = match event {
        NotifyEvent::TurnComplete => "turn complete",
        NotifyEvent::PermissionRequired => "needs input",
        NotifyEvent::QuestionRequired => "needs your answer",
    };
    NotificationText { title, detail: format!("{kind} - {happened}") }
}

fn osc9_escape_sequence(message: &str) -> Cow<'_, str> {
    let sanitized = sanitize_osc9_message(message);
    let mut sequence = String::with_capacity(sanitized.len() + 8);
    sequence.push('\u{1b}');
    sequence.push_str("]9;");
    sequence.push_str(&sanitized);
    sequence.push('\u{1b}');
    sequence.push('\\');
    Cow::Owned(sequence)
}

fn sanitize_osc9_message(message: &str) -> String {
    let mut sanitized = String::with_capacity(message.len());
    for ch in message.chars() {
        match ch {
            '\u{07}' | '\u{1b}' | '\u{9c}' => {}
            '\r' | '\n' => sanitized.push(' '),
            _ => sanitized.push(ch),
        }
    }
    sanitized
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::app::session::UiSession;

    #[test]
    fn defaults_to_focused() {
        let mgr = NotificationManager::new(Osc9NotificationMode::default());
        assert!(mgr.is_focused(), "should default to focused to suppress spurious notifications");
    }

    #[test]
    fn focus_lost_sets_unfocused() {
        let mut mgr = NotificationManager::new(Osc9NotificationMode::default());
        mgr.on_focus_lost();
        assert!(!mgr.is_focused());
    }

    #[test]
    fn focus_gained_restores_focused() {
        let mut mgr = NotificationManager::new(Osc9NotificationMode::default());
        mgr.on_focus_lost();
        mgr.on_focus_gained();
        assert!(mgr.is_focused());
    }

    #[test]
    fn turn_complete_text_names_the_project_and_the_session_kind() {
        let worker = notification_text(NotifyEvent::TurnComplete, "forge", Some("chat-stutter"));
        assert_eq!(
            worker.osc9_line(),
            "forge - worker chat-stutter - turn complete",
            "the line carries project, kind, label and event on its own",
        );

        let lead = notification_text(NotifyEvent::TurnComplete, "forge", None);
        assert_eq!(lead.osc9_line(), "forge - lead - turn complete");

        assert_ne!(
            worker.osc9_line(),
            lead.osc9_line(),
            "a worker's turn-complete must not read as a lead's",
        );
    }

    #[test]
    fn permission_text_names_the_project_and_the_session_kind() {
        let worker =
            notification_text(NotifyEvent::PermissionRequired, "busymail", Some("demo-route"));
        assert_eq!(worker.osc9_line(), "busymail - worker demo-route - needs input");

        let lead = notification_text(NotifyEvent::PermissionRequired, "busymail", None);
        assert_eq!(lead.osc9_line(), "busymail - lead - needs input");
    }

    #[test]
    fn question_text_names_the_project_and_the_session_kind() {
        let worker =
            notification_text(NotifyEvent::QuestionRequired, "busymail", Some("demo-route"));
        assert_eq!(worker.osc9_line(), "busymail - worker demo-route - needs your answer");

        let lead = notification_text(NotifyEvent::QuestionRequired, "busymail", None);
        assert_eq!(lead.osc9_line(), "busymail - lead - needs your answer");
    }

    /// A key with no bucket is where an "unresolved project" now lands:
    /// `notify` returns early, so nothing is delivered and nothing reads
    /// as the app name.
    #[test]
    fn an_unresolved_project_delivers_nothing_rather_than_the_app_name() {
        let mut app = App::test_default();
        app.notifications = NotificationManager::new(Osc9NotificationMode::On);
        app.notifications.on_focus_lost();
        let unknown = forge_workspace::SessionKey::from_session_id("no-such-session");

        app.notify(NotifyEvent::TurnComplete, &unknown);

        assert!(
            app.notifications.take_delivered().is_empty(),
            "an event with no bucket must deliver nothing, never a line reading as the app name",
        );
    }

    fn seed_bucket(app: &mut App, id: &str, project: &str) -> forge_workspace::SessionKey {
        let key = forge_workspace::SessionKey::from_str_for_test(id);
        let bucket = UiSession::new(key.clone(), project);
        app.sessions.insert(key.clone(), bucket);
        key
    }

    fn seed_worker(
        app: &App,
        project_key: &forge_workspace::ProjectKey,
        key: &forge_workspace::SessionKey,
        label: &str,
    ) {
        let ws = app.workspace.as_ref().expect("test workspace");
        ws.insert_live_worker(
            project_key,
            forge_workspace::WorkerEntry {
                label: label.to_owned(),
                charter: String::new(),
                session_key: key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: String::new(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
    }

    #[test]
    fn notification_context_reads_the_event_session_not_the_active_one() {
        let mut app = App::test_default();
        let active = seed_bucket(&mut app, "session-a", "alpha");
        let worker = seed_bucket(&mut app, "session-b", "beta");
        app.active_session_key = Some(active.clone());
        seed_worker(
            &app,
            &forge_workspace::ProjectKey::new_for_test("p-beta"),
            &worker,
            "egen-lead",
        );

        assert_eq!(
            app.notification_context(&worker),
            Some(NotifyContext {
                project: "beta".to_owned(),
                worker_label: Some("egen-lead".to_owned()),
            }),
            "the event session's project + worker label, never the active tab's",
        );
        assert_eq!(
            app.notification_context(&active),
            Some(NotifyContext { project: "alpha".to_owned(), worker_label: None }),
            "a lead session resolves no worker label",
        );
    }

    #[test]
    fn notification_context_is_none_for_an_unknown_session() {
        let app = App::test_default();
        let unknown = forge_workspace::SessionKey::from_session_id("no-such-session");
        assert_eq!(app.notification_context(&unknown), None);
    }

    /// A focused terminal (the manager's default) delivers nothing.
    #[test]
    fn focused_terminal_delivers_nothing() {
        let mut app = App::test_default();
        let key = seed_bucket(&mut app, "session-a", "companies");

        app.notify(NotifyEvent::TurnComplete, &key);

        assert!(
            app.notifications.take_delivered().is_empty(),
            "a focused terminal delivers nothing",
        );
    }

    /// The single line the escape carries stands alone: the event
    /// session's project, its kind, the worker's label where there is
    /// one, and the event.
    #[test]
    fn unfocused_worker_turn_complete_line_names_the_worker() {
        let mut app = App::test_default();
        let lead_key = seed_bucket(&mut app, "session-lead", "beta");
        let worker_key = seed_bucket(&mut app, "session-worker", "beta");
        seed_worker(
            &app,
            &forge_workspace::ProjectKey::new_for_test("p-beta"),
            &worker_key,
            "chat-stutter",
        );
        app.notifications = NotificationManager::new(Osc9NotificationMode::On);
        app.notifications.on_focus_lost();

        app.notify(NotifyEvent::TurnComplete, &lead_key);
        app.notify(NotifyEvent::TurnComplete, &worker_key);

        let lines: Vec<_> = app
            .notifications
            .take_delivered()
            .into_iter()
            .map(|delivered| delivered.osc9_line)
            .collect();
        assert_eq!(
            lines,
            vec!["beta - lead - turn complete", "beta - worker chat-stutter - turn complete"],
            "the two turn-completes are told apart on the line alone",
        );
    }

    /// Permission and question events reach the line through the same
    /// path a turn complete does, worker label included.
    #[test]
    fn unfocused_worker_prompts_name_the_worker_on_the_line() {
        let mut app = App::test_default();
        let worker_key = seed_bucket(&mut app, "session-worker", "busymail");
        seed_worker(
            &app,
            &forge_workspace::ProjectKey::new_for_test("p-busymail"),
            &worker_key,
            "demo-route",
        );
        app.notifications = NotificationManager::new(Osc9NotificationMode::On);
        app.notifications.on_focus_lost();

        app.notify(NotifyEvent::PermissionRequired, &worker_key);
        app.notify(NotifyEvent::QuestionRequired, &worker_key);

        let lines: Vec<_> = app
            .notifications
            .take_delivered()
            .into_iter()
            .map(|delivered| delivered.osc9_line)
            .collect();
        assert_eq!(
            lines,
            vec![
                "busymail - worker demo-route - needs input",
                "busymail - worker demo-route - needs your answer",
            ],
        );
    }

    /// Nothing persisted can turn a notification off. A stored channel
    /// preference that used to select "no notification" has no reader
    /// left, so the escape is written regardless.
    #[test]
    fn a_stored_channel_preference_cannot_suppress_the_escape() {
        let mut app = App::test_default();
        let key = seed_bucket(&mut app, "session-a", "companies");
        app.config.committed_preferences_document =
            serde_json::json!({ "preferredNotifChannel": "notifications_disabled" });
        app.notifications.on_focus_lost();

        app.notify(NotifyEvent::TurnComplete, &key);

        let lines: Vec<_> = app
            .notifications
            .take_delivered()
            .into_iter()
            .map(|delivered| delivered.osc9_line)
            .collect();
        assert_eq!(
            lines,
            vec!["companies - lead - turn complete"],
            "a stored channel preference must not change what is delivered",
        );
    }

    /// The escape is the whole delivery, so switching it off delivers
    /// nothing.
    #[test]
    fn unfocused_terminal_delivers_nothing_when_the_escape_is_off() {
        let mut app = App::test_default();
        let key = seed_bucket(&mut app, "session-a", "companies");
        app.notifications = NotificationManager::new(Osc9NotificationMode::Off);
        app.notifications.on_focus_lost();

        app.notify(NotifyEvent::TurnComplete, &key);

        assert!(
            app.notifications.take_delivered().is_empty(),
            "the escape is the whole delivery, so switching it off delivers nothing",
        );
    }

    /// An unfocused notification is the escape and nothing else: no
    /// capability decides it and no channel plans around it.
    #[test]
    fn an_unfocused_notification_carries_the_escape_and_nothing_else() {
        let mut app = App::test_default();
        let key = seed_bucket(&mut app, "session-a", "companies");
        app.notifications.on_focus_lost();

        app.notify(NotifyEvent::TurnComplete, &key);

        assert_eq!(
            app.notifications.take_delivered(),
            vec![DeliveredNotification { osc9_line: "companies - lead - turn complete".to_owned() }],
            "the escape is the whole delivery",
        );
    }

    #[test]
    fn osc9_sequence_uses_st_terminator_and_sanitizes_message() {
        assert_eq!(
            osc9_escape_sequence("hello\n\u{1b}world\u{07}").as_ref(),
            "\u{1b}]9;hello world\u{1b}\\"
        );
    }
}
