//! Subprocess transport for the `claude` binary.
//!
//! Two layers:
//!
//! - [`codec`] — pure JSON-line encode/decode. Testable without a subprocess.
//! - [`process`] — tokio subprocess lifecycle wrapping the codec. (Added in
//!   the next task.)

pub mod codec;
