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
/// and the detail line (event phrase + worker label).
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

/// Central notification manager.
///
/// Tracks whether the terminal window is focused (via crossterm
/// `FocusGained`/`FocusLost` events backed by DECSET 1004) and dispatches
/// notifications only when the window is **not** focused.
///
/// Three notification layers exist; the channel decides which run:
/// 1. **Terminal bell** (`BEL \x07`) -- causes a taskbar flash / dock bounce
///    on virtually every terminal emulator.
/// 2. **Desktop notification** via `notify-rust` -- OS-native toast popup
///    (Windows Toast, macOS Notification Center, Linux freedesktop D-Bus).
///    Spawned on a background thread so it never blocks the TUI event loop.
/// 3. **OSC 9 escape** -- while the terminal is believed to support it,
///    channels that can emit it suppress the desktop notification (and the
///    bell too, except on `iterm2_with_bell`), so a multiplexer that strips
///    the escape silently leaves nothing. The `[ui] notifications_osc9`
///    forge.toml key forces that belief off: the Iterm2 channel regains
///    bell + desktop, Ghostty keeps desktop only.
#[derive(Debug)]
pub struct NotificationManager {
    terminal_focused: bool,
    osc9_mode: Osc9NotificationMode,
}

impl Default for NotificationManager {
    fn default() -> Self {
        Self::new(Osc9NotificationMode::default())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationPlan {
    ring_bell: bool,
    send_desktop: bool,
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
        Self { terminal_focused: true, osc9_mode }
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
    pub fn notify(
        &self,
        channel: PreferredNotifChannel,
        event: NotifyEvent,
        context: &NotifyContext,
    ) {
        if self.terminal_focused {
            return;
        }
        let text =
            notification_text(event, context.project.as_deref(), context.worker_label.as_deref());
        let plan =
            notification_plan(channel, detect_terminal_capabilities(), self.osc9_mode, &text);
        if let Some(line) = plan.osc9_text {
            send_osc9_notification(&line);
        }
        if plan.ring_bell {
            ring_bell();
        }
        if plan.send_desktop {
            send_desktop_notification(text.title, text.detail);
        }
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
        let context = self.notification_context(session_key);
        #[cfg(feature = "testing")]
        self.test_notifications.borrow_mut().push((event, context.clone()));
        self.notifications.notify(
            self.config.preferred_notification_channel_effective(),
            event,
            &context,
        );
    }

    /// The event session's project name + worker label, read at notify
    /// time. Falls back to defaults when the bucket is gone or the
    /// session is not a live worker.
    fn notification_context(&self, session_key: &SessionKey) -> NotifyContext {
        NotifyContext {
            project: self.sessions.get(session_key).and_then(|bucket| bucket.project.clone()),
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
    let _ = std::io::stdout().write_all(b"\x07");
    let _ = std::io::stdout().flush();
}

/// Spawn a background thread that sends an OS-native desktop notification.
///
/// Runs on `std::thread::spawn` rather than tokio because `notify-rust`'s
/// `show()` may block on a D-Bus round-trip (Linux) or COM call (Windows).
/// Failures are logged at debug; the terminal bell is the reliable fallback.
fn send_desktop_notification(title: String, body: String) {
    std::thread::spawn(move || {
        if let Err(error) = notify_rust::Notification::new().summary(&title).body(&body).show() {
            tracing::debug!(
                target: crate::logging::targets::APP_LIFECYCLE,
                error = %error,
                title,
                "desktop notification failed",
            );
        }
    });
}

fn send_osc9_notification(message: &str) {
    use std::io::Write;

    let sequence = osc9_escape_sequence(message);
    let _ = std::io::stdout().write_all(sequence.as_bytes());
    let _ = std::io::stdout().flush();
}

fn notification_plan(
    channel: PreferredNotifChannel,
    capabilities: TerminalCapabilities,
    osc9_mode: Osc9NotificationMode,
    text: &NotificationText,
) -> NotificationPlan {
    let osc9_available = match osc9_mode {
        Osc9NotificationMode::Auto => capabilities.osc9_notifications,
        Osc9NotificationMode::Off => false,
    };
    let osc9_text = osc9_available.then(|| text.osc9_line());
    match channel {
        PreferredNotifChannel::NotificationsDisabled => {
            NotificationPlan { ring_bell: false, send_desktop: false, osc9_text: None }
        }
        PreferredNotifChannel::TerminalBell => {
            NotificationPlan { ring_bell: true, send_desktop: false, osc9_text: None }
        }
        // "Auto / iTerm2" replaced the original always-bell-plus-desktop behavior.
        // Preserve that reliable fallback whenever OSC 9 is unavailable.
        PreferredNotifChannel::Iterm2 => NotificationPlan {
            ring_bell: osc9_text.is_none(),
            send_desktop: osc9_text.is_none(),
            osc9_text,
        },
        PreferredNotifChannel::Ghostty => {
            NotificationPlan { ring_bell: false, send_desktop: osc9_text.is_none(), osc9_text }
        }
        PreferredNotifChannel::Iterm2WithBell => {
            NotificationPlan { ring_bell: true, send_desktop: osc9_text.is_none(), osc9_text }
        }
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
    let mut iterm_session = false;

    for (key, value) in vars {
        match key.as_str() {
            "TERM_PROGRAM" => term_program = Some(value),
            "ITERM_SESSION_ID" if !value.is_empty() => iterm_session = true,
            _ => {}
        }
    }

    let osc9_notifications =
        matches!(term_program.as_deref(), Some("iTerm.app" | "ghostty")) || iterm_session;
    TerminalCapabilities { osc9_notifications }
}

/// The title the toast carries when the session has no project yet.
const APP_NAME: &str = "forge";

/// Build the delivered strings for one event from the session's
/// project + worker label. The title stays short (the project); the
/// detail names the event, and the worker when the session is one.
fn notification_text(
    event: NotifyEvent,
    project: Option<&str>,
    worker_label: Option<&str>,
) -> NotificationText {
    let title = project.unwrap_or(APP_NAME).to_owned();
    let detail = match (event, worker_label) {
        (NotifyEvent::TurnComplete, _) => "turn complete".to_owned(),
        (NotifyEvent::PermissionRequired, Some(label)) => format!("worker {label} needs input"),
        (NotifyEvent::PermissionRequired, None) => "permission needs your approval".to_owned(),
        (NotifyEvent::QuestionRequired, Some(label)) => {
            format!("worker {label} needs your answer")
        }
        (NotifyEvent::QuestionRequired, None) => "question needs your answer".to_owned(),
    };
    NotificationText { title, detail }
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
        NotificationText { title: "companies".to_owned(), detail: "turn complete".to_owned() }
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
            NotificationPlan { ring_bell: false, send_desktop: false, osc9_text: None }
        );
    }

    #[test]
    fn terminal_bell_plan_skips_desktop_notification() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::TerminalBell,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: true, send_desktop: false, osc9_text: None }
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
                send_desktop: false,
                osc9_text: Some("companies - turn complete".to_owned()),
            }
        );
    }

    #[test]
    fn iterm2_auto_preserves_bell_and_desktop_fallback_when_osc9_is_unavailable() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2,
                TerminalCapabilities { osc9_notifications: false },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: true, send_desktop: true, osc9_text: None }
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
                send_desktop: false,
                osc9_text: Some("companies - turn complete".to_owned()),
            }
        );
    }

    #[test]
    fn iterm2_with_bell_falls_back_to_desktop_and_bell() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2WithBell,
                TerminalCapabilities { osc9_notifications: false },
                Osc9NotificationMode::Auto,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: true, send_desktop: true, osc9_text: None }
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
                send_desktop: false,
                osc9_text: Some("companies - turn complete".to_owned()),
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
    fn unsupported_term_does_not_advertise_osc9() {
        let capabilities =
            terminal_capabilities_from_env([("TERM_PROGRAM".to_owned(), "wezterm".to_owned())]);

        assert!(!capabilities.osc9_notifications);
    }

    #[test]
    fn osc9_override_off_forces_iterm2_to_bell_and_desktop() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Iterm2,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Off,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: true, send_desktop: true, osc9_text: None }
        );
    }

    #[test]
    fn osc9_override_off_leaves_ghostty_with_desktop_only() {
        assert_eq!(
            notification_plan(
                PreferredNotifChannel::Ghostty,
                TerminalCapabilities { osc9_notifications: true },
                Osc9NotificationMode::Off,
                &fixture_text(),
            ),
            NotificationPlan { ring_bell: false, send_desktop: true, osc9_text: None }
        );
    }

    #[test]
    fn turn_complete_text_carries_the_project() {
        let text = notification_text(NotifyEvent::TurnComplete, Some("companies"), None);
        assert_eq!(text.title, "companies");
        assert_eq!(text.detail, "turn complete");
        assert_eq!(text.osc9_line(), "companies - turn complete");
    }

    #[test]
    fn permission_text_varies_by_worker_label() {
        let worker =
            notification_text(NotifyEvent::PermissionRequired, Some("forge"), Some("egen-lead"));
        assert_eq!(worker.title, "forge");
        assert_eq!(worker.detail, "worker egen-lead needs input");

        let lead = notification_text(NotifyEvent::PermissionRequired, Some("forge"), None);
        assert_eq!(lead.detail, "permission needs your approval");
    }

    #[test]
    fn question_text_varies_by_worker_label() {
        let worker =
            notification_text(NotifyEvent::QuestionRequired, Some("forge"), Some("egen-lead"));
        assert_eq!(worker.detail, "worker egen-lead needs your answer");

        let lead = notification_text(NotifyEvent::QuestionRequired, Some("forge"), None);
        assert_eq!(lead.detail, "question needs your answer");
    }

    #[test]
    fn text_falls_back_to_the_app_name_without_a_project() {
        let text = notification_text(NotifyEvent::TurnComplete, None, None);
        assert_eq!(text.title, "forge");
        assert_eq!(text.detail, "turn complete");
        assert_eq!(text.osc9_line(), "forge - turn complete");
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
    fn notification_context_falls_back_without_bucket_or_worker() {
        let app = App::test_default();
        let unknown = forge_workspace::SessionKey::from_session_id("no-such-session");
        assert_eq!(app.notification_context(&unknown), NotifyContext::default());
    }

    #[test]
    fn osc9_sequence_uses_st_terminator_and_sanitizes_message() {
        assert_eq!(
            osc9_escape_sequence("hello\n\u{1b}world\u{07}").as_ref(),
            "\u{1b}]9;hello world\u{1b}\\"
        );
    }
}
