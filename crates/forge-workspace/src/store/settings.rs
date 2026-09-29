//! The retired `settings` table, kept only to drop it.
//!
//! Its one row was the `/spinner` override, which went when the spinner moved
//! to the terminal - nothing reads or writes the table now, so there is
//! nothing to drain before [`drop_table`] removes it.
//!
//! The same shape as [`crate::store::model_catalog`], whose test pins the
//! absent-table behaviour this relies on rather than pinning it twice.

use redb::TableDefinition;

use super::Db;

const SETTINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("settings");

/// Remove the table. Returns whether a table was there to drop. Callers
/// compact afterwards, because dropping a table reclaims nothing on its own.
pub fn drop_table(db: &Db) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    // `false` for a store that never held an override, which is not an error:
    // the boot drops cleanly either way.
    let dropped = txn.delete_table(SETTINGS)?;
    txn.commit()?;
    Ok(dropped)
}
