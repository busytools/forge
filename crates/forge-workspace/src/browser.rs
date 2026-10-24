//! The browser the sessions drive: the port the tools ask through, and the
//! hand-offs that park on the person.
//!
//! **A port, not a dependency.** forge-server owns the browser fleet - the
//! CEF hosts, the drivers and the profiles - and implements [`BrowserHost`],
//! so a session's call reaches a browser without this crate knowing where
//! that browser lives. A workspace with no fleet installed is a test or a
//! view; its browser tools answer [`NO_BROWSER_HOST`].

use forge_primitives::SessionSlot;
use forge_primitives::browser::{HandOff, HandOffEnding};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::workspace::Workspace;

/// A viewer's handle on one profile, as the transport keeps it.
pub struct BrowserAttachment {
    /// The profile's key, for the calls that follow (input, control, nav).
    pub key: String,
    /// The id the binary frames carry.
    pub id: u16,
    /// The newest frame, and a way to follow it.
    pub frames: tokio::sync::watch::Receiver<Option<forge_primitives::browser::BrowserFrame>>,
    /// The controller and the viewer count, as they change.
    pub view: tokio::sync::watch::Receiver<forge_primitives::browser::BrowserViewState>,
    /// Where the page's history sits, as it moves - the bar's arrows read
    /// this rather than asking after every click.
    pub nav: tokio::sync::watch::Receiver<Option<forge_primitives::browser::BrowserNavState>>,
    /// Why the profile's browser stopped, when it did.
    pub failures: tokio::sync::watch::Receiver<Option<String>>,
    /// A page's file chooser, while a person has to answer one.
    pub files: tokio::sync::watch::Receiver<Option<forge_primitives::browser::BrowserFileAsk>>,
    /// Finished downloads, each once, for the viewer that takes it.
    pub downloads: tokio::sync::broadcast::Receiver<forge_primitives::browser::BrowserDownload>,
    /// The page's own audio, streamed while it plays.
    pub audio: tokio::sync::broadcast::Receiver<forge_primitives::browser::BrowserAudio>,
}

/// The server's browser fleet, as the workspace calls it.
#[async_trait::async_trait]
pub trait BrowserHost: Send + Sync {
    /// Run one browser tool through the profile the call names.
    async fn call(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<forge_primitives::browser::BrowserPart>, String>;

    /// Attach a viewer: the profile's newest frame and its viewer state.
    async fn attach(
        &self,
        viewer: u64,
        seat: &SessionSlot,
        profile: Option<&str>,
    ) -> Result<BrowserAttachment, String>;

    /// A viewer's connection ended.
    async fn detach(&self, viewer: u64);

    /// One input event, only from the viewer that holds the role.
    async fn input(
        &self,
        profile: &str,
        viewer: u64,
        seq: u64,
        event: &forge_primitives::browser::BrowserInputEvent,
    ) -> Result<(), String>;

    /// Answer a page's file chooser: the files a person chose, or none.
    async fn answer_files(
        &self,
        profile: &str,
        id: u64,
        files: Vec<forge_primitives::browser::BrowserUpload>,
    ) -> Result<(), String>;

    /// Give `viewer` the controller role.
    async fn take_control(&self, profile: &str, viewer: u64) -> Result<(), String>;

    /// Lay the page out to the controller's surface: device pixels and the
    /// ratio they draw at. **Only the controller's size applies**; a
    /// watcher's window does not move the page under the person driving it.
    async fn resize(&self, profile: &str, viewer: u64, width: u32, height: u32, scale: f64);

    /// Release the role, when this viewer still holds it.
    async fn release_control(&self, profile: &str, viewer: u64);

    /// Move through the page's history, answering where it now sits. **Only
    /// the controller moves the page**: a watcher's move would move it under
    /// the person driving it.
    async fn nav(
        &self,
        profile: &str,
        viewer: u64,
        back: bool,
    ) -> Result<forge_primitives::browser::BrowserNavState, String>;

    /// Where the page's history sits, when the browser can answer.
    async fn nav_state(&self, profile: &str) -> Option<forge_primitives::browser::BrowserNavState>;

    /// The profiles this seat may see, for a client's strip: its own named
    /// profiles, the ownerless ones, and the shared browser.
    async fn profiles(
        &self,
        seat: &SessionSlot,
    ) -> Result<Vec<forge_primitives::browser::BrowserProfileRow>, String>;

    /// Close a named profile's browser, keeping its logins. The seat must own
    /// the name; the shared browser is not a profile to close.
    async fn close_profile(&self, seat: &SessionSlot, name: &str) -> Result<(), String>;

    /// Delete a named profile and its logins. The seat must own the name.
    async fn delete_profile(&self, seat: &SessionSlot, name: &str) -> Result<(), String>;

    /// The seats that remain: a named profile whose seat is gone is ended and
    /// its directory removed. Called when a seat is torn down.
    async fn sweep_seats(&self, seats: &[SessionSlot]);

    /// End every open browser and its driver, keeping the profiles on disk.
    /// **The hosts are the server's children**, so a clean stop ends them; a
    /// profile's logins stay, and the next boot opens its carried page.
    /// Answers the profiles whose host outlived the teardown, named, so a
    /// stop that leaves a process behind cannot report success.
    async fn stop(&self) -> Vec<String>;
}

/// The error a browser tool answers with when the workspace has no fleet.
pub const NO_BROWSER_HOST: &str = "this server has no browser host installed";

impl Workspace {
    /// Hold a browser hand-off for the person at a client, and hand back its
    /// id plus the receiver the caller awaits.
    ///
    /// **No timeout rides the wait.** The ask dock's own timelines say a held
    /// prompt sits for hours, so nothing here ends it but an answer or the
    /// caller's own drop. The one case answered up front is a stream nobody
    /// can answer on: holding a session on a prompt no view can draw would
    /// park it forever, which is the failure this path exists to refuse.
    pub(crate) fn register_browser_hand_off(
        &self,
        caller: &SessionSlot,
        handoff: HandOff,
    ) -> (uuid::Uuid, oneshot::Receiver<HandOffEnding>) {
        let (sender, receiver) = oneshot::channel();
        let id = handoff.id;
        self.browser_handoffs.lock().insert(id, (caller.clone(), handoff.clone(), sender));
        let answerable = self.update_sender().send_answering(
            crate::protocol::SessionUpdate::BrowserHandOffPending { key: caller.clone(), handoff },
        );
        if !answerable {
            self.browser_handoffs.lock().remove(&id);
            let (_ignored_sender, dead_receiver) = oneshot::channel();
            return (id, dead_receiver);
        }
        (id, receiver)
    }

    /// Whether a hand-off with this id is still registered to `caller`,
    /// without removing it - the read the dispatch guard makes before an
    /// answer.
    pub(crate) fn browser_handoff_waiting(&self, id: uuid::Uuid, caller: &SessionSlot) -> bool {
        self.browser_handoffs.lock().get(&id).is_some_and(|(owner, _, _)| owner == caller)
    }

    /// Whether a parked hand-off holds this profile: one that named it, or -
    /// a hand-off that named none - any browser. **Keyed to the profile and
    /// never to the seat**: the shared browser's last caller can be any seat,
    /// and a hand-off parked by one of them must hold the profile against
    /// every seat's close. The read the idle warning makes before anything is
    /// delivered or closed.
    pub(crate) fn browser_handoff_holds(&self, profile: &str) -> bool {
        self.browser_handoffs
            .lock()
            .values()
            .any(|(_, handoff, _)| handoff.profile.as_deref().is_none_or(|named| named == profile))
    }

    /// Remove a parked hand-off, answer its waiter, and tell every view it is
    /// gone. `false` when the id is not this caller's to resolve.
    pub(crate) fn resolve_browser_hand_off(
        &self,
        id: uuid::Uuid,
        caller: &SessionSlot,
        ending: HandOffEnding,
    ) -> bool {
        let mut parked = self.browser_handoffs.lock();
        if !parked.get(&id).is_some_and(|(owner, _, _)| owner == caller) {
            return false;
        }
        let Some((owner, _, sender)) = parked.remove(&id) else { return false };
        drop(parked);
        // **The ending is what tells an answer from an abandonment**, and
        // hand-offs were seen resolving by themselves in a live round
        // (2026-10-07) with nothing naming the hand that sent it.
        tracing::debug!(
            event_name = "browser_hand_off_resolved",
            id = %id,
            ending = ?ending,
            "a parked browser hand-off left the registry"
        );
        let _ = sender.send(ending);
        let _ = self.update_sender().send(crate::protocol::SessionUpdate::BrowserHandOffResolved {
            key: owner,
            id,
            ending,
        });
        true
    }
}

/// A fleet for the crate's own tests: calls land on a channel, and the
/// viewer half answers refusals nothing asks for.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use forge_primitives::browser::{BrowserInputEvent, BrowserNavState, BrowserPart};
    use tokio::sync::mpsc;

    pub(crate) struct StubHost {
        pub(crate) calls: mpsc::UnboundedSender<(String, Value)>,
    }

    #[async_trait::async_trait]
    impl BrowserHost for StubHost {
        async fn call(
            &self,
            _seat: &SessionSlot,
            tool: &str,
            args: Value,
        ) -> Result<Vec<BrowserPart>, String> {
            let _ = self.calls.send((tool.to_owned(), args));
            Ok(vec![BrowserPart::Text { text: "clicked".to_owned() }])
        }

        async fn attach(
            &self,
            _viewer: u64,
            _seat: &SessionSlot,
            _profile: Option<&str>,
        ) -> Result<BrowserAttachment, String> {
            Err("no viewers here".to_owned())
        }

        async fn detach(&self, _viewer: u64) {}

        async fn input(
            &self,
            _profile: &str,
            _viewer: u64,
            _seq: u64,
            _event: &BrowserInputEvent,
        ) -> Result<(), String> {
            Err("no viewers here".to_owned())
        }

        async fn answer_files(
            &self,
            _profile: &str,
            _id: u64,
            _files: Vec<forge_primitives::browser::BrowserUpload>,
        ) -> Result<(), String> {
            Err("no viewers here".to_owned())
        }

        async fn take_control(&self, _profile: &str, _viewer: u64) -> Result<(), String> {
            Err("no viewers here".to_owned())
        }

        async fn resize(
            &self,
            _profile: &str,
            _viewer: u64,
            _width: u32,
            _height: u32,
            _scale: f64,
        ) {
        }

        async fn release_control(&self, _profile: &str, _viewer: u64) {}

        async fn nav(
            &self,
            _profile: &str,
            _viewer: u64,
            _back: bool,
        ) -> Result<BrowserNavState, String> {
            Err("no viewers here".to_owned())
        }

        async fn nav_state(&self, _profile: &str) -> Option<BrowserNavState> {
            None
        }

        async fn profiles(
            &self,
            _seat: &SessionSlot,
        ) -> Result<Vec<forge_primitives::browser::BrowserProfileRow>, String> {
            Err("no viewers here".to_owned())
        }

        async fn close_profile(&self, _seat: &SessionSlot, _name: &str) -> Result<(), String> {
            Err("no viewers here".to_owned())
        }

        async fn delete_profile(&self, _seat: &SessionSlot, _name: &str) -> Result<(), String> {
            Err("no viewers here".to_owned())
        }

        async fn sweep_seats(&self, seats: &[SessionSlot]) {
            let _ = self.calls.send(("sweep_seats".to_owned(), serde_json::json!(seats)));
        }

        async fn stop(&self) -> Vec<String> {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handoff(reason: &str) -> HandOff {
        HandOff {
            id: uuid::Uuid::new_v4(),
            reason: reason.to_owned(),
            profile: Some("hunt".to_owned()),
        }
    }

    /// **A hand-off is addressed to the session that asked**, so the dock the
    /// person answers belongs to the session waiting on it rather than to
    /// whichever one happens to be focused.
    #[tokio::test]
    async fn a_hand_off_is_addressed_to_the_session_that_asked() {
        let (ws, mut rx) = Workspace::testing_stub();
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let (_id, _answer) = ws.register_browser_hand_off(&caller, handoff("solve the CAPTCHA"));

        let mut addressed = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::BrowserHandOffPending { key, .. } = frame.update
            {
                addressed.push(key);
            }
        }
        assert_eq!(addressed, vec![caller], "the hand-off is addressed to the asker");
    }

    /// Answering a hand-off nobody parked is refused rather than answered
    /// into the void.
    #[tokio::test]
    async fn answering_a_hand_off_that_is_not_pending_is_refused() {
        let (ws, _rx) = Workspace::testing_stub();
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        assert!(
            !ws.resolve_browser_hand_off(uuid::Uuid::new_v4(), &caller, HandOffEnding::NotNow),
            "a hand-off that is not parked cannot be resolved",
        );
    }

    /// **No view can draw the dock: the caller fails closed rather than
    /// parking forever.** This is the one case a wait does not hold for, and
    /// it must not hold the session on a prompt nothing can answer.
    #[tokio::test]
    async fn a_hand_off_with_no_ui_to_answer_it_fails_closed() {
        let (ws, rx) = Workspace::testing_stub();
        drop(rx);
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let (_id, answer) = ws.register_browser_hand_off(&caller, handoff("solve the CAPTCHA"));

        assert!(
            ws.browser_handoffs.lock().is_empty(),
            "an unanswerable hand-off is dropped, not held forever",
        );
        assert!(answer.await.is_err(), "a dead receiver is the fail-closed read");
    }

    /// The resolve answers the blocked waiter AND tells the views it is gone,
    /// so a dock standing elsewhere retires with the same ending the answer
    /// carried.
    #[tokio::test]
    async fn resolving_a_hand_off_answers_its_waiter_and_tells_the_views() {
        let (ws, mut rx) = Workspace::testing_stub();
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, answer) = ws.register_browser_hand_off(&caller, handoff);
        while rx.try_recv().is_ok() {}

        assert!(ws.resolve_browser_hand_off(id, &caller, HandOffEnding::Done));

        assert_eq!(
            answer.await.expect("the waiter is answered"),
            HandOffEnding::Done,
            "the ending the resolve carried is what the blocked handler gets",
        );
        let resolved = rx.try_recv().expect("the views hear it left the registry");
        let crate::protocol::SessionUpdate::BrowserHandOffResolved { key, id: seen, ending } =
            resolved.update
        else {
            panic!("the update is the resolved one");
        };
        assert_eq!(key, caller);
        assert_eq!(seen, id);
        assert_eq!(ending, HandOffEnding::Done);
    }

    /// Only the session that asked may resolve it: an answer from another
    /// seat is refused by the owner read, not applied.
    #[tokio::test]
    async fn only_the_owner_resolves_a_hand_off() {
        let (ws, _rx) = Workspace::testing_stub();
        let owner = SessionSlot::from_str_for_test("owner-uuid");
        let other = SessionSlot::from_str_for_test("other-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, _answer) = ws.register_browser_hand_off(&owner, handoff);

        assert!(
            !ws.resolve_browser_hand_off(id, &other, HandOffEnding::Done),
            "another seat's answer is refused",
        );
        assert!(ws.browser_handoff_waiting(id, &owner), "and the hand-off stays pending");
    }
}
