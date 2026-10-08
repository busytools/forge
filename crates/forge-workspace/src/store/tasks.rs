//! Durable task persistence on the redb `tasks` table.
//!
//! The whole [`Task`] record is stored as serde-json keyed by its
//! project and id - no field schema on disk, so a type change needs no
//! table migration. Machine-local, like the crons beside it.

use anyhow::Context;
use forge_primitives::tasks::{Estimate, LinkKind, Task, TaskLink, TaskStatus, Waiting};
use redb::{ReadableTable, TableDefinition};

use super::Db;

const TASKS: TableDefinition<&str, &[u8]> = TableDefinition::new("tasks");

/// The stored shape: [`Task`] plus the two spellings earlier versions
/// wrote - `artifact` as a bare string, and `blocked` as a status. Reads
/// go through here so a legacy row loads as the record it meant rather
/// than failing the whole set.
#[derive(serde::Deserialize)]
struct TaskRecord {
    id: forge_primitives::tasks::TaskId,
    project_name: String,
    subject: String,
    #[serde(default)]
    active_form: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    status: RecordStatus,
    #[serde(default)]
    owner: Option<forge_primitives::SessionSlot>,
    #[serde(default)]
    parent: Option<forge_primitives::tasks::TaskId>,
    #[serde(default)]
    waiting_on: Option<Waiting>,
    #[serde(default)]
    estimate: Option<RecordEstimate>,
    #[serde(default)]
    rank: Option<i64>,
    #[serde(default)]
    verify: Option<forge_primitives::tasks::Verify>,
    #[serde(default)]
    links: Vec<TaskLink>,
    #[serde(default)]
    attempt: u32,
    #[serde(default)]
    archived_at: Option<std::time::SystemTime>,
    created_at: std::time::SystemTime,
    updated_at: std::time::SystemTime,
    #[serde(default)]
    artifact: Option<String>,
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordStatus {
    Pending,
    InProgress,
    Waiting,
    Completed,
    Failed,
    Canceled,
    /// The old spelling: a wait whose reason was never stated.
    Blocked,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum RecordEstimate {
    /// The old shape: free words, parsed on read.
    Words(String),
    Parsed(Estimate),
}

impl TaskRecord {
    fn into_task(self) -> Task {
        let blocked = matches!(self.status, RecordStatus::Blocked);
        let status = match self.status {
            RecordStatus::Pending => TaskStatus::Pending,
            RecordStatus::InProgress => TaskStatus::InProgress,
            RecordStatus::Waiting | RecordStatus::Blocked => TaskStatus::Waiting,
            RecordStatus::Completed => TaskStatus::Completed,
            RecordStatus::Failed => TaskStatus::Failed,
            RecordStatus::Canceled => TaskStatus::Canceled,
        };
        let mut links = self.links;
        if let Some(artifact) = &self.artifact
            && !links.iter().any(|l| l.target == *artifact)
        {
            links.push(TaskLink {
                kind: LinkKind::for_target(artifact),
                label: None,
                target: artifact.clone(),
                state: None,
                added_at: self.created_at,
            });
        }
        Task {
            id: self.id,
            project_name: self.project_name,
            subject: self.subject,
            active_form: self.active_form,
            detail: self.detail,
            status,
            owner: self.owner,
            parent: self.parent,
            waiting_on: if blocked {
                // The migration's unstated wait: the board draws it as a
                // miss, which is what an old blocked row was.
                Some(Waiting { kind: None, detail: None, on: None, verification: false })
            } else {
                self.waiting_on
            },
            estimate: self.estimate.map(|estimate| match estimate {
                // Words the grammar cannot read keep their text and carry
                // no seconds, which the board reads as no estimate rather
                // than inventing one.
                RecordEstimate::Words(words) => {
                    Estimate::parse(&words).unwrap_or(Estimate { secs: 0, words })
                }
                RecordEstimate::Parsed(parsed) => parsed,
            }),
            rank: self.rank,
            verify: self.verify,
            links,
            attempt: self.attempt,
            archived_at: self.archived_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

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
        match serde_json::from_slice::<TaskRecord>(value.value()) {
            Ok(record) => out.push(record.into_task()),
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
            waiting_on: None,
            estimate: None,
            rank: None,
            verify: None,
            links: Vec::new(),
            attempt: 0,
            archived_at: None,
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

    /// A second write replaces the set rather than merging into it. The
    /// row the new set omits is the row a delete or a cascade just removed,
    /// and leaving it in redb brings that task back on the next boot.
    #[test]
    fn a_second_write_drops_the_rows_it_omits() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        replace_all(&db, &[sample_task("t-1", "forge"), sample_task("t-2", "forge")])
            .expect("seed");
        replace_all(&db, &[sample_task("t-2", "forge")]).expect("replace");
        let read = list(&db).expect("list");
        assert_eq!(read.len(), 1, "the omitted task is gone from the store, not just from memory");
        assert_eq!(read[0].id, TaskId::from("t-2"), "the surviving task is the one written");
    }

    #[test]
    fn an_absent_table_reads_as_empty_not_an_error() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        assert!(list(&db).expect("list").is_empty(), "a fresh database has no tasks");
    }

    /// Write a raw JSON row, the way an older forge wrote it.
    fn write_raw(db: &Db, key: &str, json: &str) {
        let txn = db.database().begin_write().expect("write txn");
        {
            let mut table = txn.open_table(TASKS).expect("open");
            table.insert(key, json.as_bytes()).expect("insert");
        }
        txn.commit().expect("commit");
    }

    /// A row an earlier forge wrote: `artifact` as a bare string, the
    /// estimate as free words.
    #[test]
    fn a_legacy_artifact_row_reads_as_a_link() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        write_raw(
            &db,
            "forge\u{0}t-1",
            r#"{
                "id": "t-1", "project_name": "forge", "subject": "old row",
                "active_form": null, "detail": null, "status": "in_progress",
                "owner": null, "parent": null,
                "artifact": "https://github.com/busytools/forge/pull/1889",
                "estimate": "1d",
                "created_at": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
                "updated_at": {"secs_since_epoch": 0, "nanos_since_epoch": 0}
            }"#,
        );
        let read = list(&db).expect("list");
        assert_eq!(read.len(), 1, "the legacy row loads: {read:?}");
        let task = &read[0];
        assert_eq!(task.links.len(), 1, "the artifact folded into a link");
        assert_eq!(task.links[0].kind, LinkKind::Pr, "classified by its shape");
        assert_eq!(task.links[0].target, "https://github.com/busytools/forge/pull/1889",);
        assert_eq!(
            task.estimate.as_ref().map(|e| (e.words.as_str(), e.secs)),
            Some(("1d", 86_400)),
            "the legacy estimate parsed with its words kept",
        );
    }

    /// The old `blocked` spelling is a wait whose reason was never stated:
    /// the board draws it as a miss rather than a blank.
    #[test]
    fn a_legacy_blocked_row_reads_as_an_unstated_wait() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        write_raw(
            &db,
            "forge\u{0}t-2",
            r#"{
                "id": "t-2", "project_name": "forge", "subject": "stuck row",
                "active_form": null, "detail": null, "status": "blocked",
                "owner": null, "parent": null,
                "created_at": {"secs_since_epoch": 0, "nanos_since_epoch": 0},
                "updated_at": {"secs_since_epoch": 0, "nanos_since_epoch": 0}
            }"#,
        );
        let read = list(&db).expect("list");
        assert_eq!(read.len(), 1, "the blocked row loads: {read:?}");
        assert_eq!(read[0].status, TaskStatus::Waiting);
        assert_eq!(
            read[0].waiting_on,
            Some(Waiting { kind: None, detail: None, on: None, verification: false }),
            "no kind, no words - the miss the board shows",
        );
    }

    /// A fresh row round-trips through the record shape unchanged.
    #[test]
    fn a_current_row_round_trips_through_the_store() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let mut task = sample_task("t-3", "forge");
        task.estimate = Some(Estimate { words: "2h".to_owned(), secs: 7_200 });
        task.links.push(TaskLink {
            kind: LinkKind::Issue,
            label: Some("#1886".to_owned()),
            target: "https://example.invalid/issues/1886".to_owned(),
            state: Some("open".to_owned()),
            added_at: std::time::SystemTime::UNIX_EPOCH,
        });
        replace_all(&db, std::slice::from_ref(&task)).expect("write");
        assert_eq!(list(&db).expect("list"), vec![task], "no migration touches a current row");
    }
}
