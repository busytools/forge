//! Cron MCP - durable scheduled prompts (`mcp__forge__cron__*`).
//!
//! A forge cron fires a prompt into the session that owns it, on a
//! schedule, and survives forge restarts (persisted to the machine-local
//! redb store; see [`crate::store::cron`]). Unlike the cloud routines
//! (`create_trigger` /
//! `CronCreate`), which fire into cloud-hosted sessions, these durably
//! target the local forge process.
//!
//! The tools (`cron__create` / `cron__list` / `cron__delete`) are
//! ANY-CALLER and owner-scoped: `create` stamps the caller as the new
//! entry's owner, and `list` / `delete` narrow to the crons the caller
//! owns. Mirrors `workers__list`, not the lead-only `workers__spawn`.
//! Cron-list mutations are direct `Workspace` methods (state writes), not
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

use crate::SessionSlot;
use crate::mcp::cron::facade::{CronCreateError, CronDeleteError, CronFacade};

pub(crate) mod facade;
pub(crate) mod schedule;

/// Attach the three cron-coordination tools to an existing
/// [`McpServerBuilder`]. Called for BOTH lead and worker sessions (crons
/// are any-caller), so `build_forge_server` invokes this unconditionally.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn CronFacade>,
    slot: SessionSlot,
) -> McpServerBuilder {
    let create = Create { facade: facade.clone(), slot: slot.clone() };
    let list = List { facade: facade.clone(), slot: slot.clone() };
    let delete = Delete { facade, slot };
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
    let mut obj = serde_json::json!({
        "id": entry.id.as_str(),
        "project": entry.project_name,
        "schedule": schedule,
        "prompt": entry.prompt,
        "next_fire": fmt_rfc3339(entry.next_fire),
    });
    if let (Some(desc), Some(map)) = (&entry.description, obj.as_object_mut()) {
        map.insert("description".to_owned(), serde_json::Value::String(desc.clone()));
    }
    obj
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
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct CreateArgs {
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    run_once_at: Option<String>,
    prompt: String,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Create {
    fn name(&self) -> &'static str {
        "cron__create"
    }

    fn description(&self) -> &'static str {
        "Schedule a durable prompt that fires into the session that registers it and survives \
         forge restarts. Provide EXACTLY ONE of `schedule` (a 5-field cron expression like \
         \"0 9 * * *\" for 9am daily, evaluated in your local timezone) or `run_once_at` (an \
         RFC3339 timestamp like \"2026-07-01T09:00:00Z\" for a single fire), plus `prompt` (the \
         text delivered as a user turn when it fires). Optionally pass `description` - a short \
         human summary of what the job does and why - which the UI shows as the schedule's \
         headline. If that session isn't open at fire time, forge spawns it first, unless it can \
         no longer be started (a worker whose directory is gone). Returns the cron's id (use it \
         with cron__delete). Recurring crons repeat; run-once crons delete themselves after \
         firing. The cron is registered to you, and cron__list and cron__delete act on the crons \
         you registered. Any session in the project may create crons."
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
                "description": {
                    "type": "string",
                    "description": "Optional short human summary of what this job does and why \
                                    (e.g. \"Morning summary\"). Shown as the schedule's \
                                    headline in the UI; falls back to the prompt's first line \
                                    when omitted.",
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
        let description = args.description.and_then(|d| {
            let trimmed = d.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        });
        match self.facade.create_cron(&self.slot, kind, args.prompt, description) {
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
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for List {
    fn name(&self) -> &'static str {
        "cron__list"
    }

    fn description(&self) -> &'static str {
        "List the durable crons you registered, in your project. Returns a JSON array of {id, \
         project, schedule, prompt, next_fire}. Use an id with cron__delete. An empty array means \
         you have no crons registered, or that your project could not be resolved, or that this \
         run could not read its stored crons. Takes no arguments. Any session in the project may \
         call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    async fn call(&self, _input: ToolInput) -> ToolOutput {
        let crons = self.facade.list_crons(&self.slot);
        let arr: Vec<serde_json::Value> = crons.iter().map(cron_to_json).collect();
        match serde_json::to_string_pretty(&serde_json::Value::Array(arr)) {
            Ok(json) => ToolOutput::text(json),
            Err(err) => tool_error(format!("cron-list serialization failed: {err}")),
        }
    }
}

struct Delete {
    facade: Arc<dyn CronFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct DeleteArgs {
    id: String,
}

#[async_trait::async_trait]
impl Tool for Delete {
    fn name(&self) -> &'static str {
        "cron__delete"
    }

    fn description(&self) -> &'static str {
        "Delete a durable cron you registered, in your project, by id (from cron__list / \
         cron__create). Any session in the project may call this."
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
        match self.facade.delete_cron(&self.slot, &CronId::from(args.id.as_str())) {
            Ok(()) => ToolOutput::text(format!("deleted cron {}", args.id)),
            Err(CronDeleteError::UnknownCallerProject) => {
                tool_error("couldn't resolve your project".to_owned())
            }
            Err(CronDeleteError::NoSuchCron) => {
                tool_error(format!("no cron with id {} in this project", args.id))
            }
            Err(CronDeleteError::NotOwnedByCaller) => tool_error(format!(
                "cron {} exists in this project but was not registered by you",
                args.id
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::cron::facade::MockCronFacade;

    fn caller_slot() -> SessionSlot {
        SessionSlot::from_str_for_test("caller")
    }

    fn sample_entry(id: &str) -> CronEntry {
        CronEntry {
            id: CronId::from(id),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: SystemTime::UNIX_EPOCH,
            description: None,
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
        let tool = Create { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({ "schedule": "0 9 * * *", "prompt": "stand-up" })))
            .await;
        assert!(!out.is_error, "valid create succeeds: {}", out.blocks[0].text);
        assert!(out.blocks[0].text.contains("c1"), "output carries the id");

        let calls = mock.create_calls.lock();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0].1, CronKind::Recurring(e) if e == "0 9 * * *"));
        assert_eq!(calls[0].2, "stand-up");
        assert_eq!(calls[0].3, None, "no description arg reaches the facade as None");
    }

    #[tokio::test]
    async fn create_threads_and_trims_description() {
        let mock = Arc::new(MockCronFacade::new());
        let tool = Create { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({
                "schedule": "0 9 * * *",
                "prompt": "stand-up",
                "description": "  Morning summary  "
            })))
            .await;
        assert!(!out.is_error, "create with description succeeds: {}", out.blocks[0].text);
        assert_eq!(
            mock.create_calls.lock()[0].3.as_deref(),
            Some("Morning summary"),
            "the description is trimmed and threaded to the facade",
        );

        let blank = tool
            .call(input(serde_json::json!({
                "schedule": "0 9 * * *",
                "prompt": "stand-up",
                "description": "   "
            })))
            .await;
        assert!(!blank.is_error);
        assert_eq!(
            mock.create_calls.lock()[1].3,
            None,
            "a whitespace-only description collapses to None",
        );
    }

    #[tokio::test]
    async fn create_with_run_once_at_parses_rfc3339() {
        let mock = Arc::new(MockCronFacade::new());
        let tool = Create { facade: mock.clone(), slot: caller_slot() };

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
        let tool = Create { facade: mock.clone(), slot: caller_slot() };

        let out = tool
            .call(input(serde_json::json!({ "run_once_at": "not a date", "prompt": "x" })))
            .await;
        assert!(out.is_error);
        assert!(mock.create_calls.lock().is_empty(), "invalid input never reaches the facade");
    }

    #[tokio::test]
    async fn create_rejects_both_and_neither_schedule() {
        let mock = Arc::new(MockCronFacade::new());
        let tool = Create { facade: mock.clone(), slot: caller_slot() };

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
        let tool = Create { facade: mock.clone(), slot: caller_slot() };

        let out =
            tool.call(input(serde_json::json!({ "schedule": "nonsense", "prompt": "x" }))).await;
        assert!(out.is_error);
        assert!(out.blocks[0].text.contains("bad pattern"), "facade error surfaced to the LLM");
    }

    #[tokio::test]
    async fn list_returns_the_callers_crons() {
        let mock = Arc::new(MockCronFacade::new());
        *mock.crons.lock() = vec![sample_entry("a"), sample_entry("b")];
        let tool = List { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error);
        assert!(out.blocks[0].text.contains("\"a\"") && out.blocks[0].text.contains("\"b\""));
    }

    #[tokio::test]
    async fn delete_removes_by_id() {
        let mock = Arc::new(MockCronFacade::new());
        *mock.delete_result.lock() = Some(Ok(()));
        let tool = Delete { facade: mock.clone(), slot: caller_slot() };

        let out = tool.call(input(serde_json::json!({ "id": "c1" }))).await;
        assert!(!out.is_error);
        assert_eq!(mock.delete_calls.lock()[0].1, CronId::from("c1"));
    }

    /// A refusal has to say which case it is: an id that exists in the
    /// project but is not the caller's own reads as *not yours*, an id
    /// the project never had reads as *not there*. One string for both
    /// lets a caller take its own refused delete as proof the cron is
    /// gone.
    #[tokio::test]
    async fn delete_refusals_name_the_case() {
        let mock = Arc::new(MockCronFacade::new());
        let tool = Delete { facade: mock.clone(), slot: caller_slot() };

        *mock.delete_result.lock() = Some(Err(CronDeleteError::NotOwnedByCaller));
        let not_yours = tool.call(input(serde_json::json!({ "id": "c1" }))).await;
        assert!(not_yours.is_error, "a refused delete is an error");
        assert_eq!(
            not_yours.blocks[0].text,
            "cron c1 exists in this project but was not registered by you",
            "a cron that exists but is not the caller's own must say so, without naming a session",
        );

        *mock.delete_result.lock() = Some(Err(CronDeleteError::NoSuchCron));
        let not_there = tool.call(input(serde_json::json!({ "id": "c1" }))).await;
        assert!(not_there.is_error, "an unknown id is an error");
        assert_eq!(
            not_there.blocks[0].text, "no cron with id c1 in this project",
            "an id the project never had must not read as another session's cron",
        );
    }

    /// Both tools narrow to the crons the caller owns, so a description
    /// claiming project scope sends a caller looking for crons it could
    /// never see - and one naming another session's crons sends it
    /// looking for a set it is not meant to query at all.
    #[test]
    fn tool_descriptions_scope_to_the_caller_not_the_project() {
        // Wordings that disclose crons beyond the caller's own. A net, not
        // a closure: whether a wording discloses is a judgement over
        // open-ended prose, so no list of these is complete, and this is
        // the only guard against a sentence appended beside the scope
        // clause - the scope pin covers a reword of that clause, not an
        // addition next to it. Bigrams rather than the bare word `session`,
        // so "Any session in the project may call this" survives.
        const DISCLOSING: &[&str] = &[
            "another session",
            "other session",
            "other worker",
            "other owner",
            "each session",
            "sibling",
            "separate sets",
            "another agent",
        ];

        let mock = MockCronFacade::new().into_arc();
        let slot = caller_slot();
        let create = Create { facade: mock.clone(), slot: slot.clone() };
        let list = List { facade: mock.clone(), slot: slot.clone() };
        let delete = Delete { facade: mock, slot };
        let list_desc = list.description();

        for (name, desc) in [
            ("cron__create", create.description()),
            ("cron__list", list_desc),
            ("cron__delete", delete.description()),
        ] {
            assert!(
                desc.contains("you registered"),
                "{name} must name the caller's own crons rather than the project: {desc}",
            );
            let lowered = desc.to_lowercase();
            for shape in DISCLOSING {
                assert!(
                    !lowered.contains(shape),
                    "{name} must not disclose that other sessions' crons exist ({shape}): {desc}",
                );
            }
        }
        assert!(
            list_desc.contains("An empty array means you have no crons registered"),
            "an empty list must read as the caller having none, never as the project having \
             none: {list_desc}",
        );
        assert!(
            list_desc.contains("or that your project could not be resolved"),
            "an empty list must not rule out the caller that could not be resolved, which also \
             lists nothing: {list_desc}",
        );
        assert!(
            list_desc.contains("or that this run could not read its stored crons"),
            "an empty list must own up to a run that could not read the store, which starts with \
             none: {list_desc}",
        );
    }

    /// A worker's directory can be gone with its row kept, and then
    /// nothing starts the owner and the fire does not land - so the
    /// spawn promise cannot be unconditional.
    #[test]
    fn cron_create_description_qualifies_the_spawn_promise() {
        let create = Create { facade: MockCronFacade::new().into_arc(), slot: caller_slot() };
        let desc = create.description();
        assert!(
            desc.contains("unless it can no longer be started"),
            "the spawn promise must carry the case where the owner cannot be started: {desc}",
        );
    }

    /// Any-caller is an affordance a caller-scoped description cannot
    /// carry, and a worker cannot infer it: it is who may CALL, not what
    /// another session's crons hold.
    #[test]
    fn tool_descriptions_keep_the_any_caller_sentence() {
        let mock = MockCronFacade::new().into_arc();
        let slot = caller_slot();
        let create = Create { facade: mock.clone(), slot: slot.clone() };
        let list = List { facade: mock.clone(), slot: slot.clone() };
        let delete = Delete { facade: mock, slot };
        for (name, desc) in [
            ("cron__create", create.description()),
            ("cron__list", list.description()),
            ("cron__delete", delete.description()),
        ] {
            assert!(
                desc.contains("Any session in the project may"),
                "{name} must keep saying who may call it: {desc}",
            );
        }
    }

    /// The fired prompt lands in the session that registered the cron,
    /// which for a worker is its own - so a description naming the
    /// project's session points that worker at a session it is not
    /// talking to.
    #[test]
    fn cron_create_description_names_the_registering_session_as_the_fire_target() {
        let create = Create { facade: MockCronFacade::new().into_arc(), slot: caller_slot() };
        let desc = create.description();
        assert!(
            desc.contains("fires into the session that registers it"),
            "the create description must say where the prompt lands: {desc}",
        );
        assert!(
            !desc.contains("project's session"),
            "the prompt does not land in the project's session: {desc}",
        );
    }

    #[test]
    fn tool_names_are_the_cron_family() {
        // Assert the base tool names only. Combined with the `forge`
        // server name (proven in mcp::tests::build_forge_server_*) they
        // render as `mcp__forge__cron__<x>` on the LLM side, which the SDK
        // auto-approve fast-path covers via the `mcp__forge__` prefix
        // (asserted in forge-sdk options.rs).
        let mock = MockCronFacade::new().into_arc();
        let slot = caller_slot();
        let create = Create { facade: mock.clone(), slot: slot.clone() };
        let list = List { facade: mock.clone(), slot: slot.clone() };
        let delete = Delete { facade: mock, slot };
        assert_eq!(create.name(), "cron__create");
        assert_eq!(list.name(), "cron__list");
        assert_eq!(delete.name(), "cron__delete");
    }
}
