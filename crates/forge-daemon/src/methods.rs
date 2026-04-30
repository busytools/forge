//! JSON-RPC method handlers. Each module hosts the handlers for one
//! method namespace (`daemon.*`, `session.*`, `sessions.*`, …).

pub mod context;
pub mod daemon;
pub mod mcp;
pub mod multi_client;
pub mod plugins;
pub mod prompts;
pub mod session;
pub mod sessions;
