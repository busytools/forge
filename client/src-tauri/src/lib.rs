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

#[cfg(desktop)]
fn updater(app: &tauri::AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    app.updater_builder().timeout(UPDATE_TIMEOUT).build().map_err(|err| err.to_string())
}

use tauri::Manager as _;

pub mod browser;

/// The app, as a library: the Android target links it as a native library, and
/// the desktop binary in `main.rs` runs the same builder.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let log_file = log_file(&context.config().identifier);

    let builder = tauri::Builder::default().plugin(
        tauri_plugin_log::Builder::new().level(tauri_plugin_log::log::LevelFilter::Info).build(),
    );

    // The updater plugin stops at the desktop, and these commands are the
    // client's own so both platforms reach one JS surface: Android answers the
    // same three from its Kotlin side. The browser's command rides the same
    // handler - a second `invoke_handler` would replace this one rather than
    // add to it - and the host itself is the client's on both platforms, so
    // the mobile arm registers it too.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build()).invoke_handler(
        tauri::generate_handler![
            browser::browser_call,
            browser::browser_context_release,
            browser::browser_show,
            check_update,
            install_update,
            restart_app
        ],
    );
    #[cfg(not(desktop))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        browser::browser_call,
        browser::browser_context_release,
        browser::browser_show
    ]);

    let run = builder
        .setup(|app| {
            tauri_plugin_log::log::info!("forge client started");
            // The browser host is handed to the frontend whether or not its
            // directories resolve: a client that cannot host says so when it
            // is asked, rather than refusing to start.
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
            // **The browser comes up with the app**, so it is there before
            // any session asks for it: the launch takes seconds and a tool
            // call should not pay for it, and a browser that cannot start
            // says so here, in the client's log, rather than as a failed tool
            // call nobody can attribute. Spawned rather than awaited - the
            // window does not wait on a browser - and the driver stays lazy,
            // since it exists to serve calls.
            let starting = std::sync::Arc::clone(&host);
            tauri::async_runtime::spawn(async move {
                match starting.start().await {
                    Ok(active) => {
                        tauri_plugin_log::log::info!("the browser is up on port {}", active.port)
                    }
                    Err(why) => tauri_plugin_log::log::warn!("the browser did not start: {why}"),
                }
            });
            app.manage(host);
            Ok(())
        })
        .run(context);

    if let Err(err) = run {
        let line = format!("forge client failed to start: {err}");
        record(log_file.as_deref(), &line);
        eprintln!("{line}");
        std::process::exit(1);
    }
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

/// Download and install the found update. Restarting is the caller's next
/// step, so an install never takes the window out from under a reader.
#[cfg(desktop)]
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
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
    })
}

/// Restart into the installed update.
#[cfg(desktop)]
#[tauri::command]
fn restart_app(app: tauri::AppHandle) {
    app.restart()
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
