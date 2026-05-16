//! Channel-based [`Agent`] handle — the public consumer surface.
//!
//! `Agent::spawn` constructs a `ForgeSdkBridge` under the hood and
//! spawns one background task: the **command dispatcher**, which
//! drains `mpsc::UnboundedReceiver<Command>` and calls the matching
//! inherent method on the bridge. The bridge's `AgentEvent`
//! receiver is handed back to consumers via `take_events()`.
//!
//! Direct-return accessors (config_dir, oauth_credentials,
//! settings_documents, etc.) live on [`AgentHandle`] as method
//! passthroughs to the bridge.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_primitives::Command;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::forge_sdk_bridge::ForgeSdkBridge;

/// Handle returned by [`Agent::spawn`]. Owns the channels + a thin
/// passthrough to the bridge's direct-return accessors.
pub struct AgentHandle {
    /// UI → agent commands. Used internally by the inherent
    /// command-shorthand methods (`prompt_text`, `cancel`, etc.). Public
    /// so callers can `send(Command::...)` directly when richer flows
    /// are needed.
    pub commands: mpsc::UnboundedSender<Command>,
    /// Bridge's raw `AgentEvent` receiver. Single-take via
    /// [`AgentHandle::take_events`] — forge-tui's translator consumes
    /// this and converts to its `ClientEvent` shape.
    agent_events: Mutex<Option<mpsc::UnboundedReceiver<crate::client::AgentEvent>>>,
    /// Bridge handle used for direct-return accessors (config_dir,
    /// settings_documents, oauth_*) plus internal command dispatch.
    bridge: Arc<ForgeSdkBridge>,
}

impl AgentHandle {
    /// Take ownership of the bridge's `AgentEvent` receiver. Returns
    /// `Some` exactly once.
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<crate::client::AgentEvent>> {
        self.agent_events.lock().take()
    }

    /// Direct-accessor passthrough — delegates to `ForgeSdkBridge::config_dir`.
    pub fn config_dir(&self) -> PathBuf {
        self.bridge.config_dir()
    }

    /// OS PID of the bound `claude` child, when one is currently
    /// attached. Surfaces to forge-workspace for the Inspector
    /// pane's PROCESSES OS-walk.
    pub fn claude_pid(&self) -> Option<u32> {
        self.bridge.claude_pid()
    }

    /// Direct-accessor passthrough.
    pub fn project_memory_path(&self, cwd: &Path) -> PathBuf {
        self.bridge.project_memory_path(cwd)
    }

    /// Direct-accessor passthrough.
    pub fn oauth_credentials(&self) -> Option<crate::cloud::oauth_credentials::OauthCredentials> {
        self.bridge.oauth_credentials()
    }

    /// Direct-accessor passthrough.
    pub fn settings_documents(&self, cwd: &Path) -> crate::userdata::settings::SettingsDocuments {
        self.bridge.settings_documents(cwd)
    }

    /// Direct-accessor passthrough.
    pub fn write_settings_document(
        &self,
        target: &crate::userdata::settings::SettingsTarget,
        document: &serde_json::Value,
    ) -> Result<(), forge_sdk::Error> {
        self.bridge.write_settings_document(target, document)
    }

    /// Direct-accessor passthrough.
    pub async fn oauth_usage(
        &self,
    ) -> Result<crate::cloud::oauth_usage::OauthUsage, crate::cloud::oauth_usage::OauthUsageError>
    {
        self.bridge.oauth_usage().await
    }

    /// Test-only accessor returning a clone of the bridge's bound
    /// `config_dir`. Hidden from public docs; production code reads
    /// the path via the spawn path or via [`AgentHandle::config_dir`].
    /// `#[doc(hidden)] pub` rather than `#[cfg(test)]` so integration
    /// tests in sibling crates' `tests/` directories can reach it
    /// (Rust's `#[cfg(test)]` items aren't visible across crate
    /// boundaries).
    #[doc(hidden)]
    pub fn config_dir_for_test(&self) -> PathBuf {
        self.bridge.config_dir()
    }

    /// Returns a clone of the bridge's bound forge-account
    /// `display_name` (when forge-workspace picked one). Used by
    /// the connect-flow to emit a `ForgeAccountIdentityReady`
    /// event right after spawn — eliminates the welcome-message
    /// flicker that would otherwise wait for the slow status
    /// snapshot to arrive from the CLI.
    pub fn display_name(&self) -> Option<String> {
        self.bridge.display_name()
    }

    // ---- Fire-and-forget Command shorthands ----
    //
    // Each method builds the matching `Command` variant and pushes it
    // onto `commands`. Returns `Err` only if the dispatcher task has
    // shut down (channel closed). Errors from the underlying
    // forge_sdk::Client surface asynchronously via the events stream.

    fn send(&self, cmd: Command) -> anyhow::Result<()> {
        self.commands.send(cmd).map_err(|_| anyhow::anyhow!("agent dispatcher shut down"))
    }

    pub fn new_session(
        &self,
        cwd: String,
        launch_settings: crate::client::SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        // Propagate serialise failure instead of silently launching
        // with `Value::Null` (which the dispatcher then deserialises
        // to default settings, losing the user's configured model /
        // permission_mode / effort with no breadcrumb). The struct
        // is `Default`able so a real serialise miss is unlikely;
        // making it explicit catches forward-compat breakage early.
        let launch_settings = serde_json::to_value(launch_settings)
            .map_err(|e| anyhow::anyhow!("failed to encode launch settings: {e}"))?;
        self.send(Command::NewSession { cwd, launch_settings })
    }

    pub fn resume_session(
        &self,
        session_id: String,
        cwd: String,
        launch_settings: crate::client::SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let launch_settings = serde_json::to_value(launch_settings)
            .map_err(|e| anyhow::anyhow!("failed to encode launch settings: {e}"))?;
        self.send(Command::ResumeSession { session_id: session_id.into(), cwd, launch_settings })
    }

    /// Resume the recorded `session_id`; if resume fails (stale
    /// catalog entry, cross-account scan, deleted `.jsonl`), retry as
    /// a fresh session in `cwd`. The user only sees a
    /// `ConnectionFailed` event when both attempts fail.
    pub fn resume_or_new_session(
        &self,
        session_id: String,
        cwd: String,
        launch_settings: crate::client::SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        let launch_settings = serde_json::to_value(launch_settings)
            .map_err(|e| anyhow::anyhow!("failed to encode launch settings: {e}"))?;
        self.send(Command::ResumeOrNewSession {
            session_id: session_id.into(),
            cwd,
            launch_settings,
        })
    }

    pub fn prompt_text(&self, session_id: String, text: String) -> anyhow::Result<()> {
        self.send(Command::Prompt { session_id: session_id.into(), text })
    }

    pub fn prompt_with_images(
        &self,
        session_id: String,
        text: String,
        images: Vec<forge_primitives::ImageAttachment>,
    ) -> anyhow::Result<()> {
        self.send(Command::PromptWithImages { session_id: session_id.into(), text, images })
    }

    pub fn cancel(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::Cancel { session_id: session_id.into() })
    }

    pub fn set_mode(&self, session_id: String, mode: String) -> anyhow::Result<()> {
        self.send(Command::SetMode { session_id: session_id.into(), mode })
    }

    pub fn set_model(&self, session_id: String, model: String) -> anyhow::Result<()> {
        self.send(Command::SetModel { session_id: session_id.into(), model })
    }

    pub fn generate_session_title(
        &self,
        session_id: String,
        description: String,
    ) -> anyhow::Result<()> {
        self.send(Command::GenerateSessionTitle { session_id: session_id.into(), description })
    }

    pub fn rename_session(&self, session_id: String, title: String) -> anyhow::Result<()> {
        self.send(Command::RenameSession { session_id: session_id.into(), title })
    }

    pub fn get_status_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::GetStatusSnapshot { session_id: session_id.into() })
    }

    pub fn get_oauth_credentials_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::GetOauthCredentialsSnapshot { session_id: session_id.into() })
    }

    pub fn get_context_usage(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::GetContextUsage { session_id: session_id.into() })
    }

    pub fn reload_plugins(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::ReloadPlugins { session_id: session_id.into() })
    }

    pub fn get_mcp_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::GetMcpSnapshot { session_id: session_id.into() })
    }

    pub fn respond_to_elicitation(
        &self,
        session_id: String,
        elicitation_request_id: String,
        action: forge_primitives::ElicitationAction,
        content: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        self.send(Command::RespondToElicitation {
            session_id: session_id.into(),
            elicitation_request_id,
            action,
            content,
        })
    }

    pub fn reconnect_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        self.send(Command::ReconnectMcpServer { session_id: session_id.into(), server_name })
    }

    pub fn toggle_mcp_server(
        &self,
        session_id: String,
        server_name: String,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.send(Command::ToggleMcpServer { session_id: session_id.into(), server_name, enabled })
    }

    pub fn set_mcp_servers(
        &self,
        session_id: String,
        servers: std::collections::BTreeMap<String, forge_primitives::McpServerConfig>,
    ) -> anyhow::Result<()> {
        self.send(Command::SetMcpServers { session_id: session_id.into(), servers })
    }

    pub fn authenticate_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        self.send(Command::AuthenticateMcpServer { session_id: session_id.into(), server_name })
    }

    pub fn clear_mcp_auth(&self, session_id: String, server_name: String) -> anyhow::Result<()> {
        self.send(Command::ClearMcpAuth { session_id: session_id.into(), server_name })
    }

    pub fn submit_mcp_oauth_callback_url(
        &self,
        session_id: String,
        server_name: String,
        callback_url: String,
    ) -> anyhow::Result<()> {
        self.send(Command::SubmitMcpOauthCallbackUrl {
            session_id: session_id.into(),
            server_name,
            callback_url,
        })
    }

    pub fn permission_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: forge_primitives::PermissionOutcome,
    ) -> anyhow::Result<()> {
        self.send(Command::PermissionResponse {
            session_id: session_id.into(),
            tool_call_id: tool_call_id.into(),
            outcome,
        })
    }

    pub fn question_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: forge_primitives::QuestionOutcome,
    ) -> anyhow::Result<()> {
        self.send(Command::QuestionResponse {
            session_id: session_id.into(),
            tool_call_id: tool_call_id.into(),
            outcome,
        })
    }
}

/// Agent factory — wraps a private `ForgeSdkBridge` behind a channel API.
pub struct Agent;

impl Agent {
    /// Construct a test stub: `AgentHandle` backed by a fresh
    /// ForgeSdkBridge that's never actually driven (no `new_session`
    /// call). Returns the handle plus a `Receiver<Command>` that
    /// drains every command the test exercises.
    ///
    /// Safe to call outside a Tokio runtime — no tasks are spawned.
    /// The bridge's events stream is dropped on the floor; tests that
    /// don't drive sessions never see events anyway. The bridge is
    /// bound to a synthetic `/tmp/forge-testing-stub` config_dir;
    /// since no session is driven, no I/O hits this path.
    pub fn testing_stub() -> (AgentHandle, mpsc::UnboundedReceiver<Command>) {
        let bridge = ForgeSdkBridge::default();
        // Drop the bridge's events receiver immediately — tests don't
        // run a real session so nothing is producing.
        let _ = bridge.take_events();

        let (commands_tx, commands_rx) = mpsc::unbounded_channel::<Command>();
        // Hand a fresh empty channel as the agent_events receiver so
        // the AgentHandle shape matches production. Nothing will ever
        // push to it; `take_events()` returns the dead receiver and
        // `recv()` parks forever.
        let (_dead_tx, dead_rx) = mpsc::unbounded_channel::<crate::client::AgentEvent>();

        let handle = AgentHandle {
            commands: commands_tx,
            agent_events: Mutex::new(Some(dead_rx)),
            bridge: Arc::new(bridge),
        };
        (handle, commands_rx)
    }

    /// Spawn a new agent runtime bound to `config_dir` with an
    /// optional forge-account `display_name`. Both are stored on
    /// the bridge as typed fields. `config_dir` is consulted by
    /// every in-process accessor (oauth, settings, catalog scans)
    /// and exported to the spawned `claude` subprocess as
    /// `CLAUDE_CONFIG_DIR`. `display_name`, when set, is surfaced
    /// via [`crate::client::AgentEvent::StatusSnapshot`] so the TUI
    /// renders which forge-account the bridge is bound to. Returns a handle
    /// holding the command sender + events receiver + direct-
    /// accessor passthroughs.
    pub fn spawn(config_dir: PathBuf, display_name: Option<String>) -> AgentHandle {
        let bridge = ForgeSdkBridge::new(config_dir, display_name);
        let agent_event_rx = bridge.take_events().unwrap_or_else(|| mpsc::unbounded_channel().1);

        let (commands_tx, commands_rx) = mpsc::unbounded_channel::<Command>();

        // Command dispatcher task.
        let dispatch_bridge = Arc::new(bridge);
        tokio::spawn(dispatch_commands(commands_rx, Arc::clone(&dispatch_bridge)));

        AgentHandle {
            commands: commands_tx,
            agent_events: Mutex::new(Some(agent_event_rx)),
            bridge: dispatch_bridge,
        }
    }
}

async fn dispatch_commands(
    mut commands_rx: mpsc::UnboundedReceiver<Command>,
    bridge: Arc<ForgeSdkBridge>,
) {
    while let Some(cmd) = commands_rx.recv().await {
        if let Err(err) = dispatch(cmd, &bridge) {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                error = %err,
                "agent command dispatch failed",
            );
        }
    }
}

/// Dispatch one `Command` to the matching `ForgeSdkBridge` method.
fn dispatch(cmd: Command, bridge: &ForgeSdkBridge) -> anyhow::Result<()> {
    use forge_primitives::Command as C;

    match cmd {
        C::NewSession { cwd, launch_settings } => {
            // Symmetric to the encode-side `?`-propagation in
            // `AgentHandle::new_session`. Round-trip is safe in
            // practice when `Serialize`/`Deserialize` are mutually
            // consistent (the standard derive pair), but a
            // forward-compat break in SessionLaunchSettings — e.g.
            // adding `#[serde(deny_unknown_fields)]` or splitting a
            // field — would silently strip user config here without
            // this log.
            let launch = serde_json::from_value(launch_settings).unwrap_or_else(|e| {
                tracing::error!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    error = %e,
                    "failed to decode launch_settings on dispatcher receive; falling back to default",
                );
                crate::client::SessionLaunchSettings::default()
            });
            bridge.new_session(cwd, launch)
        }
        C::ResumeSession { session_id, cwd, launch_settings } => {
            let launch = serde_json::from_value(launch_settings).unwrap_or_else(|e| {
                tracing::error!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    error = %e,
                    "failed to decode launch_settings on dispatcher receive; falling back to default",
                );
                crate::client::SessionLaunchSettings::default()
            });
            bridge.resume_session(session_id.into_string(), cwd, launch)
        }
        C::ResumeOrNewSession { session_id, cwd, launch_settings } => {
            let launch = serde_json::from_value(launch_settings).unwrap_or_else(|e| {
                tracing::error!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    error = %e,
                    "failed to decode launch_settings on dispatcher receive; falling back to default",
                );
                crate::client::SessionLaunchSettings::default()
            });
            bridge.resume_or_new_session(session_id.into_string(), cwd, launch)
        }
        C::Prompt { session_id, text } => bridge.prompt_text(session_id.into_string(), text),
        C::PromptWithImages { session_id, text, images } => {
            bridge.prompt_with_images(session_id.into_string(), text, images)
        }
        C::Cancel { session_id } => bridge.cancel(session_id.into_string()),
        C::SetMode { session_id, mode } => bridge.set_mode(session_id.into_string(), mode),
        C::SetModel { session_id, model } => bridge.set_model(session_id.into_string(), model),
        C::GenerateSessionTitle { session_id, description } => {
            bridge.generate_session_title(session_id.into_string(), description)
        }
        C::RenameSession { session_id, title } => {
            bridge.rename_session(session_id.into_string(), title)
        }
        C::RewindFiles { session_id, user_message_id } => {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                session_id = %session_id,
                user_message_id = %user_message_id,
                "Command::RewindFiles dispatched but bridge surface not yet wired; dropping",
            );
            Ok(())
        }
        C::GetStatusSnapshot { session_id } => bridge.get_status_snapshot(session_id.into_string()),
        C::GetOauthCredentialsSnapshot { session_id } => {
            bridge.get_oauth_credentials_snapshot(session_id.into_string())
        }
        C::GetContextUsage { session_id } => bridge.get_context_usage(session_id.into_string()),
        C::GetMcpSnapshot { session_id } => bridge.get_mcp_snapshot(session_id.into_string()),
        C::PermissionResponse { session_id, tool_call_id, outcome } => bridge.permission_response(
            session_id.into_string(),
            tool_call_id.into_string(),
            outcome,
        ),
        C::QuestionResponse { session_id, tool_call_id, outcome } => {
            bridge.question_response(session_id.into_string(), tool_call_id.into_string(), outcome)
        }
        C::RespondToElicitation { session_id, elicitation_request_id, action, content } => bridge
            .respond_to_elicitation(
                session_id.into_string(),
                elicitation_request_id,
                action,
                content,
            ),
        C::ReconnectMcpServer { session_id, server_name } => {
            bridge.reconnect_mcp_server(session_id.into_string(), server_name)
        }
        C::ToggleMcpServer { session_id, server_name, enabled } => {
            bridge.toggle_mcp_server(session_id.into_string(), server_name, enabled)
        }
        C::SetMcpServers { session_id, servers } => {
            bridge.set_mcp_servers(session_id.into_string(), servers)
        }
        C::AuthenticateMcpServer { session_id, server_name } => {
            bridge.authenticate_mcp_server(session_id.into_string(), server_name)
        }
        C::ClearMcpAuth { session_id, server_name } => {
            bridge.clear_mcp_auth(session_id.into_string(), server_name)
        }
        C::SubmitMcpOauthCallbackUrl { session_id, server_name, callback_url } => bridge
            .submit_mcp_oauth_callback_url(session_id.into_string(), server_name, callback_url),
        C::ReloadPlugins { session_id } => bridge.reload_plugins(session_id.into_string()),
    }
}
