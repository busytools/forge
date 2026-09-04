//! Scenario: a corrupt line between valid frames must not end the session.
//!
//! A decode failure on one stream-json line used to terminate the
//! reader, so a single malformed frame from a non-Anthropic backend
//! (a null where a counter belongs, truncated JSON) killed the session
//! even though forge already skips frames it cannot recognise at all
//! (`DecodedLine::Unknown`). This scenario drives a real Client
//! against a fake `claude` whose stdout carries valid frames around
//! two corrupt lines, and asserts the valid frames still round-trip
//! and the corrupt ones produce only the typed skip.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::PathBuf;
use std::time::Duration;

use forge_primitives::Message;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use forge_sdk::{Client, Error, OptionsBuilder};

/// Answers the `initialize` handshake by echoing its request_id, then
/// emits two valid frames around two corrupt lines (truncated JSON,
/// and valid JSON of the wrong shape) before EOF.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
if [ "${1:-}" = "--version" ]; then
  echo "2.1.201 (Claude Code)"
  exit 0
fi
# Answer the initialize handshake by echoing its request_id.
while IFS= read -r line; do
  rid=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  if [ -n "$rid" ]; then
    printf '{"type":"control_response","response":{"request_id":"%s","subtype":"success","response":{}}}\n' "$rid"
    break
  fi
done
printf '{"type":"stream_event","uuid":"before","session_id":"sess","event":{"type":"message_start"}}\n'
printf '{"type":"stream_event"\n'
printf '{"type":"stream_event"}\n'
printf '{"type":"stream_event","uuid":"after","session_id":"sess","event":{"type":"message_start"}}\n'
"#;

/// Emits a corrupt line BEFORE answering the initialize handshake:
/// pre-init frames are strict, and the spawn must fail.
const FAKE_CLAUDE_STRICT: &str = r#"#!/bin/sh
if [ "${1:-}" = "--version" ]; then
  echo "2.1.201 (Claude Code)"
  exit 0
fi
printf '{"type":"stream_event"}\n'
# Answer the initialize handshake by echoing its request_id.
while IFS= read -r line; do
  rid=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  if [ -n "$rid" ]; then
    printf '{"type":"control_response","response":{"request_id":"%s","subtype":"success","response":{}}}\n' "$rid"
    break
  fi
done
"#;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fake_claude(tag: &str, script_text: &str) -> (TempDir, PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("forge-malformed-line-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let script = dir.join("claude");
    std::fs::write(&script, script_text).expect("write fake claude");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude");
    }
    (TempDir(dir), script)
}

#[tokio::test]
async fn corrupt_lines_between_valid_frames_do_not_end_the_session() {
    let (_tmp, script) = fake_claude("e2e", FAKE_CLAUDE);
    let (client, mut events) =
        Client::spawn(OptionsBuilder::new().binary(script.display().to_string()).build())
            .await
            .expect("spawn fake claude");

    let mut seen = Vec::new();
    while let Some(item) = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("events stream did not close within 10s; the reader hung")
    {
        seen.push(item);
    }
    drop(client);

    assert_eq!(seen.len(), 2, "both valid frames must arrive: {seen:?}");
    assert!(
        seen.iter().all(Result::is_ok),
        "no corrupt line may surface as an Err event: {seen:?}"
    );
    let uuids: Vec<&str> = seen
        .iter()
        .map(|r| match r {
            Ok(Message::StreamEvent { uuid, .. }) => uuid.as_str(),
            other => panic!("expected StreamEvent frames, got {other:?}"),
        })
        .collect();
    assert_eq!(uuids, ["before", "after"], "frames must arrive in stream order");
}

#[tokio::test]
async fn a_corrupt_line_before_initialize_still_fails_the_spawn() {
    let (_tmp, script) = fake_claude("init-strict", FAKE_CLAUDE_STRICT);
    let outcome =
        Client::spawn(OptionsBuilder::new().binary(script.display().to_string()).build()).await;

    match outcome {
        Err(Error::MessageParse { reason, .. }) => {
            assert!(
                reason.contains("raw line:"),
                "the failure must name the offending line: {reason}"
            );
        }
        other => panic!("expected MessageParse from pre-init strictness, got {other:?}"),
    }
}

#[test]
fn decode_classifies_each_corrupt_line_as_a_typed_skip() {
    let truncated = r#"{"type":"stream_event""#;
    let wrong_shape = r#"{"type":"stream_event"}"#;
    let valid =
        r#"{"type":"stream_event","uuid":"u","session_id":"s","event":{"type":"message_start"}}"#;

    assert!(
        matches!(decode_dispatch(truncated, 3), DecodedLine::Malformed { line: 3, .. }),
        "invalid JSON must classify as the typed skip"
    );
    assert!(
        matches!(decode_dispatch(wrong_shape, 4), DecodedLine::Malformed { line: 4, .. }),
        "valid JSON of the wrong shape must classify as the typed skip"
    );
    assert!(
        matches!(decode_dispatch(valid, 5), DecodedLine::Message(_)),
        "a valid frame must still decode to a Message"
    );
}
