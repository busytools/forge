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

    /// `true` when this backend charges per token rather than against a
    /// plan allowance, so its usage is money over a period and it has
    /// no window to be a percentage of.
    ///
    /// Written out per variant rather than with `matches!`, so a new
    /// provider has to state its billing model instead of defaulting to
    /// windows and rendering `5h` / `7d` labels it may not have.
    pub const fn bills_by_spend(self) -> bool {
        match self {
            Self::Anthropic | Self::Codex => false,
            Self::Openrouter => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ACCEPTED` is the load error's list of what the user may write,
    /// and it is hand-maintained, so a new variant can ship without
    /// appearing in it and leave the error telling people the wrong set.
    #[test]
    fn accepted_lists_every_variant() {
        for (variant, token) in [
            (Provider::Anthropic, "anthropic"),
            (Provider::Codex, "codex"),
            (Provider::Openrouter, "openrouter"),
        ] {
            let serialized =
                serde_json::to_string(&variant).expect("provider serializes to its toml token");
            assert_eq!(
                serialized,
                format!("\"{token}\""),
                "the toml spelling of {variant:?} is what a user writes",
            );
            assert!(
                Provider::ACCEPTED.contains(token),
                "ACCEPTED must name {token}, or the load error lists the wrong set",
            );
        }
    }
}
