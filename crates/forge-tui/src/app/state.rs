pub mod accessors;
pub mod block_cache;
pub mod cache_metrics;
pub(crate) mod focus;
mod history_retention;
pub mod messages;
pub mod monitors;
pub(crate) mod render_budget;
pub mod schedules;
pub mod sessions;
pub mod tool_call_info;
pub mod tool_calls;
pub(crate) mod turn;
pub mod types;
pub mod viewport;
pub(crate) mod welcome;
pub mod workflows;

// Re-export all public types so external `use crate::app::state::X` paths still work.
pub use block_cache::BlockCache;
pub use cache_metrics::CacheMetrics;
pub(crate) use messages::MarkdownRenderKey;
pub use messages::{
    CachedMessageSegment, ChatMessage, IncrementalMarkdown, MessageBlock, MessageRenderCache,
    MessageRenderCacheKey, MessageRenderSignature, MessageRole, NoticeBlock, NoticeDedupKey,
    RateLimitIncidentKey, SystemSeverity, TextBlock, TextBlockSpacing, TurnInfo, WelcomeBlock,
    hash_text_block_content, hash_welcome_block_content,
};
pub use tool_call_info::{
    AnsweredQuestion, ToolCallInfo, is_execute_tool_name, is_monitor_tool_name,
};
pub use types::{
    AppStatus, AttentionEntry, AttentionKind, BackgroundTask, ExtraUsage, FailedTurn, HelpView,
    HistoryRetentionStats, LoginHint, McpState, ModeInfo, ModeState, MonitorEntry, MonitorStatus,
    PasteSessionState, PendingCommandAck, PhaseEntry, PhaseStatus, RecentSessionInfo,
    RenderCacheBudget, ReviewRepliesWaiting, SUBAGENT_TAIL_CAP, ScheduleEntry, ScheduleKind,
    ScrollbarDragState, SelectionKind, SelectionPoint, SelectionState, SessionTurnState,
    SessionUsageState, StopHookEntry, StopHookSummaryState, SubagentChildEntry, SubagentEntry,
    TodoItem, TodoStatus, ToolCallScope, UsageSnapshot, UsageSourceKind, UsageState, UsageWindow,
    WorkflowEntry, WorkflowStatus,
};
pub use viewport::{
    ChatViewport, LayoutInvalidation, LayoutInvalidation as InvalidationLevel,
    LayoutRemeasureReason, ScrollbarGeometry, compute_scrollbar_geometry,
};

use crate::agent::model;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Instant;
use tokio::sync::mpsc;

use super::config::ConfigState;
use super::dialog;
use super::file_index;
use super::focus::FocusManager;
use super::plugins::PluginsState;
use super::view::ActiveView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutocompleteKind {
    Mention,
    Slash,
    Subagent,
    Emoji,
}

/// Which of forge's text editors is currently accepting input. Every
/// editor is an [`super::input::InputState`], so paste, clipboard and
/// dictation-burst handling is shared; only the submit semantics differ
/// per site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFocus {
    /// The chat draft on the active session.
    Chat,
    /// The /diff inline comment editor.
    DiffComment,
    /// The /diff Finish-review overview editor.
    DiffFinishReview,
    /// No editor open - text has nowhere to land.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd)]
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
/// Every variant is x+y bounded. In the projects pane's row grid a
/// body target stops short of the right-edge control gutter (see
/// [`control_gutter_start`]) rather than spanning the pane width, so a
/// control that only exists while the row is live can never occupy a
/// column the same row treated as body one frame earlier.
///
/// Only `ProjectHeader`, `WorkerRow` and their two close controls are
/// in that grid. The other six variants are stamped by the top bar,
/// the Inspector or the account panel and keep their own geometry, so
/// the gutter says nothing about them - and `CopySessionId` in
/// particular starts one column left of `control_gutter_start`, so it
/// genuinely overlaps the body target of whatever row the account
/// panel is painted over. Clicks resolve that by checking control
/// targets before row bodies, not by geometry. A per-row control added
/// to the Inspector's attention band, which is full width today, would
/// want the gutter treatment before it could rely on geometry either.
#[derive(Debug, Clone)]
pub enum PaneHitTarget {
    /// Click on a project name row → switch active session to its
    /// lead.
    ProjectHeader { project_name: String, y: u16, height: u16, x_start: u16, x_end: u16 },
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
    /// Click on the [`ROW_CLOSE_BUTTON`] at the right edge of an
    /// active project row → close that project's session (drop the
    /// bucket + tell the workspace to release its pool entry so the
    /// underlying `claude` subprocess can exit).
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
    /// snapshot has a diff to show (either layer populated, i.e.
    /// `worktree` or `branch_ahead` is `LayerState::Populated`)
    /// AND the inspector scroll offset is 0 (otherwise the header
    /// is off-screen).
    InspectorGitOpenDiff { y: u16, height: u16, x_start: u16, x_end: u16 },
    /// Click anywhere on the Inspector pane's `MCP SERVERS` section
    /// (header or any row) → open the same MCP view the `/mcp` slash
    /// command opens. Stamped for the section's on-screen rect,
    /// clipped when the section scrolls off either edge.
    InspectorMcpOpenStatus { y: u16, height: u16, x_start: u16, x_end: u16 },
    /// Click on the Inspector pane's `PR #N` row -> open the PR's
    /// URL in the system browser via `Command::OpenUrl`. `url` is
    /// captured at stamp time (the snapshot's `GitPrInfo.url`) so a
    /// session switch between render and click can't open a stale
    /// PR.
    InspectorGitPrOpen { url: String, y: u16, height: u16, x_start: u16, x_end: u16 },
    /// Click on a row in the Inspector's pinned NEEDS ATTENTION band ->
    /// switch the active session to `session_key` so its pending
    /// prompt lands in the chat. Stamped per row during render; the
    /// band is pinned so no scroll-offset gate is needed.
    InspectorAttentionRow {
        session_key: forge_workspace::SessionKey,
        y: u16,
        height: u16,
        x_start: u16,
        x_end: u16,
    },
    /// Click on the `⎘` glyph at the right end of the projects-pane
    /// account footer's Session row → copy the full session id to
    /// the clipboard. `session_id` is captured at stamp time so the
    /// handler doesn't have to look it up again (and so a session
    /// switch between render and click can't write the wrong id).
    CopySessionId { session_id: String, y: u16, height: u16, x_start: u16, x_end: u16 },
    /// [`ROW_CLOSE_BUTTON`] on a worker tree-child row. Click
    /// dispatches `Command::CloseWorker { project_key, label }`.
    CloseWorker {
        project_key: forge_workspace::ProjectKey,
        label: String,
        y: u16,
        height: u16,
        x_start: u16,
        x_end: u16,
    },
    /// Label area of a worker tree-child row. Click switches focus to
    /// the worker's chat (same gesture as a project-row click, but the
    /// destination is the worker's `SessionKey` not the lead's).
    WorkerRow {
        project_key: forge_workspace::ProjectKey,
        label: String,
        session_key: forge_workspace::SessionKey,
        y: u16,
        height: u16,
        x_start: u16,
        x_end: u16,
    },
}

impl PaneHitTarget {
    /// Whether the target's row range covers `y` (inclusive of `y`,
    /// exclusive of `y + height`). Private on purpose: a y-only
    /// hit-test is what let a close button claim columns the cold row
    /// routed to its body, so [`Self::contains`] is the only way to
    /// resolve a click.
    fn contains_y(&self, y: u16) -> bool {
        let (start, height) = match self {
            Self::ProjectHeader { y, height, .. }
            | Self::TopBarIcon { y, height, .. }
            | Self::InspectorTopBarIcon { y, height, .. }
            | Self::OverlayClose { y, height, .. }
            | Self::CloseSession { y, height, .. }
            | Self::InspectorGitOpenDiff { y, height, .. }
            | Self::InspectorGitPrOpen { y, height, .. }
            | Self::InspectorMcpOpenStatus { y, height, .. }
            | Self::InspectorAttentionRow { y, height, .. }
            | Self::CopySessionId { y, height, .. }
            | Self::CloseWorker { y, height, .. }
            | Self::WorkerRow { y, height, .. } => (*y, *height),
        };
        (start..start.saturating_add(height)).contains(&y)
    }

    /// Full hit-test: the click must fall inside the recorded row
    /// range and the recorded `[x_start, x_end)` column range. The
    /// match is exhaustive with no unconstrained arm, so a new variant
    /// cannot be added without deciding its columns.
    pub fn contains(&self, x: u16, y: u16) -> bool {
        if !self.contains_y(y) {
            return false;
        }
        match self {
            Self::ProjectHeader { x_start, x_end, .. }
            | Self::WorkerRow { x_start, x_end, .. }
            | Self::TopBarIcon { x_start, x_end, .. }
            | Self::InspectorTopBarIcon { x_start, x_end, .. }
            | Self::OverlayClose { x_start, x_end, .. }
            | Self::CloseSession { x_start, x_end, .. }
            | Self::InspectorGitOpenDiff { x_start, x_end, .. }
            | Self::InspectorGitPrOpen { x_start, x_end, .. }
            | Self::InspectorMcpOpenStatus { x_start, x_end, .. }
            | Self::InspectorAttentionRow { x_start, x_end, .. }
            | Self::CopySessionId { x_start, x_end, .. }
            | Self::CloseWorker { x_start, x_end, .. } => (*x_start..*x_end).contains(&x),
        }
    }
}

/// First column of the right-edge control gutter, for a projects-pane
/// project or worker row spanning `area`. Those rows' body targets end
/// here and their close controls start here, so the two ranges are
/// adjacent by construction. Targets stamped by other surfaces keep
/// their own geometry and are not bound by this.
pub fn control_gutter_start(area: ratatui::layout::Rect) -> u16 {
    area.x.saturating_add(area.width).saturating_sub(CONTROL_GUTTER_WIDTH)
}

/// The close button painted at the right edge of a live project or
/// worker row. Exported so the paint and the hit band are measured
/// from one glyph rather than from two literals that agree today.
pub const ROW_CLOSE_BUTTON: &str = " x ";

/// Columns a row reserves at its right edge: one separator, the
/// [`ROW_CLOSE_BUTTON`], one right pad. Both the row's name budget and
/// its close band are derived from this, so widening the button cannot
/// move the drawn glyph out from under the clickable band.
///
/// The band itself runs `[gutter_start, row_right - 1)`: the separator
/// column is tolerance, and the right pad column stays inert.
const CONTROL_GUTTER_WIDTH: u16 = 5;

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
    pub render_start: usize,
    pub local_scroll: usize,
    pub rendered_msgs: usize,
    pub last_rendered_idx: Option<usize>,
    pub rendered_line_count: usize,
    pub last_message_idx: Option<usize>,
    pub last_message_height: Option<usize>,
    pub selection_snapshot_active: bool,
}

// `App` is the god struct - bools are independent UI flags (autoscroll, paste-detected, dirty-rerender). Bundling defeats clarity at call sites.
pub struct App {
    pub active_view: ActiveView,
    pub config: ConfigState,
    pub settings_home_override: Option<PathBuf>,
    pub status: AppStatus,
    pub should_quit: bool,
    /// `true` once preflight has handed over to wherever the invocation
    /// was headed. Latches for the run: see
    /// [`crate::app::preflight::advance`].
    pub preflight_done: bool,
    /// `true` once preflight has painted a cancelled model fetch, which
    /// is what lets forge quit having said what it kept and where.
    pub preflight_cancel_drawn: bool,
    /// `true` once the boot spawn has logged that it is waiting on the
    /// account plan. Keeps that line to one, rather than one per frame
    /// of a 120fps loop.
    pub spawn_deferred_logged: bool,
    /// Optional fatal app error that should be surfaced at CLI boundary.
    pub exit_error: Option<crate::error::AppError>,
    /// Boot-wave fresh-start flag from the `--new` launch flag. When
    /// set, the boot dispatch stamps `force_new` on the boot
    /// `SessionLaunchSettings` it spawns with, so both the focused
    /// project (`StartDefault`) and the rest (`SpawnProject`) bring
    /// their leads + workers up fresh instead of resuming. Later
    /// click-to-spawn builds its own settings (force_new false) and is
    /// unaffected.
    pub start_new_run: bool,
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
    #[rustfmt::skip] #[cfg(feature = "testing")] pub test_notifications: std::cell::RefCell<Vec<super::notify::NotifyEvent>>,
    /// Per-session state buckets, keyed by claude session UUID.
    /// [`super::session::UiSession`] value type one bucket at a time.
    pub sessions: std::collections::HashMap<forge_workspace::SessionKey, super::session::UiSession>,
    /// Which entry of [`Self::sessions`] the renderer reads from.
    /// `None` only in the brief pre-Connect window where no session
    /// has landed in the map yet.
    pub active_session_key: Option<forge_workspace::SessionKey>,
    /// Synthetic spawn key the user asked to be taken to, set when a
    /// click wakes a cold project and consumed by the `Spawning`
    /// reducer once that bucket exists. The reducer cannot focus
    /// unconditionally - every `auto_start` project emits `Spawning`
    /// at boot and the first to arrive would steal the tab - so a
    /// user-driven wake records its intent here instead.
    pub pending_spawn_focus: Option<forge_workspace::SessionKey>,
    /// Snapshot of the durable forge crons (`mcp__forge__cron`) the
    /// active session itself created, refreshed on the ~1s ticker
    /// (`git_diff::apply_timer_tick`) from
    /// `Workspace::crons_for_project`, then narrowed to the session's
    /// own `team_role`. The Inspector SCHEDULES section reads this cache
    /// each render instead of hitting the workspace per frame (mirrors
    /// the git-diff snapshot pattern). Empty when there's no active
    /// project or the session created no cron.
    pub forge_crons: Vec<forge_primitives::CronEntry>,
    /// Presentation rows for the Inspector SCHEDULES section, humanized
    /// once per ~1s tick by [`App::refresh_forge_crons`] (parallel to the
    /// raw `forge_crons`). The render reads these instead of resolving
    /// the local timezone + humanizing per frame; the live countdown
    /// still recomputes from each row's `fire_at` at render time.
    pub forge_schedule_rows: Vec<crate::app::state::types::ScheduleEntry>,
    /// The Gotify subscriptions the active session itself created, plus
    /// the stream connection status. Refreshed on the ~1s tick by
    /// [`App::refresh_gotify`] and scoped by own `team_role`; the
    /// Inspector GOTIFY section reads these caches each render (mirrors
    /// `forge_crons`). The section renders only while connected with at
    /// least one owned subscription (see `gotify_section_visible`).
    pub gotify_subs: Vec<forge_primitives::GotifySubscription>,
    pub gotify_connected: bool,
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
    /// `events::apply_session_update`. User actions flow out via
    /// `workspace.dispatch(Command::...)`.
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
    /// Send / receive ends of the TUI-internal channel the
    /// `crate::app::dictate_devices` enumeration tasks use to hand the
    /// `/dictate` overlay its device catalog. Same shape as
    /// `git_diff_event_*`.
    pub dictate_devices_tx: std_mpsc::Sender<crate::app::dictate_devices::DictateDevicesEvent>,
    pub dictate_devices_rx: std_mpsc::Receiver<crate::app::dictate_devices::DictateDevicesEvent>,
    /// The last device catalog the workspace enumerated, or why it
    /// could not. `None` until the first `/dictate` open asks for one;
    /// a fresh open re-enumerates and replaces it on arrival.
    pub dictate_devices: Option<Result<forge_workspace::DictateDeviceCatalog, String>>,
    /// A catalog request is in flight; the tick guards on it so rapid
    /// opens do not stack walks.
    pub dictate_devices_in_flight: bool,
    /// The overlay re-opened since the last walk: the cache is stale
    /// and the next main-loop tick re-enumerates.
    pub dictate_devices_dirty: bool,
    /// Send / receive ends of the channel the one-shot
    /// `crate::app::review_waiting` recompute tasks hand their result
    /// back on. Same shape as `git_diff_event_*`.
    pub review_waiting_event_tx: std_mpsc::Sender<crate::app::review_waiting::ReviewWaitingEvent>,
    pub review_waiting_event_rx: std_mpsc::Receiver<crate::app::review_waiting::ReviewWaitingEvent>,
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
    pub usage_overlay_event_tx: std_mpsc::Sender<crate::app::usage_overlay::UsageOverlayEvent>,
    pub usage_overlay_event_rx: std_mpsc::Receiver<crate::app::usage_overlay::UsageOverlayEvent>,
    /// Monotonic counter bumped by every `/diff` invocation. Events
    /// arriving on `diff_overlay_event_rx` carry the seq they were
    /// spawned under; the drain pump only opens the overlay for
    /// the latest seq, so a rapid second `/diff` correctly
    /// supersedes the first instead of replaying the older result.
    pub diff_scan_seq: u64,
    /// Latest installed-vs-published claude CLI version snapshot.
    /// `None` until the startup fetch task lands. Rendered by the
    /// bottom-left account panel; missing values render as DIM `-`
    /// so the panel's row count stays constant.
    pub cli_version_info: Option<forge_workspace::env::cli_version::CliVersionInfo>,
    pub spinner_frame: usize,
    pub spinner_last_advance_at: Option<Instant>,
    /// Active spinner style for every animated surface (chat, input,
    /// projects pane, inspector, launchpad). Seeded from the config
    /// `spinner` field at startup; mutated live by `/spinner`.
    pub spinner_style: forge_workspace::SpinnerStyle,
    /// Monotonic start anchor for the time-based spinner. Frame index
    /// derives from `spinner_epoch.elapsed() / cadence_ms`.
    pub spinner_epoch: Instant,
    /// How often the run loop repaints while something is animating.
    /// Seeded from the config `fps` field at startup; read-only after.
    pub repaint_cadence: forge_workspace::RepaintCadence,
    /// Open `/spinner` picker overlay state; `None` when closed.
    pub spinner_picker: Option<crate::app::spinner_picker::SpinnerPickerState>,
    /// Open `/model` picker overlay state; `None` when closed.
    pub model_picker: Option<crate::app::model_picker::ModelPickerState>,
    /// Open `/account` picker overlay state; `None` when closed.
    pub account_picker: Option<crate::app::account_picker::AccountPickerState>,
    /// Open `/dictate` overlay state; `None` when closed.
    pub dictate_picker: Option<crate::app::dictate_picker::DictatePickerState>,
    /// Push-to-talk press tracking for the configured dictate key.
    pub(crate) dictate_key: crate::app::dictate_key::DictateKeyState,
    /// Whether a dictate start has been dispatched whose echo has not
    /// landed yet. Optimistic: set by the key handler's dispatch,
    /// cleared by the `DictateStarted` / `DictateEnded` reducers. The
    /// press classification reads this alongside the buckets so a
    /// second tap before the echo cannot dispatch a duplicate start.
    pub(crate) dictate_take_pending: bool,
    /// Session-level preference for collapsing non-Execute tool call bodies.
    /// Toggled by Ctrl+X and applied at render/layout time.
    pub tools_collapsed: bool,
    /// Test-only spy: records the level of the most recent
    /// `invalidate_layout` call. Used by the chat-tool-grouping v2
    /// regression-guard test to assert ctrl+x cycling a focused group
    /// stays at `MessageChanged(msg_idx)` and never escalates to
    /// `Global` (which produced a ~74ms hitch on long sessions
    /// before the fix). Cleared by tests that want to observe the
    /// next invalidation cleanly.
    #[cfg(any(test, feature = "testing"))]
    pub last_invalidation_level: std::cell::Cell<Option<crate::app::InvalidationLevel>>,
    /// Whether the Wide-tier Projects pane is currently visible.
    /// Toggled by Cmd+Left (Ctrl+Left elsewhere) at Wide / Medium tiers. In-memory only -
    /// each launch starts visible. Has no effect at Narrow tier -
    /// that tier renders the top bar unconditionally and uses
    /// [`Self::projects_pane_overlay_open`] for the on-demand
    /// overlay.
    pub projects_pane_visible: bool,
    /// Scroll offset (in row units) for the Projects pane body. Top
    /// banner row + DIM rule stay pinned regardless; the project /
    /// session list scrolls under them. Bottom account footer also
    /// stays pinned. In-memory only - each launch starts at 0.
    /// Mouse wheel over the pane bumps this; renderer clamps against
    /// `(total_rows - visible_height)` each frame so a wheel-past-end
    /// settles at the bottom rather than scrolling past.
    pub projects_pane_scroll_offset: u16,
    /// Whether the Narrow-tier Projects overlay is currently open.
    /// Transient - NOT persisted; each launch starts closed. Toggled
    /// by the same chord at Narrow tier or by clicking the `▤` icon in the
    /// top bar; closed by clicking the overlay's `✕` glyph, by Esc,
    /// or by switching to a project / session row inside the overlay.
    pub projects_pane_overlay_open: bool,
    /// Whether the Wide/Medium-tier Inspector pane is currently
    /// visible (right side, mirror of [`Self::projects_pane_visible`]).
    /// Toggled by Cmd+Right (Ctrl+Right elsewhere). In-memory only - each launch starts visible.
    /// Has no effect at Narrow tier - that tier uses
    /// [`Self::inspector_pane_overlay_open`] for the on-demand
    /// overlay.
    pub inspector_pane_visible: bool,
    /// Whether the Narrow-tier Inspector overlay is currently open.
    /// Transient - NOT persisted; each launch starts closed. Toggled
    /// by the same chord at Narrow tier or by clicking the `▦` icon in the
    /// top bar; closed by clicking the overlay's `✕` glyph or by
    /// Esc. Mutually exclusive with `projects_pane_overlay_open` -
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
    // list is per-project - switching active session via the
    // Projects pane naturally swaps the list along with the bucket.
    // See `App::recent_sessions` / `App::recent_sessions_mut`.
    /// State for the launchpad's project picker, which preflight hands
    /// over to when forge was given no project, and which `/launchpad`
    /// returns to.
    /// Always present - reset whenever the active view transitions
    /// to [`ActiveView::Launchpad`] via the launchpad open helper.
    /// When the active view is anything else this is unused but
    /// kept allocated so transitions are cheap.
    pub launchpad: crate::app::LaunchpadState,
    /// Diff overlay state - `Some` while [`ActiveView::Diff`] is
    /// up, `None` otherwise. Dropped on overlay close so a stale
    /// snapshot can't leak into the next open.
    pub diff_overlay: Option<crate::app::DiffOverlayState>,
    /// Open `:shortcode:` emoji picker. App-level rather than
    /// per-session because it serves the /diff review editors too.
    pub emoji: Option<super::emoji::EmojiState>,
    /// Usage overlay state - `Some` while [`ActiveView::Usage`] is up,
    /// `None` otherwise. Dropped on close so a stale report can't leak
    /// into the next open.
    pub usage_overlay: Option<crate::app::UsageOverlayState>,
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
    /// Desired OS pointer shape from the last hover hit-test. The main
    /// loop emits the OSC only when this differs from
    /// `emitted_pointer_shape`. Hover is a terminal side-channel - it
    /// never triggers a redraw.
    pub(crate) pointer_shape: crate::app::events::mouse::PointerShape,
    /// Last pointer shape actually written to the terminal (de-dupes
    /// OSC 22 writes; a still pointer costs nothing). `None` until the
    /// first emit, so the initial flush always fires - that's what sets
    /// the arrow at startup instead of inheriting the terminal's own
    /// text-surface I-beam default.
    pub(crate) emitted_pointer_shape: Option<crate::app::events::mouse::PointerShape>,
    /// Set when a resize arrived, cleared once the loop has rewritten the
    /// keyboard enhancement flags. A byte-transparent session manager
    /// leaves the flags on the terminal rather than the session, so a
    /// reattach needs them set again; SIGWINCH is the signal that one
    /// may have happened.
    pub(crate) needs_keyboard_flags_restore: bool,
    /// Area where the Inspector pane's **scrollable body** was last
    /// rendered (excluding the pinned banner + rule above it). Used
    /// by the mouse-wheel handler to detect "wheel scrolled while
    /// cursor is over the inspector pane" and adjust the active
    /// session's `inspector_scroll_offset`. `Rect::default()` until
    /// the first inspector render.
    pub rendered_inspector_body_area: ratatui::layout::Rect,
    /// Rect of the Projects pane's scrollable body (the area below
    /// the pinned `PROJECTS` banner / rule and above the account
    /// footer). Mirror of `rendered_inspector_body_area` - used by
    /// the mouse handler to route wheel events to
    /// `projects_pane_scroll_offset` instead of the chat viewport.
    /// `Rect::default()` until the first projects-pane render.
    pub rendered_projects_pane_body_area: ratatui::layout::Rect,
    // `file_index: FileIndexState` moved to `UiSession.file_index`
    // (per-session bucket). The scanner is project-scoped - switching
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
    /// Project name from the CLI's positional `<PROJECT>` argument, if
    /// any. `None` means open the `default = true` project.
    /// Forwarded to [`forge_workspace::SessionTarget::Named`] when the
    /// connection task spins up.
    pub startup_project: Option<String>,
    /// True while `events::session_reset::load_resume_history` is
    /// walking on-disk history through the shared SDK-message
    /// dispatcher. Replay reuses the live walker so content blocks,
    /// tool_use, todos, and plans land in the bucket via the same code
    /// path - but the walker also has side effects that are wrong for
    /// replay, most notably the lifecycle `Running` write in
    /// `handle_assistant`. Replay messages are historical rather than
    /// live wire content, so every such side effect checks this flag.
    /// Cleared at end of replay so subsequent live messages on the
    /// same session behave normally.
    pub replay_in_progress: bool,
}

impl App {
    /// `true` when the active session has a registered agent handle
    /// in the workspace's `DomainSession`. Production code consults
    /// this rather than holding an `Arc<AgentHandle>` directly -
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
    #[allow(clippy::expect_used)]
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

    /// Active session's monotonic scope epoch.
    pub fn session_scope_epoch(&self) -> u64 {
        self.active_session().map_or(0, |s| s.session_scope_epoch)
    }

    /// Increment the active session's scope epoch.
    pub fn bump_session_scope_epoch(&mut self) {
        let bucket = self.active_bucket_mut();
        bucket.session_scope_epoch = bucket.session_scope_epoch.saturating_add(1);
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

    pub(crate) fn sync_after_message_tail_changed(&mut self, msg_idx: usize) {
        if let Some(message) = self.active_messages_mut().get_mut(msg_idx) {
            message.invalidate_render_cache();
        }
        self.sync_render_cache_message_tail(msg_idx);
        self.recompute_message_retained_bytes(msg_idx);
        self.invalidate_layout(InvalidationLevel::MessageChanged(msg_idx));
    }

    /// Invalidate message layout caches at the given level.
    ///
    /// Single entry point for all layout invalidation. Replaces the former
    /// `mark_message_layout_dirty` / `mark_all_message_layout_dirty` methods.
    pub fn invalidate_layout(&mut self, level: LayoutInvalidation) {
        #[cfg(any(test, feature = "testing"))]
        self.last_invalidation_level.set(Some(level));
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
        // #310 architectural fix: invalidating layout always implies
        // re-rendering. Without this, callers that forget to set
        // needs_redraw produce idle-state bugs where the invalidated
        // state never reaches the screen (e.g. ctrl+x group cycle,
        // group summary click). Setting it unconditionally here kills
        // the whole class - a no-op when the caller already set it;
        // the cure when they didn't.
        self.needs_redraw = true;
    }

    /// Mark every background session's message heights stale, for an
    /// App-global render preference that changes what all of them paint
    /// - `LayoutInvalidation::Global` reaches the active viewport only.
    ///
    /// Marking is the whole job: each session's remeasure stays lazy and
    /// is paid when it next renders. No `layout_generation` bump, since
    /// the per-block measurement keys already carry the preference.
    pub(crate) fn invalidate_background_session_layouts(&mut self) {
        let active = self.active_session_key.clone();
        for (key, session) in &mut self.sessions {
            if Some(key) == active.as_ref() {
                continue;
            }
            session.viewport.invalidate_all_messages(LayoutRemeasureReason::Global);
        }
    }

    /// Out-of-range indices are dropped; passing one is a caller bug,
    /// not a supported input.
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
        let (dictate_devices_tx, dictate_devices_rx) = std_mpsc::channel();
        let (review_waiting_tx, review_waiting_rx) = std_mpsc::channel();
        let (process_scan_tx, process_scan_rx) = std_mpsc::channel();
        let (cli_version_tx, cli_version_rx) = std_mpsc::channel();
        let (diff_overlay_tx, diff_overlay_rx) = std_mpsc::channel();
        let (usage_overlay_tx, usage_overlay_rx) = std_mpsc::channel();
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
            preflight_done: false,
            preflight_cancel_drawn: false,
            spawn_deferred_logged: false,
            exit_error: None,
            start_new_run: false,
            workspace: Some(workspace),
            #[rustfmt::skip] #[cfg(feature = "testing")] test_dispatched_permission_outcomes: std::cell::RefCell::new(Vec::new()),
            #[rustfmt::skip] #[cfg(feature = "testing")] test_dispatched_question_outcomes: std::cell::RefCell::new(Vec::new()),
            #[rustfmt::skip] #[cfg(feature = "testing")] test_notifications: std::cell::RefCell::new(Vec::new()),
            sessions,
            active_session_key: Some(pending_key),
            pending_spawn_focus: None,
            forge_crons: Vec::new(),
            forge_schedule_rows: Vec::new(),
            gotify_subs: Vec::new(),
            gotify_connected: false,
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
            dictate_devices_tx,
            dictate_devices_rx,
            dictate_devices: None,
            dictate_devices_in_flight: false,
            dictate_devices_dirty: false,
            review_waiting_event_tx: review_waiting_tx,
            review_waiting_event_rx: review_waiting_rx,
            process_scan_event_tx: process_scan_tx,
            process_scan_event_rx: process_scan_rx,
            cli_version_event_tx: cli_version_tx,
            cli_version_event_rx: cli_version_rx,
            diff_overlay_event_tx: diff_overlay_tx,
            diff_overlay_event_rx: diff_overlay_rx,
            usage_overlay_event_tx: usage_overlay_tx,
            usage_overlay_event_rx: usage_overlay_rx,
            diff_scan_seq: 0,
            cli_version_info: None,
            spinner_frame: 0,
            spinner_last_advance_at: None,
            spinner_style: forge_workspace::SpinnerStyle::default(),
            spinner_epoch: Instant::now(),
            repaint_cadence: forge_workspace::RepaintCadence::default(),
            spinner_picker: None,
            model_picker: None,
            account_picker: None,
            dictate_picker: None,
            dictate_key: crate::app::dictate_key::DictateKeyState::default(),
            dictate_take_pending: false,
            tools_collapsed: true,
            #[cfg(any(test, feature = "testing"))]
            last_invalidation_level: std::cell::Cell::new(None),
            projects_pane_visible: true,
            projects_pane_scroll_offset: 0,
            projects_pane_overlay_open: false,
            inspector_pane_visible: true,
            inspector_pane_overlay_open: false,
            pane_hit_targets: Vec::new(),
            layout: crate::ui::layout::AppLayout::default(),
            force_redraw: false,
            focus: FocusManager::default(),
            plugins: PluginsState::default(),
            launchpad: crate::app::LaunchpadState::default(),
            diff_overlay: None,
            emoji: None,
            usage_overlay: None,
            cached_frame_area: ratatui::layout::Rect::default(),
            scrollbar_drag: None,
            rendered_chat_lines: Vec::new(),
            rendered_chat_area: ratatui::layout::Rect::default(),
            rendered_input_lines: Vec::new(),
            rendered_input_area: ratatui::layout::Rect::default(),
            pointer_shape: crate::app::events::mouse::PointerShape::Default,
            emitted_pointer_shape: None,
            needs_keyboard_flags_restore: false,
            rendered_inspector_body_area: ratatui::layout::Rect::default(),
            rendered_projects_pane_body_area: ratatui::layout::Rect::default(),
            paste_burst: super::paste_burst::PasteBurstDetector::new(),
            needs_redraw: true,
            notifications: super::notify::NotificationManager::new(),
            perf: None,
            render_cache_budget: RenderCacheBudget::default(),
            fps_ema: None,
            last_frame_at: None,
            connection_started: false,
            startup_project: None,
            replay_in_progress: false,
        }
    }
}

#[cfg(test)]
impl App {
    /// Test-only: stamp a process snapshot on the active session so
    /// `collect_active_processes` can be exercised end-to-end with a
    /// populated OS scan.
    pub(crate) fn set_active_process_snapshot_for_test(
        &mut self,
        snapshot: forge_workspace::env::processes::ProcessSnapshot,
    ) {
        self.active_bucket_mut().process_snapshot = Some(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    // App tool_call_index

    pub(super) fn make_test_app() -> App {
        App::test_default()
    }

    pub(super) fn assistant_text_block(text: &str) -> MessageBlock {
        MessageBlock::Text(TextBlock::from_complete(text))
    }

    pub(super) fn user_text_message(text: &str) -> ChatMessage {
        ChatMessage::new(MessageRole::User, vec![assistant_text_block(text)])
    }

    pub(super) fn assistant_tool_message(id: &str, status: model::ToolCallStatus) -> ChatMessage {
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
                terminal_output: Some("x".repeat(1024)),
                monitor_output_tail: Vec::default(),
                monitor_status: None,
                render_epoch: 0,
                layout_epoch: 0,
                last_measured_width: 0,
                last_measured_height: 0,
                last_measured_layout_epoch: 0,
                last_measured_layout_generation: 0,
                last_measured_tools_collapsed: false,
                cache: BlockCache::default(),
                collapsed_override: None,
                last_measured_y_in_msg: 0,
                answered_questions: Vec::new(),
            }))],
        )
    }

    pub(super) fn assistant_bash_tool_message(
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
                terminal_output: Some("x".repeat(1024)),
                monitor_output_tail: Vec::default(),
                monitor_status: None,
                render_epoch: 0,
                layout_epoch: 0,
                last_measured_width: 0,
                last_measured_height: 0,
                last_measured_layout_epoch: 0,
                last_measured_layout_generation: 0,
                last_measured_tools_collapsed: false,
                cache: BlockCache::default(),
                collapsed_override: None,
                last_measured_y_in_msg: 0,
                answered_questions: Vec::new(),
            }))],
        )
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

    /// #310 architectural-fix regression-lock: `invalidate_layout`
    /// must always set `needs_redraw` so callers that invalidate layout
    /// can't silently fail to surface their change. Pre-fix, two
    /// handlers (`toggle_all_tool_calls` + `handle_group_summary_click`)
    /// called `invalidate_layout(MessageChanged)` without setting
    /// `needs_redraw`, leaving the grouping cycle dead in idle state.
    /// The fix makes the invariant "invalidating layout implies
    /// re-rendering" intrinsic to the function.
    #[test]
    fn invalidate_layout_sets_needs_redraw_for_message_changed() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("a"));
        app.needs_redraw = false;
        app.invalidate_layout(InvalidationLevel::MessageChanged(0));
        assert!(
            app.needs_redraw,
            "invalidate_layout(MessageChanged) must set needs_redraw so the next frame renders the invalidated state"
        );
    }

    #[test]
    fn invalidate_layout_sets_needs_redraw_for_messages_from() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("a"));
        app.needs_redraw = false;
        app.invalidate_layout(InvalidationLevel::MessagesFrom(0));
        assert!(app.needs_redraw, "invalidate_layout(MessagesFrom) must set needs_redraw");
    }

    #[test]
    fn invalidate_layout_sets_needs_redraw_for_global() {
        let mut app = make_test_app();
        app.active_messages_mut().push(user_text_message("a"));
        app.needs_redraw = false;
        app.invalidate_layout(InvalidationLevel::Global);
        assert!(app.needs_redraw, "invalidate_layout(Global) must set needs_redraw");
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
