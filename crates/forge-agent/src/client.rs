use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::sync::mpsc;

/// Behavioural seam between the TUI and the agent backend.
///
/// Pinning callers to a trait rather than a concrete struct lets
/// alternative backends (forge-sdk in-process, future remote daemons,
/// stub implementations for tests) plug in without changing call sites
/// across `app/*`. The current production implementation is
/// `crate::forge_sdk_bridge::ForgeSdkBridge` (private).
///
/// Two method shapes coexist on this trait:
///
/// - **Fire-and-forget commands** — most session-lifecycle methods
///   (`prompt_text`, `cancel`, `set_mode`, …) return
///   `anyhow::Result<()>` and emit results back through the existing
///   [`crate::client::AgentEvent`] stream.
/// - **Direct-return accessors** — synchronous reads/writes that
///   return their result directly (`config_dir`, `oauth_credentials`,
///   `settings_documents`, `write_settings_document`,
///   `project_memory_path`) plus one async method (`oauth_usage`,
///   does HTTPS). The `ForgeSdkBridge` impl delegates these to
///   `forge_sdk::*` free functions today; a remote-daemon impl
///   would do RPC over the same trait shape.
///
/// `#[async_trait(?Send)]` matches the
/// `Rc<dyn AgentBridge>` single-threaded App-state model — futures
/// returned by the trait don't need a `Send` bound.
#[async_trait(?Send)]
pub trait AgentBridge {
    /// Take ownership of the outbound `AgentEvent` receiver. Returns
    /// `Some` exactly once per bridge instance — subsequent calls
    /// return `None`. The connection task calls this immediately after
    /// constructing the bridge to wire up its event-relay loop.
    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<AgentEvent>>;

    fn prompt_text(&self, session_id: String, text: String) -> anyhow::Result<PromptResponse>;

    fn prompt_with_images(
        &self,
        session_id: String,
        text: String,
        images: Vec<forge_primitives::ImageAttachment>,
    ) -> anyhow::Result<PromptResponse>;

    fn cancel(&self, session_id: String) -> anyhow::Result<()>;

    fn set_mode(&self, session_id: String, mode: String) -> anyhow::Result<()>;

    fn set_model(&self, session_id: String, model: String) -> anyhow::Result<()>;

    fn generate_session_title(&self, session_id: String, description: String)
    -> anyhow::Result<()>;

    fn rename_session(&self, session_id: String, title: String) -> anyhow::Result<()>;

    fn get_status_snapshot(&self, session_id: String) -> anyhow::Result<()>;

    fn get_oauth_credentials_snapshot(&self, session_id: String) -> anyhow::Result<()>;

    fn get_context_usage(&self, session_id: String) -> anyhow::Result<()>;

    fn reload_plugins(&self, session_id: String) -> anyhow::Result<()>;

    fn get_mcp_snapshot(&self, session_id: String) -> anyhow::Result<()>;

    fn respond_to_elicitation(
        &self,
        session_id: String,
        elicitation_request_id: String,
        action: forge_primitives::ElicitationAction,
        content: Option<serde_json::Value>,
    ) -> anyhow::Result<()>;

    fn reconnect_mcp_server(&self, session_id: String, server_name: String) -> anyhow::Result<()>;

    fn toggle_mcp_server(
        &self,
        session_id: String,
        server_name: String,
        enabled: bool,
    ) -> anyhow::Result<()>;

    fn set_mcp_servers(
        &self,
        session_id: String,
        servers: std::collections::BTreeMap<String, forge_primitives::McpServerConfig>,
    ) -> anyhow::Result<()>;

    fn authenticate_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()>;

    fn clear_mcp_auth(&self, session_id: String, server_name: String) -> anyhow::Result<()>;

    fn submit_mcp_oauth_callback_url(
        &self,
        session_id: String,
        server_name: String,
        callback_url: String,
    ) -> anyhow::Result<()>;

    fn new_session(
        &self,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()>;

    fn resume_session(
        &self,
        session_id: String,
        launch_settings: SessionLaunchSettings,
    ) -> anyhow::Result<()>;

    fn permission_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: forge_primitives::PermissionOutcome,
    ) -> anyhow::Result<()>;

    fn question_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: forge_primitives::QuestionOutcome,
    ) -> anyhow::Result<()>;

    /// Start watching `cwd`'s `.git` machinery for branch changes.
    /// Snapshots flow back via `AgentEvent::GitContextSnapshot`
    /// (initial state queued before the call returns; subsequent
    /// snapshots only on actual branch change). Calling again with
    /// the same `session_id` aborts and replaces any existing
    /// watcher for that session.
    fn start_git_context_watch(&self, session_id: String, cwd: PathBuf) -> anyhow::Result<()>;

    /// Stop the git-context watcher for `session_id`. No-op when no
    /// watcher is active. Watchers also stop automatically when the
    /// session closes / the bridge worker shuts down.
    fn stop_git_context_watch(&self, session_id: String) -> anyhow::Result<()>;

    // ---- Direct-return accessors (lifted from forge_sdk::* free fns) ----

    /// Resolve the Claude config directory. Honours
    /// `$CLAUDE_CONFIG_DIR` (when set + non-empty) else falls back
    /// to `$HOME/.claude`.
    fn config_dir(&self) -> PathBuf;

    /// Resolve the project's auto-memory file:
    /// `<config_dir>/projects/<project_key>/memory/MEMORY.md`. The
    /// returned path may not exist on disk; callers decide.
    fn project_memory_path(&self, cwd: &Path) -> PathBuf;

    /// Read OAuth credentials from `<config_dir>/.credentials.json`
    /// or, on macOS, the matching keychain entry. Returns `None`
    /// when no credentials are present.
    fn oauth_credentials(&self) -> Option<crate::cloud::oauth_credentials::OauthCredentials>;

    /// Read all three settings documents (user, project-local,
    /// preferences) from disk. Each field is `None` when the
    /// underlying file is missing or unreadable.
    fn settings_documents(&self, cwd: &Path) -> crate::userdata::settings::SettingsDocuments;

    /// Atomically write a settings document to the path
    /// [`crate::userdata::settings::SettingsTarget`] resolves to.
    ///
    /// # Errors
    ///
    /// Returns [`forge_sdk::Error`] when the underlying write fails
    /// — see [`crate::userdata::settings::write_settings_document`]
    /// for the failure modes.
    fn write_settings_document(
        &self,
        target: &crate::userdata::settings::SettingsTarget,
        document: &serde_json::Value,
    ) -> Result<(), forge_sdk::Error>;

    /// Fetch the OAuth usage payload from
    /// `api.anthropic.com/api/oauth/usage`. The bearer token is
    /// resolved internally; it never crosses the trait boundary.
    ///
    /// # Errors
    ///
    /// See [`crate::cloud::oauth_usage::OauthUsageError`].
    async fn oauth_usage(
        &self,
    ) -> Result<crate::cloud::oauth_usage::OauthUsage, crate::cloud::oauth_usage::OauthUsageError>;
}

#[derive(Debug, Clone)]
pub struct PromptResponse {
    pub stop_reason: String,
}
use forge_primitives as types;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLaunchSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_progress_summaries: Option<bool>,
}

impl SessionLaunchSettings {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.language.is_none()
            && self.settings.is_none()
            && self.agent_progress_summaries.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub event: AgentEvent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    Connected {
        session_id: String,
        cwd: String,
        current_model: types::CurrentModel,
        #[serde(default)]
        available_models: Vec<types::AvailableModel>,
        mode: Option<types::ModeState>,
        history_updates: Option<Vec<types::SessionUpdate>>,
    },
    AuthRequired {
        method_name: String,
        method_description: String,
    },
    ConnectionFailed {
        message: String,
    },
    PermissionRequest {
        session_id: String,
        request: types::PermissionRequest,
    },
    QuestionRequest {
        session_id: String,
        request: types::QuestionRequest,
    },
    ElicitationRequest {
        session_id: String,
        request: types::ElicitationRequest,
    },
    ElicitationComplete {
        session_id: String,
        elicitation_id: String,
        server_name: Option<String>,
    },
    McpAuthRedirect {
        session_id: String,
        redirect: types::McpAuthRedirect,
    },
    McpOperationError {
        session_id: String,
        error: types::McpOperationError,
    },
    SlashError {
        session_id: String,
        message: String,
    },
    RuntimeReloadCompleted {
        session_id: String,
    },
    RuntimeReloadFailed {
        session_id: String,
        message: String,
    },
    SessionReplaced {
        session_id: String,
        cwd: String,
        current_model: types::CurrentModel,
        #[serde(default)]
        available_models: Vec<types::AvailableModel>,
        mode: Option<types::ModeState>,
        history_updates: Option<Vec<types::SessionUpdate>>,
    },
    SessionsListed {
        sessions: Vec<types::SessionListEntry>,
    },
    StatusSnapshot {
        session_id: String,
        account: forge_sdk::AccountInfo,
    },
    OauthCredentialsSnapshot {
        session_id: String,
        credentials: Option<crate::cloud::oauth_credentials::OauthCredentials>,
    },
    GitContextSnapshot {
        session_id: String,
        context: crate::env::git::GitContext,
    },
    ContextUsage {
        session_id: String,
        percentage: Option<u8>,
    },
    McpSnapshot {
        session_id: String,
        #[serde(default)]
        servers: Vec<forge_sdk::McpServerStatus>,
        error: Option<String>,
    },
    /// Raw `forge_sdk::Message` envelope flowing in parallel to
    /// `SessionUpdate` events during the bridge-collapse refactor.
    /// Phase 1.3 emits these alongside the bridge's existing
    /// `SessionUpdate`s; Phase 2 progressively migrates per-variant
    /// dispatch to the App's `handle_sdk_message`; Phase 3 removes
    /// the bridge unpacker and `SessionUpdate` entirely.
    SdkMessage {
        session_id: String,
        msg: forge_sdk::Message,
    },
}

impl AgentEvent {
    #[must_use]
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Connected { .. } => "connected",
            Self::AuthRequired { .. } => "auth_required",
            Self::ConnectionFailed { .. } => "connection_failed",
            Self::PermissionRequest { .. } => "permission_request",
            Self::QuestionRequest { .. } => "question_request",
            Self::ElicitationRequest { .. } => "elicitation_request",
            Self::ElicitationComplete { .. } => "elicitation_complete",
            Self::McpAuthRedirect { .. } => "mcp_auth_redirect",
            Self::McpOperationError { .. } => "mcp_operation_error",
            Self::SlashError { .. } => "slash_error",
            Self::RuntimeReloadCompleted { .. } => "runtime_reload_completed",
            Self::RuntimeReloadFailed { .. } => "runtime_reload_failed",
            Self::SessionReplaced { .. } => "session_replaced",
            Self::SessionsListed { .. } => "sessions_listed",
            Self::StatusSnapshot { .. } => "status_snapshot",
            Self::OauthCredentialsSnapshot { .. } => "oauth_credentials_snapshot",
            Self::GitContextSnapshot { .. } => "git_context_snapshot",
            Self::ContextUsage { .. } => "context_usage",
            Self::McpSnapshot { .. } => "mcp_snapshot",
            Self::SdkMessage { .. } => "sdk_message",
        }
    }

    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Connected { session_id, .. }
            | Self::PermissionRequest { session_id, .. }
            | Self::QuestionRequest { session_id, .. }
            | Self::ElicitationRequest { session_id, .. }
            | Self::ElicitationComplete { session_id, .. }
            | Self::McpAuthRedirect { session_id, .. }
            | Self::McpOperationError { session_id, .. }
            | Self::SlashError { session_id, .. }
            | Self::RuntimeReloadCompleted { session_id, .. }
            | Self::RuntimeReloadFailed { session_id, .. }
            | Self::SessionReplaced { session_id, .. }
            | Self::StatusSnapshot { session_id, .. }
            | Self::OauthCredentialsSnapshot { session_id, .. }
            | Self::GitContextSnapshot { session_id, .. }
            | Self::ContextUsage { session_id, .. }
            | Self::McpSnapshot { session_id, .. }
            | Self::SdkMessage { session_id, .. } => Some(session_id.as_str()),
            Self::AuthRequired { .. }
            | Self::ConnectionFailed { .. }
            | Self::SessionsListed { .. } => None,
        }
    }

    #[must_use]
    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::PermissionRequest { request, .. } => {
                Some(request.tool_call.tool_call_id.as_str())
            }
            Self::QuestionRequest { request, .. } => Some(request.tool_call.tool_call_id.as_str()),
            Self::Connected { .. }
            | Self::AuthRequired { .. }
            | Self::ConnectionFailed { .. }
            | Self::ElicitationRequest { .. }
            | Self::ElicitationComplete { .. }
            | Self::McpAuthRedirect { .. }
            | Self::McpOperationError { .. }
            | Self::SlashError { .. }
            | Self::RuntimeReloadCompleted { .. }
            | Self::RuntimeReloadFailed { .. }
            | Self::SessionReplaced { .. }
            | Self::SessionsListed { .. }
            | Self::StatusSnapshot { .. }
            | Self::OauthCredentialsSnapshot { .. }
            | Self::GitContextSnapshot { .. }
            | Self::ContextUsage { .. }
            | Self::McpSnapshot { .. }
            | Self::SdkMessage { .. } => None,
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[cfg(test)]
mod tests {
    use super::{AgentEvent, EventEnvelope, SessionLaunchSettings};
    use forge_primitives as types;

    #[test]
    fn event_envelope_roundtrip_json() {
        let env = EventEnvelope {
            request_id: None,
            event: AgentEvent::SessionsListed {
                sessions: Vec::new(),
            },
        };
        let _ = types::TerminalReason::Completed; // keep import alive
        let json = serde_json::to_string(&env).expect("serialize");
        let decoded: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, env);
    }

    #[test]
    fn session_launch_settings_serializes_agent_progress_summaries() {
        let settings = SessionLaunchSettings {
            settings: Some(serde_json::json!({ "model": "haiku" })),
            agent_progress_summaries: Some(true),
            ..SessionLaunchSettings::default()
        };

        let json = serde_json::to_value(&settings).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "settings": { "model": "haiku" },
                "agent_progress_summaries": true
            })
        );
    }
}
