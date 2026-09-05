//! Per-file worker-tag scan cache on the redb `session_tags` table.
//!
//! Keyed by session-file path; the value is how far that transcript has
//! been scanned and the tag found up to there. A tag row can sit
//! anywhere in a transcript and the last one wins, so the catalog scan
//! otherwise reads every byte of every session on the boot path.
//!
//! Read and written in bulk, once per catalog scan: 600-odd transcripts
//! against a transaction each would cost more than the reading it saves.

use anyhow::Context;
use forge_agent::userdata::catalog::scan::SessionTagScan;
use redb::{ReadableTable, TableDefinition};
use std::collections::HashMap;

use super::Db;

const SESSION_TAGS: TableDefinition<&str, &[u8]> = TableDefinition::new("session_tags");

/// Every cached scan. A fresh database has no table until the first
/// write, which is an empty map rather than an error; one undecodable
/// row is skipped with a warn, costing that file a full re-scan rather
/// than discarding the rest of the cache.
pub fn load_all(db: &Db) -> anyhow::Result<HashMap<String, SessionTagScan>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(SESSION_TAGS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = HashMap::new();
    for entry in table.iter()? {
        let (path, value) = entry?;
        match serde_json::from_slice::<SessionTagScan>(value.value()) {
            Ok(scan) => {
                out.insert(path.value().to_owned(), scan);
            }
            Err(error) => tracing::warn!(
                target: "forge_workspace::store::session_tags",
                %error,
                path = %path.value(),
                "undecodable tag-scan row; that transcript will be re-scanned in full",
            ),
        }
    }
    Ok(out)
}

/// Persist the scans that moved, in one transaction.
pub fn store_all(db: &Db, scans: &[(String, SessionTagScan)]) -> anyhow::Result<()> {
    if scans.is_empty() {
        return Ok(());
    }
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(SESSION_TAGS)?;
        for (path, scan) in scans {
            let value = serde_json::to_vec(scan)
                .with_context(|| format!("serialize tag scan for {path}"))?;
            table.insert(path.as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// Drop cached rows whose transcript no longer exists. The table only
/// grows otherwise: a deleted session's row survives every scan, and
/// the load cost at boot is paid per row forever. A row whose path
/// cannot be stat'd is dropped - a missing file is stale by
/// definition, and an unreadable one only costs a re-scan.
pub fn prune_missing(db: &Db) -> anyhow::Result<usize> {
    let paths: Vec<String> =
        load_all(db)?.into_keys().filter(|path| std::fs::metadata(path).is_err()).collect();
    if paths.is_empty() {
        return Ok(0);
    }
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(SESSION_TAGS)?;
        for path in &paths {
            table.remove(path.as_str())?;
        }
    }
    txn.commit()?;
    Ok(paths.len())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn scan(tag: Option<&str>, scanned_len: u64) -> SessionTagScan {
        SessionTagScan { tag: tag.map(ToOwned::to_owned), scanned_len }
    }

    #[test]
    fn a_stored_scan_round_trips() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("db.redb")).unwrap();

        assert!(load_all(&db).unwrap().is_empty(), "a fresh database caches nothing");

        store_all(&db, &[("/p/a.jsonl".to_owned(), scan(Some("forge:worker:x"), 4096))]).unwrap();

        let loaded = load_all(&db).unwrap();
        assert_eq!(
            loaded.get("/p/a.jsonl"),
            Some(&scan(Some("forge:worker:x"), 4096)),
            "both halves must survive: a tag with no offset re-scans, an offset with no tag lies"
        );
    }

    #[test]
    fn prune_drops_rows_of_missing_transcripts_only() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("db.redb")).unwrap();
        let live = dir.path().join("live.jsonl");
        std::fs::write(&live, "{}\n").unwrap();

        store_all(
            &db,
            &[
                (live.to_string_lossy().into_owned(), scan(None, 3)),
                ("/p/deleted.jsonl".to_owned(), scan(None, 4096)),
            ],
        )
        .unwrap();

        let pruned = prune_missing(&db).unwrap();
        assert_eq!(pruned, 1, "only the missing transcript's row goes");
        let loaded = load_all(&db).unwrap();
        assert_eq!(loaded.len(), 1, "the live transcript's row survives");
        assert!(loaded.contains_key(live.to_string_lossy().as_ref()));
    }
}
