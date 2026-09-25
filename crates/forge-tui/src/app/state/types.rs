use crate::agent::model;

pub use forge_primitives::runtime::{ModeInfo, ModeState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HelpView {
    #[default]
    Keys,
    SlashCommands,
    Subagents,
}

/// Login hint displayed when authentication is required during connection.
/// Rendered as a banner above the input field.
pub struct LoginHint {
    pub method_name: String,
    pub method_description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingCommandAck {
    CurrentMode,
    CurrentModel,
}

/// Snapshot of a `Message::StopHookSummary` event bound to an
/// assistant message in chat (#273). Rendered as a collapsed
/// 1-liner with `[▶ expand]`; the expanded view enumerates the
/// per-hook breakdown.
#[derive(Debug, Clone)]
pub struct StopHookSummaryState {
    /// Owning assistant message id, used to anchor the chip + body
    /// when re-rendering on scroll.
    pub message_idx: usize,
    /// Number of hooks that fired. Wire `hookCount`.
    pub actions: u32,
    /// Per-hook command + duration. Wire `hookInfos`.
    pub hooks: Vec<StopHookEntry>,
}

/// One row in the expanded stop-hook summary.
#[derive(Debug, Clone)]
pub struct StopHookEntry {
    pub command: String,
    /// Milliseconds the hook took; `None` on 2.1.263's plugin-injected
    /// entries, which carry no `durationMs`.
    pub duration_ms: Option<u64>,
}

/// Lifecycle status of a Monitor (`Monitor` tool_use).
/// A Monitor row stays surfaced until ALL session monitors transition
/// to a terminal variant (`Stopped` / `Completed` / `TimedOut`); the
/// MONITORS Inspector section auto-clears when no monitor is still
/// `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStatus {
    /// Monitor is active. Persistent monitors stay `Running` until
    /// TaskStop or session end; non-persistent monitors run until
    /// their `timeout_ms` expires or the watched command exits.
    Running,
    /// Monitor terminated via TaskStop / killed / clean exit.
    Stopped,
    /// Monitor completed cleanly (synonym for Stopped on the
    /// renderer; preserved as a distinct variant in case downstream
    /// callers want to disambiguate normal-exit from explicit-kill).
    Completed,
    /// Monitor's `timeout_ms` fired. Renderer surfaces a distinct
    /// `· timed out` badge so users see the failure mode at a glance.
    TimedOut,
}

/// A single Monitor entry surfaced in chat + the
/// Inspector MONITORS section. Populated on Monitor tool_use,
/// updated on terminal lifecycle events (TaskStop, task_updated
/// with `status: stopped|killed|failed`, Result origin marker for
/// timeout).
#[derive(Debug, Clone)]
pub struct MonitorEntry {
    /// `tool_use_id` from the Monitor `tool_use` block - the
    /// canonical id chat-stream / Inspector / mouse routing all
    /// reference.
    pub tool_use_id: String,
    /// Task id assigned by the CLI when the Monitor starts (extracted
    /// from `tool_use_result.taskId`). `None` until the start
    /// confirmation arrives. Used to correlate against `TaskStarted`
    /// / `TaskUpdated` wire events.
    pub task_id: Option<String>,
    /// `tool_input.description` - the headline label.
    pub description: String,
    /// `tool_input.command` - the watched shell command.
    pub command: String,
    /// `tool_input.persistent` - when true the Monitor stays alive
    /// across multiple events; when false a single exit ends it.
    pub persistent: bool,
    /// `tool_input.timeout_ms` - zero when no explicit timeout
    /// (persistent monitors typically pass zero).
    pub timeout_ms: u64,
    /// Lifecycle status. Drives MONITORS-section visibility and the
    /// chat one-liner (`◉ Monitor started · ...` vs `◉ Monitor
    /// stopped · ...`).
    pub status: MonitorStatus,
    /// Path to the local-bash task's `output_file`
    /// (CLI writes the watched command's stdout here). Stamped from
    /// `task_notification.output_file`. The Monitor section reads
    /// this on `task_notification` / `task_progress` events to
    /// refresh the visible tail. `None` when the wire hasn't carried
    /// a file path yet (Monitor just started).
    pub output_file: Option<std::path::PathBuf>,
    /// Rolling 12-line tail of monitor output (most-recent at the
    /// end). Bounded so a long-running Monitor doesn't grow the
    /// Inspector pane indefinitely. #275 Task 4: now populated from
    /// the on-disk `output_file` (the actual watched-command stdout)
    /// rather than from `task_notification.summary` (which only ever
    /// carried "Monitor X stream ended" / similar boilerplate).
    pub output_tail: std::collections::VecDeque<String>,
    /// Per-row expand toggle for the Inspector section. Click on the
    /// row in the Inspector flips this; `false` collapses to a
    /// one-liner row.
    pub expanded_in_inspector: bool,
}

impl MonitorEntry {
    /// Maximum lines kept in `output_tail`. Bounded so the Inspector
    /// row doesn't grow unbounded for a long-lived monitor.
    pub const OUTPUT_TAIL_MAX: usize = 12;

    /// True when this entry is still actively watching. Used by the
    /// MONITORS-section visibility predicate (`section drops when no
    /// running monitor remains`).
    pub fn is_running(&self) -> bool {
        self.status == MonitorStatus::Running
    }
}

/// A CLI-tracked background task from a `background_tasks_changed`
/// event's snapshot. `local_bash` entries feed the Inspector PROCESSES
/// section (deduped against the OS scan); agents surface in their own
/// section. `task_type` names the kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTask {
    pub task_id: String,
    pub task_type: String,
    pub description: String,
}

impl BackgroundTask {
    /// Whether this task's kind routes to an Inspector section at all:
    /// `local_bash` to PROCESSES, an agent kind to SUBAGENTS. One list,
    /// shared by the drift warning in `handle_background_tasks_changed`
    /// and the Projects-pane row glyph - a kind the CLI renames on one
    /// side only renders nowhere while its spinner keeps turning.
    pub(crate) fn routes_to_inspector_section(&self) -> bool {
        matches!(self.task_type.as_str(), "local_bash" | "agent" | "local_agent")
    }
}

/// What forge saw of a rostered task's tool card when its `task_started`
/// mapping was made: whether the card was in the messages at all, and the
/// wire command when it carried one. Every Inspector section paints from that
/// card, so a rostered task with none draws nothing.
///
/// Recorded once, at the mapping, while the card is normally the newest thing
/// in the messages; a `task_started` whose card forge cannot find records
/// `card_seen: false` instead. The row glyph and the PROCESSES feed read these
/// facts rather than the history, which is what keeps the frame-tick gate off
/// a per-tick message sweep.
///
/// A card the history drops after the mapping leaves these facts standing: a
/// pruned agent card still promotes the row glyph, while a pruned bash card
/// still draws, from the recorded command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTaskCard {
    pub tool_use_id: String,
    pub card_seen: bool,
    /// The card's wire command, for the kinds that carry one. `None` for a
    /// card with no `command` field (an agent dispatch) and for no card.
    pub command: Option<String>,
}

impl SessionTaskCard {
    /// A mapping whose card forge could not find - a `task_started` arriving
    /// before its tool call, or a fixture that seeds the mapping alone.
    pub fn unseen(tool_use_id: String) -> Self {
        Self { tool_use_id, card_seen: false, command: None }
    }
}

/// Kind of a schedule entry in the Inspector SCHEDULES section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleKind {
    /// One-shot `ScheduleWakeup` (the /loop dynamic-pacing mechanism).
    Wakeup,
    /// A forge cron (`mcp__forge__cron`). `recurring` distinguishes
    /// repeat vs one-shot.
    Cron { recurring: bool },
}

/// A pending time-based schedule surfaced in the Inspector SCHEDULES
/// section. Wakeups carry a concrete `fire_at`; forge crons carry the
/// humanized schedule in `schedule`.
#[derive(Debug, Clone)]
pub struct ScheduleEntry {
    pub kind: ScheduleKind,
    /// Wakeup: the `reason`. Cron: the headline candidate - the
    /// prompt's first line.
    pub label: String,
    /// Cron only: the human "what/why" from `cron__create`, preferred
    /// over `label` as the row headline. `None` for wakeups and crons
    /// without one.
    pub description: Option<String>,
    /// Cron only: the humanized schedule (`daily at 09:00`, `today
    /// 14:30`). Empty for wakeups.
    pub schedule: String,
    /// When it fires: `now + delaySeconds` for a wakeup, the cron's
    /// next occurrence for a forge cron.
    pub fire_at: Option<std::time::SystemTime>,
    /// When the entry was created; informational for wakeups.
    pub created_at: std::time::SystemTime,
}

impl ScheduleEntry {
    /// True when `now` is at/after the entry's validity end. Only a
    /// wakeup expires here: a forge cron's lifetime is the store's, and
    /// its row is refreshed from there each tick rather than pruned
    /// against this struct.
    pub fn is_expired(&self, now: std::time::SystemTime) -> bool {
        matches!(self.kind, ScheduleKind::Wakeup) && self.fire_at.is_some_and(|t| now >= t)
    }
}

/// What a session needs attention for, in the Inspector NEEDS
/// ATTENTION band. Prompt kinds derive from the front
/// `PromptState.source`; `Failed` derives from
/// [`crate::app::session::UiSession::failed_turn`] and `ReviewReplies`
/// from [`crate::app::session::UiSession::review_replies_waiting`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionKind {
    /// A `can_use_tool` permission request; `tool` is the raw tool
    /// name (e.g. `Bash`, `mcp__forge__agents__spawn`), not a display
    /// title.
    Permission { tool: String },
    /// An `AskUserQuestion` request.
    Question,
    /// The session's last turn died after the CLI exhausted its own
    /// retries. Renders red rather than yellow: nothing is being asked
    /// of the user, the turn is simply gone.
    Failed { error: forge_primitives::ApiRetryError, status: Option<u16> },
    /// A worker answered review comments this session filed and nobody
    /// has come back to them. Ranks below the other two - nothing is
    /// blocked on it - and covers the sessions the GIT header badge
    /// can't, since the band excludes the active one.
    ReviewReplies { count: usize },
}

/// Worker answers on a session's review threads that are still owed a
/// reviewer turn, parked on the session bucket so the Inspector GIT
/// badge and the NEEDS ATTENTION band both read one field instead of
/// querying the store per frame. Fed by
/// [`forge_workspace::SessionUpdate::ReviewActivityNotice`] and
/// recomputed authoritatively whenever `/diff` hydrates its threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRepliesWaiting {
    /// The branch the count is about. The GIT badge suppresses itself
    /// once the header describes a different branch, since `/diff`
    /// would open on that one instead.
    pub branch: String,
    pub count: usize,
    /// When the signal first appeared, so the band's wait-age is the
    /// real one rather than the last recompute.
    pub since: std::time::SystemTime,
}

impl ReviewRepliesWaiting {
    /// Fold a fresh count for `branch` into the prior signal. Zero
    /// clears it, but only a reviewer turn retires an answer - an empty
    /// result for one branch says nothing about the branch a live count
    /// belongs to, so that one survives. A still-live count on the same
    /// branch keeps its original `since` so a recompute can't reset the
    /// wait-age.
    pub fn merge(prior: Option<&Self>, branch: &str, count: usize) -> Option<Self> {
        if count == 0 {
            return prior.filter(|p| p.branch != branch).cloned();
        }
        let since = prior
            .filter(|p| p.branch == branch)
            .map_or_else(std::time::SystemTime::now, |p| p.since);
        Some(Self { branch: branch.to_owned(), count, since })
    }
}

/// A turn that ended in error, retained on the session bucket so the
/// Inspector band and the Projects pane keep surfacing it until the
/// user attends to the session or it runs another turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedTurn {
    /// Wire classification, taken from the last `api_retry` seen
    /// during the turn and [`forge_primitives::ApiRetryError::Unknown`]
    /// when the turn died without one.
    pub error: forge_primitives::ApiRetryError,
    /// Raw HTTP status behind `error`, when the CLI reported one.
    pub status: Option<u16>,
    pub failed_at: std::time::SystemTime,
}

/// One background session needing the user, rendered as a row in
/// the Inspector NEEDS ATTENTION band. Built by
/// [`crate::app::App::needs_attention_sessions`], sorted stalest-first.
#[derive(Debug, Clone)]
pub struct AttentionEntry {
    /// Session to switch to when the row is clicked.
    pub session_key: forge_workspace::SessionSlot,
    /// Project name (white bold in the row).
    pub name: String,
    /// Worker role in parens (dim), when the session is a worker.
    pub role: Option<String>,
    pub kind: AttentionKind,
    /// When the prompt entered the queue, or when the turn failed; the
    /// row's wait-age is `now - enqueued_at` and the band sorts
    /// oldest-first.
    pub enqueued_at: std::time::SystemTime,
}

/// One row in the SUBAGENTS Inspector section's per-root tail. The
/// otherwise-hidden child tool call this row represents - the
/// underlying `ToolCallInfo` stays `hidden: true` in the chat
/// stream, the Inspector surface is the only place it appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentChildEntry {
    /// `sdk_tool_name` from the child `ToolCallInfo`. Drives the
    /// kind glyph + label via `theme::tool_name_label`, same as a
    /// chat tool row.
    pub sdk_tool_name: String,
    /// Already-shortened title (`tc.title` after the standard
    /// `shorten_tool_title` pass at tool-use arrival).
    pub title: String,
    pub status: model::ToolCallStatus,
}

/// One SUBAGENTS Inspector entry: a `Task` / `Agent` dispatch
/// (the visible root) plus the last [`SUBAGENT_TAIL_CAP`] of its
/// otherwise-hidden child tool calls. Built per-render by
/// `App::subagents_view` from the active session's message list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentEntry {
    pub tool_use_id: String,
    /// `subagent_type · first-line-of-description` when both are in
    /// `raw_input`; falls back to whichever piece is present, then
    /// the raw sdk_tool_name (`Task` / `Agent`).
    pub label: String,
    pub status: model::ToolCallStatus,
    /// Last [`SUBAGENT_TAIL_CAP`] children (block order), populated only
    /// while the root is running. A terminal root instead renders the
    /// trailing `· N tools` summary; a queued root shows neither.
    pub tail: Vec<SubagentChildEntry>,
    /// Total visible-or-hidden child tool calls registered under
    /// this root - the truncated `tail` may carry fewer entries.
    pub total_count: usize,
}

/// Maximum children kept in the live tail before rolling into the
/// `+N more` overflow on the terminal-summary line. Picked to match
/// the SUBAGENTS mockup in `docs/book/src/ui/inspector-processes.md`.
pub const SUBAGENT_TAIL_CAP: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSessionInfo {
    pub session_id: String,
    pub summary: String,
    pub last_modified_ms: u64,
    pub cwd: Option<String>,
    pub custom_title: Option<String>,
    pub first_prompt: Option<String>,
}

// Wire-shape usage types live in forge-primitives::usage; re-exported
// here so existing import paths (`crate::app::UsageSnapshot`, etc.)
// keep resolving.
pub use forge_primitives::usage::{ExtraUsage, UsageSnapshot, UsageSourceKind, UsageWindow};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageState {
    pub snapshot: Option<UsageSnapshot>,
    pub in_flight: bool,
    pub last_error: Option<String>,
}

/// Which refresh class is queued behind an in-flight
/// `get_context_usage` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPending {
    Auto,
    Forced,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionUsageState {
    pub last_compaction_trigger: Option<model::CompactionTrigger>,
    pub last_compaction_pre_tokens: Option<u64>,
    /// Compactions this session has been through: seeded at connect from
    /// the transcript, incremented on each live boundary. Seeded rather
    /// than counted per run because nothing else persists it across a
    /// resume.
    pub compaction_count: u32,
    pub context_usage_percent: Option<u8>,
    /// Raw model context-window size in tokens (e.g. 200_000 for
    /// Sonnet's base cap, 1_000_000 for the 1M variant). Read by
    /// the projects-pane footer to render `200K` / `1M` beneath
    /// the Ctx bar. `None` until the first ContextUsage poll
    /// returns a snapshot for this session.
    pub context_max_tokens: Option<u64>,
    pub context_usage_in_flight: bool,
    /// Class of the refresh queued behind the in-flight one. A queued
    /// forced refresh replays past the gates when the response lands.
    pub context_usage_refresh_pending: Option<RefreshPending>,
    /// When the last `get_context_usage` was actually sent for this
    /// session. Bounds the auto refresh to one send per
    /// `CONTEXT_USAGE_MIN_SEND_INTERVAL`.
    pub context_usage_last_sent: Option<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct McpState {
    pub servers: Vec<forge_primitives::McpServerStatus>,
    pub in_flight: bool,
    pub last_error: Option<String>,
    /// When the last snapshot request went out, for the background
    /// re-poll cadence. Stamped by every request path, not just the
    /// background one, so a manual refresh also defers the next poll.
    pub last_refresh_requested: Option<std::time::Instant>,
}

// Per-session SDK turn state lives in
// `forge_primitives::runtime::SessionTurnState`. Re-exported here
// so the existing `crate::app::state::types::SessionTurnState`
// import path resolves.
pub use forge_primitives::runtime::SessionTurnState;

pub const DEFAULT_RENDER_CACHE_BUDGET_BYTES: usize = 24 * 1024 * 1024;
pub const DEFAULT_HISTORY_RETENTION_MAX_BYTES: usize = 64 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderCacheBudget {
    pub max_bytes: usize,
    pub last_total_bytes: usize,
    pub total_evictions: usize,
}

impl Default for RenderCacheBudget {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_RENDER_CACHE_BUDGET_BYTES,
            last_total_bytes: 0,
            total_evictions: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryRetentionPolicy {
    pub max_bytes: usize,
}

impl Default for HistoryRetentionPolicy {
    fn default() -> Self {
        Self { max_bytes: DEFAULT_HISTORY_RETENTION_MAX_BYTES }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HistoryRetentionStats {
    pub total_before_bytes: usize,
    pub total_after_bytes: usize,
    pub dropped_messages: usize,
    pub dropped_bytes: usize,
    pub total_dropped_messages: usize,
    pub total_dropped_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheBudgetEnforceStats {
    pub total_before_bytes: usize,
    pub total_after_bytes: usize,
    pub evicted_bytes: usize,
    pub evicted_blocks: usize,
    /// Bytes in protected (non-evictable) blocks excluded from the budget comparison.
    pub protected_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    /// Waiting for bridge adapter connection (TUI shown, input disabled).
    Connecting,
    /// A slash command is in flight (input disabled, spinner shown).
    CommandPending,
    Ready,
    Thinking,
    Running,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallScope {
    MainAgent,
    SubagentRoot,
    SubagentChild { parent_tool_use_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Chat,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub row: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionState {
    pub kind: SelectionKind,
    pub start: SelectionPoint,
    pub end: SelectionPoint,
    pub dragging: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarDragState {
    /// Row offset from thumb top where the initial click happened.
    pub thumb_grab_offset: usize,
    /// Visible track length used when the drag started.
    pub track_space: usize,
    /// Maximum scrollable row offset when the drag started.
    pub max_scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteSessionState {
    pub id: u64,
    pub start: SelectionPoint,
    pub placeholder_index: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_entry_wakeup_expires_at_fire_time() {
        let t0 = std::time::SystemTime::UNIX_EPOCH;
        let fire = t0 + std::time::Duration::from_secs(60);
        let e = ScheduleEntry {
            kind: ScheduleKind::Wakeup,
            label: "poll".into(),
            description: None,
            schedule: String::new(),
            fire_at: Some(fire),
            created_at: t0,
        };
        assert!(!e.is_expired(t0));
        assert!(e.is_expired(fire));
        assert!(e.is_expired(fire + std::time::Duration::from_secs(1)));
    }

    #[test]
    fn monitor_entry_is_running_predicate_matches_status() {
        let mut entry = MonitorEntry {
            tool_use_id: "tu".to_owned(),
            task_id: None,
            description: "x".to_owned(),
            command: "y".to_owned(),
            persistent: false,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        };
        assert!(entry.is_running());
        entry.status = MonitorStatus::Stopped;
        assert!(!entry.is_running());
        entry.status = MonitorStatus::Completed;
        assert!(!entry.is_running());
        entry.status = MonitorStatus::TimedOut;
        assert!(!entry.is_running());
    }
}
