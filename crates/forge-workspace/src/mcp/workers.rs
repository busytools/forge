//! Workers MCP - project-internal child-agent coordination. Mirror
//! of `crate::mcp::peers`, scoped to within-project addressing by
//! label rather than cross-project addressing by project name.
//!
//! See `docs/superpowers/specs/2026-05-21-workers-mcp-design.md`.

use std::sync::Arc;

use forge_sdk::mcp::server::{McpServer, McpServerBuilder};
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use crate::mcp::peers::facade::CallerKeyResolver;
use crate::mcp::workers::facade::{WorkerFacade, WorkerSpawnError};

pub mod facade;
pub mod types;

pub use types::WorkerEntry;

/// Build the per-session workers MCP server with all four workers
/// tools closure-bound to `caller_key`. Server is named `forge` so
/// tool names render to the LLM as `mcp__forge__workers__<name>` -
/// matching the spec namespace and the auto-approve fast-path in
/// `forge-sdk`'s `control_dispatch` which matches `mcp__forge__`
/// at the tool-name level. The peer-coordination server uses the
/// same `forge` name; task 12 reconciles registration so both sets
/// of tools surface together.
pub fn build_server(facade: Arc<dyn WorkerFacade>, caller_key: CallerKeyResolver) -> McpServer {
    let spawn = Spawn { facade: facade.clone(), caller_key: caller_key.clone() };
    let list = List { facade, caller_key };
    McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")).tool(spawn).tool(list).build()
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
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
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
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
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
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
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
        let tool = Spawn { facade, caller_key: CallerKeyResolver::from_fixed(fake_key("lead-key")) };
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
}
