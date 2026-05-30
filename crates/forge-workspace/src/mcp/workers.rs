//! Workers MCP - project-internal child-agent coordination. Mirror
//! of `crate::mcp::peers`, scoped to within-project addressing by
//! label rather than cross-project addressing by project name.
//!
//! See `docs/superpowers/specs/2026-05-21-workers-mcp-design.md`.

use std::sync::Arc;

#[cfg(any(test, feature = "testing"))]
use forge_sdk::mcp::server::McpServer;
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use crate::mcp::peers::facade::{CallerKeyResolver, PeerStatsDelta};
use crate::mcp::peers::types::{CorrelationId, InflightAsk, WrappedKind, WrappedPrompt};
use crate::mcp::workers::facade::{
    LEAD_LABEL, WorkerDeliverError, WorkerFacade, WorkerLeadDeliverError, WorkerSpawnError,
};

pub mod facade;
pub mod types;

/// Default hop limit for forwarded ask/tell chains within a project.
/// Mirrors the peer-MCP value (#114 v1 brainstorm locked at 10).
const HOP_LIMIT: u8 = 10;

/// Composite key stored in `InflightAsk.target_project` for worker-
/// bound asks. The `::` separator can never appear in a real project
/// name (forge.toml validation rejects it), so this shape is
/// distinguishable from peer-MCP's plain-project-name fill at zero
/// schema cost. `expire_inflight_for_closed_worker` matches on the
/// composite when a worker's session is torn down.
#[must_use]
pub(crate) fn worker_target_project_key(project_key: &str, label: &str) -> String {
    format!("{project_key}::{label}")
}

/// Build a standalone `forge` MCP server carrying only the four
/// workers-coordination tools. Used in tests for isolated workers-MCP
/// coverage; the production build_site uses
/// `crate::mcp::build_forge_server` which combines peers + workers
/// into one server (the CLI rejects duplicate-name MCP servers, so
/// both modules must register their tools through a single builder).
#[cfg(any(test, feature = "testing"))]
pub fn build_server(facade: Arc<dyn WorkerFacade>, caller_key: CallerKeyResolver) -> McpServer {
    add_tools(McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")), facade, caller_key).build()
}

/// Attach the four workers-coordination tools to an existing
/// [`McpServerBuilder`]. The parent module's `build_forge_server`
/// calls this to share the `forge` server name with peers' tools.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn WorkerFacade>,
    caller_key: CallerKeyResolver,
) -> McpServerBuilder {
    let spawn = Spawn { facade: facade.clone(), caller_key: caller_key.clone() };
    let list = List { facade: facade.clone(), caller_key: caller_key.clone() };
    let tell = Tell { facade: facade.clone(), caller_key: caller_key.clone() };
    let ask = Ask { facade: facade.clone(), caller_key: caller_key.clone() };
    let create_role = CreateRole { facade, caller_key };
    builder.tool(spawn).tool(list).tool(tell).tool(ask).tool(create_role)
}

/// `workers__spawn` - lead-only. Allocates a new SessionTask in the
/// caller's project, threading `charter` through the new session's
/// system-prompt addendum, then returns the assigned `session_id` and
/// `tag` (`forge:worker:<label>`).
///
/// Arguments:
/// - `label` (string, required) - free-form short identifier used by
///   `workers__tell` / `workers__ask` to address this worker
/// - `charter` (string, required) - the worker's mission text, surfaced
///   to its LLM as a system-prompt addendum
///
/// Returns a JSON object: `{ "session_id": "...", "tag": "forge:worker:..." }`.
pub(crate) struct Spawn {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct SpawnArgs {
    label: String,
    charter: String,
}

#[async_trait::async_trait]
impl Tool for Spawn {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "workers__spawn"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Spawn a new worker session inside YOUR project (lead-only). \
         The worker is a full forge session - its own claude subprocess, \
         own chat view, own permissions - addressable from your session \
         by `label` via workers__tell / workers__ask. `charter` is the \
         worker's mission statement; it is threaded into the new \
         session's system prompt so the worker LLM has context for what \
         it is being asked to do. Returns the worker's session_id and \
         tag (`forge:worker:<label>`). Labels are free-form and need \
         not be unique - if you spawn multiple workers with the same \
         label, addressing picks the latest-spawned. The label 'lead' \
         is reserved (used by workers__tell / workers__ask to address \
         the spawning lead) and rejected here. Use workers__list to \
         see your project's current worker pool. This tool errors if \
         called from a worker session; only the project lead may spawn."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Short free-form identifier you will use to address this worker later via workers__tell / workers__ask. Non-empty after trim. Duplicates are allowed; if you reuse a label, addressing picks the latest-spawned.",
                },
                "charter": {
                    "type": "string",
                    "description": "The worker's mission statement. Threaded into the new session's system prompt so the worker LLM understands what it is being asked to do. Non-empty after trim. Write it as direct instructions to the worker.",
                },
            },
            "required": ["label", "charter"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: SpawnArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };

        let caller_key = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
        match self.facade.spawn_worker(&caller_key, args.label, args.charter).await {
            Ok(reply) => {
                let body = serde_json::json!({
                    "session_id": reply.session_id,
                    "tag": reply.tag,
                });
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_spawn_error(&err)),
        }
    }
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![ToolOutputBlock { text }], is_error: true }
}

fn format_spawn_error(err: &WorkerSpawnError) -> String {
    match err {
        WorkerSpawnError::NotLeadCaller => {
            "workers__spawn is lead-only; this session is a worker. Workers cannot \
             spawn other workers in v1."
                .to_owned()
        }
        WorkerSpawnError::EmptyLabel => "label must be non-empty after trim".to_owned(),
        WorkerSpawnError::ReservedLabel => format!(
            "label '{LEAD_LABEL}' is reserved - workers__tell / workers__ask use it as \
             the addressing keyword for the caller's lead. Pick a different label."
        ),
        WorkerSpawnError::EmptyCharter => "charter must be non-empty after trim".to_owned(),
        WorkerSpawnError::UnknownCallerProject => {
            "could not resolve caller to a known project (forge bug)".to_owned()
        }
        WorkerSpawnError::DispatchFailed { message } => {
            format!("worker spawn failed: {message}")
        }
        WorkerSpawnError::WorktreeCreationFailed { reason } => {
            format!("worktree creation failed: {reason}")
        }
    }
}

/// `workers__list` - any-caller snapshot of every worker live in the
/// caller's project. Returns a JSON array of `WorkerStatus`.
///
/// Used by the LLM to discover the local worker pool before calling
/// `workers__tell` / `workers__ask`. Returns an empty array when the
/// project has no live workers (or when the caller resolves to no
/// known project, which should never happen in practice).
pub(crate) struct List {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[async_trait::async_trait]
impl Tool for List {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "workers__list"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "List every worker currently live in YOUR project. Returns a \
         JSON array of worker snapshots (label, full charter, status, \
         session_id, spawned_at, spawned_by_session_id). Both lead and \
         worker sessions may call this; workers see the same set as \
         the lead. Use the labels from this output as targets for \
         workers__tell / workers__ask. An empty array means no workers \
         are live in your project. Takes no arguments."
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
        let workers = self.facade.list_workers(&caller_key);
        match serde_json::to_string_pretty(&workers) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("worker-list serialization failed: {err}")),
        }
    }
}

/// `workers__tell` - fire-and-forget message to a worker in the
/// caller's project, addressed by label. If multiple workers share
/// the label, the latest-spawned wins (resolved by the facade).
///
/// Arguments:
/// - `label` (string, required) - worker label from `workers__list`
/// - `message` (string, required) - the message body
///
/// Returns a JSON object with `correlation_id` and a `delivered`
/// status string for symmetry with peer-MCP's tell.
pub(crate) struct Tell {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct TellArgs {
    label: String,
    message: String,
    #[serde(default)]
    in_reply_to: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Tell {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "workers__tell"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Send a message to a worker in YOUR project by label. Two \
         shapes: (1) UNSOLICITED - omit `in_reply_to` to send standalone \
         prose; the message lands as a new user turn in the target's \
         chat, rendered as an incoming-from-<caller> block. (2) REPLY \
         to an earlier `workers__ask` - set `in_reply_to` to the \
         correlation_id from that ask's wrapper, and the original asker \
         sees your message rendered as a Reply (the inflight ask closes; \
         the asker's outgoing counter + your incoming counter both \
         decrement). If multiple workers share the same label, \
         addressing picks the latest-spawned. The reserved label 'lead' \
         targets the caller's lead (worker-only - a worker can use this \
         to send FYIs / replies back to whoever spawned it; project \
         leads have no lead and will get an error). Available to both \
         lead and worker callers (apart from the 'lead' case). Run \
         workers__list first to confirm the label and that the worker \
         is live."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Worker label from workers__list. Case-sensitive. If multiple workers share the label, the latest-spawned receives the message. Use the reserved label 'lead' to address the caller's spawning lead (worker-only).",
                },
                "message": {
                    "type": "string",
                    "description": "Message body. Rendered as a new user turn in the target's chat - write it as direct instructions or context.",
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Optional. Set to the correlation_id (q-XXXXXXXX) of an inbound `workers__ask` to mark this as a reply. The original asker sees it as a Reply envelope; the inflight ask closes and stat counters decrement on both sides. Omit for unsolicited messages.",
                },
            },
            "required": ["label", "message"],
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

        // Validate `in_reply_to` shape at the tool boundary. A
        // malformed id would silently miss the inflight-map lookup
        // and degrade to Message, hiding the actual problem - reject
        // explicitly so the LLM sees + fixes.
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

        // Classify the tell. If `in_reply_to` resolves to an
        // InflightAsk whose `caller` matches the resolved target
        // session, this is a clean reply - kind=Reply, and we'll
        // close the ask + decrement stats post-dispatch. Otherwise
        // it degrades to Message (or stays Message for unsolicited).
        let (kind, reply_target_session) =
            classify_workers_tell(&*self.facade, &caller_key, &args.label, in_reply_to_id.as_ref());

        let correlation_id = CorrelationId::new_tell();
        let identity = self.facade.caller_identity(&caller_key);
        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind,
            sender_name: identity.name,
            sender_org: identity.org,
            hop: 1,
            hop_limit: HOP_LIMIT,
            body: args.message,
        };

        // Reserved keyword: `label="lead"` routes back to the caller's
        // lead session (resolved from the worker's
        // `spawned_by_session_id`). Lead callers get a clear error -
        // leads don't have a lead. See `LEAD_LABEL` in
        // `mcp::workers::facade`.
        let delivery_ok = if args.label == LEAD_LABEL {
            match self.facade.deliver_prompt_to_lead(&caller_key, wrapped) {
                Ok(_) => true,
                Err(err) => return tool_error(format_lead_deliver_error(&err)),
            }
        } else {
            match self.facade.deliver_worker_prompt(&caller_key, &args.label, wrapped) {
                Ok(_) => true,
                Err(err) => return tool_error(format_deliver_error(&args.label, &err)),
            }
        };

        // Post-dispatch close-out for clean replies. Mirrors peers
        // `tell_agent` behaviour: remove the inflight entry + bump
        // both sides' counters so the asker's outgoing balances
        // their original ask and the replier's incoming balances
        // their inbound question. Skip when the tell was unsolicited
        // or degraded.
        if delivery_ok
            && matches!(kind, WrappedKind::Reply)
            && let (Some(id), Some(target)) =
                (in_reply_to_id.as_ref(), reply_target_session.as_ref())
        {
            self.facade.complete_inflight_ask(id);
            self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::IncomingMinus1);
            self.facade.bump_inflight_stats(target, PeerStatsDelta::OutgoingMinus1);
        }

        deliver_ok_response(&correlation_id)
    }
}

/// Classify a `workers__tell` based on the optional `in_reply_to`.
///
/// - No `in_reply_to` → `(Message, None)` - unsolicited.
/// - `in_reply_to` resolves to an InflightAsk AND the resolved
///   target session of this tell == the original ask's caller →
///   `(Reply, Some(asker_session))` - clean reply.
/// - `in_reply_to` provided but resolves to nothing, or resolves to
///   an ask whose caller doesn't match this tell's target →
///   `(Message, None)` with a warn log (degraded reply). Same
///   semantics as peers `classify_tell`.
fn classify_workers_tell(
    facade: &dyn WorkerFacade,
    caller_key: &crate::SessionKey,
    target_label: &str,
    in_reply_to: Option<&CorrelationId>,
) -> (WrappedKind, Option<crate::SessionKey>) {
    let Some(id) = in_reply_to else {
        return (WrappedKind::Message, None);
    };
    let Some(ask) = facade.resolve_correlation(id) else {
        tracing::warn!(
            target: "forge_workspace::mcp::workers",
            correlation_id = id.as_str(),
            target_label,
            "tell in_reply_to references unknown correlation_id; degrading to Message"
        );
        return (WrappedKind::Message, None);
    };
    // Resolve the tell's target to a session_key and compare with
    // the original ask's caller. For label="lead": resolve via the
    // caller's spawned_by_session_id. For other labels: live-workers
    // lookup in the caller's project.
    let target_session = resolve_target_session(facade, caller_key, target_label);
    match target_session {
        Some(target) if target == ask.caller => (WrappedKind::Reply, Some(target)),
        Some(target) => {
            tracing::warn!(
                target: "forge_workspace::mcp::workers",
                correlation_id = id.as_str(),
                target_label,
                target_session = %target.as_str(),
                ask_caller = %ask.caller.as_str(),
                "tell in_reply_to target mismatch (label resolves to a different session than the original asker); degrading to Message"
            );
            (WrappedKind::Message, None)
        }
        None => {
            tracing::warn!(
                target: "forge_workspace::mcp::workers",
                correlation_id = id.as_str(),
                target_label,
                "tell in_reply_to could not resolve target label to a session; degrading to Message"
            );
            (WrappedKind::Message, None)
        }
    }
}

/// Resolve a `workers__tell` target label to a concrete session key.
/// `LEAD_LABEL` → the caller's `spawned_by_session_id`. Other labels
/// → look up the latest-spawned live worker in the caller's project
/// matching the label. Returns `None` when the lookup fails (caller
/// has no worker entry, label not live, etc.).
fn resolve_target_session(
    facade: &dyn WorkerFacade,
    caller_key: &crate::SessionKey,
    target_label: &str,
) -> Option<crate::SessionKey> {
    let cp = facade.caller_project(caller_key)?;
    if target_label == LEAD_LABEL {
        // Caller must be a worker for "lead" addressing to mean
        // anything; for leads, this target is undefined and the
        // deliver path will reject the call anyway.
        if cp.is_lead {
            return None;
        }
        return facade
            .list_workers(caller_key)
            .into_iter()
            .find(|w| w.session_id == caller_key.as_str())
            .map(|w| crate::SessionKey::from_session_id(w.spawned_by_session_id));
    }
    // Sibling-worker / cross-worker addressing. Use the same
    // latest-spawned-wins rule as the dispatcher.
    facade
        .list_workers(caller_key)
        .into_iter()
        .rev()
        .find(|w| w.label == target_label)
        .map(|w| crate::SessionKey::from_session_id(w.session_id))
}

/// Helper: build the standard `{ correlation_id, status: "delivered" }`
/// response body. Pulled out so both Tell + Ask + lead-delivery paths
/// emit a single canonical shape.
fn deliver_ok_response(correlation_id: &CorrelationId) -> ToolOutput {
    let body = serde_json::json!({
        "correlation_id": correlation_id.as_str(),
        "status": "delivered",
    });
    match serde_json::to_string_pretty(&body) {
        Ok(json) => ToolOutput::text(json),
        Err(err) => tool_error(format!("response serialization failed: {err}")),
    }
}

/// LLM-facing error text for `WorkerLeadDeliverError`. Mirrors
/// `format_deliver_error`'s shape so the worker LLM gets the same
/// flavor of guidance regardless of which addressing path it took.
fn format_lead_deliver_error(err: &WorkerLeadDeliverError) -> String {
    match err {
        WorkerLeadDeliverError::UnknownCaller => {
            "could not resolve caller to a known worker session (forge bug)".to_owned()
        }
        WorkerLeadDeliverError::LeadCallerHasNoLead => {
            "label='lead' is worker-only; this session is a project lead and has no lead \
             to talk back to. Use peers__* to reach another project's lead, or \
             workers__tell/ask with a real worker label."
                .to_owned()
        }
        WorkerLeadDeliverError::LeadGone { lead_session_id } => format!(
            "lead session '{lead_session_id}' is no longer live. The lead closed since this \
             worker was spawned; the worker pool will cascade-close shortly."
        ),
        WorkerLeadDeliverError::HopLimitExceeded { hop, limit } => format!(
            "hop limit exceeded forwarding to lead ({hop}/{limit}). The chain has reached \
             its maximum depth - your message will not be forwarded."
        ),
    }
}

fn format_deliver_error(label: &str, err: &WorkerDeliverError) -> String {
    match err {
        WorkerDeliverError::UnknownLabel { project_key, .. } => format!(
            "no live worker with label '{label}' in project '{project_key}'. Call \
             workers__list to discover the current worker pool."
        ),
        WorkerDeliverError::HopLimitExceeded { hop, limit } => format!(
            "hop limit exceeded forwarding to worker '{label}' ({hop}/{limit}). The \
             chain has reached its maximum depth - your message will not be forwarded."
        ),
    }
}

/// `workers__ask` - async question to a worker in the caller's
/// project. Returns immediately with a correlation_id; the reply
/// lands later as a fresh user-turn injection in the caller's chat
/// when the target worker replies. Mirrors peer-MCP's AskAgent
/// behavior (return immediately, reply arrives asynchronously).
///
/// Arguments:
/// - `label` (string, required) - worker label from `workers__list`
/// - `question` (string, required) - the question body
///
/// Returns a JSON object with `correlation_id` (starts with `q-`)
/// and a `delivered` status string.
pub(crate) struct Ask {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct AskArgs {
    label: String,
    question: String,
}

#[async_trait::async_trait]
impl Tool for Ask {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "workers__ask"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Ask a worker in YOUR project a question and receive their \
         reply asynchronously. Returns IMMEDIATELY with a \
         correlation_id (e.g. q-7f3a92e0); this tool does NOT wait \
         for the reply. The target's LLM will see your question as a \
         new user turn, do its work, and respond. The reply lands as \
         a fresh user turn in YOUR chat whenever it's ready - finish \
         your current turn naturally and continue with other work. \
         Multiple asks can run in parallel - fire several workers__ask \
         calls in one turn and the replies arrive independently. The \
         reserved label 'lead' targets the caller's lead (worker-only \
         - use it to ask the spawning lead for direction; project \
         leads have no lead and will get an error). Available to both \
         lead and worker callers (apart from the 'lead' case). If \
         multiple workers share the label, addressing picks the \
         latest-spawned. Run workers__list first to confirm the label \
         and that the worker is live."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Worker label from workers__list. Case-sensitive. If multiple workers share the label, the latest-spawned receives the question.",
                },
                "question": {
                    "type": "string",
                    "description": "Question body. Rendered as a new user turn in the worker's chat - write it as a direct request. Include enough context for the worker to answer without further round-trips.",
                },
            },
            "required": ["label", "question"],
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
        let correlation_id = CorrelationId::new_ask();

        let identity = self.facade.caller_identity(&caller_key);
        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind: WrappedKind::Question,
            sender_name: identity.name,
            sender_org: identity.org,
            hop: 1,
            hop_limit: HOP_LIMIT,
            body: args.question,
        };

        // Resolve the caller's project so we can stamp a composite
        // `target_project` field on the InflightAsk that is unique to
        // worker traffic. Worker labels are scoped to a project, so
        // `<project_key>::<label>` is the natural disambiguator -
        // and the `::` separator can never appear in a real project
        // name (it's a forge.toml validation constraint), so the
        // composite cannot collide with the peer-MCP target_project
        // shape (a bare project name). expire_inflight_*_for_worker
        // matches on this composite when a worker closes.
        let caller_project_key = self
            .facade
            .caller_project(&caller_key)
            .map(|cp| cp.project_key.as_str().to_owned())
            .unwrap_or_default();
        let target_project_composite = worker_target_project_key(&caller_project_key, &args.label);

        // Register the inflight ask BEFORE dispatching delivery so a
        // fast-path worker (idle, processes the prompt immediately on
        // its next turn) can't fire a reply that hits
        // `resolve_correlation` before the ask is in the map. On
        // dispatch failure we roll back the registration so the
        // inflight map doesn't leak. Same race-safety pattern as
        // peer-MCP's AskAgent.
        self.facade.register_inflight_ask(InflightAsk {
            correlation_id: correlation_id.clone(),
            caller: caller_key.clone(),
            caller_project: caller_project_key,
            target_project: target_project_composite,
        });
        // Bump the caller's outgoing counter. Mirrors peers__ask_agent
        // so the sidebar badge reflects "I have N asks awaiting reply"
        // regardless of whether the asks went peer-ward or
        // worker-ward. Decrement fires when the recipient's
        // `workers__tell` with `in_reply_to` closes the ask.
        self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::OutgoingPlus1);

        // Reserved keyword: `label="lead"` routes back to the caller's
        // lead session (the worker's spawner). Workers ask their lead
        // for project-level direction; leads can't use this addressing
        // (no lead above them).
        if args.label == LEAD_LABEL {
            return match self.facade.deliver_prompt_to_lead(&caller_key, wrapped) {
                Ok(_) => deliver_ok_response(&correlation_id),
                Err(err) => {
                    // Rollback: delivery never landed. Counter +
                    // inflight both rewind so the map / badge stay
                    // consistent with reality.
                    self.facade.complete_inflight_ask(&correlation_id);
                    self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::OutgoingMinus1);
                    tool_error(format_lead_deliver_error(&err))
                }
            };
        }

        match self.facade.deliver_worker_prompt(&caller_key, &args.label, wrapped) {
            Ok(_) => deliver_ok_response(&correlation_id),
            Err(err) => {
                // Rollback: the dispatch never reached the worker so
                // the inflight_asks entry + outgoing bump would
                // otherwise leak.
                self.facade.complete_inflight_ask(&correlation_id);
                self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::OutgoingMinus1);
                tool_error(format_deliver_error(&args.label, &err))
            }
        }
    }
}

/// `workers__create_role` - lead-only. Writes charter + initial-kick
/// (and, optionally, resume-kick) files for a new role under
/// `~/.claude/forge-team/<label>/`. The next forge restart can then
/// include `<label>` in `forge.toml`'s `team = [...]` to spawn workers
/// with this charter.
///
/// Arguments:
/// - `label` (string, required) - the role label, may contain `/` for
///   namespace subdirectories (e.g. `hub-modules/researcher`). Validated
///   against `..` / `.` / empty segments to prevent path traversal.
/// - `charter` (string, required) - markdown body of the charter. The
///   spawned worker's LLM sees this as a system-prompt addendum.
/// - `initial_kick` (string, required) - the worker's first-turn message
///   on connect. Drives the worker's initial action + report-back.
/// - `resume_kick` (string, optional) - the worker's re-orient message
///   when a session is resumed rather than freshly spawned. When `Some`,
///   writes `<label>/resume-kick.md` alongside the other two. When
///   absent or `None`, the resume-kick file is not written (PR #226's
///   opt-in convention: absent = caller falls back to the default kick
///   or skips per the past-progress guard).
/// - `overwrite` (bool, optional, default false) - if false (default),
///   refuses when ANY of the target files already exist. Set true to
///   replace. Both the existence check and the overwrite scope are
///   limited to the files the call writes: when `resume_kick=None`,
///   an existing `resume-kick.md` is neither read as a collision nor
///   touched. A role whose resume-kick was provisioned manually
///   survives a charter-only refresh that passes `overwrite=true`
///   without `resume_kick`.
///
/// Returns a JSON object:
/// `{ "label", "charter_path", "kick_path", "resume_kick_path"? }`.
/// The `resume_kick_path` field is present iff `resume_kick` was
/// provided; absent (not null) otherwise so callers that don't read it
/// see no change.
pub(crate) struct CreateRole {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct CreateRoleArgs {
    label: String,
    charter: String,
    initial_kick: String,
    #[serde(default)]
    resume_kick: Option<String>,
    #[serde(default)]
    overwrite: bool,
}

#[async_trait::async_trait]
impl Tool for CreateRole {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "workers__create_role"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Create a new engineering-team role by writing its charter and \
         initial-kick files under `~/.claude/forge-team/<label>/`. \
         Optionally writes a `resume-kick.md` companion when \
         `resume_kick` is provided (PR #226's opt-in convention - when \
         present, the file re-orients the worker on session resume \
         instead of using the fresh-spawn kick). Lead-only. The label \
         may contain `/` for namespace subdirectories (e.g. \
         `hub-modules/researcher` writes to \
         `~/.claude/forge-team/hub-modules/researcher/charter.md`). \
         After creation, add the label to `forge.toml`'s \
         `team = [...]` and restart forge to spawn workers with this \
         charter. Refuses by default if any target file already \
         exists; pass `overwrite=true` to replace. The overwrite \
         scope is limited to the files the call writes: when \
         `resume_kick` is omitted, an existing `resume-kick.md` is \
         neither read as a collision nor touched, so a manually \
         provisioned resume-kick survives a charter-only refresh. \
         Validates the label against path-traversal (no `..`, no \
         leading `/`, no empty / `.` segments)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Role label. May contain `/` for namespace subdirectories (e.g. `hub-modules/researcher`). Non-empty after trim. Rejected if it contains `..` / `.` segments or starts with `/`.",
                },
                "charter": {
                    "type": "string",
                    "description": "Markdown body of the role charter. Threaded into the spawned worker's system prompt so the worker LLM understands its mission, inputs, outputs, workflow, boundaries, and anti-patterns. Non-empty after trim.",
                },
                "initial_kick": {
                    "type": "string",
                    "description": "First-turn message dispatched to the worker on Connected. Drives the worker's initial action + report-back. Non-empty after trim.",
                },
                "resume_kick": {
                    "type": "string",
                    "description": "Optional re-orient message used when a worker session is resumed (rather than freshly spawned). When provided, writes `<label>/resume-kick.md` alongside the other two files; when absent or empty after trim, the resume-kick file is not written and the resume path falls back to the default kick (or skips per the past-progress guard). Non-empty after trim when provided.",
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Replace existing files when true. Default false: refuses if ANY target file already exists (charter, kick, or - when `resume_kick` is provided - resume-kick). The overwrite scope is limited to the files the call writes: a `resume_kick=None` call never touches an existing resume-kick.md, even with `overwrite=true`, so a manually provisioned resume-kick survives a charter-only refresh.",
                },
            },
            "required": ["label", "charter", "initial_kick"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: CreateRoleArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };

        let caller_key = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };

        // Lead-only: workers cannot create roles in v1.
        let Some(caller_project) = self.facade.caller_project(&caller_key) else {
            return tool_error(
                "workers__create_role: caller resolves to no known project".to_owned(),
            );
        };
        if !caller_project.is_lead {
            return tool_error(
                "workers__create_role is lead-only; this session is a worker. Workers cannot create roles in v1.".to_owned(),
            );
        }

        // Trim then validate non-empty content.
        let label = args.label.trim().to_owned();
        let charter = args.charter.trim_end().to_owned();
        let initial_kick = args.initial_kick.trim_end().to_owned();
        if charter.is_empty() {
            return tool_error("charter must be non-empty after trim".to_owned());
        }
        if initial_kick.is_empty() {
            return tool_error("initial_kick must be non-empty after trim".to_owned());
        }
        // `resume_kick` is opt-in: absent or `None` -> don't write
        // the file. When provided, hold it to the same non-empty
        // contract the other two prompts use so a stray empty arg
        // doesn't plant a zero-byte file the resume path would then
        // dispatch as the worker's first turn.
        let resume_kick = match args.resume_kick.as_ref() {
            Some(text) => {
                let trimmed = text.trim_end().to_owned();
                if trimmed.is_empty() {
                    return tool_error(
                        "resume_kick must be non-empty after trim when provided".to_owned(),
                    );
                }
                Some(trimmed)
            }
            None => None,
        };

        // Validate label format + reject reserved keyword.
        if label == crate::team::LEAD_LABEL {
            return tool_error(format!(
                "label '{}' is reserved - it addresses the caller's lead via \
                 workers__tell / workers__ask and ships as a built-in default. \
                 Pick a different label.",
                crate::team::LEAD_LABEL
            ));
        }
        if let Err(err) = crate::team::validate_label(&label) {
            return tool_error(err.to_string());
        }

        let role_dir = match crate::team::role_dir(&label) {
            Ok(d) => d,
            Err(err) => return tool_error(err.to_string()),
        };
        let charter_path = role_dir.join("charter.md");
        let kick_path = role_dir.join("kick.md");
        let resume_kick_path = role_dir.join("resume-kick.md");

        // Refuse when ANY target file exists + overwrite=false. The
        // resume-kick path only joins the existence-check when
        // `resume_kick` was supplied; absent input means we're not
        // writing that file, so a pre-existing resume-kick.md is none
        // of our business.
        if !args.overwrite {
            let resume_collision = resume_kick.is_some() && resume_kick_path.exists();
            if charter_path.exists() || kick_path.exists() || resume_collision {
                return tool_error(format!(
                    "role '{label}' already has one or more target files at {}. Pass overwrite=true to replace.",
                    role_dir.display()
                ));
            }
        }

        // Create parent dir (including namespace subdirectories).
        if let Err(err) = std::fs::create_dir_all(&role_dir) {
            return tool_error(format!(
                "failed to create role directory {}: {err}",
                role_dir.display()
            ));
        }
        if let Err(err) = atomic_write(&charter_path, charter.as_bytes()) {
            return tool_error(format!(
                "failed to write charter at {}: {err}",
                charter_path.display()
            ));
        }
        if let Err(err) = atomic_write(&kick_path, initial_kick.as_bytes()) {
            return tool_error(format!("failed to write kick at {}: {err}", kick_path.display()));
        }
        if let Some(ref text) = resume_kick
            && let Err(err) = atomic_write(&resume_kick_path, text.as_bytes())
        {
            return tool_error(format!(
                "failed to write resume-kick at {}: {err}",
                resume_kick_path.display()
            ));
        }

        let mut body = serde_json::json!({
            "label": label,
            "charter_path": charter_path.display().to_string(),
            "kick_path": kick_path.display().to_string(),
        });
        if resume_kick.is_some()
            && let Some(map) = body.as_object_mut()
        {
            map.insert(
                "resume_kick_path".to_owned(),
                serde_json::Value::String(resume_kick_path.display().to_string()),
            );
        }
        match serde_json::to_string_pretty(&body) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("response serialization failed: {err}")),
        }
    }
}

/// Write-then-rename for crash-safe file replacement. The temp file
/// lives in the same directory as the final path so rename is
/// guaranteed-atomic on the same filesystem.
fn atomic_write(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let temp = parent.join(format!(
        ".{}.tmp",
        path.file_name().map_or("write", |f| f.to_str().unwrap_or("write"))
    ));
    std::fs::write(&temp, contents)?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionKey;
    use crate::mcp::workers::facade::{CallerProject, MockWorkerFacade};
    use crate::protocol::WorkerSpawnReply;

    fn fake_key(s: &str) -> SessionKey {
        SessionKey::from_session_id(s)
    }

    fn lead_caller(name: &str) -> CallerProject {
        CallerProject { project_key: crate::ProjectKey::new(name), is_lead: true }
    }

    fn worker_caller(name: &str) -> CallerProject {
        CallerProject { project_key: crate::ProjectKey::new(name), is_lead: false }
    }

    #[tokio::test]
    async fn spawn_lead_caller_returns_session_id_and_tag() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "new-uuid".into(),
            tag: "forge:worker:reviewer".into(),
        }));
        let facade = mock.into_arc();
        let tool =
            Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "reviewer",
                    "charter": "Review every diff before merge.",
                }),
            })
            .await;
        assert!(!output.is_error, "spawn happy path should not error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert_eq!(parsed["session_id"], "new-uuid");
        assert_eq!(parsed["tag"], "forge:worker:reviewer");
    }

    #[tokio::test]
    async fn spawn_non_lead_caller_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("worker-key"), worker_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("worker-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "nested",
                    "charter": "should fail",
                }),
            })
            .await;
        assert!(output.is_error, "non-lead caller must surface as is_error");
        assert!(
            output.blocks[0].text.to_lowercase().contains("lead-only"),
            "error body should explain lead-only restriction, got: {}",
            output.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn spawn_empty_label_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "   ",
                    "charter": "non-empty",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.to_lowercase().contains("label"));
    }

    #[tokio::test]
    async fn spawn_empty_charter_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "reviewer",
                    "charter": "",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.to_lowercase().contains("charter"));
    }

    #[tokio::test]
    async fn spawn_invalid_args_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    // missing required 'charter'
                    "label": "reviewer",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.to_lowercase().contains("invalid"));
    }

    #[test]
    fn spawn_metadata_shape() {
        let mock = MockWorkerFacade::new();
        let facade = mock.into_arc();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("k")) };
        assert_eq!(tool.name(), "workers__spawn");
        assert!(tool.description().to_lowercase().contains("worker"));
        let schema = tool.input_schema();
        let required = schema["required"].as_array().expect("required field present");
        assert!(required.iter().any(|v| v == "label"));
        assert!(required.iter().any(|v| v == "charter"));
    }

    fn fake_worker(label: &str, charter: &str) -> forge_primitives::WorkerStatus {
        forge_primitives::WorkerStatus {
            label: label.to_owned(),
            charter: charter.to_owned(),
            status: forge_primitives::WorkerLiveness::Running,
            session_id: format!("session-{label}"),
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".to_owned(),
            diagnostic: None,
        }
    }

    #[tokio::test]
    async fn list_returns_workers_for_caller_project() {
        let mock = MockWorkerFacade::new();
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        mock.workers.lock().insert(
            "forge".into(),
            vec![
                fake_worker("reviewer", "Review every diff."),
                fake_worker("tester", "Run all tests after changes."),
            ],
        );
        let facade = mock.into_arc();
        let tool = List { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error, "list happy path should not error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let arr = parsed.as_array().expect("output is array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["label"], "reviewer");
        assert_eq!(arr[0]["charter"], "Review every diff.");
        assert_eq!(arr[1]["label"], "tester");
    }

    #[tokio::test]
    async fn list_returns_empty_array_when_no_workers() {
        let mock = MockWorkerFacade::new();
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        // No workers pre-loaded for "forge".
        let facade = mock.into_arc();
        let tool = List { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert_eq!(parsed.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_returns_empty_when_caller_unresolved() {
        let mock = MockWorkerFacade::new();
        // No caller mapping pre-loaded - resolves to None, facade
        // returns an empty Vec.
        let facade = mock.into_arc();
        let tool =
            List { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("ghost-key")) };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert_eq!(parsed.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_works_for_worker_callers_too() {
        // Workers can list - the spec says caller is "any". The mock
        // returns the same project's worker set regardless of lead vs
        // worker, mirroring production semantics.
        let mock = MockWorkerFacade::new();
        let caller = fake_key("worker-key");
        mock.callers.lock().insert(caller.clone(), worker_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("reviewer", "charter")]);
        let facade = mock.into_arc();
        let tool = List { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert_eq!(parsed.as_array().unwrap().len(), 1);
    }

    #[test]
    fn list_metadata_shape() {
        let mock = MockWorkerFacade::new();
        let facade = mock.into_arc();
        let tool = List { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("k")) };
        assert_eq!(tool.name(), "workers__list");
        assert!(tool.description().to_lowercase().contains("list"));
        let schema = tool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tell_dispatches_when_label_matches() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("reviewer", "charter")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "reviewer",
                    "message": "Please look at PR #42.",
                }),
            })
            .await;
        assert!(!output.is_error, "tell happy path should not error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let id = parsed["correlation_id"].as_str().expect("correlation_id present");
        assert!(id.starts_with("t-"), "tell ids prefix t-, got {id}");
        let dispatched = mock.deliver_calls.lock();
        assert_eq!(dispatched.len(), 1, "exactly one deliver dispatch");
        assert_eq!(dispatched[0].1, "reviewer");
        assert!(matches!(dispatched[0].2.kind, WrappedKind::Message));
    }

    #[tokio::test]
    async fn tell_unknown_label_is_error() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "missing",
                    "message": "hi",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.contains("missing"));
        assert_eq!(mock.deliver_calls.lock().len(), 0, "no dispatch when label unknown");
    }

    #[tokio::test]
    async fn tell_worker_caller_succeeds() {
        // Any caller (lead OR worker) may use workers__tell.
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("worker-key");
        mock.callers.lock().insert(caller.clone(), worker_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("peer-worker", "charter")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "peer-worker",
                    "message": "hi from a fellow worker",
                }),
            })
            .await;
        assert!(!output.is_error, "worker callers may tell other workers");
        assert_eq!(mock.deliver_calls.lock().len(), 1);
    }

    #[tokio::test]
    async fn tell_invalid_args_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade = mock.into_arc();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    // missing required 'message'
                    "label": "reviewer",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.to_lowercase().contains("invalid"));
    }

    #[test]
    fn tell_metadata_shape() {
        let mock = MockWorkerFacade::new();
        let facade = mock.into_arc();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("k")) };
        assert_eq!(tool.name(), "workers__tell");
        // Tool description must surface the two shapes (unsolicited
        // vs reply) so the LLM knows about `in_reply_to`.
        assert!(tool.description().to_lowercase().contains("in_reply_to"));
        let schema = tool.input_schema();
        let required = schema["required"].as_array().expect("required field present");
        assert!(required.iter().any(|v| v == "label"));
        // `in_reply_to` is an OPTIONAL property - present in the schema
        // but not in `required`, mirroring peers__tell_agent.
        assert!(schema["properties"].as_object().unwrap().contains_key("in_reply_to"));
        assert!(required.iter().all(|v| v != "in_reply_to"));
        assert!(required.iter().any(|v| v == "message"));
    }

    #[tokio::test]
    async fn ask_registers_inflight_and_dispatches() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("reviewer", "charter")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Ask { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "reviewer",
                    "question": "What's the toolchain?",
                }),
            })
            .await;
        assert!(!output.is_error, "ask happy path should not error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let id = parsed["correlation_id"].as_str().expect("correlation_id present");
        assert!(id.starts_with("q-"), "ask ids prefix q-, got {id}");

        // Registration + dispatch both happened.
        assert_eq!(mock.inflight.lock().len(), 1, "inflight ask registered");
        let dispatched = mock.deliver_calls.lock();
        assert_eq!(dispatched.len(), 1, "exactly one deliver dispatch");
        assert_eq!(dispatched[0].1, "reviewer");
        assert!(matches!(dispatched[0].2.kind, WrappedKind::Question));
    }

    #[tokio::test]
    async fn ask_unknown_label_rolls_back_inflight() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        // No 'missing' worker pre-loaded.
        mock.workers.lock().insert("forge".into(), vec![]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Ask { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "missing",
                    "question": "hi",
                }),
            })
            .await;
        assert!(output.is_error);
        // Race-safety: register-before-dispatch means the entry was
        // briefly there, but the failure-rollback removed it.
        assert_eq!(
            mock.inflight.lock().len(),
            0,
            "failed delivery must roll back the inflight registration"
        );
        assert_eq!(mock.deliver_calls.lock().len(), 0, "no deliver dispatch when label unknown");
    }

    #[tokio::test]
    async fn ask_worker_caller_succeeds() {
        // Workers may also ask other workers.
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("worker-key");
        mock.callers.lock().insert(caller.clone(), worker_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("peer-worker", "charter")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Ask { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "peer-worker",
                    "question": "anything new?",
                }),
            })
            .await;
        assert!(!output.is_error);
        assert_eq!(mock.inflight.lock().len(), 1);
        assert_eq!(mock.deliver_calls.lock().len(), 1);
    }

    #[tokio::test]
    async fn ask_invalid_args_is_error() {
        let mock = Arc::new(MockWorkerFacade::new());
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Ask { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    // missing required 'question'
                    "label": "reviewer",
                }),
            })
            .await;
        assert!(output.is_error);
        // Schema-level rejection happens before any registration.
        assert_eq!(mock.inflight.lock().len(), 0);
    }

    #[test]
    fn ask_metadata_shape() {
        let mock = MockWorkerFacade::new();
        let facade = mock.into_arc();
        let tool = Ask { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("k")) };
        assert_eq!(tool.name(), "workers__ask");
        assert!(tool.description().to_lowercase().contains("asynchronous"));
        let schema = tool.input_schema();
        let required = schema["required"].as_array().expect("required field present");
        assert!(required.iter().any(|v| v == "label"));
        assert!(required.iter().any(|v| v == "question"));
    }

    #[test]
    fn build_server_registers_all_workers_tools() {
        let mock = MockWorkerFacade::new();
        let facade = mock.into_arc();
        let server = build_server(facade, CallerKeyResolver::from_fixed(fake_key("test")));
        let debug = format!("{server:?}");
        for expected in [
            "workers__spawn",
            "workers__list",
            "workers__tell",
            "workers__ask",
            "workers__create_role",
        ] {
            assert!(
                debug.contains(expected),
                "build_server must include {expected}; debug: {debug}",
            );
        }
    }

    // ---------------------------------------------------------------
    // Reserved `"lead"` addressing - workers__spawn rejects the label,
    // workers__tell / workers__ask route to the caller's spawning
    // lead when the label matches.
    // ---------------------------------------------------------------

    /// Helper: build a `WorkerStatus` whose `session_id` matches the
    /// caller's key and whose `spawned_by_session_id` is `lead_uuid`.
    /// `MockWorkerFacade::deliver_prompt_to_lead` reads this entry to
    /// resolve the worker's lead.
    fn worker_with_lead(
        label: &str,
        session_id: &str,
        lead_uuid: &str,
    ) -> forge_primitives::WorkerStatus {
        forge_primitives::WorkerStatus {
            label: label.to_owned(),
            charter: "test charter".to_owned(),
            status: forge_primitives::WorkerLiveness::Running,
            session_id: session_id.to_owned(),
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: lead_uuid.to_owned(),
            diagnostic: None,
        }
    }

    #[tokio::test]
    async fn spawn_reserved_lead_label_is_error() {
        // workers__spawn must reject label='lead' because workers__tell
        // / workers__ask reserve that label for addressing the caller's
        // lead - letting it through would let a worker shadow the
        // reserved keyword.
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "charter": "doesn't matter",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(
            output.blocks[0].text.contains("reserved"),
            "error must mention 'reserved': {:?}",
            output.blocks,
        );
    }

    #[tokio::test]
    async fn spawn_reserved_lead_label_trimmed_is_error() {
        // Trim must happen before the reserved-keyword check so
        // '  lead  ' is still rejected.
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "  lead  ",
                    "charter": "doesn't matter",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.contains("reserved"));
    }

    #[tokio::test]
    async fn tell_label_lead_from_worker_routes_to_lead_delivery() {
        // Worker caller, label='lead' → routes to the
        // deliver_prompt_to_lead facade path. The mock records the
        // delivery under the synthetic label '<lead>' so the assertion
        // is unambiguous about which facade method fired.
        let mock = Arc::new(MockWorkerFacade::new());
        let worker_key = fake_key("worker-uuid");
        mock.callers.lock().insert(worker_key.clone(), worker_caller("forge"));
        mock.workers
            .lock()
            .insert("forge".into(), vec![worker_with_lead("probe-a", "worker-uuid", "lead-uuid")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(worker_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "message": "FYI from the worker",
                }),
            })
            .await;
        assert!(!output.is_error, "worker→lead tell should succeed: {:?}", output.blocks);
        let dispatched = mock.deliver_calls.lock();
        assert_eq!(dispatched.len(), 1, "exactly one delivery");
        assert_eq!(dispatched[0].1, "<lead>", "mock tags lead deliveries with '<lead>'");
    }

    #[tokio::test]
    async fn tell_label_lead_from_lead_is_error() {
        // Lead caller using label='lead' - error. Leads have no lead.
        let mock = Arc::new(MockWorkerFacade::new());
        let lead_key = fake_key("lead-key");
        mock.callers.lock().insert(lead_key.clone(), lead_caller("forge"));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(lead_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "message": "should fail",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(
            output.blocks[0].text.to_lowercase().contains("lead"),
            "error text mentions lead: {:?}",
            output.blocks,
        );
        assert_eq!(mock.deliver_calls.lock().len(), 0, "no delivery on lead-from-lead");
    }

    #[tokio::test]
    async fn ask_label_lead_from_worker_routes_to_lead_and_registers_inflight() {
        // Worker → lead via workers__ask should:
        //  * route through deliver_prompt_to_lead (mock tags '<lead>')
        //  * register the inflight ask BEFORE delivery
        //  * not roll back since delivery succeeded
        let mock = Arc::new(MockWorkerFacade::new());
        let worker_key = fake_key("worker-uuid");
        mock.callers.lock().insert(worker_key.clone(), worker_caller("forge"));
        mock.workers
            .lock()
            .insert("forge".into(), vec![worker_with_lead("probe-a", "worker-uuid", "lead-uuid")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Ask { facade, caller_key: CallerKeyResolver::from_fixed(worker_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "question": "Should I keep going on this branch?",
                }),
            })
            .await;
        assert!(!output.is_error, "worker→lead ask should succeed: {:?}", output.blocks);
        assert_eq!(mock.deliver_calls.lock().len(), 1);
        assert_eq!(mock.deliver_calls.lock()[0].1, "<lead>");
        // Inflight registration survives a successful delivery - only
        // dispatch failures roll back. One ask = one entry.
        assert_eq!(mock.inflight.lock().len(), 1, "inflight registered + retained on success");
    }

    #[tokio::test]
    async fn ask_label_lead_from_lead_is_error_and_rolls_back_inflight() {
        // Lead → label='lead' must error AND roll back the inflight
        // registration so the map doesn't leak. Same race-safety
        // contract as the non-lead error paths.
        let mock = Arc::new(MockWorkerFacade::new());
        let lead_key = fake_key("lead-key");
        mock.callers.lock().insert(lead_key.clone(), lead_caller("forge"));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Ask { facade, caller_key: CallerKeyResolver::from_fixed(lead_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "question": "lead asking itself?",
                }),
            })
            .await;
        assert!(output.is_error);
        assert_eq!(mock.deliver_calls.lock().len(), 0);
        assert_eq!(mock.inflight.lock().len(), 0, "rollback after delivery error");
    }

    #[tokio::test]
    async fn tell_label_lead_when_worker_entry_missing_errors() {
        // Worker caller exists, but no WorkerEntry is preloaded - the
        // mock can't resolve the lead and surfaces UnknownCaller. The
        // production path hits this branch when the worker was just
        // closed between Tool invocation and facade dispatch.
        let mock = Arc::new(MockWorkerFacade::new());
        let worker_key = fake_key("orphan-worker");
        mock.callers.lock().insert(worker_key.clone(), worker_caller("forge"));
        // workers map empty - no entry for this session_id
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(worker_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "message": "trying to reach lead",
                }),
            })
            .await;
        assert!(output.is_error);
        assert_eq!(mock.deliver_calls.lock().len(), 0);
    }

    // ---------------------------------------------------------------
    // `in_reply_to` closes the inflight ask + decrements counters
    // ---------------------------------------------------------------

    /// Helper: register an inflight ask in the mock so a follow-up
    /// `workers__tell(in_reply_to=...)` can classify as a Reply.
    fn register_ask(
        mock: &MockWorkerFacade,
        correlation_id: &str,
        caller: SessionKey,
        caller_project: &str,
        target_composite: &str,
    ) -> CorrelationId {
        let id = CorrelationId(correlation_id.to_owned());
        mock.inflight.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                caller,
                caller_project: caller_project.to_owned(),
                target_project: target_composite.to_owned(),
            },
        );
        id
    }

    #[tokio::test]
    async fn ask_bumps_outgoing_plus_one_on_caller() {
        // workers__ask must mirror peers__ask_agent and stamp
        // OutgoingPlus1 on the caller so the sidebar badge reflects
        // "I have N asks awaiting reply" regardless of channel.
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("worker-uuid");
        mock.callers.lock().insert(caller.clone(), worker_caller("forge"));
        mock.workers
            .lock()
            .insert("forge".into(), vec![worker_with_lead("probe-a", "worker-uuid", "lead-uuid")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Ask { facade, caller_key: CallerKeyResolver::from_fixed(caller.clone()) };
        let _ = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "question": "stat-bump test",
                }),
            })
            .await;
        let bumps = mock.bumps.lock();
        assert!(
            bumps.iter().any(|(k, d)| *k == caller && *d == PeerStatsDelta::OutgoingPlus1),
            "ask must bump OutgoingPlus1 on caller; got {bumps:?}",
        );
    }

    #[tokio::test]
    async fn tell_reply_closes_inflight_and_decrements_both_sides() {
        // Scenario: worker-A asked the lead via workers__ask. Now
        // the lead replies via workers__tell(in_reply_to=q-XXX,
        // label="worker-A"). The reply must:
        //   - close the inflight entry
        //   - bump IncomingMinus1 on the lead (replier)
        //   - bump OutgoingMinus1 on worker-A (original asker)
        let mock = Arc::new(MockWorkerFacade::new());
        let lead_key = fake_key("lead-uuid");
        let worker_key = fake_key("worker-uuid");
        mock.callers.lock().insert(lead_key.clone(), lead_caller("forge"));
        // Pre-load: live workers so the tell can resolve label →
        // session.
        mock.workers
            .lock()
            .insert("forge".into(), vec![worker_with_lead("worker-A", "worker-uuid", "lead-uuid")]);
        // Pre-register: an inflight ask from worker-A → lead.
        let ask_id = register_ask(&mock, "q-deadbeef", worker_key.clone(), "forge", "forge::lead");

        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(lead_key.clone()) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "worker-A",
                    "message": "lead's reply",
                    "in_reply_to": ask_id.as_str(),
                }),
            })
            .await;
        assert!(!output.is_error, "reply tell should succeed: {:?}", output.blocks);
        // Inflight entry removed.
        assert!(
            mock.inflight.lock().get(&ask_id).is_none(),
            "inflight ask must be removed after a clean reply",
        );
        // Both decrements landed.
        let bumps = mock.bumps.lock();
        assert!(
            bumps.iter().any(|(k, d)| *k == lead_key && *d == PeerStatsDelta::IncomingMinus1),
            "lead must get IncomingMinus1; got {bumps:?}",
        );
        assert!(
            bumps.iter().any(|(k, d)| *k == worker_key && *d == PeerStatsDelta::OutgoingMinus1),
            "worker-A (original asker) must get OutgoingMinus1; got {bumps:?}",
        );
    }

    #[tokio::test]
    async fn tell_with_unknown_in_reply_to_degrades_to_message() {
        // in_reply_to set but the correlation_id is not in the
        // inflight map (already completed, never registered, …):
        // degrade to Message - delivery still succeeds, but no
        // decrement fires.
        let mock = Arc::new(MockWorkerFacade::new());
        let lead_key = fake_key("lead-uuid");
        mock.callers.lock().insert(lead_key.clone(), lead_caller("forge"));
        mock.workers
            .lock()
            .insert("forge".into(), vec![worker_with_lead("worker-A", "worker-uuid", "lead-uuid")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(lead_key.clone()) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "worker-A",
                    "message": "stale reply",
                    "in_reply_to": "q-00000000",
                }),
            })
            .await;
        assert!(!output.is_error, "degraded path still delivers");
        // No Incoming/Outgoing Minus1 bumps on the degraded path.
        let bumps = mock.bumps.lock();
        let has_decrement = bumps.iter().any(|(_, d)| {
            *d == PeerStatsDelta::IncomingMinus1 || *d == PeerStatsDelta::OutgoingMinus1
        });
        assert!(!has_decrement, "degraded reply must not decrement: {bumps:?}");
    }

    #[tokio::test]
    async fn tell_with_malformed_in_reply_to_is_error() {
        // Garbage `in_reply_to` value → tool returns is_error before
        // doing any work. Mirrors peers__tell_agent's validation.
        let mock = Arc::new(MockWorkerFacade::new());
        let lead_key = fake_key("lead-uuid");
        mock.callers.lock().insert(lead_key.clone(), lead_caller("forge"));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(lead_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "worker-A",
                    "message": "x",
                    "in_reply_to": "not-a-correlation-id",
                }),
            })
            .await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.contains("in_reply_to"));
    }

    /// RAII guard that restores the prior `forge_team_root` override
    /// on drop so a panicking test doesn't leak a stale tempdir
    /// override onto its neighbours. The Mutex inside
    /// `test_forge_team_root` keeps the override globally consistent
    /// while we hold the guard.
    struct ForgeTeamRootGuard {
        prior: Option<std::path::PathBuf>,
    }

    impl Drop for ForgeTeamRootGuard {
        fn drop(&mut self) {
            crate::team::set_forge_team_root_for_test(self.prior.take());
        }
    }

    fn redirect_forge_team_root(root: std::path::PathBuf) -> ForgeTeamRootGuard {
        let prior = crate::team::set_forge_team_root_for_test(Some(root));
        ForgeTeamRootGuard { prior }
    }

    fn lead_create_role_tool(mock: Arc<MockWorkerFacade>) -> CreateRole {
        let lead_key = fake_key("lead-uuid");
        mock.callers.lock().insert(lead_key.clone(), lead_caller("forge"));
        let facade: Arc<dyn WorkerFacade> = mock;
        CreateRole { facade, caller_key: CallerKeyResolver::from_fixed(lead_key) }
    }

    #[tokio::test]
    async fn create_role_writes_three_files_when_resume_kick_some() {
        // resume_kick present: all three files land + the returned
        // JSON carries `resume_kick_path`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = redirect_forge_team_root(tmp.path().to_path_buf());

        let mock = Arc::new(MockWorkerFacade::new());
        let tool = lead_create_role_tool(mock);
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "researcher",
                    "charter": "Investigate $TOPIC and summarise findings.",
                    "initial_kick": "Pick the first topic from the queue and report back.",
                    "resume_kick": "Resume the in-flight investigation; check your notes first.",
                }),
            })
            .await;

        assert!(!output.is_error, "create_role with resume_kick must succeed: {:?}", output.blocks);

        let charter_disk = tmp.path().join("researcher").join("charter.md");
        let kick_disk = tmp.path().join("researcher").join("kick.md");
        let resume_kick_disk = tmp.path().join("researcher").join("resume-kick.md");
        assert!(charter_disk.exists(), "charter.md must exist on disk");
        assert!(kick_disk.exists(), "kick.md must exist on disk");
        assert!(resume_kick_disk.exists(), "resume-kick.md must exist on disk");
        let body: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("returned JSON parses");
        assert!(body.get("resume_kick_path").is_some(), "JSON must include resume_kick_path");
        assert_eq!(
            body["resume_kick_path"].as_str().unwrap(),
            resume_kick_disk.display().to_string(),
        );
    }

    #[tokio::test]
    async fn create_role_omits_resume_kick_when_none() {
        // No resume_kick arg: only two files written + returned JSON
        // omits `resume_kick_path` (additive contract preserved).
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = redirect_forge_team_root(tmp.path().to_path_buf());

        let mock = Arc::new(MockWorkerFacade::new());
        let tool = lead_create_role_tool(mock);
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "researcher",
                    "charter": "charter body",
                    "initial_kick": "kick body",
                }),
            })
            .await;

        assert!(!output.is_error);
        let resume_kick_disk = tmp.path().join("researcher").join("resume-kick.md");
        assert!(!resume_kick_disk.exists(), "resume-kick.md must NOT be written when arg is None");
        let body: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("returned JSON parses");
        assert!(
            body.get("resume_kick_path").is_none(),
            "JSON must omit resume_kick_path when arg is None: {body:?}",
        );
    }

    #[tokio::test]
    async fn create_role_overwrite_true_replaces_all_three_files() {
        // overwrite=true success path: seed stale charter + kick +
        // resume-kick, call with new content for all three, assert
        // on-disk content matches the new payload. The existing
        // three regression tests cover additive shape + refusal;
        // this one locks the actual replacement behavior.
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = redirect_forge_team_root(tmp.path().to_path_buf());

        let role_dir = tmp.path().join("researcher");
        std::fs::create_dir_all(&role_dir).unwrap();
        std::fs::write(role_dir.join("charter.md"), b"stale charter").unwrap();
        std::fs::write(role_dir.join("kick.md"), b"stale kick").unwrap();
        std::fs::write(role_dir.join("resume-kick.md"), b"stale resume-kick").unwrap();

        let mock = Arc::new(MockWorkerFacade::new());
        let tool = lead_create_role_tool(mock);
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "researcher",
                    "charter": "fresh charter",
                    "initial_kick": "fresh kick",
                    "resume_kick": "fresh resume-kick",
                    "overwrite": true,
                }),
            })
            .await;

        assert!(
            !output.is_error,
            "overwrite=true must succeed when all three exist: {:?}",
            output.blocks
        );
        let charter_disk = std::fs::read_to_string(role_dir.join("charter.md")).unwrap();
        let kick_disk = std::fs::read_to_string(role_dir.join("kick.md")).unwrap();
        let resume_kick_disk = std::fs::read_to_string(role_dir.join("resume-kick.md")).unwrap();
        assert_eq!(charter_disk, "fresh charter");
        assert_eq!(kick_disk, "fresh kick");
        assert_eq!(resume_kick_disk, "fresh resume-kick");
    }

    #[tokio::test]
    async fn create_role_overwrite_refuses_when_any_of_three_exists() {
        // overwrite=false + resume-kick.md already present: refuse.
        // Locks the existence-check covering all three target sites,
        // not just the original two.
        let tmp = tempfile::tempdir().expect("tempdir");
        let _guard = redirect_forge_team_root(tmp.path().to_path_buf());

        // Seed only the resume-kick file in advance; charter + kick
        // are absent. Without the broadened existence-check, the
        // tool would happily overwrite the existing resume-kick.md.
        let role_dir = tmp.path().join("researcher");
        std::fs::create_dir_all(&role_dir).unwrap();
        std::fs::write(role_dir.join("resume-kick.md"), b"prior content").unwrap();

        let mock = Arc::new(MockWorkerFacade::new());
        let tool = lead_create_role_tool(mock);
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "researcher",
                    "charter": "new charter",
                    "initial_kick": "new kick",
                    "resume_kick": "new resume kick",
                }),
            })
            .await;

        assert!(output.is_error, "must refuse when resume-kick.md already exists");
        assert!(
            output.blocks[0].text.contains("overwrite=true"),
            "error message must hint at overwrite=true: {:?}",
            output.blocks[0].text,
        );
        // And the prior content is untouched.
        let kept =
            std::fs::read_to_string(role_dir.join("resume-kick.md")).expect("prior content kept");
        assert_eq!(kept, "prior content");
    }
}
