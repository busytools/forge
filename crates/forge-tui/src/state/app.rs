#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    clippy::disallowed_methods,
    clippy::while_let_loop,
    clippy::collapsible_if,
    reason = "lifted upstream from claude-code-rust"
)]

use crate::state::dialog;
use crate::state::focus::{FocusContext, FocusManager, FocusOwner, FocusTarget};
use crate::state::git_context::GitContextState;
use crate::state::input::{InputSnapshot, InputState};
use crate::state::messages::{ChatMessage, NoticeDedupKey};
use crate::state::model;
use crate::state::types::{
    AppStatus, CancelOrigin, HelpView, HistoryRetentionPolicy, HistoryRetentionStats, LoginHint,
    McpState, ModeState, PasteSessionState, RecentSessionInfo, RenderCacheBudget,
    ScrollbarDragState, SelectionState, SessionPickerState, SessionUsageState, TodoItem,
    ToolCallScope,
};
use crate::state::viewport::{ChatViewport, LayoutInvalidation, LayoutRemeasureReason};

use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Active view (Chat / SessionPicker). Upstream had Config / Trusted
/// variants too; both stay out of the TUI's view enum until the
/// matching upstream modules lift (see project_forge_tui_cuts_to_revisit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActiveView {
    #[default]
    Chat,
    SessionPicker,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalToolCallRef {
    pub terminal_id: String,
    pub msg_idx: usize,
    pub block_idx: usize,
}

impl TerminalToolCallRef {
    #[must_use]
    pub fn new(terminal_id: String, msg_idx: usize, block_idx: usize) -> Self {
        Self { terminal_id, msg_idx, block_idx }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutocompleteKind {
    Mention,
    Slash,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NoticeStage {
    Warning,
    Rejected,
    PlanLimitTurnError,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnNoticeLocation {
    Inline { msg_idx: usize, block_idx: usize },
    Standalone { msg_idx: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnNoticeRef {
    pub dedup_key: NoticeDedupKey,
    pub stage: NoticeStage,
    pub location: TurnNoticeLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatRenderTraceState {
    pub width: u16,
    pub content_height: usize,
    pub viewport_height: usize,
    pub auto_scroll: bool,
    pub pinned_to_bottom: bool,
    pub scroll_target: usize,
    pub scroll_offset: usize,
    pub max_scroll: usize,
    pub first_visible: usize,
    pub render_start: usize,
    pub local_scroll: usize,
    pub rendered_msgs: usize,
    pub last_rendered_idx: Option<usize>,
    pub rendered_line_count: usize,
    pub last_message_idx: Option<usize>,
    pub last_message_height: Option<usize>,
    pub selection_snapshot_active: bool,
}

/// forge-tui App state.
///
/// Trimmed from upstream `claude-code-rust`'s `app/state/mod.rs`. See
/// `project_forge_tui_cuts_to_revisit` in auto-memory for the full
/// list of cuts; the dropped/deferred fields land progressively as
/// Tier B / Tier C UI surfaces them.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    pub active_view: ActiveView,

    // ---- chat state ----
    pub messages: Vec<ChatMessage>,
    /// Cached approximate retained bytes for each message, parallel to `messages`.
    pub message_retained_bytes: Vec<usize>,
    /// Rolling total of `message_retained_bytes`.
    pub retained_history_bytes: usize,
    pub viewport: ChatViewport,
    pub input: InputState,
    pub status: AppStatus,
    pub session_id: Option<model::SessionId>,
    /// Session id currently being resumed via `/resume`.
    pub resuming_session_id: Option<String>,
    /// Daemon WS connection. `None` until subscribed.
    pub conn: Option<crate::client::Client>,
    /// Monotonic session authority epoch used to ignore stale async view data.
    pub session_scope_epoch: u64,

    pub current_model: Option<model::CurrentModel>,
    pub cwd: String,
    pub cwd_raw: String,
    pub mode: Option<ModeState>,
    /// Latest config options observed from `config_option_update` events.
    /// Populated by daemon snapshots; the upstream `config: ConfigState`
    /// editor UI has not been lifted yet.
    pub config_options: std::collections::BTreeMap<String, serde_json::Value>,
    /// Login hint shown above the input field when authentication is
    /// required. Daemon pushes this when claude reports `auth_required`.
    pub login_hint: Option<LoginHint>,

    pub should_quit: bool,
    /// Optional fatal error that should be surfaced at CLI boundary.
    pub exit_error: Option<anyhow::Error>,

    // ---- help overlay ----
    pub help_view: HelpView,
    pub help_open: bool,
    pub help_dialog: dialog::DialogState,
    pub help_visible_count: usize,

    /// Spinner label shown while a slash command is in flight. Stays
    /// `None` in forge until slash command machinery lifts; help.rs
    /// reads it for the "Processing command…" overlay.
    pub pending_command_label: Option<String>,

    // ---- pending interactions / cancel ----
    pub pending_interaction_ids: Vec<String>,
    pub cancelled_turn_pending_hint: bool,
    pub pending_cancel_origin: Option<CancelOrigin>,
    pub pending_auto_submit_after_cancel: bool,

    // ---- spinner / turn UI ----
    pub spinner_frame: usize,
    pub spinner_last_advance_at: Option<Instant>,
    pub active_turn_assistant_message_idx: Option<usize>,
    pub tools_collapsed: bool,
    pub active_task_ids: HashSet<String>,
    pub tool_call_scopes: HashMap<String, ToolCallScope>,
    pub force_redraw: bool,
    pub tool_call_index: HashMap<String, (usize, usize)>,

    // ---- todos panel ----
    pub todos: Vec<TodoItem>,
    pub show_todo_panel: bool,
    pub todo_scroll: usize,
    pub todo_selected: usize,
    pub cached_todo_compact: Option<ratatui::text::Line<'static>>,

    // ---- focus ----
    pub focus: FocusManager,

    // ---- catalog (daemon-pushed) ----
    pub available_commands: Vec<model::AvailableCommand>,
    pub available_agents: Vec<model::AvailableAgent>,
    pub available_models: Vec<model::AvailableModel>,
    pub recent_sessions: Vec<RecentSessionInfo>,
    pub session_picker: SessionPickerState,

    // ---- selection / scrollbar / rendered cache ----
    pub cached_frame_area: ratatui::layout::Rect,
    pub selection: Option<SelectionState>,
    pub scrollbar_drag: Option<ScrollbarDragState>,
    pub rendered_chat_lines: Vec<String>,
    pub rendered_chat_area: ratatui::layout::Rect,
    pub rendered_input_lines: Vec<String>,
    pub rendered_input_area: ratatui::layout::Rect,

    // ---- input / paste ----
    pub pending_submit: Option<InputSnapshot>,
    pub paste_burst: crate::state::paste_burst::PasteBurstDetector,
    pub pending_paste_text: String,
    /// File-index scanner state for `@` mention autocomplete.
    pub file_index: crate::state::file_index::FileIndexState,
    /// `@` mention autocomplete dropdown state.
    pub mention: Option<crate::state::mention::MentionState>,
    /// `&name` subagent autocomplete dropdown state.
    pub subagent: Option<crate::state::subagent::SubagentState>,
    /// Slash-command autocomplete dropdown state.
    pub slash: Option<crate::state::slash::SlashState>,
    /// Channel: file_index scanner -> drain_events on each frame.
    pub file_index_event_tx: std::sync::mpsc::Sender<crate::state::file_index::FileIndexEvent>,
    /// Channel: drain_events consumes scanner events.
    pub file_index_event_rx: std::sync::mpsc::Receiver<crate::state::file_index::FileIndexEvent>,
    pub pending_paste_session: Option<PasteSessionState>,
    pub active_paste_session: Option<PasteSessionState>,
    pub next_paste_session_id: u64,
    pub pending_images: Vec<crate::state::clipboard_image::ImageAttachment>,

    // ---- git ----
    pub(crate) git_context: GitContextState,

    // ---- daemon-pushed snapshots ----
    pub session_usage: SessionUsageState,
    pub mcp: McpState,
    pub fast_mode_state: model::FastModeState,
    pub runtime_session_state: Option<model::RuntimeSessionState>,
    pub prompt_suggestion: Option<String>,
    pub last_rate_limit_update: Option<model::RateLimitUpdate>,
    pub turn_notice_refs: Vec<TurnNoticeRef>,
    pub is_compacting: bool,

    // ---- terminal tool call indexing ----
    pub terminal_tool_calls: Vec<TerminalToolCallRef>,
    pub terminal_tool_call_membership: HashSet<TerminalToolCallRef>,

    // ---- render cadence ----
    pub needs_redraw: bool,
    pub perf: Option<crate::perf::Profiler>,
    pub render_cache_budget: RenderCacheBudget,
    pub(crate) render_cache_slots:
        Vec<Vec<crate::state::render_budget::RenderCacheSlotState>>,
    pub(crate) render_cache_total_bytes: usize,
    pub(crate) render_cache_protected_bytes: usize,
    pub(crate) render_cache_evictable:
        std::collections::BTreeSet<crate::state::render_budget::RenderCacheEvictionKey>,
    pub(crate) render_cache_tail_msg_idx: Option<usize>,
    pub history_retention: HistoryRetentionPolicy,
    pub history_retention_stats: HistoryRetentionStats,
    pub fps_ema: Option<f32>,
    pub last_frame_at: Option<Instant>,
    pub last_chat_render_trace_state: Option<ChatRenderTraceState>,
    pub(crate) last_active_turn_height_state: Option<(usize, bool, bool)>,

    // ---- forge-side fields (not in upstream) ----
    /// WS daemon URL for footer display + reconnect.
    pub daemon_url: String,
    /// Connection lifecycle for the footer connection glyph.
    pub connection: ConnectionState,
    /// Local primary/viewer role for `current_session`.
    pub role: Role,
    /// One-line status/toast message rendered above the input.
    pub status_msg: String,
    /// Active permission modal awaiting user input.
    pub pending_permission: Option<PendingPermission>,
}

/// Connection state — drives the footer connection glyph. Forge-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ConnectionState {
    /// Initial handshake.
    #[default]
    Connecting,
    /// Live link to the daemon.
    Connected,
    /// Backoff retry pending.
    Reconnecting {
        /// Seconds until the next retry attempt.
        next_retry_secs: u32,
    },
    /// Gave up retrying or the user dismissed.
    Disconnected,
}

/// Local primary/viewer role for the currently subscribed session.
/// Forge-specific (mirrors daemon's role assignment).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Role {
    /// No session subscribed.
    #[default]
    Vacant,
    /// We hold primary; permission/hook requests come to us.
    Primary,
    /// Someone else holds primary; we read but don't answer.
    Viewer,
}

/// Snapshot of an outstanding permission request awaiting user input.
/// Forge-specific (daemon's reverse-RPC permission modal).
#[derive(Debug)]
#[non_exhaustive]
pub struct PendingPermission {
    /// JSON-RPC id of the originating reverse-RPC.
    pub rev_id: serde_json::Value,
    /// Original params from the request.
    pub params: serde_json::Value,
    /// Set when the prompt came in via the daemon's queue (after
    /// reconnect). Queued prompts answer via `prompts.respond` rather
    /// than a synchronous reverse-RPC response.
    pub prompt_id: Option<String>,
}

impl PendingPermission {
    /// Construct a `PendingPermission`.
    #[must_use]
    pub fn new(
        rev_id: serde_json::Value,
        params: serde_json::Value,
        prompt_id: Option<String>,
    ) -> Self {
        Self {
            rev_id,
            params,
            prompt_id,
        }
    }
}

impl App {
    /// Default-construct App with empty chat state and no session.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// O(1) lookup: tool_call_id → (msg_idx, block_idx). Used by
    /// permission/question dispatch and footer interaction tracking.
    #[must_use]
    pub fn lookup_tool_call(&self, id: &str) -> Option<(usize, usize)> {
        self.tool_call_index.get(id).copied()
    }

    /// Register a tool call's location in `tool_call_index`. Called
    /// when a `tool_use` block is appended to a message.
    pub fn index_tool_call(&mut self, id: String, msg_idx: usize, block_idx: usize) {
        self.tool_call_index.insert(id, (msg_idx, block_idx));
    }

    /// Mark this tool call as "active" (root-level Task/Agent in
    /// progress). Display indicator in footer until removed.
    pub fn insert_active_task(&mut self, id: String) {
        self.active_task_ids.insert(id);
    }

    /// Drop active-task tracking for this id.
    pub fn remove_active_task(&mut self, id: &str) {
        self.active_task_ids.remove(id);
    }

    /// Record a tool call's scope (main agent / subagent root /
    /// subagent child). Permission UI honours the distinction.
    pub fn register_tool_call_scope(&mut self, id: String, scope: ToolCallScope) {
        self.tool_call_scopes.insert(id, scope);
    }

    /// Lookup tool-call scope by id. None if not registered.
    #[must_use]
    pub fn tool_call_scope(&self, id: &str) -> Option<ToolCallScope> {
        self.tool_call_scopes.get(id).cloned()
    }

    /// Wipe transient tool-scope state at session boundaries.
    pub fn clear_tool_scope_tracking(&mut self) {
        self.tool_call_scopes.clear();
        self.active_task_ids.clear();
        self.tool_call_index.clear();
        self.terminal_tool_calls.clear();
        self.terminal_tool_call_membership.clear();
    }

    /// Branch name for footer rendering. Pulls from the live
    /// git_context watcher; returns None when cwd is outside a repo
    /// or branch hasn't resolved yet.
    #[must_use]
    pub fn git_branch(&self) -> Option<&str> {
        self.git_context.branch_name()
    }

    /// Whether the help overlay is open. Trivial accessor for symmetry
    /// with upstream's API surface.
    #[must_use]
    pub fn is_help_active(&self) -> bool {
        self.help_open
    }

    /// Whether any autocomplete (mention / slash / subagent) is open.
    /// Always false in the trimmed App until those modules lift; help
    /// + footer focus routing keys off this.
    #[must_use]
    pub fn autocomplete_focus_available(&self) -> bool {
        false
    }

    /// Build the focus context that drives `FocusManager::owner` /
    /// `claim` lookups. Tracks which focus targets are currently
    /// available (todo panel visible, autocomplete open, pending
    /// permission, help overlay).
    #[must_use]
    fn focus_context(&self) -> FocusContext {
        FocusContext::new(
            self.show_todo_panel && !self.todos.is_empty(),
            self.autocomplete_focus_available(),
            !self.pending_interaction_ids.is_empty(),
        )
        .with_help(self.is_help_active())
    }

    /// Current key-routing owner. Layered on top of focus_context to
    /// resolve which UI element should receive directional keys.
    #[must_use]
    pub fn focus_owner(&self) -> FocusOwner {
        self.focus.owner(self.focus_context())
    }

    /// Claim key routing for a navigation target (todo, mention, help
    /// overlay, etc.). The latest claimant wins.
    pub fn claim_focus_target(&mut self, target: FocusTarget) {
        let context = self.focus_context();
        self.focus.claim(target, context);
    }

    /// Release a key-routing claim previously made via
    /// `claim_focus_target`.
    pub fn release_focus_target(&mut self, target: FocusTarget) {
        let context = self.focus_context();
        self.focus.release(target, context);
    }

    /// Whether the file-index scanner should respect `.gitignore`.
    /// Reads from `config_options` snapshot; defaults to true (matches
    /// upstream's setting default).
    #[must_use]
    pub fn respect_gitignore_effective(&self) -> bool {
        self.config_options
            .get("respect_gitignore")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    }

    /// Resolved thinking effort. Reads from `config_options` snapshot
    /// pushed by the daemon; falls back to `Medium` until the daemon
    /// wire surface ships the value (cuts list: ConfigState).
    #[must_use]
    pub fn thinking_effort_effective(&self) -> model::EffortLevel {
        self.config_options
            .get("thinking_effort")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| match s {
                "low" => Some(model::EffortLevel::Low),
                "medium" => Some(model::EffortLevel::Medium),
                "high" => Some(model::EffortLevel::High),
                _ => None,
            })
            .unwrap_or(model::EffortLevel::Medium)
    }

    /// Mark the next frame as forced-redraw (e.g. after viewport
    /// resize).
    pub fn force_full_redraw(&mut self) {
        self.force_redraw = true;
        self.needs_redraw = true;
    }

    /// Set todos list and invalidate the cached compact line.
    pub fn set_todos(&mut self, todos: Vec<TodoItem>) {
        self.todos = todos;
        self.cached_todo_compact = None;
    }

    /// Apply a layout invalidation to the viewport. Lifted verbatim
    /// from upstream's `App::invalidate_layout`.
    pub fn invalidate_layout(&mut self, level: LayoutInvalidation) {
        match level {
            LayoutInvalidation::MessageChanged(idx) => {
                self.viewport.invalidate_message(idx);
            }
            LayoutInvalidation::MessagesFrom(idx) => {
                self.viewport.invalidate_messages_from(idx);
            }
            LayoutInvalidation::Global => {
                if self.messages.is_empty() {
                    return;
                }
                self.viewport.invalidate_all_messages(LayoutRemeasureReason::Global);
                self.viewport.bump_layout_generation();
            }
            LayoutInvalidation::Resize => {
                debug_assert!(
                    false,
                    "Resize should not be dispatched through invalidate_layout"
                );
            }
        }
    }

    /// Drop focus claims that are no longer valid. Lifted verbatim
    /// from upstream.
    pub fn normalize_focus_stack(&mut self) {
        let context = self.focus_context();
        self.focus.normalize(context);
    }

    // sync_render_cache_slot lifted via state/render_budget.rs impl App.
    // recompute_message_retained_bytes lifted via state/history_retention.rs impl App.

    /// Whether the recorded active-turn-assistant index still points at
    /// a real Assistant message. Lifted from upstream.
    #[must_use]
    pub fn active_turn_assistant_idx(&self) -> Option<usize> {
        self.active_turn_assistant_message_idx.filter(|&idx| {
            self.messages.get(idx).is_some_and(|msg| {
                matches!(msg.role, crate::state::messages::MessageRole::Assistant)
            })
        })
    }

    /// Bind the active-turn-assistant message id, if it points at an
    /// Assistant role message.
    pub fn bind_active_turn_assistant(&mut self, idx: usize) {
        self.active_turn_assistant_message_idx = self
            .messages
            .get(idx)
            .is_some_and(|msg| {
                matches!(msg.role, crate::state::messages::MessageRole::Assistant)
            })
            .then_some(idx);
    }

    /// Drop the active-turn-assistant binding.
    pub fn clear_active_turn_assistant(&mut self) {
        self.active_turn_assistant_message_idx = None;
    }

    /// Drop all turn-local notice refs.
    pub(crate) fn clear_turn_notice_refs(&mut self) {
        self.turn_notice_refs.clear();
    }

    /// Wipe terminal-tool-call indexing.
    pub(crate) fn clear_terminal_tool_call_tracking(&mut self) {
        self.terminal_tool_calls.clear();
        self.terminal_tool_call_membership.clear();
    }

    /// Terminal id associated with a Pending/InProgress execute tool call,
    /// if any.
    #[must_use]
    pub(crate) fn tracked_terminal_id_for_tool(
        tc: &crate::state::tool_call_info::ToolCallInfo,
    ) -> Option<String> {
        (tc.is_execute_tool()
            && matches!(
                tc.status,
                model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress
            ))
        .then(|| tc.terminal_id.clone())
        .flatten()
    }

    /// Shift active-turn-assistant index after a message insertion at `idx`.
    pub(crate) fn shift_active_turn_assistant_for_insert(&mut self, idx: usize) {
        if let Some(owner_idx) = self.active_turn_assistant_message_idx
            && idx <= owner_idx
        {
            self.active_turn_assistant_message_idx = Some(owner_idx.saturating_add(1));
        }
    }

    /// Shift active-turn-assistant index after a message removal at `idx`.
    pub(crate) fn shift_active_turn_assistant_for_remove(&mut self, idx: usize) {
        let Some(owner_idx) = self.active_turn_assistant_message_idx else {
            return;
        };
        self.active_turn_assistant_message_idx = match idx.cmp(&owner_idx) {
            std::cmp::Ordering::Less => Some(owner_idx.saturating_sub(1)),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(owner_idx),
        };
    }

    /// Shift turn-notice ref msg indices after a message insertion at `idx`.
    pub(crate) fn shift_turn_notice_refs_for_insert(&mut self, idx: usize) {
        for notice_ref in &mut self.turn_notice_refs {
            match &mut notice_ref.location {
                TurnNoticeLocation::Inline { msg_idx, .. }
                | TurnNoticeLocation::Standalone { msg_idx }
                    if idx <= *msg_idx =>
                {
                    *msg_idx = msg_idx.saturating_add(1);
                }
                TurnNoticeLocation::Inline { .. } | TurnNoticeLocation::Standalone { .. } => {}
            }
        }
    }

    /// Shift / drop turn-notice ref msg indices after a message removal at `idx`.
    pub(crate) fn shift_turn_notice_refs_for_remove(&mut self, idx: usize) {
        self.turn_notice_refs.retain_mut(|notice_ref| match &mut notice_ref.location {
            TurnNoticeLocation::Inline { msg_idx, .. }
            | TurnNoticeLocation::Standalone { msg_idx } => match idx.cmp(msg_idx) {
                std::cmp::Ordering::Less => {
                    *msg_idx = msg_idx.saturating_sub(1);
                    true
                }
                std::cmp::Ordering::Equal => false,
                std::cmp::Ordering::Greater => true,
            },
        });
    }

    /// Remap turn-notice refs after a bulk message drop.
    pub(crate) fn remap_turn_notice_refs_after_message_drop(
        &mut self,
        old_to_new: &[Option<usize>],
    ) {
        self.turn_notice_refs.retain_mut(|notice_ref| match &mut notice_ref.location {
            TurnNoticeLocation::Inline { msg_idx, .. }
            | TurnNoticeLocation::Standalone { msg_idx } => {
                let Some(new_idx) = old_to_new.get(*msg_idx).copied().flatten() else {
                    return false;
                };
                *msg_idx = new_idx;
                true
            }
        });
    }

    /// Whether the input draft has any user text. Used by focus
    /// routing to decide whether Enter should submit or focus the
    /// pending permission queue.
    #[must_use]
    pub fn has_draft_input_for_focus(&self) -> bool {
        !self.input.is_empty()
    }

    /// Re-derive focus claims from current chat state. Lifted from
    /// upstream; called on view changes and on incoming
    /// permission/question events.
    pub fn rebuild_chat_focus_from_state(&mut self) {
        use crate::state::inline_interactions::{
            clear_inline_interaction_focus, focus_next_inline_interaction,
        };

        if self.active_view != ActiveView::Chat {
            return;
        }

        self.normalize_focus_stack();

        if self.pending_interaction_ids.is_empty() {
            clear_inline_interaction_focus(self);
        } else if self.focus_owner() == FocusOwner::Permission
            || !self.has_draft_input_for_focus()
        {
            focus_next_inline_interaction(self);
        } else {
            clear_inline_interaction_focus(self);
        }

        if self.autocomplete_focus_available() {
            self.claim_focus_target(FocusTarget::Mention);
        } else {
            self.release_focus_target(FocusTarget::Mention);
        }

        if self.is_help_active()
            && self.pending_interaction_ids.is_empty()
            && !self.autocomplete_focus_available()
        {
            self.claim_focus_target(FocusTarget::Help);
        } else {
            self.release_focus_target(FocusTarget::Help);
        }
    }
}

impl Default for App {
    fn default() -> Self {
        let (file_index_tx, file_index_rx) = std::sync::mpsc::channel();
        Self {
            active_view: ActiveView::default(),

            messages: Vec::new(),
            message_retained_bytes: Vec::new(),
            retained_history_bytes: 0,
            viewport: ChatViewport::default(),
            input: InputState::default(),
            status: AppStatus::Connecting,
            session_id: None,
            resuming_session_id: None,
            conn: None,
            session_scope_epoch: 0,

            current_model: None,
            cwd: String::new(),
            cwd_raw: String::new(),
            mode: None,
            config_options: std::collections::BTreeMap::new(),
            login_hint: None,

            should_quit: false,
            exit_error: None,

            help_view: HelpView::default(),
            help_open: false,
            help_dialog: dialog::DialogState::default(),
            help_visible_count: 0,
            pending_command_label: None,

            pending_interaction_ids: Vec::new(),
            cancelled_turn_pending_hint: false,
            pending_cancel_origin: None,
            pending_auto_submit_after_cancel: false,

            spinner_frame: 0,
            spinner_last_advance_at: None,
            active_turn_assistant_message_idx: None,
            tools_collapsed: false,
            active_task_ids: HashSet::new(),
            tool_call_scopes: HashMap::new(),
            force_redraw: false,
            tool_call_index: HashMap::new(),

            todos: Vec::new(),
            show_todo_panel: false,
            todo_scroll: 0,
            todo_selected: 0,
            cached_todo_compact: None,

            focus: FocusManager::default(),

            available_commands: Vec::new(),
            available_agents: Vec::new(),
            available_models: Vec::new(),
            recent_sessions: Vec::new(),
            session_picker: SessionPickerState::default(),

            cached_frame_area: ratatui::layout::Rect::default(),
            selection: None,
            scrollbar_drag: None,
            rendered_chat_lines: Vec::new(),
            rendered_chat_area: ratatui::layout::Rect::default(),
            rendered_input_lines: Vec::new(),
            rendered_input_area: ratatui::layout::Rect::default(),

            pending_submit: None,
            paste_burst: crate::state::paste_burst::PasteBurstDetector::default(),
            pending_paste_text: String::new(),
            file_index: crate::state::file_index::FileIndexState::default(),
            mention: None,
            subagent: None,
            slash: None,
            file_index_event_tx: file_index_tx,
            file_index_event_rx: file_index_rx,
            pending_paste_session: None,
            active_paste_session: None,
            next_paste_session_id: 0,
            pending_images: Vec::new(),

            git_context: GitContextState::default(),

            session_usage: SessionUsageState::default(),
            mcp: McpState::default(),
            fast_mode_state: model::FastModeState::Off,
            runtime_session_state: None,
            prompt_suggestion: None,
            last_rate_limit_update: None,
            turn_notice_refs: Vec::new(),
            is_compacting: false,

            terminal_tool_calls: Vec::new(),
            terminal_tool_call_membership: HashSet::new(),

            needs_redraw: true,
            perf: None,
            render_cache_budget: RenderCacheBudget::default(),
            render_cache_slots: Vec::new(),
            render_cache_total_bytes: 0,
            render_cache_protected_bytes: 0,
            render_cache_evictable: std::collections::BTreeSet::new(),
            render_cache_tail_msg_idx: None,
            history_retention: HistoryRetentionPolicy::default(),
            history_retention_stats: HistoryRetentionStats::default(),
            fps_ema: None,
            last_frame_at: None,
            last_chat_render_trace_state: None,
            last_active_turn_height_state: None,

            daemon_url: String::new(),
            connection: ConnectionState::default(),
            role: Role::default(),
            status_msg: String::new(),
            pending_permission: None,
        }
    }
}
