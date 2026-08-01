//! Cached LiteLLM pricing on the redb `pricing_cache` table.
//!
//! One row holds the most-recent fetch: the raw json plus its
//! `fetched_at`. `/usage` prices from this cache (empty until the first
//! background fetch lands); the fetch refreshes it about once a day.

use std::time::SystemTime;

use anyhow::Context;
use redb::TableDefinition;
use serde::{Deserialize, Serialize};

use super::Db;

const PRICING_CACHE: TableDefinition<&str, &[u8]> = TableDefinition::new("pricing_cache");

/// The single row key; there is only ever one cached pricing snapshot.
const KEY: &str = "litellm";

/// A fetched LiteLLM pricing file plus the instant it landed.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct CachedPricing {
    pub fetched_at: SystemTime,
    pub json: String,
}

/// The cached pricing snapshot, or `None` when nothing has been fetched
/// yet (or the row fails to decode).
pub fn load(db: &Db) -> anyhow::Result<Option<CachedPricing>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(PRICING_CACHE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match table.get(KEY)? {
        Some(value) => {
            Ok(Some(serde_json::from_slice(value.value()).context("decode cached pricing")?))
        }
        None => Ok(None),
    }
}

/// Overwrite the cached pricing snapshot with `entry`.
pub fn store(db: &Db, entry: &CachedPricing) -> anyhow::Result<()> {
    let value = serde_json::to_vec(entry).context("serialize cached pricing")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(PRICING_CACHE)?;
        table.insert(KEY, value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    fn open_db() -> (tempfile::TempDir, Db) {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        (dir, db)
    }

    #[test]
    fn load_is_none_before_any_fetch() {
        let (_dir, db) = open_db();
        assert!(load(&db).expect("load").is_none());
    }

    #[test]
    fn store_then_load_round_trips_and_overwrites() {
        let (_dir, db) = open_db();
        let first = CachedPricing {
            fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            json: r#"{"m":{}}"#.to_owned(),
        };
        store(&db, &first).expect("store");
        assert_eq!(load(&db).expect("load"), Some(first));

        let second = CachedPricing {
            fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            json: r#"{"n":{}}"#.to_owned(),
        };
        store(&db, &second).expect("store again");
        assert_eq!(load(&db).expect("load").expect("present").json, r#"{"n":{}}"#);
    }

    #[test]
    fn load_surfaces_a_corrupt_blob_as_error() {
        let (_dir, db) = open_db();
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(PRICING_CACHE).expect("open table");
            table.insert(KEY, b"not json".as_slice()).expect("insert corrupt");
        }
        txn.commit().expect("commit");
        // A decode failure is a diagnosable error the caller can warn on,
        // not a silent cache miss.
        assert!(load(&db).is_err(), "a corrupt row surfaces as an error");
    }
}
