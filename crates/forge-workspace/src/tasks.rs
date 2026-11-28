//! The task cluster on [`Workspace`]: the durable task store accessor
//! and its mutation helpers, a second `impl` block beside `crons.rs` so
//! every caller keeps its path.
//!
//! The core polices the moves here, so no caller can talk a row into an
//! impossible state: the verify gate lives in the transition into
//! completed, closing is completing a root, a claim is one atomic write,
//! a cancel of a row waiting on the user carries the reason it records,
//! and every status change lands in the history.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use forge_primitives::tasks::Verify;
use forge_primitives::tasks::{
    Answer, AnswerValue, AskKind, Attestation, By, Deadline, DefaultIfIgnored, Disposition,
    LinkKind, Request, ReservedBy, Task, TaskId, TaskStatus, TaskTransition, Waiting, WaitingKind,
};

use crate::SessionSlot;
use crate::protocol::RankMove;
use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;

/// Why a claim was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimError {
    /// No row carries the id.
    NotFound,
    /// The stated id matched more than one row, each named.
    Ambiguous(Vec<IdCandidate>),
    /// The row is held by a live seat, named.
    OwnedByLiveSeat(String),
    /// The row is reserved for the user: the sweep leaves it, and so does
    /// every caller's claim.
    Reserved,
    /// The row is not pending, so there is nothing to start.
    NotPending(TaskStatus),
    /// The row holds a wait: it is in play, not startable.
    Waiting,
    /// The caller read a version the row is no longer at: another writer
    /// got there first.
    VersionMismatch { expected: u64, current: u64 },
    /// The caller already has a row in progress.
    AlreadyWorking,
}

/// The status as a refusal spells it.
fn status_word(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in progress",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Canceled => "canceled",
    }
}

/// Why a move was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveError {
    /// Completing a root row while its children are open; cancel them
    /// first to abandon the epic. The children's own SUBJECTS ride it:
    /// "cancel them first" names nothing a reader can go and do.
    OpenChildren(Vec<String>),
    /// The caller does not own the row it is trying to wait.
    NotOwner,
    /// No row carries the id.
    NotFound,
    /// The stated id matched more than one row, each named.
    Ambiguous(Vec<IdCandidate>),
    /// The user's verdict landed on a row that is not waiting on their
    /// look.
    NotWaitingVerification,
    /// The user answered a row that is not waiting on a question.
    NotWaitingQuestion,
    /// A completion landed on a row waiting on the user's own look: the
    /// look is what completes it.
    WaitingOnLook,
    /// A completion landed on a row already waiting on something else
    /// while the row's own completion waits on the user: the gate would
    /// trade the wait it holds for a look park and leave the status
    /// reading `waiting`, so the move is refused by name instead. The
    /// wait's own words ride it.
    CompletingWhileWaiting(String),
    /// A patch would clear the look gate a row is bound by: the user's
    /// own approve is what clears it.
    ClearingLookGate,
    /// A cancel landed on a row waiting on the user without the reason
    /// the withdraw records: the ask leaves with its why on the row,
    /// never silently.
    WithdrawNeedsReason,
    /// A delete would remove a row waiting on the user: the ask is what is
    /// pending, not the row's life, and the board's answer or approve is
    /// what settles it. The row's SUBJECT rides it so the refusal says
    /// which ask.
    DeletingLook(String),
    /// Closing a ROOT row would archive a row waiting on the user: removal
    /// by close is not a way to settle it either. The row's SUBJECT rides
    /// it, as the delete's does.
    ClosingLook(String),
    /// A delete would remove a row whose decision was answered: the record
    /// of what was asked and what was chosen is the only copy, and removal
    /// is not a way to erase it. The row's SUBJECT rides it.
    DeletingDecision(String),
    /// An attestation came from a session that is not the project's lead:
    /// only the lead may stand behind the user's recorded approval.
    AttestLeadOnly,
    /// An attestation stated no quoted approval, so there is nothing to
    /// stand behind.
    AttestNeedsEvidence,
    /// A reopen named a row carrying no lead attestation: there is no
    /// attested look to strike.
    NotAttested,
    /// A completion claim carries no evidence at all: absence is never
    /// success, so there is nothing for forge to check.
    EvidenceMissing,
    /// The evidence names an artifact the project's repository does not
    /// hold, or the check could not run. The reason is the check's own.
    EvidenceUnverified(String),
    /// A completion moved by the project's own lead failed the artifact
    /// check. The refusal carries the stale-catalogue hint: a session
    /// resumed from before `lead_verified` existed keeps its spawn-time
    /// tool schema, so the field is dropped before forge sees it and the
    /// refusal would otherwise read as a plain missing commit. The reason
    /// is the check's own.
    EvidenceUnverifiedLead(String),
    /// `lead_verified` came from a session that is not the project's
    /// lead: only the lead may vouch for an artifact the gate cannot
    /// check, and a worker cannot stand behind its own work.
    LeadVerificationLeadOnly,
    /// `lead_verified` is present but carries no words; a lead's
    /// verification is what it says was checked.
    LeadVerificationEmpty,
    /// An update named a version the row is not at: another writer got
    /// there first, and the caller is reading a stale record.
    VersionMismatch { expected: u64, current: u64 },
    /// A decision answer named a choice the row's options do not carry.
    NotAnOption(String),
    /// The answer's shape does not fit the ask's kind: a clarification
    /// takes text, an exception a disposition, a teaching its lesson. The
    /// kind's need rides it.
    AnswerShape(String),
    /// The ask's class is already settled by a project policy note. The
    /// class and the note's text ride it.
    PolicySettled { class: String, text: String },
    /// A wait was stated on the user with nothing to answer and no look to
    /// give: neither an ask nor a verification park, it would draw as a
    /// card with no door. Every park on the user is one or the other.
    BareUserWait,
    /// A teaching's policy note could not be stored, so the answer was not
    /// recorded: a success that leaves the lesson on nothing is not one.
    /// The class rides it.
    PolicyWriteFailed(String),
    /// The archive could not be read, so the row it was asked about is
    /// unknown rather than absent. The read's own error rides it.
    ArchiveUnreadable(String),
}

/// How long a terminal tree stays in the live set before the retention
/// archives it. The bulk board hides terminal rows at once; this window is
/// only how long the row stays in the live set and its store, so a
/// just-finished row can be read, linked or reopened without a restore
/// first. A tree under a live epic waits for the epic, which closes it.
pub(crate) const RETENTION_SECS: u64 = 24 * 3_600;

/// How long a connector's check of a link stays fresh. The sweep refreshes
/// the stale ones, and a completion that rests on an older or failed check
/// is refused - a cached verdict is evidence only while it is current.
pub(crate) const CONNECTOR_REFRESH_SECS: u64 = 300;

/// The deadline a wait on the person carries when its asker names none:
/// **every item carries one**, and its named default is escalate - never
/// approve, because consent by inattention is not consent.
pub(crate) const USER_WAIT_DEADLINE_SECS: u64 = 24 * 3_600;

/// The deadline every wait on the person is minted with when the asker
/// names none. The default escalates to the lead, which raises the ask
/// again or withdraws it with a reason.
pub(crate) fn default_user_deadline(at: SystemTime) -> Deadline {
    Deadline {
        at: at + Duration::from_secs(USER_WAIT_DEADLINE_SECS),
        default: DefaultIfIgnored::Escalate,
        ran_at: None,
    }
}

/// Whether a row is parked on the user's own look - the verification wait
/// the gate lands a user-bound completion on. The completion guard reads
/// it: only the user's approve completes such a row. A row merely BOUND
/// by the gate, never parked on it, is work the caller may abandon.
///
/// The park belongs to the row's OWN gate (#2423): a verification wait a
/// row left behind while its gate was inherited from the epic stops
/// counting, so a seat can complete it rather than being refused by a
/// look nobody owes.
fn parked_on_look(task: &Task) -> bool {
    task.verify == Some(Verify::User)
        && task.waiting_on.as_ref().is_some_and(|wait| wait.verification)
}

/// Whether `by` is the project's own lead seat: the one seat that may
/// stand behind an artifact the mechanical check cannot reach. A lead of
/// another project is not this project's lead, so the project name is
/// part of the match.
fn moved_by_project_lead(by: &By, project_name: &str) -> bool {
    matches!(by, By::Seat(slot) if slot.is_lead() && slot.project() == project_name)
}

/// Whether a row presents as waiting on the user: an ask on the user or a
/// verification park. The removal guards and the withdraw door read this
/// set - an ask on the user leaves by the user's own answer or approve,
/// or by a reasoned withdraw. The fleet count and the chase's user nudge
/// derive the same set from the stored fields.
///
/// A verification park is the user's only when the row carries its own
/// gate (#2423): the gate is minted on the row, never inherited from its
/// epic, so a park an inherited gate left behind is not an ask on the
/// user and a seat may move the row.
///
/// A dependency or resource wait is not this set's, however the rows
/// around it are gated: the wait is on the task, not the user, and
/// `verify: user` gates a row's own completion rather than its wait. A
/// target that parks on the look has not ended, so the chain keeps
/// waiting, but the waiting row stays the caller's to move or remove
/// (#2415).
pub(crate) fn parked_on_user(task: &Task) -> bool {
    let Some(wait) = task.waiting_on.as_ref() else { return false };
    if wait.verification {
        return task.verify == Some(Verify::User);
    }
    wait.kind == Some(WaitingKind::User)
}

/// The words a refusal names a wait by: its kind, and the row it waits on
/// when the wait names one.
fn wait_words(wait: Option<&Waiting>) -> String {
    let Some(wait) = wait else { return "a wait".to_owned() };
    let what = match wait.kind {
        Some(WaitingKind::User) => "a question for the user",
        Some(WaitingKind::Dependency) => "a dependency",
        Some(WaitingKind::Resource) => "a resource",
        None => "a wait",
    };
    match &wait.on {
        Some(id) => format!("{what} on {}", id.as_str()),
        None => what.to_owned(),
    }
}

/// Whether the person's deferral still holds: until its time passes, or -
/// for an activity snooze - until the row moves.
pub(crate) fn snooze_active(wait: &Waiting, updated_at: SystemTime, now: SystemTime) -> bool {
    let Some(snooze) = wait.snoozed.as_ref() else { return false };
    match snooze.until {
        forge_primitives::tasks::SnoozeUntil::At(until) => now < until,
        forge_primitives::tasks::SnoozeUntil::Activity => updated_at <= snooze.since,
    }
}

/// What an ask of `kind` needs back, as a refusal states it.
fn answer_shape_need(kind: AskKind) -> String {
    match kind {
        AskKind::Decision => {
            "a decision takes `choice`: one of the options the row asks about".to_owned()
        }
        AskKind::Clarification => {
            "a clarification takes `text`: the fact the ask needs, in the person's words".to_owned()
        }
        AskKind::Exception => format!(
            "an exception takes `disposition`: one of {}",
            Disposition::ALL.map(Disposition::as_str).join(", "),
        ),
        AskKind::Teaching => {
            "a teaching takes `text`: the lesson, which becomes the project's policy note for \
             its class"
                .to_owned()
        }
    }
}

/// An answer value as one line, for a legacy row's detail fallback.
fn value_words(value: &AnswerValue) -> String {
    match value {
        AnswerValue::Choice(choice) => choice.clone(),
        AnswerValue::Text(text) => text.clone(),
        AnswerValue::Disposition(disposition) => disposition.as_str().to_owned(),
    }
}

/// Append the user's words to a row's detail, keeping what was there. A
/// blank append writes nothing - an empty detail is no detail, and a
/// `Some("")` left by a wordless send-back would read as an empty ask.
fn append_words(task: &mut Task, words: &str) {
    let words = words.trim();
    if words.is_empty() {
        return;
    }
    let prior = task.detail.take().unwrap_or_default();
    task.detail =
        Some(if prior.trim().is_empty() { words.to_owned() } else { format!("{prior}\n{words}") });
}

/// One connector read folded onto a link's stored check: the new check and
/// whether the answer differs from the last one. **A read that found the
/// same state, head, CI verdict and problem again moves `at` only** - the
/// check stays fresh, while the row's version and `changed_at` move only
/// for news. Otherwise every live agent's versioned write would be refused
/// every five minutes for a read that discovered nothing, and the chase's
/// new-input signal would re-fire on a steady connector.
fn merge_check(
    prior: Option<&forge_primitives::tasks::LinkCheck>,
    checked: &crate::env::git_diff::PullRequestState,
    now: SystemTime,
) -> (forge_primitives::tasks::LinkCheck, bool) {
    let same = prior.is_some_and(|old| {
        old.state == checked.state
            && old.head == checked.head
            && old.ci == checked.ci
            && old.problem == checked.problem
    });
    (
        forge_primitives::tasks::LinkCheck {
            at: now,
            changed_at: if same { prior.and_then(|old| old.changed_at) } else { Some(now) },
            state: checked.state.clone(),
            head: checked.head.clone(),
            ci: checked.ci.clone(),
            problem: checked.problem.clone(),
        },
        !same,
    )
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no such row"),
            Self::Ambiguous(candidates) => write!(f, "{}", ambiguous_words(candidates)),
            Self::OwnedByLiveSeat(label) => write!(f, "{label} holds this row"),
            Self::Reserved => write!(
                f,
                "this row is reserved for the user; it is their pick to start, not a claimable row"
            ),
            Self::NotPending(status) => {
                write!(f, "this row is {}; only a pending row can be claimed", status_word(*status))
            }
            Self::Waiting => write!(
                f,
                "this row is waiting on something; move it off waiting before claiming it"
            ),
            Self::VersionMismatch { expected, current } => write!(
                f,
                "the row moved since you read it: you named version {expected}, the row is at \
                 {current}"
            ),
            Self::AlreadyWorking => write!(f, "you already have a row in progress"),
        }
    }
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenChildren(open) => {
                // The first few by name, the rest counted: an epic with thirty
                // open children is one line on the board's service line, and a
                // thirty-subject sentence is not a sentence a reader reads.
                let named = open.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
                let rest = open.len().saturating_sub(3);
                let more = if rest == 0 { String::new() } else { format!(" and {rest} more") };
                if open.len() == 1 {
                    write!(
                        f,
                        "1 child is still open ({named}); cancel it first to abandon the epic"
                    )
                } else {
                    write!(
                        f,
                        "{} children are still open ({named}{more}); cancel them first to abandon the epic",
                        open.len()
                    )
                }
            }
            Self::NotOwner => write!(f, "you do not own this row"),
            Self::NotFound => write!(f, "no such row"),
            Self::Ambiguous(candidates) => write!(f, "{}", ambiguous_words(candidates)),
            Self::NotWaitingVerification => write!(f, "this row is not waiting on your look"),
            Self::NotWaitingQuestion => write!(f, "this row is not waiting on a question"),
            Self::WaitingOnLook => write!(
                f,
                "this row is waiting on the user's look; the board's approve completes it"
            ),
            Self::CompletingWhileWaiting(waited) => write!(
                f,
                "this row is waiting on {waited}; completing it here would park it on the \
                 user's look instead of completing it - move it off waiting first (a move to \
                 in_progress drops the wait), then complete it"
            ),
            Self::ClearingLookGate => write!(
                f,
                "this row's completion waits on the user's look; an update cannot take that away, \
                 and the board's approve completes it"
            ),
            Self::WithdrawNeedsReason => write!(
                f,
                "this row is waiting on the user; withdrawing it needs `detail`, the reason the \
                 ask is withdrawn, and a cancel stating none - or only what the row already \
                 says - is refused"
            ),
            Self::DeletingLook(subject) => write!(
                f,
                "\"{subject}\" is waiting on the user; the board's answer or approve settles it, \
                 and deleting the row cannot take the ask away"
            ),
            Self::ClosingLook(subject) => write!(
                f,
                "\"{subject}\" is waiting on the user; the board's answer or approve settles it, \
                 and closing the epic cannot take the ask away"
            ),
            Self::DeletingDecision(subject) => write!(
                f,
                "\"{subject}\" records an answered decision, and that record is the only copy; \
                 the row cannot be deleted - cancel it if the work is over, and the record stays \
                 readable"
            ),
            Self::AttestLeadOnly => {
                write!(f, "only the project lead may attest an approval; this session is a worker")
            }
            Self::AttestNeedsEvidence => write!(
                f,
                "an attestation needs the approval quoted in `approval`, and it must not be empty"
            ),
            Self::NotAttested => {
                write!(f, "this row carries no lead attestation to reopen")
            }
            Self::EvidenceMissing => write!(
                f,
                "this row names no evidence for its completion; state `evidence` with the commit \
                 (sha) or the path the work left, so forge can check it"
            ),
            Self::EvidenceUnverified(reason) => write!(
                f,
                "the completion's evidence did not verify: {reason}. Fix the artifact or the \
                 evidence and move the row again"
            ),
            Self::EvidenceUnverifiedLead(reason) => write!(
                f,
                "the completion's evidence did not verify: {reason}. Fix the artifact or the \
                 evidence and move the row again. If the artifact is outside the project's \
                 repository and you are its lead, a session resumed from before \
                 `lead_verified` existed carries its spawn-time tool schema and drops the \
                 field: respawn the session to pick it up, then state `lead_verified` and \
                 move the row again"
            ),
            Self::LeadVerificationLeadOnly => write!(
                f,
                "only the project's lead may state `lead_verified`; this session is not that \
                 seat, and a worker cannot vouch for its own artifact"
            ),
            Self::LeadVerificationEmpty => write!(
                f,
                "`lead_verified` is the lead's own verification in words, and this one is \
                 empty: state what was checked and where, or omit it and name the artifact"
            ),
            Self::VersionMismatch { expected, current } => write!(
                f,
                "this row is at version {current} and you named {expected}: another write got \
                 there first. Read the row again (tasks__get) and restate the change"
            ),
            Self::NotAnOption(choice) if choice.trim().is_empty() => {
                write!(f, "an answer needs a choice: state the option the user picked")
            }
            Self::NotAnOption(choice) => write!(
                f,
                "\"{choice}\" is not one of the options this row asks about; answer with one of \
                 them, or state the choice the user gave"
            ),
            Self::AnswerShape(need) => write!(f, "{need}"),
            Self::PolicySettled { class, text } => write!(
                f,
                "\"{class}\" is settled by project policy (\"{text}\"), so this ask is not \
                 raised again; cite the note instead"
            ),
            Self::BareUserWait => write!(
                f,
                "a wait on the user states what they answer: pass `ask` with the kind and the \
                 question, or park a finished row on their look. A bare user wait has no door"
            ),
            Self::PolicyWriteFailed(class) => write!(
                f,
                "the project's policy note for class \"{class}\" could not be stored, so the \
                 answer was not recorded and the row still waits. Retry once the store is \
                 writable"
            ),
            Self::ArchiveUnreadable(reason) => write!(
                f,
                "the task archive could not be read ({reason}), so this row is unknown rather \
                 than absent. Retry"
            ),
        }
    }
}

/// One row an ambiguous id could have named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdCandidate {
    pub id: TaskId,
    pub subject: String,
}

/// Why a stated id did not land on exactly one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IdError {
    /// No row carries the id, or any id it prefixes.
    NotFound,
    /// More than one row's id starts with it, each named.
    Ambiguous(Vec<IdCandidate>),
}

/// The rows an ambiguous id matched, as one line: each row's id and what
/// it is, so a caller picks one without going back to the board.
pub(crate) fn ambiguous_words(candidates: &[IdCandidate]) -> String {
    let named = candidates
        .iter()
        .map(|candidate| format!("{} ({})", candidate.id.as_str(), candidate.subject))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} rows match: {named}; use a longer prefix", candidates.len())
}

impl std::fmt::Display for IdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no such row"),
            Self::Ambiguous(candidates) => write!(f, "{}", ambiguous_words(candidates)),
        }
    }
}

impl From<IdError> for ClaimError {
    fn from(error: IdError) -> Self {
        match error {
            IdError::NotFound => Self::NotFound,
            IdError::Ambiguous(candidates) => Self::Ambiguous(candidates),
        }
    }
}

impl From<IdError> for MoveError {
    fn from(error: IdError) -> Self {
        match error {
            IdError::NotFound => Self::NotFound,
            IdError::Ambiguous(candidates) => Self::Ambiguous(candidates),
        }
    }
}

/// Resolve a caller-stated id against a project's live rows: an exact id,
/// or a unique prefix of one, since a caller is often handed the short
/// form a person writes.
fn find_task_index(tasks: &[Task], project_name: &str, stated: &TaskId) -> Result<usize, IdError> {
    let scoped = || tasks.iter().enumerate().filter(|(_, task)| task.project_name == project_name);
    if let Some((index, _)) = scoped().find(|(_, task)| task.id == *stated) {
        return Ok(index);
    }
    // The empty string prefixes every id and names no row.
    if stated.as_str().is_empty() {
        return Err(IdError::NotFound);
    }
    let matched: Vec<usize> = scoped()
        .filter(|(_, task)| task.id.as_str().starts_with(stated.as_str()))
        .map(|(index, _)| index)
        .collect();
    match matched.as_slice() {
        [] => Err(IdError::NotFound),
        [index] => Ok(*index),
        several => Err(IdError::Ambiguous(
            several
                .iter()
                .map(|index| IdCandidate {
                    id: tasks[*index].id.clone(),
                    subject: tasks[*index].subject.clone(),
                })
                .collect(),
        )),
    }
}

/// The verify a row is bound by: the gate it was MINTED with, or none.
/// An epic's `verify: user` governs the epic's own completion, never its
/// children's (#2423) - a child that owes no look is not parked on one
/// because its epic carries the gate.
fn bound_verify(row: &Task) -> Verify {
    row.verify.unwrap_or(Verify::None)
}

/// The verify the row at `index` is bound by.
fn effective_verify(tasks: &[Task], index: usize) -> Verify {
    bound_verify(&tasks[index])
}

impl Workspace {
    /// Lock the task list and apply `f`. Every mutation routes through here
    /// to take the lock, and the mutation site persists what it changed from
    /// inside its own closure - a row at a time, never the whole in-memory
    /// set. A boot read that failed leaves that set EMPTY while the db
    /// handle stays live, so a whole-set write would erase every durable
    /// row the read never loaded.
    pub(crate) fn with_tasks_mut<R>(&self, f: impl FnOnce(&mut Vec<Task>) -> R) -> R {
        let mut tasks = self.tasks.lock();
        f(&mut tasks)
    }

    /// Persist one task row, replacing any prior record for its id. Called
    /// from inside the mutation's lock.
    fn persist_task(&self, task: &Task) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        if let Err(error) = crate::store::tasks::upsert(db, task) {
            tracing::error!(
                target: "forge_workspace::tasks",
                task_id = %task.id.as_str(),
                %error,
                "persisting a task to the store failed; it may be lost on restart",
            );
        }
    }

    /// Delete one task row by project and id. Called from inside the
    /// mutation's lock.
    fn delete_stored_task(&self, project_name: &str, id: &TaskId) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        if let Err(error) = crate::store::tasks::delete(db, project_name, id) {
            tracing::error!(
                target: "forge_workspace::tasks",
                task_id = %id.as_str(),
                %error,
                "deleting a task from the store failed; it may come back on restart",
            );
        }
    }

    /// Resolve a caller-stated id to the row it names: an exact id, or a
    /// unique prefix of one. Backs the delete path, which resolves before
    /// it removes so an id naming nothing still removes nothing.
    pub(crate) fn resolve_task_id(
        &self,
        project_name: &str,
        stated: &TaskId,
    ) -> Result<TaskId, IdError> {
        let tasks = self.tasks.lock();
        let index = find_task_index(&tasks, project_name, stated)?;
        Ok(tasks[index].id.clone())
    }

    /// The project holding a row whose exact id is `stated`, when that row
    /// is outside `project_name`. Backs the refusal that keeps an id
    /// another project owns apart from one no project has; a stated prefix
    /// resolves only inside the caller's own project, so only a full id is
    /// classified here.
    pub(crate) fn task_project_outside(
        &self,
        project_name: &str,
        stated: &TaskId,
    ) -> Option<String> {
        let tasks = self.tasks.lock();
        tasks
            .iter()
            .find(|task| task.id == *stated && task.project_name != project_name)
            .map(|task| task.project_name.clone())
    }

    /// Append a task and persist, recording its creation in the history.
    /// Backs `tasks__create`.
    pub(crate) fn push_task(&self, task: Task) {
        let project_name = task.project_name.clone();
        let recorded = task.clone();
        self.with_tasks_mut(|tasks| {
            self.persist_task(&task);
            tasks.push(task);
        });
        self.record_transition(&project_name, &recorded.id, None, recorded.status, By::System);
        self.announce_tasks_changed(&project_name);
    }

    /// Apply `f` to the task `id` in `project_name`, persist, record the
    /// transition, and return the record as the write left it (`None`
    /// when no such task is there). Open to every session in the project.
    ///
    /// Rules ride on top of `f`, so no caller can forget them: a
    /// transition into `completed` on a row minted verify=user lands in
    /// `waiting` (the verify gate, never inherited from the epic); a
    /// patch cannot take a user-bound row's look gate away - clearing its
    /// verify is refused by name; a completion of a row already waiting on
    /// the user's look is refused by name; a cancel of a row waiting on
    /// the user is the withdraw door - it records the reason the caller
    /// states in `detail`, and a cancel without one is refused (#2340);
    /// a move off `waiting` drops the wait the row was holding, so a
    /// completion of any other waiting row applies and clears - except on
    /// a verify=user row, where a seat's completion is refused by name
    /// while the row waits: the gate would trade its wait for a look park
    /// and answer with the status unchanged (#2414). The user's own move
    /// off waiting to completed keeps that path and parks on the look;
    /// and completing a ROOT row closes it - the whole tree moves to the
    /// archive, refused while any child is still open or while any row in
    /// the tree is parked on the user (#2251).
    pub(crate) fn update_task(
        &self,
        project_name: &str,
        id: &TaskId,
        by: By,
        f: impl FnOnce(&mut Task),
    ) -> Result<Option<Task>, MoveError> {
        self.update_task_at_version(project_name, id, by, None, f)
    }

    /// [`Workspace::update_task`] with the version the caller read. A
    /// stated version the row is no longer at refuses the write, naming
    /// both, so a lost update is visible instead of silently overwriting
    /// another writer's work.
    pub(crate) fn update_task_at_version(
        &self,
        project_name: &str,
        id: &TaskId,
        by: By,
        expected_version: Option<u64>,
        f: impl FnOnce(&mut Task),
    ) -> Result<Option<Task>, MoveError> {
        self.move_task(project_name, id, by, true, expected_version, |task| {
            f(task);
            Ok(())
        })
    }

    /// The one commit path every task move runs through. `gate` is the
    /// verify gate: it fires on a transition into `completed`, and the
    /// user's own approval is the one move that passes it (approving IS
    /// the verification). `f` may refuse before anything moves.
    fn move_task(
        &self,
        project_name: &str,
        id: &TaskId,
        by: By,
        gate: bool,
        expected_version: Option<u64>,
        f: impl FnOnce(&mut Task) -> Result<(), MoveError>,
    ) -> Result<Option<Task>, MoveError> {
        let at = SystemTime::now();
        let outcome = self.with_tasks_mut(|tasks| {
            let index = match find_task_index(tasks, project_name, id) {
                Ok(index) => index,
                // An id naming no row is the caller's own not-found answer;
                // one naming several is a refusal that names them.
                Err(IdError::NotFound) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            if let Some(expected) = expected_version
                && tasks[index].version != expected
            {
                return Err(MoveError::VersionMismatch { expected, current: tasks[index].version });
            }
            let mut next = tasks[index].clone();
            let prior = next.status;
            f(&mut next)?;

            // The look gate is the user's to clear: an update that would
            // clear a row's own gate is refused by name.
            if gate
                && effective_verify(tasks, index) == Verify::User
                && bound_verify(&next) != Verify::User
            {
                return Err(MoveError::ClearingLookGate);
            }
            // The look is what completes a row waiting on it: a
            // completion here is refused, naming the look.
            if next.status == TaskStatus::Completed && parked_on_look(&next) {
                return Err(MoveError::WaitingOnLook);
            }
            // The withdraw door (#2340): a seat cancelling a row waiting
            // on the user records the reason the same call states in
            // `detail`, appended to what the row already said - the ask
            // that goes away keeps its story. A cancel stating nothing is
            // refused, so the user's pending ask is never dropped silently.
            if matches!(by, By::Seat(_))
                && next.status == TaskStatus::Canceled
                && parked_on_user(&tasks[index])
            {
                let reason = next.detail.clone().unwrap_or_default();
                if reason.trim().is_empty() || next.detail == tasks[index].detail {
                    return Err(MoveError::WithdrawNeedsReason);
                }
                next.detail.clone_from(&tasks[index].detail);
                append_words(&mut next, &reason);
            }
            // The evidence gate: a claim of completion rests on an artifact
            // forge checks on this move, and a claim with nothing to check
            // refuses. It fires before the look gate parks a verify=user
            // row, so a park carries verified evidence for the approve
            // that follows.
            if next.status == TaskStatus::Completed {
                self.verify_completion_evidence(project_name, &mut next, &by)?;
            }
            // A move off `waiting` drops the wait the row was holding.
            if prior == TaskStatus::Waiting && next.status != TaskStatus::Waiting {
                next.waiting_on = None;
            }

            if gate
                && next.status == TaskStatus::Completed
                && effective_verify(tasks, index) == Verify::User
            {
                // A row already holding another wait would have that wait
                // traded for a look park, leaving the status reading
                // `waiting`: the caller sees success and no change.
                // Refused by name instead (#2414); the user's own moves
                // keep their path.
                if matches!(by, By::Seat(_)) && prior == TaskStatus::Waiting {
                    return Err(MoveError::CompletingWhileWaiting(wait_words(
                        tasks[index].waiting_on.as_ref(),
                    )));
                }
                next.waiting_on = Some(Waiting {
                    kind: Some(WaitingKind::User),
                    detail: None,
                    on: None,
                    verification: true,
                    parked: false,
                    // A look carries a deadline too, and its default
                    // escalates: a look is never approved by a timer.
                    due: Some(default_user_deadline(at)),
                    acknowledged_at: None,
                    snoozed: None,
                    expired_at: None,
                    since: Some(at),
                });
                next.status = TaskStatus::Waiting;
            }

            let closing = next.parent.is_none() && next.status == TaskStatus::Completed;
            let mut removed: Vec<Task> = Vec::new();
            if closing {
                let open: Vec<String> = tasks
                    .iter()
                    .filter(|t| {
                        t.project_name == project_name
                            && t.parent.as_ref() == Some(&next.id)
                            && !t.status.is_terminal()
                    })
                    .map(|t| t.subject.clone())
                    .collect();
                if !open.is_empty() {
                    return Err(MoveError::OpenChildren(open));
                }
                next.archived_at = Some(at);
                // Collect the whole tree, deepest rows included.
                let mut doomed = vec![next.id.clone()];
                let mut cursor = 0;
                while cursor < doomed.len() {
                    let current = doomed[cursor].clone();
                    cursor += 1;
                    for task in tasks.iter() {
                        if task.project_name == project_name
                            && task.parent.as_ref() == Some(&current)
                            && !doomed.contains(&task.id)
                        {
                            doomed.push(task.id.clone());
                        }
                    }
                }
                // The ask a row is waiting on is not the close's to take
                // either (#2251's class, found reviewing the delete's
                // guard): the archive would settle it by removal. The
                // open-children check above sees direct children only, so
                // this one walks the whole cascade. The moved row is
                // exempt by id: its stale copy in `tasks` is not what
                // closes - `next` is, and the gate refuses a parked
                // completion (the user's own approve clears the wait).
                if let Some(task) = tasks.iter().find(|t| {
                    t.project_name == project_name
                        && t.id != next.id
                        && doomed.contains(&t.id)
                        && parked_on_user(t)
                }) {
                    return Err(MoveError::ClosingLook(task.subject.clone()));
                }
                removed = tasks
                    .iter()
                    .filter(|t| t.project_name == project_name && doomed.contains(&t.id))
                    .cloned()
                    .collect();
                for task in &mut removed {
                    task.archived_at = Some(at);
                    task.version = task.version.saturating_add(1);
                }
                tasks.retain(|t| !(t.project_name == project_name && doomed.contains(&t.id)));
            }

            next.version = next.version.saturating_add(1);
            next.updated_at = at;
            if removed.is_empty() {
                tasks[index] = next.clone();
                self.persist_task(&next);
            } else {
                if let Some(row) = removed.iter_mut().find(|t| t.id == next.id) {
                    *row = next.clone();
                }
                // The close took the tree out of the live set: each row
                // leaves the store with it.
                for task in &removed {
                    self.delete_stored_task(project_name, &task.id);
                }
            }
            Ok(Some((next, prior, removed)))
        })?;

        let Some((task, prior, removed)) = outcome else { return Ok(None) };
        if !removed.is_empty()
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::task_archive::append(db, &removed)
        {
            tracing::error!(
                target: "forge_workspace::tasks",
                %error,
                "archiving a closed tree failed; the rows are gone from the live set",
            );
        }
        if prior != task.status {
            self.record_transition(project_name, &task.id, Some(prior), task.status, by);
        }
        if task.status.is_terminal() {
            self.resolve_wait_on(project_name, &task.id);
        }
        self.announce_tasks_changed(project_name);
        Ok(Some(task))
    }

    /// Check a completion's claim on the move that lands it: the artifact
    /// must exist in the project's repository, and the row keeps the stamp
    /// of that check. A claim naming nothing refuses, and a failed check
    /// refuses with the reason it found.
    ///
    /// `lead_verified` is a claim form of its own: when it carries words,
    /// the project's lead (or the user's own approve) may land the
    /// completion and the mechanical check is skipped - the lead's word
    /// stands for an artifact the gate cannot reach. A worker cannot vouch
    /// for its own artifact.
    pub(crate) fn verify_completion_evidence(
        &self,
        project_name: &str,
        task: &mut Task,
        by: &By,
    ) -> Result<(), MoveError> {
        let Some(evidence) = task.evidence.as_mut() else {
            return Err(MoveError::EvidenceMissing);
        };
        let lead = moved_by_project_lead(by, project_name);
        if evidence.lead_verified.is_some() {
            if evidence.lead_verified.as_deref().is_some_and(|words| words.trim().is_empty()) {
                return Err(MoveError::LeadVerificationEmpty);
            }
            // The user's own approve is the verification its gate waits
            // on, so it needs no seat; the lead's word is the seat's to
            // give.
            if !lead && !matches!(by, By::User) {
                return Err(MoveError::LeadVerificationLeadOnly);
            }
            evidence.verified_at = Some(SystemTime::now());
            evidence.problem = None;
        } else {
            let commit =
                evidence.commit.as_deref().map(str::trim).filter(|value| !value.is_empty());
            let path = evidence.path.as_deref().map(str::trim).filter(|value| !value.is_empty());
            if commit.is_none() && path.is_none() {
                return Err(MoveError::EvidenceMissing);
            }
            // The lead's own failure carries the stale-catalogue hint: a
            // session resumed from before `lead_verified` existed drops
            // the field before forge sees it, and would otherwise read a
            // plain missing-artifact refusal.
            let refused = |reason: String| {
                if lead {
                    MoveError::EvidenceUnverifiedLead(reason)
                } else {
                    MoveError::EvidenceUnverified(reason)
                }
            };
            let repo = self
                .list_projects()
                .into_iter()
                .find(|view| view.name == project_name)
                .map(|view| view.path)
                .ok_or_else(|| {
                    refused(format!(
                        "no project named {project_name} is loaded, so there is no repository to \
                         check the claim against"
                    ))
                })?;
            let verifier = std::sync::Arc::clone(&self.evidence_verifier.read());
            match verifier.verify(&repo, commit, path) {
                Ok(()) => {
                    evidence.verified_at = Some(SystemTime::now());
                    evidence.problem = None;
                }
                Err(reason) => return Err(refused(reason)),
            }
        }
        // A project that opted into a connector also waits on its pull
        // requests: the head and CI the connector reports are the claim's
        // other half, and a check that is stale, failed or red is not a
        // verification. A project with no connector asks for nothing here.
        if self.project_connector(project_name).is_some() {
            let now = SystemTime::now();
            for link in task.links.iter().filter(|link| link.kind == LinkKind::Pr) {
                let fresh = link.last_check.as_ref().filter(|check| {
                    check.problem.is_none()
                        && now.duration_since(check.at).map_or(0, |age| age.as_secs())
                            <= CONNECTOR_REFRESH_SECS
                });
                let Some(check) = fresh else {
                    let why = link
                        .last_check
                        .as_ref()
                        .and_then(|check| check.problem.clone())
                        .unwrap_or_else(|| "it has not been checked recently".to_owned());
                    return Err(MoveError::EvidenceUnverified(format!(
                        "the project's connector cannot vouch for {}: {why}. It refreshes every \
                         five minutes; check the link and move the row again",
                        link.target,
                    )));
                };
                // Absence is never success: a pull request whose CI state
                // is unknown or not yet green cannot vouch for a completion.
                match check.ci.as_deref() {
                    Some("passing") => {}
                    Some("failing") => {
                        return Err(MoveError::EvidenceUnverified(format!(
                            "{} reports failing CI; a red pull request does not complete a row",
                            link.target,
                        )));
                    }
                    Some(elsewhere) => {
                        return Err(MoveError::EvidenceUnverified(format!(
                            "{} reports CI {elsewhere}; a pull request whose CI has not passed \
                             does not complete a row",
                            link.target,
                        )));
                    }
                    None => {
                        return Err(MoveError::EvidenceUnverified(format!(
                            "{} reports no CI verdict; an absent check is never a pass, so the \
                             pull request cannot vouch for this completion",
                            link.target,
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// A project's outward connector, when it opted into one. The
    /// test-only project overlay is read too, the way `list_projects`
    /// reads it, so a test can seed an opted-in project.
    fn project_connector(&self, project_name: &str) -> Option<crate::config::ConnectorKind> {
        let config = self.config_arc();
        if let Some(connector) = config
            .projects
            .iter()
            .find(|project| project.name == project_name)
            .and_then(|project| project.connector)
        {
            return Some(connector);
        }
        #[cfg(any(test, feature = "testing"))]
        {
            return self
                .test_extra_projects
                .lock()
                .iter()
                .find(|project| project.name == project_name)
                .and_then(|project| project.connector);
        }
        #[cfg(not(any(test, feature = "testing")))]
        None
    }

    /// Refresh every opted-in project's pull-request links through its
    /// connector, caching each check on the link with its timestamp.
    ///
    /// A project with no connector contributes nothing: no link is looked
    /// at and no child is spawned, which is what keeps the default board
    /// self-contained. Refreshes land on the row as a versioned write;
    /// `updated_at` is left alone, because a connector's read is not
    /// someone touching the row.
    pub async fn refresh_connectors(&self, now: SystemTime) {
        // The project list is taken and every lock released before the
        // first await: a guard held across a spawn would keep this future
        // off the supervisor's Send bound.
        let opted_in: Vec<(String, std::path::PathBuf, std::collections::HashMap<String, String>)> = {
            let config = self.config_arc();
            #[cfg(any(test, feature = "testing"))]
            let extra = self.test_extra_projects.lock();
            let projects: Box<dyn Iterator<Item = &crate::config::LoadedProject>> = {
                #[cfg(any(test, feature = "testing"))]
                {
                    Box::new(config.projects.iter().chain(extra.iter()))
                }
                #[cfg(not(any(test, feature = "testing")))]
                {
                    Box::new(config.projects.iter())
                }
            };
            projects
                .filter(|project| project.connector.is_some())
                .map(|project| (project.name.clone(), project.path.clone(), project.env.clone()))
                .collect()
        };
        for (project, path, env) in opted_in {
            let mut announced = false;
            for task in self.tasks_for_project(&project) {
                if task.status.is_terminal() {
                    continue;
                }
                let mut links = task.links.clone();
                let mut refreshed = false;
                let mut changed = false;
                for link in links.iter_mut().filter(|link| link.kind == LinkKind::Pr) {
                    let fresh = link.last_check.as_ref().is_some_and(|check| {
                        now.duration_since(check.at).map_or(0, |age| age.as_secs())
                            <= CONNECTOR_REFRESH_SECS
                    });
                    if fresh {
                        continue;
                    }
                    let checked =
                        crate::env::git_diff::check_pull_request(&path, &env, &link.target).await;
                    let (check, differs) = merge_check(link.last_check.as_ref(), &checked, now);
                    link.state.clone_from(&checked.state);
                    link.last_check = Some(check);
                    refreshed = true;
                    if differs {
                        changed = true;
                    }
                }
                if !refreshed {
                    continue;
                }
                let id = task.id.clone();
                let project_name = project.clone();
                self.with_tasks_mut(|tasks| {
                    if let Some(stored) = tasks
                        .iter_mut()
                        .find(|stored| stored.id == id && stored.project_name == project_name)
                    {
                        stored.links = links;
                        if changed {
                            stored.version = stored.version.saturating_add(1);
                        }
                        self.persist_task(stored);
                    }
                });
                if changed {
                    announced = true;
                }
            }
            if announced {
                self.announce_tasks_changed(&project);
            }
        }
    }

    /// The user moves a row's state directly - the drag across the board.
    /// Any wait the row was holding drops with the move, and the verify
    /// gate holds for the user's own drag too: a drag into `completed` on a
    /// verify=user row lands in waiting for a look, not done.
    pub(crate) fn user_move_task(
        &self,
        project_name: &str,
        id: &TaskId,
        to: TaskStatus,
    ) -> Result<Option<Task>, MoveError> {
        self.update_task(project_name, id, By::User, |task| {
            task.waiting_on = None;
            task.status = to;
        })
    }

    /// The lead completes a row waiting on the user's look, recording the
    /// approval the user gave. Only a lead attests, only a look wait is
    /// the look's to clear (a decision is the user's to answer), and the
    /// quoted evidence is required. The row completes as the user's own
    /// approve would, wearing the attestation the board draws.
    /// The unversioned door the tests drive; every production caller names
    /// the version it read.
    #[cfg(test)]
    pub(crate) fn attest_task(
        &self,
        project_name: &str,
        id: &TaskId,
        slot: &SessionSlot,
        evidence: &str,
    ) -> Result<Option<Task>, MoveError> {
        self.attest_task_at_version(project_name, id, slot, evidence, None)
    }

    /// `Workspace::attest_task` with the version the caller read.
    pub(crate) fn attest_task_at_version(
        &self,
        project_name: &str,
        id: &TaskId,
        slot: &SessionSlot,
        evidence: &str,
        expected: Option<u64>,
    ) -> Result<Option<Task>, MoveError> {
        if slot.label() != crate::LEAD_LABEL {
            return Err(MoveError::AttestLeadOnly);
        }
        let quote = evidence.trim();
        if quote.is_empty() {
            return Err(MoveError::AttestNeedsEvidence);
        }
        let at = SystemTime::now();
        self.move_task(project_name, id, By::Seat(slot.clone()), false, expected, |task| {
            if task.status != TaskStatus::Waiting || !parked_on_look(task) {
                return Err(MoveError::NotWaitingVerification);
            }
            task.waiting_on = None;
            task.verify = Some(Verify::None);
            task.attested = Some(Attestation { evidence: quote.to_owned(), at });
            append_words(task, &format!("Lead-attested look, quoted: \"{quote}\""));
            task.status = TaskStatus::Completed;
            Ok(())
        })
    }

    /// The user reserves a row's pick (or releases it): the sweep leaves a
    /// reserved row for them rather than dispatching it.
    pub(crate) fn reserve_task(
        &self,
        project_name: &str,
        id: &TaskId,
        reserved: bool,
    ) -> Result<Option<Task>, MoveError> {
        self.update_task(project_name, id, By::User, |task| {
            task.reserved = reserved.then_some(ReservedBy::User);
        })
    }

    /// The user strikes a lead attestation: the look gate is re-minted
    /// and the row parks on the user again, recorded in the history.
    pub(crate) fn reopen_task(
        &self,
        project_name: &str,
        id: &TaskId,
    ) -> Result<Option<Task>, MoveError> {
        let at = SystemTime::now();
        self.move_task(project_name, id, By::User, false, None, |task| {
            if task.attested.is_none() {
                return Err(MoveError::NotAttested);
            }
            task.attested = None;
            task.verify = Some(Verify::User);
            task.waiting_on = Some(Waiting {
                kind: Some(WaitingKind::User),
                detail: None,
                on: None,
                verification: true,
                parked: false,
                due: Some(default_user_deadline(at)),
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: Some(at),
            });
            task.status = TaskStatus::Waiting;
            append_words(task, "Reopened by the user: the look is owed again.");
            Ok(())
        })
    }

    /// The person marked the ask seen without answering it: the acknowledged
    /// state is theirs, distinct from an answer, and the drive re-raises it
    /// if it stands unresolved too long.
    pub(crate) fn acknowledge_task(
        &self,
        project_name: &str,
        id: &TaskId,
    ) -> Result<Option<Task>, MoveError> {
        let at = SystemTime::now();
        self.move_task(project_name, id, By::User, false, None, |task| {
            let on_user =
                task.waiting_on.as_ref().is_some_and(|wait| wait.kind == Some(WaitingKind::User));
            if task.status != TaskStatus::Waiting || !on_user {
                return Err(MoveError::NotWaitingQuestion);
            }
            if let Some(wait) = task.waiting_on.as_mut() {
                wait.acknowledged_at = Some(at);
            }
            Ok(())
        })
    }

    /// The person deferred the ask: until a time they named, or until the
    /// row sees new activity. A snoozed ask is quiet on the board's chase
    /// and the drive's escalation, and it keeps its record either way.
    pub(crate) fn snooze_task(
        &self,
        project_name: &str,
        id: &TaskId,
        until: forge_primitives::tasks::SnoozeUntil,
    ) -> Result<Option<Task>, MoveError> {
        let at = SystemTime::now();
        // One clock value for the stamp and the row's touch: the snooze's
        // own write must not read as the activity that ends it.
        let outcome = self.with_tasks_mut(|tasks| {
            let index = match find_task_index(tasks, project_name, id) {
                Ok(index) => index,
                Err(IdError::NotFound) => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let on_user = tasks[index]
                .waiting_on
                .as_ref()
                .is_some_and(|wait| wait.kind == Some(WaitingKind::User));
            if tasks[index].status != TaskStatus::Waiting || !on_user {
                return Err(MoveError::NotWaitingQuestion);
            }
            if let Some(wait) = tasks[index].waiting_on.as_mut() {
                wait.snoozed = Some(forge_primitives::tasks::Snooze { until, since: at });
            }
            tasks[index].version = tasks[index].version.saturating_add(1);
            tasks[index].updated_at = at;
            self.persist_task(&tasks[index]);
            Ok(Some(tasks[index].clone()))
        })?;
        if outcome.is_some() {
            self.announce_tasks_changed(project_name);
        }
        Ok(outcome)
    }

    /// Run an ask's expiry default: the row ends canceled with the reason,
    /// and the ask's own record - the wait with `expired_at` and the
    /// request - stays on the row, because the archive keeps the only copy
    /// of what was asked.
    pub(crate) fn expire_ask(
        &self,
        project_name: &str,
        id: &TaskId,
        now: SystemTime,
        reason: &str,
    ) -> Option<Task> {
        let outcome = self.with_tasks_mut(|tasks| {
            let index = tasks
                .iter()
                .position(|task| task.project_name == project_name && task.id == *id)?;
            let prior = tasks[index].status;
            let task = &mut tasks[index];
            if let Some(wait) = task.waiting_on.as_mut() {
                wait.expired_at = Some(now);
            }
            append_words(task, reason);
            task.status = TaskStatus::Canceled;
            task.version = task.version.saturating_add(1);
            task.updated_at = now;
            self.persist_task(task);
            Some((task.clone(), prior))
        })?;
        let (task, prior) = outcome;
        self.record_transition(project_name, &task.id, Some(prior), task.status, By::System);
        self.announce_tasks_changed(project_name);
        Some(task)
    }

    /// Re-arm an ask's deadline one period out (the `reask` default).
    pub(crate) fn rearm_ask(&self, project_name: &str, id: &TaskId, next_at: SystemTime) -> bool {
        let changed = self.with_tasks_mut(|tasks| {
            let Some(task) =
                tasks.iter_mut().find(|task| task.project_name == project_name && task.id == *id)
            else {
                return false;
            };
            let Some(due) = task.waiting_on.as_mut().and_then(|wait| wait.due.as_mut()) else {
                return false;
            };
            due.at = next_at;
            due.ran_at = None;
            task.version = task.version.saturating_add(1);
            self.persist_task(task);
            true
        });
        // A client drawing `due at X` must hear the new deadline.
        if changed {
            self.announce_tasks_changed(project_name);
        }
        changed
    }

    /// Record that an ask's deadline default ran (the `escalate` default,
    /// which keeps the ask waiting).
    pub(crate) fn mark_ask_default_ran(
        &self,
        project_name: &str,
        id: &TaskId,
        now: SystemTime,
    ) -> bool {
        let changed = self.with_tasks_mut(|tasks| {
            let Some(task) =
                tasks.iter_mut().find(|task| task.project_name == project_name && task.id == *id)
            else {
                return false;
            };
            let Some(due) = task.waiting_on.as_mut().and_then(|wait| wait.due.as_mut()) else {
                return false;
            };
            due.ran_at = Some(now);
            task.version = task.version.saturating_add(1);
            self.persist_task(task);
            true
        });
        // The deadline moved to `ran`: a client drawing it must hear that.
        if changed {
            self.announce_tasks_changed(project_name);
        }
        changed
    }

    /// The user's approval of a row waiting on their look: the wait
    /// clears, the verify gate is satisfied, and the row completes - a
    /// root closing its tree like any completion.
    pub(crate) fn approve_task(
        &self,
        project_name: &str,
        id: &TaskId,
    ) -> Result<Option<Task>, MoveError> {
        self.move_task(project_name, id, By::User, false, None, |task| {
            if task.status != TaskStatus::Waiting
                || !task.waiting_on.as_ref().is_some_and(|wait| wait.verification)
            {
                return Err(MoveError::NotWaitingVerification);
            }
            task.waiting_on = None;
            task.verify = Some(Verify::None);
            task.status = TaskStatus::Completed;
            Ok(())
        })
    }

    /// The user sends a verified row back: it returns to its owner in
    /// progress, and the words land in the detail - the reason a row was
    /// sent back is the record, not a counter.
    pub(crate) fn send_back_task(
        &self,
        project_name: &str,
        id: &TaskId,
        words: &str,
    ) -> Result<Option<Task>, MoveError> {
        self.move_task(project_name, id, By::User, false, None, |task| {
            if task.status != TaskStatus::Waiting
                || !task.waiting_on.as_ref().is_some_and(|wait| wait.verification)
            {
                return Err(MoveError::NotWaitingVerification);
            }
            task.waiting_on = None;
            task.status = TaskStatus::InProgress;
            append_words(task, words);
            Ok(())
        })
    }

    /// Answer a row waiting on an ask: the value the kind needed, when, and
    /// the note the person left. The record is the row's own and nothing
    /// else is written to it; the row resumes in progress. A teaching's
    /// answer also writes the project policy note its class is settled by.
    pub(crate) fn answer_decision(
        &self,
        project_name: &str,
        id: &TaskId,
        by: By,
        value: AnswerValue,
        note: Option<&str>,
        lesson: Option<&str>,
    ) -> Result<Option<Task>, MoveError> {
        self.answer_decision_at_version(project_name, id, by, value, note, lesson, None)
    }

    /// `Workspace::answer_decision` with the version the caller read.
    pub(crate) fn answer_decision_at_version(
        &self,
        project_name: &str,
        id: &TaskId,
        by: By,
        value: AnswerValue,
        note: Option<&str>,
        lesson: Option<&str>,
        expected: Option<u64>,
    ) -> Result<Option<Task>, MoveError> {
        let at = SystemTime::now();
        let lesson = lesson.map(str::trim).filter(|lesson| !lesson.is_empty());
        let moved = self.move_task(project_name, id, by, false, expected, |task| {
            // An answer is the ask's own door: a resource or dependency
            // wait is resumed by an update, not by recording a value
            // nobody asked for.
            let on_user = task
                .waiting_on
                .as_ref()
                .is_some_and(|wait| !wait.verification && wait.kind == Some(WaitingKind::User));
            if task.status != TaskStatus::Waiting || !on_user {
                return Err(MoveError::NotWaitingQuestion);
            }
            match task.request.as_mut() {
                Some(request) => {
                    // A decision's choice is named against its own options
                    // first, so the refusal quotes the value the person gave
                    // rather than only the kind's shape.
                    if let (AskKind::Decision, AnswerValue::Choice(choice)) = (request.kind, &value)
                        && !request.options.is_empty()
                        && !request.options.iter().any(|option| option == choice)
                    {
                        return Err(MoveError::NotAnOption(choice.clone()));
                    }
                    // **The value must be the shape the kind needs**: an
                    // option list cannot answer a clarification, and a
                    // teaching without its lesson writes no policy. A
                    // teaching whose `text` arrived but whose `lesson` did
                    // not hears the lesson named, not the text it already
                    // gave.
                    if !request.answer_fits(&value, lesson) {
                        let need = if request.kind == AskKind::Teaching && lesson.is_none() {
                            "a teaching takes `lesson` as well as `text`: the lesson is the \
                             policy note its class is settled by"
                                .to_owned()
                        } else {
                            answer_shape_need(request.kind)
                        };
                        return Err(MoveError::AnswerShape(need));
                    }
                    // **A teaching's policy note lands before the row
                    // resumes**, so an answer that reports success has its
                    // note stored rather than only logged.
                    if request.kind == AskKind::Teaching
                        && let (Some(class), Some(lesson)) = (request.class.as_deref(), lesson)
                        && self.settle_policy(project_name, class, lesson, &task.id).is_none()
                    {
                        return Err(MoveError::PolicyWriteFailed(class.to_owned()));
                    }
                    // The record is the answer and nothing more: the value
                    // the kind returned, when, and the note. A second
                    // answer to the same ask is refused by the wait check
                    // above, because answering resumes the row.
                    request.answer = Some(Answer {
                        value,
                        note: note
                            .map(str::trim)
                            .filter(|note| !note.is_empty())
                            .map(str::to_owned),
                        lesson: lesson.map(str::to_owned),
                        at,
                    });
                }
                // A row written before the record: the words are the
                // answer, and they land in the detail as they always did.
                None => append_words(task, &value_words(&value)),
            }
            task.waiting_on = None;
            task.status = TaskStatus::InProgress;
            Ok(())
        })?;
        Ok(moved)
    }

    /// Stamp `project`'s board as looked at - the window's opening, or the
    /// person answering an ask. Server-side so every client agrees.
    pub(crate) fn record_board_seen(&self, project_name: &str, at: SystemTime) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        if let Err(error) = crate::store::board_seen::record(db, project_name, at) {
            tracing::error!(
                target: "forge_workspace::tasks",
                event_name = "board_seen_write_failed",
                project = %project_name,
                %error,
                "recording the board's last-look stamp failed",
            );
        }
    }

    /// When `project`'s board was last looked at.
    pub fn last_board_seen(&self, project_name: &str) -> Option<SystemTime> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        match crate::store::board_seen::last_seen(db, project_name) {
            Ok(seen) => seen,
            Err(error) => {
                tracing::error!(
                    target: "forge_workspace::tasks",
                    event_name = "board_seen_read_failed",
                    project = %project_name,
                    %error,
                    "reading the board's last-look stamp failed; the digest reads as unread",
                );
                None
            }
        }
    }

    /// What changed on `project`'s board since `since`: rows that moved,
    /// rows that ended, asks that expired, and linked pull requests merged.
    /// The window's digest strip is one line each of these counts.
    pub fn board_delta(
        &self,
        project_name: &str,
        since: Option<SystemTime>,
    ) -> crate::board::BoardDelta {
        use crate::board::DeltaItem;
        let since = since.unwrap_or(SystemTime::UNIX_EPOCH);
        let mut tasks = self.tasks_for_project(project_name);
        // **A row the store archived is still a row that moved.** A tree
        // close archives its rows at once and retention takes the rest, so a
        // digest that read only the live set would report a finished epic as
        // nothing at all - the very absence the strip exists to disprove.
        let mut refused = None;
        match self.archived_tasks_for_project_checked(project_name) {
            Ok(archived) => {
                for task in archived {
                    if !tasks.iter().any(|live| live.id == task.id) {
                        tasks.push(task);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::tasks",
                    event_name = "board_delta_archive_read_failed",
                    project = %project_name,
                    %error,
                    "the archive could not be read; the digest counts the live rows only",
                );
                refused =
                    Some("the archive could not be read; the counts are live rows only".to_owned());
            }
        }
        let history = {
            let guard = self.db.lock();
            guard.as_ref().map_or_else(Vec::new, |db| {
                crate::store::task_history::list_for_project(db, project_name).unwrap_or_default()
            })
        };
        let after = |at: SystemTime| at > since;
        let find = |id: &TaskId| tasks.iter().find(|task| task.id == *id);
        // **One item per row**, newest first: the line that counts a move
        // opens onto the rows it counts, not onto every transition.
        let mut moved: Vec<DeltaItem> = Vec::new();
        let mut landed: Vec<DeltaItem> = Vec::new();
        let mut moved_seen: HashSet<TaskId> = HashSet::new();
        let mut landed_seen: HashSet<TaskId> = HashSet::new();
        for step in history.iter().rev().filter(|step| after(step.at)) {
            let Some(task) = find(&step.task_id) else { continue };
            if step.to.is_terminal() {
                if landed_seen.insert(step.task_id.clone()) {
                    landed.push(DeltaItem {
                        id: Some(step.task_id.as_str().to_owned()),
                        subject: task.subject.clone(),
                        word: status_word(step.to).to_owned(),
                        at: step.at,
                    });
                }
                continue;
            }
            if !moved_seen.insert(step.task_id.clone()) {
                continue;
            }
            // A resumed ask carries its answer: a resolved one draws as
            // answered, with the value it returned.
            let answered = task
                .request
                .as_ref()
                .and_then(|request| request.answer.as_ref())
                .filter(|answer| after(answer.at));
            let word = match answered {
                Some(answer) => format!("answered: {}", value_words(&answer.value)),
                None => format!("moved to {}", status_word(step.to)),
            };
            moved.push(DeltaItem {
                id: Some(step.task_id.as_str().to_owned()),
                subject: task.subject.clone(),
                word,
                at: step.at,
            });
        }
        // The asks whose deadline ended them keep their record and draw as
        // expired, never as a canceled row only.
        let expired: Vec<DeltaItem> = tasks
            .iter()
            .filter_map(|task| {
                let at = task
                    .waiting_on
                    .as_ref()
                    .and_then(|wait| wait.expired_at)
                    .filter(|at| after(*at))?;
                Some(DeltaItem {
                    id: Some(task.id.as_str().to_owned()),
                    subject: task.subject.clone(),
                    word: "expired: no answer by its deadline, and its record stays".to_owned(),
                    at,
                })
            })
            .collect();
        let merged: Vec<DeltaItem> = tasks
            .iter()
            .flat_map(|task| task.links.iter().map(move |link| (task, link)))
            .filter(|(_, link)| {
                link.kind == LinkKind::Pr && link.state.as_deref() == Some("merged")
            })
            .filter_map(|(task, link)| {
                let at = link
                    .last_check
                    .as_ref()
                    .and_then(|check| check.changed_at)
                    .filter(|at| after(*at))?;
                Some(DeltaItem {
                    id: Some(task.id.as_str().to_owned()),
                    subject: task.subject.clone(),
                    word: format!("{} merged", link.label.as_deref().unwrap_or(&link.target)),
                    at,
                })
            })
            .collect();
        crate::board::BoardDelta {
            moved: moved.len(),
            landed: landed.len(),
            expired: expired.len(),
            merged: merged.len(),
            moved_items: moved,
            landed_items: landed,
            expired_items: expired,
            merged_items: merged,
            refused,
        }
    }

    /// Whether a policy note settles `class` for `project`.
    pub(crate) fn policy_settles(&self, project_name: &str, class: &str) -> bool {
        self.policy_note(project_name, class).is_some()
    }

    /// The note `project` holds for `class`, when it has settled one.
    fn policy_note(
        &self,
        project_name: &str,
        class: &str,
    ) -> Option<crate::store::policy::PolicyNote> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        match crate::store::policy::get_for_class(db, project_name, class) {
            Ok(note) => note,
            Err(error) => {
                tracing::error!(
                    target: "forge_workspace::tasks",
                    event_name = "policy_read_failed",
                    project = %project_name,
                    class = %class,
                    %error,
                    "reading a project's policy note failed; the class reads as unsettled",
                );
                None
            }
        }
    }

    /// Write the policy note that settles `class` - a teaching's answer, or
    /// the lead's own `tasks__policy` write - and return it. A missing or
    /// failed store answers `None`, and the teaching's answer door refuses
    /// on it: a success that left the lesson on nothing is not one.
    pub(crate) fn settle_policy(
        &self,
        project_name: &str,
        class: &str,
        text: &str,
        source_task: &TaskId,
    ) -> Option<crate::store::policy::PolicyNote> {
        let guard = self.db.lock();
        let db = guard.as_ref()?;
        let note = crate::store::policy::PolicyNote {
            id: uuid::Uuid::new_v4().to_string(),
            project_name: project_name.to_owned(),
            class: class.to_owned(),
            text: text.to_owned(),
            source_task: source_task.as_str().to_owned(),
            at: SystemTime::now(),
        };
        match crate::store::policy::append(db, &note) {
            Ok(()) => Some(note),
            Err(error) => {
                tracing::error!(
                    target: "forge_workspace::tasks",
                    event_name = "policy_write_failed",
                    project = %project_name,
                    class = %class,
                    %error,
                    "writing the policy note failed; the teaching's answer is refused rather \
                     than stored without its note",
                );
                None
            }
        }
    }

    /// `project`'s settled policy notes, by class.
    pub(crate) fn policy_notes_for_project(
        &self,
        project_name: &str,
    ) -> Vec<crate::store::policy::PolicyNote> {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return Vec::new() };
        match crate::store::policy::list_for_project(db, project_name) {
            Ok(notes) => notes,
            Err(error) => {
                tracing::error!(
                    target: "forge_workspace::tasks",
                    event_name = "policy_read_failed",
                    project = %project_name,
                    %error,
                    "reading a project's policy notes failed; the set reads as empty",
                );
                Vec::new()
            }
        }
    }

    /// Move a row in its queue: `Top` above everything, `Up` and `Down`
    /// one place, and `Before` where a drag landed - directly above the row
    /// it was dropped on, or the end of the queue when it names none. The
    /// whole queue is renumbered in one write, so the order a reader sees is
    /// the order the next read returns. The bool says whether the row's
    /// place changed: a pick-up-and-put-back lands the same position.
    pub(crate) fn rank_task(
        &self,
        project_name: &str,
        id: &TaskId,
        to: RankMove,
    ) -> Result<Option<(Task, bool)>, MoveError> {
        let at = SystemTime::now();
        let outcome = self.with_tasks_mut(|tasks| {
            let index = tasks.iter().position(|t| t.id == *id && t.project_name == project_name)?;
            // The queue: this project's rows in the order the board reads
            // them (rank, then creation).
            let mut queue: Vec<usize> = tasks
                .iter()
                .enumerate()
                .filter(|(_, t)| t.project_name == project_name)
                .map(|(i, _)| i)
                .collect();
            queue.sort_by_key(|i| (tasks[*i].rank.unwrap_or(i64::MAX), tasks[*i].created_at));
            let position = queue.iter().position(|i| *i == index)?;
            let moved = queue.remove(position);
            let target = match to {
                // A drag's drop position: the row to sit above, looked up in
                // the queue the removal leaves, so a drag downwards and one
                // upwards land alike.
                RankMove::Before(before) => before
                    .as_deref()
                    .and_then(|id| queue.iter().position(|i| tasks[*i].id.as_str() == id))
                    .unwrap_or(queue.len()),
                RankMove::Top => 0,
                RankMove::Up => position.saturating_sub(1),
                // The queue is one shorter for the row that just left it.
                RankMove::Down => (position + 1).min(queue.len()),
            };
            let changed = target != position;
            queue.insert(target, moved);
            for (rank, i) in queue.iter().enumerate() {
                tasks[*i].rank = Some(i64::try_from(rank).unwrap_or(i64::MAX));
                tasks[*i].version = tasks[*i].version.saturating_add(1);
            }
            tasks[index].updated_at = at;
            // The queue was renumbered: one write per row it holds.
            for i in &queue {
                self.persist_task(&tasks[*i]);
            }
            Some((tasks[index].clone(), changed))
        });
        if outcome.is_some() {
            self.announce_tasks_changed(project_name);
        }
        Ok(outcome)
    }

    /// Give a row an owner, or take it back. The returned record carries
    /// the owner it had before, read under the same lock as the write so a
    /// concurrent claim cannot slip between the two.
    pub(crate) fn assign_task(
        &self,
        project_name: &str,
        id: &TaskId,
        owner: Option<SessionSlot>,
    ) -> Result<Option<(Task, Option<SessionSlot>)>, MoveError> {
        let mut prior = None;
        let moved = self.move_task(project_name, id, By::User, false, None, |task| {
            prior.clone_from(&task.owner);
            task.owner = owner;
            Ok(())
        })?;
        Ok(moved.map(|task| (task, prior)))
    }

    /// Claim a row by id: owner and in_progress land in one write, and the
    /// claim is refused - by name - for a row a live seat holds, a row
    /// that is not ready (pending, nothing waiting on it, not reserved),
    /// or a caller already working another row. Epics are unchanged: a
    /// claim names the row it wants.
    /// The unversioned door the tests drive; every production caller names
    /// the version it read.
    #[cfg(test)]
    pub(crate) fn claim_task(
        &self,
        project_name: &str,
        slot: &SessionSlot,
        id: &TaskId,
    ) -> Result<Task, ClaimError> {
        self.claim_task_at_version(project_name, slot, id, None)
    }

    /// `Workspace::claim_task` with the version the caller read.
    pub(crate) fn claim_task_at_version(
        &self,
        project_name: &str,
        slot: &SessionSlot,
        id: &TaskId,
        expected: Option<u64>,
    ) -> Result<Task, ClaimError> {
        let outcome = self.with_tasks_mut(|tasks| {
            let already = tasks.iter().any(|t| {
                t.project_name == project_name
                    && t.status == TaskStatus::InProgress
                    && t.owner.as_ref().map(SessionSlot::label) == Some(slot.label())
            });
            if already {
                return Err(ClaimError::AlreadyWorking);
            }
            let index = find_task_index(tasks, project_name, id)?;
            if let Some(expected) = expected
                && tasks[index].version != expected
            {
                return Err(ClaimError::VersionMismatch {
                    expected,
                    current: tasks[index].version,
                });
            }
            // The three ways a row can be unclaimable are named apart: a
            // reserved pick is the user's, a row in another state is not
            // startable, and a waiting row is already in play.
            if tasks[index].reserved.is_some() {
                return Err(ClaimError::Reserved);
            }
            if tasks[index].waiting_on.is_some() {
                return Err(ClaimError::Waiting);
            }
            if !tasks[index].is_ready() {
                return Err(ClaimError::NotPending(tasks[index].status));
            }
            let holder = tasks[index].owner.clone();
            if let Some(holder) = holder
                && holder.label() != slot.label()
                && self.seat_is_live(&holder)
            {
                return Err(ClaimError::OwnedByLiveSeat(holder.label().to_owned()));
            }
            let prior = tasks[index].status;
            tasks[index].owner = Some(slot.clone());
            tasks[index].status = TaskStatus::InProgress;
            tasks[index].version = tasks[index].version.saturating_add(1);
            tasks[index].updated_at = SystemTime::now();
            self.persist_task(&tasks[index]);
            Ok((tasks[index].clone(), prior))
        });
        let (task, prior) = outcome?;
        self.record_transition(
            project_name,
            &task.id,
            Some(prior),
            task.status,
            By::Seat(slot.clone()),
        );
        self.announce_tasks_changed(project_name);
        Ok(task)
    }

    /// Put a row the caller owns into `waiting`, stating what it waits on.
    /// The unversioned door the tests drive; every production caller names
    /// the version it read.
    #[cfg(test)]
    pub(crate) fn wait_task(
        &self,
        project_name: &str,
        id: &TaskId,
        slot: &SessionSlot,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
    ) -> Result<Task, MoveError> {
        self.wait_task_recording(project_name, id, slot, kind, detail, on, None, None, None)
    }

    /// `Workspace::wait_task` with the version the caller read.
    pub(crate) fn wait_task_at_version(
        &self,
        project_name: &str,
        id: &TaskId,
        slot: &SessionSlot,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
        expected: Option<u64>,
    ) -> Result<Task, MoveError> {
        self.wait_task_recording(project_name, id, slot, kind, detail, on, None, None, expected)
    }

    /// `Workspace::wait_request_task_at_version` for a decision, without a
    /// version: the unversioned door the tests drive.
    #[cfg(test)]
    pub(crate) fn wait_decision_task(
        &self,
        project_name: &str,
        id: &TaskId,
        slot: &SessionSlot,
        detail: Option<String>,
        question: String,
        options: Vec<String>,
    ) -> Result<Task, MoveError> {
        self.wait_request_task_at_version(
            project_name,
            id,
            slot,
            detail,
            Request { kind: AskKind::Decision, question, options, class: None, answer: None },
            None,
            None,
        )
    }

    /// The ask door for any kind on the person: the request record the
    /// facade built, and the deadline that runs if it is ignored.
    pub(crate) fn wait_request_task_at_version(
        &self,
        project_name: &str,
        id: &TaskId,
        slot: &SessionSlot,
        detail: Option<String>,
        request: Request,
        due: Option<Deadline>,
        expected: Option<u64>,
    ) -> Result<Task, MoveError> {
        // **A settled class is not asked again.** The policy note that
        // settles it is quoted, so the asker reads the answer it already
        // has rather than re-raising the class.
        if let Some(class) = request.class.as_deref()
            && let Some(note) = self.policy_note(project_name, class)
        {
            return Err(MoveError::PolicySettled { class: class.to_owned(), text: note.text });
        }
        self.wait_task_recording(
            project_name,
            id,
            slot,
            WaitingKind::User,
            detail,
            None,
            Some(request),
            due,
            expected,
        )
    }

    fn wait_task_recording(
        &self,
        project_name: &str,
        id: &TaskId,
        slot: &SessionSlot,
        kind: WaitingKind,
        detail: Option<String>,
        on: Option<&TaskId>,
        request: Option<Request>,
        due: Option<Deadline>,
        expected: Option<u64>,
    ) -> Result<Task, MoveError> {
        let at = SystemTime::now();
        let outcome = self.with_tasks_mut(|tasks| {
            let index = find_task_index(tasks, project_name, id)?;
            if let Some(expected) = expected
                && tasks[index].version != expected
            {
                return Err(MoveError::VersionMismatch { expected, current: tasks[index].version });
            }
            let owner = tasks[index].owner.as_ref().map(SessionSlot::label).map(str::to_owned);
            if owner.as_deref() != Some(slot.label()) {
                return Err(MoveError::NotOwner);
            }
            // The blocker resolves the same way, and after the ownership
            // check: a caller waiting a row that is not its own hears
            // that, not what its `on` says.
            let on = match on {
                Some(stated) => {
                    Some(tasks[find_task_index(tasks, project_name, stated)?].id.clone())
                }
                None => None,
            };
            // **Every park on the user is an ask or a look.** A bare `user`
            // wait has nothing to answer and no approve door; looks are
            // minted by their own park paths, never through this door.
            if kind == WaitingKind::User && request.is_none() {
                return Err(MoveError::BareUserWait);
            }
            let prior = tasks[index].status;
            // **A repeated identical wait is the same wait**: keep its start,
            // so its age and a notification's identity do not move under a
            // caller re-stating what the row already holds.
            let same = tasks[index].waiting_on.as_ref().filter(|wait| {
                wait.kind == Some(kind)
                    && wait.detail.as_deref() == detail.as_deref()
                    && wait.on.as_ref() == on.as_ref()
                    && !wait.verification
                    && request.as_ref().map(|r| (r.kind, r.question.as_str()))
                        == tasks[index].request.as_ref().map(|r| (r.kind, r.question.as_str()))
            });
            let since = same.and_then(|wait| wait.since).unwrap_or(at);
            tasks[index].waiting_on = Some(Waiting {
                kind: Some(kind),
                detail,
                on,
                verification: false,
                parked: false,
                // Every item carries a deadline: one the asker named, else
                // the escalate default.
                due: due.or_else(|| (kind == WaitingKind::User).then(|| default_user_deadline(at))),
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: Some(since),
            });
            if let Some(request) = request {
                // A new ask replaces the last one: the record is the ask in
                // front of the person, not a history of asks.
                tasks[index].request = Some(request);
            }
            tasks[index].status = TaskStatus::Waiting;
            tasks[index].version = tasks[index].version.saturating_add(1);
            tasks[index].updated_at = at;
            self.persist_task(&tasks[index]);
            Ok((tasks[index].clone(), prior))
        })?;
        let (task, prior) = outcome;
        if prior != task.status {
            self.record_transition(
                project_name,
                &task.id,
                Some(prior),
                task.status,
                By::Seat(slot.clone()),
            );
        }
        self.announce_tasks_changed(project_name);
        Ok(task)
    }

    /// Park an overrun row waiting on its owner: the chase's re-plan ask is a
    /// state the board draws, not a message alone. A row already off
    /// `in_progress` is left alone - the owner got there first.
    pub(crate) fn park_overrun(&self, project_name: &str, id: &TaskId) -> Option<Task> {
        let at = SystemTime::now();
        let outcome = self.with_tasks_mut(|tasks| {
            let index = tasks
                .iter()
                .position(|task| task.project_name == project_name && task.id == *id)?;
            if tasks[index].status != TaskStatus::InProgress {
                return None;
            }
            tasks[index].waiting_on = Some(Waiting {
                kind: Some(WaitingKind::Resource),
                detail: Some(
                    "over its estimate: re-plan it - rescope, split, or close it with the reason"
                        .to_owned(),
                ),
                on: None,
                verification: false,
                parked: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: Some(at),
            });
            tasks[index].status = TaskStatus::Waiting;
            tasks[index].version = tasks[index].version.saturating_add(1);
            tasks[index].updated_at = at;
            self.persist_task(&tasks[index]);
            Some(tasks[index].clone())
        });
        if outcome.is_some() {
            self.announce_tasks_changed(project_name);
        }
        outcome
    }

    /// Clear every wait on `blocker` - any terminal end of it counts, and
    /// so does its deletion - and hand the released rows back so the
    /// caller can message their owners.
    pub(crate) fn resolve_wait_on(&self, project_name: &str, blocker: &TaskId) -> Vec<Task> {
        let released = self.with_tasks_mut(|tasks| {
            let mut released: Vec<Task> = Vec::new();
            for task in tasks.iter_mut() {
                if task.project_name != project_name || task.status != TaskStatus::Waiting {
                    continue;
                }
                let waits_on_blocker =
                    task.waiting_on.as_ref().and_then(|w| w.on.as_ref()) == Some(blocker);
                if !waits_on_blocker {
                    continue;
                }
                task.waiting_on = None;
                task.status = TaskStatus::Pending;
                task.version = task.version.saturating_add(1);
                task.updated_at = SystemTime::now();
                released.push(task.clone());
            }
            for task in &released {
                self.persist_task(task);
            }
            released
        });
        for task in &released {
            self.record_transition(
                project_name,
                &task.id,
                Some(TaskStatus::Waiting),
                TaskStatus::Pending,
                By::System,
            );
        }
        if !released.is_empty() {
            self.announce_tasks_changed(project_name);
        }
        released
    }

    /// Whether a seat has something behind it right now: a registered
    /// domain or agent, or a live worker registry row.
    pub(crate) fn seat_is_live(&self, slot: &SessionSlot) -> bool {
        if self.domain_session_for(slot).is_some() || self.has_agent_for(slot) {
            return true;
        }
        let key = self
            .list_projects()
            .into_iter()
            .find(|view| view.name == slot.project())
            .map(|view| view.key);
        let Some(key) = key else { return false };
        self.list_live_workers(&key).iter().any(|entry| &entry.slot == slot)
    }

    /// Append one status transition to the durable history.
    fn record_transition(
        &self,
        project_name: &str,
        id: &TaskId,
        from: Option<TaskStatus>,
        to: TaskStatus,
        by: By,
    ) {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else { return };
        let transition = TaskTransition {
            task_id: id.clone(),
            project_name: project_name.to_owned(),
            from,
            to,
            at: SystemTime::now(),
            by,
        };
        if let Err(error) = crate::store::task_history::append(db, &[transition]) {
            tracing::error!(
                target: "forge_workspace::tasks",
                %error,
                "persisting task history failed; a move is missing from the history",
            );
        }
    }

    /// Remove the task `id` in `project_name` together with every
    /// descendant, persist, and return the records removed (empty when
    /// nothing was). Backs `tasks__delete`. The cascade happens here
    /// rather than at the call site so a parent can never be deleted out
    /// from under children that would then have nobody to close them.
    ///
    /// A tree holding a row waiting on the user - a verification park or
    /// a decision wait - is refused by name (#2251): removal would settle
    /// that row by deletion, an ask the update path refuses to unbind,
    /// complete, or drop without a reason. The check walks the whole
    /// cascade, so deleting the epic above the row is no way around it.
    /// The unversioned door the tests drive; every production caller names
    /// the version it read.
    #[cfg(test)]
    pub(crate) fn remove_task_tree(
        &self,
        project_name: &str,
        id: &TaskId,
    ) -> Result<Vec<Task>, MoveError> {
        self.remove_task_tree_at_version(project_name, id, None)
    }

    /// `Workspace::remove_task_tree` with the version the caller read.
    pub(crate) fn remove_task_tree_at_version(
        &self,
        project_name: &str,
        id: &TaskId,
        expected: Option<u64>,
    ) -> Result<Vec<Task>, MoveError> {
        let removed = self.with_tasks_mut(|tasks| {
            if let Some(expected) = expected {
                // A delete is a write like any other: it names the version
                // it read, so a row another writer moved is not removed
                // under their feet.
                let Some(task) =
                    tasks.iter().find(|task| task.project_name == project_name && task.id == *id)
                else {
                    return Ok(Vec::new());
                };
                if task.version != expected {
                    return Err(MoveError::VersionMismatch { expected, current: task.version });
                }
            }
            let mut doomed = vec![id.clone()];
            let mut cursor = 0;
            while cursor < doomed.len() {
                let current = doomed[cursor].clone();
                cursor += 1;
                for task in tasks.iter() {
                    if task.project_name == project_name
                        && task.parent.as_ref() == Some(&current)
                        && !doomed.contains(&task.id)
                    {
                        doomed.push(task.id.clone());
                    }
                }
            }
            if let Some(task) = tasks.iter().find(|task| {
                task.project_name == project_name
                    && doomed.contains(&task.id)
                    && parked_on_user(task)
            }) {
                return Err(MoveError::DeletingLook(task.subject.clone()));
            }
            // An ANSWERED decision is the record of what was asked and what
            // was chosen (#3031): the row is the only copy, so removal is
            // not a way to erase it. A cancel keeps it readable.
            if let Some(task) = tasks.iter().find(|task| {
                task.project_name == project_name
                    && doomed.contains(&task.id)
                    && task.request.as_ref().is_some_and(|request| request.answer.is_some())
            }) {
                return Err(MoveError::DeletingDecision(task.subject.clone()));
            }
            let mut removed = Vec::new();
            tasks.retain(|t| {
                let taking = t.project_name == project_name && doomed.contains(&t.id);
                if taking {
                    removed.push(t.clone());
                }
                !taking
            });
            for task in &removed {
                self.delete_stored_task(project_name, &task.id);
            }
            Ok(removed)
        })?;
        if !removed.is_empty() {
            // A removed row can never end, so its waiters are released
            // here - the other half of "any terminal end clears the wait".
            for task in &removed {
                self.resolve_wait_on(project_name, &task.id);
            }
            self.announce_tasks_changed(project_name);
        }
        Ok(removed)
    }

    /// Archive every terminal tree that has stood still past
    /// [`RETENTION_SECS`], returning the rows taken. A top-level row goes
    /// only with a tree in which every row has ended: an open child holds
    /// its parent where it is, and a completed child stays under its live
    /// epic because the epic's rollup reads it. Called from the same sweep
    /// as the chase, so a row leaves without anyone asking.
    pub(crate) fn retain_terminal_rows(&self, now: SystemTime) -> Vec<Task> {
        let mut archived = Vec::new();
        let projects: Vec<String> =
            self.list_projects().into_iter().map(|view| view.name).collect();
        for project in projects {
            let age = |task: &Task| {
                now.duration_since(task.updated_at).map_or(0, |elapsed| elapsed.as_secs())
            };
            let taken = self.with_tasks_mut(|tasks| {
                let mut children: std::collections::HashMap<&TaskId, Vec<usize>> =
                    std::collections::HashMap::new();
                for (index, task) in tasks.iter().enumerate() {
                    if task.project_name == project
                        && let Some(parent) = &task.parent
                    {
                        children.entry(parent).or_default().push(index);
                    }
                }
                let mut doomed: Vec<TaskId> = Vec::new();
                for task in tasks.iter() {
                    if task.project_name != project
                        || task.parent.is_some()
                        || !task.status.is_terminal()
                        || age(task) < RETENTION_SECS
                    {
                        continue;
                    }
                    let mut tree = vec![task.id.clone()];
                    let mut whole = true;
                    let mut cursor = 0;
                    while cursor < tree.len() && whole {
                        let current = tree[cursor].clone();
                        cursor += 1;
                        for index in children.get(&current).into_iter().flatten() {
                            // **Every row in the tree waits its own
                            // window**: a child that just ended must not be
                            // archived because its root ended a day earlier.
                            if !tasks[*index].status.is_terminal()
                                || age(&tasks[*index]) < RETENTION_SECS
                            {
                                whole = false;
                                break;
                            }
                            tree.push(tasks[*index].id.clone());
                        }
                    }
                    if whole {
                        doomed.extend(tree);
                    }
                }
                if doomed.is_empty() {
                    return Vec::new();
                }
                let doomed: HashSet<TaskId> = doomed.into_iter().collect();
                let mut taken = Vec::new();
                tasks.retain(|task| {
                    let taking = task.project_name == project && doomed.contains(&task.id);
                    if taking {
                        taken.push(task.clone());
                    }
                    !taking
                });
                for task in &mut taken {
                    task.archived_at = Some(now);
                    task.version = task.version.saturating_add(1);
                    self.delete_stored_task(&project, &task.id);
                }
                taken
            });
            if taken.is_empty() {
                continue;
            }
            if let Some(db) = self.db.lock().as_ref()
                && let Err(error) = crate::store::task_archive::append(db, &taken)
            {
                tracing::error!(
                    target: "forge_workspace::tasks",
                    project = %project,
                    %error,
                    "retaining a terminal tree failed to archive it; the rows are gone from the live set",
                );
            }
            self.announce_tasks_changed(&project);
            archived.extend(taken);
        }
        archived
    }

    /// A project's archived rows, oldest archive first, with the failure kept
    /// apart from an empty archive:
    /// a caller looking a row up must not answer "no such task" for a store
    /// it could not read.
    pub fn archived_tasks_for_project_checked(
        &self,
        project_name: &str,
    ) -> Result<Vec<Task>, String> {
        let guard = self.db.lock();
        let Some(db) = guard.as_ref() else {
            return Err("this run has no task store open".to_owned());
        };
        let mut rows = crate::store::task_archive::list_for_project(db, project_name)
            .map_err(|error| error.to_string())?;
        rows.sort_by_key(|task| task.archived_at);
        Ok(rows)
    }

    /// Bring an archived row and the tree it took back to the live set,
    /// with its stamp cleared. The row returns terminal: the bulk board
    /// still keeps it out of the way, and the reopened door is a move. An
    /// id the archive does not hold is absent, not an error.
    pub(crate) fn restore_task(
        &self,
        project_name: &str,
        id: &TaskId,
    ) -> Result<Option<Task>, MoveError> {
        let archived = self
            .archived_tasks_for_project_checked(project_name)
            .map_err(MoveError::ArchiveUnreadable)?;
        if !archived.iter().any(|task| task.id == *id) {
            return Ok(None);
        }
        let mut tree: Vec<TaskId> = vec![id.clone()];
        let mut cursor = 0;
        while cursor < tree.len() {
            let current = tree[cursor].clone();
            cursor += 1;
            for task in &archived {
                if task.parent.as_ref() == Some(&current) && !tree.contains(&task.id) {
                    tree.push(task.id.clone());
                }
            }
        }
        let mut restored: Vec<Task> =
            archived.into_iter().filter(|task| tree.contains(&task.id)).collect();
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::task_archive::remove(db, &restored)
        {
            tracing::error!(
                target: "forge_workspace::tasks",
                project = %project_name,
                %error,
                "restoring a row failed to take it off the archive; it may read twice",
            );
        }
        let at = SystemTime::now();
        for task in &mut restored {
            task.archived_at = None;
            task.version = task.version.saturating_add(1);
            task.updated_at = at;
        }
        self.with_tasks_mut(|tasks| {
            for task in &restored {
                self.persist_task(task);
                tasks.push(task.clone());
            }
        });
        self.announce_tasks_changed(project_name);
        Ok(restored.into_iter().find(|task| task.id == *id))
    }

    /// Tell every view the project's task set moved, as the set the write
    /// just left. It routes on the project's lead seat, which is the seat a
    /// project-scoped section belongs to; a name no project carries has no
    /// seat to route on and announces nothing.
    pub(crate) fn announce_tasks_changed(&self, project_name: &str) {
        let Some(key) = self.lead_slot_for_project(project_name) else {
            tracing::debug!(
                target: "forge_workspace::tasks",
                event_name = "tasks_changed_unroutable",
                project = %project_name,
                "a task write named a project no project carries; nothing was announced",
            );
            return;
        };
        // **The frame carries the live set, exactly what the board read
        // returns.** A terminal row has left the bulk board; carrying it
        // here re-added every lingering finished row to the drawn lanes
        // after each write until the next read (#3031).
        let tasks: Vec<Task> = self
            .tasks_for_project(project_name)
            .into_iter()
            .filter(|task| !task.status.is_terminal())
            .collect();
        let _ = self.update_tx.send(SessionUpdate::TasksChanged { key, tasks });
    }

    /// The tasks registered for `project_name`. Backs `tasks__list` and
    /// the Inspector TASKS snapshot.
    pub fn tasks_for_project(&self, project_name: &str) -> Vec<Task> {
        self.tasks.lock().iter().filter(|t| t.project_name == project_name).cloned().collect()
    }

    /// Whether this run's boot could not load the durable board. The
    /// in-memory set is empty while rows may be on disk, so a reader must
    /// not report "none open" for a board it could not read - the handoff
    /// brief says "not read" instead.
    pub fn durable_tasks_unavailable(&self) -> bool {
        self.tasks_load_failed.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Register a task directly, bypassing the MCP create path.
    /// Cross-crate test access to the otherwise `pub(crate)` task store
    /// so a view can exercise the Inspector's `refresh_tasks`
    /// resolution against a seeded task.
    ///
    /// The store write without the announcement: a fixture seeds state, it
    /// does not perform the write a view is owed a frame for.
    pub fn seed_test_task(&self, task: forge_primitives::tasks::Task) {
        self.with_tasks_mut(|tasks| {
            self.persist_task(&task);
            tasks.push(task);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SeatUpdate;
    use crate::{SessionSlot, SessionUpdate};
    use forge_primitives::tasks::{
        By, Evidence, LinkCheck, Task, TaskId, TaskLink, TaskStatus, Waiting, WaitingKind,
    };
    use tempfile::tempdir;

    /// A pull-request link as a row carries it, unchecked by default.
    fn pr_link(target: &str) -> TaskLink {
        TaskLink {
            kind: forge_primitives::tasks::LinkKind::Pr,
            label: None,
            target: target.to_owned(),
            state: None,
            last_check: None,
            added_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// A pull-request link with a connector check stamped now.
    fn pr_link_checked(target: &str, ci: Option<&str>) -> TaskLink {
        TaskLink {
            state: Some("open".to_owned()),
            last_check: Some(LinkCheck {
                at: std::time::SystemTime::now(),
                changed_at: Some(std::time::SystemTime::now()),
                state: Some("open".to_owned()),
                head: Some("abc123".to_owned()),
                ci: ci.map(str::to_owned),
                problem: None,
            }),
            ..pr_link(target)
        }
    }

    /// A verifier that answers exactly what the test tells it: the
    /// workspace's own check is the trait, so the gate's refusals are
    /// driven without a repository.
    struct FixedVerifier(std::sync::Mutex<Result<(), String>>);

    impl FixedVerifier {
        fn accepting() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self(std::sync::Mutex::new(Ok(()))))
        }
        fn refusing(reason: &str) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self(std::sync::Mutex::new(Err(reason.to_owned()))))
        }
    }

    impl crate::env::evidence::EvidenceVerifier for FixedVerifier {
        fn verify(
            &self,
            _repo: &std::path::Path,
            _commit: Option<&str>,
            _path: Option<&str>,
        ) -> Result<(), String> {
            let answer = self.0.lock().expect("the test's verifier lock");
            answer.clone()
        }
    }

    fn install_db(ws: &Workspace, dir: &std::path::Path) {
        let db = crate::store::Db::open(&dir.join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
    }

    fn live_worker(ws: &Workspace, project: &str, label: &str) {
        let key = ws
            .list_projects()
            .into_iter()
            .find(|view| view.name == project)
            .expect("seeded project")
            .key
            .clone();
        ws.insert_live_worker(
            &key,
            crate::WorkerEntry {
                label: label.to_owned(),
                charter: "charter".to_owned(),
                slot: SessionSlot::worker("TestOrg", project, label),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead("TestOrg", project),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: None,
                diagnostic: None,
                kick: None,
            },
        );
    }

    fn history_of(ws: &Workspace, project: &str) -> Vec<forge_primitives::tasks::TaskTransition> {
        let db = ws.db.lock();
        crate::store::task_history::list_for_project(db.as_ref().expect("db installed"), project)
            .expect("history")
    }

    fn archived_of(ws: &Workspace, project: &str) -> Vec<Task> {
        let db = ws.db.lock();
        crate::store::task_archive::list_for_project(db.as_ref().expect("db installed"), project)
            .expect("archive")
    }

    /// The one `TasksChanged` on a test's update stream, as its key and the
    /// set it announces.
    fn next_tasks_changed(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SeatUpdate>,
    ) -> (SessionSlot, Vec<Task>) {
        match rx.try_recv().map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
            Ok(SessionUpdate::TasksChanged { key, tasks }) => (key, tasks),
            other => panic!("expected a TasksChanged on the stream, got {other:?}"),
        }
    }

    /// The LAST `TasksChanged` a test's stream holds: every task write
    /// announces its project's set, so the frame a test is asking about is
    /// the one its own write left, not the creation that came before it.
    fn last_tasks_changed(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SeatUpdate>,
    ) -> (SessionSlot, Vec<Task>) {
        let mut last = None;
        while let Ok(frame) = rx.try_recv() {
            if let SessionUpdate::TasksChanged { key, tasks } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                last = Some((key, tasks));
            }
        }
        last.expect("the stream holds a TasksChanged")
    }

    fn subjects(tasks: &[Task]) -> Vec<String> {
        tasks.iter().map(|task| task.subject.clone()).collect()
    }

    /// A claim the stub verifier accepts: this suite is about the moves,
    /// and the gate's own refusals have their own tests below.
    fn passing_evidence() -> Option<forge_primitives::tasks::Evidence> {
        Some(forge_primitives::tasks::Evidence {
            commit: Some("test-commit".to_owned()),
            path: None,
            lead_verified: None,
            excerpt: None,
            verified_at: None,
            problem: None,
        })
    }

    fn sample_task(id: &str, project: &str) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: project.to_owned(),
            subject: format!("subject {id}"),
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
            evidence: passing_evidence(),
            version: 0,
            archived_at: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn sample_task_with_parent(id: &str, parent: Option<&str>, project: &str) -> Task {
        Task { parent: parent.map(TaskId::from), ..sample_task(id, project) }
    }

    /// A row whose subject is its own words, so a refusal naming the rows
    /// it matched reads in the assertion that names them too.
    fn named_task(id: &str, subject: &str, project: &str) -> Task {
        Task { subject: subject.to_owned(), ..sample_task(id, project) }
    }

    #[test]
    fn a_task_can_be_updated_by_a_session_that_does_not_own_it() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("forge", "/tmp/tp-update-any");
        // A child row: a completing ROOT closes and archives (own test).
        let mut seeded = sample_task("t-1", "forge");
        seeded.parent = Some(TaskId::from("epic"));
        ws.push_task(seeded);
        let changed = ws
            .update_task(
                "forge",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "forge")),
                |t| {
                    t.status = TaskStatus::Completed;
                },
            )
            .expect("no refusal");
        assert!(changed.is_some(), "any session in the project may move a task");
        assert_eq!(ws.tasks_for_project("forge")[0].status, TaskStatus::Completed);
    }

    #[test]
    fn tasks_for_project_does_not_leak_across_projects() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.push_task(sample_task("t-1", "forge"));
        assert_eq!(ws.tasks_for_project("forge").len(), 1, "listed for its own project");
        assert!(ws.tasks_for_project("other").is_empty(), "scoped by project name");
    }

    #[test]
    fn a_write_persists_to_the_store() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws.push_task(sample_task("t-1", "forge"));
        let stored =
            crate::store::tasks::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
        assert_eq!(stored.len(), 1, "the in-memory set and the store never diverge");
    }

    /// Deleting is durable, not just in-memory: the tree has to leave the
    /// store too, or the next boot reads it back.
    ///
    /// The two cross-project tasks are what pin the cascade's project
    /// guards, and nothing else reaches them: a same-project task outside
    /// the tree is never a cascade candidate, and its id is never doomed.
    /// One shares the doomed id, so the `retain` would take it without its
    /// own project check; the other is a cross-project child of a doomed id
    /// whose id collides with a task that has to survive here, so the
    /// collection would carry that collision into this project and the
    /// retain would then take the survivor.
    #[test]
    fn deleting_a_task_takes_its_children() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws.push_task(sample_task_with_parent("epic", None, "forge"));
        ws.push_task(sample_task_with_parent("sub-a", Some("epic"), "forge"));
        ws.push_task(sample_task_with_parent("sub-b", Some("sub-a"), "forge"));
        // A task in the SAME project but outside the tree: the cascade must
        // take the descendants and nothing else.
        ws.push_task(sample_task("sibling", "forge"));
        ws.push_task(sample_task("epic", "elsewhere"));
        ws.push_task(sample_task_with_parent("sibling", Some("epic"), "elsewhere"));
        assert_eq!(
            ws.remove_task_tree("forge", &TaskId::from("epic"))
                .expect("nothing in the tree waits on a look")
                .len(),
            3,
            "the tree is removed: the epic and its two descendants",
        );
        let left = ws.tasks_for_project("forge");
        assert_eq!(
            left.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            vec![TaskId::from("sibling")],
            "children and grandchildren go with the parent, and nothing else does",
        );
        let stored =
            crate::store::tasks::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
        assert!(
            stored.iter().all(|t| t.project_name != "forge" || t.id == TaskId::from("sibling")),
            "the deletion reached the store, not only the in-memory set: {stored:?}",
        );
        assert_eq!(
            ws.tasks_for_project("elsewhere").len(),
            2,
            "another project keeps both of its tasks, one of them sharing a doomed id",
        );
    }

    /// The failed-boot-read state for the task store: the store holds
    /// durable rows, the in-memory set is EMPTY (a `list` failure at boot
    /// warned and started with none), and the db handle is live. Every
    /// task mutation writes only what it changed - a door that changed
    /// nothing writes nothing, and a door that changed something writes
    /// its own row - so no write after such a boot can erase the rows the
    /// failed read never loaded.
    ///
    /// Both kinds of door are walked on purpose: a whole-set write that
    /// only fires on a successful mutation would slip past a pin that ran
    /// the no-op doors alone.
    ///
    /// Mutant: have any mutation site (or the shared mutation helper) write
    /// the whole in-memory set, at a door that changed nothing or one that
    /// changed something, where either write takes both seeded rows with it.
    #[test]
    fn task_mutations_over_a_failed_boot_read_do_not_erase_the_store() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-tasks-failed-load");
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        // The rows the failed boot read left out of memory: in the store only.
        crate::store::tasks::upsert(&db, &sample_task("seed-1", "proj")).expect("seed 1");
        crate::store::tasks::upsert(&db, &sample_task("seed-2", "proj")).expect("seed 2");
        ws.install_db_for_test(db);
        let persisted = || {
            crate::store::tasks::list(ws.db.lock().as_ref().expect("db installed")).expect("list")
        };
        let seeded =
            |rows: &[Task]| rows.iter().filter(|t| t.id.as_str().starts_with("seed-")).count();
        // The one assertion every door below makes: it wrote its own row at
        // most, never the whole in-memory set.
        let still_seeded = |door: &str| {
            assert_eq!(
                seeded(&persisted()),
                2,
                "after {door} the rows the failed read never loaded must still be there",
            );
        };
        let lead = SessionSlot::lead("TestOrg", "proj");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        let other = SessionSlot::worker("TestOrg", "proj", "w-2");

        ws.push_task(sample_task("created", "proj"));
        still_seeded("a create");

        // Doors that change nothing: each names a row the failed read never
        // loaded, so nothing moves and nothing may be written.
        assert!(
            ws.update_task("proj", &TaskId::from("seed-1"), By::Seat(lead.clone()), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .is_none(),
            "precondition: the empty in-memory set holds no such row",
        );
        still_seeded("an update naming no loaded row");

        assert_eq!(
            ws.claim_task("proj", &worker, &TaskId::from("seed-1")),
            Err(ClaimError::NotFound),
            "precondition: the empty in-memory set holds no such row to claim",
        );
        still_seeded("a refused claim");

        assert_eq!(
            ws.wait_task(
                "proj",
                &TaskId::from("seed-1"),
                &worker,
                WaitingKind::Dependency,
                None,
                None
            ),
            Err(MoveError::NotFound),
            "precondition: the empty in-memory set holds no such row to wait",
        );
        still_seeded("a refused wait");

        assert!(
            ws.rank_task("proj", &TaskId::from("seed-1"), RankMove::Top)
                .expect("no refusal")
                .is_none(),
            "precondition: the empty in-memory set holds no such row to rank",
        );
        still_seeded("a rank naming no loaded row");

        assert!(
            ws.remove_task_tree("proj", &TaskId::from("seed-1"))
                .expect("nothing in the tree waits on a look")
                .is_empty(),
            "precondition: the empty in-memory set holds no such tree",
        );
        still_seeded("a delete that took nothing");

        assert!(
            ws.resolve_wait_on("proj", &TaskId::from("seed-1")).is_empty(),
            "precondition: no loaded row waits on it",
        );
        still_seeded("a wait-release sweep over an unloaded row");

        // Doors that DO change something, each on a loaded row: a whole-set
        // write gated behind the change takes the unloaded rows with it.
        ws.claim_task("proj", &worker, &TaskId::from("created")).expect("the loaded row is free");
        still_seeded("a claim of a loaded row");

        ws.wait_task(
            "proj",
            &TaskId::from("created"),
            &worker,
            WaitingKind::Dependency,
            None,
            None,
        )
        .expect("the owner may wait its own row");
        still_seeded("a wait on a loaded row");

        ws.rank_task("proj", &TaskId::from("created"), RankMove::Top)
            .expect("no refusal")
            .expect("the loaded row is there");
        still_seeded("a rank of a loaded row");

        // The move's two write branches: an update that keeps the row live
        // (a child), and one that closes a root and takes its tree with it.
        ws.push_task(sample_task_with_parent("child", Some("ghost-epic"), "proj"));
        ws.update_task("proj", &TaskId::from("child"), By::Seat(lead.clone()), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the loaded row is there");
        still_seeded("an update of a loaded row");

        ws.push_task(sample_task("root", "proj"));
        ws.update_task("proj", &TaskId::from("root"), By::Seat(lead.clone()), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the loaded row is there");
        still_seeded("a completed root closing its tree");

        // A released wait: the sweep writes each row it hands back.
        ws.push_task(sample_task("blocker", "proj"));
        ws.push_task(sample_task("waiter", "proj"));
        ws.claim_task("proj", &other, &TaskId::from("waiter")).expect("the loaded row is free");
        ws.wait_task(
            "proj",
            &TaskId::from("waiter"),
            &other,
            WaitingKind::Dependency,
            None,
            Some(&TaskId::from("blocker")),
        )
        .expect("the owner may wait its own row");
        ws.update_task("proj", &TaskId::from("blocker"), By::Seat(lead.clone()), |task| {
            task.status = TaskStatus::Canceled;
        })
        .expect("no refusal")
        .expect("the loaded row is there");
        still_seeded("a blocker ending that releases a wait");

        ws.push_task(sample_task_with_parent("tree-root", None, "proj"));
        ws.push_task(sample_task_with_parent("tree-child", Some("tree-root"), "proj"));
        assert_eq!(
            ws.remove_task_tree("proj", &TaskId::from("tree-root"))
                .expect("nothing in the tree waits on a look")
                .len(),
            2,
            "precondition: both loaded rows are removed",
        );
        still_seeded("a delete of a loaded tree");
    }

    /// Every task write announces the project's set on the project's lead
    /// seat, so the inspector's TASKS section pops on the frame that moved it
    /// rather than on the next unrelated read.
    ///
    /// The three writers are pinned together because they are one section's
    /// three doors. Mutants: drop any one emission (the drain is empty), or
    /// announce a set taken before the write (the announced set disagrees
    /// with the store).
    #[test]
    fn every_task_write_announces_the_projects_set() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-tasks");
        let lead = SessionSlot::lead("TestOrg", "proj");

        // A child row: completing a ROOT would close and archive the tree,
        // which is its own test; this one is about the announcements.
        let mut seeded = sample_task("t-1", "proj");
        seeded.parent = Some(TaskId::from("epic"));
        ws.push_task(seeded);
        let (key, tasks) = next_tasks_changed(&mut rx);
        assert_eq!(key, lead, "a create routes on the project's lead seat");
        assert_eq!(subjects(&tasks), ["subject t-1"], "and carries the set the core now holds");

        assert!(
            ws.update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                }
            )
            .expect("no refusal")
            .is_some()
        );
        let (key, tasks) = next_tasks_changed(&mut rx);
        assert_eq!(key, lead, "an update routes the same way");
        assert!(
            tasks.is_empty(),
            "the frame is the LIVE set: a finished row left it, and the closed shelf reads \
             those on its own"
        );

        assert!(
            !ws.remove_task_tree("proj", &TaskId::from("t-1"))
                .expect("nothing in the tree waits on a look")
                .is_empty()
        );
        let (_, tasks) = next_tasks_changed(&mut rx);
        assert!(tasks.is_empty(), "a delete announces the set it left behind");
    }

    /// A write that changed nothing emits nothing: an update for an id no
    /// task carries and a delete of one nothing holds are both refusals, and
    /// an announcement would be news about a write that never happened.
    ///
    /// Mutant: announce unconditionally, where the refused pair is news the
    /// section never moved for.
    #[test]
    fn a_task_write_that_changed_nothing_announces_nothing() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-tasks-noop");

        assert!(
            ws.update_task(
                "proj",
                &TaskId::from("ghost"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                }
            )
            .expect("no refusal")
            .is_none(),
            "precondition: no task carries the id, so the update is refused",
        );
        assert!(
            ws.remove_task_tree("proj", &TaskId::from("ghost"))
                .expect("nothing in the tree waits on a look")
                .is_empty(),
            "precondition: nothing holds the id, so the delete is refused",
        );

        let announced = rx.try_recv();
        assert!(
            announced.is_err(),
            "a refused update and a refused delete announce nothing, and this was announced: \
             {announced:?}",
        );
    }

    /// A row waiting on the user's look, reached the way the gate produces
    /// one: a verify=user row completing.
    fn row_awaiting_look(ws: &Workspace, id: &str) {
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task(id, "proj") });
        ws.update_task("proj", &TaskId::from(id), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");
    }

    #[test]
    fn a_user_verdict_completes_with_by_user_in_the_history() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-approve");
        install_db(&ws, dir.path());
        row_awaiting_look(&ws, "t-1");

        let approved = ws
            .approve_task("proj", &TaskId::from("t-1"))
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(approved.status, TaskStatus::Completed, "approval completes the row");
        assert_eq!(approved.waiting_on, None, "and clears the wait");
        assert_eq!(approved.verify, Some(Verify::None), "the gate is satisfied, not re-armed");
        let history = history_of(&ws, "proj");
        let last = history.last().expect("a last transition");
        assert_eq!((last.from, last.to), (Some(TaskStatus::Waiting), TaskStatus::Completed));
        assert_eq!(last.by, By::User, "the verdict is stamped as the user's own move");
    }

    /// The lead's attestation completes a look wait the lead watched the
    /// user approve: the quoted words land on the row, the gate is
    /// satisfied, and the history stamps the lead's own move.
    #[test]
    fn a_lead_attests_the_look_with_the_quoted_approval() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-attest");
        install_db(&ws, dir.path());
        row_awaiting_look(&ws, "t-1");
        let lead = SessionSlot::lead("TestOrg", "proj");

        let attested = ws
            .attest_task("proj", &TaskId::from("t-1"), &lead, "looks good, merge it")
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(attested.status, TaskStatus::Completed, "the attestation completes the row");
        assert_eq!(attested.waiting_on, None, "and clears the wait");
        assert_eq!(attested.verify, Some(Verify::None), "the gate is satisfied, not re-armed");
        let mark = attested.attested.as_ref().expect("the row carries the attestation");
        assert_eq!(mark.evidence, "looks good, merge it", "the quote is the record");
        assert!(
            attested.detail.as_deref().unwrap_or_default().contains("looks good, merge it"),
            "the quote lands on the row: {:?}",
            attested.detail,
        );
        let history = history_of(&ws, "proj");
        let last = history.last().expect("a last transition");
        assert_eq!((last.from, last.to), (Some(TaskStatus::Waiting), TaskStatus::Completed));
        assert_eq!(last.by, By::Seat(lead), "the attestation is the lead's own move");
    }

    /// Only the project's lead stands behind the user's approval.
    #[test]
    fn a_worker_cannot_attest_a_look() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-attest-worker");
        row_awaiting_look(&ws, "t-1");
        let worker = SessionSlot::worker("TestOrg", "proj", "w1");

        let refused = ws
            .attest_task("proj", &TaskId::from("t-1"), &worker, "looks good")
            .expect_err("a worker cannot attest");
        assert_eq!(refused, MoveError::AttestLeadOnly);
        let stored = ws.tasks_for_project("proj");
        let row = stored.iter().find(|task| task.id == TaskId::from("t-1")).expect("t-1");
        assert_eq!(row.status, TaskStatus::Waiting, "the row stays parked");
        assert_eq!(row.attested, None, "and carries no attestation");
    }

    /// The quote is the whole point: an attestation stating nothing is
    /// refused rather than recorded as a blank claim.
    #[test]
    fn an_attestation_needs_the_quoted_approval() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-attest-empty");
        row_awaiting_look(&ws, "t-1");
        let lead = SessionSlot::lead("TestOrg", "proj");

        let refused = ws
            .attest_task("proj", &TaskId::from("t-1"), &lead, "   ")
            .expect_err("an empty approval is nothing to stand behind");
        assert_eq!(refused, MoveError::AttestNeedsEvidence);
        assert_eq!(ws.tasks_for_project("proj")[0].status, TaskStatus::Waiting);
    }

    /// A decision is the user's to answer; the lead's door is the look's
    /// only.
    #[test]
    fn a_decision_wait_is_not_the_leads_to_attest() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-attest-decision");
        ws.push_task(Task {
            status: TaskStatus::Waiting,
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::User),
                detail: Some("which shape".to_owned()),
                on: None,
                verification: false,
                parked: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: None,
            }),
            ..sample_task("t-1", "proj")
        });
        let lead = SessionSlot::lead("TestOrg", "proj");

        let refused = ws
            .attest_task("proj", &TaskId::from("t-1"), &lead, "he said yes to something")
            .expect_err("a decision is not the lead's to attest");
        assert_eq!(refused, MoveError::NotWaitingVerification);
        assert_eq!(ws.tasks_for_project("proj")[0].status, TaskStatus::Waiting);
    }

    /// The user strikes an attestation: the row parks on the look again,
    /// the gate is re-minted, and the history records the user's move. A
    /// child row, so the completion stays live - an attested ROOT closes
    /// its tree as the user's own approve does.
    #[test]
    fn the_users_reopen_strikes_the_attestation_and_parks_the_row() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-reopen");
        install_db(&ws, dir.path());
        ws.push_task(sample_task("epic", "proj"));
        ws.push_task(Task {
            verify: Some(Verify::User),
            parent: Some(TaskId::from("epic")),
            ..sample_task("t-1", "proj")
        });
        ws.update_task("proj", &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");
        let lead = SessionSlot::lead("TestOrg", "proj");
        ws.attest_task("proj", &TaskId::from("t-1"), &lead, "looks good")
            .expect("no refusal")
            .expect("the row is there");

        let reopened = ws
            .reopen_task("proj", &TaskId::from("t-1"))
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(reopened.status, TaskStatus::Waiting, "the row is back on the look");
        assert_eq!(reopened.attested, None, "the attestation is struck");
        assert_eq!(reopened.verify, Some(Verify::User), "the gate is re-minted");
        assert!(
            reopened.waiting_on.as_ref().is_some_and(|wait| wait.verification),
            "and parked on the user: {:?}",
            reopened.waiting_on,
        );
        assert!(
            reopened.waiting_on.as_ref().and_then(|wait| wait.since).is_some(),
            "and the wait starts at the reopen, not whenever it was first parked: {:?}",
            reopened.waiting_on,
        );
        let history = history_of(&ws, "proj");
        let last = history.last().expect("a last transition");
        assert_eq!((last.from, last.to), (Some(TaskStatus::Completed), TaskStatus::Waiting));
        assert_eq!(last.by, By::User, "the reopen is the user's own move");
    }

    /// A row with no attestation has nothing to reopen.
    #[test]
    fn a_reopen_without_an_attestation_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-reopen-none");
        row_awaiting_look(&ws, "t-1");

        let refused =
            ws.reopen_task("proj", &TaskId::from("t-1")).expect_err("nothing was attested");
        assert_eq!(refused, MoveError::NotAttested);
        assert_eq!(ws.tasks_for_project("proj")[0].status, TaskStatus::Waiting);
    }

    /// The user drags a row across the board: the state moves, the wait the
    /// row was holding drops, and the move is stamped as the user's own.
    #[test]
    fn a_user_move_moves_the_row_and_clears_its_wait() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        install_db(&ws, dir.path());
        let mut waiting = sample_task("t-1", "forge");
        waiting.status = TaskStatus::Waiting;
        waiting.waiting_on = Some(Waiting {
            kind: Some(WaitingKind::Dependency),
            detail: None,
            on: None,
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        });
        ws.push_task(waiting);

        let moved = ws
            .user_move_task("forge", &TaskId::from("t-1"), TaskStatus::InProgress)
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(moved.status, TaskStatus::InProgress, "the drag moved the state");
        assert_eq!(moved.waiting_on, None, "and dropped the wait it was holding");
        let history = history_of(&ws, "forge");
        let last = history.last().expect("a last transition");
        assert_eq!((last.from, last.to), (Some(TaskStatus::Waiting), TaskStatus::InProgress));
        assert_eq!(last.by, By::User, "the drag is the user's own move");
    }

    /// The gate holds for the user's own drag too: completing a verify=user
    /// row lands it in waiting for a look rather than done.
    #[test]
    fn a_user_move_still_takes_the_verify_gate() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("forge", "/tmp/tp-verify-gate");
        let mut row = sample_task("t-1", "forge");
        row.verify = Some(Verify::User);
        ws.push_task(row);

        let landed = ws
            .user_move_task("forge", &TaskId::from("t-1"), TaskStatus::Completed)
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(landed.status, TaskStatus::Waiting, "the gate lands it in waiting");
        assert!(
            landed.waiting_on.as_ref().is_some_and(|wait| wait.verification),
            "as a verification wait"
        );
    }

    #[test]
    fn a_send_back_returns_the_row_with_the_reason() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-sendback");
        install_db(&ws, dir.path());
        row_awaiting_look(&ws, "t-1");

        let sent = ws
            .send_back_task("proj", &TaskId::from("t-1"), "the rail elides wrong")
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(sent.status, TaskStatus::InProgress, "back to its owner's hands");
        assert_eq!(
            sent.detail.as_deref(),
            Some("the rail elides wrong"),
            "and the words are in the detail",
        );
    }

    /// A wordless send-back appends nothing: an empty detail is no detail,
    /// and a `Some("")` there would read as an empty ask on the card.
    #[test]
    fn a_wordless_send_back_leaves_the_detail_untouched() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-sendback-blank");
        row_awaiting_look(&ws, "t-1");

        let sent = ws
            .send_back_task("proj", &TaskId::from("t-1"), "   ")
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(sent.detail, None, "a blank send-back writes no detail");
    }

    #[test]
    fn an_answer_resumes_a_question_wait() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-answer");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        ws.wait_request_task_at_version(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            None,
            Request {
                kind: AskKind::Clarification,
                question: "which store should the window draw from?".to_owned(),
                options: Vec::new(),
                class: None,
                answer: None,
            },
            None,
            None,
        )
        .expect("ask");

        let answered = ws
            .answer_decision(
                "proj",
                &TaskId::from("t-1"),
                By::User,
                AnswerValue::Text("keep the window raise".to_owned()),
                None,
                None,
            )
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(answered.status, TaskStatus::InProgress, "the answer resumes the row");
        assert_eq!(answered.waiting_on, None);
        let record = answered
            .request
            .as_ref()
            .and_then(|request| request.answer.as_ref())
            .expect("the answer is the record");
        assert_eq!(record.value, AnswerValue::Text("keep the window raise".to_owned()));
    }

    /// A row written before the record existed carries no request: the
    /// value still answers it, landing in the detail as it always did.
    #[test]
    fn a_pre_record_question_wait_takes_the_value_into_its_detail() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-answer-legacy");
        ws.push_task(Task {
            status: TaskStatus::Waiting,
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::User),
                detail: Some("keep the window raise".to_owned()),
                on: None,
                verification: false,
                parked: false,
                since: Some(SystemTime::now()),
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
            }),
            ..sample_task("t-1", "proj")
        });

        let answered = ws
            .answer_decision(
                "proj",
                &TaskId::from("t-1"),
                By::User,
                AnswerValue::Text("keep the window raise".to_owned()),
                None,
                None,
            )
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(answered.status, TaskStatus::InProgress, "the answer resumes the row");
        assert_eq!(answered.detail.as_deref(), Some("keep the window raise"));
    }

    /// The digest's counts carry the rows behind them: an answered ask is
    /// one moved item naming its answer, and an expired ask keeps its
    /// record on the expired line rather than reading as a bare cancel.
    #[test]
    fn the_board_delta_carries_the_items_its_counts_count() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-delta-items");
        install_db(&ws, dir.path());
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        let since = SystemTime::now();
        let raise = |id: &str, kind: AskKind| {
            ws.push_task(Task { owner: Some(worker.clone()), ..sample_task(id, "proj") });
            ws.wait_request_task_at_version(
                "proj",
                &TaskId::from(id),
                &worker,
                None,
                Request {
                    kind,
                    question: "what now?".to_owned(),
                    options: Vec::new(),
                    class: None,
                    answer: None,
                },
                None,
                None,
            )
            .expect("the ask lands");
        };

        raise("answered", AskKind::Clarification);
        ws.answer_decision(
            "proj",
            &TaskId::from("answered"),
            By::User,
            AnswerValue::Text("the scratch store".to_owned()),
            None,
            None,
        )
        .expect("the answer lands")
        .expect("the row is there");
        raise("expired", AskKind::Decision);
        ws.expire_ask("proj", &TaskId::from("expired"), SystemTime::now(), "expired: no answer");

        let delta = ws.board_delta("proj", Some(since));
        let moved = delta
            .moved_items
            .iter()
            .find(|item| item.subject == "subject answered")
            .expect("the answered row is a moved item");
        assert!(
            moved.word.contains("answered") && moved.word.contains("the scratch store"),
            "the moved item names the answer: {moved:?}",
        );
        assert_eq!(delta.expired, 1, "one ask expired: {delta:?}");
        assert_eq!(delta.expired_items[0].subject, "subject expired");
        assert!(
            delta.expired_items[0].word.contains("expired"),
            "the expired item keeps its own word: {:?}",
            delta.expired_items[0],
        );
        assert!(delta.landed_items.iter().any(|item| item.subject == "subject expired"));
    }

    /// **A row the store archived is still a row that moved**: a tree close
    /// archives its rows at once and retention takes the rest, so a digest
    /// that read only the live set would report a finished epic as nothing.
    #[test]
    fn the_board_delta_counts_a_row_the_archive_holds() {
        use crate::store::{task_archive, task_history};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-delta-archived");
        install_db(&ws, dir.path());
        let since = SystemTime::now();
        let mut ended = sample_task("archived-1", "proj");
        ended.status = TaskStatus::Completed;
        ended.subject = "the epic that ended".to_owned();
        ended.updated_at = SystemTime::now();
        {
            let guard = ws.db.lock();
            let db = guard.as_ref().expect("db");
            task_archive::append(db, &[ended.clone()]).expect("archive the row");
            task_history::append(
                db,
                &[TaskTransition {
                    task_id: TaskId::from("archived-1"),
                    project_name: "proj".to_owned(),
                    from: Some(TaskStatus::InProgress),
                    to: TaskStatus::Completed,
                    at: SystemTime::now(),
                    by: By::System,
                }],
            )
            .expect("its transition");
        }
        let delta = ws.board_delta("proj", Some(since));
        assert_eq!(delta.landed, 1, "the archived row landed: {delta:?}");
        assert_eq!(delta.landed_items[0].subject, "the epic that ended");
    }

    /// A closed row's page is derived like a live one's: its worked time and
    /// its history come from the log, not from placeholders.
    #[test]
    fn a_closed_rows_detail_derives_its_worked_time_and_history() {
        let mut task = sample_task("done-1", "proj");
        task.status = TaskStatus::Completed;
        let epoch = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
        let history = [
            TaskTransition {
                task_id: TaskId::from("done-1"),
                project_name: "proj".to_owned(),
                from: Some(TaskStatus::Pending),
                to: TaskStatus::InProgress,
                at: epoch(0),
                by: By::Seat(SessionSlot::worker("TestOrg", "proj", "w-1")),
            },
            TaskTransition {
                task_id: TaskId::from("done-1"),
                project_name: "proj".to_owned(),
                from: Some(TaskStatus::InProgress),
                to: TaskStatus::Completed,
                at: epoch(100),
                by: By::Seat(SessionSlot::lead("TestOrg", "proj")),
            },
        ];
        let row = crate::board::detail_row(task, &history, epoch(200), false);
        assert_eq!(row.history_total, 2, "the drawer's whole trail: {row:?}");
        assert_eq!(row.history.len(), 2);
        assert_eq!(row.worked_secs, 100, "the worked time is the log's, not zero");
    }

    /// **Every park on the user is an ask or a look**: a bare `user` wait
    /// has no door to draw, so the wait door refuses it by name.
    #[test]
    fn a_bare_wait_on_the_user_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-bare-user-wait");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        assert_eq!(
            ws.wait_task("proj", &TaskId::from("t-1"), &worker, WaitingKind::User, None, None),
            Err(MoveError::BareUserWait),
        );
    }

    /// A decision wait records the question and the options the lead
    /// enumerated, and the answer is the record: what was chosen, when and
    /// the note, with the row resumed.
    #[test]
    fn a_decision_wait_records_its_question_and_the_answer_is_the_record() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-decision");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });

        let waited = ws
            .wait_decision_task(
                "proj",
                &TaskId::from("t-1"),
                &worker,
                Some("why it needs the user".to_owned()),
                "Ship the parser behind a flag?".to_owned(),
                vec!["flag it".to_owned(), "ship it outright".to_owned()],
            )
            .expect("the owner may wait its own row");
        let request = waited.request.expect("the ask rides the row");
        assert_eq!(request.question, "Ship the parser behind a flag?");
        assert_eq!(request.options, ["flag it", "ship it outright"]);
        assert_eq!(request.answer, None, "a waiting row has no answer yet");
        assert_eq!(waited.status, TaskStatus::Waiting);

        let answered = ws
            .answer_decision(
                "proj",
                &TaskId::from("t-1"),
                By::User,
                AnswerValue::Choice("flag it".to_owned()),
                Some("until the bench is green"),
                None,
            )
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(answered.status, TaskStatus::InProgress, "the answer resumes the row");
        assert_eq!(answered.waiting_on, None, "and clears the wait");
        let answer = answered.request.expect("the record keeps the ask").answer.expect("answered");
        assert_eq!(answer.value_words(), "flag it");
        assert_eq!(answer.note.as_deref(), Some("until the bench is green"));
    }

    /// A choice the row did not offer is refused by name; answering is not
    /// a way to put words in the options list.
    #[test]
    fn an_answer_naming_a_choice_the_row_does_not_offer_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-decision-refused");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.wait_decision_task(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            None,
            "Which shape?".to_owned(),
            vec!["small".to_owned(), "large".to_owned()],
        )
        .expect("the wait lands");

        assert_eq!(
            ws.answer_decision(
                "proj",
                &TaskId::from("t-1"),
                By::User,
                AnswerValue::Choice("huge".to_owned()),
                None,
                None,
            ),
            Err(MoveError::NotAnOption("huge".to_owned())),
            "a choice the row does not offer is refused",
        );
        assert_eq!(
            ws.tasks_for_project("proj")[0].status,
            TaskStatus::Waiting,
            "and the row stays waiting for the user",
        );
    }

    /// An answer is the decision's own door: a resource or dependency wait
    /// is resumed by a move, not by recording a choice nobody asked for.
    #[test]
    fn an_answer_is_refused_on_a_wait_that_is_not_a_decision() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-answer-gate");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.wait_task(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            WaitingKind::Resource,
            Some("the CI run".to_owned()),
            None,
        )
        .expect("the wait lands");

        assert_eq!(
            ws.answer_decision(
                "proj",
                &TaskId::from("t-1"),
                By::User,
                AnswerValue::Choice("ship it".to_owned()),
                None,
                None,
            ),
            Err(MoveError::NotWaitingQuestion),
            "a resource wait takes no choice",
        );
        assert_eq!(
            ws.tasks_for_project("proj")[0].status,
            TaskStatus::Waiting,
            "and the row stays waiting",
        );
    }

    /// An answered decision is the only copy of what was asked and what
    /// was chosen: the delete door refuses it, so a cancel - which keeps
    /// the row - is the way to end the work.
    #[test]
    fn a_delete_does_not_erase_an_answered_decision() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-decision-record");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.wait_decision_task(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            None,
            "Which shape?".to_owned(),
            vec!["small".to_owned(), "large".to_owned()],
        )
        .expect("the wait lands");
        ws.answer_decision(
            "proj",
            &TaskId::from("t-1"),
            By::User,
            AnswerValue::Choice("small".to_owned()),
            Some("it fits"),
            None,
        )
        .expect("the answer lands")
        .expect("the row is there");

        let refused = ws
            .remove_task_tree("proj", &TaskId::from("t-1"))
            .expect_err("the record is the only copy");
        assert!(
            matches!(&refused, MoveError::DeletingDecision(subject) if subject.contains("t-1")),
            "the refusal names the row: {refused}",
        );
        let rows = ws.tasks_for_project("proj");
        assert_eq!(rows.len(), 1, "the row and its record survive");
        assert_eq!(
            rows[0]
                .request
                .as_ref()
                .and_then(|request| request.answer.as_ref().map(Answer::value_words))
                .as_deref(),
            Some("small"),
            "the answer is still readable",
        );
    }

    /// The ask kinds take one value each: text for a clarification and a
    /// teaching, an enumerated disposition for an exception. A choice
    /// cannot answer a clarification, and a teaching writes the policy note
    /// its class is settled by - a second ask of that class is refused.
    #[test]
    fn each_ask_kind_takes_its_own_value_and_a_teaching_settles_its_class() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        install_db(&ws, dir.path());
        ws.seed_test_project("proj", "/tmp/tp-ask-kinds");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        let raise = |id: &str, request: Request| {
            ws.push_task(Task { owner: Some(worker.clone()), ..sample_task(id, "proj") });
            ws.wait_request_task_at_version(
                "proj",
                &TaskId::from(id),
                &worker,
                None,
                request,
                None,
                None,
            )
            .expect("the ask lands");
        };

        raise(
            "clarify",
            Request {
                kind: AskKind::Clarification,
                question: "Which account is the staging one?".to_owned(),
                options: Vec::new(),
                class: None,
                answer: None,
            },
        );
        assert!(
            matches!(
                ws.answer_decision(
                    "proj",
                    &TaskId::from("clarify"),
                    By::User,
                    AnswerValue::Choice("Granite".to_owned()),
                    None,
                    None,
                ),
                Err(MoveError::AnswerShape(_)),
            ),
            "an option list cannot answer a clarification",
        );
        let answered = ws
            .answer_decision(
                "proj",
                &TaskId::from("clarify"),
                By::User,
                AnswerValue::Text("Granite".to_owned()),
                None,
                None,
            )
            .expect("the answer lands")
            .expect("the row is there");
        assert_eq!(
            answered.request.expect("the record").answer.expect("answered").value_words(),
            "Granite",
        );

        raise(
            "fault",
            Request {
                kind: AskKind::Exception,
                question: "The flake is back: what now?".to_owned(),
                options: Vec::new(),
                class: None,
                answer: None,
            },
        );
        assert!(
            matches!(
                ws.answer_decision(
                    "proj",
                    &TaskId::from("fault"),
                    By::User,
                    AnswerValue::Text("retry".to_owned()),
                    None,
                    None,
                ),
                Err(MoveError::AnswerShape(_)),
            ),
            "an exception takes an enumerated disposition, not words",
        );
        ws.answer_decision(
            "proj",
            &TaskId::from("fault"),
            By::User,
            AnswerValue::Disposition(Disposition::Retry),
            Some("once, then report"),
            None,
        )
        .expect("the disposition lands")
        .expect("the row is there");

        let lesson = "Report the run; never retry a flake twice.";
        raise(
            "teach",
            Request {
                kind: AskKind::Teaching,
                question: "Which rule settles flaky CI retries?".to_owned(),
                options: Vec::new(),
                class: Some("ci-flake".to_owned()),
                answer: None,
            },
        );
        let taught = ws
            .answer_decision(
                "proj",
                &TaskId::from("teach"),
                By::User,
                AnswerValue::Text(lesson.to_owned()),
                None,
                Some(lesson),
            )
            .expect("the teaching lands")
            .expect("the row is there");
        assert_eq!(
            taught
                .request
                .as_ref()
                .and_then(|request| request.answer.as_ref())
                .and_then(|answer| answer.lesson.as_deref()),
            Some(lesson),
            "the lesson is persisted on the row, not only in the policy note",
        );
        let notes = ws.policy_notes_for_project("proj");
        assert_eq!(notes.len(), 1, "the lesson is a project policy note: {notes:?}");
        assert_eq!(notes[0].class, "ci-flake");
        assert_eq!(notes[0].text, lesson);

        // The class is settled: raising it again is refused, naming the note.
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("teach-again", "proj") });
        let refused = ws
            .wait_request_task_at_version(
                "proj",
                &TaskId::from("teach-again"),
                &worker,
                None,
                Request {
                    kind: AskKind::Teaching,
                    question: "Same trouble again?".to_owned(),
                    options: Vec::new(),
                    class: Some("ci-flake".to_owned()),
                    answer: None,
                },
                None,
                None,
            )
            .expect_err("a settled class is not raised again");
        assert!(
            matches!(&refused, MoveError::PolicySettled { class, text }
                if class == "ci-flake" && text.contains("never retry")),
            "the refusal quotes the note: {refused}",
        );
    }

    /// The item states: acknowledge is not an answer, a snooze is quiet
    /// until its time or the row's next move, and an expiry ends the row
    /// canceled while the ask's record stays on it.
    #[test]
    fn an_ask_acknowledges_snoozes_and_expires_with_its_record_kept() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        install_db(&ws, dir.path());
        ws.seed_test_project("proj", "/tmp/tp-ask-states");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.wait_request_task_at_version(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            None,
            Request {
                kind: AskKind::Clarification,
                question: "Which account?".to_owned(),
                options: Vec::new(),
                class: None,
                answer: None,
            },
            None,
            None,
        )
        .expect("the ask lands");

        let acknowledged =
            ws.acknowledge_task("proj", &TaskId::from("t-1")).expect("the ack lands").expect("row");
        assert_eq!(acknowledged.status, TaskStatus::Waiting, "acknowledged is not answered");
        let wait = acknowledged.waiting_on.as_ref().expect("still waiting");
        assert!(wait.acknowledged_at.is_some(), "the seen stamp is recorded");

        let snoozed = ws
            .snooze_task(
                "proj",
                &TaskId::from("t-1"),
                forge_primitives::tasks::SnoozeUntil::Activity,
            )
            .expect("the snooze lands")
            .expect("row");
        let wait = snoozed.waiting_on.as_ref().expect("still waiting");
        assert!(wait.snoozed.is_some(), "the deferral is recorded");
        assert!(
            snooze_active(wait, snoozed.updated_at, SystemTime::now()),
            "an activity snooze holds until the row moves",
        );
        assert!(
            !snooze_active(
                wait,
                snoozed.updated_at + std::time::Duration::from_secs(1),
                SystemTime::now()
            ),
            "a touch ends it",
        );

        let expired = ws
            .expire_ask("proj", &TaskId::from("t-1"), SystemTime::now(), "expired: no answer")
            .expect("the row is there");
        assert_eq!(expired.status, TaskStatus::Canceled, "expiry ends the row canceled");
        let wait = expired.waiting_on.as_ref().expect("the wait's record is kept");
        assert!(wait.expired_at.is_some(), "and says when it expired");
        assert!(expired.request.is_some(), "the ask itself is kept");
    }

    #[test]
    fn a_verdict_on_a_row_not_waiting_for_look_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-refuse-verdict");
        ws.push_task(sample_task("t-1", "proj"));

        assert_eq!(
            ws.approve_task("proj", &TaskId::from("t-1")),
            Err(MoveError::NotWaitingVerification),
            "a pending row is not waiting on a look",
        );
        assert_eq!(
            ws.answer_decision(
                "proj",
                &TaskId::from("t-1"),
                By::User,
                AnswerValue::Text("words".to_owned()),
                None,
                None,
            ),
            Err(MoveError::NotWaitingQuestion),
        );
    }

    #[test]
    fn rank_up_swaps_against_the_neighbour() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-rank");
        for (id, rank) in [("first", 0), ("second", 1), ("third", 2)] {
            ws.push_task(Task { rank: Some(rank), ..sample_task(id, "proj") });
        }

        let order = || -> Vec<String> {
            let mut rows: Vec<(i64, String)> = ws
                .tasks_for_project("proj")
                .into_iter()
                .map(|t| (t.rank.unwrap_or(i64::MAX), t.id.as_str().to_owned()))
                .collect();
            rows.sort();
            rows.into_iter().map(|(_, id)| id).collect()
        };

        ws.rank_task("proj", &TaskId::from("third"), RankMove::Up).expect("no refusal");
        assert_eq!(
            order(),
            vec!["first", "third", "second"],
            "up moves the row one place toward the front, past its neighbour",
        );

        ws.rank_task("proj", &TaskId::from("third"), RankMove::Down).expect("no refusal");
        assert_eq!(
            order(),
            vec!["first", "second", "third"],
            "down moves it one place back, to where it started",
        );

        ws.rank_task("proj", &TaskId::from("third"), RankMove::Top).expect("no refusal");
        let top = ws
            .tasks_for_project("proj")
            .into_iter()
            .min_by_key(|t| t.rank.unwrap_or(i64::MAX))
            .expect("a row");
        assert_eq!(top.id, TaskId::from("third"), "top puts it above everything");
    }

    /// A drag's drop position is a rank: the row sits directly above the row
    /// it was dropped on, and a drop below everything is the end of the lane.
    #[test]
    fn a_drop_position_ranks_the_row_where_it_landed() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-drop");
        for (id, rank) in [("first", 0), ("second", 1), ("third", 2)] {
            ws.push_task(Task { rank: Some(rank), ..sample_task(id, "proj") });
        }

        let order = || -> Vec<String> {
            let mut rows: Vec<(i64, String)> = ws
                .tasks_for_project("proj")
                .into_iter()
                .map(|t| (t.rank.unwrap_or(i64::MAX), t.id.as_str().to_owned()))
                .collect();
            rows.sort();
            rows.into_iter().map(|(_, id)| id).collect()
        };

        // Dragged DOWN past its neighbours: the row it lands above is found
        // in the queue the drag leaves, so "first" sits directly above
        // "third" rather than at the end behind it.
        ws.rank_task("proj", &TaskId::from("first"), RankMove::Before(Some("third".to_owned())))
            .expect("no refusal");
        assert_eq!(
            order(),
            vec!["second", "first", "third"],
            "the drop lands directly above the row it was dropped on",
        );

        ws.rank_task("proj", &TaskId::from("second"), RankMove::Before(None)).expect("no refusal");
        assert_eq!(
            order(),
            vec!["first", "third", "second"],
            "and a drop below every card is the end of the queue",
        );
    }

    #[test]
    fn claim_sets_owner_and_in_progress_in_one_write() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim");
        install_db(&ws, dir.path());
        ws.push_task(sample_task("t-1", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");

        let claimed =
            ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("the row is free");
        assert_eq!(claimed.owner.as_ref().map(SessionSlot::label), Some("w-1"));
        assert_eq!(claimed.status, TaskStatus::InProgress, "owner and status in one write");
        assert_eq!(claimed.version, 1, "the claim is the row's first write");
        let history = history_of(&ws, "proj");
        assert_eq!(history.len(), 2, "creation and the claim: {history:?}");
        let claim = history.last().expect("the claim's transition");
        assert_eq!(claim.from, Some(TaskStatus::Pending));
        assert_eq!(claim.to, TaskStatus::InProgress);
        assert_eq!(claim.by, By::Seat(worker));
    }

    /// Every write the row takes moves its version, so an update naming
    /// the version it read can tell a lost write from a clean one. Row
    /// creation is version 0; nothing else leaves it where it was.
    #[test]
    fn every_write_bumps_the_version() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-version");
        install_db(&ws, dir.path());
        ws.push_task(sample_task("t-1", "proj"));
        let version = |ws: &Workspace| {
            ws.tasks_for_project("proj").into_iter().next().expect("the row").version
        };
        assert_eq!(version(&ws), 0, "creation is version 0");

        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.update_task("proj", &TaskId::from("t-1"), By::Seat(worker.clone()), |task| {
            task.active_form = Some("fixing the rail".to_owned());
        })
        .expect("no refusal")
        .expect("the row is there");
        assert_eq!(version(&ws), 1, "an update is a write");
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        assert_eq!(version(&ws), 2, "a claim is a write");
        ws.wait_task("proj", &TaskId::from("t-1"), &worker, WaitingKind::Resource, None, None)
            .expect("wait");
        assert_eq!(version(&ws), 3, "a wait is a write");
    }

    /// An update that names the version it read lands; one naming a version
    /// the row has left is refused with both numbers, and nothing moves.
    #[test]
    fn a_stale_version_is_refused_naming_both() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-version-mismatch");
        ws.push_task(sample_task("t-1", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");

        ws.update_task_at_version(
            "proj",
            &TaskId::from("t-1"),
            By::Seat(worker.clone()),
            Some(0),
            |task| {
                task.active_form = Some("the first write".to_owned());
            },
        )
        .expect("no refusal")
        .expect("the row is there");

        assert_eq!(
            ws.update_task_at_version(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(worker),
                Some(0),
                |task| task.active_form = Some("the lost write".to_owned()),
            ),
            Err(MoveError::VersionMismatch { expected: 0, current: 1 }),
            "a write naming the version it read before another landed is refused",
        );
        let stored = &ws.tasks_for_project("proj")[0];
        assert_eq!(stored.active_form.as_deref(), Some("the first write"), "nothing moved");
        assert_eq!(stored.version, 1, "and the refused write did not bump the version");
    }

    #[test]
    fn a_claim_of_a_live_seats_row_is_refused_by_name() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-live");
        live_worker(&ws, "proj", "w-1");
        let mut task = sample_task("t-1", "proj");
        task.owner = Some(SessionSlot::worker("TestOrg", "proj", "w-1"));
        ws.push_task(task);
        let other = SessionSlot::worker("TestOrg", "proj", "w-2");

        assert_eq!(
            ws.claim_task("proj", &other, &TaskId::from("t-1")),
            Err(ClaimError::OwnedByLiveSeat("w-1".to_owned())),
            "a live seat's row is refused by name",
        );
    }

    /// A dead owner's row is nobody's: a claim on it is the explicit
    /// re-dispatch the design asks for, not a race.
    #[test]
    fn a_dead_owners_row_can_be_reclaimed() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-dead");
        let mut task = sample_task("t-1", "proj");
        task.owner = Some(SessionSlot::worker("TestOrg", "proj", "gone"));
        ws.push_task(task);
        let worker = SessionSlot::worker("TestOrg", "proj", "w-2");

        let claimed = ws
            .claim_task("proj", &worker, &TaskId::from("t-1"))
            .expect("a dead owner does not hold a row");
        assert_eq!(claimed.owner.as_ref().map(SessionSlot::label), Some("w-2"));
    }

    #[test]
    fn a_second_in_progress_row_for_one_worker_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-wip");
        ws.push_task(sample_task("t-1", "proj"));
        ws.push_task(sample_task("t-2", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("first claim");

        assert_eq!(
            ws.claim_task("proj", &worker, &TaskId::from("t-2")),
            Err(ClaimError::AlreadyWorking),
            "one in progress per worker",
        );
    }

    /// A pick reserved for the user is nobody's to claim (#2423/C).
    #[test]
    fn a_reserved_pick_is_not_claimable() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-reserved");
        ws.push_task(Task { reserved: Some(ReservedBy::User), ..sample_task("kept", "proj") });
        ws.push_task(sample_task("free", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");

        assert_eq!(
            ws.claim_task("proj", &worker, &TaskId::from("kept")),
            Err(ClaimError::Reserved),
            "a reserved row is the user's pick, refused by name",
        );
        let claimed =
            ws.claim_task("proj", &worker, &TaskId::from("free")).expect("the free row is there");
        assert_eq!(claimed.id, TaskId::from("free"), "the unreserved row is claimable");
    }

    /// The reservation is the user's to set and release, and the write
    /// returns the row as it stands after it.
    #[test]
    fn a_pick_is_reserved_and_released() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-reserve");
        ws.push_task(sample_task("t-1", "proj"));

        let reserved = ws
            .reserve_task("proj", &TaskId::from("t-1"), true)
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(reserved.reserved, Some(ReservedBy::User), "the pick is the user's");
        assert_eq!(
            ws.tasks_for_project("proj")[0].reserved,
            Some(ReservedBy::User),
            "and the write persists",
        );

        let released = ws
            .reserve_task("proj", &TaskId::from("t-1"), false)
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(released.reserved, None, "releasing clears it");
    }

    /// A worker is handed a row by the short id a person writes, so an
    /// id a caller states resolves to the row it names: exactly when the
    /// id is complete, and by unique prefix otherwise. Two rows sharing
    /// the prefix is a refusal that names them - not the "no such row"
    /// that sends the caller back to the board to copy a uuid.
    #[test]
    fn a_claim_resolves_a_stated_id_by_exact_or_unique_prefix() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-prefix");
        ws.push_task(named_task("7ed0aaaa-0000-4000-8000-000000000001", "fix the rail", "proj"));
        ws.push_task(named_task("7ed0bbbb-0000-4000-8000-000000000002", "add tests", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");

        let refused = ws
            .claim_task("proj", &worker, &TaskId::from("7ed0"))
            .expect_err("two rows carry that prefix");
        let said = refused.to_string();
        assert!(said.contains("7ed0aaaa-0000-4000-8000-000000000001"), "one row is named: {said}");
        assert!(said.contains("7ed0bbbb-0000-4000-8000-000000000002"), "and the other: {said}");

        let unknown = ws
            .claim_task("proj", &worker, &TaskId::from("ffff"))
            .expect_err("no row carries that prefix");
        assert!(
            unknown.to_string().contains("no such row"),
            "an id naming no row answers as it always did: {unknown}",
        );

        let claimed = ws
            .claim_task("proj", &worker, &TaskId::from("7ed0bb"))
            .expect("one row carries that prefix");
        assert_eq!(
            claimed.id,
            TaskId::from("7ed0bbbb-0000-4000-8000-000000000002"),
            "the prefix landed on its row",
        );
    }

    #[test]
    fn an_update_resolves_a_stated_id_by_exact_or_unique_prefix() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-update-prefix");
        // Child rows: completing a ROOT closes and archives, which is its
        // own test.
        ws.push_task(Task {
            parent: Some(TaskId::from("epic")),
            ..named_task("7ed0aaaa-0000-4000-8000-000000000001", "fix the rail", "proj")
        });
        ws.push_task(Task {
            parent: Some(TaskId::from("epic")),
            ..named_task("7ed0bbbb-0000-4000-8000-000000000002", "add tests", "proj")
        });
        let seat = By::Seat(SessionSlot::lead("TestOrg", "proj"));

        let refused = ws
            .update_task("proj", &TaskId::from("7ed0"), seat.clone(), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("two rows carry that prefix");
        let said = refused.to_string();
        assert!(
            said.contains("7ed0aaaa-0000-4000-8000-000000000001")
                && said.contains("7ed0bbbb-0000-4000-8000-000000000002"),
            "the refusal names both rows it matched: {said}",
        );

        let moved = ws
            .update_task("proj", &TaskId::from("7ed0bb"), seat, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(moved.id, TaskId::from("7ed0bbbb-0000-4000-8000-000000000002"));
        assert_eq!(moved.status, TaskStatus::Completed, "the prefix moved its own row");
    }

    #[test]
    fn a_wait_resolves_a_stated_id_by_exact_or_unique_prefix() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-wait-prefix");
        ws.push_task(named_task("7ed0aaaa-0000-4000-8000-000000000001", "fix the rail", "proj"));
        ws.push_task(named_task("7ed0bbbb-0000-4000-8000-000000000002", "add tests", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.claim_task("proj", &worker, &TaskId::from("7ed0bb")).expect("claim by prefix");

        // The blocker resolves the same way, and the full id is what
        // lands in the wait - the release matches it exactly.
        let waited = ws
            .wait_task(
                "proj",
                &TaskId::from("7ed0bb"),
                &worker,
                WaitingKind::Dependency,
                None,
                Some(&TaskId::from("7ed0aa")),
            )
            .expect("the owner may wait its own row");
        assert_eq!(waited.id, TaskId::from("7ed0bbbb-0000-4000-8000-000000000002"));
        assert_eq!(
            waited.waiting_on.as_ref().and_then(|wait| wait.on.clone()),
            Some(TaskId::from("7ed0aaaa-0000-4000-8000-000000000001")),
            "the blocker is stored as the row its prefix named",
        );

        let refused = ws
            .wait_task("proj", &TaskId::from("7ed0"), &worker, WaitingKind::User, None, None)
            .expect_err("two rows carry that prefix");
        assert!(
            refused.to_string().contains("7ed0bbbb-0000-4000-8000-000000000002"),
            "the refusal names the rows it matched: {refused}",
        );
    }

    /// The row's owner reads before its blocker does: a caller waiting a
    /// row that is not its own hears that, whatever it puts in `on`.
    #[test]
    fn a_wait_checks_ownership_before_it_resolves_the_blocker() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-wait-owner-first");
        ws.push_task(sample_task("t-1", "proj"));
        let holder = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.claim_task("proj", &holder, &TaskId::from("t-1")).expect("claim");

        let other = SessionSlot::worker("TestOrg", "proj", "w-2");
        assert_eq!(
            ws.wait_task(
                "proj",
                &TaskId::from("t-1"),
                &other,
                WaitingKind::Dependency,
                None,
                Some(&TaskId::from("ghost")),
            ),
            Err(MoveError::NotOwner),
            "the refusal is about the row, not the blocker it named",
        );
    }

    /// An exact id is a name, not a prefix of somebody else's: the row it
    /// spells wins over the rows it merely starts.
    #[test]
    fn an_exact_id_wins_over_a_longer_id_it_prefixes() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-exact-wins");
        ws.push_task(named_task("7ed0", "the exact row", "proj"));
        ws.push_task(named_task("7ed0bbbb-0000-4000-8000-000000000002", "the longer row", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");

        let claimed = ws
            .claim_task("proj", &worker, &TaskId::from("7ed0"))
            .expect("the exact id names its row");
        assert_eq!(claimed.id, TaskId::from("7ed0"), "the exact row, not the one it prefixes");
    }

    #[test]
    fn a_waiting_row_is_not_claimable_until_its_wait_resolves() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-claim-wait");
        ws.push_task(sample_task("blocker", "proj"));
        ws.push_task(sample_task("t-2", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.claim_task("proj", &worker, &TaskId::from("t-2"))
            .expect("claim the row before waiting it");
        ws.wait_task(
            "proj",
            &TaskId::from("t-2"),
            &worker,
            WaitingKind::Dependency,
            Some("the board read has to land first".to_owned()),
            Some(&TaskId::from("blocker")),
        )
        .expect("the owner may wait its own row");
        assert_eq!(
            ws.claim_task("proj", &worker, &TaskId::from("t-2")),
            Err(ClaimError::Waiting),
            "a waiting row is in play, not startable",
        );

        // The blocker ending - canceled, not completed - clears the wait.
        ws.update_task(
            "proj",
            &TaskId::from("blocker"),
            By::Seat(SessionSlot::lead("TestOrg", "proj")),
            |task| task.status = TaskStatus::Canceled,
        )
        .expect("no refusal")
        .expect("the blocker is there");
        let t2 = ws.tasks_for_project("proj").into_iter().find(|t| t.id == TaskId::from("t-2"));
        let t2 = t2.expect("t-2 is live");
        assert_eq!(t2.status, TaskStatus::Pending, "a canceled blocker still clears the wait");
        assert_eq!(t2.waiting_on, None);
        ws.claim_task("proj", &worker, &TaskId::from("t-2"))
            .expect("and the row is claimable again");
    }

    /// The live shape: a worker parks its row on a decision wait - the
    /// answer is the user's look, typically - and another seat completes
    /// the row once that answer is in hand. The completion applies, and
    /// the wait clears as its consequence.
    #[test]
    fn another_seat_completes_a_row_parked_on_a_decision_wait() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-complete-parked");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        ws.wait_request_task_at_version(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            Some("the user's look".to_owned()),
            Request {
                kind: AskKind::Clarification,
                question: "the user's look".to_owned(),
                options: Vec::new(),
                class: None,
                answer: None,
            },
            None,
            None,
        )
        .expect("the owner may wait its own row");

        let moved = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(moved.status, TaskStatus::Completed, "the completion applies as asked");
        assert_eq!(moved.waiting_on, None, "and the wait clears as its consequence");
    }

    /// A row already waiting on the user's own look cannot be completed
    /// through `tasks__update` - the look is what completes it - and the
    /// refusal names the wait it sits on and what clears it, rather than
    /// answering with the row still waiting as though the completion had
    /// applied.
    #[test]
    fn completing_a_verification_wait_is_refused_naming_the_look() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-refuse-look");
        row_awaiting_look(&ws, "t-1");

        let refused = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect_err("the user's look is what completes a verification wait");
        assert_eq!(
            refused,
            MoveError::WaitingOnLook,
            "the look's own refusal, not one that merely mentions a look",
        );

        let stored = ws.tasks_for_project("proj");
        assert_eq!(stored[0].status, TaskStatus::Waiting, "and the row keeps waiting for the look");
    }

    /// The reported sequence, not just the seeded state (#2414): a worker
    /// parks its row on a dependency wait through `tasks__wait`, then
    /// another seat completes it. On a verify=user row the completion is
    /// refused by name, naming the wait, and the dependency wait survives
    /// untouched.
    #[test]
    fn completing_a_waiting_user_bound_row_is_refused_by_name() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-complete-waiting");
        ws.push_task(sample_task("blocker", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task {
            verify: Some(Verify::User),
            owner: Some(worker.clone()),
            ..sample_task("t-1", "proj")
        });
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        ws.wait_task(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            WaitingKind::Dependency,
            Some("the board read has to land first".to_owned()),
            Some(&TaskId::from("blocker")),
        )
        .expect("the owner may wait its own row");

        let refused = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect_err("the gate would trade the dependency wait for a look park");
        assert_eq!(
            refused,
            MoveError::CompletingWhileWaiting("a dependency on blocker".to_owned()),
            "the refusal names the wait the row sits on",
        );
        assert!(
            refused.to_string().contains("a dependency on blocker"),
            "and the words a caller reads name it too: {refused}",
        );
        let stored = ws.tasks_for_project("proj");
        let row = stored.iter().find(|task| task.id == TaskId::from("t-1")).expect("the row");
        assert_eq!(row.status, TaskStatus::Waiting, "the row keeps the status it held");
        let held = row.waiting_on.as_ref().expect("the wait it held");
        assert_eq!(
            (held.kind, held.detail.as_deref(), held.on.as_ref(), held.verification, held.parked),
            (
                Some(WaitingKind::Dependency),
                Some("the board read has to land first"),
                Some(&TaskId::from("blocker")),
                false,
                false
            ),
            "and the wait it held, not a look park the caller never asked for",
        );
        assert!(held.since.is_some(), "the stored wait keeps its own start");
    }

    /// The recovery the refusal advertises (#2414): moving the row off
    /// waiting drops the dependency wait, and the next completion parks it
    /// on the look the way any user-bound completion does.
    #[test]
    fn moving_a_dependency_wait_off_waiting_then_completing_parks_on_the_look() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-move-then-complete");
        ws.push_task(sample_task("blocker", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task {
            verify: Some(Verify::User),
            owner: Some(worker.clone()),
            ..sample_task("t-1", "proj")
        });
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        ws.wait_task(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            WaitingKind::Dependency,
            None,
            Some(&TaskId::from("blocker")),
        )
        .expect("the owner may wait its own row");
        let lead = SessionSlot::lead("TestOrg", "proj");

        let moved = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(lead.clone()), |task| {
                task.status = TaskStatus::InProgress;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(moved.status, TaskStatus::InProgress, "the move off waiting lands");
        assert_eq!(moved.waiting_on, None, "and drops the dependency wait");

        let landed = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(lead), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(landed.status, TaskStatus::Waiting, "the gate parks the completion");
        assert!(
            landed.waiting_on.as_ref().is_some_and(|wait| wait.verification),
            "on the user's look: {landed:?}",
        );
    }

    /// The refusal is a seat's, not a state's (#2414): the user's own move
    /// of a waiting verify=user row to completed still parks it on the
    /// look, which is the path the refusal tells a seat to take.
    #[test]
    fn a_user_move_still_parks_a_waiting_user_bound_row() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-user-move-waiting");
        ws.push_task(Task {
            verify: Some(Verify::User),
            status: TaskStatus::Waiting,
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::Dependency),
                detail: None,
                on: None,
                verification: false,
                parked: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: None,
            }),
            ..sample_task("t-1", "proj")
        });

        let landed = ws
            .user_move_task("proj", &TaskId::from("t-1"), TaskStatus::Completed)
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(landed.status, TaskStatus::Waiting, "the drag lands as a look park");
        let wait = landed.waiting_on.as_ref().expect("the re-parked look");
        assert!(wait.verification, "with the look the user's own completion is owed: {landed:?}");
        assert!(
            wait.since.is_some(),
            "a re-parked look on a row already waiting is stamped, so its age and a \
             notification's identity are the new wait's: {landed:?}",
        );
    }

    /// **A pre-upgrade look park is stamped when it is rebuilt.** A row
    /// stored before `since` existed holds a value-identical verification
    /// park with no stamp; the drag to completed rebuilds it, and the
    /// construction site is where the new start is written - a comparison
    /// against the old value could not tell the two apart.
    #[test]
    fn a_pre_upgrade_look_park_is_stamped_when_re_parked() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-pre-upgrade-park");
        ws.push_task(Task {
            verify: Some(Verify::User),
            status: TaskStatus::Waiting,
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::User),
                detail: None,
                on: None,
                verification: true,
                parked: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: None,
            }),
            ..sample_task("t-1", "proj")
        });

        let landed = ws
            .user_move_task("proj", &TaskId::from("t-1"), TaskStatus::Completed)
            .expect("no refusal")
            .expect("the row is there");
        let wait = landed.waiting_on.as_ref().expect("the rebuilt look park");
        assert!(wait.verification, "it is still the look: {landed:?}");
        assert!(wait.since.is_some(), "the rebuilt park carries its own start: {landed:?}");
    }

    /// **Re-stating the same wait keeps its start.** A repeated `tasks__wait`
    /// with the same kind, words and blocker is the same ask: its age must
    /// not reset and its identity must not change into a second
    /// notification.
    #[test]
    fn a_repeated_identical_wait_keeps_its_start() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-repeat-wait");
        let seat = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(seat.clone()), ..sample_task("t-1", "proj") });

        let ask = || Request {
            kind: AskKind::Clarification,
            question: "sign this off".to_owned(),
            options: Vec::new(),
            class: None,
            answer: None,
        };
        let first = ws
            .wait_request_task_at_version(
                "proj",
                &TaskId::from("t-1"),
                &seat,
                Some("sign this off".to_owned()),
                ask(),
                None,
                None,
            )
            .expect("the owner may wait its own row");
        let started = first.waiting_on.as_ref().and_then(|wait| wait.since).expect("stamped");
        let again = ws
            .wait_request_task_at_version(
                "proj",
                &TaskId::from("t-1"),
                &seat,
                Some("sign this off".to_owned()),
                ask(),
                None,
                None,
            )
            .expect("a repeated wait is allowed");
        assert_eq!(
            again.waiting_on.as_ref().and_then(|wait| wait.since),
            Some(started),
            "the same wait keeps its start",
        );
    }

    /// The refusal names every wait shape a row can hold (#2414), so the
    /// caller learns what it is sitting on and not merely that it waits.
    #[test]
    fn the_wait_refusal_words_name_each_wait() {
        let wait = |kind: Option<WaitingKind>, on: Option<TaskId>| Waiting {
            kind,
            detail: None,
            on,
            verification: false,
            parked: false,
            due: None,
            acknowledged_at: None,
            snoozed: None,
            expired_at: None,
            since: None,
        };
        assert_eq!(wait_words(None), "a wait", "no stored wait");
        assert_eq!(wait_words(Some(&wait(None, None))), "a wait", "a legacy unstated wait");
        assert_eq!(
            wait_words(Some(&wait(Some(WaitingKind::User), None))),
            "a question for the user",
            "the wait on the person a row can be parked on",
        );
        assert_eq!(wait_words(Some(&wait(Some(WaitingKind::Resource), None))), "a resource");
        assert_eq!(
            wait_words(Some(&wait(Some(WaitingKind::Dependency), Some(TaskId::from("blocker"))))),
            "a dependency on blocker",
            "a dependency names its blocker",
        );
    }

    /// The withdraw door (#2340): a seat cancels a row parked on the
    /// user's look with the reason it states, which lands appended to
    /// what the row already said - the ask that goes away keeps its
    /// story.
    #[test]
    fn a_withdraw_records_the_reason_on_a_parked_row() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-withdraw-reason");
        let mut row = sample_task("t-1", "proj");
        row.verify = Some(Verify::User);
        row.detail = Some("the ask".to_owned());
        ws.push_task(row);
        ws.update_task("proj", &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");

        let withdrawn = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Canceled;
                    task.detail = Some("moot: landed via #2296".to_owned());
                },
            )
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(withdrawn.status, TaskStatus::Canceled, "the cancel lands");
        assert_eq!(withdrawn.waiting_on, None, "and the withdrawn ask leaves the waiting set");
        assert_eq!(
            withdrawn.detail.as_deref(),
            Some("the ask\nmoot: landed via #2296"),
            "the reason lands appended to what the row already said",
        );
    }

    /// A bare status flip on a parked row is refused: the user's pending
    /// ask leaves with its reason recorded, never silently.
    #[test]
    fn a_withdraw_without_a_reason_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-withdraw-bare");
        row_awaiting_look(&ws, "t-1");

        let refused = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Canceled;
                },
            )
            .expect_err("a cancel stating no reason is refused");
        assert_eq!(refused, MoveError::WithdrawNeedsReason, "the door's own refusal");
        let stored = ws.tasks_for_project("proj");
        assert_eq!(stored[0].status, TaskStatus::Waiting, "and the row keeps waiting");
        assert!(
            stored[0].waiting_on.as_ref().is_some_and(|wait| wait.verification),
            "with the look it was parked on intact",
        );
    }

    /// The door covers a decision wait too: the ask is on the user there
    /// as well. A bare cancel is refused, and a stated reason lands.
    #[test]
    fn a_withdraw_of_a_decision_wait_records_the_reason() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-withdraw-question");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        ws.wait_request_task_at_version(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            Some("which shape".to_owned()),
            Request {
                kind: AskKind::Clarification,
                question: "which shape?".to_owned(),
                options: Vec::new(),
                class: None,
                answer: None,
            },
            None,
            None,
        )
        .expect("the owner may wait its own row");

        let lead = SessionSlot::lead("TestOrg", "proj");
        assert_eq!(
            ws.update_task("proj", &TaskId::from("t-1"), By::Seat(lead.clone()), |task| {
                task.status = TaskStatus::Canceled;
            })
            .expect_err("a bare cancel of a question waiting on the user is refused"),
            MoveError::WithdrawNeedsReason,
        );

        let withdrawn = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(lead), |task| {
                task.status = TaskStatus::Canceled;
                task.detail = Some("the question dissolved".to_owned());
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(withdrawn.status, TaskStatus::Canceled);
        assert_eq!(withdrawn.waiting_on, None, "the withdrawn question leaves the waiting set");
        assert_eq!(
            withdrawn.detail.as_deref(),
            Some("the question dissolved"),
            "the reason lands on the row",
        );
    }

    /// The requirement is scoped to the user's waits: a seat cancel of an
    /// ordinary wait - and of a row holding no wait at all - lands as it
    /// always did, with no reason required.
    #[test]
    fn a_seat_cancel_of_an_ordinary_row_stays_silent() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-cancel-ordinary");
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(sample_task("blocker", "proj"));
        ws.push_task(Task { owner: Some(worker.clone()), ..sample_task("t-1", "proj") });
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        ws.wait_task(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            WaitingKind::Dependency,
            Some("the board read first".to_owned()),
            Some(&TaskId::from("blocker")),
        )
        .expect("the owner may wait its own row");

        let lead = SessionSlot::lead("TestOrg", "proj");
        let cancelled = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(lead.clone()), |task| {
                task.status = TaskStatus::Canceled;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(cancelled.status, TaskStatus::Canceled);
        assert_eq!(cancelled.waiting_on, None, "an ordinary wait drops with the cancel");

        // The live shape of the incident's own cleanup: an un-parked row
        // cancels by seat like any other work.
        let plain = ws
            .update_task("proj", &TaskId::from("blocker"), By::Seat(lead), |task| {
                task.status = TaskStatus::Canceled;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(plain.status, TaskStatus::Canceled, "no reason is asked of ordinary rows");
    }

    /// The user's own drag is not the lead's door: their cancel of a
    /// parked row needs no reason - the wait is the user's to withdraw.
    #[test]
    fn a_users_own_cancel_of_a_parked_row_needs_no_reason() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-user-cancel-parked");
        row_awaiting_look(&ws, "t-1");

        let moved = ws
            .user_move_task("proj", &TaskId::from("t-1"), TaskStatus::Canceled)
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(moved.status, TaskStatus::Canceled, "the user's own move lands");
        assert_eq!(moved.waiting_on, None, "and the wait goes with it");
    }

    /// A child row completes on its own terms under a verify=user epic
    /// (#2423): `verify: user` is a property a row is MINTED with, and an
    /// epic's gate governs the epic's own completion rather than its
    /// children's.
    #[test]
    fn a_child_of_a_verify_user_epic_completes_without_a_look() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-child-gate");
        let worker = SessionSlot::worker("TestOrg", "proj", "w1");
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task("epic", "proj") });
        ws.push_task(sample_task_with_parent("t-1", Some("epic"), "proj"));

        let completed = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(worker), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(completed.status, TaskStatus::Completed, "the child owes no look");
        assert_eq!(completed.waiting_on, None, "and nothing parks it");
    }

    /// The boundary the rule keeps (#2423): the epic's OWN gate still
    /// governs the epic's completion - the root parks on the look, and the
    /// board's approve is what completes it.
    #[test]
    fn an_epic_root_still_parks_on_its_own_gate() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-epic-gate");
        let lead = SessionSlot::lead("TestOrg", "proj");
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task("epic", "proj") });

        let parked = ws
            .update_task("proj", &TaskId::from("epic"), By::Seat(lead), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(parked.status, TaskStatus::Waiting, "the root's own gate parks it");
        assert!(
            parked.waiting_on.as_ref().is_some_and(|wait| wait.verification),
            "the wait is the look's: {:?}",
            parked.waiting_on,
        );

        let approved = ws
            .approve_task("proj", &TaskId::from("epic"))
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(approved.status, TaskStatus::Completed, "the approve completes the epic");
    }

    /// A park an inherited gate left behind stops reading as a user wait
    /// (#2423): the row's own `verify` is unchanged, and a seat's
    /// completion lands instead of being refused by the look.
    #[test]
    fn a_park_minted_by_an_inherited_gate_stops_reading_as_a_user_wait() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-stale-park");
        let worker = SessionSlot::worker("TestOrg", "proj", "w1");
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task("epic", "proj") });
        ws.push_task(Task {
            status: TaskStatus::Waiting,
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::User),
                detail: None,
                on: None,
                verification: true,
                parked: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: None,
            }),
            ..sample_task_with_parent("t-1", Some("epic"), "proj")
        });

        let completed = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(worker), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("an inherited park is not the look's to refuse")
            .expect("the row is there");
        assert_eq!(completed.status, TaskStatus::Completed, "the child completes");
        assert_eq!(completed.waiting_on, None, "and the stale wait drops with the move");
    }

    /// The look gate is not an update's to clear. A `verify: none` patch on
    /// a user-bound row is refused by name, so the two-call path (clear,
    /// then complete) has no first step - and the stored row keeps the
    /// gate it was minted with.
    #[test]
    fn an_update_cannot_clear_a_user_verify() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-verify-clear");
        let lead = SessionSlot::lead("TestOrg", "proj");
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task("t-1", "proj") });
        ws.push_task(sample_task("t-2", "proj"));

        let refused = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(lead.clone()), |task| {
                task.verify = Some(Verify::None);
            })
            .expect_err("an update cannot clear the user's look gate");
        assert_eq!(
            refused,
            MoveError::ClearingLookGate,
            "the clear's own refusal, not another move error",
        );

        let stored = ws.tasks_for_project("proj");
        let own = stored.iter().find(|t| t.id == TaskId::from("t-1")).expect("t-1");
        assert_eq!(own.verify, Some(Verify::User), "t-1 keeps its own gate");

        // Setting the gate is still the lead's: a row may be armed.
        ws.update_task("proj", &TaskId::from("t-2"), By::Seat(lead), |task| {
            task.verify = Some(Verify::User);
        })
        .expect("no refusal")
        .expect("the row is there");
    }

    /// Reparenting never touches a gate (#2423): a child of a verify=user
    /// epic carries no gate of its own, so moving it out is not the look's
    /// to refuse - and a row carrying its own gate keeps it wherever it
    /// moves, since the gate is minted on the row rather than inherited.
    #[test]
    fn reparenting_does_not_touch_a_row_s_own_gate() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-verify-reparent");
        let lead = SessionSlot::lead("TestOrg", "proj");
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task("gated", "proj") });
        ws.push_task(Task { ..sample_task("free", "proj") });
        ws.push_task(Task { parent: Some(TaskId::from("gated")), ..sample_task("t-2", "proj") });
        ws.push_task(Task {
            parent: Some(TaskId::from("gated")),
            verify: Some(Verify::User),
            ..sample_task("t-3", "proj")
        });

        ws.update_task("proj", &TaskId::from("t-2"), By::Seat(lead.clone()), |task| {
            task.parent = Some(TaskId::from("free"));
        })
        .expect("no refusal")
        .expect("a child never inherited the epic's gate");

        let moved = ws
            .update_task("proj", &TaskId::from("t-3"), By::Seat(lead), |task| {
                task.parent = Some(TaskId::from("free"));
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(moved.verify, Some(Verify::User), "its own gate travels with it");
        assert_eq!(moved.parent, Some(TaskId::from("free")), "and the reparent landed");
    }

    /// The third door around the look gate (#2251): the update path refuses
    /// to clear the gate and the board's approve is what completes a row
    /// parked on it, so a delete must not settle the row by removal either.
    /// The guard rides the removal itself, which is what stops the cascade
    /// from stepping around it through the epic above.
    #[test]
    fn a_delete_cannot_take_a_row_waiting_on_the_look() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-delete-look");
        ws.push_task(sample_task("epic", "proj"));
        ws.push_task(Task {
            parent: Some(TaskId::from("epic")),
            verify: Some(Verify::User),
            ..sample_task("t-1", "proj")
        });
        ws.update_task("proj", &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");

        let refused = ws
            .remove_task_tree("proj", &TaskId::from("epic"))
            .expect_err("the look a child is parked on is not a delete's to take");
        assert_eq!(
            refused,
            MoveError::DeletingLook("subject t-1".to_owned()),
            "the refusal names the row whose look is pending",
        );
        assert_eq!(
            subjects(&ws.tasks_for_project("proj")),
            ["subject epic".to_owned(), "subject t-1".to_owned()],
            "and nothing went: the epic above the row is not a way around",
        );
    }

    /// The delete's guard covers the user's other ask too (#2340): a row
    /// waiting on a question is not a removal's to take either - it would
    /// settle the ask by deletion, the same way it would settle a look.
    #[test]
    fn a_delete_cannot_take_a_row_waiting_on_a_question() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-delete-question");
        ws.push_task(sample_task("epic", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(sample_task_with_parent("t-1", Some("epic"), "proj"));
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        ws.wait_request_task_at_version(
            "proj",
            &TaskId::from("t-1"),
            &worker,
            Some("which shape".to_owned()),
            Request {
                kind: AskKind::Clarification,
                question: "which shape?".to_owned(),
                options: Vec::new(),
                class: None,
                answer: None,
            },
            None,
            None,
        )
        .expect("the owner may wait its own row");

        let refused = ws
            .remove_task_tree("proj", &TaskId::from("t-1"))
            .expect_err("a question waiting on the user is not a delete's to take");
        assert_eq!(
            refused,
            MoveError::DeletingLook("subject t-1".to_owned()),
            "the refusal names the row whose ask is pending",
        );
        assert_eq!(ws.tasks_for_project("proj").len(), 2, "and nothing went");
    }

    /// The close is the other removal door, and the question park is the
    /// other ask (#2340): a grandchild waiting on a question, under a
    /// branch already cancelled, is not archived by the root's close.
    #[test]
    fn a_root_close_cannot_take_a_grandchild_waiting_on_a_question() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-close-question");
        ws.push_task(sample_task("root", "proj"));
        ws.push_task(sample_task_with_parent("branch", Some("root"), "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.push_task(sample_task_with_parent("asked", Some("branch"), "proj"));
        ws.claim_task("proj", &worker, &TaskId::from("asked")).expect("claim");
        ws.wait_request_task_at_version(
            "proj",
            &TaskId::from("asked"),
            &worker,
            Some("which shape".to_owned()),
            Request {
                kind: AskKind::Clarification,
                question: "which shape?".to_owned(),
                options: Vec::new(),
                class: None,
                answer: None,
            },
            None,
            None,
        )
        .expect("the owner may wait its own row");
        let lead = SessionSlot::lead("TestOrg", "proj");
        ws.update_task("proj", &TaskId::from("branch"), By::Seat(lead.clone()), |task| {
            task.status = TaskStatus::Canceled;
        })
        .expect("no refusal")
        .expect("the row is there");

        let refused = ws
            .update_task("proj", &TaskId::from("root"), By::Seat(lead), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("the archive is not a way to settle a pending question");
        assert_eq!(refused, MoveError::ClosingLook("subject asked".to_owned()));
        assert_eq!(ws.tasks_for_project("proj").len(), 3, "and nothing was archived");
    }

    /// The gate protects the look, not the row's life: once the approve has
    /// settled it, the epics and rows around it are the caller's to clear,
    /// the same as any finished work.
    #[test]
    fn a_delete_lands_once_the_look_is_given() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-delete-approved");
        ws.push_task(sample_task("epic", "proj"));
        ws.push_task(Task {
            parent: Some(TaskId::from("epic")),
            verify: Some(Verify::User),
            ..sample_task("t-1", "proj")
        });
        ws.update_task("proj", &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");
        ws.approve_task("proj", &TaskId::from("t-1")).expect("no refusal").expect("the row");

        let removed = ws
            .remove_task_tree("proj", &TaskId::from("epic"))
            .expect("the look landed; the tree is the caller's to clear");
        assert_eq!(subjects(&removed).len(), 2, "the epic and its approved child");
    }

    /// A row merely BOUND by the user's verify - never parked on the look,
    /// because nothing ever tried to complete it - is still work the caller
    /// may abandon: the gate fires on the transition into completed, so an
    /// unstarted row's delete skips no look.
    #[test]
    fn a_delete_lands_on_a_user_bound_row_that_never_reached_the_look() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-delete-pending");
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task("t-1", "proj") });

        let removed = ws
            .remove_task_tree("proj", &TaskId::from("t-1"))
            .expect("no look is pending on an unstarted row");
        assert_eq!(removed.len(), 1, "the row went");
    }

    /// A dependency wait is on the task it names, not on the user: a row's
    /// `verify: user` gates that row's own completion, so neither it nor
    /// its epic's gate makes the waiting child a user ask the delete must
    /// refuse (#2415, #2423).
    #[test]
    fn a_dependency_wait_under_a_verify_user_epic_is_deletable() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-dep-under-epic");
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task("epic", "proj") });
        ws.push_task(sample_task_with_parent("blocker", Some("epic"), "proj"));
        ws.push_task(Task {
            status: TaskStatus::Waiting,
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::Dependency),
                detail: Some("the blocker lands first".to_owned()),
                on: Some(TaskId::from("blocker")),
                verification: false,
                parked: false,
                due: None,
                acknowledged_at: None,
                snoozed: None,
                expired_at: None,
                since: None,
            }),
            ..sample_task_with_parent("t-1", Some("epic"), "proj")
        });

        let rows = ws.tasks_for_project("proj");
        let index = rows.iter().position(|task| task.id == TaskId::from("t-1")).expect("the row");
        assert_eq!(
            effective_verify(&rows, index),
            Verify::None,
            "premise: the epic's gate does not bind the waiting child",
        );

        let removed = ws
            .remove_task_tree("proj", &TaskId::from("t-1"))
            .expect("a dependency wait is not an ask on the user");
        assert_eq!(subjects(&removed), ["subject t-1".to_owned()], "the waiting row went alone");
    }

    /// A parked row in ANOTHER project sharing the named id is not this
    /// delete's business: the guard's project test is what keeps a foreign
    /// row from refusing (or being named by) this project's delete.
    #[test]
    fn a_parked_row_in_another_project_does_not_refuse_the_delete() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-delete-foreign");
        // The row's own project is loaded too: the evidence gate checks a
        // completion against the project's repository.
        ws.seed_test_project("elsewhere", "/tmp/tp-delete-foreign-e");
        ws.push_task(Task { verify: Some(Verify::User), ..sample_task("t-1", "elsewhere") });
        ws.update_task("elsewhere", &TaskId::from("t-1"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");
        ws.push_task(sample_task("t-1", "proj"));

        let removed = ws
            .remove_task_tree("proj", &TaskId::from("t-1"))
            .expect("another project's parked row is not this delete's business");
        assert_eq!(removed.len(), 1, "this project's row went");
        assert_eq!(ws.tasks_for_project("elsewhere").len(), 1, "and the parked one stayed");
    }

    /// The close guard's two scoping clauses, one control each: a parked
    /// row in ANOTHER project sharing a doomed row's id must not refuse
    /// this project's close, and a parked row in the SAME project but
    /// outside the closing tree must not either. Without them a close
    /// anywhere refuses on an unrelated row's look, naming a subject the
    /// caller never touched.
    #[test]
    fn a_parked_row_outside_the_closing_tree_does_not_refuse_the_close() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-close-scope");
        ws.seed_test_project("elsewhere", "/tmp/tp-close-scope-e");
        // The tree being closed: a root and its child, both unparked.
        ws.push_task(sample_task("root", "proj"));
        ws.push_task(sample_task_with_parent("child", Some("root"), "proj"));
        // A parked row sharing a doomed id, in another project: a child, so
        // completing it parks on the look instead of closing a tree of its own.
        ws.push_task(Task {
            parent: Some(TaskId::from("epic-e")),
            verify: Some(Verify::User),
            ..sample_task("child", "elsewhere")
        });
        ws.update_task("elsewhere", &TaskId::from("child"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");
        // And a parked row in this project, outside the tree.
        ws.push_task(Task {
            parent: Some(TaskId::from("epic-p")),
            verify: Some(Verify::User),
            ..sample_task("outside", "proj")
        });
        ws.update_task("proj", &TaskId::from("outside"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");
        // The root's own child is abandoned, so only the parked rows could
        // stand in the close's way.
        ws.update_task("proj", &TaskId::from("child"), By::System, |task| {
            task.status = TaskStatus::Canceled;
        })
        .expect("no refusal")
        .expect("the row is there");

        let closed = ws
            .update_task(
                "proj",
                &TaskId::from("root"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect("neither parked row is the closing tree's to refuse")
            .expect("the row is there");
        assert_eq!(closed.status, TaskStatus::Completed, "the root closed");
        assert_eq!(
            subjects(&ws.tasks_for_project("proj")),
            ["subject outside".to_owned()],
            "the tree went and the parked row outside it stayed",
        );
        assert_eq!(ws.tasks_for_project("elsewhere").len(), 1, "and the foreign one stayed");
    }

    /// The close is the other verb that removes rows (#2251's class, found
    /// reviewing the delete's guard): completing a ROOT row archives its
    /// whole tree, and the open-children check sees direct children only -
    /// so a grandchild parked on the look, under a branch already cancelled,
    /// used to be archived with its look pending.
    #[test]
    fn a_root_close_cannot_take_a_grandchild_waiting_on_the_look() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-close-look");
        ws.push_task(sample_task("root", "proj"));
        ws.push_task(sample_task_with_parent("branch", Some("root"), "proj"));
        ws.push_task(Task {
            parent: Some(TaskId::from("branch")),
            verify: Some(Verify::User),
            ..sample_task("gated", "proj")
        });
        let lead = SessionSlot::lead("TestOrg", "proj");
        // The grandchild reaches the look and is parked on it.
        ws.update_task("proj", &TaskId::from("gated"), By::System, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");
        // The branch is abandoned: terminal, so the open-children check passes.
        ws.update_task("proj", &TaskId::from("branch"), By::Seat(lead.clone()), |task| {
            task.status = TaskStatus::Canceled;
        })
        .expect("no refusal")
        .expect("the row is there");

        let refused = ws
            .update_task("proj", &TaskId::from("root"), By::Seat(lead), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("the archive is not a way to settle the parked look");
        assert_eq!(refused, MoveError::ClosingLook("subject gated".to_owned()));
        assert_eq!(ws.tasks_for_project("proj").len(), 3, "and nothing was archived");
    }

    #[test]
    fn a_verify_user_row_cannot_be_completed_by_any_caller() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-verify");
        ws.push_task(Task {
            verify: Some(forge_primitives::tasks::Verify::User),
            ..sample_task("t-1", "proj")
        });
        let lead = SessionSlot::lead("TestOrg", "proj");

        let moved = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(lead), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(moved.status, TaskStatus::Waiting, "the gate lands it in waiting");
        let wait = moved.waiting_on.expect("the wait is the verify gate");
        assert_eq!(
            (wait.kind, wait.detail, wait.on, wait.verification, wait.parked),
            (Some(WaitingKind::User), None, None, true, false),
            "the wait is the verify gate, which the board draws as approve / send back",
        );
        assert!(wait.since.is_some(), "the wait is stamped as it begins");
    }

    #[test]
    fn completing_a_root_archives_the_tree_and_a_child_completion_does_not() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-archive");
        install_db(&ws, dir.path());
        ws.push_task(sample_task_with_parent("epic", None, "proj"));
        ws.push_task(Task {
            status: TaskStatus::Completed,
            ..sample_task_with_parent("child", Some("epic"), "proj")
        });
        let lead = SessionSlot::lead("TestOrg", "proj");

        // A child completing on its own archives nothing.
        ws.update_task("proj", &TaskId::from("child"), By::Seat(lead.clone()), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal");
        assert_eq!(ws.tasks_for_project("proj").len(), 2, "a completed child stays live");

        // Completing the root closes it: the tree leaves the live set and
        // lands in the archive with its stamp.
        ws.update_task("proj", &TaskId::from("epic"), By::Seat(lead), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal");
        assert!(ws.tasks_for_project("proj").is_empty(), "the closed tree is off the live board");
        let archived = archived_of(&ws, "proj");
        assert_eq!(archived.len(), 2, "the epic and its child went to the archive: {archived:?}");
        assert!(archived.iter().all(|t| t.archived_at.is_some()), "each row carries its stamp");
    }

    /// A terminal tree stands quiet past the retention window and then
    /// leaves the live board on its own, stamped into the archive - the
    /// rows were history the moment they ended, and the window is only how
    /// long they stay readable before the archive takes them.
    #[test]
    fn a_terminal_tree_past_the_window_archives_itself() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-retain");
        install_db(&ws, dir.path());
        let mut root = sample_task("epic", "proj");
        root.status = TaskStatus::Completed;
        ws.push_task(root);
        let mut child = sample_task_with_parent("child", Some("epic"), "proj");
        child.status = TaskStatus::Completed;
        ws.push_task(child);

        let now = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(crate::tasks::RETENTION_SECS + 1);
        let archived = ws.retain_terminal_rows(now);
        assert_eq!(archived.len(), 2, "the whole tree leaves together: {archived:?}");
        assert!(ws.tasks_for_project("proj").is_empty(), "nothing terminal is left on the board");
        let kept = archived_of(&ws, "proj");
        assert_eq!(kept.len(), 2, "both rows are in the archive: {kept:?}");
        assert!(kept.iter().all(|t| t.archived_at == Some(now)), "each row carries the stamp");
        assert!(kept.iter().all(|t| t.version == 1), "archiving is a write");
        let (_, announced) = last_tasks_changed(&mut rx);
        assert!(announced.is_empty(), "the board is told the row left: {announced:?}");
    }

    /// A row inside its window stays: the retention is a delay, not an
    /// instant sweep.
    #[test]
    fn a_terminal_row_inside_its_window_stays() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-retain-window");
        install_db(&ws, dir.path());
        let mut done = sample_task("done", "proj");
        done.status = TaskStatus::Completed;
        done.updated_at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100);
        ws.push_task(done);

        let now = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(100 + crate::tasks::RETENTION_SECS - 1);
        assert!(ws.retain_terminal_rows(now).is_empty(), "the window has not passed");
        assert_eq!(ws.tasks_for_project("proj").len(), 1, "the row is still live");
        assert!(archived_of(&ws, "proj").is_empty(), "and not archived");
    }

    /// A terminal root with an open child is still a tree in play: the
    /// archive never takes a parent out from under work that has not
    /// ended.
    #[test]
    fn a_terminal_root_with_an_open_child_stays() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-retain-open");
        install_db(&ws, dir.path());
        let mut root = sample_task("epic", "proj");
        root.status = TaskStatus::Canceled;
        ws.push_task(root);
        ws.push_task(sample_task_with_parent("child", Some("epic"), "proj"));

        let now = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(crate::tasks::RETENTION_SECS + 1);
        assert!(
            ws.retain_terminal_rows(now).is_empty(),
            "an open child holds the tree where it is",
        );
        assert_eq!(ws.tasks_for_project("proj").len(), 2, "nothing moved");
    }

    /// A completed child of a live epic stays: the epic's rollup reads it
    /// until the epic itself closes.
    #[test]
    fn a_completed_child_of_a_live_epic_stays_live() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-retain-child");
        install_db(&ws, dir.path());
        ws.push_task(sample_task("epic", "proj"));
        let mut child = sample_task_with_parent("child", Some("epic"), "proj");
        child.status = TaskStatus::Completed;
        ws.push_task(child);

        let now = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(crate::tasks::RETENTION_SECS + 1);
        assert!(ws.retain_terminal_rows(now).is_empty(), "the epic still holds its child");
        assert_eq!(ws.tasks_for_project("proj").len(), 2, "nothing moved");
    }

    /// A restored tree comes back to the live board whole, with its stamp
    /// cleared and the write recorded - the archive is a shelf, not a
    /// grave.
    #[test]
    fn a_restored_tree_returns_to_the_live_board() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-restore");
        install_db(&ws, dir.path());
        ws.push_task(sample_task_with_parent("epic", None, "proj"));
        ws.push_task(Task {
            status: TaskStatus::Completed,
            ..sample_task_with_parent("child", Some("epic"), "proj")
        });
        let lead = SessionSlot::lead("TestOrg", "proj");
        ws.update_task("proj", &TaskId::from("epic"), By::Seat(lead), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal");
        assert!(ws.tasks_for_project("proj").is_empty(), "precondition: the tree is archived");

        let restored = ws
            .restore_task("proj", &TaskId::from("epic"))
            .expect("no refusal")
            .expect("the archive holds the tree");
        assert_eq!(restored.id, TaskId::from("epic"));
        assert_eq!(restored.archived_at, None, "the restored row is live again");
        assert_eq!(ws.tasks_for_project("proj").len(), 2, "the whole tree came back");
        assert!(archived_of(&ws, "proj").is_empty(), "and left the archive");
        // A restored row is still terminal, so it belongs to the closed
        // shelf (re-read when that toggle opens), never to the live set the
        // frame carries: announcing it here re-added finished rows to the
        // drawn lanes after every write.
        let (_, announced) = last_tasks_changed(&mut rx);
        assert!(announced.is_empty(), "no finished row rides the live set: {announced:?}");
    }

    #[test]
    fn restoring_an_id_the_archive_does_not_hold_is_absent() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-restore-absent");
        install_db(&ws, dir.path());

        assert_eq!(
            ws.restore_task("proj", &TaskId::from("nope")),
            Ok(None),
            "an id the archive does not hold is absent, not an error",
        );
    }

    /// Absence is never success: a row with no evidence at all cannot
    /// claim completion, whatever door it is moved through.
    #[test]
    fn a_completion_without_evidence_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-gate-missing");
        ws.push_task(Task { evidence: None, ..sample_task("t-1", "proj") });
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");

        assert_eq!(
            ws.update_task("proj", &TaskId::from("t-1"), By::Seat(worker), |task| {
                task.status = TaskStatus::Completed;
            }),
            Err(MoveError::EvidenceMissing),
            "there is nothing to check, so the claim is refused",
        );
        assert_eq!(
            ws.tasks_for_project("proj")[0].status,
            TaskStatus::Pending,
            "the row did not move",
        );
    }

    /// An evidence block naming no artifact is the same absence wearing a
    /// shape: refused, not accepted as a checked claim.
    #[test]
    fn a_completion_whose_evidence_names_nothing_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-gate-empty");
        ws.install_evidence_verifier(FixedVerifier::accepting());
        ws.push_task(Task {
            evidence: Some(Evidence {
                commit: None,
                path: None,
                lead_verified: None,
                excerpt: Some("looks fine to me".to_owned()),
                verified_at: None,
                problem: None,
            }),
            ..sample_task("t-1", "proj")
        });

        assert_eq!(
            ws.update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            }),
            Err(MoveError::EvidenceMissing),
            "an excerpt is the reader's evidence, never the claim",
        );
    }

    /// A claim the repository cannot back refuses, naming the check's own
    /// reason so the caller can fix the artifact.
    #[test]
    fn a_completion_with_unverifiable_evidence_is_refused_naming_the_check() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-gate-refused");
        ws.install_evidence_verifier(FixedVerifier::refusing(
            "commit deadbeef does not exist in /tmp/tp-gate-refused",
        ));
        ws.push_task(sample_task("t-1", "proj"));

        let refused = ws
            .update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("the artifact is not there");
        assert_eq!(
            refused,
            MoveError::EvidenceUnverified(
                "commit deadbeef does not exist in /tmp/tp-gate-refused".to_owned()
            ),
        );
        assert!(
            refused.to_string().contains("deadbeef"),
            "the refusal carries the check's reason: {refused}",
        );
    }

    /// A verified completion carries the stamp of the check that passed,
    /// which is the wire's `verified at`.
    #[test]
    fn a_verified_completion_stamps_the_check() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-gate-stamp");
        ws.install_evidence_verifier(FixedVerifier::accepting());
        ws.push_task(sample_task("t-1", "proj"));

        let done = ws
            .update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(done.status, TaskStatus::Completed);
        let evidence = done.evidence.expect("the claim rides the row");
        assert!(evidence.verified_at.is_some(), "the passing check is stamped");
        assert_eq!(evidence.problem, None);
    }

    /// The look gate follows the same rule: a verify=user row's completion
    /// verifies before it parks, so the row the user is asked to approve
    /// carries a claim forge has already checked.
    #[test]
    fn a_look_park_carries_verified_evidence() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-gate-park");
        ws.install_evidence_verifier(FixedVerifier::accepting());
        let mut row = sample_task("t-1", "proj");
        row.verify = Some(Verify::User);
        ws.push_task(row);

        let parked = ws
            .update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(parked.status, TaskStatus::Waiting, "the look gate parks it");
        assert!(
            parked.evidence.as_ref().is_some_and(|evidence| evidence.verified_at.is_some()),
            "the park carries a verified claim",
        );
    }

    /// The approve door re-runs the check on the move: a repository that no
    /// longer holds the artifact refuses the approve even though the park's
    /// stamp is on the row.
    #[test]
    fn an_approve_reverifies_the_evidence() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-gate-approve");
        ws.install_evidence_verifier(FixedVerifier::accepting());
        let mut row = sample_task("t-1", "proj");
        row.verify = Some(Verify::User);
        ws.push_task(row);
        ws.update_task("proj", &TaskId::from("t-1"), By::User, |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal")
        .expect("the row is there");

        ws.install_evidence_verifier(FixedVerifier::refusing("the commit was force-pushed away"));
        let refused =
            ws.approve_task("proj", &TaskId::from("t-1")).expect_err("the artifact is gone now");
        assert_eq!(
            refused,
            MoveError::EvidenceUnverified("the commit was force-pushed away".to_owned()),
        );
        assert_eq!(
            ws.tasks_for_project("proj")[0].status,
            TaskStatus::Waiting,
            "the row stays parked for the look",
        );
    }

    /// A project that opted into the GitHub connector waits on its pull
    /// requests too: an unchecked link is the wire's unknown, and unknown
    /// does not complete a row.
    #[test]
    fn a_connector_project_refuses_a_completion_on_an_unchecked_pull_request() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_connector(
            "proj",
            "/tmp/tp-connector",
            crate::config::ConnectorKind::Github,
        );
        ws.install_evidence_verifier(FixedVerifier::accepting());
        let mut row = sample_task("t-1", "proj");
        row.links.push(pr_link("https://example.invalid/pull/7"));
        ws.push_task(row);

        let refused = ws
            .update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("the connector has not seen the pull request");
        assert!(
            matches!(&refused, MoveError::EvidenceUnverified(reason)
                if reason.contains("connector") && reason.contains("pull/7")),
            "the refusal names the connector and the link: {refused}",
        );
    }

    /// A fresh red check is a refusal of its own: a failing pull request
    /// does not complete a row.
    #[test]
    fn a_connector_project_refuses_a_completion_on_failing_ci() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_connector(
            "proj",
            "/tmp/tp-connector-red",
            crate::config::ConnectorKind::Github,
        );
        ws.install_evidence_verifier(FixedVerifier::accepting());
        let mut row = sample_task("t-1", "proj");
        row.links.push(pr_link_checked("https://example.invalid/pull/7", Some("failing")));
        ws.push_task(row);

        let refused = ws
            .update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("red CI cannot complete a row");
        assert!(
            matches!(&refused, MoveError::EvidenceUnverified(reason)
                if reason.contains("failing CI")),
            "the refusal names the verdict: {refused}",
        );
    }

    /// **Absence is never success**: a pull request whose CI rollup is
    /// absent or unknown does not vouch for a completion, even with a fresh
    /// check and a green-looking state.
    #[test]
    fn a_connector_project_refuses_a_completion_with_no_ci_verdict() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_connector(
            "proj",
            "/tmp/tp-connector-none",
            crate::config::ConnectorKind::Github,
        );
        ws.install_evidence_verifier(FixedVerifier::accepting());
        let mut row = sample_task("t-1", "proj");
        row.links.push(pr_link_checked("https://example.invalid/pull/7", None));
        ws.push_task(row);

        let refused = ws
            .update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("an absent CI verdict cannot complete a row");
        assert!(
            matches!(&refused, MoveError::EvidenceUnverified(reason)
                if reason.contains("no CI verdict")),
            "the refusal names the absence: {refused}",
        );
    }

    /// A connector read that found the same answer again is not news: the
    /// check's `at` moves so it stays fresh, while `changed_at` and the
    /// row's version do not - so a steady connector neither refuses a live
    /// agent's versioned write nor re-fires the new-input chase.
    #[test]
    fn an_unchanged_connector_read_keeps_changed_at_and_the_version() {
        let epoch =
            |secs: u64| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let prior = LinkCheck {
            at: epoch(1_000),
            changed_at: Some(epoch(900)),
            state: Some("open".to_owned()),
            head: Some("abc123".to_owned()),
            ci: Some("passing".to_owned()),
            problem: None,
        };
        let same = crate::env::git_diff::PullRequestState {
            state: Some("open".to_owned()),
            head: Some("abc123".to_owned()),
            ci: Some("passing".to_owned()),
            problem: None,
        };
        let (check, differs) = merge_check(Some(&prior), &same, epoch(2_000));
        assert!(!differs, "the same answer is not news");
        assert_eq!(check.at, epoch(2_000), "the freshness stamp moves");
        assert_eq!(check.changed_at, Some(epoch(900)), "and the change stamp does not");

        let moved = crate::env::git_diff::PullRequestState {
            state: same.state.clone(),
            head: Some("def456".to_owned()),
            ci: same.ci.clone(),
            problem: None,
        };
        let (check, differs) = merge_check(Some(&prior), &moved, epoch(2_000));
        assert!(differs, "a new head is news");
        assert_eq!(check.changed_at, Some(epoch(2_000)), "and stamps the change");

        let (first, differs) = merge_check(None, &same, epoch(2_000));
        assert!(differs, "a first read is always news");
        assert_eq!(first.changed_at, Some(epoch(2_000)));
    }

    /// A fresh green check lets the completion through, and the same row
    /// with no connector is not asked about its link at all.
    #[test]
    fn a_connector_project_completes_on_a_fresh_green_check() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_connector(
            "proj",
            "/tmp/tp-connector-green",
            crate::config::ConnectorKind::Github,
        );
        ws.install_evidence_verifier(FixedVerifier::accepting());
        let mut row = sample_task("t-1", "proj");
        row.links.push(pr_link_checked("https://example.invalid/pull/7", Some("passing")));
        ws.push_task(row);

        let done = ws
            .update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("no refusal")
            .expect("the row is there");
        assert_eq!(done.status, TaskStatus::Completed);

        // The control: the same unchecked link in a project with no
        // connector completes, because nothing there asks for a PR.
        let (plain, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        plain.seed_test_project("proj", "/tmp/tp-connector-none");
        plain.install_evidence_verifier(FixedVerifier::accepting());
        let mut row = sample_task("t-1", "proj");
        row.links.push(pr_link("https://example.invalid/pull/7"));
        plain.push_task(row);
        plain
            .update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect("a self-contained project asks for no pull request");
    }

    /// An old check is not fresh: a cached verdict is evidence only while
    /// it is current, so a link checked past the window refuses.
    #[test]
    fn an_expired_connector_check_is_not_evidence() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project_with_connector(
            "proj",
            "/tmp/tp-connector-stale",
            crate::config::ConnectorKind::Github,
        );
        ws.install_evidence_verifier(FixedVerifier::accepting());
        let mut row = sample_task("t-1", "proj");
        let mut link = pr_link_checked("https://example.invalid/pull/7", Some("passing"));
        if let Some(check) = link.last_check.as_mut() {
            check.at = SystemTime::now()
                - std::time::Duration::from_secs(crate::tasks::CONNECTOR_REFRESH_SECS + 1);
        }
        row.links.push(link);
        ws.push_task(row);

        assert!(
            matches!(
                ws.update_task("proj", &TaskId::from("t-1"), By::User, |task| {
                    task.status = TaskStatus::Completed;
                }),
                Err(MoveError::EvidenceUnverified(reason)) if reason.contains("connector")
            ),
            "an expired check is refused",
        );
    }

    /// A real git repository at `path` holding one commit, its head
    /// returned. The sibling-repo case is the field's whole reason, so
    /// the test drives git itself rather than a verifier that answers
    /// what the test says.
    fn repo_with_commit(path: &std::path::Path, words: &str) -> String {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .env("GIT_AUTHOR_NAME", "forge")
                .env("GIT_AUTHOR_EMAIL", "forge@example.invalid")
                .env("GIT_COMMITTER_NAME", "forge")
                .env("GIT_COMMITTER_EMAIL", "forge@example.invalid")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?} failed");
        };
        std::fs::create_dir_all(path).expect("mkdir");
        run(&["init", "-q"]);
        std::fs::write(path.join("done.txt"), words).expect("write");
        run(&["add", "."]);
        run(&["-c", "commit.gpgsign=false", "commit", "-q", "-m", words]);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git runs");
        String::from_utf8(out.stdout).expect("utf8").trim().to_owned()
    }

    /// Work whose artifact lives in a sibling repository cannot complete
    /// a row through the mechanical check, and the project's lead is the
    /// one seat that can vouch for it. A real repository backs both
    /// halves: the commit exists, just not in the project's own repo.
    ///
    /// Mutant: accept `lead_verified` from any seat - the worker test
    /// beside this one is what catches it - or keep the mechanical check
    /// running when the lead has vouched, which fails here because the
    /// project repo does not hold the sibling's commit.
    #[test]
    fn a_project_lead_vouches_for_a_sibling_repositories_artifact() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let project_repo = dir.path().join("project");
        repo_with_commit(&project_repo, "project work");
        ws.seed_test_project("proj", project_repo.to_str().expect("utf8 path"));
        ws.install_evidence_verifier(std::sync::Arc::new(crate::env::evidence::GitEvidence));
        let sibling_commit = repo_with_commit(&dir.path().join("sibling"), "sibling work");
        let quote = "read the sibling repo at this sha: the five port rows pass";
        ws.push_task(Task {
            evidence: Some(Evidence {
                commit: Some(sibling_commit),
                lead_verified: Some(quote.to_owned()),
                ..passing_evidence().expect("a sample claim")
            }),
            ..sample_task("t-1", "proj")
        });

        let done = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect("the lead's word is the verification")
            .expect("the row is there");
        assert_eq!(done.status, TaskStatus::Completed);
        let evidence = done.evidence.expect("the claim rides the row");
        assert!(evidence.verified_at.is_some(), "the lead's check is stamped like any other");
        assert_eq!(evidence.problem, None);
        assert_eq!(evidence.lead_verified.as_deref(), Some(quote), "the quote stays on the block");
    }

    /// Vouching is the lead's own act: a worker stating `lead_verified` is
    /// refused, and so is another project's lead. The refusal names the
    /// rule, and the row does not move.
    #[test]
    fn only_the_projects_lead_may_vouch_for_an_artifact() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-vouch-seat");
        let quote = "read the sibling repo at this sha";
        ws.push_task(Task {
            evidence: Some(Evidence {
                commit: None,
                path: None,
                lead_verified: Some(quote.to_owned()),
                ..passing_evidence().expect("a sample claim")
            }),
            ..sample_task("t-1", "proj")
        });

        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        let refused = ws
            .update_task("proj", &TaskId::from("t-1"), By::Seat(worker), |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("a worker cannot vouch for its own artifact");
        assert_eq!(refused, MoveError::LeadVerificationLeadOnly);
        assert!(
            refused.to_string().contains("only the project's lead"),
            "the refusal names the rule: {refused}",
        );

        let foreign_lead = SessionSlot::lead("TestOrg", "other");
        assert_eq!(
            ws.update_task("proj", &TaskId::from("t-1"), By::Seat(foreign_lead), |task| {
                task.status = TaskStatus::Completed;
            }),
            Err(MoveError::LeadVerificationLeadOnly),
            "a lead of another project is not this project's lead",
        );
        assert_eq!(
            ws.tasks_for_project("proj")[0].status,
            TaskStatus::Pending,
            "the row did not move",
        );
    }

    /// The lead's word is a claim form of its own: no commit and no path,
    /// and the mechanical check - which would find nothing - never runs.
    #[test]
    fn the_lead_verification_alone_completes_a_row() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let project_repo = dir.path().join("project");
        std::fs::create_dir_all(&project_repo).expect("mkdir");
        ws.seed_test_project("proj", project_repo.to_str().expect("utf8 path"));
        ws.install_evidence_verifier(std::sync::Arc::new(crate::env::evidence::GitEvidence));
        ws.push_task(Task {
            evidence: Some(Evidence {
                commit: None,
                path: None,
                lead_verified: Some("I read the report; it holds".to_owned()),
                excerpt: None,
                verified_at: None,
                problem: None,
            }),
            ..sample_task("t-1", "proj")
        });

        let done = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect("the lead's word needs no artifact")
            .expect("the row is there");
        assert_eq!(done.status, TaskStatus::Completed);
        assert!(
            done.evidence.as_ref().is_some_and(|evidence| evidence.verified_at.is_some()),
            "the lead's check is stamped like any other",
        );
    }

    /// A lead's verification carries its words: a present but blank one is
    /// refused by name rather than read as an absent claim.
    #[test]
    fn a_blank_lead_verification_is_refused_by_name() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-vouch-blank");
        ws.push_task(Task {
            evidence: Some(Evidence {
                commit: None,
                path: None,
                lead_verified: Some("   ".to_owned()),
                excerpt: None,
                verified_at: None,
                problem: None,
            }),
            ..sample_task("t-1", "proj")
        });

        let refused = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect_err("blank words are not a verification");
        assert_eq!(refused, MoveError::LeadVerificationEmpty);
        assert!(
            refused.to_string().contains("lead_verified"),
            "the refusal names the field: {refused}",
        );
    }

    /// The absent field changes nothing for a worker, and the lead's own
    /// failure carries the stale-catalogue hint: a session resumed from
    /// before the field existed drops it, and would otherwise read a plain
    /// missing-commit refusal with no way to learn why.
    #[test]
    fn a_lead_seats_failed_check_carries_the_stale_catalogue_hint() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let project_repo = dir.path().join("project");
        std::fs::create_dir_all(&project_repo).expect("mkdir");
        ws.seed_test_project("proj", project_repo.to_str().expect("utf8 path"));
        ws.install_evidence_verifier(std::sync::Arc::new(crate::env::evidence::GitEvidence));
        ws.push_task(sample_task("t-1", "proj"));

        let refused = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect_err("the project repo does not hold the claim");
        assert!(
            matches!(&refused, MoveError::EvidenceUnverifiedLead(reason)
                if reason.contains("test-commit")),
            "the lead's refusal carries the check's own reason: {refused}",
        );
        let said = refused.to_string();
        assert!(
            said.contains("the completion's evidence did not verify: commit test-commit"),
            "today's text is unchanged: {said}",
        );
        assert!(
            said.contains("lead_verified") && said.contains("respawn the session"),
            "the lead's refusal ends with the stale-catalogue hint: {said}",
        );

        let worker_refused = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::worker("TestOrg", "proj", "w-1")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect_err("a worker's claim fails the same check");
        assert!(
            matches!(&worker_refused, MoveError::EvidenceUnverified(_)),
            "a worker's refusal keeps today's variant: {worker_refused}",
        );
        assert!(
            !worker_refused.to_string().contains("lead_verified"),
            "a worker's refusal does not carry the hint: {worker_refused}",
        );
    }

    /// The lead's verification lets a gated row park for the look; it
    /// never stands in for the look. The user's own approve completes it,
    /// and the quote stays on the block.
    #[test]
    fn a_lead_vouched_look_still_waits_for_the_users_approve() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-vouch-look");
        let quote = "I read the sibling repo; the report holds";
        let mut row = sample_task("t-1", "proj");
        row.verify = Some(Verify::User);
        row.evidence = Some(Evidence {
            commit: None,
            path: None,
            lead_verified: Some(quote.to_owned()),
            excerpt: None,
            verified_at: None,
            problem: None,
        });
        ws.push_task(row);

        let parked = ws
            .update_task(
                "proj",
                &TaskId::from("t-1"),
                By::Seat(SessionSlot::lead("TestOrg", "proj")),
                |task| {
                    task.status = TaskStatus::Completed;
                },
            )
            .expect("the lead's word is the verification")
            .expect("the row is there");
        assert_eq!(parked.status, TaskStatus::Waiting, "the look is not bypassed");
        assert!(
            parked.waiting_on.as_ref().is_some_and(|wait| wait.verification),
            "it waits on the user's look",
        );

        let approved = ws
            .approve_task("proj", &TaskId::from("t-1"))
            .expect("the user's approve is its own verification")
            .expect("the row is there");
        assert_eq!(approved.status, TaskStatus::Completed, "the approve completes it");
        let evidence = approved.evidence.expect("the claim rides the row");
        assert_eq!(evidence.lead_verified.as_deref(), Some(quote), "the quote stays");
        assert!(evidence.verified_at.is_some(), "the approve re-stamps the check");
    }

    /// The default is off: a project with no connector refreshes nothing,
    /// so no child runs and no link moves.
    #[tokio::test]
    async fn a_project_without_a_connector_refreshes_nothing() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-no-connector");
        let mut row = sample_task("t-1", "proj");
        row.links.push(pr_link("https://example.invalid/pull/7"));
        ws.push_task(row);

        ws.refresh_connectors(SystemTime::now()).await;
        let stored = &ws.tasks_for_project("proj")[0];
        assert_eq!(stored.links[0].last_check, None, "nothing checked the link");
        assert_eq!(stored.version, 0, "and nothing was written");
    }

    #[test]
    fn completing_a_root_with_open_children_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-open-children");
        install_db(&ws, dir.path());
        ws.push_task(sample_task_with_parent("epic", None, "proj"));
        ws.push_task(sample_task_with_parent("child", Some("epic"), "proj"));
        let lead = SessionSlot::lead("TestOrg", "proj");

        assert_eq!(
            ws.update_task("proj", &TaskId::from("epic"), By::Seat(lead), |task| {
                task.status = TaskStatus::Completed;
            }),
            Err(MoveError::OpenChildren(vec!["subject child".to_owned()])),
            "the refusal names the child that still has to be cancelled",
        );
        assert_eq!(ws.tasks_for_project("proj").len(), 2, "nothing moved");
        assert!(archived_of(&ws, "proj").is_empty(), "nothing archived");

        // Thirty open children is one line on the board's service line: the
        // message names the first few and counts the rest rather than
        // inlining a thirty-subject sentence.
        for n in 0..30 {
            ws.push_task(sample_task_with_parent(&format!("c{n}"), Some("epic"), "proj"));
        }
        let said = ws
            .update_task("proj", &TaskId::from("epic"), By::User, |task| {
                task.status = TaskStatus::Completed;
            })
            .expect_err("the root is refused")
            .to_string();
        assert!(said.contains("28 more"), "the tail was not counted: {said}");
        assert!(
            !said.contains("c20"),
            "the message inlines every subject rather than eliding: {said}"
        );
    }

    #[test]
    fn every_move_lands_in_the_history_with_its_by() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/tp-history");
        install_db(&ws, dir.path());
        ws.push_task(sample_task("t-1", "proj"));
        let worker = SessionSlot::worker("TestOrg", "proj", "w-1");
        ws.claim_task("proj", &worker, &TaskId::from("t-1")).expect("claim");
        ws.update_task("proj", &TaskId::from("t-1"), By::Seat(worker.clone()), |task| {
            task.status = TaskStatus::Completed;
        })
        .expect("no refusal");

        let history = history_of(&ws, "proj");
        assert_eq!(history.len(), 3, "creation, claim and completion: {history:?}");
        assert_eq!(
            (history[1].from, history[1].to, history[1].by.clone()),
            (Some(TaskStatus::Pending), TaskStatus::InProgress, By::Seat(worker.clone())),
        );
        assert_eq!(
            (history[2].from, history[2].to, history[2].by.clone()),
            (Some(TaskStatus::InProgress), TaskStatus::Completed, By::Seat(worker)),
        );
    }
}
