//! TUI-side consumer of [`forge_agent::env::processes::scan`].
//!
//! Drives the Inspector pane's PROCESSES section by polling the
//! OS-level descendant tree of the active session's `claude`
//! subprocess. Mirrors the [`crate::app::git_diff`] pattern exactly:
//! 1 s ticker + 1 s staleness rule + std-mpsc channel + spawned
//! local task per scan + per-session in-flight guard + generation
//! counter for cwd / session swaps.
//!
//! The agent-side `scan` is synchronous (sysinfo refresh is a
//! CPU-bound system call, not async I/O) and runs in ~50–100 ms,
//! so the spawned task wraps the call in `tokio::task::spawn_blocking`
//! to keep the runtime responsive between polls.

#![allow(clippy::module_name_repetitions)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use forge_workspace::SessionKey;
use forge_workspace::env::processes::ProcessSnapshot;

use crate::app::App;
use crate::app::session::UiSession;

/// How often the ticker pokes the drain pump. Cheap timestamp check
/// at 1 s; the actual `sysinfo` refresh runs at most every
/// [`SNAPSHOT_STALENESS`].
const TICKER_INTERVAL: Duration = Duration::from_secs(1);

/// How fresh the snapshot must be before we skip a refresh. 1 s
/// matches [`TICKER_INTERVAL`] so a refresh effectively fires on
/// (nearly) every tick — the panel reads as live. The sysinfo scan
/// runs in ~50-100 ms on the blocking pool, so per-tick cost is
/// negligible on multi-core machines and doesn't stall the UI loop.
/// If the panel becomes a CPU hotspot on some machines, bumping
/// this back to 2 s is a one-line revert.
const SNAPSHOT_STALENESS: Duration = Duration::from_secs(1);

/// Max events to apply per drain pump tick — same budget as
/// `file_index` / `git_diff` so all three scanners share a single
/// bound.
const EVENT_DRAIN_BUDGET: usize = 64;

/// Events shuttled from spawned process-scanner tasks back to the
/// main loop.
#[derive(Debug)]
pub enum ProcessScanEvent {
    /// A scanner task finished. `generation` lets `drain_events`
    /// drop stale results when the session's claude PID has changed
    /// (e.g. spawn-time → new session swap) since the scan started.
    SnapshotReady { key: SessionKey, generation: u64, snapshot: ProcessSnapshot },
    /// The 1 s ticker fired. `drain_events` resolves the current
    /// active session at consume time and issues a fresh refresh.
    TimerTick,
}

/// Public ticker driver — call once at App construction. Spawns a
/// task that fires [`ProcessScanEvent::TimerTick`] every
/// [`TICKER_INTERVAL`] until the channel sender goes away.
pub fn spawn_ticker(tx: std_mpsc::Sender<ProcessScanEvent>) {
    tokio::task::spawn_local(async move {
        let mut interval = tokio::time::interval(TICKER_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // First tick fires immediately — skip it so we don't double-poll
        // at startup (App construction itself triggers a refresh via
        // the cold-start `should_refresh` path on the first
        // `apply_timer_tick`).
        interval.tick().await;
        loop {
            interval.tick().await;
            if tx.send(ProcessScanEvent::TimerTick).is_err() {
                // Receiver dropped — main loop is gone, app
                // shutting down. Stop the ticker.
                return;
            }
        }
    });
}

/// RAII drop-guard mirroring `git_diff::ScanInFlightGuard`. Resets
/// the in-flight flag on drop so a panic mid-scan can't strand a
/// session permanently unable to refresh.
struct ScanInFlightGuard(Arc<AtomicBool>);

impl Drop for ScanInFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Spawn a tokio local task that runs
/// `workspace.scan_processes(claude_pid)` on a blocking pool and
/// sends a `SnapshotReady` on completion.
///
/// Early-returns (debug-logged) when:
/// - `claude_pid` is `None` (pre-spawn / disconnected session).
/// - The session's `scan_in_flight` guard is already set.
pub fn request_refresh(
    tx: std_mpsc::Sender<ProcessScanEvent>,
    key: SessionKey,
    claude_pid: Option<u32>,
    generation: u64,
    scan_in_flight: Arc<AtomicBool>,
) {
    let Some(claude_pid) = claude_pid else {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "process_scan_refresh_skipped",
            message = "process scan refresh skipped: no claude pid",
            outcome = "skipped",
            reason = "no_pid",
            key = %key.as_str(),
        );
        return;
    };
    if scan_in_flight.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "process_scan_refresh_skipped",
            message = "process scan refresh skipped: already in flight",
            outcome = "skipped",
            reason = "in_flight",
            key = %key.as_str(),
        );
        return;
    }

    let guard = ScanInFlightGuard(scan_in_flight);
    tokio::task::spawn_local(async move {
        let _guard = guard;
        // `sysinfo` refresh is CPU-bound — offload to the blocking
        // pool so the per-second ticker on this runtime doesn't
        // stall behind the ~50–100 ms scan.
        let snapshot = match tokio::task::spawn_blocking(move || {
            forge_workspace::Workspace::scan_processes(claude_pid)
        })
        .await
        {
            Ok(snap) => snap,
            Err(err) => {
                tracing::warn!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "process_scan_join_failed",
                    message = "process scan blocking task panicked or was cancelled",
                    outcome = "failure",
                    error = %err,
                    key = %key.as_str(),
                );
                return;
            }
        };
        let _ = tx.send(ProcessScanEvent::SnapshotReady { key, generation, snapshot });
    });
}

/// Drain pending process-scan events from the channel and apply
/// them to per-session state. Called from `App`'s main loop
/// alongside `git_diff::drain_events`. Bounded by
/// [`EVENT_DRAIN_BUDGET`].
pub fn drain_events(app: &mut App) {
    for _ in 0..EVENT_DRAIN_BUDGET {
        let event = match app.process_scan_event_rx.try_recv() {
            Ok(event) => event,
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        apply_event(app, event);
    }
}

fn apply_event(app: &mut App, event: ProcessScanEvent) {
    match event {
        ProcessScanEvent::SnapshotReady { key, generation, snapshot } => {
            apply_snapshot_ready(app, &key, generation, snapshot);
        }
        ProcessScanEvent::TimerTick => {
            apply_timer_tick(app);
        }
    }
}

fn apply_snapshot_ready(
    app: &mut App,
    key: &SessionKey,
    generation: u64,
    snapshot: ProcessSnapshot,
) {
    let Some(session) = app.sessions.get_mut(key) else {
        tracing::trace!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "process_scan_event_dropped",
            message = "process snapshot for unknown session",
            outcome = "dropped",
            reason = "unknown_session",
            key = %key.as_str(),
        );
        return;
    };
    if generation != session.process_scan_generation {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "process_scan_event_dropped",
            message = "process snapshot generation stale",
            outcome = "dropped",
            reason = "stale_generation",
            key = %key.as_str(),
            event_generation = generation,
            session_generation = session.process_scan_generation,
        );
        return;
    }
    session.process_snapshot = Some(snapshot);
    session.process_last_refreshed_at = Some(Instant::now());
    app.needs_redraw = true;
}

fn apply_timer_tick(app: &mut App) {
    let Some(active_key) = app.active_session_key.clone() else {
        return;
    };
    let Some(session) = app.sessions.get(&active_key) else {
        return;
    };
    if session.session_id.is_none() {
        return;
    }
    if !should_refresh(session) {
        return;
    }
    let Some(workspace) = app.workspace.as_ref() else {
        return;
    };
    let claude_pid = workspace.claude_pid(&active_key);
    let generation = session.process_scan_generation;
    let scan_in_flight = Arc::clone(&session.process_scan_in_flight);
    request_refresh(
        app.process_scan_event_tx.clone(),
        active_key,
        claude_pid,
        generation,
        scan_in_flight,
    );
}

/// True when the session needs a fresh scan: no snapshot yet OR the
/// last one is older than [`SNAPSHOT_STALENESS`].
fn should_refresh(session: &UiSession) -> bool {
    match session.process_last_refreshed_at {
        None => true,
        Some(last) => last.elapsed() >= SNAPSHOT_STALENESS,
    }
}
