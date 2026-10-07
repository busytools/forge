fn main() {
    // **CEF claims NSApplication before tauri (through tao) touches it**, and
    // its own subprocesses re-enter this binary here; both live in the
    // bootstrap, which must be the first thing that runs.
    #[cfg(all(desktop, target_os = "macos"))]
    forge_client::browser::cef::bootstrap();

    forge_client::run();
}
