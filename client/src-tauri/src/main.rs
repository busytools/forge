fn main() {
    // **CEF claims NSApplication before tauri (through tao) touches it**, and
    // its own subprocesses re-enter this binary here; both live in the
    // bootstrap, which must be the first thing that runs. The identifier
    // names the app-data directory CEF's profile hangs off, the same one the
    // browser host resolves later.
    #[cfg(all(desktop, target_os = "macos"))]
    {
        let context: tauri::Context = tauri::generate_context!();
        forge_client::browser::cef::bootstrap(context.config().identifier.as_str());
    }

    forge_client::run();
}
