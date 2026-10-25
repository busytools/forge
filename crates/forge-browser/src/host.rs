//! One profile's browser host: the process behind `--cef-host`.
//!
//! Arguments name the profile's cache root, the DevTools port and the frame
//! ring. The host initializes CEF, creates one windowless browser, reports
//! that it is live in the ring's header, and pumps CEF's work and a 60 Hz
//! external begin-frame cadence until it is ended. The server owns the
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

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use cef::*;

use crate::engine::{HostApp, elapsed_ms, initialize, pump};
use crate::osr::{Header, MAX_HEIGHT, MAX_WIDTH, Ring, STATE_FAILED, STATE_LIVE};

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
    /// The server that spawned this host: the process the loop watches, so a
    /// server killed without its exit path takes its hosts with it. `None`
    /// for a host a person or a test launched directly.
    pub parent: Option<u32>,
}

/// Parse `--cef-host --profile DIR --port N --ring NAME [--frames PATH]
/// [--size WxH] [--profile-id N] [--fps N] [--quality N] [--parent PID]`.
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
            "--parent" => {
                // **A pid the signal calls can carry**: `0` probes the
                // caller's own process group, and anything past `i32::MAX`
                // casts negative and probes another group id. Either reads
                // as alive forever, so a host line naming one is refused
                // rather than run unwatched.
                let pid = args.next()?.parse::<u32>().ok()?;
                if pid == 0 || pid > i32::MAX as u32 {
                    return None;
                }
                parent = Some(pid);
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

/// Whether `pid` names a running process. `EPERM` means it exists but belongs
/// to someone else, which is alive as far as this host is concerned.
fn running(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let checked = unsafe { libc::kill(pid as i32, 0) };
    checked == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The server recorded at spawn, and when the loop last asked after it.
struct ParentWatch {
    parent: Option<u32>,
    last: u64,
}

impl ParentWatch {
    fn new(parent: Option<u32>) -> Self {
        Self { parent, last: 0 }
    }

    /// Whether the server is gone, asked at most once per interval.
    ///
    /// **The pid comes from the spawn line, not `getppid`.** A `getppid` read
    /// at host start races the server's death: one that finds the host
    /// already reparented answers `1`, which `kill(1, 0)` reads as alive
    /// forever, and the watch never fires. The spawner's own pid names the
    /// process this host must outlive, whatever its parent becomes.
    fn gone(&mut self, now: u64) -> bool {
        let Some(pid) = self.parent else {
            return false;
        };
        if now.saturating_sub(self.last) < PARENT_CHECK_EVERY_MS {
            return false;
        }
        self.last = now;
        !running(pid)
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
    pending: Arc<Pending>,
    /// Ends the tab's frame worker when the tab goes.
    stop: Arc<AtomicBool>,
    /// A viewer's key cue: the tab's next frame is full quality.
    key: Arc<AtomicBool>,
}

/// The profile's open tabs, numbered in creation order and never renumbered.
#[derive(Default)]
struct Tabs {
    next: u16,
    open: BTreeMap<u16, Tab>,
}

impl Tabs {
    /// The number the next tab takes. **Peeked before creation and committed
    /// after it**, so a browser that failed to create burns no number and the
    /// host's numbering stays the server's numbering.
    fn peek(&self) -> u16 {
        self.next.saturating_add(1).max(1)
    }

    fn commit(&mut self, tab: u16) {
        self.next = tab;
    }
}

/// A tab a page asked for. **Created on the pump thread**, never inside the
/// CEF callback that asked; CEF does not tolerate a browser created under
/// its own popup callback.
struct CreateRequest {
    url: String,
    opener: u16,
}

/// The most popup tabs a page may queue before the pump has made room.
const MAX_TAB_REQUESTS: usize = 16;

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

/// What every tab's callbacks and the pump share: the profile's one ring,
/// its one frame socket, and the tab map.
struct HostRuntime {
    profile: u16,
    ring: Arc<Ring>,
    tabs: Arc<Mutex<Tabs>>,
    inputs: Arc<Mutex<VecDeque<crate::input::HostInput>>>,
    creates: Arc<Mutex<VecDeque<CreateRequest>>>,
    recording: Arc<Recording>,
    /// Whether any viewer is attached; **the profile's**, not one tab's.
    paused: Arc<AtomicBool>,
    frames: PathBuf,
    fps: u16,
    quality: u8,
    downloads: DownloadHandler,
}

impl HostRuntime {
    /// Open one tab at `url`. **The one place a browser is created**: the
    /// first tab, a server command and a page's popup all come through here.
    fn create_tab(&self, tab: u16, url: &str, opener: Option<u16>) -> bool {
        if self.tabs.lock().unwrap_or_else(PoisonError::into_inner).open.contains_key(&tab) {
            return false;
        }
        let pending: Arc<Pending> = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let key = Arc::new(AtomicBool::new(false));
        let paint: crate::osr::PaintSink = {
            let pending = Arc::clone(&pending);
            let paused = Arc::clone(&self.paused);
            let recording = Arc::clone(&self.recording);
            Arc::new(move |view, width, height, distinct| {
                // **A recording is not the viewers'.** A take runs whether or
                // not anyone is watching, so it is offered the viewport
                // before the encoding pause.
                recording.offer(tab, view, width, height);
                if paused.load(Ordering::Acquire) {
                    return;
                }
                push_paint(&pending, view, width, height, distinct);
            })
        };
        let audio: crate::audio::AudioSink = {
            let pending = Arc::clone(&pending);
            Arc::new(move |packet: crate::channel::Audio| push_audio(&pending, packet))
        };
        let render = crate::osr::handler(Arc::clone(&self.ring), Some(Arc::clone(&paint)));
        let page_audio = crate::audio::handler(self.profile, tab, Arc::clone(&audio));
        let mut client = HostClient::new(
            tab,
            Arc::clone(&self.ring),
            paint,
            render,
            page_audio,
            self.downloads.clone(),
            Arc::clone(&self.creates),
        );
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
            None,
        );
        let Some(browser) = browser else { return false };
        let Some(host) = browser.host() else { return false };
        // CEF's visibility and focus calls are re-asserted once a page is
        // there: the windowless surface has no window to inherit them from.
        host.was_hidden(0);
        host.set_focus(1);
        host.set_windowless_frame_rate(60);
        {
            let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.commit(tab);
            tabs.open.insert(
                tab,
                Tab {
                    browser,
                    pending: Arc::clone(&pending),
                    stop: Arc::clone(&stop),
                    key: Arc::clone(&key),
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
        crate::control::emit(&crate::control::HostEvent::TabCreated {
            tab,
            opener,
            url: url.to_owned(),
        });
        true
    }

    /// Close one tab and end its frame worker; a page target CEF has already
    /// lost is a no-op.
    fn close_tab(&self, tab: u16) {
        let removed = {
            let mut tabs = self.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.open.remove(&tab)
        };
        let Some(removed) = removed else { return };
        removed.stop.store(true, Ordering::Release);
        removed.pending.1.notify_all();
        if let Some(host) = removed.browser.host() {
            host.close_browser(1);
        }
        crate::control::emit(&crate::control::HostEvent::TabClosed { tab });
    }

    /// The number the next tab takes.
    fn next_tab(&self) -> u16 {
        self.tabs.lock().unwrap_or_else(PoisonError::into_inner).peek()
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

    /// Create the tabs pages asked for. **The pump thread does this**, after
    /// CEF's callback that asked has returned.
    fn drain_creates(&self) {
        loop {
            let request = {
                let mut creates = self.creates.lock().unwrap_or_else(PoisonError::into_inner);
                creates.pop_front()
            };
            let Some(request) = request else { return };
            let tab = self.next_tab();
            if !self.create_tab(tab, &request.url, Some(request.opener)) {
                eprintln!(
                    "cef host: a page's popup tab was not created (event_name \
                     cef_tab_create_failed, opener {}, url {})",
                    request.opener, request.url,
                );
            }
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
                crate::input::HostInput::TabNew { url } => {
                    let tab = self.next_tab();
                    let url = url.as_deref().unwrap_or("about:blank");
                    if !self.create_tab(tab, url, None) {
                        eprintln!(
                            "cef host: a requested tab was not created (event_name \
                             cef_tab_create_failed, tab {tab}, url {url})"
                        );
                    }
                }
                crate::input::HostInput::TabClose { tab } => self.close_tab(tab),
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
fn push_paint(pending: &Pending, view: &[u8], width: u32, height: u32, distinct: bool) {
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
    let paint = PaintJob { pixels: view.to_vec(), width, height, distinct };
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
                    sink.offer(&job.pixels, job.width, job.height);
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
    let runtime = HostRuntime {
        profile: args.profile_id,
        ring: Arc::clone(&ring),
        tabs: Arc::new(Mutex::new(Tabs::default())),
        inputs: Arc::new(Mutex::new(VecDeque::new())),
        creates: Arc::new(Mutex::new(VecDeque::new())),
        recording: Recording::new(),
        paused: Arc::new(AtomicBool::new(true)),
        frames: args.frames.clone(),
        fps: args.fps,
        quality: args.quality,
        downloads: crate::download::handler(&args.profile),
    };
    // **The host opens one blank boot tab.** It is the page the driver
    // attaches to, and a call navigates it once the veto is registered.
    //
    // **A browser with no host is not a live host**: the state and the log
    // below are what the server reads to say this profile is up, so a
    // browser that cannot be driven fails here rather than reporting live.
    if !runtime.create_tab(runtime.next_tab(), "about:blank", None) {
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
    // watch is the backstop; the server's own teardown is the fast path.
    let mut parent = ParentWatch::new(args.parent);
    // SAFETY: the header is mapped for this process's whole life.
    let mut last_request = unsafe { (*ring.header()).request_seq.load(Ordering::Acquire) };
    // SAFETY: as above.
    let mut last_key = unsafe { (*ring.header()).key_seq.load(Ordering::Acquire) };
    loop {
        if let Some(why) = stopping(&STOP, &mut parent, elapsed_ms()) {
            if why == Stop::ServerGone {
                eprintln!(
                    "cef host: the server that spawned this host is gone (event_name \
                     cef_parent_gone, pid {})",
                    args.parent.unwrap_or_default()
                );
            }
            break;
        }
        let header = ring.header();
        // SAFETY: the header is mapped for this process's whole life.
        let header_ref = unsafe { &*header };
        // The driver asked for another view size: adopt it and tell CEF.
        let request = header_ref.request_seq.load(Ordering::Acquire);
        if request != last_request {
            last_request = request;
            let width = header_ref.request_width.load(Ordering::Relaxed).clamp(1, MAX_WIDTH);
            let height = header_ref.request_height.load(Ordering::Relaxed).clamp(1, MAX_HEIGHT);
            let scale = header_ref.request_scale_milli.load(Ordering::Relaxed).clamp(100, 8_000);
            header_ref.width.store(width, Ordering::Relaxed);
            header_ref.height.store(height, Ordering::Relaxed);
            header_ref.scale_milli.store(scale, Ordering::Relaxed);
            runtime.each_host(|host| {
                // The pixel ratio first: it changes the DIP rect as well as
                // the buffer, and CEF lays out against both.
                host.notify_screen_info_changed();
                host.was_resized();
                // **A resize repaints even a still page**: a blank or static
                // document has no layout change to trigger one, and the
                // viewer would keep the old size until something else moved.
                host.invalidate(PaintElementType::VIEW);
            });
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
        runtime.drain_creates();
        runtime.apply_inputs();
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
    struct HostLoadHandler {}

    impl LoadHandler {
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
        creates: Arc<Mutex<VecDeque<CreateRequest>>>,
    }

    impl LifeSpanHandler {
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            // The browser outlives every surface: a close request proceeds
            // only on the host's own terms.
            0
        }

        /// **A popup is a tab of this host, not a second window.** CEF's own
        /// popup is cancelled; a web address it asked for is queued as a tab
        /// under this one, and the pump creates it once this callback has
        /// returned. Anything but a web page stays refused: a tab for one
        /// would be a blank the person never asked to see.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: i32,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: i32,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut i32>,
        ) -> i32 {
            let url = target_url.map(ToString::to_string).unwrap_or_default();
            if let Some(url) = popup_url(&url) {
                let mut creates = self.creates.lock().unwrap_or_else(PoisonError::into_inner);
                // **A page cannot grow this without end**: a scripted popup
                // loop floods the pump faster than it drains.
                if creates.len() < MAX_TAB_REQUESTS {
                    creates.push_back(CreateRequest { url: url.to_owned(), opener: self.tab });
                }
            }
            1
        }
    }
}

/// The address a popup asked for, when it is a web page a tab can open.
///
/// **Anything but a web page is blocked outright**: `about:blank` and a
/// `javascript:` popup have no page of their own to show.
fn popup_url(url: &str) -> Option<&str> {
    (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
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
        // Where a page's popup asks for a tab, for the pump to create.
        creates: Arc<Mutex<VecDeque<CreateRequest>>>,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }

        fn audio_handler(&self) -> Option<AudioHandler> {
            Some(self.audio.clone())
        }

        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(crate::dialog::handler())
        }

        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.downloads.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(HostLoadHandler::new())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(HostLifeSpanHandler::new(self.tab, Arc::clone(&self.creates)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::popup_url;

    /// **A pid the signal cannot name is refused.** `0` probes the caller's
    /// own group and a value past `i32::MAX` casts negative; either reads as
    /// alive forever, so a host line carrying one would never watch its
    /// server. A line with no parent is a host launched by hand.
    #[test]
    fn a_parent_line_is_parsed_and_an_unsignalable_pid_is_refused() {
        let args = |extra: &[&'static str]| -> Vec<String> {
            let mut args = vec!["--cef-host", "--profile", "/p/cef", "--port", "1", "--ring", "r"];
            args.extend_from_slice(extra);
            args.into_iter().map(str::to_owned).collect()
        };
        assert_eq!(super::parse(args(&[])).expect("parses").parent, None);
        assert_eq!(super::parse(args(&["--parent", "4242"])).expect("parses").parent, Some(4242));
        assert!(super::parse(args(&["--parent", "0"])).is_none(), "a group probe is refused");
        assert!(
            super::parse(args(&["--parent", "4294967296"])).is_none(),
            "a pid past i32::MAX is refused"
        );
        assert!(super::parse(args(&["--parent"])).is_none(), "a flag with no value is refused");
    }

    /// **The loop stops on a signal or on a gone server**, and the interval
    /// is the watch's own: a pid is not probed on every pass.
    #[test]
    fn a_gone_server_stops_the_host_on_the_watch_interval() {
        use super::{PARENT_CHECK_EVERY_MS, ParentWatch, Stop, stopping};
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut unwatched = ParentWatch::new(None);
        assert_eq!(stopping(&stop, &mut unwatched, u64::MAX), None, "no parent is not a stop");

        let mut alive = ParentWatch::new(Some(std::process::id()));
        assert_eq!(
            stopping(&stop, &mut alive, PARENT_CHECK_EVERY_MS),
            None,
            "the running server is alive"
        );

        let mut child =
            std::process::Command::new("/bin/sleep").arg("5").spawn().expect("a child to watch");
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait();
        let mut gone = ParentWatch::new(Some(pid));
        assert_eq!(stopping(&stop, &mut gone, 0), None, "the interval comes first");
        assert_eq!(
            stopping(&stop, &mut gone, PARENT_CHECK_EVERY_MS),
            Some(Stop::ServerGone),
            "a gone server is the shutdown"
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
        let paint: crate::osr::PaintSink = Arc::new(|_, _, _, _| {});
        let audio: crate::audio::AudioSink = Arc::new(|_| {});
        let client = super::HostClient::new(
            1,
            Arc::clone(&ring),
            paint,
            crate::osr::handler(Arc::clone(&ring), None),
            crate::audio::handler(1, 1, audio),
            crate::download::handler(std::path::Path::new("/tmp")),
            Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        );
        let view = client.render_handler().expect("a handler for the page");
        let popup = client.render_handler().expect("a handler for the popup");
        assert!(
            std::ptr::eq(view.as_base(), popup.as_base()),
            "the popup's bounds and the popup's paint must meet in one state"
        );
        crate::osr::Ring::unlink(&name);
    }

    /// **A tab's number is peeked before its browser exists and committed
    /// after**, so a browser that failed to create burns no number and the
    /// host's numbering stays what the server registered.
    #[test]
    fn a_failed_creation_burns_no_tab_number() {
        use super::Tabs;
        let mut tabs = Tabs::default();
        assert_eq!(tabs.peek(), 1, "the first tab is 1");
        tabs.commit(1);
        assert_eq!(tabs.peek(), 2, "the next tab is 2");
        tabs.commit(2);
        assert_eq!(tabs.peek(), 3);
    }

    /// **The newest paint wins, except over a queued popup frame.** The view
    /// CEF paints after a popup is the page with the menu removed, so a
    /// plain repaint arrives immediately after the popup frame and would
    /// replace it before the encoder saw it - the menu lost a second way.
    #[test]
    fn a_queued_popup_frame_is_not_replaced_by_a_plain_repaint() {
        use super::{PaintJob, replaces_queued};
        let job = |distinct| PaintJob { pixels: Vec::new(), width: 1, height: 1, distinct };
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
            Job::Frame(PaintJob { pixels: Vec::new(), width, height: 1, distinct })
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
            Job::Frame(PaintJob { pixels: Vec::new(), width, height: 1, distinct })
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
        let paint = |width, distinct| PaintJob { pixels: Vec::new(), width, height: 1, distinct };
        let next = encode_target(paint(1, true), Some(Job::Frame(paint(2, false))));
        assert_eq!(next.width, 1, "the held popup is encoded");
        assert!(next.distinct);
    }

    /// **A web page's popup becomes a tab, and anything else is refused.**
    /// The first is what a job site's "open in a new tab" link means; the
    /// second keeps a blank or scripted popup from becoming a tab nobody
    /// asked to see.
    #[test]
    fn a_web_popup_opens_a_tab_and_anything_else_is_blocked() {
        assert_eq!(popup_url("https://example.com/job"), Some("https://example.com/job"));
        assert_eq!(popup_url("http://example.com/"), Some("http://example.com/"));
        assert_eq!(popup_url("about:blank"), None);
        assert_eq!(popup_url("javascript:window.close()"), None);
        assert_eq!(popup_url(""), None);
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
