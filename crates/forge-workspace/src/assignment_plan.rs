//! Deterministic per-session account assignment.
//!
//! The plan answers "which account does THIS session of THIS project
//! spawn under?" with a deterministic lookup, computed once when
//! every account has reached a terminal `LoadingState`. Replaces the
//! global round-robin cursor from PR #240 for normal-spawn paths;
//! `pick_for_project` stays as the fallback when the plan is empty
//! (boot-not-yet-loaded path).
//!
//! Algorithm (per spec §3 of #246):
//! - Pool: six tiers in priority order; the first non-empty tier
//!   supplies the pool. (1) primaries Ready, not saturated; (2)
//!   fallbacks Ready, not saturated; (3) primaries Ready, saturated;
//!   (4) fallbacks Ready, saturated; (5) degraded accounts
//!   (terminal-but-not-Ready; spawning on one is legitimate because
//!   the 429 hit the usage probe, not inference), primaries before
//!   fallbacks; (6) dark. Primaries are the org's `accounts` list
//!   (missing or empty defaults to every ready account); fallbacks
//!   the org's `fallback_accounts` list (empty means none). Missing
//!   allow-listed names drop out of their tier.
//! - Offset: each project's position in the projects list, mod the
//!   pool size. Spreads the workload so different projects don't all
//!   hammer the first account.
//! - Assignment: account = pool[(offset + session_n) % pool.len()].
//!   Only the lead is known at boot, at session_n=0.
//! - Workers: each takes the next session_n when it spawns, with the
//!   same offset arithmetic, so the rotation continues from the lead.
//!
//! Frozen overlay: when a Bailed account recovers (Section 4.4),
//! recompute against the now-larger pool but PRESERVE existing
//! `(project, session)` assignments. Only new entries pick up the
//! recovered account; sessions already running keep their boot-time
//! account so their wire identity doesn't shift mid-run.

use std::collections::HashMap;

use crate::account::AccountKey;
use crate::target::ProjectKey;

/// Session-within-project identifier. `"lead"` for the project's
/// primary session; a worker's own label for everything else.
pub type SessionLabel = String;

/// Per-project metadata cached on the plan so `assign_adhoc_worker`
/// can extend the assignment without re-running the full algorithm.
/// The `pool` snapshot captures which accounts were `Ready` at
/// compute time, in the order the algorithm consumed them; the
/// `offset` is what slot the project's lead landed in. Adhoc workers
/// extend session_n past `next_session_n` and wrap with the same
/// modular arithmetic.
#[derive(Debug, Clone)]
struct ProjectSlot {
    pool: Vec<AccountKey>,
    offset: usize,
    next_session_n: usize,
    /// True when `pool` came from the degraded fallback - every
    /// allow-listed account was terminal-but-not-Ready at compute
    /// time. Surfaced so callers can mark the assignment degraded.
    degraded: bool,
    /// True when `pool` came from a fallback tier (2 or 4) - every
    /// primary was saturated or down, so the rotation runs on the
    /// org's `fallback_accounts`. Surfaced so callers can warn.
    fallback: bool,
}

/// Deterministic assignment of `(project, session_label) -> account`.
/// Storage is a HashMap; lookups are O(1). The `slots` field is the
/// per-project bookkeeping for `assign_adhoc_worker`.
#[derive(Debug, Clone, Default)]
pub struct AssignmentPlan {
    assignments: HashMap<(ProjectKey, SessionLabel), AccountKey>,
    slots: HashMap<ProjectKey, ProjectSlot>,
}

/// Compact project shape the algorithm consumes. Mirrors
/// `forge_workspace::config::LoadedProject` (the boot-time config
/// load result) without dragging the full struct into the plan
/// module's surface area.
#[derive(Debug)]
pub struct ProjectInput {
    pub key: ProjectKey,
    /// Allow-listed account names from `[[orgs]].accounts` (inherited).
    /// Empty/missing -> defaults to all ready accounts at compute
    /// time.
    pub accounts: Vec<String>,
    /// Fallback account names from `[[orgs]].fallback_accounts`
    /// (inherited). Empty -> no fallback tiers.
    pub fallback_accounts: Vec<String>,
}

impl AssignmentPlan {
    /// Look up the account assigned to `(project, label)`. Returns
    /// `None` when the project has no entry (the project's pool was
    /// empty at compute time) or the label wasn't assigned (a new
    /// adhoc-worker label that needs `assign_adhoc_worker` first).
    pub fn lookup(&self, project: &ProjectKey, label: &SessionLabel) -> Option<&AccountKey> {
        self.assignments.get(&(project.clone(), label.clone()))
    }

    /// Merge `fresh` into this plan in frozen-overlay mode: keep
    /// every existing `(project, label) -> account` assignment,
    /// add only entries from `fresh` that aren't already present,
    /// and refresh the per-project bookkeeping (`slots`) so future
    /// `assign_adhoc_worker` calls use the recovered pool.
    ///
    /// Called by `Workspace::recompute_plan_if_ready` after the
    /// boot-time plan has been populated (subsequent re-computes
    /// from runtime state transitions). Without the frozen overlay
    /// a recovered Bailed account would shift existing sessions to
    /// different accounts mid-run, breaking the wire-identity
    /// invariant.
    pub fn merge_frozen(&mut self, fresh: AssignmentPlan) {
        for (key, account) in fresh.assignments {
            self.assignments.entry(key).or_insert(account);
        }
        // Refresh slots so adhoc workers see the new pool sizes /
        // offsets. The `next_session_n` field carries the count of
        // assignments at compute time; preserve the higher count
        // (if existing plan has issued more adhoc workers than the
        // fresh plan, those extra entries are still in
        // `assignments` and the counter must not regress).
        for (project_key, mut fresh_slot) in fresh.slots {
            match self.slots.get(&project_key) {
                Some(existing) => {
                    if existing.next_session_n > fresh_slot.next_session_n {
                        fresh_slot.next_session_n = existing.next_session_n;
                    }
                    self.slots.insert(project_key, fresh_slot);
                }
                None => {
                    self.slots.insert(project_key, fresh_slot);
                }
            }
        }
    }

    /// Re-home a resume onto the re-tiered pool - the resume
    /// re-tier's frozen-overlay extend: the project's slot swaps to
    /// the re-tiered pool (the adhoc counter never regresses) and the
    /// label takes a fresh slot in it, `offset` for the lead and the
    /// next counter slot (the `assign_adhoc_worker` arithmetic) for a
    /// worker, so two separately-resumed workers do not collapse onto
    /// one account. Other rows unmoved. Returns the assigned account.
    pub(crate) fn retier_assignment(
        &mut self,
        project: &ProjectKey,
        label: &str,
        pool: Vec<AccountKey>,
        offset: usize,
        degraded: bool,
        fallback: bool,
    ) -> AccountKey {
        let slot = self.slots.entry(project.clone()).or_insert(ProjectSlot {
            pool: Vec::new(),
            offset,
            next_session_n: 0,
            degraded,
            fallback,
        });
        slot.pool = pool;
        slot.offset = offset;
        slot.degraded = degraded;
        slot.fallback = fallback;
        let account = if label == "lead" {
            let idx = slot.offset % slot.pool.len();
            slot.pool[idx].clone()
        } else {
            let session_n = slot.next_session_n;
            slot.next_session_n += 1;
            let idx = (slot.offset + session_n) % slot.pool.len();
            slot.pool[idx].clone()
        };
        self.assignments.insert((project.clone(), label.to_owned()), account.clone());
        account
    }

    /// `true` when the plan has zero entries for `project`. Surfaced
    /// to the launchpad so projects whose pool resolved to empty
    /// (the allow-list intersects neither the ready nor the degraded
    /// set) render a `no usable accounts` hint and stay unclickable
    /// even though `all_loaded` returned true.
    pub fn project_has_no_assignments(&self, project: &ProjectKey) -> bool {
        !self.assignments.keys().any(|(p, _)| p == project)
    }

    /// `true` when `project`'s pool came from the degraded fallback -
    /// every allow-listed account was terminal-but-not-Ready at
    /// compute time and the pool was built from those accounts rather
    /// than going dark. `false` for unknown projects and healthy
    /// pools.
    pub fn slot_degraded(&self, project: &ProjectKey) -> bool {
        self.slots.get(project).is_some_and(|slot| slot.degraded)
    }

    /// `true` when `project`'s pool came from a fallback tier (2 or
    /// 4) - no primary account was Ready and under its cap, so the
    /// rotation runs on the org's `fallback_accounts`. `false` for
    /// unknown projects, degraded pools, and primary-tier pools.
    pub fn slot_fallback(&self, project: &ProjectKey) -> bool {
        self.slots.get(project).is_some_and(|slot| slot.fallback)
    }

    /// Assign an account to a worker spawned mid-session via
    /// `workers__spawn` or similar adhoc path. Extends the
    /// project's assignment using the same modular-arithmetic shape
    /// as `compute_plan` so the rotation stays consistent across
    /// boot-time and runtime spawns. Returns the assigned account
    /// (owned clone - the borrow checker can't reconcile re-fetching
    /// from `&mut self`'s post-insert state), or `None` when the
    /// project is unknown or its pool is empty.
    ///
    /// `is_usable` re-checks live account state at spawn time: unlike
    /// the boot-time pool (frozen when accounts first went `Ready`), a
    /// mid-session account may have since hit its usage cap. When the
    /// round-robin slot lands on an unusable account the assignment
    /// walks forward to the next usable one; if the whole pool is
    /// unusable it falls back to the round-robin pick so a spawn never
    /// silently refuses (the user sees the subprocess's own 429),
    /// matching `pick_for_project`'s fallback.
    pub fn assign_adhoc_worker(
        &mut self,
        project: &ProjectKey,
        label: &SessionLabel,
        is_usable: impl Fn(&AccountKey) -> bool,
    ) -> Option<AccountKey> {
        // Check existing assignment first - adhoc workers may be
        // re-spawned under the same label; preserve the original
        // assignment to keep wire identity stable. The re-check applies
        // only to a fresh assignment, never re-homing a running worker.
        if let Some(account) = self.assignments.get(&(project.clone(), label.clone())) {
            return Some(account.clone());
        }

        let slot = self.slots.get_mut(project)?;
        if slot.pool.is_empty() {
            return None;
        }
        let session_n = slot.next_session_n;
        slot.next_session_n += 1;
        let len = slot.pool.len();
        let base = (slot.offset + session_n) % len;
        let pool_idx = (0..len)
            .map(|step| (base + step) % len)
            .find(|&idx| is_usable(&slot.pool[idx]))
            .unwrap_or(base);
        let account = slot.pool[pool_idx].clone();
        self.assignments.insert((project.clone(), label.clone()), account.clone());
        Some(account)
    }
}

/// The six-tier pool for one project, first non-empty tier winning:
/// (1) primaries Ready, not saturated; (2) fallbacks Ready, not
/// saturated; (3) primaries Ready, saturated; (4) fallbacks Ready,
/// saturated; (5) degraded accounts, primaries before fallbacks;
/// (6) dark - an empty pool. The bools report the degraded tier and
/// whether a fallback tier (2 or 4) won. Consumed by `compute_plan`
/// (all projects at once) and by the resume path's single-project
/// re-tier.
pub(crate) fn tier_pool(
    accounts: &[String],
    fallback_accounts: &[String],
    ready: &[AccountKey],
    degraded: &[AccountKey],
    saturated: &[AccountKey],
) -> (Vec<AccountKey>, bool, bool) {
    // Intersect a name allow-list with a source account set. The
    // primary list defaults to the whole source when empty (the
    // common solo-account shape); an empty fallback list means no
    // fallbacks, never "every account".
    let intersect = |names: &[String], sources: &[AccountKey]| {
        names
            .iter()
            .filter_map(|name| sources.iter().find(|k| k.0 == *name).cloned())
            .collect::<Vec<_>>()
    };
    let primary_pool = |sources: &[AccountKey]| {
        if accounts.is_empty() { sources.to_vec() } else { intersect(accounts, sources) }
    };
    let fallback_pool = |sources: &[AccountKey]| intersect(fallback_accounts, sources);

    // Tier 3 only fires once every ready primary is saturated, so
    // taking the ready primaries whole is the saturated tier.
    let primaries = primary_pool(ready);
    let fallbacks = fallback_pool(ready);
    let primaries_usable: Vec<AccountKey> =
        primaries.iter().filter(|k| !saturated.iter().any(|s| s == *k)).cloned().collect();
    let fallbacks_usable: Vec<AccountKey> =
        fallbacks.iter().filter(|k| !saturated.iter().any(|s| s == *k)).cloned().collect();
    if !primaries_usable.is_empty() {
        (primaries_usable, false, false)
    } else if !fallbacks_usable.is_empty() {
        (fallbacks_usable, false, true)
    } else if !primaries.is_empty() {
        (primaries, false, false)
    } else if !fallbacks.is_empty() {
        (fallbacks, false, true)
    } else {
        // A dual-listed account is primary-tier: primary membership
        // wins, so the fallback half never duplicates it into the
        // rotation.
        let mut pool = primary_pool(degraded);
        for account in fallback_pool(degraded) {
            if !pool.contains(&account) {
                pool.push(account);
            }
        }
        let degraded = !pool.is_empty();
        (pool, degraded, false)
    }
}

/// Compute the boot-time assignment plan from the ready and degraded
/// account sets + the project list. Pure function: same inputs always
/// produce the same output. Section 4.4 of #246 uses a frozen-overlay
/// variant that merges this output with an existing plan.
pub fn compute_plan(
    ready_accounts: &[AccountKey],
    degraded_accounts: &[AccountKey],
    saturated: &[AccountKey],
    projects: &[ProjectInput],
) -> AssignmentPlan {
    let mut plan = AssignmentPlan::default();

    for (project_idx, project) in projects.iter().enumerate() {
        let (pool, degraded, fallback) = tier_pool(
            &project.accounts,
            &project.fallback_accounts,
            ready_accounts,
            degraded_accounts,
            saturated,
        );

        if pool.is_empty() {
            // Project has no usable account. Record an empty slot so
            // `project_has_no_assignments` can distinguish this from
            // an unconfigured project; assign_adhoc_worker will
            // return None.
            plan.slots.insert(
                project.key.clone(),
                ProjectSlot {
                    pool: Vec::new(),
                    offset: 0,
                    next_session_n: 0,
                    degraded: false,
                    fallback: false,
                },
            );
            continue;
        }

        let offset = project_idx % pool.len();

        // Only the lead is known at boot; every worker takes its slot
        // from `assign_adhoc_worker` when it spawns, so session_n starts
        // at 1 with the lead holding 0.
        plan.assignments.insert((project.key.clone(), "lead".to_owned()), pool[offset].clone());

        plan.slots.insert(
            project.key.clone(),
            ProjectSlot { pool, offset, next_session_n: 1, degraded, fallback },
        );
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ak(name: &str) -> AccountKey {
        AccountKey(name.to_owned())
    }

    fn pk(name: &str) -> ProjectKey {
        ProjectKey::new(name)
    }

    fn project(key: &str, accounts: &[&str]) -> ProjectInput {
        project_with_fallbacks(key, accounts, &[])
    }

    fn project_with_fallbacks(key: &str, accounts: &[&str], fallbacks: &[&str]) -> ProjectInput {
        ProjectInput {
            key: pk(key),
            accounts: accounts.iter().map(|s| (*s).to_owned()).collect(),
            fallback_accounts: fallbacks.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn compute_plan_matches_worked_example() {
        // Spec §3 worked example: 4 ready accounts; 2 projects.
        // forge (idx 0, workers planner/implementer/reviewer/debugger/tester):
        //   pool size 4, offset 0
        //   lead -> pool[0] = gateway
        //   planner -> pool[1] = gateway1
        //   implementer -> pool[2] = personal
        //   reviewer -> pool[3] = stargate
        //   debugger -> pool[4 % 4 = 0] = gateway (wraps)
        //   tester -> pool[5 % 4 = 1] = gateway1
        // data-modules (idx 1, workers babysitter/librarian):
        //   pool size 4, offset 1
        //   lead -> pool[(1 + 0) % 4 = 1] = gateway1
        //   babysitter -> pool[(1 + 1) % 4 = 2] = personal
        //   librarian -> pool[(1 + 2) % 4 = 3] = stargate
        let accounts = vec![ak("gateway"), ak("gateway1"), ak("personal"), ak("stargate")];
        let names: Vec<&str> = vec!["gateway", "gateway1", "personal", "stargate"];
        let projects = vec![project("forge", &names), project("data-modules", &names)];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);
        // Boot assigns the lead; every worker takes its slot when it
        // spawns, continuing the same rotation.
        for label in ["planner", "implementer", "reviewer", "debugger", "tester"] {
            let _ = plan.assign_adhoc_worker(&pk("forge"), &label.into(), |_| true);
        }
        for label in ["babysitter", "librarian"] {
            let _ = plan.assign_adhoc_worker(&pk("data-modules"), &label.into(), |_| true);
        }

        assert_eq!(plan.lookup(&pk("forge"), &"lead".into()), Some(&ak("gateway")));
        assert_eq!(plan.lookup(&pk("forge"), &"planner".into()), Some(&ak("gateway1")));
        assert_eq!(plan.lookup(&pk("forge"), &"implementer".into()), Some(&ak("personal")));
        assert_eq!(plan.lookup(&pk("forge"), &"reviewer".into()), Some(&ak("stargate")));
        assert_eq!(plan.lookup(&pk("forge"), &"debugger".into()), Some(&ak("gateway")));
        assert_eq!(plan.lookup(&pk("forge"), &"tester".into()), Some(&ak("gateway1")));

        assert_eq!(plan.lookup(&pk("data-modules"), &"lead".into()), Some(&ak("gateway1")));
        assert_eq!(plan.lookup(&pk("data-modules"), &"babysitter".into()), Some(&ak("personal")));
        assert_eq!(plan.lookup(&pk("data-modules"), &"librarian".into()), Some(&ak("stargate")));
    }

    #[test]
    fn compute_plan_drops_typo_accounts() {
        // Project allow-list contains a name that doesn't appear in
        // the ready set; the algorithm silently filters it out
        // rather than panicking or assigning a nonexistent account.
        let accounts = vec![ak("gateway"), ak("personal")];
        let projects = vec![project("forge", &["gateway", "typo-account", "personal"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);
        let _ = plan.assign_adhoc_worker(&pk("forge"), &"worker1".into(), |_| true);

        // Pool reduces to [gateway, personal]; offset 0; size 2.
        assert_eq!(plan.lookup(&pk("forge"), &"lead".into()), Some(&ak("gateway")));
        assert_eq!(plan.lookup(&pk("forge"), &"worker1".into()), Some(&ak("personal")));
    }

    #[test]
    fn compute_plan_empty_pool_no_assignments() {
        // Project's allow-list contains only accounts that aren't
        // ready (e.g., all Bailed). The project records a slot but
        // produces zero assignments; `project_has_no_assignments`
        // reports true.
        let accounts = vec![ak("gateway")];
        let projects = vec![project("forge", &["bailed-account"])];
        let plan = compute_plan(&accounts, &[], &[], &projects);

        assert!(plan.project_has_no_assignments(&pk("forge")));
        assert_eq!(plan.lookup(&pk("forge"), &"lead".into()), None);
    }

    #[test]
    fn compute_plan_missing_accounts_defaults_to_all_ready() {
        // Project with empty allow-list -> defaults to every ready
        // account. Common case for solo-account setups.
        let accounts = vec![ak("gateway"), ak("personal")];
        let projects = vec![project("forge", &[])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);
        let _ = plan.assign_adhoc_worker(&pk("forge"), &"w1".into(), |_| true);

        assert_eq!(plan.lookup(&pk("forge"), &"lead".into()), Some(&ak("gateway")));
        assert_eq!(plan.lookup(&pk("forge"), &"w1".into()), Some(&ak("personal")));
    }

    #[test]
    fn compute_plan_single_account_wraps_all_sessions_to_it() {
        // One account: every session lands on it. This pins that a
        // len-1 pool never yields an out-of-range index, not the
        // rotation - no arithmetic variant is observable at len 1.
        let accounts = vec![ak("only")];
        let projects = vec![project("forge", &["only"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);
        for label in ["a", "b", "c", "d"] {
            let _ = plan.assign_adhoc_worker(&pk("forge"), &label.into(), |_| true);
        }

        for label in ["lead", "a", "b", "c", "d"] {
            assert_eq!(plan.lookup(&pk("forge"), &label.into()), Some(&ak("only")));
        }
    }

    #[test]
    fn compute_plan_prefers_non_saturated_accounts() {
        // Org allows [gateway, gateway1, personal]; gateway + gateway1
        // are at the usage cap. Every session must land on personal -
        // the saturated accounts drop out of the pool.
        let accounts = vec![ak("gateway"), ak("gateway1"), ak("personal")];
        let saturated = vec![ak("gateway"), ak("gateway1")];
        let projects = vec![project("forge", &["gateway", "gateway1", "personal"])];
        let mut plan = compute_plan(&accounts, &[], &saturated, &projects);
        // Workers reach the plan by spawning now, so assign them the way
        // a spawn does before asserting where they landed.
        for label in ["planner", "implementer"] {
            let _ = plan.assign_adhoc_worker(&pk("forge"), &label.into(), |_| true);
        }

        for label in ["lead", "planner", "implementer"] {
            assert_eq!(
                plan.lookup(&pk("forge"), &label.into()),
                Some(&ak("personal")),
                "session {label} must avoid the saturated accounts",
            );
        }
    }

    #[test]
    fn compute_plan_falls_back_when_all_candidates_saturated() {
        // Org allows only [gateway, gateway1] and both are capped - no
        // alternative. The pool must still include them so the project
        // gets assigned rather than going dark.
        let accounts = vec![ak("gateway"), ak("gateway1")];
        let saturated = vec![ak("gateway"), ak("gateway1")];
        let projects = vec![project("gateway-backend", &["gateway", "gateway1"])];
        let mut plan = compute_plan(&accounts, &[], &saturated, &projects);
        let _ = plan.assign_adhoc_worker(&pk("gateway-backend"), &"worker1".into(), |_| true);

        assert!(
            !plan.project_has_no_assignments(&pk("gateway-backend")),
            "all-saturated org must still get assignments, not go dark",
        );
        // offset 0, pool [gateway, gateway1]: lead -> gateway, worker1 -> gateway1.
        assert_eq!(plan.lookup(&pk("gateway-backend"), &"lead".into()), Some(&ak("gateway")));
        assert_eq!(plan.lookup(&pk("gateway-backend"), &"worker1".into()), Some(&ak("gateway1")));
    }

    #[test]
    fn assign_adhoc_worker_extends_with_consistent_arithmetic() {
        // Boot assigns only the lead (session_n=0), so the first spawn
        // is session_n=1, slot = (offset + 1) % pool_size.
        let accounts = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project("p", &["a", "b", "c"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);

        // Pool = [a, b, c], offset = 0, next_session_n = 1.
        // Adhoc session_n=1, slot=(0+1)%3=1 -> b.
        let assigned = plan.assign_adhoc_worker(&pk("p"), &"adhoc".into(), |_| true);
        assert_eq!(assigned, Some(ak("b")));
        assert_eq!(plan.lookup(&pk("p"), &"adhoc".into()), Some(&ak("b")));
    }

    #[test]
    fn assign_adhoc_worker_wraps_around_pool() {
        // 3-account pool, lead only at boot (next_session_n=1).
        // 4 adhoc spawns -> session_n=1,2,3,4 -> slots=1,2,0,1 ->
        // accounts b, c, a, b.
        let accounts = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project("p", &["a", "b", "c"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);

        let picks: Vec<AccountKey> = (0..4)
            .map(|n| {
                plan.assign_adhoc_worker(&pk("p"), &format!("adhoc-{n}"), |_| true)
                    .expect("pool non-empty")
            })
            .collect();
        assert_eq!(picks, vec![ak("b"), ak("c"), ak("a"), ak("b")]);
    }

    #[test]
    fn assign_adhoc_worker_rotates_past_rate_limited_account() {
        // Pool [a, b, c], offset 0. Boot assigned only the lead
        // (session_n=0 -> a), so the first adhoc is session_n=1 and the
        // raw round-robin slot is (0 + 1) % 3 = 1 -> b. With b
        // rate-limited, the assignment must walk forward to the next
        // usable account (c) instead of silently landing on b.
        let accounts = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project("p", &["a", "b", "c"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);

        let assigned = plan.assign_adhoc_worker(&pk("p"), &"adhoc".into(), |k| k != &ak("b"));
        assert_eq!(
            assigned,
            Some(ak("c")),
            "adhoc worker must rotate off the rate-limited slot to the next usable account",
        );
        assert_eq!(plan.lookup(&pk("p"), &"adhoc".into()), Some(&ak("c")));
    }

    #[test]
    fn assign_adhoc_worker_falls_back_when_all_accounts_unusable() {
        // Same pool; the raw slot lands on b. When EVERY candidate is
        // unusable the assignment must still return an account (the raw
        // round-robin pick) rather than None, so the spawn proceeds and
        // the user sees the subprocess's own 429 instead of forge
        // silently refusing - matching pick_for_project's fallback.
        let accounts = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project("p", &["a", "b", "c"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);

        let assigned = plan.assign_adhoc_worker(&pk("p"), &"adhoc".into(), |_| false);
        assert_eq!(
            assigned,
            Some(ak("b")),
            "all-unusable pool must still assign (the raw round-robin pick), never None",
        );
    }

    #[test]
    fn assign_adhoc_worker_returns_existing_assignment_idempotently() {
        // Re-spawning a worker under the same label preserves its
        // original assignment - wire identity doesn't shift.
        let accounts = vec![ak("a"), ak("b")];
        let projects = vec![project("p", &["a", "b"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);

        let first = plan.assign_adhoc_worker(&pk("p"), &"reviewer".into(), |_| true);
        let second = plan.assign_adhoc_worker(&pk("p"), &"reviewer".into(), |_| true);
        assert_eq!(first, second);
    }

    #[test]
    fn assign_adhoc_worker_returns_none_for_empty_pool() {
        let accounts = vec![ak("a")];
        let projects = vec![project("p", &["bailed"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);
        assert_eq!(plan.assign_adhoc_worker(&pk("p"), &"any".into(), |_| true), None);
    }

    #[test]
    fn assign_adhoc_worker_returns_none_for_unknown_project() {
        let plan_input_projects: Vec<ProjectInput> = Vec::new();
        let mut plan = compute_plan(&[ak("a")], &[], &[], &plan_input_projects);
        assert_eq!(plan.assign_adhoc_worker(&pk("absent"), &"x".into(), |_| true), None);
    }

    #[test]
    fn project_has_no_assignments_false_when_assignments_exist() {
        let plan = compute_plan(&[ak("a")], &[], &[], &[project("p", &["a"])]);
        assert!(!plan.project_has_no_assignments(&pk("p")));
    }

    #[test]
    fn project_has_no_assignments_true_when_project_unknown() {
        // No project entry at all -> trivially no assignments.
        let plan = AssignmentPlan::default();
        assert!(plan.project_has_no_assignments(&pk("nope")));
    }

    #[test]
    fn merge_frozen_preserves_existing_assignments() {
        // Boot-time: only "b" is ready, so every session lands there.
        // The pools must DISAGREE about where the lead goes - boot puts
        // it on b, recovery on pool[0] = a - or the overlay has no
        // conflict to refuse and the test cannot tell or_insert from
        // insert.
        let boot_accounts = vec![ak("b")];
        let projects = vec![project("p", &["a", "b"])];
        let mut plan = compute_plan(&boot_accounts, &[], &[], &projects);
        let _ = plan.assign_adhoc_worker(&pk("p"), &"w1".into(), |_| true);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("b")));
        assert_eq!(plan.lookup(&pk("p"), &"w1".into()), Some(&ak("b")));

        // Recovery: account "a" comes back online. Fresh plan would put
        // the lead on a, but the frozen overlay must PRESERVE the
        // existing assignments.
        let recovered_accounts = vec![ak("a"), ak("b")];
        let fresh = compute_plan(&recovered_accounts, &[], &[], &projects);
        plan.merge_frozen(fresh);

        assert_eq!(
            plan.lookup(&pk("p"), &"lead".into()),
            Some(&ak("b")),
            "lead must keep its boot-time account",
        );
        assert_eq!(
            plan.lookup(&pk("p"), &"w1".into()),
            Some(&ak("b")),
            "w1 must keep its boot-time account",
        );
    }

    #[test]
    fn merge_frozen_extends_with_new_adhoc_targets() {
        // After merge, the per-project slot's pool covers the
        // recovered accounts so future adhoc workers can land on
        // them. Existing sessions stay put.
        let boot_accounts = vec![ak("a")];
        let projects = vec![project("p", &["a", "b"])];
        let mut plan = compute_plan(&boot_accounts, &[], &[], &projects);

        let recovered_accounts = vec![ak("a"), ak("b")];
        let fresh = compute_plan(&recovered_accounts, &[], &[], &projects);
        plan.merge_frozen(fresh);

        // The next adhoc worker (session_n = 1 - boot only assigned
        // lead at session_n = 0) lands on pool[1] = b.
        let assigned = plan.assign_adhoc_worker(&pk("p"), &"w1".into(), |_| true);
        assert_eq!(
            assigned,
            Some(ak("b")),
            "adhoc worker after recovery lands on the recovered account",
        );
    }

    #[test]
    fn merge_frozen_preserves_adhoc_counter_progress() {
        // Boot + 2 adhoc workers issued. Recovery shouldn't roll the
        // counter back so a third adhoc lands at the right slot.
        let accounts = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project("p", &["a", "b", "c"])];
        let mut plan = compute_plan(&accounts, &[], &[], &projects);
        let _ = plan.assign_adhoc_worker(&pk("p"), &"w1".into(), |_| true); // slot 1 -> b
        let _ = plan.assign_adhoc_worker(&pk("p"), &"w2".into(), |_| true); // slot 2 -> c

        // Re-compute against the same ready set (e.g., a Bailed
        // account elsewhere recovered without affecting this
        // project's pool). The frozen overlay must keep counter at 3.
        let fresh = compute_plan(&accounts, &[], &[], &projects);
        plan.merge_frozen(fresh);
        let assigned = plan.assign_adhoc_worker(&pk("p"), &"w3".into(), |_| true); // slot 3 mod 3 = 0 -> a
        assert_eq!(assigned, Some(ak("a")));
    }

    #[test]
    fn compute_plan_falls_back_to_degraded_accounts_when_no_ready_pool() {
        // Project allow-lists [a]; `a` is degraded (rate-limited),
        // nothing Ready. The project is assigned the degraded account
        // rather than going dark, and the slot is marked degraded.
        let degraded = vec![ak("a")];
        let projects = vec![project("p", &["a"])];
        let plan = compute_plan(&[], &degraded, &[], &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("a")));
        assert!(plan.slot_degraded(&pk("p")), "the fallback pool is marked degraded");
    }

    #[test]
    fn compute_plan_prefers_ready_over_degraded() {
        // Both `a` (degraded) and `b` (ready) are allow-listed; only
        // `b` is assigned - the degraded pool never dilutes a healthy
        // one.
        let ready = vec![ak("b")];
        let degraded = vec![ak("a")];
        let projects = vec![project("p", &["a", "b"])];
        let plan = compute_plan(&ready, &degraded, &[], &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("b")));
        assert!(!plan.slot_degraded(&pk("p")), "a healthy pool is not degraded");
    }

    #[test]
    fn compute_plan_dark_only_when_both_pools_empty() {
        // Allow-listed account is neither ready nor degraded -> still
        // an empty slot.
        let projects = vec![project("p", &["ghost"])];
        let plan = compute_plan(&[], &[], &[], &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), None);
        assert!(!plan.slot_degraded(&pk("p")));
    }

    #[test]
    fn compute_plan_prefers_a_saturated_ready_account_over_a_degraded_one() {
        // `a` is Ready but at the cap, `b` is degraded. The saturated
        // fallback fires before the degraded tier: a saturated account
        // still logs in, a degraded one already failed its probe.
        let ready = vec![ak("a")];
        let degraded = vec![ak("b")];
        let saturated = vec![ak("a")];
        let projects = vec![project("p", &["a", "b"])];
        let plan = compute_plan(&ready, &degraded, &saturated, &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("a")));
        assert!(!plan.slot_degraded(&pk("p")), "a saturated-Ready pool is not the degraded tier");
    }

    #[test]
    fn compute_plan_degraded_pool_dedups_a_dual_listed_account() {
        // `b` is both primary and fallback, all three degraded: the
        // degraded pool holds it once, primary membership winning. A
        // duplicate would rotate it twice - w2 lands on `c` with the
        // dedup, on `b` again without it.
        let degraded = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project_with_fallbacks("p", &["a", "b"], &["b", "c"])];
        let mut plan = compute_plan(&[], &degraded, &[], &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("a")));
        let _ = plan.assign_adhoc_worker(&pk("p"), &"w1".into(), |_| true);
        let w2 = plan.assign_adhoc_worker(&pk("p"), &"w2".into(), |_| true);
        assert_eq!(w2, Some(ak("c")), "the dual-listed account is not rotated twice");
    }

    #[test]
    fn compute_plan_degraded_pool_respects_the_allow_list() {
        // Both accounts degraded; the project pins only `a`. The
        // degraded pool is the allow-list intersection, not every
        // degraded account - `b` must never be assigned to this
        // project.
        let degraded = vec![ak("a"), ak("b")];
        let projects = vec![project("p", &["a"])];
        let mut plan = compute_plan(&[], &degraded, &[], &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("a")));
        let worker = plan.assign_adhoc_worker(&pk("p"), &"w1".into(), |_| true);
        assert_eq!(worker, Some(ak("a")), "the pool holds only the allow-listed degraded account");
    }

    #[test]
    fn saturated_primary_falls_to_ready_fallback() {
        // The primary is Ready but at its cap and the fallback is
        // Ready: tier 1 is empty, so the lead falls to tier 2 instead
        // of the saturated primary.
        let ready = vec![ak("sub"), ak("api")];
        let saturated = vec![ak("sub")];
        let projects = vec![project_with_fallbacks("p", &["sub"], &["api"])];
        let plan = compute_plan(&ready, &[], &saturated, &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("api")));
        assert!(plan.slot_fallback(&pk("p")), "tier 2 marks the slot fallback");
        assert!(!plan.slot_degraded(&pk("p")));
    }

    #[test]
    fn saturated_fallback_beats_going_dark() {
        // The primary bailed and the fallback is Ready but at its cap:
        // tier 4 supplies the pool so the project assigns rather than
        // going dark - and the slot is not marked degraded.
        let ready = vec![ak("api2")];
        let degraded = vec![ak("sub")];
        let saturated = vec![ak("api2")];
        let projects = vec![project_with_fallbacks("p", &["sub"], &["api2"])];
        let plan = compute_plan(&ready, &degraded, &saturated, &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("api2")));
        assert!(plan.slot_fallback(&pk("p")), "tier 4 marks the slot fallback");
        assert!(
            !plan.slot_degraded(&pk("p")),
            "a saturated-Ready fallback pool is not the degraded tier",
        );
    }

    #[test]
    fn degraded_is_the_last_tier() {
        // Every allow-listed account bailed: the degraded tier assigns
        // primaries before fallbacks and marks the slot degraded.
        let degraded = vec![ak("sub"), ak("api")];
        let projects = vec![project_with_fallbacks("p", &["sub"], &["api"])];
        let mut plan = compute_plan(&[], &degraded, &[], &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("sub")));
        assert!(plan.slot_degraded(&pk("p")));
        let worker = plan.assign_adhoc_worker(&pk("p"), &"w1".into(), |_| true);
        assert_eq!(
            worker,
            Some(ak("api")),
            "the fallback follows the primary in the degraded pool"
        );
    }

    #[test]
    fn no_fallback_config_is_byte_identical_to_today() {
        // The primary is saturated and no fallbacks are configured:
        // the existing never-go-dark saturated fallback (tier 3)
        // assigns the primary itself - the fallback tiers never
        // contribute.
        let ready = vec![ak("sub")];
        let saturated = vec![ak("sub")];
        let projects = vec![project("p", &["sub"])];
        let plan = compute_plan(&ready, &[], &saturated, &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("sub")));
        assert!(!plan.slot_degraded(&pk("p")));
    }

    #[test]
    fn ready_primary_beats_ready_fallback() {
        // Both Ready and unsaturated: tier 1 wins, the lead stays on
        // the primary.
        let ready = vec![ak("sub"), ak("api")];
        let projects = vec![project_with_fallbacks("p", &["sub"], &["api"])];
        let plan = compute_plan(&ready, &[], &[], &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("sub")));
        assert!(!plan.slot_fallback(&pk("p")), "tier 1 is not a fallback tier");
    }
}
