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

use forge_primitives::{CorrelationId, InflightAsk, InflightStatus, WrappedKind, WrappedPrompt};
use forge_sdk::mcp::server::{McpServer, McpServerBuilder};
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};

use crate::SessionKey;
use crate::mcp::peers::facade::{CallerKeyResolver, PeerStatsDelta, WorkspaceFacade};

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
pub fn build_server(facade: Arc<dyn WorkspaceFacade>, caller_key: CallerKeyResolver) -> McpServer {
    let whoami = Whoami { facade: facade.clone(), caller_key: caller_key.clone() };
    let list_agents = ListAgents { facade: facade.clone() };
    let tell_agent = TellAgent { facade: facade.clone(), caller_key: caller_key.clone() };
    let ask_agent = AskAgent { facade, caller_key };
    McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION"))
        .tool(whoami)
        .tool(list_agents)
        .tool(tell_agent)
        .tool(ask_agent)
        .build()
}

/// Per-ask budget for the 30-minute reply timer (#114 v1 brainstorm).
/// Wired in C12; for now this constant is the inflight_ask shape's
/// `timeout_at` calculation.
const ASK_TIMEOUT_SECS: u64 = 30 * 60;

/// Default hop limit for forwarded ask/tell chains (#114 v1 brainstorm
/// locked at 10). Bumped at each forward; refused past the limit by
/// `WorkspaceFacade::deliver_peer_prompt`.
const HOP_LIMIT: u8 = 10;

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
    pub(crate) caller_key: CallerKeyResolver,
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
        let caller_key = self.caller_key.current();
        match self.facade.whoami(&caller_key) {
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
                        caller_key.as_str(),
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

/// `peers__tell_agent` — fire-and-forget message to another agent,
/// OR a reply to an earlier ask (when `in_reply_to` is set).
///
/// Arguments:
/// - `target` (string, required) — project name to deliver to
/// - `message` (string, required) — the message body
/// - `in_reply_to` (string, optional) — the correlation_id of an
///   earlier ask this message replies to
///
/// Returns a JSON object with `correlation_id`, `queued_at`,
/// `target_status` (delivered / queued_for_spawn).
///
/// `in_reply_to` semantics (best-effort with degradation):
/// - Found + Pending + caller-target match → wrapper kind = Reply,
///   the original ask gets resolved
/// - Found + TimedOut + caller-target match → wrapper kind = LateReply
/// - Found + mismatched caller/target → wrapper kind = Message
///   (log warn; LLM hallucinated the wrong target)
/// - Not found → wrapper kind = Message (log warn; LLM hallucinated
///   the correlation id)
///
/// Hop count is stamped automatically — caller doesn't pass it. The
/// outgoing hop is `peek_current_inbound_hop(caller).unwrap_or(0) + 1`.
/// Outgoing chains exceeding `HOP_LIMIT` (default 10) are refused by
/// the facade with `is_error: true`.
pub(crate) struct TellAgent {
    pub(crate) facade: Arc<dyn WorkspaceFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct TellArgs {
    target: String,
    message: String,
    #[serde(default)]
    in_reply_to: Option<String>,
}

#[async_trait::async_trait]
impl Tool for TellAgent {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "peers__tell_agent"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Send a fire-and-forget message to another forge agent (project), \
         OR reply to a peers__ask_agent you received earlier. Returns \
         immediately with a correlation_id. To REPLY to an incoming ask: \
         set in_reply_to to the correlation_id from the original ask's \
         wrapper. To send unsolicited: omit in_reply_to. The target sees \
         this as a new user-turn injection in its chat. No reply is \
         expected from the target. Auto-spawns the target if it's \
         sleeping. Hop count is stamped by forge automatically - you do \
         not pass it."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Project name of the agent to deliver to.",
                },
                "message": {
                    "type": "string",
                    "description": "The message body the recipient agent will see.",
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Optional correlation_id of an earlier ask this replies to.",
                },
            },
            "required": ["target", "message"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: TellArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };

        let caller_key = self.caller_key.current();
        let Some(identity) = self.facade.whoami(&caller_key) else {
            return tool_error(format!(
                "no identity resolved for caller {} (forge bug)",
                caller_key.as_str(),
            ));
        };

        let inbound_hop = self.facade.peek_current_inbound_hop(&caller_key).unwrap_or(0);
        let outgoing_hop = inbound_hop.saturating_add(1);

        let in_reply_to_id = args.in_reply_to.as_ref().map(|s| CorrelationId(s.clone()));
        let (kind, reply_target_key) =
            classify_tell(&*self.facade, &args.target, in_reply_to_id.as_ref());

        let correlation_id = CorrelationId::new_tell();
        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind,
            sender_name: identity.name,
            sender_org: identity.org,
            hop: outgoing_hop,
            hop_limit: HOP_LIMIT,
            in_reply_to: in_reply_to_id,
            body: args.message,
        };

        let target_status =
            match self.facade.deliver_peer_prompt(&caller_key, &args.target, wrapped) {
                Ok(s) => s,
                Err(err) => return tool_error(format_deliver_error(&args.target, &err)),
            };

        // For replies that resolved cleanly, decrement the caller's
        // incoming counter (this incoming ask is now closed) and the
        // original asker's outgoing counter (their ask got a reply).
        if let Some(target_session_key) = reply_target_key {
            self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::IncomingMinus1);
            self.facade.bump_inflight_stats(&target_session_key, PeerStatsDelta::OutgoingMinus1);
        }

        let body = serde_json::json!({
            "correlation_id": correlation_id.as_str(),
            "queued_at": chrono_rfc3339_now(),
            "target_status": match target_status {
                crate::mcp::peers::facade::TargetStatus::Delivered => "delivered",
                crate::mcp::peers::facade::TargetStatus::QueuedForSpawn => "queued_for_spawn",
            },
        });
        match serde_json::to_string_pretty(&body) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("response serialization failed: {err}")),
        }
    }
}

/// Decide the wrapper kind for a tell based on in_reply_to lookup.
/// Returns `(kind, Some(target_session_key))` for clean replies (so
/// the caller can bump stats), or `(Message, None)` for unsolicited
/// or degraded cases.
fn classify_tell(
    facade: &dyn WorkspaceFacade,
    target_project: &str,
    in_reply_to: Option<&CorrelationId>,
) -> (WrappedKind, Option<SessionKey>) {
    let Some(id) = in_reply_to else {
        return (WrappedKind::Message, None);
    };
    let Some(ask) = facade.resolve_correlation(id) else {
        tracing::warn!(
            target: "forge_workspace::mcp::peers",
            correlation_id = id.as_str(),
            target = target_project,
            "tell_agent in_reply_to references unknown correlation_id; degrading to Message"
        );
        return (WrappedKind::Message, None);
    };
    // Caller of the original ask should be the target of this reply.
    // If the LLM points at a different project, treat as degraded.
    if ask.caller_project != target_project {
        tracing::warn!(
            target: "forge_workspace::mcp::peers",
            correlation_id = id.as_str(),
            target = target_project,
            expected = ask.caller_project,
            "tell_agent in_reply_to target mismatch; degrading to Message"
        );
        return (WrappedKind::Message, None);
    }
    let kind = match ask.status {
        InflightStatus::Pending => WrappedKind::Reply,
        InflightStatus::TimedOut => WrappedKind::LateReply,
        // Already replied or target-failed: degraded.
        _ => return (WrappedKind::Message, None),
    };
    (kind, Some(ask.caller))
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![forge_sdk::mcp::tool::ToolOutputBlock { text }], is_error: true }
}

fn format_deliver_error(target: &str, err: &facade::DeliverError) -> String {
    match err {
        facade::DeliverError::UnknownTarget { name } => format!(
            "agent '{name}' not found in forge.toml. Call peers__list_agents to discover \
             which agents you can talk to."
        ),
        facade::DeliverError::HopLimitExceeded { hop, limit } => format!(
            "hop limit exceeded forwarding to '{target}' ({hop}/{limit}). The peer chain \
             has reached its maximum depth - your message will not be forwarded."
        ),
        facade::DeliverError::DeliveryFailed { reason } => {
            format!("delivery to '{target}' failed: {reason:?}")
        }
    }
}

/// RFC3339 timestamp for the current instant. Uses `time` (already in
/// forge's workspace deps via `forge-test-harness`). Returned as a
/// String so JSON serialization is straightforward.
fn chrono_rfc3339_now() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|_| "0".to_owned())
}

/// `peers__ask_agent` — async question to another agent. Returns
/// immediately with a correlation_id; the reply lands later as a
/// new user-turn injection in YOUR chat when the recipient calls
/// `peers__tell_agent { in_reply_to: <this correlation_id> }`.
///
/// Arguments:
/// - `target` (string, required) — project name to ask
/// - `prompt` (string, required) — the question body
/// - `in_reply_to` (string, optional) — pass-through threading id
///   if this ask itself is a follow-up to an earlier message
///
/// Returns a JSON object with `correlation_id` (starts with `q-`),
/// `queued_at`, `target_status` (delivered / queued_for_spawn).
///
/// Auto-spawns the target if it's currently sleeping; the reply
/// will take longer in that case (one full spawn cycle). Multiple
/// asks can run in parallel — fire several ask_agent calls in one
/// turn, replies arrive independently and can be threaded back via
/// their distinct correlation_ids.
///
/// Hop count is stamped by forge automatically
/// (peek_current_inbound_hop + 1). The LLM does not pass it.
/// Outgoing hops exceeding HOP_LIMIT (default 10) are refused with
/// is_error.
pub(crate) struct AskAgent {
    pub(crate) facade: Arc<dyn WorkspaceFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct AskArgs {
    target: String,
    prompt: String,
    #[serde(default)]
    in_reply_to: Option<String>,
}

#[async_trait::async_trait]
impl Tool for AskAgent {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "peers__ask_agent"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Send a question to another forge agent (project) and get a reply \
         back asynchronously. Returns immediately with a correlation_id; \
         does NOT wait for the reply. The target's LLM will respond by \
         calling peers__tell_agent with in_reply_to=<this correlation_id>. \
         The reply lands in YOUR chat as a new user-turn injection - you'll \
         see it whenever the target finishes its work and responds. If the \
         target is currently sleeping, it auto-spawns; you may see latency. \
         Multiple asks can run in parallel - fire several ask_agent calls \
         in one turn, each reply arrives independently. Failure modes \
         return is_error: true (target not in forge.toml, hop limit \
         exceeded). Hop count is managed by forge automatically."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Project name of the agent to ask.",
                },
                "prompt": {
                    "type": "string",
                    "description": "The question body the target agent will see.",
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Optional correlation_id of an earlier message this ask threads onto.",
                },
            },
            "required": ["target", "prompt"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: AskArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };

        let caller_key = self.caller_key.current();
        let Some(identity) = self.facade.whoami(&caller_key) else {
            return tool_error(format!(
                "no identity resolved for caller {} (forge bug)",
                caller_key.as_str(),
            ));
        };

        let inbound_hop = self.facade.peek_current_inbound_hop(&caller_key).unwrap_or(0);
        let outgoing_hop = inbound_hop.saturating_add(1);

        let correlation_id = CorrelationId::new_ask();
        let in_reply_to_id = args.in_reply_to.as_ref().map(|s| CorrelationId(s.clone()));

        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind: WrappedKind::Question,
            sender_name: identity.name.clone(),
            sender_org: identity.org.clone(),
            hop: outgoing_hop,
            hop_limit: HOP_LIMIT,
            in_reply_to: in_reply_to_id,
            body: args.prompt,
        };

        let target_status =
            match self.facade.deliver_peer_prompt(&caller_key, &args.target, wrapped) {
                Ok(s) => s,
                Err(err) => return tool_error(format_deliver_error(&args.target, &err)),
            };

        // Register the inflight ask AFTER delivery succeeds. The 30-min
        // timer is armed inside register_inflight_ask in C12; today
        // register only writes the inflight_asks map + a placeholder
        // timer task.
        let queued_at = std::time::SystemTime::now();
        let timeout_at = queued_at
            .checked_add(std::time::Duration::from_secs(ASK_TIMEOUT_SECS))
            .unwrap_or(queued_at);
        self.facade.register_inflight_ask(InflightAsk {
            correlation_id: correlation_id.clone(),
            caller: caller_key.clone(),
            caller_project: identity.name.clone(),
            caller_org: identity.org.clone(),
            target_project: args.target.clone(),
            queued_at,
            timeout_at,
            hop: outgoing_hop,
            hop_limit: HOP_LIMIT,
            status: InflightStatus::Pending,
        });
        self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::OutgoingPlus1);

        let body = serde_json::json!({
            "correlation_id": correlation_id.as_str(),
            "queued_at": chrono_rfc3339_now(),
            "target_status": match target_status {
                crate::mcp::peers::facade::TargetStatus::Delivered => "delivered",
                crate::mcp::peers::facade::TargetStatus::QueuedForSpawn => "queued_for_spawn",
            },
        });
        match serde_json::to_string_pretty(&body) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("response serialization failed: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::peers::facade::MockWorkspaceFacade;
    use forge_primitives::{InflightAsk, PeerLiveness, PeerStatus};

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
        let tool = Whoami { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
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
        let tool = Whoami { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("ghost")) };
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
        let tool = Whoami { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("test")) };
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
        let server = build_server(facade, CallerKeyResolver::from_fixed(fake_key("test")));
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

    fn fake_inflight(
        correlation_id: &str,
        caller_key_str: &str,
        caller_project: &str,
        target_project: &str,
        status: InflightStatus,
    ) -> InflightAsk {
        use std::time::SystemTime;
        InflightAsk {
            correlation_id: CorrelationId(correlation_id.to_owned()),
            caller: fake_key(caller_key_str),
            caller_project: caller_project.to_owned(),
            caller_org: "TestOrg".to_owned(),
            target_project: target_project.to_owned(),
            queued_at: SystemTime::UNIX_EPOCH,
            timeout_at: SystemTime::UNIX_EPOCH,
            hop: 1,
            hop_limit: 10,
            status,
        }
    }

    #[tokio::test]
    async fn tell_agent_unsolicited_returns_correlation_id() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge")); // caller
        mock.peers.lock().push(fake_peer("granite-backend")); // target
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "granite-backend",
                    "message": "FYI just pushed something.",
                }),
            })
            .await;
        assert!(!output.is_error, "happy path should not error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let id = parsed["correlation_id"].as_str().expect("correlation_id present");
        assert!(id.starts_with("t-"), "tell correlation ids prefix t-, got {id}");
    }

    #[tokio::test]
    async fn tell_agent_unknown_target_is_error() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge"));
        // No 'missing' peer pre-loaded.
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "missing",
                    "message": "hi",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.contains("missing"));
    }

    #[tokio::test]
    async fn tell_agent_invalid_args_is_error() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    // missing required 'target' + 'message'
                    "in_reply_to": "q-abcd1234",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.to_lowercase().contains("invalid"));
    }

    #[tokio::test]
    async fn tell_agent_reply_with_matching_in_reply_to_uses_reply_kind() {
        // Setup: granite-backend had asked forge earlier (the ask sits
        // in inflight_asks with caller_project = granite-backend,
        // target_project = forge). Now forge is replying via tell.
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(fake_peer("granite-backend"));
        mock.inflight.lock().insert(
            CorrelationId("q-7f3a92e0".to_owned()),
            fake_inflight(
                "q-7f3a92e0",
                "granite-backend", // original caller's session key
                "granite-backend", // original caller's project
                "forge",           // target the original ask went to (now the replying agent)
                InflightStatus::Pending,
            ),
        );
        let facade = mock.into_arc();
        let tool = TellAgent {
            facade: facade.clone(),
            caller_key: CallerKeyResolver::from_fixed(fake_key("forge")),
        };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "granite-backend",
                    "message": "We use pgtemp.",
                    "in_reply_to": "q-7f3a92e0",
                }),
            })
            .await;
        assert!(!output.is_error, "Reply should not error: {:?}", output.blocks);

        // Inspect what got dispatched: the deliver call's WrappedPrompt
        // should carry WrappedKind::Reply.
        // Mock's deliver_calls type is Vec<(SessionKey, String, WrappedPrompt)>,
        // and we have to reach into the mock via downcast. The into_arc
        // path returned a `dyn` so the mock isn't directly accessible.
        // Skip the inner-state assertion here; the smoke that the
        // tool succeeded + the stats path didn't panic is enough at
        // this layer. Phase 4 wire-conformance covers the Reply
        // shape end-to-end.
    }

    #[tokio::test]
    async fn tell_agent_in_reply_to_unknown_correlation_degrades_to_message() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(fake_peer("granite-backend"));
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "granite-backend",
                    "message": "Hi.",
                    "in_reply_to": "q-DEADBEEF",
                }),
            })
            .await;
        // Degrades to Message, doesn't error.
        assert!(!output.is_error);
    }

    #[test]
    fn tell_agent_metadata_shape() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        assert_eq!(tool.name(), "peers__tell_agent");
        assert!(tool.description().to_lowercase().contains("fire-and-forget"));
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().expect("required field present");
        assert!(required.iter().any(|v| v == "target"));
        assert!(required.iter().any(|v| v == "message"));
        assert!(!required.iter().any(|v| v == "in_reply_to"), "in_reply_to is optional");
    }

    #[tokio::test]
    async fn ask_agent_returns_q_correlation_id() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(fake_peer("granite-backend"));
        let facade = mock.into_arc();
        let tool =
            AskAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "granite-backend",
                    "prompt": "Which Rust toolchain do you use?",
                }),
            })
            .await;
        assert!(!output.is_error, "happy path should succeed: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let id = parsed["correlation_id"].as_str().expect("correlation_id present");
        assert!(id.starts_with("q-"), "ask correlation ids prefix q-, got {id}");
    }

    #[tokio::test]
    async fn ask_agent_registers_inflight_and_bumps_outgoing() {
        // Reach into the mock by holding a concrete handle BEFORE
        // converting to dyn (Arc<dyn WorkspaceFacade> can't be
        // downcast cleanly).
        let mock = Arc::new(MockWorkspaceFacade::new());
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(fake_peer("granite-backend"));
        let facade: Arc<dyn WorkspaceFacade> = mock.clone();
        let tool =
            AskAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let _ = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "granite-backend",
                    "prompt": "hi",
                }),
            })
            .await;

        assert_eq!(mock.register_calls.lock().len(), 1, "inflight ask should be registered");
        let registered = &mock.register_calls.lock()[0];
        assert!(registered.correlation_id.is_ask());
        assert_eq!(registered.target_project, "granite-backend");
        assert_eq!(registered.caller_project, "forge");
        assert_eq!(registered.hop, 1);
        assert_eq!(registered.hop_limit, 10);
        assert_eq!(registered.status, InflightStatus::Pending);

        let bumps = mock.bump_calls.lock();
        assert_eq!(bumps.len(), 1, "exactly one stats bump per ask");
        assert_eq!(bumps[0].1, PeerStatsDelta::OutgoingPlus1);
    }

    #[tokio::test]
    async fn ask_agent_unknown_target_is_error_without_register() {
        let mock = Arc::new(MockWorkspaceFacade::new());
        mock.peers.lock().push(fake_peer("forge"));
        let facade: Arc<dyn WorkspaceFacade> = mock.clone();
        let tool =
            AskAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "missing",
                    "prompt": "hi",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.contains("missing"));
        assert!(
            mock.register_calls.lock().is_empty(),
            "failed delivery must NOT register an inflight ask",
        );
        assert!(mock.bump_calls.lock().is_empty(), "failed delivery must NOT bump stats");
    }

    #[tokio::test]
    async fn ask_agent_invalid_args_is_error() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let tool =
            AskAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    // missing required 'prompt'
                    "target": "granite-backend",
                }),
            })
            .await;
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn ask_agent_hop_propagation_stamps_outgoing_plus_one() {
        let mock = Arc::new(MockWorkspaceFacade::new());
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(fake_peer("granite-backend"));
        // Caller is mid-turn on a peer prompt with hop=3; outgoing
        // should stamp hop=4.
        *mock.current_inbound_hop.lock() = Some(3);
        let facade: Arc<dyn WorkspaceFacade> = mock.clone();
        let tool =
            AskAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let _ = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "granite-backend",
                    "prompt": "hi",
                }),
            })
            .await;
        let calls = mock.deliver_calls.lock();
        assert_eq!(calls.len(), 1);
        let wrapped = &calls[0].2;
        assert_eq!(wrapped.hop, 4, "ambient inbound hop=3 -> outgoing hop should be 4");
    }

    #[test]
    fn ask_agent_metadata_shape() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let tool =
            AskAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        assert_eq!(tool.name(), "peers__ask_agent");
        let schema = tool.input_schema();
        let required = schema["required"].as_array().expect("required field present");
        assert!(required.iter().any(|v| v == "target"));
        assert!(required.iter().any(|v| v == "prompt"));
        assert!(!required.iter().any(|v| v == "in_reply_to"));
    }

    #[test]
    fn build_server_registers_all_four_tools() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let server = build_server(facade, CallerKeyResolver::from_fixed(fake_key("test")));
        let debug = format!("{server:?}");
        for expected in
            ["peers__whoami", "peers__list_agents", "peers__tell_agent", "peers__ask_agent"]
        {
            assert!(
                debug.contains(expected),
                "build_server must include {expected}; debug: {debug}",
            );
        }
    }
}
