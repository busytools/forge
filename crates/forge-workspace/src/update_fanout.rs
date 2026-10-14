//! Fan-out for the workspace's [`SessionUpdate`] stream.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::SessionSlot;
use crate::protocol::SessionUpdate;

/// One frame on the stream: the update, and the number the seat's own
/// counter minted for it.
///
/// **The number is what makes a miss detectable.** Every frame the workspace
/// emits for a seat is numbered on that seat's counter - whichever site
/// emitted it - so a client that reads 5 and then 7 knows exactly one frame
/// went missing and can ask for it. `seq` is `None` for a frame belonging to
/// no seat, which rides the stream without joining any sequence.
#[derive(Clone, Debug)]
pub struct SeatUpdate {
    pub seq: Option<u64>,
    pub update: SessionUpdate,
}

/// What a subscriber can do with what it is sent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriberRole {
    /// Renders the workspace's prompts - permission, question, Slack
    /// draft - and answers them. The answering view's role, and the one
    /// [`UpdateFanout::send_answering`] reports on.
    Answering,
    /// Reads the stream only. A prompt delivered to observers alone is
    /// one nobody will answer, so a path that parks a turn on an answer
    /// must not count this subscriber.
    Observing,
}

/// One subscriber's end of the fan-out.
struct Registration {
    tx: mpsc::UnboundedSender<SeatUpdate>,
    role: SubscriberRole,
}

#[derive(Default)]
struct Shared {
    subscribers: Vec<Registration>,
    /// Updates emitted before the first subscriber attached, held for
    /// whoever attaches first so a notice raised during boot is not lost.
    pending: Vec<SeatUpdate>,
    /// Set by the first [`UpdateFanout::subscribe`]. After that a send
    /// that reaches nobody is dropped rather than held, so a subscriber
    /// attaching later inherits no backlog.
    attached: bool,
    /// Where each seat's stream has got to: the number its newest frame was
    /// minted under. Absent for a seat that has not emitted yet, and for one
    /// whose last frame opened a new epoch.
    ///
    /// **Per seat, keyed by the frame's own slot**, so a frame for one seat
    /// can neither advance another's counter nor leave a hole in it.
    sequences: HashMap<SessionSlot, u64>,
}

impl Shared {
    /// Number `update` on its seat's counter, or leave it unnumbered when it
    /// belongs to no seat.
    ///
    /// **An occupant change closes its epoch.** `Connected` and
    /// `SessionReplaced` are numbered on the run they end and the seat's
    /// counter starts over, so the frame after one opens the new epoch at 1:
    /// the client learns the occupant from that frame itself and wipes to
    /// expect 1, never a gap.
    fn numbered(&mut self, update: SessionUpdate) -> SeatUpdate {
        let Some(slot) = update.slot() else {
            return SeatUpdate { seq: None, update };
        };
        let seq = self.sequences.get(slot).copied().unwrap_or(0) + 1;
        if matches!(update, SessionUpdate::Connected { .. } | SessionUpdate::SessionReplaced { .. })
        {
            self.sequences.remove(slot);
        } else {
            self.sequences.insert(slot.clone(), seq);
        }
        SeatUpdate { seq: Some(seq), update }
    }
}

/// Cloneable sender half of the workspace's fan-out to its subscribers.
///
/// Every clone shares one registry. [`Self::send`] hands the update to
/// each subscriber and drops any whose receiver has gone, so the
/// registry never keeps a dead one.
#[derive(Clone, Default)]
pub(crate) struct UpdateFanout {
    shared: Arc<Mutex<Shared>>,
}

impl UpdateFanout {
    /// Mint a subscriber stream, declaring what the caller can do with
    /// it. The first caller also takes whatever was emitted before it
    /// attached, whichever role it declares; every later one carries
    /// what is emitted after its own call and nothing before it.
    pub(crate) fn subscribe(&self, role: SubscriberRole) -> mpsc::UnboundedReceiver<SeatUpdate> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut shared = self.shared.lock();
        for held in std::mem::take(&mut shared.pending) {
            let _ = tx.send(held);
        }
        shared.attached = true;
        shared.subscribers.push(Registration { tx, role });
        rx
    }

    /// Mint a subscriber that carries only what is emitted from here on,
    /// and leaves the pre-attach backlog for whoever attaches next.
    ///
    /// A mirror - something that folds the stream into state of its own
    /// and reads the rest from a snapshot when it is asked - has no use
    /// for the backlog. Taking it would be worse than useless: the
    /// backlog goes to the first caller, so a mirror attaching at boot
    /// would silently take the boot notice out of the view that renders
    /// prompts.
    pub(crate) fn subscribe_without_backlog(
        &self,
        role: SubscriberRole,
    ) -> mpsc::UnboundedReceiver<SeatUpdate> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared.lock().subscribers.push(Registration { tx, role });
        rx
    }

    /// Deliver `update` to every subscriber, and report whether one took
    /// it. `false` is the "no receiver" signal the single-take channel
    /// gave, which `spawn::try_emit` and `SessionTask::emit` log on; an
    /// update emitted before anything attached is held for the first
    /// subscriber and reports `false` too, since no subscriber has it.
    ///
    /// The last subscriber is taken out of the registry to receive
    /// `update` itself while the rest are sent a copy, so the common
    /// one-subscriber case clones nothing. It goes back only if it took
    /// the update, which is also where a dead one leaves.
    pub(crate) fn send(&self, update: SessionUpdate) -> bool {
        self.deliver(update, None)
    }

    /// How many subscribers are attached.
    ///
    /// A test uses this to tell one subscription from another. "Did anyone
    /// receive" cannot: a listener that is always there - the socket's own
    /// fold - makes every emit land, however many views have come and gone.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn count(&self) -> usize {
        self.shared.lock().subscribers.len()
    }

    /// How many subscribers could answer a prompt. Test-only: the role decides
    /// whether the core parks a turn on a subscriber's reply, and nothing else
    /// reports it.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn answering_count(&self) -> usize {
        self.shared
            .lock()
            .subscribers
            .iter()
            .filter(|held| held.role == SubscriberRole::Answering)
            .count()
    }

    /// Deliver `update` to every subscriber, and report whether one that
    /// can ANSWER it took it. A path that parks a turn on an answer uses
    /// this rather than [`Self::send`]: an observer takes the update and
    /// never replies, so counting it would leave the turn waiting on a
    /// response nobody will send.
    ///
    /// Unlike [`Self::send`], an update emitted before anything attached
    /// is not held for the first subscriber: replaying it would offer
    /// the first view a reply that reaches nothing. So a caller must
    /// fail its turn closed on a `false` return; one that parked the
    /// turn anyway belongs on [`Self::send`].
    pub(crate) fn send_answering(&self, update: SessionUpdate) -> bool {
        self.deliver(update, Some(SubscriberRole::Answering))
    }

    fn deliver(&self, update: SessionUpdate, required: Option<SubscriberRole>) -> bool {
        let mut shared = self.shared.lock();
        let update = shared.numbered(update);
        let Some(tail) = shared.subscribers.pop() else {
            // Hold what was emitted before anything attached, so a boot
            // notice is not lost. A role-gated update is not held,
            // because a caller of `send_answering` must fail its turn
            // closed on a false return: replaying it would offer the
            // first subscriber a reply that reaches nothing.
            if !shared.attached && required.is_none() {
                shared.pending.push(update);
            }
            return false;
        };
        let counts = |role: SubscriberRole| required.is_none_or(|want| want == role);
        let mut took_it = false;
        shared.subscribers.retain(|subscriber| {
            let live = subscriber.tx.send(update.clone()).is_ok();
            took_it |= live && counts(subscriber.role);
            live
        });
        if tail.tx.send(update).is_ok() {
            took_it |= counts(tail.role);
            shared.subscribers.push(tail);
        }
        took_it
    }
}

#[cfg(test)]
mod tests {
    use forge_primitives::SessionId;
    use forge_primitives::cloud::service_status::ServiceSeverity;
    use forge_primitives::runtime::CurrentModel;

    use super::*;
    use crate::SessionSlot;

    fn status(message: &str) -> SessionUpdate {
        SessionUpdate::ServiceStatus {
            severity: ServiceSeverity::Warning,
            message: message.to_owned(),
        }
    }

    fn seat(label: &str) -> SessionSlot {
        SessionSlot::from_str_for_test(label)
    }

    /// A frame the seat emits. Any of them will do, and this one is the
    /// cheapest to build.
    fn a_turn_ends(key: &SessionSlot) -> SessionUpdate {
        SessionUpdate::TurnComplete { key: key.clone(), terminal_reason: None }
    }

    /// The seat's occupant changing, which is the frame an epoch turns on.
    fn occupant_change(key: &SessionSlot) -> SessionUpdate {
        SessionUpdate::SessionReplaced {
            key: key.clone(),
            session_id: SessionId("a-new-occupant".to_owned()),
            cwd: "/tmp".to_owned(),
            current_model: CurrentModel::new("m", "m", "m"),
            available_models: Vec::new(),
            mode: None,
            history: Vec::new(),
            dropped: 0,
            compaction_count: 0,
        }
    }

    /// The next frame on `who`'s stream, unread.
    fn frame(rx: &mut mpsc::UnboundedReceiver<SeatUpdate>, who: &str) -> SeatUpdate {
        rx.try_recv().unwrap_or_else(|err| {
            panic!("expected an update on the {who} subscriber's stream: {err}")
        })
    }

    /// The next frame's number, for the tests that read a seat's sequence.
    fn next_seq(rx: &mut mpsc::UnboundedReceiver<SeatUpdate>, who: &str) -> Option<u64> {
        frame(rx, who).seq
    }

    fn said(frame: SeatUpdate) -> String {
        match frame.update {
            SessionUpdate::ServiceStatus { message, .. } => message,
            other => panic!("expected the ServiceStatus envelope this test sends, got {other:?}"),
        }
    }

    fn next(rx: &mut mpsc::UnboundedReceiver<SeatUpdate>, who: &str) -> String {
        said(frame(rx, who))
    }

    /// Catches `send` delivering to only the first subscriber, which is
    /// the single-take behaviour this replaces.
    #[test]
    fn each_subscriber_receives_every_update() {
        let fanout = UpdateFanout::default();
        let mut first = fanout.subscribe(SubscriberRole::Answering);
        let mut second = fanout.subscribe(SubscriberRole::Answering);

        assert!(fanout.send(status("one")), "a subscribed fan-out delivers");

        assert_eq!(next(&mut first, "first"), "one", "the first subscriber sees the update");
        assert_eq!(
            next(&mut second, "second"),
            "one",
            "the second subscriber sees the same update"
        );
    }

    /// Catches dropping the prune in `send`, which leaves a dead sender
    /// in the registry for the life of the workspace. The dropped
    /// subscriber is subscribed FIRST, so it is not the one `send` takes
    /// out as the tail and the prune is the only thing that can remove
    /// it.
    #[test]
    fn a_subscriber_that_drops_leaves_the_fan_out() {
        let fanout = UpdateFanout::default();
        let dropped = fanout.subscribe(SubscriberRole::Answering);
        let mut kept = fanout.subscribe(SubscriberRole::Answering);
        drop(dropped);

        assert!(fanout.send(status("one")), "the live subscriber still receives");

        assert_eq!(
            fanout.shared.lock().subscribers.len(),
            1,
            "the dropped subscriber is gone from the registry",
        );
        assert_eq!(next(&mut kept, "kept"), "one", "the live subscriber still receives");
    }

    /// Catches buffering emitted updates and replaying them to the next
    /// subscriber, which hands a view a backlog it did not ask for.
    #[test]
    fn a_late_subscriber_sees_only_what_follows_it() {
        let fanout = UpdateFanout::default();
        let mut first = fanout.subscribe(SubscriberRole::Answering);
        assert!(fanout.send(status("before")), "the first subscriber receives");

        let mut late = fanout.subscribe(SubscriberRole::Answering);
        assert!(fanout.send(status("after")), "both subscribers receive");

        assert_eq!(
            next(&mut first, "first"),
            "before",
            "the first subscriber keeps its earlier update"
        );
        assert_eq!(
            next(&mut first, "first"),
            "after",
            "the first subscriber also sees the later one"
        );
        assert_eq!(next(&mut late, "late"), "after", "the late subscriber misses the backlog");
        assert!(
            late.try_recv().is_err(),
            "the late subscriber is handed the backlog and nothing more",
        );
    }

    /// Catches dropping the hold for what was emitted before anything
    /// attached, which loses a notice the workspace raises during boot
    /// and leaves the first subscriber starting empty.
    #[test]
    fn the_first_subscriber_takes_what_was_emitted_before_it() {
        let fanout = UpdateFanout::default();
        assert!(!fanout.send(status("boot")), "nothing has attached to take it");

        let mut first = fanout.subscribe(SubscriberRole::Answering);
        let mut second = fanout.subscribe(SubscriberRole::Answering);

        assert_eq!(
            next(&mut first, "first"),
            "boot",
            "the first subscriber is handed the boot notice"
        );
        assert!(second.try_recv().is_err(), "the second subscriber is handed no boot backlog");
    }

    /// A mirror takes no backlog and leaves it for whoever attaches next.
    ///
    /// Both halves are the point. A view that folds the stream into state
    /// of its own reads the rest from a snapshot, so replaying to it is
    /// waste; and because the backlog goes to the FIRST caller, a mirror
    /// attaching at boot would otherwise take the boot notice out of the
    /// view that renders prompts. Catches either direction being changed:
    /// a mirror pointed at the backlog-taking form, or a prompt-rendering
    /// view pointed at the mirror's.
    #[test]
    fn a_mirror_takes_no_backlog_and_leaves_it_for_the_next_subscriber() {
        let fanout = UpdateFanout::default();
        assert!(!fanout.send(status("boot")), "nothing has attached to take it");

        let mut mirror = fanout.subscribe_without_backlog(SubscriberRole::Observing);
        assert!(
            mirror.try_recv().is_err(),
            "a mirror is handed nothing that was emitted before it attached",
        );

        let mut answering = fanout.subscribe(SubscriberRole::Answering);
        assert_eq!(
            next(&mut answering, "answering"),
            "boot",
            "the boot notice is still there for the view that renders prompts",
        );

        assert!(fanout.send(status("later")), "an update after both attached reaches one of them");
        assert_eq!(next(&mut mirror, "mirror"), "later", "and the mirror carries it too");
    }

    /// Catches holding a role-gated update for the first subscriber. The
    /// guard that raised it has already failed it closed, so replaying
    /// it would hand the first view a prompt whose slot is gone and
    /// offer a reply that reaches nothing.
    #[test]
    fn a_role_gated_update_is_not_held_for_the_first_subscriber() {
        let fanout = UpdateFanout::default();
        assert!(!fanout.send(status("notice")), "a plain update is held");
        assert!(!fanout.send_answering(status("prompt")), "nothing can answer it yet");

        let mut first = fanout.subscribe(SubscriberRole::Answering);

        assert_eq!(
            next(&mut first, "first"),
            "notice",
            "the first subscriber is handed the plain update",
        );
        assert!(first.try_recv().is_err(), "the failed-closed prompt is not replayed to it");
    }

    /// Catches pointing a guard at `send`, which counts an observer as an
    /// answer: the update reaches the observer, but the turn that raised
    /// it would wait forever on a reply an observer never sends.
    #[test]
    fn an_observer_takes_the_update_without_answering_it() {
        let fanout = UpdateFanout::default();
        let mut observer = fanout.subscribe(SubscriberRole::Observing);

        assert!(!fanout.send_answering(status("one")), "an observer does not answer");
        assert_eq!(next(&mut observer, "observer"), "one", "the observer still sees the update");

        let mut answering = fanout.subscribe(SubscriberRole::Answering);
        assert!(fanout.send_answering(status("two")), "an answering subscriber answers");
        assert_eq!(next(&mut answering, "answering"), "two", "it sees the update too");
    }

    /// Catches `send` swallowing the no-subscriber case, which silences
    /// the "receiver is gone" diagnostics at both emit sites.
    #[test]
    fn send_without_a_subscriber_reports_nothing_delivered() {
        let fanout = UpdateFanout::default();

        assert!(!fanout.send(status("one")), "an unsubscribed fan-out delivers to nobody");
    }

    /// A seat's frames are numbered on that seat's own counter, 1 then 2.
    #[test]
    fn a_seats_frames_carry_their_own_numbers() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");

        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the seat's first frame is number 1");
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "and its next is the successor");
    }

    /// An occupant change ends its epoch: the frame itself is the last of
    /// the run it closes, and the next frame opens the new epoch at 1.
    ///
    /// Catches resetting before the frame is numbered, which leaves the
    /// occupant frame carrying a number from the new epoch it opens.
    #[test]
    fn an_occupant_change_restarts_the_seats_numbers() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(fanout.send(occupant_change(&seat)), "a subscribed fan-out delivers");

        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the seat's first frame is number 1");
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "and its next is the successor");
        assert_eq!(
            next_seq(&mut rx, "seat"),
            Some(3),
            "the occupant frame is numbered on the counter it ends",
        );

        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert_eq!(
            next_seq(&mut rx, "seat"),
            Some(1),
            "and the frame after it opens the new epoch at 1",
        );
    }

    /// The first `Connected` of a fresh seat is the same rule: the seat had
    /// no occupant to number against, so the frame closes a run of one and
    /// the epoch it opens starts at 1.
    #[test]
    fn a_fresh_connect_opens_its_epoch_at_one() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(
            fanout.send(SessionUpdate::Connected {
                key: seat.clone(),
                session_id: SessionId("the-first-occupant".to_owned()),
                cwd: "/tmp".to_owned(),
                current_model: CurrentModel::new("m", "m", "m"),
                available_models: Vec::new(),
                mode: None,
                history: Vec::new(),
                dropped: 0,
                compaction_count: 0,
            }),
            "a subscribed fan-out delivers",
        );

        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the connect is the seat's first frame");

        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert_eq!(
            next_seq(&mut rx, "seat"),
            Some(1),
            "and the epoch it opens starts at 1, so the client wiping on it expects 1",
        );
    }

    /// A frame that belongs to no seat rides unnumbered, and numbering it
    /// would put a hole in every seat's sequence at once.
    #[test]
    fn a_frame_for_no_seat_rides_unnumbered() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(fanout.send(status("boot")), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");

        assert_eq!(
            next_seq(&mut rx, "app-level"),
            None,
            "a workspace-level frame carries no number",
        );
        assert_eq!(
            next_seq(&mut rx, "seat"),
            Some(1),
            "and it does not advance any seat's counter",
        );
    }

    /// Two seats interleaved keep independent counters: a frame for another
    /// seat must not advance this one's.
    ///
    /// Catches one counter for the whole fan-out, which is the reading that
    /// makes a second seat's traffic look like a hole in the first's.
    #[test]
    fn two_seats_keep_independent_counters() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let lead = seat("lead");
        let worker = seat("worker");

        assert!(fanout.send(a_turn_ends(&lead)), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&worker)), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&lead)), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&worker)), "a subscribed fan-out delivers");

        assert_eq!(next_seq(&mut rx, "lead"), Some(1), "the lead's first frame is number 1");
        assert_eq!(
            next_seq(&mut rx, "worker"),
            Some(1),
            "the worker's own run opens at 1 rather than continuing the lead's",
        );
        assert_eq!(next_seq(&mut rx, "lead"), Some(2), "the lead's next is the successor");
        assert_eq!(next_seq(&mut rx, "worker"), Some(2), "and so is the worker's");
    }

    /// The boot backlog keeps the numbers its frames were minted with, so a
    /// client joining at the first subscribe has no hole at the join.
    #[test]
    fn the_backlog_keeps_the_numbers_its_frames_were_minted_with() {
        let fanout = UpdateFanout::default();
        let seat = seat("lead");
        assert!(!fanout.send(a_turn_ends(&seat)), "nothing has attached to take it");
        assert!(!fanout.send(a_turn_ends(&seat)), "nothing has attached to take it");

        let mut first = fanout.subscribe(SubscriberRole::Answering);
        assert!(fanout.send(a_turn_ends(&seat)), "the subscriber is attached");

        assert_eq!(next_seq(&mut first, "first"), Some(1), "the held frame keeps its number");
        assert_eq!(next_seq(&mut first, "first"), Some(2), "and so does the one after it");
        assert_eq!(
            next_seq(&mut first, "first"),
            Some(3),
            "with the live frame continuing from the backlog rather than restarting",
        );
    }
}
