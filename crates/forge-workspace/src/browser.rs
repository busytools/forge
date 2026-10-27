//! The browser the sessions drive: the port the tools ask through, and the
//! hand-offs that park on the person.
//!
//! **A port, not a dependency.** forge-server owns the browser fleet - the
//! CEF hosts, the drivers and the profiles - and implements [`BrowserHost`],
//! so a session's call reaches a browser without this crate knowing where
//! that browser lives. A workspace with no fleet installed is a test or a
//! view; its browser tools answer [`NO_BROWSER_HOST`].

use forge_primitives::SessionSlot;
use forge_primitives::browser::{HandOff, HandOffAnswer, HandOffEnding};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::workspace::Workspace;

/// A viewer's handle on one tab, as the transport keeps it.
pub struct BrowserAttachment {
    /// The profile's key, for the calls that follow (input, control, nav).
    pub key: String,
    /// The id the binary frames carry.
    pub id: u16,
    /// The tab this viewer is on, for every call that follows.
    pub tab: u16,
    /// The newest frame, and a way to follow it.
    pub frames: tokio::sync::watch::Receiver<Option<forge_primitives::browser::BrowserFrame>>,
    /// The tab's controller and the viewer count, as they change.
    pub view: tokio::sync::watch::Receiver<forge_primitives::browser::BrowserViewState>,
    /// The profile's tabs, as they change.
    pub tabs: tokio::sync::watch::Receiver<forge_primitives::browser::BrowserTabList>,
    /// Where the tab's history sits, as it moves - the bar's arrows read
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

/// A live-seat snapshot and the generation its capture took.
///
/// [`Workspace::live_seats`] serialises captures, so the generation orders
/// whole seat sets; a sweep applies only a newer one (see [`BrowserHost`]).
pub struct LiveSeats {
    pub generation: u64,
    pub seats: Vec<SessionSlot>,
}

/// The server's browser fleet, as the workspace calls it.
#[async_trait::async_trait]
pub trait BrowserHost: Send + Sync {
    /// Run one browser tool through the tab the calling session drives.
    async fn call(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<forge_primitives::browser::BrowserPart>, String>;

    /// Attach a viewer to one of the seat's tabs. `tab` is `None` for the
    /// seat's current tab, else its profile's first.
    async fn attach(
        &self,
        viewer: u64,
        seat: &SessionSlot,
        tab: Option<u16>,
    ) -> Result<BrowserAttachment, String>;

    /// A viewer's connection ended.
    async fn detach(&self, viewer: u64);

    /// One input event, only from the viewer that holds the role on that tab.
    async fn input(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        seq: u64,
        event: &forge_primitives::browser::BrowserInputEvent,
    ) -> Result<(), String>;

    /// Answer a page's file chooser: the files a person chose, or none.
    async fn answer_files(
        &self,
        profile: &str,
        tab: u16,
        id: u64,
        files: Vec<forge_primitives::browser::BrowserUpload>,
    ) -> Result<(), String>;

    /// Give `viewer` the controller role on one tab.
    async fn take_control(&self, profile: &str, tab: u16, viewer: u64) -> Result<(), String>;

    /// Lay one tab out to the controller's surface: device pixels and the
    /// ratio they draw at. **Only the controller's size applies**; a
    /// watcher's window does not move the page under the person driving it.
    async fn resize(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        width: u32,
        height: u32,
        scale: f64,
    );

    /// Release the role, when this viewer still holds it on that tab.
    async fn release_control(&self, profile: &str, tab: u16, viewer: u64);

    /// Move one tab through its history, answering where it now sits. **Only
    /// the controller moves the page**: a watcher's move would move it under
    /// the person driving it.
    async fn nav(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        back: bool,
    ) -> Result<forge_primitives::browser::BrowserNavState, String>;

    /// Where one tab's history sits, when the browser can answer.
    async fn nav_state(
        &self,
        profile: &str,
        tab: u16,
    ) -> Option<forge_primitives::browser::BrowserNavState>;

    /// A seat's tabs as its strip draws them.
    async fn tabs(
        &self,
        seat: &SessionSlot,
    ) -> Result<forge_primitives::browser::BrowserTabList, String>;

    /// A stream of a seat's tab rows, for a strip that follows them.
    async fn tab_stream(
        &self,
        seat: &SessionSlot,
    ) -> Result<tokio::sync::watch::Receiver<forge_primitives::browser::BrowserTabList>, String>;

    /// The seats that remain: a tab whose session is gone is closed, except a
    /// tab a parked hand-off holds. Called when a seat is torn down.
    /// **`generation` orders captures** ([`LiveSeats`]): a sweep at or below
    /// the last applied one is dropped, so a stale snapshot cannot re-admit
    /// an ended seat.
    async fn sweep_sessions(&self, seats: &[SessionSlot], generation: u64);

    /// A `/new` or `/resume`: the slot stays and the previous occupant's tabs
    /// close, except a tab a parked hand-off holds.
    async fn replace_seat(&self, seat: &SessionSlot);

    /// **The seat has ended**, as opposed to a sweep's guess: the fleet must
    /// answer a hand-off resolution from this fact, so a closed lead's held
    /// tab does not survive Done. Synchronous, so it lands before the close
    /// work that follows it. A successor clears it by warming or calling.
    fn session_ended(&self, seat: &SessionSlot);

    /// A hand-off parked on one of the seat's tabs: the tab is held against
    /// the sweep and the idle clock until it resolves. **Refused when the tab
    /// is not open or not this seat's**, before any ask reaches a person.
    async fn hand_off_started(&self, seat: &SessionSlot, tab: u16) -> Result<(), String>;

    /// A hand-off left the registry: the tab is released, and a session that
    /// went while it was held takes its tab with it.
    async fn hand_off_finished(&self, seat: &SessionSlot, tab: u16);

    /// Create the seat's profile entry and start its host, with no tab. The
    /// first seat of a profile calls it at spawn; later seats reuse the host.
    async fn warm(&self, seat: &SessionSlot);

    /// End every open browser and its driver, keeping the profiles on disk.
    /// **The hosts are the server's children**, so a clean stop ends them; a
    /// profile's logins stay, and **no tab survives a host** - the next boot's
    /// first call opens a fresh tab. Answers the profiles whose host outlived
    /// the teardown, named, so a stop that leaves a process behind cannot
    /// report success.
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
    pub(crate) async fn register_browser_hand_off(
        &self,
        caller: &SessionSlot,
        handoff: HandOff,
    ) -> Result<(uuid::Uuid, oneshot::Receiver<HandOffEnding>), String> {
        let (sender, receiver) = oneshot::channel();
        let id = handoff.id;
        let tab = handoff.tab;
        // **The hold is the service's, and it is awaited.** A sweep that ran
        // between a detached hold and the ask would see the tab as an orphan
        // and close it under the person, and a tab that is not this seat's
        // would send the person to a page the session never opened: the
        // fleet's refusal is the tool call's refusal, before any ask exists.
        self.browser_hand_off_started(caller, tab).await?;
        self.browser_handoffs.lock().insert(id, (caller.clone(), handoff.clone(), sender));
        let answerable = self.update_sender().send_answering(
            crate::protocol::SessionUpdate::BrowserHandOffPending { key: caller.clone(), handoff },
        );
        if !answerable {
            self.browser_handoffs.lock().remove(&id);
            // **A raise no view can show has no ask to resolve**, so the hold
            // it took is given back here rather than left on the tab forever.
            self.tell_browser_hand_off_finished(caller, tab);
            let (_ignored_sender, dead_receiver) = oneshot::channel();
            return Ok((id, dead_receiver));
        }
        Ok((id, receiver))
    }

    /// Take the fleet's hold for a hand-off, awaited so its refusal is the
    /// tool call's. Answers the fleet's own sentence for a tab that is not
    /// open or not this seat's.
    async fn browser_hand_off_started(&self, caller: &SessionSlot, tab: u16) -> Result<(), String> {
        let Some(host) = self.browser_host() else {
            return Err(NO_BROWSER_HOST.to_owned());
        };
        host.hand_off_started(caller, tab).await
    }

    /// Tell the fleet a hand-off left its tab: the service acts on a tab that
    /// is still held, and a resolution for anything else touches nothing.
    pub(crate) fn tell_browser_hand_off_finished(&self, caller: &SessionSlot, tab: u16) {
        let (Some(host), Ok(runtime)) =
            (self.browser_host(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        let seat = caller.clone();
        runtime.spawn(async move { host.hand_off_finished(&seat, tab).await });
    }

    /// Whether a hand-off with this id is still registered to `caller`,
    /// without removing it - the read the dispatch guard makes before an
    /// answer.
    pub(crate) fn browser_handoff_waiting(&self, id: uuid::Uuid, caller: &SessionSlot) -> bool {
        self.browser_handoffs.lock().get(&id).is_some_and(|(owner, _, _)| owner == caller)
    }

    /// Remove a parked hand-off, answer its waiter, and tell every view it is
    /// gone. `false` when the id is not this caller's to resolve.
    pub(crate) fn resolve_browser_hand_off(
        &self,
        id: uuid::Uuid,
        caller: &SessionSlot,
        answer: HandOffAnswer,
    ) -> bool {
        let mut parked = self.browser_handoffs.lock();
        if !parked.get(&id).is_some_and(|(owner, _, _)| owner == caller) {
            return false;
        }
        let Some((owner, handoff, sender)) = parked.remove(&id) else { return false };
        drop(parked);
        // **The tab is released with the answer**, whichever ending it
        // carried: a resolved hand-off holds nothing.
        self.tell_browser_hand_off_finished(&owner, handoff.tab);
        // **Every ending names the hand that sent it**: an answer with
        // nothing attributable behind it is what two live rounds were spent
        // on (2026-10-07, 2026-10-26), so the door, the connection and the
        // seat and tab the hand-off held are all here.
        tracing::debug!(
            event_name = "browser_hand_off_resolved",
            id = %id,
            ending = ?answer.ending,
            source = ?answer.source,
            client = answer.client,
            seat = %owner.display(),
            tab = handoff.tab,
            "a parked browser hand-off left the registry"
        );
        let _ = sender.send(answer.ending);
        let _ = self.update_sender().send(crate::protocol::SessionUpdate::BrowserHandOffResolved {
            key: owner,
            id,
            answer,
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

    impl StubHost {
        fn heard(&self, what: &str, args: Value) {
            let _ = self.calls.send((what.to_owned(), args));
        }
    }

    #[async_trait::async_trait]
    impl BrowserHost for StubHost {
        async fn call(
            &self,
            _seat: &SessionSlot,
            tool: &str,
            args: Value,
        ) -> Result<Vec<BrowserPart>, String> {
            self.heard(tool, args);
            Ok(vec![BrowserPart::Text { text: "clicked".to_owned() }])
        }

        async fn attach(
            &self,
            _viewer: u64,
            _seat: &SessionSlot,
            _tab: Option<u16>,
        ) -> Result<BrowserAttachment, String> {
            Err("no viewers here".to_owned())
        }

        async fn detach(&self, _viewer: u64) {}

        async fn input(
            &self,
            _profile: &str,
            _tab: u16,
            _viewer: u64,
            _seq: u64,
            _event: &BrowserInputEvent,
        ) -> Result<(), String> {
            Err("no viewers here".to_owned())
        }

        async fn answer_files(
            &self,
            _profile: &str,
            _tab: u16,
            _id: u64,
            _files: Vec<forge_primitives::browser::BrowserUpload>,
        ) -> Result<(), String> {
            Err("no viewers here".to_owned())
        }

        async fn take_control(
            &self,
            _profile: &str,
            _tab: u16,
            _viewer: u64,
        ) -> Result<(), String> {
            Err("no viewers here".to_owned())
        }

        async fn resize(
            &self,
            _profile: &str,
            _tab: u16,
            _viewer: u64,
            _width: u32,
            _height: u32,
            _scale: f64,
        ) {
        }

        async fn release_control(&self, _profile: &str, _tab: u16, _viewer: u64) {}

        async fn nav(
            &self,
            _profile: &str,
            _tab: u16,
            _viewer: u64,
            _back: bool,
        ) -> Result<BrowserNavState, String> {
            Err("no viewers here".to_owned())
        }

        async fn nav_state(&self, _profile: &str, _tab: u16) -> Option<BrowserNavState> {
            None
        }

        async fn tabs(
            &self,
            _seat: &SessionSlot,
        ) -> Result<forge_primitives::browser::BrowserTabList, String> {
            Err("no viewers here".to_owned())
        }

        async fn tab_stream(
            &self,
            _seat: &SessionSlot,
        ) -> Result<tokio::sync::watch::Receiver<forge_primitives::browser::BrowserTabList>, String>
        {
            Err("no viewers here".to_owned())
        }

        async fn sweep_sessions(&self, seats: &[SessionSlot], generation: u64) {
            self.heard(
                "sweep_sessions",
                serde_json::json!({ "seats": seats, "generation": generation }),
            );
        }

        async fn replace_seat(&self, seat: &SessionSlot) {
            self.heard("replace_seat", serde_json::json!(seat));
        }

        fn session_ended(&self, seat: &SessionSlot) {
            self.heard("session_ended", serde_json::json!(seat));
        }

        async fn hand_off_started(&self, seat: &SessionSlot, tab: u16) -> Result<(), String> {
            self.heard("hand_off_started", serde_json::json!({ "seat": seat, "tab": tab }));
            // A host's tabs start at 1; 0 stands in for one that is not open
            // so the refusal has a caller.
            if tab == 0 {
                return Err(format!("tab {tab} is not open"));
            }
            Ok(())
        }

        async fn hand_off_finished(&self, seat: &SessionSlot, tab: u16) {
            self.heard("hand_off_finished", serde_json::json!({ "seat": seat, "tab": tab }));
        }

        async fn warm(&self, seat: &SessionSlot) {
            self.heard("warm", serde_json::json!(seat));
        }

        async fn stop(&self) -> Vec<String> {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::testing::StubHost;
    use forge_primitives::browser::HandOffSource;

    fn handoff(reason: &str) -> HandOff {
        HandOff { id: uuid::Uuid::new_v4(), reason: reason.to_owned(), tab: 2 }
    }

    /// One answer from the given door, with a connection behind it.
    fn answer(ending: HandOffEnding, source: HandOffSource) -> HandOffAnswer {
        HandOffAnswer { ending, source, client: Some(4) }
    }

    /// A workspace with a fleet installed and the calls it is told.
    fn with_host(
        ws: &std::sync::Arc<Workspace>,
    ) -> tokio::sync::mpsc::UnboundedReceiver<(String, serde_json::Value)> {
        let (calls, heard) = tokio::sync::mpsc::unbounded_channel();
        ws.install_browser_host(std::sync::Arc::new(StubHost { calls }));
        heard
    }

    /// **A hand-off is addressed to the session that asked**, so the dock the
    /// person answers belongs to the session waiting on it rather than to
    /// whichever one happens to be focused.
    #[tokio::test]
    async fn a_hand_off_is_addressed_to_the_session_that_asked() {
        let (ws, mut rx) = Workspace::testing_stub();
        let _heard = with_host(&ws);
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let (_id, _answer) = ws
            .register_browser_hand_off(&caller, handoff("solve the CAPTCHA"))
            .await
            .expect("parks");

        let mut addressed = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::BrowserHandOffPending { key, .. } = frame.update
            {
                addressed.push(key);
            }
        }
        assert_eq!(addressed, vec![caller], "the hand-off is addressed to the asker");
    }

    /// **A parked hand-off holds its tab in the fleet before the ask is
    /// parked.** A sweep that ran between the two would read the tab as an
    /// orphan and close it under the person.
    #[tokio::test]
    async fn registering_a_hand_off_holds_its_tab_in_the_fleet() {
        let (ws, _rx) = Workspace::testing_stub();
        let (calls, mut heard) = tokio::sync::mpsc::unbounded_channel();
        ws.install_browser_host(std::sync::Arc::new(StubHost { calls }));
        let caller = SessionSlot::from_str_for_test("caller-uuid");

        let (_id, _answer) = ws
            .register_browser_hand_off(&caller, handoff("solve the CAPTCHA"))
            .await
            .expect("parks");

        let (what, args) = heard.recv().await.expect("the fleet hears the hold");
        assert_eq!(what, "hand_off_started", "the fleet is told which tab is held");
        assert_eq!(args, serde_json::json!({ "seat": caller, "tab": 2 }));
    }

    /// **The answer releases the tab.** A resolved hand-off holds nothing, so
    /// the sweep and the idle clock see it like any other tab again.
    #[tokio::test]
    async fn resolving_a_hand_off_releases_its_tab() {
        let (ws, _rx) = Workspace::testing_stub();
        let (calls, mut heard) = tokio::sync::mpsc::unbounded_channel();
        ws.install_browser_host(std::sync::Arc::new(StubHost { calls }));
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, _answer) = ws.register_browser_hand_off(&caller, handoff).await.expect("parks");
        let started = heard.recv().await.map(|(what, _)| what);
        assert_eq!(started, Some("hand_off_started".to_owned()));

        assert!(ws.resolve_browser_hand_off(
            id,
            &caller,
            answer(HandOffEnding::Done, HandOffSource::Takeover),
        ));

        let (what, args) = heard.recv().await.expect("the fleet hears the release");
        assert_eq!(what, "hand_off_finished", "the ending releases the tab it held");
        assert_eq!(args, serde_json::json!({ "seat": caller, "tab": 2 }));
    }

    /// A raise no view can show still took its hold, so that hold is given
    /// back rather than left on the tab forever.
    #[tokio::test]
    async fn a_hand_off_no_view_can_show_releases_its_tab() {
        let (ws, rx) = Workspace::testing_stub();
        drop(rx);
        let (calls, mut heard) = tokio::sync::mpsc::unbounded_channel();
        ws.install_browser_host(std::sync::Arc::new(StubHost { calls }));
        let caller = SessionSlot::from_str_for_test("caller-uuid");

        let (_id, answer) = ws
            .register_browser_hand_off(&caller, handoff("solve the CAPTCHA"))
            .await
            .expect("parks");
        assert!(answer.await.is_err(), "the fail-closed read");

        let mut calls = Vec::new();
        while let Some((what, _)) = heard.recv().await {
            calls.push(what);
            if calls.len() == 2 {
                break;
            }
        }
        assert_eq!(
            calls,
            vec!["hand_off_started".to_owned(), "hand_off_finished".to_owned()],
            "the hold taken for an unanswerable raise is released",
        );
    }

    /// Answering a hand-off nobody parked is refused rather than answered
    /// into the void.
    #[tokio::test]
    async fn answering_a_hand_off_that_is_not_pending_is_refused() {
        let (ws, _rx) = Workspace::testing_stub();
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        assert!(
            !ws.resolve_browser_hand_off(
                uuid::Uuid::new_v4(),
                &caller,
                answer(HandOffEnding::NotNow, HandOffSource::Takeover),
            ),
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
        let _heard = with_host(&ws);
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let (_id, answer) = ws
            .register_browser_hand_off(&caller, handoff("solve the CAPTCHA"))
            .await
            .expect("parks");

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
        let _heard = with_host(&ws);
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, waiter) = ws.register_browser_hand_off(&caller, handoff).await.expect("parks");
        while rx.try_recv().is_ok() {}

        let taken = answer(HandOffEnding::Done, HandOffSource::Takeover);
        assert!(ws.resolve_browser_hand_off(id, &caller, taken));

        assert_eq!(
            waiter.await.expect("the waiter is answered"),
            HandOffEnding::Done,
            "the ending the resolve carried is what the blocked handler gets",
        );
        let resolved = rx.try_recv().expect("the views hear it left the registry");
        let crate::protocol::SessionUpdate::BrowserHandOffResolved { key, id: seen, answer: heard } =
            resolved.update
        else {
            panic!("the update is the resolved one");
        };
        assert_eq!(key, caller);
        assert_eq!(seen, id);
        // **The whole answer crosses, not just its meaning**: a view that did
        // not answer reads the ending, and the log names the door and the
        // connection that did.
        assert_eq!(heard, taken, "the door and the client ride the resolved update");
    }

    /// **A connection going away is not an answer.** The park lives in the
    /// workspace's registry, not in a subscription: a client whose socket
    /// drops under a parked ask must leave it waiting, never end it as a
    /// decline on the person's behalf.
    #[tokio::test]
    async fn a_subscriber_going_away_does_not_answer_a_parked_hand_off() {
        let (ws, rx) = Workspace::testing_stub();
        let _heard = with_host(&ws);
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, mut waiter) =
            ws.register_browser_hand_off(&caller, handoff).await.expect("parks");

        drop(rx);

        assert!(ws.browser_handoff_waiting(id, &caller), "the park is still the core's");
        assert!(
            matches!(waiter.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
            "and the blocked call is still waiting for the person",
        );
    }

    /// **A person who spoke is not a person who declined.** The feedback line
    /// sends the session their words and ends the park, and the ending it
    /// sends must say so: `NotNow` there told the session "the person
    /// declined, decide what to do instead" while they were still working the
    /// page (live, 2026-10-26, twice on one CAPTCHA).
    #[tokio::test]
    async fn a_feedback_answer_reaches_the_waiter_as_feedback() {
        let (ws, _rx) = Workspace::testing_stub();
        let _heard = with_host(&ws);
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, waiter) = ws.register_browser_hand_off(&caller, handoff).await.expect("parks");

        assert!(ws.resolve_browser_hand_off(
            id,
            &caller,
            answer(HandOffEnding::Feedback, HandOffSource::Feedback),
        ));

        assert_eq!(
            waiter.await.expect("the waiter is answered"),
            HandOffEnding::Feedback,
            "the person spoke: the ending says feedback, never a decline",
        );
    }

    /// Only the session that asked may resolve it: an answer from another
    /// seat is refused by the owner read, not applied.
    #[tokio::test]
    async fn only_the_owner_resolves_a_hand_off() {
        let (ws, _rx) = Workspace::testing_stub();
        let _heard = with_host(&ws);
        let owner = SessionSlot::from_str_for_test("owner-uuid");
        let other = SessionSlot::from_str_for_test("other-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, _answer) = ws.register_browser_hand_off(&owner, handoff).await.expect("parks");

        assert!(
            !ws.resolve_browser_hand_off(
                id,
                &other,
                answer(HandOffEnding::Done, HandOffSource::Takeover),
            ),
            "another seat's answer is refused",
        );
        assert!(ws.browser_handoff_waiting(id, &owner), "and the hand-off stays pending");
    }
}
