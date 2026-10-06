//! The composer's live state: the take a seat is dictating, the notice a
//! finished take left, and the prompt a seat is parked on.
//!
//! The wire announces each of these once and retains nothing, so what a
//! composer draws is folded here from the stream. A view that attaches to an
//! already-running session reads none of it from the core, which is why it
//! lives beside the surface rather than in the view.
//!
//! **The take and the notice are read only in-process** - by the parked web
//! view, which folds beside the sessions - and never cross the socket: a
//! take belongs to the connection that started it, so the record says
//! nothing about one and its updates go to that connection alone.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use forge_primitives::Message;
use forge_primitives::SessionSlot;

use crate::SessionUpdate;
// One prompt as a composer draws it, whichever copy it came from: the wire's,
// or the one the core kept beside the answer's oneshot for a view that
// attached after it landed.
use crate::surface::DictateOutcome;
use crate::surface::PendingAsk as Ask;

/// How many level readings the meter keeps: at the mockup's own six pixels a
/// cell, a little over 700 pixels of track. A slot narrower than that is
/// filled edge to edge and the oldest readings clip, which is the case for a
/// session column at 1440; a wider one shares the leftover across the cells,
/// so the window reaches the left edge too.
const METER_CELLS: usize = 120;

/// The top of the meter's own scale, in dBFS. A reading is measured between
/// the take's own silence floor and this.
const METER_CEILING_DB: f32 = 0.0;

/// The shortest a meter cell is drawn: a cell is a past reading rather than
/// a pulse, so the quietest one still has to be visible as one.
const METER_FLOOR_PERCENT: f32 = 12.0;

/// What a take is doing, as a composer draws it.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Recording,
    Transcribing,
}

/// A dictation take, as the stream reported it. Folded in [`Composer::apply`]
/// because the wire announces each take once and retains nothing.
#[derive(Clone)]
pub struct Take {
    /// The take's own number among the seat's takes. A resolver or a
    /// progress report for an older one is about a take that is over.
    generation: u64,
    pub phase: Phase,
    /// The take's own silence floor, which the meter measures against
    /// rather than assuming one.
    floor_db: f32,
    /// Newest reading last, each a fraction of the take's own range.
    pub levels: VecDeque<f32>,
    /// The newest reading in dBFS, for the row's own figure.
    pub peak_db: f32,
    /// Settled segments, and their total once the take is closed. A live
    /// take cannot know its own total.
    pub progress: (usize, Option<usize>),
    pub started: Instant,
}

impl Take {
    fn new(floor_db: f32, generation: u64) -> Self {
        Self {
            generation,
            phase: Phase::Recording,
            floor_db,
            levels: VecDeque::new(),
            peak_db: floor_db,
            progress: (0, None),
            started: Instant::now(),
        }
    }

    /// One reading, as a fraction of the take's own range. A non-finite
    /// reading is silence rather than a level: the floor stands in for it,
    /// which is what makes structural silence read as the floor rather
    /// than as a spike.
    fn push(&mut self, peak_db: f32) {
        let reading = if peak_db.is_finite() { peak_db } else { self.floor_db };
        self.peak_db = reading;
        let span = (METER_CEILING_DB - self.floor_db).max(1.0);
        let fraction = ((reading - self.floor_db) / span).clamp(0.0, 1.0);
        if self.levels.len() >= METER_CELLS {
            self.levels.pop_front();
        }
        self.levels.push_back(fraction);
    }

    /// How tall a meter draws `level`, as a percentage of the bar.
    pub fn height(level: f32) -> f32 {
        METER_FLOOR_PERCENT + level * (100.0 - METER_FLOOR_PERCENT - 4.0)
    }
}

/// What a finished take left behind.
#[derive(Clone)]
pub enum Notice {
    /// The take's words, which the box takes at the caret. `truncated`
    /// means the take hit its cap and is partial.
    Landed { text: String, truncated: bool },
    /// A line about a take that produced nothing to insert.
    Line { tone: &'static str, text: String },
}

impl Notice {
    /// The notice a finished take leaves, worded per outcome. A take that
    /// landed leaves words rather than a line, and one the reader abandoned
    /// leaves nothing at all.
    fn of(outcome: &DictateOutcome, floor_db: f32) -> Option<Self> {
        let line = |tone, text: String| Some(Self::Line { tone, text });
        match outcome {
            DictateOutcome::Landed { text, truncated } => {
                Some(Self::Landed { text: text.clone(), truncated: *truncated })
            }
            DictateOutcome::Cancelled => None,
            DictateOutcome::Empty => {
                line("q", "that was all filler \u{b7} nothing to insert".to_owned())
            }
            DictateOutcome::NoAudio { peak_db, seconds } if peak_db.is_finite() => line(
                "q",
                format!(
                    "nothing above {} dBFS in {seconds}s \u{b7} loudest was {peak_db:.1} \
                     \u{b7} try again",
                    floor_db.round()
                ),
            ),
            DictateOutcome::NoAudio { .. } => line(
                "bad",
                "no signal from the microphone at all \u{b7} check permission or mute".to_owned(),
            ),
            DictateOutcome::Refused { message } => line("bad", message.clone()),
            DictateOutcome::Failed => line(
                "q",
                "dictation failed \u{b7} try again; restart forge if it repeats".to_owned(),
            ),
        }
    }

    /// The line this notice draws, if it draws one. A landed take draws its
    /// words in the box instead.
    pub fn line(&self) -> Option<String> {
        match self {
            Self::Landed { truncated: true, .. } => {
                Some("this is what fitted \u{b7} keep going from the end".to_owned())
            }
            Self::Landed { .. } => None,
            Self::Line { text, .. } => Some(text.clone()),
        }
    }

    pub fn tone(&self) -> &'static str {
        match self {
            Self::Landed { .. } => "warn",
            Self::Line { tone, .. } => tone,
        }
    }
}

/// The core's pending set is what says a prompt waits; this holds only what
/// it offers. A take is here for the same reason, and its generation is what
/// keeps an older take's reports from drawing over a newer one.
#[derive(Default, Clone)]
pub struct Composer {
    takes: HashMap<SessionSlot, Take>,
    notices: HashMap<SessionSlot, Notice>,
    compacting: HashSet<SessionSlot>,
    /// The prompts each seat is holding: a draft leads, then arrival order.
    /// A parallel batch parks several at once, and the dock draws the front.
    asks: HashMap<SessionSlot, Vec<Ask>>,
    sign_ins: HashMap<SessionSlot, SignIn>,
}

/// The sign-in a seat is waiting on, as the wire names it. The method is what
/// makes the hint say which account rather than only that one is needed.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SignIn {
    pub method_name: String,
    pub method_description: String,
}

impl Take {
    /// The take's own silence floor, which its meter measures against rather
    /// than assuming a fixed one.
    pub fn floor_db(&self) -> f32 {
        self.floor_db
    }

    /// How long the take has run. `Instant` is a Rust mechanism and does not
    /// cross, so what crosses is the duration a client draws.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }
}

impl Composer {
    /// Fold one update in, answering whether a composer has to be redrawn.
    ///
    /// An arm that consumes what it resolves answers for the update rather
    /// than for the state it landed on, because the boot's fold applies
    /// every update before a session stream sees it: a take's end read off
    /// what a second application changed would be false for whoever applied
    /// second, and the box would keep drawing a take the fold had taken.
    /// Every other arm is answered by its own guard, which holds the same
    /// way on both applications.
    pub fn apply(&mut self, update: &SessionUpdate) -> bool {
        match update {
            SessionUpdate::DictateStarted { key, floor_db, generation, .. } => {
                // A new take supersedes whatever the seat was doing, its
                // notice included: the words it left are already in the
                // draft the browser holds.
                self.takes.insert(key.clone(), Take::new(*floor_db, *generation));
                self.notices.remove(key);
                true
            }
            SessionUpdate::DictateLevel { key, peak_db, .. } => {
                // The wire carries no generation on a level, and the
                // stream is one order per seat, so the take it belongs to
                // is whichever is live: the level that arrives after one
                // ended finds none and is dropped. The other seat's page
                // must not be redrawn for it either: a take's fifty
                // readings a second are news to the composer holding it.
                if let Some(take) = self.takes.get_mut(key) {
                    take.push(*peak_db);
                    return true;
                }
                false
            }
            SessionUpdate::DictateTranscribing { key, .. } => {
                if let Some(take) = self.takes.get_mut(key) {
                    take.phase = Phase::Transcribing;
                    return true;
                }
                false
            }
            SessionUpdate::DictateProgress { key, generation, done, total, .. } => {
                if let Some(take) = self.takes.get_mut(key)
                    && take.generation == *generation
                {
                    take.progress = (*done, *total);
                    return true;
                }
                false
            }
            SessionUpdate::DictateEnded { key, outcome, generation, .. } => {
                // The take goes only if it is the one this resolves - a
                // refusal resolves none, and a tail from a take that is gone
                // is not this one - but the answer is the seat's either way,
                // because the fold that ran first already took the state.
                if matches!(outcome, DictateOutcome::Refused { .. })
                    || self.takes.get(key).is_some_and(|take| take.generation == *generation)
                {
                    let floor_db = self.takes.remove(key).map_or(-50.0, |take| take.floor_db);
                    if let Some(notice) = Notice::of(outcome, floor_db) {
                        self.notices.insert(key.clone(), notice);
                    }
                }
                true
            }
            SessionUpdate::AuthRequired { key, method_name, method_description } => {
                self.sign_ins.insert(
                    key.clone(),
                    SignIn {
                        method_name: method_name.clone(),
                        method_description: method_description.clone(),
                    },
                );
                true
            }
            SessionUpdate::PermissionRequest { key, request, .. } => {
                let ask = Ask::Permission(Box::new(request.clone()));
                self.park(key, ask);
                true
            }
            SessionUpdate::QuestionRequest { key, request, .. } => {
                let ask = Ask::Question(Box::new(request.clone()));
                self.park(key, ask);
                true
            }
            SessionUpdate::SlackPostPending { key, draft } => {
                let ask = Ask::SlackDraft(Box::new(draft.clone()));
                self.park(key, ask);
                true
            }
            // The draft left the core's registry - answered in whichever
            // view, expired, or its asking session gone - so this copy goes
            // with it, or a page keeps drawing a decision no answer can
            // reach.
            SessionUpdate::SlackDraftResolved { key, id, .. } => {
                let emptied = match self.asks.get_mut(key) {
                    Some(queue) => {
                        queue.retain(|ask| !matches!(ask, Ask::SlackDraft(held) if held.id == *id));
                        queue.is_empty()
                    }
                    None => false,
                };
                if emptied {
                    self.asks.remove(key);
                }
                // True whatever this view held, for the same reason the arm
                // below is: the draft is gone from the core.
                true
            }
            SessionUpdate::BrowserHandOffPending { key, handoff } => {
                let ask = Ask::BrowserHandOff(Box::new(handoff.clone()));
                self.park(key, ask);
                true
            }
            // Same as the draft's resolution: the hand-off left the core's
            // registry, so a page that keeps drawing it offers a prompt no
            // answer can reach.
            SessionUpdate::BrowserHandOffResolved { key, id, .. } => {
                let emptied = match self.asks.get_mut(key) {
                    Some(queue) => {
                        queue.retain(
                            |ask| !matches!(ask, Ask::BrowserHandOff(held) if held.id == *id),
                        );
                        queue.is_empty()
                    }
                    None => false,
                };
                if emptied {
                    self.asks.remove(key);
                }
                true
            }
            // The prompt is settled, so its dock goes. This is the only
            // thing on the stream that says so: answering leaves the core's
            // pending set either way, and a view that answered from another
            // seat's page would otherwise keep drawing it.
            SessionUpdate::PendingInteractionResolved { key, tool_id, question_index } => {
                let emptied = match self.asks.get_mut(key) {
                    Some(queue) => {
                        queue.retain(|ask| !resolved(ask, tool_id, *question_index));
                        queue.is_empty()
                    }
                    None => false,
                };
                if emptied {
                    self.asks.remove(key);
                }
                // True whatever this view held: the prompt is gone from the
                // core, and a view that never had the ask still draws the
                // dock from the core's own record of what is pending.
                true
            }
            // The CLI announces a compaction on the status frame and
            // clears it with a null, which is the only place either is
            // said. Everything else on the conversation is the chat's.
            SessionUpdate::ChatAppended { key, msg, .. } => {
                if let Message::System { subtype, data, .. } = msg
                    && subtype == "status"
                {
                    // True for either status rather than for the change it
                    // makes: the boot's own fold applies every update first,
                    // so a second application sees no difference and a
                    // difference is not what the answer is about - the status
                    // frame is the composer's news and the box is redrawn.
                    let field = data.get("status");
                    if field.and_then(serde_json::Value::as_str) == Some("compacting") {
                        self.compacting.insert(key.clone());
                        return true;
                    }
                    if field.is_some_and(serde_json::Value::is_null) {
                        self.compacting.remove(key);
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    pub fn take(&self, slot: &SessionSlot) -> Option<&Take> {
        self.takes.get(slot)
    }

    pub fn notice(&self, slot: &SessionSlot) -> Option<&Notice> {
        self.notices.get(slot)
    }

    /// Park one prompt by the rule every hop keeps: **a draft leads the
    /// queue**, and everything else waits oldest first.
    ///
    /// The draft's precedence is the read's own - it is held in the core's
    /// registry rather than in the session's pending set - so a fold that
    /// appended one would disagree with the read about the front.
    ///
    /// A frame this fold sees twice must not park the same prompt twice: the
    /// boot's fold and the page's stream both apply every update.
    fn park(&mut self, key: &SessionSlot, ask: Ask) {
        let held = ask_key(&ask);
        let queue = self.asks.entry(key.clone()).or_default();
        if queue.iter().any(|waiting| ask_key(waiting) == held) {
            return;
        }
        // The kinds held in the CORE's registries rather than in the
        // session's own pending set lead: a draft and a browser hand-off are
        // both answered by their own id, and the read orders them the same
        // way, so a fold that appended one would disagree with the read
        // about the front.
        let at = if matches!(ask, Ask::SlackDraft(_) | Ask::BrowserHandOff(_)) {
            queue
                .iter()
                .position(|waiting| !matches!(waiting, Ask::SlackDraft(_) | Ask::BrowserHandOff(_)))
                .unwrap_or(queue.len())
        } else {
            queue.len()
        };
        queue.insert(at, ask);
    }

    /// The prompt a dock draws: the front of the seat's queue, which is the
    /// oldest - a seat parks a parallel batch one behind the other.
    pub fn ask(&self, slot: &SessionSlot) -> Option<&Ask> {
        self.asks.get(slot)?.first()
    }

    /// Every prompt the seat is holding, oldest first.
    pub fn asks(&self, slot: &SessionSlot) -> &[Ask] {
        self.asks.get(slot).map_or(&[], Vec::as_slice)
    }

    pub fn compacting(&self, slot: &SessionSlot) -> bool {
        self.compacting.contains(slot)
    }

    pub fn sign_in(&self, slot: &SessionSlot) -> Option<&SignIn> {
        self.sign_ins.get(slot)
    }
}

/// The key an ask is held under: its call, and the round for a question - one
/// tool call carries a whole batch and advances the index. A draft and a
/// browser hand-off are each answered by their own id and name no call.
fn ask_key(ask: &Ask) -> (String, Option<u64>) {
    match ask {
        Ask::Permission(request) => (request.tool_call.tool_call_id.clone(), None),
        Ask::Question(request) => {
            (request.tool_call.tool_call_id.clone(), Some(request.question_index))
        }
        Ask::SlackDraft(draft) => (draft.id.to_string(), None),
        Ask::BrowserHandOff(handoff) => (handoff.id.to_string(), None),
    }
}

/// Whether a resolution names this ask: its call, and its round when the
/// frame carries one. A frame naming no round is an older core's, and the id
/// is all it can mean there.
///
/// **The round as well as the call**: a batch reuses one tool id and advances
/// the index, and the next round's request can land before this round's
/// resolution, so a clear on the id alone drops the ask that just parked
/// (#1717).
fn resolved(ask: &Ask, tool_id: &str, round: Option<u64>) -> bool {
    match ask {
        Ask::Permission(request) => request.tool_call.tool_call_id == tool_id,
        Ask::Question(request) => {
            request.tool_call.tool_call_id == tool_id
                && round.is_none_or(|index| index == request.question_index)
        }
        // A draft and a browser hand-off are each answered by their own id
        // rather than by a tool call, so a resolved interaction never names
        // one.
        Ask::SlackDraft(_) | Ask::BrowserHandOff(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::permission_interaction::PermissionRequest;
    use forge_primitives::question::QuestionRequest;

    /// A question as the core emits one, with the call and the round a
    /// parallel batch and a batch's rounds are told apart by.
    fn question(tool_id: &str, index: u64) -> QuestionRequest {
        serde_json::from_value(serde_json::json!({
            "tool_call": {
                "tool_call_id": tool_id,
                "title": "AskUserQuestion",
                "kind": "other",
                "status": "pending",
                "content": [],
                "raw_input": {},
                "locations": [],
            },
            "prompt": {
                "question": "which?",
                "header": "Envs",
                "multi_select": false,
                "options": [],
            },
            "question_index": index,
            "total_questions": 2,
        }))
        .expect("a question request off the wire")
    }

    fn asked(slot: &SessionSlot, request: QuestionRequest) -> SessionUpdate {
        SessionUpdate::QuestionRequest {
            key: slot.clone(),
            tool_id: request.tool_call.tool_call_id.clone(),
            request,
        }
    }

    fn settled(slot: &SessionSlot, tool_id: &str, round: Option<u64>) -> SessionUpdate {
        SessionUpdate::PendingInteractionResolved {
            key: slot.clone(),
            tool_id: tool_id.to_owned(),
            question_index: round,
        }
    }

    /// The (call, round) the front ask is held under, which is what tells one
    /// of a parallel pair or a batch's rounds from the next.
    fn front(composer: &Composer, slot: &SessionSlot) -> (String, Option<u64>) {
        ask_key(composer.ask(slot).expect("a front ask"))
    }

    /// The copy it did hold goes with it, or the view keeps drawing an ask
    /// the core has settled.
    #[test]
    fn a_settled_prompt_drops_the_copy_this_view_held() {
        let mut composer = Composer::default();
        let slot = SessionSlot::lead("Busytools", "forge");
        let request: PermissionRequest = serde_json::from_value(serde_json::json!({
            "tool_call": {
                "tool_call_id": "held-1",
                "title": "Bash",
                "kind": "execute",
                "status": "pending",
                "content": [],
                "locations": [],
                "raw_input": {"command": "ls"},
            },
            "options": [],
        }))
        .expect("a permission request off the wire");
        composer.park(&slot, Ask::Permission(Box::new(request)));

        assert!(
            composer.apply(&SessionUpdate::PendingInteractionResolved {
                key: slot.clone(),
                tool_id: "held-1".to_owned(),
                question_index: None,
            }),
            "the box redraws for a prompt this view held",
        );
        assert!(composer.ask(&slot).is_none(), "and the copy it held is gone with it");

        assert!(
            composer.apply(&SessionUpdate::PendingInteractionResolved {
                key: slot,
                tool_id: "a-prompt-this-view-never-folded".to_owned(),
                question_index: None,
            }),
            "the box redraws for a settled prompt this view never folded too",
        );
    }

    /// **The loss #1717 was filed for.** Two AskUserQuestion calls in ONE
    /// assistant message run in parallel with different tool ids, and the
    /// dock's single slot dropped A the moment B parked: resolution(A) then
    /// id-mismatched and was ignored, and A's next round replaced B before B
    /// ever drew. A queue keeps both, and a resolution frees its own.
    #[test]
    fn a_parallel_batch_keeps_both_asks_and_frees_its_own() {
        let mut composer = Composer::default();
        let slot = SessionSlot::lead("Busytools", "forge");

        composer.apply(&asked(&slot, question("toolu_a", 0)));
        composer.apply(&asked(&slot, question("toolu_b", 0)));
        assert_eq!(composer.asks(&slot).len(), 2, "both parallel asks are held");
        assert_eq!(
            front(&composer, &slot),
            ("toolu_a".to_owned(), Some(0)),
            "and the oldest is the front a dock draws",
        );

        composer.apply(&settled(&slot, "toolu_a", Some(0)));
        assert_eq!(
            front(&composer, &slot),
            ("toolu_b".to_owned(), Some(0)),
            "the resolution frees its own ask and the front falls to the next",
        );

        composer.apply(&asked(&slot, question("toolu_a", 1)));
        assert_eq!(composer.asks(&slot).len(), 2, "and A's next round parks beside B, not over it");
        let b = ("toolu_b".to_owned(), Some(0));
        assert_eq!(front(&composer, &slot), b, "which B still leads");
    }

    /// A batch reuses one tool call id and advances the round, so the clear
    /// has to read the round as well as the call: a resolution for round 0
    /// must not take round 1's ask with it.
    #[test]
    fn a_round_of_one_call_dequeues_only_its_own_round() {
        let mut composer = Composer::default();
        let slot = SessionSlot::lead("Busytools", "forge");

        composer.apply(&asked(&slot, question("toolu_q", 1)));
        composer.apply(&settled(&slot, "toolu_q", Some(0)));
        assert_eq!(
            composer.asks(&slot).len(),
            1,
            "an earlier round's resolution leaves it standing"
        );

        composer.apply(&settled(&slot, "toolu_q", Some(1)));
        assert!(composer.asks(&slot).is_empty(), "and its own round settles it");
    }

    /// One tool call carries a whole batch and advances the round, so its two
    /// rounds share a call id: the round is what tells them apart, and a key
    /// that dropped it would fold round 1 into round 0.
    #[test]
    fn a_batchs_rounds_park_side_by_side_oldest_first() {
        let mut composer = Composer::default();
        let slot = SessionSlot::lead("Busytools", "forge");

        composer.apply(&asked(&slot, question("toolu_q", 0)));
        composer.apply(&asked(&slot, question("toolu_q", 1)));

        assert_eq!(composer.asks(&slot).len(), 2, "both rounds of the call are held");
        assert_eq!(
            front(&composer, &slot),
            ("toolu_q".to_owned(), Some(0)),
            "and the oldest is the front",
        );
    }

    /// The rule every hop keeps: a draft leads the queue, then arrival order.
    /// The draft's registry carries no arrival order to offer and the read
    /// has always answered a draft first, so a fold that appended one would
    /// disagree with the read about the front.
    #[test]
    fn a_draft_leads_the_queue_and_its_resolution_falls_back() {
        let mut composer = Composer::default();
        let slot = SessionSlot::lead("Busytools", "forge");
        let draft = forge_primitives::slack::SlackDraft {
            id: uuid::Uuid::parse_str("0192e1c0-0000-7000-8000-000000000000").expect("a uuid"),
            workspace: "acme".to_owned(),
            conversation: "C1".to_owned(),
            conversation_label: "acme".to_owned(),
            thread_ts: None,
            text: "hello".to_owned(),
            tool: "slack__post".to_owned(),
        };

        composer.apply(&asked(&slot, question("toolu_q", 0)));
        composer
            .apply(&SessionUpdate::SlackPostPending { key: slot.clone(), draft: draft.clone() });

        assert_eq!(
            ask_key(composer.ask(&slot).expect("a front ask")),
            (draft.id.to_string(), None),
            "the draft leads the question that was already waiting",
        );

        composer.apply(&SessionUpdate::SlackDraftResolved {
            key: slot.clone(),
            id: draft.id,
            ending: forge_primitives::slack::SlackDraftEnding::Expired,
        });

        assert_eq!(
            front(&composer, &slot),
            ("toolu_q".to_owned(), Some(0)),
            "and its resolution falls back to the question",
        );
    }

    /// The fold applies every update twice - once on the boot's fold, once on
    /// the page's stream - so a request must not park the same ask twice.
    #[test]
    fn a_request_folded_twice_parks_one_ask() {
        let mut composer = Composer::default();
        let slot = SessionSlot::lead("Busytools", "forge");
        let update = asked(&slot, question("toolu_q", 0));

        composer.apply(&update);
        composer.apply(&update);

        assert_eq!(composer.asks(&slot).len(), 1, "a repeat is not a second ask");
    }

    /// A take's readings are the composer's news only for the seat holding
    /// it. A level for a seat this fold has no take for - another session's
    /// page, or the tail of one that ended - must draw nothing, or fifty
    /// readings a second redraw a region that cannot have changed for the
    /// whole length of a take. Catches an arm that answers for the update
    /// rather than for whether it applies.
    #[test]
    fn a_level_for_a_seat_with_no_take_is_not_news() {
        let mut composer = Composer::default();

        assert!(
            !composer.apply(&SessionUpdate::DictateLevel {
                key: SessionSlot::lead("Busytools", "forge"),
                peak_db: -20.0,
                initiator: None,
            }),
            "a level with no take to draw it in is not this box's news",
        );
    }

    /// The answer is about the update, not about the state it landed on. Two
    /// things apply each update to this fold - the boot's own and the page's
    /// stream - so an arm that answers for what it changed is false for
    /// whoever applies second, and a take's end arrives with the take the
    /// first application already took. Catches that guard.
    #[test]
    fn a_takes_end_is_the_boxes_news_even_with_the_take_gone() {
        let mut composer = Composer::default();
        let slot = SessionSlot::lead("Busytools", "forge");

        assert!(
            composer.apply(&SessionUpdate::DictateEnded {
                key: slot,
                outcome: DictateOutcome::NoAudio { peak_db: -61.0, seconds: 3 },
                generation: 7,
                initiator: None,
            }),
            "an end that resolves a take this fold no longer holds still redraws the box",
        );
    }

    /// A take that could not start is a reason the box owes the reader: it
    /// resolves no take, so its generation matches nothing this view holds
    /// and the guard would answer "not mine" to a take that never existed,
    /// leaving the control a click that says nothing.
    #[test]
    fn a_refused_take_draws_its_reason() {
        let mut composer = Composer::default();
        let slot = SessionSlot::lead("Busytools", "forge");

        assert!(
            composer.apply(&SessionUpdate::DictateEnded {
                key: slot.clone(),
                outcome: DictateOutcome::Refused { message: "no microphone".to_owned() },
                generation: 0,
                initiator: None,
            }),
            "a take that never started still redraws the box",
        );
        let notice =
            composer.notice(&slot).expect("the refusal is held").line().expect("and draws a line");
        assert_eq!(notice, "no microphone", "with the core's own reason");
    }
}
