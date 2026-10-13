//! Startup-time process-state bumps. Currently just the
//! `RLIMIT_NOFILE` raise (#251) - macOS launchd hands GUI-spawned
//! processes a soft cap of 256 open FDs, and multi-session forge
//! steady-state crosses that ceiling (~15-25 FDs per session ×
//! claude pipes / watchers / MCP / tokio kqueues). Without the bump,
//! git scans and other openers fail with `EMFILE` once enough
//! sessions are open.
//!
//! That per-session estimate was measured when each session also held
//! proxy sockets, so it now runs high; re-measure before treating it
//! as current.

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

/// The value `scripts/install.sh` exports before its cargo call. Any
/// other value, including the empty string a build script sees when the
/// variable is absent, means the guard did not run.
const GUARDED_MARKER: &str = "install.sh";

/// What to warn about `provenance`, or `None` when the build came
/// through the guarded path.
///
/// `cargo install` ignores `Cargo.lock` unless `--locked` is passed, so
/// a hand-rolled install silently resolves dependencies afresh and can
/// ship a graph no CI run ever tested. That is not hypothetical: it is
/// how a blocking terminal query reached a user while `just check` was
/// green. The remedy belongs in the message because a log line that
/// only names the problem leaves the reader where that incident
/// started.
fn build_provenance_warning(provenance: &str) -> Option<String> {
    if provenance == GUARDED_MARKER {
        return None;
    }
    Some(
        "this binary was built outside scripts/install.sh, so its dependency graph may not be \
         the one the lockfile pins and CI tested; rebuild with scripts/install.sh, or with \
         cargo install --locked if installing by hand"
            .to_owned(),
    )
}

/// Record how this binary was built, so a drifted one says so instead
/// of being silent.
///
/// Call once at startup after tracing init. The `Cargo.lock` digest is
/// reported alongside because it catches a build that honoured a
/// locally-modified one, which provenance cannot see because the guard
/// did run. It only means something compared against a build of the
/// released tag - a bare digest in a log is not self-evidently correct,
/// and being FNV-1a it will not match `shasum`.
pub fn report_build_provenance() {
    let provenance = crate::FORGE_BUILD_PROVENANCE;
    let cargo_lock = crate::FORGE_CARGO_LOCK_DIGEST;
    match build_provenance_warning(provenance) {
        None => tracing::info!(
            target: crate::logging::targets::APP_LIFECYCLE,
            event_name = "build_provenance",
            message = "binary built through the guarded install path",
            outcome = "success",
            provenance,
            cargo_lock_digest = cargo_lock,
        ),
        Some(remedy) => tracing::warn!(
            target: crate::logging::targets::APP_LIFECYCLE,
            event_name = "build_provenance_unguarded",
            message = %remedy,
            outcome = "failure",
            provenance,
            cargo_lock_digest = cargo_lock,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A binary built outside the guard has to say so AND say what to
    /// do about it - a log line that only names the problem leaves the
    /// reader where this incident started.
    #[test]
    fn an_unguarded_build_reports_how_to_rebuild_it() {
        let warning = build_provenance_warning("unguarded").expect("unguarded builds warn");
        assert!(
            warning.contains("scripts/install.sh"),
            "the warning names the guarded path to rebuild with: {warning}",
        );
        assert!(
            warning.contains("--locked"),
            "the warning names the flag a hand-rolled build needs: {warning}",
        );
    }

    /// The guarded path is the whole point of the marker: a build that
    /// went through it must be silent, or the warning is noise on every
    /// correct install and stops being read.
    #[test]
    fn a_guarded_build_does_not_warn() {
        assert!(
            build_provenance_warning(GUARDED_MARKER).is_none(),
            "a build through the guarded path has nothing to report",
        );
    }

    /// Anything that is not the marker is untrusted, including an empty
    /// value - the default when the env var is absent, which is exactly
    /// what a hand-rolled `cargo install` produces.
    #[test]
    fn an_unrecognised_marker_is_treated_as_unguarded() {
        for value in ["", "yes", "1", "GUARDED", "guarded "] {
            assert!(
                build_provenance_warning(value).is_some(),
                "{value:?} is not the marker and must not be trusted as guarded",
            );
        }
    }

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
