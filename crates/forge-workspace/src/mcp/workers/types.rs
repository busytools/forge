//! Workers MCP shared types. `WorkerStatus` is re-exported from
//! `forge-primitives`; this module owns the workspace-internal
//! `WorkerEntry` which adds routing metadata (session_key) on top
//! of the wire shape.

use std::time::SystemTime;

use forge_primitives::{WorkerLiveness, WorkerStatus};

use crate::SessionKey;

/// In-memory entry stored in `Workspace.live_workers[project_key]`.
/// `WorkerStatus` is the wire shape returned by `workers__list`;
/// `session_key` is the workspace-internal routing handle.
#[derive(Debug, Clone)]
pub struct WorkerEntry {
    pub label: String,
    pub charter: String,
    pub session_key: SessionKey,
    pub status: WorkerLiveness,
    pub spawned_at: SystemTime,
    pub spawned_by_session_id: String,
}

impl WorkerEntry {
    /// Project the workspace-internal entry to the wire shape.
    /// `session_id` is the worker's claude-issued session UUID (=
    /// `session_key.as_str().to_owned()` once Connected).
    #[must_use]
    pub fn to_status(&self) -> WorkerStatus {
        WorkerStatus {
            label: self.label.clone(),
            charter: self.charter.clone(),
            status: self.status,
            session_id: self.session_key.as_str().to_owned(),
            spawned_at: self.spawned_at,
            spawned_by_session_id: self.spawned_by_session_id.clone(),
        }
    }
}
