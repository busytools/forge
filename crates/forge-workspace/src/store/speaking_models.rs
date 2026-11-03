//! What this machine has installed for the speaking role, keyed by variant.

use redb::{ReadableTable, TableDefinition};

use crate::tts::SpeakingInstall;

use super::Db;

/// One row per installed variant, keyed by the variant's name.
const INSTALLED: TableDefinition<&str, &[u8]> = TableDefinition::new("speaking_installed");

/// Every speaking install, oldest first.
pub fn installed(db: &Db) -> anyhow::Result<Vec<SpeakingInstall>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(INSTALLED) {
        Ok(table) => table,
        // A store that never installed a speaking model has no table, which
        // is not an error: the boot reads cleanly either way.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut rows = Vec::new();
    for entry in table.iter()? {
        let (_, value) = entry?;
        rows.push(serde_json::from_slice(value.value())?);
    }
    rows.sort_by(|a: &SpeakingInstall, b: &SpeakingInstall| a.at.cmp(&b.at));
    Ok(rows)
}

/// Record one installed variant, replacing any row for the same variant.
pub fn record_installed(db: &Db, install: &SpeakingInstall) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(INSTALLED)?;
        table.insert(install.variant.as_str(), serde_json::to_vec(install)?.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}
