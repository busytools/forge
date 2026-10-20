//! The overlay view's ivars, on the process main thread.
//!
//! **`NSView` is main-thread-only**, and a libtest thread is not the main
//! thread, so this target is `harness = false`: `main` builds a real Wry app
//! and runs the check through the crate's `testing` entry point. It answers
//! nextest's `--list --format terse` in libtest's own shape.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    if std::env::args().any(|arg| arg == "--list") {
        // `--ignored` must list only ignored tests, and this one never is;
        // listing it there reads to nextest as an ignored test and skips it.
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("the_overlay_view_ivars_hold: test");
        }
        return std::process::ExitCode::SUCCESS;
    }
    let app = tauri::Builder::default()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("a Wry app builds on the main thread");
    match forge_client::browser::cef::surface::testing::the_overlay_view_ivars_hold(
        app.handle().clone(),
    ) {
        Ok(()) => {
            println!("the overlay view stores, clears and drops its ivars");
            std::process::ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("the overlay view check failed: {why}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    // No AppKit, no overlay view: nothing to list and nothing to run.
    std::process::ExitCode::SUCCESS
}
