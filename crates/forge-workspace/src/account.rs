//! Account selection — internal to forge-workspace.
//!
//! `Workspace::get_agent_handle` consults `pick_for_project` on
//! every spawn; the chosen `AccountKey` becomes the spawned Agent's
//! `CLAUDE_CONFIG_DIR` override.
//!
//! **Policy — tiered, rate-limit aware:**
//!
//! 1. **Unknown-usage accounts** (no snapshot yet) sort first, in
//!    forge.toml definition order. Picking them warms the cache.
//! 2. **Available accounts** (5h util < 100% AND 7d util < 100%)
//!    sort next. Within this tier:
//!    - Lowest 5h utilisation wins (most immediate headroom).
//!    - Tie-break on lowest 7d utilisation.
//!    - Final tie-break on forge.toml definition order.
//! 3. **Rate-limited accounts** (either 5h or 7d at 100%) sort last.
//!    Definition order within this tier — picker still returns
//!    something so the spawn doesn't fail, but the spawned session
//!    will trip the rate limit immediately and surface that to the
//!    user. If the user's pin contains any non-rate-limited account
//!    it will always be preferred over this tier.
//!
//! No LRU, no round-robin, no fallback outside the project's pin.
//! Rate-limited accounts are visible in the panel bars so the user
//! can manually broaden the pin or wait for the window to reset.

use std::path::PathBuf;

use forge_primitives::usage::UsageSnapshot;

use crate::config::LoadedAccount;

/// Internal newtype wrapping the account's `display_name`.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub(crate) struct AccountKey(pub String);

/// Classification of the latest usage-poll attempt outcome for an
/// account. Surfaced to the TUI's bottom panel so empty bars can
/// distinguish "still warming the cache" from a real failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageFetchStatus {
    /// Anthropic returned HTTP 429 — too many concurrent polls
    /// against the OAuth `/api/oauth/usage` endpoint (typical when
    /// multiple forge instances poll from the same machine).
    RateLimited,
    /// OAuth credentials on disk are past their expires_at — needs
    /// `/login` to refresh.
    Expired,
    /// API returned 401/403 — token rejected (may be revoked).
    Unauthorized,
    /// Network failure reaching the API (DNS, TLS, timeout, …).
    NetworkFailed,
    /// Decode / unknown-HTTP-status fallthrough. Distinct from the
    /// above so renderers can show a generic "fetch error" when the
    /// cause doesn't map to a known bucket.
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct AccountState {
    pub config_dir: PathBuf,
    /// Latest usage snapshot fetched by the workspace's background
    /// poller. `None` until the first successful fetch. Drives the
    /// picker's order; also surfaced to the TUI's bottom panel via
    /// `Workspace::usage_for`.
    pub usage: Option<UsageSnapshot>,
    /// Latest poll-attempt outcome when the fetch failed. Cleared
    /// (set to `None`) on the next successful fetch. The TUI reads
    /// this via `Workspace::usage_error_for` to render a DIM hint
    /// (`rate-limited` / `expired` / …) next to a 5h/7d row whose
    /// bar can't fill because the underlying request didn't return
    /// data. Without it the empty bar reads like a forge bug rather
    /// than an upstream failure.
    pub last_error: Option<UsageFetchStatus>,
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
            by_key.insert(
                key,
                AccountState {
                    config_dir: account.config_dir.clone(),
                    usage: None,
                    last_error: None,
                },
            );
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

    /// Replace the cached usage snapshot for `key` and clear any
    /// stale `last_error`. Called from the background poller on a
    /// successful fetch. Silent no-op when `key` isn't registered
    /// (defensive — invariant says every poller key was inserted in
    /// `new()`).
    pub fn set_usage(&mut self, key: &AccountKey, snapshot: UsageSnapshot) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.usage = Some(snapshot);
            state.last_error = None;
        }
    }

    /// Record the latest poll-attempt failure for `key`. Called from
    /// the background poller's error branch. The cached `usage`
    /// snapshot is preserved so the panel keeps showing the last
    /// known-good bars (a fresh 429 doesn't blank them out).
    pub fn set_last_error(&mut self, key: &AccountKey, status: UsageFetchStatus) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.last_error = Some(status);
        }
    }

    /// Look up the cached usage snapshot for `key`. `None` when the
    /// poller hasn't yet succeeded for this account.
    pub fn usage(&self, key: &AccountKey) -> Option<&UsageSnapshot> {
        self.by_key.get(key).and_then(|s| s.usage.as_ref())
    }

    /// Look up the latest poll-attempt failure for `key`. `None`
    /// when the most recent attempt succeeded (or no attempt has
    /// been made yet).
    pub fn usage_error(&self, key: &AccountKey) -> Option<UsageFetchStatus> {
        self.by_key.get(key).and_then(|s| s.last_error)
    }

    /// Pick the best account within the project's pinned `allowed`
    /// subset using the tiered rate-limit-aware policy described in
    /// the module docs.
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
        let mut candidates: Vec<(usize, &AccountKey, Option<&UsageSnapshot>)> = allowed
            .iter()
            .enumerate()
            .filter_map(|(idx, name)| {
                // `find` rather than constructing an AccountKey then
                // looking up by reference — the keys vector is short
                // (one entry per [[accounts]]) so linear scan is fine
                // AND we get back a reference into `ordered_keys` for
                // stable lifetime.
                self.ordered_keys.iter().find(|k| k.0 == *name).map(|k| {
                    let usage = self.by_key.get(k).and_then(|s| s.usage.as_ref());
                    (idx, k, usage)
                })
            })
            .collect();
        candidates.sort_by(|a, b| {
            let tier_a = tier_of(a.2);
            let tier_b = tier_of(b.2);
            tier_a.cmp(&tier_b).then_with(|| {
                // Within the same tier: known-vs-known sorts by 5h
                // util ascending then 7d util ascending then
                // definition order. Both-unknown + mixed (which the
                // tier sort already segregated, defensive) fall
                // through to definition order.
                if let (Some(sa), Some(sb)) = (a.2, b.2) {
                    let f_a = five_hour_util(sa);
                    let f_b = five_hour_util(sb);
                    let s_a = seven_day_util(sa);
                    let s_b = seven_day_util(sb);
                    f_a.partial_cmp(&f_b)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| s_a.partial_cmp(&s_b).unwrap_or(std::cmp::Ordering::Equal))
                        .then_with(|| a.0.cmp(&b.0))
                } else {
                    a.0.cmp(&b.0)
                }
            })
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
        let dir = self.by_key.get(&picked).map_or_else(PathBuf::new, |s| s.config_dir.clone());
        (picked, dir)
    }
}

/// Tier classification driving the sort: 0 = unknown (warm cache),
/// 1 = available (under both windows), 2 = rate-limited (hit at
/// least one limit). Sorted ascending so unknown sorts before
/// available sorts before rate-limited.
fn tier_of(usage: Option<&UsageSnapshot>) -> u8 {
    match usage {
        None => 0,
        Some(snapshot) if is_rate_limited(snapshot) => 2,
        Some(_) => 1,
    }
}

/// True when either window has hit 100% utilisation. Such an account
/// will immediately trip the API rate limit on the next request, so
/// it's excluded from the "available" tier and only used as a
/// fallback when every pinned account is rate-limited.
fn is_rate_limited(snapshot: &UsageSnapshot) -> bool {
    five_hour_util(snapshot) >= 100.0 || seven_day_util(snapshot) >= 100.0
}

fn five_hour_util(snapshot: &UsageSnapshot) -> f64 {
    snapshot.five_hour.as_ref().map_or(0.0, |w| w.utilization)
}

/// Binding 7-day utilisation: max across the three 7-day windows
/// (`seven_day`, `seven_day_opus`, `seven_day_sonnet`). Whichever
/// is most-used is the binding constraint for "is this account
/// 7-day rate-limited."
fn seven_day_util(snapshot: &UsageSnapshot) -> f64 {
    let windows = [
        snapshot.seven_day.as_ref().map(|w| w.utilization),
        snapshot.seven_day_opus.as_ref().map(|w| w.utilization),
        snapshot.seven_day_sonnet.as_ref().map(|w| w.utilization),
    ];
    windows.into_iter().flatten().fold(0.0_f64, f64::max)
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
    fn picks_account_with_lowest_five_hour_when_neither_rate_limited() {
        // Stargate: 5h=80% — high
        // Gateway:  5h=10% — most headroom on the immediate window
        // Both under both windows. Picker uses lowest 5h.
        let mut map = AccountStateMap::new(&[make_account("Stargate"), make_account("Gateway")]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(80.0), Some(60.0)));
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(10.0), Some(20.0)));
        let (picked, _) = map.pick_for_project(&["Stargate".to_owned(), "Gateway".to_owned()]);
        assert_eq!(picked.0, "Gateway");
    }

    #[test]
    fn five_hour_is_the_primary_sort_key() {
        // Stargate: 5h=10%, 7d=90% — high 7d but immediate headroom
        // Gateway:  5h=50%, 7d=50%
        // Neither is rate-limited (both 7d windows < 100%), so the
        // 5h primary key wins for Stargate.
        let mut map = AccountStateMap::new(&[make_account("Stargate"), make_account("Gateway")]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(10.0), Some(90.0)));
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&["Stargate".to_owned(), "Gateway".to_owned()]);
        assert_eq!(picked.0, "Stargate");
    }

    #[test]
    fn rate_limited_account_excluded_in_favour_of_available_one() {
        // Gateway: 5h=0%, 7d=100% — RATE LIMITED on 7d.
        // Stargate: 5h=80%, 7d=80% — heavily used but neither at 100%.
        // Picker must exclude Gateway even though its 5h is lower.
        // This is the bug the user hit: 7d=100% was being ignored
        // by the old binding-pct algo.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Stargate")]);
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(0.0), Some(100.0)));
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(80.0), Some(80.0)));
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Stargate".to_owned()]);
        assert_eq!(picked.0, "Stargate");
    }

    #[test]
    fn rate_limited_account_excluded_on_five_hour_too() {
        // Gateway: 5h=100%, 7d=0% — rate limited on 5h.
        // Stargate: 5h=80%, 7d=80% — available.
        // Must pick Stargate.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Stargate")]);
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(100.0), Some(0.0)));
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(80.0), Some(80.0)));
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Stargate".to_owned()]);
        assert_eq!(picked.0, "Stargate");
    }

    #[test]
    fn unknown_usage_sorts_first_in_definition_order() {
        // Stargate has data; Gateway + Personal don't.
        // Picker picks Gateway (first unknown in definition order)
        // to warm the cache, even when Stargate looks healthy.
        let mut map = AccountStateMap::new(&[
            make_account("Gateway"),
            make_account("Stargate"),
            make_account("Personal"),
        ]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(10.0), Some(20.0)));
        let (picked, _) = map.pick_for_project(&[
            "Gateway".to_owned(),
            "Stargate".to_owned(),
            "Personal".to_owned(),
        ]);
        assert_eq!(picked.0, "Gateway");
    }

    #[test]
    fn all_rate_limited_falls_back_to_definition_order() {
        // Every pinned account hit at least one limit. Picker still
        // returns something (the spawn must not fail) — definition
        // order picks the first one.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Stargate")]);
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(100.0), Some(100.0)));
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(100.0), Some(100.0)));
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Stargate".to_owned()]);
        assert_eq!(picked.0, "Gateway");
    }

    #[test]
    fn available_wins_over_rate_limited_regardless_of_pin_order() {
        // Gateway is first in the pin AND rate-limited; Stargate is
        // second AND available. Tier sort must lift Stargate ahead.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Stargate")]);
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(50.0), Some(100.0)));
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(99.9), Some(99.9)));
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Stargate".to_owned()]);
        assert_eq!(picked.0, "Stargate");
    }

    #[test]
    fn pin_restricts_pool_to_subset() {
        // Three accounts globally; pin only two. Personal has the
        // most remaining (100%) but it's NOT in the pin — must be
        // excluded.
        let mut map = AccountStateMap::new(&[
            make_account("Stargate"),
            make_account("Gateway"),
            make_account("Personal"),
        ]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(50.0), Some(50.0)));
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(70.0), Some(70.0)));
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(0.0), Some(0.0)));
        let (picked, _) = map.pick_for_project(&["Stargate".to_owned(), "Gateway".to_owned()]);
        assert_eq!(picked.0, "Stargate");
        assert_ne!(picked.0, "Personal");
    }

    #[test]
    fn seven_day_tiebreak_when_five_hour_equal() {
        // Both at 5h=50, tiebreaker is 7d. Gateway has lower 7d so
        // it wins despite the alphabetic ordering.
        let mut map = AccountStateMap::new(&[make_account("Stargate"), make_account("Gateway")]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(50.0), Some(70.0)));
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(50.0), Some(30.0)));
        let (picked, _) = map.pick_for_project(&["Stargate".to_owned(), "Gateway".to_owned()]);
        assert_eq!(picked.0, "Gateway");
    }

    #[test]
    fn definition_order_final_tiebreak() {
        // Identical 5h + 7d → definition order. Stargate first in
        // the allow list wins.
        let mut map = AccountStateMap::new(&[make_account("Stargate"), make_account("Gateway")]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(50.0), Some(50.0)));
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&["Stargate".to_owned(), "Gateway".to_owned()]);
        assert_eq!(picked.0, "Stargate");
    }

    #[test]
    fn config_dir_lookup_returns_path() {
        let mut map = AccountStateMap::new(&[make_account("Stargate")]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(0.0), Some(0.0)));
        let dir = map.config_dir(&AccountKey("Stargate".to_owned()));
        assert_eq!(dir, Some(&PathBuf::from("/fake/Stargate")));
    }

    #[test]
    fn seven_day_util_takes_max_across_windows() {
        // seven_day = 30, seven_day_opus = 80 → binding = max = 80
        let mut s = snapshot(Some(20.0), Some(30.0));
        s.seven_day_opus = Some(UsageWindow {
            label: "7-day Opus",
            utilization: 80.0,
            resets_at: None,
            reset_description: None,
        });
        assert!((seven_day_util(&s) - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn is_rate_limited_fires_on_either_window() {
        assert!(is_rate_limited(&snapshot(Some(100.0), Some(0.0))));
        assert!(is_rate_limited(&snapshot(Some(0.0), Some(100.0))));
        assert!(is_rate_limited(&snapshot(Some(100.0), Some(100.0))));
        assert!(!is_rate_limited(&snapshot(Some(99.9), Some(99.9))));
    }
}
