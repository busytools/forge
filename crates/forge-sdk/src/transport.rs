//! Subprocess transport for the `claude` binary.
//!
//! Two layers:
//!
//! - [`codec`]  -  pure JSON-line encode/decode. Testable without a subprocess.
//! - [`process`]  -  tokio subprocess lifecycle wrapping the codec.
//!
//! [`process::Subprocess`] is the only transport. Wire-recording for
//! conformance baselines lives at the [`Options`](crate::Options)
//! level via the `tee_inbound` / `tee_outbound` callbacks  -  there is
//! no public `Transport` trait or `spawn_with_transport` injection
//! seam.

pub mod codec;
pub mod process;
pub mod proxy;
