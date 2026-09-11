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

/// Keyed by the workspace label and the conversation id, not by the
/// subscription: a conversation can be watched by more than one
/// subscription, and the cursor belongs to the conversation.
const WATERMARKS: TableDefinition<&str, &str> = TableDefinition::new("slack_watermarks");

/// `\u{0}` joins the pair. A Slack conversation id is alphanumeric and a
/// workspace label is a TOML string, so neither carries one.
fn watermark_key(workspace: &str, conversation: &str) -> String {
    format!("{workspace}\u{0}{conversation}")
}

/// The last `ts` delivered for a conversation in a workspace, or `None`
/// when that conversation has never been swept.
pub fn watermark(db: &Db, workspace: &str, conversation: &str) -> anyhow::Result<Option<String>> {
    let key = watermark_key(workspace, conversation);
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(WATERMARKS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match table.get(key.as_str())? {
        Some(value) => Ok(Some(value.value().to_owned())),
        None => Ok(None),
    }
}

/// Record the last `ts` delivered for a conversation. Stored as the string
/// Slack sent it, never as a number.
pub fn set_watermark(db: &Db, workspace: &str, conversation: &str, ts: &str) -> anyhow::Result<()> {
    let key = watermark_key(workspace, conversation);
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(WATERMARKS)?;
        table.insert(key.as_str(), ts)?;
    }
    txn.commit()?;
    Ok(())
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
    fn a_record_round_trips_with_the_mention_target() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let mut s = sub("p1");
        s.target = SlackSubscriptionTarget::Mentions;
        insert(&db, &s).expect("insert");
        assert_eq!(list(&db).expect("list")[0].target, SlackSubscriptionTarget::Mentions);
    }

    #[test]
    fn a_watermark_round_trips_as_the_string_slack_sent() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        // Trailing zeros and a microsecond fraction: a value that has been
        // through an f64 comes back as "1700000000.0001" and silently moves
        // the cursor, so pin the exact text.
        let ts = "1700000000.000100";
        set_watermark(&db, "acme", "C1", ts).expect("set");
        assert_eq!(
            watermark(&db, "acme", "C1").expect("get"),
            Some(ts.to_owned()),
            "the ts must survive verbatim, never as a number",
        );
    }

    #[test]
    fn an_unset_watermark_reads_as_none() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        assert_eq!(
            watermark(&db, "acme", "C1").expect("a fresh database has no table yet"),
            None,
            "a conversation never swept has no cursor, which is not an error",
        );
    }

    #[test]
    fn a_watermark_is_scoped_to_its_workspace_and_conversation() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        set_watermark(&db, "acme", "C1", "100.000001").expect("set acme/C1");
        set_watermark(&db, "acme", "C2", "200.000002").expect("set acme/C2");
        set_watermark(&db, "beta", "C1", "300.000003").expect("set beta/C1");

        assert_eq!(watermark(&db, "acme", "C1").expect("get"), Some("100.000001".to_owned()));
        assert_eq!(watermark(&db, "acme", "C2").expect("get"), Some("200.000002".to_owned()));
        assert_eq!(
            watermark(&db, "beta", "C1").expect("get"),
            Some("300.000003".to_owned()),
            "the same conversation id in another workspace keeps its own cursor",
        );
        assert_eq!(watermark(&db, "beta", "C2").expect("get"), None);
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
