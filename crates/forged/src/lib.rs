//! forged — daemon wrapping forge-sdk over a JSON-RPC 2.0 WebSocket wire.
//!
//! See `~/.claude-subspace/plans/2026-04-25-forged-wire-spec.md` for the
//! authoritative protocol definition.

pub use error::Error;

pub mod error;
