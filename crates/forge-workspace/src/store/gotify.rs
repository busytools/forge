//! Gotify subscription CRUD on the redb `subscriptions` table.
//!
//! The whole [`GotifySubscription`] record is stored as serde-json
//! keyed by its uuid bytes - no field schema on disk, so a v1->v2 type
//! change needs no table migration.

use anyhow::Context;
use forge_primitives::GotifySubscription;
use redb::{ReadableTable, TableDefinition};
use uuid::Uuid;

use super::Db;

const SUBS: TableDefinition<[u8; 16], &[u8]> = TableDefinition::new("subscriptions");

/// Persist a subscription, replacing any prior record with the same id.
pub fn insert(db: &Db, sub: &GotifySubscription) -> anyhow::Result<()> {
    let value = serde_json::to_vec(sub).context("serialize subscription")?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(SUBS)?;
        table.insert(sub.id.as_bytes(), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Every persisted subscription.
pub fn list(db: &Db) -> anyhow::Result<Vec<GotifySubscription>> {
    read_all(db)
}

/// Delete a subscription by id. Returns whether a record existed.
pub fn remove(db: &Db, id: Uuid) -> anyhow::Result<bool> {
    let txn = db.database().begin_write()?;
    let existed = {
        let mut table = txn.open_table(SUBS)?;
        table.remove(id.as_bytes())?.is_some()
    };
    txn.commit()?;
    Ok(existed)
}

fn read_all(db: &Db) -> anyhow::Result<Vec<GotifySubscription>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(SUBS) {
        Ok(t) => t,
        // A fresh database has no table until the first write; an absent
        // table is an empty list, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (id, value) = entry?;
        match serde_json::from_slice::<GotifySubscription>(value.value()) {
            Ok(sub) => out.push(sub),
            // One undecodable record (schema drift, a corrupt blob) must not
            // wipe the rest of the durable set - skip it and warn.
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::gotify",
                id = %Uuid::from_bytes(id.value()),
                error = %err,
                "skipping Gotify subscription record that failed to decode",
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use tempfile::tempdir;

    fn sub(project: &str) -> GotifySubscription {
        GotifySubscription {
            id: Uuid::new_v4(),
            project: project.to_owned(),
            team_role: None,
            applications: vec![],
            min_priority: None,
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn subscription_store_round_trip() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let s1 = sub("p1");
        let s2 = sub("p2");
        insert(&db, &s1).expect("insert s1");
        insert(&db, &s2).expect("insert s2");

        let all = list(&db).expect("list");
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|s| s.id == s1.id) && all.iter().any(|s| s.id == s2.id));

        assert!(remove(&db, s1.id).expect("remove"), "an existing id removes to true");
        assert!(!remove(&db, s1.id).expect("remove again"), "an absent id removes to false");
        assert_eq!(list(&db).expect("list").len(), 1);
    }

    #[test]
    fn dynamic_worker_subscription_survives_db_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");

        let mut worker_sub = sub("forge");
        worker_sub.team_role = Some("scratch".to_owned());
        let id = worker_sub.id;
        {
            let db = Db::open(&path).expect("open db");
            insert(&db, &worker_sub).expect("insert");
        }

        // Reopen the store the way a forge restart does; the boot path in
        // workspace.rs rebuilds the active set from `list`.
        let db = Db::open(&path).expect("reopen db");
        let restored = list(&db).expect("list after reopen");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, id);
        assert_eq!(
            restored[0].team_role.as_deref(),
            Some("scratch"),
            "a dynamic worker's durable sub survives a restart with its label intact",
        );
    }

    #[test]
    fn corrupt_record_is_skipped_not_fatal() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let good = sub("p1");
        insert(&db, &good).expect("insert good");

        // A blob that isn't a valid subscription must not poison the load.
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(SUBS).expect("open table");
            table.insert(&[0u8; 16], "not a subscription".as_bytes()).expect("insert corrupt");
        }
        txn.commit().expect("commit");

        let all = list(&db).expect("list tolerates the corrupt blob");
        assert_eq!(all.len(), 1, "the good record survives a corrupt sibling");
        assert_eq!(all[0].id, good.id);
    }
}
