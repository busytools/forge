//! Command + SessionUpdate channel envelopes between forge-tui and
//! forge-workspace. TUI dispatches Commands; workspace per-session
//! tasks emit SessionUpdates back via a fan-in channel.
//!
//! ## Dual Command shape (deliberate)
//!
//! Two `Command` enums exist in the workspace: this one, keyed by
//! [`SessionKey`], and [`forge_primitives::AgentCommand`], keyed by
//! `session_id: String`. They overlap on variant names (Prompt,
//! Cancel, SetMode, …) but serve different boundary layers:
//!
//! - **`forge_workspace::protocol::Command`** is the TUI ↔ workspace
//!   envelope. SessionKey routing, App-level variants
//!   (SpawnProject / SpawnSession / StartDefault), the
//!   workspace-internal Respond* + MCP cluster.
//! - **`forge_primitives::AgentCommand`** is the workspace ↔ agent
//!   envelope. session_id-keyed, raw shapes the AgentHandle
//!   dispatcher recognises.
//!
//! Collapsing them would force the AgentHandle dispatcher to handle
//! App-level variants it has no business in (SpawnProject is a
//! workspace concern; SessionKey is a routing concern; neither
//! belongs in the agent layer). The current split keeps each
//! envelope minimal at its respective boundary. The translation
//! happens in `session_task::execute_command_via_handle`.

use std::path::PathBuf;

use forge_agent::client::SessionLaunchSettings;
use forge_primitives::cloud::oauth_credentials::OauthCredentials;
use forge_primitives::cloud::service_status::ServiceSeverity;
use forge_primitives::error::AppError;
use forge_primitives::permission::PermissionMode;
use forge_primitives::permission_ui::{PermissionOutcome, PermissionRequest};
use forge_primitives::plugins::{PluginsCliActionSuccess, PluginsInventorySnapshot};
use forge_primitives::question::{QuestionOutcome, QuestionRequest};
use forge_primitives::runtime::{AvailableModel, CurrentModel, ModeState, TerminalReason};
use forge_primitives::{
    AccountInfo, ForgeAccountIdentity, ImageAttachment, McpOperationError, McpServerStatus,
    Message, PeerInflightStats, SessionId, SessionListEntry,
};
use tokio::sync::oneshot;

use crate::SessionKey;
use crate::mcp::peers::types::WrappedPrompt;

// `TurnErrorClass` lives in forge-primitives so the classifier (in
// forge-agent) and consumers (in forge-tui, via this protocol module)
// share one enum. Re-exported here so existing call sites keep
// resolving via `forge_workspace::protocol::TurnErrorClass`.
pub use forge_primitives::TurnErrorClass;

/// One pending interaction response slot. Workspace stores these
/// keyed by `tool_id` in `DomainSession.pending_interactions`.
/// `Command::RespondPermission` / `RespondQuestion` look up the
/// matching slot and send the outcome down the oneshot.
pub enum PendingInteractionSlot {
    Permission(oneshot::Sender<PermissionOutcome>),
    Question(oneshot::Sender<QuestionOutcome>),
}

impl std::fmt::Debug for PendingInteractionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permission(_) => f.write_str("PendingInteractionSlot::Permission"),
            Self::Question(_) => f.write_str("PendingInteractionSlot::Question"),
        }
    }
}

/// Synchronous return from `Command::SpawnWorker` - the session_id
/// that the new worker was issued and the tag value applied. Threaded
/// back to the calling `workers__spawn` Tool impl via the oneshot
/// receiver so the LLM sees `{session_id, tag}` in the tool result.
#[derive(Debug, Clone)]
pub struct WorkerSpawnReply {
    pub session_id: String,
    pub tag: String,
}

/// Outcome of a [`Command::DespawnWorker`], sent back to the calling
/// `workers__despawn` Tool via the command's `respond` oneshot.
#[derive(Debug)]
pub enum DespawnResult {
    /// The worker was torn down (subprocess killed, dropped from
    /// `live_workers`, inflight asks expired). `worktree_cleanup_warning`
    /// is `Some` when the post-teardown `git worktree remove` failed -
    /// the worker is still gone; only the worktree directory lingers.
    /// Teardown and worktree cleanup are independent: a cleanup failure
    /// never rolls back the kill.
    Despawned { worktree_cleanup_warning: Option<String> },
    /// Despawn refused: the worktree has uncommitted/untracked changes
    /// or unpushed commits and `force` was not set. Nothing was torn
    /// down; the worker stays live. `reason` names what is dirty.
    Blocked { reason: String },
    /// No live worker matched `label` (already gone or never existed).
    NotFound,
}

/// Mutation kind for a `SessionUpdate::WorkerStatusChanged` event.
/// `Added` and `StatusChanged` carry a fresh `WorkerStatus` snapshot;
/// `Removed` carries the last-known snapshot for symmetry but the TUI
/// reducer treats it as a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatusAction {
    Added,
    Removed,
    StatusChanged,
}

/// Command envelope: forge-tui -> forge-workspace.
///
/// Every variant carries a `SessionKey` identifying the target
/// session task. `Workspace::dispatch` fans the variant into the
/// matching task's command receiver.
pub enum Command {
    Prompt {
        key: SessionKey,
        text: String,
        attachments: Vec<ImageAttachment>,
    },
    Cancel {
        key: SessionKey,
    },
    SetMode {
        key: SessionKey,
        mode: PermissionMode,
    },
    SetModel {
        key: SessionKey,
        model: String,
    },
    NewSession {
        key: SessionKey,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    },
    ResumeSession {
        key: SessionKey,
        session_id: String,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    },
    RespondPermission {
        key: SessionKey,
        tool_id: String,
        outcome: PermissionOutcome,
    },
    RespondQuestion {
        key: SessionKey,
        tool_id: String,
        outcome: QuestionOutcome,
    },
    /// Reconnect a configured MCP server.
    ReconnectMcpServer {
        key: SessionKey,
        server_name: String,
    },
    /// Toggle a configured MCP server on/off.
    ToggleMcpServer {
        key: SessionKey,
        server_name: String,
        enabled: bool,
    },
    /// User clicked an inactive project to wake it. No `key` - the
    /// session doesn't exist yet; workspace synthesizes a key and
    /// emits `SessionUpdate::Spawning` then `::Connected` with the
    /// real key. This is an App-level Command, not per-session.
    SpawnProject {
        project_name: String,
        launch_settings: SessionLaunchSettings,
    },
    /// User clicked a non-lead session row. Workspace spawns an
    /// agent for the specific session_id, synthesizing a key, and
    /// emits `Spawning` then `Connected` with the real key.
    SpawnSession {
        session_id: String,
        launch_settings: SessionLaunchSettings,
    },
    /// App start. Workspace spawns the default project (or the
    /// project passed on CLI argv) and emits the spawning + connected
    /// updates. TUI calls this once at startup. `is_fatal_on_failure`
    /// flags whether a pre-Connected failure should emit
    /// `SessionUpdate::FatalError` alongside the `ConnectionFailed`;
    /// startup is fatal (nothing to render), the sleeping-spawn flows
    /// are not (the user has an active session whose state must
    /// survive a spawn failure).
    StartDefault {
        project_name: Option<String>,
        launch_settings: SessionLaunchSettings,
    },
    /// Peer-coordination delivery (#114 v1). Dispatched by the
    /// `mcp__forge__peers__ask_agent` / `peers__tell_agent` tool
    /// impls via `WorkspaceFacade::deliver_peer_prompt`. Routed to
    /// `spawn::handle_deliver_peer_prompt` which: (a) resolves
    /// `target_project` to a `SessionKey`; (b) if target is running,
    /// stamps `current_inbound_hop` on target's DomainSession and
    /// dispatches a plain `Command::Prompt` carrying the wrapper
    /// prose; (c) if sleeping, buffers `wrapped` in target's
    /// `pending_peer_prompts` and dispatches `Command::SpawnProject`;
    /// (d) on `target_project` not in forge.toml, fires the dual-path
    /// `PeerAskFailed` notification back to caller.
    ///
    /// App-level command (`key()` returns `None`); workspace routes
    /// to the App-level handler in `spawn.rs`. The `caller` field is
    /// the source session's key for routing failure notifications and
    /// for in_reply_to validation lookups.
    DeliverPeerPrompt {
        caller: SessionKey,
        target_project: String,
        wrapped: WrappedPrompt,
    },
    /// Spawn a new worker session in `project_key`. Dispatched by
    /// the `workers__spawn` MCP Tool impl after caller-tag validation,
    /// or by the engineering-team Connected hook when reviving
    /// across-restart workers.
    ///
    /// `resume_existing`: when `Some(session_id)`, the handler resumes
    /// the named session via `SessionTarget::Session` instead of
    /// spawning fresh. The session_id MUST already carry the
    /// `forge:worker:<label>` tag (the team Connected hook verifies
    /// this before dispatching). `WorkerEntry::needs_tag` is set false
    /// on the resume path since the tag is already on disk. `None`
    /// preserves the original fresh-spawn path used by `workers__spawn`.
    ///
    /// `return_to` carries the spawn result back to the calling tool
    /// invocation; `Ok((session_id, tag))` on success, `Err(message)`
    /// on failure (e.g. tag-write failed; see spawn handler). On the
    /// resume path the team Connected hook ignores the reply (the
    /// worker is for the lead's benefit, not in response to a tool
    /// call).
    SpawnWorker {
        project_key: crate::ProjectKey,
        label: String,
        charter: String,
        spawned_by_session_id: String,
        resume_existing: Option<String>,
        /// Inline first-message for `workers__spawn(kick=...)`. Delivered
        /// as the worker's first user turn on Connected via the same
        /// kick dispatcher the file-driven `kick.md` uses. `None` -> no
        /// auto-kick (the worker idles until told).
        kick: Option<String>,
        return_to: oneshot::Sender<Result<WorkerSpawnReply, String>>,
    },
    /// Close (terminate agent + remove from `live_workers`) the
    /// worker identified by `label` in `project_key`. Dispatched by
    /// the TUI's per-row close click. If duplicates exist, the latest-
    /// spawned matching entry is removed.
    CloseWorker {
        project_key: crate::ProjectKey,
        label: String,
    },
    /// Despawn the worker identified by `label` in `project_key`:
    /// terminate its agent, drop it from `live_workers`, expire its
    /// inflight asks, AND clean up its git worktree. Dispatched by the
    /// `workers__despawn` MCP tool (lead-only). Unlike `CloseWorker`
    /// (the TUI X-button), this also removes the worker's git worktree:
    /// a clean worktree is removed; a dirty one (uncommitted/untracked
    /// or unpushed commits) blocks the despawn unless `force`. The
    /// outcome flows back via `respond`.
    DespawnWorker {
        project_key: crate::ProjectKey,
        label: String,
        force: bool,
        respond: oneshot::Sender<DespawnResult>,
    },
    /// Deliver a wrapped peer-style prompt to a worker. Same envelope
    /// as `DeliverPeerPrompt` but addressed by worker label within
    /// the caller's project rather than by cross-project name.
    DeliverWorkerPrompt {
        caller: SessionKey,
        project_key: crate::ProjectKey,
        target_label: String,
        wrapped: WrappedPrompt,
    },
    /// Deliver a wrapped peer-style prompt from a worker back to its
    /// lead. Dispatched by the `workers__tell` / `workers__ask` Tool
    /// impls when the caller addresses `label="lead"`. The target
    /// `SessionKey` is resolved at Tool dispatch time from the
    /// worker's `spawned_by_session_id` so the handler can deliver
    /// directly without re-doing the lookup against a possibly-mutated
    /// `live_workers` map. Wire shape is identical to
    /// `DeliverWorkerPrompt` (same PeerEnvelopeAppended echo + same
    /// `Command::Prompt` dispatch into the target session).
    DeliverWorkerPromptToLead {
        caller: SessionKey,
        target_lead_key: SessionKey,
        wrapped: WrappedPrompt,
    },
    /// Deliver a matched Gotify notification into `project` as a plain
    /// user turn (spawning the project if asleep, exactly like a cron
    /// prompt). `team_role` targets a durable team worker when `Some`;
    /// `None` targets the project lead. Dispatched by
    /// `Workspace::route_gotify_message`, one per matching subscription;
    /// handled by `spawn::deliver_gotify_message`. `notification` carries
    /// the resolved app name, title, message, and priority - its
    /// `to_prose()` is the user-turn text, and the same struct drives the
    /// chat-echo. App-level command (`key()` returns `None`).
    DeliverGotifyMessage {
        project: String,
        team_role: Option<String>,
        notification: crate::mcp::gotify::types::GotifyNotification,
    },
}

impl Command {
    /// The `SessionKey` this command routes to, or `None` for
    /// App-level commands (`SpawnProject`, `SpawnSession`,
    /// `StartDefault`). `Workspace::dispatch` routes `None` commands
    /// to its app-level handler (which synthesizes the new session
    /// key and spawns the agent); `Some(key)` commands route to the
    /// matching SessionTask.
    pub fn key(&self) -> Option<&SessionKey> {
        match self {
            Self::Prompt { key, .. }
            | Self::Cancel { key }
            | Self::SetMode { key, .. }
            | Self::SetModel { key, .. }
            | Self::NewSession { key, .. }
            | Self::ResumeSession { key, .. }
            | Self::RespondPermission { key, .. }
            | Self::RespondQuestion { key, .. }
            | Self::ReconnectMcpServer { key, .. }
            | Self::ToggleMcpServer { key, .. } => Some(key),
            Self::SpawnProject { .. }
            | Self::SpawnSession { .. }
            | Self::StartDefault { .. }
            | Self::DeliverPeerPrompt { .. }
            | Self::SpawnWorker { .. }
            | Self::CloseWorker { .. }
            | Self::DespawnWorker { .. }
            | Self::DeliverWorkerPrompt { .. }
            | Self::DeliverWorkerPromptToLead { .. }
            | Self::DeliverGotifyMessage { .. } => None,
        }
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Custom Debug: discriminant + routing key (and a few cheap
        // identifying fields) only. `oneshot::Sender` on `SpawnWorker`
        // isn't Debug, so deriving isn't an option; payloads can be
        // bulky and aren't useful in trace output.
        match self {
            Self::Prompt { key, .. } => {
                f.debug_struct("Prompt").field("key", key).finish_non_exhaustive()
            }
            Self::Cancel { key } => f.debug_struct("Cancel").field("key", key).finish(),
            Self::SetMode { key, mode } => {
                f.debug_struct("SetMode").field("key", key).field("mode", mode).finish()
            }
            Self::SetModel { key, model } => {
                f.debug_struct("SetModel").field("key", key).field("model", model).finish()
            }
            Self::NewSession { key, cwd, .. } => f
                .debug_struct("NewSession")
                .field("key", key)
                .field("cwd", cwd)
                .finish_non_exhaustive(),
            Self::ResumeSession { key, session_id, cwd, .. } => f
                .debug_struct("ResumeSession")
                .field("key", key)
                .field("session_id", session_id)
                .field("cwd", cwd)
                .finish_non_exhaustive(),
            Self::RespondPermission { key, tool_id, .. } => f
                .debug_struct("RespondPermission")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish_non_exhaustive(),
            Self::RespondQuestion { key, tool_id, .. } => f
                .debug_struct("RespondQuestion")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish_non_exhaustive(),
            Self::ReconnectMcpServer { key, server_name } => f
                .debug_struct("ReconnectMcpServer")
                .field("key", key)
                .field("server_name", server_name)
                .finish(),
            Self::ToggleMcpServer { key, server_name, enabled } => f
                .debug_struct("ToggleMcpServer")
                .field("key", key)
                .field("server_name", server_name)
                .field("enabled", enabled)
                .finish(),
            Self::SpawnProject { project_name, .. } => f
                .debug_struct("SpawnProject")
                .field("project_name", project_name)
                .finish_non_exhaustive(),
            Self::SpawnSession { session_id, .. } => f
                .debug_struct("SpawnSession")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::StartDefault { project_name, .. } => f
                .debug_struct("StartDefault")
                .field("project_name", project_name)
                .finish_non_exhaustive(),
            Self::DeliverPeerPrompt { caller, target_project, .. } => f
                .debug_struct("DeliverPeerPrompt")
                .field("caller", caller)
                .field("target_project", target_project)
                .finish_non_exhaustive(),
            Self::SpawnWorker { project_key, label, spawned_by_session_id, .. } => f
                .debug_struct("SpawnWorker")
                .field("project_key", project_key)
                .field("label", label)
                .field("spawned_by_session_id", spawned_by_session_id)
                .field("return_to", &"<oneshot::Sender>")
                .finish_non_exhaustive(),
            Self::CloseWorker { project_key, label } => f
                .debug_struct("CloseWorker")
                .field("project_key", project_key)
                .field("label", label)
                .finish(),
            Self::DespawnWorker { project_key, label, force, .. } => f
                .debug_struct("DespawnWorker")
                .field("project_key", project_key)
                .field("label", label)
                .field("force", force)
                .finish_non_exhaustive(),
            Self::DeliverWorkerPrompt { caller, project_key, target_label, .. } => f
                .debug_struct("DeliverWorkerPrompt")
                .field("caller", caller)
                .field("project_key", project_key)
                .field("target_label", target_label)
                .finish_non_exhaustive(),
            Self::DeliverWorkerPromptToLead { caller, target_lead_key, .. } => f
                .debug_struct("DeliverWorkerPromptToLead")
                .field("caller", caller)
                .field("target_lead_key", target_lead_key)
                .finish_non_exhaustive(),
            Self::DeliverGotifyMessage { project, team_role, notification } => f
                .debug_struct("DeliverGotifyMessage")
                .field("project", project)
                .field("team_role", team_role)
                .field("app", &notification.app)
                .field("priority", &notification.priority)
                .finish_non_exhaustive(),
        }
    }
}

/// Update envelope: forge-workspace -> forge-tui.
///
/// Permission/Question variants do NOT carry response oneshots -
/// responses flow back via `Command::Respond*`. The workspace stores
/// the oneshot in `DomainSession.pending_interactions` when emitting
/// these variants.
pub enum SessionUpdate {
    /// Workspace has synthesized a spawning state for a project /
    /// session wake (in response to `Command::SpawnProject` /
    /// `Command::SpawnSession` / `Command::StartDefault`). TUI
    /// creates a placeholder UiSession under `key` and shows the
    /// "Waking {display_name}…" message. The real `Connected` /
    /// `KeyRenamed` updates land soon after.
    ///
    /// `key` is a synthetic key the workspace generates (e.g.,
    /// `__spawn_<project>__` or `__resume_<session_id>__`). When the
    /// agent emits its first `system/init` with the real session
    /// UUID, workspace migrates internally and emits
    /// `KeyRenamed { from: synth, to: real }`.
    Spawning {
        key: SessionKey,
        project_name: String,
        cwd: String,
        display_name: String,
    },
    /// Workspace migrated `from` (synthetic spawn key) to `to` (the
    /// real claude session UUID). TUI re-keys its UiSession map:
    /// `ui_sessions.remove(&from)` → `ui_sessions.insert(to, bucket)`.
    /// If `active_session_key == Some(from)`, TUI updates it to
    /// `Some(to)` so render keeps following.
    KeyRenamed {
        from: SessionKey,
        to: SessionKey,
    },
    Connected {
        key: SessionKey,
        session_id: SessionId,
        cwd: String,
        current_model: CurrentModel,
        available_models: Vec<AvailableModel>,
        mode: Option<ModeState>,
        history: Vec<Message>,
    },
    SessionReplaced {
        key: SessionKey,
        session_id: SessionId,
        cwd: String,
        current_model: CurrentModel,
        available_models: Vec<AvailableModel>,
        mode: Option<ModeState>,
        history: Vec<Message>,
    },
    ConnectionFailed {
        key: SessionKey,
        message: String,
        fatal: bool,
    },
    AuthRequired {
        key: SessionKey,
        method_name: String,
        method_description: String,
    },
    SlashCommandError {
        key: SessionKey,
        message: String,
    },
    RuntimeReloadCompleted {
        session_id: String,
    },
    RuntimeReloadFailed {
        session_id: String,
        message: String,
    },
    /// Permission prompt. No response_tx - TUI replies via
    /// `Command::RespondPermission { tool_id, outcome }`.
    PermissionRequest {
        key: SessionKey,
        tool_id: String,
        request: PermissionRequest,
    },
    /// AskUserQuestion prompt. Same shape; reply via
    /// `Command::RespondQuestion { tool_id, outcome }`.
    QuestionRequest {
        key: SessionKey,
        tool_id: String,
        request: QuestionRequest,
    },
    McpOperationError {
        key: SessionKey,
        error: McpOperationError,
    },
    TurnComplete {
        key: SessionKey,
        terminal_reason: Option<TerminalReason>,
    },
    TurnCancelled {
        key: SessionKey,
    },
    TurnError {
        key: SessionKey,
        message: String,
        class: Option<TurnErrorClass>,
        terminal_reason: Option<TerminalReason>,
    },
    ChatAppended {
        session_id: String,
        msg: Message,
    },
    HookObservation {
        session_id: String,
        tool_use_id: Option<String>,
        permission_mode: Option<String>,
        effort: Option<String>,
        agent_id: Option<String>,
        agent_type: Option<String>,
    },
    StatusSnapshot {
        session_id: String,
        account: AccountInfo,
        forge_account: Option<ForgeAccountIdentity>,
    },
    ForgeAccountIdentity {
        key: SessionKey,
        display_name: String,
    },
    OauthCredentialsSnapshot {
        session_id: String,
        credentials: Option<OauthCredentials>,
    },
    ContextUsageSnapshot {
        session_id: String,
        percentage: Option<u8>,
        /// Raw model context-window size in tokens (e.g. `1_000_000`
        /// for Opus 1M). `None` until the upstream probe reports it.
        max_tokens: Option<u64>,
    },
    McpSnapshot {
        session_id: String,
        servers: Vec<McpServerStatus>,
        error: Option<String>,
    },
    SessionsListed {
        /// Bucket this session list belongs to. The catalog scan that
        /// produces `sessions` runs against the spawning session's
        /// `cwd`, so the listing is project-scoped - routing onto
        /// the requesting bucket prevents another session's `/resume`
        /// autocomplete from inheriting a stale project's list.
        key: SessionKey,
        sessions: Vec<SessionListEntry>,
    },
    ServiceStatus {
        severity: ServiceSeverity,
        message: String,
    },
    PluginsInventoryUpdated {
        cwd_raw: String,
        snapshot: PluginsInventorySnapshot,
        claude_path: PathBuf,
    },
    PluginsInventoryRefreshFailed {
        cwd_raw: String,
        message: String,
    },
    PluginsCliActionSucceeded {
        cwd_raw: String,
        result: PluginsCliActionSuccess,
    },
    PluginsCliActionFailed {
        cwd_raw: String,
        message: String,
    },
    /// Peer-coordination ask in-flight stats changed for `key`. Fired
    /// whenever `bump_inflight_stats` mutates the session's
    /// `PeerInflightStats` (ask sent, reply received, timeout, delivery
    /// failure). TUI reducer arm updates the sidebar peer-activity
    /// badge in the Projects pane.
    PeerInflightStatsChanged {
        key: SessionKey,
        stats: PeerInflightStats,
    },
    /// Workspace pushed a change to `live_workers[project_key]`. The
    /// TUI reducer updates the projects pane's tree-children based on
    /// `action`. `status` is the snapshot at the moment of the change
    /// (relevant for Added and StatusChanged; ignored for Removed but
    /// carried for symmetry). `is_git_repo_at_spawn` is the worker's
    /// cached "was the project a git repo at spawn time" flag - the
    /// TUI's close-toast formatter reads it on `Removed` events
    /// (after which the `WorkerEntry` is gone from `live_workers`,
    /// so a lookup-by-label would fail).
    WorkerStatusChanged {
        project_key: crate::ProjectKey,
        action: WorkerStatusAction,
        status: forge_primitives::WorkerStatus,
        is_git_repo_at_spawn: bool,
    },
    /// A peer-coordination envelope arrived at session `session_id`.
    /// Carries the typed `WrappedPrompt` so the TUI reducer can build
    /// the chat-side echo from real fields instead of having the
    /// workspace forge a `Message::User` carrying prose for the TUI to
    /// re-parse (audit I11). The recipient's LLM still receives the
    /// prose via a separate `Command::Prompt` dispatch - that's the
    /// CLI's input channel and stays text-shaped.
    PeerEnvelopeAppended {
        session_id: String,
        wrapped: crate::mcp::peers::types::WrappedPrompt,
    },
    /// A matched Gotify notification arrived at session `session_id`.
    /// Carries the typed `GotifyNotification` so the TUI reducer builds
    /// the chat-side echo from real fields (mirrors PeerEnvelopeAppended).
    /// The session's LLM receives the same prose via a separate
    /// `Command::Prompt` - this update only drives the visible echo.
    GotifyNotificationAppended {
        session_id: String,
        notification: crate::mcp::gotify::types::GotifyNotification,
    },
    /// A due cron fired into session `session_id`. Carries the fired
    /// prompt text so the TUI reducer builds the chat-side echo (mirrors
    /// GotifyNotificationAppended). The session's LLM receives the same
    /// text via a separate `Command::Prompt` - this update only drives
    /// the visible echo.
    CronPromptAppended {
        session_id: String,
        text: String,
    },
    FatalError(AppError),
}

impl SessionUpdate {
    /// The [`SessionKey`] this update routes to, or `None` for
    /// updates that target App-level state (`SessionsListed`,
    /// `ServiceStatus`, usage, plugin, key-rename, fatal-error).
    /// Variants carrying a raw `session_id` synthesize a key from it.
    pub fn session_key(&self) -> Option<SessionKey> {
        match self {
            Self::Spawning { key, .. }
            | Self::Connected { key, .. }
            | Self::SessionReplaced { key, .. }
            | Self::ConnectionFailed { key, .. }
            | Self::AuthRequired { key, .. }
            | Self::SlashCommandError { key, .. }
            | Self::PermissionRequest { key, .. }
            | Self::QuestionRequest { key, .. }
            | Self::McpOperationError { key, .. }
            | Self::TurnComplete { key, .. }
            | Self::TurnCancelled { key }
            | Self::TurnError { key, .. }
            | Self::ForgeAccountIdentity { key, .. }
            | Self::SessionsListed { key, .. }
            | Self::PeerInflightStatsChanged { key, .. } => Some(key.clone()),
            Self::RuntimeReloadCompleted { session_id }
            | Self::RuntimeReloadFailed { session_id, .. }
            | Self::ChatAppended { session_id, .. }
            | Self::HookObservation { session_id, .. }
            | Self::StatusSnapshot { session_id, .. }
            | Self::OauthCredentialsSnapshot { session_id, .. }
            | Self::ContextUsageSnapshot { session_id, .. }
            | Self::McpSnapshot { session_id, .. }
            | Self::PeerEnvelopeAppended { session_id, .. }
            | Self::GotifyNotificationAppended { session_id, .. }
            | Self::CronPromptAppended { session_id, .. } => {
                Some(SessionKey::from_session_id(session_id.clone()))
            }
            Self::KeyRenamed { .. }
            | Self::ServiceStatus { .. }
            | Self::PluginsInventoryUpdated { .. }
            | Self::PluginsInventoryRefreshFailed { .. }
            | Self::PluginsCliActionSucceeded { .. }
            | Self::PluginsCliActionFailed { .. }
            | Self::WorkerStatusChanged { .. }
            | Self::FatalError(..) => None,
        }
    }
}

impl std::fmt::Debug for SessionUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Custom Debug - discriminant + key/session_id only.
        // Payloads can be large or non-Debug-friendly; the
        // discriminant + routing key is sufficient for trace logs.
        match self {
            Self::Spawning { key, project_name, .. } => f
                .debug_struct("Spawning")
                .field("key", key)
                .field("project_name", project_name)
                .finish_non_exhaustive(),
            Self::KeyRenamed { from, to } => {
                f.debug_struct("KeyRenamed").field("from", from).field("to", to).finish()
            }
            Self::Connected { key, .. } => {
                f.debug_struct("Connected").field("key", key).finish_non_exhaustive()
            }
            Self::SessionReplaced { key, .. } => {
                f.debug_struct("SessionReplaced").field("key", key).finish_non_exhaustive()
            }
            Self::ConnectionFailed { key, .. } => {
                f.debug_struct("ConnectionFailed").field("key", key).finish_non_exhaustive()
            }
            Self::AuthRequired { key, .. } => {
                f.debug_struct("AuthRequired").field("key", key).finish_non_exhaustive()
            }
            Self::SlashCommandError { key, .. } => {
                f.debug_struct("SlashCommandError").field("key", key).finish_non_exhaustive()
            }
            Self::RuntimeReloadCompleted { session_id } => {
                f.debug_struct("RuntimeReloadCompleted").field("session_id", session_id).finish()
            }
            Self::RuntimeReloadFailed { session_id, .. } => f
                .debug_struct("RuntimeReloadFailed")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::PermissionRequest { key, tool_id, .. } => f
                .debug_struct("PermissionRequest")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish_non_exhaustive(),
            Self::QuestionRequest { key, tool_id, .. } => f
                .debug_struct("QuestionRequest")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish_non_exhaustive(),
            Self::McpOperationError { key, .. } => {
                f.debug_struct("McpOperationError").field("key", key).finish_non_exhaustive()
            }
            Self::TurnComplete { key, .. } => {
                f.debug_struct("TurnComplete").field("key", key).finish_non_exhaustive()
            }
            Self::TurnCancelled { key } => {
                f.debug_struct("TurnCancelled").field("key", key).finish()
            }
            Self::TurnError { key, .. } => {
                f.debug_struct("TurnError").field("key", key).finish_non_exhaustive()
            }
            Self::ChatAppended { session_id, .. } => f
                .debug_struct("ChatAppended")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::HookObservation { session_id, .. } => f
                .debug_struct("HookObservation")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::StatusSnapshot { session_id, .. } => f
                .debug_struct("StatusSnapshot")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::ForgeAccountIdentity { key, .. } => {
                f.debug_struct("ForgeAccountIdentity").field("key", key).finish_non_exhaustive()
            }
            Self::OauthCredentialsSnapshot { session_id, .. } => f
                .debug_struct("OauthCredentialsSnapshot")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::ContextUsageSnapshot { session_id, .. } => f
                .debug_struct("ContextUsageSnapshot")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::McpSnapshot { session_id, .. } => f
                .debug_struct("McpSnapshot")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::SessionsListed { key, sessions } => f
                .debug_struct("SessionsListed")
                .field("key", key)
                .field("count", &sessions.len())
                .finish(),
            Self::ServiceStatus { .. } => f.debug_struct("ServiceStatus").finish_non_exhaustive(),
            Self::PluginsInventoryUpdated { cwd_raw, .. } => f
                .debug_struct("PluginsInventoryUpdated")
                .field("cwd_raw", cwd_raw)
                .finish_non_exhaustive(),
            Self::PluginsInventoryRefreshFailed { cwd_raw, .. } => f
                .debug_struct("PluginsInventoryRefreshFailed")
                .field("cwd_raw", cwd_raw)
                .finish_non_exhaustive(),
            Self::PluginsCliActionSucceeded { cwd_raw, .. } => f
                .debug_struct("PluginsCliActionSucceeded")
                .field("cwd_raw", cwd_raw)
                .finish_non_exhaustive(),
            Self::PluginsCliActionFailed { cwd_raw, .. } => f
                .debug_struct("PluginsCliActionFailed")
                .field("cwd_raw", cwd_raw)
                .finish_non_exhaustive(),
            Self::PeerInflightStatsChanged { key, stats } => f
                .debug_struct("PeerInflightStatsChanged")
                .field("key", key)
                .field("stats", stats)
                .finish(),
            Self::WorkerStatusChanged { project_key, action, status, is_git_repo_at_spawn } => f
                .debug_struct("WorkerStatusChanged")
                .field("project_key", project_key)
                .field("action", action)
                .field("label", &status.label)
                .field("is_git_repo_at_spawn", is_git_repo_at_spawn)
                .finish_non_exhaustive(),
            Self::PeerEnvelopeAppended { session_id, wrapped } => f
                .debug_struct("PeerEnvelopeAppended")
                .field("session_id", session_id)
                .field("correlation_id", &wrapped.correlation_id)
                .field("kind", &wrapped.kind)
                .finish_non_exhaustive(),
            Self::GotifyNotificationAppended { session_id, notification } => f
                .debug_struct("GotifyNotificationAppended")
                .field("session_id", session_id)
                .field("app", &notification.app)
                .field("priority", &notification.priority)
                .finish_non_exhaustive(),
            Self::CronPromptAppended { session_id, text } => f
                .debug_struct("CronPromptAppended")
                .field("session_id", session_id)
                .field("text", text)
                .finish(),
            Self::FatalError(err) => f.debug_struct("FatalError").field("error", err).finish(),
        }
    }
}

/// Errors from `Workspace::dispatch`.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("no session task registered for key {0:?}")]
    UnknownSession(SessionKey),
    #[error("session task for key {0:?} has closed its command channel")]
    SessionClosed(SessionKey),
}

#[cfg(test)]
mod workers_command_tests {
    use super::*;
    use forge_primitives::{WorkerLiveness, WorkerStatus};
    use std::time::SystemTime;

    #[test]
    fn worker_status_action_added_constructs() {
        let action = WorkerStatusAction::Added;
        assert!(format!("{action:?}").contains("Added"));
    }

    #[test]
    fn worker_spawn_reply_constructs() {
        let r = WorkerSpawnReply { session_id: "abc".into(), tag: "forge:worker:reviewer".into() };
        assert_eq!(r.tag, "forge:worker:reviewer");
    }

    #[test]
    fn worker_status_session_update_variant_compiles() {
        let _u = SessionUpdate::WorkerStatusChanged {
            project_key: crate::ProjectKey::new("test"),
            action: WorkerStatusAction::Added,
            status: WorkerStatus {
                label: "reviewer".into(),
                charter: "you are a reviewer".into(),
                status: WorkerLiveness::Running,
                session_id: "abc".into(),
                spawned_at: SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead-uuid".into(),
                diagnostic: None,
            },
            is_git_repo_at_spawn: true,
        };
    }
}
