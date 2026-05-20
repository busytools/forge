//! TUI-side consumer of [`forge_workspace::Workspace::fetch_cli_version_info`].
//!
//! One-shot fetch at app startup: spawns a `tokio::task::spawn_local`
//! that awaits the workspace mediator and ships the result back via
//! a std mpsc channel. [`drain_events`] applies the snapshot to
//! `App.cli_version_info` so the bottom-left account panel can
//! render the forge + claude version rows + `↑ vX.Y.Z` update
//! indicator.
//!
//! Mirrors the `file_index` / `git_diff` channel pattern but
//! without a ticker — the version probes are stable enough that
//! a single startup fetch is fine for v1. Bumping that to a
//! periodic re-fetch (e.g. once an hour) is a localised change here.

#![allow(clippy::module_name_repetitions)]

use std::sync::mpsc as std_mpsc;

use forge_workspace::env::cli_version::CliVersionInfo;

use crate::app::App;

/// Single-variant event carrying the merged CLI version snapshot
/// back from the spawned probe task to the main loop.
#[derive(Debug)]
pub struct CliVersionEvent {
    pub snapshot: CliVersionInfo,
}

/// Spawn a tokio local task that awaits
/// `forge_workspace::env::cli_version::fetch_info()` and sends a
/// single `CliVersionEvent` on completion. Best-effort send: the
/// receiver going away (app shutdown) is fine, the result just drops.
pub fn spawn_fetch(tx: std_mpsc::Sender<CliVersionEvent>) {
    tokio::task::spawn_local(async move {
        let snapshot = forge_workspace::env::cli_version::fetch_info().await;
        let _ = tx.send(CliVersionEvent { snapshot });
    });
}

/// Drain pending CLI-version events from the channel and apply to
/// `App.cli_version_info`. Called from `App`'s main loop alongside
/// `file_index::drain_events` / `git_diff::drain_events`. At most
/// one event arrives over the app's lifetime, but the drain loop
/// stays bounded for symmetry with the other consumers.
pub fn drain_events(app: &mut App) {
    loop {
        let event = match app.cli_version_event_rx.try_recv() {
            Ok(event) => event,
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        app.cli_version_info = Some(event.snapshot);
        app.needs_redraw = true;
    }
}
