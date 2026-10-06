use std::io::Write;
use std::path::PathBuf;
#[cfg(desktop)]
use std::time::Duration;

#[cfg(desktop)]
use tauri_plugin_updater::UpdaterExt;
// `state` and `config` on the handle are the Manager trait's, and the phone's
// commands reach the plugin through them.
#[cfg(target_os = "android")]
use tauri::Manager as _;

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
    app.updater_builder()
        .timeout(UPDATE_TIMEOUT)
        .build()
        .map_err(|err| err.to_string())
}

/// The app, as a library: the Android target links it as a native library, and
/// the desktop binary in `main.rs` runs the same builder.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let log_file = log_file(&context.config().identifier);

    let builder = tauri::Builder::default().plugin(
        tauri_plugin_log::Builder::new()
            .level(tauri_plugin_log::log::LevelFilter::Info)
            .build(),
    );

    // The updater plugin stops at the desktop; the phone's fetch, download,
    // signer check and installer handoff live in its own Kotlin plugin. Both
    // platforms answer the same commands, so the client's update line is one
    // surface either way.
    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            check_update,
            install_update,
            restart_app
        ]);

    #[cfg(target_os = "android")]
    let builder = builder
        .plugin(android::init())
        .invoke_handler(tauri::generate_handler![check_update, install_update]);

    let run = builder
        .setup(|_app| {
            tauri_plugin_log::log::info!("forge client started");
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
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|err| {
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
    handle
        .0
        .run_mobile_plugin_async("check", args)
        .await
        .map_err(|err| err.to_string())
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
            android::InstallArgs {
                version: found.version,
                url: found.url,
            },
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
