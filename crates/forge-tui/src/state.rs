//! Lifted state types from claude-code-rust.
//!
//! Phase 2 brings the data model: `ChatMessage` / `MessageBlock` /
//! `ToolCallInfo` (chat content), `BlockCache` / `MessageRenderCache`
//! (per-block render cache), `SessionUpdate` and friends (the wire
//! event surface), plus the wire types for MCP / elicitation / mode
//! info.
//!
//! No `App` struct here yet — that lands in Phase 3 once the chat /
//! footer / picker UI are wired.

pub mod agent_types;
pub mod block_cache;
pub mod cache_metrics;
pub mod dialog;
pub mod focus;
pub mod git_context;
pub mod messages;
pub mod model;
pub mod paste_burst;
pub mod tool_call_info;
pub mod types;
pub mod viewport;

pub use viewport::{LayoutInvalidation, LayoutRemeasureReason};
