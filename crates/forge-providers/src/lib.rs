//! `forge-providers` - one backend per `forge.toml` `provider`
//! token.
//!
//! Each [`ProviderBackend`] owns credential resolution, the probe
//! request and its payload mapping, and the billing shape for one
//! provider. The [`ProviderHost`] port, implemented by forge-agent,
//! is the only filesystem, keychain or process plumbing a backend may
//! reach, so this crate stays HTTP + mapping and is testable offline.

mod anthropic;
mod codex;
pub mod helpers;
pub mod model_catalog;
mod openrouter;
mod zai;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
pub use forge_primitives::account::Provider;
pub use forge_primitives::cloud::oauth_credentials::OauthCredentials;
pub use forge_primitives::usage::AccountBudget;
pub use forge_primitives::usage::UsageSnapshot;
pub use forge_primitives::usage::UsageSourceKind;
pub use forge_primitives::usage::oauth::OauthUsageError;

pub use crate::model_catalog::ModelCatalog;

pub use crate::anthropic::{Anthropic, token_bearer};
pub use crate::codex::Codex;
pub use crate::openrouter::Openrouter;
pub use crate::zai::Zai;

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

    /// The source kind this backend's probe stamps on the snapshots
    /// it returns; `budget` refuses a cached snapshot of any other
    /// kind.
    fn source(&self) -> UsageSourceKind;

    /// The account picker's budget shape for one account's cached
    /// usage snapshot.
    ///
    /// A snapshot whose source is not this backend's is treated as
    /// absent: the redb cache survives a `forge.toml` provider edit
    /// and is re-seeded at every boot, so a stale row is normal, and
    /// rendering windows as money (or the reverse) is worse than
    /// saying nothing.
    fn budget(&self, account: &str, snapshot: Option<&UsageSnapshot>) -> AccountBudget {
        let unknown =
            AccountBudget::Unknown { spend_billed: self.billing() == BillingModel::Spend };
        let Some(snapshot) = snapshot else {
            return unknown;
        };
        if snapshot.source != self.source() {
            warn_unusable_snapshot(account, self.token(), snapshot.source);
            return unknown;
        }
        match self.billing() {
            BillingModel::Windows => AccountBudget::Subscription {
                five_hour_util: snapshot.five_hour_util(),
                seven_day_util: snapshot.seven_day_util(),
                resets_at: snapshot.binding_reset_at(),
            },
            BillingModel::Spend => {
                if let Some(spend) = snapshot.spend.as_ref() {
                    AccountBudget::Api {
                        daily: spend.daily,
                        weekly: spend.weekly,
                        monthly: spend.monthly,
                    }
                } else {
                    // Unreachable from today's mapper, which refuses a
                    // body with no figures - same warn as a source
                    // mismatch rather than a second silent path.
                    warn_unusable_snapshot(account, self.token(), snapshot.source);
                    unknown
                }
            }
        }
    }

    /// The provider's model picker rows, or None to keep the
    /// discovered list. Today: openrouter only.
    fn model_catalog(&self) -> Option<&'static dyn ModelCatalog> {
        None
    }
}

/// A cached snapshot the renderer cannot use under this backend.
///
/// Warns rather than falling back quietly: the redb row is rewritten
/// only after a successful poll, so a stale one outlives a `forge.toml`
/// edit and is re-seeded every boot. An account whose provider changed
/// and whose new endpoint is failing would otherwise show empty columns
/// indefinitely with nothing anywhere saying why.
fn warn_unusable_snapshot(account: &str, provider: Provider, source: UsageSourceKind) {
    tracing::warn!(
        target: "forge_providers",
        account = %account,
        provider = ?provider,
        source = source.label(),
        "cached usage snapshot does not fit the account's provider; showing no figures until a \
         fresh probe lands",
    );
}

static ANTHROPIC: Anthropic = Anthropic;
static CODEX: Codex = Codex;
static OPENROUTER: Openrouter = Openrouter;
static ZAI: Zai = Zai;
static BACKENDS: &[&dyn ProviderBackend] = &[&ANTHROPIC, &CODEX, &OPENROUTER, &ZAI];

/// The backend registered for `token`. Every `Provider` variant
/// resolves; the Option keeps the trait open for a token whose
/// backend is not worth a probe.
pub fn backend(token: Provider) -> Option<&'static dyn ProviderBackend> {
    match token {
        Provider::Anthropic => Some(&ANTHROPIC),
        Provider::Codex => Some(&CODEX),
        Provider::Openrouter => Some(&OPENROUTER),
        Provider::Zai => Some(&ZAI),
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
        assert_eq!(backend.source(), UsageSourceKind::Oauth);
    }

    #[test]
    fn codex_backend_is_windowed() {
        let backend = backend(Provider::Codex).expect("registered");
        assert_eq!(backend.token(), Provider::Codex);
        assert_eq!(backend.billing(), BillingModel::Windows);
        assert_eq!(backend.source(), UsageSourceKind::Oauth);
    }

    #[test]
    fn openrouter_backend_is_spend() {
        let backend = backend(Provider::Openrouter).expect("registered");
        assert_eq!(backend.token(), Provider::Openrouter);
        assert_eq!(backend.billing(), BillingModel::Spend);
        assert_eq!(backend.source(), UsageSourceKind::OpenRouterKey);
    }

    #[test]
    fn zai_backend_is_windowed() {
        let backend = backend(Provider::Zai).expect("registered");
        assert_eq!(backend.token(), Provider::Zai);
        assert_eq!(backend.billing(), BillingModel::Windows);
        assert_eq!(backend.source(), UsageSourceKind::ZaiMonitor);
    }

    fn budget_snapshot(
        source: UsageSourceKind,
        spend: Option<forge_primitives::usage::ApiSpend>,
    ) -> UsageSnapshot {
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        let window = |utilization| forge_primitives::usage::UsageWindow {
            utilization,
            resets_at: Some(future),
            reset_description: None,
        };
        UsageSnapshot {
            source,
            fetched_at: std::time::SystemTime::UNIX_EPOCH,
            five_hour: Some(window(100.0)),
            seven_day: Some(window(20.0)),
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend,
        }
    }

    /// Every `(backend, source)` pair `budget` can be handed. The
    /// mismatch pairs are the reason the stale-cache refusal exists - a
    /// stale cached row survives a `forge.toml` provider change and is
    /// re-seeded at every boot - and a `budget` that keyed on billing
    /// alone would render windows as money with every other test still
    /// green.
    #[test]
    fn budget_covers_every_backend_and_source_pair() {
        use forge_primitives::usage::ApiSpend;

        let spend = ApiSpend {
            daily: 0.5,
            weekly: 1.0,
            monthly: 2.0,
            limit: None,
            limit_remaining: None,
            limit_reset: None,
            expires_at: None,
        };

        for backend in all() {
            let spend_billed = backend.billing() == BillingModel::Spend;
            let unknown = AccountBudget::Unknown { spend_billed };

            // No snapshot still carries the billing model, so the empty
            // row sits under the labels the account would really have.
            assert_eq!(backend.budget("Acct", None), unknown, "{:?}", backend.token());

            for source in [
                UsageSourceKind::Oauth,
                UsageSourceKind::OpenRouterKey,
                UsageSourceKind::ZaiMonitor,
            ] {
                let snapshot = budget_snapshot(source, Some(spend.clone()));
                let expected = if source == backend.source() {
                    match backend.billing() {
                        BillingModel::Windows => AccountBudget::Subscription {
                            five_hour_util: Some(100.0),
                            seven_day_util: Some(20.0),
                            resets_at: snapshot.binding_reset_at(),
                        },
                        BillingModel::Spend => {
                            AccountBudget::Api { daily: 0.5, weekly: 1.0, monthly: 2.0 }
                        }
                    }
                } else {
                    unknown.clone()
                };
                assert_eq!(
                    backend.budget("Acct", Some(&snapshot)),
                    expected,
                    "{:?} with a {:?} snapshot",
                    backend.token(),
                    source,
                );
            }

            // A spend backend whose matching-source snapshot carries no
            // figures maps to nothing - today's openrouter mapper
            // refuses such a body, so this takes the same warn path a
            // stale row takes.
            if spend_billed {
                let bare = budget_snapshot(backend.source(), None);
                assert_eq!(backend.budget("Acct", Some(&bare)), unknown);
            }
        }
    }
}
