//! Startup-time process-state bumps. Currently just the
//! `RLIMIT_NOFILE` raise (#251) - macOS launchd hands GUI-spawned
//! processes a soft cap of 256 open FDs, and multi-session forge
//! steady-state crosses that ceiling (~15-25 FDs per session ×
//! claude pipes / proxy sockets / watchers / MCP / tokio kqueues).
//! Without the bump, git scans, the wire-rewriter proxy, and other
//! openers fail with `EMFILE` once enough sessions are open.

use rlimit::Resource;

/// Target soft `RLIMIT_NOFILE` value. Well below macOS
/// `kern.maxfilesperproc` (typically 184320) and Linux containers'
/// usual hard caps (1024-65536). Capped against the process's hard
/// limit at call time so a tighter inherited hard cap clamps the
/// effective bump gracefully.
const TARGET_NOFILE: u64 = 8192;

/// Raise the soft `RLIMIT_NOFILE` limit toward `TARGET_NOFILE`,
/// clamped at the process's hard cap. No-op when the inherited soft
/// limit is already at or above the target. Logs the action at
/// `info` on success and at `warn` when getrlimit / setrlimit fails.
///
/// Should be called once at binary startup AFTER tracing init (so
/// the log lines land) and BEFORE any code that opens sockets /
/// files / pipes (so the bump applies to every subsequent opener).
/// Safe to call multiple times - the no-op branch makes repeats
/// cheap and idempotent.
pub fn raise_fd_limit() {
    let (soft, hard) = match Resource::NOFILE.get() {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(
                target: "forge_tui::startup",
                error = %err,
                "NOFILE getrlimit failed; leaving FD limit at inherited value",
            );
            return;
        }
    };
    let new_soft = TARGET_NOFILE.min(hard);
    if new_soft <= soft {
        tracing::info!(
            target: "forge_tui::startup",
            soft,
            hard,
            "RLIMIT_NOFILE already at or above target; no change",
        );
        return;
    }
    match Resource::NOFILE.set(new_soft, hard) {
        Ok(()) => {
            tracing::info!(
                target: "forge_tui::startup",
                old_soft = soft,
                new_soft,
                hard,
                "raised RLIMIT_NOFILE soft limit",
            );
        }
        Err(err) => {
            tracing::warn!(
                target: "forge_tui::startup",
                error = %err,
                soft,
                hard,
                new_soft,
                "NOFILE setrlimit failed; leaving at inherited value",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin both branches of `raise_fd_limit`: when the inherited soft
    /// limit is already at or above `min(TARGET_NOFILE, hard)`, the
    /// call is a no-op; otherwise the soft limit is bumped to that
    /// value. `Resource::NOFILE` is process-wide state and other
    /// tests may have already bumped it, so this test is robust to
    /// either initial condition.
    #[test]
    fn raise_fd_limit_bumps_soft_toward_target() {
        let (soft_before, hard) = Resource::NOFILE.get().expect("NOFILE getrlimit");

        raise_fd_limit();

        let (soft_after, _) = Resource::NOFILE.get().expect("NOFILE getrlimit");
        let expected = TARGET_NOFILE.min(hard);
        if soft_before >= expected {
            assert_eq!(
                soft_after, soft_before,
                "no-op branch: soft already >= target ({expected}), must not regress",
            );
        } else {
            assert_eq!(
                soft_after, expected,
                "bump branch: soft must rise to min(TARGET_NOFILE, hard) = {expected}",
            );
        }
    }

    /// Calling `raise_fd_limit` twice in a row is idempotent - the
    /// second call sees the bumped soft limit and takes the no-op
    /// branch. Pins the "safe to call multiple times" claim in the
    /// docstring so a future bug that overwrites the bump (e.g. a
    /// stray `set(soft, hard)` with the original soft) surfaces here.
    #[test]
    fn raise_fd_limit_is_idempotent() {
        raise_fd_limit();
        let (soft_after_first, _) = Resource::NOFILE.get().expect("NOFILE getrlimit");
        raise_fd_limit();
        let (soft_after_second, _) = Resource::NOFILE.get().expect("NOFILE getrlimit");
        assert_eq!(
            soft_after_first, soft_after_second,
            "second call must be a no-op; got {soft_after_first} then {soft_after_second}",
        );
    }
}
