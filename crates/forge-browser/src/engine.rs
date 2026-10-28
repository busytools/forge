//! The host's own Chromium: framework load, CEF initialization and the pump.
//!
//! **The host process is the browser process.** It loads the framework from
//! its own bundle, runs CEF's subprocess dance for its helpers, claims
//! NSApplication, and initializes CEF with windowless rendering and one
//! profile's cache root. Nothing here draws or serves a session; the host
//! loop in [`crate::host`] drives one OSR browser whose frames go into a
//! shared ring.

#![allow(
    clippy::transmute_ptr_to_ptr,
    reason = "CEF's wrap_* macros transmute the wrapper reference to the base object"
)]

use std::sync::OnceLock;

use cef::*;

static CLOCK: OnceLock<std::time::Instant> = OnceLock::new();

/// Milliseconds since the host's clock started.
pub fn elapsed_ms() -> u64 {
    let elapsed = CLOCK.get_or_init(std::time::Instant::now).elapsed();
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

cef::wrap_browser_process_handler! {
    struct PumpHandler {}

    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, _delay_ms: i64) {
            // **Pumped unconditionally instead.** Gating the pump on this
            // signal delayed CDP input: a click arriving while CEF's next
            // scheduled work sat far ahead waited for it, which the person
            // felt as a click that takes a moment. A 1 ms loop processes
            // work a thousand times a second at a few percent of a core.
        }
    }
}

/// The endpoint a host answers CDP on.
pub fn endpoint(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Whether this process is one of CEF's subprocesses: every one re-enters
/// the bundle with a `--type=` switch.
pub fn is_subprocess() -> bool {
    std::env::args().any(|arg| arg.starts_with("--type="))
}

/// Run CEF's subprocess path and exit. The helper app's whole life is
/// `execute_process`; the browser process is the one that carries on.
pub fn run_subprocess() -> std::process::ExitCode {
    let Ok(exe) = std::env::current_exe() else {
        return std::process::ExitCode::from(1);
    };
    let loader = library_loader::LibraryLoader::new(&exe, true);
    if !loader.load() {
        eprintln!("forge helper: the CEF framework did not load");
        return std::process::ExitCode::from(1);
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = args::Args::new();
    let ret = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    if ret >= 0 { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::from(1) }
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
        // **Both paths, to the profile itself.** `cache_path` is the one that
        // makes the profile persistent (cookies, storage, logins); alone,
        // `root_cache_path` only names the shared parent and CEF runs the
        // profile incognito.
        cache_path: CefString::from(profile.to_string_lossy().as_ref()),
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

/// One turn of CEF's work. **Called every iteration**, not on CEF's own
/// schedule: see the browser process handler above.
pub fn pump() {
    do_message_loop_work();
}

/// The Chromium switches every host adds before the feature list is built.
///
/// **`MacAppCodeSignClone` is the disk one.** Chromium clones the whole app
/// bundle into `/var/folders/.../X` per launch to keep a staged update's
/// signature valid; a CEF host has no updater, and 172 test launches filled a
/// disk. The rest are the keychain and windowless-browser rules above.
///
/// **One entry per switch**: a repeated switch keeps the last value, so a
/// second `disable-blink-features` tuple would silently replace this one.
pub(crate) const HOST_SWITCHES: &[(&str, Option<&str>)] = &[
    ("use-mock-keychain", None),
    ("password-store", Some("basic")),
    ("disable-renderer-backgrounding", None),
    ("disable-backgrounding-occluded-windows", None),
    ("disable-features", Some("MacAppCodeSignClone")),
    // No platform authenticator here, so a site that sees WebAuthn offers a
    // passkey path that cannot finish; the password form is the way in.
    ("disable-blink-features", Some("WebAuth")),
];

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
            for (name, value) in HOST_SWITCHES {
                match value {
                    Some(value) => cmd.append_switch_with_value(
                        Some(&CefString::from(*name)),
                        Some(&CefString::from(*value)),
                    ),
                    None => cmd.append_switch(Some(&CefString::from(*name))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HOST_SWITCHES;

    /// **The code-sign clone is off before the feature list exists.** It is
    /// the per-launch disk cost; the keychain and windowless rules stay.
    #[test]
    fn the_host_disables_the_mac_app_code_sign_clone() {
        assert!(
            HOST_SWITCHES.contains(&("disable-features", Some("MacAppCodeSignClone"))),
            "without this every launch clones the app bundle into the temp tree",
        );
        assert!(HOST_SWITCHES.contains(&("use-mock-keychain", None)), "the keychain rule stays");
    }

    /// **The feature is Blink's `WebAuth`, not the API's class name**; a
    /// switch naming `WebAuthn` matches nothing and the API comes back.
    #[test]
    fn the_host_offers_no_webauthn() {
        assert!(
            HOST_SWITCHES.contains(&("disable-blink-features", Some("WebAuth"))),
            "a page that sees PublicKeyCredential takes a passkey path this host cannot finish",
        );
    }
}
