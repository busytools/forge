//! Re-redact committed captures in place, so a new rule in
//! [`session_redact`](forge_test_harness::sdk_wire::session_redact) can be
//! applied to the corpus that was captured before it existed. Run:
//!
//! ```bash
//! cargo run -p forge-test-harness --example sdk_reredact_capture -- \
//!   crates/forge-test-harness/baselines/sdk/2.1.220 \
//!   .claude/skills/claude-cli-upgrade/reference-captures
//! ```
//!
//! Takes files or directories, and handles both committed shapes: a
//! baseline's `{"dir","line"}` envelope and a reference capture's raw
//! stream-json. `--check` reports what would change and writes nothing,
//! which is the same question `sdk_capture_hygiene` asks.
//!
//! Redacting through the library rather than by hand is the point. The
//! gate asserts a committed capture is a fixed point of `WireRedactor`,
//! so anything that reproduces its rules approximately can leave a line
//! that no longer round-trips, and the gate then silently checks a weaker
//! property on it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::process::ExitCode;

use forge_test_harness::sdk_wire::session_redact::WireRedactor;

/// One committed line, split into whatever wrapper it arrived in and the
/// wire line inside. Baselines wrap; reference captures do not.
struct Wrapped {
    envelope: Option<serde_json::Value>,
    wire: String,
}

fn unwrap_line(raw: &str) -> Wrapped {
    let parsed = serde_json::from_str::<serde_json::Value>(raw);
    match parsed {
        Ok(v) if v.get("dir").is_some() => {
            let wire = v["line"].as_str().expect("envelope has a line").to_owned();
            Wrapped { envelope: Some(v), wire }
        }
        _ => Wrapped { envelope: None, wire: raw.to_owned() },
    }
}

fn rewrap(w: &Wrapped, redacted: String) -> String {
    match &w.envelope {
        None => redacted,
        Some(env) => {
            let mut env = env.clone();
            env["line"] = serde_json::Value::String(redacted);
            serde_json::to_string(&env).expect("envelope serialise")
        }
    }
}

fn jsonl_files(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    if path.is_file() {
        return vec![path.to_owned()];
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    out.sort();
    out
}

/// Returns the number of lines the redactor would change, and the new
/// body when any of them do.
fn reredact(path: &std::path::Path) -> Result<(usize, String), String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let wrapped: Vec<Wrapped> =
        body.lines().filter(|l| !l.trim().is_empty()).map(unwrap_line).collect();
    let redactor = WireRedactor::for_trace(wrapped.iter().map(|w| w.wire.as_str()))?;

    let mut changed = 0usize;
    let mut out = String::new();
    for w in &wrapped {
        let redacted = redactor.redact_line(&w.wire)?;
        if redacted != w.wire {
            changed += 1;
        }
        out.push_str(&rewrap(w, redacted));
        out.push('\n');
    }
    Ok((changed, out))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut check_only = false;
    let mut targets: Vec<std::path::PathBuf> = Vec::new();
    for a in args.iter().skip(1) {
        match a.as_str() {
            "--check" => check_only = true,
            other if other.starts_with('-') => {
                eprintln!("unknown flag: {other}");
                return ExitCode::from(2);
            }
            other => targets.push(std::path::PathBuf::from(other)),
        }
    }
    if targets.is_empty() {
        eprintln!(
            "usage: {} [--check] <capture-file-or-dir>...\n\n\
             Re-redacts committed captures in place through WireRedactor, \
             for when a new redaction rule postdates the corpus.\n\n\
             --check reports what would change and writes nothing.",
            args.first().map_or("sdk_reredact_capture", String::as_str)
        );
        return ExitCode::from(2);
    }

    let mut total_changed = 0usize;
    let mut files_changed = 0usize;
    for target in &targets {
        let files = jsonl_files(target);
        if files.is_empty() {
            eprintln!("no .jsonl files under {}", target.display());
            return ExitCode::from(1);
        }
        for path in files {
            match reredact(&path) {
                Err(e) => {
                    eprintln!("{}: {e}", path.display());
                    return ExitCode::from(1);
                }
                Ok((0, _)) => {}
                Ok((changed, out)) => {
                    total_changed += changed;
                    files_changed += 1;
                    println!("{}: {changed} lines", path.display());
                    if !check_only && let Err(e) = std::fs::write(&path, out) {
                        eprintln!("write {}: {e}", path.display());
                        return ExitCode::from(1);
                    }
                }
            }
        }
    }

    let verb = if check_only { "would change" } else { "changed" };
    println!("{verb} {total_changed} lines across {files_changed} files");
    if check_only && total_changed > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS }
}
