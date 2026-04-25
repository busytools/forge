//! forged error type. Bridges to JSON-RPC error codes via the spec's
//! -32000 / -32100 ranges (see §7.4.15 of the wire spec).

/// Top-level error returned by forged operations.
///
/// M0 stub — the full taxonomy of JSON-RPC error codes lands in M1.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Placeholder variant returned by surfaces that haven't been wired
    /// up yet. Replaced with concrete variants in M1.
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
}
