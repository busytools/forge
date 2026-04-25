//! Reverse-RPC issuer + outstanding-id resolver.
//!
//! When the SDK invokes a callback (`can_use_tool` or a hook), the daemon
//! issues a JSON-RPC request to the session's primary client and waits
//! for the response. If no primary is connected — or the primary
//! disconnects mid-request — the prompt parks in the per-session
//! [`PromptQueue`](crate::prompt_queue::PromptQueue) (D14) and the
//! awaiting handler keeps waiting until either a client reconnects +
//! answers, or the configured timeout fires (D13: 1 hour for hooks).

use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::Error;
use crate::connection::Outbound;
use crate::jsonrpc::{Notification, Request};
use crate::prompt_queue::{PendingPrompt, PromptKind};
use crate::registry::DaemonState;
use crate::session_state::SessionId;

/// Internal helper — try to send the reverse-RPC to the primary's
/// outbound channel. Returns `Some(())` on successful send (in which
/// case the responder has been stashed in `outstanding_reverse`); `None`
/// when there is no primary or the primary's channel is closed (and any
/// staged responder has been peeled back).
///
/// Splitting this out lets the caller hand `tx` ownership into the
/// branch that does insert, while still recovering `tx` cleanly when
/// the send fails.
fn try_send_to_primary(
    state: &DaemonState,
    session_id: &SessionId,
    rev_id: &str,
    method: &str,
    prompt_id: &str,
    params: &Value,
    tx: oneshot::Sender<Value>,
) -> Result<(), oneshot::Sender<Value>> {
    let Some(handle) = state.get_session(session_id) else {
        return Err(tx);
    };
    let primary_id = handle.primary.lock().clone();
    let Some(pid) = primary_id else {
        return Err(tx);
    };
    // Snapshot the primary connection's outbound channel without
    // holding the connections lock while we send.
    let outbound = state
        .connections
        .lock()
        .get(&pid)
        .map(|c| c.outbound.clone());
    let Some(out) = outbound else {
        return Err(tx);
    };
    // Stash the responder before sending so we don't race with the
    // response arriving before we record the awaiter.
    state
        .outstanding_reverse
        .lock()
        .insert(rev_id.to_owned(), tx);
    let req = Request::new(
        method,
        serde_json::json!({
            "session_id": session_id.0,
            "prompt_id": prompt_id,
            "params": params,
        }),
        Value::String(rev_id.to_owned()),
    );
    if out.send(Outbound::Request(req)).is_ok() {
        return Ok(());
    }
    // Channel closed — peel back the responder so it isn't leaked in
    // `outstanding_reverse`. Return ownership to the caller so it can
    // park the prompt in the queue instead.
    if let Some(stale_tx) = state.outstanding_reverse.lock().remove(rev_id) {
        Err(stale_tx)
    } else {
        // Already taken by something else racing us — this branch
        // should be unreachable in practice because `resolve()` would
        // have sent already, meaning the rx side already got an
        // answer. Manufacturing a fresh sender keeps the caller's
        // ownership-transfer contract honest.
        let (new_tx, _) = oneshot::channel();
        Err(new_tx)
    }
}

/// Issue a reverse-RPC to the session's primary, or park in the queue
/// if no primary is connected. Returns the client's response value (or
/// times out per `timeout`).
///
/// On timeout, emits a `prompts.expired` notification to all subscribers
/// of the session (M4.5) so any reconnected client knows the prompt is
/// gone.
///
/// # Errors
///
/// - [`Error::SessionNotFound`] if the session id is unknown.
/// - [`Error::TemporarilyUnavailable`] on timeout or channel drop.
pub async fn issue_to_primary(
    state: &DaemonState,
    session_id: &SessionId,
    method: &str,
    params: Value,
    kind: PromptKind,
    timeout: Duration,
) -> Result<Value, Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;

    let prompt_id = format!("prompt_{}", Uuid::new_v4());
    let rev_id = format!("rev_{}", Uuid::new_v4());
    let (tx, rx) = oneshot::channel::<Value>();

    // Hot path: deliver to the primary if one is connected.
    let send_result =
        try_send_to_primary(state, session_id, &rev_id, method, &prompt_id, &params, tx);

    if let Err(returned_tx) = send_result {
        // No primary OR primary's channel is closed — park in queue.
        let prompt = PendingPrompt {
            prompt_id: prompt_id.clone(),
            kind,
            issued_at: SystemTime::now(),
            expires_at: SystemTime::now() + timeout,
            params,
            responder: returned_tx,
        };
        handle.prompts.enqueue(prompt);
    }

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) => Err(Error::TemporarilyUnavailable(format!(
            "reverse-RPC channel dropped before answer ({method})"
        ))),
        Err(_elapsed) => {
            // Cleanup on timeout — drop from both potential resting
            // places (outstanding table + queue).
            state.outstanding_reverse.lock().remove(&rev_id);
            let _ = handle.prompts.take(&prompt_id);

            // Emit `prompts.expired` so reconnected subscribers learn
            // about the timeout (M4.5).
            let frame = Outbound::Notification(Notification::new(
                "prompts.expired",
                serde_json::json!({
                    "session_id": session_id.0,
                    "prompt_id": prompt_id,
                    "fallback": "deny",
                }),
            ));
            crate::broadcast::fanout(state, session_id, &frame);

            Err(Error::TemporarilyUnavailable(format!(
                "reverse-RPC timed out after {}s ({method})",
                timeout.as_secs()
            )))
        }
    }
}

/// Resolve an outstanding reverse-RPC by its `rev_id`. Called by the
/// server's read loop when an inbound JSON-RPC response arrives whose
/// id matches an outstanding entry.
pub fn resolve(state: &DaemonState, rev_id: &str, value: Value) {
    if let Some(tx) = state.outstanding_reverse.lock().remove(rev_id) {
        let _ = tx.send(value);
    }
}
