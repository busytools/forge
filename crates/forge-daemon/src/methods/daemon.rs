//! `daemon.*` method handlers.

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::registry::DaemonState;

/// Wire-shape result of `daemon.status` — see wire spec §7.4.9.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DaemonStatus {
    /// Seconds elapsed since the daemon started.
    pub uptime_seconds: u64,
    /// Number of active forge-sdk sessions on this daemon.
    pub active_sessions: usize,
    /// Number of connected WebSocket clients.
    pub connected_clients: usize,
    /// Most recent daemon-level error message, if any.
    pub last_error: Option<String>,
    /// Daemon binary version (`CARGO_PKG_VERSION`).
    pub version: &'static str,
    /// Build identifier (commit SHA when set via `FORGED_BUILD_SHA`, else `"dev"`).
    pub build: &'static str,
    /// `WireGuard` IP the listener is bound to, if any.
    pub wg_ip_bound: Option<String>,
}

/// `daemon.status` — see wire spec §7.4.9.
///
/// # Errors
///
/// Currently infallible; future expansions may surface state-read failures.
#[allow(
    clippy::unused_async,
    reason = "M1 stub is sync; sibling handlers (M2+) are genuinely async — keep signature uniform for the dispatcher"
)]
pub async fn status(state: &DaemonState) -> Result<DaemonStatus, Error> {
    Ok(DaemonStatus {
        uptime_seconds: state.started_at.elapsed().as_secs(),
        active_sessions: state.sessions.lock().len(),
        connected_clients: state.connections.lock().len(),
        last_error: None,
        version: env!("CARGO_PKG_VERSION"),
        build: option_env!("FORGED_BUILD_SHA").unwrap_or("dev"),
        wg_ip_bound: None,
    })
}
