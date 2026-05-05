//! Translation helpers used by callers that need to convert wire-shape
//! data (forge-sdk message payloads, control responses) into typed
//! Rust values. forge-tui consumes these from its event-loop
//! translator paths.

pub mod agents;
pub mod error_handling;
pub mod state_parsing;
