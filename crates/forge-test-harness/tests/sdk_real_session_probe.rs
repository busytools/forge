//! Opt-in decoder probe against Claude Code's on-disk session .jsonl
//! files.
//!
//! Claude Code persists each session to
//! `$CLAUDE_CONFIG_DIR/projects/<project-slug>/<session-id>.jsonl`.
//! The persistence format is a near-superset of the stream-json wire
//! protocol but uses camelCase (`sessionId`) and carries extra
//! persistence-only fields (`parentUuid`, `cwd`, `timestamp`, …). A
//! transformer in [`forge_test_harness::sdk_wire::session_redact`] rewrites each
//! line into wire shape + redacts PII.
//!
//! When `FORGE_REAL_SESSIONS=<path>` is set, this test walks that
//! directory tree, finds every `*.jsonl` except Workflow journals,
//! transforms each decodable entry, and feeds it through
//! `decode_dispatch`. Purpose: catch decoder regressions against
//! real-world data the developer has accumulated without committing
//! any session content to the repo.
//!
//! Typical invocation (point it at one project: a full `projects` dir
//! is minutes of work and exceeds nextest's default 120s test
//! timeout):
//!
//! ```bash
//! FORGE_REAL_SESSIONS="${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects/<slug>" \
//!   cargo nextest run -p forge-test-harness --no-capture \
//!   real_session_decode_probe
//! ```
//!
//! No output is persisted - failures surface as stderr lines plus a
//! panic summarising the count. To produce a committed redacted
//! baseline from a specific session, run the `redact-session` example
//! instead.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

use forge_primitives::ContentBlock;
use forge_primitives::Message;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use forge_test_harness::sdk_wire::session_redact::redact_session_file;

/// Walk a decoded `Message` for any `ContentBlock::Unknown` variants
/// and surface them as `(type_str, snippet)` pairs. The snippet is a
/// short JSON preview of the unknown block so a consumer (or
/// human reading logs) can identify the shape without dumping the
/// entire payload.
fn unknown_content_blocks(msg: &Message) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let blocks: Option<&[ContentBlock]> = match msg {
        Message::Assistant { message, .. } => Some(&message.content),
        Message::User { message, .. } => Some(&message.content),
        _ => None,
    };
    if let Some(blocks) = blocks {
        for block in blocks {
            if let ContentBlock::Unknown { type_str, raw } = block {
                let preview =
                    serde_json::to_string(raw).unwrap_or_default().chars().take(200).collect();
                out.push((type_str.clone(), preview));
            }
        }
    }
    out
}

fn jsonl_files_under(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            out.extend(jsonl_files_under(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
    out
}

/// Matches on file name alone, at any path: the Workflow layout has a
/// `workflows/` dir both beside and under `subagents/`, so a path check
/// would miss the spelling it does not know. A journal shares the
/// `.jsonl` extension but is not a session transcript - its `result`
/// rows are workflow events with no `subtype`, which the decoder reads
/// as corrupt frames.
fn is_workflow_journal(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some("journal.jsonl")
}

/// One probe run's tally over a directory tree.
#[derive(Default)]
struct ProbeReport {
    files_seen: usize,
    journals_skipped: usize,
    files_transform_failed: usize,
    frames_tried: usize,
    frames_decoded: usize,
    unknown_types: Vec<(PathBuf, usize, String)>,
    unknown_blocks: Vec<(PathBuf, usize, String, String)>,
    decode_errors: Vec<(PathBuf, usize, String)>,
}

fn probe_root(root: &Path) -> ProbeReport {
    let mut report = ProbeReport::default();
    let mut files = jsonl_files_under(root);
    report.journals_skipped = files.iter().filter(|p| is_workflow_journal(p)).count();
    files.retain(|p| !is_workflow_journal(p));

    for path in &files {
        report.files_seen += 1;
        let Ok(body) = std::fs::read_to_string(path) else {
            eprintln!("skipping unreadable {}", path.display());
            continue;
        };
        let transformed = match redact_session_file(&body) {
            Ok(lines) => lines,
            Err(e) => {
                report.files_transform_failed += 1;
                eprintln!("transform failed on {}: {e}", path.display());
                continue;
            }
        };
        for (idx, line) in transformed.iter().enumerate() {
            report.frames_tried += 1;
            match decode_dispatch(line, (idx + 1) as u64) {
                DecodedLine::Unknown { type_str, raw: _ } => {
                    report.unknown_types.push((path.clone(), idx + 1, type_str));
                }
                DecodedLine::Message(msg) => {
                    report.frames_decoded += 1;
                    for (block_type, preview) in unknown_content_blocks(&msg) {
                        report.unknown_blocks.push((path.clone(), idx + 1, block_type, preview));
                    }
                }
                DecodedLine::Malformed { line, reason } => {
                    let line: usize = usize::try_from(line).unwrap_or(usize::MAX);
                    report.decode_errors.push((path.clone(), line, reason));
                }
                _ => {
                    report.frames_decoded += 1;
                }
            }
        }
    }
    report
}

#[test]
fn real_session_decode_probe() {
    let Ok(root_str) = std::env::var("FORGE_REAL_SESSIONS") else {
        eprintln!(
            "FORGE_REAL_SESSIONS not set; skipping real-session probe. \
             Set it to e.g. $CLAUDE_CONFIG_DIR/projects to run."
        );
        return;
    };
    let root = PathBuf::from(&root_str);
    assert!(root.exists(), "FORGE_REAL_SESSIONS points at a missing path: {}", root.display());

    let report = probe_root(&root);

    eprintln!(
        "real-session probe: files={} journals_skipped={} transform_failed={} \
         frames_tried={} frames_decoded={} unknown_top_types={} \
         unknown_content_blocks={} errors={}",
        report.files_seen,
        report.journals_skipped,
        report.files_transform_failed,
        report.frames_tried,
        report.frames_decoded,
        report.unknown_types.len(),
        report.unknown_blocks.len(),
        report.decode_errors.len()
    );

    if !report.decode_errors.is_empty() {
        eprintln!("\n--- decode errors (first 10) ---");
        for (path, line, err) in report.decode_errors.iter().take(10) {
            eprintln!("{}:{}: {err}", path.display(), line);
        }
        panic!("{} decode errors in real-session probe", report.decode_errors.len());
    }
    if !report.unknown_types.is_empty() {
        eprintln!("\n--- unknown TOP-LEVEL types seen (first 10) ---");
        for (path, line, ty) in report.unknown_types.iter().take(10) {
            eprintln!("  session={} line={} unknown type=\"{}\"", path.display(), line, ty);
        }
        eprintln!(
            "note: unknown top-level types are tolerated via DecodedLine::Unknown; \
             listed here so CLI-bump reviews spot new frame types"
        );
    }
    if !report.unknown_blocks.is_empty() {
        eprintln!("\n--- unknown CONTENT BLOCK types seen (first 10) ---");
        for (path, line, block_type, preview) in report.unknown_blocks.iter().take(10) {
            eprintln!(
                "  session={}\n  line={} block_type=\"{}\" preview={}\n",
                path.display(),
                line,
                block_type,
                preview
            );
        }
        eprintln!(
            "note: unknown content blocks land in ContentBlock::Unknown; \
             above logs identify session + line + content shape so a \
             reviewer can promote the block to a typed variant if it's \
             a known Anthropic API type."
        );
    }
}

/// Fixture tree laid out like the real one: `<slug>/<session-id>.jsonl`
/// plus the session's `<session-id>/subagents/workflows/<wf-id>/` dir.
const SESSION_ID: &str = "11111111-1111-1111-1111-111111111111";

const SESSION_LINES: &str = r#"{"type":"user","sessionId":"sess-fixture","uuid":"uuid-fixture-user","parentUuid":null,"message":{"role":"user","content":"probe fixture prompt"}}
{"type":"assistant","sessionId":"sess-fixture","uuid":"uuid-fixture-assistant","parentUuid":"uuid-fixture-user","message":{"id":"msg-fixture","role":"assistant","model":"claude-sonnet-5","content":[{"type":"text","text":"probe fixture reply"}]}}
"#;

const AGENT_LINES: &str = r#"{"type":"user","isSidechain":true,"sessionId":"sess-fixture","uuid":"uuid-fixture-sidechain","parentUuid":null,"message":{"role":"user","content":"probe fixture sidechain prompt"}}
"#;

/// Rows as a real Workflow journal writes them; the `result` row is the
/// one the decoder reads as a session frame missing `subtype`.
const JOURNAL_LINES: &str = r#"{"type":"launched"}
{"type":"started","key":"v2:fixture","agentId":"agent-fixture","label":"ping"}
{"type":"result","key":"v2:fixture","agentId":"agent-fixture","result":"pong"}
"#;

/// A session-line-shaped `result` frame with none of its required
/// fields - the corruption class the probe exists to catch.
const BAD_RESULT_LINE: &str = r#"{"type":"result","sessionId":"sess-fixture"}"#;

fn write_file(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("fixture path has a parent"))
        .expect("create fixture dir");
    std::fs::write(path, body).expect("write fixture file");
}

fn write_probe_tree(root: &Path, session_body: &str) -> PathBuf {
    let project = root.join("-probe-fixture");
    let session_dir = project.join(SESSION_ID);
    let workflow = session_dir.join("subagents/workflows/wf_fixture");
    write_file(&project.join(format!("{SESSION_ID}.jsonl")), session_body);
    write_file(&workflow.join("journal.jsonl"), JOURNAL_LINES);
    write_file(&workflow.join("agent-fixture.jsonl"), AGENT_LINES);
    root.to_path_buf()
}

/// Positive control: a tree holding a Workflow journal reports no
/// decode errors, and the session-shaped files beside the journal are
/// still read and decoded.
#[test]
fn workflow_journals_are_not_read_as_session_lines() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = write_probe_tree(tmp.path(), SESSION_LINES);

    let report = probe_root(&root);

    assert_eq!(
        report.journals_skipped, 1,
        "the Workflow journal must be the one file counted as skipped"
    );
    assert!(
        report.decode_errors.is_empty(),
        "a tree holding a Workflow journal must report no decode errors, got {:?}",
        report.decode_errors
    );
    assert_eq!(
        report.files_seen, 2,
        "the session transcript and the sidechain transcript must still be read"
    );
    assert_eq!(
        report.frames_decoded, 3,
        "the three session-shaped fixture lines must still decode"
    );
}

/// Negative control: the narrowed read must not blind the probe - a
/// genuinely bad line inside a session transcript is still reported.
#[test]
fn a_bad_session_line_is_still_a_decode_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = write_probe_tree(tmp.path(), &format!("{SESSION_LINES}{BAD_RESULT_LINE}\n"));

    let report = probe_root(&root);

    assert_eq!(
        report.decode_errors.len(),
        1,
        "the bad session line must be the one error, got {:?}",
        report.decode_errors
    );
    let (path, _, reason) = &report.decode_errors[0];
    assert!(
        path.to_string_lossy().ends_with(&format!("{SESSION_ID}.jsonl")),
        "the error must name the session transcript, got {}",
        path.display()
    );
    assert!(
        reason.contains("subtype"),
        "the error must name the missing `subtype` field, got {reason}"
    );
}
