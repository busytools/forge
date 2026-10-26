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

use crate::logging::{LOG_ROTATION_MAX_BYTES, RollingFileWriter, default_diagnostics_dir};

/// Where the web client's perf log sits beside the server's own `forge.log`.
const PERF_LOG_NAME: &str = "forge-perf-web.log";

/// One rolled copy, as the shell's sink keeps.
const PERF_ROTATION_FILES: usize = 1;

/// Append one batch to the web client's perf log.
pub(crate) fn append(records: &[serde_json::Value]) -> anyhow::Result<()> {
    let dir = default_diagnostics_dir()?;
    append_in(&dir, records)
}

/// The write itself, over a directory a test can hand it.
fn append_in(dir: &Path, records: &[serde_json::Value]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(PERF_LOG_NAME);
    let mut writer =
        RollingFileWriter::new(&path, true, LOG_ROTATION_MAX_BYTES, PERF_ROTATION_FILES)?;
    for record in records {
        writeln!(writer, "{}", serde_json::to_string(record)?)?;
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
        )
        .expect("append");

        let text = std::fs::read_to_string(dir.path().join(PERF_LOG_NAME)).expect("read back");
        assert!(text.contains("\"invented\":true"), "the page's field survives: {text}");
    }

    /// **The file is capped, not unbounded**: a diagnostic that grows without
    /// a limit is a disk leak with a filename. The shipping cap is 10 MiB;
    /// the rule is the cap, not the number, so this drives a tiny one.
    #[test]
    fn the_log_rolls_at_its_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(PERF_LOG_NAME);
        let mut writer = RollingFileWriter::new(&path, true, 6, PERF_ROTATION_FILES).expect("writer");
        writer.write_all(b"aaaaa\n").expect("first write");
        writer.write_all(b"bbbbb\n").expect("second write");
        writer.flush().expect("flush");

        let fresh = std::fs::read_to_string(&path).expect("read back");
        let rolled =
            std::fs::read_to_string(dir.path().join(format!("{PERF_LOG_NAME}.1"))).expect("rolled");
        assert_eq!(fresh, "bbbbb\n", "the fresh file holds only the batch that crossed: {fresh}");
        assert_eq!(rolled, "aaaaa\n", "and the batch before it is the rolled copy: {rolled}");
    }
}
