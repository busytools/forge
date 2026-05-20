//! Turn-error classification used by both forge-agent (the classifier
//! lives there next to the matching regex helpers) and
//! forge-workspace (re-emitted via `SessionUpdate::TurnError`). The
//! enum itself lives here so neither crate has to re-state it.

/// Coarse classification of a turn-level error message. The
/// classifier itself lives in forge-agent's
/// `translate::error_handling`; this enum is the wire shape it
/// produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnErrorClass {
    PlanLimit,
    AuthRequired,
    Internal,
    Other,
}
