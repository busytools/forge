//! One seat's conversation, held by the transport and kept up to date.
//!
//! **The read is what costs.** Measured on a 68.9 MB transcript (13,451
//! messages), one request spends 202 ms reading and parsing the file, 18 ms
//! folding it into turns and 20 ms encoding those turns - so a fix caching
//! only the fold removes 8% of the cost it was written for. The transport
//! holds the read's own output and shares it: a page slices a window out of
//! it, and a subscribe encodes the newest of it.
//!
//! **A seat's history has two sources, and the seed never overwrites the
//! stream's.** The first is the session task, which produces the conversation:
//! the fold sees `Connected` for every seat that starts or resumes while the
//! transport runs, so the frames and the copy they build have one producer and
//! one order.
//!
//! The second is the seat's own transcript, read when the transport holds
//! nothing and a page opens on it - a sleeping seat has no live copy, and
//! waiting for the spawn that would carry the same history back is the wait
//! this exists to remove (`transport::wire::seed_from_transcript`). It only
//! ever inserts: a copy the stream inserted while the file was being read is
//! kept, and a copy of another occupant is waited for rather than replaced. A
//! transcript is a row subset - it carries no `Result` frames - so a seed over
//! a live copy would drop frames the stream had already delivered.
//!
//! **That is what makes a buffer unnecessary rather than merely deleted.** A
//! frame emitted before a seat is held finds no conversation and is dropped -
//! the precondition is that the fold is attached before the first
//! `Connected`, which the transport's own test asserts. A frame emitted
//! after is applied to what the seed left.
//!
//! **What is released is the front, and no more of it than the cap.** A page
//! cursor names a message in the conversation's own numbering rather than in
//! the held list's, so a cursor a drop has passed is answered as what it is -
//! the floor, with nothing above it - rather than as a window the client did
//! not ask for. The seat's copy is therefore kept for the seat's life, and
//! what that costs is bounded by the cap below rather than by the transcript.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use forge_primitives::{Message, SessionSlot};
// The window this copy is kept in. The fan-out's ring is sized from the same
// caps, because a ring exists to fill a copy that is behind; the ceilings' why,
// and what the drop costs, are stated there.
use forge_workspace::conversation_turns::{TurnScan, opens_for_debt};
use forge_workspace::conversation_window::Window;
// The delivered liveness rule, folded off the frames this copy sees - the same
// one the client's `inFlightOf` keeps and the core folds into a seat's own
// turn state.
use forge_workspace::{TurnLiveness, liveness_of};

use crate::SessionUpdate;
use crate::transcript::Rendered;

/// One seat's conversation: the messages, where its turns sit and how many
/// times it has compacted.
///
/// All three come out of one read and one fold, so a page cut on the fold's
/// boundaries and a record built from `messages` cannot disagree about where a
/// turn begins, and the count cannot describe a different conversation than
/// the one held.
pub struct Conversation {
    window: Window,
    /// The fold's boundaries and task endings, which a page is cut on.
    ///
    /// **Nothing but those is held**, because the transport draws nothing: a
    /// client folds the frames a page hands it, so the boundaries are what a
    /// page here is cut on and the endings are what its turns carry.
    rendered: Rendered,
    compaction_count: u32,
    /// The messages the cap has dropped off the front, ever.
    ///
    /// **A page cursor is measured against this rather than against the held
    /// list.** A cursor names a message in the conversation's own numbering,
    /// which a drop does not move, so a cursor that outlived one still names
    /// the message it meant - and one naming a message the drop has taken is
    /// answered with the floor rather than with whatever now sits at its old
    /// place in the list.
    dropped: usize,
    /// The newest frame number this copy reflects, which a reseed resets.
    ///
    /// **An answer's number is taken from it**, because a reseed lands before
    /// the fold's reset mark and a pre-seal number would have the client skip
    /// the new run's first frames (#2443).
    watermark: u64,
    /// The messages moved since the fold last ran.
    ///
    /// **Memoized rather than made incremental**, because making it
    /// incremental means deciding where a turn begins from the appended
    /// message alone - a second turn-boundary rule, which is the drift
    /// `turn_ranges`' own doc exists to prevent. One rule in one place, and
    /// it runs when the messages changed rather than once per request.
    ///
    /// **And it never runs under this lock.** The socket folds the core's
    /// stream in ONE task for every seat, so a fold holding a seat's lock
    /// stalls update delivery for every client on every seat.
    /// [`Held::fold`] is where it runs instead.
    dirty: bool,
    /// The messages are out being folded, so what is here is half a state.
    ///
    /// **A reader must wait rather than look.** A fold takes the messages out
    /// in O(1), renders them off the lock, and puts them back with the spans,
    /// so between those two the seat reads as no messages against the
    /// PREVIOUS fold's boundaries, and a page sliced on those walks off the
    /// end of an empty list. Two requests on one seat is the ordinary case
    /// for that, not an exotic one.
    folding: bool,
    /// The uuids of the delivery rows this transport forged.
    ///
    /// **A forged row is a frame no transcript has**, and nothing about its
    /// shape says so - it is a user row like the prompt it draws. Only the
    /// site that forged it knows, which is why the set lives here rather than
    /// in the row rule. [`Conversation::has_a_row`] is what reads it: a walk
    /// below the window names this seat's oldest row TO THE FILE, and naming
    /// one the file never wrote answers the read nothing - which reads as the
    /// end of the history.
    forged: std::collections::HashSet<String>,
    /// The occupant this copy was last seeded for.
    ///
    /// **What tells a copy of the previous run from the current one.** A seal
    /// is minted by the fanout and applied by the fold, and a read landing
    /// between the two would otherwise pair the new epoch's count with the old
    /// run's rows: a copy whose occupant is not the seat's is no copy at all.
    occupant: Option<forge_primitives::SessionId>,
    /// Whether the seat's turn is in flight right now, folded off the frames
    /// this copy has seen (`liveness_of` - the delivered rule: the model's
    /// own frames open a turn, its `Result` or error closes it).
    ///
    /// **The fold's caller carries this**, because the frame that would say it
    /// directly (`session_state_changed`) is never sent: the fold reads it as
    /// "the open turn the frames describe is live", which is what keeps a
    /// mid-call delivery from being cut off from a result that has not
    /// streamed yet (#2379).
    live: bool,
    /// Where the stream opens turns, folded over every frame this copy has
    /// seen rather than over the window: a cut moves the window's front, not
    /// the rule that says which frame opens a turn.
    scan: TurnScan,
    /// The thinking growth a cut took out of the turn open at the window's
    /// end, which the page serving that turn carries back into its first
    /// counter.
    ///
    /// **It never crosses a turn.** A `Result` closes the turn it belonged to,
    /// and a frame that opens a turn starts the next one with nothing carried.
    dropped_thinking: i64,
}

impl Conversation {
    /// A conversation from the history a connect or a resume carried.
    ///
    /// **It does not fold.** The spans come from the next
    /// [`Held::fold`], which runs on a blocking task; folding here would run
    /// an 18 ms render on whatever task built this, and that task is the
    /// socket's single stream folder.
    ///
    /// `carried` is the floor `history` arrives with: the messages the read it
    /// came from left behind a window that was taken before the history
    /// crossed (see [`Conversation::seed`]), so a window handed over on its
    /// own still tells the whole conversation's numbering.
    pub fn new(messages: Vec<Message>, compaction_count: u32, carried: usize) -> Self {
        // **The history is converted in place before the cut**, so what is
        // weighed and kept is the shape this copy holds rather than the shape
        // the wire carried; and the cut is still what bounds what the copy
        // costs, so a resume's transient is the history it handed over plus a
        // window rather than two transcripts.
        //
        // The liveness is read before the cut, off the same frames: the
        // seed's own ladder is what a copy materialised mid-turn reads from
        // before any frame arrives.
        let live = live_of_seed(&messages);
        let blocks = as_blocks(messages);
        // **The scan is primed over the whole history**, not the window: its
        // state is what the frames before a turn left, and a cut takes frames
        // the next opening would otherwise be read under.
        let mut scan = TurnScan::default();
        for message in &blocks {
            scan.opens(message);
        }
        let (window, shed) = Window::tail_of(&blocks);
        let dropped = carried.saturating_add(shed.frames);
        Self {
            window,
            rendered: Rendered { turns: Vec::new(), endings: HashMap::new() },
            compaction_count,
            dropped,
            watermark: 0,
            dirty: true,
            folding: false,
            forged: std::collections::HashSet::new(),
            occupant: None,
            live,
            scan,
            // A settled turn still draws its report, so a seed keeps the debt
            // its own cut produced whether or not the turn is still open.
            dropped_thinking: shed.thinking,
        }
    }

    /// The conversation a seat with nothing behind it answers as.
    pub fn empty() -> Self {
        Self::new(Vec::new(), 0, 0)
    }

    /// The occupant this copy was last seeded for.
    pub fn occupant(&self) -> Option<&forge_primitives::SessionId> {
        self.occupant.as_ref()
    }

    /// Say which occupant this copy is for.
    pub fn set_occupant(&mut self, occupant: Option<forge_primitives::SessionId>) {
        self.occupant = occupant;
    }

    /// The newest frame number this copy reflects.
    pub fn watermark(&self) -> u64 {
        self.watermark
    }

    /// Say how far this copy is current, which a reseed resets and an applied
    /// frame advances.
    pub fn set_watermark(&mut self, seq: u64) {
        self.watermark = seq;
    }

    /// Replace the whole conversation, which is what a connect, a resume or a
    /// `/new` says: the history it carries is the truth about this seat.
    ///
    /// **It marks the fold rather than running it**, for the reason
    /// [`Conversation::new`] gives: this is reached from the stream fold, and
    /// a render there stalls every seat rather than this one.
    ///
    /// `carried` is the floor the history arrives with - the messages the read
    /// it came from left behind a window taken before it crossed, which
    /// `Connected` and `SessionReplaced` carry beside the history they hand
    /// over (a resume's read is windowed at the emit). Without it a seat
    /// reseeded from a window reports the window's own front as the
    /// conversation's, and a client's walk below it stops there instead of
    /// crossing into the transcript.
    ///
    /// **The numbering restarts**, because a connect or a resume carries a
    /// transcript READ - a row subset, not this copy's frame sequence - so an
    /// index carried over it would be off by however far the two diverge. A
    /// cursor from before resolves by its own number again and is answered
    /// from the floor when that number falls below the front the new window
    /// starts at.
    pub fn seed(&mut self, messages: Vec<Message>, compaction_count: u32, carried: usize) {
        // **Converted before the cut.** The copy a seat already holds went
        // through `as_blocks` at its own seed, so a `<task-notification>` row
        // and the block it converts to are one frame. The cut then runs over
        // the converted history, so the window it keeps is the one weighed.
        let live = live_of_seed(&messages);
        let blocks = as_blocks(messages);
        let mut scan = TurnScan::default();
        for message in &blocks {
            scan.opens(message);
        }
        let (kept, shed) = Window::tail_of(&blocks);
        self.live = live;
        self.window = kept;
        self.scan = scan;
        self.dropped_thinking = shed.thinking;
        // **The boundaries go with the messages they describe.** The fold's
        // turns name a prefix of the messages, and a reader that saw the new
        // list under the old boundaries would slice off the end of it.
        // Clearing them makes the pair consistent at every instant; the next
        // fold rebuilds them.
        self.rendered.turns.clear();
        self.rendered.endings.clear();
        self.dropped = carried.saturating_add(shed.frames);
        // A frame whose id is no longer held cannot be met again, so the set
        // that answers "did forge forge this" holds only what the seat keeps.
        self.forged.retain(|id| self.window.messages().iter().any(|m| frame_id(m) == Some(id)));
        self.compaction_count = compaction_count;
        self.watermark = 0;
        self.dirty = true;
    }

    /// One frame the session emitted.
    pub fn append(&mut self, message: Message) {
        self.push(message);
    }

    /// One row this transport forged for display, which no transcript holds.
    ///
    /// **Forged rows are frames the file counts no row for**, and nothing
    /// about their shape says so - a delivery row is a user row like the
    /// prompt it draws. So the seat keeps their ids, and [`Self::has_a_row`]
    /// answers with them: a walk below the floor names the seat's oldest row
    /// to the file, and one the file never wrote answers nothing.
    pub fn append_forged(&mut self, message: Message) {
        if let Some(id) = frame_id(&message) {
            self.forged.insert(id.to_owned());
        }
        self.push(message);
    }

    /// Whether a transcript row carries `message`: the kinds the row rule
    /// reads, and nothing this transport forged for display.
    ///
    /// **The seat's own answer, because half of it is the seat's.** The rule
    /// knows the kinds a file writes; only the seat knows which of its own
    /// rows it minted, and a forged row is a user row like the prompt it
    /// draws. A walk below the window needs both halves: naming the file a row
    /// it does not carry answers the read nothing, which a client draws as the
    /// end of the history.
    pub fn has_a_row(&self, message: &Message) -> bool {
        forge_workspace::has_a_transcript_row(message)
            && !frame_id(message).is_some_and(|id| self.forged.contains(id))
    }

    fn push(&mut self, message: Message) {
        // **The scan sees every frame once, in stream order**, so its answer
        // stays exact across a fold (which empties the window, never the
        // scan). A queued prompt opens a turn in the fold the window serves,
        // so it opens one for the debt too.
        let opened = opens_for_debt(&mut self.scan, &message);
        // **A boundary that arrives moves the count**, the same way the
        // session task's own count does. Without this the record reports the
        // value its last SEED carried, which is stale for every compaction
        // since - and the header reads the count while the boundary draws its
        // own row.
        if matches!(message, Message::CompactBoundary { .. }) {
            self.compaction_count = self.compaction_count.saturating_add(1);
        }
        // Every frame's own claim about the turn folds in here, the same walk
        // a seat's `DomainSession` makes: the model's frames open a turn, its
        // `Result` or error closes it, anything else says nothing.
        if let Some(said) = liveness_of(&message) {
            self.live = said == TurnLiveness::Working;
        }
        // **Before the push, so the drop pays for the room it makes.** After
        // it, this one frame is what tips the store into a doubling the drop
        // is about to throw away. Both ceilings are the window's own.
        let shed = self.window.push(message, opened);
        // **Only a new turn clears the debt.** A settled turn is still the
        // newest turn a page serves, and its report draws the figure - so the
        // debt survives a `Result` and an `Error` alike, the two turn ends
        // `liveness_of` names, and only a frame that opens the next turn
        // starts with nothing carried.
        if opened {
            self.dropped_thinking = 0;
        } else {
            self.dropped_thinking = self.dropped_thinking.saturating_add(shed.thinking);
        }
        if shed.frames > 0 {
            self.dropped = self.dropped.saturating_add(shed.frames);
            // A forged row the seat no longer holds cannot be met again, so
            // the set that answers "did forge forge this" keeps only what it
            // keeps.
            let held: std::collections::HashSet<&str> =
                self.window.messages().iter().filter_map(frame_id).collect();
            self.forged.retain(|id| held.contains(id.as_str()));
            // The same reason a seed clears them: the boundaries name message
            // indices, and the drop has just moved every one of them.
            self.rendered.turns.clear();
            self.rendered.endings.clear();
        }
        self.dirty = true;
    }

    /// The messages if the fold is behind them, taken in O(1).
    fn take_dirty(&mut self) -> Option<Window> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        self.folding = true;
        Some(std::mem::take(&mut self.window))
    }

    /// Put a fold back, with whatever arrived while it ran appended.
    ///
    /// `complete` is false when the render did not reach its end, which is the
    /// guard's path: the messages are back but the boundaries installed with
    /// them describe nothing.
    fn merge_fold(&mut self, mut folded: Window, rendered: Rendered, complete: bool) {
        let folded_len = folded.len();
        folded.append(&mut self.window);
        self.window = folded;
        self.rendered = rendered;
        self.folding = false;
        // The tail arrived while the fold ran and is not in those spans, and a
        // fold that did not finish has no spans at all - either way the next
        // reader folds again rather than cutting a turn on a list that has
        // moved.
        if !complete || self.window.len() != folded_len {
            self.dirty = true;
        }
    }

    pub fn messages(&self) -> &[Message] {
        self.window.messages()
    }

    pub fn rendered(&self) -> &Rendered {
        &self.rendered
    }

    pub fn compaction_count(&self) -> u32 {
        self.compaction_count
    }

    /// Set the count a copy reports, for a copy built from a window rather
    /// than seeded: the frames in the window carry only the boundaries after
    /// its front, and the stream's own count is what the record draws.
    pub fn set_compaction_count(&mut self, count: u32) {
        self.compaction_count = count;
    }

    /// How many messages the cap has dropped off the front, which is what
    /// turns a held index into the one a page's cursor carries.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// The thinking growth a cut took out of the turn open at the window's
    /// end, which a page of that turn carries back into its first counter.
    pub fn dropped_thinking(&self) -> i64 {
        self.dropped_thinking
    }

    /// What this copy weighs, as the total budget counts it.
    pub fn bytes(&self) -> usize {
        self.window.bytes()
    }
}

/// The id a frame carries, for the frames that carry one.
pub(crate) fn frame_id(message: &Message) -> Option<&str> {
    match message {
        Message::User { uuid, .. } | Message::Assistant { uuid, .. } => uuid.as_deref(),
        Message::StopHookSummary { uuid, .. } | Message::CompactBoundary { uuid, .. } => Some(uuid),
        _ => None,
    }
}

/// A history as the CLI wrote it, with its task notices in the shape a view
/// reads an ending from.
///
/// **The read converts these and the seed must too.** `notices_as_blocks` is
/// applied where a conversation is READ off disk, and a held copy seeded from
/// a connect takes the history as the WIRE carries it - so a
/// resumed seat would serve a `<task-notification>` row as the reader's own
/// words, with the ending unread and, for a notice in the same turn as its
/// call, nothing ending that call at all. Idempotent, so a history that came
/// through the read is unchanged.
fn as_blocks(messages: Vec<Message>) -> Vec<Message> {
    crate::transcript::notices_as_blocks(messages)
}

/// The delivered ladder folded over a seed's own frames: the last frame that
/// says anything about the turn wins, the way every frame appended later
/// folds in (see [`Conversation::live`]).
fn live_of_seed(messages: &[Message]) -> bool {
    messages.iter().filter_map(liveness_of).next_back() == Some(TurnLiveness::Working)
}

/// One seat's conversation, and the fold that may be running over it.
///
/// **Two locks, because a fold must not be held against the stream and a
/// reader must not see half of one.** A fold takes the messages out in O(1)
/// under the mutex, renders them outside it - so the socket's single stream
/// folder never waits on a render - and puts them back under the mutex with
/// the spans. Between those two the seat holds no messages and the PREVIOUS
/// fold's boundaries, and a page sliced on those walks off the end of an
/// empty list.
///
/// So the mutex is paired with a condvar: a reader waits out a fold in
/// progress rather than looking at it. The wait is on the reader's own
/// blocking task, and it is bounded by one render.
pub struct Held {
    conversation: Mutex<Conversation>,
    folded: Condvar,
    /// What this copy weighs, kept beside the conversation so the total
    /// budget reads it without locking the seat.
    bytes: std::sync::atomic::AtomicUsize,
    /// When this seat was last touched, for the total budget's LRU.
    ///
    /// **A monotonic counter rather than a clock**, so two seats touched in
    /// the same instant still order. The counter lives on [`Conversations`].
    last_touch: std::sync::atomic::AtomicU64,
}

impl Held {
    pub fn new(conversation: Conversation) -> Self {
        let bytes = conversation.bytes();
        Self {
            conversation: Mutex::new(conversation),
            folded: Condvar::new(),
            bytes: std::sync::atomic::AtomicUsize::new(bytes),
            last_touch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// What this copy weighs, without locking the seat.
    pub fn bytes(&self) -> usize {
        self.bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// When this seat was last touched, without locking the seat.
    pub fn last_touch(&self) -> u64 {
        self.last_touch.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Say this seat was just touched, at `seq`.
    pub fn touch(&self, seq: u64) {
        self.last_touch.store(seq, std::sync::atomic::Ordering::Relaxed);
    }

    /// Re-read the seat's weight from its conversation, answering the delta
    /// against what the budget last counted.
    fn reweigh(&self, conversation: &Conversation) -> isize {
        let new = conversation.bytes();
        let old = self.bytes.swap(new, std::sync::atomic::Ordering::Relaxed);
        new.cast_signed() - old.cast_signed()
    }

    /// Whether this copy belongs to the seat's current occupant.
    ///
    /// **A copy with no occupant matches anything, and so does a header
    /// naming nobody**: a transcript that named no session seeds a copy with
    /// no occupant, and a sleeping seat's header names no run - neither is
    /// discarded for lacking a fact nothing ever recorded.
    pub fn holds_occupant(&self, occupant: Option<&forge_primitives::SessionId>) -> bool {
        match (self.lock().occupant.as_ref(), occupant) {
            (Some(held), Some(current)) => held == current,
            _ => true,
        }
    }

    /// The conversation, waiting out a fold that is running over it.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Conversation> {
        let mut conversation = self.conversation.lock().unwrap_or_else(PoisonError::into_inner);
        while conversation.folding {
            conversation = self.folded.wait(conversation).unwrap_or_else(PoisonError::into_inner);
        }
        conversation
    }

    /// Put a fold's result back and wake every reader waiting on it.
    fn install(&self, window: Window, rendered: Rendered, complete: bool) {
        let mut conversation = self.conversation.lock().unwrap_or_else(PoisonError::into_inner);
        conversation.merge_fold(window, rendered, complete);
        self.folded.notify_all();
    }

    /// Fold if needed, then read, retrying once if a reseed landed in between.
    ///
    /// **Two acquisitions, and the gap between them is why this retries.** A
    /// `Connected` or a `/resume` can replace the conversation
    /// between the fold and the read, and what a reader then holds is the new
    /// messages against boundaries the fold cleared - one turn covering the
    /// whole conversation rather than a window of them. The retry closes the
    /// common case; a second reseed inside the second window still gets
    /// through, which is stated in the PR rather than claimed away.
    pub fn read<T>(&self, read: impl FnOnce(&Conversation) -> T) -> T {
        for _ in 0..2 {
            self.fold();
            let conversation = self.lock();
            if conversation.dirty {
                drop(conversation);
                continue;
            }
            return read(&conversation);
        }
        read(&self.lock())
    }

    /// Fold if the messages moved, outside the lock and off the reactor.
    ///
    /// The caller is a blocking task that has nothing else to do with the
    /// conversation until this returns.
    pub fn fold(&self) {
        let Some(taken) = Folding::begin(self) else {
            return;
        };
        let rendered = crate::transcript::render(taken.messages(), taken.live());
        taken.finish(rendered);
    }
}

/// The messages a fold has taken out, and the promise that they go back.
///
/// **This is a guard rather than a straight-line take-and-restore because
/// the render between them is a place a panic can land**, and a panic there
/// with no guard is out of all proportion to its trigger: the messages are
/// dropped, `folding` stays set, and `notify_all` is never reached - so every
/// reader of the seat parks in [`Held::lock`] on a notification that never
/// comes, and the seat is wedged for the process's life. Through
/// [`Conversations::apply`] the same lock is the socket's single stream
/// folder, so it is every client on every seat that stops being delivered to
/// rather than one page.
///
/// With the guard, a panic costs one bad fold: the messages go back, the
/// boundaries are left for the next fold to rebuild, and the readers wake.
struct Folding<'a> {
    held: &'a Held,
    window: Option<Window>,
    live: bool,
}

impl<'a> Folding<'a> {
    /// Take the window if a fold is due, or `None` when there is nothing to
    /// do - a fold already running, or nothing moved.
    fn begin(held: &'a Held) -> Option<Self> {
        let mut conversation = held.conversation.lock().unwrap_or_else(PoisonError::into_inner);
        // A fold already running owns the messages; this caller has nothing
        // to do, and waiting here would be waiting on a peer rather than on
        // work it can start.
        if conversation.folding {
            return None;
        }
        let window = conversation.take_dirty()?;
        // Read with the window, under the same lock: the state the fold is
        // handed is the seat's own at the moment its messages went out.
        let live = conversation.live;
        Some(Self { held, window: Some(window), live })
    }

    /// The messages this fold took.
    fn messages(&self) -> &[Message] {
        self.window.as_ref().map_or(&[], Window::messages)
    }

    /// Whether the seat's turn was in flight when the window went out.
    fn live(&self) -> bool {
        self.live
    }

    /// Put the fold back and wake the readers.
    fn finish(mut self, rendered: Rendered) {
        let window = self.window.take().unwrap_or_default();
        self.held.install(window, rendered, true);
    }
}

impl Drop for Folding<'_> {
    /// Unwinding out of the render: the messages go back, and the seat stays
    /// due a fold so the next read rebuilds the boundaries.
    fn drop(&mut self) {
        let Some(window) = self.window.take() else {
            return;
        };
        self.held.install(
            window,
            // No boundaries: the fold did not reach its end, so the next one
            // rebuilds them rather than this leaving half of one behind.
            Rendered { turns: Vec::new(), endings: HashMap::new() },
            false,
        );
    }
}

/// The conversations the transport is holding, one per seat it has heard of.
///
/// **Every live seat, not only the watched ones, and a seat is let go only
/// when the total budget forces it.** A conversation's window is bounded,
/// but nothing bounds the SUM across seats: every seat that opens a
/// conversation keeps its copy for the process's life, so a server with
/// hundreds of seats holds hundreds of windows. A total budget evicts the
/// least recently touched seat when the sum passes it. An evicted seat
/// reopens through the cold read path - its transcript seed - so the only
/// visible difference is that one read.
///
/// What it holds is proportional to what is RUNNING plus the sleeping seats
/// someone has opened: a sleeping seat's copy is seeded from its transcript
/// and kept, which is what makes the read land before any spawn. What one
/// seat costs is the window the ceilings keep rather than its transcript,
/// and the window is the per-seat bound. The total budget is the cross-seat
/// bound.
#[derive(Default)]
pub struct Conversations {
    held: Mutex<HashMap<SessionSlot, Arc<Held>>>,
    /// How many bytes of conversation the held copies may total, or `0` for
    /// no total budget.
    ///
    /// **The per-seat window is bounded; the total across seats is not.** A
    /// seat's copy is capped at its window, but every seat that opens a
    /// conversation keeps its copy for the process's life. This budget bounds
    /// the sum: when the total passes it, the least recently touched seat is
    /// evicted. [`forge_primitives::ServerConfig::conversation_memory_budget_bytes`]
    /// carries the why.
    budget_bytes: usize,
    /// The running total of what the held copies weigh.
    total_bytes: std::sync::atomic::AtomicUsize,
    /// A monotonic touch counter, so two seats touched in one instant still
    /// order for the LRU.
    touch_seq: std::sync::atomic::AtomicU64,
    /// Drops a seat's ring when its held copy is evicted, so the two go
    /// together. `None` in tests and wherever the ring lives elsewhere.
    ring_evictor: Option<Arc<dyn forge_workspace::ConversationEvictor>>,
    /// Fires when a seat's copy is first held or reseeded, so a read waiting
    /// for it to become current knows it moved.
    seeded: tokio::sync::Notify,
    /// The newest number the fold loop has consumed for each seat, in a watch
    /// so an encode can wait on it.
    ///
    /// **What an answer's copy is measured against.** A subscribe's answer
    /// carries the number its seat's stream will continue from, and every
    /// frame numbered below that must be inside the copy it carries - because
    /// the client adopts `seq - 1` and treats a later delivery of anything at
    /// or below it as already drawn. The copy is filled by the fold loop,
    /// which consumes the fanout in a task of its own, so "the frame was
    /// emitted before the counter was read" does not yet mean "the copy has
    /// it": this is where that is known.
    consumed: Mutex<HashMap<SessionSlot, tokio::sync::watch::Sender<u64>>>,
    /// The lock a seat's fill runs under.
    ///
    /// **What makes "applied once" a fact rather than a hope.** The fold loop
    /// and an encode's catch-up can both apply the same frames, and the
    /// watermark between them is read and written by both; the lock spans each
    /// side's check, applies and mark, so a frame crosses exactly once
    /// whichever path got there first.
    fills: Mutex<HashMap<SessionSlot, Arc<std::sync::Mutex<()>>>>,
    /// Seats a read is building a record from right now.
    ///
    /// **A copy is only safe from the budget while a read is using it.** A
    /// read catches its seat up and then asks for the copy; an eviction
    /// landing between the two would drop the copy the read just rebuilt and
    /// the read would seed from a transcript that may lag the frames it just
    /// folded. The guard spans the catch-up and the copy's lookup, so the
    /// budget skips a seat while a reader walks to it.
    readers: Mutex<HashMap<SessionSlot, usize>>,
    /// The ordinal each seat without a held copy was last seen at.
    ///
    /// **What lets a seat with no copy be a victim.** The held map carries a
    /// touch per seat, but a ring-only seat has no entry there, and the
    /// budget's total includes its ring. The ordinal comes from the same
    /// counter as a held seat's touch, so the two compare.
    last_seen: Mutex<HashMap<SessionSlot, u64>>,
    /// The seats whose retained memory the budget dropped.
    ///
    /// **What tells an eviction from a seat that never had a copy.** A dropped
    /// seat's rows below its window are old and the transcript answers for
    /// them, so its next read may rebuild from the window's own front. A seat
    /// nothing ever folded has no such backing: a window that no longer
    /// reaches its first frame is a hole, and the read must refuse it. The
    /// mark is cleared the moment a copy is held again.
    evicted: Mutex<std::collections::HashSet<SessionSlot>>,
    /// A test's hold on the fold loop, so the window above can be driven
    /// rather than raced: while it is set, the loop does not consume.
    #[cfg(any(test, feature = "testing"))]
    fold_held: std::sync::atomic::AtomicBool,
    /// A test's hold on the next transcript seed, so a stream copy that lands
    /// inside the seed's window is driven rather than raced.
    #[cfg(any(test, feature = "testing"))]
    seed_hold: Mutex<Option<Arc<SeedHold>>>,
}

/// Where a held transcript seed parks.
///
/// The seed announces it has arrived and waits to be let go, so a test can
/// put the seat's own `Connected` into that window on purpose.
#[cfg(any(test, feature = "testing"))]
pub struct SeedHold {
    /// Signalled once the seed has parked. A notify with nobody waiting keeps
    /// its permit, so the test may start waiting after the seed arrives.
    pub entered: tokio::sync::Notify,
    /// Taken by the seed once the test lets it go.
    pub release: tokio::sync::Semaphore,
}

#[cfg(any(test, feature = "testing"))]
impl Default for SeedHold {
    fn default() -> Self {
        Self { entered: tokio::sync::Notify::new(), release: tokio::sync::Semaphore::new(0) }
    }
}

/// One read's hold on a seat's copy, released when the read finishes.
pub struct ReadGuard<'a> {
    conversations: &'a Conversations,
    slot: SessionSlot,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        let mut readers =
            self.conversations.readers.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = readers.get_mut(&self.slot) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                readers.remove(&self.slot);
            }
        }
    }
}

impl Conversations {
    pub fn new() -> Self {
        Self::default()
    }

    /// A set of conversations bounded by a total byte budget, whose evicted
    /// seats also drop their ring through `evictor`.
    ///
    /// **`0` means no total budget**, the same as the shipped key's opt-out.
    pub fn with_budget(
        budget_bytes: usize,
        evictor: Option<Arc<dyn forge_workspace::ConversationEvictor>>,
    ) -> Self {
        Self { budget_bytes, ring_evictor: evictor, ..Self::default() }
    }

    /// What the held copies weigh in total.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// What the held copies and the rings weigh together.
    ///
    /// **The budget's actual subject.** The eviction checks this, and a test
    /// reads it to see the sum the budget bounds.
    pub fn total_retained_bytes(&self) -> usize {
        let held = self.total_bytes.load(std::sync::atomic::Ordering::Relaxed);
        let rings = self.ring_evictor.as_ref().map_or(0, |evictor| evictor.total_ring_bytes());
        held.saturating_add(rings)
    }

    /// The budget the held copies are held to, or `0` for none.
    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// Move the running total by `delta`, saturating at zero.
    ///
    /// **A negative delta larger than the total would wrap**, and a wrapped
    /// total reads as a budget already blown, so every seat gets evicted on
    /// the next apply. Saturating keeps the accounting honest when a copy
    /// shrinks more than it grew.
    fn adjust_total(&self, delta: isize) {
        if delta > 0 {
            self.total_bytes.fetch_add(delta.unsigned_abs(), std::sync::atomic::Ordering::Relaxed);
        } else if delta < 0 {
            self.subtract_total(delta.unsigned_abs());
        }
    }

    /// Take `bytes` off the running total, saturating at zero.
    ///
    /// **A compare-and-swap loop rather than `fetch_update`**, which is
    /// deprecated in favour of `try_update`, while `try_update` is newer than
    /// the workspace's MSRV. The loop saturates so a copy that shrank more
    /// than the total claims cannot wrap the total around.
    fn subtract_total(&self, bytes: usize) {
        let mut current = self.total_bytes.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(bytes);
            match self.total_bytes.compare_exchange_weak(
                current,
                next,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Evict least-recently-touched seats until the total is under budget,
    /// never touching `protect`.
    ///
    /// **The seat being served stays**, however old its touch: a read or an
    /// apply is actively using it, and evicting it would answer the caller
    /// with a copy that is already gone. A seat a read is walking to, or one
    /// whose fill is running, stays for the same reason.
    ///
    /// **The victim set spans both maps.** The total counts a seat's held copy
    /// and its ring together, so a seat with no copy is a victim too: without
    /// that, its bytes could hold the total over budget while every held seat
    /// was evicted to pay for them.
    fn evict_over_budget(&self, protect: &SessionSlot) {
        if self.budget_bytes == 0 {
            return;
        }
        if self.total_retained_bytes() <= self.budget_bytes {
            return;
        }
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let mut skipped: std::collections::HashSet<SessionSlot> = std::collections::HashSet::new();
        loop {
            if self.total_retained_bytes() <= self.budget_bytes {
                break;
            }
            let Some((victim, holds_a_copy)) = self.oldest_victim(protect, &held, &skipped) else {
                let total = self.total_retained_bytes();
                tracing::debug!(
                    event_name = "conversation_budget_unmet",
                    total,
                    budget = self.budget_bytes,
                    "the retained conversation total is over budget and no seat can be evicted; \
                     every one is being served, being read, or filling"
                );
                break;
            };
            if !self.evict_one(&victim, holds_a_copy, &mut held) {
                // The seat began a fill between the pick and the lock. Skip it
                // for this pass rather than block the folder behind it.
                skipped.insert(victim);
            }
        }
    }

    /// The oldest evictable seat, and whether it holds a copy.
    ///
    /// **One ordinal spaces both maps**, so a ring-only seat and a held one
    /// compare: the held copy's touch and the ring-only seat's last sighting
    /// come from the same counter.
    fn oldest_victim(
        &self,
        protect: &SessionSlot,
        held: &HashMap<SessionSlot, Arc<Held>>,
        skipped: &std::collections::HashSet<SessionSlot>,
    ) -> Option<(SessionSlot, bool)> {
        let readers = self.readers.lock().unwrap_or_else(PoisonError::into_inner);
        let reading = |slot: &SessionSlot| readers.get(slot).is_some_and(|count| *count > 0);
        let mut best: Option<(u64, SessionSlot, bool)> = None;
        for (slot, copy) in held {
            if slot == protect || skipped.contains(slot) || reading(slot) {
                continue;
            }
            let ordinal = copy.last_touch();
            if best.as_ref().is_none_or(|(oldest, _, _)| ordinal < *oldest) {
                best = Some((ordinal, slot.clone(), true));
            }
        }
        if let Some(evictor) = &self.ring_evictor {
            let last_seen = self.last_seen.lock().unwrap_or_else(PoisonError::into_inner);
            for (slot, ordinal) in last_seen.iter() {
                if slot == protect
                    || skipped.contains(slot)
                    || held.contains_key(slot)
                    || reading(slot)
                    || evictor.ring_bytes(slot) == 0
                {
                    continue;
                }
                if best.as_ref().is_none_or(|(oldest, _, _)| *ordinal < *oldest) {
                    best = Some((*ordinal, slot.clone(), false));
                }
            }
        }
        best.map(|(_, slot, holds_a_copy)| (slot, holds_a_copy))
    }

    /// Drop one seat's retained memory, holding its fill lock so a fill
    /// cannot start under the drop.
    ///
    /// **Try-lock, never block.** The fold loop holds a seat's fill lock while
    /// it applies that seat's frame, and the eviction runs from inside that
    /// apply. Blocking here would deadlock the folder against itself.
    fn evict_one(
        &self,
        slot: &SessionSlot,
        holds_a_copy: bool,
        held: &mut HashMap<SessionSlot, Arc<Held>>,
    ) -> bool {
        let fill = self.fill_lock(slot);
        let Ok(_filling) = fill.try_lock() else {
            return false;
        };
        if holds_a_copy {
            let Some(evicted) = held.remove(slot) else {
                return false;
            };
            let bytes = evicted.bytes();
            self.subtract_total(bytes);
            tracing::debug!(
                event_name = "conversation_evicted",
                victim = ?slot,
                bytes,
                ring = true,
                "evicted a seat's held conversation over the total budget"
            );
            if let Some(evictor) = &self.ring_evictor {
                evictor.evict_ring(slot);
            }
        } else {
            let Some(evictor) = &self.ring_evictor else {
                return false;
            };
            let bytes = evictor.ring_bytes(slot);
            if bytes == 0 {
                return false;
            }
            evictor.evict_ring(slot);
            tracing::debug!(
                event_name = "conversation_evicted",
                victim = ?slot,
                bytes,
                ring = true,
                "evicted a ring-only seat's window over the total budget"
            );
        }
        // **The next read knows the seat was dropped, not merely empty.** See
        // the `evicted` field: a dropped seat may rebuild from its window's
        // front, a seat nothing ever folded may not.
        self.evicted.lock().unwrap_or_else(PoisonError::into_inner).insert(slot.clone());
        true
    }

    /// The newest number the fold loop has consumed for `slot`.
    pub fn consumed_for(&self, slot: &SessionSlot) -> u64 {
        self.consumed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(slot)
            .map_or(0, |watch| *watch.borrow())
    }

    /// Say the fold loop has consumed `seq` for `slot`.
    ///
    /// **`send_replace`, not `send`.** The watch is created with its receiver
    /// dropped, so `consumed_for` can read the watermark with nobody waiting -
    /// and `send` is a no-op when a channel has no receivers, which would leave
    /// the watermark at 0 forever and make every read of it wrong.
    pub fn consumed(&self, slot: &SessionSlot, seq: u64) {
        let mut watches = self.consumed.lock().unwrap_or_else(PoisonError::into_inner);
        let watch = watches.entry(slot.clone()).or_insert_with(|| {
            let (tx, _rx) = tokio::sync::watch::channel(0);
            tx
        });
        watch.send_replace(seq);
    }

    /// Wait, up to `within`, for the fold to have consumed `want` for `slot`,
    /// answering whether it did.
    ///
    /// **Bounded, because the caller has a fallback and a stuck fold must not
    /// become a stall**: the caller fills the gap from the seat's own window
    /// and refuses only if that cannot reach either.
    pub async fn await_consumed(
        &self,
        slot: &SessionSlot,
        want: u64,
        within: std::time::Duration,
    ) -> bool {
        let mut watch = {
            let mut watches = self.consumed.lock().unwrap_or_else(PoisonError::into_inner);
            watches
                .entry(slot.clone())
                .or_insert_with(|| {
                    let (tx, _rx) = tokio::sync::watch::channel(0);
                    tx
                })
                .subscribe()
        };
        if *watch.borrow() >= want {
            return true;
        }
        tokio::time::timeout(within, watch.wait_for(|seen| *seen >= want))
            .await
            .is_ok_and(|seen| seen.is_ok())
    }

    /// The lock one seat's fill runs under: taken by the fold loop and by an
    /// encode's catch-up, so their watermark checks and applies cannot
    /// interleave.
    pub fn fill_lock(&self, slot: &SessionSlot) -> Arc<std::sync::Mutex<()>> {
        let mut fills = self.fills.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(fills.entry(slot.clone()).or_default())
    }

    /// Hold the fold loop, or let it go. Test-only: it is the seam that makes
    /// the copy's lag deterministic instead of a race.
    #[cfg(any(test, feature = "testing"))]
    pub fn hold_the_fold(&self, held: bool) {
        self.fold_held.store(held, std::sync::atomic::Ordering::Release);
    }

    /// Whether a test is holding the fold loop.
    #[cfg(any(test, feature = "testing"))]
    pub fn fold_is_held(&self) -> bool {
        self.fold_held.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Hold the next transcript seed before its read, so a test can land the
    /// seat's own copy inside that window. Test-only.
    #[cfg(any(test, feature = "testing"))]
    pub fn hold_the_seed(&self, hold: Arc<SeedHold>) {
        *self.seed_hold.lock().unwrap_or_else(PoisonError::into_inner) = Some(hold);
    }

    /// Park at a test's seed hold, when one is set: say it has arrived, then
    /// wait to be let go.
    #[cfg(any(test, feature = "testing"))]
    pub async fn pass_the_seed_hold(&self) {
        let hold = self.seed_hold.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if let Some(hold) = hold {
            hold.entered.notify_one();
            let _release = hold.release.acquire().await;
        }
    }

    /// The seat's conversation if one is held, touched as the LRU's youngest.
    ///
    /// **A seat with no copy is touched too**, in the map the budget reads for
    /// ring-only seats: a read of an evicted seat is a use, and without the
    /// mark the next eviction would take the seat it just served.
    pub fn get(&self, slot: &SessionSlot) -> Option<Arc<Held>> {
        let held =
            self.held.lock().unwrap_or_else(PoisonError::into_inner).get(slot).map(Arc::clone);
        let seq = self.touch_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(held) = &held {
            held.touch(seq);
        } else {
            self.note_seen(slot, seq);
        }
        held
    }

    /// Whether the budget dropped this seat's retained memory, with no copy
    /// held since.
    pub fn was_evicted(&self, slot: &SessionSlot) -> bool {
        self.evicted.lock().unwrap_or_else(PoisonError::into_inner).contains(slot)
    }

    /// Say a seat with no held copy was just seen, at `ordinal`.
    fn note_seen(&self, slot: &SessionSlot, ordinal: u64) {
        self.last_seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(slot.clone(), ordinal);
    }

    /// Hold the budget off `slot` while a read builds its record.
    ///
    /// **What keeps a rebuild from being evicted before it is served.** See
    /// the `readers` field: the guard spans the catch-up and the lookup.
    pub fn reading(&self, slot: &SessionSlot) -> ReadGuard<'_> {
        *self
            .readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(slot.clone())
            .or_insert(0) += 1;
        ReadGuard { conversations: self, slot: slot.clone() }
    }

    /// Say a seat the fold has consumed `seq` for, never moving it backwards.
    ///
    /// **What a transcript seed needs.** A seed is a copy of the rows up to
    /// the number the read was catching up to, so marking that number is what
    /// stops a later fill from applying the same frames onto it; and it must
    /// never undo a number the fold marked while the seed was being read.
    pub fn advance_consumed(&self, slot: &SessionSlot, seq: u64) {
        let mut watches = self.consumed.lock().unwrap_or_else(PoisonError::into_inner);
        let watch = watches.entry(slot.clone()).or_insert_with(|| {
            let (tx, _rx) = tokio::sync::watch::channel(0);
            tx
        });
        if seq > *watch.borrow() {
            watch.send_replace(seq);
        }
    }

    /// Whether the seat's turn is in flight right now, from its own frames.
    ///
    /// `false` for a seat this transport has not heard of: nothing holds a
    /// turn there, and a reader folding its transcript is reading a settled
    /// conversation.
    pub fn live_of(&self, slot: &SessionSlot) -> bool {
        self.get(slot).is_some_and(|held| held.lock().live)
    }

    /// Hold `conversation` for `slot`, unless the stream got there first.
    pub fn insert(&self, slot: &SessionSlot, conversation: Conversation) -> Arc<Held> {
        let held = {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            let existed = held.contains_key(slot);
            let entry = Arc::clone(
                held.entry(slot.clone()).or_insert_with(|| Arc::new(Held::new(conversation))),
            );
            if !existed {
                self.adjust_total(entry.bytes().cast_signed());
            }
            // **A seat just held or just read is the LRU's youngest**, so the
            // next eviction does not take the seat this call is serving. A
            // fresh `Held` starts at zero, which would read as the oldest.
            let seq = self.touch_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            entry.touch(seq);
            // **A copy is held again, so the seat is no longer an evicted
            // one**: its window's front is no longer the only thing the read
            // can rebuild from.
            self.evicted.lock().unwrap_or_else(PoisonError::into_inner).remove(slot);
            entry
        };
        self.seeded.notify_waiters();
        self.evict_over_budget(slot);
        held
    }

    /// Replace a seat's copy outright.
    ///
    /// **What a copy built from a ring stretch needs**: `insert` keeps what is
    /// there, and the stretch replaces it so the frames after it apply to the
    /// copy the read is served from.
    pub fn replace(&self, slot: &SessionSlot, conversation: Conversation) -> Arc<Held> {
        let (held, delta) = {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            let fresh = Arc::new(Held::new(conversation));
            let fresh_bytes = fresh.bytes();
            let old = held.insert(slot.clone(), Arc::clone(&fresh));
            let old_bytes = old.as_ref().map_or(0, |held| held.bytes());
            // **A seat just replaced is the LRU's youngest**, the same as a
            // seat just inserted: the replacement is being served now.
            let seq = self.touch_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            fresh.touch(seq);
            self.evicted.lock().unwrap_or_else(PoisonError::into_inner).remove(slot);
            (fresh, fresh_bytes.cast_signed() - old_bytes.cast_signed())
        };
        self.adjust_total(delta);
        self.seeded.notify_waiters();
        self.evict_over_budget(slot);
        held
    }

    /// A future that resolves when some seat's copy is first held or reseeded.
    ///
    /// **Enable it before looking the seat up**, or a seat seeded between the
    /// lookup and the wait leaves the caller waiting for something that has
    /// already happened - and bound the wait, because a seat whose session is
    /// gone never answers at all.
    ///
    /// A reseed counts: a seal the header has already moved on from lands on
    /// the copy the seat holds, in place, so a reader waiting for that copy
    /// to become current is woken by it rather than by the seat's first hold.
    pub fn seeded_notice(&self) -> tokio::sync::futures::Notified<'_> {
        self.seeded.notified()
    }

    /// Fold one update into the seat it names.
    ///
    /// `seq` is the number the frame was minted under; the copy records it as
    /// the newest it reflects.
    pub fn apply(&self, update: &SessionUpdate, seq: Option<u64>) {
        let Some(slot) = update.slot() else {
            return;
        };
        let mut delta = 0isize;
        match update {
            // A connect and a resume say the same thing about this seat -
            // here is its conversation - so they RESEED it, materialising it
            // if this is the first word about the seat. Appending instead
            // would put the history in front of the frames the seat already
            // carried.
            SessionUpdate::Connected { history, compaction_count, dropped, session_id, .. }
            | SessionUpdate::SessionReplaced {
                history,
                compaction_count,
                dropped,
                session_id,
                ..
            } => {
                self.reseed(slot, history, *compaction_count, *dropped, Some(session_id));
            }
            SessionUpdate::ChatAppended { msg, .. } => {
                // **A compaction boundary is a frame like any other.** It
                // once dropped the seat so the next ask would reseed it, and
                // that is the door this change closed everywhere else:
                // replacing a copy the task did not produce is how a frame it
                // never sees gets lost. The boundary is appended, the task
                // counts it, and a reader keeps the history a compaction did
                // not take away.
                if let Some(held) = self.get(slot) {
                    let mut conversation = held.lock();
                    conversation.append(msg.clone());
                    if let Some(seq) = seq {
                        conversation.set_watermark(seq);
                    }
                    delta = held.reweigh(&conversation);
                }
            }
            // The delivery-failure notice is a row a CONNECTION forges per
            // client at send time, so it never arrives here as a
            // `ChatAppended` - and a held copy that ignored it would lose the
            // row while its ANSWER stayed, which is a reply to nothing.
            // Forged once here, where the update arrives, rather than in each
            // connection that sends it. Every other delivery's row is the
            // session task's, drawn at the CLI's `started` and applied as the
            // `ChatAppended` it is.
            //
            // **Every slotted update also moves the copy's watermark**, not
            // only the ones that forge a row: a frame that draws nothing still
            // covers its number, and an answer that left it out would
            // under-claim. That is why this arm takes the seat's lock for
            // every update on a live seat, and why `Held::lock` waits out a
            // running fold.
            _ => {
                let Some(held) = self.get(slot) else {
                    return;
                };
                // The parse and its failure log run before the seat's lock is
                // taken; the lock below covers only the append and the mark.
                let forged = crate::delivery::delivery_turn(update, slot).map(|(msg, _)| msg);
                let mut conversation = held.lock();
                if let Some(msg) = forged {
                    // Forged for display, and in no transcript: the seat has
                    // to know, so a walk below the floor never names one to
                    // the file as the row it stopped at.
                    conversation.append_forged(msg);
                }
                if let Some(seq) = seq {
                    conversation.set_watermark(seq);
                }
                delta = held.reweigh(&conversation);
            }
        }
        self.adjust_total(delta);
        self.evict_over_budget(slot);
    }

    /// Put `history` on `slot`, materialising the seat when this is the first
    /// word about it.
    ///
    /// **Held means reseed and NOT insert**: `insert` keeps what is there, so
    /// a connect on a seat already carrying a conversation would leave that
    /// conversation in place and the new history discarded.
    ///
    /// The watermark goes to zero: a seal opens a new epoch, whose frame
    /// count starts at 1.
    fn reseed(
        &self,
        slot: &SessionSlot,
        history: &[Message],
        compaction_count: u32,
        carried: usize,
        occupant: Option<&forge_primitives::SessionId>,
    ) {
        if let Some(held) = self.get(slot) {
            let delta = {
                let mut conversation = held.lock();
                conversation.seed(history.to_vec(), compaction_count, carried);
                conversation.set_watermark(0);
                if let Some(occupant) = occupant {
                    conversation.set_occupant(Some(occupant.clone()));
                }
                held.reweigh(&conversation)
            };
            // A reader waiting for a copy of another occupant is waiting for
            // exactly this, and it landed in place.
            self.seeded.notify_waiters();
            self.adjust_total(delta);
            self.evict_over_budget(slot);
        } else {
            let mut conversation = Conversation::new(history.to_vec(), compaction_count, carried);
            conversation.set_watermark(0);
            if let Some(occupant) = occupant {
                conversation.set_occupant(Some(occupant.clone()));
            }
            self.insert(slot, conversation);
        }
    }

    /// How many seats are held.
    pub fn len(&self) -> usize {
        self.held.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use forge_primitives::{ContentBlock, SessionId};
    use forge_workspace::ConversationEvictor;
    use forge_workspace::conversation_window::{CONVERSATION_CAP, CONVERSATION_SLACK};

    use super::*;

    fn a_seat() -> SessionSlot {
        SessionSlot::lead("TestOrg", "proj")
    }

    /// **A copy with no occupant answers for any occupant.** The arm is
    /// load-bearing, not defensive: a transcript that named no session seeds
    /// a copy with no occupant, and reading it as "not the seat's" would drop
    /// that copy's history.
    #[test]
    fn a_copy_with_no_occupant_matches_any_occupant() {
        let bare = Held::new(Conversation::empty());
        assert!(
            bare.holds_occupant(Some(&SessionId::from("6c2d3e4f-5061-4b72-8839-ad1e2f3a4b56"))),
            "an occupant-less copy must match a live header, or a sleeping seat answers empty",
        );

        let mut seeded = Conversation::empty();
        let old = SessionId::from("1f2e3d4c-5b6a-4978-8801-1234567890ab");
        let new = SessionId::from("2a3b4c5d-6e7f-4809-9a1b-234567890abc");
        seeded.set_occupant(Some(old.clone()));
        let seeded = Held::new(seeded);
        assert!(seeded.holds_occupant(Some(&old)), "a copy of the current occupant is the seat's");
        assert!(!seeded.holds_occupant(Some(&new)), "a copy of another occupant is no copy");
    }

    fn a_model() -> forge_primitives::runtime::CurrentModel {
        forge_primitives::runtime::CurrentModel {
            requested_id: None,
            resolved_id: "claude-opus-5".to_owned(),
            display_name_short: "Opus".to_owned(),
            display_name_long: "Opus 5".to_owned(),
            catalog_id: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_auto_mode: None,
            supports_adaptive_thinking: None,
            is_authoritative: false,
        }
    }

    /// An assistant frame calling `tool`, optionally one a sub-agent made.
    fn a_frame(text: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        }))
        .expect("a user frame")
    }

    fn said(message: &Message) -> String {
        let Message::User { message, .. } = message else {
            return String::new();
        };
        message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// `turns` turns of three frames each: the row that opens one, a call the
    /// turn made, and the result answering it.
    ///
    /// **Three rather than one, so a drop lands mid-turn.** A cap that cut
    /// its window at a turn boundary whatever it did would be a cap this
    /// fixture could not tell from one that cut wherever it liked.
    fn a_long_history(turns: usize) -> Vec<Message> {
        let mut messages = Vec::with_capacity(turns * 3);
        for at in 0..turns {
            messages.push(a_frame(&format!("turn {at}")));
            messages.push(
                serde_json::from_value(serde_json::json!({
                    "type": "assistant",
                    "uuid": format!("a{at}"),
                    "message": {
                        "id": format!("m{at}"),
                        "role": "assistant",
                        "model": "claude-opus-5",
                        "content": [{
                            "type": "tool_use",
                            "id": format!("tu{at}"),
                            "name": "Bash",
                            "input": {"command": "ls"},
                        }],
                    },
                    "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
                }))
                .expect("an assistant frame"),
            );
            messages.push(
                serde_json::from_value(serde_json::json!({
                    "type": "user",
                    "uuid": format!("u{at}"),
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": format!("tu{at}"),
                            "content": "ok",
                        }],
                    },
                    "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
                }))
                .expect("a user frame"),
            );
        }
        messages
    }

    /// The words each turn of a page opened on.
    fn turn_texts(page: &crate::transport::wire::Page) -> Vec<String> {
        page.turns
            .iter()
            .map(|turn| {
                turn.messages
                    .iter()
                    .find_map(|frame| {
                        frame["message"]["content"][0]["text"].as_str().map(str::to_owned)
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The newest page of a held conversation, as a client reads one.
    fn newest_page(held: &Held, turns: u32) -> crate::transport::wire::Page {
        held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                None,
                turns,
            )
        })
    }

    /// The newest page with the cut's thinking debt folded into it, which is
    /// the shape the transport serves.
    fn page_with_debt(held: &Held, turns: u32) -> crate::transport::wire::Page {
        held.read(|held| {
            crate::transport::wire::page_within(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                None,
                turns,
                usize::MAX,
                "test",
                None,
            )
        })
    }

    /// One thinking-token counter, with a cumulative absolute that is not its
    /// delta - so summing the wrong field fails.
    fn a_counter(at: usize, delta: i64) -> Message {
        let absolute = at.saturating_add(1) * usize::try_from(delta).expect("a positive delta");
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "thinking_tokens",
            "estimated_tokens": absolute,
            "estimated_tokens_delta": delta,
            "uuid": format!("t-{at}"),
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        }))
        .expect("a counter frame")
    }

    /// The frame a compaction leaves, which the scan reads as a boundary.
    fn a_compact_boundary() -> Message {
        Message::CompactBoundary {
            trigger: "auto".to_owned(),
            pre_tokens: 100_000,
            post_tokens: 21_000,
            uuid: "b1c2d3e4-0000-4000-8000-000000000001".to_owned(),
            session_id: "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45".to_owned(),
            metadata_extras: serde_json::Map::new(),
            extras: serde_json::Map::new(),
        }
    }

    /// The frame an error leaves, which ends a turn as a `Result` does.
    fn a_error_frame() -> Message {
        Message::Error { error: "the turn failed".to_owned(), extras: serde_json::Map::new() }
    }

    /// One turn with `counters` thinking-token frames after its opening row.
    fn a_thought_heavy_turn(counters: usize) -> Vec<Message> {
        let mut messages = vec![a_frame("a thought-heavy turn")];
        for at in 0..counters {
            messages.push(a_counter(at, 1));
        }
        messages
    }

    /// The sum of the counter deltas a page serves.
    fn served_thinking(page: &crate::transport::wire::Page) -> i64 {
        page.turns
            .iter()
            .flat_map(|turn| turn.messages.iter())
            .filter_map(|frame| {
                if frame["type"] == "system" && frame["subtype"] == "thinking_tokens" {
                    frame["estimated_tokens_delta"].as_i64()
                } else {
                    None
                }
            })
            .sum()
    }

    /// **A cut inside a running turn leaves a debt the page pays.** The window
    /// caps at 4,000 frames plus 1,000 slack, and a thinking storm is exactly
    /// the turn that outgrows it: the cut takes the turn's leading counters,
    /// and the page must carry their growth back on the first counter it has.
    #[test]
    fn a_thought_heavy_turn_serves_its_whole_sum_after_the_window_sheds_its_head() {
        let counters = 5_100;
        let held = Held::new(Conversation::new(a_thought_heavy_turn(counters), 0, 0));

        let dropped = held.read(Conversation::dropped_thinking);
        assert!(dropped > 0, "the cut took the turn's leading counters");

        let page = page_with_debt(&held, u32::MAX);
        assert_eq!(
            served_thinking(&page),
            i64::try_from(counters).expect("the test's count fits"),
            "the page carries the whole turn, not only the counters that survived",
        );
    }

    /// **A settled turn still draws its report, so its debt survives the
    /// result.** Only a frame that opens the next turn clears it, and the rule
    /// names no turn end at all - `Result` and `Error` are treated alike.
    #[test]
    fn the_dropped_thinking_survives_a_result_and_clears_on_the_next_turn() {
        let mut conversation = Conversation::empty();
        conversation.append(a_frame("a thought-heavy turn"));
        for at in 0..5_100 {
            conversation.append(a_counter(at, 1));
        }
        let carried = conversation.dropped_thinking();
        assert!(carried > 0, "the drop carries a debt");

        conversation.append(a_result_frame());
        assert_eq!(
            conversation.dropped_thinking(),
            carried,
            "a settled turn is still the newest turn a page serves",
        );

        conversation.append(a_error_frame());
        assert_eq!(conversation.dropped_thinking(), carried, "an error ends a turn the same way");

        conversation.append(a_frame("the next turn"));
        assert_eq!(
            conversation.dropped_thinking(),
            0,
            "a frame that opens a turn starts with nothing carried",
        );
    }

    /// **A turn that fits is served exactly as it was.** No cut means no debt,
    /// and the page's frames are the input's own bytes.
    #[test]
    fn a_turn_that_fits_is_served_unchanged() {
        let mut conversation = Conversation::empty();
        conversation.append(a_frame("a small turn"));
        for at in 0..10 {
            conversation.append(a_counter(at, 2));
        }
        assert_eq!(conversation.dropped_thinking(), 0, "no cut, no debt");

        let expected: Vec<serde_json::Value> = conversation
            .messages()
            .iter()
            .map(|message| serde_json::to_value(message).expect("a frame encodes"))
            .collect();
        let held = Held::new(conversation);
        let page = page_with_debt(&held, u32::MAX);
        assert_eq!(served_thinking(&page), 20, "the page is the turn's own sum");
        assert_eq!(
            page.turns[0].messages, expected,
            "a fitting turn's frames are its input, byte for byte",
        );
    }

    /// **A second drop in one turn adds to the debt; it does not replace it.**
    /// The live path drops a thousand frames at a time once the window passes
    /// its slack, and the counters it sheds accumulate until a turn opens.
    #[test]
    fn a_second_live_drop_accumulates_the_debt() {
        let counters = 6_100;
        let mut conversation = Conversation::empty();
        conversation.append(a_frame("a thought-heavy turn"));
        for at in 0..counters {
            conversation.append(a_counter(at, 1));
        }
        assert_eq!(conversation.dropped_thinking(), 2_000, "two drops of a thousand counters");

        let held = Held::new(conversation);
        let page = page_with_debt(&held, u32::MAX);
        assert_eq!(
            served_thinking(&page),
            i64::try_from(counters).expect("the test's count fits"),
            "the page carries the whole turn",
        );
    }

    /// **A settled seed keeps the debt its own cut produced.** The newest turn
    /// is settled, and its report still draws the figure.
    #[test]
    fn a_settled_seed_keeps_its_cut_debt() {
        let counters = 5_100;
        let mut history = a_thought_heavy_turn(counters);
        history.push(a_result_frame());
        let held = Held::new(Conversation::new(history, 0, 0));

        assert!(held.read(Conversation::dropped_thinking) > 0, "the seed's cut carries a debt");
        let page = page_with_debt(&held, u32::MAX);
        assert_eq!(
            served_thinking(&page),
            i64::try_from(counters).expect("the test's count fits"),
            "the settled page carries the whole turn",
        );
    }

    /// **A seed primes the scan, and an append after it continues from that
    /// state.** A compaction boundary in the seeded history is what makes the
    /// continuation that follows open no turn; a fresh scan would open one and
    /// clear the debt.
    #[test]
    fn a_seed_primes_the_scan_for_the_frames_after_it() {
        let mut history = a_thought_heavy_turn(5_100);
        history.push(a_compact_boundary());
        let mut conversation = Conversation::new(history, 0, 0);
        let carried = conversation.dropped_thinking();
        assert!(carried > 0, "precondition: the seed's cut carries a debt");

        conversation.append(a_frame(
            "This session is being continued from a previous conversation that ran out of context.",
        ));
        assert_eq!(
            conversation.dropped_thinking(),
            carried,
            "the continuation is not an opening, so the debt stays",
        );
    }

    fn a_connect(history: Vec<Message>, compaction_count: u32) -> SessionUpdate {
        SessionUpdate::Connected {
            key: a_seat(),
            session_id: SessionId::new("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45"),
            cwd: "/tmp".to_owned(),
            current_model: a_model(),
            available_models: Vec::new(),
            mode: None,
            history,
            dropped: 0,
            compaction_count,
        }
    }

    /// **The property the whole seed rests on: a seal is a reseed, not an
    /// append.** If it appended instead of replacing, a seat would carry its
    /// history in front of the frames it already had.
    #[test]
    fn a_connect_replaces_what_the_seat_held() {
        let history = vec![a_frame("first"), a_frame("second")];

        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::new(vec![a_frame("stale")], 0, 0));
        held.apply(&a_connect(history, 0), None);

        let read: Vec<String> = {
            let conversation = held.get(&a_seat()).expect("the seat is held");
            let conversation = conversation.lock();
            conversation.messages().iter().map(said).collect()
        };
        assert_eq!(
            read,
            vec!["first".to_owned(), "second".to_owned()],
            "a connect replaces what the seat held",
        );
    }

    /// A frame the model produced, which is what opens a turn's liveness.
    fn a_working_frame() -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "uuid": "a-working",
            "message": {
                "id": "msg_working",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{ "type": "text", "text": "working" }],
            },
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        }))
        .expect("an assistant frame")
    }

    /// The frame that ends a turn, per the delivered ladder.
    fn a_result_frame() -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "uuid": "r-1",
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
            "is_error": false,
            "num_turns": 1,
            "duration_ms": 12,
            "duration_api_ms": 9,
        }))
        .expect("a result frame")
    }

    /// An assistant frame calling `id`, which is both a call and the frame
    /// that opens a turn's liveness.
    fn a_call_frame(id: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "uuid": format!("a-{id}"),
            "message": {
                "id": "msg_call",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{
                    "type": "tool_use",
                    "id": id,
                    "name": "mcp__forge__agents__despawn",
                    "input": { "label": "probe" },
                }],
            },
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        }))
        .expect("an assistant frame")
    }

    /// The chase's board notice, as the CLI presses it into a running turn:
    /// a text row under the id forge minted for the dispatch.
    fn a_forge_row(uuid: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "message": {
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "task board: \"probe holds no row\" has no live owner",
                }],
            },
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        }))
        .expect("a user frame")
    }

    /// **The hop that carries the seat's flag into the fold is pinned.**
    ///
    /// The fold's own tests hand `render` the flag directly, so nothing there
    /// would notice a caller passing `false` - and #2379's fix would revert
    /// quietly with a full green suite. This folds THROUGH `Held` with a live
    /// seat and the mid-call shape, asserting the one boundary the hop exists
    /// to protect.
    #[test]
    fn a_live_seats_mid_call_notice_folds_into_one_turn_through_held() {
        let held = Held::new(Conversation::new(
            vec![
                a_frame("despawn the probe"),
                a_call_frame("call_1"),
                a_forge_row("forge_nudge_1_deadbeef"),
            ],
            0,
            0,
        ));

        let turns = held.read(|held| held.rendered().turns.len());
        assert_eq!(turns, 1, "the notice stays in the turn the seat's ladder reports live");
    }

    /// **The seat's turn state travels to the fold, folded off its frames.**
    ///
    /// The frame that would say a turn is running (`session_state_changed`)
    /// is never sent, so the fold cannot read liveness off its own list: the
    /// transport derives the delivered ladder - the model's own frames open a
    /// turn, its `Result` (or the error it gives up with) closes it - and the
    /// fold reads that as "the open turn is live", which is what keeps a
    /// mid-call delivery from being cut off from a result that has not
    /// streamed yet (#2379). A seed materialised mid-turn starts live for the
    /// same reason.
    #[test]
    fn the_seats_live_turn_travels_to_the_fold() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::empty());
        assert!(!held.live_of(&a_seat()), "nothing is open on an empty seat");

        held.apply(
            &SessionUpdate::ChatAppended { key: a_seat(), msg: a_working_frame(), source: None },
            None,
        );
        assert!(held.live_of(&a_seat()), "the model's own frame opens a turn");

        held.apply(
            &SessionUpdate::ChatAppended { key: a_seat(), msg: a_result_frame(), source: None },
            None,
        );
        assert!(!held.live_of(&a_seat()), "and the turn's report closes it");

        held.apply(&a_connect(vec![a_frame("mid-turn"), a_working_frame()], 0), None);
        assert!(held.live_of(&a_seat()), "a seed materialised mid-turn reads live");
    }

    /// A frame the session emits after the seed joins the conversation, which
    /// is what the held copy is for.
    #[test]
    fn a_frame_after_a_seed_joins_the_conversation() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::empty());
        held.apply(&a_connect(vec![a_frame("from the replay")], 0), None);

        held.apply(
            &SessionUpdate::ChatAppended { key: a_seat(), msg: a_frame("after"), source: None },
            None,
        );

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let conversation = conversation.lock();
        let said: Vec<String> = conversation.messages().iter().map(said).collect();
        assert_eq!(said, vec!["from the replay".to_owned(), "after".to_owned()]);
    }

    /// A compaction the session reports MOVES the seat's count, which is what
    /// the header reads.
    ///
    /// **The boundary arrives as a frame rather than in a seed**, so a record
    /// that took only the count its last connect carried would report the
    /// number from before every compaction since.
    #[test]
    fn a_compact_boundary_frame_moves_the_seats_count() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::empty());
        held.apply(&a_connect(vec![a_frame("before the compaction")], 0), None);

        held.apply(
            &SessionUpdate::ChatAppended {
                key: a_seat(),
                msg: Message::CompactBoundary {
                    trigger: "auto".to_owned(),
                    pre_tokens: 100_000,
                    post_tokens: 21_000,
                    uuid: "b1c2d3e4-0000-4000-8000-000000000001".to_owned(),
                    session_id: "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45".to_owned(),
                    metadata_extras: serde_json::Map::new(),
                    extras: serde_json::Map::new(),
                },
                source: None,
            },
            None,
        );

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let conversation = conversation.lock();
        assert_eq!(
            conversation.compaction_count(),
            1,
            "the count follows the frames the session emits, not only the seed",
        );
    }

    /// A seed carries the count it brings, on both routes in.
    ///
    /// **The frame test above pins the half that moves; this is the half that
    /// arrives WITH a history**, and forcing it to zero at either constructor
    /// would report "no compactions" on a resumed seat with nothing failing.
    #[test]
    fn a_seed_carries_the_count_it_brings() {
        let fresh = Conversations::new();
        fresh.apply(&a_connect(vec![a_frame("resumed")], 3), None);
        let conversation = fresh.get(&a_seat()).expect("the seat is held");
        assert_eq!(conversation.lock().compaction_count(), 3, "a connect's count is kept");

        let reseeded = Conversations::new();
        reseeded.insert(&a_seat(), Conversation::new(vec![a_frame("stale")], 0, 0));
        reseeded.apply(&a_connect(vec![a_frame("replayed")], 2), None);
        let conversation = reseeded.get(&a_seat()).expect("the seat is held");
        assert_eq!(
            conversation.lock().compaction_count(),
            2,
            "and a reseed replaces it rather than zeroing it",
        );
    }

    /// A connect holds the seat's conversation whether or not anyone is
    /// watching it, because nothing can rebuild one that was let go: the
    /// conversation has no read behind it, so a seat dropped is a seat whose
    /// next ask has no answer.
    #[test]
    fn a_connect_holds_the_seat_without_anyone_watching() {
        let held = Conversations::new();

        held.apply(&a_connect(vec![a_frame("running, unwatched")], 0), None);

        assert_eq!(held.len(), 1, "the seat is held from its connect");
        let conversation = held.get(&a_seat()).expect("the seat is held");
        let conversation = conversation.lock();
        assert_eq!(conversation.messages().len(), 1, "carrying the history the connect brought");
    }

    /// The dispatch rule and its flag moved to `forge-workspace` with the push
    /// that announces the raise: the rule's own test lives beside it there
    /// (`a_dispatch_is_an_assistant_frame_calling_task_or_agent`), and this
    /// crate answers the flag through the view surface rather than computing
    /// it.
    /// A reseed leaves the boundaries consistent with the messages they
    /// describe, so a page over a just-reseeded seat cannot slice past the end
    /// of the list it is reading.
    ///
    /// **`messages` and `spans` are one value in two fields**, and a reader
    /// sees both or neither: the spans name a prefix of the messages, and a
    /// list that shrank under boundaries that did not is how a page walks off
    /// the end of an empty one.
    #[test]
    fn a_reseed_leaves_the_boundaries_describing_the_messages() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::new(vec![a_frame("one"), a_frame("two")], 0, 0));
        let conversation = held.get(&a_seat()).expect("the seat is held");
        conversation.fold();
        assert_eq!(conversation.lock().rendered().turns.len(), 2, "precondition: two turns folded");

        held.apply(&a_connect(Vec::new(), 0), None);

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let held = conversation.lock();
        assert!(held.messages().is_empty(), "the reseed replaced the messages");
        assert!(
            held.rendered().turns.is_empty(),
            "and the boundaries went with them, rather than describing a list that is gone",
        );
    }

    /// A fold that does not reach its end gives the messages back, and is
    /// folded again by the next read.
    ///
    /// **The guard's whole job, and the failure it prevents is a wedge rather
    /// than a bad page**: the messages are out of the seat while the render
    /// runs, so a panic that unwound past them would drop the conversation,
    /// leave `folding` set, and never wake the readers - parking every page
    /// request on the seat, and through `apply` the socket's single stream
    /// folder, for the process's life.
    ///
    /// **And the boundaries a guard installs describe nothing**, so the seat
    /// has to stay due a fold: a read that took it as folded would cut its
    /// page on no turns and hand the whole conversation back as one - the
    /// unbounded response this change exists to bound.
    #[test]
    fn a_fold_that_does_not_finish_gives_the_messages_back() {
        let conversation = Conversation::new(vec![a_frame("kept")], 0, 0);

        let held = Held::new(conversation);
        let taken = Folding::begin(&held).expect("a fold is due");
        // Read through the raw mutex rather than `lock`: `lock` WAITS for
        // a fold in progress, which is the state this test is standing in.
        let out = held.conversation.lock().expect("the mutex").messages().len();
        assert_eq!(out, 0, "precondition: the messages are out of the seat");

        // Dropped rather than finished, which is what unwinding does.
        drop(taken);

        {
            let held = held.lock();
            assert!(!held.folding, "the seat is not left waiting for a fold that will never end");
            assert_eq!(held.messages().len(), 1, "and the messages it took are back in it");
        }

        let turns = held.read(|conversation| conversation.rendered().turns.len());
        assert_eq!(turns, 1, "the next read folds them, rather than serving no boundaries");
    }

    /// The failure notice's forged row is part of the conversation the
    /// transport is holding.
    ///
    /// **It is forged per CONNECTION at send time, so it never arrives here as
    /// a `ChatAppended`** - and a held copy that ignored it would lose the row
    /// while its answer stayed, which is a reply to nothing. Nothing observed
    /// this: with the append removed the whole workspace suite stays green.
    #[test]
    fn a_forged_failure_notice_row_joins_the_conversation_it_is_held_for() {
        use forge_workspace::{MessageId, WrappedKind, WrappedPrompt};

        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::new(vec![a_frame("asked")], 0, 0));

        held.apply(
            &SessionUpdate::PeerEnvelopeAppended {
                key: a_seat(),
                uuid: "cap-notice".to_owned(),
                wrapped: WrappedPrompt {
                    id: MessageId("m-1".to_owned()),
                    kind: WrappedKind::DeliveryFailureNotice,
                    sender_name: "companies".to_owned(),
                    sender_org: "Busytools".to_owned(),
                    body: "target session connection lost".to_owned(),
                },
            },
            None,
        );

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let conversation = conversation.lock();
        let said: Vec<String> = conversation.messages().iter().map(said).collect();
        assert_eq!(
            said,
            vec![
                "asked".to_owned(),
                "[Message to agent 'companies' (org 'Busytools') failed to deliver: target \
                 session connection lost]"
                    .to_owned(),
            ],
            "the notice the connection forges is in the conversation a page is built from",
        );
    }

    /// **A delivery the session task draws at `started` forges nothing here.**
    /// Its row arrives as a `ChatAppended` like any other; a forged copy from
    /// the typed update would put the same words in the conversation twice.
    #[test]
    fn a_delivery_that_draws_at_started_appends_no_forged_row() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::new(vec![a_frame("asked")], 0, 0));

        held.apply(
            &SessionUpdate::CronPromptAppended {
                key: a_seat(),
                text: "the cron fired".to_owned(),
                uuid: "cap-cron".to_owned(),
                cron_id: "c1".to_owned(),
                description: Some("Morning summary".to_owned()),
            },
            None,
        );

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let conversation = conversation.lock();
        let said: Vec<String> = conversation.messages().iter().map(said).collect();
        assert_eq!(
            said,
            vec!["asked".to_owned()],
            "the typed update is not a row: the CLI's started frame draws it",
        );
    }

    /// A read after a reseed answers a page over the reseeded conversation, and
    /// never a slice past the end of it.
    ///
    /// **What this pins is the OUTCOME and not the retry that usually brings
    /// it about.** A reseed landing between `read`'s fold and its read is a
    /// race with no deterministic trigger, so the retry cannot be made to fire
    /// on demand; what a test can hold is that the state a reseed leaves is one
    /// a page can be cut on. A raced probe of 300 iterations saw no incoherent
    /// page, and that is evidence rather than proof.
    #[test]
    fn a_read_after_a_reseed_is_a_page_and_not_a_panic() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::new(vec![a_frame("one"), a_frame("two")], 0, 0));

        // A reseed with no fold after it: the boundaries are cleared, so what
        // a reader gets is the whole list as one turn rather than a slice past
        // the end of it.
        held.apply(&a_connect(vec![a_frame("replaced")], 0), None);

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let page = conversation.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                None,
                5,
            )
        });
        assert_eq!(page.turns.len(), 1, "the reseeded conversation is one turn");
        assert_eq!(
            page.turns[0].messages.len(),
            1,
            "and the page carries what the reseed put there",
        );
    }

    /// A task notice a history carries reaches the conversation in the shape a
    /// view reads an ending from, not as the reader's own words.
    ///
    /// **The read converts these and a seed takes the history raw**, so
    /// without this a resumed seat serves the `<task-notification>` XML as
    /// text, the ending goes unread, and for a notice in the same turn as its
    /// call nothing ends that call at all - the conversion #1364 shipped,
    /// undone by the held copy.
    #[test]
    fn a_task_notice_in_a_seeded_history_is_a_block_and_not_text() {
        let notice = "<task-notification><tool-use-id>tu1</tool-use-id>\
                      <status>completed</status><summary>done</summary></task-notification>";

        // Both routes a history takes in - a connect that materialises the
        // seat and a reseed that replaces one - because each converts it
        // itself, and either could be the one that forgot.
        let fresh = Conversation::new(vec![a_frame(notice)], 0, 0);
        let mut reseeded = Conversation::new(vec![a_frame("stale")], 0, 0);
        reseeded.seed(vec![a_frame(notice)], 0, 0);

        for (conversation, route) in [(&fresh, "a connect"), (&reseeded, "a reseed")] {
            let Message::User { message, .. } = &conversation.messages()[0] else {
                panic!("a user frame is what the history carried ({route})");
            };
            assert!(
                matches!(
                    &message.content[0],
                    ContentBlock::QueuedCommand { command_mode: Some(mode), .. }
                        if mode == "task-notification"
                ),
                "the notice is served as the block a view reads an ending from, not as text \
                 ({route}): {:?}",
                message.content[0],
            );
        }
    }

    /// A frame on a seat nothing has seeded is dropped rather than inventing
    /// a conversation: a seat whose connect this transport never saw has no
    /// copy, and there is nothing to append to.
    #[test]
    fn a_frame_before_a_seat_is_seeded_holds_nothing() {
        let held = Conversations::new();

        held.apply(
            &SessionUpdate::ChatAppended {
                key: a_seat(),
                msg: a_frame("before the seed"),
                source: None,
            },
            None,
        );

        assert_eq!(held.len(), 0, "a frame alone does not materialise a seat");
    }

    /// The cap bounds what a seat holds: appending past its slack drops the
    /// oldest messages and keeps the newest.
    ///
    /// **The drop runs at the slack and keeps the cap**, so the seat carries
    /// a window plus a slack's worth of room for the appends that refill it -
    /// and it runs before the push, which is what keeps the store from
    /// doubling past the slack it was sized for.
    #[test]
    fn an_append_past_the_caps_slack_drops_the_oldest_messages() {
        let mut conversation = Conversation::empty();
        for at in 0..CONVERSATION_CAP + CONVERSATION_SLACK {
            conversation.append(a_frame(&format!("{at}")));
        }
        assert_eq!(
            conversation.messages().len(),
            CONVERSATION_CAP + CONVERSATION_SLACK,
            "precondition: the slack is filled and nothing has been dropped",
        );

        conversation.append(a_frame("one past the slack"));

        assert_eq!(
            conversation.messages().len(),
            CONVERSATION_CAP + 1,
            "the drop keeps the cap, and the frame that triggered it joins them",
        );
        assert_eq!(
            conversation.dropped(),
            CONVERSATION_SLACK,
            "and it counted every message it left behind",
        );
        assert_eq!(
            said(&conversation.messages()[0]),
            format!("{CONVERSATION_SLACK}"),
            "the oldest kept is the one the cap reaches back to",
        );
        assert_eq!(
            said(conversation.messages().last().expect("a newest")),
            "one past the slack",
            "and the newest is the frame that just arrived",
        );
    }

    /// **A seed over the cap keeps the window and reports the cut** - the
    /// store's own shape is the window module's to pin, and this is the seat's
    /// half: what it keeps and what it counts as left behind.
    #[test]
    fn a_conversation_over_the_cap_keeps_the_window_and_reports_the_cut() {
        let one = a_frame("a message");
        let history = vec![one; CONVERSATION_CAP * 4];

        let conversation = Conversation::new(history, 0, 0);

        assert_eq!(conversation.messages().len(), CONVERSATION_CAP, "a seed keeps the cap");
        assert_eq!(conversation.dropped(), CONVERSATION_CAP * 3, "and reports what it left behind");
    }

    /// **The drop does not touch the window.** `SUBSCRIBE_TURNS` is what every
    /// client is handed when it attaches, so the page read over a capped
    /// conversation has to answer exactly what it answers over the same
    /// conversation with nothing dropped - the same turns, whole, and a
    /// cursor naming the same message.
    #[test]
    fn the_drop_leaves_the_newest_page_exactly_as_it_was() {
        let messages = a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK);
        let whole = crate::transcript::render(&messages, false);
        let unbounded = crate::transport::wire::page(
            &messages,
            &whole,
            0,
            0,
            None,
            crate::transport::wire::SUBSCRIBE_TURNS,
        );

        let conversation = Conversation::new(messages, 0, 0);
        assert!(conversation.dropped() > 0, "precondition: the seed was over the cap");
        let held = Held::new(conversation);
        let capped = newest_page(&held, crate::transport::wire::SUBSCRIBE_TURNS);

        assert_eq!(
            turn_texts(&capped),
            turn_texts(&unbounded),
            "the newest page is the conversation's, not the window's",
        );
        assert_eq!(
            capped.cursor, unbounded.cursor,
            "and the cursor above it names the same message: a drop renumbers the held list, \
             not the conversation",
        );
    }

    /// **A connect restarts the numbering.** It hands over a history read off
    /// the transcript, which is a row subset rather than this seat's own frame
    /// sequence, so an offset carried over it would be off by however far the
    /// two diverge.
    #[test]
    fn a_connects_history_restarts_the_numbering() {
        let held = Conversations::new();
        held.apply(&a_connect(a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK), 0), None);
        let conversation = held.get(&a_seat()).expect("the seat is held");
        let dropped = conversation.lock().dropped();
        assert!(dropped > 0, "precondition: the connect was over the cap");

        let window = conversation.lock().messages().to_vec();
        held.apply(&a_connect(window, 0), None);
        assert_eq!(
            conversation.lock().dropped(),
            0,
            "a connect's history restarts it, however its window looks",
        );
    }

    /// **A cursor keeps its numbering across a drop the APPENDS ran too.**
    ///
    /// The two tests beside this one seed their drops through a constructor,
    /// where the offset is set once; a seat a frame at a time past its slack
    /// is the ordinary case, and the numbering has to accumulate there -
    /// a drop that overwrote the count instead of adding to it would resolve
    /// the cursor that many messages high and answer a client the newest
    /// window rather than the page it asked for.
    #[test]
    fn a_cursor_keeps_its_numbering_across_an_append_driven_drop() {
        let held = Held::new(Conversation::new(
            a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK),
            0,
            0,
        ));
        let dropped_at_seed = held.lock().dropped();
        assert!(dropped_at_seed > 0, "precondition: the seed was over the cap");

        // The page above the newest one, taken while the seat holds what the
        // seed left it.
        let cursor = newest_page(&held, 2).cursor.expect("a page above the newest one");
        let above_before = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                None,
                4,
            )
        });

        // Frames until the appends spend the slack and a second drop runs.
        // A couple past the slack, because the seed's own drop is measured to
        // the turn it lands on and so can leave the held list a frame or two
        // further under the cap.
        for at in 0..=CONVERSATION_SLACK + 2 {
            held.lock().append(a_frame(&format!("later {at}")));
        }
        assert!(
            held.lock().dropped() > dropped_at_seed,
            "precondition: the appends outgrew the cap's slack and a drop ran",
        );

        let above = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                Some(&cursor),
                2,
            )
        });
        assert_eq!(
            turn_texts(&above),
            turn_texts(&above_before)[..2].to_vec(),
            "the page above a cursor is the page above the turn it named, drop or no drop",
        );
    }

    /// **A cursor outlives the drop it was written before.** It names a
    /// message in the conversation's own numbering, so the page above it is
    /// still the page above it - and a cursor the drop has passed is answered
    /// with the floor, which is what the client holding it can actually read,
    /// rather than with whatever now sits at that index in the held list.
    #[test]
    fn a_cursor_survives_a_drop_and_stops_at_the_floor() {
        let conversation =
            Conversation::new(a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK), 0, 0);
        let held = Held::new(conversation);
        let dropped = held.lock().dropped();
        assert!(dropped > 0, "precondition: the seed was over the cap");

        // The newest page, and the cursor it hands back for the page above it.
        let newest = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                None,
                2,
            )
        });
        let cursor = newest.cursor.expect("there is a page above the newest one");

        let above = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                Some(&cursor),
                2,
            )
        });
        let carried = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                None,
                4,
            )
        });
        assert_eq!(
            turn_texts(&above),
            turn_texts(&carried)[..2].to_vec(),
            "the cursor names the turn it was handed for, and the page above it is that page's \
             own",
        );

        // A client whose oldest turn is below the floor is asking for what is
        // above a conversation this seat no longer holds: the page above it
        // is empty, and its `None` is what stops the walk.
        let floor = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                held.dropped_thinking(),
                Some(&dropped.saturating_sub(1).to_string()),
                2,
            )
        });
        assert!(
            floor.turns.is_empty(),
            "a cursor the drop passed is answered with nothing above it: {:?}",
            turn_texts(&floor),
        );
        assert!(
            floor.cursor.is_none(),
            "and with no page above it to ask for: the turn it named is not held any more",
        );
    }

    /// A `ConversationEvictor` that records which seats it was asked to drop,
    /// so a test can see the held copy and the ring go together.
    #[derive(Default)]
    struct RecordingEvictor {
        evicted: Mutex<Vec<SessionSlot>>,
        ring_bytes: Mutex<HashMap<SessionSlot, usize>>,
    }

    impl RecordingEvictor {
        fn evicted(&self) -> Vec<SessionSlot> {
            self.evicted.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }

        fn set_ring_bytes(&self, slot: &SessionSlot, bytes: usize) {
            self.ring_bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(slot.clone(), bytes);
        }
    }

    impl ConversationEvictor for RecordingEvictor {
        fn evict_ring(&self, slot: &SessionSlot) {
            self.evicted.lock().unwrap_or_else(PoisonError::into_inner).push(slot.clone());
            self.ring_bytes.lock().unwrap_or_else(PoisonError::into_inner).remove(slot);
        }

        fn total_ring_bytes(&self) -> usize {
            self.ring_bytes.lock().unwrap_or_else(PoisonError::into_inner).values().sum()
        }

        fn ring_bytes(&self, slot: &SessionSlot) -> usize {
            self.ring_bytes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(slot)
                .copied()
                .unwrap_or(0)
        }
    }

    fn a_connect_for(slot: &SessionSlot, history: Vec<Message>) -> SessionUpdate {
        SessionUpdate::Connected {
            key: slot.clone(),
            session_id: SessionId::new("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45"),
            cwd: "/tmp".to_owned(),
            current_model: a_model(),
            available_models: Vec::new(),
            mode: None,
            history,
            dropped: 0,
            compaction_count: 0,
        }
    }

    fn a_chat_for(slot: &SessionSlot, text: &str) -> SessionUpdate {
        SessionUpdate::ChatAppended { key: slot.clone(), msg: a_frame(text), source: None }
    }

    /// One seat's copy weighs this much, measured rather than assumed.
    fn a_seat_weight() -> usize {
        let history: Vec<Message> = (0..40).map(|i| a_frame(&format!("message {i}"))).collect();
        let weight = Conversation::new(history, 0, 0).bytes();
        assert!(weight > 0, "a seat with forty messages weighs something");
        weight
    }

    /// **The total budget bounds the sum across seats, and the oldest goes
    /// first.** The order is the assertion: whichever comparator a walk uses,
    /// a fourth seat over a three-seat budget must take the oldest and leave
    /// the middle.
    #[test]
    fn the_total_budget_evicts_the_least_recently_touched_seat() {
        let history =
            || -> Vec<Message> { (0..40).map(|i| a_frame(&format!("message {i}"))).collect() };
        let seat = |project: &str| SessionSlot::lead("TestOrg", project);
        let weight = a_seat_weight();
        let budget = weight * 3 + weight / 2;

        let seat_a = seat("proj-a");
        let seat_b = seat("proj-b");
        let seat_c = seat("proj-c");
        let seat_d = seat("proj-d");
        let conversations = Conversations::with_budget(budget, None);
        conversations.apply(&a_connect_for(&seat_a, history()), None);
        conversations.apply(&a_connect_for(&seat_b, history()), None);
        conversations.apply(&a_connect_for(&seat_c, history()), None);
        assert_eq!(conversations.len(), 3, "three copies fit the budget");
        conversations.apply(&a_connect_for(&seat_d, history()), None);
        assert_eq!(conversations.len(), 3, "the fourth evicts exactly one");
        assert!(conversations.get(&seat_a).is_none(), "the oldest seat is the one that goes");
        assert!(conversations.get(&seat_b).is_some(), "the middle seat survives");
        assert!(conversations.get(&seat_c).is_some(), "and the newest of the first three");
        assert!(conversations.get(&seat_d).is_some(), "the seat being served survives");
        assert!(
            conversations.total_retained_bytes() <= budget,
            "the total lands under the budget: {} <= {budget}",
            conversations.total_retained_bytes(),
        );
    }

    /// **The rings count against the same budget, and drop with the seat.**
    /// A seat's retained memory is its held copy and its ring together; a
    /// budget that counted only the copies would let the rings hold the total
    /// over while every copy was evicted to pay for bytes it cannot touch.
    #[test]
    fn ring_bytes_count_against_the_budget_and_drop_with_the_seat() {
        let history =
            || -> Vec<Message> { (0..40).map(|i| a_frame(&format!("message {i}"))).collect() };
        let seat = |project: &str| SessionSlot::lead("TestOrg", project);
        let weight = a_seat_weight();
        let ring_weight = weight / 4;
        assert!(ring_weight > 0, "a ring weighs something too");
        let budget = weight * 2 + weight / 4;

        let seat_a = seat("proj-a");
        let seat_b = seat("proj-b");
        let evictor = Arc::new(RecordingEvictor::default());
        evictor.set_ring_bytes(&seat_a, ring_weight);
        evictor.set_ring_bytes(&seat_b, ring_weight);
        let conversations = Conversations::with_budget(
            budget,
            Some(Arc::clone(&evictor) as Arc<dyn ConversationEvictor>),
        );
        conversations.apply(&a_connect_for(&seat_a, history()), None);
        assert_eq!(conversations.len(), 1, "one copy and its ring fit");
        conversations.apply(&a_connect_for(&seat_b, history()), None);
        assert_eq!(conversations.len(), 1, "the second seat's arrival evicts the older");
        assert_eq!(
            evictor.evicted(),
            vec![seat_a.clone()],
            "the older seat's ring went with its held copy",
        );
        assert!(conversations.get(&seat_b).is_some(), "the seat being served survives");
        assert_eq!(evictor.total_ring_bytes(), ring_weight, "the survivor's ring remains");
        assert!(
            conversations.total_retained_bytes() <= budget,
            "the total lands under the budget: {} <= {budget}",
            conversations.total_retained_bytes(),
        );
    }

    /// **An append is a seat growing, and the budget must see it.** A copy
    /// that grows only by appends has to move the total the moment it grows;
    /// a budget blind to that never trips in a running server, where almost
    /// everything a seat holds arrived after its connect.
    #[test]
    fn appends_grow_the_total_and_a_grown_seat_is_evicted_by_another() {
        let base =
            |n: usize| -> Vec<Message> { (0..n).map(|i| a_frame(&format!("base {i}"))).collect() };
        let extra: Vec<Message> = (0..20).map(|i| a_frame(&format!("added {i}"))).collect();
        let seat_a = SessionSlot::lead("TestOrg", "proj-a");
        let seat_b = SessionSlot::lead("TestOrg", "proj-b");
        let small = Conversation::new(base(5), 0, 0).bytes();
        let mut grown_copy = Conversation::new(base(5), 0, 0);
        for message in &extra {
            grown_copy.append(message.clone());
        }
        let grown = grown_copy.bytes();
        assert!(grown > small, "appends grow a copy");

        // One seat grown by its own frames, and the total follows it there.
        let conversations = Conversations::with_budget(grown + small / 2, None);
        conversations.apply(&a_connect_for(&seat_a, base(5)), None);
        let before = conversations.total_bytes();
        for message in &extra {
            conversations.apply(
                &SessionUpdate::ChatAppended {
                    key: seat_a.clone(),
                    msg: message.clone(),
                    source: None,
                },
                None,
            );
        }
        let after = conversations.total_bytes();
        assert!(after > before, "an append moves the total: {before} -> {after}");
        assert_eq!(
            after,
            conversations.get(&seat_a).expect("held").bytes(),
            "and the total is what the copy weighs",
        );

        // The grown seat is the one the budget must take when another lands.
        conversations.apply(&a_connect_for(&seat_b, base(5)), None);
        assert!(conversations.get(&seat_a).is_none(), "the grown seat is evicted by the arrival");
        assert!(conversations.get(&seat_b).is_some(), "and the arriving seat is served");

        // Never while it is the serving seat, even over budget. The budget is
        // the copy's own weight, so the next append takes it over.
        let conversations = Conversations::with_budget(after, None);
        conversations.apply(&a_connect_for(&seat_a, base(5)), None);
        for message in &extra {
            conversations.apply(
                &SessionUpdate::ChatAppended {
                    key: seat_a.clone(),
                    msg: message.clone(),
                    source: None,
                },
                None,
            );
        }
        conversations.apply(&a_chat_for(&seat_a, "one more"), None);
        assert!(
            conversations.get(&seat_a).is_some(),
            "a seat applying its own frame is never the victim",
        );
        assert!(
            conversations.total_bytes() > conversations.budget_bytes(),
            "even while it is over the budget",
        );
    }

    /// **A read is a use, and the budget counts it.** A seat just read must
    /// not be the next eviction, however long it had been idle before.
    #[test]
    fn a_read_touch_makes_its_seat_the_youngest() {
        let weight = a_seat_weight();
        let budget = weight * 2 + weight / 2;
        let seat_a = SessionSlot::lead("TestOrg", "proj-a");
        let seat_b = SessionSlot::lead("TestOrg", "proj-b");
        let seat_c = SessionSlot::lead("TestOrg", "proj-c");
        let history =
            || -> Vec<Message> { (0..40).map(|i| a_frame(&format!("message {i}"))).collect() };
        let conversations = Conversations::with_budget(budget, None);
        conversations.apply(&a_connect_for(&seat_a, history()), None);
        conversations.apply(&a_connect_for(&seat_b, history()), None);
        assert!(conversations.get(&seat_a).is_some(), "the read finds its copy");
        conversations.apply(&a_connect_for(&seat_c, history()), None);
        assert!(conversations.get(&seat_b).is_none(), "the untouched seat is the one evicted");
        assert!(conversations.get(&seat_a).is_some(), "the seat that was read survives");
        assert!(conversations.get(&seat_c).is_some(), "the seat being served survives");
    }

    /// **The seat being served is never the victim**, even when it is the
    /// oldest: the filter is the serving seat's protection, not the touch's.
    #[test]
    fn the_serving_seat_is_never_the_victim() {
        let weight = a_seat_weight();
        let ring_weight = weight / 2;
        let budget = weight * 2 + weight / 4;
        let seat_a = SessionSlot::lead("TestOrg", "proj-a");
        let seat_b = SessionSlot::lead("TestOrg", "proj-b");
        let history =
            || -> Vec<Message> { (0..40).map(|i| a_frame(&format!("message {i}"))).collect() };
        let evictor = Arc::new(RecordingEvictor::default());
        let conversations = Conversations::with_budget(
            budget,
            Some(Arc::clone(&evictor) as Arc<dyn ConversationEvictor>),
        );
        conversations.apply(&a_connect_for(&seat_a, history()), None);
        conversations.apply(&a_connect_for(&seat_b, history()), None);
        assert_eq!(conversations.len(), 2, "both copies fit before the rings are counted");
        // The rings arrive with no apply to hang the eviction on, so the
        // serving seat is handed to the eviction directly, and it is the
        // OLDER one.
        evictor.set_ring_bytes(&seat_a, ring_weight);
        evictor.set_ring_bytes(&seat_b, ring_weight);
        conversations.evict_over_budget(&seat_a);
        assert!(conversations.get(&seat_a).is_some(), "the protected seat survives");
        assert!(conversations.get(&seat_b).is_none(), "the other seat pays");
    }

    /// **A read in flight holds the budget off its seat.** The read catches
    /// the seat up and then builds a record from the copy; an eviction in that
    /// window would drop what the read just rebuilt.
    #[test]
    fn a_seat_with_a_read_in_flight_is_not_evicted() {
        let weight = a_seat_weight();
        let budget = weight + weight / 2;
        let seat_a = SessionSlot::lead("TestOrg", "proj-a");
        let seat_b = SessionSlot::lead("TestOrg", "proj-b");
        let history =
            || -> Vec<Message> { (0..40).map(|i| a_frame(&format!("message {i}"))).collect() };
        let conversations = Conversations::with_budget(budget, None);
        conversations.apply(&a_connect_for(&seat_a, history()), None);
        let reading = conversations.reading(&seat_a);
        conversations.apply(&a_connect_for(&seat_b, history()), None);
        assert!(conversations.get(&seat_a).is_some(), "the seat being read survives the budget");
        drop(reading);
        conversations.apply(&a_connect_for(&seat_b, history()), None);
        assert!(conversations.get(&seat_a).is_none(), "and goes once the read is done");
        assert!(conversations.get(&seat_b).is_some(), "the seat being served survives");
    }

    /// **A fill in flight holds the budget off its seat.** The fill's applies
    /// must land on a copy that is still there; the eviction skips a seat
    /// whose fill lock is held rather than block the folder behind it.
    #[test]
    fn a_seat_whose_fill_is_running_is_not_evicted() {
        let weight = a_seat_weight();
        let budget = weight + weight / 2;
        let seat_a = SessionSlot::lead("TestOrg", "proj-a");
        let seat_b = SessionSlot::lead("TestOrg", "proj-b");
        let history =
            || -> Vec<Message> { (0..40).map(|i| a_frame(&format!("message {i}"))).collect() };
        let conversations = Conversations::with_budget(budget, None);
        conversations.apply(&a_connect_for(&seat_a, history()), None);
        let fill = conversations.fill_lock(&seat_a);
        let filling = fill.lock().unwrap_or_else(PoisonError::into_inner);
        conversations.apply(&a_connect_for(&seat_b, history()), None);
        assert!(conversations.get(&seat_a).is_some(), "the seat being filled survives the budget");
        drop(filling);
        conversations.apply(&a_connect_for(&seat_b, history()), None);
        assert!(conversations.get(&seat_a).is_none(), "and goes once its fill is done");
    }

    /// **A seat with no copy is a victim too.** Its ring is in the budget's
    /// total, so a budget that could only trade held copies would evict them
    /// all and still be over, with the ring-only seat's bytes untouched.
    #[test]
    fn a_ring_only_seat_is_evicted_by_its_own_recency() {
        let weight = a_seat_weight();
        let seat_held = SessionSlot::lead("TestOrg", "proj-held");
        let seat_ring = SessionSlot::lead("TestOrg", "proj-ring");
        let history =
            || -> Vec<Message> { (0..40).map(|i| a_frame(&format!("message {i}"))).collect() };
        let evictor = Arc::new(RecordingEvictor::default());
        evictor.set_ring_bytes(&seat_ring, weight);
        let conversations = Conversations::with_budget(
            weight,
            Some(Arc::clone(&evictor) as Arc<dyn ConversationEvictor>),
        );
        // The ring-only seat is seen first, at an older ordinal.
        conversations.apply(&a_chat_for(&seat_ring, "a frame with no copy"), Some(1));
        assert!(
            conversations.total_retained_bytes() <= conversations.budget_bytes(),
            "the ring alone fits the budget",
        );
        conversations.apply(&a_connect_for(&seat_held, history()), None);
        assert!(conversations.get(&seat_held).is_some(), "the seat being served survives");
        assert_eq!(
            evictor.evicted(),
            vec![seat_ring.clone()],
            "the ring-only seat's window paid for the overrun",
        );
        assert!(
            conversations.total_retained_bytes() <= conversations.budget_bytes(),
            "the total lands under the budget",
        );
    }
}
