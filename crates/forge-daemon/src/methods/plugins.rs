//! `plugins.*` method handlers — minimal layer that proxies CLI
//! plugin operations through the session actor.
//!
//! forge-daemon does not maintain its own plugin inventory; the CLI
//! is the source of truth for installed/marketplace plugins. The
//! daemon's role is to forward `plugins.reload` requests through the
//! session actor and return the CLI's raw response so forge-tui can
//! re-render its plugin overlay.

use crate::Error;
use crate::registry::DaemonState;
use crate::session_state::{Command, SessionId, dispatch_command};

/// `plugins.reload` — ask the CLI to refresh the session's plugin
/// inventory (slash commands, agents, MCP servers). Returns the raw
/// JSON the CLI emitted; forge-tui parses it on its side.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport errors.
pub async fn reload(
    state: &DaemonState,
    session_id: &SessionId,
) -> Result<serde_json::Value, Error> {
    dispatch_command(state, session_id, |reply| Command::PluginsReload { reply }).await
}
