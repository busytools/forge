//! The server's browser host executable, and CEF's own subprocess entry.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use forge_browser::{engine, host};

    // CEF's subprocesses re-enter this binary with `--type=`; their whole
    // life is `execute_process`.
    if engine::is_subprocess() {
        return engine::run_subprocess();
    }
    if let Some(args) = host::parse(std::env::args().skip(1)) {
        return host::run(&args);
    }
    eprintln!("usage: forge-browser --cef-host --profile DIR --port N --ring NAME [--size WxH]");
    std::process::ExitCode::from(2)
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("forge-browser: the CEF host is macOS-only");
    std::process::ExitCode::from(1)
}
