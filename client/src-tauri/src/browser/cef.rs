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

/// Whether `initialize` has answered. The client's loop starts whether or
/// not CEF did, so the pump must never reach an uninitialized CEF.
static INITIALIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// The process-wide app: the rules every browser this client runs obeys,
// decided before CEF parses its own command line.
wrap_app! {
    pub struct ClientApp;

    impl App {
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

/// The process bootstrap, at the very top of `main`.
///
/// CEF re-enters this binary for its own subprocesses (and on macOS through
/// the bundle's helper apps); `execute_process` is what runs them, and a
/// subprocess's whole life is that call. The browser process carries on to
/// claim NSApplication and `initialize`.
pub fn bootstrap() {
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

    let mut app = ClientApp::new();
    let settings = Settings {
        no_sandbox: 1,
        // **The pump is the embedder's.** tao drives its own loop and CEF's
        // work is done on it - see `pump`.
        external_message_pump: 1,
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
    } else {
        eprintln!("forge client: CEF did not initialize; the browser stays off");
    }
}

/// One turn of CEF's work, run from the client's own loop (tauri's
/// `RunEvent::MainEventsCleared`).
pub fn pump() {
    if INITIALIZED.load(std::sync::atomic::Ordering::Acquire) {
        do_message_loop_work();
    }
}
