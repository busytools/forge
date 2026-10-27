//! The account pool: the account state map behind one handle.
//!
//! Every operation the caller performs on account state goes through a
//! named method here. The callers never access the inner maps directly,
//! so the locking discipline is owned by this type and can change
//! without touching any call site.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use parking_lot::Mutex;

use crate::Provider;

use crate::UsageSnapshot;
use crate::account::{
    AccountKey, AccountStateMap, LoadingState, Unusable, UsageFetchStatus, account_serves,
};

/// The account state map, behind one handle.
pub struct AccountPool {
    accounts: Mutex<AccountStateMap>,
}

impl AccountPool {
    pub fn new(specs: &[forge_primitives::account::LoadedAccount]) -> Self {
        Self { accounts: Mutex::new(AccountStateMap::new(specs)) }
    }

    /// Empty map for the `testing` feature's `Workspace::testing_stub`.
    #[cfg(any(test, feature = "testing"))]
    pub fn empty_for_test() -> Self {
        Self { accounts: Mutex::new(AccountStateMap::empty_for_test()) }
    }

    pub fn all_loaded(&self) -> bool {
        self.accounts.lock().all_loaded()
    }

    pub fn usage(&self, display_name: &str) -> Option<UsageSnapshot> {
        self.accounts.lock().usage(&AccountKey(display_name.to_owned())).cloned()
    }

    pub fn usage_error(&self, display_name: &str) -> Option<UsageFetchStatus> {
        self.accounts.lock().usage_error(&AccountKey(display_name.to_owned()))
    }

    pub fn auth(&self, display_name: &str) -> Option<forge_primitives::account::AccountAuth> {
        self.accounts.lock().auth(&AccountKey(display_name.to_owned()))
    }

    pub fn env(&self, key: &AccountKey) -> Option<HashMap<String, String>> {
        self.accounts.lock().env(key).cloned()
    }

    pub fn provider(&self, key: &AccountKey) -> Option<forge_primitives::account::Provider> {
        self.accounts.lock().provider(key)
    }

    pub fn loading_state(&self, key: &AccountKey) -> LoadingState {
        self.accounts.lock().loading_state(key)
    }

    pub fn is_saturated(&self, key: &AccountKey) -> bool {
        self.accounts.lock().is_saturated(key)
    }

    pub fn unusable_reason(&self, key: &AccountKey) -> Option<Unusable> {
        self.accounts.lock().unusable_reason(key)
    }

    pub fn scheduler_should_probe(&self, key: &AccountKey) -> bool {
        self.accounts.lock().scheduler_should_probe(key)
    }

    pub fn should_probe_now(&self, key: &AccountKey) -> bool {
        self.accounts.lock().should_probe_now(key)
    }

    pub fn set_usage(&self, key: &AccountKey, snapshot: UsageSnapshot) {
        self.accounts.lock().set_usage(key, snapshot);
    }

    pub fn set_loading(&self, key: &AccountKey, loading: LoadingState) {
        self.accounts.lock().set_loading(key, loading);
    }

    pub fn set_last_error(
        &self,
        key: &AccountKey,
        status: UsageFetchStatus,
        retry_after: Option<Duration>,
    ) {
        self.accounts.lock().set_last_error(key, status, retry_after);
    }

    pub fn clear_last_error(&self, key: &AccountKey) {
        self.accounts.lock().clear_last_error(key);
    }

    pub fn disarm_override(&self, key: &AccountKey) {
        self.accounts.lock().disarm_override(key);
    }

    pub fn seed_from_cache(&self, cached: &BTreeMap<String, UsageSnapshot>) {
        self.accounts.lock().seed_from_cache(cached);
    }

    pub fn snapshots_for_cache(&self) -> BTreeMap<String, UsageSnapshot> {
        self.accounts.lock().snapshots_for_cache()
    }

    pub fn account_names(&self) -> Vec<String> {
        self.accounts.lock().ordered_keys.iter().map(|k| k.0.clone()).collect()
    }

    /// The account keys in declaration order.
    pub fn ordered_account_keys(&self) -> Vec<AccountKey> {
        self.accounts.lock().ordered_keys.clone()
    }

    /// How long until the account's next scheduled probe, when one is
    /// pending.
    pub fn next_probe_after(&self, key: &AccountKey) -> Option<Duration> {
        self.accounts
            .lock()
            .by_key
            .get(key)
            .and_then(|s| s.next_probe_at)
            .and_then(|t| t.checked_duration_since(std::time::Instant::now()))
    }

    /// The poller's work list: every account due a probe, with its
    /// provider and env.
    pub fn probe_entries(&self) -> Vec<(AccountKey, Provider, HashMap<String, String>)> {
        let state = self.accounts.lock();
        state
            .ordered_keys
            .iter()
            .filter(|key| state.scheduler_should_probe(key))
            .filter_map(|key| {
                let provider = state.provider(key)?;
                Some((key.clone(), provider, state.env(key).cloned().unwrap_or_default()))
            })
            .collect()
    }

    /// Disarm the one-shot reset-clear override for accounts whose
    /// backoff is genuinely active.
    pub fn disarm_backoff_overrides(&self, keys: &[AccountKey]) {
        let mut state = self.accounts.lock();
        for key in keys {
            if !state.should_probe_now(key) {
                state.disarm_override(key);
            }
        }
    }

    /// Run the declared-model selection walk for one org pin.
    pub fn select_account(
        &self,
        pin: &crate::selection::OrgPin,
        org: &str,
        model: &str,
    ) -> Result<AccountKey, crate::selection::SelectionError> {
        let state = self.accounts.lock();
        crate::selection::select_account(&state, pin, org, model)
    }

    /// `true` when the account serves `model`. An unknown account
    /// serves nothing.
    pub fn declares(&self, key: &AccountKey, model: &str) -> bool {
        self.accounts
            .lock()
            .by_key
            .get(key)
            .is_some_and(|account| account_serves(&account.models, &account.model_aliases, model))
    }

    /// The upstream slug the account maps `canonical` to, when the
    /// account declares a different upstream spelling. `None` forwards
    /// the canonical name unchanged.
    pub fn model_slug_for(&self, key: &AccountKey, canonical: &str) -> Option<String> {
        self.accounts.lock().by_key.get(key)?.model_slugs.get(canonical).cloned()
    }

    /// Set a slug mapping on one account. Test setup only - production
    /// slugs arrive through the config-loaded state map.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_model_slug(&self, key: &AccountKey, canonical: &str, slug: &str) {
        if let Some(account) = self.accounts.lock().by_key.get_mut(key) {
            account.model_slugs.insert(canonical.to_owned(), slug.to_owned());
        }
    }

    /// The provider and env of one account, for credential resolution.
    pub fn provider_and_env(
        &self,
        key: &AccountKey,
    ) -> Option<(Provider, HashMap<String, String>)> {
        let state = self.accounts.lock();
        let account = state.by_key.get(key)?;
        Some((account.provider, account.env.clone()))
    }

    /// Replace the whole state map. Test fixture setup only.
    #[cfg(any(test, feature = "testing"))]
    pub fn replace_state_for_test(&self, map: AccountStateMap) {
        *self.accounts.lock() = map;
    }

    /// Replace the account set with one a config reload produced.
    ///
    /// Health for an account present in both sets is carried across: a
    /// reload is not a reason to blank the usage bars, and resetting a
    /// terminal `LoadingState` would make a ready account unspawnable until
    /// its next probe. An account new to the set starts `Loading`.
    pub fn replace_accounts(&self, specs: &[forge_primitives::account::LoadedAccount]) {
        let mut current = self.accounts.lock();
        let mut next = AccountStateMap::new(specs);
        for key in &next.ordered_keys {
            let Some(previous) = current.by_key.get(key) else { continue };
            let Some(state) = next.by_key.get_mut(key) else { continue };
            state.usage.clone_from(&previous.usage);
            state.last_error.clone_from(&previous.last_error);
            state.next_probe_at = previous.next_probe_at;
            state.consecutive_failures = previous.consecutive_failures;
            state.override_armed = previous.override_armed;
            state.loading = previous.loading;
        }
        // An account the edit removed is retired rather than dropped: a
        // session already bound to it keeps resolving its provider and env,
        // while `select_account` skips a retired entry so no new selection
        // can pick it.
        for (key, previous) in &current.by_key {
            if next.by_key.contains_key(key) {
                continue;
            }
            let mut retired = previous.clone();
            retired.retired = true;
            next.by_key.insert(key.clone(), retired);
        }
        *current = next;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::SystemTime;

    use forge_primitives::account::{LoadedAccount, Provider};
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind};

    use super::AccountPool;
    use crate::account::AccountKey;

    fn account(name: &str) -> LoadedAccount {
        LoadedAccount {
            display_name: name.to_owned(),
            provider: Provider::Anthropic,
            base_url: None,
            models: vec!["test-model".to_owned()],
            model_slugs: HashMap::new(),
            model_aliases: HashMap::new(),
            env: HashMap::new(),
        }
    }

    fn snapshot() -> UsageSnapshot {
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: SystemTime::UNIX_EPOCH,
            five_hour: None,
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
            balance: None,
        }
    }

    /// A reload rebuilds the map; an account in both sets keeps its reading
    /// and its terminal loading state, so the edit does not blank the bars
    /// or refuse spawns until the next probe.
    #[test]
    fn a_reload_carries_surviving_accounts_health_across() {
        let pool = AccountPool::new(&[account("Kept"), account("Gone")]);
        let kept = AccountKey("Kept".to_owned());
        let gone = AccountKey("Gone".to_owned());
        pool.set_usage(&kept, snapshot());
        pool.set_loading(&kept, crate::LoadingState::Ready);

        pool.replace_accounts(&[account("Kept"), account("Added")]);

        assert!(pool.usage("Kept").is_some(), "the surviving account keeps its usage reading");
        assert_eq!(
            pool.loading_state(&kept),
            crate::LoadingState::Ready,
            "the surviving account stays ready, so a spawn is not refused after an edit",
        );
        assert_eq!(
            pool.loading_state(&AccountKey("Added".to_owned())),
            crate::LoadingState::Loading,
            "an account the reload added waits for its first probe",
        );
        assert!(
            pool.provider_and_env(&gone).is_some(),
            "a bound session still resolves a removed account's provider and env",
        );
    }

    /// The reload window a guard cannot close: a caller holding the old pin
    /// (which still names the account) selects against the new pool. The
    /// retired entry must not answer, or a brand-new session binds to an
    /// account the edit removed.
    #[test]
    fn a_retired_account_is_never_selected() {
        let pool = AccountPool::new(&[account("Kept"), account("Gone")]);
        let gone = AccountKey("Gone".to_owned());
        pool.set_loading(&gone, crate::LoadingState::Ready);
        pool.replace_accounts(&[account("Kept")]);

        let pin = crate::selection::OrgPin {
            accounts: vec!["Gone".to_owned()],
            fallback_accounts: Vec::new(),
        };
        let picked =
            crate::selection::select_account(&pool.accounts.lock(), &pin, "Org", "test-model");

        assert_eq!(
            picked,
            Err(crate::selection::SelectionError::NoEligibleAccount {
                model: "test-model".to_owned(),
                org: "Org".to_owned(),
            }),
            "a retired account is not offered to a new selection",
        );
    }
}
