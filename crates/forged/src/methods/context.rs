//! `context.*` method handlers — currently only `context.get`.
//!
//! Same actor-pattern as [`crate::methods::session`] / [`crate::methods::mcp`]
//! — the dispatch handler enqueues a [`Command::ContextGet`] on the
//! session's mpsc and awaits the actor's reply.

use forge_sdk::ContextUsageResponse;

use crate::Error;
use crate::registry::DaemonState;
use crate::session_state::{Command, SessionId, dispatch_command};

/// `context.get` — query current context-window usage for the named
/// session. Returns the typed [`ContextUsageResponse`] describing
/// per-category token totals.
///
/// # Errors
///
/// `SessionNotFound` if the id is unknown; `Sdk` for transport / parse
/// errors bubbled from the SDK.
pub async fn get(
    state: &DaemonState,
    session_id: &SessionId,
) -> Result<ContextUsageResponse, Error> {
    dispatch_command(state, session_id, |reply| Command::ContextGet { reply }).await
}
