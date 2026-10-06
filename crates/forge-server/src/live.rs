//! What a view has learned from the stream, which its first render and every
//! later one both read.
//!
//! Three of the facts here are announced on the wire once and retained by
//! nobody: whether a completion has been shown, what a take is doing, and
//! which seats a view is showing. A view that attaches to an already-running
//! session cannot reconstruct them, so they are folded here rather than in
//! the view.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use forge_primitives::Message;
use forge_primitives::SessionSlot;
use forge_primitives::runtime::RuntimeSessionState;

use crate::SessionUpdate;
use crate::composer::Composer;
use crate::surface::is_success_result;
use crate::translate::state_parsing::parse_runtime_session_state;
use crate::unseen::Unseen;

/// What the view has learned from the stream, which the first render and
/// every later one both read.
#[derive(Default)]
pub struct Live {
    unseen: Unseen,
    /// The seats a page is open on, by how many connections are showing
    /// them: a turn finishing on one of those is a turn the reader watched.
    attached: HashMap<SessionSlot, usize>,
    composer: Composer,
}

/// What the stream has said, as one render reads it. A render takes this
/// rather than the lock: the guard is not `Send`, and a handler that held
/// it across its own awaits could not be one.
#[derive(Default)]
pub struct LiveState {
    pub unseen: Unseen,
    pub composer: Composer,
}

impl Live {
    pub fn new() -> Self {
        Self::default()
    }

    /// A panicking task must not take the view's live state with it.
    pub fn lock(live: &Mutex<Self>) -> MutexGuard<'_, Self> {
        live.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn snapshot(&self) -> LiveState {
        LiveState { unseen: self.unseen.clone(), composer: self.composer.clone() }
    }

    /// This view has shown `slot`, so nothing about it is unseen.
    pub fn seen(&mut self, slot: &SessionSlot) {
        self.unseen.clear(slot);
    }

    /// A page is open on `slot`, which is this view showing it, so a mark
    /// armed before the page opened goes with it. Counted, because two tabs
    /// on one seat are one seat still being shown.
    pub fn attach(&mut self, slot: &SessionSlot) {
        *self.attached.entry(slot.clone()).or_default() += 1;
        self.unseen.clear(slot);
    }

    /// One page on `slot` has gone. The seat is let go with the last of them.
    pub fn detach(&mut self, slot: &SessionSlot) {
        let Some(count) = self.attached.get_mut(slot) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.attached.remove(slot);
        }
    }

    /// Whether a page is holding `slot`.
    pub fn is_attached(&self, slot: &SessionSlot) -> bool {
        self.attached.contains_key(slot)
    }

    /// Fold one update in, answering what it asks of each page.
    ///
    /// The filter is what keeps a busy turn from re-sending the fleet for
    /// every token of it: only the updates that can change what a page
    /// draws redraw it. Two answers rather than one, because the pages are
    /// different: a take's twenty readings a second are the composer's news
    /// and not the fleet's, and the fleet's region carries no composer.
    pub fn apply(&mut self, update: &SessionUpdate) -> Redraw {
        let composer = self.composer.apply(update);
        let fleet = match fleet_news(update) {
            FleetNews::Nothing => false,
            // A turn finished on a session this page is not showing, so the
            // row earns its diamond until the session is opened. A seat a
            // page is open on has already shown it, so it earns nothing,
            // and the row settles out of running like any other.
            FleetNews::Completed(key) => {
                if !self.attached.contains_key(key) {
                    self.unseen.mark_completed(key);
                }
                true
            }
            // Work started again, which supersedes the completion the
            // diamond marks, and a fresh occupant whose history is not a
            // completion this page failed to show. Serving the seat's page
            // clears it too, so this is the clear for a seat nobody opened.
            FleetNews::Running(key) | FleetNews::Occupant(key) => {
                self.unseen.clear(key);
                true
            }
            FleetNews::Redraw => true,
        };
        Redraw { fleet, composer }
    }
}

/// What one update asks of the fleet region: the rows, and the marks on them.
///
/// One classification for its two readers - the view folding the stream, and
/// the socket deciding which subscribers an update belongs to - because a
/// second table of the variants would drift from this one.
pub enum FleetNews<'a> {
    /// The region draws nothing of it. Most of the stream: a turn's own words
    /// are the bulk of it, and no row shows one.
    Nothing,
    /// A row or a mark changed.
    Redraw,
    /// A turn finished on the seat.
    Completed(&'a SessionSlot),
    /// Work started again on the seat.
    Running(&'a SessionSlot),
    /// A fresh occupant took the seat.
    Occupant(&'a SessionSlot),
}

impl FleetNews<'_> {
    /// Whether the region draws anything of this update at all.
    pub fn any(&self) -> bool {
        !matches!(self, Self::Nothing)
    }
}

/// Classify one update for the fleet region.
///
/// The filter is what keeps a busy turn from re-sending the fleet for every
/// token of it: only the updates that can change what a row draws belong here.
pub fn fleet_news(update: &SessionUpdate) -> FleetNews<'_> {
    match update {
        // A prompt frame is a user turn: neither arm below draws anything of
        // it, so the origin does not change what the fleet folds.
        SessionUpdate::ChatAppended { key, msg, .. } => match msg {
            Message::Result { is_error, subtype, .. }
                if is_success_result(*is_error, subtype) =>
            {
                FleetNews::Completed(key)
            }
            Message::System { subtype, data, .. } if subtype == "session_state_changed" => {
                if parse_runtime_session_state(data.get("state")) == Some(RuntimeSessionState::Running)
                {
                    FleetNews::Running(key)
                } else {
                    FleetNews::Redraw
                }
            }
            Message::BackgroundTasksChanged { .. } => FleetNews::Redraw,
            _ => FleetNews::Nothing,
        },
        // The row set, and what each row is.
        SessionUpdate::Spawning { key, .. }
        | SessionUpdate::Connected { key, .. }
        | SessionUpdate::SessionReplaced { key, .. } => FleetNews::Occupant(key),
        // Everything else that changes what a row or a card says. The
        // catalog, the dictation snapshot and the claude version all arrive
        // after the listener binds: a page opened in those first seconds
        // would otherwise keep the empty answer it painted until the next
        // tick.
        SessionUpdate::CatalogLoaded
        | SessionUpdate::CliVersionChanged
        // The account pool, which the band's own card draws and no row does.
        // A page that read it once drew `0 ready, probing` until the next
        // unrelated redraw.
        | SessionUpdate::AccountsChanged
        | SessionUpdate::DictateAvailability
        | SessionUpdate::ConnectionFailed { .. }
        | SessionUpdate::AuthRequired { .. }
        | SessionUpdate::TurnError { .. }
        | SessionUpdate::TurnCancelled { .. }
        | SessionUpdate::PermissionRequest { .. }
        | SessionUpdate::QuestionRequest { .. }
        // Answering moves the seat out of the rail's needs-you group and
        // drops the inspector's pending row, so it redraws a row even though
        // it is the composer that asked for it.
        | SessionUpdate::PendingInteractionResolved { .. }
        // A held draft is the third kind of ask: its seat moves into the
        // needs-you group while it waits and back out when it resolves, so a
        // home-only subscriber has to be sent the pair or its row reads as
        // it stood before the draft (#1758). A browser hand-off is the same
        // shape on the same grounds.
        | SessionUpdate::SlackPostPending { .. }
        | SessionUpdate::SlackDraftResolved { .. }
        | SessionUpdate::BrowserHandOffPending { .. }
        | SessionUpdate::BrowserHandOffResolved { .. }
        | SessionUpdate::WorkerStatusChanged { .. }
        // The project's task set, its schedules and its connector
        // subscriptions moved - the three sections the home's project row
        // draws from this stream.
        | SessionUpdate::TasksChanged { .. }
        | SessionUpdate::CronSchedulesChanged { .. }
        | SessionUpdate::ConnectorSubscriptionsChanged { .. } => FleetNews::Redraw,
        // Everything else is the conversation, which no row shows.
        _ => FleetNews::Nothing,
    }
}

/// What one update asks of the two pages that fold the stream: the fleet
/// region, which draws rows and marks, and the composer, which draws a
/// take, a prompt and a sign-in. An update can be news to one and not the
/// other, so one answer cannot serve both.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Redraw {
    pub fleet: bool,
    pub composer: bool,
}

impl Redraw {
    /// Whether either page has to be redrawn.
    pub fn any(self) -> bool {
        self.fleet || self.composer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_message(subtype: &str, is_error: bool) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": subtype,
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": is_error,
            "num_turns": 1,
            "session_id": "s",
        }))
        .expect("parse a result message")
    }

    fn appended(key: &SessionSlot, msg: Message) -> SessionUpdate {
        SessionUpdate::ChatAppended { key: key.clone(), msg, origin: None }
    }

    fn session_state(state: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "session_state_changed",
            "session_id": "s",
            "state": state,
        }))
        .expect("parse a state message")
    }

    /// A diamond goes when its seat moves on without being looked at: work
    /// started again, or a fresh occupant took the slot. The other clear,
    /// the seat's page being served, is pinned through the route.
    #[test]
    fn starting_work_again_clears_the_diamond() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.apply(&appended(&slot, result_message("success", false)));
        assert!(live.snapshot().unseen.is_unseen(&slot), "precondition: the turn armed it");

        assert!(
            live.apply(&appended(&slot, session_state("running"))).fleet,
            "a turn starting redraws the page",
        );
        assert!(
            !live.snapshot().unseen.is_unseen(&slot),
            "and clears a completion nobody looked at",
        );

        // The same for a slot a new occupant took: its history is not a
        // completion this page failed to show.
        let taken = SessionSlot::lead("Org", "other");
        live.apply(&appended(&taken, result_message("success", false)));
        assert!(live.snapshot().unseen.is_unseen(&taken), "precondition: armed");

        live.apply(&SessionUpdate::Connected {
            key: taken.clone(),
            session_id: forge_primitives::SessionId::new("new"),
            cwd: "/proj".to_owned(),
            current_model: forge_primitives::CurrentModel {
                resolved_id: "claude".to_owned(),
                display_name_short: "claude".to_owned(),
                display_name_long: "claude".to_owned(),
                requested_id: None,
                catalog_id: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_auto_mode: None,
                supports_adaptive_thinking: None,
                is_authoritative: true,
            },
            available_models: Vec::new(),
            mode: None,
            history: Vec::new(),
            compaction_count: 0,
        });

        assert!(
            !live.snapshot().unseen.is_unseen(&taken),
            "a replaced occupant starts from a clean row",
        );

        // And clearing one slot leaves the rest alone, which is the
        // property the fold has to keep: a new turn somewhere is not news
        // about somewhere else.
        let untouched = SessionSlot::lead("Org", "untouched");
        live.apply(&appended(&slot, result_message("success", false)));
        live.apply(&appended(&untouched, result_message("success", false)));
        live.apply(&appended(&slot, session_state("running")));
        let unseen = live.snapshot().unseen;
        assert!(!unseen.is_unseen(&slot), "the slot that started again is cleared");
        assert!(unseen.is_unseen(&untouched), "and the slot that did not keeps its diamond");
    }

    /// Catches a diamond armed by the wrong result, and a page redrawn for
    /// the conversation it does not show.
    #[test]
    fn only_a_finished_turn_arms_the_diamond() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();

        assert!(
            !live.apply(&appended(&slot, result_message("error_during_execution", true))).fleet,
            "a turn that failed is not a turn that finished",
        );
        assert!(!live.snapshot().unseen.is_unseen(&slot), "so nothing is unseen");

        assert!(
            live.apply(&appended(&slot, result_message("success", false))).fleet,
            "a finished turn redraws the page",
        );
        assert!(live.snapshot().unseen.is_unseen(&slot), "and leaves the diamond");

        let other = SessionSlot::lead("Org", "other");
        assert!(
            live.apply(&appended(&other, result_message("success", false))).fleet,
            "a second slot's finish is the same kind of event",
        );
        let unseen = live.snapshot().unseen;
        assert!(unseen.is_unseen(&other), "and earns its own diamond");
        assert!(unseen.is_unseen(&slot), "without clearing the first slot's");
    }

    /// A turn that finishes on a seat whose page is open is a turn the reader
    /// watched, so it earns no diamond - and the row still redraws, because
    /// the turn that was running has ended.
    #[test]
    fn an_attached_seat_earns_no_diamond_and_still_settles() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.attach(&slot);

        assert!(
            live.apply(&appended(&slot, result_message("success", false))).fleet,
            "the row settles out of running, so the page is redrawn",
        );
        assert!(
            !live.snapshot().unseen.is_unseen(&slot),
            "and the page that is open on it has shown the turn",
        );
    }

    /// The other half: with the page gone, a completion is unlooked again.
    #[test]
    fn a_seat_arms_again_once_its_page_closes() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.attach(&slot);
        live.detach(&slot);

        assert!(
            live.apply(&appended(&slot, result_message("success", false))).fleet,
            "the diamond is the home's news again",
        );
        assert!(
            live.snapshot().unseen.is_unseen(&slot),
            "so a seat nobody is showing earns its diamond",
        );
    }

    /// Two tabs on one seat are two connections and one seat still being
    /// shown. Catches holding the attachment as a flag, where closing either
    /// tab re-arms a mark the other tab is still displaying.
    #[test]
    fn two_pages_on_one_seat_hold_it_until_both_close() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.attach(&slot);
        live.attach(&slot);
        live.detach(&slot);

        live.apply(&appended(&slot, result_message("success", false)));
        assert!(
            !live.snapshot().unseen.is_unseen(&slot),
            "the tab still open on it has shown the turn",
        );

        live.detach(&slot);
        live.apply(&appended(&slot, result_message("success", false)));
        assert!(
            live.snapshot().unseen.is_unseen(&slot),
            "and with both gone the seat is unlooked again",
        );
    }

    /// The window between the page being served and its stream attaching: a
    /// completion landing in it would otherwise sit on a page that is open.
    #[test]
    fn attaching_clears_a_mark_that_armed_before_it() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.apply(&appended(&slot, result_message("success", false)));
        assert!(live.snapshot().unseen.is_unseen(&slot), "precondition: the turn armed it");

        live.attach(&slot);
        assert!(
            !live.snapshot().unseen.is_unseen(&slot),
            "the page opening is the reader being shown the seat",
        );
    }

    /// A seat nobody is showing leaves the map rather than sitting in it at
    /// zero, which on a process up for a week is a seat-shaped leak.
    #[test]
    fn a_closed_page_leaves_no_entry_behind() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.attach(&slot);
        assert_eq!(live.attached.len(), 1, "precondition: the seat is held");

        live.detach(&slot);
        assert!(
            live.attached.is_empty(),
            "the last page closing takes the seat out of the map: {:?}",
            live.attached,
        );
    }

    /// The bulk of the stream is the conversation, which this page does not
    /// draw; redrawing for it would re-send the fleet per token.
    #[test]
    fn a_chat_message_does_not_redraw_the_page() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();

        assert!(
            !live
                .apply(&SessionUpdate::ChatAppended {
                    key: slot,
                    origin: None,
                    msg: serde_json::from_value(serde_json::json!({
                        "type": "user",
                        "message": { "role": "user", "content": "hello" },
                        "session_id": "s",
                    }))
                    .expect("parse a user message"),
                })
                .any(),
            "a chat message is not something this page draws",
        );
    }

    /// A take's readings are the composer's news, not the fleet's. The
    /// region this stream re-sends draws rows, so redrawing it for every
    /// level would rebuild the whole page twenty times a second while a
    /// take runs - and a composer still has to redraw for them, or the meter
    /// never moves. Catches either answer being taken for the other.
    #[test]
    fn a_takes_readings_redraw_the_composer_and_not_the_fleet() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();

        for update in [
            SessionUpdate::DictateStarted {
                key: slot.clone(),
                floor_db: -50.0,
                generation: 1,
                initiator: None,
            },
            SessionUpdate::DictateLevel { key: slot.clone(), peak_db: -20.0, initiator: None },
        ] {
            let redraw = live.apply(&update);
            assert!(!redraw.fleet, "{update:?} is not a row changing");
            assert!(redraw.composer, "{update:?} is the composer's to draw");
        }
    }

    /// The updates that land after the listener binds, and that a page
    /// opened in that window has already painted an answer for: the
    /// catalog scan, the dictation snapshot, the claude version probe and
    /// the account pool settling. Catches a page that keeps the empty
    /// answer until the next tick.
    ///
    /// The pool is the one that bites hardest, because nothing else in the
    /// stream mentions it: a quiet forge emits no other update, so a card
    /// that read `0 ready, probing` at subscribe reads it for the life of
    /// the connection.
    #[test]
    fn the_late_boot_updates_redraw_the_page() {
        let mut live = Live::new();

        for update in [
            SessionUpdate::CatalogLoaded,
            SessionUpdate::DictateAvailability,
            SessionUpdate::CliVersionChanged,
            SessionUpdate::AccountsChanged,
        ] {
            assert!(live.apply(&update).fleet, "{update:?} is exactly a render wake-up");
        }
    }
}
