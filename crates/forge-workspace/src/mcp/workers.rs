//! Workers MCP - project-internal child-agent coordination. Mirror
//! of `crate::mcp::peers`, scoped to within-project addressing by
//! label rather than cross-project addressing by project name.
//!
//! See `docs/superpowers/specs/2026-05-21-workers-mcp-design.md`.

use std::sync::Arc;

use forge_sdk::mcp::server::{McpServer, McpServerBuilder};
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use crate::mcp::peers::facade::CallerKeyResolver;
use crate::mcp::peers::types::{CorrelationId, InflightAsk, WrappedKind, WrappedPrompt};
use crate::mcp::workers::facade::{WorkerDeliverError, WorkerFacade, WorkerSpawnError};

pub mod facade;
pub mod types;

pub use types::WorkerEntry;

/// Default hop limit for forwarded ask/tell chains within a project.
/// Mirrors the peer-MCP value (#114 v1 brainstorm locked at 10).
const HOP_LIMIT: u8 = 10;

/// Build a standalone `forge` MCP server carrying only the four
/// workers-coordination tools. Used in tests for isolated workers-MCP
/// coverage; the production build_site uses
/// [`crate::mcp::build_forge_server`] which combines peers + workers
/// into one server (the CLI rejects duplicate-name MCP servers, so
/// both modules must register their tools through a single builder).
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
    let ask = Ask { facade, caller_key };
    builder.tool(spawn).tool(list).tool(tell).tool(ask)
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
         label, addressing picks the latest-spawned. Use workers__list \
         to see your project's current worker pool. This tool errors \
         if called from a worker session; only the project lead may \
         spawn."
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

        let caller_key = self.caller_key.current();
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
        WorkerSpawnError::EmptyCharter => "charter must be non-empty after trim".to_owned(),
        WorkerSpawnError::UnknownCallerProject => {
            "could not resolve caller to a known project (forge bug)".to_owned()
        }
        WorkerSpawnError::DispatchFailed { message } => {
            format!("worker spawn failed: {message}")
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
        let caller_key = self.caller_key.current();
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
}

#[async_trait::async_trait]
impl Tool for Tell {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "workers__tell"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Send a fire-and-forget message to a worker in YOUR project by \
         label. The message lands as a new user turn in the worker's \
         chat, rendered as an incoming-from-<caller> block. No reply is \
         awaited; use workers__ask if you need an answer back. If \
         multiple workers share the same label, addressing picks the \
         latest-spawned. Available to both lead and worker callers. \
         Run workers__list first to confirm the label and that the \
         worker is live."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "label": {
                    "type": "string",
                    "description": "Worker label from workers__list. Case-sensitive. If multiple workers share the label, the latest-spawned receives the message.",
                },
                "message": {
                    "type": "string",
                    "description": "Message body. Rendered as a new user turn in the worker's chat - write it as direct instructions or context to the worker.",
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

        let caller_key = self.caller_key.current();
        let correlation_id = CorrelationId::new_tell();
        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind: WrappedKind::Message,
            sender_name: caller_key.as_str().to_owned(),
            sender_org: String::new(),
            hop: 1,
            hop_limit: HOP_LIMIT,
            body: args.message,
        };

        match self.facade.deliver_worker_prompt(&caller_key, &args.label, wrapped) {
            Ok(_) => {
                let body = serde_json::json!({
                    "correlation_id": correlation_id.as_str(),
                    "status": "delivered",
                });
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_deliver_error(&args.label, &err)),
        }
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
         for the reply. The worker's LLM will see your question as a \
         new user turn, do its work, and respond. The reply lands as \
         a fresh user turn in YOUR chat whenever it's ready - finish \
         your current turn naturally and continue with other work. \
         Multiple asks can run in parallel - fire several workers__ask \
         calls in one turn and the replies arrive independently. \
         Available to both lead and worker callers. If multiple \
         workers share the label, addressing picks the latest-spawned. \
         Run workers__list first to confirm the label and that the \
         worker is live."
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

        let caller_key = self.caller_key.current();
        let correlation_id = CorrelationId::new_ask();

        let wrapped = WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind: WrappedKind::Question,
            sender_name: caller_key.as_str().to_owned(),
            sender_org: String::new(),
            hop: 1,
            hop_limit: HOP_LIMIT,
            body: args.question,
        };

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
            caller_project: caller_key.as_str().to_owned(),
            caller_org: String::new(),
            target_project: args.label.clone(),
        });

        match self.facade.deliver_worker_prompt(&caller_key, &args.label, wrapped) {
            Ok(_) => {
                let body = serde_json::json!({
                    "correlation_id": correlation_id.as_str(),
                    "status": "delivered",
                });
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Err(err) => {
                // Rollback: the dispatch never reached the worker so
                // the inflight_asks entry would otherwise leak.
                self.facade.complete_inflight_ask(&correlation_id);
                tool_error(format_deliver_error(&args.label, &err))
            }
        }
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
        assert!(tool.description().to_lowercase().contains("fire-and-forget"));
        let schema = tool.input_schema();
        let required = schema["required"].as_array().expect("required field present");
        assert!(required.iter().any(|v| v == "label"));
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
    fn build_server_registers_all_four_workers_tools() {
        let mock = MockWorkerFacade::new();
        let facade = mock.into_arc();
        let server = build_server(facade, CallerKeyResolver::from_fixed(fake_key("test")));
        let debug = format!("{server:?}");
        for expected in ["workers__spawn", "workers__list", "workers__tell", "workers__ask"] {
            assert!(
                debug.contains(expected),
                "build_server must include {expected}; debug: {debug}",
            );
        }
    }
}
