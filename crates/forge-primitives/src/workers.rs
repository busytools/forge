//! Worker-session shared types: tag constants, status snapshot,
//! and the `worker_tag()` helper used by every crate that needs
//! to format a worker's `{"type":"tag","tag":"forge:worker:<label>"}`
//! JSONL row consistently.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Tag value written to the JSONL of every project-default (lead)
/// session. Consumed by the resolver: `latest(forge:lead) →
/// latest(untagged) → fresh`.
pub const FORGE_LEAD_TAG: &str = "forge:lead";

/// Prefix shared by every worker session's tag. The full tag is
/// formatted as `forge:worker:<label>` via [`worker_tag`].
pub const FORGE_WORKER_TAG_PREFIX: &str = "forge:worker:";

/// Format a worker session's tag value: `forge:worker:<label>`.
/// Used at spawn (writer side) and at scan (reader side).
#[must_use]
pub fn worker_tag(label: &str) -> String {
    format!("{FORGE_WORKER_TAG_PREFIX}{label}")
}

/// Liveness of a worker session in the workspace's `live_workers`
/// map. `Spawning` is the brief window between
/// `Command::SpawnWorker` dispatch and the new session's
/// `Connected` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerLiveness {
    Spawning,
    Running,
}

/// Snapshot of one worker. Returned by `workers__list` and threaded
/// through `SessionUpdate::WorkerStatusChanged` for TUI rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub label: String,
    pub charter: String,
    pub status: WorkerLiveness,
    pub session_id: String,
    pub spawned_at: SystemTime,
    /// session_id of the caller that issued the `workers__spawn`
    /// call. In v1 this is always the project's lead; field exists
    /// pre-baked for v2 worker-spawn-from-worker (currently gated).
    pub spawned_by_session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_tag_concatenates_prefix_and_label() {
        assert_eq!(worker_tag("reviewer"), "forge:worker:reviewer");
    }

    #[test]
    fn worker_tag_allows_unicode_labels() {
        assert_eq!(worker_tag("résumé"), "forge:worker:résumé");
    }

    #[test]
    fn worker_tag_empty_label_yields_bare_prefix() {
        // Empty-label validation happens at the spawn tool gate, not
        // at the format helper. The helper produces what it produces.
        assert_eq!(worker_tag(""), "forge:worker:");
    }

    #[test]
    fn lead_tag_constant_value() {
        assert_eq!(FORGE_LEAD_TAG, "forge:lead");
    }

    #[test]
    fn worker_prefix_constant_value() {
        assert_eq!(FORGE_WORKER_TAG_PREFIX, "forge:worker:");
    }
}
