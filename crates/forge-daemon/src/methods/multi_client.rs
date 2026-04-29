//! `session.claim_primary` + `session.peers` handlers (M5).
//!
//! Per D11 (revised 2026-04-26) `claim_primary` is always granted: the
//! caller becomes primary unconditionally, the existing primary is
//! demoted to viewer, and a `session.primary_changed` notification fans
//! out to every subscriber. There is no approval flow — the
//! `transfer_primary` / `release_primary` shapes considered earlier
//! were dropped from the wire surface.
//!
//! `peers` is a read-only enumeration of subscribers with their role
//! (`primary` / `viewer`), friendly name, and connection-established
//! timestamp.

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::connection::{ConnectionId, Outbound};
use crate::jsonrpc::Notification;
use crate::registry::DaemonState;
use crate::session_state::{SessionHandle, SessionId};

/// Atomically claim the primary slot for `caller` and fan out the
/// notification trio that goes with it: the caller's
/// `session.role_assigned { role: "primary" }`, the displaced primary's
/// `session.role_assigned { role: "viewer", reason: "demoted" }` (when
/// applicable), and the all-subscribers `session.primary_changed`
/// broadcast.
///
/// Returns the previous primary (if any). Caller picks the reasons —
/// `subscribe` uses `"initial"` / `"auto_takeover_on_connect"` based on
/// whether the slot was empty; `claim_primary` always uses `"claim"`
/// for the role and `"claimed"` for the broadcast. `log_site` is
/// embedded in the warn logs that fire when a writer task is gone
/// between the connections-map snapshot and the send (round 4 fix M3
/// in both places).
///
/// Lock ordering follows
/// [`crate::session_state::SessionState`]'s contract: `subscribers`
/// then `primary`, both released before any send.
#[allow(
    clippy::too_many_arguments,
    reason = "the four reason strings document the wire-shape contract; bundling them in a struct adds a layer without clarifying intent"
)]
pub(crate) fn become_primary(
    state: &DaemonState,
    handle: &SessionHandle,
    session_id: &SessionId,
    caller: &ConnectionId,
    role_reason_initial: &'static str,
    role_reason_takeover: &'static str,
    broadcast_reason_initial: &'static str,
    broadcast_reason_takeover: &'static str,
    log_site: &'static str,
) -> Option<ConnectionId> {
    // 1. Add caller to subscribers + atomic primary swap.
    let old_primary = {
        let mut subs = handle.subscribers.lock();
        if !subs.contains(caller) {
            subs.push(caller.clone());
        }
        let mut p = handle.primary.lock();
        let prev = p.clone();
        *p = Some(caller.clone());
        prev
    };
    let displaced_other = matches!(&old_primary, Some(prev) if prev != caller);
    let (role_reason, broadcast_reason) = if displaced_other {
        (role_reason_takeover, broadcast_reason_takeover)
    } else {
        (role_reason_initial, broadcast_reason_initial)
    };

    // 2. Notify caller of role.
    let caller_outbound = state
        .connections
        .lock()
        .get(caller)
        .map(|c| c.outbound.clone());
    match caller_outbound {
        Some(out) => {
            if out
                .try_send(Outbound::Notification(Notification::new(
                    "session.role_assigned",
                    serde_json::json!({
                        "session_id": session_id.0,
                        "role": "primary",
                        "primary": caller.0,
                        "reason": role_reason,
                    }),
                )))
                .is_err()
            {
                tracing::warn!(
                    caller_id = %caller.0,
                    session_id = %session_id.0,
                    log_site,
                    "caller's role_assigned notification dropped (writer task gone)"
                );
            }
        }
        None => {
            tracing::warn!(
                caller_id = %caller.0,
                session_id = %session_id.0,
                log_site,
                "caller's outbound channel not in connections map"
            );
        }
    }

    // 3. Notify displaced primary (if distinct) of demotion.
    if displaced_other
        && let Some(old) = old_primary.as_ref()
    {
        let old_outbound = state
            .connections
            .lock()
            .get(old)
            .map(|c| c.outbound.clone());
        if let Some(out) = old_outbound
            && out
                .try_send(Outbound::Notification(Notification::new(
                    "session.role_assigned",
                    serde_json::json!({
                        "session_id": session_id.0,
                        "role": "viewer",
                        "primary": caller.0,
                        "reason": "demoted",
                    }),
                )))
                .is_err()
        {
            tracing::warn!(
                displaced_id = %old.0,
                session_id = %session_id.0,
                log_site,
                "displaced primary's role_assigned dropped (writer task gone)"
            );
        }
    }

    // 4. Broadcast primary_changed to all subscribers.
    let frame = Outbound::Notification(Notification::new(
        "session.primary_changed",
        serde_json::json!({
            "session_id": session_id.0,
            "primary": caller.0,
            "previous": old_primary.as_ref().map(|c| c.0.clone()),
            "reason": broadcast_reason,
        }),
    ));
    crate::broadcast::fanout(state, session_id, &frame);

    old_primary
}

/// `session.claim_primary` — always granted; demotes any existing primary.
///
/// Wire shape per the spec §7.4.13. Notification flow mirrors
/// auto-takeover (see [`crate::methods::session::subscribe`]):
///
/// 1. Caller receives `session.role_assigned { role: "primary",
///    reason: "claim" }`.
/// 2. Displaced primary (if distinct) receives `session.role_assigned
///    { role: "viewer", reason: "demoted" }`.
/// 3. All subscribers receive `session.primary_changed { reason:
///    "claimed" }`.
///
/// Idempotency: claiming when already primary is a no-op for the role
/// state but still fires the `session.role_assigned` notification (so
/// clients can rely on a frame after every successful claim).
///
/// # Errors
///
/// [`Error::SessionNotFound`] if the session id is unknown.
pub fn claim_primary(
    state: &DaemonState,
    caller: &ConnectionId,
    session_id: &SessionId,
) -> Result<(), Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;

    become_primary(
        state,
        &handle,
        session_id,
        caller,
        "claim",   // role_reason_initial
        "claim",   // role_reason_takeover
        "claimed", // broadcast_reason_initial
        "claimed", // broadcast_reason_takeover
        "claim_primary",
    );
    Ok(())
}

/// `session.peers` — see wire spec §7.4.13.
///
/// Returns the session's subscribers with their role + friendly name +
/// connection-established timestamp. Only subscribers of the named
/// session appear; daemon-wide connections that never subscribed to
/// this session are excluded.
///
/// # Errors
///
/// [`Error::SessionNotFound`] if the session id is unknown.
pub fn peers(state: &DaemonState, session_id: &SessionId) -> Result<PeersResult, Error> {
    let handle = state
        .get_session(session_id)
        .ok_or_else(|| Error::SessionNotFound(session_id.0.clone()))?;
    let primary = handle.primary.lock().clone();
    let subs = handle.subscribers.lock().clone();
    let conns = state.connections.lock().clone();

    let peers: Vec<PeerEntry> = subs
        .iter()
        .filter_map(|cid| {
            let conn = conns.get(cid)?;
            let role = if Some(cid) == primary.as_ref() {
                "primary"
            } else {
                "viewer"
            };
            Some(PeerEntry {
                connection_id: cid.0.clone(),
                name: conn.name.clone(),
                role: role.into(),
                connected_at: conn.connected_at_iso.clone(),
            })
        })
        .collect();
    Ok(PeersResult { peers })
}

/// Wire-shape result of `session.peers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PeersResult {
    /// The session's subscribers, with role + identity.
    pub peers: Vec<PeerEntry>,
}

/// One entry in [`PeersResult::peers`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PeerEntry {
    /// `conn_<uuid>` — stable for the lifetime of the WS connection.
    pub connection_id: String,
    /// Friendly name supplied by the client via `?name=` on handshake.
    pub name: Option<String>,
    /// Either `"primary"` or `"viewer"`.
    pub role: String,
    /// ISO-8601 timestamp the connection was established.
    pub connected_at: String,
}
