use super::config::PreferredNotifChannel;
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
    /// `None` before Connect.
    pub project: Option<String>,
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
/// when the `testing` feature is on: the OSC 9 line and the bell, in
/// delivery order.
#[cfg(feature = "testing")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredNotification {
    pub osc9_line: Option<String>,
    pub bell: bool,
}

/// Central notification manager.
///
/// Tracks whether the terminal window is focused (via crossterm
/// `FocusGained`/`FocusLost` events backed by DECSET 1004) and dispatches
/// notifications only when the window is **not** focused.
///
/// Two notification layers exist; the channel decides which run:
/// 1. **Terminal bell** (`BEL \x07`) -- causes a taskbar flash / dock bounce
///    on virtually every terminal emulator.
/// 2. **OSC 9 escape** -- while the terminal is believed to support it,
///    channels that can emit it suppress the bell (except on
///    `iterm2_with_bell`), so a multiplexer that strips the escape silently
///    leaves nothing. The `[ui] notifications_osc9` forge.toml key forces
///    that belief either way: `off` leaves the Iterm2 channel the bell alone
///    and Ghostty nothing at all, `on` sends the escape regardless of
///    detection.
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationPlan {
    ring_bell: bool,
    osc9_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalCapabilities {
    osc9_notifications: bool,
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
    pub fn notify(
        &self,
        channel: PreferredNotifChannel,
        event: NotifyEvent,
        session_key: &SessionKey,
        context: &NotifyContext,
    ) {
        if self.terminal_focused {
            return;
        }
        let text =
            notification_text(event, context.project.as_deref(), context.worker_label.as_deref());
        let plan =
            notification_plan(channel, detect_terminal_capabilities(), self.osc9_mode, &text);
        if let Some(line) = &plan.osc9_text {
            send_osc9_notification(line);
        }
        if plan.ring_bell {
            ring_bell();
        }
        let dispatched = plan.ring_bell || plan.osc9_text.is_some();
        tracing::info!(
            target: crate::logging::targets::APP_NOTIFY,
            event_name = if dispatched { "notification_fired" } else { "notification_planned_no_channels" },
            message = if dispatched {
                "unfocused notification dispatched"
            } else {
                "notification channel disabled; nothing dispatched"
            },
            outcome = if dispatched { "success" } else { "skipped" },
            session_key = %session_key.as_str(),
            resolved_project = ?context.project,
            resolved_worker_label = ?context.worker_label,
            event = ?event,
            channel = ?channel,
            title = %text.title,
            detail = %text.detail,
            ring_bell = plan.ring_bell,
            osc9 = plan.osc9_text.is_some(),
        );
        // The `testing` feature records what was delivered so tests
        // can assert it; the sends above still run.
        #[cfg(feature = "testing")]
        self.delivered.borrow_mut().push(DeliveredNotification {
            osc9_line: plan.osc9_text.clone(),
            bell: plan.ring_bell,
        });
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
        // The test capture drains regardless of focus (tests run
        // focused); production skips the lookup entirely while focused.
        #[cfg(feature = "testing")]
        {
            let context = self.notification_context(session_key);
            self.test_notifications.borrow_mut().push((event, context));
        }
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
        let context = self.notification_context(session_key);
        self.notifications.notify(
            self.config.preferred_notification_channel_effective(),
            event,
            session_key,
            &context,
        );
    }

    /// The event session's project name + worker label, read at notify
    /// time. An unresolved project means the delivered line cannot name
    /// one, so it is logged rather than papered over with a name that
    /// would read as plausible.
    fn notification_context(&self, session_key: &SessionKey) -> NotifyContext {
        let project = self.sessions.get(session_key).and_then(|bucket| bucket.project.clone());
        if project.is_none() {
            tracing::warn!(
                target: crate::logging::targets::APP_NOTIFY,
                event_name = "notification_project_unresolved",
                message = "notification session has no project; the delivered line cannot name one",
                outcome = "degraded",
                session_key = %session_key.as_str(),
            );
        }
        NotifyContext {
            project,
            worker_label: self
                .workspace
                .as_ref()
                .and_then(|ws| ws.worker_lookup_for_session(session_key))
                .map(|(_, label, _)| label),
        }
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

/// Write the ASCII BEL character to stdout, causing a taskbar flash / dock
/// bounce in most terminal emulators.
fn ring_bell() {
    use std::io::Write;
    let result = std::io::stdout().write_all(b"\x07").and_then(|()| std::io::stdout().flush());
    if let Err(error) = result {
        tracing::warn!(
            target: crate::logging::targets::APP_NOTIFY,
            event_name = "bell_send_failed",
            message = "could not write the terminal bell",
            outcome = "failure",
            error_message = %error,
        );
    }
}

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

fn notification_plan(
    channel: PreferredNotifChannel,
    capabilities: TerminalCapabilities,
    osc9_mode: Osc9NotificationMode,
    text: &NotificationText,
) -> NotificationPlan {
    let osc9_available = match osc9_mode {
        Osc9NotificationMode::Auto => capabilities.osc9_notifications,
        Osc9NotificationMode::On => true,
        Osc9NotificationMode::Off => false,
    };
    let osc9_text = osc9_available.then(|| text.osc9_line());
    match channel {
        PreferredNotifChannel::NotificationsDisabled => {
            NotificationPlan { ring_bell: false, osc9_text: None }
        }
        PreferredNotifChannel::TerminalBell => {
            NotificationPlan { ring_bell: true, osc9_text: None }
        }
        // "Auto / iTerm2" replaced the original always-bell behavior.
        // Preserve that reliable fallback whenever OSC 9 is unavailable.
        PreferredNotifChannel::Iterm2 => {
            NotificationPlan { ring_bell: osc9_text.is_none(), osc9_text }
        }
        PreferredNotifChannel::Ghostty => NotificationPlan { ring_bell: false, osc9_text },
        PreferredNotifChannel::Iterm2WithBell => NotificationPlan { ring_bell: true, osc9_text },
    }
}

fn detect_terminal_capabilities() -> TerminalCapabilities {
    terminal_capabilities_from_env(
        std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?))),
    )
}

fn terminal_capabilities_from_env<I>(vars: I) -> TerminalCapabilities
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut term_program = None::<String>;
    let mut term = None::<String>;
    let mut iterm_session = false;

    for (key, value) in vars {
        match key.as_str() {
            "TERM_PROGRAM" => term_program = Some(value),
            "TERM" => term = Some(value),
            "ITERM_SESSION_ID" if !value.is_empty() => iterm_session = true,
            _ => {}
        }
    }

    // `TERM` is read because a multiplexer can drop `TERM_PROGRAM` while
    // forwarding `TERM` unchanged, which is how shpool reaches Ghostty.
    let osc9_notifications = matches!(term_program.as_deref(), Some("iTerm.app" | "ghostty"))
        || matches!(term.as_deref(), Some("xterm-ghostty"))
        || iterm_session;
    TerminalCapabilities { osc9_notifications }
}

/// The title an unresolved project renders as. A missing project means
/// the session bucket is unstamped, which is a bug: naming it loudly
/// beats substituting a plausible string that hides the bug.
const UNKNOWN_PROJECT: &str = "unknown project";

/// Build the delivered strings for one event from the session's
/// project + worker label. The title is the project; the detail names
/// the session's kind and the event, because on the OSC 9 path this
/// line is the only thing forge controls - the terminal supplies the
/// rest of the banner.
fn notification_text(
    event: NotifyEvent,
    project: Option<&str>,
    worker_label: Option<&str>,
) -> NotificationText {
    let title = project.unwrap_or(UNKNOWN_PROJECT).to_owned();
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

    // Fixed text for the plan tests: channel gating is what they pin,
    // not wording.
    fn fixture_text() -> NotificationText {
        NotificationText {
            title: "companies".to_owned(),
            detail: "lead - turn complete".to_owned(),
        }
    }

    #[test]
    fn disabled_notifications_plan_is_silent() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::NotificationsDisabled,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: false, osc9_text: None }
        );
    }

    #[test]
    fn terminal_bell_plan_rings_only_the_bell() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::TerminalBell,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: true, osc9_text: None }
        );
    }

    #[test]
    fn iterm2_uses_osc9_when_supported() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan {
                ring_bell: false,
                osc9_text: Some("companies - lead - turn complete".to_owned()),
            }
        );
    }

    #[test]
    fn iterm2_auto_falls_back_to_the_bell_when_osc9_is_unavailable() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2,
                TerminalCapabilities { osc9_notifications: false },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: true, osc9_text: None }
        );
    }

    #[test]
    fn iterm2_with_bell_uses_osc9_and_bell_when_supported() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2WithBell,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan {
                ring_bell: true,
                osc9_text: Some("companies - lead - turn complete".to_owned()),
            }
        );
    }

    #[test]
    fn iterm2_with_bell_keeps_the_bell_when_osc9_is_unavailable() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2WithBell,
                TerminalCapabilities { osc9_notifications: false },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: true, osc9_text: None }
        );
    }

    #[test]
    fn ghostty_uses_osc9_when_supported() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Ghostty,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan {
                ring_bell: false,
                osc9_text: Some("companies - lead - turn complete".to_owned()),
            }
        );
    }

    #[test]
    fn detects_iterm2_via_term_program() {
        let capabilities =
            terminal_capabilities_from_env([("TERM_PROGRAM".to_owned(), "iTerm.app".to_owned())]);

        assert!(capabilities.osc9_notifications);
    }

    #[test]
    fn detects_iterm2_via_session_id() {
        let capabilities =
            terminal_capabilities_from_env([("ITERM_SESSION_ID".to_owned(), "w0t1p0".to_owned())]);

        assert!(capabilities.osc9_notifications);
    }

    #[test]
    fn detects_ghostty_via_term_program() {
        let capabilities =
            terminal_capabilities_from_env([("TERM_PROGRAM".to_owned(), "ghostty".to_owned())]);

        assert!(capabilities.osc9_notifications);
    }

    #[test]
    fn detects_ghostty_via_term_when_term_program_is_absent() {
        let capabilities =
            terminal_capabilities_from_env([("TERM".to_owned(), "xterm-ghostty".to_owned())]);

        assert!(
            capabilities.osc9_notifications,
            "TERM survives a multiplexer that drops TERM_PROGRAM",
        );
    }

    #[test]
    fn a_term_that_is_not_ghostty_does_not_advertise_osc9() {
        let capabilities =
            terminal_capabilities_from_env([("TERM".to_owned(), "xterm-256color".to_owned())]);

        assert!(
            !capabilities.osc9_notifications,
            "only ghostty's TERM value counts, so this branch is not matching every TERM",
        );
    }

    #[test]
    fn unsupported_term_does_not_advertise_osc9() {
        let capabilities =
            terminal_capabilities_from_env([("TERM_PROGRAM".to_owned(), "wezterm".to_owned())]);

        assert!(!capabilities.osc9_notifications);
    }

    #[test]
    fn osc9_override_off_forces_iterm2_to_the_bell() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Off,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: true, osc9_text: None }
        );
    }

    #[test]
    fn osc9_override_off_leaves_ghostty_with_no_channel() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Ghostty,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Off,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: false, osc9_text: None },
            "Ghostty has no fallback once the escape is off",
        );
    }

    #[test]
    fn osc9_override_on_sends_osc9_from_iterm2_despite_negative_detection() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2,
                TerminalCapabilities { osc9_notifications: false },
                Osc9NotificationMode::On,
                &fixture_text(),
            ),
            NotificationPlan {
                ring_bell: false,
                osc9_text: Some("companies - lead - turn complete".to_owned()),
            },
            "On forces OSC 9 and suppresses the bell, detection notwithstanding",
        );
    }

    #[test]
    fn osc9_override_on_sends_osc9_from_ghostty_despite_negative_detection() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Ghostty,
                TerminalCapabilities { osc9_notifications: false },
                Osc9NotificationMode::On,
                &fixture_text(),
            ),
            NotificationPlan {
                ring_bell: false,
                osc9_text: Some("companies - lead - turn complete".to_owned()),
            },
            "On forces OSC 9 for a terminal that does not announce itself",
        );
    }

    #[test]
    fn turn_complete_text_names_the_project_and_the_session_kind() {
        let worker =
            notification_text(NotifyEvent::TurnComplete, Some("forge"), Some("chat-stutter"));
        assert_eq!(
            worker.osc9_line(),
            "forge - worker chat-stutter - turn complete",
            "the line carries project, kind, label and event on its own",
        );

        let lead = notification_text(NotifyEvent::TurnComplete, Some("forge"), None);
        assert_eq!(lead.osc9_line(), "forge - lead - turn complete");

        assert_ne!(
            worker.osc9_line(),
            lead.osc9_line(),
            "a worker's turn-complete must not read as a lead's",
        );
    }

    #[test]
    fn permission_text_names_the_project_and_the_session_kind() {
        let worker = notification_text(
            NotifyEvent::PermissionRequired,
            Some("busymail"),
            Some("demo-route"),
        );
        assert_eq!(worker.osc9_line(), "busymail - worker demo-route - needs input");

        let lead = notification_text(NotifyEvent::PermissionRequired, Some("busymail"), None);
        assert_eq!(lead.osc9_line(), "busymail - lead - needs input");
    }

    #[test]
    fn question_text_names_the_project_and_the_session_kind() {
        let worker =
            notification_text(NotifyEvent::QuestionRequired, Some("busymail"), Some("demo-route"));
        assert_eq!(worker.osc9_line(), "busymail - worker demo-route - needs your answer");

        let lead = notification_text(NotifyEvent::QuestionRequired, Some("busymail"), None);
        assert_eq!(lead.osc9_line(), "busymail - lead - needs your answer");
    }

    #[test]
    fn an_unresolved_project_does_not_render_as_the_app_name() {
        let text = notification_text(NotifyEvent::TurnComplete, None, None);

        assert_eq!(
            text.osc9_line(),
            "unknown project - lead - turn complete",
            "an unstamped bucket must read as missing, not as the app name",
        );
    }

    fn seed_bucket(app: &mut App, id: &str, project: &str) -> forge_workspace::SessionKey {
        let key = forge_workspace::SessionKey::from_str_for_test(id);
        let mut bucket = UiSession::new(key.clone());
        bucket.project = Some(project.to_owned());
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
            NotifyContext {
                project: Some("beta".to_owned()),
                worker_label: Some("egen-lead".to_owned()),
            },
            "the event session's project + worker label, never the active tab's",
        );
        assert_eq!(
            app.notification_context(&active),
            NotifyContext { project: Some("alpha".to_owned()), worker_label: None },
            "a lead session resolves no worker label",
        );
    }

    #[test]
    fn notification_context_is_empty_for_an_unknown_session() {
        let app = App::test_default();
        let unknown = forge_workspace::SessionKey::from_session_id("no-such-session");
        assert_eq!(app.notification_context(&unknown), NotifyContext::default());
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
            .filter_map(|delivered| delivered.osc9_line)
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
            .filter_map(|delivered| delivered.osc9_line)
            .collect();
        assert_eq!(
            lines,
            vec![
                "busymail - worker demo-route - needs input",
                "busymail - worker demo-route - needs your answer",
            ],
        );
    }

    /// With the escape unavailable the Iterm2 channel falls back to the
    /// bell alone; OSC 9 mode Off pins that regardless of the test
    /// process's environment.
    #[test]
    fn unfocused_terminal_delivers_only_the_bell_when_osc9_is_unavailable() {
        let mut app = App::test_default();
        let key = seed_bucket(&mut app, "session-a", "companies");
        app.notifications = NotificationManager::new(Osc9NotificationMode::Off);
        app.notifications.on_focus_lost();

        app.notify(NotifyEvent::TurnComplete, &key);

        assert_eq!(
            app.notifications.take_delivered(),
            vec![DeliveredNotification { osc9_line: None, bell: true }],
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
