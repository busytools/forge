use crate::agent::error_handling::TurnErrorClass;
use crate::agent::model;
use crate::app::plugins::{PluginsCliActionSuccess, PluginsInventorySnapshot};
use crate::app::{UsageSnapshot, UsageSourceKind};
use crate::error::AppError;
use forge_workspace::SessionKey;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Messages sent from the backend bridge path to the App/UI layer.
///
/// Variants that route to per-session state carry an explicit
/// `session_key` field (or a `session_id` from which the key is
/// derived at routing time). The per-session multiplexer in
/// [`crate::app::events::handle_client_event`] uses this key to
/// look up the target [`crate::app::session::Session`] bucket so
/// background-session events update silently while only active-
/// session events flip [`crate::app::App::needs_redraw`].
///
/// App-global variants that aren't bound to any single session
/// (`SessionsListed`, `Plugins*`, `Usage*`, `FatalError`,
/// `ServiceStatus`) carry no `session_key` field and update App-
/// level state directly.
pub enum ClientEvent {
    /// Permission request that needs user input.
    ///
    /// `request.session_id` carries the routing key.
    PermissionRequest {
        request: model::RequestPermissionRequest,
        response_tx: tokio::sync::oneshot::Sender<model::RequestPermissionResponse>,
    },
    /// Question request from `AskUserQuestion` that needs structured user input.
    ///
    /// `request.session_id` carries the routing key.
    QuestionRequest {
        request: model::RequestQuestionRequest,
        response_tx: tokio::sync::oneshot::Sender<model::RequestQuestionResponse>,
    },
    /// MCP elicitation request that needs auth or other MCP input.
    McpElicitationRequest { session_key: SessionKey, request: forge_primitives::ElicitationRequest },
    /// MCP elicitation completed in the SDK.
    McpElicitationCompleted {
        session_key: SessionKey,
        elicitation_id: String,
        server_name: Option<String>,
    },
    /// MCP auth redirect returned directly by the SDK auth call.
    McpAuthRedirect { session_key: SessionKey, redirect: forge_primitives::McpAuthRedirect },
    /// MCP operation failed and should be surfaced in the MCP config UI.
    McpOperationError { session_key: SessionKey, error: forge_primitives::McpOperationError },
    /// A prompt turn completed successfully.
    TurnComplete {
        session_key: SessionKey,
        terminal_reason: Option<forge_primitives::TerminalReason>,
    },
    /// `cancel` notification was accepted by the bridge.
    TurnCancelled { session_key: SessionKey },
    /// A prompt turn failed with an error.
    TurnError {
        session_key: SessionKey,
        message: String,
        terminal_reason: Option<forge_primitives::TerminalReason>,
    },
    /// A prompt turn failed with bridge-provided classification metadata.
    TurnErrorClassified {
        session_key: SessionKey,
        message: String,
        class: TurnErrorClass,
        terminal_reason: Option<forge_primitives::TerminalReason>,
    },
    /// Background connection completed successfully.
    ///
    /// `session_id` is the claude-issued session UUID; the
    /// multiplexer derives [`SessionKey`] from it at routing time.
    ///
    /// `pre_connect_key` carries the synthetic-key sentinel under
    /// which the App seeded the spawning bucket (e.g.
    /// `__spawn_<project>__` or `__conn_pending__`). The handler
    /// uses this to migrate ONLY the spawn-task's own bucket onto
    /// the real session UUID — without it, rapid clicks on
    /// different sleeping projects could cause one Connected event
    /// to migrate another spawn's bucket. `None` is tolerated for
    /// backward compatibility / tests; the handler falls back to
    /// the legacy heuristic in that case.
    Connected {
        session_id: model::SessionId,
        cwd: String,
        current_model: model::CurrentModel,
        available_models: Vec<model::AvailableModel>,
        mode: Option<crate::app::ModeState>,
        history_updates: Vec<forge_primitives::Message>,
        pre_connect_key: Option<SessionKey>,
    },
    /// Background connection failed.
    ConnectionFailed { session_key: SessionKey, message: String },
    /// Authentication is required before a session can be created.
    AuthRequired { session_key: SessionKey, method_name: String, method_description: String },
    /// Slash-command execution failed with a user-facing error.
    SlashCommandError { session_key: SessionKey, message: String },
    /// Session runtime plugin reload completed successfully.
    RuntimeReloadCompleted { session_id: String },
    /// Session runtime plugin reload failed after dispatch.
    RuntimeReloadFailed { session_id: String, message: String },
    /// Custom slash command replaced the active session.
    SessionReplaced {
        session_id: model::SessionId,
        cwd: String,
        current_model: model::CurrentModel,
        available_models: Vec<model::AvailableModel>,
        mode: Option<crate::app::ModeState>,
        history_updates: Vec<forge_primitives::Message>,
    },
    /// Recent sessions discovered via SDK session listing.
    SessionsListed { sessions: Vec<forge_primitives::SessionListEntry> },
    /// Startup Claude Code status check detected degraded/outage conditions.
    ServiceStatus { severity: ServiceStatusSeverity, message: String },
    /// /login completed via `claude auth login` -- credentials stored, ready to start a session.
    AuthCompleted { session_key: SessionKey, conn: Arc<forge_agent::AgentHandle> },
    /// /logout completed via `claude auth logout`.
    LogoutCompleted { session_key: SessionKey },
    /// Status snapshot received from bridge (account info).
    StatusSnapshotReceived {
        session_id: String,
        account: forge_primitives::AccountInfo,
        forge_account: Option<forge_primitives::ForgeAccountIdentity>,
    },
    /// Forge-side account identity is known the moment
    /// `Workspace::get_agent_handle` returns — much earlier than
    /// the CLI-side `StatusSnapshot`. Emitted once per connection
    /// so the welcome message can render `Account: <name>` as soon
    /// as the workspace picks the account, rather than waiting for
    /// the CLI subprocess to boot.
    ForgeAccountIdentityReady { session_key: SessionKey, display_name: String },
    /// OAuth credentials snapshot received from bridge. `credentials` is
    /// `None` when no credentials file exists or it's empty/malformed.
    OauthCredentialsSnapshotReceived {
        session_id: String,
        credentials: Option<forge_agent::cloud::oauth_credentials::OauthCredentials>,
    },
    /// Git introspection snapshot pushed by the bridge whenever the
    /// repo's branch resolution changes (initial state included).
    GitContextSnapshotReceived { session_id: String, context: forge_agent::env::git::GitContext },
    /// Session context window usage received from bridge.
    ContextUsageReceived { session_id: String, percentage: Option<u8> },
    /// MCP server snapshot received from bridge.
    McpSnapshotReceived {
        session_id: String,
        servers: Vec<forge_primitives::McpServerStatus>,
        error: Option<String>,
    },
    /// Raw `forge_primitives::Message` envelope received from the
    /// bridge worker. The App's `events::sdk_message::handle_sdk_message`
    /// dispatches per-variant handlers that mutate App state directly.
    SdkMessageReceived { session_id: String, msg: forge_primitives::Message },
    /// CLI runtime state observed from a hook input as it passed
    /// through the SDK's hook-callback dispatch. Higher-fidelity than
    /// `system/status` events for mode / effort drift detection. The
    /// App prefers these values for the mode and effort chips and uses
    /// `agent_id` + `agent_type` to attribute sub-agent tool calls.
    HookObservation {
        session_id: String,
        tool_use_id: Option<String>,
        permission_mode: Option<String>,
        effort: Option<String>,
        agent_id: Option<String>,
        agent_type: Option<String>,
    },
    /// Usage refresh task started.
    UsageRefreshStarted { epoch: u64 },
    /// Usage refresh completed successfully.
    UsageSnapshotReceived { epoch: u64, snapshot: UsageSnapshot },
    /// Usage refresh failed.
    UsageRefreshFailed { epoch: u64, message: String, source: UsageSourceKind },
    /// Claude CLI plugin inventory refresh completed.
    PluginsInventoryUpdated {
        cwd_raw: String,
        snapshot: PluginsInventorySnapshot,
        claude_path: PathBuf,
    },
    /// Claude CLI plugin inventory refresh failed.
    PluginsInventoryRefreshFailed { cwd_raw: String, message: String },
    /// Plugin CLI action completed and returned a refreshed inventory snapshot.
    PluginsCliActionSucceeded { cwd_raw: String, result: PluginsCliActionSuccess },
    /// Plugin CLI action failed.
    PluginsCliActionFailed { cwd_raw: String, message: String },
    /// Fatal app error that should terminate and map to an exit code.
    FatalError(AppError),
}

impl ClientEvent {
    /// Routing key for the per-session multiplexer. Variants that
    /// carry an explicit `session_key` field return it directly;
    /// variants that carry a `session_id: String` (or
    /// `model::SessionId`) derive a [`SessionKey`] from it.
    /// App-global variants (sessions list, plugins, usage refresh,
    /// fatal error, service status) return [`None`] — they update
    /// App-level state and don't need session routing.
    #[must_use]
    pub fn session_key(&self) -> Option<SessionKey> {
        match self {
            Self::PermissionRequest { request, .. } => {
                Some(SessionKey::from_session_id(request.session_id.to_string()))
            }
            Self::QuestionRequest { request, .. } => {
                Some(SessionKey::from_session_id(request.session_id.to_string()))
            }
            Self::McpElicitationRequest { session_key, .. }
            | Self::McpElicitationCompleted { session_key, .. }
            | Self::McpAuthRedirect { session_key, .. }
            | Self::McpOperationError { session_key, .. }
            | Self::TurnComplete { session_key, .. }
            | Self::TurnCancelled { session_key }
            | Self::TurnError { session_key, .. }
            | Self::TurnErrorClassified { session_key, .. }
            | Self::ConnectionFailed { session_key, .. }
            | Self::AuthRequired { session_key, .. }
            | Self::SlashCommandError { session_key, .. }
            | Self::AuthCompleted { session_key, .. }
            | Self::LogoutCompleted { session_key }
            | Self::ForgeAccountIdentityReady { session_key, .. } => Some(session_key.clone()),
            Self::Connected { session_id, .. } | Self::SessionReplaced { session_id, .. } => {
                Some(SessionKey::from_session_id(session_id.to_string()))
            }
            Self::RuntimeReloadCompleted { session_id }
            | Self::RuntimeReloadFailed { session_id, .. }
            | Self::StatusSnapshotReceived { session_id, .. }
            | Self::OauthCredentialsSnapshotReceived { session_id, .. }
            | Self::GitContextSnapshotReceived { session_id, .. }
            | Self::ContextUsageReceived { session_id, .. }
            | Self::McpSnapshotReceived { session_id, .. }
            | Self::SdkMessageReceived { session_id, .. }
            | Self::HookObservation { session_id, .. } => {
                Some(SessionKey::from_session_id(session_id.clone()))
            }
            Self::SessionsListed { .. }
            | Self::ServiceStatus { .. }
            | Self::UsageRefreshStarted { .. }
            | Self::UsageSnapshotReceived { .. }
            | Self::UsageRefreshFailed { .. }
            | Self::PluginsInventoryUpdated { .. }
            | Self::PluginsInventoryRefreshFailed { .. }
            | Self::PluginsCliActionSucceeded { .. }
            | Self::PluginsCliActionFailed { .. }
            | Self::FatalError(..) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatusSeverity {
    Warning,
    Error,
}

/// Shared handle to all spawned terminal processes.
pub type TerminalMap = Rc<RefCell<HashMap<String, TerminalProcess>>>;

/// Minimal terminal process state used by UI snapshot rendering.
pub struct TerminalProcess {
    pub child: Option<tokio::process::Child>,
    /// Accumulated stdout+stderr - append-only, never cleared.
    pub output_buffer: Arc<Mutex<Vec<u8>>>,
    /// The shell command that was executed.
    pub command: String,
}

/// Kill all spawned terminal child processes. Call on app exit.
pub fn kill_all_terminals(terminals: &TerminalMap) {
    let mut map = terminals.borrow_mut();
    for (_, terminal) in map.iter_mut() {
        if let Some(child) = terminal.child.as_mut() {
            let _ = child.start_kill();
        }
    }
    map.clear();
}
