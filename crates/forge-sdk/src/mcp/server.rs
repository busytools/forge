//! The in-process MCP server.
//!
//! Holds a registry of [`Tool`] implementations and dispatches JSON-RPC
//! requests to them. Transport is OUT of scope here - this type is a pure
//! request/response router. The `mcp::orchestration` module wires `dispatch`
//! calls to the `mcp_message` control-request arm of `Client::handle_control`.

use std::collections::BTreeMap;
use std::sync::Arc;

use tracing::{debug, warn};

use crate::mcp::protocol::{
    JsonRpcRequest, JsonRpcResponse, JsonRpcResult, LATEST_PROTOCOL_VERSION, McpError,
    SUPPORTED_PROTOCOL_VERSIONS, ServerInfo, ToolDescription,
};
use crate::mcp::tool::{Tool, ToolInput, ToolOutput};

/// What the client's requested version resolved to. Kept distinct from the
/// answered string because an unsupported version and a missing one are
/// different events: `protocolVersion` is required, so the latter is a broken
/// client rather than a negotiation outcome.
#[derive(Debug, PartialEq, Eq)]
enum Negotiated<'a> {
    /// A version we speak.
    Agreed(&'static str),
    /// A version string we do not speak.
    Unsupported(&'a str),
    /// Absent, or present but not a string.
    Malformed,
}

fn classify_protocol_version(params: Option<&serde_json::Value>) -> Negotiated<'_> {
    let Some(want) =
        params.and_then(|p| p.get("protocolVersion")).and_then(serde_json::Value::as_str)
    else {
        return Negotiated::Malformed;
    };
    SUPPORTED_PROTOCOL_VERSIONS
        .into_iter()
        .find(|v| *v == want)
        .map_or(Negotiated::Unsupported(want), Negotiated::Agreed)
}

/// Echo the client's requested protocol version when we speak it, else answer
/// with our latest - the spec's SHOULD for an unsupported request.
fn negotiate_protocol_version(params: Option<&serde_json::Value>) -> &'static str {
    match classify_protocol_version(params) {
        Negotiated::Agreed(v) => v,
        Negotiated::Unsupported(want) => {
            debug!(requested = %want, "mcp server: unsupported protocol version, answering latest");
            LATEST_PROTOCOL_VERSION
        }
        Negotiated::Malformed => {
            warn!("mcp server: initialize has no string protocolVersion, answering latest");
            LATEST_PROTOCOL_VERSION
        }
    }
}

/// A fully-constructed MCP server. Clone is cheap (just bumps Arcs).
#[derive(Clone)]
pub struct McpServer {
    name: String,
    version: String,
    tools: BTreeMap<String, Arc<dyn Tool>>,
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
    /// Whether any registered tool's name starts with `prefix`, so a
    /// host can introspect which tool group it registered.
    pub fn has_tool_prefix(&self, prefix: &str) -> bool {
        self.tools.keys().any(|name| name.starts_with(prefix))
    }

    /// Dispatch one JSON-RPC request to the appropriate handler.
    ///
    /// Returns `None` for JSON-RPC notifications (no `id`) - notifications
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
                protocol_version: negotiate_protocol_version(req.params.as_ref()).into(),
                capabilities: serde_json::json!({"tools": {"listChanged": false}}),
                server_info: ServerInfo { name: self.name.clone(), version: self.version.clone() },
            }),
            "ping" => Ok(JsonRpcResult::Empty {}),
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
        let name = params.get("name").and_then(|v| v.as_str()).ok_or_else(|| McpError {
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
    tools: BTreeMap<String, Arc<dyn Tool>>,
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
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self { name: name.into(), version: version.into(), tools: BTreeMap::new() }
    }

    /// Register a tool. A duplicate name replaces the earlier registration -
    /// five modules chain onto one builder, so a collision would otherwise
    /// drop a tool with nothing to show for it.
    pub fn tool<T: Tool + 'static>(mut self, tool: T) -> Self {
        let name = tool.name().to_string();
        if self.tools.insert(name.clone(), Arc::new(tool)).is_some() {
            warn!(%name, server = %self.name, "mcp tool registered twice; keeping the later one");
        }
        self
    }

    /// Finalise into a runnable server.
    pub fn build(self) -> McpServer {
        McpServer { name: self.name, version: self.version, tools: self.tools }
    }
}

#[cfg(test)]
mod tests_negotiation {
    use super::{Negotiated, classify_protocol_version};
    use serde_json::json;

    /// Both answer with our latest on the wire, so only the classifier can
    /// tell them apart - an unsupported version is a negotiation outcome, a
    /// missing one is a broken client.
    #[test]
    fn unsupported_and_malformed_are_distinct() {
        let unsupported = json!({"protocolVersion": "1999-01-01"});
        assert_eq!(
            classify_protocol_version(Some(&unsupported)),
            Negotiated::Unsupported("1999-01-01")
        );

        for malformed in [json!({"protocolVersion": 5}), json!({"capabilities": {}}), json!(null)] {
            assert_eq!(
                classify_protocol_version(Some(&malformed)),
                Negotiated::Malformed,
                "params: {malformed}"
            );
        }
        assert_eq!(classify_protocol_version(None), Negotiated::Malformed);
    }

    #[test]
    fn every_supported_version_is_agreed() {
        for v in super::SUPPORTED_PROTOCOL_VERSIONS {
            let params = json!({ "protocolVersion": v });
            assert_eq!(classify_protocol_version(Some(&params)), Negotiated::Agreed(v));
        }
    }
}
