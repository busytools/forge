//! Opt-in decoder probe against Claude Code's on-disk session .jsonl
//! files.
//!
//! Claude Code persists each session to
//! `$CLAUDE_CONFIG_DIR/projects/<project-slug>/<session-id>.jsonl`.
//! The persistence format is a near-superset of the stream-json wire
//! protocol but uses camelCase (`sessionId`) and carries extra
//! persistence-only fields (`parentUuid`, `cwd`, `timestamp`, …). A
//! transformer in [`forge_conformance::session_redact`] rewrites each
//! line into wire shape + redacts PII.
//!
//! When `FORGE_REAL_SESSIONS=<path>` is set, this test walks that
//! directory tree, finds every `*.jsonl`, transforms each decodable
//! entry, and feeds it through `decode_dispatch`. Purpose: catch
//! decoder regressions against real-world data the developer has
//! accumulated without committing any session content to the repo.
//!
//! Typical invocation:
//!
//! ```bash
//! FORGE_REAL_SESSIONS=$HOME/.claude-stargate/projects \
//!   cargo nextest run -p forge-conformance --no-capture \
//!   real_session_decode_probe
//! ```
//!
//! No output is persisted — failures surface as stderr lines plus a
//! panic summarising the count. To produce a committed redacted
//! baseline from a specific session, run the `redact-session` example
//! instead.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use forge_conformance::session_redact::redact_session_file;
use forge_sdk::Message;
use forge_sdk::content::ContentBlock;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};

/// Walk a decoded `Message` for any `ContentBlock::Unknown` variants
/// and surface them as `(type_str, snippet)` pairs. The snippet is a
/// short JSON preview of the unknown block so a TUI consumer (or
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
                let preview = serde_json::to_string(raw)
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
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

#[test]
fn real_session_decode_probe() {
    let Ok(root_str) = std::env::var("FORGE_REAL_SESSIONS") else {
        eprintln!(
            "FORGE_REAL_SESSIONS not set; skipping real-session probe. \
             Set it to e.g. $HOME/.claude-stargate/projects to run."
        );
        return;
    };
    let root = PathBuf::from(&root_str);
    if !root.exists() {
        panic!(
            "FORGE_REAL_SESSIONS points at a missing path: {}",
            root.display()
        );
    }

    let files = jsonl_files_under(&root);
    eprintln!(
        "probing {} session files under {}",
        files.len(),
        root.display()
    );

    let mut files_seen = 0_usize;
    let mut files_transform_failed = 0_usize;
    let mut frames_tried = 0_usize;
    let mut frames_decoded = 0_usize;
    let mut decode_errors: Vec<(PathBuf, usize, String)> = Vec::new();
    let mut unknown_types: Vec<(PathBuf, usize, String)> = Vec::new();
    let mut unknown_blocks: Vec<(PathBuf, usize, String, String)> = Vec::new();

    for path in &files {
        files_seen += 1;
        let Ok(body) = std::fs::read_to_string(path) else {
            eprintln!("skipping unreadable {}", path.display());
            continue;
        };
        let transformed = match redact_session_file(&body) {
            Ok(lines) => lines,
            Err(e) => {
                files_transform_failed += 1;
                eprintln!("transform failed on {}: {e}", path.display());
                continue;
            }
        };
        for (idx, line) in transformed.iter().enumerate() {
            frames_tried += 1;
            match decode_dispatch(line, (idx + 1) as u64) {
                Ok(DecodedLine::Unknown { type_str, raw: _ }) => {
                    unknown_types.push((path.clone(), idx + 1, type_str));
                }
                Ok(DecodedLine::Message(msg)) => {
                    frames_decoded += 1;
                    for (block_type, preview) in unknown_content_blocks(&msg) {
                        unknown_blocks.push((path.clone(), idx + 1, block_type, preview));
                    }
                }
                Ok(_) => {
                    frames_decoded += 1;
                }
                Err(e) => {
                    decode_errors.push((path.clone(), idx + 1, format!("{e}")));
                }
            }
        }
    }

    eprintln!(
        "real-session probe: files={} transform_failed={} \
         frames_tried={} frames_decoded={} unknown_top_types={} \
         unknown_content_blocks={} errors={}",
        files_seen,
        files_transform_failed,
        frames_tried,
        frames_decoded,
        unknown_types.len(),
        unknown_blocks.len(),
        decode_errors.len()
    );

    if !decode_errors.is_empty() {
        eprintln!("\n--- decode errors (first 10) ---");
        for (path, line, err) in decode_errors.iter().take(10) {
            eprintln!("{}:{}: {err}", path.display(), line);
        }
        panic!(
            "{} decode errors in real-session probe",
            decode_errors.len()
        );
    }
    if !unknown_types.is_empty() {
        eprintln!("\n--- unknown TOP-LEVEL types seen (first 10) ---");
        for (path, line, ty) in unknown_types.iter().take(10) {
            eprintln!(
                "  session={} line={} unknown type=\"{}\"",
                path.display(),
                line,
                ty
            );
        }
        eprintln!(
            "note: unknown top-level types are tolerated via DecodedLine::Unknown; \
             listed here so CLI-bump reviews spot new frame types"
        );
    }
    if !unknown_blocks.is_empty() {
        eprintln!("\n--- unknown CONTENT BLOCK types seen (first 10) ---");
        for (path, line, block_type, preview) in unknown_blocks.iter().take(10) {
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
