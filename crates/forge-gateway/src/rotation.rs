//! Rotation state: which accounts are exhausted, when their cooldowns
//! end, and how many 429s are on the streak.
//!
//! Four triggers mark an account exhausted (spec §Rotation): a
//! `rate_limit_event` whose status is not `allowed`, a
//! `anthropic-ratelimit-unified-status` / `-overage-status` of
//! `rejected` on the failing response, any window at 100% utilization
//! with a reset ahead, and five consecutive 429s from one account
//! inside 60 seconds. The first three are proven exhaustion; the
//! streak is the heuristic, which is why its numbers are config.

use std::collections::HashMap;
use std::time::{Duration, Instant};

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

/// Per-account rotation state: cooldown end, and the streak.
#[derive(Default)]
pub struct RotationState {
    /// Cooldown end per account; an account inside its cooldown is
    /// skipped by selection and cannot be re-bound.
    cooldowns: Mutex<HashMap<AccountKey, Instant>>,
    /// Consecutive 429s per account, with the time of the last one.
    streaks: Mutex<HashMap<AccountKey, (u32, Instant)>>,
}

impl RotationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark an account exhausted until `until`.
    pub fn cool_down(&self, key: &AccountKey, until: Instant) {
        self.cooldowns.lock().insert(key.clone(), until);
    }

    /// `true` while the account is inside a cooldown.
    pub fn is_cooling_down(&self, key: &AccountKey, now: Instant) -> bool {
        self.cooldowns.lock().get(key).is_some_and(|until| *until > now)
    }

    /// Record a 429 from `account` at `now`; rotates when the streak
    /// reaches [`STREAK_COUNT`] inside [`STREAK_WINDOW`]. `true` when
    /// the streak fired.
    pub fn record_429(&self, key: &AccountKey, now: Instant) -> bool {
        let mut streaks = self.streaks.lock();
        let entry = streaks.entry(key.clone()).or_insert((0, now));
        if now.duration_since(entry.1) > STREAK_WINDOW {
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

/// `true` when the model's family rule binds it to an Anthropic
/// account.
pub fn wants_anthropic(model: &str) -> bool {
    model.starts_with("claude-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_streak_fires_on_the_fifth_429_inside_the_window() {
        let state = RotationState::new();
        let key = AccountKey("A".to_owned());
        let mut now = Instant::now();
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
        let mut now = Instant::now();
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
        state.cool_down(&key, Instant::now() + Duration::from_secs(60));
        assert!(state.is_cooling_down(&key, Instant::now()));
        assert!(!state.is_cooling_down(&key, Instant::now() + Duration::from_secs(61)));
    }

    #[test]
    fn wants_anthropic_is_the_prefix_rule() {
        assert!(wants_anthropic("claude-opus-5"));
        assert!(!wants_anthropic("glm-5.3-flash"));
        assert!(!wants_anthropic("claude"), "the settled rule is the claude- prefix");
    }
}
