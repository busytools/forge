//! Account selection — internal to forge-workspace.
//!
//! `Workspace::get_agent_handle` consults `pick_for_project` on
//! every spawn; the chosen `AccountKey` becomes the spawned Agent's
//! `CLAUDE_CONFIG_DIR` override.
//!
//! **One policy only**: for each candidate account in the project's
//! pinned `accounts = [...]` subset, compute the "binding remaining
//! pct" — `100 − max(5h_utilisation, 7d_utilisation_of_any_window)`
//! — and pick whichever has the most remaining. Unknown-usage
//! accounts (cache cold / fetch failed) sort BEFORE known ones so we
//! pick them first and warm the cache. No LRU, no round-robin, no
//! fallback outside the pin even when every pinned account is over
//! quota — the picker still picks the best of a bad set so the user
//! sees the bar fill and switches tier on their own.

use std::path::PathBuf;

use forge_primitives::usage::UsageSnapshot;

use crate::config::LoadedAccount;

/// Internal newtype wrapping the account's `display_name`.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub(crate) struct AccountKey(pub String);

#[derive(Debug, Clone)]
pub(crate) struct AccountState {
    pub config_dir: PathBuf,
    /// Latest usage snapshot fetched by the workspace's 30s
    /// background poller. `None` until the first successful fetch.
    /// Drives the picker's order; also surfaced to the TUI's
    /// bottom panel via `Workspace::usage_for`.
    pub usage: Option<UsageSnapshot>,
}

#[derive(Debug)]
pub(crate) struct AccountStateMap {
    pub ordered_keys: Vec<AccountKey>, // forge.toml definition order
    pub by_key: std::collections::HashMap<AccountKey, AccountState>,
}

impl AccountStateMap {
    /// Empty map for the `testing` feature's `Workspace::testing_stub`.
    /// Production code paths reach this map only via account pickers
    /// (`pick_for_project`), which a test fixture should never exercise.
    #[cfg(feature = "testing")]
    pub fn empty_for_test() -> Self {
        Self { ordered_keys: Vec::new(), by_key: std::collections::HashMap::new() }
    }

    pub fn new(accounts: &[LoadedAccount]) -> Self {
        let mut ordered_keys = Vec::with_capacity(accounts.len());
        let mut by_key = std::collections::HashMap::with_capacity(accounts.len());
        for account in accounts {
            let key = AccountKey(account.display_name.clone());
            ordered_keys.push(key.clone());
            by_key
                .insert(key, AccountState { config_dir: account.config_dir.clone(), usage: None });
        }
        Self { ordered_keys, by_key }
    }

    /// Look up the on-disk config_dir for `key`. Used by the
    /// background poller (which reads OAuth credentials from disk
    /// without spawning an Agent) and by the spawn path (which
    /// hands the dir to `Agent::spawn` as `CLAUDE_CONFIG_DIR`).
    pub fn config_dir(&self, key: &AccountKey) -> Option<&PathBuf> {
        self.by_key.get(key).map(|s| &s.config_dir)
    }

    /// Replace the cached usage snapshot for `key`. Called from the
    /// background poller. Silent no-op when `key` isn't registered
    /// (defensive — invariant says every poller key was inserted in
    /// `new()`).
    pub fn set_usage(&mut self, key: &AccountKey, snapshot: UsageSnapshot) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.usage = Some(snapshot);
        }
    }

    /// Look up the cached usage snapshot for `key`. `None` when the
    /// poller hasn't yet succeeded for this account.
    pub fn usage(&self, key: &AccountKey) -> Option<&UsageSnapshot> {
        self.by_key.get(key).and_then(|s| s.usage.as_ref())
    }

    /// Pick the account with the most remaining usage budget within
    /// the project's pinned `allowed` subset. Unknown-usage accounts
    /// sort first (in `allowed`-list order) so the picker forces
    /// data acquisition before settling on numbers. Within known
    /// accounts: sort by binding-remaining-pct descending, with
    /// alpha tie-break on `display_name` for stable ordering.
    ///
    /// Returns the picked key + its config_dir. The caller's spawn
    /// path uses the dir to seed `CLAUDE_CONFIG_DIR`.
    ///
    /// Panics: `allowed` must be non-empty AND every name must
    /// resolve to a key in `by_key` (config-load enforces both
    /// invariants). The defensive `unwrap_or_else` keeps the path
    /// out of an unreachable `panic!` form.
    pub fn pick_for_project(&self, allowed: &[String]) -> (AccountKey, PathBuf) {
        debug_assert!(!allowed.is_empty(), "pick_for_project requires a non-empty allow list");
        let mut candidates: Vec<(usize, &AccountKey, Option<f64>)> = allowed
            .iter()
            .enumerate()
            .filter_map(|(idx, name)| {
                // `find` rather than constructing an AccountKey then
                // looking up by reference — the keys vector is short
                // (one entry per [[accounts]]) so linear scan is fine
                // AND we get back a reference into `ordered_keys` for
                // stable lifetime.
                self.ordered_keys.iter().find(|k| k.0 == *name).map(|k| {
                    let remaining = self.by_key.get(k).and_then(|s| s.usage.as_ref()).map(remaining_pct);
                    (idx, k, remaining)
                })
            })
            .collect();
        candidates.sort_by(|a, b| match (a.2, b.2) {
            // Unknown first, preserving definition order via the
            // enumerate index.
            (None, None) => a.0.cmp(&b.0),
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            // Known: highest remaining first, alpha tie-break on name.
            (Some(a_rem), Some(b_rem)) => b_rem
                .partial_cmp(&a_rem)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.0.cmp(&b.1.0)),
        });
        // Invariant: candidates is non-empty because allowed is
        // non-empty and config-load validated every name exists.
        // Fallback path is structurally unreachable but never
        // panics — returns the first registered account.
        let picked = candidates.first().map_or_else(
            || {
                tracing::error!(
                    target: "forge_workspace::account",
                    "pick_for_project: candidates resolved to empty; allow list = {allowed:?}",
                );
                self.ordered_keys.first().cloned().unwrap_or(AccountKey(String::new()))
            },
            |(_, k, _)| (*k).clone(),
        );
        let dir = self
            .by_key
            .get(&picked)
            .map_or_else(PathBuf::new, |s| s.config_dir.clone());
        (picked, dir)
    }
}

/// Compute the binding remaining-pct for a usage snapshot:
/// `100 − max(5h_util, 7d_util_of_any_window)`. Saturating to
/// `[0, 100]`. The picker maximises this value.
///
/// 7-day utilization is taken as the maximum across `seven_day`,
/// `seven_day_opus`, and `seven_day_sonnet` windows — whichever is
/// most-used is the binding constraint. 5-hour is the single
/// `five_hour` window. The picker honours `min(5h_rem, 7d_rem)` in
/// spirit by using `max(5h_util, 7d_util)`: the more-used window
/// IS the binding constraint, and `100 - max` is the headroom.
fn remaining_pct(snapshot: &UsageSnapshot) -> f64 {
    let five = snapshot.five_hour.as_ref().map_or(0.0, |w| w.utilization);
    let seven_iter = [
        snapshot.seven_day.as_ref().map(|w| w.utilization),
        snapshot.seven_day_opus.as_ref().map(|w| w.utilization),
        snapshot.seven_day_sonnet.as_ref().map(|w| w.utilization),
    ];
    let seven = seven_iter.into_iter().flatten().fold(0.0_f64, f64::max);
    let bound: f64 = five.max(seven);
    (100.0_f64 - bound).clamp(0.0, 100.0)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
    use std::time::SystemTime;

    fn make_account(name: &str) -> LoadedAccount {
        LoadedAccount {
            display_name: name.to_owned(),
            config_dir: PathBuf::from(format!("/fake/{name}")),
        }
    }

    fn snapshot(five_hour: Option<f64>, seven_day: Option<f64>) -> UsageSnapshot {
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: SystemTime::UNIX_EPOCH,
            five_hour: five_hour.map(|util| UsageWindow {
                label: "5-hour",
                utilization: util,
                resets_at: None,
                reset_description: None,
            }),
            seven_day: seven_day.map(|util| UsageWindow {
                label: "7-day",
                utilization: util,
                resets_at: None,
                reset_description: None,
            }),
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
        }
    }

    #[test]
    fn picks_account_with_most_remaining_budget() {
        // Subspace: 80% used → 20% remaining
        // Granite:  10% used → 90% remaining
        // Picker must pick Granite.
        let mut map =
            AccountStateMap::new(&[make_account("Subspace"), make_account("Granite")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(80.0), Some(60.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(10.0), Some(20.0)));
        let (picked, _) = map.pick_for_project(&[
            "Subspace".to_owned(),
            "Granite".to_owned(),
        ]);
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn binding_window_is_the_more_constrained() {
        // Subspace: 5h 10% used, 7d 90% used → bound by 7d → 10% remaining
        // Granite:  5h 50% used, 7d 50% used → 50% remaining
        // Granite wins despite worse 5h because Subspace is pinned by 7d.
        let mut map =
            AccountStateMap::new(&[make_account("Subspace"), make_account("Granite")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(10.0), Some(90.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&[
            "Subspace".to_owned(),
            "Granite".to_owned(),
        ]);
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn unknown_usage_sorts_first_in_definition_order() {
        // Subspace has data; Granite + Personal don't.
        // Picker must pick Granite (first unknown in definition order).
        let mut map = AccountStateMap::new(&[
            make_account("Granite"),
            make_account("Subspace"),
            make_account("Personal"),
        ]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(10.0), Some(20.0)));
        let (picked, _) = map.pick_for_project(&[
            "Granite".to_owned(),
            "Subspace".to_owned(),
            "Personal".to_owned(),
        ]);
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn over_limit_account_still_picked_if_best_of_subset() {
        // Both accounts over-limit; pick the less-over one. No
        // fallback to outside the subset.
        let mut map =
            AccountStateMap::new(&[make_account("Subspace"), make_account("Granite")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(99.5), Some(50.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(100.0), Some(100.0)));
        let (picked, _) = map.pick_for_project(&[
            "Subspace".to_owned(),
            "Granite".to_owned(),
        ]);
        assert_eq!(picked.0, "Subspace");
    }

    #[test]
    fn pin_restricts_pool_to_subset() {
        // Three accounts globally; pin only two. Personal has the
        // most remaining (100%) but it's NOT in the pin — must be
        // excluded.
        let mut map = AccountStateMap::new(&[
            make_account("Subspace"),
            make_account("Granite"),
            make_account("Personal"),
        ]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(50.0), Some(50.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(70.0), Some(70.0)));
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(0.0), Some(0.0)));
        let (picked, _) =
            map.pick_for_project(&["Subspace".to_owned(), "Granite".to_owned()]);
        assert_eq!(picked.0, "Subspace");
        assert_ne!(picked.0, "Personal");
    }

    #[test]
    fn alpha_tie_break_when_remaining_pct_equal() {
        // Two accounts, identical remaining. Tie-break alphabetical
        // on display name.
        let mut map =
            AccountStateMap::new(&[make_account("Subspace"), make_account("Granite")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(50.0), Some(50.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&[
            "Subspace".to_owned(),
            "Granite".to_owned(),
        ]);
        assert_eq!(picked.0, "Granite", "alpha tie-break should pick Granite < Subspace");
    }

    #[test]
    fn config_dir_lookup_returns_path() {
        let mut map = AccountStateMap::new(&[make_account("Subspace")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(0.0), Some(0.0)));
        let dir = map.config_dir(&AccountKey("Subspace".to_owned()));
        assert_eq!(dir, Some(&PathBuf::from("/fake/Subspace")));
    }

    #[test]
    fn remaining_pct_clamps_to_zero_when_over_100() {
        // Defensive: if utilization > 100 the snapshot mapper would
        // already clamp, but the picker's math should also be safe.
        let s = snapshot(Some(100.0), Some(100.0));
        assert!((remaining_pct(&s) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn remaining_pct_uses_max_seven_day_window() {
        // seven_day = 30, seven_day_opus = 80 → binding = max = 80
        let mut s = snapshot(Some(20.0), Some(30.0));
        s.seven_day_opus = Some(UsageWindow {
            label: "7-day Opus",
            utilization: 80.0,
            resets_at: None,
            reset_description: None,
        });
        // bound = max(20, max(30, 80, 0)) = 80
        assert!((remaining_pct(&s) - 20.0).abs() < f64::EPSILON);
    }
}
