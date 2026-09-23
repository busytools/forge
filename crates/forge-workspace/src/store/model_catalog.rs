//! The retired `model_catalog` table, kept only to drop it.
//!
//! `f5a8292c` deleted the OpenRouter catalog machinery and the store
//! module that owned this table, which left its rows behind in every
//! store that had cached a catalog. Nothing in the tree reads or writes
//! it now, so there is nothing to drain before [`drop_table`] removes
//! it.

use redb::TableDefinition;

use super::Db;

const MODEL_CATALOG: TableDefinition<&str, &[u8]> = TableDefinition::new("model_catalog");

/// Remove the table. Returns whether a table was there to drop.
/// Callers compact afterwards, because dropping a table reclaims
/// nothing on its own.
pub fn drop_table(db: &Db) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    // `false` for a store that never had the table, which is not an
    // error: the boot drops cleanly either way.
    let dropped = txn.delete_table(MODEL_CATALOG)?;
    txn.commit()?;
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Dropping reports whether the table was there, so a store that
    /// never cached a catalog does not read as a failed drop.
    #[test]
    fn drop_table_reports_whether_the_table_was_there() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        assert!(
            !drop_table(&db).expect("drop an absent table"),
            "a store that never cached a catalog drops nothing",
        );

        {
            let txn = db.database().begin_write().expect("begin");
            {
                let mut table = txn.open_table(MODEL_CATALOG).expect("open table");
                table
                    .insert("https://openrouter.ai/api/v1", "cached".as_bytes())
                    .expect("insert a cached catalog");
            }
            txn.commit().expect("commit");
        }

        assert!(drop_table(&db).expect("drop the table"), "a planted table drops to true");

        let txn = db.database().begin_read().expect("begin read");
        assert!(txn.open_table(MODEL_CATALOG).is_err(), "the table is gone rather than emptied");
    }
}
