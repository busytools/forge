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
//! prompt: a prompt that no longer appears in any transcript keeps a row
//! nothing looks up.

use std::collections::BTreeMap;

use forge_primitives::cron::CronName;
use redb::TableDefinition;

use super::Db;

const CRON_NAMES: TableDefinition<&str, &[u8]> = TableDefinition::new("cron_names");

/// Remember the schedule a fired prompt came from, replacing any prior record
/// for the same prompt.
pub fn put(db: &Db, uuid: &str, name: &CronName) -> anyhow::Result<()> {
    let value = serde_json::to_vec(name)?;
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(CRON_NAMES)?;
        table.insert(uuid, value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
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
    use tempfile::tempdir;

    fn name(description: Option<&str>) -> CronName {
        CronName { cron_id: "cron-1".to_owned(), description: description.map(str::to_owned) }
    }

    #[test]
    fn a_fired_prompt_keeps_its_schedule_across_a_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        {
            let db = Db::open(&path).expect("open db");
            put(&db, "p-1", &name(Some("Morning summary"))).expect("write");
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
        put(&db, "asked", &name(None)).expect("write asked");
        put(&db, "other", &name(Some("not this page"))).expect("write other");
        put(&db, "missing-description", &name(None)).expect("write bare");

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
    fn a_fresh_store_answers_no_pairings_rather_than_failing() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");
        assert!(for_uuids(&db, &["p-1"]).expect("a fresh store reads empty").is_empty());
    }
}
