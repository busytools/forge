//! Single-instance boot guard.
//!
//! forge guarantees one instance per config dir. [`crate::Workspace::new`]
//! calls [`acquire`] at startup, taking a non-blocking exclusive `flock`
//! on a never-renamed lockfile. If another forge already holds it, boot
//! refuses with an [`AcquireError::AlreadyRunning`]: a clean stderr
//! message + non-zero exit, not a panic. Otherwise the caller's PID is
//! written and the lock is held for the process lifetime (flock
//! auto-releases on exit/crash, so there is no stale-lock cleanup). One
//! instance per config dir is what lets the rest of forge - the cron
//! store especially - coordinate with just an in-process mutex instead
//! of cross-process locking.
//!
//! The lockfile is MACHINE-LOCAL: it lives under forge's app-support base
//! ([`forge_sdk::app_support_dir`], beside the logs) at
//! `locks/<hash>.lock`, where `<hash>` is derived from the config-dir
//! path so distinct profiles / config dirs on one machine get distinct,
//! independently-held locks.
//!
//! Why there and not beside the other forge state in `<config_dir>/forge/`:
//! `flock` binds the open file description (the inode), not the path, and
//! rewriting a file by renaming a temp over it swaps the inode. forge's
//! own data files (forge.toml / cron.toml) are rewritten that way, so the
//! lock must be a dedicated file that is only ever opened + flocked, never
//! renamed; and the config dir is Syncthing-synced across the user's Macs,
//! where Syncthing also applies incoming changes by rename - a lock synced
//! in from another Mac would swap the inode out from under a running forge,
//! orphaning its flock and letting a second instance start. A machine-local
//! lock outside the synced dir sidesteps both.
//!
//! A v0.18.0 boot may have left a stray `<config_dir>/forge/forge.lock`; it
//! is now unused and harmless (a fresh boot writes the machine-local lock
//! instead), so there is no migration.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

/// Subdirectory of the app-support base that holds the per-config-dir
/// lockfiles.
const LOCK_DIR_NAME: &str = "locks";

/// Why [`acquire`] refused.
#[derive(Debug)]
pub(crate) enum AcquireError {
    /// A live forge already owns this config dir. `pid` is the holder's
    /// PID read from the lockfile (`None` if the file was empty or
    /// couldn't be parsed).
    AlreadyRunning { pid: Option<u32> },
}

/// Machine-local lock path for `config_dir` under `app_support`:
/// `<app_support>/locks/<hex-hash-of-config-dir>.lock`. The config-dir
/// path is canonicalised first (best-effort) so symlinked / trailing-slash
/// variants resolve to one lock.
fn lock_path(config_dir: &Path, app_support: &Path) -> PathBuf {
    app_support.join(LOCK_DIR_NAME).join(format!("{}.lock", forge_sdk::config_dir_hash(config_dir)))
}

/// Acquire the per-config-dir single-instance lock under `app_support`.
///
/// The caller resolves the base, so a test can pass a tempdir and never
/// write to the real app-support directory. In production that base is
/// [`forge_sdk::app_support_dir`], which keeps the lock out of the
/// Syncthing-synced config dir (see the module docs).
///
/// On success the returned `File` MUST be held for the process lifetime;
/// dropping it (or process exit / crash) releases the flock, so there is
/// no stale-lock cleanup. `Ok(None)` is the degraded path: the lock dir
/// couldn't be created, the lockfile couldn't be opened, or the
/// filesystem rejected `flock`, so forge boots without the guarantee
/// rather than refusing over an exotic FS (the local APFS/ext4 the user
/// runs always supports flock). `Err` means another forge instance
/// already owns this config dir.
pub(crate) fn acquire(config_dir: &Path, app_support: &Path) -> Result<Option<File>, AcquireError> {
    let path = lock_path(config_dir, app_support);
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            target: "forge_workspace::single_instance",
            error = %e,
            path = %parent.display(),
            "lock dir create failed; single-instance guard skipped",
        );
        return Ok(None);
    }
    let mut file =
        match OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    target: "forge_workspace::single_instance",
                    error = %e,
                    path = %path.display(),
                    "lockfile open failed; single-instance guard skipped",
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
                "flock on lockfile failed; single-instance guard skipped",
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

    /// A tempdir standing in for the machine-local app-support base, so
    /// tests never write to the real `~/Library/Application Support`.
    fn base() -> tempfile::TempDir {
        tempdir().expect("tempdir")
    }

    #[test]
    fn lock_lives_under_locks_subdir_of_the_base() {
        let cfg = tempdir().expect("cfg");
        let base = base();
        let _lock = acquire(cfg.path(), base.path()).expect("acquire ok").expect("holds the lock");
        let path = lock_path(cfg.path(), base.path());
        assert!(path.is_file(), "lock is a real file");
        assert_eq!(
            path.parent().and_then(Path::file_name).and_then(|s| s.to_str()),
            Some("locks"),
            "lock lives under <base>/locks/",
        );
    }

    #[test]
    fn distinct_config_dirs_get_distinct_locks_held_at_once() {
        let base = base();
        let cfg_a = tempdir().expect("cfg a");
        let cfg_b = tempdir().expect("cfg b");
        let a = acquire(cfg_a.path(), base.path()).expect("a ok").expect("a holds");
        let b = acquire(cfg_b.path(), base.path()).expect("b ok").expect("b holds");
        assert_ne!(
            lock_path(cfg_a.path(), base.path()),
            lock_path(cfg_b.path(), base.path()),
            "different config dirs hash to different lock files",
        );
        // Both locks are held simultaneously under the same base -
        // one config dir's lock never contends with another's.
        drop((a, b));
    }

    #[test]
    fn acquire_succeeds_on_fresh_dir_and_records_pid() {
        let cfg = tempdir().expect("cfg");
        let base = base();
        let lock = acquire(cfg.path(), base.path()).expect("acquire ok");
        assert!(lock.is_some(), "a fresh config dir acquires the lock");

        let contents =
            std::fs::read_to_string(lock_path(cfg.path(), base.path())).expect("read lock");
        assert_eq!(
            contents.trim().parse::<u32>().expect("pid parses"),
            std::process::id(),
            "the lockfile records our PID",
        );
    }

    #[test]
    fn second_acquire_reports_already_running_with_pid() {
        let cfg = tempdir().expect("cfg");
        let base = base();
        let _first = acquire(cfg.path(), base.path()).expect("first ok").expect("first holds");

        match acquire(cfg.path(), base.path()) {
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
        let cfg = tempdir().expect("cfg");
        let base = base();
        {
            let _first = acquire(cfg.path(), base.path()).expect("first ok").expect("first holds");
        }
        let second = acquire(cfg.path(), base.path()).expect("second ok");
        assert!(second.is_some(), "the lock is reacquirable after the holder drops it");
    }
}
