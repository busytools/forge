//! The in-process MCP server.
//!
//! Holds a registry of [`Tool`] implementations and dispatches JSON-RPC
//! requests to them. Transport is OUT of scope here — this type is a pure
//! request/response router. The `mcp::orchestration` module wires `dispatch`
//! calls to the `mcp_message` control-request arm of `Client::handle_control`.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::debug;

use crate::mcp::protocol::{
    JsonRpcRequest, JsonRpcResponse, JsonRpcResult, McpError, ServerInfo, ToolDescription,
};
use crate::mcp::tool::{Tool, ToolInput, ToolOutput};

/// A fully-constructed MCP server. Clone is cheap (just bumps Arcs).
#[derive(Clone)]
pub struct McpServer {
    name: String,
    version: String,
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl McpServer {
    /// Server name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Registered tool names. Iteration order is not guaranteed.
    #[must_use]
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Dispatch one JSON-RPC request to the appropriate handler.
    ///
    /// Returns `None` for JSON-RPC notifications (no `id`) — notifications
    /// must not elicit a response per the spec. Returns `Some(response)` for
    /// every id-bearing request, success or error.
    ///
    /// This method is pure (modulo tool side-effects): no I/O, no
    /// timeouts, no mutation of `self`. Thread-safe; cheap to call
    /// concurrently.
    pub async fn dispatch(&self, req: &JsonRpcRequest) -> Option<JsonRpcResponse> {
        let Some(id) = req.id.clone() else {
            debug!(method = %req.method, "mcp server: notification ignored");
            return None;
        };
        let result: Result<JsonRpcResult, McpError> = match req.method.as_str() {
            "initialize" => Ok(JsonRpcResult::Initialize {
                protocol_version: "2024-11-05".into(),
                capabilities: serde_json::json!({"tools": {"listChanged": false}}),
                server_info: ServerInfo {
                    name: self.name.clone(),
                    version: self.version.clone(),
                },
            }),
            "notifications/initialized" => {
                return None;
            }
            "tools/list" => {
                let tools: Vec<ToolDescription> = self
                    .tools
                    .values()
                    .map(|t| ToolDescription {
                        name: t.name().into(),
                        description: t.description().into(),
                        input_schema: t.input_schema(),
                    })
                    .collect();
                Ok(JsonRpcResult::ToolsList { tools })
            }
            "tools/call" => match self.call_tool(req.params.as_ref()).await {
                Ok(output) => Ok(JsonRpcResult::ToolsCall {
                    content: output.to_mcp_content(),
                    is_error: output.is_error,
                }),
                Err(err) => Err(err),
            },
            other => Err(McpError {
                code: -32601,
                message: format!("method not found: {other}"),
                data: None,
            }),
        };

        Some(match result {
            Ok(r) => JsonRpcResponse::success(id, r),
            Err(e) => JsonRpcResponse::error(id, e),
        })
    }

    async fn call_tool(&self, params: Option<&serde_json::Value>) -> Result<ToolOutput, McpError> {
        let params = params.ok_or_else(|| McpError {
            code: -32602,
            message: "tools/call requires params".into(),
            data: None,
        })?;
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| McpError {
                code: -32602,
                message: "tools/call params.name missing".into(),
                data: None,
            })?;
        let tool = self.tools.get(name).ok_or_else(|| McpError {
            code: -32602,
            message: format!("unknown tool: {name}"),
            data: None,
        })?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        let input = ToolInput { value: args };
        Ok(tool.call(input).await)
    }
}

/// Builder for [`McpServer`].
pub struct McpServerBuilder {
    name: String,
    version: String,
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for McpServerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerBuilder")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl McpServerBuilder {
    /// Start a new builder.
    #[must_use]
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            tools: HashMap::new(),
        }
    }

    /// Register a tool.
    #[must_use]
    pub fn tool<T: Tool + 'static>(mut self, tool: T) -> Self {
        let name = tool.name().to_string();
        self.tools.insert(name, Arc::new(tool));
        self
    }

    /// Finalise into a runnable server.
    #[must_use]
    pub fn build(self) -> McpServer {
        McpServer {
            name: self.name,
            version: self.version,
            tools: self.tools,
        }
    }
}
