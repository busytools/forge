//! Logging setup. Three rolling-file streams under `config.log_dir`:
//!
//! - `forged.events.log` — INFO+ structured events
//! - `forged.errors.log` — WARN+ filter for quick scanning
//! - `forged.audit.log` — per-WS-connection records (target = `forged::audit`)
//!
//! Daily rotation; retention is enforced by a sweep at startup that
//! deletes files older than `log_retention_days`.

use std::path::{Path, PathBuf};

use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt};

use crate::Error;
use crate::config::Config;

/// Filename prefix shared by every log file forged emits. The retention
/// sweep filters on this so we never touch unrelated files in the log
/// directory.
const FORGED_LOG_PREFIX: &str = "forged.";

/// Initialise logging based on the daemon config.
///
/// Idempotent — `try_init` returns `Err` quietly if a global subscriber
/// has already been installed (e.g. from an earlier call inside a
/// multi-test process). The sweep + log-dir creation still happen so
/// that callers exercising those side effects in tests get the same
/// behaviour as a cold daemon boot.
///
/// # Errors
///
/// I/O errors when creating `log_dir`, or appender-init errors when the
/// chosen filename prefix is invalid.
pub fn init(config: &Config) -> Result<(), Error> {
    let dir = expand_home(&config.log_dir);
    std::fs::create_dir_all(&dir).map_err(Error::Io)?;
    sweep_old(&dir, config.log_retention_days);

    let events = build_appender(&dir, "forged.events.log")?;
    let errors = build_appender(&dir, "forged.errors.log")?;
    let audit = build_appender(&dir, "forged.audit.log")?;

    // Default RUST_LOG: forged at INFO, forge_sdk at WARN. Operator can
    // override with `RUST_LOG=forged=debug` etc.
    let env = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("forged=info,forge_sdk=warn"));

    // Events: everything not specifically scoped to audit. We exclude
    // the `forged::audit` target so the audit stream doesn't double-tee
    // into events.
    let events_layer = fmt::layer()
        .with_writer(events)
        .with_target(true)
        .with_filter(EnvFilter::new(
            "forged=info,forge_sdk=warn,forged::audit=off",
        ));

    // Errors: WARN+ across the workspace, again excluding audit to keep
    // streams disjoint.
    let errors_layer = fmt::layer()
        .with_writer(errors)
        .with_target(true)
        .with_filter(EnvFilter::new("warn,forged::audit=off"));

    // Audit: only `forged::audit` events (target-scoped). INFO+ keeps
    // routine connection-open / connection-close traceable.
    let audit_layer = fmt::layer()
        .with_writer(audit)
        .with_target(true)
        .with_filter(EnvFilter::new("forged::audit=info"));

    // `try_init` is the idempotent variant — second call returns Err but
    // we ignore it because the first install is the one that wins.
    let _ = Registry::default()
        .with(env)
        .with(events_layer)
        .with(errors_layer)
        .with(audit_layer)
        .try_init();
    Ok(())
}

/// Expand a leading `~/` against `$HOME`. Other paths are passed through
/// untouched.
#[must_use]
pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Build a daily-rotation appender. Uses [`Builder`] (not
/// `RollingFileAppender::new`) so init failures surface as
/// [`Error::InternalError`] rather than panicking — important because
/// `panic` is denied for non-test code in this crate.
fn build_appender(
    dir: &Path,
    prefix: &str,
) -> Result<tracing_appender::rolling::RollingFileAppender, Error> {
    Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .build(dir)
        .map_err(|e| Error::InternalError(format!("logging init ({prefix}): {e}")))
}

/// Walk `dir`, deleting files whose mtime is older than `retention_days`
/// AND whose filename starts with `forged.`. The prefix filter is
/// belt-and-braces — a misconfigured `log_dir` pointing at a shared
/// directory shouldn't take out unrelated state.
fn sweep_old(dir: &Path, retention_days: u32) {
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(
        u64::from(retention_days) * 86_400,
    )) else {
        return;
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "logging::sweep_old: read_dir failed"
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.starts_with(FORGED_LOG_PREFIX) {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            tracing::debug!(
                path = %entry.path().display(),
                "logging::sweep_old: skipping entry without modified time"
            );
            continue;
        };
        if mtime < cutoff {
            // Best-effort delete; log dir cleanup never fails a boot.
            // Log so operators can spot recurring permission/IO issues.
            if let Err(e) = std::fs::remove_file(entry.path()) {
                tracing::warn!(
                    path = %entry.path().display(),
                    error = %e,
                    "logging::sweep_old: remove_file failed"
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{FORGED_LOG_PREFIX, expand_home, sweep_old};
    use std::fs;
    use std::time::{Duration, SystemTime};

    #[test]
    fn expand_home_leaves_absolute_path_untouched() {
        let p = expand_home("/var/log/forged");
        assert_eq!(p, std::path::PathBuf::from("/var/log/forged"));
    }

    #[test]
    fn expand_home_passes_through_when_no_tilde() {
        // No leading `~/` → returned verbatim, regardless of HOME.
        let p = expand_home("relative/path/forged");
        assert_eq!(p, std::path::PathBuf::from("relative/path/forged"));
    }

    #[test]
    fn sweep_old_deletes_only_forged_prefixed_files_past_retention() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        let old = dir.join(format!("{FORGED_LOG_PREFIX}events.log.2020-01-01"));
        let recent = dir.join(format!("{FORGED_LOG_PREFIX}events.log.9999-12-31"));
        let unrelated = dir.join("README.txt");
        fs::write(&old, "old").unwrap();
        fs::write(&recent, "recent").unwrap();
        fs::write(&unrelated, "leave-me-alone").unwrap();
        // Force `old` mtime to ~30 days ago.
        let thirty_days_ago = SystemTime::now() - Duration::from_secs(30 * 86_400);
        let f = fs::File::open(&old).unwrap();
        f.set_modified(thirty_days_ago).unwrap();

        sweep_old(dir, 14);

        assert!(!old.exists(), "old forged log should have been swept");
        assert!(recent.exists(), "recent forged log should remain");
        assert!(
            unrelated.exists(),
            "non-forged-prefixed file must never be touched"
        );
    }
}
