use std::time::{Duration, SystemTime};

use crate::app::{App, UsageSnapshot, UsageWindow};

/// Pull the latest cached usage snapshot for the active session's
/// account out of the workspace's account-usage pool, populating
/// `UsageState` on the active session. The pool is refreshed every
/// 30 s by the workspace's background poller; this function is
/// purely a sync read-and-copy - no fetch task, no TTL logic.
pub(crate) fn request_refresh_if_needed(app: &mut App) {
    let Some(workspace) = app.workspace.clone() else { return };
    follow_binding(app, &workspace);
    let Some(name) = app.active_account_display_name() else { return };
    let snapshot = workspace.usage_for(&name);
    let Some(slot) = app.usage_mut() else {
        return;
    };
    let changed = !same_snapshot(slot.snapshot.as_ref(), snapshot.as_ref());
    slot.snapshot = snapshot;
    slot.in_flight = false;
    slot.last_error = None;
    if changed {
        app.needs_redraw = true;
    }
}

/// Re-key the panel onto the account the gateway is serving the
/// active session with, so a session re-selected onto another account
/// stops rendering the spawn-time name over the serving account's
/// windows. The snapshot needs no dropping: the tick's read is keyed
/// on this name and lands right after.
fn follow_binding(app: &mut App, workspace: &forge_workspace::Workspace) {
    let Some(bound) =
        app.active_session_key.as_ref().and_then(|key| workspace.bound_account_for(key))
    else {
        return;
    };
    if app.active_account_display_name().as_deref() == Some(bound.0.as_str()) {
        return;
    }
    app.set_active_account_display_name(Some(bound.0.clone()));
    app.sync_welcome_snapshot();
    // The read below only marks a redraw when the figures move, so a
    // re-key onto matching windows would leave the stale name painted.
    app.needs_redraw = true;
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

pub(crate) fn reset_for_session_change(app: &mut App) {
    if let Some(slot) = app.usage_mut() {
        slot.snapshot = None;
        slot.in_flight = false;
        slot.last_error = None;
    }
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

    fn usage_snapshot(five_hour: f64, seven_day: f64) -> UsageSnapshot {
        let window =
            |utilization| UsageWindow { utilization, resets_at: None, reset_description: None };
        UsageSnapshot {
            source: crate::app::UsageSourceKind::Oauth,
            fetched_at: SystemTime::now(),
            five_hour: Some(window(five_hour)),
            seven_day: Some(window(seven_day)),
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            balance: None,
            spend: None,
        }
    }

    /// The measured case's starting state: the gateway has moved the
    /// session to `OpenRouter-TM` while the panel still names
    /// `Granite`, the account it spawned on. The caller plants the
    /// panel's windows, since which figures the two accounts report is
    /// what separates the cases below. The tempdir comes back with the
    /// app so it outlives the workspace.
    fn app_showing_the_spawn_time_account() -> (App, tempfile::TempDir) {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let forge = config_dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(
            forge.join("forge.toml"),
            "[[orgs]]\nname = \"TestOrg\"\naccounts = [\"Granite\", \"OpenRouter-TM\"]\n\n\
             [[orgs.projects]]\nname = \"forge\"\npath = \"/tmp\"\n\n\
             [[accounts]]\ndisplay_name = \"Granite\"\ntoken = \"t\"\nmodels = [\"claude-sonnet-5\"]\nprovider = \"anthropic\"\n\n\
             [[accounts]]\ndisplay_name = \"OpenRouter-TM\"\ntoken = \"t\"\nmodels = [\"deepseek-v4.1-flash\"]\nprovider = \"openrouter\"\nbase_url = \"https://openrouter.ai/api\"\n",
        )
        .expect("write forge.toml");
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");

        let key = forge_workspace::SessionSlot::from_str_for_test(App::TEST_SESSION_KEY);
        workspace.seed_test_bound_session(&key, "OpenRouter-TM");
        workspace.seed_test_usage("OpenRouter-TM", usage_snapshot(11.0, 22.0));

        let mut app = App::test_default();
        app.workspace = Some(std::sync::Arc::new(workspace));
        app.set_active_account_display_name(Some("Granite".to_owned()));
        app.needs_redraw = false;
        (app, config_dir)
    }

    /// One tick has to re-key the row and take the serving account's
    /// windows with it, leaving the spawn-time account's behind.
    #[test]
    fn a_tick_follows_the_session_onto_the_account_the_gateway_serves_it_with() {
        let (mut app, _dir) = app_showing_the_spawn_time_account();
        app.usage_mut().expect("active session").snapshot = Some(usage_snapshot(88.0, 99.0));

        request_refresh_if_needed(&mut app);

        assert_eq!(
            app.active_account_display_name().as_deref(),
            Some("OpenRouter-TM"),
            "the row names the account the gateway is serving with",
        );
        assert_eq!(
            app.usage_mut()
                .expect("active session")
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.five_hour.as_ref())
                .map(|window| window.utilization),
            Some(11.0),
            "the windows under the row are the serving account's, not the spawn-time one's",
        );
    }

    /// A re-key repaints on its own. The usage read that follows only
    /// marks a redraw when the figures move, so a session moved onto an
    /// account reporting the windows already on screen is the one shape
    /// where nothing else asks for the frame.
    #[test]
    fn a_rekey_repaints_even_when_both_accounts_report_the_same_windows() {
        let (mut app, _dir) = app_showing_the_spawn_time_account();
        app.usage_mut().expect("active session").snapshot = Some(usage_snapshot(11.0, 22.0));

        request_refresh_if_needed(&mut app);

        assert_eq!(
            app.active_account_display_name().as_deref(),
            Some("OpenRouter-TM"),
            "the row still follows the binding",
        );
        assert!(
            app.needs_redraw,
            "a re-key repaints even when the two accounts' windows happen to match",
        );
    }

    #[test]
    fn formats_day_scale_reset() {
        let target = SystemTime::now() + Duration::from_secs(4 * 24 * 60 * 60 + 12 * 60 * 60);
        let formatted = format_window_reset(&UsageWindow {
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
            utilization: 40.0,
            resets_at: None,
            reset_description: Some("Resets Feb 12 at 1:30pm (Asia/Calcutta)".to_owned()),
        };
        assert_eq!(
            format_window_reset(&window),
            Some("Resets Feb 12 at 1:30pm (Asia/Calcutta)".to_owned())
        );
    }
}
