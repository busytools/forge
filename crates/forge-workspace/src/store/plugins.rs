//! Plugin update history on the redb `plugin_updates` table: one row
//! per installed entry (key `plugin_id|scope`), holding the latest
//! update forge applied to it - what moved, from where, and the
//! marketplace ref a rollback can restore. Values are serde-json.

use std::collections::BTreeMap;

use anyhow::Context;
use redb::{ReadableTable, TableDefinition};

use super::Db;
use forge_primitives::plugins::PluginUpdateRecord;

const PLUGIN_UPDATES: TableDefinition<&str, &[u8]> = TableDefinition::new("plugin_updates");

/// The store key for one installed entry.
fn record_key(plugin_id: &str, scope: &str) -> String {
    format!("{plugin_id}|{scope}")
}

/// Persist one batch of update outcomes in a single transaction,
/// replacing any earlier record for the same installed entries. A
/// mid-batch failure leaves nothing half-written.
pub fn record_updates(db: &Db, records: &[PluginUpdateRecord]) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(PLUGIN_UPDATES)?;
        for record in records {
            let value = serde_json::to_vec(record).context("serialize plugin update record")?;
            table
                .insert(record_key(&record.plugin_id, &record.scope).as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// Every remembered update, keyed by `plugin_id|scope`. A record that
/// fails to decode is skipped with a warn so one corrupt blob can't
/// take the history down.
pub fn update_records(db: &Db) -> anyhow::Result<BTreeMap<String, PluginUpdateRecord>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(PLUGIN_UPDATES) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = BTreeMap::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        match serde_json::from_slice::<PluginUpdateRecord>(value.value()) {
            Ok(record) => {
                out.insert(key.value().to_owned(), record);
            }
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::plugins",
                key = %key.value(),
                error = %err,
                "skipping plugin update record that failed to decode",
            ),
        }
    }
    Ok(out)
}

/// Drop the record for one installed entry - the rollback a record
/// made possible has been used, and there is no earlier version to go
/// back to behind it.
pub fn clear_update_record(db: &Db, plugin_id: &str, scope: &str) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(PLUGIN_UPDATES)?;
        table.remove(record_key(plugin_id, scope).as_str())?;
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn record(plugin_id: &str, to_version: &str) -> PluginUpdateRecord {
        PluginUpdateRecord {
            plugin_id: plugin_id.to_owned(),
            marketplace: "probe-market".to_owned(),
            scope: "user".to_owned(),
            cwd_raw: "/proj".to_owned(),
            from_version: Some("0.1.0".to_owned()),
            to_version: Some(to_version.to_owned()),
            marketplace_ref_before: Some("2d7d4c6".to_owned()),
            updated_at: "2026-09-04T06:00:00Z".to_owned(),
            trigger: forge_primitives::plugins::PluginUpdateTrigger::Auto,
        }
    }

    #[test]
    fn records_round_trip_and_replace_by_key() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        record_updates(&db, &[record("hello@probe-market", "0.2.0")]).expect("write");
        record_updates(&db, &[record("pensive@claude-night-market", "1.7.3")]).expect("write");
        record_updates(&db, &[record("hello@probe-market", "0.3.0")]).expect("overwrite");

        let records = update_records(&db).expect("read");
        assert_eq!(records.len(), 2, "one row per installed entry");
        assert_eq!(records["hello@probe-market|user"].to_version.as_deref(), Some("0.3.0"));
        assert_eq!(records["hello@probe-market|user"].marketplace, "probe-market");
    }

    #[test]
    fn records_survive_db_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        {
            let db = Db::open(&path).expect("open db");
            record_updates(&db, &[record("hello@probe-market", "0.2.0")]).expect("write");
        }
        let reopened = Db::open(&path).expect("reopen db");
        let records = update_records(&reopened).expect("read");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn an_empty_store_reads_as_no_records() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        assert!(update_records(&db).expect("read").is_empty());
    }

    /// A consumed rollback drops its record; the record's promise is
    /// one rollback, not a permanent one.
    #[test]
    fn clear_removes_only_the_target_record() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        record_updates(
            &db,
            &[
                record("hello@probe-market", "0.2.0"),
                record("pensive@claude-night-market", "1.7.3"),
            ],
        )
        .expect("write");

        clear_update_record(&db, "hello@probe-market", "user").expect("clear");

        let records = update_records(&db).expect("read");
        assert_eq!(records.len(), 1, "only the rolled-back entry is forgotten");
        assert!(records.contains_key("pensive@claude-night-market|user"));
    }
}
