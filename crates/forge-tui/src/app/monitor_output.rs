//! #275 Task 4: read the watched-command stdout from the Monitor
//! task's `output_file` and return the last N lines for the
//! Inspector's MONITORS tail.
//!
//! The CLI's local-bash Monitor flavour streams the command's stdout
//! to a file on disk (path carried via `task_notification.output_file`
//! — confirmed in
//! `~/Projects/forge/.claude/skills/claude-cli-upgrade/reference-captures/monitor.jsonl`)
//! rather than over the wire. Without reading that file, the tail
//! only ever surfaces the "Monitor X stream ended" summary line.
//! This helper tails the file's last `max_lines` lines so the
//! MONITORS section shows the actual command output.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Read the last `max_lines` lines of `path` into a `Vec` ordered
/// oldest-first. Returns `None` on any read error (file missing,
/// permission denied, mid-write corruption) so the caller can
/// distinguish "couldn't read, don't replace the prior tail" from
/// "file is genuinely empty". The empty-file case returns
/// `Some(vec![])`.
///
/// Trailing partial lines (the file is still growing as the monitor
/// runs) are tolerated: `BufRead::lines` yields `Some(Err(_))` for
/// the unterminated chunk and we silently skip it. Standard for
/// tail-style readers.
///
/// Errors emit `tracing::warn!` with the path + reason so operators
/// see why the tail looks empty without forge panicking on a
/// transient filesystem hiccup.
#[must_use]
pub fn read_output_file_tail(path: &Path, max_lines: usize) -> Option<Vec<String>> {
    if max_lines == 0 {
        return Some(Vec::new());
    }
    let file = match File::open(path) {
        Ok(f) => f,
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "monitor_output_file_open_failed",
                message = "could not open Monitor output_file; tail unavailable",
                outcome = "failure",
                path = %path.display(),
                error_kind = ?err.kind(),
                error_message = %err,
            );
            return None;
        }
    };
    let mut ring: VecDeque<String> = VecDeque::with_capacity(max_lines);
    for line in BufReader::new(file).lines() {
        match line {
            Ok(text) => {
                if ring.len() == max_lines {
                    ring.pop_front();
                }
                ring.push_back(text);
            }
            Err(err) => {
                // Mid-write partial line or transient IO error; skip
                // the chunk and keep going. The next refresh will
                // pick up a complete view.
                tracing::warn!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "monitor_output_file_partial_read",
                    message = "Monitor output_file read yielded a partial line; skipping",
                    outcome = "skipped",
                    path = %path.display(),
                    error_kind = ?err.kind(),
                );
            }
        }
    }
    Some(ring.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(contents: &str) -> std::path::PathBuf {
        // Hash the contents so concurrent tests get unique paths.
        use std::hash::{Hash, Hasher};
        let dir = std::env::temp_dir();
        let nonce = std::process::id();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        contents.hash(&mut hasher);
        let id = hasher.finish();
        let path = dir.join(format!("forge_monitor_tail_test_{nonce}_{id}.log"));
        let mut f = File::create(&path).expect("create tmp");
        f.write_all(contents.as_bytes()).expect("write tmp");
        path
    }

    #[test]
    fn tail_returns_last_n_lines_in_order() {
        let path = write_tmp(
            "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\ngolf\nhotel\nindia\njuliet\nkilo\nlima\nmike\nnovember\noscar\n",
        );
        let lines = read_output_file_tail(&path, 12).expect("read ok");
        assert_eq!(lines.len(), 12);
        // Last 12 of 15 -> drops alpha, bravo, charlie.
        assert_eq!(lines.first().map(String::as_str), Some("delta"));
        assert_eq!(lines.last().map(String::as_str), Some("oscar"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tail_under_cap_returns_all_lines() {
        let path = write_tmp("one\ntwo\nthree\n");
        let lines = read_output_file_tail(&path, 12).expect("read ok");
        assert_eq!(lines, vec!["one".to_owned(), "two".to_owned(), "three".to_owned()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_file_returns_empty_vec() {
        let path = write_tmp("");
        let lines = read_output_file_tail(&path, 12).expect("read ok");
        assert!(lines.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_returns_none() {
        let path = std::path::PathBuf::from("/nonexistent/forge_monitor_output_file_test.log");
        let result = read_output_file_tail(&path, 12);
        assert!(result.is_none(), "missing file must return None so caller skips replace");
    }

    #[test]
    fn zero_max_lines_returns_empty_vec_without_reading() {
        // Pass a clearly-nonexistent path; with max_lines=0 we must
        // never even touch it.
        let path = std::path::PathBuf::from("/nonexistent/zero_lines.log");
        let result = read_output_file_tail(&path, 0).expect("zero max yields Ok(empty)");
        assert!(result.is_empty());
    }

    #[test]
    fn handles_trailing_partial_line_without_newline() {
        // File with unterminated trailing line: BufRead::lines yields
        // it as a final Ok([whatever]) entry — included in the tail.
        let path = write_tmp("one\ntwo\npartial-without-newline");
        let lines = read_output_file_tail(&path, 12).expect("read ok");
        assert_eq!(
            lines,
            vec!["one".to_owned(), "two".to_owned(), "partial-without-newline".to_owned()]
        );
        let _ = std::fs::remove_file(&path);
    }
}
