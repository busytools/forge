//! Thin TUI-side spawner: kicks off the cloud-side statuspage poller
//! and translates the result into `SessionUpdate::ServiceStatus`.

use super::App;
use forge_agent::cloud::service_status::fetch_service_status;
use forge_workspace::SessionUpdate;
use tracing::{Instrument as _, info_span};

const STATUSPAGE_SUMMARY_URL: &str = "https://status.claude.com/api/v2/summary.json";

pub fn start_service_status_check(app: &App) {
    let update_tx = app.update_tx.clone();
    tracing::info!(
        target: crate::logging::targets::APP_NETWORK,
        event_name = "service_check_started",
        message = "service status check started",
        outcome = "start",
        url = STATUSPAGE_SUMMARY_URL,
    );

    let service_status_span = info_span!(
        target: crate::logging::targets::APP_NETWORK,
        "service_status_check",
        url = STATUSPAGE_SUMMARY_URL,
    );

    tokio::task::spawn_local(
        async move {
            let Some(issue) = fetch_service_status().await else {
                return;
            };
            tracing::info!(
                target: crate::logging::targets::APP_NETWORK,
                event_name = "service_issue_detected",
                message = "service status issue detected",
                outcome = "success",
                severity = ?issue.severity,
            );
            // `forge_agent::cloud::service_status::ServiceSeverity` is
            // the same wire-shape enum as
            // `forge_primitives::cloud::service_status::ServiceSeverity`
            // (re-exported); the `SessionUpdate::ServiceStatus` variant
            // can consume it directly.
            let _ = update_tx.send(SessionUpdate::ServiceStatus {
                severity: issue.severity,
                message: issue.message,
            });
        }
        .instrument(service_status_span),
    );
}
