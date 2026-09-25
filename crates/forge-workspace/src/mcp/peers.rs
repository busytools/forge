//! The cross-project engine the `agents__*` family drives: the narrow
//! workspace-state surface its cross-project calls need, plus the wire
//! types it shares with the in-project engine.
//!
//! Named for the family it grew out of. The tool surface itself is
//! [`crate::mcp::agents`].

pub mod facade;
pub mod types;
