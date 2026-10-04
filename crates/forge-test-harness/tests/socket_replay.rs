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
//! - The FRAME census is one list per enum, expanded into both a `match` with
//!   no wildcard arm and the names the record is built from. One list rather
//!   than two, because two is how the record comes to state a wire name the
//!   server no longer sends while everything stays green - the failure this
//!   artifact exists for, one level up. A variant added, removed or renamed is
//!   a compile error before it is ever a failing test.
//! - The FIELD record is a path-and-key walk over encoded values. A renamed
//!   field moves a line in `chat.json`.
//!
//! **What this cannot see, stated rather than implied.** A payload is pinned
//! only for the frames sampled below, and the fields inside a variant nobody
//! samples are not pinned at all. `Task` and `CronEntry` have no fixture
//! behind them, which `wire.rs` counts and names on every run.
//!
//! **Nothing here reads the client.** The records say what the server emits;
//! whether a page reads those names is a reader comparing the two, not a
//! check. That comparison is the whole point of regenerating deliberately.
//!
//! **One dependency worth naming.** The wire names come from serde's own
//! unknown-variant error, which is a message serde formats rather than a
//! contract it promises. It fails closed: a parse that finds no
//! `expected one of` clause yields nothing, and the emptiness assertion
//! fires rather than the comparison passing against an empty set.
//!
//! **A SPLIT rename is the one naming change no name check here can read.**
//! serde can name a variant one way on the way out and another on the way
//! in, and the probe asks the deserialize side while the record is built
//! from Rust names - so the record would state a tag the server does not
//! send, green, and a client reading what serde WRITES draws blank. Only the
//! sampled frames catch it, for their own payloads. `assert_no_split_renames`
//! asserts the shape is absent from both sources carrying these enums - and
//! from `messages.rs`, which carries none of them and is scanned for the chat
//! payload's sake - which is complete for it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use forge_primitives::{Message, SessionSlot};
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use forge_server::transport::PROTOCOL_VERSION;
use forge_server::transport::envelope::{ClientMessage, ClientSettings, ServerMessage, Subject};
use forge_server::transport::wire::{DeviceWire, TurnWire};
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

/// The census: **one list per enum, expanded twice.**
///
/// Two lists is how this drifts, and it is the failure the artifact exists
/// for, so there is one. The same invocation emits the `match` the compiler
/// checks and the `&[&str]` the record is built from, which means a variant
/// the server renames has to be fixed in exactly one place and both move
/// together. A `match` with no wildcard arm makes a variant ADDED or REMOVED
/// a compile error, and a pattern naming a variant that no longer exists is
/// one too, so the list cannot disagree with the enum in either direction.
/// `stringify!` makes the names the variants' own rather than retyped.
///
/// **The list is in declaration order, and that is load-bearing.** serde
/// reports the names it accepts in the order the variants are declared, and
/// `assert_serde_names!` compares the two as a SEQUENCE rather than a set:
/// a set cannot see two variants swapping names with each other, which
/// leaves the record stating the opposite of what the server sends. Each
/// entry carries its own kind, because a tuple variant needs a different
/// pattern and an interleaved list is the only way to keep the order.
macro_rules! census {
    ($enum:ident,
     [$($v:ident $kind:ident),* $(,)?],
     $census:ident, $names:ident) => {
        census!(@arms $enum, $census; [] $($v $kind,)*);

        const $names: &[&str] = &[ $( stringify!($v), )* ];
    };

    // The arms are accumulated here rather than written in place, so one
    // interleaved list can carry both kinds: a struct variant and a tuple
    // variant need different patterns, and a `match` cannot be handed a
    // choice of pattern by a nested macro.
    (@arms $enum:ident, $census:ident; [$($arms:tt)*]) => {
        fn $census(value: &$enum) -> &'static str {
            match value {
                $($arms)*
            }
        }
    };
    (@arms $enum:ident, $census:ident; [$($arms:tt)*] $v:ident struct, $($rest:tt)*) => {
        census!(@arms $enum, $census;
            [$($arms)* $enum::$v { .. } => stringify!($v),] $($rest)*);
    };
    (@arms $enum:ident, $census:ident; [$($arms:tt)*] $v:ident tuple, $($rest:tt)*) => {
        census!(@arms $enum, $census;
            [$($arms)* $enum::$v(..) => stringify!($v),] $($rest)*);
    };
}

/// The wire names serde will accept, read out of the error it raises for a
/// tag no variant carries.
///
/// Each name arrives backticked, and the last one carries the error's own
/// position after it, so the token between the backticks is what is taken.
/// A parse that found nothing returns empty rather than a junk name, and the
/// caller fails on that rather than comparing against an empty set.
fn serde_names(error: &str) -> Vec<String> {
    error
        .split_once("expected one of ")
        .map(|(_, listed)| {
            listed
                .split(", ")
                .filter_map(|piece| {
                    let piece = piece.trim().trim_start_matches('`');
                    piece.split_once('`').map(|(name, _)| name.to_owned())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Assert that the sources carrying these enums hold no SPLIT renaming.
///
/// **This is the one naming change every other check in this file is blind
/// to.** serde can name a variant one way on the way out and another on the
/// way in - `#[serde(rename(serialize = "a", deserialize = "b"))]`, or the
/// same split on a container's `rename_all`. The accepted-name probe asks
/// serde's DESERIALIZE side, because that is the side its error comes from;
/// the record is built from Rust names. So a split rename leaves the record
/// stating a tag the server does not send, green, and a client - which reads
/// what serde WRITES - draws blank.
///
/// The sampled frames catch it for their own payloads and nothing else
/// does, so the shape is asserted against the source instead, which is
/// complete for it. Whitespace is stripped first, because
/// `rename ( serialize` is the same attribute.
fn assert_no_split_renames() {
    const SPLIT: &[&str] = &[
        "rename(serialize",
        "rename_all(serialize",
        "rename(deserialize",
        "rename_all(deserialize",
    ];
    for (name, source) in [
        ("envelope.rs", include_str!("../../forge-server/src/transport/envelope.rs")),
        ("protocol.rs", include_str!("../../forge-workspace/src/protocol.rs")),
        ("messages.rs", include_str!("../../forge-primitives/src/messages.rs")),
    ] {
        let flat: String = source.chars().filter(|ch| !ch.is_whitespace()).collect();
        for pattern in SPLIT {
            assert!(
                !flat.contains(pattern),
                "{name} carries a `{pattern}` renaming, which names a wire value one way on the \
                 way out and another on the way in. The record is built from Rust names and the \
                 serde probe asks the deserialize side, so the record would state a tag the \
                 server does not send while every check here passed."
            );
        }
    }
}

/// Assert that the names this record carries are the names serde accepts,
/// **in the same order**.
///
/// **`wire_name(stringify!(Variant))` is an assumption**, not a reading: it
/// says a variant's wire name is its Rust name under `rename_all`. For a
/// while the only check on it was the eleven variants a sample is built for,
/// which left the rest stating a tag the server might not send - and a
/// variant-level `#[serde(rename)]` produced exactly that, green, with a
/// regeneration that diffed clean because both sides used the same
/// assumption.
///
/// **The comparison is a sequence, not a set.** The error names the tags
/// serde accepts without saying which variant each belongs to, so a set
/// cannot see two variants swapping names with each other - and a swap is
/// the one attack that leaves the record stating the opposite of what the
/// server sends while every name in it is a real one. serde lists them in
/// declaration order, so each census list is kept in that order.
///
/// **A failure here is one of two things**, and the message says so: a name
/// moved, or the census list is out of declaration order. Reordering the
/// list to clear the message is the one repair that would lose the check.
macro_rules! assert_serde_names {
    ($probe:expr, $enum:ty, $names:expr) => {{
        let Err(error) = serde_json::from_str::<$enum>($probe) else {
            panic!("{}: a tag no variant carries decoded", stringify!($enum));
        };
        let accepted = serde_names(&error.to_string());
        assert!(
            !accepted.is_empty(),
            "{}: serde named no variant, so this comparison proved nothing",
            stringify!($enum)
        );
        let recorded: Vec<String> = $names.iter().map(|name| wire_name(name)).collect();
        assert_eq!(
            accepted,
            recorded,
            "{}: the wire names serde accepts are not the ones this record carries, in the \
             order it carries them, so the record states a tag the server does not send. \
             Either a name moved, or this census list is out of the enum's declaration order \
             - reordering the list to silence this is the repair that loses the check.",
            stringify!($enum)
        );
    }};
}

census!(ServerMessage,
    [
        Greeting struct, Snapshot struct, Update struct, Page struct,
        Devices struct, Reply struct, Error struct,
    ],
    server_message_census, SERVER_MESSAGE_VARIANTS);

census!(Subject,
    [Home struct, Session tuple, Usage struct],
    subject_census, SUBJECT_VARIANTS);

census!(SessionUpdate,
    [
        Spawning struct, Connected struct, HistoryReplayed struct, SessionReplaced struct,
        ConnectionFailed struct,
        AuthRequired struct, SlashCommandError struct, Notice struct,
        RuntimeReloadCompleted struct,
        RuntimeReloadFailed struct, SetModeFailed struct, SetModelFailed struct,
        PermissionRequest struct, QuestionRequest struct, PendingInteractionResolved struct,
        McpOperationError struct, TurnComplete struct, TurnCancelled struct, TurnError struct,
        ChatAppended struct, HookObservation struct, StatusSnapshot struct,
        ForgeAccountIdentity struct, DictateOverrides struct, DictateDevicePin struct,
        OauthCredentialsSnapshot struct, ContextUsageSnapshot struct, McpSnapshot struct,
        ProcessesChanged struct, MonitorsChanged struct, BackgroundTasksChanged struct,
        WorkChanged struct, SlashCommandsChanged struct, SubagentsChanged struct,
        DispatchesChanged struct, FileIndexChanged struct,
        SessionsListed struct, ServiceStatus struct, CatalogLoaded struct,
        CliVersionChanged struct, AccountsChanged struct, PluginsInventoryUpdated struct,
        PluginsInventoryRefreshFailed struct, PluginsCliActionSucceeded struct,
        PluginsCliActionFailed struct, PluginsUpdateRunProgress struct,
        PluginsUpdateRunFinished struct, PluginsRollbackSucceeded struct,
        PluginsRollbackFailed struct, WorkerStatusChanged struct,
        PeerEnvelopeAppended struct,
        GotifyNotificationAppended struct, CronPromptAppended struct,
        SlackMessageAppended struct, SlackPostPending struct, SlackDraftResolved struct,
        PromptQueuedWhileBusy struct, PromptQueued struct, PromptLifecycle struct,
        PromptCancelResolved struct,
        ReviewActivityNotice struct, DictateAvailability struct,
        DictateStarted struct, DictateLevel struct, DictateTranscribing struct,
        DictateProgress struct, DictateEnded struct, FatalError tuple,
    ],
    session_update_census, SESSION_UPDATE_VARIANTS);

census!(Command,
    [
        Prompt struct, PromptUnder struct, Cancel struct, CancelQueuedPrompt struct,
        ReplayConversation struct, SetMode struct, SetModel struct,
        NewSession struct,
        ResumeSession struct, RespondPermission struct, RespondSlackPost struct,
        RespondQuestion struct, SetDictateOverride struct, ResetDictateOverrides struct,
        SetDictateDevice struct, ReconnectMcpServer struct, ToggleMcpServer struct,
        SpawnProject struct, SpawnSession struct, StartDefault struct, DeliverPeerPrompt struct,
        SpawnWorker struct, CloseWorker struct, OpenUrl struct, DespawnWorker struct,
        DeliverWorkerPrompt struct, DeliverWorkerPromptToLead struct,
        DeliverGotifyMessage struct, DictateStart struct, DictateStream struct,
        DictateStop struct,
        SaveReviewThreads struct, RemoveReviewThread struct, SetReviewThreadStatus struct,
        CloseSession struct, UpsertReviewThread struct, SubmitReview struct,
    ],
    command_census, COMMAND_VARIANTS);

// What a client sends. Easy to leave out and it crosses the socket: the
// client writes these tags by hand, so a rename here fails nothing until the
// command does nothing when it is pressed.
census!(ClientMessage,
    [Subscribe struct, Unsubscribe struct, Command struct, More struct, Devices struct],
    client_message_census, CLIENT_MESSAGE_VARIANTS);

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
                    return *message;
                }
            }
        }
    }
    panic!("no committed SDK baseline carries a decodable message");
}

struct Contract {
    records: Vec<(&'static str, String)>,
    frames: Vec<(&'static str, usize)>,
    update_payload_sampled: Vec<String>,
    payload_frames_sampled: Vec<String>,
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

    assert_no_split_renames();

    // Every name this record carries, against the names serde accepts. The
    // samples below check the same thing for eleven variants by encoding a
    // value; this checks all of them, which is the part a `rename` could
    // otherwise move while the record stayed green.
    assert_serde_names!(r#"{"kind":"__forge_probe__"}"#, ServerMessage, SERVER_MESSAGE_VARIANTS);
    assert_serde_names!(r#"{"kind":"__forge_probe__"}"#, ClientMessage, CLIENT_MESSAGE_VARIANTS);
    assert_serde_names!(r#""__forge_probe__""#, Subject, SUBJECT_VARIANTS);
    assert_serde_names!(r#""__forge_probe__""#, SessionUpdate, SESSION_UPDATE_VARIANTS);
    assert_serde_names!(r#""__forge_probe__""#, Command, COMMAND_VARIANTS);

    let samples = [
        ServerMessage::Greeting {
            version: PROTOCOL_VERSION,
            settings: ClientSettings {
                mark: None,
                theme: None,
                font: None,
                dictate: forge_workspace::DictateAxes::default(),
            },
        },
        ServerMessage::Snapshot { subject: Subject::Home, data: Value::Null },
        ServerMessage::Update { update: Box::new(SessionUpdate::CatalogLoaded) },
        ServerMessage::Page { conversation: seat.clone(), turns: Vec::new(), cursor: None },
        ServerMessage::Reply { reply_to: 1, body: Value::Null },
        ServerMessage::Error {
            what: "what".to_owned(),
            why: "why".to_owned(),
            // Named, which is the shape a `more` refusal carries: the field is
            // what lets a client holding several seats' asks drain only its
            // own.
            seat: Some(seat.clone()),
        },
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

    // All five, because they are cheap and the client writes these tags by
    // hand: nothing else in the tree would notice one moving.
    let client_messages = [
        ClientMessage::Subscribe { what: Subject::Home, answering: false },
        ClientMessage::Unsubscribe { what: Subject::Home },
        ClientMessage::Command {
            command: Box::new(Command::OpenUrl { url: String::new() }),
            reply_to: None,
        },
        ClientMessage::More { conversation: seat.clone(), before: None, turns: 1 },
        ClientMessage::Devices,
    ];
    for sample in &client_messages {
        let encoded = serde_json::to_value(sample).expect("a client message encodes");
        let tag = encoded
            .get("kind")
            .and_then(Value::as_str)
            .expect("every client message is tagged on `kind`");
        assert_eq!(
            tag,
            wire_name(client_message_census(sample)),
            "a client message's wire name moved"
        );
    }

    // The payloads this record samples. `ChatAppended` is #1321's: the
    // Rust field is `actions` and the wire name is `hookCount`, and a fold
    // reading the Rust one draws nothing. `Page` is what the client pages on,
    // and an empty page pins nothing about a turn.
    let update_sample =
        SessionUpdate::ChatAppended { key: seat.clone(), msg: sample_message(), origin: None };
    let encoded = serde_json::to_value(&update_sample).expect("the update sample encodes");
    assert_eq!(
        external_tag(&encoded),
        wire_name(session_update_census(&update_sample)),
        "an update's wire name moved"
    );

    let mut payload_sampled: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    payload_sampled.insert(
        "ChatAppended".to_owned(),
        shape_of(&ServerMessage::Update { update: Box::new(update_sample) }),
    );
    payload_sampled.insert(
        "Page".to_owned(),
        shape_of(&ServerMessage::Page {
            conversation: seat.clone(),
            turns: vec![TurnWire {
                key: Some("turn-1".to_owned()),
                messages: vec![json!({ "type": "user" })],
            }],
            cursor: Some("message-1".to_owned()),
        }),
    );
    // The greeting, because its settings gained the dictate axes a
    // capturing client starts on: without a sample, the new key is as
    // invisible to this record as it was before the record existed.
    payload_sampled.insert(
        "Greeting".to_owned(),
        shape_of(&ServerMessage::Greeting {
            version: PROTOCOL_VERSION,
            settings: ClientSettings {
                mark: None,
                theme: None,
                font: None,
                dictate: forge_workspace::DictateAxes::default(),
            },
        }),
    );
    payload_sampled.insert(
        "Devices".to_owned(),
        shape_of(&ServerMessage::Devices {
            devices: vec![DeviceWire {
                id: "a-mic".to_owned(),
                name: "Studio Mic".to_owned(),
                is_default: true,
            }],
            configured: Some("a-mic".to_owned()),
        }),
    );

    json!({
        "server_message": named(SERVER_MESSAGE_VARIANTS),
        "session_update": named(SESSION_UPDATE_VARIANTS),
        "subject": named(SUBJECT_VARIANTS),
        "command": named(COMMAND_VARIANTS),
        "client_message": named(CLIENT_MESSAGE_VARIANTS),
        "payload_sampled": payload_sampled,
        "command_sampled": command_sampled(&seat),
        "dictate_frame": dictate_frame_record(),
    })
}

/// The commands a client writes by hand, and the fields it writes them with.
///
/// A `Command` rides INSIDE a `client_message`, so the census above pins only
/// its name - a field renamed on the server would leave a client's own literal
/// writing a key nothing reads, with every suite green on both sides.
fn command_sampled(seat: &SessionSlot) -> BTreeMap<String, BTreeMap<String, Value>> {
    let mut sampled: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    let mut shape = Shape::default();
    shape.record(
        &serde_json::to_value(Command::DictateStream {
            key: seat.clone(),
            options: forge_workspace::DictateAxes::default(),
        })
        .expect("a command encodes"),
        "",
    );
    sampled.insert(
        "DictateStream".to_owned(),
        shape
            .paths
            .iter()
            .map(|(path, keys)| (path.clone(), json!(keys.iter().collect::<Vec<_>>())))
            .collect(),
    );
    sampled
}

/// The dictate frame's byte-level contract, asserted and then recorded.
///
/// A frame is raw bytes rather than a serde enum, so this pins the two
/// things a reader can check about it: the header and cap the server
/// enforces, and a fixed vector whose BYTES decode to the samples they
/// carry, which is what pins byte order and scaling together. The server
/// only decodes, so the fixture runs that way: bytes in, samples out.
fn dictate_frame_record() -> Value {
    use forge_server::transport::frame::{self, HEADER_BYTES, MAX_PAYLOAD_BYTES};

    // The fixture: one of each interesting sample, little-endian i16 on the
    // wire. A big-endian read of these bytes, or a /32767 scale, moves the
    // decoded samples and fails the assertion below.
    const FIXTURE: &[i16] = &[0, 16384, -32768, 8192, -8192, 32767, -16384, 4096];

    // Every codec, one list expanded twice: the `match` has no wildcard
    // arm, so a codec added or removed is a compile error here before it is
    // a failing record.
    let codecs: Vec<Value> = [frame::Codec::PcmI16]
        .into_iter()
        .map(|codec| {
            let name = match codec {
                frame::Codec::PcmI16 => "PcmI16",
            };
            json!({ "name": name, "tag": codec.tag() })
        })
        .collect();

    let mut bytes = vec![frame::Codec::PcmI16.tag()];
    for sample in FIXTURE {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    let expected: Vec<f32> = FIXTURE.iter().map(|&s| f32::from(s) / 32768.0).collect();

    let decoded = frame::decode(&bytes).expect("the recorded fixture is a frame this server takes");
    assert_eq!(
        decoded.samples, expected,
        "the recorded fixture's bytes must decode to the samples they carry"
    );

    let hex = bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    });

    json!({
        "header_bytes": HEADER_BYTES,
        "max_payload_bytes": MAX_PAYLOAD_BYTES,
        "codecs": codecs,
        "fixture": { "hex": hex, "samples": expected },
    })
}

/// An enum's variants, each against the wire name it encodes as.
fn named(variants: &[&str]) -> BTreeMap<String, String> {
    variants.iter().map(|rust_name| ((*rust_name).to_owned(), wire_name(rust_name))).collect()
}

/// One sample frame's path-and-key shape, as the record writes it.
fn shape_of(frame: &ServerMessage) -> BTreeMap<String, Value> {
    let mut shape = Shape::default();
    shape.record(&serde_json::to_value(frame).expect("the sample encodes"), "");
    shape
        .paths
        .iter()
        .map(|(path, keys)| (path.clone(), json!(keys.iter().collect::<Vec<_>>())))
        .collect()
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

    let frames_record = frames_record();

    // **Read off the record rather than written beside it.** A literal here
    // is a denominator asserted rather than measured: adding a third sampled
    // payload moved the record and left the printed line still saying one,
    // which is the same defect as a counter that reads a shape it did not
    // build.
    let payload_frames_sampled: Vec<String> = frames_record
        .get("payload_sampled")
        .and_then(Value::as_object)
        .map(|sampled| sampled.keys().cloned().collect())
        .unwrap_or_default();

    // Of those, the ones that are an `SessionUpdate`. **Both figures print**:
    // the map's own total is what a sample for another enum moves, and the
    // update count is the one with a denominator of its own. Reporting only
    // the second left a sample added for `ServerMessage` moving the record
    // with the printed line unchanged.
    let update_payload_sampled: Vec<String> = payload_frames_sampled
        .iter()
        .filter(|name| SESSION_UPDATE_VARIANTS.contains(&name.as_str()))
        .cloned()
        .collect();

    let frames = serde_json::to_string_pretty(&frames_record).expect("the frame record renders");

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
            ("client_message", CLIENT_MESSAGE_VARIANTS.len()),
        ],
        update_payload_sampled,
        payload_frames_sampled,
        chat_keys: chat.keys(),
        chat_paths: chat.paths(),
        chat_nulls: chat.nulls.len(),
        chat_empty_arrays: chat.empty_arrays.len(),
    }
}

#[test]
fn the_committed_socket_contract_is_still_what_the_server_emits() {
    let built = contract();
    report(&built);
    if let Err(degraded) = floors(&built) {
        panic!("{degraded}");
    }

    let mut drifted: Vec<String> = Vec::new();
    for (name, body) in &built.records {
        let path = record_path(name);
        match std::fs::read_to_string(&path) {
            Ok(committed) if committed == *body => {}
            Ok(committed) => drifted.push(describe_drift(name, &committed, body)),
            // A record that is there and unreadable is a different fault from
            // one that is missing, and the error says which.
            Err(error) => drifted.push(format!(
                "{name}: cannot be read at {} ({error}), so nothing was compared",
                path.display()
            )),
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

/// What this run pinned. Printed rather than asserted silently, because a
/// coverage number nobody sees is the same as not having one.
fn report(built: &Contract) {
    let classified: usize = built.frames.iter().map(|(_, count)| count).sum();
    let per_enum = built
        .frames
        .iter()
        .map(|(name, count)| format!("{name} {count}"))
        .collect::<Vec<_>>()
        .join(" / ");
    eprintln!(
        "socket contract: frames {classified} classified ({per_enum}) | \
         payload frames sampled {} (update payloads {} of {}) | \
         chat payload {} keys over {} paths, {} null, {} empty collections",
        built.payload_frames_sampled.len(),
        built.update_payload_sampled.len(),
        SESSION_UPDATE_VARIANTS.len(),
        built.chat_keys,
        built.chat_paths,
        built.chat_nulls,
        built.chat_empty_arrays,
    );
}

/// The floor. A record that lost its rows compares equal to less and would
/// otherwise report exactly as clean as a full one - the failure
/// `sdk_replay.rs` guards its own corpus against.
///
/// Run by the writer as well as the reader, so a degraded record fails where
/// it is made rather than only where it is read.
fn floors(built: &Contract) -> Result<(), String> {
    for (enum_name, count) in &built.frames {
        if *count == 0 {
            return Err(format!(
                "the {enum_name} census classified no variants, so every check over it is \
                 asserting against an empty set"
            ));
        }
    }
    if built.chat_paths == 0 || built.chat_keys == 0 {
        return Err(
            "the chat record carries no path and no key, so a rename inside a fold payload is as \
             invisible as it was before the record existed"
                .to_owned(),
        );
    }
    // The sampled counts have a floor for the same reason the others do, and
    // this one had none: a map holding no update payload printed `sampled 0
    // of 56`, which reads as a measurement rather than as a record that
    // samples nothing. The writer runs these, so it is what stops a
    // sample-free record reaching disk.
    if built.payload_frames_sampled.is_empty() {
        return Err("the record samples no payload frame at all, so every count it prints is a \
             denominator against nothing"
            .to_owned());
    }
    if built.update_payload_sampled.is_empty() {
        return Err(
            "the record samples no UPDATE payload, so the `N of 56` it prints is a denominator \
             against nothing - which reads as a measurement rather than as a record that samples \
             none"
                .to_owned(),
        );
    }
    Ok(())
}

/// Which lines moved, so a failure names the field rather than the file.
///
/// The totals come first because they are the load-bearing number: a rename
/// that ALSO stops a frame decoding does not read as one name swapped for
/// another, it reads as a whole payload leaving the corpus and a generic
/// bucket arriving in its place. Without the totals that shape is invisible,
/// and a total of zero for a record full of names is worse than no total.
fn describe_drift(name: &str, committed: &str, fresh: &str) -> String {
    let committed_lines: BTreeSet<&str> = committed.lines().collect();
    let fresh_lines: BTreeSet<&str> = fresh.lines().collect();
    let gone: Vec<&&str> = committed_lines.difference(&fresh_lines).take(12).collect();
    let added: Vec<&&str> = fresh_lines.difference(&committed_lines).take(12).collect();
    format!(
        "{name}: {} name(s) pinned -> {}\n  {} line(s) gone:  {gone:#?}\n  {} line(s) added: {added:#?}",
        pinned(committed),
        pinned(fresh),
        committed_lines.difference(&fresh_lines).count(),
        fresh_lines.difference(&committed_lines).count(),
    )
}

/// How many names a record pins. `unreadable` when the JSON will not parse,
/// which must not read as "nothing pinned".
///
/// **Walked rather than matched against the shapes it happens to have.** An
/// earlier version of this counted one shape and returned zero for the
/// other, which is a denominator that lies quietly; a shallow version of the
/// same mistake then counted the census sections of `frames.json` and missed
/// the payload section nested one level deeper. A name is a string, a list
/// of names is an array, and everything else is a container to descend.
fn pinned(body: &str) -> String {
    fn count(value: &Value) -> usize {
        match value {
            Value::Array(names) => names.len(),
            Value::String(_) => 1,
            Value::Object(entries) => entries.values().map(count).sum(),
            _ => 0,
        }
    }
    match serde_json::from_str::<Value>(body) {
        Ok(value) => count(&value).to_string(),
        Err(_) => "unreadable".to_owned(),
    }
}

/// Writes the records from the current code. Run deliberately:
/// `just conformance-record-socket`.
///
/// **What this writes is only what the code currently emits**, so a record
/// produced here pins the present shape and a wrong one equally. Each has to
/// be READ against the intended shape before it is committed; generating one
/// is not the work. The floors run here too, so a degraded record fails at
/// the write rather than in whichever run reads it next.
///
/// Each file is written beside its record and renamed over it, so a failure
/// anywhere in the write leaves the record it found rather than half a new
/// one. The staging name carries this process's id, because two
/// regenerations sharing one name would have one clobber the other's bytes
/// and the clean writer failing on a file the other had already moved.
#[test]
#[ignore = "writes the records; run deliberately"]
fn write_the_socket_records() {
    let built = contract();
    floors(&built).expect("the record is degraded, so writing it would pin the wrong shape");
    std::fs::create_dir_all(record_dir()).expect("create the record directory");
    for (name, body) in &built.records {
        let path = record_path(name);
        let staging = path.with_extension(format!("json.{}.new", std::process::id()));
        std::fs::write(&staging, body).expect("write the staged record");
        std::fs::rename(&staging, &path).expect("move the record into place");
        eprintln!("wrote {}", path.display());
    }
}
