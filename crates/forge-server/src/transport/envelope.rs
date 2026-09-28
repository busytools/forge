//! What a client and this server exchange.
//!
//! Both enums are tagged on `kind`, which is added beside a message's own
//! fields rather than replacing anything, so a reader can tell what a
//! message is without knowing its shape. **The tag is the contract**:
//! adding a variant is compatible and renaming one is not.

use forge_primitives::{SessionSlot, WebConfig};
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
}

impl Subject {
    /// Whether an update belongs to what this subscription asked for.
    ///
    /// A session subscription is a seat, so the workspace is ASKED which seat
    /// a variant routes to rather than this matching the variants itself.
    ///
    /// A home subscription is a row per seat plus the App-level facts, so it
    /// takes the classification that already exists for the same question:
    /// [`fleet_news`], which is what the view folding this stream draws the
    /// fleet from. It is narrow on purpose - a row states a seat's lifecycle,
    /// its status and its pending state, and no row shows a word of its
    /// conversation - so a second table of variants here would both drift and
    /// hand a home subscriber every token of every seat in the fleet.
    pub fn covers(&self, update: &SessionUpdate) -> bool {
        match self {
            Self::Home => fleet_news(update).any(),
            Self::Session(seat) => update.slot() == Some(seat),
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
}

/// What the server sends.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMessage {
    Greeting {
        version: u32,
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
    Page {
        conversation: SessionSlot,
        rows: Vec<serde_json::Value>,
        cursor: Option<String>,
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
    },
}

/// What the client draws with, from `forge.toml` by way of the server.
///
/// The client never reads the config file: `[web] mark`, `[web] theme` and
/// `[web] font` arrive here, and `None` on any of them means the built-in
/// rather than a pinned value - a commented line in a hand-authored file has
/// to mean "unset".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ClientSettings {
    pub mark: Option<String>,
    pub theme: Option<String>,
    pub font: Option<String>,
}

impl From<&WebConfig> for ClientSettings {
    fn from(config: &WebConfig) -> Self {
        Self { mark: config.mark.clone(), theme: config.theme.clone(), font: config.font.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The client's settings come off the server's own config, so the
    /// client never reads `forge.toml` and the two cannot drift.
    #[test]
    fn the_client_settings_are_the_configs_view_settings() {
        let config = WebConfig {
            enabled: false,
            port: 9,
            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            mark: Some("strike".to_owned()),
            theme: Some("dark".to_owned()),
            font: Some("system".to_owned()),
        };

        let settings = ClientSettings::from(&config);

        assert_eq!(settings.mark.as_deref(), Some("strike"));
        assert_eq!(settings.theme.as_deref(), Some("dark"));
        assert_eq!(settings.font.as_deref(), Some("system"));
    }
}
