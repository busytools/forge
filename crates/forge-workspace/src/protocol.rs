//! Command + SessionUpdate channel envelopes between forge-tui and
//! forge-workspace. TUI dispatches Commands; workspace per-session
//! tasks emit SessionUpdates back via a fan-in channel.
//!
//! Wire shapes are FINAL as of Phase 1 of the MVVM refactor (#102).
//! Phases 3a-d migrate emitters/consumers but the variant shapes
//! themselves don't change.

use std::collections::BTreeMap;
use std::path::PathBuf;

use forge_agent::client::SessionLaunchSettings;
use forge_primitives::cloud::oauth_credentials::OauthCredentials;
use forge_primitives::cloud::service_status::ServiceSeverity;
use forge_primitives::error::AppError;
use forge_primitives::permission::PermissionMode;
use forge_primitives::permission_ui::{PermissionOutcome, PermissionRequest};
use forge_primitives::permissions::PermissionUpdate;
use forge_primitives::plugins::{PluginsCliActionSuccess, PluginsInventorySnapshot};
use forge_primitives::question::{QuestionOutcome, QuestionRequest};
use forge_primitives::runtime::{AvailableModel, CurrentModel, ModeState, TerminalReason};
use forge_primitives::usage::{UsageSnapshot, UsageSourceKind};
use forge_primitives::{
    AccountInfo, ElicitationAction, ElicitationRequest, ForgeAccountIdentity, ImageAttachment,
    McpAuthRedirect, McpOperationError, McpServerConfig, McpServerStatus, Message, SessionId,
    SessionListEntry,
};
use tokio::sync::oneshot;

use crate::SessionKey;

/// Turn-error classification. The full surface (including the
/// `Internal` / `Other` matrix) lives in
/// `forge_agent::translate::error_handling`; this slimmer enum is
/// re-stated here so the protocol module doesn't fan an
/// implementation-detail crate dependency through every consumer of
/// `SessionUpdate`. The two enums agree on variant names; the
/// [`From`] impl below maps between them so future variant divergence
/// becomes a compile error rather than a silent drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnErrorClass {
    PlanLimit,
    AuthRequired,
    Internal,
    Other,
}

impl From<forge_agent::translate::error_handling::TurnErrorClass> for TurnErrorClass {
    fn from(value: forge_agent::translate::error_handling::TurnErrorClass) -> Self {
        use forge_agent::translate::error_handling::TurnErrorClass as Src;
        match value {
            Src::PlanLimit => Self::PlanLimit,
            Src::AuthRequired => Self::AuthRequired,
            Src::Internal => Self::Internal,
            Src::Other => Self::Other,
        }
    }
}

/// One pending interaction response slot. Workspace stores these
/// keyed by `tool_id` in `DomainSession.pending_interactions`.
/// `Command::RespondPermission` / `RespondQuestion` /
/// `RespondElicitation` look up the matching slot and send the
/// outcome down the oneshot.
pub enum PendingInteractionSlot {
    Permission(oneshot::Sender<PermissionOutcome>),
    Question(oneshot::Sender<QuestionOutcome>),
    Elicitation(oneshot::Sender<ElicitationAction>),
}

impl std::fmt::Debug for PendingInteractionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permission(_) => f.write_str("PendingInteractionSlot::Permission"),
            Self::Question(_) => f.write_str("PendingInteractionSlot::Question"),
            Self::Elicitation(_) => f.write_str("PendingInteractionSlot::Elicitation"),
        }
    }
}

/// Command envelope: forge-tui -> forge-workspace.
///
/// Every variant carries a `SessionKey` identifying the target
/// session task. `Workspace::dispatch` fans the variant into the
/// matching task's command receiver. Phase 1 implements
/// `Respond*` end-to-end; other variants log + drop until Phase 2
/// wires them to `AgentHandle` methods.
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
    ResumeOrNewSession {
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
    /// MCP elicitation response. Currently routed directly to
    /// `AgentHandle::respond_to_elicitation` — the workspace never
    /// stores a `PendingInteractionSlot::Elicitation` slot for
    /// inbound `ElicitationRequest`s, so the oneshot path used by
    /// permission/question round-trips doesn't apply here.
    RespondElicitation {
        key: SessionKey,
        elicitation_id: String,
        action: ElicitationAction,
        content: Option<serde_json::Value>,
    },
    /// Request a fresh session title from the bridge.
    GenerateSessionTitle {
        key: SessionKey,
        description: String,
    },
    /// Persist a custom title for the active session.
    RenameSession {
        key: SessionKey,
        title: String,
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
    /// Replace the live MCP server registration for this session.
    SetMcpServers {
        key: SessionKey,
        servers: BTreeMap<String, McpServerConfig>,
    },
    /// Begin OAuth (or similar) auth flow for an MCP server.
    AuthenticateMcpServer {
        key: SessionKey,
        server_name: String,
    },
    /// Wipe cached auth for an MCP server.
    ClearMcpAuth {
        key: SessionKey,
        server_name: String,
    },
    /// Submit a captured OAuth callback URL for an MCP server.
    SubmitMcpOauthCallbackUrl {
        key: SessionKey,
        server_name: String,
        callback_url: String,
    },
    CloseSession {
        key: SessionKey,
    },
    RequestStatusSnapshot {
        key: SessionKey,
        session_id: String,
    },
    RequestMcpSnapshot {
        key: SessionKey,
        session_id: String,
    },
    RequestContextUsage {
        key: SessionKey,
        session_id: String,
    },
    RequestOauthCredentials {
        key: SessionKey,
        session_id: String,
    },
    RuntimeReload {
        key: SessionKey,
        session_id: String,
    },
    UpdatePermissions {
        key: SessionKey,
        session_id: String,
        update: PermissionUpdate,
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
            | Self::ResumeOrNewSession { key, .. }
            | Self::RespondPermission { key, .. }
            | Self::RespondQuestion { key, .. }
            | Self::RespondElicitation { key, .. }
            | Self::GenerateSessionTitle { key, .. }
            | Self::RenameSession { key, .. }
            | Self::ReconnectMcpServer { key, .. }
            | Self::ToggleMcpServer { key, .. }
            | Self::SetMcpServers { key, .. }
            | Self::AuthenticateMcpServer { key, .. }
            | Self::ClearMcpAuth { key, .. }
            | Self::SubmitMcpOauthCallbackUrl { key, .. }
            | Self::CloseSession { key }
            | Self::RequestStatusSnapshot { key, .. }
            | Self::RequestMcpSnapshot { key, .. }
            | Self::RequestContextUsage { key, .. }
            | Self::RequestOauthCredentials { key, .. }
            | Self::RuntimeReload { key, .. }
            | Self::UpdatePermissions { key, .. } => Some(key),
            Self::SpawnProject { .. } | Self::SpawnSession { .. } | Self::StartDefault { .. } => {
                None
            }
        }
    }
}

/// Update envelope: forge-workspace -> forge-tui.
///
/// FINAL variant shapes as of Phase 1. Permission/Question/Elicitation
/// variants do NOT carry response oneshots — responses flow back via
/// `Command::Respond*`. The workspace stores the oneshot in
/// `DomainSession.pending_interactions` when emitting these variants.
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
    AuthCompleted {
        key: SessionKey,
    },
    LogoutCompleted {
        key: SessionKey,
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
    /// MCP elicitation. Reply via
    /// `Command::RespondElicitation { elicitation_id, action }`.
    McpElicitationRequest {
        key: SessionKey,
        elicitation_id: String,
        request: ElicitationRequest,
    },
    McpElicitationCompleted {
        key: SessionKey,
        elicitation_id: String,
        server_name: Option<String>,
    },
    McpAuthRedirect {
        key: SessionKey,
        redirect: McpAuthRedirect,
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
    UsageRefreshStarted {
        /// Bucket the in-flight fetch belongs to. Used by the TUI
        /// reducer to route lifecycle flags onto the right
        /// `UiSession.usage` slot even if the user switched sessions
        /// mid-fetch. Dropped silently when the bucket no longer
        /// exists (rare; session closed before the fetch landed).
        key: SessionKey,
    },
    UsageSnapshotReceived {
        key: SessionKey,
        snapshot: UsageSnapshot,
    },
    UsageRefreshFailed {
        key: SessionKey,
        message: String,
        source: UsageSourceKind,
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
            | Self::AuthCompleted { key, .. }
            | Self::LogoutCompleted { key }
            | Self::SlashCommandError { key, .. }
            | Self::PermissionRequest { key, .. }
            | Self::QuestionRequest { key, .. }
            | Self::McpElicitationRequest { key, .. }
            | Self::McpElicitationCompleted { key, .. }
            | Self::McpAuthRedirect { key, .. }
            | Self::McpOperationError { key, .. }
            | Self::TurnComplete { key, .. }
            | Self::TurnCancelled { key }
            | Self::TurnError { key, .. }
            | Self::ForgeAccountIdentity { key, .. }
            | Self::UsageRefreshStarted { key, .. }
            | Self::UsageSnapshotReceived { key, .. }
            | Self::UsageRefreshFailed { key, .. }
            | Self::SessionsListed { key, .. } => Some(key.clone()),
            Self::RuntimeReloadCompleted { session_id }
            | Self::RuntimeReloadFailed { session_id, .. }
            | Self::ChatAppended { session_id, .. }
            | Self::HookObservation { session_id, .. }
            | Self::StatusSnapshot { session_id, .. }
            | Self::OauthCredentialsSnapshot { session_id, .. }
            | Self::ContextUsageSnapshot { session_id, .. }
            | Self::McpSnapshot { session_id, .. } => {
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
            Self::AuthCompleted { key, .. } => {
                f.debug_struct("AuthCompleted").field("key", key).finish_non_exhaustive()
            }
            Self::LogoutCompleted { key } => {
                f.debug_struct("LogoutCompleted").field("key", key).finish()
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
            Self::McpElicitationRequest { key, elicitation_id, .. } => f
                .debug_struct("McpElicitationRequest")
                .field("key", key)
                .field("elicitation_id", elicitation_id)
                .finish_non_exhaustive(),
            Self::McpElicitationCompleted { key, elicitation_id, .. } => f
                .debug_struct("McpElicitationCompleted")
                .field("key", key)
                .field("elicitation_id", elicitation_id)
                .finish_non_exhaustive(),
            Self::McpAuthRedirect { key, .. } => {
                f.debug_struct("McpAuthRedirect").field("key", key).finish_non_exhaustive()
            }
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
            Self::UsageRefreshStarted { key } => {
                f.debug_struct("UsageRefreshStarted").field("key", key).finish()
            }
            Self::UsageSnapshotReceived { key, .. } => {
                f.debug_struct("UsageSnapshotReceived").field("key", key).finish_non_exhaustive()
            }
            Self::UsageRefreshFailed { key, .. } => {
                f.debug_struct("UsageRefreshFailed").field("key", key).finish_non_exhaustive()
            }
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
