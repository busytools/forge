//! Per-session state bucket.
//!
//! Phase 2a moves ~50 fields off `App` into this struct. Commit 1
//! (this commit) ships the struct empty — subsequent bucket-migration
//! commits add field groups one bucket at a time, each leaving the
//! tree compiling + tests passing.
//!
//! `App.sessions: HashMap<SessionKey, Session>` holds N sessions;
//! `App.active_session_key` points at the rendered one. Background
//! sessions accumulate state silently while the user is elsewhere
//! (Phase 2 of the side-panes feature; backend prerequisite for the
//! Projects pane UI).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use forge_workspace::SessionKey;

use crate::agent::events::TerminalMap;
use crate::agent::model;
use crate::app::state::messages::ChatMessage;
use crate::app::state::types::{CancelOrigin, SessionTurnState, ToolCallScope};
use crate::app::state::viewport::ChatViewport;
use crate::app::state::{TerminalToolCallRef, TurnNoticeRef};

/// Per-session runtime state. Initialised when a session connects;
/// dropped when the session is closed or forge-tui exits.
///
/// `Default` only — `AgentHandle` doesn't derive `Debug` (it owns
/// callback closures), so we can't derive `Debug` here either.
#[derive(Default)]
pub struct Session {
    /// The claude-issued session UUID, also used as the map key.
    /// Stored here for symmetry; the map lookup uses the same value.
    pub key: Option<SessionKey>,
    /// Claude-issued session id (typed wrapper). `None` until the
    /// first `Connected` event from this session's bridge.
    pub session_id: Option<model::SessionId>,
    /// Agent connection handle for this session. `None` while the
    /// session's bridge is starting up.
    pub conn: Option<Arc<forge_agent::AgentHandle>>,
    /// Monotonic session authority epoch — used to ignore stale
    /// async view data after a session reset / reconnect.
    pub session_scope_epoch: u64,
    /// Chat history buffer for this session. Welcome message at
    /// index 0; user/assistant turns appended.
    pub messages: Vec<ChatMessage>,
    /// Cached approximate retained bytes for each message,
    /// parallel to [`Self::messages`].
    pub message_retained_bytes: Vec<usize>,
    /// Rolling total of [`Self::message_retained_bytes`].
    pub retained_history_bytes: usize,
    /// Single owner of all chat layout state: scroll, per-message
    /// heights, prefix sums.
    pub viewport: ChatViewport,
    /// Message index that owns the current main-assistant turn
    /// indicators (spinner, status chips). Cleared on `TurnComplete`.
    pub active_turn_assistant_message_idx: Option<usize>,

    // ---- Turn lifecycle ----
    /// Per-session SDK turn state — model-resolution cache, mode
    /// capability, MCP cooldowns, auth/error flags.
    pub turn_state: SessionTurnState,
    /// True while the SDK reports active compaction.
    pub is_compacting: bool,
    /// When true, the current/next turn completion should clear
    /// local conversation history. Set by `/compact` once the
    /// command is accepted for bridge forwarding.
    pub pending_compact_clear: bool,
    /// Tool call IDs with pending inline interactions, ordered by
    /// arrival. The first entry is the focused interaction that
    /// receives keyboard input. Up / Down arrow keys cycle focus
    /// through the list.
    pub pending_interaction_ids: Vec<String>,
    /// Set when a cancel notification succeeds; consumed on
    /// `TurnComplete` to render a red interruption hint in chat.
    pub cancelled_turn_pending_hint: bool,
    /// Origin of the in-flight cancellation request, if any.
    pub pending_cancel_origin: Option<CancelOrigin>,
    /// Latest prompt suggestion from the SDK, shown in the input
    /// hint band.
    pub prompt_suggestion: Option<String>,
    /// Latest rate-limit telemetry from the SDK.
    pub last_rate_limit_update: Option<model::RateLimitUpdate>,
    /// Turn-local inline/system notices that may upgrade in place
    /// during the active turn.
    pub turn_notice_refs: Vec<TurnNoticeRef>,

    // ---- Tool tracking ----
    /// IDs of root Task/Agent tool calls currently `InProgress`.
    /// Use `App::insert_active_task()` / `remove_active_task()`.
    pub active_task_ids: HashSet<String>,
    /// Tool scope keyed by tool call ID; used to distinguish
    /// main-agent, subagent roots, and explicitly owned subagent
    /// child tools.
    pub tool_call_scopes: HashMap<String, ToolCallScope>,
    /// O(1) lookup: `tool_call_id` -> `(message_index, block_index)`.
    /// Use `App::lookup_tool_call()` / `index_tool_call()`.
    pub tool_call_index: HashMap<String, (usize, usize)>,
    /// Shared terminal process map - used to snapshot output on
    /// completion.
    pub terminals: TerminalMap,
    /// Indexed terminal tool calls for per-frame terminal snapshot
    /// updates. Avoids O(n*m) scan of all messages/blocks every
    /// frame.
    pub terminal_tool_calls: Vec<TerminalToolCallRef>,
    /// Membership index for [`Self::terminal_tool_calls`], used to
    /// avoid linear duplicate checks.
    pub terminal_tool_call_membership: HashSet<TerminalToolCallRef>,
    /// Hook-observed sub-agent attribution: maps `tool_use_id` to
    /// the sub-agent's typed identifier (e.g. `"general-purpose"`).
    /// Used to label tool-call rows fired by sub-agents (#84
    /// partial).
    pub subagent_attribution: HashMap<String, String>,
}

impl Session {
    #[must_use]
    pub fn new(key: SessionKey) -> Self {
        Self { key: Some(key), ..Self::default() }
    }
}
