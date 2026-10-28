//! The web page's perf log: the frame records a client with no shell measures,
//! written beside `forge.log`.
//!
//! The page owns the measurement (`client/src/perf.ts`); a shell writes it
//! through its own invoke and a page sends the same batches over the socket.
//! Records arrive as the page serialised them and leave as one JSON line each,
//! under the log's own size cap with a single rolled copy - the log is read
//! after the fact, and the newest window is what a reader wants.

use std::io::Write as _;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::logging::{LOG_ROTATION_MAX_BYTES, RollingFileWriter, default_diagnostics_dir};

/// Where the web client's perf log sits beside the server's own `forge.log`.
const PERF_LOG_NAME: &str = "forge-perf-web.log";

/// One rolled copy, as the shell's sink keeps.
const PERF_ROTATION_FILES: usize = 1;

/// One append at a time.
///
/// **Every connection's batch shares one file**, and a rotation is a rename:
/// two appends interleaving would race each other's rename and split a record
/// across the roll. The lock covers the blocking pool's own callers too.
static WRITE: Mutex<()> = Mutex::new(());

fn write_guard() -> MutexGuard<'static, ()> {
    match WRITE.lock() {
        Ok(guard) => guard,
        // **A panic in one append is not the file's fault**, and poisoning
        // every later batch would be a silent loss. The guard holds no state.
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Append one batch to the web client's perf log.
pub(crate) fn append(records: &[serde_json::Value]) -> anyhow::Result<()> {
    let _guard = write_guard();
    let dir = default_diagnostics_dir()?;
    append_in(&dir, records, LOG_ROTATION_MAX_BYTES)
}

/// The write itself, over a directory and a cap a test can hand it.
fn append_in(dir: &Path, records: &[serde_json::Value], max_bytes: u64) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(PERF_LOG_NAME);
    // **A file already at the cap keeps its rolled copy.** The writer's own
    // at-cap start discards a file that is over the cap; the previous window
    // is the one a reader wants, so it is rolled here and the writer starts
    // on an empty file.
    let current = std::fs::metadata(&path).map_or(0, |meta| meta.len());
    if current >= max_bytes {
        let rolled = dir.join(format!("{PERF_LOG_NAME}.1"));
        let _ = std::fs::remove_file(&rolled);
        std::fs::rename(&path, &rolled)?;
    }
    let mut writer = RollingFileWriter::new(&path, true, max_bytes, PERF_ROTATION_FILES)?;
    for record in records {
        // **One buffer per record**, so a rotation can never split a line.
        let line = format!("{}\n", serde_json::to_string(record)?);
        writer.write_all(line.as_bytes())?;
    }
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A batch lands as one JSON line per record**, so the web page's log
    /// reads exactly as a shell's does.
    #[test]
    fn a_batch_lands_as_one_json_line_each() {
        let dir = tempfile::tempdir().expect("tempdir");
        append_in(
            dir.path(),
            &[
                serde_json::json!({"schema": "forge-perf/v1", "kind": "run_started", "ts_ms": 1}),
                serde_json::json!({"schema": "forge-perf/v1", "kind": "frame", "ms": 120.0}),
            ],
            LOG_ROTATION_MAX_BYTES,
        )
        .expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record: {text}");
        let parsed: serde_json::Value = serde_json::from_str(lines[1]).expect("a JSON line");
        assert_eq!(parsed["kind"], "frame");
        assert_eq!(parsed["ms"], 120.0);
    }

    /// **A record the server does not know still reaches the file.** The page
    /// learns fields the server never parses; writing the page's own JSON is
    /// what keeps a new mark visible without a server change.
    #[test]
    fn a_record_with_a_field_the_server_does_not_know_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        append_in(
            dir.path(),
            &[serde_json::json!({"schema": "forge-perf/v1", "kind": "frame", "invented": true})],
            LOG_ROTATION_MAX_BYTES,
        )
        .expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        assert!(text.contains("\"invented\":true"), "the page's field survives: {text}");
    }

    /// **A file already at the cap keeps its rolled copy.** The writer's own
    /// at-cap start deletes it; here the previous window is what a reader
    /// wants, so the batch lands in a fresh file beside the old one.
    #[test]
    fn a_file_at_the_cap_is_kept_as_the_rolled_copy() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(PERF_LOG_NAME), "x".repeat(20)).expect("seed at the cap");
        append_in(dir.path(), &[serde_json::json!({"kind": "frame", "ts_ms": 1})], 20)
            .expect("append");

        let rolled = std::fs::read_to_string(dir.path().join(format!("{PERF_LOG_NAME}.1")))
            .expect("the previous window");
        assert_eq!(rolled, "x".repeat(20), "the at-cap copy is kept: {rolled}");
        let fresh = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        assert!(fresh.contains("\"frame\""), "and the new batch is in the fresh file: {fresh}");
    }

    /// **A batch that crosses the cap rolls whole records**, not bytes: the
    /// first record stays in the rolled copy and the second opens the fresh
    /// file.
    #[test]
    fn a_batch_crossing_the_cap_rolls_between_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let record =
            |ts: u64| serde_json::json!({ "kind": "frame", "ts_ms": ts, "pad": "aaaaaaaa" });
        append_in(dir.path(), &[record(1), record(2)], 40).expect("append");

        let rolled = std::fs::read_to_string(dir.path().join(format!("{PERF_LOG_NAME}.1")))
            .expect("the first record rolled");
        let fresh = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        assert!(rolled.contains("\"ts_ms\":1"), "the first record is whole in the roll: {rolled}");
        assert!(!rolled.contains("\"ts_ms\":2"), "and the second is not in it: {rolled}");
        assert!(fresh.contains("\"ts_ms\":2"), "the second is whole in the fresh file: {fresh}");
    }
}
