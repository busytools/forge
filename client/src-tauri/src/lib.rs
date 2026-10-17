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

/// How long the exit reap is given before the quit goes on without it. The
/// normal reap is a kill by port per profile, well inside this; a close
/// waiting out a launch in flight is cut here rather than hanging the quit,
/// and the next boot's sweep reaps whatever the bound left.
#[cfg(desktop)]
const EXIT_REAP_BOUND: Duration = Duration::from_secs(10);

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

pub mod asks;
pub mod bridge;
pub mod browser;
pub mod mic;
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
                    ClientEvent::Refused { key, why } => {
                        app.emit("client://refused", serde_json::json!({ "key": key, "why": why }))
                    }
                    ClientEvent::Role { hosting } => app.emit("client://role", hosting),
                    ClientEvent::Asks { inflight } => app.emit("client://asks", inflight),
                    ClientEvent::Profiles => app.emit("client://profiles", ()),
                    ClientEvent::Mic(bytes) => app.emit("client://mic", bytes),
                };
            }
        });
    }

    /// Build the bridge over whatever browser host this target has, manage
    /// it, and start its pump. Both arms call this; a target with no host
    /// yet still gets a bridge, and it answers asks with that reason.
    pub fn init(app: &tauri::AppHandle) {
        let host = app
            .try_state::<Arc<crate::browser::BrowserHost>>()
            .map(|state| Arc::clone(state.inner()));
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Bridge::new(host, events);
        // **The idle release starts with the bridge**: its clock is the
        // host's, its warning rides the bridge's socket, and a client with
        // no host or no connection simply has nothing to sweep.
        #[cfg(desktop)]
        crate::browser::idle::start(&bridge);
        app.manage(Arc::clone(&bridge));
        app.manage(Arc::new(crate::mic::Mic::default()));
        pump(app.clone(), rx);
    }

    /// Dial the server. **`async`, though the body is sync**: a sync command
    /// runs on the main thread, where no tokio reactor is running and the
    /// tasks the connection spawns would panic at birth. Under `async` the
    /// body runs on the runtime instead.
    #[tauri::command(async)]
    pub fn client_connect(bridge: tauri::State<'_, Arc<Bridge>>, url: String) {
        bridge.connect(url);
    }

    #[tauri::command]
    pub fn client_state(bridge: tauri::State<'_, Arc<Bridge>>) -> Value {
        bridge.state()
    }

    #[tauri::command]
    pub fn client_subscribe(
        bridge: tauri::State<'_, Arc<Bridge>>,
        what: Value,
        answering: bool,
        browser: bool,
    ) {
        bridge.subscribe(what, answering, browser);
    }

    #[tauri::command]
    pub fn client_unsubscribe(bridge: tauri::State<'_, Arc<Bridge>>, what: Value) {
        bridge.unsubscribe(&what);
    }

    #[tauri::command]
    pub fn client_refresh(bridge: tauri::State<'_, Arc<Bridge>>, what: Value) {
        bridge.refresh(&what);
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

    #[tauri::command]
    pub fn client_take_role(bridge: tauri::State<'_, Arc<Bridge>>) {
        bridge.take_role();
    }

    #[tauri::command]
    pub fn client_heartbeat(bridge: tauri::State<'_, Arc<Bridge>>) {
        bridge.heartbeat();
    }

    /// End the connection for good; no retry follows.
    #[tauri::command]
    pub fn client_close(bridge: tauri::State<'_, Arc<Bridge>>) {
        bridge.close();
    }
}

/// The app, as a library: the Android target links it as a native library, and
/// the desktop binary in `main.rs` runs the same builder.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let log_file = log_file(&context.config().identifier);

    let builder = tauri::Builder::default().plugin(
        tauri_plugin_log::Builder::new().level(tauri_plugin_log::log::LevelFilter::Info).build(),
    );

    // The updater plugin stops at the desktop; the phone's fetch, download,
    // signer check and installer handoff live in its own Kotlin plugin. Both
    // platforms answer the same commands, so the client's update line is one
    // surface either way. The browser's commands are the desktop arm's here
    // because the desktop host is built in this arm's setup; the phone serves
    // the same set from its own host, `browser::android::init()`'s engine
    // bridge (its engine is the system WebView, its driver runs under
    // libnode in-process).
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build()).invoke_handler(
        tauri::generate_handler![
            browser::browser_call,
            browser::browser_profile_close,
            browser::browser_profile_delete,
            browser::browser_profiles,
            browser::browser_windowed,
            browser::browser_show,
            browser::browser_hide,
            browser::browser_used,
            check_update,
            install_update,
            restart_app,
            commands::client_connect,
            commands::client_state,
            commands::client_subscribe,
            commands::client_unsubscribe,
            commands::client_refresh,
            commands::client_more,
            commands::client_seat_range,
            commands::client_dispatch,
            commands::client_frame,
            commands::client_mic_hold,
            commands::client_mic_start,
            commands::client_mic_drain,
            commands::client_mic_stop,
            commands::client_mic_devices,
            commands::client_read_document,
            commands::client_file_content,
            commands::client_commit_diff,
            commands::client_take_role,
            commands::client_heartbeat,
            commands::client_close,
            perf::client_perf
        ],
    );
    // iOS lands here too, and it registers nothing: no host exists there to
    // serve either set (the update commands are the Kotlin plugin's, which
    // is Android's own, and the browser's engine is the phone's system
    // WebView). Android passes through this arm and then REPLACES the
    // handler below, with both sets real.
    #[cfg(not(desktop))]
    let builder = builder;

    // **`invoke_handler` REPLACES the handler, it does not add to it** - so
    // the Android arm carries every command the desktop arm does. The
    // browser's host is real on the phone now (its system WebView for the
    // engine, in-app libnode for the driver), so its command set is the
    // desktop's, served by `browser::android`'s engine bridge.
    #[cfg(target_os = "android")]
    let builder = builder.plugin(android::init()).plugin(browser::android::init()).invoke_handler(
        tauri::generate_handler![
            browser::browser_call,
            browser::browser_profile_close,
            browser::browser_profile_delete,
            browser::browser_profiles,
            browser::browser_windowed,
            browser::browser_show,
            browser::browser_hide,
            browser::browser_used,
            check_update,
            install_update,
            commands::client_connect,
            commands::client_state,
            commands::client_subscribe,
            commands::client_unsubscribe,
            commands::client_refresh,
            commands::client_more,
            commands::client_seat_range,
            commands::client_dispatch,
            commands::client_frame,
            commands::client_mic_hold,
            commands::client_mic_start,
            commands::client_mic_drain,
            commands::client_mic_stop,
            commands::client_mic_devices,
            commands::client_read_document,
            commands::client_file_content,
            commands::client_commit_diff,
            commands::client_take_role,
            commands::client_heartbeat,
            commands::client_close,
            perf::client_perf
        ],
    );

    let run = builder
        .setup(|app| {
            tauri_plugin_log::log::info!("forge client started");
            #[cfg(not(desktop))]
            let _ = &app;
            // **The desktop host, built and started here.** The phone's own
            // host is `browser::android::init()`'s (its setup builds it
            // around the Kotlin engine handle and brings the WebView up), so
            // this block and its macOS-shaped start stay desktop-only.
            #[cfg(desktop)]
            {
                // The host is handed to the frontend whether or not its
                // directories resolve: a client that cannot host says so when
                // it is asked, rather than refusing to start.
                let host = match browser::StackPaths::resolve(app.handle()) {
                    Ok(paths) => {
                        tauri_plugin_log::log::info!("browser stack at {}", paths.stack.display());
                        std::sync::Arc::new(browser::BrowserHost::new(paths))
                    }
                    Err(why) => {
                        tauri_plugin_log::log::warn!("browser host unavailable: {why}");
                        std::sync::Arc::new(browser::BrowserHost::unavailable(why))
                    }
                };
                // **No browser launches here** (spam-control A4): the first
                // real need - a call or a raise - brings one up on demand, so
                // a client start costs nothing for browsers nobody asked for.
                // What still runs is the reap, the only cover for a client
                // that never ran its exit - a crash, a SIGKILL, or the update
                // restart, whose main-thread path skips `RunEvent::Exit` by
                // design; it launches nothing, and with no launch to mistake
                // for one, it reaps only what a previous run left.
                let reap = std::sync::Arc::clone(&host);
                tauri::async_runtime::spawn(async move {
                    reap.reap_orphans().await;
                });
                app.manage(host);
            }
            // The bridge, over whatever host this target has: the desktop's
            // managed just above, the phone's in its own plugin's setup. It
            // is what the page's commands reach, and its events are the
            // only way the socket and the records reach the page.
            commands::init(app.handle());
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
        // **A host must not outlive its client.** Every browser this client
        // claimed dies with it. `RunEvent::Exit` covers a quit and `exit()`;
        // a crash, a SIGKILL and the update restart's main-thread path run no
        // exit handler at all, and the next boot's sweep is what reaps those.
        #[cfg(desktop)]
        if let tauri::RunEvent::Exit = event
            && let Some(host) = handle.try_state::<std::sync::Arc<browser::BrowserHost>>()
        {
            tauri::async_runtime::block_on(async {
                let _ = tokio::time::timeout(EXIT_REAP_BOUND, host.reap_owned()).await;
            });
        }
        // The held microphone goes with the client too, so the indicator is
        // out the moment the window is; a crash and a SIGKILL release it the
        // way the OS releases any process's device.
        if let tauri::RunEvent::Exit = event
            && let Some(mic) = handle.try_state::<std::sync::Arc<crate::mic::Mic>>()
        {
            mic.release();
        }
    });
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
