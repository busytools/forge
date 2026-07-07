//! Workspace-side per-session state.
//!
//! Holds only what workspace itself needs: routing metadata
//! (`AgentHandle` slot, claude-issued `session_id`) and the
//! pending-interaction mailbox. Operational state TUI renders
//! (lifecycle, cwd, account info) lives on
//! `forge_tui::app::session::UiSession`, not duplicated here - the
//! lone exception is [`DomainSession::runtime_state`], the one turn
//! signal the workspace needs authoritatively for the `/account`
//! switch backstop.

use std::collections::HashMap;
use std::sync::Arc;

use forge_agent::AgentHandle;
use forge_primitives::{RuntimeSessionState, SessionId};

use crate::SessionKey;
use crate::mcp::gotify::types::GotifyNotification;
use crate::mcp::peers::types::WrappedPrompt;
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
    /// Peer messages targeted at this session that arrived while the
    /// session was still spawning (pre-Connected). Workspace's
    /// `deliver_peer_prompt` pushes here when the target is sleeping
    /// and a `Command::SpawnProject` is in flight; `SessionTask`
    /// drains atomically on `AgentEvent::Connected` and re-dispatches
    /// each as a regular `Command::Prompt`. Empty in steady state.
    pub pending_peer_prompts: Vec<WrappedPrompt>,
    /// Plain cron prompts targeted at this session that arrived while it
    /// was still spawning (pre-Connected). A due cron whose project
    /// session is asleep pushes its prompt here and dispatches
    /// `Command::SpawnProject`; `SessionTask` drains on
    /// `AgentEvent::Connected` and re-dispatches each as a regular
    /// `Command::Prompt` (a plain user turn, not a peer envelope). Empty
    /// in steady state.
    pub pending_cron_prompts: Vec<String>,
    /// Gotify notifications targeted at this session that arrived while
    /// it was still spawning (pre-Connected). Same drain path as
    /// [`Self::pending_cron_prompts`], but typed - `SessionTask` emits a
    /// chat echo and re-dispatches each as a plain user turn on
    /// `AgentEvent::Connected`. Empty in steady state.
    pub pending_gotify_prompts: Vec<GotifyNotification>,
    /// Hop count of the most-recent peer wrapper the LLM is currently
    /// processing. Stamped by `Workspace::deliver_peer_prompt` with
    /// `max(current.unwrap_or(0), wrapped.hop)` BEFORE dispatching
    /// `Command::Prompt`; cleared by `SessionTask` on `TurnComplete`.
    /// Read by `WorkspaceFacade::peek_current_inbound_hop` so the
    /// outbound ask/tell tools can stamp `hop = current + 1` on
    /// forwarded messages without the LLM having to pass it. `None`
    /// when the LLM is mid-turn on a user-initiated (not peer-
    /// forwarded) prompt.
    pub current_inbound_hop: Option<u8>,
    /// `--new` boot-wave flag, stamped at spawn time from
    /// `SessionLaunchSettings.force_new`. For a project lead it makes
    /// the Connected-time team spawn skip the worker resume scan
    /// (`resume_existing = None` for every role), so the workers come
    /// up fresh alongside their fresh lead. `false` for every non-boot
    /// spawn.
    pub spawned_force_new: bool,
    /// Latest runtime liveness mirrored from the session's
    /// `session_state_changed` wire messages. Operational turn state
    /// otherwise lives on the TUI's `UiSession`; this one signal is
    /// duplicated here so `handle_switch_account` can authoritatively
    /// refuse an `/account` switch while a turn is in flight (the TUI
    /// idle-gate alone can race a just-delivered peer / cron / gotify
    /// prompt). `None` until the first state message.
    pub runtime_state: Option<RuntimeSessionState>,
}

impl DomainSession {
    /// Construct a fresh `DomainSession` bound to `key` with the
    /// given `conn`. Pre-spawn / pre-Connect callers pass `None` to
    /// register a placeholder domain whose handle slot fills in once
    /// the spawn handler runs.
    pub fn new(key: SessionKey, conn: Option<Arc<AgentHandle>>) -> Self {
        Self {
            key,
            session_id: None,
            conn,
            pending_interactions: HashMap::new(),
            pending_peer_prompts: Vec::new(),
            pending_cron_prompts: Vec::new(),
            pending_gotify_prompts: Vec::new(),
            current_inbound_hop: None,
            spawned_force_new: false,
            runtime_state: None,
        }
    }
}

impl std::fmt::Debug for DomainSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainSession")
            .field("key", &self.key)
            .field("session_id", &self.session_id)
            .field("pending_interactions_count", &self.pending_interactions.len())
            .field("pending_peer_prompts_count", &self.pending_peer_prompts.len())
            .field("pending_cron_prompts_count", &self.pending_cron_prompts.len())
            .field("pending_gotify_prompts_count", &self.pending_gotify_prompts.len())
            .field("current_inbound_hop", &self.current_inbound_hop)
            .finish_non_exhaustive()
    }
}
