//! Subprocess transport for the `claude` binary.
//!
//! Two layers:
//!
//! - [`codec`] — pure JSON-line encode/decode. Testable without a subprocess.
//! - [`process`] — tokio subprocess lifecycle wrapping the codec.
//!
//! The [`Transport`] trait at the module root is the extensibility
//! seam: implement it to plug in alternative I/O (remote, in-memory,
//! containerised) and pass a boxed instance to
//! [`Client::spawn_with_transport`](crate::Client::spawn_with_transport).
//! The shipped [`process::Subprocess`] is one implementation.

use std::sync::Arc;

use async_trait::async_trait;

use crate::Error;

pub mod codec;
pub mod process;

/// Shareable, `Send + Sync` writer half. Transports that split read
/// from write under the hood (e.g. the shipped
/// [`process::Subprocess`]) hand out an mpsc-backed writer that's safe
/// to clone into `tokio::spawn`'d tasks. Consumers that want the actor
/// pattern (a long-running `next_event` in one task + concurrent
/// commands + detached `handle_control` dispatch in another) need
/// this — the `Transport` trait's `&mut self`-bound `write_line`
/// doesn't allow concurrent writes.
///
/// Default-implementations return `None` from
/// [`Transport::try_clone_writer`]; transports that can be split should
/// override.
#[async_trait]
pub trait AsyncWriter: Send + Sync + std::fmt::Debug {
    /// Write one line of stream-json to the transport. Caller supplies
    /// the trailing `\n` (matches [`Transport::write_line`]).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on write failure or after the transport has
    /// closed its write half.
    async fn write_line(&self, line: &str) -> Result<(), Error>;
}

/// Abstract I/O surface that [`Client`](crate::Client) drives. Mirrors
/// Python SDK's `Transport` abstract base (`_internal/transport/__init__.py`).
///
/// Every method takes `&mut self` so implementors can be owned via
/// `Box<dyn Transport>` — the value lives inside `Client` for the
/// lifetime of the session, and the Rust compiler enforces exclusive
/// access on every call.
///
/// # Implementor notes
///
/// - `read_line` returns `Ok(None)` at EOF. The caller drains in a
///   loop until `None`.
/// - `write_line` must honour the caller's trailing newline — this
///   trait does not add one. Mirrors
///   [`codec::encode_user_prompt`] which already emits `\n`.
/// - `end_input` closes the write half (equivalent to dropping stdin
///   on a subprocess). Subsequent `write_line` calls MUST return an
///   error. Safe to call multiple times.
/// - `close` releases all underlying resources. Idempotent — callers
///   may invoke it multiple times, the second call is a no-op. After
///   `close` both `read_line` and `write_line` return EOF / error.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Read one line from the transport (without the trailing `\n`).
    /// Returns `Ok(None)` at end-of-stream.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on read failure.
    async fn read_line(&mut self) -> Result<Option<String>, Error>;

    /// Write one line of stream-json to the transport. The caller
    /// supplies the trailing `\n`; this trait does not add one.
    /// Mirrors the contract of [`codec::encode_user_prompt`].
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on write failure, including post-`end_input` or
    /// post-`close` writes.
    async fn write_line(&mut self, line: &str) -> Result<(), Error>;

    /// End the input stream — close the write half so the remote
    /// sees EOF. Safe to call multiple times.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on flush failure.
    async fn end_input(&mut self) -> Result<(), Error>;

    /// Release all underlying resources. Idempotent.
    ///
    /// # Errors
    ///
    /// [`Error::Process`] / [`Error::Io`] — implementors surface
    /// shutdown errors to the caller rather than swallowing.
    async fn close(&mut self) -> Result<(), Error>;

    /// Optional shareable writer handle. Transports that split read
    /// from write internally (mpsc-bridged, multi-task) return
    /// `Some(...)`; transports whose write side requires `&mut self`
    /// (the default [`process::Subprocess`]) return `None`.
    ///
    /// When `Some(...)`, the returned [`AsyncWriter`] is safe to clone
    /// into `tokio::spawn`'d tasks. This is what enables the daemon's
    /// actor pattern — see [`crate::Client::try_dispatch_handle`].
    fn try_clone_writer(&self) -> Option<Arc<dyn AsyncWriter>> {
        None
    }
}
