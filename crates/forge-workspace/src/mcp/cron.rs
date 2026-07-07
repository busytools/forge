//! Cron MCP - durable scheduled prompts (`mcp__forge__cron__*`).
//!
//! A forge cron fires a prompt into a project's session on a schedule
//! and survives forge restarts (persisted to `forge/cron.toml`; see
//! [`crate::cron_store`]). Unlike the cloud routines (`create_trigger` /
//! `CronCreate`), which fire into cloud-hosted sessions, these durably
//! target the local forge process.
//!
//! The tools (`cron__create` / `cron__list` / `cron__delete`) are
//! ANY-CALLER, scoped to the caller's own project - mirroring
//! `workers__list`, not the lead-only `workers__spawn`. Cron-list
//! mutations are direct `Workspace` methods (state writes), not
//! Command-bus dispatches.
//!
//! - [`schedule`] - pure due-check, next-fire, and boot catch-up math.
//! - [`facade`] - the `CronFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

use std::sync::Arc;
use std::time::SystemTime;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use forge_primitives::cron::{CronEntry, CronId, CronKind};

use crate::mcp::cron::facade::{CronCreateError, CronDeleteError, CronFacade};
use crate::mcp::peers::facade::CallerKeyResolver;

pub(crate) mod facade;
pub(crate) mod schedule;

/// Attach the three cron-coordination tools to an existing
/// [`McpServerBuilder`]. Called for BOTH lead and worker sessions (crons
/// are any-caller), so `build_forge_server` invokes this unconditionally.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn CronFacade>,
    caller_key: CallerKeyResolver,
) -> McpServerBuilder {
    let create = Create { facade: facade.clone(), caller_key: caller_key.clone() };
    let list = List { facade: facade.clone(), caller_key: caller_key.clone() };
    let delete = Delete { facade, caller_key };
    builder.tool(create).tool(list).tool(delete)
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput { blocks: vec![ToolOutputBlock { text }], is_error: true }
}

/// Parse an RFC3339 timestamp to a `SystemTime` via the `time` crate
/// (chrono stays confined to the schedule module).
fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(SystemTime::from)
}

/// Format a `SystemTime` as a UTC RFC3339 string for tool output.
fn fmt_rfc3339(t: SystemTime) -> String {
    time::OffsetDateTime::from(t)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Readable JSON for one cron entry (the tool-output shape the LLM sees).
fn cron_to_json(entry: &CronEntry) -> serde_json::Value {
    let schedule = match &entry.kind {
        CronKind::Recurring(expr) => serde_json::json!({ "recurring": expr }),
        CronKind::Once(at) => serde_json::json!({ "once_at": fmt_rfc3339(*at) }),
    };
    serde_json::json!({
        "id": entry.id.as_str(),
        "project": entry.project_name,
        "schedule": schedule,
        "prompt": entry.prompt,
        "next_fire": fmt_rfc3339(entry.next_fire),
    })
}

fn format_create_error(err: &CronCreateError) -> String {
    match err {
        CronCreateError::UnknownCallerProject => {
            "couldn't resolve your project; is this session attached to a forge.toml project?"
                .to_owned()
        }
        CronCreateError::InvalidExpression(msg) => format!("invalid cron expression: {msg}"),
        CronCreateError::NoUpcomingOccurrence => {
            "that schedule has no upcoming occurrence (a run-once time in the past, or a cron \
             expression that never matches)"
                .to_owned()
        }
    }
}

struct Create {
    facade: Arc<dyn CronFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct CreateArgs {
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    run_once_at: Option<String>,
    prompt: String,
}

#[async_trait::async_trait]
impl Tool for Create {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "cron__create"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Schedule a durable prompt that fires into YOUR project's session and survives forge \
         restarts. Provide EXACTLY ONE of `schedule` (a 5-field cron expression like \"0 9 * * *\" \
         for 9am daily, evaluated in your local timezone) or `run_once_at` (an RFC3339 timestamp \
         like \"2026-07-01T09:00:00Z\" for a single fire), plus `prompt` (the text delivered as a \
         user turn when it fires). If the project's session isn't open at fire time, forge spawns \
         it first. Returns the cron's id (use it with cron__delete). Recurring crons repeat; \
         run-once crons delete themselves after firing. Any session in the project may create \
         crons."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "schedule": {
                    "type": "string",
                    "description": "A 5-field cron expression (minute hour day month weekday), \
                                    evaluated in the host's local timezone. Mutually exclusive \
                                    with run_once_at.",
                },
                "run_once_at": {
                    "type": "string",
                    "description": "An RFC3339 timestamp for a single fire. Mutually exclusive \
                                    with schedule.",
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt delivered as a user turn when the cron fires.",
                },
            },
            "required": ["prompt"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: CreateArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let kind = match (args.schedule, args.run_once_at) {
            (Some(expr), None) => CronKind::Recurring(expr),
            (None, Some(rfc)) => match parse_rfc3339(&rfc) {
                Some(t) => CronKind::Once(t),
                None => {
                    return tool_error(format!(
                        "run_once_at must be an RFC3339 timestamp (e.g. 2026-07-01T09:00:00Z); \
                         got {rfc:?}"
                    ));
                }
            },
            (Some(_), Some(_)) => {
                return tool_error(
                    "provide exactly one of `schedule` or `run_once_at`, not both".to_owned(),
                );
            }
            (None, None) => {
                return tool_error(
                    "provide either `schedule` (a 5-field cron expression) or `run_once_at` (an \
                     RFC3339 timestamp)"
                        .to_owned(),
                );
            }
        };
        let caller = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
        match self.facade.create_cron(&caller, kind, args.prompt) {
            Ok(entry) => match serde_json::to_string_pretty(&cron_to_json(&entry)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_create_error(&err)),
        }
    }
}

struct List {
    facade: Arc<dyn CronFacade>,
    caller_key: CallerKeyResolver,
}

#[async_trait::async_trait]
impl Tool for List {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "cron__list"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "List the durable crons registered for YOUR project. Returns a JSON array of {id, \
         project, schedule, prompt, next_fire}. Use an id with cron__delete. An empty array means \
         no crons are scheduled. Takes no arguments. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        let caller = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
        let crons = self.facade.list_crons(&caller);
        let arr: Vec<serde_json::Value> = crons.iter().map(cron_to_json).collect();
        match serde_json::to_string_pretty(&serde_json::Value::Array(arr)) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("cron-list serialization failed: {err}")),
        }
    }
}

struct Delete {
    facade: Arc<dyn CronFacade>,
    caller_key: CallerKeyResolver,
}

#[derive(serde::Deserialize)]
struct DeleteArgs {
    id: String,
}

#[async_trait::async_trait]
impl Tool for Delete {
    #[allow(clippy::unnecessary_literal_bound)]
    fn name(&self) -> &str {
        "cron__delete"
    }

    #[allow(clippy::unnecessary_literal_bound)]
    fn description(&self) -> &str {
        "Delete a durable cron in YOUR project by id (from cron__list / cron__create). Any \
         session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The cron id to delete." },
            },
            "required": ["id"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: DeleteArgs = match serde_json::from_value(input.value) {
            Ok(a) => a,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let caller = match self.caller_key.current() {
            Ok(k) => k,
            Err(err) => return tool_error(err.to_string()),
        };
        match self.facade.delete_cron(&caller, &CronId::from(args.id.as_str())) {
            Ok(true) => ToolOutput::text(format!("deleted cron {}", args.id)),
            Ok(false) => tool_error(format!("no cron with id {} in your project", args.id)),
            Err(CronDeleteError::UnknownCallerProject) => {
                tool_error("couldn't resolve your project".to_owned())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionKey;
    use crate::mcp::cron::facade::MockCronFacade;

    fn resolver() -> CallerKeyResolver {
        CallerKeyResolver::from_fixed(SessionKey::from_session_id("caller"))
    }

    fn sample_entry(id: &str) -> CronEntry {
        CronEntry {
            id: CronId::from(id),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: SystemTime::UNIX_EPOCH,
            team_role: None,
        }
    }

    fn input(value: serde_json::Value) -> ToolInput {
        ToolInput { value }
    }

    #[tokio::test]
    async fn create_with_cron_expr_calls_facade_and_returns_entry() {
        let mock = Arc::new(MockCronFacade::new());
        *mock.create_result.lock() = Some(Ok(sample_entry("c1")));
        let tool = Create { facade: mock.clone(), caller_key: resolver() };

        let out = tool
            .call(input(serde_json::json!({ "schedule": "0 9 * * *", "prompt": "stand-up" })))
            .await;
        assert!(!out.is_error, "valid create succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("c1"), "output carries the id");

        let calls = mock.create_calls.lock();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0].1, CronKind::Recurring(e) if e == "0 9 * * *"));
        assert_eq!(calls[0].2, "stand-up");
    }

    #[tokio::test]
    async fn create_with_run_once_at_parses_rfc3339() {
        let mock = Arc::new(MockCronFacade::new());
        let tool = Create { facade: mock.clone(), caller_key: resolver() };

        let out = tool
            .call(input(
                serde_json::json!({ "run_once_at": "2030-01-01T09:00:00Z", "prompt": "deploy" }),
            ))
            .await;
        assert!(!out.is_error, "valid rfc3339 accepted: {}", out.blocks[0].text);
        assert!(matches!(mock.create_calls.lock()[0].1, CronKind::Once(_)));
    }

    #[tokio::test]
    async fn create_rejects_bad_rfc3339_without_touching_facade() {
        let mock = Arc::new(MockCronFacade::new());
        let tool = Create { facade: mock.clone(), caller_key: resolver() };

        let out = tool
            .call(input(serde_json::json!({ "run_once_at": "not a date", "prompt": "x" })))
            .await;
        assert!(out.is_error);
        assert!(mock.create_calls.lock().is_empty(), "invalid input never reaches the facade");
    }

    #[tokio::test]
    async fn create_rejects_both_and_neither_schedule() {
        let mock = Arc::new(MockCronFacade::new());
        let tool = Create { facade: mock.clone(), caller_key: resolver() };

        let both = tool
            .call(input(serde_json::json!({
                "schedule": "0 9 * * *",
                "run_once_at": "2030-01-01T09:00:00Z",
                "prompt": "x"
            })))
            .await;
        assert!(both.is_error, "both schedule kinds is an error");

        let neither = tool.call(input(serde_json::json!({ "prompt": "x" }))).await;
        assert!(neither.is_error, "no schedule kind is an error");
    }

    #[tokio::test]
    async fn create_surfaces_invalid_expression_error() {
        let mock = Arc::new(MockCronFacade::new());
        *mock.create_result.lock() =
            Some(Err(CronCreateError::InvalidExpression("bad pattern".to_owned())));
        let tool = Create { facade: mock.clone(), caller_key: resolver() };

        let out =
            tool.call(input(serde_json::json!({ "schedule": "nonsense", "prompt": "x" }))).await;
        assert!(out.is_error);
        assert!(out.blocks[0].text.contains("bad pattern"), "facade error surfaced to the LLM");
    }

    #[tokio::test]
    async fn list_returns_project_crons() {
        let mock = Arc::new(MockCronFacade::new());
        *mock.crons.lock() = vec![sample_entry("a"), sample_entry("b")];
        let tool = List { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error);
        assert!(out.blocks[0].text.contains("\"a\"") && out.blocks[0].text.contains("\"b\""));
    }

    #[tokio::test]
    async fn delete_removes_by_id() {
        let mock = Arc::new(MockCronFacade::new());
        *mock.delete_result.lock() = Some(Ok(true));
        let tool = Delete { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "id": "c1" }))).await;
        assert!(!out.is_error);
        assert_eq!(mock.delete_calls.lock()[0].1, CronId::from("c1"));
    }

    #[tokio::test]
    async fn delete_missing_id_is_error() {
        let mock = Arc::new(MockCronFacade::new());
        *mock.delete_result.lock() = Some(Ok(false));
        let tool = Delete { facade: mock.clone(), caller_key: resolver() };

        let out = tool.call(input(serde_json::json!({ "id": "ghost" }))).await;
        assert!(out.is_error, "deleting an unknown id signals an error to the LLM");
    }

    #[test]
    fn tool_names_are_the_cron_family() {
        // Assert the base tool names only. Combined with the `forge`
        // server name (proven in mcp::tests::build_forge_server_*) they
        // render as `mcp__forge__cron__<x>` on the LLM side, which the SDK
        // auto-approve fast-path covers via the `mcp__forge__` prefix
        // (asserted in forge-sdk options.rs).
        let mock = MockCronFacade::new().into_arc();
        let resolver = resolver();
        let create = Create { facade: mock.clone(), caller_key: resolver.clone() };
        let list = List { facade: mock.clone(), caller_key: resolver.clone() };
        let delete = Delete { facade: mock, caller_key: resolver };
        assert_eq!(create.name(), "cron__create");
        assert_eq!(list.name(), "cron__list");
        assert_eq!(delete.name(), "cron__delete");
    }
}
