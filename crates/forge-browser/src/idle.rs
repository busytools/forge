//! The idle release: a profile no viewer has touched for a while closes -
//! unless a hand-off is waiting on the person.
//!
//! **The clock is the server's**, because the browser and the profiles are:
//! every call, every raise and every close touches the profile, and a sweep
//! reads the clocks. A profile is asked about once at the idle bound, and
//! closed when the grace is over unless something holds it.
//!
//! The warning is not this crate's to deliver: the workspace phrases it and
//! puts it in the seat's session, and answers whether it landed, whether a
//! hand-off holds the profile, or that there is no seat to tell. A close
//! ends the host, not the page: the next call brings the browser back on the
//! page last shown, so a session finds what it prepared where it left it.

use std::time::{Duration, Instant};

use forge_primitives::SessionSlot;

/// How many minutes a named profile may sit untouched before its owner is
/// told. The book (`docs/book/src/configuration.md`) documents this pair as
/// what an unset `[client]` key means.
pub const IDLE_MINUTES: u64 = 20;

/// How long the browser lives past its idle bound unless it is used.
pub const GRACE_MINUTES: u64 = 10;

/// How often the sweep looks. The clock is coarse on purpose: a browser
/// sitting a few seconds past its bound is not a fault.
pub const SWEEP: Duration = Duration::from_secs(30);

/// One profile's clock.
#[derive(Clone, Copy, Debug)]
pub struct Touch {
    /// When a seat last used the profile: a call, a raise, a close.
    pub at: Instant,
    /// When its owner was last warned, `None` while the streak is fresh.
    pub warned: Option<Instant>,
}

impl Touch {
    pub fn now() -> Self {
        Self { at: Instant::now(), warned: None }
    }

    /// A use resets the whole streak, the warning included.
    pub fn touched(&mut self) {
        *self = Self::now();
    }
}

/// What one candidate's clock says this pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Idle past the bound and not warned: ask whether it may close, and arm
    /// the grace.
    Warn,
    /// Warned and past the grace: ask whether anything holds it, and close
    /// unless something does.
    Close,
}

/// The clock itself, pure: what the sweep should do for a profile touched at
/// `last`, warned at `warned` (if ever), now.
pub fn step(
    last: Instant,
    warned: Option<Instant>,
    now: Instant,
    idle: Duration,
    grace: Duration,
) -> Step {
    if now.duration_since(last) < idle {
        return Step::Nothing;
    }
    match warned {
        None => Step::Warn,
        Some(at) if now.duration_since(at) >= grace => Step::Close,
        Some(_) => Step::Nothing,
    }
}

/// What the workspace answered to one warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The owner was told; the grace arms.
    Delivered,
    /// A hand-off is waiting on the person: nothing was said and nothing may
    /// close. The grace re-arms, so an abandoned hand-off closes later rather
    /// than never.
    Held,
    /// No live session to tell: the profile is the close's.
    NoSession,
    /// The close's own question, answered: nothing holds it.
    Clear,
    /// The question could not be delivered; nothing is concluded and the
    /// sweep tries again next pass.
    Unanswered,
}

/// How the sweep delivers a warning, as the server implements it.
#[async_trait::async_trait]
pub trait IdleWarn: Send + Sync {
    /// Ask about one profile: `seat` is the owner to warn (`None` for the
    /// shared browser's keyless close question), `name` is the profile's name
    /// as a person reads it.
    async fn warn(
        &self,
        seat: Option<&SessionSlot>,
        name: &str,
        idle_minutes: u64,
        grace_minutes: u64,
        at_close: bool,
    ) -> Verdict;
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(30 * 60);
    const GRACE: Duration = Duration::from_secs(10 * 60);

    /// The book documents 20 and 10 as what an unset pair means - a half hour
    /// of total idle life; a drift here must fail a test, not the user's
    /// expectation.
    #[test]
    fn the_defaults_are_the_documents_twenty_and_ten() {
        assert_eq!(IDLE_MINUTES, 20);
        assert_eq!(GRACE_MINUTES, 10);
    }

    /// The whole clock, one case per arm: a fresh touch sits quiet, an idle
    /// one warns once, and the warning's grace decides between waiting and
    /// closing.
    #[test]
    fn the_clock_warns_once_and_then_closes() {
        let base = Instant::now();
        let fresh = base + Duration::from_secs(60);
        assert_eq!(step(base, None, fresh, IDLE, GRACE), Step::Nothing, "a fresh touch is quiet");

        let idle = base + IDLE;
        assert_eq!(step(base, None, idle, IDLE, GRACE), Step::Warn, "at the bound, warn");

        let warned_at = idle;
        let within = warned_at
            .checked_add(GRACE)
            .and_then(|at| at.checked_sub(Duration::from_secs(1)))
            .expect("the test's clock");
        assert_eq!(
            step(base, Some(warned_at), within, IDLE, GRACE),
            Step::Nothing,
            "inside the grace, the browser lives",
        );

        let past = warned_at + GRACE;
        assert_eq!(
            step(base, Some(warned_at), past, IDLE, GRACE),
            Step::Close,
            "past the grace, close"
        );
    }

    /// A use resets the whole streak: the old warning no longer stands over
    /// a fresh touch.
    #[test]
    fn a_touch_resets_the_streak() {
        let base = Instant::now();
        let warned_at = base + IDLE;
        let touched_again = warned_at + Duration::from_secs(5);
        assert_eq!(
            step(touched_again, None, warned_at + GRACE, IDLE, GRACE),
            Step::Nothing,
            "a touch inside the old grace makes the browser fresh again",
        );
    }
}
