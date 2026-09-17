//! Session identity on the redb `sessions` table.
//!
//! One row per `(org, project, label)`, holding the `claude` session id
//! forge spawns or resumes that session under. The label carries the
//! role: [`LEAD_LABEL`] for a project's lead, the worker's own label
//! otherwise, so no separate kind column is needed. The value is stored
//! as serde-json and the worker-only fields are absent on a lead's row.
//!
//! A worker row written by [`migrate_from_dynamic_workers`] gets its
//! spawn args from `dynamic_workers`, which never stored an id. A worker
//! that has no row yet therefore starts fresh under a newly minted id; one
//! that already has a row keeps the occupant that row names.

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
/// `session_id` is absent on a worker row the sweep creates, because
/// `dynamic_workers` never stored one; every other field is worker-only,
/// so a lead's row carries none of them.
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
    /// Whether the worker runs in a worktree, which fixes the directory
    /// its session starts in: `worker_tag_dir` reads it to compose the
    /// path. Absent on a row written before the field existed, and the
    /// spawn probes for it then - the row is what lets the launchpad and
    /// the boot wave ask the same question without a git probe of their
    /// own.
    #[serde(default)]
    pub is_git_repo: Option<bool>,
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

/// Write raw bytes at a row, so a test can plant a record that will not
/// decode. Test-only: every production write goes through [`put`].
#[cfg(test)]
pub(crate) fn put_raw_for_test(
    db: &Db,
    org: &str,
    project: &str,
    label: &str,
    value: &[u8],
) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(SESSIONS)?;
        table.insert((org, project, label), value)?;
    }
    txn.commit()?;
    Ok(())
}

/// What a sweep of `dynamic_workers` accounted for. The retired table is
/// only safe to drop when [`Self::drained`], and everything else holds it
/// for a later boot.
pub struct SweepOutcome {
    /// Rows copied into `sessions` and removed from the retired table.
    pub moved: usize,
    /// Rows left where they are: no configured project carries their
    /// key, so they cannot be keyed by `(org, project, label)`.
    pub unkeyable: usize,
    /// Rows the retired table held when the sweep looked.
    pub rows: usize,
}

impl SweepOutcome {
    /// Nothing is left in the retired table: every row it held was
    /// copied, so dropping it loses nothing. An unkeyable row counts as
    /// not drained by construction - it is in `rows` and not in `moved` -
    /// and a later boot retries it, which is what the hold is for.
    pub fn drained(&self) -> bool {
        self.moved == self.rows
    }
}

/// Move any `dynamic_workers` rows onto `sessions`, and take each one out
/// of the retired table as it lands.
///
/// A worker's persisted row carries no id, so the session it names starts
/// fresh under a newly minted one rather than resuming. An existing
/// `sessions` row for the same `(org, project, label)` keeps its id: this
/// merges the spawn args onto it, which is the only place they exist for a
/// worker spawned by the release that wrote `sessions` ids and
/// `dynamic_workers` args.
///
/// A worker whose project the config no longer names cannot be keyed by
/// `(org, project, label)`: it is left where it is, warned rather than
/// dropped silently, and it holds the table for a later boot.
///
/// Both halves are idempotent, so a boot interrupted between the copy and
/// the removal simply repeats the copy.
pub fn migrate_from_dynamic_workers(
    db: &Db,
    projects: &[ProjectIdentity],
) -> anyhow::Result<SweepOutcome> {
    // What the table HOLDS, not what decoded. An entry whose value will
    // not decode is skipped by the loop below and can never be moved, so
    // counting the decoded rows would call a table with an unreadable
    // entry drained and let the caller drop it.
    let rows = dynamic_workers::count(db)?;
    let workers = dynamic_workers::list_all(db)?;
    let mut moved = 0;
    let mut unkeyable = 0;
    for worker in &workers {
        let Some(project) = projects.iter().find(|p| p.key == worker.project_key) else {
            tracing::warn!(
                target: "forge_workspace::store::sessions",
                project_key = %worker.project_key,
                label = %worker.label,
                "no configured project for this worker's key; leaving its row in \
                 dynamic_workers",
            );
            unkeyable += 1;
            continue;
        };
        let fields = SessionRecord {
            org: project.org.clone(),
            project: project.name.clone(),
            label: worker.label.clone(),
            session_id: None,
            charter: Some(worker.charter.clone()),
            kick: worker.kick.clone(),
            resume_kick: worker.resume_kick.clone(),
            interactive: Some(worker.interactive),
            is_git_repo: None,
        };
        // An existing row is the id-bearing one: merge onto it so the
        // occupant it names survives. `update` leaves an absent field at
        // its stored value, and it reports whether there was a row to
        // merge onto.
        if !update(db, &fields)? {
            put(db, &fields)?;
        }
        dynamic_workers::delete(db, &worker.project_key, &worker.label)?;
        moved += 1;
    }
    if moved > 0 {
        tracing::info!(
            target: "forge_workspace::store::sessions",
            rows = moved,
            "moved persisted workers onto the sessions table and out of the retired one",
        );
    }
    Ok(SweepOutcome { moved, unkeyable, rows })
}

/// Every row for `(org, project)`, in label order. What a project's lead
/// reads to re-spawn its persisted workers.
pub fn list_for_project(db: &Db, org: &str, project: &str) -> anyhow::Result<Vec<SessionRecord>> {
    Ok(list_all(db)?.into_iter().filter(|row| row.org == org && row.project == project).collect())
}

/// One persisted row, without its body: what a caller needs to decide
/// whether that row's worker can still start.
pub struct WorkerRowIndex {
    pub org: String,
    pub project: String,
    pub label: String,
    pub session_id: Option<String>,
    pub is_git_repo: Option<bool>,
}

/// The two body fields [`WorkerRowIndex`] reads. Deserialising into this
/// rather than [`SessionRecord`] is the point: serde skips what it is not
/// asked for, so the charter - kilobytes on a project's lead row - is
/// never allocated.
#[derive(Deserialize)]
struct RowStart {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    is_git_repo: Option<bool>,
}

/// Every row's identity plus the fields that say whether its worker can
/// still start, in key order.
///
/// A caller needs this rather than `(org, project, label)` alone because
/// the launchpad decides a row is still real from its recorded gitness
/// and its stored id, and it needs [`list_all`] rather than this only if
/// it also wants the charter.
pub fn worker_row_index(db: &Db) -> anyhow::Result<Vec<WorkerRowIndex>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(SESSIONS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        let (org, project, label) = key.value();
        // A value that will not decode leaves both fields absent, which
        // reads exactly like a row written before the gitness field
        // existed: offerable, and the spawn repairs it.
        let start = serde_json::from_slice::<RowStart>(value.value())
            .unwrap_or(RowStart { session_id: None, is_git_repo: None });
        out.push(WorkerRowIndex {
            org: org.to_owned(),
            project: project.to_owned(),
            label: label.to_owned(),
            // An empty string is absence, not an id - see `decode`.
            session_id: start.session_id.filter(|id| !id.is_empty()),
            is_git_repo: start.is_git_repo,
        });
    }
    Ok(out)
}

/// Every session row, in key order.
pub fn list_all(db: &Db) -> anyhow::Result<Vec<SessionRecord>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(SESSIONS) {
        Ok(t) => t,
        // A fresh database has no table until the first write; an absent
        // table is an empty store, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        match decode(value.value()) {
            Ok(row) => out.push(row),
            // One undecodable row (schema drift, a corrupt blob) must not
            // hide the rest. The key stays readable when the value does
            // not, so name which session lost its row.
            Err(error) => {
                let (row_org, row_project, row_label) = key.value();
                tracing::warn!(
                    target: "forge_workspace::store::sessions",
                    org = %row_org,
                    project = %row_project,
                    label = %row_label,
                    %error,
                    "skipping session row that failed to decode",
                );
            }
        }
    }
    Ok(out)
}

/// Merge `fields` onto the row at `(org, project, label)`, leaving an
/// absent field at its stored value. Returns whether a row existed; this
/// never creates one, because the row is what makes a worker re-spawn on
/// the next lead connect.
pub fn update(db: &Db, fields: &SessionRecord) -> anyhow::Result<bool> {
    let Some(mut row) = get(db, &fields.org, &fields.project, &fields.label)? else {
        return Ok(false);
    };
    // `Option::clone_from` overwrites, so each guard is what keeps an
    // absent field at its stored value.
    if fields.session_id.is_some() {
        row.session_id.clone_from(&fields.session_id);
    }
    if fields.charter.is_some() {
        row.charter.clone_from(&fields.charter);
    }
    if fields.kick.is_some() {
        row.kick.clone_from(&fields.kick);
    }
    if fields.resume_kick.is_some() {
        row.resume_kick.clone_from(&fields.resume_kick);
    }
    if fields.interactive.is_some() {
        row.interactive = fields.interactive;
    }
    if fields.is_git_repo.is_some() {
        row.is_git_repo = fields.is_git_repo;
    }
    put(db, &row)?;
    Ok(true)
}

/// Delete the row at `(org, project, label)`. Returns whether one
/// existed. This is what stops a despawned worker coming back: the row is
/// the only thing a boot re-spawns from.
pub fn delete(db: &Db, org: &str, project: &str, label: &str) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let existed = {
        let mut table = txn.open_table(SESSIONS)?;
        table.remove((org, project, label))?.is_some()
    };
    txn.commit()?;
    Ok(existed)
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
            is_git_repo: None,
        }
    }

    /// A record carrying only the fields an `update` supplies; the rest
    /// stay absent, which is what makes them keep their stored value.
    fn record_fields(
        org: &str,
        project: &str,
        label: &str,
        session_id: Option<&str>,
        kick: Option<&str>,
    ) -> SessionRecord {
        SessionRecord { kick: kick.map(str::to_owned), ..record(org, project, label, session_id) }
    }

    /// The first boot after this table exists is the migration: every
    /// persisted worker crosses over with its fields, and its id is left
    /// empty because `dynamic_workers` never stored one - so the session
    /// it names starts fresh rather than resuming.
    #[test]
    fn the_first_open_copies_the_persisted_workers_over_once() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        dynamic_workers::insert_for_test(&db, &worker("proj-a", "steward", true))
            .expect("seed steward");
        dynamic_workers::insert_for_test(&db, &worker("proj-a", "quartermaster", false))
            .expect("seed quartermaster");

        let projects = [project("proj-a", "Personal", "forge")];
        let outcome = migrate_from_dynamic_workers(&db, &projects).expect("migrate");
        assert_eq!(outcome.moved, 2, "both persisted workers cross over");
        assert!(outcome.drained(), "and nothing is left in the retired table");

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
                // The retired table never held it; the spawn probes and
                // records it on the worker's next spawn.
                is_git_repo: None,
            },
            "the row carries the worker's fields and no id",
        );

        let second = migrate_from_dynamic_workers(&db, &projects).expect("second boot");
        assert_eq!(second.moved, 0, "a second boot copies nothing");
        assert!(
            second.drained(),
            "and it is a clean drain: the first sweep took every row with it, so the table is \
             empty and the caller may drop it",
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
        dynamic_workers::insert_for_test(&db, &worker("proj-gone", "steward", false))
            .expect("seed");

        let outcome = migrate_from_dynamic_workers(&db, &[project("proj-a", "Personal", "forge")])
            .expect("migrate");
        assert_eq!(outcome.moved, 0, "a worker with no configured project is not copied");
        assert_eq!(outcome.unkeyable, 1, "and it is counted as left behind rather than dropped");
        assert!(
            !outcome.drained(),
            "so the retired table is not safe to drop: its row exists nowhere else",
        );
        assert!(get(&db, "Personal", "forge", "steward").expect("read").is_none());
        assert_eq!(
            dynamic_workers::list_all(&db).expect("list workers").len(),
            1,
            "its row is still where it was",
        );
    }

    /// A row written before this table existed has a NULL id, and a row
    /// a spawn has since filled carries one: both decode, so a store
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

    /// The three additions the worker registry needs: a project's rows
    /// list, a field merge that leaves an absent field alone, and a
    /// delete that reports whether it removed anything.
    #[test]
    fn list_update_and_delete_are_scoped_to_the_row() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let mut steward = record("Personal", "forge", "steward", Some("id-1"));
        steward.charter = Some("charter".to_owned());
        put(&db, &steward).expect("put steward");
        put(&db, &record("Personal", "forge", "lead", Some("id-2"))).expect("put lead");
        put(&db, &record("Personal", "other", "steward", None)).expect("put elsewhere");

        assert_eq!(
            list_for_project(&db, "Personal", "forge").expect("list").len(),
            2,
            "a project's rows are scoped by org and project",
        );
        assert_eq!(list_all(&db).expect("list all").len(), 3, "and the whole table lists");

        assert!(
            update(&db, &record_fields("Personal", "forge", "steward", None, Some("new kick")))
                .expect("update"),
            "an existing row updates",
        );
        let updated = get(&db, "Personal", "forge", "steward").expect("get").expect("row");
        assert_eq!(updated.kick.as_deref(), Some("new kick"), "the supplied field lands");
        assert_eq!(
            updated.session_id.as_deref(),
            Some("id-1"),
            "and an absent field keeps its stored value",
        );
        assert_eq!(updated.charter.as_deref(), Some("charter"), "including the charter");

        assert!(
            !update(&db, &record_fields("Personal", "forge", "ghost", None, Some("k")))
                .expect("update ghost"),
            "an absent row is not created",
        );
        assert!(get(&db, "Personal", "forge", "ghost").expect("get").is_none());

        assert!(delete(&db, "Personal", "forge", "steward").expect("delete"));
        assert!(get(&db, "Personal", "forge", "steward").expect("get").is_none());
        assert!(
            !delete(&db, "Personal", "forge", "steward").expect("delete again"),
            "a second delete reports nothing removed",
        );
        assert!(
            get(&db, "Personal", "other", "steward").expect("get").is_some(),
            "deleting one project's row leaves another's alone",
        );
    }

    /// A row whose value will not decode is skipped, not fatal: the key
    /// alone stays readable, so `list_all` reports the rest.
    #[test]
    fn list_all_skips_an_undecodable_row() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        put(&db, &record("Personal", "forge", "lead", Some("id-1"))).expect("put");
        put_raw_for_test(&db, "Personal", "forge", "corrupt", b"not a record")
            .expect("plant the undecodable row");

        let rows = list_all(&db).expect("list tolerates the corrupt blob");
        assert_eq!(rows.len(), 1, "the good row survives a corrupt sibling");
        assert_eq!(rows[0].label, "lead");
    }

    /// An entry whose value will not decode cannot be moved, so the sweep
    /// must not report a clean drain while one is there - the drop that
    /// follows would destroy it, and it exists nowhere else. The row count
    /// therefore comes from the table, not from the rows that decoded.
    #[test]
    fn a_table_holding_an_undecodable_entry_is_never_reported_as_drained() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        dynamic_workers::insert_for_test(&db, &worker("proj-a", "steward", false))
            .expect("seed steward");
        dynamic_workers::put_raw_for_test(&db, "proj-a", "unreadable", b"not a worker")
            .expect("plant a retired row that will not decode");

        let projects = [project("proj-a", "Personal", "forge")];
        let outcome = migrate_from_dynamic_workers(&db, &projects).expect("migrate");
        assert_eq!(outcome.moved, 1, "the readable row crosses over");
        assert_eq!(outcome.rows, 2, "and the count is what the table holds, not what decoded");
        assert!(
            !outcome.drained(),
            "so the caller must not drop a table still holding an entry nothing else has",
        );
    }

    /// `worker_row_index` takes the identity off the composite key, so a
    /// row whose body will not decode still reports its label and stays
    /// offerable. That is what the launchpad's worker rows are built from,
    /// once per frame: read through `list_all` instead and an undecodable
    /// body takes its label off the screen, and every row's charter is
    /// materialised on the render path.
    #[test]
    fn worker_row_index_reports_labels_without_the_row_body() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        put(&db, &record("Personal", "forge", "lead", Some("id-1"))).expect("put");
        let mut listed = record("Personal", "forge", "worker", Some("id-2"));
        listed.is_git_repo = Some(true);
        put(&db, &listed).expect("put");
        put_raw_for_test(&db, "Personal", "forge", "corrupt", b"not a record")
            .expect("plant the undecodable row");

        let rows = worker_row_index(&db).expect("the index reads the key and two fields");
        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["corrupt", "lead", "worker"],
            "every label survives, including the one whose body does not decode",
        );
        let worker = rows.iter().find(|row| row.label == "worker").expect("the worker row");
        assert_eq!(worker.session_id.as_deref(), Some("id-2"), "the stored id is projected");
        assert_eq!(
            worker.is_git_repo,
            Some(true),
            "and so is the gitness the launchpad decides off",
        );
        let corrupt = rows.iter().find(|row| row.label == "corrupt").expect("the corrupt row");
        assert_eq!(
            (corrupt.session_id.as_deref(), corrupt.is_git_repo),
            (None, None),
            "an unreadable body reads as unknown, which the launchpad offers",
        );
    }

    /// An empty id is absence, not an id: a blank is not an answer a
    /// spawn can resume onto.
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
