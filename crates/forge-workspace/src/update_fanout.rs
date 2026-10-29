//! Fan-out for the workspace's [`SessionUpdate`] stream.

use std::collections::{HashMap, HashSet, VecDeque};
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
    /// **Shared rather than owned, so one delivery is one pointer.** A frame
    /// goes to every subscriber of the fleet, and each of them filters it
    /// afterwards - so a deep clone per subscriber paid for a payload most of
    /// them drop. The ring holds the same pointer.
    pub update: Arc<SessionUpdate>,
}

/// How many frames one seat's stream keeps for repair.
///
/// **Sized from the window a client is served from**, like the conversation
/// copy the task holds: it is the stretch a repair is answered from memory,
/// and what falls past it is the transcript's own read. A ring is shed whole
/// at an occupant change - the numbers start over, so the frames before one
/// are unreachable.
const SEAT_RING: usize = crate::conversation_window::CONVERSATION_CAP;

/// The byte ceiling on one seat's ring: the same 64 MiB the conversation copy
/// beside it keeps ([`crate::conversation_window::CONVERSATION_CAP_BYTES`]).
///
/// **A ring exists to fill a copy that is behind**, so a ring allowed to weigh
/// more than the copy it repairs bounds nothing a reader feels. The count alone
/// left a released seat (a despawned worker, a lead closed for the day) pinning
/// up to the full frame count for the process's life, and one
/// `FileIndexChanged` frame holds a whole tree's index: at a hundred thousand
/// files that is 15 MB per retained frame.
const SEAT_RING_BYTES: usize = crate::conversation_window::CONVERSATION_CAP_BYTES;

/// What a frame costs against [`SEAT_RING_BYTES`], counted from the payloads
/// that can grow rather than from its serialization.
///
/// The big frames are big in their entries and messages; paying a JSON pass at
/// mint would double the cost of exactly the frames this ceiling exists for. A
/// frame whose payload is not counted here is charged a frame's fixed
/// overhead, which is what keeps a long run of small frames bounded too.
fn frame_bytes(update: &SessionUpdate) -> usize {
    /// Fields, numbers and names a wire entry carries beside its strings.
    const ENTRY: usize = 96;
    /// A frame's own fields, its slot and its envelope.
    const FRAME: usize = 256;
    let payload = match update {
        SessionUpdate::FileIndexChanged { index, .. } => index
            .entries
            .iter()
            .map(|(key, candidate)| {
                key.len()
                    + candidate.rel_path.len()
                    + candidate.rel_path_lower.len()
                    + candidate.basename_lower.len()
                    + ENTRY
            })
            .sum(),
        SessionUpdate::ProcessesChanged { snapshot, .. } => snapshot
            .processes
            .iter()
            .map(|entry| entry.name.len() + entry.command.len() + ENTRY)
            .sum(),
        SessionUpdate::ChatAppended { msg, .. } => crate::conversation_window::wire_bytes(msg),
        SessionUpdate::CallOutput { output, .. } => match output {
            forge_primitives::CallOutput::Lines(lines) => {
                lines.iter().map(|line| line.len() + ENTRY).sum()
            }
            forge_primitives::CallOutput::FileGone | forge_primitives::CallOutput::NoPath => 0,
        },
        SessionUpdate::SubagentCardsChanged { cards, .. } => cards
            .iter()
            .map(|card| {
                card.name.len()
                    + card.dispatch_id.len()
                    + card.agent_type.as_deref().map_or(0, str::len)
                    + card
                        .tail
                        .iter()
                        .map(|call| call.name.len() + call.title.len() + ENTRY)
                        .sum::<usize>()
                    + ENTRY
            })
            .sum(),
        // Anything else is a frame of its own fixed weight.
        _ => 0,
    };
    payload.saturating_add(FRAME)
}

/// One frame in a seat's ring, with the weight it was charged when it was
/// minted: eviction subtracts that rather than weighing the frame again
/// under the fan-out's lock.
struct Kept {
    frame: SeatUpdate,
    bytes: usize,
}

/// Keep one unringed number, bounded to a ring's frame count - the stretch
/// beyond which no window a fill could serve reaches.
fn remember(numbers: &mut VecDeque<u64>, seq: u64, cap: usize) {
    numbers.push_back(seq);
    while numbers.len() > cap {
        numbers.pop_front();
    }
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
///
/// **The channel is unbounded, and what bounds it instead is the connection.**
/// A bounded channel forces the sender to choose between blocking - which
/// would hold every seat's stream behind one slow reader, under this fan-out's
/// own lock - and dropping a frame, which rule 25 forbids. So the sender never
/// waits, and the receiving connection drains continuously: a socket that
/// stops accepting its frames is cut at `WRITE_BOUND` (60 s), so a subscriber
/// that keeps up holds a batch's worth and one that does not is disconnected
/// rather than queued against forever. The per-frame weight is a pointer now,
/// shared with the ring below rather than copied per subscriber.
struct Registration {
    tx: mpsc::UnboundedSender<SeatUpdate>,
    role: SubscriberRole,
}

struct Shared {
    subscribers: Vec<Registration>,
    /// Where each seat's stream has got to: the number its newest frame was
    /// minted under. Absent for a seat that has not emitted yet, and for one
    /// whose last frame opened a new epoch.
    ///
    /// **Per seat, keyed by the frame's own slot**, so a frame for one seat
    /// can neither advance another's counter nor leave a hole in it.
    sequences: HashMap<SessionSlot, u64>,
    /// The newest frames each seat's stream has emitted, oldest first, for a
    /// client that read a gap in the numbers and asks for what it missed.
    ///
    /// Held here rather than on the session task because here is where every
    /// frame of the seat crosses, whichever site emitted it - and the number a
    /// repair hands back is the one minted here.
    rings: HashMap<SessionSlot, VecDeque<Kept>>,
    /// What each seat's ring weighs, beside the frames themselves, so the
    /// byte ceiling is a running total rather than a walk of the ring.
    ring_bytes: HashMap<SessionSlot, usize>,
    /// The numbers each seat has minted that the ring does NOT hold: the
    /// frames that close an epoch. A reader measuring whether a copy covers a
    /// stretch needs them, because a window can never serve them and a gap
    /// holding only one of them costs the copy nothing.
    ///
    /// **Per epoch, not per seat.** An occupant change mints from 1 again, so
    /// the numbers a seal's epoch carried are re-minted by the run after it -
    /// and the record of them goes with the ring. Bounded to a ring's frame
    /// count: a number older than that lies beyond any window a fill could
    /// serve, where the count it underwrites is a refusal rather than a hole.
    unringed: HashMap<SessionSlot, VecDeque<u64>>,
    /// The seats whose latest epoch-opening seal carried rows.
    ///
    /// **A seal's history is not in the ring**, so a reader filling a copy
    /// from the window cannot rebuild what the epoch opened on (#2446). An
    /// empty-history seal opens its epoch with nothing to miss, so each mint
    /// REPLACES this rather than adding to it. The value standing between an
    /// occupant change and its seal's mint is the previous seal's, which is
    /// what notices that gap.
    sealed_with_rows: HashSet<SessionSlot>,
    /// How many frames a ring keeps. [`SEAT_RING`] in production; a test
    /// drives it down so the transcript fallback is reachable without
    /// emitting thousands of frames.
    ring_cap: usize,
    /// How many bytes a ring keeps. [`SEAT_RING_BYTES`] in production; a test
    /// drives it down so the ceiling is reachable without a giant tree.
    ring_cap_bytes: usize,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            subscribers: Vec::new(),
            sequences: HashMap::new(),
            rings: HashMap::new(),
            ring_bytes: HashMap::new(),
            unringed: HashMap::new(),
            sealed_with_rows: HashSet::new(),
            ring_cap: SEAT_RING,
            ring_cap_bytes: SEAT_RING_BYTES,
        }
    }
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
    ///
    /// **One lock serializes numbering and delivery.** [`UpdateFanout::deliver`]
    /// mints and fans out under the same guard, so the numbers a seat's
    /// subscribers receive are in the order they were minted. Lifting the mint
    /// out of that guard - or fanning out around it - reorders a seat's stream
    /// against its own numbers.
    fn numbered(&mut self, update: SessionUpdate, charge: usize) -> SeatUpdate {
        let Some(slot) = update.slot().cloned() else {
            return SeatUpdate { seq: None, update: Arc::new(update) };
        };
        let seq = self.next_seq(&slot);
        // **The frames that open or close an epoch stay out of the ring.**
        // They carry the count of the run they end - and the history the
        // epoch hands over - so a ring that kept one would report a front
        // number the new run cannot answer for, and let that old number
        // collide with a new one as the count climbs back to it.
        let epoch_frame = matches!(
            &update,
            SessionUpdate::Connected { .. }
                | SessionUpdate::SessionReplaced { .. }
                | SessionUpdate::Releasing { .. }
        );
        if matches!(
            &update,
            SessionUpdate::Connected { .. } | SessionUpdate::SessionReplaced { .. }
        ) {
            self.sequences.remove(&slot);
            // **A new occupant starts counting again**, so the frames before
            // it belong to a stream no client holds any more - and a repair
            // that could reach into them would splice two conversations. The
            // unringed numbers are the same: the run after a seal mints them
            // again, and a stale one would cover a gap the new run opened.
            self.rings.remove(&slot);
            self.ring_bytes.remove(&slot);
            self.unringed.remove(&slot);
            // **The mark describes the LATEST seal, so each mint replaces
            // it.** Only a seal that carried rows leaves a gap the ring cannot
            // fill, and a later blank epoch must not inherit the previous
            // one's mark: its ring was cleared at this mint, so a fill holds
            // its whole history. Before the mint the value is still the
            // previous seal's, which is what notices the header-move window.
            let carried = match &update {
                SessionUpdate::Connected { history, .. }
                | SessionUpdate::SessionReplaced { history, .. } => !history.is_empty(),
                _ => false,
            };
            if carried {
                self.sealed_with_rows.insert(slot.clone());
            } else {
                self.sealed_with_rows.remove(&slot);
            }
        } else if matches!(update, SessionUpdate::Releasing { .. }) {
            // **The counter still advances** - the frame carries a number like
            // any other, and leaving the counter behind mints that number
            // again for the frame after it, which a client reads as one
            // already drawn.
            //
            // **The ring stays.** A seat a connection re-opens after its
            // session ended is answered from its record, and that read can be
            // behind frames this release ends; shedding the ring would refuse
            // it with a hole no window could fill. It is shed at the next
            // occupant's seal, which is where the numbers start over.
            self.sequences.insert(slot.clone(), seq);
            remember(self.unringed.entry(slot.clone()).or_default(), seq, self.ring_cap);
        } else {
            self.sequences.insert(slot.clone(), seq);
        }
        let frame = SeatUpdate { seq: Some(seq), update: Arc::new(update) };
        if !epoch_frame {
            let ring = self.rings.entry(slot.clone()).or_default();
            ring.push_back(Kept { frame: frame.clone(), bytes: charge });
            let bytes = self.ring_bytes.entry(slot).or_default();
            *bytes = bytes.saturating_add(charge);
            // Two ceilings, one cut: a giant index frame must not survive on
            // the count alone, and a long run of small frames must not survive
            // on the byte ceiling alone.
            while ring.len() > 1 && (ring.len() > self.ring_cap || *bytes > self.ring_cap_bytes) {
                let Some(old) = ring.pop_front() else {
                    break;
                };
                *bytes = bytes.saturating_sub(old.bytes);
            }
        }
        frame
    }

    /// The frames of `slot`'s ring that carry the numbers in `from..=to`,
    /// oldest first.
    fn frames_between(&self, slot: &SessionSlot, from: u64, to: u64) -> Vec<SeatUpdate> {
        self.rings
            .get(slot)
            .map(|ring| {
                ring.iter()
                    .map(|kept| &kept.frame)
                    .filter(|frame| frame.seq.is_some_and(|seq| (from..=to).contains(&seq)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The oldest number `slot`'s ring still holds, `None` for a seat that
    /// has emitted nothing.
    fn oldest_kept(&self, slot: &SessionSlot) -> Option<u64> {
        self.rings.get(slot).and_then(|ring| ring.front()).and_then(|kept| kept.frame.seq)
    }

    /// How many of the numbers in `from..=to` on `slot`'s stream the ring
    /// holds no frame for.
    fn unringed_between(&self, slot: &SessionSlot, from: u64, to: u64) -> u64 {
        let Some(numbers) = self.unringed.get(slot) else {
            return 0;
        };
        u64::try_from(numbers.iter().filter(|number| (from..=to).contains(number)).count())
            .unwrap_or(u64::MAX)
    }

    /// The number `slot`'s next frame will be minted under.
    ///
    /// A seat that has emitted nothing - and one whose last frame opened a new
    /// epoch - is at 1, so this never answers 0: a client reads it as the
    /// number to expect, and 0 would have it expect a frame that cannot exist.
    fn next_seq(&self, slot: &SessionSlot) -> u64 {
        self.sequences.get(slot).copied().unwrap_or(0) + 1
    }

    /// Whether `slot`'s latest epoch-opening seal carried rows.
    fn epoch_opened_with_rows(&self, slot: &SessionSlot) -> bool {
        self.sealed_with_rows.contains(slot)
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
    /// it. A stream carries what is emitted after this call and nothing
    /// before it; a subscriber reads what it missed from the subject's
    /// snapshot instead.
    pub(crate) fn subscribe(&self, role: SubscriberRole) -> mpsc::UnboundedReceiver<SeatUpdate> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.shared.lock().subscribers.push(Registration { tx, role });
        rx
    }

    /// Deliver `update` to every subscriber, and report whether one took
    /// it. `false` is the "no receiver" signal the single-take channel
    /// gave, which `spawn::try_emit` and `SessionTask::emit` log on.
    ///
    /// The last subscriber is taken out of the registry to receive
    /// `update` itself while the rest are sent a copy, so the common
    /// one-subscriber case clones nothing. It goes back only if it took
    /// the update, which is also where a dead one leaves.
    pub(crate) fn send(&self, update: SessionUpdate) -> bool {
        self.deliver(update, None)
    }

    /// The number `slot`'s next frame will be minted under.
    ///
    /// The read behind a seat's reconnect answer: the occupant names the
    /// epoch and this names where its stream stands, so a client that has
    /// just read the seat knows exactly what to expect next.
    pub(crate) fn next_seq(&self, slot: &SessionSlot) -> u64 {
        self.shared.lock().next_seq(slot)
    }

    /// The frames of `slot`'s window numbered in `from..=to`, oldest first,
    /// for a client that read a gap in them.
    pub(crate) fn frames_between(&self, slot: &SessionSlot, from: u64, to: u64) -> Vec<SeatUpdate> {
        self.shared.lock().frames_between(slot, from, to)
    }

    /// The oldest number `slot`'s window still holds: what tells a range it
    /// can answer from memory from one it cannot.
    pub(crate) fn oldest_kept(&self, slot: &SessionSlot) -> Option<u64> {
        self.shared.lock().oldest_kept(slot)
    }

    /// How many numbers in `from..=to` on `slot`'s stream no window can hold.
    pub(crate) fn unringed_between(&self, slot: &SessionSlot, from: u64, to: u64) -> u64 {
        self.shared.lock().unringed_between(slot, from, to)
    }

    /// Whether `slot`'s latest epoch-opening seal carried rows, which no ring
    /// holds.
    pub(crate) fn epoch_opened_with_rows(&self, slot: &SessionSlot) -> bool {
        self.shared.lock().epoch_opened_with_rows(slot)
    }

    /// Drive the ring's cap down, so a test reaches the transcript fallback
    /// without emitting a window's worth of frames.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn set_ring_cap(&self, cap: usize) {
        self.shared.lock().ring_cap = cap;
    }

    /// Drive the ring's byte ceiling down, so a test reaches it with frames a
    /// fixture can build rather than with a giant tree.
    #[cfg(test)]
    pub(crate) fn set_ring_cap_bytes(&self, bytes: usize) {
        self.shared.lock().ring_cap_bytes = bytes;
    }

    /// What `slot`'s ring weighs, for the tests that pin its ceiling.
    #[cfg(test)]
    pub(crate) fn ring_bytes(&self, slot: &SessionSlot) -> usize {
        self.shared.lock().ring_bytes.get(slot).copied().unwrap_or(0)
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
        // **Weighed before the lock.** A chat frame's charge is its JSON, and
        // paying that inside the fan-out's guard would serialize every seat's
        // stream behind it.
        let charge = frame_bytes(&update);
        let mut shared = self.shared.lock();
        let update = shared.numbered(update, charge);
        let Some(tail) = shared.subscribers.pop() else {
            return false;
        };
        let counts = |role: SubscriberRole| required.is_none_or(|want| want == role);
        let mut took_it = false;
        shared.subscribers.retain(|subscriber| {
            let live = subscriber
                .tx
                .send(SeatUpdate { seq: update.seq, update: Arc::clone(&update.update) })
                .is_ok();
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

    /// A `FileIndexChanged` of `entries` files, for the tests that drive the
    /// ring's byte ceiling: the variant that pins a whole tree per frame.
    fn a_big_index(key: &SessionSlot, entries: usize) -> SessionUpdate {
        let mut index = crate::file_index::FileIndex::default();
        for nth in 0..entries {
            let rel = format!("src/deeply/nested/module/number-{nth:04}/file-{nth:04}.rs");
            index.entries.insert(
                rel.clone(),
                crate::file_index::FileCandidate {
                    rel_path: rel.clone(),
                    rel_path_lower: rel.to_lowercase(),
                    basename_lower: rel.to_lowercase(),
                    depth: 5,
                },
            );
        }
        SessionUpdate::FileIndexChanged { key: key.clone(), index: Arc::new(index) }
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
        match std::sync::Arc::unwrap_or_clone(frame.update) {
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
        assert_eq!(next(&mut late, "late"), "after", "the late subscriber sees only the later one");
        assert!(late.try_recv().is_err(), "the late subscriber carries nothing from before it");
    }

    /// **A delivery hands out the same update, not a copy of it.** The ring
    /// and every subscriber hold one `Arc`: a payload nobody mutates is
    /// shared, and a frame a subscriber filters out costs it a pointer rather
    /// than a clone of a window.
    #[test]
    fn a_delivery_shares_one_update_with_the_ring_and_every_subscriber() {
        let fanout = UpdateFanout::default();
        let mut first = fanout.subscribe(SubscriberRole::Answering);
        let mut second = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(fanout.send(a_turn_ends(&seat)), "both subscribers receive it");
        let one = first.try_recv().expect("the first subscriber's frame");
        let two = second.try_recv().expect("the second subscriber's frame");
        assert!(
            Arc::ptr_eq(&one.update, &two.update),
            "the two subscribers were handed one payload, not two",
        );
        let ringed = fanout.frames_between(&seat, 1, 1);
        assert_eq!(ringed.len(), 1, "the ring kept the frame");
        assert!(
            Arc::ptr_eq(&one.update, &ringed[0].update),
            "and the ring holds the same payload rather than a second copy",
        );
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

    /// **An occupant change leaves the ring holding only the new epoch.** The
    /// frame that OPENS an epoch carries the count of the run it ends, so a
    /// ring that kept it would report a front number the new run cannot answer
    /// for, and the old number would collide with a new one as the count climbs
    /// back to it. Repairs would fall to the transcript for frames the window
    /// still holds.
    #[test]
    fn an_occupant_change_leaves_the_ring_on_the_new_epoch_alone() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(fanout.send(occupant_change(&seat)), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");

        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the old run's first frame");
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "and its second");
        assert_eq!(
            next_seq(&mut rx, "seat"),
            Some(3),
            "the occupant frame is numbered on the counter it ends",
        );
        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "and the new epoch opens at 1");
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "counting on");

        assert_eq!(
            fanout.oldest_kept(&seat),
            Some(1),
            "the ring's front is the new epoch's first frame, not the frame that ended the old run",
        );
        assert_eq!(
            fanout.frames_between(&seat, 1, 3).iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![Some(1), Some(2)],
            "and a range reaches only the new count's frames",
        );
    }

    /// **A release advances the seat's counter like any other frame.** Keeping
    /// the epoch frame out of the ring is where the branch began, and leaving
    /// the counter behind with it mints the released frame's number twice: the
    /// frame after it repeats a number a client has already drawn and is
    /// skipped.
    #[test]
    fn a_release_advances_the_counter_it_is_numbered_on() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(
            fanout.send(SessionUpdate::Releasing { key: seat.clone() }),
            "a subscribed fan-out delivers"
        );
        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");

        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the first frame");
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "the release");
        assert_eq!(
            next_seq(&mut rx, "seat"),
            Some(3),
            "and the frame after it, rather than a number minted twice",
        );
    }

    /// **The number a release is numbered under is one no ring will ever
    /// hold.** A reader measuring whether a copy covers a stretch has to leave
    /// that number out of the count, or every seat that closes reads as a copy
    /// behind its own count - ready to be repaired from a window that cannot
    /// hold the frame in question. A new epoch forgets the old release, because
    /// the numbers after a seal are minted from 1 again.
    #[test]
    fn the_release_is_the_number_the_ring_cannot_serve() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(
            fanout.send(SessionUpdate::Releasing { key: seat.clone() }),
            "a subscribed fan-out delivers"
        );
        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the row");
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "and the release after it");

        assert_eq!(
            fanout.unringed_between(&seat, 1, 1),
            0,
            "a row the window holds is not counted among the numbers it cannot serve",
        );
        assert_eq!(
            fanout.unringed_between(&seat, 1, 2),
            1,
            "and the release beside it is, because no window can hold it",
        );

        assert!(fanout.send(occupant_change(&seat)), "a subscribed fan-out delivers");
        for _ in 0..3 {
            assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        }
        assert_eq!(
            fanout.unringed_between(&seat, 1, 3),
            0,
            "the new epoch mints from 1 again, so the old release's number is forgotten",
        );
    }

    /// **A release keeps the frames before it reachable.** A seat a connection
    /// re-opens after its session ended is answered from its record, and that
    /// read can be behind frames the release ends - the ring is what fills it.
    /// It is shed at the next epoch's seal, where the numbers start over, not
    /// here.
    #[test]
    fn a_release_keeps_the_frames_before_it_reachable() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");

        assert!(fanout.send(a_turn_ends(&seat)), "a subscribed fan-out delivers");
        assert!(
            fanout.send(SessionUpdate::Releasing { key: seat.clone() }),
            "a subscribed fan-out delivers"
        );
        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the row");
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "and the release after it");

        assert_eq!(
            fanout.frames_between(&seat, 1, 1).iter().map(|frame| frame.seq).collect::<Vec<_>>(),
            vec![Some(1)],
            "the frame before the release is still there for a copy that is behind it",
        );
        assert_eq!(
            fanout.unringed_between(&seat, 1, 2),
            1,
            "and the release itself is still a number no window holds",
        );
    }

    /// **A released seat's ring answers its byte ceiling.** The frames before
    /// a release are kept so a re-open can fill its copy, but a seat that
    /// never gets a new occupant must not pin them for the process's life: the
    /// ceiling drops the oldest, and the newest frame survives even alone over
    /// it - the same rule the conversation window keeps for the turn it cuts
    /// inside.
    #[test]
    fn a_released_seat_keeps_only_what_its_byte_ceiling_holds() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");
        // Two of these frames fit; the ceiling is what sheds them, not a
        // count a fixture had to reach.
        fanout.set_ring_cap_bytes(10_000);

        assert!(fanout.send(a_big_index(&seat, 20)), "a subscribed fan-out delivers");
        assert!(
            fanout.send(SessionUpdate::Releasing { key: seat.clone() }),
            "a subscribed fan-out delivers"
        );
        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the frame before the release");
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "and the release");

        for _ in 0..3 {
            assert!(fanout.send(a_big_index(&seat, 20)), "a subscribed fan-out delivers");
        }

        assert!(
            fanout.ring_bytes(&seat) <= 10_000,
            "the ring answers its byte ceiling: {} bytes",
            fanout.ring_bytes(&seat),
        );
        assert!(
            fanout.frames_between(&seat, 1, 1).is_empty(),
            "and the frame before the release is shed once the ring outgrows it",
        );
        assert!(
            !fanout.frames_between(&seat, 3, 5).is_empty(),
            "while the newest frames are what a repair is answered from",
        );
    }

    /// **A frame over the ceiling survives alone.** The newest frame is what a
    /// repair is answered from, so the cut stops at the last one - the same
    /// rule the conversation window keeps for the turn it cuts inside - and
    /// the ring states its true weight rather than hiding it.
    #[test]
    fn a_frame_over_the_ceiling_survives_alone() {
        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");
        fanout.set_ring_cap_bytes(10_000);

        // About 18 KB, over the ceiling on its own.
        assert!(fanout.send(a_big_index(&seat, 60)), "a subscribed fan-out delivers");
        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the frame");
        assert_eq!(
            fanout.frames_between(&seat, 1, 1).len(),
            1,
            "the newest frame survives even alone over the ceiling",
        );
        assert!(
            fanout.ring_bytes(&seat) > 10_000,
            "and the ring states its true weight: {} bytes",
            fanout.ring_bytes(&seat),
        );
    }

    /// **A call output and a card tail are charged by their payload.** Both
    /// grow with what a person reads - a command's tail, an instance's calls -
    /// and a ceiling that counted them as a frame's fixed weight would let a
    /// heavy seat pin its whole window.
    #[test]
    fn a_call_output_and_a_card_tail_are_charged_by_their_payload() {
        use forge_primitives::runtime::{SubagentCall, SubagentCallStatus, SubagentCard};

        let fanout = UpdateFanout::default();
        let mut rx = fanout.subscribe(SubscriberRole::Answering);
        let seat = seat("lead");
        let line = "x".repeat(4_000);

        assert!(
            fanout.send(SessionUpdate::CallOutput {
                key: seat.clone(),
                call_id: "tu-1".to_owned(),
                output: forge_primitives::CallOutput::Lines(vec![
                    line.clone(),
                    line.clone(),
                    line.clone(),
                ]),
            }),
            "a subscribed fan-out delivers",
        );
        assert_eq!(next_seq(&mut rx, "seat"), Some(1), "the call output");
        let output_bytes = fanout.ring_bytes(&seat);
        assert!(
            output_bytes > 12_000,
            "three 4 KB lines are charged against the ceiling: {output_bytes} bytes",
        );

        let card = SubagentCard {
            name: "an instance".to_owned(),
            dispatch_id: "tu-2".to_owned(),
            agent_type: None,
            running: true,
            failed: false,
            backgrounded: false,
            ended_at_ms: None,
            calls: 3,
            tail: (0..3)
                .map(|_| SubagentCall {
                    name: "Bash".to_owned(),
                    title: line.clone(),
                    status: SubagentCallStatus::Completed,
                })
                .collect(),
            usage: None,
        };
        assert!(
            fanout
                .send(SessionUpdate::SubagentCardsChanged { key: seat.clone(), cards: vec![card] }),
            "a subscribed fan-out delivers",
        );
        assert_eq!(next_seq(&mut rx, "seat"), Some(2), "and the cards after it");
        let both = fanout.ring_bytes(&seat);
        assert!(
            both > output_bytes + 12_000,
            "the card tail's calls are charged too: {both} bytes over the {output_bytes} before \
             it",
        );
    }

    /// The boot backlog keeps the numbers its frames were minted with, so a
    /// client joining at the first subscribe has no hole at the join.
    #[test]
    fn a_frame_that_reached_nobody_still_advances_its_seats_number() {
        let fanout = UpdateFanout::default();
        let seat = seat("lead");
        assert!(!fanout.send(a_turn_ends(&seat)), "nothing has attached to take it");
        assert!(!fanout.send(a_turn_ends(&seat)), "nothing has attached to take it");

        let mut first = fanout.subscribe(SubscriberRole::Answering);
        assert!(fanout.send(a_turn_ends(&seat)), "the subscriber is attached");

        assert_eq!(
            next_seq(&mut first, "first"),
            Some(3),
            "the delivered frame continues the count rather than reusing a number",
        );
    }
}
