//! Account selection — internal to forge-workspace.
//!
//! `Workspace::get_agent_handle` consults `pick_next` on every
//! spawn; the chosen `AccountKey` becomes the spawned Agent's
//! `CLAUDE_CONFIG_DIR` override. Persistence is handled by
//! `state.rs`.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::config::{LoadedAccount, SelectionPolicy};

/// Internal newtype wrapping the account's `display_name`. Phase
/// 1b doesn't surface this publicly.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
#[allow(dead_code)] // wired up in Task 5
pub(crate) struct AccountKey(pub String);

#[derive(Debug, Clone)]
#[allow(dead_code)] // wired up in Task 5
pub(crate) struct AccountState {
    pub config_dir: PathBuf,
    pub last_used_at: Option<SystemTime>,
}

#[derive(Debug)]
#[allow(dead_code)] // wired up in Task 5
pub(crate) struct AccountStateMap {
    pub ordered_keys: Vec<AccountKey>,         // forge.toml definition order
    pub by_key: std::collections::HashMap<AccountKey, AccountState>,
    pub policy: SelectionPolicy,
    pub round_robin_next: usize,
}

#[allow(dead_code)] // wired up in Task 5
impl AccountStateMap {
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
            let last_used_at = persisted_last_used
                .get(&account.display_name)
                .and_then(|x| x.as_ref())
                .copied();
            by_key.insert(
                key,
                AccountState {
                    config_dir: account.config_dir.clone(),
                    last_used_at,
                },
            );
        }
        Self {
            ordered_keys,
            by_key,
            policy,
            round_robin_next: persisted_round_robin_next.unwrap_or(0),
        }
    }

    /// Picks the next account, updates its `last_used_at`, and
    /// (for round_robin) advances the cursor. Returns the picked
    /// key + its config_dir.
    ///
    /// The lookup back into `by_key` is structurally infallible
    /// because both pickers only return keys constructed in
    /// [`Self::new`]. The `if let Some(...)` is just a clippy-
    /// friendly form; the path is otherwise an effective no-op.
    pub fn pick_next(&mut self, now: SystemTime) -> (AccountKey, PathBuf) {
        let picked = match self.policy {
            SelectionPolicy::LeastRecentlyUsed => self.pick_lru(),
            SelectionPolicy::RoundRobin => self.pick_round_robin(),
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

    fn pick_lru(&self) -> AccountKey {
        // Sort: None first; then ascending Some; tie-break alphabetical.
        let mut candidates: Vec<&AccountKey> = self.ordered_keys.iter().collect();
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

    fn pick_round_robin(&mut self) -> AccountKey {
        let idx = self.round_robin_next % self.ordered_keys.len();
        let picked = self.ordered_keys[idx].clone();
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
        let (picked, _) = map.pick_next(now);
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
        let (picked, _) = map.pick_next(now);
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
        let (picked, _) = map.pick_next(now);
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
        let (a, _) = map.pick_next(now);
        let (b, _) = map.pick_next(now);
        let (c, _) = map.pick_next(now);
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
        let (picked, _) = map.pick_next(now);
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
        let (_, dir) = map.pick_next(SystemTime::UNIX_EPOCH);
        assert_eq!(dir, PathBuf::from("/fake/Subspace"));
    }
}
