//! One seat's conversation, held by the transport and kept up to date.
//!
//! **The read is what costs.** Measured on a 68.9 MB transcript (13,451
//! messages), one request spends 202 ms reading and parsing the file, 18 ms
//! folding it into turns and 20 ms encoding those turns - so a fix caching
//! only the fold removes 8% of the cost it was written for. The transport
//! holds the read's own output and shares it: a page slices a window out of
//! it, and a subscribe encodes the newest of it.
//!
//! **The seed comes from the task that produces the conversation, never from
//! a second walk of the file.** The fold sees `Connected` for every seat that
//! starts or resumes while the transport runs, and asks for
//! [`SessionUpdate::HistoryReplayed`] for a seat whose connect it missed
//! (a session already running when this transport started). Both are emitted
//! by the session task in its own sequence, so the seed and the frames that
//! follow it have one producer and one order.
//!
//! **That is what makes a buffer unnecessary rather than merely deleted.** A
//! frame emitted before the replay reaches the fold first, finds no
//! conversation, and is dropped - and it is inside the replay's cut, because
//! the task emitted it before it handled the command. A frame emitted after
//! is applied to what the replay left. Two producers is what made the earlier
//! design race with itself; there is one here.
//!
//! **What is released is the front, and no more of it than the cap.** A page
//! cursor names a message in the conversation's own numbering rather than in
//! the held list's, so a cursor a drop has passed is answered as what it is -
//! the floor, with nothing above it - rather than as a window the client did
//! not ask for; and a held copy let go is rebuilt from a replay, which the
//! session task answers out of its own stream - so a frame forge itself
//! forged, which never passes through that stream, would be discarded with
//! it. The seat's copy is therefore kept for the seat's life, and what that
//! costs is bounded by the cap below rather than by the transcript.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use forge_primitives::{Message, SessionSlot};
// The window this copy is kept in, which is the session task's window too: the
// two hold the same conversation, and a source kept shorter than the copy that
// reads it is a page that cannot reach its cap. The cap's why, and what the
// drop costs, are stated there.
use forge_workspace::conversation_window::{CONVERSATION_CAP, CONVERSATION_SLACK, drop_past_cap};

use crate::SessionUpdate;
use crate::transcript::Rendered;

/// Which route a reseed came in by, which is what decides whether the page
/// numbering can be carried over it.
///
/// **The distinction is where the frames came from, not what they say.** A
/// replay is the session task's answer with its own stream: the frames this
/// seat was seeded from, in the order they arrived, so a reseed by it leaves
/// the window - and therefore every cursor a client holds - where it was. A
/// connect, a resume or a `/new` carries a history the CLI handed over or a
/// scan read off the transcript, which is a row subset rather than this
/// copy's frame sequence: an index carried over it would be off by however
/// far the two diverge, so the numbering restarts - a cursor from before
/// resolves by its own number again, as it did before the numbering existed,
/// and is answered from the floor when that number falls below the front the
/// new window starts at.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reseed {
    /// `SessionUpdate::HistoryReplayed`.
    Replay,
    /// `SessionUpdate::Connected` and `SessionUpdate::SessionReplaced`.
    Fresh,
}

/// One seat's conversation: the messages, where its turns sit and how many
/// times it has compacted.
///
/// All three come out of one read and one fold, so a page cut on the fold's
/// boundaries and a record built from `messages` cannot disagree about where a
/// turn begins, and the count cannot describe a different conversation than
/// the one held.
pub struct Conversation {
    messages: Vec<Message>,
    /// The fold's boundaries and task endings, which a page is cut on.
    ///
    /// **Its units are dropped**, because the transport draws nothing: a
    /// client folds the frames a page hands it, and the units exist for a
    /// view. Holding them would be the largest part of this struct's memory
    /// for something nothing here reads.
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
    /// The messages moved since the fold last ran.
    ///
    /// **Memoized rather than made incremental**, because making it
    /// incremental means deciding where a turn begins from the appended
    /// message alone - a second turn-boundary rule, which is the drift
    /// `turn_ranges`' own doc exists to prevent. One rule in one place is
    /// worth 18 ms, and it runs when the messages changed rather than once
    /// per request.
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
    /// The indices, in this conversation's own numbering, of the frames the
    /// drops have carried off that a transcript row does not carry.
    ///
    /// **Why a page below the floor needs them: the two numberings count
    /// different things.** A conversation counts every frame the live stream
    /// emits - a turn's `Result`, its thinking-token frames, a `system`
    /// subtype the CLI never wrote, a delivery row forge forged - and a
    /// transcript counts only its rows. The frames *above* the floor are
    /// still held and can be counted; the ones below it are gone, so the seat
    /// records their positions as it lets them go. Without them, a page below
    /// the floor would be numbered as if every frame had a row and would skip
    /// one row for each rowless frame in between.
    ///
    /// Sorted, ascending: every drop takes a front.
    without_rows: Vec<usize>,
    /// The uuids of the delivery rows this transport forged.
    ///
    /// **A forged row is a frame no transcript has**, and nothing about its
    /// shape says so - it is a user row like the prompt it draws. Only the
    /// site that forged it knows, which is why the set lives here rather than
    /// in the row rule.
    forged: std::collections::HashSet<String>,
}

impl Conversation {
    /// A conversation from the history a connect, a resume or a replay
    /// carried.
    ///
    /// **It does not fold.** The spans come from the next
    /// [`Held::fold`], which runs on a blocking task; folding here would run
    /// an 18 ms render on whatever task built this, and that task is the
    /// socket's single stream folder.
    pub fn new(mut messages: Vec<Message>, compaction_count: u32) -> Self {
        // **The drop runs before the conversion**, so a resume's transient
        // cost is the history it handed over plus a window, rather than two
        // transcripts. What the drop takes is read first, because a frame it
        // takes and no transcript row carries is one the numbering has to
        // keep counting.
        let cut = forge_workspace::conversation_window::frames_dropped(&messages);
        let without_rows = rowless(&messages[..cut], 0, &std::collections::HashSet::new());
        let dropped = drop_past_cap(&mut messages);
        Self {
            messages: as_blocks(messages),
            rendered: Rendered { units: Vec::new(), turns: Vec::new(), endings: HashMap::new() },
            compaction_count,
            dropped,
            dirty: true,
            folding: false,
            without_rows,
            forged: std::collections::HashSet::new(),
        }
    }

    /// The conversation a seat with nothing behind it answers as.
    pub fn empty() -> Self {
        Self::new(Vec::new(), 0)
    }

    /// Replace the whole conversation, which is what a connect, a resume or a
    /// replay says: the history it carries is the truth about this seat.
    ///
    /// **It marks the fold rather than running it**, for the reason
    /// [`Conversation::new`] gives: this is reached from the stream fold, and
    /// a render there stalls every seat rather than this one.
    ///
    /// `route` decides whether the numbering may be carried over the reseed:
    /// see [`Reseed`].
    pub fn seed(&mut self, mut messages: Vec<Message>, compaction_count: u32, route: Reseed) {
        // What the seed's own drop takes, and what a shorter window drops off
        // this seat's front, are frames the numbering keeps counting only if
        // nothing but a row carries them.
        let cut = forge_workspace::conversation_window::frames_dropped(&messages);
        let incoming = rowless(&messages[..cut], 0, &self.forged);
        let lost = self.messages.len().saturating_sub(messages.len());
        let leaving = rowless(&self.messages[..lost], self.dropped, &self.forged);
        let seeded = drop_past_cap(&mut messages);
        // **Converted before the comparison.** The copy a seat already holds
        // went through `as_blocks` at its own seed, so a `<task-notification>`
        // row and the block it converts to are one frame - and compared raw
        // they would read a reseed of the same window as a different one.
        let kept = as_blocks(messages);
        // **The numbering is carried over a replay, and only when the window
        // did not move.** The replay is the seat's own stream: the same frames
        // in the same order, ending at the same one, so a client's cursors
        // still name the turns they named - and the offset moves by however
        // much further back - or shorter - the new window reaches from that
        // end, no further than zero allows. A window that ends elsewhere
        // restarts the numbering instead: a delivery row the transport forged
        // and the task never saw makes one, and so does a tail the reseed
        // dropped. A connect or a resume restarts it always, because the
        // history it carries is a transcript READ - a row subset, not this
        // copy's frame sequence - and an offset carried over that is an offset
        // off by however the two diverged.
        let shifted = self
            .dropped
            .saturating_sub(kept.len().saturating_sub(self.messages.len()))
            .saturating_add(self.messages.len().saturating_sub(kept.len()));
        let carried = route == Reseed::Replay
            && self.messages.last().is_some()
            && self.messages.last() == kept.last();
        self.messages = kept;
        // **The boundaries go with the messages they describe.** The fold's
        // turns name a prefix of the messages, and a reader that saw the new
        // list under the old boundaries would slice off the end of it.
        // Clearing them makes the pair consistent at every instant; the next
        // fold rebuilds them.
        self.rendered.turns.clear();
        self.rendered.endings.clear();
        self.dropped = if carried { shifted } else { seeded };
        // **What the numbering counts below the floor.** A connect restarts
        // it, so nothing above the new floor is needed; a carried replay keeps
        // the seat's own positions and adds the frames this seed took off its
        // front, which the floor has just passed.
        self.without_rows = if carried {
            let mut kept = std::mem::take(&mut self.without_rows);
            kept.retain(|at| *at < self.dropped);
            kept.extend(incoming.into_iter().filter(|at| *at < self.dropped));
            kept.extend(leaving);
            kept
        } else {
            incoming
        };
        // A frame whose id is no longer held cannot be met again, so the set
        // that answers "did forge forge this" holds only what the seat keeps.
        self.forged.retain(|id| self.messages.iter().any(|m| frame_id(m) == Some(id)));
        self.compaction_count = compaction_count;
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
    /// prompt it draws. So the seat keeps their ids, and a page below the
    /// floor counts the frames they were in.
    pub fn append_forged(&mut self, message: Message) {
        if let Some(id) = frame_id(&message) {
            self.forged.insert(id.to_owned());
        }
        self.push(message);
    }

    fn push(&mut self, message: Message) {
        // **A boundary that arrives moves the count**, the same way the
        // session task's own retain does. Without this the record reports the
        // value its last SEED carried, which is stale for every compaction
        // since - and a view draws a marker from it.
        if matches!(message, Message::CompactBoundary { .. }) {
            self.compaction_count = self.compaction_count.saturating_add(1);
        }
        // **Before the push, so the drop pays for the room it makes.** After
        // it, this one frame is what tips the store into a doubling the drop
        // is about to throw away.
        if self.messages.len() >= CONVERSATION_CAP + CONVERSATION_SLACK {
            self.drop_to_cap();
        }
        self.messages.push(message);
        self.dirty = true;
    }

    /// Drop the oldest messages once the held list has outgrown the cap's
    /// slack, and leave the seat due a fold.
    fn drop_to_cap(&mut self) {
        // **Read what is going before it goes**: a frame no transcript row
        // carries is one the numbering keeps counting, and after the drop its
        // place is the only thing that said where it was.
        let cut = forge_workspace::conversation_window::frames_dropped(&self.messages);
        self.without_rows.extend(rowless(&self.messages[..cut], self.dropped, &self.forged));
        self.dropped = self.dropped.saturating_add(drop_past_cap(&mut self.messages));
        // The same reason a seed clears them: the boundaries name message
        // indices, and the drop has just moved every one of them.
        self.rendered.turns.clear();
        self.rendered.endings.clear();
        self.dirty = true;
    }

    /// The row number the transcript read should end a page at, for a page
    /// below the frame `cursor` names.
    ///
    /// The read numbers transcript rows as if every frame were one, because
    /// that is all it can see; the seat counts the frames the transcript
    /// never wrote, so this is where the two numberings meet. `cursor` is a
    /// frame index in this conversation's own numbering, and the number this
    /// returns is the read's: the rows strictly below the cursor, ended after
    /// the newest of them.
    pub fn rows_below_frame(&self, cursor: usize) -> usize {
        // The session's first frame has nothing above it, and a cursor naming
        // it is a client asking for the page above the beginning.
        if cursor == 0 {
            return 0;
        }
        let carried = self.without_rows.len();
        let rowless_at_or_below =
            |frame: usize| self.without_rows.partition_point(|at| *at <= frame);
        // The newest ROW strictly below the cursor: a frame the transcript
        // never wrote is stepped over, because there is no row to end at.
        let cursor = cursor.saturating_sub(1);
        let mut row = cursor;
        while row > 0 && rowless_at_or_below(row) > rowless_at_or_below(row - 1) {
            row -= 1;
        }
        // The read's number for that row, and one past it: a row at true
        // index `row` counts as `row + carried - k(row)` - every rowless
        // frame below the floor shifts it up by one.
        carried + row - rowless_at_or_below(row) + 1
    }

    /// The frame index the transcript read's row number `row` stands for:
    /// the inverse of the numbering [`Self::rows_below_frame`] reads.
    pub fn frame_of_row(&self, row: usize) -> usize {
        let carried = self.without_rows.len();
        let rowless_at_or_below =
            |frame: usize| self.without_rows.partition_point(|at| *at <= frame);
        let mut frame = row.saturating_sub(carried);
        for _ in 0..8 {
            let stepped = row - carried + rowless_at_or_below(frame);
            if stepped <= frame {
                break;
            }
            frame = stepped;
        }
        frame
    }

    /// The messages if the fold is behind them, taken in O(1).
    fn take_dirty(&mut self) -> Option<Vec<Message>> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        self.folding = true;
        Some(std::mem::take(&mut self.messages))
    }

    /// Put a fold back, with whatever arrived while it ran appended.
    ///
    /// `complete` is false when the render did not reach its end, which is the
    /// guard's path: the messages are back but the boundaries installed with
    /// them describe nothing.
    fn merge_fold(&mut self, mut folded: Vec<Message>, rendered: Rendered, complete: bool) {
        let folded_len = folded.len();
        folded.append(&mut self.messages);
        self.messages = folded;
        self.rendered = rendered;
        self.folding = false;
        // The tail arrived while the fold ran and is not in those spans, and a
        // fold that did not finish has no spans at all - either way the next
        // reader folds again rather than cutting a turn on a list that has
        // moved.
        if !complete || self.messages.len() != folded_len {
            self.dirty = true;
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn rendered(&self) -> &Rendered {
        &self.rendered
    }

    pub fn compaction_count(&self) -> u32 {
        self.compaction_count
    }

    /// How many messages the cap has dropped off the front, which is what
    /// turns a held index into the one a page's cursor carries.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// The frames the drops have taken that no transcript row carries: the
    /// count a page below the floor is numbered through.
    pub fn without_rows(&self) -> &[usize] {
        &self.without_rows
    }
}

/// The absolute indices of the frames in `frames` - which start at `from` in
/// the conversation's numbering - that no transcript row carries.
fn rowless(
    frames: &[Message],
    from: usize,
    forged: &std::collections::HashSet<String>,
) -> Vec<usize> {
    frames
        .iter()
        .enumerate()
        .filter(|(_, message)| !carries_a_row(message, forged))
        .map(|(at, _)| from + at)
        .collect()
}

/// Whether a transcript row carries `message`: the kinds the row rule reads,
/// and nothing this transport forged for display.
fn carries_a_row(message: &Message, forged: &std::collections::HashSet<String>) -> bool {
    forge_workspace::has_a_transcript_row(message)
        && !frame_id(message).is_some_and(|id| forged.contains(id))
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
/// a connect or a replay takes the history as the WIRE carries it - so a
/// resumed seat would serve a `<task-notification>` row as the reader's own
/// words, with the ending unread and, for a notice in the same turn as its
/// call, nothing ending that call at all. Idempotent, so a history that came
/// through the read is unchanged.
fn as_blocks(messages: Vec<Message>) -> Vec<Message> {
    crate::transcript::notices_as_blocks(messages)
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
}

impl Held {
    pub fn new(conversation: Conversation) -> Self {
        Self { conversation: Mutex::new(conversation), folded: Condvar::new() }
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
    fn install(&self, messages: Vec<Message>, rendered: Rendered, complete: bool) {
        let mut conversation = self.conversation.lock().unwrap_or_else(PoisonError::into_inner);
        conversation.merge_fold(messages, rendered, complete);
        self.folded.notify_all();
    }

    /// Fold if needed, then read, retrying once if a reseed landed in between.
    ///
    /// **Two acquisitions, and the gap between them is why this retries.** A
    /// `Connected`, a `/resume` or a replay can replace the conversation
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
        let mut rendered = crate::transcript::render(taken.messages());
        // The units are a view's, and the transport draws nothing: a page
        // hands the frames on and the client folds them.
        rendered.units.clear();
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
    messages: Option<Vec<Message>>,
}

impl<'a> Folding<'a> {
    /// Take the messages if a fold is due, or `None` when there is nothing to
    /// do - a fold already running, or nothing moved.
    fn begin(held: &'a Held) -> Option<Self> {
        let mut conversation = held.conversation.lock().unwrap_or_else(PoisonError::into_inner);
        // A fold already running owns the messages; this caller has nothing
        // to do, and waiting here would be waiting on a peer rather than on
        // work it can start.
        if conversation.folding {
            return None;
        }
        let messages = conversation.take_dirty()?;
        Some(Self { held, messages: Some(messages) })
    }

    /// The messages this fold took.
    fn messages(&self) -> &[Message] {
        self.messages.as_deref().unwrap_or(&[])
    }

    /// Put the fold back and wake the readers.
    fn finish(mut self, rendered: Rendered) {
        let messages = self.messages.take().unwrap_or_default();
        self.held.install(messages, rendered, true);
    }
}

impl Drop for Folding<'_> {
    /// Unwinding out of the render: the messages go back, and the seat stays
    /// due a fold so the next read rebuilds the boundaries.
    fn drop(&mut self) {
        let Some(messages) = self.messages.take() else {
            return;
        };
        self.held.install(
            messages,
            // No boundaries: the fold did not reach its end, so the next one
            // rebuilds them rather than this leaving half of one behind.
            Rendered { units: Vec::new(), turns: Vec::new(), endings: HashMap::new() },
            false,
        );
    }
}

/// The conversations the transport is holding, one per live seat.
///
/// **Every live seat, not only the watched ones, and no seat is let go.** A
/// conversation has no read behind it any more, so releasing one is not a
/// bound - nothing could rebuild it, and the next ask for that seat would
/// have no answer. What it holds is therefore proportional to what is
/// RUNNING rather than to what is being read, which is the price of having
/// one producer of the conversation rather than two. What one seat costs is
/// the cap's window rather than its transcript.
#[derive(Default)]
pub struct Conversations {
    held: Mutex<HashMap<SessionSlot, Arc<Held>>>,
    /// Fires when a seat is first held, so a request that had to ask for a
    /// replay knows when its answer has landed.
    seeded: tokio::sync::Notify,
    /// Where each seat's last transcript page stopped, so the page below it is
    /// a seek rather than a read that locates the anchor again.
    ///
    /// **Per seat, and never process-wide**: a transcript position belongs to
    /// the session it was read from, and two seats can hold two sessions. A
    /// position the file has outgrown is caught where it is used - the read
    /// verifies the row it names before trusting it - rather than here.
    transcript_at: Mutex<HashMap<SessionSlot, forge_primitives::TranscriptAnchor>>,
}

impl Conversations {
    pub fn new() -> Self {
        Self::default()
    }

    /// The seat's conversation if one is held.
    pub fn get(&self, slot: &SessionSlot) -> Option<Arc<Held>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner).get(slot).map(Arc::clone)
    }

    /// Hold `conversation` for `slot`, unless the stream got there first.
    pub fn insert(&self, slot: &SessionSlot, conversation: Conversation) -> Arc<Held> {
        let held = {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(
                held.entry(slot.clone()).or_insert_with(|| Arc::new(Held::new(conversation))),
            )
        };
        self.seeded.notify_waiters();
        held
    }

    /// A future that resolves when some seat is newly held.
    ///
    /// **Enable it before looking the seat up**, or a seat seeded between the
    /// lookup and the wait leaves the caller waiting for something that has
    /// already happened - and bound the wait, because a seat whose session is
    /// gone never answers at all.
    pub fn seeded_notice(&self) -> tokio::sync::futures::Notified<'_> {
        self.seeded.notified()
    }

    /// Fold one update into the seat it names.
    pub fn apply(&self, update: &SessionUpdate) {
        let Some(slot) = update.slot() else {
            return;
        };
        match update {
            // A connect, a resume and a replay all say the same thing about
            // this seat - here is its conversation - so they all RESEED it,
            // materialising it if this is the first word about the seat.
            // Appending a replay instead would put the history in front of
            // the frames the seat already carried.
            //
            // **Which of them it is decides whether the page numbering
            // survives the reseed**, so the two routes are named apart even
            // though the work is one: see [`Reseed`].
            SessionUpdate::Connected { history, compaction_count, .. }
            | SessionUpdate::SessionReplaced { history, compaction_count, .. } => {
                self.reseed(slot, history, *compaction_count, Reseed::Fresh);
            }
            SessionUpdate::HistoryReplayed { history, compaction_count, .. } => {
                self.reseed(slot, history, *compaction_count, Reseed::Replay);
            }
            SessionUpdate::ChatAppended { msg, .. } => {
                // **A compaction boundary is a frame like any other.** It
                // once dropped the seat so the next ask would replay it, and
                // that is the door this change closed everywhere else: a
                // replay replaces a copy it did not produce, so dropping to
                // refresh something is how a frame the task never sees gets
                // lost. The boundary is appended, the task counts it, and a
                // reader keeps the history a compaction did not take away.
                if let Some(held) = self.get(slot) {
                    held.lock().append(msg.clone());
                }
            }
            // A delivery is a row a CONNECTION forges per client at send
            // time, so it never arrives here as a `ChatAppended` - and a held
            // copy that ignored it would lose the row while its ANSWER
            // stayed, which is a reply to nothing. Forged once here, where
            // the update arrives, rather than in each connection that sends
            // it.
            _ => {
                if let Some(msg) = crate::delivery::delivery_turn(update, slot)
                    && let Some(held) = self.get(slot)
                {
                    // Forged for display, and in no transcript: the seat has
                    // to know, because a page below the floor numbers the
                    // frames the file never wrote.
                    held.lock().append_forged(msg);
                }
            }
        }
    }

    /// Put `history` on `slot`, materialising the seat when this is the first
    /// word about it.
    ///
    /// **Held means reseed and NOT insert**: `insert` keeps what is there, so
    /// a connect on a seat already carrying a conversation would leave that
    /// conversation in place and the new history discarded.
    fn reseed(
        &self,
        slot: &SessionSlot,
        history: &[Message],
        compaction_count: u32,
        route: Reseed,
    ) {
        match self.get(slot) {
            Some(held) => held.lock().seed(history.to_vec(), compaction_count, route),
            None => {
                self.insert(slot, Conversation::new(history.to_vec(), compaction_count));
            }
        }
    }

    /// The anchor the next page below `before` reads from, when the seat's
    /// walk has already been there: the row its last page stopped at, with the
    /// byte that row starts at.
    ///
    /// `None` when the seat has no such position, or when the request is above
    /// it - a client asking from the floor again - and the read then locates
    /// the caller's own anchor instead.
    pub fn anchor_below(
        &self,
        slot: &SessionSlot,
        before: usize,
    ) -> Option<forge_primitives::TranscriptAnchor> {
        let at =
            self.transcript_at.lock().unwrap_or_else(PoisonError::into_inner).get(slot).cloned()?;
        (at.index >= before && at.offset.is_some()).then_some(at)
    }

    /// Remember where a page's read stopped, for the page below it.
    pub fn remember_anchor(&self, slot: &SessionSlot, at: forge_primitives::TranscriptAnchor) {
        self.transcript_at.lock().unwrap_or_else(PoisonError::into_inner).insert(slot.clone(), at);
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

    use super::*;

    fn a_seat() -> SessionSlot {
        SessionSlot::lead("TestOrg", "proj")
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
                None,
                turns,
            )
        })
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
            compaction_count,
        }
    }

    fn a_replay(history: Vec<Message>, compaction_count: u32) -> SessionUpdate {
        SessionUpdate::HistoryReplayed { key: a_seat(), history, compaction_count }
    }

    /// **The property the whole seed rests on: both routes are a reseed, not
    /// an append.**
    ///
    /// A connect the fold witnessed and a replay it asked for have to leave
    /// the seat in the same state. If either appended instead of replacing, a
    /// seat would carry its history in front of the frames it already had -
    /// and a test that only compared the two messages lists would pass while
    /// the conversation held both.
    #[test]
    fn a_seat_seeded_by_a_connect_and_by_a_replay_are_the_same_conversation() {
        let history = vec![a_frame("first"), a_frame("second")];

        let by_connect = Conversations::new();
        by_connect.insert(&a_seat(), Conversation::new(vec![a_frame("stale")], 0));
        by_connect.apply(&a_connect(history.clone(), 0));

        let by_replay = Conversations::new();
        by_replay.insert(&a_seat(), Conversation::new(vec![a_frame("stale")], 0));
        by_replay.apply(&a_replay(history.clone(), 0));

        let read = |held: &Conversations| {
            let held = held.get(&a_seat()).expect("the seat is held");
            let held = held.lock();
            held.messages().iter().map(said).collect::<Vec<_>>()
        };
        let expected = vec!["first".to_owned(), "second".to_owned()];
        assert_eq!(read(&by_connect), expected, "a connect replaces what the seat held");
        assert_eq!(
            read(&by_replay),
            expected,
            "and a replay replaces it the same way - not appends it in front of what was there",
        );
    }

    /// A frame the session emits after the seed joins the conversation, which
    /// is what the held copy is for.
    #[test]
    fn a_frame_after_a_seed_joins_the_conversation() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::empty());
        held.apply(&a_replay(vec![a_frame("from the replay")], 0));

        held.apply(&SessionUpdate::ChatAppended {
            key: a_seat(),
            msg: a_frame("after"),
            origin: None,
        });

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let conversation = conversation.lock();
        let said: Vec<String> = conversation.messages().iter().map(said).collect();
        assert_eq!(said, vec!["from the replay".to_owned(), "after".to_owned()]);
    }

    /// A compaction the session reports MOVES the seat's count, which is what
    /// a view draws its marker from.
    ///
    /// **The boundary arrives as a frame rather than in a seed**, so a record
    /// that took only the count its last connect carried would report the
    /// number from before every compaction since.
    #[test]
    fn a_compact_boundary_frame_moves_the_seats_count() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::empty());
        held.apply(&a_replay(vec![a_frame("before the compaction")], 0));

        held.apply(&SessionUpdate::ChatAppended {
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
            origin: None,
        });

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
        fresh.apply(&a_connect(vec![a_frame("resumed")], 3));
        let conversation = fresh.get(&a_seat()).expect("the seat is held");
        assert_eq!(conversation.lock().compaction_count(), 3, "a connect's count is kept");

        let reseeded = Conversations::new();
        reseeded.insert(&a_seat(), Conversation::new(vec![a_frame("stale")], 0));
        reseeded.apply(&a_replay(vec![a_frame("replayed")], 2));
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

        held.apply(&a_connect(vec![a_frame("running, unwatched")], 0));

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
        held.insert(&a_seat(), Conversation::new(vec![a_frame("one"), a_frame("two")], 0));
        let conversation = held.get(&a_seat()).expect("the seat is held");
        conversation.fold();
        assert_eq!(conversation.lock().rendered().turns.len(), 2, "precondition: two turns folded");

        held.apply(&a_replay(Vec::new(), 0));

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
        let conversation = Conversation::new(vec![a_frame("kept")], 0);

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

    /// A delivery row the transport forges for a seat is part of the
    /// conversation it is holding.
    ///
    /// **It is forged per CONNECTION at send time, so it never arrives here as
    /// a `ChatAppended`** - and a held copy that ignored it would lose the row
    /// while its answer stayed, which is a reply to nothing. Nothing observed
    /// this: with the append removed the whole workspace suite stays green.
    #[test]
    fn a_forged_delivery_row_joins_the_conversation_it_is_held_for() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::new(vec![a_frame("asked")], 0));

        held.apply(&SessionUpdate::CronPromptAppended {
            key: a_seat(),
            text: "the cron fired".to_owned(),
            uuid: "cap-cron".to_owned(),
        });

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let conversation = conversation.lock();
        let said: Vec<String> = conversation.messages().iter().map(said).collect();
        assert_eq!(
            said,
            vec!["asked".to_owned(), "[Cron]\n\nthe cron fired".to_owned()],
            "the delivery the connection forges is in the conversation a page is built from",
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
        held.insert(&a_seat(), Conversation::new(vec![a_frame("one"), a_frame("two")], 0));

        // A reseed with no fold after it: the boundaries are cleared, so what
        // a reader gets is the whole list as one turn rather than a slice past
        // the end of it.
        held.apply(&a_replay(vec![a_frame("replaced")], 0));

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let page = conversation.read(|held| {
            crate::transport::wire::page(held.messages(), held.rendered(), held.dropped(), None, 5)
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
        let fresh = Conversation::new(vec![a_frame(notice)], 0);
        let mut reseeded = Conversation::new(vec![a_frame("stale")], 0);
        reseeded.seed(vec![a_frame(notice)], 0, Reseed::Fresh);

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
    /// a conversation: a seat whose connect this transport never saw is asked
    /// for a replay, and until that lands there is nothing to append to.
    #[test]
    fn a_frame_before_a_seat_is_seeded_holds_nothing() {
        let held = Conversations::new();

        held.apply(&SessionUpdate::ChatAppended {
            key: a_seat(),
            msg: a_frame("before the seed"),
            origin: None,
        });

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

    /// **The store the drop frees is the point of it.** A `Vec` keeps the
    /// capacity it grew to, so a drop that drained rather than rebuilt would
    /// leave the seat holding a transcript's worth of buffer with a window's
    /// worth of messages in it - which is what the live heap's large
    /// contiguous blocks look like from the inside.
    #[test]
    fn a_conversation_over_the_cap_is_held_in_a_window_sized_store() {
        let one = a_frame("a message");
        let history = vec![one; CONVERSATION_CAP * 4];

        let conversation = Conversation::new(history, 0);

        assert_eq!(conversation.messages().len(), CONVERSATION_CAP, "a seed keeps the cap");
        assert_eq!(conversation.dropped(), CONVERSATION_CAP * 3, "and reports what it left behind");
        assert!(
            conversation.messages.capacity() <= CONVERSATION_CAP + CONVERSATION_SLACK,
            "the store it left is the window's rather than the history's: capacity for {} \
             messages over a history of {}",
            conversation.messages.capacity(),
            CONVERSATION_CAP * 4,
        );
    }

    /// **The drop does not touch the window.** `SUBSCRIBE_TURNS` is what every
    /// client is handed when it attaches, so the page read over a capped
    /// conversation has to answer exactly what it answers over the same
    /// conversation with nothing dropped - the same turns, whole, and a
    /// cursor naming the same message.
    #[test]
    fn the_drop_leaves_the_newest_page_exactly_as_it_was() {
        let messages = a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK);
        let whole = crate::transcript::render(&messages);
        let unbounded = crate::transport::wire::page(
            &messages,
            &whole,
            0,
            None,
            crate::transport::wire::SUBSCRIBE_TURNS,
        );

        let conversation = Conversation::new(messages, 0);
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

    /// **A replay reseed of a seat already held keeps the numbering.** The
    /// transport asks for a replay on a seat it believes nothing holds, and a
    /// connect can land first - so one seat is seeded twice with the same
    /// conversation, from two histories of different lengths. The window does
    /// not move, so the numbering may not either: a client's cursor taken
    /// before the reseed still names the turn it named.
    ///
    /// It is the REPLAY route that carries, and the assertions at the end pin
    /// the other one: a connect's history is a transcript read rather than
    /// this seat's own frames, so it restarts the numbering whatever its
    /// window looks like.
    #[test]
    fn a_reseed_of_the_same_conversation_carries_the_numbering_over() {
        let held =
            Held::new(Conversation::new(a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK), 0));
        let dropped = held.lock().dropped();
        assert!(dropped > 0, "precondition: the connect was over the cap");

        let cursor = newest_page(&held, 2).cursor.expect("a page above the newest one");

        // The replay: the same conversation, ending at the same frame, handed
        // over as the window the session task keeps rather than the transcript
        // the connect carried.
        let window = held.lock().messages().to_vec();
        held.lock().seed(window.clone(), 0, Reseed::Replay);

        let above = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                Some(&cursor),
                2,
            )
        });
        let carried = held.read(|held| {
            crate::transport::wire::page(held.messages(), held.rendered(), held.dropped(), None, 4)
        });
        assert_eq!(
            turn_texts(&above),
            turn_texts(&carried)[..2].to_vec(),
            "the page above a cursor is the page above the turn it named, reseed or no reseed",
        );
        assert_eq!(
            held.lock().dropped(),
            dropped,
            "and the numbering did not move under the client holding it",
        );

        // The other direction: the same conversation reseeded as a window that
        // reaches LESS far back. The offset moves with it, so the turn a
        // cursor names still resolves to the turn it named rather than to
        // whatever sits where it used to.
        let short_by = 1_000;
        let shorter = held.lock().messages()[short_by..].to_vec();
        held.lock().seed(shorter, 0, Reseed::Replay);
        assert_eq!(
            held.lock().dropped(),
            dropped + short_by,
            "a shorter window of the same conversation renumbers its front, not its tail",
        );
        let shorter_above = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                Some(&cursor),
                2,
            )
        });
        assert_eq!(
            turn_texts(&shorter_above),
            turn_texts(&carried)[..2].to_vec(),
            "and the page above the cursor is still that page",
        );

        // A CONNECT hands over a history read off the transcript, which is a
        // row subset rather than this seat's own frame sequence - so the same
        // window by inspection still restarts the numbering, and a cursor from
        // the last one is not resolved against an offset that does not hold.
        held.lock().seed(window, 0, Reseed::Fresh);
        assert_eq!(
            held.lock().dropped(),
            0,
            "a connect's history restarts the numbering whatever its window looks like",
        );
    }

    /// **The window is compared as the seat HOLDS it, not as it arrived.** The
    /// copy a seat carries went through the conversion at its own seed, and a
    /// replay hands its window over raw - so a tail that is one frame in two
    /// shapes (`<task-notification>` as written, and the block it becomes)
    /// would read as a different window and renumber a seat a client is
    /// paging.
    #[test]
    fn a_reseed_whose_raw_tail_is_the_held_tail_still_carries() {
        let notice = "<task-notification><tool-use-id>tu1</tool-use-id>\
                      <status>completed</status><summary>done</summary></task-notification>";
        let mut history = a_long_history(CONVERSATION_CAP);
        history.push(a_frame(notice));

        let held = Held::new(Conversation::new(history, 0));
        let dropped = held.lock().dropped();
        assert!(dropped > 0, "precondition: the history was over the cap");

        // The same window as the seat carries, with the notice in the shape a
        // replay hands over rather than the shape the seed converted it to.
        let mut window = held.lock().messages()[..CONVERSATION_CAP - 1].to_vec();
        window.push(a_frame(notice));
        held.lock().seed(window, 0, Reseed::Replay);

        assert_eq!(
            held.lock().dropped(),
            dropped,
            "the seat's own window handed over raw is still the seat's window",
        );
    }

    /// **Which update arrived decides the route**, pinned where the update is
    /// read rather than where the seed is called: a record that passed the
    /// wrong one would renumber a seat a client is paging, or carry an offset
    /// a transcript read never earned.
    #[test]
    fn the_update_decides_whether_the_numbering_survives_the_reseed() {
        let held = Conversations::new();
        held.apply(&a_connect(a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK), 0));
        let conversation = held.get(&a_seat()).expect("the seat is held");
        let dropped = conversation.lock().dropped();
        assert!(dropped > 0, "precondition: the connect was over the cap");

        let window = conversation.lock().messages().to_vec();
        held.apply(&a_replay(window.clone(), 0));
        assert_eq!(
            conversation.lock().dropped(),
            dropped,
            "a replay of the seat's own frames carries the numbering",
        );

        held.apply(&a_connect(window, 0));
        assert_eq!(
            conversation.lock().dropped(),
            0,
            "and a connect's history restarts it, however its window looks",
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
        let held =
            Held::new(Conversation::new(a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK), 0));
        let dropped_at_seed = held.lock().dropped();
        assert!(dropped_at_seed > 0, "precondition: the seed was over the cap");

        // The page above the newest one, taken while the seat holds what the
        // seed left it.
        let cursor = newest_page(&held, 2).cursor.expect("a page above the newest one");
        let above_before = held.read(|held| {
            crate::transport::wire::page(held.messages(), held.rendered(), held.dropped(), None, 4)
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
            Conversation::new(a_long_history(CONVERSATION_CAP + CONVERSATION_SLACK), 0);
        let held = Held::new(conversation);
        let dropped = held.lock().dropped();
        assert!(dropped > 0, "precondition: the seed was over the cap");

        // The newest page, and the cursor it hands back for the page above it.
        let newest = held.read(|held| {
            crate::transport::wire::page(held.messages(), held.rendered(), held.dropped(), None, 2)
        });
        let cursor = newest.cursor.expect("there is a page above the newest one");

        let above = held.read(|held| {
            crate::transport::wire::page(
                held.messages(),
                held.rendered(),
                held.dropped(),
                Some(&cursor),
                2,
            )
        });
        let carried = held.read(|held| {
            crate::transport::wire::page(held.messages(), held.rendered(), held.dropped(), None, 4)
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
}
