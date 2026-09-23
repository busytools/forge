//! Rotation state: which accounts are exhausted, when their cooldowns
//! end, and how many 429s are on the streak.
//!
//! Four triggers mark an account exhausted (spec, Rotation): a
//! `rate_limit_event` whose status is not `allowed`, a
//! `anthropic-ratelimit-unified-status` / `-overage-status` of
//! `rejected` on the response, any window at 100% utilization
//! with a reset ahead, and five consecutive 429s from one account
//! inside 60 seconds. The first three are proven exhaustion; the
//! streak is the heuristic, which is why its numbers are config.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use crate::account::AccountKey;

/// The default streak: this many consecutive 429s from one account
/// inside [`STREAK_WINDOW`] fire the rotation.
pub const STREAK_COUNT: u32 = 5;

/// The default window the streak is counted over.
pub const STREAK_WINDOW: Duration = Duration::from_secs(60);

/// The default cooldown applied when neither the failing response nor
/// the account's own usage probe reports a reset time.
pub const NO_RESET_COOLDOWN: Duration = Duration::from_secs(60);

/// The rotation numbers from `[gateway]`: how many 429s fire the
/// streak, the window they are counted over, and the cooldown when no
/// reset time is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationNumbers {
    pub streak_count: u32,
    pub streak_window: Duration,
    pub no_reset_cooldown: Duration,
}

impl Default for RotationNumbers {
    fn default() -> Self {
        Self {
            streak_count: STREAK_COUNT,
            streak_window: STREAK_WINDOW,
            no_reset_cooldown: NO_RESET_COOLDOWN,
        }
    }
}

/// A unix-seconds reset time, as the wire frames and headers carry.
pub fn reset_instant(unix_seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(unix_seconds)
}

/// Per-account rotation state: cooldown end, and the streak.
pub struct RotationState {
    /// Streak count, window, and no-reset cooldown, from `[gateway]`.
    streak_count: u32,
    streak_window: Duration,
    no_reset_cooldown: Duration,
    /// Cooldown end per account; an account inside its cooldown is
    /// skipped by selection and cannot be re-bound.
    cooldowns: Mutex<HashMap<AccountKey, SystemTime>>,
    /// Consecutive 429s per account, with the time of the last one.
    streaks: Mutex<HashMap<AccountKey, (u32, SystemTime)>>,
}

impl Default for RotationState {
    fn default() -> Self {
        Self::with_numbers(RotationNumbers::default())
    }
}

impl RotationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The numbers from `[gateway]`: how many 429s fire the streak,
    /// the window they are counted over, and the cooldown when no
    /// reset time is known.
    pub fn with_numbers(numbers: RotationNumbers) -> Self {
        Self {
            streak_count: numbers.streak_count,
            streak_window: numbers.streak_window,
            no_reset_cooldown: numbers.no_reset_cooldown,
            cooldowns: Mutex::new(HashMap::new()),
            streaks: Mutex::new(HashMap::new()),
        }
    }

    /// The cooldown applied when no reset time is known.
    pub fn no_reset_cooldown(&self) -> Duration {
        self.no_reset_cooldown
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
    /// reaches the configured count inside the configured window.
    /// `true` when the streak fired.
    ///
    /// The window is a max-gap since the last 429, not a sliding
    /// count: a gap beyond the window restarts at 1, so a slow drip
    /// still accumulates. That over-rotates a drip marginally rather
    /// than under-rotating a burst - the conservative direction.
    pub fn record_429(&self, key: &AccountKey, now: SystemTime) -> bool {
        let mut streaks = self.streaks.lock();
        let entry = streaks.entry(key.clone()).or_insert((0, now));
        if now.duration_since(entry.1).unwrap_or(Duration::ZERO) > self.streak_window {
            *entry = (1, now);
        } else {
            entry.0 += 1;
            entry.1 = now;
        }
        entry.0 >= self.streak_count
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

    #[test]
    fn configured_numbers_replace_the_defaults() {
        let state = RotationState::with_numbers(RotationNumbers {
            streak_count: 2,
            streak_window: Duration::from_secs(30),
            no_reset_cooldown: Duration::from_secs(120),
        });
        let key = AccountKey("A".to_owned());
        let mut now = SystemTime::now();
        assert!(!state.record_429(&key, now), "the configured count must not fire early");
        now += Duration::from_secs(10);
        assert!(
            state.record_429(&key, now),
            "the second 429 inside the configured window fires at count 2"
        );
        assert_eq!(
            state.no_reset_cooldown(),
            Duration::from_secs(120),
            "the configured no-reset cooldown is what the failure paths apply"
        );
        // The configured window is the one the max-gap reads against.
        let key = AccountKey("B".to_owned());
        let mut now = SystemTime::now();
        state.record_429(&key, now);
        now += Duration::from_secs(31);
        assert!(
            !state.record_429(&key, now),
            "a 429 beyond the configured 30s window restarts the count"
        );
    }
}
