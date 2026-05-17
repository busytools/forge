pub mod block_cache;
pub mod cache_metrics;
mod history_retention;
pub mod messages;
pub(crate) mod render_budget;
pub mod tool_call_info;
pub mod types;
pub mod viewport;

// Re-export all public types so external `use crate::app::state::X` paths still work.
pub use block_cache::BlockCache;
pub use cache_metrics::CacheMetrics;
pub(crate) use messages::MarkdownRenderKey;
pub use messages::{
    CachedMessageSegment, ChatMessage, IncrementalMarkdown, MessageBlock, MessageRenderCache,
    MessageRenderCacheKey, MessageRenderSignature, MessageRole, NoticeBlock, NoticeDedupKey,
    RateLimitIncidentKey, SystemSeverity, TextBlock, TextBlockSpacing, WelcomeBlock,
    hash_text_block_content, hash_welcome_block_content,
};
pub use tool_call_info::{
    InlinePermission, InlineQuestion, TerminalSnapshotMode, ToolCallInfo, is_execute_tool_name,
};
pub use types::{
    AppStatus, ExtraUsage, HelpView, HistoryRetentionPolicy, HistoryRetentionStats, LoginHint,
    McpState, MessageUsage, ModeInfo, ModeState, PasteSessionState, PendingCommandAck,
    RecentSessionInfo, RenderCacheBudget, ScrollbarDragState, SelectionKind, SelectionPoint,
    SelectionState, SessionPickerState, SessionTurnState, SessionUsageState, TodoItem, TodoStatus,
    ToolCallScope, UsageSnapshot, UsageSourceKind, UsageSourceMode, UsageState, UsageWindow,
};
pub use viewport::{
    ChatViewport, LayoutInvalidation, LayoutInvalidation as InvalidationLevel,
    LayoutRemeasureReason, ScrollbarGeometry, compute_scrollbar_geometry,
};

use crate::agent::model;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Instant;
use tokio::sync::mpsc;

use super::config::ConfigState;
use super::dialog;
use super::file_index;
use super::focus::{FocusContext, FocusManager, FocusOwner, FocusTarget};
use super::inline_interactions::{clear_inline_interaction_focus, focus_next_inline_interaction};
use super::input::{InputSnapshot, parse_paste_placeholder_before_cursor};
use super::mention;
use super::plugins::PluginsState;
use super::slash;
use super::subagent;
use super::view::ActiveView;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalToolCallRef {
    pub terminal_id: String,
    pub msg_idx: usize,
    pub block_idx: usize,
}

impl TerminalToolCallRef {
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

/// A click-targetable row in the Projects pane, stamped by
/// [`crate::ui::projects_pane::render`] during paint and read by the
/// mouse handler on click. Same render-time-stamp pattern as the
/// per-tool-call expand/collapse.
///
/// `ProjectHeader` and `SessionRow` are y-only — they span the full
/// pane width, so an x-coord doesn't add information. `TopBarIcon`
/// and `OverlayClose` are x+y bounded — they target a specific glyph
/// position on a one-row band shared with other content.
#[derive(Debug, Clone)]
pub enum PaneHitTarget {
    /// Click on a project name row → switch active session to its
    /// lead.
    ProjectHeader { project_name: String, y: u16, height: u16 },
    /// Click on a session row in the active project's drilldown →
    /// switch active session to that specific session.
    SessionRow { session_key: forge_workspace::SessionKey, y: u16, height: u16 },
    /// Click on the `▤` icon in the Narrow-tier top bar → toggle
    /// the Projects overlay.
    TopBarIcon { y: u16, height: u16, x_start: u16, x_end: u16 },
    /// Click on the `▦` icon in the Narrow-tier top bar (right end)
    /// → toggle the Inspector overlay. Mirror of `TopBarIcon` for
    /// the right-side pane.
    InspectorTopBarIcon { y: u16, height: u16, x_start: u16, x_end: u16 },
    /// Click on the `✕` glyph in the overlay banner → close the
    /// overlay without switching sessions.
    OverlayClose { y: u16, height: u16, x_start: u16, x_end: u16 },
    /// Click on the `×` glyph at the right edge of an active project
    /// row → close that project's session (drop the bucket + tell
    /// the workspace to release its pool entry so the underlying
    /// `claude` subprocess can exit).
    CloseSession {
        session_key: forge_workspace::SessionKey,
        y: u16,
        height: u16,
        x_start: u16,
        x_end: u16,
    },
    /// Click on the `🦉` glyph at the right edge of the Inspector
    /// pane's `GIT` section header → open the full-screen Diff
    /// overlay with auto-detected target. Only stamped when the
    /// snapshot has a diff to show (Worktree / BranchVsDefault)
    /// AND the inspector scroll offset is 0 (otherwise the header
    /// is off-screen).
    InspectorGitOpenDiff { y: u16, height: u16, x_start: u16, x_end: u16 },
}

impl PaneHitTarget {
    /// Whether the target's row range covers `y` (inclusive of `y`,
    /// exclusive of `y + height`). For full-width row targets
    /// (`ProjectHeader`, `SessionRow`) this is the only check the
    /// hit-tester needs; for x+y-bounded targets (`TopBarIcon`,
    /// `OverlayClose`) call [`Self::contains`] instead so the column
    /// constraint also applies.
    pub fn contains_y(&self, y: u16) -> bool {
        let (start, height) = match self {
            Self::ProjectHeader { y, height, .. }
            | Self::SessionRow { y, height, .. }
            | Self::TopBarIcon { y, height, .. }
            | Self::InspectorTopBarIcon { y, height, .. }
            | Self::OverlayClose { y, height, .. }
            | Self::CloseSession { y, height, .. }
            | Self::InspectorGitOpenDiff { y, height, .. } => (*y, *height),
        };
        (start..start.saturating_add(height)).contains(&y)
    }

    /// Full hit-test (x + y). For full-width row targets the x
    /// component is unconstrained; for x+y-bounded targets (top-bar
    /// icon, overlay close, per-row close) the click must fall within
    /// the recorded `[x_start, x_end)` range.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        if !self.contains_y(y) {
            return false;
        }
        match self {
            Self::ProjectHeader { .. } | Self::SessionRow { .. } => true,
            Self::TopBarIcon { x_start, x_end, .. }
            | Self::InspectorTopBarIcon { x_start, x_end, .. }
            | Self::OverlayClose { x_start, x_end, .. }
            | Self::CloseSession { x_start, x_end, .. }
            | Self::InspectorGitOpenDiff { x_start, x_end, .. } => (*x_start..*x_end).contains(&x),
        }
    }
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

// `App` is the god struct — bools are independent UI flags (autoscroll, paste-detected, dirty-rerender). Bundling defeats clarity at call sites.
pub struct App {
    pub active_view: ActiveView,
    pub config: ConfigState,
    pub settings_home_override: Option<PathBuf>,
    pub status: AppStatus,
    pub should_quit: bool,
    /// Optional fatal app error that should be surfaced at CLI boundary.
    pub exit_error: Option<crate::error::AppError>,
    /// Multi-session orchestrator. Hands out `AgentHandle`s via
    /// `get_agent_handle(SessionTarget::Default, ...)` at startup.
    /// `None` only in test contexts (`App::test_default`); production
    /// startup always populates this before construction.
    pub workspace: Option<Arc<forge_workspace::Workspace>>,
    /// Test-only capture for dispatched permission/question outcomes
    /// while the App has no `workspace`. Lets the legacy permission
    /// / question unit + integration tests assert "the user-pick
    /// handler fired outcome X for tool_id Y" without spinning up a
    /// real workspace in every test. Populated when
    /// `dispatch_permission_outcome` or `dispatch_question_outcome`
    /// is called on an App whose `workspace` is `None`.
    ///
    /// Behind the `testing` Cargo feature so the production API
    /// surface never carries these fields. Cross-crate integration
    /// tests enable the feature via this crate's
    /// `[dev-dependencies]` self-ref in `Cargo.toml`.
    #[rustfmt::skip] #[cfg(feature = "testing")] pub test_dispatched_permission_outcomes: std::cell::RefCell<Vec<(String, forge_primitives::PermissionOutcome)>>,
    #[rustfmt::skip] #[cfg(feature = "testing")] pub test_dispatched_question_outcomes: std::cell::RefCell<Vec<(String, forge_primitives::QuestionOutcome)>>,
    /// Per-session state buckets, keyed by claude session UUID.
    /// [`super::session::UiSession`] value type one bucket at a time.
    pub sessions: std::collections::HashMap<forge_workspace::SessionKey, super::session::UiSession>,
    /// Which entry of [`Self::sessions`] the renderer reads from.
    /// `None` only in the brief pre-Connect window where no session
    /// has landed in the map yet.
    pub active_session_key: Option<forge_workspace::SessionKey>,
    /// Active help overlay view when `?` help is open.
    pub help_view: HelpView,
    /// Whether the help overlay is explicitly open.
    pub help_open: bool,
    /// Scroll/selection state for the Slash and Subagents help tabs.
    pub help_dialog: dialog::DialogState,
    /// Number of items that currently fit in the help viewport (updated each render).
    /// Used by key handlers for accurate scroll step size.
    pub help_visible_count: usize,
    /// Receiver for `SessionUpdate`s emitted by the workspace. The
    /// main event loop reads from here and dispatches via
    /// `events::apply_session_update`. Replaces the legacy
    /// `event_tx`/`event_rx` `ClientEvent` channel — user actions
    /// flow out via `workspace.dispatch(Command::...)`.
    pub update_rx: mpsc::UnboundedReceiver<forge_workspace::SessionUpdate>,
    /// Sender shared with TUI-internal async tasks (plugin inventory,
    /// usage refresh, slash command executors) that need to emit
    /// `SessionUpdate` envelopes back to the App event loop. Cloned
    /// from `workspace.update_sender()` at App construction; falls
    /// back to a no-op sender in test contexts.
    pub update_tx: mpsc::UnboundedSender<forge_workspace::SessionUpdate>,
    pub file_index_event_tx: std_mpsc::Sender<file_index::FileIndexEvent>,
    pub file_index_event_rx: std_mpsc::Receiver<file_index::FileIndexEvent>,
    /// Send / receive ends of the TUI-internal channel that the
    /// `crate::app::git_diff` background scanner tasks use to
    /// hand `GitDiffSnapshot` results back to the main loop.
    /// Mirrors the file_index channel pattern.
    pub git_diff_event_tx: std_mpsc::Sender<crate::app::git_diff::GitDiffEvent>,
    pub git_diff_event_rx: std_mpsc::Receiver<crate::app::git_diff::GitDiffEvent>,
    /// Send / receive ends of the channel for the
    /// `crate::app::process_scanner` OS-walk scanner. Same shape
    /// as `git_diff_event_*` but carries `ProcessScanEvent`.
    pub process_scan_event_tx: std_mpsc::Sender<crate::app::process_scanner::ProcessScanEvent>,
    pub process_scan_event_rx: std_mpsc::Receiver<crate::app::process_scanner::ProcessScanEvent>,
    /// Send / receive ends of the TUI-internal channel that the
    /// `crate::app::cli_version` startup fetch task uses to hand
    /// the merged `CliVersionInfo` snapshot back to the main loop.
    /// One-shot in practice (single fetch at startup); the channel
    /// stays open for the app's lifetime in case a follow-up
    /// re-fetch is added later.
    pub cli_version_event_tx: std_mpsc::Sender<crate::app::cli_version::CliVersionEvent>,
    pub cli_version_event_rx: std_mpsc::Receiver<crate::app::cli_version::CliVersionEvent>,
    pub diff_overlay_event_tx: std_mpsc::Sender<crate::app::diff_overlay::DiffOverlayEvent>,
    pub diff_overlay_event_rx: std_mpsc::Receiver<crate::app::diff_overlay::DiffOverlayEvent>,
    /// Monotonic counter bumped by every `/diff` invocation. Events
    /// arriving on `diff_overlay_event_rx` carry the seq they were
    /// spawned under; the drain pump only opens the overlay for
    /// the latest seq, so a rapid second `/diff` correctly
    /// supersedes the first instead of replaying the older result.
    pub diff_scan_seq: u64,
    /// Latest installed-vs-published claude CLI version snapshot.
    /// `None` until the startup fetch task lands. Rendered by the
    /// bottom-left account panel; missing values render as DIM `—`
    /// so the panel's row count stays constant.
    pub cli_version_info: Option<forge_workspace::env::cli_version::CliVersionInfo>,
    pub spinner_frame: usize,
    pub spinner_last_advance_at: Option<Instant>,
    /// Session-level preference for collapsing non-Execute tool call bodies.
    /// Toggled by Ctrl+X and applied at render/layout time.
    pub tools_collapsed: bool,
    /// Whether the Wide-tier Projects pane is currently visible.
    /// Toggled by Ctrl+B at Wide / Medium tiers. In-memory only —
    /// each launch starts visible. Has no effect at Narrow tier —
    /// that tier renders the top bar unconditionally and uses
    /// [`Self::projects_pane_overlay_open`] for the on-demand
    /// overlay.
    pub projects_pane_visible: bool,
    /// Whether the Narrow-tier Projects overlay is currently open.
    /// Transient — NOT persisted; each launch starts closed. Toggled
    /// by Ctrl+B at Narrow tier or by clicking the `▤` icon in the
    /// top bar; closed by clicking the overlay's `✕` glyph, by Esc,
    /// or by switching to a project / session row inside the overlay.
    pub projects_pane_overlay_open: bool,
    /// Whether the Wide/Medium-tier Inspector pane is currently
    /// visible (right side, mirror of [`Self::projects_pane_visible`]).
    /// Toggled by Ctrl+E. In-memory only — each launch starts visible.
    /// Has no effect at Narrow tier — that tier uses
    /// [`Self::inspector_pane_overlay_open`] for the on-demand
    /// overlay.
    pub inspector_pane_visible: bool,
    /// Whether the Narrow-tier Inspector overlay is currently open.
    /// Transient — NOT persisted; each launch starts closed. Toggled
    /// by Ctrl+E at Narrow tier or by clicking the `▦` icon in the
    /// top bar; closed by clicking the overlay's `✕` glyph or by
    /// Esc. Mutually exclusive with `projects_pane_overlay_open` —
    /// opening one closes the other.
    pub inspector_pane_overlay_open: bool,
    /// Click hit-targets stamped by
    /// [`crate::ui::projects_pane::render`]. Cleared on each render
    /// and refilled. The mouse handler iterates this to find what
    /// was clicked.
    pub pane_hit_targets: Vec<PaneHitTarget>,
    /// Last computed `AppLayout`, captured each frame so the mouse
    /// handler has rect coordinates available for click math. The
    /// Projects pane click path uses `layout.pane.contains(...)` to
    /// gate the pane-aware hit-test.
    pub layout: crate::ui::layout::AppLayout,
    /// Force a full terminal clear on next render frame.
    pub force_redraw: bool,
    /// Focus manager for directional/navigation key ownership.
    pub focus: FocusManager,
    /// Plugin inventory and UI state for the Config > Plugins view.
    pub plugins: PluginsState,
    // `recent_sessions: Vec<RecentSessionInfo>` moved to
    // `UiSession.recent_sessions` (per-session bucket). The session
    // list is per-project — switching active session via the
    // Projects pane naturally swaps the list along with the bucket.
    // See `App::recent_sessions` / `App::recent_sessions_mut`.
    /// Selection state for the startup session picker screen.
    pub session_picker: SessionPickerState,
    /// State for the launchpad view (project picker shown when forge
    /// is invoked without a project argv, or after `/launchpad`).
    /// Always present — reset whenever the active view transitions
    /// to [`ActiveView::Launchpad`] via the launchpad open helper.
    /// When the active view is anything else this is unused but
    /// kept allocated so transitions are cheap.
    pub launchpad: crate::app::LaunchpadState,
    /// Diff overlay state — `Some` while [`ActiveView::Diff`] is
    /// up, `None` otherwise. Dropped on overlay close so a stale
    /// snapshot can't leak into the next open.
    pub diff_overlay: Option<crate::app::DiffOverlayState>,
    /// Last known frame area (for mouse selection mapping).
    pub cached_frame_area: ratatui::layout::Rect,
    /// Active scrollbar drag state while left mouse button is held on the rail.
    pub scrollbar_drag: Option<ScrollbarDragState>,
    /// Cached rendered chat lines for selection/copy.
    pub rendered_chat_lines: Vec<String>,
    /// Area where chat content was rendered (for selection mapping).
    pub rendered_chat_area: ratatui::layout::Rect,
    /// Cached rendered input lines for selection/copy.
    pub rendered_input_lines: Vec<String>,
    /// Area where input content was rendered (for selection mapping).
    pub rendered_input_area: ratatui::layout::Rect,
    /// Area where the Inspector pane's **scrollable body** was last
    /// rendered (excluding the pinned banner + rule above it). Used
    /// by the mouse-wheel handler to detect "wheel scrolled while
    /// cursor is over the inspector pane" and adjust the active
    /// session's `inspector_scroll_offset`. `Rect::default()` until
    /// the first inspector render.
    pub rendered_inspector_body_area: ratatui::layout::Rect,
    // `file_index: FileIndexState` moved to `UiSession.file_index`
    // (per-session bucket). The scanner is project-scoped — switching
    // active session shows the new project's files. The channel
    // endpoints (`file_index_event_tx` / `_rx`) stay App-level since
    // the scanner thread is a single workspace-wide pump. See
    // `App::file_index` / `App::file_index_mut`.
    /// Timing-based paste burst detector. Detects rapid character streams
    /// (paste delivered as individual key events) and buffers them into a
    /// single paste payload. Fallback for terminals without bracketed paste.
    pub paste_burst: super::paste_burst::PasteBurstDetector,
    // `usage: UsageState` moved to `UiSession.usage` (per-session
    // bucket). Each forge session fetches Anthropic plan utilisation
    // independently; the Projects-pane account panel reads via the
    // active bucket. See `App::usage` / `App::usage_mut`.
    /// Dirty flag: skip `terminal.draw()` when nothing changed since last frame.
    pub needs_redraw: bool,
    /// Central notification manager (bell + desktop toast when unfocused).
    pub notifications: super::notify::NotificationManager,
    /// Performance logger. Present only when built with `--features perf`.
    /// Taken out (`Option::take`) during render, used, then put back to avoid
    /// borrow conflicts with `&mut App`.
    pub perf: Option<crate::perf::PerfLogger>,
    /// Global in-memory budget for rendered block and message caches.
    pub render_cache_budget: RenderCacheBudget,
    /// Smoothed frames-per-second (EMA of presented frame cadence).
    pub fps_ema: Option<f32>,
    /// Timestamp of the previous presented frame.
    pub last_frame_at: Option<Instant>,
    pub connection_started: bool,
    pub startup_resume_id: Option<String>,
    pub startup_resume_requested: bool,
    pub startup_session_picker_requested: bool,
    pub startup_recent_sessions_loaded: bool,
    pub startup_session_picker_resolved: bool,
    /// Project name from the CLI's positional `<PROJECT>` argument, if
    /// any. `None` means open the `default = true` project.
    /// Forwarded to [`forge_workspace::SessionTarget::Named`] when the
    /// connection task spins up.
    pub startup_project: Option<String>,
    /// True while `events::session_reset::load_resume_history` is
    /// walking on-disk history through the shared SDK-message
    /// dispatcher. Replay reuses the live walker so content blocks,
    /// tool_use, todos, and plans land in the bucket via the same code
    /// path — but the walker also has side effects that are wrong for
    /// replay (most notably the lifecycle `Running` write in
    /// `handle_assistant`, added so a mid-turn click flips the
    /// Projects-pane spinner on). Replay messages are historical, not
    /// live wire content, so the lifecycle write must be skipped while
    /// this flag is true. Cleared at end of replay so subsequent live
    /// messages on the same session behave normally.
    pub replay_in_progress: bool,
}

impl App {
    // ---- Multi-session accessors ----

    /// Synthetic session key used during the pre-Connect window
    /// (test contexts and the brief startup interval before the
    /// first `Connected` event lands). [`Self::set_session_id`]
    /// migrates the bucket onto the real session key when the
    /// claude-issued id arrives.
    pub(crate) const PRE_CONNECT_KEY: &'static str = "__conn_pending__";

    /// Returns a reference to the currently-active session bucket,
    /// or `None` in the brief pre-Connect window before any session
    /// has landed in [`Self::sessions`].
    pub fn active_session(&self) -> Option<&super::session::UiSession> {
        self.active_session_key.as_ref().and_then(|key| self.sessions.get(key))
    }

    /// Mutable accessor for the active session bucket.
    pub fn try_active_bucket_mut(&mut self) -> Option<&mut super::session::UiSession> {
        let key = self.active_session_key.clone()?;
        self.sessions.get_mut(&key)
    }

    /// Lookup a session by key (used by the event multiplexer to
    /// route background-session events to their bucket).
    pub fn session_mut(
        &mut self,
        key: &forge_workspace::SessionKey,
    ) -> Option<&mut super::session::UiSession> {
        self.sessions.get_mut(key)
    }

    /// Find a session bucket whose `cwd_raw` matches `path`. Used
    /// by the launchpad-click and projects-pane-click handlers to
    /// land the user on the resumed bucket for a project.
    pub fn find_running_bucket_for_path(&self, path: &str) -> Option<forge_workspace::SessionKey> {
        self.sessions.iter().find(|(_, s)| s.cwd_raw.as_str() == path).map(|(k, _)| k.clone())
    }

    /// Read access to the active session's input editor. Each session
    /// owns its own editor so switching the active session naturally
    /// swaps the visible input.
    pub fn input(&self) -> &super::input::InputState {
        // Fallback to a static default for the brief pre-Connect
        // window where no bucket has landed yet; in practice the
        // pre-Connect bucket is seeded at startup so this branch is
        // never hit in production.
        static EMPTY_INPUT: std::sync::OnceLock<super::input::InputState> =
            std::sync::OnceLock::new();
        self.active_session()
            .map_or_else(|| EMPTY_INPUT.get_or_init(super::input::InputState::new), |s| &s.input)
    }

    /// Mutable access to the active session's input editor. Companion
    /// to [`Self::input`].
    pub fn input_mut(&mut self) -> &mut super::input::InputState {
        &mut self.active_bucket_mut().input
    }

    /// Switch which session the renderer reads from. State on both
    /// sides is preserved (in-memory buckets in `sessions`); the
    /// next paint reflects the new active session. No-op if `key`
    /// is already active or unknown.
    pub fn switch_active_session(&mut self, key: forge_workspace::SessionKey) {
        // Local helper: map a session's lifecycle state to the
        // App-level status enum so a background turn that completed
        // while the user was away doesn't leave a stale `Thinking` /
        // `Running` status on switch-in.
        fn status_for_lifecycle(
            lifecycle: crate::app::session::SessionLifecycleState,
        ) -> AppStatus {
            use crate::app::session::SessionLifecycleState as L;
            match lifecycle {
                L::Spawning => AppStatus::Connecting,
                L::Running => AppStatus::Running,
                L::Sleeping
                | L::Idle
                | L::Attention
                | L::AuthRequired
                | L::Failed
                | L::LoggedOut => AppStatus::Ready,
            }
        }
        if self.active_session_key.as_ref() == Some(&key) {
            return;
        }
        if !self.sessions.contains_key(&key) {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "switch_active_session_unknown_key",
                key = ?key,
                "switch_active_session called with unknown key"
            );
            return;
        }

        // `App.status` is derived freshly from the destination
        // bucket's `lifecycle_state` instead of being snapshotted, so
        // a background turn that completed while the user was away
        // doesn't leave a stale `Thinking`/`Running` status on the
        // incoming bucket. Input state lives on each `UiSession`, so
        // switching `active_session_key` naturally swaps the editor
        // — no draft snapshot/restore needed.
        let incoming_lifecycle = self
            .sessions
            .get(&key)
            .map_or(crate::app::session::SessionLifecycleState::Idle, |s| s.lifecycle_state);
        self.active_session_key = Some(key);
        self.status = status_for_lifecycle(incoming_lifecycle);
        // Update terminal/tab title immediately on switch so the host
        // terminal reflects the project the user just selected. The
        // render-loop's tab-title call (in `app::run`) only fires
        // every animating frame or on explicit `needs_redraw`
        // transitions; some terminals coalesce/debounce OSC 2 titles
        // when fired close together, so calling here directly with
        // the incoming bucket's cwd guarantees one canonical update
        // per switch.
        crate::app::tab_title::update_tab_title(&self.status, self.spinner_frame, self.cwd());
        // Ensure the file index for `@`-mention autocomplete is
        // started for the incoming bucket. Each bucket owns its own
        // `FileIndexState`; if this is the first time we've switched
        // to this bucket the index is empty and needs a fresh scan
        // against the bucket's cwd. `ensure_started` is idempotent:
        // it's a no-op when the bucket's index is already scanning
        // or has a current root matching the cwd.
        crate::app::file_index::ensure_started(self);
        // No explicit git-diff refresh on session switch — the 10s
        // timer (which fires its first tick immediately) catches any
        // stale snapshot on the next pump cycle.
        //
        // Activation parity with the chat-direct path
        // (`forge <project>`). That path lands the user in a fully
        // wired session via `apply_connected_presentation`'s active
        // branch — file index restart, chat focus rebuild, runtime
        // tabs refresh, the same per-session refresh chain. The
        // launchpad-pick path spawns the project in the BACKGROUND
        // branch (because `__conn_pending__` is still active at
        // Connected time) and then relies on `switch_active_session`
        // to bring the bucket up to the same activation level.
        // Without these calls clicking forge from the launchpad
        // leaves the chat input unfocused, the runtime tabs stale,
        // and the bottom panel bars empty even though the bucket
        // itself carries the data.
        crate::app::file_index::restart(self);
        self.rebuild_chat_focus_from_state();
        crate::app::config::refresh_runtime_tabs_for_session_change(self);
        crate::app::session_runtime::request_status_snapshot_refresh(self);
        crate::app::session_runtime::request_oauth_credentials_snapshot_refresh(self);
        crate::app::session_runtime::request_context_usage_refresh(self);
        crate::app::usage::request_refresh_if_needed(self);
        self.sync_welcome_snapshot();
        self.force_redraw = true;
        self.needs_redraw = true;
    }

    /// Borrow the active session's chat buffer.
    ///
    /// Production startup and `App::test_default()` both seed a
    /// pre-Connect bucket so the active session is always populated.
    /// On the off chance the invariant is violated we log and return
    /// a static empty slice so call sites stay infallible.
    pub fn messages(&self) -> &[ChatMessage] {
        self.active_session().map_or(&[], |s| s.messages.as_slice())
    }

    /// Mutable borrow of the active session's chat buffer.
    ///
    /// Returns a mutable reference to the active bucket's `messages`
    /// vector. Auto-creates the pre-Connect bucket if the active
    /// session is missing, so call sites don't need to guard.
    pub fn active_messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.active_bucket_mut().messages
    }

    /// Borrow the parallel `message_retained_bytes` cache.
    pub fn message_retained_bytes(&self) -> &[usize] {
        self.active_session().map_or(&[], |s| s.message_retained_bytes.as_slice())
    }

    /// Mutable borrow of the `message_retained_bytes` cache.
    pub fn message_retained_bytes_mut(&mut self) -> &mut Vec<usize> {
        &mut self.active_bucket_mut().message_retained_bytes
    }

    /// Active session's rolling retained-history byte total.
    pub fn retained_history_bytes(&self) -> usize {
        self.active_session().map_or(0, |s| s.retained_history_bytes)
    }

    /// Mutable accessor for the rolling retained-history byte total.
    pub fn retained_history_bytes_mut(&mut self) -> &mut usize {
        &mut self.active_bucket_mut().retained_history_bytes
    }

    /// Borrow the active session's chat viewport.
    ///
    /// Falls back to a leaked default viewport if the active bucket
    /// is missing — the production startup path always seeds one,
    /// so the fallback is a safety net rather than a hot path.
    pub fn viewport(&self) -> &ChatViewport {
        static FALLBACK: std::sync::OnceLock<ChatViewport> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.viewport,
            None => FALLBACK.get_or_init(ChatViewport::new),
        }
    }

    /// Mutable accessor for the active session's chat viewport.
    /// Auto-creates the pre-Connect bucket if missing.
    pub fn active_viewport_mut(&mut self) -> &mut ChatViewport {
        &mut self.active_bucket_mut().viewport
    }

    /// Active session's main-assistant turn message index.
    pub fn active_turn_assistant_message_idx(&self) -> Option<usize> {
        self.active_session().and_then(|s| s.active_turn_assistant_message_idx)
    }

    /// Set the active session's main-assistant turn message index.
    pub fn set_active_turn_assistant_message_idx(&mut self, idx: Option<usize>) {
        self.active_bucket_mut().active_turn_assistant_message_idx = idx;
    }

    /// Internal helper: yield a `&mut Session` for the active bucket,
    /// auto-creating a pre-Connect synthetic bucket if no active
    /// session exists. Used by the `_mut` accessors so call sites
    /// can stay infallible.
    ///
    /// Hot path: chat render and ~50 other `_mut` accessors hit this
    /// per frame. Uses the `HashMap::entry` API to avoid the extra
    /// `SessionKey` clone an `if !contains { insert }` shape would
    /// need.
    fn active_bucket_mut(&mut self) -> &mut super::session::UiSession {
        use std::collections::hash_map::Entry;
        // The active key is normally already set; the synthetic
        // fallback is the cold first-touch path.
        let key = if let Some(key) = self.active_session_key.clone() {
            key
        } else {
            let synthetic = forge_workspace::SessionKey::from_session_id(Self::PRE_CONNECT_KEY);
            self.active_session_key = Some(synthetic.clone());
            synthetic
        };
        match self.sessions.entry(key) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let new = super::session::UiSession::new(e.key().clone());
                e.insert(new)
            }
        }
    }

    /// Active session's claude session id, or `None` in the
    /// pre-Connect window.
    ///
    /// Workspace keeps an internal copy on `DomainSession.session_id`
    /// for `AgentHandle` dispatch; TUI mirrors that id onto the
    /// active bucket via `set_session_id`. This accessor reads the
    /// TUI mirror so render code doesn't need to lock the workspace.
    pub fn session_id(&self) -> Option<model::SessionId> {
        self.active_session()
            .and_then(|s| s.session_id.as_ref())
            .map(|sid| model::SessionId::new(sid.to_string()))
    }

    /// Set the active session's session_id. Ensures the sessions
    /// map has an entry keyed by the id; sets `active_session_key`
    /// to that entry.
    ///
    /// `id = None` clears the active bucket's `session_id` and
    /// `key` fields but leaves the bucket attached to
    /// `active_session_key`. The active-path event handlers
    /// (`auth_required`, `connection_failed`) call this from inside
    /// a longer cleanup sequence that still needs to write into the
    /// active bucket — finalizing in-flight tool calls to Failed,
    /// pushing system messages — so the user can see what happened.
    /// Removing the bucket here would orphan that work into a
    /// freshly-minted pre-Connect bucket.
    ///
    /// If a synthetic-keyed bucket exists (from an earlier
    /// `install_testing_stub` before `set_session_id` — test ordering),
    /// migrates that bucket's contents to the real key so the conn
    /// + session_id end up on the same bucket.
    ///
    /// Leak guard: when `active_session_key` was previously `None`
    /// (Connect-after-failure path), sweeps stale buckets from
    /// earlier disconnect cycles.
    pub fn set_session_id(&mut self, id: Option<model::SessionId>) {
        if let Some(id) = id {
            {
                let prev_active_was_none = self.active_session_key.is_none();
                let key = forge_workspace::SessionKey::from_session_id(id.to_string());
                let primitive_id = forge_primitives::SessionId::new(id.to_string());
                // Migrate any synthetic-keyed bucket onto the real key.
                // Guard against the case where BOTH a synthetic bucket
                // and the real-key bucket already exist: in that case
                // the real bucket is authoritative, and we must NOT
                // overwrite it with the synthetic. Stamp the real
                // bucket's session_id and drop the synthetic.
                let pending = forge_workspace::SessionKey::from_session_id(Self::PRE_CONNECT_KEY);
                if let Some(mut existing) = self.sessions.remove(&pending) {
                    if self.sessions.contains_key(&key) {
                        tracing::warn!(
                            target: crate::logging::targets::APP_SESSION,
                            event_name = "set_session_id_synthetic_dropped",
                            message = "synthetic pre-Connect bucket dropped because the real-key bucket already existed",
                            outcome = "dropped",
                            session_id = %id,
                            reason = "real_bucket_present",
                        );
                        drop(existing);
                    } else {
                        existing.key = Some(key.clone());
                        existing.session_id = Some(primitive_id.clone());
                        self.sessions.insert(key.clone(), existing);
                        // Mirror the bucket re-key onto the workspace's
                        // `DomainSession` handle map so the workspace's
                        // `domain_session_for(real_key)` lookup keeps
                        // resolving after the synthetic→real migration.
                        if let Some(ws) = self.workspace.as_ref() {
                            ws.rekey_domain_session(&pending, key.clone());
                        }
                    }
                } else {
                    let bucket = self
                        .sessions
                        .entry(key.clone())
                        .or_insert_with(|| super::session::UiSession::new(key.clone()));
                    bucket.session_id = Some(primitive_id.clone());
                }
                self.active_session_key = Some(key.clone());
                // Mirror `session_id` onto the workspace's
                // DomainSession so `AgentHandle` dispatch (which
                // routes by claude-issued session UUID) finds it.
                // Auto-create a handle-less domain when the workspace
                // doesn't yet have one for `key` — covers the rare
                // test path that calls `set_session_id` before any
                // domain is registered.
                if let Some(ws) = self.workspace.as_ref() {
                    if ws.domain_session_for(&key).is_none() {
                        ws.register_domain_session(key.clone(), None);
                    }
                    ws.set_session_id_in_domain(&key, Some(primitive_id));
                }
                // If we landed on an existing bucket without going
                // through the synthetic-migration branch above, ensure
                // its `session_id` mirror is current.
                if let Some(bucket) = self.sessions.get_mut(&key) {
                    bucket.session_id = Some(forge_primitives::SessionId::new(id.to_string()));
                }
                // Connect-after-failure cleanup: when no session was
                // active before this call, sweep stale buckets that
                // accumulated across earlier disconnect cycles.
                if prev_active_was_none {
                    self.sessions.retain(|k, _| *k == key);
                }
            }
        } else {
            // Clear the bucket's `key` field so it stops
            // advertising the now-stale id (the next
            // `set_session_id(Some(...))` re-stamps it). Keep
            // the bucket attached to `active_session_key` so
            // the active-path handler can keep writing into it
            // (failed tool calls, system messages — see doc
            // comment above). Also clear the workspace's
            // DomainSession session_id so readers observe `None`.
            if let Some(s) = self.try_active_bucket_mut() {
                s.key = None;
                s.session_id = None;
            }
            if let Some(ws) = self.workspace.as_ref()
                && let Some(key) = self.active_session_key.as_ref()
            {
                ws.set_session_id_in_domain(key, None);
            }
        }
    }

    /// `true` when the active session has a registered agent handle
    /// in the workspace's `DomainSession`. Production code consults
    /// this rather than holding an `Arc<AgentHandle>` directly —
    /// outbound traffic flows through `Workspace::dispatch` /
    /// `Workspace::refresh_*` calls.
    pub fn has_active_agent(&self) -> bool {
        let Some(workspace) = self.workspace.as_ref() else { return false };
        let Some(key) = self.active_session_key.as_ref() else { return false };
        workspace.has_agent_for(key)
    }

    /// Dispatch a workspace [`forge_workspace::Command`] for the
    /// active session. Stamps the active `SessionKey` onto
    /// `builder`'s output before dispatching. No-op (returns
    /// `Err(UnknownSession)`) when there is no active session.
    ///
    /// # Errors
    ///
    /// Propagates [`forge_workspace::DispatchError`] from the
    /// underlying `Workspace::dispatch`.
    pub fn dispatch_command(
        &self,
        builder: impl FnOnce(forge_workspace::SessionKey) -> forge_workspace::Command,
    ) -> Result<(), forge_workspace::DispatchError> {
        let workspace = self.workspace.as_ref().ok_or_else(|| {
            forge_workspace::DispatchError::UnknownSession(
                forge_workspace::SessionKey::from_session_id("__no_workspace__"),
            )
        })?;
        let key = self.active_session_key.clone().ok_or_else(|| {
            forge_workspace::DispatchError::UnknownSession(
                forge_workspace::SessionKey::from_session_id("__no_active__"),
            )
        })?;
        workspace.dispatch(builder(key))
    }

    /// Install a fresh testing stub agent against the active
    /// session's [`forge_workspace::DomainSession`], auto-creating a
    /// pre-Connect bucket when no active session exists yet. Returns
    /// the matching `forge_primitives::AgentCommand` receiver so tests can
    /// assert on the commands the workspace routes through the stub.
    ///
    /// Test-only entry point: production flows register handles via
    /// `Workspace::get_agent_handle`.
    #[cfg(any(test, feature = "testing"))]
    #[allow(clippy::expect_used, clippy::missing_panics_doc)]
    pub fn install_testing_stub(
        &mut self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<forge_primitives::AgentCommand> {
        if self.active_session_key.is_none() {
            let key = forge_workspace::SessionKey::from_session_id(Self::PRE_CONNECT_KEY);
            self.sessions
                .entry(key.clone())
                .or_insert_with(|| super::session::UiSession::new(key.clone()));
            self.active_session_key = Some(key);
        }
        let key = self.active_session_key.clone().expect("active_session_key was just set above");
        let workspace = self.workspace.as_ref().expect("workspace required for testing stub");
        workspace.install_testing_stub(&key)
    }

    /// Detach the testing-stub agent from the active session's
    /// `DomainSession` (no-op when none is installed).
    #[cfg(any(test, feature = "testing"))]
    pub fn clear_active_conn(&mut self) {
        let Some(key) = self.active_session_key.clone() else { return };
        let Some(workspace) = self.workspace.as_ref() else { return };
        if let Some(domain) = workspace.domain_session_for(&key) {
            domain.lock().conn = None;
        }
    }

    /// Active session's monotonic scope epoch.
    pub fn session_scope_epoch(&self) -> u64 {
        self.active_session().map_or(0, |s| s.session_scope_epoch)
    }

    /// Increment the active session's scope epoch.
    pub fn bump_session_scope_epoch(&mut self) {
        let bucket = self.active_bucket_mut();
        bucket.session_scope_epoch = bucket.session_scope_epoch.saturating_add(1);
    }

    // ---- Turn lifecycle accessors ----

    /// Run `f` with read-only access to the active session's
    /// turn state. Falls through to a fresh `SessionTurnState::default()`
    /// when no active bucket exists (pre-Connect window).
    pub fn with_turn_state<R>(&self, f: impl FnOnce(&SessionTurnState) -> R) -> R {
        match self.active_session() {
            Some(s) => f(&s.turn_state),
            None => f(&SessionTurnState::default()),
        }
    }

    /// Run `f` with mutable access to the active session's turn
    /// state. Auto-creates the pre-Connect bucket if missing.
    pub fn with_turn_state_mut<R>(&mut self, f: impl FnOnce(&mut SessionTurnState) -> R) -> R {
        f(&mut self.active_bucket_mut().turn_state)
    }

    /// Active session's `is_compacting` flag.
    pub fn is_compacting(&self) -> bool {
        self.active_session().is_some_and(|s| s.is_compacting)
    }

    /// Set the active session's `is_compacting` flag.
    pub fn set_is_compacting(&mut self, value: bool) {
        self.active_bucket_mut().is_compacting = value;
    }

    /// Active session's `pending_compact_clear` flag.
    pub fn pending_compact_clear(&self) -> bool {
        self.active_session().is_some_and(|s| s.pending_compact_clear)
    }

    /// Set the active session's `pending_compact_clear` flag.
    pub fn set_pending_compact_clear(&mut self, value: bool) {
        self.active_bucket_mut().pending_compact_clear = value;
    }

    /// Borrow the active session's pending interaction id list.
    pub fn pending_interaction_ids(&self) -> &[String] {
        self.active_session().map_or(&[], |s| s.pending_interaction_ids.as_slice())
    }

    /// Mutable borrow of the pending interaction id list.
    pub fn pending_interaction_ids_mut(&mut self) -> &mut Vec<String> {
        &mut self.active_bucket_mut().pending_interaction_ids
    }

    /// Active session's cancelled-turn pending hint flag.
    pub fn cancelled_turn_pending_hint(&self) -> bool {
        self.active_session().is_some_and(|s| s.cancelled_turn_pending_hint)
    }

    /// Set the active session's cancelled-turn pending hint flag.
    pub fn set_cancelled_turn_pending_hint(&mut self, value: bool) {
        self.active_bucket_mut().cancelled_turn_pending_hint = value;
    }

    /// Active session's pending cancel origin.
    pub fn pending_cancel(&self) -> bool {
        self.active_session().is_some_and(|s| s.pending_cancel)
    }

    /// Set the active session's pending cancel origin.
    pub fn set_pending_cancel(&mut self, value: bool) {
        self.active_bucket_mut().pending_cancel = value;
    }

    /// Borrow the active session's prompt suggestion.
    pub fn prompt_suggestion(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.prompt_suggestion.as_deref())
    }

    /// Set the active session's prompt suggestion.
    pub fn set_prompt_suggestion(&mut self, value: Option<String>) {
        self.active_bucket_mut().prompt_suggestion = value;
    }

    /// Borrow the active session's last rate-limit update.
    pub fn last_rate_limit_update(&self) -> Option<&model::RateLimitUpdate> {
        self.active_session().and_then(|s| s.last_rate_limit_update.as_ref())
    }

    /// Set the active session's last rate-limit update.
    pub fn set_last_rate_limit_update(&mut self, value: Option<model::RateLimitUpdate>) {
        self.active_bucket_mut().last_rate_limit_update = value;
    }

    /// Borrow the active session's turn notice ref list.
    pub fn turn_notice_refs(&self) -> &[TurnNoticeRef] {
        self.active_session().map_or(&[], |s| s.turn_notice_refs.as_slice())
    }

    /// Mutable borrow of the turn notice ref list.
    pub fn turn_notice_refs_mut(&mut self) -> &mut Vec<TurnNoticeRef> {
        &mut self.active_bucket_mut().turn_notice_refs
    }

    // ---- Tool tracking accessors ----

    /// Borrow the active session's active task id set.
    ///
    /// Falls back to a leaked empty set when the active bucket is
    /// missing — matches the existing infallible-reader pattern
    /// (`viewport()`, `turn_state()`, …).
    pub fn active_task_ids(&self) -> &HashSet<String> {
        static FALLBACK: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.active_task_ids,
            None => FALLBACK.get_or_init(HashSet::new),
        }
    }

    /// Mutable borrow of the active task id set.
    pub fn active_task_ids_mut(&mut self) -> &mut HashSet<String> {
        &mut self.active_bucket_mut().active_task_ids
    }

    /// Borrow the active session's tool call scope map.
    pub fn tool_call_scopes(&self) -> &HashMap<String, ToolCallScope> {
        static FALLBACK: std::sync::OnceLock<HashMap<String, ToolCallScope>> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.tool_call_scopes,
            None => FALLBACK.get_or_init(HashMap::new),
        }
    }

    /// Mutable borrow of the tool call scope map.
    pub fn tool_call_scopes_mut(&mut self) -> &mut HashMap<String, ToolCallScope> {
        &mut self.active_bucket_mut().tool_call_scopes
    }

    /// Borrow the active session's tool call index.
    pub fn tool_call_index(&self) -> &HashMap<String, (usize, usize)> {
        static FALLBACK: std::sync::OnceLock<HashMap<String, (usize, usize)>> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.tool_call_index,
            None => FALLBACK.get_or_init(HashMap::new),
        }
    }

    /// Mutable borrow of the tool call index.
    pub fn active_tool_call_index_mut(&mut self) -> &mut HashMap<String, (usize, usize)> {
        &mut self.active_bucket_mut().tool_call_index
    }

    /// Borrow the active session's terminal map (a shared
    /// `Rc<RefCell<...>>`).
    ///
    /// Returns `None` only in the brief pre-Connect window where no
    /// session bucket exists. Stays fallible because `TerminalMap`
    /// (`Rc<RefCell<...>>`) is `!Send + !Sync`, so a `OnceLock`
    /// fallback (the pattern used by the other infallible readers)
    /// won't compile. Both `App::test_default()` and `connect()`
    /// seed a bucket up front so production callers can treat this
    /// as effectively always-`Some`.
    pub fn terminals(&self) -> Option<&crate::agent::events::TerminalMap> {
        self.active_session().map(|s| &s.terminals)
    }

    /// Mutable borrow of the active session's terminal map.
    /// Auto-creates the pre-Connect bucket if missing.
    pub fn terminals_mut(&mut self) -> &mut crate::agent::events::TerminalMap {
        &mut self.active_bucket_mut().terminals
    }

    /// Borrow the active session's terminal tool call list.
    pub fn terminal_tool_calls(&self) -> &[TerminalToolCallRef] {
        self.active_session().map_or(&[], |s| s.terminal_tool_calls.as_slice())
    }

    /// Mutable borrow of the terminal tool call list.
    pub fn terminal_tool_calls_mut(&mut self) -> &mut Vec<TerminalToolCallRef> {
        &mut self.active_bucket_mut().terminal_tool_calls
    }

    /// Borrow the active session's terminal tool call membership
    /// set.
    pub fn terminal_tool_call_membership(&self) -> &HashSet<TerminalToolCallRef> {
        static FALLBACK: std::sync::OnceLock<HashSet<TerminalToolCallRef>> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.terminal_tool_call_membership,
            None => FALLBACK.get_or_init(HashSet::new),
        }
    }

    /// Mutable borrow of the terminal tool call membership set.
    pub fn terminal_tool_call_membership_mut(&mut self) -> &mut HashSet<TerminalToolCallRef> {
        &mut self.active_bucket_mut().terminal_tool_call_membership
    }

    /// Borrow the active session's subagent attribution map.
    pub fn subagent_attribution(&self) -> &HashMap<String, String> {
        static FALLBACK: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.subagent_attribution,
            None => FALLBACK.get_or_init(HashMap::new),
        }
    }

    /// Mutable borrow of the subagent attribution map.
    pub fn subagent_attribution_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.active_bucket_mut().subagent_attribution
    }

    // ---- Runtime + model accessors ----

    /// Borrow the active session's current model resolution.
    pub fn current_model(&self) -> Option<&model::CurrentModel> {
        self.active_session().and_then(|s| s.current_model.as_ref())
    }

    /// Set the active session's current model resolution.
    pub fn set_current_model(&mut self, value: Option<model::CurrentModel>) {
        self.active_bucket_mut().current_model = value;
    }

    /// Mutable borrow of the active session's current model.
    pub fn current_model_mut(&mut self) -> Option<&mut model::CurrentModel> {
        self.active_bucket_mut().current_model.as_mut()
    }

    /// Borrow the active session's available-models list.
    pub fn available_models(&self) -> &[model::AvailableModel] {
        self.active_session().map_or(&[], |s| s.available_models.as_slice())
    }

    /// Mutable borrow of the available-models list.
    pub fn available_models_mut(&mut self) -> &mut Vec<model::AvailableModel> {
        &mut self.active_bucket_mut().available_models
    }

    /// Borrow the active session's available-commands list.
    pub fn available_commands(&self) -> &[model::AvailableCommand] {
        self.active_session().map_or(&[], |s| s.available_commands.as_slice())
    }

    /// Mutable borrow of the available-commands list.
    pub fn available_commands_mut(&mut self) -> &mut Vec<model::AvailableCommand> {
        &mut self.active_bucket_mut().available_commands
    }

    /// Borrow the active session's available-agents list.
    pub fn available_agents(&self) -> &[model::AvailableAgent] {
        self.active_session().map_or(&[], |s| s.available_agents.as_slice())
    }

    /// Mutable borrow of the available-agents list.
    pub fn available_agents_mut(&mut self) -> &mut Vec<model::AvailableAgent> {
        &mut self.active_bucket_mut().available_agents
    }

    /// Borrow the active session's mode snapshot.
    pub fn mode(&self) -> Option<&ModeState> {
        self.active_session().and_then(|s| s.mode.as_ref())
    }

    /// Set the active session's mode snapshot.
    pub fn set_mode(&mut self, value: Option<ModeState>) {
        self.active_bucket_mut().mode = value;
    }

    /// Mutable borrow of the active session's mode snapshot.
    pub fn mode_mut(&mut self) -> Option<&mut ModeState> {
        self.active_bucket_mut().mode.as_mut()
    }

    /// Active session's hook-observed permission mode.
    pub fn observed_permission_mode(&self) -> Option<forge_workspace::PermissionMode> {
        self.active_session().and_then(|s| s.observed_permission_mode)
    }

    /// Set the active session's hook-observed permission mode.
    pub fn set_observed_permission_mode(&mut self, value: Option<forge_workspace::PermissionMode>) {
        self.active_bucket_mut().observed_permission_mode = value;
    }

    /// Active session's hook-observed effort level.
    pub fn observed_effort(&self) -> Option<model::EffortLevel> {
        self.active_session().and_then(|s| s.observed_effort)
    }

    /// Set the active session's hook-observed effort level.
    pub fn set_observed_effort(&mut self, value: Option<model::EffortLevel>) {
        self.active_bucket_mut().observed_effort = value;
    }

    /// Borrow the active session's observed assistant model id.
    pub fn observed_assistant_model(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.observed_assistant_model.as_deref())
    }

    /// Set the active session's observed assistant model id.
    pub fn set_observed_assistant_model(&mut self, value: Option<String>) {
        self.active_bucket_mut().observed_assistant_model = value;
    }

    /// Active session's runtime session state.
    pub fn runtime_session_state(&self) -> Option<model::RuntimeSessionState> {
        self.active_session().and_then(|s| s.runtime_session_state)
    }

    /// Set the active session's runtime session state.
    pub fn set_runtime_session_state(&mut self, value: Option<model::RuntimeSessionState>) {
        self.active_bucket_mut().runtime_session_state = value;
    }

    /// Active session's fast-mode state.
    pub fn fast_mode_state(&self) -> model::FastModeState {
        self.active_session().map_or(model::FastModeState::Off, |s| s.fast_mode_state)
    }

    /// Set the active session's fast-mode state.
    pub fn set_fast_mode_state(&mut self, value: model::FastModeState) {
        self.active_bucket_mut().fast_mode_state = value;
    }

    /// Borrow the active session's config-options map.
    pub fn config_options(&self) -> &BTreeMap<String, serde_json::Value> {
        static FALLBACK: std::sync::OnceLock<BTreeMap<String, serde_json::Value>> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.config_options,
            None => FALLBACK.get_or_init(BTreeMap::new),
        }
    }

    /// Mutable borrow of the config-options map.
    pub fn config_options_mut(&mut self) -> &mut BTreeMap<String, serde_json::Value> {
        &mut self.active_bucket_mut().config_options
    }

    /// Borrow the active session's session-usage telemetry.
    pub fn session_usage(&self) -> &SessionUsageState {
        static FALLBACK: std::sync::OnceLock<SessionUsageState> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.session_usage,
            None => FALLBACK.get_or_init(SessionUsageState::default),
        }
    }

    /// Mutable borrow of the session-usage telemetry.
    pub fn session_usage_mut(&mut self) -> &mut SessionUsageState {
        &mut self.active_bucket_mut().session_usage
    }

    /// Borrow the active session's Anthropic-plan usage state. The
    /// pane footer's `5h` / `7d` bars read this. Returns a static
    /// empty state during the brief pre-Connect window where no
    /// session bucket exists yet.
    pub fn usage(&self) -> &UsageState {
        static FALLBACK: std::sync::OnceLock<UsageState> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.usage,
            None => FALLBACK.get_or_init(UsageState::default),
        }
    }

    /// Mutable borrow of the active session's usage state. Used by
    /// `app::usage::request_refresh` to flip the in-flight flag
    /// before spawning the fetch task.
    pub fn usage_mut(&mut self) -> &mut UsageState {
        &mut self.active_bucket_mut().usage
    }

    /// Mutable borrow of a specific session's usage state by key.
    /// Used by the `Usage*` reducers in `events/client.rs` to route
    /// a fetch result onto the bucket that requested it, even if
    /// the user has switched active session mid-fetch. Returns
    /// `None` when the target bucket no longer exists (session
    /// closed before the result landed — drop the result silently).
    pub fn usage_mut_for(&mut self, key: &forge_workspace::SessionKey) -> Option<&mut UsageState> {
        self.sessions.get_mut(key).map(|s| &mut s.usage)
    }

    /// Active session's catalog of resumable sessions. The
    /// `/resume <id>` autocomplete and startup picker read from
    /// this list. Returns an empty slice in the brief pre-Connect
    /// window where no bucket exists.
    pub fn recent_sessions(&self) -> &[RecentSessionInfo] {
        self.active_session().map_or(&[], |s| s.recent_sessions.as_slice())
    }

    /// Mutable borrow of the active session's recent-sessions list.
    /// Used by tests + the SDK-side bridge polling path.
    pub fn recent_sessions_mut(&mut self) -> &mut Vec<RecentSessionInfo> {
        &mut self.active_bucket_mut().recent_sessions
    }

    /// Mutable borrow of a specific bucket's recent-sessions list.
    /// Used by `handle_sessions_listed_event` to route the wire
    /// payload onto the bucket that requested the scan.
    pub fn recent_sessions_mut_for(
        &mut self,
        key: &forge_workspace::SessionKey,
    ) -> Option<&mut Vec<RecentSessionInfo>> {
        self.sessions.get_mut(key).map(|s| &mut s.recent_sessions)
    }

    // ---- Per-session UI/input accessors (latent smells migrated) ----
    //
    // Each pair below mirrors a `UiSession` field with a read accessor
    // returning a reference and a mut accessor returning `&mut <T>`.
    // The mut accessor auto-creates the active bucket via
    // `active_bucket_mut`, so call sites can always write.

    pub fn login_hint(&self) -> Option<&LoginHint> {
        self.active_session().and_then(|s| s.login_hint.as_ref())
    }
    pub fn login_hint_mut(&mut self) -> &mut Option<LoginHint> {
        &mut self.active_bucket_mut().login_hint
    }

    pub fn resuming_session_id(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.resuming_session_id.as_deref())
    }
    pub fn resuming_session_id_mut(&mut self) -> &mut Option<String> {
        &mut self.active_bucket_mut().resuming_session_id
    }

    pub fn pending_command_label(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.pending_command_label.as_deref())
    }
    pub fn pending_command_label_mut(&mut self) -> &mut Option<String> {
        &mut self.active_bucket_mut().pending_command_label
    }

    pub fn pending_command_ack(&self) -> Option<&PendingCommandAck> {
        self.active_session().and_then(|s| s.pending_command_ack.as_ref())
    }
    pub fn pending_command_ack_mut(&mut self) -> &mut Option<PendingCommandAck> {
        &mut self.active_bucket_mut().pending_command_ack
    }

    pub fn selection(&self) -> Option<&SelectionState> {
        self.active_session().and_then(|s| s.selection.as_ref())
    }
    pub fn selection_mut(&mut self) -> &mut Option<SelectionState> {
        &mut self.active_bucket_mut().selection
    }

    pub fn pending_submit(&self) -> Option<&InputSnapshot> {
        self.active_session().and_then(|s| s.pending_submit.as_ref())
    }
    pub fn pending_submit_mut(&mut self) -> &mut Option<InputSnapshot> {
        &mut self.active_bucket_mut().pending_submit
    }

    pub fn pending_paste_text(&self) -> &str {
        self.active_session().map_or("", |s| s.pending_paste_text.as_str())
    }
    pub fn pending_paste_text_mut(&mut self) -> &mut String {
        &mut self.active_bucket_mut().pending_paste_text
    }

    pub fn pending_paste_session(&self) -> Option<&PasteSessionState> {
        self.active_session().and_then(|s| s.pending_paste_session.as_ref())
    }
    pub fn pending_paste_session_mut(&mut self) -> &mut Option<PasteSessionState> {
        &mut self.active_bucket_mut().pending_paste_session
    }

    pub fn active_paste_session(&self) -> Option<&PasteSessionState> {
        self.active_session().and_then(|s| s.active_paste_session.as_ref())
    }
    pub fn active_paste_session_mut(&mut self) -> &mut Option<PasteSessionState> {
        &mut self.active_bucket_mut().active_paste_session
    }

    pub fn next_paste_session_id(&self) -> u64 {
        self.active_session().map_or(1, |s| s.next_paste_session_id)
    }
    pub fn allocate_paste_session_id(&mut self) -> u64 {
        let slot = &mut self.active_bucket_mut().next_paste_session_id;
        let id = *slot;
        *slot = slot.saturating_add(1);
        id
    }

    pub fn pending_images(&self) -> &[crate::app::clipboard_image::ImageAttachment] {
        self.active_session().map_or(&[], |s| s.pending_images.as_slice())
    }
    pub fn pending_images_mut(&mut self) -> &mut Vec<crate::app::clipboard_image::ImageAttachment> {
        &mut self.active_bucket_mut().pending_images
    }

    pub fn mention(&self) -> Option<&mention::MentionState> {
        self.active_session().and_then(|s| s.mention.as_ref())
    }
    pub fn mention_mut(&mut self) -> &mut Option<mention::MentionState> {
        &mut self.active_bucket_mut().mention
    }

    pub fn slash(&self) -> Option<&slash::SlashState> {
        self.active_session().and_then(|s| s.slash.as_ref())
    }
    pub fn slash_mut(&mut self) -> &mut Option<slash::SlashState> {
        &mut self.active_bucket_mut().slash
    }

    pub fn subagent(&self) -> Option<&subagent::SubagentState> {
        self.active_session().and_then(|s| s.subagent.as_ref())
    }
    pub fn subagent_mut(&mut self) -> &mut Option<subagent::SubagentState> {
        &mut self.active_bucket_mut().subagent
    }

    /// Active session's file-index state for `@`-mention autocomplete.
    /// Returns an empty default state when no active session exists
    /// (test paths, brief pre-Connect window).
    pub fn file_index(&self) -> &super::file_index::FileIndexState {
        static FALLBACK: std::sync::OnceLock<super::file_index::FileIndexState> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.file_index,
            None => FALLBACK.get_or_init(super::file_index::FileIndexState::default),
        }
    }

    /// Mutable borrow of the active session's file index. Used by
    /// the scanner + watcher lifecycle in `app::file_index` and the
    /// `@`-mention reducer in `app::mention`.
    pub fn file_index_mut(&mut self) -> &mut super::file_index::FileIndexState {
        &mut self.active_bucket_mut().file_index
    }

    // ---- Account / auth accessors ----

    /// Active session's account-info snapshot.
    pub fn account_info(&self) -> Option<forge_primitives::AccountInfo> {
        self.active_session().and_then(|s| s.account_info.clone())
    }

    /// Set the active session's account-info snapshot.
    pub fn set_account_info(&mut self, value: Option<forge_primitives::AccountInfo>) {
        self.active_bucket_mut().account_info = value;
    }

    /// Active session's forge-side account display name.
    pub fn active_account_display_name(&self) -> Option<String> {
        self.active_session().and_then(|s| s.active_account_display_name.clone())
    }

    /// Set the active session's forge-side account display name.
    pub fn set_active_account_display_name(&mut self, value: Option<String>) {
        self.active_bucket_mut().active_account_display_name = value;
    }

    /// Borrow the active session's OAuth credentials snapshot.
    pub fn oauth_credentials(
        &self,
    ) -> Option<&forge_primitives::cloud::oauth_credentials::OauthCredentials> {
        self.active_session().and_then(|s| s.oauth_credentials.as_ref())
    }

    /// Set the active session's OAuth credentials snapshot.
    pub fn set_oauth_credentials(
        &mut self,
        value: Option<forge_primitives::cloud::oauth_credentials::OauthCredentials>,
    ) {
        self.active_bucket_mut().oauth_credentials = value;
    }

    // ---- Filesystem accessors ----

    /// Borrow the active session's display-friendly cwd.
    ///
    /// Returns an empty string only in the brief pre-Connect window
    /// before any session bucket exists; production startup and
    /// `App::test_default()` both seed a bucket up front.
    pub fn cwd(&self) -> &str {
        self.active_session().map_or("", |s| s.cwd.as_str())
    }

    /// Set the active session's display-friendly cwd.
    pub fn set_cwd(&mut self, value: impl Into<String>) {
        self.active_bucket_mut().cwd = value.into();
    }

    /// Active session's raw filesystem cwd.
    pub fn cwd_raw(&self) -> String {
        self.active_session().map_or_else(String::new, |s| s.cwd_raw.clone())
    }

    /// Set the active session's raw filesystem cwd.
    pub fn set_cwd_raw(&mut self, value: impl Into<String>) {
        self.active_bucket_mut().cwd_raw = value.into();
    }

    /// Active session's files-accessed counter.
    pub fn files_accessed(&self) -> usize {
        self.active_session().map_or(0, |s| s.files_accessed)
    }

    /// Set the active session's files-accessed counter.
    pub fn set_files_accessed(&mut self, value: usize) {
        self.active_bucket_mut().files_accessed = value;
    }

    /// Increment the active session's files-accessed counter by one.
    pub fn increment_files_accessed(&mut self) {
        let s = self.active_bucket_mut();
        s.files_accessed = s.files_accessed.saturating_add(1);
    }

    /// Borrow the active session's MCP state snapshot.
    ///
    /// Falls back to a leaked default for the brief pre-Connect
    /// window. Production startup seeds a synthetic bucket up front,
    /// so the fallback is a safety net rather than a hot path.
    pub fn mcp(&self) -> &McpState {
        static FALLBACK: std::sync::OnceLock<McpState> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.mcp,
            None => FALLBACK.get_or_init(McpState::default),
        }
    }

    /// Mutable borrow of the active session's MCP state snapshot.
    /// Auto-creates the pre-Connect bucket if missing.
    pub fn mcp_mut(&mut self) -> &mut McpState {
        &mut self.active_bucket_mut().mcp
    }

    // ---- Todos accessors ----

    /// Borrow the active session's todo list.
    pub fn todos(&self) -> &[TodoItem] {
        self.active_session().map_or(&[], |s| s.todos.as_slice())
    }

    /// Mutable borrow of the active session's todo list.
    pub fn todos_mut(&mut self) -> &mut Vec<TodoItem> {
        &mut self.active_bucket_mut().todos
    }

    /// Active session's TodoWrite-verification-nudge flag.
    pub fn todo_verification_nudge(&self) -> bool {
        self.active_session().is_some_and(|s| s.todo_verification_nudge)
    }

    /// Set the active session's TodoWrite-verification-nudge flag.
    /// Called by the TodoWrite tool-result handler to surface (or
    /// clear) the dim-yellow notice above the Inspector pane's TASKS
    /// section.
    pub fn set_todo_verification_nudge(&mut self, value: bool) {
        self.active_bucket_mut().todo_verification_nudge = value;
    }

    // ---- Render cache + history retention accessors ----

    /// Borrow the active session's render-cache slot grid.
    pub(crate) fn render_cache_slots(&self) -> &[Vec<render_budget::RenderCacheSlotState>] {
        self.active_session().map_or(&[], |s| s.render_cache_slots.as_slice())
    }

    /// Mutable borrow of the active session's render-cache slot grid.
    /// Auto-creates the pre-Connect bucket if missing.
    pub(crate) fn render_cache_slots_mut(
        &mut self,
    ) -> &mut Vec<Vec<render_budget::RenderCacheSlotState>> {
        &mut self.active_bucket_mut().render_cache_slots
    }

    /// Active session's rolling render-cache total bytes.
    pub(crate) fn render_cache_total_bytes(&self) -> usize {
        self.active_session().map_or(0, |s| s.render_cache_total_bytes)
    }

    /// Mutable accessor for the rolling render-cache total bytes.
    pub(crate) fn render_cache_total_bytes_mut(&mut self) -> &mut usize {
        &mut self.active_bucket_mut().render_cache_total_bytes
    }

    /// Active session's rolling render-cache protected bytes.
    pub(crate) fn render_cache_protected_bytes(&self) -> usize {
        self.active_session().map_or(0, |s| s.render_cache_protected_bytes)
    }

    /// Mutable accessor for the rolling render-cache protected bytes.
    pub(crate) fn render_cache_protected_bytes_mut(&mut self) -> &mut usize {
        &mut self.active_bucket_mut().render_cache_protected_bytes
    }

    /// Borrow the active session's evictable render-cache key set.
    pub(crate) fn render_cache_evictable(
        &self,
    ) -> Option<&BTreeSet<render_budget::RenderCacheEvictionKey>> {
        self.active_session().map(|s| &s.render_cache_evictable)
    }

    /// Mutable borrow of the evictable render-cache key set.
    pub(crate) fn render_cache_evictable_mut(
        &mut self,
    ) -> &mut BTreeSet<render_budget::RenderCacheEvictionKey> {
        &mut self.active_bucket_mut().render_cache_evictable
    }

    /// Active session's protected streaming-tail message index, if any.
    pub(crate) fn render_cache_tail_msg_idx(&self) -> Option<usize> {
        self.active_session().and_then(|s| s.render_cache_tail_msg_idx)
    }

    /// Set the active session's protected streaming-tail message index.
    pub(crate) fn set_render_cache_tail_msg_idx(&mut self, value: Option<usize>) {
        self.active_bucket_mut().render_cache_tail_msg_idx = value;
    }

    /// Borrow the active session's history-retention policy.
    pub fn history_retention(&self) -> HistoryRetentionPolicy {
        self.active_session().map_or_else(HistoryRetentionPolicy::default, |s| s.history_retention)
    }

    /// Mutable accessor for the history-retention policy.
    pub fn history_retention_mut(&mut self) -> &mut HistoryRetentionPolicy {
        &mut self.active_bucket_mut().history_retention
    }

    /// Borrow the active session's history-retention enforcement
    /// statistics.
    pub fn history_retention_stats(&self) -> &HistoryRetentionStats {
        static FALLBACK: std::sync::OnceLock<HistoryRetentionStats> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.history_retention_stats,
            None => FALLBACK.get_or_init(HistoryRetentionStats::default),
        }
    }

    /// Mutable accessor for the history-retention enforcement
    /// statistics.
    pub fn history_retention_stats_mut(&mut self) -> &mut HistoryRetentionStats {
        &mut self.active_bucket_mut().history_retention_stats
    }

    /// Borrow the active session's cache-metrics accumulator.
    pub fn cache_metrics(&self) -> &CacheMetrics {
        static FALLBACK: std::sync::OnceLock<CacheMetrics> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.cache_metrics,
            None => FALLBACK.get_or_init(CacheMetrics::default),
        }
    }

    /// Mutable accessor for the cache-metrics accumulator.
    pub fn cache_metrics_mut(&mut self) -> &mut CacheMetrics {
        &mut self.active_bucket_mut().cache_metrics
    }

    /// Active session's previous-frame active-turn height state.
    pub(crate) fn last_active_turn_height_state(&self) -> Option<(usize, bool, bool)> {
        self.active_session().and_then(|s| s.last_active_turn_height_state)
    }

    /// Set the active session's previous-frame active-turn height state.
    pub(crate) fn set_last_active_turn_height_state(&mut self, value: Option<(usize, bool, bool)>) {
        self.active_bucket_mut().last_active_turn_height_state = value;
    }

    /// Borrow the active session's last chat-render trace snapshot.
    pub fn last_chat_render_trace_state(&self) -> Option<ChatRenderTraceState> {
        self.active_session().and_then(|s| s.last_chat_render_trace_state)
    }

    /// Set the active session's last chat-render trace snapshot.
    pub fn set_last_chat_render_trace_state(&mut self, value: Option<ChatRenderTraceState>) {
        self.active_bucket_mut().last_chat_render_trace_state = value;
    }

    /// Queue a paste payload for drain-cycle finalization.
    ///
    /// This is fed by paste payloads captured from terminal events.
    pub fn queue_paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let chunk_chars = text.chars().count();
        let had_pending_submit = self.pending_submit().is_some();
        *self.pending_submit_mut() = None;
        if self.pending_paste_text().is_empty() {
            let continued_session = self.active_paste_session().copied().and_then(|session| {
                let current_line = self.input().lines().get(self.input().cursor_row())?;
                let idx =
                    parse_paste_placeholder_before_cursor(current_line, self.input().cursor_col())?;
                (session.placeholder_index == Some(idx)).then_some(session)
            });
            let opened = continued_session.unwrap_or_else(|| {
                let id = self.allocate_paste_session_id();
                PasteSessionState {
                    id,
                    start: SelectionPoint {
                        row: self.input().cursor_row(),
                        col: self.input().cursor_col(),
                    },
                    placeholder_index: None,
                }
            });
            *self.pending_paste_session_mut() = Some(opened);
            tracing::debug!(
                target: crate::logging::targets::APP_PASTE,
                event_name = "paste_queue_opened",
                message = "paste queue session opened",
                outcome = "start",
                session_id = opened.id,
                start_row = opened.start.row,
                start_col = opened.start.col,
                placeholder_index = ?opened.placeholder_index,
                chunk_chars,
                had_pending_submit,
            );
        }
        self.pending_paste_text_mut().push_str(text);
        let pending_chars = self.pending_paste_text().chars().count();
        tracing::debug!(
            target: crate::logging::targets::APP_PASTE,
            event_name = "paste_queue_updated",
            message = "paste queue updated",
            outcome = "success",
            chunk_chars,
            pending_chars,
            had_pending_submit,
        );
    }

    /// Mark one presented frame at `now`, updating smoothed FPS.
    pub fn mark_frame_presented(&mut self, now: Instant) {
        let Some(prev) = self.last_frame_at.replace(now) else {
            return;
        };
        let dt = now.saturating_duration_since(prev).as_secs_f32();
        if dt <= f32::EPSILON {
            return;
        }
        let fps = (1.0 / dt).clamp(0.0, 240.0);
        self.fps_ema = Some(match self.fps_ema {
            Some(current) => current * 0.9 + fps * 0.1,
            None => fps,
        });
    }

    pub fn frame_fps(&self) -> Option<f32> {
        self.fps_ema
    }

    /// Ensure the synthetic welcome message exists at index 0.
    pub fn ensure_welcome_message(&mut self) {
        if self.messages().first().is_some_and(|m| matches!(m.role, MessageRole::Welcome)) {
            return;
        }
        self.insert_message_tracked(0, self.build_welcome_message());
    }

    /// Returns `(label, value)` for the welcome message's account
    /// line. The line's *layout slot* is reserved from the first
    /// frame in workspace mode — `Account: …` shows immediately,
    /// then the value fills in once data lands. Avoids the
    /// alternative options (line pops in late, or flickers
    /// `Granite` → `Granite · team`) that surface as stale UI.
    ///
    /// Resolution table:
    /// - Workspace mode + both pieces → `"Account: name · tier"`.
    /// - Workspace mode + partial/no data → `"Account: …"` skeleton.
    /// - Legacy mode (no workspace) + tier only → `"Subscription: tier"`.
    /// - Legacy mode + no data → empty (renderer hides line).
    fn welcome_account_display(&self) -> (String, String) {
        // Both accessors return owned values from the bucket; trim +
        // clone into owned form to avoid binding to temporaries.
        let display_name = self
            .active_account_display_name()
            .map(|n| n.trim().to_owned())
            .filter(|s| !s.is_empty());
        let subscription = self
            .account_info()
            .and_then(|a| a.subscription_type)
            .map(|t| t.trim().to_owned())
            .filter(|s| !s.is_empty());
        let workspace_mode = self.workspace.is_some();

        match (workspace_mode, display_name, subscription) {
            (_, Some(name), Some(tier)) => ("Account".to_owned(), format!("{name} · {tier}")),
            (true, _, _) => ("Account".to_owned(), "…".to_owned()),
            (false, _, Some(tier)) => ("Subscription".to_owned(), tier),
            (false, _, None) => (String::new(), String::new()),
        }
    }

    fn welcome_cwd_display(&self) -> &str {
        let cwd = self.cwd().trim();
        if cwd.is_empty() { "-" } else { cwd }
    }

    fn welcome_session_id_display(&self) -> String {
        self.session_id()
            .map(|s| s.to_string())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "-".to_owned())
    }

    pub(crate) fn build_welcome_message(&self) -> ChatMessage {
        let (label, value) = self.welcome_account_display();
        let session_id = self.welcome_session_id_display();
        let mut message = ChatMessage::welcome(
            crate::FORGE_VERSION,
            &value,
            self.welcome_cwd_display(),
            &session_id,
        );
        // Override the constructor's default "Subscription" label
        // with the dynamic one chosen by `welcome_account_display`.
        if let Some(MessageBlock::Welcome(welcome)) = message.blocks.first_mut() {
            welcome.account_label = label;
        }
        message
    }

    pub(crate) fn current_welcome_tip_seed(&self) -> Option<u64> {
        let first = self.messages().first()?;
        let MessageBlock::Welcome(welcome) = first.blocks.first()? else {
            return None;
        };
        Some(welcome.tip_seed)
    }

    pub(crate) fn apply_welcome_tip_seed(message: &mut ChatMessage, tip_seed: u64) {
        let Some(MessageBlock::Welcome(welcome)) = message.blocks.first_mut() else {
            return;
        };
        welcome.tip_seed = tip_seed;
    }

    /// Update the welcome message with the latest session/account snapshot.
    pub fn sync_welcome_snapshot(&mut self) {
        // Carry the build-stamped version (with short SHA) through
        // every sync, not the bare `CARGO_PKG_VERSION`. Otherwise the
        // first sync after construction strips the SHA off the
        // welcome banner — the launchpad version line still shows
        // `+<sha>`, but the chat-view welcome reads as bare
        // `0.15.1`, which makes screenshots ambiguous about which
        // commit was running.
        let version = crate::FORGE_VERSION;
        let (label, value) = self.welcome_account_display();
        let cwd = self.welcome_cwd_display().to_owned();
        let session_id = self.welcome_session_id_display();
        let Some(first) = self.active_messages_mut().first_mut() else {
            return;
        };
        if !matches!(first.role, MessageRole::Welcome) {
            return;
        }
        let Some(MessageBlock::Welcome(welcome)) = first.blocks.first_mut() else {
            return;
        };
        if welcome.version != version
            || welcome.account_label != label
            || welcome.subscription != value
            || welcome.cwd != cwd
            || welcome.session_id != session_id
        {
            version.clone_into(&mut welcome.version);
            welcome.account_label = label;
            welcome.subscription = value;
            welcome.cwd = cwd;
            welcome.session_id = session_id;
            welcome.cache.invalidate();
            self.sync_render_cache_slot(0, 0);
            self.recompute_message_retained_bytes(0);
            self.invalidate_layout(InvalidationLevel::MessagesFrom(0));
        }
    }

    /// Track a Task/Agent tool call as active (in-progress subagent).
    pub fn insert_active_task(&mut self, id: String) {
        self.active_task_ids_mut().insert(id);
    }

    /// Remove a Task/Agent tool call from the active set (completed/failed).
    pub fn remove_active_task(&mut self, id: &str) {
        self.active_task_ids_mut().remove(id);
    }

    pub fn register_tool_call_scope(&mut self, id: String, scope: ToolCallScope) {
        self.tool_call_scopes_mut().insert(id, scope);
    }

    pub fn tool_call_scope(&self, id: &str) -> Option<ToolCallScope> {
        self.tool_call_scopes().get(id).cloned()
    }

    pub(crate) fn tracked_terminal_id_for_tool(tc: &ToolCallInfo) -> Option<String> {
        (tc.is_execute_tool()
            && matches!(
                tc.status,
                model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress
            ))
        .then(|| tc.terminal_id.clone())
        .flatten()
    }

    pub fn clear_tool_scope_tracking(&mut self) {
        self.tool_call_scopes_mut().clear();
        self.active_task_ids_mut().clear();
    }

    /// Look up the (`message_index`, `block_index`) for a tool call ID.
    pub fn lookup_tool_call(&self, id: &str) -> Option<(usize, usize)> {
        self.tool_call_index().get(id).copied()
    }

    /// Register a tool call's position in the message/block arrays.
    pub fn index_tool_call(&mut self, id: String, msg_idx: usize, block_idx: usize) {
        self.active_tool_call_index_mut().insert(id, (msg_idx, block_idx));
    }

    pub(crate) fn sync_terminal_tool_call(
        &mut self,
        terminal_id: String,
        msg_idx: usize,
        block_idx: usize,
    ) {
        let desired = TerminalToolCallRef::new(terminal_id, msg_idx, block_idx);
        if self.terminal_tool_call_membership().contains(&desired) {
            return;
        }
        self.untrack_terminal_tool_call(msg_idx, block_idx);
        self.terminal_tool_call_membership_mut().insert(desired.clone());
        self.terminal_tool_calls_mut().push(desired);
    }

    pub(crate) fn untrack_terminal_tool_call(&mut self, msg_idx: usize, block_idx: usize) {
        let removed: Vec<_> = self
            .terminal_tool_calls()
            .iter()
            .filter(|entry| entry.msg_idx == msg_idx && entry.block_idx == block_idx)
            .cloned()
            .collect();
        if removed.is_empty() {
            return;
        }
        self.terminal_tool_calls_mut()
            .retain(|entry| entry.msg_idx != msg_idx || entry.block_idx != block_idx);
        for entry in removed {
            self.terminal_tool_call_membership_mut().remove(&entry);
        }
    }

    pub(crate) fn clear_terminal_tool_call_tracking(&mut self) {
        self.terminal_tool_calls_mut().clear();
        self.terminal_tool_call_membership_mut().clear();
    }

    pub(crate) fn sync_after_message_blocks_changed(&mut self, msg_idx: usize) {
        self.note_render_cache_structure_changed();
        if let Some(message) = self.active_messages_mut().get_mut(msg_idx) {
            message.invalidate_render_cache();
        }
        self.sync_render_cache_message(msg_idx);
        self.recompute_message_retained_bytes(msg_idx);
        self.invalidate_layout(InvalidationLevel::MessageChanged(msg_idx));
    }

    /// Invalidate message layout caches at the given level.
    ///
    /// Single entry point for all layout invalidation. Replaces the former
    /// `mark_message_layout_dirty` / `mark_all_message_layout_dirty` methods.
    pub fn invalidate_layout(&mut self, level: LayoutInvalidation) {
        match level {
            LayoutInvalidation::MessageChanged(idx) => {
                self.active_viewport_mut().invalidate_message(idx);
            }
            LayoutInvalidation::MessagesFrom(idx) => {
                self.active_viewport_mut().invalidate_messages_from(idx);
            }
            LayoutInvalidation::Global => {
                if self.messages().is_empty() {
                    return;
                }
                self.active_viewport_mut().invalidate_all_messages(LayoutRemeasureReason::Global);
                self.active_viewport_mut().bump_layout_generation();
            }
            LayoutInvalidation::Resize => {
                // Resize is handled by viewport.on_frame(). This arm exists
                // for exhaustiveness; production code should not reach it.
                debug_assert!(false, "Resize should not be dispatched through invalidate_layout");
            }
        }
    }

    pub(crate) fn invalidate_message_set<I>(&mut self, indices: I)
    where
        I: IntoIterator<Item = usize>,
    {
        let unique: BTreeSet<_> =
            indices.into_iter().filter(|&idx| idx < self.messages().len()).collect();
        for idx in unique {
            self.active_viewport_mut().invalidate_message(idx);
        }
    }

    /// Enforce history retention and record metrics.
    ///
    /// Wrapper around `enforce_history_retention` that feeds the returned stats
    /// into `CacheMetrics` and emits rate-limited structured tracing. Call this
    /// instead of `enforce_history_retention()` at all non-test call sites.
    pub fn enforce_history_retention_tracked(&mut self) {
        let stats = self.enforce_history_retention();
        let policy = self.history_retention();
        let should_log = self.cache_metrics_mut().record_history_enforcement(&stats, policy);
        if should_log {
            let snap = cache_metrics::build_snapshot(
                &self.render_cache_budget,
                self.history_retention_stats(),
                policy,
                self.cache_metrics(),
                self.viewport(),
                0, // entry_count not needed for history-only log
                0,
                stats.dropped_messages,
                0, // protected_bytes not relevant for history-only log
            );
            cache_metrics::emit_history_metrics(&snap);
        }
    }

    /// Force-finish any lingering in-progress tool calls.
    /// Returns the number of tool calls that were transitioned.
    pub fn finalize_in_progress_tool_calls(&mut self, new_status: model::ToolCallStatus) -> usize {
        let mut changed = 0usize;
        let mut cleared_interaction = false;
        let mut changed_message_indices = Vec::new();
        let mut changed_slots = Vec::new();
        let mut detached_terminal = false;

        for (msg_idx, msg) in self.active_messages_mut().iter_mut().enumerate() {
            for (block_idx, block) in msg.blocks.iter_mut().enumerate() {
                if let MessageBlock::ToolCall(tc) = block {
                    let tc = tc.as_mut();
                    if matches!(
                        tc.status,
                        model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                    ) {
                        tc.status = new_status;
                        tc.mark_tool_call_layout_dirty();
                        changed_slots.push((msg_idx, block_idx));
                        if tc.pending_permission.take().is_some() {
                            cleared_interaction = true;
                        }
                        if tc.pending_question.take().is_some() {
                            cleared_interaction = true;
                        }
                        if tc.is_execute_tool() && tc.terminal_id.take().is_some() {
                            detached_terminal = true;
                        }
                        if changed_message_indices.last().copied() != Some(msg_idx) {
                            changed_message_indices.push(msg_idx);
                        }
                        changed += 1;
                    }
                }
            }
        }

        if detached_terminal {
            self.rebuild_tool_indices_and_terminal_refs();
        }

        for (msg_idx, block_idx) in changed_slots {
            self.sync_render_cache_slot(msg_idx, block_idx);
        }

        for msg_idx in changed_message_indices.iter().copied() {
            self.recompute_message_retained_bytes(msg_idx);
        }

        if changed > 0 || cleared_interaction {
            self.invalidate_message_set(changed_message_indices.iter().copied());
            self.pending_interaction_ids_mut().clear();
            self.release_focus_target(FocusTarget::Permission);
        }

        changed
    }

    /// Clear any inline permission/question UI still attached to tool calls.
    /// Returns the number of tool call blocks that changed.
    pub fn clear_inline_tool_interactions(&mut self) -> usize {
        let mut changed = 0usize;
        let mut changed_message_indices = Vec::new();
        let mut changed_slots = Vec::new();

        for (msg_idx, msg) in self.active_messages_mut().iter_mut().enumerate() {
            for (block_idx, block) in msg.blocks.iter_mut().enumerate() {
                let MessageBlock::ToolCall(tc) = block else {
                    continue;
                };
                let tc = tc.as_mut();
                let mut block_changed = false;
                if tc.pending_permission.take().is_some() {
                    block_changed = true;
                }
                if tc.pending_question.take().is_some() {
                    block_changed = true;
                }
                if !block_changed {
                    continue;
                }
                tc.mark_tool_call_layout_dirty();
                changed_slots.push((msg_idx, block_idx));
                if changed_message_indices.last().copied() != Some(msg_idx) {
                    changed_message_indices.push(msg_idx);
                }
                changed += 1;
            }
        }

        for (msg_idx, block_idx) in changed_slots {
            self.sync_render_cache_slot(msg_idx, block_idx);
        }

        for msg_idx in changed_message_indices.iter().copied() {
            self.recompute_message_retained_bytes(msg_idx);
        }

        if changed > 0 {
            self.invalidate_message_set(changed_message_indices.iter().copied());
        }

        if changed > 0 || !self.pending_interaction_ids().is_empty() {
            self.pending_interaction_ids_mut().clear();
            self.release_focus_target(FocusTarget::Permission);
        }

        changed
    }

    /// Clear runtime-only turn tracking while preserving the message history itself.
    pub fn finalize_turn_runtime_artifacts(&mut self, new_status: model::ToolCallStatus) {
        let _ = self.finalize_in_progress_tool_calls(new_status);
        let _ = self.clear_inline_tool_interactions();
        self.clear_tool_scope_tracking();
    }

    /// Build a minimal `App` for unit/integration tests.
    /// All fields get sensible defaults; the `mpsc` channel is wired up internally.
    ///
    /// Wires a `Workspace::testing_stub()` so any code path that
    /// reaches `Workspace::dispatch` / `Workspace::refresh_*` finds
    /// a registered `forge_workspace::DomainSession` keyed by the
    /// `__conn_pending__` synthetic. The underlying `AgentHandle`
    /// is the `Agent::testing_stub` no-op bridge; commands sent
    /// through it are silently dropped. Behind the `testing` Cargo
    /// feature so production builds don't pull in the stub helpers.
    #[cfg(feature = "testing")]
    pub fn test_default() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<forge_workspace::SessionUpdate>();
        let (file_index_tx, file_index_rx) = std_mpsc::channel();
        let (git_diff_tx, git_diff_rx) = std_mpsc::channel();
        let (process_scan_tx, process_scan_rx) = std_mpsc::channel();
        let (cli_version_tx, cli_version_rx) = std_mpsc::channel();
        let (diff_overlay_tx, diff_overlay_rx) = std_mpsc::channel();
        let pending_key = forge_workspace::SessionKey::from_session_id(Self::PRE_CONNECT_KEY);
        let mut pending_session = super::session::UiSession::new(pending_key.clone());
        // Seed a synthetic `current_model` so tests that depend on
        // model-resolution UI paths see a stable value.
        pending_session.current_model = Some(
            model::CurrentModel::new("test-model", "test-model", "test-model").authoritative(true),
        );
        // Seed display-friendly + raw cwd on the bucket. Both fields
        // live on `UiSession`; workspace's `DomainSession` holds only
        // routing metadata (AgentHandle, session_id).
        pending_session.cwd = "/test".into();
        pending_session.cwd_raw = "/test".into();
        let mut sessions = std::collections::HashMap::new();
        sessions.insert(pending_key.clone(), pending_session);

        // Build a Workspace stub and register a DomainSession for the
        // pre-Connect key. The DomainSession carries the routing
        // metadata (handle slot, session_id, pending interactions);
        // tests that exercise "post-Connect" flows install a stub
        // handle via `App::install_testing_stub`, which writes onto
        // this same DomainSession's `conn` slot. Tests that target
        // the pre-Connect state observe `has_active_agent() == false`
        // until they do.
        let (workspace, _update_rx) = forge_workspace::Workspace::testing_stub();
        workspace.register_domain_session(pending_key.clone(), None);

        Self {
            active_view: ActiveView::Chat,
            config: ConfigState::default(),
            settings_home_override: None,
            status: AppStatus::Ready,
            should_quit: false,
            exit_error: None,
            workspace: Some(workspace),
            #[rustfmt::skip] #[cfg(feature = "testing")] test_dispatched_permission_outcomes: std::cell::RefCell::new(Vec::new()),
            #[rustfmt::skip] #[cfg(feature = "testing")] test_dispatched_question_outcomes: std::cell::RefCell::new(Vec::new()),
            sessions,
            active_session_key: Some(pending_key),
            help_view: HelpView::Keys,
            help_open: false,
            help_dialog: dialog::DialogState::default(),
            help_visible_count: 0,
            update_rx: rx,
            update_tx: tx,
            file_index_event_tx: file_index_tx,
            file_index_event_rx: file_index_rx,
            git_diff_event_tx: git_diff_tx,
            git_diff_event_rx: git_diff_rx,
            process_scan_event_tx: process_scan_tx,
            process_scan_event_rx: process_scan_rx,
            cli_version_event_tx: cli_version_tx,
            cli_version_event_rx: cli_version_rx,
            diff_overlay_event_tx: diff_overlay_tx,
            diff_overlay_event_rx: diff_overlay_rx,
            diff_scan_seq: 0,
            cli_version_info: None,
            spinner_frame: 0,
            spinner_last_advance_at: None,
            tools_collapsed: false,
            projects_pane_visible: true,
            projects_pane_overlay_open: false,
            inspector_pane_visible: true,
            inspector_pane_overlay_open: false,
            pane_hit_targets: Vec::new(),
            layout: crate::ui::layout::AppLayout::default(),
            force_redraw: false,
            focus: FocusManager::default(),
            plugins: PluginsState::default(),
            session_picker: SessionPickerState::default(),
            launchpad: crate::app::LaunchpadState::default(),
            diff_overlay: None,
            cached_frame_area: ratatui::layout::Rect::default(),
            scrollbar_drag: None,
            rendered_chat_lines: Vec::new(),
            rendered_chat_area: ratatui::layout::Rect::default(),
            rendered_input_lines: Vec::new(),
            rendered_input_area: ratatui::layout::Rect::default(),
            rendered_inspector_body_area: ratatui::layout::Rect::default(),
            paste_burst: super::paste_burst::PasteBurstDetector::new(),
            needs_redraw: true,
            notifications: super::notify::NotificationManager::new(),
            perf: None,
            render_cache_budget: RenderCacheBudget::default(),
            fps_ema: None,
            last_frame_at: None,
            connection_started: false,
            startup_resume_id: None,
            startup_resume_requested: false,
            startup_session_picker_requested: false,
            startup_recent_sessions_loaded: false,
            startup_session_picker_resolved: false,
            startup_project: None,
            replay_in_progress: false,
        }
    }

    /// Resolve the effective focus owner for Up/Down and other directional keys.
    pub fn focus_owner(&self) -> FocusOwner {
        self.focus.owner(self.focus_context())
    }

    pub fn active_turn_assistant_idx(&self) -> Option<usize> {
        self.active_turn_assistant_message_idx().filter(|&idx| {
            self.messages().get(idx).is_some_and(|msg| matches!(msg.role, MessageRole::Assistant))
        })
    }

    pub fn bind_active_turn_assistant(&mut self, idx: usize) {
        let next = self
            .messages()
            .get(idx)
            .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant))
            .then_some(idx);
        self.set_active_turn_assistant_message_idx(next);
    }

    pub fn bind_active_turn_assistant_to_tail(&mut self) {
        if let Some(idx) = self.messages().len().checked_sub(1) {
            self.bind_active_turn_assistant(idx);
        } else {
            self.clear_active_turn_assistant();
        }
    }

    pub fn clear_active_turn_assistant(&mut self) {
        self.set_active_turn_assistant_message_idx(None);
    }

    pub(crate) fn clear_turn_notice_refs(&mut self) {
        self.turn_notice_refs_mut().clear();
    }

    pub(crate) fn shift_turn_notice_refs_for_insert(&mut self, idx: usize) {
        for notice_ref in self.turn_notice_refs_mut() {
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

    pub(crate) fn shift_turn_notice_refs_for_remove(&mut self, idx: usize) {
        self.turn_notice_refs_mut().retain_mut(|notice_ref| match &mut notice_ref.location {
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

    pub(crate) fn remap_turn_notice_refs_after_message_drop(
        &mut self,
        old_to_new: &[Option<usize>],
    ) {
        self.turn_notice_refs_mut().retain_mut(|notice_ref| match &mut notice_ref.location {
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

    pub fn clear_session_runtime_identity(&mut self) {
        self.set_session_id(None);
        self.set_current_model(None);
        self.set_mode(None);
        self.set_fast_mode_state(model::FastModeState::Off);
        *self.session_usage_mut() = SessionUsageState::default();
    }

    pub(crate) fn shift_active_turn_assistant_for_insert(&mut self, idx: usize) {
        if let Some(owner_idx) = self.active_turn_assistant_message_idx()
            && idx <= owner_idx
        {
            self.set_active_turn_assistant_message_idx(Some(owner_idx.saturating_add(1)));
        }
    }

    pub(crate) fn shift_active_turn_assistant_for_remove(&mut self, idx: usize) {
        let Some(owner_idx) = self.active_turn_assistant_message_idx() else {
            return;
        };
        let next = match idx.cmp(&owner_idx) {
            std::cmp::Ordering::Less => Some(owner_idx.saturating_sub(1)),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(owner_idx),
        };
        self.set_active_turn_assistant_message_idx(next);
    }

    pub fn active_autocomplete_kind(&self) -> Option<AutocompleteKind> {
        if self.mention().is_some() {
            Some(AutocompleteKind::Mention)
        } else if self.slash().is_some() {
            Some(AutocompleteKind::Slash)
        } else if self.subagent().is_some() {
            Some(AutocompleteKind::Subagent)
        } else {
            None
        }
    }

    pub fn is_help_active(&self) -> bool {
        self.help_open
    }

    pub fn sync_help_open_with_input(&mut self) {
        if self.help_open && self.input().text().trim() != "?" {
            self.help_open = false;
            self.release_focus_target(FocusTarget::Help);
        }
    }

    pub fn autocomplete_focus_available(&self) -> bool {
        self.mention().is_some_and(mention::MentionState::has_selectable_candidates)
            || self.slash().is_some()
            || self.subagent().is_some()
    }

    pub fn has_draft_input_for_focus(&self) -> bool {
        !self.input().is_empty()
    }

    pub fn rebuild_chat_focus_from_state(&mut self) {
        if self.active_view != ActiveView::Chat {
            return;
        }

        self.normalize_focus_stack();

        if self.pending_interaction_ids().is_empty() {
            clear_inline_interaction_focus(self);
        } else if self.focus_owner() == FocusOwner::Permission || !self.has_draft_input_for_focus()
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
            && self.pending_interaction_ids().is_empty()
            && !self.autocomplete_focus_available()
        {
            self.claim_focus_target(FocusTarget::Help);
        } else {
            self.release_focus_target(FocusTarget::Help);
        }

        self.normalize_focus_stack();
    }

    /// Claim key routing for a navigation target.
    /// The latest claimant wins.
    pub fn claim_focus_target(&mut self, target: FocusTarget) {
        let context = self.focus_context();
        self.focus.claim(target, context);
    }

    /// Release key routing claim for a navigation target.
    pub fn release_focus_target(&mut self, target: FocusTarget) {
        let context = self.focus_context();
        self.focus.release(target, context);
    }

    /// Drop claims that are no longer valid for current state.
    pub fn normalize_focus_stack(&mut self) {
        let context = self.focus_context();
        self.focus.normalize(context);
    }

    fn focus_context(&self) -> FocusContext {
        let mut ctx = FocusContext::empty();
        // TodoList focus is intentionally never enabled — the bottom
        // todo panel was replaced by the read-only Inspector pane.
        if self.autocomplete_focus_available() {
            ctx = ctx.with(FocusTarget::Mention);
        }
        if !self.pending_interaction_ids().is_empty() {
            ctx = ctx.with(FocusTarget::Permission);
        }
        if self.is_help_active() {
            ctx = ctx.with(FocusTarget::Help);
        }
        ctx
    }
}

#[cfg(test)]
mod tests {
    // =====
    // TESTS: 28
    // =====

    use super::*;
    use crate::app::dialog;
    use crate::app::slash::{SlashCandidate, SlashContext, SlashState};
    use pretty_assertions::assert_eq;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    #[test]
    fn test_default_seeds_pre_connect_bucket_so_accessors_are_infallible() {
        let app = App::test_default();
        // Task 3 onwards: per-session field accessors (messages, viewport,
        // …) need an active session to read/write. test_default seeds a
        // synthetic pre-Connect bucket so call sites stay infallible
        // before Connect lands.
        assert_eq!(app.sessions.len(), 1);
        let pre_connect_key = forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY);
        assert_eq!(app.active_session_key.as_ref(), Some(&pre_connect_key));
        assert!(app.active_session().is_some());
    }

    #[test]
    fn inserting_a_session_makes_it_active_via_accessors() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_str_for_test("abc-123");
        app.sessions
            .entry(key.clone())
            .or_insert_with(|| crate::app::session::UiSession::new(key.clone()));
        app.active_session_key = Some(key.clone());

        assert_eq!(app.active_session_key.as_ref(), Some(&key));
        assert!(app.active_session().is_some());
        assert_eq!(app.active_session().and_then(|s| s.key.as_ref()), Some(&key));
        assert!(app.try_active_bucket_mut().is_some());
        assert!(app.session_mut(&key).is_some());
    }

    /// Clicking a launchpad-auto_started project triggers the
    /// per-session refresh chain (status / oauth / context-usage /
    /// 5h+7d) so the bottom panel's bars populate on the destination
    /// session, not just on connect.
    ///
    /// `request_context_usage_refresh` flips
    /// `session_usage.context_usage_in_flight = true` when it
    /// successfully proceeds (it needs workspace + active key +
    /// session_id, all of which the destination bucket has post-switch).
    /// Observing the flag flip on the destination bucket proves
    /// `switch_active_session` invoked the refresh chain.
    #[test]
    fn switch_active_session_triggers_context_usage_refresh_on_destination() {
        let mut app = App::test_default();
        let _pre_connect_outbox = app.install_testing_stub();
        // Seed a second bucket and stamp it with a session_id so the
        // refresh fns clear their session_id gate after the switch.
        let dest_key = forge_workspace::SessionKey::from_str_for_test("destination-session");
        let mut dest_bucket = crate::app::session::UiSession::new(dest_key.clone());
        dest_bucket.session_id =
            Some(forge_primitives::SessionId::new(dest_key.as_str().to_owned()));
        app.sessions.insert(dest_key.clone(), dest_bucket);
        // Hold the destination's command receiver alive at test scope —
        // dropping it before `switch_active_session` runs makes the
        // workspace's stub-handle send fail, which routes through the
        // error arm in `request_context_usage_refresh` and resets the
        // in_flight flag we're trying to observe.
        let _dest_outbox = if let Some(workspace) = app.workspace.as_ref() {
            let (handle, outbox) = forge_workspace::Workspace::testing_stub_handle();
            let domain = workspace
                .register_domain_session(dest_key.clone(), Some(std::sync::Arc::new(handle)));
            domain.lock().session_id =
                Some(forge_primitives::SessionId::new(dest_key.as_str().to_owned()));
            Some(outbox)
        } else {
            None
        };
        // Sanity baseline: destination bucket's context-usage is idle.
        assert!(
            !app.sessions
                .get(&dest_key)
                .expect("dest bucket")
                .session_usage
                .context_usage_in_flight,
            "destination bucket should start with context_usage idle",
        );

        app.switch_active_session(dest_key.clone());

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&dest_key),
            "switch must promote destination to active",
        );
        assert!(
            app.sessions.get(&dest_key).expect("dest bucket").session_usage.context_usage_in_flight,
            "switch_active_session must call request_context_usage_refresh on the new active \
             (otherwise the launchpad-click bottom-panel bars sit empty)",
        );
    }

    /// Regression: the pre-connect bucket's `cwd_raw` must not be
    /// seeded from `std::env::current_dir()` — forge.toml is the
    /// source of truth (Hard Rule #15). In launchpad mode (no argv
    /// project), the pre-connect bucket's `cwd_raw` stays empty so
    /// it cannot collide with any project lookup. This test pins
    /// that invariant for `test_default`'s pre-connect bucket.
    #[test]
    fn test_default_pre_connect_bucket_does_not_collide_with_project_paths() {
        let app = App::test_default();
        let pre_connect_key = forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY);
        let pre_bucket = app.sessions.get(&pre_connect_key).expect("pre-connect bucket");
        // `test_default` seeds `/test` for stable rendering; production
        // launchpad-mode pre-connect uses an empty `cwd_raw`. Either
        // way, the invariant the production fix relies on is that no
        // real project's `path` ever ends up matching the pre-connect
        // bucket's `cwd_raw` — there is no way to construct a forge
        // project named `/test` and pre-connect cannot equal a real
        // project's `path` accidentally because nothing reads from
        // `current_dir()` to seed it anymore.
        assert!(
            pre_bucket.cwd_raw == "/test" || pre_bucket.cwd_raw.is_empty(),
            "pre-connect bucket should hold a sentinel cwd, got {:?}",
            pre_bucket.cwd_raw,
        );
    }

    /// `find_running_bucket_for_path` returns the unique bucket
    /// matching `path` when one exists. The pre-connect bucket
    /// never participates because its `cwd_raw` is sourced from
    /// `forge.toml`-or-empty, not from `current_dir()` — so it
    /// cannot accidentally match a real project's `path`.
    #[test]
    fn find_running_bucket_for_path_returns_matching_real_bucket() {
        let mut app = App::test_default();
        let project_path = "/Users/vedhavyas/Projects/forge";
        let real_key =
            forge_workspace::SessionKey::from_str_for_test("11111111-2222-3333-4444-555555555555");
        let mut real_bucket = crate::app::session::UiSession::new(real_key.clone());
        real_bucket.cwd_raw = project_path.to_owned();
        app.sessions.insert(real_key.clone(), real_bucket);

        let picked = app.find_running_bucket_for_path(project_path).expect("a bucket should match");
        assert_eq!(picked, real_key);
    }

    /// No bucket matches → `None`. Used by the click handler to
    /// fall through to the catalog / cold-spawn paths.
    #[test]
    fn find_running_bucket_for_path_returns_none_when_no_match() {
        let app = App::test_default();
        assert!(app.find_running_bucket_for_path("/Users/vedhavyas/Projects/forge").is_none());
    }

    // BlockCache

    #[test]
    fn cache_lifecycle_covers_default_store_invalidate_and_restore() {
        let mut cache = BlockCache::default();
        assert!(cache.get().is_none());

        cache.store(vec![Line::from("old")]);
        assert_eq!(cache.get().unwrap().len(), 1);

        cache.invalidate();
        cache.invalidate();
        cache.invalidate();
        assert!(cache.get().is_none());

        cache.store(vec![Line::from("new")]);
        let lines = cache.get().unwrap();
        assert_eq!(lines.len(), 1);
        let span_content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(span_content, "new");
    }

    #[test]
    fn cache_store_empty_lines() {
        let mut cache = BlockCache::default();
        cache.store(Vec::new());
        let lines = cache.get().unwrap();
        assert!(lines.is_empty());
    }

    /// Store twice without invalidating - second store overwrites first.
    #[test]
    fn cache_store_overwrite_without_invalidate() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("first")]);
        cache.store(vec![Line::from("second"), Line::from("line2")]);
        let lines = cache.get().unwrap();
        assert_eq!(lines.len(), 2);
        let content: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(content, "second");
    }

    /// `get()` called twice returns consistent data.
    #[test]
    fn cache_get_twice_consistent() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("stable")]);
        let first = cache.get().unwrap().len();
        let second = cache.get().unwrap().len();
        assert_eq!(first, second);
    }

    // BlockCache

    #[test]
    fn cache_store_many_lines() {
        let mut cache = BlockCache::default();
        let lines: Vec<Line<'static>> =
            (0..1000).map(|i| Line::from(Span::raw(format!("line {i}")))).collect();
        cache.store(lines);
        assert_eq!(cache.get().unwrap().len(), 1000);
    }

    #[test]
    fn cache_store_splits_into_kb_segments() {
        let mut cache = BlockCache::default();
        let long = "x".repeat(800);
        let lines: Vec<Line<'static>> = (0..12).map(|_| Line::from(long.clone())).collect();
        cache.store(lines);
        assert!(cache.segment_count() > 1);
        assert!(cache.cached_bytes() > 0);
    }

    #[test]
    fn cache_invalidate_without_store() {
        let mut cache = BlockCache::default();
        cache.invalidate();
        assert!(cache.get().is_none());
    }

    #[test]
    fn cache_rapid_store_invalidate_cycle() {
        let mut cache = BlockCache::default();
        for i in 0..50 {
            cache.store(vec![Line::from(format!("v{i}"))]);
            assert!(cache.get().is_some());
            cache.invalidate();
            assert!(cache.get().is_none());
        }
        cache.store(vec![Line::from("final")]);
        assert!(cache.get().is_some());
    }

    /// Store styled lines with multiple spans per line.
    #[test]
    fn cache_store_styled_lines() {
        let mut cache = BlockCache::default();
        let line = Line::from(vec![
            Span::styled("bold", Style::default().fg(Color::Red)),
            Span::raw(" normal "),
            Span::styled("blue", Style::default().fg(Color::Blue)),
        ]);
        cache.store(vec![line]);
        let lines = cache.get().unwrap();
        assert_eq!(lines[0].spans.len(), 3);
    }

    /// Version counter after many invalidations - verify it doesn't
    /// accidentally wrap to 0 (which would make stale data appear fresh).
    /// With u64, 10K invalidations is nowhere near overflow.
    #[test]
    fn cache_version_no_false_fresh_after_many_invalidations() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("data")]);
        for _ in 0..10_000 {
            cache.invalidate();
        }
        // Cache was invalidated 10K times without re-storing - must be stale
        assert!(cache.get().is_none());
    }

    /// Invalidate, store, invalidate, store - alternating pattern.
    #[test]
    fn cache_alternating_invalidate_store() {
        let mut cache = BlockCache::default();
        for i in 0..100 {
            cache.invalidate();
            assert!(cache.get().is_none(), "stale after invalidate at iter {i}");
            cache.store(vec![Line::from(format!("v{i}"))]);
            assert!(cache.get().is_some(), "fresh after store at iter {i}");
        }
    }

    // BlockCache height

    #[test]
    fn cache_height_default_returns_none() {
        let cache = BlockCache::default();
        assert!(cache.height_at(80).is_none());
    }

    #[test]
    fn cache_store_with_height_then_height_at() {
        let mut cache = BlockCache::default();
        cache.store_with_height(vec![Line::from("hello")], 1, 80);
        assert_eq!(cache.height_at(80), Some(1));
        assert!(cache.get().is_some());
    }

    #[test]
    fn cache_height_at_wrong_width_returns_none() {
        let mut cache = BlockCache::default();
        cache.store_with_height(vec![Line::from("hello")], 1, 80);
        assert!(cache.height_at(120).is_none());
    }

    #[test]
    fn cache_height_invalidated_returns_none() {
        let mut cache = BlockCache::default();
        cache.store_with_height(vec![Line::from("hello")], 1, 80);
        cache.invalidate();
        assert!(cache.height_at(80).is_none());
    }

    #[test]
    fn clear_session_runtime_identity_resets_session_usage() {
        let mut app = App::test_default();
        app.set_session_id(Some(crate::agent::model::SessionId::new("session-1")));
        app.set_current_model(Some(
            crate::agent::model::CurrentModel::new("sonnet", "Claude Sonnet", "Claude Sonnet")
                .authoritative(true),
        ));
        app.set_mode(Some(crate::app::ModeState {
            current_mode_id: "plan".to_owned(),
            current_mode_name: "Plan".to_owned(),
            available_modes: Vec::new(),
        }));
        let usage = app.session_usage_mut();
        usage.context_usage_percent = Some(62);
        usage.context_usage_in_flight = true;
        usage.context_usage_refresh_pending = true;
        usage.last_compaction_pre_tokens = Some(123_456);

        app.clear_session_runtime_identity();

        assert!(app.session_id().is_none());
        assert!(app.current_model().is_none());
        assert!(app.mode().is_none());
        assert_eq!(*app.session_usage(), SessionUsageState::default());
    }

    #[test]
    fn cache_store_without_height_has_no_height() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello")]);
        // store() without height leaves wrapped_width at 0
        assert!(cache.height_at(80).is_none());
    }

    #[test]
    fn cache_store_with_height_overwrite() {
        let mut cache = BlockCache::default();
        cache.store_with_height(vec![Line::from("old")], 1, 80);
        cache.invalidate();
        cache.store_with_height(vec![Line::from("new long line")], 3, 120);
        assert_eq!(cache.height_at(120), Some(3));
        assert!(cache.height_at(80).is_none());
    }

    // BlockCache set_height (separate from store)

    #[test]
    fn cache_set_height_after_store() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello")]);
        assert!(cache.height_at(80).is_none()); // no height yet
        cache.set_height(1, 80);
        assert_eq!(cache.height_at(80), Some(1));
        assert!(cache.get().is_some()); // lines still valid
    }

    #[test]
    fn cache_set_height_update_width() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("hello world")]);
        cache.set_height(1, 80);
        assert_eq!(cache.height_at(80), Some(1));
        // Re-measure at new width
        cache.set_height(2, 40);
        assert_eq!(cache.height_at(40), Some(2));
        assert!(cache.height_at(80).is_none()); // old width no longer valid
    }

    #[test]
    fn cache_set_height_invalidate_clears_height() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("data")]);
        cache.set_height(3, 80);
        cache.invalidate();
        assert!(cache.height_at(80).is_none()); // version mismatch
    }

    #[test]
    fn cache_set_height_on_invalidated_cache_returns_none() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("data")]);
        cache.invalidate(); // version != 0
        cache.set_height(5, 80);
        // height_at returns None because cache is stale (version != 0)
        assert!(cache.height_at(80).is_none());
    }

    #[test]
    fn cache_store_then_set_height_matches_store_with_height() {
        let mut cache_a = BlockCache::default();
        cache_a.store(vec![Line::from("test")]);
        cache_a.set_height(2, 100);

        let mut cache_b = BlockCache::default();
        cache_b.store_with_height(vec![Line::from("test")], 2, 100);

        assert_eq!(cache_a.height_at(100), cache_b.height_at(100));
        assert_eq!(cache_a.get().unwrap().len(), cache_b.get().unwrap().len());
    }

    #[test]
    fn cache_measure_and_set_height_from_segments() {
        let mut cache = BlockCache::default();
        let lines = vec![
            Line::from("alpha beta gamma delta epsilon"),
            Line::from("zeta eta theta iota kappa lambda"),
            Line::from("mu nu xi omicron pi rho sigma"),
        ];
        cache.store(lines.clone());
        let measured = cache.measure_and_set_height(16).expect("expected measured height");
        let expected = ratatui::widgets::Paragraph::new(ratatui::text::Text::from(lines))
            .wrap(ratatui::widgets::Wrap { trim: false })
            .line_count(16);
        assert_eq!(measured, expected);
        assert_eq!(cache.height_at(16), Some(expected));
    }

    #[test]
    fn cache_get_updates_last_access_tick() {
        let mut cache = BlockCache::default();
        cache.store(vec![Line::from("tick")]);
        let before = cache.last_access_tick();
        let _ = cache.get();
        let after = cache.last_access_tick();
        assert!(after > before);
    }

    // App tool_call_index

    fn make_test_app() -> App {
        App::test_default()
    }

    fn assistant_text_block(text: &str) -> MessageBlock {
        MessageBlock::Text(TextBlock::from_complete(text))
    }

    fn user_text_message(text: &str) -> ChatMessage {
        ChatMessage::new(MessageRole::User, vec![assistant_text_block(text)], None)
    }

    fn assistant_tool_message(id: &str, status: model::ToolCallStatus) -> ChatMessage {
        ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(ToolCallInfo {
                id: id.to_owned(),
                title: format!("tool {id}"),
                sdk_tool_name: "Read".to_owned(),
                raw_input: None,
                raw_input_bytes: 0,
                output_metadata: None,
                task_metadata: None,
                status,
                content: Vec::new(),
                hidden: false,
                terminal_id: None,
                terminal_command: None,
                terminal_output: Some("x".repeat(1024)),
                terminal_output_len: 1024,
                terminal_bytes_seen: 1024,
                terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
                render_epoch: 0,
                layout_epoch: 0,
                last_measured_width: 0,
                last_measured_height: 0,
                last_measured_layout_epoch: 0,
                last_measured_layout_generation: 0,
                cache: BlockCache::default(),
                pending_permission: None,
                pending_question: None,
                collapsed_override: None,
                last_measured_y_in_msg: 0,
            }))],
            None,
        )
    }

    fn assistant_bash_tool_message(
        id: &str,
        status: model::ToolCallStatus,
        terminal_id: &str,
    ) -> ChatMessage {
        ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(ToolCallInfo {
                id: id.to_owned(),
                title: format!("tool {id}"),
                sdk_tool_name: "Bash".to_owned(),
                raw_input: None,
                raw_input_bytes: 0,
                output_metadata: None,
                task_metadata: None,
                status,
                content: Vec::new(),
                hidden: false,
                terminal_id: Some(terminal_id.to_owned()),
                terminal_command: Some("echo hi".to_owned()),
                terminal_output: Some("x".repeat(1024)),
                terminal_output_len: 1024,
                terminal_bytes_seen: 1024,
                terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
                render_epoch: 0,
                layout_epoch: 0,
                last_measured_width: 0,
                last_measured_height: 0,
                last_measured_layout_epoch: 0,
                last_measured_layout_generation: 0,
                cache: BlockCache::default(),
                pending_permission: None,
                pending_question: None,
                collapsed_override: None,
                last_measured_y_in_msg: 0,
            }))],
            None,
        )
    }

    fn assistant_tool_message_with_pending_permission(id: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(ToolCallInfo {
                id: id.to_owned(),
                title: format!("tool {id}"),
                sdk_tool_name: "Read".to_owned(),
                raw_input: None,
                raw_input_bytes: 0,
                output_metadata: None,
                task_metadata: None,
                status: model::ToolCallStatus::Completed,
                content: Vec::new(),
                hidden: false,
                terminal_id: None,
                terminal_command: None,
                terminal_output: Some("x".repeat(1024)),
                terminal_output_len: 1024,
                terminal_bytes_seen: 1024,
                terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
                render_epoch: 0,
                layout_epoch: 0,
                last_measured_width: 0,
                last_measured_height: 0,
                last_measured_layout_epoch: 0,
                last_measured_layout_generation: 0,
                cache: BlockCache::default(),
                pending_permission: Some(InlinePermission {
                    options: vec![model::PermissionOption::new(
                        "allow-once",
                        "Allow once",
                        model::PermissionOptionKind::AllowOnce,
                    )],
                    display: None,
                    tool_id: id.to_owned(),
                    selected_index: 0,
                    focused: false,
                }),
                pending_question: None,
                collapsed_override: None,
                last_measured_y_in_msg: 0,
            }))],
            None,
        )
    }

    #[test]
    fn enforce_render_cache_budget_evicts_lru_block() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("a")], None),
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("b")], None),
        ];

        let bytes_a = if let MessageBlock::Text(block) = &mut app.active_messages_mut()[0].blocks[0]
        {
            block.cache.store(vec![Line::from("x".repeat(2200))]);
            block.cache.cached_bytes()
        } else {
            0
        };
        let bytes_b = if let MessageBlock::Text(block) = &mut app.active_messages_mut()[1].blocks[0]
        {
            block.cache.store(vec![Line::from("y".repeat(2200))]);
            let _ = block.cache.get();
            block.cache.cached_bytes()
        } else {
            0
        };

        app.render_cache_budget.max_bytes = bytes_b;
        let stats = app.enforce_render_cache_budget();
        assert!(stats.evicted_blocks >= 1);
        assert!(stats.evicted_bytes >= bytes_a);
        assert!(stats.total_after_bytes <= app.render_cache_budget.max_bytes);
        assert_eq!(stats.protected_bytes, 0);

        if let MessageBlock::Text(block) = &app.messages()[0].blocks[0] {
            assert_eq!(block.cache.cached_bytes(), 0);
        } else {
            panic!("expected text block");
        }
        if let MessageBlock::Text(block) = &app.messages()[1].blocks[0] {
            assert_eq!(block.cache.cached_bytes(), bytes_b);
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn enforce_render_cache_budget_protects_streaming_tail_message() {
        let mut app = make_test_app();
        app.status = AppStatus::Thinking;
        *app.active_messages_mut() = vec![ChatMessage::new(
            MessageRole::Assistant,
            vec![assistant_text_block("streaming tail")],
            None,
        )];

        let before = if let MessageBlock::Text(block) = &mut app.active_messages_mut()[0].blocks[0]
        {
            block.cache.store(vec![Line::from("z".repeat(4096))]);
            block.cache.cached_bytes()
        } else {
            0
        };
        app.render_cache_budget.max_bytes = 64;
        let stats = app.enforce_render_cache_budget();
        assert_eq!(stats.evicted_blocks, 0);
        assert_eq!(stats.evicted_bytes, 0);
        assert_eq!(stats.protected_bytes, before);

        if let MessageBlock::Text(block) = &app.messages()[0].blocks[0] {
            assert_eq!(block.cache.cached_bytes(), before);
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn enforce_render_cache_budget_excludes_protected_from_budget() {
        let mut app = make_test_app();
        app.status = AppStatus::Running;
        *app.active_messages_mut() = vec![
            ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block("old message")],
                None,
            ),
            ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block("streaming tail")],
                None,
            ),
        ];

        let bytes_a = if let MessageBlock::Text(block) = &mut app.active_messages_mut()[0].blocks[0]
        {
            block.cache.store(vec![Line::from("x".repeat(2200))]);
            block.cache.cached_bytes()
        } else {
            0
        };
        let bytes_b = if let MessageBlock::Text(block) = &mut app.active_messages_mut()[1].blocks[0]
        {
            block.cache.store(vec![Line::from("y".repeat(5000))]);
            block.cache.cached_bytes()
        } else {
            0
        };

        // Budget fits old message alone but not old + tail combined.
        app.render_cache_budget.max_bytes = bytes_a + 100;
        assert!(bytes_a + bytes_b > app.render_cache_budget.max_bytes);

        let stats = app.enforce_render_cache_budget();

        // Protected bytes should be the streaming tail.
        assert_eq!(stats.protected_bytes, bytes_b);
        // No eviction: budgeted bytes (bytes_a) are under max_bytes.
        assert_eq!(stats.evicted_blocks, 0);
        assert_eq!(stats.evicted_bytes, 0);
        // Old message cache intact.
        if let MessageBlock::Text(block) = &app.messages()[0].blocks[0] {
            assert_eq!(block.cache.cached_bytes(), bytes_a);
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn enforce_render_cache_budget_protects_active_streaming_owner_not_physical_tail() {
        let mut app = make_test_app();
        app.status = AppStatus::Running;
        *app.active_messages_mut() = vec![
            ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block("old message")],
                None,
            ),
            ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block("active streaming owner")],
                None,
            ),
            ChatMessage::new(
                MessageRole::System(Some(SystemSeverity::Info)),
                vec![assistant_text_block("late trailing system row")],
                None,
            ),
        ];
        app.bind_active_turn_assistant(1);

        if let MessageBlock::Text(block) = &mut app.active_messages_mut()[0].blocks[0] {
            block.cache.store(vec![Line::from("x".repeat(2000))]);
        }
        let protected_bytes =
            if let MessageBlock::Text(block) = &mut app.active_messages_mut()[1].blocks[0] {
                block.cache.store(vec![Line::from("y".repeat(4000))]);
                block.cache.cached_bytes()
            } else {
                0
            };
        if let MessageBlock::Text(block) = &mut app.active_messages_mut()[2].blocks[0] {
            block.cache.store(vec![Line::from("z".repeat(5000))]);
        }

        app.render_cache_budget.max_bytes = 64;
        let stats = app.enforce_render_cache_budget();

        assert_eq!(stats.protected_bytes, protected_bytes);
    }

    #[test]
    fn enforce_render_cache_budget_evicts_when_budgeted_over_limit() {
        let mut app = make_test_app();
        app.status = AppStatus::Running;
        *app.active_messages_mut() = vec![
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("old-a")], None),
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("old-b")], None),
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("streaming")], None),
        ];

        // Populate caches: messages 0 and 1 evictable, message 2 protected.
        if let MessageBlock::Text(block) = &mut app.active_messages_mut()[0].blocks[0] {
            block.cache.store(vec![Line::from("x".repeat(3000))]);
        }
        let bytes_b = if let MessageBlock::Text(block) = &mut app.active_messages_mut()[1].blocks[0]
        {
            block.cache.store(vec![Line::from("y".repeat(3000))]);
            let _ = block.cache.get(); // touch to make more recently accessed
            block.cache.cached_bytes()
        } else {
            0
        };
        let bytes_c = if let MessageBlock::Text(block) = &mut app.active_messages_mut()[2].blocks[0]
        {
            block.cache.store(vec![Line::from("z".repeat(5000))]);
            block.cache.cached_bytes()
        } else {
            0
        };

        // Budget fits message B but not A+B (excludes C as protected).
        app.render_cache_budget.max_bytes = bytes_b + 100;

        let stats = app.enforce_render_cache_budget();

        assert_eq!(stats.protected_bytes, bytes_c);
        assert!(stats.evicted_blocks >= 1); // message A evicted (older access)
        // Message B should survive (more recent access).
        if let MessageBlock::Text(block) = &app.messages()[1].blocks[0] {
            assert_eq!(block.cache.cached_bytes(), bytes_b);
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn enforce_render_cache_budget_protected_bytes_zero_when_not_streaming() {
        let mut app = make_test_app();
        app.status = AppStatus::Ready;
        *app.active_messages_mut() = vec![ChatMessage::new(
            MessageRole::Assistant,
            vec![assistant_text_block("done")],
            None,
        )];

        if let MessageBlock::Text(block) = &mut app.active_messages_mut()[0].blocks[0] {
            block.cache.store(vec![Line::from("x".repeat(2000))]);
        }
        app.render_cache_budget.max_bytes = usize::MAX;

        let stats = app.enforce_render_cache_budget();
        assert_eq!(stats.protected_bytes, 0);
    }

    #[test]
    fn enforce_render_cache_budget_accounts_for_message_render_cache() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block(&"a".repeat(4000))],
                None,
            ),
            ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block(&"b".repeat(4000))],
                None,
            ),
        ];

        let spinner = crate::ui::SpinnerState {
            frame: 0,
            is_active_turn_assistant: false,
            show_empty_thinking: false,
            show_thinking: false,
            show_compacting: false,
        };

        let _ = crate::ui::measure_message_height_cached(
            &mut app.active_messages_mut()[0],
            &spinner,
            80,
            1,
        );
        let _ = crate::ui::measure_message_height_cached(
            &mut app.active_messages_mut()[1],
            &spinner,
            80,
            1,
        );

        let bytes_a = app.messages()[0].render_cache.cached_bytes();
        let bytes_b = app.messages()[1].render_cache.cached_bytes();
        assert!(bytes_a > 0);
        assert!(bytes_b > 0);

        app.rebuild_render_cache_accounting();
        app.render_cache_budget.max_bytes = bytes_b;
        let stats = app.enforce_render_cache_budget();

        assert!(stats.evicted_bytes >= bytes_a);
        assert!(
            app.messages()[0].render_cache.cached_bytes() == 0
                || app.messages()[1].render_cache.cached_bytes() == 0
        );
    }

    #[test]
    fn enforce_history_retention_noop_under_budget() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("small message"),
            user_text_message("another message"),
        ];
        app.history_retention_mut().max_bytes = usize::MAX / 4;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 0);
        assert_eq!(stats.total_dropped_messages, 0);
        assert!(!app.messages().iter().any(App::is_history_hidden_marker_message));
    }

    #[test]
    fn enforce_history_retention_drops_oldest_and_adds_marker() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("first old message"),
            user_text_message("second old message"),
            user_text_message("third old message"),
        ];
        app.history_retention_mut().max_bytes = 1;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 3);
        assert!(matches!(app.messages()[0].role, MessageRole::Welcome));
        assert!(app.messages().iter().any(App::is_history_hidden_marker_message));
        assert_eq!(app.messages().len(), 2);
    }

    #[test]
    fn enforce_history_retention_preserves_in_progress_tool_message() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("droppable"),
            assistant_tool_message("tool-keep", model::ToolCallStatus::InProgress),
        ];
        app.history_retention_mut().max_bytes = 1;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 1);
        assert!(app.messages().iter().any(|msg| {
            msg.blocks.iter().any(|block| {
                matches!(
                    block,
                    MessageBlock::ToolCall(tc) if tc.id == "tool-keep"
                        && matches!(tc.status, model::ToolCallStatus::InProgress)
                )
            })
        }));
    }

    #[test]
    fn enforce_history_retention_preserves_pending_tool_message() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("droppable"),
            assistant_tool_message("tool-pending", model::ToolCallStatus::Pending),
        ];
        app.history_retention_mut().max_bytes = 1;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 1);
        assert!(app.messages().iter().any(|msg| {
            msg.blocks
                .iter()
                .any(|block| matches!(block, MessageBlock::ToolCall(tc) if tc.id == "tool-pending"))
        }));
    }

    #[test]
    fn enforce_history_retention_preserves_permission_tool_message() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("droppable"),
            assistant_tool_message_with_pending_permission("tool-perm"),
        ];
        app.history_retention_mut().max_bytes = 1;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 1);
        assert!(app.messages().iter().any(|msg| {
            msg.blocks
                .iter()
                .any(|block| matches!(block, MessageBlock::ToolCall(tc) if tc.id == "tool-perm"))
        }));
    }

    #[test]
    fn enforce_history_retention_rebuilds_tool_index_after_prune() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("drop this"),
            assistant_bash_tool_message("tool-idx", model::ToolCallStatus::InProgress, "term-1"),
        ];
        app.index_tool_call("tool-idx".to_owned(), 99, 99);
        app.sync_terminal_tool_call("stale-term".to_owned(), 99, 99);
        app.history_retention_mut().max_bytes = 1;

        let _ = app.enforce_history_retention();
        assert_eq!(app.lookup_tool_call("tool-idx"), Some((2, 0)));
        assert_eq!(app.terminal_tool_calls().len(), 1);
        assert_eq!(app.terminal_tool_call_membership().len(), 1);
        assert_eq!(app.terminal_tool_calls()[0].terminal_id, "term-1");
        assert_eq!(app.terminal_tool_calls()[0].msg_idx, 2);
        assert_eq!(app.terminal_tool_calls()[0].block_idx, 0);
    }

    #[test]
    fn enforce_history_retention_preserves_active_turn_assistant_message() {
        let mut app = make_test_app();
        app.status = AppStatus::Thinking;
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("drop this"),
            ChatMessage::new(MessageRole::Assistant, Vec::new(), None),
        ];
        app.bind_active_turn_assistant(2);
        app.history_retention_mut().max_bytes = 1;

        let stats = app.enforce_history_retention();

        assert_eq!(stats.dropped_messages, 1);
        assert_eq!(app.active_turn_assistant_idx(), Some(2));
        assert!(matches!(app.messages()[2].role, MessageRole::Assistant));
    }

    #[test]
    fn enforce_history_retention_remaps_active_turn_assistant_after_prune() {
        let mut app = make_test_app();
        app.status = AppStatus::Thinking;
        *app.active_messages_mut() = vec![
            user_text_message("drop this"),
            ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block("streaming reply")],
                None,
            ),
        ];
        app.bind_active_turn_assistant(1);
        app.history_retention_mut().max_bytes = App::measure_message_bytes(&app.messages()[1]);

        let stats = app.enforce_history_retention();

        assert_eq!(stats.dropped_messages, 1);
        assert_eq!(app.active_turn_assistant_idx(), Some(1));
        assert!(App::is_history_hidden_marker_message(&app.messages()[0]));
        assert!(matches!(app.messages()[1].role, MessageRole::Assistant));
    }

    #[test]
    fn enforce_history_retention_keeps_single_marker_on_repeat() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("drop me"),
        ];
        app.history_retention_mut().max_bytes = 1;

        let first = app.enforce_history_retention();
        let second = app.enforce_history_retention();
        let marker_count =
            app.messages().iter().filter(|msg| App::is_history_hidden_marker_message(msg)).count();

        assert_eq!(first.dropped_messages, 1);
        assert_eq!(second.dropped_messages, 0);
        assert_eq!(marker_count, 1);
    }

    #[test]
    fn enforce_history_retention_preserves_manual_scroll_anchor_across_drop_and_marker_insert() {
        let mut app = make_test_app();
        *app.active_messages_mut() = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("drop me first"),
            user_text_message("keep this anchored"),
            user_text_message("tail"),
        ];
        let _ = app.active_viewport_mut().on_frame(40, 12);
        {
            let n = app.messages().len();
            app.active_viewport_mut().sync_message_count(n);
        };
        for idx in 0..app.messages().len() {
            app.active_viewport_mut().set_message_height(idx, 4);
        }
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        app.active_viewport_mut().auto_scroll = false;
        app.active_viewport_mut().scroll_offset = 9;
        app.active_viewport_mut().scroll_target = 9;
        app.active_viewport_mut().scroll_pos = 9.0;
        app.history_retention_mut().max_bytes = app
            .measure_history_bytes()
            .saturating_sub(App::measure_message_bytes(&app.messages()[1]));

        let _ = app.enforce_history_retention();

        assert!(app.messages().iter().any(App::is_history_hidden_marker_message));
        assert_eq!(app.active_viewport_mut().scroll_anchor_to_restore(), Some((2, 1)));
    }

    #[test]
    fn lookup_missing_returns_none() {
        let app = make_test_app();
        assert!(app.lookup_tool_call("nonexistent").is_none());
    }

    #[test]
    fn index_and_lookup() {
        let mut app = make_test_app();
        app.index_tool_call("tc-123".into(), 2, 5);
        assert_eq!(app.lookup_tool_call("tc-123"), Some((2, 5)));
    }

    // App tool_call_index

    /// Index same ID twice - second write overwrites first.
    #[test]
    fn index_overwrite_existing() {
        let mut app = make_test_app();
        app.index_tool_call("tc-1".into(), 0, 0);
        app.index_tool_call("tc-1".into(), 5, 10);
        assert_eq!(app.lookup_tool_call("tc-1"), Some((5, 10)));
    }

    /// Empty string as tool call ID.
    #[test]
    fn index_empty_string_id() {
        let mut app = make_test_app();
        app.index_tool_call(String::new(), 1, 2);
        assert_eq!(app.lookup_tool_call(""), Some((1, 2)));
    }

    /// Stress: 1000 tool calls indexed and looked up.
    #[test]
    fn index_stress_1000_entries() {
        let mut app = make_test_app();
        for i in 0..1000 {
            app.index_tool_call(format!("tc-{i}"), i, i * 2);
        }
        // Spot check first, middle, last
        assert_eq!(app.lookup_tool_call("tc-0"), Some((0, 0)));
        assert_eq!(app.lookup_tool_call("tc-500"), Some((500, 1000)));
        assert_eq!(app.lookup_tool_call("tc-999"), Some((999, 1998)));
        // Non-existent still returns None
        assert!(app.lookup_tool_call("tc-1000").is_none());
    }

    /// Unicode in tool call ID.
    #[test]
    fn index_unicode_id() {
        let mut app = make_test_app();
        app.index_tool_call("\u{1F600}-tool".into(), 3, 7);
        assert_eq!(app.lookup_tool_call("\u{1F600}-tool"), Some((3, 7)));
    }

    // active_task_ids

    #[test]
    fn active_task_insert_remove() {
        let mut app = make_test_app();
        app.insert_active_task("task-1".into());
        assert!(app.active_task_ids().contains("task-1"));
        app.remove_active_task("task-1");
        assert!(!app.active_task_ids().contains("task-1"));
    }

    #[test]
    fn remove_nonexistent_task_is_noop() {
        let mut app = make_test_app();
        app.remove_active_task("does-not-exist");
        assert!(app.active_task_ids().is_empty());
    }

    // active_task_ids

    /// Insert same ID twice - set deduplicates; one remove clears it.
    #[test]
    fn active_task_insert_duplicate() {
        let mut app = make_test_app();
        app.insert_active_task("task-1".into());
        app.insert_active_task("task-1".into());
        assert_eq!(app.active_task_ids().len(), 1);
        app.remove_active_task("task-1");
        assert!(app.active_task_ids().is_empty());
    }

    /// Insert many tasks, remove in different order.
    #[test]
    fn active_task_insert_many_remove_out_of_order() {
        let mut app = make_test_app();
        for i in 0..100 {
            app.insert_active_task(format!("task-{i}"));
        }
        assert_eq!(app.active_task_ids().len(), 100);
        // Remove in reverse order
        for i in (0..100).rev() {
            app.remove_active_task(&format!("task-{i}"));
        }
        assert!(app.active_task_ids().is_empty());
    }

    /// Mixed insert/remove interleaving.
    #[test]
    fn active_task_interleaved_insert_remove() {
        let mut app = make_test_app();
        app.insert_active_task("a".into());
        app.insert_active_task("b".into());
        app.remove_active_task("a");
        app.insert_active_task("c".into());
        assert!(!app.active_task_ids().contains("a"));
        assert!(app.active_task_ids().contains("b"));
        assert!(app.active_task_ids().contains("c"));
        assert_eq!(app.active_task_ids().len(), 2);
    }

    /// Remove from empty set multiple times - no panic.
    #[test]
    fn active_task_remove_from_empty_repeatedly() {
        let mut app = make_test_app();
        for i in 0..100 {
            app.remove_active_task(&format!("ghost-{i}"));
        }
        assert!(app.active_task_ids().is_empty());
    }

    /// `clear_tool_scope_tracking` must also clear `active_task_ids`;
    /// a leaked task ID from a cancelled turn would otherwise cause
    /// main-agent tools on the next turn to be misclassified as
    /// Subagent scope.
    #[test]
    fn clear_tool_scope_tracking_also_clears_active_task_ids() {
        let mut app = make_test_app();
        app.insert_active_task("task-leaked".into());
        assert!(!app.active_task_ids().is_empty());
        app.clear_tool_scope_tracking();
        assert!(app.active_task_ids().is_empty(), "active_task_ids must be cleared at turn end");
    }

    #[test]
    fn finalize_in_progress_tool_calls_detaches_execute_terminal_refs() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_bash_tool_message(
            "bash-1",
            model::ToolCallStatus::InProgress,
            "term-1",
        ));
        app.index_tool_call("bash-1".to_owned(), 0, 0);
        app.sync_terminal_tool_call("term-1".to_owned(), 0, 0);

        let changed = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed);

        assert_eq!(changed, 1);
        assert!(app.terminal_tool_calls().is_empty());
        assert!(app.terminal_tool_call_membership().is_empty());
        let MessageBlock::ToolCall(tc) = &app.messages()[0].blocks[0] else {
            panic!("expected tool call");
        };
        assert_eq!(tc.status, model::ToolCallStatus::Completed);
        assert_eq!(tc.terminal_id, None);
    }

    #[test]
    fn insert_message_tracked_nontail_rebuilds_tool_indices_and_invalidates_suffix() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("before"));
        app.active_messages_mut()
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::Completed));
        app.active_messages_mut().push(user_text_message("after"));
        app.index_tool_call("tool-1".to_owned(), 1, 0);

        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().sync_message_count(3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        app.insert_message_tracked(1, user_text_message("inserted"));
        {
            let n = app.messages().len();
            app.active_viewport_mut().sync_message_count(n);
        };

        assert_eq!(app.lookup_tool_call("tool-1"), Some((2, 0)));
        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(1));
        assert_eq!(app.active_viewport_mut().prefix_dirty_from(), Some(1));
    }

    #[test]
    fn remove_message_tracked_nontail_rebuilds_tool_indices_and_invalidates_suffix() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("before"));
        app.active_messages_mut()
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::Completed));
        app.active_messages_mut().push(user_text_message("after"));
        app.index_tool_call("tool-1".to_owned(), 1, 0);

        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().sync_message_count(3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        let removed = app.remove_message_tracked(0);
        {
            let n = app.messages().len();
            app.active_viewport_mut().sync_message_count(n);
        };

        assert!(removed.is_some());
        assert_eq!(app.lookup_tool_call("tool-1"), Some((0, 0)));
        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(0));
        assert_eq!(app.active_viewport_mut().prefix_dirty_from(), Some(0));
    }

    #[test]
    fn remove_message_tracked_tail_removes_orphaned_tool_indices() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("before"));
        app.active_messages_mut()
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::Completed));
        app.index_tool_call("tool-1".to_owned(), 1, 0);

        let removed = app.remove_message_tracked(1);

        assert!(removed.is_some());
        assert!(app.lookup_tool_call("tool-1").is_none());
    }

    #[test]
    fn remove_message_tracked_prunes_tool_scope_entries() {
        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::Completed));
        app.index_tool_call("tool-1".to_owned(), 0, 0);
        app.register_tool_call_scope(
            "tool-1".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "task-1".to_owned() },
        );

        let removed = app.remove_message_tracked(0);

        assert!(removed.is_some());
        assert_eq!(app.tool_call_scope("tool-1"), None);
    }

    #[test]
    fn clear_messages_tracked_clears_tool_and_terminal_tracking() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_bash_tool_message(
            "bash-1",
            model::ToolCallStatus::InProgress,
            "term-1",
        ));
        app.index_tool_call("bash-1".to_owned(), 0, 0);
        app.sync_terminal_tool_call("term-1".to_owned(), 0, 0);
        app.pending_interaction_ids_mut().push("bash-1".into());

        app.clear_messages_tracked();

        assert!(app.messages().is_empty());
        assert!(app.tool_call_index().is_empty());
        assert!(app.terminal_tool_calls().is_empty());
        assert!(app.terminal_tool_call_membership().is_empty());
        assert!(app.pending_interaction_ids().is_empty());
    }

    #[test]
    fn rebuild_tool_indices_skips_completed_terminal_refs() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_bash_tool_message(
            "bash-1",
            model::ToolCallStatus::Completed,
            "term-1",
        ));
        app.index_tool_call("bash-1".to_owned(), 0, 0);
        app.sync_terminal_tool_call("term-1".to_owned(), 0, 0);

        app.rebuild_tool_indices_and_terminal_refs();

        assert!(app.terminal_tool_calls().is_empty());
        assert!(app.terminal_tool_call_membership().is_empty());
    }

    #[test]
    fn finalize_in_progress_tool_calls_invalidates_all_changed_messages() {
        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::InProgress));
        app.active_messages_mut().push(user_text_message("gap"));
        app.active_messages_mut()
            .push(assistant_tool_message("tool-2", model::ToolCallStatus::InProgress));

        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().sync_message_count(3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        let changed = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed);

        assert_eq!(changed, 2);
        assert!(!app.active_viewport_mut().message_height_is_current(0));
        assert!(app.active_viewport_mut().message_height_is_current(1));
        assert!(!app.active_viewport_mut().message_height_is_current(2));
        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(0));
    }

    // IncrementalMarkdown

    /// Simple render function for tests: wraps each line in a `Line`.
    fn test_render(src: &str) -> Vec<Line<'static>> {
        src.lines().map(|l| Line::from(l.to_owned())).collect()
    }

    fn test_render_key() -> super::messages::MarkdownRenderKey {
        super::messages::MarkdownRenderKey { width: 80, bg: None, preserve_newlines: false }
    }

    #[test]
    fn incr_default_empty() {
        let incr = IncrementalMarkdown::default();
        assert!(incr.full_text().is_empty());
    }

    #[test]
    fn incr_from_complete() {
        let incr = IncrementalMarkdown::from_complete("hello world");
        assert_eq!(incr.full_text(), "hello world");
    }

    #[test]
    fn incr_append_single_chunk() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("hello");
        assert_eq!(incr.full_text(), "hello");
    }

    #[test]
    fn incr_append_accumulates_chunks() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("line1");
        incr.append("\nline2");
        incr.append("\nline3");
        assert_eq!(incr.full_text(), "line1\nline2\nline3");
    }

    #[test]
    fn incr_append_preserves_paragraph_delimiters() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("para1\n\npara2");
        assert_eq!(incr.full_text(), "para1\n\npara2");
    }

    #[test]
    fn incr_full_text_reconstruction() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("p1\n\np2\n\np3");
        assert_eq!(incr.full_text(), "p1\n\np2\n\np3");
    }

    #[test]
    fn incr_lines_renders_all() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("line1\n\nline2\n\nline3");
        let lines = incr.lines(test_render_key(), &test_render);
        // test_render maps each source line to one output line
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn incr_ensure_rendered_preserves_text() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("p1\n\np2\n\ntail");
        incr.ensure_rendered(test_render_key(), &test_render);
        assert_eq!(incr.full_text(), "p1\n\np2\n\ntail");
    }

    #[test]
    fn incr_invalidate_renders_preserves_text() {
        let mut incr = IncrementalMarkdown::default();
        incr.append("p1\n\np2\n\ntail");
        incr.invalidate_renders();
        assert_eq!(incr.full_text(), "p1\n\np2\n\ntail");
    }

    #[test]
    fn incr_reuses_rendered_prefix_chunks() {
        use std::cell::Cell;

        let calls = Cell::new(0usize);
        let render = |src: &str| -> Vec<Line<'static>> {
            calls.set(calls.get() + 1);
            test_render(src)
        };

        let mut incr = IncrementalMarkdown::default();
        incr.append("p1\n\np2");
        let _ = incr.lines(test_render_key(), &render);
        assert_eq!(calls.get(), 2);

        incr.append(" tail");
        let _ = incr.lines(test_render_key(), &render);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn incr_does_not_split_inside_fenced_code_blocks() {
        let calls = std::cell::Cell::new(0usize);
        let render = |src: &str| -> Vec<Line<'static>> {
            calls.set(calls.get() + 1);
            test_render(src)
        };

        let mut incr = IncrementalMarkdown::default();
        incr.append("```rust\nfn main() {\n\nprintln!(\"hi\");\n}\n```\n\nafter");
        let _ = incr.lines(test_render_key(), &render);

        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn incr_streaming_simulation() {
        // Simulate a realistic streaming scenario
        let mut incr = IncrementalMarkdown::default();
        let chunks = ["Here is ", "some text.\n", "\nNext para", "graph here.\n\n", "Final."];
        for chunk in chunks {
            incr.append(chunk);
        }
        assert_eq!(incr.full_text(), "Here is some text.\n\nNext paragraph here.\n\nFinal.");
    }

    // ChatViewport

    #[test]
    fn viewport_new_defaults() {
        let vp = ChatViewport::new();
        assert_eq!(vp.scroll_offset, 0);
        assert_eq!(vp.scroll_target, 0);
        assert!(vp.auto_scroll);
        assert_eq!(vp.width, 0);
        assert!(vp.message_heights.is_empty());
        assert!(vp.oldest_stale_index().is_none());
        assert!(!vp.resize_remeasure_active());
        assert!(vp.height_prefix_sums.is_empty());
    }

    #[test]
    fn viewport_on_frame_sets_width() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        assert_eq!(vp.width, 80);
        assert_eq!(vp.height, 24);
    }

    #[test]
    fn viewport_on_frame_resize_invalidates() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.set_message_height(0, 10);
        vp.set_message_height(1, 20);
        vp.rebuild_prefix_sums();

        // Resize: old heights are kept as approximations,
        // but width markers are invalidated so re-measurement happens.
        let _ = vp.on_frame(120, 24);
        assert_eq!(vp.message_height(0), 10); // kept, not zeroed
        assert_eq!(vp.message_height(1), 20); // kept, not zeroed
        assert_eq!(vp.message_heights_width, 0); // forces re-measure
        assert_eq!(vp.prefix_sums_width, 0); // forces rebuild
    }

    #[test]
    fn viewport_on_frame_same_width_no_invalidation() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.set_message_height(0, 10);
        let _ = vp.on_frame(80, 24); // same width
        assert_eq!(vp.message_height(0), 10); // not zeroed
    }

    #[test]
    fn viewport_on_frame_height_change_preserves_message_measurements() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(2);
        vp.set_message_height(0, 10);
        vp.set_message_height(1, 20);
        vp.mark_heights_valid();
        vp.rebuild_prefix_sums();

        let change = vp.on_frame(80, 12);

        assert!(!change.width_changed);
        assert!(change.height_changed);
        assert_eq!(vp.height, 12);
        assert_eq!(vp.message_heights_width, 80);
        assert_eq!(vp.prefix_sums_width, 80);
        assert!(!vp.resize_remeasure_active());
        assert!(vp.message_height_is_current(0));
        assert!(vp.message_height_is_current(1));
    }

    #[test]
    fn viewport_message_height_set_and_get() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.set_message_height(0, 5);
        vp.set_message_height(1, 10);
        assert_eq!(vp.message_height(0), 5);
        assert_eq!(vp.message_height(1), 10);
        assert_eq!(vp.message_height(2), 0); // out of bounds
    }

    #[test]
    fn viewport_message_height_grows_vec() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.set_message_height(5, 42);
        assert_eq!(vp.message_heights.len(), 6);
        assert_eq!(vp.message_height(5), 42);
        assert_eq!(vp.message_height(3), 0); // gap filled with 0
    }

    #[test]
    fn viewport_invalidate_message_tracks_oldest_index() {
        let mut vp = ChatViewport::new();
        vp.sync_message_count(8);
        vp.mark_heights_valid();
        vp.invalidate_message(5);
        vp.invalidate_message(2);
        vp.invalidate_message(7);
        assert_eq!(vp.oldest_stale_index(), Some(2));
    }

    #[test]
    fn viewport_mark_heights_valid_clears_dirty_index() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(2);
        vp.mark_heights_valid();
        vp.invalidate_message(1);
        assert_eq!(vp.oldest_stale_index(), Some(1));
        vp.mark_heights_valid();
        assert!(vp.oldest_stale_index().is_none());
    }

    #[test]
    fn viewport_resize_remeasure_tracks_partial_exactness() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(3);
        vp.set_message_height(0, 4);
        vp.set_message_height(1, 5);
        vp.set_message_height(2, 6);
        vp.mark_heights_valid();

        let _ = vp.on_frame(120, 24);
        assert!(vp.resize_remeasure_active());
        assert!(!vp.message_height_is_current(0));

        vp.mark_message_height_measured(1);
        assert!(vp.message_height_is_current(1));
        assert!(!vp.message_height_is_current(0));

        vp.mark_heights_valid();
        assert_eq!(vp.message_heights_width, 120);
        assert!(vp.message_height_is_current(0));
        assert!(!vp.resize_remeasure_active());
    }

    #[test]
    fn viewport_resize_remeasure_expands_outward_from_anchor() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(6);
        vp.mark_heights_valid();

        let _ = vp.on_frame(100, 24);
        vp.ensure_resize_remeasure_anchor(2, 3, 6);

        assert_eq!(vp.next_resize_remeasure_index(6), Some(1));
        assert_eq!(vp.next_resize_remeasure_index(6), Some(0));
        assert_eq!(vp.next_resize_remeasure_index(6), Some(4));
        assert_eq!(vp.next_resize_remeasure_index(6), Some(5));
        assert_eq!(vp.next_resize_remeasure_index(6), None);
        assert!(!vp.resize_remeasure_active());
    }

    #[test]
    fn viewport_restore_resize_anchor_keeps_same_message_visible() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(4);
        for idx in 0..4 {
            vp.set_message_height(idx, 5);
        }
        vp.mark_heights_valid();
        vp.rebuild_prefix_sums();

        vp.auto_scroll = false;
        vp.scroll_offset = 7;
        vp.scroll_target = 7;
        vp.scroll_pos = 7.0;

        let _ = vp.on_frame(40, 24);
        let (anchor_idx, anchor_offset) =
            vp.resize_scroll_anchor().expect("resize should snapshot a scroll anchor");
        assert_eq!((anchor_idx, anchor_offset), (1, 2));

        vp.set_message_height(0, 12);
        vp.set_message_height(1, 8);
        vp.set_message_height(2, 6);
        vp.set_message_height(3, 6);
        vp.prefix_sums_width = 0;
        vp.rebuild_prefix_sums();
        vp.restore_scroll_anchor(anchor_idx, anchor_offset);

        assert_eq!(vp.scroll_offset, 14);
        assert_eq!(vp.find_first_visible(vp.scroll_offset), 1);
    }

    #[test]
    fn viewport_preserves_resize_anchor_when_followup_remeasure_replaces_plan() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(4);
        for idx in 0..4 {
            vp.set_message_height(idx, 5);
        }
        vp.mark_heights_valid();
        vp.rebuild_prefix_sums();

        vp.auto_scroll = false;
        vp.scroll_offset = 7;
        vp.scroll_target = 7;
        vp.scroll_pos = 7.0;

        let _ = vp.on_frame(40, 24);
        let resize_anchor = vp.resize_scroll_anchor().expect("resize should preserve an anchor");
        assert_eq!(resize_anchor, (1, 2));
        assert_eq!(vp.remeasure_reason(), Some(LayoutRemeasureReason::Resize));

        vp.invalidate_messages_from(0);

        assert_eq!(vp.remeasure_reason(), Some(LayoutRemeasureReason::MessagesFrom));
        assert_eq!(vp.resize_scroll_anchor(), Some(resize_anchor));
        assert_eq!(vp.scroll_anchor_to_restore(), Some(resize_anchor));
    }

    #[test]
    fn viewport_message_change_preserves_manual_anchor() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(4);
        for idx in 0..4 {
            vp.set_message_height(idx, 5);
        }
        vp.mark_heights_valid();
        vp.rebuild_prefix_sums();

        vp.auto_scroll = false;
        vp.scroll_offset = 7;
        vp.scroll_target = 7;
        vp.scroll_pos = 7.0;

        vp.invalidate_message(0);

        let anchor =
            vp.scroll_anchor_to_restore().expect("manual scroll should preserve an anchor");
        assert_eq!(anchor, (1, 2));

        vp.set_message_height(0, 12);
        vp.mark_message_height_measured(0);
        vp.rebuild_prefix_sums();
        assert_eq!(vp.ready_scroll_anchor_to_restore(), Some(anchor));

        vp.restore_scroll_anchor(anchor.0, anchor.1);
        assert_eq!(vp.scroll_offset, 14);
        assert_eq!(vp.find_first_visible(vp.scroll_offset), 1);
    }

    #[test]
    fn viewport_delays_anchor_restore_until_prefix_above_is_exact() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(4);
        for idx in 0..4 {
            vp.set_message_height(idx, 5);
        }
        vp.mark_heights_valid();
        vp.rebuild_prefix_sums();

        vp.auto_scroll = false;
        vp.scroll_offset = 12;
        vp.scroll_target = 12;
        vp.scroll_pos = 12.0;

        let _ = vp.on_frame(40, 24);
        let anchor = vp.resize_scroll_anchor().expect("resize should preserve an anchor");
        assert_eq!(anchor, (2, 2));
        assert_eq!(vp.scroll_anchor_to_restore(), Some(anchor));
        assert_eq!(vp.ready_scroll_anchor_to_restore(), None);

        vp.set_message_height(2, 9);
        vp.mark_message_height_measured(2);
        vp.rebuild_prefix_sums();
        assert_eq!(vp.ready_scroll_anchor_to_restore(), None);

        vp.set_message_height(0, 11);
        vp.mark_message_height_measured(0);
        vp.set_message_height(1, 8);
        vp.mark_message_height_measured(1);
        vp.rebuild_prefix_sums();

        assert_eq!(vp.ready_scroll_anchor_to_restore(), Some(anchor));
    }

    #[test]
    fn viewport_prioritizes_rows_above_preserved_anchor_until_restore_is_exact() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(6);
        for idx in 0..6 {
            vp.set_message_height(idx, 5);
        }
        vp.mark_heights_valid();
        vp.rebuild_prefix_sums();

        vp.auto_scroll = false;
        vp.scroll_offset = 12;
        vp.scroll_target = 12;
        vp.scroll_pos = 12.0;

        let _ = vp.on_frame(40, 24);
        vp.ensure_resize_remeasure_anchor(2, 3, 6);

        assert_eq!(vp.next_resize_remeasure_index(6), Some(1));
        assert_eq!(vp.next_resize_remeasure_index(6), Some(0));
        assert_eq!(vp.next_resize_remeasure_index(6), Some(4));
    }

    #[test]
    fn viewport_global_remeasure_preserves_anchor_while_prefix_above_converges() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.sync_message_count(6);
        for idx in 0..6 {
            vp.set_message_height(idx, 5);
        }
        vp.mark_heights_valid();
        vp.rebuild_prefix_sums();

        vp.auto_scroll = false;
        vp.scroll_offset = 17;
        vp.scroll_target = 17;
        vp.scroll_pos = 17.0;

        vp.invalidate_all_messages(LayoutRemeasureReason::Global);
        let anchor =
            vp.scroll_anchor_to_restore().expect("global remeasure should preserve an anchor");
        assert_eq!(anchor, (3, 2));

        vp.invalidate_message(5);

        assert_eq!(vp.remeasure_reason(), Some(LayoutRemeasureReason::MessageChanged));
        assert_eq!(vp.scroll_anchor_to_restore(), Some(anchor));

        vp.set_message_height(0, 12);
        vp.mark_message_height_measured(0);
        vp.set_message_height(1, 8);
        vp.mark_message_height_measured(1);
        vp.rebuild_prefix_sums();

        assert_eq!(vp.find_first_visible(vp.scroll_offset), 1);

        vp.restore_scroll_anchor(anchor.0, anchor.1);

        assert_eq!(vp.find_first_visible(vp.scroll_offset), 3);
        assert_eq!(vp.scroll_offset, 27);
    }

    #[test]
    fn viewport_prefix_sums_basic() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.set_message_height(0, 5);
        vp.set_message_height(1, 10);
        vp.set_message_height(2, 3);
        vp.rebuild_prefix_sums();
        assert_eq!(vp.total_message_height(), 18);
        assert_eq!(vp.cumulative_height_before(0), 0);
        assert_eq!(vp.cumulative_height_before(1), 5);
        assert_eq!(vp.cumulative_height_before(2), 15);
    }

    #[test]
    fn viewport_prefix_sums_streaming_fast_path() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.set_message_height(0, 5);
        vp.set_message_height(1, 10);
        vp.rebuild_prefix_sums();
        assert_eq!(vp.total_message_height(), 15);

        // Simulate streaming: last message grows
        vp.set_message_height(1, 20);
        vp.rebuild_prefix_sums(); // should hit fast path
        assert_eq!(vp.total_message_height(), 25);
        assert_eq!(vp.cumulative_height_before(1), 5);
    }

    #[test]
    fn viewport_find_first_visible() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.set_message_height(0, 10);
        vp.set_message_height(1, 10);
        vp.set_message_height(2, 10);
        vp.rebuild_prefix_sums();

        assert_eq!(vp.find_first_visible(0), 0);
        assert_eq!(vp.find_first_visible(10), 1);
        assert_eq!(vp.find_first_visible(15), 1);
        assert_eq!(vp.find_first_visible(20), 2);
    }

    #[test]
    fn viewport_find_first_visible_handles_offsets_before_first_boundary() {
        let mut vp = ChatViewport::new();
        let _ = vp.on_frame(80, 24);
        vp.set_message_height(0, 10);
        vp.set_message_height(1, 10);
        vp.rebuild_prefix_sums();

        assert_eq!(vp.find_first_visible(0), 0);
        assert_eq!(vp.find_first_visible(5), 0);
        assert_eq!(vp.find_first_visible(15), 1);
    }

    #[test]
    fn viewport_scroll_up_down() {
        let mut vp = ChatViewport::new();
        vp.scroll_target = 20;
        vp.scroll_pos = 20.0;
        vp.scroll_offset = 20;
        vp.auto_scroll = true;

        vp.scroll_up(5);
        assert_eq!(vp.scroll_target, 15);
        assert!((vp.scroll_pos - 15.0).abs() < f32::EPSILON);
        assert_eq!(vp.scroll_offset, 15);
        assert!(!vp.auto_scroll); // disabled on manual scroll

        vp.scroll_down(3);
        assert_eq!(vp.scroll_target, 18);
        assert!((vp.scroll_pos - 18.0).abs() < f32::EPSILON);
        assert_eq!(vp.scroll_offset, 18);
        assert!(!vp.auto_scroll); // not re-engaged by scroll_down
    }

    #[test]
    fn viewport_scroll_up_saturates() {
        let mut vp = ChatViewport::new();
        vp.scroll_target = 2;
        vp.scroll_pos = 2.0;
        vp.scroll_offset = 2;
        vp.scroll_up(10);
        assert_eq!(vp.scroll_target, 0);
        assert!(vp.scroll_pos.abs() < f32::EPSILON);
        assert_eq!(vp.scroll_offset, 0);
    }

    #[test]
    fn viewport_engage_auto_scroll() {
        let mut vp = ChatViewport::new();
        vp.auto_scroll = false;
        vp.engage_auto_scroll();
        assert!(vp.auto_scroll);
    }

    #[test]
    fn viewport_default_eq_new() {
        let a = ChatViewport::new();
        let b = ChatViewport::default();
        assert_eq!(a.width, b.width);
        assert_eq!(a.auto_scroll, b.auto_scroll);
        assert_eq!(a.message_heights.len(), b.message_heights.len());
    }

    fn focus_test_app_with_available_targets() -> App {
        let mut app = make_test_app();
        app.pending_interaction_ids_mut().push("perm-1".into());
        *app.slash_mut() = Some(SlashState {
            trigger_row: 0,
            trigger_col: 0,
            query: String::new(),
            context: SlashContext::CommandName,
            candidates: vec![SlashCandidate {
                insert_value: "/config".into(),
                primary: "/config".into(),
                secondary: Some("Open settings".into()),
            }],
            dialog: dialog::DialogState::default(),
        });
        app
    }

    #[test]
    fn focus_owner_respects_target_priority_and_release_order() {
        let mut app = focus_test_app_with_available_targets();

        assert_eq!(app.focus_owner(), FocusOwner::Input);

        app.claim_focus_target(FocusTarget::Permission);
        assert_eq!(app.focus_owner(), FocusOwner::Permission);

        app.claim_focus_target(FocusTarget::Mention);
        assert_eq!(app.focus_owner(), FocusOwner::Mention);

        app.release_focus_target(FocusTarget::Mention);
        assert_eq!(app.focus_owner(), FocusOwner::Permission);

        app.release_focus_target(FocusTarget::Permission);
        assert_eq!(app.focus_owner(), FocusOwner::Input);
    }

    #[test]
    fn focus_owner_falls_back_to_input_when_claimed_target_is_unavailable() {
        let mut app = make_test_app();
        // Mention focus is only valid when slash/mention state is set;
        // claiming it without that state should fall back to Input.
        app.claim_focus_target(FocusTarget::Mention);
        assert_eq!(app.focus_owner(), FocusOwner::Input);
    }

    // --- InvalidationLevel tests ---

    #[test]
    fn invalidate_single_tail_preserves_prefix_sums() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("a"));
        app.active_messages_mut().push(user_text_message("b"));
        app.active_messages_mut().push(user_text_message("c"));
        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().set_message_height(0, 5);
        app.active_viewport_mut().set_message_height(1, 10);
        app.active_viewport_mut().set_message_height(2, 3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        app.invalidate_layout(InvalidationLevel::MessageChanged(2)); // tail

        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(2));
        assert_eq!(app.active_viewport_mut().prefix_dirty_from(), Some(2));
        assert_eq!(app.viewport().prefix_sums_width, 0);
    }

    #[test]
    fn invalidate_single_nontail_invalidates_prefix_sums() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("a"));
        app.active_messages_mut().push(user_text_message("b"));
        app.active_messages_mut().push(user_text_message("c"));
        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().set_message_height(0, 5);
        app.active_viewport_mut().set_message_height(1, 10);
        app.active_viewport_mut().set_message_height(2, 3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        app.invalidate_layout(InvalidationLevel::MessageChanged(1)); // non-tail

        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(1));
        assert_eq!(app.active_viewport_mut().prefix_dirty_from(), Some(1));
        assert_eq!(app.viewport().prefix_sums_width, 0);
    }

    #[test]
    fn invalidate_from_always_invalidates_prefix_sums() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("a"));
        app.active_messages_mut().push(user_text_message("b"));
        app.active_messages_mut().push(user_text_message("c"));
        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().set_message_height(0, 5);
        app.active_viewport_mut().set_message_height(1, 10);
        app.active_viewport_mut().set_message_height(2, 3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();
        assert_ne!(app.viewport().prefix_sums_width, 0);

        // From at tail index still invalidates prefix sums (unlike Single).
        app.invalidate_layout(InvalidationLevel::MessagesFrom(2));

        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(2));
        assert_eq!(app.active_viewport_mut().prefix_dirty_from(), Some(2));
        assert_eq!(app.viewport().prefix_sums_width, 0);
    }

    #[test]
    fn invalidate_from_zero_matches_old_mark_all() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("a"));
        app.active_messages_mut().push(user_text_message("b"));
        app.active_messages_mut().push(user_text_message("c"));
        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().set_message_height(0, 5);
        app.active_viewport_mut().set_message_height(1, 10);
        app.active_viewport_mut().set_message_height(2, 3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        app.invalidate_layout(InvalidationLevel::MessagesFrom(0));

        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(0));
        assert_eq!(app.active_viewport_mut().prefix_dirty_from(), Some(0));
        assert_eq!(app.viewport().prefix_sums_width, 0);
    }

    #[test]
    fn invalidate_global_bumps_generation() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("a"));
        app.active_messages_mut().push(user_text_message("b"));
        app.active_messages_mut().push(user_text_message("c"));
        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().sync_message_count(3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();
        let gen_before = app.viewport().layout_generation;

        app.invalidate_layout(InvalidationLevel::Global);

        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(0));
        assert_eq!(app.active_viewport_mut().prefix_dirty_from(), Some(0));
        assert_eq!(app.viewport().prefix_sums_width, 0);
        assert_eq!(app.viewport().layout_generation, gen_before + 1);
    }

    #[test]
    fn invalidate_global_noop_on_empty() {
        let mut app = make_test_app();
        assert!(app.messages().is_empty());
        let gen_before = app.viewport().layout_generation;

        app.invalidate_layout(InvalidationLevel::Global);

        assert!(app.active_viewport_mut().oldest_stale_index().is_none());
        assert_eq!(app.viewport().layout_generation, gen_before);
    }

    #[test]
    fn invalidate_message_tracks_oldest_stale_index() {
        let mut app = make_test_app();
        // Need enough messages so all indices are non-tail for consistent behavior.
        for _ in 0..10 {
            app.active_messages_mut().push(user_text_message("x"));
        }
        app.active_viewport_mut().sync_message_count(10);
        app.active_viewport_mut().mark_heights_valid();

        app.invalidate_layout(InvalidationLevel::MessageChanged(5));
        app.invalidate_layout(InvalidationLevel::MessageChanged(2));
        app.invalidate_layout(InvalidationLevel::MessageChanged(7));

        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(2));
    }

    #[test]
    fn invalidation_level_eq_and_debug() {
        assert_eq!(InvalidationLevel::MessageChanged(5), InvalidationLevel::MessageChanged(5));
        assert_ne!(InvalidationLevel::MessageChanged(5), InvalidationLevel::MessagesFrom(5));
        assert_eq!(InvalidationLevel::Global, InvalidationLevel::Global);
        assert_eq!(InvalidationLevel::Resize, InvalidationLevel::Resize);
        // Debug derive works
        let dbg = format!("{:?}", InvalidationLevel::MessagesFrom(3));
        assert!(dbg.contains("MessagesFrom"));
    }
}
