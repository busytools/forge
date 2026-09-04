//! Replay every committed baseline through `decode_dispatch` and assert
//! zero decode failures. Runs on every `cargo test` / `just check` -
//! no API cost.
//!
//! This is the "always-on" half of the conformance story:
//! - Live capture mode (`FORGE_WIRE_CAPTURE=1`, the `--ignored` tests)
//!   re-records baselines against the current pinned CLI version.
//! - This file loads each committed baseline and verifies every inbound
//!   line still decodes cleanly via forge-sdk's decoder. If forge-sdk's
//!   decoder regresses, this test fails loudly.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::Message;
use forge_sdk::mcp::McpServerBuilder;
use forge_sdk::mcp::protocol::JsonRpcRequest;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use forge_test_harness::sdk_wire::{
    PINNED_CLI_VERSION, baseline_dir, decode_all_inbound, load_baseline,
};

fn committed_scenarios() -> Vec<String> {
    let dir = baseline_dir();
    if !dir.exists() {
        return Vec::new();
    }
    let mut scenarios: Vec<String> = std::fs::read_dir(&dir)
        .expect("read baseline_dir")
        .filter_map(std::result::Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".jsonl").map(str::to_string)
        })
        .collect();
    scenarios.sort();
    scenarios
}

/// Replay otherwise reads inbound lines only, so nothing offline asserts a
/// byte forge puts on the wire.
///
/// Two different things are checked per handshake, and only the second
/// catches a regression made after the baseline was captured: that the
/// recorded answer agrees with the recorded request, and that **the current
/// code still produces that answer when handed that request**. Comparing the
/// two recorded lines alone is a self-consistency check - it would stay green
/// while the server was rewritten to answer anything at all.
///
/// Expect this to fail between a `PINNED_CLI_VERSION` bump and the re-capture
/// that follows it; `.claude/skills/claude-cli-upgrade/` says not to run
/// replay in that window.
#[tokio::test]
async fn initialize_answers_the_requested_protocol_version() {
    let mut total = 0usize;

    for scenario in committed_scenarios() {
        let log = load_baseline(&scenario);

        let mut requests: std::collections::HashMap<String, JsonRpcRequest> =
            std::collections::HashMap::new();
        for line in log.inbound() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let msg = &v["request"]["message"];
            if v["request"]["subtype"] != "mcp_message" || msg["method"] != "initialize" {
                continue;
            }
            let (Some(id), Ok(req)) =
                (v["request_id"].as_str(), serde_json::from_value::<JsonRpcRequest>(msg.clone()))
            else {
                continue;
            };
            requests.insert(id.to_owned(), req);
        }

        let mut checked = 0usize;
        for line in log.outbound() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let resp = &v["response"];
            let mcp = &resp["response"]["mcp_response"];
            let Some(recorded) = mcp["result"]["protocolVersion"].as_str() else { continue };

            let id = resp["request_id"].as_str().unwrap_or_default();
            let req = requests.get(id).unwrap_or_else(|| {
                panic!("{scenario}: initialize response {id} has no matching request")
            });

            let asked = req.params.as_ref().and_then(|p| p["protocolVersion"].as_str());
            assert_eq!(Some(recorded), asked, "{scenario}: capture answers a version nobody asked");
            assert_eq!(mcp.get("id"), req.id.as_ref(), "{scenario}: inner JSON-RPC id differs");

            let info = &mcp["result"]["serverInfo"];
            let live = McpServerBuilder::new(
                info["name"].as_str().unwrap_or_default(),
                info["version"].as_str().unwrap_or_default(),
            )
            .build()
            .dispatch(req)
            .await
            .expect("initialize always answers");
            let live = serde_json::to_value(&live).expect("serialise");
            assert_eq!(
                live["result"]["protocolVersion"].as_str(),
                Some(recorded),
                "{scenario}: the code no longer answers what the baseline captured"
            );
            checked += 1;
        }

        assert_eq!(
            checked,
            requests.len(),
            "{scenario}: {} initialize request(s) captured but {checked} answer(s) verified",
            requests.len()
        );
        total += checked;
    }

    // Guards the whole thing going quiet if the correlation ever stops
    // matching - the exact failure shape it exists to catch.
    assert!(total > 0, "no initialize handshake found in any committed baseline");
}

/// The baseline this replaced held a REFUSED compaction - "Not enough
/// messages to compact" - which is a well-formed frame, so it decoded
/// clean forever while the boundary the scenario exists to record never
/// arrived. Decode-cleanliness cannot distinguish the two, exactly as
/// `all_baselines_decode_cleanly` cannot distinguish an empty corpus
/// from a passing one without its own floor. So the frame is asserted by
/// name, and through the decoder rather than as a substring, which also
/// pins that it still reaches the typed variant.
#[test]
fn the_compact_baseline_carries_a_real_compaction() {
    let dir = baseline_dir();
    if !dir.exists() {
        eprintln!("no baselines directory at {} - skipping", dir.display());
        return;
    }

    let log = load_baseline("compact");
    let boundaries = log
        .inbound()
        .iter()
        .filter(|line| {
            matches!(
                decode_dispatch(line, 1),
                DecodedLine::Message(Message::CompactBoundary { .. })
            )
        })
        .count();

    assert!(
        boundaries >= 1,
        "the compact baseline carries no compact_boundary frame, so the compaction path is \
         uncovered - a capture of a refused compaction replays clean while covering nothing",
    );
}

#[test]
fn all_baselines_decode_cleanly() {
    let dir = baseline_dir();
    if !dir.exists() {
        eprintln!(
            "no baselines directory at {} - skipping (run a live capture first)",
            dir.display()
        );
        return;
    }

    let scenarios = committed_scenarios();

    if scenarios.is_empty() {
        eprintln!(
            "no baselines in {} - skipping. Capture some with FORGE_WIRE_CAPTURE=1.",
            dir.display()
        );
        return;
    }

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut summary: Vec<(String, usize, usize, usize, usize)> = Vec::new();

    for scenario in &scenarios {
        let log = load_baseline(scenario);
        let report = decode_all_inbound(&log);
        // An empty baseline decodes to an empty report, and an empty
        // report is clean - so without a floor, a corpus that got
        // truncated or emptied passes this test while asserting nothing.
        // Collected rather than asserted here, so a decoder regression
        // still gets its per-scenario drift report printed below.
        let decoded =
            report.messages + report.controls + report.control_cancels + report.control_responses;
        summary.push((
            scenario.clone(),
            report.messages,
            report.controls,
            report.control_cancels,
            report.control_responses,
        ));
        if decoded == 0 {
            failures.push((scenario.clone(), "decoded no inbound lines at all".to_string()));
        } else if !report.is_clean() {
            failures.push((scenario.clone(), format!("{report:#?}")));
        }
    }

    eprintln!("pinned CLI version: {PINNED_CLI_VERSION}");
    eprintln!(
        "scenarios: {} | decode summary (messages / controls / cancels / responses):",
        scenarios.len()
    );
    for (name, m, c, cc, cr) in &summary {
        eprintln!("  {name}: {m}m / {c}c / {cc}cc / {cr}cr");
    }

    if !failures.is_empty() {
        eprintln!("\n{} scenario(s) failed the replay gate:", failures.len());
        for (scen, rpt) in &failures {
            eprintln!("--- {scen} ---\n{rpt}\n");
        }
        panic!(
            "replay regressions detected - either forge-sdk's decoder drifted, the CLI's \
             wire shape changed and we need to recapture baselines, or a baseline lost \
             its contents."
        );
    }
}
