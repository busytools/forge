//! Assert every committed capture is a fixed point of the redactor:
//! running it again changes nothing. Stated that way it covers the
//! spelling rules without being written FROM them, which is what made a
//! substring blocklist blind.
//!
//! It also refuses a user frame carrying a key outside the wire allowlist.
//! The persistence-to-wire transform drops the disk-only names it knows
//! before a baseline is written, so a committed capture holding one
//! describes a wire shape the CLI cannot send - the state a hand edit or a
//! stale corpus leaves and the fixed-point check cannot see, because the
//! redactor does not rewrite those names either. An allowlist rather than a
//! list of known-bad names is what makes the NEXT disk-only spelling red
//! instead of quiet.
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

use forge_test_harness::sdk_wire::session_redact::{
    RedactState, WireRedactor, transform_persistence_line,
};
use forge_test_harness::sdk_wire::{baseline_dir, legacy_baseline_dir};

/// The keys a user frame may carry on the wire, read from the pinned
/// binary's own three emitters for one (2.1.280): the transcript's user arm
/// (which writes `type`, `message`, `session_id`, `parent_tool_use_id`,
/// `uuid`, `timestamp`, `isSynthetic`, `tool_use_result`,
/// `tool_result_meta`, `origin`), the replay arm (which adds `isReplay` and
/// `file_attachments`), and the hook/subagent arm (which adds
/// `subagent_type` and `task_description`). `is_virtual` is the one the
/// transform writes rather than an emitter: it folds the disk's
/// `isVirtual` into the wire spelling, so a capture generated from a
/// virtual row carries it. **Nothing outside this list is a user-frame
/// key**, so a committed capture carrying one describes a row the CLI
/// cannot send.
///
/// An allowlist fails closed: a capture that legitimately gains a wire
/// field is an explicit edit here, and the gate names the key it refused.
/// The transform's drop list only covers the names it already knows, and
/// the fixed-point check cannot see a name it never rewrites.
const WIRE_USER_FRAME_KEYS: &[&str] = &[
    "file_attachments",
    "isReplay",
    "isSynthetic",
    "is_virtual",
    "message",
    "origin",
    "parent_tool_use_id",
    "session_id",
    "subagent_type",
    "task_description",
    "timestamp",
    "tool_result_meta",
    "tool_use_result",
    "type",
    "uuid",
];

/// The user-frame keys `frame` carries that the wire allowlist does not. The
/// caller has already narrowed `frame` to a user frame.
///
/// **Top-level keys only.** `message` is the wire's own message shape and
/// `tool_use_result` is arbitrary tool output, whose keys are tool-specific
/// and unbounded; neither is the frame's key set, and the transform's drop
/// list is top-level for the same reason. A disk-only name nested inside
/// either was never dropped on the way in, so it is not this gate's subject.
fn unknown_user_keys(frame: &serde_json::Value) -> Vec<String> {
    let Some(map) = frame.as_object() else {
        return Vec::new();
    };
    map.keys().filter(|key| !WIRE_USER_FRAME_KEYS.contains(&key.as_str())).cloned().collect()
}

/// Every directory of committed captures. The reference captures are a
/// raw `claude --print` shell redirect that no code path redacts, so they
/// need this guard more than the baselines do.
fn capture_dirs() -> Vec<std::path::PathBuf> {
    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    vec![
        baseline_dir(),
        legacy_baseline_dir(),
        repo_root.join(".claude/skills/claude-cli-upgrade/reference-captures"),
    ]
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

/// Scan every capture under `dirs`, returning the file count and every reason
/// a file is not a clean, wire-shaped fixed point.
fn scan(dirs: &[std::path::PathBuf]) -> (usize, Vec<String>) {
    let mut checked = 0usize;
    let mut hits: Vec<String> = Vec::new();

    for dir in dirs {
        let files = jsonl_files(dir);
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
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                    && v.get("type").and_then(serde_json::Value::as_str) == Some("user")
                {
                    for key in unknown_user_keys(&v) {
                        hits.push(format!(
                            "{}:{}: a user frame carries the key {key}, \
                             which the wire allowlist does not",
                            path.display(),
                            i + 1
                        ));
                    }
                }
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

    (checked, hits)
}

#[test]
fn committed_captures_are_a_fixed_point_of_the_redactor() {
    let (checked, hits) = scan(&capture_dirs());

    assert!(checked > 0, "no committed captures were checked");
    assert!(
        hits.is_empty(),
        "committed captures are not redacted ({checked} files checked):\n  {}",
        hits.join("\n  ")
    );
}

/// **A key the transform does not know is still caught, and named.** The
/// allowlist exists for the spelling nobody has seen yet: `queueSkipAttachments`
/// is disk-only (the transform drops it) and is not one of the four marks the
/// old guard knew, so a committed capture carrying it used to pass every check.
/// A capture that legitimately gains a wire field is an explicit allowlist
/// edit, not a silent pass.
#[test]
fn a_new_disk_only_key_fails_the_hygiene_gate_by_name() {
    let frame = serde_json::json!({
        "type": "user",
        "session_id": "s1",
        "parent_tool_use_id": null,
        "message": {"role": "user", "content": "a row"},
        "uuid": "u-1",
        "timestamp": "2026-01-01T00:00:00.000Z",
    });
    let mut refused = frame.clone();
    refused["queueSkipAttachments"] = serde_json::json!("disk value");
    let dir = tempfile::tempdir().expect("a scratch dir");
    std::fs::write(dir.path().join("capture.jsonl"), format!("{refused}\n")).expect("write");

    let (checked, hits) = scan(&[dir.path().to_path_buf()]);
    assert_eq!(checked, 1, "the gate did not scan the capture");
    assert!(
        hits.iter().any(|hit| hit.contains("queueSkipAttachments")),
        "the gate passed a user-frame key outside the allowlist: {hits:?}"
    );

    // And the list is read rather than a blanket refusal: the redactor's own
    // output for a virtual row carries `is_virtual`, which is a wire key, so
    // a capture generated from that row must not be refused.
    let disk = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": "a virtual row"},
        "uuid": "uuid-virtual",
        "isVirtual": true,
    });
    let out = transform_persistence_line(&disk.to_string(), &mut RedactState::default())
        .expect("transforms")
        .expect("maps to a wire frame");
    let wire: serde_json::Value = serde_json::from_str(&out).expect("stays valid JSON");
    assert_eq!(
        wire.get("is_virtual"),
        Some(&serde_json::json!(true)),
        "the redactor no longer folds isVirtual into is_virtual, so this half pins nothing"
    );
    assert!(
        unknown_user_keys(&wire).is_empty(),
        "the gate refused a key the redactor itself writes: {:?}",
        unknown_user_keys(&wire)
    );
}
