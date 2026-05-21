//! Account selection — internal to forge-workspace.
//!
//! `Workspace::get_agent_handle` consults `pick_for_project` on
//! every spawn; the chosen `AccountKey` becomes the spawned Agent's
//! `CLAUDE_CONFIG_DIR` override.
//!
//! **Policy — pin priority with rate-limit + probe-failure skip:**
//!
//! Sort all candidates by tier, then by `forge.toml` definition
//! order within the tier. The pin is authoritative — the picker
//! never load-balances on utilisation. A pin of
//! `[Granite, Granite1, Personal]` means "always pick Granite when
//! it's healthy; only fall through to Granite1 when Granite is
//! saturated; only reach Personal when both above are blocked."
//!
//! Tiers (lower = preferred):
//!
//! 1. **Unknown-fresh** (no usage snapshot AND no probe failure
//!    recorded). Picks warm the cache.
//! 2. **Available** (5h util < 100% AND 7d util < 100%). The
//!    normal "use this one" tier.
//! 3. **Rate-limited with data** (5h or 7d at 100%). Cleanly
//!    skipped in favour of any healthier pin entry.
//! 4. **Probe rate-limited** (`/api/oauth/usage` returned 429).
//!    Account state is opaque — almost certainly hot. Demoted
//!    below known-rate-limited because we'd rather pick the
//!    devil-we-know.
//! 5. **Probe failed transiently** (network / unknown HTTP).
//!    Demoted below probe-rate-limited but above expired creds.
//! 6. **Credentials broken** (Expired / Unauthorized). The
//!    account literally cannot serve a request — last-resort
//!    only, so the spawn at least returns something rather than
//!    blocking on an empty pool.
//!
//! Tier 0 splits truly-cold (no probe yet) from perpetually-failing
//! (probe failed) so a broken account doesn't pin at top-of-sort
//! and starve the picker.
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
    /// Whether sessions for this account spawn with the
    /// wire-classification rewriter proxy attached. Mirrors the
    /// `[[accounts]] proxy = true|false` toggle in forge.toml.
    /// Defaults to `true` when the field is absent.
    pub proxy: bool,
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
    /// Earliest wall-clock time we're allowed to probe this account
    /// again. The poller skips accounts whose `next_probe_at` is in
    /// the future. Used for per-account exponential backoff when
    /// Anthropic's `/api/oauth/usage` endpoint keeps returning 429
    /// — without backoff every poll cycle re-trips the per-IP rate
    /// limit and we never accumulate usage data. `None` means
    /// "probe on the next tick" (default).
    pub next_probe_at: Option<std::time::Instant>,
    /// Consecutive probe failures since the last success. Drives
    /// the backoff schedule: each consecutive failure doubles the
    /// next-probe delay (capped at a sensible ceiling). Reset to
    /// 0 on success.
    pub consecutive_failures: u32,
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
                    proxy: account.proxy,
                    usage: None,
                    last_error: None,
                    next_probe_at: None,
                    consecutive_failures: 0,
                },
            );
        }
        Self { ordered_keys, by_key }
    }

    /// Seed the in-memory map from a previously-persisted cache.
    /// Each known account that appears in `cached` gets its `usage`
    /// populated; unknown cache entries (account removed from
    /// forge.toml) are ignored. Does NOT clear `last_error` or
    /// `next_probe_at` — the cache is purely seed data; the live
    /// poller still drives backoff. Used by `Workspace::new` to make
    /// the launchpad picker non-empty on cold boot.
    pub fn seed_from_cache(
        &mut self,
        cached: &std::collections::BTreeMap<String, crate::account_cache::CachedAccountUsage>,
    ) {
        for (name, entry) in cached {
            let key = AccountKey(name.clone());
            if let Some(state) = self.by_key.get_mut(&key) {
                state.usage = Some(entry.snapshot.clone());
            }
        }
    }

    /// Snapshot the per-account `usage` for writing to the on-disk
    /// cache. Accounts with no live snapshot are omitted so the cache
    /// file doesn't grow placeholders.
    pub fn snapshots_for_cache(
        &self,
    ) -> std::collections::BTreeMap<String, crate::account_cache::CachedAccountUsage> {
        let mut out = std::collections::BTreeMap::new();
        for (key, state) in &self.by_key {
            if let Some(snapshot) = state.usage.clone() {
                out.insert(
                    key.0.clone(),
                    crate::account_cache::CachedAccountUsage {
                        snapshot,
                        cached_at: std::time::SystemTime::now(),
                    },
                );
            }
        }
        out
    }

    /// Look up the on-disk config_dir for `key`. Used by the
    /// background poller (which reads OAuth credentials from disk
    /// without spawning an Agent) and by the spawn path (which
    /// hands the dir to `Agent::spawn` as `CLAUDE_CONFIG_DIR`).
    pub fn config_dir(&self, key: &AccountKey) -> Option<&PathBuf> {
        self.by_key.get(key).map(|s| &s.config_dir)
    }

    /// Whether sessions for `key` should spawn with the rewriter
    /// proxy attached. Returns `false` for unknown keys (defensive;
    /// the spawn path's normal-case key always resolves).
    pub fn proxy_enabled(&self, key: &AccountKey) -> bool {
        self.by_key.get(key).is_some_and(|s| s.proxy)
    }

    /// Replace the cached usage snapshot for `key` and clear any
    /// stale `last_error`. Resets the consecutive-failure counter
    /// and clears the next-probe gate so this account returns to
    /// the default cadence. Silent no-op when `key` isn't registered
    /// (defensive — invariant says every poller key was inserted in
    /// `new()`).
    pub fn set_usage(&mut self, key: &AccountKey, snapshot: UsageSnapshot) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.usage = Some(snapshot);
            state.last_error = None;
            state.next_probe_at = None;
            state.consecutive_failures = 0;
        }
    }

    /// Record the latest poll-attempt failure for `key` and schedule
    /// the next probe.
    ///
    /// `retry_after` (when `Some`) is the server-provided hold-down
    /// duration — typically Anthropic's `Retry-After` header on 429.
    /// We honour it verbatim because the server knows when its
    /// per-account bucket will reset; guessing with our own backoff
    /// either over- or under-shoots and keeps the limit hot. When
    /// `None` (network failures, unknown HTTP status, server didn't
    /// send Retry-After), fall back to a local exponential schedule.
    ///
    /// The cached `usage` snapshot is preserved so the panel keeps
    /// showing the last known-good bars (a fresh 429 doesn't blank
    /// them out).
    pub fn set_last_error(
        &mut self,
        key: &AccountKey,
        status: UsageFetchStatus,
        retry_after: Option<std::time::Duration>,
    ) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.last_error = Some(status);
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            // Anthropic returns `Retry-After: 0` on the /api/oauth/usage
            // 429 path, which is "no specific hint" rather than "you can
            // retry now". Trusting it literally schedules next_probe_at
            // at the same instant and the next round re-trips the same
            // rate limit — that's the burst-probe behaviour the warm
            // loop kept hitting. Treat any sub-second hint as missing
            // and fall through to the exponential default
            // (starts at 30 s), so the leaky-bucket actually gets
            // a chance to refill.
            let delay = match retry_after {
                Some(d) if d >= std::time::Duration::from_secs(1) => d,
                _ => backoff_delay(state.consecutive_failures),
            };
            state.next_probe_at = Some(std::time::Instant::now() + delay);
        }
    }

    /// `true` when the poller may probe `key` now. `false` when the
    /// account is in an active backoff window from a recent failure.
    /// Cold-cache accounts (no last_error) always return `true`.
    pub fn should_probe_now(&self, key: &AccountKey) -> bool {
        let Some(state) = self.by_key.get(key) else { return true };
        state.next_probe_at.is_none_or(|t| t <= std::time::Instant::now())
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
        // Carry (def_order_idx, key, usage, last_error) per
        // candidate so the tier sort can see probe failures (not
        // just usage data). Without the last_error consult, an
        // account whose probe perpetually 429s or has expired
        // OAuth credentials stays at tier 0 (Unknown) forever and
        // dominates the sort over accounts with healthy probes.
        let mut candidates: Vec<(
            usize,
            &AccountKey,
            Option<&UsageSnapshot>,
            Option<UsageFetchStatus>,
        )> = allowed
            .iter()
            .enumerate()
            .filter_map(|(idx, name)| {
                self.ordered_keys.iter().find(|k| k.0 == *name).map(|k| {
                    let state = self.by_key.get(k);
                    let usage = state.and_then(|s| s.usage.as_ref());
                    let last_error = state.and_then(|s| s.last_error);
                    (idx, k, usage, last_error)
                })
            })
            .collect();
        candidates.sort_by(|a, b| {
            // Priority-order policy: first account in pin order that
            // isn't saturated (tier 2) wins. Tier still gates so a
            // saturated first-pin account (tier 3+) cleanly falls
            // through to the next pin entry that's healthier. Within
            // a tier, ties always go to forge.toml definition order
            // — no load-balancing on utilisation. The user's pin
            // expresses intent; the picker just respects it and
            // skips clearly-saturated accounts.
            let tier_a = tier_of(a.2, a.3);
            let tier_b = tier_of(b.2, b.3);
            tier_a.cmp(&tier_b).then_with(|| a.0.cmp(&b.0))
        });
        // Diagnostic log — one line per pick decision listing the
        // tier and probe state of every candidate so a future
        // "why was account X picked?" triage can correlate from
        // logs without re-running with extra instrumentation.
        let decision_summary: Vec<String> = candidates
            .iter()
            .map(|(_, k, u, e)| {
                let tier = tier_of(*u, *e);
                let usage_state = match u {
                    None => "no-snapshot".to_owned(),
                    Some(s) => format!("5h={:.0}%/7d={:.0}%", five_hour_util(s), seven_day_util(s)),
                };
                let err_state = e.map_or("none".to_owned(), |e| format!("{e:?}"));
                format!("{}=tier{}({usage_state},err={err_state})", k.0, tier)
            })
            .collect();
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
            |(_, k, _, _)| (*k).clone(),
        );
        tracing::info!(
            target: "forge_workspace::account",
            event_name = "account_picked",
            message = "account picker decision",
            outcome = "picked",
            picked = %picked.0,
            allowed = ?allowed,
            candidates = ?decision_summary,
        );
        let dir = self.by_key.get(&picked).map_or_else(PathBuf::new, |s| s.config_dir.clone());
        (picked, dir)
    }
}

/// Tier classification driving the sort. Lower = preferred.
///
/// - **0** Unknown-fresh: no usage snapshot yet AND no probe error
///   recorded — picking warms the cache.
/// - **1** Available: usage known, under 100% on both windows.
/// - **2** Rate-limited (data-known): usage poll succeeded, account
///   has hit 100% on at least one window.
/// - **3** Probe rate-limited: the `/api/oauth/usage` endpoint
///   itself returned 429. We can't see the usage, but a persistent
///   probe-429 is a strong signal the account is hot — don't prefer
///   it over accounts with healthy probes.
/// - **4** Probe failed transiently (network / unknown HTTP): treat
///   as unknown-fresh equivalent BUT demoted below available so a
///   healthy-but-unprobed-account doesn't outrank a known-available
///   one.
/// - **5** Credentials broken (expired / unauthorized): the account
///   literally cannot serve a request without re-login. Last-resort
///   only.
///
/// Without the last_error consultation, accounts whose probe
/// perpetually fails sit at tier 0 forever and dominate the sort —
/// the picker keeps choosing them despite having no idea if they're
/// actually usable.
fn tier_of(usage: Option<&UsageSnapshot>, last_error: Option<UsageFetchStatus>) -> u8 {
    match (usage, last_error) {
        (Some(snapshot), _) if is_rate_limited(snapshot) => 2,
        (Some(_), _) => 1,
        // No usage yet — distinguish "still warming" from "probe
        // failed". The error variant determines how badly we demote.
        (None, None) => 0,
        (None, Some(UsageFetchStatus::RateLimited)) => 3,
        (None, Some(UsageFetchStatus::NetworkFailed | UsageFetchStatus::Other)) => 4,
        (None, Some(UsageFetchStatus::Expired | UsageFetchStatus::Unauthorized)) => 5,
    }
}

/// True when either window has hit 100% utilisation. Such an account
/// will immediately trip the API rate limit on the next request, so
/// it's excluded from the "available" tier and only used as a
/// fallback when every pinned account is rate-limited.
fn is_rate_limited(snapshot: &UsageSnapshot) -> bool {
    five_hour_util(snapshot) >= 100.0 || seven_day_util(snapshot) >= 100.0
}

/// Per-account exponential backoff schedule for usage-probe
/// failures. Doubles each consecutive failure, capped at 10
/// minutes. The 60 s poll loop ticks ~once a minute; without
/// backoff, a transient per-IP /usage 429 would persist across
/// every cycle for as long as Anthropic kept rate-limiting,
/// preventing any account from accumulating fresh usage data.
///
/// | consecutive | delay        |
/// |-------------|--------------|
/// | 1           | 30 s         |
/// | 2           | 1 min        |
/// | 3           | 2 min        |
/// | 4           | 4 min        |
/// | 5           | 8 min        |
/// | 6+          | 10 min (cap) |
fn backoff_delay(consecutive_failures: u32) -> std::time::Duration {
    use std::time::Duration;
    const CAP: Duration = Duration::from_secs(600); // 10 min
    let exp = consecutive_failures.min(20); // shift saturates at 20; well under u64::MAX
    let seconds = 30_u64.saturating_mul(1_u64 << exp.saturating_sub(1));
    Duration::from_secs(seconds).min(CAP)
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
            proxy: true,
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
    fn priority_order_picks_first_in_pin_when_both_available() {
        // Subspace: 5h=80%
        // Granite:  5h=10%
        // Both under 100% on both windows → tier 2. Priority-order
        // policy: first in pin order wins regardless of utilisation
        // (the user's pin expresses intent; the picker respects it).
        let mut map = AccountStateMap::new(&[make_account("Subspace"), make_account("Granite")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(80.0), Some(60.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(10.0), Some(20.0)));
        let (picked, _) = map.pick_for_project(&["Subspace".to_owned(), "Granite".to_owned()]);
        assert_eq!(picked.0, "Subspace", "first pin entry wins when both are healthy");
    }

    #[test]
    fn priority_order_respects_pin_order_when_first_pin_has_higher_seven_day() {
        // Subspace: 5h=10%, 7d=90% — high 7d but still under 100%
        // Granite:  5h=50%, 7d=50%
        // Both tier 2. Priority-order: first in pin wins.
        let mut map = AccountStateMap::new(&[make_account("Subspace"), make_account("Granite")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(10.0), Some(90.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&["Subspace".to_owned(), "Granite".to_owned()]);
        assert_eq!(picked.0, "Subspace", "pin order over utilisation");
    }

    #[test]
    fn rate_limited_account_excluded_in_favour_of_available_one() {
        // Granite: 5h=0%, 7d=100% — RATE LIMITED on 7d.
        // Subspace: 5h=80%, 7d=80% — heavily used but neither at 100%.
        // Picker must exclude Granite even though its 5h is lower.
        let mut map = AccountStateMap::new(&[make_account("Granite"), make_account("Subspace")]);
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(0.0), Some(100.0)));
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(80.0), Some(80.0)));
        let (picked, _) = map.pick_for_project(&["Granite".to_owned(), "Subspace".to_owned()]);
        assert_eq!(picked.0, "Subspace");
    }

    #[test]
    fn rate_limited_account_excluded_on_five_hour_too() {
        // Granite: 5h=100%, 7d=0% — rate limited on 5h.
        // Subspace: 5h=80%, 7d=80% — available.
        // Must pick Subspace.
        let mut map = AccountStateMap::new(&[make_account("Granite"), make_account("Subspace")]);
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(100.0), Some(0.0)));
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(80.0), Some(80.0)));
        let (picked, _) = map.pick_for_project(&["Granite".to_owned(), "Subspace".to_owned()]);
        assert_eq!(picked.0, "Subspace");
    }

    #[test]
    fn unknown_usage_sorts_first_in_definition_order() {
        // Subspace has data; Granite + Personal don't.
        // Picker picks Granite (first unknown in definition order)
        // to warm the cache, even when Subspace looks healthy.
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
    fn all_rate_limited_falls_back_to_definition_order() {
        // Every pinned account hit at least one limit. Picker still
        // returns something (the spawn must not fail) — definition
        // order picks the first one.
        let mut map = AccountStateMap::new(&[make_account("Granite"), make_account("Subspace")]);
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(100.0), Some(100.0)));
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(100.0), Some(100.0)));
        let (picked, _) = map.pick_for_project(&["Granite".to_owned(), "Subspace".to_owned()]);
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn available_wins_over_rate_limited_regardless_of_pin_order() {
        // Granite is first in the pin AND rate-limited; Subspace is
        // second AND available. Tier sort must lift Subspace ahead.
        let mut map = AccountStateMap::new(&[make_account("Granite"), make_account("Subspace")]);
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(50.0), Some(100.0)));
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(99.9), Some(99.9)));
        let (picked, _) = map.pick_for_project(&["Granite".to_owned(), "Subspace".to_owned()]);
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
        let (picked, _) = map.pick_for_project(&["Subspace".to_owned(), "Granite".to_owned()]);
        assert_eq!(picked.0, "Subspace");
        assert_ne!(picked.0, "Personal");
    }

    #[test]
    fn priority_order_ignores_seven_day_difference_when_both_available() {
        // Both at 5h=50, different 7d. Old policy used 7d as a
        // tiebreaker; the priority-order policy respects pin order
        // — Subspace first → Subspace wins even with worse 7d.
        let mut map = AccountStateMap::new(&[make_account("Subspace"), make_account("Granite")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(50.0), Some(70.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(50.0), Some(30.0)));
        let (picked, _) = map.pick_for_project(&["Subspace".to_owned(), "Granite".to_owned()]);
        assert_eq!(picked.0, "Subspace", "pin order over 7d util");
    }

    #[test]
    fn definition_order_final_tiebreak() {
        // Identical 5h + 7d → definition order. Subspace first in
        // the allow list wins.
        let mut map = AccountStateMap::new(&[make_account("Subspace"), make_account("Granite")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(50.0), Some(50.0)));
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&["Subspace".to_owned(), "Granite".to_owned()]);
        assert_eq!(picked.0, "Subspace");
    }

    #[test]
    fn known_available_account_beats_probe_failed_account() {
        // Granite has a fresh successful probe (tier 1, 30% used);
        // Personal's probe keeps returning 429 (last_error =
        // RateLimited, usage still None → tier 3, probe rate-limited).
        // Granite wins.
        let mut map = AccountStateMap::new(&[make_account("Granite"), make_account("Personal")]);
        map.set_usage(&AccountKey("Granite".to_owned()), snapshot(Some(30.0), Some(30.0)));
        map.set_last_error(&AccountKey("Personal".to_owned()), UsageFetchStatus::RateLimited, None);
        let (picked, _) = map.pick_for_project(&["Granite".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Granite", "available account must beat probe-rate-limited one");
    }

    #[test]
    fn expired_credentials_demote_to_last_resort() {
        // Granite1 has expired OAuth (literally can't serve a
        // request); Personal has working probe but is fully rate-
        // limited (tier 2). Personal still wins because tier 2
        // ranks above tier 5 (Expired).
        let mut map = AccountStateMap::new(&[make_account("Granite1"), make_account("Personal")]);
        map.set_last_error(&AccountKey("Granite1".to_owned()), UsageFetchStatus::Expired, None);
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(100.0), Some(100.0)));
        let (picked, _) = map.pick_for_project(&["Granite1".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Personal", "rate-limited-but-callable beats expired-can't-serve");
    }

    #[test]
    fn fresh_unknown_still_wins_over_known_available() {
        // True cold-cache case: no last_error, no usage. Granite
        // has no probe attempt yet → tier 0 → preferred over
        // Personal which has known-available usage (tier 1).
        // Confirms the original "warm the cache" behaviour survives
        // for accounts that haven't been polled yet.
        let mut map = AccountStateMap::new(&[make_account("Granite"), make_account("Personal")]);
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(30.0), Some(30.0)));
        // Granite has no usage AND no last_error → tier 0.
        let (picked, _) = map.pick_for_project(&["Granite".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn network_failure_demotes_below_available() {
        // Granite probe failed with a network error (tier 4); Personal
        // has a healthy probe at moderate utilisation (tier 1).
        // Personal wins — we'd rather pick an account whose state
        // we know than one whose state we don't.
        let mut map = AccountStateMap::new(&[make_account("Granite"), make_account("Personal")]);
        map.set_last_error(
            &AccountKey("Granite".to_owned()),
            UsageFetchStatus::NetworkFailed,
            None,
        );
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&["Granite".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Personal");
    }

    #[test]
    fn backoff_delay_doubles_then_caps_at_10_minutes() {
        use std::time::Duration;
        assert_eq!(backoff_delay(1), Duration::from_secs(30));
        assert_eq!(backoff_delay(2), Duration::from_secs(60));
        assert_eq!(backoff_delay(3), Duration::from_secs(120));
        assert_eq!(backoff_delay(4), Duration::from_secs(240));
        assert_eq!(backoff_delay(5), Duration::from_secs(480));
        assert_eq!(backoff_delay(6), Duration::from_secs(600), "capped at 10 min");
        assert_eq!(backoff_delay(20), Duration::from_secs(600), "still capped");
    }

    #[test]
    fn should_probe_now_true_for_cold_cache() {
        let map = AccountStateMap::new(&[make_account("Granite")]);
        assert!(map.should_probe_now(&AccountKey("Granite".to_owned())));
    }

    #[test]
    fn set_last_error_schedules_next_probe_in_future() {
        let mut map = AccountStateMap::new(&[make_account("Granite")]);
        let key = AccountKey("Granite".to_owned());
        map.set_last_error(&key, UsageFetchStatus::RateLimited, None);
        assert!(!map.should_probe_now(&key), "first failure puts account in backoff");
    }

    #[test]
    fn retry_after_overrides_exponential_backoff() {
        // Anthropic returns Retry-After: 3048 (seconds) for a deeply
        // rate-limited account. Our local exponential schedule would
        // pick 30 s for the first failure — vastly under-shoot the
        // actual reset and re-trip the limit. The server-provided
        // retry_after must win.
        use std::time::Duration;
        let mut map = AccountStateMap::new(&[make_account("Granite1")]);
        let key = AccountKey("Granite1".to_owned());
        let t0 = std::time::Instant::now();
        map.set_last_error(&key, UsageFetchStatus::RateLimited, Some(Duration::from_secs(3048)));
        let next = map.by_key.get(&key).and_then(|s| s.next_probe_at).expect("scheduled");
        let gap = next.saturating_duration_since(t0);
        assert!(
            gap >= Duration::from_secs(3047) && gap <= Duration::from_secs(3050),
            "next_probe_at ≈ now + 3048s; got {gap:?}",
        );
    }

    #[test]
    fn set_usage_clears_backoff_so_account_probes_again() {
        let mut map = AccountStateMap::new(&[make_account("Granite")]);
        let key = AccountKey("Granite".to_owned());
        map.set_last_error(&key, UsageFetchStatus::RateLimited, None);
        assert!(!map.should_probe_now(&key));
        map.set_usage(&key, snapshot(Some(10.0), Some(10.0)));
        assert!(map.should_probe_now(&key), "successful probe clears the backoff gate");
    }

    #[test]
    fn config_dir_lookup_returns_path() {
        let mut map = AccountStateMap::new(&[make_account("Subspace")]);
        map.set_usage(&AccountKey("Subspace".to_owned()), snapshot(Some(0.0), Some(0.0)));
        let dir = map.config_dir(&AccountKey("Subspace".to_owned()));
        assert_eq!(dir, Some(&PathBuf::from("/fake/Subspace")));
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
