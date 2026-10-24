//! The idle release: a profile nobody has touched for a while closes.
//!
//! **The clock is the server's**, because the browser and the profiles are:
//! every call and every attach touches the profile, and a sweep reads the
//! clocks. A close ends the host and its tabs, not the profile: the next call
//! starts a fresh browser with the logins intact.
//!
//! **A held tab holds the profile open.** A parked hand-off is a person being
//! asked to act in the page, so the sweep skips a profile with any held tab
//! rather than closing it under them; the hold's resolution is what lets the
//! clock close it later.

use std::time::{Duration, Instant};

/// How often the sweep looks. The clock is coarse on purpose: a browser
/// sitting a few seconds past its bound is not a fault.
pub const SWEEP: Duration = Duration::from_secs(30);

/// One profile's clock.
#[derive(Clone, Copy, Debug)]
pub struct Touch {
    /// When a seat last used the profile: a call and an attach.
    pub at: Instant,
}

impl Touch {
    pub fn now() -> Self {
        Self { at: Instant::now() }
    }

    pub fn touched(&mut self) {
        *self = Self::now();
    }
}

/// What one candidate's clock says this pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    Close,
}

/// The clock itself, pure: what the sweep should do for a profile touched at
/// `last`, now.
pub fn step(last: Instant, now: Instant, idle: Duration) -> Step {
    if now.duration_since(last) < idle { Step::Nothing } else { Step::Close }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(30 * 60);

    /// The clock, one case per arm: a fresh touch sits quiet, and a use
    /// restarts the bound.
    #[test]
    fn the_clock_closes_at_the_bound_and_a_touch_resets_it() {
        let base = Instant::now();
        assert_eq!(
            step(base, base + Duration::from_secs(60), IDLE),
            Step::Nothing,
            "a fresh touch is quiet",
        );
        let inside = (base + IDLE).checked_sub(Duration::from_secs(1)).expect("the test's clock");
        assert_eq!(
            step(base, inside, IDLE),
            Step::Nothing,
            "just inside the bound it is still quiet"
        );
        assert_eq!(step(base, base + IDLE, IDLE), Step::Close, "at the bound it closes");
        assert_eq!(
            step(base + Duration::from_secs(60), base + IDLE, IDLE),
            Step::Nothing,
            "a touch inside the old bound makes the browser fresh again",
        );
    }
}
