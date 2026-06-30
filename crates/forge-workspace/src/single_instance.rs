//! Single-instance boot guard.
//!
//! forge guarantees one instance per config dir. The binary's startup
//! ([`crate::single_instance::acquire`], called from forge-tui's `main`
//! before [`crate::Workspace::new`]) takes a non-blocking exclusive
//! `flock` on a never-renamed `<config_dir>/forge.lock`. If another
//! forge already holds it, boot refuses with [`AcquireError::AlreadyRunning`]
//! - a clean stderr message + non-zero exit, NOT a panic. Otherwise the
//! caller's PID is written and the lock is held for the process lifetime
//! (flock auto-releases on exit/crash, so there is no stale-lock cleanup).
//!
//! The lock scopes to the config dir, so a second forge on the SAME
//! forge.toml/state is blocked while a genuinely separate profile /
//! config-dir is allowed. One instance per config dir is what lets the
//! rest of forge - the cron store especially - coordinate with just an
//! in-process mutex instead of cross-process locking.
//!
//! Why a dedicated lockfile rather than locking forge.toml /
//! forge-state.toml directly: `flock` binds the open file description
//! (the inode), and those data files are rewritten via tmp + rename,
//! which swaps a fresh inode in and silently breaks a lock held on the
//! old one. `forge.lock` is only ever opened + flocked, never renamed.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::Path;

use rustix::fs::{FlockOperation, flock};

const LOCK_FILE_RELATIVE_PATH: &str = "forge.lock";

/// Why [`acquire`] refused.
#[derive(Debug)]
pub(crate) enum AcquireError {
    /// A live forge already owns this config dir. `pid` is the holder's
    /// PID read from `forge.lock` (`None` if the file was empty or
    /// couldn't be parsed).
    AlreadyRunning { pid: Option<u32> },
}

/// Acquire the per-config-dir single-instance lock.
///
/// On success the returned `File` MUST be held for the process lifetime
/// - dropping it (or process exit / crash) releases the flock, so there
/// is no stale-lock cleanup. `Ok(None)` is the degraded path: the
/// lockfile couldn't be opened or the filesystem rejected `flock`, so
/// forge boots without the guarantee rather than refusing over an exotic
/// FS (the local APFS/ext4 the user runs always supports flock). `Err`
/// means another forge instance already owns this config dir.
pub(crate) fn acquire(config_dir: &Path) -> Result<Option<File>, AcquireError> {
    let path = config_dir.join(LOCK_FILE_RELATIVE_PATH);
    let mut file =
        match OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    target: "forge_workspace::single_instance",
                    error = %e,
                    path = %path.display(),
                    "forge.lock open failed; single-instance guard skipped",
                );
                return Ok(None);
            }
        };
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            record_pid(&mut file);
            Ok(Some(file))
        }
        // EWOULDBLOCK / EAGAIN (same value on Unix) is the contended
        // case: another process holds the exclusive lock.
        Err(e) if e == rustix::io::Errno::WOULDBLOCK || e == rustix::io::Errno::AGAIN => {
            Err(AcquireError::AlreadyRunning { pid: read_pid(&mut file) })
        }
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::single_instance",
                error = %e,
                "flock on forge.lock failed; single-instance guard skipped",
            );
            Ok(None)
        }
    }
}

/// Stamp our PID into the (now exclusively-held) lockfile so a would-be
/// second instance can name us in its refusal. Best-effort: a failed
/// write doesn't void the lock - the flock is what enforces
/// single-instance; the PID is only for the message.
fn record_pid(file: &mut File) {
    let pid = std::process::id();
    let _ = file.set_len(0);
    let _ = file.rewind();
    let _ = file.write_all(pid.to_string().as_bytes());
    let _ = file.flush();
}

/// Read the holder's PID from the lockfile. `None` when empty or
/// unparseable.
fn read_pid(file: &mut File) -> Option<u32> {
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acquire_succeeds_on_fresh_dir_and_records_pid() {
        let dir = tempdir().expect("tempdir");
        let lock = acquire(dir.path()).expect("acquire ok");
        assert!(lock.is_some(), "a fresh config dir acquires the lock");

        let contents =
            std::fs::read_to_string(dir.path().join(LOCK_FILE_RELATIVE_PATH)).expect("read lock");
        assert_eq!(
            contents.trim().parse::<u32>().expect("pid parses"),
            std::process::id(),
            "the lockfile records our PID",
        );
    }

    #[test]
    fn second_acquire_reports_already_running_with_pid() {
        let dir = tempdir().expect("tempdir");
        let _first = acquire(dir.path()).expect("first ok").expect("first holds the lock");

        match acquire(dir.path()) {
            Err(AcquireError::AlreadyRunning { pid }) => {
                assert_eq!(pid, Some(std::process::id()), "refusal names the holder's PID");
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        // `_first` stays held across the second acquire and releases at
        // end of scope.
    }

    #[test]
    fn lock_releases_on_drop_allowing_reacquire() {
        let dir = tempdir().expect("tempdir");
        {
            let _first = acquire(dir.path()).expect("first ok").expect("first holds the lock");
        }
        let second = acquire(dir.path()).expect("second ok");
        assert!(second.is_some(), "the lock is reacquirable after the holder drops it");
    }
}
