//! Re-exports of shared type definitions from `forge-primitives`.
//!
//! All type definitions previously lived here directly. The 2026-05-05
//! restructure (phase 2) moved them to the new `forge-primitives` crate
//! so forge-agent (incoming in phase 3) and any future consumer can
//! reach them without depending on forge-tui.
//!
//! This module is kept as a re-export shim so existing call sites
//! (`crate::agent::types::ModeState`, etc.) continue to resolve.

pub use forge_primitives::*;
