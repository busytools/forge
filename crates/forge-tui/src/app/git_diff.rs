//! TUI-side consumer of [`forge_agent::env::git_diff::scan`].
//!
//! Owns the refresh cadence (10s periodic timer + event-driven
//! triggers). Spawned local tasks `await` the workspace's
//! `scan_git_diff` mediator and ship the result back through a std
//! mpsc channel; [`drain_events`] applies the result to the
//! addressed [`UiSession`].
//!
//! Mirrors the existing OAuth-usage refresh pattern (TUI spawns,
//! workspace mediates, agent does the actual work) - see the
//! "Crate placement guide" in `CLAUDE.md`.

#![allow(clippy::module_name_repetitions)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use forge_primitives::git_diff::GitDiffSnapshot;
use forge_workspace::SessionKey;

use crate::app::App;
use crate::app::session::UiSession;

/// How often the ticker pokes the drain pump. The actual `git`
/// subprocess only runs every [`SNAPSHOT_STALENESS`] - the ticker
/// is just a cheap check that compares timestamps. Short enough
/// that cold-start (snapshot == None) is caught within ~1s; cheap
/// enough that the per-second tokio wake is invisible.
const TICKER_INTERVAL: Duration = Duration::from_secs(1);

/// How fresh the snapshot must be before we skip a refresh. When
/// snapshot age exceeds this OR snapshot is `None`, the next
/// ticker poke spawns a scan.
const SNAPSHOT_STALENESS: Duration = Duration::from_secs(10);

/// Max events to apply per drain pump tick. Mirrors `file_index`'s
/// drain budget so a stalled producer can't block the render loop
/// arbitrarily long.
const EVENT_DRAIN_BUDGET: usize = 64;

/// Events shuttled from spawned scanner tasks back to the main
/// loop.
#[derive(Debug)]
pub enum GitDiffEvent {
    /// A scanner task finished. `key` identifies the originating
    /// session bucket; `generation` lets `drain_events` drop stale
    /// results when the session's cwd has changed since the scan
    /// was kicked off. The snapshot is boxed to keep the
    /// `TimerTick` variant from being dominated by it - the
    /// snapshot grew once layer-2 stats joined the type.
    SnapshotReady { key: SessionKey, generation: u64, snapshot: Box<GitDiffSnapshot> },
    /// The 10s idle ticker fired. `drain_events` resolves the
    /// current active session + cwd at consume time and issues a
    /// fresh refresh request - embedding the key here would let it
    /// drift if the user switched sessions between fire and consume.
    TimerTick,
}

/// RAII drop-guard that clears the in-flight flag on drop. Wraps the
/// spawned scan task so a panic mid-scan can't strand the guard at
/// `true` and leave the session permanently unable to refresh.
/// `scan` itself is infallible by construction, but a panic via OOM
/// / allocator failure / future executor bug would otherwise be
/// invisible - the next ticker tick would see the guard still set
/// and silently skip forever.
struct ScanInFlightGuard(Arc<AtomicBool>);

impl Drop for ScanInFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Spawn a tokio local task that awaits
/// `workspace.scan_git_diff(cwd, prev)` and sends a `SnapshotReady`
/// on completion.
///
/// `prev_snapshot` carries the session's most-recent
/// `GitDiffSnapshot`, cloned by the caller (the spawn moves
/// ownership into the task). The scanner uses it to short-circuit
/// the `gh pr list` call when the branch hasn't changed - see
/// [`forge_agent::env::git_diff::scan`].
///
/// Early-returns (and logs at debug level) when:
/// - `cwd` is empty (synthetic spawn key, no real project).
/// - The session's `scan_in_flight` guard is already set (a
///   previous scan hasn't completed; let it win).
pub fn request_refresh(
    tx: std_mpsc::Sender<GitDiffEvent>,
    key: SessionKey,
    cwd: std::path::PathBuf,
    generation: u64,
    scan_in_flight: Arc<AtomicBool>,
    prev_snapshot: Option<GitDiffSnapshot>,
) {
    if cwd.as_os_str().is_empty() {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "git_diff_refresh_skipped",
            message = "git diff refresh skipped: empty cwd",
            outcome = "skipped",
            reason = "empty_cwd",
            key = %key.as_str(),
        );
        return;
    }
    // Compare-and-swap so two parallel callers can't both win.
    if scan_in_flight.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "git_diff_refresh_skipped",
            message = "git diff refresh skipped: already in flight",
            outcome = "skipped",
            reason = "in_flight",
            key = %key.as_str(),
        );
        return;
    }

    let guard = ScanInFlightGuard(scan_in_flight);
    tokio::task::spawn_local(async move {
        // `_guard` resets the in-flight flag on task exit (normal
        // completion, panic, or runtime abort). Moved into the task
        // so its lifetime brackets the await.
        let _guard = guard;
        let snapshot = forge_workspace::env::git_diff::scan(&cwd, prev_snapshot.as_ref()).await;
        // Best-effort send; the receiver going away (app shutdown)
        // is fine, drop the result. Box the snapshot so the channel
        // event keeps the `TimerTick` / `SnapshotReady` variants
        // balanced in size (clippy::large_enum_variant).
        let _ =
            tx.send(GitDiffEvent::SnapshotReady { key, generation, snapshot: Box::new(snapshot) });
    });
}

/// Drain pending git-diff events from the channel and apply to
/// per-session state. Called from `App`'s main loop alongside
/// `file_index::drain_events`. Bounded by [`EVENT_DRAIN_BUDGET`]
/// events per tick.
pub fn drain_events(app: &mut App) {
    for _ in 0..EVENT_DRAIN_BUDGET {
        let event = match app.git_diff_event_rx.try_recv() {
            Ok(event) => event,
            // Empty: nothing more this tick. Disconnected: the
            // ticker / scanner senders are gone (app shutdown).
            // Either way we stop draining - nothing else will arrive
            // during this main-loop pass.
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        apply_event(app, event);
    }
}

fn apply_event(app: &mut App, event: GitDiffEvent) {
    match event {
        GitDiffEvent::SnapshotReady { key, generation, snapshot } => {
            apply_snapshot_ready(app, &key, generation, *snapshot);
        }
        GitDiffEvent::TimerTick => {
            apply_timer_tick(app);
        }
    }
}

fn apply_snapshot_ready(
    app: &mut App,
    key: &SessionKey,
    generation: u64,
    snapshot: GitDiffSnapshot,
) {
    let Some(session) = app.sessions.get_mut(key) else {
        // Session closed during a scan - benign, not actionable.
        // TRACE so the log doesn't flood when the user is rapidly
        // opening / closing sessions.
        tracing::trace!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "git_diff_event_dropped",
            message = "git diff snapshot for unknown session",
            outcome = "dropped",
            reason = "unknown_session",
            key = %key.as_str(),
        );
        return;
    };
    if generation != session.git_diff_generation {
        // Stale generation - usually a session-cwd swap landed
        // mid-scan. WARN-level so operators can spot a hung scanner
        // (repeated stale drops on the same key with no progress)
        // when triaging a "GIT section frozen" report.
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "git_diff_event_dropped",
            message = "git diff snapshot generation stale",
            outcome = "dropped",
            reason = "stale_generation",
            key = %key.as_str(),
            event_generation = generation,
            session_generation = session.git_diff_generation,
        );
        return;
    }
    session.git_diff_snapshot = Some(snapshot);
    session.git_diff_last_refreshed_at = Some(Instant::now());
    app.needs_redraw = true;
}

/// Single rule applied on every ticker poke: refresh the active
/// session when the snapshot is missing OR older than
/// [`SNAPSHOT_STALENESS`]. Everything else is a no-op (cheap
/// timestamp compare).
fn apply_timer_tick(app: &mut App) {
    let Some(active_key) = app.active_session_key.clone() else {
        return;
    };
    let Some(session) = app.sessions.get(&active_key) else {
        return;
    };
    // Only poll truly-connected sessions with a real cwd. Synthetic
    // spawn buckets (`__spawn_<name>__`) have empty cwd_raw;
    // pre-Connect buckets have no session_id.
    if session.cwd_raw.is_empty() || session.session_id.is_none() {
        return;
    }
    if !should_refresh(session) {
        return;
    }
    // For git-repo worker sessions the scan must run inside the
    // `<project_root>/.claude/worktrees/<label>` fork, but `cwd_raw`
    // varies by lifecycle: fresh spawns carry the project root
    // (the pre-fork value from `AgentEvent::Connected.cwd`), resumed
    // sessions carry the worktree path itself (claude chdir'd
    // before writing the catalog row that the resume path reads).
    // `git_scan_cwd_for_session` anchors on the worker's project_key
    // so both shapes converge on the same final path. The workspace
    // is supposed to be Some after init (MVVM contract); a None
    // here is a structural break - trip the debug_assert and warn
    // the release path so the silent fallback to `cwd_raw_path`
    // doesn't hide the failure.
    let cwd_raw_path = std::path::PathBuf::from(session.cwd_raw.clone());
    debug_assert!(
        app.workspace.is_some(),
        "workspace unset after init (apply_timer_tick); MVVM contract violated",
    );
    let cwd = if let Some(workspace) = app.workspace.as_ref() {
        workspace.git_scan_cwd_for_session(&active_key, &cwd_raw_path)
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "git_diff_workspace_unset",
            message = "App.workspace is None during apply_timer_tick; scanning cwd_raw without worker-cwd resolution",
            outcome = "fallback",
            key = %active_key.as_str(),
        );
        cwd_raw_path
    };
    let generation = session.git_diff_generation;
    let scan_in_flight = Arc::clone(&session.git_diff_scan_in_flight);
    // Clone the prior snapshot so the spawned scan can reuse cached
    // PR info when the branch hasn't changed. The session keeps its
    // own copy for render until the new snapshot lands.
    let prev_snapshot = session.git_diff_snapshot.clone();
    request_refresh(
        app.git_diff_event_tx.clone(),
        active_key,
        cwd,
        generation,
        scan_in_flight,
        prev_snapshot,
    );
}

/// Refresh rule: fetch when the snapshot is missing OR the last
/// successful refresh is at least [`SNAPSHOT_STALENESS`] old.
/// Skips otherwise - cached snapshot is fresh enough.
fn should_refresh(session: &UiSession) -> bool {
    if session.git_diff_snapshot.is_none() {
        return true;
    }
    session.git_diff_last_refreshed_at.is_none_or(|last| last.elapsed() >= SNAPSHOT_STALENESS)
}

/// Spawn the periodic ticker as a tokio local task. Sends a
/// `TimerTick` on the channel every [`TICKER_INTERVAL`] with
/// `MissedTickBehavior::Skip`. The drain pump's `apply_timer_tick`
/// is what decides (via `should_refresh`) whether to actually spawn
/// a scan; the ticker itself just pokes. Exits when the receiver is
/// dropped (app shutdown).
pub fn spawn_periodic_timer(tx: std_mpsc::Sender<GitDiffEvent>) {
    tokio::task::spawn_local(async move {
        let mut interval = tokio::time::interval(TICKER_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if tx.send(GitDiffEvent::TimerTick).is_err() {
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::git::GitBranch;
    use forge_primitives::git_diff::LayerState;

    fn snapshot() -> GitDiffSnapshot {
        GitDiffSnapshot {
            branch: GitBranch::Named("main".into()),
            default_branch: Some("main".into()),
            in_repo: true,
            worktree: LayerState::Clean,
            branch_ahead: LayerState::Clean,
            pr: None,
            closes: Vec::new(),
            scanner_ok: true,
        }
    }

    fn session_with_generation(generation: u64) -> UiSession {
        UiSession { git_diff_generation: generation, ..UiSession::default() }
    }

    /// `apply_snapshot_ready` writes the snapshot when the carried
    /// generation matches the session's current generation epoch.
    #[test]
    fn apply_snapshot_ready_writes_when_generation_matches() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_str_for_test("project-a");
        app.sessions.insert(key.clone(), session_with_generation(7));
        // `test_default` seeds `needs_redraw = true`; reset so the
        // post-apply check actually proves the flip rather than just
        // observing the seed.
        app.needs_redraw = false;

        apply_snapshot_ready(&mut app, &key, 7, snapshot());

        let session = app.sessions.get(&key).expect("session exists");
        assert!(session.git_diff_snapshot.is_some());
        assert!(session.git_diff_last_refreshed_at.is_some());
        assert!(app.needs_redraw);
    }

    /// Stale generation: a scan started against an older cwd
    /// (generation < current) lands after the user switched cwds.
    /// The drain pump must drop it rather than overwrite the
    /// freshly-invalidated `None` snapshot.
    #[test]
    fn apply_snapshot_ready_drops_when_generation_stale() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_str_for_test("project-a");
        app.sessions.insert(key.clone(), session_with_generation(7));
        // `test_default` seeds `needs_redraw = true`; reset so the
        // post-apply check is meaningful.
        app.needs_redraw = false;

        // Event carries an older generation than the session currently
        // tracks - drain pump must reject it.
        apply_snapshot_ready(&mut app, &key, 6, snapshot());

        let session = app.sessions.get(&key).expect("session exists");
        assert!(session.git_diff_snapshot.is_none(), "stale snapshot must be rejected");
        assert!(session.git_diff_last_refreshed_at.is_none());
        assert!(!app.needs_redraw);
    }

    /// Unknown-session: a scan landing after its bucket has been
    /// removed (session closed) must not panic - it just logs and
    /// drops.
    #[test]
    fn apply_snapshot_ready_drops_for_unknown_session() {
        let mut app = App::test_default();
        let key = forge_workspace::SessionKey::from_str_for_test("vanished");
        // No entry inserted into `app.sessions` for `key`.
        // `test_default` seeds `needs_redraw = true`; reset so the
        // post-apply check is meaningful.
        app.needs_redraw = false;

        apply_snapshot_ready(&mut app, &key, 0, snapshot());

        assert!(!app.sessions.contains_key(&key), "unknown session stays unknown");
        assert!(!app.needs_redraw);
    }

    /// `request_refresh` early-returns when an empty cwd is passed
    /// (guards against the synthetic `__spawn_<name>__` bucket whose
    /// `cwd_raw` is empty until Connected fires).
    #[test]
    fn request_refresh_skips_when_cwd_empty() {
        // No tokio runtime needed: we only exercise the synchronous
        // early-return path.
        let scan_in_flight = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std_mpsc::channel();

        request_refresh(
            tx,
            forge_workspace::SessionKey::from_str_for_test("project-a"),
            std::path::PathBuf::new(), // empty
            0,
            Arc::clone(&scan_in_flight),
            None,
        );

        // No event spawned, guard untouched, channel empty.
        assert!(!scan_in_flight.load(Ordering::Acquire));
        assert!(rx.try_recv().is_err());
    }

    /// `request_refresh` early-returns when the in-flight guard is
    /// already set (a previous scan is racing to completion and we
    /// let it win). The CAS observation is the test surface.
    #[test]
    fn request_refresh_skips_when_already_in_flight() {
        let scan_in_flight = Arc::new(AtomicBool::new(true)); // pre-set
        let (tx, rx) = std_mpsc::channel();

        request_refresh(
            tx,
            forge_workspace::SessionKey::from_str_for_test("project-a"),
            std::path::PathBuf::from("/tmp/some-cwd"),
            0,
            Arc::clone(&scan_in_flight),
            None,
        );

        // Guard stays set (we didn't take it); no event spawned.
        assert!(scan_in_flight.load(Ordering::Acquire));
        assert!(rx.try_recv().is_err());
    }

    /// `should_refresh` flags a missing snapshot as refresh-needed
    /// unconditionally.
    #[test]
    fn should_refresh_flags_missing_snapshot() {
        let session = UiSession::default();
        assert!(should_refresh(&session));
    }

    /// `should_refresh` skips when the snapshot is fresh (refreshed
    /// just now).
    #[test]
    fn should_refresh_skips_fresh_snapshot() {
        let session = UiSession {
            git_diff_snapshot: Some(snapshot()),
            git_diff_last_refreshed_at: Some(Instant::now()),
            ..UiSession::default()
        };
        assert!(!should_refresh(&session));
    }

    /// `should_refresh` flags a stale snapshot (last refresh older
    /// than `SNAPSHOT_STALENESS`).
    #[test]
    fn should_refresh_flags_stale_snapshot() {
        // `checked_sub` keeps clippy's unchecked-time-subtraction
        // lint happy; expect only fires on a clock that hasn't moved
        // past `SNAPSHOT_STALENESS + 1s` since boot.
        let past = Instant::now()
            .checked_sub(SNAPSHOT_STALENESS + Duration::from_secs(1))
            .expect("monotonic clock is at least SNAPSHOT_STALENESS + 1s past boot");
        let session = UiSession {
            git_diff_snapshot: Some(snapshot()),
            git_diff_last_refreshed_at: Some(past),
            ..UiSession::default()
        };
        assert!(should_refresh(&session));
    }
}
