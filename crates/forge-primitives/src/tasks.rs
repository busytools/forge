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

/// What a waiting row waits on. A decision routes to the user; a
/// dependency and a resource route to the lead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingKind {
    Decision,
    Dependency,
    Resource,
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
    /// Classify a bare target by its shape: a pull-request url reads `Pr`,
    /// everything else `Path`. Nothing decides on the kind, so a miss
    /// costs a drawn word.
    pub fn for_target(target: &str) -> Self {
        if target.contains("/pull/") { Self::Pr } else { Self::Path }
    }
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
    /// The session holding it. `None` is `unclaimed`.
    pub owner: Option<SessionSlot>,
    /// The task this one belongs under, in the same project.
    pub parent: Option<TaskId>,
    /// Why this row cannot proceed, when `status` is waiting.
    #[serde(default)]
    pub waiting_on: Option<Waiting>,
    pub estimate: Option<Estimate>,
    /// Queue order; lower reads first, ties break by creation.
    #[serde(default)]
    pub rank: Option<i64>,
    /// Whether completion waits on the user; a task inherits its epic's.
    #[serde(default)]
    pub verify: Option<Verify>,
    #[serde(default)]
    pub links: Vec<TaskLink>,
    /// Claims and send-backs, so a loop is visible.
    #[serde(default)]
    pub attempt: u32,
    /// Set when the tree archived; archived rows are history, not live work.
    #[serde(default)]
    pub archived_at: Option<SystemTime>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
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
    EstimateNudge,
    Escalate,
    QueueStall,
    WaitStall,
    Unaccounted,
    Death,
    Retro,
    EpicClose,
}

/// One fired chase: the row it was about (absent for a seat-level rung)
/// and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskChase {
    pub task_id: Option<TaskId>,
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
            owner: Some(SessionSlot::worker("Busytools", "forge", "agents-merge")),
            parent: None,
            estimate: Some(Estimate { words: "1d".to_owned(), secs: 86_400 }),
            rank: Some(10),
            verify: Some(Verify::User),
            links: vec![TaskLink {
                kind: LinkKind::Pr,
                label: Some("PR #1889".to_owned()),
                target: "https://example.invalid/pull/1889".to_owned(),
                state: Some("open".to_owned()),
                added_at: std::time::SystemTime::UNIX_EPOCH,
            }],
            waiting_on: None,
            attempt: 2,
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

    #[test]
    fn a_waiting_row_carries_its_kind_and_the_task_it_waits_on() {
        let waiting = Waiting {
            kind: Some(WaitingKind::Dependency),
            detail: Some("the board read has to land first".to_owned()),
            on: Some(TaskId::from("t-9")),
            verification: false,
        };
        let json = serde_json::to_vec(&waiting).expect("serialize");
        let back: Waiting = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, waiting, "a dependency wait keeps its blocker");
    }

    /// The migration's unstated wait: a stored `blocked` row lands here, and
    /// the board draws it as a miss rather than a blank.
    #[test]
    fn waiting_without_a_kind_is_representable() {
        let waiting = Waiting { kind: None, detail: None, on: None, verification: false };
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
            project_name: "forge".to_owned(),
            rung: ChaseRung::Unaccounted,
            at: std::time::SystemTime::UNIX_EPOCH,
        };
        let json = serde_json::to_vec(&chase).expect("serialize");
        let back: TaskChase = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, chase);
        assert_eq!(
            serde_json::to_string(&ChaseRung::EstimateNudge).expect("serialize"),
            "\"estimate_nudge\"",
            "the rung's spelling is what the store reads back",
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
