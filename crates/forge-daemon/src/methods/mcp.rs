//! `mcp.*` method handlers — MCP server status / reconnect / toggle.
//!
//! Each handler is a thin command-sender into the session's actor
//! task. The actor (spawned at `session.spawn`) is the sole owner of
//! the [`forge_sdk::Client`]; locking the
//! [`forge_sdk::Client`] from multiple tasks would deadlock —
//! see [`crate::methods::session`] for the actor-pattern rationale.

use forge_sdk::McpStatusResponse;

use crate::Error;
use crate::registry::DaemonState;
use crate::session_state::{Command, SessionId, dispatch_command};

/// `mcp.status` — query MCP server status for the named session.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport / parse
/// errors bubbled from the SDK.
pub async fn status(
    state: &DaemonState,
    session_id: &SessionId,
) -> Result<McpStatusResponse, Error> {
    dispatch_command(state, session_id, |reply| Command::McpStatus { reply }).await
}

/// `mcp.reconnect` — drop and re-establish the named MCP server's
/// connection.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn reconnect(
    state: &DaemonState,
    session_id: &SessionId,
    server_name: &str,
) -> Result<(), Error> {
    dispatch_command(state, session_id, |reply| Command::McpReconnect {
        server_name: server_name.to_owned(),
        reply,
    })
    .await
}

/// `mcp.toggle` — enable / disable a named MCP server.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn toggle(
    state: &DaemonState,
    session_id: &SessionId,
    server_name: &str,
    enabled: bool,
) -> Result<(), Error> {
    dispatch_command(state, session_id, |reply| Command::McpToggle {
        server_name: server_name.to_owned(),
        enabled,
        reply,
    })
    .await
}
