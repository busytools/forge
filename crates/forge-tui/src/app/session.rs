//! Per-session state bucket.
//!

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use forge_workspace::SessionKey;

use crate::agent::model;
use crate::app::file_index::FileIndexState;
use crate::app::input::InputSnapshot;
use crate::app::input::InputState;
use crate::app::state::cache_metrics::CacheMetrics;
use crate::app::state::messages::{ChatMessage, LiveTurn};
use crate::app::state::render_budget::{RenderCacheEvictionKey, RenderCacheSlotState};
use crate::app::state::types::{
    BackgroundTask, HistoryRetentionPolicy, HistoryRetentionStats, LoginHint, McpState, ModeState,
    MonitorEntry, PasteSessionState, PendingCommandAck, RecentSessionInfo, SelectionState,
    SessionUsageState, StopHookSummaryState, TodoItem, ToolCallScope, UsageState, WorkflowEntry,
};
use crate::app::state::viewport::ChatViewport;
use crate::app::state::{ChatRenderTraceState, TurnNoticeRef};
pub use forge_primitives::runtime::SessionLifecycleState;
use forge_primitives::runtime::{RuntimeSessionState, SessionTurnState};
use forge_primitives::{AccountInfo, PeerInflightStats, SessionId};

/// Per-session runtime state. Initialised when a session connects;
/// dropped when the session is closed or forge-tui exits.
///
/// `Default` is hand-rolled (rather than derived) because
/// `next_paste_session_id` seeds to 1, not 0. Every other field falls
/// through to its type's `Default::default()`; if a field needs a
/// non-default initializer, factor it through [`UiSession::new`]
/// rather than expanding the manual impl.
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
    /// This session's `/dictate` normalizer-axis overrides, mirrored
    /// from the workspace's `DomainSession` via
    /// `SessionUpdate::DictateOverrides` echoes. The `/dictate`
    /// dialog's markers and reset row read from here.
    pub dictate_overrides: forge_workspace::DictateOverrides,
    /// This session's `/dictate` input-device pick, mirrored from the
    /// workspace via `SessionUpdate::DictateDevicePin` echoes. The
    /// dialog's Device row reads from here; `None` means the
    /// configured pin stands.
    pub dictate_device_pin: Option<forge_workspace::DictateDeviceChoice>,
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
    /// forge.toml project NAME this tab belongs to (equals
    /// `CronEntry.project_name` / `ProjectView.name`), stamped once the
    /// bucket resolves to a project and `None` pre-Connect. Scopes the
    /// Inspector SCHEDULES + GOTIFY snapshots by name rather than
    /// re-deriving the project from `cwd_raw` (fragile for empty /
    /// synthetic / tilde / worktree cwd forms).
    pub project: Option<String>,
    /// Monotonic session authority epoch - bumped on each session
    /// reset (`/new`, login, logout) so stale async view data can be
    /// ignored.
    pub session_scope_epoch: u64,
    /// SDK turn state - model-resolution cache, mode capability,
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
    /// Pre-apply snapshot taken by the optimistic `/mode` apply, used
    /// to roll the chip back when the CLI refuses the switch
    /// (`SessionUpdate::SetModeFailed`).
    pub pending_mode_rollback: Option<ModeRollback>,
    /// Pre-apply snapshot taken by the optimistic `/model` apply, used
    /// to roll the chip back when the CLI refuses the switch
    /// (`SessionUpdate::SetModelFailed`).
    pub pending_model_rollback: Option<ModelRollback>,
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
    /// Latest config options observed from bridge `config_option_update` events.
    pub config_options: BTreeMap<String, serde_json::Value>,
    /// Session-wide usage and cost telemetry from the bridge.
    pub session_usage: SessionUsageState,

    // ---- Account / auth ----
    /// OAuth credentials snapshot from the bridge - populated at
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
    /// Per-session because the index is project-scoped - switching
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

    /// Estimated thinking tokens accumulated so far in the current
    /// in-flight turn (#273), summed from the `ThinkingTokens` deltas.
    /// Cleared at each turn boundary, which is why the turn info row
    /// mirrors it onto the message rather than reading it at render.
    pub latest_thinking_tokens: Option<u64>,

    /// Latest `Message::StopHookSummary` for the current turn
    /// (#273), bound to the assistant message id. Rendered as a
    /// collapsed 1-liner `↳ hook summary · N actions [▶ expand]`
    /// at end-of-turn when `actions > 0`. Cleared on session reset.
    pub last_stop_hook_summary: Option<StopHookSummaryState>,

    /// Per-message expansion state for the stop-hook summary
    /// surface (#273). Click `[▶ expand]` toggles the entry.
    pub stop_hook_summary_expanded: std::collections::HashMap<usize, bool>,

    /// Live accounting for the turn in flight, feeding the turn-info
    /// row while it counts up. Reset when a turn starts and again when
    /// its Result settles.
    pub live_turn: LiveTurn,

    /// Typed submits sent while a turn was in flight whose turn has
    /// not been observed to start yet (the TUI twin of
    /// `DomainSession.turn_pending`). The CLI emits nothing on the
    /// wire for a queued turn until its first token, so the
    /// idle-settling paths consult this to keep the spinner open
    /// across the gap instead of settling to Ready/Idle.
    pub queued_turn_sends: usize,

    /// Set when turn-complete re-opened the session for a queued
    /// send. The next live assistant envelope is that queued turn
    /// starting; it consumes one send and clears this.
    pub queued_turn_awaiting_start: bool,

    /// Wall clock past which a re-opened queued turn with no
    /// assistant envelope is force-settled, so a desync cannot
    /// strand a spinner forever.
    pub queued_turn_force_settle_at: Option<std::time::SystemTime>,

    /// Set by the force-settle sweep. Nothing on the wire
    /// distinguishes a dead queued turn from a slow one, so a live
    /// envelope arriving after expiry re-opens the session through
    /// this flag instead of the settle being read as a verdict.
    pub queued_turn_force_settled: bool,

    /// `Message::Result.duration_api_ms` from the previous Result in
    /// this session. That counter is cumulative over the session, so
    /// the next turn's API time is its delta against this.
    pub prev_duration_api_ms: Option<u64>,

    /// In-flight Monitor entries surfaced as the
    /// Inspector MONITORS section + the chat one-liner notices.
    /// Populated when a `Monitor` tool_use enters the assistant
    /// stream; mutated on terminal lifecycle events. The MONITORS
    /// section drops out entirely (`append_monitors_section`
    /// early-returns) when no entry is `Running`. Output lines
    /// arrive through the tail-feed wiring and capped at
    /// `MonitorEntry::OUTPUT_TAIL_MAX` per entry.
    pub monitors: Vec<MonitorEntry>,

    /// CLI-authoritative background-task snapshot. `local_bash` entries
    /// feed the Inspector PROCESSES section (agents / workflows surface
    /// in SUBAGENTS / WORKFLOWS). Replaced wholesale on each
    /// `background_tasks_changed` event. Session-scoped because
    /// background tasks outlive the turn that spawned them.
    pub background_tasks: Vec<BackgroundTask>,

    /// Session-scoped `task_id` -> `tool_use_id`, mirroring
    /// `SessionTurnState::task_tool_use_ids` but surviving turn
    /// finalisation. Populated at `task_started` (when the mapping is live);
    /// the turn-scoped copy is wiped every turn-complete, so surfaces driven
    /// by the session-scoped `background_tasks` registry - SUBAGENTS
    /// backgrounded-agent liveness and the PROCESSES `local_bash` feed -
    /// resolve a task that outlived its turn through this map instead. Both
    /// consumers INTERSECT it with the registry (the authoritative gate), so
    /// a stale entry is inert, never a phantom live row. Cleanup is split by
    /// kind: an agent's entry is dropped at its `task_notification`; a
    /// rostered non-agent's when it leaves `background_tasks` (the roster
    /// diff). An entry for a task that never enters the roster and gets no
    /// `task_notification` persists until session reset - bounded and inert.
    pub session_task_tool_use_ids: std::collections::HashMap<String, String>,

    /// Agent-kind tasks this session saw backgrounded and that no
    /// terminal `task_updated` / `task_notification` has cleared. The
    /// roster replaces wholesale and can arrive after the spawning
    /// turn's Result, so a snapshot-only read collapses the subagent
    /// exemption on a badly-timed frame and nothing re-registers it
    /// (#790). This is the historical half of the liveness signal: set
    /// when the task first reports backgrounded, cleared only by a
    /// terminal event, session teardown, or a turn error.
    pub backgrounded_roots: HashSet<String>,

    /// Pending time-based schedules (`ScheduleWakeup` + `CronCreate`)
    /// surfaced in the Inspector SCHEDULES section. Pruned by the
    /// ~1s timer tick via `App::prune_expired_schedules`.
    pub schedules: Vec<crate::app::state::types::ScheduleEntry>,

    /// In-flight Workflow entries surfaced as the
    /// Inspector WORKFLOWS section + the chat one-liner notice.
    /// Populated when a `Workflow` tool_use enters the assistant
    /// stream; per-phase state mutated from each `task_progress`
    /// event carrying a `workflow_progress` snapshot. Auto-clears
    /// once every entry transitions out of `InProgress`.
    pub workflows: Vec<WorkflowEntry>,

    // ---- Git diff snapshot (Inspector GIT section) ----
    /// Latest poll result. `None` until the first scan completes
    /// (post-Connect). Replaces the retired `GitContextWatcher`
    /// branch push - the snapshot carries branch info too.
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
    /// Generation epoch for the process scanner. Bumped alongside
    /// `git_diff_generation` when a Connected delivers a changed cwd,
    /// so a scan kicked off against the old `claude_pid` is dropped
    /// if it lands after the swap.
    pub process_scan_generation: u64,
    /// In-flight scan guard. `request_refresh` short-circuits when
    /// already `true`.
    pub process_scan_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// When the latest snapshot was applied. Drives the
    /// staleness rule in `process_scanner::should_refresh`.
    pub process_last_refreshed_at: Option<std::time::Instant>,

    /// Per-group collapse level for the chat tool-call grouping
    /// feature. Absent key means default L2 (summary). Keyed by the
    /// leading tool's `tool_use_id` so the identity is stable across
    /// renders (blocks are append-only after construction).
    pub group_collapse_levels: std::collections::HashMap<
        crate::ui::message::grouping::GroupId,
        crate::ui::message::grouping::GroupCollapseLevel,
    >,

    /// Per-messaging-group collapse level for the chat messaging
    /// grouping feature. Sibling of `group_collapse_levels` but keyed
    /// separately so tool-group and messaging-group leader ids never
    /// collide on the same hashmap. Cleared on Cmd+X by
    /// `toggle_all_tool_calls` alongside the tool-group map.
    pub messaging_group_collapse_levels: std::collections::HashMap<
        crate::ui::message::grouping::GroupId,
        crate::ui::message::grouping::GroupCollapseLevel,
    >,

    // ---- Inspector pane scroll state ----
    /// Vertical scroll offset (lines from top) for the Inspector
    /// pane's scrollable body - everything from the `GIT` section
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

    /// This session's chat draft, parked while its dock is morphed into
    /// the prompt widget and restored when `prompt_queue` drains. Lives
    /// on the bucket rather than on `App` so a draft captured under one
    /// session can never be restored into another.
    pub input_draft_snapshot: Option<String>,

    /// Set when a turn on this session died, cleared when the user
    /// switches to the session or it starts another turn. Drives the
    /// Inspector NEEDS ATTENTION row and the Projects-pane `✕`.
    pub failed_turn: Option<crate::app::FailedTurn>,

    /// Worker answers on this session's reviews still owed a reviewer
    /// turn. Drives the Inspector GIT header badge (active session) and
    /// the NEEDS ATTENTION row (backgrounded). Persists until the
    /// reviewer replies, resolves or reopens - not on merely opening
    /// `/diff`.
    pub review_replies_waiting: Option<crate::app::ReviewRepliesWaiting>,

    /// Whether the `app::review_waiting` boot recompute has reached an
    /// ANSWER for this session - including "nothing waiting". A read git
    /// could not complete never sets it, so a timed-out `rev-parse` at
    /// boot leaves the session queued rather than dark for the run.
    pub review_waiting_settled: bool,

    /// Whether that recompute is running right now, so the ~1s ticker
    /// doesn't stack a second `git` call on top of the first. Cleared by
    /// an RAII guard on task exit, so a panic mid-call cannot strand it.
    pub review_waiting_in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>,

    /// How many reads have failed for this session. Bounds the retries a
    /// checkout git cannot read gets, since a plain non-git project is
    /// indistinguishable from a timeout at that call.
    pub review_waiting_failed_reads: u8,

    /// Classification of the most recent `api_retry` on the in-flight
    /// turn, so a turn error that follows exhausted retries can name
    /// what actually killed it. Cleared when a turn starts.
    pub last_api_retry: Option<(forge_primitives::ApiRetryError, Option<u16>)>,

    /// When forge's next continuation turn for this session is due,
    /// after a transient server error killed the last one. See
    /// `crate::app::events::auto_continue`.
    pub auto_continue_due_at: Option<std::time::SystemTime>,

    /// Continuations forge has already sent for the current failure
    /// streak. Capped at `auto_continue::MAX_ATTEMPTS`; reset when a
    /// turn completes.
    pub auto_continue_attempts: u32,

    /// Active dictation take on this session's composer, if any. The
    /// take is bound to the bucket: results land here regardless of
    /// which tab is focused when the transcript resolves.
    pub(crate) dictate: Option<crate::app::dictate::DictateIndicator>,
    /// The composer border's eased colour during a take and its
    /// afterglow. Dropped once the ease is back at the normal orange.
    pub(crate) dictate_border: Option<crate::app::dictate::DictateBorder>,
    /// Post-take notice row and the input `content_version` it was
    /// stamped under. The next keystroke bumps the version, which is
    /// what clears the notice - no key path needs to know about it.
    pub(crate) dictate_notice: Option<crate::app::dictate::DictateNotice>,
    pub(crate) dictate_notice_version: u64,
}

impl UiSession {
    pub fn new(key: SessionKey) -> Self {
        Self { key: Some(key), ..Self::default() }
    }

    /// Stamp a post-take notice against the current draft version, so
    /// it renders until the next keystroke.
    pub(crate) fn set_dictate_notice(&mut self, notice: crate::app::dictate::DictateNotice) {
        self.dictate_notice_version = self.input.content_version;
        self.dictate_notice = Some(notice);
    }

    /// The notice to render, if the draft has not been edited since it
    /// was stamped.
    pub(crate) fn visible_dictate_notice(&self) -> Option<&crate::app::dictate::DictateNotice> {
        let stamped = self.dictate_notice_version;
        self.dictate_notice.as_ref().filter(|_| self.input.content_version == stamped)
    }

    /// True while the session has a live backgrounded task (bash / agent /
    /// workflow). The CLI keeps `background_tasks` to the currently-live
    /// set, replacing it wholesale on each `background_tasks_changed`, so a
    /// non-empty registry means work is happening even after the spawning
    /// turn has completed. Drives the Projects-pane activity spinner
    /// alongside the turn-driven lifecycle state.
    pub fn has_live_background_work(&self) -> bool {
        !self.background_tasks.is_empty()
    }

    /// Drop the CLI-fed background-task registry, its task-id ->
    /// tool-use-id mirror, and the sticky backgrounded roots. The three
    /// are cleared together: on session teardown (connection failure,
    /// reset) no terminal event can ever arrive, so without this the
    /// registry - and the activity spinner + frame-tick it drives -
    /// would stay stale forever.
    pub fn clear_background_task_registry(&mut self) {
        self.background_tasks.clear();
        self.session_task_tool_use_ids.clear();
        self.backgrounded_roots.clear();
    }

    /// The `tool_use_id`s of every currently-backgrounded task the CLI still
    /// lists as running, across all task kinds. Intersecting the
    /// `background_tasks` roster (the authoritative liveness gate) with the
    /// session task map (`task_id` -> `tool_use_id`) is the signal that
    /// survives turn finalisation; a map entry whose task already left the
    /// roster is excluded, so a leaked mapping never resurrects a phantom
    /// live row. Sticky roots ([`Self::backgrounded_roots`]) union in on
    /// top; each is credited only while its session-map entry still exists,
    /// so a roster departure ends a root's liveness ahead of its terminal
    /// event.
    pub fn backgrounded_alive_tool_use_ids(&self) -> HashSet<&str> {
        let task_ids: HashSet<&str> =
            self.background_tasks.iter().map(|task| task.task_id.as_str()).collect();
        let mut alive: HashSet<&str> = self
            .session_task_tool_use_ids
            .iter()
            .filter(|(task_id, _)| task_ids.contains(task_id.as_str()))
            .map(|(_, tool_use_id)| tool_use_id.as_str())
            .collect();
        alive.extend(
            self.backgrounded_roots
                .iter()
                .filter(|id| self.session_task_tool_use_ids.values().any(|v| v == *id))
                .map(|id| id.as_str()),
        );
        alive
    }

    /// [`Self::backgrounded_alive_tool_use_ids`] plus everything hanging
    /// off those roots at any depth, which is what a turn-boundary sweep
    /// must spare: only the root gets a `TaskStarted` and a roster row,
    /// so anything under a live backgrounded subagent is invisible to the
    /// roster and would be swept to a terminal status - `Completed` at a
    /// turn's Result, `Failed` on a cancel - while it runs.
    ///
    /// Depth matters because a `Task` issued by a subagent registers as
    /// that subagent's child rather than a root, so its own children
    /// reach the roster only through the chain above them.
    /// `clear_tool_scope_tracking` and the three sweeps all take this
    /// set; `subagents_view` still resolves one hop.
    pub fn backgrounded_alive_with_children(&self) -> HashSet<String> {
        let roots = self.backgrounded_alive_tool_use_ids();
        let mut alive: HashSet<String> = roots.iter().map(|id| (*id).to_owned()).collect();
        alive.extend(
            self.tool_call_scopes
                .keys()
                .filter(|id| self.resolves_to_live_root(id, &roots))
                .cloned(),
        );
        alive
    }

    /// Whether a tool call hangs off one of `roots`, at any depth. A `Task`
    /// issued by a subagent registers as that subagent's child rather than
    /// a root, so its own children reach the roster only through the chain
    /// above them; walking one hop leaves them looking unowned and they get
    /// swept while they run.
    fn resolves_to_live_root(&self, id: &str, roots: &HashSet<&str>) -> bool {
        let mut cursor = id;
        // The chain cannot exceed the map, and a cycle would otherwise spin.
        for _ in 0..self.tool_call_scopes.len() {
            match self.tool_call_scopes.get(cursor) {
                Some(ToolCallScope::SubagentChild { parent_tool_use_id }) => {
                    if roots.contains(parent_tool_use_id.as_str()) {
                        return true;
                    }
                    cursor = parent_tool_use_id.as_str();
                }
                _ => return false,
            }
        }
        false
    }

    /// Flip every open tool call hanging off `root_id` at any depth to
    /// `Completed`. Fired by the roster diff when a backgrounded root
    /// departs: its children have no terminal event of their own and
    /// would otherwise stay open until the next turn boundary. The root
    /// itself is untouched - its own `task_updated` lands a frame after
    /// the drain. Returns the touched (message, block) slots for
    /// render-cache sync.
    pub(crate) fn settle_children_of(&mut self, root_id: &str) -> Vec<(usize, usize)> {
        let roots: HashSet<&str> = std::iter::once(root_id).collect();
        let doomed: HashSet<String> = self
            .tool_call_scopes
            .iter()
            .filter(|(id, scope)| {
                matches!(scope, ToolCallScope::SubagentChild { .. })
                    && self.resolves_to_live_root(id, &roots)
            })
            .map(|(id, _)| id.clone())
            .collect();
        let mut settled: Vec<(usize, usize)> = Vec::new();
        for (msg_idx, msg) in self.messages.iter_mut().enumerate() {
            for (block_idx, block) in msg.blocks.iter_mut().enumerate() {
                let crate::app::MessageBlock::ToolCall(tc) = block else { continue };
                let tc = tc.as_mut();
                if doomed.contains(tc.id.as_str())
                    && matches!(
                        tc.status,
                        model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                    )
                {
                    tc.status = model::ToolCallStatus::Completed;
                    tc.mark_tool_call_layout_dirty();
                    settled.push((msg_idx, block_idx));
                }
            }
        }
        settled
    }
}

/// Whether a session's Projects-pane row shows the activity spinner: an
/// in-progress turn (`Running` / `Spawning`), or an otherwise-Idle session
/// with a live backgrounded task. Attention / AuthRequired / Failed keep
/// their own glyph, so the promotion is over the Idle bullet only. Shared
/// by the row glyph (`glyph_for_lifecycle`) and the frame-tick gate
/// (`App::shows_activity`) so the two never disagree about what
/// animates.
pub fn session_shows_spinner(lifecycle: SessionLifecycleState, has_background_work: bool) -> bool {
    matches!(lifecycle, SessionLifecycleState::Running | SessionLifecycleState::Spawning)
        || (matches!(lifecycle, SessionLifecycleState::Idle) && has_background_work)
}

/// What the optimistic `/mode` apply changed, snapshotted so a CLI
/// refusal (`SessionUpdate::SetModeFailed`) can restore it.
#[derive(Debug, Clone)]
pub struct ModeRollback {
    pub mode_state: Option<ModeState>,
    pub turn_mode: Option<forge_workspace::PermissionMode>,
    pub supported_mode_ids: Vec<forge_workspace::PermissionMode>,
}

/// What the optimistic `/model` apply changed, snapshotted so a CLI
/// refusal (`SessionUpdate::SetModelFailed`) can restore it.
#[derive(Debug, Clone)]
pub struct ModelRollback {
    pub current_model: Option<model::CurrentModel>,
    pub requested_model_id: Option<String>,
}

impl UiSession {
    /// Restore the snapshot parked by the optimistic `/mode` apply.
    /// Returns false when no snapshot is parked.
    pub fn rollback_pending_mode(&mut self) -> bool {
        let Some(snapshot) = self.pending_mode_rollback.take() else { return false };
        self.mode = snapshot.mode_state;
        self.turn_state.mode = snapshot.turn_mode;
        self.turn_state.supported_mode_ids = snapshot.supported_mode_ids;
        true
    }

    /// Restore the snapshot parked by the optimistic `/model` apply.
    /// Returns false when no snapshot is parked.
    pub fn rollback_pending_model(&mut self) -> bool {
        let Some(snapshot) = self.pending_model_rollback.take() else { return false };
        self.current_model = snapshot.current_model;
        self.turn_state.requested_model_id = snapshot.requested_model_id;
        true
    }

    /// Clear the session-identity mirror set a hard teardown applies -
    /// the one place to add a mirror field, so a new field cannot be
    /// missed at one of the hand-synced teardown sites. The list in
    /// `App::clear_session_runtime_identity` mirrors this one field
    /// for field; add to both or neither.
    pub fn clear_runtime_identity(&mut self) {
        self.key = None;
        self.session_id = None;
        self.account_info = None;
        self.current_model = None;
        self.observed_assistant_model = None;
        self.mode = None;
        self.runtime_session_state = None;
        self.observed_permission_mode = None;
        self.observed_effort = None;
        self.pending_mode_rollback = None;
        self.pending_model_rollback = None;
        self.session_usage = crate::app::state::SessionUsageState::default();
        self.cancelled_turn_pending_hint = false;
        self.pending_cancel = false;
        self.last_rate_limit_update = None;
        // The workspace no longer holds these for the identity being
        // torn down; mirrors must not either.
        self.dictate_overrides = forge_workspace::DictateOverrides::default();
        self.dictate_device_pin = None;
        self.mcp = McpState::default();
    }
}

impl Default for UiSession {
    fn default() -> Self {
        // Hand-rolled because `next_paste_session_id` seeds to 1;
        // every other field takes its type default, so a new field
        // lands here without further thought.
        Self {
            key: Option::default(),
            dictate_overrides: forge_workspace::DictateOverrides::default(),
            dictate_device_pin: None,
            backgrounded_roots: HashSet::new(),
            session_id: Option::default(),
            lifecycle_state: SessionLifecycleState::default(),
            cwd_raw: String::default(),
            project: Option::default(),
            session_scope_epoch: u64::default(),
            turn_state: SessionTurnState::default(),
            account_info: Option::default(),
            active_account_display_name: Option::default(),
            runtime_session_state: Option::default(),
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
            subagent_attribution: HashMap::default(),
            current_model: Option::default(),
            available_models: Vec::default(),
            available_commands: Vec::default(),
            available_agents: Vec::default(),
            mode: Option::default(),
            pending_mode_rollback: Option::default(),
            pending_model_rollback: Option::default(),
            observed_permission_mode: Option::default(),
            observed_effort: Option::default(),
            observed_assistant_model: Option::default(),
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
            next_paste_session_id: 1,
            pending_images: Vec::default(),
            mention: Option::default(),
            slash: Option::default(),
            subagent: Option::default(),
            todos: Vec::default(),
            latest_thinking_tokens: None,
            last_stop_hook_summary: None,
            stop_hook_summary_expanded: std::collections::HashMap::default(),
            live_turn: LiveTurn::default(),
            queued_turn_sends: 0,
            queued_turn_awaiting_start: false,
            queued_turn_force_settle_at: None,
            queued_turn_force_settled: false,
            prev_duration_api_ms: None,
            monitors: Vec::default(),
            background_tasks: Vec::default(),
            session_task_tool_use_ids: std::collections::HashMap::default(),
            schedules: Vec::default(),
            workflows: Vec::default(),
            group_collapse_levels: std::collections::HashMap::default(),
            messaging_group_collapse_levels: std::collections::HashMap::default(),
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
            input_draft_snapshot: Option::default(),
            failed_turn: Option::default(),
            review_replies_waiting: Option::default(),
            review_waiting_settled: false,
            review_waiting_in_flight: std::sync::Arc::default(),
            review_waiting_failed_reads: 0,
            last_api_retry: Option::default(),
            auto_continue_due_at: Option::default(),
            auto_continue_attempts: 0,
            dictate: Option::default(),
            dictate_border: Option::default(),
            dictate_notice: Option::default(),
            dictate_notice_version: u64::default(),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::UiSession;
    use crate::app::App;

    /// `clear_runtime_identity` is the mirror-clear choke point for a
    /// hard teardown; the observed-assistant-model mirror must leave
    /// with the rest of the identity set.
    #[test]
    fn clear_runtime_identity_clears_observed_assistant_model() {
        let mut session = UiSession {
            observed_assistant_model: Some("claude-observed".to_owned()),
            ..UiSession::default()
        };

        session.clear_runtime_identity();

        assert!(session.observed_assistant_model.is_none());
    }

    /// The dictate mirrors ride the same teardown: a hard clear must
    /// not leave a pin the workspace no longer holds.
    #[test]
    fn clear_runtime_identity_clears_the_dictate_mirrors() {
        let mut session = UiSession {
            dictate_overrides: forge_workspace::DictateOverrides {
                styling: Some(forge_workspace::Styling::Formal),
                ..Default::default()
            },
            dictate_device_pin: Some(forge_workspace::DictateDeviceChoice::System),
            ..UiSession::default()
        };

        session.clear_runtime_identity();

        assert_eq!(session.dictate_overrides, forge_workspace::DictateOverrides::default());
        assert_eq!(session.dictate_device_pin, None);
    }

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

    /// The `background_tasks` registry lists only currently-live
    /// backgrounded tasks (the CLI replaces it wholesale on each
    /// `background_tasks_changed`), so a non-empty registry is the
    /// "session is doing background work" signal the Projects pane reads.
    #[test]
    fn has_live_background_work_tracks_registry_emptiness() {
        use crate::app::state::types::BackgroundTask;

        let mut session = super::UiSession::new(forge_workspace::SessionKey::from_session_id("bg"));
        assert!(!session.has_live_background_work(), "empty registry is not live work");

        session.background_tasks.push(BackgroundTask {
            task_id: "t1".to_owned(),
            task_type: "local_bash".to_owned(),
            description: "cargo build".to_owned(),
        });
        assert!(session.has_live_background_work(), "a live backgrounded task is live work");
    }

    /// The backgrounded-alive set resolves every task kind (bash, agent,
    /// workflow) through the session map, and the registry gates it: a
    /// roster row with no map entry is unresolvable, and a map entry with
    /// no roster row is already drained - both excluded.
    #[test]
    fn backgrounded_alive_tool_use_ids_resolves_all_task_types() {
        use crate::app::state::types::BackgroundTask;

        let mut session = super::UiSession::new(forge_workspace::SessionKey::from_session_id("bg"));
        for (task_id, task_type) in
            [("task-bash", "local_bash"), ("task-agent", "agent"), ("task-wf", "local_workflow")]
        {
            session.background_tasks.push(BackgroundTask {
                task_id: task_id.to_owned(),
                task_type: task_type.to_owned(),
                description: String::new(),
            });
            session.session_task_tool_use_ids.insert(task_id.to_owned(), format!("tu-{task_id}"));
        }
        // Roster row with no session-map entry: excluded (unresolvable).
        session.background_tasks.push(BackgroundTask {
            task_id: "task-unmapped".to_owned(),
            task_type: "local_bash".to_owned(),
            description: String::new(),
        });
        // Session-map entry with no roster row: excluded (already drained).
        session.session_task_tool_use_ids.insert("task-stale".to_owned(), "tu-stale".to_owned());

        let mut got: Vec<&str> = session.backgrounded_alive_tool_use_ids().into_iter().collect();
        got.sort_unstable();
        assert_eq!(
            got,
            vec!["tu-task-agent", "tu-task-bash", "tu-task-wf"],
            "every mapped roster task resolves regardless of kind; unmapped roster rows \
             and stale map entries are excluded",
        );
    }
}
