//! Account selection — internal to forge-workspace.
//!
//! `Workspace::get_agent_handle` consults `pick_for_project` on
//! every spawn; the chosen `AccountKey` becomes the spawned Agent's
//! `CLAUDE_CONFIG_DIR` override. Persistence is handled by
//! `state.rs`.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::config::{LoadedAccount, SelectionPolicy};

/// Internal newtype wrapping the account's `display_name`. Phase
/// 1b doesn't surface this publicly.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub(crate) struct AccountKey(pub String);

#[derive(Debug, Clone)]
pub(crate) struct AccountState {
    pub config_dir: PathBuf,
    pub last_used_at: Option<SystemTime>,
}

#[derive(Debug)]
pub(crate) struct AccountStateMap {
    pub ordered_keys: Vec<AccountKey>, // forge.toml definition order
    pub by_key: std::collections::HashMap<AccountKey, AccountState>,
    pub policy: SelectionPolicy,
    pub round_robin_next: usize,
}

impl AccountStateMap {
    /// Empty map for the `testing` feature's `Workspace::testing_stub`.
    /// Production code paths reach this map only via account pickers
    /// (`pick_for_project`), which a test fixture should never exercise.
    #[cfg(feature = "testing")]
    pub fn empty_for_test() -> Self {
        Self {
            ordered_keys: Vec::new(),
            by_key: std::collections::HashMap::new(),
            policy: SelectionPolicy::default(),
            round_robin_next: 0,
        }
    }

    pub fn new(
        accounts: &[LoadedAccount],
        policy: SelectionPolicy,
        persisted_round_robin_next: Option<usize>,
        persisted_last_used: &std::collections::HashMap<String, Option<SystemTime>>,
    ) -> Self {
        let mut ordered_keys = Vec::with_capacity(accounts.len());
        let mut by_key = std::collections::HashMap::with_capacity(accounts.len());
        for account in accounts {
            let key = AccountKey(account.display_name.clone());
            ordered_keys.push(key.clone());
            let last_used_at =
                persisted_last_used.get(&account.display_name).and_then(|x| x.as_ref()).copied();
            by_key
                .insert(key, AccountState { config_dir: account.config_dir.clone(), last_used_at });
        }
        Self {
            ordered_keys,
            by_key,
            policy,
            round_robin_next: persisted_round_robin_next.unwrap_or(0),
        }
    }

    /// Project-scoped pick. `allowed` is the optional account-name
    /// whitelist from `[[projects]].accounts`. When set, the LRU /
    /// round-robin policy applies only to the subset; the picked
    /// account's `last_used_at` is stamped so the global LRU clock
    /// stays consistent for unpinned projects. `None` falls through
    /// to the full account pool (today's behaviour).
    ///
    /// Unknown names in `allowed` are silently skipped — config-load
    /// already validates that every name resolves to a defined
    /// account, so this is defence-in-depth. Empty / all-skipped
    /// `allowed` falls back to the unrestricted pool with a warning
    /// (also a config-load invariant; the runtime guard exists so a
    /// future refactor that drops the strict load can't crash spawn).
    ///
    /// The lookup back into `by_key` is structurally infallible
    /// because both pickers only return keys constructed in
    /// [`Self::new`]. The `if let Some(...)` is just a clippy-
    /// friendly form; the path is otherwise an effective no-op.
    pub fn pick_for_project(
        &mut self,
        allowed: Option<&[String]>,
        now: SystemTime,
    ) -> (AccountKey, PathBuf) {
        let allowed_keys = allowed.map(|names| {
            names
                .iter()
                .map(|n| AccountKey(n.clone()))
                .filter(|k| self.by_key.contains_key(k))
                .collect::<Vec<_>>()
        });
        let restricted: Option<&[AccountKey]> = match allowed_keys.as_deref() {
            Some([]) => {
                tracing::warn!(
                    target: "forge_workspace::account",
                    "project pinned accounts list resolved to zero known names; falling back to global pool",
                );
                None
            }
            Some(keys) => Some(keys),
            None => None,
        };

        let picked = match self.policy {
            SelectionPolicy::LeastRecentlyUsed => self.pick_lru(restricted),
            SelectionPolicy::RoundRobin => self.pick_round_robin(restricted),
        };
        let dir = if let Some(state) = self.by_key.get_mut(&picked) {
            state.last_used_at = Some(now);
            state.config_dir.clone()
        } else {
            // Unreachable in practice — pickers only ever return
            // keys constructed in `new()`. Returning an empty
            // path lets the caller surface a clear failure rather
            // than panicking.
            tracing::error!(
                target: "forge_workspace::account",
                key = %picked.0,
                "picker returned key not present in by_key map; invariant violated",
            );
            PathBuf::new()
        };
        (picked, dir)
    }

    fn pick_lru(&self, restricted: Option<&[AccountKey]>) -> AccountKey {
        // Sort: None first; then ascending Some; tie-break alphabetical.
        // `restricted` narrows the candidate pool to a project's pinned
        // accounts; `None` means the full account pool.
        let mut candidates: Vec<&AccountKey> = match restricted {
            Some(subset) => subset.iter().collect(),
            None => self.ordered_keys.iter().collect(),
        };
        candidates.sort_by(|a, b| {
            let a_used = self.by_key.get(a).and_then(|s| s.last_used_at);
            let b_used = self.by_key.get(b).and_then(|s| s.last_used_at);
            match (a_used, b_used) {
                (None, None) => a.0.cmp(&b.0),
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(a_t), Some(b_t)) => a_t.cmp(&b_t).then_with(|| a.0.cmp(&b.0)),
            }
        });
        candidates[0].clone()
    }

    fn pick_round_robin(&mut self, restricted: Option<&[AccountKey]>) -> AccountKey {
        // Restricted pool gets its own modular cycle off the shared
        // counter. Sharing the counter keeps the visit order stable
        // for the unpinned case while still cycling within the
        // subset for pinned projects.
        let pool: &[AccountKey] = match restricted {
            Some(subset) => subset,
            None => &self.ordered_keys,
        };
        let idx = self.round_robin_next % pool.len();
        let picked = pool[idx].clone();
        self.round_robin_next = self.round_robin_next.wrapping_add(1);
        picked
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn make_account(name: &str) -> LoadedAccount {
        LoadedAccount {
            display_name: name.to_owned(),
            config_dir: PathBuf::from(format!("/fake/{name}")),
        }
    }

    #[test]
    fn lru_picks_oldest_first() {
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        let mut last_used: std::collections::HashMap<String, Option<SystemTime>> =
            std::collections::HashMap::new();
        last_used.insert("Granite".to_owned(), Some(later));
        last_used.insert("Subspace".to_owned(), Some(earlier));

        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &last_used,
        );
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3000);
        let (picked, _) = map.pick_for_project(None, now);
        assert_eq!(picked.0, "Subspace");
        // Picked account's last_used_at is updated to `now`.
        assert_eq!(map.by_key.get(&picked).unwrap().last_used_at, Some(now));
    }

    #[test]
    fn lru_none_sorts_before_some() {
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        let mut last_used = std::collections::HashMap::new();
        last_used.insert("Subspace".to_owned(), Some(later));
        // Granite has never been used (no entry).

        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &last_used,
        );
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3000);
        let (picked, _) = map.pick_for_project(None, now);
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn lru_tie_breaks_alphabetically_when_both_never_used() {
        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &std::collections::HashMap::new(),
        );
        let now = SystemTime::UNIX_EPOCH;
        let (picked, _) = map.pick_for_project(None, now);
        // Granite < Subspace alphabetically.
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn round_robin_cycles_in_definition_order() {
        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::RoundRobin,
            None,
            &std::collections::HashMap::new(),
        );
        let now = SystemTime::UNIX_EPOCH;
        let (a, _) = map.pick_for_project(None, now);
        let (b, _) = map.pick_for_project(None, now);
        let (c, _) = map.pick_for_project(None, now);
        assert_eq!(a.0, "Subspace");
        assert_eq!(b.0, "Granite");
        assert_eq!(c.0, "Subspace");
    }

    #[test]
    fn round_robin_resumes_from_persisted_next() {
        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::RoundRobin,
            Some(1), // resume at index 1 = Granite
            &std::collections::HashMap::new(),
        );
        let now = SystemTime::UNIX_EPOCH;
        let (picked, _) = map.pick_for_project(None, now);
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn lru_restricted_pool_only_picks_within_subset() {
        // Two accounts with a global LRU clock that would otherwise
        // pick Granite (never used). Pin "Subspace" only — picker must
        // pick Subspace despite Granite being globally older.
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        let mut last_used = std::collections::HashMap::new();
        last_used.insert("Subspace".to_owned(), Some(later));

        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &last_used,
        );
        let allowed = vec!["Subspace".to_owned()];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3000);
        let (picked, _) = map.pick_for_project(Some(&allowed), now);
        assert_eq!(picked.0, "Subspace");
    }

    #[test]
    fn lru_restricted_pool_lru_within_subset() {
        // Three accounts, pin two. Picker chooses the LRU of the
        // pinned pair, ignoring the third account's clock.
        let earlier = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        let third = SystemTime::UNIX_EPOCH + Duration::from_secs(500);
        let mut last_used = std::collections::HashMap::new();
        last_used.insert("Granite".to_owned(), Some(later));
        last_used.insert("Subspace".to_owned(), Some(earlier));
        last_used.insert("Personal".to_owned(), Some(third));

        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite"), make_account("Personal")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &last_used,
        );
        // Personal is the global LRU (oldest clock) but it's NOT in
        // the pinned set. The picker must pick Subspace (LRU of the
        // {Subspace, Granite} pair).
        let allowed = vec!["Subspace".to_owned(), "Granite".to_owned()];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3000);
        let (picked, _) = map.pick_for_project(Some(&allowed), now);
        assert_eq!(picked.0, "Subspace");
    }

    #[test]
    fn round_robin_restricted_pool_cycles_within_subset() {
        // Cycle counter is shared, but the pool restriction means
        // only subset entries are picked. Three calls with a
        // 2-account allow list should yield A, B, A.
        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite"), make_account("Personal")],
            SelectionPolicy::RoundRobin,
            None,
            &std::collections::HashMap::new(),
        );
        let allowed = vec!["Subspace".to_owned(), "Granite".to_owned()];
        let now = SystemTime::UNIX_EPOCH;
        let (a, _) = map.pick_for_project(Some(&allowed), now);
        let (b, _) = map.pick_for_project(Some(&allowed), now);
        let (c, _) = map.pick_for_project(Some(&allowed), now);
        assert_eq!(a.0, "Subspace");
        assert_eq!(b.0, "Granite");
        assert_eq!(c.0, "Subspace");
    }

    #[test]
    fn pick_for_project_none_falls_back_to_full_pool() {
        // Equivalent to the historical `pick_next` path. Three
        // accounts; LRU clock points at the second.
        let later = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        let mut last_used = std::collections::HashMap::new();
        last_used.insert("Subspace".to_owned(), Some(later));

        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &last_used,
        );
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3000);
        let (picked, _) = map.pick_for_project(None, now);
        // Granite has no clock → wins None-before-Some sort.
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn pick_for_project_unknown_names_silently_skip() {
        // Defence-in-depth: if the allowlist contains a bogus name
        // (shouldn't happen after config validation, but a future
        // refactor might drop the strict load), the picker filters
        // it out and uses the valid remainder.
        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &std::collections::HashMap::new(),
        );
        let allowed = vec!["NotAnAccount".to_owned(), "Subspace".to_owned()];
        let now = SystemTime::UNIX_EPOCH;
        let (picked, _) = map.pick_for_project(Some(&allowed), now);
        assert_eq!(picked.0, "Subspace");
    }

    #[test]
    fn pick_for_project_all_unknown_falls_back_to_full_pool() {
        // The runtime guard: if every name in the allowlist resolves
        // to nothing, fall back to the unrestricted pool instead of
        // panicking on an empty candidate vec.
        let mut map = AccountStateMap::new(
            &[make_account("Subspace"), make_account("Granite")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &std::collections::HashMap::new(),
        );
        let allowed = vec!["NotAnAccount".to_owned(), "AlsoBogus".to_owned()];
        let now = SystemTime::UNIX_EPOCH;
        let (picked, _) = map.pick_for_project(Some(&allowed), now);
        // Both names bogus → falls back to full pool → Granite wins
        // alpha tie-break (both never-used).
        assert_eq!(picked.0, "Granite");
    }

    #[test]
    fn pick_returns_correct_config_dir() {
        let mut map = AccountStateMap::new(
            &[make_account("Subspace")],
            SelectionPolicy::LeastRecentlyUsed,
            None,
            &std::collections::HashMap::new(),
        );
        let (_, dir) = map.pick_for_project(None, SystemTime::UNIX_EPOCH);
        assert_eq!(dir, PathBuf::from("/fake/Subspace"));
    }
}
