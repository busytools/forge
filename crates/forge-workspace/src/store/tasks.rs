//! Durable task persistence on the redb `tasks` table.
//!
//! The whole [`Task`] record is stored as serde-json keyed by its
//! project and id - no field schema on disk, so a type change needs no
//! table migration. Machine-local, like the crons beside it.

use anyhow::Context;
use forge_primitives::tasks::Task;
use redb::{ReadableTable, TableDefinition};

use super::Db;

const TASKS: TableDefinition<&str, &[u8]> = TableDefinition::new("tasks");

/// The row key for `task`: its project as well as its id, separated by a
/// NUL a project name cannot carry.
fn row_key(task: &Task) -> String {
    format!("{}\u{0}{}", task.project_name, task.id.as_str())
}

/// Every persisted task.
pub fn list(db: &Db) -> anyhow::Result<Vec<Task>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(TASKS) {
        Ok(t) => t,
        // A fresh database has no table until the first write; an absent
        // table is an empty list, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        match serde_json::from_slice::<Task>(value.value()) {
            Ok(task) => out.push(task),
            // One undecodable record (schema drift, a corrupt blob) must not
            // wipe the rest of the durable set - skip it and warn.
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::tasks",
                row = %key.value(),
                error = %err,
                "skipping task record that failed to decode",
            ),
        }
    }
    Ok(out)
}

/// Overwrite the table with `tasks`: clear every existing row then insert
/// the full set in one write txn. Backs every workspace
/// persist-after-mutation.
pub fn replace_all(db: &Db, tasks: &[Task]) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(TASKS)?;
        let existing: Vec<String> = table
            .iter()?
            .map(|entry| entry.map(|(k, _)| k.value().to_owned()))
            .collect::<Result<_, _>>()?;
        for key in existing {
            table.remove(key.as_str())?;
        }
        for task in tasks {
            let value = serde_json::to_vec(task).context("serialize task")?;
            table.insert(row_key(task).as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use forge_primitives::tasks::{TaskId, TaskStatus};
    use tempfile::tempdir;

    fn open_db(dir: &Path) -> Db {
        Db::open(&dir.join("db.redb")).expect("open db")
    }

    fn sample_task(id: &str, project: &str) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: project.to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status: TaskStatus::Pending,
            owner: None,
            parent: None,
            artifact: None,
            estimate: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn tasks_are_scoped_by_project() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let forge_task = sample_task("t-1", "forge");
        let other_task = sample_task("t-2", "other");
        replace_all(&db, &[forge_task.clone(), other_task]).expect("write");
        let read = list(&db).expect("list");
        assert_eq!(read.len(), 2, "both projects' tasks are stored");
        assert!(
            read.iter().any(|t| t.project_name == "forge" && t.id == TaskId::from("t-1")),
            "the forge task is stored under its own project",
        );
    }

    #[test]
    fn the_same_id_in_two_projects_stores_two_rows() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        replace_all(&db, &[sample_task("t-1", "forge"), sample_task("t-1", "other")])
            .expect("write");
        let read = list(&db).expect("list");
        assert_eq!(read.len(), 2, "one project's row cannot overwrite another's on a shared id");
    }

    #[test]
    fn an_absent_table_reads_as_empty_not_an_error() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        assert!(list(&db).expect("list").is_empty(), "a fresh database has no tasks");
    }
}
