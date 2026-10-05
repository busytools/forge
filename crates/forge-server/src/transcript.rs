//! The conversation fold: wire messages in, the units a view draws out.
//!
//! The TUI partitions one message's blocks at a time, and a run of tool
//! calls spans messages - a call and its result are two of them - so this
//! walks the whole conversation and folds it in one pass. The rules it
//! folds by are not restated here: the row a call summarises under (the
//! wire-level row policy in `grouping`), the status a run summarises under
//! ([`crate::grouping::aggregate_call_status`]) and the peer parsers
//! ([`crate::envelope`], [`crate::peer_outbound`]) are the ones the TUI
//! already groups by.
//!
//! Where it differs from the TUI it is because the mockup draws something
//! else: a mutation folds as an `edit` family instead of breaking the run,
//! an envelope that is not agent traffic is a notice instead of a turn, a
//! question the assistant asked is a card rather than a call, and a Monitor
//! is not in the conversation at all.

use std::collections::HashMap;

use forge_primitives::messages::StopHookInfo;
use forge_primitives::runtime::RuntimeSessionState;
use forge_primitives::{ContentBlock, Message, StopReason, ToolCallContent};
// The row predicates the fold opens a turn by, which the conversation window
// reads too: they live in the window's crate so the two share one copy.
use forge_workspace::conversation_turns::{
    claims_skill_call, is_continuation, is_dispatched, is_image_note, is_task_notice,
};

use crate::envelope::{PeerInboundKind, detect_inbound};
use crate::family::{ToolFamily, tool_label};
use crate::grouping::{
    CallParts, KindRow, aggregate_call_status, family_target, is_edit_tool, wire_row,
};
use crate::model::tool_call_info::{
    AnsweredQuestion, is_ask_question_tool_name, is_monitor_tool_name,
};
use crate::model::{LiveTurn, LiveUsage, ToolCallStatus, TurnInfo};
use crate::peer_outbound::{PeerOutboundKind, detect_outbound_call};

/// One thing a view draws, in the order the conversation produced it.
// Serialize only, and the whole family below it: a `ChatUnit` is a shape the
// server hands a client and never reads one back. Deriving `Deserialize` too
// would be dead code here, and `PeerCard.kind` is a `&'static str` that cannot
// borrow one anyway.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", content = "unit", rename_all = "snake_case")]
pub enum ChatUnit {
    /// A turn the user wrote.
    UserTurn { text: String },
    /// Prose the assistant wrote.
    AssistantText { text: String },
    /// A maximal run of consecutive tool calls, drawn as one group.
    ToolGroup {
        /// The families the run met, in first-appearance order, each with
        /// the calls under it.
        families: Vec<FamilyLeaves>,
        /// What the run's header reports.
        status: ToolCallStatus,
    },
    /// A question the assistant asked and a person answered, as the
    /// mockup draws it: the question, what was picked, and what was typed.
    /// One unit per call, one pair per question it asked.
    QuestionCard { asked: Vec<AnsweredQuestion> },
    /// A peer message the conversation holds: one it sent, or one it
    /// received.
    PeerCard(PeerCard),
    /// A run of two or more consecutive peer messages, drawn as one group
    /// with a count. The threshold is the TUI's: a lone message is the card
    /// it is, and two are a group.
    MessagingGroup { cards: Vec<PeerCard> },
    /// A line the conversation carries that nobody typed: an external
    /// delivery, a scheduled fire, or a failure the workspace reported.
    Notice(Notice),
    /// What the turn's hooks did, drawn as the chip the terminal draws: the
    /// count, and one row per hook behind it. The wire sends none of these
    /// when no hook fired.
    Hooks {
        /// The frame's own id, which is what a view keys the row's open
        /// state on.
        key: String,
        actions: u32,
        infos: Vec<StopHookInfo>,
    },
    /// What a settled turn did, as the view's own row draws it: the turn's
    /// wall clock, its API time, and the tokens and cost the CLI reported.
    /// The web view's row is this, built from the result frame the fold
    /// reads or derived from the turn's own rows; the terminal builds the
    /// same record from the live stream, so the two draw one type rather
    /// than a copy each. `ended_at_local` is the field they differ on: the
    /// terminal stamps it off its own clock as the result arrives, while a
    /// turn read from a transcript carries the instant the CLI wrote instead
    /// and lets the view render it.
    TurnReport {
        info: TurnInfo,
        /// What names the row, which is what a view keys its open state on:
        /// the instant the turn's own first row carried, so a row inserted
        /// above it leaves it named the same. Absent for a turn the fold
        /// placed no clock on and whose frame carried no id, which is a row
        /// a view does not remember open rather than one it remembers under
        /// a neighbour's name. Two turns opening on one clock would share it,
        /// and the fold gives the second one a name of its own, so no two rows
        /// in a conversation carry one name.
        key: Option<String>,
    },
}

/// One family's calls inside a group.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FamilyLeaves {
    /// The class the row belongs to, which is what a view picks its glyph
    /// from. A label alone cannot tell a server named `read` from the read
    /// family.
    pub row: KindRow,
    /// The word the row draws: a family word, an MCP server's own name, or
    /// `edit` for a mutation.
    pub label: String,
    pub calls: Vec<ToolLeaf>,
}

/// How loudly a notice reads: the mockup's three rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSeverity {
    Info,
    Warning,
    Error,
}

/// A notice, as the envelope it arrived in.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Notice {
    pub severity: NoticeSeverity,
    /// Where it came from, for a renderer that gives each source its own
    /// chrome: `gotify`, `cron`, `slack`, `peer` or `worker`.
    pub source: &'static str,
    pub text: String,
}

/// One call inside a group: what its own row shows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolLeaf {
    /// The `tool_use` id the wire gave it.
    pub id: String,
    /// The class this call belongs to, which a row that draws one call
    /// rather than a family picks its glyph from. A label alone cannot
    /// tell a server named `read` from the read family.
    pub row: KindRow,
    /// The tool's own label, for a row that names the tool rather than
    /// its family.
    pub label: &'static str,
    /// The CLI's own name for the tool.
    ///
    /// Beside the label rather than instead of it: the label is the word a
    /// row draws and holds no way back to the tool, so a client handed only
    /// that can draw the card and cannot say which call it is.
    pub name: String,
    /// The tool's title: the file, command or query it names.
    pub title: String,
    /// The command a call ran, when it ran one. Separate from the title
    /// because a call that carries a description shows that as its title,
    /// and the command it actually ran would otherwise appear nowhere.
    pub command: Option<String>,
    pub status: ToolCallStatus,
    /// What the row opens on: the diff a mutation carries in its input, and
    /// whatever the call's result put beside it, in the shapes the shared
    /// builder resolves. Empty for a call that has not come back yet.
    pub content: Vec<ToolCallContent>,
}

/// A peer message, as the envelope it arrived in or the call that sent
/// it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PeerCard {
    /// The seat it went to, or came from.
    pub peer: String,
    pub body: String,
    /// True when it arrived rather than was sent.
    pub inbound: bool,
    /// What kind of traffic it is, which the group draws as its rows:
    /// `message` for a peer message, or a failure kind for a notice. The
    /// direction survives in [`Self::inbound`]; this is the kind, and the
    /// two are not the same question.
    pub kind: &'static str,
}

/// Where one turn sits in the conversation it was folded from, and what names
/// it.
///
/// **In MESSAGE terms rather than in units**, because a page crosses as whole
/// turns: a client is handed each turn's messages and folds them itself, so
/// the boundary a page is cut on has to name a message.
pub struct TurnSpan {
    /// The message the turn opens at.
    pub opens_at: usize,
    /// The key the fold named the turn by, `None` when nothing did - a
    /// conversation read from a transcript carries no `Result` frame, and the
    /// key comes from one.
    pub key: Option<String>,
}

/// A folded conversation: the units a view draws, and where its turns sit.
pub struct Rendered {
    pub units: Vec<ChatUnit>,
    pub turns: Vec<TurnSpan>,
    /// Every task ending the conversation holds, by the call each names.
    ///
    /// Read here because the fold is where it is read from at all - the
    /// preamble's pass over the messages - and carried out so the turns built
    /// from this answer do not walk the conversation again for it. A page is
    /// the case that pays: it is built while a reader scrolls.
    pub endings: HashMap<String, TaskEnding>,
}

/// Open a turn: the unit that opens it and the span a page is cut on are
/// pushed together, so where the fold says a turn begins and where the page
/// cuts one cannot come apart.
///
/// **One place opens a turn.** The fold has two ways in - a block that is the
/// user's own turn, and a queued prompt hoisted into one - and a span pushed
/// at only one of them leaves every turn of the other kind outside the page's
/// boundary list.
fn open_turn(turns: &mut Vec<TurnSpan>, units: &mut Vec<ChatUnit>, text: String, index: usize) {
    turns.push(TurnSpan { opens_at: index, key: None });
    units.push(ChatUnit::UserTurn { text });
}

/// Fold a conversation into the units a view draws.
pub fn render_units(messages: &[Message]) -> Vec<ChatUnit> {
    render(messages).units
}

/// Fold a conversation into the units a view draws, and report where its turns
/// sit in the message list.
///
/// One walk serves both: a second boundary rule over the messages is how the
/// two would come to disagree about where a turn begins, and the page is cut
/// on the answer.
pub fn render(messages: &[Message]) -> Rendered {
    let results = result_statuses(messages);
    let endings = task_endings(messages);
    let answers = question_answers(messages);
    let mut units: Vec<ChatUnit> = Vec::new();
    let mut run: Vec<((KindRow, String), ToolLeaf)> = Vec::new();
    let mut peers: Vec<PeerCard> = Vec::new();
    let mut prev_api: Option<u64> = None;
    let mut model: Option<String> = None;
    let mut thinking: Option<u64> = None;
    let mut traced = TurnTrace::default();
    // The names this fold has already given out, so a second turn opening on
    // one clock takes its own rather than the first turn's.
    let mut keys: HashMap<String, usize> = HashMap::new();
    // The last state the session reported. A conversation read from a
    // transcript reports none, so a page opened fresh draws its last turn's
    // row here; a live session's trailing turn is left to the view's own
    // live row.
    let mut running_turn = false;
    // Where each turn opens, taken as the fold pushes the unit that opens it:
    // one push per turn, in order, so the keys below line up with them.
    let mut turns: Vec<TurnSpan> = Vec::new();
    // The skills whose `Skill` call the open turn holds and whose body has not
    // arrived yet, oldest first: a body belongs in the call's turn, and the
    // claim is what keeps it there. Cleared wherever a turn opens, so a body
    // cannot join a turn whose calls are not the ones it belongs to.
    let mut turn_skills: Vec<String> = Vec::new();
    // Whether a compaction boundary has landed in the open turn. The
    // continuation prompt that follows belongs beside it - it is the row's own
    // account - so it opens no turn of its own while this holds.
    let mut turn_boundary = false;
    // Whether the open turn has taken any tool call. The harness's line about
    // an image it read arrives right behind the call that read it, so it joins
    // that turn rather than opening one; the note claims the call by content
    // in the views, and this only keeps the two in one turn.
    let mut turn_calls = false;
    for (index, message) in messages.iter().enumerate() {
        // A sub-agent's frames are the SUBAGENTS surface's, not the chat's.
        if is_dispatched(message) {
            continue;
        }
        // What the turn's hooks did. A frame reporting none of them draws
        // nothing, which is the terminal's rule too.
        if let Message::StopHookSummary { actions, hook_infos, uuid, .. } = message {
            if *actions > 0 {
                // The calls before it end here, as they do at any other
                // unit: a hook frame lands at the end of a turn's work, and
                // a chip above the calls it followed reads as out of order.
                flush(&mut run, &mut units);
                flush_peers(&mut peers, &mut units);
                units.push(ChatUnit::Hooks {
                    key: uuid.clone(),
                    actions: *actions,
                    infos: hook_infos.clone(),
                });
            }
            continue;
        }
        // The boundary a compaction left. It draws no unit of its own here -
        // the web client folds the frame itself - but which turn it lands in
        // is what the continuation prompt after it must join, so it is watched.
        if matches!(message, Message::CompactBoundary { .. }) {
            turn_boundary = true;
        }
        // What the turn has thought so far, summed from the frame deltas: the
        // wire's running counter restarts at every thinking block, so the
        // absolute field understates any turn that thought more than once.
        // The result carries no estimate of its own, so the frames before it
        // are the only place a settled row can read one.
        if let Message::ThinkingTokens { estimated_tokens_delta, .. } = message {
            let delta = u64::try_from(*estimated_tokens_delta).unwrap_or(0);
            thinking = Some(thinking.unwrap_or(0).saturating_add(delta));
            continue;
        }
        // A settled turn's row, which the view draws under the work it
        // accounts for. It arrives as a message of its own rather than as a
        // block, so it ends the run the calls before it built.
        if let Message::Result { uuid, .. } = message {
            flush(&mut run, &mut units);
            flush_peers(&mut peers, &mut units);
            if let Some(mut info) = turn_report(message, model.as_deref(), &mut prev_api) {
                info.thinking_tokens = thinking.take();
                let key = take_key(&mut keys, traced.opened.clone(), uuid.as_deref());
                units.push(ChatUnit::TurnReport { info, key });
            }
            traced = TurnTrace::default();
            running_turn = false;
            continue;
        }
        // A state frame is the turn boundary the wire alone reports, and the
        // only place a session says one is in flight. The turn before it
        // draws its row here, since nothing else closes it, and the state
        // says whether the turn after it is the one running.
        if let Some(running) = session_running_state(message) {
            flush(&mut run, &mut units);
            flush_peers(&mut peers, &mut units);
            close_traced(&mut traced, &mut units, true, &mut keys);
            running_turn = running;
            continue;
        }
        if let Message::Assistant { message: envelope, .. } = message {
            model = Some(envelope.model.clone());
        }
        let (assistant, content) = match message {
            Message::Assistant { message: envelope, .. } => (true, envelope.content.as_slice()),
            Message::User { message: envelope, .. } => (false, envelope.content.as_slice()),
            _ => continue,
        };
        for block in content {
            match block {
                // A skill's body, which the CLI injects as the reader's own
                // row right after the call that loaded it. It stays in that
                // call's turn - the second telling of what the call's row
                // already carries - and a body no call holds falls through to
                // the arms below and opens a turn as it always did.
                ContentBlock::Text { text, .. }
                    if !assistant && claims_skill_call(&mut turn_skills, text) => {}
                // The continuation prompt a compaction leaves behind, which
                // belongs in the boundary's own turn: the client hangs it on
                // that row, and a turn of its own draws the compaction twice.
                // With no boundary in this turn it opens one as it always did.
                ContentBlock::Text { text, .. }
                    if !assistant && turn_boundary && is_continuation(text) =>
                {
                    turn_boundary = false;
                }
                // The harness's own line about an image it just read: it
                // belongs in the open turn, right behind the call whose result
                // carried the picture, where the view hangs it on that call's
                // row as the caption. It draws no unit of its own either way.
                ContentBlock::Text { text, .. }
                    if !assistant && turn_calls && is_image_note(text) => {}
                // The harness's task ending in its other carrier: the same XML
                // written into a user row, which the scan hands on as this
                // plain text. A turn opened for it draws a task id and an
                // output path attributed to the reader, so the row opens none -
                // and the ending it carries was read in the pre-pass above,
                // which is the only place that can see across turns.
                ContentBlock::Text { text, .. } if !assistant && is_task_notice(text) => {}
                ContentBlock::Text { text, .. } => match text_unit(assistant, text) {
                    TextUnit::Peer(card) => {
                        flush(&mut run, &mut units);
                        // A peer message is a user row the CLI answered as a
                        // turn of its own, so the block above it belongs to
                        // the turn before: that turn's row lands here.
                        close_traced(&mut traced, &mut units, true, &mut keys);
                        peers.push(card);
                    }
                    TextUnit::Unit(unit) => {
                        flush(&mut run, &mut units);
                        flush_peers(&mut peers, &mut units);
                        // Any user row that draws as a unit opens a turn,
                        // whether a person wrote it or a delivery carried it,
                        // so the work above it draws its row first.
                        if !assistant {
                            close_traced(&mut traced, &mut units, true, &mut keys);
                        }
                        // A delivery draws as a notice, which is a row inside
                        // the turn already open; only the user's own turn
                        // opens one.
                        match unit {
                            ChatUnit::UserTurn { text } => {
                                open_turn(&mut turns, &mut units, text, index);
                                turn_skills.clear();
                                turn_boundary = false;
                                turn_calls = false;
                            }
                            other => units.push(other),
                        }
                    }
                },
                ContentBlock::QueuedCommand { prompt, command_mode, .. } => {
                    // The harness's own background-completion notice rides the
                    // same block as a prompt, and the scan hoists it into a
                    // user envelope without reading the mode - so a turn opened
                    // for it is a row nothing fills, which the page draws as a
                    // blank line. The terminal never draws one: it filters the
                    // row rather than the turn.
                    if is_completion_notice(command_mode.as_deref(), prompt) {
                        continue;
                    }
                    let text = queued_command_text(prompt);
                    // A queued prompt can be the words a person typed into a
                    // question's free-text field, in which case it belongs on
                    // the card. A card that already carries what was typed
                    // leaves it as the turn it is: the same words twice is
                    // worse than a turn.
                    if !absorb_typed(&mut units, &text) {
                        flush(&mut run, &mut units);
                        flush_peers(&mut peers, &mut units);
                        close_traced(&mut traced, &mut units, true, &mut keys);
                        open_turn(&mut turns, &mut units, text, index);
                        turn_skills.clear();
                        turn_boundary = false;
                        turn_calls = false;
                    }
                }
                ContentBlock::ToolUse { id, name, input, .. }
                | ContentBlock::ServerToolUse { id, name, input, .. } => {
                    turn_calls = true;
                    // A `Skill` call's own name for the skill it loads, held
                    // until that skill's body arrives: the body is what this
                    // claim decides the turn of.
                    if let Some(want) = input
                        .get("skill")
                        .and_then(serde_json::Value::as_str)
                        .filter(|_| name.eq_ignore_ascii_case("skill"))
                    {
                        turn_skills.push(want.to_owned());
                    }
                    push_call(
                        id, name, input, &results, &endings, &answers, &mut run, &mut peers,
                        &mut units,
                    );
                }
                // A result is not a unit of its own: it is what the call
                // it answers already carries.
                _ => {}
            }
        }
        // Watched after the turn it may have started has been written out:
        // a prompt's own clock belongs to the turn it opens, not to the one
        // it ended.
        traced.watch(message);
    }
    flush(&mut run, &mut units);
    flush_peers(&mut peers, &mut units);
    // The trailing turn is the one a running session is still writing, and
    // the view draws that turn's row itself. Every other turn is closed by
    // the turn after it, or by the result that reported it.
    if !running_turn {
        close_traced(&mut traced, &mut units, false, &mut keys);
    }
    // Each turn's name, off the report the fold closed it with: the first
    // report after an opening belongs to that turn, which is the same rule the
    // page used to slice turns out of the units.
    let mut open: Option<usize> = None;
    let mut next = 0_usize;
    for unit in &units {
        if matches!(unit, ChatUnit::UserTurn { .. }) {
            open = Some(next);
            next += 1;
            continue;
        }
        if let ChatUnit::TurnReport { key, .. } = unit
            && let Some(span) = open.and_then(|at| turns.get_mut(at))
        {
            span.key.clone_from(key);
        }
    }
    Rendered { units, turns, endings }
}

/// What a session-state frame says about a turn being in flight: `Some(true)`
/// when the session reports it is running one, `Some(false)` for any other
/// state it reports, and `None` for every frame that is not a state change.
///
/// Two readers ask this one question: the fold, which leaves a running turn's
/// row to the view, and the view, which draws that turn's live row.
pub fn session_running_state(message: &Message) -> Option<bool> {
    let Message::System { subtype, data, .. } = message else {
        return None;
    };
    if subtype != "session_state_changed" {
        return None;
    }
    Some(
        crate::translate::state_parsing::parse_runtime_session_state(data.get("state"))
            == Some(RuntimeSessionState::Running),
    )
}

/// The facts a turn's own rows carry, gathered while the fold walks them.
///
/// A transcript holds no result frame, so a turn read from one has no row of
/// its own unless one is derived here: the CLI writes that frame to the
/// wire and not to the file.
#[derive(Default)]
struct TurnTrace {
    /// The first and last clocks the turn's rows carried.
    opened: Option<String>,
    ended: Option<String>,
    /// The input-side usage the turn's frames reported. `LiveTurn` is the
    /// same accumulation a running turn shows, so the keying rule lives in
    /// one place and a derived row cannot drift from a live one.
    usage: LiveTurn,
    model: Option<String>,
    /// The stop the turn's last assistant frame reported. A frame handing
    /// back for a tool call has not ended a turn.
    stopped: Option<StopReason>,
    /// Whether the turn wrote an assistant frame at all: a prompt nothing
    /// answered draws no row, the way the terminal draws none.
    worked: bool,
}

impl TurnTrace {
    fn watch(&mut self, message: &Message) {
        match message {
            Message::Assistant { message: envelope, timestamp, .. } => {
                self.worked = true;
                if !envelope.model.is_empty() {
                    self.model = Some(envelope.model.clone());
                }
                if let Some(usage) = &envelope.usage {
                    self.usage.record(
                        envelope.id.clone(),
                        LiveUsage {
                            input_tokens: usage.input_tokens,
                            cache_read_tokens: usage.cache_read_input_tokens,
                            cache_written_tokens: usage.cache_creation_input_tokens,
                        },
                    );
                }
                self.stopped = envelope.stop_reason;
                self.clock(timestamp.as_deref());
            }
            Message::User { timestamp, .. } => self.clock(timestamp.as_deref()),
            _ => {}
        }
    }

    fn clock(&mut self, at: Option<&str>) {
        let Some(at) = at else {
            return;
        };
        self.opened.get_or_insert_with(|| at.to_owned());
        self.ended = Some(at.to_owned());
    }

    /// The span the turn's own rows support: `None` when either end does not
    /// parse or the pair runs backwards, which a transcript does hand back -
    /// a replayed row carries the stamp it was written with, so it can sit
    /// behind the row before it. The header prints a span and nothing else
    /// where one belongs, so a turn with no usable span draws no row: a zero
    /// there would be a measurement this row does not have.
    fn span_ms(&self) -> Option<u64> {
        let from = self.opened.as_deref()?;
        let to = self.ended.as_deref()?;
        forge_workspace::env::timezone::millis_between(from, to)
    }

    /// Whether the turn ran anything worth a row at all.
    fn drew_work(&self) -> bool {
        self.worked && self.span_ms().is_some()
    }

    /// Whether the turn's own last frame ended it rather than handing back
    /// for a tool call. Only a turn closing at the end of a conversation
    /// needs this: a turn the next prompt closed is over whatever its last
    /// frame stopped for.
    fn stopped_of_its_own_accord(&self) -> bool {
        self.stopped.is_some_and(|stop| stop != StopReason::ToolUse)
    }

    /// The row the turn's own rows support. Output tokens, api time and cost
    /// stay absent because a transcript records none of them: an assistant
    /// row's output count is the streaming placeholder rather than a count,
    /// and the cost is the CLI's own accounting.
    fn report(&self) -> TurnInfo {
        let totals = self.usage.totals();
        TurnInfo {
            duration_ms: self.span_ms(),
            // The instant, not a formatted clock: the fold is a function of
            // the messages, and which zone a reader is in is the view's to
            // know.
            ended_at_utc: self.ended.clone(),
            model: self.model.clone(),
            input_tokens: totals.map(|usage| usage.input_tokens),
            cache_read_tokens: totals.map(|usage| usage.cache_read_tokens),
            cache_written_tokens: totals.map(|usage| usage.cache_written_tokens),
            ..TurnInfo::default()
        }
    }
}

/// Write the row a turn read from a transcript draws, then start the next
/// turn's trace. A turn the wire already settled draws its own row and this
/// one nothing, so the two never count one turn twice.
///
/// `known_over` is whether whatever closed the turn says so - the next prompt
/// or a state frame. A turn closing at the end of a conversation has nothing
/// saying it, and needs its own last frame to have stopped rather than handed
/// back for a tool call.
fn close_traced(
    trace: &mut TurnTrace,
    units: &mut Vec<ChatUnit>,
    known_over: bool,
    keys: &mut HashMap<String, usize>,
) {
    if trace.drew_work() && (known_over || trace.stopped_of_its_own_accord()) {
        let key = take_key(keys, trace.opened.clone(), None);
        units.push(ChatUnit::TurnReport { info: trace.report(), key });
    }
    *trace = TurnTrace::default();
}

/// Whether a `parent_tool_use_id` names the dispatch a frame ran under. The
/// guard lives in `forge_primitives` because the folds that read the field
/// span three crates, and this path is kept for the callers that reach it
/// here.
pub use forge_primitives::messages::names_a_dispatch;

/// What names a settled row: the instant the turn's own first row carried,
/// which survives an insertion above it where a count of the rows before it
/// does not.
///
/// Two turns can open on one clock - the CLI writes a command and its answer
/// in the same millisecond - and the page keeps ONE map of the rows it has
/// been told to open, so two rows sharing a name would open and close
/// together. No transcript measured emits such a pair, and what stops one
/// today is `drew_work`, a guard about a turn drawing rather than about its
/// being named, so a name already taken in this fold takes an ordinal instead
/// of resting on that. Deterministic for a fixed transcript, unique within
/// one. The frame's own id stands in for a turn the fold placed no clock on,
/// and a turn with neither is named by nothing rather than by someone else's
/// name.
fn take_key(
    keys: &mut HashMap<String, usize>,
    opened: Option<String>,
    frame: Option<&str>,
) -> Option<String> {
    let base = opened.or_else(|| frame.map(str::to_owned))?;
    let nth = keys.entry(base.clone()).or_insert(0);
    *nth += 1;
    Some(if *nth == 1 { base } else { format!("{base}#{nth}") })
}

/// Add one call to the conversation: a peer card, a question's card, a
/// run-breaker drawn on its own, or a member of the run being built.
///
/// A Monitor is none of these: it is dropped. The inspector owns monitors
/// and the chat draws nothing for one, so a row here would put a watcher in
/// the conversation and name it under a family of its own.
fn push_call(
    id: &str,
    name: &str,
    input: &serde_json::Value,
    results: &HashMap<String, Recorded>,
    endings: &HashMap<String, TaskEnding>,
    answers: &HashMap<String, serde_json::Value>,
    run: &mut Vec<((KindRow, String), ToolLeaf)>,
    peers: &mut Vec<PeerCard>,
    units: &mut Vec<ChatUnit>,
) {
    if is_monitor_tool_name(name) {
        return;
    }
    if let Some(card) = outbound_card(name, input) {
        flush(run, units);
        peers.push(card);
    } else if is_ask_question_tool_name(name) {
        flush(run, units);
        flush_peers(peers, units);
        units.push(question_card(id, input, answers));
    } else {
        flush_peers(peers, units);
        run.push((family_row(name), with_ending(leaf(id, name, input, results), id, endings)));
    }
}

/// The call's row with the persisted ending that names it applied.
///
/// A backgrounded call's own result says the command is running and is not an
/// error, so the result alone draws a task that then failed as completed; the
/// notice says how it actually ended. What the harness said joins the row
/// rather than replacing it, because the result's own text is what names the
/// task's output file - the terminal's card replaces its body with the same
/// sentence, and the page's row keeps both, which is the drawing this follows.
fn with_ending(mut call: ToolLeaf, id: &str, endings: &HashMap<String, TaskEnding>) -> ToolLeaf {
    let Some(ending) = endings.get(id) else {
        return call;
    };
    if let Some(status) = ending.status {
        call.status = status;
    }
    if !ending.summary.is_empty() {
        call.content.push(ToolCallContent::Content {
            content: forge_primitives::ChunkContent::Text { text: ending.summary.clone() },
        });
    }
    call
}

/// The row a call folds under. A mutation folds under one `edit` family
/// whatever tool it was, which is what the mockup draws: the wire's own
/// answer gives each of the four mutation tools a row of its own, so
/// `Edit, Write, Edit` would draw two rows both labelled `edit` and the
/// second one out of order.
pub fn family_row(sdk_tool_name: &str) -> (KindRow, String) {
    if is_edit_tool(sdk_tool_name) {
        return (KindRow::Family(ToolFamily::Own("edit")), "edit".to_owned());
    }
    wire_row(sdk_tool_name)
}

/// The card a question draws: each question the call asked, with what the
/// person picked and what they typed.
///
/// The answer is recorded on the result row the CLI writes beside the tool
/// result (`tool_use_result.answers`, keyed by the question's own text), and
/// a value matching none of the question's option labels is what was typed
/// rather than picked. A question nobody answered is still a card: what was
/// asked is worth drawing without it.
fn question_card(
    id: &str,
    input: &serde_json::Value,
    answers: &HashMap<String, serde_json::Value>,
) -> ChatUnit {
    let recorded = answers.get(id);
    let asked = input
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .map(|questions| {
            questions.iter().map(|question| answered_question(question, recorded)).collect()
        })
        .unwrap_or_default();
    ChatUnit::QuestionCard { asked }
}

/// One question of a call, with its answer.
fn answered_question(
    question: &serde_json::Value,
    recorded: Option<&serde_json::Value>,
) -> AnsweredQuestion {
    let text =
        question.get("question").and_then(serde_json::Value::as_str).unwrap_or_default().to_owned();
    let labels: Vec<&str> = question
        .get("options")
        .and_then(serde_json::Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| option.get("label").and_then(serde_json::Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let mut picked_labels = Vec::new();
    let mut typed_note = None;
    for value in answer_values(recorded, &text) {
        if labels.contains(&value.as_str()) {
            picked_labels.push(value);
        } else if typed_note.is_none() {
            typed_note = Some(value);
        }
    }
    // A note beside a picked option arrives in its own map, keyed the same
    // way: the two shapes are one answer, and neither is the whole of it.
    if typed_note.is_none() {
        typed_note = recorded
            .and_then(|result| result.get("annotations"))
            .and_then(|annotations| annotations.get(&text))
            .and_then(|annotation| annotation.get("notes"))
            .and_then(serde_json::Value::as_str)
            .filter(|note| !note.is_empty())
            .map(str::to_owned);
    }
    AnsweredQuestion { question: text, picked_labels, typed_note }
}

/// What was answered for `question`: a string, or the array a multi-select
/// answer arrives as.
fn answer_values(recorded: Option<&serde_json::Value>, question: &str) -> Vec<String> {
    let Some(value) = recorded
        .and_then(|result| result.get("answers"))
        .and_then(serde_json::Value::as_object)
        .and_then(|answers| answers.get(question))
    else {
        return Vec::new();
    };
    let out: Vec<String> = match value {
        serde_json::Value::String(text) => vec![text.clone()],
        serde_json::Value::Array(items) => {
            items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect()
        }
        _ => Vec::new(),
    };
    // An empty answer is the CLI's "nothing picked" rather than something
    // said, and counting it as an answer is what loses the note beside it.
    out.into_iter().filter(|value| !value.is_empty()).collect()
}

/// Every question's recorded answer, by the call it belongs to.
fn question_answers(messages: &[Message]) -> HashMap<String, serde_json::Value> {
    let mut out = HashMap::new();
    for message in messages {
        let Message::User { message: envelope, tool_use_result, .. } = message else {
            continue;
        };
        let Some(recorded) = tool_use_result else {
            continue;
        };
        for block in &envelope.content {
            if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                out.insert(tool_use_id.clone(), recorded.clone());
            }
        }
    }
    out
}

/// Hand a queued prompt to the question card it answers, when the card has
/// a question with nothing typed beside it yet. `false` when there is no
/// such card, so the caller draws the prompt as the turn it is.
fn absorb_typed(units: &mut [ChatUnit], text: &str) -> bool {
    let Some(ChatUnit::QuestionCard { asked }) = units.last_mut() else {
        return false;
    };
    match asked.iter_mut().find(|pair| pair.typed_note.is_none()) {
        Some(pair) => {
            pair.typed_note = Some(text.to_owned());
            true
        }
        None => false,
    }
}

/// Whether a queued block is the harness's background-completion notice
/// rather than anything a person said.
///
/// The mode is the field that says which of the three kinds arrived; the
/// terminal reads the text's own prefix instead. Both are read here, because a
/// fold keyed on one of them drifts from the other the first time either moves.
fn is_completion_notice(command_mode: Option<&str>, prompt: &serde_json::Value) -> bool {
    command_mode == Some("task-notification") || is_task_notice(&queued_command_text(prompt))
}

/// What a persisted task ending says, which is what the live wire's
/// `task_notification` frame says: the CLI writes the frame's fields as XML
/// into the transcript instead.
///
/// Public because [`Rendered`] carries them: a caller building turns needs the
/// endings the fold already read, rather than a second walk to find them.
pub struct TaskEnding {
    /// The call it ends, from the notice's `<tool-use-id>`.
    pub call: String,
    /// How the task ended. `None` for a word this does not know.
    pub status: Option<ToolCallStatus>,
    /// The harness's own sentence about the outcome.
    pub summary: String,
    /// The notice as the CLI wrote it, which a carried copy repeats verbatim.
    pub text: String,
    /// Where the notice sits in the conversation it was read from, which is
    /// what says whether a turn already holds it.
    pub at: usize,
}

/// The notice as the block a view reads an ending from: the shape the scan
/// hoists an `attachment` row into, so both carriers reach a client as one
/// thing rather than as two the client has to know about.
pub(crate) fn notice_block(text: &str) -> ContentBlock {
    ContentBlock::QueuedCommand {
        prompt: serde_json::Value::String(text.to_owned()),
        command_mode: Some("task-notification".to_owned()),
        source_uuid: None,
        extras: serde_json::Map::new(),
    }
}

/// Every task notice the conversation holds as that one block, whichever
/// carrier the CLI persisted it in.
///
/// The row itself is kept - it is the notice, and dropping it would be the
/// silence rule 25 forbids - and only its shape changes, into the one a view
/// already reads an ending from. What a view draws for it is nothing: the
/// ending is drawn on the call's own row.
pub(crate) fn notices_as_blocks(mut messages: Vec<Message>) -> Vec<Message> {
    for message in &mut messages {
        let Message::User { message: envelope, .. } = message else {
            continue;
        };
        for block in &mut envelope.content {
            if let ContentBlock::Text { text, .. } = block
                && is_task_notice(text)
            {
                *block = notice_block(text);
            }
        }
    }
    messages
}

/// The ending a persisted notice carries, or `None` when the text is not one
/// or names no call to end.
fn task_ending(text: &str) -> Option<TaskEnding> {
    if !is_task_notice(text) {
        return None;
    }
    Some(TaskEnding {
        call: tag(text, "tool-use-id")?.to_owned(),
        status: tag(text, "status").and_then(task_status),
        summary: tag(text, "summary").unwrap_or_default().to_owned(),
        text: text.to_owned(),
        at: 0,
    })
}

/// The text between `<name>` and `</name>`, trimmed.
fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let (_, rest) = text.split_once(&format!("<{name}>"))?;
    let (inside, _) = rest.split_once(&format!("</{name}>"))?;
    Some(inside.trim())
}

/// A task's own status word, as the frame carrying it draws the call.
///
/// The same map the terminal applies to `task_updated.patch.status`: the wire
/// says `running` where the row says in progress, and `stopped` is its word
/// for a graceful cancel, which draws as the kill it is.
///
/// **An unrecognised word is `None`, where the terminal maps it to
/// `Pending`.** The terminal applies that map to a frame arriving on a live
/// call; here the word arrives after the call is finished, so an unreadable
/// word means the task ended without saying how, and drawing the call as
/// pending again would walk it back down.
fn task_status(word: &str) -> Option<ToolCallStatus> {
    match word {
        "running" => Some(ToolCallStatus::InProgress),
        "completed" => Some(ToolCallStatus::Completed),
        "failed" => Some(ToolCallStatus::Failed),
        "killed" | "stopped" => Some(ToolCallStatus::Killed),
        _ => None,
    }
}

/// The text a `queued_command` block carries: a plain string for a typed
/// prompt, or a content-block array for a multi-modal one, where only the
/// text blocks are the words the user typed and every other block renders
/// as a `[type]` placeholder so the reader sees something rather than a
/// blank.
pub fn queued_command_text(prompt: &serde_json::Value) -> String {
    if let Some(text) = prompt.as_str() {
        return text.to_owned();
    }
    let Some(blocks) = prompt.as_array() else {
        return serde_json::to_string(prompt).unwrap_or_else(|_| String::from("[unrenderable]"));
    };
    blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                block.get("text").and_then(serde_json::Value::as_str).map(str::to_owned)
            }
            Some(other) => Some(format!("[{other}]")),
            None => None,
        })
        .collect::<Vec<String>>()
        .join("\n")
}

/// What one text block turns into. A peer card is not a unit yet: the run
/// of them it belongs to has to be collected before the group it may become
/// is known.
enum TextUnit {
    Peer(PeerCard),
    Unit(ChatUnit),
}

/// One text block: the user's own turn, or the assistant's - unless it is
/// an envelope, which arrives as the user turn's prose.
fn text_unit(assistant: bool, text: &str) -> TextUnit {
    if !assistant && let Some(unit) = inbound_unit(text) {
        return match unit {
            ChatUnit::PeerCard(card) => TextUnit::Peer(card),
            other => TextUnit::Unit(other),
        };
    }
    let text = text.to_owned();
    let unit =
        if assistant { ChatUnit::AssistantText { text } } else { ChatUnit::UserTurn { text } };
    TextUnit::Unit(unit)
}

/// The unit an envelope carries. A peer comms envelope is a card; every
/// other kind the workspace injects is a notice - an external delivery, a
/// scheduled fire, or a failure - and rendering one as the user's turn
/// would put a protocol header inside a bubble nobody typed. The match is
/// exhaustive on purpose: a new envelope kind is a compile error here, not
/// a silent turn.
fn inbound_unit(text: &str) -> Option<ChatUnit> {
    match detect_inbound(text)? {
        PeerInboundKind::Message { from, body, .. } => {
            Some(ChatUnit::PeerCard(PeerCard { peer: from, body, inbound: true, kind: "message" }))
        }
        PeerInboundKind::Gotify { app, title, message, priority } => {
            let severity = if priority >= GOTIFY_ELEVATED_PRIORITY {
                NoticeSeverity::Warning
            } else {
                NoticeSeverity::Info
            };
            Some(notice(
                severity,
                "gotify",
                &format!("app '{app}' \u{b7} priority {priority}: {title}\n{message}"),
            ))
        }
        PeerInboundKind::Cron { prompt } => Some(notice(NoticeSeverity::Info, "cron", &prompt)),
        PeerInboundKind::Slack { workspace, channel, author, body } => {
            let head = match author {
                Some(author) => format!("{workspace} \u{b7} {channel} \u{b7} {author}"),
                None => format!("{workspace} \u{b7} {channel}"),
            };
            Some(notice(NoticeSeverity::Info, "slack", &format!("{head}: {body}")))
        }
        PeerInboundKind::DeliveryFailure { target, org, reason } => Some(notice(
            NoticeSeverity::Warning,
            "peer",
            &format!("'{target}' ({org}) failed to deliver: {reason}"),
        )),
        PeerInboundKind::WorkerSpawnFailed { label, reason } => Some(notice(
            NoticeSeverity::Warning,
            "worker",
            &format!("'{label}' failed to spawn: {reason}"),
        )),
    }
}

/// A Gotify delivery at or above this priority reads as a warning rather
/// than as information. The same number is the TUI's cue for the same row,
/// where it is private: a divergence here shows up as the two views giving
/// one delivery different severities.
const GOTIFY_ELEVATED_PRIORITY: u8 = 5;

/// A notice, with its text trimmed: a Gotify envelope's message is often
/// empty and would otherwise leave a bare newline under the title.
fn notice(severity: NoticeSeverity, source: &'static str, text: &str) -> ChatUnit {
    ChatUnit::Notice(Notice { severity, source, text: text.trim_end().to_owned() })
}

/// The card an outbound peer call draws, if it is one.
fn outbound_card(name: &str, input: &serde_json::Value) -> Option<PeerCard> {
    let PeerOutboundKind { target, body } = detect_outbound_call(name, input)?;
    Some(PeerCard { peer: target, body, inbound: false, kind: "message" })
}

/// One call as a transcript holds it: the wire's name and input, the title
/// and content the shared call builder resolves, and what its result
/// recorded. A call with no result yet reads as `Pending`, which is what the
/// resume path hands the TUI for the same file.
pub(crate) fn leaf(
    id: &str,
    name: &str,
    input: &serde_json::Value,
    results: &HashMap<String, Recorded>,
) -> ToolLeaf {
    let mut call = forge_workspace::tooling::create_tool_call(id, name, input, None);
    let recorded = results.get(id);
    let (status, content) = match recorded {
        Some(recorded) => {
            // The result's own shapes ride the shared builder rather than a
            // second reader here: it is what the TUI draws the same rows
            // from, and two readers would drift. The builder resolves the
            // input's own body too - an edit's diff arrives in the input -
            // so its answer is the whole of what the row opens on.
            let fields = forge_workspace::tooling::build_tool_result_fields(
                recorded.status == ToolCallStatus::Failed,
                recorded.content.as_ref(),
                Some(&call),
                recorded.result.as_ref(),
            );
            (fields.status.unwrap_or(recorded.status), fields.content.unwrap_or_default())
        }
        // A call that has not come back yet still carries what its own input
        // says: an edit's diff is in the call, not in the result.
        None => (ToolCallStatus::Pending, std::mem::take(&mut call.content)),
    };
    ToolLeaf {
        id: id.to_owned(),
        row: family_row(name).0,
        label: tool_label(name),
        name: name.to_owned(),
        // What the row names, resolved the way the terminal's own tree
        // resolves it: a search call's target is its pattern, a read's is
        // its path, and a call the builders have no target for keeps the
        // title the CLI gave it. The CLI's title is the tool's own name for
        // some calls, which a view that strips a label from it draws blank.
        title: family_target(CallParts { name, input: Some(input), title: &call.title })
            .unwrap_or_else(|| call.title.clone()),
        command: input
            .get("command")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_owned),
        status,
        content,
    }
}

/// What one call's result recorded: the status it settled at, the row's own
/// content, and the CLI's record beside it. All three are what the shared
/// result builder reads.
pub(crate) struct Recorded {
    pub(crate) status: ToolCallStatus,
    pub(crate) content: Option<serde_json::Value>,
    pub(crate) result: Option<serde_json::Value>,
}

/// One settled turn's report, from the frame that recorded it.
///
/// `prev_api` is the session-cumulative API clock at the previous result:
/// the wire counts it up across the session, so this turn's figure is the
/// delta. Without a previous result there is no delta to take, and the
/// cumulative field is the session's clock rather than this turn's, so the
/// figure is left absent: a fold cannot tell a session's first result from
/// one it joined mid-flight, and a read that keeps conversation rows alone
/// always joins mid-flight. A value below the previous one means the counter
/// restarted and is already per-turn, which leaves nothing to subtract
/// either. A resulting zero is "not attributed" rather than "took no time",
/// so it is absent for the same reason.
fn turn_report(
    message: &Message,
    model: Option<&str>,
    prev_api: &mut Option<u64>,
) -> Option<TurnInfo> {
    let Message::Result { duration_ms, duration_api_ms, total_cost_usd, usage, .. } = message
    else {
        return None;
    };
    let api_ms = match *prev_api {
        Some(prev) if *duration_api_ms >= prev => duration_api_ms.checked_sub(prev),
        _ => None,
    };
    *prev_api = Some(*duration_api_ms);
    Some(TurnInfo {
        duration_ms: Some(*duration_ms),
        api_ms: api_ms.filter(|ms| *ms > 0),
        model: model.filter(|name| !name.is_empty()).map(str::to_owned),
        input_tokens: usage.as_ref().map(|usage| usage.input_tokens),
        output_tokens: usage.as_ref().map(|usage| usage.output_tokens),
        cache_read_tokens: usage.as_ref().map(|usage| usage.cache_read_input_tokens),
        cache_written_tokens: usage.as_ref().map(|usage| usage.cache_creation_input_tokens),
        session_cost_usd: *total_cost_usd,
        ..TurnInfo::default()
    })
}

/// Every task ending the conversation holds, by the call each names.
///
/// Both carriers, because the CLI writes the same notice two ways: an
/// `attachment` row the scan hoists into a `queued_command` block, and a
/// `user` row whose content string is the XML.
///
/// **A pre-pass, and that is the point of it.** The ending is frequently not
/// in its call's turn: the notice row opens a turn of its own, and a delivery,
/// a peer message or a person's next prompt can sit between the two, so a fold
/// that read them in one pass could only see the endings that happened to
/// follow their call closely. This is the same shape [`result_statuses`] takes
/// for the same reason: the whole conversation is read once, keyed by call,
/// and the walk reads the answer where it needs it.
///
/// **The last ending naming a call wins**, which is the rule the results
/// pre-pass already keeps and the only one that reads a repeated ending as the
/// newer word rather than the older. It is load-bearing rather than
/// hypothetical: ten of this machine's calls carry two notices. Nine of those
/// pairs are the restart case, completed first and stopped second - written
/// once a restart found no completion record - and one runs the other way, so
/// the rule is what decides between them rather than the order the CLI
/// happens to write.
pub(crate) fn task_endings(messages: &[Message]) -> HashMap<String, TaskEnding> {
    let mut out: HashMap<String, TaskEnding> = HashMap::new();
    for (at, message) in messages.iter().enumerate() {
        let Message::User { message: envelope, .. } = message else {
            continue;
        };
        for block in &envelope.content {
            let text = match block {
                ContentBlock::Text { text, .. } => text.clone(),
                ContentBlock::QueuedCommand { prompt, .. } => queued_command_text(prompt),
                _ => continue,
            };
            if let Some(mut ending) = task_ending(&text) {
                ending.at = at;
                out.insert(ending.call.clone(), ending);
            }
        }
    }
    out
}

/// Every tool result the conversation holds, by the call it answers.
///
/// Two shapes, and both have to be read: an ordinary call's result is a
/// user turn, while a server-side tool's (`web_search`, `advisor`) arrives
/// inline in the assistant message that made the call. Reading only the
/// user turns leaves a server tool pending for good and holds its group's
/// aggregate there with it.
pub(crate) fn result_statuses(messages: &[Message]) -> HashMap<String, Recorded> {
    let mut out = HashMap::new();
    for message in messages {
        match message {
            Message::User { message: envelope, tool_use_result, .. } => {
                for block in &envelope.content {
                    if let ContentBlock::ToolResult { tool_use_id, content, is_error, .. } = block {
                        let status = if *is_error {
                            ToolCallStatus::Failed
                        } else {
                            ToolCallStatus::Completed
                        };
                        out.insert(
                            tool_use_id.clone(),
                            Recorded {
                                status,
                                content: Some(content.clone()),
                                result: tool_use_result.clone(),
                            },
                        );
                    }
                }
            }
            Message::Assistant { message: envelope, .. } => {
                for block in &envelope.content {
                    if let ContentBlock::ServerToolResult { tool_use_id, content, .. } = block {
                        out.insert(
                            tool_use_id.clone(),
                            Recorded {
                                status: ToolCallStatus::Completed,
                                content: Some(content.clone()),
                                result: None,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Close the run being built, if it has one, as one group: the rows it met
/// in first-appearance order, and the status its calls summarise under.
fn flush(run: &mut Vec<((KindRow, String), ToolLeaf)>, units: &mut Vec<ChatUnit>) {
    if run.is_empty() {
        return;
    }
    let calls = std::mem::take(run);
    let status = aggregate_call_status(calls.iter().map(|(_, leaf)| leaf.status));
    let mut families: Vec<FamilyLeaves> = Vec::new();
    for ((row, label), leaf) in calls {
        match families.iter_mut().find(|family| family.row == row && family.label == label) {
            Some(family) => family.calls.push(leaf),
            None => families.push(FamilyLeaves { row, label, calls: vec![leaf] }),
        }
    }
    units.push(ChatUnit::ToolGroup { families, status });
}

/// Close the peer run being collected, if it has one: two or more messages
/// are one group with a count, and one is the card it is.
fn flush_peers(peers: &mut Vec<PeerCard>, units: &mut Vec<ChatUnit>) {
    let mut cards = std::mem::take(peers);
    match cards.len() {
        0 => {}
        1 => units.push(ChatUnit::PeerCard(cards.remove(0))),
        _ => units.push(ChatUnit::MessagingGroup { cards }),
    }
}

#[cfg(test)]
mod tests {
    use forge_primitives::{
        AssistantEnvelope, ChunkContent, ContentBlock, Message, ToolCallContent, UserEnvelope,
    };

    use crate::family::ToolFamily;
    use crate::grouping::KindRow;
    use crate::model::ToolCallStatus;

    use super::{ChatUnit, NoticeSeverity, ToolLeaf, render, render_units, task_ending};

    /// An assistant message carrying `content`.
    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: "msg_01".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-opus-4-5".to_owned(),
                content,
                stop_reason: None,
                stop_sequence: None,
                usage: None,
                extras: serde_json::Map::new(),
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
            timestamp: None,
            extras: serde_json::Map::new(),
        }
    }

    /// One assistant message carrying a single tool call. `family` is the
    /// word these tests speak in, mapped to the SDK name it stands for,
    /// and `n` numbers the id so two calls in one run never share one.
    fn tool_call_at(family: &str, n: usize) -> Message {
        let sdk_name = match family {
            "read" => "Read",
            "search" => "Grep",
            "bash" => "Bash",
            "edit" => "Edit",
            other => other,
        };
        assistant(vec![ContentBlock::ToolUse {
            id: format!("toolu_{family}_{n}"),
            name: sdk_name.to_owned(),
            input: serde_json::json!({"file_path": "src/lib.rs", "command": "just check"}),
            extras: serde_json::Map::new(),
        }])
    }

    fn tool_call(family: &str) -> Message {
        tool_call_at(family, 0)
    }

    /// An assistant message carrying a call by its exact SDK name, for the
    /// names a family word does not cover.
    fn tool_call_named(name: &str) -> Message {
        assistant(vec![ContentBlock::ToolUse {
            id: format!("toolu_{name}"),
            name: name.to_owned(),
            input: serde_json::json!({"file_path": "src/lib.rs"}),
            extras: serde_json::Map::new(),
        }])
    }

    fn tool_call_messages(families: &[&str]) -> Vec<Message> {
        families.iter().enumerate().map(|(n, family)| tool_call_at(family, n)).collect()
    }

    /// A turn's hook summary, as the wire sends it.
    fn stop_hook(actions: u32) -> Message {
        Message::StopHookSummary {
            actions,
            hook_infos: Vec::new(),
            hook_errors: Vec::new(),
            has_output: true,
            level: "suggestion".to_owned(),
            prevented_continuation: false,
            stop_reason: String::new(),
            tool_use_id: "toolu_hook".to_owned(),
            parent_tool_use_id: None,
            session_id: "session".to_owned(),
            uuid: "hooks-1".to_owned(),
            extras: serde_json::Map::new(),
        }
    }

    /// The hook chip is the page's own unit, and it obeys the rules the
    /// terminal's chip does: it draws after the calls it followed, a frame
    /// reporting no hooks draws nothing, and a dispatched agent's hooks are
    /// not the session's.
    #[test]
    fn a_hook_frame_draws_after_the_run_it_followed() {
        let units = render_units(&[tool_call_at("read", 1), stop_hook(2)]);
        assert_eq!(units.len(), 2, "the chip is a unit of its own");
        assert!(matches!(units[0], ChatUnit::ToolGroup { .. }), "after the calls it followed");
        assert!(matches!(units[1], ChatUnit::Hooks { .. }), "with the chip last");

        assert!(
            render_units(&[stop_hook(0)]).is_empty(),
            "a frame reporting no hooks draws nothing",
        );
        assert!(
            render_units(&[dispatched(stop_hook(2))]).is_empty(),
            "and a dispatched agent's hook is not the session's",
        );
    }

    /// The same frame as it arrives from a sub-agent: the dispatch's own
    /// tool-use id in `parent_tool_use_id`, which is how the wire names the
    /// agent a frame belongs to.
    fn dispatched(mut message: Message) -> Message {
        match &mut message {
            Message::Assistant { parent_tool_use_id, .. }
            | Message::User { parent_tool_use_id, .. }
            | Message::StopHookSummary { parent_tool_use_id, .. } => {
                *parent_tool_use_id = Some("toolu_dispatch".to_owned());
            }
            other => panic!("no sub-agent produces a frame of this shape: {other:?}"),
        }
        message
    }

    fn assistant_text(text: &str) -> Message {
        assistant(vec![ContentBlock::Text {
            text: text.to_owned(),
            extras: serde_json::Map::new(),
        }])
    }

    /// Consecutive tool calls with nothing between them are ONE group.
    /// This is the rule the mockup was built on and the one a reader will
    /// get wrong first.
    #[test]
    fn consecutive_tool_calls_fold_into_one_group() {
        let messages = tool_call_messages(&["read", "search", "read"]);
        let units = render_units(&messages);
        assert_eq!(units.len(), 1, "three consecutive calls are one unit");
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        let named: Vec<&str> = families.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(named, ["read", "search"], "read and search, in first-appearance order");
        assert_eq!(families[0].calls.len(), 2, "both reads sit under the row they belong to");
        assert_eq!(families[1].calls.len(), 1, "and the search under its own");
    }

    /// An edit folds into the run as a family of its own. The TUI breaks a
    /// run on a mutation because it draws the diff open on its own; the
    /// mockup draws an `edit` family inside the group with its leaves open,
    /// so this fold cannot borrow the TUI's rule, and the row's word is
    /// `edit` rather than the tool's own `Edit`.
    #[test]
    fn an_edit_folds_into_the_run_as_its_own_family() {
        let messages = tool_call_messages(&["read", "edit", "read"]);
        let units = render_units(&messages);
        assert_eq!(units.len(), 1, "the edit does not break the run");
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        let named: Vec<&str> = families.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(named, ["read", "edit"], "with the mockup's own word for a mutation");
        assert_eq!(families[1].calls.len(), 1, "and the edit under that row");
        assert_eq!(
            families[0].row,
            KindRow::Family(ToolFamily::Read),
            "each row carries the class a view picks its glyph from",
        );
        assert_eq!(
            families[1].row,
            KindRow::Family(ToolFamily::Own("edit")),
            "so the edit row reads as a class of its own, not as the generic tool row",
        );
    }

    /// Every mutation folds under the one `edit` row, whatever tool it was.
    /// The wire's own answer gives each of the four tools a row of its own,
    /// which draws two rows both labelled `edit`, with the second one out of
    /// order behind the first.
    #[test]
    fn every_mutation_folds_under_one_row() {
        let messages = tool_call_messages(&["edit", "Write", "edit", "MultiEdit"]);
        let units = render_units(&messages);
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };

        assert_eq!(families.len(), 1, "one row, not one per tool name");
        assert_eq!(families[0].label, "edit");
        assert_eq!(families[0].calls.len(), 4, "with every mutation under it, in order");
        let ids: Vec<&str> = families[0].calls.iter().map(|call| call.id.as_str()).collect();
        assert_eq!(
            ids,
            ["toolu_edit_0", "toolu_Write_1", "toolu_edit_2", "toolu_MultiEdit_3"],
            "and in the order they ran",
        );
    }

    /// A Monitor is not in the conversation at all: the inspector owns it,
    /// and the mockup draws nothing for it in the chat. Folded as a row, the
    /// page shows a watcher beside the calls it is watching, under a family
    /// that has no name of its own.
    ///
    /// It is dropped without breaking the run it sat inside: the reader sees
    /// the calls around it as the one run they are, since nothing is drawn
    /// between them.
    #[test]
    fn a_monitor_is_not_in_the_conversation() {
        let monitor = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_monitor".to_owned(),
            name: "Monitor".to_owned(),
            input: serde_json::json!({"description": "watch the deploy", "command": "tail -f log"}),
            extras: serde_json::Map::new(),
        }]);
        let messages = [tool_call_at("read", 0), monitor, tool_call_at("read", 1)];

        let units = render_units(&messages);

        assert_eq!(units.len(), 1, "the run it sat inside is still one unit");
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        assert_eq!(families.len(), 1, "with no row for the monitor");
        assert_eq!(families[0].calls.len(), 2, "and both reads under their own");
    }

    /// A run of peer messages is ONE group with a count, which is how the
    /// TUI folds it and what the mockup draws; a lone message stays the
    /// card it is. Folded as separate cards, a burst of peer traffic reads
    /// as a wall of frames with nothing saying they arrived together.
    #[test]
    fn a_run_of_peer_messages_is_one_group() {
        let steward = peer_message("t-1", "steward", "IT IMPORTED. The window is lost");
        let planner = peer_message("t-2", "planner", "picking up the migration now");

        let units = render_units(&[steward, planner]);

        assert_eq!(units.len(), 1, "two consecutive peer messages are one unit");
        let ChatUnit::MessagingGroup { cards } = &units[0] else {
            panic!("a messaging group");
        };
        assert_eq!(cards.len(), 2, "carrying both of them");
        assert_eq!(cards[0].peer, "steward", "in the order the conversation produced them");
        assert_eq!(cards[1].peer, "planner", "and each keeps its own words");
        assert_eq!(cards[1].body, "picking up the migration now");

        let alone = render_units(&[peer_message("t-3", "tester", "take the render half")]);
        assert!(
            matches!(alone[0], ChatUnit::PeerCard(_)),
            "a lone message is the card it is, with no group around it",
        );
    }

    /// The two classes the mockup draws outside a run - a question waiting
    /// on a person and a peer block - split it, and the mutation does not:
    /// an edit is a family inside the group with its leaves open.
    #[test]
    fn only_the_calls_the_mockup_draws_alone_break_the_run() {
        let question = tool_call_named("AskUserQuestion");
        let peer = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_peer".to_owned(),
            name: "mcp__forge__agents__send_message".to_owned(),
            input: serde_json::json!({"project": "companies", "message": "did it land?"}),
            extras: serde_json::Map::new(),
        }]);
        let edit = tool_call("edit");
        let read = tool_call_at("read", 9);

        // A breaker on either side of a run: three units, the middle one
        // drawn alone.
        for breaker in [question, peer] {
            let units = render_units(&[read.clone(), breaker, read.clone()]);
            assert_eq!(units.len(), 3, "the run splits around a call drawn on its own");
            assert!(
                matches!(&units[1], ChatUnit::PeerCard(_) | ChatUnit::QuestionCard { .. }),
                "and that call is the unit in the middle",
            );
        }

        // The mutation does not break it: one group, with the edit inside.
        let units = render_units(&[read.clone(), edit, read]);
        assert_eq!(units.len(), 1, "a mutation folds into the run instead");
    }

    /// Two MCP servers are two rows, not one `tool` row. The mockup draws
    /// each server as its own family, and a server's name is only known at
    /// runtime, which is why the row carries a label rather than a family.
    #[test]
    fn each_mcp_server_is_its_own_family_row() {
        let messages = [
            tool_call_named("mcp__playwright__browser_click"),
            tool_call_named("mcp__forge__agents__list"),
        ];
        let units = render_units(&messages);
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        let named: Vec<&str> = families.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(named, ["playwright", "forge"], "one row per server, in first-appearance order");
        let classes: Vec<KindRow> = families.iter().map(|row| row.row).collect();
        assert_eq!(
            classes,
            [KindRow::Mcp, KindRow::Mcp],
            "and each says it is a server rather than a family, so a view tells them from a \
             family row that happens to share the word",
        );
    }

    /// An envelope that is not agent traffic is not the user's own turn: a
    /// Gotify delivery, a cron fire, a Slack bundle, a failed delivery and a
    /// failed worker spawn each arrive as a notice, which is what the mockup's
    /// notice rows are for. Folded as a turn, the page shows a protocol
    /// header inside a bubble the user never typed.
    #[test]
    fn an_external_envelope_is_a_notice_and_not_a_turn() {
        let quiet = user(vec![ContentBlock::Text {
            text: "[Gotify - app 'watcher', priority 3]\n\ndeploy finished".to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let loud = user(vec![ContentBlock::Text {
            text: "[Gotify - app 'ci', priority 9]\n\nbuild failed".to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let slack = user(vec![ContentBlock::Text {
            text: "[Slack - workspace 'Busytools', #forge] id ts\n\nsteward: the gate is green\n"
                .to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let failed = user(vec![ContentBlock::Text {
            text: "[Ask id=q-1 to agent 'companies' (org 'Busytools') failed to deliver: channel closed]"
                .to_owned(),
            extras: serde_json::Map::new(),
        }]);

        let units = render_units(&[quiet, loud, slack, failed]);

        let ChatUnit::Notice(quiet) = &units[0] else {
            panic!("a notice");
        };
        assert_eq!(quiet.severity, NoticeSeverity::Info, "a quiet delivery is information");
        assert_eq!(quiet.source, "gotify", "and says where it came from");
        assert!(quiet.text.contains("deploy finished"), "carrying its own words: {}", quiet.text);
        assert!(
            quiet.text.contains("priority 3"),
            "and the priority it arrived at: {}",
            quiet.text
        );

        let ChatUnit::Notice(loud) = &units[1] else {
            panic!("a notice");
        };
        assert_eq!(loud.severity, NoticeSeverity::Warning, "an elevated priority is a warning");

        let ChatUnit::Notice(slack) = &units[2] else {
            panic!("a notice");
        };
        assert_eq!(slack.source, "slack", "a Slack bundle says so");
        assert!(
            slack.text.contains("Busytools") && slack.text.contains("steward"),
            "and keeps the workspace and author its header draws: {}",
            slack.text,
        );

        let ChatUnit::Notice(failed) = &units[3] else {
            panic!("a notice");
        };
        assert_eq!(failed.severity, NoticeSeverity::Warning, "a failed delivery is");
        assert_eq!(failed.source, "peer", "and names the seat it could not reach");
        assert!(failed.text.contains("channel closed"), "keeping the reason: {}", failed.text);
    }

    /// A non-tool block ends the run: the next call starts a NEW group.
    #[test]
    fn a_non_tool_block_breaks_the_run() {
        let messages = [tool_call("read"), assistant_text("here it is"), tool_call("bash")];
        let units = render_units(&messages);
        assert_eq!(units.len(), 3, "the prose splits the run in two");
    }

    /// A peer message that arrived as the user turn's prose, the way the
    /// scan hands an inbound envelope to a reader.
    fn peer_message(id: &str, from: &str, body: &str) -> Message {
        user(vec![ContentBlock::Text {
            text: format!("[Message id={id} from agent '{from}' (org 'Busytools')]\n\n{body}"),
            extras: serde_json::Map::new(),
        }])
    }

    /// A question the assistant asked and a person answered. The fold draws
    /// the mockup's card rather than a bare tool call: the question, what the
    /// person picked, and what they typed.
    ///
    /// The answer is recorded on the result row the CLI wrote
    /// (`tool_use_result.answers`, keyed by the question's own text), and a
    /// value that matches none of the question's options is what was typed
    /// rather than picked.
    #[test]
    fn an_answered_question_is_a_card() {
        let asked = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_q".to_owned(),
            name: "AskUserQuestion".to_owned(),
            input: serde_json::json!({"questions": [{
                "question": "Which colour do you prefer?",
                "header": "Colour",
                "multiSelect": false,
                "options": [{"label": "Red"}, {"label": "Blue"}, {"label": "Green"}],
            }]}),
            extras: serde_json::Map::new(),
        }]);
        let answered = user_answered(
            "toolu_q",
            serde_json::json!({
                "questions": [{"question": "Which colour do you prefer?"}],
                "answers": {"Which colour do you prefer?": "Blue"},
            }),
        );

        let units = render_units(&[asked, answered]);

        let ChatUnit::QuestionCard { asked } = &units[0] else {
            panic!("a question card");
        };
        assert_eq!(asked.len(), 1, "one pair for the one question the call asked");
        assert_eq!(asked[0].question, "Which colour do you prefer?", "carrying the question");
        assert_eq!(asked[0].picked_labels, ["Blue"], "and what was picked");
        assert_eq!(asked[0].typed_note, None, "with nothing typed this time");
    }

    /// What the person typed lands on the card too, whether the CLI recorded
    /// it as a free-text answer or as the note beside a picked option. A
    /// question whose answer matches no option is the typed one.
    #[test]
    fn a_typed_answer_lands_on_the_card() {
        let asked = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_q".to_owned(),
            name: "AskUserQuestion".to_owned(),
            input: serde_json::json!({"questions": [{
                "question": "Which colour do you prefer?",
                "header": "Colour",
                "multiSelect": true,
                "options": [{"label": "Red"}, {"label": "Blue"}],
            }]}),
            extras: serde_json::Map::new(),
        }]);
        let answered = user_answered(
            "toolu_q",
            serde_json::json!({
                "answers": {"Which colour do you prefer?": ["Red", "and keep the green case too"]},
            }),
        );

        let units = render_units(&[asked, answered]);

        let ChatUnit::QuestionCard { asked } = &units[0] else {
            panic!("a question card");
        };
        assert_eq!(asked[0].picked_labels, ["Red"], "the option that matches a label is the pick");
        assert_eq!(
            asked[0].typed_note.as_deref(),
            Some("and keep the green case too"),
            "and the value that matches none of them is what was typed",
        );
    }

    /// A question nobody answered is still a card: the fold shows what was
    /// asked, with nothing picked, rather than the tool call's own row.
    #[test]
    fn an_unanswered_question_is_a_card_without_an_answer() {
        let asked = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_q".to_owned(),
            name: "AskUserQuestion".to_owned(),
            input: serde_json::json!({"questions": [{
                "question": "Which colour do you prefer?",
                "options": [{"label": "Red"}],
            }]}),
            extras: serde_json::Map::new(),
        }]);
        let unanswered = user_answered("toolu_q", serde_json::json!({"answers": {}}));

        let units = render_units(&[asked, unanswered]);

        let ChatUnit::QuestionCard { asked } = &units[0] else {
            panic!("a question card");
        };
        assert_eq!(asked[0].question, "Which colour do you prefer?", "the question is drawn");
        assert!(asked[0].picked_labels.is_empty(), "with nothing picked");
        assert_eq!(asked[0].typed_note, None, "and nothing typed");
    }

    /// A user turn carrying `content`.
    fn user(content: Vec<ContentBlock>) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content,
                extras: serde_json::Map::new(),
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
            timestamp: None,
            synthetic: false,
            extras: serde_json::Map::new(),
        }
    }

    /// The user's mid-turn queued prompt, as the scan hoists it out of the
    /// transcript's attachment row.
    fn queued_prompt(text: &str) -> Message {
        user(vec![ContentBlock::QueuedCommand {
            prompt: serde_json::Value::String(text.to_owned()),
            command_mode: Some("prompt".to_owned()),
            source_uuid: None,
            extras: serde_json::Map::new(),
        }])
    }

    /// The result row for a question, as the CLI writes it: the tool result,
    /// and beside it the record of what was answered.
    fn user_answered(tool_use_id: &str, recorded: serde_json::Value) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.to_owned(),
                    content: serde_json::Value::String(
                        "Your questions have been answered".to_owned(),
                    ),
                    is_error: false,
                    extras: serde_json::Map::new(),
                }],
                extras: serde_json::Map::new(),
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: Some(recorded),
            timestamp: None,
            synthetic: false,
            extras: serde_json::Map::new(),
        }
    }

    /// The result the tool at `tool_use_id` came back with.
    fn tool_result(tool_use_id: &str, is_error: bool) -> Message {
        user(vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content: serde_json::Value::String("output".to_owned()),
            is_error,
            extras: serde_json::Map::new(),
        }])
    }

    /// The handle a row needs to identify a call. `Task` is the case that
    /// shows why the leaf carries both: its row draws the word `Subagent`,
    /// and that word leads back to no tool, so a view handed only the label
    /// can draw the card and cannot say which call it is.
    ///
    /// Read through a transcript rather than through hand-built frames, so the
    /// fold is walked the way a page walks it.
    #[tokio::test]
    async fn a_groups_leaves_carry_the_tools_name_beside_its_label() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &[
                    r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"go"},"session_id":"s"}"#,
                    r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"tu1","name":"Task","input":{"description":"investigate","prompt":"look"}}]}}"#,
                    r#"{"type":"result","uuid":"r1","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
                ],
            )
            .expect("the transcript seeds");
        let seat = forge_primitives::SessionSlot::lead("TestOrg", "proj");
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");

        let units = render_units(&surface.conversation(&seat, &cwd).messages);
        let leaf = units
            .iter()
            .find_map(|unit| match unit {
                ChatUnit::ToolGroup { families, .. } => {
                    families.iter().flat_map(|family| family.calls.iter()).next()
                }
                _ => None,
            })
            .expect("the fold produced a group with a call in it");

        assert_eq!(leaf.name, "Task", "the leaf names the tool the CLI ran");
        assert_eq!(
            leaf.label, "Subagent",
            "and the word its row draws is a different thing, which is why both are carried",
        );
    }

    /// A user's mid-turn prompt reached the page. On the read path it has
    /// no other shape: the scan hoists the queued row into a user envelope
    /// so a reader can rebuild the bubble, and a fold that ignores the
    /// block drops the question the assistant went on to answer.
    #[test]
    fn a_queued_prompt_is_a_user_turn() {
        let messages = [queued_prompt("and keep the multiSelect case too")];
        let units = render_units(&messages);
        assert_eq!(units.len(), 1, "the queued prompt is one unit");
        let ChatUnit::UserTurn { text } = &units[0] else {
            panic!("a user turn");
        };
        assert_eq!(text, "and keep the multiSelect case too", "carrying what the user typed");
    }

    /// The harness's background-completion notice is not a turn.
    ///
    /// It reaches the fold as the same `queued_command` block a prompt does -
    /// the scan hoists the attachment row without reading its mode - and a
    /// turn opened for it is a row nothing fills. The page draws that as a
    /// blank line between two turns, which is the defect #1324 names; the
    /// assertion is about the CUT, because a fold that dropped the text alone
    /// would still leave the empty turn standing.
    ///
    /// **Two signals, and each is pinned alone.** Across 10,148 queued blocks
    /// in this machine's transcripts, 3,113 carry the mode, 3,113 open with
    /// the tag, and none disagrees - so the pair looks redundant, and a row
    /// carrying both could not tell a two-signal guard from a one-signal one.
    /// A skill's body arrives as the reader's own user row, right after the
    /// call that loaded it - and a page read can cut the two apart. The body
    /// belongs in the call's turn, where the call's row is what tells the
    /// story: a turn of its own is the same thing said twice under the
    /// reader's name, and the web client pairs the two by name whatever their
    /// turns say.
    #[test]
    fn a_skill_body_stays_in_the_turn_whose_call_loaded_it() {
        let call = |want: &str| {
            assistant(vec![ContentBlock::ToolUse {
                id: format!("toolu_{want}"),
                name: "Skill".to_owned(),
                input: serde_json::json!({ "skill": want }),
                extras: serde_json::Map::new(),
            }])
        };
        let body = |path: &str| {
            user(vec![ContentBlock::Text {
                text: format!("Base directory for this skill: {path}\n\n# The skill\n\nDo it."),
                extras: serde_json::Map::new(),
            }])
        };
        let prompt = || {
            user(vec![ContentBlock::Text { text: "go".to_owned(), extras: serde_json::Map::new() }])
        };

        let rendered =
            render(&[prompt(), call("unslop"), body("/Users/ved/.claude/skills/unslop")]);
        assert_eq!(rendered.turns.len(), 1, "the body opens no turn of its own");
        assert_eq!(
            rendered.units.iter().filter(|unit| matches!(unit, ChatUnit::UserTurn { .. })).count(),
            1,
            "and draws no second row under the reader's name"
        );

        // The plugin spelling: the call says `a:b` where the path ends `b`.
        let cached = render(&[
            prompt(),
            call("ui-ux-pro-max:ui-ux-pro-max"),
            body("/Users/ved/.claude/plugins/cache/x/ui-ux-pro-max/2.13.0"),
        ]);
        assert_eq!(cached.turns.len(), 1, "a versioned plugin path still matches its call");

        // The control: a body no call claimed opens a turn, so nothing is lost.
        let orphan = render(&[prompt(), body("/Users/ved/.claude/skills/other")]);
        assert_eq!(orphan.turns.len(), 2, "an unclaimed body opens a turn as it always did");

        // The carrier a tool-invoked skill uses: the skill's own markdown,
        // named only by its title heading.
        let titled = render(&[
            prompt(),
            call("pr-review-loop"),
            user(vec![ContentBlock::Text {
                text: "# PR Review Loop\n\nReview a change with parallel reviewers.".to_owned(),
                extras: serde_json::Map::new(),
            }]),
        ]);
        assert_eq!(titled.turns.len(), 1, "a titled body stays in its call's turn");

        // The harness's line about an image it read: it belongs behind the
        // call that read it, where the view hangs it on that call's row.
        let note = user(vec![ContentBlock::Text {
            text: "[Image: original 2782x1034, displayed at 2000x743. Multiply coordinates by 1.39 to map to original image.]".to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let pictured = render(&[prompt(), tool_call("read"), note.clone()]);
        assert_eq!(pictured.turns.len(), 1, "the image line opens no turn of its own");

        // The control: with no call in the turn it opens one, so nothing is lost.
        let loose = render(&[prompt(), note]);
        assert_eq!(loose.turns.len(), 2, "with no call to join it opens a turn");
    }

    /// The continuation prompt a compaction leaves behind arrives as the
    /// reader's own row, right after the boundary frame. It belongs in the
    /// boundary's turn - the web client hangs it on that row - and a prompt
    /// with no boundary opens a turn as it always did, so nothing is lost.
    #[test]
    fn a_continuation_prompt_stays_in_the_boundarys_turn() {
        let boundary = || Message::CompactBoundary {
            trigger: "auto".to_owned(),
            pre_tokens: 68_031,
            post_tokens: 9_149,
            uuid: "cb-1".to_owned(),
            session_id: "session".to_owned(),
            metadata_extras: serde_json::Map::new(),
            extras: serde_json::Map::new(),
        };
        let summary = user(vec![ContentBlock::Text {
            text: "This session is being continued from a previous conversation that ran out of context. And so on.".to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let prompt = || {
            user(vec![ContentBlock::Text { text: "go".to_owned(), extras: serde_json::Map::new() }])
        };

        let rendered = render(&[prompt(), boundary(), summary.clone()]);
        assert_eq!(rendered.turns.len(), 1, "the prompt opens no turn of its own");
        assert_eq!(
            rendered.units.iter().filter(|unit| matches!(unit, ChatUnit::UserTurn { .. })).count(),
            1,
            "and draws no row under the reader's name"
        );

        // The control: no boundary in the turn, so it opens one as it did.
        let orphan = render(&[prompt(), summary]);
        assert_eq!(orphan.turns.len(), 2, "a prompt with no boundary still opens a turn");
    }

    #[test]
    fn a_notification_row_opens_no_turn() {
        let by_mode = user(vec![ContentBlock::QueuedCommand {
            prompt: serde_json::Value::String("Task bj5g0t2kq done".to_owned()),
            command_mode: Some("task-notification".to_owned()),
            source_uuid: None,
            extras: serde_json::Map::new(),
        }]);
        let by_tag = user(vec![ContentBlock::QueuedCommand {
            prompt: serde_json::Value::String(
                "<task-notification>Task bj5g0t2kq completed</task-notification>".to_owned(),
            ),
            command_mode: Some("prompt".to_owned()),
            source_uuid: None,
            extras: serde_json::Map::new(),
        }]);

        for (notice, signal) in [(by_mode, "the mode"), (by_tag, "the tag")] {
            let rendered = render(&[notice]);
            assert!(rendered.turns.is_empty(), "a completion notice opens no turn: {signal}");
            assert!(rendered.units.is_empty(), "and draws no unit of its own: {signal}");
        }
    }

    /// **The window's turn scan names no frame the fold opens no turn at.**
    ///
    /// The window cuts its oldest edge on a turn's first frame, and it reads
    /// the fold's own row predicates to find one - so the two walks have to
    /// agree on the direction that matters: a frame the scan named and the fold
    /// drew as something else would start the window inside a turn, which is
    /// the whole thing the window's front is for.
    ///
    /// The other direction is pinned as the one place they are meant to differ:
    /// a queued prompt opens a turn in the fold unless a question card takes it,
    /// and the scan cannot see the card, so it leaves the prompt alone.
    #[test]
    fn the_windows_turn_scan_names_no_frame_the_fold_opens_no_turn_at() {
        let text = |text: &str| {
            user(vec![ContentBlock::Text { text: text.to_owned(), extras: serde_json::Map::new() }])
        };
        let call = |name: &str, input: serde_json::Value, id: &str| {
            assistant(vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
                extras: serde_json::Map::new(),
            }])
        };
        let boundary = Message::CompactBoundary {
            trigger: "auto".to_owned(),
            pre_tokens: 68_031,
            post_tokens: 9_149,
            uuid: "cb-1".to_owned(),
            session_id: "session".to_owned(),
            metadata_extras: serde_json::Map::new(),
            extras: serde_json::Map::new(),
        };
        let continued = "This session is being continued from a previous conversation";
        let messages = vec![
            text("hello"),
            call("Skill", serde_json::json!({ "skill": "pr-review-loop" }), "toolu_1"),
            text("Base directory for this skill: /x/pr-review-loop\n\n# T\n\nDo it."),
            text(continued),
            text("<task-notification><tool-use-id>tu1</tool-use-id></task-notification>"),
            text("[Cron]\n\ndo the thing"),
            call("Bash", serde_json::json!({ "command": "ls" }), "toolu_2"),
            text("[Image: original 2782x1034, displayed at 2000x743.]"),
            queued_prompt("queued mid-turn"),
            boundary.clone(),
            text(continued),
            text("second prompt"),
            // The state a turn opens under does not straddle the open: a claim
            // an earlier turn made, a boundary it took and a call it made are
            // all gone by the next turn's own first frame. Each shape below is
            // one a stale flag would swallow - a body the claim no longer
            // holds, an image note behind no call, a continuation behind no
            // boundary - so dropping any one of the three resets loses a head.
            call("Skill", serde_json::json!({ "skill": "pr-review-loop" }), "toolu_3"),
            text("third prompt"),
            text("Base directory for this skill: /x/pr-review-loop\n\n# T\n\nDo it."),
            call("Bash", serde_json::json!({ "command": "ls" }), "toolu_4"),
            text("fourth prompt"),
            text("[Image: original 2782x1034, displayed at 2000x743.]"),
            boundary,
            text("fifth prompt"),
            text(continued),
            // The carriers and non-heads the two walks must agree about: a
            // titled skill body behind its own call, a body no call holds, a
            // result row, and a sub-agent's frame.
            call("Skill", serde_json::json!({ "skill": "pr-review-loop" }), "toolu_5"),
            text("# PR Review Loop\n\nReview a change with parallel reviewers."),
            text("Base directory for this skill: /x/other\n\n# T\n\nDo it."),
            tool_result("tu1", false),
            dispatched(text("a sub-agent's row")),
            text("sixth prompt"),
        ];

        let opened: Vec<usize> = render(&messages).turns.iter().map(|span| span.opens_at).collect();
        let mut scan = forge_workspace::conversation_turns::TurnScan::default();
        let named: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| scan.opens(message))
            .map(|(at, _)| at)
            .collect();

        assert_eq!(
            named,
            vec![0, 3, 11, 13, 14, 16, 17, 19, 20, 23, 26],
            "precondition: the scan names the fixture's own prompts - each state-reset frame \
             included - and the suppressions hold",
        );
        assert!(
            named.iter().all(|at| opened.contains(at)),
            "every frame the window would cut at is one the fold opens a turn at: \
             scan {named:?}, fold {opened:?}",
        );
        assert!(
            opened.contains(&8) && !named.contains(&8),
            "and the queued prompt is where they are meant to differ, the scan leaving it alone: \
             scan {named:?}, fold {opened:?}",
        );
    }

    /// A persisted notice IS an ending, and it says the same three things the
    /// live wire's `task_notification` frame says: which call it ends, how it
    /// ended, and the harness's own sentence.
    ///
    /// Read off a real row rather than a typed one, because the parser is the
    /// thing under test and a notice written to match a parser proves only
    /// that it matches itself.
    #[test]
    fn a_persisted_notice_is_read_as_the_ending_it_carries() {
        let row: serde_json::Value =
            serde_json::from_str(crate::fixtures::SAME_TURN_USER_ROW[2]).expect("a real row");
        let text = row["message"]["content"].as_str().expect("the notice's own text");

        let ending = task_ending(text).expect("a task notice");
        assert_eq!(ending.call, "call_da4c7ee117d14d48ba99e036", "the call it ends");
        assert_eq!(ending.status, Some(ToolCallStatus::Failed), "how it ended");
        assert_eq!(
            ending.summary,
            "Background command \"Watch the account-lifecycle CI run\" failed with exit code 1",
            "and what the harness said about it",
        );
    }

    /// Fold a transcript seeded from `rows`, through the read a view makes:
    /// the scan, the replay synthesiser and the fold. The rows are the ones a
    /// test names, and they are real ones - a fold driven from hand-built
    /// frames would agree with itself about a shape the CLI does not write.
    fn folded_transcript(rows: &[&str]) -> Vec<ChatUnit> {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.seed_transcript("TestOrg", "proj", "lead", rows).expect("the transcript seeds");
        fleet.install_agent("TestOrg", "proj", "lead");
        let seat = forge_primitives::SessionSlot::lead("TestOrg", "proj");
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");
        render_units(&surface.conversation(&seat, &cwd).messages)
    }

    /// The call the fold drew under `id`.
    fn call_in<'a>(units: &'a [ChatUnit], id: &str) -> &'a ToolLeaf {
        units
            .iter()
            .find_map(|unit| match unit {
                ChatUnit::ToolGroup { families, .. } => families
                    .iter()
                    .flat_map(|family| family.calls.iter())
                    .find(|call| call.id == id),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the fold drew a call under {id}"))
    }

    /// The text a call's row carries, in order.
    fn texts_of(call: &ToolLeaf) -> Vec<String> {
        call.content
            .iter()
            .filter_map(|piece| match piece {
                ToolCallContent::Content { content: ChunkContent::Text { text } } => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// A transcript's own messages, as the read hands them to the fold BEFORE
    /// a view's normalisation: `session_history` is the read the spawn and the
    /// terminal perform, and the surface's rewrite of the carriers is one
    /// caller's step rather than part of the read itself.
    fn read_raw(rows: &[&str]) -> Vec<Message> {
        let dir = tempfile::tempdir().expect("tempdir");
        let projects = dir.path().join("projects").join("any-project-key");
        std::fs::create_dir_all(&projects).expect("the project dir");
        // A uuid, because the read refuses a name that is not one; it walks
        // every project dir for the file, so no key has to be derived.
        let session = "b095cf6c-1be5-4337-9965-0dc6e46f6b57";
        std::fs::write(projects.join(format!("{session}.jsonl")), rows.join("\n"))
            .expect("the transcript writes");
        forge_workspace::session_history(dir.path(), session, "").messages
    }

    /// The notice a task ending arrives as, carrying `status` and `summary`
    /// for `call`.
    ///
    /// Written rather than read off disk, because the rules these pin are
    /// about inputs the corpus does not hold: an ending whose status word
    /// nothing knows, and one that disagrees with the result beside it.
    fn notice(call: &str, status: &str, summary: &str) -> Message {
        user(vec![ContentBlock::Text {
            text: format!(
                "<task-notification>\n<tool-use-id>{call}</tool-use-id>\n\
                 <status>{status}</status>\n<summary>{summary}</summary>\n</task-notification>"
            ),
            extras: serde_json::Map::new(),
        }])
    }

    /// The second carrier opens no turn either.
    ///
    /// It is the same notice the attachment holds, and a fold that reads one
    /// and not the other leaves the raw XML drawn as the reader's own words:
    /// 1,200 rows of it across this machine's 2.1.280 transcripts, with the
    /// task id and the output path in the text.
    #[test]
    fn the_user_rows_notice_opens_no_turn() {
        let units = folded_transcript(crate::fixtures::SAME_TURN_USER_ROW);

        assert!(
            !units.iter().any(|unit| matches!(unit, ChatUnit::UserTurn { .. })),
            "a notice the CLI persisted as a user row draws no turn of its own",
        );
    }

    /// An ending ends the call it names, and what the harness said joins the
    /// row rather than replacing it.
    ///
    /// Three slices, one property each time: the text carrier and the
    /// attachment carrier, one where a turn of its own sits between the notice
    /// and its call, and one where the call is a dispatched agent - 699 of
    /// this machine's 2,194 paired endings name an `Agent` rather than a
    /// backgrounded command.
    ///
    /// The launch's own result says the task is running and is not an error,
    /// so a fold that settles the call on it draws a failed or killed task as
    /// completed. That result's text must survive alongside the notice's
    /// sentence: it is what names the task's output file.
    #[test]
    fn an_ending_ends_the_call_it_names() {
        for (rows, call, status, said, result_says) in [
            (
                crate::fixtures::SAME_TURN_USER_ROW,
                "call_da4c7ee117d14d48ba99e036",
                ToolCallStatus::Failed,
                "Background command \"Watch the account-lifecycle CI run\" failed with exit code 1",
                "Command is running in background with ID: t00000001.",
            ),
            (
                crate::fixtures::CROSS_TURN_ATTACHMENT,
                "call_17c05f64497746b0ac450728",
                ToolCallStatus::Failed,
                "Background command \"Restart the harness with a long terminate window\" failed with exit code 100",
                "Command is running in background with ID: t00000003.",
            ),
            (
                crate::fixtures::KILLED_AGENT,
                "toolu_013WM27Lt98mvnUq573pXxYD",
                ToolCallStatus::Killed,
                "Agent \"Run slow counting loop\" was stopped by user",
                "Async agent launched successfully.",
            ),
        ] {
            let units = folded_transcript(rows);
            let call = call_in(&units, call);
            let texts = texts_of(call);

            assert_eq!(call.status, status, "the notice's own word ends the call: {texts:?}");
            assert!(
                texts.iter().any(|text| text == said),
                "and what the harness said rides its row: {texts:?}",
            );
            assert!(
                texts.iter().any(|text| text.starts_with(result_says)),
                "beside the result's own text rather than in place of it: {texts:?}",
            );
        }
    }

    /// Two endings naming one call: the later word is the one that stands.
    ///
    /// The pair is a real one, twelve minutes apart: the first notice says the
    /// command completed, and the second - written after a restart found no
    /// completion record - says it stopped. Ten of this machine's calls carry
    /// two notices, so the rule decides drawings rather than a hypothetical,
    /// and a first-wins fold draws this call completed when it stopped. The
    /// rule is the one the results pre-pass already keeps, where a second
    /// result for a call replaces the first.
    #[test]
    fn the_later_ending_naming_one_call_wins() {
        let units = folded_transcript(crate::fixtures::DUPLICATE_ENDINGS);
        let call = call_in(&units, "call_370cb30e2834491daf5bb4e2");
        let texts = texts_of(call);

        assert_eq!(call.status, ToolCallStatus::Killed, "the second notice's word: {texts:?}");
        assert!(
            texts.iter().any(|text| text
                == "Background shell command didn't finish before the previous session ended"),
            "and the sentence it carried: {texts:?}",
        );
        assert!(
            !texts.iter().any(|text| text.contains("completed (exit code 0)")),
            "not the first notice's word: {texts:?}",
        );
    }

    /// The fold reads the raw carrier too - a message list taken straight off
    /// a transcript, before any view's normalisation.
    ///
    /// **A guard for the fold's own contract, not dead weight.** `render` and
    /// `render_units` are public and take the CLI's own messages; the
    /// surface's rewrite of the carriers into one block is one caller's step,
    /// not part of the read. A fold that read only the normalised shape would
    /// leave the second, quieter contract - a caller handing it a transcript -
    /// drawing the raw XML as the reader's own turn.
    #[test]
    fn a_raw_transcripts_notice_is_read() {
        let units = render_units(&read_raw(crate::fixtures::SAME_TURN_USER_ROW));

        assert!(
            !units.iter().any(|unit| matches!(unit, ChatUnit::UserTurn { .. })),
            "the row the CLI wrote draws no turn of its own",
        );
        assert_eq!(
            call_in(&units, "call_da4c7ee117d14d48ba99e036").status,
            ToolCallStatus::Failed,
            "and the ending it carries still ends the call",
        );
    }

    /// How an ending's own word meets the status the call already had.
    ///
    /// **Two deliberate divergences from the terminal, and both are read by
    /// whoever next aligns forge with it.**
    ///
    /// An unrecognised word keeps the status the call had, where the terminal
    /// maps every unknown to `Pending`. A task whose ending forge cannot read
    /// is one it does not know has ended, and pending is a claim that it did
    /// not stop.
    ///
    /// And a recognised word stands even over a result that failed. The notice
    /// is the CLI's last word about the task, and the page's own fold reads it
    /// the same way - the two folds are one vocabulary. The terminal keeps a
    /// `Failed` or `Killed` call when a notification arrives, because it
    /// applies one to a live card a real failure has already settled, and no
    /// read of a transcript holds that state.
    #[test]
    fn an_endings_word_meets_the_status_the_call_already_had() {
        let launch = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_ending".to_owned(),
            name: "Bash".to_owned(),
            input: serde_json::json!({"command": "just check"}),
            extras: serde_json::Map::new(),
        }]);
        let failed = tool_result("toolu_ending", true);

        for (status, expected) in
            [("reticulating", ToolCallStatus::Failed), ("completed", ToolCallStatus::Completed)]
        {
            let units = render_units(&[
                launch.clone(),
                failed.clone(),
                notice("toolu_ending", status, "what the harness said"),
            ]);
            assert_eq!(
                call_in(&units, "toolu_ending").status,
                expected,
                "the ending's word is {status}",
            );
        }
    }

    /// A sub-agent's frames are not the chat's. The terminal suppresses
    /// them - `handle_assistant` drops a parented frame's text and thinking,
    /// scoping its calls to the SUBAGENTS inspector - and the drawing says
    /// the same in one line. The control rides in the same test: the same
    /// three frames without a parent id ARE the conversation, so a fold
    /// that dropped them all could not pass this.
    #[test]
    fn a_sub_agents_frames_are_not_the_chat() {
        let prompt = user(vec![ContentBlock::Text {
            text: "run the tests".to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let prose = assistant_text("the second one failed");
        let call = tool_call("search");

        let child: Vec<Message> =
            [prompt.clone(), prose.clone(), call.clone()].into_iter().map(dispatched).collect();
        assert!(
            render_units(&child).is_empty(),
            "a sub-agent's prompt, prose and calls draw nothing in the parent's chat",
        );

        let parent = [prompt, prose, call];
        assert_eq!(
            render_units(&parent).len(),
            3,
            "and the same three frames are the conversation when the agent is the session's own",
        );

        // An empty id names no dispatch, and the terminal reads the field
        // the same way: a frame carrying one is the session's own.
        let mut blank = assistant_text("the session's own line");
        if let Message::Assistant { parent_tool_use_id, .. } = &mut blank {
            *parent_tool_use_id = Some(String::new());
        }
        assert_eq!(render_units(&[blank]).len(), 1, "an empty id is not a dispatch");

        // A parent that names nothing the session dispatched: the guard
        // reads the field rather than looking the id up, and the wire has
        // frames parented to a uuid with no dispatch behind it at all.
        let mut stray = assistant_text("parented to something else");
        if let Message::Assistant { parent_tool_use_id, .. } = &mut stray {
            *parent_tool_use_id = Some("8f3c1d20-0a4b-4c6e-9d21-77e5a0b3c914".to_owned());
        }
        assert!(
            render_units(&[stray]).is_empty(),
            "a parent that names no dispatch still suppresses the frame",
        );
    }

    /// A sub-agent works while its session's own turn does, so its calls
    /// arrive inside a run of the main agent's. They must not join the
    /// group: the header would count them and the families would list a
    /// server the session never called.
    #[test]
    fn a_sub_agents_call_does_not_join_the_sessions_group() {
        let interleaved =
            [tool_call_at("read", 1), dispatched(tool_call("search")), tool_call_at("read", 2)];
        let units = render_units(&interleaved);
        assert_eq!(units.len(), 1, "the run is one group either way");
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        assert_eq!(families.len(), 1, "with the session's own family alone");
        assert_eq!(families[0].calls.len(), 2, "and its own two calls under it");
    }

    /// Every call carries its own status, and the group summarises the
    /// run, so a failed call must not paint as a clean one.
    #[test]
    fn a_result_sets_the_status_of_the_call_it_answers() {
        let messages = [
            tool_call_at("read", 0),
            tool_result("toolu_read_0", true),
            tool_call_at("read", 1),
            tool_result("toolu_read_1", false),
        ];
        let units = render_units(&messages);
        let ChatUnit::ToolGroup { families, status } = &units[0] else {
            panic!("a tool group");
        };
        let statuses: Vec<ToolCallStatus> =
            families[0].calls.iter().map(|call| call.status).collect();
        assert_eq!(
            statuses,
            [ToolCallStatus::Failed, ToolCallStatus::Completed],
            "each call reads the status its own result recorded",
        );
        assert_eq!(*status, ToolCallStatus::Failed, "and the run reports the failure it holds");
    }

    /// A call carries what its row opens on: the result's own content,
    /// resolved by the same builder the TUI draws the same rows from. A leaf
    /// with no body is a row that expands to nothing.
    #[test]
    fn a_leaf_carries_what_its_result_put_beside_it() {
        let messages = [tool_call("bash"), tool_result("toolu_bash_0", false)];

        let units = render_units(&messages);

        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        let leaf = &families[0].calls[0];
        let text: String = leaf
            .content
            .iter()
            .filter_map(|content| match content {
                ToolCallContent::Content { content: ChunkContent::Text { text } } => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert!(text.contains("output"), "the row opens on the result's own words: {text}");
    }

    /// A mutation, as the wire carries one: its diff is in the call's own
    /// input.
    fn edit_call() -> Message {
        assistant(vec![ContentBlock::ToolUse {
            id: "toolu_edit".to_owned(),
            name: "Edit".to_owned(),
            input: serde_json::json!({
                "file_path": "crates/forge-web/src/home.css",
                "old_string": "  text-decoration: none; flex: none;",
                "new_string": "  text-decoration: none; flex: 0 1 auto;",
            }),
            extras: serde_json::Map::new(),
        }])
    }

    fn diffs_in(leaf: &super::ToolLeaf) -> usize {
        leaf.content
            .iter()
            .filter(|content| matches!(content, ToolCallContent::Diff { .. }))
            .count()
    }

    /// A mutation carries its diff from the input alone, which is what the
    /// mockup draws open by default: the edit family's leaves are the one
    /// row that shows its body without being asked.
    #[test]
    fn a_mutation_carries_its_diff() {
        let units = render_units(&[edit_call()]);

        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        assert_eq!(
            diffs_in(&families[0].calls[0]),
            1,
            "the edit's row carries the diff its input describes",
        );
    }

    /// A mutation that has come back carries ONE diff, not two: the result
    /// builder resolves the input's body as well, so a leaf that also kept
    /// the call's own content would draw the same diff twice.
    #[test]
    fn a_mutation_that_came_back_carries_one_diff() {
        let units = render_units(&[edit_call(), tool_result("toolu_edit", false)]);

        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        let call = &families[0].calls[0];
        assert_eq!(call.status, ToolCallStatus::Completed, "the result settles it");
        assert_eq!(diffs_in(call), 1, "and its body is one diff, not two");
    }

    /// A server-side tool's result arrives inline in the assistant message
    /// that made the call, not as a user turn. Reading only the user turns
    /// leaves a `web_search` row pending for good, and holds its group's
    /// aggregate at `Pending` with it.
    #[test]
    fn a_server_tool_result_settles_its_call() {
        let call = assistant(vec![ContentBlock::ServerToolUse {
            id: "srvtoolu_1".to_owned(),
            name: "web_search".to_owned(),
            input: serde_json::json!({"query": "ratatui list widget"}),
            extras: serde_json::Map::new(),
        }]);
        let result = assistant(vec![ContentBlock::ServerToolResult {
            tool_use_id: "srvtoolu_1".to_owned(),
            content: serde_json::json!({"type": "web_search_result"}),
            extras: serde_json::Map::new(),
        }]);

        let units = render_units(&[call, result]);

        let ChatUnit::ToolGroup { families, status } = &units[0] else {
            panic!("a tool group");
        };
        assert_eq!(
            families[0].calls[0].status,
            ToolCallStatus::Completed,
            "the server tool's own result settles it",
        );
        assert_eq!(*status, ToolCallStatus::Completed, "and the run it is in with it");
    }

    /// Peer traffic is a card on both sides: an envelope that arrived as
    /// the user turn's prose, and a call the session made. Folded as a
    /// turn or a tool call, the page would show the protocol instead of
    /// the message.
    ///
    /// Each is rendered alone: two in a row are the group that
    /// [`a_run_of_peer_messages_is_one_group`] pins, which is a different
    /// question from what one message is.
    #[test]
    fn peer_traffic_is_a_card_and_not_a_turn_or_a_call() {
        let arrived = user(vec![ContentBlock::Text {
            text: "[Message id=m-1 from agent 'companies' (org 'Busytools')]\n\nis the cron issue filed?"
                .to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let sent = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_ask".to_owned(),
            name: "mcp__forge__agents__send_message".to_owned(),
            input: serde_json::json!({"project": "forge", "message": "did it land?"}),
            extras: serde_json::Map::new(),
        }]);

        let inbound = render_units(&[arrived]);
        let ChatUnit::PeerCard(inbound) = &inbound[0] else {
            panic!("a peer card");
        };
        assert!(inbound.inbound, "the envelope that arrived reads as inbound");
        assert_eq!(inbound.peer, "companies", "and names who sent it");
        assert_eq!(inbound.body, "is the cron issue filed?", "with what it said");

        let outbound = render_units(&[sent]);
        let ChatUnit::PeerCard(outbound) = &outbound[0] else {
            panic!("a peer card");
        };
        assert!(!outbound.inbound, "the call the session made reads as outbound");
        assert_eq!(outbound.peer, "forge", "and names the seat it went to");
        assert_eq!(outbound.body, "did it land?", "with what it asked");
    }

    /// The decoded inbound frames of one captured baseline, in order.
    ///
    /// A capture rather than a fixture: the API clock counts up across the
    /// session on real traffic, and a hand-built row would only restate the
    /// reading it is meant to check.
    fn captured(name: &str) -> Vec<Message> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../forge-test-harness/baselines/sdk");
        // The version directory is named for the CLI the capture came from,
        // so the capture is what identifies it: two directories holding the
        // same name would make this walk pick by directory order.
        let holding: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("the baseline directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.join(format!("{name}.jsonl")).is_file())
            .collect();
        assert_eq!(holding.len(), 1, "one captured version holds {name}");
        let raw =
            std::fs::read_to_string(holding[0].join(format!("{name}.jsonl"))).expect("the capture");
        raw.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|envelope| envelope["dir"] == "in")
            .filter_map(|envelope| envelope["line"].as_str().map(str::to_owned))
            .filter_map(|line| serde_json::from_str::<Message>(&line).ok())
            .collect()
    }

    /// A settled turn's row reports what that turn used, not what the
    /// session had reached. The capture's results are really cumulative -
    /// 3171 ms of API time at the second one is 1281 ms of it for that turn -
    /// and the last carries an unattributed zero, which is a frame that
    /// measured nothing rather than a turn that took no time.
    ///
    /// The first result has no previous one to subtract, so its figure is
    /// absent rather than the session's whole clock: a fold cannot know
    /// whether the list it was handed starts at the session's beginning, and
    /// on this page it never does.
    #[test]
    fn a_settled_turn_reports_its_own_api_time() {
        let reported: Vec<Option<u64>> = render_units(&captured("compact"))
            .into_iter()
            .filter_map(|unit| match unit {
                ChatUnit::TurnReport { info, .. } => Some(info.api_ms),
                _ => None,
            })
            .collect();

        assert_eq!(
            reported,
            [None, Some(1_281), Some(1_158), Some(1_383), Some(1_381), Some(1_336), None],
            "the deltas the captured clock works out to, and nothing where there is no anchor",
        );
    }

    /// A fold handed a mid-session stretch of results reports nothing for the
    /// first one, which is the shape this page always gives it: the read
    /// keeps conversation rows alone, so the results the fold sees start
    /// wherever the page attached. Taking the cumulative field there would
    /// report the session's clock as one turn's.
    #[test]
    fn a_fold_that_joins_mid_session_reports_nothing_for_its_first_result() {
        let results: Vec<Message> = captured("compact").into_iter().filter(is_result).collect();
        assert_eq!(results.len(), 7, "the capture holds the seven results this walks");
        // The third result onward: the page joins a session, it does not
        // start one.
        let reported: Vec<Option<u64>> = render_units(&results[2..])
            .into_iter()
            .filter_map(|unit| match unit {
                ChatUnit::TurnReport { info, .. } => Some(info.api_ms),
                _ => None,
            })
            .collect();

        assert_eq!(
            reported,
            [None, Some(1_383), Some(1_381), Some(1_336), None],
            "the first figure is unknown, and the ones after it are still deltas",
        );
    }

    fn is_result(msg: &Message) -> bool {
        matches!(msg, Message::Result { .. })
    }

    /// The wire's thinking counter restarts at every thinking block, so a
    /// turn's estimate is the sum of its deltas. This capture is the shape:
    /// its counter reaches 161, starts again at 50, and ends at 349, and the
    /// seven blocks it carried are 510 tokens of thinking. Reading the
    /// absolute field reports 349 and a counter that moves backwards at the
    /// restart.
    #[test]
    fn a_turn_that_thought_twice_reports_every_block() {
        let reported = render_units(&captured("exit_plan_mode"))
            .into_iter()
            .find_map(|unit| match unit {
                ChatUnit::TurnReport { info, .. } => Some(info.thinking_tokens),
                _ => None,
            })
            .expect("the capture has a settled turn");

        assert_eq!(reported, Some(510), "every block the turn thought, not the last one");
    }

    /// A question answered with a note and nothing picked. The CLI records
    /// the note under the result's `annotations` and leaves the answer empty,
    /// so an empty answer is no answer and the note is what was said.
    #[test]
    fn a_note_beside_an_empty_answer_is_what_was_said() {
        let units = render_units(&captured("question_notes_only_response"));
        let asked = units
            .iter()
            .find_map(|unit| match unit {
                ChatUnit::QuestionCard { asked } => Some(asked),
                _ => None,
            })
            .expect("the capture asks a question");

        assert_eq!(asked[0].question, "Which colour do you prefer?");
        assert!(asked[0].picked_labels.is_empty(), "nothing was picked");
        assert_eq!(
            asked[0].typed_note.as_deref(),
            Some("test feedback from forge unified-prompt harness"),
            "and the note beside the empty answer is what the card carries",
        );
    }
}

/// One policy for the text a `queued_command` block carries: the fold
/// above reads it, and so does the TUI's own message walker.
#[cfg(test)]
mod queued_command_tests {
    use super::queued_command_text;
    use serde_json::json;

    #[test]
    fn plain_string_prompt_round_trips() {
        let prompt = json!("Q1, let's give.");
        assert_eq!(queued_command_text(&prompt), "Q1, let's give.");
    }

    #[test]
    fn multi_block_prompt_concatenates_text_blocks() {
        let prompt = json!([
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {"type": "base64", "data": "..."}},
        ]);
        assert_eq!(queued_command_text(&prompt), "look at this\n[image]");
    }

    #[test]
    fn unknown_inner_block_type_renders_as_placeholder() {
        let prompt = json!([
            {"type": "text", "text": "hi"},
            {"type": "future_block_type", "payload": "..."},
        ]);
        assert_eq!(queued_command_text(&prompt), "hi\n[future_block_type]");
    }

    #[test]
    fn empty_array_returns_empty_string() {
        let prompt = json!([]);
        assert_eq!(queued_command_text(&prompt), "");
    }

    #[test]
    fn non_array_non_string_falls_back_to_json_literal() {
        let prompt = json!({"weird": "shape"});
        let out = queued_command_text(&prompt);
        assert!(out.contains("weird"), "an unrenderable shape still shows something: {out}");
    }
}

#[cfg(test)]
mod turn_report_tests {
    use forge_primitives::{
        AssistantEnvelope, ContentBlock, Message, StopReason, Usage, UserEnvelope,
    };

    use super::{ChatUnit, render_units};

    /// An assistant frame the CLI wrote at `at`, having stopped for `stop`.
    fn assistant_at(text: &str, at: &str, stop: StopReason, usage: Option<Usage>) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: "msg_01".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-opus-5".to_owned(),
                content: vec![ContentBlock::Text {
                    text: text.to_owned(),
                    extras: serde_json::Map::new(),
                }],
                stop_reason: Some(stop),
                stop_sequence: None,
                usage,
                extras: serde_json::Map::new(),
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
            timestamp: Some(at.to_owned()),
            extras: serde_json::Map::new(),
        }
    }

    fn user_at(text: &str, at: &str) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::Text {
                    text: text.to_owned(),
                    extras: serde_json::Map::new(),
                }],
                extras: serde_json::Map::new(),
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
            timestamp: Some(at.to_owned()),
            synthetic: false,
            extras: serde_json::Map::new(),
        }
    }

    /// A session-state frame, as the wire sends it.
    fn state_frame(state: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "session_state_changed",
            "state": state,
            "uuid": format!("state-{state}"),
            "session_id": "session",
        }))
        .expect("a state frame")
    }

    fn use_at(id: &str, at: &str) -> Message {
        let Message::Assistant { message, session_id, parent_tool_use_id, error, uuid, .. } =
            assistant_at("", at, StopReason::ToolUse, None)
        else {
            panic!("assistant_at builds one");
        };
        let mut message = message;
        message.id = format!("msg_{id}");
        message.content = vec![ContentBlock::ToolUse {
            id: id.to_owned(),
            name: "Read".to_owned(),
            input: serde_json::json!({"file_path": "/tmp/a.rs"}),
            extras: serde_json::Map::new(),
        }];
        Message::Assistant {
            message,
            session_id,
            parent_tool_use_id,
            error,
            uuid,
            timestamp: Some(at.to_owned()),
            extras: serde_json::Map::new(),
        }
    }

    fn reports(units: &[ChatUnit]) -> Vec<&crate::model::TurnInfo> {
        units
            .iter()
            .filter_map(|unit| match unit {
                ChatUnit::TurnReport { info, .. } => Some(info),
                _ => None,
            })
            .collect()
    }

    /// What each settled row is named, in the order the fold emitted them.
    fn keys(units: &[ChatUnit]) -> Vec<Option<String>> {
        units
            .iter()
            .filter_map(|unit| match unit {
                ChatUnit::TurnReport { key, .. } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    /// One result frame, as the wire sends it: the turn it closes, and the
    /// id that frame carries.
    fn result_frame(uuid: Option<&str>) -> Message {
        Message::Result {
            subtype: "success".to_owned(),
            session_id: "session".to_owned(),
            is_error: false,
            num_turns: 1,
            duration_ms: 41_059,
            duration_api_ms: 40_742,
            stop_reason: None,
            total_cost_usd: Some(0.16),
            usage: None,
            result: None,
            structured_output: None,
            extras: serde_json::Map::new(),
            model_usage: None,
            permission_denials: None,
            errors: None,
            uuid: uuid.map(str::to_owned),
            terminal_reason: None,
        }
    }

    /// A settled row is named for the turn it is rather than for its place:
    /// the name is the instant the turn's own first row carried, so a turn
    /// inserted above another leaves that one named the same. A count of the
    /// rows drawn before it renames every row after the insertion.
    #[test]
    fn a_settled_row_is_named_for_its_turn_and_not_for_its_place() {
        let first = [
            user_at("first", "2026-04-22T04:15:27.000Z"),
            assistant_at("done", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
            user_at("second", "2026-04-22T05:00:00.000Z"),
            assistant_at("done", "2026-04-22T05:01:00.000Z", StopReason::EndTurn, None),
        ];
        // The same conversation with a turn inserted above the last one.
        let inserted = [
            user_at("first", "2026-04-22T04:15:27.000Z"),
            assistant_at("done", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
            user_at("inserted", "2026-04-22T04:30:00.000Z"),
            assistant_at("done", "2026-04-22T04:31:00.000Z", StopReason::EndTurn, None),
            user_at("second", "2026-04-22T05:00:00.000Z"),
            assistant_at("done", "2026-04-22T05:01:00.000Z", StopReason::EndTurn, None),
        ];

        assert_eq!(
            keys(&render_units(&first)),
            [
                Some("2026-04-22T04:15:27.000Z".to_owned()),
                Some("2026-04-22T05:00:00.000Z".to_owned()),
            ],
            "each row is named for its own turn's opening instant",
        );
        assert_eq!(
            keys(&render_units(&inserted)).last(),
            Some(&Some("2026-04-22T05:00:00.000Z".to_owned())),
            "and an inserted turn leaves the row after it named the same",
        );
    }

    /// A turn the wire settled is named the way a turn read back is: the
    /// instant its own first row carried. The frame's id is the fallback for
    /// a turn the fold placed no clock on, and naming the row for the frame
    /// instead would rename every one of them across a reload.
    #[test]
    fn a_wire_settled_turn_is_named_for_its_opening_instant() {
        let messages = [
            user_at("ship it", "2026-04-22T04:15:27.000Z"),
            assistant_at("Shipped.", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
            result_frame(Some("result-1")),
        ];

        assert_eq!(
            keys(&render_units(&messages)),
            [Some("2026-04-22T04:15:27.000Z".to_owned())],
            "the turn's own instant, not the id of the frame that reported it",
        );
    }

    /// A turn the fold can place no clock on and whose frame carried no id
    /// is named by nothing at all. A row a view does not remember open is
    /// the safe answer; a name it shares with a neighbour is the one that
    /// opens the wrong body.
    #[test]
    fn a_turn_nothing_can_name_carries_no_name() {
        assert_eq!(
            keys(&render_units(&[result_frame(None)])),
            [None],
            "no clock and no frame id, so no name rather than another row's",
        );
    }

    /// A turn the fold placed no clock on takes the id of the frame that
    /// reported it. The arm is reachable rather than belt-and-braces: the
    /// baselines carry result frames with no timestamp, so a fold can meet
    /// one with nothing in the trace to name it by.
    #[test]
    fn a_turn_with_no_clock_takes_the_frame_id() {
        assert_eq!(
            keys(&render_units(&[result_frame(Some("result-1"))])),
            [Some("result-1".to_owned())],
            "nothing opened a trace, so the frame's own id names the row",
        );
    }

    /// Two turns can open on one clock - the CLI writes a command and its
    /// answer in the same millisecond - and the page keeps ONE map of the
    /// rows it has been told to open, so two rows sharing a name open and
    /// close together. The second turn takes a name of its own.
    #[test]
    fn two_turns_opening_on_one_clock_do_not_share_a_name() {
        let messages = [
            user_at("first", "2026-04-22T04:15:27.000Z"),
            assistant_at("done", "2026-04-22T04:15:27.000Z", StopReason::EndTurn, None),
            user_at("second", "2026-04-22T04:15:27.000Z"),
            assistant_at("done", "2026-04-22T04:15:27.000Z", StopReason::EndTurn, None),
        ];

        assert_eq!(
            keys(&render_units(&messages)),
            [
                Some("2026-04-22T04:15:27.000Z".to_owned()),
                Some("2026-04-22T04:15:27.000Z#2".to_owned()),
            ],
            "the second turn is named its own rather than the first's",
        );
    }

    /// A transcript holds no result frame, so a turn read from one draws its
    /// own row or draws nothing at all. The row's clock is the rows' own:
    /// first write to last, and the end is the instant the last row carried.
    #[test]
    fn a_turn_read_from_a_transcript_draws_its_own_row() {
        let messages = [
            user_at("make the call tree the default", "2026-04-22T04:15:27.000Z"),
            assistant_at("Done.", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
        ];
        let units = render_units(&messages);

        let [info] = reports(&units)[..] else {
            panic!("one settled turn draws one row, got {units:?}");
        };
        assert_eq!(
            info.duration_ms,
            Some(161_000),
            "the wall clock between the turn's own two rows",
        );
        assert_eq!(
            info.ended_at_utc.as_deref(),
            Some("2026-04-22T04:18:08.000Z"),
            "and the end is the last row's own instant, for the view to place",
        );
        assert_eq!(
            info.ended_at_local, None,
            "which the fold does not render: the reader's zone is the view's to know",
        );
        assert_eq!(info.model.as_deref(), Some("claude-opus-5"), "with the model that wrote it");
    }

    /// The wire's own result closes a turn with the CLI's measurements, and
    /// the fold must not add a second derived row on top of it.
    #[test]
    fn a_turn_the_wire_settled_draws_one_row_and_not_two() {
        let messages = [
            user_at("ship it", "2026-04-22T04:15:27.000Z"),
            assistant_at("Shipped.", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
            result_frame(None),
        ];
        let units = render_units(&messages);

        let [info] = reports(&units)[..] else {
            panic!("one turn draws one row, got {units:?}");
        };
        assert_eq!(
            info.duration_ms,
            Some(41_059),
            "and the number is the CLI's own, not one derived from the rows",
        );
    }

    /// A turn that handed back for a tool call has not ended, so the fold
    /// draws no row for it while it is the last thing in the conversation:
    /// the view draws the live one while it runs.
    #[test]
    fn a_turn_still_calling_tools_draws_no_row_of_its_own() {
        let messages = [
            user_at("read the file", "2026-04-22T04:15:27.000Z"),
            use_at("toolu_1", "2026-04-22T04:15:29.000Z"),
        ];
        let units = render_units(&messages);

        assert!(reports(&units).is_empty(), "nothing ended this turn: {units:?}");
    }

    /// A turn the transcript has already closed draws its row whatever its
    /// last frame stopped for: the rows after it are the CLI's own write that
    /// the turn is over, which is what a turn ending on a tool call and
    /// reopened by a row nobody typed looks like.
    #[test]
    fn a_turn_the_transcript_closed_draws_its_row() {
        let messages = [
            user_at("run the skill", "2026-04-22T04:15:27.000Z"),
            use_at("toolu_1", "2026-04-22T04:15:29.000Z"),
            user_at("Base directory for this skill: /tmp/skill", "2026-04-22T04:15:31.000Z"),
        ];
        let units = render_units(&messages);

        let [info] = reports(&units)[..] else {
            panic!("the closed turn draws its row, got {units:?}");
        };
        assert_eq!(info.duration_ms, Some(2_000), "over its own two rows");
    }

    /// A pair of row clocks that runs backwards or does not parse gives no
    /// span, and the header prints a span where one belongs: a turn with no
    /// usable pair draws no row rather than a zeroed clock. A replayed row
    /// carries the stamp it was written with, so the backwards pair is real.
    #[test]
    fn a_turn_whose_clocks_give_no_span_draws_no_row() {
        let backwards = [
            user_at("count it", "2026-04-22T04:18:08.000Z"),
            assistant_at("done", "2026-04-22T04:15:27.000Z", StopReason::EndTurn, None),
        ];
        assert!(
            reports(&render_units(&backwards)).is_empty(),
            "a span that runs backwards is not one",
        );

        let unreadable = [
            user_at("count it", "2026-04-22T04:15:27.000Z"),
            assistant_at("done", "nope", StopReason::EndTurn, None),
        ];
        assert!(
            reports(&render_units(&unreadable)).is_empty(),
            "and neither is a clock that does not parse",
        );
    }

    /// A turn whose frames carried no clock draws no row: a report is not a
    /// place to invent a time, and the header would otherwise print a zero
    /// where the turn's own clock goes.
    #[test]
    fn a_turn_with_no_row_clocks_draws_no_row() {
        let messages = [
            Message::User {
                message: UserEnvelope {
                    role: "user".to_owned(),
                    content: vec![ContentBlock::Text {
                        text: "no clocks here".to_owned(),
                        extras: serde_json::Map::new(),
                    }],
                    extras: serde_json::Map::new(),
                },
                session_id: "session".to_owned(),
                parent_tool_use_id: None,
                uuid: None,
                tool_use_result: None,
                timestamp: None,
                synthetic: false,
                extras: serde_json::Map::new(),
            },
            Message::Assistant {
                message: AssistantEnvelope {
                    id: "msg_01".to_owned(),
                    role: "assistant".to_owned(),
                    model: "claude-opus-5".to_owned(),
                    content: vec![ContentBlock::Text {
                        text: "nor here".to_owned(),
                        extras: serde_json::Map::new(),
                    }],
                    stop_reason: Some(StopReason::EndTurn),
                    stop_sequence: None,
                    usage: None,
                    extras: serde_json::Map::new(),
                },
                session_id: "session".to_owned(),
                parent_tool_use_id: None,
                error: None,
                uuid: None,
                timestamp: None,
                extras: serde_json::Map::new(),
            },
        ];
        let units = render_units(&messages);

        assert!(
            reports(&units).is_empty(),
            "a row of dashes and a zeroed clock says less than none: {units:?}",
        );
    }

    /// The row carries the input side the assistant frames reported, summed
    /// once per message, and nothing a transcript cannot support. The counts
    /// that stay absent are the ones the row has no record of, so a later
    /// pass filling a dash from another field fails here rather than shipping
    /// a number the transcript never wrote.
    #[test]
    fn a_read_rows_counts_are_summed_once_per_message() {
        let usage = Usage {
            input_tokens: 7,
            output_tokens: 999,
            cache_read_input_tokens: 1_000,
            cache_creation_input_tokens: 200,
            extras: serde_json::Map::new(),
        };
        let messages = [
            user_at("count it", "2026-04-22T04:15:27.000Z"),
            assistant_at(
                "part one",
                "2026-04-22T04:15:31.000Z",
                StopReason::ToolUse,
                Some(usage.clone()),
            ),
            assistant_at("part two", "2026-04-22T04:15:33.000Z", StopReason::EndTurn, Some(usage)),
        ];
        let units = render_units(&messages);

        let [info] = reports(&units)[..] else {
            panic!("one settled turn draws one row, got {units:?}");
        };
        assert_eq!(info.input_tokens, Some(7), "one message, counted once");
        assert_eq!(info.cache_read_tokens, Some(1_000), "and its cache reads once too");
        assert_eq!(info.cache_written_tokens, Some(200), "including what it wrote");
        assert_eq!(
            info.output_tokens, None,
            "an assistant frame's output count is a streaming placeholder, not a count",
        );
        assert_eq!(info.api_ms, None, "and no turn writes the API clock to a transcript");
        assert_eq!(info.session_cost_usd, None, "nor the cost, which the CLI keeps to itself");
        assert_eq!(
            info.thinking_tokens, None,
            "nor the reasoning estimate, which only deltas carry"
        );
    }

    /// A session-state frame is the boundary the wire alone reports: a turn
    /// the transcript left open is closed by it, and the state says whether
    /// the turn after it is running.
    #[test]
    fn a_state_frame_closes_the_turn_before_it_and_holds_the_one_after() {
        let read_then_running = [
            user_at("count it", "2026-04-22T04:15:27.000Z"),
            assistant_at("done", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
            state_frame("running"),
        ];
        assert_eq!(
            reports(&render_units(&read_then_running)).len(),
            1,
            "a turn read from a transcript keeps its row when the next one starts",
        );

        let running = [
            state_frame("running"),
            user_at("and then", "2026-04-22T04:20:00.000Z"),
            assistant_at("more", "2026-04-22T04:21:00.000Z", StopReason::EndTurn, None),
        ];
        assert!(
            reports(&render_units(&running)).is_empty(),
            "the turn the session is running draws the view's live row, not one from here",
        );

        let settled = [
            state_frame("running"),
            user_at("and then", "2026-04-22T04:20:00.000Z"),
            assistant_at("more", "2026-04-22T04:21:00.000Z", StopReason::EndTurn, None),
            state_frame("idle"),
        ];
        assert_eq!(
            reports(&render_units(&settled)).len(),
            1,
            "and nothing running leaves the row to the fold again",
        );
    }

    /// A turn the next prompt closed draws its row whatever the session
    /// reports: the gate belongs to the trailing turn alone.
    #[test]
    fn a_closed_turn_draws_while_a_later_one_runs() {
        let messages = [
            user_at("first", "2026-04-22T04:15:27.000Z"),
            assistant_at("one", "2026-04-22T04:16:00.000Z", StopReason::EndTurn, None),
            state_frame("running"),
            user_at("second", "2026-04-22T04:20:00.000Z"),
        ];
        assert_eq!(
            reports(&render_units(&messages)).len(),
            1,
            "the first turn's row survives a running second",
        );
    }

    /// Two turns each draw their own row, in the order they ran.
    #[test]
    fn each_turn_draws_its_own_row() {
        let messages = [
            user_at("first", "2026-04-22T04:15:27.000Z"),
            assistant_at("one", "2026-04-22T04:16:00.000Z", StopReason::EndTurn, None),
            user_at("second", "2026-04-22T04:20:00.000Z"),
            assistant_at("two", "2026-04-22T04:21:30.000Z", StopReason::EndTurn, None),
        ];
        let units = render_units(&messages);

        let finished: Vec<Option<u64>> = reports(&units).iter().map(|i| i.duration_ms).collect();
        assert_eq!(
            finished,
            [Some(33_000), Some(90_000)],
            "each turn's own span, not the session's",
        );
    }
}
