//! Dynamic-worker persistence on the redb `dynamic_workers` table.
//!
//! LLM-spawned ("dynamic") workers persist their spawn args here so a
//! forge restart can re-spawn them, the way config-driven ("static")
//! workers already resume. Keyed by `(project_key, label)` - at most one
//! dynamic worker per label per project. The whole record is stored as
//! serde-json; the session_id is deliberately NOT stored, since resume is
//! recovered from the `forge:worker:<label>` catalog tag.

use anyhow::Context;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use super::Db;

const DYNAMIC_WORKERS: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("dynamic_workers");

/// A persisted dynamic worker's re-spawn args. `charter` and `kick` are
/// the resolved values from the originating `workers__spawn` (inline or
/// role-file-loaded), so re-spawn is self-contained. The spawning lead's
/// session_id is deliberately absent: a re-spawn re-parents to whatever
/// lead is current on reconnect, so the original is never read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicWorker {
    pub project_key: String,
    pub label: String,
    pub charter: String,
    pub kick: Option<String>,
}

/// Persist a dynamic worker, replacing any prior record with the same
/// `(project_key, label)`.
pub fn insert(db: &Db, worker: &DynamicWorker) -> anyhow::Result<()> {
    let value = serde_json::to_vec(worker).context("serialize dynamic worker")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(DYNAMIC_WORKERS)?;
        table.insert((worker.project_key.as_str(), worker.label.as_str()), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Delete the dynamic worker keyed by `(project_key, label)`. Returns
/// whether a record existed.
pub fn delete(db: &Db, project_key: &str, label: &str) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let existed = {
        let mut table = txn.open_table(DYNAMIC_WORKERS)?;
        table.remove((project_key, label))?.is_some()
    };
    txn.commit()?;
    Ok(existed)
}

/// Every persisted dynamic worker for `project_key`.
pub fn list_for_project(db: &Db, project_key: &str) -> anyhow::Result<Vec<DynamicWorker>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(DYNAMIC_WORKERS) {
        Ok(t) => t,
        // A fresh database has no table until the first write; an absent
        // table is an empty list, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (_key, value) = entry?;
        match serde_json::from_slice::<DynamicWorker>(value.value()) {
            Ok(worker) if worker.project_key == project_key => out.push(worker),
            Ok(_) => {}
            // One undecodable record (schema drift, a corrupt blob) must not
            // wipe the rest of the durable set - skip it and warn.
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::dynamic_workers",
                error = %err,
                "skipping dynamic worker record that failed to decode",
            ),
        }
    }
    Ok(out)
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
        }
    }

    #[test]
    fn dynamic_worker_store_round_trip() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let w1 = worker("proj-a", "reviewer");
        let w2 = worker("proj-a", "tester");
        let w3 = worker("proj-b", "reviewer");
        insert(&db, &w1).expect("insert w1");
        insert(&db, &w2).expect("insert w2");
        insert(&db, &w3).expect("insert w3");

        let a = list_for_project(&db, "proj-a").expect("list a");
        assert_eq!(a.len(), 2, "proj-a has two dynamic workers");
        assert!(a.iter().any(|w| w.label == "reviewer") && a.iter().any(|w| w.label == "tester"));
        let b = list_for_project(&db, "proj-b").expect("list b");
        assert_eq!(b.len(), 1, "proj-b is scoped separately from proj-a");
        assert_eq!(b[0].label, "reviewer");

        // Re-insert of the same (project, label) replaces, never duplicates.
        let mut w1b = worker("proj-a", "reviewer");
        w1b.charter = "updated charter".to_owned();
        insert(&db, &w1b).expect("re-insert reviewer");
        let a = list_for_project(&db, "proj-a").expect("list a again");
        assert_eq!(a.len(), 2, "re-insert of the same key replaces, no duplicate row");
        assert_eq!(
            a.iter().find(|w| w.label == "reviewer").expect("reviewer present").charter,
            "updated charter",
        );

        // Delete is scoped by (project, label).
        assert!(
            delete(&db, "proj-a", "reviewer").expect("delete"),
            "an existing row deletes to true"
        );
        assert!(
            !delete(&db, "proj-a", "reviewer").expect("delete again"),
            "an absent row deletes to false",
        );
        assert_eq!(list_for_project(&db, "proj-a").expect("list").len(), 1);
        assert_eq!(
            list_for_project(&db, "proj-b").expect("list").len(),
            1,
            "deleting from proj-a leaves proj-b untouched",
        );
    }

    #[test]
    fn corrupt_record_is_skipped_not_fatal() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let good = worker("proj-a", "reviewer");
        insert(&db, &good).expect("insert good");

        // A blob that isn't a valid dynamic worker must not poison the load.
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(DYNAMIC_WORKERS).expect("open table");
            table.insert(("proj-a", "corrupt"), "not a worker".as_bytes()).expect("insert corrupt");
        }
        txn.commit().expect("commit");

        let a = list_for_project(&db, "proj-a").expect("list tolerates the corrupt blob");
        assert_eq!(a.len(), 1, "the good record survives a corrupt sibling");
        assert_eq!(a[0].label, "reviewer");
    }
}
