//! Statuspage-derived service status wire shapes.

/// Severity of a detected service-status issue. Mirrored on the UI
/// side as `ClientEvent::ServiceStatus { severity, .. }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceSeverity {
    Warning,
    Error,
}

/// A classified Anthropic service-status issue from the Statuspage
/// summary endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServiceIssue {
    pub severity: ServiceSeverity,
    pub message: String,
}
