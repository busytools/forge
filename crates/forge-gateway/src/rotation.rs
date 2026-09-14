//! Rotation state: which accounts are exhausted, when their cooldowns
//! end, and how many 429s are on the streak.
//!
//! Four triggers mark an account exhausted (spec, Rotation): a
//! `rate_limit_event` whose status is not `allowed`, a
//! `anthropic-ratelimit-unified-status` / `-overage-status` of
//! `rejected` on the failing response, any window at 100% utilization
//! with a reset ahead, and five consecutive 429s from one account
//! inside 60 seconds. The first three are proven exhaustion; the
//! streak is the heuristic, which is why its numbers are config.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use crate::account::AccountKey;

/// The streak fires when this many consecutive 429s land from one
/// account inside [`STREAK_WINDOW`].
pub const STREAK_COUNT: u32 = 5;

/// The window the streak is counted over.
pub const STREAK_WINDOW: Duration = Duration::from_secs(60);

/// The cooldown applied when neither the failing response nor the
/// quota probe reports a reset time.
pub const NO_RESET_COOLDOWN: Duration = Duration::from_secs(60);

/// A unix-seconds reset time, as the wire frames and headers carry.
pub fn reset_instant(unix_seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(unix_seconds)
}

/// Per-account rotation state: cooldown end, and the streak.
#[derive(Default)]
pub struct RotationState {
    /// Cooldown end per account; an account inside its cooldown is
    /// skipped by selection and cannot be re-bound.
    cooldowns: Mutex<HashMap<AccountKey, SystemTime>>,
    /// Consecutive 429s per account, with the time of the last one.
    streaks: Mutex<HashMap<AccountKey, (u32, SystemTime)>>,
}

impl RotationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark an account exhausted until `until`.
    pub fn cool_down(&self, key: &AccountKey, until: SystemTime) {
        self.cooldowns.lock().insert(key.clone(), until);
    }

    /// `true` while the account is inside a cooldown.
    pub fn is_cooling_down(&self, key: &AccountKey, now: SystemTime) -> bool {
        self.cooldowns.lock().get(key).is_some_and(|until| *until > now)
    }

    /// The soonest a cooling account recovers, for the failure
    /// message. `None` when nothing is cooling.
    pub fn soonest_reset(&self, now: SystemTime) -> Option<SystemTime> {
        self.cooldowns.lock().values().filter(|until| **until > now).copied().min()
    }

    /// Record a 429 from `account` at `now`; rotates when the streak
    /// reaches [`STREAK_COUNT`] inside [`STREAK_WINDOW`]. `true` when
    /// the streak fired.
    pub fn record_429(&self, key: &AccountKey, now: SystemTime) -> bool {
        let mut streaks = self.streaks.lock();
        let entry = streaks.entry(key.clone()).or_insert((0, now));
        if now.duration_since(entry.1).unwrap_or(Duration::ZERO) > STREAK_WINDOW {
            *entry = (1, now);
        } else {
            entry.0 += 1;
            entry.1 = now;
        }
        entry.0 >= STREAK_COUNT
    }

    /// Clear the streak: any non-429 success resets it.
    pub fn reset_streak(&self, key: &AccountKey) {
        self.streaks.lock().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_streak_fires_on_the_fifth_429_inside_the_window() {
        let state = RotationState::new();
        let key = AccountKey("A".to_owned());
        let mut now = SystemTime::now();
        for _ in 0..4 {
            assert!(!state.record_429(&key, now), "the streak must not fire early");
            now += Duration::from_secs(10);
        }
        assert!(state.record_429(&key, now), "the fifth 429 inside the window fires");
    }

    #[test]
    fn a_gap_outside_the_window_resets_the_streak() {
        let state = RotationState::new();
        let key = AccountKey("A".to_owned());
        let mut now = SystemTime::now();
        for _ in 0..4 {
            state.record_429(&key, now);
            now += Duration::from_secs(10);
        }
        now += Duration::from_secs(61);
        assert!(!state.record_429(&key, now), "a 429 outside the window restarts the count");
    }

    #[test]
    fn cooldowns_expire() {
        let state = RotationState::new();
        let key = AccountKey("A".to_owned());
        let now = SystemTime::now();
        state.cool_down(&key, now + Duration::from_secs(60));
        assert!(state.is_cooling_down(&key, now));
        assert!(!state.is_cooling_down(&key, now + Duration::from_secs(61)));
    }

    #[test]
    fn the_soonest_reset_is_the_earliest_live_cooldown() {
        let state = RotationState::new();
        let now = SystemTime::now();
        state.cool_down(&AccountKey("A".to_owned()), now + Duration::from_secs(90));
        state.cool_down(&AccountKey("B".to_owned()), now + Duration::from_secs(30));
        state.cool_down(&AccountKey("C".to_owned()), now - Duration::from_secs(1));
        assert_eq!(
            state.soonest_reset(now),
            Some(now + Duration::from_secs(30)),
            "an expired cooldown is not a live reset"
        );
        assert_eq!(state.soonest_reset(now + Duration::from_secs(120)), None);
    }

    #[test]
    fn a_unix_reset_time_maps_from_the_epoch() {
        let reset = reset_instant(1_000_000);
        assert_eq!(
            reset.duration_since(SystemTime::UNIX_EPOCH).unwrap(),
            Duration::from_secs(1_000_000)
        );
    }
}
