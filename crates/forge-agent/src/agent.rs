//! Channel-based [`Agent`] handle - the public consumer surface.
//!
//! `Agent::spawn` constructs a `ForgeSdkBridge` under the hood and
//! spawns one background task: the **command dispatcher**, which
//! drains `mpsc::UnboundedReceiver<AgentCommand>` and calls the matching
//! inherent method on the bridge. The bridge's `AgentEvent`
//! receiver is handed back to consumers via `take_events()`.
//!
//! Direct-return accessors (config_dir, settings_documents, etc.)
//! live on [`AgentHandle`] as method passthroughs to the bridge.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_primitives::{AgentCommand, PermissionMode};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::forge_sdk_bridge::ForgeSdkBridge;

/// Handle returned by [`Agent::spawn`]. Owns the channels + a thin
/// passthrough to the bridge's direct-return accessors.
pub struct AgentHandle {
    /// UI → agent commands. Used internally by the inherent
    /// command-shorthand methods (`prompt_text`, `cancel`, etc.). Public
    /// so callers can `send(AgentCommand::...)` directly when richer flows
    /// are needed.
    pub commands: mpsc::UnboundedSender<AgentCommand>,
    /// Bridge's raw `AgentEvent` receiver. Single-take via
    /// [`AgentHandle::take_events`] - the workspace's session task
    /// consumes this and translates it into `SessionUpdate`s.
    agent_events: Mutex<Option<mpsc::UnboundedReceiver<crate::client::AgentEvent>>>,
    /// Bridge handle used for direct-return accessors (config_dir,
    /// settings_documents, …) plus internal command dispatch.
    bridge: Arc<ForgeSdkBridge>,
}

impl AgentHandle {
    /// Take ownership of the bridge's `AgentEvent` receiver. Returns
    /// `Some` exactly once.
    pub fn take_events(&self) -> Option<mpsc::UnboundedReceiver<crate::client::AgentEvent>> {
        self.agent_events.lock().take()
    }

    // Direct-accessor passthroughs to the bridge. Each method
    // simply forwards arguments - no transform.

    pub fn config_dir(&self) -> PathBuf {
        self.bridge.config_dir()
    }

    /// OS PID of the bound `claude` child, when one is currently
    /// attached. Surfaces to forge-workspace for the Inspector
    /// pane's PROCESSES OS-walk.
    pub fn claude_pid(&self) -> Option<u32> {
        self.bridge.claude_pid()
    }

    pub fn project_memory_path(&self, cwd: &Path) -> PathBuf {
        self.bridge.project_memory_path(cwd)
    }

    pub fn settings_documents(
        &self,
        cwd: Option<&Path>,
    ) -> crate::userdata::settings::SettingsDocuments {
        self.bridge.settings_documents(cwd)
    }

    /// Returns a clone of the bridge's bound forge-account
    /// `display_name` (when forge-workspace picked one). Used by
    /// the connect-flow to emit a `ForgeAccountIdentityReady`
    /// event right after spawn - eliminates the welcome-message
    /// flicker that would otherwise wait for the slow status
    /// snapshot to arrive from the CLI.
    pub fn display_name(&self) -> Option<String> {
        self.bridge.display_name()
    }

    /// The bridge's resolved forge.toml env. Test-only, and one hop
    /// short of what the child receives - the subprocess also gets
    /// `CLAUDE_CONFIG_DIR` and the SDK version. It exists so a test
    /// can observe the map a spawn carries rather than what the
    /// resolution helper returned.
    #[cfg(any(test, feature = "testing"))]
    pub fn env(&self) -> std::collections::HashMap<String, String> {
        self.bridge.env()
    }

    /// Disconnect the bound `claude` subprocess: take the bridge's
    /// client slot and run the SDK's graceful shutdown (signal the
    /// reader task, drain, close the child). Both layers are
    /// idempotent via `Option::take`, so repeated calls on clones are
    /// no-ops. Called from `SessionTask`'s exit path so release /
    /// despawn / teardown paths actually kill the subprocess instead
    /// of leaving it alive until forge exits.
    pub async fn disconnect(&self) {
        if let Some(client) = self.bridge.clear_client()
            && let Err(err) = client.disconnect().await
        {
            tracing::warn!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                error = %err,
                "client disconnect failed during session teardown",
            );
        }
    }

    // ---- Fire-and-forget AgentCommand shorthands ----
    //
    // Each method builds the matching `AgentCommand` variant and pushes it
    // onto `commands`. Returns `Err` only if the dispatcher task has
    // shut down (channel closed) or `launch_settings` serialisation
    // fails. Errors from the underlying forge_sdk::Client surface
    // asynchronously via the events stream.

    fn send(&self, cmd: AgentCommand) -> Result<(), AgentError> {
        self.commands.send(cmd).map_err(|_| AgentError::DispatcherShutDown)
    }

    /// Start a session under `session_id`, or under an id the CLI picks
    /// when that is `None`.
    pub fn new_session(
        &self,
        session_id: Option<String>,
        cwd: String,
        launch_settings: crate::client::SessionLaunchSettings,
    ) -> Result<(), AgentError> {
        // Propagate serialise failure instead of launching with `Value::Null`:
        // the bridge REFUSES a value it cannot decode rather than
        // deserialising it to default settings, and the refusal is logged and
        // emitted as a `ConnectionFailed`, so a lossy payload stops the spawn
        // with a visible failure instead of dropping the configured model or
        // the blocked-tools denial silently.
        let launch_settings = serde_json::to_value(launch_settings)?;
        self.send(AgentCommand::NewSession { session_id, cwd, launch_settings })
    }

    pub fn resume_session(
        &self,
        session_id: String,
        cwd: String,
        launch_settings: crate::client::SessionLaunchSettings,
    ) -> Result<(), AgentError> {
        let launch_settings = serde_json::to_value(launch_settings)?;
        self.send(AgentCommand::ResumeSession {
            session_id: session_id.into(),
            cwd,
            launch_settings,
        })
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
    ) -> Result<(), AgentError> {
        let launch_settings = serde_json::to_value(launch_settings)?;
        self.send(AgentCommand::ResumeOrNewSession {
            session_id: session_id.into(),
            cwd,
            launch_settings,
        })
    }

    pub fn prompt_text(
        &self,
        session_id: String,
        text: String,
        uuid: String,
    ) -> Result<(), AgentError> {
        self.send(AgentCommand::Prompt { session_id: session_id.into(), text, uuid })
    }

    pub fn prompt_with_images(
        &self,
        session_id: String,
        text: String,
        images: Vec<forge_primitives::ImageAttachment>,
        uuid: String,
    ) -> Result<(), AgentError> {
        self.send(AgentCommand::PromptWithImages {
            session_id: session_id.into(),
            text,
            images,
            uuid,
        })
    }

    pub fn cancel(&self, session_id: String) -> Result<(), AgentError> {
        self.send(AgentCommand::Cancel { session_id: session_id.into() })
    }

    pub fn cancel_queued(&self, session_id: String, uuid: String) -> Result<(), AgentError> {
        self.send(AgentCommand::CancelQueuedPrompt { session_id: session_id.into(), uuid })
    }

    pub fn set_mode(&self, session_id: String, mode: PermissionMode) -> Result<(), AgentError> {
        self.send(AgentCommand::SetMode { session_id: session_id.into(), mode })
    }

    pub fn set_model(&self, session_id: String, model: String) -> Result<(), AgentError> {
        self.send(AgentCommand::SetModel { session_id: session_id.into(), model })
    }

    pub fn get_context_usage(&self, session_id: String) -> Result<(), AgentError> {
        self.send(AgentCommand::GetContextUsage { session_id: session_id.into() })
    }

    pub fn get_mcp_snapshot(&self, session_id: String) -> Result<(), AgentError> {
        self.send(AgentCommand::GetMcpSnapshot { session_id: session_id.into() })
    }

    pub fn permission_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: forge_primitives::PermissionOutcome,
    ) -> Result<(), AgentError> {
        self.send(AgentCommand::PermissionResponse {
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
    ) -> Result<(), AgentError> {
        self.send(AgentCommand::QuestionResponse {
            session_id: session_id.into(),
            tool_call_id: tool_call_id.into(),
            outcome,
        })
    }
}

/// Errors surfaced from `AgentHandle`'s fire-and-forget command
/// methods. The underlying `claude` subprocess work flows back
/// asynchronously via the events stream - these are dispatch-time
/// failures only.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The command dispatcher task has shut down; the channel is
    /// closed and no further commands will be processed.
    #[error("agent dispatcher shut down")]
    DispatcherShutDown,
    /// Serialising `SessionLaunchSettings` to JSON failed before the
    /// command could be dispatched.
    #[error("failed to encode launch settings: {0}")]
    EncodeFailed(#[from] serde_json::Error),
    /// The command needs a session id but the bridge hasn't emitted
    /// its first `Connected` yet, so it was dropped.
    #[error("no active session for {command}")]
    NoSession { command: &'static str },
}

/// Agent factory - wraps a private `ForgeSdkBridge` behind a channel API.
pub struct Agent;

impl Agent {
    /// Construct a test stub: `AgentHandle` backed by a fresh
    /// ForgeSdkBridge that's never actually driven (no `new_session`
    /// call). Returns the handle plus a `Receiver<AgentCommand>` that
    /// drains every command the test exercises.
    ///
    /// Safe to call outside a Tokio runtime - no tasks are spawned.
    /// The bridge's events stream is dropped on the floor; tests that
    /// don't drive sessions never see events anyway. The bridge is
    /// bound to a synthetic `/tmp/forge-testing-stub` config_dir;
    /// since no session is driven, no I/O hits this path.
    #[cfg(any(test, feature = "testing"))]
    pub fn testing_stub() -> (AgentHandle, mpsc::UnboundedReceiver<AgentCommand>) {
        let bridge = ForgeSdkBridge::default();
        // Drop the bridge's events receiver immediately - tests don't
        // run a real session so nothing is producing.
        let _ = bridge.take_events();

        let (commands_tx, commands_rx) = mpsc::unbounded_channel::<AgentCommand>();
        // Hand a fresh empty channel as the agent_events receiver so
        // the AgentHandle shape matches production. The `_dead_tx`
        // drops at end of scope; `take_events()` returns the
        // receiver and its first `recv()` then returns `None`.
        let (_dead_tx, dead_rx) = mpsc::unbounded_channel::<crate::client::AgentEvent>();

        let handle = AgentHandle {
            commands: commands_tx,
            agent_events: Mutex::new(Some(dead_rx)),
            bridge: Arc::new(bridge),
        };
        (handle, commands_rx)
    }

    /// Spawn a new agent runtime bound to `config_dir` with an
    /// optional forge-account `display_name`. Both are stored on the
    /// bridge as typed fields. `config_dir` is consulted by every
    /// in-process accessor (oauth, settings, catalog scans) and
    /// exported to the spawned `claude` subprocess as
    /// `CLAUDE_CONFIG_DIR`. `display_name`, when set, is read back
    /// through the handle's `display_name()` so a view
    /// renders which forge-account the bridge is bound to. `env`
    /// carries the session's resolved forge.toml env - `[env]`, then
    /// `[accounts.env]`, then the spawning project's `[[orgs.projects]]`
    /// `env` - stamped onto the spawned subprocess
    /// alongside `CLAUDE_CONFIG_DIR`. Returns a
    /// handle holding the command sender + events receiver + direct-
    /// accessor passthroughs.
    pub fn spawn(
        config_dir: PathBuf,
        display_name: Option<String>,
        extra_mcp_servers: Vec<(String, forge_sdk::mcp::McpServer)>,
        env: HashMap<String, String>,
    ) -> AgentHandle {
        let bridge = ForgeSdkBridge::new(config_dir, display_name, extra_mcp_servers, env);
        let agent_event_rx = bridge.take_events().unwrap_or_else(|| mpsc::unbounded_channel().1);

        let (commands_tx, commands_rx) = mpsc::unbounded_channel::<AgentCommand>();

        // AgentCommand dispatcher task.
        let dispatch_bridge = Arc::new(bridge);
        let span = tracing::info_span!(
            "agent_dispatch",
            config_dir = %dispatch_bridge.config_dir().display(),
        );
        tokio::spawn(dispatch_commands(commands_rx, Arc::clone(&dispatch_bridge)).instrument(span));

        AgentHandle {
            commands: commands_tx,
            agent_events: Mutex::new(Some(agent_event_rx)),
            bridge: dispatch_bridge,
        }
    }
}

async fn dispatch_commands(
    mut commands_rx: mpsc::UnboundedReceiver<AgentCommand>,
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

/// Dispatch one `AgentCommand` to the matching `ForgeSdkBridge` method.
/// Internal - kept on `anyhow` because the bridge surface itself
/// returns `anyhow::Result` and the dispatcher task only logs the
/// error before continuing. The public AgentHandle surface above
/// uses the typed `AgentError`.
fn dispatch(cmd: AgentCommand, bridge: &ForgeSdkBridge) -> anyhow::Result<()> {
    use forge_primitives::AgentCommand as C;

    match cmd {
        C::NewSession { session_id, cwd, launch_settings } => {
            bridge.new_session(session_id, cwd, launch_settings)
        }
        C::ResumeSession { session_id, cwd, launch_settings } => {
            bridge.resume_session(session_id.into_string(), cwd, launch_settings)
        }
        C::ResumeOrNewSession { session_id, cwd, launch_settings } => {
            bridge.resume_or_new_session(session_id.into_string(), cwd, launch_settings)
        }
        C::Prompt { session_id, text, uuid } => {
            bridge.prompt_text(session_id.into_string(), text, uuid)
        }
        C::PromptWithImages { session_id, text, images, uuid } => {
            bridge.prompt_with_images(session_id.into_string(), text, images, uuid)
        }
        C::Cancel { session_id } => bridge.cancel(session_id.into_string()),
        C::CancelQueuedPrompt { session_id, uuid } => {
            bridge.cancel_queued(session_id.into_string(), uuid)
        }
        C::SetMode { session_id, mode } => bridge.set_mode(session_id.into_string(), mode),
        C::SetModel { session_id, model } => bridge.set_model(session_id.into_string(), model),
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
    }
}

/// Decode launch settings off the dispatcher channel. Symmetric to the
/// encode-side `?`-propagation in `AgentHandle::new_session`.
///
/// **A failure refuses the spawn** (Ved, 2026-10-15). The settings carry the
/// blocked-tools denial (`disallowedTools`) among the rest, so a spawn on
/// defaults would start a session WIDER than the one that was configured -
/// silently unblocking tools. The decode runs in the BRIDGE, where a refusal
/// goes on the same event every other spawn failure does (`ConnectionFailed`),
/// so the seat records the failure and rolls its row instead of sitting at
/// Spawning with nothing to say.
pub(crate) fn decode_launch_settings(
    value: serde_json::Value,
) -> anyhow::Result<crate::client::SessionLaunchSettings> {
    serde_json::from_value(value).map_err(|e| {
        anyhow::anyhow!(
            "the launch settings could not be decoded, so the spawn is refused rather than \
             started with none of them (the blocked-tools denial rides them): {e}"
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod decode_launch_settings_tests {
    use super::decode_launch_settings;

    #[test]
    fn decodes_the_stamped_settings_intact() {
        let value = serde_json::json!({
            "settings": { "model": "haiku" },
            "agent_progress_summaries": true,
            "charter": "worker charter",
            "extra_args": [["disallowedTools", "Bash"]],
        });
        let decoded = decode_launch_settings(value).expect("the stamped settings decode");
        assert_eq!(
            decoded.settings.as_ref().and_then(|s| s.get("model")).and_then(|v| v.as_str()),
            Some("haiku"),
            "the stamped settings survive the decode"
        );
        assert_eq!(decoded.agent_progress_summaries, Some(true));
        assert_eq!(decoded.charter.as_deref(), Some("worker charter"));
        assert_eq!(
            decoded.extra_args,
            vec![("disallowedTools".to_owned(), Some("Bash".to_owned()))],
            "the blocked-tools denial's carrier survives the decode"
        );
    }

    #[test]
    fn the_dispatch_refuses_rather_than_defaulting() {
        // **The property #1183 is about, pinned at the SEAM.** A defaulting
        // arm (`decode_launch_settings(..).unwrap_or_default()`) compiles and
        // passes every other test while every spawn launches on defaults with
        // the denial dropped - so the refusal is asserted where it is used,
        // not only inside the decoder (Ved refused the spawn, 2026-10-15).
        let command = forge_primitives::AgentCommand::NewSession {
            session_id: Some("a-test-seat".to_owned()),
            cwd: "/tmp".to_owned(),
            launch_settings: serde_json::json!(42),
        };
        let bridge = crate::forge_sdk_bridge::ForgeSdkBridge::default();
        let mut events = bridge.take_events().expect("a fresh bridge yields its events receiver");
        let error = super::dispatch(command, &bridge)
            .expect_err("an undecodable payload refuses the dispatch rather than defaulting");
        assert!(
            error.to_string().contains("launch settings could not be decoded"),
            "and the refusal is the decoder's own, not whatever the bridge would say: {error}"
        );
        // **And it goes where every spawn failure goes.** The event is what
        // the seat's task records the failure by and rolls the row on, so a
        // refused spawn is visible and retryable, not a seat stuck Spawning.
        match events.try_recv() {
            Ok(crate::AgentEvent::ConnectionFailed { message, .. }) => assert!(
                message.contains("refused"),
                "the event says the spawn was refused: {message}"
            ),
            other => panic!("expected a ConnectionFailed for the refused spawn, got {other:?}"),
        }
    }

    #[test]
    fn refuses_the_spawn_on_an_undecodable_value() {
        // The settings carry the blocked-tools denial, so a decode failure
        // must not start a session on defaults: it refuses, naming the
        // denial's carrier (Ved, 2026-10-15).
        let error = decode_launch_settings(serde_json::json!(42))
            .expect_err("an undecodable value refuses rather than defaults");
        assert!(
            error.to_string().contains("blocked-tools denial"),
            "the refusal names what the settings carry: {error}"
        );
    }
}
