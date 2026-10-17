//! The embed-capture spike (option B), scratch: ScreenCaptureKit on the
//! client's own window - the system content picker, and the stream that
//! draws the offscreen browser into the client over it.
//!
//! Never merges; the plan and its findings live beside the survey report.
//! The picker lives here rather than in the spike's CLI harness because the
//! picker's UI only presents for an app the person is actually using: a
//! session-launched CLI asked and no UI ever appeared.
//!
//! Everything SCK touches runs on the main thread: the picker, the stream,
//! the overlay view - so the picked filter and the live stream are held in
//! thread-locals and no `Send` is owed anywhere.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadOnly};
use objc2_app_kit::{NSEvent, NSView, NSWindowCollectionBehavior};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::CVImageBuffer;
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerConfiguration,
    SCContentSharingPickerMode, SCContentSharingPickerObserver, SCShareableContent,
    SCShareableContentStyle, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
};
use serde_json::json;

// The frame's own IOSurface, straight off the pixel buffer - what a CALayer
// draws with no copy.
#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetIOSurface(pixel_buffer: *mut c_void) -> *mut c_void;
}

// The live overlay view and the live stream, kept on the main thread only.
// The CDP sender is the input side: one connection to the captured browser's
// page endpoint, held by a worker task.
thread_local! {
    static OVERLAY: RefCell<Option<Retained<OverlayView>>> = const { RefCell::new(None) };
    static STREAM: RefCell<Option<Retained<SCStream>>> = const { RefCell::new(None) };
}

/// The picked filter, kept as a retained raw pointer behind a lock.
/// **The picker's callback arrives on a framework thread, not necessarily
/// main** - the first pick landed there and a main-thread-only store then
/// read back empty ("no window picked yet", measured 2026-10-17 18:21:54) -
/// so the store is thread-safe and the filter is only ever USED on the main
/// thread, where every SCK call lives.
static PICKED: Mutex<Option<usize>> = Mutex::new(None);

fn store_picked(filter: &SCContentFilter) {
    let pointer: *const SCContentFilter = filter;
    let retained = unsafe { Retained::retain(pointer.cast_mut()) };
    if let Some(retained) = retained {
        let raw = Retained::into_raw(retained) as usize;
        if let Ok(mut held) = PICKED.lock() {
            *held = Some(raw);
        }
    }
}

/// The picked filter, retained again for the caller - the store keeps its
/// own reference, so a re-show after a hide still finds it.
fn picked_filter() -> Option<Retained<SCContentFilter>> {
    let raw = PICKED.lock().ok().and_then(|held| *held)?;
    unsafe { Retained::retain(raw as *mut SCContentFilter) }
}

/// The one scratch file the input side reads: which browser page to drive,
/// and the geometry that maps an overlay point onto it. Written by the
/// spike's own tooling; nothing else reads it.
const TARGET: &str = "/tmp/embed-spike/state/cdp.json";

/// Where an overlay point lands on the captured page: the overlay's own
/// bounds are scaled onto the whole window image (chrome included; the
/// offset measured into `chrome`), and CDP speaks the captured page's
/// viewport in CSS pixels.
struct Mapping {
    image_pt: (f64, f64),
    chrome: f64,
    port: u16,
    page_path: String,
}

static INPUT: Mutex<Option<tokio::sync::mpsc::UnboundedSender<(String, serde_json::Value)>>> =
    Mutex::new(None);

/// The latency probe's switch: while on, every input send and every frame
/// arrival is logged with the wall clock, so click-to-visible can be read
/// off the log afterwards. Off by default - a stream logs one line per 2 s.
static PER_FRAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn wall_micros() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_micros())
        .unwrap_or(0)
}

/// Turn the per-frame latency logging on or off.
#[tauri::command]
pub fn capture_perf(enabled: bool) {
    PER_FRAME.store(enabled, std::sync::atomic::Ordering::Relaxed);
    tauri_plugin_log::log::info!(
        "embed-spike perf probe: {enabled} (event_name embed_spike_perf)"
    );
}

/// One connection to the captured page, held by a worker task: an event
/// sends a `(method, params)` and returns, so a click costs a channel hop,
/// not a websocket handshake.
fn ensure_input_worker(port: u16, page_path: &str) {
    if let Ok(mut held) = INPUT.lock() {
        if held.is_some() {
            return;
        }
        let (sender, mut receiver) =
            tokio::sync::mpsc::unbounded_channel::<(String, serde_json::Value)>();
        let path = page_path.to_owned();
        // **tauri's runtime, not `tokio::spawn`**: this runs on the MAIN
        // thread inside `with_webview`, where no tokio reactor exists - a
        // bare `tokio::spawn` panicked here ("there is no reactor running"),
        // and the panic unwound the event loop and took the app down with
        // it (measured 2026-10-17 18:26:42 and 18:32:04).
        tauri::async_runtime::spawn(async move {
            let mut connection: Option<super::window::Cdp> = None;
            while let Some((method, params)) = receiver.recv().await {
                if connection.is_none() {
                    match super::window::Cdp::connect(port, &path).await {
                        Ok(connected) => connection = Some(connected),
                        Err(why) => {
                            tauri_plugin_log::log::warn!(
                                "embed-spike input: the page endpoint refused a connection \
                                 (event_name embed_spike_input_connect, port {port}): {why}"
                            );
                            continue;
                        }
                    }
                }
                if let Some(open) = connection.as_mut() {
                    if PER_FRAME.load(std::sync::atomic::Ordering::Relaxed) {
                        tauri_plugin_log::log::info!(
                            "embed_spike_input method={method} t={} (event_name \
                             embed_spike_input)",
                            wall_micros(),
                        );
                    }
                    if let Err(why) = open.call(&method, params, None).await {
                        tauri_plugin_log::log::warn!(
                            "embed-spike input: {method} failed (event_name \
                             embed_spike_input_call): {why}"
                        );
                        connection = None;
                    }
                }
            }
        });
        *held = Some(sender);
    }
}

fn send_input(method: &str, params: serde_json::Value) {
    if let Ok(held) = INPUT.lock() {
        if let Some(sender) = held.as_ref() {
            let _ = sender.send((method.to_owned(), params));
        }
    }
}

/// What the picked filter named, as the command answers.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PickedWindow {
    window_id: u32,
    title: String,
    app: String,
    frame: (f64, f64, f64, f64),
    scale: f64,
}

enum Outcome {
    Picked(PickedWindow),
    Cancelled,
    Failed(String),
}

struct ObserverIvars {
    deliver: Mutex<Option<Box<dyn FnOnce(Outcome) + Send>>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = ObserverIvars]
    struct PickerObserver;

    unsafe impl NSObjectProtocol for PickerObserver {}

    unsafe impl SCContentSharingPickerObserver for PickerObserver {
        #[unsafe(method(contentSharingPicker:didUpdateWithFilter:forStream:))]
        fn did_update(
            &self,
            _picker: &SCContentSharingPicker,
            filter: &SCContentFilter,
            _stream: Option<&SCStream>,
        ) {
            store_picked(filter);
            let described = describe(filter);
            let deliver = self.ivars().deliver.lock().ok().and_then(|mut held| held.take());
            if let Some(deliver) = deliver {
                deliver(Outcome::Picked(described));
            }
        }

        #[unsafe(method(contentSharingPicker:didCancelForStream:))]
        fn did_cancel(&self, _picker: &SCContentSharingPicker, _stream: Option<&SCStream>) {
            let deliver = self.ivars().deliver.lock().ok().and_then(|mut held| held.take());
            if let Some(deliver) = deliver {
                deliver(Outcome::Cancelled);
            }
        }

        #[unsafe(method(contentSharingPickerStartDidFailWithError:))]
        fn did_fail(&self, error: &NSError) {
            let said = error.localizedDescription().to_string();
            let deliver = self.ivars().deliver.lock().ok().and_then(|mut held| held.take());
            if let Some(deliver) = deliver {
                deliver(Outcome::Failed(said));
            }
        }
    }
);

impl PickerObserver {
    fn new(deliver: Box<dyn FnOnce(Outcome) + Send>) -> Retained<Self> {
        let observer = Self::alloc().set_ivars(ObserverIvars { deliver: Mutex::new(Some(deliver)) });
        unsafe { msg_send![super(observer), init] }
    }
}

/// What the filter named, from its first included window.
fn describe(filter: &SCContentFilter) -> PickedWindow {
    let rect = unsafe { filter.contentRect() };
    let scale = f64::from(unsafe { filter.pointPixelScale() });
    let windows = unsafe { filter.includedWindows() };
    let first = windows.firstObject();
    let (window_id, title, app) = match &first {
        Some(window) => {
            let id = u32::try_from(unsafe { window.windowID() }).unwrap_or(0);
            let title = unsafe { window.title() }.map(|t| t.to_string()).unwrap_or_default();
            let app = unsafe { window.owningApplication() }
                .map(|a| unsafe { a.applicationName() }.to_string())
                .unwrap_or_default();
            (id, title, app)
        }
        None => (0, String::new(), String::new()),
    };
    PickedWindow {
        window_id,
        title,
        app,
        frame: (rect.origin.x, rect.origin.y, rect.size.width, rect.size.height),
        scale,
    }
}

/// Present the system content picker and answer what was picked.
///
/// **Called from the page, by a person's click**: the picker's UI only
/// presents for an app the person is actually using, which is the whole
/// reason this lives in the client rather than in the spike's CLI harness.
/// The picker, asked for - the one call both the command and the launch
/// auto-present share.
fn present_picker(deliver: Box<dyn FnOnce(Outcome) + Send>) {
    let observer = PickerObserver::new(deliver);
    unsafe {
        let picker = SCContentSharingPicker::sharedPicker();
        let configuration = SCContentSharingPickerConfiguration::new();
        configuration.setAllowedPickerModes(SCContentSharingPickerMode::SingleWindow);
        picker.setDefaultConfiguration(&configuration);
        picker.addObserver(ProtocolObject::from_ref(&*observer));
        // The header's own words: a picker needs to be marked active for
        // its UI to appear. **And it is presented WITH the content style** -
        // the platform's own sample calls `present(for: nil, using:
        // .window)`; a bare `present()` was asked for on 2026-10-17
        // (client 16:31:54, CLI 15:50:37) and no UI ever came back.
        picker.setActive(true);
        // Insurance, in the header's own terms: the picker only shows a
        // streamless UI while maximumStreamCount allows one.
        picker.setMaximumStreamCount(Some(&objc2_foundation::NSNumber::new_isize(1)));
        picker.presentPickerUsingContentStyle(SCShareableContentStyle::Window);
    }
    // The picker's observer list is the picker's business; forgetting the
    // unique pointer here keeps the observer alive for the whole pick
    // whichever policy it keeps (scratch code - a leak of one object per
    // pick is nothing).
    std::mem::forget(observer);
}

/// Scratch self-test, no person needed: builds the overlay view, the frame
/// sink class and the stream configuration on the live webview 3 s after
/// launch - everything `start_capture` does except the filter and the stream
/// itself - and logs how far it got. A crash that follows a pick is bisected
/// by this.
pub fn selftest(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let Some(window) = tauri::Manager::get_webview_window(&app, "main") else {
            return;
        };
        // The all-spaces behavior set at setup does NOT stick (the event
        // loop is not running yet there) - set it again here, three seconds
        // in, so the window follows the person to whatever space he is on.
        join_every_space(&window);
        let (sender, receiver) = tokio::sync::oneshot::channel::<Result<(), String>>();
        if window
            .with_webview(move |pv| {
                let said = test_overlay(&pv);
                let _ = sender.send(said);
            })
            .is_ok()
        {
            match receiver.await {
                Ok(Ok(())) => tauri_plugin_log::log::info!(
                    "embed-spike selftest: overlay half survived (event_name embed_spike_selftest_ok)"
                ),
                Ok(Err(why)) => tauri_plugin_log::log::warn!(
                    "embed-spike selftest: overlay half refused cleanly (event_name embed_spike_selftest_err): {why}"
                ),
                Err(_) => tauri_plugin_log::log::error!(
                    "embed-spike selftest: no answer (event_name embed_spike_selftest_silent)"
                ),
            }
        }
    });
}

fn test_overlay(pv: &tauri::webview::PlatformWebview) -> Result<(), String> {
    let wk_view: &NSView = unsafe { &*pv.inner().cast::<NSView>() };
    let superview = unsafe { wk_view.superview() }.ok_or("no superview")?;
    let wk_bounds = wk_view.bounds();
    let rect_in_wk = NSRect::new(
        NSPoint::new(0.0, wk_bounds.size.height - 300.0),
        NSSize::new(400.0, 300.0),
    );
    let rect = superview.convertRect_fromView(rect_in_wk, Some(wk_view));
    let mtm = objc2::MainThreadMarker::new().ok_or("not on the main thread")?;
    let mapping = Mapping {
        image_pt: (1200.0, 988.0),
        chrome: 81.0,
        port: 1,
        page_path: String::new(),
    };
    let overlay = OverlayView::new(rect, mapping, mtm);
    overlay.setWantsLayer(true);
    superview.addSubview(&overlay);
    if let Some(layer) = overlay.layer() {
        layer.setMasksToBounds(true);
        layer.setContentsScale(2.0);
    }
    let _sink = FrameSink::new(overlay.clone());
    let configuration = unsafe { SCStreamConfiguration::new() };
    unsafe {
        configuration.setWidth(2400);
        configuration.setHeight(1976);
        configuration.setMinimumFrameInterval(CMTime::new(1, 60));
        configuration.setQueueDepth(5);
        configuration.setShowsCursor(false);
        configuration.setIgnoreShadowsSingleWindow(true);
    }
    overlay.removeFromSuperview();
    Ok(())
}

/// Scratch: present the picker shortly after launch, with no click. A
/// session-launched context never draws the picker, and a posted click does
/// not reach this app's page, so on a launchedByLS=1 instance this is the
/// one machine-side way to ask for it.
pub fn autopresent(app: tauri::AppHandle) {
    // **Only when the spike's own tooling asks for it** (a marker file): the
    // picker must never fire on a launch the person has not been told about,
    // and the staged attempt fires it by hand from the page's button.
    if !std::path::Path::new("/tmp/embed-spike/state/autopresent").exists() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let for_main = app.clone();
        let _ = app.run_on_main_thread(move || {
            let handle = for_main.clone();
            tauri_plugin_log::log::info!("embed-spike autopresent: asking (event_name embed_spike_autopresent)");
            let deliver = Box::new(move |outcome: Outcome| match outcome {
                Outcome::Picked(picked) => {
                    tauri_plugin_log::log::info!(
                        "embed-spike autopresent: picked {} (event_name embed_spike_autopresent_picked)",
                        serde_json::to_string(&picked).unwrap_or_default(),
                    );
                    if let Some(window) =
                        tauri::Manager::get_webview_window(&handle, "main")
                    {
                        tauri::async_runtime::spawn(async move {
                            match show_after_target(&window).await {
                                Ok(()) => tauri_plugin_log::log::info!(
                                    "embed-spike autopresent: capture started (event_name embed_spike_autopresent_shown)"
                                ),
                                Err(why) => tauri_plugin_log::log::warn!(
                                    "embed-spike autopresent: capture failed (event_name embed_spike_autopresent_showfailed): {why}"
                                ),
                            }
                        });
                    }
                }
                Outcome::Cancelled => tauri_plugin_log::log::info!(
                    "embed-spike autopresent: cancelled (event_name embed_spike_autopresent_cancelled)"
                ),
                Outcome::Failed(said) => tauri_plugin_log::log::warn!(
                    "embed-spike autopresent: failed (event_name embed_spike_autopresent_failed): {said}"
                ),
            });
            present_picker(deliver);
        });
    });
}

#[tauri::command]
pub async fn capture_pick(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
) -> Result<PickedWindow, String> {
    let (sender, receiver) = tokio::sync::oneshot::channel::<Outcome>();
    app.run_on_main_thread(move || {
        let deliver = Box::new(move |outcome| {
            let _ = sender.send(outcome);
        });
        present_picker(deliver);
    })
    .map_err(|why| format!("the main thread refused the picker: {why}"))?;

    match receiver.await {
        Ok(Outcome::Picked(picked)) => {
            show_after_target(&window)
                .await
                .map_err(|why| format!("the window was picked, but the capture did not start: {why}"))?;
            Ok(picked)
        }
        Ok(Outcome::Cancelled) => Err("the picker was cancelled".to_owned()),
        Ok(Outcome::Failed(said)) => Err(format!("the picker failed to start: {said}")),
        Err(_) => Err("the picker observer never answered".to_owned()),
    }
}

/// The overlay's frame in its superview: the whole webview minus the scratch
/// strip at the top (which is in page coordinates, top-left), expressed in
/// the superview's space. One function for the first frame and for every
/// resize, so the view can never disagree with where the image is drawn.
fn overlay_rect(wk_view: &NSView) -> Option<NSRect> {
    let superview = unsafe { wk_view.superview()? };
    let bounds = wk_view.bounds();
    let rect_in_wk = NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(bounds.size.width, (bounds.size.height - STRIP).max(1.0)),
    );
    Some(superview.convertRect_fromView(rect_in_wk, Some(wk_view)))
}

/// Once per process: keep the overlay's frame true across the window's own
/// resizes. **Measured by the person 2026-10-17**: he maximized the client
/// and the overlay stayed at its old rect, so his clicks and hover went
/// stale - the fix is this, plus the mapping reading the view's own bounds.
fn watch_resize(window: &tauri::WebviewWindow) {
    static WATCHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WATCHED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let handle = window.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Resized(_)) {
            let handle = handle.clone();
            let _ = handle.with_webview(|pv| {
                let wk_view: &NSView = unsafe { &*pv.inner().cast::<NSView>() };
                if let Some(rect) = overlay_rect(wk_view) {
                    OVERLAY.with(|held| {
                        if let Some(overlay) = held.borrow().as_ref() {
                            overlay.setFrame(rect);
                        }
                    });
                }
            });
        }
    });
}

/// The pick is the whole ask: once a target is chosen the surface comes up on
/// its own, so the person's next action is using it, not pressing another
/// scratch button.
async fn show_after_target(window: &tauri::WebviewWindow) -> Result<(), String> {
    watch_resize(window);
    let (shown, answer) = tokio::sync::oneshot::channel::<Result<(), String>>();
    window
        .with_webview(move |pv| {
            let _ = shown.send(start_capture(&pv));
        })
        .map_err(|why| format!("the client window was not there: {why}"))?;
    answer.await.map_err(|_| "the overlay never answered".to_owned())?
}

/// Scratch: make the window findable wherever the person is - on every space
/// and over a fullscreen app - so the look never needs a space switch.
/// macOS hides an all-spaces window from a fullscreen space unless the window
/// is also full-screen auxiliary.
pub fn join_every_space(window: &tauri::WebviewWindow) {
    let _ = window.with_webview(|pv| {
        let wk: &NSView = unsafe { &*pv.inner().cast::<NSView>() };
        if let Some(ns_window) = wk.window() {
            let behavior = ns_window.collectionBehavior();
            ns_window.setCollectionBehavior(
                behavior
                    | NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
        }
    });
}

/// The picker's fallback (the plan's Task 1 step 3): ask for the Screen
/// Recording grant, then filter the parked window directly.
///
/// **The first `SCShareableContent` request is what raises the TCC prompt**,
/// and the answer needs the person; a granted app gets the window list with
/// no prompt at all. The window the picker would have named is found by id in
/// `/tmp/embed-spike/state/cdp.json`.
#[tauri::command]
pub async fn capture_grant(window: tauri::WebviewWindow) -> Result<PickedWindow, String> {
    let target: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(TARGET)
            .map_err(|why| format!("no capture target at {TARGET}: {why}"))?,
    )
    .map_err(|why| format!("the capture target at {TARGET} does not parse: {why}"))?;
    let window_id = u32::try_from(target["window_id"].as_u64().unwrap_or(0))
        .map_err(|_| "the capture target names no window".to_owned())?;

    let (sender, receiver) = tokio::sync::oneshot::channel::<Result<PickedWindow, String>>();
    window
        .with_webview(move |_pv| {
            let deliver = Mutex::new(Some(sender));
            let completion = block2::RcBlock::new(
                move |content: *mut SCShareableContent, error: *mut NSError| {
                    match direct_from_listing(content, error, window_id) {
                        Ok((picked, filter)) => {
                            store_picked(&filter);
                            let deliver = deliver.lock().ok().and_then(|mut held| held.take());
                            if let Some(sender) = deliver {
                                let _ = sender.send(Ok(picked));
                            }
                        }
                        Err(said) => {
                            let deliver = deliver.lock().ok().and_then(|mut held| held.take());
                            if let Some(sender) = deliver {
                                let _ = sender.send(Err(said));
                            }
                        }
                    }
                },
            );
            unsafe {
                SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
                    false,
                    false,
                    &completion,
                );
            }
        })
        .map_err(|why| format!("the client window was not there: {why}"))?;

    let picked = receiver.await.map_err(|_| "the listing never answered".to_owned())??;
    show_after_target(&window)
        .await
        .map_err(|why| format!("the window was found, but the capture did not start: {why}"))?;
    Ok(picked)
}

/// The listing's answer as a picked window plus the direct filter - the other
/// half of what the picker hands over. Runs on the listing's own queue, so
/// nothing here touches main-thread-only state.
fn direct_from_listing(
    content: *mut SCShareableContent,
    error: *mut NSError,
    window_id: u32,
) -> Result<(PickedWindow, Retained<SCContentFilter>), String> {
    if !error.is_null() {
        let said = unsafe { &*error }.localizedDescription().to_string();
        return Err(format!("the screen-recording listing was refused: {said}"));
    }
    let Some(content) = (unsafe { content.as_ref() }) else {
        return Err("the listing answered with nothing".to_owned());
    };
    let windows = unsafe { content.windows() };
    let found = windows
        .iter()
        .find(|candidate| u32::try_from(unsafe { candidate.windowID() }).ok() == Some(window_id))
        .ok_or_else(|| format!("window {window_id} is not in the listing"))?;
    let filter: Retained<SCContentFilter> = unsafe {
        msg_send![SCContentFilter::alloc(), initWithDesktopIndependentWindow: &*found]
    };
    let described = describe(&filter);
    Ok((described, filter))
}

/// The strip the scratch door keeps at the top of the page, in points.
const STRIP: f64 = 72.0;

/// What a running stream counts: frames, and the last window's own rate.
struct SinkIvars {
    overlay: Retained<OverlayView>,
    frames: Cell<u64>,
    started: Cell<Instant>,
    reported_at: Cell<u64>,
    no_layer: Cell<u64>,
}

/// The overlay view: the captured window drawn in a layer, and every pointer
/// and key event on it forwarded to the captured page over CDP - the
/// connection the driver itself uses.
struct OverlayIvars {
    mapping: Mapping,
}

define_class!(
    #[unsafe(super(NSView))]
    #[ivars = OverlayIvars]
    struct OverlayView;

    impl OverlayView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // The first click into an inactive window both activates it and is
        // delivered here, so a click is never eaten.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                let _ = window.makeFirstResponder(Some(self));
            }
            let (x, y) = self.page_point(event);
            send_input(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mousePressed",
                    "x": x,
                    "y": y,
                    "button": "left",
                    "buttons": 1,
                    "clickCount": click_count(event),
                }),
            );
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let (x, y) = self.page_point(event);
            send_input(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseReleased",
                    "x": x,
                    "y": y,
                    "button": "left",
                    "buttons": 0,
                    "clickCount": click_count(event),
                }),
            );
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let (x, y) = self.page_point(event);
            send_input(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved",
                    "x": x,
                    "y": y,
                    "button": "left",
                    "buttons": 1,
                }),
            );
        }

        // Hover: the window accepts mouse-moved events while a capture is up.
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let (x, y) = self.page_point(event);
            send_input(
                "Input.dispatchMouseEvent",
                json!({ "type": "mouseMoved", "x": x, "y": y, "buttons": 0 }),
            );
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let (x, y) = self.page_point(event);
            let precise = event.hasPreciseScrollingDeltas();
            let lines = if precise { 1.0 } else { 10.0 };
            send_input(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseWheel",
                    "x": x,
                    "y": y,
                    "deltaX": -event.scrollingDeltaX() * lines,
                    "deltaY": -event.scrollingDeltaY() * lines,
                }),
            );
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let (key, code, virtual_code): (&str, &str, i64) = match event.keyCode() {
                36 | 76 => ("Enter", "Enter", 13),
                48 => ("Tab", "Tab", 9),
                51 => ("Backspace", "Backspace", 8),
                53 => ("Escape", "Escape", 27),
                123 => ("ArrowLeft", "ArrowLeft", 37),
                124 => ("ArrowRight", "ArrowRight", 39),
                125 => ("ArrowDown", "ArrowDown", 40),
                126 => ("ArrowUp", "ArrowUp", 38),
                _ => ("", "", -1),
            };
            if virtual_code >= 0 {
                for kind in ["rawKeyDown", "keyUp"] {
                    send_input(
                        "Input.dispatchKeyEvent",
                        json!({
                            "type": kind,
                            "key": key,
                            "code": code,
                            "windowsVirtualKeyCode": virtual_code,
                            "nativeVirtualKeyCode": virtual_code,
                        }),
                    );
                }
                return;
            }
            // Everything else is text: the characters the layout produced,
            // with control characters left out.
            let typed = event.characters().map(|chars| chars.to_string()).unwrap_or_default();
            let clean: String = typed.chars().filter(|c| !c.is_control()).collect();
            if !clean.is_empty() {
                send_input("Input.insertText", json!({ "text": clean }));
            }
        }
    }
);

impl OverlayView {
    fn new(frame: NSRect, mapping: Mapping, mtm: objc2::MainThreadMarker) -> Retained<Self> {
        let view = Self::alloc(mtm).set_ivars(OverlayIvars { mapping });
        unsafe { msg_send![super(view), initWithFrame: frame] }
    }

    /// The captured page's viewport CSS point for an event: overlay-local,
    /// flipped to top-left, scaled onto the window image by **the overlay's
    /// own bounds** - where the image is actually drawn, which survives a
    /// window resize - then the chrome offset taken off the top.
    fn page_point(&self, event: &NSEvent) -> (f64, f64) {
        let local = self.convertPoint_fromView(event.locationInWindow(), None);
        let bounds = self.bounds();
        let x_from_top = local.x;
        let y_from_top = bounds.size.height - local.y;
        let mapping = &self.ivars().mapping;
        let image_x = x_from_top * mapping.image_pt.0 / bounds.size.width;
        let image_y = y_from_top * mapping.image_pt.1 / bounds.size.height;
        (image_x, image_y - mapping.chrome)
    }
}

fn click_count(event: &NSEvent) -> i64 {
    event.clickCount() as i64
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = SinkIvars]
    struct FrameSink;

    unsafe impl NSObjectProtocol for FrameSink {}

    unsafe impl SCStreamOutput for FrameSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            of_type: SCStreamOutputType,
        ) {
            if of_type != SCStreamOutputType::Screen {
                return;
            }
            let ivars = self.ivars();
            let n = ivars.frames.get() + 1;
            ivars.frames.set(n);

            // The frame's own surface goes straight onto the layer. No copy:
            // the layer holds the surface, and the compositor draws it.
            if let Some(buffer) = unsafe { sample.image_buffer() } {
                let pixels: *const CVImageBuffer = &*buffer;
                let surface =
                    unsafe { CVPixelBufferGetIOSurface(pixels.cast_mut().cast::<c_void>()) };
                if !surface.is_null() {
                    match ivars.overlay.layer() {
                        Some(layer) => {
                            let contents: &AnyObject = unsafe { &*surface.cast::<AnyObject>() };
                            unsafe { layer.setContents(Some(contents)) };
                        }
                        None => ivars.no_layer.set(ivars.no_layer.get() + 1),
                    }
                }
            }

            if PER_FRAME.load(std::sync::atomic::Ordering::Relaxed) {
                tauri_plugin_log::log::info!(
                    "embed_spike_frame n={n} t={} (event_name embed_spike_frame)",
                    wall_micros(),
                );
            }

            let elapsed = ivars.started.get().elapsed();
            if elapsed >= Duration::from_secs(2) {
                let last = ivars.reported_at.get();
                let rate = (n - last) as f64 / elapsed.as_secs_f64();
                ivars.reported_at.set(n);
                ivars.started.set(Instant::now());
                tauri_plugin_log::log::info!(
                    "embed-spike stream: frames={n} last_window_fps={rate:.1} no_layer={} \
                     (event_name embed_spike_fps)",
                    ivars.no_layer.get(),
                );
            }
        }
    }
);

impl FrameSink {
    fn new(overlay: Retained<OverlayView>) -> Retained<Self> {
        let sink = Self::alloc().set_ivars(SinkIvars {
            overlay,
            frames: Cell::new(0),
            started: Cell::new(Instant::now()),
            reported_at: Cell::new(0),
            no_layer: Cell::new(0),
        });
        unsafe { msg_send![super(sink), init] }
    }
}

/// Build the stream over the picked content and hang the overlay view that
/// draws it. Runs on the main thread (inside `with_webview`).
fn start_capture(pv: &tauri::webview::PlatformWebview) -> Result<(), String> {
    if STREAM.with(|held| held.borrow().is_some()) {
        return Err("a stream is already running".to_owned());
    }
    let filter = picked_filter().ok_or("no window picked yet - press the picker button first")?;

    let wk_view: &NSView = unsafe { &*pv.inner().cast::<NSView>() };
    let superview =
        unsafe { wk_view.superview() }.ok_or("the webview had no superview to draw in")?;
    // The overlay covers the webview minus the scratch strip, converted from
    // the webview's own space - the same function every resize uses.
    let rect = overlay_rect(wk_view).ok_or("the webview had no frame to draw over")?;

    let mtm = objc2::MainThreadMarker::new().ok_or("the overlay was not made on the main thread")?;

    // The input side's target: which page to drive, and the geometry that
    // maps an overlay point onto it (measured, recorded by the spike's own
    // tooling - the client never guesses it).
    let target: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(TARGET)
            .map_err(|why| format!("no capture target at {TARGET}: {why}"))?,
    )
    .map_err(|why| format!("the capture target at {TARGET} does not parse: {why}"))?;
    let mapping = Mapping {
        image_pt: (
            target["img_w"].as_f64().unwrap_or(1200.0),
            target["img_h"].as_f64().unwrap_or(988.0),
        ),
        chrome: target["chrome"].as_f64().unwrap_or(81.0),
        port: u16::try_from(target["port"].as_u64().unwrap_or(0))
            .map_err(|_| "the capture target names no usable port".to_owned())?,
        page_path: target["page"]
            .as_str()
            .ok_or("the capture target names no page")?
            .to_owned(),
    };
    ensure_input_worker(mapping.port, &mapping.page_path);

    let overlay = OverlayView::new(rect, mapping, mtm);
    overlay.setWantsLayer(true);
    superview.addSubview(&overlay);
    if let Some(window) = overlay.window() {
        window.setAcceptsMouseMovedEvents(true);
    }
    let scale = f64::from(unsafe { filter.pointPixelScale() });
    if let Some(layer) = overlay.layer() {
        layer.setMasksToBounds(true);
        layer.setContentsScale(scale);
    }

    let configuration = unsafe { SCStreamConfiguration::new() };
    let content = unsafe { filter.contentRect() };
    unsafe {
        configuration.setWidth((content.size.width * scale).round() as usize);
        configuration.setHeight((content.size.height * scale).round() as usize);
        configuration.setMinimumFrameInterval(CMTime::new(1, 60));
        configuration.setQueueDepth(5);
        configuration.setShowsCursor(false);
        configuration.setIgnoreShadowsSingleWindow(true);
    }

    let sink = FrameSink::new(overlay.clone());
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &configuration,
            None,
        )
    };
    unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*sink),
                SCStreamOutputType::Screen,
                Some(DispatchQueue::main()),
            )
            .map_err(|error| {
                format!("the stream refused its output: {}", error.localizedDescription())
            })?;
    }
    tauri_plugin_log::log::info!(
        "embed-spike stream: starting at {}x{} px (scale {scale}) over the overlay's {}x{} pt \
         (event_name embed_spike_stream_start)",
        (content.size.width * scale).round(),
        (content.size.height * scale).round(),
        rect.size.width,
        rect.size.height,
    );
    unsafe { stream.startCaptureWithCompletionHandler(None) };
    // The latency probe rides every stream: per-frame and per-input stamps,
    // read off the log afterwards. Scratch logging, nothing parses it at
    // runtime.
    PER_FRAME.store(true, std::sync::atomic::Ordering::Relaxed);

    // The stream keeps its outputs; these keep the stream and the view.
    std::mem::forget(sink);
    OVERLAY.with(|held| *held.borrow_mut() = Some(overlay));
    STREAM.with(|held| *held.borrow_mut() = Some(stream));
    Ok(())
}

/// Stop the stream and take the overlay down.
fn stop_capture() {
    if let Some(stream) = STREAM.with(|held| held.borrow_mut().take()) {
        unsafe { stream.stopCaptureWithCompletionHandler(None) };
    }
    if let Some(overlay) = OVERLAY.with(|held| held.borrow_mut().take()) {
        overlay.removeFromSuperview();
    }
}

/// Start the stream over the picked window and draw it in the client's own
/// window, inside `region` (viewport CSS pixels).
#[tauri::command]
pub async fn capture_show(
    window: tauri::WebviewWindow,
    _x: f64,
    _y: f64,
    _width: f64,
    _height: f64,
) -> Result<(), String> {
    // The page's rect used to place the overlay and it was wrong (it came
    // from the window's inner size, titlebar included); the overlay now
    // takes the webview's own frame minus the strip, so the page's numbers
    // are accepted for compatibility and ignored.
    watch_resize(&window);
    let (sender, receiver) = tokio::sync::oneshot::channel::<Result<(), String>>();
    window
        .with_webview(move |pv| {
            let _ = sender.send(start_capture(&pv));
        })
        .map_err(|why| format!("the client window was not there: {why}"))?;
    receiver.await.map_err(|_| "the overlay never answered".to_owned())?
}

/// Stop the stream and remove the overlay.
#[tauri::command]
pub async fn capture_hide(window: tauri::WebviewWindow) -> Result<(), String> {
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    window
        .with_webview(move |_pv| {
            stop_capture();
            let _ = sender.send(());
        })
        .map_err(|why| format!("the client window was not there: {why}"))?;
    receiver.await.map_err(|_| "the overlay never answered".to_owned())
}
