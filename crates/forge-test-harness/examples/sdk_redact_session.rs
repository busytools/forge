//! Redact a Claude Code session `.jsonl` into a forge-conformance
//! baseline. Run:
//!
//! ```bash
//! cargo run -p forge-test-harness --example redact_session -- \
//!   <input-session.jsonl> <output-baseline.jsonl>
//! ```
//!
//! Transforms persistence format → stream-json wire shape and applies
//! the redaction rules listed in
//! [`session_redact`](forge_test_harness::sdk_wire::session_redact) -
//! read what those do and do not cover before committing the output,
//! since prose content in a captured tool result is not among them.
//! Output is deterministic, and replays cleanly through
//! `forge-conformance`'s `all_baselines_decode_cleanly` when placed
//! under `baselines/<PINNED_CLI_VERSION>/`.
//!
//! This writes its own `{"dir","line"}` envelope rather than going
//! through `TraceLog::to_jsonl`, so the wire-trace redaction does not
//! apply here.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::ExitCode;

use forge_test_harness::sdk_wire::session_redact::redact_session_path;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut input: Option<std::path::PathBuf> = None;
    let mut output: Option<std::path::PathBuf> = None;
    let mut max_frames: Option<usize> = None;
    let mut iter = args.iter().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--max-frames" => {
                let Some(v) = iter.next() else {
                    eprintln!("--max-frames needs a value");
                    return ExitCode::from(2);
                };
                let Ok(n) = v.parse::<usize>() else {
                    eprintln!("--max-frames: not a number: {v}");
                    return ExitCode::from(2);
                };
                max_frames = Some(n);
            }
            other if other.starts_with('-') => {
                eprintln!("unknown flag: {other}");
                return ExitCode::from(2);
            }
            _ => {
                if input.is_none() {
                    input = Some(std::path::PathBuf::from(a));
                } else if output.is_none() {
                    output = Some(std::path::PathBuf::from(a));
                } else {
                    eprintln!("unexpected argument: {a}");
                    return ExitCode::from(2);
                }
            }
        }
    }
    let (Some(input), Some(output)) = (input, output) else {
        eprintln!(
            "usage: {} [--max-frames N] <input-session.jsonl> <output-baseline.jsonl>\n\n\
             Transforms a Claude Code session persistence file into a \
             redacted, stream-json-shaped baseline suitable for \
             commit under baselines/<PINNED_CLI_VERSION>/.\n\n\
             --max-frames caps the number of frames written (handy for \
             trimming a multi-megabyte session into a small fixture).",
            args.first().map_or("redact_session", String::as_str)
        );
        return ExitCode::from(2);
    };

    let (mut lines, summary) = match redact_session_path(&input) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("redact failed: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!("{summary}");
    if let Some(cap) = max_frames
        && lines.len() > cap
    {
        eprintln!("trimming from {} frames to --max-frames={cap}", lines.len());
        lines.truncate(cap);
    }

    // Emit one `{"dir":"in","line":"..."}` entry per frame, matching
    // what `RecordingTransport` writes - the replay decoder consumes
    // the `in` direction.
    let mut body = String::new();
    for line in &lines {
        let envelope = serde_json::json!({"dir": "in", "line": line});
        body.push_str(&serde_json::to_string(&envelope).expect("envelope serialise"));
        body.push('\n');
    }
    if let Err(e) = std::fs::write(&output, body) {
        eprintln!("write {}: {e}", output.display());
        return ExitCode::from(1);
    }
    eprintln!("wrote {} frames to {}", lines.len(), output.display());
    ExitCode::SUCCESS
}
