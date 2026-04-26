//! Wire-conformance harness for forge.
//!
//! Two scopes, kept as sibling submodules so an SDK regression and a
//! daemon regression don't get tangled:
//!
//! - [`sdk_wire`] — forge-sdk ↔ `claude` CLI stream-json. Replay
//!   on every `cargo check` / `just check`; live capture (opt-in via
//!   `FORGE_WIRE_CAPTURE=1`) updates baselines under `baselines/sdk/`.
//! - [`daemon_wire`] — forge-daemon ↔ client JSON-RPC over WS. Replay
//!   plus a single live `scenarios_basic` covering the M1–M5 round
//!   trips; baselines live under `baselines/daemon/`.
//!
//! The two modules share nothing beyond the workspace dev-deps, so
//! consumers depend on whichever scope they're testing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod daemon_wire;
pub mod sdk_wire;
