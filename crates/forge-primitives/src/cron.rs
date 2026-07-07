//! Durable forge-cron wire types.
//!
//! A forge cron is a scheduled prompt that fires into a project's
//! session and survives forge restarts. It lives in the
//! `mcp__forge__cron__*` tool family alongside peers + workers. The
//! list persists to `<config_dir>/forge/cron.toml` for restart
//! durability; the single-instance boot guard makes one forge process
//! per config dir the sole writer, so the in-process mutex (not the
//! file) is the serialization point.
//!
//! These are pure data shapes - the parsing, due-check, and catch-up
//! math live in `forge-workspace::mcp::cron`, and the persistence in
//! `forge-workspace::cron_store`.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Identifier for a durable forge cron entry. Minted at create time
/// (a UUIDv4 in the `cron__create` handler) and used by `cron__delete`
/// to address an entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct CronId(pub String);

impl CronId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CronId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for CronId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for CronId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// What schedule drives a cron.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CronKind {
    /// A 5-field cron expression evaluated at minute granularity.
    /// After each fire `next_fire` advances to the next matching slot.
    Recurring(String),
    /// A one-shot fire at the given instant. After firing the entry
    /// self-deletes. `next_fire` mirrors this instant.
    Once(SystemTime),
}

/// One persisted forge cron. The scheduler's due-check reads
/// `next_fire` uniformly for both kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronEntry {
    pub id: CronId,
    /// forge.toml project NAME this cron belongs to and fires into.
    /// Delivery resolves it against the running project leads (the
    /// live path) or `Command::SpawnProject { project_name }` (the
    /// spawn-to-fire path), so it is the human-facing name, not the
    /// directory-derived `ProjectKey` hash.
    pub project_name: String,
    pub kind: CronKind,
    pub prompt: String,
    pub created_at: SystemTime,
    /// Last instant this cron actually fired. `None` until the first
    /// fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fire: Option<SystemTime>,
    /// Absolute instant of the next scheduled fire. For `Once` this
    /// equals the kind's instant.
    pub next_fire: SystemTime,
    /// Owner: `None` for a lead cron (also every seeded / legacy entry),
    /// `Some(label)` for a worker's. Routes the fire to its owner and
    /// scopes `cron__list` / `cron__delete` to that owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_role: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn cron_id_round_trips_through_string() {
        let id = CronId::from("abc-123");
        assert_eq!(id.as_str(), "abc-123");
        assert_eq!(id.to_string(), "abc-123");
        assert_eq!(CronId::new("x").0, "x");
    }

    #[test]
    fn recurring_entry_serde_round_trips() {
        let entry = CronEntry {
            id: CronId::from("id-1"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up summary".to_owned(),
            created_at: epoch(1_700_000_000),
            last_fire: None,
            next_fire: epoch(1_700_032_400),
            team_role: None,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: CronEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry, back);
    }

    #[test]
    fn once_entry_serde_round_trips() {
        let entry = CronEntry {
            id: CronId::from("id-2"),
            project_name: "airmail".to_owned(),
            kind: CronKind::Once(epoch(1_700_100_000)),
            prompt: "deploy".to_owned(),
            created_at: epoch(1_700_000_000),
            last_fire: Some(epoch(1_700_050_000)),
            next_fire: epoch(1_700_100_000),
            team_role: None,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        let back: CronEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(entry, back);
    }
}
