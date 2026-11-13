//! One profile's browser host: the process behind `--cef-host`.
//!
//! Arguments name the profile's cache root, the DevTools port and the frame
//! ring. The host initializes CEF, creates one windowless browser per tab,
//! reports that it is live in the ring's header, and pumps CEF's work and a
//! 60 Hz external begin-frame cadence until it is ended. The server owns the
//! record on disk; this process writes nothing beside the profile beyond
//! what Chromium itself does.

#![allow(
    unsafe_code,
    reason = "the signal handler and the ring header are this process's C entry points"
)]
#![allow(
    clippy::transmute_ptr_to_ptr,
    reason = "CEF's wrap_* macros transmute the wrapper reference to the base object"
)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use cef::*;

use crate::bridge::{self, Channels};
use crate::engine::{HostApp, elapsed_ms, initialize, pump};
use crate::osr::{Header, Ring, STATE_FAILED, STATE_LIVE};

/// The launch arguments, as the server passes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub profile: PathBuf,
    pub port: u16,
    pub ring: String,
    /// The frame socket the server bound. **Passed, not derived**: a profile
    /// under app-support is longer than a Unix socket's address allows. A
    /// host launched by hand falls back to the profile's own directory.
    pub frames: PathBuf,
    pub width: u32,
    pub height: u32,
    /// The id the server assigned this profile on the frame wire.
    pub profile_id: u16,
    /// Frames encoded per second; paints faster than this are coalesced.
    pub fps: u16,
    /// Lossy WebP quality, 0-100.
    pub quality: u8,
    /// The number of the read end whose write end the spawning server
    /// holds: EOF on it means that process is gone. **Identity, not a pid** -
    /// a recycled pid and an unreaped zombie both answer `kill(pid, 0)` as
    /// alive, while this read end is tied to the process holding the write
    /// end (#2499). `None` for a host a person launched directly.
    pub parent: Option<std::os::fd::RawFd>,
}

/// Parse `--cef-host --profile DIR --port N --ring NAME [--frames PATH]
/// [--size WxH] [--profile-id N] [--fps N] [--quality N] [--parent-pipe FD]`.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Option<Args> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("--cef-host") {
        return None;
    }
    let mut profile = None;
    let mut port = None;
    let mut ring = None;
    let mut frames = None;
    let mut size = (1100_u32, 620_u32);
    let mut profile_id = 0_u16;
    let mut fps = 30_u16;
    let mut quality = 75_u8;
    let mut parent = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => profile = args.next().map(PathBuf::from),
            "--port" => port = args.next().and_then(|value| value.parse::<u16>().ok()),
            "--ring" => ring = args.next(),
            "--frames" => frames = args.next().map(PathBuf::from),
            "--parent-pipe" => {
                // **The read end the server handed over.** Anything below
                // the standard streams is a fd the spawn dup'd for this
                // host; `0` would be stdin and is refused rather than read.
                let fd = args.next()?.parse::<std::os::fd::RawFd>().ok()?;
                if fd < 3 {
                    return None;
                }
                parent = Some(fd);
            }
            "--profile-id" => {
                profile_id = args.next().and_then(|value| value.parse().ok()).unwrap_or(profile_id);
            }
            "--fps" => fps = args.next().and_then(|value| value.parse().ok()).unwrap_or(fps),
            "--quality" => {
                quality = args.next().and_then(|value| value.parse().ok()).unwrap_or(quality);
            }
            "--size" => {
                size = args
                    .next()
                    .and_then(|value| {
                        let (width, height) = value.split_once('x')?;
                        Some((width.parse().ok()?, height.parse().ok()?))
                    })
                    .unwrap_or(size);
            }
            _ => return None,
        }
    }
    let profile = profile?;
    Some(Args {
        frames: frames.unwrap_or_else(|| crate::channel::socket_path(&profile)),
        profile,
        port: port?,
        ring: ring?,
        width: size.0,
        height: size.1,
        profile_id,
        fps,
        quality,
        parent,
    })
}

/// Set by SIGTERM/SIGINT, read by the pump loop.
static STOP: AtomicBool = AtomicBool::new(false);

/// The most pointer gestures that may wait for the pump before the oldest
/// goes. A wedged pump is a dead host; the queue must not grow without end.
const MAX_INPUT_BACKLOG: usize = 1024;

/// The visibility calls one show cue makes, as a trait so the acknowledgment
/// beside them is testable without a browser.
trait ShowTarget {
    fn show_page(&self);
}

impl ShowTarget for BrowserHost {
    fn show_page(&self) {
        self.was_hidden(0);
        self.set_focus(1);
        // **A still page does not paint on its own.** The encoder was paused
        // with no viewer, so a viewer that attaches to a page that has not
        // changed since would sit on a blank surface without this.
        self.invalidate(PaintElementType::VIEW);
    }
}

/// Which show cue the loop has applied. Zero, not the header's current value:
/// a cue written while the loop was still starting is acted on by the first
/// pass rather than mistaken for the state it started in.
#[derive(Default)]
struct ShowCue {
    last: u64,
}

impl ShowCue {
    /// The cue to apply, when this loop has not handled it yet.
    fn take(&mut self, show: u64) -> Option<u64> {
        if show == self.last {
            return None;
        }
        self.last = show;
        Some(show)
    }
}

/// Apply one cue: the visibility calls on every tab, then the acknowledgment,
/// only with a host to make them. A profile with no browser host cannot be
/// shown, so its cue is left unacknowledged for the caller's bound.
fn apply_show(header: &Header, show: u64, targets: &[&dyn ShowTarget]) {
    if targets.is_empty() {
        return;
    }
    for target in targets {
        target.show_page();
    }
    header.show_applied.store(show, Ordering::Release);
    eprintln!("cef host: page shown before a call (event_name cef_page_shown, seq {show})");
}

extern "C" fn stop_on_signal(_signal: i32) {
    STOP.store(true, Ordering::Release);
}

/// **A kill is a shutdown, not a stop.** The server ends a host with a
/// signal; without a handler that is an immediate death, and Chromium's
/// cookie and storage writes since its last flush (up to half a minute) go
/// with it. The handler only raises a flag; the loop closes the browser and
/// calls CEF's own shutdown.
fn stop_on_termination() {
    // SAFETY: the handler stores an atomic and does nothing else.
    unsafe {
        libc::signal(libc::SIGTERM, stop_on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop_on_signal as *const () as libc::sighandler_t);
    }
}

/// How often the loop asks whether the server that spawned it is still there.
///
/// **A whole second, not every tick.** The watch is a backstop for a server
/// that died without running its exit, so a second of lag is a second of a
/// host nothing drives; a syscall per millisecond would be a cost every host
/// pays for a case almost none meet.
const PARENT_CHECK_EVERY_MS: u64 = 1000;

/// A pipe pair for tests that drive the watch directly: the read end is the
/// host's, the write end stands in for the server holding it.
#[cfg(test)]
fn parent_watch_pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    let mut fds = [0_i32; 2];
    // SAFETY: pipe writes two fresh fds into the array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "the test's pipe opens");
    // SAFETY: both fds are fresh and owned by the caller from here on.
    unsafe {
        (std::os::fd::FromRawFd::from_raw_fd(fds[0]), std::os::fd::FromRawFd::from_raw_fd(fds[1]))
    }
}

/// The pipe end the spawning server handed over, and when the loop last
/// asked after it.
///
/// **Identity, not a pid.** The read end was inherited across exec from the
/// spawn line, and the write end lives in the server for as long as that
/// process does: EOF is causally tied to that process, where `kill(pid, 0)`
/// answers alive for a recycled pid and for a zombie alike (#2499).
struct ParentWatch {
    parent: Option<std::os::fd::OwnedFd>,
    last: u64,
}

impl ParentWatch {
    fn new(parent: Option<std::os::fd::OwnedFd>) -> Self {
        if let Some(fd) = &parent {
            // Non-blocking, or the first read would wait for a write this
            // pipe never carries: the server holds the write end and writes
            // nothing.
            // SAFETY: fcntl on a fd this process owns for its whole life.
            unsafe {
                let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
                if flags >= 0 {
                    libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }
        Self { parent, last: 0 }
    }

    /// The read end's number, for the line the stop prints.
    fn fd(&self) -> Option<std::os::fd::RawFd> {
        self.parent.as_ref().map(std::os::fd::AsRawFd::as_raw_fd)
    }

    /// Whether the server is gone, asked at most once per interval.
    ///
    /// A read of `0` is EOF - the last copy of the write end closed with the
    /// server. `EAGAIN` is nothing to read, which is the normal answer while
    /// it lives. Neither pid reuse nor a zombie can make this answer wrong,
    /// because no number is involved.
    fn gone(&mut self, now: u64) -> bool {
        let Some(parent) = self.parent.as_ref() else {
            return false;
        };
        if now.saturating_sub(self.last) < PARENT_CHECK_EVERY_MS {
            return false;
        }
        self.last = now;
        let mut byte = [0_u8; 1];
        // SAFETY: reading one byte from a live fd into a one-byte buffer.
        let read = unsafe { libc::read(parent.as_raw_fd(), byte.as_mut_ptr().cast(), byte.len()) };
        match read {
            0 => true,
            -1 => std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN),
            // The server writes nothing, so a byte is a surprise rather than
            // a death; the next interval asks again.
            _ => false,
        }
    }
}

/// Why the loop is stopping.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    Signal,
    ServerGone,
}

/// One decision for both shutdowns, so both take the same exit: the browser
/// closes and CEF's store flushes, rather than a process that simply stops.
fn stopping(stop: &AtomicBool, watch: &mut ParentWatch, now: u64) -> Option<Stop> {
    if stop.load(Ordering::Acquire) {
        return Some(Stop::Signal);
    }
    watch.gone(now).then_some(Stop::ServerGone)
}

/// **The host's first act, before CEF initializes.**
fn begin() {
    crate::client_application::setup();
}

/// One painted view waiting for the encoder: the pixels copied out of CEF's
/// buffer, because that buffer is the paint callback's alone.
struct PaintJob {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    /// The page's own viewport this paint was laid out at.
    page: (u32, u32),
    /// Not a repaint of the same view: a popup appearing or closing.
    distinct: bool,
}

/// Whether a paint replaces the one already waiting for the encoder.
///
/// **The newest paint wins, except over a queued popup frame.** CEF paints
/// the view without the menu right after a popup, so replacing the popup
/// before the encoder takes it loses the menu a second way.
fn replaces_queued(held: Option<&PaintJob>, distinct: bool) -> bool {
    !held.is_some_and(|job| job.distinct) || distinct
}

/// The job the encoder sends now: **a queued popup first, otherwise the
/// newest paint**. CEF paints the view without the menu right after a popup,
/// so taking only the newest would lose the menu; for plain paints the newest
/// is the state the person is waiting for, and the older queued one is a
/// superseded frame.
fn take_for_encode(queue: &mut std::collections::VecDeque<Job>) -> Option<Job> {
    if matches!(queue.front(), Some(Job::Frame(next)) if next.distinct) {
        return queue.pop_front();
    }
    let newest = queue.iter().rposition(|job| matches!(job, Job::Frame(_)));
    newest.and_then(|at| queue.remove(at))
}

/// What the worker encodes: **the frame it already holds if that one is a
/// popup** (it was the oldest paint, and the view without the menu follows
/// it), otherwise the newest queued frame, otherwise the held one.
fn encode_target(held: PaintJob, newest: Option<Job>) -> PaintJob {
    if held.distinct {
        return held;
    }
    match newest {
        Some(Job::Frame(next)) => next,
        _ => held,
    }
}

/// One thing waiting for the host's channel writer: a frame to encode, or a
/// packet of the page's audio to pass on.
enum Job {
    Frame(PaintJob),
    Audio(crate::channel::Audio),
}

/// One tab's frame queue and the wake its worker waits on. **A tab owns its
/// own queue and encoder**, so a busy background tab cannot starve the tab a
/// person is looking at.
type Pending = (Mutex<VecDeque<Job>>, Condvar);

/// One tab: a windowless browser, its own client, and the frame stream it
/// paints. The tab's number is the server's stable name for it.
struct Tab {
    browser: Browser,
    /// The tab whose page opened this one; `None` for a tab the host made.
    opener: Option<u16>,
    /// Whether this tab's main frame has started a load; see `TabParts`.
    loaded: Arc<AtomicBool>,
    /// This popup's place in its opener's count, given back when it ends.
    place: Option<Arc<Place>>,
    pending: Arc<Pending>,
    /// Ends the tab's frame worker when the tab goes.
    stop: Arc<AtomicBool>,
    /// A viewer's key cue: the tab's next frame is full quality.
    key: Arc<AtomicBool>,
    /// The size this tab lays out at; the profile's request unless a take
    /// has laid this one out apart.
    size: Arc<crate::osr::ViewSize>,
}

/// The profile's open tabs, numbered in creation order and never renumbered.
#[derive(Default)]
struct Tabs {
    next: u16,
    open: BTreeMap<u16, Tab>,
}

impl Tabs {
    /// The number a tab takes. **Reserved before its browser exists**, because
    /// the browser is created synchronously and CEF runs a nested message loop
    /// inside that call: a page's popup can be admitted and given a number
    /// while the create is still in progress, and a number peeked beforehand
    /// would be handed out twice. A create that fails gives its number back
    /// through `unreserve` unless a nested admission has taken a newer one.
    ///
    /// **`None` when every number is spent.** The number's width is the
    /// wire's, so the counter cannot widen, and handing out a live tab's
    /// number again would replace that tab; the caller refuses the tab
    /// instead of inserting over one.
    fn reserve(&mut self) -> Option<u16> {
        let tab = self.next.checked_add(1)?;
        self.next = tab;
        Some(tab)
    }

    /// Give back a number whose browser was never created. **Only the newest
    /// reservation goes**: the create runs a nested loop, and a popup admitted
    /// inside it has already taken the next number, so this leaves the gap
    /// rather than rewinding over a live reservation.
    fn unreserve(&mut self, tab: u16) {
        if self.next == tab && !self.open.contains_key(&tab) {
            self.next = tab.saturating_sub(1);
        }
    }

    /// Take one tab out and end its frame worker. The page itself may be what
    /// closed it - an OAuth popup calls `window.close()` when it is done - so
    /// the row goes here as surely as when the host closed it.
    fn end(&mut self, tab: u16) -> Option<Tab> {
        let removed = self.open.remove(&tab)?;
        removed.stop.store(true, Ordering::Release);
        removed.pending.1.notify_all();
        Some(removed)
    }
}

/// The most popups one page may hold open at once. **A page cannot grow this
/// without end**: every popup is a browser of its own.
const MAX_POPUP_TABS: usize = 16;

/// **The gate key for a create fault that belongs to no page.** `Tabs::reserve`
/// starts at 1 and never returns 0, so this is stable where a requested tab's
/// number is not: `unreserve` rewinds the counter, and a popup admitted inside
/// the create's nested loop advances it.
const NO_OPENER: u16 = 0;

/// The refusals a popup that was admitted resolves. **Both admission paths
/// share this list**, so a kind added here is cleared by each of them, and a
/// page is past what opening one hit.
const ADMISSION_REFUSALS: [&str; 4] = ["bridged", "unbound", "create_failed", "subframe"];

/// How long a popup is given to start its own first document before the host
/// starts one. **Long enough to lose a race with nothing**: a popup that
/// commits on its own does so within a frame or two, and one still empty at
/// this point never will.
const POPUP_BOOT_GRACE_MS: u64 = 250;

/// How many navigations the host offers one popup. **Not one**: a load the
/// engine drops on the first offer is still started by the second, and a
/// popup that never starts at all must not be retried for ever.
const POPUP_BOOT_TRIES: u8 = 8;

/// How long a popup is given to start the document it asked for. **A bound on
/// the whole start, not on one navigation**: a popup that never gets there
/// holds its birth forever, and every reconcile then pays a driver recovery
/// for a tab that will never pair.
const POPUP_BOOT_GIVE_UP_MS: u64 = 10_000;

/// The host's recording, shared by the paint callback and the input pump.
///
/// **The file is written here, in the process that paints**, so the frames it
/// holds are the composed viewport itself rather than a re-encode of what a
/// socket carried.
struct Recording {
    running: AtomicBool,
    /// The tab whose paints the take records; 0 means no take.
    tab: AtomicU16,
    slot: crate::record::FrameSlot,
    held: Mutex<Option<crate::record::Recorder>>,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(false),
            tab: AtomicU16::new(0),
            slot: Arc::new(Mutex::new(None)),
            held: Mutex::new(None),
        })
    }

    /// The recording takes one painted viewport of the tab that started it.
    /// **Only the recording's own size is kept**: a controller resize while
    /// the take runs is a frame the file does not need.
    fn offer(&self, tab: u16, view: &[u8], width: u32, height: u32) {
        if !self.running.load(Ordering::Acquire)
            || self.tab.load(Ordering::Acquire) != tab
            || width != crate::record::WIDTH
            || height != crate::record::HEIGHT
        {
            return;
        }
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(view.to_vec()));
    }

    fn start(&self, tab: u16, path: &str) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if held.is_some() {
            crate::control::emit(&crate::control::HostEvent::Recording {
                started: false,
                seconds: 0,
                bytes: 0,
                capped: false,
                error: Some("a recording is already running".to_owned()),
            });
            return;
        }
        match crate::record::Recorder::start(PathBuf::from(path), Arc::clone(&self.slot)) {
            Ok(recorder) => {
                *held = Some(recorder);
                self.tab.store(tab, Ordering::Release);
                self.running.store(true, Ordering::Release);
                crate::control::emit(&crate::control::HostEvent::Recording {
                    started: true,
                    seconds: 0,
                    bytes: 0,
                    capped: false,
                    error: None,
                });
            }
            Err(why) => crate::control::emit(&crate::control::HostEvent::Recording {
                started: false,
                seconds: 0,
                bytes: 0,
                capped: false,
                error: Some(why),
            }),
        }
    }

    fn stop(&self) {
        let taken = self.held.lock().unwrap_or_else(PoisonError::into_inner).take();
        self.running.store(false, Ordering::Release);
        self.tab.store(0, Ordering::Release);
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let Some(recorder) = taken else {
            crate::control::emit(&crate::control::HostEvent::Recording {
                started: false,
                seconds: 0,
                bytes: 0,
                capped: false,
                error: Some("no recording is running".to_owned()),
            });
            return;
        };
        // **Finishing a file is not the pump's work.** The writer flushes on
        // its own thread and answers the server when the MP4 is complete.
        std::thread::spawn(move || {
            let outcome = recorder.stop();
            crate::control::emit(&crate::control::HostEvent::Recording {
                started: false,
                seconds: outcome.seconds,
                bytes: outcome.bytes,
                capped: outcome.capped,
                error: outcome.error,
            });
        });
    }
}

/// The most jobs a slow channel may hold before audio is dropped.
///
/// **Audio gives way; a frame does not.** A frame has a newest-wins slot, so
/// one that waits is replaced by a later one; audio packets are a stream, and
/// a backlog only ever arrives late - past the cap the newest packet goes and
/// the client hears a gap rather than the whole stream sliding behind.
const MAX_PENDING: usize = 512;

/// One tab's frame queue and the sinks that fill it. **Every tab owns its
/// own**, whether the host made it or CEF made it for a page's popup, so a
/// popup's paints never reach the page that opened it.
struct TabParts {
    pending: Arc<Pending>,
    stop: Arc<AtomicBool>,
    key: Arc<AtomicBool>,
    size: Arc<crate::osr::ViewSize>,
    /// Whether CEF has started a load for this tab's main frame. **The
    /// popup bootstrap reads it**: a popup that started its own navigation
    /// must not be navigated again.
    loaded: Arc<AtomicBool>,
    paint: crate::osr::PaintSink,
    render: RenderHandler,
    page_audio: AudioHandler,
    downloads: DownloadHandler,
}

/// One popup's place in its opener's count.
///
/// **A place is released once.** A popup CEF aborted, one whose browser
/// closed and one the host gave up are different reports of the same place,
/// and a second release would free a place another popup still holds.
struct Place {
    opener: u16,
    released: AtomicBool,
}

/// One popup whose first document the host has not finished starting: the
/// tab, the address it asked for, when to look again, how many navigations
/// are left, and the point at which the tab is given up.
struct PopupBoot {
    tab: u16,
    url: String,
    due: u64,
    tries: u8,
    give_up_at: u64,
}

/// What one popup's boot tick does.
#[derive(Debug, PartialEq, Eq)]
enum BootStep {
    /// Start the address the popup asked for.
    Address,
    /// Look again after the grace.
    Wait,
    /// The popup is where it was asked to be, or a document of its own is up.
    Done,
    /// The popup never got there; give the tab up.
    GiveUp,
}

/// Everything one boot tick decides on, as the tab reports it.
struct BootNow<'a> {
    committed: &'a str,
    /// A document or a load has started: `loaded` or a committed URL.
    started: bool,
    /// A navigation is in flight.
    loading: bool,
    tries: u8,
    now: u64,
    give_up_at: u64,
    target: &'a str,
}

/// **The popup's own start, as one decision.** The address is the one
/// navigation this engine commits for a popup an auto-attached client holds,
/// so it is offered while nothing is in flight and never over a load; a unit
/// test pins that, and the give-up, rather than only a live host.
fn boot_step(state: &BootNow<'_>) -> BootStep {
    // **Done is done, whatever the clock says.** A popup that reached its
    // document inside the last grace is not given up for being late.
    if state.started
        && (state.committed == state.target
            || (!state.committed.is_empty() && state.committed != "about:blank"))
    {
        return BootStep::Done;
    }
    // **A load in flight is kept while it is in flight.** Closing a sign-in
    // page because a clock ran out is worse than a tab still loading.
    if state.loading {
        return BootStep::Wait;
    }
    // **The bound ends a popup with nothing in flight**: one that never
    // started, or one whose offers are spent.
    if state.now >= state.give_up_at || state.tries == 0 {
        return BootStep::GiveUp;
    }
    BootStep::Address
}

/// One popup the gate admitted, before CEF has created its browser: the tab
/// it takes, and the frame stream it owns from the moment it paints.
#[derive(Clone)]
struct AdmittedPopup {
    tab: u16,
    opener: u16,
    popup_id: i32,
    place: Arc<Place>,
    url: String,
    pending: Arc<Pending>,
    stop: Arc<AtomicBool>,
    key: Arc<AtomicBool>,
    size: Arc<crate::osr::ViewSize>,
    loaded: Arc<AtomicBool>,
}

impl AdmittedPopup {
    /// **The popup is a tab, not a window.** Left alone, CEF creates the
    /// popup as a native window, which a host with no screen cannot show; the
    /// popup lays out at the profile's size like every other tab.
    fn window_info(&self, info: &mut WindowInfo) {
        info.windowless_rendering_enabled = 1;
        info.external_begin_frame_enabled = 1;
        let (width, height) = self.size.dip();
        info.bounds = Rect { x: 0, y: 0, width, height };
    }
}

/// The host's tab admission: CEF's same-origin popups, and the browsers this
/// host creates for a server command or a cross-site open.
///
/// **For a same-origin popup, CEF creates the browser itself.** A host that
/// cancels the popup and opens a tab in its place leaves `window.open`
/// answering null, and every sign-in flow reads null as "the pop-up was
/// blocked" - the tab it substituted never helps, because the page has already
/// failed. The gate admits the popup, hands CEF a client of its own so it
/// paints and takes input like any other tab, and registers the browser as a
/// tab the moment CEF reports it created.
///
/// **A cross-site open never takes that path**; it creates a browser through
/// `create_browser`, and its two halves and origins live in
/// [`crate::bridge`].
struct PopupGate {
    ring: Arc<Ring>,
    paused: Arc<AtomicBool>,
    recording: Arc<Recording>,
    profile: u16,
    frames: PathBuf,
    fps: u16,
    quality: u8,
    downloads_dir: PathBuf,
    tabs: Arc<Mutex<Tabs>>,
    /// The popups each opener tab holds, counted from admission. **Not from
    /// the open rows**: a popup has no row until CEF reports its browser
    /// created, so one renderer turn could admit any number of them before
    /// the first row exists.
    live: Mutex<HashMap<u16, usize>>,
    /// The places handed out and not yet released, by the opener and the id
    /// CEF gave the popup, so an abort frees exactly the place it names.
    places: Mutex<HashMap<(u16, i32), Arc<Place>>>,
    /// Popups whose first document is still the host's to start.
    boot: Mutex<Vec<PopupBoot>>,
    /// Popup births taken while a tab is being created, and how many creates
    /// hold them. **A create's browser is made inside a nested CEF loop**, so
    /// a popup adopted during it would report before the tab the create is
    /// making, and the service pairs births to pages by that order. **The
    /// depth is counted**: a bridged popup's create can run inside another
    /// create's nested loop, and only the outermost hold may flush.
    held_births: Mutex<Vec<crate::control::HostEvent>>,
    holding_births: AtomicUsize,
    /// The cross-site `window.open` bridges this host holds.
    bridge: Mutex<Channels>,
    /// The popups this host created for a page, `noopener` ones included.
    /// **A noopener popup has no channel**, so this is what still lets its
    /// renderer death end its tab instead of leaving a dead row.
    bridged_tabs: Mutex<HashSet<u16>>,
    /// Refusals already logged, by tab or opener and kind. **One line per
    /// reason, not one per attempt**: a page can repeat a refused open, and
    /// the host's stderr is an append-only file nobody rotates.
    refused_logged: Mutex<HashSet<(u16, &'static str)>>,
    /// A bridged popup whose blank document has not acknowledged its role,
    /// with the address to start once it does. **The address waits**: the
    /// popup's first document must have `window.opener` before its own
    /// scripts, and the role is only established once the renderer says so.
    pending_start: Mutex<Vec<PendingStart>>,
    /// Tabs whose document has already acknowledged its role. **The ack can
    /// land before the pending start is registered** (the blank context is
    /// created inside the create's nested loop), so it is a level, not an
    /// edge: a start registered later checks this set and goes at once.
    assigned_tabs: Mutex<HashSet<u16>>,
}

/// One popup waiting for its role before the host starts its address.
struct PendingStart {
    tab: u16,
    url: String,
    at: u64,
}

/// Ends a birth hold, flushing what it kept, however the create ended.
struct BirthHold<'a> {
    popups: &'a PopupGate,
}

impl Drop for BirthHold<'_> {
    fn drop(&mut self) {
        let _ = self.popups.flush_births();
    }
}

impl PopupGate {
    /// One tab's own frame queue and sinks.
    fn parts(&self, tab: u16) -> TabParts {
        let pending: Arc<Pending> = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let key = Arc::new(AtomicBool::new(false));
        let paint: crate::osr::PaintSink = {
            let pending = Arc::clone(&pending);
            let paused = Arc::clone(&self.paused);
            let recording = Arc::clone(&self.recording);
            Arc::new(move |view, width, height, distinct, page| {
                // **A recording is not the viewers'.** A take runs whether or
                // not anyone is watching, so it is offered the viewport
                // before the encoding pause.
                recording.offer(tab, view, width, height);
                if paused.load(Ordering::Acquire) {
                    return;
                }
                push_paint(&pending, view, width, height, distinct, page);
            })
        };
        let audio: crate::audio::AudioSink = {
            let pending = Arc::clone(&pending);
            Arc::new(move |packet: crate::channel::Audio| push_audio(&pending, packet))
        };
        let size = crate::osr::ViewSize::of_ring(&self.ring);
        let render = crate::osr::handler_for_tab(
            Arc::clone(&self.ring),
            Some(Arc::clone(&paint)),
            Arc::clone(&size),
        );
        let page_audio = crate::audio::handler(self.profile, tab, audio);
        let downloads = crate::download::handler(&self.downloads_dir, tab);
        let loaded = Arc::new(AtomicBool::new(false));
        TabParts { pending, stop, key, size, loaded, paint, render, page_audio, downloads }
    }

    /// The client one tab runs on; a popup's carries its own admission so CEF's
    /// `on_after_created` can hand the browser back to the pump.
    fn client(
        self: &Arc<Self>,
        tab: u16,
        parts: &TabParts,
        popup: Option<AdmittedPopup>,
    ) -> Client {
        HostClient::new(
            tab,
            Arc::clone(&self.ring),
            Arc::clone(&parts.paint),
            parts.render.clone(),
            parts.page_audio.clone(),
            parts.downloads.clone(),
            Arc::clone(&parts.loaded),
            Arc::clone(self),
            FocusState::new(tab),
            popup,
        )
    }

    /// Admit one popup: the number its tab takes and the client CEF creates
    /// the popup's browser with, or `None` to cancel it.
    fn admit(
        self: &Arc<Self>,
        opener: u16,
        popup_id: i32,
        url: &str,
    ) -> Option<(Client, AdmittedPopup)> {
        {
            // **The page's own count, from the reservation.** Another tab's
            // popups never refuse this one's sign-in, and a burst of
            // `window.open` in one renderer turn cannot outrun a count that
            // waits for a tab row.
            let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            let held = live.entry(opener).or_default();
            if *held >= MAX_POPUP_TABS {
                if let Some(line) =
                    self.cap_refusal(opener, u64::try_from(popup_id).unwrap_or_default(), url)
                {
                    eprintln!("{line}");
                }
                return None;
            }
            *held += 1;
        }
        let place = Arc::new(Place { opener, released: AtomicBool::new(false) });
        // **A host whose numbers are spent refuses the popup**, and the place
        // it took goes back with it: a place that is never released is a slot
        // the opener's page never gets again.
        let Some(tab) = self.tabs.lock().unwrap_or_else(PoisonError::into_inner).reserve() else {
            self.release(&place);
            if let Some(line) = self.exhausted_line(Some(opener), url) {
                eprintln!("{line}");
            }
            return None;
        };
        // **A popup admitted resolves the refusals a page hit opening one**,
        // so its next one writes a line. The cap is not one of them: a freed
        // place ends that episode, not the admission that refills it. Only an
        // admitted popup reaches this: a refused one resolved nothing.
        for kind in ADMISSION_REFUSALS {
            self.clear_refusal(opener, kind);
        }
        // **A replaced place is released before its key is taken.** The ids
        // CEF hands out are unique today, and a repeat would otherwise leak
        // the replaced reservation for the host's whole life.
        let replaced = self
            .places
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((opener, popup_id), Arc::clone(&place));
        if let Some(replaced) = replaced {
            self.release(&replaced);
        }
        let parts = self.parts(tab);
        let admitted = AdmittedPopup {
            tab,
            opener,
            popup_id,
            place,
            url: url.to_owned(),
            pending: Arc::clone(&parts.pending),
            stop: Arc::clone(&parts.stop),
            key: Arc::clone(&parts.key),
            size: Arc::clone(&parts.size),
            loaded: Arc::clone(&parts.loaded),
        };
        let client = self.client(tab, &parts, Some(admitted.clone()));
        Some((client, admitted))
    }

    /// Register a popup browser CEF created as a tab: **the browser is CEF's,
    /// the tab and its frame stream are this host's.** CEF reports the browser
    /// safe to drive here, so the tab is made now rather than queued for a
    /// later pass; a popup that closes itself at once is then a row that ends
    /// instead of one that never existed.
    fn adopt(&self, browser: Browser, admitted: AdmittedPopup) {
        let AdmittedPopup { tab, opener, popup_id, place, url, pending, stop, key, size, loaded } =
            admitted;
        // **The popup exists, so its place is the tab's now.**
        self.places.lock().unwrap_or_else(PoisonError::into_inner).remove(&(opener, popup_id));
        // **The number was reserved at admission**, so an open row here is a
        // pairing bug; the browser is closed rather than left as a tab
        // nothing paints.
        if self.tabs.lock().unwrap_or_else(PoisonError::into_inner).open.contains_key(&tab) {
            eprintln!(
                "cef host: a page's popup was not registered (event_name \
                 cef_popup_not_registered, tab {tab}, opener {opener}, url {url})"
            );
            self.release(&place);
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
            return;
        }
        if let Some(host) = browser.host() {
            // The same calls a created tab makes, plus the resize: CEF sized
            // the popup to what `window.open` asked for, and it lays out at
            // this profile's view like every other tab.
            host.was_hidden(0);
            host.set_focus(1);
            host.set_windowless_frame_rate(60);
            host.notify_screen_info_changed();
            host.was_resized();
        }
        // **A popup a DevTools client is auto-attached to never starts its
        // own first document.** Measured: the target appears as `other`, its
        // URL stays empty and every page-domain command times out, while the
        // same popup in a host with no attached client runs. The pump starts
        // the address the popup asked for, and that is the one navigation
        // this engine commits for such a popup; see `bootstrap`.
        let now = elapsed_ms();
        self.boot.lock().unwrap_or_else(PoisonError::into_inner).push(PopupBoot {
            tab,
            url: url.clone(),
            due: now.saturating_add(POPUP_BOOT_GRACE_MS),
            tries: POPUP_BOOT_TRIES,
            give_up_at: now.saturating_add(POPUP_BOOT_GIVE_UP_MS),
        });
        {
            let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.insert(
                tab,
                Tab {
                    browser,
                    opener: Some(opener),
                    loaded: Arc::clone(&loaded),
                    place: Some(place),
                    pending: Arc::clone(&pending),
                    stop: Arc::clone(&stop),
                    key: Arc::clone(&key),
                    size: Arc::clone(&size),
                },
            );
        }
        start_tab_encoder(
            pending,
            stop,
            crate::frame::Sink::new(self.profile, tab, self.frames.clone(), self.fps, self.quality),
            Arc::clone(&self.paused),
            key,
        );
        self.emit_birth(crate::control::HostEvent::TabCreated {
            tab,
            opener: Some(opener),
            url,
            request: None,
        });
    }

    /// **A popup adopted while a tab is being created waits its turn.** The
    /// create's browser is made inside a nested CEF loop, so this birth would
    /// otherwise reach the server before the tab the create is making, and
    /// the service counts births against pages in that order.
    fn hold_births(&self) -> BirthHold<'_> {
        self.holding_births.fetch_add(1, Ordering::AcqRel);
        BirthHold { popups: self }
    }

    /// Announce a tab's birth, or keep it while a create holds births.
    fn emit_birth(&self, event: crate::control::HostEvent) {
        if self.holding_births.load(Ordering::Acquire) > 0 {
            self.held_births.lock().unwrap_or_else(PoisonError::into_inner).push(event);
            return;
        }
        crate::control::emit(&event);
    }

    /// **The held births go after the create's own**, in the order they were
    /// adopted, which is the order the engine made them. Answers what it
    /// sent, so a test can hold it to sending rather than only to emptying
    /// the buffer.
    ///
    /// **Only the outermost hold flushes.** A bridged popup's create can run
    /// inside another create's nested loop, and its own hold must not send
    /// the outer create's births early.
    fn flush_births(&self) -> Vec<crate::control::HostEvent> {
        if self.holding_births.fetch_sub(1, Ordering::AcqRel) > 1 {
            return Vec::new();
        }
        let held = {
            let mut held = self.held_births.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *held)
        };
        for event in &held {
            crate::control::emit(event);
        }
        held
    }

    /// Start a popup's own document when CEF has not.
    ///
    /// **The address is the one navigation this engine commits** for a popup
    /// an auto-attached DevTools client holds. Measured: a second navigation
    /// of such a popup - the host's own `load_url`, a driver's `Page.navigate`
    /// or the page's own `location` - never commits (CEF times out waiting
    /// for a browser info response), so the address cannot be preceded by a
    /// blank document either. It is offered while nothing is in flight and
    /// never over a load; a popup that cannot get that far is given up rather
    /// than left blank for ever.
    fn bootstrap(&self, now: u64) {
        // **A popup whose role is acknowledged starts here**, on the pump
        // thread and never inside a create's nested loop. **One whose role is
        // never acknowledged is given up**, or a renderer that never
        // installed its shim would leave a blank tab holding a place for the
        // host's whole life.
        let mut ready: Vec<PendingStart> = Vec::new();
        let mut expired: Vec<PendingStart> = Vec::new();
        {
            let known = self.assigned_tabs.lock().unwrap_or_else(PoisonError::into_inner);
            let mut pending = self.pending_start.lock().unwrap_or_else(PoisonError::into_inner);
            let mut at = 0;
            while at < pending.len() {
                let tab = pending[at].tab;
                if known.contains(&tab) {
                    ready.push(pending.remove(at));
                } else if now.saturating_sub(pending[at].at) >= POPUP_BOOT_GIVE_UP_MS {
                    expired.push(pending.remove(at));
                } else {
                    at += 1;
                }
            }
        }
        for entry in ready {
            self.assigned_tabs.lock().unwrap_or_else(PoisonError::into_inner).remove(&entry.tab);
            self.navigate_tab(entry.tab, &entry.url);
        }
        for entry in expired {
            eprintln!(
                "cef host: a popup never acknowledged its bridge role (event_name \
                 cef_opener_bridge_unstarted, profile {}, tab {}, address {})",
                self.profile,
                entry.tab,
                origin_words(&entry.url),
            );
            self.close_tab(entry.tab);
        }
        let mut due = Vec::new();
        {
            let mut boot = self.boot.lock().unwrap_or_else(PoisonError::into_inner);
            let mut at = 0;
            while at < boot.len() {
                if boot[at].due <= now {
                    due.push(boot.remove(at));
                } else {
                    at += 1;
                }
            }
        }
        for mut entry in due {
            let state = {
                let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
                tabs.open.get(&entry.tab).map(|tab| (tab.browser.clone(), Arc::clone(&tab.loaded)))
            };
            // **A tab that is gone has no birth left to pair.**
            let Some((browser, loaded)) = state else { continue };
            let frame = browser.main_frame();
            let committed = frame
                .as_ref()
                .map(|frame| CefStringUtf16::from(&frame.url()).to_string())
                .unwrap_or_default();
            let target = if entry.url.is_empty() { "about:blank" } else { entry.url.as_str() };
            let step = boot_step(&BootNow {
                committed: &committed,
                started: loaded.load(Ordering::Acquire) || !committed.is_empty(),
                loading: browser.is_loading() != 0,
                tries: entry.tries,
                now,
                give_up_at: entry.give_up_at,
                target,
            });
            match step {
                BootStep::Done => continue,
                BootStep::GiveUp => {
                    self.give_up(&entry);
                    continue;
                }
                BootStep::Wait => {}
                BootStep::Address => {
                    if let Some(frame) = &frame {
                        frame.load_url(Some(&CefString::from(target)));
                        entry.tries -= 1;
                    }
                }
            }
            entry.due = now.saturating_add(POPUP_BOOT_GRACE_MS);
            self.boot.lock().unwrap_or_else(PoisonError::into_inner).push(entry);
        }
    }

    /// **One popup, one place.** Whatever path reports the popup gone, the
    /// first release is the one that counts: a second would free a place a
    /// different popup still holds.
    fn release(&self, place: &Place) {
        if place.released.swap(true, Ordering::AcqRel) {
            return;
        }
        {
            let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(held) = live.get_mut(&place.opener) {
                *held = held.saturating_sub(1);
                if *held == 0 {
                    live.remove(&place.opener);
                }
            }
        }
        // **A freed place is the cap's episode boundary.** The page is under
        // the cap again, so a later refusal is a new episode and its line is
        // worth having; the admission that refills the place does not end the
        // refusal, because the page never left it.
        self.clear_refusal(place.opener, "cap");
    }

    /// Whether this refusal is the first of its kind for the tab or opener.
    /// **One line per reason, not one per attempt**: the host's stderr is an
    /// append-only file. Each reason has its own clear rule - a tab's
    /// navigation or end clears them all; a popup admitted clears the ones
    /// opening one hits; a freed place clears the cap's; a too-large message
    /// stays the document's until it goes.
    fn first_refusal(&self, who: u16, kind: &'static str) -> bool {
        self.refused_logged.lock().unwrap_or_else(PoisonError::into_inner).insert((who, kind))
    }

    /// Drop one refusal reason for a tab, so its next occurrence writes a
    /// line again.
    fn clear_refusal(&self, who: u16, kind: &'static str) {
        self.refused_logged.lock().unwrap_or_else(PoisonError::into_inner).remove(&(who, kind));
    }

    /// The line for a page at its popup cap, when this attempt is the
    /// episode's first. **One line per episode**: a freed place ends it,
    /// because that is the page's only way back under the cap; an admission
    /// refills the place without ending the refusal.
    fn cap_refusal(&self, opener: u16, popup: u64, url: &str) -> Option<String> {
        if !self.first_refusal(opener, "cap") {
            return None;
        }
        Some(format!(
            "cef host: a page's popup was refused (event_name cef_popup_refused, profile {}, \
             opener {opener}, popup {popup}, address {})",
            self.profile,
            origin_words(url)
        ))
    }

    /// The line for a host whose tab numbers are spent, once per opener
    /// document. **A page can keep calling `window.open`**, so the line is
    /// keyed rather than written per attempt; the opener's navigation or end
    /// re-arms it, which is the grain of the document that saw the refusal.
    /// A failed create at the ceiling can still hand its number back through
    /// `unreserve`, so the refusal is not necessarily the host's last.
    fn exhausted_line(&self, opener: Option<u16>, url: &str) -> Option<String> {
        if !self.first_refusal(opener.unwrap_or(NO_OPENER), "exhausted") {
            return None;
        }
        let opener = opener.map_or_else(|| "none".to_owned(), |opener| opener.to_string());
        Some(format!(
            "cef host: no tab number was left (event_name cef_tab_numbers_exhausted, profile {}, \
             opener {opener}, address {})",
            self.profile,
            origin_words(url)
        ))
    }

    /// The line for a browser the host could not create, when this attempt is
    /// the episode's first. **One line per page, not one per attempt**: a
    /// popup's fault is its opener's, and a requested tab's is the host's,
    /// because that tab's number is rewound and reused.
    fn create_failed_line(&self, opener: Option<u16>, tab: u16, url: &str) -> Option<String> {
        if !self.first_refusal(opener.unwrap_or(NO_OPENER), "create_failed") {
            return None;
        }
        let opener = opener.map_or_else(|| "none".to_owned(), |opener| opener.to_string());
        Some(format!(
            "cef host: a tab was not created (event_name cef_tab_create_failed, profile {}, \
             tab {tab}, opener {opener}, address {})",
            self.profile,
            origin_words(url)
        ))
    }

    /// **A requested tab that was created re-arms the create-fault line**: the
    /// fault is over, so a later one is a new episode.
    fn requested_create_succeeded(&self) {
        self.clear_refusal(NO_OPENER, "create_failed");
    }

    /// **A create's whole bookkeeping, from its own outcome.** A created
    /// browser re-arms the host's requested-tab line, so a fault that returns
    /// later is a new episode; a failed one writes its line when the episode
    /// is new. Both branches of `create_browser` call this, so the emission
    /// and the re-arm cannot drift apart. Answers whether it wrote.
    fn note_create(&self, opener: Option<u16>, tab: u16, url: &str, created: bool) -> bool {
        if created {
            if opener.is_none() {
                self.requested_create_succeeded();
            }
            return false;
        }
        let Some(line) = self.create_failed_line(opener, tab, url) else {
            return false;
        };
        eprintln!("{line}");
        true
    }

    /// A message a subframe sends is refused before its arguments are read,
    /// and answers the line to write when it is the first from that tab.
    /// **One line per tab until its main frame navigates, or it ends, or a
    /// popup is admitted from it**: the frame can repeat it, and the host's
    /// stderr is an append-only file.
    ///
    /// The native object belongs to the main frame. A same-origin subframe
    /// can still call the top window's object, but CEF reports that call as
    /// the main frame's, so a page cannot make this line repeat today; it is
    /// the gate a renderer that sends from a subframe meets.
    fn refuse_subframe(&self, tab: u16) -> Option<String> {
        if !self.first_refusal(tab, "subframe") {
            return None;
        }
        Some(format!(
            "cef host: a subframe tried the opener bridge (event_name \
             cef_opener_bridge_subframe, profile {}, tab {tab})",
            self.profile
        ))
    }

    fn clear_refusals(&self, who: u16) {
        self.refused_logged
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(tab, _)| *tab != who);
    }

    /// **A popup CEF never created gives its place back**, or a page that
    /// opened one that failed would hold a count against itself for the life
    /// of the host. The place is the one CEF's own popup id names.
    fn aborted(&self, opener: u16, popup_id: i32) {
        let place =
            self.places.lock().unwrap_or_else(PoisonError::into_inner).remove(&(opener, popup_id));
        if let Some(place) = place {
            self.release(&place);
        }
    }

    /// Give one popup up: **one line naming what the person lost**, then the
    /// tab goes so the service stops waiting on a page that will never exist.
    fn give_up(&self, entry: &PopupBoot) {
        let ended = self.tabs.lock().unwrap_or_else(PoisonError::into_inner).end(entry.tab);
        let Some(ended) = ended else { return };
        let opener = ended.opener.unwrap_or_default();
        eprintln!(
            "cef host: a popup was given up (event_name cef_popup_given_up, profile {}, tab {}, \
             opener {opener}, address {})",
            self.profile,
            entry.tab,
            origin_words(&entry.url),
        );
        if let Some(place) = &ended.place {
            self.release(place);
        }
        self.bridge_tab_ended(entry.tab);
        if let Some(host) = ended.browser.host() {
            host.close_browser(1);
        }
        crate::control::emit(&crate::control::HostEvent::TabClosed { tab: entry.tab });
    }

    /// Drop one tab whose browser CEF is destroying.
    fn forget(&self, tab: u16) {
        let ended = self.tabs.lock().unwrap_or_else(PoisonError::into_inner).end(tab);
        let Some(ended) = ended else { return };
        if let Some(place) = &ended.place {
            self.release(place);
        }
        self.bridge_tab_ended(tab);
        crate::control::emit(&crate::control::HostEvent::TabClosed { tab });
    }

    /// Close one tab, give its place back, and tell the bridge. **The one
    /// close path for a host-created tab**: a call's close, the shim's
    /// `window.close()`, and a bridge peer's `close()`.
    fn close_tab(&self, tab: u16) {
        let ended = self.tabs.lock().unwrap_or_else(PoisonError::into_inner).end(tab);
        let Some(ended) = ended else { return };
        if let Some(place) = &ended.place {
            self.release(place);
        }
        self.bridge_tab_ended(tab);
        if let Some(host) = ended.browser.host() {
            host.close_browser(1);
        }
        crate::control::emit(&crate::control::HostEvent::TabClosed { tab });
    }

    // ---- the opener bridge ------------------------------------------------

    /// Create one browser and register it as a tab. **The one place a browser
    /// is created here**: a server command's tab and a page's cross-site open
    /// both come through it.
    ///
    /// The number is reserved before the create (CEF runs a nested loop
    /// inside it), and births are held for its duration so a popup adopted in
    /// that loop reports after the tab the create is making. `emit_now` is
    /// the requested tab's: its birth goes out ahead of the held popups, so
    /// it is sent directly; a bridged popup's birth rides the hold.
    fn create_browser(
        self: &Arc<Self>,
        opener: Option<u16>,
        url: &str,
        request: Option<u64>,
        place: Option<Arc<Place>>,
        bridged: Option<&(u64, String)>,
        emit_now: bool,
    ) -> Option<u16> {
        let births = self.hold_births();
        // **A host whose numbers are spent refuses the tab** rather than
        // inserting over the one that still holds the last number.
        let Some(tab) = self.tabs.lock().unwrap_or_else(PoisonError::into_inner).reserve() else {
            if let Some(line) = self.exhausted_line(opener, url) {
                eprintln!("{line}");
            }
            return None;
        };
        // **The channel is bound before the browser is created.** The create
        // runs a nested CEF loop, and the popup's first document can post to
        // its `window.opener` inside that loop; a channel bound afterwards
        // would drop that message.
        if let (Some((id, handle)), Some(opener)) = (bridged, opener) {
            let bound = self.bridge.lock().unwrap_or_else(PoisonError::into_inner).bind(
                *id,
                opener,
                tab,
                handle.clone(),
            );
            if let Err(why) = bound {
                // **One line for the page, not one per attempt.** The handle
                // is the page's, so it can repeat the refusal in a loop, and
                // the host's stderr is an append-only file nobody rotates.
                // Keyed by the opener because that is the page whose
                // navigation or admitted popup clears it.
                if self.first_refusal(opener, "unbound") {
                    eprintln!(
                        "cef host: a bridge channel was not bound (event_name \
                         cef_opener_bridge_unbound, profile {}, tab {tab}, opener {opener}): {why}",
                        self.profile
                    );
                }
                // **A refusal the page can repeat must not spend a number**,
                // or a handle that is not ours walks the counter to its u16
                // ceiling and the next insert replaces a live tab.
                self.tabs.lock().unwrap_or_else(PoisonError::into_inner).unreserve(tab);
                return None;
            }
        }
        let parts = self.parts(tab);
        let mut client = self.client(tab, &parts, None);
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            // **The frame source is ours.** With CEF's own scheduling under
            // an external message pump the same page painted at ~26 fps; an
            // explicit 60 Hz cadence paints at 55-60 (the probe's
            // measurement).
            external_begin_frame_enabled: 1,
            ..Default::default()
        };
        // **Opaque, unlike CEF's default.** A page with no background rule is
        // transparent, and the takeover's dark ground would show through it
        // as a blank surface; the app's own background is what the browser
        // fills.
        let settings = BrowserSettings { background_color: 0xFF04_050A, ..Default::default() };
        let mut extra = bridged.and_then(|(id, _)| bridge_extra(*id));
        // **A bridged popup's browser starts blank**, and the host starts the
        // requested address only once a document has acknowledged the role.
        // A `noopener` popup has no role to wait for and starts at the address
        // itself, so it takes no second navigation. The birth still carries
        // the requested address, which is what the service pairs and lists.
        let start = if bridged.is_some() { "about:blank" } else { url };
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&CefString::from(start)),
            Some(&settings),
            extra.as_mut(),
            // **The global context**: the one whose autofill preferences
            // `turn_off_autofill` sets.
            None,
        );
        let host = browser.as_ref().and_then(cef::ImplBrowser::host);
        let (Some(browser), Some(host)) = (browser, host) else {
            // **A create fault can persist across a page's attempts**, and
            // each attempt is a line otherwise; the note keys a popup's on
            // its opener and a requested tab's on the host sentinel.
            self.note_create(opener, tab, url, false);
            // **A channel bound for a browser that never existed goes with
            // it**, or a later popup could post into a channel no tab owns.
            if bridged.is_some() {
                self.bridge.lock().unwrap_or_else(PoisonError::into_inner).popup_ended(tab);
            }
            self.tabs.lock().unwrap_or_else(PoisonError::into_inner).unreserve(tab);
            return None;
        };
        // **A create's bookkeeping follows its own outcome**: a requested tab
        // that was created re-arms its fault line, so a fault that returns
        // later is a new episode.
        self.note_create(opener, tab, url, true);
        // **Only a requested tab is shown here.** A bridged popup's visibility
        // and focus are re-asserted by `on_load_end`, which every tab already
        // goes through; the raw call on a host whose pointer resolves null is
        // the crash `f7b0659b` fixes, and this path does not need to risk it.
        if opener.is_none() {
            host.was_hidden(0);
            host.set_focus(1);
            host.set_windowless_frame_rate(60);
        }
        // **Every tab this function creates with an opener is a bridged
        // popup**, `noopener` included; CEF's own popups arrive through
        // `adopt` and never reach this map.
        if opener.is_some() {
            self.bridged_tabs.lock().unwrap_or_else(PoisonError::into_inner).insert(tab);
        }
        {
            let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.insert(
                tab,
                Tab {
                    browser,
                    opener,
                    loaded: Arc::clone(&parts.loaded),
                    place,
                    pending: Arc::clone(&parts.pending),
                    stop: Arc::clone(&parts.stop),
                    key: Arc::clone(&parts.key),
                    size: Arc::clone(&parts.size),
                },
            );
        }
        start_tab_encoder(
            parts.pending,
            parts.stop,
            crate::frame::Sink::new(self.profile, tab, self.frames.clone(), self.fps, self.quality),
            Arc::clone(&self.paused),
            parts.key,
        );
        let event =
            crate::control::HostEvent::TabCreated { tab, opener, url: url.to_owned(), request };
        if emit_now {
            crate::control::emit(&event);
        } else {
            self.emit_birth(event);
        }
        drop(births);
        Some(tab)
    }

    /// Admit one page's cross-site `window.open` and create its browser.
    fn open_bridged(
        self: &Arc<Self>,
        opener: u16,
        handle: &str,
        url: &str,
        id: u64,
        noopener: bool,
    ) -> Result<u16, String> {
        let opener_url = self
            .tab_url(opener)
            .ok_or_else(|| format!("the popup's opener tab {opener} is not open"))?;
        if !bridge::is_cross_origin(url, &opener_url) {
            return Err("not a cross-origin web target".to_owned());
        }
        if !noopener
            && self
                .bridge
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .handle_taken(opener, handle)
        {
            return Err("the opener reused a bridge handle".to_owned());
        }
        {
            let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            let held = live.entry(opener).or_default();
            if *held >= MAX_POPUP_TABS {
                if let Some(line) = self.cap_refusal(opener, id, url) {
                    eprintln!("{line}");
                }
                return Err("the page already holds its open popups' limit".to_owned());
            }
            *held += 1;
        }
        let place = Arc::new(Place { opener, released: AtomicBool::new(false) });
        let extra = if noopener { None } else { Some((id, handle.to_owned())) };
        let Some(tab) = self.create_browser(
            Some(opener),
            url,
            None,
            Some(Arc::clone(&place)),
            extra.as_ref(),
            false,
        ) else {
            self.release(&place);
            return Err("the popup's browser could not be created".to_owned());
        };
        // **A popup admitted resolves the refusals opening one hits**: one
        // line on entering a refusal and one on leaving it, not one per
        // attempt. The cap is not one of them: a freed place ends that
        // episode, and a too-large message is the document's until it goes.
        for kind in ADMISSION_REFUSALS {
            self.clear_refusal(opener, kind);
        }
        if !noopener {
            // **The address waits for the role.** The renderer acknowledges
            // once `window.opener` is installed in its document; only then
            // can the popup's own scripts safely see it. An ack that landed
            // inside the create (the blank context exists before this runs)
            // is remembered, so the start goes immediately.
            self.pending_start.lock().unwrap_or_else(PoisonError::into_inner).push(PendingStart {
                tab,
                url: url.to_owned(),
                at: elapsed_ms(),
            });
            let assigned =
                self.assigned_tabs.lock().unwrap_or_else(PoisonError::into_inner).contains(&tab);
            if !assigned {
                self.announce_bridge(tab, id);
            }
        }
        Ok(tab)
    }

    fn bridge_assigned(&self, tab: u16) {
        // **The start is the pump's, never the callback's.** An ack can land
        // inside the create's nested CEF loop; navigating from there times
        // out the new browser's info response. The pump sees the flag on its
        // next tick, outside every create.
        self.assigned_tabs.lock().unwrap_or_else(PoisonError::into_inner).insert(tab);
    }

    /// A page's `window.open`: admit, create, and refuse into the proxy the
    /// page already holds when it cannot be done.
    fn bridge_open(
        self: &Arc<Self>,
        opener: u16,
        handle: &str,
        url: &str,
        _name: &str,
        noopener: bool,
    ) {
        let id = self.bridge.lock().unwrap_or_else(PoisonError::into_inner).next_id();
        if let Err(why) = self.open_bridged(opener, handle, url, id, noopener) {
            if self.first_refusal(opener, "bridged") {
                eprintln!(
                    "cef host: a cross-site popup was not bridged (event_name \
                     cef_opener_bridge_refused, profile {}, opener {opener}, address {}): {why}",
                    self.profile,
                    origin_words(url)
                );
            }
            if !noopener {
                self.send_bridge_event(opener, "refused", handle, "", "", "");
            }
        }
    }

    /// One `postMessage` through the relay. **The sending side is the tab the
    /// message arrived on**, never a field the page supplies.
    fn bridge_post(&self, from: u16, handle: &str, target: &str, data: &str) {
        if data.len() > bridge::MAX_MESSAGE_BYTES {
            if self.first_refusal(from, "too_large") {
                eprintln!(
                    "cef host: a bridged message was too large (event_name \
                     cef_opener_bridge_too_large, profile {}, tab {from}, bytes {})",
                    self.profile,
                    data.len()
                );
            }
            return;
        }
        let channel = {
            let channels = self.bridge.lock().unwrap_or_else(PoisonError::into_inner);
            if handle.is_empty() {
                channels.by_popup(from)
            } else {
                channels.by_handle(from, handle)
            }
        };
        // **A stray or mismatched message is silent**, as the platform's own
        // postMessage is; a line per drop would be a page-controlled log
        // flood.
        let Some(channel) = channel else { return };
        let (recipient, out_handle) = if handle.is_empty() {
            if !channel.opener_open {
                return;
            }
            (channel.opener, channel.handle)
        } else {
            (channel.popup, String::new())
        };
        let (Some(sender), Some(target_url)) = (self.tab_url(from), self.tab_url(recipient)) else {
            return;
        };
        if !bridge::target_allows(target, &target_url) {
            return;
        }
        let Some(origin) = bridge::origin(&sender) else { return };
        self.send_bridge_event(recipient, "message", &out_handle, &origin, data, target);
    }

    /// One proxy's `location` write: the other side of the channel navigates.
    fn bridge_navigate(&self, from: u16, handle: &str, url: &str) {
        // **A navigation the proxy may not steer to is silent**, as the
        // platform's own window navigation is; a line per refusal would be a
        // page-controlled log flood.
        if !bridge::navigable(url) {
            return;
        }
        let recipient = {
            let channels = self.bridge.lock().unwrap_or_else(PoisonError::into_inner);
            if handle.is_empty() {
                channels
                    .by_popup(from)
                    .filter(|channel| channel.opener_open)
                    .map(|channel| channel.opener)
            } else {
                channels.by_handle(from, handle).map(|channel| channel.popup)
            }
        };
        let Some(tab) = recipient else { return };
        let browser = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.get(&tab).map(|tab| tab.browser.clone())
        };
        let Some(browser) = browser else { return };
        if let Some(frame) = browser.main_frame() {
            frame.load_url(Some(&CefString::from(url)));
        }
    }

    /// The opener closed its popup through the proxy.
    fn bridge_close(&self, from: u16, handle: &str) {
        let popup = {
            self.bridge
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .by_handle(from, handle)
                .map(|channel| channel.popup)
        };
        if let Some(tab) = popup {
            self.close_tab(tab);
        }
    }

    /// **A host-created tab is not script-closable to Blink**, so the shim
    /// routes the popup's `window.close()` here.
    fn bridge_close_self(&self, from: u16) {
        let popup = {
            self.bridge
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .by_popup(from)
                .map(|channel| channel.popup)
        };
        if let Some(tab) = popup {
            self.close_tab(tab);
            return;
        }
        // **A noopener popup that took the shim's role through `window.name`
        // has no channel**, so the tab's own record that it was ours to close
        // is the one this side keeps.
        if self.bridged_tabs.lock().unwrap_or_else(PoisonError::into_inner).contains(&from) {
            self.close_tab(from);
        }
    }

    /// One tab ended: tell the other half of every channel it held. A popup's
    /// opener hears `closed`; an opener's popups hear it too, because the
    /// window they opened cannot answer any more.
    fn bridge_tab_ended(&self, tab: u16) {
        self.clear_refusals(tab);
        self.bridged_tabs.lock().unwrap_or_else(PoisonError::into_inner).remove(&tab);
        self.assigned_tabs.lock().unwrap_or_else(PoisonError::into_inner).remove(&tab);
        self.pending_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|entry| entry.tab != tab);
        let (popup_channel, opener_channels) = {
            let mut channels = self.bridge.lock().unwrap_or_else(PoisonError::into_inner);
            (channels.popup_ended(tab), channels.opener_ended(tab))
        };
        if let Some(channel) = popup_channel {
            self.send_bridge_event(channel.opener, "closed", &channel.handle, "", "", "");
        }
        for channel in opener_channels {
            self.send_bridge_event(channel.popup, "closed", "", "", "", "");
        }
    }

    /// The opener's main frame started a navigation: its old document's
    /// handles die with it. The popups stay, as platform popups do.
    fn bridge_opener_navigated(&self, tab: u16) {
        self.bridge.lock().unwrap_or_else(PoisonError::into_inner).opener_navigated(tab);
        // A new document may refuse for a new reason; its first line is worth
        // having.
        self.clear_refusals(tab);
    }

    /// A bridged popup's main frame started a navigation in a renderer that
    /// may not have received the browser's `extra_info`: name the bridge
    /// again, so the new document installs `window.opener`.
    fn bridge_popup_navigated(&self, tab: u16) {
        let channel = { self.bridge.lock().unwrap_or_else(PoisonError::into_inner).by_popup(tab) };
        if let Some(channel) = channel {
            self.announce_bridge(tab, channel.id);
            // **A document that loads after its opener died is told then.**
            // The close was pushed when it happened; a popup whose context
            // was not ready for it would otherwise read `closed: false` for
            // the rest of its life.
            if !channel.opener_open {
                self.send_bridge_event(tab, "closed", "", "", "", "");
            }
        }
    }

    /// A bridged popup's renderer died. **The tab ends**: its opener must not
    /// read `closed: false` for a page that can never answer. A `noopener`
    /// popup has no channel, so the tab itself is the record that it was ours.
    fn bridge_renderer_gone(&self, tab: u16) {
        let bridged =
            self.bridged_tabs.lock().unwrap_or_else(PoisonError::into_inner).contains(&tab);
        if bridged {
            self.close_tab(tab);
        }
    }

    fn tab_url(&self, tab: u16) -> Option<String> {
        let browser = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.get(&tab)?.browser.clone()
        };
        let frame = browser.main_frame()?;
        Some(CefStringUtf16::from(&frame.url()).to_string())
    }

    /// Send one bridge event to a tab's main frame. A tab with no frame yet
    /// drops it, as a platform message to a gone window does.
    ///
    /// `target` rides along for a message so the receiving document can
    /// re-check the target origin at dispatch, where a commit cannot land
    /// between the check and the event.
    fn send_bridge_event(
        &self,
        tab: u16,
        kind: &str,
        handle: &str,
        origin: &str,
        data: &str,
        target: &str,
    ) {
        let browser = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.get(&tab).map(|tab| tab.browser.clone())
        };
        let Some(browser) = browser else { return };
        let Some(frame) = browser.main_frame() else { return };
        send_bridge_message(
            &frame,
            "forge.bridge.event",
            &[
                kind.to_owned(),
                handle.to_owned(),
                origin.to_owned(),
                data.to_owned(),
                target.to_owned(),
            ],
        );
    }

    fn navigate_tab(&self, tab: u16, url: &str) {
        let browser = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.get(&tab).map(|tab| tab.browser.clone())
        };
        let Some(browser) = browser else { return };
        if let Some(frame) = browser.main_frame() {
            frame.load_url(Some(&CefString::from(url)));
        }
    }

    fn announce_bridge(&self, tab: u16, id: u64) {
        let browser = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.get(&tab).map(|tab| tab.browser.clone())
        };
        let Some(browser) = browser else { return };
        let Some(frame) = browser.main_frame() else { return };
        send_bridge_message(&frame, "forge.bridge.assign", &[id.to_string()]);
    }
}

/// What every tab's callbacks and the pump share: the profile's one ring,
/// its one frame socket, and the tab map.
struct HostRuntime {
    tabs: Arc<Mutex<Tabs>>,
    inputs: Arc<Mutex<VecDeque<crate::input::HostInput>>>,
    popups: Arc<PopupGate>,
    recording: Arc<Recording>,
    /// Whether any viewer is attached; **the profile's**, not one tab's.
    paused: Arc<AtomicBool>,
}

impl HostRuntime {
    /// Open one tab at `url`. **The one place the host creates a browser for
    /// a server command or a page's cross-site open**: the first tab and a
    /// call both come through here. A same-origin CEF popup is CEF's to
    /// create, and joins as a tab through `adopt`.
    ///
    /// **The number is reserved before the create, never after it.** The create
    /// is synchronous and CEF runs a nested message loop inside it, so a page's
    /// popup can be admitted and take a number while this call is still
    /// running; reserving first is what keeps the two apart.
    fn create_tab(&self, url: &str, request: Option<u64>) -> bool {
        self.popups.create_browser(None, url, request, None, None, true).is_some()
    }

    /// Close one tab and end its frame worker; a page target CEF has already
    /// lost is a no-op.
    fn close_tab(&self, tab: u16) {
        self.popups.close_tab(tab);
    }

    /// One tab's host, when the tab is open.
    fn tab_host(&self, tab: u16) -> Option<BrowserHost> {
        let browser = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.get(&tab).map(|entry| entry.browser.clone())
        };
        browser?.host()
    }

    /// Run `f` against every open tab's host.
    fn each_host(&self, mut f: impl FnMut(&BrowserHost)) {
        let browsers: Vec<Browser> = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.values().map(|tab| tab.browser.clone()).collect()
        };
        for browser in browsers {
            if let Some(host) = browser.host() {
                f(&host);
            }
        }
    }

    /// The profile's requested size: every tab takes it and repaints. **A
    /// tab laid out apart by a take is included** - the controller's resize
    /// ends its exception.
    fn apply_size(&self, width: u32, height: u32, scale: f64) {
        let hosts: Vec<BrowserHost> = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            for tab in tabs.open.values() {
                tab.size.set(width, height, scale);
            }
            tabs.open.values().filter_map(|tab| tab.browser.host()).collect()
        };
        for host in hosts {
            // The pixel ratio first: it changes the DIP rect as well as the
            // buffer, and CEF lays out against both.
            host.notify_screen_info_changed();
            host.was_resized();
            // **A resize repaints even a still page**: a blank or static
            // document has no layout change to trigger one, and the viewer
            // would keep the old size until something else moved.
            host.invalidate(PaintElementType::VIEW);
        }
    }

    /// Apply one show cue to every tab.
    fn show(&self, header: &Header, show: u64) {
        let browsers: Vec<Browser> = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.values().map(|tab| tab.browser.clone()).collect()
        };
        let hosts: Vec<BrowserHost> = browsers.iter().filter_map(ImplBrowser::host).collect();
        let targets: Vec<&dyn ShowTarget> =
            hosts.iter().map(|host| host as &dyn ShowTarget).collect();
        apply_show(header, show, &targets);
    }

    /// Ask every tab for a full-quality frame.
    fn ask_key(&self) {
        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        for tab in tabs.open.values() {
            tab.key.store(true, Ordering::Release);
        }
    }

    /// Hand the queued gestures to CEF, in the order they arrived, and run
    /// the tab commands beside them.
    ///
    /// **These calls are the routing.** CEF's own `SendMouse*Event` is what
    /// forwards an event into a windowless popup when the point is inside its
    /// rect; the same event sent to the page target over DevTools never
    /// reaches the popup view and closes a select's menu instead of choosing
    /// from it.
    fn apply_inputs(&self) {
        let mut queue = self.inputs.lock().unwrap_or_else(PoisonError::into_inner);
        while let Some(input) = queue.pop_front() {
            match input {
                crate::input::HostInput::Mouse { tab, action, x, y, button, clicks, modifiers } => {
                    let Some(host) = self.tab_host(tab) else { continue };
                    let event =
                        MouseEvent { x, y, modifiers: crate::input::cef_modifiers(modifiers) };
                    match action {
                        forge_primitives::browser::MouseAction::Move => {
                            host.send_mouse_move_event(Some(&event), 0);
                        }
                        forge_primitives::browser::MouseAction::Press => {
                            host.send_mouse_click_event(
                                Some(&event),
                                mouse_button(button),
                                0,
                                clicks,
                            );
                        }
                        forge_primitives::browser::MouseAction::Release => {
                            host.send_mouse_click_event(
                                Some(&event),
                                mouse_button(button),
                                1,
                                clicks,
                            );
                        }
                    }
                }
                crate::input::HostInput::Wheel { tab, x, y, delta_x, delta_y, modifiers } => {
                    let Some(host) = self.tab_host(tab) else { continue };
                    let event =
                        MouseEvent { x, y, modifiers: crate::input::cef_modifiers(modifiers) };
                    host.send_mouse_wheel_event(Some(&event), delta_x, delta_y);
                }
                // **A server command opens any address it names.** Unlike a
                // page's popup this is trusted, so a `data:` document is a
                // page like any other.
                crate::input::HostInput::TabNew { url, request } => {
                    let url = url.as_deref().unwrap_or("about:blank");
                    self.create_tab(url, request);
                }
                crate::input::HostInput::TabClose { tab } => self.close_tab(tab),
                // **One tab's layout, not the profile's.** A take records at
                // 1280x720 without resizing every other tab.
                crate::input::HostInput::TabResize { tab, width, height, scale_milli } => {
                    let size = {
                        let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
                        tabs.open.get(&tab).map(|tab| Arc::clone(&tab.size))
                    };
                    if let Some(size) = size {
                        size.set(width, height, f64::from(scale_milli) / 1000.0);
                        if let Some(host) = self.tab_host(tab) {
                            host.notify_screen_info_changed();
                            host.was_resized();
                            host.invalidate(PaintElementType::VIEW);
                        }
                    }
                }
                crate::input::HostInput::File { id, paths } => {
                    if !crate::dialog::answer(id, &paths) {
                        eprintln!(
                            "cef host: a file answer named no open dialog (event_name \
                             cef_file_answer_stray, id {id})"
                        );
                    }
                }
                crate::input::HostInput::Dialog { tab, accept, prompt } => {
                    if !crate::jsdialog::answer(tab, accept, prompt.as_deref()) {
                        eprintln!(
                            "cef host: a dialog answer named no open dialog (event_name \
                             cef_dialog_answer_stray, tab {tab})"
                        );
                    }
                }
                crate::input::HostInput::RecordStart { tab, path } => {
                    self.recording.start(tab, &path);
                }
                crate::input::HostInput::RecordStop => self.recording.stop(),
            }
        }
    }

    /// Close every tab, letting CEF flush each one's store.
    fn close_all(&self) {
        let tabs: Vec<u16> = {
            let tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.keys().copied().collect()
        };
        for tab in tabs {
            self.close_tab(tab);
        }
    }
}

/// Queue one paint for a tab's worker.
fn push_paint(
    pending: &Pending,
    view: &[u8],
    width: u32,
    height: u32,
    distinct: bool,
    page: (u32, u32),
) {
    let (queue, wake) = pending;
    let mut held = queue.lock().unwrap_or_else(PoisonError::into_inner);
    // **A queued popup frame is not replaced by a plain repaint.** The queue
    // keeps the newest paint, and the view CEF paints after a popup has the
    // menu removed - so replacing the popup with it before the encoder takes
    // it loses the menu a second way.
    let replace = matches!(
        held.back(),
        Some(job) if matches!(job, Job::Frame(job) if replaces_queued(Some(job), distinct))
    );
    let paint = PaintJob { pixels: view.to_vec(), width, height, page, distinct };
    if replace {
        if let Some(slot) = held.back_mut() {
            *slot = Job::Frame(paint);
        }
    } else {
        held.push_back(Job::Frame(paint));
    }
    drop(held);
    wake.notify_one();
}

/// Queue one audio packet for a tab's worker.
///
/// **A backlog drops the newest packet.** Audio is a stream: one that queues
/// is heard late, and a cap turns a growing delay into a short gap.
fn push_audio(pending: &Pending, packet: crate::channel::Audio) {
    let (queue, wake) = pending;
    let mut held = queue.lock().unwrap_or_else(PoisonError::into_inner);
    if held.len() >= MAX_PENDING {
        return;
    }
    held.push_back(Job::Audio(packet));
    drop(held);
    wake.notify_one();
}

/// One tab's frame worker: **the interval is spent waiting, never dropping**.
/// The queue keeps one frame, replaced by each newer paint, so a burst's last
/// state is what gets encoded; a paint dropped for landing too close to the
/// last send was a state no later paint repaired - a typed character or a
/// reload that never reached the viewer.
fn start_tab_encoder(
    pending: Arc<Pending>,
    stop: Arc<AtomicBool>,
    mut sink: crate::frame::Sink,
    paused: Arc<AtomicBool>,
    key_asked: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let job = {
                let (queue, wake) = &*pending;
                let mut guard = queue.lock().unwrap_or_else(PoisonError::into_inner);
                while guard.is_empty() {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    guard = wake.wait(guard).unwrap_or_else(PoisonError::into_inner);
                }
                guard.pop_front()
            };
            let Some(job) = job else { continue };
            match job {
                Job::Frame(job) => {
                    sink.set_paused(paused.load(Ordering::Acquire));
                    if key_asked.swap(false, Ordering::AcqRel) {
                        sink.force_key();
                    }
                    let wait = sink.interval_remaining();
                    if !wait.is_zero() {
                        std::thread::sleep(wait);
                    }
                    let newest = {
                        let (queue, _) = &*pending;
                        let mut held = queue.lock().unwrap_or_else(PoisonError::into_inner);
                        take_for_encode(&mut held)
                    };
                    let job = encode_target(job, newest);
                    sink.offer(&job.pixels, job.width, job.height, job.page);
                }
                // **Audio never waits on the frame rate**: it is a clock of
                // its own, and the paused state is the viewer's.
                Job::Audio(audio) => sink.offer_audio(&audio),
            }
        }
    })
}

/// The host process's whole life.
pub fn run(args: &Args) -> ExitCode {
    begin();
    stop_on_termination();

    // **A background app is napped.** A host with no window of its own reads
    // as an idle app to macOS; the activity keeps its pump and frames out of
    // App Nap's coalescing.
    keep_awake();

    if let Err(why) = std::fs::create_dir_all(&args.profile) {
        return fail(&why.to_string());
    }
    let ring = match Ring::create(&args.ring, args.width, args.height) {
        Ok(ring) => {
            // **This ring is this child's**: the pid is what the server
            // checks before it trusts the endpoint it probed.
            ring.set_pid(std::process::id());
            Arc::new(ring)
        }
        Err(why) => return fail(&why),
    };

    let mut app = HostApp::new();
    let cef_args = args::Args::new();
    if !initialize(&args.profile, args.port, &mut app, &cef_args) {
        ring.set_state(STATE_FAILED);
        return ExitCode::from(3);
    }

    // **The frame stream is the host's second output.** The ring stays for
    // the client-side surface until it is removed; the server reads the
    // encoded frames.
    //
    // **Encoding and the socket write are each tab's worker, not the paint
    // callback's.** CEF paints on the thread that also drives the page, and a
    // server that stops reading would otherwise hold that thread in a frame
    // write for as long as the bound allows. The callback copies the newest
    // paint into that tab's slot - replacing what was not taken - and the
    // tab's worker encodes and writes it.
    let tabs: Arc<Mutex<Tabs>> = Arc::new(Mutex::new(Tabs::default()));
    let paused = Arc::new(AtomicBool::new(true));
    let recording = Recording::new();
    let runtime = HostRuntime {
        tabs: Arc::clone(&tabs),
        inputs: Arc::new(Mutex::new(VecDeque::new())),
        popups: Arc::new(PopupGate {
            ring: Arc::clone(&ring),
            paused: Arc::clone(&paused),
            recording: Arc::clone(&recording),
            profile: args.profile_id,
            frames: args.frames.clone(),
            fps: args.fps,
            quality: args.quality,
            downloads_dir: args.profile.clone(),
            tabs,
            live: Mutex::new(HashMap::new()),
            places: Mutex::new(HashMap::new()),
            boot: Mutex::new(Vec::new()),
            held_births: Mutex::new(Vec::new()),
            holding_births: AtomicUsize::new(0),
            bridge: Mutex::new(Channels::default()),
            bridged_tabs: Mutex::new(HashSet::new()),
            refused_logged: Mutex::new(HashSet::new()),
            pending_start: Mutex::new(Vec::new()),
            assigned_tabs: Mutex::new(HashSet::new()),
        }),
        recording,
        paused,
    };
    // **The host opens one blank boot tab.** It is the page the driver
    // attaches to, and a call navigates it once the veto is registered.
    //
    // **A browser with no host is not a live host**: the state and the log
    // below are what the server reads to say this profile is up, so a
    // browser that cannot be driven fails here rather than reporting live.
    if !runtime.create_tab("about:blank", None) {
        ring.set_state(STATE_FAILED);
        return fail("the windowless browser was not created");
    }
    // **Pointer input arrives on stdin, one line per gesture.** The reader
    // only queues: CEF's mouse calls are made on the pump thread below, the
    // one that owns the browsers, and they are the calls that route a click
    // into a windowless popup.
    {
        let inputs = Arc::clone(&runtime.inputs);
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let Some(input) = crate::input::parse_host_line(&line) else { continue };
                let mut queue = inputs.lock().unwrap_or_else(PoisonError::into_inner);
                // **A wedged pump cannot grow this forever**: when it is
                // full the oldest gesture goes, because the newest one is
                // the one the person still means.
                if queue.len() >= MAX_INPUT_BACKLOG {
                    queue.pop_front();
                }
                queue.push_back(input);
            }
        });
    }
    ring.set_state(STATE_LIVE);
    ring.set_visible(true, true);
    // **stderr, which the server redirects into this profile's
    // `cef-host.log`.** The host's own log line goes to the stream the
    // launcher captures, so it is there for a launch that failed before any
    // subscriber could be built.
    eprintln!(
        "cef host: live (pid {}, profile {}, port {}, ring {})",
        std::process::id(),
        args.profile.display(),
        args.port,
        args.ring,
    );

    let mut last_begin = 0_u64;
    let mut cues = ShowCue::default();
    // **A server killed without its exit path takes its hosts with it.** The
    // watch is the backstop; the server's own teardown is the fast path. The
    // fd is this process's own, inherited across exec for this watch.
    // SAFETY: the number came from the spawn's `dup2`, and nothing else in
    // this process owns or closes it.
    let watch = args.parent.map(|fd| unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) });
    let mut parent = ParentWatch::new(watch);
    // SAFETY: the header is mapped for this process's whole life.
    let mut last_request = unsafe { (*ring.header()).request_seq.load(Ordering::Acquire) };
    // SAFETY: as above.
    let mut last_key = unsafe { (*ring.header()).key_seq.load(Ordering::Acquire) };
    loop {
        if let Some(why) = stopping(&STOP, &mut parent, elapsed_ms()) {
            if why == Stop::ServerGone {
                eprintln!(
                    "cef host: the server that spawned this host is gone (event_name \
                     cef_parent_gone, fd {})",
                    parent.fd().unwrap_or_default()
                );
            }
            break;
        }
        let header = ring.header();
        // SAFETY: the header is mapped for this process's whole life.
        let header_ref = unsafe { &*header };
        // The driver asked for another view size: adopt it and tell CEF.
        //
        // **The ring already holds the frame budget**, so neither axis is
        // clamped here: a tall phone frame fits the area budget while passing
        // MAX_HEIGHT, and clamping it halved the page's layout under the
        // surface that asked for it.
        let request = header_ref.request_seq.load(Ordering::Acquire);
        if request != last_request {
            last_request = request;
            let width = header_ref.request_width.load(Ordering::Relaxed).max(1);
            let height = header_ref.request_height.load(Ordering::Relaxed).max(1);
            let scale = header_ref.request_scale_milli.load(Ordering::Relaxed).clamp(100, 8_000);
            header_ref.width.store(width, Ordering::Relaxed);
            header_ref.height.store(height, Ordering::Relaxed);
            header_ref.scale_milli.store(scale, Ordering::Relaxed);
            runtime.apply_size(width, height, f64::from(scale) / 1000.0);
        }
        // **Every call the driver makes re-asserts the page visible.** A
        // windowless browser's only visibility lever is this call; a page
        // that went hidden while nothing drove it (the reported
        // `document.visibilityState` "hidden" on the first call of a fire)
        // must not stay hidden under a click that needs its frames. The
        // applied sequence is published so the caller can wait for it, and
        // the line names the cue a real fire can be read by.
        if let Some(show) = cues.take(header_ref.show_seq.load(Ordering::Acquire)) {
            runtime.show(header_ref, show);
        }
        // **Encoding follows the viewers.** The service pauses the ring
        // with nobody attached and resumes it on the first; a resume forces
        // a key frame, since what changed while nobody watched is not a
        // delta's to describe.
        let paused = header_ref.paused.load(Ordering::Acquire) != 0;
        runtime.paused.store(paused, Ordering::Release);
        // **A viewer attaching asks for a crisp first draw.** The service
        // increments the cue; the next encoded frame is full quality.
        let key = header_ref.key_seq.load(Ordering::Acquire);
        if key != last_key {
            last_key = key;
            runtime.ask_key();
            // **A page that has not changed does not repaint on its own**,
            // so the attach's key cue asks for one: a viewer joining a still
            // page draws it now rather than at the next navigation.
            runtime.each_host(|host| host.invalidate(PaintElementType::VIEW));
        }
        // **The pointer gestures go before the frame cue**, so a click is in
        // CEF before the frame it causes is asked for.
        runtime.apply_inputs();
        runtime.popups.bootstrap(elapsed_ms());
        // **8 ms, not 16.** A BeginFrame every screen refresh is the paint
        // rate, but a click landing just after one waited out the whole
        // interval; asking more often lets the next frame start sooner and
        // the compositor still paints at its own rate.
        let now = elapsed_ms();
        if now.saturating_sub(last_begin) >= 8 {
            last_begin = now;
            runtime.each_host(ImplBrowserHost::send_external_begin_frame);
        }
        pump();
        std::thread::sleep(Duration::from_millis(1));
    }
    // **The profile's own writes go before the process does.** Closing each
    // browser lets CEF's store flush; the bounded pump is what carries the
    // close through the same message loop that painted it.
    runtime.close_all();
    for _ in 0..100 {
        pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    shutdown();
    ExitCode::SUCCESS
}

/// AppKit's button numbering as CEF's own.
fn mouse_button(button: i32) -> MouseButtonType {
    match button {
        1 => MouseButtonType::RIGHT,
        2 => MouseButtonType::MIDDLE,
        _ => MouseButtonType::LEFT,
    }
}

/// One process message to a renderer, with string arguments.
fn send_bridge_message(frame: &Frame, name: &str, arguments: &[String]) {
    let Some(mut message) = process_message_create(Some(&CefString::from(name))) else { return };
    let Some(list) = message.argument_list() else { return };
    for (at, value) in arguments.iter().enumerate() {
        list.set_string(at, Some(&CefString::from(value.as_str())));
    }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
}

/// The address a log line may name: scheme, host and port, never the path or
/// query. **An OAuth redirect carries a code and a state**, and the host's
/// stderr is a file.
fn origin_words(url: &str) -> String {
    crate::bridge::origin(url).unwrap_or_else(|| "an unreadable address".to_owned())
}

/// The `extra_info` a bridged popup's browser carries into its renderers:
/// the channel id, so `window.opener` is installed before the first
/// document's scripts run.
fn bridge_extra(id: u64) -> Option<DictionaryValue> {
    let value = dictionary_value_create()?;
    let key = CefString::from("forge_bridge");
    let id = CefString::from(id.to_string().as_str());
    (value.set_string(Some(&key), Some(&id)) == 1).then_some(value)
}

fn fail(why: &str) -> ExitCode {
    eprintln!("forge host: {why}");
    ExitCode::from(2)
}

/// Keep this windowless process out of App Nap.
fn keep_awake() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let info = NSProcessInfo::processInfo();
    let activity = info.beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::LatencyCritical,
        &NSString::from_str("the browser host paints a windowless surface"),
    );
    std::mem::forget(activity);
}

/// How long a burst of focus answers is held before one line goes out.
/// **The last answer always goes out** - the value held when the window
/// closes - so a page that focuses in a loop cannot wake a viewer per toggle
/// while a real change still lands inside a blink.
const FOCUS_COALESCE: Duration = Duration::from_millis(50);

/// One tab's focused-node answer, frame by frame.
///
/// **Focus moves between frames, and their reports do not arrive in order.**
/// Moving from the page into a cross-origin iframe emits the main frame's
/// `false` and the iframe's `true` from two renderer processes with nothing
/// sequencing them, so one last-wins flag can settle `false` while the
/// iframe's field holds the keyboard. Each frame's answer is kept instead,
/// and the tab is editable when any frame's focused node is.
///
/// **A burst is coalesced to one line.** Every answer is a renderer process
/// message and a wake for the attached viewer, so a page that focuses in a
/// loop must not become a stream of them: the first answer starts a
/// [`FOCUS_COALESCE`] window, and what is held at its end is what goes out.
struct FocusState {
    tab: u16,
    inner: Mutex<FocusInner>,
}

#[derive(Default)]
struct FocusInner {
    /// Each frame's own answer: whether its focused node is editable. **The
    /// key is CEF's frame identifier, a string in 156.**
    frames: HashMap<String, bool>,
    /// The tab's settled answer, before coalescing.
    current: bool,
    /// What the last line said.
    emitted: bool,
    /// Whether a flush is already on its way.
    scheduled: bool,
}

impl FocusInner {
    /// Settle the tab's answer from its frames, and answer whether a flush
    /// is now owed and not yet scheduled.
    fn settle(&mut self) -> bool {
        self.current = self.frames.values().any(|editable| *editable);
        let due = self.current != self.emitted && !self.scheduled;
        if due {
            self.scheduled = true;
        }
        due
    }
}

impl FocusState {
    fn new(tab: u16) -> Arc<Self> {
        Arc::new(Self { tab, inner: Mutex::new(FocusInner::default()) })
    }

    /// Serve one frame's report, answering whether a line is now owed.
    fn settle(&self, frame: &str, editable: bool) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.frames.insert(frame.to_owned(), editable);
        inner.settle()
    }

    /// Report one frame's answer, scheduling the line a change is owed.
    fn report(self: &Arc<Self>, frame: &str, editable: bool) {
        if self.settle(frame, editable) {
            self.schedule();
        }
    }

    /// **A new document and a dead renderer both leave no focused node.**
    /// Every frame's answer goes, so a stale `true` from a frame that no
    /// longer exists cannot keep the phone's keyboard up.
    fn reset(self: &Arc<Self>) {
        let due = {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner.frames.clear();
            inner.settle()
        };
        if due {
            self.schedule();
        }
    }

    /// Hold the answer for [`FOCUS_COALESCE`], then write it if it moved.
    ///
    /// **A plain thread, not a CEF task**: the line is a stdout write, which
    /// needs nothing of CEF's message loop or its UI thread.
    fn schedule(self: &Arc<Self>) {
        let state = Arc::clone(self);
        std::thread::spawn(move || {
            std::thread::sleep(FOCUS_COALESCE);
            state.flush();
        });
    }

    /// Write the held answer unless it is the one already said.
    fn flush(&self) {
        let value = {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            inner.scheduled = false;
            if inner.current == inner.emitted {
                None
            } else {
                inner.emitted = inner.current;
                Some(inner.current)
            }
        };
        if let Some(editable) = value {
            crate::control::emit(&crate::control::HostEvent::Focus { tab: self.tab, editable });
        }
    }
}

cef::wrap_load_handler! {
    struct HostLoadHandler {
        // **The popup bootstrap's one signal.** A popup that has started a
        // load of its own must never be navigated again, and a committed URL
        // cannot say that: the frame keeps its last URL while the next load
        // is in flight.
        loaded: Arc<AtomicBool>,
        tab: u16,
        popups: Arc<PopupGate>,
        // The tab's focused-node answer, shared with its client: **a new
        // document holds no editable node**, so a navigation drops the
        // phone's keyboard even when the renderer reports nothing.
        focus: Arc<FocusState>,
    }

    impl LoadHandler {
        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            if frame.is_some_and(|frame| frame.is_main() == 1) {
                self.loaded.store(true, Ordering::Release);
                // **A new document has no focused node yet.** Every frame's
                // answer goes - a subframe is gone with the document too -
                // and the phone's keyboard drops even if a renderer's own
                // report races the commit.
                self.focus.reset();
                // **A main-frame navigation is a new document on this tab.**
                // An opener's handles die with the old one; a bridged popup
                // is told which bridge it is again, because a cross-site
                // navigation may have created a renderer that never saw the
                // browser's `extra_info`.
                //
                // **The handles die at load START, before commit.** The old
                // document is live until commit, so a message it sends in
                // that window is dropped where the platform would deliver
                // it; the narrowing is deliberate, because a handle minted by
                // a document that is leaving cannot be allowed to name a
                // channel after the next document exists.
                self.popups.bridge_opener_navigated(self.tab);
                self.popups.bridge_popup_navigated(self.tab);
            }
        }

        fn on_load_end(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _http_status_code: i32,
        ) {
            // CEF's visibility and focus calls are re-asserted once a page is
            // there: the windowless surface has no window to inherit them
            // from.
            let Some(browser) = browser else { return };
            let Some(host) = browser.host() else { return };
            host.was_hidden(0);
            host.set_focus(1);
            host.set_windowless_frame_rate(60);
            host.was_resized();
        }
    }
}

cef::wrap_life_span_handler! {
    struct HostLifeSpanHandler {
        tab: u16,
        popups: Arc<PopupGate>,
        // Set on a popup's own handler: the tab CEF is creating for it.
        popup: Option<AdmittedPopup>,
    }

    impl LifeSpanHandler {
        /// **A popup's browser is CEF's to create; its tab is this host's.**
        /// The number and the client were settled when the popup was
        /// admitted, so this is where the created browser becomes a tab.
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            let Some(popup) = self.popup.clone() else { return };
            self.popups.adopt(browser.clone(), popup);
        }

        /// **A popup CEF could not create gives its place back**, or a page
        /// that opened one that failed would hold a count against itself for
        /// the life of the host. **The place is the one CEF's own id names**,
        /// so a report arriving twice cannot free a second place.
        fn on_before_popup_aborted(&self, _browser: Option<&mut Browser>, popup_id: i32) {
            self.popups.aborted(self.tab, popup_id);
        }

        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            // **Windowless: 0 destroys the browser.** That is what a page's
            // own close means - an OAuth popup calls `window.close()` when it
            // is done - and `on_before_close` is where the tab goes with it.
            0
        }

        /// **A page's popup is allowed, never cancelled.** Cancelling it and
        /// opening a tab in its place leaves `window.open` answering null, and
        /// every sign-in flow reads null as "the pop-up was blocked" - the tab
        /// substituted for it never helps, because the page has already
        /// failed. The popup is given a client of its own and joins the
        /// profile's tabs once CEF has created it; its own document is the
        /// host's to start, because CEF starts none while an auto-attached
        /// client holds the target. See `PopupGate::bootstrap`.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            let Some((popup_client, admitted)) = self.popups.admit(self.tab, popup_id, &url)
            else {
                return 1;
            };
            if let Some(info) = window_info {
                admitted.window_info(info);
            }
            // **Opaque, like every tab.** CEF's default leaves the popup
            // transparent, and the takeover's dark ground would show through
            // a page with no background rule.
            if let Some(settings) = settings {
                settings.background_color = 0xFF04_050A;
            }
            if let Some(out) = client {
                *out = Some(popup_client);
            }
            0
        }

        /// **The browser going is the tab going**, when the close is the
        /// page's own `window.close()` or the host's. A tab the host already
        /// ended is a no-op here, so the server sees one `TabClosed` either
        /// way. **A crashed renderer is not this**: the browser outlives it.
        /// Only a bridged popup's death is watched, because its opener holds
        /// a `closed` flag that must not lie (see `HostRequestHandler`).
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.popups.forget(self.tab);
            // **A dialog open on this browser goes with it.** The callback has
            // nothing left to answer, and an entry kept for a destroyed page
            // would hold its tab against a browser that no longer exists.
            crate::jsdialog::forget(self.tab);
        }
    }
}

cef::wrap_client! {
    struct HostClient {
        // The tab this client's browser is.
        tab: u16,
        ring: Arc<Ring>,
        paint: crate::osr::PaintSink,
        // **One render handler for every view of this tab.** CEF asks a
        // client for its render handler per view, and a popup is its own
        // view: a handler built per call kept the popup's bounds in one and
        // painted into another, so the menu was never composited.
        render: RenderHandler,
        // One audio handler too: its stream state is the page's, and a fresh
        // one per call would forget the sample rate between packets.
        audio: AudioHandler,
        // The page's downloads land beside the profile before they are the
        // controlling client's to save.
        downloads: DownloadHandler,
        // Whether this tab's main frame has started a load, for the popup
        // bootstrap.
        loaded: Arc<AtomicBool>,
        // A page's `window.open` becomes a tab of this profile here.
        popups: Arc<PopupGate>,
        // **The tab's focused-node answer.** The renderer reports from its
        // own process, the load handler reports a navigation, and both land
        // here so one tab's answer is deduped in one place.
        focus: Arc<FocusState>,
        // Set on a popup's own client: the tab CEF is creating for it.
        popup: Option<AdmittedPopup>,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }

        fn audio_handler(&self) -> Option<AudioHandler> {
            Some(self.audio.clone())
        }

        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(crate::dialog::handler(self.tab))
        }

        /// **A page's JavaScript dialog is answered here.** With no handler
        /// CEF raises a modal nothing in this windowless host can draw or
        /// answer, and the page's own thread waits behind it for ever (#2732).
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(crate::jsdialog::handler(self.tab))
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.downloads.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(HostLoadHandler::new(
                Arc::clone(&self.loaded),
                self.tab,
                Arc::clone(&self.popups),
                Arc::clone(&self.focus),
            ))
        }

        /// **A page's bridge call.** The tab this client belongs to is the
        /// sender: the browser process never trusts an id the page names for
        /// the other side.
        fn on_process_message_received(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            let Some(message) = message else { return 0 };
            let name = CefString::from(&message.name()).to_string();
            // **A focused node is any frame's.** The main-frame rule below
            // is the opener bridge's trust boundary, and a cross-origin
            // iframe's text field takes the phone's keyboard just the same -
            // so this answer is taken before the gate, and the tab it names
            // is this client's own.
            // **The name is the renderer's own**: one const for the sender
            // and this reader, so a rename cannot leave the browser half
            // listening for a line nothing sends.
            if name == crate::bridge_renderer::FOCUS {
                let Some(args) = message.argument_list() else { return 1 };
                let editable = CefString::from(&args.string(0)).to_string() == "1";
                // **The frame that sent it keys the answer**: a cross-origin
                // iframe reports from its own process, and its arrival order
                // against the parent's is not guaranteed.
                let frame = frame
                    .as_ref()
                    .map(|frame| CefString::from(&frame.identifier()).to_string())
                    .unwrap_or_default();
                self.focus.report(&frame, editable);
                return 1;
            }
            // **The bridge is the main frame's.** A subframe has no native
            // object, so a message from one is a renderer that reached past
            // its own document; it is refused here rather than routed as the
            // top document's. **The check sits above the argument read on
            // purpose**: such a message is refused before its arguments are
            // parsed. Page script cannot drive one today, because a
            // same-origin subframe's call on the top's object is reported as
            // the main frame's; this is the gate a renderer meets.
            if frame.as_ref().is_some_and(|frame| frame.is_main() != 1) {
                if let Some(line) = self.popups.refuse_subframe(self.tab) {
                    eprintln!("{line}");
                }
                return 1;
            }
            let Some(args) = message.argument_list() else { return 1 };
            let string = |at: usize| CefString::from(&args.string(at)).to_string();
            match name.as_str() {
                "forge.bridge.open" => {
                    self.popups.bridge_open(
                        self.tab,
                        &string(0),
                        &string(1),
                        &string(2),
                        string(3) == "1",
                    );
                }
                "forge.bridge.post" => {
                    self.popups.bridge_post(self.tab, &string(0), &string(1), &string(2));
                }
                "forge.bridge.close" => self.popups.bridge_close(self.tab, &string(0)),
                "forge.bridge.close_self" => self.popups.bridge_close_self(self.tab),
                "forge.bridge.navigate" => {
                    self.popups.bridge_navigate(self.tab, &string(0), &string(1));
                }
                "forge.bridge.assigned" => self.popups.bridge_assigned(self.tab),
                _ => return 0,
            }
            1
        }

        /// **A bridged popup whose renderer died ends its tab**, so its opener
        /// never reads `closed: false` for a page that cannot answer. Other
        /// tabs keep the existing behaviour: the browser outlives a crashed
        /// renderer.
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(HostRequestHandler::new(
                self.tab,
                Arc::clone(&self.popups),
                Arc::clone(&self.focus),
            ))
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(HostLifeSpanHandler::new(
                self.tab,
                Arc::clone(&self.popups),
                self.popup.clone(),
            ))
        }
    }
}

// **A bridged popup's renderer dying ends its tab.** Every other tab keeps
// the existing behaviour: the browser outlives a crashed renderer and the tab
// is closed by a call or the sweep.
cef::wrap_request_handler! {
    struct HostRequestHandler {
        tab: u16,
        popups: Arc<PopupGate>,
        // **A renderer that dies with a field focused leaves no focused node
        // behind**, so its tab's answer is reset rather than left `true`
        // over a dead page.
        focus: Arc<FocusState>,
    }

    impl RequestHandler {
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            _status: TerminationStatus,
            _error_code: i32,
            _error_string: Option<&CefString>,
        ) {
            self.popups.bridge_renderer_gone(self.tab);
            self.focus.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    /// A popup gate on a scratch ring, for the tests that need one.
    fn gate(ring: &std::sync::Arc<crate::osr::Ring>) -> std::sync::Arc<super::PopupGate> {
        std::sync::Arc::new(super::PopupGate {
            ring: std::sync::Arc::clone(ring),
            paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            recording: super::Recording::new(),
            profile: 1,
            frames: std::path::PathBuf::from("/tmp/frames.sock"),
            fps: 30,
            quality: 75,
            downloads_dir: std::path::PathBuf::from("/tmp"),
            tabs: std::sync::Arc::new(std::sync::Mutex::new(super::Tabs::default())),
            live: std::sync::Mutex::new(std::collections::HashMap::new()),
            places: std::sync::Mutex::new(std::collections::HashMap::new()),
            boot: std::sync::Mutex::new(Vec::new()),
            held_births: std::sync::Mutex::new(Vec::new()),
            holding_births: std::sync::atomic::AtomicUsize::new(0),
            bridge: std::sync::Mutex::new(super::Channels::default()),
            bridged_tabs: std::sync::Mutex::new(std::collections::HashSet::new()),
            refused_logged: std::sync::Mutex::new(std::collections::HashSet::new()),
            pending_start: std::sync::Mutex::new(Vec::new()),
            assigned_tabs: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// **A page's popups are bounded, and the bound is given back.** A
    /// dropped release would refuse a page's sign-in for the host's whole
    /// life once it had opened sixteen popups; a double release would let the
    /// cap be walked past.
    #[test]
    fn the_popup_cap_counts_places_and_gives_them_back() {
        let name = format!("/forge-cef-test-cap-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        let mut held = Vec::new();
        for id in 0..super::MAX_POPUP_TABS {
            held.push(
                popups
                    .admit(1, i32::try_from(id).expect("a popup id"), "about:blank")
                    .expect("a popup within the cap"),
            );
        }
        assert!(popups.admit(1, 99, "about:blank").is_none(), "the cap refuses the next popup");
        assert!(
            popups.admit(2, 0, "about:blank").is_some(),
            "another page's sign-in is not refused by this page's popups",
        );
        popups.aborted(1, 7);
        assert!(
            popups.admit(1, 100, "about:blank").is_some(),
            "an aborted popup's place is usable again"
        );
        // **A repeated popup id does not double-count.** The ids CEF hands
        // out are unique today; an overwritten entry would leak the replaced
        // place for the host's life, and the cap would refuse a sign-in that
        // has not used its places up.
        let repeated = gate(&ring);
        let mut fresh = Vec::new();
        for _ in 0..2 {
            fresh.push(repeated.admit(1, 0, "about:blank").expect("the same popup id"));
        }
        for id in 1..super::MAX_POPUP_TABS {
            fresh.push(
                repeated
                    .admit(1, i32::try_from(id).expect("a popup id"), "about:blank")
                    .expect("a place the leak would have taken"),
            );
        }
        assert!(
            repeated.admit(1, 99, "about:blank").is_none(),
            "the repeated id counted once, so the cap is exactly full"
        );
        drop(fresh);
        drop(held);
        crate::osr::Ring::unlink(&name);
    }

    /// **A second release of one popup frees no place.** CEF's abort, the
    /// browser's close and the host's own give-up are different reports of
    /// one popup, and giving the place back twice would free a place another
    /// popup is still using - the cap walked past from the inside.
    #[test]
    fn a_second_release_of_one_popup_frees_no_place() {
        let name = format!("/forge-cef-test-release-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        let mut held = Vec::new();
        for id in 0..super::MAX_POPUP_TABS {
            held.push(
                popups
                    .admit(1, i32::try_from(id).expect("a popup id"), "about:blank")
                    .expect("a popup within the cap"),
            );
        }
        // One popup's place, reported twice.
        let (_, one) = &held[0];
        popups.release(&one.place);
        popups.release(&one.place);
        assert!(
            popups.admit(1, 100, "about:blank").is_some(),
            "the popup's place is the one that comes back"
        );
        assert!(
            popups.admit(1, 101, "about:blank").is_none(),
            "the second release of the same popup freed nothing"
        );
        // **An abort frees exactly the place its popup id names.** A report
        // for an id no place holds is answered, not counted against the page.
        popups.aborted(1, 424_242);
        assert!(
            popups.admit(1, 102, "about:blank").is_none(),
            "an abort for a popup that holds no place frees nothing"
        );
        drop(held);
        crate::osr::Ring::unlink(&name);
    }

    /// **A popup admitted clears the refusals opening one hits.** The page is
    /// past an unbound handle or a failed create, so its next one writes a
    /// line; a mutation that clears only the cap leaves the non-cap kinds
    /// asserted below behind.
    #[test]
    fn a_popup_admitted_clears_the_refusals_it_resolves() {
        let name = format!("/forge-cef-test-clear-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        let mut held = Vec::new();
        for id in 0..super::MAX_POPUP_TABS {
            held.push(
                popups
                    .admit(1, i32::try_from(id).expect("a popup id"), "about:blank")
                    .expect("a popup within the cap"),
            );
        }
        assert!(popups.admit(1, 99, "about:blank").is_none(), "the cap refuses the next popup");
        assert!(
            !popups.first_refusal(1, "cap"),
            "the cap line is behind the opener once it has been written"
        );
        assert!(popups.first_refusal(1, "unbound"), "the page's unbound refusal is recorded");
        assert!(
            popups.first_refusal(1, "create_failed"),
            "the page's create-failure refusal is recorded"
        );
        assert!(popups.first_refusal(1, "subframe"), "the page's subframe refusal is recorded");
        let (_, one) = &held[0];
        popups.release(&one.place);
        // **The cap refusal survives the admission.** The release before it
        // is what ends the episode, so an admission that cleared a cap
        // refusal would hide a later one from the same page.
        assert!(popups.first_refusal(1, "cap"), "the cap refusal is recorded for the page");
        assert!(popups.admit(1, 100, "about:blank").is_some(), "the freed place admits a popup");
        assert!(
            popups.first_refusal(1, "unbound"),
            "the popup admitted cleared the unbound refusal, so the next one writes a line"
        );
        assert!(
            popups.first_refusal(1, "create_failed"),
            "the popup admitted cleared the create-failure refusal, so the next one writes a line"
        );
        assert!(
            popups.first_refusal(1, "subframe"),
            "the popup admitted cleared the subframe refusal, so the next one writes a line"
        );
        assert!(
            !popups.first_refusal(1, "cap"),
            "the popup admitted left the cap refusal alone; only a freed place ends it"
        );
        drop(held);
        crate::osr::Ring::unlink(&name);
    }

    /// **A cap refusal writes one line per episode, and a freed place ends
    /// it.** The second refusal in one episode answers no line; a place given
    /// back is the page's way under the cap, so the refusal after it is a new
    /// episode.
    #[test]
    fn the_cap_refusal_is_one_line_until_a_place_frees() {
        let name = format!("/forge-cef-test-cap-line-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        let mut held = Vec::new();
        for id in 0..super::MAX_POPUP_TABS {
            held.push(
                popups
                    .admit(1, i32::try_from(id).expect("a popup id"), "about:blank")
                    .expect("a popup within the cap"),
            );
        }
        assert!(popups.admit(1, 99, "about:blank").is_none(), "the cap refuses the next popup");
        assert!(
            popups.cap_refusal(1, 98, "about:blank").is_none(),
            "a second over-cap attempt writes nothing while the episode holds"
        );
        let (_, one) = &held[0];
        popups.release(&one.place);
        assert!(popups.admit(1, 100, "about:blank").is_some(), "the freed place admits a popup");
        assert!(
            popups.cap_refusal(1, 97, "about:blank").is_some(),
            "the freed place ended the episode, so the next cap refusal writes a line"
        );
        drop(held);
        crate::osr::Ring::unlink(&name);
    }

    /// **A spent counter refuses the popup and gives its place back.** The
    /// admission takes the opener's count and its place before the number; a
    /// refusal that left either behind would shrink the opener's cap for the
    /// host's whole life. The line is the opener document's first, and it
    /// names the event.
    #[test]
    fn a_spent_counter_refuses_the_popup_and_returns_its_place() {
        let name = format!("/forge-cef-test-spent-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        popups.tabs.lock().unwrap_or_else(std::sync::PoisonError::into_inner).next = u16::MAX;
        assert!(popups.admit(1, 0, "about:blank").is_none(), "a spent counter refuses the popup");
        assert!(
            popups.live.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty(),
            "the refused popup's place is returned"
        );
        assert!(
            popups.places.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty(),
            "a refused popup takes no place key"
        );
        assert!(
            popups.exhausted_line(Some(1), "about:blank").is_none(),
            "the admission already wrote the opener's line"
        );
        let line = popups
            .exhausted_line(Some(2), "https://a.test/")
            .expect("the opener's first refusal is logged");
        assert!(
            line.contains("event_name cef_tab_numbers_exhausted,"),
            "the line names its event: {line}"
        );
        assert!(line.contains("opener 2"), "the line names the opener: {line}");
        assert!(
            popups.exhausted_line(Some(2), "https://a.test/").is_none(),
            "a second attempt in the same document writes nothing"
        );
        crate::osr::Ring::unlink(&name);
    }

    /// **A create fault's line names its event and its opener.** A requested
    /// tab has none to name and a popup names its opener.
    #[test]
    fn a_create_fault_line_names_its_event_and_opener() {
        let name = format!("/forge-create-line-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        let line = popups
            .create_failed_line(None, 5, "https://a.test/")
            .expect("the first requested-tab fault is logged");
        assert!(
            line.contains("event_name cef_tab_create_failed,"),
            "the line names its event: {line}"
        );
        assert!(line.contains("opener none"), "the line says there is no opener: {line}");
        let line = popups
            .create_failed_line(Some(3), 6, "https://b.test/")
            .expect("the opener's first fault is logged");
        assert!(line.contains("opener 3"), "the line names the opener: {line}");
        crate::osr::Ring::unlink(&name);
    }

    /// **A create's note writes its line once and re-arms on a created tab.**
    /// The four combinations: a failed popup writes one line per opener and a
    /// created popup writes none; a failed requested tab writes one line
    /// across attempts, and a created one re-arms it.
    #[test]
    fn a_create_note_writes_once_and_re_arms() {
        let name = format!("/forge-create-note-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        // A popup's fault is its opener's, and another opener is a new one.
        assert!(
            popups.note_create(Some(3), 5, "https://a.test/", false),
            "a popup's first fault writes a line"
        );
        assert!(
            !popups.note_create(Some(3), 6, "https://a.test/", false),
            "the same opener's second fault writes nothing"
        );
        assert!(
            popups.note_create(Some(4), 7, "https://a.test/", false),
            "another opener's first fault writes a line"
        );
        // A created popup writes nothing of its own.
        assert!(
            !popups.note_create(Some(3), 8, "https://a.test/", true),
            "a created popup writes nothing"
        );
        // A requested tab's fault is the host's, whatever number it takes.
        assert!(
            popups.note_create(None, 9, "https://a.test/", false),
            "the first requested-tab fault writes a line"
        );
        assert!(
            !popups.note_create(None, 10, "https://a.test/", false),
            "a second requested-tab fault writes nothing, even from another number"
        );
        // A created requested tab re-arms the host's line.
        assert!(
            !popups.note_create(None, 11, "https://a.test/", true),
            "a created requested tab writes nothing"
        );
        assert!(
            popups.note_create(None, 12, "https://a.test/", false),
            "the created tab re-armed the host's line"
        );
        crate::osr::Ring::unlink(&name);
    }

    /// **A subframe's message writes one line per tab until its main frame
    /// navigates, or it ends, or a popup is admitted from it.** The line names
    /// its event, the frame can repeat it, and another tab's first message is
    /// a new episode.
    #[test]
    fn a_repeated_subframe_message_logs_once() {
        let name = format!("/forge-cef-test-subframe-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        let line = popups.refuse_subframe(7).expect("the first message from a frame is logged");
        assert!(
            line.contains("event_name cef_opener_bridge_subframe,"),
            "the line names its event: {line}"
        );
        assert!(line.contains("tab 7"), "the line names the tab: {line}");
        assert!(popups.refuse_subframe(7).is_none(), "the same frame's next message is not");
        assert!(
            popups.refuse_subframe(8).is_some(),
            "another tab's first message is a new episode"
        );
        crate::osr::Ring::unlink(&name);
    }

    /// **A popup's address is offered while nothing is in flight, and never
    /// without end.** It is the one navigation this engine commits for a
    /// popup an auto-attached client holds, so a load in flight is given its
    /// time rather than navigated over, and a popup that never gets there is
    /// given up instead of sitting blank for ever.
    #[test]
    fn a_popups_address_is_offered_once_its_load_is_clear() {
        use super::{BootNow, BootStep, boot_step};
        let state = |committed: &'static str, started: bool, loading: bool, tries: u8| BootNow {
            committed,
            started,
            loading,
            tries,
            now: 100,
            give_up_at: 1_000,
            target: "https://auth/",
        };
        assert_eq!(
            boot_step(&state("", false, true, 8)),
            BootStep::Wait,
            "a navigation in flight is given its time, never navigated over",
        );
        assert_eq!(
            boot_step(&state("", false, false, 8)),
            BootStep::Address,
            "a popup that has not started gets its address",
        );
        assert_eq!(
            boot_step(&state("", false, false, 0)),
            BootStep::GiveUp,
            "an address that never starts with no offers left gives the tab up",
        );
        assert_eq!(
            boot_step(&state("about:blank", true, true, 8)),
            BootStep::Wait,
            "a popup does not navigate over a load in flight",
        );
        assert_eq!(
            boot_step(&state("about:blank", true, false, 8)),
            BootStep::Address,
            "the address is offered while nothing is in flight",
        );
        assert_eq!(
            boot_step(&state("https://auth/", true, false, 8)),
            BootStep::Done,
            "the address is up",
        );
        assert_eq!(
            boot_step(&state("https://auth/redirect", true, false, 8)),
            BootStep::Done,
            "a redirect is a finish, never a reason to navigate back",
        );
        assert_eq!(
            boot_step(&state("about:blank", true, false, 0)),
            BootStep::GiveUp,
            "an address that never starts with no offers left gives the tab up",
        );
        let mut blank = state("about:blank", true, false, 8);
        blank.target = "about:blank";
        assert_eq!(
            boot_step(&blank),
            BootStep::Done,
            "a popup asked for about:blank is already where it was asked to be",
        );
        let mut late = state("about:blank", true, false, 8);
        late.now = 1_000;
        assert_eq!(
            boot_step(&late),
            BootStep::GiveUp,
            "a popup that never gets there is given up at its deadline",
        );
        let mut loading_late = state("", false, true, 8);
        loading_late.now = 1_000;
        assert_eq!(
            boot_step(&loading_late),
            BootStep::Wait,
            "a load still in flight is kept past the deadline, not given up",
        );
        let mut done_late = state("https://auth/", true, false, 8);
        done_late.now = 1_000;
        assert_eq!(
            boot_step(&done_late),
            BootStep::Done,
            "a popup that reached its document inside the last grace is not given up",
        );
    }

    /// **A popup born inside a create waits for the create's own birth.** The
    /// create's browser is made in a nested CEF loop, so a popup adopted
    /// during it would report before the tab being created, and the service
    /// pairs births to pages in that order.
    #[test]
    fn a_popup_birth_inside_a_create_waits_its_turn() {
        use std::sync::atomic::Ordering;
        let name = format!("/forge-cef-test-holds-{}", std::process::id());
        let ring = std::sync::Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let popups = gate(&ring);
        let held = popups.hold_births();
        popups.emit_birth(crate::control::HostEvent::TabCreated {
            tab: 2,
            opener: Some(1),
            url: "u".to_owned(),
            request: None,
        });
        assert_eq!(
            popups.held_births.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len(),
            1,
            "a birth taken during the create waits",
        );
        assert!(popups.holding_births.load(Ordering::Acquire) > 0, "the create holds births");
        drop(held);
        assert_eq!(
            popups.holding_births.load(Ordering::Acquire),
            0,
            "the hold ends with the create"
        );
        assert!(
            popups.held_births.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_empty(),
            "the kept birth is flushed after the create's own",
        );
        // **The flush hands the births to the sender**, not merely empties
        // the buffer: a flush that took them and dropped them would leave the
        // buffer empty and the popup unreported.
        let held = popups.hold_births();
        popups.emit_birth(crate::control::HostEvent::TabCreated {
            tab: 3,
            opener: Some(1),
            url: "u".to_owned(),
            request: None,
        });
        let flushed = popups.flush_births();
        assert_eq!(flushed.len(), 1, "the flush hands the held birth on: {flushed:?}");
        assert!(
            matches!(flushed.first(), Some(crate::control::HostEvent::TabCreated { tab: 3, .. })),
            "the birth it hands on is the one that was held: {flushed:?}",
        );
        drop(held);
        crate::osr::Ring::unlink(&name);
    }

    /// **A fd the watch cannot read is refused.** The value is the read end
    /// the spawn dup'd across exec; anything below the standard streams is
    /// not such an fd. A line with no pipe is a host launched by hand.
    #[test]
    fn a_parent_pipe_is_parsed_and_a_standard_stream_is_refused() {
        use std::os::fd::AsRawFd as _;
        let args = |extra: &[&str]| -> Vec<String> {
            let mut args = vec!["--cef-host", "--profile", "/p/cef", "--port", "1", "--ring", "r"];
            args.extend_from_slice(extra);
            args.into_iter().map(str::to_owned).collect()
        };
        assert!(super::parse(args(&[])).expect("parses").parent.is_none());
        let (read, _write) = super::parent_watch_pipe();
        // The parsed Args holds only the fd number, so this test keeps the
        // read end's owner alive for the call and lets it close at the end.
        let piped =
            super::parse(args(&["--parent-pipe", &read.as_raw_fd().to_string()])).expect("parses");
        assert!(piped.parent.is_some(), "the fd is the watch");
        assert!(
            super::parse(args(&["--parent-pipe", "2"])).is_none(),
            "a standard stream is refused"
        );
        assert!(
            super::parse(args(&["--parent-pipe"])).is_none(),
            "a flag with no value is refused"
        );
    }

    /// **The loop stops on a signal or on EOF of the pipe**, and the
    /// interval is the watch's own: the fd is not read on every pass. EOF is
    /// the server's process ending - no pid is involved, so neither a
    /// recycled pid nor an unreaped zombie can read as alive (#2499).
    #[test]
    fn a_gone_server_stops_the_host_on_the_watch_interval() {
        use super::{PARENT_CHECK_EVERY_MS, ParentWatch, Stop, stopping};
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut unwatched = ParentWatch::new(None);
        assert_eq!(stopping(&stop, &mut unwatched, u64::MAX), None, "no parent is not a stop");

        let (read, write) = super::parent_watch_pipe();
        let mut alive = ParentWatch::new(Some(read));
        assert_eq!(stopping(&stop, &mut alive, 0), None, "the interval comes first");
        assert_eq!(
            stopping(&stop, &mut alive, PARENT_CHECK_EVERY_MS),
            None,
            "the writer is still open, so the server is alive"
        );

        drop(write);
        assert_eq!(
            stopping(&stop, &mut alive, PARENT_CHECK_EVERY_MS * 2),
            Some(Stop::ServerGone),
            "EOF on the watch pipe is the shutdown"
        );

        let signalled = std::sync::atomic::AtomicBool::new(true);
        let mut fresh = ParentWatch::new(None);
        assert_eq!(stopping(&signalled, &mut fresh, 0), Some(Stop::Signal));
    }

    /// **The frame socket is passed, not derived.** A profile under
    /// app-support is longer than a Unix socket's address allows, so the
    /// server binds a short path and the host writes to exactly that; a host
    /// launched by hand still falls back to the profile's own directory.
    #[test]
    fn the_servers_frame_socket_wins_and_a_hand_launch_falls_back() {
        let base = |extra: &[&'static str]| -> Vec<&'static str> {
            let mut args = vec!["--cef-host", "--profile", "/p/cef", "--port", "1", "--ring", "r"];
            args.extend_from_slice(extra);
            args
        };
        let parsed = super::parse(
            base(&["--frames", "/tmp/forge-501/frames-9-1.sock"]).into_iter().map(str::to_owned),
        )
        .expect("the arguments parse");
        assert_eq!(
            parsed.frames,
            std::path::PathBuf::from("/tmp/forge-501/frames-9-1.sock"),
            "the server's path is what the host connects to",
        );
        let fallback =
            super::parse(base(&[]).into_iter().map(str::to_owned)).expect("the fallback parses");
        assert_eq!(fallback.frames, std::path::Path::new("/p/cef/frames.sock"));
    }

    /// The tab's settled answer, for the tests that probe it.
    fn answer(focus: &super::FocusState) -> bool {
        focus.inner.lock().unwrap_or_else(super::PoisonError::into_inner).current
    }

    /// **The tab's answer is its frames'**, so a cross-origin iframe's
    /// `true` and its parent's `false` settle either way round. The two
    /// reports come from different renderer processes with nothing ordering
    /// them, and a single last-wins flag would leave the tab non-editable
    /// while the iframe's field holds the keyboard.
    #[test]
    fn focus_settles_per_frame_in_either_order() {
        let focus = super::FocusState::new(1);
        assert!(!focus.settle("main", false), "no frame has an editable node");
        assert!(focus.settle("iframe", true), "an iframe's field is text entry");
        assert!(answer(&focus), "and the tab follows its frame");
        assert!(!focus.settle("iframe", false), "losing it is not a new line");
        assert!(!answer(&focus), "while it drops the tab");

        // **The other arrival order**: the iframe's report overtakes the
        // parent's blur, and the parent's `false` must not undo it.
        let overtaken = super::FocusState::new(1);
        assert!(overtaken.settle("iframe", true), "the iframe's answer lands first");
        assert!(
            !overtaken.settle("main", false),
            "the parent's blur is the same answer, not a line",
        );
        assert!(answer(&overtaken), "and it does not undo the iframe's field");
    }

    /// **A new document drops every frame's answer**, and a `true` that
    /// never reached the wire is not written after the fact: a stale `true`
    /// from a subframe that is gone must not keep the phone's keyboard up,
    /// and a superseded one must not raise it.
    #[test]
    fn a_reset_clears_every_frame_and_holds_the_drop() {
        let focus = super::FocusState::new(1);
        assert!(focus.settle("main", true), "the first answer owes a line");
        assert!(!focus.settle("iframe", true), "a second frame inside the window is not");
        focus.reset();
        {
            let inner = focus.inner.lock().unwrap_or_else(super::PoisonError::into_inner);
            assert!(inner.frames.is_empty(), "every frame's answer went");
            assert!(!inner.current, "and the tab answers false");
        }
        focus.flush();
        {
            let inner = focus.inner.lock().unwrap_or_else(super::PoisonError::into_inner);
            assert!(!inner.emitted, "the superseded true never went out");
            assert!(!inner.scheduled, "and the window is closed");
        }

        // **When the `true` HAS gone out, the reset owes the drop.**
        let emitted = super::FocusState::new(1);
        assert!(emitted.settle("main", true));
        emitted.flush();
        assert!(
            emitted.inner.lock().unwrap_or_else(super::PoisonError::into_inner).emitted,
            "the editable answer went out",
        );
        emitted.reset();
        assert!(
            emitted.inner.lock().unwrap_or_else(super::PoisonError::into_inner).scheduled,
            "the drop is on its way",
        );
        emitted.flush();
        assert!(
            !emitted.inner.lock().unwrap_or_else(super::PoisonError::into_inner).emitted,
            "and it is what the next line says",
        );
    }

    /// **A burst is coalesced to one line, and the last answer still goes.**
    /// A page that focuses in a loop must not wake a viewer per toggle.
    #[test]
    fn a_focus_burst_is_coalesced_to_its_final_answer() {
        let focus = super::FocusState::new(1);
        assert!(!focus.settle("main", false), "nothing changed");
        assert!(focus.settle("main", true), "the first change owes a line");
        assert!(!focus.settle("main", false), "a toggle inside the window is held");
        assert!(!focus.settle("main", true), "and so is the next one");

        // The window closes with the last answer held, and writes it.
        focus.flush();
        assert!(
            focus.inner.lock().unwrap_or_else(super::PoisonError::into_inner).emitted,
            "the held answer is what went out",
        );
        assert!(focus.settle("main", false), "a change after the window owes the next line",);
    }

    /// **Every view of one tab is handed that tab's render handler.** CEF
    /// asks the client for one per view and a popup is its own view, so a
    /// handler built per call kept the popup's bounds in one instance and
    /// painted into another - the menu was drawn with no position and never
    /// composited.
    #[test]
    fn every_view_shares_its_tabs_render_handler() {
        use cef::ImplClient as _;
        use cef::rc::Rc as _;
        use std::sync::Arc;
        let name = format!("/forge-cef-test-shared-{}", std::process::id());
        let ring = Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let paint: crate::osr::PaintSink = Arc::new(|_, _, _, _, _| {});
        let audio: crate::audio::AudioSink = Arc::new(|_| {});
        let popups = gate(&ring);
        let client = super::HostClient::new(
            1,
            Arc::clone(&ring),
            paint,
            crate::osr::handler(Arc::clone(&ring), None),
            crate::audio::handler(1, 1, audio),
            crate::download::handler(std::path::Path::new("/tmp"), 1),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            popups,
            super::FocusState::new(1),
            None,
        );
        let view = client.render_handler().expect("a handler for the page");
        let popup = client.render_handler().expect("a handler for the popup");
        assert!(
            std::ptr::eq(view.as_base(), popup.as_base()),
            "the popup's bounds and the popup's paint must meet in one state"
        );
        crate::osr::Ring::unlink(&name);
    }

    /// **The newest paint wins, except over a queued popup frame.** The view
    /// CEF paints after a popup is the page with the menu removed, so a
    /// plain repaint arrives immediately after the popup frame and would
    /// replace it before the encoder saw it - the menu lost a second way.
    #[test]
    fn a_queued_popup_frame_is_not_replaced_by_a_plain_repaint() {
        use super::{PaintJob, replaces_queued};
        let job =
            |distinct| PaintJob { pixels: Vec::new(), width: 1, height: 1, page: (1, 1), distinct };
        assert!(replaces_queued(None, false), "an empty slot takes any paint");
        assert!(replaces_queued(None, true), "including a popup's");
        assert!(replaces_queued(Some(&job(false)), false), "the newest plain paint wins");
        assert!(replaces_queued(Some(&job(false)), true), "a popup frame wins over it");
        assert!(
            !replaces_queued(Some(&job(true)), false),
            "a queued popup frame survives the plain repaint after it"
        );
        assert!(replaces_queued(Some(&job(true)), true), "a newer popup state wins");
    }

    /// **The encoder takes the newest paint, not the one it first saw.** A
    /// burst coalesces to its LAST state: a typed character or a page reload
    /// whose paint landed inside the frame interval used to be dropped, and
    /// a still page never painted it again.
    #[test]
    fn the_encoder_takes_the_newest_paint() {
        use super::{Job, PaintJob, take_for_encode};
        let job = |width, distinct| {
            Job::Frame(PaintJob { pixels: Vec::new(), width, height: 1, page: (1, 1), distinct })
        };
        let mut queue =
            std::collections::VecDeque::from([job(1, false), job(2, false), job(3, false)]);
        let Some(Job::Frame(next)) = take_for_encode(&mut queue) else {
            panic!("a queued frame is taken");
        };
        assert_eq!(next.width, 3, "the newest paint is the one encoded");
        assert_eq!(queue.len(), 2, "and only the taken frame leaves the queue");
    }

    /// **A queued popup is encoded before a newer plain repaint.** CEF paints
    /// the view without the menu right after the popup, so preferring the
    /// newest plain paint would lose the menu the popup frame carries.
    #[test]
    fn the_encoder_takes_a_queued_popup_first() {
        use super::{Job, PaintJob, take_for_encode};
        let job = |width, distinct| {
            Job::Frame(PaintJob { pixels: Vec::new(), width, height: 1, page: (1, 1), distinct })
        };
        let mut queue = std::collections::VecDeque::from([job(1, true), job(2, false)]);
        let Some(Job::Frame(next)) = take_for_encode(&mut queue) else {
            panic!("a queued frame is taken");
        };
        assert_eq!(next.width, 1, "the queued popup goes first");
        assert!(next.distinct);
    }

    /// **A held popup is not replaced by a newer plain paint.** The worker
    /// pops the oldest frame, waits out the interval, then looks for a newer
    /// one; if the frame it holds is the popup, that frame is the one whose
    /// menu must reach the viewer.
    #[test]
    fn a_held_popup_survives_a_newer_plain_paint() {
        use super::{Job, PaintJob, encode_target};
        let paint = |width, distinct| PaintJob {
            pixels: Vec::new(),
            width,
            height: 1,
            page: (1, 1),
            distinct,
        };
        let next = encode_target(paint(1, true), Some(Job::Frame(paint(2, false))));
        assert_eq!(next.width, 1, "the held popup is encoded");
        assert!(next.distinct);
    }

    /// **A tab's number is reserved before its browser exists, never peeked.**
    /// The browser is created synchronously and CEF runs a nested message loop
    /// inside that call, so a page's popup can be admitted while a tab's create
    /// is still running; a peek that waited for the create to finish would hand
    /// the same number to both.
    #[test]
    fn a_number_is_reserved_before_its_browser_exists() {
        use super::Tabs;
        let mut tabs = Tabs::default();
        let first = tabs.reserve().expect("a number is free");
        assert_eq!(tabs.reserve(), Some(first + 1), "each reservation takes the next number");
        assert_eq!(tabs.open.len(), 0, "a reservation opens no row of its own");
    }

    /// **A failed create gives its number back.** A page can repeat a refusal
    /// (a handle that is not ours, or one it already used), and a number spent
    /// per refusal would walk the counter to its u16 ceiling, where the next
    /// insert lands on a live tab.
    #[test]
    fn a_failed_create_does_not_consume_a_number() {
        use super::Tabs;
        let mut tabs = Tabs::default();
        let refused = tabs.reserve().expect("a number is free");
        tabs.unreserve(refused);
        assert_eq!(tabs.reserve(), Some(refused), "the refused number is handed out again");
    }

    /// **The counter refuses at its ceiling rather than repeating a number.**
    /// The number's width is the wire's, so it cannot widen and must not wrap.
    /// The saturating counter used to hand out `u16::MAX` for every
    /// reservation after the ceiling, and a caller that inserted the number
    /// again would replace the tab that still held it.
    #[test]
    fn a_spent_counter_hands_out_no_number_at_all() {
        use super::Tabs;
        let mut tabs = Tabs { next: u16::MAX - 1, ..Tabs::default() };
        let last = tabs.reserve().expect("the ceiling is a number the counter hands out");
        assert_eq!(last, u16::MAX, "the last number the counter can hand out is the ceiling");
        assert_eq!(tabs.reserve(), None, "a spent counter hands out no number at all");
        assert_eq!(tabs.reserve(), None, "and stays spent rather than repeating the last one");
    }

    /// **A nested admission keeps its number.** A create can run CEF's loop
    /// and admit a popup before failing; the outer create's unreserve must
    /// not rewind over the reservation the popup already holds.
    #[test]
    fn unreserve_leaves_a_later_reservation_alone() {
        use super::Tabs;
        let mut tabs = Tabs::default();
        let outer = tabs.reserve().expect("a number is free");
        let inner = tabs.reserve().expect("the next number is free");
        tabs.unreserve(outer);
        assert_eq!(tabs.reserve(), Some(inner + 1), "the inner reservation keeps its place");
    }

    /// **A cue is acknowledged only when there is a host to show.** The
    /// visibility calls and the published sequence follow each other, so a
    /// cue no host answered stays unacknowledged and the caller's bound is
    /// what ends its wait.
    #[test]
    fn a_show_cue_is_acknowledged_only_with_a_host() {
        use super::{Header, ShowTarget, apply_show};
        use crate::osr::Ring;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Recorder<'a> {
            header: &'a Header,
            called: AtomicBool,
            ack_seen: AtomicBool,
        }
        impl ShowTarget for Recorder<'_> {
            fn show_page(&self) {
                self.called.store(true, Ordering::Release);
                self.ack_seen.store(
                    self.header.show_applied.load(Ordering::Acquire) != 0,
                    Ordering::Release,
                );
            }
        }

        let name = "/forge-cef-test-cue-ack";
        let ring = Ring::create(name, 4, 4).expect("a test ring");
        // SAFETY: the ring is mapped and holds a header.
        let header = unsafe { &*ring.header() };
        let recorder =
            Recorder { header, called: AtomicBool::new(false), ack_seen: AtomicBool::new(false) };

        apply_show(header, 3, &[]);
        assert_eq!(
            header.show_applied.load(Ordering::Acquire),
            0,
            "a cue with no host is not acknowledged as applied"
        );
        assert!(!recorder.called.load(Ordering::Acquire), "no host, no visibility calls");

        apply_show(header, 3, &[&recorder]);
        assert!(recorder.called.load(Ordering::Acquire), "the host took the visibility calls");
        assert!(
            !recorder.ack_seen.load(Ordering::Acquire),
            "the acknowledgment follows the visibility calls, never precedes them"
        );
        assert_eq!(
            header.show_applied.load(Ordering::Acquire),
            3,
            "the applied sequence follows the visibility calls"
        );
        Ring::unlink(name);
    }

    /// **A cue already in the header when the loop starts is acted on.** The
    /// applied marker starts at zero, not at the header's value, or a driver
    /// that cued while the process was coming up would wait out its bound
    /// against a cue this loop considered already handled.
    #[test]
    fn a_cue_written_before_the_loop_starts_is_acted_on() {
        use super::ShowCue;
        let mut cues = ShowCue::default();
        assert_eq!(cues.take(1), Some(1), "a cue already in the header is applied");
        assert_eq!(cues.take(1), None, "an unchanged cue is not applied twice");
        assert_eq!(cues.take(2), Some(2), "the next cue is applied");
    }
}
