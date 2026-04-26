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
            // try_send so a slow / blocked subscriber can't apply
            // backpressure to the rest of the fan-out. TrySendError::Full
            // = subscriber is consuming slower than we're producing
            // (sleeping laptop, hung terminal, network blip); drop the
            // frame for them. TrySendError::Closed = receiver gone =
            // connection is being torn down, the conn's own cleanup
            // path will purge the entry.
            match conn.outbound.try_send(frame.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        connection_id = %sub.0,
                        session_id = %session_id.0,
                        "fanout: dropping frame — subscriber's outbound channel is full (slow consumer)"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Round 4 — fix m4. Promoted from trace to debug.
                    // Operators may want to grep for this during incident
                    // response (e.g. when investigating notification loss
                    // for a flapping client) — debug is the lowest level
                    // visible by default with `RUST_LOG=forged=debug`,
                    // whereas trace requires explicit opt-in.
                    tracing::debug!(
                        connection_id = %sub.0,
                        "fanout: dropping frame to dead subscriber"
                    );
                }
            }
        } else {
            // Round 3 — fix M5. TOCTOU race: the subscribers list
            // referenced a connection that was unregistered between
            // the subscribers-snapshot and the connections-snapshot
            // above. The subscriber's own cleanup path will purge
            // the entry; trace so the inconsistency window is
            // visible if it ever becomes load-bearing.
            tracing::trace!(
                connection_id = %sub.0,
                session_id = %session_id.0,
                "fanout: subscriber not in connections map (TOCTOU race?)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::connection::{Connection, ConnectionId};
    use crate::jsonrpc::Notification;
    use tokio::sync::mpsc;

    /// `fanout` must NOT panic when one of the registered subscribers
    /// has a dropped receiver. The contract is: send fails silently
    /// (the connection's own cleanup path will purge the entry); no
    /// hard error reaches the caller.
    #[tokio::test]
    async fn fanout_to_dead_subscriber_does_not_panic() {
        let state = DaemonState::new();
        let sid = SessionId("sess_fanout".into());
        let (handle, _rx) = state.register_session(sid.clone());

        // Live subscriber.
        let (live_tx, mut live_rx) = mpsc::channel(crate::connection::OUTBOUND_CHANNEL_CAPACITY);
        let live_conn = Connection::new(ConnectionId("conn_live".into()), live_tx);
        state.register_connection(live_conn.clone());

        // Dead subscriber — drop the receiver immediately so any send
        // through the channel fails.
        let (dead_tx, dead_rx) = mpsc::channel(crate::connection::OUTBOUND_CHANNEL_CAPACITY);
        drop(dead_rx);
        let dead_conn = Connection::new(ConnectionId("conn_dead".into()), dead_tx);
        state.register_connection(dead_conn.clone());

        handle.subscribers.lock().push(live_conn.id.clone());
        handle.subscribers.lock().push(dead_conn.id.clone());

        let frame = Outbound::Notification(Notification::new(
            "fanout.test",
            serde_json::json!({"hello": "world"}),
        ));
        // Must not panic.
        fanout(&state, &sid, &frame);

        // Live subscriber received the frame.
        let recv = live_rx.try_recv().expect("live subscriber missed frame");
        match recv {
            Outbound::Notification(n) => {
                assert_eq!(n.method, "fanout.test");
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }
}
