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
/// `Connected` event. `Failed` is set when spawn / resume fails
/// terminally - claude rejected the resume (e.g.,
/// `No conversation found`), the subprocess exited non-zero, or
/// the connection broke before `Connected` fired. The
/// human-readable reason lives on the sibling `diagnostic` field
/// of [`WorkerStatus`] / `WorkerEntry`; keeping it off the enum
/// preserves the `Copy` bound that every match site relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerLiveness {
    Spawning,
    Running,
    Failed,
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
    /// Human-readable failure reason when `status == Failed` (the
    /// first line of claude's stderr, or the error variant name when
    /// stderr was empty). `None` when the worker isn't in `Failed`
    /// state or no diagnostic could be captured. The Projects pane
    /// renders this as a dim sub-row beneath the worker label so the
    /// user can tell at a glance whether the failure is recoverable
    /// (e.g., "No conversation found" -> fresh spawn flow kicks in)
    /// or terminal (e.g., spawn binary missing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
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

    #[test]
    fn worker_liveness_failed_is_copy_unit_variant() {
        // Failed stays a unit variant so the enum keeps `Copy`. The
        // human-readable diagnostic lives on WorkerStatus's sibling
        // field, not inside the variant. The Copy bound matters
        // because every existing match site (and `matches!` invocation)
        // moves the value into the match arm; converting to a tuple
        // variant would ripple through ~10 sites.
        fn requires_copy<T: Copy>(_: T) {}
        requires_copy(WorkerLiveness::Failed);
        assert!(matches!(WorkerLiveness::Failed, WorkerLiveness::Failed));
    }

    #[test]
    fn worker_status_diagnostic_round_trips_through_serde() {
        let status = WorkerStatus {
            label: "reviewer".into(),
            charter: "be sharp".into(),
            status: WorkerLiveness::Failed,
            session_id: "uuid-1".into(),
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".into(),
            diagnostic: Some("No conversation found".into()),
        };
        let json = serde_json::to_string(&status).expect("serialize");
        let back: WorkerStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.diagnostic.as_deref(), Some("No conversation found"));
        assert_eq!(back.status, WorkerLiveness::Failed);
    }

    #[test]
    fn worker_status_diagnostic_defaults_to_none_when_absent_in_payload() {
        // Pre-#245 payloads have no `diagnostic` field; serde default
        // must yield None so old wire shapes still decode cleanly.
        let json = r#"{
            "label": "reviewer",
            "charter": "be sharp",
            "status": "Running",
            "session_id": "uuid-1",
            "spawned_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "spawned_by_session_id": "lead-uuid"
        }"#;
        let status: WorkerStatus = serde_json::from_str(json).expect("decode legacy shape");
        assert_eq!(status.diagnostic, None);
    }
}
