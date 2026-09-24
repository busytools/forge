//! The `agents__*` family - one address space for every forge session.
//!
//! A target is a slot: `(org, project, label)`, with `lead` reserved
//! for a project's own agent. Four verbs reach any slot from any
//! session; the rest act on the caller's own project and are lead-only.

pub mod facade;
pub mod target;

use std::sync::Arc;

use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use crate::SessionSlot;
use crate::mcp::agents::facade::AgentDispatcher;
use crate::mcp::agents::target::{AgentTarget, TargetError};
use crate::mcp::peers::facade::PeerStatsDelta;
use crate::mcp::peers::types::{
    AskChannel, CorrelationId, InflightAsk, PeerStatus, WrappedKind, WrappedPrompt,
};

/// Attach the four any-caller verbs to an existing [`McpServerBuilder`].
pub(crate) fn add_shared_tools(
    builder: forge_sdk::mcp::server::McpServerBuilder,
    dispatcher: Arc<AgentDispatcher>,
    slot: SessionSlot,
) -> forge_sdk::mcp::server::McpServerBuilder {
    let whoami = Whoami { dispatcher: dispatcher.clone(), slot: slot.clone() };
    let list = List { dispatcher: dispatcher.clone(), slot: slot.clone() };
    let tell = Tell { dispatcher: dispatcher.clone(), slot: slot.clone() };
    let ask = Ask { dispatcher, slot };
    builder.tool(whoami).tool(list).tool(tell).tool(ask)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![ToolOutputBlock { text }], is_error: true }
}

fn json_output(body: &serde_json::Value) -> ToolOutput {
    match serde_json::to_string_pretty(body) {
        Ok(json) => ToolOutput::text(json),
        Err(err) => tool_error(format!("response serialization failed: {err}")),
    }
}

/// The slot an agent is addressed by, which is the shape every row and
/// every response carries.
fn slot_json(slot: &SessionSlot) -> serde_json::Value {
    serde_json::json!({
        "org": slot.org(),
        "project": slot.project(),
        "label": slot.label(),
    })
}

/// A snapshot row: the address to pass back to tell / ask, then
/// whatever the engine that produced the snapshot knows about the seat.
fn row(slot: &SessionSlot, detail: &impl serde::Serialize) -> Result<serde_json::Value, String> {
    let mut body =
        serde_json::to_value(detail).map_err(|err| format!("snapshot serialization: {err}"))?;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("slot".to_owned(), slot_json(slot));
    }
    Ok(body)
}

/// LLM-facing explanation of a target that did not resolve. Names the
/// reachable set, because a target that refuses is usually a typo.
fn target_error_message(err: &TargetError, known: &[PeerStatus]) -> String {
    match err {
        TargetError::Malformed { component } => format!(
            "the target's {component} is empty. Address an agent as (org, project, label), \
             where label names a worker and may be omitted for a project's own agent."
        ),
        TargetError::UnknownProject { org, project } => format!(
            "no project '{project}' is configured under org '{org}'. Call agents__list to see \
             the agents you can reach. Configured: {}.",
            known.iter().map(|p| format!("{}/{}", p.org, p.name)).collect::<Vec<_>>().join(", "),
        ),
    }
}

#[derive(serde::Deserialize)]
struct TargetArgs {
    org: String,
    project: String,
    #[serde(default)]
    label: Option<String>,
}

impl TargetArgs {
    fn resolve(&self, known: &[PeerStatus]) -> Result<AgentTarget, String> {
        AgentTarget::parse(known, &self.org, &self.project, self.label.as_deref())
            .map_err(|err| target_error_message(&err, known))
    }
}

/// `agents__whoami` - the caller's own slot and identity. No args.
pub(crate) struct Whoami {
    pub(crate) dispatcher: Arc<AgentDispatcher>,
    pub(crate) slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for Whoami {
    fn name(&self) -> &'static str {
        "agents__whoami"
    }

    fn description(&self) -> &'static str {
        "Returns your own forge identity: the slot you are addressed by \
         (org, project, label) and what forge knows about it - project \
         path, current status, and your in-flight ask counters. Useful \
         when an inbound envelope says 'from agent X' and you want to \
         confirm whether X is you, when you need to tell another agent \
         which slot to answer, or when you need your own org and project \
         names to address someone else. Identity is stable for the \
         session, so most callers need this only once. Takes no \
         arguments."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        let Some(identity) = self.dispatcher.peers().whoami(&self.slot) else {
            return tool_error(format!(
                "no identity resolved for caller {} (this is a forge bug; the caller slot \
                 should always resolve to a forge.toml project)",
                self.slot.display(),
            ));
        };
        match row(&self.slot, &identity) {
            Ok(body) => json_output(&body),
            Err(err) => tool_error(err),
        }
    }
}

/// `agents__list` - every seat you can address, optionally narrowed to
/// one project. No target args.
pub(crate) struct List {
    pub(crate) dispatcher: Arc<AgentDispatcher>,
    pub(crate) slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct ListArgs {
    #[serde(default)]
    project: Option<String>,
}

#[async_trait::async_trait]
impl Tool for List {
    fn name(&self) -> &'static str {
        "agents__list"
    }

    fn description(&self) -> &'static str {
        "List the agents you can reach, one row per seat, each addressed \
         by its slot (org, project, label). Two kinds of row: every \
         project's own agent, which has label 'lead', and the live \
         workers of YOUR project, each under its own label. Pass \
         `project` to narrow the list to one project. \
         \
         A project's own agent is reachable whether or not it is \
         currently running: a sleeping project's agent is spawned by the \
         first ask or tell it receives. A worker row must be live for \
         the ask or tell to land. \
         \
         Rows differ in what they carry - a project's agent reports its \
         path and liveness, a worker reports its charter, current \
         activity and session id. Every row carries the slot to pass \
         back to agents__tell / agents__ask. \
         \
         CROSS-PROJECT RULE (mutations only): whenever the user asks you \
         to CHANGE state in a project other than your own - edit files, \
         run a command, file an issue, push a branch, anything with side \
         effects - call this tool FIRST. If the target project appears \
         here, do NOT cd into it and mutate its files directly. Hand the \
         work off via agents__ask (when you need an answer or \
         confirmation back) or agents__tell (for a notification or \
         fire-and-forget hand-off). Each agent owns its own repo; stay in \
         your lane and let the other project's agent execute the change. \
         \
         Reading another project's files for context is fine - sometimes \
         scanning the source yourself gives a sharper answer than waiting \
         on an ask. The constraint is only on writes / state changes. \
         \
         Takes no arguments unless you want the `project` filter."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "project": {
                    "type": "string",
                    "description": "Optional. Project name to narrow the list to. Case-sensitive; omit to list every project.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ListArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let filter = args.project.as_deref();

        let mut rows = Vec::new();
        for project in self.dispatcher.peers().list_peers() {
            if filter.is_some_and(|name| name != project.name) {
                continue;
            }
            let slot = SessionSlot::lead(&project.org, &project.name);
            match row(&slot, &project) {
                Ok(row) => rows.push(row),
                Err(err) => return tool_error(err),
            }
        }
        for worker in self.dispatcher.workers().list_workers(&self.slot) {
            if filter.is_some_and(|name| name != worker.slot.project()) {
                continue;
            }
            match row(&worker.slot, &worker) {
                Ok(row) => rows.push(row),
                Err(err) => return tool_error(err),
            }
        }
        json_output(&serde_json::Value::Array(rows))
    }
}

/// `agents__tell` - a one-way message to any seat, or a reply to an
/// earlier ask.
pub(crate) struct Tell {
    pub(crate) dispatcher: Arc<AgentDispatcher>,
    pub(crate) slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct TellArgs {
    #[serde(flatten)]
    target: TargetArgs,
    message: String,
    #[serde(default)]
    in_reply_to: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Tell {
    fn name(&self) -> &'static str {
        "agents__tell"
    }

    fn description(&self) -> &'static str {
        "Send a one-way message to another forge agent - its own agent, \
         or a named worker - and return immediately. The target is a \
         slot: `project` and `org` name it, and `label` names the seat \
         inside that project. Omit `label` to reach the project's own \
         agent; set it to a worker's label to reach that worker. Run \
         agents__list first if you do not know the label. \
         \
         Two shapes: (1) REPLY to an inbound agents__ask - set \
         in_reply_to to the correlation_id from that ask's envelope, and \
         the original asker sees your message rendered as a Reply in its \
         own chat, wherever it lives; (2) UNSOLICITED - omit in_reply_to \
         to send standalone prose (announcements, an FYI, a hand-off). \
         The target sees the message as a new user turn and may respond \
         by asking or telling you back, or simply continue its own work. \
         A request addressed to another project's own agent is delivered \
         to that project's agent, spawning it first if it was sleeping. \
         \
         Use this instead of mutating another project's files directly \
         whenever the user asks you to notify or hand off work to another \
         project - e.g. \\\"let the gateway backend know the rewriter \
         cleanup landed\\\", \\\"ask forge to pick this up next \
         session\\\". Reading another project's files for your own context \
         is still allowed; only state changes and hand-offs go through \
         this tool. \
         \
         A `delivered` status means the queue ACCEPTED the message, not \
         that the target read it - a target that is down or wedged still \
         returns delivered, so confirm real work happened by a reply or \
         an observable artifact rather than by the ack."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "org": {
                    "type": "string",
                    "description": "Org the target project belongs to, as shown by agents__list. Case-sensitive.",
                },
                "project": {
                    "type": "string",
                    "description": "Project name of the target, as shown by agents__list. Case-sensitive. Your own project's name addresses a seat in your own project.",
                },
                "label": {
                    "type": "string",
                    "description": "Optional. Which seat inside the project: a worker's label, or 'lead' for the project's own agent. Omit for the project's own agent. Case-sensitive; if several workers share a label, the latest-spawned receives the message.",
                },
                "message": {
                    "type": "string",
                    "description": "The message body. Rendered as a new user turn in the target's chat, so write it as you would address the target directly.",
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Optional. Set to the correlation_id (q-XXXXXXXX) of an inbound agents__ask to mark this as a reply. The original asker sees it as a Reply envelope and the ask closes. Omit for unsolicited messages.",
                },
            },
            "required": ["org", "project", "message"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: TellArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let known = self.dispatcher.peers().list_peers();
        let target = match args.target.resolve(&known) {
            Ok(target) => target,
            Err(message) => return tool_error(message),
        };
        // A malformed id would miss the inflight-map lookup silently and
        // degrade a reply to a plain message, hiding the real problem.
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
        let own_project =
            target.org() == self.slot.org() && target.project() == self.slot.project();

        // A reply routes to whichever session asked, by slot: with one
        // family there is no second channel for it to have arrived on.
        if let Some(id) = in_reply_to_id.as_ref()
            && let Some(ask) = self.dispatcher.workers().resolve_correlation(id)
        {
            let (sender_name, sender_org) = self.identity(own_project);
            let wrapped = WrappedPrompt {
                correlation_id: correlation_id.clone(),
                kind: WrappedKind::Reply,
                channel: ask.channel,
                sender_name,
                sender_org,
                body: args.message,
            };
            if let Err(err) =
                self.dispatcher.workers().deliver_reply_to_caller(&ask.caller, &wrapped)
            {
                return tool_error(err.user_message());
            }
            self.dispatcher.workers().complete_inflight_ask(id);
            self.dispatcher
                .workers()
                .bump_inflight_stats(&self.slot, PeerStatsDelta::IncomingMinus1);
            self.dispatcher
                .workers()
                .bump_inflight_stats(&ask.caller, PeerStatsDelta::OutgoingMinus1);
            return Self::delivered_response(&correlation_id, &target, "delivered", None);
        }

        let note = in_reply_to_id.as_ref().map(|id| {
            format!(
                "in_reply_to {id} did not match an open ask (it may be stale or already \
                 answered), so this was delivered as a plain message rather than a reply. \
                 Re-check the correlation id if you meant to reply."
            )
        });
        let (sender_name, sender_org) = self.identity(own_project);
        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind: WrappedKind::Message,
            channel: if own_project { AskChannel::Workers } else { AskChannel::Peers },
            sender_name,
            sender_org,
            body: args.message,
        };
        match self.dispatcher.deliver(&self.slot, &target, wrapped) {
            Ok(status) => Self::delivered_response(&correlation_id, &target, status, note),
            Err(message) => tool_error(message),
        }
    }
}

impl Tell {
    /// The name and org the recipient's chat renders as the sender.
    /// Each engine already answers this for its own path, so a message
    /// reads the same as it did before the two families merged.
    fn identity(&self, own_project: bool) -> (String, String) {
        if own_project {
            let identity = self.dispatcher.workers().caller_identity(&self.slot);
            (identity.name, identity.org)
        } else {
            match self.dispatcher.peers().whoami(&self.slot) {
                Some(identity) => (identity.name, identity.org),
                None => (self.slot.label().to_owned(), String::new()),
            }
        }
    }

    fn delivered_response(
        correlation_id: &CorrelationId,
        target: &AgentTarget,
        status: &str,
        note: Option<String>,
    ) -> ToolOutput {
        let mut body = serde_json::json!({
            "correlation_id": correlation_id.as_str(),
            "target_status": status,
            "slot": slot_json(&SessionSlot::new(target.org(), target.project(), target.label())),
        });
        if let Some(note) = note
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("note".to_owned(), serde_json::Value::String(note));
        }
        json_output(&body)
    }
}

/// `agents__ask` - an async question to any seat. The reply lands in
/// the asking session's own chat.
pub(crate) struct Ask {
    pub(crate) dispatcher: Arc<AgentDispatcher>,
    pub(crate) slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct AskArgs {
    #[serde(flatten)]
    target: TargetArgs,
    prompt: String,
}

#[async_trait::async_trait]
impl Tool for Ask {
    fn name(&self) -> &'static str {
        "agents__ask"
    }

    fn description(&self) -> &'static str {
        "Ask another forge agent - its own agent, or a named worker - a \
         question and receive the reply asynchronously. The target is a \
         slot: `org` and `project` name it, and `label` names the seat \
         inside that project. Omit `label` to ask the project's own \
         agent; set it to a worker's label to ask that worker. Run \
         agents__list first if you do not know the label. \
         \
         Returns IMMEDIATELY with a correlation_id (for example \
         q-7f3a92e0); this tool does NOT wait for the reply. The target's \
         LLM sees your prompt as a new user turn, does its work - \
         possibly seconds, possibly minutes - and responds by calling \
         agents__tell with in_reply_to set to your correlation_id. That \
         reply lands as a fresh user turn in YOUR chat whenever it is \
         ready, so finish your current turn naturally and continue with \
         other work; the reply surfaces on its own. Multiple asks can run \
         in parallel - fire several in one turn and the replies arrive \
         independently, each carrying its own correlation_id. \
         \
         Use this whenever you need another agent to TAKE AN ACTION or \
         give you an authoritative answer that only that agent should \
         produce - running a build there, kicking off a migration there, \
         confirming whether a deploy landed, asking it to review a design \
         from its own context. Reading the target's files for your own \
         context is fine and often quicker than waiting on an ask; the \
         rule is only that state changes happen through the target's own \
         agent, via this tool. \
         \
         A request addressed to another project's own agent spawns that \
         project first if it was sleeping, so expect extra latency on the \
         first ask to a sleeping project. A request addressed to a worker \
         reaches it only while that worker is live; \
         if the label is gone the call fails immediately and names the \
         label."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "org": {
                    "type": "string",
                    "description": "Org the target project belongs to, as shown by agents__list. Case-sensitive.",
                },
                "project": {
                    "type": "string",
                    "description": "Project name of the target, as shown by agents__list. Case-sensitive. Your own project's name addresses a seat in your own project.",
                },
                "label": {
                    "type": "string",
                    "description": "Optional. Which seat inside the project: a worker's label, or 'lead' for the project's own agent. Omit for the project's own agent. Case-sensitive; if several workers share a label, the latest-spawned receives the question.",
                },
                "prompt": {
                    "type": "string",
                    "description": "The question body. Rendered as a new user turn in the target's chat - write it as a direct request. Include enough context that the target can answer without further round-trips.",
                },
            },
            "required": ["org", "project", "prompt"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: AskArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let known = self.dispatcher.peers().list_peers();
        let target = match args.target.resolve(&known) {
            Ok(target) => target,
            Err(message) => return tool_error(message),
        };

        let correlation_id = CorrelationId::new_ask();
        let own_project =
            target.org() == self.slot.org() && target.project() == self.slot.project();
        let (sender_name, sender_org) = if own_project {
            let identity = self.dispatcher.workers().caller_identity(&self.slot);
            (identity.name, identity.org)
        } else {
            match self.dispatcher.peers().whoami(&self.slot) {
                Some(identity) => (identity.name, identity.org),
                None => (self.slot.label().to_owned(), String::new()),
            }
        };
        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind: WrappedKind::Question,
            channel: if own_project { AskChannel::Workers } else { AskChannel::Peers },
            sender_name,
            sender_org,
            body: args.prompt,
        };

        // Register before dispatching: a target that is already running
        // can answer before the registration would otherwise land, and
        // an unresolved reply degrades silently to a plain message.
        // Roll back on a refused dispatch so the inflight map and the
        // outgoing counter do not leak an ask nobody will answer.
        let project_key = self
            .dispatcher
            .workers()
            .caller_project(&self.slot)
            .map(|cp| cp.project_key.as_str().to_owned())
            .unwrap_or_default();
        let target_project = if own_project {
            crate::mcp::workers::worker_target_project_key(&project_key, target.label())
        } else {
            target.project().to_owned()
        };
        self.dispatcher.workers().register_inflight_ask(InflightAsk {
            correlation_id: correlation_id.clone(),
            channel: wrapped.channel,
            caller: self.slot.clone(),
            target_project,
            target_session: None,
        });
        self.dispatcher.workers().bump_inflight_stats(&self.slot, PeerStatsDelta::OutgoingPlus1);

        match self.dispatcher.deliver(&self.slot, &target, wrapped) {
            Ok(status) => json_output(&serde_json::json!({
                "correlation_id": correlation_id.as_str(),
                "target_status": status,
                "slot": slot_json(&SessionSlot::new(
                    target.org(),
                    target.project(),
                    target.label(),
                )),
            })),
            Err(message) => {
                self.dispatcher.workers().complete_inflight_ask(&correlation_id);
                self.dispatcher
                    .workers()
                    .bump_inflight_stats(&self.slot, PeerStatsDelta::OutgoingMinus1);
                tool_error(message)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectKey;
    use crate::mcp::agents::target::{AgentTarget, LEAD_LABEL};
    use crate::mcp::peers::facade::{MockWorkspaceFacade, WorkspaceFacade};
    use crate::mcp::peers::types::PeerLiveness;
    use crate::mcp::workers::facade::{CallerProject, MockWorkerFacade, WorkerFacade};
    use forge_primitives::{WorkerLiveness, WorkerStatus};

    struct Host {
        peers: Arc<MockWorkspaceFacade>,
        workers: Arc<MockWorkerFacade>,
        dispatcher: Arc<AgentDispatcher>,
    }

    /// The caller leads `acme`/`core`; `other`/`proj` runs a worker `w1`.
    fn host() -> Host {
        let peers = Arc::new(MockWorkspaceFacade::new());
        peers.peers.lock().extend([configured("acme", "core"), configured("other", "proj")]);
        let workers = Arc::new(MockWorkerFacade::new());
        workers.workers.lock().insert("proj".to_owned(), vec![worker("other", "proj", "w1")]);
        workers.callers.lock().insert(
            caller(),
            CallerProject { project_key: ProjectKey::new("core"), is_lead: true },
        );
        let dispatcher = AgentDispatcher::new(
            Arc::clone(&peers) as Arc<dyn WorkspaceFacade>,
            Arc::clone(&workers) as Arc<dyn WorkerFacade>,
        );
        Host { peers, workers, dispatcher: Arc::new(dispatcher) }
    }

    fn caller() -> SessionSlot {
        SessionSlot::lead("acme", "core")
    }

    fn configured(org: &str, name: &str) -> PeerStatus {
        PeerStatus {
            name: name.to_owned(),
            org: org.to_owned(),
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            status: PeerLiveness::Running,
            in_flight_incoming: 0,
            in_flight_outgoing: 0,
            spawned_at: None,
        }
    }

    fn worker(org: &str, project: &str, label: &str) -> WorkerStatus {
        WorkerStatus {
            label: label.to_owned(),
            charter: format!("{label}'s charter"),
            status: WorkerLiveness::Running,
            session_id: format!("{label}-session"),
            slot: SessionSlot::worker(org, project, label),
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead(org, project),
            diagnostic: None,
            activity: None,
        }
    }

    fn target(org: &str, project: &str, label: Option<&str>) -> AgentTarget {
        let known = [configured("acme", "core"), configured("other", "proj")];
        AgentTarget::parse(&known, org, project, label).expect("configured project")
    }

    fn fill_address(args: &mut serde_json::Value, to: &AgentTarget) {
        args["org"] = serde_json::json!(to.org());
        args["project"] = serde_json::json!(to.project());
        if to.label() != LEAD_LABEL {
            args["label"] = serde_json::json!(to.label());
        }
    }

    async fn call_tell(host: &Host, to: AgentTarget, message: &str) -> ToolOutput {
        let tool = Tell { dispatcher: Arc::clone(&host.dispatcher), slot: caller() };
        let mut args = serde_json::json!({ "message": message });
        fill_address(&mut args, &to);
        tool.call(ToolInput { value: args }).await
    }

    async fn call_ask(host: &Host, to: AgentTarget, prompt: &str) -> ToolOutput {
        let tool = Ask { dispatcher: Arc::clone(&host.dispatcher), slot: caller() };
        let mut args = serde_json::json!({ "prompt": prompt });
        fill_address(&mut args, &to);
        tool.call(ToolInput { value: args }).await
    }

    async fn call_list(host: &Host, project_filter: Option<&str>) -> Vec<serde_json::Value> {
        let tool = List { dispatcher: Arc::clone(&host.dispatcher), slot: caller() };
        let mut args = serde_json::json!({});
        if let Some(name) = project_filter {
            args["project"] = serde_json::json!(name);
        }
        let output = tool.call(ToolInput { value: args }).await;
        assert!(!output.is_error, "list must not error: {:?}", output.blocks);
        serde_json::from_str(&output.blocks[0].text).expect("list returns a JSON array")
    }

    #[tokio::test]
    async fn tell_reaches_a_worker_in_another_project() {
        // The reach the merge adds: today this takes two leads and a
        // prose relay.
        let host = host();
        let output = call_tell(&host, target("other", "proj", Some("w1")), "hi").await;
        assert!(!output.is_error, "cross-project tell must land: {:?}", output.blocks);
        assert_eq!(host.workers.deliver_to_project_calls.lock().len(), 1);
    }

    #[tokio::test]
    async fn tell_at_a_projects_own_agent_reaches_that_projects_lead() {
        let host = host();
        let output = call_tell(&host, target("other", "proj", None), "hi").await;
        assert!(!output.is_error, "tell to another project's agent must land: {:?}", output.blocks);
        let calls = host.peers.deliver_calls.lock();
        assert_eq!(calls.len(), 1, "the peers engine carried it");
        assert_eq!(calls[0].1, "proj", "addressed by project name");
    }

    #[tokio::test]
    async fn ask_returns_the_reply_to_the_asking_session() {
        let host = host();
        let output = call_ask(&host, target("other", "proj", Some("w1")), "question").await;
        assert!(!output.is_error, "cross-project ask must land: {:?}", output.blocks);
        let asked: Vec<_> = host.workers.inflight.lock().values().cloned().collect();
        assert_eq!(asked.len(), 1, "the ask is tracked so the reply has somewhere to land");
        assert_eq!(asked[0].caller, caller(), "a cross-project ask comes back to the caller");
    }

    #[tokio::test]
    async fn list_filters_by_project() {
        let host = host();
        assert_eq!(
            call_list(&host, Some("proj")).await.len(),
            1,
            "the filter narrows to that project"
        );
        assert!(call_list(&host, None).await.len() > 1, "without it, the list is flat");
    }

    #[tokio::test]
    async fn list_addresses_every_row_by_its_slot() {
        let host = host();
        for row in call_list(&host, None).await {
            assert!(row["slot"]["org"].is_string(), "row carries its org: {row}");
            assert!(row["slot"]["project"].is_string(), "row carries its project: {row}");
            assert!(row["slot"]["label"].is_string(), "row carries its label: {row}");
        }
    }

    #[tokio::test]
    async fn whoami_returns_the_callers_own_slot() {
        let host = host();
        // The peers mock resolves identity by the caller's label rather
        // than by its project, so the fixture names a project after the
        // caller. The slot in the response comes from the caller either
        // way, which is what this pins.
        host.peers
            .peers
            .lock()
            .push(PeerStatus { name: caller().label().to_owned(), ..configured("acme", "core") });
        let tool = Whoami { dispatcher: Arc::clone(&host.dispatcher), slot: caller() };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error, "whoami must resolve the caller: {:?}", output.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert_eq!(parsed["slot"]["org"], "acme");
        assert_eq!(parsed["slot"]["project"], "core");
        assert_eq!(parsed["slot"]["label"], LEAD_LABEL);
    }

    #[tokio::test]
    async fn a_target_naming_an_unknown_project_is_refused() {
        let host = host();
        let tool = Tell { dispatcher: Arc::clone(&host.dispatcher), slot: caller() };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "org": "acme",
                    "project": "absent",
                    "message": "hi",
                }),
            })
            .await;
        assert!(output.is_error, "an unknown project must refuse, not pick a plausible seat");
        assert!(host.peers.deliver_calls.lock().is_empty(), "nothing was delivered");
    }
}
