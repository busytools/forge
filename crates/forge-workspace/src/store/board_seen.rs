//! When the person last looked at a project's board.
//!
//! **Server-side, so every client agrees**: the window's "since your last
//! look" strip and the empty state's delta are claims about the person's
//! own history, and a page-local stamp would be a different history per
//! device. The stamp moves when the window opens (or the person answers an
//! ask), never on a background read.

use std::time::SystemTime;

use redb::TableDefinition;

use super::Db;

const BOARD_SEEN: TableDefinition<&str, u64> = TableDefinition::new("board_seen");

/// When `project`'s board was last looked at, if it ever was.
pub fn last_seen(db: &Db, project: &str) -> anyhow::Result<Option<SystemTime>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(BOARD_SEEN) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(seconds) = table.get(project)? else { return Ok(None) };
    Ok(Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds.value())))
}

/// Stamp `project`'s board as looked at.
pub fn record(db: &Db, project: &str, at: SystemTime) -> anyhow::Result<()> {
    let seconds = at.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(BOARD_SEEN)?;
        table.insert(project, seconds)?;
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stamp_is_per_project_and_survives_a_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        {
            let db = Db::open(&dir.path().join("db.redb")).expect("open");
            assert_eq!(last_seen(&db, "proj").expect("read"), None, "never looked at");
            record(&db, "proj", at).expect("record");
        }
        let reopened = Db::open(&dir.path().join("db.redb")).expect("reopen");
        assert_eq!(last_seen(&reopened, "proj").expect("read"), Some(at));
        assert_eq!(last_seen(&reopened, "other").expect("read"), None, "scoped by project");
    }
}
