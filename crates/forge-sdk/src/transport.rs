//! Subprocess transport for the `claude` binary.
//!
//! Two layers:
//!
//! - [`codec`] — pure JSON-line encode/decode. Testable without a subprocess.
//! - [`process`] — tokio subprocess lifecycle wrapping the codec.

pub mod codec;
pub mod process;
