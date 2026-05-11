//! Workspace-side authoritative state for one session.
//!
//! Phase 1 introduced this struct with just `pending_interactions`.
//! Phase 2 adds the 9 operational fields that today live duplicated
//! on `forge_tui::app::session::UiSession`: `conn` (deferred from
//! Phase 1), `session_id`, `lifecycle_state`, `cwd_raw`,
//! `session_scope_epoch`, `turn_state`, `account_info`,
//! `active_account_display_name`, `runtime_session_state`. Phase 4
//! deletes the TUI-side duplicates so this struct becomes the sole
//! source of truth.

use std::collections::HashMap;
use std::sync::Arc;

use forge_agent::AgentHandle;
use forge_primitives::runtime::{RuntimeSessionState, SessionLifecycleState, SessionTurnState};
use forge_primitives::{AccountInfo, SessionId};

use crate::SessionKey;
use crate::protocol::PendingInteractionSlot;

/// Workspace's owned per-session state. One `DomainSession` per
/// active `SessionTask`. Single writer (the `SessionTask`); accessed
/// via `Arc<parking_lot::Mutex<DomainSession>>` for `Workspace`-side
/// helpers (`store_pending_interaction`,
/// `record_forge_account_identity_for_domain`,
/// `finalize_turn_in_domain`) to also write under the lock. TUI
/// readers borrow via `Workspace::domain_session_for(key)`.
#[non_exhaustive]
pub struct DomainSession {
    pub key: SessionKey,
    /// Claude-issued session UUID. `None` until the first `Connected`
    /// event from this session's bridge.
    pub session_id: Option<SessionId>,
    /// Agent connection handle bound to this session at spawn time.
    pub conn: Arc<AgentHandle>,
    /// Lifecycle state for the Projects pane glyph. Updated by
    /// the per-session `SessionTask::translate_event` as each
    /// `AgentEvent` arrives.
    pub lifecycle_state: SessionLifecycleState,
    /// Raw cwd as a filesystem path. Used for trust lookups, file
    /// indexing, project-key derivation, and `claude --resume` re-spawn
    /// reconstruction.
    pub cwd_raw: String,
    /// Monotonic session authority epoch — bumped on each session
    /// reset (`/new`, login, logout) so stale async view data can be
    /// ignored.
    pub session_scope_epoch: u64,
    /// SDK turn state — model-resolution cache, mode capability,
    /// MCP cooldowns, auth/error flags.
    pub turn_state: SessionTurnState,
    /// Account snapshot from the bridge's status event.
    pub account_info: Option<AccountInfo>,
    /// Forge-side display name of the `[[accounts]]` entry the
    /// workspace picked for this bridge.
    pub active_account_display_name: Option<String>,
    /// Latest SDK runtime liveness state (`Idle` / `Running` /
    /// `RequiresAction`). Populated by Phase 3 sub-phases as the
    /// matching TUI handlers migrate; Phase 2 reserves the field but
    /// no event writes it yet.
    pub runtime_session_state: Option<RuntimeSessionState>,
    /// Phase 1 mailbox: pending permission/question/elicitation
    /// oneshots indexed by the wire `tool_id` / `elicitation_id`.
    /// `SessionTask` pops on `Respond*`; bridge_lifecycle inserts on
    /// every `*Request` event.
    pub pending_interactions: HashMap<String, PendingInteractionSlot>,
}

impl DomainSession {
    /// Construct a fresh `DomainSession` bound to `key` with the
    /// given `conn`. Operational fields take their `Default` values
    /// or `None`; `Workspace::record_event_for_domain` writes them
    /// as events arrive.
    pub fn new(key: SessionKey, conn: Arc<AgentHandle>) -> Self {
        Self {
            key,
            session_id: None,
            conn,
            lifecycle_state: SessionLifecycleState::default(),
            cwd_raw: String::new(),
            session_scope_epoch: 0,
            turn_state: SessionTurnState::default(),
            account_info: None,
            active_account_display_name: None,
            runtime_session_state: None,
            pending_interactions: HashMap::new(),
        }
    }
}

impl std::fmt::Debug for DomainSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainSession")
            .field("key", &self.key)
            .field("session_id", &self.session_id)
            .field("lifecycle_state", &self.lifecycle_state)
            .field("cwd_raw", &self.cwd_raw)
            .field("session_scope_epoch", &self.session_scope_epoch)
            .field("account_info", &self.account_info)
            .field("active_account_display_name", &self.active_account_display_name)
            .field("runtime_session_state", &self.runtime_session_state)
            .field("pending_interactions_count", &self.pending_interactions.len())
            .finish_non_exhaustive()
    }
}
