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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use cef::*;

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
    /// would be handed out twice. A browser that fails to create burns its
    /// number, which costs a gap in a numbering nothing reads back.
    fn reserve(&mut self) -> u16 {
        let tab = self.next.saturating_add(1).max(1);
        self.next = tab;
        tab
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

/// What turns a page's `window.open` into a tab of this profile.
///
/// **CEF creates the popup's browser itself.** A host that cancels the popup
/// and opens a tab in its place leaves `window.open` answering null, and every
/// sign-in flow reads null as "the pop-up was blocked" - the tab it substituted
/// never helps, because the page has already failed. The gate admits the popup,
/// hands CEF a client of its own so it paints and takes input like any other
/// tab, and registers the browser as a tab the moment CEF reports it created.
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
    /// Popup births taken while a tab is being created, and whether a create
    /// holds them. **A create's browser is made inside a nested CEF loop**,
    /// so a popup adopted during it would report before the tab the create
    /// is making, and the service pairs births to pages by that order.
    held_births: Mutex<Vec<crate::control::HostEvent>>,
    holding_births: AtomicBool,
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
                eprintln!(
                    "cef host: a page's popup was refused (event_name cef_popup_refused, \
                     opener {opener}, popup {popup_id}, url {url})"
                );
                return None;
            }
            *held += 1;
        }
        let place = Arc::new(Place { opener, released: AtomicBool::new(false) });
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
        let tab = self.tabs.lock().unwrap_or_else(PoisonError::into_inner).reserve();
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
        self.holding_births.store(true, Ordering::Release);
        BirthHold { popups: self }
    }

    /// Announce a tab's birth, or keep it while a create holds births.
    fn emit_birth(&self, event: crate::control::HostEvent) {
        if self.holding_births.load(Ordering::Acquire) {
            self.held_births.lock().unwrap_or_else(PoisonError::into_inner).push(event);
            return;
        }
        crate::control::emit(&event);
    }

    /// **The held births go after the create's own**, in the order they were
    /// adopted, which is the order the engine made them. Answers what it
    /// sent, so a test can hold it to sending rather than only to emptying
    /// the buffer.
    fn flush_births(&self) -> Vec<crate::control::HostEvent> {
        self.holding_births.store(false, Ordering::Release);
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
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = live.get_mut(&place.opener) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                live.remove(&place.opener);
            }
        }
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
             opener {opener}, url {})",
            self.profile, entry.tab, entry.url,
        );
        if let Some(place) = &ended.place {
            self.release(place);
        }
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
        crate::control::emit(&crate::control::HostEvent::TabClosed { tab });
    }
}

/// What every tab's callbacks and the pump share: the profile's one ring,
/// its one frame socket, and the tab map.
struct HostRuntime {
    profile: u16,
    tabs: Arc<Mutex<Tabs>>,
    inputs: Arc<Mutex<VecDeque<crate::input::HostInput>>>,
    popups: Arc<PopupGate>,
    recording: Arc<Recording>,
    /// Whether any viewer is attached; **the profile's**, not one tab's.
    paused: Arc<AtomicBool>,
}

impl HostRuntime {
    /// Open one tab at `url`. **The one place the host creates a browser**: the
    /// first tab and a server command both come through here. A page's popup
    /// is CEF's to create, and joins as a tab through `adopt`.
    ///
    /// **The number is reserved before the create, never after it.** The create
    /// is synchronous and CEF runs a nested message loop inside it, so a page's
    /// popup can be admitted and take a number while this call is still
    /// running; reserving first is what keeps the two apart.
    fn create_tab(&self, url: &str, request: Option<u64>) -> bool {
        // **A popup adopted inside this create reports after the tab it is
        // making.** The create is synchronous and CEF runs a nested loop in
        // it, so the popup's birth would otherwise reach the server first and
        // the service would pair it with this tab's page.
        let births = self.popups.hold_births();
        let tab = self.tabs.lock().unwrap_or_else(PoisonError::into_inner).reserve();
        let parts = self.popups.parts(tab);
        let mut client = self.popups.client(tab, &parts, None);
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
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&settings),
            None,
            // **The global context**: the one whose autofill preferences
            // `turn_off_autofill` sets.
            None,
        );
        let host = browser.as_ref().and_then(cef::ImplBrowser::host);
        let (Some(browser), Some(host)) = (browser, host) else {
            eprintln!(
                "cef host: a tab was not created (event_name cef_tab_create_failed, tab {tab}, \
                 url {url})"
            );
            return false;
        };
        // CEF's visibility and focus calls are re-asserted once a page is
        // there: the windowless surface has no window to inherit them from.
        host.was_hidden(0);
        host.set_focus(1);
        host.set_windowless_frame_rate(60);
        {
            let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.insert(
                tab,
                Tab {
                    browser,
                    opener: None,
                    loaded: Arc::clone(&parts.loaded),
                    place: None,
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
            crate::frame::Sink::new(
                self.profile,
                tab,
                self.popups.frames.clone(),
                self.popups.fps,
                self.popups.quality,
            ),
            Arc::clone(&self.paused),
            parts.key,
        );
        // **This birth goes out ahead of the popups the create held**, and it
        // is sent directly rather than through the hold for that reason.
        crate::control::emit(&crate::control::HostEvent::TabCreated {
            tab,
            opener: None,
            url: url.to_owned(),
            request,
        });
        drop(births);
        true
    }

    /// Close one tab and end its frame worker; a page target CEF has already
    /// lost is a no-op.
    fn close_tab(&self, tab: u16) {
        let removed = self.tabs.lock().unwrap_or_else(PoisonError::into_inner).end(tab);
        let Some(removed) = removed else { return };
        // **A popup's place goes back here too**, or a page that closes its
        // own popups through the host would keep counting them.
        if let Some(place) = &removed.place {
            self.popups.release(place);
        }
        if let Some(host) = removed.browser.host() {
            host.close_browser(1);
        }
        crate::control::emit(&crate::control::HostEvent::TabClosed { tab });
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
        profile: args.profile_id,
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
            holding_births: AtomicBool::new(false),
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

cef::wrap_load_handler! {
    struct HostLoadHandler {
        // **The popup bootstrap's one signal.** A popup that has started a
        // load of its own must never be navigated again, and a committed URL
        // cannot say that: the frame keeps its last URL while the next load
        // is in flight.
        loaded: Arc<AtomicBool>,
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
        /// way. **A crashed renderer is not this**: the browser outlives it,
        /// and nothing in this host watches render-process termination.
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.popups.forget(self.tab);
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

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.downloads.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(HostLoadHandler::new(Arc::clone(&self.loaded)))
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
            holding_births: std::sync::atomic::AtomicBool::new(false),
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
        assert!(popups.holding_births.load(Ordering::Acquire), "the create holds births");
        drop(held);
        assert!(!popups.holding_births.load(Ordering::Acquire), "the hold ends with the create");
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
        let first = tabs.reserve();
        assert_eq!(tabs.reserve(), first + 1, "each reservation takes the next number");
        assert_eq!(tabs.open.len(), 0, "a reservation opens no row of its own");
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
