//! Asks a person cannot see: one OS notification per arriving ask, and the
//! seat a click on it opens.
//!
//! **The bridge decides which asks to raise** (`crate::asks`), because a
//! parked page runs no timer; this half is the platform's delivery - the
//! plugin on the desktop and the phone, a channel and the runtime permission
//! on Android, and the click that brings the window forward and names the
//! seat back to the page.
//!
//! **Every plugin call runs on one worker thread.** On Android and iOS each
//! notification call is a blocking `run_mobile_plugin` whose answer the main
//! thread has to pump, so calling one on the main thread deadlocks the
//! client; the app's own focus and life-cycle events arrive on that thread,
//! which is why they post a job here rather than calling the plugin.

use std::sync::OnceLock;
use std::sync::mpsc;

use forge_primitives::SessionSlot;
use serde_json::Value;
use tauri::{AppHandle, Emitter as _, Manager as _};
use tauri_plugin_notification::{ActionPerformed, NotificationExt, PermissionState};

use crate::asks::AskKind;

/// The channel Android delivers these on.
const CHANNEL: &str = "asks";
/// Where the notification carries the seat its click opens.
const SEAT_EXTRA: &str = "seat";
/// The action a click on the notification itself reports.
const TAP: &str = "tap";

/// One piece of work for the notification thread.
enum Job {
    /// Register the channel and the click listener.
    Init,
    /// One ask the bridge decided is news.
    Raise(SessionSlot, AskKind),
    /// The person may have come back to the app: ask a held prompt, or show
    /// what waited for one.
    Review,
}

/// The one notification thread, started by the first job.
static JOBS: OnceLock<mpsc::Sender<Job>> = OnceLock::new();

/// The seat an action was performed on, `None` for one this app did not post.
fn seat_of(action: &ActionPerformed) -> Option<Value> {
    action.notification()?.extra().get(SEAT_EXTRA).cloned()
}

/// The title one seat's ask carries: `forge · project[ · label]`.
fn title(seat: &SessionSlot) -> String {
    if seat.label() == "lead" {
        format!("forge \u{b7} {}", seat.project())
    } else {
        format!("forge \u{b7} {} \u{b7} {}", seat.project(), seat.label())
    }
}

/// What to do about one ask, given the permission and whether the person is
/// looking.
///
/// **A `Denied` reading is not a refusal.** Android derives the state from
/// `areNotificationsEnabled()`, which reads false until the runtime
/// permission is granted, so before the first request every ask sees
/// `Denied`; treating that as an answer would leave the permission
/// unreachable for the process's life. Only the request's own `Denied`
/// answer latches, and a background ask is held until the person looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Show,
    Ask,
    Hold,
    Nothing,
}

fn step(state: PermissionState, refused: bool, looking: bool) -> Step {
    if refused {
        return Step::Nothing;
    }
    match state {
        PermissionState::Granted => Step::Show,
        PermissionState::Denied
        | PermissionState::Prompt
        | PermissionState::PromptWithRationale
            if looking =>
        {
            Step::Ask
        }
        PermissionState::Denied
        | PermissionState::Prompt
        | PermissionState::PromptWithRationale => Step::Hold,
    }
}

/// Whether a permission prompt can reach the person right now. **Only
/// Android has an unasked state**, so everywhere else the reading is already
/// `Granted` and this never decides anything.
fn looking() -> bool {
    #[cfg(target_os = "android")]
    {
        crate::asks::foreground()
    }
    #[cfg(not(target_os = "android"))]
    {
        true
    }
}

/// The thread's own state: what was refused, and the asks waiting on a
/// permission that has not been answered.
struct Worker {
    app: AppHandle,
    /// Set only by an explicit denial to a request this thread made.
    refused: bool,
    /// Asks that would have been shown but for the permission.
    pending: Vec<(SessionSlot, AskKind)>,
    /// Whether anything has been shown yet.
    showed: bool,
}

impl Worker {
    fn run(&mut self, job: Job) {
        match job {
            Job::Init => self.init(),
            Job::Raise(seat, kind) => self.raise(seat, kind),
            Job::Review => self.review(),
        }
    }

    /// Register the channel and the click that opens the seat it was posted
    /// for.
    ///
    /// **A registration that fails does not stop the client**: a channel or
    /// a listener this platform refuses costs notifications, which is not
    /// worth a client that will not boot.
    fn init(&mut self) {
        let notifications = self.app.notification();
        #[cfg(target_os = "android")]
        {
            let channel = tauri_plugin_notification::Channel::builder(CHANNEL, "Asks")
                .description("an agent waiting on you")
                .build();
            if let Err(why) = notifications.create_channel(channel) {
                tauri_plugin_log::log::warn!(
                    "the asks notification channel was not created: {why}"
                );
            }
        }
        let handle = self.app.clone();
        let registered = notifications.on_action(move |action| {
            if action.action_id() != TAP {
                return;
            }
            let Some(seat) = seat_of(action) else {
                return;
            };
            if let Some(window) = handle.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
            // The page owns the route, so the seat crosses as the event's own
            // payload rather than being opened here.
            let _ = handle.emit("client://notification", seat);
        });
        if let Err(why) = registered {
            tauri_plugin_log::log::warn!(
                "the notification click listener was not registered: {why}"
            );
        }
    }

    fn raise(&mut self, seat: SessionSlot, kind: AskKind) {
        if self.refused {
            return;
        }
        let notifications = self.app.notification();
        let state = notifications.permission_state().unwrap_or(PermissionState::Granted);
        match step(state, self.refused, looking()) {
            Step::Show => self.show(&seat, kind),
            Step::Ask => {
                self.hold(seat, kind);
                self.ask();
            }
            Step::Hold => self.hold(seat, kind),
            Step::Nothing => {}
        }
    }

    /// The person may be back: drain what waited, or ask now that a prompt
    /// can reach them.
    fn review(&mut self) {
        if self.pending.is_empty() || self.refused {
            return;
        }
        let notifications = self.app.notification();
        let state = notifications.permission_state().unwrap_or(PermissionState::Granted);
        if state == PermissionState::Granted {
            self.drain();
        } else if looking() {
            self.ask();
        }
    }

    /// Remember one ask until the permission can carry it. The same seat and
    /// kind while one waits is the same ask.
    fn hold(&mut self, seat: SessionSlot, kind: AskKind) {
        if self.pending.iter().any(|(held, held_kind)| held == &seat && *held_kind == kind) {
            return;
        }
        self.pending.push((seat, kind));
    }

    /// Ask for the permission, and act only on the answer.
    fn ask(&mut self) {
        let notifications = self.app.notification();
        match notifications.request_permission() {
            Ok(PermissionState::Granted) => self.drain(),
            // **The person answered no**: nothing more is asked this run.
            Ok(PermissionState::Denied) => {
                self.refused = true;
                self.pending.clear();
            }
            // A prompt that settled on nothing is no answer, so nothing
            // latches and the next review may ask again.
            Ok(_) => {}
            Err(why) => {
                tauri_plugin_log::log::warn!("the notification permission was not asked: {why}")
            }
        }
    }

    /// Show everything a granted permission was holding.
    fn drain(&mut self) {
        for (seat, kind) in std::mem::take(&mut self.pending) {
            self.show(&seat, kind);
        }
    }

    fn show(&mut self, seat: &SessionSlot, kind: AskKind) {
        if let Err(why) = self.show_now(seat, kind) {
            // **A notification is not worth a silent failure**: the log names
            // the seat so a missing banner is answerable after the fact.
            tauri_plugin_log::log::warn!(
                "a notification for {} could not be raised: {why}",
                seat.display(),
            );
            return;
        }
        if !self.showed {
            self.showed = true;
            // **The first show is what raises the OS prompt on macOS**, and
            // the system drops that notification while it asks. The line
            // names it so a missing first notification reads as the prompt it
            // is.
            tauri_plugin_log::log::info!(
                "the first notification was shown; the OS prompt it raises can drop it",
            );
        }
    }

    fn show_now(&self, seat: &SessionSlot, kind: AskKind) -> Result<(), String> {
        let held = serde_json::to_value(seat).map_err(|why| why.to_string())?;
        self.app
            .notification()
            .builder()
            .title(title(seat))
            .body(kind.body())
            .channel_id(CHANNEL)
            .extra(SEAT_EXTRA, held)
            .show()
            .map_err(|why| why.to_string())
    }
}

/// The notification thread's sender, started on the first job.
fn jobs(app: &AppHandle) -> &'static mpsc::Sender<Job> {
    JOBS.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let app = app.clone();
        let spawned =
            std::thread::Builder::new().name("forge-notify".to_owned()).spawn(move || {
                let mut worker = Worker { app, refused: false, pending: Vec::new(), showed: false };
                for job in rx {
                    worker.run(job);
                }
            });
        if let Err(why) = spawned {
            // No thread means no notifications; the client itself is fine.
            tauri_plugin_log::log::warn!("the notification thread was not started: {why}");
        }
        tx
    })
}

/// Register the channel and the click, off the main thread.
pub fn init(app: &AppHandle) {
    let _ = jobs(app).send(Job::Init);
}

/// Raise one ask's notification on the seat it belongs to.
pub(crate) fn raise(app: &AppHandle, seat: &SessionSlot, kind: AskKind) {
    let _ = jobs(app).send(Job::Raise(seat.clone(), kind));
}

/// The person's attention moved: a held ask may be shown now.
pub fn review(app: &AppHandle) {
    let _ = jobs(app).send(Job::Review);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **An unasked permission reads `Denied` on Android**, and the request
    /// is the only thing that can tell it from a refusal - so it must ask.
    #[test]
    fn an_unasked_denial_is_a_request_not_a_silence() {
        assert_eq!(step(PermissionState::Denied, false, true), Step::Ask);
    }

    /// **A refusal the person actually gave is silence**, and is never asked
    /// again this run.
    #[test]
    fn a_refused_permission_is_silence() {
        assert_eq!(step(PermissionState::Denied, true, true), Step::Nothing);
    }

    /// **A background ask is held, not prompted**: Android cannot show the
    /// dialog behind another app, and an answer from there was nobody's.
    #[test]
    fn a_background_ask_waits_for_the_person() {
        assert_eq!(step(PermissionState::Denied, false, false), Step::Hold);
        assert_eq!(step(PermissionState::Prompt, false, false), Step::Hold);
    }

    /// A granted permission shows the ask wherever the person is.
    #[test]
    fn a_granted_permission_shows_the_ask() {
        assert_eq!(step(PermissionState::Granted, false, false), Step::Show);
        assert_eq!(step(PermissionState::Granted, true, true), Step::Nothing);
    }

    /// **A click reaches the seat the notification was posted for.** The
    /// plugin hands the extra back on the action; a renamed key is a click
    /// that focuses the window and opens nothing.
    #[test]
    fn a_click_carries_the_seat_the_notification_was_posted_for() {
        let action: ActionPerformed = serde_json::from_value(serde_json::json!({
            "actionId": "tap",
            "notification": {
                "id": 4,
                "extra": {
                    "seat": { "org": "Busytools", "project": "forge", "label": "lead" }
                }
            }
        }))
        .expect("an action the plugin can send");
        assert_eq!(
            seat_of(&action),
            Some(serde_json::json!({
                "org": "Busytools", "project": "forge", "label": "lead"
            })),
            "the seat rides the notification's own extra",
        );
    }

    /// An action on a notification this app did not post opens nothing.
    #[test]
    fn an_action_without_a_seat_is_nobody_s() {
        let action: ActionPerformed =
            serde_json::from_value(serde_json::json!({ "actionId": "tap" }))
                .expect("an action with no notification is still an action");
        assert_eq!(seat_of(&action), None);
    }

    /// The title names the seat the way a page title does: a lead is the
    /// project, a worker is `project · label`.
    #[test]
    fn the_title_names_the_seat() {
        assert_eq!(title(&SessionSlot::lead("Busytools", "forge")), "forge \u{b7} forge");
        assert_eq!(
            title(&SessionSlot::worker("Busytools", "forge", "w1")),
            "forge \u{b7} forge \u{b7} w1",
        );
    }
}
