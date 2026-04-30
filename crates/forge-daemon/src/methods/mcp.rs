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

/// `mcp.set_servers` — replace the active MCP server set on the
/// session. Server map is forwarded to the CLI as raw JSON.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn set_servers(
    state: &DaemonState,
    session_id: &SessionId,
    servers: serde_json::Value,
) -> Result<(), Error> {
    dispatch_command(state, session_id, |reply| Command::McpSetServers {
        servers,
        reply,
    })
    .await
}

/// `mcp.authenticate` — kick off an OAuth flow for the named server.
/// Returns the CLI's raw response (typically a URL or status object).
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn authenticate(
    state: &DaemonState,
    session_id: &SessionId,
    server_name: &str,
) -> Result<serde_json::Value, Error> {
    dispatch_command(state, session_id, |reply| Command::McpAuthenticate {
        server_name: server_name.to_owned(),
        reply,
    })
    .await
}

/// `mcp.clear_auth` — drop stored OAuth credentials for a server.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn clear_auth(
    state: &DaemonState,
    session_id: &SessionId,
    server_name: &str,
) -> Result<(), Error> {
    dispatch_command(state, session_id, |reply| Command::McpClearAuth {
        server_name: server_name.to_owned(),
        reply,
    })
    .await
}

/// `mcp.oauth_callback` — forward an OAuth callback URL to complete
/// MCP authentication. CLI subtype is `mcp_oauth_callback_url`.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn oauth_callback(
    state: &DaemonState,
    session_id: &SessionId,
    server_name: &str,
    callback_url: &str,
) -> Result<(), Error> {
    dispatch_command(state, session_id, |reply| Command::McpOauthCallback {
        server_name: server_name.to_owned(),
        callback_url: callback_url.to_owned(),
        reply,
    })
    .await
}
