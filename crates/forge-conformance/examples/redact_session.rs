//! Redact a Claude Code session `.jsonl` into a forge-conformance
//! baseline. Run:
//!
//! ```bash
//! cargo run -p forge-conformance --example redact_session -- \
//!   <input-session.jsonl> <output-baseline.jsonl>
//! ```
//!
//! Transforms persistence format → stream-json wire shape, scrubs
//! PII (paths, uuids, message text, tool inputs + results) and
//! writes deterministic, safe-to-commit output. The resulting file
//! replays cleanly through `forge-conformance`'s
//! `all_baselines_decode_cleanly` when placed under
//! `baselines/<PINNED_CLI_VERSION>/`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::ExitCode;

use forge_conformance::session_redact::redact_session_path;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "usage: {} <input-session.jsonl> <output-baseline.jsonl>\n\n\
             Transforms a Claude Code session persistence file into a \
             redacted, stream-json-shaped baseline suitable for \
             commit under baselines/<PINNED_CLI_VERSION>/.",
            args.first().map_or("redact_session", String::as_str)
        );
        return ExitCode::from(2);
    }
    let input = std::path::PathBuf::from(&args[1]);
    let output = std::path::PathBuf::from(&args[2]);

    let (lines, summary) = match redact_session_path(&input) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("redact failed: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!("{summary}");

    // Emit one `{"dir":"in","line":"..."}` entry per frame, matching
    // what `RecordingTransport` writes — the replay decoder consumes
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
