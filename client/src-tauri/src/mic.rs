//! The desktop's own microphone.
//!
//! The page does not capture here. This half opens the device through
//! `forge-capture`, frames what it delivers into the 20 ms mono i16 frames
//! the wire carries, and hands them to the page, which streams them down the
//! take's own socket - so the server and the take feed see exactly what the
//! browser's capture sends, and a native open skips the browser's audio
//! pipeline and its permission round trip on the way.
//!
//! **The device is held, not opened per take.** It is opened when the page
//! names an input to hold - at client start, and again whenever the pick
//! changes - and it stays open through every take, so a press records from
//! the press: no device open under it, and none of the device's own start-up
//! silence inside its first second. The cost is the microphone indicator
//! staying lit while the client is on screen and unlocked, which is the trade
//! this half makes.
//!
//! **The hold follows the screen.** It is kept while the client is on screen
//! and unlocked - the normal state - and let go when the screen is (about to
//! be) locked or the page is hidden, because a locked screen delivers no
//! audio and a capture kept through it is one every later staleness check
//! would reopen. Each reopen of a capture the device has stopped serving is
//! bounded: a few attempts with growing waits, then the hold gives up until a
//! real trigger - a press, another pick, or the screen coming back.
//!
//! While no take is recording, the held stream's frames are dropped - the
//! audio belongs to no take, and only a take's frames go to the page.
//!
//! A hold or a press from a page other than the one recording ends that
//! recording instead of failing against it: a reload discards the page that
//! held the stop, so the page that comes back is what ends the orphaned take.

#![allow(
    clippy::cast_possible_truncation,
    reason = "encode_frame clamps each sample to the i16 range one line before the cast; it is \
              the codec's own conversion"
)]
#![allow(
    clippy::needless_pass_by_value,
    reason = "hold and start take the device owned because start_within moves it onto the \
              capture thread; the sync entry points keep one shape with it"
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use forge_primitives::SessionSlot;
use serde::{Deserialize, Serialize};

use crate::bridge::Bridge;

#[cfg(target_os = "macos")]
pub(crate) mod screen;

/// This module's own log target, which the client's default directives name
/// so a line demoted under rule 20 stays readable in `forge.log`.
pub(crate) const LOG_TARGET: &str = module_path!();

/// One frame's payload: 20 ms of mono 16 kHz samples, the cadence the
/// server's meter polls at.
pub const FRAME_SAMPLES: usize = 320;

/// How long the capture stays open past the release.
///
/// The device's own pipeline holds audio that is already captured and not
/// yet handed over; it only arrives while the stream is still running. The
/// window is the browser leg's measured 42-53 ms with room for this one,
/// which was not measured separately.
pub const DRAIN_MS: u64 = 150;

/// How long a held capture may go without a block from its device before a
/// hold or a press treats it as one the device stopped serving, and opens
/// the device again.
///
/// A stream that stopped and never came back is what this is for (#2359:
/// Android disconnects the input when the audio layer under the app changes).
/// The window runs from the open as well as from the last block, so a device
/// whose first block is slower than this is opened again needlessly - which
/// costs the warm open the press was promised, never a take's audio.
const SILENT_LIMIT: Duration = Duration::from_millis(500);

/// Attempts at a device the hold can use - a reopen of a capture that
/// stopped serving, or an open that failed - per real trigger, before the
/// hold gives up; it opens nothing again until a real trigger arrives.
///
/// **A device the screen took away must not be reopened for ever.** A locked
/// or sleeping screen delivers no audio, and a device that is really gone
/// answers the same way, so an unbounded retry spends a microphone open every
/// few seconds - and each open on macOS rebuilds the menu bar.
const REOPEN_ATTEMPTS: usize = 3;

/// The wait before each retry, in the order the attempts are made.
///
/// The first is taken at once - a stream can drop for a moment and come back
/// on its own - and the rest grow, so a device that is really gone is tried a
/// few times and then left alone.
const REOPEN_WAITS: [Duration; REOPEN_ATTEMPTS] =
    [Duration::ZERO, Duration::from_secs(2), Duration::from_secs(5)];

/// How long a take's open may run before the shell says the microphone did
/// not come up.
///
/// Long enough for a permission dialog to be answered, short enough that a
/// box waiting on nothing becomes a line.
pub const OPEN_LIMIT: Duration = Duration::from_secs(20);

/// What the take's line says when the open has not answered within the
/// caller's bound, whether the bound answered it or the device did, late.
const NOT_COME_UP: &str = "the microphone did not come up";

/// What a press is answered while the screen is away or the page hidden.
///
/// The press cannot be served there - a locked screen feeds a take nothing -
/// and a device opened for it is the very reopen this rule exists to stop.
/// The line is the reader's, for the press that lands on the lock's edge.
const AWAY: &str = "the microphone is away: the screen is locked or the page is hidden";

/// Run device work off the reactor with any unwind caught and answered.
///
/// **What this buys, and what it cannot.** `spawn_blocking` already turns a
/// panic in its task into a `JoinError`, so this is not what stops an abort:
/// the `rust_foreign_exception` in #2426's report is an unwind crossing a
/// NON-Rust frame, which no catch at this layer can reach (the audio
/// callback's own guard in `forge-capture` is the one that can). What the
/// guard buys is the part the issue asked for - a named error the command
/// answers with, and a warning carrying the operation, the requested device
/// and a backtrace, logged from INSIDE the blocking task. That last point is
/// load-bearing: a take's open abandoned at `OPEN_LIMIT` is still running its
/// blocking task, and a panic after the timeout would otherwise leave no
/// line at all.
pub(crate) async fn guarded<T, F>(
    operation: &'static str,
    device: Option<String>,
    work: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    record_panic_sites();
    let joined = tokio::task::spawn_blocking(move || {
        // Clear this thread's slot first: what the hook records must be this
        // call's panic, not an earlier one on a reused thread.
        take_panic_site();
        let answered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
        match answered {
            Ok(value) => Ok(value),
            Err(panic) => {
                let why = panic_text(panic.as_ref());
                // **At the raise, not at the catch**: the hook's backtrace
                // names the frames that panicked; the fallback is the catch
                // site, and says so by being the fallback.
                let backtrace =
                    take_panic_site().unwrap_or_else(std::backtrace::Backtrace::force_capture);
                tauri_plugin_log::log::warn!(
                    target: LOG_TARGET,
                    "the microphone device layer panicked during {operation} (device {device:?}): \
                     {why}\n{backtrace}"
                );
                Err(format!("the microphone device layer panicked during {operation}: {why}"))
            }
        }
    })
    .await;
    match joined {
        Ok(answered) => answered,
        Err(error) => Err(error.to_string()),
    }
}

/// A panic's own message, or a phrase where it carried none.
fn panic_text(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|said| (*said).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no message".to_owned())
}

std::thread_local! {
    /// The backtrace a panic hook took on this thread.
    static PANIC_SITE: std::cell::RefCell<Option<std::backtrace::Backtrace>> =
        const { std::cell::RefCell::new(None) };
}

/// Install the hook that records a panic's OWN backtrace.
///
/// **Taken at the catch, a backtrace names the catch site, not the raise** -
/// the frames that panicked are already unwound away. The hook runs where the
/// panic is raised, so a guarded thread marks its slot and reads what was
/// recorded, and a hook already installed is chained rather than replaced.
fn record_panic_sites() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANIC_SITE.with(|slot| {
                if let Ok(mut slot) = slot.try_borrow_mut() {
                    *slot = Some(std::backtrace::Backtrace::force_capture());
                }
            });
            previous(info);
        }));
    });
}

/// The backtrace this thread's panic hook took, if it took one.
fn take_panic_site() -> Option<std::backtrace::Backtrace> {
    PANIC_SITE.with(|slot| slot.try_borrow_mut().ok().and_then(|mut slot| slot.take()))
}

/// The caller's give-up signal on a bounded open: the flag it sets when its
/// bound is spent, and the deadline that bound was measured from.
///
/// Both are read because the flag lands a scheduling lag AFTER the deadline:
/// an open that answers inside that lag is one the caller has already given
/// up on, and the flag alone would leave its take recording.
struct GivenUp {
    flag: Arc<AtomicBool>,
    deadline: Instant,
}

impl GivenUp {
    /// Whether the caller has given up: its flag is set, or the deadline it
    /// measured its bound from has passed.
    fn spent(&self) -> bool {
        self.flag.load(Ordering::Acquire) || Instant::now() >= self.deadline
    }
}

/// The open capture's stream side, shared with the callback that fills it.
///
/// Held across takes: the callback is fixed when the device is opened, so
/// what a take changes is this flag, not the stream. The lock is what makes
/// a take's first frame and its last one exact - the callback reads the flag
/// and the buffer under the same lock the take flips them under, so no
/// pre-press sample can land in a take and none of a take's tail can be
/// dropped for a callback that ran late.
struct Carry {
    bridge: Arc<Bridge>,
    /// `true` while a take is recording on this capture; `false` while it is
    /// held between takes, when its frames are dropped rather than sent.
    recording: AtomicBool,
    /// Samples not yet a whole frame, as the callback assembles them - and
    /// when the device last delivered, which is what says whether the stream
    /// is still being served at all.
    held: Mutex<Partial>,
}

/// The samples not yet a whole frame, and when the device last delivered a
/// block: the second is what says whether the stream is still being served.
struct Partial {
    samples: Vec<f32>,
    /// When a block last arrived - set at the open as well, so a capture that
    /// has not delivered yet reads as live until the bound passes.
    at: Instant,
}

impl Partial {
    fn new() -> Self {
        Self { samples: Vec::with_capacity(FRAME_SAMPLES), at: Instant::now() }
    }
}

impl Carry {
    fn new(bridge: &Arc<Bridge>, recording: bool) -> Self {
        Self {
            bridge: Arc::clone(bridge),
            recording: AtomicBool::new(recording),
            held: Mutex::new(Partial::new()),
        }
    }

    /// One block from the device: dropped while no take records, framed and
    /// handed on while one does.
    fn push(&self, samples: &[f32]) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        // Stamped before the drop below: the device serving a held capture
        // between takes is the fact that says it is still serving it.
        held.at = Instant::now();
        if !self.recording.load(Ordering::Relaxed) {
            return;
        }
        held.samples.extend_from_slice(samples);
        while held.samples.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = held.samples.drain(..FRAME_SAMPLES).collect();
            self.bridge.mic_frame(encode_frame(&frame));
        }
    }

    /// Whether the device has delivered a block within `limit`.
    ///
    /// A capture opens once and is held until the screen goes away or the
    /// page hides; a held stream the device has dropped stops delivering and
    /// never comes back, and nothing else about it says so (#2359: Android
    /// disconnects the input when the audio layer under the app changes).
    fn delivering(&self, limit: Duration) -> bool {
        self.held.lock().unwrap_or_else(PoisonError::into_inner).at.elapsed() < limit
    }

    /// Begin a take: the partial frame older than this instant is not the
    /// take's, so it goes rather than leading the take.
    fn begin(&self) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.samples.clear();
        self.recording.store(true, Ordering::Relaxed);
    }

    /// End a take, answering the partial frame it was holding so the caller
    /// can send it. The flag flips under the same lock the callback reads it
    /// under, so nothing lands after this returns.
    fn end(&self) -> Vec<f32> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        self.recording.store(false, Ordering::Relaxed);
        std::mem::take(&mut held.samples)
    }

    /// Whether a take is recording on this capture right now.
    fn recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }
}

/// One open capture, as the hold keeps it: the device it records from, the
/// stream side its frames feed, and how it is let go.
///
/// A trait so the hold's lifecycle - what is opened, what is reused, what is
/// released - can be tested with no audio hardware; the shell has one real
/// implementation, [`Opened`]. `Send` because the hold is Tauri-managed state
/// a command may reach from any thread.
trait Held: Send {
    fn device(&self) -> &forge_capture::Device;
    fn carry(&self) -> &Arc<Carry>;
    fn release(self: Box<Self>);
}

/// A live `forge-capture` stream.
struct Opened {
    capture: forge_capture::Capture,
    carry: Arc<Carry>,
}

impl Held for Opened {
    fn device(&self) -> &forge_capture::Device {
        self.capture.device()
    }

    fn carry(&self) -> &Arc<Carry> {
        &self.carry
    }

    fn release(self: Box<Self>) {
        self.capture.stop();
    }
}

/// Open one capture over `device`, feeding `carry`.
///
/// A boxed closure rather than a plain fn so a test can stand a device in:
/// the lifecycle below is what a hardware-free test needs to reach.
type Opener = Box<dyn Fn(Option<&str>, Arc<Carry>) -> Result<Box<dyn Held>, String> + Send + Sync>;

/// The recording on a capture as the page named it: the page load that began
/// the take, and the token that take minted.
struct Owner {
    page: String,
    take: String,
}

/// The open capture, and who it is for.
struct Warm {
    /// What the page asked for when it was opened, `None` for the system
    /// default; a take asking for the same one is served by this capture.
    requested: Option<String>,
    held: Box<dyn Held>,
    /// The take recording on this capture, `None` while none does.
    ///
    /// The page is what a hold or a press claims against - one naming a
    /// different page is a page other than the one recording, and since the
    /// client is one page, that page is gone. The token is what the take's
    /// own stop and drain match: one page runs take after take, so the page
    /// id cannot tell the take that just ended from the one that followed it.
    owner: Option<Owner>,
}

impl Warm {
    fn recording(&self) -> bool {
        self.held.carry().recording()
    }

    /// Whether `page` is the page whose recording runs on this capture.
    fn owned_by(&self, page: &str) -> bool {
        self.owner.as_ref().is_some_and(|owner| owner.page == page)
    }

    /// Whether `take` is the take recording on this capture.
    fn own_take(&self, take: &str) -> bool {
        self.owner.as_ref().is_some_and(|owner| owner.take == take)
    }

    /// Whether the device behind this capture is still delivering, and so
    /// can serve the take the caller is asking for.
    fn delivering(&self) -> bool {
        self.held.carry().delivering(SILENT_LIMIT)
    }

    /// What a press asking for `device` does with this capture: the warm
    /// take, a fresh open, or a refusal - with the delivery that decided it.
    ///
    /// Only a capture the device is still serving is worth beginning a take
    /// on: one it stopped serving would deliver the take nothing, and the
    /// reader would see a meter that never moves (#2359). The delivery is
    /// read once and handed back, because the capture's callback stamps it
    /// under its own lock: a second read can see a block this one did not,
    /// and the press's line must not contradict the path it reports.
    fn press_path(&self, device: Option<&str>) -> Found {
        let delivering = self.delivering();
        let path = if self.recording() {
            PressPath::Busy
        } else if self.requested.as_deref() == device && delivering {
            PressPath::Warm
        } else {
            PressPath::Cold
        };
        Found { path, delivering }
    }

    /// End a recording `page` does not own, if one is running.
    ///
    /// A hold or a press naming a page other than the one recording means
    /// that page is gone (the client is one page) and its stop went with it,
    /// so the recording can never be closed by the path that closes a live
    /// one - it is ended here, its audio thrown away, and the device stays
    /// held.
    fn claim(&mut self, page: &str) {
        if self.recording() && !self.owned_by(page) {
            self.end();
        }
    }

    /// Begin a take here: `take`, on `page`.
    fn begin(&mut self, page: &str, take: &str) {
        self.held.carry().begin();
        self.owner = Some(Owner { page: page.to_owned(), take: take.to_owned() });
    }

    /// End the recording here, handing over the partial frame it held, and
    /// forget the take that began it: the next take names its own.
    fn end(&mut self) -> Vec<f32> {
        self.owner = None;
        self.held.carry().end()
    }
}

/// What a press does with the hold it finds.
#[derive(Clone, Copy)]
enum PressPath {
    /// The held capture is the one asked for, and the device is feeding it.
    Warm,
    /// The device is opened: nothing is held, another pick is, or the stream
    /// stopped being fed.
    Cold,
    /// Another take is recording, so the press is refused.
    Busy,
}

impl PressPath {
    /// The path's name in the press's own line.
    fn name(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Cold => "cold",
            Self::Busy => "busy",
        }
    }
}

/// The hold a press found and what it does with it: the path, and the delivery
/// that path was chosen with.
struct Found {
    path: PressPath,
    delivering: bool,
}

/// What asking again for a capture that stopped serving does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reopen {
    /// Open the device now: the wait before this attempt has passed.
    Now,
    /// Not yet: the wait before this attempt has not passed.
    Wait,
    /// The budget is spent; nothing opens until a real trigger.
    GivenUp,
}

/// What holds the microphone back, and what a capture that stopped serving
/// has already spent trying to come back.
///
/// **The shell is the judge of the screen**, because the page cannot see a
/// locked one and a locked screen delivers no audio: a hold kept through it
/// is one every later staleness check reopens. The page reports its own
/// visibility, which the shell cannot see either.
#[derive(Default)]
struct Restraint {
    /// The screen is locked or asleep, as the platform reports it.
    screen: bool,
    /// The page says it is hidden.
    hidden: bool,
    /// The device the last hold or press named, so another pick is told from
    /// the same one asking again; and whether one has been named at all,
    /// which `None` alone cannot say - that is the system default.
    asked: Option<String>,
    stated: bool,
    /// Reopens made since the last real trigger.
    attempts: usize,
    /// When the last reopen was made.
    at: Option<Instant>,
    /// Set once the budget is spent: nothing opens until a real trigger.
    spent: bool,
    /// Whether the give-up has been said, so it is said once rather than on
    /// every hold that arrives after it.
    announced: bool,
    /// The waits before each retry: only a test carries its own, so a spacing
    /// test needs no real sleeping; the shipped build reads [`REOPEN_WAITS`].
    #[cfg(test)]
    waits: [Duration; REOPEN_ATTEMPTS],
}

impl Restraint {
    /// Whether the hold is suspended: the screen is away or the page hidden.
    fn away(&self) -> bool {
        self.screen || self.hidden
    }

    /// State the device a hold or a press asks for, answering whether it is
    /// another one - which is a real trigger, so the budget starts over.
    fn ask(&mut self, device: Option<&str>) -> bool {
        let asked = device.map(str::to_owned);
        if self.stated && self.asked == asked {
            return false;
        }
        self.asked = asked;
        self.stated = true;
        self.fresh();
        true
    }

    /// Start the budget over: a real trigger has arrived.
    fn fresh(&mut self) {
        self.attempts = 0;
        self.at = None;
        self.spent = false;
        self.announced = false;
    }

    /// What a retry's next request does. A spent budget stays spent until
    /// [`Restraint::fresh`].
    fn reopen(&mut self, now: Instant) -> Reopen {
        if self.spent || self.attempts >= REOPEN_ATTEMPTS {
            self.spent = true;
            return Reopen::GivenUp;
        }
        #[cfg(test)]
        let waits = &self.waits;
        #[cfg(not(test))]
        let waits = &REOPEN_WAITS;
        if let Some(at) = self.at
            && now.duration_since(at) < waits[self.attempts]
        {
            return Reopen::Wait;
        }
        Reopen::Now
    }

    /// Record an attempt the caller is making: a retry of a capture that
    /// stopped serving, or an open that failed. The last one spends the
    /// budget - a device that is gone must not be tried once per hold call.
    fn attempted(&mut self, now: Instant) {
        self.attempts += 1;
        self.at = Some(now);
        if self.attempts >= REOPEN_ATTEMPTS {
            self.spent = true;
        }
    }

    /// Say the budget is spent, once.
    fn announce_give_up(&mut self, device: Option<&str>) {
        if !self.announced {
            self.announced = true;
            log_reopen(device, self.attempts, true);
        }
    }
}

/// Say a reopen the hold made, or the budget it has spent - a capture that
/// stopped serving and a device that will not open are the same budget.
fn log_reopen(device: Option<&str>, attempts: usize, spent: bool) {
    tauri_plugin_log::log::debug!(
        target: LOG_TARGET,
        "the microphone hold {} the device (event_name {}, requested {device:?}, attempts \
         {attempts}, budget {REOPEN_ATTEMPTS})",
        if spent { "gives up retrying" } else { "retries" },
        if spent { "client_mic_reopen_given_up" } else { "client_mic_reopen" },
    );
}

/// Say the hold a press found and the path it took.
///
/// A press that goes cold is answerable from here: the hold's state says why
/// it was not serving the take, so a slow open is explained by the line that
/// opened it. It is `debug` - one session's own work, not forge's (rule 20) -
/// and [`crate::DEBUG_TARGETS`] is what keeps it in `forge.log`.
fn log_press(
    session: Option<&SessionSlot>,
    device: Option<&str>,
    hold: Option<&Warm>,
    found: &Found,
) {
    let session = session.map_or_else(|| "none".to_owned(), SessionSlot::display);
    tauri_plugin_log::log::debug!(
        target: LOG_TARGET,
        "a press asks for the microphone (event_name client_mic_press, session {session}, \
         requested {device:?}, held {held:?}, held_for {held_for:?}, delivering {}, path {})",
        found.delivering,
        found.path.name(),
        held = hold.map(|hold| hold.held.device().id.as_str()),
        held_for = hold.and_then(|hold| hold.requested.as_deref()),
    );
}

/// The microphone, as this half holds it: at most one capture, open from the
/// page's first hold to its release, serving every take between - and let go
/// while the screen is away or the page hidden.
pub struct Mic {
    warm: Mutex<Option<Warm>>,
    /// The screen, the page, and the reopen budget the hold keeps.
    restraint: Mutex<Restraint>,
    /// The screen's away reading as a press that is already opening sees it.
    ///
    /// A transition lands its own flag BEFORE it takes the restraint lock,
    /// because an open in flight holds that lock: the transition then queues
    /// behind the open, and the press re-reads both flags on its way out -
    /// so a capture that came up behind a lock is ended rather than left
    /// recording. One flag per input, so neither setter can clear a signal
    /// the other has already raised.
    screen_away: AtomicBool,
    /// The page's hidden reading, landed the same way as [`Mic::screen_away`].
    hidden_away: AtomicBool,
    /// How a capture is opened; the shell's own value is `forge-capture`.
    opener: Opener,
}

impl Default for Mic {
    fn default() -> Self {
        Self {
            warm: Mutex::new(None),
            screen_away: AtomicBool::new(false),
            hidden_away: AtomicBool::new(false),
            restraint: Mutex::new(Restraint::default()),
            opener: Box::new(|device, carry| {
                let seen = Arc::clone(&carry);
                let capture = forge_capture::open(device, move |block| seen.push(block.samples))
                    .map_err(|error| error.to_string())?;
                Ok(Box::new(Opened { capture, carry }))
            }),
        }
    }
}

impl Mic {
    /// Open the picked device - or the system default when nothing is
    /// picked - and keep it open until [`Mic::release`], the screen goes
    /// away, or the page hides.
    ///
    /// A pick it is already holding AND still delivering to is answered
    /// without a second open; a pick whose capture the device has stopped
    /// serving is opened again within the reopen budget, and an open that
    /// fails answers that error with the old capture left standing. A no-op
    /// for the page recording on it: the pick applies from the next take, so
    /// a change mid take never tears the stream out from under the recording.
    ///
    /// **A hold behind a locked screen or a hidden page answers no capture**
    /// ([`None`]): the hold does not exist there, and a device opened for one
    /// would be the reopen loop that fed the menu-bar leak. The page takes
    /// the hold up again when the screen is back.
    ///
    /// **A hold is also a claim on the device**, which is how the page that
    /// comes back from a reload catches the take its predecessor left
    /// recording: no live page owns that recording, so it is ended rather
    /// than kept (see `Warm::claim`), and this page's press is warm and free.
    ///
    /// A pick that will not open is an error rather than a fallback: someone
    /// who chose an interface needs to know it is not there, not to be
    /// quietly recorded on something else. The caller carries on without a
    /// hold - a press then opens the device itself, exactly as it did before
    /// there was one.
    pub fn hold(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
    ) -> Result<Option<forge_capture::Device>, String> {
        let mut restraint = self.restraint.lock().unwrap_or_else(PoisonError::into_inner);
        if restraint.away() {
            // The transition let the capture go already; a hold arriving after
            // it is answered with nothing rather than opening the device under
            // a screen that cannot feed a take.
            self.let_go("the screen or page is away");
            return Ok(None);
        }
        let pick_moved = restraint.ask(device.as_deref());
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = warm.as_mut() {
            held.claim(page);
            // A recording take keeps its stream whatever the pick; a capture
            // the device stopped serving is not kept at all, so a hold for it
            // opens the device again rather than answering with a stream that
            // delivers nothing (#2359). A capture the device IS serving is the
            // proof that whatever reopens it cost are history.
            if held.recording() || (held.requested == device && held.delivering()) {
                restraint.fresh();
                return Ok(Some(held.held.device().clone()));
            }
        }
        // **A retry is the budget's to decide; a fresh open is not.** The
        // first hold after a release (a lock, a hidden page, a pick that
        // moved) is a real trigger and opens at once. So is the first hold
        // ever. Everything after an attempt that failed or went stale waits
        // its interval, and a spent budget opens nothing until a real trigger.
        let retrying = warm.is_some() || restraint.attempts > 0;
        if retrying {
            match restraint.reopen(Instant::now()) {
                Reopen::Now => {
                    if !pick_moved {
                        log_reopen(device.as_deref(), restraint.attempts + 1, false);
                    }
                }
                Reopen::Wait => return Ok(None),
                Reopen::GivenUp => {
                    restraint.announce_give_up(device.as_deref());
                    // **Only the capture that stopped serving is let go.** A
                    // newly picked device that will not open spends the same
                    // budget while a healthy hold for the previous pick still
                    // stands, and three failures of the new one must not tear
                    // that down.
                    let dead = warm.as_ref().is_some_and(|held| {
                        !held.recording()
                            && !held.delivering()
                            && held.requested.as_deref() == device.as_deref()
                    });
                    if dead && let Some(stale) = warm.take() {
                        stale.held.release();
                    }
                    return Ok(None);
                }
            }
        }
        // Opened before the old one is let go, so a pick that will not open
        // leaves the hold that was working standing.
        // **An attempt is counted once it was made**: a retry, or a fresh
        // open that failed. A fresh open that succeeds is the state, not a
        // retry, so the first staleness after it is taken at once.
        let next = match self.open(bridge, device.as_deref(), None) {
            Ok(next) => {
                if retrying {
                    restraint.attempted(Instant::now());
                }
                next
            }
            Err(why) => {
                restraint.attempted(Instant::now());
                if restraint.spent {
                    restraint.announce_give_up(device.as_deref());
                }
                return Err(why);
            }
        };
        let resolved = next.held.device().clone();
        if let Some(stale) = warm.replace(next) {
            stale.held.release();
        }
        Ok(Some(resolved))
    }

    /// Begin a take: the held capture when it is the one asked for, a fresh
    /// open otherwise.
    ///
    /// The device is opened before this returns, so a device that will not
    /// open is an error here rather than a take that records silence. The
    /// input it opened comes back with it, which is the only place a plain
    /// take names the system default: the picker's list carries no mark.
    ///
    /// **The take consumes nothing**: the held capture serves it and stays
    /// held, so the next take is warm too. A capture opened for this take
    /// alone becomes the hold, so the pick self-corrects after a press the
    /// hold could not serve.
    ///
    /// A take this same page is already recording is refused, which is the
    /// busy guard. A press claims the device the way a hold does, so a
    /// recording left by any other page is ended first (see `Warm::claim`):
    /// the first press after a reload records, rather than being denied by
    /// the take the reload discarded.
    ///
    /// `take` is the token the page minted for this take, and the take's own
    /// identity for the stop and drain that end it - which a page id cannot
    /// be, because one page runs take after take.
    pub fn start(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
        take: &str,
    ) -> Result<forge_capture::Device, String> {
        self.start_inner(bridge, device, page, take, None)
    }

    /// [`Mic::start`], with `abandoned` read as the LAST act - once the take is
    /// already the caller's - so a give-up landing anywhere before that read,
    /// the release's own window included, ends the take in place: the capture
    /// is kept as the hold - the device is up, and the next press is warm on
    /// it - and no take nobody owns is left recording.
    fn start_inner(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
        take: &str,
        abandoned: Option<&GivenUp>,
    ) -> Result<forge_capture::Device, String> {
        // **A press races the screen going away, and loses.** The lock can
        // land between the key and this call, or while the device is opening,
        // and a device opened there is a microphone behind a locked screen -
        // the reopen the hold rule exists to stop. The first check is the
        // state this call starts in; a transition that lands while it holds
        // the guard below lands its own signal first (see [`Mic::screen_away`])
        // and queues behind it, and the checks after the begin and the open
        // read that signal. The budget is left as it was on the away path: the
        // away state is not a real trigger, and the reader's line says why
        // nothing opened.
        let mut restraint = self.restraint.lock().unwrap_or_else(PoisonError::into_inner);
        if restraint.away() {
            return Err(AWAY.to_owned());
        }
        // **A press is a real trigger**: whatever the reopen budget had spent,
        // the reader asking again is worth a fresh try, and a different pick
        // starts the budget over on its own.
        restraint.ask(device.as_deref());
        restraint.fresh();
        // Read before the hold's lock: the on-screen seat is the bridge's
        // state, and the two locks never nest.
        let session = bridge.shown();
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        let found = match warm.as_mut() {
            Some(held) => {
                held.claim(page);
                held.press_path(device.as_deref())
            }
            None => Found { path: PressPath::Cold, delivering: false },
        };
        log_press(session.as_ref(), device.as_deref(), warm.as_ref(), &found);
        let mut crossed = false;
        if let Some(held) = warm.as_mut() {
            match found.path {
                PressPath::Busy => {
                    return Err("the microphone is already recording".to_owned());
                }
                PressPath::Warm => {
                    held.begin(page, take);
                    // **The read comes after the begin, as the last act**: a
                    // warm capture answers at once, so a give-up only lands
                    // inside this call when the lock waited - a wedged open
                    // holding it - and the take begun above is one the caller
                    // gave up on. Ended in place, the capture stays the hold.
                    if abandoned.is_some_and(GivenUp::spent) {
                        held.end();
                        return Err(NOT_COME_UP.to_owned());
                    }
                    // **The lock can land while this call holds the guard**:
                    // the transition queues behind it and its signal is what
                    // this reads. A warm press that crossed it ends its take
                    // and lets the capture go below.
                    crossed = self.away_now();
                    if !crossed {
                        return Ok(held.held.device().clone());
                    }
                    held.end();
                }
                PressPath::Cold => {}
            }
        }
        if crossed {
            if let Some(stale) = warm.take() {
                stale.held.release();
            }
            return Err(AWAY.to_owned());
        }
        let next = self.open(bridge, device.as_deref(), Some((page, take)))?;
        let resolved = next.held.device().clone();
        if let Some(stale) = warm.replace(next) {
            stale.held.release();
        }
        // **The lock can land while the open blocks**: its signal lands before
        // the transition queues behind this guard, so this read is what sees
        // it. A capture that came up behind the lock is ended and let go, and
        // the press answers the away line rather than recording behind a
        // locked screen.
        if self.away_now() {
            if let Some(held) = warm.as_mut() {
                held.end();
            }
            if let Some(stale) = warm.take() {
                stale.held.release();
            }
            return Err(AWAY.to_owned());
        }
        // **And here the read comes last, after the release**: letting the
        // replaced capture go takes the capture thread's own window (up to
        // its 20 ms loop), and a give-up landing inside it must still leave no
        // take recording. Ended in place, the answered capture stays the hold.
        if abandoned.is_some_and(GivenUp::spent) {
            if let Some(held) = warm.as_mut() {
                held.end();
            }
            return Err(NOT_COME_UP.to_owned());
        }
        Ok(resolved)
    }

    /// [`Mic::start`] under a bound: an open that has not answered within
    /// `limit` is reported as the microphone not coming up rather than waited
    /// on, which is the line the page draws for a device layer that is wedged
    /// - a permission whose prompt never surfaced, a device another process
    ///   holds.
    ///
    /// The open is left to answer on its own thread, and a late success ends
    /// the take nobody owns rather than leaving it recording.
    pub async fn start_within(
        self: &Arc<Self>,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
        take: &str,
        limit: Duration,
    ) -> Result<forge_capture::Device, String> {
        // The deadline is fixed here, before the open is spawned, because the
        // opener compares it: the flag below only lands when this task wakes,
        // and a device answering inside that lag must read as given up too.
        let signal =
            GivenUp { flag: Arc::new(AtomicBool::new(false)), deadline: Instant::now() + limit };
        let flag = Arc::clone(&signal.flag);
        let mic = Arc::clone(self);
        let bridge = Arc::clone(bridge);
        let (page, take) = (page.to_owned(), take.to_owned());
        let logged = device.clone();
        let started = guarded("the take's open", logged, move || {
            mic.start_inner(&bridge, device, &page, &take, Some(&signal))
        });
        if let Ok(joined) = tokio::time::timeout(limit, started).await {
            joined?
        } else {
            flag.store(true, Ordering::Release);
            Err(NOT_COME_UP.to_owned())
        }
    }

    /// End the take at once, throwing its audio away. The device stays
    /// open, so the next press records from the press.
    ///
    /// **One naming a take other than the recording does nothing** (#2252,
    /// #2279): a stop in flight lands after the take it names has ended -
    /// the page's own re-press may already have begun the next, and a
    /// reload's discarded page leaves its take for another page to end -
    /// and the take that followed is not this one's to throw away.
    pub fn stop(&self, take: &str) {
        {
            let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(held) = warm.as_mut() else {
                return;
            };
            if !held.own_take(take) {
                return;
            }
            held.end();
        }
        // A take that outlived an away screen ends the hold it kept alive.
        self.release_if_away();
    }

    /// Hold the capture open for the drain window, hand over the last
    /// partial frame, then end the take and keep the device warm.
    ///
    /// The caller sends its stop only after this returns: the stop is what
    /// finalises the take on the server, so a frame that follows it has no
    /// take to land in.
    ///
    /// **One naming a take other than the recording drops its tail without
    /// ending anything** (#2252, #2279): the window it sleeps is wide enough
    /// for that take to end - by a drop, or the server's own `dictate_ended` -
    /// and for the page to begin its next take, and the take that followed
    /// is not this one's to throw away.
    pub async fn drain(&self, take: &str) {
        tokio::time::sleep(Duration::from_millis(DRAIN_MS)).await;
        {
            let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(held) = warm.as_mut() else {
                return;
            };
            if !held.own_take(take) {
                return;
            }
            let tail = held.end();
            if !tail.is_empty() {
                held.held.carry().bridge.mic_frame(encode_frame(&tail));
            }
        }
        // The take's own end is what lets the device go behind an away screen.
        self.release_if_away();
    }

    /// The screen is away (locked, or the display asleep), or is back.
    ///
    /// **`true` lets the held capture go.** A locked or sleeping screen
    /// delivers no audio, so a capture kept through it serves no take and is
    /// exactly the one every later staleness check reopens - the loop that
    /// rebuilt the menu bar every few seconds. **`false` starts the reopen
    /// budget over**; the page takes the hold up again from its own pick,
    /// because it is the side that knows whether a surface still offers
    /// dictation.
    #[cfg(target_os = "macos")]
    pub fn set_screen_away(&self, away: bool) {
        // **Landed before the lock**: an open in flight holds it, and the
        // press re-reads this on its way out (see [`Mic::screen_away`]). The
        // setter's own flag only, so the page's signal is never cleared here.
        if away {
            self.screen_away.store(true, Ordering::Release);
        }
        let away = {
            let mut restraint = self.restraint.lock().unwrap_or_else(PoisonError::into_inner);
            let was = restraint.away();
            restraint.screen = away;
            let now = restraint.away();
            self.screen_away.store(away, Ordering::Release);
            if was == now {
                return;
            }
            if !now {
                restraint.fresh();
            }
            now
        };
        if away {
            self.let_go("the screen is away");
        }
    }

    /// The page says it is hidden, or is back.
    ///
    /// The same rule as the screen's: the hold does not exist behind a
    /// hidden window - a minimized or occluded page feeds no take - and the
    /// page takes it up again when it is back on screen.
    pub fn set_hidden(&self, hidden: bool) {
        // The same early landing as the screen's, on its own flag.
        if hidden {
            self.hidden_away.store(true, Ordering::Release);
        }
        let away = {
            let mut restraint = self.restraint.lock().unwrap_or_else(PoisonError::into_inner);
            let was = restraint.away();
            restraint.hidden = hidden;
            let now = restraint.away();
            self.hidden_away.store(hidden, Ordering::Release);
            if was == now {
                return;
            }
            if !now {
                restraint.fresh();
            }
            now
        };
        if away {
            self.let_go("the page is hidden");
        }
    }

    /// The away reading a press re-checks on its way out, as the transitions
    /// landed it before they queued behind the press's guard.
    fn away_now(&self) -> bool {
        self.screen_away.load(Ordering::Acquire) || self.hidden_away.load(Ordering::Acquire)
    }

    /// Let the held capture go, unless a take is recording on it.
    ///
    /// **The away rule is about the hold, not a take.** A minimized window
    /// with the reader still present, or a lock landing mid-dictation, must
    /// not throw away words already spoken: the capture serves the take until
    /// the page ends it, and [`Mic::release_if_away`] lets the device go then.
    fn let_go(&self, why: &'static str) {
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(held) = warm.as_ref() else {
            return;
        };
        if held.recording() {
            tauri_plugin_log::log::debug!(
                target: LOG_TARGET,
                "the microphone hold is kept for a take in flight (event_name \
                 client_mic_hold_kept, reason {why})"
            );
            return;
        }
        let Some(mut held) = warm.take() else {
            return;
        };
        held.end();
        held.held.release();
        tauri_plugin_log::log::debug!(
            target: LOG_TARGET,
            "the microphone hold was let go (event_name client_mic_hold_released, reason {why})"
        );
    }

    /// Let the capture go when a take that outlived an away screen ends.
    ///
    /// The hold does not exist behind a locked screen or a hidden page; a
    /// take that was already recording is the one exception, and it takes the
    /// exception with it when it ends.
    fn release_if_away(&self) {
        let away = self.restraint.lock().unwrap_or_else(PoisonError::into_inner).away();
        if !away {
            return;
        }
        let stale = self.warm.lock().unwrap_or_else(PoisonError::into_inner).take();
        let Some(mut held) = stale else {
            return;
        };
        held.end();
        held.held.release();
        tauri_plugin_log::log::debug!(
            target: LOG_TARGET,
            "the microphone hold was let go with the take it was kept for (event_name \
             client_mic_hold_released, reason the take outlived an away screen)"
        );
    }

    /// Let the device go: the client is exiting, and the microphone's
    /// indicator goes out with it.
    pub fn release(&self) {
        let stale = self.warm.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(held) = stale {
            held.held.release();
        }
    }

    /// Open one capture; a take's while `owner` names the page and the take
    /// recording on it, a hold's while it is `None`.
    fn open(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<&str>,
        owner: Option<(&str, &str)>,
    ) -> Result<Warm, String> {
        let carry = Arc::new(Carry::new(bridge, owner.is_some()));
        let held = (self.opener)(device, Arc::clone(&carry))?;
        Ok(Warm {
            requested: device.map(str::to_owned),
            held,
            owner: owner.map(|(page, take)| Owner { page: page.to_owned(), take: take.to_owned() }),
        })
    }
}

/// One frame as the wire carries it: the codec tag, then little-endian i16
/// at 2^15, clamped first.
///
/// The same bytes `client/src/composer/capture.svelte.ts` sends from the
/// browser's microphone, and the server divides by the same 2^15; the two
/// encoders are pinned to one fixture on both sides.
pub fn encode_frame(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(1 + samples.len() * 2);
    bytes.push(0); // codec: pcm i16
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * 32768.0).round().clamp(-32768.0, 32767.0);
        let value = value as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// One device as the page reads it: the panel's own `{ id, label }` shape,
/// for both the take's `resolved` (`ShellMic.open`) and the picker's rows.
pub fn device_json(device: &forge_capture::Device) -> serde_json::Value {
    serde_json::json!({ "id": device.id, "label": device.name })
}

/// What a platform reports about this app's microphone, when it reports
/// anything: the shape the page's `client_mic_state` answer carries, on every
/// platform - the desktop answers `None` and the row's line draws without a
/// cause.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MicState {
    /// The device's microphone mute.
    pub muted: bool,
    /// Whether the platform is silencing THIS app's capture - a per-app mic
    /// mode, the privacy toggle.
    pub silenced: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A panicking device layer answers rather than aborting.** A Rust
    /// unwind escaping the blocking task reaches a foreign frame and becomes
    /// `rust_foreign_exception`/SIGABRT, which killed a boot (#2426); the
    /// guard turns it into the named error the command returns, carrying the
    /// panic's own words, and leaves a plain return untouched.
    #[test]
    fn a_panicking_device_call_answers_a_named_error() {
        // **The line the issue asked for, level and content.** A named error
        // alone is not enough: the WARN is what survives the launch and names
        // the panic, so its level, operation, device and backtrace are pinned.
        let kept = crate::log_capture::capture();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let err = runtime
            .block_on(guarded(
                "the take's open",
                Some("coreaudio:builtin".to_owned()),
                || -> Result<(), String> { panic!("a wedged audio layer") },
            ))
            .expect_err("a panic is answered, not propagated");
        assert!(
            err.starts_with("the microphone device layer panicked during the take's open"),
            "the answer names what panicked and where: {err}"
        );
        assert!(
            err.contains("a wedged audio layer"),
            "the panic's own words ride the answer: {err}"
        );

        let warned = crate::log_capture::records()
            .into_iter()
            .find(|line| line.message.contains("the microphone device layer panicked"))
            .expect("the panic leaves a line the shipped filter keeps");
        assert_eq!(
            warned.level,
            tauri_plugin_log::log::Level::Warn,
            "the line is a WARN, which the baseline filter keeps: {:?}",
            warned.level,
        );
        assert!(
            warned.message.contains("during the take's open")
                && warned.message.contains("coreaudio:builtin"),
            "the line names the operation and the device a reader acts on: {}",
            warned.message,
        );
        assert!(
            warned.message.lines().any(|line| line.trim_start().starts_with("0: ")),
            "the line carries the backtrace taken at the raise, not just the message: {}",
            warned.message,
        );
        drop(kept);

        let value = runtime
            .block_on(guarded("the take's open", None, || 7))
            .expect("a plain answer is untouched");
        assert_eq!(value, 7, "the guard changes nothing about what the work returned");
    }

    /// The keys the page reads the default's name back by
    /// (`dictation.ts`'s `rememberDefaultDevice`): `ShellMic.open` hands this
    /// straight to the take, so a renamed key is a panel that names nothing,
    /// and no page test sees it.
    #[test]
    fn the_device_crosses_as_the_panels_id_and_label() {
        let opened = forge_capture::Device {
            id: "coreaudio:builtin".to_owned(),
            name: "MacBook Pro Microphone".to_owned(),
            is_default: true,
        };
        assert_eq!(
            device_json(&opened),
            serde_json::json!({ "id": "coreaudio:builtin", "label": "MacBook Pro Microphone" }),
            "the page reads `id` and `label`; any other key is a default it cannot name"
        );
    }

    /// The vector `capture.test.ts` pins the page's encoder to: the two
    /// halves of one wire contract, so a change on either side fails here.
    #[test]
    fn the_frame_is_the_codec_tag_then_little_endian_i16() {
        assert_eq!(
            encode_frame(&[0.0, 0.5, -1.0, 0.25]),
            vec![0, 0x00, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x20],
            "the tag, then 0, 16384, -32768 and 8192, low byte first"
        );
    }

    #[test]
    fn what_the_microphone_cannot_have_meant_is_clamped() {
        // Full scale lands on the largest positive sample, not 32768, which
        // does not fit; an overshoot reads as full scale rather than
        // wrapping its sign.
        assert_eq!(&encode_frame(&[1.0])[1..], &[0xff, 0x7f]);
        assert_eq!(&encode_frame(&[2.0])[1..], &[0xff, 0x7f]);
        assert_eq!(&encode_frame(&[-2.0])[1..], &[0x00, 0x80]);
    }

    mod the_hold {
        use super::*;

        /// A stand-in capture: it records that it was opened, what for, and
        /// whether it was released - which is the whole of the hold's
        /// lifecycle, with no audio hardware in the test run.
        struct Fake {
            device: forge_capture::Device,
            carry: Arc<Carry>,
            log: Arc<Mutex<Vec<String>>>,
        }

        impl Held for Fake {
            fn device(&self) -> &forge_capture::Device {
                &self.device
            }

            fn carry(&self) -> &Arc<Carry> {
                &self.carry
            }

            fn release(self: Box<Self>) {
                self.log.lock().unwrap().push(format!("release {}", self.device.id));
            }
        }

        /// The shell half over stand-in devices: the log is the whole of
        /// what the hold did - what it opened and what it let go.
        struct Rig {
            mic: Mic,
            log: Arc<Mutex<Vec<String>>>,
            bridge: Arc<Bridge>,
            /// What the take sent, where a test that watches the wire reads it.
            frames: tokio::sync::mpsc::UnboundedReceiver<crate::bridge::ClientEvent>,
            /// Per open, whether that open's own press line was already in
            /// the log: the value of the line is that it exists WHILE the
            /// device is being opened, not only once it answers.
            opening: Arc<Mutex<Vec<bool>>>,
        }

        impl Rig {
            fn new() -> Self {
                let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let opened = Arc::clone(&log);
                let opening: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
                let watching = Arc::clone(&opening);
                let mic = Mic {
                    warm: Mutex::new(None),
                    screen_away: AtomicBool::new(false),
                    hidden_away: AtomicBool::new(false),
                    // Every reopen is taken at once here: the waits are the
                    // budget's own business, and this rig's tests are about
                    // what each attempt did, not when.
                    restraint: Mutex::new(Restraint {
                        waits: [Duration::ZERO; REOPEN_ATTEMPTS],
                        ..Restraint::default()
                    }),
                    opener: Box::new(move |device, carry| {
                        let name = device.unwrap_or("default").to_owned();
                        // The press's line names the device it asked for; one
                        // this test asks for only once is what makes this look
                        // that press's own - a repeated device can be
                        // answered by an earlier press's line.
                        let asked = format!("requested Some(\"{name}\")");
                        let seen = crate::log_capture::records().iter().any(|line| {
                            line.message.contains("client_mic_press")
                                && line.message.contains(&asked)
                        });
                        watching.lock().unwrap().push(seen);
                        opened.lock().unwrap().push(format!("open {name}"));
                        Ok(Box::new(Fake {
                            device: forge_capture::Device {
                                id: name,
                                name: "stand-in".to_owned(),
                                is_default: device.is_none(),
                            },
                            carry,
                            log: Arc::clone(&opened),
                        }))
                    }),
                };
                let (events, frames) = tokio::sync::mpsc::unbounded_channel();
                let bridge = Bridge::new(None, events);
                Self { mic, log, bridge, frames, opening }
            }

            fn calls(&self) -> Vec<String> {
                self.log.lock().unwrap().clone()
            }

            /// Whether each open saw its own press line already logged.
            fn opening(&self) -> Vec<bool> {
                self.opening.lock().unwrap().clone()
            }
        }

        #[test]
        fn a_hold_opens_the_device_and_keeps_it() {
            let rig = Rig::new();
            let opened = rig
                .mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            assert_eq!(opened.id, "one");
            assert_eq!(rig.calls(), vec!["open one"], "the hold is the open, and nothing else");

            // A second hold for the same pick is the same capture, not a
            // second open: the page re-states the pick it is holding.
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("idempotent")
                .expect("held");
            assert_eq!(rig.calls(), vec!["open one"], "the same pick does not reopen");
        }

        #[test]
        fn a_take_consumes_the_held_capture_without_reopening() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            let started = rig
                .mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take-1")
                .expect("starts");
            assert_eq!(started.id, "one");
            assert_eq!(rig.calls(), vec!["open one"], "the press opens nothing");

            // And ending the take does not close it either: the same device
            // serves the next take.
            rig.mic.stop("take-1");
            assert_eq!(rig.calls(), vec!["open one"], "the take's end keeps the device open");
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take-2")
                .expect("the next take is warm");
            assert_eq!(rig.calls(), vec!["open one"], "the second press opens nothing either");
        }

        /// One block from the device, as its own callback delivers them.
        fn deliver(rig: &Rig, samples: &[f32]) {
            let warm = rig.mic.warm.lock().unwrap();
            let held = warm.as_ref().expect("a held capture");
            held.held.carry().push(samples);
        }

        /// The held capture, as one whose device has stopped: its last block
        /// stamped well past the bound that calls it dead.
        fn staled(rig: &Rig) {
            let warm = rig.mic.warm.lock().unwrap();
            let held = warm.as_ref().expect("a held capture");
            held.held.carry().held.lock().unwrap().at = Instant::now()
                .checked_sub(SILENT_LIMIT * 4)
                .expect("the clock has run past four silent limits");
        }

        #[test]
        fn a_capture_that_starts_delivering_again_is_kept() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            // Stale FIRST, then a block: what keeps the capture is the
            // delivery, not the open - a fresh open stamps itself live and
            // would pass the bound with no block at all.
            staled(&rig);
            deliver(&rig, &[0.0; 8]);
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take")
                .expect("the warm press");
            assert_eq!(
                rig.calls(),
                vec!["open one"],
                "a capture the device is delivering to is the warm one the press was promised"
            );
        }

        #[test]
        fn a_press_on_a_capture_that_stopped_delivering_opens_the_device_again() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            // The device goes quiet while the capture is held: the audio
            // layer under the app changed and the stream disconnected, and a
            // disconnected stream never delivers again (#2359).
            staled(&rig);
            let started = rig
                .mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take")
                .expect("the press opens the device again");
            assert_eq!(started.id, "one", "the same input, freshly opened");
            assert_eq!(
                rig.calls(),
                vec!["open one", "open one", "release one"],
                "a press on a capture the device stopped serving opens it again"
            );
        }

        /// **Every press says the hold it found and the path it took, before
        /// the open runs.** A press that went cold used to leave no line at
        /// all, so the seconds its open took had nothing to explain them.
        #[test]
        fn a_press_says_the_hold_it_found_and_the_path_it_took() {
            let _kept = crate::log_capture::capture();
            let rig = Rig::new();
            // The seat is this test's own - no other test puts one on screen -
            // so its lines are its own even under an in-process `cargo test`,
            // where every captured line lands in one shared buffer.
            rig.bridge.showing(Some(serde_json::json!({
                "org": "Busytools",
                "project": "forge",
                "label": "lead",
            })));

            // Nothing held: the device is opened, and the line has to be there
            // while that open runs - the whole point of it.
            rig.mic
                .start(&rig.bridge, Some("held-first".to_owned()), "page", "take-1")
                .expect("a press with no hold opens the device");
            assert_eq!(rig.calls(), vec!["open held-first"], "the cold press opens the device");

            // Warm from a block the device delivers, not from the open's own
            // stamp: stale first, then a block.
            rig.mic.stop("take-1");
            staled(&rig);
            deliver(&rig, &[0.0; 8]);
            rig.mic
                .start(&rig.bridge, Some("held-first".to_owned()), "page", "take-2")
                .expect("the held capture serves the press");
            assert_eq!(rig.calls().len(), 1, "the warm press opens nothing");

            // A different pick on a capture that stopped delivering: the
            // device is opened for the pick asked for, and the hold it
            // replaces is let go.
            rig.mic.stop("take-2");
            staled(&rig);
            rig.mic
                .start(&rig.bridge, Some("asked-second".to_owned()), "page", "take-3")
                .expect("the press the hold cannot serve opens its own");
            assert_eq!(
                rig.calls(),
                vec!["open held-first", "open asked-second", "release held-first"],
                "the cold press re-acquires the device",
            );

            // Another take is recording, so the press is refused by name.
            let refused =
                rig.mic.start(&rig.bridge, Some("asked-third".to_owned()), "page", "take-4");
            assert_eq!(
                refused.err().as_deref(),
                Some("the microphone is already recording"),
                "the busy press is refused rather than served",
            );
            assert_eq!(rig.calls().len(), 3, "the busy press opens nothing");

            // A reloaded page presses while the discarded page's take still
            // runs: the claim ends it, so the line reads warm, not busy.
            deliver(&rig, &[0.0; 8]);
            rig.mic
                .start(&rig.bridge, Some("asked-second".to_owned()), "page-2", "take-5")
                .expect("the press after a reload records");
            assert_eq!(rig.calls().len(), 3, "the reload's press opens nothing");

            // The same pick, on a capture that stopped delivering: the device
            // is opened again, which is the delivery alone deciding the path.
            rig.mic.stop("take-5");
            staled(&rig);
            rig.mic
                .start(&rig.bridge, Some("asked-second".to_owned()), "page", "take-6")
                .expect("a stale capture the press would have been warm on is reopened");
            assert_eq!(
                rig.calls().len(),
                5,
                "the same pick opener is an open and a release of the capture it replaced",
            );

            // The pick moves to the system default, which resolves to a
            // device: `held` is that device while `held_for` is the no-pick.
            rig.mic.stop("take-6");
            rig.mic.hold(&rig.bridge, None, "page").expect("the default is held").expect("held");
            deliver(&rig, &[0.0; 8]);
            rig.mic
                .start(&rig.bridge, None, "page", "take-7")
                .expect("the default serves the press");
            assert_eq!(
                rig.calls(),
                vec![
                    "open held-first",
                    "open asked-second",
                    "release held-first",
                    "open asked-second",
                    "release asked-second",
                    "open default",
                    "release asked-second",
                ],
                "the hold for the default re-acquires it, and the press opens nothing",
            );
            assert_eq!(
                rig.opening(),
                vec![true, true, true, false],
                "the presses asking for a device once see their own line already logged at open; \
                 the same-pick press's look is answered by the earlier press that asked for it; \
                 the hold's own open has no press line at all",
            );

            let presses: Vec<crate::log_capture::Line> = crate::log_capture::records()
                .into_iter()
                .filter(|line| {
                    line.message.contains("client_mic_press")
                        && line.message.contains("session Busytools/forge/lead")
                })
                .collect();
            assert_eq!(
                presses.len(),
                7,
                "this test's every press is logged: {:?}",
                presses.iter().map(|line| &line.message).collect::<Vec<_>>(),
            );
            for line in &presses {
                assert_eq!(
                    line.level,
                    tauri_plugin_log::log::Level::Debug,
                    "a press is the session's own work, not forge's warning: {}",
                    line.message,
                );
            }
            // No hold: both device fields are absent while the press names one.
            assert!(
                presses[0].message.contains("requested Some(\"held-first\")")
                    && presses[0].message.contains("held None")
                    && presses[0].message.contains("held_for None")
                    && presses[0].message.contains("delivering false")
                    && presses[0].message.contains("path cold"),
                "the press with no hold says so: {}",
                presses[0].message,
            );
            // Warm: the hold serves the pick asked for.
            assert!(
                presses[1].message.contains("requested Some(\"held-first\")")
                    && presses[1].message.contains("held Some(\"held-first\")")
                    && presses[1].message.contains("held_for Some(\"held-first\")")
                    && presses[1].message.contains("delivering true")
                    && presses[1].message.contains("path warm"),
                "the warm press names the hold that served it: {}",
                presses[1].message,
            );
            // Cold on another pick: the devices differ, which is why it opened.
            assert!(
                presses[2].message.contains("requested Some(\"asked-second\")")
                    && presses[2].message.contains("held Some(\"held-first\")")
                    && presses[2].message.contains("held_for Some(\"held-first\")")
                    && presses[2].message.contains("delivering false")
                    && presses[2].message.contains("path cold"),
                "the cold press names the pick it would not use: {}",
                presses[2].message,
            );
            assert!(
                presses[3].message.contains("requested Some(\"asked-third\")")
                    && presses[3].message.contains("path busy"),
                "the refused press says so rather than reading as a take: {}",
                presses[3].message,
            );
            assert!(
                presses[4].message.contains("requested Some(\"asked-second\")")
                    && presses[4].message.contains("path warm"),
                "the press after a reload reads as warm: the claim ended the orphaned take \
                 before the hold was read: {}",
                presses[4].message,
            );
            // The same pick and no delivery: the devices match and it still
            // opened, so the line says why.
            assert!(
                presses[5].message.contains("requested Some(\"asked-second\")")
                    && presses[5].message.contains("held Some(\"asked-second\")")
                    && presses[5].message.contains("held_for Some(\"asked-second\")")
                    && presses[5].message.contains("delivering false")
                    && presses[5].message.contains("path cold"),
                "the press opens a capture the device stopped serving, and says so: {}",
                presses[5].message,
            );
            // A default hold: the device resolved, the pick it was opened for
            // absent, which is the one line that tells the two apart.
            assert!(
                presses[6].message.contains("requested None")
                    && presses[6].message.contains("held Some(\"default\")")
                    && presses[6].message.contains("held_for None")
                    && presses[6].message.contains("path warm"),
                "the default's hold is named by the device it resolved to, not the pick: {}",
                presses[6].message,
            );
        }

        #[test]
        fn a_hold_on_a_capture_that_stopped_delivering_opens_the_device_again() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            staled(&rig);
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("reopens")
                .expect("held");
            assert_eq!(
                rig.calls(),
                vec!["open one", "open one", "release one"],
                "a hold on a capture the device stopped serving opens it again"
            );
        }

        /// **The screen's lock state is the shell's to read, and a capture
        /// behind it is the loop that fed the menu-bar leak.** The lock lets
        /// the held device go, and every hold that arrives while it is on is
        /// answered with nothing rather than opening one.
        #[cfg(target_os = "macos")]
        #[test]
        fn a_locked_screen_lets_the_hold_go_and_opens_nothing_until_it_is_back() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("screen".to_owned()), "page")
                .expect("opens")
                .expect("held");

            rig.mic.set_screen_away(true);
            assert_eq!(
                rig.calls(),
                vec!["open screen", "release screen"],
                "the lock lets the held device go"
            );
            assert!(
                rig.mic
                    .hold(&rig.bridge, Some("screen".to_owned()), "page")
                    .expect("answers")
                    .is_none(),
                "a hold behind a locked screen opens nothing"
            );
            assert_eq!(rig.calls().len(), 2, "and the device stays let go");

            // The same reading twice is one release, not a second.
            rig.mic.set_screen_away(true);
            assert_eq!(rig.calls().len(), 2);

            // Back on screen: the page takes the hold up again from its pick.
            rig.mic.set_screen_away(false);
            let held = rig
                .mic
                .hold(&rig.bridge, Some("screen".to_owned()), "page")
                .expect("opens")
                .expect("held");
            assert_eq!(held.id, "screen");
            assert_eq!(
                rig.calls(),
                vec!["open screen", "release screen", "open screen"],
                "the hold is taken up again once the screen is back"
            );
        }

        /// The page's own reading of itself, which the shell cannot see: a
        /// hidden window feeds no take either, so the same rule holds.
        #[test]
        fn a_hidden_page_lets_the_hold_go_and_opens_nothing_until_it_is_back() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("hidden".to_owned()), "page")
                .expect("opens")
                .expect("held");

            rig.mic.set_hidden(true);
            assert_eq!(
                rig.calls(),
                vec!["open hidden", "release hidden"],
                "the hold is let go with the page"
            );
            assert!(
                rig.mic
                    .hold(&rig.bridge, Some("hidden".to_owned()), "page")
                    .expect("answers")
                    .is_none(),
                "a hold behind a hidden page opens nothing"
            );
            assert_eq!(rig.calls().len(), 2);

            rig.mic.set_hidden(false);
            assert!(
                rig.mic
                    .hold(&rig.bridge, Some("hidden".to_owned()), "page")
                    .expect("opens")
                    .is_some(),
                "the hold is taken up once the page is back"
            );
            assert_eq!(rig.calls().len(), 3);
        }

        /// **A press that lands after the lock opens nothing.** The key and
        /// the lock can cross; the press is answered with a line rather than
        /// a device a locked screen can never feed.
        #[cfg(target_os = "macos")]
        #[test]
        fn a_press_racing_the_lock_opens_nothing_and_says_so() {
            let rig = Rig::new();
            rig.mic.set_screen_away(true);

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("edge".to_owned()), "page", "take")
                    .err()
                    .as_deref(),
                Some(AWAY),
                "the press is refused by name",
            );
            assert!(rig.calls().is_empty(), "and no device is opened");

            rig.mic.set_screen_away(false);
            assert!(
                rig.mic.start(&rig.bridge, Some("edge".to_owned()), "page", "take").is_ok(),
                "the press records once the screen is back",
            );
            assert_eq!(rig.calls(), vec!["open edge"]);
        }

        /// **A press whose open spans the lock records nothing.** The
        /// transition lands its signal while the open blocks and queues
        /// behind the press's guard; the press reads the signal on its way
        /// out, ends the take it opened and lets the capture go.
        #[cfg(target_os = "macos")]
        #[test]
        fn a_press_whose_open_spans_the_lock_records_nothing() {
            let settle = Duration::from_secs(10);
            let (entered, arrived) = std::sync::mpsc::channel::<()>();
            let (release, parked) = std::sync::mpsc::channel::<()>();
            // The opener is a `Fn` and must be `Sync`, so the one-shot wait
            // is held where it can be taken, as the wedged-open rig does.
            let parked: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>> =
                Arc::new(Mutex::new(Some(parked)));
            let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let opened = Arc::clone(&log);
            let mic = Arc::new(Mic {
                warm: Mutex::new(None),
                screen_away: AtomicBool::new(false),
                hidden_away: AtomicBool::new(false),
                restraint: Mutex::new(Restraint::default()),
                opener: Box::new(move |device, carry| {
                    let _ = entered.send(());
                    let waiting = parked.lock().unwrap_or_else(PoisonError::into_inner).take();
                    if let Some(waiting) = waiting {
                        let _ = waiting.recv_timeout(settle);
                    }
                    let name = device.unwrap_or("default").to_owned();
                    opened.lock().unwrap().push(format!("open {name}"));
                    Ok(Box::new(Fake {
                        device: forge_capture::Device {
                            id: name,
                            name: "stand-in".to_owned(),
                            is_default: device.is_none(),
                        },
                        carry,
                        log: Arc::clone(&opened),
                    }))
                }),
            });
            let bridge = Bridge::new(None, tokio::sync::mpsc::unbounded_channel().0);

            let pressing = {
                let mic = Arc::clone(&mic);
                let bridge = Arc::clone(&bridge);
                std::thread::spawn(move || {
                    mic.start(&bridge, Some("spanning".to_owned()), "page", "take")
                })
            };
            arrived.recv_timeout(settle).expect("the open is in flight");

            // The lock lands while the open blocks: its signal is set before
            // it queues on the guard the press is holding.
            let locking = {
                let mic = Arc::clone(&mic);
                std::thread::spawn(move || mic.set_screen_away(true))
            };
            let deadline = Instant::now() + settle;
            while !mic.away_now() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(mic.away_now(), "the transition landed its signal");

            release.send(()).expect("the open is parked");
            let outcome = pressing.join().expect("the press finishes");
            locking.join().expect("the transition finishes");

            assert_eq!(
                outcome.err().as_deref(),
                Some(AWAY),
                "the press answers the away line rather than reporting a take",
            );
            assert_eq!(
                log.lock().unwrap().clone(),
                vec!["open spanning", "release spanning"],
                "the capture that came up behind the lock is let go, not left recording",
            );
            assert!(mic.warm.lock().unwrap().is_none(), "and the hold keeps nothing");
        }

        /// **The away press must not hand a spent budget a fresh start.**
        /// Moving the `fresh()` above the `away()` check would pass every
        /// other test while doing exactly that.
        #[cfg(target_os = "macos")]
        #[test]
        fn an_away_press_leaves_the_spent_budget_spent() {
            let attempts = Arc::new(Mutex::new(0usize));
            let counted = Arc::clone(&attempts);
            let mic = Mic {
                warm: Mutex::new(None),
                screen_away: AtomicBool::new(false),
                hidden_away: AtomicBool::new(false),
                restraint: Mutex::new(Restraint {
                    waits: [Duration::ZERO; REOPEN_ATTEMPTS],
                    ..Restraint::default()
                }),
                opener: Box::new(move |_, _| {
                    *counted.lock().unwrap() += 1;
                    Err("the device is gone".to_owned())
                }),
            };
            let bridge = Bridge::new(None, tokio::sync::mpsc::unbounded_channel().0);
            for _ in 0..REOPEN_ATTEMPTS {
                assert!(mic.hold(&bridge, Some("gone".to_owned()), "page").is_err());
            }
            assert!(mic.restraint.lock().unwrap().spent, "the budget is spent");

            mic.set_screen_away(true);
            assert_eq!(
                mic.start(&bridge, Some("gone".to_owned()), "page", "take").err().as_deref(),
                Some(AWAY),
                "the press is refused while away",
            );
            assert!(
                mic.restraint.lock().unwrap().spent,
                "and the away press left the spent budget spent",
            );
            assert_eq!(*attempts.lock().unwrap(), REOPEN_ATTEMPTS, "no device was opened");
        }

        /// **A take in flight is not the hold.** Hiding the page mid-take
        /// must not throw away words already spoken: the capture serves the
        /// take until the page ends it, and is let go then.
        #[test]
        fn a_take_in_flight_outlives_the_page_hiding_and_is_let_go_when_it_ends() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");

            rig.mic.set_hidden(true);
            assert_eq!(
                rig.calls(),
                vec!["open one"],
                "the take's capture is not torn out from under it",
            );

            // A re-stated hold behind the hidden page is still answered with
            // nothing, and the take keeps its stream.
            assert!(
                rig.mic
                    .hold(&rig.bridge, Some("one".to_owned()), "page")
                    .expect("answers")
                    .is_none(),
            );
            assert_eq!(rig.calls(), vec!["open one"]);

            // The page's own stop ends the take, and with it the capture the
            // away screen was waiting on.
            rig.mic.stop("take-1");
            assert_eq!(
                rig.calls(),
                vec!["open one", "release one"],
                "the capture is let go with the take it was kept for",
            );

            rig.mic.set_hidden(false);
            assert!(
                rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens").is_some(),
                "the hold is taken up again on return",
            );
            assert_eq!(rig.calls(), vec!["open one", "release one", "open one"]);
        }

        /// The drain is the other way a take ends, and it owes the same
        /// release when the screen is away.
        #[test]
        fn a_drain_ending_a_take_behind_a_hidden_page_lets_the_capture_go() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");

            rig.mic.set_hidden(true);
            drained(&rig, "take-1");
            assert_eq!(rig.calls(), vec!["open one", "release one"]);
        }

        /// **The reopen budget.** A capture the device keeps dropping is
        /// reopened a few times with growing waits, and then the hold gives
        /// up; it answers nothing and opens nothing until a press, another
        /// pick, or the screen coming back. An unbounded retry is what a
        /// locked screen used to feed, and each open rebuilds the macOS menu
        /// bar.
        #[test]
        fn a_capture_that_keeps_stopping_is_reopened_a_few_times_then_given_up() {
            let _kept = crate::log_capture::capture();
            let rig = Rig::new();
            let asks = |device: &str| {
                rig.mic
                    .hold(&rig.bridge, Some(device.to_owned()), "page")
                    .expect("a hold answers one way or the other")
            };
            let device = "stops-serving";
            assert!(asks(device).is_some(), "the first hold opens the device");

            for _ in 0..REOPEN_ATTEMPTS {
                staled(&rig);
                assert!(asks(device).is_some(), "a stale capture is reopened within the budget");
            }
            assert_eq!(
                rig.calls(),
                vec![
                    "open stops-serving",
                    "open stops-serving",
                    "release stops-serving",
                    "open stops-serving",
                    "release stops-serving",
                    "open stops-serving",
                    "release stops-serving",
                ],
                "the first open, then one reopen per attempt, each letting the dead one go",
            );

            // The budget is spent: the dead capture is let go once, and every
            // later hold - however often one arrives - opens nothing.
            staled(&rig);
            assert!(asks(device).is_none(), "a spent budget answers no capture");
            assert_eq!(rig.calls().len(), 8, "the dead capture is let go rather than kept");
            assert!(asks(device).is_none(), "and a later hold is answered the same way");
            assert_eq!(rig.calls().len(), 8, "the give-up is not a loop of its own");

            // This test's own lines, by the device only it asks for: the
            // capture buffer is shared by an in-process run, and another
            // test's reopen is not this one's count.
            let lines: Vec<String> = crate::log_capture::lines()
                .into_iter()
                .filter(|line| line.contains("requested Some(\"stops-serving\")"))
                .collect();
            let attempts: Vec<&String> = lines
                .iter()
                .filter(|line| line.contains("event_name client_mic_reopen,"))
                .collect();
            assert_eq!(
                attempts.len(),
                REOPEN_ATTEMPTS,
                "each attempt leaves one line, and no more: {attempts:?}",
            );
            assert!(
                lines.iter().any(|line| line.contains("client_mic_reopen_given_up")),
                "the give-up is named once, which is what says silence is deliberate: {lines:?}",
            );

            // **A press is a real trigger**: the reader asking again is worth
            // a fresh try, spent budget or not.
            rig.mic
                .start(&rig.bridge, Some(device.to_owned()), "page", "take-1")
                .expect("a press opens the device itself");
            assert_eq!(rig.calls().len(), 9, "the press opens a device the budget had given up on");
        }

        /// **A device that will not open spends the same budget.** A hold
        /// that fails answers the device's own words the first few times and
        /// then stops opening at all - silence, not an error per page
        /// re-statement - and a press is what asks again.
        #[test]
        fn a_device_that_will_not_open_is_left_alone_after_the_budget_until_a_press() {
            let attempts = Arc::new(Mutex::new(0usize));
            let counted = Arc::clone(&attempts);
            let mic = Mic {
                warm: Mutex::new(None),
                screen_away: AtomicBool::new(false),
                hidden_away: AtomicBool::new(false),
                restraint: Mutex::new(Restraint {
                    waits: [Duration::ZERO; REOPEN_ATTEMPTS],
                    ..Restraint::default()
                }),
                opener: Box::new(move |_, _| {
                    *counted.lock().unwrap() += 1;
                    Err("the device is gone".to_owned())
                }),
            };
            let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
            let bridge = Bridge::new(None, events);

            for _ in 0..REOPEN_ATTEMPTS {
                assert_eq!(
                    mic.hold(&bridge, Some("gone".to_owned()), "page").err().as_deref(),
                    Some("the device is gone"),
                    "a failing open answers the device's own words while the budget lasts",
                );
            }
            assert_eq!(*attempts.lock().unwrap(), REOPEN_ATTEMPTS);

            // Spent: the hold opens nothing and is no longer an error - the
            // page re-stating its hold must not read as a failure per call.
            assert!(
                mic.hold(&bridge, Some("gone".to_owned()), "page").expect("answers").is_none(),
                "a spent budget answers no capture",
            );
            assert_eq!(*attempts.lock().unwrap(), REOPEN_ATTEMPTS, "and opens nothing");

            // A press is a real trigger, and its failure is the reader's.
            assert_eq!(
                mic.start(&bridge, Some("gone".to_owned()), "page", "take").err().as_deref(),
                Some("the device is gone"),
                "the press tries again and says why",
            );
            assert_eq!(*attempts.lock().unwrap(), REOPEN_ATTEMPTS + 1);
        }

        /// **The spacing holds through the hold path too.** The rig zeroes
        /// its waits for the attempt tests, so this one sets a real one and
        /// proves a hold inside the wait opens nothing.
        #[test]
        fn a_reopen_that_is_too_soon_opens_nothing() {
            let rig = Rig::new();
            rig.mic.restraint.lock().unwrap().waits =
                [Duration::ZERO, Duration::from_secs(60), Duration::from_secs(60)];
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");

            // The first reopen is taken at once; its wait is the second's.
            staled(&rig);
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("reopens")
                .expect("held");
            let opened = rig.calls();

            staled(&rig);
            assert!(
                rig.mic
                    .hold(&rig.bridge, Some("one".to_owned()), "page")
                    .expect("answers")
                    .is_none(),
                "a reopen inside its wait is refused rather than opened",
            );
            assert_eq!(rig.calls(), opened, "and nothing is opened or released");
        }

        /// The budget's own clock, without a device: the waits grow between
        /// attempts, the last one is the last, and a real trigger starts it
        /// over.
        #[test]
        fn the_reopen_budget_waits_between_attempts_and_then_gives_up() {
            // The shipped waits, not the test build's default of zero.
            let mut restraint = Restraint { waits: REOPEN_WAITS, ..Restraint::default() };
            let start = Instant::now();
            assert!(restraint.ask(Some("one")), "the first ask of a device is a change");

            assert_eq!(restraint.reopen(start), Reopen::Now, "the first attempt is taken at once");
            restraint.attempted(start);
            assert_eq!(
                restraint.reopen(start),
                Reopen::Wait,
                "the second waits its own interval rather than opening under the first"
            );
            let second = start + REOPEN_WAITS[1];
            assert_eq!(restraint.reopen(second), Reopen::Now, "the wait spent, it opens");
            restraint.attempted(second);
            assert_eq!(
                restraint.reopen(second),
                Reopen::Wait,
                "and the next one waits its own, longer, interval"
            );
            let third = second + REOPEN_WAITS[2];
            assert_eq!(restraint.reopen(third), Reopen::Now);
            restraint.attempted(third);
            assert_eq!(
                restraint.reopen(third),
                Reopen::GivenUp,
                "the budget is spent after the last attempt"
            );
            assert_eq!(restraint.reopen(third), Reopen::GivenUp);

            restraint.fresh();
            assert_eq!(
                restraint.reopen(third),
                Reopen::Now,
                "a real trigger starts the budget over"
            );
            assert!(!restraint.ask(Some("one")), "the same device asking again is not a trigger");
            assert!(restraint.ask(Some("two")), "another pick is");
        }

        #[test]
        fn a_hold_for_a_stopped_capture_that_will_not_reopen_answers_the_error() {
            let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let opened = Arc::clone(&log);
            let mic = Mic {
                warm: Mutex::new(None),
                screen_away: AtomicBool::new(false),
                hidden_away: AtomicBool::new(false),
                restraint: Mutex::new(Restraint::default()),
                opener: Box::new(move |device, carry| {
                    let mut opened = opened.lock().unwrap();
                    if !opened.is_empty() {
                        return Err("the device is gone".to_owned());
                    }
                    let name = device.unwrap_or("default").to_owned();
                    opened.push(format!("open {name}"));
                    Ok(Box::new(Fake {
                        device: forge_capture::Device {
                            id: name,
                            name: "stand-in".to_owned(),
                            is_default: false,
                        },
                        carry,
                        log: Arc::clone(&log),
                    }))
                }),
            };
            let bridge = Bridge::new(None, tokio::sync::mpsc::unbounded_channel().0);
            mic.hold(&bridge, Some("one".to_owned()), "page").expect("opens").expect("held");

            // The device drops the capture, so re-stating the same pick opens
            // again - and that open fails. The error is the caller's answer,
            // and the capture the hold was keeping is still what it keeps.
            {
                let warm = mic.warm.lock().unwrap();
                let held = warm.as_ref().expect("a held capture");
                held.held.carry().held.lock().unwrap().at = Instant::now()
                    .checked_sub(SILENT_LIMIT * 4)
                    .expect("the clock has run past four silent limits");
            }
            // The capture the hold keeps, by identity: a fresh open of the
            // same input would carry the same id, so only the pointer proves
            // it is the SAME capture rather than a new one.
            let first = {
                let warm = mic.warm.lock().unwrap();
                Arc::clone(warm.as_ref().expect("a held capture").held.carry())
            };
            let refused = mic.hold(&bridge, Some("one".to_owned()), "page");
            assert_eq!(refused.err().as_deref(), Some("the device is gone"));
            let warm = mic.warm.lock().unwrap();
            let held = warm.as_ref().expect("a capture is still held");
            assert!(
                Arc::ptr_eq(held.held.carry(), &first),
                "the capture that was held is still standing, not a fresh open of the same input"
            );
        }

        #[test]
        fn the_pick_changing_reacquires_the_device() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            rig.mic
                .hold(&rig.bridge, Some("two".to_owned()), "page")
                .expect("reopens")
                .expect("held");
            assert_eq!(
                rig.calls(),
                vec!["open one", "open two", "release one"],
                "the new pick opens before the old one is let go"
            );
            // And the new pick is the one a take gets.
            let held = rig
                .mic
                .start(&rig.bridge, Some("two".to_owned()), "page", "take")
                .expect("the take starts");
            assert_eq!(held.id, "two", "the re-acquired device is the one recorded from");
        }

        #[test]
        fn a_press_for_another_device_takes_over_the_hold() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            rig.mic
                .start(&rig.bridge, None, "page", "take-1")
                .expect("a plain take opens the default");
            assert_eq!(
                rig.calls(),
                vec!["open one", "open default", "release one"],
                "the press the hold cannot serve opens its own, and keeps it"
            );
            // The hold now follows the pressed device: the pick moves with
            // the take rather than fighting it.
            rig.mic.stop("take-1");
            rig.mic
                .start(&rig.bridge, None, "page", "take-2")
                .expect("the next plain take is warm");
            assert_eq!(rig.calls().len(), 3, "the second press reuses the take's own capture");
        }

        #[test]
        fn release_lets_the_device_go() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            rig.mic.release();
            assert_eq!(rig.calls(), vec!["open one", "release one"]);
            rig.mic.release();
            assert_eq!(rig.calls().len(), 2, "a second release has nothing to let go");
        }

        #[test]
        fn a_hold_while_recording_waits_for_the_next_take() {
            let rig = Rig::new();
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take")
                .expect("starts cold");
            // The page recording states a new pick as it changes: its own
            // hold is not a claim from another page, so the take keeps the
            // stream it is recording on.
            let answered = rig
                .mic
                .hold(&rig.bridge, Some("two".to_owned()), "page")
                .expect("held for the next take")
                .expect("held");
            assert_eq!(
                rig.calls(),
                vec!["open one"],
                "the recording stream is not torn out from under the take"
            );
            assert_eq!(answered.id, "one", "and the take's own input is what a hold answers");
        }

        #[test]
        fn a_second_take_on_a_live_capture_is_refused() {
            let rig = Rig::new();
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");
            let refused = rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-2");
            assert_eq!(refused.err().as_deref(), Some("the microphone is already recording"));
        }

        /// The state a reload leaves: a take still recording, and the page
        /// that held its stop gone.
        fn reload(rig: &Rig, device: Option<String>) {
            rig.mic.hold(&rig.bridge, device.clone(), "page-a").expect("opens").expect("held");
            rig.mic.start(&rig.bridge, device, "page-a", "take-a").expect("starts");
            // Nothing else happens on this side: a reload discards the page
            // without ever ending the take it began.
        }

        #[test]
        fn a_press_after_the_recording_page_reloads_recovers() {
            let rig = Rig::new();
            reload(&rig, Some("one".to_owned()));
            // The page that comes back mounts and states its hold, as every
            // page does on its way in.
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page-b")
                .expect("holds")
                .expect("held");
            match rig.mic.start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b") {
                Ok(started) => assert_eq!(started.id, "one"),
                Err(why) => panic!("the press after a reload is refused: {why}"),
            }
            assert_eq!(
                rig.calls(),
                vec!["open one"],
                "only the orphaned recording ends: the device stays held across the reload"
            );
        }

        #[test]
        fn a_press_before_the_new_pages_hold_ends_the_orphaned_take_too() {
            let rig = Rig::new();
            reload(&rig, Some("one".to_owned()));
            // The mount's hold is in flight, so the press is the first call
            // the reloaded page makes.
            match rig.mic.start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b") {
                Ok(started) => assert_eq!(started.id, "one"),
                Err(why) => panic!("the press before the new page's hold is refused: {why}"),
            }
        }

        /// Run one drain to its end, the way the shell runs it: the window
        /// it sleeps for is real, so the test waits it out.
        fn drained(rig: &Rig, take: &str) {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("a runtime")
                .block_on(rig.mic.drain(take));
        }

        #[test]
        fn a_drain_in_flight_when_the_next_take_begins_leaves_it_alone() {
            let rig = Rig::new();
            reload(&rig, Some("one".to_owned()));

            // page-a's drain is invoked while it still records, and page-b's
            // press lands inside the window: the live collision, drain first.
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("a runtime")
                .block_on(async {
                    let drain = rig.mic.drain("take-a");
                    let begin = async {
                        rig.mic
                            .hold(&rig.bridge, Some("one".to_owned()), "page-b")
                            .expect("holds")
                            .expect("held");
                        rig.mic
                            .start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b")
                            .expect("page-b records");
                    };
                    tokio::join!(drain, begin);
                });

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b-2")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "the stale drain must not end the take the reloaded page began",
            );
        }

        /// The issue's own sequence (#2279): one page, two takes, and a
        /// drain still in flight from the first.
        #[test]
        fn a_drain_waking_after_the_same_pages_re_press_leaves_the_new_take_alone() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");

            // The release begins the drain and the socket drops - or the
            // server says `dictate_ended` - while it sleeps, so the take ends
            // and the page's own re-press is free.
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("a runtime")
                .block_on(async {
                    let drain = rig.mic.drain("take-1");
                    let re_press = async {
                        rig.mic.stop("take-1");
                        rig.mic
                            .start(&rig.bridge, Some("one".to_owned()), "page", "take-2")
                            .expect("the re-press records");
                    };
                    tokio::join!(drain, re_press);
                });

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "page", "take-3")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "the stale drain must not end the take the same page's re-press began",
            );
        }

        /// The stop's half of the same window: it is fire-and-forget on the
        /// page's side, so one for the take that ended can land after the
        /// re-press began.
        #[test]
        fn a_stop_for_the_take_that_ended_leaves_the_new_take_alone() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");
            rig.mic.stop("take-1");
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take-2")
                .expect("the re-press records");

            rig.mic.stop("take-1");

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "page", "take-3")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "the late stop must not end the take the re-press began",
            );
        }

        /// One page, and a first take whose token happens to equal the page's
        /// own id: the guards must read the take field, not the page, however
        /// the two strings line up.
        #[test]
        fn a_guard_reads_the_take_field_even_when_the_token_equals_the_page() {
            let rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "same")
                .expect("opens")
                .expect("held");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "same", "same").expect("starts");
            rig.mic.stop("same");
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "same", "next")
                .expect("the re-press records");

            rig.mic.stop("same");

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "same", "third")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "a stop naming the page-id take must not end the take that followed it",
            );
        }

        #[test]
        fn a_stop_from_the_reloaded_page_leaves_the_new_take_alone() {
            let rig = Rig::new();
            reload(&rig, Some("one".to_owned()));
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page-b")
                .expect("holds")
                .expect("held");
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b")
                .expect("page-b records");

            rig.mic.stop("take-a");

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b-2")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "the stale stop must not end the take the reloaded page began",
            );
        }

        #[test]
        fn a_drain_from_the_recording_take_ends_that_take() {
            let mut rig = Rig::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("opens")
                .expect("held");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");

            // A partial frame the device left mid-frame: the drain's hand-over.
            let partial = 8usize;
            {
                let warm = rig.mic.warm.lock().unwrap();
                let held = warm.as_ref().expect("warm");
                held.held.carry().push(&vec![0.0; partial]);
            }
            drained(&rig, "take-1");

            assert!(
                matches!(
                    rig.frames.try_recv(),
                    Ok(crate::bridge::ClientEvent::Mic(bytes)) if bytes.len() == 1 + partial * 2
                ),
                "the take's own drain carries the partial frame it was holding",
            );
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take-2")
                .expect("the take's own drain ended its take");
        }

        #[test]
        fn a_pick_that_will_not_open_leaves_the_working_hold_standing() {
            let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let opened = Arc::clone(&log);
            let mic = Mic {
                warm: Mutex::new(None),
                screen_away: AtomicBool::new(false),
                hidden_away: AtomicBool::new(false),
                restraint: Mutex::new(Restraint::default()),
                opener: Box::new(move |device, carry| {
                    let name = device.unwrap_or("default").to_owned();
                    if name == "gone" {
                        return Err("no input device named \"gone\"".to_owned());
                    }
                    opened.lock().unwrap().push(format!("open {name}"));
                    Ok(Box::new(Fake {
                        device: forge_capture::Device {
                            id: name,
                            name: "stand-in".to_owned(),
                            is_default: false,
                        },
                        carry,
                        log: Arc::clone(&opened),
                    }))
                }),
            };
            let bridge = Bridge::new(None, tokio::sync::mpsc::unbounded_channel().0);
            mic.hold(&bridge, Some("one".to_owned()), "page").expect("opens").expect("held");
            let refused = mic.hold(&bridge, Some("gone".to_owned()), "page");
            assert!(refused.is_err(), "the pick that will not open is reported");
            assert_eq!(
                mic.hold(&bridge, Some("one".to_owned()), "page")
                    .expect("still held")
                    .expect("held")
                    .id,
                "one",
                "the hold that was working is the one still standing"
            );
        }

        /// **A bad pick spends its budget without tearing down the good
        /// one.** Three failed opens of "bad" must not release the healthy
        /// capture the previous pick was serving on.
        #[test]
        fn a_bad_pick_that_spends_the_budget_leaves_the_healthy_hold_standing() {
            let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let opened = Arc::clone(&log);
            let attempts = Arc::new(Mutex::new(0usize));
            let counted = Arc::clone(&attempts);
            let mic = Mic {
                warm: Mutex::new(None),
                screen_away: AtomicBool::new(false),
                hidden_away: AtomicBool::new(false),
                restraint: Mutex::new(Restraint {
                    waits: [Duration::ZERO; REOPEN_ATTEMPTS],
                    ..Restraint::default()
                }),
                opener: Box::new(move |device, carry| {
                    let name = device.unwrap_or("default").to_owned();
                    if name == "bad" {
                        *counted.lock().unwrap() += 1;
                        return Err("no input device named \"bad\"".to_owned());
                    }
                    opened.lock().unwrap().push(format!("open {name}"));
                    Ok(Box::new(Fake {
                        device: forge_capture::Device {
                            id: name,
                            name: "stand-in".to_owned(),
                            is_default: false,
                        },
                        carry,
                        log: Arc::clone(&opened),
                    }))
                }),
            };
            let bridge = Bridge::new(None, tokio::sync::mpsc::unbounded_channel().0);
            mic.hold(&bridge, Some("one".to_owned()), "page").expect("opens").expect("held");

            // The budget and one more: the last is answered with nothing once
            // it is spent.
            for _ in 0..REOPEN_ATTEMPTS {
                assert!(
                    mic.hold(&bridge, Some("bad".to_owned()), "page").is_err(),
                    "the bad pick answers the device's words while the budget lasts",
                );
            }
            assert!(
                mic.hold(&bridge, Some("bad".to_owned()), "page").expect("answers").is_none(),
                "a spent budget answers no capture",
            );
            assert_eq!(*attempts.lock().unwrap(), REOPEN_ATTEMPTS);

            // The healthy hold for the previous pick never left.
            assert_eq!(
                mic.hold(&bridge, Some("one".to_owned()), "page")
                    .expect("answers")
                    .expect("held")
                    .id,
                "one",
                "the healthy capture is not torn down by another pick's failures",
            );
            assert_eq!(log.lock().unwrap().clone(), vec!["open one"], "and it was never reopened");
        }
    }

    /// The real device, through the hold. What the stand-ins cannot prove is
    /// that `forge-capture`'s own callback reaches the carry, so this opens
    /// the machine's input and reads one frame off it. Ignored by default,
    /// like the capture leaf's own hardware test; run with `--run-ignored
    /// all` on a machine with a microphone.
    #[test]
    #[ignore = "opens the real default input; run with --run-ignored all on a machine with audio hardware"]
    fn a_held_capture_hands_a_take_real_frames() {
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Bridge::new(None, events);
        let mic = Mic::default();
        let Ok(Some(held)) = mic.hold(&bridge, None, "page") else {
            eprintln!("no input on this machine; the wiring is unproven here");
            return;
        };
        let take = mic
            .start(&bridge, None, "page", "take-1")
            .expect("the held capture must serve the take");
        assert_eq!(take.id, held.id, "the take records from the device the hold opened");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut frames = 0;
        while frames == 0 && std::time::Instant::now() < deadline {
            match received.try_recv() {
                Ok(crate::bridge::ClientEvent::Mic(bytes)) => {
                    assert_eq!(
                        bytes.len(),
                        1 + FRAME_SAMPLES * 2,
                        "a take's frames are whole 20 ms frames on the wire's own shape"
                    );
                    frames += 1;
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        assert!(frames > 0, "a take on a held capture must be fed the device's frames");

        // And ending the take keeps the device: the second take is served by
        // the same capture, which is the whole point of holding it.
        mic.stop("take-1");
        let again = mic
            .start(&bridge, None, "page", "take-2")
            .expect("the next take is served without reopening");
        assert_eq!(again.id, held.id, "the same device, still open");
        mic.release();
    }

    mod the_carry {

        use super::*;

        /// The bridge's own channel, so the frames a take sends are read
        /// where the page would read them.
        fn bridge()
        -> (Arc<Bridge>, tokio::sync::mpsc::UnboundedReceiver<crate::bridge::ClientEvent>) {
            let (events, received) = tokio::sync::mpsc::unbounded_channel();
            (Bridge::new(None, events), received)
        }

        fn frame_bytes(event: &crate::bridge::ClientEvent) -> Vec<u8> {
            match event {
                crate::bridge::ClientEvent::Mic(bytes) => bytes.clone(),
                other => panic!("a take's frames cross as Mic events, got {other:?}"),
            }
        }

        #[test]
        fn a_held_capture_drops_its_frames() {
            let (bridge, mut received) = bridge();
            let carry = Carry::new(&bridge, false);
            for _ in 0..10 {
                carry.push(&[0.5; 160]);
            }
            assert!(
                received.try_recv().is_err(),
                "audio that belongs to no take must not reach the page"
            );
        }

        #[test]
        fn a_take_frames_exactly_what_it_records() {
            let (bridge, mut received) = bridge();
            let carry = Carry::new(&bridge, false);
            carry.push(&[0.5; 160]);
            carry.begin();
            // The pre-press half of a frame is not the take's.
            carry.push(&[0.5; 160]);
            assert!(received.try_recv().is_err(), "a partial frame waits for its other half");

            carry.push(&[0.25; FRAME_SAMPLES]);
            let event = received.try_recv().expect("one whole frame crosses");
            let bytes = frame_bytes(&event);
            assert_eq!(bytes.len(), 1 + FRAME_SAMPLES * 2, "the tag, then one sample per word");
            assert_eq!(
                bytes[1..5],
                encode_frame(&[0.5, 0.5])[1..5],
                "the take's first sample is the first one pushed INSIDE it"
            );
        }

        #[test]
        fn the_end_hands_over_the_partial_and_stops_the_flow() {
            let (bridge, mut received) = bridge();
            let carry = Carry::new(&bridge, true);
            carry.push(&[0.5; 100]);
            let tail = carry.end();
            assert_eq!(tail.len(), 100, "the last partial frame is the caller's to send");

            carry.push(&[0.5; FRAME_SAMPLES]);
            assert!(received.try_recv().is_err(), "and nothing crosses after the end");
        }
    }

    /// The bounded open: a device layer that answers neither way is reported
    /// rather than waited on for ever.
    mod the_bounded_open {
        use std::sync::mpsc::{Receiver, Sender};

        use tokio::sync::oneshot;

        use super::*;

        /// The bound the tests drive an open with. The shipped one is
        /// [`OPEN_LIMIT`], and what matters here is that the bound is the
        /// caller's, so no test waits the real twenty seconds.
        const BOUND: Duration = Duration::from_millis(50);

        /// The bound on everything a test does not mean to measure: the open
        /// reaching the device, the parked open being let go, and the budget
        /// an open that ANSWERS is given. A failure bound rather than a wait -
        /// nothing here is timing the open itself.
        const SETTLE: Duration = Duration::from_secs(10);

        /// A stand-in capture, as the hold's own test stands one in.
        struct Fake {
            device: forge_capture::Device,
            carry: Arc<Carry>,
            log: Arc<Mutex<Vec<String>>>,
        }

        impl Held for Fake {
            fn device(&self) -> &forge_capture::Device {
                &self.device
            }

            fn carry(&self) -> &Arc<Carry> {
                &self.carry
            }

            fn release(self: Box<Self>) {
                self.log.lock().unwrap().push(format!("release {}", self.device.id));
            }
        }

        /// A stand-in device whose FIRST open answers only when the test lets
        /// it go: the wedged layer, answering neither way until then. A later
        /// open is answered at once, so a test that trips the busy guard hears
        /// about it rather than parking again.
        struct Wedged {
            mic: Arc<Mic>,
            /// Fires as the opener is entered, which is what tells a test the
            /// open is in flight rather than a thread that never ran.
            entered: oneshot::Receiver<()>,
            /// Lets the parked open answer, once the test is done with it.
            release: Sender<()>,
            bridge: Arc<Bridge>,
            log: Arc<Mutex<Vec<String>>>,
        }

        impl Wedged {
            fn new() -> Self {
                let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let opened = Arc::clone(&log);
                let (entered, answer) = oneshot::channel::<()>();
                let (release, holding) = std::sync::mpsc::channel::<()>();
                let parked: Arc<Mutex<Option<Receiver<()>>>> = Arc::new(Mutex::new(Some(holding)));
                // The signal crosses once, and a `Fn` closure may be called
                // again, so the sender is held where it can be taken.
                let arrived: Arc<Mutex<Option<oneshot::Sender<()>>>> =
                    Arc::new(Mutex::new(Some(entered)));
                let mic = Arc::new(Mic {
                    warm: Mutex::new(None),
                    screen_away: AtomicBool::new(false),
                    hidden_away: AtomicBool::new(false),
                    restraint: Mutex::new(Restraint::default()),
                    opener: Box::new(move |device, carry| {
                        let name = device.unwrap_or("default").to_owned();
                        let sent = arrived.lock().unwrap_or_else(PoisonError::into_inner).take();
                        if let Some(sent) = sent {
                            let _ = sent.send(());
                        }
                        let waiting = parked.lock().unwrap_or_else(PoisonError::into_inner).take();
                        if let Some(waiting) = waiting {
                            let _ = waiting.recv_timeout(SETTLE);
                        }
                        opened.lock().unwrap().push(format!("open {name}"));
                        Ok(Box::new(Fake {
                            device: forge_capture::Device {
                                id: name,
                                name: "stand-in".to_owned(),
                                is_default: device.is_none(),
                            },
                            carry,
                            log: Arc::clone(&opened),
                        }))
                    }),
                });
                let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
                Self { mic, entered: answer, release, bridge: Bridge::new(None, events), log }
            }
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread().enable_time().build().expect("a runtime")
        }

        /// A stand-in capture whose release parks until the test lets it go:
        /// the window between a take installing its own capture and the one it
        /// replaced being let go.
        struct Letting {
            device: forge_capture::Device,
            carry: Arc<Carry>,
            /// Fires as the release is entered.
            entered: Arc<Mutex<Option<Sender<()>>>>,
            /// Lets the parked release finish.
            parked: Arc<Mutex<Option<Receiver<()>>>>,
        }

        impl Held for Letting {
            fn device(&self) -> &forge_capture::Device {
                &self.device
            }

            fn carry(&self) -> &Arc<Carry> {
                &self.carry
            }

            fn release(self: Box<Self>) {
                let sent = self.entered.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(sent) = sent {
                    let _ = sent.send(());
                }
                let waiting = self.parked.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(waiting) = waiting {
                    let _ = waiting.recv_timeout(SETTLE);
                }
            }
        }

        /// The swap rig: every capture's release parks, so a test can hold that
        /// window open and hand the give-up in inside it.
        struct Swap {
            mic: Arc<Mic>,
            bridge: Arc<Bridge>,
            /// Fires as a release is entered.
            letting: Receiver<()>,
            /// Lets that release finish.
            let_go: Sender<()>,
        }

        impl Swap {
            fn new() -> Self {
                let (entered, letting) = std::sync::mpsc::channel::<()>();
                let (let_go, parked) = std::sync::mpsc::channel::<()>();
                let entered = Arc::new(Mutex::new(Some(entered)));
                let parked = Arc::new(Mutex::new(Some(parked)));
                let mic = Arc::new(Mic {
                    warm: Mutex::new(None),
                    screen_away: AtomicBool::new(false),
                    hidden_away: AtomicBool::new(false),
                    restraint: Mutex::new(Restraint::default()),
                    opener: Box::new(move |device, carry| {
                        let name = device.unwrap_or("default").to_owned();
                        Ok(Box::new(Letting {
                            device: forge_capture::Device {
                                id: name,
                                name: "stand-in".to_owned(),
                                is_default: device.is_none(),
                            },
                            carry,
                            entered: Arc::clone(&entered),
                            parked: Arc::clone(&parked),
                        }))
                    }),
                });
                let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
                Self { mic, bridge: Bridge::new(None, events), letting, let_go }
            }
        }

        /// A stand-in device that answers at once, the way the machine's own
        /// inputs do. `refusal` stands one in that will not open at all.
        struct Open {
            mic: Arc<Mic>,
            bridge: Arc<Bridge>,
        }

        impl Open {
            fn new() -> Self {
                Self::with_opener(None)
            }

            fn refusing(why: &str) -> Self {
                Self::with_opener(Some(why.to_owned()))
            }

            fn with_opener(refusal: Option<String>) -> Self {
                let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let opened = Arc::clone(&log);
                let mic = Arc::new(Mic {
                    warm: Mutex::new(None),
                    screen_away: AtomicBool::new(false),
                    hidden_away: AtomicBool::new(false),
                    restraint: Mutex::new(Restraint::default()),
                    opener: Box::new(move |device, carry| {
                        if let Some(why) = &refusal {
                            return Err(why.clone());
                        }
                        let name = device.unwrap_or("default").to_owned();
                        opened.lock().unwrap().push(format!("open {name}"));
                        Ok(Box::new(Fake {
                            device: forge_capture::Device {
                                id: name,
                                name: "stand-in".to_owned(),
                                is_default: device.is_none(),
                            },
                            carry,
                            log: Arc::clone(&opened),
                        }))
                    }),
                });
                let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
                Self { mic, bridge: Bridge::new(None, events) }
            }
        }

        /// A stand-in device whose open panics: the device layer the bounded
        /// take's guard has to answer.
        struct Panicking {
            mic: Arc<Mic>,
            bridge: Arc<Bridge>,
        }

        impl Panicking {
            fn new() -> Self {
                let mic = Arc::new(Mic {
                    warm: Mutex::new(None),
                    screen_away: AtomicBool::new(false),
                    hidden_away: AtomicBool::new(false),
                    restraint: Mutex::new(Restraint::default()),
                    opener: Box::new(|_, _| panic!("a wedged audio layer")),
                });
                let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
                Self { mic, bridge: Bridge::new(None, events) }
            }
        }

        /// **The take's own open is guarded too.** `start_within` runs the
        /// open under `guarded`, so a panicking device layer answers the named
        /// error on the take's line rather than reclaiming the take in
        /// silence or crossing the FFI boundary (#2426).
        #[test]
        fn a_panicking_open_is_answered_by_the_bounded_take() {
            let rig = Panicking::new();
            let runtime = runtime();
            let err = runtime
                .block_on(rig.mic.start_within(&rig.bridge, None, "page", "take-1", SETTLE))
                .expect_err("a panicking open is answered");
            assert!(
                err.starts_with("the microphone device layer panicked during the take's open"),
                "the take's line names the panicking operation: {err}"
            );
            assert!(err.contains("a wedged audio layer"), "and the panic's own words: {err}");
        }

        #[test]
        fn an_open_that_answers_neither_way_is_reported_and_leaves_no_take() {
            let rig = Wedged::new();
            let runtime = runtime();
            // Both must complete: the open reaches the device, and the bound
            // answers it. Joined rather than ordered, so neither thread's
            // scheduling decides what this test reads.
            let (refused, entered) = runtime.block_on(async {
                tokio::join!(
                    rig.mic.start_within(&rig.bridge, None, "page", "take-1", BOUND),
                    tokio::time::timeout(SETTLE, rig.entered),
                )
            });
            // The parked open is let go FIRST, whatever the assertions below
            // make of what it did: a thread left parked would hang the test
            // rather than fail it, and a failing mutation has to fail.
            rig.release.send(()).expect("the open is parked");

            assert!(
                matches!(entered, Ok(Ok(()))),
                "the opener was entered: the bound answered an open in flight, not a thread \
                 that never ran, got {entered:?}"
            );
            assert_eq!(
                refused.err().as_deref(),
                Some("the microphone did not come up"),
                "an open that answers neither way leaves the take's line saying the microphone \
                 did not come up, rather than the page waiting on it for ever",
            );

            // The open answered late, and successfully. The take it began is
            // nobody's - the page gave up on it - so it must not be left
            // holding the microphone.
            let next = rig.mic.start(&rig.bridge, None, "page", "take-2");
            assert!(
                next.is_ok(),
                "the press after a given-up open records rather than being refused: {next:?}"
            );
            assert_eq!(
                rig.log.lock().unwrap().clone(),
                vec!["open default"],
                "the late open is kept as the hold, with the take nobody owns ended"
            );
        }

        #[test]
        fn an_open_that_answers_is_the_take_it_opened() {
            let rig = Open::new();
            let started = runtime().block_on(rig.mic.start_within(
                &rig.bridge,
                Some("one".to_owned()),
                "page",
                "take-1",
                SETTLE,
            ));
            assert_eq!(
                started.expect("an open that answers is the take").id,
                "one",
                "the bounded open hands a take that came up its own device back"
            );
        }

        #[test]
        fn a_device_that_will_not_open_keeps_its_own_words() {
            let rig = Open::refusing("no input device named \"gone\"");
            let refused = runtime().block_on(rig.mic.start_within(
                &rig.bridge,
                Some("gone".to_owned()),
                "page",
                "take-1",
                SETTLE,
            ));
            assert_eq!(
                refused.err().as_deref(),
                Some("no input device named \"gone\""),
                "only an open that answers neither way draws the bound's sentence: a device \
                 that will not open says why itself"
            );
        }

        /// A signal whose bound is spent but whose flag is unset: the
        /// scheduling lag between the deadline and the task waking to set it.
        fn spent_signal() -> GivenUp {
            GivenUp {
                flag: Arc::new(AtomicBool::new(false)),
                deadline: Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .expect("the clock has run past a second"),
            }
        }

        #[test]
        fn an_open_that_answers_past_the_deadline_is_given_up_on_without_the_flag() {
            let rig = Open::new();
            let opened =
                rig.mic.start_inner(&rig.bridge, None, "page", "take-1", Some(&spent_signal()));
            assert!(
                opened.is_err(),
                "an open answering past the deadline is the caller's lost attempt: {opened:?}"
            );
            let next = rig.mic.start(&rig.bridge, None, "page", "take-2");
            assert!(
                next.is_ok(),
                "the take nobody owns is ended rather than left recording: {next:?}"
            );
        }

        /// The flag is the caller's own word, and a paused clock is the one
        /// shape where only it can be read: tokio's clock is what paces the
        /// bound and it moves only where this test moves it, so the bound is
        /// spent - the flag set - while `Instant::now()` has barely moved.
        #[test]
        fn the_give_up_the_bound_sets_is_read_when_the_clock_is_paused() {
            let rig = Wedged::new();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("a runtime");
            let refused = runtime.block_on(async {
                tokio::join!(
                    rig.mic.start_within(&rig.bridge, None, "page", "take-1", SETTLE),
                    async {
                        rig.entered.await.expect("the opener is entered");
                        // Advanced by hand: the open holds a blocking task, and
                        // the clock never auto-advances past one.
                        tokio::time::advance(SETTLE).await;
                    },
                )
                .0
            });
            rig.release.send(()).expect("the open is parked");

            assert_eq!(
                refused.err().as_deref(),
                Some("the microphone did not come up"),
                "a bound the clock spends is a give-up like any other"
            );
            let next = rig.mic.start(&rig.bridge, None, "page", "take-2");
            assert!(
                next.is_ok(),
                "the flag the bound set is what ends the take nobody owns: {next:?}"
            );
        }

        #[test]
        fn a_spent_deadline_does_not_begin_a_warm_take() {
            let rig = Open::new();
            rig.mic
                .hold(&rig.bridge, None, "page")
                .expect("the hold opens the device")
                .expect("held");
            let opened =
                rig.mic.start_inner(&rig.bridge, None, "page", "take-1", Some(&spent_signal()));
            assert!(
                opened.is_err(),
                "a warm capture answers at once, so a spent deadline is a take that waited out \
                 a wedged open: {opened:?}"
            );
            let next = rig.mic.start(&rig.bridge, None, "page", "take-2");
            assert!(
                next.is_ok(),
                "the take nobody owns is ended rather than left recording: {next:?}"
            );
        }

        /// The tail the read has to come after: a press that changes the pick
        /// installs its own capture and lets the held one go, and that release
        /// takes the capture thread's own window. The give-up lands inside it,
        /// and the take nobody owns must still end.
        #[test]
        fn a_give_up_landing_while_the_old_capture_is_let_go_still_ends_the_take() {
            let rig = Swap::new();
            rig.mic
                .hold(&rig.bridge, Some("one".to_owned()), "page")
                .expect("the hold opens")
                .expect("held");
            let signal = GivenUp {
                flag: Arc::new(AtomicBool::new(false)),
                deadline: Instant::now() + SETTLE,
            };
            let flag = Arc::clone(&signal.flag);

            // The press runs on its own thread: it blocks inside the release,
            // which is the window this test holds open.
            let pressing = {
                let mic = Arc::clone(&rig.mic);
                let bridge = Arc::clone(&rig.bridge);
                std::thread::spawn(move || {
                    mic.start_inner(
                        &bridge,
                        Some("two".to_owned()),
                        "page",
                        "take-1",
                        Some(&signal),
                    )
                })
            };
            rig.letting.recv_timeout(SETTLE).expect("the release is entered");
            // The caller gives up while the release is inside its window.
            flag.store(true, Ordering::Release);
            rig.let_go.send(()).expect("the release is parked");

            let outcome = pressing.join().expect("the press finishes");
            assert_eq!(
                outcome.err().as_deref(),
                Some("the microphone did not come up"),
                "a give-up landing in the release's window is still the caller's word"
            );
            let next = rig.mic.start(&rig.bridge, Some("two".to_owned()), "page", "take-2");
            assert!(
                next.is_ok(),
                "the take nobody owns is ended rather than left recording: {next:?}"
            );
        }
    }
}
