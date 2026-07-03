//! Gotify inbound-integration wire types.
//!
//! A forge session subscribes to a Gotify server via the
//! `mcp__forge__gotify__*` tool family; matching notifications deliver
//! as a user-turn into the durable subscriber. These are pure data
//! shapes - the WebSocket client lives in `forge-agent::env::gotify`,
//! the subscription store in `forge-workspace::store::gotify`, and the
//! match-and-route logic in `forge-workspace`.

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
/// role) receives notifications matching the `application` +
/// `min_priority` filter. Durable ones persist to redb; ephemeral
/// ad-hoc-worker ones stay in memory only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GotifySubscription {
    pub id: uuid::Uuid,
    pub project: String,
    /// forge.toml team-worker role; `None` targets the project lead.
    pub team_role: Option<String>,
    /// Gotify application NAME filter; `None` matches any application.
    pub application: Option<String>,
    /// Priority floor; `None` matches any priority.
    pub min_priority: Option<u8>,
    pub created_at: std::time::SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn gotify_types_round_trip() {
        let sub = GotifySubscription {
            id: uuid::Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0),
            project: "trader-cc".to_owned(),
            team_role: Some("analyst".to_owned()),
            application: Some("alerts".to_owned()),
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
}
