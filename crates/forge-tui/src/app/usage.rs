use std::time::{Duration, SystemTime};

use crate::app::{App, UsageSnapshot, UsageSourceKind, UsageWindow};
use forge_workspace::SessionKey;

/// Pull the latest cached usage snapshot for the active session's
/// account out of the workspace's account-usage pool, populating
/// `UsageState` on the active session. The pool is refreshed every
/// 30 s by the workspace's background poller; this function is
/// purely a sync read-and-copy — no fetch task, no TTL logic.
pub(crate) fn request_refresh_if_needed(app: &mut App) {
    let Some(workspace) = app.workspace.as_ref() else { return };
    let Some(name) = app.active_account_display_name() else { return };
    let snapshot = workspace.usage_for(&name);
    let slot = app.usage_mut();
    let new_source = snapshot.as_ref().map(|s| s.source);
    let changed = !same_snapshot(slot.snapshot.as_ref(), snapshot.as_ref());
    slot.snapshot = snapshot;
    slot.in_flight = false;
    slot.last_error = None;
    slot.last_attempted_source = new_source;
    if changed {
        app.needs_redraw = true;
    }
}

/// Manual-refresh entry point kept for callers (welcome / settings
/// surfaces). Same shape as the if-needed variant — the workspace
/// pool is the only source of usage now, so a "manual refresh"
/// just re-reads it. The workspace poller's 30 s cadence is the
/// floor on how stale a snapshot can be.
pub(crate) fn request_refresh(app: &mut App) {
    request_refresh_if_needed(app);
}

/// Compare two `UsageSnapshot` options for equality on the fields
/// the bottom panel renders. `Eq`/`PartialEq` isn't derived on the
/// snapshot type (timestamps + labels + nested windows), so we do a
/// targeted check here: same source + identical utilisation values
/// across the 5h and 7d windows. Used to gate `needs_redraw` so a
/// no-change refresh doesn't repaint the frame.
fn same_snapshot(a: Option<&UsageSnapshot>, b: Option<&UsageSnapshot>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.source == b.source
                && window_eq(a.five_hour.as_ref(), b.five_hour.as_ref())
                && window_eq(a.seven_day.as_ref(), b.seven_day.as_ref())
                && window_eq(a.seven_day_opus.as_ref(), b.seven_day_opus.as_ref())
                && window_eq(a.seven_day_sonnet.as_ref(), b.seven_day_sonnet.as_ref())
        }
        _ => false,
    }
}

fn window_eq(a: Option<&UsageWindow>, b: Option<&UsageWindow>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            (a.utilization - b.utilization).abs() < f64::EPSILON && a.resets_at == b.resets_at
        }
        _ => false,
    }
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
