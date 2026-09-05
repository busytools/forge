//! Gotify inbound-integration wire types.
//!
//! A forge session subscribes to a Gotify server via the
//! `mcp__forge__gotify__*` tool family; matching notifications deliver
//! as a user-turn into the durable subscriber. These are pure data
//! shapes - the stream, REST lookups and match-and-route logic live in
//! `forge-connectors::gotify`, which reaches workspace state through
//! its host port, and the subscription store in
//! `forge-workspace::store::gotify`.

use serde::{Deserialize, Serialize};

/// Connection to a Gotify server, parsed from the `[gotify]` block of
/// forge.toml. One server per v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GotifyConfig {
    pub url: String,
    /// Client token for the receive stream (`/stream?token=`) and the
    /// `/application` lookup (`X-Gotify-Key`).
    pub client_token: String,
}

/// A notification decoded from the Gotify server's WebSocket stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GotifyMessage {
    pub id: u64,
    /// Gotify's stream sends the source application as lowercase `appid`.
    pub appid: u64,
    pub title: String,
    pub message: String,
    pub priority: u8,
    /// RFC3339 timestamp from the server, kept verbatim for v1.
    pub date: String,
}

/// An active subscription: which project (and optional team-worker
/// role) receives notifications matching the `applications` +
/// `min_priority` filter. Durable ones persist to redb; ephemeral
/// ad-hoc-worker ones stay in memory only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "GotifySubscriptionRepr")]
pub struct GotifySubscription {
    pub id: uuid::Uuid,
    pub project: String,
    /// Worker label owning this subscription; `None` targets the
    /// project lead. Named `team_role` for the stored shape - a rename
    /// would decode cleanly to `None` and silently reroute every
    /// worker's subscription to the lead.
    pub team_role: Option<String>,
    /// Gotify application NAME filters; empty matches any application,
    /// otherwise the message's app must be one of the listed names.
    pub applications: Vec<String>,
    /// Priority floor; `None` matches any priority.
    pub min_priority: Option<u8>,
    pub created_at: std::time::SystemTime,
}

/// Deserialization shape, tolerant of the v0.20.0 record that stored a
/// single `application: Option<String>` and no `applications` key. A
/// non-null legacy `application` folds forward into `applications`; a
/// null or absent one leaves the empty match-any set. Serialization
/// always emits the current `applications` shape (this is deser-only).
#[derive(Deserialize)]
struct GotifySubscriptionRepr {
    id: uuid::Uuid,
    project: String,
    team_role: Option<String>,
    #[serde(default)]
    applications: Vec<String>,
    #[serde(default)]
    application: Option<String>,
    min_priority: Option<u8>,
    created_at: std::time::SystemTime,
}

impl From<GotifySubscriptionRepr> for GotifySubscription {
    fn from(repr: GotifySubscriptionRepr) -> Self {
        let mut applications = repr.applications;
        if applications.is_empty() {
            applications.extend(repr.application);
        }
        Self {
            id: repr.id,
            project: repr.project,
            team_role: repr.team_role,
            applications,
            min_priority: repr.min_priority,
            created_at: repr.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn gotify_types_round_trip() {
        let sub = GotifySubscription {
            id: uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0),
            project: "web-api".to_owned(),
            team_role: Some("analyst".to_owned()),
            applications: vec!["alerts".to_owned()],
            min_priority: Some(5),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        };
        let json = serde_json::to_string(&sub).expect("serialize");
        let back: GotifySubscription = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sub, back);

        let line = r#"{"id":1,"appid":3,"title":"t","message":"m","priority":5,"date":"2026-07-03T09:18:00Z"}"#;
        let msg: GotifyMessage = serde_json::from_str(line).expect("deserialize stream line");
        assert_eq!(msg.appid, 3);
        assert_eq!(msg.priority, 5);
    }

    #[test]
    fn legacy_v0_20_0_record_migrates_forward() {
        // v0.20.0 persisted `application: Option<String>` with no
        // `applications` key. Build that shape from a current record so
        // the SystemTime encoding stays whatever serde emits.
        let base = GotifySubscription {
            id: uuid::Uuid::from_u128(1),
            project: "p".to_owned(),
            team_role: None,
            applications: vec![],
            min_priority: Some(5),
            created_at: SystemTime::UNIX_EPOCH,
        };
        let mut legacy = serde_json::to_value(&base).expect("serialize");
        legacy.as_object_mut().expect("object").remove("applications");

        legacy.as_object_mut().expect("object").insert("application".to_owned(), "alerts".into());
        let named: GotifySubscription =
            serde_json::from_value(legacy.clone()).expect("legacy named record");
        assert_eq!(
            named.applications,
            vec!["alerts".to_owned()],
            "a real old single-app filter folds forward",
        );

        legacy
            .as_object_mut()
            .expect("object")
            .insert("application".to_owned(), serde_json::Value::Null);
        let any: GotifySubscription =
            serde_json::from_value(legacy).expect("legacy null-application record");
        assert!(any.applications.is_empty(), "a null application becomes the match-any set");
    }
}
