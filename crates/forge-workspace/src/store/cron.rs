//! Durable forge-cron persistence on the redb `crons` table.
//!
//! The whole [`CronEntry`] record is stored as serde-json keyed by its
//! [`CronId`] string - no field schema on disk, so a v1->v2 type change
//! needs no table migration. Machine-local: crons are per-machine, not
//! synced across the user's Macs.

use anyhow::Context;
use forge_primitives::cron::{CronEntry, CronId};
use redb::{ReadableTable, TableDefinition};

use super::Db;

const CRONS: TableDefinition<&str, &[u8]> = TableDefinition::new("crons");

/// Persist a cron, replacing any prior record with the same id.
pub fn insert(db: &Db, cron: &CronEntry) -> anyhow::Result<()> {
    let value = serde_json::to_vec(cron).context("serialize cron")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(CRONS)?;
        table.insert(cron.id.as_str(), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

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

/// Delete a cron by id. Returns whether a record existed.
pub fn remove(db: &Db, id: &CronId) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let existed = {
        let mut table = txn.open_table(CRONS)?;
        table.remove(id.as_str())?.is_some()
    };
    txn.commit()?;
    Ok(existed)
}

/// Overwrite the table with `crons`: clear every existing row, insert the
/// full set, and - when the set is non-empty - set the one-time seed
/// marker, all in ONE write txn so the crons and the marker land
/// atomically. A delete-to-empty rewrites only the data and never unmarks,
/// so once a machine holds any crons (seeded or natively created) the seed
/// never re-reads cron.toml. Backs both the boot seed and every workspace
/// persist-after-mutation.
pub fn replace_all(db: &Db, crons: &[CronEntry]) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(CRONS)?;
        let existing: Vec<String> =
            table.iter()?.filter_map(Result::ok).map(|(k, _)| k.value().to_owned()).collect();
        for key in existing {
            table.remove(key.as_str())?;
        }
        for cron in crons {
            let value = serde_json::to_vec(cron).context("serialize cron")?;
            table.insert(cron.id.as_str(), value.as_slice())?;
        }
    }
    if !crons.is_empty() {
        mark_seeded(&txn)?;
    }
    txn.commit()?;
    Ok(())
}

/// One-time seed marker. A dedicated single-purpose table so the whole
/// migration path is excised in one place in the cron-redb follow-up.
const CRON_MIGRATION: TableDefinition<&str, &[u8]> = TableDefinition::new("cron_migration");

/// Load crons from the machine-local store, seeding once from the synced
/// `cron.toml`. A machine-local marker (not table emptiness) gates the
/// seed: [`replace_all`] sets it atomically the first time the store gains
/// a non-empty set (from this seed OR a native creation), and a
/// delete-to-empty never clears it, so once a machine has any crons the
/// seed never re-reads cron.toml. An empty/absent cron.toml seeds nothing
/// and leaves the marker unset, so a cron.toml that syncs in later can
/// still migrate a cron-less machine. Removed wholesale in the cron-redb
/// follow-up.
pub fn seed_crons_from_toml_once(db: &Db, config_dir: &std::path::Path) -> Vec<CronEntry> {
    if !seeded(db).unwrap_or(false) {
        let existing = crate::cron_store::load_crons(config_dir);
        if !existing.is_empty()
            && let Err(error) = replace_all(db, &existing)
        {
            tracing::error!(
                target: "forge_workspace::store::cron",
                %error,
                "seeding crons from cron.toml into the store failed",
            );
        }
    }
    list(db).unwrap_or_else(|error| {
        tracing::warn!(
            target: "forge_workspace::store::cron",
            %error,
            "loading durable crons failed; starting with none this run",
        );
        Vec::new()
    })
}

/// Whether the one-time cron.toml seed has already run on this machine.
fn seeded(db: &Db) -> anyhow::Result<bool> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(CRON_MIGRATION) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(table.get("seeded")?.is_some())
}

/// Set the one-time seed marker within `txn`. Called by [`replace_all`]
/// whenever the store gains a non-empty set, so a native cron creation
/// marks the machine just as the boot seed does. Shares the caller's txn
/// so the data and the marker commit atomically.
fn mark_seeded(txn: &redb::WriteTransaction) -> anyhow::Result<()> {
    let mut table = txn.open_table(CRON_MIGRATION)?;
    table.insert("seeded", [1u8].as_slice())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::cron::CronKind;
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
            last_fire: None,
            next_fire: epoch(1_700_032_400),
            team_role: None,
        }
    }

    #[test]
    fn cron_store_round_trip() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let c1 = cron("c1");
        let c2 = cron("c2");
        insert(&db, &c1).expect("insert c1");
        insert(&db, &c2).expect("insert c2");

        let all = list(&db).expect("list");
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|c| c.id == c1.id) && all.iter().any(|c| c.id == c2.id));

        assert!(remove(&db, &c1.id).expect("remove"), "an existing id removes to true");
        assert!(!remove(&db, &c1.id).expect("remove again"), "an absent id removes to false");
        assert_eq!(list(&db).expect("list").len(), 1);
    }

    #[test]
    fn replace_all_clears_then_inserts() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        insert(&db, &cron("old-1")).expect("insert old-1");
        insert(&db, &cron("old-2")).expect("insert old-2");

        // A fresh set that overlaps on one id and drops the other.
        replace_all(&db, &[cron("old-1"), cron("new-3")]).expect("replace");
        let all = list(&db).expect("list after replace");
        assert_eq!(all.len(), 2, "replace mirrors exactly the new set");
        assert!(all.iter().any(|c| c.id == CronId::from("old-1")));
        assert!(all.iter().any(|c| c.id == CronId::from("new-3")));
        assert!(!all.iter().any(|c| c.id == CronId::from("old-2")), "the dropped id is gone");

        replace_all(&db, &[]).expect("replace with empty");
        assert!(list(&db).expect("list").is_empty(), "an empty replace clears the table");
    }

    #[test]
    fn cron_survives_db_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        let entry = cron("persist-me");
        {
            let db = Db::open(&path).expect("open db");
            insert(&db, &entry).expect("insert");
        }
        let db = Db::open(&path).expect("reopen db");
        let restored = list(&db).expect("list after reopen");
        assert_eq!(restored, vec![entry], "a durable cron survives a restart intact");
    }

    #[test]
    fn corrupt_record_is_skipped_not_fatal() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let good = cron("good");
        insert(&db, &good).expect("insert good");

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
    fn seed_marker_prevents_reseed_after_deletion() {
        let dir = tempdir().expect("tempdir");
        crate::config::ensure_forge_data_dir(dir.path()).expect("forge/ dir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        // A synced cron.toml with two crons; the first run seeds them.
        crate::cron_store::store_crons(dir.path(), &[cron("c1"), cron("c2")]);
        assert_eq!(seed_crons_from_toml_once(&db, dir.path()).len(), 2, "first run seeds both");

        // The user deletes every cron; the store is now legitimately empty.
        replace_all(&db, &[]).expect("clear");

        // A second run must NOT re-seed from the still-present cron.toml. A
        // table-emptiness guard would wrongly re-read it; the marker guards.
        let after = seed_crons_from_toml_once(&db, dir.path());
        assert!(after.is_empty(), "the marker prevents re-seeding deleted crons from a stale toml");
    }

    #[test]
    fn replace_all_marks_on_non_empty_and_never_unmarks() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        assert!(!seeded(&db).expect("seeded"), "a fresh store is unmarked");
        replace_all(&db, &[]).expect("empty write");
        assert!(!seeded(&db).expect("seeded"), "an empty write leaves the store unmarked");
        replace_all(&db, &[cron("a")]).expect("non-empty write");
        assert!(seeded(&db).expect("seeded"), "the first non-empty write marks the store");
        replace_all(&db, &[]).expect("delete to empty");
        assert!(seeded(&db).expect("seeded"), "a delete-to-empty must never unmark");
    }

    #[test]
    fn a_native_cron_survives_a_later_synced_toml() {
        let dir = tempdir().expect("tempdir");
        crate::config::ensure_forge_data_dir(dir.path()).expect("forge/ dir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        // Empty first boot: nothing to seed, the store stays unmarked.
        assert!(seed_crons_from_toml_once(&db, dir.path()).is_empty());

        // A cron created natively writes the store (not cron.toml) and, being
        // the first non-empty set, marks the machine.
        replace_all(&db, &[cron("native")]).expect("native create");

        // A non-empty cron.toml then syncs in from an un-migrated Mac.
        crate::cron_store::store_crons(dir.path(), &[cron("from-toml")]);

        // The next boot must not re-seed and clobber the native cron.
        let after = seed_crons_from_toml_once(&db, dir.path());
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].id,
            CronId::from("native"),
            "the native cron survives; the later-synced toml is ignored",
        );
    }

    #[test]
    fn seed_from_toml_sets_the_marker_and_returns_the_crons() {
        let dir = tempdir().expect("tempdir");
        crate::config::ensure_forge_data_dir(dir.path()).expect("forge/ dir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        crate::cron_store::store_crons(dir.path(), &[cron("c1"), cron("c2")]);
        assert!(!seeded(&db).expect("seeded query"), "unseeded before the first run");

        let loaded = seed_crons_from_toml_once(&db, dir.path());
        assert_eq!(loaded.len(), 2, "both crons seed into the store");
        assert!(seeded(&db).expect("seeded query"), "the marker is set after seeding");
    }

    #[test]
    fn seed_ignores_later_toml_growth() {
        let dir = tempdir().expect("tempdir");
        crate::config::ensure_forge_data_dir(dir.path()).expect("forge/ dir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        crate::cron_store::store_crons(dir.path(), &[cron("c1"), cron("c2")]);
        assert_eq!(seed_crons_from_toml_once(&db, dir.path()).len(), 2);

        // A third cron appears in cron.toml (synced from another Mac); the
        // set marker means the next run ignores it.
        crate::cron_store::store_crons(dir.path(), &[cron("c1"), cron("c2"), cron("c3")]);
        let after = seed_crons_from_toml_once(&db, dir.path());
        assert_eq!(after.len(), 2, "a post-seed cron.toml edit does not re-seed");
        assert!(!after.iter().any(|c| c.id == CronId::from("c3")));
    }

    #[test]
    fn seed_with_no_toml_is_empty_and_leaves_marker_unset() {
        let dir = tempdir().expect("tempdir");
        crate::config::ensure_forge_data_dir(dir.path()).expect("forge/ dir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let loaded = seed_crons_from_toml_once(&db, dir.path());
        assert!(loaded.is_empty(), "a missing cron.toml seeds nothing, no panic");
        assert!(
            !seeded(&db).expect("seeded query"),
            "an empty cron.toml leaves the marker unset so a later synced file can still seed",
        );
    }

    #[test]
    fn seed_leaves_cron_toml_byte_unchanged() {
        let dir = tempdir().expect("tempdir");
        crate::config::ensure_forge_data_dir(dir.path()).expect("forge/ dir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        crate::cron_store::store_crons(dir.path(), &[cron("c1"), cron("c2")]);
        let toml_path = crate::config::forge_data_dir(dir.path()).join("cron.toml");
        let before = std::fs::read(&toml_path).expect("read toml before");

        seed_crons_from_toml_once(&db, dir.path());

        let after = std::fs::read(&toml_path).expect("read toml after");
        assert_eq!(before, after, "the synced cron.toml is never modified by the seed");
    }
}
