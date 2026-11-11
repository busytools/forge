//! The `Tool` trait and its I/O types.
//!
//! The output shape lives in `forge-mcp` because a tool family owned by
//! another crate produces it too; it is re-exported here so every host-side
//! import keeps its path.

use serde_json::Value;

pub use forge_mcp::{ToolOutput, ToolOutputBlock};

/// Structured input handed to a tool's `call`.
#[derive(Debug, Clone)]
pub struct ToolInput {
    /// The arguments object, exactly as the client sent it.
    pub value: Value,
}

/// A registered MCP tool. Implementations describe themselves (name,
/// description, schema) and implement the async `call` method that
/// produces an output for the given input.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Tool name. Exposed to the model as `mcp__<server>__<tool>`.
    fn name(&self) -> &str;

    /// One-line description.
    fn description(&self) -> &str;

    /// JSON Schema describing the tool's arguments.
    fn input_schema(&self) -> Value;

    /// Execute the tool.
    async fn call(&self, input: ToolInput) -> ToolOutput;
}
