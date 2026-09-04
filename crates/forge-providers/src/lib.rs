//! `forge-providers` - one backend per `forge.toml` `provider`
//! token.
//!
//! Each [`ProviderBackend`] owns credential resolution, the probe
//! request and its payload mapping, and the billing shape for one
//! provider. The [`ProviderHost`] port, implemented by forge-agent,
//! is the only filesystem, keychain or process plumbing a backend may
//! reach, so this crate stays HTTP + mapping and is testable offline.

mod anthropic;
pub mod helpers;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
pub use forge_primitives::account::Provider;
pub use forge_primitives::cloud::oauth_credentials::OauthCredentials;
pub use forge_primitives::usage::UsageSnapshot;
pub use forge_primitives::usage::oauth::OauthUsageError;

pub use crate::anthropic::{Anthropic, token_bearer};

/// Everything a backend may read about one account. `env` is the
/// merged global `[env]` + `[accounts.env]` block; the merge happens
/// in forge-workspace config load and stays there.
pub struct AccountEnv<'a> {
    pub config_dir: &'a Path,
    pub env: &'a HashMap<String, String>,
}

/// Billing shape: plan windows that reset (a percentage of an
/// allowance) or per-key spend over a period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BillingModel {
    Windows,
    Spend,
}

/// Why a probe did not produce a snapshot. `Fetch` keeps the wire
/// error classes shared with the providers not yet migrated behind
/// the trait; `Unmappable` is a 200 whose body maps to nothing.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("no credentials for the keychain plan")]
    NoCredentials,
    #[error(transparent)]
    Fetch(#[from] OauthUsageError),
    #[error("probe returned 200 but the body maps to nothing: {0}")]
    Unmappable(String),
}

/// The host port, implemented by forge-agent. The only filesystem,
/// keychain or process plumbing a backend may reach, so the crate
/// stays HTTP + mapping and is testable offline.
#[async_trait]
pub trait ProviderHost: Send + Sync {
    /// The macOS keychain entry for `config_dir`, or None.
    fn keychain(&self, config_dir: &Path) -> Option<OauthCredentials>;
    /// A reqwest client with the NODE_EXTRA_CA_CERTS roots applied
    /// and the caller's timeout baked in.
    fn http_client(&self, timeout: Duration) -> Result<reqwest::Client, String>;
    /// `claude-code/<version>`, resolved by one `claude --version`
    /// shell-out per process and cached, off the async runtime via
    /// spawn_blocking. Err preserves the probe's UaProbe failure
    /// class.
    async fn user_agent(&self) -> Result<String, String>;
}

/// One backend per `forge.toml` `provider` token. The impls are
/// stateless unit structs; per-account state arrives through
/// [`AccountEnv`], never through `self`.
#[async_trait]
pub trait ProviderBackend: Send + Sync {
    /// The token this backend serves; the registry keys on it.
    fn token(&self) -> Provider;

    /// One probe round-trip: credential resolution, URL derivation,
    /// headers, status classification, payload-to-snapshot mapping.
    /// Strict-vs-lenient mapping lives in the impls, not in a flag.
    async fn probe(
        &self,
        account: &AccountEnv<'_>,
        host: &dyn ProviderHost,
    ) -> Result<UsageSnapshot, ProbeError>;

    /// Billing shape.
    fn billing(&self) -> BillingModel;
}

static ANTHROPIC: Anthropic = Anthropic;
static BACKENDS: &[&dyn ProviderBackend] = &[&ANTHROPIC];

/// The backend registered for `token`, or None while the wave that
/// migrates each provider onto the trait is still in flight.
pub fn backend(token: Provider) -> Option<&'static dyn ProviderBackend> {
    match token {
        Provider::Anthropic => Some(&ANTHROPIC),
        Provider::Codex | Provider::Openrouter | Provider::Zai => None,
    }
}

/// Every registered backend.
pub fn all() -> &'static [&'static dyn ProviderBackend] {
    BACKENDS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_round_trips_every_registered_backend() {
        for entry in all() {
            let resolved = backend(entry.token());
            assert_eq!(
                resolved.map(ProviderBackend::token),
                Some(entry.token()),
                "backend() must resolve every backend all() lists",
            );
        }
    }

    #[test]
    fn anthropic_backend_is_windowed() {
        let backend = backend(Provider::Anthropic).expect("registered");
        assert_eq!(backend.token(), Provider::Anthropic);
        assert_eq!(backend.billing(), BillingModel::Windows);
    }

    #[test]
    fn unregistered_tokens_resolve_none() {
        assert!(backend(Provider::Codex).is_none());
        assert!(backend(Provider::Openrouter).is_none());
        assert!(backend(Provider::Zai).is_none());
    }
}
