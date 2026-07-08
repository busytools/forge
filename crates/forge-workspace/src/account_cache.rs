//! Persistent forge state: the `/spinner` override + the per-account
//! usage cache.
//!
//! Both live in the machine-local redb store ([`crate::store::state`]).
//! [`load`] / [`store`] / [`store_spinner`] are thin wrappers over that
//! tenant; [`ForgeState`] / [`CachedAccountUsage`] are the in-memory
//! shapes.
//!
//! The usage cache solves the cold-boot problem: Anthropic's
//! `/api/oauth/usage` endpoint rate-limits aggressively on per-IP burst
//! probes, so the first launch can wait 30 s+ before the warm probe gets
//! through - during which the launchpad picker ties at tier 0. The cache
//! seeds the in-memory `AccountStateMap` with the last known values until
//! the 60 s poller refreshes them. The spinner override is a user
//! preference with no such fallback.
//!
//! Failures are non-fatal: a closed store degrades to "no cache; spawn
//! paths see empty bars until the poller succeeds."

use forge_primitives::usage::UsageSnapshot;
use serde::{Deserialize, Serialize};

/// Per-account cache entry. The value type of the redb `account_usage`
/// table; reachable only in-crate since `account_cache` is a private mod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedAccountUsage {
    pub snapshot: UsageSnapshot,
}

/// In-memory forge state read from the redb store.
#[derive(Debug, Default)]
pub(crate) struct ForgeState {
    /// Runtime spinner-style override set via `/spinner`. `None` means no
    /// override - the active style falls back to forge.toml's `[ui]
    /// spinner` default.
    pub spinner: Option<crate::ui::SpinnerStyle>,
    /// Account display name to cached snapshot.
    pub account_usage: std::collections::BTreeMap<String, CachedAccountUsage>,
}

impl ForgeState {
    pub(crate) fn empty() -> Self {
        Self { spinner: None, account_usage: std::collections::BTreeMap::new() }
    }
}

/// Read the persisted forge state from the store. Each read degrades to
/// its empty default with a warn rather than failing the boot.
pub(crate) fn load(db: &crate::store::Db) -> ForgeState {
    ForgeState {
        spinner: crate::store::state::spinner(db).unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::account_cache",
                %error,
                "reading the spinner override from the store failed",
            );
            None
        }),
        account_usage: crate::store::state::account_usage(db).unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::account_cache",
                %error,
                "reading the account-usage cache from the store failed",
            );
            std::collections::BTreeMap::new()
        }),
    }
}

/// Persist the in-memory account-usage snapshots, replacing the prior
/// set. Backs the 60 s poller. Non-fatal + logged on failure.
pub(crate) fn store(
    db: &crate::store::Db,
    entries: &std::collections::BTreeMap<String, CachedAccountUsage>,
) {
    if let Err(error) = crate::store::state::replace_account_usage(db, entries) {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            %error,
            "persisting account usage to the store failed",
        );
    }
}

/// Persist the runtime spinner override (set via `/spinner`). `None`
/// clears it so the active style falls back to the forge.toml `[ui]
/// spinner` default. Non-fatal + logged on failure.
pub(crate) fn store_spinner(db: &crate::store::Db, spinner: Option<crate::ui::SpinnerStyle>) {
    if let Err(error) = crate::store::state::set_spinner(db, spinner) {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            %error,
            "persisting the spinner override to the store failed",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn cfg() -> tempfile::TempDir {
        tempdir().expect("cfg tempdir")
    }

    fn fixture_entry() -> CachedAccountUsage {
        CachedAccountUsage {
            snapshot: UsageSnapshot {
                source: UsageSourceKind::Oauth,
                fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                five_hour: Some(UsageWindow {
                    utilization: 42.0,
                    resets_at: None,
                    reset_description: None,
                }),
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
            },
        }
    }

    #[test]
    fn store_and_load_round_trip_through_redb() {
        let cfg = cfg();
        let db = crate::store::Db::open(&cfg.path().join("db.redb")).expect("open db");
        store_spinner(&db, Some(crate::ui::SpinnerStyle::Ember));
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Gateway".to_owned(), fixture_entry());
        store(&db, &entries);

        let loaded = load(&db);
        assert_eq!(loaded.spinner, Some(crate::ui::SpinnerStyle::Ember), "the spinner reloads");
        assert!(loaded.account_usage.contains_key("Gateway"), "the usage cache reloads");
    }

    #[test]
    fn store_spinner_none_clears_the_override() {
        let cfg = cfg();
        let db = crate::store::Db::open(&cfg.path().join("db.redb")).expect("open db");
        store_spinner(&db, Some(crate::ui::SpinnerStyle::Ember));
        store_spinner(&db, None);
        assert_eq!(load(&db).spinner, None, "None clears the persisted override");
    }
}
