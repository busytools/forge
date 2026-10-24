//! The host application's ivars, on the process main thread.
//!
//! **`NSApplication` is main-thread-only**, and a libtest thread is not the
//! main thread, so this target is `harness = false`: `main` runs the check
//! through the crate's `testing` entry point. It answers nextest's
//! `--list --format terse` in libtest's own shape.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    if std::env::args().any(|arg| arg == "--list") {
        // `--ignored` must list only ignored tests, and this one never is;
        // listing it there reads to nextest as an ignored test and skips it.
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("the_host_application_holds_its_ivars_and_policy: test");
        }
        return std::process::ExitCode::SUCCESS;
    }
    match forge_browser::client_application::testing::the_host_application_holds_its_ivars_and_policy()
    {
        Ok(()) => {
            println!("the host application stores its ivars and takes the accessory policy");
            std::process::ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("the application ivar check failed: {why}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    // No AppKit, no application class: nothing to list and nothing to run.
    std::process::ExitCode::SUCCESS
}
