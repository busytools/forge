//! The handoff log: one row per swap, with the numbers `at_percent` is
//! tuned from.
//!
//! A row is written at the swap and completed once its successor has run
//! its first turns: the fill it fired at, the cycle it ended, and how much
//! of the predecessor's work the successor had to re-derive. That last half
//! is what separates a premature handoff (a successor re-reading
//! everything) from a late one (a compaction that fired first).

use anyhow::Context;
use forge_primitives::{HandoffRecord, HandoffSuccessor, SessionSlot};
use redb::{ReadableTable, TableDefinition};

use super::Db;

const HANDOFFS: TableDefinition<&str, &[u8]> = TableDefinition::new("handoffs");

/// One stored row: the record, and what the successor's re-reads are
/// measured against.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredHandoff {
    record: HandoffRecord,
    /// The predecessor's read paths. Kept here rather than on the record:
    /// no view draws it, and it is only ever read against the successor.
    #[serde(default)]
    predecessor_reads: Vec<String>,
}

/// The key every row of one seat shares. Its rows sort by it first, so a
/// seat's newest row is its last.
fn seat_prefix(slot: &SessionSlot) -> String {
    format!("{}\u{0}{}\u{0}{}\u{0}", slot.org(), slot.project(), slot.label())
}

/// One row's key: the seat, then the instant (RFC 3339 sorts by time), then
/// the successor. The successor is in the key because two handoffs at one
/// instant are still two rows.
fn row_key(record: &HandoffRecord) -> String {
    format!("{}{}\u{0}{}", seat_prefix(&record.slot), record.at, record.to)
}

/// Write one handoff.
pub fn put(db: &Db, record: &HandoffRecord, predecessor_reads: &[String]) -> anyhow::Result<()> {
    let stored =
        StoredHandoff { record: record.clone(), predecessor_reads: predecessor_reads.to_vec() };
    let value = serde_json::to_vec(&stored).context("serialize handoff")?;
    let key = row_key(record);
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(HANDOFFS)?;
        table.insert(key.as_str(), value.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

/// The seat's newest handoff, which is what a view opening on the seat draws
/// as the row between the two conversations.
pub fn latest_for_slot(db: &Db, slot: &SessionSlot) -> anyhow::Result<Option<HandoffRecord>> {
    Ok(rows_for_slot(db, slot)?.pop().map(|stored| stored.record))
}

/// The handoff `occupant` was the successor of, with what its predecessor
/// had read. `None` when this occupant did not arrive by a handoff.
pub fn for_successor(
    db: &Db,
    slot: &SessionSlot,
    occupant: &str,
) -> anyhow::Result<Option<(HandoffRecord, Vec<String>)>> {
    Ok(rows_for_slot(db, slot)?
        .into_iter()
        .find(|stored| stored.record.to == occupant)
        .map(|stored| (stored.record, stored.predecessor_reads)))
}

/// Record how the successor's first turns went.
pub fn set_successor(
    db: &Db,
    slot: &SessionSlot,
    occupant: &str,
    successor: HandoffSuccessor,
) -> anyhow::Result<()> {
    let prefix = seat_prefix(slot);
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(HANDOFFS)?;
        let mut found: Option<(String, StoredHandoff)> = None;
        for entry in table.range(prefix.as_str()..)? {
            let (key, value) = entry?;
            if !key.value().starts_with(&prefix) {
                break;
            }
            let Ok(mut stored) = serde_json::from_slice::<StoredHandoff>(value.value()) else {
                continue;
            };
            if stored.record.to == occupant {
                stored.record.successor = Some(successor);
                found = Some((key.value().to_owned(), stored));
                break;
            }
        }
        if let Some((key, stored)) = found {
            let value = serde_json::to_vec(&stored).context("serialize handoff")?;
            table.insert(key.as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// A seat's rows, oldest first. A row that fails to decode is skipped
/// loudly rather than failing the read: a damaged record is not a reason to
/// stop drawing the handoff that did decode.
fn rows_for_slot(db: &Db, slot: &SessionSlot) -> anyhow::Result<Vec<StoredHandoff>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(HANDOFFS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let prefix = seat_prefix(slot);
    let mut out = Vec::new();
    for entry in table.range(prefix.as_str()..)? {
        let (key, value) = entry?;
        if !key.value().starts_with(&prefix) {
            break;
        }
        match serde_json::from_slice::<StoredHandoff>(value.value()) {
            Ok(stored) => out.push(stored),
            Err(error) => tracing::warn!(
                target: "forge_workspace::store::handoffs",
                row = %key.value(),
                %error,
                "skipping a handoff row that failed to decode",
            ),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use forge_primitives::{HandoffBrief, HandoffCycle, HandoffReason};

    fn open_db(dir: &Path) -> Db {
        Db::open(&dir.join("db.redb")).expect("open db")
    }

    fn record(to: &str, at: &str) -> HandoffRecord {
        HandoffRecord {
            slot: SessionSlot::worker("Busytools", "forge", "handoff-cut"),
            from: "old".to_owned(),
            to: to.to_owned(),
            reason: HandoffReason::Fill,
            fill_percent: Some(85),
            total_tokens: Some(850_000),
            max_tokens: Some(1_000_000),
            compaction_count: 0,
            at_percent: 85,
            brief: HandoffBrief::default(),
            transcript_path: "/tmp/old.jsonl".to_owned(),
            at: at.to_owned(),
            cycle: HandoffCycle { requests: 10, turns: 2, prompt_tokens: 100 },
            successor: None,
        }
    }

    /// The newest row is what a view opening on the seat draws, and the
    /// instant is what orders them.
    #[test]
    fn the_newest_handoff_of_a_seat_is_the_one_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let slot = record("a", "2026-10-24T08:00:00Z").slot;
        put(&db, &record("a", "2026-10-24T08:00:00Z"), &[]).expect("put a");
        put(&db, &record("b", "2026-10-24T09:00:00Z"), &[]).expect("put b");

        let latest = latest_for_slot(&db, &slot).expect("read").expect("a row");
        assert_eq!(latest.to, "b", "the newest swap is the seat's handoff");
    }

    /// A successor is found by the id it runs under, and the readings its
    /// predecessor had done ride with it.
    #[test]
    fn a_successor_is_found_by_its_occupant_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let written = record("new-id", "2026-10-24T08:00:00Z");
        put(&db, &written, &["src/a.rs".to_owned()]).expect("put");

        let (found, reads) = for_successor(&db, &written.slot, "new-id")
            .expect("read")
            .expect("the handoff this occupant arrived by");
        assert_eq!(found.to, "new-id");
        assert_eq!(reads, vec!["src/a.rs".to_owned()], "the predecessor's reads ride along");
        assert!(
            for_successor(&db, &written.slot, "someone-else").expect("read").is_none(),
            "an occupant that arrived another way is nobody's successor",
        );
    }

    /// The counts are written onto the row that named the successor, and
    /// nothing else moves.
    #[test]
    fn the_successors_counts_land_on_their_own_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let written = record("new-id", "2026-10-24T08:00:00Z");
        put(&db, &written, &[]).expect("put");

        let counts = HandoffSuccessor { turns: 10, reread_files: 2, questions: 1, corrections: 0 };
        set_successor(&db, &written.slot, "new-id", counts).expect("write");

        let latest = latest_for_slot(&db, &written.slot).expect("read").expect("a row");
        assert_eq!(latest.successor, Some(counts), "the measurement is kept");
        assert_eq!(latest.fill_percent, Some(85), "and the firing numbers are untouched");
    }

    #[test]
    fn an_absent_table_reads_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let slot = SessionSlot::lead("Busytools", "forge");
        assert!(latest_for_slot(&db, &slot).expect("read").is_none());
    }

    /// One seat's rows never leak into another's, label included.
    #[test]
    fn a_workers_handoff_is_not_its_leads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open_db(dir.path());
        let worker = record("w-new", "2026-10-24T08:00:00Z");
        put(&db, &worker, &[]).expect("put");

        assert!(
            latest_for_slot(&db, &SessionSlot::lead("Busytools", "forge")).expect("read").is_none(),
            "the lead's seat holds no row of its worker's",
        );
    }
}
