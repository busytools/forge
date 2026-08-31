//! Account identity shapes shared across the forge crates.

use serde::{Deserialize, Serialize};

/// Which backend an account talks to, declared per `[[accounts]]` in
/// `forge.toml`. This is the single source of truth for how the account
/// is probed and how its usage renders; nothing infers either from
/// `ANTHROPIC_BASE_URL`, which answers where the credential lives
/// rather than what the backend bills for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// Anthropic proper: keychain credentials, `/api/oauth/usage` on the
    /// default host, plan windows.
    Anthropic,
    /// A `claude-code-proxy` endpoint. Base-url like OpenRouter, but its
    /// proxy serves the same windowed `/api/oauth/usage` body Anthropic
    /// does, so it bills as a subscription.
    Codex,
    /// OpenRouter: pay-per-token, probed at `{base_url}/v1/key`. No
    /// plan windows and no allowance, so its usage is spend over a
    /// period rather than a percentage.
    Openrouter,
}

impl Provider {
    /// Every accepted `provider` value, for the load error a missing or
    /// unusable declaration produces.
    pub const ACCEPTED: &'static str = "\"anthropic\", \"codex\", \"openrouter\"";

    /// `true` when the account's credential is an `ANTHROPIC_AUTH_TOKEN`
    /// beside an `ANTHROPIC_BASE_URL` in `[accounts.env]` rather than a
    /// keychain entry. Both the probe and preflight's repair copy branch
    /// on this rather than on the provider itself, because every
    /// non-Anthropic provider repairs the same way.
    pub const fn uses_base_url(self) -> bool {
        matches!(self, Self::Codex | Self::Openrouter)
    }
}
