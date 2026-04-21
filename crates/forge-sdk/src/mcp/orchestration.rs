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
use crate::public_types::McpServerConfig;

/// Registry of MCP servers (both in-process SDK and external stdio /
/// SSE / HTTP). Keyed by the name under which the caller registered
/// them via `OptionsBuilder::mcp_server` / `external_mcp_server`.
#[derive(Clone, Debug, Default)]
pub(crate) struct McpHosts {
    servers: HashMap<String, McpServer>,
    external: HashMap<String, McpServerConfig>,
}

impl McpHosts {
    /// Build from the `Options.mcp_servers` vector of in-process servers
    /// plus a `HashMap` of external configs. Either side may be empty.
    pub(crate) fn new(
        sdk_entries: Vec<(String, McpServer)>,
        external: HashMap<String, McpServerConfig>,
    ) -> Self {
        let mut servers = HashMap::new();
        for (name, server) in sdk_entries {
            servers.insert(name, server);
        }
        Self { servers, external }
    }

    /// True when no servers of either flavour are registered.
    pub(crate) fn is_empty(&self) -> bool {
        self.servers.is_empty() && self.external.is_empty()
    }

    /// Build the `--mcp-config` inline JSON argument. SDK servers emit
    /// `{"type":"sdk","name":<n>}`; external servers serialise via
    /// [`McpServerConfig`]'s own serde impl. Returns `"{"mcpServers":{}}"`
    /// when empty — caller should skip adding the flag in that case.
    pub(crate) fn config_argv(&self) -> String {
        let mut servers: serde_json::Map<String, serde_json::Value> = self
            .servers
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    serde_json::json!({"type": "sdk", "name": name}),
                )
            })
            .collect();
        for (name, cfg) in &self.external {
            if let Ok(v) = serde_json::to_value(cfg) {
                servers.insert(name.clone(), v);
            }
        }
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
