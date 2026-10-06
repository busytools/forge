//! What this machine has downloaded from the dictation feed.
//!
//! App-level rather than per-project: dictation is one process-wide engine,
//! so a model is on the machine or it is not. The role it then runs is the
//! next table, when activation lands.

use redb::{ReadableTable, TableDefinition};

use crate::install::InstalledModel;

use super::Db;

/// One row per installed file, keyed by file name.
const INSTALLED: TableDefinition<&str, &[u8]> = TableDefinition::new("dictate_installed");

/// Every installed model, oldest first.
pub fn installed(db: &Db) -> anyhow::Result<Vec<InstalledModel>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(INSTALLED) {
        Ok(table) => table,
        // A store that never installed anything has no table, which is not
        // an error: the boot reads cleanly either way.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut rows = Vec::new();
    for entry in table.iter()? {
        let (_, value) = entry?;
        rows.push(serde_json::from_slice(value.value())?);
    }
    rows.sort_by(|a: &InstalledModel, b: &InstalledModel| a.at.cmp(&b.at));
    Ok(rows)
}

/// Record one installed model, replacing any row for the same file.
pub fn record_installed(db: &Db, model: &InstalledModel) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(INSTALLED)?;
        table.insert(model.file.as_str(), serde_json::to_vec(model)?.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}
