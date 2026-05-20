//! Peer-coordination tools (#114 v1).
//!
//! Four tools the LLM in any session can call to communicate with
//! other forge agents (= projects from forge.toml):
//!
//! - `peers__ask_agent` — async question to another agent. Returns a
//!   correlation_id; reply lands as a new user-turn injection in the
//!   caller's chat once the recipient's `tell_agent { in_reply_to }`
//!   fires.
//! - `peers__tell_agent` — fire-and-forget message OR reply (when
//!   `in_reply_to` is set).
//! - `peers__list_agents` — snapshot of every configured project's
//!   peer status (running / sleeping / failed + in-flight counters).
//! - `peers__whoami` — caller's own identity (project name, org,
//!   path, model, permission mode).
//!
//! All four tools take a closure-bound [`SessionKey`] identifying the
//! caller plus an [`Arc<dyn WorkspaceFacade>`] for the workspace
//! state surface. [`build_server`] bakes both into each tool's
//! struct fields when the per-session MCP server is constructed.

use std::sync::Arc;

use forge_sdk::mcp::server::{McpServer, McpServerBuilder};
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};

use crate::SessionKey;
use crate::mcp::peers::facade::WorkspaceFacade;

pub mod facade;

/// Build the per-session `forge` MCP server with all four peer tools
/// closure-bound to `caller_key`. Called from
/// `forge_agent::forge_sdk_worker::build_options_with_callback` (lands
/// in C9) once per spawned session, so each `claude` subprocess sees
/// its own identity via `peers__whoami` and addresses other agents
/// via `peers__ask_agent` / `peers__tell_agent`.
///
/// The server is named `forge`. Tool names carry the `peers__` prefix
/// so they render to the LLM as `mcp__forge__peers__<name>`. Future
/// modules (worktree, memory, …) slot in alongside `peers` under the
/// same `forge` server without touching the auto-approve fast-path
/// in forge-sdk's `control_dispatch` (which matches the
/// `mcp__forge__` prefix at the tool-name level).
pub fn build_server(facade: Arc<dyn WorkspaceFacade>, caller_key: SessionKey) -> McpServer {
    let whoami = Whoami { facade: facade.clone(), caller_key };
    let list_agents = ListAgents { facade };
    McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION"))
        .tool(whoami)
        .tool(list_agents)
        .build()
}

/// `peers__whoami` — caller's own identity. No args. Returns a
/// JSON blob containing name, org, path, current liveness, model
/// (when known), and the current in-flight peer-message counters.
///
/// Useful for the LLM when it needs to refer to itself in a message
/// to a peer ("This is `forge` asking…"), or to confirm which
/// project context it's running in.
pub(crate) struct Whoami {
    /// Workspace-state surface, captured at server-build time.
    pub(crate) facade: Arc<dyn WorkspaceFacade>,
    /// The session this server was built for. Closure-bound here so
    /// the tool doesn't need the LLM to pass identity as an arg.
    pub(crate) caller_key: SessionKey,
}

#[async_trait::async_trait]
impl Tool for Whoami {
    // `&'static str` literal returned where the trait sigs `&str`.
    // Matches the established convention in `forge_sdk::mcp::macros`.
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "peers__whoami"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Returns YOUR identity as a forge agent: project name, organization \
         (the [[orgs]] entry from forge.toml), filesystem path, current \
         liveness status, model (when running), and current in-flight \
         peer-message counters. Useful when you need to refer to yourself \
         in a message to a peer agent, or when an ask wrapper says \
         'from agent X' and you want to confirm what X resolves to. \
         Takes no arguments."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        match self.facade.whoami(&self.caller_key) {
            Some(identity) => match serde_json::to_string_pretty(&identity) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => ToolOutput {
                    blocks: vec![forge_sdk::mcp::tool::ToolOutputBlock {
                        text: format!("identity serialization failed: {err}"),
                    }],
                    is_error: true,
                },
            },
            None => ToolOutput {
                blocks: vec![forge_sdk::mcp::tool::ToolOutputBlock {
                    text: format!(
                        "no identity resolved for caller {} (this is a forge bug; the \
                         caller key should always resolve to a forge.toml project)",
                        self.caller_key.as_str(),
                    ),
                }],
                is_error: true,
            },
        }
    }
}

/// `peers__list_agents` — snapshot of every forge.toml project's
/// peer status. No args. Returns a JSON array.
///
/// Used by the LLM BEFORE calling `peers__ask_agent` or
/// `peers__tell_agent` to discover which peers exist and which are
/// currently running vs sleeping. The `in_flight_incoming` /
/// `in_flight_outgoing` counters also let the LLM gauge whether a
/// peer is busy before initiating a new conversation.
pub(crate) struct ListAgents {
    pub(crate) facade: Arc<dyn WorkspaceFacade>,
}

#[async_trait::async_trait]
impl Tool for ListAgents {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "peers__list_agents"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "List all forge agents (= projects loaded from forge.toml). Returns \
         each agent's name, organization, filesystem path, current status \
         (running / sleeping / failed), model when running, and per-agent \
         in-flight peer-message counters (asks awaiting reply both \
         incoming and outgoing). Use this BEFORE peers__ask_agent or \
         peers__tell_agent to discover which agents you can talk to and \
         to gauge whether they're already busy. Sleeping agents are still \
         callable - they auto-spawn on ask/tell. Takes no arguments."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        let peers = self.facade.list_peers();
        match serde_json::to_string_pretty(&peers) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => ToolOutput {
                blocks: vec![forge_sdk::mcp::tool::ToolOutputBlock {
                    text: format!("peer-list serialization failed: {err}"),
                }],
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::peers::facade::MockWorkspaceFacade;
    use forge_primitives::{PeerLiveness, PeerStatus};

    fn fake_key(s: &str) -> SessionKey {
        SessionKey::from_session_id(s)
    }

    fn fake_peer(name: &str) -> PeerStatus {
        PeerStatus {
            name: name.to_owned(),
            org: "TestOrg".to_owned(),
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            status: PeerLiveness::Running,
            model: Some("claude-opus-4-7".to_owned()),
            in_flight_incoming: 0,
            in_flight_outgoing: 0,
            spawned_at: None,
        }
    }

    #[tokio::test]
    async fn whoami_returns_caller_identity_as_json() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge"));
        let facade = mock.into_arc();
        let tool = Whoami { facade, caller_key: fake_key("forge") };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error, "whoami should not error on resolved identity");
        let block = &output.blocks[0];
        let parsed: serde_json::Value =
            serde_json::from_str(&block.text).expect("output is valid JSON");
        assert_eq!(parsed["name"], "forge");
        assert_eq!(parsed["org"], "TestOrg");
        assert_eq!(parsed["status"], "running");
    }

    #[tokio::test]
    async fn whoami_errors_when_caller_unresolved() {
        let mock = MockWorkspaceFacade::new();
        // No peers pre-loaded — whoami can't find one matching the
        // caller's name.
        let facade = mock.into_arc();
        let tool = Whoami { facade, caller_key: fake_key("ghost") };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(output.is_error, "unresolved caller must surface as is_error");
        assert!(
            output.blocks[0].text.contains("ghost"),
            "error body should mention the offending caller key, got: {}",
            output.blocks[0].text,
        );
    }

    #[test]
    fn whoami_metadata_shape() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let tool = Whoami { facade, caller_key: fake_key("test") };
        assert_eq!(tool.name(), "peers__whoami");
        assert!(tool.description().to_lowercase().contains("identity"));
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        // Schema describes no arguments — empty properties.
        let props = schema["properties"].as_object().expect("properties is object");
        assert!(props.is_empty(), "whoami takes no arguments");
    }

    #[test]
    fn build_server_registers_all_phase2_tools() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let server = build_server(facade, fake_key("test"));
        let debug = format!("{server:?}");
        for expected in ["peers__whoami", "peers__list_agents"] {
            assert!(
                debug.contains(expected),
                "build_server must include {expected}; debug: {debug}",
            );
        }
    }

    #[tokio::test]
    async fn list_agents_returns_all_peers_as_json_array() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(PeerStatus {
            name: "granite-backend".to_owned(),
            org: "Granite".to_owned(),
            path: std::path::PathBuf::from("/tmp/granite-backend"),
            status: PeerLiveness::Sleeping,
            model: None,
            in_flight_incoming: 0,
            in_flight_outgoing: 0,
            spawned_at: None,
        });
        let facade = mock.into_arc();
        let tool = ListAgents { facade };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let arr = parsed.as_array().expect("output is an array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "forge");
        assert_eq!(arr[0]["status"], "running");
        assert_eq!(arr[1]["name"], "granite-backend");
        assert_eq!(arr[1]["status"], "sleeping");
    }

    #[tokio::test]
    async fn list_agents_with_no_peers_returns_empty_array() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let tool = ListAgents { facade };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert_eq!(parsed.as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_agents_metadata_shape() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let tool = ListAgents { facade };
        assert_eq!(tool.name(), "peers__list_agents");
        assert!(tool.description().to_lowercase().contains("list"));
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].as_object().unwrap().is_empty());
    }
}
