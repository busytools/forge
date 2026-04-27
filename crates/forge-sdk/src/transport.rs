//! Subprocess transport for the `claude` binary.
//!
//! Two layers:
//!
//! - [`codec`] — pure JSON-line encode/decode. Testable without a subprocess.
//! - [`process`] — tokio subprocess lifecycle wrapping the codec.
//!
//! [`process::Subprocess`] is the only transport. Wire-recording for
//! conformance baselines lives at the [`Options`](crate::Options)
//! level via the `tee_inbound` / `tee_outbound` callbacks — there is
//! no public `Transport` trait or `spawn_with_transport` injection
//! seam.

use async_trait::async_trait;

use crate::Error;

pub mod codec;
pub mod process;

/// Shareable, `Send + Sync` writer half. The shipped
/// [`process::Subprocess`] hands one out internally so the SDK
/// runtime can `tokio::spawn` detached control-request dispatch on
/// a cloned writer without contending on `&mut self`.
///
/// Internal trait — there is one implementor and consumers should not
/// implement their own.
#[async_trait]
pub(crate) trait AsyncWriter: Send + Sync + std::fmt::Debug {
    /// Write one line of stream-json to the transport. Caller supplies
    /// the trailing `\n`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on write failure or after the transport has
    /// closed its write half.
    async fn write_line(&self, line: &str) -> Result<(), Error>;

    /// Close the write half so the remote sees EOF on stdin. Idempotent.
    /// After this call, [`write_line`](Self::write_line) MUST return an
    /// error.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on flush failure.
    async fn end_input(&self) -> Result<(), Error>;
}
