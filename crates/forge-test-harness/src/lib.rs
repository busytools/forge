//! Wire-conformance harness for forge.
//!
//! [`sdk_wire`] — forge-sdk ↔ `claude` CLI stream-json. Replay on
//! every `cargo check` / `just check`; live capture (opt-in via
//! `FORGE_WIRE_CAPTURE=1`) updates baselines under `baselines/sdk/`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod sdk_wire;
