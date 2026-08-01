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
//! - Pool: a project's `accounts` list filtered against the set of
//!   accounts currently in `LoadingState::Ready`, then narrowed to
//!   those not at the usage cap (falling back to the saturated set
//!   only when every candidate is capped, so a project never goes
//!   dark). Missing or empty `accounts` field defaults to "every
//!   ready account."
//! - Offset: each project's position in the projects list, mod the
//!   pool size. Spreads the workload so different projects don't all
//!   hammer the first account.
//! - Assignment: for session_n in the project's [lead] + team list,
//!   account = pool[(offset + session_n) % pool.len()]. Lead is
//!   session_n=0; team workers are 1, 2, ... in order.
//! - Adhoc workers (workers__spawn): bump a per-project counter past
//!   the boot-time team length; assign with the same offset arithmetic.
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
/// primary session; team-role labels (`"planner"`, `"reviewer"`,
/// etc.) for boot-spawned workers; ad-hoc labels from
/// `workers__spawn` calls for runtime workers.
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
    /// Static-worker labels from `static_workers = [...]`. Lead is
    /// implicit at session_n=0; static_workers[0] is session_n=1,
    /// static_workers[1] is session_n=2, etc.
    pub static_workers: Vec<String>,
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

    /// `true` when the plan has zero entries for `project`. Surfaced
    /// to the launchpad so projects whose pool resolved to empty
    /// (every allowed account Bailed, or the allow-list contains no
    /// known accounts) render a `no usable accounts` hint and stay
    /// unclickable even though `all_loaded` returned true.
    pub fn project_has_no_assignments(&self, project: &ProjectKey) -> bool {
        !self.assignments.keys().any(|(p, _)| p == project)
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

/// Compute the boot-time assignment plan from the set of ready
/// accounts + the project list. Pure function: same inputs always
/// produce the same output. Section 4.4 of #246 uses a frozen-overlay
/// variant that merges this output with an existing plan.
pub fn compute_plan(
    ready_accounts: &[AccountKey],
    saturated: &[AccountKey],
    projects: &[ProjectInput],
) -> AssignmentPlan {
    let mut plan = AssignmentPlan::default();

    for (project_idx, project) in projects.iter().enumerate() {
        // Resolve the per-project candidate pool: intersect the
        // project's `accounts` allow-list with the set of ready
        // accounts. Empty allow-list defaults to "every ready account."
        let candidates: Vec<AccountKey> = if project.accounts.is_empty() {
            ready_accounts.to_vec()
        } else {
            project
                .accounts
                .iter()
                .filter_map(|name| ready_accounts.iter().find(|k| k.0 == *name).cloned())
                .collect()
        };

        // Prefer accounts that aren't at the usage cap. Fall back to
        // the full candidate set only when every candidate is saturated,
        // so an all-exhausted org still gets assigned rather than going
        // dark.
        let usable: Vec<AccountKey> =
            candidates.iter().filter(|k| !saturated.iter().any(|s| s == *k)).cloned().collect();
        let pool = if usable.is_empty() { candidates } else { usable };

        if pool.is_empty() {
            // Project has no usable account. Record an empty slot so
            // `project_has_no_assignments` can distinguish this from
            // an unconfigured project; assign_adhoc_worker will
            // return None.
            plan.slots.insert(
                project.key.clone(),
                ProjectSlot { pool: Vec::new(), offset: 0, next_session_n: 0 },
            );
            continue;
        }

        let offset = project_idx % pool.len();

        // Lead session is session_n=0; static workers follow in order
        // as session_n=1, 2, ....
        let mut sessions = Vec::with_capacity(1 + project.static_workers.len());
        sessions.push("lead".to_owned());
        for label in &project.static_workers {
            sessions.push(label.clone());
        }

        for (session_n, label) in sessions.iter().enumerate() {
            let pool_idx = (offset + session_n) % pool.len();
            let account = pool[pool_idx].clone();
            plan.assignments.insert((project.key.clone(), label.clone()), account);
        }

        plan.slots.insert(
            project.key.clone(),
            ProjectSlot { pool, offset, next_session_n: sessions.len() },
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

    fn project(key: &str, accounts: &[&str], static_workers: &[&str]) -> ProjectInput {
        ProjectInput {
            key: pk(key),
            accounts: accounts.iter().map(|s| (*s).to_owned()).collect(),
            static_workers: static_workers.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn compute_plan_matches_worked_example() {
        // Spec §3 worked example: 4 ready accounts; 2 projects.
        // forge (idx 0, team = planner/implementer/reviewer/debugger/tester):
        //   pool size 4, offset 0
        //   lead -> pool[0] = gateway
        //   planner -> pool[1] = gateway1
        //   implementer -> pool[2] = personal
        //   reviewer -> pool[3] = stargate
        //   debugger -> pool[4 % 4 = 0] = gateway (wraps)
        //   tester -> pool[5 % 4 = 1] = gateway1
        // data-modules (idx 1, team = babysitter/librarian):
        //   pool size 4, offset 1
        //   lead -> pool[(1 + 0) % 4 = 1] = gateway1
        //   babysitter -> pool[(1 + 1) % 4 = 2] = personal
        //   librarian -> pool[(1 + 2) % 4 = 3] = stargate
        let accounts = vec![ak("gateway"), ak("gateway1"), ak("personal"), ak("stargate")];
        let names: Vec<&str> = vec!["gateway", "gateway1", "personal", "stargate"];
        let projects = vec![
            project("forge", &names, &["planner", "implementer", "reviewer", "debugger", "tester"]),
            project("data-modules", &names, &["babysitter", "librarian"]),
        ];
        let plan = compute_plan(&accounts, &[], &projects);

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
        let projects =
            vec![project("forge", &["gateway", "typo-account", "personal"], &["worker1"])];
        let plan = compute_plan(&accounts, &[], &projects);

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
        let projects = vec![project("forge", &["bailed-account"], &["worker1"])];
        let plan = compute_plan(&accounts, &[], &projects);

        assert!(plan.project_has_no_assignments(&pk("forge")));
        assert_eq!(plan.lookup(&pk("forge"), &"lead".into()), None);
        assert_eq!(plan.lookup(&pk("forge"), &"worker1".into()), None);
    }

    #[test]
    fn compute_plan_missing_accounts_defaults_to_all_ready() {
        // Project with empty allow-list -> defaults to every ready
        // account. Common case for solo-account setups.
        let accounts = vec![ak("gateway"), ak("personal")];
        let projects = vec![project("forge", &[], &["w1"])];
        let plan = compute_plan(&accounts, &[], &projects);

        assert_eq!(plan.lookup(&pk("forge"), &"lead".into()), Some(&ak("gateway")));
        assert_eq!(plan.lookup(&pk("forge"), &"w1".into()), Some(&ak("personal")));
    }

    #[test]
    fn compute_plan_single_account_wraps_all_sessions_to_it() {
        // One account + a 5-session team -> every session lands on
        // the lone account. `cursor % 1 == 0` collapses to a single
        // assignment per session.
        let accounts = vec![ak("only")];
        let projects = vec![project("forge", &["only"], &["a", "b", "c", "d"])];
        let plan = compute_plan(&accounts, &[], &projects);

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
        let projects = vec![project(
            "forge",
            &["gateway", "gateway1", "personal"],
            &["planner", "implementer"],
        )];
        let plan = compute_plan(&accounts, &saturated, &projects);

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
        let projects = vec![project("gateway-backend", &["gateway", "gateway1"], &["worker1"])];
        let plan = compute_plan(&accounts, &saturated, &projects);

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
        // After boot, the project has lead + 1 boot worker = 2
        // sessions assigned (session_n 0 and 1). An adhoc spawn is
        // session_n=2, slot = (offset + 2) % pool_size.
        let accounts = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project("p", &["a", "b", "c"], &["w1"])];
        let mut plan = compute_plan(&accounts, &[], &projects);

        // Pool = [a, b, c], offset = 0, next_session_n = 2.
        // Adhoc session_n=2, slot=(0+2)%3=2 -> c.
        let assigned = plan.assign_adhoc_worker(&pk("p"), &"adhoc".into(), |_| true);
        assert_eq!(assigned, Some(ak("c")));
        assert_eq!(plan.lookup(&pk("p"), &"adhoc".into()), Some(&ak("c")));
    }

    #[test]
    fn assign_adhoc_worker_wraps_around_pool() {
        // 3-account pool, 1 boot worker (next_session_n=2 post-boot).
        // 4 adhoc spawns -> session_n=2,3,4,5 -> slots=2,0,1,2 ->
        // accounts c, a, b, c.
        let accounts = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project("p", &["a", "b", "c"], &["w1"])];
        let mut plan = compute_plan(&accounts, &[], &projects);

        let picks: Vec<AccountKey> = (0..4)
            .map(|n| {
                plan.assign_adhoc_worker(&pk("p"), &format!("adhoc-{n}"), |_| true)
                    .expect("pool non-empty")
            })
            .collect();
        assert_eq!(picks, vec![ak("c"), ak("a"), ak("b"), ak("c")]);
    }

    #[test]
    fn assign_adhoc_worker_rotates_past_rate_limited_account() {
        // Pool [a, b, c], offset 0. Boot assigned only the lead
        // (session_n=0 -> a), so the first adhoc is session_n=1 and the
        // raw round-robin slot is (0 + 1) % 3 = 1 -> b. With b
        // rate-limited, the assignment must walk forward to the next
        // usable account (c) instead of silently landing on b.
        let accounts = vec![ak("a"), ak("b"), ak("c")];
        let projects = vec![project("p", &["a", "b", "c"], &[])];
        let mut plan = compute_plan(&accounts, &[], &projects);

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
        let projects = vec![project("p", &["a", "b", "c"], &[])];
        let mut plan = compute_plan(&accounts, &[], &projects);

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
        let projects = vec![project("p", &["a", "b"], &[])];
        let mut plan = compute_plan(&accounts, &[], &projects);

        let first = plan.assign_adhoc_worker(&pk("p"), &"reviewer".into(), |_| true);
        let second = plan.assign_adhoc_worker(&pk("p"), &"reviewer".into(), |_| true);
        assert_eq!(first, second);
    }

    #[test]
    fn assign_adhoc_worker_returns_none_for_empty_pool() {
        let accounts = vec![ak("a")];
        let projects = vec![project("p", &["bailed"], &[])];
        let mut plan = compute_plan(&accounts, &[], &projects);
        assert_eq!(plan.assign_adhoc_worker(&pk("p"), &"any".into(), |_| true), None);
    }

    #[test]
    fn assign_adhoc_worker_returns_none_for_unknown_project() {
        let plan_input_projects: Vec<ProjectInput> = Vec::new();
        let mut plan = compute_plan(&[ak("a")], &[], &plan_input_projects);
        assert_eq!(plan.assign_adhoc_worker(&pk("absent"), &"x".into(), |_| true), None);
    }

    #[test]
    fn project_has_no_assignments_false_when_assignments_exist() {
        let plan = compute_plan(&[ak("a")], &[], &[project("p", &["a"], &[])]);
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
        // Boot-time: one ready account; all forge sessions land on it.
        let boot_accounts = vec![ak("a")];
        let projects = vec![project("p", &["a", "b"], &["w1"])];
        let mut plan = compute_plan(&boot_accounts, &[], &projects);
        assert_eq!(plan.lookup(&pk("p"), &"lead".into()), Some(&ak("a")));
        assert_eq!(plan.lookup(&pk("p"), &"w1".into()), Some(&ak("a")));

        // Recovery: account "b" comes back online. Fresh plan would
        // distribute across [a, b] but the frozen overlay must
        // PRESERVE the existing (lead, w1) -> a assignments.
        let recovered_accounts = vec![ak("a"), ak("b")];
        let fresh = compute_plan(&recovered_accounts, &[], &projects);
        plan.merge_frozen(fresh);

        assert_eq!(
            plan.lookup(&pk("p"), &"lead".into()),
            Some(&ak("a")),
            "lead must keep its boot-time account",
        );
        assert_eq!(
            plan.lookup(&pk("p"), &"w1".into()),
            Some(&ak("a")),
            "w1 must keep its boot-time account",
        );
    }

    #[test]
    fn merge_frozen_extends_with_new_adhoc_targets() {
        // After merge, the per-project slot's pool covers the
        // recovered accounts so future adhoc workers can land on
        // them. Existing sessions stay put.
        let boot_accounts = vec![ak("a")];
        let projects = vec![project("p", &["a", "b"], &[])];
        let mut plan = compute_plan(&boot_accounts, &[], &projects);

        let recovered_accounts = vec![ak("a"), ak("b")];
        let fresh = compute_plan(&recovered_accounts, &[], &projects);
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
        let projects = vec![project("p", &["a", "b", "c"], &[])];
        let mut plan = compute_plan(&accounts, &[], &projects);
        let _ = plan.assign_adhoc_worker(&pk("p"), &"w1".into(), |_| true); // slot 1 -> b
        let _ = plan.assign_adhoc_worker(&pk("p"), &"w2".into(), |_| true); // slot 2 -> c

        // Re-compute against the same ready set (e.g., a Bailed
        // account elsewhere recovered without affecting this
        // project's pool). The frozen overlay must keep counter at 3.
        let fresh = compute_plan(&accounts, &[], &projects);
        plan.merge_frozen(fresh);
        let assigned = plan.assign_adhoc_worker(&pk("p"), &"w3".into(), |_| true); // slot 3 mod 3 = 0 -> a
        assert_eq!(assigned, Some(ak("a")));
    }
}
