//! Open a URL in the user's default browser via the OS-native helper
//! (`open` / `xdg-open` / `rundll32`). Fire-and-forget — the helper
//! spawns and detaches; we don't capture stdout/stderr.
//!
//! Lives in `forge-agent::env` because the CLAUDE.md crate-placement
//! anti-pattern table says subprocess work doesn't belong in
//! forge-tui. forge-workspace exposes this via
//! `Workspace::open_url_in_browser`; forge-tui dispatches via that.

/// Spawn the platform-native URL handler for `url`. Returns `Ok(())`
/// on successful spawn (the child runs detached after); `Err` carries
/// the underlying I/O error string.
pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut cmd = std::process::Command::new("rundll32.exe");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        cmd
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut cmd = std::process::Command::new("open");
        cmd.arg(url);
        cmd
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut cmd = std::process::Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("failed to open browser automatically: {error}"))
}
