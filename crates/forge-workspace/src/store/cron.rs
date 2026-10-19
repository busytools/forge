//! Durable forge-cron persistence on the redb `crons` table.
//!
//! The whole [`CronEntry`] record is stored as serde-json keyed by its
//! `CronId` string - no field schema on disk, so a v1->v2 type change
//! needs no table migration. Machine-local: crons are per-machine, not
//! synced across the user's Macs.

use anyhow::Context;
use forge_primitives::cron::{CronEntry, CronId};
use redb::{ReadableTable, TableDefinition};

use super::Db;

const CRONS: TableDefinition<&str, &[u8]> = TableDefinition::new("crons");

/// Every persisted cron.
pub fn list(db: &Db) -> anyhow::Result<Vec<CronEntry>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(CRONS) {
        Ok(t) => t,
        // A fresh database has no table until the first write; an absent
        // table is an empty list, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (id, value) = entry?;
        match serde_json::from_slice::<CronEntry>(value.value()) {
            Ok(cron) => out.push(cron),
            // One undecodable record (schema drift, a corrupt blob) must not
            // wipe the rest of the durable set - skip it and warn.
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::cron",
                id = %id.value(),
                error = %err,
                "skipping cron record that failed to decode",
            ),
        }
    }
    Ok(out)
}

/// Persist one cron, replacing any prior record with the same id. One
/// write txn per row, so a write can only touch the row it names.
pub fn upsert(db: &Db, cron: &CronEntry) -> anyhow::Result<()> {
    let value = serde_json::to_vec(cron).context("serialize cron")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(CRONS)?;
        table.insert(cron.id.as_str(), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Delete a cron by id. Returns whether a record existed.
pub fn delete(db: &Db, id: &CronId) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let existed = {
        let mut table = txn.open_table(CRONS)?;
        table.remove(id.as_str())?.is_some()
    };
    txn.commit()?;
    Ok(existed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::cron::{CronId, CronKind};
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn epoch(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn cron(id: &str) -> CronEntry {
        CronEntry {
            id: CronId::from(id),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: epoch(1_700_000_000),
            description: None,
            last_fire: None,
            next_fire: epoch(1_700_032_400),
            team_role: None,
            wake: true,
        }
    }

    #[test]
    fn cron_survives_db_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        let entry = cron("persist-me");
        {
            let db = Db::open(&path).expect("open db");
            upsert(&db, &entry).expect("write");
        }
        let db = Db::open(&path).expect("reopen db");
        let restored = list(&db).expect("list after reopen");
        assert_eq!(restored, vec![entry], "a durable cron survives a restart intact");
    }

    #[test]
    fn upsert_replaces_one_row_and_leaves_its_neighbours_alone() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        upsert(&db, &cron("a")).expect("insert a");
        upsert(&db, &cron("b")).expect("insert b");

        let mut moved = cron("a");
        moved.prompt = "moved".to_owned();
        upsert(&db, &moved).expect("replace a");

        let all = list(&db).expect("list");
        assert_eq!(all.len(), 2, "a second upsert adds a row without clearing the table");
        let a = all.iter().find(|c| c.id == CronId::from("a")).expect("a is there");
        assert_eq!(a.prompt, "moved", "and the row it names is the one replaced");
        assert!(all.iter().any(|c| c.id == CronId::from("b")), "the neighbour is untouched");

        assert!(delete(&db, &CronId::from("a")).expect("delete"), "an existing id deletes to true");
        assert!(!delete(&db, &CronId::from("a")).expect("delete again"), "and reports a miss");
        let left = list(&db).expect("list");
        assert_eq!(left.len(), 1, "only the named row is gone");
        assert_eq!(left[0].id, CronId::from("b"));
    }

    #[test]
    fn corrupt_record_is_skipped_not_fatal() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let good = cron("good");
        upsert(&db, &good).expect("write good");

        // A blob that isn't a valid cron must not poison the load.
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(CRONS).expect("open table");
            table.insert("corrupt", "not a cron".as_bytes()).expect("insert corrupt");
        }
        txn.commit().expect("commit");

        let all = list(&db).expect("list tolerates the corrupt blob");
        assert_eq!(all.len(), 1, "the good record survives a corrupt sibling");
        assert_eq!(all[0].id, good.id);
    }

    #[test]
    fn a_field_absent_team_role_deserializes_to_none() {
        // A record an old forge wrote (before team_role existed) has no
        // team_role key at all. The decode relies on it defaulting to a
        // lead cron; the round-trip tests only prove None -> no-key -> None,
        // so pin the field-absent -> None direction against a hand-authored
        // blob that never carried the key.
        let blob = serde_json::json!({
            "id": "legacy",
            "project_name": "forge",
            "kind": { "Recurring": "0 9 * * *" },
            "prompt": "p",
            "created_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "next_fire": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
        });
        let entry: CronEntry =
            serde_json::from_value(blob).expect("a field-absent record deserializes");
        assert_eq!(entry.id, CronId::from("legacy"));
        assert_eq!(entry.team_role, None, "a field-absent team_role is a lead cron");
    }
}
