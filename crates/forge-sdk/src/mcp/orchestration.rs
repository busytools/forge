//! Glue between `McpServer` instances and the `claude` binary.
//!
//! Semantics:
//! - Keeps a `HashMap<server_name, McpServer>` for dispatch.
//! - Produces the `--mcp-config` inline JSON argv for subprocess spawn.
//! - Routes `mcp_message` `control_request`s to the right server.
//!
//! No transport ownership: servers are in-process, dispatch is a direct
//! method call.

use std::collections::HashMap;

use crate::mcp::McpServer;
use crate::mcp::protocol::{JsonRpcRequest, JsonRpcResponse};

/// Registry of in-process MCP servers, keyed by the name under which the
/// caller registered them via `OptionsBuilder::mcp_server(...)`.
#[derive(Clone, Debug, Default)]
pub(crate) struct McpHosts {
    servers: HashMap<String, McpServer>,
}

impl McpHosts {
    /// Build from an `Options.mcp_servers` vector.
    pub(crate) fn new(entries: Vec<(String, McpServer)>) -> Self {
        let mut servers = HashMap::new();
        for (name, server) in entries {
            servers.insert(name, server);
        }
        Self { servers }
    }

    /// Build the `--mcp-config` inline JSON argument. Returns an empty
    /// string when no servers are registered (caller should skip adding
    /// the flag entirely in that case).
    pub(crate) fn config_argv(&self) -> String {
        let servers: serde_json::Map<String, serde_json::Value> = self
            .servers
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    serde_json::json!({"type": "sdk", "name": name}),
                )
            })
            .collect();
        serde_json::json!({"mcpServers": servers}).to_string()
    }

    /// Dispatch an incoming `mcp_message` to the right server. Returns
    /// `None` for JSON-RPC notifications (no id) or for unknown server
    /// names (error response built by caller).
    pub(crate) async fn dispatch(
        &self,
        server_name: &str,
        req: &JsonRpcRequest,
    ) -> Option<JsonRpcResponse> {
        self.servers.get(server_name)?.dispatch(req).await
    }

    /// True if a server with this name is registered.
    pub(crate) fn has(&self, server_name: &str) -> bool {
        self.servers.contains_key(server_name)
    }
}
