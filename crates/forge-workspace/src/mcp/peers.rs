//! Peer-coordination tools (#114 v1).
//!
//! Four tools the LLM in any session can call to communicate with
//! other forge agents (= projects from forge.toml):
//!
//! - `peers__ask_agent` - async question to another agent. Returns a
//!   correlation_id; reply lands as a new user-turn injection in the
//!   caller's chat once the recipient's `tell_agent { in_reply_to }`
//!   fires.
//! - `peers__tell_agent` - fire-and-forget message OR reply (when
//!   `in_reply_to` is set).
//! - `peers__list_agents` - snapshot of every configured project's
//!   peer status (running / sleeping / failed + in-flight counters).
//! - `peers__whoami` - caller's own identity (project name, org,
//!   path, model, permission mode).
//!
//! All four tools take a closure-bound [`SessionKey`] identifying the
//! caller plus an [`Arc<dyn WorkspaceFacade>`] for the workspace
//! state surface. [`build_server`] bakes both into each tool's
//! struct fields when the per-session MCP server is constructed.

use std::sync::Arc;

#[cfg(test)]
use forge_sdk::mcp::server::McpServer;
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput};

use crate::mcp::peers::facade::{CallerKeyResolver, PeerStatsDelta, WorkspaceFacade};
use crate::mcp::peers::types::{
    AskChannel, CorrelationId, InflightAsk, ReplyRouting, WrappedKind, WrappedPrompt,
};

pub mod facade;
pub mod types;

/// Build a standalone `forge` MCP server carrying only the four
/// peer-coordination tools. Used in tests for isolated peer-MCP
/// coverage; the production build_site uses
/// `crate::mcp::build_forge_server` which combines peers + workers
/// into one server (the CLI rejects duplicate-name MCP servers, so
/// both modules must register their tools through a single
/// builder).
#[cfg(test)]
pub fn build_server(facade: Arc<dyn WorkspaceFacade>, caller_key: CallerKeyResolver) -> McpServer {
    add_tools(McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")), facade, caller_key).build()
}

/// Attach the four peer-coordination tools to an existing
/// [`McpServerBuilder`]. The parent module's `build_forge_server`
/// calls this to share the `forge` server name with workers' tools.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn WorkspaceFacade>,
    caller_key: CallerKeyResolver,
) -> McpServerBuilder {
    let whoami = Whoami { facade: facade.clone(), caller_key: caller_key.clone() };
    let list_agents = ListAgents { facade: facade.clone() };
    let tell_agent = TellAgent { facade: facade.clone(), caller_key: caller_key.clone() };
    let ask_agent = AskAgent { facade, caller_key };
    builder.tool(whoami).tool(list_agents).tool(tell_agent).tool(ask_agent)
}

/// `peers__whoami` - caller's own identity. No args. Returns a
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
    fn name(&self) -> &'static str {
        "peers__whoami"
    }

    fn description(&self) -> &'static str {
        "Returns your own forge-agent identity (project name, organization, \
         filesystem path, current model, and your in-flight peer-message \
         counters). Useful when an inbound wrapper says 'from/to agent X' \
         and you want to confirm whether X is you, or when you need to \
         refer to yourself in a message to another agent. Identity is \
         stable for the session - most callers will not need to call this \
         more than once. Takes no arguments."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        let caller_key = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
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

/// `peers__list_agents` - snapshot of every forge.toml project's
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
    fn name(&self) -> &'static str {
        "peers__list_agents"
    }

    fn description(&self) -> &'static str {
        "Discover which forge agents (projects loaded from forge.toml) you \
         can talk to via peers__ask_agent / peers__tell_agent. Returns each \
         agent's project name, organization, filesystem path, current \
         liveness (running / sleeping / failed), model when running, and \
         in-flight peer-message counters (asks awaiting reply, both \
         incoming and outgoing). \
         \
         FAMILY NOTE: the peers__* tools never create sessions - they \
         address other projects' already-existing agents. Spawning a \
         worker in YOUR project is workers__spawn, a separate family; if \
         you are here to delegate to a new worker, emit that instead. \
         \
         CROSS-PROJECT RULE (mutations only): whenever the user asks you \
         to CHANGE state in a project other than your own - edit files, \
         run a command, file an issue, push a branch, anything with side \
         effects - call this tool FIRST. If the target project appears in \
         the result, do NOT cd into it and mutate its files directly. \
         Hand the work off via peers__ask_agent (when you need an answer \
         or confirmation back) or peers__tell_agent (for a notification \
         or fire-and-forget hand-off). Each agent owns its own repo; \
         stay in your lane and let the peer execute the change. \
         \
         Reading another project's files for context is fine - sometimes \
         scanning the source yourself gives a sharper answer than waiting \
         on an ask. The constraint is only on writes / state changes. \
         \
         Call this before asking or telling so you use the right project \
         name - a misspelled target returns an immediate error listing \
         the valid set. Sleeping agents are still callable; they \
         auto-spawn on the first ask or tell. Takes no arguments."
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

/// `peers__tell_agent` - fire-and-forget message to another agent,
/// OR a reply to an earlier ask (when `in_reply_to` is set).
///
/// Arguments:
/// - `target` (string, required) - project name to deliver to
/// - `message` (string, required) - the message body
/// - `in_reply_to` (string, optional) - the correlation_id of an
///   earlier ask this message replies to
///
/// Returns a JSON object with `correlation_id`, `queued_at`,
/// `target_status` (delivered / queued_for_spawn).
///
/// `in_reply_to` semantics (best-effort with degradation):
/// - Found + caller-target match → wrapper kind = Reply, the original
///   ask gets resolved
/// - Found + mismatched caller/target → wrapper kind = Message
///   (log warn; LLM hallucinated the wrong target)
/// - Not found → wrapper kind = Message (log warn; LLM hallucinated
///   the correlation id)
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
    fn name(&self) -> &'static str {
        "peers__tell_agent"
    }

    fn description(&self) -> &'static str {
        "Send a one-way message to another forge agent (project). Returns \
         immediately with a correlation_id; no reply is expected from the \
         target. Two shapes: (1) REPLY to an inbound peers__ask_agent - \
         set in_reply_to to the correlation_id from that ask's wrapper, \
         and the original asker sees your message rendered as a Reply; \
         (2) UNSOLICITED - omit in_reply_to to send standalone prose \
         (announcements, FYI, hand-offs). The target sees the message as \
         a new user turn in its chat and may respond by sending another \
         tell, asking you back via peers__ask_agent, or simply continuing \
         its own work. Auto-spawns the target if it's currently sleeping. \
         \
         Use this (instead of mutating another project's files directly) \
         whenever the user asks you to notify or hand off work to another \
         forge project - e.g. \"let gateway-backend know the rewriter \
         cleanup landed\", \"tell forge to pick this up next session\". \
         The target's own agent will integrate the news inside its own \
         chat context. Reading the target's files for your own context is \
         still allowed; only state changes / hand-offs go through this \
         tool. Run peers__list_agents first to confirm the target name. \
         A `delivered` status means the queue ACCEPTED the message, not \
         that the target read it - a down or sleeping agent still \
         returns delivered, so confirm real work happened by a reply or \
         an observable artifact rather than by the ack."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Project name of the target agent. Must match an entry from peers__list_agents - case-sensitive. A misspelled name returns an error listing valid options.",
                },
                "message": {
                    "type": "string",
                    "description": "The message body. Rendered as a new user-turn in the target's chat, so write it as you'd address the target directly.",
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Optional. Set to the correlation_id of an inbound peers__ask_agent to mark this as a reply (the original asker will see it rendered as a Reply). Omit for unsolicited messages.",
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

        let caller_key = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
        let Some(identity) = self.facade.whoami(&caller_key) else {
            return tool_error(format!(
                "no identity resolved for caller {} (forge bug)",
                caller_key.as_str(),
            ));
        };

        // Validate LLM-supplied in_reply_to at the tool boundary. A
        // malformed id (wrong prefix, wrong length, non-hex,
        // uppercase) would otherwise miss the inflight-map lookup
        // silently and degrade to a Message, hiding the actual
        // problem. Reject with is_error so the LLM can see + fix.
        let in_reply_to_id = match args.in_reply_to.as_deref() {
            None => None,
            Some(s) => match CorrelationId::from_external(s) {
                Some(id) => Some(id),
                None => {
                    return tool_error(format!(
                        "in_reply_to {s:?} is not a well-formed correlation id \
                         (expected q-XXXXXXXX or t-XXXXXXXX, 8 lowercase hex chars)"
                    ));
                }
            },
        };
        let correlation_id = CorrelationId::new_tell();
        match classify_tell(&*self.facade, in_reply_to_id.as_ref()) {
            ReplyRouting::WrongChannel { correct_tool } => {
                let id = in_reply_to_id.as_ref().map(CorrelationId::as_str).unwrap_or_default();
                tool_error(format!(
                    "this question arrived over the workers channel (from your lead or a worker \
                     on your team). Reply with `{correct_tool}(in_reply_to={id})`, not \
                     peers__tell_agent - that tool is for other projects, addressed by project \
                     name."
                ))
            }
            ReplyRouting::Reply { caller, correlation } => {
                let wrapped = WrappedPrompt {
                    correlation_id: correlation_id.clone(),
                    kind: WrappedKind::Reply,
                    channel: AskChannel::Peers,
                    sender_name: identity.name,
                    sender_org: identity.org,
                    body: args.message,
                };
                if let Err(err) = self.facade.deliver_reply_to_caller(&caller, &wrapped) {
                    return tool_error(err.user_message());
                }
                // Reply resolved cleanly: close the ask (aborting its
                // timeout timer) and decrement the replier's incoming +
                // the original asker's outgoing counters.
                self.facade.complete_inflight_ask(&correlation);
                self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::IncomingMinus1);
                self.facade.bump_inflight_stats(&caller, PeerStatsDelta::OutgoingMinus1);
                tell_ok_response(&correlation_id, "delivered", None)
            }
            ReplyRouting::Message => {
                // An unknown/stale in_reply_to fell through to a plain
                // Message; note it so the replier's LLM can retry.
                let note = in_reply_to_id.as_ref().map(|id| {
                    format!(
                        "in_reply_to {id} did not match an open ask (it may be stale \
                         or already answered), so this was delivered as a plain \
                         message rather than a reply. Re-check the correlation \
                         id if you meant to reply."
                    )
                });
                let wrapped = WrappedPrompt {
                    correlation_id: correlation_id.clone(),
                    kind: WrappedKind::Message,
                    channel: AskChannel::Peers,
                    sender_name: identity.name,
                    sender_org: identity.org,
                    body: args.message,
                };
                let status =
                    match self.facade.deliver_peer_prompt(&caller_key, &args.target, wrapped) {
                        Ok(crate::mcp::peers::facade::TargetStatus::Delivered) => "delivered",
                        Ok(crate::mcp::peers::facade::TargetStatus::QueuedForSpawn) => {
                            "queued_for_spawn"
                        }
                        Err(err) => return tool_error(format_deliver_error(&err)),
                    };
                tell_ok_response(&correlation_id, status, note)
            }
        }
    }
}

/// Classify a `tell` from its optional `in_reply_to`. A resolved
/// same-channel id routes the Reply to the asker's session (the tell's
/// `target` label is irrelevant once the correlation resolves); a
/// resolved other-channel id is a `WrongChannel` steer; no/unknown id
/// is an unsolicited `Message`.
fn classify_tell(
    facade: &dyn WorkspaceFacade,
    in_reply_to: Option<&CorrelationId>,
) -> ReplyRouting {
    let Some(id) = in_reply_to else {
        return ReplyRouting::Message;
    };
    let Some(ask) = facade.resolve_correlation(id) else {
        tracing::warn!(
            target: "forge_workspace::mcp::peers",
            correlation_id = id.as_str(),
            "tell_agent in_reply_to references unknown correlation_id; treating as unsolicited Message"
        );
        return ReplyRouting::Message;
    };
    match ask.channel {
        AskChannel::Peers => ReplyRouting::Reply { caller: ask.caller, correlation: id.clone() },
        AskChannel::Workers => {
            ReplyRouting::WrongChannel { correct_tool: AskChannel::Workers.reply_tool() }
        }
    }
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![forge_sdk::mcp::tool::ToolOutputBlock { text }], is_error: true }
}

fn format_deliver_error(err: &facade::DeliverError) -> String {
    match err {
        facade::DeliverError::UnknownTarget { name } => format!(
            "peer '{name}' is not available (no such agent in forge.toml); call \
             peers__list_agents to see who you can reach."
        ),
    }
}

/// Build the standard `tell` success body. `note` carries an optional
/// degraded-reply explanation surfaced when an unknown `in_reply_to`
/// fell through to a plain Message.
fn tell_ok_response(
    correlation_id: &CorrelationId,
    target_status: &str,
    note: Option<String>,
) -> ToolOutput {
    let mut body = serde_json::json!({
        "correlation_id": correlation_id.as_str(),
        "queued_at": chrono_rfc3339_now(),
        "target_status": target_status,
    });
    if let Some(note) = note
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert("note".to_owned(), serde_json::Value::String(note));
    }
    match serde_json::to_string_pretty(&body) {
        Ok(json) => ToolOutput::text(json),
        Err(err) => tool_error(format!("response serialization failed: {err}")),
    }
}

/// RFC3339 timestamp for the current instant. Uses `time` (already in
/// forge's workspace deps via `forge-test-harness`). Returned as a
/// String so JSON serialization is straightforward.
fn chrono_rfc3339_now() -> String {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "rfc3339 format failed; emitting epoch sentinel");
        "1970-01-01T00:00:00Z".to_owned()
    })
}

/// `peers__ask_agent` - async question to another agent. Returns
/// immediately with a correlation_id; the reply lands later as a
/// new user-turn injection in YOUR chat when the recipient calls
/// `peers__tell_agent { in_reply_to: <this correlation_id> }`.
///
/// Arguments:
/// - `target` (string, required) - project name to ask
/// - `prompt` (string, required) - the question body
/// - `in_reply_to` (string, optional) - pass-through threading id
///   if this ask itself is a follow-up to an earlier message
///
/// Returns a JSON object with `correlation_id` (starts with `q-`),
/// `queued_at`, `target_status` (delivered / queued_for_spawn).
///
/// Auto-spawns the target if it's currently sleeping; the reply
/// will take longer in that case (one full spawn cycle). Multiple
/// asks can run in parallel - fire several ask_agent calls in one
/// turn, replies arrive independently and can be threaded back via
/// their distinct correlation_ids.
pub(crate) struct AskAgent {
    pub(crate) facade: Arc<dyn WorkspaceFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct AskArgs {
    target: String,
    prompt: String,
}

#[async_trait::async_trait]
impl Tool for AskAgent {
    fn name(&self) -> &'static str {
        "peers__ask_agent"
    }

    fn description(&self) -> &'static str {
        "Ask another forge agent (project) a question and receive their \
         reply asynchronously. Returns IMMEDIATELY with a correlation_id \
         (e.g. q-7f3a92e0); this tool does NOT wait for the reply. The \
         target's LLM will see your prompt as a new user turn, do its \
         work - possibly seconds, possibly minutes - and respond by \
         calling peers__tell_agent with in_reply_to set to your \
         correlation_id. That reply lands as a fresh user turn in YOUR \
         chat whenever it's ready, so finish your current turn naturally \
         and continue with other work; the reply will surface on its own. \
         Multiple asks can run in parallel - fire several ask_agent calls \
         in one turn and the replies arrive independently, each carrying \
         its own correlation_id you can thread back. Synchronous errors \
         (target not in forge.toml) return \
         is_error: true; later-detected delivery failures arrive as a \
         '[Ask ... failed to deliver: ...]' envelope in your chat. \
         Auto-spawns sleeping targets (expect extra latency on the first \
         ask). \
         \
         Use this whenever you need another forge project to TAKE AN \
         ACTION or give you an authoritative answer that only its own \
         agent should produce - running a build there, kicking off a \
         migration there, confirming whether a deploy landed, asking it \
         to review a design from its own context. Reading the target's \
         files for your own context is fine and often quicker than \
         waiting on an ask; the rule is only that state changes happen \
         through the target's own agent, via this tool. Run \
         peers__list_agents first to confirm the target name and that \
         it's a known peer."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Project name of the target agent. Must match an entry from peers__list_agents - case-sensitive. A misspelled name returns an error listing valid options.",
                },
                "prompt": {
                    "type": "string",
                    "description": "The question body. Rendered as a new user-turn in the target's chat - write it as a direct request to the target. Include enough context that the target can answer without further round-trips.",
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Optional. Set if this ask threads onto an earlier peer message (e.g. asking a follow-up after a tell). Most asks start a fresh thread and omit this field.",
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

        let caller_key = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
        let Some(identity) = self.facade.whoami(&caller_key) else {
            return tool_error(format!(
                "no identity resolved for caller {} (forge bug)",
                caller_key.as_str(),
            ));
        };

        let correlation_id = CorrelationId::new_ask();

        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind: WrappedKind::Question,
            channel: AskChannel::Peers,
            sender_name: identity.name.clone(),
            sender_org: identity.org.clone(),
            body: args.prompt,
        };

        // Register the inflight ask BEFORE dispatching delivery so a
        // fast-path recipient (running session, idle, processes the
        // prompt immediately on its next turn) can't fire a
        // `tell_agent { in_reply_to: q-X }` reply that hits
        // `resolve_correlation` before the ask is in the map. Without
        // this order, the reply degrades silently to
        // `WrappedKind::Message` and the original ask is never marked
        // Replied. On dispatch failure we roll back the registration
        // so the sidebar outgoing-counter / inflight map don't leak.
        self.facade.register_inflight_ask(InflightAsk {
            correlation_id: correlation_id.clone(),
            channel: AskChannel::Peers,
            caller: caller_key.clone(),
            target_project: args.target.clone(),
            target_session: None,
        });
        self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::OutgoingPlus1);
        let target_status =
            match self.facade.deliver_peer_prompt(&caller_key, &args.target, wrapped) {
                Ok(s) => s,
                Err(err) => {
                    // Rollback: the dispatch never reached the
                    // recipient so the caller's outstanding-counter
                    // and inflight_asks entry would otherwise leak.
                    self.facade.complete_inflight_ask(&correlation_id);
                    self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::OutgoingMinus1);
                    return tool_error(format_deliver_error(&err));
                }
            };

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
    use crate::SessionKey;
    use crate::mcp::peers::facade::MockWorkspaceFacade;
    use crate::mcp::peers::types::{InflightAsk, PeerLiveness, PeerStatus};

    fn fake_key(s: &str) -> SessionKey {
        SessionKey::from_session_id(s)
    }

    fn fake_peer(name: &str) -> PeerStatus {
        PeerStatus {
            name: name.to_owned(),
            org: "TestOrg".to_owned(),
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            status: PeerLiveness::Running,
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
        // No peers pre-loaded - whoami can't find one matching the
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
        // Schema describes no arguments - empty properties.
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
            name: "gateway-backend".to_owned(),
            org: "Gateway".to_owned(),
            path: std::path::PathBuf::from("/tmp/gateway-backend"),
            status: PeerLiveness::Sleeping,
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
        assert_eq!(arr[1]["name"], "gateway-backend");
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
        target_project: &str,
    ) -> InflightAsk {
        InflightAsk {
            correlation_id: CorrelationId(correlation_id.to_owned()),
            channel: AskChannel::Peers,
            caller: fake_key(caller_key_str),
            target_project: target_project.to_owned(),
            target_session: None,
        }
    }

    #[tokio::test]
    async fn tell_agent_unsolicited_returns_correlation_id() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge")); // caller
        mock.peers.lock().push(fake_peer("gateway-backend")); // target
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "gateway-backend",
                    "message": "FYI just pushed something.",
                }),
            })
            .await;
        assert!(!output.is_error, "happy path should not error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let id = parsed["correlation_id"].as_str().expect("correlation_id present");
        assert!(id.starts_with("t-"), "tell correlation ids prefix t-, got {id}");
        assert!(parsed.get("note").is_none(), "unsolicited tell carries no degraded-reply note");
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
        assert!(
            output.blocks[0].text.contains("not available"),
            "unknown target should read as not available: {}",
            output.blocks[0].text,
        );
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
        // Setup: gateway-backend had asked forge earlier (the ask sits
        // in inflight_asks with caller_project = gateway-backend,
        // target_project = forge). Now forge is replying via tell.
        let mock = Arc::new(MockWorkspaceFacade::new());
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(fake_peer("gateway-backend"));
        mock.inflight.lock().insert(
            CorrelationId("q-7f3a92e0".to_owned()),
            fake_inflight(
                "q-7f3a92e0",
                "gateway-backend", // original caller's session key
                "forge",           // target the original ask went to (now the replying agent)
            ),
        );
        let facade: Arc<dyn WorkspaceFacade> = mock.clone();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "gateway-backend",
                    "message": "We use pgtemp.",
                    "in_reply_to": "q-7f3a92e0",
                }),
            })
            .await;
        assert!(!output.is_error, "Reply should not error: {:?}", output.blocks);
        // A cleanly-resolved reply is not degraded, so it carries no note.
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert!(parsed.get("note").is_none(), "a resolved reply carries no degraded note");

        let replies = mock.reply_to_caller_calls.lock();
        assert_eq!(replies.len(), 1, "reply delivered to the caller exactly once: {replies:?}");
        assert!(
            matches!(replies[0].1.kind, WrappedKind::Reply),
            "a resolved reply is delivered as a Reply, not a Message: {replies:?}"
        );
        drop(replies);
        assert_eq!(
            mock.deliver_calls.lock().len(),
            0,
            "a resolved reply never re-delivers as a fresh prompt",
        );
    }

    #[tokio::test]
    async fn tell_agent_in_reply_to_unknown_correlation_degrades_to_message() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(fake_peer("gateway-backend"));
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        // Valid-shape but never-registered id - exercises the
        // "lookup miss → degrade to Message" path. Malformed-id
        // input (wrong case, wrong length) takes the
        // CorrelationId::from_external rejection path instead and
        // is covered by tell_agent_in_reply_to_malformed_is_error.
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "gateway-backend",
                    "message": "Hi.",
                    "in_reply_to": "q-00000000",
                }),
            })
            .await;
        assert!(!output.is_error, "valid-shape unknown id degrades to Message, not error");
        // A degraded reply must not be a bare success: the result
        // carries a note so the replier's LLM learns its reply landed
        // as a plain message and can retry with the right id.
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let note = parsed["note"].as_str().expect("degraded reply carries a note");
        assert!(note.contains("q-00000000"), "note names the unresolved id: {note}");
        assert!(
            note.contains("plain message") && note.contains("open ask"),
            "note explains it landed as a plain message and the ask was not open: {note}",
        );
    }

    #[tokio::test]
    async fn tell_agent_in_reply_to_malformed_is_error() {
        // Uppercase hex isn't well-formed (`from_external` rejects).
        // Without this guard the lookup misses silently and the
        // reply degrades to a Message - hiding the LLM's bug.
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("forge"));
        mock.peers.lock().push(fake_peer("gateway-backend"));
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "gateway-backend",
                    "message": "Hi.",
                    "in_reply_to": "q-DEADBEEF",
                }),
            })
            .await;
        assert!(output.is_error, "malformed in_reply_to must surface as is_error");
        assert!(
            output.blocks[0].text.contains("well-formed"),
            "error body should mention the validation failure",
        );
    }

    #[tokio::test]
    async fn tell_reply_same_channel_routes_to_asker_ignoring_target_label() {
        // A peers-channel ask from A is open. B replies via
        // peers__tell_agent with in_reply_to=q-x but a bogus target
        // label. The reply must route by correlation straight to A's
        // session, not degrade on the mismatched label.
        let mock = Arc::new(MockWorkspaceFacade::new());
        mock.peers.lock().push(fake_peer("B")); // the replier's identity
        mock.inflight
            .lock()
            .insert(CorrelationId("q-11112222".to_owned()), fake_inflight("q-11112222", "A", "B"));
        let facade: Arc<dyn WorkspaceFacade> = mock.clone();
        let tool = TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("B")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "lead",
                    "message": "here's your answer",
                    "in_reply_to": "q-11112222",
                }),
            })
            .await;
        assert!(!output.is_error, "same-channel reply must not error: {:?}", output.blocks);
        let replies = mock.reply_to_caller_calls.lock();
        assert_eq!(replies.len(), 1, "reply delivered by session exactly once");
        assert_eq!(replies[0].0, fake_key("A"), "reply routed to the asker's session");
        assert!(matches!(replies[0].1.kind, WrappedKind::Reply), "delivered as a Reply");
        drop(replies);
        assert_eq!(mock.complete_calls.lock().len(), 1, "inflight ask closed");
        assert_eq!(mock.complete_calls.lock()[0].as_str(), "q-11112222");
        let bumps = mock.bump_calls.lock();
        assert!(
            bumps.iter().any(|(k, d)| *k == fake_key("B") && *d == PeerStatsDelta::IncomingMinus1),
            "replier's incoming decrements: {bumps:?}",
        );
        assert!(
            bumps.iter().any(|(k, d)| *k == fake_key("A") && *d == PeerStatsDelta::OutgoingMinus1),
            "asker's outgoing decrements: {bumps:?}",
        );
        assert_eq!(
            mock.deliver_calls.lock().len(),
            0,
            "a resolved reply must not fall through to deliver_peer_prompt",
        );
    }

    #[tokio::test]
    async fn tell_reply_from_workers_channel_is_steered_to_workers_tell() {
        // A workers-channel ask is open. Replying to it via
        // peers__tell_agent must be rejected with a steer to
        // workers__tell, never delivered as a peer message.
        let mock = Arc::new(MockWorkspaceFacade::new());
        mock.peers.lock().push(fake_peer("B"));
        mock.inflight.lock().insert(
            CorrelationId("q-55556666".to_owned()),
            InflightAsk {
                correlation_id: CorrelationId("q-55556666".to_owned()),
                channel: AskChannel::Workers,
                caller: fake_key("A"),
                target_project: "B".to_owned(),
                target_session: None,
            },
        );
        let facade: Arc<dyn WorkspaceFacade> = mock.clone();
        let tool = TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("B")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "A",
                    "message": "reply via the wrong tool",
                    "in_reply_to": "q-55556666",
                }),
            })
            .await;
        assert!(output.is_error, "wrong-channel reply must be a steered error");
        let text = &output.blocks[0].text;
        assert!(text.contains("workers__tell"), "steer names the right tool: {text}");
        assert!(text.contains("q-55556666"), "steer names the correlation id: {text}");
        assert_eq!(mock.reply_to_caller_calls.lock().len(), 0, "no reply delivered");
        assert_eq!(mock.complete_calls.lock().len(), 0, "ask not closed");
        assert_eq!(mock.deliver_calls.lock().len(), 0, "no unsolicited delivery either");
    }

    #[tokio::test]
    async fn tell_reply_delivery_failure_keeps_ask_open() {
        // A same-channel reply whose by-session delivery fails (asker's
        // session gone) must surface the error, leave the ask OPEN, and
        // not decrement either counter.
        let mock = Arc::new(MockWorkspaceFacade::new());
        mock.peers.lock().push(fake_peer("B"));
        mock.inflight
            .lock()
            .insert(CorrelationId("q-99990000".to_owned()), fake_inflight("q-99990000", "A", "B"));
        *mock.force_reply_error.lock() =
            Some(crate::mcp::peers::facade::ReplyDeliverError::CallerSessionGone);
        let facade: Arc<dyn WorkspaceFacade> = mock.clone();
        let tool = TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("B")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "A",
                    "message": "reply that can't land",
                    "in_reply_to": "q-99990000",
                }),
            })
            .await;
        assert!(output.is_error, "a failed reply must surface as an error");
        assert!(
            output.blocks[0].text.contains("no longer available"),
            "error carries the not-available reply message: {}",
            output.blocks[0].text,
        );
        assert_eq!(mock.complete_calls.lock().len(), 0, "the ask must stay open");
        let bumps = mock.bump_calls.lock();
        assert!(
            !bumps.iter().any(|(_, d)| *d == PeerStatsDelta::IncomingMinus1
                || *d == PeerStatsDelta::OutgoingMinus1),
            "no counters decrement on a failed reply: {bumps:?}",
        );
    }

    #[test]
    fn tell_agent_metadata_shape() {
        let mock = MockWorkspaceFacade::new();
        let facade = mock.into_arc();
        let tool =
            TellAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        assert_eq!(tool.name(), "peers__tell_agent");
        assert!(
            tool.description().to_lowercase().contains("one-way"),
            "tell_agent description should signal its one-way semantics",
        );
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
        mock.peers.lock().push(fake_peer("gateway-backend"));
        let facade = mock.into_arc();
        let tool =
            AskAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "gateway-backend",
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
        mock.peers.lock().push(fake_peer("gateway-backend"));
        let facade: Arc<dyn WorkspaceFacade> = mock.clone();
        let tool =
            AskAgent { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("forge")) };
        let _ = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "target": "gateway-backend",
                    "prompt": "hi",
                }),
            })
            .await;

        assert_eq!(mock.register_calls.lock().len(), 1, "inflight ask should be registered");
        let registered = &mock.register_calls.lock()[0];
        assert!(registered.correlation_id.as_str().starts_with("q-"));
        assert_eq!(registered.target_project, "gateway-backend");

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
        // The race-fix in `AskAgent::call` registers the ask BEFORE
        // dispatching delivery so a fast-path recipient can't beat
        // the registration. When delivery fails (as it does here on
        // UnknownTarget), we ROLL BACK the registration via
        // `complete_inflight_ask` and `OutgoingMinus1` so the inflight
        // map and sidebar counter both return to their pre-call
        // state. Net observable effect: register +1 / complete -1
        // and stats +1 / -1, all paired.
        let register_count = mock.register_calls.lock().len();
        let complete_count = mock.complete_calls.lock().len();
        assert_eq!(register_count, 1, "race-safety: ask is registered before delivery dispatch");
        assert_eq!(complete_count, 1, "rollback: failed delivery removes the just-registered ask");
        let bumps = mock.bump_calls.lock();
        assert_eq!(bumps.len(), 2, "stats bump + rollback");
        assert_eq!(bumps[0].1, PeerStatsDelta::OutgoingPlus1);
        assert_eq!(bumps[1].1, PeerStatsDelta::OutgoingMinus1);
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
                    "target": "gateway-backend",
                }),
            })
            .await;
        assert!(output.is_error);
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
