//! Durable task history: one append-only row per status transition.
//!
//! Read by the board's worked-time computation and the retro's metrics;
//! never rewritten. Keys carry project, task and the instant, so a read
//! comes back grouped by task and ordered by time within each.

use anyhow::Context;
use forge_primitives::tasks::TaskTransition;
use redb::{ReadableTable, TableDefinition};

use super::Db;

const TASK_HISTORY: TableDefinition<&str, &[u8]> = TableDefinition::new("task_history");

/// The row key for one transition: project, task, and the instant as
/// zero-padded nanos, so a task's rows read in the order they happened.
fn row_key(transition: &TaskTransition) -> String {
    let nanos =
        transition.at.duration_since(std::time::SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("{}\u{0}{}\u{0}{nanos:040}", transition.project_name, transition.task_id.as_str())
}

/// Append `transitions`; never rewrites what is there.
pub fn append(db: &Db, transitions: &[TaskTransition]) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(TASK_HISTORY)?;
        for transition in transitions {
            let value = serde_json::to_vec(transition).context("serialize transition")?;
            table.insert(row_key(transition).as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// `project`'s transitions, grouped by task and ordered by time.
pub fn list_for_project(db: &Db, project: &str) -> anyhow::Result<Vec<TaskTransition>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(TASK_HISTORY) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let prefix = format!("{project}\u{0}");
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        if !key.value().starts_with(&prefix) {
            continue;
        }
        match serde_json::from_slice::<TaskTransition>(value.value()) {
            Ok(transition) => out.push(transition),
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::task_history",
                row = %key.value(),
                error = %err,
                "skipping history row that failed to decode",
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use forge_primitives::tasks::{By, TaskId, TaskStatus};
    use tempfile::tempdir;

    fn open_db(dir: &Path) -> Db {
        Db::open(&dir.join("db.redb")).expect("open db")
    }

    fn transition(task: &str, project: &str, at_secs: u64, to: TaskStatus) -> TaskTransition {
        TaskTransition {
            task_id: TaskId::from(task),
            project_name: project.to_owned(),
            from: Some(TaskStatus::Pending),
            to,
            at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(at_secs),
            by: By::System,
        }
    }

    #[test]
    fn history_appends_and_reads_back_for_its_project() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        append(
            &db,
            &[
                transition("t-1", "forge", 20, TaskStatus::Completed),
                transition("t-1", "forge", 10, TaskStatus::InProgress),
                transition("t-2", "other", 30, TaskStatus::InProgress),
            ],
        )
        .expect("append");
        let forge = list_for_project(&db, "forge").expect("list");
        assert_eq!(forge.len(), 2, "only forge's transitions: {forge:?}");
        assert!(forge[0].at < forge[1].at, "oldest first within the task");
        assert_eq!(forge[0].to, TaskStatus::InProgress);
        assert_eq!(forge[1].to, TaskStatus::Completed);
    }

    #[test]
    fn an_absent_history_table_reads_as_empty() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        assert!(list_for_project(&db, "forge").expect("list").is_empty());
    }
}
