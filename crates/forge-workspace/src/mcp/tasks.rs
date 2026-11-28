//! Task MCP - the project's live task list (`mcp__forge__tasks__*`).
//!
//! forge owns the task list its sessions work from: a task is a declared
//! piece of work in flight, stored in the machine-local redb store (see
//! [`crate::store::tasks`]) and rendered by the Inspector's TASKS
//! section. The CLI's own task tools are gated per model and do not
//! survive a session, so forge does not use them.
//!
//! The tools (`tasks__create` / `tasks__update` / `tasks__list` /
//! `tasks__delete` / `tasks__claim` / `tasks__wait`) are ANY-CALLER,
//! scoped to the caller's own project - a worker creating its own
//! subtasks is the normal case, and any session may move any task in its
//! project; `tasks__approve` is the lead's own door and is not offered to
//! a worker. Task-list mutations are direct `Workspace` methods (state
//! writes), not Command-bus dispatches.
//!
//! - [`facade`] - the `TasksFacade` seam (prod over `Weak<Workspace>` +
//!   a mock for tool tests).

use std::sync::Arc;
use std::time::SystemTime;

use forge_sdk::mcp::server::McpServerBuilder;
use forge_sdk::mcp::tool::{Tool, ToolInput, ToolOutput, ToolOutputBlock};

use forge_primitives::tasks::{LinkKind, Task, TaskId, TaskStatus, WaitingKind};

use crate::SessionSlot;
use crate::mcp::tasks::facade::{AskDraft, TaskDraft, TaskPatch, TasksError, TasksFacade};

pub(crate) mod facade;

/// Attach the task tools to an existing [`McpServerBuilder`]. Called for
/// BOTH lead and worker sessions (tasks are any-caller), so
/// `build_forge_server` invokes this unconditionally.
pub(crate) fn add_tools(
    builder: McpServerBuilder,
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
    lead: bool,
) -> McpServerBuilder {
    let create = Create { facade: facade.clone(), slot: slot.clone() };
    let update = Update { facade: facade.clone(), slot: slot.clone() };
    let list = List { facade: facade.clone(), slot: slot.clone() };
    let get = Get { facade: facade.clone(), slot: slot.clone() };
    let delete = Delete { facade: facade.clone(), slot: slot.clone() };
    let claim = Claim { facade: facade.clone(), slot: slot.clone() };
    let wait = Wait { facade: facade.clone(), slot: slot.clone() };
    let builder =
        builder.tool(create).tool(update).tool(list).tool(get).tool(delete).tool(claim).tool(wait);
    // `tasks__approve` and `tasks__answer` are the lead's doors: only the
    // lead stands behind the user's recorded look or decision, so a worker
    // is not offered either verb.
    if lead {
        builder
            .tool(Approve { facade: facade.clone(), slot: slot.clone() })
            .tool(Answer { facade: facade.clone(), slot: slot.clone() })
            .tool(Policy { facade, slot })
    } else {
        builder
    }
}

fn tool_error(text: String) -> ToolOutput {
    ToolOutput::error(text)
}

/// Format a `SystemTime` as a UTC RFC3339 string for tool output.
fn fmt_rfc3339(t: SystemTime) -> String {
    time::OffsetDateTime::from(t)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// The status as the schema spells it.
fn status_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Canceled => "canceled",
    }
}

/// The artifact a caller read before links existed: the first
/// pull-request or path link, by its label when it has one.
fn artifact(task: &Task) -> Option<&str> {
    task.links
        .iter()
        .find(|l| matches!(l.kind, LinkKind::Pr | LinkKind::Path))
        .map(|l| l.label.as_deref().unwrap_or(l.target.as_str()))
}

/// Readable JSON for one task (the tool-output shape the LLM sees).
/// Absent optional fields are omitted rather than sent as null.
fn task_to_json(task: &Task) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_owned(), serde_json::json!(task.id.as_str()));
    map.insert("project".to_owned(), serde_json::json!(task.project_name));
    map.insert("subject".to_owned(), serde_json::json!(task.subject));
    map.insert("status".to_owned(), serde_json::json!(status_str(task.status)));
    map.insert("kind".to_owned(), serde_json::json!(kind_str(task.kind)));
    // An unclaimed task omits `owner` rather than writing a sentinel: a
    // caller that reads the field back and filters on it would otherwise
    // find none, and a session actually labelled `unclaimed` would read the
    // same as nobody.
    for (key, value) in [
        ("active_form", task.active_form.as_deref()),
        ("detail", task.detail.as_deref()),
        ("artifact", artifact(task)),
        ("estimate", task.estimate.as_ref().map(|e| e.words.as_str())),
        ("cadence", task.cadence.as_ref().map(|c| c.words.as_str())),
        ("parent", task.parent.as_ref().map(TaskId::as_str)),
        ("owner", task.owner.as_ref().map(SessionSlot::label)),
    ] {
        if let Some(value) = value {
            map.insert(key.to_owned(), serde_json::json!(value));
        }
    }
    for (key, value) in
        [("verify", task.verify.map(verify_str)), ("reserved", task.reserved.map(|_| "user"))]
    {
        if let Some(value) = value {
            map.insert(key.to_owned(), serde_json::json!(value));
        }
    }
    if let Some(attested) = &task.attested {
        map.insert(
            "attested".to_owned(),
            serde_json::json!({
                "evidence": attested.evidence,
                "at": fmt_rfc3339(attested.at),
            }),
        );
    }
    if let Some(request) = &task.request {
        map.insert("request".to_owned(), request_json(request));
    }
    if let Some(evidence) = &task.evidence {
        map.insert(
            "evidence".to_owned(),
            serde_json::json!({
                "commit": evidence.commit,
                "path": evidence.path,
                // A block stored before the write seams dropped blank
                // words reads the way a call states it: no quote at all,
                // through the one seam that defines a blank quote.
                "lead_verified": forge_primitives::tasks::lead_verification(
                    evidence.lead_verified.as_deref()
                ),
                "excerpt": evidence.excerpt,
                // The wire's `verified at`: absent until the gate's check
                // passed, so an unchecked claim reads as unknown.
                "verified_at": evidence.verified_at.map(fmt_rfc3339),
                "problem": evidence.problem,
            }),
        );
    }
    // `tasks__get` promises the row's links and its archive state: an
    // archived row that omitted `archived_at` read as a live terminal row,
    // and the evidence a completion rests on could not be followed.
    if !task.links.is_empty() {
        map.insert(
            "links".to_owned(),
            serde_json::json!(task.links.iter().map(link_json).collect::<Vec<_>>()),
        );
    }
    if let Some(archived_at) = task.archived_at {
        map.insert("archived_at".to_owned(), serde_json::json!(fmt_rfc3339(archived_at)));
    }
    map.insert("version".to_owned(), serde_json::json!(task.version));
    map.insert("created_at".to_owned(), serde_json::json!(fmt_rfc3339(task.created_at)));
    map.insert("updated_at".to_owned(), serde_json::json!(fmt_rfc3339(task.updated_at)));
    serde_json::Value::Object(map)
}

/// The kind as the tool schema spells it.
fn kind_str(kind: forge_primitives::tasks::TaskKind) -> &'static str {
    match kind {
        forge_primitives::tasks::TaskKind::Finite => "finite",
        forge_primitives::tasks::TaskKind::Standing => "standing",
    }
}

/// The link kind as the tool schema spells it.
fn link_kind_str(kind: LinkKind) -> &'static str {
    match kind {
        LinkKind::Spec => "spec",
        LinkKind::Plan => "plan",
        LinkKind::Issue => "issue",
        LinkKind::Pr => "pr",
        LinkKind::Branch => "branch",
        LinkKind::Path => "path",
        LinkKind::Other => "other",
    }
}

/// One link as the tool output reads it: where it points, what the
/// connector last saw, and when it was attached.
fn link_json(link: &forge_primitives::tasks::TaskLink) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("kind".to_owned(), serde_json::json!(link_kind_str(link.kind)));
    map.insert("target".to_owned(), serde_json::json!(link.target));
    for (key, value) in [("label", link.label.as_deref()), ("state", link.state.as_deref())] {
        if let Some(value) = value {
            map.insert(key.to_owned(), serde_json::json!(value));
        }
    }
    if let Some(check) = &link.last_check {
        map.insert(
            "last_check".to_owned(),
            serde_json::json!({
                "at": fmt_rfc3339(check.at),
                "changed_at": check.changed_at.map(fmt_rfc3339),
                "state": check.state,
                "head": check.head,
                "ci": check.ci,
                "problem": check.problem,
            }),
        );
    }
    map.insert("added_at".to_owned(), serde_json::json!(fmt_rfc3339(link.added_at)));
    serde_json::Value::Object(map)
}

/// The ask record as the tool output reads it: its kind, the question, its
/// options or class, and the value the answer returned.
fn request_json(request: &forge_primitives::tasks::Request) -> serde_json::Value {
    let ask_kind_str = |kind: forge_primitives::tasks::AskKind| match kind {
        forge_primitives::tasks::AskKind::Decision => "decision",
        forge_primitives::tasks::AskKind::Clarification => "clarification",
        forge_primitives::tasks::AskKind::Exception => "exception",
        forge_primitives::tasks::AskKind::Teaching => "teaching",
    };
    let mut map = serde_json::Map::new();
    map.insert("kind".to_owned(), serde_json::json!(ask_kind_str(request.kind)));
    map.insert("question".to_owned(), serde_json::json!(request.question));
    map.insert("options".to_owned(), serde_json::json!(request.options));
    if let Some(class) = &request.class {
        map.insert("class".to_owned(), serde_json::json!(class));
    }
    if let Some(answer) = &request.answer {
        map.insert(
            "answer".to_owned(),
            serde_json::json!({
                "value": match &answer.value {
                    forge_primitives::tasks::AnswerValue::Choice(choice) => {
                        serde_json::json!({ "choice": choice })
                    }
                    forge_primitives::tasks::AnswerValue::Text(text) => {
                        serde_json::json!({ "text": text })
                    }
                    forge_primitives::tasks::AnswerValue::Disposition(disposition) => {
                        serde_json::json!({ "disposition": disposition.as_str() })
                    }
                },
                "note": answer.note,
                "at": fmt_rfc3339(answer.at),
            }),
        );
    }
    serde_json::Value::Object(map)
}

/// The gate as the tool schema spells it.
fn verify_str(verify: forge_primitives::tasks::Verify) -> &'static str {
    match verify {
        forge_primitives::tasks::Verify::User => "user",
        forge_primitives::tasks::Verify::None => "none",
    }
}

fn format_tasks_error(err: &TasksError) -> String {
    match err {
        TasksError::UnknownCallerProject => {
            "couldn't resolve your project; is this session attached to a forge.toml project?"
                .to_owned()
        }
        TasksError::BadEstimate(words) => {
            format!("\"{words}\" is not a duration; use 30m, 2h, 1d or 1w")
        }
        TasksError::Refused(why) => {
            format!("the move was refused: {why}")
        }
        TasksError::NoSuchTask { id } => {
            format!("no task with id {id} in your project")
        }
        TasksError::TaskInAnotherProject { id, project } => {
            format!("task {id} exists in project {project}, not in your project")
        }
    }
}

struct Create {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[async_trait::async_trait]
impl Tool for Create {
    fn name(&self) -> &'static str {
        "tasks__create"
    }

    fn description(&self) -> &'static str {
        "Declare a piece of live work in YOUR project's task list. `subject` is the row's text \
         and the only required field; `active_form` is the in-progress wording (\"Adding tests\" \
         for \"Add tests\"), shown while the task is running. `owner` is the label of the session \
         holding it - a worker's label, or \"lead\" - and omitting it leaves the task unclaimed. \
         A lead creating a row for a worker assigns it at creation: the worker's label in \
         `owner`, and the kick says not to claim it. A row left in the lead's own hands \
         refuses the worker's `tasks__claim` while the lead is live, and a row left in \
         progress under a seat that goes is chased as having no live owner once the board \
         marks it stale. \
         `parent` names another task in the same project this one belongs under, `estimate` a \
         duration, `rank` the queue order, `verify` whether completion waits on the user, \
         `links` the references it carries (issues, PRs, specs, paths), `detail` free prose. \
         `kind` is `finite` (the default) for work with a finish line, or `standing` for work \
         that never finishes - the watcher, a growth loop, a weekly sweep - which carries a \
         `cadence` (a duration like the estimate) instead of an estimate and is checked when \
         that cadence comes due rather than chased for overrun. \
         `status` defaults to pending. A row whose deliverable IS the user's own read or \
         decision - a report the user asked to read, a verdict the user asked to give - is \
         minted `verify: \"user\"` explicitly: it ends parked on the user, and the board's \
         approve is what completes it. Only the lead mints this gate; a worker hands the ask \
         to its lead instead. The gate is minted on the row that owes the look; an \
         epic's `verify: \"user\"` governs the epic's own completion, and a child never \
         inherits it - a child that owes no look completes on its own terms. `reserved: \
         \"user\"` reserves the pick for the user: the drive leaves it, it never reads \
         ready, and no seat's claim takes it. Returns the stored task \
         with its id (use it with tasks__update / tasks__delete). Any session in the project \
         may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "description": "The task's row text.",
                },
                "active_form": {
                    "type": "string",
                    "description": "The in-progress wording, shown while the task is running.",
                },
                "detail": {
                    "type": "string",
                    "description": "Free prose: the why.",
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "waiting", "completed", "failed", "canceled"],
                    "description": "Where the task starts. Defaults to pending, the board's \
                                    Backlog: work kept to look at later, not by itself an \
                                    assignment. The drive nudges the lead about unowned ready \
                                    work, so take ownership to set one aside.",
                },
                "owner": {
                    "type": "string",
                    "description": "The label of the session holding the task, in your project \
                                    (\"lead\" for the lead). Omit for unclaimed.",
                },
                "parent": {
                    "type": "string",
                    "description": "The id of the task this one belongs under, in the same \
                                    project; a full id or any unique prefix of one.",
                },
                "estimate": {
                    "type": "string",
                    "description": "A duration estimate (e.g. \"1d\").",
                },
                "kind": {
                    "type": "string",
                    "enum": ["finite", "standing"],
                    "description": "Finite work with a finish line (the default), or standing \
                                    work that never finishes and is checked on a cadence.",
                },
                "cadence": {
                    "type": "string",
                    "description": "A standing row's check interval as a duration (e.g. \
                                    \"1d\"): when it comes due the row is asked whether it is \
                                    still doing its job.",
                },
                "rank": {
                    "type": "integer",
                    "description": "Queue order; lower reads first.",
                },
                "verify": {
                    "type": "string",
                    "enum": ["user", "none"],
                    "description": "Whether the row's own completion waits on the user's look. \
                                    The gate is minted on the row; a child never inherits \
                                    its epic's. A row whose deliverable IS the user's own \
                                    read or decision states `user`; anything else owes no \
                                    look, even under a `user` epic. The gate is the lead's \
                                    to mint; a worker asks its lead instead.",
                },
                "reserved": {
                    "type": "string",
                    "enum": ["user"],
                    "description": "Set `user` to reserve this pick for the user: the drive \
                                    leaves a reserved row for them, and no claim takes it.",
                },
                "links": {
                    "type": "array",
                    "description": "References this row carries: a spec, a plan, an issue, a PR, \
                                    a branch, a path. The kind is derived from the target when \
                                    omitted.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["spec", "plan", "issue", "pr", "branch", "path", "other"],
                            },
                            "label": { "type": "string" },
                            "target": { "type": "string" },
                        },
                        "required": ["target"],
                    },
                },
                "evidence": {
                    "type": "object",
                    "description": "What the completion will rest on. `commit` is a sha the \
                                    repository must hold; `path` a file that must exist at that \
                                    commit (or at HEAD when no commit is named); \
                                    `lead_verified` is the project lead's own verification, in \
                                    words, for an artifact the gate cannot check mechanically \
                                    (a sibling repo, a non-git artifact) - only the lead seat \
                                    may state it. `excerpt` is a bounded output sample for the \
                                    reader - it is never the claim itself. A row cannot \
                                    complete without one of the three claim forms.",
                    "properties": {
                        "commit": { "type": "string" },
                        "path": { "type": "string" },
                        "lead_verified": { "type": "string" },
                        "excerpt": { "type": "string" },
                    },
                    "additionalProperties": false,
                },
            },
            "required": ["subject"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let draft: TaskDraft = match serde_json::from_value(input.value) {
            Ok(draft) => draft,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.create_task(&self.slot, draft) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Update {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct UpdateArgs {
    id: String,
    #[serde(flatten)]
    patch: TaskPatch,
}

#[async_trait::async_trait]
impl Tool for Update {
    fn name(&self) -> &'static str {
        "tasks__update"
    }

    fn description(&self) -> &'static str {
        "Change a task in YOUR project by id, stating only the fields to move - everything else \
         is left alone. Pass `version` with the version you read and a write that would land \
         on a row another writer has since changed is refused with the current version, so a \
         lost update is visible. `status` is the usual one (pending / in_progress / waiting / \
         completed / failed / canceled), and `owner` takes a session label (\"lead\" for the \
         lead) or the label of a worker. Pending is the board's Backlog: work kept to look at later, not by \
         itself an assignment and not a promise to start. A pending row that reads ready and \
         unowned is dispatchable; take ownership to set it aside. \
         The board is for running work: a landed row with no children and \
         no epic above it is DELETED in the same pass (tasks__delete), never left completed; \
         a landed row under an epic - open or already terminal - is completed and left \
         terminal, because the epic reads `to close` only once every child is terminal (a \
         verify=user child parks in waiting on the user and counts only once the user \
         approves); and completing the epic's ROOT row closes it and archives the whole \
         tree. A child of an epic completes when its deliverable is out - for a change, \
         delivered means the PR exists and the delivering worker's own checks ran at its \
         head; for a report or a sweep, concluded means it is handed over. **A move to \
         `completed` needs `evidence`**: a `commit` the repository holds - the commit alone is \
         a claim forge checks - or a `path` at that commit (at HEAD when no commit is named), \
         with an `excerpt` for the reader; or `lead_verified`, the project lead's own \
         verification in words for an artifact the gate cannot check mechanically (a sibling \
         repo, a non-git artifact), which only the lead seat may state. forge checks the \
         claim on the move: a completion with no claim at all is refused, and a commit or \
         path the repository cannot back is refused rather than landed - a lead's \
         `lead_verified` stands for an artifact the check cannot reach, so that check does \
         not run for it. Being committed is not the claim; what this move states is. \
         Any session in the \
         project may move any of its tasks, so a lead can \
         complete a worker's task and a worker can put a row it holds into waiting - except \
         that a row whose claim is the project lead's verification (`lead_verified`) completes \
         only from the lead's seat, or by the user's own approve, and only the lead may \
         restate that evidence block; a worker asks its lead to complete the row, or to \
         restate the claim with the artifact. A row \
         waiting on the user's own look cannot be completed here: the board's approve completes \
         it, and a cancel of a row waiting on the user is the withdraw door once the ask has \
         gone moot: it needs `detail`, the reason the ask is withdrawn, which lands on the row, \
         and a cancel stating none - or only what the row already says - is refused. Any \
         other move off `waiting` drops the wait the row held - except a completion on a row \
         whose completion waits on the user: while it is waiting on something else the gate \
         would trade that wait for a look park, so the completion is refused by name; move it \
         off waiting first, then complete it. When the row you end is named by another row's \
         dependency wait, move every waiter off that wait in the same pass - a wait left on an \
         ended row reads that its target has ended indefinitely. `verify: \"user\"` \
         may be set here by the lead, but not taken away: a row carrying `user` keeps that \
         gate until the user's approve, so clearing it is refused rather than taken as a way \
         around the look. Only the lead mints this gate; a worker hands the ask to its lead \
         instead. \
         The gate is minted on the row, never inherited from its epic, so reparenting changes \
         no gate. A task is \
         named by its full id or any unique prefix of one; an exact id wins over any longer id \
         that starts with it, and a prefix matching more than one row is refused with the \
         matching rows named. An unknown id is an error, not a no-op. Returns the task as the \
         write left it."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The task id to change, or any unique prefix of one.",
                },
                "version": {
                    "type": "integer",
                    "description": "The row's version as you read it. Required: a version the \
                                    row is no longer at refuses the write and names the current \
                                    one, so a lost update is visible instead of overwriting \
                                    another writer.",
                },
                "subject": { "type": "string", "description": "New row text." },
                "active_form": {
                    "type": "string",
                    "description": "New in-progress wording.",
                },
                "detail": { "type": "string", "description": "New detail prose." },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "waiting", "completed", "failed", "canceled"],
                    "description": "The task's new status. `completed` is not a resting state \
                                    for a landed row with no children and no epic above it: \
                                    it is deleted in the same pass. A landed row under an \
                                    epic - open or already terminal - stays completed and \
                                    terminal so the epic can read `to close`; a verify=user \
                                    row parks in waiting on the user first. A child of an \
                                    epic completes when its deliverable is out - for a \
                                    change, delivered means the PR exists and the \
                                    delivering worker's own checks ran at its head; for a \
                                    report or a sweep, concluded means it is handed over. \
                                    Being committed is not the claim; the `evidence` this move \
                                    states is.",
                },
                "owner": {
                    "type": "string",
                    "description": "The label of the session now holding the task.",
                },
                "parent": {
                    "type": "string",
                    "description": "The id of the task this one now belongs under, or any \
                                    unique prefix of one.",
                },
                "rank": { "type": "integer", "description": "New queue order; lower reads first." },
                "verify": {
                    "type": "string",
                    "enum": ["user", "none"],
                    "description": "Whether the row's own completion waits on the user's look. \
                                    Can be set here by the lead; a worker asks its lead \
                                    instead. A `user` gate is the user's to clear: an update \
                                    cannot clear it. The gate is minted on the row, so moving \
                                    the row changes nothing.",
                },
                "parked": {
                    "type": "boolean",
                    "description": "Mark a waiting row's wait as parked by design - the user's \
                                    own deferral - so the chase leaves it alone. Only a \
                                    waiting row takes it.",
                },
                "links_add": {
                    "type": "array",
                    "description": "Links to attach; a target already on the row is left alone.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["spec", "plan", "issue", "pr", "branch", "path", "other"],
                            },
                            "label": { "type": "string" },
                            "target": { "type": "string" },
                        },
                        "required": ["target"],
                    },
                },
                "links_remove": {
                    "type": "array",
                    "description": "Links to take off, by exact target.",
                    "items": { "type": "string" },
                },
                "evidence": {
                    "type": "object",
                    "description": "The completion's claim: `commit` (a sha the repository must \
                                    hold), `path` (a file that must exist at that commit, or at \
                                    HEAD when no commit is named), `lead_verified` (the project \
                                    lead's own verification, in words, for an artifact the gate \
                                    cannot check mechanically - a sibling repo, a non-git \
                                    artifact; only the lead seat may state it, and the \
                                    mechanical check is skipped when it is present), and \
                                    `excerpt` (a bounded sample of the output, for the reader). \
                                    A move to completed is refused while the claim is missing \
                                    or does not check out; restating the claim clears its last \
                                    verdict.",
                    "properties": {
                        "commit": { "type": "string" },
                        "path": { "type": "string" },
                        "lead_verified": { "type": "string" },
                        "excerpt": { "type": "string" },
                    },
                    "additionalProperties": false,
                },
                "due": {
                    "type": "string",
                    "description": "A deadline for the row's current wait (\"30m\", \"2h\", \
                                    \"1d\"). It arrives with `default` or not at all; a look \
                                    takes one as any waiting row does.",
                },
                "default": {
                    "type": "string",
                    "enum": ["escalate", "expire", "reask"],
                    "description": "The NAMED default that runs when `due` passes: escalate \
                                    tells the lead the ask went unanswered; expire ends the \
                                    row canceled with the reason and keeps the record; reask \
                                    re-notifies the lead and re-arms the deadline. There is \
                                    no approve default.",
                },
                "estimate": { "type": "string", "description": "A duration estimate." },
            },
            "required": ["id", "version"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: UpdateArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        // Every write names the version it read: an update that skipped
        // the check could silently overwrite another writer's move.
        if args.patch.version.is_none() {
            return tool_error(
                "a write names the version it read: pass `version`, the row's version as \
                 tasks__list or tasks__get returned it"
                    .to_owned(),
            );
        }
        match self.facade.update_task(&self.slot, &TaskId::from(args.id.as_str()), args.patch) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct List {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

/// How many rows a list answer carries when the caller states no limit.
const DEFAULT_LIST_LIMIT: usize = 20;

/// The most a caller may ask a list answer to carry. A stated limit above
/// this is clamped rather than honoured: one poll must not put a project's
/// whole history back through the model's context.
const MAX_LIST_LIMIT: usize = 100;

#[derive(serde::Deserialize)]
struct ListArgs {
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    status: Option<TaskStatus>,
    #[serde(default)]
    kind: Option<forge_primitives::tasks::TaskKind>,
    #[serde(default)]
    ready: Option<bool>,
    #[serde(default)]
    detail: Option<bool>,
    #[serde(default)]
    limit: Option<usize>,
}

fn waiting_kind_str(kind: WaitingKind) -> &'static str {
    match kind {
        WaitingKind::User => "user",
        WaitingKind::Dependency => "dependency",
        WaitingKind::Resource => "resource",
    }
}

/// The statuses a default read leaves out: a completed or canceled row
/// is history, not work in play.
fn is_finished(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Completed | TaskStatus::Canceled)
}

/// The closing line a default read writes when it withheld finished
/// rows: how many, by status, and the door that reads them. Nothing is
/// dropped silently.
fn withheld_line(withheld: &[crate::board::BoardRow]) -> Option<String> {
    let count =
        |status: TaskStatus| withheld.iter().filter(|row| row.task.status == status).count();
    let mut tally = Vec::new();
    let mut doors = Vec::new();
    for (status, spelled) in
        [(TaskStatus::Completed, "completed"), (TaskStatus::Canceled, "canceled")]
    {
        let rows = count(status);
        if rows > 0 {
            tally.push(format!("{rows} {spelled}"));
            doors.push(format!("status: \"{spelled}\""));
        }
    }
    let total: usize = withheld.len();
    if total == 0 {
        return None;
    }
    let noun = if total == 1 { "row" } else { "rows" };
    let pronoun = if total == 1 { "it" } else { "them" };
    Some(format!(
        "{total} finished {noun} withheld by status: {}. Pass {} to read {pronoun}. A finished \
         row archives a day after it ended and leaves this read entirely; `tasks__get` by its id \
         returns it whole, archived rows included.",
        tally.join(", "),
        doors.join(" or "),
    ))
}

/// The board's marks in the order the description names them.
fn mark_names(marks: crate::board::Marks) -> Vec<&'static str> {
    let mut names = Vec::new();
    for (on, name) in [
        (marks.ready, "ready"),
        (marks.in_review, "in_review"),
        (marks.overdue, "overdue"),
        (marks.stale, "stale"),
        (marks.to_close, "to_close"),
    ] {
        if on {
            names.push(name);
        }
    }
    names
}

/// One board row as the tool output reads it: the record, what the board
/// derived, and the wait it is sitting on.
fn board_row_to_json(row: &crate::board::BoardRow) -> serde_json::Value {
    let mut map = match task_to_json(&row.task) {
        serde_json::Value::Object(map) => map,
        other => return other,
    };
    map.insert("worked".to_owned(), serde_json::json!(crate::board::fmt_secs(row.worked_secs)));
    map.insert(
        "updated_ago".to_owned(),
        serde_json::json!(crate::board::fmt_secs(row.updated_secs_ago)),
    );
    map.insert("marks".to_owned(), serde_json::json!(mark_names(row.marks)));
    if let Some(wait) = &row.task.waiting_on {
        map.insert(
            "waiting".to_owned(),
            serde_json::json!({
                "kind": wait.kind.map(waiting_kind_str),
                "detail": wait.detail,
                "on": wait.on.as_ref().map(TaskId::as_str),
                "verification": wait.verification,
                "parked": wait.parked,
            }),
        );
    }
    if let Some((done, total)) = row.rollup {
        map.insert("rollup".to_owned(), serde_json::json!(format!("{done}/{total}")));
    }
    if let Some(parent_subject) = &row.parent_subject {
        map.insert("parent_subject".to_owned(), serde_json::json!(parent_subject));
    }
    serde_json::Value::Object(map)
}

/// One board row at the size a first orientation reads it: identity, the
/// state it is in, what it is waiting on, what the board derives, its
/// estimate or cadence, and whether its evidence has been checked.
///
/// Everything else - the brief, the links, the hierarchy, the worked time,
/// the history - is the detail verb's, so a list answer stays deliverable
/// however busy the project is.
fn board_row_compact(row: &crate::board::BoardRow) -> serde_json::Value {
    let task = &row.task;
    let mut map = serde_json::Map::new();
    map.insert("id".to_owned(), serde_json::json!(task.id.as_str()));
    map.insert("subject".to_owned(), serde_json::json!(task.subject));
    map.insert("status".to_owned(), serde_json::json!(status_str(task.status)));
    map.insert("kind".to_owned(), serde_json::json!(kind_str(task.kind)));
    for (key, value) in [
        ("estimate", task.estimate.as_ref().map(|e| e.words.as_str())),
        ("cadence", task.cadence.as_ref().map(|c| c.words.as_str())),
        ("owner", task.owner.as_ref().map(SessionSlot::label)),
    ] {
        if let Some(value) = value {
            map.insert(key.to_owned(), serde_json::json!(value));
        }
    }
    map.insert("marks".to_owned(), serde_json::json!(mark_names(row.marks)));
    if let Some(wait) = &task.waiting_on {
        map.insert(
            "waiting".to_owned(),
            serde_json::json!({
                "kind": wait.kind.map(waiting_kind_str),
                "on": wait.on.as_ref().map(TaskId::as_str),
                "verification": wait.verification,
                "parked": wait.parked,
            }),
        );
    }
    if let Some(evidence) = &task.evidence {
        map.insert("evidence".to_owned(), serde_json::json!(evidence_state(evidence)));
    }
    serde_json::Value::Object(map)
}

/// The board's compact state word: `verified`, `failed` or `unchecked`.
/// `verified` covers both what the stamp covers - a passed check of the
/// artifact, and the project lead's word for one the check cannot reach -
/// because a lead-vouched completion is stamped like any other. `failed`
/// is unreachable unless the store is hand-edited: a refusal refuses the
/// move and never lands on the row, so a live row reads `verified` or
/// `unchecked`. Which form verified it is on the block, and the block
/// reaches the detail read and the socket's board rows - the compact list
/// and its `tasks__list` answer carry this word alone.
fn evidence_state(evidence: &forge_primitives::tasks::Evidence) -> &'static str {
    match (&evidence.verified_at, &evidence.problem) {
        (Some(_), _) => "verified",
        (None, Some(_)) => "failed",
        (None, None) => "unchecked",
    }
}

#[async_trait::async_trait]
impl Tool for List {
    fn name(&self) -> &'static str {
        "tasks__list"
    }

    fn description(&self) -> &'static str {
        "List the board of YOUR project. The answer is an envelope: `rows`, then `returned` (how \
         many this answer carries), `total` (how many the filters match), `truncated` (true \
         when `returned` is smaller), and `narrow`, the doors that cut it down. The default \
         limit is 20 and the most a caller may ask for is 100, so an answer always fits. \
         Each row is compact: id, subject, status, kind, owner, the wait it is on, the five \
         marks (ready, in_review, overdue, stale, to_close), its estimate or cadence, and \
         whether its evidence is `verified`, `failed` or `unchecked`. The default read \
         carries pending, in_progress, waiting and failed rows - a completed or canceled row \
         is history and is left out, and the read ends with a line counting what was withheld \
         by status and naming `status` as the door. A `status` filter overrides that default, \
         so `status: \"completed\"` returns the completed rows. For one row's whole record - \
         the `detail` prose, the links, the parent and its subject, the wait's words, the \
         evidence and its check, and the history - use `tasks__get`; `detail: true` here is \
         the heavier per-row echo and is meant for a short, narrowed read. Narrow with `owner` \
         (a session label), `parent` (a task id), `status`, `kind` (finite or standing), or \
         `ready` (true = pending with nothing waiting on it and not reserved for the user; \
         ownership is NOT part of the mark, so read `owner` too when the question is an \
         unowned row). An empty `rows` means your project has no tasks in flight (or only \
         terminal rows, which the closing line names, or archived rows, which `tasks__get` reads \
         by id), or that your project could not be resolved, or that this run could not read its \
         stored tasks. Any session in the \
         project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": "Only tasks held by this session label.",
                },
                "parent": {
                    "type": "string",
                    "description": "Only the tasks under this task id, or any unique prefix \
                                    of one.",
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "waiting", "completed", "failed", "canceled"],
                    "description": "Only tasks in this state.",
                },
                "kind": {
                    "type": "string",
                    "enum": ["finite", "standing"],
                    "description": "Only finite work or only standing work.",
                },
                "ready": {
                    "type": "boolean",
                    "description": "True lists only rows something can start: pending, \
                                    with nothing waiting on it, and not reserved for the \
                                    user. Ownership is not part of the mark: read `owner` \
                                    for an unowned row.",
                },
                "detail": {
                    "type": "boolean",
                    "description": "Optional. Set true to return each row's whole record \
                                    instead of the compact row: the parent and its subject, \
                                    the `detail` prose, `active_form`, the wait's detail \
                                    words, any attestation quote, how long since the last \
                                    touch, and the created/updated timestamps. Off by \
                                    default; for one row use tasks__get.",
                },
                "limit": {
                    "type": "integer",
                    "description": "How many rows this answer carries. Defaults to 20 and is \
                                    clamped to 100; `total` in the envelope says how many \
                                    the filters matched.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ListArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let parent = args.parent.as_deref().map(TaskId::from);
        let rows = self.facade.list_tasks(
            &self.slot,
            args.owner.as_deref(),
            parent.as_ref(),
            args.status,
            args.kind,
            args.ready,
        );
        // A first orientation read has to fit (#3030), so the default is
        // the rows still in play, and each is compact. A stated `status`
        // is the caller naming exactly the rows it wants, so it overrides
        // the terminal exclusion; otherwise the closing line says what was
        // withheld, so nothing is dropped silently.
        let (rows, withheld) = match args.status {
            Some(_) => (rows, Vec::new()),
            None => rows.into_iter().partition(|row| !is_finished(row.task.status)),
        };
        // The envelope: how many came back of how many there are, whether
        // the answer was cut, and the doors that narrow it. A caller never
        // has to infer a truncation from a row count.
        let total = rows.len();
        let limit = args.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
        let full = args.detail.unwrap_or(false);
        let shown: Vec<serde_json::Value> = rows
            .iter()
            .take(limit)
            .map(|row| if full { board_row_to_json(row) } else { board_row_compact(row) })
            .collect();
        let truncated = total > shown.len();
        let envelope = serde_json::json!({
            "rows": shown,
            "returned": shown.len(),
            "total": total,
            "truncated": truncated,
            "narrow": "narrow with `owner`, `parent`, `status`, `kind` or `ready`, or raise \
                       `limit` (max 100); `tasks__get` reads one row whole",
        });
        match serde_json::to_string_pretty(&envelope) {
            Ok(json) => {
                let mut blocks = vec![ToolOutputBlock::Text {
                    text: format!("now {}\n{json}", fmt_rfc3339(SystemTime::now())),
                }];
                // The closing line is its OWN block: the first block stays
                // the clock line plus the envelope, which is what a card
                // parses, and the extra block is drawn beside it rather
                // than swallowed.
                if let Some(line) = withheld_line(&withheld) {
                    blocks.push(ToolOutputBlock::Text { text: line });
                }
                ToolOutput { blocks, is_error: false }
            }
            Err(err) => tool_error(format!("task-list serialization failed: {err}")),
        }
    }
}

struct Get {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct GetArgs {
    id: String,
}

#[async_trait::async_trait]
impl Tool for Get {
    fn name(&self) -> &'static str {
        "tasks__get"
    }

    fn description(&self) -> &'static str {
        "Read ONE task of YOUR project whole: its identity and state, the `detail` brief, the \
         links, the parent and its subject, the wait it is on with its words, its evidence and \
         the gate's check of it, what the board derives (the marks, the worked time, the touch \
         age, the rollup) and its history - the newest transitions, with `history_total` \
         saying how many there are when the tail is capped. A row that has left the live board \
         reads from the archive, with `archived_at` set and no history. A task is named by its \
         full id or any unique prefix of one; an exact id wins over a longer id it prefixes, \
         and a prefix matching more than one row is refused with the matching rows named. Use \
         this after `tasks__list` names a row you need the rest of; keep the list read \
         compact. Any session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The task id to read, or any unique prefix of one.",
                },
            },
            "required": ["id"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: GetArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.get_task(&self.slot, &TaskId::from(args.id.as_str())) {
            Ok(row) => {
                let mut body = board_row_to_json(&row);
                if let serde_json::Value::Object(map) = &mut body {
                    map.insert("history".to_owned(), serde_json::json!(row.history));
                    map.insert("history_total".to_owned(), serde_json::json!(row.history_total));
                }
                match serde_json::to_string_pretty(&body) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Delete {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct DeleteArgs {
    id: String,
    version: u64,
}

#[async_trait::async_trait]
impl Tool for Delete {
    fn name(&self) -> &'static str {
        "tasks__delete"
    }

    fn description(&self) -> &'static str {
        "Remove a task in YOUR project by id together with its children in the same call, so a \
         parent never leaves subtasks pointing at a task that is gone. Use this to clear work \
         that is over: the board is for running work, so a landed row - its work merged, \
         delivered or concluded - with no children and no epic above it is deleted in the \
         same pass, never left completed; a landed row under an epic - open or already \
         terminal - is completed and left terminal instead, so the epic can read `to close` \
         (a verify=user row parks in waiting on the user first). Nothing \
         is archived. A row waiting on the user is not a delete's to \
         take - the board's answer or approve settles it, and deleting it (or a tree holding \
         it) is refused rather than taken as a way around the ask. A dependency wait is not \
         such an ask: the row waits on the task it names, not on the user, even when the epic \
         is `verify: user`. When the row you delete is named by another row's dependency wait, \
         move every waiter off that wait in the same pass - a wait left on a deleted row reads \
         that its target has ended indefinitely. \
         An ask that has gone moot is withdrawn, not removed: `tasks__update` cancels it with \
         the reason it goes away, and the row records why. The task is named by its \
         full id or any unique \
         prefix of one; an exact id wins over any longer id that starts with it, and a prefix \
         matching more than one row is refused with the matching rows named. An unknown id is an \
         error, not a no-op. Returns the removed task and how many descendants went with it. Any \
         session in the project may call this."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The task id to remove, or any unique prefix of one.",
                },
                "version": {
                    "type": "integer",
                    "description": "The row's version as you read it. A delete that would land \
                                    on a row another writer has since changed is refused with \
                                    the current version.",
                },
            },
            "required": ["id", "version"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: DeleteArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.delete_task(&self.slot, &TaskId::from(args.id.as_str()), args.version) {
            Ok(removed) => {
                let mut envelope =
                    crate::mcp::deleted::removed_record(&task_to_json(&removed.task));
                envelope["descendants_removed"] = serde_json::json!(removed.descendants_removed);
                match serde_json::to_string_pretty(&envelope) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Claim {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct ClaimArgs {
    id: String,
    version: u64,
}

#[async_trait::async_trait]
impl Tool for Claim {
    fn name(&self) -> &'static str {
        "tasks__claim"
    }

    fn description(&self) -> &'static str {
        "Claim a task in YOUR project: one atomic write that makes you its owner and marks it \
         in progress. Pass `id`, a full task id or any unique prefix of one, and `version`, \
         the row as you read it; an exact id wins over any longer id that starts with it, and \
         a prefix matching more than one row is refused with the matching rows named. Refused \
         when a live seat holds the row (named), when it is not pending, when it is waiting on \
         something, when it is reserved for the user (`reserved: \"user\"` - the user's pick \
         and nobody else's), when the version you named is stale, or when you already have a \
         row in progress: one in progress per worker. Claiming is for unassigned rows: a row a \
         lead created for you already names you as `owner`, so work it without claiming. \
         Returns the claimed record."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The task id to claim, or any unique prefix of one.",
                },
                "version": {
                    "type": "integer",
                    "description": "The row's version as you read it. A claim that would land \
                                    on a row another writer has since changed is refused with \
                                    the current version.",
                },
            },
            "required": ["id", "version"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ClaimArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        match self.facade.claim_task(&self.slot, &args.id, args.version) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Wait {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct WaitArgs {
    id: String,
    kind: WaitingKind,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    on: Option<String>,
    /// The ask's kind when it is on the person: decision, clarification,
    /// exception or teaching. A look needs no `ask` (it is the gate's).
    #[serde(default)]
    ask: Option<forge_primitives::tasks::AskKind>,
    /// The ask's question, one sentence: the card's headline.
    #[serde(default)]
    question: Option<String>,
    /// A decision's enumerated options; empty for every other kind.
    #[serde(default)]
    options: Vec<String>,
    /// A teaching's class: the recurring trouble the lesson settles.
    #[serde(default)]
    class: Option<String>,
    /// How long until the deadline default runs ("30m", "2h", "1d").
    #[serde(default)]
    due: Option<String>,
    /// The named default that runs when `due` passes: escalate, expire or
    /// reask. Never approve, and never without `due`.
    #[serde(default)]
    default: Option<forge_primitives::tasks::DefaultIfIgnored>,
    version: u64,
}

#[async_trait::async_trait]
impl Tool for Wait {
    fn name(&self) -> &'static str {
        "tasks__wait"
    }

    fn description(&self) -> &'static str {
        "Put one of YOUR rows into waiting and state what it waits on. `kind` is user (the \
         person), dependency (another task: put its id in `on`), or resource (an account, a CI \
         run, a machine). An ask on the person also carries `ask`, its kind: decision (a choice \
         among `options`, which are required), clarification (a fact only the person has, \
         answered in their own words), exception (a disposition: retry, rescope, drop or \
         diagnose), or teaching (a lesson that becomes project policy for the `class` it \
         names). `question` is the ask in one sentence and becomes the card's headline; \
         `detail` is the why. A teaching's class is what stops the trouble recurring: an ask \
         whose class a policy note already settles is refused with the note quoted. A `due` \
         duration with a `default` - escalate, expire or reask - gives the ask a deadline whose \
         NAMED default runs when it passes; the two arrive together, and there is no approve \
         default. Acknowledge and snooze are the person's own acts on the board, never tools. \
         A user wait is the at-convenience channel: a lead who can continue but wants the \
         person's answer puts the ask on a board row rather than in prose, because prose is not \
         a surface. **Urgency picks the channel**: an ask needed now goes to the person \
         directly, and an ask that is not immediate is raised here - a row waiting on the \
         person is blocked on them, not annotated. Route a decision to the person when they asked for a hold, when something \
         is still genuinely ambiguous after the decision model, peer opinions and the team's \
         own review, or when the person should look at the work when they come back. Raise a \
         clarification for a fact only they have, an exception for a disposition on a fault, \
         and a teaching when a class keeps recurring and a lesson settles it. Do not use any of \
         them to park work the decision model, your peers or the review can settle. A dependency wait is on the task it names, not on the user. A blocker parked on \
         the user's look has not ended, so the chain keeps waiting until the user approves it. \
         The waiting row is not itself waiting on the user: a row's `verify: user` gates its own \
         completion, not its dependency wait, so the row stays movable and deletable. Only the \
         lead raises a user ask: a worker that believes the call is the person's hands it to its \
         lead. A wait on the user that has gone moot is withdrawn, not stranded: a lead cancels \
         the row with the reason it goes away (`tasks__update`, `detail`), and the row records \
         why. The row and the `on` blocker are each named by a full id or any unique prefix of \
         one; an exact id wins over any longer id that starts with it, and a prefix matching \
         more than one row is refused with the matching rows named. The row must be yours. \
         Refused for a row you do not hold, or one that is not there. Returns the record in \
         waiting."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The task id to put into waiting, or any unique prefix of one.",
                },
                "kind": {
                    "type": "string",
                    "enum": ["user", "dependency", "resource"],
                    "description": "What it waits on: `user` is the person (an ask), \
                                    `dependency` another task, `resource` an account, CI or a \
                                    machine.",
                },
                "detail": {
                    "type": "string",
                    "description": "The words: what exactly is being waited for. For a user wait: \
                                    why it needs the user specifically, what is already decided, \
                                    what is open, what their answer changes, and the links to \
                                    read - a bare question with no why is its own failure, and \
                                    the flag exists to make the return cheap. A wait is a flag, \
                                    not a stop: carry on with your next held row and let the \
                                    answer resume this one, unless the user asked for a stop.",
                },
                "on": {
                    "type": "string",
                    "description": "For a dependency: the task id it waits on, or any unique \
                                    prefix of one.",
                },
                "ask": {
                    "type": "string",
                    "enum": ["decision", "clarification", "exception", "teaching"],
                    "description": "The ask's kind when it is on the person. A look needs no \
                                    `ask`: it is the verify gate's.",
                },
                "question": {
                    "type": "string",
                    "description": "The ask's question, in one sentence. It becomes the card's \
                                    headline, so state the ask rather than the background.",
                },
                "options": {
                    "type": "array",
                    "description": "A decision's choices, each a short phrase, at least one. \
                                    The person answers by picking one, so list the real \
                                    alternatives rather than one option and a rest. A \
                                    clarification or exception carries none.",
                    "items": { "type": "string" },
                },
                "class": {
                    "type": "string",
                    "description": "A teaching's class: the recurring trouble the lesson \
                                    settles, a short slug. An ask of a class a policy note \
                                    already settles is refused with the note quoted.",
                },
                "due": {
                    "type": "string",
                    "description": "How long until the deadline default runs (\"30m\", \"2h\", \
                                    \"1d\"). It arrives with `default` or not at all.",
                },
                "default": {
                    "type": "string",
                    "enum": ["escalate", "expire", "reask"],
                    "description": "The NAMED default that runs when `due` passes: escalate \
                                    tells the lead the ask went unanswered; expire ends the \
                                    row canceled with the reason and keeps the record; reask \
                                    re-notifies the lead and re-arms the deadline. There is \
                                    no approve default.",
                },
                "version": {
                    "type": "integer",
                    "description": "The row's version as you read it. A wait that would land \
                                    on a row another writer has since changed is refused with \
                                    the current version.",
                },
            },
            "required": ["id", "kind", "version"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: WaitArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        if args.kind == WaitingKind::Dependency && args.on.is_none() {
            return tool_error("a dependency wait needs `on`, the task it waits on".to_owned());
        }
        let on = args.on.as_deref().map(TaskId::from);
        // An ask on the person needs a kind; a decision without one is the
        // historical shape and reads as a decision.
        let ask = match (args.ask, args.question) {
            (None, None) => None,
            (kind, question) => Some(AskDraft {
                kind: kind.unwrap_or_default(),
                question: question.unwrap_or_default(),
                options: args.options,
                class: args.class,
                due: args.due,
                default: args.default,
            }),
        };
        match self.facade.wait_task(
            &self.slot,
            &TaskId::from(args.id.as_str()),
            args.kind,
            args.detail,
            on.as_ref(),
            ask,
            args.version,
        ) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Approve {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct ApproveArgs {
    id: String,
    /// The approval as the user gave it, quoted.
    approval: String,
    version: u64,
}

#[async_trait::async_trait]
impl Tool for Approve {
    fn name(&self) -> &'static str {
        "tasks__approve"
    }

    fn description(&self) -> &'static str {
        "Lead-only. Complete a row waiting on the user's LOOK by recording the approval the \
         user already gave, quoted from the chat or the look record. `approval` is that quote \
         and must not be empty: it lands in the row's detail and is the evidence the board \
         draws as lead-attested. This door is for a look only - never a decision or a \
         question, which are the user's to answer - and only the project lead may call it. \
         A verification wait on a row with no `verify: \"user\"` gate of its own is a stale \
         park, not a look: never attest it, and clear it with a move off waiting, or a \
         withdraw only when the ask is moot. \
         The user's own approve, send-back and cancel doors stay; the user's reopen strikes \
         the attestation, re-mints the look gate and parks the row on them again. A task is \
         named by its full id or any unique prefix of one. Refused when the row is not \
         waiting on a look, when the quote is empty, when the version you named is stale, or \
         when the caller is not the lead. The approve is a completion, so it runs the same \
         evidence gate: a commit or path the repository cannot back is refused, while a row \
         carrying the lead's `lead_verified` approves on that word without a repository \
         check."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The row waiting on the look, or any unique prefix of its id.",
                },
                "approval": {
                    "type": "string",
                    "description": "The user's approval, quoted as they gave it (e.g. \
                                    \"Looks right - merge\"). Required and non-empty.",
                },
                "version": {
                    "type": "integer",
                    "description": "The row's version as you read it. An approve that would \
                                    land on a row another writer has since changed is refused \
                                    with the current version.",
                },
            },
            "required": ["id", "approval", "version"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: ApproveArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let approval = args.approval.trim();
        if approval.is_empty() {
            return tool_error(
                "an attestation needs the user's approval quoted in `approval`; it must not be \
                 empty"
                    .to_owned(),
            );
        }
        match self.facade.attest_task(
            &self.slot,
            &TaskId::from(args.id.as_str()),
            approval,
            args.version,
        ) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Answer {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct AnswerArgs {
    id: String,
    /// A decision's choice: one of the row's options.
    #[serde(default)]
    choice: Option<String>,
    /// A clarification's or teaching's answer, in the person's words.
    #[serde(default)]
    text: Option<String>,
    /// An exception's disposition: retry, rescope, drop or diagnose.
    #[serde(default)]
    disposition: Option<String>,
    /// A teaching's lesson: the policy text its class is settled by.
    #[serde(default)]
    lesson: Option<String>,
    /// The note they left with it, when they left one.
    #[serde(default)]
    note: Option<String>,
    version: u64,
}

#[async_trait::async_trait]
impl Tool for Answer {
    fn name(&self) -> &'static str {
        "tasks__answer"
    }

    fn description(&self) -> &'static str {
        "Lead-only. Record the person's answer to a row waiting on an ask, with the ONE value \
         that ask's kind needs: `choice` for a decision (one of the row's options), `text` for \
         a clarification (the fact, in their words) and for a teaching (the lesson, with \
         `lesson` stating the policy text its class is settled by), or `disposition` for an \
         exception (retry, rescope, drop or diagnose). `note` is the short note they left with \
         it. The answer is stored as the row's request record and the row resumes in progress; \
         a teaching also writes the project policy note, so the class stops recurring. \
         **Never answer with your own opinion**: the ask is the person's to answer, and if \
         they have not answered the row stays waiting for them. A look is not an ask - that \
         door is tasks__approve. A task is named by its full id or any unique prefix of one. \
         Refused when the row is not waiting on an ask for the person, when the value's shape \
         does not fit the kind, when the choice is not one of its options, or when the caller \
         is not the lead."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The row waiting on the ask, or any unique prefix of its id.",
                },
                "choice": {
                    "type": "string",
                    "description": "For a decision: the option the person picked, as one of \
                                    the row's options.",
                },
                "text": {
                    "type": "string",
                    "description": "For a clarification: the fact the ask needed, in the \
                                    person's words. For a teaching: the lesson.",
                },
                "disposition": {
                    "type": "string",
                    "enum": ["retry", "rescope", "drop", "diagnose"],
                    "description": "For an exception: the disposal the person chose.",
                },
                "lesson": {
                    "type": "string",
                    "description": "For a teaching: the policy text the lesson settles its \
                                    class by, written to the project's policy notes.",
                },
                "note": {
                    "type": "string",
                    "description": "The short note the person left with the answer, if any.",
                },
                "version": {
                    "type": "integer",
                    "description": "The row's version as you read it. An answer that would \
                                    land on a row another writer has since changed is refused \
                                    with the current version.",
                },
            },
            "required": ["id", "version"],
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        if self.slot.label() != crate::LEAD_LABEL {
            return tool_error(
                "only the project lead records the person's answer; this session is a worker"
                    .to_owned(),
            );
        }
        let args: AnswerArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        // One door, one value: the ask's kind decides which, and the shape
        // is refused here where the caller can see all three fields.
        let stated = [args.choice.is_some(), args.text.is_some(), args.disposition.is_some()]
            .iter()
            .filter(|stated| **stated)
            .count();
        if stated != 1 {
            return tool_error(
                "an answer states exactly one of `choice`, `text` or `disposition`".to_owned(),
            );
        }
        match self.facade.answer_task(
            &self.slot,
            &TaskId::from(args.id.as_str()),
            args.choice.as_deref(),
            args.text.as_deref(),
            args.disposition.as_deref(),
            args.note.as_deref(),
            args.lesson.as_deref(),
            args.version,
        ) {
            Ok(task) => match serde_json::to_string_pretty(&task_to_json(&task)) {
                Ok(json) => ToolOutput::text(json),
                Err(err) => tool_error(format!("response serialization failed: {err}")),
            },
            Err(err) => tool_error(format_tasks_error(&err)),
        }
    }
}

struct Policy {
    facade: Arc<dyn TasksFacade>,
    slot: SessionSlot,
}

#[derive(serde::Deserialize)]
struct PolicyArgs {
    #[serde(default)]
    class: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[async_trait::async_trait]
impl Tool for Policy {
    fn name(&self) -> &'static str {
        "tasks__policy"
    }

    fn description(&self) -> &'static str {
        "Lead-only. Read YOUR project's policy notes - the lessons earlier teaching answers \
         settled, keyed by `class` - or write one yourself with `class` and `text`. A note is \
         the class's standing rule: it is appended to a lead's prompt at spawn, and an ask of a \
         class a note settles is refused with the note quoted, so the trouble stops recurring. \
         **A resumed lead does not see a note added since its spawn until its next spawn or \
         this read.** Call with neither argument to list; with both to write."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "class": {
                    "type": "string",
                    "description": "The recurring class of ask the note settles, a short slug.",
                },
                "text": {
                    "type": "string",
                    "description": "The lesson itself: the policy the class is settled by.",
                },
            },
            "additionalProperties": false,
        })
    }

    async fn call(&self, input: ToolInput) -> ToolOutput {
        let args: PolicyArgs = match serde_json::from_value(input.value) {
            Ok(args) => args,
            Err(err) => return tool_error(format!("invalid arguments: {err}")),
        };
        let note_json = |note: &crate::store::policy::PolicyNote| {
            serde_json::json!({
                "class": note.class,
                "text": note.text,
                "source_task": note.source_task,
                "at": fmt_rfc3339(note.at),
            })
        };
        match (args.class.as_deref(), args.text.as_deref()) {
            (None, None) => {
                let notes: Vec<serde_json::Value> =
                    self.facade.policy_notes(&self.slot).iter().map(note_json).collect();
                match serde_json::to_string_pretty(&serde_json::json!({ "notes": notes })) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                }
            }
            (Some(class), Some(text)) => match self.facade.add_policy_note(&self.slot, class, text)
            {
                Ok(note) => match serde_json::to_string_pretty(&note_json(&note)) {
                    Ok(json) => ToolOutput::text(json),
                    Err(err) => tool_error(format!("response serialization failed: {err}")),
                },
                Err(err) => tool_error(format_tasks_error(&err)),
            },
            _ => tool_error(
                "a policy write states both `class` and `text`; a read states neither".to_owned(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tasks::facade::{MockTasksFacade, RemovedTaskTree};
    use crate::mcp::test_support::{block_text, text_of};
    use forge_primitives::tasks::{Attestation, Estimate, ReservedBy, TaskLink, Waiting};

    fn lead_slot() -> SessionSlot {
        SessionSlot::lead("TestOrg", "myproj")
    }

    fn input(value: serde_json::Value) -> ToolInput {
        ToolInput { value }
    }

    #[tokio::test]
    async fn create_persists_a_task_for_the_callers_project() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Create { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "subject": "Merge peers and workers into agents" })))
            .await;
        assert!(!out.is_error, "create succeeds: {out:?}");
        let calls = facade.created.lock();
        assert_eq!(calls.len(), 1, "one task created");
        assert_eq!(calls[0].1.subject, "Merge peers and workers into agents");
        assert_eq!(calls[0].2.project_name, "myproj", "stamped with the caller's project");
    }

    #[tokio::test]
    async fn create_threads_every_optional_field_to_the_facade() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Create { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({
                "subject": "Merge peers and workers",
                "active_form": "Merging peers and workers",
                "detail": "One agents family.",
                "status": "in_progress",
                "owner": "agents-merge",
                "parent": "epic",
                "rank": 10,
                "verify": "user",
                "reserved": "user",
                "links": [
                    { "kind": "issue", "label": "PR #1173", "target": "https://example.invalid/pull/1173" },
                    { "target": "docs/plan.md" }
                ],
                "estimate": "1d",
            })))
            .await;
        assert!(!out.is_error, "create succeeds: {out:?}");
        let calls = facade.created.lock();
        let draft = &calls[0].1;
        assert_eq!(draft.active_form.as_deref(), Some("Merging peers and workers"));
        assert_eq!(draft.detail.as_deref(), Some("One agents family."));
        assert_eq!(draft.status, Some(TaskStatus::InProgress));
        assert_eq!(draft.owner.as_deref(), Some("agents-merge"));
        assert_eq!(draft.parent.as_deref(), Some("epic"));
        assert_eq!(draft.rank, Some(10));
        assert_eq!(draft.verify, Some(forge_primitives::tasks::Verify::User));
        assert_eq!(
            draft.reserved,
            Some(forge_primitives::tasks::ReservedBy::User),
            "the reserved pick reaches the facade",
        );
        assert_eq!(draft.links.len(), 2);
        assert_eq!(draft.links[0].kind, Some(LinkKind::Issue));
        assert_eq!(
            draft.links[1].kind, None,
            "an omitted kind is derived at the facade, not guessed here",
        );
        assert_eq!(draft.links[1].target, "docs/plan.md");
        assert_eq!(draft.estimate.as_deref(), Some("1d"));
    }

    #[tokio::test]
    async fn create_rejects_a_missing_subject_without_touching_the_facade() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Create { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "detail": "no subject" })))
            .await;
        assert!(out.is_error, "subject is required");
        assert!(facade.created.lock().is_empty(), "invalid input never reaches the facade");
    }

    #[tokio::test]
    async fn approve_threads_the_quote_and_the_caller_to_the_facade() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Approve { facade: facade.clone(), slot: lead_slot() }
            .call(input(
                serde_json::json!({ "id": "t-1", "approval": " looks good ", "version": 3 }),
            ))
            .await;
        assert!(!out.is_error, "approve succeeds: {out:?}");
        let calls = facade.attested.lock();
        assert_eq!(calls.len(), 1, "one attestation reached the facade");
        assert_eq!(calls[0].1, TaskId::from("t-1"), "the row the caller named");
        assert_eq!(calls[0].2, "looks good", "the quote is trimmed, not dropped");
    }

    #[tokio::test]
    async fn approve_refuses_an_empty_quote_without_touching_the_facade() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Approve { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "approval": "   ", "version": 3 })))
            .await;
        assert!(out.is_error, "an empty approval is nothing to stand behind");
        assert!(facade.attested.lock().is_empty(), "invalid input never reaches the facade");
    }

    /// The lead door is shipped text (#2423/B): a caller learns it is
    /// lead-only and look-only, that the quote is required, and that the
    /// user's reopen is the way back.
    #[test]
    fn the_approve_description_states_the_lead_door() {
        let approve = Approve { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        for clause in [
            "Lead-only",
            "never a decision or a question",
            "must not be empty",
            "the user's reopen strikes the attestation",
        ] {
            assert!(
                approve.description().contains(clause),
                "tasks__approve does not state the door ({clause}): {}",
                approve.description(),
            );
        }
        assert_eq!(
            approve.input_schema()["required"],
            serde_json::json!(["id", "approval", "version"]),
            "the quote and the version it read are required at the schema",
        );
    }

    /// The gate is minted on the row, never inherited, so a verification
    /// wait left behind by an inherited gate is a stale park - not a look
    /// the lead can attest. Its door is a move or a withdraw, and the
    /// description says so where the lead reads the door's scope.
    #[test]
    fn the_approve_description_separates_a_stale_park_from_the_look() {
        let approve = Approve { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        assert!(
            approve.description().contains(
                "A verification wait on a row with no `verify: \"user\"` gate of its own is a \
                 stale park, not a look"
            ),
            "the stale park is separated from the look: {}",
            approve.description(),
        );
        assert!(
            approve.description().contains(
                "never attest it, and clear it with a move off waiting, or a withdraw only \
                 when the ask is moot"
            ),
            "the stale park's own door and its moot condition are pinned: {}",
            approve.description(),
        );
    }

    #[tokio::test]
    async fn update_reports_a_missing_id_rather_than_succeeding_silently() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Update { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "nope", "status": "completed", "version": 0 })))
            .await;
        assert!(out.is_error, "an unknown id is an error, not a no-op");
        assert!(
            text_of(&out).contains("nope"),
            "the refusal names the id it was given, so the caller can see what it sent: {out:?}",
        );
    }

    /// The update refusal says which case it is: an id another project
    /// owns reads differently from one no project has, so neither can be
    /// taken for the other.
    #[tokio::test]
    async fn update_refusals_name_the_case() {
        let facade = Arc::new(MockTasksFacade::default());
        let tool = Update { facade: facade.clone(), slot: lead_slot() };

        *facade.update_result.lock() = Some(Err(TasksError::TaskInAnotherProject {
            id: "t-9".to_owned(),
            project: "otherproj".to_owned(),
        }));
        let elsewhere = tool
            .call(input(serde_json::json!({ "id": "t-9", "status": "pending", "version": 0 })))
            .await;
        assert!(elsewhere.is_error, "another project's row is refused");
        assert_eq!(
            text_of(&elsewhere),
            "task t-9 exists in project otherproj, not in your project",
            "the refusal names the project that owns the row",
        );

        *facade.update_result.lock() = Some(Err(TasksError::NoSuchTask { id: "t-9".to_owned() }));
        let absent = tool
            .call(input(serde_json::json!({ "id": "t-9", "status": "pending", "version": 0 })))
            .await;
        assert!(absent.is_error, "an absent id is refused");
        assert_eq!(
            text_of(&absent),
            "no task with id t-9 in your project",
            "an id no project has must not read as another project's row",
        );
    }

    /// The update result is the record it wrote, so the caller can name
    /// the task it moved and read the state it now holds - `updated task
    /// <id>` named neither.
    #[tokio::test]
    async fn update_echoes_the_updated_record() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut task = sample_task();
        task.status = TaskStatus::InProgress;
        task.owner = Some(SessionSlot::lead("TestOrg", "myproj"));
        *facade.update_result.lock() = Some(Ok(task));
        let out = Update { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "status": "in_progress", "version": 0 })))
            .await;
        assert!(!out.is_error, "update succeeds: {out:?}");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the task record, not prose");
        assert_eq!(json["id"], "t-1", "the record names the task it moved: {json}");
        assert_eq!(json["subject"], "Merge peers and workers");
        assert_eq!(json["status"], "in_progress", "and the state it now holds");
        assert_eq!(json["owner"], "lead");
    }

    /// Every write names the version it read, so an update that names none
    /// is refused before the facade sees it.
    #[tokio::test]
    async fn update_refuses_a_write_that_names_no_version() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Update { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "status": "pending" })))
            .await;
        assert!(out.is_error, "a write names the version it read");
        assert!(
            text_of(&out).contains("names the version"),
            "the refusal states the rule: {:?}",
            text_of(&out),
        );
        assert!(facade.updated.lock().is_empty(), "the facade is never reached");
    }

    /// A delete echoes what went, in the adopted envelope: the record as
    /// it stood, plus the cascade's own count - the row has to say how
    /// much of the tree went with it. The record is compared to
    /// `task_to_json` whole, so an echo trimmed to a couple of fields
    /// cannot read as the record.
    #[tokio::test]
    async fn delete_echoes_the_removed_record_and_the_cascade_count() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.delete_result.lock() =
            Some(Ok(RemovedTaskTree { task: sample_task(), descendants_removed: 2 }));
        let out = Delete { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "version": 0 })))
            .await;
        assert!(!out.is_error, "delete succeeds: {out:?}");
        assert_eq!(out.blocks.len(), 1, "the envelope stays one text block: {out:?}");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the adopted envelope");
        assert_eq!(json["status"], "deleted");
        assert_eq!(
            json["removed"],
            task_to_json(&sample_task()),
            "the echo is the whole record, not a hand-picked subset: {json}",
        );
        assert_eq!(json["descendants_removed"], 2);
    }

    /// The count is always there: a leaf delete says zero rather than
    /// leaving the field out, so a reader never has to tell an omitted
    /// count from a cascade that took nothing.
    #[tokio::test]
    async fn a_delete_with_no_children_still_states_the_count() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.delete_result.lock() =
            Some(Ok(RemovedTaskTree { task: sample_task(), descendants_removed: 0 }));
        let out = Delete { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "version": 0 })))
            .await;
        assert!(!out.is_error, "delete succeeds: {out:?}");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the adopted envelope");
        assert_eq!(json["descendants_removed"], 0, "a leaf states its count: {json}");
    }

    #[tokio::test]
    async fn update_states_only_the_fields_it_was_given() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.update_result.lock() = Some(Ok(sample_task()));
        let out = Update { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({
                "id": "t-1",
                "status": "waiting",
                "detail": "why",
                "links_add": [{ "target": "x" }],
                "version": 0
            })))
            .await;
        assert!(!out.is_error, "update succeeds: {out:?}");
        let patch = &facade.updated.lock()[0].2;
        assert_eq!(patch.status, Some(TaskStatus::Waiting));
        assert_eq!(patch.detail.as_deref(), Some("why"), "the detail lands with the link");
        assert_eq!(patch.links_add.len(), 1, "the stated link reaches the facade");
        assert_eq!(patch.links_add[0].target, "x");
        assert_eq!(patch.subject, None, "an unstated field is left alone");
        assert_eq!(patch.owner, None, "an unstated field is left alone");
    }

    fn sample_task() -> Task {
        Task {
            id: TaskId::from("t-1"),
            project_name: "myproj".to_owned(),
            subject: "Merge peers and workers".to_owned(),
            active_form: None,
            detail: None,
            status: TaskStatus::Pending,
            kind: forge_primitives::tasks::TaskKind::Finite,
            owner: None,
            parent: None,
            waiting_on: None,
            request: None,
            estimate: None,
            cadence: None,
            rank: None,
            verify: None,
            attested: None,
            reserved: None,
            links: Vec::new(),
            evidence: None,
            version: 0,
            archived_at: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    /// The spelling a caller reads in a result, the spelling it must send
    /// back, and the spelling the description names are the same six. Every
    /// one of those sites writes them out by hand rather than deriving them
    /// from the enum, so nothing else pins them to it.
    #[test]
    fn the_status_spellings_agree_across_the_output_and_both_schemas() {
        for (status, spelling) in [
            (TaskStatus::Pending, "pending"),
            (TaskStatus::InProgress, "in_progress"),
            (TaskStatus::Waiting, "waiting"),
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Canceled, "canceled"),
        ] {
            assert_eq!(status_str(status), spelling, "a result reads {status:?} as {spelling}");
        }
        for schema in [
            Create { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() }.input_schema(),
            Update { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() }.input_schema(),
        ] {
            let spelled: Vec<&str> = schema["properties"]["status"]["enum"]
                .as_array()
                .expect("the status property carries an enum")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect();
            assert_eq!(
                spelled,
                ["pending", "in_progress", "waiting", "completed", "failed", "canceled"],
                "a caller must be able to send back what it read",
            );
        }

        // The description is the site an agent reads BEFORE it writes anything,
        // and the one nothing checks: it named `blocked` for a release after
        // the enum dropped it, so a session following it took a hard
        // `invalid arguments` where the schema would have said the same thing
        // more cheaply.
        let description = Update { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() }
            .description()
            .to_owned();
        for spelling in ["pending", "in_progress", "waiting", "completed", "failed", "canceled"] {
            assert!(
                description.contains(spelling),
                "the description does not name {spelling}, which the schema accepts",
            );
        }
        assert!(
            !description.contains("blocked"),
            "the description names a status the enum no longer carries",
        );
    }

    /// Every field the list description promises is in the block the model
    /// reads, spelled the way the description spells it, carrying the value
    /// its own field holds - the optional entries are one shared tuple, so
    /// a crossed pair of sources would otherwise read as correct.
    #[test]
    fn a_task_block_carries_the_named_fields() {
        let task = Task {
            active_form: Some("Merging".to_owned()),
            detail: Some("why".to_owned()),
            estimate: Some(Estimate { words: "1d".to_owned(), secs: 86_400 }),
            links: vec![TaskLink {
                kind: LinkKind::Pr,
                label: Some("PR #9".to_owned()),
                target: "https://example.invalid/pull/9".to_owned(),
                state: None,
                last_check: None,
                added_at: SystemTime::UNIX_EPOCH,
            }],
            parent: Some(TaskId::from("epic")),
            owner: Some(SessionSlot::lead("TestOrg", "myproj")),
            ..sample_task()
        };
        let json = task_to_json(&task);
        for key in [
            "id",
            "project",
            "subject",
            "status",
            "active_form",
            "detail",
            "artifact",
            "estimate",
            "parent",
            "owner",
            "created_at",
            "updated_at",
        ] {
            assert!(json.get(key).is_some(), "the task block carries {key}: {json}");
        }
        for (key, value) in [
            ("id", "t-1"),
            ("project", "myproj"),
            ("subject", "Merge peers and workers"),
            ("status", "pending"),
            ("active_form", "Merging"),
            ("detail", "why"),
            ("artifact", "PR #9"),
            ("estimate", "1d"),
            ("parent", "epic"),
            ("owner", "lead"),
        ] {
            assert_eq!(
                json.get(key).and_then(serde_json::Value::as_str),
                Some(value),
                "the block's {key} is the task's own {key}: {json}",
            );
        }
    }

    /// An unclaimed task carries no `owner` key at all. Writing a sentinel
    /// makes the field unreadable: a caller filtering on what it read back
    /// finds none, and a session actually labelled `unclaimed` reads the
    /// same as nobody.
    #[test]
    fn an_unclaimed_task_omits_its_owner() {
        let json = task_to_json(&sample_task());
        assert!(json.get("owner").is_none(), "an unclaimed task carries no owner key: {json}");
    }

    #[test]
    fn a_claimed_task_names_its_owner_by_label() {
        let task = Task {
            owner: Some(SessionSlot::worker("TestOrg", "myproj", "steward")),
            ..sample_task()
        };
        let json = task_to_json(&task);
        assert_eq!(json.get("owner").and_then(serde_json::Value::as_str), Some("steward"));
    }

    /// An empty list has three causes, and the description names all of
    /// them. This is load-bearing rather than a prose lock: the caller is a
    /// model deciding whether to wait, to fix its project, or to give up,
    /// and an empty array that reads as "nothing in flight" when the store
    /// could not be read tells it to wait for work that will never appear.
    /// The cron group's list description carries the same clauses and the
    /// same test.
    #[test]
    fn the_list_description_owns_up_to_every_empty_result() {
        let desc = List { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() }
            .description()
            .to_owned();
        for clause in [
            "An empty `rows` means your project has no tasks in flight",
            "or that your project could not be resolved",
            "or that this run could not read its stored tasks",
        ] {
            assert!(desc.contains(clause), "the list description owes {clause:?}: {desc}");
        }
    }

    /// The description is the site a caller reads at the moment it picks
    /// `kind`, and `decision` is the one kind that parks a row on the user.
    /// Pinned so the parking test cannot drop out of the text that hands
    /// the caller that choice, and so the flag's context bar cannot drop
    /// out of the line the caller writes it on.
    #[test]
    fn the_wait_description_carries_the_attention_rule() {
        let wait = Wait { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        let desc = wait.description();
        for clause in [
            "Route a decision to the person when they asked for a hold",
            "something is still genuinely ambiguous after the decision model, peer opinions and \
             the team's own review",
            "when the person should look at the work when they come back",
            "a clarification for a fact only they have",
            "an exception for a disposition on a fault",
            "a teaching when a class keeps recurring and a lesson settles it",
        ] {
            assert!(desc.contains(clause), "the wait description owes {clause:?}: {desc}");
        }
        let detail = wait.input_schema()["properties"]["detail"]["description"]
            .as_str()
            .expect("the detail property carries a description")
            .to_owned();
        for clause in [
            "why it needs the user specifically, what is already decided, what is open, what \
             their answer changes, and the links to read",
            "a bare question with no why is its own failure",
            "A wait is a flag, not a stop",
            "unless the user asked for a stop",
        ] {
            assert!(detail.contains(clause), "the detail line owes {clause:?}: {detail}");
        }
    }

    /// The description is the site a caller reads as it reaches for the
    /// tool, and the failure this fixes is a caller sending the short form
    /// it was told. Pinned per tool, so a later edit cannot drop the rule
    /// from one of the four verbs that resolve an id.
    #[test]
    fn every_task_tool_description_states_the_id_prefix_rule() {
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let claim = Claim { facade: facade.clone(), slot: slot.clone() };
        let update = Update { facade: facade.clone(), slot: slot.clone() };
        let wait = Wait { facade: facade.clone(), slot: slot.clone() };
        let delete = Delete { facade: facade.clone(), slot: slot.clone() };
        let create = Create { facade: facade.clone(), slot: slot.clone() };
        let list = List { facade: facade.clone(), slot: slot.clone() };
        for (name, description, schema, also_named) in [
            ("tasks__claim", claim.description(), claim.input_schema(), &[] as &[&str]),
            ("tasks__update", update.description(), update.input_schema(), &[]),
            ("tasks__wait", wait.description(), wait.input_schema(), &["on"]),
            ("tasks__delete", delete.description(), delete.input_schema(), &[]),
        ] {
            for clause in [
                "unique prefix of one",
                // An exact id is a name, not a prefix of a longer one, and
                // no other site says so.
                "an exact id wins over any longer id that starts with it",
                "refused with the matching rows named",
            ] {
                assert!(
                    description.contains(clause),
                    "{name} does not state the rule ({clause}): {description}",
                );
            }
            for property_name in ["id"].into_iter().chain(also_named.iter().copied()) {
                let property =
                    schema["properties"][property_name]["description"].as_str().unwrap_or_else(
                        || panic!("{name} carries no {property_name} property description"),
                    );
                assert!(
                    property.contains("unique prefix"),
                    "{name}'s {property_name} property does not carry the rule: {property}",
                );
            }
        }
        // `parent` is an id a caller states too: the two writes store the
        // row it names, and the list narrows by it.
        for (name, schema) in [
            ("tasks__create", create.input_schema()),
            ("tasks__update", update.input_schema()),
            ("tasks__list", list.input_schema()),
        ] {
            let property = schema["properties"]["parent"]["description"]
                .as_str()
                .unwrap_or_else(|| panic!("{name} carries no parent property description"));
            assert!(
                property.contains("unique prefix"),
                "{name}'s parent property does not carry the rule: {property}",
            );
        }
    }

    /// The delete states the ask gate the way the update does (#2251): a
    /// caller reaching for the verb learns a row waiting on the user - a
    /// look or a question - is not its to take, so the refusal is never
    /// the first word about it. The update's own look clause is the
    /// precedent - each description clause the verb depends on carries a
    /// test that fails when it goes missing.
    #[test]
    fn the_delete_description_states_the_ask_gate() {
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let delete = Delete { facade, slot };
        let description = delete.description();
        for clause in [
            "A row waiting on the user is not a delete's to take",
            "the board's answer or approve settles it",
            "refused rather than taken as a way around the ask",
        ] {
            assert!(
                description.contains(clause),
                "tasks__delete does not state the rule ({clause}): {description}",
            );
        }
    }

    /// The dependency chain is shipped text (#2415): a dependency wait is
    /// on the task it names, not on the user, so `verify: user` on the row
    /// or its epic leaves the waiting row movable and deletable and the
    /// chain is released by the blocker's end.
    #[test]
    fn the_wait_and_delete_descriptions_state_the_dependency_chain() {
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let wait = Wait { facade: facade.clone(), slot: slot.clone() };
        let delete = Delete { facade, slot };
        for clause in [
            "A dependency wait is on the task it names, not on the user",
            "A blocker parked on the user's look has not ended, so the chain keeps waiting \
             until the user approves it",
            "a row's `verify: user` gates its own completion, not its dependency wait",
            "the row stays movable and deletable",
        ] {
            assert!(
                wait.description().contains(clause),
                "tasks__wait does not state the chain ({clause}): {}",
                wait.description(),
            );
        }
        assert!(
            delete.description().contains("A dependency wait is not such an ask"),
            "tasks__delete does not separate a dependency wait from the user's asks: {}",
            delete.description(),
        );
    }

    /// **The wait is the at-convenience channel as well as the hold**:
    /// without it a seat writes its look request in prose, which reaches
    /// nobody once the turn ends.
    #[test]
    fn the_wait_description_states_the_at_convenience_channel() {
        let facade = MockTasksFacade::new().into_arc();
        let wait = Wait { facade, slot: lead_slot() };
        for clause in [
            "A user wait is the at-convenience channel",
            "puts the ask on a board row rather than in prose",
            "prose is not a surface",
        ] {
            assert!(
                wait.description().contains(clause),
                "tasks__wait does not state the channel ({clause}): {}",
                wait.description(),
            );
        }
    }

    /// A wait left on an ended row reads that its target has ended
    /// forever, so the verb that ends the row owns moving every waiter
    /// off that wait in the same pass. Pinned on both doors, because
    /// either one can strand the chain.
    #[test]
    fn the_update_and_delete_descriptions_state_the_waiter_release() {
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let update = Update { facade: facade.clone(), slot: slot.clone() };
        let delete = Delete { facade, slot };
        for (name, description, remedy) in [
            (
                "tasks__update",
                update.description(),
                "When the row you end is named by another row's dependency wait, move every \
                 waiter off that wait in the same pass - a wait left on an ended row reads \
                 that its target has ended indefinitely",
            ),
            (
                "tasks__delete",
                delete.description(),
                "When the row you delete is named by another row's dependency wait, move every \
                 waiter off that wait in the same pass - a wait left on a deleted row reads \
                 that its target has ended indefinitely",
            ),
        ] {
            assert!(
                description.contains(remedy),
                "{name} does not state the waiter release: {description}",
            );
        }
    }

    /// The withdraw door is shipped text (#2340): each verb that reaches a
    /// row waiting on the user states the door - the cancel carrying the
    /// reason, which lands on the row - so a lead reads the way out from
    /// the description it already has in hand, not from a refusal.
    #[test]
    fn the_task_descriptions_state_the_withdraw_door() {
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let update = Update { facade: facade.clone(), slot: slot.clone() };
        let delete = Delete { facade: facade.clone(), slot: slot.clone() };
        let wait = Wait { facade, slot };
        for (name, description, clauses) in [
            (
                "tasks__update",
                update.description(),
                &[
                    "a cancel of a row waiting on the user is the withdraw door once the ask has",
                    "it needs `detail`, the reason the ask is withdrawn, which lands on the row",
                    "and a cancel stating none - or only what the row already says - is refused",
                ] as &[&str],
            ),
            (
                "tasks__delete",
                delete.description(),
                &[
                    "An ask that has gone moot is withdrawn, not removed",
                    "the reason it goes away, and the row records why",
                ],
            ),
            (
                "tasks__wait",
                wait.description(),
                &[
                    "A wait on the user that has gone moot is withdrawn, not stranded",
                    "row with the reason it goes away (`tasks__update`, `detail`), and the row \
                     records why",
                ],
            ),
        ] {
            for clause in clauses {
                assert!(
                    description.contains(clause),
                    "{name} does not state the door ({clause}): {description}",
                );
            }
        }
    }

    /// The board's Backlog is the `pending` status (#2831): the update is
    /// where an existing row is moved and the create's status property is
    /// where a new one is filed, so both say what pending means and name
    /// ownership as the way to set a row aside. Without it, pending reads as
    /// a parking space the drive then treats as work nobody has taken.
    #[test]
    fn the_task_text_says_pending_is_the_backlog() {
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let update = Update { facade: facade.clone(), slot: slot.clone() };
        let create = Create { facade, slot };
        let description = update.description();
        for clause in [
            "Pending is the board's Backlog: work kept to look at later, not by itself an \
             assignment and not a promise to start",
            "A pending row that reads ready and unowned is dispatchable; take ownership to set \
             it aside",
        ] {
            assert!(
                description.contains(clause),
                "tasks__update does not state what pending means ({clause}): {description}",
            );
        }
        let status = create.input_schema()["properties"]["status"]["description"]
            .as_str()
            .expect("the status property carries a description")
            .to_owned();
        for clause in [
            "Defaults to pending, the board's Backlog: work kept to look at later, not by \
             itself an assignment",
            "The drive nudges the lead about unowned ready work, so take ownership to set one \
             aside",
        ] {
            assert!(
                status.contains(clause),
                "tasks__create's status property owes the backlog where a row is filed \
                 ({clause}): {status}",
            );
        }
    }

    /// Delete-on-landing is shipped text (#2377): the board is for running
    /// work, so a landed row with no children and no epic above it is
    /// deleted in the same pass rather than left completed. The carve-out
    /// is the board's own: an epic reads `to close` only while it holds
    /// terminal children, so a landed child of an epic - open or already
    /// terminal - is completed and left terminal instead, and a
    /// verify=user child parks in waiting on the user first. The update's
    /// status property is where the caller picks the terminal state, so it
    /// carries all of it.
    #[test]
    fn the_task_descriptions_state_delete_on_landing_and_the_epic_carve_out() {
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let update = Update { facade: facade.clone(), slot: slot.clone() };
        let delete = Delete { facade, slot };
        for (name, description, clause) in [
            (
                "tasks__update",
                update.description(),
                "The board is for running work: a landed row with no children and no epic \
                 above it is DELETED in the same pass (tasks__delete), never left completed; \
                 a landed row under an epic - open or already terminal - is completed and \
                 left terminal, because the epic reads `to close` only once every child is \
                 terminal (a verify=user child parks in waiting on the user and counts only \
                 once the user approves); and completing the epic's ROOT row closes it and \
                 archives the whole tree",
            ),
            (
                "tasks__delete",
                delete.description(),
                "a landed row - its work merged, delivered or concluded - with no children \
                 and no epic above it is deleted in the same pass, never left completed; a \
                 landed row under an epic - open or already terminal - is completed and left \
                 terminal instead, so the epic can read `to close` (a verify=user row parks \
                 in waiting on the user first)",
            ),
        ] {
            assert!(
                description.contains(clause),
                "{name} does not state the landing rule ({clause}): {description}",
            );
        }
        let status = update.input_schema()["properties"]["status"]["description"]
            .as_str()
            .expect("the status property carries a description")
            .to_owned();
        for clause in [
            "`completed` is not a resting state for a landed row with no children and no \
             epic above it",
            "A landed row under an epic - open or already terminal - stays completed and \
             terminal so the epic can read `to close`; a verify=user row parks in waiting on \
             the user first",
        ] {
            assert!(
                status.contains(clause),
                "the status property owes the rule where the caller picks it ({clause}): \
                 {status}",
            );
        }
    }

    /// An epic child completes when its deliverable is out, not when a
    /// commit lands (#3008): a worker marked its children complete when
    /// their commits landed, before anything was pushed or a PR existed,
    /// and the board read done while nothing had been delivered. Both
    /// shapes ship because the board carries non-PR deliverables, and the
    /// change case names the worker's own checks rather than the lead's
    /// gate. The update description and its status property are the two
    /// sites a caller reads as it picks `completed`, so both carry the
    /// moment.
    #[test]
    fn the_update_description_states_the_delivery_completion_moment() {
        let update = Update { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        let status = update.input_schema()["properties"]["status"]["description"]
            .as_str()
            .expect("the status property carries a description")
            .to_owned();
        for (site, text) in [
            ("the update description", update.description().to_owned()),
            ("the status property", status),
        ] {
            for clause in [
                "A child of an epic completes when its deliverable is out",
                "for a change, delivered means the PR exists and the delivering worker's own \
                 checks ran at its head",
                "for a report or a sweep, concluded means it is handed over",
                "Being committed is not the claim",
            ] {
                assert!(
                    text.contains(clause),
                    "{site} does not state when a child completes ({clause}): {text}",
                );
            }
        }
    }

    /// The waiting-completion refusal is shipped text (#2414): a caller
    /// reaching for `status: completed` learns from the description that
    /// a user-bound row already waiting on something else refuses the
    /// move by name, so the refusal is never the first word about it.
    #[test]
    fn the_update_description_states_the_waiting_completion_refusal() {
        let update = Update { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        let description = update.description();
        for clause in [
            "except a completion on a row whose completion waits on the user",
            "the gate would trade that wait for a look park, so the completion is refused by \
             name",
            "move it off waiting first, then complete it",
        ] {
            assert!(
                description.contains(clause),
                "tasks__update does not state the refusal ({clause}): {description}",
            );
        }
    }

    /// The verify gate is the lead's to mint on the update door too: a
    /// worker that reads only the update description must not find
    /// `verify: "user"` offered to it as an unconditional set.
    #[test]
    fn the_update_verify_door_keeps_the_mint_with_the_lead() {
        let update = Update { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        assert!(
            update.description().contains(
                "Only the lead mints this gate; a worker hands the ask to its lead instead"
            ),
            "the update description keeps the gate with the lead: {}",
            update.description(),
        );
        let verify = update.input_schema()["properties"]["verify"]["description"]
            .as_str()
            .expect("the verify property carries a description")
            .to_owned();
        assert!(
            verify.contains("Can be set here by the lead; a worker asks its lead instead"),
            "the verify property keeps the gate with the lead: {verify}",
        );
    }

    /// The user-read mint is shipped text (#2377), and the gate belongs to
    /// the row it is minted on (#2423): a row whose deliverable IS the
    /// user's own read or decision states `verify: "user"`, while an
    /// epic's gate governs the epic's own completion and a child never
    /// inherits it. The verify property is where the caller sets it.
    #[test]
    fn the_create_description_states_the_user_read_mint_never_inherited() {
        let create = Create { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        let description = create.description();
        for clause in [
            "A row whose deliverable IS the user's own read or decision - a report the user \
             asked to read, a verdict the user asked to give - is minted `verify: \"user\"` \
             explicitly: it ends parked on the user, and the board's approve is what completes \
             it",
            "Only the lead mints this gate; a worker hands the ask to its lead instead",
            "an epic's `verify: \"user\"` governs the epic's own completion, and a child never \
             inherits it",
            "a child that owes no look completes on its own terms",
        ] {
            assert!(
                description.contains(clause),
                "tasks__create does not state the mint ({clause}): {description}",
            );
        }
        let verify = create.input_schema()["properties"]["verify"]["description"]
            .as_str()
            .expect("the verify property carries a description")
            .to_owned();
        for clause in [
            "The gate is minted on the row; a child never inherits its epic's",
            "anything else owes no look, even under a `user` epic",
            "The gate is the lead's to mint; a worker asks its lead instead",
        ] {
            assert!(
                verify.contains(clause),
                "the verify property owes the mint where the caller sets it ({clause}): {verify}",
            );
        }
    }

    /// Only the lead mints a user-decision wait; a worker hands the ask
    /// over instead, because a decision a worker parks outlives its
    /// answer and the lead cannot close it afterward. Pinned on the wait
    /// verb, which is where the decision kind is chosen.
    #[test]
    fn the_wait_description_keeps_the_user_decision_mint_with_the_lead() {
        let wait = Wait { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        for clause in [
            "Only the lead raises a user ask",
            "a worker that believes the call is the person's hands it to its lead",
        ] {
            assert!(
                wait.description().contains(clause),
                "tasks__wait does not keep the ask with the lead ({clause}): {}",
                wait.description(),
            );
        }
    }

    /// The assign-at-spawn path is shipped text (#2416): a lead creating
    /// a row for a worker assigns it at creation and says in the kick not
    /// to claim; claiming is for unassigned rows. Nothing else said so -
    /// a worker met the refusal before it met the rule.
    #[test]
    fn the_create_and_claim_descriptions_state_the_assign_at_spawn_pattern() {
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let create = Create { facade: facade.clone(), slot: slot.clone() };
        let claim = Claim { facade, slot };
        for clause in [
            "A lead creating a row for a worker assigns it at creation",
            "the kick says not to claim it",
            "refuses the worker's `tasks__claim` while the lead is live",
            "chased as having no live owner once the board marks it stale",
        ] {
            assert!(
                create.description().contains(clause),
                "tasks__create does not state the pattern ({clause}): {}",
                create.description(),
            );
        }
        for clause in [
            "Claiming is for unassigned rows",
            "a row a lead created for you already names you as `owner`",
            "work it without claiming",
        ] {
            assert!(
                claim.description().contains(clause),
                "tasks__claim does not state the pattern ({clause}): {}",
                claim.description(),
            );
        }
    }

    #[tokio::test]
    async fn list_returns_project_tasks() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
    }

    fn board_row(task: Task) -> crate::board::BoardRow {
        crate::board::BoardRow {
            task,
            worked_secs: 0,
            updated_secs_ago: 0,
            owner_live: true,
            wait_secs: None,
            marks: crate::board::Marks {
                ready: false,
                in_review: false,
                overdue: false,
                stale: false,
                to_close: false,
            },
            rollup: None,
            parent_subject: None,
            history: Vec::new(),
            history_total: 0,
        }
    }

    /// A row carrying every prose field the compact default is meant to
    /// leave behind, so a compact row that quietly kept one fails.
    fn rich_row() -> crate::board::BoardRow {
        let mut row = board_row(Task {
            active_form: Some("Merging peers and workers".to_owned()),
            detail: Some("the long why a sibling reads in full".to_owned()),
            estimate: Some(Estimate { words: "2h".to_owned(), secs: 7_200 }),
            parent: Some(TaskId::from("epic")),
            owner: Some(SessionSlot::worker("TestOrg", "myproj", "w-1")),
            verify: Some(forge_primitives::tasks::Verify::User),
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::User),
                detail: Some("the words the user answers".to_owned()),
                on: None,
                verification: true,
                parked: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: None,
            }),
            attested: Some(Attestation {
                evidence: "looks good".to_owned(),
                at: SystemTime::UNIX_EPOCH,
            }),
            ..sample_task()
        });
        row.worked_secs = 3_600;
        row.updated_secs_ago = 60;
        row.parent_subject = Some("the epic".to_owned());
        row
    }

    /// A row carrying every field the compact projection promises, so
    /// each one can be asserted by value rather than by the projection's
    /// own output.
    fn populated_row() -> crate::board::BoardRow {
        let mut row = board_row(Task {
            active_form: Some("Merging".to_owned()),
            detail: Some("why".to_owned()),
            estimate: Some(Estimate { words: "2h".to_owned(), secs: 7_200 }),
            parent: Some(TaskId::from("epic")),
            owner: Some(SessionSlot::worker("TestOrg", "myproj", "w-1")),
            verify: Some(forge_primitives::tasks::Verify::User),
            reserved: Some(ReservedBy::User),
            links: vec![TaskLink {
                kind: LinkKind::Pr,
                label: Some("PR #9".to_owned()),
                target: "https://example.invalid/pull/9".to_owned(),
                state: None,
                last_check: None,
                added_at: SystemTime::UNIX_EPOCH,
            }],
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::Dependency),
                detail: Some("the blocker".to_owned()),
                on: Some(TaskId::from("t-9")),
                verification: false,
                parked: true,
                since: None,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
            }),
            attested: Some(Attestation {
                evidence: "looks good".to_owned(),
                at: SystemTime::UNIX_EPOCH,
            }),
            ..sample_task()
        });
        row.worked_secs = 3_600;
        row.updated_secs_ago = 60;
        row.task.archived_at = Some(SystemTime::UNIX_EPOCH);
        row.marks.in_review = true;
        row.marks.overdue = true;
        row.task.evidence = Some(forge_primitives::tasks::Evidence {
            commit: Some("abc123".to_owned()),
            path: None,
            lead_verified: Some("read the sibling repo; it holds".to_owned()),
            excerpt: None,
            verified_at: Some(SystemTime::UNIX_EPOCH),
            problem: None,
        });
        row.rollup = Some((2, 5));
        row.parent_subject = Some("the epic".to_owned());
        row
    }

    /// The rows array under the dated header line in the FIRST block, and
    /// the closing line as its own second block when the default withheld
    /// finished rows.
    fn listed(out: &ToolOutput) -> (serde_json::Value, Option<String>) {
        let text = text_of(out);
        let body = text.split_once('\n').expect("a header line then the envelope").1;
        let json = serde_json::from_str(body).expect("the body is the list envelope");
        let tail = out.blocks.get(1).map(block_text).map(str::to_owned);
        (json, tail)
    }

    /// The envelope's `rows`, which is what a card draws.
    fn listed_json(out: &ToolOutput) -> serde_json::Value {
        listed(out).0["rows"].clone()
    }

    fn row_with(id: &str, status: TaskStatus) -> crate::board::BoardRow {
        let mut row = board_row(Task { id: TaskId::from(id), status, ..sample_task() });
        row.marks.ready = status == TaskStatus::Pending;
        row
    }

    /// The first orientation call has to fit (#3030): by default a row is
    /// the identity and live state a session acts on, and the prose, the
    /// hierarchy pointers and the raw timestamps are what `detail: true`
    /// is for.
    #[tokio::test]
    async fn the_list_returns_compact_rows_by_default() {
        let facade = Arc::new(MockTasksFacade::default());
        facade.rows.lock().push(rich_row());
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        assert_eq!(out.blocks.len(), 1, "a read that withheld nothing is one block: {out:?}");
        let json = listed_json(&out);
        let row = &json[0];
        for (key, value) in [
            ("id", "t-1"),
            ("subject", "Merge peers and workers"),
            ("status", "pending"),
            ("owner", "w-1"),
            ("estimate", "2h"),
        ] {
            assert_eq!(row[key], value, "the compact row carries {key}: {row}");
        }
        assert_eq!(row["waiting"]["kind"], "user", "the wait's state stays: {row}");
        assert_eq!(row["waiting"]["verification"], true, "including the look gate: {row}");
        for omitted in [
            "detail",
            "active_form",
            "attested",
            "parent",
            "parent_subject",
            "updated_ago",
            "worked",
            "artifact",
            "rollup",
            "verify",
            "reserved",
            "links",
            "created_at",
            "updated_at",
        ] {
            assert!(
                row.get(omitted).is_none(),
                "the compact row leaves {omitted} for `detail: true`: {row}",
            );
        }
        assert!(row["waiting"].get("detail").is_none(), "the wait's words are prose too: {row}");
    }

    /// The default read is the rows still in play - pending, in_progress
    /// and waiting - and the completed and canceled rows it left out are
    /// counted rather than dropped silently: the count is the only trace
    /// they leave in a first read. A failed row is not one of the three,
    /// so it stays in the read.
    #[tokio::test]
    async fn the_default_withholds_finished_rows_and_names_the_count() {
        let facade = Arc::new(MockTasksFacade::default());
        facade.rows.lock().extend([
            row_with("t-pending", TaskStatus::Pending),
            row_with("t-running", TaskStatus::InProgress),
            row_with("t-waiting", TaskStatus::Waiting),
            row_with("t-failed", TaskStatus::Failed),
            row_with("t-done", TaskStatus::Completed),
            row_with("t-canceled", TaskStatus::Canceled),
        ]);
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        assert_eq!(
            out.blocks.len(),
            2,
            "the withheld line is its own block, so the rows stay parseable: {out:?}",
        );
        let (json, tail) = listed(&out);
        let ids: Vec<&str> = json["rows"]
            .as_array()
            .expect("the rows")
            .iter()
            .map(|row| row["id"].as_str().expect("an id"))
            .collect();
        assert_eq!(
            ids,
            ["t-pending", "t-running", "t-waiting", "t-failed"],
            "the default is the rows still in play: {json}",
        );
        let line = tail.expect("the default names what it withheld");
        for clause in [
            "2 finished rows withheld by status: 1 completed, 1 canceled",
            "status: \"completed\"",
            "status: \"canceled\"",
        ] {
            assert!(line.contains(clause), "the withheld line states ({clause}): {line}");
        }
    }

    /// A stated `status` is the caller naming the rows it wants, so it
    /// overrides the terminal exclusion and leaves nothing withheld.
    #[tokio::test]
    async fn a_status_filter_overrides_the_terminal_exclusion() {
        let facade = Arc::new(MockTasksFacade::default());
        facade.rows.lock().extend([
            row_with("t-pending", TaskStatus::Pending),
            row_with("t-done", TaskStatus::Completed),
        ]);
        let out = List { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "status": "completed" })))
            .await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        assert_eq!(out.blocks.len(), 1, "a stated status withholds nothing: {out:?}");
        let (json, tail) = listed(&out);
        let rows = json["rows"].as_array().expect("the rows");
        assert_eq!(rows[0]["id"], "t-done", "the asked-for status is returned: {json}");
        assert_eq!(rows.len(), 1);
        assert!(tail.is_none(), "nothing was withheld from a stated status: {tail:?}");
    }

    /// The compact row is exactly the promised field set, asserted by
    /// value: a fully populated row, so a projection that quietly drops a
    /// promised field fails here rather than in a caller.
    #[tokio::test]
    async fn the_compact_row_carries_exactly_the_promised_fields() {
        let facade = Arc::new(MockTasksFacade::default());
        facade.rows.lock().push(populated_row());
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        let (json, tail) = listed(&out);
        assert!(tail.is_none(), "nothing was withheld from a pending row: {tail:?}");
        let rows = json["rows"].as_array().expect("the rows");
        let row = &rows[0];
        let mut keys: Vec<&str> =
            row.as_object().expect("a row object").keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "estimate", "evidence", "id", "kind", "marks", "owner", "status", "subject",
                "waiting",
            ],
            "the compact row is exactly the promised set: {row}",
        );
        for (key, value) in [
            ("estimate", serde_json::json!("2h")),
            ("evidence", serde_json::json!("verified")),
            ("id", serde_json::json!("t-1")),
            ("kind", serde_json::json!("finite")),
            ("marks", serde_json::json!(["in_review", "overdue"])),
            ("owner", serde_json::json!("w-1")),
            ("status", serde_json::json!("pending")),
            ("subject", serde_json::json!("Merge peers and workers")),
            (
                "waiting",
                serde_json::json!({
                    "kind": "dependency",
                    "on": "t-9",
                    "verification": false,
                    "parked": true,
                }),
            ),
        ] {
            assert_eq!(row[key], value, "the compact row's {key}: {row}");
        }
        assert!(row.get("detail").is_none(), "the brief is the detail verb's: {row}");
    }

    /// `detail: true` is the whole record, and its promised values are
    /// asserted literally rather than against the projection itself: an
    /// equality against `board_row_to_json` would pass while that
    /// function dropped a field the description promises.
    #[tokio::test]
    async fn detail_returns_each_rows_whole_record() {
        let facade = Arc::new(MockTasksFacade::default());
        facade.rows.lock().push(rich_row());
        let out = List { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "detail": true })))
            .await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        let json = listed_json(&out);
        let row = &json[0];
        for (key, value) in [
            ("parent", serde_json::json!("epic")),
            ("parent_subject", serde_json::json!("the epic")),
            ("detail", serde_json::json!("the long why a sibling reads in full")),
            ("active_form", serde_json::json!("Merging peers and workers")),
            (
                "attested",
                serde_json::json!({"evidence": "looks good", "at": "1970-01-01T00:00:00Z"}),
            ),
            ("updated_ago", serde_json::json!("1m")),
            ("created_at", serde_json::json!("1970-01-01T00:00:00Z")),
            ("updated_at", serde_json::json!("1970-01-01T00:00:00Z")),
        ] {
            assert_eq!(row[key], value, "the whole record carries {key}: {row}");
        }
        assert_eq!(
            row["waiting"]["detail"], "the words the user answers",
            "the wait's own words come back: {row}",
        );
    }

    /// Every list answer is an envelope: what it carries, how much there
    /// is, whether it was cut, and the doors that narrow it. A long board
    /// cannot come back whole by accident.
    #[tokio::test]
    async fn the_list_envelope_says_what_it_holds() {
        let facade = Arc::new(MockTasksFacade::default());
        facade
            .rows
            .lock()
            .extend((0..25).map(|n| row_with(&format!("t-{n}"), TaskStatus::Pending)));
        let out = List { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({})))
            .await;
        let (json, _) = listed(&out);
        assert_eq!(json["returned"], 20, "the default limit: {json}");
        assert_eq!(json["total"], 25, "the filters matched 25: {json}");
        assert_eq!(json["truncated"], true);
        assert!(
            json["narrow"].as_str().is_some_and(|said| said.contains("tasks__get")),
            "the envelope says where the whole row lives: {json}",
        );

        let out = List { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "limit": 5 })))
            .await;
        let (json, _) = listed(&out);
        assert_eq!(json["returned"], 5, "a stated limit is honoured: {json}");
        assert_eq!(json["total"], 25);

        let out = List { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "limit": 500 })))
            .await;
        let (json, _) = listed(&out);
        assert_eq!(json["returned"], 25, "the clamp is above the board, so all 25 ride");
        assert_eq!(json["truncated"], false);
    }

    /// The detail verb reads one row whole: the brief, the facts, the
    /// evidence and the history, which the compact list leaves behind.
    #[tokio::test]
    async fn the_detail_verb_returns_one_row_whole() {
        let facade = Arc::new(MockTasksFacade::default());
        facade.rows.lock().push(populated_row());
        let out =
            Get { facade, slot: lead_slot() }.call(input(serde_json::json!({ "id": "t-1" }))).await;
        assert!(!out.is_error, "get succeeds: {out:?}");
        let json: serde_json::Value = serde_json::from_str(text_of(&out)).expect("the row is json");
        assert_eq!(json["id"], "t-1");
        assert_eq!(json["detail"], "why", "the brief is the detail verb's");
        assert_eq!(json["evidence"]["verified_at"], "1970-01-01T00:00:00Z");
        assert_eq!(
            json["evidence"]["lead_verified"], "read the sibling repo; it holds",
            "the detail carries the lead's verification under its own key",
        );
        assert_eq!(json["history"], serde_json::json!([]));
        assert_eq!(json["history_total"], 0);
        assert_eq!(json["marks"], serde_json::json!(["in_review", "overdue"]));
        // `tasks__get` promises the links and the archive state: the
        // evidence's door and the row's shelf are read from the same answer.
        assert_eq!(json["links"][0]["target"], "https://example.invalid/pull/9");
        assert_eq!(json["links"][0]["kind"], "pr");
        assert_eq!(json["archived_at"], "1970-01-01T00:00:00Z");
    }

    /// A block stored before the write seams dropped blank words reads as
    /// no quote at all - the read boundary normalizes rather than
    /// forwarding a legacy blank.
    #[tokio::test]
    async fn the_detail_read_normalizes_a_stored_blank_quote() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut row = populated_row();
        if let Some(evidence) = row.task.evidence.as_mut() {
            evidence.lead_verified = Some("  \t ".to_owned());
        }
        facade.rows.lock().push(row);
        let out =
            Get { facade, slot: lead_slot() }.call(input(serde_json::json!({ "id": "t-1" }))).await;
        assert!(!out.is_error, "get succeeds: {out:?}");
        let json: serde_json::Value = serde_json::from_str(text_of(&out)).expect("the row is json");
        assert_eq!(
            json["evidence"]["lead_verified"],
            serde_json::Value::Null,
            "a blank stored quote reads as no quote",
        );
    }

    /// The answer verb threads the user's choice and note to the core and
    /// is the lead's door only.
    #[tokio::test]
    async fn answer_threads_the_choice_and_the_note() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Answer { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({
                "id": "t-1",
                "choice": "flag it",
                "note": "until the bench is green",
                "version": 0,
            })))
            .await;
        assert!(!out.is_error, "answer succeeds: {out:?}");
        let calls = facade.answered.lock();
        assert_eq!(calls[0].1, TaskId::from("t-1"));
        assert_eq!(calls[0].2.as_deref(), Some("flag it"), "the choice is the value the tool sent");
        assert_eq!(calls[0].5.as_deref(), Some("until the bench is green"));
    }

    /// The answer door carries one value per kind: text for a
    /// clarification and a teaching (with its lesson), a disposition for
    /// an exception, and the tool refuses when two are sent at once.
    #[tokio::test]
    async fn answer_threads_the_value_the_ask_kind_needs() {
        let facade = Arc::new(MockTasksFacade::default());
        let answer = Answer { facade: facade.clone(), slot: lead_slot() };
        let out = answer
            .call(input(serde_json::json!({
                "id": "t-1",
                "text": "Granite",
                "version": 0,
            })))
            .await;
        assert!(!out.is_error, "a clarification's text succeeds: {out:?}");
        assert_eq!(facade.answered.lock()[0].3.as_deref(), Some("Granite"), "the text rides it");

        let out = answer
            .call(input(serde_json::json!({
                "id": "t-2",
                "disposition": "retry",
                "version": 0,
            })))
            .await;
        assert!(!out.is_error, "an exception's disposition succeeds: {out:?}");
        assert_eq!(facade.answered.lock()[1].4.as_deref(), Some("retry"));

        let out = answer
            .call(input(serde_json::json!({
                "id": "t-3",
                "text": "report the flake",
                "lesson": "never retry a flake twice",
                "version": 0,
            })))
            .await;
        assert!(!out.is_error, "a teaching's lesson succeeds: {out:?}");
        assert_eq!(facade.answered.lock()[2].3.as_deref(), Some("report the flake"));
        assert_eq!(facade.answered.lock()[2].5.as_deref(), Some("never retry a flake twice"));

        let out = answer
            .call(input(serde_json::json!({
                "id": "t-4",
                "choice": "this",
                "text": "that",
                "version": 0,
            })))
            .await;
        assert!(out.is_error, "two values at once is refused");
        assert!(text_of(&out).contains("exactly one"), "{:?}", text_of(&out));
    }

    /// The policy door lists the project's notes and writes one; a write
    /// states both halves.
    #[tokio::test]
    async fn policy_lists_and_writes_with_both_halves() {
        let facade = Arc::new(MockTasksFacade::default());
        let policy = Policy { facade: facade.clone(), slot: lead_slot() };
        let out = policy.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "a bare call lists: {out:?}");
        assert!(text_of(&out).contains("\"notes\""), "the answer is the set: {:?}", text_of(&out));

        let out = policy
            .call(input(serde_json::json!({
                "class": "ci-flake",
                "text": "never retry a flake twice",
            })))
            .await;
        assert!(!out.is_error, "the lead writes a note: {out:?}");
        assert!(text_of(&out).contains("ci-flake"));

        let out = policy.call(input(serde_json::json!({ "class": "ci-flake" }))).await;
        assert!(out.is_error, "half a write is refused");
        assert!(text_of(&out).contains("both"), "{:?}", text_of(&out));
    }

    #[tokio::test]
    async fn a_worker_is_not_offered_the_answer_door() {
        let facade = Arc::new(MockTasksFacade::default());
        let worker = SessionSlot::worker("TestOrg", "myproj", "w-1");
        let out = Answer { facade: facade.clone(), slot: worker }
            .call(input(serde_json::json!({ "id": "t-1", "choice": "flag it", "version": 0 })))
            .await;
        assert!(out.is_error, "a decision is the user's to answer through the lead");
        assert!(facade.answered.lock().is_empty(), "and the facade is never reached");
    }

    /// A versioned update threads the version the caller read down to the
    /// core's own check.
    #[tokio::test]
    async fn update_threads_the_version_it_was_given() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.update_result.lock() = Some(Ok(sample_task()));
        let out = Update { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({
                "id": "t-1",
                "version": 7,
                "active_form": "Fixing the rail",
            })))
            .await;
        assert!(!out.is_error, "update succeeds: {out:?}");
        assert_eq!(facade.updated.lock()[0].2.version, Some(7));
    }

    /// The compact default, the terminal exclusion and the detail opt-in
    /// are shipped text a session reads as it reaches for the tool, so
    /// every lever is named where the caller meets it.
    #[test]
    fn the_list_description_states_the_compact_default_and_the_detail_opt_in() {
        let tool = List { facade: MockTasksFacade::new().into_arc(), slot: lead_slot() };
        for clause in [
            "The default read carries pending, in_progress, waiting and failed rows",
            "a completed or canceled row is history and is left out",
            "the read ends with a line counting what was withheld by status",
            "A `status` filter overrides that default",
            "`detail: true`",
            "`returned`",
            "`truncated`",
            "The default limit is 20",
        ] {
            assert!(
                tool.description().contains(clause),
                "the list description states ({clause}): {}",
                tool.description(),
            );
        }
        let detail = tool.input_schema()["properties"]["detail"]["description"]
            .as_str()
            .expect("the opt-in carries a description")
            .to_owned();
        assert!(detail.contains("whole record"), "the opt-in says what it returns: {detail}");
        assert!(detail.contains("the parent and its subject"), "and names the pointers: {detail}");
    }

    /// The list opens with the date, so a session reading it never does
    /// date arithmetic against bare timestamps.
    #[tokio::test]
    async fn the_list_leads_with_a_dated_line() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        let text = text_of(&out);
        let first = text.lines().next().expect("a first line");
        assert!(first.starts_with("now "), "the list opens with the date: {first}");
        assert!(first.contains('T'), "in RFC3339: {first}");
    }

    #[tokio::test]
    async fn a_listed_row_carries_its_marks_and_evidence_state() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut row = board_row(sample_task());
        row.marks.in_review = true;
        row.marks.overdue = true;
        row.task.evidence = Some(forge_primitives::tasks::Evidence {
            commit: Some("abc123".to_owned()),
            path: None,
            lead_verified: None,
            excerpt: None,
            verified_at: Some(SystemTime::UNIX_EPOCH),
            problem: None,
        });
        facade.rows.lock().push(row);
        let out = List { facade, slot: lead_slot() }.call(input(serde_json::json!({}))).await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        let json = listed_json(&out);
        let marks: Vec<&str> = json[0]["marks"]
            .as_array()
            .expect("marks array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(marks.contains(&"overdue"), "marks name the facts: {marks:?}");
        assert!(marks.contains(&"in_review"), "marks name the facts: {marks:?}");
        assert_eq!(
            json[0]["evidence"], "verified",
            "the list carries the evidence's own state: {json}",
        );
    }

    #[tokio::test]
    async fn ready_narrows_to_rows_nothing_holds() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut held = board_row(sample_task());
        held.task.owner = Some(SessionSlot::lead("TestOrg", "myproj"));
        let mut free = board_row(Task { id: TaskId::from("t-2"), ..sample_task() });
        free.marks.ready = true;
        facade.rows.lock().extend([held, free]);
        let out = List { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "ready": true })))
            .await;
        assert!(!out.is_error, "list succeeds: {out:?}");
        let text = text_of(&out);
        assert!(text.contains("t-2"), "the ready row is listed: {text}");
        assert!(!text.contains("t-1"), "the held row is not: {text}");
        assert_eq!(facade.listed.lock()[0].3, Some(true), "and the filter was threaded");
    }

    #[tokio::test]
    async fn delete_reports_a_missing_id_rather_than_succeeding_silently() {
        // The default mock finds nothing, so this is the not-found path.
        let facade = Arc::new(MockTasksFacade::default());
        let out = Delete { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "nope", "version": 0 })))
            .await;
        assert!(out.is_error, "an unknown id is an error, not a no-op");
    }

    /// The delete refusal says which case it is, the same split the
    /// update makes: a row another project owns is not an absent id.
    #[tokio::test]
    async fn delete_refusals_name_the_case() {
        let facade = Arc::new(MockTasksFacade::default());
        let tool = Delete { facade: facade.clone(), slot: lead_slot() };

        *facade.delete_result.lock() = Some(Err(TasksError::TaskInAnotherProject {
            id: "t-9".to_owned(),
            project: "otherproj".to_owned(),
        }));
        let elsewhere = tool.call(input(serde_json::json!({ "id": "t-9", "version": 0 }))).await;
        assert!(elsewhere.is_error, "another project's row is refused");
        assert_eq!(
            text_of(&elsewhere),
            "task t-9 exists in project otherproj, not in your project",
            "the refusal names the project that owns the row",
        );

        *facade.delete_result.lock() = Some(Err(TasksError::NoSuchTask { id: "t-9".to_owned() }));
        let absent = tool.call(input(serde_json::json!({ "id": "t-9", "version": 0 }))).await;
        assert!(absent.is_error, "an absent id is refused");
        assert_eq!(
            text_of(&absent),
            "no task with id t-9 in your project",
            "an id no project has must not read as another project's row",
        );
    }

    #[test]
    fn tool_names_are_the_task_family() {
        // Assert the base tool names only. Combined with the `forge`
        // server name (proven in mcp::tests::build_forge_server_*) they
        // render as `mcp__forge__tasks__<x>` on the LLM side, which the SDK
        // auto-approve fast-path covers via the `mcp__forge__` prefix
        // (asserted in forge-sdk options.rs).
        let facade = MockTasksFacade::new().into_arc();
        let slot = lead_slot();
        let create = Create { facade: facade.clone(), slot: slot.clone() };
        let update = Update { facade: facade.clone(), slot: slot.clone() };
        let list = List { facade: facade.clone(), slot: slot.clone() };
        let delete = Delete { facade: facade.clone(), slot: slot.clone() };
        let claim = Claim { facade: facade.clone(), slot: slot.clone() };
        let wait = Wait { facade: facade.clone(), slot: slot.clone() };
        let policy = Policy { facade, slot };
        assert_eq!(create.name(), "tasks__create");
        assert_eq!(update.name(), "tasks__update");
        assert_eq!(list.name(), "tasks__list");
        assert_eq!(delete.name(), "tasks__delete");
        assert_eq!(claim.name(), "tasks__claim");
        assert_eq!(policy.name(), "tasks__policy");
        assert_eq!(wait.name(), "tasks__wait");
    }

    #[tokio::test]
    async fn claim_threads_the_stated_id_and_echoes_the_row() {
        let facade = Arc::new(MockTasksFacade::default());
        let mut task = sample_task();
        task.status = TaskStatus::InProgress;
        task.owner = Some(SessionSlot::worker("TestOrg", "myproj", "w-1"));
        *facade.claim_result.lock() = Some(Ok(task));
        let slot = SessionSlot::worker("TestOrg", "myproj", "w-1");
        let out = Claim { facade: facade.clone(), slot }
            .call(input(serde_json::json!({
                "id": "t-1",
                "version": 0
            })))
            .await;
        assert!(!out.is_error, "claim succeeds: {out:?}");
        let calls = facade.claimed.lock();
        assert_eq!(calls[0].1, "t-1", "the stated id reaches the facade");
        let json: serde_json::Value =
            serde_json::from_str(text_of(&out)).expect("the result is the claimed record");
        assert_eq!(json["status"], "in_progress");
        assert_eq!(json["owner"], "w-1");
    }

    #[tokio::test]
    async fn a_claim_with_no_id_is_refused_without_touching_the_facade() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Claim { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({})))
            .await;
        assert!(out.is_error, "a claim names the row it wants");
        assert!(facade.claimed.lock().is_empty(), "invalid input never reaches the facade");
    }

    /// The claim refusal says which case it is, the same split the
    /// update and delete make: a row another project owns is not an
    /// absent id.
    #[tokio::test]
    async fn claim_refusals_name_the_case() {
        let facade = Arc::new(MockTasksFacade::default());
        let tool = Claim { facade: facade.clone(), slot: lead_slot() };

        *facade.claim_result.lock() = Some(Err(TasksError::TaskInAnotherProject {
            id: "t-9".to_owned(),
            project: "otherproj".to_owned(),
        }));
        let elsewhere = tool.call(input(serde_json::json!({ "id": "t-9", "version": 0 }))).await;
        assert!(elsewhere.is_error, "another project's row is refused");
        assert_eq!(
            text_of(&elsewhere),
            "task t-9 exists in project otherproj, not in your project",
            "the refusal names the project that owns the row",
        );

        *facade.claim_result.lock() = Some(Err(TasksError::NoSuchTask { id: "t-9".to_owned() }));
        let absent = tool.call(input(serde_json::json!({ "id": "t-9", "version": 0 }))).await;
        assert!(absent.is_error, "an absent id is refused");
        assert_eq!(
            text_of(&absent),
            "no task with id t-9 in your project",
            "an id no project has must not read as another project's row",
        );
    }

    /// The core's refusal is read by the caller: the seat that holds the
    /// row is named, not hidden behind a generic failure.
    #[tokio::test]
    async fn a_refused_claim_surfaces_the_cores_words() {
        let facade = Arc::new(MockTasksFacade::default());
        *facade.claim_result.lock() =
            Some(Err(TasksError::Refused("w-1 holds this row".to_owned())));
        let out = Claim { facade, slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "version": 0 })))
            .await;
        assert!(out.is_error, "the claim is refused");
        assert!(
            text_of(&out).contains("w-1 holds this row"),
            "and the refusal names the holder: {:?}",
            text_of(&out),
        );
    }

    /// The wait refusal says which case it is: the row or its blocker
    /// naming another project's id is not an absent id.
    #[tokio::test]
    async fn wait_refusals_name_the_case() {
        let facade = Arc::new(MockTasksFacade::default());
        let tool = Wait { facade: facade.clone(), slot: lead_slot() };

        *facade.wait_result.lock() = Some(Err(TasksError::TaskInAnotherProject {
            id: "t-9".to_owned(),
            project: "otherproj".to_owned(),
        }));
        let elsewhere = tool
            .call(input(serde_json::json!({ "id": "t-9", "kind": "resource", "version": 0 })))
            .await;
        assert!(elsewhere.is_error, "another project's row is refused");
        assert_eq!(
            text_of(&elsewhere),
            "task t-9 exists in project otherproj, not in your project",
            "the refusal names the project that owns the row",
        );

        *facade.wait_result.lock() = Some(Err(TasksError::NoSuchTask { id: "t-9".to_owned() }));
        let absent = tool
            .call(input(serde_json::json!({ "id": "t-9", "kind": "resource", "version": 0 })))
            .await;
        assert!(absent.is_error, "an absent id is refused");
        assert_eq!(
            text_of(&absent),
            "no task with id t-9 in your project",
            "an id no project has must not read as another project's row",
        );
    }

    #[tokio::test]
    async fn wait_threads_kind_detail_and_blocker() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Wait { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({
                "id": "t-1",
                "kind": "dependency",
                "detail": "the board read has to land first",
                "on": "t-9",
                "version": 0,
            })))
            .await;
        assert!(!out.is_error, "wait succeeds: {out:?}");
        let calls = facade.waited.lock();
        assert_eq!(calls[0].2, WaitingKind::Dependency);
        assert_eq!(calls[0].3.as_deref(), Some("the board read has to land first"));
        assert_eq!(calls[0].4, Some(TaskId::from("t-9")));
    }

    #[tokio::test]
    async fn a_dependency_wait_without_on_is_refused() {
        let facade = Arc::new(MockTasksFacade::default());
        let out = Wait { facade: facade.clone(), slot: lead_slot() }
            .call(input(serde_json::json!({ "id": "t-1", "kind": "dependency", "version": 0 })))
            .await;
        assert!(out.is_error, "a dependency wait names its blocker");
        assert!(facade.waited.lock().is_empty(), "invalid input never reaches the facade");
    }
}
