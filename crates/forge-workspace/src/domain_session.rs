//! Workspace-side authoritative state for one session. Phase 1
//! introduces this struct with just `pending_interactions` (the
//! oneshot storage for permission/question/elicitation responses).
//! Phase 2 adds the other 8 operational fields (session_id, conn,
//! lifecycle_state, cwd_raw, session_scope_epoch, turn_state,
//! account_info, active_account_display_name).

use std::collections::HashMap;

use crate::SessionKey;
use crate::protocol::PendingInteractionSlot;

/// Workspace's owned per-session state. One `DomainSession` per
/// active `SessionTask`. Single writer (the `SessionTask`); accessed
/// via `Arc<parking_lot::Mutex<DomainSession>>` for `Workspace`-side
/// helpers (`store_pending_interaction`, `finalize_turn_in_domain`)
/// to also write under the lock.
pub struct DomainSession {
    pub key: SessionKey,
    pub pending_interactions: HashMap<String, PendingInteractionSlot>,
    // Phase 2 adds: session_id, conn, lifecycle_state, cwd_raw,
    // session_scope_epoch, turn_state, account_info,
    // active_account_display_name.
}

impl DomainSession {
    pub fn new(key: SessionKey) -> Self {
        Self { key, pending_interactions: HashMap::new() }
    }
}

impl std::fmt::Debug for DomainSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainSession")
            .field("key", &self.key)
            .field("pending_interactions_count", &self.pending_interactions.len())
            .finish_non_exhaustive()
    }
}
