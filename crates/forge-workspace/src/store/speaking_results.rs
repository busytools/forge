//! What this machine's speaking benches measured, keyed by what they ran.
//!
//! App-level, like the words bench's results: the corpus is the machine's,
//! and a result for a different corpus is not a comparison. **The key is
//! `(model file, corpus sha)`** - overwriting the row for the same corpus is
//! what keeps a re-run from stacking a second copy of the same number.

use redb::{ReadableTable, TableDefinition};

use crate::bench::SpeakingBenchResult;

use super::Db;

/// One row per (model file, corpus), keyed by the two of them.
const RESULTS: TableDefinition<&str, &[u8]> = TableDefinition::new("speaking_bench_results");

/// The key one result is filed under.
pub fn key(result: &SpeakingBenchResult) -> String {
    format!("{}|{}", result.file, result.corpus.sha256)
}

/// Every saved result, newest first.
pub fn results(db: &Db) -> anyhow::Result<Vec<SpeakingBenchResult>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(RESULTS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut rows = Vec::new();
    for entry in table.iter()? {
        let (_, value) = entry?;
        rows.push(serde_json::from_slice(value.value())?);
    }
    rows.sort_by(|a: &SpeakingBenchResult, b: &SpeakingBenchResult| b.at.cmp(&a.at));
    Ok(rows)
}

/// Save one result, replacing any row for the same (model file, corpus).
pub fn save(db: &Db, result: &SpeakingBenchResult) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(RESULTS)?;
        table.insert(key(result).as_str(), serde_json::to_vec(result)?.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_dictate::bench::CorpusId;
    use forge_tts::bench::TtsBenchResult;

    fn result(file: &str, sha: &str, at: &str) -> SpeakingBenchResult {
        SpeakingBenchResult {
            file: file.to_owned(),
            metrics: TtsBenchResult {
                clips: 3,
                round_trip_wer: 0.125,
                first_audio_ms: 3_400,
                realtime_factor: 0.3,
            },
            at: at.to_owned(),
            corpus: CorpusId { clips: 3, audio_seconds: 9, sha256: sha.to_owned() },
        }
    }

    fn db(dir: &std::path::Path) -> Db {
        Db::open(&dir.join("speaking.redb")).expect("a store")
    }

    #[test]
    fn a_result_round_trips_and_a_rerun_over_the_same_corpus_replaces_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = db(dir.path());

        save(&store, &result("qwen.gguf", "aaaa", "2026-11-01T10:00:00Z")).unwrap();
        save(&store, &result("qwen.gguf", "bbbb", "2026-11-01T11:00:00Z")).unwrap();
        let rows = results(&store).unwrap();
        assert_eq!(rows.len(), 2, "two corpora are two results");
        assert_eq!(rows[0].at, "2026-11-01T11:00:00Z", "newest first");
        assert_eq!(rows[0].metrics.round_trip_wer, 0.125, "the figures cross whole");

        save(&store, &result("qwen.gguf", "aaaa", "2026-11-01T12:00:00Z")).unwrap();
        let rows = results(&store).unwrap();
        assert_eq!(rows.len(), 2, "a re-run replaces its own row");
        assert_eq!(rows.iter().filter(|row| row.corpus.sha256 == "aaaa").count(), 1);
    }
}
