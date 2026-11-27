//! The acknowledgement the drive has already named as forgotten, per row.
//!
//! **Persisted, not held in memory**: the drive names an
//! acknowledged-but-unanswered ask to the lead once, and a restart must not
//! lose that record and re-name the same forgetting. The row itself keeps
//! `acknowledged_at`; this table keeps which acknowledgement the drive has
//! already spoken about, so a changed acknowledgement is a new one to name.

use std::time::SystemTime;

use redb::TableDefinition;

use super::Db;

const ACK_NAMED: TableDefinition<&str, u64> = TableDefinition::new("ack_named");

/// The row key: its project as well as its id, the pair tasks use.
fn row_key(project: &str, id: &str) -> String {
    format!("{project}\u{0}{id}")
}

/// The acknowledgement already named for this row, if one was.
pub fn named(db: &Db, project: &str, id: &str) -> anyhow::Result<Option<SystemTime>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(ACK_NAMED) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(seconds) = table.get(row_key(project, id).as_str())? else {
        return Ok(None);
    };
    Ok(Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds.value())))
}

/// Record that the drive has named this row's acknowledgement.
pub fn mark(db: &Db, project: &str, id: &str, acknowledged: SystemTime) -> anyhow::Result<()> {
    let seconds =
        acknowledged.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(ACK_NAMED)?;
        table.insert(row_key(project, id).as_str(), seconds)?;
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_named_acknowledgement_is_per_row_and_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        {
            let db = Db::open(&dir.path().join("db.redb")).expect("open");
            assert_eq!(named(&db, "proj", "t-1").expect("read"), None, "never named");
            mark(&db, "proj", "t-1", at).expect("mark");
        }
        let reopened = Db::open(&dir.path().join("db.redb")).expect("reopen");
        assert_eq!(named(&reopened, "proj", "t-1").expect("read"), Some(at));
        assert_eq!(named(&reopened, "proj", "t-2").expect("read"), None, "scoped by row");
        assert_eq!(named(&reopened, "other", "t-1").expect("read"), None, "scoped by project");
    }
}
