//! What this machine has downloaded from the dictation feed, and which
//! model each role was last set to run.
//!
//! App-level rather than per-project: dictation is one process-wide engine,
//! so a model is on the machine or it is not.

use redb::{ReadableTable, TableDefinition};

use crate::dictate::DictateRole;
use crate::install::{ActiveChoice, InstalledModel};

use super::Db;

/// One row per installed file, keyed by file name.
const INSTALLED: TableDefinition<&str, &[u8]> = TableDefinition::new("dictate_installed");

/// One row per role, keyed by [`role_key`]: the user's last runtime pick.
const ACTIVE: TableDefinition<&str, &[u8]> = TableDefinition::new("dictate_active");

/// The store's own spelling of a role, stable across releases: the key a
/// pick is filed under.
pub fn role_key(role: DictateRole) -> &'static str {
    match role {
        DictateRole::Transcribing => "transcribing",
        DictateRole::Normalization => "normalization",
    }
}

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

/// The runtime pick recorded for one role, when there is one.
pub fn active(db: &Db, role: DictateRole) -> anyhow::Result<Option<ActiveChoice>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(ACTIVE) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(value) = table.get(role_key(role))? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(value.value())?))
}

/// Record the runtime pick for one role, replacing any row for it.
pub fn record_active(db: &Db, role: DictateRole, choice: &ActiveChoice) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(ACTIVE)?;
        table.insert(role_key(role), serde_json::to_vec(choice)?.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Drop one role's runtime pick, so the role answers its config key or its
/// compiled pin again.
pub fn clear_active(db: &Db, role: DictateRole) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(ACTIVE)?;
        table.remove(role_key(role))?;
    }
    txn.commit()?;
    Ok(())
}
