//! In-process MCP server hosting.
//!
//! Exposes Rust functions as MCP tools that the `claude` binary can call
//! during agentic work. SDK's `create_sdk_mcp_server`
//! + `@tool` decorator model.
//!
//! See `docs/protocol-notes.md` for observed wire details.

#[macro_use]
pub mod macros;
pub(crate) mod orchestration;
pub mod protocol;
pub mod server;
pub mod tool;

pub use protocol::{
    JsonRpcRequest, JsonRpcResponse, JsonRpcResult, McpError, ServerInfo, ToolDescription,
};
pub use server::{McpServer, McpServerBuilder};
pub use tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};
