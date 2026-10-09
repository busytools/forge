//! A command's own output: the tail of the file the CLI streams it to.
//!
//! The CLI's local-bash tasks write the command's stdout to a file on disk
//! and name it in `task_notification.output_file` rather than sending it over
//! the wire (confirmed in
//! `~/Projects/forge/.claude/skills/claude-cli-upgrade/reference-captures/monitor.jsonl`),
//! so the tail is a file read. The readers are the session task - which
//! resolves the path from its own conversation and answers a view's
//! `ReadCallOutput` with the result - and the views that hold a path already
//! (the terminal and the web pages tail a Monitor's file on their own tick).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

/// Largest slice of the file read at a time. Output files grow without bound
/// (cargo build, npm install) and this runs inline on a view's own tick, so
/// the read seeks to within this window of the end rather than reading the
/// whole file each time. 64 KiB comfortably holds the last few lines at any
/// realistic line length.
pub const TAIL_WINDOW_BYTES: u64 = 64 * 1024;

/// Read the last `max_lines` lines of `path` into a `Vec` ordered
/// oldest-first. Returns `None` on any read error (file missing,
/// permission denied, a read that failed) so the caller can distinguish
/// "could not read, keep the tail already held" from "the file is
/// genuinely empty". The empty-file case returns `Some(vec![])`.
///
/// The lines come back as the command wrote them, escape sequences and all.
/// What a renderer has to drop to draw them is the renderer's business: a
/// terminal and a page drop different bytes, so neither rule belongs on a
/// read that only hands the text over.
///
/// Only the final [`TAIL_WINDOW_BYTES`] are read: for a larger file the
/// read seeks to `len - TAIL_WINDOW_BYTES` and drops the first (probably
/// partial) line after the seek. Files under the window are read whole.
/// A file the watched command is still writing is read as it stands: an
/// unterminated final line comes back as `Ok` and is kept. The one line
/// that is dropped for good is one that is not valid UTF-8, which is
/// skipped on every read rather than once.
pub fn read_output_file_tail(path: &Path, max_lines: usize) -> Option<Vec<String>> {
    if max_lines == 0 {
        return Some(Vec::new());
    }
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(err) => {
            // A file that will not open is the session's own condition, and
            // a view is answered for it by name (`FileGone`): it is not
            // forge's health, so it stays out of WARN's signal - the level a
            // reader acts on (rule 20).
            tracing::debug!(
                event_name = "output_tail_open_failed",
                message = "could not open an output file; tail unavailable",
                outcome = "failure",
                path = %path.display(),
                error_kind = ?err.kind(),
                error_message = %err,
            );
            return None;
        }
    };
    // Seek to within TAIL_WINDOW_BYTES of the end so per-tick cost is
    // bounded by the window, not the (growing) full file size. After a
    // mid-file seek the first line is probably partial, so drop it.
    let file_len = file.metadata().map_or(0, |m| m.len());
    let drop_partial_first = file_len > TAIL_WINDOW_BYTES
        && file.seek(SeekFrom::Start(file_len - TAIL_WINDOW_BYTES)).is_ok();
    let mut ring: VecDeque<String> = VecDeque::with_capacity(max_lines);
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        match line {
            Ok(text) => {
                if idx == 0 && drop_partial_first {
                    continue;
                }
                if ring.len() == max_lines {
                    ring.pop_front();
                }
                ring.push_back(text);
            }
            Err(err) => {
                // A line that is not valid UTF-8, or a read that failed.
                // An unterminated final line does not reach here: `lines`
                // hands that back as `Ok` and it is kept. The line is
                // skipped and the others stand, which is forge's own
                // reading of the session's own output rather than a problem
                // with forge, so it is not a warning. The decodable case is
                // not transient either: those bytes are skipped on every
                // read, while a failed read may well come right at the
                // next one.
                tracing::debug!(
                    event_name = "output_tail_line_unreadable",
                    message = "an output file holds a line that could not be read; skipping it",
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
    fn read_output_file_tail_hands_over_the_bytes_the_command_wrote() {
        // Real-world Monitor commands (cargo build, npm install, anything
        // with progress bars) emit ANSI colour codes + carriage returns for
        // in-place line updates + the occasional BEL/backspace. The read
        // hands them over as written: dropping them is the renderer's rule,
        // and a terminal and a page drop different bytes.
        let raw = "\
\x1b[32mline 1 green\x1b[0m\n\
line 2 with \rcarriage return\n\
\x1b[1;31mline 3 bold red\x1b[0m\x07\n\
line 4 with \x08\x08backspace\n";
        let path = write_tmp(raw);
        let tail = read_output_file_tail(&path, 12).expect("read ok");

        assert!(
            tail.iter().any(|line| line.contains("\u{1b}[32m")),
            "the colour the command wrote is still in the tail: {tail:?}",
        );
        assert!(
            tail.iter().any(|line| line.contains('\r')),
            "the carriage return the command wrote is still in the tail: {tail:?}",
        );
        assert!(
            tail.iter().any(|line| line.contains("line 1 green")),
            "and the words it wrote are there beside them: {tail:?}",
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn handles_trailing_partial_line_without_newline() {
        // File with unterminated trailing line: BufRead::lines yields
        // it as a final Ok([whatever]) entry, included in the tail.
        let path = write_tmp("one\ntwo\npartial-without-newline");
        let lines = read_output_file_tail(&path, 12).expect("read ok");
        assert_eq!(
            lines,
            vec!["one".to_owned(), "two".to_owned(), "partial-without-newline".to_owned()]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn large_file_reads_only_the_tail_window() {
        // A file well past TAIL_WINDOW_BYTES must still return the last
        // max_lines complete lines (proves the seek path + partial-first
        // drop keep the real tail).
        use std::fmt::Write as _;
        let filler = "x".repeat(100);
        let mut contents = String::new();
        for i in 0..1200 {
            let _ = writeln!(contents, "filler-{i}-{filler}");
        }
        for tag in ["tail-a", "tail-b", "tail-c"] {
            contents.push_str(tag);
            contents.push('\n');
        }
        assert!(contents.len() as u64 > TAIL_WINDOW_BYTES, "fixture must exceed the window");
        let path = write_tmp(&contents);
        let lines = read_output_file_tail(&path, 3).expect("read ok");
        assert_eq!(lines, vec!["tail-a".to_owned(), "tail-b".to_owned(), "tail-c".to_owned()]);
        let _ = std::fs::remove_file(&path);
    }
}
