//! Slack subscription CRUD on the redb `slack_subscriptions` table.
//!
//! The whole [`SlackSubscription`] record is stored as serde-json keyed
//! by its uuid bytes, the shape `store::gotify` uses. It gets its own
//! table rather than sharing Gotify's: the two carry different record
//! shapes, so one table would decode the other's rows as garbage.

use anyhow::Context;
use forge_primitives::slack::{SlackSubscription, SlackThreadOwner, SlackThreadRecord};
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

/// Remove a conversation's sweep cursor, the `__mentions__` stream
/// included. Returns whether one existed.
pub fn remove_watermark(db: &Db, workspace: &str, conversation: &str) -> anyhow::Result<bool> {
    let key = watermark_key(workspace, conversation);
    let txn = db.database().begin_write()?;
    let existed = {
        let mut table = txn.open_table(WATERMARKS)?;
        table.remove(key.as_str())?.is_some()
    };
    txn.commit()?;
    Ok(existed)
}

/// Every conversation with a sweep cursor in one workspace, including the
/// `__mentions__` stream when present.
pub fn watermark_conversations(db: &Db, workspace: &str) -> anyhow::Result<Vec<String>> {
    let prefix = format!("{workspace}\u{0}");
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(WATERMARKS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, _) = entry?;
        if let Some(conversation) = key.value().strip_prefix(&prefix) {
            out.push(conversation.to_owned());
        }
    }
    Ok(out)
}

/// One followed thread per (workspace, conversation, parent) triple. The
/// value is JSON because the owner list rides beside the verbatim reply
/// cursor; a bare string would lose the owners.
const THREADS: TableDefinition<&str, &str> = TableDefinition::new("slack_threads");

/// `\u{0}` joins the triple; none of its parts can carry one.
fn thread_key(workspace: &str, conversation: &str, parent_ts: &str) -> String {
    format!("{workspace}\u{0}{conversation}\u{0}{parent_ts}")
}

/// One thread's record, or `None` when that thread is not followed.
pub fn thread(
    db: &Db,
    workspace: &str,
    conversation: &str,
    parent_ts: &str,
) -> anyhow::Result<Option<SlackThreadRecord>> {
    let key = thread_key(workspace, conversation, parent_ts);
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(THREADS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match table.get(key.as_str())? {
        Some(value) => Ok(Some(serde_json::from_str(value.value())?)),
        None => Ok(None),
    }
}

/// Store one thread's record, replacing any prior row for the triple.
pub fn set_thread(
    db: &Db,
    workspace: &str,
    conversation: &str,
    parent_ts: &str,
    record: &SlackThreadRecord,
) -> anyhow::Result<()> {
    let key = thread_key(workspace, conversation, parent_ts);
    let value = serde_json::to_string(record)?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(THREADS)?;
        table.insert(key.as_str(), value.as_str())?;
    }
    txn.commit()?;
    Ok(())
}

/// Every followed thread of one conversation, as (parent ts, record).
pub fn threads_for(
    db: &Db,
    workspace: &str,
    conversation: &str,
) -> anyhow::Result<Vec<(String, SlackThreadRecord)>> {
    let prefix = format!("{workspace}\u{0}{conversation}\u{0}");
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(THREADS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        let Some(parent_ts) = key.value().strip_prefix(&prefix) else { continue };
        match serde_json::from_str::<SlackThreadRecord>(value.value()) {
            Ok(record) => out.push((parent_ts.to_owned(), record)),
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::slack",
                error = %err,
                "skipping Slack thread record that failed to decode",
            ),
        }
    }
    Ok(out)
}

/// Every followed thread of one workspace, across its conversations, as
/// (conversation, parent ts, record). Backs removal-time pruning.
pub fn threads_for_workspace(
    db: &Db,
    workspace: &str,
) -> anyhow::Result<Vec<(String, String, SlackThreadRecord)>> {
    let prefix = format!("{workspace}\u{0}");
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(THREADS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        let Some(rest) = key.value().strip_prefix(&prefix) else { continue };
        let Some((conversation, parent_ts)) = rest.split_once('\u{0}') else { continue };
        match serde_json::from_str::<SlackThreadRecord>(value.value()) {
            Ok(record) => out.push((conversation.to_owned(), parent_ts.to_owned(), record)),
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::slack",
                error = %err,
                "skipping Slack thread record that failed to decode",
            ),
        }
    }
    Ok(out)
}

/// Delete one thread row. Returns whether a row existed.
pub fn remove_thread(
    db: &Db,
    workspace: &str,
    conversation: &str,
    parent_ts: &str,
) -> anyhow::Result<bool> {
    let key = thread_key(workspace, conversation, parent_ts);
    let txn = db.database().begin_write()?;
    let existed = {
        let mut table = txn.open_table(THREADS)?;
        table.remove(key.as_str())?.is_some()
    };
    txn.commit()?;
    Ok(existed)
}

/// The newest messages forge posted itself, keyed
/// `workspace\0conversation\0ts` and stamped with the seconds it was
/// recorded. The sweep refuses a message whose ts is here, because forge
/// posts as the user and the author can no longer tell them apart.
const OWN_POSTS: TableDefinition<&str, u64> = TableDefinition::new("slack_own_posts");

/// The session that recorded each own post, keyed like [`OWN_POSTS`] and
/// carrying a JSON `SlackThreadOwner`. A row of its own rather than a field
/// on the ledger's value: redb records a table's type names and refuses to
/// open one under a different definition, so widening `OWN_POSTS` past
/// `u64` would fail every existing state database at its first read. A
/// missing entry is an old row and keeps the old rule - suppress for every
/// session.
const OWN_POST_OWNERS: TableDefinition<&str, &[u8]> = TableDefinition::new("slack_own_post_owners");

/// A conversation's newest own posts kept, and the age past which a row is
/// dropped. Both are applied when that conversation records its next post,
/// so a conversation that never posts again keeps what it had. A pruned row
/// costs at most one self-delivery, the same cost as a lost ledger.
const OWN_POST_KEEP: usize = 200;
const OWN_POST_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

fn own_post_prefix(workspace: &str, conversation: &str) -> String {
    format!("{workspace}\u{0}{conversation}\u{0}")
}

fn own_post_key(workspace: &str, conversation: &str, ts: &str) -> String {
    format!("{workspace}\u{0}{conversation}\u{0}{ts}")
}

/// Whether forge posted this message, and whether it was this session that
/// posted it. A row recorded with no owner - an old row, or a caller whose
/// owner did not resolve - suppresses for every session, which is the
/// behaviour rows had before the owner was recorded.
pub fn own_post_by(
    db: &Db,
    workspace: &str,
    conversation: &str,
    ts: &str,
    owner: Option<&SlackThreadOwner>,
) -> anyhow::Result<bool> {
    let key = own_post_key(workspace, conversation, ts);
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(OWN_POSTS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if table.get(key.as_str())?.is_none() {
        return Ok(false);
    }
    let owners = match txn.open_table(OWN_POST_OWNERS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(true),
        Err(e) => return Err(e.into()),
    };
    let Some(stored) = owners.get(key.as_str())? else {
        // The row predates owners: today's rule, refused for everyone.
        return Ok(true);
    };
    let stored: SlackThreadOwner = serde_json::from_slice(stored.value())?;
    Ok(owner == Some(&stored))
}

/// Whether forge posted this message at all, whoever posted it. The mention
/// sweep's guard: a mention in forge's own words must not pull a seat into
/// the conversation it came from.
pub fn own_post_any(
    db: &Db,
    workspace: &str,
    conversation: &str,
    ts: &str,
) -> anyhow::Result<bool> {
    let key = own_post_key(workspace, conversation, ts);
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(OWN_POSTS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(table.get(key.as_str())?.is_some())
}

/// Record a message forge posted at `now` for `owner`, pruning the
/// conversation's rows past either bound and the owner entries with them.
pub fn record_own_post(
    db: &Db,
    workspace: &str,
    conversation: &str,
    ts: &str,
    now: u64,
    owner: Option<&SlackThreadOwner>,
) -> anyhow::Result<()> {
    let prefix = own_post_prefix(workspace, conversation);
    let key = own_post_key(workspace, conversation, ts);
    let encoded = owner.map(serde_json::to_vec).transpose()?;
    let cutoff = now.saturating_sub(OWN_POST_MAX_AGE_SECS);
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(OWN_POSTS)?;
        let mut owners = txn.open_table(OWN_POST_OWNERS)?;
        let mut kept: Vec<String> = Vec::new();
        let mut expired: Vec<String> = Vec::new();
        // This conversation's rows only, and only until the prefix ends:
        // the table holds every conversation's posts.
        for entry in table.range(prefix.as_str()..)? {
            let (stored, recorded) = entry?;
            let stored = stored.value();
            if !stored.starts_with(&prefix) {
                break;
            }
            // Re-recording a ts - an edit, a share read again - refreshes
            // its row; counting it here would prune a live row each time.
            if stored == key.as_str() {
                continue;
            }
            if recorded.value() < cutoff {
                expired.push(stored.to_owned());
            } else {
                kept.push(stored.to_owned());
            }
        }
        for stored in expired {
            table.remove(stored.as_str())?;
            owners.remove(stored.as_str())?;
        }
        // Keys are the timestamps, so the vector is oldest first.
        let excess = kept.len().saturating_add(1).saturating_sub(OWN_POST_KEEP);
        for stored in kept.drain(..excess) {
            table.remove(stored.as_str())?;
            owners.remove(stored.as_str())?;
        }
        table.insert(key.as_str(), now)?;
        match encoded {
            Some(encoded) => {
                owners.insert(key.as_str(), encoded.as_slice())?;
            }
            None => {
                owners.remove(key.as_str())?;
            }
        }
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Db;
    use forge_primitives::slack::{
        SlackSubscriptionTarget, SlackThreadOwner, SlackThreadRecord, SlackWatchMode,
    };
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
            name: Some("ved-test".to_owned()),
            mode: SlackWatchMode::MentionsOnly,
        };
        insert(&db, &s).expect("insert");
        let back = list(&db).expect("list");
        assert_eq!(back[0].target, s.target);
        assert_eq!(back[0].team_role.as_deref(), Some("tester"));
    }

    fn thread_record(cursor: &str, owners: &[(&str, Option<&str>)]) -> SlackThreadRecord {
        SlackThreadRecord {
            cursor: cursor.to_owned(),
            edit_watermark: None,
            participating: false,
            participants: Vec::new(),
            owners: owners
                .iter()
                .map(|(project, role)| SlackThreadOwner {
                    project: project.to_string(),
                    team_role: role.map(str::to_owned),
                })
                .collect(),
        }
    }

    #[test]
    fn a_thread_record_round_trips_with_its_owners_and_verbatim_cursor() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let record = SlackThreadRecord {
            participating: true,
            edit_watermark: Some("1700000000.000400".to_owned()),
            ..thread_record("1700000000.000100", &[("forge", Some("tester")), ("forge", None)])
        };
        set_thread(&db, "acme", "C1", "100.000001", &record).expect("set");
        let back = thread(&db, "acme", "C1", "100.000001").expect("get").expect("present");
        assert_eq!(back.cursor, "1700000000.000100", "the cursor survives verbatim");
        assert_eq!(
            back.edit_watermark.as_deref(),
            Some("1700000000.000400"),
            "the edit stamp survives, or a restart re-offers the correction",
        );
        assert_eq!(back.owners.len(), 2, "both owners ride along");
        assert_eq!(back.owners[0].team_role.as_deref(), Some("tester"));
        assert!(
            back.participating,
            "and the verdict survives: losing it would re-read whole threads",
        );
    }

    #[test]
    fn a_thread_row_is_scoped_to_workspace_conversation_and_parent() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let record = thread_record("100.1", &[("forge", None)]);
        set_thread(&db, "acme", "C1", "100.000001", &record).expect("set acme/C1/100");
        set_thread(&db, "acme", "C1", "200.000002", &record).expect("set acme/C1/200");
        set_thread(&db, "beta", "C1", "100.000001", &record).expect("set beta/C1/100");
        set_thread(&db, "acme", "C2", "100.000001", &record).expect("set acme/C2/100");

        assert_eq!(
            threads_for(&db, "acme", "C1").expect("list").len(),
            2,
            "only acme/C1 rows are that conversation's threads",
        );
        assert!(
            thread(&db, "acme", "C1", "100.000001").expect("get").is_some(),
            "the parent ts is part of the key, not an overwrite",
        );
        assert_eq!(thread(&db, "acme", "C1", "999.9").expect("get"), None);
    }

    /// The pre-owner call shape: a row written with no owner, which is what
    /// an old row is.
    fn record_ownerless(db: &Db, workspace: &str, conversation: &str, ts: &str, now: u64) {
        record_own_post(db, workspace, conversation, ts, now, None).expect("record");
    }

    fn own_ownerless(db: &Db, workspace: &str, conversation: &str, ts: &str) -> bool {
        own_post_by(db, workspace, conversation, ts, None).expect("read")
    }

    #[test]
    fn an_own_post_is_known_only_in_the_conversation_it_was_recorded() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        record_ownerless(&db, "acme", "C1", "100.000001", 1_700_000_000);

        assert!(own_ownerless(&db, "acme", "C1", "100.000001"), "the recorded ts is an own post");
        assert!(!own_ownerless(&db, "acme", "C1", "100.000002"), "a ts never recorded is not");
        assert!(!own_ownerless(&db, "acme", "C2", "100.000001"), "another conversation is not");
        assert!(!own_ownerless(&db, "beta", "C1", "100.000001"), "another workspace is not");
        assert!(
            !own_ownerless(&db, "acme", "C1", "100.0000010"),
            "the ts is compared verbatim, never as a number - both parse to the same f64",
        );
    }

    /// The ledger is bounded two ways: a conversation keeps only its newest
    /// rows, and a row past the age is dropped. Both apply when that
    /// conversation next records a post, so a quiet conversation keeps what
    /// it had. Without either the table grows for the process's whole life.
    #[test]
    fn the_ledger_keeps_only_recent_own_posts() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let now = 1_700_000_000;
        // Slack ts shape: the same-width seconds prefix, so key order and
        // time order agree the way they do on the wire.
        let ts = |offset: u64| format!("{}.000001", now + offset);

        for offset in 0..OWN_POST_KEEP as u64 {
            record_ownerless(&db, "acme", "C1", &ts(offset), now);
        }
        // Re-recording a row already held - an edit, a share read again -
        // refreshes it and prunes nothing.
        record_ownerless(&db, "acme", "C1", &ts(5), now);
        assert!(
            own_ownerless(&db, "acme", "C1", &ts(0)) && own_ownerless(&db, "acme", "C1", &ts(5)),
            "a re-recorded ts is not counted twice",
        );

        record_ownerless(&db, "acme", "C1", &ts(OWN_POST_KEEP as u64), now);
        assert!(
            !own_ownerless(&db, "acme", "C1", &ts(0)),
            "the oldest row past the count bound is dropped",
        );
        assert!(
            own_ownerless(&db, "acme", "C1", &ts(1)),
            "exactly one row is dropped: the next-oldest survives",
        );
        assert!(own_ownerless(&db, "acme", "C1", &ts(OWN_POST_KEEP as u64)), "the new row is kept");

        // The age bound is exclusive: a row recorded exactly at the cutoff
        // is still recent.
        let old = now.saturating_sub(OWN_POST_MAX_AGE_SECS);
        record_ownerless(&db, "acme", "C1", &ts(9_000), old);
        record_ownerless(&db, "acme", "C1", &ts(9_001), now);
        assert!(
            own_ownerless(&db, "acme", "C1", &ts(9_000)),
            "a row exactly at the age bound is kept",
        );
        record_ownerless(&db, "acme", "C1", &ts(9_002), now + 1);
        assert!(
            !own_ownerless(&db, "acme", "C1", &ts(9_000)),
            "a row past the age bound is dropped on the next write",
        );
    }

    /// `range(prefix..)` is a lower bound, not a filter: without the prefix
    /// guard a conversation's count bound would count every later-sorting
    /// conversation's rows and prune them, and those conversations would
    /// then echo their own posts. The filled conversation deliberately
    /// sorts AFTER the recording one, or the mutant passes anyway.
    #[test]
    fn recording_one_conversation_never_prunes_another() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let now = 1_700_000_000;
        let ts = |offset: u64| format!("{}.000001", now + offset);

        for offset in 0..OWN_POST_KEEP as u64 {
            record_ownerless(&db, "acme", "C2", &ts(offset), now);
        }
        record_ownerless(&db, "acme", "C1", &ts(9_000), now);

        assert!(
            own_ownerless(&db, "acme", "C2", &ts(0)),
            "another conversation's oldest row survives the write",
        );
        assert!(
            own_ownerless(&db, "acme", "C1", &ts(9_000)),
            "and the recording conversation keeps its row",
        );
    }

    fn owner(project: &str, role: Option<&str>) -> SlackThreadOwner {
        SlackThreadOwner { project: project.to_owned(), team_role: role.map(str::to_owned) }
    }

    /// The mention guard's read: a row written by any session, and an old
    /// ownerless row, both answer true; an unrecorded ts, another
    /// conversation and another workspace answer false. The port's own
    /// wiring is what the fake cannot prove.
    #[test]
    fn own_post_any_reads_a_row_written_by_anyone() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        record_own_post(
            &db,
            "acme",
            "C1",
            "100.000001",
            1_700_000_000,
            Some(&owner("forge", Some("a"))),
        )
        .expect("record");
        record_ownerless(&db, "acme", "C1", "100.000002", 1_700_000_000);

        assert!(
            own_post_any(&db, "acme", "C1", "100.000001").expect("read"),
            "a session's row answers",
        );
        assert!(
            own_post_any(&db, "acme", "C1", "100.000002").expect("read"),
            "an old ownerless row answers",
        );
        assert!(
            !own_post_any(&db, "acme", "C1", "100.000003").expect("read"),
            "an unrecorded ts does not",
        );
        assert!(
            !own_post_any(&db, "acme", "C2", "100.000001").expect("read"),
            "another conversation does not",
        );
        assert!(
            !own_post_any(&db, "beta", "C1", "100.000001").expect("read"),
            "another workspace does not",
        );
    }

    /// A row written before the ledger carried sessions: the value is the
    /// bare recording time, with no owner beside it. It keeps today's
    /// behaviour, suppressing for every session - the degrade path a live
    /// database's existing rows take.
    #[test]
    fn an_own_post_row_written_before_owners_suppresses_for_every_session() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let ts = "1700000000.000100";
        // The old writer, byte for byte: the table's original value type and
        // no owner entry.
        {
            let txn = db.database().begin_write().expect("write");
            {
                let mut table = txn.open_table(OWN_POSTS).expect("old table");
                table.insert(own_post_key("acme", "C1", ts).as_str(), 1_700_000_000).expect("row");
            }
            txn.commit().expect("commit");
        }

        assert!(
            own_post_by(&db, "acme", "C1", ts, Some(&owner("forge", None))).expect("read"),
            "an ownerless row refuses the lead",
        );
        assert!(
            own_post_by(&db, "acme", "C1", ts, Some(&owner("forge", Some("tester"))))
                .expect("read"),
            "and refuses a worker in the same project",
        );
        assert!(
            own_post_by(&db, "acme", "C1", ts, None).expect("read"),
            "and refuses a caller whose owner could not be resolved",
        );
    }

    /// A post recorded by one session is suppressed only for that session:
    /// every other participant in the thread still receives it.
    #[test]
    fn an_own_post_recorded_by_a_session_suppresses_only_for_that_session() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let ts = "1700000000.000100";
        let poster = owner("forge", Some("tester"));
        record_own_post(&db, "acme", "C1", ts, 1_700_000_000, Some(&poster)).expect("record");

        assert!(
            own_post_by(&db, "acme", "C1", ts, Some(&poster)).expect("read"),
            "the session that posted it never receives its own words",
        );
        assert!(
            !own_post_by(&db, "acme", "C1", ts, Some(&owner("forge", None))).expect("read"),
            "the lead in the same project does",
        );
        assert!(
            !own_post_by(&db, "acme", "C1", ts, Some(&owner("forge", Some("other"))))
                .expect("read"),
            "and so does another worker",
        );
        assert!(
            !own_post_by(&db, "acme", "C1", ts, None).expect("read"),
            "a caller whose owner did not resolve is not the poster",
        );
    }

    /// An expired row takes its owner entry with it. A later re-record of
    /// the same ts without an owner must read as the old ownerless shape,
    /// not inherit the pruned session's owner and stay hidden from everyone
    /// else.
    #[test]
    fn pruning_a_row_takes_its_owner_entry_too() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let now = 1_700_000_000;
        let ts = format!("{now}.000001");
        record_own_post(&db, "acme", "C1", &ts, now, Some(&owner("forge", Some("tester"))))
            .expect("record");

        let old = now.saturating_sub(OWN_POST_MAX_AGE_SECS);
        record_own_post(&db, "acme", "C1", &format!("{}.000002", now + 1), old, None)
            .expect("an old row");
        record_own_post(&db, "acme", "C1", &format!("{}.000003", now + 2), now + 1, None)
            .expect("a write one second on prunes the expired row");

        assert!(
            !own_post_by(&db, "acme", "C1", &ts, Some(&owner("forge", None))).expect("read"),
            "the expired row is gone",
        );
        record_own_post(&db, "acme", "C1", &ts, now + 2, None).expect("re-record ownerless");
        assert!(
            own_post_by(&db, "acme", "C1", &ts, Some(&owner("forge", Some("other"))))
                .expect("read"),
            "the re-recorded row is ownerless, so it suppresses for every session again",
        );
    }

    /// A thread row written before participants existed reads as holding
    /// none, and one written with them carries them back verbatim.
    #[test]
    fn a_thread_row_carries_its_participants_and_an_old_row_reads_empty() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        // The pre-change shape, serialised field for field.
        let legacy =
            r#"{"cursor":"100.1","edit_watermark":null,"owners":[],"participating":false}"#;
        {
            let txn = db.database().begin_write().expect("write");
            {
                let mut table = txn.open_table(THREADS).expect("threads");
                table.insert(thread_key("acme", "C1", "100.000001").as_str(), legacy).expect("row");
            }
            txn.commit().expect("commit");
        }
        let back = thread(&db, "acme", "C1", "100.000001").expect("read").expect("present");
        assert!(back.participants.is_empty(), "an old row holds no participants");

        let record = SlackThreadRecord {
            participants: vec![owner("forge", Some("tester")), owner("forge", None)],
            ..thread_record("100.1", &[])
        };
        set_thread(&db, "acme", "C1", "200.000002", &record).expect("set");
        let back = thread(&db, "acme", "C1", "200.000002").expect("read").expect("present");
        assert_eq!(back.participants.len(), 2, "both participants ride along");
        assert_eq!(back.participants[0].team_role.as_deref(), Some("tester"));
    }

    #[test]
    fn removing_a_thread_row_removes_only_that_row() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        let record = thread_record("100.1", &[("forge", None)]);
        set_thread(&db, "acme", "C1", "100.000001", &record).expect("set");
        set_thread(&db, "acme", "C1", "200.000002", &record).expect("set");

        assert!(remove_thread(&db, "acme", "C1", "100.000001").expect("remove"));
        let left = threads_for(&db, "acme", "C1").expect("list");
        assert_eq!(left.len(), 1, "one row left");
        assert_eq!(left[0].0, "200.000002", "and it is not the removed one");
        assert!(!remove_thread(&db, "acme", "C1", "100.000001").expect("remove again"));
    }
}
