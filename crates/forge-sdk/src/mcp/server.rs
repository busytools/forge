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
    instructions: Option<String>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("instructions", &self.instructions.is_some())
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

    /// The names this server carries, in the order `tools/list` answers
    /// them, so a host can record the surface it hands a child.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Every registered tool's name and input schema, so a test can pin the
    /// surface a family advertises. Test-only.
    #[cfg(any(test, feature = "testing"))]
    pub fn tool_schemas(&self) -> Vec<(String, serde_json::Value)> {
        self.tools.iter().map(|(name, tool)| (name.clone(), tool.input_schema())).collect()
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
                instructions: self.instructions.clone(),
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
    instructions: Option<String>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// Families whose tools are registered at [`Self::build`]: a provider
    /// answers for its own names, so the host keeps no table mapping a name
    /// back to the family that owns it.
    providers: Vec<Arc<dyn forge_mcp::ToolProvider>>,
}

impl std::fmt::Debug for McpServerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerBuilder")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("instructions", &self.instructions.is_some())
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("providers", &self.providers.len())
            .finish()
    }
}

impl McpServerBuilder {
    /// Start a new builder.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            instructions: None,
            tools: BTreeMap::new(),
            providers: Vec::new(),
        }
    }

    /// The text the client delivers to the model once at session start:
    /// how to use this server, in the protocol's own `instructions` field.
    pub fn instructions(mut self, text: &str) -> Self {
        self.instructions = Some(text.to_owned());
        self
    }

    /// Register a tool. A duplicate name replaces the earlier registration -
    /// five modules chain onto one builder, so a collision would otherwise
    /// drop a tool with nothing to show for it.
    pub fn tool<T: Tool + 'static>(mut self, tool: T) -> Self {
        let name = tool.name().to_string();
        self.insert(&name, Arc::new(tool));
        self
    }

    /// Register a tool family another crate owns. The family names its own
    /// tools and answers their calls; a name it does not claim reaches the
    /// next provider, or the host's unknown-tool answer.
    pub fn provider(mut self, provider: Arc<dyn forge_mcp::ToolProvider>) -> Self {
        self.providers.push(provider);
        self
    }

    fn insert(&mut self, name: &str, tool: Arc<dyn Tool>) {
        if self.tools.insert(name.to_owned(), tool).is_some() {
            warn!(name, server = %self.name, "mcp tool registered twice; keeping the later one");
        }
    }

    /// Finalise into a runnable server.
    pub fn build(mut self) -> McpServer {
        let providers = Arc::new(forge_mcp::Providers::new(std::mem::take(&mut self.providers)));
        for described in providers.tools() {
            let tool = ProviderTool {
                providers: Arc::clone(&providers),
                name: described.name.clone(),
                description: described.description,
                schema: described.schema,
            };
            self.insert(&described.name, Arc::new(tool));
        }
        McpServer {
            name: self.name,
            version: self.version,
            instructions: self.instructions,
            tools: self.tools,
        }
    }
}

/// One tool a registered family described: the host advertises what the
/// family said and forwards the call to the composition, which asks the
/// family that owns the name.
struct ProviderTool {
    providers: Arc<forge_mcp::Providers>,
    name: String,
    description: String,
    schema: serde_json::Value,
}

#[async_trait::async_trait]
impl Tool for ProviderTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        match self.providers.call(&self.name, input.value).await {
            Some(Ok(output)) => output,
            Some(Err(message)) => ToolOutput::error(message),
            // The name was advertised from this composition, so a provider
            // that no longer claims it is a family whose tools() and call()
            // disagree; answer loudly rather than as a silent empty result.
            None => {
                ToolOutput::error(format!("no registered provider claims the `{}` tool", self.name))
            }
        }
    }
}

#[cfg(test)]
mod tests_initialize {
    use super::{JsonRpcRequest, McpServerBuilder};
    use serde_json::json;

    async fn initialize_result(server: &super::McpServer) -> serde_json::Value {
        let request: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" },
        }))
        .expect("an initialize request");
        let response = server.dispatch(&request).await.expect("an id-bearing request is answered");
        serde_json::to_value(response.result.expect("initialize succeeds")).expect("serializes")
    }

    #[tokio::test]
    async fn an_initialize_result_carries_the_servers_instructions_when_set() {
        let server =
            McpServerBuilder::new("forge", "1.2.3").instructions("claim before working").build();
        let result = initialize_result(&server).await;
        assert_eq!(result["instructions"], "claim before working");
    }

    /// A server with none omits the field, so a client that never learned
    /// it sees exactly the old shape.
    #[tokio::test]
    async fn no_instructions_leaves_the_field_out_of_the_json() {
        let server = McpServerBuilder::new("forge", "1.2.3").build();
        let result = initialize_result(&server).await;
        assert!(result.get("instructions").is_none(), "absent, not null: {result}");
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

#[cfg(test)]
mod tests_tool_names {
    use super::{JsonRpcRequest, McpServer, McpServerBuilder};
    use crate::mcp::tool::{Tool, ToolInput, ToolOutput};
    use serde_json::{Value, json};
    use std::sync::Arc;

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl Tool for Named {
        fn name(&self) -> &'static str {
            self.0
        }

        fn description(&self) -> &'static str {
            "a tool the record test registers"
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {}, "additionalProperties": false})
        }

        async fn call(&self, _input: ToolInput) -> ToolOutput {
            ToolOutput::text("ok")
        }
    }

    /// **The record of a surface is the surface itself.** A spawn logs
    /// `tool_names()` as the list it handed its child, so a name the record
    /// drops - or invents - would answer a later "tool not found" wrongly.
    /// The sorting is pinned too: it is the order `tools/list` answers in.
    #[tokio::test]
    async fn the_recorded_names_are_exactly_what_tools_list_answers() {
        let server = McpServerBuilder::new("forge", "1.2.3")
            .tool(Named("tasks__list"))
            .tool(Named("browser_form_state"))
            .tool(Named("agents__list"))
            .build();

        let request: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        }))
        .expect("a tools/list request");
        let response = server.dispatch(&request).await.expect("an id-bearing request is answered");
        let result = serde_json::to_value(response.result.expect("tools/list succeeds"))
            .expect("serializes");
        let listed: Vec<String> = result["tools"]
            .as_array()
            .expect("a tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("a tool name").to_owned())
            .collect();

        assert_eq!(
            server.tool_names(),
            listed,
            "what a spawn records is exactly the set tools/list answers, in its order",
        );
    }

    /// One family registered through the provider seam: its tools are
    /// advertised from `tools()` and a call is dispatched back to `call()`.
    struct Family {
        name: &'static str,
        answer: Result<&'static str, &'static str>,
        claims: bool,
    }

    impl forge_mcp::ToolProvider for Family {
        fn tools(&self) -> Vec<forge_mcp::ToolSpec> {
            vec![forge_mcp::ToolSpec {
                name: self.name.to_owned(),
                description: "a provider-owned tool".to_owned(),
                schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            }]
        }

        fn call<'a>(
            &'a self,
            name: &'a str,
            _input: Value,
        ) -> forge_mcp::ProviderFuture<'a, Option<Result<ToolOutput, String>>> {
            Box::pin(async move {
                if !self.claims || name != self.name {
                    return None;
                }
                Some(self.answer.map(ToolOutput::text).map_err(str::to_owned))
            })
        }
    }

    async fn call_tool(server: &McpServer, name: &str) -> (Vec<Value>, bool) {
        let request: JsonRpcRequest = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": name, "arguments": {} },
        }))
        .expect("a tools/call request");
        let response = server.dispatch(&request).await.expect("an id-bearing request is answered");
        match response.result.expect("tools/call answers") {
            crate::mcp::protocol::JsonRpcResult::ToolsCall { content, is_error } => {
                (content, is_error)
            }
            other => panic!("tools/call answered something else: {other:?}"),
        }
    }

    /// **A provider's tools are dispatched back to the provider.** The
    /// advertised name must reach its own `call`, a provider's failure must
    /// cross as the tool's error, and a name advertised but no longer
    /// claimed must answer loudly rather than as (or as an empty) success.
    #[tokio::test]
    async fn a_provider_tool_answers_ok_err_and_a_stale_name() {
        let ok = McpServerBuilder::new("forge", "1.2.3")
            .provider(Arc::new(Family { name: "vault__search", answer: Ok("found"), claims: true }))
            .build();
        let (content, is_error) = call_tool(&ok, "vault__search").await;
        assert!(!is_error, "a provider's success is a success");
        assert_eq!(content[0]["text"], "found", "the provider's own answer crosses");

        let err = McpServerBuilder::new("forge", "1.2.3")
            .provider(Arc::new(Family {
                name: "vault__search",
                answer: Err("the vault is locked"),
                claims: true,
            }))
            .build();
        let (content, is_error) = call_tool(&err, "vault__search").await;
        assert!(is_error, "a provider's failure is the tool's failure");
        assert_eq!(content[0]["text"], "the vault is locked");

        let stale = McpServerBuilder::new("forge", "1.2.3")
            .provider(Arc::new(Family {
                name: "vault__search",
                answer: Ok("found"),
                claims: false,
            }))
            .build();
        let (content, is_error) = call_tool(&stale, "vault__search").await;
        assert!(is_error, "an advertised name no provider claims is an error, not a success");
        assert!(
            content[0]["text"].as_str().is_some_and(|text| text.contains("no registered provider")),
            "and the error names the cause: {content:?}",
        );
    }
}
