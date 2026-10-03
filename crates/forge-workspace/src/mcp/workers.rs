//! The in-project engine the `agents__*` family drives: the workers
//! facade, its shared types, and the spawn / despawn / update lifecycle
//! behind them.
//!
//! Named for the family it grew out of. The tool surface itself is
//! [`crate::mcp::agents`].

pub mod facade;
pub mod types;
