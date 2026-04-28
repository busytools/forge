//! forge-tui — terminal client for forge-daemon.
//!
//! Connects over WS+JSON-RPC, runs a screen-based UI, and renders
//! `session.event` streams. Each screen (Connecting, Picker,
//! Conversation, Disconnected) owns its own layout and input rules.

pub mod app;
pub mod cache_policy;
pub mod client;
pub mod input;
pub mod logging;
pub mod perf;
pub mod state;
pub mod ui;
