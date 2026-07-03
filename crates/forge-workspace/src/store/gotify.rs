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
        let (_id, value) = entry?;
        out.push(serde_json::from_slice(value.value()).context("deserialize subscription")?);
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
}
