//! Durable chase log: one row per chase message that fired.
//!
//! This is what makes "each rung fires once per row per threshold" a fact
//! rather than a hope: the sweep checks here before it speaks, and the
//! retro reads here for how often a row was asked about.

use anyhow::Context;
use forge_primitives::tasks::TaskChase;
use redb::{ReadableTable, TableDefinition};

use super::Db;

const TASK_CHASES: TableDefinition<&str, &[u8]> = TableDefinition::new("task_chases");

/// The row key for one fired chase: project, the row it was about (a
/// seat-level rung carries none), the rung's own subject (two workers
/// named in one sweep share an instant), the rung, and the instant.
fn row_key(chase: &TaskChase) -> String {
    let nanos =
        chase.at.duration_since(std::time::SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let task = chase.task_id.as_ref().map_or("-", forge_primitives::tasks::TaskId::as_str);
    let subject = chase.subject.as_deref().unwrap_or("-");
    format!("{}\u{0}{task}\u{0}{subject}\u{0}{:?}\u{0}{nanos:040}", chase.project_name, chase.rung)
}

/// Append `chases`; never rewrites what is there.
pub fn append(db: &Db, chases: &[TaskChase]) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(TASK_CHASES)?;
        for chase in chases {
            let value = serde_json::to_vec(chase).context("serialize chase")?;
            table.insert(row_key(chase).as_str(), value.as_slice())?;
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
    for entry in table.iter()? {
        let (key, value) = entry?;
        if !key.value().starts_with(&prefix) {
            continue;
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
