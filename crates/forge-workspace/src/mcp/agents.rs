//! The `agents__*` family - one address space for every forge session.
//!
//! A target is a slot: `(org, project, label)`, with `lead` reserved
//! for a project's own agent. Three verbs reach any slot from any
//! session; the rest act on the caller's own project and are lead-only.

pub mod facade;
pub mod target;

use std::sync::Arc;

use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use crate::SessionSlot;
use crate::mcp::agents::facade::AgentDispatcher;
use crate::mcp::agents::target::{AgentTarget, LEAD_LABEL, TargetError};
// Only `build_server` names this, and that is gated on the test features -
// so an ungated import is unused in the configuration install.sh builds.
#[cfg(any(test, feature = "testing"))]
use crate::mcp::peers::facade::WorkspaceFacade;
use crate::mcp::peers::types::{MessageId, PeerStatus, WrappedKind, WrappedPrompt};
use crate::mcp::workers::facade::{
    DespawnOutcome, WorkerCapSource, WorkerDespawnError, WorkerFacade, WorkerSpawnError,
    WorkerUpdateError,
};
use crate::protocol::SessionChoice;

/// Attach the three any-caller verbs to an existing
/// [`McpServerBuilder`](forge_sdk::mcp::server::McpServerBuilder).
pub(crate) fn add_shared_tools(
    builder: forge_sdk::mcp::server::McpServerBuilder,
    dispatcher: Arc<AgentDispatcher>,
    slot: SessionSlot,
) -> forge_sdk::mcp::server::McpServerBuilder {
    let whoami = Whoami { dispatcher: dispatcher.clone(), slot: slot.clone() };
    let list = List { dispatcher: dispatcher.clone(), slot: slot.clone() };
    let send = SendMessage { dispatcher, slot };
    builder.tool(whoami).tool(list).tool(send)
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

/// A snapshot row: the address to pass back to `agents__send_message`,
/// then whatever the engine that produced the snapshot knows about the
/// seat.
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
    #[serde(default)]
    org: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

/// The name and org an envelope carries for the session that sent it,
/// derived from that session's own slot: the project for a project's own
/// agent, `project/label` for a worker, with the org beside it.
///
/// Derived rather than delegated to an engine, because one identity has to
/// hold on every path - the two engines answered this differently, the
/// in-project one by label and the cross-project one by project, so a
/// single worker rendered as two senders depending on which path carried
/// its message. The label is what keeps a worker distinguishable from its
/// own lead at all: without it a worker messaging another project carried
/// the bare project name, which is exactly what its lead sends.
fn sender_identity(slot: &SessionSlot) -> (String, String) {
    (crate::mcp::peers::types::seat_name(slot), slot.org().to_owned())
}

impl TargetArgs {
    /// The seat this call names. Optional in the parsed struct rather than
    /// required, so a missing half is a message naming what to pass rather
    /// than a serde error.
    fn resolve(&self, known: &[PeerStatus]) -> Result<AgentTarget, String> {
        let (Some(org), Some(project)) = (self.org.as_deref(), self.project.as_deref()) else {
            return Err(
                "a message needs a target: pass `org` and `project`, and `label` to reach a \
                 worker rather than the project's own agent. Call agents__list to see who you \
                 can reach."
                    .to_owned(),
            );
        };
        AgentTarget::parse(known, org, project, self.label.as_deref())
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
         path, current status, and when the session was spawned. Useful \
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
         first message it receives, which is true of another \
         project's agent - your own is already up if you are reading \
         this. A worker row must be live for the message to land. \
         \
         Rows differ in what they carry - a project's agent reports its \
         path and liveness, a worker reports its charter, current \
         activity and session id. Every row carries the slot to pass \
         back to agents__send_message. \
         \
         Only your own project's workers are listed. Another project's \
         workers are addressed by their labels, and those labels come \
         from that project's own agent rather than from here. \
         \
         CROSS-PROJECT RULE (mutations only): whenever the user asks you \
         to CHANGE state in a project other than your own - edit files, \
         run a command, file an issue, push a branch, anything with side \
         effects - call this tool FIRST. If the target project appears \
         here, do NOT cd into it and mutate its files directly. Hand the \
         work off with agents__send_message, which delivers it to that \
         project's own agent. Each agent owns its own repo; stay in \
         your lane and let the other project's agent execute the change. \
         \
         Reading another project's files for context is fine - sometimes \
         scanning the source yourself gives a sharper answer than asking \
         the other agent. The constraint is only on writes / state \
         changes. \
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

/// `agents__send_message` - one verb for every send, to any seat.
///
/// There is no reply verb: a reply is another message to the seat that
/// sent you one, so every send is addressed the same way.
pub(crate) struct SendMessage {
    pub(crate) dispatcher: Arc<AgentDispatcher>,
    pub(crate) slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct SendMessageArgs {
    #[serde(flatten)]
    target: TargetArgs,
    message: String,
}

#[async_trait::async_trait]
impl Tool for SendMessage {
    fn name(&self) -> &'static str {
        "agents__send_message"
    }

    fn description(&self) -> &'static str {
        "Send a message to another forge agent - its own agent, or a \
         named worker - and return immediately. The target is a slot: \
         `org` and `project` name it, and `label` names the seat inside \
         that project. Omit `label` to reach the project's own agent; \
         set it to a worker's label to reach that worker. Run \
         agents__list for a label in your own project; a worker in \
         another project is addressed by whatever label that project's \
         own agent gives you. \
         \
         Every message is the same kind of thing, including a reply: to \
         answer an agent that sent you one, address it by its own slot \
         the way you would address anyone else. The target sees the \
         message as a new user turn and may message you back, or simply \
         continue its own work. \
         \
         Use this instead of mutating another project's files directly \
         whenever the user asks you to notify or hand off work to another \
         project - e.g. \"let the gateway backend know the rewriter \
         cleanup landed\", \"ask forge to pick this up next \
         session\". Reading another project's files for your own context \
         is still allowed; only state changes and hand-offs go through \
         this tool. \
         \
         A request addressed to another project's own agent is delivered \
         to that project's agent, spawning it first if it was sleeping, \
         so expect extra latency on the first message to a sleeping \
         project. A request addressed to a worker reaches it only while \
         that worker is live; if the label is gone the call fails \
         immediately and names the label. \
         \
         A `sent` status means the queue ACCEPTED the message, not that \
         the target read it - a target that is down or wedged still \
         returns sent, so confirm real work happened by an answer or an \
         observable artifact rather than by the ack. \
         \
         When the message you are about to send asserts that work is \
         done, reviewed, or verified, or hands over a pick where two \
         readings both survive your own reasoning and nothing outside you \
         decides between them, and the `systemone__*` tools are in your \
         list, make one call on the claim or the pick first: put the claim \
         and its evidence in `state` for a claim check, or the options you \
         have already enumerated in `criteria` for a choice. A decisive \
         answer is permission to send it plainly."
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
            },
            "required": ["org", "project", "message"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: SendMessageArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let known = self.dispatcher.peers().list_peers();
        let target = match args.target.resolve(&known) {
            Ok(target) => target,
            Err(message) => return tool_error(message),
        };

        let id = MessageId::mint();
        let (sender_name, sender_org) = sender_identity(&self.slot);
        let wrapped = WrappedPrompt {
            id: id.clone(),
            kind: WrappedKind::Message,
            sender_name,
            sender_org,
            body: args.message,
        };
        match self.dispatcher.deliver(&self.slot, &target, wrapped) {
            Ok(()) => json_output(&serde_json::json!({
                "status": "sent",
                "id": id.as_str(),
                "to": slot_json(&SessionSlot::new(
                    target.org(),
                    target.project(),
                    target.label(),
                )),
            })),
            Err(message) => tool_error(message),
        }
    }
}

/// Build a standalone `forge` MCP server carrying the whole `agents__*`
/// family as a lead sees it. The wire-conformance harness drives the
/// surface through this against mock facades.
#[cfg(any(test, feature = "testing"))]
pub fn build_server(
    peers: Arc<dyn WorkspaceFacade>,
    workers: Arc<dyn WorkerFacade>,
    slot: SessionSlot,
) -> forge_sdk::mcp::server::McpServer {
    let dispatcher = Arc::new(AgentDispatcher::new(peers, workers.clone()));
    let builder = forge_sdk::mcp::server::McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION"));
    let builder = add_shared_tools(builder, dispatcher, slot.clone());
    add_lead_tools(builder, workers, slot).build()
}

/// Attach the four lead-only verbs to an existing
/// [`McpServerBuilder`](forge_sdk::mcp::server::McpServerBuilder).
/// Each acts on the caller's own project, so a worker has no project to
/// act on and is not offered the surface.
pub(crate) fn add_lead_tools(
    builder: forge_sdk::mcp::server::McpServerBuilder,
    facade: Arc<dyn WorkerFacade>,
    slot: SessionSlot,
) -> forge_sdk::mcp::server::McpServerBuilder {
    let spawn = Spawn { facade: facade.clone(), slot: slot.clone() };
    let capacity = Capacity { facade: facade.clone(), slot: slot.clone() };
    let despawn = Despawn { facade: facade.clone(), slot: slot.clone() };
    let update = Update { facade, slot };
    builder.tool(spawn).tool(capacity).tool(despawn).tool(update)
}

fn format_spawn_error(err: &WorkerSpawnError) -> String {
    match err {
        WorkerSpawnError::NotLeadCaller => {
            "agents__spawn is lead-only; this session is a worker. Workers cannot \
             spawn other workers in v1."
                .to_owned()
        }
        WorkerSpawnError::EmptyLabel => "label must be non-empty after trim".to_owned(),
        WorkerSpawnError::ReservedLabel => format!(
            "label '{LEAD_LABEL}' is reserved - agents__send_message uses it as \
             the addressing keyword for the caller's project's own agent. Pick a different label."
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
        WorkerSpawnError::ResumeLookupFailed { label, message } => format!(
            "could not look up a prior session for '{label}': {message}. Nothing was spawned, \
             because a failed lookup is not the same answer as no prior session; retry, or \
             spawn without resume_session to start fresh"
        ),
    }
}

fn format_despawn_error(err: &WorkerDespawnError) -> String {
    match err {
        WorkerDespawnError::NotLeadCaller => {
            "agents__despawn is lead-only; this session is a worker. Only the project lead may despawn workers.".to_owned()
        }
        WorkerDespawnError::EmptyLabel => "label must be non-empty after trim".to_owned(),
        WorkerDespawnError::UnknownCallerProject => {
            "could not resolve caller to a known project (forge bug)".to_owned()
        }
        WorkerDespawnError::UnknownLabel { label, project_key } => format!(
            "no live worker with label '{label}' in project '{project_key}'. Call agents__list for your own project's pool; a worker in another project is addressed by whatever label that project's own agent gives you."
        ),
        WorkerDespawnError::DispatchFailed { message } => {
            format!("worker despawn failed: {message}")
        }
    }
}

fn format_update_error(err: &WorkerUpdateError) -> String {
    match err {
        WorkerUpdateError::UnknownCallerProject => {
            "could not resolve caller to a known project (forge bug)".to_owned()
        }
        WorkerUpdateError::NoSuchWorker { label, project_key } => format!(
            "no dynamic worker '{label}' in project '{project_key}'. agents__update revises a \
             worker created by agents__spawn, so if you meant to create one, spawn it first."
        ),
        WorkerUpdateError::StoreFailed { message } => format!("worker update failed: {message}"),
    }
}

/// `agents__spawn` - lead-only. Allocates a new session in the caller's
/// project, threading `charter` through the new session's system-prompt
/// addendum, then returns the assigned `session_id` and `tag`
/// (`forge:worker:<label>`).
pub(crate) struct Spawn {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct SpawnArgs {
    label: String,
    charter: String,
    #[serde(default)]
    kick: Option<String>,
    #[serde(default)]
    resume_kick: Option<String>,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    resume_session: bool,
    #[serde(default)]
    mcp_families: Option<Vec<String>>,
}

/// Validate and canonicalise a spawn or update `mcp_families` argument:
/// names in wire order, de-duplicated; an empty list resolves to `None`
/// (every family) for a spawn.
fn canonical_spawn_families(names: Option<&[String]>) -> Result<Option<Vec<String>>, String> {
    match names {
        Some(names) => crate::mcp::canonical_mcp_families(names),
        None => Ok(None),
    }
}

#[async_trait::async_trait]
impl Tool for Spawn {
    fn name(&self) -> &'static str {
        "agents__spawn"
    }

    fn description(&self) -> &'static str {
        "Spawn a new worker session inside YOUR project (lead-only). \
         The worker is a full forge session - its own claude subprocess, \
         own chat view, own permissions - addressable from your session \
         by its label, via agents__send_message with your own \
         org and project. `charter` is the worker's mission, threaded \
         into the new session's system prompt, and defines what that \
         worker is. PROVIDE `kick` TO START THE WORKER IMMEDIATELY: \
         the kick is delivered as the worker's first user-turn the moment \
         it connects, so it begins working at once. WITHOUT a kick the \
         worker sits idle until you send it an agents__send_message - a \
         'begin now' line in the charter does NOT run on its own, so pass \
         `kick` for any ad-hoc spawn you want to start now. Returns the \
         worker's \
         session_id and tag (`forge:worker:<label>`). A spawned worker is \
         DURABLE: it survives forge restarts and is automatically \
         re-spawned, resuming where it left off (a restarted worker is \
         told to continue, not start over), until you explicitly despawn \
         it with agents__despawn (or close its row in the Projects \
         pane). A worker whose worktree has gone is not re-spawned \
         automatically - its resume would have nowhere to start - so it \
         stops being offered until the worktree is back; passing \
         `resume_session` recreates that worktree and brings it back. \
         DESPAWNED A WORKER WHOSE CONTEXT YOU STILL WANT? Re-spawn \
         the same label with `resume_session` set: it resumes the label's \
         most recent prior session instead of starting fresh. When the \
         label has no prior session a fresh one starts and the response \
         says which session it landed on. \
         PASS `resume_kick` FOR A LONG-LIVED WORKER whose restart \
         needs specific steps - re-read a file, catch up a queue, check \
         what was mid-run - rather than that generic continue; it \
         replaces the restart note on every resume. Omit it and the \
         generic note is what a resumed worker gets. A worker cannot ask \
         the user anything directly - it has no AskUserQuestion - and \
         reaches them through you instead; PASS `interactive` only for a \
         worker the user asked to talk to directly. So spawn one per \
         distinct piece of work, and despawn once the worker has handed \
         over what you spawned it to produce: a merged PR, or equally a \
         written report, an answered question, a finished sweep - a \
         worker whose output is not a PR has no merge to wait for and \
         still needs closing. A forgotten worker keeps coming back on \
         every restart. At most one live worker per label - \
         if one already exists, this errors and you should message it \
         with agents__send_message instead of spawning again. \
         The label 'lead' is reserved (it addresses a project's own \
         agent) and rejected here. \
         Use agents__list to see your project's current worker pool. \
         `mcp_families` selects which MCP families this worker gets: \
         any of review, cron, tasks, gotify, slack and systemone (the \
         agents__* verbs are always on and cannot be listed). Omit it, \
         or pass an empty list, and the worker gets every family. Pass a \
         list and it gets exactly those; each withheld family is named \
         in the worker's own prompt so it knows what it does not have - \
         a worker without systemone is told to route decisions to you. \
         The choice is stored on the worker and survives restarts; \
         revise it with agents__update. On a resume or re-spawn that \
         states no `mcp_families`, the worker's stored selection \
         applies; stating one then revises it. \
         This tool errors if called from a worker session; only the \
         project lead may spawn."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Identifier you will use to address this worker later, as the `label` of an agents__send_message target. Non-empty after trim. At most one live worker per label - reusing a label with a live worker is rejected.",
                },
                "charter": {
                    "type": "string",
                    "description": "The worker's mission, threaded into the new session's system prompt. This is what defines the worker, so say what it is responsible for and how it should work. Non-empty after trim.",
                },
                "kick": {
                    "type": "string",
                    "description": "Optional first-turn message delivered to the worker the moment it connects, so it STARTS WORKING IMMEDIATELY (equivalent to sending an agents__send_message right after spawn). STRONGLY RECOMMENDED for ad-hoc spawns: WITHOUT a kick the worker sits idle until you send it an agents__send_message - a 'begin now' line in the charter does NOT run on its own. Omit only when you intend to drive the worker yourself with a later agents__send_message.",
                },
                "resume_kick": {
                    "type": "string",
                    "description": "Optional re-orient message delivered every time this worker is RESUMED after a forge restart, in place of the generic 'continue where you left off' note. For a LONG-LIVED worker whose restart needs specific steps - re-read a file, catch up a queue, check whether something was mid-run before re-running it - rather than a generic continue. Stored at spawn rather than delivered now; the first turn of a fresh spawn is `kick`. Non-empty after trim when provided - to keep the generic restart note, OMIT the argument rather than passing an empty string, which is rejected.",
                },
                "interactive": {
                    "type": "boolean",
                    "description": "Set true ONLY when the user asked for a worker they will talk to DIRECTLY and will have its row open. It keeps the built-in AskUserQuestion tool, which every other worker is denied: a worker's question renders in its own row, which nobody is usually watching, and an answer that does arrive is indistinguishable from a decision the user actually made - so a worker can attribute a choice to the user in good faith that the user never saw. Defaults to false, which is right for any worker you are spawning on your own initiative; that worker reaches the user through you, via its agents__send_message to you. This is fixed at spawn - changing it means despawning the worker and spawning it again.",
                },
                "resume_session": {
                    "type": "boolean",
                    "description": "Set true to RESUME this label's most recent prior session instead of starting fresh, so the old conversation arrives as history and the worker continues where it left off. The natural move after despawning a worker whose context you still want: re-spawn the same label with this set. The session is resolved from what the label is registered under, or from its own transcripts by worker tag once that is gone; if there is none to resume, a fresh one starts and the response says which happened. A live worker on the same label is still rejected; despawn or close it first. A git worker's worktree is recreated if despawn removed it, so the resumed session lands back in its run directory.",
                },
                "mcp_families": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["review", "cron", "tasks", "gotify", "slack", "systemone"] },
                    "description": "The MCP families this worker gets, exactly those listed. Omit or pass an empty list for every family. `agents__*` is always on and cannot be listed. Each withheld family is named in the worker's own prompt.",
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

        // An empty one is `Some`, so it would beat the restart-note
        // fallback and dispatch a blank first turn on every resume - the
        // same contract agents__update holds this arg to.
        if args.resume_kick.as_ref().is_some_and(|text| text.trim_end().is_empty()) {
            return tool_error("resume_kick must be non-empty after trim when provided".to_owned());
        }
        let mcp_families = match canonical_spawn_families(args.mcp_families.as_deref()) {
            Ok(families) => families,
            Err(message) => return tool_error(message),
        };
        match self
            .facade
            .spawn_worker(
                &self.slot,
                args.label,
                args.charter,
                args.kick,
                args.resume_kick,
                args.interactive,
                args.resume_session,
                mcp_families,
            )
            .await
        {
            Ok(reply) => {
                let mut body = serde_json::json!({
                    "session_id": reply.session_id,
                    "tag": reply.tag,
                    "session": match reply.session_choice {
                        SessionChoice::Resumed => "resumed the label's prior session",
                        SessionChoice::Fresh => "started a new session (resume_session was not set)",
                        SessionChoice::FreshWithoutPrior => {
                            "started a new session: no prior session found for this label"
                        }
                    },
                });
                if let Some(families) = &reply.mcp_families {
                    body["mcp_families"] = serde_json::json!(families);
                }
                if let Some(account) = &reply.rate_limited_account {
                    body["notice"] = serde_json::Value::String(format!(
                        "assigned account '{account}' is currently rate-limited or bailed. The worker spawns anyway but may hit a 429 right away; free up an account or wait for a reset."
                    ));
                }
                if let Some(warning) = &reply.durability_warning {
                    body["durability_warning"] = serde_json::Value::String(warning.clone());
                }
                json_output(&body)
            }
            Err(err) => tool_error(format_spawn_error(&err)),
        }
    }
}

/// `agents__despawn` - lead-only. Closes a worker by label and cleans
/// up its git worktree. A clean worktree is removed; a dirty one
/// (uncommitted/untracked or unpushed commits) blocks the despawn
/// unless `force`.
pub(crate) struct Despawn {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct DespawnArgs {
    label: String,
    #[serde(default)]
    force: Option<bool>,
}

#[async_trait::async_trait]
impl Tool for Despawn {
    fn name(&self) -> &'static str {
        "agents__despawn"
    }

    fn description(&self) -> &'static str {
        "Despawn (close + clean up) a worker in YOUR project by label \
         (lead-only). Kills the worker's claude subprocess, removes it \
         from agents__list, AND cleans up its git worktree. A CLEAN \
         worktree is removed as \
         part of the despawn; a DIRTY one (uncommitted/untracked changes \
         or unpushed commits) BLOCKS the despawn and returns a reason - \
         clean it up (commit + push, or reset) and retry, or pass \
         force=true to tear down and discard the worktree. Nothing is \
         ever silently discarded. The worktree-<label> branch claude \
         created for the worker is deleted alongside the worktree, but \
         only when every commit on it is reachable from some other ref - \
         another branch, a tag, a remote-tracking ref, or a worktree's \
         HEAD, so a branch \
         you already pushed still counts as reapable. One carrying \
         commits that exist nowhere else is left alone and named in a \
         branch_cleanup_warning. Returns {status:\"despawned\"} (with an \
         optional worktree_cleanup_warning when the worktree removal \
         itself failed, and an optional branch_cleanup_warning when the \
         branch was kept) or {status:\"blocked\", reason}. This is how you \
         PERMANENTLY remove a durable worker: a spawned worker otherwise \
         survives forge restarts and re-spawns automatically, so despawn \
         is what makes it stop coming back. Closing the worker's row in \
         the Projects pane does the same. Despawn once a worker has handed \
         over what it was spawned to produce: a worker whose output is a \
         PR lives until that PR merges; a worker whose output is not a PR \
         - a written report, an answered question - has no merge to wait \
         for and is done when it hands over. Errors if called from \
         a worker session; only the project lead may despawn."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Worker label from agents__list to close. Non-empty after trim.",
                },
                "force": {
                    "type": "boolean",
                    "description": "Tear down and discard the worktree even if it has uncommitted/untracked changes or unpushed commits. Default false: a dirty worktree blocks the despawn with a reason instead, so work is never silently discarded.",
                },
            },
            "required": ["label"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: DespawnArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };

        match self.facade.despawn_worker(&self.slot, &args.label, args.force.unwrap_or(false)).await
        {
            Ok(DespawnOutcome::Despawned { worktree_cleanup_warning, branch_cleanup_warning }) => {
                let mut body = serde_json::json!({ "status": "despawned" });
                if let Some(warning) = worktree_cleanup_warning
                    && let Some(map) = body.as_object_mut()
                {
                    map.insert(
                        "worktree_cleanup_warning".to_owned(),
                        serde_json::Value::String(warning),
                    );
                }
                if let Some(warning) = branch_cleanup_warning
                    && let Some(map) = body.as_object_mut()
                {
                    map.insert(
                        "branch_cleanup_warning".to_owned(),
                        serde_json::Value::String(warning),
                    );
                }
                json_output(&body)
            }
            Ok(DespawnOutcome::Blocked { reason }) => {
                json_output(&serde_json::json!({ "status": "blocked", "reason": reason }))
            }
            Err(err) => tool_error(format_despawn_error(&err)),
        }
    }
}

/// `agents__capacity` - lead-only aggregate read of the caller's
/// project worker capacity. One JSON object rather than the per-worker
/// snapshots `agents__list` returns.
pub(crate) struct Capacity {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for Capacity {
    fn name(&self) -> &'static str {
        "agents__capacity"
    }

    fn description(&self) -> &'static str {
        "Report the worker capacity of YOUR project (lead-only): the \
         configured cap, how many workers are live, and how many slots \
         are free. Use it before spawning to see whether a spawn would \
         hit the limit. The cap is the project's max_workers in \
         forge.toml when set, else forge's default; cap_source names \
         which. Takes no arguments."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        let Some(capacity) = self.facade.capacity(&self.slot) else {
            return tool_error(
                "could not resolve caller to a known project (forge bug)".to_owned(),
            );
        };
        json_output(&serde_json::json!({
            "project": capacity.project,
            "cap": capacity.cap,
            "live": capacity.live,
            "available": capacity.cap.saturating_sub(capacity.live),
            "cap_source": match capacity.cap_source {
                WorkerCapSource::ProjectMaxWorkers => "max_workers",
                WorkerCapSource::Default => "default",
            },
        }))
    }
}

/// `agents__update` - lead-only. Revises the stored `charter`, `kick`
/// and `resume_kick` of an EXISTING worker, keyed by
/// `(project_key, label)` exactly as `agents__spawn` persisted it.
///
/// Refuses when no row exists. A row is what makes a worker re-spawn on
/// the next lead connect, so creating one here would mean revising a
/// definition silently produces a worker.
pub(crate) struct Update {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct UpdateArgs {
    label: String,
    #[serde(default)]
    charter: Option<String>,
    #[serde(default)]
    kick: Option<String>,
    #[serde(default)]
    resume_kick: Option<String>,
    #[serde(default)]
    mcp_families: Option<Vec<String>>,
}

#[async_trait::async_trait]
impl Tool for Update {
    fn name(&self) -> &'static str {
        "agents__update"
    }

    fn description(&self) -> &'static str {
        "Revise a worker's stored instructions without despawning it \
         (lead-only). Replaces any of `charter`, `kick`, `resume_kick` and \
         `mcp_families` on that worker's persisted record; a field you omit \
         keeps its current value, and at least one must be supplied. TAKES \
         EFFECT ON THE WORKER'S NEXT RESPAWN, NOT IMMEDIATELY - a session's \
         system prompt and tool list are fixed when the session spawns, so \
         a running worker keeps what it started with; use \
         agents__send_message to redirect it now. \
         `mcp_families` revises which MCP families the worker gets, \
         validated exactly as agents__spawn validates it: a list replaces \
         the stored selection, and an empty list restores every family. \
         The worker must already exist: this never creates one, so spawn \
         it with agents__spawn first (which takes the same four texts). \
         Address it by the same `label` you spawned it with, as shown by \
         agents__list."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "The worker to revise, as passed to agents__spawn and listed by agents__list. Non-empty after trim. Must already exist - this never creates a worker.",
                },
                "charter": {
                    "type": "string",
                    "description": "Replacement mission text, threaded into the worker's system prompt on its next respawn. Omit to leave the stored charter unchanged. Non-empty after trim when provided.",
                },
                "kick": {
                    "type": "string",
                    "description": "Replacement first-turn message used when this worker is spawned fresh. Omit to leave the stored kick unchanged. Non-empty after trim when provided.",
                },
                "resume_kick": {
                    "type": "string",
                    "description": "Replacement re-orient message delivered when this worker is resumed after a forge restart, in place of the generic restart note. Omit to leave the stored value unchanged. Non-empty after trim when provided.",
                },
                "mcp_families": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["review", "cron", "tasks", "gotify", "slack", "systemone"] },
                    "description": "Replacement MCP-family selection for this worker, validated as agents__spawn validates it. Omit to leave the stored selection unchanged; pass an empty list to restore every family. Takes effect on the next respawn.",
                },
            },
            "required": ["label"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: UpdateArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };

        let Some(caller_project) = self.facade.caller_project(&self.slot) else {
            return tool_error("agents__update: caller resolves to no known project".to_owned());
        };
        if !caller_project.is_lead {
            return tool_error(
                "agents__update is lead-only; this session is a worker. Workers cannot revise \
                 other workers."
                    .to_owned(),
            );
        }

        let label = args.label.trim();
        if label.is_empty() {
            return tool_error("label must be non-empty after trim".to_owned());
        }

        // Same non-empty contract the spawn path holds these texts to.
        // #685 and #686 record its known gaps; match the predicate rather
        // than inventing a stronger one here.
        let mut updated: Vec<&str> = Vec::new();
        for (name, value) in
            [("charter", &args.charter), ("kick", &args.kick), ("resume_kick", &args.resume_kick)]
        {
            if let Some(text) = value {
                if text.trim_end().is_empty() {
                    return tool_error(format!(
                        "{name} must be non-empty after trim when provided"
                    ));
                }
                updated.push(name);
            }
        }
        // A present-but-empty list is the reset (every family); anything
        // else is canonicalised like the spawn argument.
        let mcp_families = match args.mcp_families.as_deref() {
            Some([]) => Some(Vec::new()),
            Some(names) => match crate::mcp::canonical_mcp_families(names) {
                Ok(families) => families,
                Err(message) => return tool_error(message),
            },
            None => None,
        };
        if mcp_families.is_some() {
            updated.push("mcp_families");
        }
        if updated.is_empty() {
            return tool_error(
                "supply at least one of charter, kick, resume_kick or mcp_families; an update \
                 with none of them would change nothing."
                    .to_owned(),
            );
        }

        match self.facade.update_worker(
            &self.slot,
            label,
            args.charter,
            args.kick,
            args.resume_kick,
            mcp_families,
        ) {
            Ok(()) => json_output(&serde_json::json!({ "label": label, "updated": updated })),
            Err(err) => tool_error(format_update_error(&err)),
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
    use crate::mcp::workers::facade::{
        CallerProject, MockWorkerFacade, WorkerCapacity, WorkerFacade,
    };
    use crate::protocol::WorkerSpawnReply;
    use forge_primitives::{WorkerLiveness, WorkerStatus};

    struct Host {
        peers: Arc<MockWorkspaceFacade>,
        workers: Arc<MockWorkerFacade>,
        dispatcher: Arc<AgentDispatcher>,
    }

    /// The lead of `acme`/`core` is a caller, and so is one of its workers;
    /// `other`/`proj` runs a worker `w1`. Both callers resolve to the same
    /// project, which is what makes a lead's call and a worker's call
    /// comparable.
    fn host() -> Host {
        let peers = Arc::new(MockWorkspaceFacade::new());
        // The peers mock resolves identity by the caller's label rather
        // than by its project, so the worker caller needs a project
        // named after it. The lead's slot is already named `lead`, which
        // is what the mock looks for.
        peers.peers.lock().extend([
            configured("acme", "core"),
            configured("other", "proj"),
            PeerStatus { name: worker_caller().label().to_owned(), ..configured("acme", "core") },
        ]);
        let workers = Arc::new(MockWorkerFacade::new());
        // `core` is the key both callers resolve to, so its own pool is
        // what `list` reads; `proj` is what a cross-project target
        // resolves against.
        workers.workers.lock().insert(
            "core".to_owned(),
            vec![worker("acme", "core", "w1"), worker("acme", "core", "w2")],
        );
        workers.workers.lock().insert("proj".to_owned(), vec![worker("other", "proj", "w1")]);
        for (slot, is_lead) in [(caller(), true), (worker_caller(), false)] {
            workers
                .callers
                .lock()
                .insert(slot, CallerProject { project_key: ProjectKey::new("core"), is_lead });
        }
        let dispatcher = AgentDispatcher::new(
            Arc::clone(&peers) as Arc<dyn WorkspaceFacade>,
            Arc::clone(&workers) as Arc<dyn WorkerFacade>,
        );
        Host { peers, workers, dispatcher: Arc::new(dispatcher) }
    }

    fn caller() -> SessionSlot {
        SessionSlot::lead("acme", "core")
    }

    /// A worker on the lead's own team. Every shared verb has to work for
    /// it as well as for the lead: the whole point of the merge is that a
    /// worker's reach is the same.
    fn worker_caller() -> SessionSlot {
        SessionSlot::worker("acme", "core", "w2")
    }

    fn configured(org: &str, name: &str) -> PeerStatus {
        PeerStatus {
            name: name.to_owned(),
            org: org.to_owned(),
            path: std::path::PathBuf::from(format!("/tmp/{name}")),
            status: PeerLiveness::Running,
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

    async fn call_send(host: &Host, to: AgentTarget, message: &str) -> ToolOutput {
        let tool = SendMessage { dispatcher: Arc::clone(&host.dispatcher), slot: caller() };
        let mut args = serde_json::json!({ "message": message });
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

    /// The despawn trigger is shipped text a session reads at the moment it
    /// decides whether to despawn, and this description plus
    /// [`Despawn::description`] are two of its three homes (the third is
    /// `lead_charter.md`). Both cases have to survive an edit here: a worker
    /// whose output is a PR lives until that PR merges, and one whose output
    /// is not a PR closes at handover.
    #[test]
    fn spawn_metadata_shape() {
        let host = host();
        let tool =
            Spawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        assert_eq!(tool.name(), "agents__spawn");
        assert!(
            tool.description().contains("no merge to wait for"),
            "the two-case despawn trigger stays: {}",
            tool.description()
        );
        assert!(
            tool.description().contains("a finished sweep"),
            "non-PR outputs stay named as despawn triggers: {}",
            tool.description()
        );
        let schema = tool.input_schema();
        let required = schema["required"].as_array().expect("required field present");
        assert!(required.iter().any(|v| v == "label"));
        assert!(
            required.iter().any(|v| v == "charter"),
            "charter is required - there is no file to fall back to"
        );
    }

    /// The send cue fires at the moment a report is composed, and the tools
    /// it names can be renamed - pinned so a rename fails here rather than
    /// shipping a cue that resolves to nothing.
    #[test]
    fn send_description_carries_its_systemone_cue() {
        let host = host();
        let tool = SendMessage { dispatcher: Arc::clone(&host.dispatcher), slot: caller() };
        assert!(
            tool.description().contains(
                "make one call on the claim or the pick first: put the claim and its evidence in `state` for a claim check"
            ),
            "the report-moment cue and its fields stay: {}",
            tool.description()
        );
        assert!(
            tool.description()
                .contains("or the options you have already enumerated in `criteria` for a choice"),
            "the choice half names its own field: {}",
            tool.description()
        );
    }

    #[test]
    fn despawn_metadata_shape() {
        let host = host();
        let tool =
            Despawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        assert_eq!(tool.name(), "agents__despawn");
        assert!(
            tool.description().contains("lives until that PR merges"),
            "the PR case waits for the merge: {}",
            tool.description()
        );
        assert!(
            tool.description().contains("done when it hands over"),
            "the non-PR case closes at handover: {}",
            tool.description()
        );
        let schema = tool.input_schema();
        let required = schema["required"].as_array().expect("required field present");
        assert!(required.iter().any(|v| v == "label"));
        assert!(required.iter().all(|v| v != "force"), "force is optional");
        assert!(schema["properties"].as_object().unwrap().contains_key("force"));
    }

    #[tokio::test]
    async fn a_send_reaches_a_worker_in_another_project() {
        // The reach the merge adds: today this takes two leads and a
        // prose relay.
        let host = host();
        let output = call_send(&host, target("other", "proj", Some("w1")), "hi").await;
        assert!(!output.is_error, "cross-project send must land: {:?}", output.blocks);
        assert_eq!(host.workers.deliver_to_project_calls.lock().len(), 1);
    }

    #[tokio::test]
    async fn a_send_at_a_projects_own_agent_reaches_that_projects_lead() {
        let host = host();
        let output = call_send(&host, target("other", "proj", None), "hi").await;
        assert!(!output.is_error, "send to another project's agent must land: {:?}", output.blocks);
        let calls = host.peers.deliver_calls.lock();
        assert_eq!(calls.len(), 1, "the peers engine carried it");
        assert_eq!(calls[0].1, "proj", "addressed by project name");
    }

    /// The result names the send and the seat it reached, and nothing
    /// else: the id is traceability for the sender's own echo, and there
    /// is no outstanding state for a second field to describe.
    #[tokio::test]
    async fn a_send_result_carries_the_id_and_the_seat_it_reached() {
        let host = host();
        let output = call_send(&host, target("other", "proj", Some("w1")), "hi").await;
        assert!(!output.is_error, "the send must land: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("send returns JSON");
        assert_eq!(parsed["status"], "sent");
        let id = parsed["id"].as_str().expect("the result carries the send's id");
        assert!(id.starts_with("m-"), "ids name the send: {id}");
        assert_eq!(
            parsed["to"],
            serde_json::json!({
                "org": "other",
                "project": "proj",
                "label": "w1",
            })
        );
        assert_eq!(
            parsed.as_object().expect("an object").len(),
            3,
            "the shape is status/id/to and nothing else: {parsed}",
        );
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
    async fn list_carries_the_callers_own_workers_under_their_labels() {
        let host = host();
        let rows = call_list(&host, None).await;
        let row = rows
            .iter()
            .find(|row| row["slot"]["label"] == "w1")
            .unwrap_or_else(|| panic!("the caller's own worker is a row: {rows:?}"));
        assert_eq!(row["slot"]["project"], "core");
        assert_eq!(row["charter"], "w1's charter", "a worker row carries its snapshot");
    }

    /// A worker's own call, one per shared verb. The role-set tests pin that
    /// a worker is OFFERED these three; these pin that they work when it calls
    /// them, which is the reach the merge widened and which nothing else
    /// exercises - every other test in this file runs as a lead.
    #[tokio::test]
    async fn a_worker_can_read_its_own_identity() {
        let host = host();
        let tool = Whoami { dispatcher: Arc::clone(&host.dispatcher), slot: worker_caller() };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error, "whoami must resolve a worker caller: {:?}", output.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert_eq!(parsed["slot"]["label"], "w2");
        assert_eq!(parsed["slot"]["project"], "core");
    }

    #[tokio::test]
    async fn a_worker_lists_its_own_projects_pool() {
        let host = host();
        let tool = List { dispatcher: Arc::clone(&host.dispatcher), slot: worker_caller() };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error, "list must answer a worker caller: {:?}", output.blocks);
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&output.blocks[0].text).expect("JSON array");
        assert!(
            rows.iter().any(|row| row["slot"]["label"] == "w1"),
            "a worker sees its project's pool: {rows:?}",
        );
    }

    #[tokio::test]
    async fn a_worker_can_message_its_lead() {
        let host = host();
        let tool = SendMessage { dispatcher: Arc::clone(&host.dispatcher), slot: worker_caller() };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "org": "acme",
                    "project": "core",
                    "label": LEAD_LABEL,
                    "message": "done",
                }),
            })
            .await;
        assert!(!output.is_error, "a worker's message to its lead must land: {:?}", output.blocks);
        // The reserved label takes the lead path, and only that path:
        // the worker path would look for a worker labelled `lead` in the
        // caller's pool and refuse.
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert_eq!(parsed["status"], "sent");
    }

    #[tokio::test]
    async fn capacity_floors_at_zero_free_slots_when_the_cap_drops_below_the_pool() {
        // `max_workers` can be lowered under a live pool, and the free-slot
        // count is rendered to the model: subtracting without flooring
        // wraps to a 20-digit number, or panics in a debug build. The
        // assertion below is therefore the release-profile kill; in debug
        // the panic is rustc's subtract-overflow and fires first.
        let host = host();
        *host.workers.capacity_reply.lock() = Some(WorkerCapacity {
            project: "core".to_owned(),
            cap: 1,
            live: 3,
            cap_source: WorkerCapSource::Default,
        });
        let tool =
            Capacity { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool.call(ToolInput { value: serde_json::json!({}) }).await;
        assert!(!output.is_error, "capacity must answer: {:?}", output.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert_eq!(parsed["available"], 0, "a cap under the live count leaves no free slot");
    }

    #[tokio::test]
    async fn spawn_hands_the_kick_and_the_interactive_flag_to_the_facade() {
        // `kick` is what starts a worker at all and `interactive` decides
        // whether it keeps AskUserQuestion. Both are persisted, so
        // defaulting either here is a worker that never starts or one
        // that can never ask its user anything.
        let host = host();
        *host.workers.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "s-1".to_owned(),
            tag: "forge:worker:reviewer".to_owned(),
            mcp_families: None,
            rate_limited_account: None,
            durability_warning: None,
            session_choice: SessionChoice::Fresh,
        }));
        let tool =
            Spawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "reviewer",
                    "charter": "review the diff",
                    "kick": "start with the diff",
                    "interactive": true,
                }),
            })
            .await;
        assert!(!output.is_error, "the spawn must be answered: {:?}", output.blocks);
        let calls = host.workers.spawn_calls.lock();
        assert_eq!(calls.len(), 1, "the facade saw the spawn");
        assert_eq!(calls[0].3.as_deref(), Some("start with the diff"), "the kick reaches it");
        assert!(calls[0].5, "the interactive flag reaches it");
    }

    #[tokio::test]
    async fn a_workers_envelope_carries_its_label_beside_its_project() {
        // Drop the label and a worker's envelope is the bare project name,
        // which is exactly what its own lead sends - so a recipient cannot
        // tell the two apart at all.
        let host = host();
        let tool = SendMessage { dispatcher: Arc::clone(&host.dispatcher), slot: worker_caller() };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "org": "other",
                    "project": "proj",
                    "label": "w1",
                    "message": "hi",
                }),
            })
            .await;
        assert!(!output.is_error, "the send must land: {:?}", output.blocks);
        let sent = host.workers.deliver_to_project_calls.lock();
        assert_eq!(sent[0].wrapped.sender_name, "core/w2", "the envelope names the worker");
        assert_eq!(sent[0].wrapped.sender_org, "acme", "and its org, not the project's");
    }

    #[tokio::test]
    async fn update_refuses_a_revision_that_would_change_nothing() {
        // Every field is optional, so a call naming none of them is a
        // no-op the caller would read as a successful revision. The
        // boundary is the assertion: the empty case refuses, and a
        // supplied field is named back rather than silently accepted.
        let host = host();
        let tool =
            Update { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };

        let empty = tool.call(ToolInput { value: serde_json::json!({ "label": "w1" }) }).await;
        assert!(empty.is_error, "an update naming no field is refused");
        assert!(
            empty.blocks[0].text.contains("at least one"),
            "the refusal says what to supply: {}",
            empty.blocks[0].text,
        );
        assert!(host.workers.update_calls.lock().is_empty(), "nothing reached the facade");

        let supplied = tool
            .call(ToolInput {
                value: serde_json::json!({ "label": "w1", "charter": "new mission" }),
            })
            .await;
        assert!(!supplied.is_error, "one supplied field is enough: {:?}", supplied.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&supplied.blocks[0].text).expect("JSON");
        assert_eq!(
            parsed["updated"],
            serde_json::json!(["charter"]),
            "only the supplied field is reported as changed",
        );
    }

    #[tokio::test]
    async fn despawn_reports_a_blocked_worktree_as_a_result() {
        // A dirty worktree leaves the worker live, so this is a result
        // the caller has to read and act on rather than a tool error.
        let host = host();
        *host.workers.despawn_outcome.lock() =
            Some(DespawnOutcome::Blocked { reason: "3 uncommitted files".to_owned() });
        let tool =
            Despawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool.call(ToolInput { value: serde_json::json!({ "label": "w1" }) }).await;
        assert!(!output.is_error, "a blocked despawn is not a tool error: {:?}", output.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert_eq!(parsed["status"], "blocked");
        assert_eq!(parsed["reason"], "3 uncommitted files", "the reason reaches the caller");
        assert!(
            !host.workers.despawn_calls.lock()[0].2,
            "an omitted force means the worktree is protected",
        );
    }

    #[tokio::test]
    async fn despawn_carries_a_cleanup_warning_into_the_result() {
        let host = host();
        *host.workers.despawn_outcome.lock() = Some(DespawnOutcome::Despawned {
            worktree_cleanup_warning: Some("the directory lingers".to_owned()),
            branch_cleanup_warning: None,
        });
        let tool =
            Despawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "w1", "force": true }) })
            .await;
        assert!(!output.is_error, "the despawn must be answered: {:?}", output.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert_eq!(parsed["status"], "despawned", "a clean despawn reports it");
        assert_eq!(
            parsed["worktree_cleanup_warning"], "the directory lingers",
            "the worktree warning reaches the caller",
        );
        assert!(
            parsed.get("branch_cleanup_warning").is_none(),
            "a warning that did not fire is absent, not null",
        );
        assert!(host.workers.despawn_calls.lock()[0].2, "force reaches the facade");
    }

    #[tokio::test]
    async fn update_forwards_only_the_supplied_fields() {
        // Every field is optional, so an omitted one must arrive as `None`
        // rather than as an empty string - which the store would write,
        // blanking a text the caller never meant to touch.
        let host = host();
        let tool =
            Update { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "w1",
                    "resume_kick": "re-read the plan first",
                }),
            })
            .await;
        assert!(!output.is_error, "the update must succeed: {:?}", output.blocks);
        let calls = host.workers.update_calls.lock();
        assert_eq!(calls.len(), 1, "the facade saw one update");
        assert_eq!(calls[0].1, "w1", "the label reaches the facade");
        assert_eq!(calls[0].2, None, "an omitted charter stays None");
        assert_eq!(calls[0].3, None, "an omitted kick stays None");
        assert_eq!(
            calls[0].4.as_deref(),
            Some("re-read the plan first"),
            "the supplied resume_kick reaches the facade",
        );
        assert!(
            output.blocks[0].text.contains("resume_kick"),
            "the reply names the changed field: {}",
            output.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn update_refuses_when_the_worker_does_not_exist() {
        // Update revises a row; it must never create one, because a row is
        // what re-spawns a worker at the next lead connect.
        let host = host();
        *host.workers.update_result.lock() = Some(Err(WorkerUpdateError::NoSuchWorker {
            label: "ghost".to_owned(),
            project_key: "core".to_owned(),
        }));
        let tool =
            Update { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "ghost", "charter": "c" }) })
            .await;
        assert!(output.is_error, "an absent worker is refused, not reported as revised");
        assert!(
            output.blocks[0].text.contains("agents__spawn"),
            "the refusal points at spawn: {}",
            output.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn update_refuses_a_field_that_is_empty_after_trim() {
        let host = host();
        let tool =
            Update { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "w1", "kick": "  \n " }) })
            .await;
        assert!(output.is_error, "a whitespace-only field is refused");
        assert!(
            output.blocks[0].text.contains("kick must be non-empty after trim"),
            "the refusal names the offending field: {}",
            output.blocks[0].text,
        );
        assert!(host.workers.update_calls.lock().is_empty(), "refused before touching the store");
    }

    #[tokio::test]
    async fn update_refuses_a_label_that_is_empty_after_trim() {
        // On the facade this tool runs against, `update_worker` checks no
        // label and defaults to `Ok(())`, so without this guard a blank
        // label would be reported as a successful revision.
        let host = host();
        let tool =
            Update { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "   ", "charter": "c" }) })
            .await;
        assert!(output.is_error, "a blank label is refused, not accepted as a revision");
        assert!(
            output.blocks[0].text.contains("label must be non-empty after trim"),
            "the refusal names the label: {}",
            output.blocks[0].text,
        );
        assert!(host.workers.update_calls.lock().is_empty(), "refused before touching the store");
    }

    #[tokio::test]
    async fn update_is_lead_only() {
        let host = host();
        let tool = Update {
            facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>,
            slot: worker_caller(),
        };
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "w1", "charter": "c" }) })
            .await;
        assert!(output.is_error, "a worker caller is refused");
        assert!(host.workers.update_calls.lock().is_empty(), "refused before touching the store");
    }

    #[tokio::test]
    async fn despawn_non_lead_caller_is_error() {
        let host = host();
        let tool = Despawn {
            facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>,
            slot: worker_caller(),
        };
        let output = tool.call(ToolInput { value: serde_json::json!({ "label": "w1" }) }).await;
        assert!(output.is_error, "a worker caller is refused");
        assert!(
            output.blocks[0].text.to_lowercase().contains("lead-only"),
            "the refusal says why: {}",
            output.blocks[0].text,
        );
        assert!(host.workers.despawn_calls.lock().is_empty(), "refused before the facade");
    }

    #[tokio::test]
    async fn despawn_empty_label_is_error() {
        let host = host();
        let tool =
            Despawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool.call(ToolInput { value: serde_json::json!({ "label": "   " }) }).await;
        assert!(output.is_error, "a blank label is refused");
        assert!(
            output.blocks[0].text.to_lowercase().contains("label"),
            "the refusal names the label: {}",
            output.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn despawn_unknown_label_is_error() {
        // The failure this pins: reporting `despawned` for a label that
        // resolved to nothing tells a lead its worker is gone while it
        // keeps coming back on the next reconnect.
        let host = host();
        let tool =
            Despawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool.call(ToolInput { value: serde_json::json!({ "label": "ghost" }) }).await;
        assert!(output.is_error, "a label with no live worker is refused, not reported as gone");
        assert!(
            output.blocks[0].text.contains("ghost"),
            "the refusal names the label: {}",
            output.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn despawn_surfaces_a_branch_cleanup_warning() {
        // The description promises this key, so a kept branch has to reach
        // the caller on an otherwise successful despawn.
        let host = host();
        *host.workers.despawn_outcome.lock() = Some(DespawnOutcome::Despawned {
            worktree_cleanup_warning: None,
            branch_cleanup_warning: Some("branch 'worktree-w1' kept: 2 commits".to_owned()),
        });
        let tool =
            Despawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };
        let output = tool.call(ToolInput { value: serde_json::json!({ "label": "w1" }) }).await;
        assert!(!output.is_error, "a kept branch is not an error: {:?}", output.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert_eq!(parsed["status"], "despawned", "a despawn with a kept branch still reports it");
        assert!(
            parsed["branch_cleanup_warning"]
                .as_str()
                .expect("the branch warning is present")
                .contains("worktree-w1"),
            "the warning names the branch: {parsed}",
        );
    }

    #[tokio::test]
    async fn a_target_naming_an_unknown_project_is_refused() {
        let host = host();
        let tool = SendMessage { dispatcher: Arc::clone(&host.dispatcher), slot: caller() };
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

    fn spawn_tool(host: &Host) -> Spawn {
        Spawn { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() }
    }

    fn preloaded_reply(families: Option<Vec<String>>) -> WorkerSpawnReply {
        WorkerSpawnReply {
            session_id: "s-1".to_owned(),
            tag: "forge:worker:reviewer".to_owned(),
            mcp_families: families,
            rate_limited_account: None,
            durability_warning: None,
            session_choice: SessionChoice::Fresh,
        }
    }

    /// The spawn argument reaches the facade in canonical wire order,
    /// and the reply echoes exactly what the worker got.
    #[tokio::test]
    async fn spawn_families_are_canonicalised_and_echoed() {
        let host = host();
        *host.workers.spawn_reply.lock() =
            Some(Ok(preloaded_reply(Some(vec!["tasks".to_owned(), "slack".to_owned()]))));
        let tool = spawn_tool(&host);

        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "reviewer",
                    "charter": "c",
                    "mcp_families": ["slack", "tasks"],
                }),
            })
            .await;

        assert!(!output.is_error, "{:?}", output.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert_eq!(
            parsed["mcp_families"],
            serde_json::json!(["tasks", "slack"]),
            "the echo names the canonical order"
        );
        assert_eq!(
            host.workers.spawn_calls.lock()[0].7,
            Some(vec!["tasks".to_owned(), "slack".to_owned()]),
            "the facade gets the canonical list"
        );
    }

    /// No families named: the facade hears `None` and the reply carries
    /// no key at all - absent means every family throughout.
    #[tokio::test]
    async fn spawn_without_families_sends_none_and_omits_the_key() {
        let host = host();
        *host.workers.spawn_reply.lock() = Some(Ok(preloaded_reply(None)));
        let tool = spawn_tool(&host);

        let output = tool
            .call(ToolInput { value: serde_json::json!({"label": "reviewer", "charter": "c"}) })
            .await;

        assert!(!output.is_error, "{:?}", output.blocks);
        let parsed: serde_json::Value = serde_json::from_str(&output.blocks[0].text).expect("JSON");
        assert!(parsed.get("mcp_families").is_none(), "no key when nothing is narrowed: {parsed}");
        assert_eq!(host.workers.spawn_calls.lock()[0].7, None);
    }

    /// A name that is not selectable is refused before anything is
    /// persisted, and the refusal names the selectable set; `agents` is
    /// refused as always-on.
    #[tokio::test]
    async fn spawn_refuses_unselectable_families() {
        for (bad, needle) in [("bogus", "selectable families are"), ("agents", "always on")] {
            let host = host();
            let tool = spawn_tool(&host);

            let output = tool
                .call(ToolInput {
                    value: serde_json::json!({
                        "label": "reviewer",
                        "charter": "c",
                        "mcp_families": [bad],
                    }),
                })
                .await;

            assert!(output.is_error, "{bad} must refuse");
            assert!(output.blocks[0].text.contains(needle), "{bad}: {}", output.blocks[0].text);
            assert!(host.workers.spawn_calls.lock().is_empty(), "{bad} persists nothing");
        }
    }

    /// Update's family contract: a list replaces, an empty list resets,
    /// an absent argument leaves the stored selection alone.
    #[tokio::test]
    async fn update_families_replace_reset_or_stay() {
        let host = host();
        let tool =
            Update { facade: Arc::clone(&host.workers) as Arc<dyn WorkerFacade>, slot: caller() };

        let output = tool
            .call(ToolInput { value: serde_json::json!({"label": "w1", "mcp_families": ["cron"]}) })
            .await;
        assert!(!output.is_error, "{:?}", output.blocks);
        assert!(
            output.blocks[0].text.contains("mcp_families"),
            "the update names what changed: {}",
            output.blocks[0].text
        );
        assert_eq!(host.workers.update_calls.lock()[0].5, Some(vec!["cron".to_owned()]));

        let output = tool
            .call(ToolInput { value: serde_json::json!({"label": "w1", "mcp_families": []}) })
            .await;
        assert!(!output.is_error, "{:?}", output.blocks);
        assert_eq!(
            host.workers.update_calls.lock()[1].5,
            Some(Vec::new()),
            "empty list is the reset"
        );

        let output = tool
            .call(ToolInput { value: serde_json::json!({"label": "w1", "charter": "c2"}) })
            .await;
        assert!(!output.is_error, "{:?}", output.blocks);
        assert_eq!(
            host.workers.update_calls.lock()[2].5,
            None,
            "absent keeps the stored selection"
        );
    }
}
