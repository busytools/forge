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
pub mod app;
pub mod block_cache;
pub mod cache_metrics;
pub mod clipboard_image;
pub mod dialog;
pub mod file_index;
pub mod focus;
pub mod git_context;
pub mod inline_interactions;
pub mod input;
pub mod keys;
pub mod mention;
pub mod messages;
pub mod model;
pub mod notify;
pub mod paste_burst;
pub mod permissions;
pub mod questions;
pub mod subagent;
pub mod tool_call_info;
pub mod types;
pub mod view;
pub mod viewport;

pub use viewport::{LayoutInvalidation, LayoutRemeasureReason};
