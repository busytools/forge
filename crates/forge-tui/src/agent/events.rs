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
    /// Phase 1+: `response_tx` no longer rides on this envelope —
    /// workspace owns the oneshot. `session_key` carries the routing
    /// key and `tool_id` indexes into
    /// `DomainSession.pending_interactions` for the matching reply.
    PermissionRequest {
        session_key: SessionKey,
        tool_id: String,
        request: model::RequestPermissionRequest,
    },
    /// Question request from `AskUserQuestion` that needs structured user input.
    ///
    /// Same shape change as [`Self::PermissionRequest`].
    QuestionRequest {
        session_key: SessionKey,
        tool_id: String,
        request: model::RequestQuestionRequest,
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
        /// AgentHandle for the freshly-spawned session, routed
        /// alongside its identifying envelope so the App can install
        /// the conn onto the right bucket without consulting a
        /// thread-local CONN_SLOT (which races under rapid spawn-
        /// sleeping clicks — second click's slot pointer replaces
        /// the first's before either Connected lands).
        conn: Arc<forge_agent::AgentHandle>,
    },
    /// Background connection failed.
    ConnectionFailed { session_key: SessionKey, message: String },
    /// Authentication is required before a session can be created.
    AuthRequired { session_key: SessionKey, method_name: String, method_description: String },
    /// Slash-command execution failed with a user-facing error.
    SlashCommandError { session_key: SessionKey, message: String },
    /// Custom slash command replaced the active session.
    SessionReplaced {
        session_id: model::SessionId,
        cwd: String,
        current_model: model::CurrentModel,
        available_models: Vec<model::AvailableModel>,
        mode: Option<crate::app::ModeState>,
        history_updates: Vec<forge_primitives::Message>,
        /// AgentHandle for the replacement session (`/new`, login,
        /// logout). Same Arc identity as the bridge's previous
        /// AgentHandle — the bridge swapped its internal Client to
        /// the new CLI subprocess but the handle is unchanged — so
        /// installing it on the new bucket carries the conn forward.
        conn: Arc<forge_agent::AgentHandle>,
    },
    /// Recent sessions discovered via SDK session listing.
    SessionsListed { sessions: Vec<forge_primitives::SessionListEntry> },
    /// Startup Claude Code status check detected degraded/outage conditions.
    ServiceStatus { severity: ServiceStatusSeverity, message: String },
    /// /login completed via `claude auth login` -- credentials stored, ready to start a session.
    AuthCompleted { session_key: SessionKey, conn: Arc<forge_agent::AgentHandle> },
    /// /logout completed via `claude auth logout`.
    LogoutCompleted { session_key: SessionKey },
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
    /// Transitional Phase 1-4: [`forge_workspace::SessionUpdate`]
    /// routed through the `ClientEvent` channel for unified dispatch
    /// in the main app event loop. Phase 4 deletes this variant when
    /// `SessionUpdate` becomes the primary channel.
    WorkspaceUpdate(forge_workspace::SessionUpdate),
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
            Self::PermissionRequest { session_key, .. }
            | Self::QuestionRequest { session_key, .. }
            | Self::McpElicitationRequest { session_key, .. }
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
            | Self::LogoutCompleted { session_key } => Some(session_key.clone()),
            Self::Connected { session_id, .. } | Self::SessionReplaced { session_id, .. } => {
                Some(SessionKey::from_session_id(session_id.to_string()))
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
            Self::WorkspaceUpdate(update) => update.session_key(),
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
