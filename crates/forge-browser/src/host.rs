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

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
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
}

/// Parse `--cef-host --profile DIR --port N --ring NAME [--frames PATH]
/// [--size WxH] [--profile-id N] [--fps N] [--quality N]`.
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
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => profile = args.next().map(PathBuf::from),
            "--port" => port = args.next().and_then(|value| value.parse::<u16>().ok()),
            "--ring" => ring = args.next(),
            "--frames" => frames = args.next().map(PathBuf::from),
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

/// Apply one cue: the visibility calls, then the acknowledgment, only with a
/// host to make them. A browser with no host cannot be shown, so its cue is
/// left unacknowledged for the caller's bound.
fn apply_show(header: &Header, show: u64, target: Option<&dyn ShowTarget>) {
    let Some(target) = target else {
        return;
    };
    target.show_page();
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

/// The host's recording, shared by the paint callback and the input pump.
///
/// **The file is written here, in the process that paints**, so the frames it
/// holds are the composed viewport itself rather than a re-encode of what a
/// socket carried.
struct Recording {
    running: AtomicBool,
    slot: crate::record::FrameSlot,
    held: Mutex<Option<crate::record::Recorder>>,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(false),
            slot: Arc::new(Mutex::new(None)),
            held: Mutex::new(None),
        })
    }

    /// The recording takes one painted viewport. **Only the recording's own
    /// size is kept**: a controller resize while the take runs is a frame the
    /// file does not need.
    fn offer(&self, view: &[u8], width: u32, height: u32) {
        if !self.running.load(Ordering::Acquire)
            || width != crate::record::WIDTH
            || height != crate::record::HEIGHT
        {
            return;
        }
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(view.to_vec()));
    }

    fn start(&self, path: &str) {
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
    // **Encoding and the socket write are a worker's, not the paint
    // callback's.** CEF paints on the thread that also drives the page, and a
    // server that stops reading would otherwise hold that thread in a frame
    // write for as long as the bound allows. The callback copies the newest
    // paint into one slot - replacing what was not taken - and the worker
    // encodes and writes it.
    let pending: Arc<(std::sync::Mutex<std::collections::VecDeque<Job>>, std::sync::Condvar)> =
        Arc::new((
            std::sync::Mutex::new(std::collections::VecDeque::new()),
            std::sync::Condvar::new(),
        ));
    let encoding_paused = Arc::new(AtomicBool::new(true));
    let key_asked = Arc::new(AtomicBool::new(false));
    {
        let (sink, pending, paused, key_asked) = (
            crate::frame::Sink::new(args.profile_id, args.frames.clone(), args.fps, args.quality),
            Arc::clone(&pending),
            Arc::clone(&encoding_paused),
            Arc::clone(&key_asked),
        );
        std::thread::spawn(move || {
            let mut sink = sink;
            loop {
                let job = {
                    let (queue, wake) = &*pending;
                    let mut guard = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    while guard.is_empty() {
                        guard = wake.wait(guard).unwrap_or_else(std::sync::PoisonError::into_inner);
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
                        // **The interval is spent waiting, never dropping.**
                        // The queue keeps one frame, replaced by each newer
                        // paint, so a burst's last state is what gets encoded.
                        // A paint dropped for landing too close to the last
                        // send was a state no later paint repaired - a typed
                        // character or a reload that never reached the viewer.
                        let wait = sink.interval_remaining();
                        if !wait.is_zero() {
                            std::thread::sleep(wait);
                        }
                        let newest = {
                            let (queue, _) = &*pending;
                            let mut held =
                                queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            take_for_encode(&mut held)
                        };
                        let job = encode_target(job, newest);
                        sink.offer(&job.pixels, job.width, job.height);
                    }
                    // **Audio never waits on the frame rate**: it is a clock
                    // of its own, and the paused state is the viewer's.
                    Job::Audio(audio) => sink.offer_audio(&audio),
                }
            }
        });
    }
    let recording = Recording::new();
    let paint: crate::osr::PaintSink = {
        let pending = Arc::clone(&pending);
        let paused = Arc::clone(&encoding_paused);
        let recording = Arc::clone(&recording);
        Arc::new(move |view, width, height, distinct| {
            // **A recording is not the viewers'.** A take runs whether or not
            // anyone is watching, so it is offered the viewport before the
            // encoding pause.
            recording.offer(view, width, height);
            if paused.load(Ordering::Acquire) {
                return;
            }
            let (queue, wake) = &*pending;
            let mut held = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // **A queued popup frame is not replaced by a plain repaint.**
            // The queue keeps the newest paint, and the view CEF paints after
            // a popup has the menu removed - so replacing the popup with it
            // before the encoder takes it loses the menu a second way.
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
        })
    };
    let audio: crate::audio::AudioSink = {
        let pending = Arc::clone(&pending);
        Arc::new(move |packet: crate::channel::Audio| {
            let (queue, wake) = &*pending;
            let mut held = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // **A backlog drops the newest packet.** Audio is a stream: one
            // that queues is heard late, and a cap turns a growing delay into
            // a short gap.
            if held.len() >= MAX_PENDING {
                return;
            }
            held.push_back(Job::Audio(packet));
            drop(held);
            wake.notify_one();
        })
    };
    let render = crate::osr::handler(Arc::clone(&ring), Some(Arc::clone(&paint)));
    let page_audio = crate::audio::handler(args.profile_id, audio);
    let downloads = crate::download::handler(&args.profile);
    let mut client = HostClient::new(Arc::clone(&ring), paint, render, page_audio, downloads);
    let window_info = WindowInfo {
        windowless_rendering_enabled: 1,
        // **The frame source is ours.** With CEF's own scheduling under an
        // external message pump the same page painted at ~26 fps; an
        // explicit 60 Hz cadence paints at 55-60 (the probe's measurement).
        external_begin_frame_enabled: 1,
        ..Default::default()
    };
    // **Opaque, unlike CEF's default.** A page with no background rule is
    // transparent, and the takeover's dark ground would show through it as a
    // blank surface; the app's own background is what the browser fills.
    let browser_settings = BrowserSettings { background_color: 0xFF04_050A, ..Default::default() };
    // **The host opens blank, and the driver navigates the carried page.**
    // The page a hand-off or strip prepared is written beside the profile;
    // opening it here would race the veto, because this document starts
    // before the installer exists. The driver navigates to it once the veto
    // is registered, and that navigation is the first document the page's
    // own script runs in.
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from("about:blank")),
        Some(&browser_settings),
        None,
        None,
    );
    let Some(browser) = browser else {
        ring.set_state(STATE_FAILED);
        return fail("the windowless browser was not created");
    };
    // **A browser with no host is not a live host**: the state and the log
    // below are what the server reads to say this profile is up, so a
    // browser that cannot be driven fails here rather than reporting live.
    let Some(host) = browser.host() else {
        ring.set_state(STATE_FAILED);
        return fail("the windowless browser has no host");
    };
    // **Pointer input arrives on stdin, one line per gesture.** The reader
    // only queues: CEF's mouse calls are made on the pump thread below, the
    // one that owns the browser, and they are the calls that route a click
    // into a windowless popup.
    let inputs: Arc<std::sync::Mutex<std::collections::VecDeque<crate::input::HostInput>>> =
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    {
        let inputs = Arc::clone(&inputs);
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let Some(input) = crate::input::parse_host_line(&line) else { continue };
                let mut queue = inputs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
    host.was_hidden(0);
    host.set_focus(1);
    host.set_windowless_frame_rate(60);
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
    // SAFETY: the header is mapped for this process's whole life.
    let mut last_request = unsafe { (*ring.header()).request_seq.load(Ordering::Acquire) };
    // SAFETY: as above.
    let mut last_key = unsafe { (*ring.header()).key_seq.load(Ordering::Acquire) };
    loop {
        if STOP.load(Ordering::Acquire) {
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
            if let Some(host) = browser.host() {
                // The pixel ratio first: it changes the DIP rect as well as
                // the buffer, and CEF lays out against both.
                host.notify_screen_info_changed();
                host.was_resized();
                // **A resize repaints even a still page**: a blank or static
                // document has no layout change to trigger one, and the
                // viewer would keep the old size until something else moved.
                host.invalidate(PaintElementType::VIEW);
            }
        }
        // **Every call the driver makes re-asserts the page visible.** A
        // windowless browser's only visibility lever is this call; a page
        // that went hidden while nothing drove it (the reported
        // `document.visibilityState` "hidden" on the first call of a fire)
        // must not stay hidden under a click that needs its frames. The
        // applied sequence is published so the caller can wait for it, and
        // the line names the cue a real fire can be read by.
        if let Some(show) = cues.take(header_ref.show_seq.load(Ordering::Acquire)) {
            let host = browser.host();
            apply_show(header_ref, show, host.as_ref().map(|host| host as &dyn ShowTarget));
        }
        // **Encoding follows the viewers.** The service pauses the ring
        // with nobody attached and resumes it on the first; a resume forces
        // a key frame, since what changed while nobody watched is not a
        // delta's to describe.
        let paused = header_ref.paused.load(Ordering::Acquire) != 0;
        encoding_paused.store(paused, Ordering::Release);
        // **A viewer attaching asks for a crisp first draw.** The service
        // increments the cue; the next encoded frame is full quality.
        let key = header_ref.key_seq.load(Ordering::Acquire);
        if key != last_key {
            last_key = key;
            key_asked.store(true, Ordering::Release);
            // **A page that has not changed does not repaint on its own**,
            // so the attach's key cue asks for one: a viewer joining a still
            // page draws it now rather than at the next navigation.
            if let Some(host) = browser.host() {
                host.invalidate(PaintElementType::VIEW);
            }
        }
        // **The pointer gestures go before the frame cue**, so a click is in
        // CEF before the frame it causes is asked for.
        apply_inputs(&browser, &inputs, &recording);
        // **8 ms, not 16.** A BeginFrame every screen refresh is the paint
        // rate, but a click landing just after one waited out the whole
        // interval; asking more often lets the next frame start sooner and
        // the compositor still paints at its own rate.
        let now = elapsed_ms();
        if now.saturating_sub(last_begin) >= 8 {
            last_begin = now;
            if let Some(host) = browser.host() {
                host.send_external_begin_frame();
            }
        }
        pump();
        std::thread::sleep(Duration::from_millis(1));
    }
    // **The profile's own writes go before the process does.** Closing the
    // browser lets CEF's store flush; the bounded pump is what carries the
    // close through the same message loop that painted it.
    if let Some(host) = browser.host() {
        host.close_browser(1);
    }
    for _ in 0..100 {
        pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    shutdown();
    ExitCode::SUCCESS
}

/// Hand the queued pointer gestures to CEF, in the order they arrived.
///
/// **These calls are the routing.** CEF's own `SendMouse*Event` is what
/// forwards an event into a windowless popup when the point is inside its
/// rect; the same event sent to the page target over DevTools never reaches
/// the popup view and closes a select's menu instead of choosing from it.
fn apply_inputs(
    browser: &Browser,
    inputs: &std::sync::Mutex<std::collections::VecDeque<crate::input::HostInput>>,
    recording: &Recording,
) {
    let Some(host) = browser.host() else { return };
    let mut queue = inputs.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    while let Some(input) = queue.pop_front() {
        match input {
            crate::input::HostInput::Mouse { action, x, y, button, clicks, modifiers } => {
                let event = MouseEvent { x, y, modifiers: crate::input::cef_modifiers(modifiers) };
                match action {
                    forge_primitives::browser::MouseAction::Move => {
                        host.send_mouse_move_event(Some(&event), 0);
                    }
                    forge_primitives::browser::MouseAction::Press => {
                        host.send_mouse_click_event(Some(&event), mouse_button(button), 0, clicks);
                    }
                    forge_primitives::browser::MouseAction::Release => {
                        host.send_mouse_click_event(Some(&event), mouse_button(button), 1, clicks);
                    }
                }
            }
            crate::input::HostInput::Wheel { x, y, delta_x, delta_y, modifiers } => {
                let event = MouseEvent { x, y, modifiers: crate::input::cef_modifiers(modifiers) };
                host.send_mouse_wheel_event(Some(&event), delta_x, delta_y);
            }
            crate::input::HostInput::File { id, paths } => {
                if !crate::dialog::answer(id, &paths) {
                    eprintln!(
                        "cef host: a file answer named no open dialog (event_name \
                         cef_file_answer_stray, id {id})"
                    );
                }
            }
            crate::input::HostInput::RecordStart { path } => recording.start(&path),
            crate::input::HostInput::RecordStop => recording.stop(),
        }
    }
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
    struct HostLifeSpanHandler {}

    impl LifeSpanHandler {
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            // The browser outlives every surface: a close request proceeds
            // only on the host's own terms.
            0
        }

        /// **One target, always.** A `target=_blank` (or `window.open`) would
        /// create a second page target on this host's endpoint: invisible,
        /// unclosable, and a coin flip for which one input binds to. The
        /// popup is refused, and a web address it asked for is loaded in this
        /// same browser instead - the page the person clicked for, in the
        /// surface they are looking at.
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
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
            if let Some(url) = popup_redirect(&url)
                && let Some(frame) = browser.and_then(|browser| browser.main_frame())
            {
                frame.load_url(Some(&CefString::from(url)));
            }
            1
        }
    }
}

/// The page the profile carries, when it names a web address.
///
/// **Beside the profile, like the server's own record**: the server writes
/// `page` on every raise, and a host launched after a restart opens it. A
/// missing, blank or non-web value opens `about:blank` - a carried
/// `chrome://` or `javascript:` value is not a page to resurrect.
pub fn carried_page(profile: &std::path::Path) -> String {
    crate::carry::carried(profile).unwrap_or_else(|| "about:blank".to_owned())
}

/// The address a popup asked for, when the same browser should load it.
///
/// **Anything but a web page is blocked outright**: `about:blank` and a
/// `javascript:` popup have no page of their own to show, and loading one
/// into the surface would replace the page the person is on.
fn popup_redirect(url: &str) -> Option<&str> {
    (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
}

cef::wrap_client! {
    struct HostClient {
        ring: Arc<Ring>,
        paint: crate::osr::PaintSink,
        // **One handler for every view.** CEF asks a client for its render
        // handler per view, and a popup is its own view: a handler built per
        // call kept the popup's bounds in one and painted into another, so
        // the menu was never composited.
        render: RenderHandler,
        // One audio handler too: its stream state is the page's, and a fresh
        // one per call would forget the sample rate between packets.
        audio: AudioHandler,
        // The page's downloads land beside the profile before they are the
        // controlling client's to save.
        downloads: DownloadHandler,
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
            Some(HostLifeSpanHandler::new())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::popup_redirect;

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

    /// **Every view is handed the same render handler.** CEF asks the client
    /// for one per view and a popup is its own view, so a handler built per
    /// call kept the popup's bounds in one instance and painted into another
    /// - the menu was drawn with no position and never composited.
    #[test]
    fn every_view_shares_one_render_handler() {
        use cef::ImplClient as _;
        use cef::rc::Rc as _;
        use std::sync::Arc;
        let name = format!("/forge-cef-test-shared-{}", std::process::id());
        let ring = Arc::new(crate::osr::Ring::create(&name, 4, 4).expect("a test ring"));
        let paint: crate::osr::PaintSink = Arc::new(|_, _, _, _| {});
        let audio: crate::audio::AudioSink = Arc::new(|_| {});
        let client = super::HostClient::new(
            Arc::clone(&ring),
            paint,
            crate::osr::handler(Arc::clone(&ring), None),
            crate::audio::handler(1, audio),
            crate::download::handler(std::path::Path::new("/tmp")),
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

    /// **A popup never becomes a second target, but a web page it asked for
    /// still opens.** The first is what keeps one page per host - and input
    /// bound to the page the person sees; the second is what a job site's
    /// "open in a new tab" link means.
    #[test]
    fn a_web_popup_loads_here_and_anything_else_is_blocked() {
        assert_eq!(popup_redirect("https://example.com/job"), Some("https://example.com/job"));
        assert_eq!(popup_redirect("http://example.com/"), Some("http://example.com/"));
        assert_eq!(popup_redirect("about:blank"), None);
        assert_eq!(popup_redirect("javascript:window.close()"), None);
        assert_eq!(popup_redirect(""), None);
    }

    /// **A restart reopens the page the raise showed, and only that.** The
    /// marker is a web address or nothing; a missing, blank or scheme-less
    /// value opens blank rather than resurrecting something a page wrote.
    #[test]
    fn a_carried_page_is_a_web_address_or_blank() {
        use super::carried_page;
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(carried_page(dir.path()), "about:blank", "no marker, no page");
        std::fs::write(dir.path().join("page"), b" https://example.com/job \n")
            .expect("the marker");
        assert_eq!(carried_page(dir.path()), "https://example.com/job");
        std::fs::write(dir.path().join("page"), b"javascript:alert(1)").expect("the marker");
        assert_eq!(carried_page(dir.path()), "about:blank", "not a page to resurrect");
        std::fs::write(dir.path().join("page"), b"").expect("the marker");
        assert_eq!(carried_page(dir.path()), "about:blank", "blank carries nothing");
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

        apply_show(header, 3, None);
        assert_eq!(
            header.show_applied.load(Ordering::Acquire),
            0,
            "a cue with no host is not acknowledged as applied"
        );
        assert!(!recorder.called.load(Ordering::Acquire), "no host, no visibility calls");

        apply_show(header, 3, Some(&recorder));
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
