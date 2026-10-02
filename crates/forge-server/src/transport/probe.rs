//! The socket's own bound on asking the core for a fresh context reading, the
//! same two numbers the terminal bounds its ask with: a probe is answered
//! inline over the CLI's whole transcript, so it is worth asking for only when
//! the reading it would replace is old enough and the transcript small enough
//! to pay for it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use forge_primitives::SessionSlot;

use crate::surface::inspector::ContextUsage;

/// How long a seat's reading stands before the socket asks for another, the
/// terminal's `CONTEXT_USAGE_MIN_SEND_INTERVAL`.
const MIN_INTERVAL: Duration = Duration::from_secs(60);

/// Used tokens at or above which the socket does not ask at all, the
/// terminal's `CONTEXT_USAGE_TOKEN_GATE`: only a `[1m]`-class window reaches
/// this size, where the CLI's walk over the transcript can exceed the hook
/// timeout.
const TOKEN_GATE: u64 = 500_000;

/// The socket's bound on asking, held for the life of the fold.
#[derive(Default)]
pub struct ContextProbe {
    /// When the socket last asked for each seat's reading.
    last_asked: HashMap<SessionSlot, Instant>,
}

/// Why the socket did not ask a seat a page holds, so a reading that has
/// stopped moving is answered from the log rather than from a repro.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Declined {
    /// The transcript is big enough that the CLI's computation over it can
    /// exceed the hook timeout.
    LargeTranscript,
    /// The seat was asked for a reading less than [`MIN_INTERVAL`] ago.
    RecentlyAsked,
}

impl ContextProbe {
    /// Whether the socket may ask for a fresh reading on `slot`, recording the
    /// ask when it may.
    ///
    /// The reading arrives as a thunk because it is built under the session's
    /// lock, and the interval can refuse the ask without it - so a fold that
    /// took the reading first would hold that lock once per finished turn, on
    /// the one task that serves every client.
    pub fn admit(
        &mut self,
        slot: &SessionSlot,
        reading: impl FnOnce() -> ContextUsage,
        now: Instant,
    ) -> Result<(), Declined> {
        self.expire(now);
        if self.too_soon(slot, now) {
            return Err(Declined::RecentlyAsked);
        }
        if used_tokens(&reading()).is_some_and(|used| used >= TOKEN_GATE) {
            return Err(Declined::LargeTranscript);
        }
        self.last_asked.insert(slot.clone(), now);
        Ok(())
    }

    /// Record an ask made past both bounds, which is the one case they must not
    /// refuse: a compaction settled, so the held reading was taken from a
    /// transcript that no longer exists.
    ///
    /// Both bounds read the reading they would be replacing, so without this a
    /// seat past the token gate refuses every ask and its number stands at the
    /// high value for the life of its occupant.
    pub fn force(&mut self, slot: &SessionSlot, now: Instant) {
        self.expire(now);
        self.last_asked.insert(slot.clone(), now);
    }

    /// Whether `slot` was asked inside the interval, without building a
    /// reading to find out.
    fn too_soon(&self, slot: &SessionSlot, now: Instant) -> bool {
        self.last_asked.get(slot).is_some_and(|last| now.duration_since(*last) < MIN_INTERVAL)
    }

    /// Drop the seats whose last ask has aged out: the fold has no seat-closed
    /// signal to hang a removal on, and an entry older than the interval
    /// refuses nothing, so expiry is the whole of the bound.
    fn expire(&mut self, now: Instant) {
        self.last_asked.retain(|_, last| now.duration_since(*last) < MIN_INTERVAL);
    }
}

/// The tokens `reading` implies have been used, `None` when the seat reports
/// only one half of the sum - a seat that cannot be measured, which is asked
/// rather than gated.
fn used_tokens(reading: &ContextUsage) -> Option<u64> {
    let percent = u64::from(reading.percent?);
    Some(percent.saturating_mul(reading.max_tokens?) / 100)
}

#[cfg(test)]
mod tests {
    use super::{ContextProbe, Declined, MIN_INTERVAL, TOKEN_GATE};
    use crate::surface::inspector::ContextUsage;
    use forge_primitives::SessionSlot;
    use std::time::Instant;

    fn seat(label: &str) -> SessionSlot {
        SessionSlot::lead("TestOrg", label)
    }

    fn reading(percent: u8, max_tokens: u64) -> ContextUsage {
        ContextUsage { percent: Some(percent), max_tokens: Some(max_tokens) }
    }

    /// Only a window large enough for the CLI's own walk over the transcript
    /// to exceed the hook timeout is refused, and a reading that reports one
    /// half of the sum without the other is asked for rather than assumed
    /// large: the gate bounds a cost, and an unmeasured cost is not a refusal.
    #[test]
    fn a_transcript_too_large_to_recompute_is_not_asked() {
        let mut probes = ContextProbe::default();
        let now = Instant::now();
        let window = 1_000_000;
        let at_the_gate = u8::try_from(TOKEN_GATE * 100 / window).expect("a percentage");

        assert_eq!(
            probes.admit(&seat("big"), || reading(at_the_gate, window), now),
            Err(Declined::LargeTranscript),
            "a transcript at the token gate exactly is not asked for",
        );
        assert_eq!(
            probes.admit(&seat("small"), || reading(at_the_gate - 1, window), now),
            Ok(()),
            "and one below it is, which is the whole of the gate",
        );
        assert_eq!(
            probes.admit(
                &seat("unmeasured"),
                || ContextUsage { percent: Some(99), max_tokens: None },
                now,
            ),
            Ok(()),
            "a reading carrying half the sum is asked for rather than read as large",
        );
    }

    /// The interval is a bound, not a latch: a seat asked inside it is refused,
    /// and the same seat past it is asked again - which is what keeps a reading
    /// from standing for the life of its occupant.
    #[test]
    fn a_seat_asked_within_the_interval_is_not_asked_again() {
        let mut probes = ContextProbe::default();
        let now = Instant::now();
        let small = || reading(12, 200_000);

        assert_eq!(probes.admit(&seat("busy"), small, now), Ok(()), "the first ask goes");
        assert_eq!(
            probes.admit(&seat("busy"), small, now + MIN_INTERVAL / 2),
            Err(Declined::RecentlyAsked),
            "and the same seat inside the interval is refused",
        );
        assert_eq!(
            probes.admit(&seat("busy"), small, now + MIN_INTERVAL),
            Ok(()),
            "past it the seat is askable again",
        );
        assert_eq!(
            probes.admit(&seat("busy"), small, now + MIN_INTERVAL),
            Err(Declined::RecentlyAsked),
            "and the ask just made restarts the interval rather than leaving it open",
        );
    }

    /// One fold serves every seat, so the interval has to be the seat's own: a
    /// bound held for the socket rather than for the seat would refuse a page's
    /// probe because some other seat finished a turn.
    ///
    /// The refusal of the seat that asked is here as well as the allowance of
    /// the one that did not, because the allowance alone holds with the
    /// interval gone - an assertion that both seats are let past pins nothing.
    #[test]
    fn the_interval_is_held_per_seat() {
        let mut probes = ContextProbe::default();
        let now = Instant::now();
        let small = || reading(12, 200_000);

        assert_eq!(probes.admit(&seat("busy"), small, now), Ok(()), "one seat asks");
        assert_eq!(
            probes.admit(&seat("busy"), small, now + MIN_INTERVAL / 2),
            Err(Declined::RecentlyAsked),
            "and is refused inside the interval",
        );
        assert_eq!(
            probes.admit(&seat("quiet"), small, now + MIN_INTERVAL / 2),
            Ok(()),
            "while a seat that was never asked is not refused by it",
        );
    }

    /// An entry older than the interval refuses nothing, so it is dropped
    /// rather than held for the life of the process - one per seat the socket
    /// has ever asked, on a machine that has spawned workers since boot.
    #[test]
    fn a_seat_that_asked_long_ago_is_not_held() {
        let mut probes = ContextProbe::default();
        let now = Instant::now();
        let small = || reading(12, 200_000);

        assert_eq!(probes.admit(&seat("old"), small, now), Ok(()), "one seat asks");
        assert_eq!(probes.last_asked.len(), 1, "precondition: the ask is held");
        assert_eq!(
            probes.admit(&seat("new"), small, now + MIN_INTERVAL),
            Ok(()),
            "and a second asks past the interval",
        );
        assert_eq!(
            probes.last_asked.len(),
            1,
            "the first seat's ask is gone rather than standing beside it: {:?}",
            probes.last_asked.keys().collect::<Vec<_>>(),
        );
    }

    /// The one ask the bounds may not refuse: a compaction settled, so the
    /// reading the gate would read was taken from a transcript that is gone.
    /// Without this a seat past the token gate refuses every ask and its number
    /// stands at the high value for the life of its occupant.
    #[test]
    fn a_forced_ask_goes_past_both_bounds() {
        let mut probes = ContextProbe::default();
        let now = Instant::now();
        let huge = || reading(99, 1_000_000);

        assert_eq!(
            probes.admit(&seat("full"), huge, now),
            Err(Declined::LargeTranscript),
            "precondition: the gate refuses this seat's ordinary ask",
        );

        probes.force(&seat("full"), now);
        assert_eq!(
            probes.admit(&seat("full"), huge, now + MIN_INTERVAL / 2),
            Err(Declined::RecentlyAsked),
            "and the forced ask took the seat's place in the interval like any other",
        );
    }
}
