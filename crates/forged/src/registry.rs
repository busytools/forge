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
use tokio::sync::{mpsc, oneshot};

use crate::connection::{Connection, ConnectionId};
use crate::session_state::{Command, SessionHandle, SessionId, SessionState};

/// One outstanding reverse-RPC. Tracks
/// `(session_id, conn_id, prompt_id, responder)` so disconnect cleanup
/// can synthesise an answer for any in-flight rev request whose
/// answering connection just went away.
///
/// `conn_id` is `None` when the request is parked in the per-session
/// queue (no primary at issue time).
///
/// `prompt_id` mirrors the queue's `PendingPrompt::prompt_id` so that
/// session-exit drains can emit `prompts.expired` notifications keyed
/// on the user-visible `prompt_<uuid>` (which the TUI's
/// `PromptsExpired` matcher compares against `pp.prompt_id`) rather
/// than the daemon-internal `rev_<uuid>`.
pub struct OutstandingEntry {
    /// Session the rev request belongs to.
    pub session_id: SessionId,
    /// Conn that owns the answer; `None` when parked.
    pub conn_id: Option<ConnectionId>,
    /// `prompt_<uuid>` minted by `issue_to_primary` for this rev. Carried
    /// alongside the responder so the drain path can emit
    /// `prompts.expired` with the user-visible prompt id, not the
    /// internal rev id.
    pub prompt_id: String,
    /// Resumes the SDK callback awaiting this answer.
    pub responder: oneshot::Sender<serde_json::Value>,
}

impl std::fmt::Debug for OutstandingEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutstandingEntry")
            .field("session_id", &self.session_id)
            .field("conn_id", &self.conn_id)
            .field("prompt_id", &self.prompt_id)
            .field("responder", &"<oneshot>")
            .finish()
    }
}

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
    /// Outstanding reverse-RPC requests keyed by `rev_<uuid>` id. The
    /// outstanding entry carries the session/conn context plus the
    /// oneshot sender; disconnect cleanup walks this map and synthesises
    /// answers for entries whose answering conn just went away.
    pub outstanding_reverse: Arc<Mutex<HashMap<String, OutstandingEntry>>>,
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
            outstanding_reverse: Arc::new(Mutex::new(HashMap::new())),
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
    ///
    /// Returns the list of session ids whose primary slot was cleared
    /// because they pointed at this conn — callers (the WS read loop)
    /// fan a `session.primary_changed { primary: null,
    /// reason: "disconnected" }` notification to subscribers of those
    /// sessions, and synthesise client-disconnected answers for any
    /// in-flight reverse-RPC owned by this conn.
    #[must_use = "callers should fan a session.primary_changed { reason: \"disconnected\" } notification for each cleared session"]
    pub fn unregister_connection(&self, id: &ConnectionId) -> Vec<SessionId> {
        // Walk all sessions, drop the conn from subscribers, and clear
        // the primary slot when it pointed at this conn. Capture which
        // sessions we touched so the caller can broadcast.
        let cleared = self.purge_connection_from_sessions(id);

        // Synthesise client-disconnected responses for in-flight
        // reverse-RPC owned by this conn — without this, the bridges
        // wait the full 1h timeout. Drain into a Vec so we can drop the
        // mutex before sending.
        let to_unblock: Vec<OutstandingEntry> = {
            let mut o = self.outstanding_reverse.lock();
            let keys: Vec<String> = o
                .iter()
                .filter_map(|(k, v)| {
                    if v.conn_id.as_ref() == Some(id) {
                        Some(k.clone())
                    } else {
                        None
                    }
                })
                .collect();
            keys.into_iter().filter_map(|k| o.remove(&k)).collect()
        };
        for entry in to_unblock {
            // Round 5 — symmetry trace. The responder is a
            // `oneshot::Sender<Value>`; `.send` returns `Err(value)` when
            // the receiver has already been dropped. That happens when
            // the SDK callback for this rev was cancelled on its side
            // (session shutdown raced disconnect cleanup). Benign drop —
            // the answer is no longer needed — but tracing makes
            // late-stage drops attributable during incident response.
            let session_id = entry.session_id.clone();
            let prompt_id = entry.prompt_id.clone();
            if entry
                .responder
                .send(serde_json::json!({
                    "_client_disconnected": true,
                }))
                .is_err()
            {
                tracing::trace!(
                    session_id = %session_id.0,
                    prompt_id = %prompt_id,
                    "registry: disconnect-cleanup responder receiver gone (callback already cancelled)"
                );
            }
        }

        if self.connections.lock().remove(id).is_some() {
            *self.connected_clients.lock() -= 1;
        }
        cleared
    }

    /// Walk every session and (a) drop `conn_id` from `subscribers`,
    /// (b) clear the `primary` slot if it pointed at `conn_id`. Returns
    /// the list of session ids whose primary was cleared so callers
    /// can broadcast `session.primary_changed`.
    #[must_use = "callers should fan a session.primary_changed notification for each cleared session"]
    fn purge_connection_from_sessions(&self, conn_id: &ConnectionId) -> Vec<SessionId> {
        let sessions = self.sessions.lock().clone();
        let mut cleared = Vec::new();
        for (sid, handle) in sessions {
            handle.subscribers.lock().retain(|c| c != conn_id);
            let mut p = handle.primary.lock();
            if p.as_ref() == Some(conn_id) {
                *p = None;
                cleared.push(sid);
            }
        }
        cleared
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}
