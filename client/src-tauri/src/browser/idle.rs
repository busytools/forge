//! The idle release: a browser no seat has touched for a while is warned
//! about, and then closed - unless a hand-off is waiting on the person.
//!
//! **The clock is this client's** (the host owns the profiles, every touch -
//! a session call, a raise, a lower - and the close), because the profiles
//! are per-machine. The warning is the server's: one `browser_idle_warning`
//! command goes out per candidate, the workspace phrases it and delivers it
//! into a live seat's session over the chase's own channel, and answers with
//! the verdict - `delivered` (the seat was told), `held` (a hand-off is
//! waiting on the person: nothing was sent and nothing may close), `no_session`
//! (no one to tell: the browser closes now, quietly) or, on the close's own
//! ask, `clear` (nothing holds it, close).
//!
//! **The two numbers are `[client]` keys** (`browser_idle_minutes`,
//! `browser_grace_minutes`), read off the greeting this client was sent;
//! absent means the built-in pair below. Nothing prepared survives a close
//! except the pages themselves, which reopen on the next call - that is the
//! trade the warning's own words name.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use super::profiles::Seat;
use crate::bridge::Bridge;

/// How many minutes a browser may sit untouched before its seat is told.
pub const IDLE_MINUTES: u64 = 30;

/// How long after the warning the browser lives unless it is used.
pub const GRACE_MINUTES: u64 = 10;

/// How often the sweep looks. The clock is coarse on purpose: a browser
/// sitting a few seconds past its bound is not a fault.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// One profile's clock, as the host keeps it.
#[derive(Clone, Debug)]
pub struct Touch {
    /// When a seat last used the profile (a call, a raise, a lower).
    pub at: Instant,
    /// When its seat was last warned, `None` while the streak is fresh.
    pub warned: Option<Instant>,
    /// The seat that last USED it: the owner of a named profile, or whoever
    /// last called the shared browser. `None` means no one to tell.
    pub caller: Option<Seat>,
}

/// What one profile's clock says this pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do.
    Nothing,
    /// Idle past the bound and not warned yet: tell the seat.
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

/// One profile the sweep may act on.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// The named profile, or `None` for the shared browser.
    pub name: Option<String>,
    pub dir: PathBuf,
    pub touch: Touch,
}

/// What the server answered to one warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Delivered,
    Held,
    NoSession,
    Clear,
    /// The command could not be asked (no connection, a refusal): nothing is
    /// concluded and the sweep tries again next pass.
    Unanswered,
}

/// Read one verdict off the reply body, which the server sends as its own
/// snake_case word.
fn verdict_of(body: Option<&serde_json::Value>) -> Verdict {
    match body.and_then(serde_json::Value::as_str) {
        Some("delivered") => Verdict::Delivered,
        Some("held") => Verdict::Held,
        Some("no_session") => Verdict::NoSession,
        Some("clear") => Verdict::Clear,
        _ => Verdict::Unanswered,
    }
}

/// The wire command: the tag and fields the protocol's census pins.
fn warning(
    candidate: &Candidate,
    seat: &Seat,
    idle_minutes: u64,
    grace_minutes: u64,
    at_close: bool,
) -> serde_json::Value {
    json!({
        "browser_idle_warning": {
            "key": { "org": seat.org, "project": seat.project, "label": seat.label },
            "profile": candidate.name.clone().unwrap_or_else(|| "shared".to_owned()),
            "idle_minutes": idle_minutes,
            "grace_minutes": grace_minutes,
            "at_close": at_close,
        }
    })
}

/// Start the sweep, once per client run. The loop owns its interval; each
/// pass is one read of the clocks and, at most, one command per candidate.
pub fn start(bridge: &Arc<Bridge>) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let bridge = Arc::clone(bridge);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            sweep(&bridge).await;
        }
    });
}

/// One pass: every candidate's clock, and the verdicts the server gives.
///
/// A warned-but-held profile re-asks once per grace, so an abandoned
/// hand-off (the session gone, the browser still parked) closes on the next
/// rounds rather than never.
async fn sweep(bridge: &Arc<Bridge>) {
    let Some(host) = bridge.host() else { return };
    let Some(socket) = bridge.socket() else { return };
    let (idle_minutes, grace_minutes) = bridge.browser_idle_minutes();
    let idle = Duration::from_secs(idle_minutes.saturating_mul(60));
    let grace = Duration::from_secs(grace_minutes.saturating_mul(60));
    let now = Instant::now();
    for candidate in host.idle_candidates().await {
        let at_close = match step(candidate.touch.at, candidate.touch.warned, now, idle, grace) {
            Step::Nothing => continue,
            Step::Warn => false,
            Step::Close => true,
        };
        // **No one to tell closes now, quietly** - the shared browser never
        // called, a profile whose seat is gone.
        let Some(seat) = candidate.touch.caller.as_ref() else {
            host.close_idle(candidate.name.as_deref()).await;
            continue;
        };
        let command = warning(&candidate, seat, idle_minutes, grace_minutes, at_close);
        let verdict = verdict_of(socket.dispatch(command, true).await.ok().flatten().as_ref());
        match verdict {
            // **Held**: a hand-off is waiting on the person; nothing was
            // delivered and nothing may close. The streak stays armed, and
            // the close's own ask re-checks once per grace - so an abandoned
            // hand-off (the session gone, the browser parked) closes on a
            // later round rather than never.
            Verdict::Held => host.note_warned(&candidate.dir, now).await,
            Verdict::Delivered => host.note_warned(&candidate.dir, now).await,
            Verdict::NoSession | Verdict::Clear => {
                host.close_idle(candidate.name.as_deref()).await;
            }
            // Asked and not answered (no connection, a refusal): nothing is
            // concluded; the next pass asks again.
            Verdict::Unanswered => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(30 * 60);
    const GRACE: Duration = Duration::from_secs(10 * 60);

    /// The whole clock, one case per arm: a fresh touch sits quiet, an idle
    /// one warns once, and the warning's grace decides between waiting and
    /// closing.
    #[test]
    fn the_clock_warns_once_and_then_closes() {
        let base = Instant::now();
        let fresh = base + Duration::from_secs(60);
        assert_eq!(step(base, None, fresh, IDLE, GRACE), Step::Nothing, "a fresh touch is quiet");

        let idle = base + IDLE;
        assert_eq!(
            step(base, None, idle, IDLE, GRACE),
            Step::Warn,
            "at the bound, the seat is told"
        );

        let warned_at = idle;
        let within = warned_at + GRACE - Duration::from_secs(1);
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

    /// A touch resets the whole streak: the warned flag is the caller's to
    /// clear, and a fresh touch beats whatever state stood before.
    #[test]
    fn a_touch_resets_the_streak() {
        let base = Instant::now();
        let warned_at = base + IDLE;
        let touched_again = warned_at + Duration::from_secs(5);
        assert_eq!(
            step(touched_again, None, warned_at + GRACE, IDLE, GRACE),
            Step::Nothing,
            "the old warning no longer stands over a fresh touch",
        );
    }

    /// The verdict parser: the server's own word, or nothing concluded.
    #[test]
    fn the_verdict_is_the_servers_word() {
        assert_eq!(verdict_of(Some(&json!("delivered"))), Verdict::Delivered);
        assert_eq!(verdict_of(Some(&json!("held"))), Verdict::Held);
        assert_eq!(verdict_of(Some(&json!("no_session"))), Verdict::NoSession);
        assert_eq!(verdict_of(Some(&json!("clear"))), Verdict::Clear);
        assert_eq!(verdict_of(Some(&json!("something else"))), Verdict::Unanswered);
        assert_eq!(verdict_of(None), Verdict::Unanswered);
    }

    /// The command's wire shape: the tag the protocol's census pins, the seat
    /// triple, and the profile under the name the dock would draw - the
    /// shared browser crossing as "shared" rather than an absent name.
    #[test]
    fn the_warning_carries_the_protocols_own_names() {
        let seat = Seat {
            org: "Busytools".to_owned(),
            project: "forge".to_owned(),
            label: "lead".to_owned(),
        };
        let named = Candidate {
            name: Some("hunt".to_owned()),
            dir: PathBuf::from("/tmp/hunt"),
            touch: Touch { at: Instant::now(), warned: None, caller: None },
        };
        let sent = warning(&named, &seat, 30, 10, false);
        assert_eq!(sent["browser_idle_warning"]["profile"], json!("hunt"));
        assert_eq!(sent["browser_idle_warning"]["idle_minutes"], json!(30));
        assert_eq!(sent["browser_idle_warning"]["at_close"], json!(false));
        assert_eq!(sent["browser_idle_warning"]["key"]["org"], json!("Busytools"));

        let shared = Candidate { name: None, ..named };
        let sent = warning(&shared, &seat, 30, 10, true);
        assert_eq!(sent["browser_idle_warning"]["profile"], json!("shared"));
        assert_eq!(sent["browser_idle_warning"]["at_close"], json!(true));
    }
}
