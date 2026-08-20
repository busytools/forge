//! In-process MCP server hosting.
//!
//! Exposes Rust functions as MCP tools that the `claude` binary can call
//! during agentic work, following the SDK's `create_sdk_mcp_server` and
//! `@tool` decorator model. The wire details were observed from the CLI
//! rather than specified.

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
