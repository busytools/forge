//! `context.*` method handlers — currently only `context.get`.
//!
//! Same actor-pattern as [`crate::methods::session`] / [`crate::methods::mcp`]
//! — the dispatch handler enqueues a [`Command::ContextGet`] on the
//! session's mpsc and awaits the actor's reply.

use forge_sdk::ContextUsageResponse;
use tokio::sync::oneshot;

use crate::Error;
use crate::registry::DaemonState;
use crate::session_state::{Command, SessionId};

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
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let (reply, recv) = oneshot::channel();
    handle
        .commands
        .send(Command::ContextGet { reply })
        .map_err(|_| Error::InternalError("session actor gone".into()))?;
    recv.await
        .map_err(|_| Error::InternalError("session actor dropped reply channel".into()))?
}
