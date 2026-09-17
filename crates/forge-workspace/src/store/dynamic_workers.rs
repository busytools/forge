//! The retired `dynamic_workers` table, kept only to drain it.
//!
//! Worker persistence moved onto the `sessions` table, which is keyed by
//! `(org, project, label)` and carries the same re-spawn arguments. This
//! module survives for one thing: [`list_all`], which the boot's sweep
//! reads, [`delete`], which removes each row once it has been copied, and
//! [`drop_table`], which then removes the table. Do not add a write path
//! back.

use redb::{ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

use super::Db;

const DYNAMIC_WORKERS: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("dynamic_workers");

/// A persisted worker's re-spawn args, as the retired table stored them.
/// The spawning lead's session_id was deliberately absent: a re-spawn
/// re-parents to whatever lead is current on reconnect.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicWorker {
    pub project_key: String,
    pub label: String,
    pub charter: String,
    pub kick: Option<String>,
    /// Re-orient message delivered instead of the generic forge restart
    /// note when this worker resumes; `None` keeps the generic note.
    pub resume_kick: Option<String>,
    /// Whether this worker was spawned to be talked to directly, and so
    /// keeps the built-in `AskUserQuestion` tool. `default` because a
    /// bare `bool` would otherwise fail to decode every row written
    /// before the field existed, and an undecodable row is skipped
    /// rather than reported.
    #[serde(default)]
    pub interactive: bool,
}

/// Every persisted worker, in key order. The one-time sweep into the
/// `sessions` table reads the whole set, not one project's slice.
pub fn list_all(db: &Db) -> anyhow::Result<Vec<DynamicWorker>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(DYNAMIC_WORKERS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        match serde_json::from_slice::<DynamicWorker>(value.value()) {
            Ok(worker) => out.push(worker),
            // One undecodable record (schema drift, a corrupt blob) must not
            // wipe the rest of the durable set - skip it and warn. The key
            // stays readable even when the value doesn't, so name which
            // worker lost durability.
            Err(err) => {
                let (row_project, row_label) = key.value();
                tracing::warn!(
                    target: "forge_workspace::store::dynamic_workers",
                    project = %row_project,
                    label = %row_label,
                    error = %err,
                    "skipping dynamic worker record that failed to decode",
                );
            }
        }
    }
    Ok(out)
}

/// How many rows the table holds, readable or not.
///
/// [`list_all`] skips a value that will not decode, which is right - one
/// corrupt record must not wipe the rest of the durable set - but it makes
/// the decoded rows an undercount of what is there. The sweep compares
/// this with what it moved, so the count has to come from the table.
pub fn count(db: &Db) -> anyhow::Result<usize> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(DYNAMIC_WORKERS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    Ok(usize::try_from(table.len()?)?)
}

/// Remove one row, once the sweep has copied it into `sessions`. This is
/// what makes the sweep's own progress durable: a row still here is a row
/// whose arguments exist nowhere else yet, whatever happened to an earlier
/// boot.
pub fn delete(db: &Db, project_key: &str, label: &str) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let removed = match txn.open_table(DYNAMIC_WORKERS) {
        Ok(mut table) => table.remove((project_key, label))?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(e.into()),
    };
    txn.commit()?;
    Ok(removed)
}

/// Remove the table, once the sweep has drained it. Returns whether a
/// table was there to drop. Callers compact afterwards, because dropping
/// a table reclaims nothing on its own.
pub fn drop_table(db: &Db) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    // `false` for a store that never had the table, which is not an
    // error: the boot drains cleanly either way.
    let dropped = txn.delete_table(DYNAMIC_WORKERS)?;
    txn.commit()?;
    Ok(dropped)
}

/// Plant a row, so a test can exercise the sweep against a store that
/// still has the old table. Production has no write path here.
#[cfg(test)]
pub(crate) fn insert_for_test(db: &Db, worker: &DynamicWorker) -> anyhow::Result<()> {
    put_raw_for_test(db, &worker.project_key, &worker.label, &serde_json::to_vec(worker)?)
}

/// Plant raw bytes at a row, so a test can exercise the sweep against an
/// entry whose value will not decode.
#[cfg(test)]
pub(crate) fn put_raw_for_test(
    db: &Db,
    project_key: &str,
    label: &str,
    value: &[u8],
) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(DYNAMIC_WORKERS)?;
        table.insert((project_key, label), value)?;
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn worker(project: &str, label: &str) -> DynamicWorker {
        DynamicWorker {
            project_key: project.to_owned(),
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            kick: Some(format!("kick for {label}")),
            resume_kick: Some(format!("resume kick for {label}")),
            interactive: false,
        }
    }

    /// A row persisted before `interactive` existed decodes as
    /// not-interactive. `interactive` is a bare `bool`, so without
    /// `#[serde(default)]` every already-persisted row fails to decode
    /// and the sweep skips it - the worker silently stops being
    /// re-spawned at all.
    #[test]
    fn row_written_before_interactive_existed_loads_as_not_interactive() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let old_shape = serde_json::json!({
            "project_key": "proj-a",
            "label": "steward",
            "charter": "mind the queues",
            "kick": "begin",
            "resume_kick": "re-read the notes",
        });
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(DYNAMIC_WORKERS).expect("open table");
            let value = serde_json::to_vec(&old_shape).expect("serialize old row");
            table.insert(("proj-a", "steward"), value.as_slice()).expect("insert old row");
        }
        txn.commit().expect("commit");

        let loaded = list_all(&db).expect("list");
        assert_eq!(loaded.len(), 1, "a row written before `interactive` existed still decodes");
        assert_eq!(
            loaded[0],
            DynamicWorker {
                project_key: "proj-a".to_owned(),
                label: "steward".to_owned(),
                charter: "mind the queues".to_owned(),
                kick: Some("begin".to_owned()),
                resume_kick: Some("re-read the notes".to_owned()),
                interactive: false,
            },
            "every prior field survives and an absent `interactive` reads as not-interactive",
        );
    }

    #[test]
    fn list_all_round_trips_and_skips_a_corrupt_row() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        insert_for_test(&db, &worker("proj-a", "reviewer")).expect("insert reviewer");
        insert_for_test(&db, &worker("proj-b", "tester")).expect("insert tester");
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(DYNAMIC_WORKERS).expect("open table");
            table.insert(("proj-a", "corrupt"), "not a worker".as_bytes()).expect("insert corrupt");
        }
        txn.commit().expect("commit");

        let all = list_all(&db).expect("list tolerates the corrupt blob");
        assert_eq!(all.len(), 2, "the good records survive a corrupt sibling");
        assert_eq!(
            all[1].resume_kick.as_deref(),
            Some("resume kick for tester"),
            "the record round-trips through redb",
        );
    }

    /// Dropping the table reports whether it was there, and a second
    /// drop is not an error - a store that never had the table opens and
    /// drains cleanly either way.
    #[test]
    fn drop_table_reports_whether_the_table_was_there() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        assert!(
            !drop_table(&db).expect("drop an absent table"),
            "a store that never had the table drops nothing",
        );

        insert_for_test(&db, &worker("proj-a", "reviewer")).expect("insert reviewer");
        assert!(drop_table(&db).expect("drop the table"), "a planted table drops to true");
        assert!(list_all(&db).expect("list a dropped table").is_empty(), "and its rows are gone");
    }
}
