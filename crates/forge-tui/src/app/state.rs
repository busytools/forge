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
    RateLimitIncidentKey, SystemSeverity, TextBlock, TextBlockSpacing, TurnInfo, WelcomeBlock,
    hash_text_block_content, hash_welcome_block_content,
};
pub use tool_call_info::{
    AnsweredQuestion, ToolCallInfo, is_execute_tool_name, is_monitor_tool_name,
};
pub use types::{
    AppStatus, AttentionEntry, AttentionKind, BackgroundTask, ExtraUsage, FailedTurn, HelpView,
    HistoryRetentionPolicy, HistoryRetentionStats, LoginHint, McpState, ModeInfo, ModeState,
    MonitorEntry, MonitorStatus, PasteSessionState, PendingCommandAck, PhaseEntry, PhaseStatus,
    RecentSessionInfo, RenderCacheBudget, ReviewRepliesWaiting, SUBAGENT_TAIL_CAP, ScheduleEntry,
    ScheduleKind, ScrollbarDragState, SelectionKind, SelectionPoint, SelectionState,
    SessionTurnState, SessionUsageState, StopHookEntry, StopHookSummaryState, SubagentChildEntry,
    SubagentEntry, TodoItem, TodoStatus, ToolCallScope, UsageSnapshot, UsageSourceKind, UsageState,
    UsageWindow, WorkflowEntry, WorkflowStatus,
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
use super::input::{InputSnapshot, parse_paste_placeholder_before_cursor};
use super::mention;
use super::plugins::PluginsState;
use super::slash;
use super::subagent;
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

    /// Find the LEAD session bucket whose `cwd_raw` matches `path`.
    /// Used by the launchpad-click and projects-pane-click handlers to
    /// land the user on the resumed bucket for a project.
    ///
    /// Workers spawned via mcp__forge__workers__spawn share the
    /// project's `cwd_raw`, so a naive iter().find() can return a
    /// worker bucket non-deterministically (HashMap order). Cross-
    /// reference workspace.live_workers and exclude any session key
    /// that appears there so the projects-pane click always returns
    /// the lead.
    pub fn find_running_bucket_for_path(&self, path: &str) -> Option<forge_workspace::SessionKey> {
        let worker_keys: std::collections::HashSet<forge_workspace::SessionKey> = self
            .workspace
            .as_ref()
            .map(|ws| ws.all_live_worker_session_keys().into_iter().collect())
            .unwrap_or_default();
        self.sessions
            .iter()
            .find(|(k, s)| s.cwd_raw.as_str() == path && !worker_keys.contains(k))
            .map(|(k, _)| k.clone())
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

    /// Which text editor currently receives typed characters, clipboard
    /// payloads and dictation bursts. Paste routing keys on this rather
    /// than on [`Self::active_view`] so the /diff review editors get the
    /// same treatment as the chat draft.
    pub fn input_focus(&self) -> InputFocus {
        match self.active_view {
            ActiveView::Chat => InputFocus::Chat,
            // Ordering mirrors `diff_overlay::handle_key`: the
            // Finish-review modal draws over the diff and captures keys
            // ahead of any comment editor underneath it.
            ActiveView::Diff => self.diff_overlay.as_ref().map_or(InputFocus::None, |overlay| {
                if overlay.finish_review.is_some() {
                    InputFocus::DiffFinishReview
                } else if overlay.active_input.is_some() {
                    InputFocus::DiffComment
                } else {
                    InputFocus::None
                }
            }),
            ActiveView::Launchpad | ActiveView::Plugins | ActiveView::Mcp | ActiveView::Usage => {
                InputFocus::None
            }
        }
    }

    /// Whether any text editor has focus. `false` means a paste or burst
    /// flush has nowhere to land and must be dropped.
    pub fn has_focused_text_input(&self) -> bool {
        self.input_focus() != InputFocus::None
    }

    /// The focused editor, or `None` when the active view has no text
    /// input open.
    pub fn focused_input(&self) -> Option<&super::input::InputState> {
        match self.input_focus() {
            InputFocus::Chat => Some(self.input()),
            InputFocus::DiffComment => {
                self.diff_overlay.as_ref()?.active_input.as_ref().map(|i| &i.editor)
            }
            InputFocus::DiffFinishReview => {
                self.diff_overlay.as_ref()?.finish_review.as_ref().map(|f| &f.editor)
            }
            InputFocus::None => None,
        }
    }

    /// Mutable companion to [`Self::focused_input`].
    pub fn focused_input_mut(&mut self) -> Option<&mut super::input::InputState> {
        match self.input_focus() {
            InputFocus::Chat => Some(self.input_mut()),
            InputFocus::DiffComment => {
                self.diff_overlay.as_mut()?.active_input.as_mut().map(|i| &mut i.editor)
            }
            InputFocus::DiffFinishReview => {
                self.diff_overlay.as_mut()?.finish_review.as_mut().map(|f| &mut f.editor)
            }
            InputFocus::None => None,
        }
    }

    /// Type one printable character into the focused editor, routing it
    /// through the shared paste-burst detector so a dictation burst
    /// coalesces into a single paste payload rather than a stream of
    /// keystrokes.
    pub fn type_char(&mut self, c: char, now: Instant) -> super::input::TypedChar {
        let action = self.paste_burst.on_char(c, now);
        match self.focused_input_mut() {
            Some(input) => super::input::apply_char_action(input, action, c),
            None => super::input::TypedChar::Buffered,
        }
    }

    /// Map a bucket lifecycle to the App-level status. Every focus
    /// move re-runs this (switch-in, KeyRenamed's active move, the
    /// boot id-adoption), so the mirror tracks the bucket the user
    /// lands on rather than the one they left.
    fn status_for_lifecycle(lifecycle: crate::app::session::SessionLifecycleState) -> AppStatus {
        use crate::app::session::SessionLifecycleState as L;
        match lifecycle {
            L::Spawning => AppStatus::Connecting,
            L::Running => AppStatus::Running,
            L::Sleeping | L::Idle | L::Attention | L::AuthRequired | L::Failed | L::LoggedOut => {
                AppStatus::Ready
            }
        }
    }

    /// Re-derive `App.status` from the active bucket's lifecycle.
    /// `status` is a mirror of the focused bucket, so every path that
    /// moves `active_session_key` owes one call.
    pub(crate) fn refresh_status_from_active_lifecycle(&mut self) {
        let Some(lifecycle) = self.active_session().map(|s| s.lifecycle_state) else {
            return;
        };
        let next = Self::status_for_lifecycle(lifecycle);
        if self.status != next {
            self.status = next;
            self.needs_redraw = true;
        }
    }

    /// Switch which session the renderer reads from. State on both
    /// sides is preserved (in-memory buckets in `sessions`); the
    /// next paint reflects the new active session. No-op if `key`
    /// is already active or unknown; a same-key landing still
    /// re-derives the status mirror.
    ///
    /// Drops any [`Self::pending_spawn_focus`]: landing somewhere by
    /// any route settles where the user wants to be, so a spawn they
    /// asked for earlier must not pull them back when it arrives.
    pub fn switch_active_session(&mut self, key: forge_workspace::SessionKey) {
        // Cleared before the early returns: a click that lands on the
        // session already focused is still the user settling where
        // they want to be.
        self.pending_spawn_focus = None;
        if self.active_session_key.as_ref() == Some(&key) {
            // A same-key landing still settles the mirror: a focus
            // move that skipped re-derivation can leave it stale.
            self.refresh_status_from_active_lifecycle();
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
        tracing::info!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "active_session_switched",
            outcome = "success",
            from = %self.active_session_key.as_ref().map_or("<none>", |k| k.as_str()),
            to = %key.as_str(),
        );

        // `App.status` is derived freshly from the destination
        // bucket's `lifecycle_state` instead of being snapshotted, so
        // a background turn that completed while the user was away
        // doesn't leave a stale `Thinking`/`Running` status on the
        // incoming bucket. Input state lives on each `UiSession`, so
        // switching `active_session_key` naturally swaps the editor
        // - no draft snapshot/restore needed.
        let incoming_lifecycle = self
            .sessions
            .get(&key)
            .map_or(crate::app::session::SessionLifecycleState::Idle, |s| s.lifecycle_state);
        // Switching in IS attending: the incoming chat carries the error
        // block, so drop the attention entry rather than let it reappear
        // when the user switches away again.
        if let Some(bucket) = self.sessions.get_mut(&key) {
            bucket.failed_turn = None;
        }
        self.active_session_key = Some(key);
        self.status = Self::status_for_lifecycle(incoming_lifecycle);
        // Update terminal/tab title immediately on switch so the host
        // terminal reflects the project the user just selected. The
        // render-loop's tab-title call (in `app::run`) only fires
        // every animating frame or on explicit `needs_redraw`
        // transitions; some terminals coalesce/debounce OSC 2 titles
        // when fired close together, so calling here directly with
        // the incoming bucket's cwd guarantees one canonical update
        // per switch.
        crate::app::tab_title::update_tab_title(
            self.shows_activity(),
            self.spinner_frame,
            self.cwd(),
        );
        // Ensure the file index for `@`-mention autocomplete is
        // started for the incoming bucket. Each bucket owns its own
        // `FileIndexState`; if this is the first time we've switched
        // to this bucket the index is empty and needs a fresh scan
        // against the bucket's cwd. `ensure_started` is idempotent:
        // it's a no-op when the bucket's index is already scanning
        // or has a current root matching the cwd.
        crate::app::file_index::ensure_started(self);
        // No explicit git-diff refresh on session switch - the 10s
        // timer (which fires its first tick immediately) catches any
        // stale snapshot on the next pump cycle.
        //
        // Activation parity with the chat-direct path
        // (`forge <project>`). That path lands the user in a fully
        // wired session via `apply_connected_presentation`'s active
        // branch - file index restart, chat focus rebuild, runtime
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
    /// is missing - the production startup path always seeds one,
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
    /// active bucket - finalizing in-flight tool calls to Failed,
    /// pushing system messages - so the user can see what happened.
    /// Removing the bucket here would orphan that work into a
    /// freshly-minted pre-Connect bucket.
    ///
    /// If a synthetic-keyed bucket exists (from an earlier
    /// `install_testing_stub` before `set_session_id` - test ordering),
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
                self.refresh_status_from_active_lifecycle();
                // Mirror `session_id` onto the workspace's
                // DomainSession so `AgentHandle` dispatch (which
                // routes by claude-issued session UUID) finds it.
                // Auto-create a handle-less domain when the workspace
                // doesn't yet have one for `key` - covers the rare
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
            // (failed tool calls, system messages - see doc
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

    /// Whether anything is happening the user should see moving: this
    /// session's turn, a compaction, or live background work in any
    /// session. Drives the spinner clock and the terminal tab title
    /// together, so the two cannot disagree about whether forge is busy.
    pub fn shows_activity(&self) -> bool {
        matches!(
            self.status,
            AppStatus::Connecting
                | AppStatus::CommandPending
                | AppStatus::Thinking
                | AppStatus::Running
        ) || self.is_compacting()
            || self.sessions.values().any(|s| {
                crate::app::session::session_shows_spinner(
                    s.lifecycle_state,
                    s.has_live_background_work(),
                )
            })
            || self.sessions.values().any(|s| {
                s.dictate.is_some()
                    || s.dictate_border
                        .as_ref()
                        .is_some_and(|border| border.animating(Instant::now()))
            })
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
    /// missing - matches the existing infallible-reader pattern
    /// (`viewport()`, `turn_state()`, ...).
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

    /// Park the optimistic `/mode` pre-apply snapshot on the active
    /// session, for the `SetModeFailed` rollback.
    pub fn set_pending_mode_rollback(&mut self, value: Option<crate::app::session::ModeRollback>) {
        self.active_bucket_mut().pending_mode_rollback = value;
    }

    /// The active session's parked optimistic-`/mode` snapshot, if a
    /// switch is awaiting the CLI's verdict.
    pub fn pending_mode_rollback(&self) -> Option<&crate::app::session::ModeRollback> {
        self.active_session().and_then(|s| s.pending_mode_rollback.as_ref())
    }

    /// Restore the active session's parked optimistic-`/mode`
    /// snapshot. Returns false when no snapshot is parked.
    pub fn rollback_pending_mode(&mut self) -> bool {
        self.active_bucket_mut().rollback_pending_mode()
    }

    /// Park the optimistic `/model` pre-apply snapshot on the active
    /// session, for the `SetModelFailed` rollback.
    pub fn set_pending_model_rollback(
        &mut self,
        value: Option<crate::app::session::ModelRollback>,
    ) {
        self.active_bucket_mut().pending_model_rollback = value;
    }

    /// The active session's parked optimistic-`/model` snapshot, if a
    /// switch is awaiting the CLI's verdict.
    pub fn pending_model_rollback(&self) -> Option<&crate::app::session::ModelRollback> {
        self.active_session().and_then(|s| s.pending_model_rollback.as_ref())
    }

    /// Restore the active session's parked optimistic-`/model`
    /// snapshot. Returns false when no snapshot is parked.
    pub fn rollback_pending_model(&mut self) -> bool {
        self.active_bucket_mut().rollback_pending_model()
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

    /// The active tab's forge.toml project name, backing the Inspector
    /// SCHEDULES + GOTIFY snapshots. Resolved through a robust chain so a
    /// missing / lost per-bucket stamp can't blank the section while the
    /// rest of the tab (GIT, PROCESSES, the pane highlight, the top bar)
    /// renders the project fine:
    ///   1. `resolve_active_project_view` on the active KEY - the exact
    ///      resolver the projects pane + top bar use (catalog for a real
    ///      UUID, name for a `__spawn_<name>__` sentinel). Independent of
    ///      the stamp, so it resolves whenever the pane highlights the
    ///      project.
    ///   2. The per-bucket stamp (`UiSession.project`, set at Connect).
    ///   3. `project_name_for_path(cwd_raw)` - resolve from the active
    ///      bucket's cwd, the same value GIT/PROCESSES read successfully.
    pub fn active_project_name(&self) -> Option<String> {
        let active_key = self.active_session_key.as_ref()?;
        if let Some(ws) = self.workspace.as_ref() {
            let projects = ws.list_projects();
            let refs: Vec<&forge_workspace::ProjectView> = projects.iter().collect();
            if let Some(view) =
                crate::ui::projects_pane::resolve_active_project_view(active_key, &refs)
            {
                return Some(view.name.clone());
            }
        }
        if let Some(name) = self.active_session().and_then(|s| s.project.clone()) {
            return Some(name);
        }
        let cwd = self.active_session().map(|s| s.cwd_raw.clone())?;
        self.workspace.as_ref().and_then(|ws| ws.project_name_for_path(&cwd))
    }

    /// Background sessions (everything but the active one) that need
    /// the user: a prompt pending at the head of their queue, a turn
    /// that died, or unread worker answers on their review comments.
    /// [`AttentionEntry`] rows sorted stalest-first (oldest first,
    /// session id as the tiebreaker). The first two mirror the
    /// Projects-pane glyph predicates so those surfaces never disagree.
    /// Empty when nothing needs attention - the Inspector NEEDS
    /// ATTENTION band hides on empty.
    pub fn needs_attention_sessions(&self) -> Vec<AttentionEntry> {
        let active = self.active_session_key.as_ref();
        // Cheap first pass: which background sessions need the user? The
        // common case (nothing waiting) returns here without touching the
        // workspace - no `list_projects` clone, no live-workers lock -
        // since this runs on every inspector render.
        let waiting: Vec<(&forge_workspace::SessionKey, &crate::app::session::UiSession)> = self
            .sessions
            .iter()
            .filter(|(key, session)| active != Some(*key) && session_needs_attention(session))
            .collect();
        if waiting.is_empty() {
            return Vec::new();
        }

        // At least one session is waiting: resolve names / roles. One
        // (project, role) row per live worker so the per-session lookup
        // is a map hit, not a nested per-project scan.
        let projects = self.workspace.as_ref().map(|ws| ws.list_projects()).unwrap_or_default();
        let mut worker_index: HashMap<forge_workspace::SessionKey, (String, String)> =
            HashMap::new();
        if let Some(ws) = self.workspace.as_ref() {
            for project in &projects {
                for worker in ws.list_live_workers(&project.key) {
                    worker_index.insert(
                        worker.session_key.clone(),
                        (project.name.clone(), worker.label.clone()),
                    );
                }
            }
        }
        let project_refs: Vec<&forge_workspace::ProjectView> = projects.iter().collect();

        let mut entries: Vec<AttentionEntry> = Vec::with_capacity(waiting.len());
        for (key, session) in waiting {
            // One row per session. A dead turn outranks a pending prompt:
            // the prompt can no longer be answered (its oneshot died with
            // the turn), and the failure is the signal that must not be
            // missed.
            let (kind, since) = if let Some(failed) = session.failed_turn.as_ref() {
                (
                    AttentionKind::Failed { error: failed.error, status: failed.status },
                    failed.failed_at,
                )
            } else if let Some(prompt) = session.prompt_queue.front() {
                let kind = match &prompt.source {
                    crate::app::prompt::PromptSource::Permission { tool_name, .. } => {
                        AttentionKind::Permission { tool: tool_name.clone() }
                    }
                    crate::app::prompt::PromptSource::Question { .. } => AttentionKind::Question,
                };
                (kind, prompt.enqueued_at)
            } else if let Some(replies) = session.review_replies_waiting.as_ref() {
                (AttentionKind::ReviewReplies { count: replies.count }, replies.since)
            } else {
                continue;
            };
            let (name, role) = if let Some((project_name, label)) = worker_index.get(key) {
                (project_name.clone(), Some(label.clone()))
            } else {
                let name =
                    crate::ui::projects_pane::resolve_active_project_view(key, &project_refs)
                        .map(|view| view.name.clone())
                        .or_else(|| session.project.clone())
                        .unwrap_or_else(|| key.as_str().to_owned());
                (name, None)
            };
            entries.push(AttentionEntry {
                session_key: key.clone(),
                name,
                role,
                kind,
                enqueued_at: since,
            });
        }
        entries.sort_by(|a, b| {
            a.enqueued_at
                .cmp(&b.enqueued_at)
                .then_with(|| a.session_key.as_str().cmp(b.session_key.as_str()))
        });
        entries
    }

    /// Active session's files-accessed counter.
    pub fn files_accessed(&self) -> usize {
        self.active_session().map_or(0, |s| s.files_accessed)
    }

    /// Set the active session's files-accessed counter.
    pub fn set_files_accessed(&mut self, value: usize) {
        self.active_bucket_mut().files_accessed = value;
    }

    /// Retain the classification of the active session's latest
    /// `api_retry`, so a turn error following exhausted retries can name
    /// what killed it.
    pub(crate) fn set_last_api_retry(
        &mut self,
        retry: Option<(forge_primitives::ApiRetryError, Option<u16>)>,
    ) {
        self.active_bucket_mut().last_api_retry = retry;
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

    // ---- Render cache + history retention accessors ----

    /// Active session's running thinking-token estimate for the
    /// in-flight turn (#273). `None` when no `ThinkingTokens` event
    /// has fired yet or the turn just ended.
    pub fn latest_thinking_tokens(&self) -> Option<u64> {
        self.active_session().and_then(|s| s.latest_thinking_tokens)
    }

    /// Set the active session's running thinking-token estimate.
    /// Called by the `Message::ThinkingTokens` reducer; passed
    /// `None` at each turn boundary, which is what keeps one turn's
    /// estimate off the next turn's row.
    pub fn set_latest_thinking_tokens(&mut self, value: Option<u64>) {
        self.active_bucket_mut().latest_thinking_tokens = value;
    }

    /// Start the active session's live turn accounting, so the row
    /// counts from prompt dispatch rather than from the first
    /// assistant frame. A settled message is left alone.
    ///
    /// Resets the thinking accumulator with it. The row's own copy is
    /// wiped by the struct replacement below, and leaving the session
    /// field behind would add an interrupted turn's estimate to the
    /// next one's, since the deltas accumulate rather than overwrite.
    pub fn start_live_turn(&mut self, at: std::time::Instant) {
        self.set_latest_thinking_tokens(None);
        self.active_bucket_mut().live_turn.start(at);
        self.settle_orphaned_turn_rows(at);
        let Some(idx) = self
            .messages()
            .iter()
            .rposition(|m| matches!(m.role, crate::app::MessageRole::Assistant))
        else {
            return;
        };
        if let Some(msg) = self.active_messages_mut().get_mut(idx)
            && !msg.turn_info.is_settled()
        {
            msg.turn_info = crate::app::state::messages::TurnInfo {
                started_at: Some(at),
                ..crate::app::state::messages::TurnInfo::default()
            };
            msg.invalidate_render_cache();
        }
    }

    /// Settle rows still counting from a turn that can no longer
    /// produce a Result of its own - the CLI fuses a cancel-then-type
    /// prompt into the interrupted turn without emitting one, so the
    /// fresh start is the only chance to stop the clock.
    fn settle_orphaned_turn_rows(&mut self, at: std::time::Instant) {
        for msg in self.active_messages_mut() {
            if !matches!(msg.role, crate::app::MessageRole::Assistant) {
                continue;
            }
            let Some(started) = msg.turn_info.started_at else {
                continue;
            };
            if msg.turn_info.is_settled() {
                continue;
            }
            msg.turn_info.duration_ms = Some(
                u64::try_from(at.saturating_duration_since(started).as_millis())
                    .unwrap_or(u64::MAX),
            );
        }
    }

    /// Carry the in-flight turn's bar onto a freshly opened tail
    /// placeholder. A mid-turn submit rides the running turn rather
    /// than starting one, so the clock and the usage/thinking
    /// accumulators keep running; the message that was streaming sheds
    /// the row it held, leaving one bar where the Result will settle.
    ///
    /// A turn whose clock nobody started still starts the bucket clock
    /// here, so the first usage frame cannot restart the row's elapsed
    /// and the settled-row render gate cannot read the stamped row as
    /// not running.
    pub fn continue_live_turn(&mut self, at: std::time::Instant) {
        let bucket_clock = self.active_session().and_then(|s| s.live_turn.started_at);
        let live_started = if let Some(started) = bucket_clock {
            started
        } else {
            self.active_bucket_mut().live_turn.start(at);
            at
        };
        let fresh_row = || crate::app::state::messages::TurnInfo {
            started_at: Some(live_started),
            ..crate::app::state::messages::TurnInfo::default()
        };
        let mut source_idx = None;
        let mut target_idx = None;
        for idx in self
            .messages()
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m.role, crate::app::MessageRole::Assistant))
            .map(|(idx, _)| idx)
        {
            source_idx = target_idx;
            target_idx = Some(idx);
        }
        let Some(target_idx) = target_idx else {
            return;
        };
        // The take waits until the target gate has passed, so a gate
        // failure cannot silently drop the row it just carried.
        if self.active_messages_mut().get(target_idx).is_some_and(|msg| msg.turn_info.is_settled())
        {
            return;
        }
        let carried = source_idx
            .and_then(|idx| self.active_messages_mut().get_mut(idx))
            .filter(|msg| !msg.turn_info.is_settled() && !msg.turn_info.is_empty())
            .map_or_else(fresh_row, |msg| {
                let carried = std::mem::take(&mut msg.turn_info);
                msg.invalidate_render_cache();
                carried
            });
        if let Some(msg) = self.active_messages_mut().get_mut(target_idx) {
            msg.turn_info = carried;
            msg.invalidate_render_cache();
        }
    }

    /// Fold one assistant frame's usage into the live turn, returning
    /// the turn's start and running totals. Starts the turn if nothing
    /// did - cron, auto-continue and peer traffic arrive with it
    /// already under way.
    pub fn record_live_turn_usage(
        &mut self,
        message_id: String,
        usage: crate::app::state::messages::LiveUsage,
    ) -> (Option<std::time::Instant>, Option<crate::app::state::messages::LiveUsage>) {
        let live = &mut self.active_bucket_mut().live_turn;
        if live.started_at.is_none() {
            live.start(std::time::Instant::now());
        }
        live.record(message_id, usage);
        (live.started_at, live.totals())
    }

    /// Close the live turn and return the API time attributable to
    /// it, or `None` when the wire attributed none.
    ///
    /// `Result.duration_api_ms` counts up across the session, so the
    /// turn's figure is the delta; a value below the previous one
    /// means the counter restarted and is already per-turn. A
    /// resulting zero is "not attributed" rather than "took no time" -
    /// the counter is millisecond-granular, so a turn that reached the
    /// API cannot register zero.
    pub fn settle_live_turn(&mut self, duration_api_ms: u64) -> Option<u64> {
        let bucket = self.active_bucket_mut();
        let per_turn = match bucket.prev_duration_api_ms {
            Some(prev) if duration_api_ms >= prev => duration_api_ms - prev,
            _ => duration_api_ms,
        };
        bucket.prev_duration_api_ms = Some(duration_api_ms);
        bucket.live_turn = crate::app::state::messages::LiveTurn::default();
        (per_turn > 0).then_some(per_turn)
    }

    /// Active session's most recent `Message::StopHookSummary`
    /// (#273). Rendered as the collapsed `↳ hook summary · N actions`
    /// surface when `actions > 0`.
    pub fn last_stop_hook_summary(
        &self,
    ) -> Option<&crate::app::state::types::StopHookSummaryState> {
        self.active_session().and_then(|s| s.last_stop_hook_summary.as_ref())
    }

    /// Set the active session's stop-hook summary. Each turn's
    /// `Message::StopHookSummary` overwrites the prior value.
    pub fn set_last_stop_hook_summary(
        &mut self,
        value: Option<crate::app::state::types::StopHookSummaryState>,
    ) {
        self.active_bucket_mut().last_stop_hook_summary = value;
    }

    /// Toggle / set the per-message stop-hook-summary expansion
    /// flag. Default-collapsed; clicking `[▶ expand]` flips to true,
    /// `[▼ collapse]` flips back.
    pub fn toggle_stop_hook_summary_expanded(&mut self, message_idx: usize) {
        let bucket = self.active_bucket_mut();
        let entry = bucket.stop_hook_summary_expanded.entry(message_idx).or_default();
        *entry = !*entry;
    }

    /// Is the stop-hook summary for `message_idx` currently expanded?
    pub fn stop_hook_summary_expanded(&self, message_idx: usize) -> bool {
        self.active_session()
            .and_then(|s| s.stop_hook_summary_expanded.get(&message_idx).copied())
            .unwrap_or(false)
    }

    /// Active session's group collapse level for `id`. Per-group
    /// override wins; absent falls through to the global directive
    /// via `resolve_group_level` (L2Summary when collapsed, L0Bodies
    /// when expanded). Used by mouse handlers, replay tests, and
    /// non-render consumers; the chat render path consults the same
    /// resolver via `MessageRenderContext::group_level`.
    pub fn group_collapse_level(
        &self,
        id: &crate::ui::message::grouping::GroupId,
    ) -> crate::ui::message::grouping::GroupCollapseLevel {
        let per_group =
            self.active_session().and_then(|s| s.group_collapse_levels.get(id).copied());
        crate::ui::collapse::resolve_group_level(per_group, self.tools_collapsed)
    }

    /// Advance the group's collapse level one step (L2 -> L1 -> L0 -> L2).
    /// Returns the new level. Auto-creates the active bucket if missing.
    pub fn cycle_group_collapse_level(
        &mut self,
        id: &crate::ui::message::grouping::GroupId,
    ) -> crate::ui::message::grouping::GroupCollapseLevel {
        let current = self.group_collapse_level(id);
        let next = current.next();
        self.active_bucket_mut().group_collapse_levels.insert(id.clone(), next);
        next
    }

    /// Active session's messaging-group collapse level for `id`.
    /// Per-group override wins; absent falls through to the global
    /// directive via `resolve_group_level` (the same resolver that
    /// drives tool-call groups). Sibling of `group_collapse_level`
    /// keyed on `messaging_group_collapse_levels` so tool-group and
    /// messaging-group leader ids never collide.
    pub fn messaging_group_collapse_level(
        &self,
        id: &crate::ui::message::grouping::GroupId,
    ) -> crate::ui::message::grouping::GroupCollapseLevel {
        let per_group =
            self.active_session().and_then(|s| s.messaging_group_collapse_levels.get(id).copied());
        crate::ui::collapse::resolve_group_level(per_group, self.tools_collapsed)
    }

    /// Advance the messaging-group's collapse level one step
    /// (L2 -> L1 -> L0 -> L2). Returns the new level. Auto-creates
    /// the active bucket if missing.
    pub fn cycle_messaging_group_collapse_level(
        &mut self,
        id: &crate::ui::message::grouping::GroupId,
    ) -> crate::ui::message::grouping::GroupCollapseLevel {
        let current = self.messaging_group_collapse_level(id);
        let next = current.next();
        self.active_bucket_mut().messaging_group_collapse_levels.insert(id.clone(), next);
        next
    }

    /// Active session's MONITOR entries (chat notice +
    /// Inspector MONITORS section both read this).
    pub fn monitors(&self) -> &[crate::app::state::types::MonitorEntry] {
        self.active_session().map_or(&[], |s| s.monitors.as_slice())
    }

    /// Mutable accessor for the active session's
    /// MONITORS list. Auto-creates the pre-Connect bucket if missing.
    pub(crate) fn monitors_mut(&mut self) -> &mut Vec<crate::app::state::types::MonitorEntry> {
        &mut self.active_bucket_mut().monitors
    }

    /// Mutable accessor for the active session's background-task
    /// snapshot. Auto-creates the pre-Connect bucket if missing.
    pub(crate) fn background_tasks_mut(
        &mut self,
    ) -> &mut Vec<crate::app::state::types::BackgroundTask> {
        &mut self.active_bucket_mut().background_tasks
    }

    /// Record a session-scoped `task_id` -> `tool_use_id` at
    /// `task_started`, so a task that outlives its turn stays resolvable
    /// after the turn-scoped map is wiped (see
    /// `UiSession::session_task_tool_use_ids`).
    pub(crate) fn insert_session_task_mapping(&mut self, task_id: String, tool_use_id: String) {
        self.active_bucket_mut().session_task_tool_use_ids.insert(task_id, tool_use_id);
    }

    /// Drop a session-scoped task mapping when the task reaches a
    /// terminal state. No-op when absent.
    pub(crate) fn remove_session_task_mapping(&mut self, task_id: &str) {
        self.active_bucket_mut().session_task_tool_use_ids.remove(task_id);
    }

    /// Settle the open descendants of a backgrounded root that just left
    /// the roster on the active session. See
    /// [`UiSession::settle_children_of`].
    pub(crate) fn settle_departed_root_children(&mut self, root_id: &str) {
        let settled = self.active_bucket_mut().settle_children_of(root_id);
        if settled.is_empty() {
            return;
        }
        let mut changed_messages: Vec<usize> = Vec::new();
        for (msg_idx, block_idx) in &settled {
            self.sync_render_cache_slot(*msg_idx, *block_idx);
            if changed_messages.last() != Some(msg_idx) {
                changed_messages.push(*msg_idx);
            }
        }
        for msg_idx in &changed_messages {
            self.recompute_message_retained_bytes(*msg_idx);
        }
        self.invalidate_message_set(changed_messages);
    }

    /// Clear the active session's background-task registry (and its
    /// task-id mirror) on teardown. See
    /// [`UiSession::clear_background_task_registry`].
    pub(crate) fn clear_active_session_background_task_registry(&mut self) {
        self.active_bucket_mut().clear_background_task_registry();
    }

    /// Mark a tool-use id as a backgrounded agent root on the active
    /// session. See [`UiSession::backgrounded_roots`].
    pub(crate) fn mark_backgrounded_root(&mut self, tool_use_id: String) {
        self.active_bucket_mut().backgrounded_roots.insert(tool_use_id);
    }

    /// Clear one sticky backgrounded root - the terminal
    /// `task_updated` / `task_notification` path.
    pub(crate) fn clear_backgrounded_root(&mut self, tool_use_id: &str) {
        self.active_bucket_mut().backgrounded_roots.remove(tool_use_id);
    }

    /// Active session's SCHEDULES entries (Inspector SCHEDULES
    /// section). Pruned by the ~1s timer tick.
    pub fn schedules(&self) -> &[crate::app::state::types::ScheduleEntry] {
        self.active_session().map_or(&[], |s| s.schedules.as_slice())
    }

    /// Mutable accessor for the active session's SCHEDULES list.
    /// Auto-creates the pre-Connect bucket if missing.
    pub(crate) fn schedules_mut(&mut self) -> &mut Vec<crate::app::state::types::ScheduleEntry> {
        &mut self.active_bucket_mut().schedules
    }

    /// Insert/replace the session's single pending wakeup. The /loop
    /// dynamic-pacing mechanism re-arms each turn so at most one
    /// `Wakeup` entry survives - a new `ScheduleWakeup` tool_use
    /// replaces any prior wakeup regardless of `tool_use_id`. Cron
    /// entries in the same bucket are left untouched.
    pub fn upsert_wakeup_from_tool_input(
        &mut self,
        tool_use_id: &str,
        reason: &str,
        fire_at: std::time::SystemTime,
    ) {
        // #302 redux: wakeups are inherently session-scoped - the
        // /loop dynamic-pacing mechanism re-arms each turn, no
        // `durable` flag exists. The CLI kills every live wakeup at
        // session close, so any wakeup replayed during
        // `load_resume_history` is an orphan. Skip the push so
        // SCHEDULES doesn't surface phantom wakeups post-resume.
        // Live operation is untouched - the /loop re-arm path is
        // replay_in_progress=false. Mirrors the cron orphan-
        // suppression below and #291's monitor pattern at
        // `set_monitor_status`.
        if self.replay_in_progress {
            return;
        }
        let now = std::time::SystemTime::now();
        let schedules = self.schedules_mut();
        schedules.retain(|e| !matches!(e.kind, crate::app::state::types::ScheduleKind::Wakeup));
        schedules.push(crate::app::state::types::ScheduleEntry {
            key: tool_use_id.to_owned(),
            cron_id: None,
            kind: crate::app::state::types::ScheduleKind::Wakeup,
            label: if reason.is_empty() { "wakeup".to_owned() } else { reason.to_owned() },
            description: None,
            schedule: String::new(),
            fire_at: Some(fire_at),
            created_at: now,
        });
    }

    /// Insert/refresh a cron entry from a `CronCreate` tool_use,
    /// keyed by `tool_use_id` until a job id is stamped via
    /// [`Self::stamp_cron_id_from_result`]. Idempotent on re-decode.
    pub fn upsert_cron_from_tool_input(
        &mut self,
        tool_use_id: &str,
        cron_expr: &str,
        prompt: &str,
        recurring: bool,
        created_at: std::time::SystemTime,
    ) {
        // #302 redux: a native cron replayed during
        // `load_resume_history` is an orphan - the CLI reports every
        // `CronCreate` as "Session-only (not written to disk, dies when
        // Claude exits)" regardless of the requested `durable`, so no
        // live counterpart survives the resume and no CronDelete lands
        // in the transcript. Skip the push so SCHEDULES doesn't surface
        // a phantom. Mirrors #291's monitor orphan-suppression at
        // `set_monitor_status` + the wakeup guard above.
        if self.replay_in_progress {
            return;
        }
        let schedule = if cron_expr.is_empty() {
            "(unknown schedule)".to_owned()
        } else {
            crate::ui::schedule_format::humanize_cron(cron_expr)
        };
        // A one-shot fires at the expression's first match after creation,
        // then the CLI auto-deletes it without emitting a CronDelete. That
        // instant is the entry's own expiry (and its live countdown), so
        // resolve it here through the same evaluator the durable crons use.
        // `None` for an unparseable expression - the row is then retained
        // rather than expired against a guess.
        let fire_at = (!recurring)
            .then(|| {
                forge_workspace::next_fire_after(
                    &forge_primitives::cron::CronKind::Recurring(cron_expr.to_owned()),
                    created_at,
                )
            })
            .flatten();
        let label = crate::ui::inspector_pane::first_line(prompt);
        let schedules = self.schedules_mut();
        if let Some(e) = schedules.iter_mut().find(|e| e.key == tool_use_id) {
            e.schedule = schedule;
            e.kind = crate::app::state::types::ScheduleKind::Cron { recurring };
            e.fire_at = fire_at;
            e.label = label;
            return;
        }
        schedules.push(crate::app::state::types::ScheduleEntry {
            key: tool_use_id.to_owned(),
            cron_id: None,
            kind: crate::app::state::types::ScheduleKind::Cron { recurring },
            label,
            description: None,
            schedule,
            fire_at,
            created_at,
        });
    }

    /// Stamp the cron job id (from the `CronCreate` result) onto the
    /// matching entry so a later `CronDelete` can find it. No-op when
    /// the entry has already been stamped or doesn't exist.
    pub fn stamp_cron_id_from_result(&mut self, tool_use_id: &str, job_id: &str) {
        if let Some(e) = self.schedules_mut().iter_mut().find(|e| e.key == tool_use_id)
            && e.cron_id.is_none()
        {
            e.cron_id = Some(job_id.to_owned());
        }
    }

    /// Remove a cron entry whose stamped job id matches `job_id`
    /// (`CronDelete`). No-op when none matches.
    pub fn remove_cron_by_id(&mut self, job_id: &str) {
        self.schedules_mut().retain(|e| e.cron_id.as_deref() != Some(job_id));
    }

    /// Drop schedule entries that are no longer valid at `now`
    /// (passed wakeups, 7-day-expired recurring crons). Called from
    /// the ~1s timer tick.
    pub fn prune_expired_schedules(&mut self, now: std::time::SystemTime) {
        if self.active_session().is_none_or(|s| s.schedules.is_empty()) {
            return;
        }
        self.schedules_mut().retain(|e| !e.is_expired(now));
    }

    /// Recompute the active session's own durable forge-cron snapshot
    /// from the workspace, sorted soonest-first. Called on the ~1s ticker so
    /// the Inspector reads a cheap cached `Vec` instead of resolving the
    /// project + locking the workspace every render. Scopes by the active
    /// tab's stamped project NAME ([`Self::active_project_name`]): the
    /// bucket resolves its project once at Connect, so the per-tick read
    /// never re-derives it from a stale / synthetic / pre-Connect cwd.
    /// Then narrows to the session's own `team_role`, so a lead and its
    /// workers each see only what they can act on.
    /// Empty when the active bucket has no project yet or the session
    /// created no cron. Also humanizes the crons into `forge_schedule_rows`
    /// here (resolving the local timezone once) so the render never pays
    /// that per frame.
    pub fn refresh_forge_crons(&mut self) {
        let own_role = self.active_session_team_role();
        let mut crons = match (self.active_project_name(), self.workspace.as_ref()) {
            (Some(name), Some(ws)) => ws.crons_for_project(&name),
            _ => Vec::new(),
        };
        crons.retain(|c| c.team_role == own_role);
        crons.sort_by_key(|c| c.next_fire);
        // Resolve the local zone (an OS probe) only when there are crons
        // to humanize - most sessions have none.
        self.forge_schedule_rows = if crons.is_empty() {
            Vec::new()
        } else {
            let now = std::time::SystemTime::now();
            let tz = forge_workspace::env::timezone::system_timezone();
            crons
                .iter()
                .map(|c| crate::ui::inspector_pane::forge_cron_to_schedule_entry(c, now, tz))
                .collect()
        };
        self.forge_crons = crons;
    }

    /// Refresh the Gotify snapshot the Inspector GOTIFY section reads:
    /// the active session's own subscriptions (scoped by the active
    /// tab's stamped project NAME then by own `team_role`, like
    /// `refresh_forge_crons`) plus the stream connection status. Called
    /// on the ~1s ticker so the render reads cached fields instead of
    /// locking the workspace each frame.
    pub fn refresh_gotify(&mut self) {
        let own_role = self.active_session_team_role();
        let project = self.active_project_name();
        let Some(ws) = self.workspace.as_ref() else {
            self.gotify_subs = Vec::new();
            self.gotify_connected = false;
            return;
        };
        self.gotify_connected = ws.gotify_connected();
        self.gotify_subs =
            project.map(|name| ws.gotify_subscriptions_for_project(&name)).unwrap_or_default();
        self.gotify_subs.retain(|s| s.team_role == own_role);
    }

    /// The team role that owns the active session: `None` for a project
    /// lead, `Some(label)` for a worker. Scopes the SCHEDULES + GOTIFY
    /// snapshots to what this session created, matching what
    /// `cron__list` / `cron__delete` let it act on.
    ///
    /// Resolved from the live-worker registry, never the sessions
    /// catalog - workers are deliberately absent from the catalog, so a
    /// catalog read reports every worker as a lead.
    fn active_session_team_role(&self) -> Option<String> {
        let ws = self.workspace.as_ref()?;
        let key = self.active_session_key.as_ref()?;
        ws.worker_lookup_for_session(key).map(|(_, label, _)| label)
    }

    /// Insert / update a `MonitorEntry` based on a fresh
    /// `Monitor` tool_use. Idempotent: a matching `tool_use_id`
    /// refreshes the existing entry's input fields without touching
    /// `status` or `output_tail`. Returns true when a new entry was
    /// pushed.
    pub fn upsert_monitor_from_tool_input(
        &mut self,
        tool_use_id: &str,
        description: String,
        command: String,
        persistent: bool,
        timeout_ms: u64,
    ) -> bool {
        // a fresh live Monitor tool_use is `Running`
        // until the wire emits a terminal `task_updated`. But during
        // `load_resume_history` replay the replay walker doesn't pipe
        // terminal `task_updated` events back into the status
        // setter, so a Monitor that was historically completed gets
        // restored as `Running` and stays that way forever - blocking
        // `clear_monitors_if_all_terminal` for legit completed
        // siblings. Restored Monitors land in `Stopped` initially;
        // a terminal `task_updated` arriving later in the same
        // replay walk (or live afterwards) re-flips via
        // `set_monitor_status_by_task_id` to the wire's terminal
        // variant. The setter is gated on the wire's `is_terminal`
        // check at `sdk_message.rs:1116-1141`, so only completed /
        // failed / killed / stopped events drive a re-flip - a
        // `running` event mid-walk does NOT push Stopped back to
        // Running. That's intentional: the value of starting in
        // Stopped is to keep blocked monitors out of the
        // all-terminal-clear predicate; if a historical Monitor
        // genuinely WAS still running at replay time, the next
        // live event resolves it on its own terms.
        // Replay seeds a TERMINAL status so the restored entry stops
        // blocking `clear_monitors_if_all_terminal`. `Completed` rather
        // than `Stopped`: the seed is a placeholder, not a wire signal,
        // and the renderer now paints non-success terminals with a red
        // failure glyph - so seeding `Stopped` would assert a failure we
        // have no evidence for on every monitor in every resumed
        // session. A terminal `task_updated` later in the same replay
        // walk re-flips it to whatever actually happened.
        let initial_status = if self.replay_in_progress {
            crate::app::state::types::MonitorStatus::Completed
        } else {
            crate::app::state::types::MonitorStatus::Running
        };
        let monitors = self.monitors_mut();
        if let Some(existing) = monitors.iter_mut().find(|m| m.tool_use_id == tool_use_id) {
            existing.description = description;
            existing.command = command;
            existing.persistent = persistent;
            existing.timeout_ms = timeout_ms;
            return false;
        }
        monitors.push(crate::app::state::types::MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: None,
            description,
            command,
            persistent,
            timeout_ms,
            status: initial_status,
            output_file: None,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        });
        true
    }

    /// Stamp the `task_id` discovered from the Monitor's
    /// `tool_use_result` (or from `TaskStarted` mapping). No-op when
    /// no matching entry exists or the entry already has a task_id.
    pub fn stamp_monitor_task_id(&mut self, tool_use_id: &str, task_id: String) {
        if let Some(entry) = self.monitors_mut().iter_mut().find(|m| m.tool_use_id == tool_use_id)
            && entry.task_id.is_none()
        {
            entry.task_id = Some(task_id);
        }
    }

    /// Transition the matching Monitor entry to a terminal status,
    /// keyed by the wire `task_id`. Used by lifecycle event handlers
    /// that carry the task_id (e.g. wire `TaskUpdated`). The
    /// all-completed predicate is no longer run here; #277 Bug 5a
    /// deferred that trigger to `handle_task_notification` so the
    /// `task_updated terminal -> task_notification with output_file`
    /// wire ordering can stamp the tail before the entry gets
    /// drained. Callers that mutate status without going through
    /// `handle_task_notification` should call
    /// `clear_monitors_if_all_terminal` themselves if they need
    /// the auto-clear behaviour.
    pub fn set_monitor_status_by_task_id(
        &mut self,
        task_id: &str,
        status: crate::app::state::types::MonitorStatus,
    ) {
        let Some(entry) =
            self.monitors_mut().iter_mut().find(|m| m.task_id.as_deref() == Some(task_id))
        else {
            return;
        };
        entry.status = status;
        let tool_use_id = entry.tool_use_id.clone();
        self.stamp_monitor_status_on_tool_call(&tool_use_id, status);
    }

    /// Mirror a monitor's liveness onto its chat block. The block reads
    /// this rather than `ToolCallInfo::status`, which the "Monitor
    /// started" ack drives terminal while the monitor is still alive.
    fn stamp_monitor_status_on_tool_call(
        &mut self,
        tool_use_id: &str,
        status: crate::app::state::types::MonitorStatus,
    ) {
        let Some((msg_idx, block_idx)) = self.lookup_tool_call(tool_use_id) else {
            return;
        };
        let Some(MessageBlock::ToolCall(tc)) =
            self.active_messages_mut().get_mut(msg_idx).and_then(|m| m.blocks.get_mut(block_idx))
        else {
            return;
        };
        if tc.monitor_status == Some(status) {
            return;
        }
        tc.monitor_status = Some(status);
        tc.mark_tool_call_layout_dirty();
        self.invalidate_lifecycle_block_height(msg_idx, block_idx);
    }

    /// Finish a lifecycle-block mutation the way the backgrounded-`Bash`
    /// stream does (`app::terminal`): marking the tool dirty rebuilds
    /// the render, but the viewport keeps its own prefix-sum of message
    /// heights and this block's height swings as the tail fills and
    /// again when it collapses.
    fn invalidate_lifecycle_block_height(&mut self, msg_idx: usize, block_idx: usize) {
        self.sync_render_cache_slot(msg_idx, block_idx);
        self.recompute_message_retained_bytes(msg_idx);
        self.invalidate_message_set(std::iter::once(msg_idx));
    }

    /// Liveness of the monitor owning `tool_use_id`, read at
    /// `ToolCallInfo` construction. `None` when no entry matches.
    pub fn monitor_status_for_tool_use(
        &self,
        tool_use_id: &str,
    ) -> Option<crate::app::state::types::MonitorStatus> {
        self.monitors().iter().find(|m| m.tool_use_id == tool_use_id).map(|m| m.status)
    }

    /// Stamp the `output_file` path on the matching
    /// Monitor entry. The CLI carries this via
    /// `task_notification.output_file`. Idempotent: same path
    /// overwrites cleanly so repeated `task_notification` events
    /// don't drift the entry's source-of-truth.
    pub fn set_monitor_output_file_by_task_id(&mut self, task_id: &str, path: std::path::PathBuf) {
        if let Some(entry) =
            self.monitors_mut().iter_mut().find(|m| m.task_id.as_deref() == Some(task_id))
        {
            entry.output_file = Some(path);
        }
    }

    /// REPLACE the matching Monitor's `output_tail`
    /// with the supplied lines (typically the most-recent N lines
    /// of its `output_file`). The file is authoritative - the
    /// renderer's tail must match the file, not accumulate stale
    /// entries from prior events. No-op if no entry matches.
    ///
    /// Also stamps the last 5 lines onto the matching `ToolCallInfo`'s
    /// `monitor_output_tail` and marks the tool call's layout dirty so
    /// the in-chat live block re-renders in place - but only when that
    /// rendered tail actually changed, so a timer-polled Monitor with no
    /// new output doesn't churn the cache. Mirrors the
    /// `apply_terminal_payload` precedent in `terminal.rs` (terminal
    /// stream + dirty bump).
    pub fn replace_monitor_output_tail_by_task_id(&mut self, task_id: &str, lines: &[String]) {
        const CHAT_TAIL_MAX: usize = 5;
        // First update the per-session MonitorEntry. Capture the
        // tool_use_id so the chat-tail stamp below can find the
        // matching ToolCallInfo through `tool_call_index`.
        let tool_use_id = {
            let Some(entry) =
                self.monitors_mut().iter_mut().find(|m| m.task_id.as_deref() == Some(task_id))
            else {
                return;
            };
            entry.output_tail = lines.iter().cloned().collect();
            entry.tool_use_id.clone()
        };
        // Slice the last 5 lines for the chat block. Skip the
        // `lookup_tool_call` -> `messages_mut` walk when the bucket
        // doesn't carry that tool_use_id yet (the ToolCall block
        // arrives via `handle_tool_call` and indexing happens slightly
        // after); the next refresh tick re-stamps once indexed.
        let last_five: Vec<String> = if lines.len() <= CHAT_TAIL_MAX {
            lines.to_vec()
        } else {
            lines[lines.len() - CHAT_TAIL_MAX..].to_vec()
        };
        let Some((msg_idx, block_idx)) = self.lookup_tool_call(&tool_use_id) else {
            return;
        };
        let Some(MessageBlock::ToolCall(tc)) =
            self.active_messages_mut().get_mut(msg_idx).and_then(|m| m.blocks.get_mut(block_idx))
        else {
            return;
        };
        // A timer-polled Monitor with no new output re-runs this path; only
        // re-stamp + invalidate when the rendered chat tail actually changed.
        if tc.monitor_output_tail == last_five {
            return;
        }
        tc.monitor_output_tail = last_five;
        tc.mark_tool_call_layout_dirty();
        self.invalidate_lifecycle_block_height(msg_idx, block_idx);
    }

    /// Read the matching Monitor's stored `output_file`
    /// and refresh its `output_tail` with the last
    /// `MonitorEntry::OUTPUT_TAIL_MAX` lines. Called on each
    /// `task_notification` / `task_progress` event for the monitor.
    /// Silently no-ops when:
    /// - the matching entry has no stored `output_file` yet
    ///   (Monitor just started, hasn't received its first
    ///   `task_notification` with the path)
    /// - the helper returns `None` (file missing / permission denied
    ///   / IO error - the helper logs the WARN; we preserve the
    ///   prior tail)
    pub fn refresh_monitor_output_tail_from_file(&mut self, task_id: &str) {
        let path = self
            .monitors()
            .iter()
            .find(|m| m.task_id.as_deref() == Some(task_id))
            .and_then(|m| m.output_file.clone());
        let Some(path) = path else {
            return;
        };
        if let Some(lines) = crate::app::monitor_output::read_output_file_tail(
            &path,
            crate::app::state::types::MonitorEntry::OUTPUT_TAIL_MAX,
        ) {
            self.replace_monitor_output_tail_by_task_id(task_id, &lines);
        }
    }

    /// Drain the MONITORS list once every entry has transitioned out of
    /// `Running`. Matches the TODOs all-completed auto-clear shape so
    /// the Inspector section drops out entirely. Called explicitly from
    /// `handle_task_notification` rather than implicitly from
    /// `set_monitor_status_by_task_id`, so the
    /// `task_updated terminal -> task_notification with output_file`
    /// wire ordering can stamp the tail before the entry gets drained.
    pub fn clear_monitors_if_all_terminal(&mut self) {
        let monitors = self.monitors_mut();
        if !monitors.is_empty() && monitors.iter().all(|m| !m.is_running()) {
            monitors.clear();
        }
    }

    /// Active session's WORKFLOW entries.
    pub fn workflows(&self) -> &[crate::app::state::types::WorkflowEntry] {
        self.active_session().map_or(&[], |s| s.workflows.as_slice())
    }

    /// Whether `subagents_view` would return anything, without building it.
    /// Short-circuits on the first live root instead of indexing every tool
    /// call in the session. Root derivation mirrors `subagents_view`,
    /// including the unscoped parents of registered child scopes (#808).
    pub fn has_active_subagent_root(&self) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        // No scopes at all means no roots and no child frames, so skip
        // the message walk entirely - the common case for a session that
        // never dispatched one.
        if session.tool_call_scopes.is_empty() {
            return false;
        }
        let backgrounded_alive = session.backgrounded_alive_tool_use_ids();
        // Parents named by child scopes; the ones carrying no scope of
        // their own are root candidates alongside registered roots.
        let mut referenced_parents: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for scope in session.tool_call_scopes.values() {
            if let crate::app::state::types::ToolCallScope::SubagentChild { parent_tool_use_id } =
                scope
            {
                referenced_parents.insert(parent_tool_use_id.as_str());
            }
        }
        // First occurrence of an id wins, mirroring the `by_id` index the
        // view builds, so a duplicate cannot revive a drained root.
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut open_child_parents: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for msg in &session.messages {
            for block in &msg.blocks {
                let crate::app::MessageBlock::ToolCall(tc) = block else {
                    continue;
                };
                if !seen.insert(tc.id.as_str()) {
                    continue;
                }
                let id = tc.id.as_str();
                let is_open = matches!(
                    tc.status,
                    crate::agent::model::ToolCallStatus::InProgress
                        | crate::agent::model::ToolCallStatus::Pending
                );
                let scope = session.tool_call_scopes.get(id);
                let is_root =
                    matches!(scope, Some(crate::app::state::types::ToolCallScope::SubagentRoot))
                        || (scope.is_none() && referenced_parents.contains(id));
                if is_root && (backgrounded_alive.contains(id) || is_open) {
                    return true;
                }
                if is_open
                    && let Some(crate::app::state::types::ToolCallScope::SubagentChild {
                        parent_tool_use_id,
                    }) = scope
                {
                    open_child_parents.insert(parent_tool_use_id.as_str());
                }
            }
        }
        // A root kept open only by its children (#808): the parent must
        // be among the walked cards, and either a registered root or an
        // unscoped parent candidate.
        open_child_parents.iter().any(|parent| {
            seen.contains(*parent)
                && match session.tool_call_scopes.get(*parent) {
                    Some(crate::app::state::types::ToolCallScope::SubagentRoot) => true,
                    Some(_) => false,
                    None => referenced_parents.contains(parent),
                }
        })
    }

    /// Active-session SUBAGENTS Inspector view. Derives one entry
    /// per `Task` / `Agent` dispatch (a visible root) plus a tail of
    /// the last `SUBAGENT_TAIL_CAP` `SubagentChild` tool calls under
    /// each root, identified via `parent_tool_use_id` on the
    /// scope-registered map. Returns an empty Vec when every root is
    /// terminal AND absent from the session roster - mirrors
    /// `clear_workflows_if_all_terminal` so the section auto-clears.
    /// Pure derive over `UiSession` state; no mutation, no new wire
    /// surface.
    pub fn subagents_view(&self) -> Vec<crate::app::state::types::SubagentEntry> {
        let Some(session) = self.active_session() else {
            return Vec::new();
        };

        // Index every tool call by id and remember the registered
        // scope. Walking each message linearly preserves block order,
        // which is what feeds the chronological tail later.
        let mut by_id: std::collections::HashMap<&str, &crate::app::ToolCallInfo> =
            std::collections::HashMap::new();
        let mut ordered_tool_ids: Vec<&str> = Vec::new();
        for msg in &session.messages {
            for block in &msg.blocks {
                if let crate::app::MessageBlock::ToolCall(tc) = block
                    && !by_id.contains_key(tc.id.as_str())
                {
                    by_id.insert(tc.id.as_str(), tc.as_ref());
                    ordered_tool_ids.push(tc.id.as_str());
                }
            }
        }

        // Children per parent id from the registered child scopes; a
        // parent is keyed only when its own card is in the index.
        let mut children_by_parent: std::collections::HashMap<
            &str,
            Vec<&crate::app::ToolCallInfo>,
        > = std::collections::HashMap::new();
        for id in &ordered_tool_ids {
            let Some(tc) = by_id.get(id) else { continue };
            if let Some(crate::app::state::types::ToolCallScope::SubagentChild {
                parent_tool_use_id,
            }) = self.tool_call_scope(id)
                && let Some((&parent_key, _)) = by_id.get_key_value(parent_tool_use_id.as_str())
            {
                // The parent's id is in the registered scope - copy a
                // stable str borrow off the indexed map (its keys
                // outlive the children vec).
                children_by_parent.entry(parent_key).or_default().push(tc);
            }
        }
        // A resumed agent's replayed Task card carries no scope (resume
        // registers none), but its live child frames still name the
        // card - such a parent is a root too (#808).
        let unscoped_parents: std::collections::HashSet<&str> = children_by_parent
            .keys()
            .filter(|id| self.tool_call_scope(id).is_none())
            .copied()
            .collect();
        // Roots in dispatch order, scoped and unscoped alike.
        let mut roots: Vec<&crate::app::ToolCallInfo> = Vec::new();
        for id in &ordered_tool_ids {
            let scope = self.tool_call_scope(id);
            let is_root =
                matches!(scope, Some(crate::app::state::types::ToolCallScope::SubagentRoot))
                    || (scope.is_none() && unscoped_parents.contains(*id));
            if is_root && let Some(tc) = by_id.get(id) {
                roots.push(tc);
            }
        }

        // Liveness follows the task's real lifecycle, not the turn. The CLI
        // backgrounds a subagent with an immediate sentinel tool_result that
        // flips its root card terminal while the task keeps running, and its
        // spawning turn Results before it finishes - so `status` alone is
        // unreliable and the turn-scoped alive set is wiped underneath it.
        // The durable signal is the session roster (`background_tasks`
        // INTERSECT the session task map), which survives turn finalisation
        // and covers every backgrounded kind. A genuinely running
        // non-backgrounded root still surfaces via its own in-flight status.
        let backgrounded_alive = session.backgrounded_alive_tool_use_ids();
        // A root kept open by its children: a resumed agent's own card is
        // terminal from the replay, so an open child under it is the only
        // running evidence (#808).
        let open_child_roots: std::collections::HashSet<&str> = children_by_parent
            .iter()
            .filter_map(|(parent, children)| {
                children
                    .iter()
                    .any(|c| {
                        matches!(
                            c.status,
                            crate::agent::model::ToolCallStatus::InProgress
                                | crate::agent::model::ToolCallStatus::Pending
                        )
                    })
                    .then_some(*parent)
            })
            .collect();
        let root_is_active = |root: &&crate::app::ToolCallInfo| {
            backgrounded_alive.contains(root.id.as_str())
                || open_child_roots.contains(root.id.as_str())
                || matches!(
                    root.status,
                    crate::agent::model::ToolCallStatus::InProgress
                        | crate::agent::model::ToolCallStatus::Pending
                )
        };
        // Auto-clear: the section disappears only once no root is still
        // active (every root both terminal-status AND drained from the
        // alive set). Empty `roots` already gates via `is_empty`.
        if !roots.is_empty() && !roots.iter().any(root_is_active) {
            return Vec::new();
        }

        let cap = crate::app::state::types::SUBAGENT_TAIL_CAP;
        roots
            .into_iter()
            .map(|root| {
                let children = children_by_parent.remove(root.id.as_str()).unwrap_or_default();
                let total_count = children.len();
                // Alive-but-terminal roots (backgrounded) render running; a
                // still-`Pending` root stays queued rather than spinning.
                let running = root_is_active(&root)
                    && root.status != crate::agent::model::ToolCallStatus::Pending;
                let status = if running {
                    crate::agent::model::ToolCallStatus::InProgress
                } else {
                    root.status
                };
                let tail = if running {
                    let tail_start = children.len().saturating_sub(cap);
                    children[tail_start..]
                        .iter()
                        .map(|c| crate::app::state::types::SubagentChildEntry {
                            sdk_tool_name: c.sdk_tool_name.clone(),
                            title: c.title.clone(),
                            status: c.status,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                crate::app::state::types::SubagentEntry {
                    tool_use_id: root.id.clone(),
                    label: subagent_label_from_root(root),
                    status,
                    tail,
                    total_count,
                }
            })
            .collect()
    }

    /// Mutable accessor for the active session's
    /// WORKFLOWS list. Auto-creates the pre-Connect bucket if
    /// missing.
    pub(crate) fn workflows_mut(&mut self) -> &mut Vec<crate::app::state::types::WorkflowEntry> {
        &mut self.active_bucket_mut().workflows
    }

    /// Insert / refresh a `WorkflowEntry` from a
    /// `Workflow` tool_use's parsed input. Idempotent: a matching
    /// `tool_use_id` refreshes `meta_name` / `meta_description`
    /// without touching `phases` / `status`. Returns true on new
    /// insertion.
    pub fn upsert_workflow_from_tool_input(
        &mut self,
        tool_use_id: &str,
        meta_name: String,
        meta_description: Option<String>,
    ) -> bool {
        // Replay seeds the terminal status, matching
        // `upsert_monitor_from_tool_input`. Nothing can move a replayed
        // entry off its seeded status: the resume walk is fed by
        // `synthesize_replay_messages`, which emits only User /
        // Assistant envelopes, so neither `TaskProgress` nor
        // `TaskUpdated` reaches the walk. Seeded `InProgress` a
        // historical workflow would read as running forever and hold
        // the WORKFLOWS section open for its completed siblings.
        let initial_status = if self.replay_in_progress {
            crate::app::state::types::WorkflowStatus::Completed
        } else {
            crate::app::state::types::WorkflowStatus::InProgress
        };
        let workflows = self.workflows_mut();
        if let Some(existing) = workflows.iter_mut().find(|w| w.tool_use_id == tool_use_id) {
            existing.meta_name = meta_name;
            existing.meta_description = meta_description;
            return false;
        }
        workflows.push(crate::app::state::types::WorkflowEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: None,
            meta_name,
            meta_description,
            phases: Vec::new(),
            status: initial_status,
            final_result_summary: None,
            expanded_in_inspector: false,
        });
        true
    }

    /// Stamp `task_id` on a workflow entry (from
    /// `TaskStarted`'s task_id ↔ tool_use_id mapping). No-op when
    /// no entry matches or the entry already has a task_id.
    pub fn stamp_workflow_task_id(&mut self, tool_use_id: &str, task_id: String) {
        if let Some(entry) = self.workflows_mut().iter_mut().find(|w| w.tool_use_id == tool_use_id)
            && entry.task_id.is_none()
        {
            entry.task_id = Some(task_id);
        }
    }

    /// Apply a `workflow_progress` snapshot to the
    /// matching workflow (keyed by `task_id`). The wire snapshot is
    /// monotonic (start → progress → done), so the latest event
    /// authoritatively determines each phase's status.
    pub fn apply_workflow_progress_by_task_id(
        &mut self,
        task_id: &str,
        events: &[forge_primitives::WorkflowProgressEvent],
    ) {
        if let Some(entry) =
            self.workflows_mut().iter_mut().find(|w| w.task_id.as_deref() == Some(task_id))
        {
            entry.apply_workflow_progress(events);
        }
        self.clear_workflows_if_all_terminal();
    }

    /// Transition a workflow into the terminal
    /// `Completed` status (called from `TaskUpdated` terminal
    /// patch). Triggers the all-completed clear.
    pub fn set_workflow_completed_by_task_id(&mut self, task_id: &str) {
        if let Some(entry) =
            self.workflows_mut().iter_mut().find(|w| w.task_id.as_deref() == Some(task_id))
        {
            entry.status = crate::app::state::types::WorkflowStatus::Completed;
        }
        self.clear_workflows_if_all_terminal();
    }

    /// Drain the WORKFLOWS list once every entry has finished -
    /// matches the MONITORS / TODOs all-completed clear shape.
    pub fn clear_workflows_if_all_terminal(&mut self) {
        let workflows = self.workflows_mut();
        if !workflows.is_empty() && workflows.iter().all(|w| !w.is_in_progress()) {
            workflows.clear();
        }
    }

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
            let cursor = self
                .focused_input()
                .map(|input| SelectionPoint { row: input.cursor_row(), col: input.cursor_col() });
            let continued_session = self.active_paste_session().copied().and_then(|session| {
                let input = self.focused_input()?;
                let current_line = input.lines().get(input.cursor_row())?;
                let idx = parse_paste_placeholder_before_cursor(current_line, input.cursor_col())?;
                (session.placeholder_index == Some(idx)).then_some(session)
            });
            let opened = continued_session.unwrap_or_else(|| {
                let id = self.allocate_paste_session_id();
                PasteSessionState {
                    id,
                    start: cursor.unwrap_or(SelectionPoint { row: 0, col: 0 }),
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

    /// Returns `(label, value)` for the welcome message's account
    /// line. The line's *layout slot* is reserved from the first
    /// frame in workspace mode - `Account: ...` shows immediately,
    /// then the value fills in once data lands. Avoids the
    /// alternative options (line pops in late, or flickers
    /// `Gateway` → `Gateway · team`) that surface as stale UI.
    ///
    /// Resolution table:
    /// - Workspace mode + both pieces → `"Account: name · tier"`.
    /// - Workspace mode + partial/no data → `"Account: ..."` skeleton.
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
            (true, _, _) => ("Account".to_owned(), "\u{2026}".to_owned()),
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
        // welcome banner - the launchpad version line still shows
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

    /// Whether a tool call's card is still non-terminal. Independent
    /// evidence that a backgrounded task is alive, for the window where
    /// the roster has not caught up.
    fn tool_call_is_open(&self, id: &str) -> bool {
        self.lookup_tool_call(id)
            .and_then(|(mi, bi)| self.messages().get(mi)?.blocks.get(bi))
            .is_some_and(|block| match block {
                MessageBlock::ToolCall(tc) => matches!(
                    tc.status,
                    model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                ),
                _ => false,
            })
    }

    /// The positive form: the card exists AND has reached a terminal
    /// status. An id with no card in the message list has no evidence
    /// either way and reads as not settled (#791).
    fn tool_call_is_settled(&self, id: &str) -> bool {
        self.lookup_tool_call(id)
            .and_then(|(mi, bi)| self.messages().get(mi)?.blocks.get(bi))
            .is_some_and(|block| match block {
                MessageBlock::ToolCall(tc) => !matches!(
                    tc.status,
                    model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                ),
                _ => false,
            })
    }

    pub fn clear_tool_scope_tracking(&mut self) {
        // Preserve scope tracking for still-running backgrounded roots and
        // their children so a backgrounded subagent stays identifiable in
        // SUBAGENTS across turn boundaries; a blanket clear made it vanish
        // until its next child re-registered the scope.
        // `background_tasks_changed` can land a frame after the `Result`,
        // so this read can see an empty roster for a subagent that is
        // running, and nothing re-registers a dropped scope. Closing that
        // needs a durable was-backgrounded signal (#790).
        let alive = self
            .active_session()
            .map(super::session::UiSession::backgrounded_alive_with_children)
            .unwrap_or_default();
        let open_roots: HashSet<String> = self
            .tool_call_scopes()
            .iter()
            .filter(|(_, scope)| {
                matches!(scope, crate::app::state::types::ToolCallScope::SubagentRoot)
            })
            .map(|(id, _)| id.clone())
            .filter(|id| self.active_task_ids().contains(id) || self.tool_call_is_open(id))
            .collect();
        let dropped_while_open: Vec<String> =
            open_roots.iter().filter(|id| !alive.contains(id.as_str())).map(Clone::clone).collect();
        for id in &dropped_while_open {
            tracing::warn!(
                target: crate::logging::targets::APP_TOOL,
                event_name = "subagent_root_dropped_while_open",
                message = "dropping a subagent root's scope while its card is still open; it will not be re-registered and SUBAGENTS loses it",
                outcome = "dropped",
                tool_call_id = %id,
            );
        }
        // A child whose own card is terminal cannot be swept into anything,
        // so holding its scope only grows the map with the subagent's total
        // tool-call count (#791). A live grandchild behind such a child is
        // still spared: a terminal-yet-running nested Task carries its own
        // roster row and is a live root in `alive` itself.
        let settled_children: HashSet<String> = self
            .tool_call_scopes()
            .iter()
            .filter(|(id, scope)| {
                matches!(scope, crate::app::state::types::ToolCallScope::SubagentChild { .. })
                    && self.tool_call_is_settled(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.tool_call_scopes_mut().retain(|id, scope| match scope {
            crate::app::state::types::ToolCallScope::SubagentRoot => alive.contains(id.as_str()),
            crate::app::state::types::ToolCallScope::SubagentChild { parent_tool_use_id } => {
                alive.contains(parent_tool_use_id.as_str()) && !settled_children.contains(id)
            }
            crate::app::state::types::ToolCallScope::MainAgent => false,
        });
        self.active_task_ids_mut().clear();
    }

    /// Look up the (`message_index`, `block_index`) for a tool call ID.
    pub fn lookup_tool_call(&self, id: &str) -> Option<(usize, usize)> {
        self.tool_call_index().get(id).copied()
    }

    /// Stamp a resolved answer onto an AskUserQuestion tool call: append
    /// the question -> answer pair, un-hide it (it was chat-suppressed
    /// while the dock prompt was live), and invalidate its render so the
    /// answered-card paints. No-op when the tool call isn't found (e.g.
    /// the session switched between prompt and answer).
    pub(crate) fn record_answered_question(&mut self, tool_id: &str, answered: AnsweredQuestion) {
        let Some((mi, bi)) = self.lookup_tool_call(tool_id) else {
            return;
        };
        if let Some(MessageBlock::ToolCall(tc)) =
            self.active_messages_mut().get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
        {
            let tc = tc.as_mut();
            tc.answered_questions.push(answered);
            tc.hidden = false;
            tc.mark_tool_call_render_dirty();
            tc.mark_tool_call_layout_dirty();
        }
    }

    /// Register a tool call's position in the message/block arrays.
    pub fn index_tool_call(&mut self, id: String, msg_idx: usize, block_idx: usize) {
        self.active_tool_call_index_mut().insert(id, (msg_idx, block_idx));
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

    /// Force-finish any lingering in-progress tool calls.
    /// Returns the number of tool calls that were transitioned.
    ///
    /// A live backgrounded subagent is exempt, root and children alike:
    /// it outlives the turn and settles via its own `task_updated`.
    pub fn finalize_in_progress_tool_calls(&mut self, new_status: model::ToolCallStatus) -> usize {
        let mut changed = 0usize;
        let mut changed_message_indices = Vec::new();
        let mut changed_slots = Vec::new();
        // Open calls first, so liveness is answered per call - O(depth)
        // each - instead of deriving the eager exempt set off the whole
        // scope map (#793).
        let open_ids: Vec<String> = self
            .messages()
            .iter()
            .flat_map(|msg| &msg.blocks)
            .filter_map(|block| match block {
                MessageBlock::ToolCall(tc)
                    if matches!(
                        tc.status,
                        model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                    ) =>
                {
                    Some(tc.id.clone())
                }
                _ => None,
            })
            .collect();
        let exempt: std::collections::HashSet<&str> = open_ids
            .iter()
            .filter(|id| {
                self.active_session()
                    .is_some_and(|session| session.is_backgrounded_alive_or_descendant(id))
            })
            .map(String::as_str)
            .collect();

        for (msg_idx, msg) in self.active_messages_mut().iter_mut().enumerate() {
            for (block_idx, block) in msg.blocks.iter_mut().enumerate() {
                if let MessageBlock::ToolCall(tc) = block {
                    let tc = tc.as_mut();
                    if matches!(
                        tc.status,
                        model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                    ) && !exempt.contains(tc.id.as_str())
                    {
                        tc.status = new_status;
                        tc.mark_tool_call_layout_dirty();
                        changed_slots.push((msg_idx, block_idx));
                        // A completed execute's captured terminal id no
                        // longer means anything to the renderer.
                        if tc.is_execute_tool() {
                            tc.terminal_id = None;
                        }
                        if changed_message_indices.last().copied() != Some(msg_idx) {
                            changed_message_indices.push(msg_idx);
                        }
                        changed += 1;
                    }
                }
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

        tracing::debug!(
            target: crate::logging::targets::APP_TOOL,
            event_name = "tool_call_sweep",
            message = "swept open tool calls at a turn boundary",
            outcome = "success",
            sweep_site = "submit_or_turn_exit",
            new_status = ?new_status,
            count = changed,
            exempt_count = exempt.len(),
        );
        changed
    }

    /// Clear runtime-only turn tracking while preserving the message history itself.
    pub fn finalize_turn_runtime_artifacts(&mut self, new_status: model::ToolCallStatus) {
        let _ = self.finalize_in_progress_tool_calls(new_status);
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

    /// Open a fresh assistant turn: push an empty assistant placeholder
    /// at the tail and bind the active-turn pointer onto it. Shared by
    /// the typed-submit (`input_submit::dispatch_prompt`) and
    /// delivered-prompt (`sdk_message::push_peer_envelope_user_turn_if_present`)
    /// turn-open paths so the thinking spinner pins to the new tail
    /// placeholder in both - the pointer is what `chat::msg_spinner`
    /// reads to decide which message wears the spinner.
    pub(crate) fn push_active_turn_assistant_placeholder(&mut self) {
        self.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new()));
        self.bind_active_turn_assistant_to_tail();
    }

    /// Keep the thinking spinner anchored while a turn is running: bind onto
    /// an empty assistant tail (a genuine in-flight placeholder), else open a
    /// fresh placeholder.
    pub(crate) fn ensure_running_turn_spinner_anchor(&mut self) {
        if !matches!(self.status, AppStatus::Thinking | AppStatus::Running) {
            return;
        }
        if self.active_turn_assistant_idx().is_some() {
            return;
        }
        let tail_is_empty_assistant = self
            .messages()
            .last()
            .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant) && msg.blocks.is_empty());
        if tail_is_empty_assistant {
            self.bind_active_turn_assistant_to_tail();
        } else {
            self.push_active_turn_assistant_placeholder();
        }
    }

    /// Drop a trailing empty assistant placeholder if the tail is one. A
    /// prior turn-open (typed or delivered) may have pushed a placeholder
    /// that never received tokens; stripping it before the next user
    /// bubble keeps rapid-fire turns from stranding blank assistant
    /// bubbles between them. Shared by the typed-submit and
    /// delivered-prompt turn-open paths.
    pub(crate) fn strip_trailing_empty_assistant_placeholder(&mut self) {
        let Some(tail_idx) = self.messages().len().checked_sub(1) else {
            return;
        };
        let tail_is_empty_asst = self
            .messages()
            .get(tail_idx)
            .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant) && msg.blocks.is_empty());
        if tail_is_empty_asst {
            let _ = self.remove_message_tracked(tail_idx);
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
        self.set_observed_assistant_model(None);
        self.set_mode(None);
        self.set_runtime_session_state(None);
        self.set_observed_permission_mode(None);
        self.set_observed_effort(None);
        self.set_pending_mode_rollback(None);
        self.set_pending_model_rollback(None);
        *self.session_usage_mut() = SessionUsageState::default();
        let bucket = self.active_bucket_mut();
        bucket.dictate_overrides = forge_workspace::DictateOverrides::default();
        bucket.dictate_device_pin = None;
    }

    pub(crate) fn shift_active_turn_assistant_for_insert(&mut self, idx: usize) {
        if let Some(owner_idx) = self.active_turn_assistant_message_idx()
            && idx <= owner_idx
        {
            self.set_active_turn_assistant_message_idx(Some(owner_idx.saturating_add(1)));
        }
    }

    pub(crate) fn shift_stop_hook_summary_for_insert(&mut self, idx: usize) {
        if let Some(summary) = self.active_bucket_mut().last_stop_hook_summary.as_mut()
            && idx <= summary.message_idx
        {
            summary.message_idx = summary.message_idx.saturating_add(1);
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

    pub(crate) fn shift_stop_hook_summary_for_remove(&mut self, idx: usize) {
        let Some(owner_idx) = self.last_stop_hook_summary().map(|s| s.message_idx) else {
            return;
        };
        match idx.cmp(&owner_idx) {
            std::cmp::Ordering::Less => {
                if let Some(summary) = self.active_bucket_mut().last_stop_hook_summary.as_mut() {
                    summary.message_idx = owner_idx.saturating_sub(1);
                }
            }
            std::cmp::Ordering::Equal => self.set_last_stop_hook_summary(None),
            std::cmp::Ordering::Greater => {}
        }
    }

    pub(crate) fn remap_stop_hook_summary_after_message_drop(
        &mut self,
        old_to_new: &[Option<usize>],
    ) {
        let Some(old_idx) = self.last_stop_hook_summary().map(|s| s.message_idx) else {
            return;
        };
        match old_to_new.get(old_idx).copied().flatten() {
            Some(new_idx) => {
                if let Some(summary) = self.active_bucket_mut().last_stop_hook_summary.as_mut() {
                    summary.message_idx = new_idx;
                }
            }
            None => self.set_last_stop_hook_summary(None),
        }
    }

    pub fn active_autocomplete_kind(&self) -> Option<AutocompleteKind> {
        if self.emoji.is_some() {
            Some(AutocompleteKind::Emoji)
        } else if self.mention().is_some() {
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

    /// Whether the emoji picker has rows to navigate. Separate from
    /// [`Self::autocomplete_focus_available`] because the picker is
    /// app-level and lives in the /diff view too.
    pub fn emoji_focus_available(&self) -> bool {
        self.emoji.as_ref().is_some_and(super::emoji::EmojiState::has_selectable_candidates)
    }

    pub fn rebuild_chat_focus_from_state(&mut self) {
        if self.active_view != ActiveView::Chat {
            return;
        }

        self.normalize_focus_stack();

        if self.autocomplete_focus_available() {
            self.claim_focus_target(FocusTarget::Mention);
        } else {
            self.release_focus_target(FocusTarget::Mention);
        }

        if self.is_help_active() && !self.autocomplete_focus_available() {
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
        if self.autocomplete_focus_available() {
            ctx = ctx.with(FocusTarget::Mention);
        }
        if self.emoji_focus_available() {
            ctx = ctx.with(FocusTarget::Emoji);
        }
        if self.is_help_active() {
            ctx = ctx.with(FocusTarget::Help);
        }
        ctx
    }
}

/// Whether a background session belongs in the NEEDS ATTENTION band.
/// Three field reads and nothing else: this is
/// [`App::needs_attention_sessions`]'s first pass, which runs on every
/// inspector frame and must fall through without touching the
/// workspace when nothing is waiting.
fn session_needs_attention(session: &crate::app::session::UiSession) -> bool {
    !session.prompt_queue.is_empty()
        || session.failed_turn.is_some()
        || session.review_replies_waiting.is_some()
}

/// Build the SUBAGENTS row's header label from a Task/Agent root
/// tool call's `raw_input`. Combines `subagent_type` with the first
/// non-empty line of `description` (or `prompt` as a sibling fallback)
/// into `"<type> · <line>"`. Falls back to either piece on its own
/// when the other is missing, then to the raw `sdk_tool_name` so the
/// row always renders something even on a malformed dispatch.
fn subagent_label_from_root(root: &ToolCallInfo) -> String {
    let raw = root.raw_input.as_ref().and_then(|v| v.as_object());
    let read = |k: &str| -> Option<String> {
        raw.and_then(|r| r.get(k))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let subagent_type = read("subagent_type");
    let summary = read("description")
        .or_else(|| read("prompt"))
        .and_then(|s| s.lines().find(|line| !line.trim().is_empty()).map(str::to_owned))
        .map(|s| s.trim().to_owned());
    match (subagent_type, summary) {
        (Some(kind), Some(line)) => format!("{kind} \u{b7} {line}"),
        (Some(kind), None) => kind,
        (None, Some(line)) => line,
        (None, None) => root.sdk_tool_name.clone(),
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
    use crate::app::dialog;
    use crate::app::slash::{SlashCandidate, SlashContext, SlashState};
    use pretty_assertions::assert_eq;

    #[test]
    fn replace_monitor_output_tail_stamps_tool_call_info_and_bumps_dirty() {
        use crate::agent::model::ToolCallStatus;
        use crate::app::state::types::{MonitorEntry, MonitorStatus};
        use std::collections::VecDeque;

        let mut app = App::test_default();
        let tool_use_id = "tu-mon-1";
        let task_id = "task-mon-1";

        // Seed the active session's MonitorEntry.
        app.monitors_mut().push(MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some(task_id.to_owned()),
            description: "demo".to_owned(),
            command: "echo demo".to_owned(),
            persistent: true,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: VecDeque::new(),
            expanded_in_inspector: false,
        });

        // Push a matching ToolCall MessageBlock with a fresh ToolCallInfo
        // + index it so `lookup_tool_call` finds it.
        let tc_info = ToolCallInfo {
            id: tool_use_id.to_owned(),
            title: "Monitor".to_owned(),
            sdk_tool_name: "Monitor".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_output: None,
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
        };
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc_info))],
        ));
        let msg_idx = app.messages().len() - 1;
        app.index_tool_call(tool_use_id.to_owned(), msg_idx, 0);
        let initial_layout_epoch = {
            let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
            let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
                panic!("expected ToolCall block");
            };
            tc.layout_epoch
        };

        // Act: replace tail with 8 lines.
        let lines: Vec<String> = (1..=8).map(|i| format!("line {i}")).collect();
        app.replace_monitor_output_tail_by_task_id(task_id, &lines);

        // Assert: monitor_output_tail carries the LAST 5 lines.
        let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert_eq!(
            tc.monitor_output_tail,
            vec![
                "line 4".to_owned(),
                "line 5".to_owned(),
                "line 6".to_owned(),
                "line 7".to_owned(),
                "line 8".to_owned(),
            ]
        );
        assert!(
            tc.layout_epoch > initial_layout_epoch,
            "layout_epoch must bump so the cached chat block re-renders in place"
        );
    }

    #[test]
    fn record_answered_question_unhides_and_appends() {
        use crate::agent::model::ToolCallStatus;
        let mut app = App::test_default();
        let tool_use_id = "tu-q-1";
        // A chat-suppressed (hidden) AskUserQuestion, as it sits while
        // the dock prompt is the live answering surface.
        let tc_info = ToolCallInfo {
            id: tool_use_id.to_owned(),
            title: "AskUserQuestion".to_owned(),
            sdk_tool_name: "AskUserQuestion".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: true,
            terminal_id: None,
            terminal_output: None,
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
        };
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc_info))],
        ));
        let msg_idx = app.messages().len() - 1;
        app.index_tool_call(tool_use_id.to_owned(), msg_idx, 0);

        app.record_answered_question(
            tool_use_id,
            crate::app::AnsweredQuestion {
                question: "Which path?".to_owned(),
                picked_labels: vec!["Clean card".to_owned()],
                typed_note: None,
            },
        );

        let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert!(!tc.hidden, "answered question must un-hide so the card renders");
        assert_eq!(tc.answered_questions.len(), 1);
        assert_eq!(tc.answered_questions[0].picked_labels, vec!["Clean card".to_owned()]);
        assert!(tc.answered_questions[0].typed_note.is_none());
    }

    #[test]
    fn replace_monitor_output_tail_handles_fewer_than_five_lines() {
        use crate::agent::model::ToolCallStatus;
        use crate::app::state::types::{MonitorEntry, MonitorStatus};
        use std::collections::VecDeque;

        let mut app = App::test_default();
        let tool_use_id = "tu-mon-2";
        let task_id = "task-mon-2";
        app.monitors_mut().push(MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some(task_id.to_owned()),
            description: "demo".to_owned(),
            command: "echo demo".to_owned(),
            persistent: false,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: VecDeque::new(),
            expanded_in_inspector: false,
        });
        let tc_info = ToolCallInfo {
            id: tool_use_id.to_owned(),
            title: "Monitor".to_owned(),
            sdk_tool_name: "Monitor".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_output: None,
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
        };
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc_info))],
        ));
        let msg_idx = app.messages().len() - 1;
        app.index_tool_call(tool_use_id.to_owned(), msg_idx, 0);

        app.replace_monitor_output_tail_by_task_id(
            task_id,
            &["one".to_owned(), "two".to_owned(), "three".to_owned()],
        );

        let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert_eq!(
            tc.monitor_output_tail,
            vec!["one".to_owned(), "two".to_owned(), "three".to_owned()],
            "tails shorter than 5 are kept verbatim",
        );
    }

    #[test]
    fn replace_monitor_output_tail_unchanged_is_noop_changed_still_dirties() {
        use crate::agent::model::ToolCallStatus;
        use crate::app::state::types::{MonitorEntry, MonitorStatus};
        use std::collections::VecDeque;

        let mut app = App::test_default();
        let tool_use_id = "tu-mon-3";
        let task_id = "task-mon-3";
        app.monitors_mut().push(MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some(task_id.to_owned()),
            description: "demo".to_owned(),
            command: "echo demo".to_owned(),
            persistent: true,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: VecDeque::new(),
            expanded_in_inspector: false,
        });
        let tc_info = ToolCallInfo {
            id: tool_use_id.to_owned(),
            title: "Monitor".to_owned(),
            sdk_tool_name: "Monitor".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_output: None,
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
        };
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc_info))],
        ));
        let msg_idx = app.messages().len() - 1;
        app.index_tool_call(tool_use_id.to_owned(), msg_idx, 0);

        let lines = vec!["alpha".to_owned(), "beta".to_owned()];

        // First refresh stamps the tail and bumps layout_epoch.
        app.replace_monitor_output_tail_by_task_id(task_id, &lines);
        let epoch_after_first = {
            let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
            let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
                panic!("expected ToolCall block");
            };
            assert_eq!(tc.monitor_output_tail, lines);
            tc.layout_epoch
        };

        // Second refresh with the SAME tail must not re-invalidate.
        app.replace_monitor_output_tail_by_task_id(task_id, &lines);
        let epoch_after_unchanged = {
            let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
            let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
                panic!("expected ToolCall block");
            };
            tc.layout_epoch
        };
        assert_eq!(
            epoch_after_unchanged, epoch_after_first,
            "an unchanged monitor-tail refresh must not dirty the cached block",
        );

        // Third refresh with a CHANGED tail re-stamps and re-dirties.
        let changed = vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()];
        app.replace_monitor_output_tail_by_task_id(task_id, &changed);
        let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert_eq!(tc.monitor_output_tail, changed, "a changed tail must re-stamp");
        assert!(
            tc.layout_epoch > epoch_after_unchanged,
            "a changed monitor-tail refresh must dirty so the block re-renders",
        );
    }

    #[test]
    fn upsert_wakeup_replaces_prior_wakeup() {
        let mut app = App::test_default();
        let t0 = std::time::SystemTime::UNIX_EPOCH;
        app.upsert_wakeup_from_tool_input("tu1", "first", t0 + std::time::Duration::from_secs(60));
        app.upsert_wakeup_from_tool_input(
            "tu2",
            "second",
            t0 + std::time::Duration::from_secs(120),
        );
        let s = app.schedules();
        assert_eq!(s.len(), 1, "re-armed wakeup replaces the prior one");
        assert_eq!(s[0].label, "second");
        assert_eq!(s[0].key, "tu2");
    }

    #[test]
    fn prune_expired_schedules_drops_passed_wakeup() {
        let mut app = App::test_default();
        let t0 = std::time::SystemTime::UNIX_EPOCH;
        let fire = t0 + std::time::Duration::from_secs(60);
        app.upsert_wakeup_from_tool_input("tu1", "poll", fire);
        app.prune_expired_schedules(t0); // before fire - kept
        assert_eq!(app.schedules().len(), 1);
        app.prune_expired_schedules(fire); // at fire - dropped
        assert!(app.schedules().is_empty());
    }

    #[test]
    fn cron_lifecycle_upsert_stamp_delete() {
        use crate::app::state::types::ScheduleKind;
        let mut app = App::test_default();
        let t0 = std::time::SystemTime::UNIX_EPOCH;
        app.upsert_cron_from_tool_input(
            "tu1",
            "*/5 * * * *",
            "Lead heartbeat\nCheck the merge gate.",
            true,
            t0,
        );
        assert_eq!(app.schedules().len(), 1);
        assert!(matches!(app.schedules()[0].kind, ScheduleKind::Cron { recurring: true, .. }));
        assert_eq!(
            app.schedules()[0].schedule,
            "every 5 minutes",
            "a cloud cron humanizes its expression",
        );
        assert_eq!(
            app.schedules()[0].label,
            "Lead heartbeat",
            "a native cron headlines on its prompt's first line",
        );
        // Stamp the job id discovered from the CronCreate result.
        app.stamp_cron_id_from_result("tu1", "job-abc");
        assert_eq!(app.schedules()[0].cron_id.as_deref(), Some("job-abc"));
        // CronDelete by job id removes it.
        app.remove_cron_by_id("job-abc");
        assert!(app.schedules().is_empty());
    }

    #[test]
    fn cron_upsert_idempotent_on_retry() {
        let mut app = App::test_default();
        let t0 = std::time::SystemTime::UNIX_EPOCH;
        app.upsert_cron_from_tool_input("tu1", "*/5 * * * *", "", true, t0);
        app.upsert_cron_from_tool_input("tu1", "*/5 * * * *", "", true, t0);
        assert_eq!(app.schedules().len(), 1, "re-decoded same tool_use_id stays one entry");
    }

    #[test]
    fn one_shot_cron_resolves_a_fire_time_and_then_prunes() {
        let mut app = App::test_default();
        let created = std::time::SystemTime::now();
        // A one-shot pinned to a day-of-month + month, the shape the CLI
        // emits for "run once at <time>".
        app.upsert_cron_from_tool_input("tu1", "48 16 24 4 *", "", false, created);

        let fire = app.schedules()[0].fire_at.expect("one-shot resolves its next occurrence");
        assert!(fire > created, "the fire time is the first match after creation");

        app.prune_expired_schedules(fire - std::time::Duration::from_secs(1));
        assert_eq!(app.schedules().len(), 1, "retained while pending");
        app.prune_expired_schedules(fire);
        assert!(app.schedules().is_empty(), "dropped once its fire time passes");
    }

    #[test]
    fn recurring_cron_carries_no_fire_time() {
        let mut app = App::test_default();
        app.upsert_cron_from_tool_input("tu1", "0 9 * * *", "", true, std::time::SystemTime::now());
        assert!(
            app.schedules()[0].fire_at.is_none(),
            "a recurring cron badges `recurring`; its schedule already carries the timing",
        );
    }

    #[test]
    fn cron_upsert_empty_expr_shows_unknown_schedule() {
        let mut app = App::test_default();
        let t0 = std::time::SystemTime::UNIX_EPOCH;
        app.upsert_cron_from_tool_input("tu1", "", "", true, t0);
        assert_eq!(
            app.schedules()[0].schedule,
            "(unknown schedule)",
            "an empty cloud cron expr renders a placeholder, not a blank schedule",
        );
    }

    /// FIX (4th attempt): the reported bug state is an active web-api
    /// tab where GIT / PROCESSES render and the projects pane + top bar
    /// highlight web-api, yet SCHEDULES is blank - because the
    /// per-bucket project STAMP is `None`. The fix resolves the active
    /// project through the SAME `resolve_active_project_view` the pane +
    /// top bar use (a catalog match on the real session UUID), so
    /// SCHEDULES populates despite the missing stamp AND a blanked
    /// cwd_raw. This isolates the primary chain link: neither the stamp
    /// nor the cwd can resolve here, only the key/catalog resolver.
    #[test]
    fn refresh_forge_crons_resolves_via_pane_resolver_when_stamp_none() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/Users/me/Projects/web-api";
        ws.seed_test_project("web-api", path);
        // Mirror production: record_connected_session stamps the on-disk
        // catalog at Connect, which is exactly what resolve_active_project_view
        // reads to highlight the active project in the pane + top bar.
        let uuid = "acbd8a76-448b-4dda-bb01-dd930cdd261a";
        ws.record_connected_session(path, uuid, None);
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "web-api".to_owned(),
            kind: CronKind::Recurring("18 9 * * 1-5".to_owned()),
            prompt: "market open".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        // Active tab is the real web-api session, but the stamp is None
        // AND cwd_raw is blank - only the catalog resolver can succeed.
        let key = forge_workspace::SessionKey::from_session_id(uuid);
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.project = None;
        bucket.cwd_raw = String::new();
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(
            app.active_project_name().as_deref(),
            Some("web-api"),
            "resolves via the pane/top-bar resolver despite a None stamp + blank cwd",
        );
        assert_eq!(app.forge_crons, vec![cron], "SCHEDULES populates via the robust chain");
    }

    #[test]
    fn refresh_forge_crons_caches_humanized_schedule_rows() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/Users/me/Projects/web-api";
        ws.seed_test_project("web-api", path);
        let uuid = "acbd8a76-448b-4dda-bb01-dd930cdd261a";
        ws.record_connected_session(path, uuid, None);
        ws.seed_test_cron(CronEntry {
            id: CronId::from("c1"),
            project_name: "web-api".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "market open".to_owned(),
            description: Some("Morning digest".to_owned()),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        });
        let key = forge_workspace::SessionKey::from_session_id(uuid);
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.project = Some("web-api".to_owned());
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(app.forge_crons.len(), 1, "raw snapshot still populated");
        assert_eq!(
            app.forge_schedule_rows.len(),
            1,
            "the tick humanizes the cron into a cached presentation row",
        );
        let row = &app.forge_schedule_rows[0];
        assert_eq!(row.schedule, "daily at 09:00", "schedule humanized once on the tick");
        assert_eq!(row.description.as_deref(), Some("Morning digest"), "description headlines");
        assert_eq!(
            row.fire_at,
            Some(std::time::SystemTime::UNIX_EPOCH),
            "fire_at drives the countdown"
        );
    }

    /// Last-resort chain link: when the stamp is None AND the catalog has
    /// no entry for the active UUID (resolve_active_project_view misses),
    /// the active project still resolves from the bucket's cwd_raw - the
    /// same value GIT/PROCESSES read successfully.
    #[test]
    fn refresh_forge_crons_falls_back_to_cwd_when_stamp_none_and_no_catalog() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/Users/me/Projects/web-api";
        ws.seed_test_project("web-api", path);
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "web-api".to_owned(),
            kind: CronKind::Recurring("18 9 * * 1-5".to_owned()),
            prompt: "market open".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        // Real UUID NOT in the catalog + stamp None: only cwd_raw resolves.
        let key = forge_workspace::SessionKey::from_session_id("uncatalogued-uuid");
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.project = None;
        bucket.cwd_raw = path.to_owned();
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(app.active_project_name().as_deref(), Some("web-api"));
        assert_eq!(app.forge_crons, vec![cron], "SCHEDULES resolves via the cwd fallback");
    }

    /// REPRODUCE (recurring SCHEDULES-blank bug, 3rd attempt): the
    /// Inspector scopes forge crons by the tab's stamped project NAME,
    /// never by re-deriving the project from `cwd_raw`. A bucket whose
    /// `project` is set but whose `cwd_raw` does NOT path-prefix-match
    /// the project's stored (expanded) path still surfaces the project's
    /// crons. The pre-fix cwd-prefix match returned empty for exactly
    /// this mismatch (here a tilde form vs the expanded project path) -
    /// the class of failure that kept web-api's SCHEDULES blank.
    #[test]
    fn refresh_forge_crons_scopes_by_stamped_project_name_not_cwd() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");

        // Project path is stored expanded; the bucket cwd is a tilde
        // form that cannot prefix-match it.
        ws.seed_test_project("web-api", "/Users/me/Projects/web-api");
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "web-api".to_owned(),
            kind: CronKind::Recurring("18 9 * * 1-5".to_owned()),
            prompt: "market open".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        let key = forge_workspace::SessionKey::from_session_id("__spawn_web-api__");
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.project = Some("web-api".to_owned());
        bucket.cwd_raw = "~/Projects/web-api".to_owned();
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(
            app.forge_crons,
            vec![cron],
            "SCHEDULES resolves via the stamped project name regardless of the cwd form",
        );
    }

    /// A synthetic `__spawn_<name>__` active key resolves its project via
    /// the same pane/top-bar resolver (by name), so SCHEDULES populates
    /// even when the bucket carries no stamp. A truly-unresolvable active
    /// bucket - no name match, not in the catalog, no stamp, cwd under no
    /// project - still degrades cleanly to empty rather than surfacing
    /// another project's crons.
    #[test]
    fn refresh_forge_crons_resolves_synthetic_spawn_key_by_name() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");

        ws.seed_test_project("cronproj", "/tmp/cronproj-inspector");
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "cronproj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        // Synthetic spawn key with NO stamp: resolves to cronproj by name.
        let synthetic = forge_workspace::SessionKey::from_session_id("__spawn_cronproj__");
        let mut bucket = crate::app::session::UiSession::new(synthetic.clone());
        bucket.project = None;
        app.sessions.insert(synthetic.clone(), bucket);
        app.active_session_key = Some(synthetic);

        app.refresh_forge_crons();
        assert_eq!(
            app.forge_crons,
            vec![cron],
            "synthetic spawn key resolves the project by name, no stamp needed",
        );

        // Degrade cleanly: an active bucket that resolves via no link (not
        // a known project name, not catalogued, no stamp, cwd under no
        // project) yields empty rather than another project's crons.
        let orphan = forge_workspace::SessionKey::from_session_id("orphan-uuid");
        let mut orphan_bucket = crate::app::session::UiSession::new(orphan.clone());
        orphan_bucket.project = None;
        orphan_bucket.cwd_raw = "/tmp/unmapped-dir".to_owned();
        app.sessions.insert(orphan.clone(), orphan_bucket);
        app.active_session_key = Some(orphan);

        app.refresh_forge_crons();
        assert!(app.forge_crons.is_empty(), "an unresolvable active bucket yields empty SCHEDULES");
    }

    /// The Inspector scopes SCHEDULES by the stamped project name, so it
    /// surfaces the project's crons no matter what the active session key
    /// looks like - a real claude UUID (project lead), a worker session
    /// key, or a synthetic spawn placeholder. The bucket cwd is left
    /// blank to prove the resolution no longer depends on it.
    #[test]
    fn refresh_forge_crons_resolves_across_active_key_shapes() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "cronproj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };

        for key_str in ["11111111-2222-3333-4444-555555555555", "worker-uuid", "__spawn_cronproj__"]
        {
            let mut app = App::test_default();
            let ws = app.workspace.clone().expect("test workspace");
            ws.seed_test_project("cronproj", "/tmp/cronproj-shapes");
            ws.seed_test_cron(cron.clone());

            let key = forge_workspace::SessionKey::from_session_id(key_str);
            let mut bucket = crate::app::session::UiSession::new(key.clone());
            bucket.project = Some("cronproj".to_owned());
            app.sessions.insert(key.clone(), bucket);
            app.active_session_key = Some(key);

            app.refresh_forge_crons();
            assert_eq!(
                app.forge_crons,
                vec![cron.clone()],
                "active key {key_str} resolves the project's crons via the stamped name",
            );
        }
    }

    // ── needs_attention_sessions (Inspector NEEDS ATTENTION band) ──────────

    /// Seed a background session carrying one pending permission prompt
    /// enqueued `secs` after the UNIX epoch; stamps a project name so
    /// the row resolves without a workspace catalog. Returns its key.
    fn seed_attention_session(app: &mut App, id: &str, secs: u64) -> forge_workspace::SessionKey {
        let key = forge_workspace::SessionKey::from_session_id(id);
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some(format!("proj-{id}"));
        let mut prompt = crate::app::prompt::PromptState::from_permission(
            format!("tc-{id}"),
            crate::app::prompt::tests::make_permission_request(),
        );
        prompt.enqueued_at =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        session.prompt_queue.push_back(prompt);
        app.sessions.insert(key.clone(), session);
        key
    }

    #[test]
    fn needs_input_sessions_empty_when_no_prompts() {
        let app = App::test_default();
        assert!(app.needs_attention_sessions().is_empty(), "no pending prompts -> no rows");
    }

    /// The band's first pass runs on every inspector frame, so a settled
    /// session must fall out of it on field reads alone - that is what
    /// lets `needs_attention_sessions` return before it clones
    /// `list_projects` or takes the live-workers lock.
    #[test]
    fn attention_first_pass_ignores_a_settled_session() {
        let settled = crate::app::session::UiSession::new(
            forge_workspace::SessionKey::from_session_id("quiet"),
        );
        assert!(!session_needs_attention(&settled), "nothing waiting -> not a band candidate");

        let mut waiting = crate::app::session::UiSession::new(
            forge_workspace::SessionKey::from_session_id("quiet"),
        );
        waiting.review_replies_waiting = Some(crate::app::ReviewRepliesWaiting {
            branch: "feat".to_owned(),
            count: 1,
            since: std::time::SystemTime::UNIX_EPOCH,
        });
        assert!(session_needs_attention(&waiting), "unread worker answers are a candidate");
    }

    #[test]
    fn needs_input_sessions_includes_background_and_excludes_active() {
        let mut app = App::test_default();
        seed_attention_session(&mut app, "bg", 100);
        let active = seed_attention_session(&mut app, "active", 50);
        app.active_session_key = Some(active);

        let entries = app.needs_attention_sessions();
        let keys: Vec<&str> = entries.iter().map(|e| e.session_key.as_str()).collect();
        assert_eq!(keys, vec!["bg"], "the active session is excluded even with a pending prompt");
        assert!(matches!(entries[0].kind, crate::app::AttentionKind::Permission { .. }));
    }

    #[test]
    fn needs_input_sessions_sorted_stalest_first() {
        let mut app = App::test_default();
        // Insert newest-first to prove the sort reorders by enqueue age,
        // not by insertion order.
        seed_attention_session(&mut app, "newest", 300);
        seed_attention_session(&mut app, "oldest", 100);
        seed_attention_session(&mut app, "middle", 200);
        let entries = app.needs_attention_sessions();
        let order: Vec<&str> = entries.iter().map(|e| e.session_key.as_str()).collect();
        assert_eq!(order, vec!["oldest", "middle", "newest"], "stalest (oldest enqueue) on top");
    }

    #[test]
    fn needs_input_sessions_reports_question_kind() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("q");
        let mut session = crate::app::session::UiSession::new(key.clone());
        let mut prompt = crate::app::prompt::PromptState::from_question(
            "tc-q".to_owned(),
            crate::app::prompt::tests::make_question_request(false),
        );
        prompt.enqueued_at = std::time::SystemTime::UNIX_EPOCH;
        session.prompt_queue.push_back(prompt);
        app.sessions.insert(key, session);
        let entries = app.needs_attention_sessions();
        assert!(
            entries.iter().any(|e| matches!(e.kind, crate::app::AttentionKind::Question)),
            "a session with an AskUserQuestion prompt reports the Question kind",
        );
    }

    #[test]
    fn needs_input_sessions_tiebreaks_equal_enqueue_by_session_id() {
        // `sessions` is a HashMap (unordered iteration), so equal enqueue
        // times must resolve deterministically via the session-id
        // tiebreak or the band would flicker order between frames. Seed
        // in reverse id order to prove the sort, not insertion order.
        let mut app = App::test_default();
        seed_attention_session(&mut app, "zeta", 500);
        seed_attention_session(&mut app, "alpha", 500);
        let entries = app.needs_attention_sessions();
        let order: Vec<&str> = entries.iter().map(|e| e.session_key.as_str()).collect();
        assert_eq!(order, vec!["alpha", "zeta"], "equal enqueue -> deterministic id tiebreak");
    }

    #[test]
    fn needs_input_sessions_resolves_worker_role_from_live_worker() {
        use forge_workspace::{SessionKey, WorkerEntry};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project("core-v1", "/tmp/core-v1");
        // Insert the worker under the SAME key list_projects returns
        // (derived from the project path, not the name).
        let project_key = ws
            .list_projects()
            .into_iter()
            .find(|p| p.name == "core-v1")
            .expect("seeded project")
            .key;
        let worker_key = SessionKey::from_session_id("worker-steward-uuid");
        ws.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "steward".into(),
                charter: "be sharp".into(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        // A waiting (background) bucket for that worker session.
        let mut session = crate::app::session::UiSession::new(worker_key.clone());
        let mut prompt = crate::app::prompt::PromptState::from_permission(
            "tc-w".to_owned(),
            crate::app::prompt::tests::make_permission_request(),
        );
        prompt.enqueued_at = std::time::SystemTime::UNIX_EPOCH;
        session.prompt_queue.push_back(prompt);
        app.sessions.insert(worker_key.clone(), session);

        let entries = app.needs_attention_sessions();
        let entry = entries.iter().find(|e| e.session_key == worker_key).expect("worker entry");
        assert_eq!(entry.name, "core-v1", "name resolves to the owning project");
        assert_eq!(entry.role.as_deref(), Some("steward"), "role resolves to the worker label");
    }

    // ── failed-turn attention rows ─────────────────────────────────

    /// Seed a background session whose last turn failed `secs` after the
    /// epoch with the given classification. Returns its key.
    fn seed_failed_session(
        app: &mut App,
        id: &str,
        secs: u64,
        error: forge_primitives::ApiRetryError,
        status: Option<u16>,
    ) -> forge_workspace::SessionKey {
        let key = forge_workspace::SessionKey::from_session_id(id);
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some(format!("proj-{id}"));
        session.failed_turn = Some(crate::app::FailedTurn {
            error,
            status,
            failed_at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        });
        app.sessions.insert(key.clone(), session);
        key
    }

    #[test]
    fn needs_attention_sessions_includes_failed_background_session() {
        let mut app = App::test_default();
        let key = seed_failed_session(
            &mut app,
            "bg",
            100,
            forge_primitives::ApiRetryError::ServerError,
            Some(529),
        );

        let entries = app.needs_attention_sessions();
        let entry = entries.iter().find(|e| e.session_key == key).expect("failed row present");
        assert_eq!(
            entry.kind,
            crate::app::AttentionKind::Failed {
                error: forge_primitives::ApiRetryError::ServerError,
                status: Some(529),
            },
            "a failed background turn surfaces as a Failed attention row",
        );
    }

    #[test]
    fn needs_attention_sessions_excludes_failed_active_session() {
        let mut app = App::test_default();
        let active = seed_failed_session(
            &mut app,
            "active",
            100,
            forge_primitives::ApiRetryError::Unknown,
            None,
        );
        app.active_session_key = Some(active);
        assert!(
            app.needs_attention_sessions().is_empty(),
            "the session the user is looking at already shows its error in the chat",
        );
    }

    /// Seed a background session holding `count` unread worker answers on
    /// its review comments, waiting since `secs` after the epoch.
    fn seed_review_replies_session(
        app: &mut App,
        id: &str,
        secs: u64,
        count: usize,
    ) -> forge_workspace::SessionKey {
        let key = forge_workspace::SessionKey::from_session_id(id);
        let mut session = crate::app::session::UiSession::new(key.clone());
        session.project = Some(format!("proj-{id}"));
        session.review_replies_waiting = Some(crate::app::ReviewRepliesWaiting {
            branch: "feat".to_owned(),
            count,
            since: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs),
        });
        app.sessions.insert(key.clone(), session);
        key
    }

    #[test]
    fn needs_attention_sessions_includes_waiting_review_replies() {
        let mut app = App::test_default();
        let key = seed_review_replies_session(&mut app, "bg", 100, 2);

        let entries = app.needs_attention_sessions();
        let entry = entries.iter().find(|e| e.session_key == key).expect("review row present");
        assert_eq!(entry.kind, crate::app::AttentionKind::ReviewReplies { count: 2 });
        assert_eq!(
            entry.enqueued_at,
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100),
            "the row ages from when the replies landed",
        );
    }

    /// The band is about work happening ELSEWHERE - the active session's
    /// own waiting replies are the GIT header badge's job.
    #[test]
    fn needs_attention_sessions_excludes_active_sessions_review_replies() {
        let mut app = App::test_default();
        let active = seed_review_replies_session(&mut app, "active", 100, 3);
        app.active_session_key = Some(active);
        assert!(
            app.needs_attention_sessions().is_empty(),
            "the session the user is looking at gets the GIT badge instead",
        );
    }

    /// Nothing is blocked on an unread reply, so a session that is also
    /// waiting on the user shows that instead.
    #[test]
    fn needs_attention_sessions_prefers_a_pending_prompt_over_review_replies() {
        let mut app = App::test_default();
        let key = seed_attention_session(&mut app, "both", 100);
        app.sessions.get_mut(&key).expect("seeded bucket").review_replies_waiting =
            Some(crate::app::ReviewRepliesWaiting {
                branch: "feat".to_owned(),
                count: 4,
                since: std::time::SystemTime::UNIX_EPOCH,
            });

        let entries = app.needs_attention_sessions();
        assert_eq!(entries.len(), 1, "one row per session");
        assert!(
            matches!(entries[0].kind, crate::app::AttentionKind::Permission { .. }),
            "the pending prompt outranks the unread replies: {:?}",
            entries[0].kind,
        );
    }

    /// A failed turn outranks a stale pending prompt on the same session:
    /// the band emits one row per session and the error is the signal the
    /// user must not miss.
    #[test]
    fn needs_attention_sessions_prefers_failure_over_pending_prompt() {
        let mut app = App::test_default();
        let key = seed_attention_session(&mut app, "both", 100);
        app.sessions.get_mut(&key).expect("seeded bucket").failed_turn =
            Some(crate::app::FailedTurn {
                error: forge_primitives::ApiRetryError::BillingError,
                status: Some(400),
                failed_at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200),
            });

        let entries = app.needs_attention_sessions();
        assert_eq!(entries.len(), 1, "one row per session");
        assert!(
            matches!(entries[0].kind, crate::app::AttentionKind::Failed { .. }),
            "the failure wins over the pending prompt: {:?}",
            entries[0].kind,
        );
    }

    /// Switching to a failed session IS attending to it - the chat shows
    /// the error block, so the band entry must not survive to reappear
    /// the next time the user switches away.
    #[test]
    fn failed_turn_clears_when_session_becomes_active() {
        let mut app = App::test_default();
        let key = seed_failed_session(
            &mut app,
            "bg",
            100,
            forge_primitives::ApiRetryError::ServerError,
            Some(529),
        );
        // A second bucket so there is somewhere to switch away to.
        seed_failed_session(&mut app, "other", 50, forge_primitives::ApiRetryError::Unknown, None);
        app.active_session_key = Some(forge_workspace::SessionKey::from_session_id("other"));

        app.switch_active_session(key.clone());
        assert!(
            app.sessions.get(&key).expect("bucket").failed_turn.is_none(),
            "attending to the session clears its failure",
        );

        app.switch_active_session(forge_workspace::SessionKey::from_session_id("other"));
        assert!(
            !app.needs_attention_sessions().iter().any(|e| e.session_key == key),
            "the attended failure does not come back on switch-away",
        );
    }

    /// The boot id-adoption moves the active key onto the real bucket
    /// without a switch, so the status mirror must re-derive there
    /// instead of keeping the boot Connecting.
    #[test]
    fn set_session_id_adopts_status_from_the_destination_bucket() {
        let mut app = App::test_default();
        app.status = AppStatus::Connecting;
        let uuid = "11111111-2222-3333-4444-555555555555";
        let real = forge_workspace::SessionKey::from_session_id(uuid);
        let mut bucket = crate::app::session::UiSession::new(real.clone());
        bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
        app.sessions.insert(real.clone(), bucket);

        app.set_session_id(Some(crate::agent::model::SessionId::new(uuid)));

        assert_eq!(
            app.active_session_key.as_ref(),
            Some(&real),
            "adoption lands on the real bucket"
        );
        assert_eq!(
            app.status,
            AppStatus::Ready,
            "status re-derives from the adopted bucket instead of sticking at Connecting"
        );
    }

    /// A pick that lands on the already-focused session must still
    /// settle the status mirror: after the boot id-adoption the next
    /// launchpad Enter hits this path with a stale Connecting.
    #[test]
    fn same_key_switch_still_derives_status_from_the_bucket() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_session_id("same");
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key.clone());
        app.status = AppStatus::Connecting;

        app.switch_active_session(key);

        assert_eq!(
            app.status,
            AppStatus::Ready,
            "a same-key landing re-derives from the bucket's Idle lifecycle"
        );
    }

    /// The launchpad-stall repro end to end: the boot adoption moves
    /// focus, the user's first pick lands on the same key, and the
    /// composer must come out of the blocked-input set.
    #[test]
    fn boot_adoption_then_same_key_pick_leaves_the_composer_typable() {
        let mut app = App::test_default();
        app.status = AppStatus::Connecting;
        let real = forge_workspace::SessionKey::from_session_id("boot-resumed");
        let mut bucket = crate::app::session::UiSession::new(real.clone());
        bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Idle;
        app.sessions.insert(real.clone(), bucket);

        app.set_session_id(Some(crate::agent::model::SessionId::new("boot-resumed")));
        app.switch_active_session(real);

        assert!(
            !matches!(
                app.status,
                AppStatus::Connecting | AppStatus::CommandPending | AppStatus::Error
            ),
            "the composer must be typable after the pick, got {:?}",
            app.status
        );
    }

    /// A session that starts another turn has recovered - the stale
    /// failure row must go, whether the turn came from the user or from
    /// forge's own auto-continue.
    #[test]
    fn failed_turn_clears_when_a_new_turn_starts() {
        let mut app = App::test_default();
        let key = seed_failed_session(
            &mut app,
            "bg",
            100,
            forge_primitives::ApiRetryError::ServerError,
            Some(529),
        );
        crate::app::events::set_bucket_lifecycle_state(
            &mut app,
            &key,
            crate::app::session::SessionLifecycleState::Running,
        );
        assert!(
            app.sessions.get(&key).expect("bucket").failed_turn.is_none(),
            "a new turn on the session clears the failure",
        );
    }

    /// Going Idle is not recovery - the turn-error path itself parks the
    /// bucket at Idle, so clearing there would erase the row the instant
    /// it was set.
    #[test]
    fn failed_turn_survives_an_idle_transition() {
        let mut app = App::test_default();
        let key = seed_failed_session(
            &mut app,
            "bg",
            100,
            forge_primitives::ApiRetryError::ServerError,
            Some(529),
        );
        crate::app::events::set_bucket_lifecycle_state(
            &mut app,
            &key,
            crate::app::session::SessionLifecycleState::Idle,
        );
        assert!(
            app.sessions.get(&key).expect("bucket").failed_turn.is_some(),
            "Idle is where a failed turn parks; the row must outlive it",
        );
    }

    /// A worker spawned into a git worktree carries the worktree path
    /// (`<project>/.claude/worktrees/<label>`) as its cwd, but its bucket
    /// is stamped with the PARENT project name (resolved at Connect), so
    /// the Inspector surfaces the parent project's crons.
    #[test]
    fn refresh_forge_crons_resolves_worktree_worker_via_parent_project() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};

        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/tmp/cronproj-worktree";
        ws.seed_test_project("cronproj", path);
        let cron = CronEntry {
            id: CronId::from("c1"),
            project_name: "cronproj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };
        ws.seed_test_cron(cron.clone());

        let key = forge_workspace::SessionKey::from_session_id("worktree-worker-uuid");
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.cwd_raw = format!("{path}/.claude/worktrees/reviewer");
        bucket.project = Some("cronproj".to_owned());
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_forge_crons();
        assert_eq!(
            app.forge_crons,
            vec![cron],
            "a worktree worker's Inspector resolves its parent project's crons",
        );
    }

    /// GOTIFY mirrors SCHEDULES: the Inspector scopes the active project's
    /// subscriptions by the stamped project name, regardless of the
    /// bucket cwd (here a worktree path that does not equal the project
    /// root).
    #[test]
    fn refresh_gotify_scopes_by_stamped_project_name() {
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        let path = "/tmp/gotify-inspector-proj";
        ws.seed_test_project("gproj", path);
        ws.seed_test_gotify_subscription(forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: "gproj".to_owned(),
            team_role: None,
            applications: vec!["alerts".to_owned()],
            min_priority: Some(5),
            created_at: std::time::SystemTime::UNIX_EPOCH,
        });

        let key = forge_workspace::SessionKey::from_session_id("__spawn_gproj__");
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.cwd_raw = format!("{path}/.claude/worktrees/reviewer");
        bucket.project = Some("gproj".to_owned());
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key);

        app.refresh_gotify();
        assert_eq!(app.gotify_subs.len(), 1, "GOTIFY resolves subscriptions via the stamped name");
    }

    // ---------------------------------------------------------
    // Own-scope: SCHEDULES + GOTIFY show only the active session's
    // own items. A lead owns the `team_role: None` set, a worker its
    // own label's; neither sees the other's.
    // ---------------------------------------------------------

    /// An App whose active tab is `session_id`, stamped with a freshly
    /// seeded `project`. The session is a lead until
    /// [`seed_live_worker`] registers it.
    fn app_on_project(
        project: &str,
        session_id: &str,
    ) -> (App, Arc<forge_workspace::Workspace>, forge_workspace::SessionKey) {
        let mut app = App::test_default();
        let ws = app.workspace.clone().expect("test workspace");
        ws.seed_test_project(project, &format!("/tmp/{project}"));
        let key = forge_workspace::SessionKey::from_session_id(session_id);
        let mut bucket = crate::app::session::UiSession::new(key.clone());
        bucket.project = Some(project.to_owned());
        app.sessions.insert(key.clone(), bucket);
        app.active_session_key = Some(key.clone());
        (app, ws, key)
    }

    /// Register `session` as the live worker `label` of `project`, the
    /// state `worker_lookup_for_session` reads to resolve a session's
    /// own role. Workers never reach the sessions catalog, so this
    /// registry is the only source.
    fn seed_live_worker(
        ws: &forge_workspace::Workspace,
        project: &str,
        label: &str,
        session: &forge_workspace::SessionKey,
    ) {
        let project_key =
            ws.list_projects().into_iter().find(|p| p.name == project).expect("seeded project").key;
        ws.insert_live_worker(
            &project_key,
            forge_workspace::WorkerEntry {
                label: label.to_owned(),
                charter: "charter".to_owned(),
                session_key: session.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".to_owned(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
    }

    fn cron_owned_by(
        id: &str,
        project: &str,
        team_role: Option<&str>,
    ) -> forge_primitives::CronEntry {
        forge_primitives::CronEntry {
            id: forge_primitives::cron::CronId::from(id),
            project_name: project.to_owned(),
            kind: forge_primitives::cron::CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            description: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: team_role.map(str::to_owned),
        }
    }

    fn sub_owned_by(
        id: u128,
        project: &str,
        team_role: Option<&str>,
    ) -> forge_primitives::GotifySubscription {
        forge_primitives::GotifySubscription {
            id: uuid::Uuid::from_u128(id),
            project: project.to_owned(),
            team_role: team_role.map(str::to_owned),
            applications: vec!["alerts".to_owned()],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn cron_ids(app: &App) -> Vec<&str> {
        app.forge_crons.iter().map(|c| c.id.as_str()).collect()
    }

    fn sub_ids(app: &App) -> Vec<u128> {
        app.gotify_subs.iter().map(|s| s.id.as_u128()).collect()
    }

    #[test]
    fn refresh_forge_crons_shows_only_the_leads_own_crons() {
        let (mut app, ws, _) = app_on_project("scoped", "lead-uuid");
        ws.seed_test_cron(cron_owned_by("lead-c", "scoped", None));
        ws.seed_test_cron(cron_owned_by("worker-c", "scoped", Some("steward")));

        app.refresh_forge_crons();

        assert_eq!(cron_ids(&app), vec!["lead-c"], "a lead sees only lead-created crons");
        assert_eq!(app.forge_schedule_rows.len(), 1, "the humanized rows match the scoped set");
    }

    #[test]
    fn refresh_forge_crons_shows_only_the_workers_own_crons() {
        let (mut app, ws, key) = app_on_project("scoped", "steward-uuid");
        seed_live_worker(&ws, "scoped", "steward", &key);
        ws.seed_test_cron(cron_owned_by("lead-c", "scoped", None));
        ws.seed_test_cron(cron_owned_by("steward-c", "scoped", Some("steward")));
        ws.seed_test_cron(cron_owned_by("reviewer-c", "scoped", Some("reviewer")));

        app.refresh_forge_crons();

        assert_eq!(
            cron_ids(&app),
            vec!["steward-c"],
            "a worker sees neither the lead's crons nor a sibling worker's",
        );
    }

    #[test]
    fn refresh_gotify_shows_only_the_leads_own_subscriptions() {
        let (mut app, ws, _) = app_on_project("scoped", "lead-uuid");
        ws.seed_test_gotify_subscription(sub_owned_by(1, "scoped", None));
        ws.seed_test_gotify_subscription(sub_owned_by(2, "scoped", Some("steward")));

        app.refresh_gotify();

        assert_eq!(sub_ids(&app), vec![1], "a lead sees only lead-created subscriptions");
    }

    #[test]
    fn refresh_gotify_shows_only_the_workers_own_subscriptions() {
        let (mut app, ws, key) = app_on_project("scoped", "steward-uuid");
        seed_live_worker(&ws, "scoped", "steward", &key);
        ws.seed_test_gotify_subscription(sub_owned_by(1, "scoped", None));
        ws.seed_test_gotify_subscription(sub_owned_by(2, "scoped", Some("steward")));
        ws.seed_test_gotify_subscription(sub_owned_by(3, "scoped", Some("reviewer")));

        app.refresh_gotify();

        assert_eq!(
            sub_ids(&app),
            vec![2],
            "a worker sees neither the lead's subscriptions nor a sibling worker's",
        );
    }

    /// A session that created nothing leaves both caches empty, which is
    /// what makes the Inspector omit both sections rather than draw a
    /// bare header.
    #[test]
    fn refresh_leaves_both_caches_empty_for_a_session_owning_nothing() {
        let (mut app, ws, key) = app_on_project("scoped", "steward-uuid");
        seed_live_worker(&ws, "scoped", "steward", &key);
        ws.seed_test_cron(cron_owned_by("lead-c", "scoped", None));
        ws.seed_test_gotify_subscription(sub_owned_by(1, "scoped", None));

        app.refresh_forge_crons();
        app.refresh_gotify();

        assert!(app.forge_crons.is_empty(), "no owned cron leaves the SCHEDULES cache empty");
        assert!(app.forge_schedule_rows.is_empty(), "and no humanized rows to render");
        assert!(app.gotify_subs.is_empty(), "no owned subscription leaves the GOTIFY cache empty");
    }

    #[test]
    fn test_default_seeds_pre_connect_bucket_so_accessors_are_infallible() {
        let app = App::test_default();
        // Task 3 onwards: per-session field accessors (messages, viewport,
        // ...) need an active session to read/write. test_default seeds a
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

    #[test]
    fn peer_envelope_insert_shifts_stop_hook_summary_index() {
        use crate::app::state::types::StopHookSummaryState;
        use crate::app::{ChatMessage, MessageBlock, MessageRole, TextBlock};
        let mut app = App::test_default();
        let msg = |t: &str| {
            ChatMessage::new(
                MessageRole::User,
                vec![MessageBlock::Text(TextBlock::from_complete(t))],
            )
        };
        let bound_idx = app.messages().len();
        app.push_message_tracked(msg("bound"));
        app.set_last_stop_hook_summary(Some(StopHookSummaryState {
            message_idx: bound_idx,
            actions: 1,
            hooks: Vec::new(),
        }));
        // A peer envelope inserts before the summary's bound message; the
        // chip's anchor index must follow it down.
        app.insert_message_tracked(bound_idx, msg("peer"));
        assert_eq!(app.last_stop_hook_summary().map(|s| s.message_idx), Some(bound_idx + 1));
    }

    #[test]
    fn remove_shifts_then_clears_stop_hook_summary_index() {
        use crate::app::state::types::StopHookSummaryState;
        use crate::app::{ChatMessage, MessageBlock, MessageRole, TextBlock};
        let mut app = App::test_default();
        let msg = |t: &str| {
            ChatMessage::new(
                MessageRole::User,
                vec![MessageBlock::Text(TextBlock::from_complete(t))],
            )
        };
        let base = app.messages().len();
        app.push_message_tracked(msg("before"));
        app.push_message_tracked(msg("bound"));
        let bound_idx = base + 1;
        app.set_last_stop_hook_summary(Some(StopHookSummaryState {
            message_idx: bound_idx,
            actions: 1,
            hooks: Vec::new(),
        }));
        // Removing a message before the anchor decrements its index.
        app.remove_message_tracked(base);
        assert_eq!(app.last_stop_hook_summary().map(|s| s.message_idx), Some(bound_idx - 1));
        // Removing the anchor itself clears the summary.
        app.remove_message_tracked(bound_idx - 1);
        assert!(app.last_stop_hook_summary().is_none());
    }

    /// Clicking a launchpad-auto_started project triggers the
    /// per-session refresh chain (status / oauth / context-usage /
    /// 5h+7d) so the bottom panel's bars populate on the destination
    /// session, not just on connect.
    ///
    /// An Edit-family name breaks a tool-call run, so this renders as
    /// `RenderUnit::Individual` and is handed `app.tools_collapsed`
    /// directly instead of a group-derived level; grouped tools get
    /// that flag overwritten and never reach the measure call. Plain
    /// text content rather than a `Diff` keeps it out of the carve-out.
    fn ungrouped_tool_message(id: &str) -> ChatMessage {
        let mut msg = assistant_tool_message(id, model::ToolCallStatus::Failed);
        if let MessageBlock::ToolCall(tc) = &mut msg.blocks[0] {
            tc.sdk_tool_name = "Edit".to_owned();
            tc.title = format!("Edit {id}");
            tc.content =
                vec![model::ToolCallContent::from("alpha\nbeta\ngamma\ndelta\nepsilon".to_owned())];
        }
        msg
    }

    fn head_tool(app: &App) -> &ToolCallInfo {
        match &app.messages()[0].blocks[0] {
            MessageBlock::ToolCall(tc) => tc,
            _ => panic!("expected a tool call"),
        }
    }

    /// Draw a real chat frame, so the tool goes through the same
    /// grouping and measurement the running app puts it through.
    fn render_chat_frame(app: &mut App, width: u16, height: u16) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                crate::ui::chat::render(
                    frame,
                    ratatui::layout::Rect::new(0, 0, width, height),
                    app,
                    &[],
                );
            })
            .expect("draw");
    }

    /// ctrl+x flips the collapse preference App-wide, but
    /// `invalidate_layout`'s `Global` arm bumps only the ACTIVE
    /// session's viewport and clears overrides only on that session's
    /// blocks, and switching sessions invalidates nothing. So an
    /// unfocused session reaches its next render with width, layout
    /// epoch and generation all unmoved: without the preference in the
    /// key nothing forces a remeasure, and the tool keeps a height
    /// taken under the old preference. Mouse hit-testing sizes the
    /// click box from that height.
    #[test]
    fn cross_session_collapse_flip_remeasures_ungrouped_tool() {
        const W: u16 = 80;
        const H: u16 = 40;

        let mut app = make_test_app();
        app.tools_collapsed = true;
        let a_key = app.active_session_key.clone().expect("an active session");

        let b_key = forge_workspace::SessionKey::from_str_for_test("collapse-cross-session");
        let mut b_bucket = crate::app::session::UiSession::new(b_key.clone());
        b_bucket.messages = vec![ungrouped_tool_message("cross-session")];
        app.sessions.insert(b_key.clone(), b_bucket);

        // B renders once collapsed, stamping its per-tool key.
        app.switch_active_session(b_key.clone());
        render_chat_frame(&mut app, W, H);
        let collapsed_height = head_tool(&app).last_measured_height;
        let epoch_before = head_tool(&app).layout_epoch;
        let width_before = head_tool(&app).last_measured_width;
        let generation_before = app.viewport().layout_generation;

        // The flip happens while A is focused.
        app.switch_active_session(a_key);
        crate::app::keys::toggle_all_tool_calls(&mut app);
        assert!(!app.tools_collapsed, "ctrl+x expanded the shared preference");

        // Back to B at the same size, no resize and no click.
        app.switch_active_session(b_key);
        render_chat_frame(&mut app, W, H);
        assert_eq!(
            generation_before,
            app.viewport().layout_generation,
            "no resize, so B's generation must not move",
        );
        assert_eq!(epoch_before, head_tool(&app).layout_epoch, "and its layout epoch must not");
        assert_eq!(width_before, head_tool(&app).last_measured_width, "nor its measured width");
        let after_flip = head_tool(&app).last_measured_height;

        // What the same tool measures from cold under the new preference.
        let mut cold = make_test_app();
        cold.tools_collapsed = false;
        *cold.active_messages_mut() = vec![ungrouped_tool_message("cross-session")];
        render_chat_frame(&mut cold, W, H);
        let correct_height = head_tool(&cold).last_measured_height;

        assert_ne!(
            collapsed_height, correct_height,
            "the preference has to move this tool's height, or the assertion below is free",
        );
        assert_eq!(
            after_flip, correct_height,
            "B's tool kept a height measured under the old preference \
             (collapsed={collapsed_height}, correct={correct_height}, got={after_flip})",
        );
    }

    /// #651. The per-tool measurement key above repairs the tool's own
    /// cached height, but the viewport's per-message height is written
    /// only by the remeasure pass, and that pass skips any message
    /// whose stale bit is clear. So an unfocused session reaches its
    /// next render reporting rows measured under the old preference
    /// while painting the new one, and every row offset below it -
    /// scroll geometry, click hit-testing - is off by the difference.
    #[test]
    fn cross_session_collapse_flip_remeasures_background_viewport_height() {
        const W: u16 = 80;
        const H: u16 = 40;

        fn head_height(app: &App) -> usize {
            app.viewport().message_height(0)
        }

        let mut app = make_test_app();
        app.tools_collapsed = true;
        let a_key = app.active_session_key.clone().expect("an active session");
        *app.active_messages_mut() = vec![ungrouped_tool_message("cross-session")];

        let b_key = forge_workspace::SessionKey::from_str_for_test("collapse-background-height");
        let mut b_bucket = crate::app::session::UiSession::new(b_key.clone());
        b_bucket.messages = vec![ungrouped_tool_message("cross-session")];
        app.sessions.insert(b_key.clone(), b_bucket);

        // Both sessions measure once under the collapsed preference.
        render_chat_frame(&mut app, W, H);
        app.switch_active_session(b_key.clone());
        render_chat_frame(&mut app, W, H);
        let collapsed_height = head_height(&app);

        // The flip happens while A is focused.
        app.switch_active_session(a_key);
        crate::app::keys::toggle_all_tool_calls(&mut app);
        assert!(!app.tools_collapsed, "ctrl+x expanded the shared preference");
        render_chat_frame(&mut app, W, H);
        let active_height = head_height(&app);

        // Back to B at the same size, no resize and no click.
        app.switch_active_session(b_key);
        render_chat_frame(&mut app, W, H);
        let background_height = head_height(&app);

        // What the same message measures from cold under the new preference.
        let mut cold = make_test_app();
        cold.tools_collapsed = false;
        *cold.active_messages_mut() = vec![ungrouped_tool_message("cross-session")];
        render_chat_frame(&mut cold, W, H);
        let expanded_height = head_height(&cold);

        assert_ne!(
            collapsed_height, expanded_height,
            "the preference has to move this message's height, or the assertions below are free",
        );
        assert_eq!(
            active_height, expanded_height,
            "the focused session's viewport must remeasure on the flip \
             (collapsed={collapsed_height}, correct={expanded_height}, got={active_height})",
        );
        assert_eq!(
            background_height, expanded_height,
            "the background session's viewport kept a height measured under the old preference \
             (collapsed={collapsed_height}, correct={expanded_height}, got={background_height})",
        );
    }

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
        // Hold the destination's command receiver alive at test scope -
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
    /// seeded from `std::env::current_dir()` - forge.toml is the
    /// source of truth (Hard Rule #14). In launchpad mode (no argv
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
        // bucket's `cwd_raw` - there is no way to construct a forge
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
    /// `forge.toml`-or-empty, not from `current_dir()` - so it
    /// cannot accidentally match a real project's `path`.
    #[test]
    fn find_running_bucket_for_path_returns_matching_real_bucket() {
        let mut app = App::test_default();
        let project_path = "/Users/developer/Projects/forge";
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
        assert!(app.find_running_bucket_for_path("/Users/developer/Projects/forge").is_none());
    }

    /// Regression for commit 23f46b8: when a worker session shares
    /// the project's cwd_raw with the lead, `find_running_bucket_
    /// for_path` must return the lead's session_key, never the
    /// worker's. Before the fix, HashMap iteration order could
    /// surface either bucket non-deterministically and the projects-
    /// pane click landed on a worker instead of going back to the
    /// lead.
    #[test]
    fn find_running_bucket_for_path_excludes_worker_session_keys() {
        use forge_workspace::WorkerEntry;
        use forge_workspace::{ProjectKey, SessionKey};

        let mut app = App::test_default();
        let project_path = "/Users/developer/Projects/forge";

        let lead_key = SessionKey::from_str_for_test("aaaaaaaa-1111-2222-3333-444444444444");
        let worker_key = SessionKey::from_str_for_test("bbbbbbbb-1111-2222-3333-444444444444");

        let mut lead_bucket = crate::app::session::UiSession::new(lead_key.clone());
        lead_bucket.cwd_raw = project_path.to_owned();
        app.sessions.insert(lead_key.clone(), lead_bucket);

        let mut worker_bucket = crate::app::session::UiSession::new(worker_key.clone());
        worker_bucket.cwd_raw = project_path.to_owned();
        app.sessions.insert(worker_key.clone(), worker_bucket);

        // Inject the worker into the workspace's live_workers map so
        // the filter inside find_running_bucket_for_path sees it.
        let workspace = app.workspace.as_ref().expect("test_default wires a workspace");
        let project_key = ProjectKey::new_for_test("-Users-developer-Projects-forge");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "test-worker".to_owned(),
                charter: "noop".to_owned(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: lead_key.as_str().to_owned(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        let picked = app
            .find_running_bucket_for_path(project_path)
            .expect("lead bucket should match even with a worker at the same cwd");
        assert_eq!(picked, lead_key, "lead must be returned; worker must be excluded");
        assert_ne!(picked, worker_key);
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
        usage.context_usage_refresh_pending = Some(crate::app::state::types::RefreshPending::Auto);
        usage.last_compaction_pre_tokens = Some(123_456);
        {
            let bucket = app.active_bucket_mut();
            bucket.dictate_overrides.styling = Some(forge_workspace::Styling::Formal);
            bucket.dictate_device_pin =
                Some(forge_workspace::DictateDeviceChoice::Device("shure-id".into()));
        }

        app.clear_session_runtime_identity();

        assert!(app.session_id().is_none());
        assert!(app.current_model().is_none());
        assert!(app.mode().is_none());
        assert_eq!(*app.session_usage(), SessionUsageState::default());
        let bucket = app.active_bucket_mut();
        assert_eq!(
            bucket.dictate_overrides,
            forge_workspace::DictateOverrides::default(),
            "a torn-down identity keeps no override mirrors"
        );
        assert_eq!(
            bucket.dictate_device_pin, None,
            "a torn-down identity keeps no device pin: the workspace holds none"
        );
    }

    #[test]
    fn clear_session_runtime_identity_clears_observed_assistant_model() {
        let mut app = App::test_default();
        app.set_observed_assistant_model(Some("claude-observed".to_owned()));

        app.clear_session_runtime_identity();

        assert!(app.observed_assistant_model().is_none());
    }

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

    /// Identity-layer sibling of the finalize exemption: a still-running
    /// backgrounded root and its children keep their scope across
    /// turn-complete so SUBAGENTS can still identify them; a main-agent
    /// scope always clears, and a completed root's scope drops on the next
    /// clear once it leaves the roster - so nothing leaks.
    #[test]
    fn clear_tool_scope_tracking_retains_live_backgrounded_scopes_then_drops_them() {
        use crate::app::state::types::{BackgroundTask, ToolCallScope};

        let mut app = make_test_app();
        app.tool_call_scopes_mut().insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
        app.tool_call_scopes_mut().insert(
            "toolu_child".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
        );
        app.tool_call_scopes_mut().insert("toolu_main".to_owned(), ToolCallScope::MainAgent);
        app.insert_session_task_mapping("task-root".to_owned(), "toolu_root".to_owned());
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "task-root".to_owned(),
            task_type: "local_agent".to_owned(),
            description: String::new(),
        }];

        // Turn-complete while the agent is still backgrounded.
        app.clear_tool_scope_tracking();
        assert!(app.tool_call_scope("toolu_root").is_some(), "live backgrounded root retained");
        assert!(app.tool_call_scope("toolu_child").is_some(), "its child retained");
        assert!(app.tool_call_scope("toolu_main").is_none(), "main-agent scope always cleared");

        // The task completes + drops from the roster; the next clear drops it.
        app.remove_session_task_mapping("task-root");
        app.clear_tool_scope_tracking();
        assert!(app.tool_call_scope("toolu_root").is_none(), "completed root scope dropped");
        assert!(app.tool_call_scope("toolu_child").is_none(), "orphaned child scope dropped");
    }

    /// A backgrounded subagent's children used to hold their scopes until
    /// the ROOT settled, so the scope map grew with the subagent's total
    /// tool-call count (#791). A child whose own card is terminal cannot
    /// be swept into anything - sweeps only touch open calls - so its
    /// scope drops at the turn boundary; the root and still-open
    /// children stay.
    #[test]
    fn terminal_children_drop_their_scope_at_the_turn_boundary() {
        use crate::app::state::types::{BackgroundTask, ToolCallScope};

        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_root", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_open_child", model::ToolCallStatus::InProgress));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_done_child", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_dead_child", model::ToolCallStatus::Failed));
        for (idx, id) in ["toolu_root", "toolu_open_child", "toolu_done_child", "toolu_dead_child"]
            .into_iter()
            .enumerate()
        {
            app.index_tool_call(id.to_owned(), idx, 0);
        }
        app.tool_call_scopes_mut().insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
        for id in ["toolu_open_child", "toolu_done_child", "toolu_dead_child"] {
            app.tool_call_scopes_mut().insert(
                id.to_owned(),
                ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
            );
        }
        app.insert_session_task_mapping("task-root".to_owned(), "toolu_root".to_owned());
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "task-root".to_owned(),
            task_type: "local_agent".to_owned(),
            description: String::new(),
        }];

        app.clear_tool_scope_tracking();

        assert!(app.tool_call_scope("toolu_root").is_some(), "live root kept");
        assert!(
            app.tool_call_scope("toolu_open_child").is_some(),
            "the open child keeps its scope - it still needs the sweep exemption",
        );
        assert!(
            app.tool_call_scope("toolu_done_child").is_none(),
            "a terminal child's scope drops at the boundary",
        );
        assert!(
            app.tool_call_scope("toolu_dead_child").is_none(),
            "a failed child's scope drops too",
        );
    }

    /// The pin behind the rule above: a terminal nested Task may drop its
    /// scope, but a grandchild still running under it must not lose its
    /// sweep exemption - a nested Task that is terminal-yet-backgrounded
    /// carries its own roster row, so the grandchild resolves to IT as a
    /// live root, not through the dropped scope.
    #[test]
    fn a_live_grandchild_is_not_stranded_behind_its_terminal_nested_parent() {
        use crate::app::state::types::{BackgroundTask, ToolCallScope};

        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_root", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_nested", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_gchild", model::ToolCallStatus::InProgress));
        for (idx, id) in ["toolu_root", "toolu_nested", "toolu_gchild"].into_iter().enumerate() {
            app.index_tool_call(id.to_owned(), idx, 0);
        }
        app.tool_call_scopes_mut().insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
        app.tool_call_scopes_mut().insert(
            "toolu_nested".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
        );
        app.tool_call_scopes_mut().insert(
            "toolu_gchild".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_nested".to_owned() },
        );
        app.insert_session_task_mapping("task-root".to_owned(), "toolu_root".to_owned());
        app.insert_session_task_mapping("task-nested".to_owned(), "toolu_nested".to_owned());
        *app.background_tasks_mut() = vec![
            BackgroundTask {
                task_id: "task-root".to_owned(),
                task_type: "local_agent".to_owned(),
                description: String::new(),
            },
            BackgroundTask {
                task_id: "task-nested".to_owned(),
                task_type: "local_agent".to_owned(),
                description: String::new(),
            },
        ];

        app.clear_tool_scope_tracking();
        assert!(
            app.tool_call_scope("toolu_gchild").is_some(),
            "the grandchild's scope survives its parent's drop",
        );
        assert_eq!(
            app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed),
            0,
            "and the sweep spares it while it runs",
        );
    }

    /// The turn-boundary sweeps answer liveness per open call (#793);
    /// they must not derive the eager alive-with-children set off the
    /// whole scope map - that cost scales with the map #791 just
    /// bounded. The debug record in `backgrounded_alive_with_children`
    /// is the probe: a sweep site must not emit it, while the sweep
    /// still spares exactly the live work. The probe is introduced by
    /// this PR, so this test is mutation-verified rather than red on
    /// main - deleting the eager call cannot exist as a prior state.
    #[test]
    fn the_turn_boundary_sweep_does_not_build_the_eager_exempt_set() {
        use std::sync::{Arc, Mutex};

        #[derive(Default, Clone)]
        struct EventNames(Arc<Mutex<Vec<String>>>);

        struct CollectEventName(String);

        impl tracing::field::Visit for CollectEventName {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "event_name" {
                    self.0 = format!("{value:?}").trim_matches('"').to_owned();
                }
            }
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventNames {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut visitor = CollectEventName(String::new());
                event.record(&mut visitor);
                if !visitor.0.is_empty() {
                    self.0.lock().expect("capture").push(visitor.0);
                }
            }
        }

        use crate::app::state::types::{BackgroundTask, ToolCallScope};
        use tracing_subscriber::layer::SubscriberExt;

        let names = EventNames::default();
        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_root", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_child", model::ToolCallStatus::InProgress));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_plain_bash", model::ToolCallStatus::InProgress));
        for (idx, id) in ["toolu_root", "toolu_child", "toolu_plain_bash"].into_iter().enumerate() {
            app.index_tool_call(id.to_owned(), idx, 0);
        }
        app.tool_call_scopes_mut().insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
        app.tool_call_scopes_mut().insert(
            "toolu_child".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
        );
        app.insert_session_task_mapping("task-root".to_owned(), "toolu_root".to_owned());
        *app.background_tasks_mut() = vec![BackgroundTask {
            task_id: "task-root".to_owned(),
            task_type: "local_agent".to_owned(),
            description: String::new(),
        }];

        let subscriber = tracing_subscriber::registry().with(names.clone());
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(
                app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed),
                1,
                "only the unrelated bash sweeps; the live child is exempt either way",
            );
        });
        assert!(
            !names
                .0
                .lock()
                .expect("capture")
                .iter()
                .any(|name| name == "backgrounded_alive_set_built"),
            "the sweep derived the eager exempt set off the scope map; saw {:?}",
            names.0.lock().expect("capture"),
        );
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

        let changed = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed);

        assert_eq!(changed, 1);
        let MessageBlock::ToolCall(tc) = &app.messages()[0].blocks[0] else {
            panic!("expected tool call");
        };
        assert_eq!(tc.status, model::ToolCallStatus::Completed);
        assert_eq!(tc.terminal_id, None);
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

    fn focus_test_app_with_available_targets() -> App {
        let mut app = make_test_app();
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

        app.claim_focus_target(FocusTarget::Mention);
        assert_eq!(app.focus_owner(), FocusOwner::Mention);

        app.release_focus_target(FocusTarget::Mention);
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

    // -----------------------------------------------------------
    // replay-orphan Monitor state.
    // -----------------------------------------------------------

    #[test]
    fn upsert_monitor_during_replay_starts_in_a_terminal_state() {
        // During `load_resume_history` (replay_in_progress = true)
        // the wire walker doesn't re-emit terminal `task_updated`
        // events into the status setter. A replayed Monitor that
        // historically completed must NOT be reconstructed as
        // Running, otherwise it blocks
        // `clear_monitors_if_all_terminal` for any live sibling.
        let mut app = make_test_app();
        app.replay_in_progress = true;
        app.upsert_monitor_from_tool_input(
            "tu_replay",
            "historical monitor".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        let monitors = app.monitors();
        assert_eq!(monitors.len(), 1);
        assert_eq!(
            monitors[0].status,
            crate::app::state::types::MonitorStatus::Completed,
            "a replay-inserted monitor starts terminal so it stops blocking the \
             all-terminal clear, and Completed because the seed is a placeholder \
             rather than evidence the watched command failed",
        );
    }

    #[test]
    fn upsert_monitor_live_path_still_starts_running() {
        // Outside replay (replay_in_progress = false), live Monitor
        // tool_use events keep their existing Running default so the
        // ◉ glyph + " · running" badge animate while the watched
        // command runs.
        let mut app = make_test_app();
        assert!(!app.replay_in_progress, "live default");
        app.upsert_monitor_from_tool_input(
            "tu_live",
            "live monitor".to_owned(),
            "true".to_owned(),
            true,
            300_000,
        );
        let monitors = app.monitors();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].status, crate::app::state::types::MonitorStatus::Running);
    }

    /// The replay seed must not leak into the live path: a workflow
    /// launched now has to render as in progress and keep the WORKFLOWS
    /// section open until the wire says otherwise. Seeding terminal
    /// unconditionally would drain the section at the first status flip
    /// of any sibling, while the workflow was still running.
    #[test]
    fn upsert_workflow_live_path_still_starts_in_progress() {
        let mut app = make_test_app();
        assert!(!app.replay_in_progress, "live default");
        app.upsert_workflow_from_tool_input("tu_live_wf", "nightly-sweep".to_owned(), None);
        let workflows = app.workflows();
        assert_eq!(workflows.len(), 1);
        assert_eq!(
            workflows[0].status,
            crate::app::state::types::WorkflowStatus::InProgress,
            "a live workflow starts in progress",
        );
    }

    // -----------------------------------------------------------
    // #302 redux: replay-orphan Schedule entries (cron + wakeup).
    // Mirror of the Monitor orphan-suppression pattern above. The
    // CLI kills session-only crons + all wakeups at session close,
    // but the persisted ScheduleEntry replays on resume - without
    // these guards, the SCHEDULES section surfaces phantoms.
    // -----------------------------------------------------------

    #[test]
    fn upsert_cron_during_replay_skips_recurring_cron() {
        let mut app = make_test_app();
        app.replay_in_progress = true;
        let now = std::time::SystemTime::now();

        app.upsert_cron_from_tool_input("tu-orphan", "*/5 * * * *", "", true, now);

        assert!(
            app.schedules().is_empty(),
            "recurring crons replayed during resume must NOT push an entry; got: {:?}",
            app.schedules()
        );
    }

    #[test]
    fn upsert_cron_during_replay_skips_one_shot_cron() {
        let mut app = make_test_app();
        app.replay_in_progress = true;
        let now = std::time::SystemTime::now();

        app.upsert_cron_from_tool_input("tu-once", "48 16 24 4 *", "", false, now);

        assert!(
            app.schedules().is_empty(),
            "a replayed one-shot cron already fired and auto-deleted; got: {:?}",
            app.schedules()
        );
    }

    #[test]
    fn upsert_cron_outside_replay_pushes_both_kinds() {
        let mut app = make_test_app();
        assert!(!app.replay_in_progress, "live default");
        let now = std::time::SystemTime::now();

        app.upsert_cron_from_tool_input("tu-live-recurring", "* * * * *", "", true, now);
        app.upsert_cron_from_tool_input("tu-live-once", "48 16 24 4 *", "", false, now);

        assert_eq!(
            app.schedules().len(),
            2,
            "live operation pushes both kinds; got: {:?}",
            app.schedules()
        );
    }

    #[test]
    fn upsert_wakeup_during_replay_is_suppressed() {
        let mut app = make_test_app();
        app.replay_in_progress = true;
        let fire_at = std::time::SystemTime::now() + std::time::Duration::from_secs(60);

        app.upsert_wakeup_from_tool_input("tu-wake", "loop poll", fire_at);

        assert!(
            app.schedules().is_empty(),
            "wakeups replayed during resume must NOT push an entry; got: {:?}",
            app.schedules()
        );
    }

    #[test]
    fn upsert_wakeup_outside_replay_pushes_normally() {
        let mut app = make_test_app();
        assert!(!app.replay_in_progress, "live default");
        let fire_at = std::time::SystemTime::now() + std::time::Duration::from_secs(60);

        app.upsert_wakeup_from_tool_input("tu-live-wake", "poll", fire_at);

        assert_eq!(
            app.schedules().len(),
            1,
            "live wakeups push normally; got: {:?}",
            app.schedules()
        );
    }

    // -----------------------------------------------------------
    // auto-clear race against task_notification.
    // -----------------------------------------------------------

    #[test]
    fn set_monitor_status_no_longer_clears_implicitly() {
        // Pre-#277 the status setter called
        // `clear_monitors_if_all_terminal` at its end. That dropped
        // single-monitor entries before `task_notification` could
        // stamp the tail. Bug 5a deferred the trigger to
        // `handle_task_notification`. Confirm the setter no longer
        // drains the Vec on its own.
        let mut app = make_test_app();
        app.upsert_monitor_from_tool_input(
            "tu_solo",
            "solo monitor".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        app.stamp_monitor_task_id("tu_solo", "task_solo".to_owned());
        app.set_monitor_status_by_task_id(
            "task_solo",
            crate::app::state::types::MonitorStatus::Completed,
        );
        // Entry survives the status flip - waiting for
        // handle_task_notification to call the clear.
        assert_eq!(app.monitors().len(), 1);
        assert_eq!(app.monitors()[0].status, crate::app::state::types::MonitorStatus::Completed,);
    }

    #[test]
    fn explicit_clear_drains_when_all_terminal() {
        // The clear helper is now `pub` so `handle_task_notification`
        // can call it. Verify the predicate still drains correctly.
        let mut app = make_test_app();
        app.upsert_monitor_from_tool_input("tu_a", "a".to_owned(), "true".to_owned(), false, 0);
        app.upsert_monitor_from_tool_input("tu_b", "b".to_owned(), "true".to_owned(), false, 0);
        app.stamp_monitor_task_id("tu_a", "task_a".to_owned());
        app.stamp_monitor_task_id("tu_b", "task_b".to_owned());
        app.set_monitor_status_by_task_id(
            "task_a",
            crate::app::state::types::MonitorStatus::Completed,
        );
        app.set_monitor_status_by_task_id(
            "task_b",
            crate::app::state::types::MonitorStatus::Completed,
        );
        // Without the explicit call the entries persist (Bug 5a).
        assert_eq!(app.monitors().len(), 2);
        app.clear_monitors_if_all_terminal();
        assert!(app.monitors().is_empty());
    }

    #[test]
    fn explicit_clear_skips_when_any_still_running() {
        let mut app = make_test_app();
        app.upsert_monitor_from_tool_input(
            "tu_run",
            "still running".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        app.upsert_monitor_from_tool_input(
            "tu_done",
            "done".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        app.stamp_monitor_task_id("tu_done", "task_done".to_owned());
        app.set_monitor_status_by_task_id(
            "task_done",
            crate::app::state::types::MonitorStatus::Completed,
        );
        app.clear_monitors_if_all_terminal();
        // Predicate sees the Running entry and skips the drain.
        assert_eq!(app.monitors().len(), 2);
    }

    #[test]
    fn replay_restored_monitor_accepts_terminal_completed_event() {
        // Replay inserts the entry in Stopped. A subsequent terminal
        // `task_updated` (routed via `set_monitor_status_by_task_id`)
        // re-flips Stopped -> Completed. After #277 Bug 5a the
        // setter no longer drains the section implicitly, so the
        // entry persists post-flip and the invariant is checkable
        // directly. The `expect` makes the test fail loudly if a
        // future refactor restores the implicit clear and the
        // entry goes missing.
        let mut app = make_test_app();
        app.replay_in_progress = true;
        app.upsert_monitor_from_tool_input(
            "tu_replay",
            "historical monitor".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        // Stamp task_id so the by_task_id setter can find it.
        app.stamp_monitor_task_id("tu_replay", "task_x".to_owned());
        app.set_monitor_status_by_task_id(
            "task_x",
            crate::app::state::types::MonitorStatus::Completed,
        );
        let monitor = app
            .monitors()
            .first()
            .expect("replay-restored entry must persist post-Bug-5a setter call");
        assert_eq!(
            monitor.status,
            crate::app::state::types::MonitorStatus::Completed,
            "terminal event must re-flip the replay-restored entry",
        );
    }

    #[test]
    fn group_collapse_level_defaults_to_l2_when_absent() {
        use crate::ui::message::grouping::{GroupCollapseLevel, GroupId};
        let app = App::test_default();
        let id = GroupId::from_leader_id("tu-x");
        assert_eq!(app.group_collapse_level(&id), GroupCollapseLevel::L2Summary);
    }

    /// Cmd+X with no prior click flips the global `tools_collapsed`
    /// flag and emits a Global invalidation. Per-group cycling is
    /// bound to mouse-click on a group summary row; the keyboard
    /// shortcut is the global toggle, always.
    #[test]
    fn cmd_x_with_no_prior_click_toggles_global_tools_collapsed() {
        let mut app = App::test_default();
        let initial = app.tools_collapsed;
        app.last_invalidation_level.set(None);
        super::super::keys::toggle_all_tool_calls(&mut app);
        assert_eq!(app.tools_collapsed, !initial, "Cmd+X must flip tools_collapsed globally",);
        assert_eq!(
            app.last_invalidation_level.get(),
            Some(crate::app::InvalidationLevel::Global),
            "Cmd+X must emit Global invalidation",
        );
    }

    /// Cmd+X clears every tool-call's `collapsed_override` across
    /// the active session's message list so older / scrolled-up
    /// tools snap to the global state on the flip - per-tool
    /// overrides don't survive Cmd+X.
    #[test]
    fn cmd_x_clears_collapsed_override_on_all_tool_calls() {
        use crate::agent::model;
        use crate::app::{BlockCache, ChatMessage, MessageBlock, MessageRole, ToolCallInfo};
        let mut app = App::test_default();
        let push_tool = |app: &mut App, id: &str, override_val: bool| {
            app.active_messages_mut().push(ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(ToolCallInfo {
                    id: id.to_owned(),
                    title: format!("Read {id}"),
                    sdk_tool_name: "Read".to_owned(),
                    raw_input: None,
                    raw_input_bytes: 0,
                    output_metadata: None,
                    task_metadata: None,
                    status: model::ToolCallStatus::Completed,
                    content: Vec::new(),
                    hidden: false,
                    terminal_id: None,
                    terminal_output: None,
                    monitor_output_tail: Vec::default(),
                    monitor_status: None,
                    render_epoch: 0,
                    layout_epoch: 0,
                    last_measured_y_in_msg: 0,
                    answered_questions: Vec::new(),
                    last_measured_height: 0,
                    last_measured_width: 0,
                    last_measured_layout_epoch: 0,
                    last_measured_layout_generation: 0,
                    last_measured_tools_collapsed: false,
                    cache: BlockCache::default(),
                    collapsed_override: Some(override_val),
                }))],
            ));
        };
        push_tool(&mut app, "tu-a", true);
        push_tool(&mut app, "tu-b", false);

        let read_override = |app: &App, id: &str| -> Option<bool> {
            app.active_session()
                .expect("active session")
                .messages
                .iter()
                .find_map(|msg| {
                    msg.blocks.iter().find_map(|b| match b {
                        MessageBlock::ToolCall(tc) if tc.id == id => Some(tc.collapsed_override),
                        _ => None,
                    })
                })
                .expect("tool found")
        };
        assert_eq!(read_override(&app, "tu-a"), Some(true));
        assert_eq!(read_override(&app, "tu-b"), Some(false));

        super::super::keys::toggle_all_tool_calls(&mut app);

        assert_eq!(
            read_override(&app, "tu-a"),
            None,
            "tool A's collapsed_override must clear on Cmd+X",
        );
        assert_eq!(
            read_override(&app, "tu-b"),
            None,
            "tool B's collapsed_override must clear on Cmd+X",
        );
    }

    /// Cmd+X clears every peer-inbound text block's
    /// `peer_collapsed_override` so MCP messages snap to the global
    /// state on the flip - per-peer-block overrides don't survive
    /// Cmd+X.
    #[test]
    fn cmd_x_clears_peer_collapsed_override_on_all_text_blocks() {
        let mut app = App::test_default();
        let push_peer = |app: &mut App, sender: &str, override_val: bool| {
            let text =
                format!("[Message id=t-12345678 from agent '{sender}' (org 'Personal')]\n\nhi");
            let mut block = TextBlock::from_complete(&text);
            block.peer_collapsed_override = Some(override_val);
            app.active_messages_mut().push(ChatMessage::new_peer_envelope(
                MessageRole::User,
                vec![MessageBlock::Text(block)],
            ));
        };
        push_peer(&mut app, "peer-a", true);
        push_peer(&mut app, "peer-b", false);

        let read_override = |app: &App, msg_idx: usize| -> Option<bool> {
            match &app.active_session().expect("session").messages[msg_idx].blocks[0] {
                MessageBlock::Text(b) => b.peer_collapsed_override,
                _ => panic!("expected text block"),
            }
        };
        assert_eq!(read_override(&app, 0), Some(true));
        assert_eq!(read_override(&app, 1), Some(false));

        super::super::keys::toggle_all_tool_calls(&mut app);

        assert_eq!(
            read_override(&app, 0),
            None,
            "peer A's peer_collapsed_override must clear on Cmd+X",
        );
        assert_eq!(
            read_override(&app, 1),
            None,
            "peer B's peer_collapsed_override must clear on Cmd+X",
        );
    }

    /// Cmd+X clears the `group_collapse_levels` map so older /
    /// scrolled-up groups snap to the global state on the flip -
    /// per-group cycle state doesn't survive Cmd+X.
    #[test]
    fn cmd_x_clears_group_collapse_levels_map() {
        use crate::ui::message::grouping::GroupId;
        let mut app = App::test_default();
        let group_a = GroupId::from_leader_id("tu-leader-a");
        let group_b = GroupId::from_leader_id("tu-leader-b");
        let _ = app.cycle_group_collapse_level(&group_a);
        let _ = app.cycle_group_collapse_level(&group_b);
        assert!(
            app.active_session().expect("session").group_collapse_levels.contains_key(&group_a),
            "group A's level recorded pre-Cmd+X",
        );
        assert!(
            app.active_session().expect("session").group_collapse_levels.contains_key(&group_b),
            "group B's level recorded pre-Cmd+X",
        );

        super::super::keys::toggle_all_tool_calls(&mut app);

        assert!(
            app.active_session().expect("session").group_collapse_levels.is_empty(),
            "group_collapse_levels must be cleared on Cmd+X",
        );
    }

    /// Regression-lock: after Cmd+X clears overrides, the per-tool
    /// `collapsed_override` field is still writable so the next
    /// click can set a fresh per-tool override. The click path is
    /// unchanged; the clear is only at Cmd+X time.
    #[test]
    fn click_on_tool_after_cmd_x_sets_fresh_collapsed_override() {
        use crate::agent::model;
        use crate::app::{BlockCache, ChatMessage, MessageBlock, MessageRole, ToolCallInfo};
        fn read_override(app: &App) -> Option<bool> {
            app.active_session()
                .expect("session")
                .messages
                .iter()
                .find_map(|msg| {
                    msg.blocks.iter().find_map(|b| match b {
                        MessageBlock::ToolCall(tc) if tc.id == "tu-a" => {
                            Some(tc.collapsed_override)
                        }
                        _ => None,
                    })
                })
                .expect("tool found")
        }
        fn set_override(app: &mut App, value: Option<bool>) {
            for msg in app.active_messages_mut() {
                for b in &mut msg.blocks {
                    if let MessageBlock::ToolCall(tc) = b
                        && tc.id == "tu-a"
                    {
                        tc.collapsed_override = value;
                        return;
                    }
                }
            }
            panic!("tool not found");
        }
        let mut app = App::test_default();
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(ToolCallInfo {
                id: "tu-a".to_owned(),
                title: "Read tu-a".to_owned(),
                sdk_tool_name: "Read".to_owned(),
                raw_input: None,
                raw_input_bytes: 0,
                output_metadata: None,
                task_metadata: None,
                status: model::ToolCallStatus::Completed,
                content: Vec::new(),
                hidden: false,
                terminal_id: None,
                terminal_output: None,
                monitor_output_tail: Vec::default(),
                monitor_status: None,
                render_epoch: 0,
                layout_epoch: 0,
                last_measured_y_in_msg: 0,
                answered_questions: Vec::new(),
                last_measured_height: 0,
                last_measured_width: 0,
                last_measured_layout_epoch: 0,
                last_measured_layout_generation: 0,
                last_measured_tools_collapsed: false,
                cache: BlockCache::default(),
                collapsed_override: Some(true),
            }))],
        ));

        super::super::keys::toggle_all_tool_calls(&mut app);

        assert_eq!(read_override(&app), None, "Cmd+X cleared the override");

        // Simulate a click setting a fresh override post-Cmd+X.
        set_override(&mut app, Some(false));
        assert_eq!(
            read_override(&app),
            Some(false),
            "post-Cmd+X mutation must set a fresh collapsed_override",
        );
    }

    #[test]
    fn cycle_group_collapse_level_walks_l2_l1_l0_back_to_l2() {
        use crate::ui::message::grouping::{GroupCollapseLevel, GroupId};
        let mut app = App::test_default();
        let id = GroupId::from_leader_id("tu-x");
        assert_eq!(app.cycle_group_collapse_level(&id), GroupCollapseLevel::L1Titles);
        assert_eq!(app.group_collapse_level(&id), GroupCollapseLevel::L1Titles);
        assert_eq!(app.cycle_group_collapse_level(&id), GroupCollapseLevel::L0Bodies);
        assert_eq!(app.cycle_group_collapse_level(&id), GroupCollapseLevel::L2Summary);
    }

    // ─── SUBAGENTS Inspector view (subagents_view) ─────────────────
    //
    // Helpers build a session with a Task root + N SubagentChild
    // tool calls underneath it. Each child is registered via
    // `register_tool_call_scope` so `subagents_view` can group them.

    fn make_subagent_root_tc(
        id: &str,
        subagent_type: &str,
        description: &str,
        status: model::ToolCallStatus,
    ) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: "Task".to_owned(),
            sdk_tool_name: "Task".to_owned(),
            raw_input: Some(serde_json::json!({
                "subagent_type": subagent_type,
                "description": description,
                "prompt": description,
            })),
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_output: None,
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
        }
    }

    fn make_subagent_child_tc(id: &str, sdk_tool_name: &str, title: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: title.to_owned(),
            sdk_tool_name: sdk_tool_name.to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: model::ToolCallStatus::Completed,
            content: Vec::new(),
            hidden: true,
            terminal_id: None,
            terminal_output: None,
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
        }
    }

    fn push_subagent_session(
        app: &mut App,
        root: ToolCallInfo,
        children: Vec<ToolCallInfo>,
    ) -> String {
        let root_id = root.id.clone();
        app.register_tool_call_scope(root_id.clone(), ToolCallScope::SubagentRoot);
        let mut blocks: Vec<MessageBlock> = Vec::with_capacity(1 + children.len());
        blocks.push(MessageBlock::ToolCall(Box::new(root)));
        for child in children {
            app.register_tool_call_scope(
                child.id.clone(),
                ToolCallScope::SubagentChild { parent_tool_use_id: root_id.clone() },
            );
            blocks.push(MessageBlock::ToolCall(Box::new(child)));
        }
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, blocks));
        root_id
    }

    /// One running root + a handful of children produces one entry
    /// in the SUBAGENTS view. The label combines subagent_type with
    /// the first line of description; the tail carries each child's
    /// `sdk_tool_name` + `title` in chronological order; total_count
    /// matches the actual children pushed.
    #[test]
    fn subagents_view_collects_roots_and_children() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-1",
            "Explore",
            "map hidden tool calls\nadditional context line",
            model::ToolCallStatus::InProgress,
        );
        let children = vec![
            make_subagent_child_tc("tu-c-1", "Grep", "Grep SubagentChild"),
            make_subagent_child_tc("tu-c-2", "Read", "Read inspector_pane.rs"),
            make_subagent_child_tc("tu-c-3", "Bash", "git log --oneline -3"),
        ];
        push_subagent_session(&mut app, root, children);

        let view = app.subagents_view();
        assert_eq!(view.len(), 1, "one running root produces one entry; got {view:?}");
        let entry = &view[0];
        assert_eq!(entry.tool_use_id, "tu-root-1");
        assert_eq!(
            entry.label, "Explore · map hidden tool calls",
            "label combines subagent_type + first line of description; got {:?}",
            entry.label,
        );
        assert_eq!(entry.status, model::ToolCallStatus::InProgress);
        assert_eq!(entry.total_count, 3);
        assert_eq!(entry.tail.len(), 3);
        assert_eq!(entry.tail[0].sdk_tool_name, "Grep");
        assert_eq!(entry.tail[1].sdk_tool_name, "Read");
        assert_eq!(entry.tail[2].sdk_tool_name, "Bash");
        assert_eq!(entry.tail[2].title, "git log --oneline -3");
    }

    #[test]
    fn subagent_label_from_root_combines_type_and_description() {
        let root = make_subagent_root_tc(
            "tu-label-1",
            "Explore",
            "Map the pipeline",
            model::ToolCallStatus::InProgress,
        );
        assert_eq!(subagent_label_from_root(&root), "Explore · Map the pipeline");
    }

    /// With neither `subagent_type` nor `description`, the label falls
    /// back to `sdk_tool_name` (here `"Task"`).
    #[test]
    fn subagent_label_from_root_falls_back_to_tool_name_on_empty_input() {
        let mut root = make_subagent_root_tc(
            "tu-label-2",
            "Explore",
            "Map the pipeline",
            model::ToolCallStatus::InProgress,
        );
        root.raw_input = Some(serde_json::json!({}));
        assert_eq!(subagent_label_from_root(&root), "Task");
    }

    /// Tail cap: more than [`SUBAGENT_TAIL_CAP`] children -> tail
    /// surfaces only the LAST N (most recent), total_count counts
    /// every child registered under the root.
    #[test]
    fn subagents_view_tail_caps_at_constant() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-2",
            "code-reviewer",
            "review the diff",
            model::ToolCallStatus::InProgress,
        );
        // 6 children -> tail cap (4) keeps only the LAST 4: c-3..c-6.
        let mut children = Vec::new();
        for i in 1..=6 {
            children.push(make_subagent_child_tc(
                &format!("tu-c-{i}"),
                "Read",
                &format!("file-{i}.rs"),
            ));
        }
        push_subagent_session(&mut app, root, children);

        let view = app.subagents_view();
        assert_eq!(view.len(), 1);
        let entry = &view[0];
        assert_eq!(entry.total_count, 6, "total_count counts every child");
        assert_eq!(
            entry.tail.len(),
            SUBAGENT_TAIL_CAP,
            "tail caps at SUBAGENT_TAIL_CAP; got {} entries",
            entry.tail.len(),
        );
        assert_eq!(
            entry.tail.first().map(|c| c.title.as_str()),
            Some("file-3.rs"),
            "tail drops the oldest children (file-1, file-2); got {:?}",
            entry.tail,
        );
        assert_eq!(
            entry.tail.last().map(|c| c.title.as_str()),
            Some("file-6.rs"),
            "tail ends with the newest child; got {:?}",
            entry.tail,
        );
    }

    /// Auto-clear: when every root in the session is at a terminal
    /// status the view returns empty, mirroring
    /// `clear_workflows_if_all_terminal` so the section disappears.
    #[test]
    fn subagents_view_returns_empty_when_every_root_is_terminal() {
        let mut app = App::test_default();
        let root_a = make_subagent_root_tc(
            "tu-root-a",
            "Explore",
            "first",
            model::ToolCallStatus::Completed,
        );
        let children_a = vec![make_subagent_child_tc("tu-c-a", "Read", "foo.rs")];
        push_subagent_session(&mut app, root_a, children_a);
        let root_b = make_subagent_root_tc(
            "tu-root-b",
            "code-reviewer",
            "second",
            model::ToolCallStatus::Failed,
        );
        push_subagent_session(&mut app, root_b, Vec::new());

        assert!(
            app.subagents_view().is_empty(),
            "every-terminal session must auto-clear the view; got {:?}",
            app.subagents_view(),
        );
    }

    /// The Inspector's section gate only needs a bool, so it uses
    /// `has_active_subagent_root` rather than building the view and
    /// throwing it away. The two must agree on every state that flips
    /// the gate, or the section appears and disappears wrongly.
    #[test]
    fn has_active_subagent_root_matches_subagents_view_emptiness() {
        fn check(label: &str, app: &App) {
            assert_eq!(
                app.has_active_subagent_root(),
                !app.subagents_view().is_empty(),
                "{label}: predicate disagreed with the view it stands in for",
            );
        }

        check("no dispatch", &App::test_default());

        let mut all_terminal = App::test_default();
        push_subagent_session(
            &mut all_terminal,
            make_subagent_root_tc("tu-a", "Explore", "done", model::ToolCallStatus::Completed),
            vec![make_subagent_child_tc("tu-a-c", "Read", "foo.rs")],
        );
        push_subagent_session(
            &mut all_terminal,
            make_subagent_root_tc("tu-b", "code-reviewer", "gone", model::ToolCallStatus::Failed),
            Vec::new(),
        );
        check("every root terminal", &all_terminal);

        // Resumed shape (#808): the root card replays unscoped and a
        // live child frame names it. The two walks are separately
        // implemented, so the new derivation is agreed on exactly here.
        let resumed_with = |child_status: model::ToolCallStatus| {
            let mut app = App::test_default();
            let mut child = make_subagent_child_tc("tu-resumed-c", "Bash", "sleep");
            child.status = child_status;
            app.active_messages_mut().push(ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(make_subagent_root_tc(
                    "tu-resumed",
                    "Explore",
                    "resumed",
                    model::ToolCallStatus::Completed,
                )))],
            ));
            app.register_tool_call_scope(
                "tu-resumed-c".to_owned(),
                ToolCallScope::SubagentChild { parent_tool_use_id: "tu-resumed".to_owned() },
            );
            app.active_messages_mut().push(ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(child))],
            ));
            app
        };
        check(
            "resumed unscoped root, live child",
            &resumed_with(model::ToolCallStatus::InProgress),
        );
        check(
            "resumed unscoped root, settled child",
            &resumed_with(model::ToolCallStatus::Completed),
        );

        let mut mixed = App::test_default();
        push_subagent_session(
            &mut mixed,
            make_subagent_root_tc(
                "tu-done",
                "code-reviewer",
                "done",
                model::ToolCallStatus::Completed,
            ),
            Vec::new(),
        );
        push_subagent_session(
            &mut mixed,
            make_subagent_root_tc(
                "tu-run",
                "Explore",
                "running",
                model::ToolCallStatus::InProgress,
            ),
            Vec::new(),
        );
        check("one root still running", &mixed);

        let mut pending = App::test_default();
        push_subagent_session(
            &mut pending,
            make_subagent_root_tc("tu-pend", "Explore", "queued", model::ToolCallStatus::Pending),
            Vec::new(),
        );
        check("pending root", &pending);

        let mut backgrounded = App::test_default();
        push_subagent_session(
            &mut backgrounded,
            make_subagent_root_tc("tu-bg", "Explore", "bg scan", model::ToolCallStatus::Completed),
            Vec::new(),
        );
        backgrounded.insert_session_task_mapping("task-bg".to_owned(), "tu-bg".to_owned());
        *backgrounded.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-bg".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "bg scan".to_owned(),
        }];
        check("terminal root still alive in the session roster", &backgrounded);

        let mut orphan_child = App::test_default();
        orphan_child.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(make_subagent_child_tc(
                "tu-orphan",
                "Read",
                "x.rs",
            )))],
        ));
        check("tool call carrying no registered scope", &orphan_child);
    }

    /// Mixed terminal + in-progress roots: ANY in-progress keeps the
    /// section visible. Returns BOTH roots so the user can see the
    /// completed one's `· N tools` summary next to the running one's
    /// live tail.
    #[test]
    fn subagents_view_keeps_terminal_roots_when_others_still_running() {
        let mut app = App::test_default();
        let done = make_subagent_root_tc(
            "tu-root-done",
            "code-reviewer",
            "review the diff",
            model::ToolCallStatus::Completed,
        );
        let done_children = vec![
            make_subagent_child_tc("tu-c-done-1", "Read", "diff.rs"),
            make_subagent_child_tc("tu-c-done-2", "Grep", "old"),
        ];
        push_subagent_session(&mut app, done, done_children);
        let running = make_subagent_root_tc(
            "tu-root-run",
            "Explore",
            "ongoing",
            model::ToolCallStatus::InProgress,
        );
        push_subagent_session(&mut app, running, Vec::new());

        let view = app.subagents_view();
        assert_eq!(view.len(), 2, "both roots present while one is in-progress; got {view:?}");
        let done_entry = view.iter().find(|e| e.tool_use_id == "tu-root-done").expect("done");
        assert_eq!(done_entry.total_count, 2);
        assert_eq!(done_entry.status, model::ToolCallStatus::Completed);
        assert!(
            done_entry.tail.is_empty(),
            "terminal root carries no live tail (the section renders `· N tools` from total_count instead); got {:?}",
            done_entry.tail,
        );
        let running_entry = view.iter().find(|e| e.tool_use_id == "tu-root-run").expect("run");
        assert!(
            running_entry.tail.is_empty()
                || running_entry.tail.len() <= crate::app::SUBAGENT_TAIL_CAP,
            "in-progress root's tail respects the cap; got {:?}",
            running_entry.tail,
        );
    }

    /// No subagent dispatches in the session -> empty view (section
    /// stays hidden).
    #[test]
    fn subagents_view_empty_when_no_subagent_dispatch() {
        let app = App::test_default();
        assert!(app.subagents_view().is_empty());
    }

    /// Regression: a subagent the CLI backgrounds gets an immediate
    /// sentinel tool_result that flips its root card to terminal while
    /// the subagent keeps running. Liveness comes from the session roster
    /// (`background_tasks` intersected with the session task map), so the
    /// section must stay visible for the task's true lifetime even though
    /// the card status reads
    /// terminal - mirroring the PROCESSES section.
    #[test]
    fn subagents_view_keeps_backgrounded_root_alive_via_session_roster() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-bg",
            "Explore",
            "long-running background scan",
            model::ToolCallStatus::Completed,
        );
        push_subagent_session(&mut app, root, Vec::new());
        // task_started recorded the session-scoped mapping and the CLI
        // registry lists it as live; no terminal task_updated has drained
        // it yet.
        app.insert_session_task_mapping("task-bg".to_owned(), "tu-root-bg".to_owned());
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-bg".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "long-running background scan".to_owned(),
        }];

        let view = app.subagents_view();
        assert_eq!(
            view.len(),
            1,
            "a backgrounded-but-alive subagent stays in the SUBAGENTS view; got {view:?}",
        );
        assert_eq!(view[0].tool_use_id, "tu-root-bg");
    }

    /// Companion to the keeps-alive test: a backgrounded root whose
    /// sentinel status reads terminal but that is still live in the
    /// session roster must render as *running* - InProgress status
    /// (spinner, no `· N tools` summary) AND its live tool tail
    /// preserved. Deriving the row from `root.status` alone would mark a
    /// still-working task done and drop its tail.
    #[test]
    fn subagents_view_backgrounded_alive_root_shows_running_with_tail() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-bg2",
            "Explore",
            "long-running background scan",
            model::ToolCallStatus::Completed,
        );
        // More children than the cap so this also exercises the tail cap
        // on the alive-via-registry path (the existing cap test drives an
        // InProgress-status root instead).
        let child_count = SUBAGENT_TAIL_CAP + 2;
        let mut children = Vec::new();
        for i in 1..=child_count {
            children.push(make_subagent_child_tc(
                &format!("tu-bg-c{i}"),
                "Read",
                &format!("bg-file-{i}.rs"),
            ));
        }
        push_subagent_session(&mut app, root, children);
        app.insert_session_task_mapping("task-bg2".to_owned(), "tu-root-bg2".to_owned());
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-bg2".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "long-running background scan".to_owned(),
        }];

        let view = app.subagents_view();
        assert_eq!(view.len(), 1, "alive backgrounded root stays; got {view:?}");
        assert_eq!(
            view[0].status,
            model::ToolCallStatus::InProgress,
            "alive backgrounded root must render running, not its sentinel-terminal status; got {:?}",
            view[0].status,
        );
        assert_eq!(
            view[0].total_count, child_count,
            "total_count counts every child; got {}",
            view[0].total_count,
        );
        assert_eq!(
            view[0].tail.len(),
            SUBAGENT_TAIL_CAP,
            "alive backgrounded root keeps its live tail, capped at SUBAGENT_TAIL_CAP; got {:?}",
            view[0].tail,
        );
    }

    /// Regression (unify-activity): a backgrounded AGENT that outlives its
    /// spawning turn. The sentinel flips the root terminal and turn
    /// finalisation wipes the turn-scoped alive set, so the turn-scoped
    /// path drops it - and an agent has no OS process to fall back to. The
    /// session-scoped `background_tasks` registry (agent kind, resolved via
    /// the session-scoped task map) must keep it in SUBAGENTS with its
    /// tail, mirroring how WORKFLOWS survives across turns.
    #[test]
    fn subagents_view_keeps_backgrounded_agent_alive_via_registry_after_turn_reset() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-bg-agent",
            "Explore",
            "long-running background agent",
            model::ToolCallStatus::Completed,
        );
        let child = make_subagent_child_tc("tu-bg-agent-c1", "Read", "conv-row.tsx");
        push_subagent_session(&mut app, root, vec![child]);
        // task_started recorded the session-scoped mapping (survives reset).
        app.insert_session_task_mapping("task-bg-agent".to_owned(), "tu-root-bg-agent".to_owned());
        // The CLI registry still lists it as a live backgrounded agent.
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-bg-agent".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "long-running background agent".to_owned(),
        }];
        // Turn finalisation wiped the turn-scoped liveness.
        let _: () = app.with_turn_state_mut(|ts| {
            ts.task_tool_use_ids.clear();
        });

        let view = app.subagents_view();
        assert_eq!(
            view.len(),
            1,
            "a backgrounded agent still in the registry survives turn reset; got {view:?}",
        );
        assert_eq!(view[0].tool_use_id, "tu-root-bg-agent");
        assert_eq!(
            view[0].status,
            model::ToolCallStatus::InProgress,
            "registry-alive backgrounded agent renders running; got {:?}",
            view[0].status,
        );
        assert_eq!(
            view[0].tail.len(),
            1,
            "its live tool tail is preserved; got {:?}",
            view[0].tail
        );
    }

    /// Locks the intersection design: the session map alone must NOT keep a
    /// root alive - the `background_tasks` registry is the authoritative
    /// gate. A terminal-status root with a session-map entry but an EMPTY
    /// registry (and wiped turn state) auto-clears. Guards against a future
    /// refactor dropping the registry gate (which would resurrect stale
    /// leaked map entries as phantom live rows).
    #[test]
    fn subagents_view_session_map_without_registry_does_not_keep_root_alive() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-gate",
            "Explore",
            "finished agent",
            model::ToolCallStatus::Completed,
        );
        push_subagent_session(&mut app, root, Vec::new());
        // Map entry present (e.g. a leaked mapping), but the registry is
        // empty and the turn-scoped liveness is wiped.
        app.insert_session_task_mapping("task-gate".to_owned(), "tu-root-gate".to_owned());
        let _: () = app.with_turn_state_mut(|ts| {
            ts.task_tool_use_ids.clear();
        });

        assert!(
            app.subagents_view().is_empty(),
            "a session-map entry alone (no registry gate) must not keep a terminal root alive; got {:?}",
            app.subagents_view(),
        );
    }

    /// A freshly-dispatched root sits at `Pending` (queued `○`) until the
    /// CLI reports progress. The liveness promotion is only for a
    /// backgrounded root whose sentinel flipped it terminal - it must NOT
    /// fire for a not-yet-started `Pending` root just because that root
    /// counts as active for the section gate.
    #[test]
    fn subagents_view_pending_root_stays_pending() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-pending",
            "Explore",
            "queued scan",
            model::ToolCallStatus::Pending,
        );
        push_subagent_session(&mut app, root, Vec::new());

        let view = app.subagents_view();
        assert_eq!(view.len(), 1, "a pending root still shows in the section; got {view:?}");
        assert_eq!(
            view[0].status,
            model::ToolCallStatus::Pending,
            "a not-yet-started root stays Pending (queued), not forced to the running spinner; got {:?}",
            view[0].status,
        );
    }
}
