//! One profile's browser host: the process behind `--cef-host`.
//!
//! Arguments name the profile's cache root, the DevTools port and the frame
//! ring. The host initializes CEF, creates one windowless browser, reports
//! that it is live in the ring's header, and pumps CEF's work and a 60 Hz
//! external begin-frame cadence until it is killed. The client owns the
//! record on disk; this process writes nothing beside the profile beyond
//! what Chromium itself does.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use cef::*;

use super::osr::{MAX_HEIGHT, MAX_WIDTH, Ring, STATE_FAILED, STATE_LIVE};
use super::{HostApp, elapsed_ms, pump};

/// The launch arguments, as the client passes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    pub profile: PathBuf,
    pub port: u16,
    pub ring: String,
    pub width: u32,
    pub height: u32,
}

/// Parse `--cef-host --profile DIR --port N --ring NAME --size WxH`.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Option<Args> {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("--cef-host") {
        return None;
    }
    let mut profile = None;
    let mut port = None;
    let mut ring = None;
    let mut size = (1100_u32, 620_u32);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => profile = args.next().map(PathBuf::from),
            "--port" => port = args.next().and_then(|value| value.parse::<u16>().ok()),
            "--ring" => ring = args.next(),
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
    Some(Args { profile: profile?, port: port?, ring: ring?, width: size.0, height: size.1 })
}

/// The host process's whole life.
pub fn run(args: Args) -> ! {
    // AppKit's application object, before CEF initializes.
    super::client_application::setup();

    // **A background app is napped.** A host with no window of its own reads
    // as an idle app to macOS; the activity keeps its pump and frames out of
    // App Nap's coalescing.
    keep_awake();

    if let Err(why) = std::fs::create_dir_all(&args.profile) {
        fail(&why.to_string());
    }
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
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from("about:blank")),
        Some(&BrowserSettings::default()),
        None,
        None,
    );
    let Some(browser) = browser else {
        ring.set_state(STATE_FAILED);
        fail("the windowless browser was not created");
    };
    if let Some(host) = browser.host() {
        host.was_hidden(0);
        host.set_focus(1);
        host.set_windowless_frame_rate(60);
    }
    ring.set_state(STATE_LIVE);
    ring.set_visible(true, true);
    tauri_plugin_log::log::info!(
        "a CEF host is live (event_name cef_host_live, pid {}, profile {}, port {}, ring {})",
        std::process::id(),
        args.profile.display(),
        args.port,
        args.ring,
    );

    let mut last_begin = 0_u64;
    // SAFETY: the header is mapped for this process's whole life.
    let mut last_request = unsafe { (*ring.header()).request_seq.load(Ordering::Acquire) };
    loop {
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
            let Some(host) = browser.and_then(|browser| browser.host()) else { return };
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
    }
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
