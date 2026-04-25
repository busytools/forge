//! forged — daemon wrapping forge-sdk over a JSON-RPC 2.0 WebSocket wire.
//!
//! See `~/.claude-stargate/plans/2026-04-25-forged-wire-spec.md` for the
//! authoritative protocol definition.

pub use error::Error;

pub mod bridged_transport;
pub mod broadcast;
pub mod connection;
pub mod error;
pub mod iso8601;
pub mod jsonrpc;
pub mod methods;
pub mod prompt_queue;
pub mod registry;
pub mod reverse_rpc;
pub mod sdk_callbacks;
pub mod server;
pub mod session_state;
pub mod status_cli;
