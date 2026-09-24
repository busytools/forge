//! The in-project engine the `agents__*` family drives: the workers
//! facade, its shared types, and the spawn / despawn / update lifecycle
//! behind them.
//!
//! Named for the family it grew out of. The tool surface itself is
//! [`crate::mcp::agents`].

pub mod facade;
pub mod types;

/// Composite key stored in `InflightAsk.target_project` for worker-
/// bound asks. The `::` separator can never appear in a real project
/// name (forge.toml validation rejects it), so this shape is
/// distinguishable from a cross-project ask's plain project name at
/// zero schema cost. `expire_inflight_for_closed_worker` matches on the
/// composite when a worker's session is torn down.
pub(crate) fn worker_target_project_key(project_key: &str, label: &str) -> String {
    format!("{project_key}::{label}")
}
