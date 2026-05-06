//! Wire-conformance harness for forge.
//!
//! [`sdk_wire`] — forge-sdk ↔ `claude` CLI stream-json. Replay on
//! every `cargo check` / `just check`; live capture (opt-in via
//! `FORGE_WIRE_CAPTURE=1`) updates baselines under `baselines/sdk/`.

// This crate IS test infrastructure. `panic` / `expect` / `unwrap`
// are the right error-reporting paths for harness setup failures
// (missing baselines, regex compile, tempdir creation) — there's no
// recovery path beyond aborting the test run.
#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

pub mod sdk_wire;
