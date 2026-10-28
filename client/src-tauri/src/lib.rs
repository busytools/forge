use std::io::Write;
use std::path::PathBuf;
#[cfg(desktop)]
use std::time::Duration;

#[cfg(desktop)]
use tauri_plugin_updater::UpdaterExt;

/// Bound on each update request - the check and the download alike. A stalled
/// connection otherwise holds the header's "updating..." for the session.
#[cfg(desktop)]
const UPDATE_TIMEOUT: Duration = Duration::from_secs(60);

/// The stages that finish an install, named by the platform that needs them.
/// The client's line draws its words per stage, so this is a fact rather than
/// copy: the desktop restarts into the swapped bundle, the phone's install is
/// the system prompt and the app is replaced with it.
#[cfg(desktop)]
const STAGE_RESTART: &str = "restart";
#[cfg(target_os = "android")]
const STAGE_INSTALL: &str = "install";

#[cfg(desktop)]
fn updater(app: &tauri::AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    app.updater_builder().timeout(UPDATE_TIMEOUT).build().map_err(|err| err.to_string())
}

// `path` and `state` on the handle are the Manager trait's; one import serves
// every target, the phone included.
use tauri::Manager as _;

mod asks;
pub mod bridge;
pub mod browser;
mod launch;
#[cfg(test)]
mod log_capture;
#[cfg(all(target_os = "macos", feature = "look-driver"))]
mod look;
pub mod mic;
#[cfg(target_os = "android")]
pub mod micstate;
pub mod notify;
pub mod perf;
pub mod records;
pub mod socket;

/// The webview's commands: the page reaches the socket and the records
/// through these, and hears their moves as `client://*` events. Thin by
/// design - the logic is `bridge.rs`'s, and its tests are there.
mod commands {
    use std::sync::Arc;

    use serde_json::Value;
    use tauri::Emitter as _;
    use tauri::Manager as _;

    use crate::bridge::{Bridge, ClientEvent};

    /// Push the bridge's events to the page under their own names.
    pub fn pump(app: tauri::AppHandle, mut rx: tokio::sync::mpsc::UnboundedReceiver<ClientEvent>) {
        tauri::async_runtime::spawn(async move {
            while let Some(event) = rx.recv().await {
                let _ = match event {
                    ClientEvent::Connection => app.emit("client://connection", ()),
                    ClientEvent::Inbound(message) => app.emit("client://inbound", message),
                    ClientEvent::Framed { subject, bytes, parse_ms } => app.emit(
                        "client://framed",
                        serde_json::json!({ "subject": subject, "bytes": bytes, "parse_ms": parse_ms }),
                    ),
                    ClientEvent::Written { key, declared, waited_ms } => app.emit(
                        "client://written",
                        serde_json::json!({ "key": key, "declared": declared, "waited_ms": waited_ms }),
                    ),
                    ClientEvent::Refused { key, why } => {
                        app.emit("client://refused", serde_json::json!({ "key": key, "why": why }))
                    }
                    ClientEvent::Mic(bytes) => app.emit("client://mic", bytes),
                };
            }
        });
    }

    /// Build the bridge, manage it, and start its pump. Both arms call this.
    pub fn init(app: &tauri::AppHandle) {
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Bridge::new(Some(app.clone()), events);
        app.manage(Arc::clone(&bridge));
        app.manage(Arc::new(crate::mic::Mic::default()));
        // **The viewer is the desktop's**, and so is the native layer it
        // draws into: the phone paints the same frames in its page instead.
        #[cfg(target_os = "macos")]
        app.manage(crate::browser::Viewer::new(app.clone()));
        pump(app.clone(), rx);
    }

    /// Dial the server. **`async`, though the body is sync**: a sync command
    /// runs on the main thread, where no tokio reactor is running and the
    /// tasks the connection spawns would panic at birth. Under `async` the
    /// body runs on the runtime instead.
    #[tauri::command(async)]
    pub fn client_connect(bridge: tauri::State<'_, Arc<Bridge>>, url: String) -> u64 {
        bridge.connect(url)
    }

    #[tauri::command]
    pub fn client_state(bridge: tauri::State<'_, Arc<Bridge>>) -> Value {
        bridge.state()
    }

    #[tauri::command]
    pub fn client_subscribe(bridge: tauri::State<'_, Arc<Bridge>>, what: Value, answering: bool) {
        bridge.subscribe(what, answering);
    }

    #[tauri::command]
    pub fn client_unsubscribe(bridge: tauri::State<'_, Arc<Bridge>>, what: Value) {
        bridge.unsubscribe(&what);
    }

    #[tauri::command]
    pub fn client_refresh(bridge: tauri::State<'_, Arc<Bridge>>, what: Value) {
        bridge.refresh(&what);
    }

    /// Give a subject back entirely: every reader and every server entry.
    #[tauri::command]
    pub fn client_release(bridge: tauri::State<'_, Arc<Bridge>>, what: Value) {
        bridge.release(&what);
    }

    /// Which seat the page has on screen; null for none.
    ///
    /// **Sync on purpose**: two rapid invokes must land in order, and the
    /// body only enqueues - the plugin work is the notify thread's.
    #[tauri::command]
    pub fn client_showing(bridge: tauri::State<'_, Arc<Bridge>>, seat: Option<Value>) {
        bridge.showing(seat);
    }

    /// Ask again for a subject the page still holds, adding no reader: the
    /// page's half of a resume after a drop.
    #[tauri::command]
    pub fn client_relist(bridge: tauri::State<'_, Arc<Bridge>>, what: Value, answering: bool) {
        bridge.relist(what, answering);
    }

    /// Older turns of a conversation; false means no socket is open and no
    /// page is coming. The default width is the page's own `MORE_TURNS`.
    #[tauri::command]
    pub fn client_more(
        bridge: tauri::State<'_, Arc<Bridge>>,
        conversation: Value,
        before: Option<String>,
        turns: Option<u64>,
    ) -> bool {
        bridge.more(&conversation, before, turns.unwrap_or(20))
    }

    /// One repair ask: the frames a seat's stream emitted between two numbers.
    /// False means no socket is open and no answer is coming, which costs
    /// nothing - the reconnect's snapshot is the whole record.
    #[tauri::command]
    pub fn client_seat_range(
        bridge: tauri::State<'_, Arc<Bridge>>,
        slot: Value,
        epoch: Option<String>,
        from: u64,
        to: u64,
    ) -> bool {
        bridge.seat_range(&slot, epoch, from, to)
    }

    #[tauri::command]
    pub async fn client_dispatch(
        bridge: tauri::State<'_, Arc<Bridge>>,
        command: Value,
        reply: bool,
    ) -> Result<Option<Value>, String> {
        let bridge = Arc::clone(bridge.inner());
        bridge.dispatch(command, reply).await
    }

    #[tauri::command]
    pub fn client_frame(bridge: tauri::State<'_, Arc<Bridge>>, bytes: Vec<u8>) -> bool {
        bridge.frame(bytes)
    }

    /// Keep this machine's microphone open for the picked input, so a press
    /// records from the press rather than opening a device under it.
    ///
    /// The page names the input to hold - at client start, and again when
    /// the pick changes - and the device stays open, and its indicator lit,
    /// until the client exits. A take consumes the held stream without
    /// closing it, so the hold outlives every take.
    ///
    /// A hold that will not open is answered rather than drawn: the caller
    /// carries on, and a press then opens the device itself exactly as it
    /// did before there was a hold. The open blocks on hardware, so it runs
    /// off the main thread like the take's.
    ///
    /// The hold also says which page load it comes from, which is how a
    /// reload mid-take is caught: a take left recording by a page that is
    /// gone can be ended by nothing else, so a hold from another page ends it
    /// rather than being answered by it.
    #[tauri::command]
    pub async fn client_mic_hold(
        bridge: tauri::State<'_, Arc<Bridge>>,
        mic: tauri::State<'_, Arc<crate::mic::Mic>>,
        device: Option<String>,
        page: String,
    ) -> Result<Value, String> {
        let bridge = Arc::clone(bridge.inner());
        let mic = Arc::clone(mic.inner());
        let answered = tauri::async_runtime::spawn_blocking(move || {
            mic.hold(&bridge, device, &page).map(|held| crate::mic::device_json(&held))
        })
        .await
        .map_err(|error| error.to_string())?;
        if let Err(why) = &answered {
            // A hold that will not open is not the reader's to act on at the
            // moment it happens - the press still opens the device itself -
            // but the log says why the warming is off.
            tauri_plugin_log::log::warn!("the microphone hold did not open: {why}");
        }
        answered
    }

    /// Open this machine's microphone for a take, and answer with the input
    /// it opened.
    ///
    /// The held capture serves the take when it is the one asked for, so
    /// this usually answers without touching the device at all; a device
    /// that is not held is opened here, which blocks on hardware and so runs
    /// off the main thread. The input comes back in the panel's own
    /// `{ id, label }` shape, which is what it names the system default by -
    /// asking for no device resolves to that default here, and nothing the
    /// page holds knows which one it was.
    ///
    /// The frames arrive as `client://mic` events while the take runs, and
    /// only then: a held stream's audio belongs to no take and is dropped.
    ///
    /// A press says which page load it comes from the way a hold does, so the
    /// first press after a reload ends the take the discarded page left
    /// rather than being refused by it, and it carries the take's own token,
    /// which the page minted as it opened the mic and is what the take's
    /// drain and stop name.
    ///
    /// **The open is bounded** (`crate::mic::OPEN_LIMIT`): a device layer that
    /// answers neither way is reported on the take's line rather than leaving
    /// the page waiting with nothing drawn.
    #[tauri::command]
    pub async fn client_mic_start(
        bridge: tauri::State<'_, Arc<Bridge>>,
        mic: tauri::State<'_, Arc<crate::mic::Mic>>,
        device: Option<String>,
        page: String,
        take: String,
    ) -> Result<Value, String> {
        let bridge = Arc::clone(bridge.inner());
        let mic = Arc::clone(mic.inner());
        mic.start_within(&bridge, device, &page, &take, crate::mic::OPEN_LIMIT)
            .await
            .map(|held| crate::mic::device_json(&held))
    }

    /// End the take: the audio still in flight lands, the last partial frame
    /// goes to the page, and the device stays open and warm for the next
    /// press.
    ///
    /// The caller sends its stop after this answers - the server finalises
    /// the take on that stop, so a frame that follows it would have no take
    /// to land in. It names the take's own token, so a drain still in flight
    /// from a take that has ended - a discarded page's, or the page's own
    /// last take - ends nothing.
    #[tauri::command]
    pub async fn client_mic_drain(
        mic: tauri::State<'_, Arc<crate::mic::Mic>>,
        take: String,
    ) -> Result<(), String> {
        let mic = Arc::clone(mic.inner());
        mic.drain(&take).await;
        Ok(())
    }

    /// End the take at once: an abandoned take throws its audio away, so
    /// nothing waits for the pipeline to land. The device stays open and
    /// warm for the next press, and the take is named for the same reason
    /// the drain names it: one in flight from a take that has ended ends
    /// nothing.
    #[tauri::command]
    pub fn client_mic_stop(mic: tauri::State<'_, Arc<crate::mic::Mic>>, take: String) {
        mic.stop(&take);
    }

    /// The inputs this machine can record from, for the panel's pick: the
    /// ids are the ones the capture resolves, which the browser's own
    /// device list does not know.
    #[tauri::command]
    pub async fn client_mic_devices() -> Result<Vec<Value>, String> {
        tauri::async_runtime::spawn_blocking(|| {
            forge_capture::devices()
                .map(|all| all.iter().map(crate::mic::device_json).collect::<Vec<Value>>())
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// One commit's own diff; false means no socket is open and no answer is
    /// coming.
    #[tauri::command]
    pub fn client_commit_diff(
        bridge: tauri::State<'_, Arc<Bridge>>,
        session: Value,
        sha: String,
    ) -> bool {
        bridge.commit_diff(&session, &sha)
    }

    /// One file's whole content at a revision; false means no socket is open
    /// and no answer is coming. The answer is a `file_content` frame, read
    /// from the page's own frame stream.
    #[tauri::command]
    pub fn client_file_content(
        bridge: tauri::State<'_, Arc<Bridge>>,
        session: Value,
        path: String,
        sha: Option<String>,
    ) -> bool {
        bridge.file_content(&session, path, sha)
    }

    /// One document read; false means no socket is open and no answer is
    /// coming. The answer is a `document_read` frame or an error, read from
    /// the page's own frame stream.
    #[tauri::command]
    pub fn client_read_document(
        bridge: tauri::State<'_, Arc<Bridge>>,
        session: Value,
        path: String,
        reviewable: bool,
    ) -> bool {
        bridge.read_document(&session, path, reviewable)
    }

    /// Attach this connection to a seat's browser tab as a viewer, answering
    /// the profile and tab it attached: a raise that named no tab is the
    /// asking seat's current one, and the page lowers that pair on Done.
    #[tauri::command]
    pub async fn client_browser_view(
        bridge: tauri::State<'_, Arc<Bridge>>,
        seat: Value,
        tab: Option<u16>,
    ) -> Result<Value, String> {
        bridge.browser_view(seat, tab).await
    }

    /// The tabs of this seat's profile, for the strip and the bar. Subscribing
    /// the connection means every later change is pushed as a `browser_tabs`
    /// event.
    #[tauri::command]
    pub async fn client_browser_tabs(
        bridge: tauri::State<'_, Arc<Bridge>>,
        seat: Value,
    ) -> Result<Value, String> {
        bridge.browser_tabs(seat).await
    }

    /// Take the controller role, or give it back.
    #[tauri::command]
    pub fn client_browser_control(bridge: tauri::State<'_, Arc<Bridge>>, take: bool) {
        bridge.browser_control(take);
    }

    /// Move through the viewed page's history.
    #[tauri::command]
    pub fn client_browser_nav(bridge: tauri::State<'_, Arc<Bridge>>, back: bool) {
        bridge.browser_nav(back);
    }

    /// The controller's surface changed size: device pixels and the ratio.
    #[tauri::command]
    pub fn client_browser_resize(
        bridge: tauri::State<'_, Arc<Bridge>>,
        width: u32,
        height: u32,
        scale: f64,
    ) {
        bridge.browser_resize(width, height, scale);
    }

    /// One input event from the controlling viewer.
    #[tauri::command]
    pub fn client_browser_input(
        bridge: tauri::State<'_, Arc<Bridge>>,
        event: forge_primitives::browser::BrowserInputEvent,
    ) {
        bridge.browser_input(event);
    }

    #[tauri::command]
    pub fn client_heartbeat(bridge: tauri::State<'_, Arc<Bridge>>) {
        bridge.heartbeat();
    }

    /// End one connection for good; no retry follows.
    ///
    /// `id` is the connection `client_connect` answered with, so a page
    /// connection the bridge has already replaced cannot shut the live one.
    #[tauri::command]
    pub fn client_close(bridge: tauri::State<'_, Arc<Bridge>>, id: u64) {
        bridge.close(id);
    }
}

/// The client logger's default directives, as data: `Info` for the crate, and
/// `debug` for the targets whose demoted lines must stay readable (rule 20) -
/// the client's half of the server's own directive table.
const LOG_BASELINE: tauri_plugin_log::log::LevelFilter = tauri_plugin_log::log::LevelFilter::Info;
const DEBUG_LEVEL: tauri_plugin_log::log::LevelFilter = tauri_plugin_log::log::LevelFilter::Debug;

/// The targets kept at [`DEBUG_LEVEL`]: the mic's press line is a session's
/// own work rather than forge's, and it is what a slow open is answered from.
pub(crate) const DEBUG_TARGETS: &[&str] = &[mic::LOG_TARGET];

/// The highest level any target is kept at. The logger's own facade filter
/// must allow at least this much, or it drops a record [`keeps`] would keep -
/// which with a `debug` target means every `debug!` body in the client now
/// evaluates, a formatting call apiece for the ones the filter then drops.
pub(crate) const LOG_MAX: tauri_plugin_log::log::LevelFilter = max_level(LOG_BASELINE, DEBUG_LEVEL);

const fn max_level(
    a: tauri_plugin_log::log::LevelFilter,
    b: tauri_plugin_log::log::LevelFilter,
) -> tauri_plugin_log::log::LevelFilter {
    if a as u8 >= b as u8 { a } else { b }
}

/// Whether the client's logger keeps a record: [`LOG_BASELINE`] for the crate,
/// [`DEBUG_LEVEL`] for a target [`DEBUG_TARGETS`] names or a module under one.
///
/// **The one policy both loggers read**: the shipped logger filters on it, and
/// the tests' capture keeps by it, so a test cannot model a filter the app
/// does not ship.
pub(crate) fn keeps(level: tauri_plugin_log::log::Level, target: &str) -> bool {
    level <= limit_for(target)
}

/// The level `target` is kept at: the most specific [`DEBUG_TARGETS`] entry
/// naming it or a parent module of it, the baseline otherwise.
fn limit_for(target: &str) -> tauri_plugin_log::log::LevelFilter {
    let mut named = target;
    loop {
        if DEBUG_TARGETS.contains(&named) {
            return DEBUG_LEVEL;
        }
        match named.rfind("::") {
            Some(at) => named = &named[..at],
            None => return LOG_BASELINE,
        }
    }
}

/// The client's logger under those directives.
fn client_logger() -> tauri_plugin_log::Builder {
    tauri_plugin_log::Builder::new()
        .level(LOG_MAX)
        .filter(|record| keeps(record.level(), record.target()))
}

/// The app, as a library: the Android target links it as a native library, and
/// the desktop binary in `main.rs` runs the same builder.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let log_file = log_file(&context.config().identifier);

    let builder = tauri::Builder::default().plugin(client_logger().build());

    // The updater plugin stops at the desktop; the phone's fetch, download,
    // signer check and installer handoff live in its own Kotlin plugin. Both
    // platforms answer the same commands, so the client's update line is one
    // surface either way. The viewer's commands exist on both: the desktop's
    // draw a native overlay, the phone's draw nothing and its page paints
    // the frames on a canvas.
    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // **A native resize settles the webview's frame after the page has
        // measured it**, and full screen's exit is the case that leaves the
        // overlay over the bar. The page is asked for one fresh measure once
        // the window's own resize has settled.
        .on_window_event(|window, event| {
            if matches!(
                event,
                tauri::WindowEvent::Resized(_) | tauri::WindowEvent::ScaleFactorChanged { .. }
            ) {
                browser::remeasure_soon(window.app_handle());
            }
        })
        .invoke_handler(
        tauri::generate_handler![
            browser::viewer::browser_surface_rect,
            browser::browser_takeover,
            browser::browser_kept_frame,
            commands::client_browser_view,
            commands::client_browser_tabs,
            commands::client_browser_control,
            commands::client_browser_nav,
            commands::client_browser_resize,
            commands::client_browser_input,
            check_update,
            install_update,
            restart_app,
            commands::client_connect,
            commands::client_state,
            commands::client_subscribe,
            commands::client_unsubscribe,
            commands::client_refresh,
            commands::client_showing,
            commands::client_relist,
            commands::client_release,
            commands::client_more,
            commands::client_seat_range,
            commands::client_dispatch,
            commands::client_frame,
            commands::client_mic_hold,
            commands::client_mic_start,
            commands::client_mic_drain,
            commands::client_mic_stop,
            commands::client_mic_devices,
            client_mic_state,
            client_tap_seat,
            commands::client_read_document,
            commands::client_file_content,
            commands::client_commit_diff,
            commands::client_heartbeat,
            commands::client_close,
            perf::client_perf
        ],
    );
    // iOS lands here too, and it registers nothing: the update commands are
    // the Kotlin plugin's, which is Android's own. Android passes through
    // this arm and then REPLACES the handler below.
    #[cfg(not(desktop))]
    let builder = builder;

    // **`invoke_handler` REPLACES the handler, it does not add to it** - so
    // the Android arm carries every command the desktop arm does, the
    // viewer's included (its stubs draw nothing: the phone's page paints the
    // frames itself).
    #[cfg(target_os = "android")]
    let builder = builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(android::init())
        .plugin(micstate::init())
        .plugin(launch::init())
        .invoke_handler(tauri::generate_handler![
            browser::viewer::browser_surface_rect,
            browser::browser_takeover,
            browser::browser_kept_frame,
            commands::client_browser_view,
            commands::client_browser_tabs,
            commands::client_browser_control,
            commands::client_browser_nav,
            commands::client_browser_resize,
            commands::client_browser_input,
            check_update,
            install_update,
            commands::client_connect,
            commands::client_state,
            commands::client_subscribe,
            commands::client_unsubscribe,
            commands::client_refresh,
            commands::client_showing,
            commands::client_relist,
            commands::client_release,
            commands::client_more,
            commands::client_seat_range,
            commands::client_dispatch,
            commands::client_frame,
            commands::client_mic_hold,
            commands::client_mic_start,
            commands::client_mic_drain,
            commands::client_mic_stop,
            commands::client_mic_devices,
            client_mic_state,
            client_tap_seat,
            commands::client_read_document,
            commands::client_file_content,
            commands::client_commit_diff,
            commands::client_heartbeat,
            commands::client_close,
            perf::client_perf
        ]);

    let run = builder
        .setup(|app| {
            tauri_plugin_log::log::info!("forge client started");
            #[cfg(not(desktop))]
            let _ = &app;
            // The bridge, the viewer and the pump: the socket lives here, and
            // its events are the only way the server reaches the page.
            commands::init(app.handle());
            // The asks a person cannot see: the channel and the click that
            // opens the seat a notification was posted for. **The plugin's
            // calls run on their own thread** - a mobile one blocks until
            // the main thread pumps, and this closure is that thread.
            #[cfg(any(desktop, target_os = "android"))]
            notify::init(app.handle());
            // **A built bundle can be driven for a look from this machine's
            // agent**, which has neither Accessibility nor Screen Recording:
            // the driver is the app's own door to those (feature-gated; the
            // shipping bundle never compiles it).
            #[cfg(all(target_os = "macos", feature = "look-driver"))]
            look::init(app.handle());
            Ok(())
        })
        .build(context);

    let app = match run {
        Ok(app) => app,
        Err(err) => {
            let line = format!("forge client failed to start: {err}");
            record(log_file.as_deref(), &line);
            eprintln!("{line}");
            std::process::exit(1);
        }
    };

    app.run(|handle, event| {
        // The held microphone goes with the client too, so the indicator is
        // out the moment the window is; a crash and a SIGKILL release it the
        // way the OS releases any process's device.
        if matches!(&event, tauri::RunEvent::Exit) {
            if let Some(mic) = handle.try_state::<std::sync::Arc<crate::mic::Mic>>() {
                mic.release();
            }
            return;
        }
        // **Looking away is when an ask on the shown seat becomes news**, and
        // no frame carries that moment.
        if let tauri::RunEvent::WindowEvent { event, .. } = &event
            && let Some(foreground) = attention(event)
        {
            #[cfg(mobile)]
            crate::asks::set_foreground(foreground);
            review_asks(handle, foreground);
        }
    });
}

/// The foreground state a window event states, or `None` when it says
/// nothing about the person's attention.
///
/// **A suspended mobile window stands in for a lost focus** (tao's Android
/// window never reports one), and a resume is the person coming back.
fn attention(event: &tauri::WindowEvent) -> Option<bool> {
    match event {
        tauri::WindowEvent::Focused(focused) => Some(*focused),
        #[cfg(mobile)]
        tauri::WindowEvent::Suspended => Some(false),
        #[cfg(mobile)]
        tauri::WindowEvent::Resumed => Some(true),
        _ => None,
    }
}

/// Re-read the asked state for the notification rule, where a focus or a
/// life-cycle change says the person's gaze moved.
fn review_asks(handle: &tauri::AppHandle, looking: bool) {
    if let Some(bridge) = handle.try_state::<std::sync::Arc<crate::bridge::Bridge>>() {
        bridge.review_asks(looking);
    }
}

/// What this platform is doing to the microphone right now, or `None` where
/// it reports nothing of the kind - the desktop and the browser both draw the
/// row's line without a cause.
#[cfg(desktop)]
#[tauri::command]
async fn client_mic_state() -> Result<Option<crate::mic::MicState>, String> {
    Ok(None)
}

/// [`client_mic_state`], answered by the phone's own readers.
#[cfg(target_os = "android")]
#[tauri::command]
async fn client_mic_state(app: tauri::AppHandle) -> Result<Option<crate::mic::MicState>, String> {
    let Some(reader) = app.try_state::<std::sync::Arc<crate::micstate::Reader>>() else {
        // The row's line still draws without a cause, so this is the log's
        // business rather than the page's.
        tauri_plugin_log::log::warn!("the microphone's state readers are not up");
        return Ok(None);
    };
    match reader.state().await {
        Ok(state) => Ok(Some(state)),
        Err(why) => {
            tauri_plugin_log::log::warn!("the microphone's state could not be read: {why}");
            Ok(None)
        }
    }
}

/// The seat a notification tap launched the client on, asked once by the page
/// that comes up. Every other launch, on every platform, answers `None`.
#[tauri::command]
async fn client_tap_seat(app: tauri::AppHandle) -> Option<forge_primitives::SessionSlot> {
    crate::launch::take(&app).await
}

/// The version an update check found, or `None` when this build is current.
#[cfg(desktop)]
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    match updater(&app)?.check().await {
        Ok(found) => Ok(found.map(|update| update.version)),
        Err(err) => Err(err.to_string()),
    }
}

/// Download and install the found update, answering with the stage that
/// finishes it: the desktop swaps the bundle under a running app, so the
/// restart is the reader's next step and an install never takes the window
/// out from under them.
#[cfg(desktop)]
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<String, String> {
    let mut update = updater(&app)?
        .check()
        .await
        .map_err(|err| err.to_string())?
        .ok_or_else(|| "no update is available".to_string())?;
    // A found update carries no timeout of its own - `check` builds it with
    // `timeout: None` and the download only bounds itself when this is set -
    // so the bound is put back on before the download runs.
    update.timeout = Some(UPDATE_TIMEOUT);
    update.download_and_install(|_, _| {}, || {}).await.map_err(|err| {
        tauri_plugin_log::log::warn!("the client update failed to install: {err}");
        err.to_string()
    })?;
    Ok(STAGE_RESTART.to_string())
}

/// Restart into the installed update.
#[cfg(desktop)]
#[tauri::command]
fn restart_app(app: tauri::AppHandle) {
    app.restart()
}

/// The phone's half of the updater, as a Kotlin plugin the commands below
/// bridge to. Everything the update does on Android is Android's own: the
/// manifest fetch, the download, the signer read of this install, and the
/// installer intent.
#[cfg(target_os = "android")]
mod android {
    use std::sync::Mutex;

    use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
    use tauri::{Manager, Runtime};

    /// The Kotlin handle, and the release the last check offered. The install
    /// works from the remembered one rather than asking again: the line the
    /// reader tapped names that version, and a check between the two would
    /// install a release the line never named.
    pub struct Updater<R: Runtime>(pub PluginHandle<R>, pub Mutex<Option<Found>>);

    /// The Kotlin handle for a picked content URI's document name.
    pub struct FileName<R: Runtime>(pub PluginHandle<R>);

    #[derive(serde::Serialize)]
    pub struct UriArgs {
        pub uri: String,
    }

    #[derive(serde::Serialize)]
    pub struct CheckArgs {
        pub version: String,
        pub endpoint: String,
    }

    #[derive(Clone, serde::Deserialize)]
    pub struct Found {
        pub version: String,
        pub url: String,
    }

    #[derive(serde::Serialize)]
    pub struct InstallArgs {
        pub version: String,
        pub url: String,
    }

    pub fn init<R: Runtime>() -> TauriPlugin<R> {
        Builder::new("androidupdate")
            .setup(|app, api| {
                let handle = api.register_android_plugin("dev.vedhavyas.forge", "UpdatePlugin")?;
                app.manage(Updater(handle, Mutex::new(None)));
                let names = api.register_android_plugin("dev.vedhavyas.forge", "FileNamePlugin")?;
                app.manage(FileName(names));
                Ok(())
            })
            .build()
    }
}

/// The endpoint both platforms read, from the one place it is written.
#[cfg(target_os = "android")]
fn update_endpoint(app: &tauri::AppHandle) -> Result<String, String> {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|updater| updater.get("endpoints"))
        .and_then(|endpoints| endpoints.as_array())
        .and_then(|endpoints| endpoints.first())
        .and_then(|endpoint| endpoint.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "no updater endpoint is configured".to_string())
}

#[cfg(target_os = "android")]
async fn android_check(app: &tauri::AppHandle) -> Result<Option<android::Found>, String> {
    let handle = app.state::<android::Updater<tauri::Wry>>();
    let args = android::CheckArgs {
        version: app.package_info().version.to_string(),
        endpoint: update_endpoint(app)?,
    };
    handle.0.run_mobile_plugin_async("check", args).await.map_err(|err| err.to_string())
}

/// The version an update check found, or `None` when this build is current.
/// What it found is remembered for the install that line-offering may follow.
#[cfg(target_os = "android")]
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let found = android_check(&app).await?;
    if let Ok(mut remembered) = app.state::<android::Updater<tauri::Wry>>().1.lock() {
        *remembered = found.clone();
    }
    Ok(found.map(|found| found.version))
}

/// Download the remembered release, check it, and hand it to the system
/// installer, answering with the stage that finishes it: the phone's install
/// IS the prompt, and the app is replaced with it.
#[cfg(target_os = "android")]
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<String, String> {
    let handle = app.state::<android::Updater<tauri::Wry>>();
    let found = handle
        .1
        .lock()
        .ok()
        .and_then(|remembered| remembered.clone())
        .ok_or_else(|| "no update is available".to_string())?;
    handle
        .0
        .run_mobile_plugin_async::<()>(
            "install",
            android::InstallArgs { version: found.version, url: found.url },
        )
        .await
        .map_err(|err| err.to_string())?;
    Ok(STAGE_INSTALL.to_string())
}

/// Where the log plugin's file target lands, named from the product rather than
/// the crate. Recomputes `tauri::path::PathResolver::app_log_dir` for macOS,
/// because the failure path below has no handle to ask.
fn log_file(identifier: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Logs").join(identifier).join("forge.log"))
}

/// The line a failed start leaves.
///
/// This arm is reachable only from a build failure, which happens at or before
/// the log plugin's own setup - so the logger is not up, and a bundle launched
/// from Finder hands stderr to nobody. It is the one place a line has to be
/// written by hand.
fn record(log_file: Option<&std::path::Path>, line: &str) {
    if let Some(path) = log_file {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::attention;

    /// **A blur is the moment the rule must look again**, and no frame
    /// carries it. A gain of focus asks the same review; an event that says
    /// nothing about attention leaves the rule alone.
    #[test]
    fn a_focus_change_moves_the_attention_rule() {
        assert_eq!(attention(&tauri::WindowEvent::Focused(false)), Some(false));
        assert_eq!(attention(&tauri::WindowEvent::Focused(true)), Some(true));
        assert_eq!(attention(&tauri::WindowEvent::Destroyed), None);
    }

    /// **The one policy the shipped logger and the tests' capture share.** A
    /// named target keeps `debug` and a module under it does too, since a
    /// file that moves into a subdirectory is the same module's own line; an
    /// unnamed target stops at `Info`, which is what keeps `forge.log`
    /// readable.
    #[test]
    fn the_default_directives_keep_debug_for_their_own_targets() {
        use super::{DEBUG_TARGETS, keeps};
        use tauri_plugin_log::log::Level;

        for target in DEBUG_TARGETS {
            assert!(keeps(Level::Debug, target), "a named target keeps `debug`: {target}");
            assert!(
                keeps(Level::Debug, &format!("{target}::submodule")),
                "a module under a named target keeps `debug` too: {target}",
            );
            assert!(keeps(Level::Info, target), "and `Info` is kept as well: {target}");
        }
        assert!(
            !keeps(Level::Debug, "forge_client::socket"),
            "an unnamed target stops at `Info`: debug is dropped",
        );
        assert!(keeps(Level::Warn, "forge_client::socket"), "a warning always crosses");
        assert!(keeps(Level::Info, "forge_client::socket"), "so does `Info`");
    }
}
