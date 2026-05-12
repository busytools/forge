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
use std::time::Instant;

use forge_workspace::SessionKey;

use crate::agent::events::TerminalMap;
use crate::agent::model;
use crate::app::git_context::GitContextState;
use crate::app::input::InputState;
use crate::app::state::cache_metrics::CacheMetrics;
use crate::app::state::messages::ChatMessage;
use crate::app::state::render_budget::{RenderCacheEvictionKey, RenderCacheSlotState};
use crate::app::state::types::{
    CancelOrigin, HistoryRetentionPolicy, HistoryRetentionStats, McpState, ModeState,
    RecentSessionInfo, SessionUsageState, TodoItem, ToolCallScope, UsageState,
};
use crate::app::state::viewport::ChatViewport;
use crate::app::state::{ChatRenderTraceState, TerminalToolCallRef, TurnNoticeRef};
pub use forge_primitives::runtime::SessionLifecycleState;
use forge_primitives::runtime::{RuntimeSessionState, SessionTurnState};
use forge_primitives::{AccountInfo, SessionId};

/// Per-session runtime state. Initialised when a session connects;
/// dropped when the session is closed or forge-tui exits.
///
/// `Default` is hand-rolled (rather than derived) because
/// [`Self::last_activity_at`] is an [`Instant`] which has no
/// `Default` impl. Every other field falls through to its type's
/// `Default::default()`; if a field needs a non-default
/// initializer, factor it through [`UiSession::new`] rather than
/// expanding the manual impl.
///
/// Owns the operational state TUI renders. Workspace is a thin
/// proxy that holds routing metadata (AgentHandle pool, command
/// senders, pending interactions) but never duplicates these
/// fields. TUI reducers in `app::events::*` update them from
/// `SessionUpdate` payloads as events arrive.
#[allow(clippy::struct_excessive_bools)]
pub struct UiSession {
    /// The claude-issued session UUID, also used as the map key.
    /// Stored here for symmetry; the map lookup uses the same value.
    pub key: Option<SessionKey>,
    /// TUI-side mirror of the workspace's authoritative `session_id`.
    /// Workspace stamps the real id onto `DomainSession.session_id`
    /// (for `AgentHandle` dispatch); TUI mirrors it here for render
    /// code so no per-frame workspace lock is needed.
    pub session_id: Option<SessionId>,
    /// Lifecycle state for the Projects pane glyph + the render loop's
    /// "is_animating" probe. Updated by the TUI reducers in
    /// `app::events::*` as each `SessionUpdate` arrives.
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
    /// `RequiresAction`).
    pub runtime_session_state: Option<RuntimeSessionState>,
    /// Wall-clock instant of the last wire event applied to this
    /// session. Seeded at bucket creation so the Projects pane's
    /// "2m" / "1h" / "5d" rendering has a stable baseline before
    /// the first event arrives.
    pub last_activity_at: Instant,
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
    /// Fast mode state telemetry from the SDK.
    pub fast_mode_state: model::FastModeState,
    /// Latest config options observed from bridge `config_option_update` events.
    pub config_options: BTreeMap<String, serde_json::Value>,
    /// Session-wide usage and cost telemetry from the bridge.
    pub session_usage: SessionUsageState,

    // ---- Account / auth ----
    /// OAuth credentials snapshot from the bridge — populated at
    /// session connect, refreshed after `/login` and `/logout` so
    /// callers can ask "is the user authenticated?" without doing
    /// their own filesystem walk to `<config_dir>/.credentials.json`.
    pub oauth_credentials: Option<forge_primitives::cloud::oauth_credentials::OauthCredentials>,

    // ---- Filesystem ----
    /// Display-friendly cwd (`~/foo` form) used by status panel /
    /// footer / welcome card.
    pub cwd: String,
    /// Number of files accessed during the active turn (incremented
    /// on Read/Edit/Write tool starts, reset on TurnComplete).
    pub files_accessed: usize,
    /// Git repo context used by footer/status rendering and live
    /// branch tracking. Mutated by bridge-pushed git context
    /// snapshots.
    pub(crate) git_context: GitContextState,
    /// Config > MCP live server snapshot and refresh lifecycle.
    pub mcp: McpState,
    /// Anthropic plan usage snapshot and refresh lifecycle. Per-session
    /// rather than per-account: each session fetches independently
    /// (idempotent + TTL-gated; redundant fetches across same-account
    /// sessions are cheap). Read by the Projects-pane account/status
    /// panel and the `/usage` config tab. Routed by `SessionKey` in
    /// the `Usage*` `SessionUpdate` envelopes so an in-flight fetch
    /// that lands after the user has switched sessions still writes
    /// to the bucket that requested it.
    pub usage: UsageState,
    /// Catalog of resumable sessions for this bucket's project,
    /// produced by `forge_sdk_worker::list_recent_sessions` against
    /// the bucket's `cwd`. Drives the startup `/resume` picker and
    /// `/resume <id>` autocomplete. Per-session so the autocomplete
    /// always lists the active project's sessions even when the user
    /// has switched mid-session.
    pub recent_sessions: Vec<RecentSessionInfo>,

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

    /// Per-session input editor. Each session owns its own input
    /// state; switching the active session naturally swaps the editor
    /// because the accessor reads from this bucket. Replaces the
    /// pre-Phase-6 App-level `input` field plus the per-bucket
    /// `draft_input` snapshot/restore dance in `switch_active_session`.
    pub input: InputState,
}

impl UiSession {
    #[must_use]
    pub fn new(key: SessionKey) -> Self {
        Self { key: Some(key), last_activity_at: Instant::now(), ..Self::default() }
    }
}

impl Default for UiSession {
    fn default() -> Self {
        // `Instant` has no `Default` impl, so the derive is replaced
        // with a hand-rolled version that seeds `last_activity_at`
        // to "now" and falls through to `Default::default()` for
        // every other field via destructuring of an internal
        // synthesizer. The shape stays maintainable: any field added
        // to `UiSession` whose type does have `Default` lands here
        // for free without code change.
        Self {
            key: Option::default(),
            session_id: Option::default(),
            lifecycle_state: SessionLifecycleState::default(),
            cwd_raw: String::default(),
            session_scope_epoch: u64::default(),
            turn_state: SessionTurnState::default(),
            account_info: Option::default(),
            active_account_display_name: Option::default(),
            runtime_session_state: Option::default(),
            last_activity_at: Instant::now(),
            messages: Vec::default(),
            message_retained_bytes: Vec::default(),
            retained_history_bytes: usize::default(),
            viewport: ChatViewport::default(),
            active_turn_assistant_message_idx: Option::default(),
            is_compacting: bool::default(),
            pending_compact_clear: bool::default(),
            pending_interaction_ids: Vec::default(),
            cancelled_turn_pending_hint: bool::default(),
            pending_cancel_origin: Option::default(),
            prompt_suggestion: Option::default(),
            last_rate_limit_update: Option::default(),
            turn_notice_refs: Vec::default(),
            active_task_ids: HashSet::default(),
            tool_call_scopes: HashMap::default(),
            tool_call_index: HashMap::default(),
            terminals: TerminalMap::default(),
            terminal_tool_calls: Vec::default(),
            terminal_tool_call_membership: HashSet::default(),
            subagent_attribution: HashMap::default(),
            current_model: Option::default(),
            available_models: Vec::default(),
            available_commands: Vec::default(),
            available_agents: Vec::default(),
            mode: Option::default(),
            observed_permission_mode: Option::default(),
            observed_effort: Option::default(),
            observed_assistant_model: Option::default(),
            fast_mode_state: model::FastModeState::default(),
            config_options: BTreeMap::default(),
            session_usage: SessionUsageState::default(),
            oauth_credentials: Option::default(),
            cwd: String::default(),
            files_accessed: usize::default(),
            git_context: GitContextState::default(),
            mcp: McpState::default(),
            usage: UsageState::default(),
            recent_sessions: Vec::default(),
            todos: Vec::default(),
            show_todo_panel: bool::default(),
            todo_scroll: usize::default(),
            todo_selected: usize::default(),
            cached_todo_compact: Option::default(),
            render_cache_slots: Vec::default(),
            render_cache_total_bytes: usize::default(),
            render_cache_protected_bytes: usize::default(),
            render_cache_evictable: BTreeSet::default(),
            render_cache_tail_msg_idx: Option::default(),
            history_retention: HistoryRetentionPolicy::default(),
            history_retention_stats: HistoryRetentionStats::default(),
            cache_metrics: CacheMetrics::default(),
            last_active_turn_height_state: Option::default(),
            last_chat_render_trace_state: Option::default(),
            input: InputState::default(),
        }
    }
}

// Phase 4 deleted the `pub type Session = UiSession;` back-compat
// alias; the ~250 call sites that used `UiSession::new(...)` etc.
// were migrated to `UiSession::new(...)`. `UiSession` owns the
// operational state TUI renders; workspace's `DomainSession` holds
// only the routing metadata (`AgentHandle` slot, `session_id`,
// pending interactions).

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
