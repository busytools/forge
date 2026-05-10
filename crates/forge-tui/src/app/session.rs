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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use forge_workspace::SessionKey;

use crate::agent::events::TerminalMap;
use crate::agent::model;
use crate::app::state::messages::ChatMessage;
use crate::app::state::types::{
    CancelOrigin, ModeState, SessionTurnState, SessionUsageState, ToolCallScope,
};
use crate::app::state::viewport::ChatViewport;
use crate::app::state::{TerminalToolCallRef, TurnNoticeRef};

/// Per-session runtime state. Initialised when a session connects;
/// dropped when the session is closed or forge-tui exits.
///
/// No `Debug` derive — `AgentHandle` owns callback closures and
/// doesn't derive `Debug`. `Default` is provided by hand because
/// [`model::FastModeState`] is plain wire enum without a `Default`
/// impl; the rest of the fields fall through to their type defaults.
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

    // ---- Runtime + model ----
    /// Current model resolution as advertised by the bridge.
    pub current_model: Option<model::CurrentModel>,
    /// Models advertised by the agent SDK for this session.
    pub available_models: Vec<model::AvailableModel>,
    /// Commands advertised by the agent via `AvailableCommandsUpdate`.
    pub available_commands: Vec<model::AvailableCommand>,
    /// Subagents advertised by the agent via `AvailableAgentsUpdate`.
    pub available_agents: Vec<model::AvailableAgent>,
    /// Latest mode snapshot from the SDK's `system/status` events.
    pub mode: Option<ModeState>,
    /// Hook-observed permission mode. Higher fidelity than [`Self::mode`]
    /// when the CLI changes mode without re-emitting status (#88).
    pub observed_permission_mode: Option<crate::agent::state::PermissionMode>,
    /// Hook-observed effort level. Same pattern as
    /// [`Self::observed_permission_mode`].
    pub observed_effort: Option<model::EffortLevel>,
    /// Most recent model id observed on a `Message::Assistant`
    /// envelope. Higher-fidelity than `current_model.resolved_id` for
    /// per-turn model verification.
    pub observed_assistant_model: Option<String>,
    /// Latest SDK runtime liveness state.
    pub runtime_session_state: Option<model::RuntimeSessionState>,
    /// Fast mode state telemetry from the SDK.
    pub fast_mode_state: model::FastModeState,
    /// Latest config options observed from bridge `config_option_update` events.
    pub config_options: BTreeMap<String, serde_json::Value>,
    /// Session-wide usage and cost telemetry from the bridge.
    pub session_usage: SessionUsageState,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            key: None,
            session_id: None,
            conn: None,
            session_scope_epoch: 0,
            messages: Vec::new(),
            message_retained_bytes: Vec::new(),
            retained_history_bytes: 0,
            viewport: ChatViewport::default(),
            active_turn_assistant_message_idx: None,
            turn_state: SessionTurnState::default(),
            is_compacting: false,
            pending_compact_clear: false,
            pending_interaction_ids: Vec::new(),
            cancelled_turn_pending_hint: false,
            pending_cancel_origin: None,
            prompt_suggestion: None,
            last_rate_limit_update: None,
            turn_notice_refs: Vec::new(),
            active_task_ids: HashSet::new(),
            tool_call_scopes: HashMap::new(),
            tool_call_index: HashMap::new(),
            terminals: TerminalMap::default(),
            terminal_tool_calls: Vec::new(),
            terminal_tool_call_membership: HashSet::new(),
            subagent_attribution: HashMap::new(),
            current_model: None,
            available_models: Vec::new(),
            available_commands: Vec::new(),
            available_agents: Vec::new(),
            mode: None,
            observed_permission_mode: None,
            observed_effort: None,
            observed_assistant_model: None,
            runtime_session_state: None,
            fast_mode_state: model::FastModeState::Off,
            config_options: BTreeMap::new(),
            session_usage: SessionUsageState::default(),
        }
    }
}

impl Session {
    #[must_use]
    pub fn new(key: SessionKey) -> Self {
        Self { key: Some(key), ..Self::default() }
    }
}
