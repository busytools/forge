//! Account selection: which account serves a session the gateway has
//! no binding for.
//!
//! The walk is the org's `accounts` list then `fallback_accounts`,
//! filtered by declared models: the first account whose `models` list
//! includes the requested model wins. Within the surviving set the
//! first account in walk order wins, with saturation demoting an
//! account to a later tier: ready beats saturated, and a Bailed
//! account is the last resort (its 429 hit the usage probe, not
//! inference).

use crate::account::{AccountKey, AccountStateMap, LoadingState};

/// An org's walk order: the primary pin, then fallbacks.
#[derive(Debug, Clone, Default)]
pub struct OrgPin {
    pub accounts: Vec<String>,
    pub fallback_accounts: Vec<String>,
}

/// Why no account was selected. Loud by design: a silent fallback
/// would route a session to an account its model cannot use.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectionError {
    #[error("no account in org '{org}' serves model '{model}'")]
    NoEligibleAccount { model: String, org: String },
}

/// `true` when the account declares `model` among the models it
/// serves.
fn declares(state: &AccountStateMap, key: &AccountKey, model: &str) -> bool {
    state.by_key.get(key).is_some_and(|account| account.models.iter().any(|m| m == model))
}

/// Pick the account for `model` out of `org`'s walk order: the first
/// account whose declared models include the model. Ready accounts
/// beat saturated ones, and Bailed accounts are the last resort.
pub fn select_account(
    state: &AccountStateMap,
    pin: &OrgPin,
    org: &str,
    model: &str,
) -> Result<AccountKey, SelectionError> {
    let mut ready: Option<AccountKey> = None;
    let mut saturated: Option<AccountKey> = None;
    let mut degraded: Option<AccountKey> = None;

    for name in pin.accounts.iter().chain(pin.fallback_accounts.iter()) {
        let key = AccountKey(name.clone());
        let Some(account) = state.by_key.get(&key) else {
            continue;
        };
        if !declares(state, &key, model) {
            continue;
        }
        match account.loading {
            LoadingState::Loading => continue,
            LoadingState::Ready if state.is_saturated(&key) => {
                if saturated.is_none() {
                    saturated = Some(key);
                }
            }
            LoadingState::Ready => {
                if ready.is_none() {
                    ready = Some(key);
                }
            }
            LoadingState::Bailed => {
                if degraded.is_none() {
                    degraded = Some(key);
                }
            }
        }
        if ready.is_some() {
            // Walk order decides: the first ready account in the org's
            // own order is the assignment, exactly as the plan's tier
            // walk would produce.
            break;
        }
    }

    ready.or(saturated).or(degraded).ok_or_else(|| SelectionError::NoEligibleAccount {
        model: model.to_owned(),
        org: org.to_owned(),
    })
}

/// The org's walk order narrowed to the accounts that declare `model`,
/// preserving pin order. Both lists can come back empty; the caller
/// decides whether that is fatal.
pub fn org_lists_for_model(state: &AccountStateMap, pin: &OrgPin, model: &str) -> OrgPin {
    let keep = |names: &[String]| {
        names
            .iter()
            .filter(|name| declares(state, &AccountKey((*name).clone()), model))
            .cloned()
            .collect::<Vec<String>>()
    };
    OrgPin { accounts: keep(&pin.accounts), fallback_accounts: keep(&pin.fallback_accounts) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;
    use crate::account::UsageFetchStatus;
    use std::collections::HashMap;
    use std::time::Duration;

    fn window(resets_at: Option<std::time::SystemTime>) -> forge_primitives::usage::UsageWindow {
        forge_primitives::usage::UsageWindow {
            utilization: 100.0,
            resets_at,
            reset_description: None,
        }
    }

    /// One fixture account: name, provider, loading state, last probe
    /// error, and the models it declares.
    type Spec<'a> = (&'a str, Provider, LoadingState, Option<UsageFetchStatus>, Vec<&'a str>);

    fn pool_with(accounts: &[Spec<'_>]) -> AccountStateMap {
        let specs: Vec<forge_primitives::account::LoadedAccount> = accounts
            .iter()
            .map(|(name, provider, _, _, models)| forge_primitives::account::LoadedAccount {
                display_name: (*name).to_owned(),
                provider: *provider,
                base_url: None,
                models: models.iter().map(|m| (*m).to_owned()).collect(),
                model_slugs: std::collections::HashMap::new(),
                env: HashMap::new(),
            })
            .collect();
        let mut state = AccountStateMap::new(&specs);
        for (name, _, loading, error, _) in accounts {
            let key = AccountKey((*name).to_owned());
            state.set_loading(&key, *loading);
            if let Some(error) = error {
                state.set_last_error(&key, *error, None);
            }
        }
        state
    }

    fn pin(accounts: &[&str], fallbacks: &[&str]) -> OrgPin {
        OrgPin {
            accounts: accounts.iter().map(|s| (*s).to_owned()).collect(),
            fallback_accounts: fallbacks.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn only_an_account_declaring_the_model_is_selected() {
        let state = pool_with(&[
            ("Openrouter", Provider::Openrouter, LoadingState::Ready, None, vec!["glm-5.3-flash"]),
            ("Anthropic", Provider::Anthropic, LoadingState::Ready, None, vec!["claude-opus-5"]),
        ]);
        let selected = select_account(
            &state,
            &pin(&["Openrouter", "Anthropic"], &[]),
            "Default",
            "claude-opus-5",
        )
        .expect("the anthropic account declares the model");
        assert_eq!(
            selected,
            AccountKey("Anthropic".to_owned()),
            "the walk skips accounts that do not declare the model",
        );
    }

    #[test]
    fn the_first_account_declaring_the_model_wins() {
        let state = pool_with(&[
            ("Zai", Provider::Zai, LoadingState::Ready, None, vec!["glm-5.3-flash"]),
            ("Openrouter", Provider::Openrouter, LoadingState::Ready, None, vec!["glm-5.3-flash"]),
        ]);
        let selected =
            select_account(&state, &pin(&["Zai", "Openrouter"], &[]), "Default", "glm-5.3-flash")
                .expect("both accounts declare the model");
        assert_eq!(
            selected,
            AccountKey("Zai".to_owned()),
            "first in walk order, not a guessed best",
        );
    }

    #[test]
    fn a_non_declaring_account_is_skipped() {
        let state = pool_with(&[
            ("Anthropic", Provider::Anthropic, LoadingState::Ready, None, vec!["claude-opus-5"]),
            ("Zai", Provider::Zai, LoadingState::Ready, None, vec!["glm-5.3-flash"]),
        ]);
        let selected =
            select_account(&state, &pin(&["Anthropic", "Zai"], &[]), "Default", "glm-5.3-flash")
                .expect("a declaring account is in the walk");
        assert_eq!(selected, AccountKey("Zai".to_owned()));
    }

    #[test]
    fn no_declaring_account_fails_naming_the_model_and_org() {
        let state = pool_with(&[(
            "Anthropic",
            Provider::Anthropic,
            LoadingState::Ready,
            None,
            vec!["claude-opus-5"],
        )]);
        let error = select_account(&state, &pin(&["Anthropic"], &[]), "Default", "glm-5.3-flash")
            .expect_err("no account declares the model");
        assert_eq!(
            error,
            SelectionError::NoEligibleAccount {
                model: "glm-5.3-flash".to_owned(),
                org: "Default".to_owned(),
            },
        );
    }

    #[test]
    fn saturated_accounts_are_demoted_but_not_dropped() {
        let mut state = pool_with(&[
            ("Stargate", Provider::Anthropic, LoadingState::Ready, None, vec!["claude-opus-5"]),
            ("Gateway", Provider::Anthropic, LoadingState::Ready, None, vec!["claude-opus-5"]),
        ]);
        // Push Stargate over the cap: a full window with a future reset.
        let key = AccountKey("Stargate".to_owned());
        state.set_usage(
            &key,
            crate::UsageSnapshot {
                source: crate::UsageSourceKind::Oauth,
                fetched_at: std::time::SystemTime::UNIX_EPOCH,
                five_hour: Some(window(Some(
                    std::time::SystemTime::now() + Duration::from_secs(3600),
                ))),
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
                spend: None,
                balance: None,
            },
        );
        let selected =
            select_account(&state, &pin(&["Stargate", "Gateway"], &[]), "Default", "claude-opus-5")
                .expect("a ready account exists");
        assert_eq!(selected, AccountKey("Gateway".to_owned()), "ready beats saturated");
    }

    #[test]
    fn fallback_accounts_are_walked_after_the_primaries() {
        // Same tier on both tiers of the pin: walk order decides, and
        // the primaries come first.
        let state = pool_with(&[
            ("Primary", Provider::Anthropic, LoadingState::Bailed, None, vec!["claude-opus-5"]),
            ("Fallback", Provider::Anthropic, LoadingState::Bailed, None, vec!["claude-opus-5"]),
        ]);
        let selected =
            select_account(&state, &pin(&["Primary"], &["Fallback"]), "Default", "claude-opus-5")
                .expect("the degraded tier is the last resort");
        assert_eq!(selected, AccountKey("Primary".to_owned()), "within a tier, walk order decides");

        // A ready fallback beats a degraded primary: state outranks
        // tier, which is what sends a session to a live fallback
        // instead of a dead primary.
        let state = pool_with(&[
            ("Primary", Provider::Anthropic, LoadingState::Bailed, None, vec!["claude-opus-5"]),
            ("Fallback", Provider::Anthropic, LoadingState::Ready, None, vec!["claude-opus-5"]),
        ]);
        let selected =
            select_account(&state, &pin(&["Primary"], &["Fallback"]), "Default", "claude-opus-5")
                .expect("the ready fallback is selectable");
        assert_eq!(
            selected,
            AccountKey("Fallback".to_owned()),
            "a ready fallback outranks a degraded primary",
        );
    }

    #[test]
    fn loading_accounts_are_skipped_entirely() {
        let state = pool_with(&[
            ("Stargate", Provider::Anthropic, LoadingState::Loading, None, vec!["claude-opus-5"]),
            ("Gateway", Provider::Anthropic, LoadingState::Ready, None, vec!["claude-opus-5"]),
        ]);
        let selected =
            select_account(&state, &pin(&["Stargate", "Gateway"], &[]), "Default", "claude-opus-5")
                .expect("a terminal account exists");
        assert_eq!(
            selected,
            AccountKey("Gateway".to_owned()),
            "a non-terminal account is not selectable",
        );
    }

    #[test]
    fn an_unknown_account_name_in_the_pin_is_skipped() {
        let state = pool_with(&[(
            "Stargate",
            Provider::Anthropic,
            LoadingState::Ready,
            None,
            vec!["claude-opus-5"],
        )]);
        let selected =
            select_account(&state, &pin(&["Ghost", "Stargate"], &[]), "Default", "claude-opus-5")
                .expect("the known account resolves");
        assert_eq!(selected, AccountKey("Stargate".to_owned()));
    }

    #[test]
    fn org_lists_for_model_keeps_only_declaring_accounts_in_order() {
        let state = pool_with(&[
            ("Personal", Provider::Anthropic, LoadingState::Ready, None, vec!["claude-opus-5"]),
            (
                "OpenRouter-TM",
                Provider::Openrouter,
                LoadingState::Ready,
                None,
                vec!["deepseek-v4.1-flash"],
            ),
            (
                "OpenRouter",
                Provider::Openrouter,
                LoadingState::Ready,
                None,
                vec!["deepseek-v4.1-flash"],
            ),
        ]);
        let pin = OrgPin {
            accounts: vec!["Personal".into(), "OpenRouter-TM".into()],
            fallback_accounts: vec!["OpenRouter".into()],
        };
        let filtered = org_lists_for_model(&state, &pin, "deepseek-v4.1-flash");
        assert_eq!(filtered.accounts, vec!["OpenRouter-TM".to_owned()]);
        assert_eq!(filtered.fallback_accounts, vec!["OpenRouter".to_owned()]);
    }

    #[test]
    fn org_lists_for_model_is_empty_when_nothing_declares_the_model() {
        let state = pool_with(&[(
            "Personal",
            Provider::Anthropic,
            LoadingState::Ready,
            None,
            vec!["claude-opus-5"],
        )]);
        let pin = OrgPin { accounts: vec!["Personal".into()], fallback_accounts: vec![] };
        let filtered = org_lists_for_model(&state, &pin, "deepseek-v4.1-flash");
        assert!(filtered.accounts.is_empty(), "no account declares the model");
        assert!(filtered.fallback_accounts.is_empty(), "no fallback declares the model");
    }
}
