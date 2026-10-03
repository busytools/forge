//! Live-capture scenario: the `command_lifecycle` frames the CLI emits for a
//! prompt that carried a uuid on the way in.
//!
//! **Empirical finding from live capture 2026-10-03** (this is the whole point
//! of the scenario):
//!
//! 1. A prompt written on an idle session under a uuid answers with
//!    `command_lifecycle {command_uuid, state: "queued"}` and then
//!    `state: "started"` back to back - the CLI took it at once.
//! 2. A prompt written while the turn is in flight answers with exactly one
//!    frame at that moment: `queued`.
//! 3. That prompt's `started` lands at the turn boundary that delivers it -
//!    what lets a view say queued versus taken.
//! 4. The frames carry ONLY for uuid-stamped prompts: the rig's no-uuid runs
//!    captured zero across four scenarios. That absence is why the stamp is
//!    load-bearing, and it is measured outside this scenario (the client has
//!    no unstamped send path to drive here).
//!
//! The committed baseline locks the presence and order of the queued/started
//! pair for both prompts; `wire_capture_queued_command` (2026-05-13) records
//! the older absence and its doc comment predates these frames.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::Duration;

use forge_primitives::Message;
use forge_sdk::{Client, OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::{attach_recording, decode_all_inbound};

const P1: &str = "cap_p1";
const P2: &str = "cap_p2";

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_queued_prompt_lifecycle() {
    if std::env::var("FORGE_WIRE_CAPTURE").is_err() {
        eprintln!("FORGE_WIRE_CAPTURE not set; skipping");
        return;
    }

    let (builder, log_arc) = attach_recording(
        OptionsBuilder::new()
            .max_turns(4)
            .permission_mode(PermissionMode::AcceptEdits)
            .allowed_tools(vec!["Bash".to_string()]),
    );
    let opts = builder.build();

    let dump_trace = |tag: &str| -> std::path::PathBuf {
        let log = log_arc.lock();
        let target =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wire-traces");
        std::fs::create_dir_all(&target).expect("create wire-traces dir");
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = target.join(format!("capture-{tag}-{ts}.jsonl"));
        std::fs::write(&path, log.to_jsonl().expect("redact + serialise trace"))
            .expect("write trace");
        eprintln!(
            "wire trace ({tag}): {} [in={} out={}]",
            path.display(),
            log.inbound().len(),
            log.outbound().len()
        );
        path
    };

    let (client, mut events) = match Client::spawn(opts).await {
        Ok(pair) => pair,
        Err(e) => {
            let path = dump_trace("queued-prompt-lifecycle-spawn-failed");
            panic!("Client::spawn failed - trace written to {}: {e}", path.display());
        }
    };

    // Both ids are fixed rather than minted so the baseline is deterministic
    // and its diff readable; the CLI treats them as opaque and only echoes
    // them back as `command_uuid`.
    if let Err(e) = client
        .send_user_message_under(
            "Run `sleep 2 && echo forge-lifecycle-scenario` with the Bash tool, then report \
             what it printed.",
            P1,
        )
        .await
    {
        panic!("p1 send failed - trace at {}: {e}", dump_trace("p1-send-failed").display());
    }

    // Long enough that the first prompt's turn is in flight when the second
    // lands - that is the whole shape this scenario exists to record.
    tokio::time::sleep(Duration::from_millis(700)).await;
    if let Err(e) =
        client.send_user_message_under("Also, reply with the single word pong.", P2).await
    {
        panic!("p2 send failed - trace at {}: {e}", dump_trace("p2-send-failed").display());
    }

    let mut saw_result = false;
    while let Some(item) = events.recv().await {
        match item {
            Ok(msg) => {
                if matches!(msg, Message::Result { .. }) {
                    saw_result = true;
                    break;
                }
            }
            Err(e) => {
                panic!(
                    "events stream errored mid-drain - trace at {}: {e}",
                    dump_trace("queued-prompt-lifecycle-drain-failed").display()
                );
            }
        }
    }
    if let Err(e) = client.disconnect().await {
        eprintln!("queued-prompt-lifecycle: disconnect failed (non-fatal): {e}");
    }

    let trace_path = dump_trace("queued-prompt-lifecycle");
    let log = log_arc.lock();
    let report = decode_all_inbound(&log);
    assert!(
        report.is_clean(),
        "decode regressions in captured trace\n trace: {}\n report: {report:#?}",
        trace_path.display()
    );
    assert!(saw_result, "the scenario did not reach a Result frame");

    // The frames the scenario exists for: both prompts queued AND started,
    // under the ids forge stamped.
    let lifecycle: Vec<(String, String)> = log
        .inbound()
        .iter()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v["type"] != "command_lifecycle" {
                return None;
            }
            Some((v["command_uuid"].as_str()?.to_string(), v["state"].as_str()?.to_string()))
        })
        .collect();
    for (id, what) in [(P1, "the idle prompt"), (P2, "the prompt queued mid-turn")] {
        for state in ["queued", "started"] {
            assert!(
                lifecycle.contains(&(id.to_string(), state.to_string())),
                "{what} ({id}) produced no `{state}` lifecycle frame; captured: {lifecycle:?}"
            );
        }
    }
    eprintln!("lifecycle frames captured: {lifecycle:?}");
}
