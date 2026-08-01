//! TUI-side consumer of [`forge_workspace::Workspace::fetch_cli_version_info`].
//!
//! Spawns a local task that fetches the merged CLI-version snapshot at
//! startup and then re-probes on [`REFRESH_INTERVAL`] so a transient
//! startup miss (proxy down, briefly offline) self-heals within the
//! session. Each result is shipped back over a std mpsc channel;
//! [`drain_events`] merges it into `App.cli_version_info`, keeping a
//! previously-resolved field when a later probe comes back empty.
//!
//! Mirrors the `file_index` / `git_diff` channel pattern.

use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use forge_workspace::env::cli_version::CliVersionInfo;

use crate::app::App;

/// How often the version probe re-runs after the immediate startup
/// fetch. Versions change rarely, so a few minutes is ample - the
/// point is only to recover a transient startup failure, not to track
/// releases tightly.
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Single-variant event carrying a merged CLI version snapshot back
/// from the spawned probe task to the main loop.
#[derive(Debug)]
pub struct CliVersionEvent {
    pub snapshot: CliVersionInfo,
}

/// Spawn a tokio local task that probes the CLI versions immediately
/// and then every [`REFRESH_INTERVAL`], sending a `CliVersionEvent`
/// after each probe. Best-effort send: a gone receiver (app shutdown)
/// ends the loop.
pub fn spawn_fetch(tx: std_mpsc::Sender<CliVersionEvent>) {
    tokio::task::spawn_local(async move {
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // The first tick completes immediately, preserving the
            // startup fetch; later ticks pace the refresh.
            interval.tick().await;
            let snapshot = forge_workspace::env::cli_version::fetch_info().await;
            if tx.send(CliVersionEvent { snapshot }).is_err() {
                return;
            }
        }
    });
}

/// Drain pending CLI-version events and merge each into
/// `App.cli_version_info`. Called from `App`'s main loop alongside
/// `git_diff::drain_events`. Redraws only when the merge changes the
/// stored snapshot.
pub fn drain_events(app: &mut App) {
    loop {
        let event = match app.cli_version_event_rx.try_recv() {
            Ok(event) => event,
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        let merged = merge_snapshot(app.cli_version_info.as_ref(), event.snapshot);
        if app.cli_version_info.as_ref() != Some(&merged) {
            app.cli_version_info = Some(merged);
            app.needs_redraw = true;
        }
    }
}

/// Merge a freshly-probed snapshot over the stored one, keeping a
/// previously-resolved field when the new probe came back `None` for
/// it - a later failed network probe must not wipe a `latest` an
/// earlier probe already found.
fn merge_snapshot(prev: Option<&CliVersionInfo>, next: CliVersionInfo) -> CliVersionInfo {
    let Some(prev) = prev else { return next };
    CliVersionInfo {
        installed: next.installed.or_else(|| prev.installed.clone()),
        latest: next.latest.or_else(|| prev.latest.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_prev_latest_when_new_probe_has_none() {
        let prev =
            CliVersionInfo { installed: Some("2.1.156".into()), latest: Some("2.1.201".into()) };
        let next = CliVersionInfo { installed: Some("2.1.156".into()), latest: None };
        let merged = merge_snapshot(Some(&prev), next);
        assert_eq!(merged.latest.as_deref(), Some("2.1.201"));
        assert_eq!(merged.installed.as_deref(), Some("2.1.156"));
    }

    #[test]
    fn merge_keeps_prev_installed_when_new_probe_has_none() {
        let prev = CliVersionInfo { installed: Some("2.1.156".into()), latest: None };
        let next = CliVersionInfo { installed: None, latest: Some("2.1.201".into()) };
        let merged = merge_snapshot(Some(&prev), next);
        assert_eq!(merged.installed.as_deref(), Some("2.1.156"));
        assert_eq!(merged.latest.as_deref(), Some("2.1.201"));
    }

    #[test]
    fn merge_takes_new_latest_when_present() {
        let prev =
            CliVersionInfo { installed: Some("2.1.156".into()), latest: Some("2.1.201".into()) };
        let next =
            CliVersionInfo { installed: Some("2.1.156".into()), latest: Some("2.1.210".into()) };
        let merged = merge_snapshot(Some(&prev), next);
        assert_eq!(merged.latest.as_deref(), Some("2.1.210"));
    }

    #[test]
    fn merge_returns_next_when_no_prev() {
        let next =
            CliVersionInfo { installed: Some("2.1.156".into()), latest: Some("2.1.201".into()) };
        let merged = merge_snapshot(None, next.clone());
        assert_eq!(merged, next);
    }

    #[test]
    fn drain_merges_and_preserves_prev_latest_without_redraw() {
        let mut app = App::test_default();
        app.cli_version_info = Some(CliVersionInfo {
            installed: Some("2.1.156".into()),
            latest: Some("2.1.201".into()),
        });
        app.needs_redraw = false;
        app.cli_version_event_tx
            .send(CliVersionEvent {
                snapshot: CliVersionInfo { installed: Some("2.1.156".into()), latest: None },
            })
            .expect("send event");

        drain_events(&mut app);

        let info = app.cli_version_info.expect("snapshot present");
        assert_eq!(info.latest.as_deref(), Some("2.1.201"), "good latest survives a failed probe");
        assert!(!app.needs_redraw, "no visible change, so no redraw");
    }

    #[test]
    fn drain_applies_and_redraws_on_change() {
        let mut app = App::test_default();
        app.cli_version_info = None;
        app.needs_redraw = false;
        app.cli_version_event_tx
            .send(CliVersionEvent {
                snapshot: CliVersionInfo {
                    installed: Some("2.1.156".into()),
                    latest: Some("2.1.201".into()),
                },
            })
            .expect("send event");

        drain_events(&mut app);

        assert!(app.needs_redraw);
        assert!(app.cli_version_info.expect("snapshot present").has_update());
    }
}
