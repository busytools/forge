//! The client's own Chromium, in-process: CEF.
//!
//! **The real engine, rendered natively.** The takeover shows this browser's
//! own view inside the client's window - no frames, no relay - and the
//! sessions' driver attaches to its CDP endpoint the way it attached to the
//! vendored Chrome. This module is the process bootstrap: the helper dance
//! every CEF process needs, the NSApplication subclass CEF's macOS protocol
//! hooks into, one `initialize`, and the pump that carries CEF's work on the
//! client's own event loop.
//!
//! **Order matters at boot.** CEF claims NSApplication before tao (which
//! Tauri rides) touches it, so `bootstrap` runs at the top of `main`; the
//! pump runs later, from the tauri loop.

use cef::*;

#[cfg(target_os = "macos")]
mod client_application;
#[cfg(target_os = "macos")]
pub mod view;

/// Whether `initialize` has answered. The client's loop starts whether or
/// not CEF did, so the pump must never reach an uninitialized CEF.
static INITIALIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The port CEF's own CDP answers on, chosen before `initialize` (CEF's
/// `remote_debugging_port` of 0 means DISABLED, unlike the browser flag, so
/// a port is always named). What the sessions' driver attaches to.
static DEBUG_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// **CEF's own schedule for the pump.** `on_schedule_message_pump_work` says
/// when work exists; `pump` runs only when that says so. Calling
/// `do_message_loop_work` unconditionally per loop turn - the shape this
/// replaced - spins CrBrowserMain at >50% CPU on an idle client.
static DUE_MILLIS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CLOCK: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn elapsed_ms() -> u64 {
    CLOCK.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// CEF called: work is due after `delay_ms` (<= 0 means now).
fn note_schedule(delay_ms: i64) {
    let due = elapsed_ms().saturating_add(delay_ms.max(0) as u64);
    DUE_MILLIS.store(due, std::sync::atomic::Ordering::Release);
}

/// Whether CEF's last schedule has come due.
pub fn due() -> bool {
    elapsed_ms() >= DUE_MILLIS.load(std::sync::atomic::Ordering::Acquire)
}

/// The first port nothing holds, from a small private range.
fn pick_debug_port() -> u16 {
    for port in 9411..9430_u16 {
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    0
}

/// The port CEF's CDP answers on, or 0 when CEF is not up.
pub fn debug_port() -> u16 {
    DEBUG_PORT.load(std::sync::atomic::Ordering::Acquire)
}

// The process-wide app: the rules every browser this client runs obeys,
// decided before CEF parses its own command line - and the schedule the
// pump obeys.
wrap_app! {
    pub struct ClientApp;

    impl App {
        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(ClientBrowserProcessHandler::new())
        }

        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(cmd) = command_line else { return };
            // **Never the OS keychain.** Untold, Chromium reaches for
            // "Chromium Safe Storage" and macOS raises a dialog at whoever
            // is at the machine, once per ask - the same flags the vendored
            // browser's launches carry.
            cmd.append_switch(Some(&CefString::from("use-mock-keychain")));
            cmd.append_switch_with_value(
                Some(&CefString::from("password-store")),
                Some(&CefString::from("basic")),
            );
            // **A hidden browser keeps working.** The view is only shown
            // during a takeover; between them the sessions still drive the
            // page, and a backgrounded renderer would stall their calls.
            cmd.append_switch(Some(&CefString::from("disable-renderer-backgrounding")));
            cmd.append_switch(Some(&CefString::from("disable-backgrounding-occluded-windows")));
        }
    }
}

// CEF's schedule signal, carried into `DUE_MILLIS`.
wrap_browser_process_handler! {
    struct ClientBrowserProcessHandler {}

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            note_schedule(delay_ms);
        }
    }
}

/// The process bootstrap, at the very top of `main`.
///
/// CEF re-enters this binary for its own subprocesses (and on macOS through
/// the bundle's helper apps); `execute_process` is what runs them, and a
/// subprocess's whole life is that call. The browser process carries on to
/// claim NSApplication and `initialize`.
pub fn bootstrap(identifier: &str) {
    // A diagnostic door for the dev loop: the client with no CEF at all.
    if std::env::var_os("FORGE_NO_CEF").is_some() {
        eprintln!("forge client: CEF skipped (FORGE_NO_CEF)");
        return;
    }
    // **The framework comes first.** Every CEF wrapper below calls into it -
    // `Args` and the command line included - and before `load` + `api_hash`
    // those calls land on null pointers.
    let Ok(executable) = std::env::current_exe() else {
        eprintln!("forge client: the executable path is unavailable; CEF stays off");
        return;
    };
    let loader = library_loader::LibraryLoader::new(&executable, false);
    if !loader.load() {
        eprintln!("forge client: the CEF framework did not load; CEF stays off");
        return;
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = args::Args::new();
    let Some(cmd_line) = args.as_cmd_line() else {
        eprintln!("forge client: the command line could not be read; CEF stays off");
        return;
    };
    let is_browser_process = cmd_line.has_switch(Some(&CefString::from("type"))) != 1;

    let ret = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    if !is_browser_process {
        // A subprocess answers through its exit code and lives no longer.
        std::process::exit(if ret >= 0 { 0 } else { 1 });
    }

    #[cfg(target_os = "macos")]
    client_application::setup();

    // **The profile is forge's own.** Its own directory for now: the
    // vendored Chromium still runs beside it on `browser/profile` during the
    // migration, and two Chromiums on one profile means a `SingletonLock`
    // fight - CEF refuses to start rather than risk the profile. The
    // retirement step moves this to `browser/profile` as the vendored one
    // goes, and CEF's `DevToolsActivePort` then lands where the host already
    // looks for it.
    let profile = match dirs::data_dir() {
        Some(dir) => dir.join(identifier).join("browser/cef-profile"),
        None => {
            eprintln!("forge client: no application data directory; CEF stays off");
            return;
        }
    };
    let profile = CefString::from(profile.to_string_lossy().as_ref());
    if let Err(why) = std::fs::create_dir_all(profile.to_string()) {
        eprintln!("forge client: the profile directory cannot be made ({why}); CEF stays off");
        return;
    }

    let port = pick_debug_port();
    if port == 0 {
        eprintln!("forge client: no free port for CEF's CDP; CEF stays off");
        return;
    }
    DEBUG_PORT.store(port, std::sync::atomic::Ordering::Release);

    let mut app = ClientApp::new();
    let settings = Settings {
        no_sandbox: 1,
        // **The pump is the embedder's.** tao drives its own loop and CEF's
        // work is done on it - see `pump`.
        external_message_pump: 1,
        // The cache root IS the profile: cookies, logins and the rest.
        root_cache_path: profile,
        remote_debugging_port: i32::from(port),
        ..Default::default()
    };
    let initialized = initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if initialized == 1 {
        INITIALIZED.store(true, std::sync::atomic::Ordering::Release);
        eprintln!("forge client: CEF is up, debugging on 127.0.0.1:{port}");
    } else {
        DEBUG_PORT.store(0, std::sync::atomic::Ordering::Release);
        eprintln!("forge client: CEF did not initialize; the browser stays off");
    }
}

/// One turn of CEF's work, run from the client's own loop - **only when
/// CEF's schedule asked for it.** Anything else is a spin.
pub fn pump() {
    if !INITIALIZED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    if !due() {
        return;
    }
    do_message_loop_work();
}
