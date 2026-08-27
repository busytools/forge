//! Per-file usage-summary cache on the redb `token_usage` table.
//!
//! Keyed by canonical session-file path; the value is that file's
//! deduped `FileUsageSummary`. The cache is incremental: `/usage`
//! reuses a file whose mtime and size still match the cached entry
//! rather than re-parsing a transcript that can reach 100 MB.

use anyhow::Context;
use forge_agent::env::token_usage::FileUsageSummary;
use redb::TableDefinition;

use super::Db;

const TOKEN_USAGE: TableDefinition<&str, &[u8]> = TableDefinition::new("token_usage");

/// The cached summary for `path`, or `None` when absent (or the table
/// doesn't exist yet). A decode failure is surfaced so a corrupt row is
/// diagnosable rather than silently treated as a cache miss.
pub fn load(db: &Db, path: &str) -> anyhow::Result<Option<FileUsageSummary>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(TOKEN_USAGE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match table.get(path)? {
        Some(value) => Ok(Some(decode(value.value(), path)?)),
        None => Ok(None),
    }
}

/// Store `summary` for `path`, overwriting any prior entry.
pub fn store(db: &Db, path: &str, summary: &FileUsageSummary) -> anyhow::Result<()> {
    let value = serde_json::to_vec(summary).context("serialize usage summary")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(TOKEN_USAGE)?;
        table.insert(path, value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Decode a stored blob, tagging a failure with the owning path.
fn decode(bytes: &[u8], path: &str) -> anyhow::Result<FileUsageSummary> {
    serde_json::from_slice(bytes).with_context(|| format!("decode usage summary for {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_agent::env::token_usage::TokenCounts;
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn summary(project: &str, output: u64) -> FileUsageSummary {
        let mut by_model_day = BTreeMap::new();
        let mut days = BTreeMap::new();
        days.insert("2026-07-08".to_owned(), TokenCounts { output, ..TokenCounts::default() });
        by_model_day.insert("m".to_owned(), days);
        FileUsageSummary {
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            size: 42,
            folded_project: project.to_owned(),
            project_resolved: true,
            by_model_day,
        }
    }

    fn open_db() -> (tempfile::TempDir, Db) {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        (dir, db)
    }

    #[test]
    fn load_is_none_on_miss() {
        let (_dir, db) = open_db();
        assert!(load(&db, "/nope.jsonl").expect("load").is_none());
    }

    #[test]
    fn store_then_load_round_trips() {
        let (_dir, db) = open_db();
        let entry = summary("forge", 7);
        store(&db, "/a.jsonl", &entry).expect("store");
        assert_eq!(load(&db, "/a.jsonl").expect("load"), Some(entry));
    }

    #[test]
    fn a_row_predating_project_resolved_decodes_as_unresolved() {
        let (_dir, db) = open_db();
        // A row exactly as the previous version wrote it: the same blob
        // minus the field that did not exist yet.
        let mut old = serde_json::to_value(summary("auto", 7)).expect("serialize");
        old.as_object_mut()
            .expect("a summary serializes to an object")
            .remove("project_resolved")
            .expect("project_resolved is the field being dropped");
        let bytes = serde_json::to_vec(&old).expect("serialize old shape");
        let txn = db.database().begin_write().expect("begin write");
        {
            let mut table = txn.open_table(TOKEN_USAGE).expect("open table");
            table.insert("/a.jsonl", bytes.as_slice()).expect("insert");
        }
        txn.commit().expect("commit");

        let loaded = load(&db, "/a.jsonl").expect("load").expect("an old row still decodes");
        assert_eq!(loaded.by_model_day["m"]["2026-07-08"].output, 7, "its counts survive");
        assert!(
            !loaded.project_resolved,
            "an old row reads as unresolved, so its label is re-derived rather than frozen",
        );
    }

    #[test]
    fn store_overwrites_prior_entry() {
        let (_dir, db) = open_db();
        store(&db, "/a.jsonl", &summary("forge", 1)).expect("first");
        store(&db, "/a.jsonl", &summary("forge", 9)).expect("second");
        let loaded = load(&db, "/a.jsonl").expect("load").expect("present");
        assert_eq!(loaded.by_model_day["m"]["2026-07-08"].output, 9);
    }
}
