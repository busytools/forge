//! Durable archive: task rows whose tree has closed.
//!
//! Append-only, and never read by the live board - the retro and any
//! history view read here. Rows leave only when their project is deleted.

use anyhow::Context;
use forge_primitives::tasks::Task;
use redb::{ReadableTable, TableDefinition};

use super::Db;

const TASK_ARCHIVE: TableDefinition<&str, &[u8]> = TableDefinition::new("tasks_archive");

/// The row key for `task`: its project as well as its id.
fn row_key(task: &Task) -> String {
    format!("{}\u{0}{}", task.project_name, task.id.as_str())
}

/// Append `tasks`; never rewrites what is there.
pub fn append(db: &Db, tasks: &[Task]) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(TASK_ARCHIVE)?;
        for task in tasks {
            let value = serde_json::to_vec(task).context("serialize archived task")?;
            table.insert(row_key(task).as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// `project`'s archived rows.
pub fn list_for_project(db: &Db, project: &str) -> anyhow::Result<Vec<Task>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(TASK_ARCHIVE) {
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
        match serde_json::from_slice::<Task>(value.value()) {
            Ok(task) => out.push(task),
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::task_archive",
                row = %key.value(),
                error = %err,
                "skipping archived row that failed to decode",
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use forge_primitives::tasks::{Task, TaskId, TaskStatus};
    use tempfile::tempdir;

    fn open_db(dir: &Path) -> Db {
        Db::open(&dir.join("db.redb")).expect("open db")
    }

    fn archived_task(id: &str, project: &str) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: project.to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status: TaskStatus::Completed,
            owner: None,
            parent: None,
            waiting_on: None,
            estimate: None,
            rank: None,
            verify: None,
            links: Vec::new(),
            attempt: 0,
            archived_at: Some(std::time::SystemTime::UNIX_EPOCH),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn archived_rows_append_and_read_back_for_their_project() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        append(&db, &[archived_task("t-1", "forge"), archived_task("t-2", "other")])
            .expect("append");
        let forge = list_for_project(&db, "forge").expect("list");
        assert_eq!(forge.len(), 1, "only forge's archived rows: {forge:?}");
        assert_eq!(forge[0].id, TaskId::from("t-1"));
        assert!(forge[0].archived_at.is_some(), "an archived row carries its stamp");
    }

    #[test]
    fn an_absent_archive_table_reads_as_empty() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        assert!(list_for_project(&db, "forge").expect("list").is_empty());
    }
}
