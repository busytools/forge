//! Workspace-side per-session state.
//!
//! Holds only what workspace itself needs: routing metadata
//! (`AgentHandle` slot, claude-issued `session_id`) and the
//! pending-interaction mailbox. Operational state TUI renders
//! (lifecycle, cwd, turn state, account info) lives on
//! `forge_tui::app::session::UiSession`, never duplicated here.

use std::collections::HashMap;
use std::sync::Arc;

use forge_agent::AgentHandle;
use forge_primitives::SessionId;

use crate::SessionKey;
use crate::protocol::PendingInteractionSlot;

/// Workspace's owned per-session state. One `DomainSession` per
/// active `SessionTask`. Single writer (the `SessionTask`); accessed
/// via `Arc<parking_lot::Mutex<DomainSession>>` so the `Workspace`
/// can route commands without locking the whole pool.
pub struct DomainSession {
    pub key: SessionKey,
    /// Claude-issued session UUID. `None` until the first `Connected`
    /// event from this session's bridge. Workspace consults this when
    /// dispatching `AgentHandle` calls that route by session id.
    pub session_id: Option<SessionId>,
    /// Agent connection handle bound to this session at spawn time.
    /// `None` for pre-spawn / pre-Connect domains (forge-tui's
    /// `connect::create_app` registers a placeholder handle-less
    /// domain so the spawn handler can fill it in later).
    pub conn: Option<Arc<AgentHandle>>,
    /// Pending permission/question/elicitation oneshots indexed by the
    /// wire `tool_id` / `elicitation_id`. `SessionTask` pops on
    /// `Respond*` commands; bridge inserts on every `*Request` event.
    pub pending_interactions: HashMap<String, PendingInteractionSlot>,
}

impl DomainSession {
    /// Construct a fresh `DomainSession` bound to `key` with the
    /// given `conn`. Pre-spawn / pre-Connect callers pass `None` to
    /// register a placeholder domain whose handle slot fills in once
    /// the spawn handler runs.
    pub fn new(key: SessionKey, conn: Option<Arc<AgentHandle>>) -> Self {
        Self { key, session_id: None, conn, pending_interactions: HashMap::new() }
    }
}

impl std::fmt::Debug for DomainSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainSession")
            .field("key", &self.key)
            .field("session_id", &self.session_id)
            .field("pending_interactions_count", &self.pending_interactions.len())
            .finish_non_exhaustive()
    }
}
