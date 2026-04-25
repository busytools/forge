//! JSON-RPC method handlers. Each module hosts the handlers for one
//! method namespace (`daemon.*`, `session.*`, `sessions.*`, …).

pub mod context;
pub mod daemon;
pub mod mcp;
pub mod session;
pub mod sessions;
