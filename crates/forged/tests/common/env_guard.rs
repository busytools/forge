//! RAII guard for env-var mutations in integration tests.
//!
//! Snapshots the prior value on construction, applies the new value,
//! and restores the original on drop. Survives test panics — Drop
//! runs even when the test fn unwinds — so a panicking test that
//! mutated an env var doesn't leak into every subsequent test in the
//! same process.
//!
//! Tests that mutate the same env var must serialise around a shared
//! mutex (the env-var space is process-global) — this guard does NOT
//! itself serialise; that's the caller's job.

#![allow(
    dead_code,
    reason = "common test helpers are pulled in per-test; not every test uses every helper"
)]
#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};

/// RAII guard that snapshots an env var on construction, applies the
/// new value, and restores the original on drop.
pub struct EnvGuard {
    key: &'static str,
    prior: Option<OsString>,
}

impl EnvGuard {
    /// Set `key=value` for the lifetime of the guard. The prior value
    /// is restored on drop.
    pub fn new(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let prior = std::env::var_os(key);
        // SAFETY: callers serialise mutation around a process-wide
        // lock; the guard restores the original value on drop even if
        // the test panics.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, prior }
    }

    /// Unset `key` for the lifetime of the guard. The prior value is
    /// restored on drop.
    pub fn unset(key: &'static str) -> Self {
        let prior = std::env::var_os(key);
        // SAFETY: see `new`.
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: serialised access via the test's ENV_LOCK.
        unsafe {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
