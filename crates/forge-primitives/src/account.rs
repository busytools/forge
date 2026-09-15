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
    /// Anthropic proper: a setup-token credential, plan windows on the
    /// default host.
    Anthropic,
    /// A `claude-code-proxy` endpoint. Base-url like OpenRouter, but its
    /// proxy serves the same windowed `/api/oauth/usage` body Anthropic
    /// does, so it bills as a subscription.
    Codex,
    /// OpenRouter: pay-per-token, probed at `{base_url}/v1/key`. No
    /// plan windows and no allowance, so its usage is spend over a
    /// period rather than a percentage.
    Openrouter,
    /// Z.ai GLM coding plan: a credit-windowed subscription probed at
    /// the monitor host derived from the base url's host root. Chat
    /// rides the same env-override shape as the other base-url
    /// providers.
    Zai,
}

impl Provider {
    /// Every accepted `provider` value, for the load error a missing or
    /// unusable declaration produces.
    pub const ACCEPTED: &'static str = "\"anthropic\", \"codex\", \"openrouter\", \"zai\"";

    /// `true` when the account authenticates with a flat `token`
    /// beside a flat `base_url` (a base-url provider). Both the probe
    /// and preflight's repair copy branch on this rather than on the
    /// provider itself. The billing model lives on the provider's
    /// forge-gateway backend instead.
    pub const fn uses_base_url(self) -> bool {
        matches!(self, Self::Codex | Self::Openrouter | Self::Zai)
    }
}

/// One account as the config load produces it. The gateway's account
/// state is built from these, and the spawn path stamps the per-account
/// env onto the child from here.
#[derive(Debug)]
pub struct LoadedAccount {
    pub display_name: String,
    /// Declared backend. Drives the usage probe and the billing shape.
    /// See [`Provider`].
    pub provider: Provider,
    /// The flat `base_url` key. `None` for an Anthropic account: the
    /// gateway constant is its upstream.
    pub base_url: Option<String>,
    /// The canonical model names the account serves. Drives selection
    /// and the picker.
    pub models: Vec<String>,
    /// Canonical name -> upstream slug, only where the spellings
    /// differ. Keys are always members of [`Self::models`].
    pub model_slugs: std::collections::HashMap<String, String>,
    /// Provider-behaviour extras (timeouts, context caps, fallback
    /// switches) plus the derived credential and base-url variable
    /// stamps the gateway and the child read.
    pub env: std::collections::HashMap<String, String>,
}

/// How an account proves who it is, which is the only thing that
/// changes what preflight tells you to do about a failed one. Derived
/// from the provider plus the account's merged env; it carries the
/// distinction and none of the secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountAuth {
    /// The account authenticates with a flat `token` beside a flat
    /// `base_url` (the base-url providers). Repaired by editing the
    /// account block, which needs a restart; the 60 s usage poll
    /// recovers a transient bail.
    BaseUrl,
    /// The account's credential is a setup token, stamped onto
    /// `CLAUDE_CODE_OAUTH_TOKEN` - the only credential an Anthropic
    /// account has. Repaired by minting or re-minting the token on the
    /// account block, which needs a restart; the 60 s usage poll
    /// recovers a transient bail. An account whose block carries no
    /// token classifies here too: the repair it needs is the same.
    Token,
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
            (Provider::Zai, "zai"),
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

    #[test]
    fn zai_parses_from_its_toml_token() {
        let parsed: Provider = serde_json::from_str("\"zai\"").expect("zai parses");
        assert_eq!(parsed, Provider::Zai);
    }

    #[test]
    fn zai_rides_the_base_url_credential_shape() {
        assert!(
            Provider::Zai.uses_base_url(),
            "the credential is an ANTHROPIC_AUTH_TOKEN beside an ANTHROPIC_BASE_URL",
        );
    }
}
