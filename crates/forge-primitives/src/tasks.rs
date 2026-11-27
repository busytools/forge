//! Task records: the shape a declared piece of live work takes as it
//! crosses crate boundaries.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::SessionSlot;

/// Identifies a task within its project.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(String);

impl TaskId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for TaskId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TaskId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Where a task is in its life: `waiting` says what it waits on, and
/// `failed` and `canceled` are terminal without being done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Waiting,
    Completed,
    Failed,
    Canceled,
}

impl TaskStatus {
    /// Whether nothing more happens on a row in this state. The one
    /// terminal predicate: the board, the chase and the store's cascade
    /// all read it rather than spelling the set out again.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

/// What a waiting row waits on. A decision routes to the user; a
/// dependency and a resource route to the lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingKind {
    Decision,
    Dependency,
    Resource,
}

/// A lead's record of a look approval it witnessed, written when the lead
/// completes a look wait on the user's behalf. Only a lead attests, and
/// only a look: a decision is the user's to answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// The approval quoted from the user, as the lead read it.
    pub evidence: String,
    pub at: SystemTime,
}

/// Who reserved a row's pick. The sweep leaves a reserved pick for the
/// user rather than dispatching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservedBy {
    User,
}

/// Why a row cannot proceed. `kind` is absent only for a row migrated from
/// the old `blocked` spelling, which the board draws as an unstated wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    pub kind: Option<WaitingKind>,
    pub detail: Option<String>,
    /// The task this row waits on, for a dependency.
    pub on: Option<TaskId>,
    /// The verify gate: the board's action is approve / send back rather
    /// than answer.
    pub verification: bool,
    /// The lead classified this wait as parked by design - the user's own
    /// deferral - so the chase leaves it alone rather than nudging it.
    #[serde(default)]
    pub parked: bool,
    /// When the wait began, stamped as it is raised. `None` on a row stored
    /// before the field existed, which reads back as its creation.
    #[serde(default)]
    pub since: Option<SystemTime>,
}

/// Whether a row is finite work with a finish line, or standing work
/// that never finishes: the watcher, a growth loop, a weekly sweep. A
/// finite row carries an estimate and is the chase's re-plan trigger; a
/// standing row carries a cadence, is never chased for overrun, and is
/// checked when its cadence comes due.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    #[default]
    Finite,
    Standing,
}

/// The answer a decision reached: what was chosen, when, and the note
/// the user left with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionAnswer {
    pub choice: String,
    #[serde(default)]
    pub note: Option<String>,
    pub at: SystemTime,
}

/// A question a row is waiting on the user for: the ask, the options the
/// lead enumerated, and the answer once one is given. The board records
/// decisions, not conversations - the thread lives in the chat, and
/// nothing but the answer is ever written here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub answer: Option<DecisionAnswer>,
}

/// A duration estimate: the words a reader sees ("2h", "1d") and the
/// seconds the chase compares against worked time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    pub words: String,
    pub secs: u64,
}

impl Estimate {
    /// Parse "30m" / "2h" / "1d" / "1w": digits then one unit, nothing
    /// else. The one grammar the tools, the store's migration and the
    /// board all read.
    pub fn parse(words: &str) -> Option<Estimate> {
        let words = words.trim();
        let split = words.find(|c: char| !c.is_ascii_digit())?;
        let (digits, unit) = words.split_at(split);
        let n: u64 = digits.parse().ok()?;
        let secs = match unit {
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            "w" => 604_800,
            _ => return None,
        };
        (n > 0).then(|| Estimate { words: words.to_owned(), secs: n * secs })
    }
}

/// Whether a row's completion waits on the user's look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verify {
    User,
    None,
}

/// What a link points at. Generic to any VCS: a GitHub pull request and a
/// GitLab merge request are both `Pr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    Spec,
    Plan,
    Issue,
    Pr,
    Branch,
    Path,
    Other,
}

impl LinkKind {
    /// Classify a bare target by its shape: a target containing `/pull/`
    /// reads `Pr`, everything else `Path`. Nothing decides on the kind, so a
    /// miss costs a drawn word - and the key is GitHub's own path, so a
    /// GitLab merge request reads `Path` until a shape names it.
    pub fn for_target(target: &str) -> Self {
        if target.contains("/pull/") { Self::Pr } else { Self::Path }
    }
}

/// The most of an output excerpt a row keeps. A completion's excerpt is
/// the reader's evidence, not a log dump: four thousand characters is
/// enough to show a test result or a diff stat, and short enough that a
/// list answer carrying a project's rows stays deliverable.
pub const EVIDENCE_EXCERPT_MAX: usize = 4_000;

/// What a completion rests on, and forge's own check of it.
///
/// A row that changes code, docs or a page cannot move to completed
/// without one of the artifact forms: a commit that must exist in the
/// project's repository, or a path that must exist at that commit (or at
/// `HEAD` when no commit is named). The excerpt travels beside them for
/// the reader; it is never the whole claim, because output cannot be
/// checked.
///
/// `verified_at` is stamped by the gate on the move that checks the
/// artifact, and `problem` carries why the last check failed - absence is
/// never success, and an unchecked claim reads as unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub excerpt: Option<String>,
    #[serde(default)]
    pub verified_at: Option<SystemTime>,
    #[serde(default)]
    pub problem: Option<String>,
}

impl Evidence {
    /// The excerpt as a row keeps it: at most [`EVIDENCE_EXCERPT_MAX`]
    /// characters, cut on a character boundary so a multi-byte glyph is
    /// never split.
    pub fn bounded_excerpt(excerpt: &str) -> String {
        match excerpt.char_indices().nth(EVIDENCE_EXCERPT_MAX) {
            Some((cut, _)) => excerpt[..cut].to_owned(),
            None => excerpt.to_owned(),
        }
    }
}

/// One connector's look at a link, cached on the row so the board never
/// waits on a network read and a blip draws as unknown rather than as
/// success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkCheck {
    /// When the connector last read the link. Freshness is measured from
    /// here, so an unchanged read still keeps the check current.
    pub at: SystemTime,
    /// When the read last reported something DIFFERENT from the one before
    /// it. New input is measured from here: a connector re-reading the same
    /// head every five minutes has seen nothing new.
    #[serde(default)]
    pub changed_at: Option<SystemTime>,
    /// The target's state as the connector spells it (open / closed /
    /// merged), absent when the check could not say.
    #[serde(default)]
    pub state: Option<String>,
    /// The head revision the connector reported.
    #[serde(default)]
    pub head: Option<String>,
    /// The CI verdict: `passing`, `failing` or `pending`. Absent is
    /// unknown, which is never read as passing.
    #[serde(default)]
    pub ci: Option<String>,
    /// Why the last check failed, when it did.
    #[serde(default)]
    pub problem: Option<String>,
}

/// One reference a row carries: the epic's spec and plan, a task's issues
/// and PRs, anything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLink {
    pub kind: LinkKind,
    pub label: Option<String>,
    /// A url or a path.
    pub target: String,
    /// Read back where forge can (open / closed / merged).
    pub state: Option<String>,
    /// The connector's cached look at the target. `None` means nothing has
    /// checked it - the board's unknown.
    #[serde(default)]
    pub last_check: Option<LinkCheck>,
    pub added_at: SystemTime,
}

/// One piece of live work in one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub project_name: String,
    pub subject: String,
    /// The in-progress wording. The renderer uses it for a running row,
    /// so it is carried rather than derived.
    pub active_form: Option<String>,
    pub detail: Option<String>,
    pub status: TaskStatus,
    /// Finite work or standing work; a row stored before the field is
    /// finite, which is what every row was.
    #[serde(default)]
    pub kind: TaskKind,
    /// The session holding it. `None` is `unclaimed`.
    pub owner: Option<SessionSlot>,
    /// The task this one belongs under, in the same project.
    pub parent: Option<TaskId>,
    /// Why this row cannot proceed, when `status` is waiting.
    #[serde(default)]
    pub waiting_on: Option<Waiting>,
    /// The question a decision wait asks the user, its options and its
    /// answer. Absent on a row that is not a decision.
    #[serde(default)]
    pub decision: Option<Decision>,
    /// A finite row's finish line, and the chase's re-plan trigger.
    pub estimate: Option<Estimate>,
    /// A standing row's check interval, in the same grammar as an
    /// estimate: when the row is due to be asked whether it is still
    /// doing its job.
    #[serde(default)]
    pub cadence: Option<Estimate>,
    /// Queue order; lower reads first, ties break by creation.
    #[serde(default)]
    pub rank: Option<i64>,
    /// The user-look gate the row was minted with. An epic's gate governs
    /// the epic's own completion; a child never inherits it.
    #[serde(default)]
    pub verify: Option<Verify>,
    /// The lead's attestation when it completed the look on the user's
    /// behalf; `None` for the user's own approve and for rows never gated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attested: Option<Attestation>,
    /// The user reserved this row's pick: the sweep leaves it for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<ReservedBy>,
    #[serde(default)]
    pub links: Vec<TaskLink>,
    /// What the completion rests on: the artifact forge checks, the
    /// excerpt the reader sees, and when the check last ran.
    #[serde(default)]
    pub evidence: Option<Evidence>,
    /// How many writes the row has taken. An update states the version it
    /// read, so a lost write is refused visibly rather than overwriting
    /// what another seat wrote. Row creation is version 0.
    #[serde(default)]
    pub version: u64,
    /// Set when the tree archived; archived rows are history, not live work.
    #[serde(default)]
    pub archived_at: Option<SystemTime>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

impl Task {
    /// Whether a seat could start this row: pending, holding no wait, and
    /// not reserved for the user. The one ready predicate; a caller that
    /// needs more - an unowned row for dispatch, a live holder for a
    /// claim - adds that condition rather than restating this one.
    pub fn is_ready(&self) -> bool {
        self.status == TaskStatus::Pending && self.waiting_on.is_none() && self.reserved.is_none()
    }

    /// When the row's current wait began, stamped as the wait is raised.
    /// A wait stored before the stamp reads as the row's creation, never
    /// its last edit: an edit that moved the row answered nothing. The
    /// one wait start the marks, the chase and the attention item read.
    pub fn wait_started(&self) -> Option<SystemTime> {
        let wait = self.waiting_on.as_ref()?;
        Some(wait.since.unwrap_or(self.created_at))
    }

    /// How long the row has held its wait, from that same start.
    pub fn wait_age_secs(&self, now: SystemTime) -> Option<u64> {
        let started = self.wait_started()?;
        Some(now.duration_since(started).map_or(0, |d| d.as_secs()))
    }
}

/// Who moved a row: the seat that called, the user from the board, or the
/// core itself (a cleared dependency, the verify gate, archiving).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum By {
    Seat(SessionSlot),
    User,
    System,
}

/// One status transition, appended to the history the board's worked time
/// and the retro's metrics are computed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTransition {
    pub task_id: TaskId,
    pub project_name: String,
    pub from: Option<TaskStatus>,
    pub to: TaskStatus,
    pub at: SystemTime,
    pub by: By,
}

/// A chase rung that fired. Each fires once per row (or seat) per
/// threshold; the chase log is what makes that a fact rather than a hope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChaseRung {
    /// A finite row's worked time passed its estimate: the one re-plan ask
    /// to its owner (which parks the row), then nothing until an estimate
    /// is restated.
    EstimateNudge,
    /// The re-plan ask went unanswered past the cadence: the lead hears
    /// once.
    Escalate,
    QueueStall,
    WaitStall,
    /// A wait resolved: the waiter hears once that the thing it waited on
    /// has ended.
    WaitResolved,
    Unaccounted,
    Death,
    Retro,
    EpicClose,
    /// A standing row's cadence came due: is it doing its job, should it
    /// change shape or end?
    Cadence,
    /// An in-progress row has not moved past its band, and its owner is
    /// live: the row is stuck rather than gone.
    Stuck,
    /// A connector reported something about a link after the row's last
    /// touch: the row may be obsolete or urgent now.
    NewInput,
    /// Retired: a wait on the user is never chased by an agent, and no
    /// code fires this. Kept so a firing already in the log decodes.
    UserWaitDigest,
}

/// One fired chase: the row it was about (absent for a seat-level rung),
/// the rung's own subject where a row id does not identify it, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskChase {
    pub task_id: Option<TaskId>,
    /// The rung's subject where a row id does not identify it: the
    /// worker a seat-level rung names, or the estimate a nudge fired
    /// against - so a restated estimate arms the nudge again. `default`
    /// reads a row written before the field.
    #[serde(default)]
    pub subject: Option<String>,
    pub project_name: String,
    pub rung: ChaseRung,
    pub at: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_task() -> Task {
        Task {
            id: TaskId::from("t-1"),
            project_name: "forge".to_owned(),
            subject: "Merge peers and workers into agents".to_owned(),
            active_form: Some("Merging peers and workers into agents".to_owned()),
            detail: Some("One agents__ MCP family.".to_owned()),
            status: TaskStatus::InProgress,
            kind: TaskKind::Finite,
            owner: Some(SessionSlot::worker("Busytools", "forge", "agents-merge")),
            parent: None,
            decision: Some(Decision {
                question: "Merge the families?".to_owned(),
                options: vec!["merge".to_owned(), "wait".to_owned()],
                answer: Some(DecisionAnswer {
                    choice: "merge".to_owned(),
                    note: Some("after the bench".to_owned()),
                    at: std::time::SystemTime::UNIX_EPOCH,
                }),
            }),
            estimate: Some(Estimate { words: "1d".to_owned(), secs: 86_400 }),
            cadence: None,
            rank: Some(10),
            verify: Some(Verify::User),
            attested: Some(Attestation {
                evidence: "looks good".to_owned(),
                at: std::time::SystemTime::UNIX_EPOCH,
            }),
            reserved: Some(ReservedBy::User),
            links: vec![TaskLink {
                kind: LinkKind::Pr,
                label: Some("PR #1889".to_owned()),
                target: "https://example.invalid/pull/1889".to_owned(),
                state: Some("open".to_owned()),
                last_check: None,
                added_at: std::time::SystemTime::UNIX_EPOCH,
            }],
            waiting_on: None,
            evidence: Some(Evidence {
                commit: Some("0123456789abcdef".to_owned()),
                path: Some("crates/forge-primitives/src/tasks.rs".to_owned()),
                excerpt: Some("cargo nextest run: 206 passed".to_owned()),
                verified_at: Some(std::time::SystemTime::UNIX_EPOCH),
                problem: None,
            }),
            version: 3,
            archived_at: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn task_round_trips_through_json_with_every_field_set() {
        let task = sample_task();
        let json = serde_json::to_vec(&task).expect("serialize");
        let back: Task = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, task, "every field survives a store round trip");
    }

    /// A standing row is checked on its cadence, never chased for
    /// overrun, so it carries a cadence where a finite row carries an
    /// estimate.
    #[test]
    fn a_standing_row_carries_a_cadence() {
        let task = Task {
            kind: TaskKind::Standing,
            cadence: Estimate::parse("1d"),
            estimate: None,
            ..sample_task()
        };
        let json = serde_json::to_vec(&task).expect("serialize");
        let back: Task = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back.kind, TaskKind::Standing);
        assert_eq!(back.cadence, Estimate::parse("1d"));
        assert_eq!(back.estimate, None, "a standing row has no finish line to overrun");
        assert_eq!(
            serde_json::to_string(&TaskKind::Standing).expect("serialize"),
            "\"standing\"",
            "the spelling the store writes and the tools read",
        );
    }

    /// The board records decisions, not conversations: the question, the
    /// options the lead enumerated, and the answer once given - what was
    /// chosen, when, and the note. Nothing else is written to it.
    #[test]
    fn a_decision_record_keeps_its_question_options_and_answer() {
        let decision = Decision {
            question: "Ship the parser rewrite behind a flag?".to_owned(),
            options: vec!["flag it".to_owned(), "ship it outright".to_owned()],
            answer: Some(DecisionAnswer {
                choice: "flag it".to_owned(),
                note: Some("until the bench is green".to_owned()),
                at: std::time::SystemTime::UNIX_EPOCH,
            }),
        };
        let json = serde_json::to_vec(&decision).expect("serialize");
        let back: Decision = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, decision, "the record keeps the ask, the options and the answer");
        let unanswered = Decision { answer: None, ..decision };
        let back: Decision =
            serde_json::from_slice(&serde_json::to_vec(&unanswered).expect("serialize"))
                .expect("deserialize");
        assert_eq!(back.answer, None, "a row waiting on the question carries no answer yet");
    }

    /// The stored rows outlive the struct that wrote them: a row written
    /// before the kind, the cadence, the decision and the version reads
    /// back as finite work at version zero rather than failing the set.
    #[test]
    fn a_row_written_before_the_new_fields_reads_as_finite_and_unversioned() {
        let mut older = serde_json::to_value(sample_task()).expect("serialize");
        let object = older.as_object_mut().expect("a row is an object");
        for gone in ["kind", "cadence", "decision", "version", "attempt", "evidence"] {
            object.remove(gone);
        }
        object.insert("attempt".to_owned(), serde_json::json!(3));
        let read: Task = serde_json::from_value(older).expect("a row written before the fields");
        assert_eq!(read.kind, TaskKind::Finite, "an old row is finite work");
        assert_eq!(read.cadence, None);
        assert_eq!(read.decision, None);
        assert_eq!(read.evidence, None, "a row written before the gate carries no claim");
        assert_eq!(read.version, 0, "nothing names a version it has never written");
    }

    /// The one terminal predicate: the statuses nothing more happens on.
    /// `failed` is terminal without being done, and `canceled` is a
    /// deliberate withdrawal - both end the row.
    #[test]
    fn the_terminal_statuses_are_the_ones_nothing_more_happens_on() {
        for live in [TaskStatus::Pending, TaskStatus::InProgress, TaskStatus::Waiting] {
            assert!(!live.is_terminal(), "{live:?} is work in play");
        }
        for ended in [TaskStatus::Completed, TaskStatus::Failed, TaskStatus::Canceled] {
            assert!(ended.is_terminal(), "{ended:?} is an end");
        }
    }

    /// The one ready predicate: pending, holding no wait, not reserved.
    /// Callers that need more (an unowned row for dispatch, a live holder
    /// for a claim) add their own condition rather than restating this one.
    #[test]
    fn a_row_is_ready_only_when_pending_unwaited_and_unreserved() {
        let plain =
            Task { status: TaskStatus::Pending, reserved: None, waiting_on: None, ..sample_task() };
        let ready = plain.clone();
        assert!(ready.is_ready(), "a pending row with nothing in its way can be started");
        let held = Task {
            waiting_on: Some(Waiting {
                kind: Some(WaitingKind::Dependency),
                detail: None,
                on: None,
                verification: false,
                parked: false,
                since: None,
            }),
            ..plain.clone()
        };
        assert!(!held.is_ready(), "a wait the status does not show still blocks a start");
        let kept = Task { reserved: Some(ReservedBy::User), ..plain.clone() };
        assert!(!kept.is_ready(), "a pick reserved for the user is nobody else's to start");
        for live in [TaskStatus::InProgress, TaskStatus::Waiting, TaskStatus::Completed] {
            assert!(!Task { status: live, ..plain.clone() }.is_ready(), "{live:?} is not a start");
        }
    }

    /// The one wait age: from the wait's own start, never from the row's
    /// last edit (which moves for a link, a park or a drag that answered
    /// nothing), and from the creation for a wait stored before the stamp.
    #[test]
    fn the_wait_age_runs_from_the_waits_own_start() {
        let waiting = |since: Option<SystemTime>| Waiting {
            kind: Some(WaitingKind::Resource),
            detail: Some("a CI run".to_owned()),
            on: None,
            verification: false,
            parked: false,
            since,
        };
        let started = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(150);
        let row = Task {
            created_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100),
            updated_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(200),
            waiting_on: Some(waiting(Some(started))),
            ..sample_task()
        };
        let now = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(400);
        assert_eq!(row.wait_started(), Some(started), "the wait's own start, not the edit");
        assert_eq!(row.wait_age_secs(now), Some(250), "the age counts from that start");

        let older = Task { waiting_on: Some(waiting(None)), ..row.clone() };
        assert_eq!(
            older.wait_started(),
            Some(older.created_at),
            "a wait stored before the stamp reads as its creation",
        );
        let not_waiting = Task { waiting_on: None, ..row };
        assert_eq!(not_waiting.wait_age_secs(now), None, "a row holding no wait has no wait age");
    }

    /// The evidence block is what a completion rests on: the artifact, the
    /// excerpt, and the gate's own verdict. An unverified claim carries no
    /// stamp, which is the wire's `unknown`.
    #[test]
    fn an_evidence_block_keeps_the_artifact_its_excerpt_and_its_verdict() {
        let evidence = Evidence {
            commit: Some("0123456789abcdef".to_owned()),
            path: Some("crates/forge-workspace/src/board.rs".to_owned()),
            excerpt: Some("12 tests run: 12 passed".to_owned()),
            verified_at: Some(std::time::SystemTime::UNIX_EPOCH),
            problem: None,
        };
        let json = serde_json::to_vec(&evidence).expect("serialize");
        let back: Evidence = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, evidence, "every part of the claim survives the store");

        let unchecked = Evidence { verified_at: None, problem: None, ..evidence.clone() };
        let back: Evidence =
            serde_json::from_slice(&serde_json::to_vec(&unchecked).expect("serialize"))
                .expect("deserialize");
        assert_eq!(back.verified_at, None, "nothing verified it, so it reads as unknown");
        let failed = Evidence {
            verified_at: None,
            problem: Some("commit deadbeef is not in /tmp/repo".to_owned()),
            ..evidence
        };
        assert_eq!(
            serde_json::from_slice::<Evidence>(&serde_json::to_vec(&failed).expect("serialize"))
                .expect("deserialize")
                .problem
                .as_deref(),
            Some("commit deadbeef is not in /tmp/repo"),
            "a failed lookup keeps why",
        );
    }

    /// The excerpt is bounded on a character boundary: a long log does not
    /// ride, and a split glyph does not corrupt the record.
    #[test]
    fn an_excerpt_is_bounded_on_a_character_boundary() {
        let short = "12 tests run: 12 passed";
        assert_eq!(Evidence::bounded_excerpt(short), short, "a short excerpt rides whole");

        let long = "y".repeat(EVIDENCE_EXCERPT_MAX + 500);
        assert_eq!(Evidence::bounded_excerpt(&long).chars().count(), EVIDENCE_EXCERPT_MAX);

        // A multi-byte glyph straddling the cut is dropped whole rather
        // than split into an invalid string.
        let glyphs = "é".repeat(EVIDENCE_EXCERPT_MAX + 1);
        let cut = Evidence::bounded_excerpt(&glyphs);
        assert_eq!(cut.chars().count(), EVIDENCE_EXCERPT_MAX);
        assert!(cut.is_char_boundary(cut.len()));
    }

    /// The attempt tick is cut: nothing acted on it, and the board keeps
    /// the reason a row was sent back in its detail instead.
    #[test]
    fn a_row_no_longer_carries_an_attempt() {
        let json = serde_json::to_value(sample_task()).expect("serialize");
        let wrote_attempt = json.get("attempt").is_some();
        assert!(!wrote_attempt, "the record writes no attempt any more: {json}");
    }

    #[test]
    fn a_waiting_row_carries_its_kind_and_the_task_it_waits_on() {
        let waiting = Waiting {
            kind: Some(WaitingKind::Dependency),
            detail: Some("the board read has to land first".to_owned()),
            on: Some(TaskId::from("t-9")),
            verification: false,
            parked: false,
            since: None,
        };
        let json = serde_json::to_vec(&waiting).expect("serialize");
        let back: Waiting = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, waiting, "a dependency wait keeps its blocker");
    }

    /// The migration's unstated wait: a stored `blocked` row lands here, and
    /// the board draws it as a miss rather than a blank.
    #[test]
    fn waiting_without_a_kind_is_representable() {
        let waiting = Waiting {
            kind: None,
            detail: None,
            on: None,
            verification: false,
            parked: false,
            since: None,
        };
        let json = serde_json::to_vec(&waiting).expect("serialize");
        let back: Waiting = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back.kind, None);
    }

    #[test]
    fn an_estimate_keeps_its_words_and_its_seconds() {
        let estimate = Estimate { words: "1d".to_owned(), secs: 86_400 };
        let json = serde_json::to_vec(&estimate).expect("serialize");
        let back: Estimate = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(
            back, estimate,
            "the words are what a reader sees; the seconds are what the chase reads",
        );
    }

    #[test]
    fn a_link_keeps_its_kind_label_target_and_state() {
        let link = TaskLink {
            kind: LinkKind::Pr,
            label: Some("PR #1889".to_owned()),
            target: "https://example.invalid/pull/1".to_owned(),
            state: Some("open".to_owned()),
            last_check: None,
            added_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let json = serde_json::to_vec(&link).expect("serialize");
        let back: TaskLink = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, link);
    }

    /// A bare target's shape is all a fold can know: a pull-request url
    /// reads `Pr`, everything else `Path`. Nothing reads the kind to make
    /// a decision, so a miss costs a drawn word, not a step.
    #[test]
    fn a_target_is_classified_from_its_shape() {
        assert_eq!(
            LinkKind::for_target("https://github.com/busytools/forge/pull/1889"),
            LinkKind::Pr,
        );
        assert_eq!(LinkKind::for_target("docs/superpowers/specs/x.md"), LinkKind::Path);
    }

    /// The grammar the tools and the store share: digits then one unit.
    #[test]
    fn an_estimate_parses_its_grammar() {
        assert_eq!(Estimate::parse("30m"), Some(Estimate { words: "30m".to_owned(), secs: 1_800 }));
        assert_eq!(Estimate::parse("2h"), Some(Estimate { words: "2h".to_owned(), secs: 7_200 }));
        assert_eq!(Estimate::parse("1d"), Some(Estimate { words: "1d".to_owned(), secs: 86_400 }));
        assert_eq!(Estimate::parse("1w"), Some(Estimate { words: "1w".to_owned(), secs: 604_800 }));
        for refused in ["soonish", "1x", "0h", "d", "1 d", ""] {
            assert_eq!(Estimate::parse(refused), None, "{refused:?} is not a duration");
        }
    }

    #[test]
    fn a_transition_records_from_to_at_and_by() {
        let transition = TaskTransition {
            task_id: TaskId::from("t-1"),
            project_name: "forge".to_owned(),
            from: Some(TaskStatus::Pending),
            to: TaskStatus::InProgress,
            at: std::time::SystemTime::UNIX_EPOCH,
            by: By::Seat(SessionSlot::worker("Busytools", "forge", "task-board")),
        };
        let json = serde_json::to_vec(&transition).expect("serialize");
        let back: TaskTransition = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, transition);
        let json = serde_json::to_vec(&By::System).expect("serialize");
        assert_eq!(serde_json::from_slice::<By>(&json).expect("deserialize"), By::System);
    }

    /// A chase may belong to a seat rather than a row - the unaccounted
    /// rung names a worker, not a task.
    #[test]
    fn a_chase_records_its_rung_and_may_have_no_task() {
        let chase = TaskChase {
            task_id: None,
            subject: Some("w-1".to_owned()),
            project_name: "forge".to_owned(),
            rung: ChaseRung::Unaccounted,
            at: std::time::SystemTime::UNIX_EPOCH,
        };
        let json = serde_json::to_vec(&chase).expect("serialize");
        let back: TaskChase = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, chase);
        // **A row written before the field still reads.** The log's rows
        // outlive the struct that wrote them, so a decode that demanded the
        // key would unread every chase already stored.
        let mut older: serde_json::Value = serde_json::from_slice(&json).expect("the row as data");
        older.as_object_mut().expect("a row is an object").remove("subject");
        let read: TaskChase =
            serde_json::from_value(older).expect("a row written before the field decodes");
        assert_eq!(read.subject, None, "and carries no subject");
        assert_eq!(
            serde_json::to_string(&ChaseRung::EstimateNudge).expect("serialize"),
            "\"estimate_nudge\"",
            "the rung's spelling is what the store reads back",
        );
        assert_eq!(
            serde_json::to_string(&ChaseRung::UserWaitDigest).expect("serialize"),
            "\"user_wait_digest\"",
            "the newest rung's spelling is what the store reads back",
        );
    }

    /// The spelling each status takes in the store and in the tools'
    /// `status` enum. Rows are written with no schema on disk, so a renamed
    /// spelling silently unreads every task already written under the old
    /// one.
    #[test]
    fn the_status_spellings_the_store_and_the_tools_share_are_stable() {
        let spellings: Vec<String> = [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Waiting,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Canceled,
        ]
        .into_iter()
        .map(|status| serde_json::to_string(&status).expect("serialize"))
        .collect();
        assert_eq!(
            spellings,
            [
                "\"pending\"",
                "\"in_progress\"",
                "\"waiting\"",
                "\"completed\"",
                "\"failed\"",
                "\"canceled\""
            ],
            "the status spellings the tools offer and the store reads back",
        );
    }
}
