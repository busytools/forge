use std::io::Write;
use std::path::PathBuf;

use tauri::Manager as _;

pub mod browser;

/// The app, as a library: the Android target links it as a native library, and
/// the desktop binary in `main.rs` runs the same builder.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let log_file = log_file(&context.config().identifier);

    let run = tauri::Builder::default()
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(tauri_plugin_log::log::LevelFilter::Info)
                .build(),
        )
        .invoke_handler(tauri::generate_handler![browser::browser_call])
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
