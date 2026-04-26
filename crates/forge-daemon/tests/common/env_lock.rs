//! Process-wide lock for tests that mutate env vars.
//!
//! The env-var space is process-global, so concurrent mutations across
//! parallel-running tests would race. Each test that calls
//! `EnvGuard::new` / `EnvGuard::unset` takes this lock first. Two test
//! files (`m3_listing.rs` and `m6_operations.rs`) used to define their
//! own static `ENV_LOCK`s; that meant tests across files could run
//! concurrently and stomp each other. Promoting to a shared lock makes
//! the serialisation contract whole-process.
//!
//! Usage (sync tests):
//! ```ignore
//! mod common {
//!     pub mod env_guard;
//!     pub mod env_lock;
//! }
//! use crate::common::env_lock::ENV_LOCK;
//!
//! #[test]
//! fn my_test_that_mutates_env() {
//!     let _g = ENV_LOCK.lock();
//!     // ... mutate via EnvGuard ...
//! }
//! ```
//!
//! Usage (async tests): use `lock().await` and the guard returned is
//! safe to hold across `.await` points (clippy's `await_holding_lock`
//! lint targets sync mutex guards, not tokio's).

#![allow(
    dead_code,
    reason = "common test helpers are pulled in per-test; not every test uses every helper"
)]

use std::sync::OnceLock;

/// Process-wide async lock — every test that mutates env vars must
/// take this guard before constructing an `EnvGuard`. Tokio mutex (not
/// `parking_lot`) so async tests can hold the guard across `.await`
/// points without tripping clippy's `await_holding_lock` lint and
/// without risking blocking the executor.
///
/// Sync tests (`#[test]`) use `block_on`-friendly fallback by calling
/// `.blocking_lock()` from a tokio Mutex. Most sync tests in this
/// crate already use `parking_lot::Mutex` directly; the wrapper below
/// preserves the `lock()` ergonomic shape so legacy `let _g = ENV_LOCK.lock();`
/// callers don't need to change.
pub struct EnvLock {
    inner: OnceLock<tokio::sync::Mutex<()>>,
}

impl EnvLock {
    fn mutex(&self) -> &tokio::sync::Mutex<()> {
        self.inner.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    /// Acquire the lock — sync variant. Uses `blocking_lock` so this
    /// works from `#[test]` (no async context). Must NOT be called
    /// from an async context — use [`Self::lock_async`] instead.
    #[must_use = "guard must be bound to a binding to keep the lock held for the test scope"]
    pub fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutex().blocking_lock()
    }

    /// Acquire the lock — async variant. Safe to hold across
    /// `.await` points (tokio mutex, not `std::sync` / `parking_lot`).
    pub async fn lock_async(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutex().lock().await
    }
}

/// Process-wide env-var serialisation lock.
pub static ENV_LOCK: EnvLock = EnvLock {
    inner: OnceLock::new(),
};
