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

/// Where a task is in its life. Live work only: a finished task leaves
/// the store rather than acquiring an archive state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Blocked,
    Completed,
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
    /// The PR or path this work produced.
    pub artifact: Option<String>,
    pub estimate: Option<String>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_round_trips_through_json_with_every_field_set() {
        let task = Task {
            id: TaskId::from("t-1"),
            project_name: "forge".to_owned(),
            subject: "Merge peers and workers into agents".to_owned(),
            active_form: Some("Merging peers and workers into agents".to_owned()),
            detail: Some("One agents__ MCP family.".to_owned()),
            status: TaskStatus::InProgress,
            owner: Some(SessionSlot::worker("Busytools", "forge", "agents-merge")),
            parent: None,
            artifact: Some("PR #1173".to_owned()),
            estimate: Some("1d".to_owned()),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let json = serde_json::to_vec(&task).expect("serialize");
        let back: Task = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back, task, "every field survives a store round trip");
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
            TaskStatus::Blocked,
            TaskStatus::Completed,
        ]
        .into_iter()
        .map(|status| serde_json::to_string(&status).expect("serialize"))
        .collect();
        assert_eq!(
            spellings,
            ["\"pending\"", "\"in_progress\"", "\"blocked\"", "\"completed\""],
            "the status spellings the tools offer and the store reads back",
        );
    }
}
