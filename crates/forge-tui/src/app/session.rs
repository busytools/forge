//! Per-session state bucket.
//!

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use forge_workspace::SessionKey;

use crate::agent::events::TerminalMap;
use crate::agent::model;
use crate::app::file_index::FileIndexState;
use crate::app::input::InputSnapshot;
use crate::app::input::InputState;
use crate::app::state::cache_metrics::CacheMetrics;
use crate::app::state::messages::ChatMessage;
use crate::app::state::render_budget::{RenderCacheEvictionKey, RenderCacheSlotState};
use crate::app::state::types::{
    HistoryRetentionPolicy, HistoryRetentionStats, LoginHint, McpState, ModeState, MonitorEntry,
    PasteSessionState, PendingCommandAck, RecentSessionInfo, SelectionState, SessionUsageState,
    StopHookSummaryState, TodoItem, ToolCallScope, UsageState, WorkflowEntry,
};
use crate::app::state::viewport::ChatViewport;
use crate::app::state::{ChatRenderTraceState, TerminalToolCallRef, TurnNoticeRef};
pub use forge_primitives::runtime::SessionLifecycleState;
use forge_primitives::runtime::{RuntimeSessionState, SessionTurnState};
use forge_primitives::{AccountInfo, PeerInflightStats, SessionId};

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
    /// Peer-coordination in-flight counters (#114). Mirrors
    /// `Workspace.peer_stats[key]`; updated by the reducer arm for
    /// `SessionUpdate::PeerInflightStatsChanged`. Drives the
    /// sidebar peer-activity badges in [`crate::ui::projects_pane`].
    pub peer_badges: PeerInflightStats,
    /// Last instant at which `peer_badges.timed_out` or
    /// `peer_badges.delivery_failed` incremented. Used to fade the
    /// transient failure indicators 60 s after they fire so the
    /// sidebar doesn't stay red forever.
    pub peer_badges_last_failure_at: Option<Instant>,
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
    /// Set when a cancel notification succeeds; consumed on
    /// `TurnComplete` to render a red interruption hint in chat.
    pub cancelled_turn_pending_hint: bool,
    /// Origin of the in-flight cancellation request, if any.
    pub pending_cancel: bool,
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
    pub observed_permission_mode: Option<forge_workspace::PermissionMode>,
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
    /// Config > MCP live server snapshot and refresh lifecycle.
    pub mcp: McpState,
    /// Anthropic plan usage snapshot and refresh lifecycle. Per-session
    /// rather than per-account: each session fetches independently
    /// (idempotent + TTL-gated; redundant fetches across same-account
    /// sessions are cheap). Read by the Projects-pane account/status
    /// panel. Routed by `SessionKey` in the `Usage*` `SessionUpdate`
    /// envelopes so an in-flight fetch that lands after the user has
    /// switched sessions still writes to the bucket that requested it.
    pub usage: UsageState,
    /// Catalog of resumable sessions for this bucket's project,
    /// produced by `forge_sdk_worker::list_recent_sessions` against
    /// the bucket's `cwd`. Drives the startup `/resume` picker and
    /// `/resume <id>` autocomplete. Per-session so the autocomplete
    /// always lists the active project's sessions even when the user
    /// has switched mid-session.
    pub recent_sessions: Vec<RecentSessionInfo>,
    /// File index for `@`-mention autocomplete. Scans the bucket's
    /// `cwd` and updates incrementally via the workspace-wide
    /// `FileIndexEvent` channel (`App::file_index_event_tx`).
    /// Per-session because the index is project-scoped — switching
    /// active session via the Projects pane must show the new
    /// project's files, not the previous project's.
    pub file_index: FileIndexState,

    // ---- Latent smells migrated to per-session in the same pass ----
    /// Pending `/login` hint shown above the input. Per-session so
    /// an auth-required prompt in one session doesn't leak into
    /// another session's input area when the user switches.
    pub login_hint: Option<LoginHint>,
    /// Session id currently being resumed via `/resume`. Per-session
    /// so the resume marker doesn't follow the user across switches.
    pub resuming_session_id: Option<String>,
    /// Spinner label shown while a slash command is in flight
    /// (`CommandPending` status). Per-session.
    pub pending_command_label: Option<String>,
    /// Ack marker required to clear `CommandPending` for strict
    /// completion semantics. Per-session.
    pub pending_command_ack: Option<PendingCommandAck>,
    /// Active text selection (mouse-driven). Per-session so a
    /// selection started in one session doesn't render in another
    /// after a switch.
    pub selection: Option<SelectionState>,
    /// Deferred plain-Enter submit state for the current input.
    /// Per-session.
    pub pending_submit: Option<InputSnapshot>,
    /// Buffered `Event::Paste` payload for this drain cycle.
    /// Per-session because pastes belong to the editor that
    /// received them.
    pub pending_paste_text: String,
    /// Pending paste session metadata for the currently queued
    /// paste payload. Per-session.
    pub pending_paste_session: Option<PasteSessionState>,
    /// Most recent active placeholder paste session. Per-session.
    pub active_paste_session: Option<PasteSessionState>,
    /// Monotonic counter for paste session identifiers. Per-session.
    pub next_paste_session_id: u64,
    /// Pending image attachments queued via Ctrl+V and consumed on
    /// submit. Per-session because they belong to the editor that
    /// received the paste.
    pub pending_images: Vec<crate::app::clipboard_image::ImageAttachment>,
    /// Active `@`-mention autocomplete state. Per-session because
    /// the dropdown belongs to this bucket's input editor.
    pub mention: Option<crate::app::mention::MentionState>,
    /// Active slash-command autocomplete state. Per-session.
    pub slash: Option<crate::app::slash::SlashState>,
    /// Active subagent autocomplete state (`&name`). Per-session.
    pub subagent: Option<crate::app::subagent::SubagentState>,

    // ---- Tasks ----
    /// Current task list from Claude's `TaskCreate` / `TaskUpdate`
    /// tool calls (#268). Rendered by the inspector pane on the
    /// right side of the chat view.
    pub todos: Vec<TodoItem>,

    /// Cumulative thinking-token count for the current in-flight
    /// turn (#273). Set by the `Message::ThinkingTokens` reducer;
    /// cleared on `Message::Result` (turn end). When `Some`, the
    /// spinner-chip renders as `⠋ thinking · 1.2k tok`.
    pub latest_thinking_tokens: Option<u64>,

    /// Last completed turn's wall-clock duration in milliseconds
    /// from `Message::TurnDuration` (#273). Rendered as the banner
    /// chip `Claude · 12.4s` next to the assistant role label on
    /// the active turn.
    pub last_turn_duration_ms: Option<u64>,

    /// Latest `Message::StopHookSummary` for the current turn
    /// (#273), bound to the assistant message id. Rendered as a
    /// collapsed 1-liner `↳ hook summary · N actions [▶ expand]`
    /// at end-of-turn when `actions > 0`. Cleared on session reset.
    pub last_stop_hook_summary: Option<StopHookSummaryState>,

    /// Per-message expansion state for the stop-hook summary
    /// surface (#273). Click `[▶ expand]` toggles the entry.
    pub stop_hook_summary_expanded: std::collections::HashMap<usize, bool>,

    /// #273 Task 8: in-flight Monitor entries surfaced as the
    /// Inspector MONITORS section + the chat one-liner notices.
    /// Populated when a `Monitor` tool_use enters the assistant
    /// stream; mutated on terminal lifecycle events. The MONITORS
    /// section drops out entirely (`append_monitors_section`
    /// early-returns) when no entry is `Running`. Output lines
    /// arrive through the tail-feed wiring and capped at
    /// `MonitorEntry::OUTPUT_TAIL_MAX` per entry.
    pub monitors: Vec<MonitorEntry>,

    /// #273 Task 9: in-flight Workflow entries surfaced as the
    /// Inspector WORKFLOWS section + the chat one-liner notice.
    /// Populated when a `Workflow` tool_use enters the assistant
    /// stream; per-phase state mutated from each `task_progress`
    /// event carrying a `workflow_progress` snapshot. Auto-clears
    /// once every entry transitions out of `InProgress`.
    pub workflows: Vec<WorkflowEntry>,

    // ---- Git diff snapshot (Inspector GIT section) ----
    /// Latest poll result. `None` until the first scan completes
    /// (post-Connect). Replaces the retired `GitContextWatcher`
    /// branch push — the snapshot carries branch info too.
    pub git_diff_snapshot: Option<forge_primitives::git_diff::GitDiffSnapshot>,
    /// Generation epoch. Bumped on cwd change (Connected,
    /// SessionReplaced). Spawned scanner echoes it into its event
    /// so `drain_events` can drop stale results from a previous
    /// cwd.
    pub git_diff_generation: u64,
    /// In-flight scan guard. Spawned scanner task sets to `true`
    /// before running, clears on exit. `request_refresh` early-
    /// returns when this is already `true`.
    pub git_diff_scan_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// When the latest snapshot was applied. Used by the
    /// switch-session refresh hook to decide whether to refresh
    /// on `active_session_key` change.
    pub git_diff_last_refreshed_at: Option<std::time::Instant>,

    // ---- Process snapshot (Inspector PROCESSES section, OS walk) ----
    /// Latest sysinfo-walk snapshot of claude's descendant tree.
    /// `None` until the first scan completes. Mirrors `git_diff_snapshot`
    /// but holds OS-level process state instead of git state.
    pub process_snapshot: Option<forge_workspace::env::processes::ProcessSnapshot>,
    /// Generation epoch for the process scanner. Bumped on session
    /// swap so a scan kicked off against the old `claude_pid` can be
    /// dropped if it lands after the swap.
    pub process_scan_generation: u64,
    /// In-flight scan guard. `request_refresh` short-circuits when
    /// already `true`.
    pub process_scan_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// When the latest snapshot was applied. Drives the
    /// staleness rule in `process_scanner::should_refresh`.
    pub process_last_refreshed_at: Option<std::time::Instant>,

    // ---- Inspector pane scroll state ----
    /// Vertical scroll offset (lines from top) for the Inspector
    /// pane's scrollable body — everything from the `GIT` section
    /// downward. The banner + rule above it stay pinned. Lives
    /// per-session so switching sessions and coming back preserves
    /// where the user was looking, same shape as chat scrollback.
    /// Reset to `0` on session creation; mouse wheel + future
    /// keyboard nav mutate it.
    pub inspector_scroll_offset: u16,

    /// Last connection error captured when this bucket transitioned
    /// to [`SessionLifecycleState::Failed`]. `None` for any other
    /// lifecycle state. The launchpad picker renders this beneath
    /// a failed project row; the chat view doesn't surface it yet.
    pub last_connection_error: Option<String>,

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
    /// state; switching the active session naturally swaps the
    /// editor because the accessor reads from this bucket.
    pub input: InputState,

    /// Per-session FIFO queue of pending prompts. The dock shows
    /// `prompt_queue.front()` when this session is active.
    pub prompt_queue: std::collections::VecDeque<crate::app::prompt::PromptState>,
}

impl UiSession {
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
            peer_badges: PeerInflightStats::default(),
            peer_badges_last_failure_at: Option::default(),
            messages: Vec::default(),
            message_retained_bytes: Vec::default(),
            retained_history_bytes: usize::default(),
            viewport: ChatViewport::default(),
            active_turn_assistant_message_idx: Option::default(),
            is_compacting: bool::default(),
            pending_compact_clear: bool::default(),
            cancelled_turn_pending_hint: bool::default(),
            pending_cancel: false,
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
            mcp: McpState::default(),
            usage: UsageState::default(),
            recent_sessions: Vec::default(),
            file_index: FileIndexState::default(),
            login_hint: Option::default(),
            resuming_session_id: Option::default(),
            pending_command_label: Option::default(),
            pending_command_ack: Option::default(),
            selection: Option::default(),
            pending_submit: Option::default(),
            pending_paste_text: String::default(),
            pending_paste_session: Option::default(),
            active_paste_session: Option::default(),
            next_paste_session_id: 1, // Match the legacy App-level seed.
            pending_images: Vec::default(),
            mention: Option::default(),
            slash: Option::default(),
            subagent: Option::default(),
            todos: Vec::default(),
            latest_thinking_tokens: None,
            last_turn_duration_ms: None,
            last_stop_hook_summary: None,
            stop_hook_summary_expanded: std::collections::HashMap::default(),
            monitors: Vec::default(),
            workflows: Vec::default(),
            git_diff_snapshot: None,
            git_diff_generation: 0,
            git_diff_scan_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            git_diff_last_refreshed_at: None,
            process_snapshot: None,
            process_scan_generation: 0,
            process_scan_in_flight: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            process_last_refreshed_at: None,
            inspector_scroll_offset: 0,
            last_connection_error: Option::default(),
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
            prompt_queue: std::collections::VecDeque::new(),
        }
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
