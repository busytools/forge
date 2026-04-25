//! Daemon-wide state — connections + sessions registries.
//!
//! M1 introduced this file with start-time + counters for `daemon.status`.
//! M2 expands it with the sessions `HashMap` so handlers can register and
//! look up live `forge_sdk::Client` instances, plus a connections
//! `HashMap` so the broadcast helper can fan notifications out to
//! subscribers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::connection::{Connection, ConnectionId};
use crate::session_state::{Command, SessionHandle, SessionId, SessionState};

/// Shared daemon state — cloned cheaply (each field is `Arc`-backed) and
/// passed to every spawned connection handler.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct DaemonState {
    /// Wall-clock instant the daemon started accepting connections.
    pub started_at: Instant,
    /// Active session count, surfaced via `daemon.status`.
    pub active_sessions: Arc<Mutex<usize>>,
    /// Connected WS client count, surfaced via `daemon.status`.
    pub connected_clients: Arc<Mutex<usize>>,
    /// Live sessions keyed by daemon-minted [`SessionId`].
    pub sessions: Arc<Mutex<HashMap<SessionId, SessionHandle>>>,
    /// Live connections keyed by [`ConnectionId`]. Cloned on each fanout so
    /// the broadcast helper can address subscribers without holding the
    /// daemon-wide lock across send attempts.
    pub connections: Arc<Mutex<HashMap<ConnectionId, Connection>>>,
}

impl DaemonState {
    /// Construct fresh state with `started_at = Instant::now()`.
    #[must_use]
    pub fn new() -> Self {
        Self::new_for_test(Instant::now())
    }

    /// Construct state with a caller-provided `started_at` — useful in tests
    /// that need to assert a specific elapsed-time value.
    #[must_use]
    pub fn new_for_test(started_at: Instant) -> Self {
        Self {
            started_at,
            active_sessions: Arc::new(Mutex::new(0)),
            connected_clients: Arc::new(Mutex::new(0)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a new session. Returns the freshly-allocated handle plus the
    /// receiver-half of its command channel — the caller spawns the actor
    /// task that drives the underlying [`forge_sdk::Client`] using `rx`.
    ///
    /// Splitting registration from spawn means tests can register a fake
    /// session (no real subprocess) and inspect the registry without a
    /// running actor.
    #[must_use = "the caller must consume the command receiver to drive the session"]
    pub fn register_session(
        &self,
        id: SessionId,
    ) -> (SessionHandle, mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = Arc::new(SessionState::new(id.clone(), tx));
        self.sessions.lock().insert(id, handle.clone());
        *self.active_sessions.lock() += 1;
        (handle, rx)
    }

    /// Unregister a session. No-op if the id is unknown.
    pub fn unregister_session(&self, id: &SessionId) {
        if self.sessions.lock().remove(id).is_some() {
            *self.active_sessions.lock() -= 1;
        }
    }

    /// Look up a session by id.
    #[must_use]
    pub fn get_session(&self, id: &SessionId) -> Option<SessionHandle> {
        self.sessions.lock().get(id).cloned()
    }

    /// Register a connection — adds it to the lookup map and bumps the
    /// `connected_clients` counter atomically.
    pub fn register_connection(&self, conn: Connection) {
        self.connections.lock().insert(conn.id.clone(), conn);
        *self.connected_clients.lock() += 1;
    }

    /// Unregister a connection — removes it from the lookup map and
    /// decrements `connected_clients`. No-op if the id is unknown.
    pub fn unregister_connection(&self, id: &ConnectionId) {
        if self.connections.lock().remove(id).is_some() {
            *self.connected_clients.lock() -= 1;
        }
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}
