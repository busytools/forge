//! `TasksFacade` - the seam between the task MCP tools and workspace
//! state. The production impl ([`ProdTasksFacade`]) resolves the caller's
//! project and drives the direct `Workspace` task methods; the mock
//! records calls for tool tests.
//!
//! Task-list mutations are direct `Workspace` methods (lock the `tasks`
//! mutex + persist), NOT Command-bus dispatches - a task is workspace
//! state, like the cron list beside it, not a session action.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_primitives::tasks::{
    AnswerValue, AskKind, By, Deadline, DefaultIfIgnored, Disposition, Estimate, Evidence,
    LinkKind, Request, Task, TaskId, TaskKind, TaskLink, TaskStatus, Verify, Waiting, WaitingKind,
};

use crate::SessionSlot;
use crate::mcp::caller_context::caller_context;
use crate::store::policy::PolicyNote;
use crate::tasks::{ClaimError, IdError, MoveError};
use crate::workspace::Workspace;

/// Why a `tasks__*` call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TasksError {
    /// The caller couldn't be mapped to a project (transient race, or the
    /// session ended). Shouldn't happen for a live session.
    UnknownCallerProject,
    /// The estimate words do not parse as a duration; named rather than
    /// stored so a caller hears it instead of losing the value.
    BadEstimate(String),
    /// The core refused the move (open children on a close).
    Refused(String),
    /// No task in the caller's project carries that id.
    NoSuchTask { id: String },
    /// The id names a row another project owns. The last two are the
    /// cases a refusal keeps apart: a caller told only "no such task"
    /// takes a typo and another project's row for the same answer.
    TaskInAnotherProject { id: String, project: String },
}

/// One link as a caller states it: the kind is derived from the target's
/// shape when it is not given.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct TaskLinkDraft {
    #[serde(default)]
    pub kind: Option<LinkKind>,
    #[serde(default)]
    pub label: Option<String>,
    pub target: String,
}

impl TaskLinkDraft {
    fn to_link(&self, at: SystemTime) -> TaskLink {
        TaskLink {
            kind: self.kind.unwrap_or_else(|| LinkKind::for_target(&self.target)),
            label: self.label.clone(),
            target: self.target.clone(),
            state: None,
            last_check: None,
            added_at: at,
        }
    }
}

/// What a completion rests on, as a caller states it. The verdict is
/// forge's own: a stated claim always arrives unchecked, so a new claim
/// never inherits the last one's stamp.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct EvidenceDraft {
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub excerpt: Option<String>,
}

impl EvidenceDraft {
    fn to_evidence(&self) -> Evidence {
        let named = |value: &Option<String>| {
            value.as_deref().map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
        };
        Evidence {
            commit: named(&self.commit),
            path: named(&self.path),
            excerpt: self
                .excerpt
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(Evidence::bounded_excerpt),
            verified_at: None,
            problem: None,
        }
    }
}

/// A task as `tasks__create` states it: the fields the caller supplies,
/// before the project, the id and the timestamps are stamped on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct TaskDraft {
    pub subject: String,
    #[serde(default)]
    pub active_form: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// The label of the session holding it, in the caller's project.
    /// Absent is `unclaimed`.
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub estimate: Option<String>,
    /// Finite work or standing work; omitted reads as finite.
    #[serde(default)]
    pub kind: Option<forge_primitives::tasks::TaskKind>,
    /// A standing row's check interval, in the estimate grammar.
    #[serde(default)]
    pub cadence: Option<String>,
    /// Queue order; lower reads first.
    #[serde(default)]
    pub rank: Option<i64>,
    /// The gate the row is minted with; an epic's is never inherited.
    #[serde(default)]
    pub verify: Option<forge_primitives::tasks::Verify>,
    /// The user reserved this pick: the sweep leaves it for them.
    #[serde(default)]
    pub reserved: Option<forge_primitives::tasks::ReservedBy>,
    #[serde(default)]
    pub links: Vec<TaskLinkDraft>,
    /// What the completion will rest on. Stated at create so a row can be
    /// minted with its claim, or later by an update.
    #[serde(default)]
    pub evidence: Option<EvidenceDraft>,
}

/// An ask on the person as `tasks__wait` states it: the kind, the question,
/// and what the kind needs - a decision's options, a teaching's class, a
/// deadline with its named default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AskDraft {
    pub kind: AskKind,
    pub question: String,
    pub options: Vec<String>,
    pub class: Option<String>,
    /// Duration words ("2h", "1d"); `None` means no deadline.
    pub due: Option<String>,
    pub default: Option<DefaultIfIgnored>,
}

/// The tree a `tasks__delete` removed: the named task as it stood, and how
/// many descendants went with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemovedTaskTree {
    pub task: Task,
    pub descendants_removed: usize,
}

/// The fields `tasks__update` may change. An absent field is left alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct TaskPatch {
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub active_form: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub estimate: Option<String>,
    #[serde(default)]
    pub rank: Option<i64>,
    #[serde(default)]
    pub verify: Option<forge_primitives::tasks::Verify>,
    /// Whether the lead parks this wait by design: the chase leaves it
    /// alone rather than nudging it. Applies to a row that is waiting.
    #[serde(default)]
    pub parked: Option<bool>,
    /// Links to attach; a target already present on the row is left as it
    /// is, so the same PR linked twice stays one link.
    #[serde(default)]
    pub links_add: Vec<TaskLinkDraft>,
    /// Links to take off, by exact target.
    #[serde(default)]
    pub links_remove: Vec<String>,
    /// A new completion claim. It arrives unchecked, so restating it
    /// clears any earlier verdict - the gate stamps the next check.
    #[serde(default)]
    pub evidence: Option<EvidenceDraft>,
    /// A deadline for the row's current wait, in the estimate grammar; it
    /// arrives with `default`.
    #[serde(default)]
    pub due: Option<String>,
    /// The named default that runs when `due` passes.
    #[serde(default)]
    pub default: Option<DefaultIfIgnored>,
    /// The version the caller read. A stated version the row is not at
    /// refuses the write, so a lost update is visible.
    #[serde(default)]
    pub version: Option<u64>,
}

impl TaskPatch {
    /// Apply the stated fields to `task`. `org` and `project` name the
    /// caller's project, which is where an owner label is resolved;
    /// `estimate` arrives pre-parsed so an unparseable one is refused
    /// before any field moves.
    fn apply(
        &self,
        task: &mut Task,
        org: &str,
        project: &str,
        estimate: Option<Estimate>,
        at: SystemTime,
    ) {
        if let Some(subject) = &self.subject {
            task.subject.clone_from(subject);
        }
        if let Some(active_form) = &self.active_form {
            task.active_form = Some(active_form.clone());
        }
        if let Some(detail) = &self.detail {
            task.detail = Some(detail.clone());
        }
        if let Some(status) = self.status {
            task.status = status;
        }
        if let Some(owner) = &self.owner {
            task.owner = Some(SessionSlot::new(org, project, owner.clone()));
        }
        if let Some(parent) = &self.parent {
            task.parent = Some(TaskId::from(parent.as_str()));
        }
        if let Some(rank) = self.rank {
            task.rank = Some(rank);
        }
        if let Some(verify) = self.verify {
            task.verify = Some(verify);
        }
        if let Some(parked) = self.parked
            && let Some(wait) = task.waiting_on.as_mut()
        {
            wait.parked = parked;
        }
        for draft in &self.links_add {
            if !task.links.iter().any(|l| l.target == draft.target) {
                task.links.push(draft.to_link(at));
            }
        }
        task.links.retain(|l| !self.links_remove.contains(&l.target));
        if let Some(evidence) = &self.evidence {
            task.evidence = Some(evidence.to_evidence());
        }
        if let Some(estimate) = estimate {
            task.estimate = Some(estimate);
        }
    }
}

/// The refusal for a stated id that names nothing in the caller's
/// project: an id another project owns is named as such, so a caller
/// cannot read "no such task" as proof the row was never there.
fn missing_task_error(ws: &Workspace, project_name: &str, id: &TaskId) -> TasksError {
    let stated = id.as_str().to_owned();
    match ws.task_project_outside(project_name, id) {
        Some(project) => TasksError::TaskInAnotherProject { id: stated, project },
        None => TasksError::NoSuchTask { id: stated },
    }
}

/// The parent a write stores: the row a stated id names when it lands on
/// one, the words as stated when it names nothing (a child may point at a
/// row that is gone), and a refusal naming the rows when it names several.
fn resolve_parent(
    ws: &Workspace,
    project_name: &str,
    stated: Option<&str>,
) -> Result<Option<TaskId>, TasksError> {
    let Some(stated) = stated else { return Ok(None) };
    match ws.resolve_task_id(project_name, &TaskId::from(stated)) {
        Ok(id) => Ok(Some(id)),
        Err(IdError::NotFound) => Ok(Some(TaskId::from(stated))),
        Err(error @ IdError::Ambiguous(_)) => Err(TasksError::Refused(error.to_string())),
    }
}

/// The list's narrowing, one definition for the real path and the mock:
/// `owner` by label, `parent` by id, `status` by state, `kind` by the
/// row's work shape, and `ready` by the board's own mark.
fn narrow_rows(
    rows: Vec<crate::board::BoardRow>,
    owner: Option<&str>,
    parent: Option<&TaskId>,
    status: Option<TaskStatus>,
    kind: Option<forge_primitives::tasks::TaskKind>,
    ready: Option<bool>,
) -> Vec<crate::board::BoardRow> {
    rows.into_iter()
        .filter(|row| {
            owner.is_none_or(|label| row.task.owner.as_ref().is_some_and(|o| o.label() == label))
        })
        .filter(|row| parent.is_none_or(|id| row.task.parent.as_ref() == Some(id)))
        .filter(|row| status.is_none_or(|wanted| row.task.status == wanted))
        .filter(|row| kind.is_none_or(|wanted| row.task.kind == wanted))
        .filter(|row| ready.is_none_or(|wanted| row.marks.ready == wanted))
        .collect()
}

/// The archived row a stated id names: an exact id first, then a unique
/// prefix. `IdError` carries the same two cases the live resolution does.
fn find_archived(
    archived: &[forge_primitives::tasks::Task],
    stated: &TaskId,
) -> Result<forge_primitives::tasks::Task, IdError> {
    if let Some(found) = archived.iter().find(|task| task.id == *stated) {
        return Ok(found.clone());
    }
    if stated.as_str().is_empty() {
        return Err(IdError::NotFound);
    }
    let matched: Vec<&forge_primitives::tasks::Task> =
        archived.iter().filter(|task| task.id.as_str().starts_with(stated.as_str())).collect();
    match matched.as_slice() {
        [found] => Ok((*found).clone()),
        [] => Err(IdError::NotFound),
        several => Err(IdError::Ambiguous(
            several
                .iter()
                .map(|task| crate::tasks::IdCandidate {
                    id: task.id.clone(),
                    subject: task.subject.clone(),
                })
                .collect(),
        )),
    }
}

/// The task tools' view of the workspace. Sync - task-list mutations are
/// direct state writes with no async handler to await.
pub(crate) trait TasksFacade: Send + Sync {
    /// Register a task for the caller's project and return the new
    /// record, with its id and timestamps stamped.
    fn create_task(&self, caller: &SessionSlot, draft: TaskDraft) -> Result<Task, TasksError>;

    /// The caller's project's board rows, narrowed by `owner` (a label),
    /// `parent` (a task id), `status`, `kind` and `ready` when supplied.
    fn list_tasks(
        &self,
        caller: &SessionSlot,
        owner: Option<&str>,
        parent: Option<&TaskId>,
        status: Option<TaskStatus>,
        kind: Option<forge_primitives::tasks::TaskKind>,
        ready: Option<bool>,
    ) -> Vec<crate::board::BoardRow>;

    /// One row, whole: the record, its derived facts and its history; the
    /// archive's rows come back with their `archived_at` set and no
    /// history. Backs `tasks__get`.
    fn get_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
    ) -> Result<crate::board::BoardRow, TasksError>;

    /// Apply `patch` to the task `id` in the caller's project and return
    /// the record as the write left it. A missing id names its case:
    /// [`TasksError::NoSuchTask`] when the project has no such id,
    /// [`TasksError::TaskInAnotherProject`] when another project holds it.
    /// Open to every session in the project.
    fn update_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        patch: TaskPatch,
    ) -> Result<Task, TasksError>;

    /// Remove the task `id` and its descendants within the caller's
    /// project, returning the named task as it stood and how many
    /// descendants went with it. A missing id names its case the way
    /// [`TasksFacade::update_task`] does. `version` is the row as the
    /// caller read it.
    fn delete_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        version: u64,
    ) -> Result<RemovedTaskTree, TasksError>;

    /// Claim `id` for the caller at the version they read. Backs
    /// `tasks__claim`.
    fn claim_task(&self, caller: &SessionSlot, id: &str, version: u64) -> Result<Task, TasksError>;

    /// Put the caller's row `id` into waiting. Backs `tasks__wait`; an ask
    /// on the person arrives as a draft carrying the kind and what the
    /// kind needs. `version` is the row as the caller read it.
    fn wait_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
        ask: Option<AskDraft>,
        version: u64,
    ) -> Result<Task, TasksError>;

    /// Record the person's answer to an ask: the value the kind needed -
    /// a decision's `choice`, a clarification's `text`, an exception's
    /// `disposition` - the note they left, and a teaching's `lesson`.
    /// Backs `tasks__answer`.
    fn answer_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        choice: Option<&str>,
        text: Option<&str>,
        disposition: Option<&str>,
        note: Option<&str>,
        lesson: Option<&str>,
        version: u64,
    ) -> Result<Task, TasksError>;

    /// The caller's project's settled policy notes. Backs `tasks__policy`.
    fn policy_notes(&self, caller: &SessionSlot) -> Vec<PolicyNote>;

    /// Add a policy note directly - the lead's own write, beside the one a
    /// teaching answer lands.
    fn add_policy_note(
        &self,
        caller: &SessionSlot,
        class: &str,
        text: &str,
    ) -> Result<PolicyNote, TasksError>;

    /// The lead completes a look wait with the approval the user gave,
    /// quoted. Backs `tasks__approve`; the core refuses a non-lead.
    fn attest_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        approval: &str,
        version: u64,
    ) -> Result<Task, TasksError>;
}

/// Production facade over `Weak<Workspace>` (weak to avoid a cycle with
/// the MCP server the workspace owns).
pub(crate) struct ProdTasksFacade {
    workspace: Weak<Workspace>,
}

impl ProdTasksFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn TasksFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

impl TasksFacade for ProdTasksFacade {
    fn create_task(&self, caller: &SessionSlot, draft: TaskDraft) -> Result<Task, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        let now = SystemTime::now();
        let estimate = match draft.estimate.as_deref() {
            Some(words) => Some(
                Estimate::parse(words).ok_or_else(|| TasksError::BadEstimate(words.to_owned()))?,
            ),
            None => None,
        };
        let cadence = match draft.cadence.as_deref() {
            Some(words) => Some(
                Estimate::parse(words).ok_or_else(|| TasksError::BadEstimate(words.to_owned()))?,
            ),
            None => None,
        };
        let links = draft.links.iter().map(|draft| draft.to_link(now)).collect();
        // **Only the lead mints a user gate.** The tool descriptions say
        // so; creation is where a worker would otherwise route around it.
        if draft.verify == Some(Verify::User) && caller.label() != crate::LEAD_LABEL {
            return Err(TasksError::Refused(
                "only the project lead mints a user gate; hand the ask to your lead, and it \
                 creates or gates the row"
                    .to_owned(),
            ));
        }
        let mut task = Task {
            id: TaskId::from(uuid::Uuid::new_v4().to_string()),
            project_name: cx.project_name.clone(),
            subject: draft.subject,
            active_form: draft.active_form,
            detail: draft.detail,
            status: draft.status.unwrap_or(TaskStatus::Pending),
            kind: draft.kind.unwrap_or_default(),
            owner: draft
                .owner
                .map(|label| SessionSlot::new(&cx.project_org, &cx.project_name, label)),
            parent: resolve_parent(&ws, &cx.project_name, draft.parent.as_deref())?,
            waiting_on: None,
            request: None,
            estimate,
            cadence,
            rank: draft.rank,
            verify: draft.verify,
            attested: None,
            reserved: draft.reserved,
            links,
            evidence: draft.evidence.as_ref().map(EvidenceDraft::to_evidence),
            version: 0,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        // A row's kind fixes its clock: standing work is checked on a
        // cadence, finite work against an estimate. Neither can be
        // corrected after creation, so a mismatched pair is refused here
        // rather than stored as a row that reads wrong forever.
        match (task.kind, task.cadence.is_some(), task.estimate.is_some()) {
            (TaskKind::Standing, false, _) => {
                return Err(TasksError::Refused(
                    "a standing row needs a `cadence` (30m, 2h, 1d, 1w): it is the interval its \
                     check comes due on"
                        .to_owned(),
                ));
            }
            (TaskKind::Standing, _, true) => {
                return Err(TasksError::Refused(
                    "a standing row is checked on its `cadence`, not an `estimate`; state the \
                     interval instead"
                        .to_owned(),
                ));
            }
            (TaskKind::Finite, true, _) => {
                return Err(TasksError::Refused(
                    "`cadence` is standing work's; a finite row takes an `estimate` and is \
                     chased when it overruns"
                        .to_owned(),
                ));
            }
            _ => {}
        }
        // A row minted straight into `completed` is a completion claim
        // like any other, and goes through the same gate: creation is not
        // a way around the evidence.
        if task.status == TaskStatus::Completed {
            ws.verify_completion_evidence(&cx.project_name, &mut task)
                .map_err(|refused| TasksError::Refused(refused.to_string()))?;
            // Nor a way around the look park: a finished row whose own
            // completion waits on the user lands waiting on that look.
            if task.verify == Some(Verify::User) {
                task.waiting_on = Some(Waiting {
                    kind: Some(WaitingKind::User),
                    detail: None,
                    on: None,
                    verification: true,
                    parked: false,
                    replan: false,
                    due: Some(crate::tasks::default_user_deadline(now)),
                    acknowledged_at: None,
                    snoozed: None,
                    expired_at: None,
                    since: Some(now),
                });
                task.status = TaskStatus::Waiting;
            }
        }
        ws.push_task(task.clone());
        Ok(task)
    }

    fn list_tasks(
        &self,
        caller: &SessionSlot,
        owner: Option<&str>,
        parent: Option<&TaskId>,
        status: Option<TaskStatus>,
        kind: Option<forge_primitives::tasks::TaskKind>,
        ready: Option<bool>,
    ) -> Vec<crate::board::BoardRow> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let Some(cx) = caller_context(&ws, caller) else { return Vec::new() };
        let rows =
            ws.board_rows(&cx.project_name, SystemTime::now(), crate::board::DEFAULT_STALE_SECS);
        // The filter names a row the same way an id does. An id naming
        // nothing, or several, keeps the literal comparison - the empty
        // answer a filter gives a name that is not there.
        let parent = parent.map(|stated| {
            ws.resolve_task_id(&cx.project_name, stated).unwrap_or_else(|_| stated.clone())
        });
        narrow_rows(rows, owner, parent.as_ref(), status, kind, ready)
    }

    fn get_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
    ) -> Result<crate::board::BoardRow, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        let rows =
            ws.board_rows(&cx.project_name, SystemTime::now(), crate::board::DEFAULT_STALE_SECS);
        let live = match ws.resolve_task_id(&cx.project_name, id) {
            Ok(resolved) => rows.into_iter().find(|row| row.task.id == resolved),
            Err(IdError::Ambiguous(candidates)) => {
                return Err(TasksError::Refused(crate::tasks::ambiguous_words(&candidates)));
            }
            Err(IdError::NotFound) => None,
        };
        if let Some(row) = live {
            return Ok(row);
        }
        // The archive: a row that has left the live set is still readable,
        // and reads as what it is - closed, with no live history.
        // **An unreadable archive is not an absent row.** Answering "no such
        // task" for a store that could not be read would tell a caller the
        // record never existed.
        let archived = match ws.archived_tasks_for_project_checked(&cx.project_name) {
            Ok(rows) => rows,
            Err(reason) => {
                return Err(TasksError::Refused(format!(
                    "the task archive could not be read ({reason}), so the row may be there; \
                     the read is retryable"
                )));
            }
        };
        match find_archived(&archived, id) {
            Ok(task) => Ok(crate::board::BoardRow {
                task,
                worked_secs: 0,
                updated_secs_ago: 0,
                owner_live: true,
                wait_secs: None,
                marks: crate::board::Marks::default(),
                rollup: None,
                parent_subject: None,
                history: Vec::new(),
                history_total: 0,
            }),
            Err(error) => Err(match error {
                IdError::NotFound => missing_task_error(&ws, &cx.project_name, id),
                IdError::Ambiguous(candidates) => {
                    TasksError::Refused(crate::tasks::ambiguous_words(&candidates))
                }
            }),
        }
    }

    fn update_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        patch: TaskPatch,
    ) -> Result<Task, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        // The gate is the lead's to mint here as well: an update is the
        // other door creation has, and the descriptions promise one rule.
        if patch.verify == Some(Verify::User) && caller.label() != crate::LEAD_LABEL {
            return Err(TasksError::Refused(
                "only the project lead mints a user gate; hand the ask to your lead, and it \
                 gates the row"
                    .to_owned(),
            ));
        }
        let estimate = match patch.estimate.as_deref() {
            Some(words) => Some(
                Estimate::parse(words).ok_or_else(|| TasksError::BadEstimate(words.to_owned()))?,
            ),
            None => None,
        };
        let mut patch = patch;
        if let Some(parent) = resolve_parent(&ws, &cx.project_name, patch.parent.as_deref())? {
            patch.parent = Some(parent.as_str().to_owned());
        }
        // A deadline arrives whole: a duration with its named default.
        let deadline = match (patch.due.as_deref(), patch.default) {
            (None, None) => None,
            (Some(words), Some(default)) => {
                let Some(period) = Estimate::parse(words) else {
                    return Err(TasksError::BadEstimate(words.to_owned()));
                };
                Some(Deadline {
                    at: SystemTime::now() + std::time::Duration::from_secs(period.secs),
                    default,
                    ran_at: None,
                })
            }
            (Some(_), None) => {
                return Err(TasksError::Refused(
                    "a deadline names its default-if-ignored: escalate, expire or reask".to_owned(),
                ));
            }
            (None, Some(_)) => {
                return Err(TasksError::Refused(
                    "a default-if-ignored needs the `due` it runs at".to_owned(),
                ));
            }
        };
        let at = SystemTime::now();
        let expected = patch.version;
        ws.update_task_at_version(
            &cx.project_name,
            id,
            By::Seat(caller.clone()),
            expected,
            |task| {
                patch.apply(task, &cx.project_org, &cx.project_name, estimate, at);
                if let Some(deadline) = deadline
                    && let Some(wait) = task.waiting_on.as_mut()
                {
                    wait.due = Some(deadline);
                }
            },
        )
        .map_err(|refused| TasksError::Refused(refused.to_string()))?
        .ok_or_else(|| missing_task_error(&ws, &cx.project_name, id))
    }

    fn delete_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        version: u64,
    ) -> Result<RemovedTaskTree, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        // The stated id resolves before anything is removed: the cascade
        // also collects orphans pointing at the id, so "something was
        // removed" would be true for an id that never existed.
        let resolved = match ws.resolve_task_id(&cx.project_name, id) {
            Ok(resolved) => resolved,
            Err(IdError::NotFound) => return Err(missing_task_error(&ws, &cx.project_name, id)),
            Err(error @ IdError::Ambiguous(_)) => {
                return Err(TasksError::Refused(error.to_string()));
            }
        };
        let removed = ws
            .remove_task_tree_at_version(&cx.project_name, &resolved, Some(version))
            .map_err(|refused| TasksError::Refused(refused.to_string()))?;
        // The tree can be gone by the time the removal runs only if
        // another seat removed it between the resolve and the lock.
        let Some(task) = removed.iter().find(|t| t.id == resolved).cloned() else {
            return Err(missing_task_error(&ws, &cx.project_name, id));
        };
        Ok(RemovedTaskTree { task, descendants_removed: removed.len() - 1 })
    }

    fn claim_task(&self, caller: &SessionSlot, id: &str, version: u64) -> Result<Task, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        let stated = TaskId::from(id);
        match ws.claim_task_at_version(&cx.project_name, caller, &stated, Some(version)) {
            Ok(task) => Ok(task),
            Err(ClaimError::NotFound) => Err(missing_task_error(&ws, &cx.project_name, &stated)),
            Err(refused) => Err(TasksError::Refused(refused.to_string())),
        }
    }

    fn wait_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
        ask: Option<AskDraft>,
        version: u64,
    ) -> Result<Task, TasksError> {
        // **The request rides every bulk row it is on**, so its record is
        // bounded where it enters: a question one sentence, its options a
        // few short phrases, its class a slug.
        const MAX_QUESTION: usize = 280;
        const MAX_OPTION: usize = 120;
        const MAX_OPTIONS: usize = 8;
        const MAX_CLASS: usize = 64;
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        // **An ask on the person is the lead's to raise.** The
        // instructions and the wait description both say so; a worker's
        // ask goes through its lead, which owns what the user is shown.
        if (kind == WaitingKind::User || ask.is_some()) && caller.label() != crate::LEAD_LABEL {
            return Err(TasksError::Refused(
                "only the project lead raises an ask on the user; report the question to \
                 your lead and let it put the row to them"
                    .to_owned(),
            ));
        }
        let waited = if let Some(draft) = ask {
            let question = draft.question.trim();
            if question.is_empty() {
                return Err(TasksError::Refused(
                    "an ask needs `question`: the ask in one sentence".to_owned(),
                ));
            }
            if question.chars().count() > MAX_QUESTION {
                return Err(TasksError::Refused(format!(
                    "the question is {} characters; keep it to {MAX_QUESTION}",
                    question.chars().count(),
                )));
            }
            match draft.kind {
                AskKind::Decision => {
                    if draft.options.is_empty() {
                        return Err(TasksError::Refused(
                            "a decision enumerates at least one option; a question with a \
                             free-text answer is a clarification"
                                .to_owned(),
                        ));
                    }
                    if draft.options.len() > MAX_OPTIONS {
                        return Err(TasksError::Refused(format!(
                            "{} options is more than a card can carry; list at most {MAX_OPTIONS}",
                            draft.options.len(),
                        )));
                    }
                    if let Some(over) = draft
                        .options
                        .iter()
                        .find(|o| o.trim().is_empty() || o.chars().count() > MAX_OPTION)
                    {
                        return Err(TasksError::Refused(format!(
                            "every option is a short phrase of at most {MAX_OPTION} characters; \
                             \"{over}\" is not"
                        )));
                    }
                }
                AskKind::Clarification | AskKind::Exception => {
                    if !draft.options.is_empty() {
                        return Err(TasksError::Refused(
                            "only a decision carries options; a clarification is answered in \
                             words and an exception by a disposition"
                                .to_owned(),
                        ));
                    }
                }
                AskKind::Teaching => {
                    if !draft.options.is_empty() {
                        return Err(TasksError::Refused(
                            "only a decision carries options; a teaching is answered with the \
                             lesson"
                                .to_owned(),
                        ));
                    }
                }
            }
            let class = if draft.kind == AskKind::Teaching {
                let Some(class) = draft.class.as_deref().map(str::trim).filter(|c| !c.is_empty())
                else {
                    return Err(TasksError::Refused(
                        "a teaching names its `class`: the recurring trouble the lesson settles"
                            .to_owned(),
                    ));
                };
                if class.chars().count() > MAX_CLASS {
                    return Err(TasksError::Refused(format!(
                        "the class is {} characters; keep it to {MAX_CLASS}",
                        class.chars().count(),
                    )));
                }
                Some(class.to_owned())
            } else {
                if draft.class.is_some() {
                    return Err(TasksError::Refused(
                        "only a teaching carries a `class`".to_owned(),
                    ));
                }
                None
            };
            // **A deadline names the default that runs when it passes**,
            // and there is no approve default: the pair arrives together.
            let due = match (draft.due.as_deref(), draft.default) {
                (None, None) => None,
                (Some(words), Some(default)) => {
                    let Some(period) = Estimate::parse(words) else {
                        return Err(TasksError::BadEstimate(words.to_owned()));
                    };
                    Some(Deadline {
                        at: SystemTime::now() + std::time::Duration::from_secs(period.secs),
                        default,
                        ran_at: None,
                    })
                }
                (Some(_), None) => {
                    return Err(TasksError::Refused(
                        "a deadline names its default-if-ignored: escalate, expire or reask"
                            .to_owned(),
                    ));
                }
                (None, Some(_)) => {
                    return Err(TasksError::Refused(
                        "a default-if-ignored needs the `due` it runs at".to_owned(),
                    ));
                }
            };
            ws.wait_request_task_at_version(
                &cx.project_name,
                id,
                caller,
                detail,
                Request {
                    kind: draft.kind,
                    question: question.to_owned(),
                    options: draft.options,
                    class,
                    answer: None,
                },
                due,
                Some(version),
            )
        } else {
            ws.wait_task_at_version(&cx.project_name, id, caller, kind, detail, on, Some(version))
        };
        match waited {
            Ok(task) => Ok(task),
            Err(MoveError::NotFound) => {
                // The row resolves first, so a row that is there means
                // the blocker is what named nothing.
                let stated = match ws.resolve_task_id(&cx.project_name, id) {
                    Ok(_) => on,
                    Err(_) => Some(id),
                };
                Err(match stated {
                    Some(stated) => missing_task_error(&ws, &cx.project_name, stated),
                    None => TasksError::Refused(MoveError::NotFound.to_string()),
                })
            }
            Err(refused) => Err(TasksError::Refused(refused.to_string())),
        }
    }

    fn answer_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        choice: Option<&str>,
        text: Option<&str>,
        disposition: Option<&str>,
        note: Option<&str>,
        lesson: Option<&str>,
        version: u64,
    ) -> Result<Task, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        // **One door, one value for the kind**: a decision's choice, a
        // clarification's text, an exception's disposition. A teaching's
        // text doubles as the lesson the policy note is written from.
        let value = match (choice, text, disposition) {
            (Some(choice), None, None) => AnswerValue::Choice(choice.trim().to_owned()),
            (None, Some(text), None) => AnswerValue::Text(text.trim().to_owned()),
            (None, None, Some(disposition)) => {
                let Some(disposition) = Disposition::parse(disposition) else {
                    return Err(TasksError::Refused(format!(
                        "a disposition is one of {}; \"{disposition}\" is not",
                        Disposition::ALL.map(Disposition::as_str).join(", "),
                    )));
                };
                AnswerValue::Disposition(disposition)
            }
            _ => {
                return Err(TasksError::Refused(
                    "an answer states exactly one of `choice`, `text` or `disposition`".to_owned(),
                ));
            }
        };
        // The answer is the person's own act, relayed through a session:
        // recording it as that session's move would attribute the value to
        // the worker that typed it.
        match ws.answer_decision_at_version(
            &cx.project_name,
            id,
            By::User,
            value,
            note,
            lesson,
            Some(version),
        ) {
            Ok(Some(task)) => Ok(task),
            Ok(None) => Err(missing_task_error(&ws, &cx.project_name, id)),
            Err(refused) => Err(TasksError::Refused(refused.to_string())),
        }
    }

    fn policy_notes(&self, caller: &SessionSlot) -> Vec<PolicyNote> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let Some(cx) = caller_context(&ws, caller) else { return Vec::new() };
        ws.policy_notes_for_project(&cx.project_name)
    }

    fn add_policy_note(
        &self,
        caller: &SessionSlot,
        class: &str,
        text: &str,
    ) -> Result<PolicyNote, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        if caller.label() != crate::LEAD_LABEL {
            return Err(TasksError::Refused(
                "only the project lead writes a policy note; hand the lesson to your lead"
                    .to_owned(),
            ));
        }
        let class = class.trim();
        let text = text.trim();
        if class.is_empty() || class.chars().count() > 64 {
            return Err(TasksError::Refused(
                "a policy class is a short slug, 1 to 64 characters".to_owned(),
            ));
        }
        if text.is_empty() || text.chars().count() > 1_000 {
            return Err(TasksError::Refused(
                "a policy note is the lesson itself, 1 to 1000 characters".to_owned(),
            ));
        }
        // The lead's own write records its source as any row carrying the
        // class, or the lead's seat when none does.
        ws.settle_policy(&cx.project_name, class, text, &TaskId::from("lead")).ok_or_else(|| {
            TasksError::Refused(
                "the policy note could not be written; this run's store is unavailable".to_owned(),
            )
        })
    }

    fn attest_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        approval: &str,
        version: u64,
    ) -> Result<Task, TasksError> {
        let ws = self.workspace.upgrade().ok_or(TasksError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(TasksError::UnknownCallerProject)?;
        match ws.attest_task_at_version(&cx.project_name, id, caller, approval, Some(version)) {
            Ok(Some(task)) => Ok(task),
            Ok(None) => Err(missing_task_error(&ws, &cx.project_name, id)),
            Err(refused) => Err(TasksError::Refused(refused.to_string())),
        }
    }
}

/// One recorded `create_task` call: caller, draft, the record returned.
#[cfg(test)]
type CreateCall = (SessionSlot, TaskDraft, Task);

/// A canned record for the mock's default result.
#[cfg(test)]
fn mock_task(caller: &SessionSlot) -> Task {
    let now = SystemTime::UNIX_EPOCH;
    Task {
        id: TaskId::from("mock-task-id"),
        project_name: caller.project().to_owned(),
        subject: "mock".to_owned(),
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
        created_at: now,
        updated_at: now,
    }
}

/// The filters one `tasks__list` call was made with.
#[cfg(test)]
type ListCall = (Option<String>, Option<String>, Option<TaskStatus>, Option<bool>);

/// One `tasks__claim` call: the caller and the row it asked for.
#[cfg(test)]
type ClaimCall = (SessionSlot, String);

/// One `tasks__answer` call: the caller, the row, the value parts, and the
/// note.
#[cfg(test)]
type AnswerCall =
    (SessionSlot, TaskId, Option<String>, Option<String>, Option<String>, Option<String>);

/// One `tasks__wait` call: the caller, the row, the wait kind, the detail,
/// the row waited on, and the ask draft when the wait is on the person.
#[cfg(test)]
type WaitCall =
    (SessionSlot, TaskId, WaitingKind, Option<String>, Option<TaskId>, Option<AskDraft>);

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool correctly parses args, resolves the caller, and surfaces
/// facade results/errors - without a real workspace.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockTasksFacade {
    pub created: parking_lot::Mutex<Vec<CreateCall>>,
    pub rows: parking_lot::Mutex<Vec<crate::board::BoardRow>>,
    pub listed: parking_lot::Mutex<Vec<ListCall>>,
    pub updated: parking_lot::Mutex<Vec<(SessionSlot, TaskId, TaskPatch)>>,
    pub update_result: parking_lot::Mutex<Option<Result<Task, TasksError>>>,
    pub deleted: parking_lot::Mutex<Vec<(SessionSlot, TaskId)>>,
    pub delete_result: parking_lot::Mutex<Option<Result<RemovedTaskTree, TasksError>>>,
    pub claimed: parking_lot::Mutex<Vec<ClaimCall>>,
    pub claim_result: parking_lot::Mutex<Option<Result<Task, TasksError>>>,
    pub waited: parking_lot::Mutex<Vec<WaitCall>>,
    pub wait_result: parking_lot::Mutex<Option<Result<Task, TasksError>>>,
    pub answered: parking_lot::Mutex<Vec<AnswerCall>>,
    pub answer_result: parking_lot::Mutex<Option<Result<Task, TasksError>>>,
    pub got: parking_lot::Mutex<Vec<(SessionSlot, TaskId)>>,
    pub attested: parking_lot::Mutex<Vec<(SessionSlot, TaskId, String)>>,
}

#[cfg(test)]
impl MockTasksFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn into_arc(self) -> Arc<dyn TasksFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
impl TasksFacade for MockTasksFacade {
    fn create_task(&self, caller: &SessionSlot, draft: TaskDraft) -> Result<Task, TasksError> {
        let now = SystemTime::UNIX_EPOCH;
        let task = Task {
            id: TaskId::from("mock-task-id"),
            project_name: caller.project().to_owned(),
            subject: draft.subject.clone(),
            active_form: draft.active_form.clone(),
            detail: draft.detail.clone(),
            status: draft.status.unwrap_or(TaskStatus::Pending),
            kind: draft.kind.unwrap_or_default(),
            owner: draft
                .owner
                .clone()
                .map(|label| SessionSlot::new(caller.org(), caller.project(), label)),
            // The mock keeps the parent as stated; resolving it to the row
            // it names is the prod facade's step, so a tool-level test must
            // read a prefix here as unresolved.
            parent: draft.parent.as_deref().map(TaskId::from),
            waiting_on: None,
            request: None,
            estimate: draft.estimate.as_deref().and_then(Estimate::parse),
            cadence: draft.cadence.as_deref().and_then(Estimate::parse),
            rank: draft.rank,
            verify: draft.verify,
            attested: None,
            reserved: draft.reserved,
            links: draft.links.iter().map(|draft| draft.to_link(now)).collect(),
            evidence: draft.evidence.as_ref().map(EvidenceDraft::to_evidence),
            version: 0,
            archived_at: None,
            created_at: now,
            updated_at: now,
        };
        self.created.lock().push((caller.clone(), draft, task.clone()));
        Ok(task)
    }

    fn claim_task(
        &self,
        caller: &SessionSlot,
        id: &str,
        _version: u64,
    ) -> Result<Task, TasksError> {
        self.claimed.lock().push((caller.clone(), id.to_owned()));
        self.claim_result.lock().clone().unwrap_or_else(|| Ok(mock_task(caller)))
    }

    fn wait_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
        ask: Option<AskDraft>,
        _version: u64,
    ) -> Result<Task, TasksError> {
        self.waited.lock().push((caller.clone(), id.clone(), kind, detail, on.cloned(), ask));
        self.wait_result.lock().clone().unwrap_or_else(|| Ok(mock_task(caller)))
    }

    fn answer_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        choice: Option<&str>,
        text: Option<&str>,
        disposition: Option<&str>,
        note: Option<&str>,
        lesson: Option<&str>,
        _version: u64,
    ) -> Result<Task, TasksError> {
        self.answered.lock().push((
            caller.clone(),
            id.clone(),
            choice.map(str::to_owned),
            text.map(str::to_owned),
            disposition.map(str::to_owned),
            lesson.or(note).map(str::to_owned),
        ));
        self.answer_result.lock().clone().unwrap_or_else(|| Ok(mock_task(caller)))
    }

    fn policy_notes(&self, _caller: &SessionSlot) -> Vec<PolicyNote> {
        Vec::new()
    }

    fn add_policy_note(
        &self,
        caller: &SessionSlot,
        class: &str,
        text: &str,
    ) -> Result<PolicyNote, TasksError> {
        Ok(PolicyNote {
            id: "mock-policy".to_owned(),
            project_name: caller.project().to_owned(),
            class: class.to_owned(),
            text: text.to_owned(),
            source_task: "mock".to_owned(),
            at: SystemTime::UNIX_EPOCH,
        })
    }

    fn attest_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        approval: &str,
        _version: u64,
    ) -> Result<Task, TasksError> {
        self.attested.lock().push((caller.clone(), id.clone(), approval.to_owned()));
        let mut task = mock_task(caller);
        task.attested = Some(forge_primitives::tasks::Attestation {
            evidence: approval.to_owned(),
            at: SystemTime::UNIX_EPOCH,
        });
        Ok(task)
    }

    fn list_tasks(
        &self,
        _caller: &SessionSlot,
        owner: Option<&str>,
        parent: Option<&TaskId>,
        status: Option<TaskStatus>,
        kind: Option<forge_primitives::tasks::TaskKind>,
        ready: Option<bool>,
    ) -> Vec<crate::board::BoardRow> {
        self.listed.lock().push((
            owner.map(str::to_owned),
            parent.map(|id| id.as_str().to_owned()),
            status,
            ready,
        ));
        narrow_rows(self.rows.lock().clone(), owner, parent, status, kind, ready)
    }

    fn get_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
    ) -> Result<crate::board::BoardRow, TasksError> {
        self.got.lock().push((caller.clone(), id.clone()));
        self.rows
            .lock()
            .iter()
            .find(|row| row.task.id == *id)
            .cloned()
            .ok_or_else(|| TasksError::NoSuchTask { id: id.as_str().to_owned() })
    }

    fn update_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        patch: TaskPatch,
    ) -> Result<Task, TasksError> {
        self.updated.lock().push((caller.clone(), id.clone(), patch));
        self.update_result
            .lock()
            .clone()
            .unwrap_or_else(|| Err(TasksError::NoSuchTask { id: id.as_str().to_owned() }))
    }

    fn delete_task(
        &self,
        caller: &SessionSlot,
        id: &TaskId,
        _version: u64,
    ) -> Result<RemovedTaskTree, TasksError> {
        self.deleted.lock().push((caller.clone(), id.clone()));
        self.delete_result
            .lock()
            .clone()
            .unwrap_or_else(|| Err(TasksError::NoSuchTask { id: id.as_str().to_owned() }))
    }
}

/// Owner scoping and list filtering over a real `Workspace` (the mock
/// above can't exercise `caller_context`).
#[cfg(test)]
mod prod_facade_tests {
    use super::*;
    use crate::WorkerEntry;
    use forge_primitives::WorkerLiveness;

    fn worker_entry(project: &str, label: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: "review".to_owned(),
            slot: SessionSlot::worker("TestOrg", project, label),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", project),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            cwd: None,
            diagnostic: None,
            kick: None,
        }
    }

    fn seeded_task(id: &str, subject: &str) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: "myproj".to_owned(),
            subject: subject.to_owned(),
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
            evidence: Some(Evidence {
                commit: Some("test-commit".to_owned()),
                path: None,
                excerpt: None,
                verified_at: None,
                problem: None,
            }),
            version: 0,
            archived_at: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn draft(subject: &str, owner: Option<&str>, parent: Option<&str>) -> TaskDraft {
        TaskDraft {
            subject: subject.to_owned(),
            active_form: None,
            detail: None,
            status: None,
            owner: owner.map(str::to_owned),
            parent: parent.map(str::to_owned),
            estimate: None,
            kind: None,
            cadence: None,
            rank: None,
            verify: None,
            reserved: None,
            links: Vec::new(),
            // A claim the stub verifier accepts: the gate's own refusals
            // have their own tests, and every other row here is about a
            // different rule.
            evidence: Some(EvidenceDraft {
                commit: Some("test-commit".to_owned()),
                path: None,
                excerpt: None,
            }),
        }
    }

    fn fixture() -> (Arc<Workspace>, Arc<dyn TasksFacade>, SessionSlot, SessionSlot) {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", "/tmp/b2-myproj");
        ws.seed_test_project("otherproj", "/tmp/b2-otherproj");
        let key =
            ws.list_projects().into_iter().find(|v| v.name == "myproj").expect("seeded view").key;
        ws.record_connected_session("/tmp/b2-myproj", "lead-uuid", None);
        ws.insert_live_worker(&key, worker_entry("myproj", "reviewer"));
        let facade = ProdTasksFacade::from_arc(&ws);
        (
            ws,
            facade,
            SessionSlot::lead("TestOrg", "myproj"),
            SessionSlot::worker("TestOrg", "myproj", "reviewer"),
        )
    }

    /// The descriptions say the user gate and the decision wait are the
    /// lead's; both doors enforce it rather than only saying so.
    #[test]
    fn a_worker_cannot_mint_a_gate_or_raise_a_decision() {
        let (_ws, facade, lead, worker) = fixture();
        let refused = facade
            .create_task(
                &worker,
                TaskDraft { verify: Some(Verify::User), ..draft("gated", None, None) },
            )
            .expect_err("a worker does not mint a user gate");
        assert!(
            matches!(&refused, TasksError::Refused(said) if said.contains("only the project lead")),
            "the refusal says whose door it is: {refused:?}",
        );

        let row = facade
            .create_task(&lead, draft("plain", Some("reviewer"), None))
            .expect("the lead creates the row");
        let refused = facade
            .update_task(
                &worker,
                &row.id,
                TaskPatch { verify: Some(Verify::User), ..TaskPatch::default() },
            )
            .expect_err("an update is not the worker's way in either");
        assert!(
            matches!(&refused, TasksError::Refused(said) if said.contains("only the project lead")),
            "the update door says whose it is: {refused:?}",
        );

        let refused = facade
            .wait_task(
                &worker,
                &row.id,
                WaitingKind::User,
                None,
                None,
                Some(decision_ask("Which way?", &["this", "that"])),
                row.version,
            )
            .expect_err("a worker does not raise a decision on the user");
        assert!(
            matches!(&refused, TasksError::Refused(said) if said.contains("only the project lead")),
            "the wait door says whose it is: {refused:?}",
        );
    }

    /// A decision draft with its options, as the wait door states one.
    fn decision_ask(question: &str, options: &[&str]) -> AskDraft {
        AskDraft {
            kind: AskKind::Decision,
            question: question.to_owned(),
            options: options.iter().map(|option| (*option).to_owned()).collect(),
            class: None,
            due: None,
            default: None,
        }
    }

    /// The request record rides every bulk row it is on, so it is bounded
    /// where it enters: a question one sentence, its options short phrases.
    #[test]
    fn a_decision_record_is_bounded_where_it_enters() {
        let (_ws, facade, lead, _worker) = fixture();
        let row = facade.create_task(&lead, draft("ask", None, None)).expect("create");
        let version = row.version;
        let long_question = "x".repeat(281);
        let refused = facade
            .wait_task(
                &lead,
                &row.id,
                WaitingKind::User,
                None,
                None,
                Some(decision_ask(&long_question, &["this"])),
                version,
            )
            .expect_err("an over-long question is refused");
        assert!(
            matches!(&refused, TasksError::Refused(said) if said.contains("280")),
            "the refusal names the bound: {refused:?}",
        );
        let long_option = "o".repeat(121);
        let refused = facade
            .wait_task(
                &lead,
                &row.id,
                WaitingKind::User,
                None,
                None,
                Some(decision_ask("Which way?", &[long_option.as_str()])),
                version,
            )
            .expect_err("an over-long option is refused");
        assert!(
            matches!(&refused, TasksError::Refused(said) if said.contains("120")),
            "the refusal names the option bound: {refused:?}",
        );
        let refused = facade
            .wait_task(
                &lead,
                &row.id,
                WaitingKind::User,
                None,
                None,
                Some(decision_ask("  ", &["this"])),
                version,
            )
            .expect_err("a decision with no question is refused");
        assert!(matches!(&refused, TasksError::Refused(_)));
        // A decision enumerates options: a free-text question is a
        // clarification, and the door says so.
        let refused = facade
            .wait_task(
                &lead,
                &row.id,
                WaitingKind::User,
                None,
                None,
                Some(decision_ask("Which way?", &[])),
                version,
            )
            .expect_err("a decision with no options is refused");
        assert!(
            matches!(&refused, TasksError::Refused(said) if said.contains("clarification")),
            "the refusal points at the right kind: {refused:?}",
        );
    }

    /// A row minted straight into `completed` with a user gate parks on
    /// the look like any other completion: creation is not a way around
    /// the park either.
    #[test]
    fn a_created_completed_row_with_a_user_gate_parks_on_the_look() {
        let (_ws, facade, lead, _worker) = fixture();
        let task = facade
            .create_task(
                &lead,
                TaskDraft {
                    status: Some(TaskStatus::Completed),
                    verify: Some(Verify::User),
                    ..draft("finished, and owed a look", None, None)
                },
            )
            .expect("the stub evidence checks out");
        assert_eq!(task.status, TaskStatus::Waiting, "the look park is where the completion lands");
        assert!(
            task.waiting_on.as_ref().is_some_and(|wait| wait.verification),
            "and it waits on the look: {:?}",
            task.waiting_on,
        );
        assert!(
            task.evidence.as_ref().is_some_and(|e| e.verified_at.is_some()),
            "with checked evidence"
        );
    }

    #[test]
    fn a_draft_is_stamped_with_the_callers_project_and_id() {
        let (_ws, facade, lead, _worker) = fixture();
        let task = facade
            .create_task(&lead, draft("Merge peers and workers", None, None))
            .expect("create");
        assert_eq!(task.project_name, "myproj", "stamped with the caller's project");
        assert!(!task.id.as_str().is_empty(), "the facade mints the id");
        assert_eq!(task.status, TaskStatus::Pending, "an unstated status is pending");
        assert_eq!(task.owner, None, "an unstated owner is unclaimed");
        assert_eq!(task.created_at, task.updated_at, "a fresh task's stamps agree");
    }

    #[test]
    fn list_narrows_by_owner_and_parent_within_the_project() {
        let (_ws, facade, lead, worker) = fixture();
        let epic = facade.create_task(&lead, draft("epic", None, None)).expect("epic");
        facade
            .create_task(&lead, draft("mine", Some("reviewer"), Some(epic.id.as_str())))
            .expect("child");
        facade.create_task(&lead, draft("unclaimed", None, None)).expect("sibling");

        assert_eq!(
            facade.list_tasks(&lead, None, None, None, None, None).len(),
            3,
            "unfiltered is the whole project"
        );
        let by_owner = facade.list_tasks(&lead, Some("reviewer"), None, None, None, None);
        assert_eq!(by_owner.len(), 1, "owner narrows to that label");
        assert_eq!(by_owner[0].task.subject, "mine");
        let by_parent = facade.list_tasks(&lead, None, Some(&epic.id), None, None, None);
        assert_eq!(by_parent.len(), 1, "parent narrows to that task's children");
        assert_eq!(by_parent[0].task.subject, "mine");
        assert_eq!(
            facade.list_tasks(&worker, Some("reviewer"), None, None, None, None).len(),
            1,
            "a worker's list is the same project's set",
        );
    }

    /// A task whose parent is gone leaves an orphan pointing at an id that
    /// is not in the store. Deleting that id must report nothing removed,
    /// not succeed on the orphan's back - and must not collect the orphan
    /// on the way, which would destroy work under a name nothing owns.
    #[test]
    fn delete_reports_nothing_removed_for_an_id_that_was_never_there() {
        let (_ws, facade, lead, _worker) = fixture();
        facade
            .create_task(&lead, draft("orphan", None, Some("ghost")))
            .expect("a child pointing at a parent that never existed");
        assert_eq!(
            facade.delete_task(&lead, &TaskId::from("ghost"), 0),
            Err(TasksError::NoSuchTask { id: "ghost".to_owned() }),
            "an id that was never there is not a successful delete, even with orphans pointing \
             at it",
        );
        assert_eq!(
            facade.list_tasks(&lead, None, None, None, None, None).len(),
            1,
            "the orphan is not collected under a name nothing owns",
        );
    }

    /// An id another project owns is a different answer from an id no
    /// project has. Both reads refuse, but the refusal has to say which
    /// case it is: a caller told only "no such task" cannot tell a typo
    /// from a row sitting on another project's board.
    #[test]
    fn a_task_in_another_project_is_not_an_unknown_id() {
        let (ws, facade, lead, _worker) = fixture();
        let mut elsewhere = seeded_task("other-1", "another project's row");
        elsewhere.project_name = "otherproj".to_owned();
        ws.seed_test_task(elsewhere);

        assert_eq!(
            facade.update_task(
                &lead,
                &TaskId::from("other-1"),
                TaskPatch { status: Some(TaskStatus::Completed), ..TaskPatch::default() },
            ),
            Err(TasksError::TaskInAnotherProject {
                id: "other-1".to_owned(),
                project: "otherproj".to_owned(),
            }),
            "the update names the project that owns the row",
        );
        assert_eq!(
            facade.delete_task(&lead, &TaskId::from("other-1"), 0),
            Err(TasksError::TaskInAnotherProject {
                id: "other-1".to_owned(),
                project: "otherproj".to_owned(),
            }),
            "the delete names the project that owns the row",
        );
        let rows = ws.tasks_for_project("otherproj");
        assert_eq!(rows.len(), 1, "the other project's row is still there: {rows:?}");
        assert_eq!(
            rows[0].status,
            TaskStatus::Pending,
            "and neither refusal touched the other project's row",
        );
    }

    /// A full id no project holds reads as absent, not as another
    /// project's row - and a PREFIX of a foreign id is absent too: only
    /// the row's own id names the other project, never a leading part of
    /// it.
    #[test]
    fn an_id_no_project_has_reads_as_absent() {
        let (ws, facade, lead, _worker) = fixture();
        let mut elsewhere = seeded_task("other-1", "another project's row");
        elsewhere.project_name = "otherproj".to_owned();
        ws.seed_test_task(elsewhere);

        for stated in ["other", "nope"] {
            assert_eq!(
                facade.update_task(&lead, &TaskId::from(stated), TaskPatch::default()),
                Err(TasksError::NoSuchTask { id: stated.to_owned() }),
                "{stated} names no row in this project",
            );
            assert_eq!(
                facade.delete_task(&lead, &TaskId::from(stated), 0),
                Err(TasksError::NoSuchTask { id: stated.to_owned() }),
                "{stated} names no row in this project",
            );
        }
    }

    /// The claim and wait doors answer the same two cases: an id another
    /// project owns names that project, an id no project has is absent -
    /// including when the foreign id is the blocker a wait states.
    #[test]
    fn claim_and_wait_refusals_name_the_case() {
        let (ws, facade, lead, _worker) = fixture();
        let mut elsewhere = seeded_task("other-1", "another project's row");
        elsewhere.project_name = "otherproj".to_owned();
        ws.seed_test_task(elsewhere);
        let own = facade
            .create_task(&lead, draft("the waiting row", Some("lead"), None))
            .expect("create");

        let foreign = || TasksError::TaskInAnotherProject {
            id: "other-1".to_owned(),
            project: "otherproj".to_owned(),
        };
        assert_eq!(
            facade.claim_task(&lead, "other-1", 0),
            Err(foreign()),
            "a claim naming another project's row says which project holds it",
        );
        assert_eq!(
            facade.wait_task(
                &lead,
                &TaskId::from("other-1"),
                WaitingKind::Resource,
                None,
                None,
                None,
                0,
            ),
            Err(foreign()),
            "the row a wait names is another project's",
        );
        assert_eq!(
            facade.wait_task(
                &lead,
                &own.id,
                WaitingKind::Dependency,
                None,
                Some(&TaskId::from("other-1")),
                None,
                0,
            ),
            Err(foreign()),
            "the blocker a wait names is another project's",
        );

        assert_eq!(
            facade.claim_task(&lead, "other", 0),
            Err(TasksError::NoSuchTask { id: "other".to_owned() }),
            "a prefix of another project's id names no row here",
        );
        assert_eq!(
            facade.claim_task(&lead, "nope", 0),
            Err(TasksError::NoSuchTask { id: "nope".to_owned() }),
        );
        assert_eq!(
            facade.wait_task(
                &lead,
                &TaskId::from("nope"),
                WaitingKind::Resource,
                None,
                None,
                None,
                0,
            ),
            Err(TasksError::NoSuchTask { id: "nope".to_owned() }),
            "a missing row is absent, not another project's",
        );
        assert_eq!(
            facade.wait_task(
                &lead,
                &own.id,
                WaitingKind::Dependency,
                None,
                Some(&TaskId::from("nope")),
                None,
                0,
            ),
            Err(TasksError::NoSuchTask { id: "nope".to_owned() }),
            "a missing blocker is absent, not another project's",
        );
    }

    /// The update hands back the record it wrote, so the tool can echo a
    /// task rather than an id: the moved field, the row's own words, and
    /// the stamp the write just set.
    ///
    /// **The seeded row carries an epoch `updated_at`**, so a record
    /// echoed without the write's own stamp reads as a pass on a fresh
    /// task and fails here.
    /// The update patch's `parked` door (#2423/C): it marks a waiting
    /// row's wait as the user's own deferral, and a row that is not waiting
    /// has no wait to mark, so the patch leaves it alone rather than
    /// inventing one.
    #[test]
    fn an_update_parks_a_wait_and_only_a_wait() {
        let (ws, facade, lead, _worker) = fixture();
        let mut waiting = seeded_task("waiting", "a deferral");
        waiting.status = TaskStatus::Waiting;
        waiting.waiting_on = Some(forge_primitives::tasks::Waiting {
            kind: Some(forge_primitives::tasks::WaitingKind::User),
            detail: None,
            on: None,
            verification: false,
            parked: false,
            replan: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        });
        ws.seed_test_task(waiting);
        ws.seed_test_task(seeded_task("plain", "a row with no wait"));

        let parked = facade
            .update_task(
                &lead,
                &TaskId::from("waiting"),
                TaskPatch { parked: Some(true), ..TaskPatch::default() },
            )
            .expect("the waiting row is there");
        assert!(
            parked.waiting_on.as_ref().is_some_and(|wait| wait.parked),
            "the wait is the user's by design: {parked:?}",
        );

        let untouched = facade
            .update_task(
                &lead,
                &TaskId::from("plain"),
                TaskPatch { parked: Some(true), ..TaskPatch::default() },
            )
            .expect("the plain row is there");
        assert_eq!(untouched.waiting_on, None, "a row with no wait invents none");
    }

    #[test]
    fn update_returns_the_record_it_wrote() {
        let (ws, facade, lead, _worker) = fixture();
        // A child row: completing a ROOT would close and archive it, which
        // is a different behaviour with its own test.
        let mut seeded = seeded_task("t-1", "before");
        seeded.parent = Some(TaskId::from("epic"));
        ws.seed_test_task(seeded);

        let updated = facade
            .update_task(
                &lead,
                &TaskId::from("t-1"),
                TaskPatch { status: Some(TaskStatus::Completed), ..TaskPatch::default() },
            )
            .expect("the task is there to move");

        assert_eq!(updated.id, TaskId::from("t-1"), "the echoed record is the task that moved");
        assert_eq!(updated.subject, "before", "carrying the row's own words");
        assert_eq!(updated.status, TaskStatus::Completed, "and the state it now holds");
        assert!(
            updated.updated_at > SystemTime::UNIX_EPOCH,
            "the write stamps `updated_at`; a record echoed without it keeps the epoch: {:?}",
            updated.updated_at,
        );
        assert_eq!(
            ws.tasks_for_project("myproj")[0].status,
            TaskStatus::Completed,
            "and it is the stored record, not a copy the write left behind",
        );
    }

    /// A stated id that is a unique PREFIX of the row's own uuid - the
    /// row's id without its last character - names the row, and a patch
    /// carrying `detail` and `links_add` together applies both in the one
    /// call. A prefix is the whole recovery the tools promise; an id
    /// corrupted inside is not one, and `the_incidents_corrupted_id_names_no_row`
    /// holds that side.
    #[test]
    fn a_stated_prefix_lands_the_combined_update() {
        let (ws, facade, lead, _worker) = fixture();
        let id = "2b5387e5-24d6-4ae6-a69f-0f1f86f158f7";
        ws.seed_test_task(seeded_task(id, "the row"));
        let stated = &id[..id.len() - 1];
        assert!(
            id.starts_with(stated) && stated != id,
            "the stated id is the row's own prefix, one character shorter",
        );

        let updated = facade
            .update_task(
                &lead,
                &TaskId::from(stated),
                TaskPatch {
                    detail: Some("why".to_owned()),
                    links_add: vec![TaskLinkDraft {
                        kind: Some(LinkKind::Pr),
                        label: Some("PR #2097".to_owned()),
                        target: "https://example.invalid/pull/2097".to_owned(),
                    }],
                    ..TaskPatch::default()
                },
            )
            .expect("a unique prefix of the row's own id still names the row");

        assert_eq!(updated.id, TaskId::from(id), "the prefix resolves to the stored id");
        assert_eq!(updated.detail.as_deref(), Some("why"), "the detail lands with the link");
        assert_eq!(updated.links.len(), 1, "the link lands with the detail");
        assert_eq!(updated.links[0].target, "https://example.invalid/pull/2097");
    }

    /// The id `tasks__update` was actually refused for: the row's uuid
    /// with a character dropped from inside it, which is not a prefix of
    /// the stored id. No prefix resolution can name that row - the id is
    /// corrupted, not short - so the lookup finds nothing and the row
    /// keeps its state.
    #[test]
    fn the_incidents_corrupted_id_names_no_row() {
        let (ws, facade, lead, _worker) = fixture();
        let id = "2b5387e5-24d6-4ae6-a69f-0f1f86f158f7";
        let stated = "2b5387e5-24d6-4ae6-a69f-0f1f86f1587";
        ws.seed_test_task(seeded_task(id, "the row"));
        assert!(!id.starts_with(stated), "the drop is interior, so the stated id is no prefix");

        let updated = facade
            .update_task(
                &lead,
                &TaskId::from(stated),
                TaskPatch {
                    detail: Some("why".to_owned()),
                    links_add: vec![TaskLinkDraft {
                        kind: Some(LinkKind::Pr),
                        label: Some("PR #2097".to_owned()),
                        target: "https://example.invalid/pull/2097".to_owned(),
                    }],
                    ..TaskPatch::default()
                },
            )
            .expect_err("a corrupted id names no row");
        assert_eq!(
            updated,
            TasksError::NoSuchTask { id: stated.to_owned() },
            "an id with a character dropped from inside names no row",
        );

        let stored = ws.tasks_for_project("myproj");
        assert_eq!(stored[0].detail, None, "the row's detail is untouched");
        assert!(stored[0].links.is_empty(), "and no link attaches under a corrupted id");
    }

    /// The delete hands back the tree it removed: the named task as it
    /// stood and how many descendants went with it, so the tool can echo
    /// both rather than a bare id.
    ///
    /// **The store is insertion-ordered and the named task is created
    /// last of its tree on purpose.** `c` (the delete target) is created
    /// after `b`, and `b` is then re-parented under `c`, so the records
    /// removed are `[b, c, d]` - an implementation that answers with "the
    /// first thing removed" instead of the id the call named reads `b`
    /// here, which is reachable through the tool surface alone.
    #[test]
    fn delete_returns_the_tree_it_removed() {
        let (_ws, facade, lead, _worker) = fixture();
        facade.create_task(&lead, draft("sibling", None, None)).expect("sibling");
        let b = facade.create_task(&lead, draft("b", None, None)).expect("b");
        let c = facade.create_task(&lead, draft("c", None, None)).expect("c");
        facade.create_task(&lead, draft("d", None, Some(b.id.as_str()))).expect("d, b's child");
        facade
            .update_task(
                &lead,
                &b.id,
                TaskPatch { parent: Some(c.id.as_str().to_owned()), ..TaskPatch::default() },
            )
            .expect("b is there to move");

        let removed = facade
            .delete_task(&lead, &c.id, 0)
            .expect("the task the call named is there to remove");

        assert_eq!(
            removed.task.id, c.id,
            "the record is the task named by the call, not the first record the cascade removed",
        );
        assert_eq!(removed.task.subject, "c", "as it stood just before removal");
        assert_eq!(removed.descendants_removed, 2, "b and d went with it, and the count says so");
        assert_eq!(
            facade.list_tasks(&lead, None, None, None, None, None).len(),
            1,
            "only the sibling survives"
        );
    }

    /// Delete resolves the stated id the way the other verbs do: a unique
    /// prefix lands on its row, and a prefix two rows share refuses with
    /// both named rather than removing nothing under a "no such row".
    #[test]
    fn delete_resolves_a_stated_id_by_exact_or_unique_prefix() {
        let (ws, facade, lead, _worker) = fixture();
        ws.seed_test_task(seeded_task("7ed0aaaa-0000-4000-8000-000000000001", "fix the rail"));
        ws.seed_test_task(seeded_task("7ed0bbbb-0000-4000-8000-000000000002", "add tests"));

        let said = match facade.delete_task(&lead, &TaskId::from("7ed0"), 0) {
            Err(TasksError::Refused(said)) => said,
            other => panic!("a refusal naming the rows it matched, got {other:?}"),
        };
        assert!(
            said.contains("7ed0aaaa-0000-4000-8000-000000000001")
                && said.contains("7ed0bbbb-0000-4000-8000-000000000002"),
            "the refusal names both rows it matched: {said}",
        );

        let removed = facade
            .delete_task(&lead, &TaskId::from("7ed0bb"), 0)
            .expect("one row carries that prefix");
        assert_eq!(removed.task.id, TaskId::from("7ed0bbbb-0000-4000-8000-000000000002"));
        assert_eq!(removed.task.subject, "add tests", "the prefix removed its own row");
        assert_eq!(
            facade.list_tasks(&lead, None, None, None, None, None).len(),
            1,
            "and left the other row alone",
        );
    }

    /// The look gate reaches the delete door (#2251): a tree holding a row
    /// parked on the user's look is refused by name, the row survives, and
    /// the refusal is the error the caller reads - never an "ok, nothing
    /// removed" that would read as the cascade having worked.
    #[test]
    fn delete_refuses_a_row_waiting_on_the_look() {
        let (_ws, facade, lead, _worker) = fixture();
        let gated = facade.create_task(&lead, draft("gated", None, None)).expect("gated");
        facade
            .update_task(
                &lead,
                &gated.id,
                TaskPatch {
                    verify: Some(forge_primitives::tasks::Verify::User),
                    ..TaskPatch::default()
                },
            )
            .expect("arm the gate");
        facade
            .update_task(
                &lead,
                &gated.id,
                TaskPatch { status: Some(TaskStatus::Completed), ..TaskPatch::default() },
            )
            .expect("the completion lands on the look");

        let version = facade.get_task(&lead, &gated.id).expect("the gated row").task.version;
        let said = match facade.delete_task(&lead, &gated.id, version) {
            Err(TasksError::Refused(said)) => said,
            other => panic!("a refusal naming the row, got {other:?}"),
        };
        assert!(said.contains("gated"), "the refusal names the row: {said}");
        assert_eq!(
            facade.list_tasks(&lead, None, None, None, None, None).len(),
            1,
            "and the row is still there to be looked at",
        );
    }

    /// Minting a row straight into `completed` is a completion claim too,
    /// so it goes through the same gate rather than around it.
    #[test]
    fn a_row_created_completed_is_gated_like_a_move() {
        let (_ws, facade, lead, _worker) = fixture();
        let refused = facade
            .create_task(
                &lead,
                TaskDraft {
                    status: Some(TaskStatus::Completed),
                    evidence: None,
                    ..draft("a finished row", Some("lead"), None)
                },
            )
            .expect_err("a completion with no evidence is refused");
        assert!(
            matches!(&refused, TasksError::Refused(said) if said.contains("evidence")),
            "the refusal names the claim: {refused:?}",
        );
    }

    /// A parent is an id too, and the store holds the row it names: a
    /// child created - or re-parented - under a short form is the epic's
    /// child for every literal comparison the board makes.
    #[test]
    fn a_stated_parent_is_stored_as_the_row_it_names() {
        let (ws, facade, lead, _worker) = fixture();
        let epic = "7ed0aaaa-0000-4000-8000-000000000001";
        ws.seed_test_task(seeded_task(epic, "the epic"));

        let child =
            facade.create_task(&lead, draft("the child", None, Some("7ed0aaaa"))).expect("create");
        assert_eq!(
            child.parent,
            Some(TaskId::from(epic)),
            "the store holds the row the stated prefix named",
        );

        let moved = facade
            .update_task(
                &lead,
                &child.id,
                TaskPatch { parent: Some("7ed0aaaa".to_owned()), ..TaskPatch::default() },
            )
            .expect("the child is there");
        assert_eq!(moved.parent, Some(TaskId::from(epic)), "the patch resolves it the same way");
    }

    /// The other half of the rule: a parent naming nothing stays as
    /// stated - a child may point at a row that is gone - and one naming
    /// several rows refuses with the rows named.
    #[test]
    fn a_parent_naming_nothing_stays_and_naming_several_refuses() {
        let (ws, facade, lead, _worker) = fixture();
        ws.seed_test_task(seeded_task("7ed0aaaa-0000-4000-8000-000000000001", "fix the rail"));
        ws.seed_test_task(seeded_task("7ed0bbbb-0000-4000-8000-000000000002", "add tests"));

        let orphan = facade
            .create_task(&lead, draft("orphan", None, Some("ghost")))
            .expect("a parent naming nothing is stored as stated");
        assert_eq!(orphan.parent, Some(TaskId::from("ghost")));

        let said = match facade.create_task(&lead, draft("ambiguous", None, Some("7ed0"))) {
            Err(TasksError::Refused(said)) => said,
            other => panic!("a parent naming several rows refuses, got {other:?}"),
        };
        assert!(
            said.contains("7ed0aaaa-0000-4000-8000-000000000001")
                && said.contains("7ed0bbbb-0000-4000-8000-000000000002"),
            "the refusal names both rows it matched: {said}",
        );
    }

    /// The list's parent filter names a row the same way a write does, so
    /// narrowing by the short form a caller holds finds the children.
    #[test]
    fn list_narrows_by_a_stated_parent_prefix() {
        let (ws, facade, lead, _worker) = fixture();
        let epic = "7ed0aaaa-0000-4000-8000-000000000001";
        ws.seed_test_task(seeded_task(epic, "the epic"));
        facade.create_task(&lead, draft("child", None, Some(epic))).expect("child");
        facade.create_task(&lead, draft("sibling", None, None)).expect("sibling");

        let kids =
            facade.list_tasks(&lead, None, Some(&TaskId::from("7ed0aaaa")), None, None, None);
        assert_eq!(kids.len(), 1, "the prefix names the parent row: {kids:?}");
        assert_eq!(kids[0].task.subject, "child");
    }

    /// The live shape, at the seam the tools call through: a worker hands
    /// its verify=user row to the look - the gate lands it waiting - and
    /// the lead's completion of it is refused by name, moving the owner
    /// along with the status and all. The live echo of `"status":
    /// "waiting"` under a success was this refusal wearing a success, with
    /// the row's holder a live seat throughout.
    #[test]
    fn a_lead_completing_a_workers_row_that_waits_on_the_look_is_refused_by_name() {
        let (_ws, facade, lead, worker) = fixture();
        let task = facade
            .create_task(
                &lead,
                TaskDraft {
                    verify: Some(forge_primitives::tasks::Verify::User),
                    evidence: Some(EvidenceDraft {
                        commit: Some("test-commit".to_owned()),
                        path: None,
                        excerpt: None,
                    }),
                    ..draft("the parked row", Some("reviewer"), Some("epic"))
                },
            )
            .expect("create");
        let handed = facade
            .update_task(
                &worker,
                &task.id,
                TaskPatch { status: Some(TaskStatus::Completed), ..TaskPatch::default() },
            )
            .expect("the row is there");
        assert_eq!(
            handed.status,
            TaskStatus::Waiting,
            "precondition: the gate hands the worker's row to the look",
        );

        match facade.update_task(
            &lead,
            &task.id,
            TaskPatch {
                owner: Some("lead".to_owned()),
                status: Some(TaskStatus::Completed),
                ..TaskPatch::default()
            },
        ) {
            Err(TasksError::Refused(said)) => {
                assert!(
                    said.contains("waiting on the user's look"),
                    "the refusal names the look, not another move error's words: {said}",
                );
            }
            other => panic!(
                "the lead's completion of a row waiting on the look is refused, got {other:?}"
            ),
        }
    }

    /// The withdraw door at the tool's own seam (#2340): a bare cancel of
    /// a row parked on the user is refused by name, and the reason a
    /// cancel states lands on the row.
    #[test]
    fn a_cancel_through_the_facade_records_the_reason_and_a_bare_one_is_refused() {
        let (_ws, facade, lead, worker) = fixture();
        let task = facade
            .create_task(
                &lead,
                TaskDraft {
                    verify: Some(forge_primitives::tasks::Verify::User),
                    evidence: Some(EvidenceDraft {
                        commit: Some("test-commit".to_owned()),
                        path: None,
                        excerpt: None,
                    }),
                    ..draft("the parked row", Some("reviewer"), Some("epic"))
                },
            )
            .expect("create");
        facade
            .update_task(
                &worker,
                &task.id,
                TaskPatch { status: Some(TaskStatus::Completed), ..TaskPatch::default() },
            )
            .expect("the row is there");

        match facade.update_task(
            &lead,
            &task.id,
            TaskPatch { status: Some(TaskStatus::Canceled), ..TaskPatch::default() },
        ) {
            Err(TasksError::Refused(said)) => assert!(
                said.contains("withdrawing it needs `detail`"),
                "the refusal names the door's requirement: {said}",
            ),
            other => panic!("a bare cancel of a parked row is refused, got {other:?}"),
        }

        let withdrawn = facade
            .update_task(
                &lead,
                &task.id,
                TaskPatch {
                    status: Some(TaskStatus::Canceled),
                    detail: Some("moot: landed via #2296".to_owned()),
                    ..TaskPatch::default()
                },
            )
            .expect("the row is there");
        assert_eq!(withdrawn.status, TaskStatus::Canceled, "the withdraw lands");
        assert_eq!(withdrawn.waiting_on, None, "the ask leaves the waiting set");
        assert_eq!(
            withdrawn.detail.as_deref(),
            Some("moot: landed via #2296"),
            "and the reason lands on the row",
        );
    }

    /// The live call that walked around the gate: clearing a user-bound
    /// row's verify while asking to complete it is refused by name, and
    /// the row keeps its gate - so the two-call path has no first step.
    #[test]
    fn a_clear_of_a_user_verify_is_refused_by_name() {
        let (ws, facade, lead, _worker) = fixture();
        let task = facade
            .create_task(
                &lead,
                TaskDraft {
                    verify: Some(forge_primitives::tasks::Verify::User),
                    ..draft("the gated row", Some("reviewer"), Some("epic"))
                },
            )
            .expect("create");

        let refused = facade.update_task(
            &lead,
            &task.id,
            TaskPatch {
                status: Some(TaskStatus::Completed),
                verify: Some(forge_primitives::tasks::Verify::None),
                ..TaskPatch::default()
            },
        );
        match refused {
            Err(TasksError::Refused(said)) => assert!(
                said.contains("an update cannot take that away"),
                "the refusal says the gate is not the update's to clear: {said}",
            ),
            other => panic!("the clear-and-complete call is refused, got {other:?}"),
        }
        let stored = ws.tasks_for_project("myproj");
        assert_eq!(
            stored[0].verify,
            Some(forge_primitives::tasks::Verify::User),
            "and the row keeps the gate",
        );
    }

    /// An estimate that is not a duration is refused with its own words,
    /// and nothing else in the patch moves: the caller hears the mistake
    /// instead of the value being dropped on the floor.
    #[test]
    fn an_unparseable_estimate_is_refused_and_moves_nothing() {
        let (ws, facade, lead, _worker) = fixture();
        let task = facade.create_task(&lead, draft("before", None, None)).expect("create");
        let refused = facade.update_task(
            &lead,
            &task.id,
            TaskPatch {
                subject: Some("after".to_owned()),
                estimate: Some("soonish".to_owned()),
                ..TaskPatch::default()
            },
        );
        assert_eq!(
            refused,
            Err(TasksError::BadEstimate("soonish".to_owned())),
            "the refusal names the words it could not read",
        );
        assert_eq!(
            ws.tasks_for_project("myproj")[0].subject,
            "before",
            "and no field moved on the refused write",
        );
    }

    #[test]
    fn create_refuses_an_estimate_it_cannot_parse() {
        let (_ws, facade, lead, _worker) = fixture();
        assert_eq!(
            facade.create_task(
                &lead,
                TaskDraft { estimate: Some("tomorrow".to_owned()), ..draft("x", None, None) },
            ),
            Err(TasksError::BadEstimate("tomorrow".to_owned())),
        );
    }

    /// `ready` narrows to rows something can start - the one filter that
    /// leans on the board's derived mark rather than a stored field.
    #[test]
    fn ready_narrows_out_rows_something_holds() {
        let (ws, facade, lead, _worker) = fixture();
        let mut held = seeded_task("t-1", "held");
        held.owner = Some(SessionSlot::lead("TestOrg", "myproj"));
        held.status = TaskStatus::InProgress;
        ws.seed_test_task(held);
        ws.seed_test_task(seeded_task("t-2", "free"));

        let ready = facade.list_tasks(&lead, None, None, None, None, Some(true));
        assert_eq!(ready.len(), 1, "only the free row is ready: {ready:?}");
        assert_eq!(ready[0].task.id, TaskId::from("t-2"));
        assert!(ready[0].marks.ready, "and it carries the mark it was filtered by");
    }

    /// Every field `tasks__update` can state moves, not just the two the
    /// other tests happen to use.
    #[test]
    fn a_patch_moves_every_field_it_can_state() {
        let (ws, facade, lead, _worker) = fixture();
        let task = facade.create_task(&lead, draft("before", None, None)).expect("create");
        facade
            .update_task(
                &lead,
                &task.id,
                TaskPatch {
                    subject: Some("after".to_owned()),
                    active_form: Some("doing".to_owned()),
                    detail: Some("why".to_owned()),
                    status: Some(TaskStatus::Waiting),
                    owner: Some("lead".to_owned()),
                    parent: Some("epic".to_owned()),
                    links_add: vec![TaskLinkDraft {
                        kind: None,
                        label: None,
                        target: "PR #9".to_owned(),
                    }],
                    estimate: Some("2d".to_owned()),
                    rank: Some(3),
                    verify: Some(forge_primitives::tasks::Verify::User),
                    ..TaskPatch::default()
                },
            )
            .expect("the task is there to move");
        let stored = &ws.tasks_for_project("myproj")[0];
        assert_eq!(stored.subject, "after");
        assert_eq!(stored.active_form.as_deref(), Some("doing"));
        assert_eq!(stored.detail.as_deref(), Some("why"));
        assert_eq!(stored.status, TaskStatus::Waiting);
        assert_eq!(stored.owner.as_ref().map(SessionSlot::label), Some("lead"));
        assert_eq!(stored.parent.as_ref().map(TaskId::as_str), Some("epic"));
        assert_eq!(
            stored.links.first().map(|l| l.target.as_str()),
            Some("PR #9"),
            "the stated artifact lands as a link",
        );
        assert_eq!(
            stored.estimate.as_ref().map(|e| e.words.as_str()),
            Some("2d"),
            "and the estimate keeps its words with its seconds parsed",
        );
        assert_eq!(stored.estimate.as_ref().map(|e| e.secs), Some(172_800));
        assert_eq!(stored.rank, Some(3), "rank moves");
        assert_eq!(stored.verify, Some(forge_primitives::tasks::Verify::User), "verify moves");

        // The same target twice stays one link, and remove takes it off.
        facade
            .update_task(
                &lead,
                &task.id,
                TaskPatch {
                    links_add: vec![TaskLinkDraft {
                        kind: None,
                        label: None,
                        target: "PR #9".to_owned(),
                    }],
                    ..TaskPatch::default()
                },
            )
            .expect("the task is there");
        assert_eq!(
            ws.tasks_for_project("myproj")[0].links.len(),
            1,
            "a re-added target stays one link",
        );
        facade
            .update_task(
                &lead,
                &task.id,
                TaskPatch { links_remove: vec!["PR #9".to_owned()], ..TaskPatch::default() },
            )
            .expect("the task is there");
        assert!(
            ws.tasks_for_project("myproj")[0].links.is_empty(),
            "remove takes it off by exact target",
        );
    }

    #[test]
    fn update_moves_a_task_owned_by_another_session() {
        let (ws, facade, lead, worker) = fixture();
        // A child row: a completing root closes and archives (own test).
        let task =
            facade.create_task(&worker, draft("review this", None, Some("epic"))).expect("create");
        facade
            .update_task(
                &lead,
                &task.id,
                TaskPatch {
                    status: Some(TaskStatus::Completed),
                    owner: Some("lead".to_owned()),
                    ..TaskPatch::default()
                },
            )
            .expect("the lead completes a worker's task");
        let stored = ws.tasks_for_project("myproj");
        assert_eq!(stored[0].status, TaskStatus::Completed, "the status moved");
        assert_eq!(
            stored[0].owner.as_ref().map(SessionSlot::label),
            Some("lead"),
            "the owner moved to the lead's label",
        );
    }
}
