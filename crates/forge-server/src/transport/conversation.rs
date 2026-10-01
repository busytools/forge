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
//! **Nothing is released, and nothing drops messages from the front.** A page
//! cursor is a message index, so a prefix dropped under a client holding one
//! would answer the wrong window silently; and a held copy let go is rebuilt
//! from a replay, which the session task answers out of its own stream - so a
//! frame forge itself forged, which never passes through that stream, would
//! be discarded with it. The seat's copy is therefore kept for the seat's
//! life, and the memory is one conversation per seat that has ever connected.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use forge_primitives::{ContentBlock, Message, SessionSlot};

use crate::SessionUpdate;
use crate::transcript::{Rendered, names_a_dispatch};

/// One seat's conversation: the messages, where its turns sit, how many times
/// it has compacted, and whether a sub-agent was ever dispatched in it.
///
/// All four come out of one read and one fold, so a page cut on the fold's
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
    has_dispatches: bool,
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
}

impl Conversation {
    /// A conversation from the history a connect, a resume or a replay
    /// carried.
    ///
    /// **It does not fold.** The spans come from the next
    /// [`Held::fold`], which runs on a blocking task; folding here would run
    /// an 18 ms render on whatever task built this, and that task is the
    /// socket's single stream folder.
    ///
    /// `has_dispatches` is the exception and it is a scan rather than a
    /// render: it has to be right the moment a conversation exists, because a
    /// record may read it before any fold has run.
    pub fn new(messages: Vec<Message>, compaction_count: u32) -> Self {
        let messages = as_blocks(messages);
        let has_dispatches = messages.iter().any(is_dispatch);
        Self {
            messages,
            rendered: Rendered { units: Vec::new(), turns: Vec::new(), endings: HashMap::new() },
            compaction_count,
            has_dispatches,
            dirty: true,
            folding: false,
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
    pub fn seed(&mut self, messages: Vec<Message>, compaction_count: u32) {
        self.messages = as_blocks(messages);
        // **The boundaries go with the messages they describe.** The fold's
        // turns name a prefix of the messages, and a reader that saw the new
        // list under the old boundaries would slice off the end of it.
        // Clearing them makes the pair consistent at every instant; the next
        // fold rebuilds them.
        self.rendered.turns.clear();
        self.rendered.endings.clear();
        self.compaction_count = compaction_count;
        self.has_dispatches = self.messages.iter().any(is_dispatch);
        self.dirty = true;
    }

    /// One frame the session emitted.
    pub fn append(&mut self, message: Message) {
        // **A boundary that arrives moves the count**, the same way the
        // session task's own retain does. Without this the record reports the
        // value its last SEED carried, which is stale for every compaction
        // since - and a view draws a marker from it.
        if matches!(message, Message::CompactBoundary { .. }) {
            self.compaction_count = self.compaction_count.saturating_add(1);
        }
        self.has_dispatches |= is_dispatch(&message);
        self.messages.push(message);
        self.dirty = true;
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

    /// Whether the conversation holds a sub-agent dispatch.
    ///
    /// **Computed here because it is a fact about the conversation and not
    /// about a window of it.** The inspector's subagents section reads it,
    /// and it used to scan every frame of every turn to decide - which a
    /// bounded page would have blinded, reporting "no sub-agents ran" for a
    /// seat that dispatched one an hour ago.
    ///
    /// The rule is the client's own, kept identical: an assistant frame that
    /// is not a sub-agent's, carrying a `Task` or `Agent` call.
    pub fn has_dispatches(&self) -> bool {
        self.has_dispatches
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

/// A frame that dispatches a sub-agent, as the conversation sees one.
fn is_dispatch(message: &Message) -> bool {
    let Message::Assistant { message, parent_tool_use_id, .. } = message else {
        return false;
    };
    if names_a_dispatch(parent_tool_use_id.as_deref()) {
        return false;
    }
    message.content.iter().any(|block| {
        matches!(block, ContentBlock::ToolUse { name, .. } if name == "Task" || name == "Agent")
    })
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
/// **Every live seat, not only the watched ones, and nothing is released.**
/// A conversation has no read behind it any more, so letting one go is not a
/// bound - nothing could rebuild it, and the next ask for that seat would
/// have no answer. What it holds is therefore proportional to what is
/// RUNNING rather than to what is being read, which is the price of having
/// one producer of the conversation rather than two.
#[derive(Default)]
pub struct Conversations {
    held: Mutex<HashMap<SessionSlot, Arc<Held>>>,
    /// Fires when a seat is first held, so a request that had to ask for a
    /// replay knows when its answer has landed.
    seeded: tokio::sync::Notify,
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
            SessionUpdate::Connected { history, compaction_count, .. }
            | SessionUpdate::SessionReplaced { history, compaction_count, .. }
            | SessionUpdate::HistoryReplayed { history, compaction_count, .. } => {
                // Held means reseed and NOT insert: `insert` keeps what is
                // there, so a connect on a seat already carrying a
                // conversation would leave that conversation in place and the
                // new history discarded.
                match self.get(slot) {
                    Some(held) => held.lock().seed(history.clone(), *compaction_count),
                    None => {
                        self.insert(slot, Conversation::new(history.clone(), *compaction_count));
                    }
                }
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
                    held.lock().append(msg);
                }
            }
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
    use forge_primitives::SessionId;

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
    fn an_assistant_frame(tool: &str, parent: Option<&str>) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "m1",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{"type": "tool_use", "id": "tu1", "name": tool, "input": {}}],
            },
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
            "parent_tool_use_id": parent,
        }))
        .expect("an assistant frame")
    }

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
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn a_connect(history: Vec<Message>) -> SessionUpdate {
        SessionUpdate::Connected {
            key: a_seat(),
            session_id: SessionId::new("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45"),
            cwd: "/tmp".to_owned(),
            current_model: a_model(),
            available_models: Vec::new(),
            mode: None,
            history,
            compaction_count: 0,
        }
    }

    fn a_replay(history: Vec<Message>) -> SessionUpdate {
        SessionUpdate::HistoryReplayed { key: a_seat(), history, compaction_count: 0 }
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
        by_connect.apply(&a_connect(history.clone()));

        let by_replay = Conversations::new();
        by_replay.insert(&a_seat(), Conversation::new(vec![a_frame("stale")], 0));
        by_replay.apply(&a_replay(history.clone()));

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
        held.apply(&a_replay(vec![a_frame("from the replay")]));

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
        held.apply(&a_replay(vec![a_frame("before the compaction")]));

        held.apply(&SessionUpdate::ChatAppended {
            key: a_seat(),
            msg: Message::CompactBoundary {
                trigger: "auto".to_owned(),
                pre_tokens: 100_000,
                uuid: "b1c2d3e4-0000-4000-8000-000000000001".to_owned(),
                session_id: "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45".to_owned(),
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

    /// A connect holds the seat's conversation whether or not anyone is
    /// watching it, because nothing can rebuild one that was let go: the
    /// conversation has no read behind it, so a seat dropped is a seat whose
    /// next ask has no answer.
    #[test]
    fn a_connect_holds_the_seat_without_anyone_watching() {
        let held = Conversations::new();

        held.apply(&a_connect(vec![a_frame("running, unwatched")]));

        assert_eq!(held.len(), 1, "the seat is held from its connect");
        let conversation = held.get(&a_seat()).expect("the seat is held");
        let conversation = conversation.lock();
        assert_eq!(conversation.messages().len(), 1, "carrying the history the connect brought");
    }

    /// **The dispatch rule lives here now, and this is its test.** It was the
    /// client's, scanned over every frame of every turn to decide whether to
    /// draw the inspector's subagents section; a bounded page can no longer
    /// answer it that way, so the conversation computes it where it is folded
    /// and the record carries it.
    ///
    /// The rule is the client's own, kept identical: an assistant frame whose
    /// `parent_tool_use_id` is absent or blank - `names_a_dispatch`'s
    /// non-empty-string guard, which is what the client checked - carrying a
    /// `Task` or `Agent` call. A sub-agent's own calls are the sub-agent's and
    /// do not count for the session that dispatched it.
    #[test]
    fn a_dispatch_is_an_assistant_frame_calling_task_or_agent() {
        let dispatch = |message| Conversation::new(vec![message], 0).has_dispatches();

        assert!(dispatch(an_assistant_frame("Task", None)), "a Task call is a dispatch");
        assert!(dispatch(an_assistant_frame("Agent", None)), "and so is an Agent call");
        assert!(!dispatch(an_assistant_frame("Bash", None)), "a Bash call is not one");
        assert!(
            !dispatch(an_assistant_frame("Task", Some("tu-parent"))),
            "and a sub-agent's own Task call is the sub-agent's, not this session's",
        );
        assert!(!dispatch(a_frame("hello")), "a user frame dispatches nothing");
    }

    /// The rule answers for a frame the session emits after the seed too, not
    /// only for the history a connect carried - otherwise a dispatch made
    /// while a client is watching would go unnoticed until the seat was
    /// re-seeded.
    #[test]
    fn a_dispatch_appended_after_a_seed_is_seen() {
        let held = Conversations::new();
        held.insert(&a_seat(), Conversation::empty());
        held.apply(&a_replay(vec![a_frame("nothing dispatched here")]));

        let read = || {
            let conversation = held.get(&a_seat()).expect("the seat is held");
            let conversation = conversation.lock();
            conversation.has_dispatches()
        };
        assert!(!read(), "precondition: the seeded conversation holds no dispatch");

        held.apply(&SessionUpdate::ChatAppended {
            key: a_seat(),
            msg: an_assistant_frame("Task", None),
            origin: None,
        });

        assert!(read(), "a dispatch appended after the seed is seen");
    }

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

        held.apply(&a_replay(Vec::new()));

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
        held.apply(&a_replay(vec![a_frame("replaced")]));

        let conversation = held.get(&a_seat()).expect("the seat is held");
        let page = conversation
            .read(|held| crate::transport::wire::page(held.messages(), held.rendered(), None, 5));
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
        reseeded.seed(vec![a_frame(notice)], 0);

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
}
