/// Why a synthesis attempt failed; the tool layer words its message from
/// these variants.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("model file missing: {0}")]
    ModelMissing(String),
    #[error("model load failed: {0}")]
    Load(String),
    #[error("render failed: {0}")]
    Render(String),
    /// The caller stopped a bench: what ran is not a result.
    #[error("bench cancelled")]
    Cancelled,
}
