//! Workers MCP - project-internal child-agent coordination. Mirror
//! of `crate::mcp::peers`, scoped to within-project addressing by
//! label rather than cross-project addressing by project name.

use std::sync::Arc;

#[cfg(any(test, feature = "testing"))]
use forge_sdk::mcp::server::McpServer;
use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use crate::mcp::peers::facade::{CallerKeyResolver, PeerStatsDelta};
use crate::mcp::peers::types::{
    AskChannel, CorrelationId, InflightAsk, ReplyRouting, WrappedKind, WrappedPrompt,
};
use crate::mcp::workers::facade::{
    DespawnOutcome, LEAD_LABEL, WorkerDeliverError, WorkerDespawnError, WorkerFacade,
    WorkerLeadDeliverError, WorkerSpawnError, WorkerUpdateError,
};

pub mod facade;
pub mod types;

/// Composite key stored in `InflightAsk.target_project` for worker-
/// bound asks. The `::` separator can never appear in a real project
/// name (forge.toml validation rejects it), so this shape is
/// distinguishable from peer-MCP's plain-project-name fill at zero
/// schema cost. `expire_inflight_for_closed_worker` matches on the
/// composite when a worker's session is torn down.
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
    let despawn = Despawn { facade: facade.clone(), caller_key: caller_key.clone() };
    let update = Update { facade, caller_key };
    builder.tool(spawn).tool(list).tool(tell).tool(ask).tool(despawn).tool(update)
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
/// - `resume_kick` (string, optional) - re-orient message used in place
///   of the generic restart note whenever this worker is resumed
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
    #[serde(default)]
    kick: Option<String>,
    #[serde(default)]
    resume_kick: Option<String>,
    #[serde(default)]
    interactive: bool,
}

#[async_trait::async_trait]
impl Tool for Spawn {
    fn name(&self) -> &'static str {
        "workers__spawn"
    }

    fn description(&self) -> &'static str {
        "Spawn a new worker session inside YOUR project (lead-only). \
         The worker is a full forge session - its own claude subprocess, \
         own chat view, own permissions - addressable from your session \
         by `label` via workers__tell / workers__ask. `charter` is the \
         worker's mission, threaded into the new session's system \
         prompt, and defines what that worker is. PROVIDE `kick` TO START THE WORKER IMMEDIATELY: \
         the kick is delivered as the worker's first user-turn the moment \
         it connects, so it begins working at once. WITHOUT a kick the \
         worker sits idle until you send it a workers__tell - a 'begin \
         now' line in the charter does NOT run on its own, so pass `kick` \
         for any ad-hoc spawn you want to start now. Returns the worker's \
         session_id and tag (`forge:worker:<label>`). A spawned worker is \
         DURABLE: it survives forge restarts and is automatically \
         re-spawned, resuming where it left off (a restarted worker is \
         told to continue, not start over), until you explicitly despawn \
         it with workers__despawn (or close its row in the Projects \
         pane). PASS `resume_kick` FOR A LONG-LIVED WORKER whose restart \
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
         with workers__tell / workers__ask instead of spawning again. \
         The label 'lead' is reserved (used by workers__tell / \
         workers__ask to address the spawning lead) and rejected here. \
         Use workers__list to see your project's current worker pool. \
         This tool errors if called from a worker session; only the \
         project lead may spawn."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Identifier you will use to address this worker later via workers__tell / workers__ask. Non-empty after trim. At most one live worker per label - reusing a label with a live worker is rejected.",
                },
                "charter": {
                    "type": "string",
                    "description": "The worker's mission, threaded into the new session's system prompt. This is what defines the worker, so say what it is responsible for and how it should work. Non-empty after trim.",
                },
                "kick": {
                    "type": "string",
                    "description": "Optional first-turn message delivered to the worker the moment it connects, so it STARTS WORKING IMMEDIATELY (equivalent to sending a workers__tell right after spawn). STRONGLY RECOMMENDED for ad-hoc spawns: WITHOUT a kick the worker sits idle until you send it a workers__tell - a 'begin now' line in the charter does NOT run on its own. Omit only when you intend to drive the worker yourself with a later workers__tell.",
                },
                "resume_kick": {
                    "type": "string",
                    "description": "Optional re-orient message delivered every time this worker is RESUMED after a forge restart, in place of the generic 'continue where you left off' note. For a LONG-LIVED worker whose restart needs specific steps - re-read a file, catch up a queue, check whether something was mid-run before re-running it - rather than a generic continue. Stored at spawn rather than delivered now; the first turn of a fresh spawn is `kick`. Non-empty after trim when provided - to keep the generic restart note, OMIT the argument rather than passing an empty string, which is rejected.",
                },
                "interactive": {
                    "type": "boolean",
                    "description": "Set true ONLY when the user asked for a worker they will talk to DIRECTLY and will have its row open. It keeps the built-in AskUserQuestion tool, which every other worker is denied: a worker's question renders in its own row, which nobody is usually watching, and an answer that does arrive is indistinguishable from a decision the user actually made - so a worker can attribute a choice to the user in good faith that the user never saw. Defaults to false, which is right for any worker you are spawning on your own initiative; that worker reaches the user through you, via its workers__ask('lead', ...). This is fixed at spawn - changing it means despawning the worker and spawning it again.",
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
        // An empty one is `Some`, so it would beat the restart-note
        // fallback and dispatch a blank first turn on every resume - the
        // same contract `workers__update` holds this arg to.
        if args.resume_kick.as_ref().is_some_and(|text| text.trim_end().is_empty()) {
            return tool_error("resume_kick must be non-empty after trim when provided".to_owned());
        }
        match self
            .facade
            .spawn_worker(
                &caller_key,
                args.label,
                args.charter,
                args.kick,
                args.resume_kick,
                args.interactive,
            )
            .await
        {
            Ok(reply) => {
                let mut body = serde_json::json!({
                    "session_id": reply.session_id,
                    "tag": reply.tag,
                });
                if let Some(account) = &reply.rate_limited_account {
                    body["notice"] = serde_json::Value::String(format!(
                        "assigned account '{account}' is currently rate-limited or bailed. The worker spawns anyway but may hit a 429 right away; free up an account or wait for a reset."
                    ));
                }
                if let Some(warning) = &reply.durability_warning {
                    body["durability_warning"] = serde_json::Value::String(warning.clone());
                }
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

/// `workers__despawn` - lead-only. Closes a worker by label and cleans
/// up its git worktree. A clean worktree is removed; a dirty one
/// (uncommitted/untracked or unpushed commits) blocks the despawn
/// unless `force`. The `worktree-<label>` branch behind the worktree is
/// reaped with it when no commit would become unreachable.
///
/// Arguments:
/// - `label` (string, required) - the worker to close, from `workers__list`
/// - `force` (bool, optional, default false) - discard a dirty worktree
///
/// Returns `{ "status": "despawned" }` (optionally with a
/// `worktree_cleanup_warning` or a `branch_cleanup_warning`), or
/// `{ "status": "blocked", "reason": ... }`.
pub(crate) struct Despawn {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) caller_key: CallerKeyResolver,
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
        "workers__despawn"
    }

    fn description(&self) -> &'static str {
        "Despawn (close + clean up) a worker in YOUR project by label \
         (lead-only). Kills the worker's claude subprocess, removes it \
         from workers__list, expires any inflight asks addressed to it, \
         AND cleans up its git worktree. A CLEAN worktree is removed as \
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
                    "description": "Worker label from workers__list to close. Non-empty after trim.",
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

        let caller_key = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };

        match self
            .facade
            .despawn_worker(&caller_key, &args.label, args.force.unwrap_or(false))
            .await
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
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Ok(DespawnOutcome::Blocked { reason }) => {
                let body = serde_json::json!({ "status": "blocked", "reason": reason });
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_despawn_error(&err)),
        }
    }
}

fn format_despawn_error(err: &WorkerDespawnError) -> String {
    match err {
        WorkerDespawnError::NotLeadCaller => {
            "workers__despawn is lead-only; this session is a worker. Only the project lead may despawn workers.".to_owned()
        }
        WorkerDespawnError::EmptyLabel => "label must be non-empty after trim".to_owned(),
        WorkerDespawnError::UnknownCallerProject => {
            "could not resolve caller to a known project (forge bug)".to_owned()
        }
        WorkerDespawnError::UnknownLabel { label, project_key } => format!(
            "no live worker with label '{label}' in project '{project_key}'. Call workers__list to see the current pool."
        ),
        WorkerDespawnError::DispatchFailed { message } => {
            format!("worker despawn failed: {message}")
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
    fn name(&self) -> &'static str {
        "workers__list"
    }

    fn description(&self) -> &'static str {
        "List every worker currently live in YOUR project. Returns a \
         JSON array of worker snapshots (label, full charter, status, \
         activity, session_id, spawned_at, spawned_by_session_id). \
         `status` is the spawn outcome only - it stops moving once a \
         worker connects, so a worker idle for an hour still reads \
         Running there. Read `activity` for what the worker is doing \
         now: Running (a turn is in progress), Idle (connected, no \
         turn), Attention (blocked on a permission prompt or question \
         it cannot answer itself), Spawning, Failed, or Sleeping \
         (listed but its session is gone). Attention needs you to \
         unblock it. Idle means either the worker has finished and \
         needs a message, or its turn has not started yet - queued \
         behind another turn, or behind the boot kick drainer - so give \
         a freshly spawned or freshly messaged worker a moment before \
         re-sending. And Running is not proof of life: a worker that \
         died mid-turn keeps reading Running until forge restarts, so \
         check a long silent Running by asking the worker rather than \
         trusting the field. These workers are \
         durable: they persist across forge restarts and re-spawn \
         automatically until despawned, so this set is what will come \
         back after a restart. Both lead and worker sessions may call \
         this; workers see the same set as the lead. Use the labels \
         from this output as targets for workers__tell / workers__ask. \
         An empty array means no workers are live in your project. \
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
    fn name(&self) -> &'static str {
        "workers__tell"
    }

    fn description(&self) -> &'static str {
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
         is live. A `delivered` status means the queue ACCEPTED the \
         message, not that the target read it - a down or wedged worker \
         still returns delivered, so confirm real work happened by a \
         reply or an observable artifact rather than by the ack."
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

        let identity = self.facade.caller_identity(&caller_key);
        let correlation_id = CorrelationId::new_tell();

        match classify_workers_tell(&*self.facade, in_reply_to_id.as_ref()) {
            ReplyRouting::WrongChannel { correct_tool } => {
                let id = in_reply_to_id.as_ref().map(CorrelationId::as_str).unwrap_or_default();
                tool_error(format!(
                    "this question arrived over the peers channel (from another project). Reply \
                     with `{correct_tool}(target='<project>', in_reply_to={id})`, not \
                     workers__tell - that tool is for your own lead or a worker on your team."
                ))
            }
            ReplyRouting::Reply { caller, correlation } => {
                let wrapped = WrappedPrompt {
                    correlation_id: correlation_id.clone(),
                    kind: WrappedKind::Reply,
                    channel: AskChannel::Workers,
                    sender_name: identity.name,
                    sender_org: identity.org,
                    body: args.message,
                };
                if let Err(err) = self.facade.deliver_reply_to_caller(&caller, &wrapped) {
                    return tool_error(err.user_message());
                }
                // Reply resolved cleanly: close the ask + decrement the
                // replier's incoming and the original asker's outgoing.
                self.facade.complete_inflight_ask(&correlation);
                self.facade.bump_inflight_stats(&caller_key, PeerStatsDelta::IncomingMinus1);
                self.facade.bump_inflight_stats(&caller, PeerStatsDelta::OutgoingMinus1);
                deliver_ok_response(&correlation_id, None)
            }
            ReplyRouting::Message => {
                let wrapped = WrappedPrompt {
                    correlation_id: correlation_id.clone(),
                    kind: WrappedKind::Message,
                    channel: AskChannel::Workers,
                    sender_name: identity.name,
                    sender_org: identity.org,
                    body: args.message,
                };
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
                // Reserved keyword: `label="lead"` routes back to the
                // caller's lead session; other labels address a worker.
                // Lead callers using `lead` get a clear error - leads
                // have no lead. See `LEAD_LABEL` in `mcp::workers::facade`.
                let delivery = if args.label == LEAD_LABEL {
                    self.facade
                        .deliver_prompt_to_lead(&caller_key, wrapped)
                        .map_err(|err| format_lead_deliver_error(&err))
                } else {
                    self.facade
                        .deliver_worker_prompt(&caller_key, &args.label, wrapped)
                        .map_err(|err| format_deliver_error(&args.label, &err))
                };
                match delivery {
                    Ok(_) => deliver_ok_response(&correlation_id, note),
                    Err(msg) => tool_error(msg),
                }
            }
        }
    }
}

/// Classify a `workers__tell` from its optional `in_reply_to`. A
/// resolved same-channel id routes the Reply to the asker's session
/// (the `label` arg is irrelevant once the correlation resolves); a
/// resolved other-channel id is a `WrongChannel` steer; no/unknown id
/// is an unsolicited `Message`.
fn classify_workers_tell(
    facade: &dyn WorkerFacade,
    in_reply_to: Option<&CorrelationId>,
) -> ReplyRouting {
    let Some(id) = in_reply_to else {
        return ReplyRouting::Message;
    };
    let Some(ask) = facade.resolve_correlation(id) else {
        tracing::warn!(
            target: "forge_workspace::mcp::workers",
            correlation_id = id.as_str(),
            "workers__tell in_reply_to references unknown correlation_id; treating as unsolicited Message"
        );
        return ReplyRouting::Message;
    };
    match ask.channel {
        AskChannel::Workers => ReplyRouting::Reply { caller: ask.caller, correlation: id.clone() },
        AskChannel::Peers => {
            ReplyRouting::WrongChannel { correct_tool: AskChannel::Peers.reply_tool() }
        }
    }
}

/// Helper: build the standard `{ correlation_id, status: "delivered" }`
/// response body. Pulled out so both Tell + Ask + lead-delivery paths
/// emit a single canonical shape. `note` carries an optional
/// degraded-reply explanation surfaced when an unknown `in_reply_to`
/// fell through to a plain Message.
fn deliver_ok_response(correlation_id: &CorrelationId, note: Option<String>) -> ToolOutput {
    let mut body = serde_json::json!({
        "correlation_id": correlation_id.as_str(),
        "status": "delivered",
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
        WorkerLeadDeliverError::LeadGone => {
            "your lead is not available (its session closed since this worker was spawned)."
                .to_owned()
        }
    }
}

fn format_deliver_error(label: &str, err: &WorkerDeliverError) -> String {
    match err {
        WorkerDeliverError::UnknownLabel { project_key, .. } => format!(
            "worker '{label}' is not available (no live worker by that label in \
             '{project_key}'); call workers__list to see the current pool."
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
    fn name(&self) -> &'static str {
        "workers__ask"
    }

    fn description(&self) -> &'static str {
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
            channel: AskChannel::Workers,
            sender_name: identity.name,
            sender_org: identity.org,
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
            channel: AskChannel::Workers,
            caller: caller_key.clone(),
            target_project: target_project_composite,
            target_session: None,
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
                Ok(_) => deliver_ok_response(&correlation_id, None),
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
            Ok(_) => deliver_ok_response(&correlation_id, None),
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

/// `workers__update` - lead-only. Revises the stored `charter`, `kick`
/// and `resume_kick` of an EXISTING dynamic-worker row, keyed by
/// `(project_key, label)` exactly as `workers__spawn` persisted it.
///
/// Refuses when no row exists. A row is what makes a worker re-spawn on
/// the next lead connect, so creating one here would mean revising a
/// definition silently produces a worker.
///
/// Arguments:
/// - `label` (string, required) - the worker to revise, as passed to
///   `workers__spawn`
/// - `charter` / `kick` / `resume_kick` (string, optional) - each
///   replaces the stored value; an omitted field is left untouched, and
///   at least one must be supplied
///
/// Returns `{ "label", "updated": [...] }` naming the fields that changed.
pub(crate) struct Update {
    pub(crate) facade: Arc<dyn WorkerFacade>,
    pub(crate) caller_key: CallerKeyResolver,
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
}

#[async_trait::async_trait]
impl Tool for Update {
    fn name(&self) -> &'static str {
        "workers__update"
    }

    fn description(&self) -> &'static str {
        "Revise a worker's stored instructions without despawning it \
         (lead-only). Replaces any of `charter`, `kick` and `resume_kick` \
         on that worker's persisted record; a field you omit keeps its \
         current value, and at least one must be supplied. TAKES EFFECT ON \
         THE WORKER'S NEXT RESPAWN, NOT IMMEDIATELY - a session's system \
         prompt is fixed when the session spawns, so a running worker \
         keeps what it started with; use workers__tell to redirect it now. \
         The worker must already exist: this never creates one, so spawn \
         it with workers__spawn first (which takes the same three texts). \
         Address it by the same `label` you spawned it with, as shown by \
         workers__list."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "The worker to revise, as passed to workers__spawn and listed by workers__list. Non-empty after trim. Must already exist - this never creates a worker.",
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

        let caller_key = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };

        let Some(caller_project) = self.facade.caller_project(&caller_key) else {
            return tool_error("workers__update: caller resolves to no known project".to_owned());
        };
        if !caller_project.is_lead {
            return tool_error(
                "workers__update is lead-only; this session is a worker. Workers cannot revise \
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
        if updated.is_empty() {
            return tool_error(
                "supply at least one of charter, kick or resume_kick; an update with none of \
                 them would change nothing."
                    .to_owned(),
            );
        }

        match self.facade.update_worker(
            &caller_key,
            label,
            args.charter,
            args.kick,
            args.resume_kick,
        ) {
            Ok(()) => {
                let body = serde_json::json!({ "label": label, "updated": updated });
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_update_error(&err)),
        }
    }
}

fn format_update_error(err: &WorkerUpdateError) -> String {
    match err {
        WorkerUpdateError::UnknownCallerProject => {
            "could not resolve caller to a known project (forge bug)".to_owned()
        }
        WorkerUpdateError::NoSuchWorker { label, project_key } => format!(
            "no dynamic worker '{label}' in project '{project_key}'. workers__update revises a \
             worker created by workers__spawn, so if you meant to create one, spawn it first."
        ),
        WorkerUpdateError::StoreFailed { message } => format!("worker update failed: {message}"),
    }
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
            rate_limited_account: None,
            durability_warning: None,
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
                    // missing required 'label' (charter is optional)
                    "charter": "review the diff",
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

    /// A spawn with no `charter` is refused outright rather than
    /// resolving one off disk.
    #[tokio::test]
    async fn spawn_without_charter_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output =
            tool.call(ToolInput { value: serde_json::json!({ "label": "reviewer" }) }).await;
        assert!(output.is_error, "an omitted charter must be refused");
        assert!(
            output.blocks[0].text.to_lowercase().contains("charter"),
            "the refusal names the missing field: {}",
            output.blocks[0].text,
        );
    }

    #[tokio::test]
    async fn spawn_passes_kick_through_to_facade() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "forge:worker:reviewer".into(),
            rate_limited_account: None,
            durability_warning: None,
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "reviewer",
                    "charter": "Review the diff.",
                    "kick": "Begin: triage the failing test now.",
                }),
            })
            .await;
        assert!(!output.is_error, "spawn with kick should not error: {:?}", output.blocks);
        let calls = mock.spawn_calls.lock();
        assert_eq!(
            calls[0].3.as_deref(),
            Some("Begin: triage the failing test now."),
            "kick passes through",
        );
    }

    /// A long-lived worker can carry its own restart instructions; they
    /// are persisted at spawn, not delivered now.
    #[tokio::test]
    async fn spawn_passes_resume_kick_through_to_facade() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "forge:worker:steward".into(),
            rate_limited_account: None,
            durability_warning: None,
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "steward",
                    "charter": "Mind the queues.",
                    "kick": "Begin: drain the queues.",
                    "resume_kick": "Re-read the taste notes, then drain both queues.",
                }),
            })
            .await;
        assert!(!output.is_error, "spawn with resume_kick should not error: {:?}", output.blocks);
        let calls = mock.spawn_calls.lock();
        assert_eq!(
            calls[0].4.as_deref(),
            Some("Re-read the taste notes, then drain both queues."),
            "resume_kick passes through",
        );
    }

    /// An empty `resume_kick` is `Some`, so it would win the restart-note
    /// fallback and dispatch a blank first turn on every future resume.
    /// Refused at the boundary, the way `workers__update` refuses the
    /// same argument.
    #[tokio::test]
    async fn spawn_rejects_whitespace_only_resume_kick() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "t".into(),
            rate_limited_account: None,
            durability_warning: None,
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "steward",
                    "charter": "Mind the queues.",
                    "resume_kick": "   \n  ",
                }),
            })
            .await;
        assert!(output.is_error, "a whitespace-only resume_kick must be refused");
        assert!(
            output.blocks[0].text.contains("resume_kick must be non-empty after trim"),
            "refusal matches the sibling message shape: {}",
            output.blocks[0].text,
        );
        assert!(mock.spawn_calls.lock().is_empty(), "refused before anything spawns");
    }

    #[tokio::test]
    async fn spawn_without_kick_passes_none() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "t".into(),
            rate_limited_account: None,
            durability_warning: None,
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({ "label": "reviewer", "charter": "Review." }),
            })
            .await;
        assert!(!output.is_error);
        assert_eq!(mock.spawn_calls.lock()[0].3, None, "absent kick is None");
        assert_eq!(
            mock.spawn_calls.lock()[0].4,
            None,
            "absent resume_kick is None, so the restart note stays the default",
        );
        assert!(
            !mock.spawn_calls.lock()[0].5,
            "absent interactive means the worker is not offered AskUserQuestion",
        );
    }

    /// The lead is the party that knows whether the user asked for a
    /// worker they will talk to directly, so the opt-in is an argument
    /// rather than a default.
    #[tokio::test]
    async fn spawn_passes_interactive_through_to_facade() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "forge:worker:pairing".into(),
            rate_limited_account: None,
            durability_warning: None,
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "pairing",
                    "charter": "Pair with Ved on the parser.",
                    "interactive": true,
                }),
            })
            .await;
        assert!(!output.is_error, "spawn with interactive should not error: {:?}", output.blocks);
        assert!(mock.spawn_calls.lock()[0].5, "interactive passes through");
    }

    #[tokio::test]
    async fn spawn_surfaces_rate_limited_account_as_notice() {
        // When the adhoc assignment fell back onto a rate-limited
        // account, the spawn tool result carries a `notice` naming it so
        // the lead sees the situation at spawn.
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "forge:worker:reviewer".into(),
            rate_limited_account: Some("gateway".into()),
            durability_warning: None,
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({ "label": "reviewer", "charter": "Review." }),
            })
            .await;
        assert!(!output.is_error);
        let body: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid json body");
        let notice = body["notice"].as_str().expect("notice present when rate-limited");
        assert!(notice.contains("gateway"), "notice names the rate-limited account: {notice}");
    }

    #[tokio::test]
    async fn spawn_surfaces_durability_warning_when_persist_failed() {
        // A failed durability persist (store down / write error) still
        // spawns the worker, but the tool result carries a
        // durability_warning so the lead knows it won't survive a restart.
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "forge:worker:reviewer".into(),
            rate_limited_account: None,
            durability_warning: Some(
                "spawned, but persisting this worker for durability failed".into(),
            ),
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({ "label": "reviewer", "charter": "Review." }),
            })
            .await;
        assert!(!output.is_error, "a persist failure does not fail the spawn");
        let body: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid json body");
        let warning = body["durability_warning"]
            .as_str()
            .expect("durability_warning present on persist fail");
        assert!(warning.contains("durability"), "warning explains the durability gap: {warning}");
    }

    #[tokio::test]
    async fn spawn_omits_durability_warning_on_success() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "forge:worker:reviewer".into(),
            rate_limited_account: None,
            durability_warning: None,
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({ "label": "reviewer", "charter": "Review." }),
            })
            .await;
        assert!(!output.is_error);
        let body: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid json body");
        assert!(body.get("durability_warning").is_none(), "no warning when persistence succeeds");
    }

    #[tokio::test]
    async fn spawn_omits_notice_when_account_usable() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        *mock.spawn_reply.lock() = Some(Ok(WorkerSpawnReply {
            session_id: "u".into(),
            tag: "forge:worker:reviewer".into(),
            rate_limited_account: None,
            durability_warning: None,
        }));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({ "label": "reviewer", "charter": "Review." }),
            })
            .await;
        assert!(!output.is_error);
        let body: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid json body");
        assert!(body.get("notice").is_none(), "no notice when the account is usable");
    }

    #[test]
    fn spawn_schema_has_optional_kick() {
        let mock = MockWorkerFacade::new();
        let facade = mock.into_arc();
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("k")) };
        let schema = tool.input_schema();
        assert!(
            schema["properties"].as_object().expect("properties").contains_key("kick"),
            "schema exposes the kick property"
        );
        let required = schema["required"].as_array().expect("required present");
        assert!(required.iter().all(|v| v != "kick"), "kick is optional");
    }

    #[tokio::test]
    async fn despawn_lead_caller_succeeds() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("reviewer", "c")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Despawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output =
            tool.call(ToolInput { value: serde_json::json!({ "label": "reviewer" }) }).await;
        assert!(!output.is_error, "despawn happy path should not error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert_eq!(parsed["status"], "despawned");
        assert!(!mock.despawn_calls.lock()[0].2, "force defaults to false");
    }

    #[tokio::test]
    async fn despawn_non_lead_caller_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("worker-key"), worker_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Despawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("worker-key")) };
        let output = tool.call(ToolInput { value: serde_json::json!({ "label": "x" }) }).await;
        assert!(output.is_error, "non-lead caller must surface as is_error");
        assert!(output.blocks[0].text.to_lowercase().contains("lead-only"));
    }

    #[tokio::test]
    async fn despawn_empty_label_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        let facade = mock.into_arc();
        let tool =
            Despawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool.call(ToolInput { value: serde_json::json!({ "label": "   " }) }).await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.to_lowercase().contains("label"));
    }

    #[tokio::test]
    async fn despawn_unknown_label_is_error() {
        let mock = MockWorkerFacade::new();
        mock.callers.lock().insert(fake_key("lead-key"), lead_caller("forge"));
        // No workers preloaded -> the label resolves to nothing.
        let facade = mock.into_arc();
        let tool =
            Despawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
        let output = tool.call(ToolInput { value: serde_json::json!({ "label": "ghost" }) }).await;
        assert!(output.is_error);
        assert!(output.blocks[0].text.contains("ghost"));
    }

    #[tokio::test]
    async fn despawn_blocked_surfaces_reason() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("reviewer", "c")]);
        *mock.despawn_outcome.lock() =
            Some(DespawnOutcome::Blocked { reason: "2 unpushed commits".into() });
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Despawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output =
            tool.call(ToolInput { value: serde_json::json!({ "label": "reviewer" }) }).await;
        assert!(!output.is_error, "blocked is a normal outcome, not an error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert_eq!(parsed["status"], "blocked");
        assert!(parsed["reason"].as_str().expect("reason present").contains("unpushed"));
    }

    /// A kept branch has to reach the caller, so the warning renders as
    /// its own key on an otherwise successful despawn.
    #[tokio::test]
    async fn despawn_surfaces_branch_cleanup_warning() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("reviewer", "c")]);
        *mock.despawn_outcome.lock() = Some(DespawnOutcome::Despawned {
            worktree_cleanup_warning: None,
            branch_cleanup_warning: Some("branch 'worktree-reviewer' kept: 2 commits".into()),
        });
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Despawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output =
            tool.call(ToolInput { value: serde_json::json!({ "label": "reviewer" }) }).await;
        assert!(!output.is_error, "a kept branch is not an error: {:?}", output.blocks);
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        assert_eq!(parsed["status"], "despawned");
        assert!(
            parsed["branch_cleanup_warning"]
                .as_str()
                .expect("branch warning present")
                .contains("worktree-reviewer"),
            "the warning names the branch: {parsed}"
        );
    }

    #[tokio::test]
    async fn despawn_force_passes_through() {
        let mock = Arc::new(MockWorkerFacade::new());
        let caller = fake_key("lead-key");
        mock.callers.lock().insert(caller.clone(), lead_caller("forge"));
        mock.workers.lock().insert("forge".into(), vec![fake_worker("reviewer", "c")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Despawn { facade, caller_key: CallerKeyResolver::from_fixed(caller) };
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "reviewer", "force": true }) })
            .await;
        assert!(!output.is_error);
        assert!(mock.despawn_calls.lock()[0].2, "force=true passes through to the facade");
    }

    #[test]
    fn despawn_metadata_shape() {
        let mock = MockWorkerFacade::new();
        let facade = mock.into_arc();
        let tool = Despawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("k")) };
        assert_eq!(tool.name(), "workers__despawn");
        assert!(tool.description().to_lowercase().contains("despawn"));
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

    fn fake_worker(label: &str, charter: &str) -> forge_primitives::WorkerStatus {
        forge_primitives::WorkerStatus {
            label: label.to_owned(),
            charter: charter.to_owned(),
            status: forge_primitives::WorkerLiveness::Running,
            session_id: format!("session-{label}"),
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".to_owned(),
            diagnostic: None,
            activity: None,
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
        assert!(
            output.blocks[0].text.contains("not available"),
            "unknown worker label should read as not available: {}",
            output.blocks[0].text,
        );
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
            "workers__despawn",
            "workers__update",
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
            activity: None,
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

    #[tokio::test]
    async fn tell_lead_gone_says_lead_not_available() {
        // Worker caller whose recorded lead session has closed (the
        // mock treats an empty spawned_by as LeadGone). The error must
        // read as the lead being not available.
        let mock = Arc::new(MockWorkerFacade::new());
        let worker_key = fake_key("worker-uuid");
        mock.callers.lock().insert(worker_key.clone(), worker_caller("forge"));
        mock.workers
            .lock()
            .insert("forge".into(), vec![worker_with_lead("probe", "worker-uuid", "")]);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(worker_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({ "label": "lead", "message": "any news?" }),
            })
            .await;
        assert!(output.is_error);
        assert!(
            output.blocks[0].text.contains("lead is not available"),
            "lead-gone should read as the lead not being available: {}",
            output.blocks[0].text,
        );
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
        target_composite: &str,
    ) -> CorrelationId {
        let id = CorrelationId(correlation_id.to_owned());
        mock.inflight.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: AskChannel::Workers,
                caller,
                target_project: target_composite.to_owned(),
                target_session: None,
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
        let ask_id = register_ask(&mock, "q-deadbeef", worker_key.clone(), "forge::lead");

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
        // A degraded reply carries a note so the replier's LLM learns
        // it landed as a plain message and can retry with the right id.
        let parsed: serde_json::Value =
            serde_json::from_str(&output.blocks[0].text).expect("valid JSON");
        let note = parsed["note"].as_str().expect("degraded reply carries a note");
        assert!(note.contains("q-00000000"), "note names the unresolved id: {note}");
        assert!(
            note.contains("plain message") && note.contains("open ask"),
            "note explains it landed as a plain message and the ask was not open: {note}",
        );
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

    #[tokio::test]
    async fn tell_reply_worker_to_lead_routes_to_asker_by_correlation() {
        // The lead asked a worker via workers__ask (inflight caller =
        // lead). The worker replies with workers__tell(target="lead",
        // in_reply_to=q-y). The reply must route by correlation
        // straight to the asking lead's session and close the ask -
        // the exact airmail scenario, now via the right tool.
        let mock = Arc::new(MockWorkerFacade::new());
        let lead_key = fake_key("lead-uuid");
        let worker_key = fake_key("worker-uuid");
        mock.callers.lock().insert(worker_key.clone(), worker_caller("forge"));
        let ask_id = register_ask(&mock, "q-33334444", lead_key.clone(), "forge::worker-A");
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(worker_key.clone()) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "message": "done, here's the result",
                    "in_reply_to": ask_id.as_str(),
                }),
            })
            .await;
        assert!(!output.is_error, "worker->lead reply must not error: {:?}", output.blocks);
        let replies = mock.reply_to_caller_calls.lock();
        assert_eq!(replies.len(), 1, "reply delivered by session exactly once");
        assert_eq!(replies[0].0, lead_key, "reply routed to the asking lead's session");
        assert!(matches!(replies[0].1.kind, WrappedKind::Reply), "delivered as a Reply");
        drop(replies);
        assert!(mock.inflight.lock().get(&ask_id).is_none(), "inflight ask closed");
        assert_eq!(
            mock.deliver_calls.lock().len(),
            0,
            "a resolved reply must not also fall through to worker/lead delivery",
        );
        let bumps = mock.bumps.lock();
        assert!(
            bumps.iter().any(|(k, d)| *k == worker_key && *d == PeerStatsDelta::IncomingMinus1),
            "replier's incoming decrements: {bumps:?}",
        );
        assert!(
            bumps.iter().any(|(k, d)| *k == lead_key && *d == PeerStatsDelta::OutgoingMinus1),
            "asking lead's outgoing decrements: {bumps:?}",
        );
    }

    #[tokio::test]
    async fn tell_reply_from_peers_channel_is_steered_to_peers_tell() {
        // A peers-channel ask is open. Replying to it via workers__tell
        // must be rejected with a steer to peers__tell_agent, never
        // delivered as a worker message.
        let mock = Arc::new(MockWorkerFacade::new());
        let lead_key = fake_key("lead-uuid");
        mock.callers.lock().insert(lead_key.clone(), lead_caller("forge"));
        mock.inflight.lock().insert(
            CorrelationId("q-77778888".to_owned()),
            InflightAsk {
                correlation_id: CorrelationId("q-77778888".to_owned()),
                channel: AskChannel::Peers,
                caller: fake_key("some-peer"),
                target_project: "forge".to_owned(),
                target_session: None,
            },
        );
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(lead_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "worker-A",
                    "message": "reply via the wrong tool",
                    "in_reply_to": "q-77778888",
                }),
            })
            .await;
        assert!(output.is_error, "wrong-channel reply must be a steered error");
        let text = &output.blocks[0].text;
        assert!(text.contains("peers__tell_agent"), "steer names the right tool: {text}");
        assert!(text.contains("q-77778888"), "steer names the correlation id: {text}");
        assert_eq!(mock.reply_to_caller_calls.lock().len(), 0, "no reply delivered");
        assert_eq!(mock.deliver_calls.lock().len(), 0, "no unsolicited delivery");
        assert!(
            mock.inflight.lock().get(&CorrelationId("q-77778888".to_owned())).is_some(),
            "ask stays open",
        );
    }

    #[tokio::test]
    async fn tell_reply_delivery_failure_keeps_ask_open() {
        // A worker->lead reply whose by-session delivery fails (asker's
        // session gone) must surface the error, leave the ask OPEN, and
        // not decrement either counter.
        let mock = Arc::new(MockWorkerFacade::new());
        let lead_key = fake_key("lead-uuid");
        let worker_key = fake_key("worker-uuid");
        mock.callers.lock().insert(worker_key.clone(), worker_caller("forge"));
        let ask_id = register_ask(&mock, "q-99990000", lead_key.clone(), "forge::worker-A");
        *mock.force_reply_error.lock() =
            Some(crate::mcp::peers::facade::ReplyDeliverError::CallerSessionGone);
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Tell { facade, caller_key: CallerKeyResolver::from_fixed(worker_key) };
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "lead",
                    "message": "reply that can't land",
                    "in_reply_to": ask_id.as_str(),
                }),
            })
            .await;
        assert!(output.is_error, "a failed reply must surface as an error");
        assert!(
            output.blocks[0].text.contains("no longer available"),
            "error carries the not-available reply message: {}",
            output.blocks[0].text,
        );
        assert!(mock.inflight.lock().get(&ask_id).is_some(), "the ask must stay open");
        let bumps = mock.bumps.lock();
        assert!(
            !bumps.iter().any(|(_, d)| *d == PeerStatsDelta::IncomingMinus1
                || *d == PeerStatsDelta::OutgoingMinus1),
            "no counters decrement on a failed reply: {bumps:?}",
        );
    }

    fn lead_update_tool(mock: Arc<MockWorkerFacade>) -> Update {
        let lead_key = fake_key("lead-uuid");
        mock.callers.lock().insert(lead_key.clone(), lead_caller("forge"));
        let facade: Arc<dyn WorkerFacade> = mock;
        Update { facade, caller_key: CallerKeyResolver::from_fixed(lead_key) }
    }

    /// Only the supplied fields travel to the store. An omitted one
    /// arrives as `None` so the stored value is left alone rather than
    /// blanked, and the reply names what actually changed.
    #[tokio::test]
    async fn update_forwards_only_the_supplied_fields() {
        let mock = Arc::new(MockWorkerFacade::new());
        let tool = lead_update_tool(mock.clone());
        let output = tool
            .call(ToolInput {
                value: serde_json::json!({
                    "label": "steward",
                    "resume_kick": "Re-read the taste notes first.",
                }),
            })
            .await;
        assert!(!output.is_error, "update must succeed: {:?}", output.blocks);
        let calls = mock.update_calls.lock();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "steward");
        assert_eq!(calls[0].2, None, "an omitted charter stays None");
        assert_eq!(calls[0].3, None, "an omitted kick stays None");
        assert_eq!(calls[0].4.as_deref(), Some("Re-read the taste notes first."));
        assert!(
            output.blocks[0].text.contains("resume_kick"),
            "the reply names the changed field: {}",
            output.blocks[0].text,
        );
    }

    /// An update against a label with no persisted row is refused and
    /// points at spawn. It must never bring a worker into existence,
    /// since a row is what re-spawns one at the next lead connect.
    #[tokio::test]
    async fn update_refuses_when_the_worker_does_not_exist() {
        let mock = Arc::new(MockWorkerFacade::new());
        *mock.update_result.lock() = Some(Err(WorkerUpdateError::NoSuchWorker {
            label: "ghost".to_owned(),
            project_key: "forge".to_owned(),
        }));
        let tool = lead_update_tool(mock.clone());
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "ghost", "charter": "c" }) })
            .await;
        assert!(output.is_error, "an absent worker must be refused");
        let text = &output.blocks[0].text;
        assert!(text.contains("workers__spawn"), "the refusal points at spawn: {text}");
        assert!(
            !text.contains("forge-team"),
            "must not send the caller to files that no longer exist: {text}",
        );
        assert!(
            !text.contains("workers__list"),
            "must not point at the tool that lists the worker it just denied: {text}",
        );
    }

    #[tokio::test]
    async fn update_refuses_when_no_field_is_supplied() {
        let mock = Arc::new(MockWorkerFacade::new());
        let tool = lead_update_tool(mock.clone());
        let output =
            tool.call(ToolInput { value: serde_json::json!({ "label": "steward" }) }).await;
        assert!(output.is_error, "an update that would change nothing must be refused");
        assert!(mock.update_calls.lock().is_empty(), "refused before touching the store");
    }

    #[tokio::test]
    async fn update_refuses_a_field_that_is_empty_after_trim() {
        let mock = Arc::new(MockWorkerFacade::new());
        let tool = lead_update_tool(mock.clone());
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "steward", "kick": "   \n " }) })
            .await;
        assert!(output.is_error, "a whitespace-only field must be refused");
        assert!(
            output.blocks[0].text.contains("kick must be non-empty after trim"),
            "the refusal names the offending field: {}",
            output.blocks[0].text,
        );
        assert!(mock.update_calls.lock().is_empty(), "refused before touching the store");
    }

    #[tokio::test]
    async fn update_is_lead_only() {
        let mock = Arc::new(MockWorkerFacade::new());
        let worker_key = fake_key("worker-uuid");
        mock.callers.lock().insert(worker_key.clone(), worker_caller("forge"));
        let facade: Arc<dyn WorkerFacade> = mock.clone();
        let tool = Update { facade, caller_key: CallerKeyResolver::from_fixed(worker_key) };
        let output = tool
            .call(ToolInput { value: serde_json::json!({ "label": "steward", "charter": "c" }) })
            .await;
        assert!(output.is_error, "a worker caller must be refused");
        assert!(mock.update_calls.lock().is_empty(), "refused before touching the store");
    }
}
