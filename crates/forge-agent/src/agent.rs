//! Channel-based [`Agent`] handle — the public API forge-tui will
//! migrate to in phase 5.
//!
//! `Agent::spawn` constructs a [`ForgeSdkBridge`] under the hood and
//! starts two background tasks:
//!
//! 1. **Event translator** — drains the bridge's `AgentEvent` receiver
//!    and forwards each event onto `events_tx` as a
//!    [`forge_primitives::Event`].
//! 2. **Command dispatcher** — drains an `mpsc::UnboundedReceiver<Command>`
//!    and calls the matching `AgentBridge` method on the bridge.
//!
//! Direct-return accessors (config_dir, oauth_credentials, settings_documents,
//! etc.) live on [`AgentHandle`] as direct method passthroughs; phase 6
//! will move them off the bridge into userdata Commands and remove this
//! escape hatch.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use forge_primitives::{Command, Event};
use tokio::sync::mpsc;

use crate::client::AgentBridge;
use crate::forge_sdk_bridge::ForgeSdkBridge;

/// Handle returned by [`Agent::spawn`]. Owns the channels + a thin
/// passthrough to the bridge's direct-return accessors.
pub struct AgentHandle {
    /// UI → agent commands. Used internally by the inherent
    /// command-shorthand methods (`prompt_text`, `cancel`, etc.).
    /// Public so phase 6 callers can `send(Command::...)` directly
    /// when richer flows are needed.
    pub commands: mpsc::UnboundedSender<Command>,
    /// Translated `forge_primitives::Event` receiver. Currently
    /// unused at runtime (phase 6 wires it); held here so the
    /// translator task has somewhere to push.
    #[allow(dead_code)]
    events: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
    /// Backward-compat receiver: hands callers the bridge's raw
    /// `AgentEvent` stream so existing translators (forge-tui's
    /// `bridge_lifecycle::AgentEvent → ClientEvent`) keep working.
    /// Phase 6 deletes this when call sites move to the Event stream.
    agent_events: Mutex<Option<mpsc::UnboundedReceiver<crate::client::AgentEvent>>>,
    /// Bridge handle used for direct-return accessors only.
    /// Phase 6 will eliminate this when accessors migrate.
    bridge: Arc<ForgeSdkBridge>,
}

impl AgentHandle {
    /// Take ownership of the bridge's raw `AgentEvent` receiver.
    /// Returns `Some` exactly once. forge-tui's existing translator
    /// consumes this; phase 6 swaps to the translated Event stream.
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<crate::client::AgentEvent>> {
        self.agent_events.lock().ok().and_then(|mut g| g.take())
    }

    /// Take ownership of the translated [`Event`] receiver — phase 6 API.
    /// Currently unused at runtime; reserved for the channel migration.
    #[allow(dead_code)]
    pub fn take_event_stream(&self) -> Option<mpsc::UnboundedReceiver<Event>> {
        self.events.lock().ok().and_then(|mut g| g.take())
    }

    /// Direct-accessor passthrough — see [`crate::client::AgentBridge::config_dir`].
    #[must_use]
    pub fn config_dir(&self) -> PathBuf {
        self.bridge.config_dir()
    }

    /// Direct-accessor passthrough.
    #[must_use]
    pub fn project_memory_path(&self, cwd: &Path) -> PathBuf {
        self.bridge.project_memory_path(cwd)
    }

    /// Direct-accessor passthrough.
    #[must_use]
    pub fn oauth_credentials(&self) -> Option<forge_sdk::OauthCredentials> {
        self.bridge.oauth_credentials()
    }

    /// Direct-accessor passthrough.
    #[must_use]
    pub fn settings_documents(&self, cwd: &Path) -> forge_sdk::SettingsDocuments {
        self.bridge.settings_documents(cwd)
    }

    /// Direct-accessor passthrough.
    pub fn write_settings_document(
        &self,
        target: &forge_sdk::SettingsTarget,
        document: &serde_json::Value,
    ) -> Result<(), forge_sdk::Error> {
        self.bridge.write_settings_document(target, document)
    }

    /// Direct-accessor passthrough.
    pub async fn oauth_usage(&self) -> Result<forge_sdk::OauthUsage, forge_sdk::OauthUsageError> {
        self.bridge.oauth_usage().await
    }

    // ---- Fire-and-forget Command shorthands ----
    //
    // Each method builds the matching `Command` variant and pushes it
    // onto `commands`. Returns `Err` only if the dispatcher task has
    // shut down (channel closed). Errors from the underlying
    // forge_sdk::Client surface asynchronously via the events stream.

    fn send(&self, cmd: Command) -> anyhow::Result<()> {
        self.commands
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("agent dispatcher shut down"))
    }

    pub fn new_session(
        &self,
        cwd: String,
        launch_settings: crate::client::SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        self.send(Command::NewSession {
            cwd,
            launch_settings: serde_json::to_value(launch_settings)
                .unwrap_or(serde_json::Value::Null),
        })
    }

    pub fn resume_session(
        &self,
        session_id: String,
        launch_settings: crate::client::SessionLaunchSettings,
    ) -> anyhow::Result<()> {
        self.send(Command::ResumeSession {
            session_id: session_id.into(),
            launch_settings: serde_json::to_value(launch_settings)
                .unwrap_or(serde_json::Value::Null),
        })
    }

    pub fn prompt_text(
        &self,
        session_id: String,
        text: String,
    ) -> anyhow::Result<crate::client::PromptResponse> {
        self.send(Command::Prompt {
            session_id: session_id.into(),
            text,
        })?;
        Ok(crate::client::PromptResponse {
            stop_reason: "in_progress".to_owned(),
        })
    }

    pub fn prompt_with_images(
        &self,
        session_id: String,
        text: String,
        images: Vec<forge_primitives::ImageAttachment>,
    ) -> anyhow::Result<crate::client::PromptResponse> {
        self.send(Command::PromptWithImages {
            session_id: session_id.into(),
            text,
            images,
        })?;
        Ok(crate::client::PromptResponse {
            stop_reason: "in_progress".to_owned(),
        })
    }

    pub fn cancel(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::Cancel {
            session_id: session_id.into(),
        })
    }

    pub fn set_mode(&self, session_id: String, mode: String) -> anyhow::Result<()> {
        self.send(Command::SetMode {
            session_id: session_id.into(),
            mode,
        })
    }

    pub fn set_model(&self, session_id: String, model: String) -> anyhow::Result<()> {
        self.send(Command::SetModel {
            session_id: session_id.into(),
            model,
        })
    }

    pub fn generate_session_title(
        &self,
        session_id: String,
        description: String,
    ) -> anyhow::Result<()> {
        self.send(Command::GenerateSessionTitle {
            session_id: session_id.into(),
            description,
        })
    }

    pub fn rename_session(&self, session_id: String, title: String) -> anyhow::Result<()> {
        self.send(Command::RenameSession {
            session_id: session_id.into(),
            title,
        })
    }

    pub fn get_status_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::GetStatusSnapshot {
            session_id: session_id.into(),
        })
    }

    pub fn get_oauth_credentials_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::GetOauthCredentialsSnapshot {
            session_id: session_id.into(),
        })
    }

    pub fn get_context_usage(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::GetContextUsage {
            session_id: session_id.into(),
        })
    }

    pub fn reload_plugins(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::ReloadPlugins {
            session_id: session_id.into(),
        })
    }

    pub fn get_mcp_snapshot(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::GetMcpSnapshot {
            session_id: session_id.into(),
        })
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
        self.send(Command::ReconnectMcpServer {
            session_id: session_id.into(),
            server_name,
        })
    }

    pub fn toggle_mcp_server(
        &self,
        session_id: String,
        server_name: String,
        enabled: bool,
    ) -> anyhow::Result<()> {
        self.send(Command::ToggleMcpServer {
            session_id: session_id.into(),
            server_name,
            enabled,
        })
    }

    pub fn set_mcp_servers(
        &self,
        session_id: String,
        servers: std::collections::BTreeMap<String, forge_primitives::McpServerConfig>,
    ) -> anyhow::Result<()> {
        self.send(Command::SetMcpServers {
            session_id: session_id.into(),
            servers,
        })
    }

    pub fn authenticate_mcp_server(
        &self,
        session_id: String,
        server_name: String,
    ) -> anyhow::Result<()> {
        self.send(Command::AuthenticateMcpServer {
            session_id: session_id.into(),
            server_name,
        })
    }

    pub fn clear_mcp_auth(&self, session_id: String, server_name: String) -> anyhow::Result<()> {
        self.send(Command::ClearMcpAuth {
            session_id: session_id.into(),
            server_name,
        })
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

    pub fn start_git_context_watch(&self, session_id: String, cwd: PathBuf) -> anyhow::Result<()> {
        self.send(Command::StartGitContextWatch {
            session_id: session_id.into(),
            cwd,
        })
    }

    pub fn stop_git_context_watch(&self, session_id: String) -> anyhow::Result<()> {
        self.send(Command::StopGitContextWatch {
            session_id: session_id.into(),
        })
    }
}

/// Agent factory — wraps [`ForgeSdkBridge`] behind a channel API.
pub struct Agent;

impl Agent {
    /// Construct a test stub: `AgentHandle` backed by a fresh
    /// ForgeSdkBridge that's never actually driven (no `new_session`
    /// call). Returns the handle plus a `Receiver<Command>` that
    /// drains every command the test exercises — replaces
    /// `RecordingBridge` from the bridge-collapse era.
    #[must_use]
    pub fn testing_stub() -> (AgentHandle, mpsc::UnboundedReceiver<Command>) {
        let bridge = ForgeSdkBridge::new();
        let agent_event_rx = bridge
            .take_events()
            .unwrap_or_else(|| mpsc::unbounded_channel().1);

        // Two command channels: one the AgentHandle pushes onto, one
        // the test drains. The dispatcher task is NOT spawned, so
        // commands accumulate in the receiver instead of being
        // forwarded to the (uninitialised) bridge — exactly what
        // tests want.
        let (commands_tx, commands_rx) = mpsc::unbounded_channel::<Command>();
        let (_events_tx, events_rx) = mpsc::unbounded_channel::<Event>();
        let (_passthrough_tx, passthrough_rx) =
            mpsc::unbounded_channel::<crate::client::AgentEvent>();
        // Drain the bridge's events into the void so the channel
        // doesn't fill up if a test happens to hit a path that emits.
        tokio::spawn(async move {
            let mut rx = agent_event_rx;
            while rx.recv().await.is_some() {}
        });

        let handle = AgentHandle {
            commands: commands_tx,
            events: Mutex::new(Some(events_rx)),
            agent_events: Mutex::new(Some(passthrough_rx)),
            bridge: Arc::new(bridge),
        };
        (handle, commands_rx)
    }

    /// Spawn a new agent runtime. Returns a handle holding the
    /// command sender + events receiver + direct-accessor passthroughs.
    #[must_use]
    pub fn spawn() -> AgentHandle {
        let bridge = ForgeSdkBridge::new();
        let agent_event_rx = bridge
            .take_events()
            .unwrap_or_else(|| mpsc::unbounded_channel().1);

        let (commands_tx, commands_rx) = mpsc::unbounded_channel::<Command>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<Event>();

        // Tee the bridge's AgentEvent stream: one branch goes to the
        // forward-looking Event translator (events_tx); the other
        // gets handed to call sites via take_events() so existing
        // forge-tui translators work unchanged in phase 5.
        let (passthrough_tx, passthrough_rx) =
            mpsc::unbounded_channel::<crate::client::AgentEvent>();
        let translator_tx = events_tx.clone();
        tokio::spawn(tee_events(agent_event_rx, passthrough_tx, translator_tx));

        // Command dispatcher task.
        let dispatch_bridge = Arc::new(bridge);
        tokio::spawn(dispatch_commands(commands_rx, Arc::clone(&dispatch_bridge)));

        AgentHandle {
            commands: commands_tx,
            events: Mutex::new(Some(events_rx)),
            agent_events: Mutex::new(Some(passthrough_rx)),
            bridge: dispatch_bridge,
        }
    }
}

/// Forward each `AgentEvent` to both the passthrough receiver
/// (consumed by forge-tui's existing translator) and the Event
/// translator (consumed by phase-6 call sites). One arrives, both
/// see it.
async fn tee_events(
    mut agent_event_rx: mpsc::UnboundedReceiver<crate::client::AgentEvent>,
    passthrough_tx: mpsc::UnboundedSender<crate::client::AgentEvent>,
    translator_tx: mpsc::UnboundedSender<Event>,
) {
    while let Some(event) = agent_event_rx.recv().await {
        let cloned = event.clone();
        let _ = passthrough_tx.send(event);
        if let Some(translated) = translate(cloned) {
            let _ = translator_tx.send(translated);
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

/// Convert an `AgentEvent` (forge-agent's internal shape) into a
/// `forge_primitives::Event`. Some AgentEvent payload fields are
/// forge-sdk types; we serde-encode them to `Value` so the wire-shape
/// `Event` doesn't reach into forge-sdk.
fn into_value<T: serde::Serialize>(v: T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

fn translate(event: crate::client::AgentEvent) -> Option<Event> {
    use crate::client::AgentEvent as A;
    use forge_primitives::SessionId;

    Some(match event {
        A::Connected {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
        } => Event::Connected {
            session_id: SessionId::new(session_id),
            cwd,
            current_model: into_value(current_model),
            available_models: available_models.into_iter().map(into_value).collect(),
            mode: mode.map(into_value),
            history_updates: history_updates.map(into_value),
        },
        A::AuthRequired {
            method_name,
            method_description,
        } => Event::AuthRequired {
            method_name,
            method_description,
        },
        A::ConnectionFailed { message } => Event::ConnectionFailed { message },
        A::SessionReplaced {
            session_id,
            cwd,
            current_model,
            available_models,
            mode,
            history_updates,
        } => Event::SessionReplaced {
            session_id: SessionId::new(session_id),
            cwd,
            current_model: into_value(current_model),
            available_models: available_models.into_iter().map(into_value).collect(),
            mode: mode.map(into_value),
            history_updates: history_updates.map(into_value),
        },
        A::SessionsListed { sessions } => Event::SessionsListed { sessions },
        A::SdkMessage { session_id, msg } => Event::SdkMessage {
            session_id: SessionId::new(session_id),
            msg: into_value(msg),
        },
        A::PermissionRequest {
            session_id,
            request,
        } => Event::PermissionRequest {
            session_id: SessionId::new(session_id),
            request: into_value(request),
        },
        A::QuestionRequest {
            session_id,
            request,
        } => Event::QuestionRequest {
            session_id: SessionId::new(session_id),
            request: into_value(request),
        },
        A::ElicitationRequest {
            session_id,
            request,
        } => Event::ElicitationRequest {
            session_id: SessionId::new(session_id),
            request: into_value(request),
        },
        A::ElicitationComplete {
            session_id,
            elicitation_id,
            server_name,
        } => Event::ElicitationComplete {
            session_id: SessionId::new(session_id),
            elicitation_id,
            server_name,
        },
        A::McpAuthRedirect {
            session_id,
            redirect,
        } => Event::McpAuthRedirect {
            session_id: SessionId::new(session_id),
            redirect: into_value(redirect),
        },
        A::McpOperationError { session_id, error } => Event::McpOperationError {
            session_id: SessionId::new(session_id),
            error: into_value(error),
        },
        A::McpSnapshot {
            session_id,
            servers,
            error,
        } => Event::McpSnapshot {
            session_id: SessionId::new(session_id),
            servers: into_value(servers),
            error,
        },
        A::SlashError {
            session_id,
            message,
        } => Event::SlashError {
            session_id: SessionId::new(session_id),
            message,
        },
        A::RuntimeReloadCompleted { session_id } => Event::RuntimeReloadCompleted {
            session_id: SessionId::new(session_id),
        },
        A::RuntimeReloadFailed {
            session_id,
            message,
        } => Event::RuntimeReloadFailed {
            session_id: SessionId::new(session_id),
            message,
        },
        A::StatusSnapshot {
            session_id,
            account,
        } => Event::StatusSnapshot {
            session_id: SessionId::new(session_id),
            account: into_value(account),
        },
        A::OauthCredentialsSnapshot {
            session_id,
            credentials,
        } => Event::OauthCredentialsSnapshot {
            session_id: SessionId::new(session_id),
            credentials: into_value(credentials),
        },
        A::GitContextSnapshot {
            session_id,
            context,
        } => Event::GitContextSnapshot {
            session_id: SessionId::new(session_id),
            context: into_value(context),
        },
        A::ContextUsage {
            session_id,
            percentage,
        } => Event::ContextUsage {
            session_id: SessionId::new(session_id),
            percentage,
        },
    })
}

/// Dispatch one `Command` to the matching `AgentBridge` method.
fn dispatch(cmd: Command, bridge: &ForgeSdkBridge) -> anyhow::Result<()> {
    use forge_primitives::Command as C;

    match cmd {
        C::NewSession {
            cwd,
            launch_settings,
        } => {
            let launch = serde_json::from_value(launch_settings)
                .unwrap_or_else(|_| crate::client::SessionLaunchSettings::default());
            bridge.new_session(cwd, launch)
        }
        C::ResumeSession {
            session_id,
            launch_settings,
        } => {
            let launch = serde_json::from_value(launch_settings)
                .unwrap_or_else(|_| crate::client::SessionLaunchSettings::default());
            bridge.resume_session(session_id.into_string(), launch)
        }
        C::Prompt { session_id, text } => bridge
            .prompt_text(session_id.into_string(), text)
            .map(|_| ()),
        C::PromptWithImages {
            session_id,
            text,
            images,
        } => bridge
            .prompt_with_images(session_id.into_string(), text, images)
            .map(|_| ()),
        C::Cancel { session_id } => bridge.cancel(session_id.into_string()),
        C::SetMode { session_id, mode } => bridge.set_mode(session_id.into_string(), mode),
        C::SetModel { session_id, model } => bridge.set_model(session_id.into_string(), model),
        C::GenerateSessionTitle {
            session_id,
            description,
        } => bridge.generate_session_title(session_id.into_string(), description),
        C::RenameSession { session_id, title } => {
            bridge.rename_session(session_id.into_string(), title)
        }
        C::RewindFiles { .. } => {
            // Bridge doesn't expose rewind_files directly through trait;
            // phase 5 may surface it. Skip for now.
            Ok(())
        }
        C::GetStatusSnapshot { session_id } => bridge.get_status_snapshot(session_id.into_string()),
        C::GetOauthCredentialsSnapshot { session_id } => {
            bridge.get_oauth_credentials_snapshot(session_id.into_string())
        }
        C::GetContextUsage { session_id } => bridge.get_context_usage(session_id.into_string()),
        C::GetMcpSnapshot { session_id } => bridge.get_mcp_snapshot(session_id.into_string()),
        C::PermissionResponse {
            session_id,
            tool_call_id,
            outcome,
        } => bridge.permission_response(
            session_id.into_string(),
            tool_call_id.into_string(),
            outcome,
        ),
        C::QuestionResponse {
            session_id,
            tool_call_id,
            outcome,
        } => bridge.question_response(
            session_id.into_string(),
            tool_call_id.into_string(),
            outcome,
        ),
        C::RespondToElicitation {
            session_id,
            elicitation_request_id,
            action,
            content,
        } => bridge.respond_to_elicitation(
            session_id.into_string(),
            elicitation_request_id,
            action,
            content,
        ),
        C::ReconnectMcpServer {
            session_id,
            server_name,
        } => bridge.reconnect_mcp_server(session_id.into_string(), server_name),
        C::ToggleMcpServer {
            session_id,
            server_name,
            enabled,
        } => bridge.toggle_mcp_server(session_id.into_string(), server_name, enabled),
        C::SetMcpServers {
            session_id,
            servers,
        } => bridge.set_mcp_servers(session_id.into_string(), servers),
        C::AuthenticateMcpServer {
            session_id,
            server_name,
        } => bridge.authenticate_mcp_server(session_id.into_string(), server_name),
        C::ClearMcpAuth {
            session_id,
            server_name,
        } => bridge.clear_mcp_auth(session_id.into_string(), server_name),
        C::SubmitMcpOauthCallbackUrl {
            session_id,
            server_name,
            callback_url,
        } => bridge.submit_mcp_oauth_callback_url(
            session_id.into_string(),
            server_name,
            callback_url,
        ),
        C::ReloadPlugins { session_id } => bridge.reload_plugins(session_id.into_string()),
        C::StartGitContextWatch { session_id, cwd } => {
            bridge.start_git_context_watch(session_id.into_string(), cwd)
        }
        C::StopGitContextWatch { session_id } => {
            bridge.stop_git_context_watch(session_id.into_string())
        }
        // `Command` is `#[non_exhaustive]`; a wildcard arm is required
        // by the compiler. Future variants log + drop until phase 5
        // updates the dispatcher.
        _ => {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                "agent dispatcher: unhandled Command variant",
            );
            Ok(())
        }
    }
}
