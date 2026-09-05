//! Account selection - internal to forge-workspace.
//!
//! `Workspace::get_agent_handle` consults `pick_for_project` on
//! every spawn; the chosen `AccountKey` becomes the spawned Agent's
//! `CLAUDE_CONFIG_DIR` override.
//!
//! **Policy - two-tier filter + global round-robin:**
//!
//! 1. **Usable** - usage is unknown, OR usage shows under 100% on
//!    both windows, OR last probe failed transiently (network /
//!    other HTTP). The picker can try this account; the spawned
//!    `claude` either succeeds or surfaces its own rate-limit error
//!    we can react to.
//! 2. **Unusable** - usage shows 100% on at least one window, OR
//!    `/api/oauth/usage` returned 429, OR credentials are expired /
//!    unauthorized. Known to be either at the cap or unable to
//!    authenticate.
//!
//! Within the usable tier, a single global round-robin counter
//! rotates picks across every healthy account in the project's
//! `accounts` allow-list. The counter is shared across all projects
//! (one increment per pick) so concurrent spawns from different
//! projects continue rotating instead of all hammering whichever
//! account happens to be first in their respective lists. Counter
//! is in-memory only; resets to 0 on forge restart (no persistence).
//!
//! If every account in the allow-list is Unusable, the picker falls
//! back to the first entry so the spawn doesn't fail outright - the
//! user gets visible feedback from the spawned subprocess's own
//! 401/429 rather than from forge silently refusing.
//!
//! Rate-limited accounts are visible in the bottom-panel bars so the
//! user can manually broaden the allow-list or wait for the window
//! to reset.

use std::path::PathBuf;

use forge_primitives::usage::{UsageSnapshot, UsageWindow};

use crate::config::LoadedAccount;

/// Internal newtype wrapping the account's `display_name`.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub(crate) struct AccountKey(pub String);

/// Classification of the latest usage-poll attempt outcome for an
/// account. Surfaced to the TUI's bottom panel so empty bars can
/// distinguish "still warming the cache" from a real failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageFetchStatus {
    /// Anthropic returned HTTP 429 - too many concurrent polls
    /// against the OAuth `/api/oauth/usage` endpoint (typical when
    /// multiple forge instances poll from the same machine).
    RateLimited,
    /// OAuth credentials on disk are past their expires_at - needs
    /// `/login` to refresh.
    Expired,
    /// API returned 401/403 - token rejected (may be revoked).
    Unauthorized,
    /// Network failure reaching the API (DNS, TLS, timeout, …).
    NetworkFailed,
    /// Decode / unknown-HTTP-status fallthrough. Distinct from the
    /// above so renderers can show a generic "fetch error" when the
    /// cause doesn't map to a known bucket.
    Other,
}

/// Boot-time loading state for an account. The launchpad gates click
/// and spawn until every account in the map has resolved to `Ready`
/// or `Bailed`; both terminal states feed into the assignment-plan
/// computation, while `Loading` and `Refreshing` keep the launchpad
/// dim. A bailed account's `usage` is `None` by construction (the
/// loader clears it on the transition).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadingState {
    /// First-pass keychain fetch + probe in progress. The launchpad
    /// shows `○` (yellow) for this account.
    Loading,
    /// A 401 with `loggedIn=true` triggered a `claude -p hi` refresh;
    /// next iteration will re-probe with the rotated token. Glyph is
    /// the same yellow `○` as `Loading` (user-visible distinction is
    /// not necessary; the launchpad gate cares about terminal-vs-not).
    Refreshing,
    /// Probe returned 200; account is usable and may be assigned to
    /// sessions. Launchpad glyph: `●` (green).
    Ready,
    /// Either `loggedIn=false` from `claude auth status` or the
    /// refresh path itself failed terminally. User must `/login`
    /// interactively; the 30 s recovery poll will retry from
    /// `Loading` once auth_status flips back. Launchpad glyph: `⚠`
    /// (red).
    Bailed,
}

#[derive(Debug, Clone)]
pub(crate) struct AccountState {
    pub config_dir: PathBuf,
    /// Declared backend from `[[accounts]] provider`. Decides which
    /// endpoint the usage probe hits and which credential it carries.
    pub provider: forge_primitives::account::Provider,
    /// Per-account environment from `[accounts.env]`. Stamped onto the
    /// account's `claude` subprocess at spawn, and read by the usage
    /// probe for the `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` a
    /// base-url provider authenticates with.
    pub env: std::collections::HashMap<String, String>,
    /// When true, the account is excluded from every auto-assignment
    /// path (the assignment plan and the round-robin fallback) but
    /// stays globally selectable in the `/account` picker. Mirrors the
    /// `[[accounts]] experimental = true` toggle in forge.toml.
    pub experimental: bool,
    /// Optional CLI permission mode stamped into launch settings at
    /// spawn. Mirrors the `[[accounts]] permission_mode` key.
    pub permission_mode: Option<forge_primitives::permission::PermissionMode>,
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
    /// (without backoff every poll cycle re-trips the per-IP rate
    /// limit and we never accumulate usage data). `None` means
    /// "probe on the next tick" (default).
    pub next_probe_at: Option<std::time::Instant>,
    /// Consecutive probe failures since the last success. Drives
    /// the backoff schedule: each consecutive failure doubles the
    /// next-probe delay (capped at a sensible ceiling). Reset to
    /// 0 on success.
    pub consecutive_failures: u32,
    /// One-shot arming flag for the `has_just_cleared_cap_window`
    /// scheduler hook. Without it, a stale-reset snapshot paired with
    /// sustained probe failure would re-trigger the override every
    /// poll cycle, defeating the exponential backoff schedule -
    /// forge would hammer Anthropic's `/api/oauth/usage` endpoint once
    /// per minute through a multi-hour outage instead of backing off.
    /// `set_usage` arms it (a fresh snapshot earns one override
    /// attempt once its `resets_at` passes); `disarm_override` clears
    /// it after the scheduler fires an override. False until the
    /// first successful probe lands a snapshot. Distinct from
    /// `LoadingState` - rate-limit concern, not auth-recovery.
    pub override_armed: bool,
    /// Where this account sits in the boot-time loading state
    /// machine. The launchpad's "all accounts loaded" gate consults
    /// this across every account; the assignment plan only includes
    /// accounts whose state is `Ready`.
    pub loading: LoadingState,
}

#[derive(Debug)]
pub(crate) struct AccountStateMap {
    pub ordered_keys: Vec<AccountKey>, // forge.toml definition order
    pub by_key: std::collections::HashMap<AccountKey, AccountState>,
    /// Global round-robin cursor for `pick_for_project`. Each pick
    /// in the usable tier reads `cursor % usable_len`, then bumps
    /// the cursor. Shared across all projects so rotation spans the
    /// whole spawn stream, not just per-project. In-memory only -
    /// resets to 0 on forge restart.
    rr_cursor: std::sync::atomic::AtomicUsize,
}

impl AccountStateMap {
    /// Empty map for the `testing` feature's `Workspace::testing_stub`.
    /// Production code paths reach this map only via account pickers
    /// (`pick_for_project`), which a test fixture should never exercise.
    #[cfg(any(test, feature = "testing"))]
    pub fn empty_for_test() -> Self {
        Self {
            ordered_keys: Vec::new(),
            by_key: std::collections::HashMap::new(),
            rr_cursor: std::sync::atomic::AtomicUsize::new(0),
        }
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
                    provider: account.provider,
                    env: account.env.clone(),
                    experimental: account.experimental,
                    permission_mode: account.permission_mode,
                    usage: None,
                    last_error: None,
                    next_probe_at: None,
                    consecutive_failures: 0,
                    override_armed: false,
                    loading: LoadingState::Loading,
                },
            );
        }
        Self { ordered_keys, by_key, rr_cursor: std::sync::atomic::AtomicUsize::new(0) }
    }

    /// Seed the in-memory map from a previously-persisted cache.
    /// Each known account that appears in `cached` gets its `usage`
    /// populated; unknown cache entries (account removed from
    /// forge.toml) are ignored. Does NOT clear `last_error` or
    /// `next_probe_at` - the cache is purely seed data; the live
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
                out.insert(key.0.clone(), crate::account_cache::CachedAccountUsage { snapshot });
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

    /// The account's env map: global `[env]` extended by its own
    /// `[accounts.env]`. Consumed by the spawn path (stamped onto the
    /// child) and the usage poller / loader (base url, bearer, setup
    /// token). `None` for unknown keys.
    pub fn env(&self, key: &AccountKey) -> Option<&std::collections::HashMap<String, String>> {
        self.by_key.get(key).map(|s| &s.env)
    }

    /// Declared [`Provider`] for `key`, the input the probe plan and the
    /// preflight repair copy both branch on. `None` for unknown keys.
    pub fn provider(&self, key: &AccountKey) -> Option<forge_primitives::account::Provider> {
        self.by_key.get(key).map(|s| s.provider)
    }

    /// Optional per-account `permission_mode` for `key`. Consumed by
    /// the spawn path to stamp `permissions.defaultMode` into launch
    /// settings. `None` for unknown keys or an account without the key.
    pub fn permission_mode(
        &self,
        key: &AccountKey,
    ) -> Option<forge_primitives::permission::PermissionMode> {
        self.by_key.get(key).and_then(|s| s.permission_mode)
    }

    /// [`Self::provider`] for a key that came out of this map, so a miss
    /// means the map changed underneath the caller. Anthropic is the
    /// safe default - it probes the keychain rather than an endpoint
    /// derived from an env this account may not have - but it is the
    /// wrong answer for a base-url account, whose repair copy would then
    /// tell the user to run `/login`. Warn rather than pick silently.
    pub fn provider_or_anthropic(&self, key: &AccountKey) -> forge_primitives::account::Provider {
        self.provider(key).unwrap_or_else(|| {
            tracing::warn!(
                target: "forge_workspace::account",
                account = %key.0,
                "no provider for a key taken from the account map; assuming anthropic",
            );
            forge_primitives::account::Provider::Anthropic
        })
    }

    /// Distinct on-disk config_dirs across every known account. Used by
    /// the worker-resume scan, which must look under every account a
    /// worker could have been spawned under (the assignment-plan
    /// rotation distributes workers across accounts).
    pub fn config_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        for state in self.by_key.values() {
            if !dirs.contains(&state.config_dir) {
                dirs.push(state.config_dir.clone());
            }
        }
        dirs
    }

    /// Whether `key` is an experimental account (excluded from
    /// auto-assignment, picker-only). Returns `false` for unknown
    /// keys.
    pub fn is_experimental(&self, key: &AccountKey) -> bool {
        self.by_key.get(key).is_some_and(|s| s.experimental)
    }

    /// Replace the cached usage snapshot for `key` and clear any
    /// stale `last_error`. Resets the consecutive-failure counter
    /// and clears the next-probe gate so this account returns to
    /// the default cadence. Also transitions `loading` to `Ready` -
    /// a successful probe is the terminal-success state for the
    /// boot-time loading task. Silent no-op when `key` isn't
    /// registered (defensive - invariant says every poller key was
    /// inserted in `new()`).
    pub fn set_usage(&mut self, key: &AccountKey, snapshot: UsageSnapshot) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.usage = Some(snapshot);
            state.last_error = None;
            state.next_probe_at = None;
            state.consecutive_failures = 0;
            state.loading = LoadingState::Ready;
            // Arm the scheduler-hook override: this fresh snapshot
            // earns ONE probe attempt once its `resets_at` passes.
            // Without re-arming, the hook stays disarmed forever after
            // its first fire and the stale-cache bar never gets a fresh
            // probe even on a healthy account.
            state.override_armed = true;
        }
    }

    /// Drive a `LoadingState` transition for `key`. Used by the
    /// boot-time loading task to step between `Loading` →
    /// `Refreshing` → terminal, and by the recovery poll to flip a
    /// `Bailed` account back to `Loading` once `auth_status` reports
    /// logged-in. Setting `Bailed` clears the cached `usage` so the
    /// renderer drops the stale %bar (replaces the PR #238 3-strike
    /// counter; bailed accounts have no live snapshot by construction).
    pub fn set_loading(&mut self, key: &AccountKey, loading: LoadingState) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.loading = loading;
            if loading == LoadingState::Bailed {
                state.usage = None;
            }
        }
    }

    /// Disarm the scheduler hook for `key` after firing an override
    /// probe attempt. One-shot semantics: the hook fires once per
    /// arming (i.e. once per fresh snapshot). Subsequent stale-reset
    /// state must wait for the next successful probe (which re-arms
    /// via `set_usage`) or until the existing backoff timer
    /// (`next_probe_at`) elapses.
    pub fn disarm_override(&mut self, key: &AccountKey) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.override_armed = false;
        }
    }

    /// Record the latest poll-attempt failure for `key` and schedule
    /// the next probe.
    ///
    /// `retry_after` (when `Some`) is the server-provided hold-down
    /// duration - typically Anthropic's `Retry-After` header on 429.
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
            // Auth-recovery cache-clear: an Unauthorized or Expired
            // probe response means the account's credential is dead.
            // Transition `loading` to `Bailed` and drop the
            // cached `usage` so the renderer surfaces the error label
            // instead of the stale %bar. Recovery paths differ by
            // class: the 30 s recovery poll
            // (account_loader::run_recovery_poll) picks a keychain
            // account back up once `claude auth status` reports
            // logged-in, while a base-url or token account recovers
            // via the 60 s usage poller (after the edited env is
            // re-read at a restart). Other statuses leave
            // `loading` alone (a transient `RateLimited` or
            // `NetworkFailed` is not auth-related; the cache stays
            // and the account remains Ready for the assignment plan).
            // Replaces the PR #238 `consecutive_unauthorized` 3-strike
            // counter - the recovery poll absorbs transient 401s.
            if matches!(status, UsageFetchStatus::Unauthorized | UsageFetchStatus::Expired) {
                let prev = state.loading;
                state.loading = LoadingState::Bailed;
                state.usage = None;
                // Surface the transition so an operator triaging
                // "account suddenly stopped working" can see WHEN the
                // bail happened + which probe-class triggered it.
                // Without this log a Ready -> Bailed flip is silent
                // until a poller runs again.
                if prev != LoadingState::Bailed {
                    tracing::warn!(
                        target: "forge_workspace::account",
                        event_name = "account_bailed",
                        account = %key.0,
                        prev_state = ?prev,
                        status = ?status,
                        "account transitioned to Bailed via probe failure; the pollers will retry it",
                    );
                }
            }
            // Anthropic returns `Retry-After: 0` on the /api/oauth/usage
            // 429 path, which is "no specific hint" rather than "you can
            // retry now". Trusting it literally schedules next_probe_at
            // at the same instant and the next round re-trips the same
            // rate limit - that's the burst-probe behaviour the warm
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

    /// `true` when every account in the map has reached a terminal
    /// `LoadingState` (`Ready` or `Bailed`). The launchpad uses this
    /// as the gate for un-dimming project rows + un-blocking clicks;
    /// the assignment-plan compute step also fires off this signal.
    /// Empty maps return `true` (vacuous; no accounts means no work
    /// to wait on - relevant in the testing-stub path).
    pub fn all_loaded(&self) -> bool {
        self.by_key
            .values()
            .all(|s| matches!(s.loading, LoadingState::Ready | LoadingState::Bailed))
    }

    /// True when `key`'s loaded usage snapshot shows it at-or-beyond
    /// the plan cap - the same saturation signal the fallback picker's
    /// tier classification uses. A Ready-but-saturated account logs in
    /// fine but trips the rate limit on its next request, so the
    /// assignment plan prefers other accounts when one is available.
    pub(crate) fn is_saturated(&self, key: &AccountKey) -> bool {
        self.by_key.get(key).and_then(|s| s.usage.as_ref()).is_some_and(is_rate_limited)
    }

    /// `true` when `key` is currently pickable for a fresh assignment:
    /// tier-0 (not at the usage cap, not probe-blocked by a
    /// 429/expired/unauthorized) and not `Bailed`. Single source of
    /// truth for the usable filter shared by `pick_for_project` and the
    /// ad-hoc assignment guard.
    pub fn is_account_usable(&self, key: &AccountKey) -> bool {
        tier_of(self.usage(key), self.usage_error(key)) == 0
            && self.loading_state(key) != LoadingState::Bailed
    }

    /// Snapshot the current `LoadingState` for `key`. Returns
    /// `LoadingState::Loading` by default for unknown keys
    /// (defensive - the launchpad's render path may briefly hold an
    /// account key that hasn't yet been registered in the map during
    /// reload).
    pub fn loading_state(&self, key: &AccountKey) -> LoadingState {
        self.by_key.get(key).map_or(LoadingState::Loading, |s| s.loading)
    }

    /// Combined scheduler signal: probe `key` on the current cycle if
    /// the normal backoff gate (`should_probe_now`) is open OR the
    /// one-shot reset-clear override hook (`has_just_cleared_cap_window`)
    /// is firing. Both sides of the OR carry through to the workspace
    /// poller, which calls this method to decide which accounts to
    /// probe each tick. Centralising the OR here keeps a future
    /// refactor from accidentally dropping one half of the signal -
    /// the unit test pins the truth table that the scheduler relies
    /// on, regardless of whether the call site spells out both
    /// predicates inline.
    pub fn scheduler_should_probe(&self, key: &AccountKey) -> bool {
        self.should_probe_now(key) || self.has_just_cleared_cap_window(key)
    }

    /// `true` when at least one cached window shows the account just
    /// transitioned out of its cap (utilization at-or-above 100%,
    /// `resets_at` now in the past) AND the override hook is armed.
    /// The scheduler ORs this with `should_probe_now` so a fresh probe
    /// lands on the next poll cycle after the reset moment, instead of
    /// waiting through the remainder of an active backoff window.
    /// Without the hook, an account 429'd with a multi-hour
    /// `Retry-After` keeps painting the stale "100%" bar for hours
    /// past the actual reset because the next probe is gated until the
    /// backoff timer elapses.
    ///
    /// The `override_armed` gate enforces one-shot semantics: each
    /// successful probe arms the hook exactly once, and the scheduler
    /// disarms it after firing. A persistently failing probe series
    /// won't keep tripping the override every cycle.
    pub fn has_just_cleared_cap_window(&self, key: &AccountKey) -> bool {
        let Some(state) = self.by_key.get(key) else {
            return false;
        };
        if !state.override_armed {
            return false;
        }
        let Some(usage) = state.usage.as_ref() else {
            return false;
        };
        let now = std::time::SystemTime::now();
        let windows = [
            usage.five_hour.as_ref(),
            usage.seven_day.as_ref(),
            usage.seven_day_opus.as_ref(),
            usage.seven_day_sonnet.as_ref(),
        ];
        windows
            .into_iter()
            .flatten()
            .any(|w| w.utilization >= 100.0 && w.resets_at.is_some_and(|when| when <= now))
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

    /// Drop the recorded failure for `key`, so a bail that never
    /// recorded its own cause falls back to the unrecorded default
    /// rather than wearing a stale class.
    pub fn clear_last_error(&mut self, key: &AccountKey) {
        if let Some(state) = self.by_key.get_mut(key) {
            state.last_error = None;
        }
    }

    /// Pick an account within the project's `allowed` subset using
    /// tier-gated round-robin (see module docs).
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
        // Resolve allow-list entries to known keys, preserving
        // allow-list order. Carry usage + last_error + loading so
        // tier_of can see the full picture - an account whose
        // boot-time loading task ended in `Bailed` (auth_status said
        // logged-out, refresh failed, etc.) must NOT be picked even
        // if its last_error is None - tier_of's existing inputs
        // wouldn't catch a Bailed-without-recent-error case, which
        // is the exact shape after the recovery poll transitions
        // Loading -> Bailed without firing set_last_error.
        let candidates: Vec<(
            &AccountKey,
            Option<&UsageSnapshot>,
            Option<UsageFetchStatus>,
            LoadingState,
        )> = allowed
            .iter()
            .filter_map(|name| self.ordered_keys.iter().find(|k| k.0 == *name))
            // Experimental accounts are picker-only: never auto-assigned,
            // even when pinned by the project's allow-list.
            .filter(|k| !self.is_experimental(k))
            .map(|k| {
                let state = self.by_key.get(k);
                (
                    k,
                    state.and_then(|s| s.usage.as_ref()),
                    state.and_then(|s| s.last_error),
                    state.map_or(LoadingState::Loading, |s| s.loading),
                )
            })
            .collect();
        // Usable subset, in allow-list order. Round-robin rotates
        // across this filtered list so saturated / expired / bailed
        // accounts never get picked even when their slot in the
        // cursor cycle comes up.
        let usable: Vec<&AccountKey> = candidates
            .iter()
            .filter(|(k, _, _, _)| self.is_account_usable(k))
            .map(|(k, _, _, _)| *k)
            .collect();
        let picked = if usable.is_empty() {
            // Every allow-list entry is Unusable. Spawn must still
            // proceed so the user sees the spawned subprocess's
            // 401/429 rather than forge silently refusing - fall
            // back to the first allow-list entry that exists.
            candidates.first().map_or_else(
                || {
                    tracing::error!(
                        target: "forge_workspace::account",
                        "pick_for_project: candidates resolved to empty; allow list = {allowed:?}",
                    );
                    // Last resort still skips experimental accounts, so
                    // an all-experimental allow-list falls onto the first
                    // non-experimental account rather than assigning one.
                    self.ordered_keys
                        .iter()
                        .find(|k| !self.is_experimental(k))
                        .cloned()
                        .unwrap_or(AccountKey(String::new()))
                },
                |(k, _, _, _)| (*k).clone(),
            )
        } else {
            // `Relaxed` is sufficient: the cursor only needs to
            // advance monotonically; the exact interleaving with
            // other shared-state reads is irrelevant for load
            // balancing.
            let idx =
                self.rr_cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % usable.len();
            usable[idx].clone()
        };
        // Diagnostic log - one line per pick decision listing the
        // tier and probe state of every candidate so a future
        // "why was account X picked?" triage can correlate from
        // logs without re-running with extra instrumentation.
        let decision_summary: Vec<String> = candidates
            .iter()
            .map(|(k, u, e, l)| {
                let tier = tier_of(*u, *e);
                let usage_state = match u {
                    None => "no-snapshot".to_owned(),
                    // A diagnostic line, so an absent window reads as 0
                    // here deliberately rather than widening the format.
                    Some(s) => format!(
                        "5h={:.0}%/7d={:.0}%",
                        s.five_hour_util().unwrap_or(0.0),
                        s.seven_day_util().unwrap_or(0.0),
                    ),
                };
                let err_state = e.map_or("none".to_owned(), |e| format!("{e:?}"));
                format!("{}=tier{}({usage_state},err={err_state},loading={l:?})", k.0, tier)
            })
            .collect();
        tracing::debug!(
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

/// Two-tier classification driving the picker. Round-robin rotates
/// among tier-0 entries; tier 1 is only used when no tier-0 candidate
/// exists in the allow-list (the all-unusable fallback path).
///
/// - **0** Usable: usage unknown (no probe yet), OR usage known
///   under 100% on both windows, OR last probe failed transiently
///   (network / unknown HTTP - we just don't know yet). The picker
///   can try this account.
/// - **1** Unusable: usage shows 100% on at least one window, OR
///   `/api/oauth/usage` returned 429, OR credentials are expired /
///   unauthorized. Either at the cap or unable to authenticate.
fn tier_of(usage: Option<&UsageSnapshot>, last_error: Option<UsageFetchStatus>) -> u8 {
    let saturated = usage.is_some_and(is_rate_limited);
    let probe_blocked = matches!(
        last_error,
        Some(
            UsageFetchStatus::RateLimited
                | UsageFetchStatus::Expired
                | UsageFetchStatus::Unauthorized
        )
    );
    u8::from(saturated || probe_blocked)
}

/// True when ANY window in the snapshot is currently at-or-beyond
/// the plan cap AND its scheduled reset has not yet passed. Such an
/// account will immediately trip the API rate limit on the next
/// request, so it's excluded from the "available" tier and only used
/// as a fallback when every pinned account is rate-limited.
///
/// Each window's predicate (`UsageWindow::is_currently_limited`) gates
/// on `resets_at` so a stale cached snapshot from before the window
/// reset doesn't keep the account permanently classified as
/// rate-limited. The pair "utilization >= 100% AND resets_at > now"
/// is what makes the classification self-clearing across the reset
/// boundary - earlier shapes that checked utilization alone needed
/// a separate cache-invalidation pathway, which has churned through
/// several abandoned designs.
fn is_rate_limited(snapshot: &UsageSnapshot) -> bool {
    let windows = [
        snapshot.five_hour.as_ref(),
        snapshot.seven_day.as_ref(),
        snapshot.seven_day_opus.as_ref(),
        snapshot.seven_day_sonnet.as_ref(),
    ];
    windows.into_iter().flatten().any(UsageWindow::is_currently_limited)
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
            provider: forge_primitives::account::Provider::Anthropic,
            env: std::collections::HashMap::new(),
            experimental: false,
            permission_mode: None,
        }
    }

    #[test]
    fn account_state_map_carries_the_permission_mode() {
        let with_mode = LoadedAccount {
            permission_mode: Some(forge_primitives::permission::PermissionMode::BypassPermissions),
            ..make_account("Openrouter")
        };
        let map = AccountStateMap::new(&[with_mode, make_account("Stargate")]);
        assert_eq!(
            map.permission_mode(&AccountKey("Openrouter".to_owned())),
            Some(forge_primitives::permission::PermissionMode::BypassPermissions),
            "the spawn path reads the mode off the account map",
        );
        assert_eq!(
            map.permission_mode(&AccountKey("Stargate".to_owned())),
            None,
            "an account without the key reports None",
        );
        assert_eq!(
            map.permission_mode(&AccountKey("Unknown".to_owned())),
            None,
            "an unknown key reports None",
        );
    }

    /// Default snapshot fixture used by every account-picker test. The
    /// `resets_at` value matters: under the resets_at-driven predicate,
    /// a window at 100% utilization with `resets_at = None` is NOT
    /// classified as rate-limited (None means "no future reset to clear
    /// this", which would strand the account forever). Real probes
    /// always emit a future `resets_at` alongside the percentage, so
    /// the fixture mirrors that by defaulting to a reset 1 hour out.
    /// The few tests that need an EXPIRED reset (the staleness case)
    /// build their snapshot inline rather than through this helper.
    fn snapshot(five_hour: Option<f64>, seven_day: Option<f64>) -> UsageSnapshot {
        let future = SystemTime::now() + std::time::Duration::from_secs(3600);
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: SystemTime::UNIX_EPOCH,
            five_hour: five_hour.map(|util| UsageWindow {
                utilization: util,
                resets_at: Some(future),
                reset_description: None,
            }),
            seven_day: seven_day.map(|util| UsageWindow {
                utilization: util,
                resets_at: Some(future),
                reset_description: None,
            }),
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
        }
    }

    #[test]
    fn is_experimental_reflects_account_flag() {
        let mut exp = make_account("Exp");
        exp.experimental = true;
        let map = AccountStateMap::new(&[make_account("Regular"), exp]);
        assert!(map.is_experimental(&AccountKey("Exp".to_owned())));
        assert!(!map.is_experimental(&AccountKey("Regular".to_owned())));
        assert!(
            !map.is_experimental(&AccountKey("Unknown".to_owned())),
            "unknown key is not experimental",
        );
    }

    #[test]
    fn env_accessor_returns_account_env() {
        let mut codex = make_account("Codex");
        codex.env.insert("ANTHROPIC_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        let map = AccountStateMap::new(&[codex, make_account("Plain")]);
        assert_eq!(
            map.env(&AccountKey("Codex".to_owned()))
                .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
                .map(String::as_str),
            Some("http://localhost:18765"),
        );
        assert!(
            map.env(&AccountKey("Plain".to_owned()))
                .is_some_and(std::collections::HashMap::is_empty),
            "account with no env -> empty map",
        );
        assert!(map.env(&AccountKey("Unknown".to_owned())).is_none(), "unknown key -> None");
    }

    #[test]
    fn is_account_usable_covers_tier_and_loading_table() {
        let mut map = AccountStateMap::new(&[
            make_account("ready-low"),
            make_account("saturated"),
            make_account("probe-rate-limited"),
            make_account("probe-expired"),
            make_account("probe-unauthorized"),
            make_account("bailed"),
            make_account("refreshing"),
        ]);

        // tier-0 + Ready -> usable.
        map.set_usage(&AccountKey("ready-low".to_owned()), snapshot(Some(10.0), Some(20.0)));
        // Saturated usage (100%) -> unusable even though loading is Ready.
        map.set_usage(&AccountKey("saturated".to_owned()), snapshot(Some(100.0), None));
        // Probe errors -> unusable. RateLimited leaves loading alone;
        // Expired / Unauthorized also flip loading to Bailed.
        map.set_last_error(
            &AccountKey("probe-rate-limited".to_owned()),
            UsageFetchStatus::RateLimited,
            None,
        );
        map.set_last_error(
            &AccountKey("probe-expired".to_owned()),
            UsageFetchStatus::Expired,
            None,
        );
        map.set_last_error(
            &AccountKey("probe-unauthorized".to_owned()),
            UsageFetchStatus::Unauthorized,
            None,
        );
        // Bailed with clear usage + no last_error -> unusable purely on
        // the loading axis (set_loading(Bailed) clears usage, so tier is 0).
        map.set_loading(&AccountKey("bailed".to_owned()), LoadingState::Bailed);
        // Refreshing with clear tier-0 usage -> usable; only Bailed is
        // excluded on the loading axis, not Refreshing.
        map.set_usage(&AccountKey("refreshing".to_owned()), snapshot(Some(10.0), Some(20.0)));
        map.set_loading(&AccountKey("refreshing".to_owned()), LoadingState::Refreshing);

        assert!(map.is_account_usable(&AccountKey("ready-low".to_owned())));
        assert!(!map.is_account_usable(&AccountKey("saturated".to_owned())));
        assert!(!map.is_account_usable(&AccountKey("probe-rate-limited".to_owned())));
        assert!(!map.is_account_usable(&AccountKey("probe-expired".to_owned())));
        assert!(!map.is_account_usable(&AccountKey("probe-unauthorized".to_owned())));
        assert!(!map.is_account_usable(&AccountKey("bailed".to_owned())));
        assert!(map.is_account_usable(&AccountKey("refreshing".to_owned())));
    }

    #[test]
    fn priority_order_picks_first_in_pin_when_both_available() {
        // Stargate: 5h=80%
        // Gateway:  5h=10%
        // Both under 100% on both windows → tier 2. Priority-order
        // policy: first in pin order wins regardless of utilisation
        // (the user's pin expresses intent; the picker respects it).
        let mut map = AccountStateMap::new(&[make_account("Stargate"), make_account("Gateway")]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(80.0), Some(60.0)));
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(10.0), Some(20.0)));
        let (picked, _) = map.pick_for_project(&["Stargate".to_owned(), "Gateway".to_owned()]);
        assert_eq!(picked.0, "Stargate", "first pin entry wins when both are healthy");
    }

    #[test]
    fn priority_order_respects_pin_order_when_first_pin_has_higher_seven_day() {
        // Stargate: 5h=10%, 7d=90% - high 7d but still under 100%
        // Gateway:  5h=50%, 7d=50%
        // Both tier 2. Priority-order: first in pin wins.
        let mut map = AccountStateMap::new(&[make_account("Stargate"), make_account("Gateway")]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(10.0), Some(90.0)));
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&["Stargate".to_owned(), "Gateway".to_owned()]);
        assert_eq!(picked.0, "Stargate", "pin order over utilisation");
    }

    #[test]
    fn rate_limited_account_excluded_in_favour_of_available_one() {
        // Gateway: 5h=0%, 7d=100% - RATE LIMITED on 7d.
        // Stargate: 5h=80%, 7d=80% - heavily used but neither at 100%.
        // Picker must exclude Gateway even though its 5h is lower.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Stargate")]);
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(0.0), Some(100.0)));
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(80.0), Some(80.0)));
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Stargate".to_owned()]);
        assert_eq!(picked.0, "Stargate");
    }

    #[test]
    fn rate_limited_account_excluded_on_five_hour_too() {
        // Gateway: 5h=100%, 7d=0% - rate limited on 5h.
        // Stargate: 5h=80%, 7d=80% - available.
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
        // returns something (the spawn must not fail) - definition
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
        // most remaining (100%) but it's NOT in the pin - must be
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
    fn priority_order_ignores_seven_day_difference_when_both_available() {
        // Both at 5h=50, different 7d. Old policy used 7d as a
        // tiebreaker; the priority-order policy respects pin order
        // - Stargate first → Stargate wins even with worse 7d.
        let mut map = AccountStateMap::new(&[make_account("Stargate"), make_account("Gateway")]);
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(50.0), Some(70.0)));
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(50.0), Some(30.0)));
        let (picked, _) = map.pick_for_project(&["Stargate".to_owned(), "Gateway".to_owned()]);
        assert_eq!(picked.0, "Stargate", "pin order over 7d util");
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
    fn known_available_account_beats_probe_failed_account() {
        // Gateway has a fresh successful probe (tier 1, 30% used);
        // Personal's probe keeps returning 429 (last_error =
        // RateLimited, usage still None → tier 3, probe rate-limited).
        // Gateway wins.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Personal")]);
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(30.0), Some(30.0)));
        map.set_last_error(&AccountKey("Personal".to_owned()), UsageFetchStatus::RateLimited, None);
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Gateway", "available account must beat probe-rate-limited one");
    }

    #[test]
    fn expired_creds_and_saturated_both_unusable_pin_order_decides() {
        // Both accounts are unusable in different ways: Gateway1's
        // OAuth has expired, Personal is fully rate-limited. Under
        // the two-tier model both fall into tier 1 (Unusable) so
        // pin order decides - Gateway1 wins as first in pin.
        let mut map = AccountStateMap::new(&[make_account("Gateway1"), make_account("Personal")]);
        map.set_last_error(&AccountKey("Gateway1".to_owned()), UsageFetchStatus::Expired, None);
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(100.0), Some(100.0)));
        let (picked, _) = map.pick_for_project(&["Gateway1".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Gateway1", "all-unusable falls back to pin order");
    }

    #[test]
    fn unknown_and_available_both_usable_pin_order_decides() {
        // Both accounts are in tier 0 (Usable) under the two-tier
        // model - Gateway is unprobed (unknown), Personal has known
        // healthy usage. Pin order decides → Gateway wins.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Personal")]);
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(30.0), Some(30.0)));
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Gateway", "both usable → pin order");
    }

    #[test]
    fn network_failure_treated_as_usable_pin_order_decides() {
        // Gateway's probe failed transiently (network error) - we
        // don't actually know it's saturated, so the two-tier model
        // keeps it in tier 0 (Usable). Personal has a healthy probe
        // also in tier 0. Both usable → pin order wins → Gateway
        // (first in pin).
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Personal")]);
        map.set_last_error(
            &AccountKey("Gateway".to_owned()),
            UsageFetchStatus::NetworkFailed,
            None,
        );
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(50.0), Some(50.0)));
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Gateway", "transient network error doesn't demote - pin order wins");
    }

    #[test]
    fn round_robin_rotates_across_consecutive_picks() {
        // Three healthy accounts in the allow-list, all tier 0.
        // First pick (cursor=0) → first usable, second (cursor=1) →
        // second usable, third (cursor=2) → third usable, fourth
        // wraps back to first (cursor=3, 3 % 3 = 0).
        let map = AccountStateMap::new(&[
            make_account("Gateway"),
            make_account("Gateway1"),
            make_account("Personal"),
        ]);
        let allow = ["Gateway".to_owned(), "Gateway1".to_owned(), "Personal".to_owned()];
        let picks: Vec<String> = (0..4).map(|_| map.pick_for_project(&allow).0.0).collect();
        assert_eq!(
            picks,
            vec![
                "Gateway".to_owned(),
                "Gateway1".to_owned(),
                "Personal".to_owned(),
                "Gateway".to_owned(),
            ],
            "round-robin must rotate through the usable subset and wrap",
        );
    }

    #[test]
    fn round_robin_skips_unusable_in_rotation() {
        // Gateway is rate-limited (tier 1); Gateway1 + Personal are
        // tier 0. Rotation must alternate between the TWO usable
        // entries and never land on Gateway even though it's first
        // in the allow-list.
        let mut map = AccountStateMap::new(&[
            make_account("Gateway"),
            make_account("Gateway1"),
            make_account("Personal"),
        ]);
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(100.0), Some(100.0)));
        map.set_usage(&AccountKey("Gateway1".to_owned()), snapshot(Some(20.0), Some(20.0)));
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(30.0), Some(30.0)));
        let allow = ["Gateway".to_owned(), "Gateway1".to_owned(), "Personal".to_owned()];
        let picks: Vec<String> = (0..4).map(|_| map.pick_for_project(&allow).0.0).collect();
        assert_eq!(
            picks,
            vec![
                "Gateway1".to_owned(),
                "Personal".to_owned(),
                "Gateway1".to_owned(),
                "Personal".to_owned(),
            ],
            "round-robin must skip Gateway (tier 1) and alternate between the two usable accounts",
        );
    }

    #[test]
    fn round_robin_cursor_is_global_across_projects() {
        // Two projects share the SAME AccountStateMap (the cursor is
        // a single field on the map, not per-project). Interleaved
        // picks from project A and project B share the cursor, so
        // each pick advances the shared cursor regardless of which
        // project asked.
        let map = AccountStateMap::new(&[make_account("Gateway"), make_account("Gateway1")]);
        let project_a = ["Gateway".to_owned(), "Gateway1".to_owned()];
        let project_b = ["Gateway".to_owned(), "Gateway1".to_owned()];
        // Pick: A (cursor=0 → Gateway), B (cursor=1 → Gateway1),
        //       A (cursor=2 → Gateway), B (cursor=3 → Gateway1).
        let picks = vec![
            map.pick_for_project(&project_a).0.0,
            map.pick_for_project(&project_b).0.0,
            map.pick_for_project(&project_a).0.0,
            map.pick_for_project(&project_b).0.0,
        ];
        assert_eq!(
            picks,
            vec![
                "Gateway".to_owned(),
                "Gateway1".to_owned(),
                "Gateway".to_owned(),
                "Gateway1".to_owned(),
            ],
            "cursor must be shared across projects, not reset per project",
        );
    }

    #[test]
    fn round_robin_with_single_usable_account_always_picks_it() {
        // Only Gateway is usable; the other two are saturated. Every
        // pick lands on Gateway - `cursor % 1 == 0` collapses the
        // rotation to a single account.
        let mut map = AccountStateMap::new(&[
            make_account("Gateway"),
            make_account("Gateway1"),
            make_account("Personal"),
        ]);
        map.set_usage(&AccountKey("Gateway".to_owned()), snapshot(Some(10.0), Some(10.0)));
        map.set_usage(&AccountKey("Gateway1".to_owned()), snapshot(Some(100.0), Some(100.0)));
        map.set_usage(&AccountKey("Personal".to_owned()), snapshot(Some(100.0), Some(100.0)));
        let allow = ["Gateway".to_owned(), "Gateway1".to_owned(), "Personal".to_owned()];
        for _ in 0..5 {
            let (picked, _) = map.pick_for_project(&allow);
            assert_eq!(picked.0, "Gateway");
        }
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
        let map = AccountStateMap::new(&[make_account("Gateway")]);
        assert!(map.should_probe_now(&AccountKey("Gateway".to_owned())));
    }

    #[test]
    fn set_last_error_schedules_next_probe_in_future() {
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let key = AccountKey("Gateway".to_owned());
        map.set_last_error(&key, UsageFetchStatus::RateLimited, None);
        assert!(!map.should_probe_now(&key), "first failure puts account in backoff");
    }

    #[test]
    fn retry_after_overrides_exponential_backoff() {
        // Anthropic returns Retry-After: 3048 (seconds) for a deeply
        // rate-limited account. Our local exponential schedule would
        // pick 30 s for the first failure - vastly under-shoot the
        // actual reset and re-trip the limit. The server-provided
        // retry_after must win.
        use std::time::Duration;
        let mut map = AccountStateMap::new(&[make_account("Gateway1")]);
        let key = AccountKey("Gateway1".to_owned());
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
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let key = AccountKey("Gateway".to_owned());
        map.set_last_error(&key, UsageFetchStatus::RateLimited, None);
        assert!(!map.should_probe_now(&key));
        map.set_usage(&key, snapshot(Some(10.0), Some(10.0)));
        assert!(map.should_probe_now(&key), "successful probe clears the backoff gate");
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
        s.seven_day_opus =
            Some(UsageWindow { utilization: 80.0, resets_at: None, reset_description: None });
        assert_eq!(s.seven_day_util(), Some(80.0), "the most-used 7d window is the binding one");
    }

    /// An empty fold produced 0.0 with nothing that looked like a
    /// default, so a 200 carrying only the session window reported the
    /// account as 0% used across seven days.
    #[test]
    fn seven_day_util_is_none_when_every_window_is_absent() {
        let mut s = snapshot(Some(20.0), None);
        s.seven_day_opus = None;
        s.seven_day_sonnet = None;
        assert_eq!(s.seven_day_util(), None, "no 7d window means no 7d reading");
        assert_eq!(s.five_hour_util(), Some(20.0), "the window that is present still reports");
    }

    #[test]
    fn is_rate_limited_fires_on_either_window() {
        assert!(is_rate_limited(&snapshot(Some(100.0), Some(0.0))));
        assert!(is_rate_limited(&snapshot(Some(0.0), Some(100.0))));
        assert!(is_rate_limited(&snapshot(Some(100.0), Some(100.0))));
        assert!(!is_rate_limited(&snapshot(Some(99.9), Some(99.9))));
    }

    /// Build a snapshot with explicit `resets_at` values per window. The
    /// public `snapshot()` helper hides this for the common test case
    /// (resets in the future); this variant lets the
    /// staleness/transition tests drive the resets_at clock.
    fn snapshot_with_resets(
        five_hour: Option<(f64, Option<SystemTime>)>,
        seven_day: Option<(f64, Option<SystemTime>)>,
    ) -> UsageSnapshot {
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: SystemTime::UNIX_EPOCH,
            five_hour: five_hour.map(|(util, resets_at)| UsageWindow {
                utilization: util,
                resets_at,
                reset_description: None,
            }),
            seven_day: seven_day.map(|(util, resets_at)| UsageWindow {
                utilization: util,
                resets_at,
                reset_description: None,
            }),
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
        }
    }

    #[test]
    fn is_rate_limited_false_when_cached_window_reset_already_passed() {
        // Stale cached snapshot: probe reported 100% an hour ago but
        // the reset moment has come and gone. Predicate must return
        // false so the rotation re-considers the account WITHOUT
        // waiting for a fresh probe to overwrite the cache.
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        let snap = snapshot_with_resets(Some((100.0, Some(past))), Some((50.0, Some(past))));
        assert!(!is_rate_limited(&snap), "stale 100% reading must not strand the account");
    }

    #[test]
    fn is_rate_limited_true_when_only_one_window_is_capped_and_still_in_window() {
        // 5h at 100% + still in window; 7d below cap. Either window
        // can keep the account out of the pool while it's in its
        // hold-down period.
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        let snap = snapshot_with_resets(Some((100.0, Some(future))), Some((30.0, Some(future))));
        assert!(is_rate_limited(&snap));
    }

    /// Build a snapshot exercising the opus-specific 7-day window.
    /// Max-plan accounts hit the opus cap independently of the
    /// shared 7-day envelope, and `is_rate_limited` must walk every
    /// window in the four-tuple - a future short-circuit on
    /// `seven_day` would silently break Max-plan classification.
    fn opus_only_capped_snapshot(util: f64, resets_at: Option<SystemTime>) -> UsageSnapshot {
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: SystemTime::UNIX_EPOCH,
            five_hour: Some(UsageWindow {
                utilization: 30.0,
                resets_at: Some(SystemTime::now() + std::time::Duration::from_secs(3600)),
                reset_description: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: 30.0,
                resets_at: Some(SystemTime::now() + std::time::Duration::from_secs(3600)),
                reset_description: None,
            }),
            seven_day_opus: Some(UsageWindow {
                utilization: util,
                resets_at,
                reset_description: None,
            }),
            seven_day_sonnet: None,
            extra_usage: None,
            spend: None,
        }
    }

    /// Sister of `opus_only_capped_snapshot` for the sonnet 7-day
    /// window. Same rationale: a sonnet-only cap must bubble through
    /// `is_rate_limited` even when no other window is at the limit.
    fn sonnet_only_capped_snapshot(util: f64, resets_at: Option<SystemTime>) -> UsageSnapshot {
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: SystemTime::UNIX_EPOCH,
            five_hour: Some(UsageWindow {
                utilization: 30.0,
                resets_at: Some(SystemTime::now() + std::time::Duration::from_secs(3600)),
                reset_description: None,
            }),
            seven_day: Some(UsageWindow {
                utilization: 30.0,
                resets_at: Some(SystemTime::now() + std::time::Duration::from_secs(3600)),
                reset_description: None,
            }),
            seven_day_opus: None,
            seven_day_sonnet: Some(UsageWindow {
                utilization: util,
                resets_at,
                reset_description: None,
            }),
            extra_usage: None,
            spend: None,
        }
    }

    #[test]
    fn is_rate_limited_fires_on_opus_only_capped_window() {
        // Max-plan reality: shared 5h + 7d windows healthy, opus-7d
        // at cap. The picker must classify as rate-limited or the
        // account keeps getting picked and trips API 429s.
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        let snap = opus_only_capped_snapshot(100.0, Some(future));
        assert!(is_rate_limited(&snap), "opus 7d cap (alone) must be detected");
    }

    #[test]
    fn is_rate_limited_fires_on_sonnet_only_capped_window() {
        // Symmetry with opus: sonnet-7d at cap must also bubble up.
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        let snap = sonnet_only_capped_snapshot(100.0, Some(future));
        assert!(is_rate_limited(&snap), "sonnet 7d cap (alone) must be detected");
    }

    #[test]
    fn is_rate_limited_false_when_opus_capped_but_reset_passed() {
        // Stale opus-window snapshot - resets_at has come and gone.
        // Must classify as NOT rate-limited so the picker reconsiders
        // the account; the same self-clearing applies per-window.
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        let snap = opus_only_capped_snapshot(100.0, Some(past));
        assert!(!is_rate_limited(&snap), "stale opus window must self-clear like any other");
    }

    #[test]
    fn is_rate_limited_false_when_window_at_cap_but_no_reset_scheduled() {
        // None means "no scheduled reset" - we cannot prove the limit
        // is still in effect. Treating it as limited would forever
        // strand the account on a single bad cached reading.
        let snap = snapshot_with_resets(Some((100.0, None)), Some((100.0, None)));
        assert!(!is_rate_limited(&snap), "None resets_at must not classify as limited");
    }

    #[test]
    fn has_just_cleared_cap_window_true_when_cached_window_reset_passed() {
        // Cache snapshot has util 100% with resets_at in the past:
        // the scheduler hook must fire so a fresh probe overwrites
        // the stale bar.
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        let stale = snapshot_with_resets(Some((100.0, Some(past))), Some((30.0, Some(past))));
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        map.set_usage(&k, stale);
        assert!(map.has_just_cleared_cap_window(&k));
    }

    #[test]
    fn has_just_cleared_cap_window_false_when_window_still_in_cap() {
        // Cap window still active: hook must not fire (the existing
        // `should_probe_now` gate covers normal cadence).
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        let live = snapshot_with_resets(Some((100.0, Some(future))), Some((30.0, Some(future))));
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        map.set_usage(&k, live);
        assert!(!map.has_just_cleared_cap_window(&k));
    }

    #[test]
    fn has_just_cleared_cap_window_false_when_no_snapshot_cached() {
        // Cold cache - nothing to compare against. Hook returns false;
        // the normal cold-cache `should_probe_now == true` already
        // schedules the first probe.
        let map = AccountStateMap::new(&[make_account("Gateway")]);
        assert!(!map.has_just_cleared_cap_window(&AccountKey("Gateway".to_owned())));
    }

    #[test]
    fn has_just_cleared_cap_window_false_when_below_cap() {
        // Below-cap snapshot with a past resets_at: NOT a freshly
        // cleared cap, just a stale-but-irrelevant timestamp. The
        // hook only cares about the at-cap transition.
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        let snap = snapshot_with_resets(Some((50.0, Some(past))), Some((30.0, Some(past))));
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        map.set_usage(&k, snap);
        assert!(!map.has_just_cleared_cap_window(&k));
    }

    #[test]
    fn override_disarms_after_firing_and_does_not_refire_until_rearmed() {
        // Setup mirrors the production sustained-failure shape: a
        // stale cached snapshot (util=100, resets_at in the past)
        // plus an active backoff timer from a recent 429. Without
        // the disarm gate, every poll cycle would re-fire the hook
        // and hammer Anthropic through the entire backoff window.
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        let stale = snapshot_with_resets(Some((100.0, Some(past))), None);
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        map.set_usage(&k, stale);
        // Push next_probe_at into the future so should_probe_now is
        // false: this is the "in backoff" state the scheduler hook is
        // meant to override.
        map.set_last_error(
            &k,
            UsageFetchStatus::RateLimited,
            Some(std::time::Duration::from_secs(3000)),
        );
        assert!(!map.should_probe_now(&k), "backoff active");
        assert!(map.has_just_cleared_cap_window(&k), "hook fires while armed");

        // Scheduler picks up the account via the hook + fires its
        // override probe; disarm runs.
        map.disarm_override(&k);
        assert!(!map.has_just_cleared_cap_window(&k), "disarmed; no second override this cycle");
        assert!(!map.should_probe_now(&k), "backoff still active");
        // Combined scheduler signal: neither predicate fires - the
        // account waits for the backoff timer to elapse.
        assert!(
            !map.should_probe_now(&k) && !map.has_just_cleared_cap_window(&k),
            "scheduler must respect backoff once the override has been consumed",
        );
    }

    #[test]
    fn fresh_set_usage_rearms_override_for_next_reset_boundary() {
        // After the override fires + disarms once, a NEW successful
        // probe (set_usage) must re-arm the hook so future stale-reset
        // boundaries can still trigger their one-shot override.
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        let stale = snapshot_with_resets(Some((100.0, Some(past))), None);
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        map.set_usage(&k, stale.clone());
        map.disarm_override(&k);
        assert!(!map.has_just_cleared_cap_window(&k), "disarmed");

        // A fresh probe lands - even if the snapshot is STILL
        // stale-reset, the hook re-arms because set_usage represents
        // a brand-new probe attempt's success.
        map.set_usage(&k, stale);
        assert!(map.has_just_cleared_cap_window(&k), "fresh set_usage re-arms the hook");
    }

    #[test]
    fn override_not_armed_on_construction_so_hook_skips_cold_accounts() {
        // Without an arming step, a brand-new AccountStateMap entry
        // must NOT fire the hook even if some hypothetical stale
        // snapshot were planted manually. Cold accounts are picked up
        // via cold-cache `should_probe_now == true`, not the override.
        let map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        assert!(!map.has_just_cleared_cap_window(&k), "no arm = no override");
    }

    // -------------------------------------------------------------
    // Scheduler OR truth table - pins the combined signal the
    // workspace poller relies on. A future refactor that swaps the
    // OR for AND or drops one half loses the behavior with CI green
    // unless this table covers it.
    // -------------------------------------------------------------

    #[test]
    fn scheduler_should_probe_fires_via_should_probe_now_alone() {
        // Cold-cache account: no failures yet, no snapshot. The normal
        // `should_probe_now` returns true (next_probe_at is None);
        // the hook returns false (no snapshot to override against).
        // OR result: true.
        let map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        assert!(map.should_probe_now(&k));
        assert!(!map.has_just_cleared_cap_window(&k));
        assert!(map.scheduler_should_probe(&k), "OR true via should_probe_now");
    }

    #[test]
    fn scheduler_should_probe_fires_via_hook_alone_during_active_backoff() {
        // The signature override scenario from the planner: a stale
        // cached snapshot (util=100, resets_at in the past) paired
        // with an active backoff timer from a recent 429. The hook
        // must be the deciding factor:
        // - should_probe_now == false (backoff active)
        // - has_just_cleared_cap_window == true (armed + stale reset)
        // - scheduler_should_probe == true (OR fires)
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        let stale = snapshot_with_resets(Some((100.0, Some(past))), None);
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        map.set_usage(&k, stale);
        map.set_last_error(
            &k,
            UsageFetchStatus::RateLimited,
            Some(std::time::Duration::from_secs(3000)),
        );

        assert!(!map.should_probe_now(&k), "backoff active");
        assert!(map.has_just_cleared_cap_window(&k), "armed + stale reset");
        assert!(map.scheduler_should_probe(&k), "OR fires when hook alone is true");
    }

    #[test]
    fn scheduler_should_probe_skips_when_both_predicates_false() {
        // Backoff active AND the snapshot has a future resets_at (no
        // stale state). Both sides of the OR return false; the
        // account is skipped this cycle and waits for the backoff
        // timer to elapse.
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        let live = snapshot_with_resets(Some((100.0, Some(future))), None);
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = AccountKey("Gateway".to_owned());
        map.set_usage(&k, live);
        map.set_last_error(
            &k,
            UsageFetchStatus::RateLimited,
            Some(std::time::Duration::from_secs(3000)),
        );

        assert!(!map.should_probe_now(&k));
        assert!(!map.has_just_cleared_cap_window(&k));
        assert!(!map.scheduler_should_probe(&k), "OR false when both sides are false");
    }

    #[test]
    fn pick_returns_account_once_cached_window_reset_passes() {
        // Cached snapshot says 100% but the resets_at has come and gone
        // (no fresh probe has overwritten the cache yet). The picker
        // must put the account back into the usable tier rather than
        // hold it out indefinitely.
        let past = SystemTime::now() - std::time::Duration::from_secs(60);
        let stale_snap = snapshot_with_resets(Some((100.0, Some(past))), Some((100.0, Some(past))));
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Stargate")]);
        map.set_usage(&AccountKey("Gateway".to_owned()), stale_snap);
        // Stargate stays healthy (snapshot helper sets resets_at in
        // the future so 100% IS still limited for Stargate).
        map.set_usage(&AccountKey("Stargate".to_owned()), snapshot(Some(100.0), Some(100.0)));
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Stargate".to_owned()]);
        assert_eq!(picked.0, "Gateway", "stale-reset Gateway usable; live-capped Stargate not");
    }

    // ---------------------------------------------------------------
    // #246: LoadingState gates the launchpad + drives assignment plan.
    //
    // Replaces the PR #238 `consecutive_unauthorized` 3-strike counter:
    // a single 401 now transitions to `LoadingState::Bailed` (clearing
    // the cached usage), and the 30 s recovery poll
    // (account_loader::run_recovery_poll) is what absorbs transient
    // failures by retrying from Loading once auth_status reports
    // logged-in. The user-visible effect is the same - bailed accounts
    // surface the `⚠ unauthorized - /login` label instead of a stale
    // %bar - just the storage shape moved.
    // ---------------------------------------------------------------

    fn key(name: &str) -> AccountKey {
        AccountKey(name.to_owned())
    }

    #[test]
    fn loading_state_initial_is_loading() {
        // Brand-new account state has no probe result yet; the launchpad
        // dims its row + footer glyph until the loading task lands a
        // terminal verdict (Ready or Bailed).
        let map = AccountStateMap::new(&[make_account("Gateway")]);
        assert_eq!(map.loading_state(&key("Gateway")), LoadingState::Loading);
    }

    #[test]
    fn loading_state_returns_loading_for_unknown_key() {
        // Defensive: the launchpad render path may briefly hold a key
        // that hasn't been registered yet; treat unknown keys as
        // Loading rather than panicking.
        let map = AccountStateMap::new(&[make_account("Gateway")]);
        assert_eq!(map.loading_state(&key("NotRegistered")), LoadingState::Loading);
    }

    #[test]
    fn set_usage_transitions_loading_to_ready() {
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = key("Gateway");
        assert_eq!(map.loading_state(&k), LoadingState::Loading);
        map.set_usage(&k, snapshot(Some(30.0), Some(40.0)));
        assert_eq!(map.loading_state(&k), LoadingState::Ready);
    }

    #[test]
    fn set_last_error_unauthorized_transitions_to_bailed_and_clears_usage() {
        // Subsumes the PR #238 three-strike test. Single Unauthorized
        // (not three) now flips loading to Bailed and clears the
        // cached usage so the renderer drops the stale %bar in favour
        // of the unauthorized label. The recovery poll re-runs the
        // loading task once auth_status reports logged-in.
        let mut map = AccountStateMap::new(&[make_account("Personal")]);
        let k = key("Personal");
        map.set_usage(&k, snapshot(Some(30.0), Some(40.0)));
        assert!(map.usage(&k).is_some(), "cache primed");
        assert_eq!(map.loading_state(&k), LoadingState::Ready);

        map.set_last_error(&k, UsageFetchStatus::Unauthorized, None);

        assert_eq!(map.loading_state(&k), LoadingState::Bailed);
        assert!(map.usage(&k).is_none(), "Bailed clears usage by construction");
    }

    #[test]
    fn set_last_error_expired_transitions_to_bailed_and_clears_usage() {
        // Expired is in the same auth-recovery family as Unauthorized;
        // both drive the same Bailed transition.
        let mut map = AccountStateMap::new(&[make_account("Personal")]);
        let k = key("Personal");
        map.set_usage(&k, snapshot(Some(30.0), Some(40.0)));

        map.set_last_error(&k, UsageFetchStatus::Expired, None);

        assert_eq!(map.loading_state(&k), LoadingState::Bailed);
        assert!(map.usage(&k).is_none());
    }

    #[test]
    fn set_last_error_rate_limited_leaves_loading_unchanged() {
        // RateLimited is a probe-side throttle, not an auth failure;
        // loading state must NOT transition to Bailed. The cached
        // usage is also preserved so the bottom panel can keep showing
        // the last known-good bars while the probe backs off.
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = key("Gateway");
        map.set_usage(&k, snapshot(Some(30.0), Some(40.0)));
        assert_eq!(map.loading_state(&k), LoadingState::Ready);

        map.set_last_error(&k, UsageFetchStatus::RateLimited, None);

        assert_eq!(map.loading_state(&k), LoadingState::Ready, "RateLimited preserves Ready");
        assert!(map.usage(&k).is_some(), "cache preserved under RateLimited");
    }

    #[test]
    fn set_last_error_network_failed_leaves_loading_unchanged() {
        // Network errors are transient and unknown - the loading state
        // stays at whatever it was, and the cache is preserved.
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = key("Gateway");
        map.set_usage(&k, snapshot(Some(30.0), Some(40.0)));

        map.set_last_error(&k, UsageFetchStatus::NetworkFailed, None);

        assert_eq!(map.loading_state(&k), LoadingState::Ready);
        assert!(map.usage(&k).is_some());
    }

    #[test]
    fn set_loading_bailed_clears_usage() {
        // Driven by the loading task to clear stale snapshots when
        // transitioning to Bailed; mirrors the side-effect that
        // set_last_error performs for the auth-error case.
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = key("Gateway");
        map.set_usage(&k, snapshot(Some(30.0), Some(40.0)));

        map.set_loading(&k, LoadingState::Bailed);

        assert_eq!(map.loading_state(&k), LoadingState::Bailed);
        assert!(map.usage(&k).is_none(), "explicit set_loading(Bailed) also clears usage");
    }

    #[test]
    fn set_loading_to_loading_does_not_clear_usage() {
        // Recovery poll transitions Bailed → Loading when auth_status
        // flips back. The transition itself shouldn't wipe a cache
        // that might've been re-primed since the bail. (In practice
        // the cache is already None on a Bailed account, but the
        // contract should be explicit.)
        let mut map = AccountStateMap::new(&[make_account("Gateway")]);
        let k = key("Gateway");
        map.set_usage(&k, snapshot(Some(30.0), Some(40.0)));

        map.set_loading(&k, LoadingState::Loading);

        assert_eq!(map.loading_state(&k), LoadingState::Loading);
        assert!(map.usage(&k).is_some(), "transition to Loading preserves any cached snapshot");
    }

    #[test]
    fn bailed_account_recovers_to_ready_via_set_usage() {
        // Recovery flow: account got Bailed by a probe failure; the
        // recovery poll's re-run of loading lands a fresh probe;
        // set_usage transitions back to Ready, re-priming the cache.
        let mut map = AccountStateMap::new(&[make_account("Personal")]);
        let k = key("Personal");
        map.set_usage(&k, snapshot(Some(30.0), Some(40.0)));
        map.set_last_error(&k, UsageFetchStatus::Unauthorized, None);
        assert_eq!(map.loading_state(&k), LoadingState::Bailed);

        map.set_usage(&k, snapshot(Some(50.0), Some(60.0)));

        assert_eq!(map.loading_state(&k), LoadingState::Ready);
        assert!(map.usage(&k).is_some());
    }

    #[test]
    fn all_loaded_true_for_empty_map() {
        let map = AccountStateMap::empty_for_test();
        assert!(map.all_loaded(), "empty map is vacuously all-loaded");
    }

    #[test]
    fn all_loaded_false_when_any_loading() {
        let map = AccountStateMap::new(&[make_account("Gateway"), make_account("Personal")]);
        assert!(!map.all_loaded(), "fresh accounts start in Loading; gate must stay closed");
    }

    #[test]
    fn all_loaded_false_when_any_refreshing() {
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Personal")]);
        map.set_loading(&key("Gateway"), LoadingState::Refreshing);
        map.set_usage(&key("Personal"), snapshot(Some(10.0), Some(20.0)));
        assert!(!map.all_loaded(), "Refreshing is mid-flight, not terminal");
    }

    #[test]
    fn pick_for_project_skips_bailed_accounts() {
        // Bailed account is in the allow list with no last_error (the
        // recovery poll explicitly transitioned via set_loading, not
        // set_last_error). Without the LoadingState filter, tier_of
        // would classify it as tier 0 (usable=true) because both
        // usage and last_error are None. The picker must NOT return
        // it; the Ready account must win.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Personal")]);
        // Gateway: ready
        map.set_usage(&key("Gateway"), snapshot(Some(20.0), Some(20.0)));
        // Personal: bailed via direct set_loading (mirrors recovery
        // poll's auth_status=logged_out -> Bailed path, which has
        // no associated last_error).
        map.set_loading(&key("Personal"), LoadingState::Bailed);
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Personal".to_owned()]);
        assert_eq!(
            picked.0, "Gateway",
            "pick_for_project must skip Bailed even without a recent last_error",
        );
    }

    #[test]
    fn pick_for_project_all_bailed_falls_back_to_first() {
        // Every allow-list entry is Bailed. The fallback path still
        // returns the first allow-list entry so spawn proceeds and
        // the user sees the spawned subprocess's own error rather
        // than forge silently refusing.
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Personal")]);
        map.set_loading(&key("Gateway"), LoadingState::Bailed);
        map.set_loading(&key("Personal"), LoadingState::Bailed);
        let (picked, _) = map.pick_for_project(&["Gateway".to_owned(), "Personal".to_owned()]);
        assert_eq!(picked.0, "Gateway");
    }

    #[test]
    fn pick_for_project_excludes_experimental_even_when_only_usable() {
        // Exp is experimental AND the only usable account; Regular is
        // Bailed. The picker must still refuse Exp and fall back to the
        // non-experimental candidate rather than assign an experimental
        // account. Experimental accounts are picker-only.
        let mut exp = make_account("Exp");
        exp.experimental = true;
        let mut map = AccountStateMap::new(&[make_account("Regular"), exp]);
        map.set_loading(&key("Regular"), LoadingState::Bailed);
        map.set_usage(&key("Exp"), snapshot(Some(10.0), Some(10.0)));
        let (picked, _) = map.pick_for_project(&["Regular".to_owned(), "Exp".to_owned()]);
        assert_eq!(
            picked.0, "Regular",
            "experimental account is never auto-assigned, even as the only usable one",
        );
    }

    #[test]
    fn pick_for_project_never_returns_experimental_as_last_resort() {
        // The allow-list contains ONLY an experimental account. It is
        // filtered out entirely, so the last-resort fallback skips it
        // and returns the first non-experimental account instead.
        let mut exp = make_account("Exp");
        exp.experimental = true;
        let mut map = AccountStateMap::new(&[exp, make_account("Regular")]);
        map.set_usage(&key("Exp"), snapshot(Some(10.0), Some(10.0)));
        map.set_usage(&key("Regular"), snapshot(Some(10.0), Some(10.0)));
        let (picked, _) = map.pick_for_project(&["Exp".to_owned()]);
        assert_eq!(
            picked.0, "Regular",
            "experimental never returned; last-resort skips to a non-experimental account",
        );
    }

    #[test]
    fn all_loaded_true_when_mix_of_ready_and_bailed() {
        let mut map = AccountStateMap::new(&[make_account("Gateway"), make_account("Personal")]);
        map.set_usage(&key("Gateway"), snapshot(Some(10.0), Some(20.0)));
        map.set_last_error(&key("Personal"), UsageFetchStatus::Unauthorized, None);
        assert_eq!(map.loading_state(&key("Gateway")), LoadingState::Ready);
        assert_eq!(map.loading_state(&key("Personal")), LoadingState::Bailed);
        assert!(map.all_loaded(), "Ready + Bailed are both terminal states");
    }
}
