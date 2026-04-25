//! Daemon-wide state — connections + sessions registries.
//!
//! M1 stub: only the start-time and counters needed for `daemon.status`.
//! Expanded in M2+ as session state lands.

use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Instant;

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
        }
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}
