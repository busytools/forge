//! Wire conformance harness - verify forge-sdk's decoder handles every
//! stream-json frame the live `claude` binary emits in realistic
//! scenarios.
//!
//! ## What this checks
//!
//! 1. **Decode completeness.** Every inbound line must round-trip
//!    through `transport::codec::decode_dispatch` without error. Any
//!    unknown top-level `type` or control-request `subtype` surfaces
//!    as a panic with the failing line attached.
//! 2. **Trace capture.** Full stdin + stdout byte-capture lands at
//!    `target/wire-traces/capture-<scenario>-<ts>.jsonl`, one
//!    `{"dir":"in"|"out","line":"..."}` object per line.
//! 3. **Metadata observability.** Captures turn count, cost, and
//!    duration from the `Message::Result` frame.
//!
//! ## How to run
//!
//! ```bash
//! # Uses whatever CLAUDE_CONFIG_DIR currently resolves to.
//! FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness --no-capture
//! ```
//!
//! Opt-in because this burns real API tokens (small - a trivial
//! prompt). Skipped silently when the env var is unset.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::{SystemTime, UNIX_EPOCH};

use forge_primitives::Message;
use forge_sdk::{Client, OptionsBuilder};
use forge_test_harness::sdk_wire::{TraceLog, attach_recording, decode_all_inbound};

fn timestamp_tag() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    format!("{secs}")
}

fn write_trace(scenario: &str, log: &TraceLog) -> std::path::PathBuf {
    let target =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wire-traces");
    std::fs::create_dir_all(&target).expect("create wire-traces dir");
    let path = target.join(format!("capture-{scenario}-{}.jsonl", timestamp_tag()));
    let body = log.to_jsonl().expect("redact + serialise trace");
    std::fs::write(&path, body).expect("write trace");
    path
}

fn assert_decode_completeness(log: &TraceLog, trace_path: &std::path::Path) {
    let report = decode_all_inbound(log);
    assert!(
        report.is_clean(),
        "wire_conformance: decode regressions in captured trace\n\
         trace: {}\n\
         report: {report:#?}",
        trace_path.display()
    );
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_trivial_prompt() {
    if std::env::var("FORGE_WIRE_CAPTURE").is_err() {
        eprintln!("FORGE_WIRE_CAPTURE not set; skipping");
        return;
    }

    // Inherit the parent env's `CLAUDE_CONFIG_DIR` so the spawned CLI uses
    // whatever profile the developer's shell is authenticated against. An
    // earlier attempt pointed at a fresh tempdir for "clean" traces - but
    // a fresh config dir has no credentials, so the CLI bails with an
    // auth-error result frame WITHOUT ever emitting a `control_response`
    // to our `initialize` request, which then hangs `send_control` forever.
    // The trade-off: captured traces include whatever hook/skill/MCP noise
    // the developer's profile produces. Decoder must tolerate that anyway
    // (it's what real library consumers will see), and our `Unknown`
    // fallbacks plus pre-init buffering keep the harness robust.
    let (builder, log_arc) = attach_recording(OptionsBuilder::new().max_turns(1));
    let opts = builder.build();

    // Scope guard: always dump whatever we captured, even on a panic partway
    // through - so failing spawns still give us a trace for post-mortem.
    let dump_trace = |tag: &str| -> std::path::PathBuf {
        let log = log_arc.lock();
        let path = write_trace(tag, &log);
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
            let path = dump_trace("trivial-spawn-failed");
            panic!("Client::spawn failed - trace written to {}: {e}", path.display());
        }
    };

    if let Err(e) = client.send_user_message("Respond with just the word OK.").await {
        let path = dump_trace("trivial-send-failed");
        panic!("send_user_message failed - trace written to {}: {e}", path.display());
    }

    let mut saw_result = false;
    let mut summary: Option<(u64, Option<f64>, u64)> = None;
    while let Some(item) = events.recv().await {
        match item {
            Ok(msg) => {
                if let Message::Result { num_turns, total_cost_usd, duration_ms, .. } = &msg {
                    saw_result = true;
                    summary = Some((*num_turns, *total_cost_usd, *duration_ms));
                    break;
                }
            }
            Err(e) => {
                let path = dump_trace("trivial-drain-failed");
                panic!("events stream errored mid-drain - trace at {}: {e}", path.display());
            }
        }
    }
    if let Err(e) = client.disconnect().await {
        eprintln!("trivial: disconnect failed (non-fatal, trace already captured): {e}");
    }

    let trace_path = dump_trace("trivial");
    let log = log_arc.lock();
    assert_decode_completeness(&log, &trace_path);

    assert!(saw_result, "trivial prompt did not produce a Result frame");
    let (turns, cost, dur_ms) = summary.unwrap();
    eprintln!(
        "captured in={} out={} | turns={} duration_ms={} cost_usd={:?}",
        log.inbound().len(),
        log.outbound().len(),
        turns,
        dur_ms,
        cost
    );
}
