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
use crate::registry::{DaemonState, OutstandingEntry};
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
    // Stash the responder BEFORE sending. The reverse race — "send first,
    // insert after" — would let a fast reply travel out → client → back →
    // resolve() while this task is still suspended between out.send() and
    // outstanding_reverse.insert(); resolve() would then miss the entry
    // and silently drop the answer. Audit 2026-04-26 (bug-hunter,
    // medium confidence) suggested reordering to avoid the symmetric
    // "fake reply slips into a not-yet-sent rev_id slot" race. In a
    // single-user trust model the latter requires either UUID collision
    // (astronomically unlikely with v4) or an adversarial peer (out of
    // scope per project_trust_model.md), so the current ordering is the
    // correct trade. If the trust model changes, switch to a "tentative"
    // flag on insert + confirm-after-send + reject-on-resolve-if-not-
    // confirmed scheme.
    state.outstanding_reverse.lock().insert(
        rev_id.to_owned(),
        OutstandingEntry {
            session_id: session_id.clone(),
            conn_id: Some(pid.clone()),
            prompt_id: prompt_id.to_owned(),
            responder: tx,
        },
    );
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
    if let Some(stale) = state.outstanding_reverse.lock().remove(rev_id) {
        Err(stale.responder)
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

/// Bag of inputs to [`park_in_queue`] — keeps clippy happy about
/// argument count without ceremony.
struct ParkArgs<'a> {
    state: &'a DaemonState,
    session_id: &'a SessionId,
    rev_id: &'a str,
    prompt_id: &'a str,
    kind: PromptKind,
    params: Value,
    timeout: Duration,
    tx: oneshot::Sender<Value>,
}

/// Park a prompt in the per-session queue. Records an outstanding-entry
/// with no `conn_id` — disconnect cleanup will leave it untouched (no
/// answering connection to track). The prompt is removed from
/// `outstanding_reverse` when `prompts.respond` resolves it via
/// [`resolve`] / [`crate::methods::prompts::respond`].
///
/// Both the queue's responder and the outstanding-reverse entry's
/// responder are kept alive — `prompts.respond` directly calls
/// [`resolve`] using the prompt's stored `rev_id`, so the queue's
/// responder is consumed but the SDK-side handler is unblocked
/// synchronously through the outstanding-reverse path. No spawned-task
/// bridge means no race against the timeout cleanup.
fn park_in_queue(args: ParkArgs<'_>) {
    let ParkArgs {
        state,
        session_id,
        rev_id,
        prompt_id,
        kind,
        params,
        timeout,
        tx,
    } = args;
    let Some(handle) = state.get_session(session_id) else {
        return;
    };
    // The queue's responder is a sentinel kept around so disconnect
    // cleanup can take + drop the queue entry without affecting the
    // outstanding-reverse path. The actual SDK-side answer flows
    // through the outstanding-reverse responder (`tx`); see the
    // direct-resolve path in `methods::prompts::respond`.
    let (queue_tx, _queue_rx) = oneshot::channel::<Value>();
    handle.prompts.enqueue(PendingPrompt {
        prompt_id: prompt_id.to_owned(),
        kind,
        issued_at: SystemTime::now(),
        expires_at: SystemTime::now() + timeout,
        params,
        responder: queue_tx,
        rev_id: Some(rev_id.to_owned()),
    });
    // Stash the outstanding entry so `prompts.respond` (which calls
    // `resolve(rev_id)`) drains it. Connection-disconnect cleanup
    // skips it because `conn_id` is None.
    state.outstanding_reverse.lock().insert(
        rev_id.to_owned(),
        OutstandingEntry {
            session_id: session_id.clone(),
            conn_id: None,
            prompt_id: prompt_id.to_owned(),
            responder: tx,
        },
    );
}

/// Notify a session's subscribers that one or more pending prompts
/// have expired because the conn that was answering them disconnected.
/// Called from the WS read loop's connection-cleanup path.
fn notify_disconnect_expired(state: &DaemonState, session_id: &SessionId, prompt_id: &str) {
    let frame = Outbound::Notification(Notification::new(
        "prompts.expired",
        serde_json::json!({
            "session_id": session_id.0,
            "prompt_id": prompt_id,
            "reason": "session_closed",
            "fallback": "deny",
        }),
    ));
    crate::broadcast::fanout(state, session_id, &frame);
}

/// Drain every pending prompt in the session's queue AND every
/// in-flight `outstanding_reverse` entry for the session. Emit
/// `prompts.expired` for each. Called when the session actor exits
/// (any reason). Each parked prompt gets a synthetic
/// `_session_closed: true` answer so the SDK callback unblocks.
///
/// Also walks `outstanding_reverse`: an entry whose primary is still
/// connected but whose session is closing has no parked-queue presence
/// (it's been delivered to the primary, just unanswered). Without this
/// step the SDK callback would wait the full timeout for an answer
/// that will never come.
pub fn drain_prompts_on_session_exit(state: &DaemonState, session_id: &SessionId) {
    let Some(handle) = state.get_session(session_id) else {
        return;
    };
    // Snapshot ids first so we can iterate without holding the queue
    // mutex across the responder.send + broadcast.
    let ids = handle.prompts.snapshot();
    for prompt_id in ids {
        if let Some(p) = handle.prompts.take(&prompt_id) {
            let _ = p.responder.send(serde_json::json!({
                "_session_closed": true,
            }));
        }
        notify_disconnect_expired(state, session_id, &prompt_id);
    }

    // Walk outstanding_reverse for in-flight entries owned by this
    // session and resolve them with the synthetic _session_closed
    // sentinel. The entry now carries the original `prompt_<uuid>`
    // alongside the rev_<uuid>; emit `prompts.expired` keyed on
    // `prompt_id` so the TUI's matcher (which compares against
    // `PendingPermission::prompt_id`, a `prompt_<uuid>`) can dismiss
    // the corresponding modal cleanly.
    let drained: Vec<OutstandingEntry> = {
        let mut o = state.outstanding_reverse.lock();
        let keys: Vec<String> = o
            .iter()
            .filter_map(|(k, v)| {
                if &v.session_id == session_id {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();
        keys.into_iter().filter_map(|k| o.remove(&k)).collect()
    };
    for entry in drained {
        let prompt_id = entry.prompt_id.clone();
        let _ = entry.responder.send(serde_json::json!({
            "_session_closed": true,
        }));
        notify_disconnect_expired(state, session_id, &prompt_id);
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
/// Soft cap on the daemon-wide outstanding-reverse map. Even in the
/// single-user trust model, a session that emits hook-heavy traffic
/// while no primary is connected will accumulate parked prompts for up
/// to HOOK_TIMEOUT_SECS (1h). Cap prevents the map from growing
/// unboundedly across an idle hour; new prompts are denied with
/// security-critical fail-closed semantics until headroom returns.
const OUTSTANDING_REVERSE_CAP: usize = 1024;

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

    if state.outstanding_reverse.lock().len() >= OUTSTANDING_REVERSE_CAP {
        tracing::warn!(
            cap = OUTSTANDING_REVERSE_CAP,
            method = %method,
            session = %session_id.0,
            "outstanding_reverse at cap; rejecting new reverse-RPC with Overloaded"
        );
        return Err(Error::Overloaded);
    }

    let prompt_id = format!("prompt_{}", Uuid::new_v4());
    let rev_id = format!("rev_{}", Uuid::new_v4());
    let (tx, rx) = oneshot::channel::<Value>();

    // Hot path: deliver to the primary if one is connected.
    let send_result =
        try_send_to_primary(state, session_id, &rev_id, method, &prompt_id, &params, tx);

    if let Err(returned_tx) = send_result {
        // No primary OR primary's channel is closed — park in queue.
        // Use the helper so the outstanding-entry table also reflects
        // the parked state (with conn_id=None so disconnect cleanup
        // skips it).
        park_in_queue(ParkArgs {
            state,
            session_id,
            rev_id: &rev_id,
            prompt_id: &prompt_id,
            kind,
            params,
            timeout,
            tx: returned_tx,
        });
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
            // about the timeout (M4.5). Shape mirrors the disconnect
            // path — both carry `reason` + `fallback` so subscribers
            // can branch uniformly on the cause.
            let frame = Outbound::Notification(Notification::new(
                "prompts.expired",
                serde_json::json!({
                    "session_id": session_id.0,
                    "prompt_id": prompt_id,
                    "reason": "timeout",
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
///
/// Unknown `rev_id` values are silently ignored — they typically
/// indicate a late-arriving response after the issuer's timeout fired
/// and cleaned up. A warn-level log keeps the path visible in
/// operator traces.
pub fn resolve(state: &DaemonState, rev_id: &str, value: Value) {
    if let Some(entry) = state.outstanding_reverse.lock().remove(rev_id) {
        let _ = entry.responder.send(value);
    } else {
        tracing::warn!(rev_id, "resolve: unknown rev_id (timeout race?)");
    }
}

/// Resolve an outstanding reverse-RPC with a typed JSON-RPC error
/// payload. Called when the client returns a `{"error": {...}}` shape
/// instead of `{"result": ...}`. Wraps the error in a sentinel object
/// the SDK bridges decode as a deny:
///
/// ```json
/// {"_jsonrpc_error": {"code": -32601, "message": "..."}}
/// ```
///
/// The bridges' `decode_*` paths recognise the `_jsonrpc_error` key
/// and surface a deny with the supplied code+message in the reason —
/// operators get to distinguish "client said deny" from "client
/// errored" in logs and audit.
pub fn resolve_error(state: &DaemonState, rev_id: &str, error_obj: &Value) {
    let code = error_obj.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let message = error_obj
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("");
    tracing::warn!(rev_id, code, message, "reverse-RPC client error");
    if let Some(entry) = state.outstanding_reverse.lock().remove(rev_id) {
        let _ = entry.responder.send(serde_json::json!({
            "_jsonrpc_error": error_obj,
        }));
    } else {
        tracing::warn!(rev_id, "resolve_error: unknown rev_id (timeout race?)");
    }
}
