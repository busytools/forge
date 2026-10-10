//! The seat records and the conversation hold, owned by this process.
//!
//! **Rust holds what the page used to fold.** `client/src/session/apply.ts`
//! and the structural half of `client/src/chat/conversation.ts` live here
//! now, so a frame lands on a record whether or not a webview is awake to
//! read it - the point of the whole move (a parked page consumed nothing and
//! every ask died behind it; the 2026-10-10 catch). The page still draws:
//! it reads a record on a coalesced notice and re-folds it with
//! `chat/units.ts`, which stays TS by decision.
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
//!   append, the in-flight header, the take - this folds exactly, because
//!   those are state rather than a view's shape. One clause cannot be exact:
//!   the turn append asks the chat fold whether a frame draws, and the fold
//!   stays TS by decision, so `opens_a_turn` tests the cases the fold's own
//!   comment names and says so.
//!
//! Every field name below was taken off `apply.ts` (which itself took them
//! off the Rust side), and the handlers file a change only when the value
//! really moved - a re-delivered frame must not read as news.

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

/// The meter's own constants, mirrored from `composer/meter.ts` and
/// `wire/limits.ts` (themselves mirrors of the server's).
const METER_CEILING_DB: f64 = 0.0;
const FALLBACK_FLOOR_DB: f64 = -50.0;
const METER_CELLS: usize = 120;

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

/// What changed when a frame was applied: the seats whose readers must be
/// told, the seats whose record must be asked for again, and whether the
/// whole fleet moved (the home re-reads on its own schedule for that).
#[derive(Default)]
pub struct Changed {
    /// Records that moved: a reader re-reads and redraws.
    pub seats: Vec<String>,
    /// Seats a REPLACES frame took a new occupant for: the app half asks the
    /// core for the record again, and the snapshot replaces it whole.
    pub refresh: Vec<String>,
    /// The fleet moved: a held home re-reads.
    pub home: bool,
}

impl Changed {
    pub fn any(&self) -> bool {
        self.home || !self.seats.is_empty() || !self.refresh.is_empty()
    }
}

/// Every subject this client holds: seats, the home, usage, the models page.
pub struct Records {
    subjects: HashMap<String, Held>,
    /// The seat slots by subject key, so an update's own `key` finds its way
    /// without re-parsing the JSON.
    seats: HashMap<String, Value>,
}

impl Default for Records {
    fn default() -> Self {
        Self::new()
    }
}

impl Records {
    pub fn new() -> Self {
        Self { subjects: HashMap::new(), seats: HashMap::new() }
    }

    /// A snapshot for a subject: the read's own answer, and the point the
    /// updates patch from.
    pub fn snapshot(&mut self, subject: &Value, data: Value) -> Changed {
        let key = subject_key(subject);
        if key.is_empty() {
            return Changed::default();
        }
        if let Some(slot) = subject.get("session") {
            self.seats.insert(key.clone(), slot.clone());
        }
        let home = key == "home";
        self.subjects.insert(key.clone(), Held::Ready(data));
        let mut changed = Changed::default();
        if home {
            changed.home = true;
        } else {
            changed.seats.push(key);
        }
        changed
    }

    /// A subscribe the core refused: the subject answers "refused" so a page
    /// draws the reason rather than an empty seat.
    pub fn refuse(&mut self, key: &str, why: &str) -> Changed {
        if key.is_empty() {
            return Changed::default();
        }
        let home = key == "home";
        self.subjects.insert(key.to_owned(), Held::Refused(why.to_owned()));
        let mut changed = Changed::default();
        if home {
            changed.home = true;
        } else {
            changed.seats.push(key.to_owned());
        }
        changed
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

    /// Which seat a subject key names, for a subscriber that must carry the
    /// slot back to the socket.
    pub fn slot_of(&self, key: &str) -> Option<&Value> {
        self.seats.get(key)
    }

    /// Fold one `update` frame's variant into its seat's record (or mark the
    /// home for a re-read). Returns the subjects a reader must be told about.
    pub fn apply(&mut self, update: &Value) -> Changed {
        let Some((name, payload)) = variant_of(update) else {
            return Changed::default();
        };
        let mut changed = Changed::default();
        // The home's own policy, ported: the server sends more than a fleet
        // region draws, and a keyless update belongs to no seat - so a
        // home-relevant update re-reads the home rather than patching a row
        // (`wire/fleet.ts`'s `coversHome`; the core folds a fleet row and a
        // page reads the fold).
        if self.subjects.contains_key("home") && covers_home(update) {
            changed.home = true;
        }
        // A seat is addressed by its slot; a keyless update belongs to the
        // home alone, and a seat this client never subscribed to is not this
        // client's to keep.
        let Some(key) = seat_key_of(update) else {
            return changed;
        };
        let Some(Held::Ready(record)) = self.subjects.get_mut(&key) else {
            return changed;
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
            changed.refresh.push(key);
        } else if seat_update(record, name, payload) {
            changed.seats.push(key);
        }
        changed
    }

    /// The socket is gone: a take goes with it.
    ///
    /// The core drops a take whose reader went away, and no frame says so - a
    /// record left drawing a recording that is over is the live store's
    /// `status` arm (`session/live.ts`, #1880). Everything else stands where
    /// it was: the reconnect re-asks every held subscription and the answers
    /// replace the records whole.
    pub fn dropped(&mut self) -> Changed {
        let mut changed = Changed::default();
        for (key, held) in &mut self.subjects {
            let Held::Ready(record) = held else { continue };
            let Some(composer) = record.get_mut("composer").and_then(Value::as_object_mut) else {
                continue;
            };
            if composer.get("take").is_some_and(|take| !take.is_null()) {
                composer.insert("take".to_owned(), Value::Null);
                changed.seats.push(key.clone());
            }
        }
        changed
    }

    /// The subject a held seat key names, for the app half's own re-read
    /// (`refresh`).
    pub fn subject_of(&self, key: &str) -> Option<Value> {
        self.seats.get(key).map(|slot| json!({ "session": slot }))
    }
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

/// Whether this update asks the home for a re-read: the fleet's own news, or
/// a keyless update, which belongs to no seat and is the home's alone. A port
/// of `wire/fleet.ts`'s `coversHome` - the server sends more than a fleet
/// region draws, and the keyless arm is why a page does not keep the service
/// status or the plugin records it read once at subscribe.
fn covers_home(update: &Value) -> bool {
    fleet_news(update) || seat_key_of(update).is_none()
}

/// The variants that redraw a row or a card, none of which carries a payload
/// the fleet reads. A port of `wire/fleet.ts`'s `REDRAWS`, which
/// `fleet.test.ts` holds equal, variant for variant, to the server's own
/// `fleet_news` arm (`crates/forge-server/src/live.rs`).
const REDRAWS: &[&str] = &[
    "catalog_loaded",
    "cli_version_changed",
    "accounts_changed",
    "dictate_availability",
    "connection_failed",
    "releasing",
    "auth_required",
    "turn_error",
    "turn_cancelled",
    "permission_request",
    "question_request",
    "pending_interaction_resolved",
    "prompt_lifecycle",
    "slack_post_pending",
    "slack_draft_resolved",
    "browser_hand_off_pending",
    "browser_hand_off_resolved",
    "worker_status_changed",
    "tasks_changed",
    "cron_schedules_changed",
    "connector_subscriptions_changed",
];

/// Whether an update is the fleet's own news. A port of `wire/fleet.ts`'s
/// `fleetNews`, collapsed to the one bit the home's re-read needs - it reads
/// the whole answer back either way, so which kind of news it was buys
/// nothing.
fn fleet_news(update: &Value) -> bool {
    let Some((name, payload)) = variant_of(update) else { return false };
    if name == "chat_appended" {
        // The bulk of the stream, and the one arm that has to look inside the
        // CLI's own frame rather than at a variant name: a turn's own words
        // are most of it and no row shows one.
        let Some(msg) = payload.get("msg") else { return false };
        return match msg.get("type").and_then(Value::as_str) {
            // A turn that finished well arms the row's completion, and a
            // failure or a cancellation arms its mark: all three redraw.
            Some("result") => true,
            // `state` sits on the frame itself; both of its outcomes redraw.
            Some("system") => matches!(
                msg.get("subtype").and_then(Value::as_str),
                Some("background_tasks_changed" | "session_state_changed")
            ),
            _ => false,
        };
    }
    // The occupant arms announce a new row under a slot; a keyless occupant
    // update is covers_home's own arm.
    REDRAWS.contains(&name) || matches!(name, "spawning" | "connected" | "session_replaced")
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
            let empty =
                record.get("queue").and_then(Value::as_array).is_none_or(|rows| rows.is_empty());
            if empty {
                return false;
            }
            let last = record
                .get("queue")
                .and_then(Value::as_array)
                .and_then(|rows| rows.last())
                .and_then(|row| row.get("text"))
                .cloned();
            record.insert("queue".to_owned(), json!([]));
            if let Some(text) = last {
                record.insert(
                    "queue_ended".to_owned(),
                    json!({ "text": text, "state": "discarded" }),
                );
            }
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
            let queue = array(record.get("queue"));
            if queue.iter().any(|row| row.get("uuid").and_then(Value::as_str) == Some(uuid)) {
                return false;
            }
            let mut queue = queue;
            queue.push(json!({
                "uuid": uuid,
                "source": payload.get("source").and_then(Value::as_str).unwrap_or("forge"),
                "text": words,
            }));
            // A new row is the next thing to look at, so the last ending goes
            // with it rather than standing beside a queue that has moved on.
            record.insert("queue_ended".to_owned(), Value::Null);
            record.insert("queue".to_owned(), Value::Array(queue));
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
            let queue = array(record.get("queue"));
            let Some(leaving) =
                queue.iter().find(|row| row.get("uuid").and_then(Value::as_str) == Some(uuid))
            else {
                return false;
            };
            let leaving = leaving.get("text").cloned();
            let remaining: Vec<Value> = queue
                .into_iter()
                .filter(|row| row.get("uuid").and_then(Value::as_str) != Some(uuid))
                .collect();
            record.insert("queue".to_owned(), Value::Array(remaining));
            // Two states leave with a word rather than silently.
            if state == "discarded" || state == "refused" {
                record.insert(
                    "queue_ended".to_owned(),
                    json!({ "text": leaving.unwrap_or(Value::Null), "state": state }),
                );
            }
            true
        }
        "prompt_cancel_resolved" => {
            let Some(uuid) = payload.get("uuid").and_then(Value::as_str) else { return false };
            if payload.get("cancelled") != Some(&Value::Bool(true)) {
                return false;
            }
            let queue = array(record.get("queue"));
            let remaining: Vec<Value> = queue
                .iter()
                .filter(|row| row.get("uuid").and_then(Value::as_str) != Some(uuid))
                .cloned()
                .collect();
            if remaining.len() == queue.len() {
                return false;
            }
            record.insert("queue".to_owned(), Value::Array(remaining));
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
        "dictate_started" => {
            let Some(floor) = payload.get("floor_db").and_then(Value::as_f64) else {
                return false;
            };
            let Some(composer) = record.get_mut("composer").and_then(Value::as_object_mut) else {
                return false;
            };
            // A new take supersedes what the seat was doing, its notice
            // included: the words it left are already in the box.
            composer.insert("take".to_owned(), new_take(floor, payload.get("generation")));
            composer.insert("notice".to_owned(), Value::Null);
            true
        }
        "dictate_level" => {
            let Some(peak) = payload.get("peak_db").and_then(Value::as_f64) else { return false };
            let Some(take) = held_take_mut(record) else { return false };
            let floor = take.get("floor_db").and_then(Value::as_f64).unwrap_or(FALLBACK_FLOOR_DB);
            let fraction = fraction_of(peak, floor);
            let mut levels = array(take.get("levels"));
            if levels.len() >= METER_CELLS {
                levels.remove(0);
            }
            levels.push(json!(fraction));
            take.insert("levels".to_owned(), Value::Array(levels));
            take.insert("peak_db".to_owned(), json!(peak));
            stamp_elapsed(take);
            true
        }
        "dictate_transcribing" => {
            let Some(take) = held_take_mut(record) else { return false };
            take.insert("phase".to_owned(), json!("transcribing"));
            stamp_elapsed(take);
            true
        }
        "dictate_progress" => {
            let Some(take) = held_take_mut(record) else { return false };
            if !of_this_take(take, payload) {
                return false;
            }
            // Either half is enough: a report naming neither says nothing.
            if payload.get("done").is_none() && payload.get("total").is_none() {
                return false;
            }
            take.insert(
                "progress".to_owned(),
                json!([
                    payload.get("done").cloned().unwrap_or(Value::Null),
                    payload.get("total").cloned().unwrap_or(Value::Null)
                ]),
            );
            stamp_elapsed(take);
            true
        }
        "dictate_ended" => {
            let outcome = outcome_of(payload.get("outcome"));
            let refused = outcome.get("refused").is_some();
            let take = held_take(record);
            // A refusal resolves no take - it answers a start that never ran.
            if refused && take.is_some() {
                return false;
            }
            // A tail from a take that is gone is not this one.
            if !refused {
                match &take {
                    Some(take) if of_this_take(take, payload) => {}
                    _ => return false,
                }
            }
            let floor = take
                .and_then(|take| take.get("floor_db").and_then(Value::as_f64))
                .unwrap_or(FALLBACK_FLOOR_DB);
            let notice = notice_of(&outcome, floor);
            let Some(composer) = record.get_mut("composer").and_then(Value::as_object_mut) else {
                return false;
            };
            composer.insert("take".to_owned(), Value::Null);
            composer.insert("notice".to_owned(), notice.unwrap_or(Value::Null));
            true
        }
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
/// decision** - `apply.ts` asks `fold([message]).length > 0`, and the fold's
/// own comments name the cases where a person's frame draws nothing: a call's
/// result arrives in a user frame and draws nothing on its own (which is what
/// holds a call and the frames that update it in one turn), and a dispatched
/// agent's frames are not this conversation at all (`units.ts`'s
/// `isDispatched` - the wire spells no dispatch as null). Those two are what
/// this tests; every other user frame carries words or a picture.
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
        // A bare content string is words, which draw.
        Some(Value::String(_)) => true,
        Some(Value::Array(blocks)) => blocks
            .iter()
            .any(|block| !matches!(block.get("type").and_then(Value::as_str), Some("tool_result"))),
        _ => false,
    }
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

/// A take as it begins, as this side's own state holds it.
fn new_take(floor_db: f64, generation: Option<&Value>) -> Value {
    let mut take = json!({
        "phase": "recording",
        "levels": [],
        "peak_db": floor_db,
        "progress": [0, null],
        "floor_db": floor_db,
        // The wire carries no duration: this is the same reading the server
        // computes, from this side's clock.
        "elapsed_ms": 0,
        "started_ms": now_ms(),
    });
    // This side's own bookkeeping, so a report for a superseded take is
    // dropped rather than drawn over the live one. The wire never sends it,
    // and a payload naming none leaves the key off - the shape of the TS
    // side's `undefined`.
    if let Some(generation) = generation
        && let Some(take) = take.as_object_mut()
    {
        take.insert("generation".to_owned(), generation.clone());
    }
    take
}

/// The take the composer holds, if any.
fn held_take(record: &Map<String, Value>) -> Option<&Map<String, Value>> {
    record.get("composer")?.get("take")?.as_object()
}

fn held_take_mut(record: &mut Map<String, Value>) -> Option<&mut Map<String, Value>> {
    record.get_mut("composer")?.get_mut("take")?.as_object_mut()
}

/// Keep the elapsed reading in step with the take's own clock.
fn stamp_elapsed(take: &mut Map<String, Value>) {
    if let Some(started) = take.get("started_ms").and_then(Value::as_u64) {
        take.insert("elapsed_ms".to_owned(), json!(now_ms().saturating_sub(started)));
    }
}

/// Whether a report about a take is about THIS take: a take with no
/// generation checks nothing - only one take per seat is live at a time.
fn of_this_take(take: &Map<String, Value>, payload: &Value) -> bool {
    match take.get("generation") {
        None => true,
        Some(held) => Some(held) == payload.get("generation"),
    }
}

/// One reading, as a fraction of the take's own range - `composer/meter.ts`'s
/// `fractionOf`, mirrored.
fn fraction_of(peak_db: f64, floor_db: f64) -> f64 {
    let span = (METER_CEILING_DB - floor_db).max(1.0);
    ((peak_db - floor_db) / span).clamp(0.0, 1.0)
}

/// A finished take's outcome, keyed: `DictateOutcome` is externally tagged,
/// so a unit variant crosses as its name alone.
fn outcome_of(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::String(name)) => {
            let mut map = Map::new();
            map.insert(name.clone(), Value::Null);
            map
        }
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// The notice a finished take leaves, worded as the server's own
/// `composer.rs` words it - a reader who dictates in any view reads the same
/// lines.
fn notice_of(outcome: &Map<String, Value>, floor_db: f64) -> Option<Value> {
    if let Some(landed) = outcome.get("landed") {
        return Some(json!({
            "kind": "landed",
            "text": landed.get("text").and_then(Value::as_str).unwrap_or(""),
            "truncated": landed.get("truncated") == Some(&Value::Bool(true)),
        }));
    }
    if outcome.contains_key("empty") {
        return Some(json!({
            "kind": "line", "tone": "q",
            "text": "that was all filler \u{b7} nothing to insert",
        }));
    }
    if let Some(silent) = outcome.get("no_audio") {
        let peak = silent.get("peak_db").and_then(Value::as_f64);
        return Some(match peak {
            None => json!({
                "kind": "line", "tone": "bad",
                "text": "no signal from the microphone at all \u{b7} check permission or mute",
            }),
            Some(peak) => {
                let seconds = silent.get("seconds").and_then(Value::as_f64).unwrap_or(0.0);
                json!({
                    "kind": "line", "tone": "q",
                    "text": format!(
                        "nothing above {} dBFS in {}s \u{b7} loudest was {:.1} \u{b7} try again",
                        floor_db.round(), seconds, peak,
                    ),
                })
            }
        });
    }
    if let Some(refused) = outcome.get("refused") {
        return Some(json!({
            "kind": "line", "tone": "bad",
            "text": refused.get("message").and_then(Value::as_str).unwrap_or(""),
        }));
    }
    if outcome.contains_key("failed") {
        return Some(json!({
            "kind": "line", "tone": "q",
            "text": "dictation failed \u{b7} try again; restart forge if it repeats",
        }));
    }
    None
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Ask the core for a subject afresh: the read a REPLACES update and a home
/// re-read both need. The subscription is given up and taken again, which is
/// what makes the core answer with a snapshot rather than a delta.
pub fn refresh(socket: &Socket, subject: &Value) {
    socket.unsubscribe(subject.clone());
    socket.subscribe(subject.clone(), true, true);
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
            "composer": { "take": null, "notice": null, "compacting": false },
            "queue": [],
            "queue_ended": null,
            "pending_asks": [],
        })
    }

    fn open() -> Records {
        let mut records = Records::new();
        records.snapshot(&seat(), record());
        records
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
        let changed =
            records.apply(&seated("context_usage_snapshot", json!({ "percentage": 0.42 })));
        assert_eq!(changed.seats.len(), 1);
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert_eq!(
            held["header"]["context"]["percent"],
            json!(0.42),
            "the update's name is read into the record's"
        );
    }

    /// A re-delivered frame is not news: the same value twice files one
    /// change.
    #[test]
    fn an_update_that_changes_nothing_is_not_a_change() {
        let mut records = open();
        let first = records.apply(&seated("dispatches_changed", json!({ "has_dispatches": true })));
        assert_eq!(first.seats.len(), 1);
        let again = records.apply(&seated("dispatches_changed", json!({ "has_dispatches": true })));
        assert!(again.seats.is_empty(), "the same flag twice is one change");
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
        // The next person's words are a turn of their own.
        records.apply(&update(json!({ "msg": words("and again"), "key": seat()["session"] })));
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        let turns = held["conversation"]["turns"].as_array().expect("turns");
        assert_eq!(turns.len(), 2, "two turns: the words opened two, nothing else split the first");
        assert_eq!(
            turns[0]["messages"].as_array().expect("messages").len(),
            3,
            "the result and the sub-agent's frame joined"
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
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert_eq!(held["header"]["turn_in_flight"], json!(true), "init opens a turn");
        records.apply(&seated("turn_complete", json!({})));
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert_eq!(held["header"]["turn_in_flight"], json!(false));
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
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        let asks = held["pending_asks"].as_array().expect("asks");
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
        let stale = records.apply(&seated(
            "pending_interaction_resolved",
            json!({ "tool_id": "t1", "question_index": 0 }),
        ));
        assert!(stale.seats.is_empty(), "round 0 is not round 1");
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert_eq!(held["pending_asks"].as_array().expect("asks").len(), 1);
        // The resolution of the round it does name clears it.
        let named = records.apply(&seated(
            "pending_interaction_resolved",
            json!({ "tool_id": "t1", "question_index": 1 }),
        ));
        assert_eq!(named.seats.len(), 1, "round 1's own resolution");
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert!(held["pending_asks"].as_array().expect("asks").is_empty());
    }

    /// The prompt pile: words in, a settled state out, and the ending only
    /// for the states that leave with a word.
    #[test]
    fn a_queued_prompt_leaves_with_a_word_only_when_it_was_not_taken() {
        let mut records = open();
        records.apply(&seated(
            "prompt_queued",
            json!({ "uuid": "u1", "text": "do it", "source": "forge" }),
        ));
        records.apply(&seated("prompt_lifecycle", json!({ "uuid": "u1", "state": "discarded" })));
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert_eq!(held["queue"].as_array().expect("queue").len(), 0);
        assert_eq!(held["queue_ended"]["state"], json!("discarded"));
        // A state a later CLI adds leaves the row standing rather than
        // dropping it on a parse miss.
        records.apply(&seated(
            "prompt_queued",
            json!({ "uuid": "u2", "text": "later", "source": "forge" }),
        ));
        let unknown = records
            .apply(&seated("prompt_lifecycle", json!({ "uuid": "u2", "state": "something_new" })));
        assert!(unknown.seats.is_empty());
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert_eq!(held["queue"].as_array().expect("queue").len(), 1);
    }

    /// A take's ending is the server's own words, and a refusal resolves no
    /// take that is drawn.
    #[test]
    fn a_refusal_does_not_clear_a_live_take() {
        let mut records = open();
        records.apply(&seated("dictate_started", json!({ "floor_db": -50.0, "generation": 1 })));
        let refused = records.apply(&seated(
            "dictate_ended",
            json!({ "outcome": { "refused": { "message": "already dictating" } }, "generation": 1 }),
        ));
        assert!(refused.seats.is_empty(), "a refusal leaves the drawn take alone");
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert!(held["composer"]["take"].is_object());
        // A real ending clears it and leaves the notice.
        records.apply(&seated("dictate_ended", json!({ "outcome": "empty", "generation": 1 })));
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert!(held["composer"]["take"].is_null());
        assert_eq!(held["composer"]["notice"]["kind"], json!("line"));
    }

    /// A REPLACES update is a fresh read, not a patch: the caller is told to
    /// ask again, and the record held now stays readable until the answer
    /// lands - never an empty seat.
    #[test]
    fn a_replaces_update_asks_for_a_read_and_keeps_the_record_readable() {
        let mut records = open();
        let changed = records.apply(&seated("session_replaced", json!({})));
        assert_eq!(changed.refresh.len(), 1, "the seat is re-read");
        assert!(changed.seats.is_empty(), "nothing was patched");
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert!(held.is_object(), "the record stands until the snapshot lands");
    }

    /// The home's own routing: the fleet's news re-reads it, and a person's
    /// own words do not - the bulk of the stream must not cost a read.
    #[test]
    fn a_fleet_news_update_marks_a_held_home_and_words_do_not() {
        let mut records = open();
        records.snapshot(&json!("home"), json!({ "projects": [] }));
        let quiet = records
            .apply(&json!({ "chat_appended": { "msg": words("hi"), "key": seat()["session"] } }));
        assert!(!quiet.home, "a turn's words are not fleet news");
        let news = records.apply(&seated("turn_error", json!({})));
        assert!(news.home, "a failing turn moves the rows");
        let keyless = records.apply(&json!({ "connection_failed": { "reason": "rate limited" } }));
        assert!(keyless.home, "a keyless update is the home's alone");
        assert!(keyless.seats.is_empty(), "and no seat's");
    }

    /// A dropped socket takes a live recording with it - the core drops a
    /// take whose reader went away, and no frame says so (#1880).
    #[test]
    fn a_dropped_socket_takes_a_live_recording_with_it() {
        let mut records = open();
        records.apply(&seated("dictate_started", json!({ "floor_db": -50.0, "generation": 1 })));
        let changed = records.dropped();
        assert_eq!(changed.seats.len(), 1);
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert!(held["composer"]["take"].is_null(), "the take went with the socket");
        assert!(records.dropped().seats.is_empty(), "and a second drop is not news");
    }

    /// A reading lands as a fraction of the take's own range, the newest kept
    /// - and a report naming half of the progress still lands.
    #[test]
    fn a_level_lands_as_a_fraction_and_half_a_progress_still_lands() {
        let mut records = open();
        records.apply(&seated("dictate_started", json!({ "floor_db": -50.0, "generation": 7 })));
        records.apply(&seated("dictate_level", json!({ "peak_db": -25.0, "generation": 7 })));
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        let take = &held["composer"]["take"];
        assert_eq!(take["levels"], json!([0.5]), "half of a 50 dB range");
        assert_eq!(take["peak_db"], json!(-25.0));
        // A report for a superseded take is dropped - the generation lives on
        // the reports that name it (`apply.ts` checks it on progress and on
        // the ending, not on a level).
        let stale =
            records.apply(&seated("dictate_progress", json!({ "done": 1, "generation": 8 })));
        assert!(stale.seats.is_empty(), "generation 8 is not the live take");
        records.apply(&seated("dictate_progress", json!({ "done": 3, "generation": 7 })));
        let held =
            records.read("session:Busytools\u{0}forge\u{0}lead").expect("held").expect("ready");
        assert_eq!(held["composer"]["take"]["progress"], json!([3, null]));
    }
}
