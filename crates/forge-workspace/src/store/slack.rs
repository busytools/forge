//! Slack subscription CRUD on the redb `slack_subscriptions` table.
//!
//! The whole [`SlackSubscription`] record is stored as serde-json keyed
//! by its uuid bytes, the shape `store::gotify` uses. It gets its own
//! table rather than sharing Gotify's: the two carry different record
//! shapes, so one table would decode the other's rows as garbage.

use anyhow::Context;
use forge_primitives::slack::SlackSubscription;
use redb::{ReadableTable, TableDefinition};
use uuid::Uuid;

use super::Db;

const SUBS: TableDefinition<[u8; 16], &[u8]> = TableDefinition::new("slack_subscriptions");

/// Persist a subscription, replacing any prior record with the same id.
pub fn insert(db: &Db, sub: &SlackSubscription) -> anyhow::Result<()> {
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
pub fn list(db: &Db) -> anyhow::Result<Vec<SlackSubscription>> {
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

fn read_all(db: &Db) -> anyhow::Result<Vec<SlackSubscription>> {
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
        match serde_json::from_slice::<SlackSubscription>(value.value()) {
            Ok(sub) => out.push(sub),
            // One undecodable record (schema drift, a corrupt blob) must not
            // wipe the rest of the durable set - skip it and warn.
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::slack",
                id = %Uuid::from_bytes(id.value()),
                error = %err,
                "skipping Slack subscription record that failed to decode",
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;
    use forge_primitives::slack::{SlackSubscriptionTarget, SlackWatchMode};
    use tempfile::tempdir;

    fn sub(project: &str) -> SlackSubscription {
        SlackSubscription {
            id: Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: project.to_owned(),
            team_role: None,
            target: SlackSubscriptionTarget::DirectMessages,
            created_at: std::time::SystemTime::UNIX_EPOCH,
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
        assert_eq!(list(&db).expect("list").len(), 2);

        assert!(remove(&db, s1.id).expect("remove"));
        let left = list(&db).expect("list");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, s2.id);
    }

    #[test]
    fn an_absent_table_reads_as_empty_rather_than_erroring() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        assert!(list(&db).expect("a fresh database has no table yet").is_empty());
    }

    #[test]
    fn a_conversation_subscription_keeps_its_mode() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let mut s = sub("p1");
        s.team_role = Some("tester".to_owned());
        s.target = SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            mode: SlackWatchMode::MentionsOnly,
        };
        insert(&db, &s).expect("insert");
        let back = list(&db).expect("list");
        assert_eq!(back[0].target, s.target);
        assert_eq!(back[0].team_role.as_deref(), Some("tester"));
    }
}
