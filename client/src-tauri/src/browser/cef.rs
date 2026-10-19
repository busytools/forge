//! The app's own Chromium: the CEF host process.
//!
//! **The client's UI process never initializes CEF.** The same bundle
//! executable in `--cef-host` mode is the browser process for one profile:
//! it loads the framework, runs CEF's subprocess dance, claims NSApplication,
//! initializes CEF with windowless rendering and its profile's cache root,
//! and drives one OSR browser whose frames go into a shared ring
//! ([`osr`]). The client spawns it, talks to its DevTools port, and draws the
//! ring.
//!
//! The process is deliberately thin: no window, no Tauri, no view of the
//! socket. Its whole job is a browser whose pixels another process can see.

#[cfg(target_os = "macos")]
pub mod client_application;
#[cfg(target_os = "macos")]
pub mod dialog;
#[cfg(target_os = "macos")]
pub mod host;
#[cfg(target_os = "macos")]
pub mod input;
#[cfg(target_os = "macos")]
pub mod osr;
#[cfg(target_os = "macos")]
pub mod spawn;
#[cfg(target_os = "macos")]
pub mod surface;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use cef::*;

/// CEF called: work is due after `delay_ms` (<= 0 means now).
static DUE_MILLIS: AtomicU64 = AtomicU64::new(0);
static CLOCK: OnceLock<std::time::Instant> = OnceLock::new();

/// Milliseconds since the host's clock started.
pub fn elapsed_ms() -> u64 {
    CLOCK.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// The schedule signal, kept where [`pump`] reads it.
fn note_schedule(delay_ms: i64) {
    let due = elapsed_ms().saturating_add(delay_ms.max(0) as u64);
    DUE_MILLIS.store(due, Ordering::Release);
}

/// Whether CEF's last schedule has come due.
fn due() -> bool {
    elapsed_ms() >= DUE_MILLIS.load(Ordering::Acquire)
}

cef::wrap_browser_process_handler! {
    struct PumpHandler {}

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            note_schedule(delay_ms);
        }
    }
}

/// The endpoint a host answers CDP on.
pub fn endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// The host's arguments, when this process was spawned in `--cef-host` mode.
pub fn host_args<I: IntoIterator<Item = String>>(args: I) -> Option<host::Args> {
    host::parse(args)
}

/// Become the browser host. The whole mode is one process's life.
pub fn host_main(args: host::Args) -> ! {
    host::run(args)
}

/// Where the framework sits for an executable at `exe`.
///
/// A browser process looks in `Contents/Frameworks` beside its
/// `Contents/MacOS` binary; a helper app looks three directories up from its
/// own binary. Both are CEF's own layout rules.
pub fn framework_path(exe: &std::path::Path, helper: bool) -> Option<std::path::PathBuf> {
    let parent = exe.parent()?;
    let path = if helper { parent.join("../../..") } else { parent.join("../Frameworks") };
    Some(path.join("Chromium Embedded Framework.framework/Chromium Embedded Framework"))
}

/// Whether this bundle carries the framework, so a host can be spawned.
pub fn available() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    framework_path(&exe, false).is_some_and(|path| path.is_file())
}

/// Whether this process is one of CEF's subprocesses: every one re-enters
/// the bundle with a `--type=` switch.
pub fn is_subprocess() -> bool {
    std::env::args().any(|arg| arg.starts_with("--type="))
}

/// Run CEF's subprocess path and exit. The helper app's whole life is
/// `execute_process`; the browser process is the one that carries on.
pub fn run_subprocess() -> ! {
    let Ok(exe) = std::env::current_exe() else {
        std::process::exit(1);
    };
    let loader = library_loader::LibraryLoader::new(&exe, true);
    if !loader.load() {
        eprintln!("forge helper: the CEF framework did not load");
        std::process::exit(1);
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = args::Args::new();
    let ret = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    std::process::exit(if ret >= 0 { 0 } else { 1 });
}

/// Load the framework and initialize CEF for one profile. Answers whether
/// the browser process came up.
///
/// **The pump is the host's** (`external_message_pump`): the host's main
/// thread is the only place `do_message_loop_work` runs.
pub fn initialize(profile: &std::path::Path, port: u16, app: &mut App, args: &args::Args) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("forge host: the executable path is unavailable");
        return false;
    };
    let loader = library_loader::LibraryLoader::new(&exe, false);
    if !loader.load() {
        eprintln!("forge host: the CEF framework did not load");
        return false;
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let settings = Settings {
        no_sandbox: 1,
        external_message_pump: 1,
        windowless_rendering_enabled: 1,
        root_cache_path: CefString::from(profile.to_string_lossy().as_ref()),
        remote_debugging_port: i32::from(port),
        ..Default::default()
    };
    let initialized = cef::initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(app),
        std::ptr::null_mut(),
    );
    if initialized != 1 {
        eprintln!("forge host: CEF did not initialize");
        return false;
    }
    CLOCK.get_or_init(std::time::Instant::now);
    true
}

/// Run CEF's work when its schedule asks for it.
pub fn pump() {
    if due() {
        do_message_loop_work();
    }
}

// The app rules every browser this host runs obeys, decided before CEF
// parses its command line.
cef::wrap_app! {
    pub struct HostApp;

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(PumpHandler::new())
        }

        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(cmd) = command_line else { return };
            // **Never the OS keychain.** Untold, Chromium reaches for
            // "Chromium Safe Storage" and macOS raises a dialog at whoever
            // is at the machine, once per ask.
            cmd.append_switch(Some(&CefString::from("use-mock-keychain")));
            cmd.append_switch_with_value(
                Some(&CefString::from("password-store")),
                Some(&CefString::from("basic")),
            );
            // **A windowless browser keeps working.** The surface is not a
            // native window; without these the renderer treats it as
            // backgrounded and stalls the sessions' calls.
            cmd.append_switch(Some(&CefString::from("disable-renderer-backgrounding")));
            cmd.append_switch(Some(&CefString::from(
                "disable-backgrounding-occluded-windows",
            )));
        }
    }
}
