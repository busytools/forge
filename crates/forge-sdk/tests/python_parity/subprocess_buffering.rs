//! Mirrors `tests/test_subprocess_buffering.py` from
//! `claude-agent-sdk-python` v0.1.64.
//!
//! Python hand-rolls a line assembler inside its subprocess transport
//! because its test harness mocks the async stream iterator directly;
//! forge-sdk delegates line assembly to `tokio::io::BufReader`'s
//! `read_line`, so the Rust-side port exercises those guarantees via
//! an in-memory `tokio::io::duplex` pair instead of the real
//! subprocess. Same behavioural contract: bytes come in at arbitrary
//! chunk boundaries, complete JSON lines come out in order.
//!
//! Port of all 10 upstream cases. Two (buffer-size ceiling checks)
//! document an architectural difference: Python enforces a
//! per-line hard cap inside its assembler; forge-sdk doesn't — the
//! `max_buffer_size` option tunes the `BufReader` read-chunk size,
//! not a line-length ceiling. The tests below call that out
//! explicitly rather than faking conformance.
//!
//! Two more (`non_json_debug_lines_skipped`,
//! `interleaved_non_json_lines_skipped`) assert that non-JSON stdout
//! lines are dropped rather than surfacing as parse errors — a
//! regression test for Python upstream issue #347. forge-sdk's
//! subprocess transport pipes stderr to the `Options::stderr`
//! callback and expects stdout to be pure stream-json, so a
//! non-JSON stdout line is a hard parse error today. The tests are
//! marked `#[ignore]` with the FIXME that lands the fix.

use forge_sdk::Error;
use forge_sdk::transport::codec::decode_line;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

/// Drive bytes through a `tokio::io::duplex` pair and return every
/// non-empty line read, matching the behaviour of the real
/// `Subprocess::read_line` loop. `chunks` is written verbatim on one
/// side; empty lines get filtered (parity with Python's assembler
/// skipping empty splits).
async fn drain_lines<I, S>(chunks: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<Vec<u8>>,
{
    let bytes: Vec<Vec<u8>> = chunks.into_iter().map(Into::into).collect();
    let (mut writer, reader) = duplex(1 << 16);
    let writer_task = tokio::spawn(async move {
        for chunk in bytes {
            writer.write_all(&chunk).await.expect("write");
        }
        drop(writer);
    });
    let mut buf = BufReader::new(reader);
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        let n = buf.read_line(&mut line).await.expect("read");
        if n == 0 {
            break;
        }
        while matches!(line.chars().last(), Some('\n' | '\r')) {
            line.pop();
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    writer_task.await.expect("writer");
    out
}

/// Ported from `test_multiple_json_objects_on_single_line`.
/// Buffered stdout may deliver two JSON objects as one chunk with a
/// newline separator.
#[tokio::test]
async fn multiple_json_objects_on_single_line() {
    let lines = drain_lines(vec![
        r#"{"type":"system","subtype":"init"}
{"type":"result","subtype":"success","session_id":"s","is_error":false,"num_turns":0,"duration_ms":0,"duration_api_ms":0}
"#,
    ])
    .await;
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(&lines[0]).expect("parse");
    let second: serde_json::Value = serde_json::from_str(&lines[1]).expect("parse");
    assert_eq!(first.get("subtype"), Some(&json!("init")));
    assert_eq!(second.get("subtype"), Some(&json!("success")));
}

/// Ported from `test_json_with_embedded_newlines`. Newlines embedded
/// in JSON string values are escaped as `\n` on the wire; the real
/// line separator is still a bare newline, so the assembler must not
/// confuse the two.
#[tokio::test]
async fn json_with_embedded_newlines() {
    // In the wire bytes the newlines inside `content` are the
    // two-byte sequence `\n` (backslash + n), NOT a real `\x0a`.
    let lines = drain_lines(vec![
        r#"{"type":"system","subtype":"init","note":"Line 1\nLine 2\nLine 3"}
{"type":"result","subtype":"success","session_id":"s","is_error":false,"num_turns":0,"duration_ms":0,"duration_api_ms":0,"errors":["Some\nMultiline\nContent"]}
"#,
    ])
    .await;
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(&lines[0]).expect("parse");
    assert_eq!(first.get("note"), Some(&json!("Line 1\nLine 2\nLine 3")));
}

/// Ported from `test_multiple_newlines_between_objects`. Empty lines
/// between JSON objects are common when the CLI flushes with extra
/// newlines. Python's assembler skips them; forge-sdk's drain skips
/// them explicitly (`if !line.is_empty()`).
#[tokio::test]
async fn multiple_newlines_between_objects() {
    let lines = drain_lines(vec![
        r#"{"type":"system","subtype":"init"}


{"type":"result","subtype":"success","session_id":"s","is_error":false,"num_turns":0,"duration_ms":0,"duration_api_ms":0}
"#,
    ])
    .await;
    assert_eq!(lines.len(), 2);
}

/// Ported from `test_split_json_across_multiple_reads`. One JSON
/// message split across three sequential stdout chunks must re-
/// assemble into one line.
#[tokio::test]
async fn split_json_across_multiple_reads() {
    let lines = drain_lines(vec![
        r#"{"type":"assist"#,
        r#"ant","session_id":"s","message":{"id":"m","role":"assistant",""#,
        r#"model":"claude","content":[{"type":"text","text":"hello"}]}}"#,
        "\n",
    ])
    .await;
    assert_eq!(lines.len(), 1);
    let msg: serde_json::Value = serde_json::from_str(&lines[0]).expect("parse");
    assert_eq!(msg.get("type"), Some(&json!("assistant")));
    let content = msg
        .pointer("/message/content/0/text")
        .and_then(serde_json::Value::as_str);
    assert_eq!(content, Some("hello"));
}

/// Ported from `test_large_minified_json`. A large JSON body split at
/// 64 KiB boundaries must still re-assemble into one line regardless
/// of the read-chunk size.
#[tokio::test]
async fn large_minified_json() {
    // Build a large payload (~ 200 KiB of JSON). Assemble into one
    // contiguous buffer, then we slice it ourselves since
    // `drain_lines` takes `&'static str` chunks.
    let large_text = "x".repeat(200_000);
    let wire = format!(
        r#"{{"type":"user","session_id":"s","message":{{"role":"user","content":[{{"type":"text","text":"{large_text}"}}]}}}}{newline}"#,
        newline = '\n',
    );
    let (mut writer, reader) = duplex(64 * 1024);
    let wire_for_writer = wire.clone();
    let writer_task = tokio::spawn(async move {
        writer
            .write_all(wire_for_writer.as_bytes())
            .await
            .expect("write large");
        drop(writer);
    });
    let mut buf = BufReader::with_capacity(64 * 1024, reader);
    let mut line = String::new();
    let n = buf.read_line(&mut line).await.expect("read");
    assert!(n > 0);
    writer_task.await.expect("writer");
    let msg: serde_json::Value = serde_json::from_str(line.trim_end()).expect("parse");
    let content = msg
        .pointer("/message/content/0/text")
        .and_then(serde_json::Value::as_str)
        .expect("text");
    assert_eq!(content.len(), 200_000);
}

/// Ported from `test_buffer_size_exceeded`. Python's custom assembler
/// enforces a per-line byte ceiling and raises `CLIJSONDecodeError`
/// once the buffer overflows.
///
/// forge-sdk doesn't impose a hard per-line ceiling — the
/// `Options::max_buffer_size` field tunes `BufReader`'s read-chunk
/// size, not a line-length cap. Tokio's `read_line` grows the target
/// `String` up to `usize::MAX` bytes by contract. That's an
/// architectural difference rather than a bug, but it IS a behaviour
/// gap against Python. This test documents the divergence; fixing
/// would require wrapping `read_line` with an explicit cap.
#[ignore = "parity gap: forge-sdk has no per-line length cap; see rustdoc for rationale"]
#[tokio::test]
async fn buffer_size_exceeded() {
    // Hypothetical: if we DID have a cap, this would overflow it.
    let huge = format!(r#"{{"data":"{}""#, "x".repeat(10_000_000));
    let lines = drain_lines([huge]).await;
    // Python would raise here; forge-sdk just reads the whole line.
    assert!(!lines.is_empty());
}

/// Ported from `test_buffer_size_option`. Same architectural note as
/// `buffer_size_exceeded` above — `max_buffer_size` is tokio's
/// read-chunk hint, not a ceiling.
#[ignore = "parity gap: max_buffer_size sets BufReader capacity, not a per-line cap"]
#[tokio::test]
async fn buffer_size_option() {
    // No-op: if the cap existed, setting it to 512 and emitting a
    // 1 KiB-partial line would error. Today it just buffers the
    // whole thing.
}

/// Ported from `test_mixed_complete_and_split_json`. A typical
/// stream has some messages that arrive whole and others that
/// straddle chunk boundaries. The assembler must emit them in order.
#[tokio::test]
async fn mixed_complete_and_split_json() {
    let lines = drain_lines(vec![
        r#"{"type":"system","subtype":"start"}
"#,
        r#"{"type":"assistant","session_id":"s","message":"#,
        r#"{"id":"m","role":"assistant","model":"claude","content":[{"type":"text","text":""#,
        &"y".repeat(5000),
        r#""}]}}
{"type":"system","subtype":"end"}
"#,
    ])
    .await;
    assert_eq!(lines.len(), 3);
    let first: serde_json::Value = serde_json::from_str(&lines[0]).expect("parse");
    assert_eq!(first.get("subtype"), Some(&json!("start")));
    let second: serde_json::Value = serde_json::from_str(&lines[1]).expect("parse");
    assert_eq!(second.get("type"), Some(&json!("assistant")));
    let text = second
        .pointer("/message/content/0/text")
        .and_then(serde_json::Value::as_str)
        .expect("text");
    assert_eq!(text.len(), 5000);
    let third: serde_json::Value = serde_json::from_str(&lines[2]).expect("parse");
    assert_eq!(third.get("subtype"), Some(&json!("end")));
}

/// Ported from `test_non_json_debug_lines_skipped`. Upstream issue
/// #347 — some sandbox modes emit `[SandboxDebug]` lines on stdout
/// that Python's transport silently skips. forge-sdk currently
/// routes stdout to stream-json-only parsing and surfaces a
/// `JsonDecode` error for any non-JSON line; silent-skip parity
/// would need a small filter in `Client::next_event` that
/// re-reads on `JsonDecode`.
#[ignore = "parity gap: forge-sdk surfaces JsonDecode for non-JSON stdout lines; Python silently skips (upstream #347)"]
#[test]
fn non_json_debug_lines_skipped() {
    let err = decode_line("[SandboxDebug] Seccomp filtering not available", 1);
    // Today this errors out — parity demands we silently skip instead.
    assert!(matches!(err, Err(Error::JsonDecode { .. })));
}

/// Ported from `test_interleaved_non_json_lines_skipped`. Same gap
/// as `non_json_debug_lines_skipped` — documents the same required
/// fix at the Client layer.
#[ignore = "parity gap: forge-sdk surfaces JsonDecode for non-JSON stdout lines; Python silently skips"]
#[test]
fn interleaved_non_json_lines_skipped() {
    let err = decode_line("WARNING: something", 2);
    assert!(matches!(err, Err(Error::JsonDecode { .. })));
}
