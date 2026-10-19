//! The idle release: a browser no seat has touched for a while is closed -
//! unless a hand-off is waiting on the person. **A named profile's creator
//! gets one message at the idle bound**; **the shared browser is never
//! announced** - its only crossing is the close itself.
//!
//! **The clock is this client's** (the host owns the profiles, every touch -
//! a session call, a raise, a lower - and the close), because the profiles
//! are per-machine. The message is the server's: one `browser_idle_warning`
//! command goes out for a named profile, the workspace phrases it and
//! delivers it into a live seat's session over the chase's own channel, and
//! answers with the verdict - `delivered` (the seat was told), `held` (a
//! hand-off is waiting on the person: nothing was sent and nothing may
//! close), `no_session` (no one to tell: the browser closes now, quietly) or,
//! on the close's own ask, `clear` (nothing holds it, close).
//!
//! **The two numbers are `[client]` keys** (`browser_idle_minutes`,
//! `browser_grace_minutes`), read off the greeting this client was sent;
//! absent means the built-in pair below. Nothing prepared survives a close:
//! the next call brings the browser back blank, and raising it brings the
//! pages back - the trade the warning's own words name.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

use super::profiles::Seat;
use crate::bridge::Bridge;

/// How many minutes a named profile may sit untouched before its creator is
/// told. **The shared browser is never told about**: it closes at the same
/// total, quietly, with no message on the way.
pub const IDLE_MINUTES: u64 = 20;

/// How long the browser lives past its idle bound unless it is used.
pub const GRACE_MINUTES: u64 = 10;

/// How often the sweep looks. The clock is coarse on purpose: a browser
/// sitting a few seconds past its bound is not a fault.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// The bound one idle dispatch is given. The socket is loopback and its
/// server answers in milliseconds; a dispatch that outlives this is a skew or
/// a wedge the sweep must not inherit - **an error frame never resolves a
/// pending dispatch reply** (a gap in the transport, not this loop's), so an
/// unbounded await here would stall every later pass for the client's
/// lifetime, silently.
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(10);

/// One profile's clock, as the host keeps it.
#[derive(Clone, Debug)]
pub struct Touch {
    /// When a seat last used the profile (a call, a raise, a lower).
    pub at: Instant,
    /// When its seat was last warned, `None` while the streak is fresh.
    pub warned: Option<Instant>,
    /// The seat a named profile's warning goes to - its creator, set by the
    /// host on the candidate it builds. `None` for the shared browser, which
    /// is never warned.
    pub caller: Option<Seat>,
}

/// What one profile's clock says this pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do.
    Nothing,
    /// Idle past the bound and not warned yet: the warn step - a named
    /// profile's message, or the shared browser's silent arm.
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

/// The wire command: the tag and fields the protocol's census pins. `seat`
/// is the seat to warn, `None` for the close's seatless question - the key
/// crosses as null then.
fn warning(
    candidate: &Candidate,
    seat: Option<&Seat>,
    idle_minutes: u64,
    grace_minutes: u64,
    at_close: bool,
) -> serde_json::Value {
    json!({
        "browser_idle_warning": {
            "key": seat.map(|seat| {
                json!({ "org": seat.org, "project": seat.project, "label": seat.label })
            }),
            "profile": candidate.name.clone().unwrap_or_else(|| "shared".to_owned()),
            "idle_minutes": idle_minutes,
            "grace_minutes": grace_minutes,
            "at_close": at_close,
        }
    })
}

/// Start the sweep, once per client run. The loop owns its interval; each
/// pass is one read of the clocks and, at most, one command per candidate.
///
/// **The tauri runtime's spawner, not `tokio::spawn`**: this is called from
/// the app's setup, where no tokio reactor is running and a bare spawn
/// panics - measured the hard way, the walk's first launch aborted 1.8 s in.
pub fn start(bridge: &Arc<Bridge>) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let bridge = Arc::clone(bridge);
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            sweep(&bridge).await;
        }
    });
}

/// What one pass does for one candidate's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crossing {
    Nothing,
    /// Arm the grace without sending anything - the shared browser's warn.
    Arm,
    /// Tell the candidate's seat.
    Warn,
    /// Ask the server whether anything holds it, and close unless something does.
    Close,
}

/// The clock plus the shape, as the decision: `named` says the candidate is a
/// named profile rather than the shared browser. **The shared browser is
/// never warned** (spam-control A2): no message exists for it, so its warn
/// arms the grace in silence.
fn crossing(named: bool, step: Step) -> Crossing {
    match step {
        Step::Nothing => Crossing::Nothing,
        Step::Warn if named => Crossing::Warn,
        Step::Warn => Crossing::Arm,
        Step::Close => Crossing::Close,
    }
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
        let step = step(candidate.touch.at, candidate.touch.warned, now, idle, grace);
        let at_close = match crossing(candidate.name.is_some(), step) {
            Crossing::Nothing => continue,
            // **The shared browser is never announced** (spam-control A2):
            // its idle walk arms the grace in silence, and the close's own
            // question below is its only crossing.
            Crossing::Arm => {
                host.note_warned(&candidate.dir, now).await;
                continue;
            }
            Crossing::Warn => false,
            Crossing::Close => true,
        };
        // **A named profile's seat is its creator** (spam-control A1): the
        // host hands the owner down on the candidate, whoever called it
        // last. The shared browser has no seat - only the close's keyless
        // question - and the hold is still the server's, so **no candidate
        // is closed without it consulted**.
        let seat = if candidate.name.is_some() { candidate.touch.caller.as_ref() } else { None };
        let command = warning(&candidate, seat, idle_minutes, grace_minutes, at_close);
        let verdict = match tokio::time::timeout(DISPATCH_TIMEOUT, socket.dispatch(command, true))
            .await
        {
            Ok(answer) => verdict_of(answer.ok().flatten().as_ref()),
            Err(_) => {
                tauri_plugin_log::log::warn!(
                    "the idle warning went unanswered (event_name browser_idle_dispatch_timeout, \
                         profile {})",
                    candidate.name.as_deref().unwrap_or("shared"),
                );
                Verdict::Unanswered
            }
        };
        match (at_close, verdict) {
            // **Held**: a hand-off is waiting on the person; nothing was
            // delivered and nothing may close. The streak stays armed, and
            // the close's own ask re-checks once per grace - so an abandoned
            // hand-off (the session gone, the browser parked) closes on a
            // later round rather than never.
            (_, Verdict::Held) => host.note_warned(&candidate.dir, now).await,
            // The warning landed: the grace arms.
            (false, Verdict::Delivered) => host.note_warned(&candidate.dir, now).await,
            // **A warning that never went is never armed.** The streak stays
            // unwarned and the next pass asks again - a duplicate warning is
            // acceptable, a close nobody was warned about is not.
            (false, Verdict::Unanswered) => {}
            // No live seat: nobody to warn - and the hold was consulted
            // above, so the profile is the close's. The close's own ask
            // succeeding settles it the same way.
            (false, Verdict::NoSession) | (true, Verdict::Clear) => {
                close_guarded(&host, &candidate, idle, grace).await;
            }
            // Shapes that cannot come back right (a warn answering Clear, a
            // close's ask answering Delivered): nothing is concluded, and
            // nothing is closed on them.
            (false, Verdict::Clear)
            | (true, Verdict::Delivered | Verdict::NoSession | Verdict::Unanswered) => {}
        }
    }
}

/// The kill, re-checked **under the one critical section the raise and every
/// touch take**: `show`, `hide` and each call hold the host's launch lock,
/// and a call's touch is wrapped in it - so a raise or a call landing while
/// the verdict was being asked blocks here, the clock and the window are
/// re-read under the lock, and only a still-idle, still-windowless browser is
/// killed. The verdict that got here is a round trip old; nothing here trusts
/// it.
///
/// **What remains outside**: another CLIENT's raise, which holds its own
/// process's lock rather than this one and does not take the file lock the
/// chromium close does. This cannot see it, and neither could a re-check
/// without a lock shared across clients.
async fn close_guarded(
    host: &crate::browser::BrowserHost,
    candidate: &Candidate,
    idle: Duration,
    grace: Duration,
) {
    let _launching = host.launch.lock().await;
    let fresh = {
        let touches = host.touches.lock().await;
        touches.get(&candidate.dir).cloned()
    };
    let Some(fresh) = fresh else {
        // The entry went with a close: there is nothing left to release.
        return;
    };
    if step(fresh.at, fresh.warned, Instant::now(), idle, grace) != Step::Close {
        return;
    }
    if host.surface_is(&candidate.dir).await {
        return;
    }
    host.close_idle(candidate.name.as_deref()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Duration = Duration::from_secs(30 * 60);
    const GRACE: Duration = Duration::from_secs(10 * 60);

    /// The book (`docs/book/src/configuration.md`) documents 20 and 10 as
    /// what an unset pair means - a half hour of total idle life; a drift
    /// here must fail a test, not the user's expectation.
    #[test]
    fn the_defaults_are_the_documents_twenty_and_ten() {
        assert_eq!(IDLE_MINUTES, 20);
        assert_eq!(GRACE_MINUTES, 10);
    }

    /// **The start survives a context with no reactor**, which is the app's
    /// setup: a bare `tokio::spawn` there aborted a real boot 1.8 s in (the
    /// walk, 2026-10-17), and this test runs in exactly that context - a
    /// plain `#[test]`, no runtime.
    #[test]
    fn the_sweep_starts_outside_a_runtime() {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Bridge::new(None, events);
        start(&bridge);
        start(&bridge);
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

    /// **The shared browser is never warned** (spam-control A2): its warn
    /// step arms the grace in silence, so the close's own question is its
    /// only crossing; a named profile's warn is the message to its seat.
    #[test]
    fn the_shared_browsers_warn_is_an_arm_in_silence() {
        assert_eq!(
            crossing(false, Step::Warn),
            Crossing::Arm,
            "the shared browser's warn never crosses"
        );
        assert_eq!(
            crossing(true, Step::Warn),
            Crossing::Warn,
            "a named profile's warn is the message"
        );
        assert_eq!(
            crossing(true, Step::Close),
            Crossing::Close,
            "a named profile closes at the grace"
        );
        assert_eq!(crossing(false, Step::Close), Crossing::Close, "so does the shared browser");
        assert_eq!(crossing(true, Step::Nothing), Crossing::Nothing, "a fresh touch is quiet");
        assert_eq!(crossing(false, Step::Nothing), Crossing::Nothing, "the shared's too");
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
        let sent = warning(&named, Some(&seat), 30, 10, false);
        assert_eq!(sent["browser_idle_warning"]["profile"], json!("hunt"));
        assert_eq!(sent["browser_idle_warning"]["idle_minutes"], json!(30));
        assert_eq!(sent["browser_idle_warning"]["at_close"], json!(false));
        assert_eq!(sent["browser_idle_warning"]["key"]["org"], json!("Busytools"));

        let shared = Candidate { name: None, ..named };
        let sent = warning(&shared, Some(&seat), 30, 10, true);
        assert_eq!(sent["browser_idle_warning"]["profile"], json!("shared"));
        assert_eq!(sent["browser_idle_warning"]["at_close"], json!(true));

        // No seat: the close's keyless question - the key crosses as null,
        // never as a made-up one.
        let sent = warning(&shared, None, 30, 10, true);
        assert!(sent["browser_idle_warning"]["key"].is_null(), "{sent}");
    }
}
