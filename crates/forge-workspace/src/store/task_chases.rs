//! Durable chase log: the newest firing of each rung per subject.
//!
//! This is what makes "each rung fires once per row per threshold" a fact
//! rather than a hope: the sweep checks here before it speaks. **Only the
//! newest firing under a subject's key is kept**, because every reader asks
//! "did it fire at or after T?", which the newest row answers alone - and a
//! held state re-appends once per cadence window, so without that the table
//! would grow with time rather than with what the board is tracking (one
//! permanently-over row is 288 rows a day at the 5-minute cadence).

use anyhow::Context;
use forge_primitives::tasks::{ChaseRung, TaskChase, TaskId};
use redb::{ReadableTable, TableDefinition};

use super::Db;

const TASK_CHASES: TableDefinition<&str, &[u8]> = TableDefinition::new("task_chases");

/// The key prefix every firing of one (project, row, rung, subject) shares:
/// keys sort by it first, so its rows are one contiguous run and its newest
/// firing is its last.
fn bucket_key(
    project: &str,
    task_id: Option<&TaskId>,
    subject: Option<&str>,
    rung: ChaseRung,
) -> String {
    let task = task_id.map_or("-", TaskId::as_str);
    let subject = subject.unwrap_or("-");
    format!("{project}\u{0}{task}\u{0}{subject}\u{0}{rung:?}\u{0}")
}

/// The row key for one fired chase: its bucket, then the instant (padded so
/// a lexicographic order is the time order).
fn row_key(chase: &TaskChase) -> String {
    let nanos =
        chase.at.duration_since(std::time::SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let bucket = bucket_key(
        &chase.project_name,
        chase.task_id.as_ref(),
        chase.subject.as_deref(),
        chase.rung,
    );
    format!("{bucket}{nanos:040}")
}

/// Append `chases`, each replacing any older firing of its own subject.
///
/// The newest firing is the one every reader asks for, so the older ones are
/// dead weight the moment it exists - and a held state appends one every
/// cadence window, so keeping them is what would make this table grow with
/// time on the board rather than with the board's own rows.
pub fn append(db: &Db, chases: &[TaskChase]) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(TASK_CHASES)?;
        for chase in chases {
            let key = row_key(chase);
            let bucket = bucket_key(
                &chase.project_name,
                chase.task_id.as_ref(),
                chase.subject.as_deref(),
                chase.rung,
            );
            // Collected before anything is removed: the range borrows the
            // table, and a key cannot be removed under a live iterator.
            let mut stale = Vec::new();
            for entry in table.range(bucket.as_str()..key.as_str())? {
                let (held, _) = entry?;
                stale.push(held.value().to_owned());
            }
            for old in stale {
                table.remove(old.as_str())?;
            }
            let value = serde_json::to_vec(chase).context("serialize chase")?;
            table.insert(key.as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// Forget every firing of one subject: the rung's state cleared, so the
/// slate resets durably rather than in a process's memory alone.
pub fn forget(
    db: &Db,
    project: &str,
    rung: ChaseRung,
    task_id: Option<&TaskId>,
    subject: Option<&str>,
) -> anyhow::Result<()> {
    let bucket = bucket_key(project, task_id, subject, rung);
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(TASK_CHASES)?;
        let mut doomed = Vec::new();
        for entry in table.range(bucket.as_str()..)? {
            let (key, _) = entry?;
            if !key.value().starts_with(bucket.as_str()) {
                break;
            }
            doomed.push(key.value().to_owned());
        }
        for key in doomed {
            table.remove(key.as_str())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// `project`'s fired chases, ordered by row and then by time.
pub fn list_for_project(db: &Db, project: &str) -> anyhow::Result<Vec<TaskChase>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(TASK_CHASES) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let prefix = format!("{project}\u{0}");
    let mut out = Vec::new();
    // **One range, not the whole table.** Keys sort by project first, so a
    // project's rows are one contiguous run: the walk stops at its end
    // instead of reading every other project's rows once per project, per
    // sweep.
    for entry in table.range(prefix.as_str()..)? {
        let (key, value) = entry?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        match serde_json::from_slice::<TaskChase>(value.value()) {
            Ok(chase) => out.push(chase),
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::task_chases",
                row = %key.value(),
                error = %err,
                "skipping chase row that failed to decode",
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use forge_primitives::tasks::{ChaseRung, TaskId};
    use tempfile::tempdir;

    fn open_db(dir: &Path) -> Db {
        Db::open(&dir.join("db.redb")).expect("open db")
    }

    fn chase(task: Option<&str>, project: &str, rung: ChaseRung, at_secs: u64) -> TaskChase {
        TaskChase {
            task_id: task.map(TaskId::from),
            subject: None,
            project_name: project.to_owned(),
            rung,
            at: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(at_secs),
        }
    }

    /// A subject's older firings are dead weight once a newer one exists:
    /// a held state appends one every cadence window, and the newest alone
    /// answers a "fired since" question.
    #[test]
    fn only_the_newest_firing_of_a_subject_is_kept() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let mut older = chase(None, "forge", ChaseRung::Unaccounted, 10);
        older.subject = Some("w-1".to_owned());
        let mut newer = chase(None, "forge", ChaseRung::Unaccounted, 20);
        newer.subject = Some("w-1".to_owned());
        append(&db, &[older, newer]).expect("append");

        let forge = list_for_project(&db, "forge").expect("list");
        assert_eq!(forge.len(), 1, "one firing stands per subject: {forge:?}");
        assert_eq!(
            forge[0].at,
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20),
            "and it is the newest",
        );
    }

    /// A project whose name extends another's is still read apart: the walk
    /// is scoped by the whole name and its separator, not by a prefix of it.
    #[test]
    fn a_projects_rows_do_not_leak_into_an_extension_of_its_name() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        append(
            &db,
            &[
                chase(Some("t-1"), "forge", ChaseRung::EstimateNudge, 10),
                chase(Some("t-2"), "forge2", ChaseRung::EstimateNudge, 11),
            ],
        )
        .expect("append");

        let forge = list_for_project(&db, "forge").expect("list");
        assert_eq!(forge.len(), 1, "only forge's own row: {forge:?}");
        assert_eq!(forge[0].task_id.as_ref().map(TaskId::as_str), Some("t-1"));
    }

    /// Clearing a state forgets its firings, durably, and only its own: a
    /// recurrence after a restart reads no record and is named at once.
    #[test]
    fn forgetting_a_subject_drops_only_its_firings() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let mut cleared = chase(None, "forge", ChaseRung::Unaccounted, 10);
        cleared.subject = Some("w-1".to_owned());
        let mut held = chase(None, "forge", ChaseRung::Unaccounted, 10);
        held.subject = Some("w-2".to_owned());
        append(&db, &[cleared, held]).expect("append");

        forget(&db, "forge", ChaseRung::Unaccounted, None, Some("w-1")).expect("forget");
        let forge = list_for_project(&db, "forge").expect("list");
        assert_eq!(forge.len(), 1, "the cleared worker's firing is gone: {forge:?}");
        assert_eq!(forge[0].subject.as_deref(), Some("w-2"), "the held one stays");
    }

    #[test]
    fn the_chase_log_answers_whether_a_rung_already_fired() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        append(
            &db,
            &[
                chase(Some("t-1"), "forge", ChaseRung::EstimateNudge, 10),
                chase(None, "forge", ChaseRung::Unaccounted, 11),
                chase(Some("t-9"), "other", ChaseRung::EstimateNudge, 12),
            ],
        )
        .expect("append");

        let forge = list_for_project(&db, "forge").expect("list");
        assert_eq!(forge.len(), 2, "only forge's chases: {forge:?}");
        assert!(
            forge.iter().any(|c| {
                c.task_id.as_ref() == Some(&TaskId::from("t-1"))
                    && c.rung == ChaseRung::EstimateNudge
            }),
            "the fired rung is found for its row",
        );
        assert!(
            forge.iter().any(|c| c.task_id.is_none() && c.rung == ChaseRung::Unaccounted),
            "a seat-level rung records with no task",
        );
    }

    /// Two workers named by one sweep share an instant, so the label is
    /// part of the key: without it the second write would overwrite the
    /// first and one worker's naming would vanish from the log.
    #[test]
    fn two_seat_level_chases_at_one_instant_both_survive() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let mut w1 = chase(None, "forge", ChaseRung::Unaccounted, 10);
        w1.subject = Some("w-1".to_owned());
        let mut w2 = chase(None, "forge", ChaseRung::Unaccounted, 10);
        w2.subject = Some("w-2".to_owned());
        append(&db, &[w1, w2]).expect("append");

        let forge = list_for_project(&db, "forge").expect("list");
        assert_eq!(forge.len(), 2, "both namings stand: {forge:?}");
    }

    #[test]
    fn an_absent_chase_table_reads_as_empty() {
        let dir = tempdir().expect("tempdir");
        let db = open_db(dir.path());
        assert!(list_for_project(&db, "forge").expect("list").is_empty());
    }
}
