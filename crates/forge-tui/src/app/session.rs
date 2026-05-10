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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use forge_workspace::SessionKey;

use crate::agent::events::TerminalMap;
use crate::agent::model;
use crate::app::git_context::GitContextState;
use crate::app::state::cache_metrics::CacheMetrics;
use crate::app::state::messages::ChatMessage;
use crate::app::state::render_budget::{RenderCacheEvictionKey, RenderCacheSlotState};
use crate::app::state::types::{
    CancelOrigin, HistoryRetentionPolicy, HistoryRetentionStats, McpState, ModeState,
    SessionTurnState, SessionUsageState, TodoItem, ToolCallScope,
};
use crate::app::state::viewport::ChatViewport;
use crate::app::state::{ChatRenderTraceState, TerminalToolCallRef, TurnNoticeRef};

/// Per-session runtime state. Initialised when a session connects;
/// dropped when the session is closed or forge-tui exits.
///
/// No `Debug` derive — `AgentHandle` owns callback closures and
/// doesn't derive `Debug`. Every field's default matches its type's
/// `Default` impl, so `Default` is derived; if a field needs a
/// non-`Default::default()` initializer, factor it through
/// [`Session::new`] (or a new constructor) rather than re-introducing
/// a hand-written impl.
#[allow(clippy::struct_excessive_bools)]
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

    // ---- Account / auth ----
    /// Account info from the bridge status snapshot (email, org, subscription).
    pub account_info: Option<forge_primitives::AccountInfo>,
    /// Forge-side account identity: which `[[accounts]]` entry from
    /// `forge.toml` the workspace picked for this bridge. `None`
    /// when forge wasn't launched via the workspace (direct
    /// `Agent::spawn` from tests / smoke). Surfaced via
    /// [`crate::agent::events::ClientEvent::StatusSnapshotReceived`]'s
    /// `forge_account` and rendered in the welcome message + Status
    /// panel.
    pub active_account_display_name: Option<String>,
    /// OAuth credentials snapshot from the bridge — populated at
    /// session connect, refreshed after `/login` and `/logout` so
    /// callers can ask "is the user authenticated?" without doing
    /// their own filesystem walk to `<config_dir>/.credentials.json`.
    pub oauth_credentials: Option<forge_agent::cloud::oauth_credentials::OauthCredentials>,

    // ---- Filesystem ----
    /// Display-friendly cwd (`~/foo` form) used by status panel /
    /// footer / welcome card.
    pub cwd: String,
    /// Raw cwd as a filesystem path, used for trust lookups, file
    /// indexing, and project-key derivation.
    pub cwd_raw: String,
    /// Number of files accessed during the active turn (incremented
    /// on Read/Edit/Write tool starts, reset on TurnComplete).
    pub files_accessed: usize,
    /// Git repo context used by footer/status rendering and live
    /// branch tracking. Mutated by bridge-pushed git context
    /// snapshots.
    pub(crate) git_context: GitContextState,
    /// Config > MCP live server snapshot and refresh lifecycle.
    pub mcp: McpState,

    // ---- Todos ----
    /// Current todo list from Claude's `TodoWrite` tool calls.
    pub todos: Vec<TodoItem>,
    /// Whether the todo panel is expanded (true) or shows compact
    /// status line (false). Toggled by Ctrl+T.
    pub show_todo_panel: bool,
    /// Scroll offset for the expanded todo panel (capped at 5
    /// visible lines).
    pub todo_scroll: usize,
    /// Selected todo index used for keyboard navigation in the open
    /// todo panel.
    pub todo_selected: usize,
    /// Cached todo compact line (invalidated on `set_todos()`).
    pub cached_todo_compact: Option<ratatui::text::Line<'static>>,

    // ---- Render cache + history retention ----
    /// Cached render-cache slot metadata parallel to
    /// `messages[*].blocks[*]` plus one synthetic per-message slot
    /// at the tail of each row.
    pub(crate) render_cache_slots: Vec<Vec<RenderCacheSlotState>>,
    /// Rolling total of cached render bytes across blocks and
    /// message-level caches.
    pub(crate) render_cache_total_bytes: usize,
    /// Rolling total of cached render bytes currently excluded from
    /// the budget.
    pub(crate) render_cache_protected_bytes: usize,
    /// Evictable cached blocks ordered by LRU and size tie-breaker.
    pub(crate) render_cache_evictable: BTreeSet<RenderCacheEvictionKey>,
    /// Last message index currently protected as the streaming tail,
    /// if any.
    pub(crate) render_cache_tail_msg_idx: Option<usize>,
    /// Byte budget for source conversation history retained in
    /// memory.
    pub history_retention: HistoryRetentionPolicy,
    /// Last history-retention enforcement statistics.
    pub history_retention_stats: HistoryRetentionStats,
    /// Cross-cutting cache metrics accumulator (enforcement counts,
    /// watermarks, rate limits).
    pub cache_metrics: CacheMetrics,
    /// Height-affecting active assistant indicator state from the
    /// previous frame.
    pub(crate) last_active_turn_height_state: Option<(usize, bool, bool)>,
    /// Last emitted chat render trace snapshot to suppress identical
    /// per-frame summaries.
    pub last_chat_render_trace_state: Option<ChatRenderTraceState>,
}

impl Session {
    #[must_use]
    pub fn new(key: SessionKey) -> Self {
        Self { key: Some(key), ..Self::default() }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use crate::app::App;

    /// Pre-Connect bucket state (cwd, files_accessed, …) accumulated
    /// before the first `Connected` event must survive the
    /// synthetic-key → real-key migration that happens when
    /// `set_session_id` finally lands. Without the migration the
    /// welcome card / status panel would lose state on connect.
    #[test]
    fn set_session_id_migrates_pre_connect_bucket_state_onto_real_key() {
        let mut app = App::test_default();
        // Pre-connect bucket holds welcome state.
        app.set_cwd("/work/foo");
        app.set_files_accessed(3);

        let pre = forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY);
        assert!(app.sessions.contains_key(&pre));

        app.set_session_id(Some(crate::agent::model::SessionId::new("real-uuid")));

        let real = forge_workspace::SessionKey::from_session_id("real-uuid");
        assert!(!app.sessions.contains_key(&pre), "synthetic bucket removed");
        assert!(app.sessions.contains_key(&real), "real bucket exists");
        assert_eq!(app.cwd(), "/work/foo");
        assert_eq!(app.files_accessed(), 3);
    }
}
