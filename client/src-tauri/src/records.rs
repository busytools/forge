//! The seat records, as the catch-up a returning page is handed.
//!
//! **Rust keeps a copy of what the page folds.** The page is still told
//! every frame as it arrives - the bridge forwards the stream, and
//! `client/src/session/apply.ts`'s fold still runs there - so what is HERE
//! is the one thing a parked page cannot rebuild for itself: the record as
//! of the moment it looked away, folded the same way the page's own fold
//! would have folded it. A heartbeat that finds the page was away hands it
//! back (see `bridge.rs`'s reconcile).
//!
//! **The record's shape is the server's own snapshot** (`transport/wire.rs`'
//! `SessionWire`), held as JSON. Two consequences keep this port small and
//! faithful at once:
//!
//! - Where the TS handler merely REPLACES a field with the payload's value
//!   (work, git, monitors, tasks, the catalogues), this stores the value
//!   verbatim - the page's own read (`session/wire.ts`'s `sessionFrom`)
//!   narrows it the same way it narrows a snapshot, so there is one narrowing
//!   in the system, on the read.
//! - Where the TS handler FOLDS - the asks queue, the prompt pile, the turn
//!   append, the in-flight header - this folds too, because those are state
//!   rather than a view's shape. One clause cannot be exact: the turn append
//!   asks the chat fold whether a frame draws, and the fold stays TS by
//!   decision, so `opens_a_turn` tests the cases the fold's own comments
//!   name and says so.
//!
//! **A take is not here, deliberately**: it belongs to the connection that
//! started it (the wire's own composer says "no take and no notice"), so the
//! page's own fold is the only place one lives and the `dictate_*` frames
//! touch no record.
//!
//! Every field name below was taken off `apply.ts` (which itself took them
//! off the Rust side).

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::socket::Socket;

/// The lifecycle states that settle a queued-prompt row, as `apply.ts` reads
/// them.
const SETTLED_STATES: [&str; 5] = ["started", "completed", "cancelled", "discarded", "refused"];

/// `PermissionMode`, as its own serde writes it: camelCase, not snake.
const MODES: [&str; 6] = ["default", "acceptEdits", "plan", "dontAsk", "auto", "bypassPermissions"];

/// `EffortLevel`, as the core's own enum serialises.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The updates that replace the seat's whole record rather than patching it:
/// a new occupant under the slot, or the first connect. Answering them is a
/// fresh read, which the app half performs (a subscribe's own snapshot).
const REPLACES: [&str; 4] = ["spawning", "connected", "history_replayed", "session_replaced"];

/// One subject's held state.
pub enum Held {
    /// The record, as the server's snapshot shape carries it.
    Ready(Value),
    /// A subscribe the core refused, with its words.
    Refused(String),
}

/// Every subject this client holds: seats, the home, usage, the models page.
///
/// **The fold is for the reconcile, and nothing else rides on it.** The page
/// is told about every frame as it arrives (the bridge forwards the stream),
/// so a record here is not a notice - it is what a parked page is handed
/// when it comes back. That is why nothing on this side re-reads the home or
/// replays a pile: the page's own stores and folds do that off the frames,
/// and the one caller of this fold is the catch-up.
pub struct Records {
    subjects: HashMap<String, Held>,
}

impl Default for Records {
    fn default() -> Self {
        Self::new()
    }
}

impl Records {
    pub fn new() -> Self {
        Self { subjects: HashMap::new() }
    }

    /// A snapshot for a subject: the read's own answer, and the point the
    /// updates patch from.
    pub fn snapshot(&mut self, subject: &Value, data: Value) {
        let key = subject_key(subject);
        if key.is_empty() {
            return;
        }
        self.subjects.insert(key, Held::Ready(data));
    }

    /// A subscribe the core refused: the subject answers "refused" so a page
    /// draws the reason rather than an empty seat.
    pub fn refuse(&mut self, key: &str, why: &str) {
        if key.is_empty() {
            return;
        }
        self.subjects.insert(key.to_owned(), Held::Refused(why.to_owned()));
    }

    /// The record a reader wants, or the refusal's words, or None for a
    /// subject this client does not hold. `None` is a state too - a page
    /// draws "not read yet" rather than an empty seat.
    pub fn read(&self, key: &str) -> Option<Result<&Value, &str>> {
        self.subjects.get(key).map(|held| match held {
            Held::Ready(value) => Ok(value),
            Held::Refused(why) => Err(why.as_str()),
        })
    }

    /// Let a subject go: the last page reader has left, and nothing here is
    /// left to answer. The wire subscription is the caller's own to give
    /// back, and a later subscribe reads afresh.
    pub fn release(&mut self, key: &str) {
        self.subjects.remove(key);
    }

    /// Fold one `update` frame's variant into its seat's record. Answers the
    /// seats a REPLACES frame took a new occupant for: those are asked for
    /// again, because what such a frame carries is not a record.
    pub fn apply(&mut self, update: &Value) -> Vec<String> {
        let Some((name, payload)) = variant_of(update) else {
            return Vec::new();
        };
        // A seat is addressed by its slot; a keyless update belongs to no
        // record this fold keeps (the page's own home fold reads the frames
        // directly), and a seat this client never subscribed to is not this
        // client's to keep.
        let Some(key) = seat_key_of(update) else {
            return Vec::new();
        };
        let Some(Held::Ready(record)) = self.subjects.get_mut(&key) else {
            return Vec::new();
        };
        if REPLACES.contains(&name) {
            // **A new occupant under the slot, or the first connect.** What
            // the frame carries is not a record - the seat's own session facts
            // and the folded transcript - so only a fresh read answers it. The
            // record held now stays readable until that read lands, which is
            // the TS store's `replaceWanted` - and its frames-counter guard
            // against a stale answer has no work here: a read and the frames
            // share this connection's own order, so the answer can never be
            // older than what is already held.
            return vec![key];
        }
        seat_update(record, name, payload);
        Vec::new()
    }

    /// The socket is gone: a take goes with it.
    ///
    /// The core drops a take whose reader went away, and no frame says so - a
    /// record left drawing a recording that is over is the live store's
    /// `status` arm (`session/live.ts`, #1880). Everything else stands where
    /// it was: the reconnect re-asks every held subscription and the answers
    /// replace the records whole.
    pub fn dropped(&mut self) {
        for held in self.subjects.values_mut() {
            let Held::Ready(record) = held else { continue };
            let Some(composer) = record.get_mut("composer").and_then(Value::as_object_mut) else {
                continue;
            };
            if composer.get("take").is_some_and(|take| !take.is_null()) {
                composer.insert("take".to_owned(), Value::Null);
            }
        }
    }
}

/// The variant's own name, for a caller that routes on it.
pub(crate) fn variant_name(update: &Value) -> Option<&str> {
    variant_of(update).map(|(name, _)| name)
}

/// A unit variant's payload, so `variant_of` hands one shape for both.
static NO_PAYLOAD: Value = Value::Null;

/// The variant's name and payload, as the core's externally tagged enums
/// cross (`{"chat_appended": {...}}`, or a bare string for a unit variant).
fn variant_of(update: &Value) -> Option<(&str, &Value)> {
    match update {
        Value::String(name) => Some((name.as_str(), &NO_PAYLOAD)),
        Value::Object(map) => map.iter().next().map(|(name, payload)| (name.as_str(), payload)),
        _ => None,
    }
}

/// The seat an update names, through its payload's own `key` - the one place
/// that reaches into a variant's fields, exactly as `protocol.ts`'s
/// `slotOf` does.
fn seat_key_of(update: &Value) -> Option<String> {
    let (_, payload) = variant_of(update)?;
    let key = payload.get("key")?;
    let (Some(org), Some(project), Some(label)) = (
        key.get("org").and_then(Value::as_str),
        key.get("project").and_then(Value::as_str),
        key.get("label").and_then(Value::as_str),
    ) else {
        return None;
    };
    Some(format!("session:{org}\u{0}{project}\u{0}{label}"))
}

/// A subject as the TS side keys it, mirroring `protocol.ts`'s `subjectKey`.
pub fn subject_key(subject: &Value) -> String {
    if let Some(s) = subject.as_str() {
        return s.to_owned();
    }
    let Some(session) = subject.get("session") else { return String::new() };
    let (Some(org), Some(project), Some(label)) = (
        session.get("org").and_then(Value::as_str),
        session.get("project").and_then(Value::as_str),
        session.get("label").and_then(Value::as_str),
    ) else {
        return String::new();
    };
    format!("session:{org}\u{0}{project}\u{0}{label}")
}

/// Fold one seat update into the record. `true` when the record really moved -
/// a re-delivered frame must not read as news.
///
/// The arms: a frame the record has nothing to do with leaves it alone
/// (the ignored list and every unknown variant), and a frame that folds
/// state folds it here exactly as `apply.ts` folds it there.
fn seat_update(record: &mut Value, name: &str, payload: &Value) -> bool {
    let Some(record) = record.as_object_mut() else { return false };
    match name {
        "connection_failed" => {
            // The CLI the rows were written to is gone, so the pile goes. The
            // ending a foot draws is this view's own observation and is NOT
            // written here: the page holds that line from the same frame.
            if queue_of(record).is_empty() {
                return false;
            }
            set_queue(record, Vec::new());
            true
        }
        "chat_appended" => {
            let Some(msg) = payload.get("msg") else { return false };
            let mut moved = false;
            if let Some(conversation) = record.get_mut("conversation")
                && let Some(turns) = conversation.get_mut("turns")
            {
                moved |= append_frame(turns, msg);
            }
            if let Some(composer) = record.get_mut("composer") {
                moved |= compacting_of(composer, msg);
            }
            if let Some(header) = record.get_mut("header") {
                moved |= header_from(header, msg);
            }
            moved
        }
        "hook_observation" => {
            let Some(header) = record.get_mut("header").and_then(Value::as_object_mut) else {
                return false;
            };
            let mut moved = false;
            if let Some(mode) = known(payload.get("permission_mode"), &MODES)
                && header.get("permission_mode") != Some(&Value::String(mode.clone()))
            {
                header.insert("permission_mode".to_owned(), Value::String(mode));
                moved = true;
            }
            if let Some(effort) = known(payload.get("effort"), &EFFORTS)
                && header.get("effort") != Some(&Value::String(effort.clone()))
            {
                header.insert("effort".to_owned(), Value::String(effort));
                moved = true;
            }
            moved
        }
        "context_usage_snapshot" => {
            // Both halves are `Option` on the update, so an absent key is a
            // fact about the session rather than a field to keep the old
            // value for; a payload carrying neither is the other case and
            // leaves the record as it was. The record's `percent` against the
            // update's `percentage`.
            if payload.get("percentage").is_none() && payload.get("max_tokens").is_none() {
                return false;
            }
            let Some(context) = record
                .get_mut("header")
                .and_then(Value::as_object_mut)
                .and_then(|header| header.get_mut("context"))
                .and_then(Value::as_object_mut)
            else {
                return false;
            };
            let mut moved = false;
            if let Some(percentage) = payload.get("percentage") {
                moved |= set(context, "percent", number_or_null(percentage));
            }
            if let Some(max) = payload.get("max_tokens") {
                moved |= set(context, "max_tokens", number_or_null(max));
            }
            moved
        }
        "mcp_snapshot" => {
            // The payload is the snapshot's own `mcp` shape, held verbatim
            // for the page's read to narrow; a frame naming no snapshot
            // leaves the record as it was.
            if payload.is_null() {
                return false;
            }
            set(record, "mcp", payload.clone())
        }
        "work_changed" => {
            let mut moved = false;
            for field in ["work", "git", "pr", "closes"] {
                moved |= set(record, field, payload.get(field).cloned().unwrap_or(Value::Null));
            }
            moved
        }
        "monitors_changed" => set(record, "monitors", array(payload.get("monitors"))),
        "background_tasks_changed" => set(record, "background_tasks", array(payload.get("tasks"))),
        "processes_changed" => {
            if !payload.get("snapshot").is_some_and(Value::is_array) {
                return false;
            }
            record.insert("processes".to_owned(), payload["snapshot"].clone());
            true
        }
        "slash_commands_changed" => set(record, "slash_commands", array(payload.get("commands"))),
        "subagents_changed" => set(record, "subagents", array(payload.get("subagents"))),
        "subagent_cards_changed" => set(record, "subagent_instances", array(payload.get("cards"))),
        "dispatches_changed" => match payload.get("has_dispatches") {
            Some(Value::Bool(flag)) => set(record, "has_dispatches", *flag),
            _ => false,
        },
        "file_index_changed" => match payload.get("index") {
            Some(index) => set(record, "file_index", index.clone()),
            None => false,
        },
        "permission_request" => parked(record, "permission", payload.get("request")),
        "question_request" => parked(record, "question", payload.get("request")),
        "slack_post_pending" => parked(record, "slack_draft", payload.get("draft")),
        "browser_hand_off_pending" => parked(record, "browser_hand_off", payload.get("handoff")),
        "prompt_queued" => {
            let (Some(uuid), Some(words)) = (
                payload.get("uuid").and_then(Value::as_str),
                payload.get("text").and_then(Value::as_str),
            ) else {
                return false;
            };
            let mut queue = queue_of(record);
            if queue.iter().any(|row| row.get("uuid").and_then(Value::as_str) == Some(uuid)) {
                return false;
            }
            queue.push(json!({
                "uuid": uuid,
                "source": payload.get("source").and_then(Value::as_str).unwrap_or("forge"),
                "text": words,
            }));
            set_queue(record, queue);
            true
        }
        "prompt_lifecycle" => {
            let (Some(uuid), Some(state)) = (
                payload.get("uuid").and_then(Value::as_str),
                payload.get("state").and_then(Value::as_str),
            ) else {
                return false;
            };
            // Only a state this build knows settles a row: a word the CLI
            // adds later leaves it standing, because dropping on a parse miss
            // is the one failure a reader cannot see.
            if !SETTLED_STATES.contains(&state) {
                return false;
            }
            let queue = queue_of(record);
            if !queue.iter().any(|row| row.get("uuid").and_then(Value::as_str) == Some(uuid)) {
                return false;
            }
            let remaining: Vec<Value> = queue
                .into_iter()
                .filter(|row| row.get("uuid").and_then(Value::as_str) != Some(uuid))
                .collect();
            // The ending two states leave with is the page's own line from
            // this same frame; a read carries what waits, never what left.
            set_queue(record, remaining);
            true
        }
        "prompt_cancel_resolved" => {
            let Some(uuid) = payload.get("uuid").and_then(Value::as_str) else { return false };
            if payload.get("cancelled") != Some(&Value::Bool(true)) {
                return false;
            }
            let queue = queue_of(record);
            let remaining: Vec<Value> = queue
                .iter()
                .filter(|row| row.get("uuid").and_then(Value::as_str) != Some(uuid))
                .cloned()
                .collect();
            if remaining.len() == queue.len() {
                return false;
            }
            set_queue(record, remaining);
            true
        }
        "pending_interaction_resolved" => {
            let Some(tool_id) = payload.get("tool_id").and_then(Value::as_str) else {
                return false;
            };
            // **The round as well as the call.** A batch reuses one tool id
            // and advances the question index, and the next round's request
            // can land before this round's resolution - clearing on the id
            // alone dropped the ask that had just parked, so every round after
            // the first lost its opening question (Ved's live find; #1717). A
            // frame naming no round is an older core, and the id is all it can
            // mean there; a permission's frames never name one.
            let index = payload.get("question_index").and_then(Value::as_u64);
            let asks = array(record.get("pending_asks"));
            let remaining: Vec<Value> = asks
                .iter()
                .filter(|ask| {
                    if ask_tool_id(ask).as_deref() != Some(tool_id) {
                        return true;
                    }
                    match (index, ask_index(ask)) {
                        (Some(wanted), Some(parked)) => wanted != parked,
                        _ => false,
                    }
                })
                .cloned()
                .collect();
            if remaining.len() == asks.len() {
                return false;
            }
            record.insert("pending_asks".to_owned(), Value::Array(remaining));
            true
        }
        "slack_draft_resolved" => clear_ask(record, payload.get("id"), "slack"),
        "browser_hand_off_resolved" => clear_ask(record, payload.get("id"), "handoff"),
        "auth_required" => {
            let method = payload.get("method_name").and_then(Value::as_str);
            let description = payload.get("method_description").and_then(Value::as_str);
            if method.is_none() && description.is_none() {
                return false;
            }
            let Some(composer) = record.get_mut("composer").and_then(Value::as_object_mut) else {
                return false;
            };
            composer.insert(
                "sign_in".to_owned(),
                json!({
                    "method_name": method.unwrap_or(""),
                    "method_description": description.unwrap_or(""),
                }),
            );
            true
        }
        // **A take is never folded here.** It belongs to the connection that
        // started it - the wire's own composer says so ("no take and no
        // notice") - so a record has no place for one and the page's own
        // fold, off the forwarded frames, is the only place one lives. The
        // `dictate_*` variants therefore touch nothing on this side.
        //
        // The turn's own end, for whichever of the three ways the core says
        // it. All three settle the turn; only `turn_error` reaches the wire
        // today.
        "turn_complete" | "turn_cancelled" | "turn_error" => settled(record),
        _ => false,
    }
}

/// A field set only when it differs, so a re-delivered frame is not news.
fn set(record: &mut Map<String, Value>, field: &str, value: impl Into<Value>) -> bool {
    let value: Value = value.into();
    if record.get(field) == Some(&value) {
        return false;
    }
    record.insert(field.to_owned(), value);
    true
}

fn array(value: Option<&Value>) -> Vec<Value> {
    value.and_then(Value::as_array).cloned().unwrap_or_default()
}

/// The pile as the record's own shape carries it: `state.queue`, oldest
/// first. **The read path for a page re-read is that field alone**, so the
/// fold writes here and never at the record's top level, where nothing looks
/// (the wire nests it under `state`, and `sessionFrom` reads only there).
fn queue_of(record: &Map<String, Value>) -> Vec<Value> {
    record
        .get("state")
        .and_then(Value::as_object)
        .and_then(|state| state.get("queue"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn set_queue(record: &mut Map<String, Value>, rows: Vec<Value>) {
    let state = record.entry("state".to_owned()).or_insert_with(|| json!({}));
    if let Some(state) = state.as_object_mut() {
        state.insert("queue".to_owned(), Value::Array(rows));
    }
}

/// A finite number, or null for anything else - the update carries both
/// context halves as `Option`, and a present-but-unreadable one is a fact the
/// record keeps as null rather than the old value.
fn number_or_null(value: &Value) -> Value {
    if value.is_number() { value.clone() } else { Value::Null }
}

/// One of `known`, when the payload named one this build knows.
fn known(value: Option<&Value>, known: &[&str]) -> Option<String> {
    let value = value?.as_str()?;
    known.contains(&value).then(|| value.to_owned())
}

/// One frame into the turn it belongs to - the record's own arm of the
/// server's fold.
///
/// **This differs from the chat's fold deliberately, and `apply.ts` names
/// where**: with no turn held, every frame opens one here; mid-turn, a
/// queued prompt opens its own turn here where the chat joins it to the row
/// above. The only frames that can open a turn in this record are a person's
/// own words (`user`), and only when they carry something that draws - which
/// is what keeps a tool result riding its call's turn rather than opening
/// one.
fn append_frame(turns: &mut Value, message: &Value) -> bool {
    let Some(turns) = turns.as_array_mut() else { return false };
    let opens = match turns.last() {
        None => true,
        Some(_) => !is_system(message) && opens_a_turn(message),
    };
    if opens {
        turns.push(json!({ "key": null, "messages": [message] }));
        return true;
    }
    if let Some(last) = turns.last_mut()
        && let Some(messages) = last.get_mut("messages").and_then(Value::as_array_mut)
    {
        messages.push(message.clone());
        return true;
    }
    false
}

fn is_system(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("system")
}

/// Whether a frame opens a turn of its own rather than joining the live one:
/// what a person said, and only where the frame draws.
///
/// **The draw test is an approximation of the chat fold, which stays TS by
/// decision** - `apply.ts` asks `fold([message]).length > 0` - and it carries
/// the cases the fold's own comments name: a call's result arrives in a user
/// frame and draws nothing on its own (which is what holds a call and the
/// frames that update it in one turn), the launch terminal's local-command
/// family is a decided ignore that draws nothing at all, a dispatched agent's
/// frames are not this conversation (`units.ts`'s `isDispatched` - the wire
/// spells no dispatch as null), and a frame whose content is not an array of
/// blocks has none to read. Everything else a person's frame carries - words,
/// a picture, a notice the harness sends, a frame a later CLI adds - draws.
fn opens_a_turn(message: &Value) -> bool {
    if message.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    if message
        .get("parent_tool_use_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty())
    {
        return false;
    }
    match message.get("message").and_then(|msg| msg.get("content")) {
        Some(Value::Array(blocks)) => blocks.iter().any(draws),
        _ => false,
    }
}

/// Whether one content block draws anything in the chat fold: the two arms
/// above, at the block's own grain.
fn draws(block: &Value) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some("tool_result") => false,
        Some("text") => {
            block.get("text").and_then(Value::as_str).is_none_or(|text| !is_local_command(text))
        }
        _ => true,
    }
}

/// The launch terminal's local-command family, which the chat filters as a
/// decided ignore (`units.ts`'s `isLocalCommand`, the same four heads).
fn is_local_command(text: &str) -> bool {
    let held = text.trim_start();
    held.starts_with("<local-command-caveat>")
        || held.starts_with("<local-command-stdout>")
        || held.starts_with("<command-name>")
        || held.starts_with("<command-message>")
}

/// What a frame says about the header: the turn, the mode and the model.
/// `init` is the frame a turn opens with, and the CLI re-fires it at the head
/// of every one.
fn header_from(header: &mut Value, message: &Value) -> bool {
    let Some(header) = header.as_object_mut() else { return false };
    let mut moved = false;
    let held = header.get("turn_in_flight").and_then(Value::as_bool).unwrap_or(false);
    let turn = in_flight_of(held, message);
    if turn != held {
        header.insert("turn_in_flight".to_owned(), json!(turn));
        moved = true;
    }
    let opens = message.get("type").and_then(Value::as_str) == Some("system")
        && message.get("subtype").and_then(Value::as_str) == Some("init");
    if opens {
        if let Some(mode) = known(message.get("permissionMode"), &MODES)
            && header.get("permission_mode") != Some(&Value::String(mode.clone()))
        {
            header.insert("permission_mode".to_owned(), Value::String(mode));
            moved = true;
        }
        if let Some(id) = message.get("model").and_then(Value::as_str).map(str::trim)
            && !id.is_empty()
        {
            let current =
                header.get("model").and_then(|m| m.get("resolved_id")).and_then(Value::as_str);
            if current != Some(id) {
                // A frame naming the model the session is already on changes
                // nothing (the server's own `reconcile_model_from_init`).
                let listed =
                    header.get("available_models").and_then(Value::as_array).and_then(|rows| {
                        rows.iter().find(|row| row.get("id").and_then(Value::as_str) == Some(id))
                    });
                let display = listed
                    .and_then(|row| row.get("display_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                header.insert(
                    "model".to_owned(),
                    json!({ "resolved_id": id, "display_name_long": display }),
                );
                moved = true;
            }
        }
    }
    moved
}

/// What a frame says about a turn being in flight: the rising edge is the
/// frame a turn opens with, the falling edges are its result and the error
/// the CLI gives up with. One rule, read by both folds.
fn in_flight_of(held: bool, message: &Value) -> bool {
    match message.get("type").and_then(Value::as_str) {
        Some("result") | Some("error") => false,
        Some("system") if message.get("subtype").and_then(Value::as_str) == Some("init") => true,
        _ => held,
    }
}

/// The compaction a status frame announces and the null that clears it.
fn compacting_of(composer: &mut Value, message: &Value) -> bool {
    if message.get("type").and_then(Value::as_str) != Some("system")
        || message.get("subtype").and_then(Value::as_str) != Some("status")
    {
        return false;
    }
    let Some(composer) = composer.as_object_mut() else { return false };
    let held = composer.get("compacting").and_then(Value::as_bool).unwrap_or(false);
    match message.get("status") {
        Some(Value::String(status)) if status == "compacting" && !held => {
            composer.insert("compacting".to_owned(), json!(true));
            true
        }
        Some(Value::Null) if held => {
            composer.insert("compacting".to_owned(), json!(false));
            true
        }
        _ => false,
    }
}

/// The record with the turn settled, or unchanged when it already was.
fn settled(record: &mut Map<String, Value>) -> bool {
    let Some(header) = record.get_mut("header").and_then(Value::as_object_mut) else {
        return false;
    };
    if header.get("turn_in_flight").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    header.insert("turn_in_flight".to_owned(), json!(false));
    true
}

/// The prompt a seat is parked on, placed by the rule every hop keeps: a
/// draft leads the queue, and everything else waits oldest first. A frame
/// carrying an ask the record already holds is not a second ask.
fn parked(record: &mut Map<String, Value>, kind: &str, request: Option<&Value>) -> bool {
    let Some(request) = request else { return false };
    let ask = json!({ "kind": kind, "request": request });
    let key = ask_key(&ask);
    let asks = array(record.get("pending_asks"));
    if let Some(key) = &key
        && asks.iter().any(|waiting| ask_key(waiting).as_deref() == Some(key.as_str()))
    {
        return false;
    }
    let leads = kind == "slack_draft" || kind == "browser_hand_off";
    let mut asks = asks;
    if !leads {
        asks.push(ask);
    } else {
        // Behind the leaders already there, ahead of everything else.
        let first_other = asks
            .iter()
            .position(|waiting| {
                let name = waiting.get("kind").and_then(Value::as_str).unwrap_or_default();
                name != "slack_draft" && name != "browser_hand_off"
            })
            .unwrap_or(asks.len());
        asks.insert(first_other, ask);
    }
    record.insert("pending_asks".to_owned(), Value::Array(asks));
    true
}

/// The key an ask is held under: the call, and the round for a question. A
/// draft and a hand-off are each answered by their own id.
fn ask_key(ask: &Value) -> Option<String> {
    let kind = ask.get("kind").and_then(Value::as_str)?;
    let request = ask.get("request").unwrap_or(&Value::Null);
    if kind == "slack_draft" {
        return request.get("id").and_then(Value::as_str).map(|id| format!("slack:{id}"));
    }
    if kind == "browser_hand_off" {
        return request.get("id").and_then(Value::as_str).map(|id| format!("handoff:{id}"));
    }
    let id = ask_tool_id(ask)?;
    Some(if kind == "question" {
        // A question's frames always name a round; one that did not is keyed
        // apart rather than folded onto round 0, which is the TS side's
        // `String(index-or-null)`.
        let round = ask_index(ask).map_or_else(|| "null".to_owned(), |at| at.to_string());
        format!("question:{id}:{round}")
    } else {
        format!("permission:{id}")
    })
}

/// The tool call a parked ask waits on.
fn ask_tool_id(ask: &Value) -> Option<String> {
    ask.get("request")?.get("tool_call")?.get("tool_call_id")?.as_str().map(str::to_owned)
}

/// The round a parked question asks, or None.
fn ask_index(ask: &Value) -> Option<u64> {
    ask.get("request")?.get("question_index")?.as_u64()
}

/// Clear a parked ask by the id its own resolution names.
fn clear_ask(record: &mut Map<String, Value>, id: Option<&Value>, prefix: &str) -> bool {
    let Some(id) = id.and_then(Value::as_str) else { return false };
    let wanted = format!("{prefix}:{id}");
    let asks = array(record.get("pending_asks"));
    let remaining: Vec<Value> = asks
        .iter()
        .filter(|ask| ask_key(ask).as_deref() != Some(wanted.as_str()))
        .cloned()
        .collect();
    if remaining.len() == asks.len() {
        return false;
    }
    record.insert("pending_asks".to_owned(), Value::Array(remaining));
    true
}

/// Ask the core for a subject afresh: the read a REPLACES update needs. The
/// subscription is given up and taken again, which is what makes the core
/// answer with a snapshot rather than a delta. **The declaration is the
/// caller's**: a refresh with flags of its own would escalate a seat watched
/// without them, and the core parks turns on whatever answered.
pub fn refresh(socket: &Socket, subject: &Value, answering: bool, browser: bool) {
    socket.unsubscribe(subject.clone());
    socket.subscribe(subject.clone(), answering, browser);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat() -> Value {
        json!({ "session": { "org": "Busytools", "project": "forge", "label": "lead" } })
    }

    fn record() -> Value {
        json!({
            "header": { "turn_in_flight": false, "context": { "percent": 0.1, "max_tokens": 1000 } },
            "conversation": { "turns": [], "compaction_count": 0 },
            "composer": { "compacting": false },
            "state": { "queue": [] },
            "pending_asks": [],
        })
    }

    fn open() -> Records {
        let mut records = Records::new();
        records.snapshot(&seat(), record());
        records
    }

    fn key() -> &'static str {
        "session:Busytools\u{0}forge\u{0}lead"
    }

    fn held(records: &Records) -> Value {
        records.read(key()).expect("held").expect("ready").clone()
    }

    fn update(payload: Value) -> Value {
        json!({ "chat_appended": payload })
    }

    fn seated(variant: &str, mut payload: Value) -> Value {
        payload["key"] = seat()["session"].clone();
        let mut update = Map::new();
        update.insert(variant.to_owned(), payload);
        Value::Object(update)
    }

    fn words(text: &str) -> Value {
        json!({ "type": "user", "message": { "content": [{ "type": "text", "text": text }] } })
    }

    /// A snapshot is what a reader gets, and a seat update patches it in
    /// place - the record is the server's shape, held.
    #[test]
    fn a_snapshot_reads_back_and_an_update_patches_it() {
        let mut records = open();
        records.apply(&seated("context_usage_snapshot", json!({ "percentage": 0.42 })));
        assert_eq!(
            held(&records)["header"]["context"]["percent"],
            json!(0.42),
            "the update's name is read into the record's"
        );
    }

    /// A person's words open a turn; a tool result and a sub-agent's frame
    /// ride the turn they belong to rather than opening one of their own.
    #[test]
    fn a_person_s_words_open_turns_and_a_frame_that_draws_nothing_joins_one() {
        let mut records = open();
        records.apply(&update(json!({ "msg": words("hi"), "key": seat()["session"] })));
        let answer = json!({ "type": "user", "message": { "content": [{ "type": "tool_result", "content": "ok" }] } });
        records.apply(&update(json!({ "msg": answer, "key": seat()["session"] })));
        let dispatched = json!({
            "type": "user",
            "parent_tool_use_id": "toolu_1",
            "message": { "content": [{ "type": "text", "text": "a sub-agent's own words" }] },
        });
        records.apply(&update(json!({ "msg": dispatched, "key": seat()["session"] })));
        // The launch terminal's typing is a decided ignore: a frame whose
        // only block is a local command draws nothing and opens no turn.
        let local = json!({
            "type": "user",
            "message": { "content": [{ "type": "text", "text": "<command-name>/clear</command-name>" }] },
        });
        records.apply(&update(json!({ "msg": local, "key": seat()["session"] })));
        // The next person's words are a turn of their own.
        records.apply(&update(json!({ "msg": words("and again"), "key": seat()["session"] })));
        let seen = held(&records);
        let turns = seen["conversation"]["turns"].as_array().expect("turns");
        assert_eq!(turns.len(), 2, "two turns: the words opened two, nothing else split the first");
        assert_eq!(
            turns[0]["messages"].as_array().expect("messages").len(),
            4,
            "the result, the sub-agent's frame and the local command joined"
        );
    }

    /// The turn's own end settles the in-flight flag - all three ways the
    /// core says it.
    #[test]
    fn a_result_settles_the_turn() {
        let mut records = open();
        records.apply(&update(
            json!({ "msg": { "type": "system", "subtype": "init" }, "key": seat()["session"] }),
        ));
        assert_eq!(held(&records)["header"]["turn_in_flight"], json!(true), "init opens a turn");
        records.apply(&seated("turn_complete", json!({})));
        assert_eq!(held(&records)["header"]["turn_in_flight"], json!(false));
    }

    /// A draft leads the queue and a question waits behind it - the ordering
    /// every hop keeps (#1717's rule).
    #[test]
    fn a_draft_leads_and_a_question_waits_behind() {
        let mut records = open();
        let question = json!({ "tool_call": { "tool_call_id": "t1" }, "question_index": 0 });
        records.apply(&seated("question_request", json!({ "request": question })));
        let draft = json!({ "id": "d1", "channel": "#x", "text": "hey" });
        records.apply(&seated("slack_post_pending", json!({ "draft": draft })));
        let seen = held(&records);
        let asks = seen["pending_asks"].as_array().expect("asks");
        assert_eq!(asks[0]["kind"], json!("slack_draft"), "the draft leads");
        assert_eq!(asks[1]["kind"], json!("question"));
    }

    /// The round as well as the call: a resolution naming a later round does
    /// not clear the parked ask, and the one naming its own round does
    /// (Ved's live find, #1717).
    #[test]
    fn a_resolution_clears_only_the_round_it_names() {
        let mut records = open();
        let question = json!({ "tool_call": { "tool_call_id": "t1" }, "question_index": 1 });
        records.apply(&seated("question_request", json!({ "request": question })));
        // The previous round's resolution lands late: same call, round 0.
        records.apply(&seated(
            "pending_interaction_resolved",
            json!({ "tool_id": "t1", "question_index": 0 }),
        ));
        assert_eq!(
            held(&records)["pending_asks"].as_array().expect("asks").len(),
            1,
            "round 0 is not round 1"
        );
        // The resolution of the round it does name clears it.
        records.apply(&seated(
            "pending_interaction_resolved",
            json!({ "tool_id": "t1", "question_index": 1 }),
        ));
        assert!(held(&records)["pending_asks"].as_array().expect("asks").is_empty());
    }

    /// The prompt pile folds where the page reads it: `state.queue`, never
    /// the record's top level, and a state this build does not know leaves
    /// the row standing rather than dropping it on a parse miss.
    #[test]
    fn a_queued_prompt_folds_into_the_state_the_page_reads() {
        let mut records = open();
        records.apply(&seated(
            "prompt_queued",
            json!({ "uuid": "u1", "text": "do it", "source": "forge" }),
        ));
        let seen = held(&records);
        assert_eq!(
            seen["state"]["queue"].as_array().expect("queue").len(),
            1,
            "the pile is folded where the read path looks"
        );
        assert!(seen["queue"].is_null(), "and nothing is written where nothing reads");
        records.apply(&seated("prompt_lifecycle", json!({ "uuid": "u1", "state": "discarded" })));
        assert_eq!(held(&records)["state"]["queue"].as_array().expect("queue").len(), 0);
        records.apply(&seated(
            "prompt_queued",
            json!({ "uuid": "u2", "text": "later", "source": "forge" }),
        ));
        records
            .apply(&seated("prompt_lifecycle", json!({ "uuid": "u2", "state": "something_new" })));
        assert_eq!(
            held(&records)["state"]["queue"].as_array().expect("queue").len(),
            1,
            "a word the CLI adds later leaves the row standing"
        );
    }

    /// A REPLACES update is a fresh read, not a patch: the caller is told to
    /// ask again, and the record held now stays readable until the answer
    /// lands - never an empty seat.
    #[test]
    fn a_replaces_update_asks_for_a_read_and_keeps_the_record_readable() {
        let mut records = open();
        let refresh = records.apply(&seated("session_replaced", json!({})));
        assert_eq!(refresh, vec![key().to_owned()], "the seat is re-read");
        assert!(held(&records).is_object(), "the record stands until the snapshot lands");
    }
}
