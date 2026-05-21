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
    let spawn = Spawn { facade, caller_key };
    McpServerBuilder::new("forge", env!("CARGO_PKG_VERSION")).tool(spawn).build()
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
}
