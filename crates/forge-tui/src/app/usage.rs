use forge_workspace::cloud::{cli, oauth};

use crate::app::{App, UsageSnapshot, UsageSourceKind, UsageSourceMode, UsageWindow};
use forge_workspace::{SessionKey, SessionUpdate};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// How long a usage snapshot stays "fresh" before
/// [`request_refresh_if_needed`] will trigger a re-fetch. The
/// render loop in `app.rs` calls `request_refresh_if_needed` every
/// tick; the TTL gates how often that call actually spawns a fetch
/// task. 2 minutes is a compromise between "panel feels live" and
/// "don't hammer Anthropic's usage endpoint".
const USAGE_REFRESH_TTL: Duration = Duration::from_secs(120);

struct UsageRefreshFailure {
    source: UsageSourceKind,
    message: String,
}

pub(crate) fn request_refresh_if_needed(app: &mut App) {
    if app.usage().in_flight {
        return;
    }
    if app.usage().snapshot.as_ref().is_some_and(is_snapshot_fresh) {
        return;
    }
    request_refresh(app);
}

pub(crate) fn request_refresh(app: &mut App) {
    if app.usage().in_flight || tokio::runtime::Handle::try_current().is_err() {
        return;
    }

    // Capture the bucket key BEFORE the fetch fires so the result
    // routes to the right session even if the user has switched
    // active bucket by the time the fetch lands. `request_refresh`
    // is a no-op when there's no active session, so the unwrap is
    // safe here — but bail defensively just in case.
    let Some(session_key) = app.active_session_key.clone() else {
        return;
    };

    apply_refresh_started_for(app, &session_key);

    let event_tx = app.update_tx.clone();
    let source_mode = app.usage().active_source;
    let cwd_raw = app.cwd_raw();
    // Optional — the CLI fallback path doesn't need a workspace, and
    // tests sometimes drive the lifecycle without one. The OAuth
    // path bails with a clear "no connection" error when the
    // workspace + active session can't be resolved.
    let workspace_key = app
        .workspace
        .as_ref()
        .map(|ws| (Arc::clone(ws), session_key.clone()));

    tokio::task::spawn_local(async move {
        let _ = event_tx
            .send(SessionUpdate::UsageRefreshStarted { key: session_key.clone() });
        match refresh_snapshot(source_mode, cwd_raw, workspace_key.as_ref()).await {
            Ok(snapshot) => {
                let _ = event_tx.send(SessionUpdate::UsageSnapshotReceived {
                    key: session_key,
                    snapshot,
                });
            }
            Err(error) => {
                let _ = event_tx.send(SessionUpdate::UsageRefreshFailed {
                    key: session_key,
                    message: error.message,
                    source: error.source,
                });
            }
        }
    });
}

pub(crate) fn apply_refresh_started_for(app: &mut App, key: &SessionKey) {
    let Some(slot) = app.usage_mut_for(key) else {
        return;
    };
    slot.in_flight = true;
    slot.last_error = None;
    slot.last_attempted_source = None;
}

pub(crate) fn apply_refresh_success_for(app: &mut App, key: &SessionKey, snapshot: UsageSnapshot) {
    let Some(slot) = app.usage_mut_for(key) else {
        return;
    };
    slot.last_attempted_source = Some(snapshot.source);
    slot.snapshot = Some(snapshot);
    slot.in_flight = false;
    slot.last_error = None;
}

pub(crate) fn apply_refresh_failure_for(
    app: &mut App,
    key: &SessionKey,
    message: String,
    source: UsageSourceKind,
) {
    let Some(slot) = app.usage_mut_for(key) else {
        return;
    };
    slot.in_flight = false;
    slot.last_error = Some(message);
    slot.last_attempted_source = Some(source);
}

pub(crate) fn reset_for_session_change(app: &mut App) {
    let slot = app.usage_mut();
    slot.snapshot = None;
    slot.in_flight = false;
    slot.last_error = None;
    slot.last_attempted_source = None;
}

pub(crate) fn visible_windows(snapshot: &UsageSnapshot) -> Vec<&UsageWindow> {
    let mut windows = Vec::new();
    if let Some(window) = snapshot.five_hour.as_ref() {
        windows.push(window);
    }
    if let Some(window) = snapshot.seven_day.as_ref() {
        windows.push(window);
    }
    if let Some(window) = snapshot.seven_day_sonnet.as_ref() {
        windows.push(window);
    }
    if let Some(window) = snapshot.seven_day_opus.as_ref() {
        windows.push(window);
    }
    windows
}

pub(crate) fn format_window_reset(window: &UsageWindow) -> Option<String> {
    if let Some(resets_at) = window.resets_at {
        return Some(format!("resets in {}", format_remaining_until(resets_at)));
    }

    let description = window.reset_description.as_deref()?.trim();
    if description.is_empty() { None } else { Some(description.to_owned()) }
}

fn is_snapshot_fresh(snapshot: &UsageSnapshot) -> bool {
    snapshot.fetched_at.elapsed().is_ok_and(|age| age < USAGE_REFRESH_TTL)
}

fn format_remaining_until(target: SystemTime) -> String {
    let Ok(remaining) = target.duration_since(SystemTime::now()) else {
        return "< 1 minute".to_owned();
    };

    if remaining < Duration::from_secs(60) {
        return "< 1 minute".to_owned();
    }

    let total_minutes = remaining.as_secs() / 60;
    let days = total_minutes / (24 * 60);
    let hours = (total_minutes % (24 * 60)) / 60;
    let minutes = total_minutes % 60;

    if days > 0 {
        return format!("{days}d {hours}h");
    }
    if hours > 0 {
        if minutes == 0 {
            return format!("{hours}h");
        }
        return format!("{hours}h {minutes}m");
    }
    format!("{minutes}m")
}

async fn refresh_snapshot(
    source_mode: UsageSourceMode,
    cwd_raw: String,
    workspace_key: Option<&(Arc<forge_workspace::Workspace>, forge_workspace::SessionKey)>,
) -> Result<UsageSnapshot, UsageRefreshFailure> {
    match source_mode {
        UsageSourceMode::Oauth => fetch_oauth_via_bridge(workspace_key).await,
        UsageSourceMode::Cli => cli::fetch_snapshot(cwd_raw)
            .await
            .map_err(|message| UsageRefreshFailure { source: UsageSourceKind::Cli, message }),
        UsageSourceMode::Auto => refresh_snapshot_auto(cwd_raw, workspace_key).await,
    }
}

async fn fetch_oauth_via_bridge(
    workspace_key: Option<&(Arc<forge_workspace::Workspace>, forge_workspace::SessionKey)>,
) -> Result<UsageSnapshot, UsageRefreshFailure> {
    let Some((workspace, key)) = workspace_key else {
        return Err(UsageRefreshFailure {
            source: UsageSourceKind::Oauth,
            message: "Bridge connection required for OAuth usage fetch.".to_owned(),
        });
    };
    let payload = workspace
        .oauth_usage(key)
        .await
        .map_err(|message| UsageRefreshFailure { source: UsageSourceKind::Oauth, message })?;
    oauth::snapshot_from_payload(payload).map_err(|error| UsageRefreshFailure {
        source: UsageSourceKind::Oauth,
        message: error.into_message(),
    })
}

async fn refresh_snapshot_auto(
    cwd_raw: String,
    workspace_key: Option<&(Arc<forge_workspace::Workspace>, forge_workspace::SessionKey)>,
) -> Result<UsageSnapshot, UsageRefreshFailure> {
    let oauth_result = match workspace_key {
        Some((workspace, key)) => match workspace.oauth_usage(key).await {
            Ok(payload) => oauth::snapshot_from_payload(payload),
            Err(message) => Err(oauth::OauthFetchError::Unavailable(message)),
        },
        None => Err(oauth::OauthFetchError::Unavailable(
            "Bridge connection required for OAuth usage fetch.".to_owned(),
        )),
    };
    match oauth_result {
        Ok(snapshot) => Ok(snapshot),
        Err(error) if error.should_fallback_to_cli() => {
            let oauth_message = error.into_message();
            cli::fetch_snapshot(cwd_raw).await.map_err(|message| UsageRefreshFailure {
                source: UsageSourceKind::Cli,
                message: format!(
                    "OAuth unavailable ({oauth_message}). CLI fallback failed: {message}"
                ),
            })
        }
        Err(error) => Err(UsageRefreshFailure {
            source: UsageSourceKind::Oauth,
            message: error.into_message(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::UsageSourceKind;

    #[test]
    fn formats_day_scale_reset() {
        let target = SystemTime::now() + Duration::from_secs(4 * 24 * 60 * 60 + 12 * 60 * 60);
        let formatted = format_window_reset(&UsageWindow {
            label: "7-day",
            utilization: 50.0,
            resets_at: Some(target),
            reset_description: None,
        })
        .expect("formatted reset");
        assert!(formatted.starts_with("resets in 4d "));
    }

    #[test]
    fn prefers_reset_description_when_no_timestamp_exists() {
        let window = UsageWindow {
            label: "7-day",
            utilization: 40.0,
            resets_at: None,
            reset_description: Some("Resets Feb 12 at 1:30pm (Asia/Calcutta)".to_owned()),
        };
        assert_eq!(
            format_window_reset(&window),
            Some("Resets Feb 12 at 1:30pm (Asia/Calcutta)".to_owned())
        );
    }

    #[test]
    fn collects_only_present_windows() {
        let snapshot = UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: SystemTime::now(),
            five_hour: Some(UsageWindow {
                label: "5-hour",
                utilization: 10.0,
                resets_at: None,
                reset_description: None,
            }),
            seven_day: None,
            seven_day_opus: Some(UsageWindow {
                label: "7-day Opus",
                utilization: 30.0,
                resets_at: None,
                reset_description: None,
            }),
            seven_day_sonnet: None,
            extra_usage: None,
        };

        let labels =
            visible_windows(&snapshot).into_iter().map(|window| window.label).collect::<Vec<_>>();
        assert_eq!(labels, vec!["5-hour", "7-day Opus"]);
    }
}
