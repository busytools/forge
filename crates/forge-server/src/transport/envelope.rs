//! What a client and this server exchange.
//!
//! Both enums are tagged on `kind`, which is added beside a message's own
//! fields rather than replacing anything, so a reader can tell what a
//! message is without knowing its shape. **The tag is the contract**:
//! adding a variant is compatible and renaming one is not.

use forge_primitives::{ClientConfig, SessionSlot};
use forge_workspace::DictateAxes;
use serde::{Deserialize, Serialize};

use crate::live::fleet_news;
use crate::{Command, SessionUpdate};

/// What a client can watch. A subscription names one of these and is
/// answered with that subject's snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    Home,
    Session(SessionSlot),
    /// The token/cost pool, scanned on the ask.
    ///
    /// It belongs to no seat - the report is the whole session-JSONL pool,
    /// bucketed by project and model - and no update carries it: the pool
    /// changes as transcripts are written, and nothing in the stream says a
    /// transcript's tokens moved. So a subscription is answered with one scan
    /// and hears nothing after it, and a client asks again by subscribing
    /// again.
    Usage,
    /// The models page's read: the pinned models, the catalogue check,
    /// the feed's rows and the updates it proposes.
    ///
    /// Carried as a subject of its own rather than inside `Home` because
    /// the rows are the whole catalogue and only the models page draws
    /// them. Unlike `Usage` it does hear updates: a check landing, boot's
    /// or a page's own, arrives as `SessionUpdate::DictateModelsChanged`.
    DictateModels,
}

impl Subject {
    /// Whether an update belongs to what this subscription asked for.
    ///
    /// A session subscription is a seat, so the workspace is ASKED which seat
    /// a variant routes to rather than this matching the variants itself.
    ///
    /// A home subscription is a row per seat plus the App-level facts, and it
    /// takes the classification that already exists for the row half:
    /// [`fleet_news`], which is what the view folding this stream draws the
    /// fleet from. It is narrow on purpose - a row states a seat's lifecycle,
    /// its status and its pending state, and no row shows a word of its
    /// conversation - so a second table of variants here would both drift and
    /// hand a home subscriber every token of every seat in the fleet.
    ///
    /// **The fleet region is not the whole home.** Two variants carry no slot,
    /// are not fleet news and are fields of the home's own snapshot - the
    /// service status and the fatal error - so `fleet_news` alone would drop
    /// them, leaving a client to draw what it read once at subscribe for the
    /// life of the connection. They come with the slot-less arm below. (The
    /// plugin records are slot-less too and reach no home subscriber: the
    /// terminal folds them onto a channel of its own, so the home's snapshot
    /// carries them for a client rather than a stream.)
    pub fn covers(&self, update: &SessionUpdate) -> bool {
        match self {
            Self::Home => fleet_news(update).any() || update.slot().is_none(),
            Self::Session(seat) => update.slot() == Some(seat),
            // Nothing carries the pool: its snapshot is a scan, and a
            // transcript's tokens moving is not an update any variant
            // announces.
            Self::Usage => false,
            // The check's own landing, and the load's signal: a page open
            // during a first 3 GB download hears models finished loading
            // through `DictateAvailability` and reads again, so its state
            // chips are not stuck on `pending` until the next check. The
            // feed belongs to no seat, so nothing else reaches here.
            Self::DictateModels => matches!(
                update,
                SessionUpdate::DictateModelsChanged { .. } | SessionUpdate::DictateAvailability
            ),
        }
    }
}

/// What a client sends.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientMessage {
    Subscribe {
        what: Subject,
        /// Whether this client can answer the prompts it is shown.
        ///
        /// Off unless the client says otherwise, because the core parks a
        /// turn on the reply of whoever registered as answering: a client
        /// counted as able to answer a prompt it cannot display hangs the
        /// turn rather than failing it. A client with a dock to answer from
        /// says so here; the connection registers with the core accordingly
        /// before it forwards anything.
        #[serde(default)]
        answering: bool,
    },
    Unsubscribe {
        what: Subject,
    },
    /// `reply_to` is set only when the client wants an answer - which is the
    /// four commands whose reply rides a oneshot, and nothing else.
    ///
    /// Boxed because a command is 360 bytes and every other variant here is
    /// a fraction of that (clippy::large_enum_variant). Serde writes a box
    /// as the value inside it, so the wire form is unchanged.
    Command {
        command: Box<Command>,
        reply_to: Option<u64>,
    },
    More {
        conversation: SessionSlot,
        before: Option<String>,
        turns: u32,
    },
    /// The inputs forge can record from, and the `[dictate] device` pin.
    ///
    /// Asked on demand rather than subscribed: the walk opens the microphone
    /// stack, and a subscription is re-read on every reconnect, so watching it
    /// would be a permission check per connection instead of one per picker.
    Devices,
}

/// What the server sends.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    Greeting {
        version: u32,
        /// The build that sent this, in the two forms the terminal's own
        /// header draws. The greeting is the only thing a client is
        /// guaranteed to have before it reads or refuses anything, so a skew
        /// can name both halves from here and nowhere else.
        forge_version: String,
        forge_version_short: String,
        settings: ClientSettings,
    },
    Snapshot {
        subject: Subject,
        data: serde_json::Value,
    },
    /// Boxed for the same reason as a command: the update is the largest
    /// thing on this wire and every other variant is small beside it.
    Update {
        update: Box<SessionUpdate>,
    },
    /// A page of one conversation, in answer to `more`: whole turns, each with
    /// the key the server named it by and the messages it ran as. Grouping a
    /// run of calls inside a turn is the client's, so the units the server
    /// folded do not cross.
    Page {
        conversation: SessionSlot,
        turns: Vec<crate::transport::wire::TurnWire>,
        cursor: Option<String>,
    },
    /// The answer to `devices`: every input forge can record from, and the pin
    /// the config sets. A walk that could not enumerate comes back as an
    /// `error` naming `devices` instead, because a client renders the refusal
    /// where the list would have been.
    Devices {
        devices: Vec<crate::transport::wire::DeviceWire>,
        /// The `[dictate] device` pin, and ONLY that: a pick replaces it for
        /// the rest of the run and rides the home snapshot's `dictate.device`,
        /// so this is not what is in force once anything has picked.
        configured: Option<String>,
    },
    /// The answer to a `Command` that carried a `reply_to`, including a
    /// refusal, so a client awaiting one is never left watching a channel
    /// that stays empty.
    Reply {
        reply_to: u64,
        body: serde_json::Value,
    },
    Error {
        what: String,
        why: String,
        /// The conversation the refusal belongs to, where it belongs to one: a
        /// `more` for a seat the server could not answer names its seat, so a
        /// client holding several seats' asks drains only its own. Optional and
        /// additive - a client of either age reads it, and a refusal that names
        /// no seat is read the way it always was.
        seat: Option<SessionSlot>,
    },
}

/// What the client draws with, from `forge.toml` by way of the server.
///
/// The client never reads the config file: `[client] mark`, `[client] theme`
/// and `[client] font` arrive here, and `None` on any of them means the
/// built-in rather than a pinned value - a commented line in a hand-authored
/// file has to mean "unset".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ClientSettings {
    pub mark: Option<String>,
    pub theme: Option<String>,
    pub font: Option<String>,
    /// The axes a client that captures starts on and resets to: the
    /// `[dictate]` keys, over the crate's own defaults. A client that
    /// captures sends the values in force with each take, so this is the
    /// default it departs from rather than anything the server remembers.
    pub dictate: DictateAxes,
}

impl ClientSettings {
    /// The client's settings: the `[client]` keys off the config, and the
    /// axes the workspace resolved.
    pub fn new(client: &ClientConfig, dictate: DictateAxes) -> Self {
        Self {
            mark: client.mark.clone(),
            theme: client.theme.clone(),
            font: client.font.clone(),
            dictate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The on-demand read a picker makes, and the answer it gets.
    ///
    /// Asked rather than subscribed: the walk opens the microphone stack, and
    /// a subscription is re-read on every reconnect, so watching it would be a
    /// permission check per connection rather than one per picker.
    #[test]
    fn a_devices_request_and_its_answer_round_trip_through_json() {
        let json = serde_json::to_string(&ClientMessage::Devices).expect("encode");
        assert!(json.contains("\"kind\":\"devices\""), "{json}");
        let back: ClientMessage = serde_json::from_str(&json).expect("decode");
        assert!(matches!(back, ClientMessage::Devices), "{json} decoded into another message");

        let answer = ServerMessage::Devices {
            devices: vec![crate::transport::wire::DeviceWire {
                id: "a-mic".to_owned(),
                name: "Studio Mic".to_owned(),
                is_default: true,
            }],
            configured: Some("a-mic".to_owned()),
        };
        let json = serde_json::to_string(&answer).expect("encode");
        assert!(json.contains("\"kind\":\"devices\""), "{json}");
        assert!(json.contains("\"is_default\":true"), "a row carries what a picker marks: {json}");
        let back: ServerMessage = serde_json::from_str(&json).expect("decode");
        let ServerMessage::Devices { devices, configured } = back else {
            panic!("{json} decoded into another message")
        };
        assert_eq!(devices.len(), 1, "the list crosses with its rows");
        assert_eq!(configured.as_deref(), Some("a-mic"), "and so does the configured pin");
    }

    #[test]
    fn a_subscribe_round_trips_through_json() {
        let sent = ClientMessage::Subscribe { what: Subject::Home, answering: true };
        let json = serde_json::to_string(&sent).expect("encode");
        assert!(json.contains("\"kind\":\"subscribe\""), "{json}");

        let back: ClientMessage = serde_json::from_str(&json).expect("decode");
        let ClientMessage::Subscribe { what, answering } = back else {
            panic!("{json} decoded into another message");
        };
        assert_eq!(what, Subject::Home, "the subject survives the round trip");
        assert!(answering, "and so does the capability the client declared");
    }

    /// The models subject hears its own landing and the load's signal -
    /// the only push that says the models finished loading - and nothing
    /// else. It is the whole live half of that page: a subscriber that
    /// never hears a landing reads as quiet, not as broken.
    #[test]
    fn the_models_subject_hears_the_landing_and_the_load() {
        let landing = SessionUpdate::DictateModelsChanged {
            models: forge_workspace::catalogue::DictateModelsSnapshot {
                enabled: true,
                models_dir: None,
                in_use: Vec::new(),
                check: forge_workspace::catalogue::CatalogueCheck::Never,
                updates: Vec::new(),
                rows: Vec::new(),
                install: forge_workspace::install::InstallState::Idle,
                activate: forge_workspace::install::ActivateState::Idle,
                installed: Vec::new(),
                bench: forge_workspace::bench::BenchState::Idle,
                results: Vec::new(),
                read_aloud: forge_workspace::bench::ReadAloudState {
                    recordings: Vec::new(),
                    recording: false,
                    error: None,
                    passage: String::new(),
                    terms: Vec::new(),
                },
            },
        };

        assert!(
            Subject::DictateModels.covers(&landing),
            "the check's landing is the subject's whole live half",
        );
        assert!(
            Subject::DictateModels.covers(&SessionUpdate::DictateAvailability),
            "and the preflight's signal, so a page open during a first download re-reads its \
             state chips instead of drawing `pending` until the next check",
        );
        assert!(
            !Subject::DictateModels.covers(&SessionUpdate::AccountsChanged),
            "another machine-level update is not the models page's to redraw",
        );
        assert!(
            Subject::Home.covers(&landing),
            "and the slot-less arm carries it home, which is what the client's own census \
             classifies it against",
        );
    }

    /// The pool belongs to no seat, so a home subscription hears it and a
    /// seat's does not.
    ///
    /// **Both home arms carry it, and the slot-less one is the load-bearing
    /// assertion here.** `fleet_news` classifies it a redraw because the
    /// band's own card is part of the region the web home re-sends; taking it
    /// out of that arm would leave this test green, because the slot-less arm
    /// below delivers it anyway. What the test can fail on is the variant
    /// gaining a seat: a key would send the pool to one session's page and
    /// leave every other subscriber drawing a card that never moves.
    #[test]
    fn the_pools_announcement_reaches_home_and_no_seat() {
        let update = SessionUpdate::AccountsChanged;
        let seat = SessionSlot::lead("TestOrg", "proj");

        assert!(update.slot().is_none(), "the pool addresses no seat, which is what routes it");
        assert!(Subject::Home.covers(&update), "home is the only subscription that could carry it");
        assert!(
            !Subject::Session(seat).covers(&update),
            "a seat's subscription is a seat, and the pool is not one",
        );
    }

    /// A parked draft is home news on its own: the seat's row moves into the
    /// needs-you group while the draft is held and back out when it resolves,
    /// so a home-ONLY subscriber that was never sent the pair reads the fleet
    /// as it stood before the draft - exactly the symptom #1758 was filed
    /// for, with no session page open to forward it.
    #[test]
    fn the_slack_draft_pair_reaches_home_and_its_seat() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        for update in [
            SessionUpdate::SlackPostPending {
                key: seat.clone(),
                draft: forge_primitives::slack::SlackDraft {
                    id: uuid::Uuid::new_v4(),
                    workspace: "acme".to_owned(),
                    conversation: "C1".to_owned(),
                    conversation_label: "acme".to_owned(),
                    thread_ts: None,
                    text: "hello".to_owned(),
                    tool: "slack__post".to_owned(),
                },
            },
            SessionUpdate::SlackDraftResolved {
                key: seat.clone(),
                id: uuid::Uuid::new_v4(),
                ending: forge_primitives::slack::SlackDraftEnding::Expired,
            },
        ] {
            assert!(
                fleet_news(&update).any(),
                "admitted by the redraw arm, not by the wildcard: {update:?}",
            );
            assert!(Subject::Home.covers(&update), "so the home is sent it: {update:?}");
            assert!(
                Subject::Session(seat.clone()).covers(&update),
                "and the seat it routes on is sent it too: {update:?}",
            );
        }
    }

    /// A section push reaches the subscribers the section belongs to: home,
    /// which draws the project row, and the seat the update routes on.
    ///
    /// Every one of these carries a key, so the slot-less arm is not what
    /// admits it - dropping it from `fleet_news`'s redraw arm is a section
    /// that stops popping, which is what this test exists to make fail by
    /// name.
    #[test]
    fn the_section_pushes_are_home_news() {
        for update in [
            SessionUpdate::TasksChanged {
                key: SessionSlot::lead("TestOrg", "proj"),
                tasks: Vec::new(),
            },
            SessionUpdate::CronSchedulesChanged {
                key: SessionSlot::lead("TestOrg", "proj"),
                crons: Vec::new(),
            },
            SessionUpdate::ConnectorSubscriptionsChanged {
                key: SessionSlot::lead("TestOrg", "proj"),
                gotify: Vec::new(),
                slack: Vec::new(),
            },
        ] {
            assert!(
                fleet_news(&update).any(),
                "admitted by the redraw arm, not by the wildcard: {update:?}",
            );
            assert!(Subject::Home.covers(&update), "so the home is sent it: {update:?}");
            assert!(
                Subject::Session(SessionSlot::lead("TestOrg", "proj")).covers(&update),
                "and the project's own seat is sent it too: {update:?}",
            );
            assert!(
                !Subject::Session(SessionSlot::lead("TestOrg", "other")).covers(&update),
                "while another seat's page is not: {update:?}",
            );
        }
    }

    /// The client's settings come off the server's own config, so the
    /// client never reads `forge.toml` and the two cannot drift - the
    /// `[client]` keys off it directly, and the `[dictate]` axes as the
    /// workspace resolved them.
    #[test]
    fn the_client_settings_are_the_configs_view_settings() {
        let config = ClientConfig {
            mark: Some("strike".to_owned()),
            theme: Some("dark".to_owned()),
            font: Some("system".to_owned()),
        };
        let axes = DictateAxes {
            styling: forge_dictate::normalize::Styling::Formal,
            structure: forge_dictate::normalize::Structure::Lists,
            context: forge_dictate::normalize::Context::Email,
        };

        let settings = ClientSettings::new(&config, axes);

        assert_eq!(settings.mark.as_deref(), Some("strike"));
        assert_eq!(settings.theme.as_deref(), Some("dark"));
        assert_eq!(settings.font.as_deref(), Some("system"));
        assert_eq!(settings.dictate, axes, "and the axes a capturing client starts on");
    }
}
