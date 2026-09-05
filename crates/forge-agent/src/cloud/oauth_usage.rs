//! Provider probe entry points not yet behind the forge-providers
//! backends: the [`ProbePlan`] decision that routes each caller to
//! its backend. Each PR of #873 moves an arm into forge-providers;
//! this module shrinks until `ProbePlan` is deleted.

use std::collections::HashMap;

pub use forge_primitives::account::Provider;
pub use forge_primitives::usage::oauth::OauthUsageError;

use forge_providers::token_bearer;

/// True when `env` carries a non-empty `CLAUDE_CODE_OAUTH_TOKEN`.
/// A token-mode account has no keychain entry of its own - the config
/// dir is shared - so both the probe and preflight's repair copy branch
/// on this rather than on the provider alone.
pub fn is_token_mode<S: std::hash::BuildHasher>(env: &HashMap<String, String, S>) -> bool {
    token_bearer(env).is_some()
}

/// How an account's usage should be probed, derived once from its
/// declared [`Provider`]. The loader and poller both read this single
/// decision so the probe source AND the response-mapping strictness
/// stay in lockstep.
///
/// Deliberately not [`Provider`] itself: the Token variant carries a
/// bearer, so this type must not cross into a view the TUI renders.
/// `forge_workspace::AccountAuth` is the secret-free counterpart.
#[derive(Debug, PartialEq, Eq)]
pub enum ProbePlan {
    /// Normal Anthropic account: default host + macOS keychain bearer,
    /// strict mapping (a 200 must carry the five-hour window), and the
    /// CLI-spawn auth-recovery refresh on a 401. Codex, OpenRouter and
    /// Zai return this too, as a bare route selector for their backend
    /// - see [`probe_plan`].
    Keychain,
    /// Token-mode Anthropic account: default host + the
    /// `CLAUDE_CODE_OAUTH_TOKEN` setup token from `[accounts.env]`, no
    /// keychain read. A setup token carries `user:inference` but not
    /// the `user:profile` scope the usage endpoint requires, so a VALID
    /// token always answers 403 `oauth_scope_insufficient` - the probe
    /// settles that refusal to the empty payload,
    /// which maps leniently to a barless Ready snapshot. A 401 is a
    /// genuinely rejected token and still classifies `Unauthorized`.
    Token { bearer: String },
}

/// Derive the [`ProbePlan`] for an account from its declared
/// [`Provider`]. The provider alone decides the shape; a non-base-url
/// provider's setup token flips the plan to [`ProbePlan::Token`].
/// `ANTHROPIC_BASE_URL` decides nothing, because it answers where the
/// credential lives rather than what the backend bills for.
///
/// Codex, OpenRouter and Zai have no plan of their own: their
/// forge-providers backends derive the base-url credential from `env`
/// itself, so they return a bare [`ProbePlan::Keychain`] that only
/// selects the caller's backend-routed arm. The env-bearer repair
/// class is derived from the provider + env, never from this value.
pub fn probe_plan<S: std::hash::BuildHasher>(
    provider: Provider,
    env: &HashMap<String, String, S>,
) -> ProbePlan {
    if !provider.uses_base_url() {
        return match token_bearer(env) {
            Some(bearer) => ProbePlan::Token { bearer: bearer.to_owned() },
            None => ProbePlan::Keychain,
        };
    }
    ProbePlan::Keychain
}

#[cfg(test)]
mod tests {

    use super::*;

    /// OpenRouter has no plan of its own since its backend took the
    /// probe over: the value only selects the caller's backend-routed
    /// arm, so a base url in env does not produce a plan of its own.
    #[test]
    fn probe_plan_openrouter_is_a_bare_backend_route_selector() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "https://openrouter.ai/api".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-or-test".to_owned());
        assert_eq!(probe_plan(Provider::Openrouter, &env), ProbePlan::Keychain);
    }

    /// Codex has no plan of its own since its backend took the probe
    /// over: the value only selects the caller's backend-routed arm,
    /// so a base url in env does not produce a base-url plan.
    #[test]
    fn probe_plan_codex_is_a_bare_backend_route_selector() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-codex".to_owned());
        assert_eq!(probe_plan(Provider::Codex, &env), ProbePlan::Keychain);
    }

    /// A base url cannot decide the probe, because it answers where
    /// the credential lives rather than what the backend bills for.
    #[test]
    fn probe_plan_keys_on_provider_not_on_base_url() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-codex".to_owned());
        assert_eq!(
            probe_plan(Provider::Anthropic, &env),
            ProbePlan::Keychain,
            "an Anthropic account keeps the keychain even with a base url set",
        );
    }

    #[test]
    fn probe_plan_anthropic_is_keychain() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "sk-anything".to_owned());
        assert_eq!(probe_plan(Provider::Anthropic, &env), ProbePlan::Keychain);
        assert_eq!(probe_plan(Provider::Anthropic, &HashMap::new()), ProbePlan::Keychain);
    }

    /// A setup token in `[accounts.env]` is the account's credential, so
    /// the probe must authenticate with it instead of reading the
    /// keychain - whose entry for the shared config dir belongs to
    /// whichever account last logged in interactively, or to nobody.
    #[test]
    fn probe_plan_anthropic_setup_token_is_token_mode() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "setup-token".to_owned());
        assert_eq!(
            probe_plan(Provider::Anthropic, &env),
            ProbePlan::Token { bearer: "setup-token".to_owned() },
            "an env setup token must not fall through to the keychain plan",
        );
    }

    /// An empty CLAUDE_CODE_OAUTH_TOKEN must not flip the plan: a real
    /// keychain account with a stale empty var in its env block would
    /// otherwise lose its probe entirely.
    #[test]
    fn probe_plan_empty_setup_token_stays_keychain() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "  ".to_owned());
        assert_eq!(probe_plan(Provider::Anthropic, &env), ProbePlan::Keychain);
    }

    /// Zai has no plan of its own since its backend took the monitor
    /// probe over: the value only selects the caller's backend-routed
    /// arm, so a base url in env does not produce a plan of its own.
    #[test]
    fn probe_plan_zai_is_a_bare_backend_route_selector() {
        let mut env = HashMap::new();
        env.insert("ANTHROPIC_BASE_URL".to_owned(), "https://api.z.ai/api/anthropic".to_owned());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "zai-key".to_owned());
        assert_eq!(probe_plan(Provider::Zai, &env), ProbePlan::Keychain);
    }
}
