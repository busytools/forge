//! The schedule each fired prompt came from, keyed by the prompt's own id.
//!
//! The `CronPromptAppended` frame carries the pairing once, when the fire is
//! delivered; a row replayed from a transcript afterwards holds the prompt's
//! prose alone. This is the read half of that pairing - written when the fire
//! is delivered, read when a conversation page is built - so a page that
//! reloads later titles the row as the live frame did (rule 25's read/live
//! symmetry).
//!
//! Machine-local, like the transcript it describes, and one row per fired
//! prompt: the boot sweeps a row once it is past the retention window.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use forge_primitives::cron::CronName;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use super::Db;

const CRON_NAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("cron_names");

/// How long a fired prompt's pairing is kept: long enough that a row replayed
/// after a reload still names its schedule, bounded so the store stops growing.
const RETENTION_SECS: u64 = 30 * 24 * 60 * 60;

/// The stored value: the name plus when the fire happened.
///
/// `fired_at` is optional only so a row written before the stamp existed still
/// decodes; every row [`put`] writes carries it.
#[derive(Serialize, Deserialize)]
struct Stored {
    #[serde(flatten)]
    name: CronName,
    #[serde(default)]
    fired_at: Option<u64>,
}

/// Remember the schedule a fired prompt came from, replacing any prior record
/// for the same prompt.
pub fn put(db: &Db, uuid: &str, name: &CronName, fired_at: SystemTime) -> anyhow::Result<()> {
    let value =
        serde_json::to_vec(&Stored { name: name.clone(), fired_at: Some(epoch(fired_at)) })?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(CRON_NAMES)?;
        table.insert(uuid, value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// Seconds since the epoch, with a clock before it read as the epoch rather
/// than as an error: a wrongly set clock costs retention accuracy, not a boot.
fn epoch(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Drop the pairings older than the retention window and stamp the ones
/// written before the stamp existed, so they age out one window from the
/// upgrade.
///
/// **A record this build cannot decode is left alone**, the same rule the read
/// path keeps: deleting what cannot be read is how a corrupt row becomes a
/// silent loss. Returns how many rows went.
pub fn prune_expired(db: &Db, now: SystemTime) -> anyhow::Result<usize> {
    let now = epoch(now);
    let mut expired = Vec::new();
    let mut unstamped = Vec::new();
    {
        let txn = db.database().begin_read()?;
        let table = match txn.open_table(CRON_NAMES) {
            Ok(table) => table,
            // A store that has never fired has nothing to sweep.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(error) => return Err(error.into()),
        };
        for row in table.iter()? {
            let (uuid, value) = row?;
            match serde_json::from_slice::<Stored>(value.value()) {
                Ok(stored) => match stored.fired_at {
                    Some(at) if now.saturating_sub(at) > RETENTION_SECS => {
                        expired.push(uuid.value().to_owned());
                    }
                    Some(_) => {}
                    None => unstamped.push((uuid.value().to_owned(), stored.name)),
                },
                Err(error) => tracing::warn!(
                    target: "forge_workspace::store::cron_names",
                    event_name = "cron_name_record_undecodable",
                    uuid = %uuid.value(),
                    %error,
                    "leaving a cron-name record this build cannot date",
                ),
            }
        }
    }
    if expired.is_empty() && unstamped.is_empty() {
        return Ok(0);
    }
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(CRON_NAMES)?;
        for uuid in &expired {
            table.remove(uuid.as_str())?;
        }
        for (uuid, name) in &unstamped {
            let bytes = serde_json::to_vec(&Stored { name: name.clone(), fired_at: Some(now) })?;
            table.insert(uuid.as_str(), bytes.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(expired.len())
}

/// The schedules for the prompts in `uuids`, keyed by the prompt's id. An id
/// this store never saw is absent from the answer rather than an error.
pub fn for_uuids(db: &Db, uuids: &[&str]) -> anyhow::Result<BTreeMap<String, CronName>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(CRON_NAMES) {
        Ok(table) => table,
        // A fresh database has no table until the first fire; an absent table
        // is an empty answer, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let mut found = BTreeMap::new();
    for uuid in uuids {
        let Some(value) = table.get(*uuid)? else {
            continue;
        };
        match serde_json::from_slice::<CronName>(value.value()) {
            Ok(name) => {
                found.insert((*uuid).to_owned(), name);
            }
            // One undecodable row must not cost the rest of the page its
            // titles, the same rule the cron table keeps.
            Err(error) => tracing::warn!(
                target: "forge_workspace::store::cron_names",
                event_name = "cron_name_record_undecodable",
                uuid = %uuid,
                %error,
                "skipping a cron-name record that failed to decode",
            ),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    fn name(description: Option<&str>) -> CronName {
        CronName { cron_id: "cron-1".to_owned(), description: description.map(str::to_owned) }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn secs(at: SystemTime) -> u64 {
        at.duration_since(SystemTime::UNIX_EPOCH).expect("after the epoch").as_secs()
    }

    #[test]
    fn a_fired_prompt_keeps_its_schedule_across_a_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        {
            let db = Db::open(&path).expect("open db");
            put(&db, "p-1", &name(Some("Morning summary")), at(1_000)).expect("write");
        }
        let db = Db::open(&path).expect("reopen db");
        let found = for_uuids(&db, &["p-1"]).expect("read");
        assert_eq!(
            found.get("p-1"),
            Some(&name(Some("Morning summary"))),
            "a fire's pairing survives the restart that loses the frame carrying it"
        );
    }

    #[test]
    fn a_read_answers_only_the_prompts_it_was_asked_for() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        put(&db, "asked", &name(None), at(1_000)).expect("write asked");
        put(&db, "other", &name(Some("not this page")), at(1_000)).expect("write other");
        put(&db, "missing-description", &name(None), at(1_000)).expect("write bare");

        let found = for_uuids(&db, &["asked", "missing-description", "never-fired"]).expect("read");
        assert_eq!(
            found.keys().collect::<Vec<_>>(),
            vec!["asked", "missing-description"],
            "a page carries the pairings its own rows need, and nothing else"
        );
        assert_eq!(
            found.get("asked").and_then(|held| held.description.clone()),
            None,
            "a cron registered without a description keeps its absence"
        );
    }

    #[test]
    fn a_sweep_drops_a_pairing_past_the_window_and_keeps_one_inside_it() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let now = at(10_000_000);
        put(&db, "old", &name(None), at(secs(now) - RETENTION_SECS - 1)).expect("write old");
        put(&db, "edge", &name(None), at(secs(now) - RETENTION_SECS)).expect("write edge");
        put(&db, "new", &name(None), at(secs(now))).expect("write new");

        let removed = prune_expired(&db, now).expect("sweep");

        assert_eq!(removed, 1, "only the pairing older than the window goes");
        let kept = for_uuids(&db, &["old", "edge", "new"]).expect("read");
        assert_eq!(
            kept.keys().collect::<Vec<_>>(),
            vec!["edge", "new"],
            "the window keeps a row at its edge and every row inside it"
        );
    }

    #[test]
    fn a_sweep_leaves_a_record_it_cannot_read_alone() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        let txn = db.database().begin_write().expect("begin write");
        {
            let mut table = txn.open_table(CRON_NAMES).expect("open table");
            table.insert("broken", b"not json".as_slice()).expect("insert");
        }
        txn.commit().expect("commit");

        let removed = prune_expired(&db, at(10_000_000)).expect("sweep");

        assert_eq!(removed, 0, "a row this build cannot date is not guessed away");
        let txn = db.database().begin_read().expect("begin read");
        let table = txn.open_table(CRON_NAMES).expect("open table");
        assert!(
            table.get("broken").expect("get").is_some(),
            "a record that will not decode stays readable to a build that can"
        );
    }

    #[test]
    fn a_sweep_stamps_a_pairing_written_before_the_window_existed() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        // A row exactly as the previous version wrote it: the name and nothing
        // that dates it.
        let legacy = serde_json::to_vec(&name(Some("Morning summary"))).expect("serialize");
        let txn = db.database().begin_write().expect("begin write");
        {
            let mut table = txn.open_table(CRON_NAMES).expect("open table");
            table.insert("p-1", legacy.as_slice()).expect("insert");
        }
        txn.commit().expect("commit");
        let now = at(10_000_000);

        let removed = prune_expired(&db, now).expect("sweep");

        assert_eq!(removed, 0, "a row with no age is kept, not dropped");
        assert_eq!(
            for_uuids(&db, &["p-1"]).expect("read").get("p-1"),
            Some(&name(Some("Morning summary"))),
            "the pairing it carried survives the stamp"
        );
        // The stamp is the boot's own instant, so the row ages out one window
        // from the upgrade rather than never.
        assert_eq!(
            prune_expired(&db, at(secs(now) + RETENTION_SECS - 1)).expect("inside the window"),
            0,
            "one second short of the window the row stays, which a stamp of the epoch would fail",
        );
        assert_eq!(
            prune_expired(&db, at(secs(now) + RETENTION_SECS + 1)).expect("second sweep"),
            1,
            "once stamped, the legacy row obeys the window"
        );
    }

    #[test]
    fn a_fresh_store_answers_no_pairings_rather_than_failing() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        assert!(for_uuids(&db, &["p-1"]).expect("a fresh store reads empty").is_empty());
    }
}
