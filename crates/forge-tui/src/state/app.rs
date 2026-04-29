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
use crate::state::focus::FocusManager;
use crate::state::git_context::GitContextState;
use crate::state::input::{InputSnapshot, InputState};
use crate::state::messages::{ChatMessage, NoticeDedupKey};
use crate::state::model;
use crate::state::types::{
    AppStatus, CancelOrigin, HelpView, HistoryRetentionPolicy, HistoryRetentionStats, McpState,
    ModeState, PasteSessionState, RecentSessionInfo, RenderCacheBudget, ScrollbarDragState,
    SelectionState, SessionPickerState, SessionUsageState, TodoItem, ToolCallScope,
};
use crate::state::viewport::ChatViewport;

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

    pub should_quit: bool,
    /// Optional fatal error that should be surfaced at CLI boundary.
    pub exit_error: Option<anyhow::Error>,

    // ---- help overlay ----
    pub help_view: HelpView,
    pub help_open: bool,
    pub help_dialog: dialog::DialogState,
    pub help_visible_count: usize,

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
    pub history_retention: HistoryRetentionPolicy,
    pub history_retention_stats: HistoryRetentionStats,
    pub fps_ema: Option<f32>,
    pub last_frame_at: Option<Instant>,
    pub last_chat_render_trace_state: Option<ChatRenderTraceState>,
    pub(crate) last_active_turn_height_state: Option<(usize, bool, bool)>,
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
}

impl Default for App {
    fn default() -> Self {
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

            should_quit: false,
            exit_error: None,

            help_view: HelpView::default(),
            help_open: false,
            help_dialog: dialog::DialogState::default(),
            help_visible_count: 0,

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
            history_retention: HistoryRetentionPolicy::default(),
            history_retention_stats: HistoryRetentionStats::default(),
            fps_ema: None,
            last_frame_at: None,
            last_chat_render_trace_state: None,
            last_active_turn_height_state: None,
        }
    }
}
