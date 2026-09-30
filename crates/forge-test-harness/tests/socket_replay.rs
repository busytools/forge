//! The socket's recorded contract, replayed on every `just check` - no
//! server, no browser, no API cost.
//!
//! This is the socket half of `sdk_replay.rs`. That one asks whether forge can
//! still DECODE what the CLI sent; this asks whether the shape the socket
//! hands a client is still the shape that was recorded. A field the server
//! renames, or a frame it starts or stops sending, moves a record and fails
//! here instead of drawing a blank in a page whose own tests are all green.
//!
//! **Two axes, and they fail differently.**
//!
//! - The FRAME census is a `match` with no wildcard arm over each enum that
//!   crosses the socket, so a variant added, removed or renamed is a compile
//!   error before it is ever a failing test. The record then carries every
//!   variant's wire name, and regenerating it is what keeps a rename honest.
//! - The FIELD record is a path-and-key walk over encoded values. A renamed
//!   field moves a line in `chat.json`.
//!
//! **What this cannot see, stated rather than implied.** The wire name of a
//! variant is derived from its Rust name, so a change to a container's
//! `rename_all` is caught only for the variants a sample is built for - all
//! six `ServerMessage`s, all three `Subject`s, one `SessionUpdate`. The other
//! fifty-five `SessionUpdate`s and all thirty-three `Command`s rest on their
//! container attribute. And an update payload is only pinned for the variants
//! sampled below, which is one.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use forge_primitives::{Message, SessionSlot};
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use forge_server::transport::PROTOCOL_VERSION;
use forge_server::transport::envelope::{ClientSettings, ServerMessage, Subject};
use forge_server::{Command, SessionUpdate};
use forge_test_harness::sdk_wire::{baseline_dir, legacy_baseline_dir, load_baseline_from};
use serde_json::{Value, json};

/// A variant's wire name under `rename_all = "snake_case"`.
fn wire_name(rust_name: &str) -> String {
    let mut out = String::with_capacity(rust_name.len() + 4);
    for (at, ch) in rust_name.char_indices() {
        if ch.is_ascii_uppercase() {
            if at > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// The census: one `match` per enum, with **no wildcard arm**.
///
/// The variant list is written here rather than derived, so a variant the
/// server adds is a non-exhaustive match and a compile error; a variant it
/// removes is a pattern naming something that no longer exists, also a
/// compile error. `stringify!` means the returned name is the variant's own,
/// so it cannot drift from the pattern beside it.
macro_rules! census {
    ($value:expr, $enum:ident, structs [$($structs:ident),* $(,)?] tuples [$($tuples:ident),* $(,)?]) => {
        match $value {
            $( $enum::$structs { .. } => stringify!($structs), )*
            $( $enum::$tuples(..) => stringify!($tuples), )*
        }
    };
}

const SERVER_MESSAGE_VARIANTS: &[&str] =
    &["Greeting", "Snapshot", "Update", "Page", "Reply", "Error"];

const SUBJECT_VARIANTS: &[&str] = &["Home", "Usage", "Session"];

const SESSION_UPDATE_VARIANTS: &[&str] = &[
    "Spawning",
    "Connected",
    "SessionReplaced",
    "ConnectionFailed",
    "AuthRequired",
    "SlashCommandError",
    "RuntimeReloadCompleted",
    "RuntimeReloadFailed",
    "SetModeFailed",
    "SetModelFailed",
    "PermissionRequest",
    "QuestionRequest",
    "PendingInteractionResolved",
    "McpOperationError",
    "TurnComplete",
    "TurnCancelled",
    "TurnError",
    "ChatAppended",
    "HookObservation",
    "StatusSnapshot",
    "ForgeAccountIdentity",
    "DictateOverrides",
    "DictateDevicePin",
    "OauthCredentialsSnapshot",
    "ContextUsageSnapshot",
    "McpSnapshot",
    "SessionsListed",
    "ServiceStatus",
    "CatalogLoaded",
    "CliVersionChanged",
    "AccountsChanged",
    "PluginsInventoryUpdated",
    "PluginsInventoryRefreshFailed",
    "PluginsCliActionSucceeded",
    "PluginsCliActionFailed",
    "PluginsUpdateRunProgress",
    "PluginsUpdateRunFinished",
    "PluginsRollbackSucceeded",
    "PluginsRollbackFailed",
    "PeerInflightStatsChanged",
    "WorkerStatusChanged",
    "PeerEnvelopeAppended",
    "GotifyNotificationAppended",
    "CronPromptAppended",
    "SlackMessageAppended",
    "SlackPostPending",
    "SlackDraftExpired",
    "PromptQueuedWhileBusy",
    "ReviewActivityNotice",
    "DictateAvailability",
    "DictateStarted",
    "DictateLevel",
    "DictateTranscribing",
    "DictateProgress",
    "DictateEnded",
    "FatalError",
];

const COMMAND_VARIANTS: &[&str] = &[
    "Prompt",
    "Cancel",
    "SetMode",
    "SetModel",
    "NewSession",
    "ResumeSession",
    "RespondPermission",
    "RespondSlackPost",
    "RespondQuestion",
    "SetDictateOverride",
    "ResetDictateOverrides",
    "SetDictateDevice",
    "ReconnectMcpServer",
    "ToggleMcpServer",
    "SpawnProject",
    "SpawnSession",
    "StartDefault",
    "DeliverPeerPrompt",
    "SpawnWorker",
    "CloseWorker",
    "OpenUrl",
    "DespawnWorker",
    "DeliverWorkerPrompt",
    "DeliverWorkerPromptToLead",
    "DeliverGotifyMessage",
    "DictateStart",
    "DictateStop",
    "SaveReviewThreads",
    "RemoveReviewThread",
    "SetReviewThreadStatus",
    "CloseSession",
    "UpsertReviewThread",
    "SubmitReview",
];

fn server_message_census(message: &ServerMessage) -> &'static str {
    census!(message, ServerMessage,
        structs [Greeting, Snapshot, Update, Page, Reply, Error]
        tuples [])
}

fn subject_census(subject: &Subject) -> &'static str {
    census!(subject, Subject, structs [Home, Usage] tuples [Session])
}

fn session_update_census(update: &SessionUpdate) -> &'static str {
    census!(update, SessionUpdate,
        structs [
            Spawning, Connected, SessionReplaced, ConnectionFailed, AuthRequired,
            SlashCommandError, RuntimeReloadCompleted, RuntimeReloadFailed, SetModeFailed,
            SetModelFailed, PermissionRequest, QuestionRequest, PendingInteractionResolved,
            McpOperationError, TurnComplete, TurnCancelled, TurnError, ChatAppended,
            HookObservation, StatusSnapshot, ForgeAccountIdentity, DictateOverrides,
            DictateDevicePin, OauthCredentialsSnapshot, ContextUsageSnapshot, McpSnapshot,
            SessionsListed, ServiceStatus, CatalogLoaded, CliVersionChanged, AccountsChanged,
            PluginsInventoryUpdated, PluginsInventoryRefreshFailed, PluginsCliActionSucceeded,
            PluginsCliActionFailed, PluginsUpdateRunProgress, PluginsUpdateRunFinished,
            PluginsRollbackSucceeded, PluginsRollbackFailed, PeerInflightStatsChanged,
            WorkerStatusChanged, PeerEnvelopeAppended, GotifyNotificationAppended,
            CronPromptAppended, SlackMessageAppended, SlackPostPending, SlackDraftExpired,
            PromptQueuedWhileBusy, ReviewActivityNotice, DictateAvailability, DictateStarted,
            DictateLevel, DictateTranscribing, DictateProgress, DictateEnded,
        ]
        tuples [FatalError])
}

fn command_census(command: &Command) -> &'static str {
    census!(command, Command,
        structs [
            Prompt, Cancel, SetMode, SetModel, NewSession, ResumeSession, RespondPermission,
            RespondSlackPost, RespondQuestion, SetDictateOverride, ResetDictateOverrides,
            SetDictateDevice, ReconnectMcpServer, ToggleMcpServer, SpawnProject, SpawnSession,
            StartDefault, DeliverPeerPrompt, SpawnWorker, CloseWorker, OpenUrl, DespawnWorker,
            DeliverWorkerPrompt, DeliverWorkerPromptToLead, DeliverGotifyMessage, DictateStart,
            DictateStop, SaveReviewThreads, RemoveReviewThread, SetReviewThreadStatus,
            CloseSession, UpsertReviewThread, SubmitReview,
        ]
        tuples [])
}

/// Where the records live, named by the protocol the server speaks rather
/// than by a literal: a version bump looks for a directory that is not there
/// and fails, which is the right answer for a client that would refuse to
/// connect anyway.
fn record_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("baselines")
        .join("socket")
        .join(PROTOCOL_VERSION.to_string())
}

fn record_path(name: &str) -> PathBuf {
    record_dir().join(name)
}

/// Every wire path in `value`, with the key set at each object.
///
/// Keys and paths only, never a value, so nothing a fixture legitimately
/// moves - a temp path, a clock reading, a pid - can reach a record.
#[derive(Default)]
struct Shape {
    paths: BTreeMap<String, BTreeSet<String>>,
    nulls: BTreeSet<String>,
    empty_arrays: BTreeSet<String>,
}

/// Whether a JSON key is a field name rather than a value.
///
/// serde writes a struct's field as an identifier whatever `rename_all` does
/// to its case, so anything else - a model name, the text of a question used
/// as a map key - is content. A record that kept one would move when the
/// content moved: a new model in a recaptured baseline, a reworded question,
/// neither of which is a shape change.
fn is_field_name(key: &str) -> bool {
    let mut chars = key.chars();
    let first = chars.next();
    matches!(first, Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Shape {
    fn record(&mut self, value: &Value, path: &str) {
        match value {
            Value::Object(fields) => {
                for (key, held) in fields {
                    if is_field_name(key) {
                        self.paths.entry(path.to_owned()).or_default().insert(key.clone());
                        if held.is_null() {
                            self.nulls.insert(format!("{path}/{key}"));
                        }
                        self.record(held, &format!("{path}/{key}"));
                    } else {
                        // A map keyed by content: the keys are not the
                        // shape, the values' own keys are.
                        self.record(held, &format!("{path}{{}}"));
                    }
                }
            }
            Value::Array(items) => {
                if items.is_empty() {
                    self.empty_arrays.insert(format!("{path}[]"));
                }
                for item in items {
                    self.record(item, &format!("{path}[]"));
                }
            }
            _ => {}
        }
    }

    fn keys(&self) -> usize {
        self.paths.values().map(BTreeSet::len).sum()
    }

    fn paths(&self) -> usize {
        self.paths.len()
    }
}

/// The tag of an externally-tagged value: its name alone for a unit variant,
/// otherwise the single key around its payload.
fn external_tag(encoded: &Value) -> String {
    match encoded {
        Value::String(name) => name.clone(),
        Value::Object(fields) => fields
            .keys()
            .next()
            .expect("an externally tagged variant is a name around its value")
            .clone(),
        other => panic!("not an externally tagged variant: {other}"),
    }
}

/// One decoded wire message, for the sample that stands for the chat
/// payload. Taken off a committed SDK baseline rather than built here, so
/// what is pinned is a shape the CLI really sent.
fn sample_message() -> Message {
    for dir in [baseline_dir(), legacy_baseline_dir()] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.strip_suffix(".jsonl").map(str::to_string)
            })
            .collect();
        names.sort();
        for name in names {
            let log = load_baseline_from(&dir, &name);
            for (at, line) in log.inbound().iter().enumerate() {
                if let DecodedLine::Message(message) = decode_dispatch(line, (at + 1) as u64) {
                    return message;
                }
            }
        }
    }
    panic!("no committed SDK baseline carries a decodable message");
}

struct Contract {
    records: Vec<(&'static str, String)>,
    frames: Vec<(&'static str, usize)>,
    update_payload_sampled: Vec<&'static str>,
    chat_keys: usize,
    chat_paths: usize,
    chat_nulls: usize,
    chat_empty_arrays: usize,
}

fn seat() -> SessionSlot {
    SessionSlot::lead("TestOrg", "proj")
}

/// The frame census, plus the one update payload this record samples.
///
/// The samples are what prove the derived wire names: every `ServerMessage`
/// and every `Subject` is built, encoded, and compared against the name the
/// census derives, so a change to either container's `rename_all` is caught
/// rather than trusted.
fn frames_record() -> Value {
    let seat = seat();
    let samples = [
        ServerMessage::Greeting {
            version: PROTOCOL_VERSION,
            settings: ClientSettings { mark: None, theme: None, font: None },
        },
        ServerMessage::Snapshot { subject: Subject::Home, data: Value::Null },
        ServerMessage::Update { update: Box::new(SessionUpdate::CatalogLoaded) },
        ServerMessage::Page { conversation: seat.clone(), turns: Vec::new(), cursor: None },
        ServerMessage::Reply { reply_to: 1, body: Value::Null },
        ServerMessage::Error { what: "what".to_owned(), why: "why".to_owned() },
    ];

    for sample in &samples {
        let encoded = serde_json::to_value(sample).expect("a captured frame encodes");
        let tag = encoded
            .get("kind")
            .and_then(Value::as_str)
            .expect("every server message is tagged on `kind`");
        assert_eq!(
            tag,
            wire_name(server_message_census(sample)),
            "the derived wire name is not the one the server writes"
        );
    }

    let subjects = [Subject::Home, Subject::Usage, Subject::Session(seat.clone())];
    for subject in &subjects {
        let encoded = serde_json::to_value(subject).expect("a subject encodes");
        assert_eq!(
            external_tag(&encoded),
            wire_name(subject_census(subject)),
            "a subject's wire name moved"
        );
    }

    let command = Command::OpenUrl { url: String::new() };
    let encoded = serde_json::to_value(&command).expect("a command encodes");
    assert_eq!(
        external_tag(&encoded),
        wire_name(command_census(&command)),
        "a command's wire name moved"
    );

    // The one update payload this record samples. #1321 lived here: the Rust
    // field is `actions` and the wire name is `hookCount`, and a fold reading
    // the Rust one draws nothing.
    let update_sample = SessionUpdate::ChatAppended { key: seat.clone(), msg: sample_message() };
    let encoded = serde_json::to_value(&update_sample).expect("the update sample encodes");
    assert_eq!(
        external_tag(&encoded),
        wire_name(session_update_census(&update_sample)),
        "an update's wire name moved"
    );

    let update = ServerMessage::Update { update: Box::new(update_sample) };
    let mut shape = Shape::default();
    shape.record(&serde_json::to_value(&update).expect("the sample encodes"), "");
    let mut sampled: BTreeMap<String, Value> = BTreeMap::new();
    for (path, keys) in &shape.paths {
        sampled.insert(path.clone(), json!(keys.iter().collect::<Vec<_>>()));
    }

    let mut ranked: BTreeMap<String, String> = BTreeMap::new();
    for rust_name in SESSION_UPDATE_VARIANTS {
        ranked.insert((*rust_name).to_owned(), wire_name(rust_name));
    }
    let mut commands: BTreeMap<String, String> = BTreeMap::new();
    for rust_name in COMMAND_VARIANTS {
        commands.insert((*rust_name).to_owned(), wire_name(rust_name));
    }
    let mut messages: BTreeMap<String, String> = BTreeMap::new();
    for rust_name in SERVER_MESSAGE_VARIANTS {
        messages.insert((*rust_name).to_owned(), wire_name(rust_name));
    }
    let mut subject_names: BTreeMap<String, String> = BTreeMap::new();
    for rust_name in SUBJECT_VARIANTS {
        subject_names.insert((*rust_name).to_owned(), wire_name(rust_name));
    }

    json!({
        "server_message": messages,
        "session_update": ranked,
        "subject": subject_names,
        "command": commands,
        "update_payload_sampled": { "ChatAppended": sampled },
    })
}

/// The `Message` field surface the chat fold reads, walked off the committed
/// SDK baselines.
///
/// Those baselines are the CLI's own bytes, and the socket re-sends the
/// decoded struct unchanged inside `chat_appended` - so this is the exact key
/// set a fold sees, taken from a live capture rather than from a schema.
fn chat_record() -> Shape {
    let mut shape = Shape::default();
    for dir in [baseline_dir(), legacy_baseline_dir()] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut names: Vec<String> = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                name.strip_suffix(".jsonl").map(str::to_string)
            })
            .collect();
        names.sort();
        for name in names {
            let log = load_baseline_from(&dir, &name);
            for (at, line) in log.inbound().iter().enumerate() {
                if let DecodedLine::Message(message) = decode_dispatch(line, (at + 1) as u64) {
                    let Ok(encoded) = serde_json::to_value(&message) else { continue };
                    shape.record(&encoded, "");
                }
            }
        }
    }
    shape
}

fn contract() -> Contract {
    let chat = chat_record();
    let mut chat_body: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (path, keys) in &chat.paths {
        chat_body.insert(path.as_str(), keys.iter().map(String::as_str).collect());
    }
    let chat_body = serde_json::to_string_pretty(&chat_body).expect("the chat record renders");

    let frames = serde_json::to_string_pretty(&frames_record()).expect("the frame record renders");

    Contract {
        records: vec![
            ("frames.json", format!("{frames}\n")),
            ("chat.json", format!("{chat_body}\n")),
        ],
        frames: vec![
            ("server_message", SERVER_MESSAGE_VARIANTS.len()),
            ("session_update", SESSION_UPDATE_VARIANTS.len()),
            ("subject", SUBJECT_VARIANTS.len()),
            ("command", COMMAND_VARIANTS.len()),
        ],
        update_payload_sampled: vec!["ChatAppended"],
        chat_keys: chat.keys(),
        chat_paths: chat.paths(),
        chat_nulls: chat.nulls.len(),
        chat_empty_arrays: chat.empty_arrays.len(),
    }
}

#[test]
fn the_committed_socket_contract_is_still_what_the_server_emits() {
    let built = contract();

    let classified: usize = built.frames.iter().map(|(_, count)| count).sum();
    let per_enum = built
        .frames
        .iter()
        .map(|(name, count)| format!("{name} {count}"))
        .collect::<Vec<_>>()
        .join(" / ");
    eprintln!(
        "socket contract: frames {classified} classified ({per_enum}) | \
         update payloads sampled {} of {} | \
         chat payload {} keys over {} paths, {} null, {} empty collections",
        built.update_payload_sampled.len(),
        SESSION_UPDATE_VARIANTS.len(),
        built.chat_keys,
        built.chat_paths,
        built.chat_nulls,
        built.chat_empty_arrays,
    );

    // The floor. A record that got emptied compares equal to nothing and
    // would otherwise report exactly as clean as a full one - the failure
    // `sdk_replay.rs` guards its own corpus against.
    for (enum_name, count) in &built.frames {
        assert!(
            *count > 0,
            "the {enum_name} census classified no variants, so every check over it is asserting \
             against an empty set"
        );
    }
    assert!(
        built.chat_paths > 0 && built.chat_keys > 0,
        "the chat record carries no path and no key, so a rename inside a fold payload is as \
         invisible as it was before the record existed"
    );

    let mut drifted: Vec<String> = Vec::new();
    for (name, body) in &built.records {
        let path = record_path(name);
        match std::fs::read_to_string(&path) {
            Ok(committed) if committed == *body => {}
            Ok(committed) => drifted.push(describe_drift(name, &committed, body)),
            Err(_) => drifted.push(format!("{name}: no record at {}", path.display())),
        }
    }

    assert!(
        drifted.is_empty(),
        "the socket contract moved and the record was not regenerated.\n\
         Regenerate with `just conformance-record-socket`, then read the diff against the \
         client: a field renamed here is a page that draws blank.\n\n{}",
        drifted.join("\n\n")
    );
}

/// Which lines moved, so a failure names the field rather than the file.
fn describe_drift(name: &str, committed: &str, fresh: &str) -> String {
    let committed: BTreeSet<&str> = committed.lines().collect();
    let fresh: BTreeSet<&str> = fresh.lines().collect();
    let gone: Vec<&&str> = committed.difference(&fresh).take(12).collect();
    let added: Vec<&&str> = fresh.difference(&committed).take(12).collect();
    format!(
        "{name}: {} line(s) gone, {} added\n  gone:  {gone:#?}\n  added: {added:#?}",
        committed.difference(&fresh).count(),
        fresh.difference(&committed).count(),
    )
}

/// Writes the records from the current code. Run deliberately:
/// `just conformance-record-socket`.
///
/// **What this writes is only what the code currently emits**, so a record
/// produced here pins the present shape and a wrong one equally. Each has to
/// be READ against the intended shape before it is committed; generating one
/// is not the work.
#[test]
#[ignore = "writes the records; run deliberately"]
fn write_the_socket_records() {
    let built = contract();
    std::fs::create_dir_all(record_dir()).expect("create the record directory");
    for (name, body) in &built.records {
        let path = record_path(name);
        std::fs::write(&path, body).expect("write the record");
        eprintln!("wrote {}", path.display());
    }
}
