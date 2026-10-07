//! The CEF helper: every subprocess Chromium spawns is one of these.
//!
//! CEF finds them by layout, not by argument: for a main executable named
//! `forge-client`, it re-enters `forge-client Helper.app`, `forge-client
//! Helper (GPU).app` and the rest inside the app's `Contents/Frameworks`.
//! A helper's whole life is `execute_process` - the browser process carries
//! the same call in `browser::cef::bootstrap` and outlives it.

#[cfg(target_os = "macos")]
fn main() {
    use cef::*;

    // The framework first: every CEF wrapper calls into it.
    let Ok(executable) = std::env::current_exe() else {
        eprintln!("forge helper: the executable path is unavailable");
        std::process::exit(1);
    };
    let loader = library_loader::LibraryLoader::new(&executable, true);
    if !loader.load() {
        eprintln!("forge helper: the CEF framework did not load");
        std::process::exit(1);
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = args::Args::new();
    let ret = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    std::process::exit(if ret >= 0 { 0 } else { 1 });
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("forge helper: the CEF helper is macOS-only");
    std::process::exit(1);
}
