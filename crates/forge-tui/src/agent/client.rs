//! Adapter between upstream's `BridgeCommand` / `BridgeEvent` and
//! forge-daemon's JSON-RPC. Replaces upstream's Node.js-subprocess
//! `BridgeClient` with a WebSocket client to forge-daemon.
//!
//! Outbound: every `BridgeCommand` variant maps to a daemon JSON-RPC
//! method call (or reverse-RPC reply) inside the writer task.
//!
//! Inbound: a reader task drains `DaemonConnection`'s raw inbound
//! channel, runs each `InboundEvent` through
//! [`crate::agent::translate::translate`], and ships any synthesised
//! `EventEnvelope` to the TUI via `event_rx`. Reverse-RPC requests
//! (`permission.request`, `session.question_request`) populate the
//! shared `reverse_lookup` so the writer's matching response branch
//! can find the original JSON-RPC id.

#![allow(
    clippy::missing_errors_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::match_same_arms
)]

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::bridge::{DaemonConnection, InboundEvent, resolve_daemon_url};
use crate::agent::translate::{
    ReverseLookup, decode_context_usage, decode_spawn_response, decode_status_snapshot, translate,
};
use crate::agent::wire::{BridgeCommand, CommandEnvelope, EventEnvelope};

/// Forge-daemon-backed bridge client. Owns the connection + the
/// reader and writer translator tasks.
pub struct BridgeClient {
    /// Outbound channel — `AgentConnection` pushes `CommandEnvelope`s
    /// here; the writer task drains and translates to JSON-RPC.
    pub command_tx: mpsc::UnboundedSender<CommandEnvelope>,
    /// Inbound `EventEnvelope`s ready for the TUI's event handler.
    /// The reader task fills this from translated daemon
    /// notifications and reverse-RPC requests.
    pub event_rx: mpsc::UnboundedReceiver<EventEnvelope>,
    /// Map keyed by `tool_call_id` / `tool_use_id` /
    /// `elicitation_request_id` → JSON-RPC request id of the
    /// reverse-RPC. The reader populates it when surfacing a
    /// request; the writer's `*Response` branch consumes it.
    pub reverse_lookup: ReverseLookup,
}

impl BridgeClient {
    /// Connect to forge-daemon (URL from `FORGE_DAEMON_URL` env var
    /// or default `ws://127.0.0.1:7373/`) and spawn the translator
    /// tasks. Returns once the WS handshake completes.
    pub async fn connect() -> Result<Self> {
        let url = resolve_daemon_url();
        Self::connect_to(&url).await
    }

    /// Connect to a specific daemon URL. Useful for tests.
    pub async fn connect_to(url: &str) -> Result<Self> {
        let (conn, daemon_events_rx) = DaemonConnection::connect(url).await?;
        let conn = Arc::new(conn);
        let reverse_lookup: ReverseLookup =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

        let (command_tx, command_rx) = mpsc::unbounded_channel::<CommandEnvelope>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<EventEnvelope>();

        tokio::spawn(writer_loop(
            Arc::clone(&conn),
            command_rx,
            Arc::clone(&reverse_lookup),
            event_tx.clone(),
        ));
        tokio::spawn(reader_loop(
            daemon_events_rx,
            event_tx,
            Arc::clone(&reverse_lookup),
        ));

        Ok(Self {
            command_tx,
            event_rx,
            reverse_lookup,
        })
    }
}

/// Inbound translator: drains raw `InboundEvent`s from the daemon
/// connection, runs each through [`translate`], and ships any
/// synthesised `EventEnvelope` to the TUI.
async fn reader_loop(
    mut rx: mpsc::UnboundedReceiver<InboundEvent>,
    event_tx: mpsc::UnboundedSender<EventEnvelope>,
    reverse_lookup: ReverseLookup,
) {
    while let Some(event) = rx.recv().await {
        for envelope in translate(event, &reverse_lookup) {
            if event_tx.send(envelope).is_err() {
                return;
            }
        }
    }
}

/// Public TUI-facing handle. Same shape as upstream's
/// `AgentConnection`; the writer routes through forge-daemon's
/// JSON-RPC instead of stdin to a Node subprocess.
pub struct AgentConnection {
    command_tx: mpsc::UnboundedSender<CommandEnvelope>,
}

impl AgentConnection {
    /// Construct a connection from an outbound mpsc — typically
    /// `BridgeClient::command_tx` cloned for the TUI.
    #[must_use]
    pub fn new(command_tx: mpsc::UnboundedSender<CommandEnvelope>) -> Self {
        Self { command_tx }
    }

    fn send(&self, command: BridgeCommand) -> Result<()> {
        self.command_tx
            .send(CommandEnvelope {
                request_id: None,
                command,
            })
            .map_err(|_| anyhow::anyhow!("forge-daemon writer closed"))
    }

    /// Plain-text user prompt.
    pub fn prompt_text(&self, session_id: String, text: String) -> Result<()> {
        self.send(BridgeCommand::Prompt {
            session_id,
            chunks: vec![crate::agent::types::PromptChunk {
                kind: "text".into(),
                value: serde_json::json!({ "text": text }),
            }],
        })
    }

    /// Multi-modal prompt (text + image chunks).
    pub fn prompt_with_chunks(
        &self,
        session_id: String,
        chunks: Vec<crate::agent::types::PromptChunk>,
    ) -> Result<()> {
        self.send(BridgeCommand::Prompt { session_id, chunks })
    }

    /// Cancel the current turn on a session.
    pub fn cancel(&self, session_id: String) -> Result<()> {
        self.send(BridgeCommand::CancelTurn { session_id })
    }

    /// Switch the session's mode.
    pub fn set_mode(&self, session_id: String, mode: String) -> Result<()> {
        self.send(BridgeCommand::SetMode { session_id, mode })
    }

    /// Switch the session's model.
    pub fn set_model(&self, session_id: String, model: String) -> Result<()> {
        self.send(BridgeCommand::SetModel { session_id, model })
    }

    /// Ask the daemon to generate a session title from a description.
    pub fn generate_session_title(&self, session_id: String, description: String) -> Result<()> {
        self.send(BridgeCommand::GenerateSessionTitle {
            session_id,
            description,
        })
    }

    /// Rename a session.
    pub fn rename_session(&self, session_id: String, title: String) -> Result<()> {
        self.send(BridgeCommand::RenameSession { session_id, title })
    }

    /// Request the daemon emit a status snapshot for a session.
    pub fn get_status_snapshot(&self, session_id: String) -> Result<()> {
        self.send(BridgeCommand::GetStatusSnapshot { session_id })
    }

    /// Request the daemon emit a context-usage snapshot.
    pub fn get_context_usage(&self, session_id: String) -> Result<()> {
        self.send(BridgeCommand::GetContextUsage { session_id })
    }

    /// Reload the session's plugin inventory.
    pub fn reload_plugins(&self, session_id: String) -> Result<()> {
        self.send(BridgeCommand::ReloadPlugins { session_id })
    }

    /// Request the daemon emit an MCP server snapshot.
    pub fn get_mcp_snapshot(&self, session_id: String) -> Result<()> {
        self.send(BridgeCommand::GetMcpSnapshot { session_id })
    }

    /// Reconnect a named MCP server.
    pub fn reconnect_mcp_server(&self, session_id: String, server_name: String) -> Result<()> {
        self.send(BridgeCommand::McpReconnect {
            session_id,
            server_name,
        })
    }

    /// Toggle a named MCP server on/off.
    pub fn toggle_mcp_server(
        &self,
        session_id: String,
        server_name: String,
        enabled: bool,
    ) -> Result<()> {
        self.send(BridgeCommand::McpToggle {
            session_id,
            server_name,
            enabled,
        })
    }

    /// Replace the session's MCP server set.
    pub fn set_mcp_servers(
        &self,
        session_id: String,
        servers: std::collections::BTreeMap<String, crate::agent::types::McpServerConfig>,
    ) -> Result<()> {
        self.send(BridgeCommand::McpSetServers {
            session_id,
            servers,
        })
    }

    /// Begin OAuth for an MCP server.
    pub fn authenticate_mcp_server(&self, session_id: String, server_name: String) -> Result<()> {
        self.send(BridgeCommand::McpAuthenticate {
            session_id,
            server_name,
        })
    }

    /// Drop stored OAuth credentials for an MCP server.
    pub fn clear_mcp_auth(&self, session_id: String, server_name: String) -> Result<()> {
        self.send(BridgeCommand::McpClearAuth {
            session_id,
            server_name,
        })
    }

    /// Forward an OAuth callback URL to complete MCP authentication.
    pub fn submit_mcp_oauth_callback_url(
        &self,
        session_id: String,
        server_name: String,
        callback_url: String,
    ) -> Result<()> {
        self.send(BridgeCommand::McpOauthCallbackUrl {
            session_id,
            server_name,
            callback_url,
        })
    }

    /// Create a fresh session in the daemon.
    pub fn new_session(
        &self,
        cwd: String,
        launch_settings: crate::agent::wire::SessionLaunchSettings,
    ) -> Result<()> {
        self.send(BridgeCommand::NewSession {
            cwd,
            launch_settings,
        })
    }

    /// Resume a previously-recorded session.
    pub fn resume_session(
        &self,
        session_id: String,
        launch_settings: crate::agent::wire::SessionLaunchSettings,
    ) -> Result<()> {
        self.send(BridgeCommand::ResumeSession {
            session_id,
            launch_settings,
            metadata: std::collections::BTreeMap::new(),
        })
    }

    /// Reply to a permission request.
    pub fn permission_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: crate::agent::types::PermissionOutcome,
    ) -> Result<()> {
        self.send(BridgeCommand::PermissionResponse {
            session_id,
            tool_call_id,
            outcome,
        })
    }

    /// Reply to an `AskUserQuestion` request.
    pub fn question_response(
        &self,
        session_id: String,
        tool_call_id: String,
        outcome: crate::agent::types::QuestionOutcome,
    ) -> Result<()> {
        self.send(BridgeCommand::QuestionResponse {
            session_id,
            tool_call_id,
            outcome,
        })
    }

    /// Reply to an MCP elicitation request.
    pub fn elicitation_response(
        &self,
        session_id: String,
        elicitation_request_id: String,
        action: crate::agent::types::ElicitationAction,
        content: Option<Value>,
    ) -> Result<()> {
        self.send(BridgeCommand::ElicitationResponse {
            session_id,
            elicitation_request_id,
            action,
            content,
        })
    }

    /// Initiate adapter shutdown.
    pub fn shutdown(&self) -> Result<()> {
        self.send(BridgeCommand::Shutdown)
    }
}

/// Outbound translator: drains the TUI's `command_tx` channel and
/// converts each `CommandEnvelope` into the matching daemon JSON-RPC
/// call (or reverse-RPC reply). When a call's response carries data
/// the TUI needs as a `BridgeEvent` (e.g. `session.spawn` -> Connected),
/// the dispatcher synthesises the envelope and sends it via `event_tx`.
async fn writer_loop(
    conn: Arc<DaemonConnection>,
    mut rx: mpsc::UnboundedReceiver<CommandEnvelope>,
    reverse_lookup: ReverseLookup,
    event_tx: mpsc::UnboundedSender<EventEnvelope>,
) {
    while let Some(envelope) = rx.recv().await {
        let request_id = envelope.request_id;
        if let Err(err) = dispatch_command(
            &conn,
            &reverse_lookup,
            &event_tx,
            request_id,
            envelope.command,
        )
        .await
        {
            tracing::warn!(error = %err, "agent writer: dispatch failed");
        }
    }
}

async fn dispatch_command(
    conn: &DaemonConnection,
    reverse_lookup: &ReverseLookup,
    event_tx: &mpsc::UnboundedSender<EventEnvelope>,
    request_id: Option<String>,
    command: BridgeCommand,
) -> Result<()> {
    use BridgeCommand as C;
    match command {
        C::Initialize { .. } => Ok(()),
        C::CreateSession {
            cwd,
            resume,
            launch_settings: _,
            metadata: _,
        } => spawn_session(conn, event_tx, request_id, &cwd, resume.as_deref()).await,
        C::NewSession {
            cwd,
            launch_settings: _,
        } => spawn_session(conn, event_tx, request_id, &cwd, None).await,
        C::ResumeSession {
            session_id,
            launch_settings: _,
            metadata: _,
        } => spawn_session(conn, event_tx, request_id, "", Some(&session_id)).await,
        C::Prompt { session_id, chunks } => {
            if chunks.iter().all(|c| c.kind == "text") {
                let prompt: String = chunks
                    .iter()
                    .filter_map(|c| c.value.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                conn.call(
                    "session.send_user_message",
                    serde_json::json!({"session_id": session_id, "prompt": prompt}),
                )
                .await?;
            } else {
                let content: Vec<Value> = chunks
                    .into_iter()
                    .map(|c| match c.kind.as_str() {
                        "text" => serde_json::json!({
                            "type": "text",
                            "text": c.value.get("text").and_then(Value::as_str).unwrap_or(""),
                        }),
                        "image" => serde_json::json!({
                            "type": "image",
                            "source": c.value.get("source").cloned().unwrap_or(Value::Null),
                        }),
                        _ => c.value,
                    })
                    .collect();
                conn.call(
                    "session.send_user_message_blocks",
                    serde_json::json!({"session_id": session_id, "content": content}),
                )
                .await?;
            }
            Ok(())
        }
        C::CancelTurn { session_id } => {
            conn.call(
                "session.interrupt",
                serde_json::json!({"session_id": session_id}),
            )
            .await?;
            Ok(())
        }
        C::SetModel { session_id, model } => {
            conn.call(
                "session.set_model",
                serde_json::json!({"session_id": session_id, "model": model}),
            )
            .await?;
            Ok(())
        }
        C::SetMode { session_id, mode } => {
            conn.call(
                "session.set_permission_mode",
                serde_json::json!({"session_id": session_id, "mode": mode}),
            )
            .await?;
            Ok(())
        }
        C::GenerateSessionTitle {
            session_id,
            description,
        } => {
            conn.call(
                "session.generate_session_title",
                serde_json::json!({"session_id": session_id, "description": description}),
            )
            .await?;
            Ok(())
        }
        C::RenameSession { session_id, title } => {
            conn.call(
                "sessions.rename",
                serde_json::json!({"session_id": session_id, "title": title}),
            )
            .await?;
            Ok(())
        }
        C::PermissionResponse {
            tool_call_id,
            outcome,
            ..
        } => {
            let id = reverse_lookup
                .lock()
                .remove(&tool_call_id)
                .ok_or_else(|| anyhow::anyhow!("permission response: unknown tool_call_id"))?;
            conn.reply(id, serde_json::to_value(outcome)?)?;
            Ok(())
        }
        C::QuestionResponse {
            tool_call_id,
            outcome,
            ..
        } => {
            let id = reverse_lookup
                .lock()
                .remove(&tool_call_id)
                .ok_or_else(|| anyhow::anyhow!("question response: unknown tool_call_id"))?;
            let payload = match outcome {
                crate::agent::types::QuestionOutcome::Answered {
                    selected_option_ids,
                    annotation: _,
                } => {
                    let mut answers = serde_json::Map::new();
                    for (i, opt) in selected_option_ids.into_iter().enumerate() {
                        answers.insert(format!("q{i}"), Value::String(opt));
                    }
                    serde_json::json!({"outcome": "answered", "answers": answers})
                }
                crate::agent::types::QuestionOutcome::Cancelled => {
                    serde_json::json!({"outcome": "cancelled"})
                }
            };
            conn.reply(id, payload)?;
            Ok(())
        }
        C::ElicitationResponse {
            elicitation_request_id,
            action,
            content,
            ..
        } => {
            let id = reverse_lookup
                .lock()
                .remove(&elicitation_request_id)
                .ok_or_else(|| anyhow::anyhow!("elicitation response: unknown id"))?;
            conn.reply(
                id,
                serde_json::json!({"action": action, "content": content}),
            )?;
            Ok(())
        }
        C::GetStatusSnapshot { session_id } => {
            let result = conn
                .call(
                    "session.status_snapshot",
                    serde_json::json!({"session_id": session_id}),
                )
                .await?;
            if let Some(envelope) = decode_status_snapshot(&result, &session_id, request_id) {
                let _ = event_tx.send(envelope);
            }
            Ok(())
        }
        C::GetContextUsage { session_id } => {
            let result = conn
                .call("context.get", serde_json::json!({"session_id": session_id}))
                .await?;
            if let Some(envelope) = decode_context_usage(&result, &session_id, request_id) {
                let _ = event_tx.send(envelope);
            }
            Ok(())
        }
        C::ReloadPlugins { session_id } => {
            conn.call(
                "plugins.reload",
                serde_json::json!({"session_id": session_id}),
            )
            .await?;
            Ok(())
        }
        C::GetMcpSnapshot { session_id } => {
            conn.call("mcp.status", serde_json::json!({"session_id": session_id}))
                .await?;
            Ok(())
        }
        C::McpReconnect {
            session_id,
            server_name,
        } => {
            conn.call(
                "mcp.reconnect",
                serde_json::json!({"session_id": session_id, "server_name": server_name}),
            )
            .await?;
            Ok(())
        }
        C::McpToggle {
            session_id,
            server_name,
            enabled,
        } => {
            conn.call(
                "mcp.toggle",
                serde_json::json!({
                    "session_id": session_id,
                    "server_name": server_name,
                    "enabled": enabled,
                }),
            )
            .await?;
            Ok(())
        }
        C::McpSetServers {
            session_id,
            servers,
        } => {
            conn.call(
                "mcp.set_servers",
                serde_json::json!({
                    "session_id": session_id,
                    "servers": serde_json::to_value(servers)?,
                }),
            )
            .await?;
            Ok(())
        }
        C::McpAuthenticate {
            session_id,
            server_name,
        } => {
            conn.call(
                "mcp.authenticate",
                serde_json::json!({"session_id": session_id, "server_name": server_name}),
            )
            .await?;
            Ok(())
        }
        C::McpClearAuth {
            session_id,
            server_name,
        } => {
            conn.call(
                "mcp.clear_auth",
                serde_json::json!({"session_id": session_id, "server_name": server_name}),
            )
            .await?;
            Ok(())
        }
        C::McpOauthCallbackUrl {
            session_id,
            server_name,
            callback_url,
        } => {
            conn.call(
                "mcp.oauth_callback",
                serde_json::json!({
                    "session_id": session_id,
                    "server_name": server_name,
                    "callback_url": callback_url,
                }),
            )
            .await?;
            Ok(())
        }
        C::Shutdown => Ok(()),
    }
}

/// Issue `session.spawn` with the given cwd / resume id, await the
/// response, and emit `BridgeEvent::Connected` carrying the
/// daemon-minted session id. The placeholder model fields in the
/// emitted Connected get superseded by `SessionUpdate::CurrentModelUpdate`
/// once the CLI's `system/init` arrives via `session.event`.
async fn spawn_session(
    conn: &DaemonConnection,
    event_tx: &mpsc::UnboundedSender<EventEnvelope>,
    request_id: Option<String>,
    cwd: &str,
    resume: Option<&str>,
) -> Result<()> {
    let mut options = serde_json::Map::new();
    if !cwd.is_empty() {
        options.insert("cwd".into(), Value::String(cwd.to_owned()));
    }
    if let Some(id) = resume {
        options.insert("resume".into(), Value::String(id.to_owned()));
    }
    let result = conn
        .call(
            "session.spawn",
            serde_json::json!({"options": Value::Object(options)}),
        )
        .await?;
    if let Some(envelope) = decode_spawn_response(&result, cwd, request_id) {
        let _ = event_tx.send(envelope);
    }
    Ok(())
}
