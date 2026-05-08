use crate::agent::error_handling::TurnErrorClass;
use crate::agent::model;
use crate::app::plugins::{PluginsCliActionSuccess, PluginsInventorySnapshot};
use crate::app::{UsageSnapshot, UsageSourceKind};
use crate::error::AppError;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Messages sent from the backend bridge path to the App/UI layer.
pub enum ClientEvent {
    /// Permission request that needs user input.
    PermissionRequest {
        request: model::RequestPermissionRequest,
        response_tx: tokio::sync::oneshot::Sender<model::RequestPermissionResponse>,
    },
    /// Question request from `AskUserQuestion` that needs structured user input.
    QuestionRequest {
        request: model::RequestQuestionRequest,
        response_tx: tokio::sync::oneshot::Sender<model::RequestQuestionResponse>,
    },
    /// MCP elicitation request that needs auth or other MCP input.
    McpElicitationRequest { request: forge_primitives::ElicitationRequest },
    /// MCP elicitation completed in the SDK.
    McpElicitationCompleted { elicitation_id: String, server_name: Option<String> },
    /// MCP auth redirect returned directly by the SDK auth call.
    McpAuthRedirect { redirect: forge_primitives::McpAuthRedirect },
    /// MCP operation failed and should be surfaced in the MCP config UI.
    McpOperationError { error: forge_primitives::McpOperationError },
    /// A prompt turn completed successfully.
    TurnComplete { terminal_reason: Option<forge_primitives::TerminalReason> },
    /// `cancel` notification was accepted by the bridge.
    TurnCancelled,
    /// A prompt turn failed with an error.
    TurnError { message: String, terminal_reason: Option<forge_primitives::TerminalReason> },
    /// A prompt turn failed with bridge-provided classification metadata.
    TurnErrorClassified {
        message: String,
        class: TurnErrorClass,
        terminal_reason: Option<forge_primitives::TerminalReason>,
    },
    /// Background connection completed successfully.
    Connected {
        session_id: model::SessionId,
        cwd: String,
        current_model: model::CurrentModel,
        available_models: Vec<model::AvailableModel>,
        mode: Option<crate::app::ModeState>,
        history_updates: Vec<forge_primitives::Message>,
    },
    /// Background connection failed.
    ConnectionFailed(String),
    /// Authentication is required before a session can be created.
    AuthRequired { method_name: String, method_description: String },
    /// Slash-command execution failed with a user-facing error.
    SlashCommandError(String),
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
    AuthCompleted { conn: Rc<forge_agent::AgentHandle> },
    /// /logout completed via `claude auth logout`.
    LogoutCompleted,
    /// Status snapshot received from bridge (account info).
    StatusSnapshotReceived { session_id: String, account: forge_primitives::AccountInfo },
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
