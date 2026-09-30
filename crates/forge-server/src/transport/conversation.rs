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
//! Nothing drops messages from the front. A page cursor is a message index,
//! so a prefix dropped under a client holding one would answer the wrong
//! window silently. Memory follows what is being watched instead: a
//! conversation is released after the last connection showing its seat goes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use forge_primitives::{ContentBlock, Message, SessionSlot};

use crate::SessionUpdate;
use crate::transcript::{TurnSpan, names_a_dispatch};

/// One seat's conversation: the messages, where its turns sit, how many times
/// it has compacted, and whether a sub-agent was ever dispatched in it.
///
/// All four come out of one read and one fold, so a page cut on `spans` and a
/// record built from `messages` cannot disagree about where a turn begins,
/// and the count cannot describe a different conversation than the one held.
pub struct Conversation {
    messages: Vec<Message>,
    spans: Vec<TurnSpan>,
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
    /// [`Conversation::fold_held`] is where it runs instead.
    dirty: bool,
}

impl Conversation {
    /// A conversation from the history a connect, a resume or a replay
    /// carried.
    pub fn new(messages: Vec<Message>, compaction_count: u32) -> Self {
        let mut conversation = Self {
            messages,
            spans: Vec::new(),
            compaction_count,
            has_dispatches: false,
            dirty: true,
        };
        conversation.refold();
        conversation
    }

    /// The conversation a seat with nothing behind it answers as.
    pub fn empty() -> Self {
        Self::new(Vec::new(), 0)
    }

    /// Replace the whole conversation, which is what a connect, a resume or a
    /// replay says: the history it carries is the truth about this seat.
    pub fn seed(&mut self, messages: Vec<Message>, compaction_count: u32) {
        self.messages = messages;
        self.compaction_count = compaction_count;
        self.refold();
    }

    /// One frame the session emitted.
    pub fn append(&mut self, message: Message) {
        self.has_dispatches |= is_dispatch(&message);
        self.messages.push(message);
        self.dirty = true;
    }

    fn refold(&mut self) {
        self.spans = crate::transcript::render(&self.messages).turns;
        self.has_dispatches = self.messages.iter().any(is_dispatch);
        self.dirty = false;
    }

    /// Fold if the messages moved, outside this lock and off the reactor.
    ///
    /// The caller is a blocking task that has nothing else to do with the
    /// conversation until this returns.
    pub fn fold_held(held: &Mutex<Self>) {
        let folded = {
            let mut conversation = held.lock().unwrap_or_else(PoisonError::into_inner);
            conversation.take_dirty()
        };
        let Some(messages) = folded else {
            return;
        };
        let spans = crate::transcript::render(&messages).turns;
        let mut conversation = held.lock().unwrap_or_else(PoisonError::into_inner);
        conversation.merge_fold(messages, spans);
    }

    /// The messages if the fold is behind them, taken in O(1).
    fn take_dirty(&mut self) -> Option<Vec<Message>> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(std::mem::take(&mut self.messages))
    }

    /// Put a fold back, with whatever arrived while it ran appended.
    fn merge_fold(&mut self, mut folded: Vec<Message>, spans: Vec<TurnSpan>) {
        let folded_len = folded.len();
        folded.append(&mut self.messages);
        self.messages = folded;
        self.spans = spans;
        // The tail arrived while the fold ran and is not in those spans, so
        // the next reader folds again rather than cutting a turn on a list
        // that has moved.
        if self.messages.len() != folded_len {
            self.dirty = true;
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn spans(&self) -> &[TurnSpan] {
        &self.spans
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
    held: Mutex<HashMap<SessionSlot, Arc<Mutex<Conversation>>>>,
    /// Fires when a seat is first held, so a request that had to ask for a
    /// replay knows when its answer has landed.
    seeded: tokio::sync::Notify,
}

impl Conversations {
    pub fn new() -> Self {
        Self::default()
    }

    /// The seat's conversation if one is held.
    pub fn get(&self, slot: &SessionSlot) -> Option<Arc<Mutex<Conversation>>> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner).get(slot).map(Arc::clone)
    }

    /// Hold `conversation` for `slot`, unless the stream got there first.
    pub fn insert(
        &self,
        slot: &SessionSlot,
        conversation: Conversation,
    ) -> Arc<Mutex<Conversation>> {
        let held = {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(
                held.entry(slot.clone()).or_insert_with(|| Arc::new(Mutex::new(conversation))),
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
                let fresh = Conversation::new(history.clone(), *compaction_count);
                self.insert(slot, fresh);
            }
            SessionUpdate::ChatAppended { msg, .. } => {
                let Some(held) = self.get(slot) else {
                    return;
                };
                // A compaction rewrote the conversation the CLI holds, and
                // what the held copy should become is a question only the
                // transcript can answer. Dropping it costs one replay at the
                // next ask and cannot keep a conversation the CLI threw away.
                if matches!(msg, Message::CompactBoundary { .. }) {
                    self.held.lock().unwrap_or_else(PoisonError::into_inner).remove(slot);
                    return;
                }
                held.lock().unwrap_or_else(PoisonError::into_inner).append(msg.clone());
            }
            _ => {}
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
            let held = held.lock().expect("the lock");
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
        let conversation = conversation.lock().expect("the lock");
        let said: Vec<String> = conversation.messages().iter().map(said).collect();
        assert_eq!(said, vec!["from the replay".to_owned(), "after".to_owned()]);
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
        let conversation = conversation.lock().expect("the lock");
        assert_eq!(conversation.messages().len(), 1, "carrying the history the connect brought");
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
