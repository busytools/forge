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

/// Command envelope: forge-tui -> forge-workspace.
///
/// Every variant carries a `SessionKey` identifying the target
/// session task. `Workspace::dispatch` fans the variant into the
/// matching task's command receiver.
#[derive(Debug)]
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
    /// User clicked an inactive project to wake it. No `key` — the
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
            | Self::DeliverPeerPrompt { .. } => None,
        }
    }
}

/// Update envelope: forge-workspace -> forge-tui.
///
/// Permission/Question variants do NOT carry response oneshots —
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
    /// Permission prompt. No response_tx — TUI replies via
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
        /// `cwd`, so the listing is project-scoped — routing onto
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
    /// A peer-coordination envelope arrived at session `session_id`.
    /// Carries the typed `WrappedPrompt` so the TUI reducer can build
    /// the chat-side echo from real fields instead of having the
    /// workspace forge a `Message::User` carrying prose for the TUI to
    /// re-parse (audit I11). The recipient's LLM still receives the
    /// prose via a separate `Command::Prompt` dispatch — that's the
    /// CLI's input channel and stays text-shaped.
    PeerEnvelopeAppended {
        session_id: String,
        wrapped: crate::mcp::peers::types::WrappedPrompt,
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
            | Self::PeerEnvelopeAppended { session_id, .. } => {
                Some(SessionKey::from_session_id(session_id.clone()))
            }
            Self::KeyRenamed { .. }
            | Self::ServiceStatus { .. }
            | Self::PluginsInventoryUpdated { .. }
            | Self::PluginsInventoryRefreshFailed { .. }
            | Self::PluginsCliActionSucceeded { .. }
            | Self::PluginsCliActionFailed { .. }
            | Self::FatalError(..) => None,
        }
    }
}

impl std::fmt::Debug for SessionUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Custom Debug — discriminant + key/session_id only.
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
            Self::PeerEnvelopeAppended { session_id, wrapped } => f
                .debug_struct("PeerEnvelopeAppended")
                .field("session_id", session_id)
                .field("correlation_id", &wrapped.correlation_id)
                .field("kind", &wrapped.kind)
                .finish_non_exhaustive(),
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
