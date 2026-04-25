//! Broadcast helper — fan a frame out to all current subscribers of a
//! session.

use crate::connection::Outbound;
use crate::registry::DaemonState;
use crate::session_state::SessionId;

/// Send `frame` to every subscriber of `session_id`. Drops connections that
/// have closed (channel send fails) — the connection's own task will clean up.
pub fn fanout(state: &DaemonState, session_id: &SessionId, frame: &Outbound) {
    let Some(handle) = state.get_session(session_id) else {
        return;
    };
    let subs = handle.subscribers.lock().clone();
    let connections = state.connections.lock().clone();
    for sub in subs {
        if let Some(conn) = connections.get(&sub) {
            if conn.outbound.send(frame.clone()).is_err() {
                tracing::trace!(
                    connection_id = %sub.0,
                    "fanout: dropping frame to dead subscriber"
                );
            }
        }
    }
}
