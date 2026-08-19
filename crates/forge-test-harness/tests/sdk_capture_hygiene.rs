//! Assert every committed capture is a fixed point of the redactor:
//! running it again changes nothing. Stated that way it covers the
//! spelling rules without being written FROM them, which is what made a
//! substring blocklist blind.
//!
//! Two things it cannot see. Prose in a captured tool result is not
//! redacted at all - read `session_redact`'s "what it does not cover".
//! And the owner rule is unverifiable here by construction: `for_trace`
//! discovers names only from the path spellings redaction destroys, so on
//! a redacted corpus the discovered set is always empty. Its only
//! enforcement point is `to_jsonl`'s capture-time redaction, covered by
//! `to_jsonl_redacts_every_entry`.
//!
//! On failure, redact the offending file: see
//! `crates/forge-test-harness/README.md`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_test_harness::sdk_wire::baseline_dir;
use forge_test_harness::sdk_wire::session_redact::WireRedactor;

/// Every directory of committed captures. The reference captures are a
/// raw `claude --print` shell redirect that no code path redacts, so they
/// need this guard more than the baselines do.
fn capture_dirs() -> Vec<std::path::PathBuf> {
    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    vec![baseline_dir(), repo_root.join(".claude/skills/claude-cli-upgrade/reference-captures")]
}

fn jsonl_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
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

/// The wire lines in a capture. Baselines wrap each one in a
/// `{"dir","line"}` envelope; reference captures are raw stream-json.
fn wire_lines(body: &str) -> Vec<String> {
    let enveloped = body.lines().next().is_some_and(|l| {
        serde_json::from_str::<serde_json::Value>(l).is_ok_and(|v| v.get("dir").is_some())
    });
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            if !enveloped {
                return l.to_string();
            }
            let v: serde_json::Value = serde_json::from_str(l).expect("outer envelope parses");
            v["line"].as_str().expect("envelope has a line").to_string()
        })
        .collect()
}

#[test]
fn committed_captures_are_a_fixed_point_of_the_redactor() {
    let mut checked = 0usize;
    let mut hits: Vec<String> = Vec::new();

    for dir in capture_dirs() {
        let files = jsonl_files(&dir);
        assert!(!files.is_empty(), "no captures found under {} - has it moved?", dir.display());
        for path in files {
            let lines = wire_lines(&std::fs::read_to_string(&path).expect("read capture"));
            let redactor = match WireRedactor::for_trace(lines.iter().map(String::as_str)) {
                Ok(r) => r,
                Err(e) => {
                    hits.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            checked += 1;
            if lines.is_empty() {
                hits.push(format!("{}: no wire lines at all", path.display()));
            }
            for (i, line) in lines.iter().enumerate() {
                match redactor.redact_line(line) {
                    Ok(again) if again == *line => {}
                    Ok(_) => hits.push(format!(
                        "{}:{}: redacting again changes it",
                        path.display(),
                        i + 1
                    )),
                    Err(e) => hits.push(format!("{}:{}: {e}", path.display(), i + 1)),
                }
            }
        }
    }

    assert!(checked > 0, "no committed captures were checked");
    assert!(
        hits.is_empty(),
        "committed captures are not redacted ({checked} files checked):\n  {}",
        hits.join("\n  ")
    );
}
