//! When a dead seat earns a restart, and how long the next one waits.
//!
//! Pure: no I/O, no clock of its own. The caller counts a seat's deaths,
//! tells this policy how many there have been, and asks whether a run has
//! lasted long enough to count as stable again.

use std::time::Duration;

use crate::config::SessionsConfig;

/// What a seat is, at the moment it died, on the axes that decide whether
/// the death is heal-eligible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SeatFacts {
    /// A project lead, or a worker with a registry row. An ad-hoc session
    /// has no row, so it is the user's to wake.
    pub durable: bool,
    /// The seat was up before this death. A first spawn or connection
    /// failure is deterministic and already surfaced.
    pub connected_once: bool,
    /// A despawn or close owns this seat; healing it would undo the stop.
    pub stopping: bool,
    /// Forge itself is going down; the boot re-spawn owns the return.
    pub shutting_down: bool,
}

/// The settled heal schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealPolicy {
    pub enabled: bool,
    pub attempts: usize,
    pub backoff: Vec<Duration>,
    pub stable: Duration,
}

impl HealPolicy {
    pub(crate) fn from_config(config: &SessionsConfig) -> Self {
        Self {
            enabled: config.heal,
            attempts: config.heal_attempts,
            backoff: config.heal_backoff.clone(),
            stable: config.heal_stable,
        }
    }

    /// Whether this death may be healed at all.
    pub fn is_eligible(&self, facts: SeatFacts) -> bool {
        self.enabled
            && facts.durable
            && facts.connected_once
            && !facts.stopping
            && !facts.shutting_down
    }

    /// How long to wait before restarting a seat that has taken `deaths`
    /// deaths, counting the one just recorded. `None` once the attempt
    /// budget is spent: the seat stays failed with its last diagnostic.
    pub fn next_after(&self, deaths: u32) -> Option<Duration> {
        if deaths == 0 || deaths as usize > self.attempts {
            return None;
        }
        let index = deaths as usize - 1;
        self.backoff.get(index).copied().or_else(|| self.backoff.last().copied())
    }

    /// Whether a seat up for `up_for` has run long enough that its next
    /// death starts the count again.
    pub fn resets_after(&self, up_for: Duration) -> bool {
        up_for >= self.stable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HealPolicy {
        HealPolicy {
            enabled: true,
            attempts: 3,
            backoff: vec![
                Duration::from_secs(5),
                Duration::from_secs(30),
                Duration::from_secs(120),
            ],
            stable: Duration::from_secs(600),
        }
    }

    /// The settled schedule: +5 s, +30 s, +2 min, then nothing - the
    /// fourth death is the escalation rather than a fourth attempt.
    #[test]
    fn the_attempt_schedule_runs_out_after_the_third_restart() {
        let policy = policy();
        assert_eq!(policy.next_after(1), Some(Duration::from_secs(5)));
        assert_eq!(policy.next_after(2), Some(Duration::from_secs(30)));
        assert_eq!(policy.next_after(3), Some(Duration::from_secs(120)));
        assert_eq!(policy.next_after(4), None, "the fourth death leaves the seat failed");
        assert_eq!(policy.next_after(0), None, "no death, no attempt");
    }

    /// The attempt count bounds the schedule rather than the list: a
    /// shorter budget stops answering once it is spent.
    #[test]
    fn the_attempt_count_bounds_the_schedule() {
        let policy = HealPolicy { attempts: 1, ..policy() };
        assert_eq!(policy.next_after(1), Some(Duration::from_secs(5)));
        assert_eq!(policy.next_after(2), None);
    }

    /// A backoff list shorter than the budget repeats its last interval,
    /// so the seat keeps spacing its attempts rather than jumping to
    /// failed.
    #[test]
    fn a_short_backoff_repeats_its_last_interval() {
        let policy = HealPolicy { backoff: vec![Duration::from_secs(7)], ..policy() };
        assert_eq!(policy.next_after(1), Some(Duration::from_secs(7)));
        assert_eq!(policy.next_after(2), Some(Duration::from_secs(7)));
        assert_eq!(policy.next_after(3), Some(Duration::from_secs(7)));
        assert_eq!(policy.next_after(4), None);
    }

    /// Eligibility is the conjunction of five facts, each of which alone
    /// must refuse: heal off, an ad-hoc seat, never connected, a stop
    /// pending, and forge shutting down.
    #[test]
    fn eligibility_refuses_each_ineligible_fact() {
        let eligible = SeatFacts { durable: true, connected_once: true, ..SeatFacts::default() };
        assert!(policy().is_eligible(eligible), "a durable seat that was up heals");

        let off = HealPolicy { enabled: false, ..policy() };
        assert!(!off.is_eligible(eligible), "heal = false turns the whole thing off");

        for (name, facts) in [
            ("an ad-hoc seat", SeatFacts { durable: false, ..eligible }),
            ("a seat that never connected", SeatFacts { connected_once: false, ..eligible }),
            ("a despawn or close", SeatFacts { stopping: true, ..eligible }),
            ("forge shutting down", SeatFacts { shutting_down: true, ..eligible }),
        ] {
            assert!(!policy().is_eligible(facts), "{name} must not heal");
        }
    }

    /// A run that has lasted the stable window resets the count; one just
    /// short of it does not, so a seat that dies every few minutes never
    /// earns itself a fresh budget.
    #[test]
    fn a_stable_run_resets_the_count() {
        let policy = policy();
        assert!(policy.resets_after(Duration::from_secs(600)));
        assert!(policy.resets_after(Duration::from_secs(601)));
        assert!(!policy.resets_after(Duration::from_secs(599)));
    }

    /// The policy's schedule is the config table's, so an edit to
    /// `[sessions]` changes what the supervisor does without a second
    /// table in between.
    #[test]
    fn the_policy_is_built_from_the_sessions_table() {
        let config = SessionsConfig {
            heal: false,
            heal_attempts: 2,
            heal_backoff: vec![Duration::from_secs(9), Duration::from_secs(60)],
            heal_stable: Duration::from_secs(120),
        };
        let policy = HealPolicy::from_config(&config);
        assert!(!policy.enabled);
        assert_eq!(policy.attempts, 2);
        assert_eq!(policy.next_after(2), Some(Duration::from_secs(60)));
        assert_eq!(policy.next_after(3), None);
        assert!(policy.resets_after(Duration::from_secs(120)));
    }
}
