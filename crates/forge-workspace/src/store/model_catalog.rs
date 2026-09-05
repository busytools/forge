//! Cached OpenRouter model catalogs on the redb `model_catalog` table.
//!
//! One row per `ANTHROPIC_BASE_URL`, so a second openrouter-shaped
//! base caches independently. The value is the parsed catalog plus its
//! `fetched_at`; the 24h freshness decision lives in
//! [`forge_providers::model_catalog`].

use anyhow::Context;
use redb::TableDefinition;

use super::Db;
pub use forge_providers::model_catalog::CachedCatalog;

const MODEL_CATALOG: TableDefinition<&str, &[u8]> = TableDefinition::new("model_catalog");

/// The cached catalog for `base_url`, or `None` when nothing has been
/// fetched yet (or the row fails to decode).
pub fn load(db: &Db, base_url: &str) -> anyhow::Result<Option<CachedCatalog>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(MODEL_CATALOG) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match table.get(base_url)? {
        Some(value) => {
            Ok(Some(serde_json::from_slice(value.value()).context("decode cached model catalog")?))
        }
        None => Ok(None),
    }
}

/// Overwrite the cached catalog for `base_url` with `entry`.
pub fn store(db: &Db, base_url: &str, entry: &CachedCatalog) -> anyhow::Result<()> {
    let value = serde_json::to_vec(entry).context("serialize cached model catalog")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(MODEL_CATALOG)?;
        table.insert(base_url, value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    use forge_providers::model_catalog::CatalogModel;

    fn open_db() -> (tempfile::TempDir, Db) {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        (dir, db)
    }

    fn entry(models: usize) -> CachedCatalog {
        CachedCatalog {
            fetched_at: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            models: (0..models)
                .map(|index| CatalogModel {
                    id: format!("vendor/model-{index}"),
                    name: format!("Model {index}"),
                    context_length: 1_048_576,
                    pricing: forge_providers::model_catalog::CatalogPricing {
                        prompt: "0.0000014".to_owned(),
                        completion: "0.0000044".to_owned(),
                    },
                    supported_parameters: vec!["tools".to_owned()],
                    architecture: forge_providers::model_catalog::CatalogArchitecture {
                        modality: "text->text".to_owned(),
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn load_is_none_before_any_fetch() {
        let (_dir, db) = open_db();
        assert!(load(&db, "https://openrouter.ai/api").expect("load").is_none());
    }

    #[test]
    fn store_then_load_round_trips_per_base_url() {
        let (_dir, db) = open_db();
        store(&db, "https://openrouter.ai/api", &entry(10)).expect("store");
        let loaded = load(&db, "https://openrouter.ai/api").expect("load").expect("present");
        assert_eq!(loaded.models.len(), 10);

        // A second base url caches independently.
        assert!(load(&db, "https://other.example/api").expect("load").is_none());
        store(&db, "https://other.example/api", &entry(1)).expect("store");
        assert_eq!(
            load(&db, "https://other.example/api").expect("load").expect("present").models.len(),
            1
        );
        assert_eq!(
            load(&db, "https://openrouter.ai/api").expect("load").expect("present").models.len(),
            10
        );
    }

    #[test]
    fn store_overwrites_the_prior_snapshot() {
        let (_dir, db) = open_db();
        let base = "https://openrouter.ai/api";
        store(&db, base, &entry(10)).expect("first store");
        store(&db, base, &entry(2)).expect("second store");
        assert_eq!(load(&db, base).expect("load").expect("present").models.len(), 2);
    }

    #[test]
    fn load_surfaces_a_corrupt_blob_as_error() {
        let (_dir, db) = open_db();
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(MODEL_CATALOG).expect("open table");
            table
                .insert("https://openrouter.ai/api", b"not json".as_slice())
                .expect("insert corrupt");
        }
        txn.commit().expect("commit");
        // A decode failure is a diagnosable error the caller can warn on,
        // not a silent cache miss.
        assert!(
            load(&db, "https://openrouter.ai/api").is_err(),
            "a corrupt row surfaces as an error"
        );
    }
}
