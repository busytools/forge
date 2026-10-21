//! One profile's browser host: the process behind `--cef-host`.
//!
//! Arguments name the profile's cache root, the DevTools port, the frame ring
//! and the client that spawned it. The host initializes CEF, creates one
//! windowless browser, reports that it is live in the ring's header, and pumps
//! CEF's work and a 60 Hz external begin-frame cadence until it is signalled
//! or its client goes. The client owns the record on disk; this process writes
//! nothing beside the profile beyond what Chromium itself does.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cef::*;

use super::osr::{Header, MAX_HEIGHT, MAX_WIDTH, Ring, STATE_FAILED, STATE_LIVE};
use super::{HostApp, elapsed_ms, pump};

/// The launch arguments, as the client passes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub profile: PathBuf,
    pub port: u16,
    pub ring: String,
    pub width: u32,
    pub height: u32,
    /// The client that spawned this host, when one did: the process the loop
    /// watches, so a client that died without its exit path takes the host
    /// with it. `None` for a host a person or a test launched directly.
    pub parent: Option<u32>,
}

/// Parse `--cef-host --profile DIR --port N --ring NAME --size WxH
/// [--parent PID]`.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Option<Args> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("--cef-host") {
        return None;
    }
    let mut profile = None;
    let mut port = None;
    let mut ring = None;
    let mut parent = None;
    let mut size = (1100_u32, 620_u32);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => profile = args.next().map(PathBuf::from),
            "--port" => port = args.next().and_then(|value| value.parse::<u16>().ok()),
            "--ring" => ring = args.next(),
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
    Some(Args {
        profile: profile?,
        port: port?,
        ring: ring?,
        width: size.0,
        height: size.1,
        parent,
    })
}

/// How often the loop asks whether the client that spawned it is still there.
///
/// **A whole second, not every tick.** The watch is a backstop for a client
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

/// The client recorded at spawn, and when the loop last asked after it.
struct ParentWatch {
    parent: Option<u32>,
    last: u64,
}

impl ParentWatch {
    fn new(parent: Option<u32>) -> Self {
        Self { parent, last: 0 }
    }

    /// Whether the client is gone, asked at most once per interval.
    ///
    /// **The pid comes from the client's own spawn line, not `getppid`.** A
    /// `getppid` read at host start races the client's death: one that finds
    /// the host already reparented answers `1`, which `kill(1, 0)` reads as
    /// alive forever, and the watch never fires. The spawner's own pid names
    /// the process this host must outlive, whatever its parent becomes.
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

/// Set by SIGTERM/SIGINT, read by the pump loop.
static STOP: AtomicBool = AtomicBool::new(false);

/// Why the loop is stopping.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    Signal,
    ClientGone,
}

/// One decision for both shutdowns, so both take the same exit: the browser
/// closes and CEF's store flushes, rather than a process that simply stops.
fn stopping(stop: &AtomicBool, watch: &mut ParentWatch, now: u64) -> Option<Stop> {
    if stop.load(Ordering::Acquire) {
        return Some(Stop::Signal);
    }
    watch.gone(now).then_some(Stop::ClientGone)
}

/// The loop's whole wiring: ask whether to stop, otherwise do one pass of the
/// host's work. A host passes the CEF pump and the real clock; a test passes a
/// no-op and its own clock, so the stop decision and the exit it takes are the
/// loop's own rather than a copy of them.
fn run_loop(
    args: &Args,
    stop: &AtomicBool,
    now: &mut impl FnMut() -> u64,
    pass: &mut impl FnMut(),
) -> Stop {
    let mut parent = ParentWatch::new(args.parent);
    loop {
        if let Some(why) = stopping(stop, &mut parent, now()) {
            return why;
        }
        pass();
    }
}

/// The host's life after the browser is up: the loop, then the shutdown both
/// of its reasons take.
///
/// **One exit, so neither reason can leave the browser unclosed** - a killed
/// client is as much a shutdown as a signal. The pass, the clock and the
/// shutdown are seams, so a test runs the composition without CEF.
fn host_until_shutdown(
    args: &Args,
    stop: &AtomicBool,
    now: &mut impl FnMut() -> u64,
    pass: &mut impl FnMut(),
    host: Option<&BrowserHost>,
    mut pump: impl FnMut(),
    shutdown: impl FnOnce(),
) {
    let why = run_loop(args, stop, now, pass);
    // **A client's death is a shutdown, like a signal.** The client reaps its
    // hosts on its exit, but a killed client runs no exit path - and a host
    // left behind holds the bundle the next install replaces. The line is all
    // that differs.
    if let Stop::ClientGone = why {
        eprintln!(
            "cef host: the client that spawned this host is gone (event_name \
             cef_host_parent_gone, parent {:?}, pid {})",
            args.parent,
            std::process::id(),
        );
    }
    // **The profile's own writes go before the process does.** Closing the
    // browser lets CEF's store flush; the bounded pump is what carries the
    // close through the same message loop that painted it.
    if let Some(host) = host {
        host.close_browser(1);
    }
    for _ in 0..100 {
        pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    shutdown();
}

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
/// a client's cue written while the loop was still starting is acted on by
/// the first pass rather than mistaken for the state it started in.
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

/// **The test build's hidden page.** Set from a marker file in the profile,
/// so the visibility proof can start from a page that really is hidden:
/// `was_hidden(1)` is otherwise never called and no test can produce the
/// state #2455 reports. Compiled out of every shipping build.
#[cfg(feature = "testing")]
static TEST_HIDE: AtomicBool = AtomicBool::new(false);

extern "C" fn stop_on_signal(_signal: i32) {
    STOP.store(true, Ordering::Release);
}

/// **A kill is a shutdown, not a stop.** The client ends a host with a
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

/// **The host's first act, before CEF initializes.** The application check
/// (`tests/app_ivars.rs`) runs this same function, so a setup removed here
/// fails that check rather than only being missed at launch.
pub fn begin() {
    super::client_application::setup();
}

/// The host process's whole life.
pub fn run(args: Args) -> ! {
    begin();
    stop_on_termination();

    // **A background app is napped.** A host with no window of its own reads
    // as an idle app to macOS; the activity keeps its pump and frames out of
    // App Nap's coalescing.
    keep_awake();

    if let Err(why) = std::fs::create_dir_all(&args.profile) {
        fail(&why.to_string());
    }
    // The test build's marker, read once: a page hidden from the start, for
    // the visibility proof only.
    #[cfg(feature = "testing")]
    TEST_HIDE.store(args.profile.join("test-hide").exists(), Ordering::Relaxed);
    let ring = match Ring::create(&args.ring, args.width, args.height) {
        Ok(ring) => Arc::new(ring),
        Err(why) => fail(&why),
    };

    let mut app = HostApp::new();
    let cef_args = args::Args::new();
    if !super::initialize(&args.profile, args.port, &mut app, &cef_args) {
        ring.set_state(STATE_FAILED);
        std::process::exit(3);
    }

    let mut client = HostClient::new(Arc::clone(&ring));
    let window_info = WindowInfo {
        windowless_rendering_enabled: 1,
        // **The frame source is ours.** With CEF's own scheduling under an
        // external message pump the same page painted at ~26 fps; an
        // explicit 60 Hz cadence paints at 55-60 (the probe's measurement).
        external_begin_frame_enabled: 1,
        ..Default::default()
    };
    // **Opaque, unlike CEF's default.** A page with no background rule is
    // transparent, and the takeover's dark ground shows through it as a
    // blank surface; the app's own background is what the browser fills.
    let browser_settings = BrowserSettings { background_color: 0xFF04_050A, ..Default::default() };
    // **The host opens blank, and the client navigates the carried page.**
    // The page a hand-off or strip prepared is written beside the profile;
    // opening it here would race the veto, because this document starts
    // before the installer exists. The client navigates to it once the veto
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
        fail("the windowless browser was not created");
    };
    // **A browser with no host is not a live host**: the state and the log
    // below are what the client reads to say this profile is up, so a
    // browser that cannot be driven fails here rather than reporting live.
    let Some(host) = browser.host() else {
        ring.set_state(STATE_FAILED);
        fail("the windowless browser has no host");
    };
    host.was_hidden(0);
    host.set_focus(1);
    host.set_windowless_frame_rate(60);
    ring.set_state(STATE_LIVE);
    ring.set_visible(true, true);
    // **stderr, which the client redirects into this profile's
    // `cef-host.log`.** The tracing logger is initialized by the client's
    // Tauri builder, and the host exits before it; a log call here goes
    // nowhere, so the host speaks on the stream the launcher captures.
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
    let mut clock = elapsed_ms;
    let mut pass = || {
        let header = ring.header();
        // SAFETY: the header is mapped for this process's whole life.
        let header_ref = unsafe { &*header };
        // The client asked for another view size: adopt it and tell CEF.
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
            }
        }
        // **Every call the client drives with re-asserts the page visible.**
        // A windowless browser's only visibility lever is this call; a page
        // that went hidden while nothing drove it (the reported
        // `document.visibilityState` "hidden" on the first call of a fire)
        // must not stay hidden under a click that needs its frames. The
        // applied sequence is published so the caller can wait for it, and
        // the line names the cue a real fire can be read by.
        if let Some(show) = cues.take(header_ref.show_seq.load(Ordering::Acquire)) {
            let host = browser.host();
            apply_show(header_ref, show, host.as_ref().map(|host| host as &dyn ShowTarget));
        }
        // **8 ms, not 16.** A BeginFrame every screen refresh is the paint
        // rate, but a click landing just after one waited out the whole
        // interval; asking more often lets the next frame start sooner and
        // the compositor still paints at its own rate. The clock is sampled
        // here, after the adopt and cue work, as it was before the loop took
        // its pass as a seam.
        let now = elapsed_ms();
        if now.saturating_sub(last_begin) >= 8 {
            last_begin = now;
            if let Some(host) = browser.host() {
                host.send_external_begin_frame();
            }
        }
        pump();
        std::thread::sleep(Duration::from_millis(1));
    };
    let closing = browser.host();
    host_until_shutdown(&args, &STOP, &mut clock, &mut pass, closing.as_ref(), pump, shutdown);
    std::process::exit(0);
}

fn fail(why: &str) -> ! {
    eprintln!("forge host: {why}");
    std::process::exit(2);
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
            // **The test build hides the page here, after the re-assert.**
            // The page records that it saw the hidden state, so one later
            // call can prove the cue flipped it back to visible.
            #[cfg(feature = "testing")]
            if TEST_HIDE.load(Ordering::Relaxed) {
                let code = CefString::from(
                    "window.__sawHidden = document.visibilityState === 'hidden'; \
                     document.addEventListener('visibilitychange', function () { \
                       if (document.visibilityState === 'hidden') window.__sawHidden = true; \
                     });",
                );
                if let Some(frame) = browser.main_frame() {
                    frame.execute_java_script(Some(&code), None, 0);
                }
                host.was_hidden(1);
                eprintln!("cef host: test hook hid the page (event_name cef_test_hidden)");
            }
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
/// **Beside the profile, like the client's own record**: the client writes
/// `page` on every raise, and a host launched after a restart opens it. A
/// missing, blank or non-web value opens `about:blank` - a carried
/// `chrome://` or `javascript:` value is not a page to resurrect.
pub fn carried_page(profile: &std::path::Path) -> String {
    let Ok(text) = std::fs::read_to_string(profile.join("page")) else {
        return "about:blank".to_owned();
    };
    let url = text.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_owned()
    } else {
        "about:blank".to_owned()
    }
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
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(super::osr::handler(Arc::clone(&self.ring)))
        }

        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(super::dialog::handler())
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
        use super::super::osr::Ring;
        use super::{Header, ShowTarget, apply_show};
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
    /// applied marker starts at zero, not at the header's value, or a client
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

    /// **The spawn line names the client.** A host launched by a person or a
    /// test carries no `--parent` and is not watched.
    #[test]
    fn the_client_pid_is_an_optional_argument() {
        use super::parse;

        let args = |extra: &[&str]| {
            let mut all: Vec<String> =
                ["--cef-host", "--profile", "/p", "--port", "9411", "--ring", "/r"]
                    .iter()
                    .map(|arg| (*arg).to_owned())
                    .collect();
            all.extend(extra.iter().map(|arg| (*arg).to_owned()));
            parse(all)
        };
        assert_eq!(args(&["--parent", "4242"]).expect("parsed").parent, Some(4242));
        assert_eq!(args(&[]).expect("parsed").parent, None, "no spawn line, no watch");

        // **A pid `kill` cannot name is refused, not run unwatched.** Zero
        // probes this host's own process group and a value past i32::MAX
        // casts negative, so either would read as alive forever.
        for refused in ["0", "-3", "garbage", "2147483648", "4294967296"] {
            assert!(
                args(&["--parent", refused]).is_none(),
                "a pid out of the signal call's range is refused: {refused}"
            );
        }
        assert!(args(&["--parent"]).is_none(), "a parent flag with no value is refused");
    }

    /// A pid that named a process which has exited and been reaped.
    fn a_gone_pid() -> u32 {
        let mut child =
            std::process::Command::new("/bin/sh").args(["-c", "exit 0"]).spawn().expect("a child");
        let pid = child.id();
        child.wait().expect("the child is reaped");
        pid
    }

    /// **The watch is the client's pid, asked once a second.** The pid comes
    /// from the spawn line, not `getppid`: a `getppid` read at host start can
    /// find the host already reparented and answer `1`, which reads as alive
    /// forever.
    #[test]
    fn the_client_watch_reads_the_spawned_pid_once_a_second() {
        use super::{PARENT_CHECK_EVERY_MS, ParentWatch, running};

        assert_eq!(PARENT_CHECK_EVERY_MS, 1_000, "the bound the doc claims, not a boundary");

        let mut unwatched = ParentWatch::new(None);
        assert!(!unwatched.gone(u64::MAX), "a host nobody spawned is not watched");

        let mut alive = ParentWatch::new(Some(std::process::id()));
        assert!(!alive.gone(1_000), "this test's own process is alive");
        assert!(!alive.gone(2_000), "and still alive a second later");

        let pid = a_gone_pid();
        assert!(!running(pid), "a reaped child is gone");

        let mut dead = ParentWatch::new(Some(pid));
        assert!(!dead.gone(999), "nothing is asked before the interval");
        assert!(dead.gone(1_000), "the interval is when the client is asked after");
    }

    /// **Both reasons take one exit.** A signal and a gone client land in the
    /// same decision, so the loop cannot shut down one way for a signal and
    /// another for the client.
    #[test]
    fn the_loop_stops_for_a_signal_or_a_gone_client() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::{PARENT_CHECK_EVERY_MS, ParentWatch, Stop, stopping};

        let stop = AtomicBool::new(false);
        let mut unwatched = ParentWatch::new(None);
        assert_eq!(
            stopping(&stop, &mut unwatched, PARENT_CHECK_EVERY_MS),
            None,
            "nothing stops it"
        );

        stop.store(true, Ordering::Release);
        let signalled = stopping(&stop, &mut unwatched, PARENT_CHECK_EVERY_MS);
        assert_eq!(signalled, Some(Stop::Signal), "a signal stops it");
        stop.store(false, Ordering::Release);

        let mut gone = ParentWatch::new(Some(a_gone_pid()));
        assert_eq!(
            stopping(&stop, &mut gone, PARENT_CHECK_EVERY_MS),
            Some(Stop::ClientGone),
            "a gone client stops it"
        );
    }

    /// **A gone client ends the loop, and no pass runs after it.** The clock
    /// and the pass are seams, so the loop's own wiring decides rather than a
    /// copy of it made by the test.
    #[test]
    fn the_loop_answers_a_gone_client_and_runs_no_pass_after_it() {
        use std::sync::atomic::AtomicBool;

        use super::{Args, PARENT_CHECK_EVERY_MS, Stop, run_loop};

        let args = Args {
            profile: std::path::PathBuf::from("/nonexistent"),
            port: 0,
            ring: "/forge-cef-loop-gone".to_owned(),
            width: 1,
            height: 1,
            parent: Some(a_gone_pid()),
        };
        let stop = AtomicBool::new(false);
        let mut at = 0_u64;
        let mut passes = 0_u32;
        let why = run_loop(
            &args,
            &stop,
            &mut || {
                at += PARENT_CHECK_EVERY_MS;
                at
            },
            &mut || {
                passes += 1;
                assert!(passes < 2, "the loop ran on past a gone client");
            },
        );
        assert_eq!(why, Stop::ClientGone, "a gone client is why the loop stops");
    }

    /// **A signal ends the loop after the passes it was told to run.** Each
    /// pass is one round of the host's work, so the loop stops after three -
    /// never before the work it was given, and never after the stop.
    #[test]
    fn the_loop_runs_its_passes_until_a_signal() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::{Args, Stop, run_loop};

        let args = Args {
            profile: std::path::PathBuf::from("/nonexistent"),
            port: 0,
            ring: "/forge-cef-loop-signal".to_owned(),
            width: 1,
            height: 1,
            parent: Some(std::process::id()),
        };
        let stop = AtomicBool::new(false);
        let mut at = 0_u64;
        let mut passes = 0_u32;
        let why = run_loop(
            &args,
            &stop,
            &mut || {
                at += 100;
                at
            },
            &mut || {
                passes += 1;
                assert!(passes < 5, "the loop ran past its signal");
                if passes == 3 {
                    stop.store(true, Ordering::Release);
                }
            },
        );
        assert_eq!(passes, 3, "the work ran until the signal, and not after");
        assert_eq!(why, Stop::Signal, "the signal is why the loop stops");
    }

    /// **A gone client's shutdown is the signal's own.** The reason only picks
    /// a line; the loop's exit always closes the browser, carries the close
    /// through the pump, and shuts CEF down.
    #[test]
    fn a_gone_client_leaves_through_the_shutdown() {
        use std::sync::atomic::AtomicBool;

        use super::{Args, PARENT_CHECK_EVERY_MS, host_until_shutdown};

        let args = Args {
            profile: std::path::PathBuf::from("/nonexistent"),
            port: 0,
            ring: "/forge-cef-leave-gone".to_owned(),
            width: 1,
            height: 1,
            parent: Some(a_gone_pid()),
        };
        let stop = AtomicBool::new(false);
        let mut at = 0_u64;
        let mut passes = 0_u32;
        let mut pumped = 0_u32;
        let mut shut = false;
        host_until_shutdown(
            &args,
            &stop,
            &mut || {
                at += PARENT_CHECK_EVERY_MS;
                at
            },
            &mut || {
                passes += 1;
                assert!(passes < 2, "the loop ran on past a gone client");
            },
            None,
            || pumped += 1,
            || shut = true,
        );
        assert!(pumped > 0, "the close was carried through the pump");
        assert!(shut, "a gone client still takes the shutdown");
    }
}
