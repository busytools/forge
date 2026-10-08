//! What this machine's benches measured, keyed by what they were about.
//!
//! App-level rather than per-project: a bench runs one model on this
//! machine's own material, and the corpus is the machine's too. **The key
//! is `(role, file, tier, corpus sha)`** - a result for a different corpus
//! is not a comparison, and overwriting the row for the same corpus is
//! what keeps a re-run from stacking a second copy of the same number.

use redb::{ReadableTable, TableDefinition};

use crate::bench::BenchResult;

use super::Db;

/// One row per (role, file, tier, corpus), keyed by the four of them.
const RESULTS: TableDefinition<&str, &[u8]> = TableDefinition::new("bench_results");

/// The key one result is filed under.
pub fn key(result: &BenchResult) -> String {
    key_parts(result.target.role, &result.target.file, result.tier, &result.corpus.sha256)
}

/// The key's own spelling, for a caller that holds the parts rather than
/// the row - the page's delete names them and nothing else.
pub fn key_parts(
    role: crate::bench::BenchRole,
    file: &str,
    tier: forge_dictate::bench::Tier,
    corpus: &str,
) -> String {
    format!("{role:?}|{file}|{tier:?}|{corpus}")
}

/// Every saved result, newest first.
pub fn results(db: &Db) -> anyhow::Result<Vec<BenchResult>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(RESULTS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut rows = Vec::new();
    for entry in table.iter()? {
        let (_, value) = entry?;
        rows.push(serde_json::from_slice(value.value())?);
    }
    rows.sort_by(|a: &BenchResult, b: &BenchResult| b.at.cmp(&a.at));
    Ok(rows)
}

/// Drop one result by its key. A key with no row is not an error: the row
/// the page asked to delete is already gone.
pub fn remove(db: &Db, key: &str) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = match txn.open_table(RESULTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        table.remove(key)?;
    }
    txn.commit()?;
    Ok(())
}

/// Save one result, replacing any row for the same (role, file, tier,
/// corpus).
pub fn save(db: &Db, result: &BenchResult) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(RESULTS)?;
        table.insert(key(result).as_str(), serde_json::to_vec(result)?.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}
