//! The conversation fold: wire messages in, where its turns sit out.
//!
//! The fold walks the whole conversation in one pass and reports the message
//! each turn opens at, which is what a page is cut on, plus the task endings
//! it holds. The rules it folds by are not restated here: the row predicates
//! it opens a turn by ([`forge_workspace::conversation_turns`]) are the ones
//! the conversation window reads too, and the envelope parser
//! ([`crate::envelope`]) is the one the read path uses.

use std::collections::HashMap;

use forge_primitives::{ContentBlock, Message, StopReason, ToolCallStatus};
// The row predicates the fold opens a turn by, which the conversation window
// reads too: they live in the window's crate so the two share one copy.
use forge_workspace::conversation_turns::{
    claims_skill_call, forge_minted, is_continuation, is_dispatched, is_image_note,
    is_interrupt_marker, is_task_notice,
};

use crate::envelope::detect_inbound;

/// Where one turn sits in the conversation it was folded from, and what names
/// it.
///
/// **In MESSAGE terms**, because a page crosses as whole turns: a client is
/// handed each turn's messages and folds them itself, so the boundary a page
/// is cut on has to name a message.
pub struct TurnSpan {
    /// The message the turn opens at.
    pub opens_at: usize,
    /// The key the fold named the turn by, `None` when nothing did - a
    /// conversation read from a transcript carries no `Result` frame, and the
    /// key comes from one.
    pub key: Option<String>,
}

/// A folded conversation: where its turns sit, and the endings it holds.
pub struct Rendered {
    pub turns: Vec<TurnSpan>,
    /// Every task ending the conversation holds, by the call each names.
    ///
    /// Read here because the fold is where it is read from at all - the
    /// preamble's pass over the messages - and carried out so the turns built
    /// from this answer do not walk the conversation again for it. A page is
    /// the case that pays: it is built while a reader scrolls.
    pub endings: HashMap<String, TaskEnding>,
}

/// Whether any of `open` is answered later than `from`.
///
/// The settled half of the delivery rule's bound: a transcript keeps a call
/// whose result never arrived - the restart shape (#1836) - and a row after it
/// belongs to the next turn. Asked on demand, at the rare row that needs it,
/// so a fold over a settled conversation pays a scan only where it joins; the
/// live walk never asks, because the caller's flag answers first.
fn answered_later(messages: &[Message], from: usize, open: &[String]) -> bool {
    messages[from + 1..].iter().any(|message| {
        let Message::User { message: envelope, .. } = message else {
            return false;
        };
        envelope.content.iter().any(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. }
            | ContentBlock::ServerToolResult { tool_use_id, .. } => {
                open.iter().any(|id| id == tool_use_id)
            }
            _ => false,
        })
    })
}

/// Open a turn, answering the span it opened: the one a page is cut on and the
/// one a later report names.
///
/// **One place opens a turn.** The fold has two ways in - a block that is the
/// user's own turn, and a queued prompt hoisted into one - and a span pushed
/// at only one of them leaves every turn of the other kind outside the page's
/// boundary list.
fn open_turn(turns: &mut Vec<TurnSpan>, index: usize) -> usize {
    turns.push(TurnSpan { opens_at: index, key: None });
    turns.len() - 1
}

/// Name the turn the fold has open, if it has one: the report that closed a
/// turn is what names it.
fn name_open(turns: &mut [TurnSpan], open: Option<usize>, key: Option<String>) {
    if let Some(span) = open.and_then(|at| turns.get_mut(at)) {
        span.key = key;
    }
}

/// Fold a conversation into where its turns sit in the message list, and the
/// task endings it holds.
///
/// One walk serves both: a second boundary rule over the messages is how the
/// two would come to disagree about where a turn begins, and the page is cut
/// on the answer.
///
/// `live` is the session's own turn state at this read, carried by the
/// CALLER rather than read off a frame: the frame that would say it
/// (`system/session_state_changed`) is emitted only behind an environment
/// variable forge never sets and appears in no capture, so the transport that
/// holds the seat's frames derives the delivered rule instead (a turn is open
/// until its `Result`, or the error the CLI gives up with) and hands the
/// answer in. The settled reads - a transcript on disk, a span page - pass
/// `false` or the seat's own flag where that is at hand.
pub fn render(messages: &[Message], live: bool) -> Rendered {
    let endings = task_endings(messages);
    let mut traced = TurnTrace::default();
    // The names this fold has already given out, so a second turn opening on
    // one clock takes its own rather than the first turn's.
    let mut keys: HashMap<String, usize> = HashMap::new();
    // Where each turn opens, taken as the fold opens it: one push per turn, in
    // order.
    let mut turns: Vec<TurnSpan> = Vec::new();
    // The turn the fold has open, which is the span a report names.
    let mut open: Option<usize> = None;
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
    // The open turn's calls still waiting on their results, oldest first. A
    // running turn's row that would otherwise open one - the chase's board
    // notice, a mid-turn prompt - is DELIVERED into this turn, and opening a
    // turn for it cuts the calls above it off from the answers below: every
    // reader of the fold then draws the call failed with no result (#2379).
    //
    // **The rule's bound is the turn's liveness, which the caller carries.**
    // While the seat's turn is live, an open call is mid-flight whatever the
    // list holds - on the live walk the result is not written yet, so a bound
    // read off the list alone would stop protecting exactly the window #2379
    // is about. The flag is the whole seat's, so an older crash-shaped turn
    // sitting in the window keeps its early row too while a later turn runs -
    // the same "whatever the list holds" policy, read on the window rather
    // than the turn. A settled shape with no result is the crash one (#1836):
    // there the open call is never coming back and the row after it opens its
    // own turn, which is what `answered_later` decides.
    let mut open_calls: Vec<String> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        // A sub-agent's frames are the SUBAGENTS surface's, not the chat's.
        if is_dispatched(message) {
            continue;
        }
        // The boundary a compaction left. It opens nothing here - the web
        // client folds the frame itself - but which turn it lands in is what
        // the continuation prompt after it must join, so it is watched.
        if matches!(message, Message::CompactBoundary { .. }) {
            turn_boundary = true;
        }
        // A settled turn's report. It arrives as a message of its own rather
        // than as a block, so it ends the turn the calls before it built and
        // names it.
        if let Message::Result { uuid, .. } = message {
            let key = take_key(&mut keys, traced.opened.clone(), uuid.as_deref());
            name_open(&mut turns, open, key);
            traced = TurnTrace::default();
            open_calls.clear();
            continue;
        }
        let (assistant, row_uuid, content) = match message {
            Message::Assistant { message: envelope, .. } => {
                (true, None, envelope.content.as_slice())
            }
            Message::User { message: envelope, uuid, .. } => {
                (false, uuid.as_deref(), envelope.content.as_slice())
            }
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
                // row as the caption. It opens no turn of its own either way.
                ContentBlock::Text { text, .. }
                    if !assistant && turn_calls && is_image_note(text) => {}
                // The harness's task ending in its other carrier: the same XML
                // written into a user row, which the scan hands on as this
                // plain text. A turn opened for it draws a task id and an
                // output path attributed to the reader, so the row opens none -
                // and the ending it carries was read in the pre-pass above,
                // which is the only place that can see across turns.
                ContentBlock::Text { text, .. } if !assistant && is_task_notice(text) => {}
                // The CLI's interrupt marker, written as the reader's own row
                // INSIDE the turn it ended: a turn opened for it separates the
                // failing `Result` from the call it ended, so the report and
                // the failure line land in a turn holding no call (#1323).
                ContentBlock::Text { text, .. } if !assistant && is_interrupt_marker(text) => {}
                ContentBlock::Text { text, .. } => {
                    // An envelope is not the reader's own words: a peer message
                    // is a user row the CLI answered as a turn of its own, so
                    // the work above it belongs to the turn before, and every
                    // other kind the workspace injects - a delivery, a fire, a
                    // failure - opens no turn either.
                    if !assistant && detect_inbound(text).is_some() {
                        close_traced(&mut traced, true, &mut keys, &mut turns, open);
                        continue;
                    }
                    // **A forge-minted row arriving while the open turn waits
                    // on a call was delivered INTO it**: the CLI presses a
                    // mid-turn write into the running turn, and the chase's
                    // board notice while an `agents__despawn` is out is the
                    // everyday one. A turn opened for it cuts the call off
                    // from the result below, and every reader of the fold
                    // draws the despawn failed with no result while it came
                    // back cleanly (#2379). A row the CLI wrote itself is not
                    // this - it opens a turn as it always did.
                    if !assistant
                        && forge_minted(row_uuid)
                        && open.is_some()
                        && !open_calls.is_empty()
                        && (live || answered_later(messages, index, &open_calls))
                    {
                        continue;
                    }
                    // The user's own row opens a turn, so the work above it is
                    // named first; the assistant's prose stays inside the turn
                    // already open.
                    if !assistant {
                        close_traced(&mut traced, true, &mut keys, &mut turns, open);
                        open = Some(open_turn(&mut turns, index));
                        turn_skills.clear();
                        turn_boundary = false;
                        turn_calls = false;
                        open_calls.clear();
                    }
                }
                ContentBlock::QueuedCommand { prompt, command_mode, source_uuid, .. } => {
                    // The harness's own background-completion notice rides the
                    // same block as a prompt, and the scan hoists it into a
                    // user envelope without reading the mode - so a turn opened
                    // for it is a row nothing fills, which the page draws as a
                    // blank line. A view never draws one: it filters the
                    // row rather than the turn.
                    if is_completion_notice(command_mode.as_deref(), prompt) {
                        continue;
                    }
                    // The same rule as the plain row above: a forge-minted
                    // queued write pressed into a running turn - a call still
                    // waiting on its result - stays in that turn (#2379). The
                    // marker rides the block's `source_uuid` when the CLI keeps
                    // the attachment shape, and the enclosing row's id when
                    // the scan hoisted it.
                    if (forge_minted(source_uuid.as_deref()) || forge_minted(row_uuid))
                        && open.is_some()
                        && !open_calls.is_empty()
                        && (live || answered_later(messages, index, &open_calls))
                    {
                        continue;
                    }
                    close_traced(&mut traced, true, &mut keys, &mut turns, open);
                    open = Some(open_turn(&mut turns, index));
                    turn_skills.clear();
                    turn_boundary = false;
                    turn_calls = false;
                    open_calls.clear();
                }
                ContentBlock::ToolUse { id, name, input, .. }
                | ContentBlock::ServerToolUse { id, name, input, .. } => {
                    turn_calls = true;
                    open_calls.push(id.clone());
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
                }
                // A result answers a call of the open turn: the turn is no
                // longer waiting on it, which is what tells a row arriving
                // later it was pressed into a running turn - or not.
                ContentBlock::ToolResult { tool_use_id, .. }
                | ContentBlock::ServerToolResult { tool_use_id, .. } => {
                    open_calls.retain(|open| open != tool_use_id);
                }
                // A result is not a row of its own: it is what the call
                // it answers already carries.
                _ => {}
            }
        }
        // Watched after the turn it may have started has been written out:
        // a prompt's own clock belongs to the turn it opens, not to the one
        // it ended.
        traced.watch(message);
    }
    // The trailing turn is the one a running session is still writing, and
    // the view draws that turn's row itself. Every other turn is closed by
    // the turn after it, or by the result that reported it.
    close_traced(&mut traced, false, &mut keys, &mut turns, open);
    Rendered { turns, endings }
}

/// The fold over a settled conversation: every fixture but the live window's,
/// which call [`render`] with the caller's own flag.
#[cfg(test)]
fn settled(messages: &[Message]) -> Rendered {
    render(messages, false)
}

/// The facts a turn's own rows carry, gathered while the fold walks them.
///
/// A transcript holds no result frame, so a turn read from one has no name of
/// its own unless one is derived here: the CLI writes that frame to the wire
/// and not to the file.
#[derive(Default)]
struct TurnTrace {
    /// The first and last clocks the turn's rows carried.
    opened: Option<String>,
    ended: Option<String>,
    /// The stop the turn's last assistant frame reported. A frame handing
    /// back for a tool call has not ended a turn.
    stopped: Option<StopReason>,
    /// Whether the turn wrote an assistant frame at all: a prompt nothing
    /// answered draws no row, the way a view draws none.
    worked: bool,
}

impl TurnTrace {
    fn watch(&mut self, message: &Message) {
        match message {
            Message::Assistant { message: envelope, timestamp, .. } => {
                self.worked = true;
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
}

/// Name the turn a transcript's own rows support, then start the next turn's
/// trace. A turn the wire already settled is named by its own result frame and
/// this one nothing, so the two never name one turn twice.
///
/// `known_over` is whether whatever closed the turn says so - the next prompt.
/// A turn closing at the end of a conversation has nothing saying it, and
/// needs its own last frame to have stopped rather than handed back for a
/// tool call.
fn close_traced(
    trace: &mut TurnTrace,
    known_over: bool,
    keys: &mut HashMap<String, usize>,
    turns: &mut [TurnSpan],
    open: Option<usize>,
) {
    if trace.drew_work() && (known_over || trace.stopped_of_its_own_accord()) {
        let key = take_key(keys, trace.opened.clone(), None);
        name_open(turns, open, key);
    }
    *trace = TurnTrace::default();
}

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

/// Whether a queued block is the harness's background-completion notice
/// rather than anything a person said.
///
/// The mode is the field that says which of the three kinds arrived, and
/// the text's own prefix is the other signal. Both are read here, because a
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
/// The same map a view applies to `task_updated.patch.status`: the wire
/// says `running` where the row says in progress, and `stopped` is its word
/// for a graceful cancel, which draws as the kill it is.
///
/// **An unrecognised word is `None`.** The word arrives here after the call
/// is finished, so an unreadable word means the task ended without saying
/// how, and drawing the call as pending again would walk it back down.
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
/// follow their call closely. A pre-pass reads the whole conversation once,
/// keyed by call, so the walk reads the answer wherever it needs it.
///
/// **The last ending naming a call wins**, the only rule that reads a repeated
/// ending as the newer word rather than the older. It is load-bearing rather than
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

#[cfg(test)]
mod tests {
    use forge_primitives::{
        AssistantEnvelope, ContentBlock, Message, ToolCallStatus, UserEnvelope,
    };

    use super::{Rendered, TaskEnding, render, settled, task_ending};

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
    /// word these tests speak in, mapped to the SDK name it stands for.
    fn tool_call(family: &str) -> Message {
        let sdk_name = match family {
            "read" => "Read",
            "search" => "Grep",
            "bash" => "Bash",
            "edit" => "Edit",
            other => other,
        };
        assistant(vec![ContentBlock::ToolUse {
            id: format!("toolu_{family}"),
            name: sdk_name.to_owned(),
            input: serde_json::json!({"file_path": "src/lib.rs", "command": "just check"}),
            extras: serde_json::Map::new(),
        }])
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

    /// An envelope that is not agent traffic is not the user's own turn: a
    /// Gotify delivery, a cron fire, a Slack bundle, a failed delivery and a
    /// failed worker spawn each arrive as an envelope, and a turn opened for
    /// one draws a protocol header under the reader's own name.
    #[test]
    fn an_external_envelope_opens_no_turn() {
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

        let rendered = settled(&[quiet, loud, slack, failed]);

        assert!(
            rendered.turns.is_empty(),
            "a delivery, a fire and a failure each open no turn of their own",
        );
    }

    /// The CLI's interrupt marker opens no turn (#1323): it is a `user` row,
    /// and it arrives INSIDE the turn it ended - the in-flight call, then the
    /// marker, then the failing `Result` - so a turn opened for it lands the
    /// report and the failure line in a turn holding no call.
    #[test]
    fn the_interrupt_marker_opens_no_turn() {
        let prompt = user(vec![ContentBlock::Text {
            text: "run the checks".to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let call = tool_call("bash");
        let marker = user(vec![ContentBlock::Text {
            text: "[Request interrupted by user]".to_owned(),
            extras: serde_json::Map::new(),
        }]);

        let rendered = settled(&[prompt, call, marker]);

        assert_eq!(
            rendered.turns.len(),
            1,
            "the marker draws inside the turn it ended, not as one of its own",
        );

        // The prefix alone is not the marker: a truncated row is some other
        // text and opens a turn as one.
        let truncated = user(vec![ContentBlock::Text {
            text: "[Request interrupted by user".to_owned(),
            extras: serde_json::Map::new(),
        }]);
        let rendered = settled(&[
            user(vec![ContentBlock::Text {
                text: "run the checks".to_owned(),
                extras: serde_json::Map::new(),
            }]),
            tool_call("bash"),
            truncated,
        ]);
        assert_eq!(rendered.turns.len(), 2, "an unterminated prefix is not the marker");
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

    /// The result the tool at `tool_use_id` came back with.
    fn tool_result(tool_use_id: &str, is_error: bool) -> Message {
        user(vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content: serde_json::Value::String("output".to_owned()),
            is_error,
            extras: serde_json::Map::new(),
        }])
    }

    /// A user's mid-turn prompt reached the page. On the read path it has
    /// no other shape: the scan hoists the queued row into a user envelope
    /// so a reader can rebuild the bubble, and a fold that ignores the
    /// block drops the question the assistant went on to answer.
    #[test]
    fn a_queued_prompt_opens_a_turn_of_its_own() {
        let rendered = settled(&[queued_prompt("and keep the multiSelect case too")]);

        assert_eq!(rendered.turns.len(), 1, "the queued prompt opens one turn");
        assert_eq!(rendered.turns[0].opens_at, 0, "at the row the prompt arrived in");
    }

    /// The reader's own row, which opens the turn.
    fn reader_row(text: &str) -> Message {
        user(vec![ContentBlock::Text { text: text.to_owned(), extras: serde_json::Map::new() }])
    }

    /// The same row under an id forge minted for a send or a dispatch.
    fn forge_row(uuid: &str, text: &str) -> Message {
        let mut row = reader_row(text);
        if let Message::User { uuid: id, .. } = &mut row {
            *id = Some(uuid.to_owned());
        }
        row
    }

    /// The call a running turn is waiting on.
    fn despawn_call() -> Message {
        assistant(vec![ContentBlock::ToolUse {
            id: "call_1".to_owned(),
            name: "mcp__forge__agents__despawn".to_owned(),
            input: serde_json::json!({ "label": "probe" }),
            extras: serde_json::Map::new(),
        }])
    }

    /// **A forge-minted row pressed into a running turn stays in that turn.**
    ///
    /// The CLI delivers a mid-turn write into the running turn, and the board
    /// chase's notice while an `agents__despawn` is out is the everyday one:
    /// the transcript shows it between the call and the result the call waits
    /// on. A turn opened for it cuts the call off from its answer, and every
    /// reader of the fold - a page, a re-attach, a transcript read - draws the
    /// despawn failed with no result while it came back cleanly (#2379).
    #[test]
    fn a_mid_call_forge_row_stays_in_the_turn_it_was_pressed_into() {
        let rendered = settled(&[
            reader_row("despawn probe"),
            despawn_call(),
            forge_row(
                "forge_nudge_38_d56ff2d2",
                "task board: \"probe holds no row\" has no live owner",
            ),
            tool_result("call_1", false),
        ]);

        assert_eq!(
            rendered.turns.len(),
            1,
            "the notice opens no turn of its own while the call is out",
        );
        assert_eq!(rendered.turns[0].opens_at, 0, "the turn still opens on the prompt row");
    }

    /// The same marker on the queued carrier, which rides `source_uuid`.
    #[test]
    fn a_mid_call_queued_write_stays_too() {
        let mut notice = queued_prompt("task board: \"probe holds no row\" has no live owner");
        if let Message::User { message, .. } = &mut notice
            && let Some(ContentBlock::QueuedCommand { source_uuid, .. }) =
                message.content.first_mut()
        {
            *source_uuid = Some("forge_nudge_38_d56ff2d2".to_owned());
        }
        let rendered = settled(&[
            reader_row("despawn probe"),
            despawn_call(),
            notice,
            tool_result("call_1", false),
        ]);

        assert_eq!(rendered.turns.len(), 1, "the queued write opens no turn either");
    }

    /// The same row at a settled turn opens one, as the reader's own rows do:
    /// the call's result has landed, so nothing is being waited on.
    #[test]
    fn a_forge_row_at_a_settled_turn_opens_one() {
        let rendered = settled(&[
            reader_row("despawn probe"),
            despawn_call(),
            tool_result("call_1", false),
            forge_row(
                "forge_nudge_38_d56ff2d2",
                "task board: \"probe holds no row\" has no live owner",
            ),
        ]);

        assert_eq!(rendered.turns.len(), 2, "a settled turn's notice opens its own");
        assert_eq!(rendered.turns[1].opens_at, 3, "on the row it arrived in");
    }

    /// **A call whose result has not streamed yet is still a live turn.**
    ///
    /// The live fold runs over the incrementally-grown window, and during
    /// every in-flight call the result is not in the list yet - so a bound
    /// read off the list alone stops protecting exactly the window #2379 is
    /// about, and the notice opens its own turn until a later fold heals it.
    /// The caller's flag is what tells that apart from the crash shape, and it
    /// is the delivered signal: the init/result ladder the transport derives
    /// from the seat's own frames.
    #[test]
    fn a_live_turns_open_call_is_protected_before_its_result_lands() {
        let rendered = render(
            &[
                reader_row("despawn probe"),
                despawn_call(),
                forge_row(
                    "forge_nudge_38_d56ff2d2",
                    "task board: \"probe holds no row\" has no live owner",
                ),
            ],
            true,
        );

        assert_eq!(
            rendered.turns.len(),
            1,
            "the notice stays in the turn the caller reports live, with no result on the list yet",
        );
    }

    /// **A call whose result never comes does not latch the delivery rule.**
    ///
    /// The restart shape (#1836): the transcript keeps a `tool_use` with no
    /// `tool_result`, and no `Result` frame either - a transcript holds none -
    /// so a guard keyed on "a call is out" alone would read that turn as
    /// running for the rest of the conversation and swallow every forge row
    /// into one crash turn: the auto-continue a restarted seat gets, every
    /// nudge, every queued write.
    #[test]
    fn a_call_whose_result_never_comes_does_not_hold_the_next_turn_shut() {
        let rendered = settled(&[
            reader_row("run the checks"),
            despawn_call(),
            forge_row("forge_prompt_9_aaaaaaaa", "continue where you left off"),
        ]);

        assert_eq!(rendered.turns.len(), 2, "the later forge row opens its own turn");
        assert_eq!(rendered.turns[1].opens_at, 2, "on its own row, after the crash turn");
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
            settled(&[prompt(), call("unslop"), body("/Users/ved/.claude/skills/unslop")]);
        assert_eq!(rendered.turns.len(), 1, "the body opens no turn of its own");

        // The plugin spelling: the call says `a:b` where the path ends `b`.
        let cached = settled(&[
            prompt(),
            call("ui-ux-pro-max:ui-ux-pro-max"),
            body("/Users/ved/.claude/plugins/cache/x/ui-ux-pro-max/2.13.0"),
        ]);
        assert_eq!(cached.turns.len(), 1, "a versioned plugin path still matches its call");

        // The control: a body no call claimed opens a turn, so nothing is lost.
        let orphan = settled(&[prompt(), body("/Users/ved/.claude/skills/other")]);
        assert_eq!(orphan.turns.len(), 2, "an unclaimed body opens a turn as it always did");

        // The carrier a tool-invoked skill uses: the skill's own markdown,
        // named only by its title heading.
        let titled = settled(&[
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
        let pictured = settled(&[prompt(), tool_call("read"), note.clone()]);
        assert_eq!(pictured.turns.len(), 1, "the image line opens no turn of its own");

        // The control: with no call in the turn it opens one, so nothing is lost.
        let loose = settled(&[prompt(), note]);
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

        let rendered = settled(&[prompt(), boundary(), summary.clone()]);
        assert_eq!(rendered.turns.len(), 1, "the prompt opens no turn of its own");

        // The control: no boundary in the turn, so it opens one as it did.
        let orphan = settled(&[prompt(), summary]);
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
            let rendered = settled(&[notice]);
            assert!(rendered.turns.is_empty(), "a completion notice opens no turn: {signal}");
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
    /// a queued prompt opens a turn in the fold, and the scan leaves it alone.
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
            // The interrupt marker's own carriers (#1323): the plain row the
            // CLI writes, and one with leading whitespace, which the fold's
            // trimmed read accepts - the scan must read both as no-heads, or
            // the window could cut inside the turn the marker stays in.
            text("[Request interrupted by user]"),
            text("\n[Request interrupted by user]"),
            // **A forge-minted notice delivered mid-call** (#2379): the CLI
            // presses it into the running turn, and the fold keeps it there -
            // so the scan must name it too, or the window could start inside
            // the turn whose call is still out.
            call("Bash", serde_json::json!({ "command": "ls" }), "toolu_9"),
            forge_row("forge_nudge_9_deadbeef", "task board: a row lost its owner"),
            tool_result("toolu_9", false),
        ];

        let opened: Vec<usize> =
            settled(&messages).turns.iter().map(|span| span.opens_at).collect();
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
        assert!(
            !named.contains(&27) && !named.contains(&28),
            "the interrupt marker opens no turn in either walk, leading whitespace included: \
             scan {named:?}",
        );
        assert!(
            !opened.contains(&27) && !opened.contains(&28),
            "and the fold keeps both inside the turn it ended: fold {opened:?}",
        );
        // A forge-minted notice delivered at an open call (#2379) is the
        // forbidden direction this fixture exists for: the fold keeps it in
        // the running turn, so a scan naming it would cut the window inside
        // that turn.
        assert!(
            !opened.contains(&30),
            "the fold keeps the mid-call notice inside the turn whose call is out: fold {opened:?}",
        );
        assert!(!named.contains(&30), "and the scan names no head there either: scan {named:?}");
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

    /// A transcript seeded from `rows`, walked the way a view walks it: the
    /// scan, the replay synthesiser and the fold. The rows are the ones a
    /// test names, and they are real ones - a fold driven from hand-built
    /// frames would agree with itself about a shape the CLI does not write.
    fn folded(rows: &[&str]) -> Rendered {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        fleet.seed_transcript("TestOrg", "proj", "lead", rows).expect("the transcript seeds");
        fleet.install_agent("TestOrg", "proj", "lead");
        let seat = forge_primitives::SessionSlot::lead("TestOrg", "proj");
        let surface = fleet.surface();
        let cwd = surface.roster().cwd_for(&seat).expect("the seat has a directory");
        settled(&surface.conversation(&seat, &cwd).messages)
    }

    /// The ending the fold read for `call`.
    fn ending_for<'a>(rendered: &'a Rendered, call: &str) -> &'a TaskEnding {
        rendered.endings.get(call).unwrap_or_else(|| panic!("an ending naming {call}"))
    }

    /// A transcript's own messages, as the read hands them to the fold BEFORE
    /// a view's normalisation: `session_history` is the read the spawn and the
    /// surface perform, and the surface's rewrite of the carriers is one
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
        let rendered = folded(crate::fixtures::SAME_TURN_USER_ROW);

        assert!(
            rendered.turns.is_empty(),
            "a notice the CLI persisted as a user row opens no turn of its own",
        );
    }

    /// An ending ends the call it names, carrying the harness's own sentence.
    ///
    /// Three slices, one property each time: the text carrier and the
    /// attachment carrier, one where a turn of its own sits between the notice
    /// and its call, and one where the call is a dispatched agent - 699 of
    /// this machine's 2,194 paired endings name an `Agent` rather than a
    /// backgrounded command.
    ///
    /// The launch's own result says the task is running and is not an error,
    /// so a fold that settled the call on it would read a failed or killed
    /// task as completed - which is why the notice's own word is what the
    /// ending carries.
    #[test]
    fn an_ending_ends_the_call_it_names() {
        for (rows, call, status, said) in [
            (
                crate::fixtures::SAME_TURN_USER_ROW,
                "call_da4c7ee117d14d48ba99e036",
                ToolCallStatus::Failed,
                "Background command \"Watch the account-lifecycle CI run\" failed with exit code 1",
            ),
            (
                crate::fixtures::CROSS_TURN_ATTACHMENT,
                "call_17c05f64497746b0ac450728",
                ToolCallStatus::Failed,
                "Background command \"Restart the harness with a long terminate window\" failed with exit code 100",
            ),
            (
                crate::fixtures::KILLED_AGENT,
                "toolu_013WM27Lt98mvnUq573pXxYD",
                ToolCallStatus::Killed,
                "Agent \"Run slow counting loop\" was stopped by user",
            ),
        ] {
            let rendered = folded(rows);
            let ending = ending_for(&rendered, call);

            assert_eq!(ending.status, Some(status), "the notice's own word ends the call");
            assert_eq!(ending.summary, said, "and what the harness said rides it");
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
        let rendered = folded(crate::fixtures::DUPLICATE_ENDINGS);
        let ending = ending_for(&rendered, "call_370cb30e2834491daf5bb4e2");

        assert_eq!(ending.status, Some(ToolCallStatus::Killed), "the second notice's word");
        assert_eq!(
            ending.summary,
            "Background shell command didn't finish before the previous session ended",
            "and the sentence it carried",
        );
        assert!(
            !ending.text.contains("completed (exit code 0)"),
            "not the first notice's word: {}",
            ending.text,
        );
    }

    /// The fold reads the raw carrier too - a message list taken straight off
    /// a transcript, before any view's normalisation.
    ///
    /// **A guard for the fold's own contract, not dead weight.** `render` is
    /// public and takes the CLI's own messages; the surface's rewrite of the
    /// carriers into one block is one caller's step, not part of the read. A
    /// fold that read only the normalised shape would leave the second,
    /// quieter contract - a caller handing it a transcript - drawing the raw
    /// XML as the reader's own turn.
    #[test]
    fn a_raw_transcripts_notice_is_read() {
        let rendered = settled(&read_raw(crate::fixtures::SAME_TURN_USER_ROW));

        assert!(rendered.turns.is_empty(), "the row the CLI wrote opens no turn of its own");
        assert_eq!(
            ending_for(&rendered, "call_da4c7ee117d14d48ba99e036").status,
            Some(ToolCallStatus::Failed),
            "and the ending it carries still names the call it ends",
        );
    }

    /// How an ending's own word reads as a status.
    ///
    /// **The rule worth reading before this map moves.** An unrecognised word
    /// is no status at all rather than `Pending`: a task whose ending forge
    /// cannot read is one it does not know has ended, and pending is a claim
    /// that it did not stop. How the word meets the status a call's own result
    /// already gave it is the client's fold, over the same wire word.
    #[test]
    fn an_endings_word_reads_as_a_status() {
        let launch = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_ending".to_owned(),
            name: "Bash".to_owned(),
            input: serde_json::json!({"command": "just check"}),
            extras: serde_json::Map::new(),
        }]);
        let failed = tool_result("toolu_ending", true);

        for (word, expected) in
            [("reticulating", None), ("completed", Some(ToolCallStatus::Completed))]
        {
            let rendered = settled(&[
                launch.clone(),
                failed.clone(),
                notice("toolu_ending", word, "what the harness said"),
            ]);
            assert_eq!(
                ending_for(&rendered, "toolu_ending").status,
                expected,
                "the ending's word is {word}",
            );
        }
    }

    /// A sub-agent's frames are not the chat's. A view suppresses
    /// them - dropping a parented frame's text and thinking and
    /// scoping its calls to the SUBAGENTS inspector - and the fold says the
    /// same in one line. The control rides in the same test: the same frame
    /// without a parent id IS the conversation, so a fold that dropped every
    /// frame could not pass this.
    #[test]
    fn a_sub_agents_frames_are_not_the_chat() {
        let prompt = || {
            user(vec![ContentBlock::Text {
                text: "run the tests".to_owned(),
                extras: serde_json::Map::new(),
            }])
        };

        assert!(
            settled(&[dispatched(prompt())]).turns.is_empty(),
            "a sub-agent's prompt opens no turn in the parent's chat",
        );
        assert_eq!(
            settled(&[prompt()]).turns.len(),
            1,
            "and the same frame is a turn when the agent is the session's own",
        );

        // An empty id names no dispatch, and a view reads the field
        // the same way: a frame carrying one is the session's own.
        let mut blank = prompt();
        if let Message::User { parent_tool_use_id, .. } = &mut blank {
            *parent_tool_use_id = Some(String::new());
        }
        assert_eq!(settled(&[blank]).turns.len(), 1, "an empty id is not a dispatch");

        // A parent that names nothing the session dispatched: the guard
        // reads the field rather than looking the id up, and the wire has
        // frames parented to a uuid with no dispatch behind it at all.
        let mut stray = prompt();
        if let Message::User { parent_tool_use_id, .. } = &mut stray {
            *parent_tool_use_id = Some("8f3c1d20-0a4b-4c6e-9d21-77e5a0b3c914".to_owned());
        }
        assert!(
            settled(&[stray]).turns.is_empty(),
            "a parent that names no dispatch still suppresses the frame",
        );
    }

    /// Peer traffic is not the reader's own turn on either side: an envelope
    /// that arrived as the user turn's prose, and a call the session made.
    /// Folded as a turn, the page would show the protocol instead of the
    /// message.
    #[test]
    fn peer_traffic_opens_no_turn() {
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

        assert!(settled(&[arrived]).turns.is_empty(), "the envelope that arrived opens no turn");
        assert!(
            settled(&[sent]).turns.is_empty(),
            "and the call the session made opens none either",
        );
    }
}

/// One policy for the text a `queued_command` block carries: the fold
/// above reads it, and so does a view's own message walker.
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
mod turns {
    use forge_primitives::{
        AssistantEnvelope, ContentBlock, Message, StopReason, Usage, UserEnvelope,
    };

    use super::{Rendered, settled};

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

    /// What each turn is named, in the order the fold opens them.
    fn keys(rendered: &Rendered) -> Vec<Option<String>> {
        rendered.turns.iter().map(|span| span.key.clone()).collect()
    }

    /// The same frame with its clock stripped: a transcript row the CLI wrote
    /// before it stamped one, which the fold has to place no time on.
    fn clockless(mut message: Message) -> Message {
        match &mut message {
            Message::User { timestamp, .. } | Message::Assistant { timestamp, .. } => {
                *timestamp = None;
            }
            other => panic!("no clock to strip: {other:?}"),
        }
        message
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

    /// A turn is named for the turn it is rather than for its place: the name
    /// is the instant the turn's own first row carried, so a turn inserted
    /// above another leaves that one named the same. A count of the turns
    /// before it renames every one after the insertion.
    #[test]
    fn a_turn_is_named_for_its_own_opening_instant_and_not_for_its_place() {
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
            keys(&settled(&first)),
            [
                Some("2026-04-22T04:15:27.000Z".to_owned()),
                Some("2026-04-22T05:00:00.000Z".to_owned()),
            ],
            "each turn is named for its own opening instant",
        );
        assert_eq!(
            keys(&settled(&inserted)).last(),
            Some(&Some("2026-04-22T05:00:00.000Z".to_owned())),
            "and an inserted turn leaves the turn after it named the same",
        );
    }

    /// A turn the wire settled is named the way a turn read back is: the
    /// instant its own first row carried. The frame's id is the fallback for
    /// a turn the fold placed no clock on, and naming the turn for the frame
    /// instead would rename every one of them across a reload.
    #[test]
    fn a_wire_settled_turn_is_named_for_its_opening_instant() {
        let messages = [
            user_at("ship it", "2026-04-22T04:15:27.000Z"),
            assistant_at("Shipped.", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
            result_frame(Some("result-1")),
        ];

        assert_eq!(
            keys(&settled(&messages)),
            [Some("2026-04-22T04:15:27.000Z".to_owned())],
            "the turn's own instant, not the id of the frame that reported it",
        );
    }

    /// A turn the fold can place no clock on and whose frame carried no id
    /// is named by nothing at all. A turn a view does not remember open is
    /// the safe answer; a name it shares with a neighbour is the one that
    /// opens the wrong body.
    #[test]
    fn a_turn_nothing_can_name_carries_no_name() {
        let messages = [
            clockless(user_at("ship it", "2026-04-22T04:15:27.000Z")),
            clockless(assistant_at(
                "Shipped.",
                "2026-04-22T04:18:08.000Z",
                StopReason::EndTurn,
                None,
            )),
            result_frame(None),
        ];

        assert_eq!(
            keys(&settled(&messages)),
            [None],
            "no clock and no frame id, so no name rather than another turn's",
        );
    }

    /// A turn the fold placed no clock on takes the id of the frame that
    /// reported it. The arm is reachable rather than belt-and-braces: the
    /// baselines carry result frames with no timestamp, so a fold can meet
    /// one with nothing in the trace to name it by.
    #[test]
    fn a_turn_with_no_clock_takes_the_frame_id() {
        let messages = [
            clockless(user_at("ship it", "2026-04-22T04:15:27.000Z")),
            clockless(assistant_at(
                "Shipped.",
                "2026-04-22T04:18:08.000Z",
                StopReason::EndTurn,
                None,
            )),
            result_frame(Some("result-1")),
        ];

        assert_eq!(
            keys(&settled(&messages)),
            [Some("result-1".to_owned())],
            "nothing placed a clock on the turn, so the frame's own id names it",
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
            keys(&settled(&messages)),
            [
                Some("2026-04-22T04:15:27.000Z".to_owned()),
                Some("2026-04-22T04:15:27.000Z#2".to_owned()),
            ],
            "the second turn is named its own rather than the first's",
        );
    }

    /// A transcript holds no result frame, so a turn read from one is named
    /// from its own rows or not at all: the opening instant of the first row
    /// the turn wrote.
    #[test]
    fn a_turn_read_from_a_transcript_carries_its_own_name() {
        let messages = [
            user_at("make the call tree the default", "2026-04-22T04:15:27.000Z"),
            assistant_at("Done.", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
        ];

        assert_eq!(
            keys(&settled(&messages)),
            [Some("2026-04-22T04:15:27.000Z".to_owned())],
            "with no result frame to name it, the turn's own opening instant does",
        );
    }

    /// The wire's own result closes a turn, and the fold must not open a
    /// second one for it: the turn it reported is the turn it names.
    #[test]
    fn a_turn_the_wire_settled_is_named_once() {
        let messages = [
            user_at("ship it", "2026-04-22T04:15:27.000Z"),
            assistant_at("Shipped.", "2026-04-22T04:18:08.000Z", StopReason::EndTurn, None),
            result_frame(None),
        ];
        let rendered = settled(&messages);

        assert_eq!(rendered.turns.len(), 1, "the result closes the turn rather than opening one");
        assert_eq!(
            keys(&rendered),
            [Some("2026-04-22T04:15:27.000Z".to_owned())],
            "and the one turn is named from its own rows",
        );
    }

    /// A turn that handed back for a tool call has not ended, so the fold
    /// names it by nothing while it is the last thing in the conversation:
    /// the view draws the live one while it runs.
    #[test]
    fn a_turn_still_calling_tools_carries_no_name() {
        let messages = [
            user_at("read the file", "2026-04-22T04:15:27.000Z"),
            use_at("toolu_1", "2026-04-22T04:15:29.000Z"),
        ];

        assert_eq!(
            keys(&settled(&messages)),
            [None],
            "nothing ended this turn, so nothing named it"
        );
    }

    /// A turn the transcript has already closed is named whatever its last
    /// frame stopped for: the rows after it are the CLI's own write that the
    /// turn is over, which is what a turn ending on a tool call and reopened
    /// by a row nobody typed looks like.
    #[test]
    fn a_turn_the_transcript_closed_carries_its_name() {
        let messages = [
            user_at("run the skill", "2026-04-22T04:15:27.000Z"),
            use_at("toolu_1", "2026-04-22T04:15:29.000Z"),
            user_at("Base directory for this skill: /tmp/skill", "2026-04-22T04:15:31.000Z"),
        ];

        assert_eq!(
            keys(&settled(&messages)),
            [Some("2026-04-22T04:15:27.000Z".to_owned()), None],
            "the closed turn is named from its own rows, and the row that closed it opens a turn \
             with nothing to name it",
        );
    }

    /// A pair of row clocks that runs backwards or does not parse gives no
    /// span, and a turn with no usable pair is named by nothing: a replayed
    /// row carries the stamp it was written with, so the backwards pair is
    /// real.
    #[test]
    fn a_turn_whose_clocks_give_no_span_carries_no_name() {
        let backwards = [
            user_at("count it", "2026-04-22T04:18:08.000Z"),
            assistant_at("done", "2026-04-22T04:15:27.000Z", StopReason::EndTurn, None),
        ];
        assert_eq!(keys(&settled(&backwards)), [None], "a span that runs backwards is not one");

        let unreadable = [
            user_at("count it", "2026-04-22T04:15:27.000Z"),
            assistant_at("done", "nope", StopReason::EndTurn, None),
        ];
        assert_eq!(
            keys(&settled(&unreadable)),
            [None],
            "and neither is a clock that does not parse",
        );
    }

    /// A turn whose frames carried no clock is named by nothing: a name is
    /// not a place to invent a time from, and the fallback frame id is only
    /// the frame's own.
    #[test]
    fn a_turn_with_no_row_clocks_carries_no_name() {
        let messages = [
            clockless(user_at("no clocks here", "2026-04-22T04:15:27.000Z")),
            clockless(assistant_at(
                "nor here",
                "2026-04-22T04:15:31.000Z",
                StopReason::EndTurn,
                None,
            )),
        ];

        assert_eq!(
            keys(&settled(&messages)),
            [None],
            "a name invented here would be one the turn does not have",
        );
    }

    /// A turn the next prompt closed is named while a later one runs: the
    /// trailing turn carries no name until its own report arrives.
    #[test]
    fn a_closed_turn_is_named_while_a_later_one_runs() {
        let messages = [
            user_at("first", "2026-04-22T04:15:27.000Z"),
            assistant_at("one", "2026-04-22T04:16:00.000Z", StopReason::EndTurn, None),
            user_at("second", "2026-04-22T04:20:00.000Z"),
        ];
        assert_eq!(
            keys(&settled(&messages)),
            [Some("2026-04-22T04:15:27.000Z".to_owned()), None],
            "the first turn's name survives a running second",
        );
    }

    /// Two turns each carry their own name, in the order they ran.
    #[test]
    fn each_turn_carries_its_own_name() {
        let messages = [
            user_at("first", "2026-04-22T04:15:27.000Z"),
            assistant_at("one", "2026-04-22T04:16:00.000Z", StopReason::EndTurn, None),
            user_at("second", "2026-04-22T04:20:00.000Z"),
            assistant_at("two", "2026-04-22T04:21:30.000Z", StopReason::EndTurn, None),
        ];

        assert_eq!(
            keys(&settled(&messages)),
            [
                Some("2026-04-22T04:15:27.000Z".to_owned()),
                Some("2026-04-22T04:20:00.000Z".to_owned()),
            ],
            "each turn's own opening instant, not the other's",
        );
    }
}
