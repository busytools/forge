//! Session identity on the redb `sessions` table.
//!
//! One row per `(org, project, label)`, holding the `claude` session id
//! forge spawns or resumes that session under. The label carries the
//! role: [`LEAD_LABEL`] for a project's lead, the worker's own label
//! otherwise, so no separate kind column is needed. The value is stored
//! as serde-json and the worker-only fields are absent on a lead's row.
//!
//! A row written by [`migrate_from_dynamic_workers`] carries no id: the
//! `dynamic_workers` table never stored one, so the derivation fills it
//! in on the boot that reads the row.

use anyhow::Context;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use super::Db;
use super::dynamic_workers;

const SESSIONS: TableDefinition<(&str, &str, &str), &[u8]> = TableDefinition::new("sessions");

/// The label a project's lead is stored under. A worker stores its own
/// label, so the label alone says which role holds the row.
pub const LEAD_LABEL: &str = "lead";

/// One session's identity, plus the worker fields a re-spawn needs.
/// `session_id` is absent on a row copied from `dynamic_workers`, which
/// never stored one; every other field is worker-only, so a lead's row
/// carries none of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub org: String,
    pub project: String,
    pub label: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub charter: Option<String>,
    #[serde(default)]
    pub kick: Option<String>,
    #[serde(default)]
    pub resume_kick: Option<String>,
    #[serde(default)]
    pub interactive: Option<bool>,
}

/// A configured project as the migration needs it: its catalog key, and
/// the org and name its session rows are keyed by. `dynamic_workers` is
/// keyed by the catalog key alone, so the caller supplies the mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub key: String,
    pub org: String,
    pub name: String,
}

/// The record for `(org, project, label)`, if the store holds one.
pub fn get(
    db: &Db,
    org: &str,
    project: &str,
    label: &str,
) -> anyhow::Result<Option<SessionRecord>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(SESSIONS) {
        Ok(t) => t,
        // A fresh database has no table until the first write; an absent
        // table is an empty store, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(value) = table.get((org, project, label))? else {
        return Ok(None);
    };
    decode(value.value()).map(Some)
}

/// Write `record`, replacing any prior row for the same identity.
pub fn put(db: &Db, record: &SessionRecord) -> anyhow::Result<()> {
    let value = serde_json::to_vec(record).context("serialize session record")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(SESSIONS)?;
        table.insert(
            (record.org.as_str(), record.project.as_str(), record.label.as_str()),
            value.as_slice(),
        )?;
    }
    txn.commit()?;
    Ok(())
}

/// Fill `sessions` from `dynamic_workers`, once.
///
/// Runs on the first boot after this table exists: a worker's persisted
/// row carries no id, so the derivation fills that in when the row is
/// read. Does nothing once `sessions` holds any row, which is what makes
/// the second boot a no-op. A worker whose project the config no longer
/// names cannot be keyed by `(org, project, label)` and is left where it
/// is, warned rather than dropped silently. Returns how many rows it
/// moved.
pub fn migrate_from_dynamic_workers(
    db: &Db,
    projects: &[ProjectIdentity],
) -> anyhow::Result<usize> {
    if !is_empty(db)? {
        return Ok(0);
    }
    let mut moved = 0;
    for worker in dynamic_workers::list_all(db)? {
        let Some(project) = projects.iter().find(|p| p.key == worker.project_key) else {
            tracing::warn!(
                target: "forge_workspace::store::sessions",
                project_key = %worker.project_key,
                label = %worker.label,
                "no configured project for this worker's key; leaving its row in \
                 dynamic_workers",
            );
            continue;
        };
        put(
            db,
            &SessionRecord {
                org: project.org.clone(),
                project: project.name.clone(),
                label: worker.label,
                session_id: None,
                charter: Some(worker.charter),
                kick: worker.kick,
                resume_kick: worker.resume_kick,
                interactive: Some(worker.interactive),
            },
        )?;
        moved += 1;
    }
    if moved > 0 {
        tracing::warn!(
            target: "forge_workspace::store::sessions",
            rows = moved,
            "migrated persisted workers into the sessions table; their session ids are \
             derived from disk on this boot",
        );
    }
    Ok(moved)
}

/// Whether the store holds no session row at all.
fn is_empty(db: &Db) -> anyhow::Result<bool> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(SESSIONS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(true),
        Err(e) => return Err(e.into()),
    };
    Ok(table.iter()?.next().is_none())
}

fn decode(value: &[u8]) -> anyhow::Result<SessionRecord> {
    let mut record: SessionRecord =
        serde_json::from_slice(value).context("decode session record")?;
    // An empty string is absence, not an id: the bridge's id slot starts
    // empty, so a blank must never read back as a session to resume.
    if record.session_id.as_deref() == Some("") {
        record.session_id = None;
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn worker(project: &str, label: &str, interactive: bool) -> dynamic_workers::DynamicWorker {
        dynamic_workers::DynamicWorker {
            project_key: project.to_owned(),
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            kick: Some(format!("kick for {label}")),
            resume_kick: Some(format!("resume kick for {label}")),
            interactive,
        }
    }

    fn project(key: &str, org: &str, name: &str) -> ProjectIdentity {
        ProjectIdentity { key: key.to_owned(), org: org.to_owned(), name: name.to_owned() }
    }

    fn record(org: &str, project: &str, label: &str, session_id: Option<&str>) -> SessionRecord {
        SessionRecord {
            org: org.to_owned(),
            project: project.to_owned(),
            label: label.to_owned(),
            session_id: session_id.map(str::to_owned),
            charter: None,
            kick: None,
            resume_kick: None,
            interactive: None,
        }
    }

    /// The first boot after this table exists is the migration: every
    /// persisted worker crosses over with its fields, and its id is left
    /// for the derivation because `dynamic_workers` never stored one.
    #[test]
    fn the_first_open_copies_the_persisted_workers_over_once() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        dynamic_workers::insert(&db, &worker("proj-a", "steward", true)).expect("seed steward");
        dynamic_workers::insert(&db, &worker("proj-a", "quartermaster", false))
            .expect("seed quartermaster");

        let projects = [project("proj-a", "Personal", "forge")];
        assert_eq!(
            migrate_from_dynamic_workers(&db, &projects).expect("migrate"),
            2,
            "both persisted workers cross over",
        );

        let steward = get(&db, "Personal", "forge", "steward").expect("read").expect("steward row");
        assert_eq!(
            steward,
            SessionRecord {
                org: "Personal".to_owned(),
                project: "forge".to_owned(),
                label: "steward".to_owned(),
                session_id: None,
                charter: Some("charter for steward".to_owned()),
                kick: Some("kick for steward".to_owned()),
                resume_kick: Some("resume kick for steward".to_owned()),
                interactive: Some(true),
            },
            "the row carries the worker's fields and no id",
        );

        assert_eq!(
            migrate_from_dynamic_workers(&db, &projects).expect("second boot"),
            0,
            "a second boot copies nothing",
        );
        assert!(
            get(&db, "Personal", "forge", "quartermaster").expect("read").is_some(),
            "and leaves the rows it copied where they were",
        );
    }

    /// A project the config no longer names cannot be keyed by
    /// `(org, project, label)`. Its worker is left where it is - the
    /// `dynamic_workers` read path is untouched in this change - and the
    /// boot carries on rather than failing over one orphan.
    #[test]
    fn a_worker_whose_project_is_no_longer_configured_is_left_behind() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        dynamic_workers::insert(&db, &worker("proj-gone", "steward", false)).expect("seed");

        assert_eq!(
            migrate_from_dynamic_workers(&db, &[project("proj-a", "Personal", "forge")])
                .expect("migrate"),
            0,
            "a worker with no configured project is not copied",
        );
        assert!(get(&db, "Personal", "forge", "steward").expect("read").is_none());
        assert_eq!(
            dynamic_workers::list_all(&db).expect("list workers").len(),
            1,
            "its row is still where it was",
        );
    }

    /// A row written before this table existed has a NULL id, and a row
    /// the derivation has filled carries one: both decode, so a store
    /// written by the previous build is readable rather than skipped.
    #[test]
    fn a_row_without_an_id_and_one_with_it_both_round_trip() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        put(&db, &record("Personal", "forge", "lead", Some("id-1"))).expect("put lead");
        put(&db, &record("Personal", "forge", "steward", None)).expect("put worker");

        assert_eq!(
            get(&db, "Personal", "forge", "lead").expect("get").and_then(|row| row.session_id),
            Some("id-1".to_owned()),
        );
        assert_eq!(
            get(&db, "Personal", "forge", "steward").expect("get").map(|row| row.session_id),
            Some(None)
        );
        assert_eq!(get(&db, "Personal", "forge", "ghost").expect("get"), None);
    }

    /// An empty id is absence, not an id: the derivation must not treat
    /// a blank as an answer it can resume.
    #[test]
    fn an_empty_session_id_is_absent() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        put(&db, &record("Personal", "forge", "lead", Some(""))).expect("put");
        assert_eq!(
            get(&db, "Personal", "forge", "lead").expect("get").and_then(|row| row.session_id),
            None,
        );
    }
}
