//! Model state shared by more than one of the model's own modules.

/// Lifecycle status of a Monitor (`Monitor` tool_use).
/// A Monitor row stays surfaced until ALL session monitors transition
/// to a terminal variant (`Stopped` / `Completed` / `TimedOut`); the
/// MONITORS Inspector section auto-clears when no monitor is still
/// `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStatus {
    /// Monitor is active. Persistent monitors stay `Running` until
    /// TaskStop or session end; non-persistent monitors run until
    /// their `timeout_ms` expires or the watched command exits.
    Running,
    /// Monitor terminated via TaskStop / killed / clean exit.
    Stopped,
    /// Monitor completed cleanly (synonym for Stopped on the
    /// renderer; preserved as a distinct variant in case downstream
    /// callers want to disambiguate normal-exit from explicit-kill).
    Completed,
    /// Monitor's `timeout_ms` fired. Renderer surfaces a distinct
    /// `· timed out` badge so users see the failure mode at a glance.
    TimedOut,
}
