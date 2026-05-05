//! Thin TUI-side spawner: kicks off the cloud-side statuspage poller
//! and translates the result into `ClientEvent::ServiceStatus`.

use super::App;
use crate::agent::events::{ClientEvent, ServiceStatusSeverity};
use forge_agent::cloud::service_status::{ServiceSeverity, fetch_service_status};
use tracing::{Instrument as _, info_span};

const STATUSPAGE_SUMMARY_URL: &str = "https://status.claude.com/api/v2/summary.json";

pub fn start_service_status_check(app: &App) {
    let event_tx = app.event_tx.clone();
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
            let severity = match issue.severity {
                ServiceSeverity::Warning => ServiceStatusSeverity::Warning,
                ServiceSeverity::Error => ServiceStatusSeverity::Error,
            };
            let _ = event_tx.send(ClientEvent::ServiceStatus { severity, message: issue.message });
        }
        .instrument(service_status_span),
    );
}
